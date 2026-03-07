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

- `buffer/pool.rs` — `IouringBufferPool`: main API, owns epoch clock + channel
- `buffer/leased.rs` — `LeasedBuffer`: !Send buffer with arena lease, `into_sendable()` for cross-thread
- `buffer/pinned.rs` — `PinnedWrite`: borrow guard for io_uring submission
- `epoch/clock.rs` — `EpochClock`: ring of arenas, epoch rotation, `rotate()` returns `Result`
- `epoch/arena.rs` — `Arena`: mmap'd bump allocator, lease counting via `Cell<usize>`
- `transfer/handle.rs` — `TransferHandle` + `SendableBuffer`: cross-thread buffer transfer
- `ring/registration.rs` — `RingRegistration`: io_uring fixed-buffer iovec management
- `config.rs` — `PoolConfig`: arena size, count, page size validation
- `gc.rs` — `BufferPinHook` + `EpochObserver` traits, `NoopHooks`
- `error.rs` — `TurbineError` enum, `Result<T>` alias

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace
```

## Safety-Critical Invariants

These are load-bearing and must not be weakened:

1. `rotate()` returns `Err` when the next arena has live leases — never warn-and-continue
2. `LeasedBuffer` and `IouringBufferPool` are `!Send` (enforced via `PhantomData<Rc<()>>`)
3. `SendableBuffer::new()` is `pub(crate)` — must go through `into_sendable()`
4. `into_sendable()` uses `ManuallyDrop` to transfer lease ownership without double-decrement
5. `drain_returns()` uses arena index (not epoch scan) with epoch sanity check
6. Arena `Drop` has `debug_assert` for leaked leases

## Conventions

- No atomics — all arena state uses `Cell<usize>` (thread-local assumption)
- Epoch lifecycle: Writable → Retired → Collected → recycled
- Tests use `NoopHooks` and `PoolConfig { arena_size: 4096, arena_count: 3, page_size: 4096 }`
- Arena minimum count is 2 (one writable, one draining)
