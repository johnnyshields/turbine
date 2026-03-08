# Turbine Conventions

## Unsafe Code

- **`#![deny(unsafe_op_in_unsafe_fn)]`** is enforced crate-wide in `turbine-core`. Every unsafe operation must be wrapped in an explicit `unsafe {}` block, even inside an `unsafe fn`. This improves auditability — grep for `unsafe {` finds every unsafe operation.

- **Single source of truth for unsafe accessors.** The `ArenaManager::write_arena()` private method is the sole place that performs unchecked access to `arenas[write_idx]`. Public methods (`current_arena()`, `current_arena_with_idx()`) delegate to it. Do not duplicate the `get_unchecked`/`unwrap_unchecked` pattern elsewhere.

- **No speculative unsafe.** Do not add unsafe variants of functions unless a profiler proves the safe version is a bottleneck. If the safe path has negligible overhead (e.g., a single bounds check dwarfed by the rest of the call), keep it safe. Dead unsafe code is a maintenance hazard and was removed in the hot-path hardening pass.

- **Invariant tests for unsafe preconditions.** Every unsafe precondition must have a corresponding test that explicitly asserts the precondition holds after every state mutation. See `write_idx_invariant_after_{new,rotate,collect,shrink}` tests in `manager.rs`.

## Atomics & Thread Safety

- `Arena::remote_returns` (`AtomicUsize`) is the **only** atomic in the arena. All other arena state uses `Cell<T>` under the thread-local assumption.
- `LeasedBuffer` and `IouringBufferPool` are `!Send`, enforced via `PhantomData<Rc<()>>`.
- Cross-thread transfer goes through `into_sendable()` → `SendableBuffer`, which uses `ManuallyDrop` to avoid double-decrement.

## Arena Lifecycle

```
Writable  →  Retired (drain queue)  →  Collected (free pool)  →  recycled as Writable
```

- `rotate()` secures the next arena **before** retiring the current one — on failure, no state is mutated.
- `rotate()` auto-collects draining arenas when the free pool is empty before allocating new.
- `collect()` only recycles arenas with zero outstanding leases.
- `shrink()` drops excess free-pool arenas beyond `max_free_arenas`.

## Testing

- Standard test config: `PoolConfig { arena_size: 4096, initial_arenas: 3, ..defaults }`
- Use `NoopHooks` for tests that don't exercise hook behavior.
- Always clean up leases in tests to avoid `debug_assert` failures in `Arena::drop`.
- Arena minimum count is 1 (one writable); draining arenas accumulate in drain queue.

## Code Style

- `#[inline(always)]` on hot-path accessors (`current_arena`, `current_arena_with_idx`, `write_arena`).
- `#[inline]` on moderately-hot paths (`arena_at`, `epoch`).
- Newtype wrappers (`ArenaIdx`, `SlotId`) for type safety — never pass raw `usize`/`u16` where a newtype is expected.
