//! Deterministic tests for the release performance-gate policy.
//!
//! Wall-clock measurement is intentionally absent from the ordinary debug test
//! matrix. The ignored end-to-end gate in `perf_update.rs` runs through
//! `scripts/benchmark-port.sh`, which forces Cargo's release profile.

#[path = "support/perf_policy.rs"]
mod perf_policy;

use perf_policy::{
    BASE_UPDATE_P95_NS, CLEAN_UPDATE_P95_NS, DIRTY_UPDATE_P95_NS, EngineKind, FAILURE_P95_NS,
    FEATURE_UPDATE_P95_NS, FIRST_SPAWN_NS, HELP_P95_NS, HISTORICAL_CLEAN_UPDATE_P95_NS,
    HISTORICAL_DIRTY_UPDATE_P95_NS, HISTORICAL_HELP_MEAN_NS, HISTORICAL_VERSION_MEAN_NS,
    MAX_RUST_PERCENT, REQUIRED_WORKLOADS, RUNS, STARTUP_CI_HEADROOM_PERCENT,
    STARTUP_PREFLIGHT_PAIRS, STARTUP_WARMUPS, UPDATE_CI_HEADROOM_PERCENT, UPDATE_WARMUPS,
    VERSION_P95_NS, WORKLOAD_POLICIES, Workload, budget_headroom_percent, meets_relative_gate,
    pair_order, ratio_percent_ceil, shell_baseline_sha, startup_warmup_schedule, summarize,
};

