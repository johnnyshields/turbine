# Harden Hot-Path Optimizations

Date: 2026-03-07
Commit: 3a2163c (feat-hotpath-optimization)

## Context

The hot-path optimization commit introduced `unsafe` code to eliminate panic branches and bounds checks from the lease path (`current_arena()`, `current_arena_with_idx()`). This review assesses safety, identifies dead code, and proposes hardening changes.

## Safety Audit Summary

The `unsafe` invariant in `current_arena()` / `current_arena_with_idx()` is **SOUND**: `write_idx` always points to a valid `Some` slab entry, maintained by `new()` (initializes to index 0) and `rotate()` (secures next arena before updating `write_idx`; early-returns on failure without mutation).

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Remove dead `slot_for_arena_unchecked()` | Quick | Medium | Auto-fix |
| 2 | Consolidate duplicated unsafe block in manager.rs | Quick | Low | Auto-fix |
| 3 | Add unsafe invariant tests for `write_idx` validity | Easy | High | Auto-fix |
| 4 | Add `#[deny(unsafe_op_in_unsafe_fn)]` crate attribute | Quick | Medium | Ask first |

## Opportunity Details

### #1 Remove dead `slot_for_arena_unchecked()`
- **What**: Delete `unsafe fn slot_for_arena_unchecked()` from `registration.rs`. It was added speculatively but is never called.
- **Where**: `crates/turbine-core/src/ring/registration.rs:157-170`
- **Why**: Dead unsafe code is a maintenance hazard — future callers may misuse it without understanding the safety contract. The safe `slot_for_arena()` is already `#[inline(always)]` and the bounds check is negligible vs the rest of the lease path.
- **Trade-offs**: None. Can be re-added if a profiler proves the bounds check matters.

### #2 Consolidate duplicated unsafe block in manager.rs
- **What**: Extract the repeated `get_unchecked + unwrap_unchecked` pattern into a single private `#[inline(always)]` helper `write_arena()`, called by both `current_arena()` and `current_arena_with_idx()`.
- **Where**: `crates/turbine-core/src/epoch/manager.rs:158-195`
- **Why**: Single source of truth for the unsafe invariant. If the invariant needs to change, only one place to update.
- **Trade-offs**: Adds one more function, but it's trivially inlined. Safety comment stays in one place.

### #3 Add unsafe invariant tests for `write_idx` validity
- **What**: Add a test that explicitly asserts `arenas[write_idx]` is `Some` after every mutation operation (new, rotate, collect, shrink). This catches any future code that breaks the invariant relied upon by the unsafe `current_arena()`.
- **Where**: `crates/turbine-core/src/epoch/manager.rs` (tests module)
- **Why**: The existing tests cover correctness indirectly but never explicitly validate the unsafe precondition. A dedicated test makes the invariant visible and self-documenting.
- **Trade-offs**: None.

### #4 Add `#[deny(unsafe_op_in_unsafe_fn)]` crate attribute
- **What**: Add `#![deny(unsafe_op_in_unsafe_fn)]` to `crates/turbine-core/src/lib.rs` to enforce that unsafe operations within unsafe functions still require explicit `unsafe {}` blocks.
- **Where**: `crates/turbine-core/src/lib.rs`
- **Why**: This is a Rust 2024 lint (currently warn-by-default). We already follow this pattern in `slot_for_arena_unchecked`, but making it a deny ensures consistency. Prevents accidentally introducing unsafe ops without explicit blocks in future code.
- **Trade-offs**: Slightly more verbose unsafe code, but improves auditability.

## Outcome

All 4 items implemented. 80 tests pass, clippy clean.

- `slot_for_arena_unchecked()` removed from `registration.rs`
- `write_arena()` private helper added to `manager.rs`, `current_arena()` and `current_arena_with_idx()` delegate to it
- 4 invariant tests added: `write_idx_invariant_after_{new,rotate,collect,shrink}`
- `#![deny(unsafe_op_in_unsafe_fn)]` added to `lib.rs`
- `lore/conventions.md` created with extended project conventions
- `CLAUDE.md` updated with new conventions
