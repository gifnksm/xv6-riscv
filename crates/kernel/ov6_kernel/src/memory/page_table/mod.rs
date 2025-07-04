//! This module provides the implementation of a page table for managing
//! virtual-to-physical address mappings in a RISC-V system. It includes
//! functionality for allocation, mapping, validation, and manipulation of page
//! table entries.

use alloc::boxed::Box;
use core::{alloc::AllocError, fmt, ops::RangeBounds};

use dataview::Pod;
use riscv::register::satp::{self, Satp};

pub use self::entry::PtEntryFlags;
use self::{
    entry::PtEntry,
    iter::{Entries, LeavesMut},
};
use super::{PhysAddr, VirtAddr, addr::PhysPageNum, page::PageFrameAllocator};
use crate::{
    error::KernelError,
    memory::{self, PAGE_SIZE, PageRound as _, level_page_size, page_manager::Page},
    println,
};

mod entry;
mod iter;

/// Represents a page table, which manages virtual-to-physical address mappings.
pub struct PageTable(Box<PageTableEntries, PageFrameAllocator>);

impl PageTable {
    /// Attempts to allocate a new page table.
    ///
    /// Returns a `Result` containing the newly allocated `PageTable` or a
    /// `KernelError` if allocation fails.
    pub fn try_allocate() -> Result<Self, KernelError> {
        Ok(Self(PageTableEntries::try_allocate()?))
    }

    /// Returns the SATP (Supervisor Address Translation and Protection)
    /// register value for this page table.
    ///
    /// This function sets the mode to Sv39 and configures the physical page
    /// number (PPN) for the page table.
    pub(super) fn satp(&self) -> Satp {
        let mut satp = Satp::from_bits(0);
        satp.set_mode(satp::Mode::Sv39);
        satp.set_ppn(self.0.phys_page_num().value());
        satp
    }

    /// Validates that the virtual address range `va_range` is mapped with the
    /// required `flags`.
    ///
    /// # Panics
    ///
    /// Panics if `va_range.start > va_range.end`.
    pub(super) fn validate<R>(&self, va_range: R, flags: PtEntryFlags) -> Result<(), KernelError>
    where
        R: RangeBounds<VirtAddr>,
    {
        self.entries(va_range).try_for_each(|(_level, va, pte)| {
            if !pte.is_valid() {
                return Err(KernelError::VirtualPageNotMapped(va));
            }
            if pte.is_leaf() {
                let mut pte_flags = pte.flags();
                if pte_flags.contains(PtEntryFlags::C) {
                    pte_flags.insert(PtEntryFlags::W);
                }
                if !pte_flags.contains(flags) {
                    return Err(KernelError::InaccessiblePage(va));
                }
            }
            Ok(())
        })
    }

    /// Maps virtual addresses starting at `va` to physical addresses starting
    /// at `pa`.
    ///
    /// # Safety
    ///
    /// - The `size` parameter must be page-aligned.
    /// - The caller must ensure that the virtual and physical addresses do not
    ///   overlap with existing mappings unless explicitly intended.
    pub(super) unsafe fn map_addrs(
        &mut self,
        va: VirtAddr,
        pa: MapTarget,
        size: usize,
        perm: PtEntryFlags,
    ) -> Result<(), KernelError> {
        unsafe { self.0.map_addrs(va, pa, size, perm) }
    }

    /// Clones pages from another page table within the specified virtual
    /// address range.
    ///
    /// Returns an error if the source pages are inaccessible or if the
    /// operation fails.
    pub(super) fn clone_pages_from<R>(
        &mut self,
        other: &mut Self,
        va_range: R,
        flags: PtEntryFlags,
    ) -> Result<(), KernelError>
    where
        R: RangeBounds<VirtAddr>,
    {
        for (level, va, src_pte) in other.leaves_mut(va_range) {
            if !src_pte.is_leaf() {
                continue;
            }
            if !src_pte.flags().contains(flags) {
                return Err(KernelError::InaccessiblePage(va));
            }

            let page_size = memory::level_page_size(level);
            src_pte.make_copy_on_write();
            let flags = src_pte.flags();

            let mut dst_va = va;
            let pa = src_pte.phys_addr();
            let page = Page::from_raw(pa);
            page.increment_ref();
            let mut dst_pa = MapTarget::FixedAddr {
                addr: page.into_raw(),
            };

            self.0
                .map_addrs_level(level, &mut dst_va, &mut dst_pa, page_size, flags)?;
        }

        Ok(())
    }

