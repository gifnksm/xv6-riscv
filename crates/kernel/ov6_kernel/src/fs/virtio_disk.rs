use alloc::boxed::Box;
use core::{array, mem, pin::Pin, ptr, sync::atomic::Ordering};

use once_init::OnceInit;
use safe_cast::{SafeInto as _, to_u16, to_u32};
use vcell::VolatileCell;

use crate::{
    fs::{
        repr::FS_BLOCK_SIZE,
        virtio::{
            BLK_SECTOR_SIZE, ConfigStatus, DeviceFeatures, MmioRegister, VirtioBlkReq,
            VirtioBlkReqType, VirtqAvail, VirtqDesc, VirtqDescFlags, VirtqUsed,
        },
    },
    memory::{layout::VIRTIO0, page::PageFrameAllocator},
    sync::{SpinLock, SpinLockCondVar},
};

// This many virtio descriptors.
// Must be a power of two.
pub const NUM: usize = 8;

struct Disk<const NUM: usize> {
    /// MMIO register base address.
    base_address: usize,

    /// A set (not a ring) of DMA descriptors, with which the
    /// driver tells the device where to read and write individual
    /// disk operations.
    ///
    /// There are NUM descriptors.
    /// Most commands consist of a "chain" (a linked list) of a couple of
    /// these descriptors.
    desc: Pin<Box<[VirtqDesc; NUM], PageFrameAllocator>>,

    /// A ring in which the driver writes descriptor numbers
    /// that the driver would like the device to process.
    ///
    /// It only includes the head descriptor of each chain.
    /// The ring has NUM elements.
    avail: Pin<Box<VirtqAvail<NUM>, PageFrameAllocator>>,

    /// A ring in which the device writes descriptor numbers that
    /// the device has finished processing (just the head of each chain).
    ///
    /// There are NUM used ring entries.
    used: Pin<Box<VirtqUsed<NUM>, PageFrameAllocator>>,

    /// Condition variable signaled when descriptors are freed.
    desc_freed: &'static SpinLockCondVar,
    /// An array of booleans indicating whether a descriptor is free.
    free: [bool; NUM],
    used_idx: u16,

    /// Track info about in-flight operations,
    /// for use when completion interrupt arrives.
    ///
    /// Indexed by first descriptor index of chain.
    info: [TrackInfo; NUM],

    /// Disk command headers.
    ///
    /// One-for-one with descriptors, for convenience.
    ops: [VirtioBlkReq; NUM],
}

unsafe impl<const N: usize> Send for Disk<N> {}

struct TrackInfo {
    status: VolatileCell<u8>,
    in_progress: bool,
    completed: &'static SpinLockCondVar,
}

static DISK: OnceInit<SpinLock<Disk<NUM>>> = OnceInit::new();

fn addr_low<T>(p: &T) -> u32 {
    let addr = ptr::from_ref(p).addr();
    (addr & 0xffff_ffff).try_into().unwrap()
}

fn addr_high<T>(p: &T) -> u32 {
    let addr = ptr::from_ref(p).addr();
    ((addr >> 32) & 0xffff_ffff).try_into().unwrap()
}

enum Request<'a> {
    DeviceToBuf(&'a mut [u8]),
    BufToDevice(&'a [u8]),
}

impl Request<'_> {
    fn len(&self) -> usize {
        match self {
            Request::DeviceToBuf(data) => data.len(),
            Request::BufToDevice(data) => data.len(),
        }
    }

    fn ty(&self) -> VirtioBlkReqType {
        match self {
            Request::DeviceToBuf(_) => VirtioBlkReqType::In,
            Request::BufToDevice(_) => VirtioBlkReqType::Out,
        }
    }

    fn flag(&self) -> VirtqDescFlags {
        match self {
            // VirtqDescFlags::WRITE is set when the device writes to the buffer.
            Request::DeviceToBuf(_) => VirtqDescFlags::WRITE,
            Request::BufToDevice(_) => VirtqDescFlags::empty(),
        }
    }

    fn as_ptr(&self) -> *const u8 {
        match self {
            Request::DeviceToBuf(data) => data.as_ptr(),
            Request::BufToDevice(data) => data.as_ptr(),
        }
    }
}

impl<const N: usize> Disk<N> {
    fn new(
        base_address: usize,
        desc_freed: &'static SpinLockCondVar,
        completed: &'static [SpinLockCondVar; N],
    ) -> Self {
        Self {
            base_address,
            desc: Box::into_pin(Box::new_in(unsafe { mem::zeroed() }, PageFrameAllocator)),
            avail: Box::into_pin(Box::new_in(unsafe { mem::zeroed() }, PageFrameAllocator)),
            used: Box::into_pin(Box::new_in(unsafe { mem::zeroed() }, PageFrameAllocator)),
            desc_freed,
            free: [true; N],
            used_idx: 0,
            info: array::from_fn(|i| TrackInfo {
                status: VolatileCell::new(0),
                in_progress: false,
                completed: &completed[i],
            }),
            ops: [const {
                VirtioBlkReq {
                    ty: VirtioBlkReqType::In,
                    reserved: 0,
                    sector: 0,
                }
            }; N],
        }
    }

