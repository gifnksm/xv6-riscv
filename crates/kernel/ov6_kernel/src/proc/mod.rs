use core::{
    cell::UnsafeCell,
    mem,
    num::NonZero,
    ops::{Deref, DerefMut},
    panic::Location,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
    time::Duration,
};

use arrayvec::ArrayVec;
use once_init::OnceInit;
use ov6_types::{fs::RawFd, os_str::OsStr, process::ProcId};

use self::{
    scheduler::Context,
    wait_lock::{Parent, WaitLock},
};
use crate::{
    cpu::Cpu,
    error::KernelError,
    file::File,
    fs::Inode,
    interrupt::{
        self,
        timer::Uptime,
        trap::{self, TrapFrame, UserRegisters},
    },
    memory::{
        PAGE_SIZE, VirtAddr,
        layout::{self, KSTACK_PAGES},
        vm_user::UserPageTable,
    },
    param::{NOFILE, NPROC},
    sync::{SpinLock, SpinLockCondVar, SpinLockGuard, TryLockError},
};

mod elf;
pub mod exec;
pub mod ops;
pub mod scheduler;
mod wait_lock;

static PROC: [Proc; NPROC] = [const { Proc::new() }; NPROC];
static INIT_PROC: OnceInit<&'static Proc> = OnceInit::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcState {
    Unused,
    Used,
    Sleeping { chan: usize },
    Runnable,
    Running,
    Zombie { exit_status: i32 },
}

#[derive(Debug)]
pub struct AlarmInfo {
    time: Uptime,
    dur: Duration,
    handler: VirtAddr,
}

impl AlarmInfo {
    pub(crate) fn is_expired(&self, now: Uptime) -> bool {
        now >= self.time
    }

    pub(crate) fn update(&mut self, now: Uptime) {
        self.time = now.saturating_add(self.dur);
    }

    pub(crate) fn handler(&self) -> VirtAddr {
        self.handler
    }
}

/// Per-process state that can be accessed from other processes.
pub struct ProcSharedData {
    /// Process ID
    pid: Option<ProcId>,
    /// Process name (for debugging)
    name: ArrayVec<u8, 16>,
    /// Process State
    state: ProcState,
    /// Process is killed
    killed: bool,
    /// Alarm information
    alarm: Option<AlarmInfo>,
    /// Process context.
    ///
    /// Call `switch()` here to enter process.
    context: Context,
}

impl ProcSharedData {
    pub fn pid(&self) -> ProcId {
        self.pid.unwrap()
    }

    pub fn name(&self) -> &OsStr {
        OsStr::from_bytes(&self.name)
    }

    pub fn set_name(&mut self, name: &OsStr) {
        self.name.clear();
        let len = usize::min(self.name.capacity(), name.len());
        self.name
            .try_extend_from_slice(&name.as_bytes()[..len])
            .unwrap();
    }

    pub fn kill(&mut self) {
        self.killed = true;
    }

    pub fn killed(&self) -> bool {
        self.killed
    }

    pub fn alarm_mut(&mut self) -> Option<&mut AlarmInfo> {
        self.alarm.as_mut()
    }

    pub fn set_alarm(&mut self, dur: Duration, handler: VirtAddr) {
        let time = Uptime::now().saturating_add(dur);
        self.alarm = Some(AlarmInfo { time, dur, handler });
    }

    pub fn clear_alarm(&mut self) {
        self.alarm = None;
    }
}

pub struct ProcShared(SpinLock<ProcSharedData>);

impl ProcShared {
    const fn new() -> Self {
        Self(SpinLock::new(ProcSharedData {
            pid: None,
            name: ArrayVec::new_const(),
            state: ProcState::Unused,
            killed: false,
            alarm: None,
            context: Context::zeroed(),
        }))
    }

    pub fn current() -> &'static Self {
        Self::try_current().unwrap()
    }

    pub fn try_current() -> Option<&'static Self> {
        let p = Proc::try_current()?;
        Some(&p.shared)
    }

    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, ProcSharedData> {
        self.0.lock()
    }

    #[track_caller]
    pub fn try_lock(&self) -> Result<SpinLockGuard<'_, ProcSharedData>, TryLockError> {
        self.0.try_lock()
    }

    unsafe fn remember_locked(&self) -> SpinLockGuard<'_, ProcSharedData> {
        unsafe { self.0.remember_locked() }
    }
}

pub struct InterruptedContext {
    pub epc: usize,
    pub user_registers: UserRegisters,
}

pub struct SignalHandlerState {
    context: InterruptedContext,
}

pub struct ProcPrivateData {
    pid: ProcId,
    /// Virtual address of kernel stack.
    kstack: VirtAddr,
    /// User page table,
    pagetable: UserPageTable,
    /// Open files
    ofile: [Option<File>; NOFILE],
    /// Current directory
    cwd: Option<Inode>,
    /// System call trace mask
    trace_mask: u64,
    signal_handler_state: Option<SignalHandlerState>,
}

impl ProcPrivateData {
    pub fn kstack(&self) -> VirtAddr {
        self.kstack
    }

