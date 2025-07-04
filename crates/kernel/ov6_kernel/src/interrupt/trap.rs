use core::{mem, ptr};

use dataview::Pod;
use riscv::{
    interrupt::{
        Trap,
        supervisor::{Exception, Interrupt},
    },
    register::{
        satp, scause, sepc,
        sstatus::{self, SPP},
        stval,
        stvec::{self, Stvec, TrapMode},
    },
};
use safe_cast::SafeInto as _;

use super::{clic, kernel_vec, plic, timer, trampoline};
use crate::{
    console::uart,
    cpu, device,
    error::KernelError,
    fs,
    interrupt::{self, timer::Uptime},
    memory::{
        PAGE_SIZE, VirtAddr,
        layout::{E1000_IRQ, KSTACK_PAGES, UART0_IRQ, VIRTIO0_IRQ},
        page_table::PtEntryFlags,
        vm_user::UserPageTable,
    },
    println,
    proc::{self, Proc, ProcPrivateData, ProcPrivateDataGuard, scheduler},
    syscall,
};

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod)]
pub struct UserRegisters {
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod)]
pub struct TrapFrame {
    /// Kernel page table.
    pub kernel_satp: usize, // 0
    /// Top of process's kernel stack.
    pub kernel_sp: usize, // 8
    /// usertrap.
    pub kernel_trap: usize, // 16
    /// Saved user program counter.
    pub epc: usize, // 24
    /// saved kernel tp
    pub kernel_hartid: usize, // 32
    pub user_registers: UserRegisters,
}

pub fn init_hart() {
    let mut stvec = stvec::Stvec::from_bits(0);
    stvec.set_address(kernel_vec::kernel_vec as usize);
    stvec.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(stvec);
    }
}

/// Handles an interrupt, exception, or system call from user space.
///
/// Called from trampoline.S
extern "C" fn trap_user() {
    assert_eq!(sstatus::read().spp(), SPP::User, "from user mode");

    // send interrupts and exceptions to kerneltrap(),
    // since we're now in the kernel.
    let mut stvec = stvec::Stvec::from_bits(0);
    stvec.set_address(kernel_vec::kernel_vec as usize);
    stvec.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(stvec);
    }

    let p = Proc::current();
    let mut private = p.borrow_private().unwrap();

    // save user program counter.
    let tf = private.trapframe_mut();
    tf.epc = sepc::read();

    let scause: Trap<Interrupt, Exception> = scause::read().cause().try_into().unwrap();
    let mut which_dev = IntrKind::NotRecognized;
    match scause {
        Trap::Exception(Exception::UserEnvCall) => {
            // system call
            if p.shared().lock().killed() {
                proc::ops::exit(p, private, -1);
            }

            // sepc points to the ecall instruction,
            // but we want to return to the next instruction.
            tf.epc += 4;

            // an interrupt will change sepc, scause, and sstatus,
            // so enable only now that we're done with those registers.
            interrupt::enable();

            let mut private_opt = Some(private);
            syscall::syscall(p, &mut private_opt);
            private = private_opt.unwrap();
        }
        Trap::Exception(Exception::StorePageFault)
            if request_user_write(&mut private, stval::read()).is_ok() => {}
        Trap::Exception(e) => {
            let mut shared = p.shared().lock();
            let pid = shared.pid();
            let name = shared.name().display();
            let sepc = sepc::read();
            let stval = stval::read();

            println!("usertrap: exception {e:?} pid={pid} name={name}");
            println!("          sepc={sepc:#x} stval={stval:#x}");
            print_user_backtrace(sepc, private.trapframe(), private.pagetable());
            shared.kill();
        }
        Trap::Interrupt(int) => {
            which_dev = handle_dev_interrupt(int);
            if which_dev == IntrKind::NotRecognized {
                let mut shared = p.shared().lock();
                let pid = shared.pid();
                let name = shared.name().display();
                let sepc = sepc::read();
                let stval = stval::read();
                println!("usertrap: unexpected interrupt {int:?} pid={pid} name={name}");
                println!("          sepc={sepc:#x} stval={stval:#x}");
                print_user_backtrace(sepc, private.trapframe(), private.pagetable());
                shared.kill();
            }
        }
    }

    {
        let mut shared = p.shared().lock();
        if shared.killed() {
            drop(shared);
            proc::ops::exit(p, private, -1);
        }
        if let Some(alarm) = shared.alarm_mut() {
            let now = Uptime::now();
            if alarm.is_expired(now) {
                alarm.update(now);
                private.enter_signal_handler(alarm.handler());
            }
        }
    }

    // gibe up the CPU if this is a timer interrupt.
    if which_dev == IntrKind::Timer {
        scheduler::yield_(p);
    }

    trap_user_ret(private);
}

fn request_user_write(private: &mut ProcPrivateData, addr: usize) -> Result<(), KernelError> {
    let va = VirtAddr::new(addr)?;
    private.pagetable_mut().request_user_write(va)?;
    Ok(())
}

fn fetch_usize(addr: usize, pt: &UserPageTable) -> Option<usize> {
    if !addr.is_multiple_of(8) {
        return None;
    }
    let va = VirtAddr::new(addr).ok()?;
    let data = pt.fetch_chunk(va, PtEntryFlags::URW).ok()?;
    if data.len() < mem::size_of::<usize>() {
        return None;
    }
    let val = usize::from_ne_bytes(data[..mem::size_of::<usize>()].try_into().unwrap());
    Some(val)
}

