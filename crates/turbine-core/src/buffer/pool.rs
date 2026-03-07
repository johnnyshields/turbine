use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::rc::Rc;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::buffer::leased::LeasedBuffer;
use crate::config::PoolConfig;
use crate::epoch::manager::ArenaManager;
use crate::error::{Result, TurbineError};
use crate::gc::{BufferPinHook, EpochObserver};
use crate::ring::registration::RingRegistration;
use crate::transfer::handle::{ReturnedBuffer, TransferHandle};
use crate::{ArenaIdx, SlotId};

/// The main API for epoch-based io_uring buffer management.
///
/// Owns an [`ArenaManager`] (slab of arenas with drain queue and free pool),
/// a [`RingRegistration`], and a crossbeam channel for cross-thread buffer returns.
///
/// Uses `UnsafeCell` for interior mutability of the manager and registration.
/// This is safe because the pool is `!Send` (enforced by `PhantomData<Rc<()>>`),
/// guaranteeing single-threaded access. No method holds a reference to the
/// inner data across a call to another method.
pub struct IouringBufferPool<H> {
    manager: UnsafeCell<ArenaManager>,
    registration: UnsafeCell<RingRegistration>,
    sender: Sender<ReturnedBuffer>,
    receiver: Receiver<ReturnedBuffer>,
    hooks: H,
    _not_send: PhantomData<Rc<()>>,
}

impl<H> IouringBufferPool<H> {
    #[inline]
    fn mgr(&self) -> &ArenaManager {
        unsafe { &*self.manager.get() }
    }

    /// # Safety justification
    /// Pool is !Send (PhantomData<Rc<()>>), guaranteeing single-threaded access.
    /// No method holds a reference across a call to another method.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn mgr_mut(&self) -> &mut ArenaManager {
        unsafe { &mut *self.manager.get() }
    }

    #[inline]
    fn reg(&self) -> &RingRegistration {
        unsafe { &*self.registration.get() }
    }

    /// See mgr_mut safety justification.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn reg_mut(&self) -> &mut RingRegistration {
        unsafe { &mut *self.registration.get() }
    }
}