    pub fn program_break(&self) -> VirtAddr {
        self.pagetable.program_break()
    }

    pub fn pagetable(&self) -> &UserPageTable {
        &self.pagetable
    }

    pub fn pagetable_mut(&mut self) -> &mut UserPageTable {
        &mut self.pagetable
    }

    pub fn update_pagetable(&mut self, pt: UserPageTable) {
        self.pagetable = pt;
    }

    pub fn trapframe(&self) -> &TrapFrame {
        self.pagetable.trapframe()
    }

    pub fn trapframe_mut(&mut self) -> &mut TrapFrame {
        self.pagetable.trapframe_mut()
    }

    pub fn ofile(&self, fd: RawFd) -> Result<&File, KernelError> {
        self.ofile
            .get(fd.get())
            .and_then(|x| x.as_ref())
            .ok_or(KernelError::FileDescriptorNotFound(fd, self.pid))
    }

    pub fn add_ofile(&mut self, file: File) -> Result<RawFd, KernelError> {
        let (fd, slot) = self
            .ofile
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
            .ok_or(KernelError::NoFreeFileDescriptorTableEntry)?;
        assert!(slot.replace(file).is_none());
        Ok(RawFd::new(fd))
    }

    pub fn unset_ofile(&mut self, fd: RawFd) -> Result<File, KernelError> {
        self.ofile
            .get_mut(fd.get())
            .and_then(Option::take)
            .ok_or(KernelError::FileDescriptorNotFound(fd, self.pid))
    }

    #[track_caller]
    pub fn cwd(&self) -> &Inode {
        self.cwd.as_ref().unwrap()
    }

    pub fn update_cwd(&mut self, cwd: Inode) -> Inode {
        self.cwd.replace(cwd).unwrap()
    }

    pub fn trace_mask(&self) -> u64 {
        self.trace_mask
    }

    pub fn set_trace_mask(&mut self, mask: u64) {
        self.trace_mask = mask;
    }

    pub fn enter_signal_handler(&mut self, handler: VirtAddr) {
        if self.signal_handler_state.is_some() {
            // already entered
            return;
        }

        let tf = self.pagetable.trapframe_mut();
        self.signal_handler_state = Some(SignalHandlerState {
            context: InterruptedContext {
                epc: tf.epc,
                user_registers: tf.user_registers,
            },
        });

        tf.epc = handler.addr();
    }

    pub fn exit_from_signal_handler(&mut self) -> Result<(), KernelError> {
        let state = self
            .signal_handler_state
            .take()
            .ok_or(KernelError::NotInSignalHandler)?;

        let tf = self.pagetable.trapframe_mut();
        tf.epc = state.context.epc;
        tf.user_registers = state.context.user_registers;

        Ok(())
    }
}

pub struct ProcPrivateDataGuard<'p> {
    proc: &'p Proc,
    private_taken: &'p AtomicBool,
    private: &'p mut ProcPrivateData,
}

impl Drop for ProcPrivateDataGuard<'_> {
    fn drop(&mut self) {
        self.private_taken.store(false, Ordering::Release);
    }
}

impl Deref for ProcPrivateDataGuard<'_> {
    type Target = ProcPrivateData;

    fn deref(&self) -> &Self::Target {
        self.private
    }
}

impl DerefMut for ProcPrivateDataGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.private
    }
}

impl ProcPrivateDataGuard<'_> {
    fn remove_private(self) {
        let proc = self.proc;

        // to avoid multiple mutable reference to `ProcPrivateData` exists concurrently
        mem::forget(self);

        let private = unsafe { proc.private.get().as_mut().unwrap() }
            .take()
            .unwrap();
        assert!(private.ofile.iter().all(Option::is_none));
        assert!(private.cwd.is_none());

        proc.private_borrowed.store(false, Ordering::Release);
    }
}

/// Per-process state.
pub struct Proc {
    /// Process sharead data
    shared: ProcShared,
    /// Parent process
    parent: Parent,
    /// Condition variable that is notified when a child process ends.
    child_ended: SpinLockCondVar,
    /// `true` if `private` is borrowed.
    private_borrowed: AtomicBool,
    /// Location where `private` is borrowed.
    borrowed_location: AtomicPtr<Location<'static>>,
    /// Process private data.
    private: UnsafeCell<Option<ProcPrivateData>>,
}

unsafe impl Sync for Proc {}

impl Proc {
    const fn new() -> Self {
        Self {
            shared: ProcShared::new(),
            parent: Parent::new(),
            child_ended: SpinLockCondVar::new(),
            private_borrowed: AtomicBool::new(false),
            borrowed_location: AtomicPtr::new(ptr::from_ref(Location::caller()).cast_mut()),
            private: UnsafeCell::new(None),
        }
    }

