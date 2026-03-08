use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

use pprof::flamegraph::Options;
use turbine_core::buffer::leased::LeasedBuffer;
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

/// Check the wall clock every this many iterations.
/// collect() scans the entire drain queue (~1-10µs per call), so 1k iters
/// keeps the check cost negligible.
const CLOCK_CHECK_INTERVAL: u64 = 1_000;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Build a large drain queue by rotating many times with held leases.
/// Returns the held leases (keeping arenas in the drain queue).
fn build_drain_queue(
    pool: &IouringBufferPool<NoopHooks>,
    buf_size: usize,
    queue_depth: usize,
) -> Vec<LeasedBuffer> {
    let mut held = Vec::with_capacity(queue_depth);
    for _ in 0..queue_depth {
        let buf = match pool.lease(buf_size) {
            Some(buf) => buf,
            None => {
                pool.rotate().unwrap();
                pool.collect();
                pool.lease(buf_size).expect("fresh arena should have space")
            }
        };
        held.push(buf);
        // Rotate to retire the current arena (lease keeps it in drain queue)
        pool.rotate().unwrap();
    }
    held
}

fn main() {
    let duration_secs: u64 = env_or("FLAMEGRAPH_DURATION_SECS", 5);
    let buf_size: usize = env_or("FLAMEGRAPH_BUF_SIZE", 64);
    let output_path: String =
        env_or("FLAMEGRAPH_OUTPUT", "target/flamegraph-collect.svg".to_string());
    let queue_depth: usize = env_or("FLAMEGRAPH_QUEUE_DEPTH", 50);

    let config = PoolConfig {
        arena_size: 4096, // Small arenas to allow many rotations
        initial_arenas: 3,
        max_total_arenas: queue_depth + 10, // room for the drain queue
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();

    // Build initial drain queue
    let mut held = build_drain_queue(&pool, buf_size, queue_depth);

    // Warm up collect() path
    for _ in 0..100 {
        pool.collect();
    }

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(10_000)
        .build()
        .expect("failed to start profiler");

    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let mut iters = 0u64;
    let mut rebuilds = 0u64;

    'outer: loop {
        for _ in 0..CLOCK_CHECK_INTERVAL {
            // Release one held lease per iteration so collect() has work to do
            if let Some(buf) = held.pop() {
                drop(buf);
            }
            let collected = pool.collect();
            black_box(collected);

            // When drain queue is empty, rebuild it
            if held.is_empty() {
                held = build_drain_queue(&pool, buf_size, queue_depth);
                rebuilds += 1;
            }
        }
        iters += CLOCK_CHECK_INTERVAL;
        if start.elapsed() >= duration {
            break 'outer;
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "Completed {iters} collect() iterations ({rebuilds} rebuilds) in {:.2}s ({:.1}ns/iter)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / iters as f64,
    );

    let report = guard.report().build().unwrap();
    let mut opts = Options::default();
    opts.title = "Turbine collect() drain queue hot path".to_string();

    let file = std::fs::File::create(&output_path).unwrap();
    report.flamegraph_with_options(file, &mut opts).unwrap();
    eprintln!("Wrote {output_path}");
}
