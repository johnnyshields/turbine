# User Guide

## Prerequisites

- **Linux** (or WSL2) -- Turbine uses `mmap`, `munmap`, `madvise`, and
  `io_uring` via the `io-uring` crate. It will not compile on macOS or Windows
  natively.
- **Rust stable** (1.80+)

## Creating a Pool

```rust
use turbine::prelude::*;

let config = PoolConfig::default(); // 4 arenas x 2 MiB
let pool = IouringBufferPool::new(config, NoopHooks)?;
```

`PoolConfig::default()` gives you four 2 MiB arenas with 32 io_uring
registration slots. See [Configuration Reference](#configuration-reference)
for all fields and their defaults.

Each I/O thread needs its own pool -- `IouringBufferPool` is `!Send`.

## Leasing Buffers

### Basic lease

`pool.lease(size)` returns `Option<LeasedBuffer>`. It returns `None` if the
current arena does not have enough space:

```rust
let mut buf = pool.lease(4096).expect("arena has space");

// Write data into the buffer.
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

// Read data back.
let data = buf.as_slice();

// Copy out to a Vec<u8> if needed.
let copied = buf.copy_out();
```

### Auto-rotating lease (preferred)

`pool.lease_or_rotate(size)` is the preferred API for hot paths. If the
current arena is full, it automatically rotates to a new epoch and retries:

```rust
let mut buf = pool.lease_or_rotate(4096)?;
buf.as_mut_slice()[..11].copy_from_slice(b"hello world");
```

This returns an error only if rotation itself fails (e.g., arena limit
exceeded) or the requested size exceeds the entire arena capacity.

## Epoch Rotation and Collection

Turbine manages buffer memory through an epoch-based lifecycle:

**Writable** -- the current arena accepts new leases via `lease()`.

**Retired** -- the arena is read-only; existing leases remain valid but no new
allocations are made. The arena sits in the drain queue.

**Collected** -- all leases have been returned. The arena is recycled into the
free pool for reuse.

### Automatic rotation

`lease_or_rotate()` handles rotation transparently. When the current arena is
full, it retires the arena and activates a new one from the free pool (or
allocates a fresh arena if the free pool is empty).

### Manual rotation

Call `pool.rotate()` to force an epoch boundary -- for example, on a timer or
after a batch of submissions:

```rust
pool.rotate()?;
```

`rotate()` auto-collects draining arenas when the free pool is empty before
allocating a new arena. This means the pool is self-sustaining under normal
load without any explicit collection calls.

### Explicit collection (optional)

Explicit `pool.collect()` reclaims arenas sooner and reduces peak RSS, but is
not required for correctness:

```rust
let collected = pool.collect(); // returns number of arenas reclaimed
```

### Targeted collection

To reclaim a specific epoch's arena (if all its leases have been returned):

```rust
pool.collect_epoch(0)?;
```

This returns an error if the epoch still has outstanding leases.

### Shrinking the free pool (optional)

`pool.shrink()` releases excess free arenas (beyond `max_free_arenas`) back to
the OS via `munmap`:

```rust
let freed = pool.shrink();
```

This is only needed if memory pressure is a concern. Under normal operation,
the free pool self-regulates via `max_free_arenas`.

## Cross-Thread Transfer

`LeasedBuffer` is `!Send` -- it holds raw pointers into thread-local arena
memory and cannot cross thread boundaries directly. To send buffer data to
another thread, convert it to a `SendableBuffer`:

```rust
let mut buf = pool.lease_or_rotate(1024)?;
buf.as_mut_slice()[..3].copy_from_slice(b"hey");

// Convert to SendableBuffer (consumes the LeasedBuffer).
let sendable = buf.into_sendable();

// SendableBuffer is Send -- ship it to another thread.
std::thread::spawn(move || {
    let data = unsafe { sendable.as_slice() };
    assert_eq!(&data[..3], b"hey");
    // sendable is dropped here -- atomic fetch_add releases the lease
});

// Back on the pool thread, collect reclaims arenas with zero leases.
pool.collect();
```

### How it works

`into_sendable()` uses `ManuallyDrop` to transfer lease ownership without
double-decrement. The `LeasedBuffer` is consumed without calling its `Drop`
(which would locally decrement the lease count). Instead, ownership transfers
to the `SendableBuffer`.

When the `SendableBuffer` is dropped on the remote thread, it atomically
increments the arena's `remote_returns` counter via `fetch_add(1, Release)`.
The pool thread sees this on its next `collect()` call and, once all leases
are accounted for, reclaims the arena.

## io_uring Registration

Register all arenas as fixed buffers before entering the hot path. This
enables `IORING_OP_WRITE_FIXED` and `IORING_OP_READ_FIXED` -- kernel-side
zero-copy I/O:

```rust
use io_uring::IoUring;

let ring = IoUring::new(256)?;

// Register all arenas as fixed buffers.
pool.register(&ring)?;
```

### PinnedWrite for SQE construction

Pin a leased buffer for io_uring submission. The `PinnedWrite` guard borrows
the buffer mutably, preventing it from being dropped while I/O is in flight:

```rust
let mut buf = pool.lease_or_rotate(4096)?;
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

let pinned = buf.pin_for_write();
let slot = pinned.buf_index(); // SlotId for IORING_OP_WRITE_FIXED

// Use slot.as_u16() when constructing the SQE:
// let sqe = opcode::WriteFixed::new(fd, pinned.as_ptr(), pinned.len() as u32, slot.as_u16());
```

### Unregistering

When you are done with io_uring fixed-buffer operations:

```rust
pool.unregister(&ring)?;
```

### Performance note

Without `pool.register()`, every `lease()` call falls through to
`slot_missing_fallback` -- a `#[cold]` path that returns `SlotId(0)`. This is
functionally correct but adds measurable overhead visible in flamegraphs.
Always register before entering the hot path.

## Hooks

Turbine provides two hook traits for observing buffer and epoch lifecycle
events. Both traits receive `&self`, so use `Cell` for interior mutability
(the pool is single-threaded).

### BufferPinHook

Called each time a buffer is leased:

```rust
trait BufferPinHook {
    fn on_pin(&self, epoch: u64, buf_id: u32);
}
```

### EpochObserver

Called on epoch transitions and arena lifecycle events:

```rust
trait EpochObserver {
    /// Epoch rotated. `retired` is now read-only; `active` is writable.
    fn on_rotate(&self, retired: u64, active: u64);

    /// A retired epoch's arena was reclaimed.
    fn on_collect(&self, epoch: u64);

    /// A new arena was allocated. Default: no-op.
    fn on_arena_alloc(&self, arena_idx: ArenaIdx) { }

    /// An arena was freed (munmapped). Default: no-op.
    fn on_arena_free(&self, arena_idx: ArenaIdx) { }

    /// Called after a collect sweep with the count of arenas collected. Default: no-op.
    fn on_collect_sweep(&self, collected: usize) { }
}
```

### NoopHooks

For standalone use when you do not need any hook callbacks:

```rust
let pool = IouringBufferPool::new(config, NoopHooks)?;
```

### Custom hooks example

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

    fn on_arena_alloc(&self, arena_idx: ArenaIdx) {
        tracing::debug!(%arena_idx, "arena allocated");
    }

    fn on_arena_free(&self, arena_idx: ArenaIdx) {
        tracing::debug!(%arena_idx, "arena freed");
    }

    fn on_collect_sweep(&self, collected: usize) {
        metrics::counter!("turbine.arenas.collected").increment(collected as u64);
    }
}

