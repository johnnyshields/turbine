use std::cell::UnsafeCell;
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use turbine_bench::{env_or, profiler_guard, write_flamegraph};
use turbine_core::buffer::pool::IouringBufferPool;
use turbine_core::config::PoolConfig;
use turbine_core::gc::NoopHooks;
use turbine_core::transfer::handle::SendableBuffer;

const RING_SIZE: usize = 64;
const RING_MASK: usize = RING_SIZE - 1;

/// Check the wall clock every this many iterations.
const CLOCK_CHECK_INTERVAL: u64 = 10_000;

#[repr(C, align(64))]
struct PaddedAtomic(AtomicUsize);

struct SpscRing<T> {
    buf: Box<[UnsafeCell<MaybeUninit<T>>; RING_SIZE]>,
    head: PaddedAtomic,
    tail: PaddedAtomic,
    closed: AtomicBool,
}

unsafe impl<T: Send> Send for SpscRing<T> {}
unsafe impl<T: Send> Sync for SpscRing<T> {}

impl<T> SpscRing<T> {
    fn new() -> Self {
        Self {
            buf: Box::new(std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit()))),
            head: PaddedAtomic(AtomicUsize::new(0)),
            tail: PaddedAtomic(AtomicUsize::new(0)),
            closed: AtomicBool::new(false),
        }
    }

    /// Push an item. Spins until space available. Returns false if closed.
    fn push(&self, val: T) -> bool {
        let tail = self.tail.0.load(Ordering::Relaxed);
        loop {
            let head = self.head.0.load(Ordering::Acquire);
            if tail.wrapping_sub(head) < RING_SIZE {
                break;
            }
            if self.closed.load(Ordering::Relaxed) {
                return false;
            }
            std::hint::spin_loop();
        }
        unsafe {
            (*self.buf[tail & RING_MASK].get()).write(val);
        }
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pop an item. Spins until data available. Returns None if closed and empty.
    fn pop(&self) -> Option<T> {
        let head = self.head.0.load(Ordering::Relaxed);
        loop {
            let tail = self.tail.0.load(Ordering::Acquire);
            if tail != head {
                break;
            }
            if self.closed.load(Ordering::Acquire) {
                // Re-load tail with Acquire to catch items pushed between our
                // last tail check and the close() Release store.
                let final_tail = self.tail.0.load(Ordering::Acquire);
                if final_tail != head {
                    break;
                }
                return None;
            }
            std::hint::spin_loop();
        }
        let val = unsafe { (*self.buf[head & RING_MASK].get()).assume_init_read() };
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(val)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl<T> Drop for SpscRing<T> {
    fn drop(&mut self) {
        // Drain any items remaining in the ring so they are properly dropped.
        // This is critical for SendableBuffer, whose Drop triggers remote_release.
        let head = *self.head.0.get_mut();
        let tail = *self.tail.0.get_mut();
        for i in head..tail {
            unsafe {
                (*self.buf[i & RING_MASK].get()).assume_init_drop();
            }
        }
    }
}

fn producer_loop(
    pool: &IouringBufferPool<NoopHooks>,
    ring: &SpscRing<SendableBuffer>,
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
            ring.push(sendable);
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
        "target/flamegraph-spsc.svg".to_string(),
    );

    let config = PoolConfig {
        arena_size: 4096 * 64, // 256 KiB — plenty of room
        initial_arenas: 3,
        page_size: 4096,
        ..Default::default()
    };
    let pool = IouringBufferPool::new(config, NoopHooks).unwrap();
    pool.pre_register_slots();

    let ring = Arc::new(SpscRing::<SendableBuffer>::new());
    let ring_consumer = Arc::clone(&ring);

    // Consumer thread: pop and drop (triggers remote_returns.fetch_add)
    let consumer = std::thread::spawn(move || {
        let mut count = 0u64;
        while let Some(buf) = ring_consumer.pop() {
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
        ring.push(sendable);
    }
    // Drain warmup buffers
    std::thread::sleep(Duration::from_millis(10));

    let guard = profiler_guard();

    let duration = Duration::from_secs(duration_secs);
    let (iters, elapsed) = producer_loop(&pool, &ring, buf_size, duration);

    // Signal consumer to stop
    ring.close();
    let consumer_count = consumer.join().unwrap();

    eprintln!(
        "Producer: {iters} sends in {:.2}s ({:.1}ns/iter)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / iters as f64,
    );
    eprintln!("Consumer: {consumer_count} receives");

    write_flamegraph(guard, "Turbine SPSC cross-thread transfer hot path", &output_path);
}
