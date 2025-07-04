//! Simple logging that allows concurrent FS system calls.
//!
//! A log transaction contains the updates of multiple FS system
//! calls. The logging system only commits when there are
//! no FS system calls active. Thus there is never
//! any reasoning required about whether a commit might
//! write an uncommitted system call's data to disk.
//!
//! A system call should call [`begin_tx()`] to mark
//! its start and end. Usually [`begin_tx()`] just increments
//! the count of in-progress FS system calls and returns.
//! But if it thinks the log is close to running out, it
//! sleeps until the last outstanding transaction commits.
//!
//! The log is a physical re-do log containiing disk blocks.
//!
//! The on-disk log format:
//!
//! ```text
//! header block, containing block #s for block A, B, C, ...
//! block A
//! block B
//! block C
//! ...
//! ```

use core::{
    convert::Infallible,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

use arrayvec::ArrayVec;
use once_init::OnceInit;
use ov6_kernel_params::LOG_SIZE;

use super::{
    block_io::{BlockGuard, BlockRef},
    repr,
};
use crate::{
    fs::{
        BlockNo, DeviceNo, SuperBlock,
        block_io::{self},
    },
    param::MAX_OP_BLOCKS,
    sync::{SpinLock, SpinLockCondVar, WaitError},
};

struct LogHeader {
    sb: &'static SuperBlock,
    dev: DeviceNo,
    blocks: ArrayVec<BlockRef, LOG_SIZE>,
}

impl LogHeader {
    fn new(dev: DeviceNo, sb: &'static SuperBlock) -> Self {
        assert!(LOG_SIZE >= sb.max_log_len());
        Self {
            sb,
            dev,
            blocks: ArrayVec::new(),
        }
    }

    fn max_len(&self) -> usize {
        self.blocks.capacity()
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }

    fn push(&mut self, block: &BlockRef) {
        assert!(self.blocks.len() < self.sb.max_log_len());
        if self.blocks.iter().all(|b| b.index() != block.index()) {
            self.blocks.push(block.clone());
        }
    }

    fn recover_from_log(&mut self) {
        self.read();
        self.install_transaction();
        self.write_log_head();
    }

    fn commit(&mut self) {
        if !self.blocks.is_empty() {
            self.write_log_body(); // Write modified blocks from cache to log
            self.write_log_head(); // Write header to disk -- the real commit
            self.install_transaction(); // Now install writes to home locations
            assert!(self.blocks.is_empty());
            self.write_log_head(); // Erase the transaction from the log
        }
    }

    /// Reads the log header from disk into the in-memory log header.
    fn read(&mut self) {
        assert!(self.blocks.is_empty());
        let mut bh = block_io::get(self.dev, self.sb.log_header_block().as_index());
        let Ok(bg) = bh.lock().read();
        let header = bg.data::<repr::LogHeader>();
        for &bn in header.block_indices() {
            let br = block_io::get(self.dev, bn as usize);
            self.push(&br);
        }
    }

    /// Writes in-memory block cache to log body.
    fn write_log_body(&mut self) {
        for (i, br) in (0..).zip(&mut self.blocks) {
            let Ok(bg) = br.lock().read();
            let mut log_br = block_io::get(self.dev, self.sb.log_body_block(i).as_index());
            let mut log_bg = log_br.lock().set_data(bg.bytes());
            let Ok(()) = log_bg.write();
        }
    }

    /// Writes in-memory log header to disk.
    ///
    /// This is the true point at which the current transaction commits.
    fn write_log_head(&self) {
        let mut br = block_io::get(self.dev, self.sb.log_header_block().as_index());
        let mut bg = br.lock().zeroed();
        let dst = bg.data_mut::<repr::LogHeader>();
        dst.set_len(self.blocks.len());
        for (i, br) in self.blocks.iter().enumerate() {
            dst.block_indices_mut()[i] = br.index().try_into().unwrap();
        }
        let Ok(()) = bg.write(); // infallible
    }

    /// Copies committed blocks from log to their home location.
    fn install_transaction(&mut self) {
        for mut br in self.blocks.drain(..) {
            let Ok(mut bg) = br.lock().read();
            let Ok(()) = bg.write();
        }
        assert!(self.blocks.is_empty());
    }
}

struct Log {
    data: SpinLock<LogData>,
    cond: SpinLockCondVar,
}

struct LogData {
    outstanding: usize,
    header: Option<LogHeader>, // If None, data is committing.
}

static LOG: OnceInit<Log> = OnceInit::new();

impl Log {
    fn new(dev: DeviceNo, sb: &'static SuperBlock) -> Self {
        let mut header = LogHeader::new(dev, sb);
        header.recover_from_log();

        Self {
            data: SpinLock::new(LogData {
                outstanding: 0,
                header: Some(header),
            }),
            cond: SpinLockCondVar::new(),
        }
    }

    /// Starts FS transaction.
    ///
    /// Called at the start of each FS system call.
    fn begin_op(&self) -> Result<(), WaitError> {
        let mut data = self.data.lock();
        loop {
            let Some(header) = &data.header else {
                // header is under committing
                match self.cond.wait(data) {
                    Ok(guard) => {
                        data = guard;
                        continue;
                    }
                    Err((_guard, WaitError::WaitingProcessAlreadyKilled)) => {
                        return Err(WaitError::WaitingProcessAlreadyKilled);
                    }
                }
            };
            if header.len() + (data.outstanding + 1) * MAX_OP_BLOCKS > header.max_len() {
                // this op might exhaust log space; wait for commit.
                match self.cond.wait(data) {
                    Ok(guard) => {
                        data = guard;
                        continue;
                    }
                    Err((_guard, WaitError::WaitingProcessAlreadyKilled)) => {
                        return Err(WaitError::WaitingProcessAlreadyKilled);
                    }
                }
            }
            data.outstanding += 1;
            break;
        }

        Ok(())
    }

    /// Starts FS transaction.
    ///
    /// Called at the start of each FS system call.
    fn force_begin_op(&self) {
        let mut data = self.data.lock();
        loop {
            let Some(header) = &data.header else {
                // header is under committing
                data = self.cond.force_wait(data);
                continue;
            };
            if header.len() + (data.outstanding + 1) * MAX_OP_BLOCKS > header.max_len() {
                // this op might exhaust log space; wait for commit.
                data = self.cond.force_wait(data);
                continue;
            }
            data.outstanding += 1;
            break;
        }
    }

    /// Ends FS transaction.
    ///
    /// Called at the end of each FS system call.
    /// Commits if this was the last outstanding operation.
    fn end_op(&self) {
        let mut header = None;

        let mut data = self.data.lock();
        data.outstanding -= 1;
        assert!(data.header.is_some()); // not under committing
        if data.outstanding == 0 {
            header = data.header.take();
        } else {
            // begin_op() may be waiting for log space,
            // and decrementing log.outstanding has decreased
            // the amount of reserved space.
            self.cond.notify();
        }
        drop(data); // unlock here

        if let Some(mut header) = header {
            // call commit w/o holding locks, since not allowed
            // to sleep with locks.
            header.commit();
            let mut data = self.data.lock();
            assert!(data.header.is_none());
            data.header = Some(header);
            self.cond.notify();
        }
    }

    #[expect(clippy::needless_pass_by_ref_mut)]
    fn write(&self, b: &mut BlockGuard<true>) {
        let data = &mut *self.data.lock();
        let header = data.header.as_mut().unwrap();
        assert!(data.outstanding > 0);

        header.push(&b.block());
    }
}

pub(super) fn init(dev: DeviceNo, sb: &'static SuperBlock) {
    LOG.init(Log::new(dev, sb));
}

/// Starts FS transaction.
///
/// Called at the start of each FS system call.
pub fn begin_tx() -> Result<Tx<'static, false>, WaitError> {
    Tx::<false>::begin()
}

