#![cfg_attr(not(test), no_std)]

use core::{ops::Range, ptr::NonNull, sync::atomic::AtomicUsize};

pub struct SlabAllocator<T> {
    range: Range<*mut T>,
    free_list: Option<NonNull<Run>>,
}

unsafe impl<T> Send for SlabAllocator<T> where T: Send {}

struct Run {
    next: Option<NonNull<Run>>,
}

impl<T> SlabAllocator<T> {
    /// Creates a new `SlabAllocator` that manages the given range of pointers.
    ///
    /// The given range of pointers satisfies layout requirement of type `T`.
    ///
    /// # Safety
    ///
    /// The given range of pointers must be valid and not overlap with other
    /// memory regions.
    ///
    /// # Panics
    ///
    /// This function will panic if:
    ///
    /// - The start or end address of the given range is not aligned to `T`.
    /// - The size of the given range is not a multiple of the size of `T`.
    #[must_use]
    pub unsafe fn new(range: Range<*mut T>) -> Self {
        const {
            assert!(size_of::<Run>() <= size_of::<T>());
            assert!(align_of::<T>().is_multiple_of(align_of::<Run>()));
        }

        assert_eq!(range.start.addr() % align_of::<T>(), 0);
        assert_eq!(range.end.addr() % align_of::<T>(), 0);
        assert_eq!((range.end.addr() - range.start.addr()) % size_of::<T>(), 0);

        let mut free_list = None;
        let mut p = range.end;
        while p > range.start {
            unsafe {
                p = p.sub(1);
            }
            let mut run = NonNull::new(p).unwrap().cast::<Run>();
            unsafe {
                run.as_mut().next = free_list;
            }
            free_list = Some(run);
        }
        Self { range, free_list }
    }

    /// Allocates a memory.
    pub fn allocate(&mut self) -> Option<NonNull<T>> {
        let ptr = self.free_list.take()?;
        self.free_list = unsafe { ptr.as_ref().next };
        Some(ptr.cast())
    }

    /// Deallocates a memory.
    ///
    /// # Safety
    ///
    /// The given address must have been previously allocated by this
    /// `SlabAllocator<T>`. The memory must not be accessed after it has been
    /// freed. The memory must not be deallocated more than once.
    ///
    /// # Panics
    ///
    /// This function will panic if:
    ///
    /// - The given address is not within the range managed by this
    ///   `SlabAllocator<T>`.
    /// - The given address is not aligned to `T`.
    pub unsafe fn deallocate(&mut self, ptr: NonNull<T>) {
        assert!(self.range.contains(&ptr.as_ptr()));
        assert_eq!(ptr.addr().get() % align_of::<T>(), 0);

        unsafe {
            let run = ptr.cast::<Run>();
            run.write(Run {
                next: self.free_list,
            });
            self.free_list = Some(run);
        }
    }
}

/// A layout of `ArcInner<T>`.
///
/// This is a helper type to use [`SlabAllocator`] as custom `Allocator` for
/// `Arc<T>`.
pub struct ArcInnerLayout<T> {
    _strong: AtomicUsize,
    _weak: AtomicUsize,
    _data: T,
}

#[cfg(test)]
mod tests {
    use core::cell::UnsafeCell;
    use std::collections::HashSet;

    use super::*;

    struct Data {
        _data: [u64; 4],
    }

    impl Data {
        const fn zeroed() -> Self {
            Self { _data: [0; 4] }
        }
    }

    struct Heap(UnsafeCell<[Data; 100]>);
    unsafe impl Sync for Heap {}

    #[test]
    fn test_page_allocator() {
        let heap = Heap(UnsafeCell::new([const { Data::zeroed() }; 100]));
        let heap_range = unsafe { (*heap.0.get()).as_mut_ptr_range() };

        let mut allocator = unsafe { SlabAllocator::new(heap_range) };

        let ptr0 = allocator.allocate().unwrap();
        assert_eq!(ptr0.addr().get() % align_of::<Data>(), 0);
        let ptr1 = allocator.allocate().unwrap();
        assert_eq!(ptr1.addr().get() % align_of::<Data>(), 0);
        assert_ne!(ptr0, ptr1);
        unsafe {
            allocator.deallocate(ptr0);
            allocator.deallocate(ptr1);
        }
    }

    #[test]
    fn test_all_pages() {
        let heap = Heap(UnsafeCell::new([const { Data::zeroed() }; 100]));
        let heap_range = unsafe { (*heap.0.get()).as_mut_ptr_range() };

        let mut allocator = unsafe { SlabAllocator::new(heap_range) };

        let mut pages = vec![];
        let mut addrs = HashSet::new();

        // allocate all pages
        for _ in 0..100 {
            let page = allocator.allocate().unwrap();
            assert_eq!(
                page.addr().get() % align_of::<Data>(),
                0,
                "page is not aligned"
            );
            assert!(addrs.insert(page.addr()), "page is duplicated");
            pages.push(page);
        }

        // fail to allocate one more page
        assert!(allocator.allocate().is_none());

        // free one page and allocate one page
        let page = pages.pop().unwrap();
        unsafe {
            allocator.deallocate(page);
        }
        let page = allocator.allocate().unwrap();
        assert_eq!(page.addr().get() % align_of::<Data>(), 0);
        pages.push(page);

        // free all pages
        for page in pages {
            unsafe {
                allocator.deallocate(page);
            }
        }
    }
}
