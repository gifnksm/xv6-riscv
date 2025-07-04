use core::arch::naked_asm;

use crate::interrupt::trap;

/// Interrupts and exceptions while in supervisor mode come here.
///
/// The current stack is a kernel stack.
/// Pushes the registers, call `trap_kernel()`.
/// When `trap_kernel()` returns, pops the registers and returns.
#[unsafe(naked)]
pub extern "C" fn kernel_vec() {
    naked_asm!(
        // make room to save registers.
        "addi sp, sp, -272",

        // save caller-saved registers.
        "sd sp, 0(sp)",
        "sd gp, 8(sp)",
        "sd tp, 16(sp)",
        "sd t0, 24(sp)",
        "sd t1, 32(sp)",
        "sd t2, 40(sp)",
        "sd a0, 48(sp)",
        "sd a1, 72(sp)",
        "sd a2, 80(sp)",
        "sd a3, 88(sp)",
        "sd a4, 96(sp)",
        "sd a5, 104(sp)",
        "sd a6, 112(sp)",
        "sd a7, 120(sp)",
        "sd t3, 128(sp)",
        "sd t4, 216(sp)",
        "sd t5, 224(sp)",
        "sd t6, 232(sp)",
        "sd fp, 240(sp)",
        "sd ra, 248(sp)",

        // A dummy stack frame is added to make it appear as if a function was called
        // from the point where the exception occurred.
        "csrr a0, sepc",
        "sd fp, 256(sp)",
        "sd a0, 264(sp)",
        "addi fp, sp, 272",

        // call the Rust trap handler in trap.rs
        "call {trap_kernel}",

        // restore registers.
        "ld sp, 0(sp)",
        "ld gp, 8(sp)",
        "ld t0, 16(sp)",
        // not tp (contains hartid), in case we moved CPUs
        "ld t1, 32(sp)",
        "ld t2, 40(sp)",
        "ld a0, 48(sp)",
        "ld a1, 72(sp)",
        "ld a2, 80(sp)",
        "ld a3, 88(sp)",
        "ld a4, 96(sp)",
        "ld a5, 104(sp)",
        "ld a6, 112(sp)",
        "ld a7, 120(sp)",
        "ld t3, 128(sp)",
        "ld t4, 216(sp)",
        "ld t5, 224(sp)",
        "ld t6, 232(sp)",
        "ld fp, 240(sp)",
        "ld ra, 248(sp)",

        "addi sp, sp, 272",

        // eturn to whatever we were doing in the kernel.
        "sret",
        trap_kernel = sym trap::trap_kernel
    )
}
