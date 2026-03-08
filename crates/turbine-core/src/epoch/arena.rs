use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{Result, TurbineError};

/// State of an arena in the epoch lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaState {
    /// Currently accepting allocations.
    Writable,
    /// Epoch has rotated; in-flight I/O may still reference this arena.
    Retired,
    /// All leases returned; arena memory can be recycled.
    Collected,
}

/// Cache-line aligned wrapper to prevent false sharing between
/// writer-hot fields and the cross-thread `remote_returns` counter.
#[repr(C, align(64))]
struct CacheAligned<T>(T);

impl<T> std::ops::Deref for CacheAligned<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for CacheAligned<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// An mmap-backed bump allocator for a single epoch's buffers.
///
/// Allocations are append-only with a single branch + store per allocation.
/// Lease counting uses `Cell<usize>` — no atomics, because arenas are
/// thread-local.
///
/// Field layout: writer-hot fields are grouped first, followed by
/// `remote_returns` on its own cache line to prevent false sharing
/// with cross-thread atomic updates.
#[repr(C)]
pub struct Arena {
    /// Base pointer to the mmap region.
    base: NonNull<u8>,
    /// Total capacity in bytes.
    capacity: usize,
    /// Current bump offset.
    offset: Cell<usize>,
    /// Number of outstanding leases (local acquires and releases).
    lease_count: Cell<usize>,
    /// Monotonically increasing buffer ID within this arena.
    next_buf_id: Cell<u32>,
    /// Lifecycle state.
    state: Cell<ArenaState>,
    /// The epoch this arena is associated with (set on activation).
    epoch: Cell<u64>,
    /// Number of cross-thread lease releases (atomically incremented by SendableBuffer::drop).
    /// Placed last and cache-aligned to avoid false sharing with writer-hot fields above.
    remote_returns: CacheAligned<AtomicUsize>,
}

// Compile-time assertion: remote_returns must start at or beyond the first cache line.
const _: () = assert!(std::mem::offset_of!(Arena, remote_returns) >= 64);

