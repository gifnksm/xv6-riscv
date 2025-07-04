#![feature(allocator_api)]
#![cfg_attr(not(test), no_std)]

use core::{
    arch::asm,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use ov6_user_lib::{
    eprint,
    io::{STDERR_FD, STDOUT_FD},
    os::ov6::syscall,
    process::{self, ProcessBuilder},
};
use ov6_user_tests::{
    expect,
    test_runner::{TestEntry, TestParam},
};

fn main() {
    TestParam::parse().run(TESTS);
}

const TESTS: &[TestEntry] = &[
    TestEntry {
        name: "test0",
        test: test0,
        tags: &[],
    },
    TestEntry {
        name: "test1",
        test: test1,
        tags: &[],
    },
    TestEntry {
        name: "test2",
        test: test2,
        tags: &[],
    },
    TestEntry {
        name: "test3",
        test: test3,
        tags: &[],
    },
];

fn msg_in_handler(msg: &'static str) {
    let _ = syscall::write(STDERR_FD, msg.as_bytes());
}

macro_rules! unwrap_in_handler {
    ($val:expr) => {
        match $val {
            Ok(v) => v,
            Err(_e) => {
                msg_in_handler(concat!(
                    "error in signal handler: ",
                    file!(),
                    ":",
                    line!(),
                    "\n"
                ));
                syscall::exit(1);
            }
        }
    };
}

static COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn periodic() {
    COUNT.fetch_add(1, Ordering::Relaxed);
    unwrap_in_handler!(syscall::write(STDOUT_FD, b"alarm!\n"));
    unwrap_in_handler!(syscall::signal_return());
}

/// Tests whether the kernel calls the alarm handler even a single time.
fn test0() {
    COUNT.store(0, Ordering::Relaxed);

    syscall::alarm_set(Duration::from_millis(200), periodic).unwrap();

    let mut incremented = false;
    for i in 0..(1000 * 500_000) {
        if i % 1_000_000 == 0 {
            eprint!(".");
        }
        if COUNT.load(Ordering::Relaxed) > 0 {
            incremented = true;
            break;
        }
    }
    syscall::alarm_clear().unwrap();
    assert!(incremented, "the kernel never called the alarm handler");
}

#[inline(never)]
fn foo(i: usize, j: &mut usize) {
    if i.is_multiple_of(2_500_000) {
        eprint!(".");
    }
    *j += 1;
}

/// Tests that the kernel handler multiple times.
///
/// Tests that, when the handler returns, it returns to the point in the program
/// where the timer interrupt occurred, with all registers holding the same
/// values they held when the interrupt occurred.
fn test1() {
    COUNT.store(0, Ordering::Relaxed);

    syscall::alarm_set(Duration::from_millis(200), periodic).unwrap();
    let mut incremented = false;
    let iter = 1000 * 500_000;
    let mut i = 0;
    let mut j = 0;
    while i < iter {
        if COUNT.load(Ordering::Relaxed) > 10 {
            incremented = true;
            break;
        }
        foo(i, &mut j);
        i += 1;
    }

    assert!(
        incremented,
        "too few calls to the handler: {}",
        COUNT.load(Ordering::Relaxed)
    );

    // the loop should have called `foo()` `iter` times, and `foo()` should
    // have incremented `j` once per call, so `j` should equal `iter`.
    // Once possible source of errors is that the handler may
    // return somewhere other than where the timer interrupt
    // occurred; another is that that registers may not be
    // restored correctly, causing `i` or `j` or the address of `j`
    // to get an incorrect value.
    assert_eq!(j, i, "foo() executed fewer times than it was called");
}

extern "C" fn slow_handler() {
    let count = COUNT.fetch_add(1, Ordering::Relaxed);
    unwrap_in_handler!(syscall::write(STDOUT_FD, b"alarm!\n"));
    if count > 0 {
        msg_in_handler("slow_handler: alarm handler called more than once");
        syscall::exit(1);
    }

    for _i in 0..1000 * 500_000 {
        // avoid compiler optimizing away loop
        unsafe {
            asm!("nop");
        }
    }

    unwrap_in_handler!(syscall::alarm_clear());
    unwrap_in_handler!(syscall::signal_return());
}

/// Tests that kernel does not allow reentrant alarm calls.
fn test2() {
    let status = ProcessBuilder::new()
        .spawn_fn(|| {
            COUNT.store(0, Ordering::Relaxed);
            syscall::alarm_set(Duration::from_millis(200), slow_handler).unwrap();
            let mut incremented = false;
            for i in 0..1000 * 500_000 {
                if i % 1_000_000 == 0 {
                    eprint!(".");
                }
                if COUNT.load(Ordering::Relaxed) > 0 {
                    incremented = true;
                    break;
                }
            }
            assert!(incremented, "alarm not called");
            process::exit(0);
        })
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success());
}

/// Dummy alarm handler
///
/// After running immediately uninstall itself and finish signal handling.
extern "C" fn dummy_handler() {
    unwrap_in_handler!(syscall::alarm_clear());
    unwrap_in_handler!(syscall::signal_return());
}

#[cfg(target_arch = "riscv64")]
fn wait_a0(count: usize) -> usize {
    let a0: usize;
    unsafe {
        asm!(
            "lui a5, 0",
            "addi a0, a5, 0xac",
            "1:",
            "addi t0, t0, -1",
            "bnez t0, 1b",
            out("a0") a0,
            in("t0") count,
        );
    }
    a0
}

#[cfg(not(target_arch = "riscv64"))]
fn wait_a0(_count: usize) -> usize {
    unimplemented!()
}

/// Tests that the return from `signal_return()` does not modify the `a0`
/// register.
fn test3() {
    syscall::alarm_set(Duration::from_millis(1), dummy_handler).unwrap();
    let a0 = wait_a0(500_000_000);
    assert_eq!(a0, 0xac, "register a0 changed");
}
