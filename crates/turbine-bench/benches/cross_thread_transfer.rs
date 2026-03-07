use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::hint::black_box;
use std::thread;

use turbine_bench::competitors::{
    slab_mutex::SlabPool, sharded_slab::ShardedSlabPool, vec_baseline::VecBaseline,
};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

const SIZES: &[usize] = &[64, 512, 4096, 65536];
const BATCH_SIZE: usize = 32;

fn arena_size_for(buf_size: usize) -> usize {
    let min = buf_size * 64;
    let aligned = (min + 4095) & !4095;
    aligned.max(4096)
}

// --- Turbine cross-thread ---

enum TurbineWork {
    Transfer(turbine_core::transfer::handle::SendableBuffer),
    Shutdown,
}

fn bench_cross_thread_turbine(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/turbine");

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
            let handle = pool.transfer_handle();

            let (tx, rx): (Sender<TurbineWork>, Receiver<TurbineWork>) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        TurbineWork::Transfer(sendable) => {
                            black_box(&sendable);
                            drop(sendable); // sends ReturnedBuffer through channel
                        }
                        TurbineWork::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let buf = match pool.lease(sz) {
                    Some(buf) => buf,
                    None => {
                        pool.drain_returns();
                        pool.rotate().unwrap();
                        let oldest = pool.clock().retained_epochs().next();
                        if let Some(epoch) = oldest {
                            let _ = pool.try_collect(epoch);
                        }
                        pool.lease(sz).expect("fresh arena should have space")
                    }
                };
                let sendable = buf.into_sendable(&handle);
                tx.send(TurbineWork::Transfer(sendable)).unwrap();

                // Periodically drain returns.
                pool.drain_returns();
            });

            // Shutdown worker.
            tx.send(TurbineWork::Shutdown).unwrap();
            worker.join().unwrap();

            // Final drain.
            pool.drain_returns();
        });
    }
    group.finish();
}

// --- Turbine batch cross-thread ---

fn bench_cross_thread_turbine_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread_batch/turbine");

    for &size in SIZES {
        let arena_size = arena_size_for(size).max(size * BATCH_SIZE * 2);
        let arena_size = (arena_size + 4095) & !4095;
        let config = PoolConfig {
            arena_size,
            arena_count: 3,
            page_size: 4096,
        };

        group.throughput(Throughput::Bytes((size * BATCH_SIZE) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = IouringBufferPool::new(config.clone(), NoopHooks).unwrap();
            let handle = pool.transfer_handle();

            let (tx, rx): (Sender<Vec<turbine_core::transfer::handle::SendableBuffer>>, Receiver<Vec<turbine_core::transfer::handle::SendableBuffer>>) = bounded(8);

            let worker = thread::spawn(move || {
                while let Ok(batch) = rx.recv() {
                    if batch.is_empty() {
                        break;
                    }
                    for sendable in batch {
                        black_box(&sendable);
                        drop(sendable);
                    }
                }
            });

            b.iter(|| {
                let mut batch = Vec::with_capacity(BATCH_SIZE);
                for _ in 0..BATCH_SIZE {
                    let buf = match pool.lease(sz) {
                        Some(buf) => buf,
                        None => {
                            pool.drain_returns();
                            pool.rotate().unwrap();
                            let oldest = pool.clock().retained_epochs().next();
                            if let Some(epoch) = oldest {
                                let _ = pool.try_collect(epoch);
                            }
                            pool.lease(sz).expect("fresh arena should have space")
                        }
                    };
                    batch.push(buf.into_sendable(&handle));
                }
                tx.send(batch).unwrap();
                pool.drain_returns();
            });

            // Shutdown worker.
            tx.send(Vec::new()).unwrap();
            worker.join().unwrap();
            pool.drain_returns();
        });
    }
    group.finish();
}

// --- Slab+Mutex cross-thread ---

enum SlabWork {
    Release(usize),
    Shutdown,
}

fn bench_cross_thread_slab_mutex(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/slab_mutex");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = SlabPool::new();
            let handle = pool.handle();

            let (tx, rx): (Sender<SlabWork>, Receiver<SlabWork>) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        SlabWork::Release(key) => {
                            handle.release(key);
                        }
                        SlabWork::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let key = pool.lease(sz);
                black_box(key);
                tx.send(SlabWork::Release(key)).unwrap();
            });

            tx.send(SlabWork::Shutdown).unwrap();
            worker.join().unwrap();
        });
    }
    group.finish();
}

// --- Sharded slab cross-thread ---

enum ShardedSlabWork {
    Release(usize),
    Shutdown,
}

fn bench_cross_thread_sharded_slab(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/sharded_slab");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = ShardedSlabPool::new();
            let handle = pool.handle();

            let (tx, rx): (Sender<ShardedSlabWork>, Receiver<ShardedSlabWork>) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        ShardedSlabWork::Release(key) => {
                            handle.release(key);
                        }
                        ShardedSlabWork::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let key = pool.lease(sz);
                black_box(key);
                tx.send(ShardedSlabWork::Release(key)).unwrap();
            });

            tx.send(ShardedSlabWork::Shutdown).unwrap();
            worker.join().unwrap();
        });
    }
    group.finish();
}

// --- Vec baseline cross-thread ---

enum VecWork {
    Transfer(Vec<u8>),
    Shutdown,
}

fn bench_cross_thread_vec_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/vec_baseline");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let (tx, rx): (Sender<VecWork>, Receiver<VecWork>) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        VecWork::Transfer(buf) => {
                            black_box(&buf);
                            VecBaseline::release(buf);
                        }
                        VecWork::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let buf = VecBaseline::lease(sz);
                black_box(&buf);
                tx.send(VecWork::Transfer(buf)).unwrap();
            });

            tx.send(VecWork::Shutdown).unwrap();
            worker.join().unwrap();
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cross_thread_turbine,
    bench_cross_thread_turbine_batch,
    bench_cross_thread_slab_mutex,
    bench_cross_thread_sharded_slab,
    bench_cross_thread_vec_baseline,
);
criterion_main!(benches);
