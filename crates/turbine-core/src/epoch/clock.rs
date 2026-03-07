use std::cell::Cell;

use crate::config::PoolConfig;
use crate::epoch::arena::{Arena, ArenaState};
use crate::error::{Result, TurbineError};

/// A fixed-size ring of arenas that rotates through epochs.
///
/// The clock maintains a monotonically increasing epoch counter. On each
/// `rotate()` call the current arena is retired (read-only) and the next
/// arena in the ring becomes writable for the new epoch.
pub struct EpochClock {
    arenas: Vec<Arena>,
    /// Monotonically increasing epoch counter.
    epoch: Cell<u64>,
    /// Index of the currently writable arena in the ring.
    write_idx: Cell<usize>,
}

impl EpochClock {
    /// Create a new epoch clock with arenas allocated per `config`.
    pub fn new(config: &PoolConfig) -> Result<Self> {
        config.validate()?;

        let mut arenas = Vec::with_capacity(config.arena_count);
        for _ in 0..config.arena_count {
            arenas.push(Arena::new(config.arena_size)?);
        }

        // Activate the first arena at epoch 0.
        arenas[0].set_epoch(0);
        arenas[0].reset(); // sets state to Writable

        Ok(Self {
            arenas,
            epoch: Cell::new(0),
            write_idx: Cell::new(0),
        })
    }

    /// The current epoch number.
    pub fn epoch(&self) -> u64 {
        self.epoch.get()
    }

    /// Reference to the currently writable arena.
    pub fn current_arena(&self) -> &Arena {
        &self.arenas[self.write_idx.get()]
    }

    /// Index of the currently writable arena in the ring.
    pub fn current_arena_idx(&self) -> usize {
        self.write_idx.get()
    }

    /// Number of arenas in the ring.
    pub fn arena_count(&self) -> usize {
        self.arenas.len()
    }

    /// Rotate to a new epoch.
    ///
    /// - Retires the current arena (state → Retired).
    /// - Advances the write index to the next arena in the ring.
    /// - Returns `Err` if the next arena still has outstanding leases (preventing
    ///   data corruption from recycling in-use memory).
    /// - Returns `Ok((retired_epoch, new_epoch))` on success.
    pub fn rotate(&self) -> Result<(u64, u64)> {
        let retired_epoch = self.epoch.get();
        let new_epoch = retired_epoch + 1;

        // Retire the current arena.
        let current_idx = self.write_idx.get();
        self.arenas[current_idx].set_state(ArenaState::Retired);

        // Advance to next slot.
        let next_idx = (current_idx + 1) % self.arenas.len();

        let next_arena = &self.arenas[next_idx];
        if next_arena.lease_count() > 0 {
            return Err(TurbineError::EpochNotCollectable(
                next_arena.epoch(),
                next_arena.lease_count(),
            ));
        }

        // Activate the next arena.
        next_arena.reset();
        next_arena.set_epoch(new_epoch);

        self.write_idx.set(next_idx);
        self.epoch.set(new_epoch);

        Ok((retired_epoch, new_epoch))
    }

    /// Try to collect (reclaim) the arena that served `epoch`.
    ///
    /// Returns `Ok(())` if the arena had zero leases and was marked Collected,
    /// or an error if it still has outstanding leases.
    pub fn try_collect(&self, epoch: u64) -> Result<()> {
        let arena = self.arena_for_epoch(epoch)?;

        if arena.lease_count() > 0 {
            return Err(TurbineError::EpochNotCollectable(
                epoch,
                arena.lease_count(),
            ));
        }

        arena.set_state(ArenaState::Collected);
        Ok(())
    }

    /// Look up the arena that served a given epoch.
    ///
    /// Only arenas currently in the ring are searchable.
    pub fn arena_for_epoch(&self, epoch: u64) -> Result<&Arena> {
        self.arenas
            .iter()
            .find(|a| a.epoch() == epoch)
            .ok_or(TurbineError::EpochNotFound(epoch))
    }

