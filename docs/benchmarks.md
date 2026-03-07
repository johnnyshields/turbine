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
| **Turbine** | **~19 ns** | **~19 ns** | **~19 ns** | **~19 ns** |
| Bumpalo | ~3 ns | ~8 ns | ~62 ns | ~2 μs |
| BytesPool | ~7 ns | ~6 ns | ~6 ns | ~6 ns |
| Crossbeam Epoch | ~39 ns | ~35 ns | ~36 ns | ~36 ns |
| Sharded Slab | ~73 ns | ~120 ns | ~139 ns | ~927 ns |
| Slab + Mutex | ~92 ns | ~116 ns | ~153 ns | ~1.2 μs |
| Vec Baseline | ~40 ns | ~47 ns | ~67 ns | ~877 ns |

### Cross-Thread Transfer

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | **~299 ns** | **~330 ns** | **~400 ns** | **~392 ns** |
| Sharded Slab | ~446 ns | ~595 ns | ~720 ns | ~1.2 μs |
| Vec Baseline | ~390 ns | ~649 ns | ~682 ns | ~1.4 μs |
| Slab + Mutex | ~589 ns | ~670 ns | ~629 ns | ~18 μs |

### Cross-Thread Batch Transfer (32 buffers per batch)

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | ~4.7 μs | ~4.6 μs | ~4.9 μs | ~4.5 μs |

### Epoch Lifecycle

| Scenario | 64 B | 512 B | 4 KiB |
|----------|------|-------|-------|
| Full cycle (lease batch, rotate, drop, collect) | ~393 ns | ~490 ns | ~382 ns |
| Rotate + collect only (no leases) | ~329 ns | — | — |

## Analysis

**Lease throughput (~19 ns, constant across all sizes).** Turbine is a bump allocator, so it shares bumpalo's O(1) allocation characteristic. It is slower than bumpalo's ~3–8 ns because it performs additional bookkeeping per lease: epoch tracking, lease counting, buf_id assignment, and registration slot lookup. Unlike bumpalo, turbine supports individual buffer lifetimes and cross-thread transfer. The flat ~19 ns regardless of buffer size confirms the bump allocator is working correctly — no `memset`/`memcpy` in the hot path.

**Cross-thread transfer (~300–400 ns).** This is where turbine excels. It beats the Vec baseline at every buffer size and dominates at 64 KiB (392 ns vs 1.4 μs). Turbine transfers a lightweight `SendableBuffer` handle (pointer + metadata) rather than moving heap data, so cost stays nearly constant as buffer size grows. The gap widens with buffer size — exactly the use case turbine targets, since io_uring buffers tend to be 4–64 KiB.

**Epoch lifecycle (~380–490 ns for a full rotate+collect cycle).** Very low overhead for the complete epoch management cycle: lease a batch of buffers, rotate to a new epoch, drop all leases, and collect the retired arena.

**Key takeaway.** Bumpalo and BytesPool are faster at raw allocation, but they cannot do what turbine does: epoch-based lifecycle management with cross-thread buffer transfer and io_uring fixed-buffer registration. Turbine occupies an unserved niche — bump-allocator speed with the lifetime management required for async io_uring workflows.
