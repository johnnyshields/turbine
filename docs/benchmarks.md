# Turbine Benchmarks

Turbine ships a benchmark suite (`turbine-bench`) comparing its allocation, cross-thread transfer, and epoch lifecycle performance against common buffer pool alternatives.

## Running Benchmarks

```bash
# Run all benchmarks
cargo bench -p turbine-bench

# Run a specific benchmark group
cargo bench -p turbine-bench -- lease_throughput
cargo bench -p turbine-bench -- cross_thread
cargo bench -p turbine-bench -- epoch_lifecycle

# Compile-check only (no execution)
cargo bench -p turbine-bench --no-run
```

Results are written to `target/criterion/` with HTML reports when gnuplot is installed.

## Competitors

| Pool | Description |
|------|-------------|
| **Turbine** | Epoch-based bump allocator with io_uring registration and cross-thread transfer |
| **Bumpalo** | Fast bump allocator — allocates by pointer increment (~3ns), but can only free by resetting the entire arena |
| **BytesPool** | Pre-allocated `Bytes` slab with O(1) lease/release from a free list |
| **Crossbeam Epoch** | Epoch-based reclamation with `Vec<u8>` buffers pinned in a crossbeam guard |
| **Slab + Mutex** | `slab::Slab<Vec<u8>>` behind `Arc<Mutex<_>>` — simple shared-state pool |
| **Sharded Slab** | Lock-free concurrent slab (`sharded_slab` crate) with per-thread sharding |
| **Vec Baseline** | Raw `Vec::with_capacity` + drop — no pooling, pure allocator cost |

## Results

Measured on Linux (WSL2), Rust 1.94 release profile. Numbers are per-operation median latency.

### Lease Throughput (single-thread)

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | **~2.1 ns** | **~2.0 ns** | **~2.1 ns** | **~2.0 ns** |
| Bumpalo | ~1.9 ns | ~5.1 ns | ~52 ns | ~1.4 μs |
| BytesPool | ~8.4 ns | ~8.0 ns | ~8.0 ns | ~7.7 ns |
| Crossbeam Epoch | ~29 ns | ~30 ns | ~37 ns | ~29 ns |
| Sharded Slab | ~49 ns | ~75 ns | ~71 ns | ~742 ns |
| Slab + Mutex | ~44 ns | ~53 ns | ~71 ns | ~712 ns |
| Vec Baseline | ~25 ns | ~45 ns | ~61 ns | ~715 ns |

### Cross-Thread Transfer

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | **~176 ns** | **~179 ns** | **~186 ns** | **~170 ns** |
| Sharded Slab | ~401 ns | ~536 ns | ~519 ns | ~1.1 μs |
| Vec Baseline | ~357 ns | ~464 ns | ~490 ns | ~1.1 μs |
| Slab + Mutex | ~529 ns | ~617 ns | ~617 ns | ~15.5 μs |

### Cross-Thread Batch Transfer (32 buffers per batch)

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | ~1.1 μs | ~1.0 μs | ~918 ns | ~1.1 μs |

### Epoch Lifecycle

| Scenario | 64 B | 512 B | 4 KiB |
|----------|------|-------|-------|
| Full cycle (lease batch, rotate, drop, collect) | ~225 ns | ~293 ns | ~240 ns |
| Rotate + collect only (no leases) | ~225 ns | — | — |

## Analysis

**Lease throughput (~2 ns, constant across all sizes).** After hot-path optimization (fused arena+index lookup, `unsafe` removal of panic branches, `#[inline(always)]` on the entire lease path, cold-path extraction), turbine now matches bumpalo's raw bump allocation speed. The ~2 ns flat latency across all buffer sizes — from 64 B to 64 KiB — confirms the bump allocator is fully inlined with zero overhead from epoch tracking, lease counting, buf_id assignment, and registration slot lookup. This represents a **5–10x improvement** over the pre-optimization baseline (~10–19 ns).

**Cross-thread transfer (~170–186 ns).** Turbine beats every competitor at every buffer size and dominates at 64 KiB (170 ns vs 1.1 μs for Vec baseline — a 6.5x advantage). Turbine transfers a lightweight `SendableBuffer` handle (pointer + metadata) rather than moving heap data, so cost stays nearly constant as buffer size grows. The gap widens with buffer size — exactly the use case turbine targets, since io_uring buffers tend to be 4–64 KiB.

**Epoch lifecycle (~225–293 ns for a full rotate+collect cycle).** Very low overhead for the complete epoch management cycle: lease a batch of buffers, rotate to a new epoch, drop all leases, and collect the retired arena.

