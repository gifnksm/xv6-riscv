use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use mutex_api::Mutex;

use crate::{
    cpu::{self, INVALID_CPUID},
    interrupt,
    proc::{self, ops::SleepError},
};

#[derive(Debug, thiserror::Error)]
pub enum TryLockError {
    #[error("lock is already held")]
    Locked,
}

#[derive(Default)]
pub struct SpinLock<T> {
    locked: AtomicBool,
    cpuid: UnsafeCell<usize>,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for SpinLock<T> where T: Send {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            cpuid: UnsafeCell::new(INVALID_CPUID),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquires the lock.
    ///
    /// Loops (spins) until the lock is acquired.
    #[track_caller]
    pub fn try_lock(&self) -> Result<SpinLockGuard<'_, T>, TryLockError> {
        // disable interrupts to avoid deadlock.
        let int_guard = interrupt::push_disabled();

        assert!(!self.holding(), "lock is already hold");

        // `Ordering::Acquire` tells the compiler and the processor to not move loads or
        // stores past this point, to ensure that the critical section's memory
        // references happen strictly after the lock is acquired.
        // On RISC-V, this emits a fence instruction.
        if self.locked.swap(true, Ordering::Acquire) {
            return Err(TryLockError::Locked);
        }

        // Record info about lock acquisition for holding() and debugging.
        unsafe {
            *self.cpuid.get() = cpu::id();
        }

        int_guard.forget(); // drop re-enables interrupts, so we must forget it here.

        Ok(SpinLockGuard { lock: self })
    }

    /// Acquires the lock.
    ///
    /// Loops (spins) until the lock is acquired.
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        // disable interrupts to avoid deadlock.
        let int_guard = interrupt::push_disabled();

        assert!(!self.holding(), "lock is already hold");

        // `Ordering::Acquire` tells the compiler and the processor to not move loads or
        // stores past this point, to ensure that the critical section's memory
        // references happen strictly after the lock is acquired.
        // On RISC-V, this emits a fence instruction.
        while self.locked.swap(true, Ordering::Acquire) {}

        // Record info about lock acquisition for holding() and debugging.
        unsafe {
            *self.cpuid.get() = cpu::id();
        }

        int_guard.forget(); // drop re-enables interrupts, so we must forget it here.

        SpinLockGuard { lock: self }
    }

    pub unsafe fn remember_locked(&self) -> SpinLockGuard<'_, T> {
        assert!(self.holding());
        SpinLockGuard { lock: self }
    }

    /// Checks whether this cpu is holding the lock.
    ///
    /// Interrupts must be off.
    fn holding(&self) -> bool {
        assert!(!interrupt::is_enabled());
        self.locked.load(Ordering::Relaxed) && unsafe { *self.cpuid.get() } == cpu::id()
    }
}

impl<T> Mutex for SpinLock<T> {
    type Data = T;
    type Guard<'a>
        = SpinLockGuard<'a, T>
    where
        T: 'a;

    fn new(data: Self::Data) -> Self {
        Self::new(data)
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.lock()
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

unsafe impl<T> Send for SpinLockGuard<'_, T> where T: Send {}
unsafe impl<T> Sync for SpinLockGuard<'_, T> where T: Sync {}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        assert!(self.lock.holding());

        unsafe {
            *self.lock.cpuid.get() = INVALID_CPUID;
        }

        // `Ordering::Release` tells the compiler and the CPU to not move loads or
        // stores past this point, to ensure that all the stores in the critical
        // section are visible to other CPUs before the lock is released,
        // and that loads in the critical section occur strictly before
        // the locks is released.
        // On RISC-V, this emits a fence instruction.
        self.lock.locked.store(false, Ordering::Release);

        unsafe {
            interrupt::pop_disabled();
        }
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<'a, T> SpinLockGuard<'a, T> {
    pub fn into_lock(self) -> &'a SpinLock<T> {
        self.lock
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("caller process already killed")]
    WaitingProcessAlreadyKilled,
}

impl From<SleepError> for WaitError {
    fn from(e: SleepError) -> Self {
        match e {
            SleepError::SleepingProcessAlreadyKilled => Self::WaitingProcessAlreadyKilled,
        }
    }
}

#[derive(Default)]
pub struct SpinLockCondVar {
    counter: AtomicU64,
}

impl SpinLockCondVar {
    pub const fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }

    pub fn wait<'a, T>(
        &self,
        mut guard: SpinLockGuard<'a, T>,
    ) -> Result<SpinLockGuard<'a, T>, (SpinLockGuard<'a, T>, WaitError)> {
        let counter = self.counter.load(Ordering::Relaxed);
        loop {
            guard = proc::ops::sleep(self, guard).map_err(|(guard, e)| (guard, e.into()))?;
            if counter != self.counter.load(Ordering::Relaxed) {
                break;
            }
        }
        Ok(guard)
    }

    pub fn force_wait<'a, T>(&self, mut guard: SpinLockGuard<'a, T>) -> SpinLockGuard<'a, T> {
        let counter = self.counter.load(Ordering::Relaxed);
        loop {
            guard = proc::ops::force_sleep(self, guard);
            if counter != self.counter.load(Ordering::Relaxed) {
                break;
            }
        }
        guard
    }

    pub fn notify(&self) {
        self.counter.fetch_add(1, Ordering::Relaxed);
        proc::ops::wakeup(self);
    }
}
