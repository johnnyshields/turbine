# Epoch-Based Buffer Rotation for io_uring

**Date:** 2026-03-06
**Status:** Implemented (turbine v0.1.0)

## Problem

High-throughput io_uring runtimes need large pools of pre-registered buffers
for fixed-buffer operations (`IORING_OP_READ_FIXED`, `IORING_OP_WRITE_FIXED`).
The conventional approach — a shared pool with lock-based or CAS-based
allocation — creates contention under load:

- **Lock-based pools** serialize all allocations through a mutex, which is
  catastrophic at high I/O rates.
- **CAS pools** (e.g. lock-free free-lists) still incur cache-line bouncing
  when multiple threads compete for the same counter or pointer.
- **Provided buffer rings** (`IORING_OP_PROVIDE_BUFFERS`) solve kernel-side
  contention but shift the problem to user-space refill logic.

None of these approaches align with the temporal locality of I/O patterns:
buffers allocated together tend to be freed together (same batch of requests,
same scheduler tick, same epoch of work).

## Insight: Temporal Isolation via Epoch Rotation

Instead of a single pool, **pre-allocate N arenas tied to scheduler epochs**.
Each epoch gets its own contiguous region of memory. The lifecycle is:

```
Writable ──rotate()──▶ Retired ──try_collect()──▶ Collected ──rotate()──▶ Writable
```

- **Writable:** The current epoch's arena. Allocations are bump-pointer
  (one branch + one store), no contention, no locking.
- **Retired:** The epoch has ended. In-flight I/O may still reference this
  memory. The arena is read-only from the application's perspective.
- **Collected:** All leases returned. The arena can be recycled for a future
  epoch.

The scheduler calls `rotate()` every N microseconds (or ticks). The bump
allocator resets to offset 0 on reuse — zero fragmentation.

## Design

### Arena (mmap + bump allocator)

Each arena is a single `mmap(MAP_ANONYMOUS | MAP_PRIVATE)` allocation.
Page-aligned, fault-on-demand, automatically zeroed by the kernel.

The bump allocator is a `Cell<usize>` offset — no atomics, because arenas
are thread-local (one per core in a thread-per-core runtime). Each allocation
is:

```
if offset + len > capacity { return None }
offset += len
```

A `Cell<usize>` lease count tracks outstanding `LeasedBuffer` references.
Arenas cannot be collected while `lease_count > 0`.

### EpochClock (rotation ring)

A fixed-size ring of N arenas (minimum 2). A monotonic epoch counter
(`Cell<u64>`) and a write index advance on each `rotate()`.

When the write index wraps around to an arena that still has outstanding
leases, a warning is logged. The old data is overwritten — this is a
misconfiguration (too few arenas or too long-lived leases) and the
application should increase `arena_count`.

### LeasedBuffer (!Send)

A `LeasedBuffer` holds a raw pointer into the arena, a length, and the
epoch/buf_id for io_uring fixed-buffer operations. It is `!Send`
(`PhantomData<Rc<()>>`) — it must stay on the thread that owns the arena.

On `Drop`, it decrements the arena's `lease_count`.

### PinnedWrite (borrow guard)

A `PinnedWrite<'a>` borrows `&mut LeasedBuffer`, preventing the buffer from
being dropped while an io_uring submission is in flight. Exposes raw pointers
and the buf_index for SQE construction.

### Cross-Thread Transfer (Split-Counter Atomic Lease)

For buffers that need to cross thread boundaries (e.g. a worker thread
processing data from an I/O thread):

1. Call `into_sendable()` on a `LeasedBuffer` → `SendableBuffer`.
2. `SendableBuffer` is `Send` (unsafe impl; safety relies on lease_count
   invariant and `Box<Arena>` pointer stability).
3. When the `SendableBuffer` is dropped on the remote thread, it atomically
   increments the arena's `remote_returns` counter via `fetch_add(1, Release)`.
4. The owning thread calls `pool.collect()` — arenas with
   `lease_count - remote_returns == 0` are eligible for recycling.

This split-counter design keeps local operations non-atomic (`Cell<usize>`)
while cross-thread release is a single atomic op. No channel, no `Arc`, no
allocation on the return path.

### io_uring Registration

`RingRegistration::register()` gathers iovecs from all arenas and calls
`submitter.register_buffers()`. The arena index maps 1:1 to the io_uring
buf_index for `IORING_OP_READ_FIXED` / `WRITE_FIXED`.

### GC Hooks

Two trait hooks for optional integration:

- **`BufferPinHook`**: `on_pin` — called when a buffer is leased from an arena.
- **`EpochObserver`**: `on_rotate` / `on_collect` / `on_arena_alloc` /
  `on_arena_free` / `on_collect_sweep` — react to epoch transitions and arena
  lifecycle events.

`NoopHooks` provides zero-cost no-op implementations for standalone use.

## Trade-offs

| Dimension | Epoch rotation | Shared pool |
|-----------|---------------|-------------|
| Allocation cost | 1 branch + 1 store | CAS loop or mutex |
| Fragmentation | Zero (bump reset) | Free-list overhead |
| Memory overhead | N × arena_size | 1 × pool_size |
| Cross-thread | Explicit transfer | Any thread can alloc |
| Lifetime tracking | Per-epoch lease count | Per-buffer ref count |

The primary cost is memory: with 4 arenas × 2 MiB = 8 MiB reserved even if
only one epoch is active. This is negligible for server workloads where the
alternative is 64K+ individual buffer allocations.

## Future Work

- **Adaptive rotation:** rotate based on arena fill level, not just time.
- **Huge pages:** `MAP_HUGETLB` for 2 MiB arenas to reduce TLB pressure.
- **Buffer splitting:** sub-divide arena allocations for scatter/gather I/O.
- **Metrics integration:** expose allocation rates, lease durations, and
  collection latency via the hook traits.