/// Starts FS transaction.
///
/// Called at the start of each FS system call.
pub fn force_begin_tx() -> Tx<'static, false> {
    Tx::<false>::force_begin()
}

pub fn begin_readonly_tx() -> Tx<'static, true> {
    Tx::<true>::begin_read_only()
}

pub struct Tx<'log, const READ_ONLY: bool> {
    log: Option<&'log Log>,
}

impl<const READ_ONLY: bool> Drop for Tx<'_, READ_ONLY> {
    fn drop(&mut self) {
        if !READ_ONLY {
            self.log.unwrap().end_op();
        }
    }
}

impl Tx<'_, false> {
    fn begin() -> Result<Self, WaitError> {
        let log = LOG.get();
        log.begin_op()?;
        Ok(Self { log: Some(log) })
    }

    fn force_begin() -> Self {
        let log = LOG.get();
        log.force_begin_op();
        Self { log: Some(log) }
    }
}

impl Tx<'_, true> {
    fn begin_read_only() -> Self {
        Self { log: None }
    }
}

impl<const READ_ONLY: bool> Tx<'_, READ_ONLY> {
    pub fn end(self) {
        let _ = self;
    }

    pub(super) fn get_block(&self, dev: DeviceNo, bn: BlockNo) -> TxBlockRef<'_, READ_ONLY> {
        TxBlockRef {
            log: self.log,
            block: block_io::get(dev, bn.value() as usize),
        }
    }

    #[expect(clippy::unused_self)]
    pub(super) fn to_writable(&self) -> Option<NestedTx<'_, false>> {
        if READ_ONLY {
            None
        } else {
            Some(NestedTx {
                tx: ManuallyDrop::new(Tx {
                    log: Some(LOG.get()),
                }),
            })
        }
    }
}