    /// Returns the current process.
    pub fn current() -> &'static Self {
        Self::try_current().unwrap()
    }

    /// Returns the current process.
    pub fn try_current() -> Option<&'static Self> {
        let p = interrupt::with_push_disabled(|| Cpu::current().proc())?;
        Some(unsafe { p.as_ref() })
    }

    pub fn shared(&self) -> &ProcShared {
        &self.shared
    }

    #[track_caller]
    #[expect(clippy::mut_from_ref)]
    fn borrow_private_raw(&self) -> &mut Option<ProcPrivateData> {
        if self.private_borrowed.swap(true, Ordering::Acquire) {
            let taker = unsafe { self.borrowed_location.load(Ordering::Relaxed).as_ref() }.unwrap();
            panic!("ProcPrivateData is already taken at {taker}");
        }
        self.borrowed_location.store(
            ptr::from_ref(Location::caller()).cast_mut(),
            Ordering::Relaxed,
        );

        unsafe { self.private.get().as_mut().unwrap() }
    }

    #[track_caller]
    fn init_private(&self, data: ProcPrivateData) -> ProcPrivateDataGuard<'_> {
        let private = self.borrow_private_raw();
        assert!(private.is_none());
        *private = Some(data);

        ProcPrivateDataGuard {
            proc: self,
            private_taken: &self.private_borrowed,
            private: private.as_mut().unwrap(),
        }
    }

    #[track_caller]
    pub fn borrow_private(&self) -> Option<ProcPrivateDataGuard<'_>> {
        let Some(private) = self.borrow_private_raw() else {
            // process already exited
            self.private_borrowed.store(false, Ordering::Release);
            return None;
        };

        Some(ProcPrivateDataGuard {
            proc: self,
            private_taken: &self.private_borrowed,
            private,
        })
    }

    fn is_child_of(&self, parent: &Self, wait_lock: &mut SpinLockGuard<WaitLock>) -> bool {
        self.parent
            .get(wait_lock)
            .is_some_and(|pp| ptr::eq(parent, pp))
    }

    fn set_parent(&self, parent: &'static Self, wait_lock: &mut SpinLockGuard<WaitLock>) {
        self.parent.set(parent, wait_lock);
    }

    fn allocate_pid() -> ProcId {
        static NEXT_PID: AtomicU32 = AtomicU32::new(1);
        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        ProcId::new(NonZero::new(pid).unwrap())
    }

    /// Returns UNUSED proc in the process table.
    ///
    /// If there is no UNUSED proc, returns None.
    /// This function also locks the proc.
    fn lock_unused_proc()
    -> Result<(usize, &'static Self, SpinLockGuard<'static, ProcSharedData>), KernelError> {
        for (i, p) in PROC.iter().enumerate() {
            let shared = p.shared.lock();
            if shared.state != ProcState::Unused {
                drop(shared);
                continue;
            }
            return Ok((i, p, shared));
        }
        Err(KernelError::NoFreeProc)
    }

    /// Returns a new process.
    ///
    /// Locks in the process table for an UNUSED proc.
    /// If found, initialize state required to run in the kenrnel,
    /// and return with the lock held.
    /// If there are no free procs, return None.
    fn allocate() -> Result<
        (
            &'static Self,
            SpinLockGuard<'static, ProcSharedData>,
            ProcPrivateDataGuard<'static>,
        ),
        KernelError,
    > {
        let (i, p, mut shared) = Self::lock_unused_proc()?;

        let pid = Self::allocate_pid();
        shared.pid = Some(pid);
        shared.state = ProcState::Used;

        let res: Result<ProcPrivateData, KernelError> = (|| {
            let private = ProcPrivateData {
                pid,
                kstack: layout::kstack(i),
                pagetable: UserPageTable::new(pid)?,
                ofile: [const { None }; NOFILE],
                cwd: None,
                trace_mask: 0,
                signal_handler_state: None,
            };

            // Set up new context to start executing at forkret,
            // which returns to user space.
            shared.context.clear();
            shared.context.ra = forkret as usize;
            shared.context.sp = private
                .kstack
                .byte_add(KSTACK_PAGES * PAGE_SIZE)
                .unwrap()
                .addr();
            Ok(private)
        })();

        let private = match res {
            Ok(private) => private,
            Err(e) => {
                p.free(&mut shared);
                return Err(e);
            }
        };

        let private = p.init_private(private);

        Ok((p, shared, private))
    }

    /// Frees a proc structure and the data hangind from it,
    /// including user pages.
    ///
    /// p.lock must be held.
    fn free(&self, shared: &mut SpinLockGuard<ProcSharedData>) {
        unsafe {
            self.parent.reset();
        }
        shared.pid = None;
        shared.name.clear();
        shared.killed = false;

        shared.state = ProcState::Unused;
    }
}

/// A fork child's very first scheduling by `scheduler()`
/// will switch for forkret.
extern "C" fn forkret() {
    // Still holding `p->shared` from `scheduler()`.
    let p = Proc::current();
    let _ = unsafe { p.shared.remember_locked() }; // unlock here
    let Some(private) = p.borrow_private() else {
        // process is already exited (in zombie state)
        scheduler::yield_(p);
        unreachable!();
    };

    trap::trap_user_ret(private);
}
