use crate::buffer::leased::LeasedBuffer;
use crate::SlotId;

/// A borrow guard that pins a `LeasedBuffer` during io_uring submission.
///
/// Prevents the buffer from being dropped while I/O is in flight by
/// holding a mutable borrow on the underlying `LeasedBuffer`.
pub struct PinnedWrite<'a> {
    buffer: &'a mut LeasedBuffer,
}

impl<'a> PinnedWrite<'a> {
    /// Create a new pinned write guard.
    pub(crate) fn new(buffer: &'a mut LeasedBuffer) -> Self {
        Self { buffer }
    }

    /// Raw pointer to the start of the buffer (for io_uring SQE).
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.buffer.as_slice().as_ptr()
    }

    /// Mutable raw pointer to the start of the buffer.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.buffer.as_mut_slice().as_mut_ptr()
    }

    /// Length of the pinned buffer in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if the pinned buffer has zero length.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// The io_uring registration slot index for fixed-buffer operations.
    #[inline]
    pub fn buf_index(&self) -> SlotId {
        self.buffer.slot_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::arena::{Arena, ArenaState};
    use crate::ArenaIdx;

    #[test]
    fn pinned_write_accessors() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(128).unwrap();
        arena.acquire_lease();

        let mut buf = unsafe { LeasedBuffer::new(ptr, 128, 1, buf_id, SlotId::new(0), &arena as *const Arena, ArenaIdx::new(0)) };

        let mut pinned = buf.pin_for_write();
        assert_eq!(pinned.len(), 128);
        assert!(!pinned.is_empty());
        assert_eq!(pinned.buf_index(), SlotId::new(0));
        assert!(!pinned.as_ptr().is_null());
        assert!(!pinned.as_mut_ptr().is_null());
    }

    #[test]
    fn pinned_write_empty_buffer() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(0).unwrap();
        arena.acquire_lease();

        let mut buf = unsafe { LeasedBuffer::new(ptr, 0, 1, buf_id, SlotId::new(0), &arena as *const Arena, ArenaIdx::new(0)) };

        let pinned = buf.pin_for_write();
        assert!(pinned.is_empty());
        assert_eq!(pinned.len(), 0);
    }
}
