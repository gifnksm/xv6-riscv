use core::{cmp, ptr};

use ov6_syscall::{RegisterValue as _, ReturnType, WaitTarget, syscall as sys};
use ov6_types::{os_str::OsStr, path::Path, process::ProcId};

use super::{PROC, ProcPrivateData, ProcPrivateDataGuard, ProcShared, WaitLock};
use crate::{
    cpu,
    error::KernelError,
    fs::{self, DeviceNo, Inode, TxInode},
    interrupt::{clic, trap},
    memory::page_table::PtEntryFlags,
    println,
    proc::{INIT_PROC, Proc, ProcState, scheduler, wait_lock},
    sync::{SpinLockCondVar, SpinLockGuard, WaitError},
    syscall::ReturnValue,
};

/// Set up first user process.
pub fn spawn_init() {
    let (p, mut shared, mut private) = Proc::allocate().unwrap();
    assert_eq!(shared.pid.unwrap().get().get(), 1);
    INIT_PROC.init(p);

    shared.context.ra = forkret_init as usize;

    let tx = fs::begin_readonly_tx();
    private.cwd = Some(Inode::from_tx(&TxInode::root(&tx)));
    tx.end();
    shared.set_name(OsStr::new("spawn_init"));
    shared.state = ProcState::Runnable;

    drop(shared);
}

extern "C" fn forkret_init() {
    // Still holding `p->shared` from `scheduler()`.
    let p = Proc::current();
    let _ = unsafe { p.shared.remember_locked() }; // unlock here

    let Some(mut private) = p.borrow_private() else {
        // process is already exited (in zombie state)
        scheduler::yield_(p);
        unreachable!();
    };

    // File system initialization must be run in the context of a
    // regular process (e.g., because it calls sleep), and thus cannot
    // be run from main().
    fs::init_in_proc(DeviceNo::ROOT);

    let argv: &[&[u8]] = &[b"/init"];
    let arg_data_size = argv.iter().map(|arg| arg.len() + 1).sum();
    let (a0, a1) =
        super::exec::exec(p, &mut private, Path::new("/init"), &argv, arg_data_size).unwrap();

    let tf = private.trapframe_mut();
    tf.user_registers.a0 = a0;
    tf.user_registers.a1 = a1.addr();

    trap::trap_user_ret(private);
}

/// Grows user memory by `n` Bytes.
pub fn resize_by(private: &mut ProcPrivateData, increment: isize) -> Result<(), KernelError> {
    let pagetable = private.pagetable_mut();
    let amt = increment.saturating_abs().cast_unsigned();
    match increment.cmp(&0) {
        cmp::Ordering::Less => pagetable.shrink_heap_by(amt)?,
        cmp::Ordering::Equal => {}
        cmp::Ordering::Greater => pagetable.grow_heap_by(amt, PtEntryFlags::URW)?,
    }
    Ok(())
}

/// Creates a new process, copying the parent.
///
/// Sets up child kernel stack to return as if from `fork()` system call.
pub fn fork(p: &'static Proc, p_private: &mut ProcPrivateData) -> Result<ProcId, KernelError> {
    let parent_name = p.shared().lock().name.clone();

    // Allocate process.
    let (np, mut np_shared, mut np_private) = Proc::allocate()?;

    // Copy use memory from parent to child.
    if let Err(e) = np_private
        .pagetable_mut()
        .clone_from(p_private.pagetable_mut())
    {
        np.free(&mut np_shared);
        drop(np_shared);
        return Err(e);
    }

    // Copy saved user registers.
    let np_tf = np_private.trapframe_mut();
    *np_tf = *p_private.trapframe();

    // Cause fork to return 0 in the child.
    let child_ret: ReturnType<sys::Fork> = Ok(None);
    ReturnValue::from(child_ret.encode()).store(np_tf);

    // increment refereence counts on open file descriptors.
    for (of, nof) in p_private.ofile.iter().zip(&mut np_private.ofile) {
        if let Some(of) = of {
            *nof = Some(of.dup());
        }
    }
    np_private.cwd.clone_from(&p_private.cwd);
    np_private.trace_mask = p_private.trace_mask;
    np_shared.name = parent_name;

    let pid = np_shared.pid.unwrap();
    drop(np_shared);

    let mut wait_lock = wait_lock::lock();
    np.parent.set(p, &mut wait_lock);
    drop(wait_lock);

    // After setting the state to Runnable, the scheduler can pick up `np` and the
    // process context may start. The started process context (e.g., forkret)
    // will refer to `ProcPrivateData`, so we must drop `np_private` here.
    drop(np_private);
    np.shared.lock().state = ProcState::Runnable;

    Ok(pid)
}

