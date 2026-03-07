# Turbine

Epoch-based buffer rotation for io_uring.

Turbine pre-allocates mmap'd buffer arenas tied to scheduler epochs. Every N
microseconds the scheduler rotates to a new arena. In-flight I/O from the
previous epoch completes into the old arena (now read-only), and the current
epoch's arena uses append-only bump allocation -- no contention, no locking.

**Status:** Early (v0.1.0) | **Platform:** Linux only | **License:** MIT

## Motivation

Turbine started as exploration into combining two models that don't normally
coexist: **BEAM/OTP-style microprocesses** and **io_uring thread-per-core I/O**.

The BEAM VM (Erlang/Elixir) excels at lightweight concurrency -- millions of
isolated processes communicating via message passing, with preemptive scheduling
and fault supervision. But its I/O model is mediated through ports and drivers
(essentially epoll-based), and it has no story for io_uring's fixed-buffer
registration, scatter/gather, or zero-copy submission.

Thread-per-core runtimes like [Monoio](https://github.com/bytedance/monoio)
and [Glommio](https://github.com/DataDog/glommio) go the other direction: they
pin work to cores, eliminate cross-thread synchronization entirely, and build
directly on io_uring. The tradeoff is that buffers are strictly thread-local --
no sharing, no migration. This is ideal for uniform network proxies but
fundamentally incompatible with the BEAM model, where any process can message
any other process regardless of which scheduler thread it runs on.

A BEAM-like runtime (e.g.
[Rebar](https://github.com/alexandernicholson/rebar)) needs **efficient
cross-thread buffer sharing** if using io_uring.
Processes on thread A produce I/O buffers that processes on thread B consume.
Neither the thread-per-core "no sharing" model nor Tokio's "Arc\<Mutex\<>>"
approach is satisfactory:

- Thread-per-core runtimes (Monoio, Glommio) simply forbid cross-thread
  buffers. Their `!Send` buffer types have no transfer path at all.
- Tokio's work-stealing model makes buffers routinely cross threads, but at the
  cost of heap allocation and atomic reference counting per buffer.
- Neither approach provides io_uring fixed-buffer registration, which requires
  stable, pre-registered memory regions.

Turbine solves this with **epoch-based buffer arenas** that are thread-local for
allocation (zero-contention bump alloc via `Cell`) but support explicit
cross-thread transfer via a **split-counter atomic lease** -- local operations
stay non-atomic, while cross-thread release is a single `fetch_add`. No
channels, no `Arc`, no mutex. The arena's memory is stable (mmap'd, `Box`-pinned
in a slab), so pointers remain valid across threads for the lifetime of the
lease.

## Why Turbine?

Most Rust io_uring runtimes solve scheduling and I/O submission but leave buffer
allocation to the application. Per-operation heap allocation and partial
fixed-buffer support leave performance on the table at extreme throughput.

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

## Architecture

```
IouringBufferPool
 ├── EpochClock (ring of N arenas)
 │    ├── Arena 0  [mmap, bump allocator, lease count]
 │    ├── Arena 1
 │    └── Arena N-1
 ├── RingRegistration (io_uring fixed-buffer iovecs)
 └── Split Counter (Cell + AtomicUsize per arena, for cross-thread returns)
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

6. **`SendableBuffer` stores `*const AtomicUsize` pointing to arena's `remote_returns`.**
   Valid because `Box<Arena>` provides address stability and the arena cannot be
   freed while outstanding leases exist (the split counter prevents it).

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

- `initial_arenas` must be >= 1 (one writable; draining arenas accumulate in drain queue)
- `arena_size` must be a multiple of `page_size`
- `registration_slots` must be >= `initial_arenas`
- Default config: 4 arenas x 2 MiB = 8 MiB total

## Hooks

Implement `BufferPinHook` and `EpochObserver` to integrate with your metrics,
GC, or debugging infrastructure:

```rust
pub trait BufferPinHook {
    fn on_pin(&self, epoch: u64, buf_id: u32);
}

pub trait EpochObserver {
    fn on_rotate(&self, retired: u64, active: u64);
    fn on_collect(&self, epoch: u64);
    fn on_arena_alloc(&self, arena_idx: ArenaIdx) {}
    fn on_arena_free(&self, arena_idx: ArenaIdx) {}
    fn on_collect_sweep(&self, collected: usize) {}
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

## Target Workloads

**Best fit:** High-throughput, steady-state I/O servers (proxies, message
brokers, storage engines) where I/O patterns have strong temporal locality.
Also well-suited as the buffer layer for custom runtimes exploring BEAM-like
concurrency models on top of io_uring.

**Risky fit:** Bursty or highly variable workloads where static arena sizing
and epoch-scoped lifetimes may waste memory or pin arenas too long.

## Integration Path

Turbine slots underneath Compio (using its decoupled driver) or a custom event
loop, replacing per-operation buffer allocation with epoch-rotated arenas.
Compio's driver-executor separation makes this feasible without forking.

## Benchmarks

See [docs/benchmarks.md](docs/benchmarks.md) for detailed numbers. Headlines:

| Path | Latency |
|------|---------|
| Lease (any size) | ~19 ns (constant -- bump alloc) |
| Cross-thread transfer | ~300--400 ns (1 atomic op) |
| Full epoch lifecycle | ~380--490 ns (rotate + collect) |

Cross-thread transfer beats Vec baseline at every buffer size and dominates at
64 KiB (392 ns vs 1.4 us) because Turbine transfers a lightweight handle
(pointer + metadata) rather than moving heap data.