impl<H: BufferPinHook + EpochObserver> IouringBufferPool<H> {
    /// Create a new buffer pool with the given configuration and hooks.
    pub fn new(config: PoolConfig, hooks: H) -> Result<Self> {
        let manager = ArenaManager::new(&config)?;
        let registration = RingRegistration::new(config.registration_slots);
        let (sender, receiver) = unbounded();

        Ok(Self {
            manager: UnsafeCell::new(manager),
            registration: UnsafeCell::new(registration),
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
    #[inline]
    pub fn lease(&self, len: usize) -> Option<LeasedBuffer> {
        let mgr = self.mgr();
        let arena = mgr.current_arena();
        let (ptr, buf_id) = arena.alloc(len)?;
        arena.acquire_lease();

        self.hooks.on_pin(arena.epoch(), buf_id);

        let arena_idx = mgr.current_arena_idx();
        let slot_id = match self.reg().slot_for_arena(arena_idx) {
            Some(id) => id,
            None => {
                if self.reg().is_registered() {
                    tracing::warn!(
                        arena_idx = arena_idx.as_usize(),
                        "arena has no registration slot despite pool being registered"
                    );
                }
                SlotId::new(0)
            }
        };
        // SAFETY: ptr points into the arena's mmap region which is valid
        // for the arena's lifetime. The arena outlives the lease because
        // lease_count > 0 prevents collection.
        let buf = unsafe {
            LeasedBuffer::new(ptr, len, arena.epoch(), buf_id, slot_id, arena, arena_idx)
        };
        Some(buf)
    }

    /// Lease `len` bytes, auto-rotating if the current arena is full.
    ///
    /// Tries to lease from the current arena. If full, rotates to a new epoch
    /// and retries. Returns an error if rotation fails or the new arena is
    /// also too small.
    pub fn lease_or_rotate(&self, len: usize) -> Result<LeasedBuffer> {
        if let Some(buf) = self.lease(len) {
            return Ok(buf);
        }

        self.rotate()?;

        self.lease(len).ok_or_else(|| {
            let available = self.mgr().current_arena().available();
            TurbineError::ArenaFull {
                requested: len,
                available,
            }
        })
    }

    /// Rotate to a new epoch: retire the current arena and activate the next.
    pub fn rotate(&self) -> Result<()> {
        let result = self.mgr_mut().rotate()?;
        self.hooks.on_rotate(result.retired_epoch, result.new_epoch);

        if let Some(new_idx) = result.new_arena_idx {
            let _ = self.reg_mut().register_arena(new_idx);
            self.hooks.on_arena_alloc(new_idx);
        }

        Ok(())
    }

    /// Collect all draining arenas with zero leases back to the free pool.
    ///
    /// Returns the number of arenas collected.
    pub fn collect(&self) -> usize {
        let collected = self.mgr_mut().collect();
        self.hooks.on_collect_sweep(collected);
        collected
    }

    /// Try to collect (reclaim) the arena that served `epoch`.
    ///
    /// Succeeds if all leases for that epoch have been returned.
    pub fn collect_epoch(&self, epoch: u64) -> Result<()> {
        let mgr = self.mgr();
        let arena = mgr
            .live_arenas()
            .find(|(_, a)| a.epoch() == epoch)
            .map(|(_, a)| a)
            .ok_or(TurbineError::EpochNotFound(epoch))?;

        if arena.lease_count() > 0 {
            return Err(TurbineError::EpochNotCollectable(
                epoch,
                arena.lease_count(),
            ));
        }

        arena.set_state(crate::epoch::arena::ArenaState::Collected);
        self.hooks.on_collect(epoch);
        Ok(())
    }

    /// Shrink the free pool, munmapping excess arenas beyond `max_free_arenas`.
    ///
    /// Returns the number of arenas freed.
    pub fn shrink(&self) -> usize {
        self.mgr_mut().shrink()
    }

    /// Register all arenas as io_uring fixed buffers.
    pub fn register(&self, ring: &io_uring::IoUring) -> Result<()> {
        self.reg_mut()
            .register(&ring.submitter(), self.mgr().live_arenas())
    }

    /// Unregister io_uring fixed buffers.
    pub fn unregister(&self, ring: &io_uring::IoUring) -> Result<()> {
        self.reg_mut().unregister(&ring.submitter())
    }

    /// Drain all cross-thread buffer returns, decrementing lease counts.
    ///
    /// Returns the number of buffers successfully drained.
    pub fn drain_returns(&self) -> usize {
        let mut count = 0usize;
        // Cache the last arena lookup — consecutive returns often share an arena.
        let mut last_idx = ArenaIdx::new(usize::MAX);
        let mut last_arena: Option<&crate::epoch::arena::Arena> = None;
        while let Ok(ret) = self.receiver.try_recv() {
            let arena = if ret.arena_idx == last_idx {
                last_arena.unwrap()
            } else {
                match self.mgr().arena_at(ret.arena_idx) {
                    Some(a) => {
                        last_idx = ret.arena_idx;
                        last_arena = Some(a);
                        a
                    }
                    None => {
                        tracing::error!(
                            arena_idx = ret.arena_idx.as_usize(),
                            epoch = ret.epoch,
                            "received return for unknown arena index"
                        );
                        continue;
                    }
                }
            };
            if arena.epoch() != ret.epoch {
                tracing::error!(
                    expected_epoch = ret.epoch,
                    actual_epoch = arena.epoch(),
                    arena_idx = ret.arena_idx.as_usize(),
                    "arena epoch mismatch in drain_returns"
                );
                continue;
            }
            arena.release_lease();
            self.hooks.on_release(ret.epoch, ret.buf_id);
            count += 1;
        }
        count
    }

    /// Create a [`TransferHandle`] for sending buffers to other threads.
    pub fn transfer_handle(&self) -> TransferHandle {
        TransferHandle::new(self.sender.clone())
    }

    /// The current epoch number.
    pub fn epoch(&self) -> u64 {
        self.mgr().epoch()
    }

    /// Bytes available in the current arena.
    pub fn available(&self) -> usize {
        self.mgr().current_arena().available()
    }

    /// Number of arenas in the drain queue.
    pub fn draining_count(&self) -> usize {
        self.mgr().draining_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::NoopHooks;

    fn test_pool() -> IouringBufferPool<NoopHooks> {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 3,
            max_free_arenas: 4,
            max_total_arenas: 0,
            registration_slots: 32,
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

        let buf = pool.lease(128).unwrap();
        assert_eq!(buf.epoch(), 0);

        pool.rotate().unwrap();

        // Can't collect epoch 0 — still has a lease.
        assert!(pool.collect_epoch(0).is_err());

        drop(buf);

        pool.collect_epoch(0).unwrap();
    }

    #[test]
    fn drain_returns_decrements_lease() {
        let pool = test_pool();

        let buf = pool.lease(64).unwrap();
        let handle = pool.transfer_handle();

        let sendable = buf.into_sendable(&handle);
        pool.rotate().unwrap();

        drop(sendable);

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
    fn rotate_with_outstanding_leases() {
        // With the new architecture, rotate always succeeds even with leases.
        let pool = test_pool();

        let _buf0 = pool.lease(64).unwrap();
        pool.rotate().unwrap();

        let _buf1 = pool.lease(64).unwrap();
        pool.rotate().unwrap();

        assert_eq!(pool.epoch(), 2);
        assert_eq!(pool.draining_count(), 2);
    }

    #[test]
    fn lease_or_rotate_auto_rotates() {
        let pool = test_pool();

        let _buf = pool.lease(4096).unwrap();
        assert!(pool.lease(1).is_none());

        let buf = pool.lease_or_rotate(64).unwrap();
        assert_eq!(buf.epoch(), 1);
        assert_eq!(pool.epoch(), 1);
    }

    #[test]
    fn collect_sweeps_draining() {
        let pool = test_pool();

        pool.rotate().unwrap();
        pool.rotate().unwrap();
        assert_eq!(pool.draining_count(), 2);

        let collected = pool.collect();
        assert_eq!(collected, 2);
        assert_eq!(pool.draining_count(), 0);
    }

    #[test]
    fn drain_returns_count_and_enables_collection() {
        let pool = test_pool();

        let buf = pool.lease(64).unwrap();
        let epoch = buf.epoch();
        let handle = pool.transfer_handle();

        let sendable = buf.into_sendable(&handle);

        pool.rotate().unwrap();

        assert!(pool.collect_epoch(epoch).is_err());

        drop(sendable);

        let drained = pool.drain_returns();
        assert_eq!(drained, 1);

        pool.collect_epoch(epoch).unwrap();
    }

    #[test]
    fn shrink_cleans_up_arenas() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 5,
            max_free_arenas: 1,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let pool = IouringBufferPool::new(config, NoopHooks).unwrap();

        // Use up arenas via rotation.
        pool.rotate().unwrap();
        pool.rotate().unwrap();

        // Collect draining arenas back to free pool.
        let collected = pool.collect();
        assert!(collected >= 2);

        // Shrink should remove excess free arenas.
        let removed = pool.shrink();
        assert!(removed > 0, "should have removed excess free arenas");
    }

    /// Compile-time assertion that IouringBufferPool is !Send.
    #[test]
    fn pool_is_not_send() {
        fn _assert_not_send<T>() {}
        _assert_not_send::<IouringBufferPool<NoopHooks>>();
    }
}