    fn read_reg(&self, reg: MmioRegister) -> u32 {
        unsafe {
            ptr::with_exposed_provenance::<u32>(self.base_address + reg as usize).read_volatile()
        }
    }

    fn write_reg(&self, reg: MmioRegister, value: u32) {
        unsafe {
            ptr::with_exposed_provenance_mut::<u32>(self.base_address + reg as usize)
                .write_volatile(value);
        }
    }

    fn init(&self) {
        assert_eq!(self.read_reg(MmioRegister::MagicValue), 0x7472_6976);
        assert_eq!(self.read_reg(MmioRegister::Version), 2);
        assert_eq!(self.read_reg(MmioRegister::DeviceId), 2);
        assert_eq!(self.read_reg(MmioRegister::VendorId), 0x554d_4551);

        let mut status = ConfigStatus::empty();

        // reset device
        self.write_reg(MmioRegister::Status, status.bits());

        // set ACKNOWLEDGE status bit
        status |= ConfigStatus::ACKNOWLEDGE;
        self.write_reg(MmioRegister::Status, status.bits());

        // set DRIVER status bit
        status |= ConfigStatus::DRIVER;
        self.write_reg(MmioRegister::Status, status.bits());

        // negotiate features
        let mut features =
            DeviceFeatures::from_bits_retain(self.read_reg(MmioRegister::DeviceFeatures));
        features.remove(DeviceFeatures::BLK_RO);
        features.remove(DeviceFeatures::BLK_SCSI);
        features.remove(DeviceFeatures::BLK_CONFIG_WCE);
        features.remove(DeviceFeatures::BLK_MQ);
        features.remove(DeviceFeatures::ANY_LAYOUT);
        features.remove(DeviceFeatures::RING_EVENT_IDX);
        features.remove(DeviceFeatures::RING_INDIRECT_DESC);
        self.write_reg(MmioRegister::DriverFeatures, features.bits());

        // tell device that feature negotiation is complete.
        status |= ConfigStatus::FEATURES_OK;
        self.write_reg(MmioRegister::Status, status.bits());

        // re-read status to ensure FEATURES_OK is set.
        status = ConfigStatus::from_bits_retain(self.read_reg(MmioRegister::Status));
        assert!(status.contains(ConfigStatus::FEATURES_OK));

        // initialize queue 0.
        self.write_reg(MmioRegister::QueueSel, 0);

        // ensure queue 0 is not in use.
        assert_eq!(self.read_reg(MmioRegister::QueueReady), 0);

        // check maximum queue size.
        let max = self.read_reg(MmioRegister::QueueNumMax);
        assert!(max != 0);
        assert!(max as usize >= N);

        // set queue size.
        self.write_reg(MmioRegister::QueueNum, to_u32!(N));

        // write physical addresses.
        self.write_reg(MmioRegister::QueueDescLow, addr_low(&*self.desc));
        self.write_reg(MmioRegister::QueueDescHigh, addr_high(&*self.desc));
        self.write_reg(MmioRegister::DriverDescLow, addr_low(&*self.avail));
        self.write_reg(MmioRegister::DriverDescHigh, addr_high(&*self.avail));
        self.write_reg(MmioRegister::DeviceDescLow, addr_low(&*self.used));
        self.write_reg(MmioRegister::DeviceDescHigh, addr_high(&*self.used));

        // queue is ready.
        self.write_reg(MmioRegister::QueueReady, 1);

        // tell device we're completely ready.
        status |= ConfigStatus::DRIVER_OK;
        self.write_reg(MmioRegister::Status, status.bits());
    }

    /// Finds a free descriptor, marks it non-free, returns its index.
    fn alloc_desc(&mut self) -> Option<u16> {
        let idx = (0..)
            .zip(&self.free)
            .find_map(|(i, free)| free.then_some(i))?;
        self.free[usize::from(idx)] = false;
        Some(idx)
    }

    /// Marks a descriptor as free.
    fn free_desc(&mut self, i: u16) {
        assert!(i < to_u16!(NUM));
        assert!(!self.free[usize::from(i)]);
        self.desc[usize::from(i)] = VirtqDesc {
            addr: 0,
            len: 0,
            flags: VirtqDescFlags::empty(),
            next: 0,
        };
        self.free[usize::from(i)] = true;
        self.desc_freed.notify();
    }

    // Frees a chain of descriptors.
    fn free_chain(&mut self, mut i: u16) {
        loop {
            let desc = &self.desc[usize::from(i)];
            let flag = desc.flags;
            let next = desc.next;
            self.free_desc(i);
            if !flag.contains(VirtqDescFlags::NEXT) {
                break;
            }
            i = next;
        }
    }

