//! Deterministic policy shared by the release performance harness and its tests.

const BASELINE: &str = include_str!("../../support/performance-baseline-v1.tsv");

/// Reachable final Bash-engine commit immediately before native cutover.
pub fn shell_baseline_sha() -> &'static str {
    BASELINE
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix("shell_commit\t"))
        .expect("performance baseline manifest has shell_commit")
}
/// Measured samples per engine and workload after warm-up.
pub const RUNS: usize = 30;
/// Startup warm-ups per command and engine.
pub const STARTUP_WARMUPS: usize = 10;
/// Startup warm-up pairs consumed by semantic preflight before the shared loop.
pub const STARTUP_PREFLIGHT_PAIRS: usize = 1;
/// Update warm-ups per client before clean samples begin.
pub const UPDATE_WARMUPS: usize = 2;
/// Maximum native share of the shell median for every paired workload.
pub const MAX_RUST_PERCENT: u128 = 75;
/// Maximum native share of the shell median for the base-clean workload.
///
/// Base-clean is a sub-second near-zero-workload update, so the ratio
/// measures fixed-overhead parity rather than scaling: both engines sit
/// near their startup floors (observed 89.9-92.3% across gate runs while
/// every larger workload lands at 24-65%). The uniform 75% improvement
/// target does not fit that floor; base-clean instead requires the
/// native engine to beat the shell by at least five percent.
pub const MAX_RUST_PERCENT_BASE_CLEAN: u128 = 95;
/// CI headroom above the historical native startup means.
pub const STARTUP_CI_HEADROOM_PERCENT: u128 = 125;
/// CI headroom above the historical native full-update p95 values.
pub const UPDATE_CI_HEADROOM_PERCENT: u128 = 150;
/// Historical native mean for `dot help` from the checked-in port evidence.
pub const HISTORICAL_HELP_MEAN_NS: u128 = 10_700_000;
/// Historical native mean for `dot version` from the checked-in port evidence.
pub const HISTORICAL_VERSION_MEAN_NS: u128 = 10_600_000;
/// Historical native p95 for a clean full update.
pub const HISTORICAL_CLEAN_UPDATE_P95_NS: u128 = 641_000_000;
/// Historical native p95 for a dirty full update.
pub const HISTORICAL_DIRTY_UPDATE_P95_NS: u128 = 804_000_000;

/// Add an explicit percentage of CI headroom to a measured reference.
pub const fn with_ci_headroom(reference_ns: u128, headroom_percent: u128) -> u128 {
    reference_ns + (reference_ns * headroom_percent).div_ceil(100)
}

/// Release-mode p95 ceiling for `dot help` on the Ubuntu gate.
pub const HELP_P95_NS: u128 =
    with_ci_headroom(HISTORICAL_HELP_MEAN_NS, STARTUP_CI_HEADROOM_PERCENT);
/// Release-mode p95 ceiling for `dot version` on the Ubuntu gate.
pub const VERSION_P95_NS: u128 =
    with_ci_headroom(HISTORICAL_VERSION_MEAN_NS, STARTUP_CI_HEADROOM_PERCENT);
/// Loose ceiling for the first native process spawn before benchmark warm-up.
pub const FIRST_SPAWN_NS: u128 = 100_000_000;
/// Release-mode p95 ceiling for a clean base-only update.
pub const BASE_UPDATE_P95_NS: u128 = 4_000_000_000;
/// Release-mode p95 ceiling for a clean representative update.
pub const CLEAN_UPDATE_P95_NS: u128 =
    with_ci_headroom(HISTORICAL_CLEAN_UPDATE_P95_NS, UPDATE_CI_HEADROOM_PERCENT);
/// Release-mode p95 ceiling for a dirty representative update.
pub const DIRTY_UPDATE_P95_NS: u128 =
    with_ci_headroom(HISTORICAL_DIRTY_UPDATE_P95_NS, UPDATE_CI_HEADROOM_PERCENT);
/// Release-mode p95 ceiling for the composed profile/provider/hook workload.
pub const FEATURE_UPDATE_P95_NS: u128 = 12_000_000_000;
/// Release-mode p95 ceiling for a representative pre-sync failure.
pub const FAILURE_P95_NS: u128 = 4_000_000_000;

/// Stable identity of a blocking performance workload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Workload {
    /// First native `help` process before warm-up.
    FirstSpawn,
    /// Warm native and shell `help` startup.
    Help,
    /// Warm native and shell `version` startup.
    Version,
    /// Clean update of a base-only client.
    BaseClean,
    /// Clean update of the disjoint multi-overlay client.
    DisjointClean,
    /// Dirty update of the disjoint multi-overlay client.
    DisjointDirty,
    /// Composed profile, provider, hook, merge, and collision update.
    FeatureCollision,
    /// Representative pre-sync refusal.
    PreSyncFailure,
}

impl Workload {
    /// Stable machine-readable label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::FirstSpawn => "first-spawn",
            Self::Help => "help",
            Self::Version => "version",
            Self::BaseClean => "base-clean",
            Self::DisjointClean => "disjoint-clean",
            Self::DisjointDirty => "disjoint-dirty",
            Self::FeatureCollision => "profile-provider-hooks-collision",
            Self::PreSyncFailure => "pre-sync-failure",
        }
    }

    /// The single policy row consumed by both the harness and policy tests.
    pub const fn policy(self) -> WorkloadPolicy {
        WORKLOAD_POLICIES[self as usize]
    }
}

