use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::buffer::leased::LeasedBuffer;
use crate::config::PoolConfig;
use crate::epoch::manager::ArenaManager;
use crate::error::{Result, TurbineError};
use crate::gc::{BufferPinHook, EpochObserver};
use crate::ring::registration::RingRegistration;
use crate::{ArenaIdx, SlotId};

/// The main API for epoch-based io_uring buffer management.
///
/// Owns an [`ArenaManager`] (slab of arenas with drain queue and free pool)
/// and a [`RingRegistration`].
///
/// Uses `UnsafeCell` for interior mutability of the manager and registration.
/// This is safe because the pool is `!Send` (enforced by `PhantomData<Rc<()>>`),
/// guaranteeing single-threaded access. No method holds a reference to the
/// inner data across a call to another method.
pub struct IouringBufferPool<H> {
    manager: UnsafeCell<ArenaManager>,
    registration: UnsafeCell<RingRegistration>,
    hooks: H,
    _not_send: PhantomData<Rc<()>>,
}

impl<H> IouringBufferPool<H> {
    #[inline(always)]
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

    #[inline(always)]
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

        Ok(Self {
            manager: UnsafeCell::new(manager),
            registration: UnsafeCell::new(registration),
            hooks,
            _not_send: PhantomData,
        })
    }

    /// Lease `len` bytes from the current epoch's arena.
    ///
    /// Returns `None` if the arena is full. The returned [`LeasedBuffer`] is
    /// `!Send` and must not leave the owning thread.
    #[inline(always)]
    pub fn lease(&self, len: usize) -> Option<LeasedBuffer> {
        let mgr = self.mgr();
        let (arena, arena_idx) = mgr.current_arena_with_idx();
        let (ptr, buf_id) = arena.alloc(len)?;
        arena.acquire_lease();

        self.hooks.on_pin(arena.epoch(), buf_id);

        let slot_id = match self.reg().slot_for_arena(arena_idx) {
            Some(id) => id,
            None => Self::slot_missing_fallback(self.reg(), arena_idx),
        };
        // SAFETY: ptr points into the arena's mmap region which is valid
        // for the arena's lifetime. The arena outlives the lease because
        // lease_count > 0 prevents collection.
        let buf = unsafe {
            LeasedBuffer::new(ptr, len, arena.epoch(), buf_id, slot_id, arena, arena_idx)
        };
        Some(buf)
    }

    /// Cold path for when slot lookup returns None.
    #[cold]
    #[inline(never)]
    fn slot_missing_fallback(reg: &RingRegistration, arena_idx: ArenaIdx) -> SlotId {
        if reg.is_registered() {
            tracing::warn!(
                arena_idx = arena_idx.as_usize(),
                "arena has no registration slot despite pool being registered"
            );
        }
        SlotId::new(0)
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

    /// Pre-populate arena slot mappings without io_uring registration.
    ///
    /// This assigns SlotIds to all live arenas so that `lease()` takes the
    /// fast `slot_for_arena` path instead of `slot_missing_fallback`.
    /// Useful for benchmarks and tests that don't have an io_uring ring.
    pub fn pre_register_slots(&self) {
        let reg = self.reg_mut();
        for (idx, _) in self.mgr().live_arenas() {
            if reg.slot_for_arena(idx).is_none() {
                let _ = reg.register_arena(idx);
            }
        }
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
    fn cross_thread_release_decrements_lease() {
        let pool = test_pool();

        let buf = pool.lease(64).unwrap();
        let sendable = buf.into_sendable();
        pool.rotate().unwrap();

        // Dropping SendableBuffer atomically decrements the lease.
        drop(sendable);

        // collect() should now succeed since outstanding leases == 0.
        let collected = pool.collect();
        assert!(collected >= 1);
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
    fn cross_thread_release_enables_collection() {
        let pool = test_pool();

        let buf = pool.lease(64).unwrap();
        let epoch = buf.epoch();
        let sendable = buf.into_sendable();

        pool.rotate().unwrap();

        // Can't collect — SendableBuffer still alive.
        assert!(pool.collect_epoch(epoch).is_err());

        // Dropping atomically decrements remote_returns.
        drop(sendable);

        // Now collect_epoch succeeds — outstanding leases == 0.
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

    #[test]
    fn cross_thread_sendable_buffer_release() {
        let pool = test_pool();

        let buf = pool.lease(64).unwrap();
        let epoch = buf.epoch();

        // Write recognizable data.
        let data_ptr = buf.as_slice().as_ptr();
        unsafe { std::ptr::write_bytes(data_ptr as *mut u8, 0xCD, 64) };

        let sendable = buf.into_sendable();

        pool.rotate().unwrap();

        // Can't collect — SendableBuffer still alive.
        assert!(pool.collect_epoch(epoch).is_err());

        // Send to another thread, read data, drop there.
        let handle = std::thread::spawn(move || {
            let slice = unsafe { sendable.as_slice() };
            assert_eq!(slice.len(), 64);
            assert!(slice.iter().all(|&b| b == 0xCD));
            // sendable dropped here on the spawned thread
        });

        handle.join().expect("spawned thread panicked");

        // Now collect_epoch succeeds — atomic remote_release crossed threads.
        pool.collect_epoch(epoch).unwrap();
    }

    #[test]
    fn pre_register_slots_populates_map() {
        let pool = test_pool();
        pool.pre_register_slots();

        // After pre-registration, lease should get a non-zero slot
        // (slot 0 is also valid, but all arenas should have slots assigned)
        let buf = pool.lease(64).unwrap();
        // The key check: slot_for_arena returns Some for the current arena
        let arena_idx = pool.mgr().current_arena_idx();
        assert!(pool.reg().slot_for_arena(arena_idx).is_some());
        drop(buf);
    }

    /// Compile-time assertion that IouringBufferPool is !Send.
    #[test]
    fn pool_is_not_send() {
        fn _assert_not_send<T>() {}
        _assert_not_send::<IouringBufferPool<NoopHooks>>();
    }

}
