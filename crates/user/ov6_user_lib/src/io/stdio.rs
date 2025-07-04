use core::fmt::{self, Write as _};

use alloc_crate::string::String;
use once_init::OnceInit;

use super::{BufRead, BufReader, BufWriter, IsTerminal, Read, Write};
use crate::{
    error::Ov6Error,
    io::{DEFAULT_BUF_SIZE, LineWriter},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, RawFd},
        ov6::syscall,
    },
    sync::spin::{Mutex, MutexGuard},
};

#[track_caller]
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    match stdout().write_fmt(args) {
        Ok(()) => {}
        Err(fmt::Error) => panic!("Error writing to stdout"),
    }
}

#[track_caller]
#[doc(hidden)]
pub fn _eprint(args: fmt::Arguments) {
    stderr().write_fmt(args).unwrap();
}

pub(crate) fn cleanup() {
    let stdout = STDOUT.try_get();
    if let Ok(stdout) = stdout
        && let Some(mut lock) = stdout.try_lock()
    {
        *lock = LineWriter::with_capacity(0, StdoutRaw {});
    }
}

pub const STDIN_FD: RawFd = RawFd::new(0);
pub const STDOUT_FD: RawFd = RawFd::new(1);
pub const STDERR_FD: RawFd = RawFd::new(2);

static STDOUT: OnceInit<Mutex<LineWriter<StdoutRaw>>> = OnceInit::new();

struct StdoutRaw {}

impl Write for StdoutRaw {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Ov6Error> {
        syscall::write(STDOUT_FD, buf)
    }

    fn flush(&mut self) -> Result<(), Ov6Error> {
        Ok(())
    }
}

struct StderrRaw {}

impl Write for StderrRaw {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Ov6Error> {
        syscall::write(STDERR_FD, buf)
    }

    fn flush(&mut self) -> Result<(), Ov6Error> {
        Ok(())
    }
}

struct StdinRaw {}

impl Read for StdinRaw {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Ov6Error> {
        syscall::read(STDIN_FD, buf)
    }
}

#[must_use]
pub fn stdout() -> Stdout {
    let _ = STDOUT.try_init_with(|| Mutex::new(LineWriter::new(StdoutRaw {})));
    let instance = loop {
        if let Ok(instance) = STDOUT.try_get() {
            break instance;
        }
    };
    let is_terminal = super::is_terminal(STDOUT_FD);
    Stdout {
        inner: instance,
        is_terminal,
    }
}

#[must_use]
pub fn stderr() -> Stderr {
    static INSTANCE: OnceInit<Mutex<BufWriter<StderrRaw>>> = OnceInit::new();
    let _ = INSTANCE.try_init_with(|| Mutex::new(BufWriter::new(StderrRaw {})));
    let instance = loop {
        if let Ok(instance) = INSTANCE.try_get() {
            break instance;
        }
    };
    Stderr { inner: instance }
}

pub fn stdin() -> Stdin {
    static INSTANCE: OnceInit<Mutex<BufReader<StdinRaw>>> = OnceInit::new();
    let _ = INSTANCE
        .try_init_with(|| Mutex::new(BufReader::with_capacity(DEFAULT_BUF_SIZE, StdinRaw {})));
    let instance = loop {
        if let Ok(instance) = INSTANCE.try_get() {
            break instance;
        }
    };
    Stdin { inner: instance }
}

pub struct Stdout {
    inner: &'static Mutex<LineWriter<StdoutRaw>>,
    is_terminal: bool,
}

pub struct StdoutLock<'lock> {
    inner: MutexGuard<'lock, LineWriter<StdoutRaw>>,
    is_terminal: bool,
}

impl AsFd for Stdout {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(STDOUT_FD) }
    }
}

impl AsRawFd for Stdout {
    fn as_raw_fd(&self) -> RawFd {
        STDOUT_FD
    }
}

impl IsTerminal for Stdout {
    fn is_terminal(&self) -> bool {
        self.is_terminal
    }
}

impl AsFd for StdoutLock<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(STDOUT_FD) }
    }
}

impl AsRawFd for StdoutLock<'_> {
    fn as_raw_fd(&self) -> RawFd {
        STDOUT_FD
    }
}

impl IsTerminal for StdoutLock<'_> {
    fn is_terminal(&self) -> bool {
        self.is_terminal
    }
}

