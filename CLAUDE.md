# Turbine

Epoch-based buffer rotation for io_uring. Linux only, MIT licensed.

## Project Structure

```
crates/
  turbine-core/   # Core library: arenas, epochs, io_uring registration, cross-thread transfer
  turbine/        # Facade crate re-exporting the public API
lore/             # Implementation notes and research documents
```

## Key Modules (turbine-core)

- `types.rs` — `ArenaIdx`, `SlotId`: newtype wrappers for type-safe arena slab indices and io_uring slot IDs
- `buffer/pool.rs` — `IouringBufferPool`: main API, owns ArenaManager, UnsafeCell interior mutability
- `buffer/leased.rs` — `LeasedBuffer`: !Send buffer with arena lease, `SlotId` for io_uring, `into_sendable()` for cross-thread
- `buffer/pinned.rs` — `PinnedWrite`: borrow guard for io_uring submission, `buf_index()` returns `SlotId`
- `epoch/manager.rs` — `ArenaManager`: slab-based arena management with drain queue and free pool, auto-collect on rotate
- `epoch/arena.rs` — `Arena`: mmap'd bump allocator, lease counting via `Cell<usize>` + `AtomicUsize` (split counter for cross-thread returns), `advise_free_unused()` for madvise (warns on failure)
- `transfer/handle.rs` — `SendableBuffer`: cross-thread buffer transfer via atomic lease release
- `ring/registration.rs` — `RingRegistration`: slot allocator + arena-to-slot mapping for io_uring
- `config.rs` — `PoolConfig`: arena size, initial count, max free/total arenas, registration slots, page size
- `gc.rs` — `BufferPinHook` + `EpochObserver` traits (with arena alloc/free/sweep hooks), `NoopHooks`
- `error.rs` — `TurbineError` enum, `Result<T>` alias

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace
```

## Safety-Critical Invariants

These are load-bearing and must not be weakened:

1. `rotate()` never blocks — retired arenas go to drain queue, only recycled after `collect()` confirms 0 leases
2. `rotate()` secures next arena BEFORE retiring current — on failure, no state is mutated
3. `rotate()` auto-collects draining arenas when free pool is empty before allocating new
4. `LeasedBuffer` and `IouringBufferPool` are `!Send` (enforced via `PhantomData<Rc<()>>`)
5. `SendableBuffer::new()` is `pub(crate)` — must go through `into_sendable()`
6. `into_sendable()` uses `ManuallyDrop` to transfer lease ownership without double-decrement
7. Arena `Drop` has `debug_assert` for leaked leases
8. Arenas stored as `Box<Arena>` in slab — pointer stability guaranteed across Vec growth
9. `ArenaIdx` and `SlotId` newtypes prevent mixing arena indices with slot IDs
10. `SendableBuffer` stores `*const AtomicUsize` pointing to arena's `remote_returns` — valid because `Box<Arena>` provides address stability and arena can't be freed while outstanding leases exist

## Conventions

- Atomics only for `Arena::remote_returns` (cross-thread lease release); all other arena state uses `Cell` (thread-local assumption)
- Epoch lifecycle: Writable → Retired → Collected → recycled
- Tests use `NoopHooks` and `PoolConfig { arena_size: 4096, initial_arenas: 3, ..defaults }`
- Arena minimum count is 1 (one writable); draining arenas accumulate in drain queue
