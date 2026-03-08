use crate::config::PoolConfig;
use crate::epoch::arena::{Arena, ArenaState};
use crate::error::{Result, TurbineError};
use crate::ArenaIdx;

/// If a draining arena retired within this many epochs of the current epoch
/// still has outstanding leases, skip checking younger arenas — they almost
/// certainly have leases too. Skipped arenas are rechecked on the next collect().
const COLLECT_YOUNG_EPOCHS: u64 = 2;

/// Result of a rotate operation.
#[derive(Debug)]
pub struct RotateResult {
    pub retired_epoch: u64,
    pub new_epoch: u64,
    /// Some(slab_idx) if a fresh arena was allocated (needs io_uring registration).
    pub new_arena_idx: Option<ArenaIdx>,
}

/// Slab-based arena manager with drain queue and free pool.
///
/// Replaces the fixed-size ring in EpochClock. Rotation never blocks on
/// outstanding leases — retired arenas go to a drain queue and are only
/// recycled after all leases return.
pub struct ArenaManager {
    arenas: Vec<Option<Box<Arena>>>, // slab, stable addresses via Box
    write_idx: ArenaIdx,
    draining: Vec<ArenaIdx>,  // slab indices, oldest first
    free_pool: Vec<ArenaIdx>, // slab indices ready for reuse
    live_count: usize,        // O(1) count of non-None slab entries
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
                free_pool.push(ArenaIdx::new(i));
            }
        }

        Ok(Self {
            arenas,
            write_idx: ArenaIdx::new(0),
            draining: Vec::new(),
            free_pool,
            live_count: config.initial_arenas,
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

        // Secure a next arena BEFORE retiring current — if this fails,
        // the current arena stays writable and no state is mutated.
        let (next_idx, new_arena_idx) = if let Some(idx) = self.free_pool.pop() {
            (idx, None)
        } else {
            // Try collecting draining arenas first to avoid unnecessary growth.
            self.collect();
            if let Some(idx) = self.free_pool.pop() {
                (idx, None)
            } else {
                let idx = self.alloc_arena()?;
                (idx, Some(idx))
            }
        };

        // Now that we have a next arena, retire the current one.
        let current = self.arenas[self.write_idx.as_usize()]
            .as_ref()
            .expect("write arena missing");
        current.set_state(ArenaState::Retired);
        self.draining.push(self.write_idx);

        // Prepare the new writable arena.
        let next = self.arenas[next_idx.as_usize()]
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
        let current_epoch = self.epoch;
        let arenas = &self.arenas;
        let free_pool = &mut self.free_pool;
        let page_size = self.page_size;
        let mut skip_remaining = false;
        self.draining.retain(|&idx| {
            if skip_remaining {
                return true;
            }
            let arena = arenas[idx.as_usize()].as_ref().expect("draining arena missing");
            // Fast path: skip Acquire barrier when leases are clearly outstanding
            if arena.has_outstanding_leases() {
                // If this arena is young (recently retired) and still has leases,
                // younger arenas almost certainly do too — skip the rest.
                if arena.epoch() >= current_epoch.saturating_sub(COLLECT_YOUNG_EPOCHS) {
                    skip_remaining = true;
                }
                return true; // keep in draining
            }
            // Slow path: Acquire-ordered check for definitive answer
            if arena.lease_count() == 0 {
                arena.advise_free_unused(page_size);
                arena.set_state(ArenaState::Collected);
                free_pool.push(idx);
                collected += 1;
                false
            } else {
                // Lease count > 0 on slow path — also check for early termination.
                if arena.epoch() >= current_epoch.saturating_sub(COLLECT_YOUNG_EPOCHS) {
                    skip_remaining = true;
                }
                true
            }
        });
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
            self.arenas[idx.as_usize()] = None;
            self.live_count -= 1;
            removed += 1;
        }
        removed
    }

    /// Returns a reference to the arena at `write_idx` without bounds or
    /// Option checks.
    ///
    /// # Safety justification
    /// `write_idx` always points to a valid, `Some` slab entry. This invariant
    /// is maintained by `new()` (sets `write_idx` to the first arena) and
    /// `rotate()` (secures the next arena before updating `write_idx`).
    #[inline(always)]
    fn write_arena(&self) -> &Arena {
        // SAFETY: write_idx is always a valid index into a Some slot.
        // See safety justification above.
        unsafe {
            self.arenas
                .get_unchecked(self.write_idx.as_usize())
                .as_ref()
                .unwrap_unchecked()
        }
    }

    /// Reference to the current writable arena.
    #[inline(always)]
    pub fn current_arena(&self) -> &Arena {
        self.write_arena()
    }

    /// Slab index of the current writable arena.
    #[inline(always)]
    pub fn current_arena_idx(&self) -> ArenaIdx {
        self.write_idx
    }

    /// Reference to the current writable arena and its slab index in one call.
    ///
    /// Avoids the overhead of calling `current_arena()` and
    /// `current_arena_idx()` separately in the hot lease path.
    #[inline(always)]
    pub fn current_arena_with_idx(&self) -> (&Arena, ArenaIdx) {
        (self.write_arena(), self.write_idx)
    }

    /// Look up an arena by slab index.
    #[inline]
    pub fn arena_at(&self, idx: ArenaIdx) -> Option<&Arena> {
        self.arenas.get(idx.as_usize()).and_then(|slot| slot.as_ref().map(|b| &**b))
    }

    /// Current epoch number.
    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Iterate all live (non-None) arenas with their slab indices.
    pub fn live_arenas(&self) -> impl Iterator<Item = (ArenaIdx, &Arena)> {
        self.arenas
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|b| (ArenaIdx::new(i), &**b)))
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
    fn alloc_arena(&mut self) -> Result<ArenaIdx> {
        if self.max_arenas > 0 && self.live_count >= self.max_arenas {
            return Err(TurbineError::ArenaLimitExceeded {
                current: self.live_count,
                max: self.max_arenas,
            });
        }

        let arena = Box::new(Arena::new(self.arena_size)?);

        // Find first None slot or push.
        let idx = if let Some(idx) = self.arenas.iter().position(|s| s.is_none()) {
            self.arenas[idx] = Some(arena);
            idx
        } else {
            let idx = self.arenas.len();
            self.arenas.push(Some(arena));
            idx
        };
        self.live_count += 1;
        Ok(ArenaIdx::new(idx))
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
        assert_eq!(mgr.current_arena_idx(), ArenaIdx::new(0));
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
        let old = mgr.arena_at(ArenaIdx::new(0)).unwrap();
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

        // Hold a lease so auto-collect in rotate() can't recycle the retired arena.
        mgr.current_arena().acquire_lease();

        let result = mgr.rotate().unwrap();
        assert!(result.new_arena_idx.is_some()); // had to allocate
        assert_eq!(mgr.live_arenas().count(), 2);

        // Clean up lease.
        mgr.arena_at(ArenaIdx::new(0)).unwrap().release_lease();
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

        // Hold lease on arena 0 before rotating it away.
        mgr.arena_at(ArenaIdx::new(0)).unwrap().acquire_lease();

        // Use the one free arena.
        mgr.rotate().unwrap();
        assert_eq!(mgr.free_count(), 0);

        // Hold lease on arena 1 (current) so auto-collect can't recycle it either.
        mgr.current_arena().acquire_lease();

        // Next rotate: no free arenas, auto-collect finds nothing, at max_arenas limit → error.
        let err = mgr.rotate().unwrap_err();
        assert!(
            matches!(err, TurbineError::ArenaLimitExceeded { current: 2, max: 2 }),
            "expected ArenaLimitExceeded, got: {err:?}"
        );

        // Clean up leases to avoid debug_assert on drop.
        mgr.arena_at(ArenaIdx::new(0)).unwrap().release_lease();
        mgr.arena_at(ArenaIdx::new(1)).unwrap().release_lease();
    }

    #[test]
    fn collect_moves_zero_lease_to_free() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();

        // Hold a lease so auto-collect in rotate() doesn't collect arena 0.
        mgr.current_arena().acquire_lease();
        mgr.rotate().unwrap();

        assert_eq!(mgr.draining_count(), 1);

        // Release the lease, then collect explicitly.
        mgr.arena_at(ArenaIdx::new(0)).unwrap().release_lease();
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
        mgr.arena_at(ArenaIdx::new(0)).unwrap().release_lease();
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

        // Hold leases during rotations so auto-collect can't recycle them.
        let mut leased_indices = Vec::new();
        for _ in 0..4 {
            let idx = mgr.current_arena_idx();
            mgr.current_arena().acquire_lease();
            leased_indices.push(idx);
            mgr.rotate().unwrap();
        }

        // Release all leases, then collect to fill free pool.
        for idx in &leased_indices {
            mgr.arena_at(*idx).unwrap().release_lease();
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
        let arena_ref = mgr.arena_at(ArenaIdx::new(0)).unwrap();
        assert_eq!(arena_ref as *const Arena, arena_ptr);

        // Data pointer is still valid (arena memory hasn't moved).
        unsafe {
            std::ptr::write_volatile(data_ptr, 42);
            assert_eq!(std::ptr::read_volatile(data_ptr), 42);
        }

        arena_ref.release_lease();
    }

    #[test]
    fn collect_partial_draining() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 4,
            max_free_arenas: 8,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();

        // Rotate 3 times, holding leases on odd-epoch arenas.
        // Arena 0 (epoch 0): acquire lease
        mgr.current_arena().acquire_lease();
        mgr.rotate().unwrap(); // epoch 0 → draining

        // Arena 1 (epoch 1): no lease
        mgr.rotate().unwrap(); // epoch 1 → draining

        // Arena 2 (epoch 2): acquire lease
        mgr.current_arena().acquire_lease();
        mgr.rotate().unwrap(); // epoch 2 → draining

        assert_eq!(mgr.draining_count(), 3);

        // Collect should only free the arena with no leases (epoch 1).
        let collected = mgr.collect();
        assert_eq!(collected, 1);
        assert_eq!(mgr.draining_count(), 2); // arenas 0 and 2 still draining

        // Clean up leases.
        mgr.arena_at(ArenaIdx::new(0)).unwrap().release_lease();
        mgr.arena_at(ArenaIdx::new(2)).unwrap().release_lease();

        // Now collect should free both.
        let collected = mgr.collect();
        assert_eq!(collected, 2);
        assert_eq!(mgr.draining_count(), 0);
    }

    #[test]
    fn collect_with_remote_returns() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();

        // Acquire leases on arena 0 (simulating buffers sent cross-thread).
        mgr.current_arena().acquire_lease();
        mgr.current_arena().acquire_lease();
        let arena0_idx = mgr.current_arena_idx();

        mgr.rotate().unwrap(); // arena 0 → draining with 2 leases

        // Acquire a lease on arena 1 too.
        mgr.current_arena().acquire_lease();
        let arena1_idx = mgr.current_arena_idx();

        mgr.rotate().unwrap(); // arena 1 → draining with 1 lease

        assert_eq!(mgr.draining_count(), 2);

        // Simulate cross-thread release of one lease on arena 0.
        // has_outstanding_leases() fast path: local=2, remote=1 → true (still outstanding).
        mgr.arena_at(arena0_idx).unwrap().remote_release();
        let collected = mgr.collect();
        assert_eq!(collected, 0); // both arenas still have outstanding leases
        assert_eq!(mgr.draining_count(), 2);

        // Release remaining lease on arena 0 via remote path.
        // has_outstanding_leases() fast path: local=2, remote=2 → false → enters slow path.
        mgr.arena_at(arena0_idx).unwrap().remote_release();
        let collected = mgr.collect();
        assert_eq!(collected, 1); // arena 0 collected, arena 1 still held
        assert_eq!(mgr.draining_count(), 1);

        // Release arena 1's lease locally.
        mgr.arena_at(arena1_idx).unwrap().release_lease();
        let collected = mgr.collect();
        assert_eq!(collected, 1);
        assert_eq!(mgr.draining_count(), 0);
    }

    #[test]
    fn collect_early_termination_skips_young() {
        // Verify that collect() skips younger arenas once a young arena with
        // outstanding leases triggers the early termination heuristic, and that
        // skipped arenas are still collected on subsequent calls.
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 1,
            max_free_arenas: 16,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();

        // Build up several epochs so we have old and young draining arenas.
        // Hold leases on all of them to prevent collection.
        let mut leased = Vec::new();
        for _ in 0..6 {
            let idx = mgr.current_arena_idx();
            mgr.current_arena().acquire_lease();
            leased.push(idx);
            mgr.rotate().unwrap();
        }
        // Now: epoch=6, draining=[0,1,2,3,4,5] all with leases held.
        assert_eq!(mgr.draining_count(), 6);
        assert_eq!(mgr.epoch(), 6);

        // Release leases on the OLD arenas (epochs 0,1,2) but keep leases on
        // YOUNG arenas (epochs 3,4,5 — within COLLECT_YOUNG_EPOCHS=2 of epoch 6).
        for &idx in &leased[..3] {
            mgr.arena_at(idx).unwrap().release_lease();
        }

        // First collect: should collect old arenas (0,1,2) then hit arena 3
        // (epoch 3, still has lease). Arena 3 is young relative to epoch 6
        // (6-3=3 > COLLECT_YOUNG_EPOCHS=2), so it won't trigger early term.
        // Arena 4 (epoch 4, 6-4=2 == COLLECT_YOUNG_EPOCHS) WILL trigger early
        // termination, skipping arena 5.
        let collected = mgr.collect();
        assert_eq!(collected, 3, "old arenas 0,1,2 should be collected");
        // Arenas 3,4,5 remain draining. Early termination skipped arena 5.
        assert_eq!(mgr.draining_count(), 3);

        // Release arena 3's lease. Arenas 4,5 still have leases.
        mgr.arena_at(leased[3]).unwrap().release_lease();

        // Second collect: arena 3 (no lease) collected. Arena 4 (young, has
        // lease) triggers early termination, skipping arena 5.
        let collected = mgr.collect();
        assert_eq!(collected, 1, "arena 3 should be collected");
        assert_eq!(mgr.draining_count(), 2);

        // Release remaining leases.
        mgr.arena_at(leased[4]).unwrap().release_lease();
        mgr.arena_at(leased[5]).unwrap().release_lease();

        // Final collect: both should be collected now.
        let collected = mgr.collect();
        assert_eq!(collected, 2, "arenas 4,5 should be collected");
        assert_eq!(mgr.draining_count(), 0);
    }

    /// Helper: assert the unsafe invariant that `arenas[write_idx]` is `Some`.
    fn assert_write_idx_valid(mgr: &ArenaManager) {
        let idx = mgr.write_idx.as_usize();
        assert!(
            idx < mgr.arenas.len() && mgr.arenas[idx].is_some(),
            "INVARIANT VIOLATED: arenas[write_idx={idx}] is not a valid Some entry"
        );
    }

    #[test]
    fn write_idx_invariant_after_new() {
        let mgr = ArenaManager::new(&test_config()).unwrap();
        assert_write_idx_valid(&mgr);
    }

    #[test]
    fn write_idx_invariant_after_rotate() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();
        for _ in 0..10 {
            mgr.rotate().unwrap();
            assert_write_idx_valid(&mgr);
        }
    }

    #[test]
    fn write_idx_invariant_after_collect() {
        let mut mgr = ArenaManager::new(&test_config()).unwrap();
        mgr.rotate().unwrap();
        mgr.rotate().unwrap();
        mgr.collect();
        assert_write_idx_valid(&mgr);
    }

    #[test]
    fn write_idx_invariant_after_shrink() {
        let config = PoolConfig {
            arena_size: 4096,
            initial_arenas: 1,
            max_free_arenas: 0,
            max_total_arenas: 0,
            registration_slots: 32,
            page_size: 4096,
        };
        let mut mgr = ArenaManager::new(&config).unwrap();
        // Rotate several times to accumulate arenas, then collect and shrink.
        for _ in 0..4 {
            mgr.rotate().unwrap();
        }
        mgr.collect();
        mgr.shrink();
        assert_write_idx_valid(&mgr);
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

        // Hold leases so auto-collect can't recycle during rotate.
        mgr.current_arena().acquire_lease(); // arena 0

        // Rotate: retire epoch 0.
        let r = mgr.rotate().unwrap();
        assert_eq!(r.retired_epoch, 0);
        assert_eq!(r.new_epoch, 1);

        mgr.current_arena().acquire_lease(); // arena 1

        // Rotate again: free pool empty, auto-collect finds nothing (leases held) → alloc new.
        let r = mgr.rotate().unwrap();
        assert_eq!(r.retired_epoch, 1);
        assert!(r.new_arena_idx.is_some());

        // Release leases, then collect: both retired arenas have no leases.
        mgr.arena_at(ArenaIdx::new(0)).unwrap().release_lease();
        mgr.arena_at(ArenaIdx::new(1)).unwrap().release_lease();
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