let pool = IouringBufferPool::new(config, MyHooks)?;
```

## Configuration Reference

| Parameter | Default | Description |
|-----------|---------|-------------|
| `arena_size` | 2 MiB | Size of each mmap'd arena. Must be a multiple of `page_size`. |
| `initial_arenas` | 4 | Arenas allocated at startup. Minimum 1. |
| `max_free_arenas` | 4 | Max arenas kept in the free pool before munmapping. |
| `max_total_arenas` | 0 | Hard cap on total arenas. 0 = unlimited. |
| `registration_slots` | 32 | Pre-allocated io_uring fixed-buffer slots. Must be >= `initial_arenas`. |
| `page_size` | 4096 | mmap page alignment. Must be a power of two. |

### Example: high-throughput server

```rust
let config = PoolConfig {
    arena_size: 4 * 1024 * 1024,  // 4 MiB
    initial_arenas: 8,
    max_free_arenas: 8,
    max_total_arenas: 0,
    registration_slots: 32,
    page_size: 4096,
};
```

### Example: constrained environment

```rust
let config = PoolConfig {
    arena_size: 64 * 1024,        // 64 KiB
    initial_arenas: 2,
    max_free_arenas: 2,
    max_total_arenas: 4,
    registration_slots: 8,
    page_size: 4096,
};
```

## Tuning

### Static Tuning

Start by matching config to your expected I/O pattern:

- **High-throughput, small buffers** (e.g., 64--256 byte network packets) --
  smaller arenas (512 KiB) work well because each allocation is tiny and many
  buffers fit per arena. Rotation is infrequent.

- **Large-buffer workloads** (e.g., 64 KiB disk reads) -- larger arenas
  (4--8 MiB) avoid rotating on every handful of leases.

- **Bursty traffic** -- increase `max_free_arenas` so that traffic spikes
  recycle arenas from the free pool instead of hitting mmap/munmap.

Use `initial_arenas = ceil(avg_in_flight_buffers * avg_buffer_size / arena_size) + 1`
as a starting point for steady-state concurrency.

### Dynamic Tuning

Turbine does not auto-resize arenas at runtime (arena size is fixed at
allocation time), but you can build adaptive behavior on top of the existing
API using `EpochObserver` hooks and pool metrics.

#### Monitoring with AdaptiveMetrics

`EpochObserver` receives `&self`, so use `Cell` for interior mutability:

```rust
use std::cell::Cell;
use std::time::Instant;