    /// Unmaps the pages of memory starting at virtual address `va` and covering
    /// `size` bytes.
    ///
    /// Returns an error if the operation fails.
    pub(super) fn unmap_addrs(&mut self, va: VirtAddr, size: usize) -> Result<(), KernelError> {
        let start = va;
        let end = va.byte_add(size)?;
        self.unmap_range(start..end);
        Ok(())
    }

    fn unmap_range<R>(&mut self, va_range: R)
    where
        R: RangeBounds<VirtAddr>,
    {
        for (level, _va, pte) in self.leaves_mut(va_range) {
            pte.free(level);
        }
    }

    fn entries<R>(&self, va_range: R) -> Entries<'_>
    where
        R: RangeBounds<VirtAddr>,
    {
        Entries::new(&self.0, va_range)
    }

    fn leaves_mut<R>(&mut self, va_range: R) -> LeavesMut<'_>
    where
        R: RangeBounds<VirtAddr>,
    {
        LeavesMut::new(&mut self.0, va_range)
    }

    /// Returns the leaf page table entry (PTE) corresponding to the virtual
    /// address `va`.
    ///
    /// Returns an error if the virtual address is not mapped.
    fn find_leaf_entry(&self, va: VirtAddr) -> Result<(usize, &PtEntry), KernelError> {
        let mut pt = &*self.0;
        for level in (0..=2).rev() {
            let index = va.level_idx(level);
            let pte = &pt.0[index];
            if !pte.is_valid() {
                return Err(KernelError::VirtualPageNotMapped(va));
            }
            if pte.is_leaf() {
                return Ok((level, pte));
            }
            assert!(pte.is_non_leaf());
            pt = pte.get_page_table().unwrap();
        }
        panic!("invalid page table");
    }

    /// Returns a mutable reference to the leaf page table entry (PTE)
    /// corresponding to the virtual address `va`.
    ///
    /// Returns an error if the virtual address is not mapped.
    fn find_leaf_entry_mut(&mut self, va: VirtAddr) -> Result<(usize, &mut PtEntry), KernelError> {
        let mut pt = &mut *self.0;
        for level in (0..=2).rev() {
            let index = va.level_idx(level);
            let pte = &mut pt.0[index];
            if !pte.is_valid() {
                return Err(KernelError::VirtualPageNotMapped(va));
            }
            if pte.is_leaf() {
                return Ok((level, pte));
            }
            assert!(pte.is_non_leaf());
            pt = pte.get_page_table_mut().unwrap();
        }
        panic!("invalid page table");
    }

    /// Fetches a chunk of memory corresponding to the virtual address `va`.
    ///
    /// Returns a slice of bytes if the operation is successful, or an error
    /// if the page is inaccessible.
    pub(super) fn fetch_chunk(
        &self,
        va: VirtAddr,
        flags: PtEntryFlags,
    ) -> Result<&[u8], KernelError> {
        let (level, pte) = self.find_leaf_entry(va)?;
        assert!(pte.is_valid() && pte.is_leaf());
        if !pte.flags().contains(flags) {
            return Err(KernelError::InaccessiblePage(va));
        }

        let page = pte.get_page_bytes(level).unwrap();
        let page_size = page.len();
        let offset = va.addr() % page_size;
        Ok(&page[offset..])
    }

    /// Fetches a mutable chunk of memory corresponding to the virtual address
    /// `va`.
    ///
    /// Returns a mutable slice of bytes if the operation is successful, or an
    /// error if the page is inaccessible.
    pub(super) fn fetch_chunk_mut(
        &mut self,
        va: VirtAddr,
        flags: PtEntryFlags,
    ) -> Result<&mut [u8], KernelError> {
        let (level, pte) = self.find_leaf_entry_mut(va)?;
        assert!(pte.is_valid() && pte.is_leaf());
        if pte.flags().contains(PtEntryFlags::C) {
            pte.request_user_write(level, va)?;
        }
        if !pte.flags().contains(flags) {
            return Err(KernelError::InaccessiblePage(va));
        }

        let page = pte.get_page_bytes_mut(level).unwrap();
        let page_size = page.len();
        let offset = va.addr() % page_size;
        Ok(&mut page[offset..])
    }

