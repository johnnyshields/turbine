# Architecture

Turbine is a specialized buffer allocator for io_uring, not a runtime. It
manages the lifecycle of mmap'd memory arenas, provides zero-contention
bump allocation, and supports cross-thread buffer transfer with minimal
synchronization overhead.

## Component Overview

```
IouringBufferPool<H>
 ├── ArenaManager           (slab of Box<Arena>, drain queue, free pool)
 │    ├── Arena 0            [mmap region, bump allocator, split-counter leases]
 │    ├── Arena 1
 │    └── Arena N
 ├── RingRegistration       (SlotAllocator bitmap, arena→slot mapping)
 └── H: BufferPinHook +     (user-provided hook implementations)
       EpochObserver
```

## Arena

`Arena` (`epoch/arena.rs`) is the fundamental unit -- an mmap'd anonymous
memory region with a bump allocator.

**Memory layout:** A single `mmap(MAP_ANONYMOUS | MAP_PRIVATE)` call allocates
a contiguous region. Page-aligned, fault-on-demand, kernel-zeroed.

**Allocation:** A `Cell<usize>` offset increments on each `alloc()` call.
One branch (capacity check) + one store (offset update) per allocation. No
atomics, no locking -- arenas are assumed thread-local.

**Lease tracking (split counter):** Two counters track outstanding buffer
references:

```
lease_count: Cell<usize>       -- incremented on acquire, decremented on local release
remote_returns: AtomicUsize    -- incremented on cross-thread release (SendableBuffer::drop)

outstanding = lease_count.get() - remote_returns.load(Acquire)
```

Local operations use `Cell` (compiles to plain load/store, zero overhead).
Cross-thread release is a single `fetch_add(1, Release)`. The `Acquire/Release`
pair ensures the pool thread sees all cross-thread decrements when checking
whether an arena can be collected.

**Lifecycle states:**

```
Writable ──rotate()──> Retired ──collect()──> Collected ──rotate()──> Writable
                                                  │
                                              (reset + reuse)
```

- **Writable:** Active arena accepting bump allocations.
- **Retired:** Epoch rotated. In-flight I/O may still reference this memory.
  `madvise(MADV_FREE)` hints the OS to reclaim unused pages.
- **Collected:** All leases returned (`outstanding == 0`). Arena moves to the
  free pool for reuse. `reset()` zeros the bump offset, buf_id counter, lease
  count, and remote_returns.

**Pointer stability:** Arenas are stored as `Box<Arena>` in a `Vec<Option<...>>`
slab. `Box` ensures heap allocation with a stable address -- the `Vec` may
grow and reallocate its pointer array, but each `Box<Arena>` stays at the same
address. This is critical for `SendableBuffer`, which stores raw pointers into
arena memory.

## ArenaManager

`ArenaManager` (`epoch/manager.rs`) owns the slab of arenas and manages their
lifecycle through three collections:

- **Slab:** `Vec<Option<Box<Arena>>>` -- the arena storage. Indices are
  `ArenaIdx` newtypes. `None` entries are slots freed by `shrink()`.
- **Drain queue:** `Vec<ArenaIdx>` -- retired arenas awaiting lease completion.
- **Free pool:** `Vec<ArenaIdx>` -- collected arenas ready for reuse.

**Rotation protocol (`rotate()`):**

