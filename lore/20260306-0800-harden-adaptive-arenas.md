# Hardening: Adaptive Arena Architecture

## Context

The adaptive arena architecture was just implemented, replacing the fixed-size epoch ring with a slab + drain-queue + free-pool design. This hardening pass reviews the new code for correctness, performance, test coverage, and API cleanliness.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Check madvise return value in `advise_free_unused()` | Quick | Medium | Auto-fix |
| 2 | Remove unused `_arena` param from `register_arena()` | Quick | Low | Auto-fix |
| 3 | Track live arena count to avoid O(n) scan in `alloc_arena()` | Easy | Medium | Auto-fix |
| 4 | Optimize `collect()` to avoid temporary Vec allocation | Easy | Medium | Auto-fix |
| 5 | Fix `lease()` silent slot_id fallback to 0 | Easy | Medium | Auto-fix |
| 6 | Add missing test: partial draining in `collect()` | Easy | Medium | Auto-fix |
| 7 | Add missing test: `shrink()` frees registration slots | Easy | Medium | Auto-fix |
| 8 | Rename `try_collect()` → `collect_epoch()` for clarity | Easy | Low | Ask first |
| 9 | Auto-collect in `rotate()` before allocating new arena | Moderate | High | Ask first |
| 10 | Newtype wrappers for `ArenaIdx` and `SlotId` | Hard | Medium | Ask first |

## Opportunity Details

### 1. Check madvise return value
- **What**: `advise_free_unused()` in arena.rs ignores the madvise return code
- **Where**: `crates/turbine-core/src/epoch/arena.rs:159-171`
- **Why**: Silent failures hide memory pressure issues; log on failure
- **Trade-offs**: None — madvise failure is non-fatal but should be observable

### 2. Remove unused `_arena` param
- **What**: `register_arena(&mut self, slab_idx, _arena)` never reads the arena
- **Where**: `crates/turbine-core/src/ring/registration.rs:114`
- **Why**: Dead parameter confuses callers, suggests incomplete implementation
- **Trade-offs**: Breaking API change (desired per harden rules)

### 3. Track live arena count in ArenaManager
- **What**: `alloc_arena()` does `arenas.iter().filter(|s| s.is_some()).count()` — full O(n) scan just to check the limit. Add `live_count: usize` field, increment on alloc, decrement on shrink.
- **Where**: `crates/turbine-core/src/epoch/manager.rs:193-215`
- **Why**: O(1) limit check instead of O(n) on every new arena allocation
- **Trade-offs**: One extra field to maintain

### 4. Optimize collect() in-place
- **What**: `collect()` creates `still_draining: Vec<usize>` then replaces `self.draining`. Use `retain()` with side-effect instead.
- **Where**: `crates/turbine-core/src/epoch/manager.rs:115-134`
- **Why**: Avoids temporary allocation on every collect sweep
- **Trade-offs**: Slightly less readable but idiomatic Rust

### 5. Fix silent slot_id fallback
- **What**: `pool.lease()` uses `slot_for_arena().unwrap_or(0)` — silently assigns slot 0 if arena not registered. Before registration, all leases get slot 0 which is correct for unregistered pools; but after registration, a missing slot is a bug.
- **Where**: `crates/turbine-core/src/buffer/pool.rs:87`
- **Why**: After calling `register()`, a missing slot mapping indicates a logic error. Log a warning when registered but slot missing.
- **Trade-offs**: Minor — adds one branch

### 6. Add test: partial draining in collect()
- **What**: Test where some arenas in drain queue have leases and others don't — verify only zero-lease arenas move to free pool
- **Where**: `crates/turbine-core/src/epoch/manager.rs` tests
- **Why**: Current tests only cover all-zero or all-nonzero lease scenarios

### 7. Add test: shrink frees registration slots
- **What**: Test that after `shrink()`, freed arena indices are also removed from registration slot map
- **Where**: `crates/turbine-core/src/buffer/pool.rs` or integration test
- **Why**: Ensures registration stays in sync with arena lifecycle

### 8. Rename try_collect → collect_epoch
- **What**: `try_collect(epoch)` collects a specific epoch. `collect()` sweeps all. Rename to `collect_epoch()` for clarity.
- **Where**: `crates/turbine-core/src/buffer/pool.rs`
- **Why**: Current naming is confusing — "try" implies fallibility, not scoping

### 9. Auto-collect in rotate() before allocating
- **What**: When `rotate()` finds free_pool empty, call `collect()` on draining arenas before allocating a new arena. If collect frees something, use it instead of mmap'ing.
- **Where**: `crates/turbine-core/src/epoch/manager.rs:86-92`
- **Why**: Prevents unnecessary arena growth when reclaimable arenas exist
- **Trade-offs**: Adds collect overhead to rotate hot path; only when free pool is empty

### 10. Newtype wrappers for ArenaIdx and SlotId
- **What**: Wrap `usize` arena indices and `u16` slot IDs in newtypes to prevent mixing
- **Where**: Throughout codebase — manager.rs, pool.rs, registration.rs, leased.rs
- **Why**: Type safety prevents passing slot ID where arena index expected and vice versa
- **Trade-offs**: Significant churn across all modules; adds boilerplate

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
