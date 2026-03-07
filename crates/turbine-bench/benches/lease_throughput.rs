use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use turbine_bench::competitors::{
    bumpalo_pool::BumpaloPool, bytes_pool::BytesPool, crossbeam_pool::CrossbeamPool,
    slab_mutex::SlabPool, sharded_slab::ShardedSlabPool, vec_baseline::VecBaseline,
};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

const SIZES: &[usize] = &[64, 512, 4096, 65536];

/// Compute arena size for a given buffer size.
/// Ensures at least 64 buffers fit, rounded up to page alignment.
fn arena_size_for(buf_size: usize) -> usize {
    let min = buf_size * 64;
    let aligned = (min + 4095) & !4095; // next multiple of 4096
    aligned.max(4096)
}

fn bench_turbine(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/turbine");

    for &size in SIZES {
        let arena_size = arena_size_for(size);
        let config = PoolConfig {
            arena_size,
            arena_count: 3,
            page_size: 4096,
        };

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = IouringBufferPool::new(config.clone(), NoopHooks).unwrap();
            let bufs_per_arena = arena_size / sz.max(1);

            b.iter(|| {
                let buf = match pool.lease(sz) {
                    Some(buf) => buf,
                    None => {
                        // Arena full — rotate and collect the oldest retired epoch.
                        pool.rotate().unwrap();
                        let oldest = pool.clock().retained_epochs().next();
                        if let Some(epoch) = oldest {
                            let _ = pool.try_collect(epoch);
                        }
                        pool.lease(sz).expect("fresh arena should have space")
                    }
                };
                black_box(&buf);
                drop(buf);
            });

            let _ = bufs_per_arena; // suppress unused warning
        });
    }
    group.finish();
}

fn bench_slab_mutex(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/slab_mutex");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = SlabPool::new();
            b.iter(|| {
                let key = pool.lease(sz);
                black_box(key);
                pool.release(key);
            });
        });
    }
    group.finish();
}

fn bench_sharded_slab(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/sharded_slab");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = ShardedSlabPool::new();
            b.iter(|| {
                let key = pool.lease(sz);
                black_box(key);
                pool.release(key);
            });
        });
    }
    group.finish();
}

fn bench_crossbeam_epoch(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/crossbeam_epoch");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = CrossbeamPool::new(sz);
            b.iter(|| {
                let buf = pool.lease(sz);
                black_box(&buf);
                pool.release(buf);
            });
        });
    }
    group.finish();
}

fn bench_bumpalo(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/bumpalo");

    for &size in SIZES {
        let capacity = arena_size_for(size);
        let bufs_per_arena = capacity / size.max(1);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let mut pool = BumpaloPool::new(capacity);
            let mut count = 0usize;
            b.iter(|| {
                let ptr = pool.lease(sz);
                black_box(ptr);
                count += 1;
                if count >= bufs_per_arena {
                    pool.reset();
                    count = 0;
                }
            });
        });
    }
    group.finish();
}

fn bench_bytes_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/bytes_pool");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let mut pool = BytesPool::new(64, sz);
            b.iter(|| {
                let buf = pool.lease();
                black_box(&buf);
                pool.release(buf);
            });
        });
    }
    group.finish();
}

fn bench_vec_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease_throughput/vec_baseline");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            b.iter(|| {
                let buf = VecBaseline::lease(sz);
                black_box(&buf);
                VecBaseline::release(buf);
            });
        });
    }
    group.finish();
}

// TODO: io_uring provided buffer benchmarks deferred — requires actual I/O submission
// (kernel picks the buffer), making apples-to-apples allocation-only comparison impossible.

criterion_group!(
    benches,
    bench_turbine,
    bench_slab_mutex,
    bench_sharded_slab,
    bench_crossbeam_epoch,
    bench_bumpalo,
    bench_bytes_pool,
    bench_vec_baseline,
);
criterion_main!(benches);
