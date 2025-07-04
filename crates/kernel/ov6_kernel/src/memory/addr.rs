use core::{
    fmt,
    ops::{Bound, Range, RangeBounds, RangeInclusive},
    ptr::{self, NonNull},
};

use dataview::Pod;
use ov6_syscall::{UserMutRef, UserMutSlice, UserRef, UserSlice};

use super::{PAGE_SHIFT, PAGE_SIZE, vm_user::UserPageTable};
use crate::error::KernelError;

const fn page_roundup(addr: usize) -> usize {
    (addr + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

const fn page_rounddown(addr: usize) -> usize {
    addr & !(PAGE_SIZE - 1)
}

const fn is_page_aligned(addr: usize) -> bool {
    addr.is_multiple_of(PAGE_SIZE)
}

const fn level_page_roundup(addr: usize, level: usize) -> usize {
    let page_size = super::level_page_size(level);
    (addr + page_size - 1) & !(page_size - 1)
}

const fn level_page_rounddown(addr: usize, level: usize) -> usize {
    let page_size = super::level_page_size(level);
    addr & !(page_size - 1)
}

const fn is_level_page_aligned(addr: usize, level: usize) -> bool {
    let page_size = super::level_page_size(level);
    addr.is_multiple_of(page_size)
}

pub trait PageRound {
    fn as_addr(&self) -> usize;
    fn from_addr(addr: usize) -> Self;

    fn page_roundup(&self) -> Self
    where
        Self: Sized,
    {
        Self::from_addr(page_roundup(self.as_addr()))
    }

    fn page_rounddown(&self) -> Self
    where
        Self: Sized,
    {
        Self::from_addr(page_rounddown(self.as_addr()))
    }

    fn is_page_aligned(&self) -> bool {
        is_page_aligned(self.as_addr())
    }

    fn level_page_roundup(&self, level: usize) -> Self
    where
        Self: Sized,
    {
        Self::from_addr(level_page_roundup(self.as_addr(), level))
    }

    fn level_page_rounddown(&self, level: usize) -> Self
    where
        Self: Sized,
    {
        Self::from_addr(level_page_rounddown(self.as_addr(), level))
    }

    fn is_level_page_aligned(&self, level: usize) -> bool {
        is_level_page_aligned(self.as_addr(), level)
    }
}

impl PageRound for usize {
    fn as_addr(&self) -> usize {
        *self
    }

    fn from_addr(addr: usize) -> Self {
        addr
    }
}

impl PageRound for VirtAddr {
    fn as_addr(&self) -> usize {
        self.0
    }

    fn from_addr(addr: usize) -> Self {
        Self::new(addr).unwrap()
    }
}

impl PageRound for PhysAddr {
    fn as_addr(&self) -> usize {
        self.0
    }

    fn from_addr(addr: usize) -> Self {
        Self::new(addr)
    }
}

struct Hex(usize);
impl fmt::Debug for Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

macro_rules! impl_fmt {
    ($ty:ident) => {
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.debug_tuple(stringify!($ty)).field(&Hex(self.0)).finish()
            }
        }
        impl fmt::LowerHex for $ty {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }
        impl fmt::UpperHex for $ty {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                fmt::UpperHex::fmt(&self.0, f)
            }
        }
        impl fmt::Pointer for $ty {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                let ptr = ptr::without_provenance::<u8>(self.0);
                fmt::Pointer::fmt(&ptr, f)
            }
        }
    };
}

/// Virtual address
///
/// The RISC-V Sv39 schema has three levels of page-table
/// pages. A page-table page contains 512 64-bit PTEs.
/// A 64-bit virtual address is split into five fields:
/// ```text
///     39..=63 -- must be zero.
///     30..=38 -- 9 bits of level-2 index.
///     21..=29 -- 9 bits of level-1 index.
///     12..=20 -- 9 bits of level-0 index.
///      0..=11 -- 12 bits byte offset with the page.
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtAddr(usize);
impl_fmt!(VirtAddr);

/// Physical Page Number of a page
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct PhysPageNum(usize);
impl_fmt!(PhysPageNum);