impl Arena {
    /// Create a new arena backed by an anonymous mmap of `size` bytes.
    ///
    /// `size` must be a multiple of `page_size` (typically 4096).
    pub(crate) fn new(size: usize) -> Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(TurbineError::Mmap(std::io::Error::last_os_error()));
        }

        let base = NonNull::new(ptr.cast::<u8>()).expect("mmap returned null without MAP_FAILED");

        Ok(Self {
            base,
            capacity: size,
            offset: Cell::new(0),
            lease_count: Cell::new(0),
            next_buf_id: Cell::new(0),
            state: Cell::new(ArenaState::Collected),
            epoch: Cell::new(0),
            remote_returns: CacheAligned(AtomicUsize::new(0)),
        })
    }

    /// Allocate `len` bytes from the bump allocator.
    ///
    /// Returns `(pointer, buf_id)` or `None` if the arena is full.
    /// The pointer is valid for the lifetime of the arena.
    #[inline(always)]
    pub fn alloc(&self, len: usize) -> Option<(*mut u8, u32)> {
        let current = self.offset.get();
        let new_offset = current + len;

        if new_offset > self.capacity {
            return None;
        }

        self.offset.set(new_offset);

        let buf_id = self.next_buf_id.get();
        self.next_buf_id.set(buf_id.wrapping_add(1));

        let ptr = unsafe { self.base.as_ptr().add(current) };
        Some((ptr, buf_id))
    }

    /// Increment the lease count.
    #[inline(always)]
    pub fn acquire_lease(&self) {
        self.lease_count.set(self.lease_count.get() + 1);
    }

    /// Decrement the lease count.
    #[inline(always)]
    pub fn release_lease(&self) {
        let count = self.lease_count.get();
        debug_assert!(count > 0, "release_lease called with zero lease count");
        self.lease_count.set(count - 1);
    }

    /// Record a cross-thread lease release (called from SendableBuffer::drop).
    #[inline]
    pub fn remote_release(&self) {
        self.remote_returns.fetch_add(1, Ordering::Release);
    }

    /// Pointer to the remote_returns counter for use by SendableBuffer.
    #[inline]
    pub fn remote_returns_ptr(&self) -> *const AtomicUsize {
        &self.remote_returns.0 as *const AtomicUsize
    }

    /// Fast check: does this arena definitely have outstanding leases?
    /// Uses Relaxed ordering — safe because if local > remote_relaxed,
    /// the true remote count is >= remote_relaxed, so (local - true_remote) > 0
    /// is guaranteed. Only returns false when it looks like leases might be zero,
    /// at which point the caller should use lease_count() with Acquire for the
    /// definitive answer.
    #[inline]
    pub fn has_outstanding_leases(&self) -> bool {
        let local = self.lease_count.get();
        let remote = self.remote_returns.load(Ordering::Relaxed);
        local > remote
    }

    /// Current number of outstanding leases (local minus remote returns).
    #[inline]
    pub fn lease_count(&self) -> usize {
        let local = self.lease_count.get();
        let remote = self.remote_returns.load(Ordering::Acquire);
        debug_assert!(local >= remote, "lease underflow: local={local}, remote={remote}");
        local - remote
    }

    /// Bytes remaining in this arena.
    #[inline]
    pub fn available(&self) -> usize {
        self.capacity - self.offset.get()
    }

    /// Total capacity of this arena.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes allocated so far.
    #[inline]
    pub fn used(&self) -> usize {
        self.offset.get()
    }

    /// Current lifecycle state.
    #[inline]
    pub fn state(&self) -> ArenaState {
        self.state.get()
    }

    /// Set the lifecycle state.
    pub fn set_state(&self, state: ArenaState) {
        self.state.set(state);
    }

    /// The epoch this arena is associated with.
    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch.get()
    }

    /// Set the epoch (called when the arena is activated).
    pub fn set_epoch(&self, epoch: u64) {
        self.epoch.set(epoch);
    }

    /// Reset the arena for reuse: zero the bump offset, buf_id counter, lease count, and remote returns.
    pub fn reset(&self) {
        self.offset.set(0);
        self.next_buf_id.set(0);
        self.lease_count.set(0);
        self.remote_returns.store(0, Ordering::Relaxed);
        self.state.set(ArenaState::Writable);
    }

    /// Base pointer for io_uring buffer registration.
    pub fn base_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Hint to OS that unused pages can be reclaimed.
    /// Called during `collect()` when an arena has zero outstanding leases.
    #[cold]
    pub fn advise_free_unused(&self, page_size: usize) {
        let used = self.offset.get();
        let start = (used + page_size - 1) & !(page_size - 1); // align up
        if start < self.capacity {
            let ret = unsafe {
                libc::madvise(
                    self.base.as_ptr().add(start).cast(),
                    self.capacity - start,
                    libc::MADV_FREE,
                )
            };
            if ret != 0 {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "madvise(MADV_FREE) failed"
                );
            }
        }
    }

    /// Return an iovec describing the entire arena.
    pub fn as_iovec(&self) -> libc::iovec {
        libc::iovec {
            iov_base: self.base.as_ptr().cast(),
            iov_len: self.capacity,
        }
    }
}

impl Drop for Arena {
    #[cold]
    fn drop(&mut self) {
        let local = self.lease_count.get();
        let remote = *self.remote_returns.0.get_mut();
        let outstanding = local - remote;
        if outstanding > 0 {
            debug_assert_eq!(outstanding, 0, "arena dropped with {} outstanding leases", outstanding);
            tracing::warn!(
                epoch = self.epoch.get(),
                leases = outstanding,
                "arena dropped with outstanding leases"
            );
        }

        let ret = unsafe { libc::munmap(self.base.as_ptr().cast(), self.capacity) };
        if ret != 0 {
            tracing::error!(
                error = %std::io::Error::last_os_error(),
                "munmap failed on arena drop"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_up_to_capacity() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);

        let (ptr, id) = arena.alloc(1024).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(id, 0);
        assert_eq!(arena.available(), 3072);

        let (_, id2) = arena.alloc(3072).unwrap();
        assert_eq!(id2, 1);
        assert_eq!(arena.available(), 0);
    }

    #[test]
    fn alloc_returns_none_when_full() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);

