use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::hint::black_box;
use std::thread;

use turbine_bench::competitors::{
    slab_mutex::SlabPool, sharded_slab::ShardedSlabPool, vec_baseline::VecBaseline,
};
use turbine_bench::{SIZES, arena_size_for};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

const BATCH_SIZE: usize = 32;

/// Generic work item for cross-thread benchmarks.
enum WorkItem<T> {
    Transfer(T),
    Shutdown,
}

fn bench_config(arena_size: usize) -> PoolConfig {
    PoolConfig {
        arena_size,
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    }
}

// --- Turbine cross-thread ---

fn bench_cross_thread_turbine(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/turbine");

    for &size in SIZES {
        let arena_size = arena_size_for(size);
        let config = bench_config(arena_size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = IouringBufferPool::new(config.clone(), NoopHooks).unwrap();
            let handle = pool.transfer_handle();

            let (tx, rx): (
                Sender<WorkItem<turbine_core::transfer::handle::SendableBuffer>>,
                Receiver<WorkItem<turbine_core::transfer::handle::SendableBuffer>>,
            ) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        WorkItem::Transfer(sendable) => {
                            black_box(&sendable);
                            drop(sendable); // sends ReturnedBuffer through channel
                        }
                        WorkItem::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let buf = match pool.lease(sz) {
                    Some(buf) => buf,
                    None => {
                        pool.drain_returns();
                        pool.rotate().unwrap();
                        pool.collect();
                        pool.lease(sz).expect("fresh arena should have space")
                    }
                };
                let sendable = buf.into_sendable(&handle);
                tx.send(WorkItem::Transfer(sendable)).unwrap();

                // Periodically drain returns.
                pool.drain_returns();
            });

            // Shutdown worker.
            tx.send(WorkItem::Shutdown).unwrap();
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
        let config = bench_config(arena_size);

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
                            pool.collect();
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

fn bench_cross_thread_slab_mutex(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/slab_mutex");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = SlabPool::new();
            let handle = pool.handle();

            let (tx, rx): (Sender<WorkItem<usize>>, Receiver<WorkItem<usize>>) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        WorkItem::Transfer(key) => {
                            handle.release(key);
                        }
                        WorkItem::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let key = pool.lease(sz);
                black_box(key);
                tx.send(WorkItem::Transfer(key)).unwrap();
            });

            tx.send(WorkItem::Shutdown).unwrap();
            worker.join().unwrap();
        });
    }
    group.finish();
}

// --- Sharded slab cross-thread ---

fn bench_cross_thread_sharded_slab(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/sharded_slab");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let pool = ShardedSlabPool::new();
            let handle = pool.handle();

            let (tx, rx): (Sender<WorkItem<usize>>, Receiver<WorkItem<usize>>) = bounded(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        WorkItem::Transfer(key) => {
                            handle.release(key);
                        }
                        WorkItem::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let key = pool.lease(sz);
                black_box(key);
                tx.send(WorkItem::Transfer(key)).unwrap();
            });

            tx.send(WorkItem::Shutdown).unwrap();
            worker.join().unwrap();
        });
    }
    group.finish();
}

// --- Vec baseline cross-thread ---

fn bench_cross_thread_vec_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_thread/vec_baseline");

    for &size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let (tx, rx) = bounded::<WorkItem<Vec<u8>>>(64);

            let worker = thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    match work {
                        WorkItem::Transfer(buf) => {
                            black_box(&buf);
                            VecBaseline::release(buf);
                        }
                        WorkItem::Shutdown => break,
                    }
                }
            });

            b.iter(|| {
                let buf = VecBaseline::lease(sz);
                black_box(&buf);
                tx.send(WorkItem::Transfer(buf)).unwrap();
            });

            tx.send(WorkItem::Shutdown).unwrap();
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
