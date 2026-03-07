# Turbine Hardening Plan

## Context

Turbine v0.1.0 was just implemented — 38 tests passing, clean build. This hardening pass reviews all 16 source files for correctness bugs, API inconsistencies, missing test coverage, and code quality issues.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Fix duplicate comment in transfer/handle.rs | Quick | Low | Auto-fix |
| 2 | Add `buf_id` to `ReturnedBuffer`, fix hardcoded `0` in `drain_returns()` | Easy | High | Ask first |
| 3 | Make `Arena::new()` `pub(crate)` — callers should go through pool | Easy | Medium | Ask first |
| 4 | Guard `arena_to_buf_index()` against `usize > u16::MAX` truncation | Easy | Medium | Ask first |
| 5 | Return drain count from `drain_returns()` → `usize` | Easy | Medium | Ask first |
| 6 | Add checked arithmetic to `buf_id` counter in `Arena::alloc()` | Easy | Low | Ask first |
| 7 | Add missing tests: multi-rotation wrap, drain_returns correctness, error display | Moderate | Medium | Ask first |

## Opportunity Details

### #1 — Fix duplicate comment in transfer/handle.rs
- **What**: Line 85-86 has `// Compile-time assertions for trait bounds.` twice
- **Where**: `crates/turbine-core/src/transfer/handle.rs:85-86`
- **Why**: Code cleanliness

### #2 — Add buf_id to ReturnedBuffer, fix drain_returns hardcoded 0
- **What**: `ReturnedBuffer` only has `epoch` and `arena_idx`. In `pool.rs:87`, `drain_returns()` calls `self.hooks.on_release(ret.epoch, 0)` with a hardcoded `0` for buf_id. Any hook implementing `BufferPinHook::on_release()` gets wrong buf_id data.
- **Where**: `transfer/handle.rs` (ReturnedBuffer struct, SendableBuffer), `buffer/pool.rs` (drain_returns)
- **Why**: Correctness — hooks receive garbage buf_id values
- **Trade-offs**: Adds one u32 to the channel message; negligible overhead

### #3 — Make Arena::new() pub(crate)
- **What**: `Arena::new(size)` is `pub` but bypasses `PoolConfig::validate()` — callers can create misaligned arenas. Should be `pub(crate)` since only `EpochClock::new()` should construct arenas.
- **Where**: `epoch/arena.rs:43`
- **Why**: Prevents API misuse; enforces validation at the only correct entry point

### #4 — Guard arena_to_buf_index truncation
- **What**: `arena_to_buf_index(arena_idx: usize) -> u16` does `arena_idx as u16` which silently truncates values > 65535. Add a debug_assert or panic.
- **Where**: `ring/registration.rs:52-54`
- **Why**: io_uring buf_index is u16; silent truncation would cause wrong-buffer reads

### #5 — Return drain count from drain_returns
- **What**: `drain_returns(&self)` returns `()`. Change to return `usize` (number of buffers drained). Callers get visibility into cross-thread return activity.
- **Where**: `buffer/pool.rs:83`
- **Why**: Observability — callers currently have no way to know if draining did anything

### #6 — Checked arithmetic for buf_id counter
- **What**: `next_buf_id.set(buf_id + 1)` in `Arena::alloc()` can overflow u32. Use `checked_add` and return `None` on overflow (same as arena-full).
- **Where**: `epoch/arena.rs:87`
- **Why**: Defensive — prevents silent wrap-around after 4B allocations per arena
- **Trade-offs**: Extremely unlikely in practice since arenas reset on reuse

### #7 — Add missing tests
- **What**: Add tests for:
  - Pool: multi-rotation wrap-around (arena reuse after N rotations)
  - Pool: `drain_returns` actually decrements lease and enables collection
  - Error: `TurbineError` Display strings are correct
  - Config: boundary values (arena_size=0, page_size=0)
- **Where**: `buffer/pool.rs`, `error.rs`, `config.rs`
- **Why**: Coverage for edge cases and error paths currently untested

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test`
