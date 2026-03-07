# Harden: Performance Optimization Commit

## Context

Hardening pass on commit `4bd03b7` ("Optimize hot-path performance: build profiles, inline hints, data structures"). The commit added build profiles, `#[inline]`/`#[cold]` annotations across 6 files, replaced `HashMap<ArenaIdx, SlotId>` with `Vec<Option<SlotId>>`, and replaced `Vec<bool>` slot allocator with a `u64` bitmap.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Add bitmap allocator edge case tests (capacity=1, capacity=64, full+free+realloc) | Quick | Medium | Auto-fix |
| 2 | Add Vec-based arena_slot_map test with large ArenaIdx gap | Quick | Medium | Auto-fix |
| 3 | Add `#[inline]` to `LeasedBuffer` and `PinnedWrite` getters | Quick | Low | Auto-fix |
| 4 | Guard `SlotAllocator::new()` against capacity=0 | Quick | Low | Auto-fix |

## Opportunity Details

### #1: Bitmap allocator edge case tests
- **What**: Add tests for `SlotAllocator` with capacity=1 (minimum), capacity=64 (maximum), and full-then-free-then-reallocate pattern at capacity=64
- **Where**: `crates/turbine-core/src/ring/registration.rs` (test module)
- **Why**: The bitmap implementation is new and replaces a simpler Vec<bool>. Current tests only exercise capacity=2,4,8. Boundary conditions (1 and 64) aren't tested.

### #2: Vec arena_slot_map test with large ArenaIdx
- **What**: Add a test that registers arenas with non-contiguous, large ArenaIdx values (e.g., 0, 50, 99) to verify `ensure_arena_capacity()` and sparse Vec indexing work correctly
- **Where**: `crates/turbine-core/src/ring/registration.rs` (test module)
- **Why**: The HashMap accepted any key; the Vec replacement relies on `ensure_arena_capacity()` for dynamic growth. A gap-heavy test confirms correctness.

### #3: `#[inline]` on LeasedBuffer/PinnedWrite getters
- **What**: Add `#[inline]` to `LeasedBuffer::epoch()`, `buf_id()`, `arena_idx()`, `slot_id()`, `len()`, `is_empty()` and `PinnedWrite::as_ptr()`, `as_mut_ptr()`, `len()`, `is_empty()`, `buf_index()`
- **Where**: `crates/turbine-core/src/buffer/leased.rs`, `crates/turbine-core/src/buffer/pinned.rs`
- **Why**: Consistency with the inline annotations added elsewhere. These are trivial field accesses on the lease hot path. The compiler likely inlines them anyway, but explicit hints ensure it across crate boundaries.

### #4: Guard SlotAllocator::new() against capacity=0
- **What**: Add `assert!(capacity > 0, "SlotAllocator requires at least 1 slot")` to `SlotAllocator::new()`
- **Where**: `crates/turbine-core/src/ring/registration.rs`, line 13
- **Why**: capacity=0 would silently produce an allocator that always returns None. While PoolConfig validation prevents this in practice, a defensive assert documents the invariant.

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace`
