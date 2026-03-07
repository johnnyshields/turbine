use std::marker::PhantomData;
use std::rc::Rc;

use crate::buffer::pinned::PinnedWrite;
use crate::epoch::arena::Arena;
use crate::transfer::handle::{SendableBuffer, TransferHandle};

/// A leased buffer from an epoch arena. Not `Send` — must stay on the
/// owning thread.
///
/// The `PhantomData<Rc<()>>` marker ensures `LeasedBuffer` is `!Send`
/// and `!Sync`, which is required because it holds a raw pointer into
/// a thread-local arena.
pub struct LeasedBuffer {
    ptr: *mut u8,
    len: usize,
    epoch: u64,
    buf_id: u32,
    /// io_uring registration slot for this buffer's arena.
    slot_id: u16,
    /// Raw pointer back to the arena for lease release on Drop.
    arena: *const Arena,
    arena_idx: usize,
    /// Prevent Send/Sync.
    _not_send: PhantomData<Rc<()>>,
}

impl LeasedBuffer {
    /// Create a new leased buffer.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to `len` valid bytes within the arena.
    /// - `arena` must remain valid for the lifetime of this lease.
    pub(crate) unsafe fn new(
        ptr: *mut u8,
        len: usize,
        epoch: u64,
        buf_id: u32,
        slot_id: u16,
        arena: *const Arena,
        arena_idx: usize,
    ) -> Self {
        Self {
            ptr,
            len,
            epoch,
            buf_id,
            slot_id,
            arena,
            arena_idx,
            _not_send: PhantomData,
        }
    }

    /// View the buffer contents as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// View the buffer contents as a mutable byte slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Copy the buffer contents into a new `Vec<u8>`.
    pub fn copy_out(&self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    /// The epoch this buffer belongs to.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The buffer ID within the arena.
    pub fn buf_id(&self) -> u32 {
        self.buf_id
    }

    /// The index of the arena in the slab.
    pub fn arena_idx(&self) -> usize {
        self.arena_idx
    }

    /// The io_uring registration slot for this buffer's arena.
    pub fn slot_id(&self) -> u16 {
        self.slot_id
    }

    /// Length of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Convert this leased buffer into a [`SendableBuffer`] that can cross thread boundaries.
    ///
    /// Consumes `self` **without** calling `Drop`, so the arena lease stays alive.
    /// The lease is transferred to the `SendableBuffer`; when it drops, a
    /// `ReturnedBuffer` message is sent through the channel for `drain_returns()`.
    pub fn into_sendable(self, handle: &TransferHandle) -> SendableBuffer {
        let me = std::mem::ManuallyDrop::new(self);
        SendableBuffer::new(
            me.ptr,
            me.len,
            me.epoch,
            me.arena_idx,
            me.buf_id,
            handle.sender().clone(),
        )
    }

    /// Pin this buffer for an io_uring write submission.
    ///
    /// The returned `PinnedWrite` borrows this buffer mutably, preventing
    /// it from being dropped while I/O is in flight.
    pub fn pin_for_write(&mut self) -> PinnedWrite<'_> {
        PinnedWrite::new(self)
    }
}

impl Drop for LeasedBuffer {
    fn drop(&mut self) {
        // SAFETY: arena pointer is valid while this lease exists because
        // the arena outlives all its leases.
        unsafe {
            (*self.arena).release_lease();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::arena::ArenaState;

    #[test]
    fn leased_buffer_read_and_copy() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(64).unwrap();
        arena.acquire_lease();

        // Write some data into the arena memory.
        unsafe {
            std::ptr::write_bytes(ptr, 0xAB, 64);
        }

        let buf = unsafe { LeasedBuffer::new(ptr, 64, 1, buf_id, 0, &arena as *const Arena, 0) };

        assert_eq!(buf.len(), 64);
        assert!(!buf.is_empty());
        assert_eq!(buf.epoch(), 1);
        assert_eq!(buf.buf_id(), 0);
        assert_eq!(buf.as_slice().len(), 64);
        assert!(buf.as_slice().iter().all(|&b| b == 0xAB));

        let copied = buf.copy_out();
        assert_eq!(copied.len(), 64);
        assert!(copied.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn leased_buffer_write() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(16).unwrap();
        arena.acquire_lease();

        let mut buf = unsafe { LeasedBuffer::new(ptr, 16, 1, buf_id, 0, &arena as *const Arena, 0) };
        buf.as_mut_slice().copy_from_slice(&[1u8; 16]);
        assert!(buf.as_slice().iter().all(|&b| b == 1));
    }

    #[test]
    fn drop_decrements_lease_count() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(32).unwrap();
        arena.acquire_lease();
        assert_eq!(arena.lease_count(), 1);

        {
            let _buf =
                unsafe { LeasedBuffer::new(ptr, 32, 1, buf_id, 0, &arena as *const Arena, 0) };
            assert_eq!(arena.lease_count(), 1);
        }
        // After drop, lease count should be decremented.
        assert_eq!(arena.lease_count(), 0);
    }

    #[test]
    fn empty_buffer() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        // Allocate zero bytes — pointer is valid but length is 0.
        let (ptr, buf_id) = arena.alloc(0).unwrap();
        arena.acquire_lease();

        let buf = unsafe { LeasedBuffer::new(ptr, 0, 1, buf_id, 0, &arena as *const Arena, 0) };
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.as_slice(), &[]);
    }

    #[test]
    fn pin_for_write_borrows_buffer() {
        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(64).unwrap();
        arena.acquire_lease();

        let mut buf = unsafe { LeasedBuffer::new(ptr, 64, 1, buf_id, 0, &arena as *const Arena, 0) };
        {
            let pinned = buf.pin_for_write();
            assert_eq!(pinned.len(), 64);
            assert_eq!(pinned.buf_index(), 0);
        }
        // Buffer is usable again after PinnedWrite is dropped.
        assert_eq!(buf.len(), 64);
    }

    #[test]
    fn into_sendable_keeps_lease_alive() {
        use crossbeam_channel::unbounded;
        use crate::transfer::handle::TransferHandle;

        let arena = Arena::new(4096).unwrap();
        arena.set_state(ArenaState::Writable);
        arena.set_epoch(1);

        let (ptr, buf_id) = arena.alloc(64).unwrap();
        arena.acquire_lease();

        let buf = unsafe { LeasedBuffer::new(ptr, 64, 1, buf_id, 0, &arena as *const Arena, 0) };

        let (tx, rx) = unbounded();
        let handle = TransferHandle::new(tx);

        // into_sendable consumes the LeasedBuffer without decrementing lease_count.
        let sendable = buf.into_sendable(&handle);
        assert_eq!(arena.lease_count(), 1, "lease must stay alive after into_sendable");

        // Drop the SendableBuffer — sends ReturnedBuffer through channel.
        drop(sendable);

        let returned = rx.try_recv().expect("should receive ReturnedBuffer on drop");
        assert_eq!(returned.epoch, 1);
        assert_eq!(returned.arena_idx, 0);
        assert_eq!(returned.buf_id, buf_id);

        // Manually release the lease (normally drain_returns would do this).
        arena.release_lease();
        assert_eq!(arena.lease_count(), 0);
    }
}
