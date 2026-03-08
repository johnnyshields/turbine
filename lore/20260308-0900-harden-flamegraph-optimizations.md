# Harden: Flamegraph-Driven Optimizations

## Context

Four changes just landed on `feat-hotpath-optimization`: cache-line padding for Arena, collect early termination, SPSC flamegraph binary, and io_uring flamegraph binary. This hardening pass reviews those changes for correctness bugs, missing tests, and code quality issues.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | SPSC ring missing Drop impl — leaked SendableBuffers | Quick | Medium | Auto-fix |
| 2 | SPSC ring termination race — consumer may miss items after close | Quick | Medium | Auto-fix |
| 3 | RingRegistration slot leak on unregister/register cycle | Easy | High | Auto-fix |
| 4 | Missing test for collect early termination skip_remaining path | Easy | Medium | Auto-fix |
| 5 | CLAUDE.md: document cache-line padding convention | Quick | Low | Ask first |

## Opportunity Details

### 1. SPSC ring missing Drop impl
- **What**: `SpscRing<T>` has no `Drop` impl. Items remaining in the ring when it's dropped are never dropped (MaybeUninit leak). For `SendableBuffer`, this means `remote_returns.fetch_add` never fires, so the arena thinks leases are still outstanding.
- **Where**: `crates/turbine-bench/src/bin/flamegraph_spsc.rs`, `SpscRing<T>` impl
- **Why**: Correctness — even in a benchmark binary, leaked resources can cause confusing behavior during development
- **Fix**: Add `impl<T> Drop for SpscRing<T>` that drains remaining items by reading from head to tail

### 2. SPSC ring termination race
- **What**: After producer calls `close()`, consumer's `pop()` may see `closed=true` before seeing the final `tail` update (Relaxed load of `closed` after Acquire load of `tail`). This can drop the last few items.
- **Where**: `crates/turbine-bench/src/bin/flamegraph_spsc.rs`, `SpscRing::pop()`
- **Why**: Correctness — the lost items won't trigger `remote_release`, silently inflating outstanding lease counts
- **Fix**: After `pop()` sees `closed=true && tail==head`, do one final Acquire re-load of `tail` to catch any items pushed between the last `tail` check and `close()`

### 3. RingRegistration slot leak on unregister/register cycle
- **What**: `RingRegistration::unregister()` sets `registered=false` but does NOT free slot allocations or clear `arena_slot_map`. The next `register()` call allocates NEW slots, leaking old ones. With 32 default slots and 3 arenas, exhaustion after ~10 unregister/register cycles.
- **Where**: `crates/turbine-core/src/ring/registration.rs:112-122` (`unregister` method)
- **Why**: **Bug** — the new io_uring flamegraph binary calls unregister/register on every rotation interval. With `FLAMEGRAPH_ROTATE_INTERVAL=1000` over 5s, this can exhaust slots and panic.
- **Fix**: In `unregister()`, iterate `arena_slot_map`, free each allocated slot via `self.slots.free()`, clear the map entries. Add a test for the unregister→register cycle.

### 4. Missing test for collect early termination
- **What**: The `skip_remaining` path in `collect()` has no dedicated test. Existing tests exercise collect but don't verify the early termination optimization.
- **Where**: `crates/turbine-core/src/epoch/manager.rs`, tests module
- **Why**: The optimization is a correctness-neutral hint, but we should verify it doesn't accidentally skip arenas that should be collected
- **Fix**: Add `collect_early_termination_skips_young` test: rotate many times with held leases at varying epochs, verify collect skips young arenas but still collects old ones on subsequent calls

### 5. CLAUDE.md: document cache-line padding convention
- **What**: The `CacheAligned<T>` wrapper and `#[repr(C)]` on Arena are a new pattern. CLAUDE.md should document this convention.
- **Where**: `CLAUDE.md` Safety-Critical Invariants or Conventions section
- **Why**: Prevents future contributors from reordering Arena fields or removing the padding
- **Fix**: Add a bullet to the conventions: "Arena uses `#[repr(C)]` with `CacheAligned<T>` to isolate `remote_returns` on its own cache line — do not reorder fields"

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace && cargo clippy --workspace`
