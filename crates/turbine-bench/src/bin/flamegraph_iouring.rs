use std::hint::black_box;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use io_uring::{opcode, types, IoUring};
use turbine_bench::{env_or, profiler_guard, write_flamegraph};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;

/// Check the wall clock every this many iterations.
const CLOCK_CHECK_INTERVAL: u64 = 10_000;

fn main() {
    let mut ring = match IoUring::new(256) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "io_uring not available ({}). \
                 This binary requires Linux 5.6+ with io_uring support. \
                 WSL2 may not support io_uring depending on kernel version.",
                e
            );
            return;
        }
    };

    let duration_secs: u64 = env_or("FLAMEGRAPH_DURATION_SECS", 5);
    let buf_size: usize = env_or("FLAMEGRAPH_BUF_SIZE", 64);
    let output_path: String = env_or(
        "FLAMEGRAPH_OUTPUT",
        "target/flamegraph-iouring.svg".to_string(),
    );
    let rotate_interval: u64 = env_or("FLAMEGRAPH_ROTATE_INTERVAL", 1000);

    let devnull = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .expect("failed to open /dev/null");
    let fd = types::Fd(devnull.as_raw_fd());

    let config = PoolConfig {
        arena_size: 4096 * 64, // 256 KiB
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();

    // Register fixed buffers with io_uring.
    if let Err(e) = pool.register(&ring) {
        eprintln!(
            "Failed to register fixed buffers with io_uring ({}). \
             Falling back would require API changes; exiting.",
            e
        );
        return;
    }

    // Warm up: a few lease + write_fixed cycles.
    for _ in 0..100 {
        let mut buf = match pool.lease(buf_size) {
            Some(b) => b,
            None => {
                pool.unregister(&ring).unwrap();
                pool.rotate().unwrap();
                pool.collect();
                pool.register(&ring).unwrap();
                pool.lease(buf_size).expect("fresh arena should have space")
            }
        };
        buf.as_mut_slice().iter_mut().for_each(|b| *b = 0xAB);

        {
            let pinned = buf.pin_for_write();
            let sqe = opcode::WriteFixed::new(
                fd,
                pinned.as_ptr(),
                pinned.len() as u32,
                pinned.buf_index().as_u16(),
            )
            .build();

            unsafe { ring.submission().push(&sqe).expect("SQ full") };
            ring.submit_and_wait(1).expect("submit failed");

            let cqe = ring.completion().next().expect("no CQE");
            assert!(cqe.result() >= 0, "write_fixed failed: {}", cqe.result());
        }
    }

    let guard = profiler_guard();

    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let mut iters = 0u64;
    let mut since_rotate = 0u64;

    'outer: loop {
        for _ in 0..CLOCK_CHECK_INTERVAL {
            let mut buf = match pool.lease(buf_size) {
                Some(b) => b,
                None => {
                    // Arena full — rotate and re-register.
                    pool.unregister(&ring).unwrap();
                    pool.rotate().unwrap();
                    pool.collect();
                    pool.register(&ring).unwrap();
                    since_rotate = 0;
                    pool.lease(buf_size).expect("fresh arena should have space")
                }
            };

            // Write some data into the buffer.
            buf.as_mut_slice().iter_mut().for_each(|b| *b = 0xAB);

            {
                let pinned = buf.pin_for_write();
                let sqe = opcode::WriteFixed::new(
                    fd,
                    pinned.as_ptr(),
                    pinned.len() as u32,
                    pinned.buf_index().as_u16(),
                )
                .build();

                unsafe { ring.submission().push(&sqe).expect("SQ full") };
                ring.submit_and_wait(1).expect("submit failed");

                let cqe = ring.completion().next().expect("no CQE");
                black_box(cqe.result());
            }

            since_rotate += 1;
            if since_rotate >= rotate_interval {
                pool.unregister(&ring).unwrap();
                pool.rotate().unwrap();
                pool.collect();
                pool.register(&ring).unwrap();
                since_rotate = 0;
            }
        }
        iters += CLOCK_CHECK_INTERVAL;
        if start.elapsed() >= duration {
            break 'outer;
        }
    }

    let elapsed = start.elapsed();
    eprintln!(
        "Completed {iters} io_uring write_fixed iterations in {:.2}s ({:.1}ns/iter)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / iters as f64,
    );

    write_flamegraph(guard, "Turbine io_uring write_fixed hot path", &output_path);
}