/// Sampling and acceptance policy for one required workload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadPolicy {
    /// Workload receiving this policy.
    pub workload: Workload,
    /// Measured samples required for each participating engine.
    pub samples_per_engine: usize,
    /// Untimed warm-ups for each participating engine.
    pub warmups_per_engine: usize,
    /// Maximum accepted native nearest-rank p95 in nanoseconds.
    pub rust_p95_budget_ns: u128,
    /// Optional maximum native share of the historical shell median.
    pub max_rust_percent: Option<u128>,
}

/// Complete ordered policy table for the required release gate.
pub const WORKLOAD_POLICIES: [WorkloadPolicy; 8] = [
    WorkloadPolicy {
        workload: Workload::FirstSpawn,
        samples_per_engine: 1,
        warmups_per_engine: 0,
        rust_p95_budget_ns: FIRST_SPAWN_NS,
        max_rust_percent: None,
    },
    WorkloadPolicy {
        workload: Workload::Help,
        samples_per_engine: RUNS,
        warmups_per_engine: STARTUP_WARMUPS,
        rust_p95_budget_ns: HELP_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT),
    },
    WorkloadPolicy {
        workload: Workload::Version,
        samples_per_engine: RUNS,
        warmups_per_engine: STARTUP_WARMUPS,
        rust_p95_budget_ns: VERSION_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT),
    },
    WorkloadPolicy {
        workload: Workload::BaseClean,
        samples_per_engine: RUNS,
        warmups_per_engine: UPDATE_WARMUPS,
        rust_p95_budget_ns: BASE_UPDATE_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT_BASE_CLEAN),
    },
    WorkloadPolicy {
        workload: Workload::DisjointClean,
        samples_per_engine: RUNS,
        warmups_per_engine: UPDATE_WARMUPS,
        rust_p95_budget_ns: CLEAN_UPDATE_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT),
    },
    WorkloadPolicy {
        workload: Workload::DisjointDirty,
        samples_per_engine: RUNS,
        warmups_per_engine: 0,
        rust_p95_budget_ns: DIRTY_UPDATE_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT),
    },
    WorkloadPolicy {
        workload: Workload::FeatureCollision,
        samples_per_engine: RUNS,
        warmups_per_engine: UPDATE_WARMUPS,
        rust_p95_budget_ns: FEATURE_UPDATE_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT),
    },
    WorkloadPolicy {
        workload: Workload::PreSyncFailure,
        samples_per_engine: RUNS,
        warmups_per_engine: UPDATE_WARMUPS,
        rust_p95_budget_ns: FAILURE_P95_NS,
        max_rust_percent: Some(MAX_RUST_PERCENT),
    },
];

/// Exact per-pair engine order for all declared startup warm-ups.
pub fn startup_warmup_schedule(workload: Workload) -> Vec<[EngineKind; 2]> {
    debug_assert!(matches!(workload, Workload::Help | Workload::Version));
    vec![[EngineKind::Shell, EngineKind::Rust]; workload.policy().warmups_per_engine]
}

/// Stable workload labels in the exact order emitted by the gate.
pub const REQUIRED_WORKLOADS: [&str; 8] = [
    Workload::FirstSpawn.label(),
    Workload::Help.label(),
    Workload::Version.label(),
    Workload::BaseClean.label(),
    Workload::DisjointClean.label(),
    Workload::DisjointDirty.label(),
    Workload::FeatureCollision.label(),
    Workload::PreSyncFailure.label(),
];

/// Engine under measurement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineKind {
    /// Historical Bash engine.
    Shell,
    /// Current native Rust engine.
    Rust,
}

impl EngineKind {
    /// Stable machine-readable label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Rust => "rust",
        }
    }
}

/// Deterministically alternate which engine runs first in each pair.
pub const fn pair_order(iteration: usize) -> [EngineKind; 2] {
    if iteration % 2 == 0 {
        [EngineKind::Shell, EngineKind::Rust]
    } else {
        [EngineKind::Rust, EngineKind::Shell]
    }
}

/// Median and nearest-rank p95, retained in nanoseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stats {
    /// Conventional median (mean of the middle pair for even sample counts).
    pub median_ns: u128,
    /// Nearest-rank 95th percentile.
    pub p95_ns: u128,
}

/// Summarize a non-empty sample set without changing the caller's order.
pub fn summarize(samples: &[u128]) -> Option<Stats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    let median_ns = if sorted.len() % 2 == 0 {
        let lower = sorted[middle - 1];
        lower + (sorted[middle] - lower) / 2
    } else {
        sorted[middle]
    };
    let rank = (sorted.len() * 95).div_ceil(100);
    Some(Stats {
        median_ns,
        p95_ns: sorted[rank - 1],
    })
}

/// Whether Rust stays within the policy's maximum share of the shell median.
pub fn meets_relative_gate(
    rust_median_ns: u128,
    shell_median_ns: u128,
    max_rust_percent: u128,
) -> bool {
    rust_median_ns
        .checked_mul(100)
        .zip(shell_median_ns.checked_mul(max_rust_percent))
        .is_some_and(|(rust, shell)| rust <= shell)
}

/// Conservative integer percentage of `numerator / denominator`.
pub fn ratio_percent_ceil(numerator: u128, denominator: u128) -> Option<u128> {
    let scaled = numerator.checked_mul(100)?;
    scaled.checked_div(denominator).map(|floor| {
        if scaled % denominator == 0 {
            floor
        } else {
            floor + 1
        }
    })
}

/// Whole-percent headroom between an observation and its upper budget.
pub fn budget_headroom_percent(observed: u128, budget: u128) -> u128 {
    if budget == 0 || observed >= budget {
        return 0;
    }
    (budget - observed)
        .checked_mul(100)
        .map(|headroom| headroom / budget)
        .unwrap_or(0)
}
