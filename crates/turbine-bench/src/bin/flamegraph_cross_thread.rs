use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use pprof::flamegraph::Options;
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;
use turbine_core::transfer::handle::SendableBuffer;

/// Check the wall clock every this many iterations.
/// Channel send is ~50-100ns, so 10k iters ≈ 0.5-1ms.
const CLOCK_CHECK_INTERVAL: u64 = 10_000;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn producer_loop(
    pool: &IouringBufferPool<NoopHooks>,
    tx: &Sender<SendableBuffer>,
    buf_size: usize,
    duration: Duration,
) -> (u64, Duration) {
    let start = Instant::now();
    let mut iters = 0u64;

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
            let sendable = buf.into_sendable();
            black_box(&sendable);
            tx.send(sendable).unwrap();
        }
        iters += CLOCK_CHECK_INTERVAL;
        if start.elapsed() >= duration {
            break 'outer;
        }
    }

    (iters, start.elapsed())
}

fn main() {
    let duration_secs: u64 = env_or("FLAMEGRAPH_DURATION_SECS", 5);
    let buf_size: usize = env_or("FLAMEGRAPH_BUF_SIZE", 64);
    let output_path: String = env_or(
        "FLAMEGRAPH_OUTPUT",
        "target/flamegraph-cross-thread.svg".to_string(),
    );

    let config = PoolConfig {
        arena_size: 4096 * 64, // 256 KiB — plenty of room
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();

    let (tx, rx) = bounded::<SendableBuffer>(64);

    // Consumer thread: receive and drop (triggers remote_returns.fetch_add)
    let consumer = std::thread::spawn(move || {
        let mut count = 0u64;
        while let Ok(buf) = rx.recv() {
            black_box(&buf);
            drop(buf);
            count += 1;
        }
        count
    });

    // Warm up
    for _ in 0..1_000 {
        let buf = pool.lease(buf_size).unwrap();
        let sendable = buf.into_sendable();
        tx.send(sendable).unwrap();
    }
    // Drain warmup buffers
    std::thread::sleep(Duration::from_millis(10));

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(10_000)
        .build()
        .expect("failed to start profiler");

    let duration = Duration::from_secs(duration_secs);
    let (iters, elapsed) = producer_loop(&pool, &tx, buf_size, duration);

    // Signal consumer to stop
    drop(tx);
    let consumer_count = consumer.join().unwrap();

    eprintln!(
        "Producer: {iters} sends in {:.2}s ({:.1}ns/iter)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / iters as f64,
    );
    eprintln!("Consumer: {consumer_count} receives");

    let report = guard.report().build().unwrap();
    let mut opts = Options::default();
    opts.title = "Turbine cross-thread transfer hot path".to_string();

    let file = std::fs::File::create(&output_path).unwrap();
    report.flamegraph_with_options(file, &mut opts).unwrap();
    eprintln!("Wrote {output_path}");
}
