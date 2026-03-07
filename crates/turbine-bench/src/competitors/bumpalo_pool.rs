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
    pub fn lease(&self, len: usize) -> &mut [u8] {
        self.bump.alloc_slice_fill_default(len)
    }

    /// Reset the bump allocator, reclaiming all memory.
    pub fn reset(&mut self) {
        self.bump.reset();
    }
}
