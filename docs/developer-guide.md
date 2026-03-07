# Developer Guide

## Prerequisites

- **Linux** (or WSL2) -- Turbine uses `mmap`, `munmap`, `madvise`, and
  `io_uring` via the `io-uring` crate. It will not compile on macOS or Windows
  natively.
- **Rust stable** (1.80+)

## Building and Testing

```bash
# Run all tests
cargo test --workspace

# Run clippy
cargo clippy --workspace

# Run benchmarks (requires Linux, takes several minutes)
cargo bench -p turbine-bench

# Compile-check benchmarks without running
cargo bench -p turbine-bench --no-run
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
      epoch_lifecycle.rs
    src/                  # Competitor implementations for comparison
docs/                    # Documentation
lore/                    # Implementation notes and research
```

## Basic Usage

### Creating a Pool

```rust
use turbine::prelude::*;

let config = PoolConfig {
    arena_size: 2 * 1024 * 1024,  // 2 MiB per arena
    initial_arenas: 4,
    max_free_arenas: 4,
    max_total_arenas: 0,           // unlimited
    registration_slots: 32,
    page_size: 4096,
};
let pool = IouringBufferPool::new(config, NoopHooks)?;
```

### Leasing Buffers

```rust
// Lease bytes from the current epoch's arena.
let mut buf = pool.lease(4096).expect("arena has space");

// Write data into the buffer.
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

// Read data back.
let data = buf.as_slice();
let copied = buf.copy_out(); // Vec<u8>
```

`lease()` returns `None` if the current arena is full. Use `lease_or_rotate()`
to automatically rotate to a new epoch:

```rust
let buf = pool.lease_or_rotate(4096)?;
```

### Epoch Rotation

```rust
// Rotate: retire the current arena, activate a new one.
pool.rotate()?;

// The old arena is now Retired. Once all its leases are dropped:
pool.collect_epoch(0)?; // reclaim epoch 0's arena

// Or collect all reclaimable arenas at once:
let collected = pool.collect();
```

### Cross-Thread Transfer

`LeasedBuffer` is `!Send` -- it cannot cross thread boundaries. To send
buffer data to another thread:

```rust
let buf = pool.lease(1024).unwrap();
buf.as_mut_slice()[..3].copy_from_slice(b"hey");

// Convert to SendableBuffer (consumes the LeasedBuffer).
let sendable = buf.into_sendable();

// SendableBuffer is Send -- ship it.
std::thread::spawn(move || {
    let data = unsafe { sendable.as_slice() };
    assert_eq!(&data[..3], b"hey");
    // sendable is dropped here -- atomic fetch_add releases the lease
});

// On the pool thread, collect reclaims arenas with zero leases.
pool.collect();
```

The `into_sendable()` call uses `ManuallyDrop` to transfer lease ownership
without double-decrement. When the `SendableBuffer` drops on the remote
thread, it increments the arena's `remote_returns` atomic counter. The pool
thread sees this when it calls `collect()`.

### io_uring Registration

```rust
use io_uring::IoUring;

let ring = IoUring::new(256)?;

// Register all arenas as fixed buffers.
pool.register(&ring)?;

// Use PinnedWrite for SQE construction.
let mut buf = pool.lease(4096).unwrap();
let pinned = buf.pin_for_write();
let slot_id = pinned.buf_index(); // SlotId for IORING_OP_WRITE_FIXED

// Unregister when done.
pool.unregister(&ring)?;
```

### Custom Hooks

Implement `BufferPinHook` and `EpochObserver` to integrate with your
application's metrics or GC:

```rust
struct MyHooks;

impl BufferPinHook for MyHooks {
    fn on_pin(&self, epoch: u64, buf_id: u32) {
        tracing::debug!(epoch, buf_id, "buffer leased");
    }
}

impl EpochObserver for MyHooks {
    fn on_rotate(&self, retired: u64, active: u64) {
        tracing::info!(retired, active, "epoch rotated");
    }

    fn on_collect(&self, epoch: u64) {
        tracing::info!(epoch, "epoch collected");
    }

    fn on_collect_sweep(&self, collected: usize) {
        metrics::counter!("turbine.arenas.collected").increment(collected as u64);
    }
}

let pool = IouringBufferPool::new(config, MyHooks)?;
```

### Memory Management

```rust
// Shrink the free pool (munmap excess arenas beyond max_free_arenas).
let freed = pool.shrink();

// Check pool state.
let epoch = pool.epoch();
let available = pool.available();
let draining = pool.draining_count();
```

## Configuration Tuning

| Parameter | Default | Guidance |
|-----------|---------|----------|
| `arena_size` | 2 MiB | Match your typical I/O batch size. Larger arenas reduce rotation frequency but waste memory if underutilized. |
| `initial_arenas` | 4 | Minimum 1. More arenas = more pipeline depth before needing to collect. |
| `max_free_arenas` | 4 | Caps memory held in the free pool. Lower = less RSS, more frequent mmap/munmap. |
| `max_total_arenas` | 0 (unlimited) | Set to bound total memory. Rotation fails with `ArenaLimitExceeded` when hit. |
| `registration_slots` | 32 | Must be >= `initial_arenas`. Max 64 (bitmap-based). |
| `page_size` | 4096 | Must match OS page size. `arena_size` must be a multiple. |

**Typical server config (high throughput):**
```rust
PoolConfig {
    arena_size: 4 * 1024 * 1024,  // 4 MiB
    initial_arenas: 8,
    max_free_arenas: 8,
    max_total_arenas: 0,
    registration_slots: 32,
    page_size: 4096,
}
```

**Constrained environment:**
```rust
PoolConfig {
    arena_size: 64 * 1024,        // 64 KiB
    initial_arenas: 2,
    max_free_arenas: 2,
    max_total_arenas: 4,
    registration_slots: 8,
    page_size: 4096,
}
```

## Safety Rules

If you are working on the turbine-core internals:

1. **Never collect an arena with outstanding leases.** `lease_count() > 0`
   means live pointers exist. Collection would cause use-after-free.

2. **`LeasedBuffer` must stay `!Send`.** It holds raw pointers into
   thread-local arena memory. The `PhantomData<Rc<()>>` marker enforces this.

3. **`SendableBuffer::new()` must stay `pub(crate)`.** External code must go
   through `into_sendable()` to prevent forging a `SendableBuffer` without
   proper lease transfer.

4. **Arenas must be `Box`-allocated in the slab.** Pointer stability is
   required for `SendableBuffer`'s raw pointers. A `Vec<Arena>` without `Box`
   would invalidate pointers on growth.

5. **`ManuallyDrop` in `into_sendable()` is load-bearing.** It prevents
   `LeasedBuffer::Drop` from running (which would locally decrement
   `lease_count`), transferring ownership to `SendableBuffer` instead.

6. **`remote_returns` ordering matters.** `fetch_add(Release)` on the writer
   thread pairs with `load(Acquire)` on the pool thread. Weakening either
   breaks the happens-before relationship and can cause premature collection.

## Running Benchmarks

```bash
# All benchmarks
cargo bench -p turbine-bench

# Specific groups
cargo bench -p turbine-bench -- lease_throughput
cargo bench -p turbine-bench -- cross_thread
cargo bench -p turbine-bench -- epoch_lifecycle
```

Results are written to `target/criterion/` with HTML reports (requires
gnuplot). See [benchmarks.md](benchmarks.md) for baseline numbers and
analysis.
