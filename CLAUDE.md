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

- `buffer/pool.rs` — `IouringBufferPool`: main API, owns ArenaManager + channel, UnsafeCell interior mutability
- `buffer/leased.rs` — `LeasedBuffer`: !Send buffer with arena lease, `slot_id` for io_uring, `into_sendable()` for cross-thread
- `buffer/pinned.rs` — `PinnedWrite`: borrow guard for io_uring submission, `buf_index()` returns slot_id
- `epoch/manager.rs` — `ArenaManager`: slab-based arena management with drain queue and free pool
- `epoch/arena.rs` — `Arena`: mmap'd bump allocator, lease counting via `Cell<usize>`, `advise_free_unused()` for madvise
- `transfer/handle.rs` — `TransferHandle` + `SendableBuffer`: cross-thread buffer transfer
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
2. `LeasedBuffer` and `IouringBufferPool` are `!Send` (enforced via `PhantomData<Rc<()>>`)
3. `SendableBuffer::new()` is `pub(crate)` — must go through `into_sendable()`
4. `into_sendable()` uses `ManuallyDrop` to transfer lease ownership without double-decrement
5. `drain_returns()` uses arena slab index (not epoch scan) with epoch sanity check
6. Arena `Drop` has `debug_assert` for leaked leases
7. Arenas stored as `Box<Arena>` in slab — pointer stability guaranteed across Vec growth

## Conventions

- No atomics — all arena state uses `Cell<usize>` (thread-local assumption)
- Epoch lifecycle: Writable → Retired → Collected → recycled
- Tests use `NoopHooks` and `PoolConfig { arena_size: 4096, initial_arenas: 3, ..defaults }`
- Arena minimum count is 1 (one writable); draining arenas accumulate in drain queue
