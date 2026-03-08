use std::hint::black_box;
use std::time::{Duration, Instant};

use pprof::flamegraph::Options;
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

fn main() {
    let config = PoolConfig {
        arena_size: 4096 * 64, // 256 KiB — plenty of room
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();

    // Warm up
    for _ in 0..1_000 {
        let buf = pool.lease(64).unwrap();
        black_box(&buf);
        drop(buf);
    }

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(10_000) // 10 kHz sampling — high resolution for ~2ns ops
        .build()
        .expect("failed to start profiler");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut iters = 0u64;

    while Instant::now() < deadline {
        let buf = match pool.lease(64) {
            Some(buf) => buf,
            None => {
                pool.rotate().unwrap();
                pool.collect();
                pool.lease(64).expect("fresh arena should have space")
            }
        };
        black_box(&buf);
        drop(buf);
        iters += 1;
    }

    eprintln!("Completed {iters} lease/drop iterations in 5s");

    let report = guard.report().build().unwrap();
    let mut opts = Options::default();
    opts.title = "Turbine lease() hot path".to_string();

    let file = std::fs::File::create("flamegraph-lease.svg").unwrap();
    report.flamegraph_with_options(file, &mut opts).unwrap();
    eprintln!("Wrote flamegraph-lease.svg");
}
