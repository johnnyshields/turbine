use bumpalo::Bump;

pub struct BumpaloPool {
    bump: Bump,
}

impl BumpaloPool {
    pub fn new(capacity: usize) -> Self {
        let bump = Bump::with_capacity(capacity);
        Self { bump }
    }

    /// Allocate a zeroed slice from the bump allocator.
    /// Returns a raw pointer to avoid lifetime issues in benchmarks.
    pub fn lease(&self, len: usize) -> *mut u8 {
        let slice = self.bump.alloc_slice_fill_default(len);
        slice.as_mut_ptr()
    }

    /// Reset the bump allocator, reclaiming all memory.
    pub fn reset(&mut self) {
        self.bump.reset();
    }
}