    /// Allocates three descriptors (they need not be contiguous).
    ///
    /// Disk transfers always use three descriptors.
    fn alloc3_desc(&mut self) -> Option<[u16; 3]> {
        let mut idx = [0; 3];
        for i in 0..3 {
            if let Some(x) = self.alloc_desc() {
                idx[i] = x;
            } else {
                for j in &idx[0..i] {
                    self.free_desc(*j);
                }
                return None;
            }
        }
        Some(idx)
    }

    fn send_request(&mut self, offset: usize, req: &Request, desc_idx: [u16; 3]) {
        assert!(offset.is_multiple_of(BLK_SECTOR_SIZE));
        let sector = (offset / BLK_SECTOR_SIZE) as u64;
        assert_eq!(req.len(), FS_BLOCK_SIZE);

        let buf0 = &mut self.ops[usize::from(desc_idx[0])];
        *buf0 = VirtioBlkReq {
            ty: req.ty(),
            reserved: 0,
            sector,
        };
        let buf0_addr = ptr::from_mut(buf0).addr();

        self.desc[usize::from(desc_idx[0])] = VirtqDesc {
            addr: buf0_addr as u64,
            len: to_u32!(size_of::<VirtioBlkReq>()),
            flags: VirtqDescFlags::NEXT,
            next: desc_idx[1],
        };

        self.desc[usize::from(desc_idx[1])] = VirtqDesc {
            addr: req.as_ptr().addr() as u64,
            len: to_u32!(FS_BLOCK_SIZE),
            flags: req.flag() | VirtqDescFlags::NEXT,
            next: desc_idx[2],
        };

        let info = &mut self.info[usize::from(desc_idx[0])];
        info.status.set(0xff); // device writes 0 on success
        self.desc[usize::from(desc_idx[2])] = VirtqDesc {
            addr: info.status.as_ptr().addr().safe_into(),
            len: 1,
            flags: VirtqDescFlags::WRITE,
            next: 0,
        };

        // record struct buf for `handle_interrupt()`.
        info.in_progress = true;

        // tell the device the first index in our chain of descriptors.
        let avail_idx = self.avail.idx.load(Ordering::Relaxed) as usize;
        self.avail.ring[avail_idx % NUM] = desc_idx[0];

        // tell the device another avail ring entry is available.
        self.avail.idx.fetch_add(1, Ordering::AcqRel);

        self.write_reg(MmioRegister::QueueNotify, 0); // value is queue number
    }
}

pub(super) fn init() {
    static REQ_COMPLETED: [SpinLockCondVar; NUM] = [const { SpinLockCondVar::new() }; NUM];
    static DESC_FREED: SpinLockCondVar = SpinLockCondVar::new();

    let disk = Disk::<NUM>::new(VIRTIO0, &DESC_FREED, &REQ_COMPLETED);
    disk.init();
    DISK.init(SpinLock::new(disk));
}

fn read_or_write(offset: usize, req: &Request) {
    let mut disk = DISK.get().lock();

    // the spec's Section 5.2 says that legacy block operations use
    // three descriptors: one for type/reserved/sector, one for the
    // data, one for a 1-byte status result.

    // allocate three descriptors.
    let desc_idx = loop {
        if let Some(idx) = disk.alloc3_desc() {
            break idx;
        }
        disk = disk.desc_freed.force_wait(disk);
    };

    // send request and wait for `handle_interrupts()` to say request has finished.
    disk.send_request(offset, req, desc_idx);
    while disk.info[usize::from(desc_idx[0])].in_progress {
        disk = disk.info[usize::from(desc_idx[0])]
            .completed
            .force_wait(disk);
    }

    // deallocate descriptors.
    disk.free_chain(desc_idx[0]);
}

pub(super) fn read(offset: usize, data: &mut [u8]) {
    read_or_write(offset, &Request::DeviceToBuf(data));
}

pub(super) fn write(offset: usize, data: &[u8]) {
    read_or_write(offset, &Request::BufToDevice(data));
}

pub fn handle_interrupt() {
    let mut disk = DISK.get().lock();

    // the device won't raise another interrupt until we tell it
    // we've seen this interrupt, which the following line does.
    // this may race with the device writing new entries to
    // the "used" ring, in which case we may process the new
    // completion entries in this interrupt, and have nothing to do
    // in the next interrupt, which is harmless.
    disk.write_reg(
        MmioRegister::InterruptAck,
        disk.read_reg(MmioRegister::InterruptStatus) & 0x3,
    );

    // the device increments disk.used.idx when it
    // adds an entry to the used ring.

    while disk.used_idx != disk.used.idx.load(Ordering::Acquire) {
        let id = disk.used.ring[disk.used_idx as usize % NUM].id as usize;

        let info = &mut disk.info[id];

        assert_eq!(info.status.get(), 0);
        info.in_progress = false; // disk is done with buf
        info.completed.notify();

        disk.used_idx = disk.used_idx.wrapping_add(1);
    }
}