/// Physical Address
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(usize);
impl_fmt!(PhysAddr);

impl VirtAddr {
    /// One beyond the highest possible virtual address.
    ///
    /// [`VirtAddr::MAX`] is actually one bit less than the max allowed by
    /// Sv39, to avoid having to sign-extend virtual addresses
    /// that have the high bit set.
    pub const MAX: Self = Self(1 << (9 * 3 + PAGE_SHIFT - 1));
    pub const MIN_AVA: Self = Self(4096);
    pub const ZERO: Self = Self(0);

    pub const fn new(addr: usize) -> Result<Self, KernelError> {
        if addr > Self::MAX.0 {
            return Err(KernelError::TooLargeVirtualAddress(addr));
        }
        Ok(Self(addr))
    }

    pub const fn with_level_idx(self, level: usize, idx: usize) -> Self {
        assert!(level <= 2);
        assert!(idx < 512);
        let shift = (9 * level) + PAGE_SHIFT;
        let mask = 0x1ff << shift;
        Self(self.0 & !mask | (idx << shift))
    }

    pub const fn level_idx(self, level: usize) -> usize {
        assert!(level <= 2);
        let shift = 9 * level + PAGE_SHIFT;
        (self.addr() >> shift) & 0x1ff
    }

    pub const fn addr(self) -> usize {
        self.0
    }

    pub const fn byte_add(self, offset: usize) -> Result<Self, KernelError> {
        let Some(addr) = self.0.checked_add(offset) else {
            return Err(KernelError::TooLargeVirtualAddress(usize::MAX));
        };
        Self::new(addr)
    }

    pub const fn byte_sub(self, offset: usize) -> Result<Self, KernelError> {
        let Some(addr) = self.0.checked_sub(offset) else {
            return Err(KernelError::VirtualAddressUnderflow);
        };
        Self::new(addr)
    }

    pub(crate) const fn checked_sub(self, other: Self) -> Option<usize> {
        self.0.checked_sub(other.0)
    }

    pub fn range_inclusive<R>(range: R) -> Option<RangeInclusive<Self>>
    where
        R: RangeBounds<Self>,
    {
        let min_va = match range.start_bound() {
            Bound::Included(va) => *va,
            Bound::Excluded(va) => va.byte_add(1).ok()?,
            Bound::Unbounded => Self::ZERO,
        };
        let max_va = match range.end_bound() {
            Bound::Included(va) => *va,
            Bound::Excluded(va) => va.byte_sub(1).ok()?,
            Bound::Unbounded => Self::MAX.byte_sub(1).ok()?,
        };

        if min_va > max_va {
            return None;
        }

        Some(min_va..=max_va)
    }
}

impl PhysPageNum {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn phys_addr(self) -> PhysAddr {
        PhysAddr(self.0 << PAGE_SHIFT)
    }

    pub const fn value(self) -> usize {
        self.0
    }
}

impl PhysAddr {
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    pub fn addr(self) -> usize {
        self.0
    }

    pub fn as_ptr<T>(self) -> *const T {
        ptr::with_exposed_provenance(self.0)
    }

    pub fn as_mut_ptr<T>(self) -> *mut T {
        ptr::with_exposed_provenance_mut(self.0)
    }

    pub fn as_non_null<T>(self) -> NonNull<T> {
        NonNull::new(ptr::with_exposed_provenance_mut(self.0)).unwrap()
    }

    pub(super) fn phys_page_num(self) -> PhysPageNum {
        PhysPageNum(self.0 >> PAGE_SHIFT)
    }

    pub const fn byte_add(self, n: usize) -> Option<Self> {
        let Some(n) = self.0.checked_add(n) else {
            return None;
        };
        Some(Self(n))
    }
}

impl<T> From<&T> for PhysAddr
where
    T: ?Sized,
{
    fn from(r: &T) -> Self {
        Self(ptr::from_ref(r).addr())
    }
}

