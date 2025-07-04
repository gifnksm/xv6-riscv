//! Utilities for controlling interrupt enability.

use core::{
    mem,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use riscv::register::sstatus;

use crate::{cpu, param::NCPU};

pub mod clic;
mod kernel_vec;
pub mod plic;
pub mod timer;
pub mod trampoline;
pub mod trap;

/// Enables interrupts.
pub fn enable() {
    unsafe {
        sstatus::set_sie();
    }
}

/// Disables interrupts.
pub fn disable() {
    unsafe {
        sstatus::clear_sie();
    }
}

/// Returns `true` if interrupts are enabled.
pub fn is_enabled() -> bool {
    sstatus::read().sie()
}

/// Returns depth of [`push_disabled()`] calls.
pub fn disabled_depth() -> usize {
    let cpuid = cpu::id();
    CPU_STATE[cpuid].push_depth.load(Ordering::Relaxed)
}

pub fn is_enabled_before_push() -> bool {
    let cpuid = cpu::id();
    CPU_STATE[cpuid].int_enabled.load(Ordering::Relaxed)
}

pub unsafe fn force_set_before_push(enabled: bool) {
    let cpuid = cpu::id();
    CPU_STATE[cpuid]
        .int_enabled
        .store(enabled, Ordering::Relaxed);
}

/// Save current interrupt enable state and disable interrupts.
pub fn push_disabled() -> Guard {
    let current = is_enabled();
    disable();

    let cpuid = cpu::id();
    let state = &CPU_STATE[cpuid];
    state.push_disabled(current);
    Guard { cpuid }
}

/// Restore interrupt enable state saved by [`push_disabled()`].
pub unsafe fn pop_disabled() {
    drop(Guard { cpuid: cpu::id() });
}

/// Guard that restores interrupt enable state when dropped.
pub struct Guard {
    cpuid: usize,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let cpuid = cpu::id();
        assert_eq!(self.cpuid, cpuid);
        assert!(!is_enabled());
        let state = &CPU_STATE[cpuid];
        if let Some(int_enabled) = state.pop_disabled()
            && int_enabled
        {
            enable();
        }
    }
}

impl Guard {
    pub fn forget(self) {
        mem::forget(self);
    }
}

pub fn with_push_disabled<T, F>(f: F) -> T
where
    F: FnOnce() -> T,
{
    let _guard = push_disabled();
    f()
}

static CPU_STATE: [CpuState; NCPU] = [const { CpuState::new() }; NCPU];

struct CpuState {
    push_depth: AtomicUsize,
    int_enabled: AtomicBool,
}

impl CpuState {
    const fn new() -> Self {
        Self {
            push_depth: AtomicUsize::new(0),
            int_enabled: AtomicBool::new(false),
        }
    }

    fn push_disabled(&self, int_enabled: bool) {
        assert!(self.push_depth.load(Ordering::Relaxed) < NCPU);
        let depth = self.push_depth.fetch_add(1, Ordering::Acquire);
        if depth == 0 {
            self.int_enabled.store(int_enabled, Ordering::Relaxed);
        }
    }

    fn pop_disabled(&self) -> Option<bool> {
        assert!(self.push_depth.load(Ordering::Relaxed) > 0);
        let int_enabled = self.int_enabled.load(Ordering::Relaxed);
        if self.push_depth.fetch_sub(1, Ordering::Release) == 1 {
            return Some(int_enabled);
        }
        None
    }
}
