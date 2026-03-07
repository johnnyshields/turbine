use std::collections::HashMap;

use crate::epoch::arena::Arena;
use crate::error::{Result, TurbineError};
use crate::{ArenaIdx, SlotId};

/// Bitmap-based slot allocator for io_uring buffer registration slots.
struct SlotAllocator {
    slots: Vec<bool>, // true = allocated
}

impl SlotAllocator {
    fn new(capacity: usize) -> Self {
        Self {
            slots: vec![false; capacity],
        }
    }

    fn alloc(&mut self) -> Option<SlotId> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if !*slot {
                *slot = true;
                return Some(SlotId::new(i as u16));
            }
        }
        None
    }

    fn free(&mut self, slot: SlotId) {
        let idx = slot.as_u16() as usize;
        debug_assert!(idx < self.slots.len(), "slot index out of bounds");
        debug_assert!(self.slots[idx], "freeing unallocated slot");
        if idx < self.slots.len() {
            self.slots[idx] = false;
        }
    }

    fn capacity(&self) -> usize {
        self.slots.len()
    }
}

/// Tracks io_uring buffer registration with dynamic slot management.
pub struct RingRegistration {
    registered: bool,
    slots: SlotAllocator,
    arena_slot_map: HashMap<ArenaIdx, SlotId>, // slab_idx → io_uring slot
    generation: u64,
}

impl RingRegistration {
    pub fn new(registration_slots: usize) -> Self {
        Self {
            registered: false,
            slots: SlotAllocator::new(registration_slots),
            arena_slot_map: HashMap::new(),
            generation: 0,
        }
    }

    /// Register arenas as fixed buffers with the io_uring submitter.
    /// Allocates slots for each arena and builds the iovec array.
    pub fn register<'a>(
        &mut self,
        submitter: &io_uring::Submitter<'_>,
        arenas: impl Iterator<Item = (ArenaIdx, &'a Arena)>,
    ) -> Result<()> {
        // Build iovec array with capacity for all slots (empty slots get zeroed iovecs)
        let mut iovecs: Vec<libc::iovec> = vec![
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            };
            self.slots.capacity()
        ];

        for (slab_idx, arena) in arenas {
            let slot = self
                .slots
                .alloc()
                .ok_or(TurbineError::NoRegistrationSlot(slab_idx))?;
            self.arena_slot_map.insert(slab_idx, slot);
            iovecs[slot.as_u16() as usize] = arena.as_iovec();
        }

        unsafe {
            submitter
                .register_buffers(&iovecs)
                .map_err(TurbineError::Registration)?;
        }
        self.registered = true;
        self.generation += 1;
        tracing::info!(
            slots = self.slots.capacity(),
            "registered io_uring fixed buffers"
        );
        Ok(())
    }

    /// Unregister previously registered buffers.
    pub fn unregister(&mut self, submitter: &io_uring::Submitter<'_>) -> Result<()> {
        if self.registered {
            submitter
                .unregister_buffers()
                .map_err(TurbineError::Registration)?;
            self.registered = false;
            self.generation += 1;
            tracing::info!("unregistered io_uring fixed buffers");
        }
        Ok(())
    }

    /// Track a new arena in the slot map (for dynamic growth).
    /// Returns the allocated slot ID.
    pub fn register_arena(&mut self, slab_idx: ArenaIdx) -> Result<SlotId> {
        let slot = self
            .slots
            .alloc()
            .ok_or(TurbineError::NoRegistrationSlot(slab_idx))?;
        self.arena_slot_map.insert(slab_idx, slot);
        self.generation += 1;
        Ok(slot)
    }

    /// Remove an arena from the slot map.
    pub fn unregister_arena(&mut self, slab_idx: ArenaIdx) {
        if let Some(slot) = self.arena_slot_map.remove(&slab_idx) {
            self.slots.free(slot);
            self.generation += 1;
        }
    }

    /// Look up the io_uring slot for a given arena slab index.
    pub fn slot_for_arena(&self, slab_idx: ArenaIdx) -> Option<SlotId> {
        self.arena_slot_map.get(&slab_idx).copied()
    }

    pub fn is_registered(&self) -> bool {
        self.registered
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Default for RingRegistration {
    fn default() -> Self {
        Self::new(32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_not_registered() {
        let reg = RingRegistration::new(16);
        assert!(!reg.is_registered());
        assert_eq!(reg.generation(), 0);
    }

    #[test]
    fn default_trait_matches_new() {
        let reg = RingRegistration::default();
        assert!(!reg.is_registered());
    }

    #[test]
    fn slot_allocator_sequential() {
        let mut alloc = SlotAllocator::new(4);
        assert_eq!(alloc.alloc(), Some(SlotId::new(0)));
        assert_eq!(alloc.alloc(), Some(SlotId::new(1)));
        assert_eq!(alloc.alloc(), Some(SlotId::new(2)));
        assert_eq!(alloc.alloc(), Some(SlotId::new(3)));
        assert_eq!(alloc.alloc(), None);
    }

    #[test]
    fn slot_allocator_reuse_after_free() {
        let mut alloc = SlotAllocator::new(4);
        let s0 = alloc.alloc().unwrap();
        let _s1 = alloc.alloc().unwrap();
        alloc.free(s0);
        // Should reuse slot 0
        assert_eq!(alloc.alloc(), Some(SlotId::new(0)));
    }

    #[test]
    fn register_arena_tracking() {
        let mut reg = RingRegistration::new(8);

        let slot = reg.register_arena(ArenaIdx::new(0)).unwrap();
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(0)), Some(slot));
        assert_eq!(reg.generation(), 1);

        let slot2 = reg.register_arena(ArenaIdx::new(1)).unwrap();
        assert_ne!(slot, slot2);
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(1)), Some(slot2));
        assert_eq!(reg.generation(), 2);
    }

    #[test]
    fn unregister_arena_frees_slot() {
        let mut reg = RingRegistration::new(2);

        let _s0 = reg.register_arena(ArenaIdx::new(0)).unwrap();
        let _s1 = reg.register_arena(ArenaIdx::new(1)).unwrap();

        // All slots full
        assert!(reg.register_arena(ArenaIdx::new(2)).is_err());

        // Free one
        reg.unregister_arena(ArenaIdx::new(0));
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(0)), None);

        // Can allocate again
        let s2 = reg.register_arena(ArenaIdx::new(2)).unwrap();
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(2)), Some(s2));
    }

    #[test]
    fn slot_for_unknown_arena_returns_none() {
        let reg = RingRegistration::new(8);
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(99)), None);
    }
}