impl<T> From<&mut T> for PhysAddr
where
    T: ?Sized,
{
    fn from(r: &mut T) -> Self {
        Self(ptr::from_mut(r).addr())
    }
}

impl<T> From<*const T> for PhysAddr
where
    T: ?Sized,
{
    fn from(ptr: *const T) -> Self {
        Self(ptr.addr())
    }
}

impl<T> From<*mut T> for PhysAddr
where
    T: ?Sized,
{
    fn from(ptr: *mut T) -> Self {
        Self(ptr.addr())
    }
}

impl<T> From<NonNull<T>> for PhysAddr
where
    T: ?Sized,
{
    fn from(ptr: NonNull<T>) -> Self {
        Self(ptr.addr().get())
    }
}

pub trait TryAsVirtAddrRange {
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError>;
}

impl TryAsVirtAddrRange for Range<VirtAddr> {
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        Ok(self.clone())
    }
}

impl<T> TryAsVirtAddrRange for UserRef<T> {
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        let start = VirtAddr::new(self.addr())?;
        let end = start.byte_add(self.size())?;
        Ok(start..end)
    }
}

impl<T> TryAsVirtAddrRange for UserMutRef<T> {
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        let start = VirtAddr::new(self.addr())?;
        let end = start.byte_add(self.size())?;
        Ok(start..end)
    }
}

impl<T> TryAsVirtAddrRange for UserSlice<T> {
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        let size = self
            .size()
            .ok_or(KernelError::TooLargeVirtualAddress(usize::MAX))?;
        let start = VirtAddr::new(self.addr())?;
        let end = start.byte_add(size)?;
        Ok(start..end)
    }
}

impl<T> TryAsVirtAddrRange for UserMutSlice<T> {
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        let size = self
            .size()
            .ok_or(KernelError::TooLargeVirtualAddress(usize::MAX))?;
        let start = VirtAddr::new(self.addr())?;
        let end = start.byte_add(size)?;
        Ok(start..end)
    }
}

impl<T> TryAsVirtAddrRange for &'_ T
where
    T: TryAsVirtAddrRange,
{
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        <T as TryAsVirtAddrRange>::try_as_va_range(*self)
    }
}

impl<T> TryAsVirtAddrRange for &'_ mut T
where
    T: TryAsVirtAddrRange,
{
    fn try_as_va_range(&self) -> Result<Range<VirtAddr>, KernelError> {
        <T as TryAsVirtAddrRange>::try_as_va_range(*self)
    }
}

pub trait AsVirtAddrRange {
    fn as_va_range(&self) -> Range<VirtAddr>;
}

impl<R> AsVirtAddrRange for Validated<R>
where
    R: TryAsVirtAddrRange,
{
    fn as_va_range(&self) -> Range<VirtAddr> {
        self.0.try_as_va_range().unwrap()
    }
}

impl<R> AsVirtAddrRange for &'_ Validated<R>
where
    R: TryAsVirtAddrRange,
{
    fn as_va_range(&self) -> Range<VirtAddr> {
        self.0.try_as_va_range().unwrap()
    }
}

