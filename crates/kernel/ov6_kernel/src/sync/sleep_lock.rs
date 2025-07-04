use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
};

use mutex_api::Mutex;
use ov6_types::process::ProcId;

use super::{SpinLock, SpinLockCondVar, TryLockError, WaitError};
use crate::cpu::Cpu;

#[derive(Default)]
pub struct SleepLock<T> {
    locked: SpinLock<(bool, Option<ProcId>)>,
    unlocked: SpinLockCondVar,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for SleepLock<T> where T: Send {}

#[derive(Debug, thiserror::Error)]
pub enum SleepLockError {
    #[error("requester process is already killed")]
    LockingProcessAlreadyKilled,
}

impl<T> SleepLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: SpinLock::new((false, None)),
            unlocked: SpinLockCondVar::new(),
            value: UnsafeCell::new(value),
        }
    }

    pub fn try_lock(&self) -> Result<SleepLockGuard<'_, T>, TryLockError> {
        let mut locked = self.locked.try_lock()?;
        if locked.0 {
            return Err(TryLockError::Locked);
        }

        locked.0 = true;
        locked.1 = Cpu::current().pid();

        Ok(SleepLockGuard { lock: self })
    }

    /// Acquires the lock.
    ///
    /// Sleeps (spins) until the lock is acquired.
    pub fn force_wait_lock(&self) -> SleepLockGuard<'_, T> {
        let mut locked = self.locked.lock();
        while locked.0 {
            locked = self.unlocked.force_wait(locked);
        }
        locked.0 = true;
        locked.1 = Cpu::current().pid();

        SleepLockGuard { lock: self }
    }

    /// Acquires the lock.
    ///
    /// Sleeps (spins) until the lock is acquired.
    pub fn wait_lock(&self) -> Result<SleepLockGuard<'_, T>, SleepLockError> {
        let mut locked = self.locked.lock();
        while locked.0 {
            match self.unlocked.wait(locked) {
                Ok(guard) => locked = guard,
                Err((_guard, WaitError::WaitingProcessAlreadyKilled)) => {
                    return Err(SleepLockError::LockingProcessAlreadyKilled);
                }
            }
        }
        locked.0 = true;
        locked.1 = Cpu::current().pid();

        Ok(SleepLockGuard { lock: self })
    }
}

impl<T> Mutex for SleepLock<T> {
    type Data = T;
    type Guard<'a>
        = SleepLockGuard<'a, T>
    where
        T: 'a;

    fn new(data: Self::Data) -> Self {
        Self::new(data)
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.force_wait_lock()
    }
}

pub struct SleepLockGuard<'a, T> {
    lock: &'a SleepLock<T>,
}

unsafe impl<T> Send for SleepLockGuard<'_, T> where T: Send {}
unsafe impl<T> Sync for SleepLockGuard<'_, T> where T: Sync {}

impl<T> Drop for SleepLockGuard<'_, T> {
    fn drop(&mut self) {
        let mut locked = self.lock.locked.lock();
        locked.0 = false;
        locked.1 = None;
        self.lock.unlocked.notify();
        drop(locked);
    }
}

impl<T> Deref for SleepLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SleepLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}