**Key takeaway.** Turbine now matches bumpalo at raw allocation speed (~2 ns) while providing features bumpalo cannot: epoch-based lifecycle management, individual buffer lifetimes, cross-thread transfer via `SendableBuffer`, and io_uring fixed-buffer registration. BytesPool is ~4x slower at allocation despite being a simpler free-list design. Turbine occupies an unserved niche — the fastest bump allocator with the lifetime management required for async io_uring workflows.

## Flamegraph Profiling

Turbine ships six flamegraph binaries in `turbine-bench` that isolate individual hot paths for profiling with `pprof`. Each binary runs for 5 seconds (configurable via `FLAMEGRAPH_DURATION_SECS`) and writes an SVG to `target/`.

### Running

```bash
# Run all flamegraphs
cargo run --release --bin flamegraph_lease
cargo run --release --bin flamegraph_rotate
cargo run --release --bin flamegraph_collect
cargo run --release --bin flamegraph_spsc
cargo run --release --bin flamegraph_cross_thread
cargo run --release --bin flamegraph_iouring
```

SVGs are written to `target/flamegraph-{lease,rotate,collect,spsc,cross-thread,iouring}.svg`.

### Results

Measured on Linux (WSL2), Rust 1.94 release profile, 64-byte buffers, 5-second runs.

| Binary | What it measures | Throughput | Per-iter |
|--------|-----------------|-----------|----------|
| `flamegraph_lease` | `lease()` + drop tight loop | 2.09B iters/5s | **2.4 ns** |
| `flamegraph_rotate` | `lease()` until full, then `rotate()` | 1.63B iters (25.4M rotations)/5s | **3.1 ns** |
| `flamegraph_collect` | `collect()` with active drain queue (50 arenas) | 123.7M iters (2.5M rebuilds)/5s | **40.4 ns** |
| `flamegraph_spsc` | Producer lease+send via lock-free SPSC ring, consumer drop | 46.6M sends/5s | **107.4 ns** |
| `flamegraph_cross_thread` | Same as SPSC but over crossbeam bounded channel | 33.7M sends/5s | **148.3 ns** |
| `flamegraph_iouring` | `write_fixed` submissions through io_uring | 17.5M iters/5s | **285.4 ns** |

### Flamegraph Analysis

**`flamegraph_lease` (2.4 ns/iter).** The entire hot path is inlined — no Turbine functions appear in samples. Only `rotate()` (0.16%) and `clock_gettime` (0.24%) are visible. The lease path (arena lookup, bump allocation, lease counting, buf_id assignment) compiles down to a handful of instructions with zero function call overhead.

**`flamegraph_rotate` (3.1 ns/iter).** `rotate()` accounts for 4.5% of total time, with `collect()` consuming ~52% of that. `advise_free_unused` (madvise) is negligible at 0.16%. The amortized rotation cost is ~0.6 ns per iteration — the loop is dominated by the lease path which is fully inlined.

**`flamegraph_collect` (40.4 ns/iter).** `collect()` dominates at ~70% of samples, which is expected — it scans the drain queue, checks lease counts (Relaxed + Acquire ordering), runs madvise, and manages the free pool. The remaining ~24% is `build_drain_queue` (benchmark harness rebuilding the drain queue via rotate+lease). The `swap_remove` loop is tight with no allocation overhead.

**`flamegraph_spsc` (107.4 ns/iter).** Uses a custom lock-free SPSC ring with cache-line-padded head/tail atomics. The hot path (lease → `into_sendable` → ring push / ring pop → `SendableBuffer::drop` with `fetch_add`) is fully inlined. The 107 ns cost is the fundamental price of cross-core atomic coordination (Release/Acquire on head/tail + the `remote_returns` `fetch_add` on drop).

**`flamegraph_cross_thread` (148.3 ns/iter).** Uses `crossbeam_channel::bounded`. ~50% of producer time is in `Sender::send`, ~15% in `SyncWaker::notify` (futex syscalls), ~4% in `sched_yield`. The ~40 ns delta vs SPSC (148 vs 107 ns) is entirely crossbeam channel overhead (futex wake/notify). No Turbine functions appear as hotspots — the buffer pool is not the bottleneck.

**`flamegraph_iouring` (285.4 ns/iter).** 78.5% of time is in the `syscall` instruction (io_uring submit + wait). `register()` is 2.75% and `unregister()` is 1.86% (both one-time init/cleanup costs). `collect()` is 0.65%. Turbine userspace overhead is under 5% of total — the kernel dominates, which is the expected profile for an io_uring write path.

**Summary.** No optimization opportunities were identified in Turbine code. The hot paths (`lease`, `rotate`, `collect`) are tight and well-inlined. The heavier benchmarks are dominated by factors outside Turbine's control: kernel syscalls (io_uring), cross-core atomic coordination (SPSC), and crossbeam channel overhead (cross_thread).
