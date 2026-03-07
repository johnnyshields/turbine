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

**Collection (`collect()`):** Iterates the drain queue with `retain()`. Arenas
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
can cross thread boundaries. It stores:

```rust
struct SendableBuffer {
    ptr: *const u8,           // 8 bytes -- pointer into arena mmap
    len: usize,               // 8 bytes
    epoch: u64,               // 8 bytes
    remote_returns: *const AtomicUsize,  // 8 bytes -- pointer into Box<Arena>
}
// Total: 32 bytes (half a cache line)
```

**Drop:** `(*self.remote_returns).fetch_add(1, Release)` -- a single atomic
operation. No channel, no allocation, no `Arc`.

**Safety:** The `unsafe impl Send` relies on two facts:
1. `ptr` points into mmap memory that is valid while `outstanding > 0`.
2. `remote_returns` points into `Box<Arena>` which has a stable address and
   cannot be freed while outstanding leases exist.

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
