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
| **Turbine** | **~10 ns** | **~19 ns** | **~13 ns** | **~11 ns** |
| Bumpalo | ~2 ns | ~8 ns | ~62 ns | ~1.7 μs |
| BytesPool | ~11 ns | ~10 ns | ~18 ns | ~12 ns |
| Crossbeam Epoch | ~42 ns | ~59 ns | ~35 ns | ~37 ns |
| Sharded Slab | ~67 ns | ~92 ns | ~96 ns | ~853 ns |
| Slab + Mutex | ~50 ns | ~60 ns | ~82 ns | ~805 ns |
| Vec Baseline | ~40 ns | ~72 ns | ~79 ns | ~826 ns |

### Cross-Thread Transfer

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | **~180 ns** | **~194 ns** | **~182 ns** | **~174 ns** |
| Sharded Slab | ~347 ns | ~491 ns | ~561 ns | ~978 ns |
| Vec Baseline | ~348 ns | ~415 ns | ~522 ns | ~1.1 μs |
| Slab + Mutex | ~496 ns | ~452 ns | ~610 ns | ~14.7 μs |

### Cross-Thread Batch Transfer (32 buffers per batch)

| Pool | 64 B | 512 B | 4 KiB | 64 KiB |
|------|------|-------|-------|--------|
| **Turbine** | ~1.1 μs | ~1.0 μs | ~918 ns | ~1.1 μs |

### Epoch Lifecycle

| Scenario | 64 B | 512 B | 4 KiB |
|----------|------|-------|-------|
| Full cycle (lease batch, rotate, drop, collect) | ~223 ns | ~237 ns | ~237 ns |
| Rotate + collect only (no leases) | ~190 ns | — | — |

## Analysis

**Lease throughput (~10–19 ns, nearly constant across sizes).** Turbine is a bump allocator, so it shares bumpalo's O(1) allocation characteristic. It is slower than bumpalo's ~2–8 ns because it performs additional bookkeeping per lease: epoch tracking, lease counting, buf_id assignment, and registration slot lookup. Unlike bumpalo, turbine supports individual buffer lifetimes and cross-thread transfer. The nearly flat latency regardless of buffer size confirms the bump allocator is working correctly — no `memset`/`memcpy` in the hot path.

**Cross-thread transfer (~174–194 ns).** This is where turbine excels. It beats every competitor at every buffer size and dominates at 64 KiB (174 ns vs 1.1 μs for Vec baseline). Turbine transfers a lightweight `SendableBuffer` handle (pointer + metadata) rather than moving heap data, so cost stays nearly constant as buffer size grows. The gap widens with buffer size — exactly the use case turbine targets, since io_uring buffers tend to be 4–64 KiB.

**Epoch lifecycle (~223–237 ns for a full rotate+collect cycle).** Very low overhead for the complete epoch management cycle: lease a batch of buffers, rotate to a new epoch, drop all leases, and collect the retired arena.

**Key takeaway.** Bumpalo and BytesPool are faster at raw allocation, but they cannot do what turbine does: epoch-based lifecycle management with cross-thread buffer transfer and io_uring fixed-buffer registration. Turbine occupies an unserved niche — bump-allocator speed with the lifetime management required for async io_uring workflows.
