# Cross-Thread Buffer Transfer

Turbine buffers are thread-local by design -- `LeasedBuffer` is `!Send`,
enforced at compile time. This is necessary because arenas use `Cell<usize>`
for allocation and lease counting, which is unsound under concurrent access.

However, many real workloads need to pass buffer data between threads: an I/O
thread receives data, a worker thread processes it. Turbine provides an
explicit, efficient transfer path via `SendableBuffer`.

## The Problem

Thread-per-core runtimes (Monoio, Glommio) avoid cross-thread sharing entirely.
Buffers stay on one core, period. This works for uniform request handlers but
breaks down when:

- A BEAM-like scheduler migrates processes between scheduler threads
- A pipeline architecture passes data from I/O threads to compute threads
- A fan-out pattern distributes work from one receiver to N workers

Tokio solves this with `Arc<Mutex<>>` and work-stealing, but pays per-buffer
atomic reference counting and heap allocation overhead.

Turbine takes a middle path: **thread-local allocation with explicit
cross-thread transfer**, paying synchronization cost only on the transfer
path.

## How It Works

### 1. Lease a buffer (thread-local, zero-cost)

```rust
let buf = pool.lease(4096).unwrap();
buf.as_mut_slice()[..5].copy_from_slice(b"hello");
```

The `LeasedBuffer` holds a raw pointer into the arena and cannot leave
the thread.

### 2. Convert to SendableBuffer

```rust
let sendable = buf.into_sendable();
```

`into_sendable()` consumes the `LeasedBuffer` via `ManuallyDrop` --
the buffer's `Drop` (which would locally decrement the lease count) is
suppressed. Instead, a `SendableBuffer` is constructed with:

- The same raw pointer and length
- The epoch for identification
- A `*const AtomicUsize` pointing to the arena's `remote_returns` counter

No allocation, no cloning, no channel setup. The `SendableBuffer` is 32
bytes (4 fields, half a cache line).

### 3. Send to another thread

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

### 4. Automatic lease release on drop

When `SendableBuffer` drops (on any thread):

```rust
(*self.remote_returns).fetch_add(1, Ordering::Release);
```

A single atomic operation. The arena's `remote_returns` counter increments,
and the pool thread will see this on its next `collect()` call via
`remote_returns.load(Ordering::Acquire)`.

### 5. Collect on the pool thread

```rust
pool.collect(); // reclaims arenas with outstanding == 0
```

`collect()` checks `lease_count() - remote_returns == 0` for each draining
arena. If all leases (both local and cross-thread) have been returned, the
arena moves to the free pool for reuse.

## Performance

| Operation | Cost |
|-----------|------|
| `into_sendable()` | ~0 ns (pointer copy, no allocation) |
| `SendableBuffer::drop` | 1 atomic `fetch_add` |
| `pool.collect()` | 1 atomic `load` per draining arena |

Compare with the previous channel-based approach:

| | Before (channel) | After (split counter) |
|---|---|---|
| Drop cost | ~8 atomics (Arc clone + channel send) | 1 atomic (fetch_add) |
| SendableBuffer size | 48 bytes (6 fields) | 32 bytes (4 fields) |
| `into_sendable()` args | `&TransferHandle` | none |
| Pool-side drain | `drain_returns()` (explicit) | built into `collect()` |

## Soundness Argument

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

## Constraints

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
