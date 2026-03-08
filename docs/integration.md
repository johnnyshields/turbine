# Integration Guide

Turbine is a buffer allocator, not a runtime. It is designed to slot
underneath an existing async runtime or custom event loop as the buffer
management layer.

## Integration Checklist

1. **Create one pool per thread** — `IouringBufferPool` is `!Send`; each I/O thread needs its own
2. **Register with io_uring** — call `pool.register(&ring)` before submitting fixed-buffer ops
3. **Lease buffers** — `pool.lease(size)` or `pool.lease_or_rotate(size)` in the hot path
4. **Pin for submission** — `buf.pin_for_write()` to get the `buf_index` for SQEs
5. **Transfer cross-thread** — `buf.into_sendable()` when sending buffer data between threads
6. **Rotate periodically** — `pool.rotate()` every N completions or on a timer
7. **Collect after rotation** — `pool.collect()` to reclaim arenas with zero outstanding leases
8. **Shrink occasionally** — `pool.shrink()` to release excess free arenas back to the OS
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

- **Epoch rotation:** Call `pool.rotate()` periodically (e.g., every N
  completions or on a timer). This retires the current arena and activates
  a fresh one.

- **Collection:** Call `pool.collect()` after rotation to reclaim arenas
  whose leases have all been returned. This is required for arena recycling.

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
let mut buf = pool.lease(4096).unwrap();
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

// Rotate and collect periodically.
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

## General Integration Pattern

Regardless of the runtime, the integration follows the same pattern:

1. **Create one pool per thread** (pool is `!Send`).
2. **Register with io_uring** via `pool.register(&ring)`.
3. **Lease buffers** in the I/O hot path via `pool.lease()`.
4. **Pin for SQE submission** via `buf.pin_for_write()`.
5. **Transfer cross-thread** via `buf.into_sendable()` when needed.
6. **Rotate periodically** via `pool.rotate()`.
7. **Collect periodically** via `pool.collect()`.
8. **Shrink occasionally** via `pool.shrink()` to release excess memory.
