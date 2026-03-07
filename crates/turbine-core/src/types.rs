/// Type-safe wrapper for arena slab indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArenaIdx(usize);

impl ArenaIdx {
    pub fn new(idx: usize) -> Self {
        Self(idx)
    }

    pub fn as_usize(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for ArenaIdx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Type-safe wrapper for io_uring registration slot IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(u16);

impl SlotId {
    pub fn new(id: u16) -> Self {
        Self(id)
    }

    pub fn as_u16(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for SlotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
