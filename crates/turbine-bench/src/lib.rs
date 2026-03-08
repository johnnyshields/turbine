pub mod competitors;

pub const SIZES: &[usize] = &[64, 512, 4096, 65536];

/// Compute arena size for a given buffer size.
/// Ensures at least 64 buffers fit, rounded up to page alignment.
pub fn arena_size_for(buf_size: usize) -> usize {
    let min = buf_size * 64;
    let aligned = (min + 4095) & !4095; // next multiple of 4096
    aligned.max(4096)
}

/// Read an environment variable, falling back to `default` if unset or unparseable.
pub fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Build a pprof profiler guard with configurable frequency.
///
/// Frequency defaults to 10 000 Hz but can be overridden via `FLAMEGRAPH_FREQUENCY`.
pub fn profiler_guard() -> pprof::ProfilerGuard<'static> {
    let freq: i32 = env_or("FLAMEGRAPH_FREQUENCY", 10_000);
    pprof::ProfilerGuardBuilder::default()
        .frequency(freq)
        .build()
        .expect("failed to start profiler")
}

/// Write a flamegraph SVG from a profiler guard.
pub fn write_flamegraph(guard: pprof::ProfilerGuard, title: &str, output_path: &str) {
    let report = guard.report().build().unwrap();
    let mut opts = pprof::flamegraph::Options::default();
    opts.title = title.to_string();
    let file = std::fs::File::create(output_path).unwrap();
    report.flamegraph_with_options(file, &mut opts).unwrap();
    eprintln!("Wrote {output_path}");
}
