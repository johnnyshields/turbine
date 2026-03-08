# Turbine

Epoch-based buffer rotation for io_uring.

**Status:** Early (v0.1.0) | **Platform:** Linux only | **License:** MIT

## Why Turbine?

Most Rust io_uring runtimes handle scheduling and submission but leave buffer
allocation to the application. Turbine replaces per-operation heap allocation
with epoch-rotated mmap'd arenas -- zero-contention bump alloc for the hot path,
explicit cross-thread transfer via a split-counter atomic lease. See
[Architecture](docs/architecture.md) for the full design.

| | Turbine | Typical runtime |
|---|---|---|
| **Alloc cost** | 1 branch + 1 store (bump) | Heap box per I/O op |
| **Contention** | Zero (thread-local `Cell`) | Allocator-dependent |
| **Fixed-buffer reg** | Full `IORING_REGISTER_BUFFERS` | Partial or none |
| **Cross-thread** | 1 atomic op (split counter) | Channel or Arc+Mutex |
| **Reclamation** | Epoch-scoped bulk collect | Per-buffer dealloc |

Turbine is not a runtime. It is designed to slot underneath one (Compio's
decoupled driver, a custom event loop, a BEAM-like scheduler) as the buffer
management layer.

## Quick Start

```rust
use turbine::prelude::*;

let config = PoolConfig::default(); // 4 arenas x 2 MiB
let mut pool = IouringBufferPool::new(config, NoopHooks)?;

// Lease a buffer from the current epoch's arena (bump alloc).
// Automatically rotates to a new arena if the current one is full.
let mut buf = pool.lease_or_rotate(4096)?;
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

// Rotate to a new epoch -- the old arena becomes read-only.
pool.rotate()?;

// Once all leases from epoch 0 are returned, reclaim its memory.
pool.collect_epoch(0)?;
```

## Cross-Thread Transfer

Buffers are `!Send` by design -- they hold raw pointers into thread-local
arenas. To send data to another thread, convert a lease into a `SendableBuffer`:

```rust
let sendable = buf.into_sendable();

// `sendable` is Send -- ship it to another thread.
// When dropped, it atomically decrements the arena's lease count.
std::thread::spawn(move || {
    let data = unsafe { sendable.as_slice() };
    // ... process data ...
}); // drop atomically releases the lease

// On the pool's thread, collect reclaims arenas with zero leases.
pool.collect();
```

## Crates

- **turbine-core** -- arenas, epochs, io_uring registration, cross-thread transfer
- **turbine** -- facade re-exporting the public API

## Configuration

```rust
PoolConfig {
    arena_size: 2 * 1024 * 1024,  // 2 MiB per arena
    initial_arenas: 4,             // 4 arenas at startup
    max_free_arenas: 4,            // max arenas kept in free pool
    max_total_arenas: 0,           // 0 = unlimited
    registration_slots: 32,        // io_uring fixed-buffer slots
    page_size: 4096,               // mmap page alignment
}
```

Default config: 4 arenas x 2 MiB = 8 MiB total. See
[User Guide](docs/guide.md) for tuning advice.

## Known Limitations

- **Arena sizing is static.** Variable I/O burst sizes may cause `ArenaFull`
  or waste memory. Adaptive rotation is future work.
- **One slow lease pins an entire arena.** Classic epoch-based reclamation
  weakness -- a single long-lived buffer blocks collection of its whole arena.
- **Registration is static.** `register_buffers()` is called once. Dynamic
  resizing requires unregister + re-register, which stalls the ring.

## Documentation

- [User Guide](docs/guide.md) -- Setup, API usage, configuration, tuning
- [Integration Guide](docs/integration.md) -- Compio, custom event loops, BEAM runtimes
- [Architecture](docs/architecture.md) -- Internals, buffer types, soundness
- [Benchmarks](docs/benchmarks.md) -- Performance data, flamegraphs, profiling
- [Contributing](docs/contributing.md) -- Building, testing, project layout
- [Future Work](docs/future.md) -- Planned improvements