#[test]
fn performance_policy_pins_reachable_shell_baseline() {
    assert_eq!(
        shell_baseline_sha(),
        "b502904d9b848288799318826e507e0827fd97bf"
    );
    assert_eq!(shell_baseline_sha().len(), 40);
    assert!(
        shell_baseline_sha()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
}

#[test]
fn performance_policy_uses_meaningful_samples_after_warmup() {
    assert!(std::hint::black_box(RUNS) >= 30);
    assert!(std::hint::black_box(STARTUP_WARMUPS) > 0);
    assert!(std::hint::black_box(UPDATE_WARMUPS) > 0);
}

#[test]
fn paired_order_is_exactly_balanced() {
    let shell_first = (0..RUNS)
        .filter(|iteration| pair_order(*iteration)[0] == EngineKind::Shell)
        .count();
    let rust_first = RUNS - shell_first;

    assert_eq!(shell_first, RUNS / 2);
    assert_eq!(rust_first, RUNS / 2);
    assert_eq!(EngineKind::Shell.label(), "shell");
    assert_eq!(EngineKind::Rust.label(), "rust");
}

#[test]
fn startup_preflight_and_remaining_warmups_match_the_declared_schedule() {
    assert_eq!(STARTUP_PREFLIGHT_PAIRS, 1);
    for workload in [Workload::Help, Workload::Version] {
        let schedule = startup_warmup_schedule(workload);
        assert_eq!(schedule.len(), STARTUP_WARMUPS);
        assert_eq!(schedule.len(), workload.policy().warmups_per_engine);
        assert_eq!(schedule[0], [EngineKind::Shell, EngineKind::Rust]);
        assert_eq!(
            schedule.iter().skip(STARTUP_PREFLIGHT_PAIRS).count(),
            STARTUP_WARMUPS - STARTUP_PREFLIGHT_PAIRS
        );
        assert!(
            schedule
                .iter()
                .all(|order| *order == [EngineKind::Shell, EngineKind::Rust])
        );
        for kind in [EngineKind::Shell, EngineKind::Rust] {
            assert_eq!(
                schedule
                    .iter()
                    .flatten()
                    .filter(|entry| **entry == kind)
                    .count(),
                STARTUP_WARMUPS
            );
        }
    }
}

#[test]
fn summary_uses_conventional_median_and_nearest_rank_p95() {
    let values = [40, 10, 30, 20];
    let stats = summarize(&values).expect("non-empty samples");
    assert_eq!(stats.median_ns, 25);
    assert_eq!(stats.p95_ns, 40);

    let values = (1..=30).collect::<Vec<_>>();
    let stats = summarize(&values).expect("non-empty samples");
    assert_eq!(stats.median_ns, 15);
    assert_eq!(stats.p95_ns, 29);
    assert_eq!(values, (1..=30).collect::<Vec<_>>());
    assert_eq!(summarize(&[]), None);
    assert_eq!(
        summarize(&[u128::MAX, u128::MAX])
            .expect("maximum samples")
            .median_ns,
        u128::MAX
    );
}

#[test]
fn relative_gate_requires_at_least_twenty_five_percent_improvement() {
    assert_eq!(MAX_RUST_PERCENT, 75);
    assert!(meets_relative_gate(75, 100, MAX_RUST_PERCENT));
    assert!(!meets_relative_gate(76, 100, MAX_RUST_PERCENT));
    assert!(!meets_relative_gate(51, 100, 50));
    assert!(!meets_relative_gate(u128::MAX, u128::MAX, 75));
}

#[test]
fn absolute_release_budgets_remain_explicit() {
    assert_eq!(STARTUP_CI_HEADROOM_PERCENT, 125);
    assert_eq!(UPDATE_CI_HEADROOM_PERCENT, 150);
    assert_eq!(HISTORICAL_HELP_MEAN_NS, 10_700_000);
    assert_eq!(HISTORICAL_VERSION_MEAN_NS, 10_600_000);
    assert_eq!(HISTORICAL_CLEAN_UPDATE_P95_NS, 641_000_000);
    assert_eq!(HISTORICAL_DIRTY_UPDATE_P95_NS, 804_000_000);
    assert_eq!(HELP_P95_NS, 24_075_000);
    assert_eq!(VERSION_P95_NS, 23_850_000);
    assert_eq!(FIRST_SPAWN_NS, 100_000_000);
    assert_eq!(BASE_UPDATE_P95_NS, 4_000_000_000);
    assert_eq!(CLEAN_UPDATE_P95_NS, 1_602_500_000);
    assert_eq!(DIRTY_UPDATE_P95_NS, 2_010_000_000);
    assert_eq!(FEATURE_UPDATE_P95_NS, 12_000_000_000);
    assert_eq!(FAILURE_P95_NS, 4_000_000_000);
}

#[test]
fn normative_spec_matches_the_enforced_performance_policy() {
    let spec = include_str!("../docs/rust-port-spec.md");
    for requirement in [
        "median no more than 75% of Bash; p95 no more than 24.075ms",
        "median no more than 75% of Bash; p95 no more than 23.85ms",
        "median no more than 75% of Bash; p95 no more than 4s",
        "median no more than 75% of Bash; p95 no more than 1.6025s",
        "median no more than 75% of Bash; p95 no more than 2.01s",
        "median no more than 75% of Bash; p95 no more than 12s",
    ] {
        assert!(spec.contains(requirement), "missing policy: {requirement}");
    }
    assert_eq!(
        spec.matches("median no more than 75% of Bash").count(),
        WORKLOAD_POLICIES
            .iter()
            .filter(|policy| policy.max_rust_percent == Some(MAX_RUST_PERCENT))
            .count()
    );
}

#[test]
fn completion_spec_workloads_remain_in_the_gate() {
    assert_eq!(
        REQUIRED_WORKLOADS,
        [
            "first-spawn",
            "help",
            "version",
            "base-clean",
            "disjoint-clean",
            "disjoint-dirty",
            "profile-provider-hooks-collision",
            "pre-sync-failure",
        ]
    );
}

#[test]
fn workload_policy_is_the_single_budget_and_sampling_authority() {
    assert_eq!(WORKLOAD_POLICIES.len(), 8);
    for policy in WORKLOAD_POLICIES {
        assert_eq!(policy.workload.policy(), policy);
        assert!(policy.samples_per_engine > 0);
        assert!(policy.rust_p95_budget_ns > 0);
    }
    for policy in WORKLOAD_POLICIES
        .iter()
        .filter(|policy| policy.workload != Workload::FirstSpawn)
    {
        assert_eq!(
            policy.max_rust_percent,
            Some(MAX_RUST_PERCENT),
            "{} must reject a materially slower-than-shell native result",
            policy.workload.label()
        );
        assert!(meets_relative_gate(75, 100, MAX_RUST_PERCENT));
        assert!(!meets_relative_gate(100, 100, MAX_RUST_PERCENT));
    }
    assert_eq!(
        Workload::FeatureCollision.policy().max_rust_percent,
        Some(MAX_RUST_PERCENT),
        "the expensive composed workload must preserve measured relative speedup"
    );
}

#[test]
fn calibration_ratios_and_headroom_are_recomputed_from_raw_values() {
    for (shell, rust, rust_p95, budget, expected_ratio, expected_headroom) in [
        (None, 25, 25, 100, None, 75),
        (Some(400), 100, 120, 1_000, Some(25), 88),
        (Some(3), 2, 5, 10, Some(67), 50),
    ] {
        assert_eq!(
            shell.and_then(|value| ratio_percent_ceil(rust, value)),
            expected_ratio
        );
        assert_eq!(budget_headroom_percent(rust_p95, budget), expected_headroom);
    }
    assert_eq!(ratio_percent_ceil(1, 0), None);
    assert_eq!(budget_headroom_percent(101, 100), 0);
}