    pub(super) fn request_user_write(&mut self, va: VirtAddr) -> Result<(), KernelError> {
        let (level, pte) = self.find_leaf_entry_mut(va)?;
        pte.request_user_write(level, va)?;
        assert!(pte.flags().contains(PtEntryFlags::W));
        Ok(())
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        self.0.free(2);
    }
}

/// Represents a target for mapping physical addresses.
#[derive(Debug, Clone, Copy)]
pub enum MapTarget {
    AllocatedAddr { zeroed: bool, allocate_new: bool },
    FixedAddr { addr: PhysAddr },
}

impl MapTarget {
    /// Creates a new `MapTarget` for a zeroed and newly allocated address.
    pub fn allocate_new_zeroed() -> Self {
        Self::AllocatedAddr {
            zeroed: true,
            allocate_new: true,
        }
    }

    /// Creates a new `MapTarget` for an allocated address with the specified
    /// properties.
    pub fn allocated_addr(zeroed: bool, allocate_new: bool) -> Self {
        Self::AllocatedAddr {
            zeroed,
            allocate_new,
        }
    }

    /// Creates a new `MapTarget` for a fixed physical address.
    pub fn fixed_addr(addr: PhysAddr) -> Self {
        Self::FixedAddr { addr }
    }

    fn is_page_aligned(&self) -> bool {
        match self {
            Self::AllocatedAddr { .. } => true,
            Self::FixedAddr { addr } => addr.is_page_aligned(),
        }
    }

    fn is_level_page_aligned(&self, level: usize) -> bool {
        match self {
            Self::AllocatedAddr { .. } => true,
            Self::FixedAddr { addr } => addr.is_level_page_aligned(level),
        }
    }

    fn level_offset(&self, level: usize) -> usize {
        match self {
            Self::AllocatedAddr { .. } => 0,
            Self::FixedAddr { addr } => addr.addr() % level_page_size(level),
        }
    }

    fn byte_add(&self, bytes: usize) -> Option<Self> {
        let new = match self {
            Self::AllocatedAddr { .. } => *self,
            Self::FixedAddr { addr } => Self::FixedAddr {
                addr: addr.byte_add(bytes)?,
            },
        };
        Some(new)
    }

    fn allocate_new(&self) -> bool {
        match self {
            Self::AllocatedAddr {
                allocate_new: create_new,
                ..
            } => *create_new,
            Self::FixedAddr { .. } => true,
        }
    }

    fn get_or_allocate(&self, level: usize) -> Result<PhysAddr, KernelError> {
        match self {
            Self::AllocatedAddr { zeroed, .. } => {
                assert_eq!(level, 0, "super page is not supported yet");
                let page = if *zeroed {
                    Page::alloc_zeroed()?
                } else {
                    Page::alloc()?
                };
                Ok(page.into_raw())
            }
            Self::FixedAddr { addr } => Ok(*addr),
        }
    }
}

/// Represents the entries in a page table.
#[repr(transparent)]
#[derive(Pod)]
struct PageTableEntries([PtEntry; 512]);

impl PageTableEntries {
    /// Allocates a new empty page table.
    ///
    /// Returns a boxed `PageTableEntries` or an error if allocation fails.
    fn try_allocate() -> Result<Box<Self, PageFrameAllocator>, KernelError> {
        let pt = Box::try_new_zeroed_in(PageFrameAllocator)
            .map_err(|AllocError| KernelError::NoFreePage)?;
        Ok(unsafe { pt.assume_init() })
    }

    /// Returns the physical address containing this page table.
    fn phys_addr(&self) -> PhysAddr {
        PhysAddr::from(self)
    }