/// Pass p's abandoned children to init.
///
/// Caller must hold `WAIT_LOCK`
fn reparent(old_parent: &Proc, new_parent: &'static Proc, wait_lock: &mut SpinLockGuard<WaitLock>) {
    for pp in &PROC {
        if pp.is_child_of(old_parent, wait_lock) {
            pp.set_parent(new_parent, wait_lock);
            new_parent.child_ended.notify();
        }
    }
}

/// Exits the current process.
///
/// Does not return.
/// An exited process remains in the zombie state
/// until its parent calls `wait()`.
pub fn exit(p: &Proc, mut p_private: ProcPrivateDataGuard, status: i32) -> ! {
    let init_proc = *INIT_PROC.get();

    // Ensure shared is not locked
    drop(p.shared().lock());

    // Ensure all destruction is done before `sched().`
    let mut shared = {
        assert!(!ptr::eq(p, init_proc), "init exiting");

        // Close all open files.
        for of in &mut p_private.ofile {
            if let Some(of) = of.take() {
                of.close();
            }
        }

        let tx = fs::force_begin_tx();
        p_private.cwd.take().unwrap().into_tx(&tx).put();
        tx.end();

        let mut wait_lock = wait_lock::lock();

        // Give any children to init.
        reparent(p, init_proc, &mut wait_lock);

        // Parent might be sleeping in wait().
        if let Some(parent) = p.parent.get(&mut wait_lock) {
            parent.child_ended.notify();
        }

        let mut shared = p.shared.lock();
        shared.state = ProcState::Zombie {
            exit_status: status,
        };

        p_private.remove_private();

        drop(wait_lock);
        shared
    };

    // Jump into the scheduler, never to return.
    scheduler::sched(&mut shared);

    unreachable!("zombie exit");
}

/// Waits for a child process to exit and return its pid.
///
/// Returns `Err` if this process has no children.
pub fn wait(p: &Proc, target: WaitTarget) -> Result<(ProcId, i32), KernelError> {
    let mut wait_lock = wait_lock::lock();

    loop {
        let mut found = false;
        for pp in &PROC {
            if !pp.is_child_of(p, &mut wait_lock) {
                continue;
            }

            // Make sure the child isn't still in `exit()` or `switch()``.
            let mut pp_shared = pp.shared.lock();

            let find_more;
            match target {
                WaitTarget::AnyProcess => {
                    found = true;
                    find_more = true;
                }
                WaitTarget::Process(pid) => {
                    if pid != pp_shared.pid() {
                        continue;
                    }
                    found = true;
                    find_more = false;
                }
            }

            if let ProcState::Zombie { exit_status } = pp_shared.state {
                // Found one.
                let pid = pp_shared.pid.unwrap();
                pp.free(&mut pp_shared);
                return Ok((pid, exit_status));
            }

            if !find_more {
                break;
            }
        }

        if !found {
            return Err(KernelError::NoWaitTarget);
        }

        // Wait for a child to exit.
        wait_lock = match p.child_ended.wait(wait_lock) {
            Ok(wait_lock) => wait_lock,
            Err((_wait_lock, WaitError::WaitingProcessAlreadyKilled)) => {
                return Err(KernelError::CallerProcessAlreadyKilled);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SleepError {
    #[error("caller process already killed")]
    SleepingProcessAlreadyKilled,
}

/// Automatically releases `lock` and sleeps on `chan`.
///
/// Reacquires lock when awakened.
///
/// Returns `Err` if the process is killed.
pub fn sleep<'a, T>(
    cond: &SpinLockCondVar,
    guard: SpinLockGuard<'a, T>,
) -> Result<SpinLockGuard<'a, T>, (SpinLockGuard<'a, T>, SleepError)> {
    sleep_common(cond, guard, false)
}

/// Automatically releases `lock` and sleeps on `chan`.
///
/// Reacquires lock when awakened.
/// Continues even if the process is killed.
pub fn force_sleep<'a, T>(
    cond: &SpinLockCondVar,
    guard: SpinLockGuard<'a, T>,
) -> SpinLockGuard<'a, T> {
    sleep_common(cond, guard, true).unwrap_or_else(|_| unreachable!())
}

