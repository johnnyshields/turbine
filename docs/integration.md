# Integration Guide

Turbine is a buffer allocator, not a runtime. It is designed to slot
underneath an existing async runtime or custom event loop as the buffer
management layer.

## Integration Checklist

1. **Create one pool per thread** — `IouringBufferPool` is `!Send`; each I/O thread needs its own
2. **Register with io_uring** — call `pool.register(&ring)` before submitting fixed-buffer ops
3. **Lease buffers** — `pool.lease_or_rotate(size)` in the hot path (auto-rotates when the arena is full)
4. **Pin for submission** — `buf.pin_for_write()` to get the `buf_index` for SQEs
5. **Transfer cross-thread** — `buf.into_sendable()` when sending buffer data between threads
6. **Collect periodically** *(optional)* — `rotate()` auto-collects when the free pool is empty, but explicit `pool.collect()` calls reclaim arenas sooner and reduce peak RSS
7. **Shrink occasionally** *(optional)* — `pool.shrink()` releases excess free arenas back to the OS; only needed if memory pressure is a concern
8. **Tune arena parameters** — match `arena_size`, `initial_arenas`, and `max_free_arenas` to your workload; consider adaptive rotation (see [Tuning Arena Parameters](#tuning-arena-parameters))
9. **Enable thin-LTO** — add `lto = "thin"` and `codegen-units = 1` to your release profile
10. **Consider PGO** — if `rotate()`/`collect()` are hot, PGO yields ~30% improvement (see [Build Optimization](#build-optimization-pgo--thin-lto))

## With Compio

[Compio](https://github.com/compio-rs/compio) is the most natural fit.
It is the only actively maintained completion-based Rust runtime that is
cross-platform (io_uring + IOCP + polling fallback) and has a **decoupled
driver architecture** -- the I/O driver can be used independently of the
async executor.

### Why Compio + Turbine

Compio currently boxes every I/O request and transfers buffer ownership
per-operation. This is sound and ergonomic but measurable at extreme
throughput. Turbine replaces per-operation allocation with epoch-rotated
arenas:

| | Compio alone | Compio + Turbine |
|---|---|---|
| Buffer allocation | Heap box per I/O op | Bump alloc (~19 ns, constant) |
| Fixed-buffer support | Partial (provided rings) | Full `IORING_REGISTER_BUFFERS` |
| Buffer reclamation | Per-operation dealloc | Epoch-scoped bulk collect |
| Cancellation cleanup | Graveyard pattern | Epoch scoping (arena freed in bulk) |

### Integration Approach

Compio's driver-executor separation means you can use `compio-driver`
directly:

```rust
use compio_driver::IoUringDriver;
use turbine::prelude::*;

// Create the Turbine pool.
let config = PoolConfig::default();
let pool = IouringBufferPool::new(config, NoopHooks)?;

// Register fixed buffers with the io_uring ring.
// (Requires access to the underlying IoUring instance.)
pool.register(&ring)?;

// In your event loop:
loop {
    // Lease a buffer for the next I/O operation.
    let mut buf = pool.lease_or_rotate(4096)?;

    // Pin for io_uring submission.
    let pinned = buf.pin_for_write();
    let slot = pinned.buf_index(); // Use as buf_index in SQE

    // Submit SQE with IORING_OP_WRITE_FIXED using slot...

    // After completion, buf is dropped (lease released) or
    // converted to SendableBuffer for cross-thread processing.
}
```

This avoids forking Compio -- you use its driver for I/O submission and
Turbine for buffer management.

### Key Considerations

- **Registration timing:** Call `pool.register()` before submitting any
  fixed-buffer operations. Registration is static -- dynamic arena growth
  after registration requires unregister + re-register.

- **Epoch rotation:** Use `pool.lease_or_rotate()` in the hot path — it
  auto-rotates when the current arena is full. Use manual `pool.rotate()`
  only if you want explicit epoch boundaries (e.g., on a timer).

- **Collection:** `rotate()` auto-collects when the free pool is empty.
  Explicit `pool.collect()` calls reclaim arenas sooner and reduce peak RSS,
  but are not required for correctness.

- **Registration and performance:** Without `pool.register()`, every `lease()`
  call falls through to `slot_missing_fallback` — a `#[cold]` path that returns
  `SlotId(0)`. This is functionally correct but adds ~1% overhead visible in
  flamegraphs. Always register with io_uring before entering the hot path.

## With a Custom Event Loop

If you are writing your own event loop directly on `io-uring`:

```rust
use io_uring::{IoUring, opcode, types};
use turbine::prelude::*;

let mut ring = IoUring::new(256)?;
let pool = IouringBufferPool::new(PoolConfig::default(), NoopHooks)?;

// Register all arena buffers as fixed buffers.
pool.register(&ring)?;

// Lease and pin a buffer for a write operation.
// lease_or_rotate() auto-rotates if the current arena is full.
let mut buf = pool.lease_or_rotate(4096)?;
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

let pinned = buf.pin_for_write();
let write_op = opcode::WriteFixed::new(
    types::Fd(fd),
    pinned.as_ptr(),
    pinned.len() as u32,
    pinned.buf_index().as_u16(),
);

// Submit and wait for completion...

// After completion, drop the PinnedWrite, then the LeasedBuffer.
drop(pinned);
drop(buf);

// Explicit rotate + collect (optional if using lease_or_rotate).
pool.rotate()?;
pool.collect();
```

## With a BEAM-like Runtime

Turbine was designed with BEAM-style concurrency in mind. Runtimes like
[Rebar](https://github.com/alexandernicholson/rebar) -- a BEAM-inspired
distributed actor runtime for Rust -- are a natural fit.
The key integration points:

### Per-Scheduler-Thread Pools

Each scheduler thread owns its own `IouringBufferPool` (the pool is `!Send`).
Processes running on that thread lease buffers locally with zero contention.

```
Scheduler Thread 0          Scheduler Thread 1
 ├── IouringBufferPool       ├── IouringBufferPool
 ├── io_uring ring            ├── io_uring ring
 └── Process mailboxes       └── Process mailboxes
```

### Cross-Thread Message Passing

When a process sends buffer data to a process on another scheduler thread:

1. The sender calls `buf.into_sendable()` to create a `SendableBuffer`.
2. The `SendableBuffer` is sent via the inter-scheduler message channel.
3. The receiving process reads the data via `unsafe { sendable.as_slice() }`.
4. When the receiving process is done, it drops the `SendableBuffer`.
5. The originating thread's pool reclaims the arena on its next `collect()`.

### GC Integration

Implement `BufferPinHook` to track which processes hold buffer references:

```rust
impl BufferPinHook for SchedulerHooks {
    fn on_pin(&self, epoch: u64, buf_id: u32) {
        // Record that the current process holds a buffer lease.
        // Block GC of this process's heap while I/O is in flight.
    }
}
```

Implement `EpochObserver` to coordinate epoch transitions with the scheduler:

```rust
impl EpochObserver for SchedulerHooks {
    fn on_rotate(&self, retired: u64, active: u64) {
        // Epoch boundary: a natural GC point.
        // Processes with no in-flight I/O in the retired epoch
        // can be collected immediately.
    }

    fn on_collect(&self, epoch: u64) {
        // Arena reclaimed: all I/O for this epoch is complete.
    }
}
```

## With Monoio or Glommio

These are thread-per-core runtimes with their own io_uring integration.
Turbine can replace their buffer allocation path, but integration is more
involved because they manage their own rings internally.

The practical approach: use Turbine's `Arena` and `ArenaManager` directly
(bypassing `IouringBufferPool`) and integrate with the runtime's ring
management. This requires deeper coupling with the runtime's internals.

## Build Optimization: PGO + thin-LTO

Turbine's `Cargo.toml` already enables `lto = "thin"` and `codegen-units = 1`
for release builds. However, **profile-guided optimization (PGO)** can yield an
additional 25-35% improvement on the `rotate()` and `collect()` hot paths by
optimizing branch layout and inlining decisions based on actual runtime behavior.

PGO is a binary-level optimization — it must be applied in the **downstream
project** that depends on Turbine, not in Turbine itself. When the downstream
binary is built with PGO, Turbine's code benefits automatically via thin-LTO
cross-crate inlining.

### PGO Build Steps

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

`llvm-profdata` ships with `rustup component add llvm-tools`. The path is
`$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata`.

### Measured Impact

Benchmarked on the Turbine flamegraph suite (WSL2, Rust 1.94):

| Hot path | Baseline (thin-LTO) | + PGO | Improvement |
|----------|---------------------|-------|-------------|
| `lease()` | 2.1 ns | 2.1 ns | — |
| `rotate()` | 3.3 ns | 2.4 ns | **27%** |
| `collect()` | 49.0 ns | 31.5 ns | **36%** |
| SPSC transfer | 89.6 ns | 100.2 ns | noise |

The `lease()` path is already fully inlined and branch-free, so PGO has no
effect. `rotate()` and `collect()` have conditional branches (drain queue
scanning, arena state checks) where PGO's branch layout optimization pays off.

### When to Use PGO

PGO is worth the build complexity when:
- Your workload is I/O-heavy with frequent epoch rotation and collection
- You are running a long-lived server where build time is amortized
- You want to squeeze the last ~30% out of buffer lifecycle management

For applications where `lease()` dominates (high-throughput, infrequent rotation),
PGO provides no measurable benefit — the hot path is already optimal.

## Tuning Arena Parameters

Turbine's `PoolConfig` controls four key dimensions. Choosing the right values
depends on your workload shape — and the optimal values can change at runtime.

### Static Tuning

Start by matching the defaults to your expected I/O pattern:

| Parameter | Default | Tune when… |
|-----------|---------|------------|
| `arena_size` | 2 MiB | Buffers are consistently small (reduce to 256 KiB–512 KiB) or consistently large (increase to 4–8 MiB). Oversized arenas waste RSS; undersized arenas cause frequent rotation. |
| `initial_arenas` | 4 | You know your steady-state concurrency. Set to `ceil(avg_in_flight_buffers × avg_buffer_size / arena_size) + 1`. |
| `max_free_arenas` | 4 | Memory-constrained environments → lower (1–2). Bursty workloads → higher (8+) to absorb spikes without mmap/munmap churn. |
| `max_total_arenas` | 0 (unlimited) | Set a hard cap in multi-tenant or shared-memory environments to prevent a single pool from consuming unbounded RSS. |

**Rules of thumb:**

- **High-throughput, small buffers** (e.g., 64–256 byte network packets): smaller arenas (512 KiB) rotate less frequently because each allocation is tiny. Many buffers fit per arena.
- **Large-buffer workloads** (e.g., 64 KiB disk reads): larger arenas (4–8 MiB) avoid rotating on every handful of leases.
- **Bursty traffic**: increase `max_free_arenas` so that traffic spikes recycle arenas from the free pool instead of hitting mmap.

### Dynamic Tuning

Turbine doesn't auto-resize arenas at runtime (arena size is fixed at allocation
time), but you can build adaptive behavior on top of the existing API using the
`EpochObserver` hooks and pool metrics.

The idea: monitor rotation frequency and arena utilization, then adjust your
rotation strategy and pool parameters for newly allocated arenas.

#### Signals to Monitor

```rust
use std::cell::Cell;
use std::time::Instant;

// EpochObserver receives &self, so use Cell for interior mutability.
struct AdaptiveMetrics {
    rotations: Cell<u64>,
    collections: Cell<u64>,
    last_check: Cell<Instant>,
}

impl EpochObserver for AdaptiveMetrics {
    fn on_rotate(&self, _retired: u64, _active: u64) {
        self.rotations.set(self.rotations.get() + 1);
        // High rotation rate → arenas are filling quickly →
        //   consider larger arenas or more aggressive pre-allocation.
    }

    fn on_collect(&self, _epoch: u64) {
        self.collections.set(self.collections.get() + 1);
        // If collections lag behind rotations → leases are long-lived →
        //   increase max_free_arenas to buffer the drain queue.
    }

    fn on_collect_sweep(&self, collected: usize) {
        // collected == 0 means no arenas were reclaimable.
        // Persistently zero → leases are very long-lived →
        //   consider larger arenas or reviewing lease lifetimes.
    }
}
```

You can also inspect pool state directly via `IouringBufferPool` methods:

- **`pool.available()`** — bytes remaining in the current write arena. If this
  is consistently high at rotation time, your arenas are oversized.
- **`pool.draining_count()`** — arenas waiting to be collected. A growing
  drain queue means collection frequency is too low or leases are too long-lived.

#### Adaptive Rotation Strategy

Instead of rotating on a fixed timer or fixed completion count, adapt the
rotation trigger based on arena fill level:

```rust
fn maybe_rotate(pool: &IouringBufferPool<MyHooks>, arena_size: usize) -> Result<()> {
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

#### Adaptive Pool Rebuilding

For long-lived servers where workload characteristics shift over time (e.g.,
day vs. night traffic patterns), you can periodically rebuild the pool with
updated parameters:

1. Observe metrics over a window (e.g., 60 seconds).
2. Compute ideal `arena_size` and `max_free_arenas` from observed rotation
   rate and drain queue depth.
3. On the next quiet period, create a new `IouringBufferPool` with updated
   `PoolConfig`, re-register with io_uring, and drain the old pool.

This is a heavier operation (requires unregister + re-register) and should only
be done infrequently — think once per minute at most, not per-request.

#### What You Cannot Change Dynamically

- **Arena size** is fixed at mmap time. Existing arenas keep their original size.
  New arenas allocated after a config change will use the new size, but only if
  you rebuild the pool.
- **Registration slots** are fixed at `register()` time. Growing beyond the
  initial slot count requires unregister + re-register.

This means dynamic tuning is primarily about adjusting **rotation frequency**,
**collection frequency**, and **free pool depth** — not resizing arenas in place.

## General Integration Pattern

Regardless of the runtime, the integration follows the same pattern — see the
[Integration Checklist](#integration-checklist) at the top of this document.
