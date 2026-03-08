/// Type-safe wrapper for arena slab indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArenaIdx(usize);

impl ArenaIdx {
    #[inline]
    pub fn new(idx: usize) -> Self {
        Self(idx)
    }

    #[inline]
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
    #[inline]
    pub fn new(id: u16) -> Self {
        Self(id)
    }

    #[inline]
    pub fn as_u16(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for SlotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn arena_idx_new_and_as_usize() {
        let idx = ArenaIdx::new(42);
        assert_eq!(idx.as_usize(), 42);
    }

    #[test]
    fn arena_idx_display() {
        let idx = ArenaIdx::new(7);
        assert_eq!(format!("{idx}"), "7");
    }

    #[test]
    fn arena_idx_debug() {
        let idx = ArenaIdx::new(3);
        assert_eq!(format!("{idx:?}"), "ArenaIdx(3)");
    }

    #[test]
    fn arena_idx_clone_copy() {
        let idx = ArenaIdx::new(5);
        let cloned = idx.clone();
        let copied = idx;
        assert_eq!(idx, cloned);
        assert_eq!(idx, copied);
    }

    #[test]
    fn arena_idx_eq_and_hash() {
        let a = ArenaIdx::new(1);
        let b = ArenaIdx::new(1);
        let c = ArenaIdx::new(2);
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn slot_id_new_and_as_u16() {
        let id = SlotId::new(100);
        assert_eq!(id.as_u16(), 100);
    }

    #[test]
    fn slot_id_display() {
        let id = SlotId::new(15);
        assert_eq!(format!("{id}"), "15");
    }

    #[test]
    fn slot_id_debug() {
        let id = SlotId::new(9);
        assert_eq!(format!("{id:?}"), "SlotId(9)");
    }

    #[test]
    fn slot_id_clone_copy() {
        let id = SlotId::new(8);
        let cloned = id.clone();
        let copied = id;
        assert_eq!(id, cloned);
        assert_eq!(id, copied);
    }

    #[test]
    fn slot_id_eq_and_hash() {
        let a = SlotId::new(10);
        let b = SlotId::new(10);
        let c = SlotId::new(20);
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn arena_idx_zero() {
        let idx = ArenaIdx::new(0);
        assert_eq!(idx.as_usize(), 0);
        assert_eq!(format!("{idx}"), "0");
    }

    #[test]
    fn slot_id_zero() {
        let id = SlotId::new(0);
        assert_eq!(id.as_u16(), 0);
        assert_eq!(format!("{id}"), "0");
    }

    #[test]
    fn slot_id_max() {
        let id = SlotId::new(u16::MAX);
        assert_eq!(id.as_u16(), u16::MAX);
    }
}