impl fmt::Write for StderrLock<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if Write::write(self, s.as_bytes()).is_err() {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

pub struct Stderr {
    inner: &'static Mutex<BufWriter<StderrRaw>>,
}

pub struct StderrLock<'lock> {
    inner: MutexGuard<'lock, BufWriter<StderrRaw>>,
}

impl AsFd for Stderr {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(STDERR_FD) }
    }
}

impl AsRawFd for Stderr {
    fn as_raw_fd(&self) -> RawFd {
        STDERR_FD
    }
}

impl IsTerminal for Stderr {
    fn is_terminal(&self) -> bool {
        self.as_fd().is_terminal()
    }
}

impl AsFd for StderrLock<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(STDERR_FD) }
    }
}

impl AsRawFd for StderrLock<'_> {
    fn as_raw_fd(&self) -> RawFd {
        STDERR_FD
    }
}

impl IsTerminal for StderrLock<'_> {
    fn is_terminal(&self) -> bool {
        self.as_fd().is_terminal()
    }
}

impl Stdout {
    #[must_use]
    pub fn lock(&self) -> StdoutLock<'_> {
        StdoutLock {
            inner: self.inner.lock(),
            is_terminal: self.is_terminal,
        }
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Ov6Error> {
        let mut inner = self.inner.lock();
        let n = inner.write(buf)?;
        if self.is_terminal {
            inner.flush()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), Ov6Error> {
        self.inner.lock().flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), Ov6Error> {
        let mut inner = self.inner.lock();
        inner.write_all(buf)?;
        if self.is_terminal {
            inner.flush()?;
        }
        Ok(())
    }
}

impl fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if Write::write(self, s.as_bytes()).is_err() {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl Write for StdoutLock<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Ov6Error> {
        let n = self.inner.write(buf)?;
        if self.is_terminal {
            self.inner.flush()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), Ov6Error> {
        self.inner.flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), Ov6Error> {
        self.inner.write_all(buf)?;
        if self.is_terminal {
            self.inner.flush()?;
        }
        Ok(())
    }
}

impl fmt::Write for StdoutLock<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if Write::write(self, s.as_bytes()).is_err() {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl Stderr {
    #[must_use]
    pub fn lock(&self) -> StderrLock<'_> {
        StderrLock {
            inner: self.inner.lock(),
        }
    }
}

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Ov6Error> {
        let mut inner = self.inner.lock();
        let n = inner.write(buf)?;
        inner.flush()?;
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), Ov6Error> {
        self.inner.lock().flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), Ov6Error> {
        let mut inner = self.inner.lock();
        inner.write_all(buf)?;
        inner.flush()?;
        Ok(())
    }
}

impl fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if Write::write(self, s.as_bytes()).is_err() {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl Write for StderrLock<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Ov6Error> {
        let n = self.inner.write(buf)?;
        self.inner.flush()?;
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), Ov6Error> {
        self.inner.flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), Ov6Error> {
        self.inner.write_all(buf)?;
        self.inner.flush()?;
        Ok(())
    }
}

pub struct Stdin {
    inner: &'static Mutex<BufReader<StdinRaw>>,
}

pub struct StdinLock<'lock> {
    inner: MutexGuard<'lock, BufReader<StdinRaw>>,
}

impl Stdin {
    #[must_use]
    pub fn lock(&self) -> StdinLock<'_> {
        StdinLock {
            inner: self.inner.lock(),
        }
    }

    pub fn read_line(&mut self, buf: &mut String) -> Result<usize, Ov6Error> {
        let mut locked = self.lock();
        locked.read_line(buf)
    }
}

impl AsFd for Stdin {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(STDIN_FD) }
    }
}

impl AsRawFd for Stdin {
    fn as_raw_fd(&self) -> RawFd {
        STDIN_FD
    }
}

impl IsTerminal for Stdin {
    fn is_terminal(&self) -> bool {
        self.as_fd().is_terminal()
    }
}

impl AsFd for StdinLock<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(STDIN_FD) }
    }
}

impl AsRawFd for StdinLock<'_> {
    fn as_raw_fd(&self) -> RawFd {
        STDIN_FD
    }
}

impl IsTerminal for StdinLock<'_> {
    fn is_terminal(&self) -> bool {
        self.as_fd().is_terminal()
    }
}

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Ov6Error> {
        self.inner.lock().read(buf)
    }
}

impl Read for StdinLock<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Ov6Error> {
        self.inner.read(buf)
    }
}

impl BufRead for StdinLock<'_> {
    fn fill_buf(&mut self) -> Result<&[u8], Ov6Error> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
    }
}
