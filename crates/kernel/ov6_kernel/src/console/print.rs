//! Formatted console output

use core::{
    arch::asm,
    fmt::{self, Write as _},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    console,
    device::test,
    sync::{SpinLock, SpinLockGuard},
};

pub static PANICKED: AtomicBool = AtomicBool::new(false);

// lock to avoid interleaving concurrent print's.
struct Print {
    locking: AtomicBool,
    lock: SpinLock<()>,
}

static PRINT: Print = Print {
    locking: AtomicBool::new(true),
    lock: SpinLock::new(()),
};

impl Print {
    fn lock(&self) -> Writer<'_> {
        let guard = self
            .locking
            .load(Ordering::Relaxed)
            .then(|| self.lock.lock());
        Writer { _guard: guard }
    }
}

struct Writer<'a> {
    _guard: Option<SpinLockGuard<'a, ()>>,
}

impl fmt::Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            console::put_char(c);
        }
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let mut writer = PRINT.lock();
    writer.write_fmt(args).unwrap();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        #[expect(clippy::used_underscore_items)]
        $crate::console::print::_print(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*))
    };
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    PRINT.locking.store(false, Ordering::Relaxed);
    println!("panic: {info}");
    print_backtrace();
    PANICKED.store(true, Ordering::Relaxed); // freeze uart output from other CPUs
    test::finish(test::Finisher::Fail(255));
}

fn print_backtrace() {
    println!("backtrace:");

    let mut fp: *const *const usize;
    unsafe {
        asm!(
            "mv {fp}, s0",
            fp = out(reg) fp,
        );
    }

    let mut depth = 0;
    while !fp.is_null() {
        let ra = unsafe { *fp.sub(1) };
        if !ra.is_null() {
            println!("{ra:#p}");
        }
        let prev_fp = unsafe { *fp.sub(2) };
        fp = prev_fp.cast();
        depth += 1;

        if depth > 100 {
            println!("... (truncated)");
            break;
        }
    }
}
