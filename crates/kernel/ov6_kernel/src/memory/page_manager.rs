//! This module provides functionality for managing memory pages, including
//! allocation, deallocation, and tracking the state of each page.

use core::{
    alloc::{AllocError, Allocator, Layout},
    fmt,
    mem::{self, MaybeUninit},
    ops::Range,
    ptr::{self, NonNull},
    slice,
    sync::atomic::{AtomicU32, Ordering},
};

use once_init::OnceInit;
use ov6_syscall::MemoryInfo;

use super::page_allocator::PageAllocator;
use crate::{
    error::KernelError,
    memory::{PAGE_SIZE, PageRound as _, PhysAddr},
};

/// A global instance of the page manager, initialized once during system setup.
static PAGE_MANAGER: OnceInit<PageManager> = OnceInit::new();

/// Initializes the page manager with the given range of physical addresses.
///
/// # Safety
///
/// This function must be called only once during system initialization. The
/// caller must ensure that the provided range of physical addresses is valid
/// and not used by other parts of the system.
pub(super) unsafe fn init(pa_range: Range<PhysAddr>) {
    PAGE_MANAGER.init(unsafe { PageManager::new(pa_range) });
}

/// Returns a reference to the global page manager.
///
/// This function assumes that the page manager has already been initialized.
pub(super) fn get() -> &'static PageManager {
    PAGE_MANAGER.get()
}

/// Represents a single page of memory managed by the `PageManager`.
///
/// This structure provides RAII-style management of memory pages, ensuring
/// that pages are properly freed when they go out of scope.
pub(super) struct Page<'a> {
    pa: PhysAddr,
    state: &'static PageState,
    manager: &'a PageManager,
}

impl fmt::Debug for Page<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page")
            .field("pa", &self.pa)
            .field("state", &self.state)
            .field("manager", &self.manager.heap_range)
            .finish()
    }
}

impl Drop for Page<'_> {
    fn drop(&mut self) {
        let free = if self.state.try_unmark_non_shareable().is_ok() {
            true
        } else {
            let prev_cnt = self.state.decrement_ref();
            prev_cnt <= 1
        };

        if free {
            unsafe {
                self.manager.allocator.free_page(self.pa.as_non_null());
            }
        }
    }
}

impl Page<'static> {
    /// Creates a `Page` instance from a raw physical address.
    ///
    /// This function assumes that the physical address corresponds to a valid
    /// memory page managed by the `PageManager`.
    pub(super) fn from_raw(pa: PhysAddr) -> Self {
        PAGE_MANAGER.get().get_page(pa)
    }

    /// Allocates one 4096-byte page of physical memory.
    ///
    /// Returns a `Page` instance on success, or an error if no memory is
    /// available. The allocated page is filled with a specific pattern to help
    /// detect uninitialized usage.
    pub(super) fn alloc() -> Result<Self, KernelError> {
        let manager = PAGE_MANAGER.get();
        let ptr = manager.allocator.alloc_page()?;
        let page = manager.get_page(ptr.into());
        assert_eq!(page.state.increment_ref(), 0);
        Ok(page)
    }

    /// Allocates one 4096-byte zeroed page of physical memory.
    ///
    /// Returns a `Page` instance on success, or an error if no memory is
    /// available. The allocated page is initialized to zero.
    pub(super) fn alloc_zeroed() -> Result<Self, KernelError> {
        let manager = PAGE_MANAGER.get();
        let ptr = manager.allocator.alloc_zeroed_page()?;
        let page = manager.get_page(ptr.into());
        assert_eq!(page.state.increment_ref(), 0);
        Ok(page)
    }

    /// Allocates a non-shareable page of physical memory.
    ///
    /// Returns a `Page` instance on success, or an error if no memory is
    /// available. The allocated page is marked as non-shareable.
    fn alloc_non_shareable_page() -> Result<Self, KernelError> {
        let manager = PAGE_MANAGER.get();
        let ptr = manager.allocator.alloc_page()?;
        let page = manager.get_page(ptr.into());
        page.state.mark_non_shareable();
        Ok(page)
    }
}

impl Page<'_> {
    pub(super) fn ref_count(&self) -> u32 {
        self.state.ref_count.load(Ordering::Acquire)
    }

    /// Consumes the `Page` and returns the underlying physical address.
    ///
    /// The caller takes ownership of the memory page and is responsible for
    /// managing its lifecycle.
    pub(super) fn into_raw(self) -> PhysAddr {
        let pa = self.pa;
        mem::forget(self);
        pa
    }

    /// Increments the reference count atomically.
    ///
    /// Returns the previous value of the reference count before the increment.
    ///
    /// # Panics
    ///
    /// Panics if the reference count is at its maximum value (`u32::MAX`).
    pub(super) fn increment_ref(&self) -> u32 {
        self.state.increment_ref()
    }
}

/// Represents the state of a single page in memory.
#[derive(Debug)]
struct PageState {
    /// The reference count of the page.
    ///
    /// A value of `0` indicates the page is free.
    /// A special value (e.g., `u32::MAX`) can indicate that multiple references
    /// are not allowed.
    ref_count: AtomicU32,
}

impl PageState {
    /// Creates a new `PageState` with a reference count of `0`.
    const fn new() -> Self {
        Self {
            ref_count: AtomicU32::new(0),
        }
    }