struct AdaptiveMetrics {
    rotations: Cell<u64>,
    collections: Cell<u64>,
    last_check: Cell<Instant>,
}

impl BufferPinHook for AdaptiveMetrics {
    fn on_pin(&self, _epoch: u64, _buf_id: u32) {}
}

impl EpochObserver for AdaptiveMetrics {
    fn on_rotate(&self, _retired: u64, _active: u64) {
        self.rotations.set(self.rotations.get() + 1);
        // High rotation rate --> arenas are filling quickly -->
        //   consider larger arenas or more aggressive pre-allocation.
    }

    fn on_collect(&self, _epoch: u64) {
        self.collections.set(self.collections.get() + 1);
        // If collections lag behind rotations --> leases are long-lived -->
        //   increase max_free_arenas to buffer the drain queue.
    }

    fn on_collect_sweep(&self, _collected: usize) {
        // collected == 0 means no arenas were reclaimable.
        // Persistently zero --> leases are very long-lived -->
        //   consider larger arenas or reviewing lease lifetimes.
    }
}
```

#### Pool metrics

Inspect pool state directly:

- **`pool.available()`** -- bytes remaining in the current write arena. If this
  is consistently high at rotation time, your arenas are oversized.
- **`pool.draining_count()`** -- arenas waiting to be collected. A growing
  drain queue means collection frequency is too low or leases are too long-lived.
- **`pool.epoch()`** -- current epoch number.

#### Adaptive rotation

Instead of rotating on a fixed timer, adapt the rotation trigger based on
arena fill level:

```rust
fn maybe_rotate(
    pool: &IouringBufferPool<impl BufferPinHook + EpochObserver>,
    arena_size: usize,
) -> Result<()> {
    // Rotate when the current arena is >75% full.
    // This avoids both premature rotation (wasted capacity)
    // and late rotation (allocation failure).
    let utilization = 1.0 - (pool.available() as f64 / arena_size as f64);

    if utilization > 0.75 {
        pool.rotate()?;
        pool.collect();
    }
    Ok(())
}
```

#### Adaptive pool rebuilding

For long-lived servers where workload characteristics shift over time (e.g.,
day vs. night traffic patterns), you can periodically rebuild the pool with
updated parameters:

1. Observe metrics over a window (e.g., 60 seconds).
2. Compute ideal `arena_size` and `max_free_arenas` from observed rotation
   rate and drain queue depth.
3. On the next quiet period, create a new `IouringBufferPool` with updated
   `PoolConfig`, re-register with io_uring, and drain the old pool.

This is a heavier operation (requires unregister + re-register) and should
only be done infrequently -- once per minute at most, not per-request.

#### What you cannot change dynamically

- **Arena size** is fixed at mmap time. Existing arenas keep their original
  size. New arenas allocated after a config change will use the new size, but
  only if you rebuild the pool.
- **Registration slots** are fixed at `register()` time. Growing beyond the
  initial slot count requires unregister + re-register.

Dynamic tuning is primarily about adjusting **rotation frequency**, **collection
frequency**, and **free pool depth** -- not resizing arenas in place.

### Build Optimization

Turbine ships with `lto = "thin"` and `codegen-units = 1` in its release
profile. These are inherited automatically when you depend on Turbine.

#### PGO (profile-guided optimization)

PGO yields an additional 27--36% improvement on the `rotate()` and `collect()`
hot paths by optimizing branch layout and inlining decisions based on actual
runtime behavior.

PGO is a binary-level optimization -- it must be applied in the **downstream
project** that depends on Turbine, not in Turbine itself. When the downstream
binary is built with PGO, Turbine's code benefits automatically via thin-LTO
cross-crate inlining.

##### PGO build steps

```bash
# 1. Build with instrumentation
RUSTFLAGS="-Cprofile-generate=target/pgo-data" \
  cargo build --release