pub(super) struct NestedTx<'a, const READ_ONLY: bool> {
    tx: ManuallyDrop<Tx<'a, READ_ONLY>>,
}

impl<'a, const READ_ONLY: bool> Deref for NestedTx<'a, READ_ONLY> {
    type Target = Tx<'a, READ_ONLY>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

pub struct TxBlockRef<'a, const READ_ONLY: bool> {
    log: Option<&'a Log>,
    block: BlockRef,
}

impl<const READ_ONLY: bool> TxBlockRef<'_, READ_ONLY> {
    pub(super) fn lock(&mut self) -> TxBlockGuard<'_, false, READ_ONLY> {
        TxBlockGuard {
            log: self.log,
            guard: Some(self.block.lock()),
        }
    }
}

pub(super) struct TxBlockGuard<'a, const VALID: bool, const READ_ONLY: bool> {
    log: Option<&'a Log>,
    guard: Option<BlockGuard<'a, VALID>>,
}

impl<const VALID: bool, const READ_ONLY: bool> Drop for TxBlockGuard<'_, VALID, READ_ONLY> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take()
            && guard.is_dirty()
            && let Ok(mut guard) = guard.try_validate()
            && let Some(log) = self.log
        {
            log.write(&mut guard);
        }
    }
}

// Implementt consuming methods (receiver is `self`) for TxBlockGuard
// This is because `Deref` and `DerefMut` cannot consume `self`.
impl<'a, const VALID: bool, const READ_ONLY: bool> TxBlockGuard<'a, VALID, READ_ONLY> {
    #[expect(clippy::unnecessary_wraps)]
    pub fn read(mut self) -> Result<TxBlockGuard<'a, true, READ_ONLY>, Infallible> {
        let Ok(guard) = self.guard.take().unwrap().read();
        Ok(TxBlockGuard {
            log: self.log,
            guard: Some(guard),
        })
    }
}

impl<'a, const VALID: bool> TxBlockGuard<'a, VALID, false> {
    pub fn zeroed(mut self) -> TxBlockGuard<'a, true, false> {
        let guard = self.guard.take().unwrap().zeroed();
        TxBlockGuard {
            log: self.log,
            guard: Some(guard),
        }
    }
}

impl<'a, const VALID: bool, const READ_ONLY: bool> Deref for TxBlockGuard<'a, VALID, READ_ONLY> {
    type Target = BlockGuard<'a, VALID>;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<const VALID: bool> DerefMut for TxBlockGuard<'_, VALID, false> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}
