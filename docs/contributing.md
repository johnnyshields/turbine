# Contributing

## Prerequisites

- Linux or WSL2
- Rust stable 1.80+

## Building and Testing

```bash
cargo test --workspace
cargo clippy --workspace
cargo bench -p turbine-bench
cargo bench -p turbine-bench --no-run  # compile-check only
```

## Project Layout

```
crates/
  turbine-core/          # Core library
    src/
      buffer/
        leased.rs        # LeasedBuffer (!Send, arena-backed)
        pinned.rs        # PinnedWrite (borrow guard for io_uring)
        pool.rs          # IouringBufferPool (main API)
      epoch/
        arena.rs         # Arena (mmap + bump allocator + split counter)
        manager.rs       # ArenaManager (slab, drain queue, free pool)
      ring/
        registration.rs  # RingRegistration (io_uring fixed-buffer slots)
      transfer/
        handle.rs        # SendableBuffer (Send, cross-thread transfer)
      config.rs          # PoolConfig
      error.rs           # TurbineError
      gc.rs              # BufferPinHook, EpochObserver, NoopHooks
      types.rs           # ArenaIdx, SlotId newtypes
      lib.rs
  turbine/               # Facade crate (re-exports)
    src/lib.rs
  turbine-bench/         # Benchmarks
    benches/
      lease_throughput.rs
      cross_thread_transfer.rs
      epoch_rotation.rs
    src/                  # Competitor implementations for comparison
docs/                    # Documentation
lore/                    # Implementation notes and research
```

## Safety Rules

These rules apply when working on turbine-core internals:

1. Never collect an arena with outstanding leases. `lease_count() > 0` means live pointers exist.
2. `LeasedBuffer` must stay `!Send`. The `PhantomData<Rc<()>>` marker enforces this.
3. `SendableBuffer::new()` must stay `pub(crate)`. External code must go through `into_sendable()`.
4. Arenas must be `Box`-allocated in the slab. Pointer stability required for SendableBuffer's raw pointers.
5. `ManuallyDrop` in `into_sendable()` is load-bearing. Prevents double-decrement.
6. `remote_returns` ordering matters. `fetch_add(Release)` pairs with `load(Acquire)`. Weakening either breaks happens-before.

## Conventions

- `#![deny(unsafe_op_in_unsafe_fn)]` is enforced crate-wide
- All unsafe access to the write-index slab goes through `ArenaManager::write_arena()`
- No speculative unsafe functions -- prefer safe paths unless a profiler proves the need
- Atomics only for `Arena::remote_returns`; all other arena state uses `Cell`
- Arena uses `#[repr(C)]` with `CacheAligned<T>` to isolate `remote_returns` on its own cache line

## Running Benchmarks

```bash
cargo bench -p turbine-bench
cargo bench -p turbine-bench -- lease_throughput
cargo bench -p turbine-bench -- cross_thread
cargo bench -p turbine-bench -- epoch_lifecycle
```

Results in `target/criterion/` with HTML reports (requires gnuplot).
See [benchmarks.md](benchmarks.md) for baseline numbers and analysis.

## Flamegraph Profiling

The `turbine-bench` crate includes flamegraph-compatible binaries for profiling hot paths. See [benchmarks.md](benchmarks.md) for setup instructions, recorded flamegraph results, and analysis.
