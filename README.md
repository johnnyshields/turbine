# Turbine

Epoch-based buffer rotation for io_uring.

Turbine pre-allocates mmap'd buffer arenas tied to scheduler epochs. Every N
microseconds the scheduler rotates to a new arena. In-flight I/O from the
previous epoch completes into the old arena (now read-only), and the current
epoch's arena uses append-only bump allocation -- no contention, no locking.

**Status:** Early (v0.1.0) | **Platform:** Linux only | **License:** MIT

## Why Turbine?

Most Rust io_uring runtimes (Compio, Monoio, Glommio) solve scheduling and I/O
submission but leave buffer allocation to the application. Per-operation heap
allocation and partial fixed-buffer support leave performance on the table at
extreme throughput.

| | Turbine | Typical runtime |
|---|---|---|
| **Alloc cost** | 1 branch + 1 store (bump) | Heap box per I/O op |
| **Contention** | Zero (thread-local `Cell`) | Allocator-dependent |
| **Fixed-buffer reg** | Full `IORING_REGISTER_BUFFERS` | Partial or none |
| **Reclamation** | Epoch-scoped bulk collect | Per-buffer dealloc |

Turbine is designed to slot underneath a runtime (Compio's decoupled driver, a
custom event loop, etc.) as the buffer management layer -- not to replace it.

## Crates

- **turbine-core** -- arenas, epochs, io_uring registration, cross-thread transfer
- **turbine** -- facade re-exporting the public API

## Quick Start

```rust
use turbine::prelude::*;

let config = PoolConfig::default(); // 4 arenas x 2 MiB
let mut pool = IouringBufferPool::new(config, NoopHooks)?;

// Lease a buffer from the current epoch's arena (bump alloc).
let mut buf = pool.lease(4096).expect("arena has space");
buf.as_mut_slice()[..5].copy_from_slice(b"hello");

// Rotate to a new epoch -- the old arena becomes read-only.
pool.rotate()?;

// Once all leases from epoch 0 are returned, reclaim its memory.
pool.try_collect(0)?;
```

## Cross-Thread Transfer

Buffers are `!Send` by design -- they hold raw pointers into thread-local
arenas. To send data to another thread, convert a lease into a `SendableBuffer`:

```rust
let handle = pool.transfer_handle();
let sendable = buf.into_sendable(&handle);

// `sendable` is Send -- ship it to another thread.
// When dropped, it notifies the owning thread via channel.
std::thread::spawn(move || {
    let data = unsafe { sendable.as_slice() };
    // ... process data ...
}); // drop sends ReturnedBuffer back

// On the pool's thread, drain returns to release leases.
pool.drain_returns();
```

## Architecture

```
IouringBufferPool
 ├── EpochClock (ring of N arenas)
 │    ├── Arena 0  [mmap, bump allocator, lease count]
 │    ├── Arena 1
 │    └── Arena N-1
 ├── RingRegistration (io_uring fixed-buffer iovecs)
 └── Channel (crossbeam, for cross-thread returns)
```

**Epoch lifecycle:** `Writable` -> `Retired` -> `Collected` -> (recycled as `Writable`)

Each arena is an mmap'd region with a bump allocator. Allocation is a single
offset increment (`Cell<usize>`) -- no atomics, no cache-line bouncing. Leases
are reference-counted per-arena; an arena cannot be recycled until all its leases
are returned.

## Safety Invariants

Turbine manages raw pointers into mmap'd memory. The following invariants
prevent use-after-free and data corruption:

1. **`rotate()` refuses to recycle arenas with live leases.** Returns
   `Err(EpochNotCollectable)` instead of warn-and-continue. This is the primary
   defense against writing into memory that in-flight I/O still references.

2. **Arena `Drop` panics on leaked leases in debug builds.** A `debug_assert`
   catches lease leaks during development; release builds log a warning.

3. **`LeasedBuffer` is `!Send`.** Enforced via `PhantomData<Rc<()>>`. Raw
   arena pointers must not cross thread boundaries. Cross-thread transfer
   requires explicit conversion via `into_sendable()`.

4. **`IouringBufferPool` is `!Send`.** The pool uses `Cell<usize>` for lease
   counts, which is unsound if accessed from multiple threads. The `!Send`
   marker prevents this at compile time.

5. **`SendableBuffer` construction is `pub(crate)`.** External code cannot
   forge a `SendableBuffer` -- it must go through `LeasedBuffer::into_sendable()`,
   which uses `ManuallyDrop` to transfer lease ownership without double-decrement.

6. **`drain_returns` uses direct arena indexing with epoch sanity checks.**
   O(1) lookup by arena index instead of linear epoch scan, with a secondary
   epoch check to detect (impossible-after-fix-1) arena recycling races.

## Configuration

```rust
PoolConfig {
    arena_size: 2 * 1024 * 1024,  // 2 MiB per arena
    arena_count: 4,                 // 4 arenas in the ring
    page_size: 4096,                // mmap page alignment
}
```

- `arena_count` must be >= 2 (one writable, at least one draining)
- `arena_size` must be a multiple of `page_size`
- Default config: 4 arenas x 2 MiB = 8 MiB total

## Hooks

Implement `BufferPinHook` and `EpochObserver` to integrate with your metrics,
GC, or debugging infrastructure:

```rust
pub trait BufferPinHook {
    fn on_pin(&self, epoch: u64, buf_id: u32);
    fn on_release(&self, epoch: u64, buf_id: u32);
}

pub trait EpochObserver {
    fn on_rotate(&self, retired: u64, active: u64);
    fn on_collect(&self, epoch: u64);
}
```

Use `NoopHooks` for standalone operation.

## Known Limitations

- **Arena sizing is static.** Variable I/O burst sizes may cause `ArenaFull`
  or waste memory. Adaptive rotation is future work.
- **One slow lease pins an entire arena.** Classic epoch-based reclamation
  weakness -- a single long-lived buffer blocks collection of its whole arena.
- **Registration is static.** `register_buffers()` is called once. Dynamic
  resizing requires unregister + re-register, which stalls the ring.
- **No benchmarks yet.** Needs comparison against slab+Mutex, crossbeam-epoch,
  and provided buffer rings under realistic I/O patterns.

## Target Workloads

**Best fit:** High-throughput, steady-state I/O servers (proxies, message
brokers, storage engines) where I/O patterns have strong temporal locality.

**Risky fit:** Bursty or highly variable workloads where static arena sizing
and epoch-scoped lifetimes may waste memory or pin arenas too long.

## Integration Path

Turbine slots underneath Compio (using its decoupled driver) or a custom event
loop, replacing per-operation buffer allocation with epoch-rotated arenas.
Compio's driver-executor separation makes this feasible without forking.
