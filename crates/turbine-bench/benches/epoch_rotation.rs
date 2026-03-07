use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use turbine_bench::arena_size_for;
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

/// Sizes for epoch rotation — excludes 65536 since epoch lifecycle is more
/// about rotation overhead than large-buffer throughput.
const EPOCH_SIZES: &[usize] = &[64, 512, 4096];

/// Full epoch lifecycle: lease a batch → rotate → drop all → collect.
fn bench_epoch_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("epoch_lifecycle");

    for &size in EPOCH_SIZES {
        let arena_size = arena_size_for(size);
        let config = PoolConfig {
            arena_size,
            arena_count: 3,
            page_size: 4096,
        };
        let bufs_per_batch = (arena_size / size.max(1)).min(64);

        group.throughput(Throughput::Elements(bufs_per_batch as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = IouringBufferPool::new(config.clone(), NoopHooks).unwrap();

            b.iter(|| {
                // Lease a batch from the current epoch.
                let epoch = pool.epoch();
                let mut bufs = Vec::with_capacity(bufs_per_batch);
                for _ in 0..bufs_per_batch {
                    if let Some(buf) = pool.lease(sz) {
                        bufs.push(buf);
                    } else {
                        break;
                    }
                }
                black_box(bufs.len());

                // Rotate to next epoch.
                pool.rotate().unwrap();

                // Drop all leases from the retired epoch.
                drop(bufs);

                // Collect the retired epoch.
                pool.try_collect(epoch).unwrap();
            });
        });
    }
    group.finish();
}

/// Empty rotation cost: rotate + collect with no leases.
fn bench_rotate_collect_only(c: &mut Criterion) {
    let config = PoolConfig {
        arena_size: 4096,
        arena_count: 3,
        page_size: 4096,
    };

    c.bench_function("rotate_collect_only", |b| {
        let pool = IouringBufferPool::new(config.clone(), NoopHooks).unwrap();

        b.iter(|| {
            let epoch = pool.epoch();
            pool.rotate().unwrap();
            pool.try_collect(epoch).unwrap();
            black_box(epoch);
        });
    });
}

criterion_group!(benches, bench_epoch_lifecycle, bench_rotate_collect_only);
criterion_main!(benches);