impl<R> AsVirtAddrRange for &'_ mut Validated<R>
where
    R: TryAsVirtAddrRange,
{
    fn as_va_range(&self) -> Range<VirtAddr> {
        self.0.try_as_va_range().unwrap()
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Validated<T>(T);

unsafe impl<T> Pod for Validated<T> where T: Pod {}

pub trait Validate: Sized {
    fn validate(self, pt: &UserPageTable) -> Result<Validated<Self>, KernelError>;
}

impl<T> Validate for UserRef<T> {
    fn validate(self, pt: &UserPageTable) -> Result<Validated<Self>, KernelError> {
        pt.validate_user_read(self.try_as_va_range()?)?;
        Ok(Validated(self))
    }
}

impl<T> Validate for UserMutRef<T> {
    fn validate(self, pt: &UserPageTable) -> Result<Validated<Self>, KernelError> {
        pt.validate_user_write(self.try_as_va_range()?)?;
        Ok(Validated(self))
    }
}

impl<T> Validate for UserSlice<T> {
    fn validate(self, pt: &UserPageTable) -> Result<Validated<Self>, KernelError> {
        pt.validate_user_read(self.try_as_va_range()?)?;
        Ok(Validated(self))
    }
}

impl<T> Validate for UserMutSlice<T> {
    fn validate(self, pt: &UserPageTable) -> Result<Validated<Self>, KernelError> {
        pt.validate_user_write(self.try_as_va_range()?)?;
        Ok(Validated(self))
    }
}

impl<T> Validated<UserRef<T>> {
    #[expect(unused)]
    pub fn addr(&self) -> usize {
        self.0.addr()
    }

    #[expect(unused)]
    pub fn size(&self) -> usize {
        self.0.size()
    }

    pub fn as_bytes(&self) -> Validated<UserSlice<u8>>
    where
        T: Pod + Sized,
    {
        Validated(self.0.as_bytes())
    }
}

impl<T> Validated<UserMutRef<T>> {
    #[expect(unused)]
    pub fn addr(&self) -> usize {
        self.0.addr()
    }

    #[expect(unused)]
    pub fn size(&self) -> usize {
        self.0.size()
    }

    pub fn as_bytes_mut(&mut self) -> Validated<UserMutSlice<u8>>
    where
        T: Pod + Sized,
    {
        Validated(self.0.as_bytes_mut())
    }
}

impl<T> Validated<UserSlice<T>> {
    pub fn addr(&self) -> usize {
        self.0.addr()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[expect(unused)]
    pub fn size(&self) -> usize {
        self.0.size().unwrap()
    }

    #[expect(unused)]
    #[track_caller]
    pub fn cast<U>(&self) -> Validated<UserSlice<U>> {
        Validated(self.0.cast())
    }

    #[track_caller]
    pub fn nth(&self, n: usize) -> Validated<UserRef<T>> {
        Validated(self.0.nth(n))
    }

    #[track_caller]
    pub fn skip(&self, amt: usize) -> Self {
        Self(self.0.skip(amt))
    }

    #[track_caller]
    pub fn take(&self, amt: usize) -> Self {
        Self(self.0.take(amt))
    }
}

impl<T> Validated<UserMutSlice<T>> {
    pub fn addr(&self) -> usize {
        self.0.addr()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[expect(unused)]
    pub fn size(&self) -> usize {
        self.0.size().unwrap()
    }

    #[track_caller]
    pub fn cast_mut<U>(&mut self) -> Validated<UserMutSlice<U>> {
        Validated(self.0.cast_mut())
    }

    #[track_caller]
    pub fn nth_mut(&mut self, n: usize) -> Validated<UserMutRef<T>> {
        Validated(self.0.nth_mut(n))
    }

    #[track_caller]
    pub fn skip_mut(&mut self, amt: usize) -> Self {
        Self(self.0.skip_mut(amt))
    }

    #[track_caller]
    pub fn take_mut(&mut self, amt: usize) -> Self {
        Self(self.0.take_mut(amt))
    }
}

#[derive(Clone, Copy, derive_more::From)]
pub enum GenericSlice<'a, T> {
    User(&'a UserPageTable, Validated<UserSlice<T>>),
    Kernel(&'a [T]),
}

impl<T> GenericSlice<'_, T> {
    pub fn len(&self) -> usize {
        match self {
            Self::User(_pt, s) => s.len(),
            Self::Kernel(s) => s.len(),
        }
    }

    pub fn skip(&self, amt: usize) -> GenericSlice<'_, T> {
        assert!(amt <= self.len());
        match self {
            Self::User(pt, s) => GenericSlice::User(pt, s.skip(amt)),
            Self::Kernel(s) => GenericSlice::Kernel(&s[amt..]),
        }
    }

    pub fn take(&self, amt: usize) -> GenericSlice<'_, T> {
        assert!(amt <= self.len());
        match self {
            Self::User(pt, s) => GenericSlice::User(pt, s.take(amt)),
            Self::Kernel(s) => GenericSlice::Kernel(&s[..amt]),
        }
    }
}

impl<'a, T> From<(&'a UserPageTable, &'a Validated<UserSlice<T>>)> for GenericSlice<'a, T> {
    fn from((pt, s): (&'a UserPageTable, &'a Validated<UserSlice<T>>)) -> Self {
        Self::User(
            pt,
            Validated(unsafe { UserSlice::from_raw_parts(s.addr(), s.len()) }),
        )
    }
}

#[derive(derive_more::From)]
pub enum GenericMutSlice<'a, T> {
    User(&'a mut UserPageTable, Validated<UserMutSlice<T>>),
    Kernel(&'a mut [T]),
}

impl<T> GenericMutSlice<'_, T> {
    pub fn len(&self) -> usize {
        match self {
            Self::User(_, s) => s.len(),
            Self::Kernel(s) => s.len(),
        }
    }

    pub fn skip_mut(&mut self, amt: usize) -> GenericMutSlice<'_, T> {
        assert!(amt <= self.len());
        match self {
            Self::User(pt, s) => GenericMutSlice::User(pt, s.skip_mut(amt)),
            Self::Kernel(s) => GenericMutSlice::Kernel(&mut s[amt..]),
        }
    }

    pub fn take_mut(&mut self, amt: usize) -> GenericMutSlice<'_, T> {
        assert!(amt <= self.len());
        match self {
            Self::User(pt, s) => GenericMutSlice::User(pt, s.take_mut(amt)),
            Self::Kernel(s) => GenericMutSlice::Kernel(&mut s[..amt]),
        }
    }
}