        arena.alloc(4096).unwrap();
        assert!(arena.alloc(1).is_none());
    }

    #[test]
    fn lease_count_tracking() {
        let arena = Arena::new(4096).unwrap();
        assert_eq!(arena.lease_count(), 0);

        arena.acquire_lease();
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 2);

        arena.release_lease();
        assert_eq!(arena.lease_count(), 1);

        arena.release_lease();
        assert_eq!(arena.lease_count(), 0);
    }

    #[test]
    fn reset_clears_offset_and_buf_id() {
        let arena = Arena::new(4096).unwrap();
        arena.alloc(2048).unwrap();
        assert_eq!(arena.used(), 2048);

        arena.reset();
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.available(), 4096);

        let (_, id) = arena.alloc(100).unwrap();
        assert_eq!(id, 0);
    }

    #[test]
    fn as_iovec_covers_full_capacity() {
        let arena = Arena::new(8192).unwrap();
        let iov = arena.as_iovec();
        assert_eq!(iov.iov_len, 8192);
        assert_eq!(iov.iov_base as *mut u8, arena.base_ptr());
    }

    #[test]
    fn remote_release_decrements_outstanding() {
        let arena = Arena::new(4096).unwrap();
        arena.acquire_lease();
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 2);

        arena.remote_release();
        assert_eq!(arena.lease_count(), 1);

        arena.remote_release();
        assert_eq!(arena.lease_count(), 0);
    }

    #[test]
    fn reset_clears_remote_returns() {
        let arena = Arena::new(4096).unwrap();
        arena.acquire_lease();
        arena.remote_release();
        assert_eq!(arena.lease_count(), 0);

        arena.reset();
        // After reset, lease_count should be 0 with no remote returns.
        assert_eq!(arena.lease_count(), 0);

        // New leases should work correctly.
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 1);
        arena.release_lease();
        assert_eq!(arena.lease_count(), 0);
    }

    #[test]
    fn mixed_local_and_remote_release() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);

        // Acquire 3 leases.
        arena.acquire_lease();
        arena.acquire_lease();
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 3);

        // Release 1 locally.
        arena.release_lease();
        assert_eq!(arena.lease_count(), 2);

        // Release 2 remotely.
        arena.remote_release();
        assert_eq!(arena.lease_count(), 1);

        arena.remote_release();
        assert_eq!(arena.lease_count(), 0);
    }

    #[test]
    fn has_outstanding_leases_after_reset() {
        let arena = Arena::new(4096).unwrap();

        // Acquire leases and verify outstanding.
        arena.acquire_lease();
        arena.acquire_lease();
        assert!(arena.has_outstanding_leases());

        // Simulate cross-thread returns.
        arena.remote_release();
        assert!(arena.has_outstanding_leases());

        arena.remote_release();
        assert!(!arena.has_outstanding_leases());

        // Reset should clear everything.
        arena.reset();
        assert!(!arena.has_outstanding_leases());

        // New leases post-reset should be tracked correctly.
        arena.acquire_lease();
        assert!(arena.has_outstanding_leases());

        arena.release_lease();
        assert!(!arena.has_outstanding_leases());
    }

    #[test]
    fn has_outstanding_leases_fast_check() {
        let arena = Arena::new(4096).unwrap();
        assert!(!arena.has_outstanding_leases());

        arena.acquire_lease();
        assert!(arena.has_outstanding_leases());

        arena.acquire_lease();
        assert!(arena.has_outstanding_leases());

        // One remote release — still has 1 outstanding
        arena.remote_release();
        assert!(arena.has_outstanding_leases());

        // Second remote release — now at zero
        arena.remote_release();
        assert!(!arena.has_outstanding_leases());
    }

    #[test]
    fn has_outstanding_leases_with_local_release() {
        let arena = Arena::new(4096).unwrap();
        arena.acquire_lease();
        arena.acquire_lease();
        assert!(arena.has_outstanding_leases());

        arena.release_lease();
        assert!(arena.has_outstanding_leases());

        arena.release_lease();
        assert!(!arena.has_outstanding_leases());
    }

    #[test]
    fn capacity_returns_size() {
        let arena = Arena::new(8192).unwrap();
        assert_eq!(arena.capacity(), 8192);
    }

    #[test]
    fn state_transitions() {
        let arena = Arena::new(4096).unwrap();
        assert_eq!(arena.state(), ArenaState::Collected); // default from new()

        arena.set_state(ArenaState::Writable);
        assert_eq!(arena.state(), ArenaState::Writable);

        arena.set_state(ArenaState::Retired);
        assert_eq!(arena.state(), ArenaState::Retired);

        arena.set_state(ArenaState::Collected);
        assert_eq!(arena.state(), ArenaState::Collected);
    }

    #[test]
    fn epoch_get_set() {
        let arena = Arena::new(4096).unwrap();
        assert_eq!(arena.epoch(), 0);

        arena.set_epoch(42);
        assert_eq!(arena.epoch(), 42);

        arena.set_epoch(u64::MAX);
        assert_eq!(arena.epoch(), u64::MAX);
    }

    #[test]
    fn base_ptr_is_non_null() {
        let arena = Arena::new(4096).unwrap();
        assert!(!arena.base_ptr().is_null());
    }

    #[test]
    fn used_tracks_allocations() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        assert_eq!(arena.used(), 0);

        arena.alloc(100).unwrap();
        assert_eq!(arena.used(), 100);

        arena.alloc(200).unwrap();
        assert_eq!(arena.used(), 300);
    }

    #[test]
    fn advise_free_unused_no_op_when_full() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.alloc(4096).unwrap();
        // start would be aligned up from 4096 = 4096, which equals capacity → no madvise
        arena.advise_free_unused(4096);
    }

    #[test]
    fn advise_free_unused_with_partial_use() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.alloc(100).unwrap();
        // Should madvise the unused portion — should not panic
        arena.advise_free_unused(4096);
    }

    #[test]
    fn arena_state_debug() {
        assert_eq!(format!("{:?}", ArenaState::Writable), "Writable");
        assert_eq!(format!("{:?}", ArenaState::Retired), "Retired");
        assert_eq!(format!("{:?}", ArenaState::Collected), "Collected");
    }

    #[test]
    fn arena_state_clone_eq() {
        let s = ArenaState::Writable;
        let s2 = s.clone();
        assert_eq!(s, s2);
        assert_ne!(ArenaState::Writable, ArenaState::Retired);
    }

    #[test]
    fn remote_returns_ptr_is_stable() {
        let arena = Arena::new(4096).unwrap();
        let p1 = arena.remote_returns_ptr();
        let p2 = arena.remote_returns_ptr();
        assert_eq!(p1, p2);
    }

    #[test]
    fn alloc_zero_bytes() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        let (ptr, id) = arena.alloc(0).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(id, 0);
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.available(), 4096);
    }

    #[test]
    fn buf_id_wraps() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        // Allocate many zero-size buffers to check buf_id increments
        for expected in 0..10u32 {
            let (_, id) = arena.alloc(0).unwrap();
            assert_eq!(id, expected);
        }
    }

    #[test]
    fn allocations_are_contiguous() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);

        let (p1, _) = arena.alloc(64).unwrap();
        let (p2, _) = arena.alloc(64).unwrap();

        let diff = unsafe { p2.offset_from(p1) };
        assert_eq!(diff, 64);
    }
}