# 2. Run representative workload to generate profile data
./target/release/your-binary  # exercise the hot paths

# 3. Merge profile data
llvm-profdata merge -o target/pgo-data/merged.profdata \
  target/pgo-data/*.profraw

# 4. Rebuild with profile data
RUSTFLAGS="-Cprofile-use=$(pwd)/target/pgo-data/merged.profdata" \
  cargo build --release
```

`llvm-profdata` ships with `rustup component add llvm-tools`. The path is:

```
$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata
```

##### Measured impact

Benchmarked on the Turbine flamegraph suite (WSL2, Rust 1.94):

| Hot path | Baseline (thin-LTO) | + PGO | Improvement |
|----------|---------------------|-------|-------------|
| `lease()` | 2.1 ns | 2.1 ns | -- |
| `rotate()` | 3.3 ns | 2.4 ns | **27%** |
| `collect()` | 49.0 ns | 31.5 ns | **36%** |
| SPSC transfer | 89.6 ns | 100.2 ns | noise |

The `lease()` path is already fully inlined and branch-free, so PGO has no
effect. `rotate()` and `collect()` have conditional branches (drain queue
scanning, arena state checks) where PGO's branch layout optimization pays off.

##### When to use PGO

PGO is worth the build complexity when:

- Your workload is I/O-heavy with frequent epoch rotation and collection.
- You are running a long-lived server where build time is amortized.
- You want to squeeze the last ~30% out of buffer lifecycle management.

For applications where `lease()` dominates (high-throughput, infrequent
rotation), PGO provides no measurable benefit -- the hot path is already
optimal.

## Pool Metrics

- **`pool.epoch()`** -- current epoch number.
- **`pool.available()`** -- bytes remaining in the current arena.
- **`pool.draining_count()`** -- arenas in the drain queue awaiting collection.

## Known Limitations

- **Arena sizing is static.** Each arena's size is fixed at mmap time. You
  cannot resize an existing arena -- rebuild the pool to change arena sizes.

- **One slow lease pins an entire arena.** A single long-lived `LeasedBuffer`
  prevents its arena from being collected, even if all other leases in that
  epoch have been returned. This is by design (the arena memory must remain
  valid while any lease exists), but it means a stalled consumer can hold
  memory indefinitely.

- **Registration is static.** The io_uring fixed-buffer slot count is fixed at
  `register()` time. Adding more slots requires `unregister()` followed by
  `register()` with a new slot count.