1. Try to pop an arena from the free pool.
2. If empty, run `collect()` to sweep the drain queue for zero-lease arenas.
3. If still empty, allocate a new arena (subject to `max_total_arenas`).
4. **Only after securing a next arena**, retire the current one. On failure,
   no state is mutated (invariant #2).
5. Set the new arena to `Writable`, assign the next epoch, update the write
   index.

This ensures `rotate()` never blocks on outstanding leases and never leaves
the pool without a writable arena.

**Collection (`collect()`):** Iterates the drain queue with `swap_remove`. Arenas
with `lease_count() == 0` move to the free pool. The split counter means this
check transparently accounts for both local and cross-thread releases.

## Buffer Types

### LeasedBuffer

`LeasedBuffer` (`buffer/leased.rs`) is a `!Send` handle to bytes within an
arena. It holds:

- Raw pointer + length into the arena's mmap region
- Epoch, buf_id, SlotId for io_uring identification
- Raw pointer back to the `Arena` for lease release on drop
- `PhantomData<Rc<()>>` to enforce `!Send` at compile time

**Drop:** Calls `arena.release_lease()` (Cell decrement -- local, non-atomic).

**`into_sendable()`:** Converts to a `SendableBuffer` for cross-thread
transfer. Uses `ManuallyDrop` to consume the `LeasedBuffer` without calling
its `Drop` (which would locally decrement the lease count). The lease ownership
transfers to the `SendableBuffer`.

### SendableBuffer

`SendableBuffer` (`transfer/handle.rs`) is `Send` -- the only buffer type that
can cross thread boundaries.

#### The Problem

Thread-per-core runtimes (Monoio, Glommio) avoid cross-thread sharing entirely
-- buffers stay on one core, period. This works for uniform request handlers but
breaks down when a BEAM-like scheduler migrates processes between scheduler
threads, a pipeline architecture passes data from I/O threads to compute
threads, or a fan-out pattern distributes work from one receiver to N workers.
Tokio solves this with `Arc<Mutex<>>` and work-stealing, but pays per-buffer
atomic reference counting and heap allocation overhead. Turbine takes a middle
path: thread-local allocation with explicit cross-thread transfer, paying
synchronization cost only on the transfer path.

#### How It Works

**1. Lease a buffer (thread-local, zero-cost)**

```rust
let buf = pool.lease(4096).unwrap();
buf.as_mut_slice()[..5].copy_from_slice(b"hello");
```

The `LeasedBuffer` holds a raw pointer into the arena and cannot leave
the thread.

**2. Convert to SendableBuffer**

```rust
let sendable = buf.into_sendable();
```

`into_sendable()` consumes the `LeasedBuffer` via `ManuallyDrop` --
the buffer's `Drop` (which would locally decrement the lease count) is
suppressed. Instead, a `SendableBuffer` is constructed with the same raw
pointer and length, the epoch for identification, and a `*const AtomicUsize`
pointing to the arena's `remote_returns` counter. No allocation, no cloning,
no channel setup.

**3. Send to another thread**

```rust
std::thread::spawn(move || {
    // Read the buffer on the remote thread.
    let data = unsafe { sendable.as_slice() };
    process(data);
    // sendable is dropped here
});
```

`SendableBuffer` implements `Send` (unsafe impl). The raw pointer is valid
because the arena cannot be freed while outstanding leases > 0.

**4. Automatic lease release on drop**

When `SendableBuffer` drops (on any thread):

```rust
(*self.remote_returns).fetch_add(1, Ordering::Release);
```

A single atomic operation. The arena's `remote_returns` counter increments,
and the pool thread will see this on its next `collect()` call via
`remote_returns.load(Ordering::Acquire)`.

**5. Collect on the pool thread**

```rust
pool.collect(); // reclaims arenas with outstanding == 0
```

`collect()` checks `lease_count() - remote_returns == 0` for each draining
arena. If all leases (both local and cross-thread) have been returned, the
arena moves to the free pool for reuse.

#### Layout and Cost

```rust
struct SendableBuffer {
    ptr: *const u8,           // 8 bytes -- pointer into arena mmap
    len: usize,               // 8 bytes
    epoch: u64,               // 8 bytes
    remote_returns: *const AtomicUsize,  // 8 bytes -- pointer into Box<Arena>
}
// Total: 32 bytes (half a cache line)
```

| Operation | Cost |
|-----------|------|
| `into_sendable()` | ~0 ns (pointer copy, no allocation) |
| `SendableBuffer::drop` | 1 atomic `fetch_add` |
| `pool.collect()` | 1 atomic `load` per draining arena |

Compared with the previous channel-based approach:

| | Before (channel) | After (split counter) |
|---|---|---|
| Drop cost | ~8 atomics (Arc clone + channel send) | 1 atomic (fetch_add) |
| SendableBuffer size | 48 bytes (6 fields) | 32 bytes (4 fields) |
| `into_sendable()` args | `&TransferHandle` | none |
| Pool-side drain | `drain_returns()` (explicit) | built into `collect()` |

#### Soundness

The `unsafe impl Send for SendableBuffer` relies on:

1. **`ptr` validity:** Points into arena mmap memory. The arena cannot be
   freed while `outstanding_leases() > 0`. A live `SendableBuffer` means
   its `fetch_add` hasn't fired, so `outstanding > 0`. Therefore the pointer
   is valid for the `SendableBuffer`'s lifetime.

2. **`remote_returns` pointer validity:** Points into `Box<Arena>` in the
   slab. `Box` provides address stability (the `Vec` may reallocate its
   pointer array, but the heap-allocated `Arena` stays put). Same lifetime
   argument as above -- the arena exists while the `SendableBuffer` does.

3. **Memory ordering:** `fetch_add(Release)` on the writer thread pairs with
   `load(Acquire)` on the pool thread. This establishes a happens-before
   relationship: the pool thread sees all writes made by the remote thread
   before the `fetch_add`.

4. **No ABA problem:** When an arena is collected and recycled, `reset()`
   zeroes both `lease_count` and `remote_returns`. But collection only happens
   after `outstanding == 0`, meaning no `SendableBuffer` for that arena
   exists on any thread. A recycled arena starts fresh.

#### Constraints

- **Pool must outlive all `SendableBuffer`s.** If the pool drops while
  `SendableBuffer`s are in flight, the raw pointers dangle. This is the
  same constraint as the previous design -- Turbine does not add lifetime
  tracking for pool destruction.

- **`as_slice()` is unsafe.** The caller must ensure the arena memory is
  valid. This is guaranteed by the lease count invariant in normal operation,
  but Turbine cannot prevent a caller from using a `SendableBuffer` after
  the pool has been dropped.

- **No per-buffer notification on cross-thread release.** The previous
  `on_release` hook was removed because it only fired for channel-based
  returns (never for local drops). With the atomic approach, the pool
  thread learns about releases in bulk via `collect()`, not per-buffer.

### PinnedWrite

`PinnedWrite<'a>` (`buffer/pinned.rs`) borrows `&'a mut LeasedBuffer`,
preventing the buffer from being dropped while an io_uring SQE references it.
Exposes `as_ptr()`, `as_mut_ptr()`, and `buf_index()` for SQE construction.

## RingRegistration

`RingRegistration` (`ring/registration.rs`) manages io_uring fixed-buffer
registration. It maintains:

- A `SlotAllocator` -- a `u64` bitmap supporting up to 64 slots, with O(1)
  alloc (trailing zeros) and free (bit clear).
- An `arena_slot_map: Vec<Option<SlotId>>` mapping slab indices to io_uring
  slot IDs.
- A `generation` counter incremented on every registration change.

**`register()`** gathers iovecs from all live arenas and calls
`submitter.register_buffers()`. Each arena maps 1:1 to an io_uring
`buf_index`, used in `IORING_OP_READ_FIXED` and `IORING_OP_WRITE_FIXED`.

## Hook System

Two traits allow integration with metrics, GC, or custom runtime logic:

**`BufferPinHook`** -- per-buffer events:
- `on_pin(epoch, buf_id)` -- called when a buffer is leased from an arena.

**`EpochObserver`** -- epoch lifecycle events:
- `on_rotate(retired, active)` -- epoch transition.
- `on_collect(epoch)` -- specific epoch reclaimed.
- `on_arena_alloc(arena_idx)` -- new arena allocated (default: no-op).
- `on_arena_free(arena_idx)` -- arena munmap'd (default: no-op).
- `on_collect_sweep(collected)` -- batch collect completed (default: no-op).

`NoopHooks` provides zero-cost no-op implementations for standalone use.

## Type Safety

Two newtype wrappers prevent mixing indices:

- `ArenaIdx(usize)` -- slab index into the arena `Vec`.
- `SlotId(u16)` -- io_uring registration slot index.

These are distinct types that cannot be accidentally interchanged.

## Error Handling

`TurbineError` covers all failure modes:

| Variant | Cause |
|---------|-------|
| `ArenaFull` | Bump allocator exhausted |
| `EpochNotFound` | No arena serving the requested epoch |
| `EpochNotCollectable` | Arena still has outstanding leases |
| `ArenaLimitExceeded` | `max_total_arenas` would be exceeded |
| `NoRegistrationSlot` | All io_uring slots occupied |
| `Mmap` / `Munmap` | OS memory mapping failure |
| `Registration` | io_uring `register_buffers` failure |
| `Madvise` | `madvise(MADV_FREE)` failure |
| `InvalidConfig` | Configuration validation failure |

All errors are non-panicking. Debug builds add `debug_assert!` for internal
invariant violations (leaked leases, underflow).

## Safety Invariants

1. **`rotate()` refuses to recycle arenas with live leases.** An arena only
   moves from the drain queue to the free pool when `outstanding == 0`.
   `rotate()` never blocks -- it allocates a new arena if no collected arenas
   are available.

2. **Arena `Drop` panics on leaked leases in debug.** A `debug_assert!` fires
   if an arena is dropped while `outstanding_leases() > 0`, catching lease
   accounting bugs during development.

3. **`LeasedBuffer` is `!Send`.** Enforced via `PhantomData<Rc<()>>`. The
   compiler rejects any attempt to move a `LeasedBuffer` across thread
   boundaries.

4. **`IouringBufferPool` is `!Send`.** The pool uses `Cell<usize>` internally,
   which is unsound under concurrent access. `PhantomData<Rc<()>>` prevents
   the pool from being sent to another thread.

5. **`SendableBuffer` construction is `pub(crate)`.** External code cannot
   forge a `SendableBuffer` -- it must go through `LeasedBuffer::into_sendable()`,
   which correctly transfers lease ownership.

6. **`SendableBuffer` stores `*const AtomicUsize`.** This raw pointer into
   `Box<Arena>` is valid because `Box` provides heap address stability and
   the arena cannot be freed while outstanding leases exist.