fn print_user_backtrace(epc: usize, tf: &TrapFrame, pt: &UserPageTable) {
    println!("user backtrace:");
    println!("{:#p}", ptr::without_provenance::<u8>(epc));
    println!("{:#p}", ptr::without_provenance::<u8>(tf.user_registers.ra));

    let mut fp = tf.user_registers.s0;
    let mut depth = 0;
    while fp != 0 {
        let Some(ra) = fp.checked_sub(8).and_then(|addr| fetch_usize(addr, pt)) else {
            println!("invalid frame pointer {fp:#x}");
            break;
        };
        if ra != 0 {
            println!("{:#p}", ptr::without_provenance::<u8>(ra));
        }
        let Some(prev_fp) = fp.checked_sub(16).and_then(|addr| fetch_usize(addr, pt)) else {
            println!("invalid frame pointer {fp:#x}");
            break;
        };
        fp = prev_fp;
        depth += 1;
        if depth > 10 {
            println!("... (truncated)");
            break;
        }
    }
}

/// Returns to user space
pub fn trap_user_ret(mut private: ProcPrivateDataGuard) {
    // we're about to switch destination of traps from
    // kerneltrap() to usertrap(), so turn off interrupts until
    // we're back in user space, where usertrap() is correct.
    interrupt::disable();

    // send syscalls, interrupts, and exceptions to uservec in trampoline.S
    let trampoline_uservec = trampoline::user_vec_addr();
    let mut stvec = Stvec::from_bits(0);
    stvec.set_address(trampoline_uservec.addr());
    stvec.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(stvec);
    }

    // set up trapframe values that uservec will need when
    // the process next traps into the kernel.
    let kstack = private.kstack();
    let tf = private.trapframe_mut();
    tf.kernel_satp = satp::read().bits(); // kernel page table
    tf.kernel_sp = kstack.byte_add(KSTACK_PAGES * PAGE_SIZE).unwrap().addr(); // process's kernel stack
    tf.kernel_trap = trap_user as usize;
    tf.kernel_hartid = cpu::id();
    let epc = tf.epc;

    // set up the registers that trampoline.S's sret will use
    // to get to user space.

    // set S Previous Privilege mode to User.
    unsafe {
        sstatus::set_spp(SPP::User);
        sstatus::set_spie();
    }

    // set S Exception Program Counter to the saved user pc.
    unsafe {
        sepc::write(epc);
    }

    // tell trampoline.S the user page table to switch to.
    let satp = private.pagetable().satp().bits();
    drop(private);

    // jump to userret in trampoline.S at the top of memory, which
    // switches to the user page table, restores user registers,
    // and switches to user mode with sret.
    let trampoline_user_ret = trampoline::user_ret_addr();
    unsafe {
        let f: extern "C" fn(usize) = mem::transmute(trampoline_user_ret.addr());
        f(satp);
    }
}

/// Interrupts and exceptions from kernel code go here via kernelvec,
/// on whatever the current kernel stack is.
pub extern "C" fn trap_kernel() {
    let sepc = sepc::read();
    let sstatus = sstatus::read();
    let scause: Trap<Interrupt, Exception> = scause::read().cause().try_into().unwrap();

    assert_eq!(sstatus.spp(), SPP::Supervisor, "from supervisor mode");
    assert!(!interrupt::is_enabled());

    let (int, which_dev) = match scause {
        Trap::Exception(e) => {
            let stval = stval::read();
            println!("kernel trap: exception {e:#?}");
            println!("             sepc={sepc:#x} stval={stval:#x}");
            panic!("unexpected trap (exception)");
        }
        Trap::Interrupt(int) => (int, handle_dev_interrupt(int)),
    };

    match which_dev {
        IntrKind::Timer => {
            // give up the CPU if this is a timer interrupt.
            if let Some(p) = Proc::try_current() {
                scheduler::yield_(p);
            }
        }
        IntrKind::InterProcessor | IntrKind::Other => {}
        IntrKind::NotRecognized => {
            let stval = stval::read();
            println!("kernel trap: interrupt {int:?}");
            println!("             sepc={sepc:#x} stval={stval:#x}");
            panic!("unexpected trap (interrupt)");
        }
    }

    // the yield_() may have caused some traps to occur,
    // so restore trap registers for use by kernelvec's sepc instruction.
    unsafe {
        sepc::write(sepc);
    }
    unsafe {
        sstatus::write(sstatus);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntrKind {
    Timer,
    Other,
    InterProcessor,
    NotRecognized,
}

/// Check if it's an external interrupt of software interrupt,
/// and handle it.
///
/// return 2 if timer interrupt,
/// 1 if other device,
/// 0 if not recognized
fn handle_dev_interrupt(int: Interrupt) -> IntrKind {
    match int {
        Interrupt::SupervisorSoft => IntrKind::NotRecognized,
        Interrupt::SupervisorTimer => {
            timer::handle_interrupt();
            IntrKind::Timer
        }
        Interrupt::SupervisorExternal => {
            // this is a supervisor external interrupt, via PLIC.

            if clic::is_software_interrupt_pending() {
                clic::complete_software_interrupt();
                return IntrKind::InterProcessor;
            }

            // irq indicates which device interrupted.
            let irq = plic::claim();

            match irq.safe_into() {
                UART0_IRQ => uart::handle_interrupt(),
                VIRTIO0_IRQ => fs::virtio_disk::handle_interrupt(),
                E1000_IRQ => device::e1000::handle_interrupt(),
                irq => {
                    if irq > 0 {
                        println!("unexpected interrupt irq={irq}");
                    }
                }
            }

            // the PLIC allows each device to raise at most one
            // interrupt at a time; tell the PLIC the device is
            // now allowed to interrupt again.
            if irq > 0 {
                plic::complete(irq);
            }
            IntrKind::Other
        }
    }
}