    /// Returns the physical page number of the physical page containing this
    /// page table.
    fn phys_page_num(&self) -> PhysPageNum {
        self.phys_addr().phys_page_num()
    }

    fn find_or_create_leaf(
        &mut self,
        level: usize,
        va: VirtAddr,
    ) -> Result<&mut PtEntry, KernelError> {
        assert!(level <= 2);
        let mut pt = self;
        for level in (level + 1..=2).rev() {
            let index = va.level_idx(level);
            let pte = &mut pt.0[index];
            if !pte.is_valid() {
                let new_pt = Self::try_allocate()?;
                pte.set_page_table(new_pt);
            }
            pt = pte.get_page_table_mut().unwrap();
        }

        let index = va.level_idx(level);
        let pte = &mut pt.0[index];
        Ok(pte)
    }

    /// Creates PTE for virtual page `vpn` that refer to
    /// physical page `ppn`.
    fn map_page(
        &mut self,
        level: usize,
        va: VirtAddr,
        pa: MapTarget,
        perm: PtEntryFlags,
    ) -> Result<(), KernelError> {
        assert!(perm.intersects(PtEntryFlags::RWX), "perm={perm:?}");
        let pte = self.find_or_create_leaf(level, va)?;

        if pte.is_valid() {
            assert!(
                !pa.allocate_new(),
                "remap on the already mapped address: va={va:?}"
            );
            let pte_perm = pte.flags() & PtEntryFlags::URWX;
            if perm != pte_perm {
                return Err(KernelError::VirtualAddressWithUnexpectedPerm(
                    va, perm, pte_perm,
                ));
            }
            return Ok(());
        }

        let pa = pa.get_or_allocate(level)?;
        unsafe {
            pte.set_phys_page_num(pa.phys_page_num(), perm | PtEntryFlags::V);
        }
        Ok(())
    }

    fn map_addrs_level(
        &mut self,
        level: usize,
        va: &mut VirtAddr,
        pa: &mut MapTarget,
        size: usize,
        perm: PtEntryFlags,
    ) -> Result<(), KernelError> {
        let page_size = memory::level_page_size(level);
        assert!(va.is_level_page_aligned(level));
        assert!(pa.is_level_page_aligned(level));
        assert!(size.is_multiple_of(page_size));

        assert!(perm.intersects(PtEntryFlags::RWX), "perm={perm:?}");

        let va_end = va.byte_add(size)?;
        while *va < va_end {
            self.map_page(level, *va, *pa, perm)?;
            *va = va.byte_add(page_size).unwrap();
            *pa = pa.byte_add(page_size).unwrap();
        }
        Ok(())
    }

    /// Creates PTEs for virtual addresses starting at `va` that refer to
    /// physical addresses starting at `pa`.
    ///
    /// # Safety
    ///
    /// - The `size` parameter must be page-aligned.
    /// - The caller must ensure that the virtual and physical addresses do not
    ///   overlap with existing mappings unless explicitly intended.
    unsafe fn map_addrs(
        &mut self,
        mut va: VirtAddr,
        mut pa: MapTarget,
        size: usize,
        perm: PtEntryFlags,
    ) -> Result<(), KernelError> {
        assert!(va.is_page_aligned());
        assert!(pa.is_page_aligned());
        assert!(size.is_multiple_of(PAGE_SIZE));
        assert!(perm.intersects(PtEntryFlags::RWX), "perm={perm:?}");

        if let MapTarget::AllocatedAddr { .. } = pa {
            // currently, supr page allocation is not supported
            return self.map_addrs_level(0, &mut va, &mut pa, size, perm);
        }

        if va.addr() % level_page_size(1) != pa.level_offset(1) {
            return self.map_addrs_level(0, &mut va, &mut pa, size, perm);
        }

        let va_end = va.byte_add(size)?;
        let lv1_start_va = va.level_page_roundup(1);
        let lv1_end_va = va_end.level_page_rounddown(1);
        if lv1_start_va >= lv1_end_va {
            return self.map_addrs_level(0, &mut va, &mut pa, size, perm);
        }

        if va < lv1_start_va {
            let size = lv1_start_va.addr() - va.addr();
            self.map_addrs_level(0, &mut va, &mut pa, size, perm)?;
        }
        if lv1_start_va < lv1_end_va {
            let size = lv1_end_va.addr() - va.addr();
            self.map_addrs_level(1, &mut va, &mut pa, size, perm)?;
        }
        if va < va_end {
            let size = va_end.addr() - va.addr();
            self.map_addrs_level(0, &mut va, &mut pa, size, perm)?;
        }
        assert_eq!(va, va_end);
        Ok(())
    }

