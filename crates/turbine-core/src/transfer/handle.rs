use crossbeam_channel::Sender;

/// Message sent back through the channel when a [`SendableBuffer`] is dropped.
#[derive(Debug, Clone, Copy)]
pub struct ReturnedBuffer {
    pub epoch: u64,
    pub arena_idx: usize,
    pub buf_id: u32,
}

/// A clonable, `Send + Sync` handle for returning buffers from other threads.
#[derive(Debug, Clone)]
pub struct TransferHandle {
    sender: Sender<ReturnedBuffer>,
}

impl TransferHandle {
    pub fn new(sender: Sender<ReturnedBuffer>) -> Self {
        Self { sender }
    }

    pub fn sender(&self) -> &Sender<ReturnedBuffer> {
        &self.sender
    }
}

/// A buffer that can be sent across threads.
///
/// When dropped, sends a [`ReturnedBuffer`] message through the channel so
/// the owning thread can decrement the arena's lease count.
pub struct SendableBuffer {
    ptr: *const u8,
    len: usize,
    epoch: u64,
    arena_idx: usize,
    buf_id: u32,
    sender: Sender<ReturnedBuffer>,
}

// SAFETY: The arena backing this buffer cannot be collected while
// lease_count > 0. The owning thread only decrements lease_count
// after receiving the ReturnedBuffer through the channel, which
// happens after this SendableBuffer is dropped.
unsafe impl Send for SendableBuffer {}

impl SendableBuffer {
    pub(crate) fn new(
        ptr: *const u8,
        len: usize,
        epoch: u64,
        arena_idx: usize,
        buf_id: u32,
        sender: Sender<ReturnedBuffer>,
    ) -> Self {
        Self { ptr, len, epoch, arena_idx, buf_id, sender }
    }

    /// Read the buffer contents.
    ///
    /// # Safety
    /// The arena memory must still be valid (guaranteed by lease_count invariant).
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl Drop for SendableBuffer {
    fn drop(&mut self) {
        let _ = self.sender.send(ReturnedBuffer {
            epoch: self.epoch,
            arena_idx: self.arena_idx,
            buf_id: self.buf_id,
        });
    }
}

// Compile-time assertions for trait bounds.
fn _assert_sendable_buffer_is_send<T: Send>() {}
fn _assert_transfer_handle_is_send<T: Send + Sync>() {}
#[allow(dead_code)]
const _: () = {
    let _ = _assert_sendable_buffer_is_send::<SendableBuffer>;
    let _ = _assert_transfer_handle_is_send::<TransferHandle>;
};

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn sendable_buffer_drop_sends_returned_buffer() {
        let (tx, rx) = unbounded();
        let data = [1u8, 2, 3, 4];

        {
            let _buf = SendableBuffer::new(
                data.as_ptr(),
                data.len(),
                42,
                7,
                0,
                tx,
            );
            // buf is dropped here
        }

        let returned = rx.try_recv().expect("should receive ReturnedBuffer on drop");
        assert_eq!(returned.epoch, 42);
        assert_eq!(returned.arena_idx, 7);
    }

    #[test]
    fn transfer_handle_clone_works() {
        let (tx, _rx) = unbounded();
        let handle = TransferHandle::new(tx);
        let handle2 = handle.clone();

        // Both handles should reference the same channel.
        let data = [0u8; 1];
        let _buf = SendableBuffer::new(data.as_ptr(), 1, 1, 0, 0, handle.sender().clone());
        let _buf2 = SendableBuffer::new(data.as_ptr(), 1, 2, 1, 0, handle2.sender().clone());

        drop(_buf);
        drop(_buf2);

        let r1 = _rx.try_recv().unwrap();
        let r2 = _rx.try_recv().unwrap();
        assert_eq!(r1.epoch, 1);
        assert_eq!(r2.epoch, 2);
    }

    #[test]
    fn returned_buffer_fields_correct() {
        let rb = ReturnedBuffer {
            epoch: 99,
            arena_idx: 3,
            buf_id: 5,
        };
        assert_eq!(rb.epoch, 99);
        assert_eq!(rb.arena_idx, 3);

        // Clone and Copy work.
        let rb2 = rb;
        let rb3 = rb;
        assert_eq!(rb2.epoch, rb3.epoch);
        assert_eq!(rb2.arena_idx, rb3.arena_idx);
    }
}
