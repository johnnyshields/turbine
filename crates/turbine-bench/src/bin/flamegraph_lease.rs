use std::hint::black_box;
use std::time::{Duration, Instant};

use turbine_bench::{env_or, profiler_guard, write_flamegraph};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

/// Check the wall clock every this many iterations.
/// At ~2ns/iter this is roughly every ~200µs — frequent enough for a
/// clean shutdown, rare enough to stay off the flamegraph.
const CLOCK_CHECK_INTERVAL: u64 = 100_000;

fn main() {
    let duration_secs: u64 = env_or("FLAMEGRAPH_DURATION_SECS", 5);
    let buf_size: usize = env_or("FLAMEGRAPH_BUF_SIZE", 64);
    let output_path: String =
        env_or("FLAMEGRAPH_OUTPUT", "target/flamegraph-lease.svg".to_string());

    let config = PoolConfig {
        arena_size: 4096 * 64, // 256 KiB — plenty of room
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();
    pool.pre_register_slots();

    // Warm up
    for _ in 0..1_000 {
        let buf = pool.lease(buf_size).unwrap();
        black_box(&buf);
        drop(buf);
    }

    let guard = profiler_guard();

    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let mut iters = 0u64;

    // Check the clock every CLOCK_CHECK_INTERVAL iterations, not every
    // iteration. At ~2ns/iter the inner batch is ~200µs — invisible to
    // the profiler but keeps us bounded to wall-clock time.
    'outer: loop {
        for _ in 0..CLOCK_CHECK_INTERVAL {
            let buf = match pool.lease(buf_size) {
                Some(buf) => buf,
                None => {
                    pool.rotate().unwrap();
                    pool.collect();
                    pool.lease(buf_size).expect("fresh arena should have space")
                }
            };
            black_box(&buf);
            drop(buf);
        }
        iters += CLOCK_CHECK_INTERVAL;
        if start.elapsed() >= duration {
            break 'outer;
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "Completed {iters} lease/drop iterations in {:.2}s ({:.1}ns/iter)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / iters as f64,
    );

    write_flamegraph(guard, "Turbine lease() hot path", &output_path);
}