    /// Bounds-checked direct access to an arena by index.
    pub fn arena_at(&self, idx: usize) -> Option<&Arena> {
        self.arenas.get(idx)
    }

    /// Iterator over all arenas in the ring (for io_uring registration).
    pub fn arenas(&self) -> &[Arena] {
        &self.arenas
    }

    /// Iterator over retired (non-collected) epochs, oldest first.
    pub fn retained_epochs(&self) -> impl Iterator<Item = u64> + '_ {
        self.arenas
            .iter()
            .filter(|a| a.state() == ArenaState::Retired)
            .map(|a| a.epoch())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(count: usize) -> PoolConfig {
        PoolConfig {
            arena_size: 4096,
            arena_count: count,
            page_size: 4096,
        }
    }

    #[test]
    fn initial_epoch_is_zero() {
        let clock = EpochClock::new(&test_config(2)).unwrap();
        assert_eq!(clock.epoch(), 0);
    }

    #[test]
    fn rotate_advances_monotonically() {
        let clock = EpochClock::new(&test_config(3)).unwrap();

        let (retired, active) = clock.rotate().unwrap();
        assert_eq!(retired, 0);
        assert_eq!(active, 1);

        let (retired, active) = clock.rotate().unwrap();
        assert_eq!(retired, 1);
        assert_eq!(active, 2);
    }

    #[test]
    fn rotate_wraps_around_ring() {
        let clock = EpochClock::new(&test_config(2)).unwrap();

        clock.rotate().unwrap(); // epoch 0→1, idx 0→1
        clock.rotate().unwrap(); // epoch 1→2, idx 1→0 (wraps)

        assert_eq!(clock.epoch(), 2);
        // The arena at index 0 should now be writable with epoch 2.
        assert_eq!(clock.current_arena().epoch(), 2);
        assert_eq!(clock.current_arena().state(), ArenaState::Writable);
    }

    #[test]
    fn rotate_blocked_by_outstanding_leases() {
        let clock = EpochClock::new(&test_config(2)).unwrap();

        // Acquire a lease on epoch 0's arena (index 0).
        clock.current_arena().acquire_lease();

        // Rotate to epoch 1 (arena index 1) — succeeds.
        clock.rotate().unwrap();

        // Rotate again would recycle arena 0 which still has a lease — must fail.
        let err = clock.rotate().unwrap_err();
        assert!(matches!(err, TurbineError::EpochNotCollectable(0, 1)));

        // Release the lease so the arena doesn't debug_assert on drop.
        clock.arena_for_epoch(0).unwrap().release_lease();
    }

    #[test]
    fn try_collect_succeeds_with_no_leases() {
        let clock = EpochClock::new(&test_config(3)).unwrap();
        clock.rotate().unwrap(); // retire epoch 0

        clock.try_collect(0).unwrap();
        let arena = clock.arena_for_epoch(0).unwrap();
        assert_eq!(arena.state(), ArenaState::Collected);
    }

    #[test]
    fn try_collect_fails_with_outstanding_leases() {
        let clock = EpochClock::new(&test_config(3)).unwrap();
        clock.current_arena().acquire_lease();
        clock.rotate().unwrap(); // retire epoch 0, which has a lease

        let err = clock.try_collect(0).unwrap_err();
        assert!(matches!(err, TurbineError::EpochNotCollectable(0, 1)));

        // Release the lease so the arena doesn't debug_assert on drop.
        clock.arena_for_epoch(0).unwrap().release_lease();
    }

    #[test]
    fn retained_epochs_lists_retired_arenas() {
        let clock = EpochClock::new(&test_config(4)).unwrap();
        clock.rotate().unwrap(); // retire 0
        clock.rotate().unwrap(); // retire 1

        let retained: Vec<u64> = clock.retained_epochs().collect();
        assert!(retained.contains(&0));
        assert!(retained.contains(&1));
        assert_eq!(retained.len(), 2);
    }

    #[test]
    fn arena_for_epoch_not_found() {
        let clock = EpochClock::new(&test_config(2)).unwrap();
        assert!(clock.arena_for_epoch(99).is_err());
    }
}
