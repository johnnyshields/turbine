use std::hint::black_box;
use std::time::{Duration, Instant};

use turbine_bench::{env_or, profiler_guard, write_flamegraph};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

/// Check the wall clock every this many iterations.
/// Each iteration includes a lease + drop and occasional rotate + collect,
/// so 10k iters ≈ 1ms — invisible to the profiler.
const CLOCK_CHECK_INTERVAL: u64 = 10_000;

fn main() {
    let duration_secs: u64 = env_or("FLAMEGRAPH_DURATION_SECS", 5);
    let buf_size: usize = env_or("FLAMEGRAPH_BUF_SIZE", 64);
    let output_path: String =
        env_or("FLAMEGRAPH_OUTPUT", "target/flamegraph-rotate.svg".to_string());

    // Small arena: force frequent rotation. Round up to page alignment.
    let arena_size = ((buf_size * 6) + 4095) & !4095;
    let config = PoolConfig {
        arena_size,
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();
    pool.pre_register_slots();

    // Warm up: force a few rotations
    for _ in 0..20 {
        while pool.lease(buf_size).is_some() {
            // exhaust current arena
        }
        pool.rotate().unwrap();
        pool.collect();
    }

    let guard = profiler_guard();

    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let mut iters = 0u64;
    let mut rotations = 0u64;

    'outer: loop {
        for _ in 0..CLOCK_CHECK_INTERVAL {
            match pool.lease(buf_size) {
                Some(buf) => {
                    black_box(&buf);
                    drop(buf);
                }
                None => {
                    pool.rotate().unwrap();
                    pool.collect();
                    rotations += 1;
                    let buf = pool.lease(buf_size).expect("fresh arena should have space");
                    black_box(&buf);
                    drop(buf);
                }
            }
        }
        iters += CLOCK_CHECK_INTERVAL;
        if start.elapsed() >= duration {
            break 'outer;
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "Completed {iters} iterations ({rotations} rotations) in {:.2}s ({:.1}ns/iter)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / iters as f64,
    );

    write_flamegraph(guard, "Turbine rotate() hot path", &output_path);
}