impl<'a, T> From<(&'a mut UserPageTable, &'a mut Validated<UserMutSlice<T>>)>
    for GenericMutSlice<'a, T>
{
    fn from((pt, s): (&'a mut UserPageTable, &'a mut Validated<UserMutSlice<T>>)) -> Self {
        Self::User(
            pt,
            Validated(unsafe { UserMutSlice::from_raw_parts(s.addr(), s.len()) }),
        )
    }
}

#[derive(Clone, Copy, derive_more::From)]
pub enum GenericSliceOfSlice<'a, T> {
    User(
        &'a UserPageTable,
        Validated<UserSlice<Validated<UserSlice<T>>>>,
    ),
    Kernel(&'a [&'a [T]]),
}

impl<T> GenericSliceOfSlice<'_, T> {
    pub fn len(&self) -> usize {
        match self {
            Self::User(_pt, s) => s.len(),
            Self::Kernel(s) => s.len(),
        }
    }

    pub fn nth(&self, n: usize) -> GenericSlice<'_, T>
    where
        T: Pod,
    {
        assert!(n < self.len());
        match self {
            Self::User(pt, s) => {
                let elem = pt.copy_u2k(&s.nth(n));
                GenericSlice::User(pt, elem)
            }
            Self::Kernel(s) => GenericSlice::Kernel(s[n]),
        }
    }
}

pub trait AsGenericSliceOfSlice<T> {
    fn len(&self) -> usize;
    fn as_generic_slice_of_slice<'a>(&'a self, pt: &'a UserPageTable)
    -> GenericSliceOfSlice<'a, T>;
}

impl<T> AsGenericSliceOfSlice<T> for Validated<UserSlice<Validated<UserSlice<T>>>> {
    fn len(&self) -> usize {
        self.len()
    }

    fn as_generic_slice_of_slice<'a>(
        &'a self,
        pt: &'a UserPageTable,
    ) -> GenericSliceOfSlice<'a, T> {
        GenericSliceOfSlice::User(
            pt,
            Self(unsafe { UserSlice::from_raw_parts(self.addr(), self.len()) }),
        )
    }
}

impl<'a, T> AsGenericSliceOfSlice<T> for &'a [&'a [T]] {
    fn len(&self) -> usize {
        (*self).len()
    }

    fn as_generic_slice_of_slice<'b>(
        &'b self,
        _pt: &'b UserPageTable,
    ) -> GenericSliceOfSlice<'b, T> {
        GenericSliceOfSlice::Kernel(self)
    }
}