    /// Increments the reference count atomically.
    ///
    /// Returns the previous value of the reference count before the increment.
    ///
    /// # Panics
    ///
    /// Panics if the reference count is at its maximum value (`u32::MAX`).
    pub(super) fn increment_ref(&self) -> u32 {
        self.ref_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                assert_ne!(
                    current,
                    u32::MAX,
                    "cannot increment ref_count of non-shareable page"
                );
                Some(current + 1)
            })
            .unwrap()
    }

    /// Decrements the reference count atomically.
    ///
    /// Returns the previous value of the reference count before the decrement.
    ///
    /// # Panics
    ///
    /// Panics if the reference count is `0` or `u32::MAX`.
    pub(super) fn decrement_ref(&self) -> u32 {
        self.ref_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                assert_ne!(
                    current,
                    u32::MAX,
                    "cannot decrement ref_count count of non-shareable page"
                );
                assert!(current > 0, "page already freed");
                Some(current - 1)
            })
            .unwrap()
    }

    /// Marks the page as non-shareable by setting the reference count to
    /// `u32::MAX`.
    ///
    /// # Panics
    ///
    /// Panics if the reference count is not `0`.
    fn mark_non_shareable(&self) {
        self.ref_count
            .compare_exchange(0, u32::MAX, Ordering::AcqRel, Ordering::Acquire)
            .unwrap();
    }

    /// Attempts to unmark the page as non-shareable by resetting the reference
    /// count to `0`.
    ///
    /// Returns `Ok(0)` if successful, or `Err(u32::MAX)` if the page is not
    /// currently marked as non-shareable.
    fn try_unmark_non_shareable(&self) -> Result<u32, u32> {
        self.ref_count
            .compare_exchange(u32::MAX, 0, Ordering::AcqRel, Ordering::Acquire)
    }
}

/// A structure to manage the state of all pages in the system.
///
/// The `PageManager` is responsible for allocating, deallocating, and tracking
/// the state of memory pages. It uses a thread-safe allocator to manage page
/// frames.
pub(super) struct PageManager {
    /// An array of `PageState` to track the state of each page.
    states: &'static [PageState],
    /// The range of physical addresses used for the heap.
    heap_range: Range<PhysAddr>,
    /// A thread-safe allocator for managing page frames.
    allocator: PageAllocator,
}

impl PageManager {
    /// Creates a new `PageManager` with the given states and heap range.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the provided heap range is valid and
    /// corresponds to the physical memory managed by the allocator.
    unsafe fn new(pa_range: Range<PhysAddr>) -> Self {
        assert!(pa_range.start <= pa_range.end);

        let possible_heap_start = pa_range.start.page_roundup();
        let heap_end = pa_range.end.page_rounddown();

        let max_pages = (heap_end.addr() - possible_heap_start.addr()) / PAGE_SIZE;
        assert!(
            max_pages > 0,
            "possible_heap_start={possible_heap_start:#x}, heap_end={heap_end:#x}"
        );

        let state_pages = max_pages.div_ceil(size_of::<PageState>());
        assert!(state_pages <= max_pages);
        let state_start = pa_range
            .start
            .addr()
            .next_multiple_of(align_of::<PageState>());

        let states = unsafe {
            slice::from_raw_parts_mut(
                ptr::with_exposed_provenance_mut::<MaybeUninit<PageState>>(state_start),
                max_pages,
            )
        };

        for state in states.iter_mut() {
            state.write(PageState::new());
        }

        let states = unsafe { states.assume_init_mut() };

        let state_end = states.as_ptr_range().end.addr();
        let heap_start = PhysAddr::new(state_end).page_roundup();

        assert!(state_start <= state_end);
        assert!(PhysAddr::new(state_end) <= heap_start);
        assert!(heap_start <= heap_end);
        assert!((heap_end.addr() - heap_start.addr()) / PAGE_SIZE <= max_pages);

        let allocator = unsafe { PageAllocator::new(heap_start..heap_end) };

        Self {
            states,
            heap_range: heap_start..heap_end,
            allocator,
        }
    }

    /// Returns the index of the page corresponding to the given physical
    /// address.
    ///
    /// # Panics
    ///
    /// Panics if the physical address is not within the heap range or is not
    /// page-aligned.
    fn page_index(&self, pa: PhysAddr) -> usize {
        assert!(self.heap_range.contains(&pa));
        assert!(pa.is_page_aligned());
        (pa.addr() - self.heap_range.start.addr()) / PAGE_SIZE
    }

    /// Returns a `Page` instance for the given physical address.
    ///
    /// # Panics
    ///
    /// Panics if the physical address is not within the heap range.
    fn get_page(&self, pa: PhysAddr) -> Page<'_> {
        let index = self.page_index(pa);
        let state = &self.states[index];
        Page {
            pa,
            state,
            manager: self,
        }
    }

    /// Checks if the given address is within the allocated address range.
    ///
    /// Returns `true` if the pointer is within the range, otherwise `false`.
    pub(super) fn is_heap_addr(&self, pa: PhysAddr) -> bool {
        self.allocator.is_heap_addr(pa.as_non_null())
    }

    /// Retrieves memory information, including the number of free and total
    /// pages.
    pub(super) fn info(&self) -> MemoryInfo {
        self.allocator.info()
    }
}

#[derive(Clone)]
pub struct PageFrameAllocator;

unsafe impl Allocator for PageFrameAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        assert!(layout.size() <= PAGE_SIZE);
        assert_eq!(PAGE_SIZE % layout.align(), 0);

        #[expect(clippy::map_err_ignore)]
        let page = Page::alloc_non_shareable_page().map_err(|_| AllocError)?;
        let pa = page.into_raw();
        Ok(NonNull::slice_from_raw_parts(pa.as_non_null(), PAGE_SIZE))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        assert!(layout.size() <= PAGE_SIZE);
        assert_eq!(PAGE_SIZE % layout.align(), 0);
        assert_eq!(ptr.addr().get() % PAGE_SIZE, 0);

        let page = Page::from_raw(ptr.into());
        drop(page);
    }
}
