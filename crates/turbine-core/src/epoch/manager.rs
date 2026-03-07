use crate::config::PoolConfig;
use crate::epoch::arena::{Arena, ArenaState};
use crate::error::{Result, TurbineError};

/// Result of a rotate operation.
#[derive(Debug)]
pub struct RotateResult {
    pub retired_epoch: u64,
    pub new_epoch: u64,
    /// Some(slab_idx) if a fresh arena was allocated (needs io_uring registration).
    pub new_arena_idx: Option<usize>,
}

/// Slab-based arena manager with drain queue and free pool.
///
/// Replaces the fixed-size ring in EpochClock. Rotation never blocks on
/// outstanding leases — retired arenas go to a drain queue and are only
/// recycled after all leases return.
pub struct ArenaManager {
    arenas: Vec<Option<Box<Arena>>>, // slab, stable addresses via Box
    write_idx: usize,
    draining: Vec<usize>,  // slab indices, oldest first
    free_pool: Vec<usize>, // slab indices ready for reuse
    epoch: u64,
    arena_size: usize,
    page_size: usize,
    max_free: usize,
    max_arenas: usize, // 0 = unlimited
}

impl ArenaManager {
    /// Create a new arena manager from the given config.
    ///
    /// Allocates `initial_arenas` arenas. The first becomes the writable arena
    /// at epoch 0; the rest go into the free pool in Collected state.
    pub fn new(config: &PoolConfig) -> Result<Self> {
        config.validate()?;

        let mut arenas: Vec<Option<Box<Arena>>> =
            Vec::with_capacity(config.initial_arenas);
        let mut free_pool = Vec::with_capacity(config.initial_arenas.saturating_sub(1));

        for i in 0..config.initial_arenas {
            let arena = Box::new(Arena::new(config.arena_size)?);
            if i == 0 {
                arena.reset();
                arena.set_epoch(0);
            }
            // Remaining arenas stay in Collected state (default from Arena::new)
            arenas.push(Some(arena));
            if i > 0 {
                free_pool.push(i);
            }
        }

        Ok(Self {
            arenas,
            write_idx: 0,
            draining: Vec::new(),
            free_pool,
            epoch: 0,
            arena_size: config.arena_size,
            page_size: config.page_size,
            max_free: config.max_free_arenas,
            max_arenas: config.max_total_arenas,
        })
    }

    /// Rotate the current epoch. The current arena is retired and a new one
    /// becomes writable.
    ///
    /// Unlike `EpochClock::rotate`, this never fails due to outstanding leases.
    /// Retired arenas are moved to the drain queue and recycled later.
    pub fn rotate(&mut self) -> Result<RotateResult> {
        let retired_epoch = self.epoch;
        let new_epoch = retired_epoch + 1;

        // Retire current arena.
        let current = self.arenas[self.write_idx]
            .as_ref()
            .expect("write arena missing");
        current.set_state(ArenaState::Retired);
        current.advise_free_unused(self.page_size);
        self.draining.push(self.write_idx);

        // Get next arena from free pool or allocate.
        let (next_idx, new_arena_idx) = if let Some(idx) = self.free_pool.pop() {
            (idx, None)
        } else {
            let idx = self.alloc_arena()?;
            (idx, Some(idx))
        };

        // Prepare the new writable arena.
        let next = self.arenas[next_idx]
            .as_ref()
            .expect("free/new arena missing");
        next.reset();
        next.set_epoch(new_epoch);

        self.write_idx = next_idx;
        self.epoch = new_epoch;

        Ok(RotateResult {
            retired_epoch,
            new_epoch,
            new_arena_idx,
        })
    }

    /// Collect arenas from the drain queue whose lease count has reached zero.
    ///
    /// Collected arenas are moved to the free pool for reuse. Returns the
    /// number of arenas collected.
    pub fn collect(&mut self) -> usize {
        let mut collected = 0;
        let mut still_draining = Vec::new();

        for idx in self.draining.drain(..) {
            let arena = self.arenas[idx]
                .as_ref()
                .expect("draining arena missing");
            if arena.lease_count() == 0 {
                arena.set_state(ArenaState::Collected);
                self.free_pool.push(idx);
                collected += 1;
            } else {
                still_draining.push(idx);
            }
        }

        self.draining = still_draining;
        collected
    }

    /// Shrink the free pool down to `max_free_arenas`, dropping excess arenas
    /// (which triggers munmap via `Drop`).
    ///
    /// Returns the number of arenas removed.
    pub fn shrink(&mut self) -> usize {
        let mut removed = 0;
        while self.free_pool.len() > self.max_free {
            let idx = self.free_pool.pop().unwrap();
            self.arenas[idx] = None;
            removed += 1;
        }
        removed
    }

