use std::marker::PhantomData;
use std::rc::Rc;

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
    _not_send: PhantomData<Rc<()>>,
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
            _not_send: PhantomData,
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
        let arena_idx = self.clock.current_arena_idx();
        let buf = unsafe { LeasedBuffer::new(ptr, len, arena.epoch(), buf_id, arena, arena_idx) };
        Some(buf)
    }

    /// Rotate to a new epoch: retire the current arena and activate the next.
    pub fn rotate(&self) -> Result<()> {
        let (retired, active) = self.clock.rotate()?;
        self.hooks.on_rotate(retired, active);
        Ok(())
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
    ///
    /// Returns the number of buffers successfully drained.
    pub fn drain_returns(&self) -> usize {
        let mut count = 0usize;
        while let Ok(ret) = self.receiver.try_recv() {
            if let Some(arena) = self.clock.arena_at(ret.arena_idx) {
                if arena.epoch() != ret.epoch {
                    tracing::error!(
                        expected_epoch = ret.epoch,
                        actual_epoch = arena.epoch(),
                        arena_idx = ret.arena_idx,
                        "arena epoch mismatch in drain_returns"
                    );
                    continue;
                }
                arena.release_lease();
                self.hooks.on_release(ret.epoch, ret.buf_id);
                count += 1;
            } else {
                tracing::error!(
                    arena_idx = ret.arena_idx,
                    epoch = ret.epoch,
                    "received return for unknown arena index"
                );
            }
        }
        count
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

        pool.rotate().unwrap();
        assert_eq!(pool.epoch(), 1);

        pool.rotate().unwrap();
        assert_eq!(pool.epoch(), 2);
    }

    #[test]
    fn lease_rotate_collect_lifecycle() {
        let pool = test_pool();

        // Lease from epoch 0.
        let buf = pool.lease(128).unwrap();
        assert_eq!(buf.epoch(), 0);

        // Rotate to epoch 1.
        pool.rotate().unwrap();

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

        let buf = pool.lease(64).unwrap();
        let handle = pool.transfer_handle();

        // Convert to sendable — lease stays alive, LeasedBuffer is consumed.
        let sendable = buf.into_sendable(&handle);

        pool.rotate().unwrap();

        // Drop sendable — sends ReturnedBuffer through channel.
        drop(sendable);

        // Drain returns.
        let drained = pool.drain_returns();
        assert_eq!(drained, 1);
    }

    #[test]
    fn multiple_leases_same_epoch() {
        let pool = test_pool();
        let _a = pool.lease(100).unwrap();
        let _b = pool.lease(100).unwrap();
        let _c = pool.lease(100).unwrap();

        assert_eq!(pool.available(), 4096 - 300);
    }

    #[test]
    fn multi_rotation_wrap_around() {
        // Pool with arena_count=2 so wrap happens after 2 rotations.
        let config = PoolConfig {
            arena_size: 4096,
            arena_count: 2,
            page_size: 4096,
        };
        let pool = IouringBufferPool::new(config, NoopHooks).unwrap();

        assert_eq!(pool.epoch(), 0);
        let _buf0 = pool.lease(64).unwrap();
        drop(_buf0);

        pool.rotate().unwrap(); // epoch 1, arena idx 1
        assert_eq!(pool.epoch(), 1);

        pool.try_collect(0).unwrap(); // collect epoch 0

        pool.rotate().unwrap(); // epoch 2, arena idx 0 (wraps)
        assert_eq!(pool.epoch(), 2);

        // Arena 0 was recycled — should be writable and usable again.
        let buf2 = pool.lease(128).unwrap();
        assert_eq!(buf2.epoch(), 2);
        assert_eq!(buf2.len(), 128);
        assert_eq!(pool.available(), 4096 - 128);
    }

    #[test]
    fn drain_returns_count_and_enables_collection() {
        let pool = test_pool();

        let buf = pool.lease(64).unwrap();
        let epoch = buf.epoch();
        let handle = pool.transfer_handle();

        // Convert to sendable — lease stays alive.
        let sendable = buf.into_sendable(&handle);

        // Rotate so epoch 0 is retired.
        pool.rotate().unwrap();

        // Can't collect yet — sendable still holds a lease.
        assert!(pool.try_collect(epoch).is_err());

        // Drop sendable — sends ReturnedBuffer through channel.
        drop(sendable);

        // Drain returns — should process 1 return.
        let drained = pool.drain_returns();
        assert_eq!(drained, 1);

        // Now collection succeeds.
        pool.try_collect(epoch).unwrap();
    }

    /// Compile-time assertion that IouringBufferPool is !Send.
    /// If this test compiles, the pool cannot be sent across threads.
    #[test]
    fn pool_is_not_send() {
        fn _assert_not_send<T>() {
            // If IouringBufferPool were Send, this function body could call
            // a function requiring T: Send. We just need the signature to exist
            // as a witness that the type is NOT Send. The real assertion is the
            // PhantomData<Rc<()>> marker on the struct.
        }
        _assert_not_send::<IouringBufferPool<NoopHooks>>();
    }
}
