# Harden Flamegraph Binaries

## Context
Four flamegraph profiling binaries (`flamegraph_lease`, `flamegraph_rotate`, `flamegraph_cross_thread`, `flamegraph_collect`) share identical boilerplate: `env_or()` helper, pprof setup, flamegraph write-out. A comment in `flamegraph_rotate.rs` is misleading. The profiler frequency is hardcoded at 10kHz with no env override.

## Effort/Impact Table

| # | Opportunity | Effort | Impact | Action |
|---|-------------|--------|--------|--------|
| 1 | Extract `env_or()` to `lib.rs` | Quick | Medium | Auto-fix |
| 2 | Extract `write_flamegraph()` helper to `lib.rs` | Quick | Medium | Auto-fix |
| 3 | Fix misleading comment in `flamegraph_rotate.rs` | Quick | Low | Auto-fix |
| 4 | Add `FLAMEGRAPH_FREQUENCY` env var support | Easy | Low | Ask first |

## Opportunity Details

### #1 — Extract `env_or()` to `lib.rs`
- **What**: Move the `env_or<T: FromStr>()` function from each binary into `crates/turbine-bench/src/lib.rs` and import it
- **Where**: `lib.rs` + all 4 `flamegraph_*.rs` binaries
- **Why**: Identical 2-line function copy-pasted 4 times; single source of truth

### #2 — Extract `write_flamegraph()` helper to `lib.rs`
- **What**: Extract the 5-line pprof report → flamegraph SVG write pattern into a shared function: `pub fn write_flamegraph(guard: pprof::ProfilerGuard, title: &str, output_path: &str)`
- **Where**: `lib.rs` + all 4 binaries
- **Why**: Same report-build → options → file-create → write → eprintln pattern duplicated 4 times

### #3 — Fix misleading comment in `flamegraph_rotate.rs`
- **What**: The doc comment on `CLOCK_CHECK_INTERVAL` says "Rotation is ~100ns" but the loop also includes lease+drop per iteration. Fix to reflect total iteration cost.
- **Where**: `crates/turbine-bench/src/bin/flamegraph_rotate.rs:11-12`
- **Why**: Accuracy

### #4 — Add `FLAMEGRAPH_FREQUENCY` env var
- **What**: Make the pprof sampling frequency configurable via `FLAMEGRAPH_FREQUENCY` (default 10000). Either integrate into the `write_flamegraph` helper or add to each binary's env parsing.
- **Where**: All 4 binaries (or the shared helper)
- **Why**: Some workloads may want different sampling rates; all other params are already env-configurable

## Execution Protocol
**DO NOT implement any changes without user approval.**
For EACH opportunity, use `AskUserQuestion`.
Options: "Implement" / "Skip (add to TODO.md)" / "Do not implement"
Ask all questions before beginning any implementation work
(do NOT do alternating ask then implement, ask then implement, etc.)
After all items resolved, run: `cargo test --workspace && cargo clippy --workspace`