    /// Reference to the current writable arena.
    pub fn current_arena(&self) -> &Arena {
        self.arenas[self.write_idx]
            .as_ref()
            .expect("write arena missing")
    }

    /// Slab index of the current writable arena.
    pub fn current_arena_idx(&self) -> usize {
        self.write_idx
    }

    /// Look up an arena by slab index.
    pub fn arena_at(&self, idx: usize) -> Option<&Arena> {
        self.arenas.get(idx).and_then(|slot| slot.as_ref().map(|b| &**b))
    }

    /// Current epoch number.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Iterate all live (non-None) arenas with their slab indices.
    pub fn live_arenas(&self) -> impl Iterator<Item = (usize, &Arena)> {
        self.arenas
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|b| (i, &**b)))
    }

    /// Number of arenas in the drain queue.
    pub fn draining_count(&self) -> usize {
        self.draining.len()
    }

    /// Number of arenas in the free pool.
    pub fn free_count(&self) -> usize {
        self.free_pool.len()
    }

    /// Allocate a new arena and insert it into the slab.
    ///
    /// Returns the slab index. Fails if `max_total_arenas` would be exceeded.
    fn alloc_arena(&mut self) -> Result<usize> {
        if self.max_arenas > 0 {
            let current = self.arenas.iter().filter(|s| s.is_some()).count();
            if current >= self.max_arenas {
                return Err(TurbineError::ArenaLimitExceeded {
                    current,
                    max: self.max_arenas,
                });
            }
        }

        let arena = Box::new(Arena::new(self.arena_size)?);

        // Find first None slot or push.
        if let Some(idx) = self.arenas.iter().position(|s| s.is_none()) {
            self.arenas[idx] = Some(arena);
            Ok(idx)
        } else {
            let idx = self.arenas.len();
            self.arenas.push(Some(arena));
            Ok(idx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PoolConfig {
        PoolConfig {
            arena_size: 4096,
            initial_arenas: 3,
            max_free_arenas: 4,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        }
    }

    #[test]
    fn new_creates_initial_arenas() {
        let mgr = ArenaManager::new(&test_config()).unwrap();
        assert_eq!(mgr.epoch(), 0);
        assert_eq!(mgr.current_arena_idx(), 0);
        assert_eq!(mgr.current_arena().state(), ArenaState::Writable);
        assert_eq!(mgr.current_arena().epoch(), 0);
        assert_eq!(mgr.free_count(), 2); // 3 initial - 1 writable
        assert_eq!(mgr.draining_count(), 0);
        assert_eq!(mgr.live_arenas().count(), 3);
    }

    #[test]
    fn rotate_retires_and_activates() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();
        let old_idx = mgr.current_arena_idx();

        let result = mgr.rotate().unwrap();
        assert_eq!(result.retired_epoch, 0);
        assert_eq!(result.new_epoch, 1);
        assert_eq!(result.new_arena_idx, None); // reused from free pool

        // Old arena is retired and draining.
        assert_eq!(mgr.arena_at(old_idx).unwrap().state(), ArenaState::Retired);
        assert_eq!(mgr.draining_count(), 1);

        // New arena is writable.
        assert_eq!(mgr.current_arena().state(), ArenaState::Writable);
        assert_eq!(mgr.current_arena().epoch(), 1);
        assert_eq!(mgr.epoch(), 1);
        assert_eq!(mgr.free_count(), 1); // started with 2, used 1
    }

    #[test]
    fn rotate_with_outstanding_leases_succeeds() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();

        // Acquire leases on the current arena.
        mgr.current_arena().acquire_lease();
        mgr.current_arena().acquire_lease();

        // Rotate should succeed even with outstanding leases.
        let result = mgr.rotate().unwrap();
        assert_eq!(result.retired_epoch, 0);
        assert_eq!(result.new_epoch, 1);

        // Old arena is draining with leases still held.
        let old = mgr.arena_at(0).unwrap();
        assert_eq!(old.state(), ArenaState::Retired);
        assert_eq!(old.lease_count(), 2);

        // Clean up leases to avoid debug_assert on drop.
        old.release_lease();
        old.release_lease();
    }

    #[test]
    fn rotate_reuses_free_pool() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();

        // Rotate to use up free arenas, then collect to recycle.
        mgr.rotate().unwrap(); // epoch 1, free=1
        mgr.rotate().unwrap(); // epoch 2, free=0

        // Collect all draining (no leases held).
        let collected = mgr.collect();
        assert_eq!(collected, 2);
        assert_eq!(mgr.free_count(), 2);
        assert_eq!(mgr.draining_count(), 0);

        // Next rotate should reuse from free pool.
        let result = mgr.rotate().unwrap();
        assert_eq!(result.new_arena_idx, None); // reused, not new
    }

    #[test]
    fn rotate_allocs_new_when_free_empty() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 1,
            max_free_arenas: 4,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();
        assert_eq!(mgr.free_count(), 0);

        let result = mgr.rotate().unwrap();
        assert!(result.new_arena_idx.is_some()); // had to allocate
        assert_eq!(mgr.live_arenas().count(), 2);
    }

    #[test]
    fn rotate_fails_at_max_arenas() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 2,
            max_free_arenas: 4,
            max_total_arenas: 2,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();

        // Use the one free arena.
        mgr.rotate().unwrap();
        assert_eq!(mgr.free_count(), 0);

        // Hold leases so collect won't free anything.
        mgr.arena_at(0).unwrap().acquire_lease();

        // Next rotate: no free arenas, at max_arenas limit → error.
        let err = mgr.rotate().unwrap_err();
        assert!(
            matches!(err, TurbineError::ArenaLimitExceeded { current: 2, max: 2 }),
            "expected ArenaLimitExceeded, got: {err:?}"
        );

        // Clean up lease to avoid debug_assert on drop.
        mgr.arena_at(0).unwrap().release_lease();
    }

    #[test]
    fn collect_moves_zero_lease_to_free() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();
        mgr.rotate().unwrap();

        assert_eq!(mgr.draining_count(), 1);
        let collected = mgr.collect();
        assert_eq!(collected, 1);
        assert_eq!(mgr.draining_count(), 0);
        // free_pool: 1 remaining from initial + 1 just collected = 2
        assert_eq!(mgr.free_count(), 2);
    }

    #[test]
    fn collect_retains_leased_arenas() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();
        mgr.current_arena().acquire_lease();
        mgr.rotate().unwrap();

        // Arena 0 has a lease, should not be collected.
        let collected = mgr.collect();
        assert_eq!(collected, 0);
        assert_eq!(mgr.draining_count(), 1);

        // Release the lease, now it should collect.
        mgr.arena_at(0).unwrap().release_lease();
        let collected = mgr.collect();
        assert_eq!(collected, 1);
        assert_eq!(mgr.draining_count(), 0);
    }

    #[test]
    fn shrink_removes_excess_free() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 1,
            max_free_arenas: 1,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();

        // Rotate several times, then collect to fill free pool.
        for _ in 0..4 {
            mgr.rotate().unwrap();
        }
        mgr.collect();
        let free_before = mgr.free_count();
        assert!(free_before > 1, "need excess free arenas for test");

        let removed = mgr.shrink();
        assert_eq!(removed, free_before - 1); // shrink to max_free=1
        assert_eq!(mgr.free_count(), 1);
    }

    #[test]
    fn pointer_stability() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();

        // Allocate from the first arena and get a pointer.
        let arena_ptr = mgr.current_arena() as *const Arena;
        mgr.current_arena().acquire_lease();
        let (data_ptr, _) = mgr.current_arena().alloc(64).unwrap();

        // Rotate many times, growing the slab.
        for _ in 0..20 {
            mgr.rotate().unwrap();
        }

        // Original arena is still at the same address (Box ensures stability).
        let arena_ref = mgr.arena_at(0).unwrap();
        assert_eq!(arena_ref as *const Arena, arena_ptr);

        // Data pointer is still valid (arena memory hasn't moved).
        unsafe {
            std::ptr::write_volatile(data_ptr, 42);
            assert_eq!(std::ptr::read_volatile(data_ptr), 42);
        }

        arena_ref.release_lease();
    }

    #[test]
    fn full_lifecycle() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 2,
            max_free_arenas: 1,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();

        // Rotate: retire epoch 0.
        let r = mgr.rotate().unwrap();
        assert_eq!(r.retired_epoch, 0);
        assert_eq!(r.new_epoch, 1);

        // Rotate again: free pool empty → alloc new.
        let r = mgr.rotate().unwrap();
        assert_eq!(r.retired_epoch, 1);
        assert!(r.new_arena_idx.is_some());

        // Collect: both retired arenas have no leases.
        let collected = mgr.collect();
        assert_eq!(collected, 2);
        assert_eq!(mgr.free_count(), 2);

        // Shrink: max_free=1, so one should be dropped.
        let removed = mgr.shrink();
        assert_eq!(removed, 1);
        assert_eq!(mgr.free_count(), 1);

        // Rotate reuses from free pool.
        let r = mgr.rotate().unwrap();
        assert_eq!(r.new_arena_idx, None);
    }
}