    fn free(&mut self, level: usize) {
        for pte in &mut self.0 {
            pte.free(level);
        }
    }
}

/// Dumps the contents of the given page table for debugging purposes.
pub(crate) fn dump_pagetable(pt: &PageTable) {
    println!("page table {:p}", pt);

    let mut state = DumpState(None);
    for (level, va, pte) in pt.entries(VirtAddr::ZERO..VirtAddr::MAX) {
        if !pte.is_valid() {
            state.dump();
            continue;
        }

        if pte.is_non_leaf() {
            state.dump();
            print_non_leaf(level, va, pte);
            continue;
        }

        state.append_or_dump(level, va, pte);
    }
    state.dump();
}

struct DumpLeaves {
    level: usize,
    flags: PtEntryFlags,
    start_va: VirtAddr,
    start_pa: PhysAddr,
    end_va: VirtAddr,
    end_pa: PhysAddr,
}

struct DumpState(Option<DumpLeaves>);

impl DumpState {
    fn dump(&mut self) {
        if let Some(leaves) = self.0.take() {
            print_leaves(&leaves);
        }
    }

    fn append_or_dump(&mut self, level: usize, va: VirtAddr, pte: &PtEntry) {
        if let Some(leaves) = &mut self.0
            && leaves.level == level
            && leaves.flags == pte.flags()
            && leaves
                .end_va
                .byte_add(level_page_size(level))
                .is_ok_and(|end_va| end_va == va)
            && leaves
                .end_pa
                .byte_add(level_page_size(level))
                .is_some_and(|end_pa| end_pa == pte.phys_addr())
        {
            leaves.end_va = va;
            leaves.end_pa = pte.phys_addr();
            return;
        }
        self.dump();
        self.0 = Some(DumpLeaves {
            level,
            flags: pte.flags(),
            start_va: va,
            start_pa: pte.phys_addr(),
            end_va: va,
            end_pa: pte.phys_addr(),
        });
    }
}

fn print_non_leaf(level: usize, va: VirtAddr, pte: &PtEntry) {
    let i = va.level_idx(level);
    let pa = pte.phys_addr();
    println!(
        "{prefix} [{i:3}] {va:#p} @ {pa:#p}",
        prefix = format_args!("{:.<1$}", "", (2 - level) * 2),
    );
}

fn print_leaves(leaves: &DumpLeaves) {
    struct Flags(PtEntryFlags);
    impl fmt::Debug for Flags {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut all_flags = PtEntryFlags::all();
            all_flags.remove(PtEntryFlags::V);
            for (name, flag) in all_flags.iter_names() {
                if self.0.contains(flag) {
                    for ch in name.chars() {
                        write!(f, "{}", ch.to_ascii_lowercase())?;
                    }
                } else {
                    write!(f, "-")?;
                }
            }
            Ok(())
        }
    }

    let &DumpLeaves {
        level,
        flags,
        start_va,
        start_pa,
        end_va,
        end_pa,
    } = leaves;

    let start_i = start_va.level_idx(level);

    let end_i = end_va.level_idx(level) + 1;
    let end_va = end_va.byte_add(level_page_size(level)).unwrap();
    let end_pa = end_pa.byte_add(level_page_size(level)).unwrap();

    println!(
        "{prefix} [{index}] {va} => {pa} {flags:?}",
        prefix = format_args!("{:.<1$}", "", (2 - level) * 2),
        index = format_args!("{:3}..{:3}", start_i, end_i),
        va = format_args!("{start_va:#p}..{end_va:#p}"),
        pa = format_args!("{start_pa:#p}..{end_pa:#p}"),
        flags = Flags(flags),
    );
}
