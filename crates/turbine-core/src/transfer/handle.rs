use std::sync::atomic::{AtomicUsize, Ordering};

/// A buffer that can be sent across threads.
///
/// When dropped, atomically increments the arena's `remote_returns` counter
/// so the owning thread sees the lease as released on next `collect()`.
pub struct SendableBuffer {
    ptr: *const u8,
    len: usize,
    epoch: u64,
    remote_returns: *const AtomicUsize,
}

// SAFETY: `ptr` points into arena mmap memory that remains valid while
// outstanding leases > 0. `remote_returns` points into `Box<Arena>` which
// provides address stability, and the arena cannot be freed while
// outstanding leases exist (a live SendableBuffer means its fetch_add
// hasn't fired yet, so outstanding > 0). The AtomicUsize is designed
// for cross-thread access.
unsafe impl Send for SendableBuffer {}

impl SendableBuffer {
    #[inline]
    pub(crate) fn new(
        ptr: *const u8,
        len: usize,
        epoch: u64,
        remote_returns: *const AtomicUsize,
    ) -> Self {
        Self { ptr, len, epoch, remote_returns }
    }

    /// Read the buffer contents.
    ///
    /// # Safety
    /// The arena memory must still be valid (guaranteed by lease_count invariant).
    #[inline]
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl Drop for SendableBuffer {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: remote_returns points into Box<Arena> with stable address.
        // Arena cannot be freed while outstanding leases > 0, and this
        // SendableBuffer represents an outstanding lease until this fetch_add.
        unsafe {
            (*self.remote_returns).fetch_add(1, Ordering::Release);
        }
    }
}

// Compile-time assertion for trait bounds.
fn _assert_sendable_buffer_is_send<T: Send>() {}
#[allow(dead_code)]
const _: () = {
    let _ = _assert_sendable_buffer_is_send::<SendableBuffer>;
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::arena::{Arena, ArenaState};

    #[test]
    fn sendable_buffer_drop_decrements_via_atomic() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, _buf_id) = arena.alloc(64).unwrap();
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 1);

        {
            let _buf = SendableBuffer::new(
                ptr,
                64,
                1,
                arena.remote_returns_ptr(),
            );
            // lease still held
            assert_eq!(arena.lease_count(), 1);
        }
        // After SendableBuffer drop, remote_returns incremented.
        assert_eq!(arena.lease_count(), 0);
    }

    #[test]
    fn sendable_buffer_len_and_epoch() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(42);

        let (ptr, _) = arena.alloc(128).unwrap();
        arena.acquire_lease();

        let buf = SendableBuffer::new(ptr, 128, 42, arena.remote_returns_ptr());
        assert_eq!(buf.len(), 128);
        assert!(!buf.is_empty());
        assert_eq!(buf.epoch(), 42);
    }

    #[test]
    fn sendable_buffer_empty() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, _) = arena.alloc(0).unwrap();
        arena.acquire_lease();

        let buf = SendableBuffer::new(ptr, 0, 1, arena.remote_returns_ptr());
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());
        assert_eq!(buf.epoch(), 1);
    }

    #[test]
    fn sendable_buffer_as_slice() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, _) = arena.alloc(16).unwrap();
        arena.acquire_lease();

        // Write recognizable data
        unsafe { std::ptr::write_bytes(ptr, 0xFE, 16) };

        let buf = SendableBuffer::new(ptr, 16, 1, arena.remote_returns_ptr());
        let slice = unsafe { buf.as_slice() };
        assert_eq!(slice.len(), 16);
        assert!(slice.iter().all(|&b| b == 0xFE));
    }

    #[test]
    fn multiple_sendable_buffers_decrement() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (p1, _) = arena.alloc(32).unwrap();
        let (p2, _) = arena.alloc(32).unwrap();
        arena.acquire_lease();
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 2);

        let b1 = SendableBuffer::new(p1, 32, 1, arena.remote_returns_ptr());
        let b2 = SendableBuffer::new(p2, 32, 1, arena.remote_returns_ptr());

        drop(b1);
        assert_eq!(arena.lease_count(), 1);

        drop(b2);
        assert_eq!(arena.lease_count(), 0);
    }
}
