# Competitive Landscape Assessment

**Date:** 2026-03-06
**Status:** Research

## What Turbine Is (and Isn't)

Turbine is **not an async runtime**. It is a specialized epoch-based buffer
allocator for io_uring fixed-buffer operations. It is designed to slot
underneath a runtime (compio, custom event loop, etc.) as the buffer
management layer.

## Comparison Matrix

| | Turbine | Compio | Monoio | Glommio | Lunatic |
|---|---|---|---|---|---|
| **What it is** | Buffer allocator | Full async runtime | Full async runtime | Full async runtime | WASM process runtime |
| **Thread model** | Thread-per-core (assumed) | Thread-per-core | Thread-per-core | Thread-per-core | Work-stealing |
| **Buffer alloc cost** | Bump (1 branch + 1 store) | Heap box per I/O op | Per-op ownership transfer | DMA-aligned alloc | Per-process heap |
| **io_uring integration** | Full fixed-buffer registration | Proactor (3 backends) | Primary backend | 3-ring architecture | None (uses epoll) |
| **Fixed-buffer support** | Yes (1:1 arena→iovec) | Partial (provided rings) | Not prominent | Not prominent | No |
| **Cross-platform** | Linux only | Linux + Windows + fallback | Linux + mio fallback | Linux only | Cross-platform (WASM) |
| **Cross-thread** | Explicit transfer + 1 atomic | N/A (per-core) | Shared waker channels | SPSC lockless channels | Serialized messages |
| **Contention on alloc** | Zero | Allocator-dependent | Per-buffer ownership | Allocator-dependent | N/A |
| **Status** | Early (v0.1.0) | Active (v0.18.0, Iggy uses it) | Active (ByteDance) | Effectively dead | Slowing |

## Detailed Comparisons

### Compio (most relevant)

Compio is the only completion-based runtime that is both cross-platform
(io_uring + IOCP + polling fallback) and actively maintained. Apache Iggy
shipped their v0.6.0 server rewrite on it, achieving 92% P9999 latency
reduction and 18% throughput improvement.

**Where Turbine and Compio are complementary:**

- Compio handles I/O submission, task scheduling, and the proactor model.
  Turbine handles buffer allocation and io_uring fixed-buffer registration.
- Compio boxes every I/O request and transfers buffer ownership per-operation.
  This is sound and ergonomic but measurable at extreme throughput.
- Compio's driver is decoupled from its executor — you can use the driver
  directly without the async executor. This makes integration with Turbine
  feasible without forking.
- Iggy's team noted that POSIX-compliant abstractions "inadequately expose
  powerful io_uring features like registered buffers." Turbine is designed
  for exactly this gap.

**Key Compio trade-offs Turbine avoids:**

- Per-operation heap allocation (Turbine: zero-heap bump alloc)
- Partial fixed-buffer support (Turbine: full `IORING_REGISTER_BUFFERS`)
- Cancellation buffer leaks via "graveyard" pattern (Turbine: epoch-scoped
  lifetime, arenas reclaimed in bulk)

### Monoio (ByteDance)

Thread-per-core runtime built around io_uring with epoll/kqueue fallback.
Uses an ownership-transfer ("rent") model for buffers — you hand the buffer
to the runtime, get it back on completion. No work-stealing, so tasks don't
need to be `Send`.

**Differentiation:** Monoio deliberately avoids cross-thread buffer sharing —
buffers are strictly thread-local with no transfer path. This is the right
call for pure thread-per-core workloads (network proxies, uniform request
handlers) but is incompatible with any model requiring cross-thread data flow,
such as BEAM-style process migration or work-stealing schedulers. Turbine
fills this gap: thread-local bump allocation for the hot path, with explicit
cross-thread transfer via a single atomic op when needed. Monoio also has no
purpose-built buffer allocator for io_uring fixed-buffer registration.

### Glommio (Datadog)

Thread-per-core runtime with three independent io_uring rings (main, latency,
poll) for fine-grained latency control. Strong DMA/direct I/O support.
Cooperative scheduling with explicit yield points.

**Differentiation:** Sophisticated latency control via multi-ring architecture,
but effectively unmaintained since Glauber Costa moved to Turso. No dedicated
buffer allocator for fixed-buffer registration.

### Lunatic

Erlang-inspired runtime using WebAssembly sandboxing for fault isolation.
Preemptive scheduling via bytecode instrumentation. Message passing with
serialization between processes.

**Differentiation:** Solving a completely different problem (fault isolation).
No io_uring, no zero-copy, significant serialization overhead. Not a
relevant comparison for buffer management.

## Why This Hasn't Been Done Before

1. **The problem is narrow.** Most io_uring users don't hit buffer allocation
   as their bottleneck — they hit syscall overhead, completion handling, or
   application logic first. You need millions of ops/sec before mutex/CAS
   contention on a buffer pool shows up in profiles.

2. **Epoch-based reclamation applied to buffer pools is non-obvious.**
   Epoch-based reclamation (crossbeam-epoch, etc.) is well-known for
   lock-free data structure memory management. Repurposing the epoch concept
   for I/O buffer rotation — where temporal locality of allocations maps to
   I/O batches — is a creative lateral transfer.

3. **The LMAX Disruptor solved a different layer.** Disruptor-style ring
   buffers handle inter-thread message passing. Turbine's ring of arenas
   handles buffer lifecycle management. Nobody appears to have combined
   Disruptor-style epoch rotation with io_uring buffer registration.

4. **io_uring fixed-buffer registration is underused.** Most Rust io_uring
   users use provided buffer rings (`IOSQE_BUFFER_SELECT`) or plain buffers.
   Fixed-buffer registration (`IORING_OP_READ_FIXED`) with large
   pre-registered regions is the highest-performance path but requires
   careful lifetime management.

5. **Thread-per-core is still niche in Rust.** Tokio dominates, and Tokio's
   work-stealing model means buffers routinely cross threads, making
   thread-local bump allocation impossible.

## Feasibility Assessment

### The core mechanism is sound

- Bump allocation in a thread-local arena is provably zero-contention.
  `Cell<usize>` is a single-threaded counter — no atomics, no cache misses.
- The lease-count invariant prevents use-after-free. The type system enforces
  this: `LeasedBuffer` is `!Send`, `SendableBuffer` requires explicit transfer.
- io_uring fixed-buffer registration with one iovec per arena is a clean 1:1
  mapping.
- The epoch ring (minimum 2 arenas) ensures a writable arena is always
  available while previous ones drain.

### Risks

1. **Arena sizing is static.** Variable I/O burst sizes may cause `ArenaFull`
   or waste memory. Adaptive rotation (future work) will be important.

2. **Single long-lived buffer pins entire arena.** Classic epoch-based
   reclamation weakness. One slow lease blocks collection of the whole arena.

3. **Registration is static.** `register_buffers()` is called once. Dynamic
   resizing requires unregister + re-register, which stalls the ring.

4. **No benchmarks yet.** Needs comparison against slab+Mutex, crossbeam-epoch,
   and provided buffer rings under realistic I/O patterns.

### Target workloads

Best fit: high-throughput, steady-state I/O servers (proxies, message brokers,
storage engines) where I/O patterns have strong temporal locality.

Risky fit: bursty or highly variable workloads where static arena sizing and
epoch-scoped lifetimes may waste memory or pin arenas too long.

## Integration Path

The realistic integration: Turbine slots underneath Compio (using its
decoupled driver) or a custom event loop, replacing per-operation buffer
allocation with epoch-rotated arenas. Compio's driver-executor separation
makes this feasible without forking.