/// Automatically releases `lock` and sleeps on `chan`.
///
/// Reacquires lock when awakened.
fn sleep_common<'a, T>(
    cond: &SpinLockCondVar,
    guard: SpinLockGuard<'a, T>,
    continue_if_killed: bool,
) -> Result<SpinLockGuard<'a, T>, (SpinLockGuard<'a, T>, SleepError)> {
    let p = ProcShared::current();
    // Must acquire `p.lock` in order to change
    // `p.state` and then call `sched()`.
    // Once we hold `p.lock()`, we can be
    // guaranteed that we won't miss any wakeup
    // (wakeup locks `p.lock`),
    // so it's okay to release `lock' here.`
    let mut shared = p.lock();

    if !continue_if_killed && shared.killed() {
        return Err((guard, SleepError::SleepingProcessAlreadyKilled));
    }

    let lock = guard.into_lock();

    // Go to sleep.
    let cond = ptr::from_ref(cond).addr();
    shared.state = ProcState::Sleeping { chan: cond };

    scheduler::sched(&mut shared);

    let killed = shared.killed();

    // Reacquire original lock.
    drop(shared);
    let guard = lock.lock();

    if !continue_if_killed && killed {
        return Err((guard, SleepError::SleepingProcessAlreadyKilled));
    }

    Ok(guard)
}

/// Wakes up all processes sleeping on `chan`.
///
/// Must be called without any processes locked.
pub fn wakeup(cond: &SpinLockCondVar) {
    let mut wakeup = 0;
    let cond = ptr::from_ref(cond).addr();
    for p in &PROC {
        let mut shared = p.shared.lock();
        if let ProcState::Sleeping { chan: ch } = shared.state
            && ch == cond
        {
            shared.state = ProcState::Runnable;
            wakeup += 1;
        }
        drop(shared);
    }

    if wakeup > 0 {
        let mut woken = 0;
        let cpuid = cpu::id();
        for i in 0..cpu::num_cpus() {
            if i == cpuid {
                continue;
            }
            if !cpu::is_idle(i) {
                continue;
            }
            clic::send_software_interrupt(i);
            woken += 1;
            if woken >= wakeup {
                break;
            }
        }
    }
}

/// Kills the process with the given PID.
///
/// The victim won't exit until it tries to return
/// to user spaec (see `usertrap()`).
pub fn kill(pid: ProcId) -> Result<(), KernelError> {
    for p in &PROC {
        let mut shared = p.shared.lock();
        if shared.pid == Some(pid) {
            shared.killed = true;
            if let ProcState::Sleeping { .. } = shared.state {
                // Wake process from sleep().
                shared.state = ProcState::Runnable;
            }
            drop(shared);
            return Ok(());
        }
        drop(shared);
    }
    Err(KernelError::ProcessNotFound(pid))
}

/// Prints a process listing to console.
///
/// For debugging.
/// Runs when user type ^P on console
pub fn dump() {
    println!();
    for p in &PROC {
        let shared = p.shared.lock();
        let pid = shared.pid;
        let state = shared.state;
        let name = shared.name.clone();
        drop(shared);
        if state == ProcState::Unused {
            continue;
        }

        let state = match state {
            ProcState::Unused => "unused",
            ProcState::Used => "used",
            ProcState::Sleeping { .. } => "sleep",
            ProcState::Runnable => "runble",
            ProcState::Running => "run",
            ProcState::Zombie { .. } => "zombie",
        };

        let pid = pid.unwrap();
        let name = OsStr::from_bytes(&name).display();
        println!("{pid:5} {state:<10} {name}");
    }
}
