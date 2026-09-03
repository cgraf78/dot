//! Performance budgets for the `dot` Rust port.
//!
//! CLI-level wall-clock budgets including process startup, following the
//! `hive-memory` `tests/perf_budget.rs` pattern: the user pays startup on
//! every invocation, so an in-process microbenchmark would miss the
//! latency that matters. p95 over repeated runs; heavy update-level
//! budgets (slice 2+) are `#[ignore]`-gated and run explicitly in CI.
//!
//! Gate jobs run these with `DOT_PERF_BUDGET_MULTIPLIER=1`. The
//! multiplier exists for slow developer hosts only.

use std::process::Command;
use std::time::{Duration, Instant};

/// Slice-1 startup budgets (reference host p95, ms).
const HELP_WARM_BUDGET_MS: u128 = 25;
const VERSION_WARM_BUDGET_MS: u128 = 30;
const RUNS: usize = 30;
const PERF_BUDGET_MULTIPLIER_ENV: &str = "DOT_PERF_BUDGET_MULTIPLIER";

fn budget_ms(base: u128) -> u128 {
    let multiplier: f64 = std::env::var(PERF_BUDGET_MULTIPLIER_ENV)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .filter(|parsed: &f64| parsed.is_finite() && *parsed > 0.0)
        .unwrap_or(1.0);
    ((base as f64) * multiplier) as u128
}

fn repeat(n: usize, mut op: impl FnMut() -> Duration) -> Vec<u128> {
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        samples.push(op().as_millis());
    }
    samples.sort_unstable();
    samples
}

fn p95_ms(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    let index = ((samples.len() as f64) * 0.95).ceil() as usize;
    samples[index.saturating_sub(1).min(samples.len() - 1)]
}

fn run_dot(args: &[&str]) -> Duration {
    // Null stdio: the child prints version/help text we do not assert on
    // here (parity owns that), and 30 inheriting children would flood
    // the test log and perturb timing with terminal writes.
    let start = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_dot"))
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run dot binary");
    assert!(status.success());
    start.elapsed()
}

#[test]
fn startup_stays_within_warm_budget() {
    // One warm-up run so the binary pages are hot, matching how the
    // budgets were calibrated.
    run_dot(&["help"]);
    let help_p95 = p95_ms(repeat(RUNS, || run_dot(&["help"])));
    let version_p95 = p95_ms(repeat(RUNS, || run_dot(&["version"])));
    eprintln!("dot help warm p95: {help_p95}ms");
    eprintln!("dot version warm p95: {version_p95}ms");
    assert!(
        help_p95 <= budget_ms(HELP_WARM_BUDGET_MS),
        "help p95 {help_p95}ms exceeds budget"
    );
    assert!(
        version_p95 <= budget_ms(VERSION_WARM_BUDGET_MS),
        "version p95 {version_p95}ms exceeds budget"
    );
}

#[test]
fn cold_start_with_cleared_caches_stays_reasonable() {
    // Cold page cache is host-state dependent, so this asserts a loose
    // ceiling (4x the warm budget), not parity: it catches linked-in
    // bloat (debug symbols, huge static constructors), not scheduling.
    // `vmtouch`style eviction is unavailable; drop nothing and accept
    // that "cold" here means first-run-in-this-test.
    let start = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_dot"))
        .arg("help")
        .env("DOT_PERF_COLD_RUN", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run dot binary");
    assert!(status.success());
    let elapsed = start.elapsed().as_millis();
    eprintln!("dot help cold: {elapsed}ms");
    assert!(
        elapsed <= budget_ms(HELP_WARM_BUDGET_MS * 4),
        "cold help {elapsed}ms exceeds ceiling"
    );
}
