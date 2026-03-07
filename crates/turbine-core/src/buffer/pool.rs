use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::buffer::leased::LeasedBuffer;
use crate::config::PoolConfig;
use crate::epoch::clock::EpochClock;
use crate::error::Result;
use crate::gc::{BufferPinHook, EpochObserver};
use crate::ring::registration::RingRegistration;
use crate::transfer::handle::{ReturnedBuffer, TransferHandle};

/// The main API for epoch-based io_uring buffer management.
///
/// Owns an [`EpochClock`] (ring of arenas), a [`RingRegistration`], and a
/// crossbeam channel for cross-thread buffer returns.
pub struct IouringBufferPool<H> {
    clock: EpochClock,
    registration: RingRegistration,
    sender: Sender<ReturnedBuffer>,
    receiver: Receiver<ReturnedBuffer>,
    hooks: H,
}

impl<H: BufferPinHook + EpochObserver> IouringBufferPool<H> {
    /// Create a new buffer pool with the given configuration and hooks.
    pub fn new(config: PoolConfig, hooks: H) -> Result<Self> {
        let clock = EpochClock::new(&config)?;
        let (sender, receiver) = unbounded();

        Ok(Self {
            clock,
            registration: RingRegistration::new(),
            sender,
            receiver,
            hooks,
        })
    }

    /// Lease `len` bytes from the current epoch's arena.
    ///
    /// Returns `None` if the arena is full. The returned [`LeasedBuffer`] is
    /// `!Send` and must not leave the owning thread.
    pub fn lease(&self, len: usize) -> Option<LeasedBuffer> {
        let arena = self.clock.current_arena();
        let (ptr, buf_id) = arena.alloc(len)?;
        arena.acquire_lease();

        self.hooks.on_pin(arena.epoch(), buf_id);

        // SAFETY: ptr points into the arena's mmap region which is valid
        // for the arena's lifetime. The arena outlives the lease because
        // lease_count > 0 prevents collection.
        let buf = unsafe { LeasedBuffer::new(ptr, len, arena.epoch(), buf_id, arena) };
        Some(buf)
    }

    /// Rotate to a new epoch: retire the current arena and activate the next.
    pub fn rotate(&self) {
        let (retired, active) = self.clock.rotate();
        self.hooks.on_rotate(retired, active);
    }

    /// Try to collect (reclaim) the arena that served `epoch`.
    ///
    /// Succeeds if all leases for that epoch have been returned.
    pub fn try_collect(&self, epoch: u64) -> Result<()> {
        self.clock.try_collect(epoch)?;
        self.hooks.on_collect(epoch);
        Ok(())
    }

    /// Register all arenas as io_uring fixed buffers.
    pub fn register(&mut self, ring: &io_uring::IoUring) -> Result<()> {
        self.registration
            .register(&ring.submitter(), self.clock.arenas())
    }

    /// Unregister io_uring fixed buffers.
    pub fn unregister(&mut self, ring: &io_uring::IoUring) -> Result<()> {
        self.registration.unregister(&ring.submitter())
    }

    /// Drain all cross-thread buffer returns, decrementing lease counts.
    pub fn drain_returns(&self) {
        while let Ok(ret) = self.receiver.try_recv() {
            if let Ok(arena) = self.clock.arena_for_epoch(ret.epoch) {
                arena.release_lease();
                self.hooks.on_release(ret.epoch, 0);
            } else {
                tracing::warn!(
                    epoch = ret.epoch,
                    "received return for unknown epoch"
                );
            }
        }
    }

    /// Create a [`TransferHandle`] for sending buffers to other threads.
    pub fn transfer_handle(&self) -> TransferHandle {
        TransferHandle::new(self.sender.clone())
    }

    /// The current epoch number.
    pub fn epoch(&self) -> u64 {
        self.clock.epoch()
    }

    /// Bytes available in the current arena.
    pub fn available(&self) -> usize {
        self.clock.current_arena().available()
    }

    /// Reference to the underlying epoch clock.
    pub fn clock(&self) -> &EpochClock {
        &self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::NoopHooks;

    fn test_pool() -> IouringBufferPool<NoopHooks> {
        let config = PoolConfig {
            arena_size: 4096,
            arena_count: 3,
            page_size: 4096,
        };
        IouringBufferPool::new(config, NoopHooks).unwrap()
    }

    #[test]
    fn lease_returns_buffer() {
        let pool = test_pool();
        let buf = pool.lease(64).unwrap();
        assert_eq!(buf.len(), 64);
        assert_eq!(buf.epoch(), 0);
    }

    #[test]
    fn lease_tracks_available() {
        let pool = test_pool();
        assert_eq!(pool.available(), 4096);

        let _buf = pool.lease(1024).unwrap();
        assert_eq!(pool.available(), 3072);
    }

    #[test]
    fn lease_returns_none_when_full() {
        let pool = test_pool();
        let _buf = pool.lease(4096).unwrap();
        assert!(pool.lease(1).is_none());
    }

    #[test]
    fn rotate_advances_epoch() {
        let pool = test_pool();
        assert_eq!(pool.epoch(), 0);

        pool.rotate();
        assert_eq!(pool.epoch(), 1);

        pool.rotate();
        assert_eq!(pool.epoch(), 2);
    }

    #[test]
    fn lease_rotate_collect_lifecycle() {
        let pool = test_pool();

        // Lease from epoch 0.
        let buf = pool.lease(128).unwrap();
        assert_eq!(buf.epoch(), 0);

        // Rotate to epoch 1.
        pool.rotate();

        // Can't collect epoch 0 — still has a lease.
        assert!(pool.try_collect(0).is_err());

        // Drop the lease.
        drop(buf);

        // Now collection succeeds.
        pool.try_collect(0).unwrap();
    }

    #[test]
    fn drain_returns_decrements_lease() {
        let pool = test_pool();

        // Lease and get transfer handle.
        let buf = pool.lease(64).unwrap();
        let handle = pool.transfer_handle();

        // Simulate cross-thread transfer.
        let sendable = crate::transfer::handle::SendableBuffer::new(
            buf.as_slice().as_ptr(),
            buf.len(),
            buf.epoch(),
            0, // arena_idx
            handle.sender().clone(),
        );

        // Drop the original lease (decrements once).
        drop(buf);

        // The arena lease_count is now 0 from the LeasedBuffer drop.
        // Acquire a new lease to simulate the sendable buffer's hold.
        pool.clock().current_arena().acquire_lease();
        pool.rotate();

        // Drop sendable — sends ReturnedBuffer through channel.
        drop(sendable);

        // Drain returns.
        pool.drain_returns();
    }

    #[test]
    fn multiple_leases_same_epoch() {
        let pool = test_pool();
        let _a = pool.lease(100).unwrap();
        let _b = pool.lease(100).unwrap();
        let _c = pool.lease(100).unwrap();

        assert_eq!(pool.available(), 4096 - 300);
    }
}
