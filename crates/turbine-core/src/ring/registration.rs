use crate::epoch::arena::Arena;
use crate::error::{Result, TurbineError};
use crate::{ArenaIdx, SlotId};

/// Bitmap-based slot allocator for io_uring buffer registration slots.
struct SlotAllocator {
    bitmap: u64,
    capacity: usize,
}

impl SlotAllocator {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "SlotAllocator requires at least 1 slot");
        assert!(capacity <= 64, "SlotAllocator supports at most 64 slots");
        Self {
            bitmap: 0,
            capacity,
        }
    }

    fn alloc(&mut self) -> Option<SlotId> {
        let free = !self.bitmap;
        if free == 0 {
            return None;
        }
        let bit = free.trailing_zeros() as usize;
        if bit >= self.capacity {
            return None;
        }
        self.bitmap |= 1 << bit;
        Some(SlotId::new(bit as u16))
    }

    fn free(&mut self, slot: SlotId) {
        let bit = slot.as_u16() as usize;
        debug_assert!(bit < self.capacity, "slot index out of bounds");
        debug_assert!(self.bitmap & (1 << bit) != 0, "freeing unallocated slot");
        self.bitmap &= !(1 << bit);
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Tracks io_uring buffer registration with dynamic slot management.
pub struct RingRegistration {
    registered: bool,
    slots: SlotAllocator,
    arena_slot_map: Vec<Option<SlotId>>, // slab_idx → io_uring slot
    generation: u64,
}

impl RingRegistration {
    pub fn new(registration_slots: usize) -> Self {
        Self {
            registered: false,
            slots: SlotAllocator::new(registration_slots),
            arena_slot_map: Vec::new(),
            generation: 0,
        }
    }

    fn ensure_arena_capacity(&mut self, idx: ArenaIdx) {
        let needed = idx.as_usize() + 1;
        if self.arena_slot_map.len() < needed {
            self.arena_slot_map.resize(needed, None);
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
            self.ensure_arena_capacity(slab_idx);
            self.arena_slot_map[slab_idx.as_usize()] = Some(slot);
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
    ///
    /// Frees all allocated slots and clears the arena-to-slot mapping so that
    /// a subsequent `register()` call starts fresh without leaking slots.
    pub fn unregister(&mut self, submitter: &io_uring::Submitter<'_>) -> Result<()> {
        if self.registered {
            submitter
                .unregister_buffers()
                .map_err(TurbineError::Registration)?;
            // Free all allocated slots so they can be reused on next register().
            for slot_opt in self.arena_slot_map.iter_mut() {
                if let Some(slot) = slot_opt.take() {
                    self.slots.free(slot);
                }
            }
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
        self.ensure_arena_capacity(slab_idx);
        self.arena_slot_map[slab_idx.as_usize()] = Some(slot);
        self.generation += 1;
        Ok(slot)
    }

    /// Remove an arena from the slot map.
    pub fn unregister_arena(&mut self, slab_idx: ArenaIdx) {
        if let Some(slot) = self
            .arena_slot_map
            .get(slab_idx.as_usize())
            .copied()
            .flatten()
        {
            self.arena_slot_map[slab_idx.as_usize()] = None;
            self.slots.free(slot);
            self.generation += 1;
        }
    }

    /// Look up the io_uring slot for a given arena slab index.
    #[inline(always)]
    pub fn slot_for_arena(&self, slab_idx: ArenaIdx) -> Option<SlotId> {
        self.arena_slot_map.get(slab_idx.as_usize()).copied().flatten()
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

    #[test]
    fn slot_allocator_capacity_one() {
        let mut alloc = SlotAllocator::new(1);
        assert_eq!(alloc.alloc(), Some(SlotId::new(0)));
        assert_eq!(alloc.alloc(), None);
        alloc.free(SlotId::new(0));
        assert_eq!(alloc.alloc(), Some(SlotId::new(0)));
    }

    #[test]
    fn slot_allocator_capacity_64() {
        let mut alloc = SlotAllocator::new(64);
        let mut slots = Vec::new();
        for i in 0..64 {
            let s = alloc.alloc().unwrap();
            assert_eq!(s.as_u16(), i as u16);
            slots.push(s);
        }
        assert_eq!(alloc.alloc(), None);

        // Free all and reallocate
        for s in &slots {
            alloc.free(*s);
        }
        for i in 0..64 {
            let s = alloc.alloc().unwrap();
            assert_eq!(s.as_u16(), i as u16);
        }
        assert_eq!(alloc.alloc(), None);
    }

    #[test]
    #[should_panic(expected = "SlotAllocator requires at least 1 slot")]
    fn slot_allocator_capacity_zero_panics() {
        SlotAllocator::new(0);
    }

    #[test]
    fn unregister_frees_slots_for_reuse() {
        // Simulate the unregister→register cycle without a real io_uring submitter
        // by directly testing that slot allocations are freed when arena_slot_map is cleared.
        let mut reg = RingRegistration::new(4);

        // Allocate all 4 slots.
        for i in 0..4 {
            reg.register_arena(ArenaIdx::new(i)).unwrap();
        }
        assert!(reg.register_arena(ArenaIdx::new(4)).is_err(), "slots should be full");

        // Simulate unregister: free all slots and clear the map.
        for slot_opt in reg.arena_slot_map.iter_mut() {
            if let Some(slot) = slot_opt.take() {
                reg.slots.free(slot);
            }
        }
        reg.registered = false;

        // Now re-register — should succeed because slots were freed.
        for i in 0..4 {
            reg.register_arena(ArenaIdx::new(i)).unwrap();
        }
        assert!(reg.register_arena(ArenaIdx::new(4)).is_err(), "slots should be full again");

        // Repeat the cycle 10 times to prove no leak.
        for _ in 0..10 {
            for slot_opt in reg.arena_slot_map.iter_mut() {
                if let Some(slot) = slot_opt.take() {
                    reg.slots.free(slot);
                }
            }
            for i in 0..4 {
                reg.register_arena(ArenaIdx::new(i)).unwrap();
            }
        }
    }

    #[test]
    fn arena_slot_map_sparse_indices() {
        let mut reg = RingRegistration::new(8);

        let s0 = reg.register_arena(ArenaIdx::new(0)).unwrap();
        let s50 = reg.register_arena(ArenaIdx::new(50)).unwrap();
        let s99 = reg.register_arena(ArenaIdx::new(99)).unwrap();

        assert_eq!(reg.slot_for_arena(ArenaIdx::new(0)), Some(s0));
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(50)), Some(s50));
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(99)), Some(s99));

        // Gaps should be None
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(25)), None);
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(75)), None);

        // Vec grew to accommodate index 99
        assert!(reg.arena_slot_map.len() >= 100);

        // Unregister middle, verify others unchanged
        reg.unregister_arena(ArenaIdx::new(50));
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(50)), None);
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(0)), Some(s0));
        assert_eq!(reg.slot_for_arena(ArenaIdx::new(99)), Some(s99));
    }
}
