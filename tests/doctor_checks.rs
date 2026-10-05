//! Native behavioral tests for the doctor check family.

use std::path::Path;
use std::process::{Command, Stdio};

use dot::doctor_checks::{
    BaseRepoInputs, CronInputs, InstallInputs, LifecycleInputs, MergeInputs, MergeSpec,
    OverlayInputs, ProviderInputs, ProviderInstaller, Record, check_base_repo,
    check_cron_freshness, check_install_layout, check_merges, check_overlays,
    check_profile_lifecycle, check_provider, check_reexec_checkpoint, check_update_lock,
    completed_identity_matches_home, is_client_checkout, parse_status_v2, render, shdeps_binary,
};
use dot_test_support::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn executable(path: &Path) {
    dot_test_support::install_fixture_executable(path, b"#!/bin/sh\nexit 0\n", 0o755)
        .expect("install fixture");
}

fn valid_local(_: &str) -> Result<(), String> {
    Ok(())
}

#[test]
fn renderer_has_stable_protocol() {
    let records = vec![
        Record::section("Heads"),
        Record::ok("fine", Some("detail".into())),
        Record::warn("careful", Some("detail".into())),
        Record::fail("broken", None),
        Record::skip("later", None),
    ];
    assert_eq!(
        render(&records),
        "\nHeads\n  ✓ fine (detail)\n  ⚠ careful\n    detail\n  ✗ broken\n  · later\n"
    );
}

#[test]
fn update_lock_resolution_clear_and_unsafe() {
    let scratch = TempDir::new("doctor-lock-native").expect("scratch");
    let missing = scratch.path().join("missing");
    assert!(render(&check_update_lock(None)).contains("path cannot be resolved"));
    assert!(render(&check_update_lock(Some(&missing))).contains("update lock is clear"));
    let file = scratch.path().join("file");
    std::fs::write(&file, b"unsafe").expect("write");
    assert!(render(&check_update_lock(Some(&file))).contains("path is unsafe"));
}

#[test]
fn update_lock_initializing_incomplete_live_and_stale() {
    let scratch = TempDir::new("doctor-lock-states").expect("scratch");
    let fresh = scratch.path().join("fresh");
    std::fs::create_dir_all(&fresh).expect("fresh lock");
    assert!(render(&check_update_lock(Some(&fresh))).contains("being initialized"));

    let aged = scratch.path().join("aged");
    std::fs::create_dir_all(&aged).expect("aged lock");
    let status = Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(&aged)
        .status()
        .expect("age lock");
    assert!(status.success());
    assert!(render(&check_update_lock(Some(&aged))).contains("record is incomplete"));

    let state = scratch.path().join("state");
    let log = dot::log::Log::new(false, false);
    let mut sink = Vec::new();
    let guard = dot::update_lock::acquire(&state, false, &log, None, &mut sink).expect("lock");
    let live = dot::update_lock::lock_path(&state);
    let output = render(&check_update_lock(Some(&live)));
    assert!(output.contains("update is currently running"));
    assert!(output.contains(&format!("pid {}", std::process::id())));
    drop(guard);

    let stale = scratch.path().join("stale");
    std::fs::create_dir_all(&stale).expect("stale lock");
    std::fs::write(
        stale.join("owner"),
        "pid\t42424242\nstart\tproc:1\ntoken\tstale\n",
    )
    .expect("owner");
    assert!(render(&check_update_lock(Some(&stale))).contains("owner is stale"));
}

#[test]
fn merge_inventory_branch_matrix() {
    let scratch = TempDir::new("doctor-merges-native").expect("scratch");
    let ext = scratch.path().join("extensions");
    // Handoff finding #8 added `specs` for output verification;
    // empty keeps this matrix on the historical count-only branches.
    let check = |enabled, count| {
        render(&check_merges(&MergeInputs {
            enabled,
            extensions_dir: ext.to_string_lossy().into_owned(),
            spec_count: count,
            inventory_error: None,
            specs: vec![],
        }))
    };
    assert!(check(false, None).contains("no extension root configured"));
    assert!(check(true, Some(0)).contains("none configured"));
    std::fs::create_dir_all(ext.join("merge-hooks.d")).expect("hooks");
    assert!(check(true, None).contains("inventory is invalid"));
    assert!(check(true, Some(2)).contains("2 hook(s)"));
}

#[test]
fn cron_freshness_trips_on_stale_and_passes_on_fresh() {
    let check = |last: Option<i64>| {
        render(&check_cron_freshness(&CronInputs {
            last_success: last,
            last_converged: None,
            last_run: None,
            last_failure: None,
            cron_available: true,
            now: 1_800_000_000,
        }))
    };
    let missing = check(None);
    assert!(missing.contains("· cron update success is unknown"));
    assert!(missing.contains("no successful cron update recorded"));
    assert!(!missing.contains('✗'));

    let fresh = check(Some(1_800_000_000 - 60));
    assert!(fresh.contains("✓ cron update succeeded recently"));
    assert!(!fresh.contains('✗'));

    // Exactly at the threshold reads fresh (staleness is strict).
    let boundary = check(Some(1_800_000_000 - 7200));
    assert!(boundary.contains("✓ cron update succeeded recently"));

    // Clock skew (a future stamp) never trips.
    let future = check(Some(1_800_000_000 + 60));
    assert!(future.contains("✓ cron update succeeded recently"));

    let stale = check(Some(1_800_000_000 - 7201));
    assert!(stale.contains("⚠ cron update has not succeeded recently"));
    assert!(stale.contains("last success 2h0m ago"));
    assert!(!stale.contains('✗'));

    // Fresh-review-B B3: a hostile `i64::MIN` stamp saturates to
    // stale instead of overflowing the age subtraction (and an
    // `i64::MAX` stamp reads fresh, never panics).
    let hostile = check(Some(i64::MIN));
    assert!(hostile.contains("⚠ cron update has not succeeded recently"));
    assert!(!hostile.contains('✗'));
    let far_future = check(Some(i64::MAX));
    assert!(far_future.contains("✓ cron update succeeded recently"));
}

#[test]
fn cron_freshness_separates_degraded_convergence_from_a_frozen_host() {
    const NOW: i64 = 1_800_000_000;
    let converged = |at: i64, failing: &str| dot::update_status::Converged {
        at,
        failing: failing.to_string(),
    };
    let check = |last: Option<i64>, conv: Option<dot::update_status::Converged>| {
        render(&check_cron_freshness(&CronInputs {
            last_success: last,
            last_converged: conv,
            last_run: None,
            last_failure: None,
            cron_available: true,
            now: NOW,
        }))
    };

    // Converging but Prune keeps failing: degraded, naming the stage and
    // how long the host has not been fully clean.
    let degraded = check(Some(NOW - 5 * 3600), Some(converged(NOW - 600, "prune")));
    assert!(
        degraded.contains("⚠ cron update degraded: prune failing"),
        "{degraded}"
    );
    assert!(
        degraded.contains("since last success 5h0m ago; last converged 10m ago"),
        "{degraded}"
    );
    assert!(
        !degraded.contains("has not succeeded recently"),
        "{degraded}"
    );
    assert!(!degraded.contains('✗'), "{degraded}");

    // Degraded since the first run after an upgrade that never ran clean.
    let never_clean = check(None, Some(converged(NOW - 60, "tools,prune")));
    assert!(
        never_clean.contains("⚠ cron update degraded: tools,prune failing"),
        "{never_clean}"
    );
    assert!(
        never_clean.contains("no clean cron update recorded; last converged 1m ago"),
        "{never_clean}"
    );

    // A recent clean run wins over a newer degraded one inside the
    // tolerance window, exactly like a transient failure today.
    let transient = check(Some(NOW - 1800), Some(converged(NOW - 60, "prune")));
    assert!(
        transient.contains("✓ cron update succeeded recently"),
        "{transient}"
    );

    // A clean convergence stamp counts as success even when the
    // last-success write was lost.
    let clean = check(None, Some(converged(NOW - 60, "")));
    assert!(
        clean.contains("✓ cron update succeeded recently"),
        "{clean}"
    );

    // Convergence stopped too: the frozen warning, noting when the host
    // last converged degraded.
    let frozen = check(
        Some(NOW - 9 * 3600),
        Some(converged(NOW - 3 * 3600, "prune")),
    );
    assert!(
        frozen.contains("⚠ cron update has not succeeded recently"),
        "{frozen}"
    );
    assert!(
        frozen.contains("last success 9h0m ago; last converged 3h0m ago"),
        "{frozen}"
    );
    // A lone stale clean convergence stamp is a stale success, not "no
    // successful cron update recorded".
    let stale_clean = check(None, Some(converged(NOW - 3 * 3600, "")));
    assert!(
        stale_clean.contains("last success 3h0m ago") && !stale_clean.contains("no successful"),
        "{stale_clean}"
    );
    let frozen_never_clean = check(None, Some(converged(NOW - 3 * 3600, "tools")));
    assert!(
        frozen_never_clean.contains("⚠ cron update has not succeeded recently")
            && frozen_never_clean
                .contains("no successful cron update recorded; last converged 3h0m ago"),
        "{frozen_never_clean}"
    );

    // Skew: an older Dot (downgrade) keeps refreshing last-success but not
    // convergence; the stale degraded stamp must not mask its state.
    let downgraded_ok = check(Some(NOW - 60), Some(converged(NOW - 9 * 3600, "prune")));
    assert!(
        downgraded_ok.contains("✓ cron update succeeded recently"),
        "{downgraded_ok}"
    );
    let downgraded_failing = check(
        Some(NOW - 3 * 3600),
        Some(converged(NOW - 9 * 3600, "prune")),
    );
    assert!(
        downgraded_failing.contains("⚠ cron update has not succeeded recently"),
        "{downgraded_failing}"
    );
    assert!(
        downgraded_failing.contains("last success 3h0m ago")
            && !downgraded_failing.contains("last converged"),
        "an older convergence than success adds nothing: {downgraded_failing}"
    );
}

fn last_run(at: i64, outcome: &str, trigger: &str, failing: &str) -> dot::update_status::LastRun {
    dot::update_status::LastRun {
        at,
        outcome: outcome.to_string(),
        trigger: trigger.to_string(),
        failing: failing.to_string(),
    }
}

#[test]
fn cron_that_never_ran_only_skips_without_a_crontab() {
    // Termux and containers have no `crontab`: such a host is updated by
    // hand by design, so a lasting warning would be noise.
    const NOW: i64 = 1_800_000_000;
    let rows = render(&check_cron_freshness(&CronInputs {
        last_success: None,
        last_converged: None,
        last_run: Some(last_run(NOW - 9 * 3600, "ok", "manual", "")),
        last_failure: None,
        cron_available: false,
        now: NOW,
    }));
    assert!(
        rows.contains(
            "· cron update has never run (no crontab on PATH; last update: manual run 9h0m ago)"
        ),
        "{rows}"
    );
    assert!(!rows.contains('⚠'), "{rows}");
}

#[test]
fn cron_that_never_ran_warns_once_a_manual_update_is_stale() {
    const NOW: i64 = 1_800_000_000;
    let check = |run: Option<dot::update_status::LastRun>| {
        render(&check_cron_freshness(&CronInputs {
            last_success: None,
            last_converged: None,
            last_run: run,
            last_failure: None,
            cron_available: true,
            now: NOW,
        }))
    };
    // Nothing ever updated this host: still unknown.
    assert!(check(None).contains("· cron update success is unknown"));
    // Updated by hand recently: cron simply has not had a slot yet.
    let recent = check(Some(last_run(NOW - 600, "ok", "manual", "")));
    assert!(
        recent.contains("· cron update has not run yet (last update: manual run 10m ago)"),
        "{recent}"
    );
    assert!(recent.contains("✓ last update succeeded (manual run 10m ago)"));
    // A cron entry would have run by now: the frozen "unknown" becomes a
    // warning instead of staying skipped forever.
    let stale = check(Some(last_run(NOW - 3 * 3600, "ok", "init", "")));
    assert!(stale.contains("⚠ cron update has never run"), "{stale}");
    assert!(
        stale.contains("last update: init run 3h0m ago\n    → schedule dot update --cron"),
        "{stale}"
    );
    assert!(!stale.contains('✗'), "{stale}");
    // A cron that keeps skipping for local edits is running, not missing,
    // and the edits are named as what blocks it.
    let skipping = check(Some(last_run(NOW - 60, "skip", "cron", "")));
    assert!(
        skipping.contains("⚠ cron update skipping: local edits block it"),
        "{skipping}"
    );
    assert!(
        skipping.contains(
            "last cron run 1m ago; no successful cron update recorded\n    → run dot status"
        ),
        "{skipping}"
    );
    // A clean cron last run whose stamp writes were lost still counts.
    let lost_stamps = check(Some(last_run(NOW - 60, "ok", "cron", "")));
    assert!(
        lost_stamps.contains("✓ cron update succeeded recently (1m ago)"),
        "{lost_stamps}"
    );
    // Cron runs that only ever failed leave no stamp but a cron last run.
    let failing = check(Some(last_run(NOW - 60, "fail", "cron", "")));
    assert!(
        failing.contains("⚠ cron update has not succeeded recently"),
        "{failing}"
    );
    assert!(
        failing.contains("no successful cron update recorded; last cron run failed 1m ago"),
        "{failing}"
    );
    assert!(
        !failing.contains("last update"),
        "a cron last run is covered by the cron row: {failing}"
    );
}

#[test]
fn hand_run_update_rows_appear_only_when_they_add_information() {
    const NOW: i64 = 1_800_000_000;
    let check = |success: Option<i64>, run: dot::update_status::LastRun| {
        render(&check_cron_freshness(&CronInputs {
            last_success: success,
            last_converged: None,
            last_run: Some(run),
            last_failure: None,
            cron_available: true,
            now: NOW,
        }))
    };
    // A recent clean cron run vouches for the host: a successful manual
    // run adds nothing.
    let quiet = check(Some(NOW - 600), last_run(NOW - 60, "ok", "manual", ""));
    assert!(!quiet.contains("last update"), "{quiet}");
    // A manual failure after that clean run is news.
    let failed = check(Some(NOW - 600), last_run(NOW - 60, "fail", "manual", ""));
    assert!(failed.contains("✓ cron update succeeded recently"));
    assert!(failed.contains("⚠ last update failed"), "{failed}");
    assert!(
        failed.contains("manual run 1m ago\n    → run dot update for the full output"),
        "{failed}"
    );
    assert!(!failed.contains('✗'), "{failed}");
    // ...unless the clean cron run is newer.
    let healed = check(Some(NOW - 60), last_run(NOW - 600, "fail", "manual", ""));
    assert!(!healed.contains("last update"), "{healed}");
    // Degraded names the stages.
    let degraded = check(
        None,
        last_run(NOW - 60, "degraded", "manual", "tools,prune"),
    );
    assert!(
        degraded.contains("⚠ last update degraded: tools,prune failing"),
        "{degraded}"
    );
    // A stale clean cron run no longer vouches: a recent manual success shows.
    let stale_cron = check(Some(NOW - 9 * 3600), last_run(NOW - 60, "ok", "manual", ""));
    assert!(stale_cron.contains("⚠ cron update has not succeeded recently"));
    assert!(
        stale_cron.contains("✓ last update succeeded"),
        "{stale_cron}"
    );
    // Outcome words from a newer Dot still render as a non-success.
    let newer = check(None, last_run(NOW - 60, "aborted", "manual", ""));
    assert!(newer.contains("⚠ last update failed"), "{newer}");
}

/// A `update.last-failure` record for `run` with `items`.
fn failure_for(
    run: &dot::update_status::LastRun,
    items: &[(&str, &str, &str)],
    omitted: usize,
) -> dot::update_status::LastFailure {
    dot::update_status::LastFailure {
        at: run.at,
        outcome: run.outcome.clone(),
        trigger: run.trigger.clone(),
        items: items
            .iter()
            .map(|(stage, name, detail)| dot::update_status::FailureItem {
                stage: stage.to_string(),
                name: name.to_string(),
                detail: detail.to_string(),
            })
            .collect(),
        omitted,
    }
}

fn cron_rows(
    success: Option<i64>,
    run: Option<dot::update_status::LastRun>,
    failure: Option<dot::update_status::LastFailure>,
    now: i64,
) -> String {
    render(&check_cron_freshness(&CronInputs {
        last_success: success,
        last_converged: None,
        last_run: run,
        last_failure: failure,
        cron_available: true,
        now,
    }))
}

#[test]
fn a_newer_cron_failure_is_not_masked_by_a_recent_clean_run() {
    // U3: a clean run 100 minutes ago used to vouch for the host until it
    // aged out, so cron runs failing since then read green.
    const NOW: i64 = 1_800_000_000;
    let run = last_run(NOW - 600, "degraded", "cron", "tools");
    let failure = failure_for(
        &run,
        &[(
            "tools",
            "watchexec/watchexec",
            "ambiguous interrupted method transition",
        )],
        0,
    );
    let rows = cron_rows(Some(NOW - 6000), Some(run.clone()), Some(failure), NOW);
    assert!(
        rows.contains("⚠ last cron run degraded: tools failing"),
        "{rows}"
    );
    assert!(!rows.contains("succeeded recently"), "{rows}");
    assert!(
        rows.contains(
            "10m ago; last success 1h40m ago; failing: tools: watchexec/watchexec (ambiguous interrupted method transition)\n    → run shdeps health, or dot update for the full output"
        ),
        "{rows}"
    );
    assert!(!rows.contains('✗'), "{rows}");

    // A failed run names its stage's items and the plain next step.
    let fail = last_run(NOW - 60, "fail", "cron", "");
    let failure = failure_for(
        &fail,
        &[
            ("repos", "dotfiles", "pull failed"),
            ("repos", "work", "pull failed"),
        ],
        0,
    );
    let rows = cron_rows(Some(NOW - 600), Some(fail.clone()), Some(failure), NOW);
    assert!(rows.contains("⚠ last cron run failed"), "{rows}");
    assert!(
        rows.contains(
            "failing: repos: dotfiles (pull failed), work (pull failed)\n    → run dot update for the full output"
        ),
        "{rows}"
    );

    // An older Dot wrote the stamp but no record: the run still warns,
    // with only the next step.
    let rows = cron_rows(Some(NOW - 600), Some(fail), None, NOW);
    assert!(rows.contains("⚠ last cron run failed"), "{rows}");
    assert!(
        rows.contains("1m ago; last success 10m ago\n    → run dot update for the full output"),
        "{rows}"
    );

    // A provider that could not even be prepared is not `shdeps health`'s
    // to explain, even though the run degraded the Tools stage.
    let unavailable = last_run(NOW - 60, "degraded", "cron", "tools");
    let failure = failure_for(
        &unavailable,
        &[(
            "provider",
            "shdeps",
            "shdeps unavailable; dependency install skipped",
        )],
        0,
    );
    let rows = cron_rows(Some(NOW - 600), Some(unavailable), Some(failure), NOW);
    assert!(
        rows.contains("failing: provider: shdeps (shdeps unavailable; dependency install skipped)\n    → run dot update for the full output"),
        "{rows}"
    );

    // A clean run after the failure heals it.
    let healed = cron_rows(
        Some(NOW - 60),
        Some(last_run(NOW - 600, "fail", "cron", "")),
        None,
        NOW,
    );
    assert!(
        healed.contains("✓ cron update succeeded recently"),
        "{healed}"
    );
}

#[test]
fn a_cause_only_attaches_to_the_run_it_describes() {
    // A record left by an earlier run (or superseded by a run of an older
    // Dot, which writes no record) must never explain the current one.
    const NOW: i64 = 1_800_000_000;
    let run = last_run(NOW - 60, "fail", "cron", "");
    let older = last_run(NOW - 3600, "fail", "cron", "");
    let stale = failure_for(&older, &[("configs", "10-old-hook", "boom")], 0);
    let rows = cron_rows(Some(NOW - 600), Some(run.clone()), Some(stale), NOW);
    assert!(!rows.contains("10-old-hook"), "{rows}");
    let manual = last_run(NOW - 60, "fail", "manual", "");
    let other_trigger = failure_for(&manual, &[("configs", "10-hook", "boom")], 0);
    let rows = cron_rows(Some(NOW - 600), Some(run), Some(other_trigger), NOW);
    assert!(!rows.contains("10-hook"), "{rows}");
}

#[test]
fn causes_attach_to_stale_degraded_and_never_converged_rows() {
    const NOW: i64 = 1_800_000_000;
    let run = last_run(NOW - 60, "fail", "cron", "");
    let failure = failure_for(&run, &[("configs", "40-claude", "exit 3")], 0);
    // Stale success.
    let stale = cron_rows(
        Some(NOW - 9 * 3600),
        Some(run.clone()),
        Some(failure.clone()),
        NOW,
    );
    assert!(
        stale.contains("⚠ cron update has not succeeded recently"),
        "{stale}"
    );
    assert!(
        stale.contains(
            "last success 9h0m ago; last cron run failed 1m ago; failing: configs: 40-claude (exit 3)\n    → run dot update"
        ),
        "{stale}"
    );
    // Never converged.
    let never = cron_rows(None, Some(run.clone()), Some(failure.clone()), NOW);
    assert!(
        never.contains(
            "no successful cron update recorded; last cron run failed 1m ago; failing: configs: 40-claude (exit 3)"
        ),
        "{never}"
    );
    // Degraded convergence.
    let degraded_run = last_run(NOW - 60, "degraded", "cron", "tools");
    let degraded_failure = failure_for(&degraded_run, &[("tools", "ripgrep", "network")], 0);
    let degraded = render(&check_cron_freshness(&CronInputs {
        last_success: Some(NOW - 5 * 3600),
        last_converged: Some(dot::update_status::Converged {
            at: NOW - 60,
            failing: "tools".to_string(),
        }),
        last_run: Some(degraded_run),
        last_failure: Some(degraded_failure),
        cron_available: true,
        now: NOW,
    }));
    assert!(
        degraded.contains("⚠ cron update degraded: tools failing"),
        "{degraded}"
    );
    assert!(
        degraded.contains(
            "last converged 1m ago; failing: tools: ripgrep (network)\n    → run shdeps health"
        ),
        "{degraded}"
    );

    // A cron run that failed after that degraded convergence is the news:
    // its own title and cause, never the degraded stages' label.
    let newer = render(&check_cron_freshness(&CronInputs {
        last_success: Some(NOW - 5 * 3600),
        last_converged: Some(dot::update_status::Converged {
            at: NOW - 5400,
            failing: "tools".to_string(),
        }),
        last_run: Some(run.clone()),
        last_failure: Some(failure.clone()),
        cron_available: true,
        now: NOW,
    }));
    assert!(newer.contains("⚠ last cron run failed"), "{newer}");
    assert!(
        newer.contains(
            "1m ago; last success 5h0m ago; last converged 1h30m ago; failing: configs: 40-claude (exit 3)"
        ),
        "{newer}"
    );
    assert!(!newer.contains("degraded"), "{newer}");
}

#[test]
fn update_warn_rows_always_carry_a_next_step() {
    const NOW: i64 = 1_800_000_000;
    // A degraded convergence without the run that wrote it (a later hand
    // run, or an older Dot) still says where to look.
    let degraded = render(&check_cron_freshness(&CronInputs {
        last_success: Some(NOW - 5 * 3600),
        last_converged: Some(dot::update_status::Converged {
            at: NOW - 60,
            failing: "prune".to_string(),
        }),
        last_run: None,
        last_failure: None,
        cron_available: true,
        now: NOW,
    }));
    assert!(
        degraded.contains("last converged 1m ago\n    → run shdeps health, or dot update"),
        "{degraded}"
    );
    // A host whose cron simply stopped: check the schedule.
    let stopped = cron_rows(Some(NOW - 9 * 3600), None, None, NOW);
    assert!(
        stopped.contains(
            "last success 9h0m ago\n    → check that dot update --cron is scheduled (crontab -l), or run dot update"
        ),
        "{stopped}"
    );
    // A clean stamp from the future (the clock stepped back) cannot hide a
    // newer failing cron run.
    let run = last_run(NOW - 60, "fail", "cron", "");
    let skewed = cron_rows(Some(NOW + 3600), Some(run), None, NOW);
    assert!(skewed.contains("⚠ last cron run failed"), "{skewed}");
}

#[test]
fn many_failing_items_fold_into_a_count_and_long_details_shorten() {
    const NOW: i64 = 1_800_000_000;
    let run = last_run(NOW - 60, "degraded", "manual", "tools");
    let long = "x".repeat(300);
    let failure = failure_for(
        &run,
        &[
            ("tools", "a", &long),
            ("tools", "b", ""),
            ("tools", "c", "why"),
            ("tools", "d", "why"),
        ],
        2,
    );
    let rows = cron_rows(None, Some(run), Some(failure), NOW);
    assert!(
        rows.contains("⚠ last update degraded: tools failing"),
        "{rows}"
    );
    assert!(
        rows.contains(", b, c (why) +3 more\n    → run shdeps health"),
        "{rows}"
    );
    let shown = rows
        .lines()
        .find(|line| line.contains("failing:"))
        .expect("cause line");
    assert!(shown.contains('…') && shown.len() < 300, "{shown}");
}

#[test]
fn a_cron_run_skipped_for_local_edits_names_them() {
    // U4: the skip used to read "has not succeeded recently" once the last
    // clean run aged out, never saying that local edits block cron.
    const NOW: i64 = 1_800_000_000;
    let run = last_run(NOW - 60, "skip", "cron", "");
    let failure = failure_for(
        &run,
        &[
            ("dirty", ".bashrc", ""),
            ("dirty", ".zshrc", ""),
            ("dirty", ".profile", ""),
            ("dirty", ".inputrc", ""),
        ],
        1,
    );
    // Even right after a clean run: cron stays frozen until resolved.
    let fresh = cron_rows(
        Some(NOW - 600),
        Some(run.clone()),
        Some(failure.clone()),
        NOW,
    );
    assert!(
        fresh.contains("⚠ cron update skipping: local edits block it"),
        "{fresh}"
    );
    assert!(
        fresh.contains(
            "last cron run 1m ago; last success 10m ago; edited: .bashrc, .zshrc, .profile +2 more\n    → run dot status, then commit, stash, or resolve the edits"
        ),
        "{fresh}"
    );
    let stale = cron_rows(Some(NOW - 9 * 3600), Some(run.clone()), Some(failure), NOW);
    assert!(
        stale.contains("⚠ cron update skipping: local edits block it")
            && stale.contains("last success 9h0m ago; edited: .bashrc"),
        "{stale}"
    );
    assert!(!stale.contains("has not succeeded recently"), "{stale}");
    // A skip that is itself stale means cron stopped too: the frozen row,
    // still naming the edits of that last run.
    let old_skip = last_run(NOW - 48 * 3600, "skip", "cron", "");
    let old_failure = failure_for(&old_skip, &[("dirty", ".zshrc", "")], 0);
    let stopped = cron_rows(
        Some(NOW - 72 * 3600),
        Some(old_skip.clone()),
        Some(old_failure.clone()),
        NOW,
    );
    assert!(
        stopped.contains("⚠ cron update has not succeeded recently"),
        "{stopped}"
    );
    assert!(
        stopped.contains(
            "last success 72h0m ago; last cron run skipped for local edits 48h0m ago; edited: .zshrc\n    → run dot status"
        ),
        "{stopped}"
    );
    let never = cron_rows(None, Some(old_skip), Some(old_failure), NOW);
    assert!(
        never.contains(
            "no successful cron update recorded; last cron run skipped for local edits 48h0m ago; edited: .zshrc"
        ),
        "{never}"
    );
    // An older Dot recorded the skip without files: the row still names
    // the cause and the next step.
    let old = cron_rows(Some(NOW - 9 * 3600), Some(run), None, NOW);
    assert!(
        old.contains("last success 9h0m ago\n    → run dot status, then commit"),
        "{old}"
    );
}

#[test]
fn a_live_update_lock_shows_how_long_it_has_been_held() {
    // K5: a hung update showed only its pid.
    let scratch = TempDir::new("doctor-lock-age").expect("scratch");
    let state = scratch.path().join("state");
    let log = dot::log::Log::new(false, false);
    let guard =
        dot::update_lock::acquire(&state, false, &log, None, &mut Vec::new()).expect("lock");
    let live = dot::update_lock::lock_path(&state);
    let fresh = render(&check_update_lock(Some(&live)));
    assert!(fresh.contains("⚠ update is currently running"), "{fresh}");
    assert!(
        fresh.contains(&format!("pid {}, running for ", std::process::id())),
        "{fresh}"
    );
    // Held past the staleness window: most likely hung, with how to stop it.
    let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(5 * 3600 + 12 * 60);
    std::fs::File::options()
        .write(true)
        .open(dot::update_lock::owner_file(&live))
        .expect("owner file")
        .set_modified(aged)
        .expect("age owner");
    let hung = render(&check_update_lock(Some(&live)));
    assert!(
        hung.contains("⚠ update has been running for 5h12m"),
        "{hung}"
    );
    assert!(
        hung.contains(&format!(
            "pid {pid}\n    → if it is hung, stop it (kill {pid}) and rerun dot update",
            pid = std::process::id()
        )),
        "{hung}"
    );
    assert!(!hung.contains('✗'), "{hung}");
    drop(guard);
}

#[test]
fn reexec_checkpoint_rows_follow_what_update_does() {
    use dot::shdeps::CheckpointState;

    let path = Path::new("/home/u/.local/state/dot/provider-reexec-failed");
    let rows = |state: CheckpointState| render(&check_reexec_checkpoint(&state, path, "/home/u"));
    assert_eq!(rows(CheckpointState::Absent), "");
    let pending = rows(CheckpointState::Pending);
    assert!(
        pending.contains("⚠ provider re-exec checkpoint pending"),
        "{pending}"
    );
    assert!(pending.contains("~/.local/state/dot/provider-reexec-failed"));
    let unreadable = rows(CheckpointState::Unreadable);
    assert!(
        unreadable.contains("✗ provider re-exec checkpoint blocks dot update"),
        "{unreadable}"
    );
    assert!(unreadable.contains("unsafe or malformed"));
    let mismatch = rows(CheckpointState::Mismatch {
        pinned: "a".repeat(40),
        active: String::new(),
    });
    assert!(
        mismatch.contains("✗ provider re-exec checkpoint blocks dot update"),
        "{mismatch}"
    );
    assert!(
        mismatch.contains("pins aaaaaaaaaaaa but dot is at <unavailable>"),
        "{mismatch}"
    );
}

/// A fake standalone install under `data/cgraf78`: the versioned release,
/// the `current` link, the control directory, and the stable root link,
/// exactly as `install.sh` publishes them.
fn standalone_install(data: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let cgraf = data.join("cgraf78");
    let release = cgraf.join(".dot-standalone/releases/v1-linux");
    std::fs::create_dir_all(release.join("lib/dot/public")).expect("release");
    std::fs::write(release.join(".dot-install.json"), b"{}\n").expect("metadata");
    std::os::unix::fs::symlink("releases/v1-linux", cgraf.join(".dot-standalone/current"))
        .expect("current");
    std::os::unix::fs::symlink(".dot-standalone/current", cgraf.join("dot")).expect("root");
    (
        cgraf.join("dot"),
        std::fs::canonicalize(&release).expect("real"),
    )
}

/// A fake Shdeps archive install at `data/cgraf78/dot`.
fn shdeps_install(data: &Path, marker: Option<&[u8]>) -> std::path::PathBuf {
    let root = data.join("cgraf78/dot");
    std::fs::create_dir_all(root.join("lib/dot/public")).expect("release");
    std::fs::write(root.join(".dot-install.json"), b"{}\n").expect("metadata");
    if let Some(marker) = marker {
        std::fs::write(root.join(".shdeps-release-layout"), marker).expect("marker");
    }
    root
}

/// The layout verdict as text: a `kind: ...` line for a healthy layout's
/// kind (which files no row; the runtime's version row names it), then
/// every problem row.
fn layout(root: &Path, source: &Path, release_root: bool, shdeps: bool) -> String {
    let layout = check_install_layout(&InstallInputs {
        home: "/nonexistent-home",
        source_real: source,
        release_root,
        managed_root: root,
        shdeps,
    });
    let kind = layout
        .kind
        .map(|kind| format!("kind: {kind}\n"))
        .unwrap_or_default();
    format!("{kind}{}", render(&layout.records))
}

#[test]
fn standalone_install_under_shdeps_warns_until_shdeps_adopts_it() {
    // Shdeps adopts the installer's layout on its next update of Dot, so the
    // standalone root alone only warns.
    let scratch = TempDir::new("doctor-layout-standalone").expect("scratch");
    let (root, release) = standalone_install(scratch.path());
    let running = layout(&root, &release, true, true);
    assert!(
        running.contains("⚠ dot is standalone-installed"),
        "{running}"
    );
    assert!(
        running.contains(
            "Shdeps adopts it on its next update of dot; if this persists, run shdeps health"
        ),
        "{running}"
    );
    assert!(!running.contains('✗'), "{running}");
    // Same verdict when a checkout runs Dot but the managed root is still
    // the standalone link Shdeps would have to adopt.
    let checkout = scratch.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    let from_checkout = layout(&root, &checkout, false, true);
    assert!(
        from_checkout.contains("⚠ dot is standalone-installed"),
        "{from_checkout}"
    );
    // A standalone binary running beside a healthy Shdeps install (a test
    // harness, say) is not the stuck shape: the managed root decides.
    let other = scratch.path().join("other");
    let managed = shdeps_install(&other, Some(b"v1 archive\n"));
    assert_eq!(layout(&managed, &release, true, true), "");
    // Without a provider the standalone installer is the upgrade path,
    // whether the managed root or the running release identifies it.
    let alone = layout(&root, &release, true, false);
    assert!(
        alone.contains("kind: standalone install; rerun install.sh to upgrade"),
        "{alone}"
    );
    assert!(!alone.contains('✗'), "{alone}");
    let running = layout(&scratch.path().join("absent/dot"), &release, true, false);
    assert!(
        running.contains("kind: standalone install; rerun install.sh to upgrade"),
        "{running}"
    );
}

#[test]
fn standalone_installer_lock_warns_because_install_sh_refuses() {
    let scratch = TempDir::new("doctor-layout-standalone-lock").expect("scratch");
    let (root, release) = standalone_install(scratch.path());
    std::fs::create_dir(scratch.path().join("cgraf78/.dot-standalone/lock")).expect("lock");
    let rows = layout(&root, &release, true, false);
    // A warning: it blocks only a manual installer rerun, never `dot update`.
    assert!(
        rows.contains("⚠ standalone installer lock is present"),
        "{rows}"
    );
    assert!(!rows.contains('✗'), "{rows}");
    assert!(rows.contains("install.sh refuses to run while it exists"));
}

#[test]
fn standalone_installer_lock_fails_under_shdeps_because_adoption_refuses() {
    let scratch = TempDir::new("doctor-layout-standalone-lock-shdeps").expect("scratch");
    let (root, release) = standalone_install(scratch.path());
    std::fs::create_dir(scratch.path().join("cgraf78/.dot-standalone/lock")).expect("lock");
    let rows = layout(&root, &release, true, true);
    assert!(rows.contains("⚠ dot is standalone-installed"), "{rows}");
    assert!(
        rows.contains("✗ standalone installer lock blocks Shdeps adoption"),
        "{rows}"
    );
    assert!(
        rows.contains("cgraf78/.dot-standalone/lock: Shdeps will not adopt"),
        "{rows}"
    );
    assert!(
        rows.contains("remove it if no install.sh is running"),
        "{rows}"
    );
}

#[test]
fn interrupted_adoption_warns_and_its_lock_blocks_the_resume() {
    // Shdeps' fallback switch parks the installer's root link as
    // `<root>.shdeps-parked-root`; an interrupted switch leaves no root.
    let scratch = TempDir::new("doctor-layout-parked").expect("scratch");
    let (root, release) = standalone_install(scratch.path());
    let parked = scratch.path().join("cgraf78/dot.shdeps-parked-root");
    std::fs::rename(&root, &parked).expect("park root link");
    let rows = layout(&root, &release, true, true);
    assert!(
        rows.contains("⚠ Shdeps adoption of the standalone install was interrupted"),
        "{rows}"
    );
    assert!(
        rows.contains("the next Shdeps update of dot finishes it"),
        "{rows}"
    );
    assert!(!rows.contains('✗'), "{rows}");
    // Shdeps refuses to resume while the installer lock exists: never call
    // that lock unused.
    std::fs::create_dir(scratch.path().join("cgraf78/.dot-standalone/lock")).expect("lock");
    let locked = layout(&root, &release, true, true);
    assert!(
        locked.contains("✗ standalone installer lock blocks Shdeps adoption"),
        "{locked}"
    );
    assert!(!locked.contains("no longer used"), "{locked}");
    // A parked link that is not the installer's is not an adoption.
    std::fs::remove_file(&parked).expect("remove parked link");
    std::os::unix::fs::symlink("elsewhere", &parked).expect("foreign parked link");
    assert!(!layout(&root, &release, true, true).contains("interrupted"));
}

#[test]
fn only_the_installers_exact_root_link_counts_as_adoptable() {
    // Shdeps adopts only `<root> -> .dot-standalone/current`, read without
    // following links; anything else would be a promise it does not keep.
    let scratch = TempDir::new("doctor-layout-exact-link").expect("scratch");
    let (root, release) = standalone_install(scratch.path());
    std::fs::remove_file(&root).expect("remove root link");
    std::os::unix::fs::symlink(".dot-standalone/releases/v1-linux", &root)
        .expect("link into releases");
    assert!(!layout(&root, &release, true, true).contains("standalone"));
    std::fs::remove_file(&root).expect("remove root link");
    std::os::unix::fs::symlink(
        scratch.path().join("cgraf78/.dot-standalone/current"),
        &root,
    )
    .expect("absolute link");
    assert!(!layout(&root, &release, true, true).contains("standalone"));
    // The lock row needs only the root link: Shdeps checks the lock before
    // it reads `current`, so a dangling `current` still reports the lock.
    std::fs::remove_file(&root).expect("remove root link");
    std::os::unix::fs::symlink(".dot-standalone/current", &root).expect("installer link");
    std::fs::remove_file(scratch.path().join("cgraf78/.dot-standalone/current"))
        .expect("remove current");
    std::fs::create_dir(scratch.path().join("cgraf78/.dot-standalone/lock")).expect("lock");
    let rows = layout(&root, &release, true, true);
    assert!(
        rows.contains("✗ standalone installer lock blocks Shdeps adoption"),
        "{rows}"
    );
}

#[test]
fn standalone_link_outside_the_releases_directory_is_not_the_installer_layout() {
    let scratch = TempDir::new("doctor-layout-not-releases").expect("scratch");
    let cgraf = scratch.path().join("cgraf78");
    let other = cgraf.join(".dot-standalone/other/v1-linux");
    std::fs::create_dir_all(other.join("lib/dot/public")).expect("other");
    std::fs::write(other.join(".dot-install.json"), b"{}\n").expect("metadata");
    std::os::unix::fs::symlink(".dot-standalone/other/v1-linux", cgraf.join("dot"))
        .expect("root link");
    let real = std::fs::canonicalize(&other).expect("real");
    for shdeps in [true, false] {
        let rows = layout(&cgraf.join("dot"), &real, true, shdeps);
        assert!(!rows.contains("standalone"), "{rows}");
    }
}

#[test]
fn shdeps_release_requires_its_layout_marker() {
    let scratch = TempDir::new("doctor-layout-shdeps").expect("scratch");
    let root = shdeps_install(scratch.path(), Some(b"v1 archive\n"));
    let real = std::fs::canonicalize(&root).expect("real");
    let healthy = layout(&root, &real, true, true);
    assert_eq!(healthy, "kind: Shdeps release\n");

    // Shdeps compares the whole file: any other content refuses.
    for content in [
        b"v2 archive\n".as_slice(),
        b"v1 archive\nextra\n",
        b"v1 archive",
        b"v1 archive\n\n",
    ] {
        std::fs::write(root.join(".shdeps-release-layout"), content).expect("marker");
        let wrong = layout(&root, &real, true, true);
        assert!(
            wrong.contains("✗ dot release layout marker is invalid"),
            "{content:?}: {wrong}"
        );
    }

    std::fs::remove_file(root.join(".shdeps-release-layout")).expect("rm marker");
    std::fs::create_dir(root.join(".shdeps-release-layout")).expect("marker dir");
    let directory = layout(&root, &real, true, true);
    assert!(
        directory.contains("✗ dot release layout marker is invalid"),
        "{directory}"
    );

    std::fs::remove_dir(root.join(".shdeps-release-layout")).expect("rm marker dir");
    let missing = layout(&root, &real, true, true);
    assert!(
        missing.contains("⚠ dot release has no Shdeps layout marker"),
        "{missing}"
    );
    assert!(!missing.contains('✗'), "{missing}");
}

#[test]
fn shdeps_release_reports_leftover_install_state() {
    let scratch = TempDir::new("doctor-layout-leftovers").expect("scratch");
    let root = shdeps_install(scratch.path(), Some(b"v1 archive\n"));
    let real = std::fs::canonicalize(&root).expect("real");
    let cgraf = scratch.path().join("cgraf78");
    std::fs::create_dir(cgraf.join("dot.shdeps-archive-backup-42-7")).expect("backup");
    std::fs::create_dir(cgraf.join(".dot.shdeps-archive-backup-43-8")).expect("hidden backup");
    // Another dependency's backup is not Dot's.
    std::fs::create_dir(cgraf.join("dots.shdeps-archive-backup-1-1")).expect("other backup");
    std::fs::create_dir_all(cgraf.join(".dot-standalone/lock")).expect("old lock");
    let rows = layout(&root, &real, true, true);
    assert!(rows.starts_with("kind: Shdeps release\n"), "{rows}");
    assert!(
        rows.contains("⚠ interrupted Shdeps install left a backup"),
        "{rows}"
    );
    // Each backup is its own item line, and the next step is a hint.
    for backup in [
        "/dot.shdeps-archive-backup-42-7\n",
        "/.dot.shdeps-archive-backup-43-8\n",
    ] {
        assert!(
            rows.lines()
                .any(|line| line.starts_with("    - ") && format!("{line}\n").ends_with(backup)),
            "{backup}: {rows}"
        );
    }
    assert!(
        rows.contains("    → remove it once dot runs from the current release\n"),
        "{rows}"
    );
    assert!(!rows.contains("dots.shdeps"), "{rows}");
    assert!(
        rows.contains("⚠ leftover standalone installer lock"),
        "{rows}"
    );
    assert!(!rows.contains('✗'), "{rows}");
}

#[test]
fn backup_is_reported_even_when_the_install_root_is_gone() {
    // A swap whose rollback also failed leaves only the backup behind.
    let scratch = TempDir::new("doctor-layout-orphan-backup").expect("scratch");
    let cgraf = scratch.path().join("cgraf78");
    std::fs::create_dir_all(cgraf.join("dot.shdeps-archive-backup-9-9")).expect("backup");
    let checkout = scratch.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    let rows = layout(&cgraf.join("dot"), &checkout, false, true);
    assert!(
        rows.contains("⚠ interrupted Shdeps install left a backup"),
        "{rows}"
    );
    assert!(
        rows.contains("the install root is missing; run dot update to reinstall it"),
        "{rows}"
    );
}

#[test]
fn install_layout_is_silent_for_checkouts_and_other_providers() {
    let scratch = TempDir::new("doctor-layout-silent").expect("scratch");
    let root = shdeps_install(scratch.path(), None);
    let real = std::fs::canonicalize(&root).expect("real");
    // No provider: a directory release is someone else's to upgrade.
    assert_eq!(layout(&root, &real, true, false), "");
    // A checkout runs Dot: the release directory is not in use.
    let checkout = scratch.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    assert_eq!(layout(&root, &checkout, false, true), "");
    // No managed root at all.
    let missing = scratch.path().join("missing/cgraf78/dot");
    assert_eq!(layout(&missing, &checkout, false, true), "");
}

fn backdate(path: &Path, secs_ago: u64) {
    let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for backdating")
        .set_modified(mtime)
        .expect("backdate mtime");
}

fn merge_spec(identity: &str, script: &Path, outputs: Vec<String>) -> MergeSpec {
    MergeSpec {
        identity: identity.to_string(),
        script: script.to_string_lossy().into_owned(),
        outputs,
        invalid: vec![],
    }
}

#[test]
fn merge_outputs_verify_existence() {
    let scratch = TempDir::new("doctor-merge-outputs").expect("scratch");
    let ext = scratch.path().join("extensions");
    std::fs::create_dir_all(ext.join("merge-hooks.d")).expect("hooks");
    let script = ext.join("merge-hooks.d/10-fixture.sh");
    std::fs::write(&script, b"merge() { :; }\n").expect("hook");
    let check = |specs: Vec<MergeSpec>| {
        render(&check_merges(&MergeInputs {
            enabled: true,
            extensions_dir: ext.to_string_lossy().into_owned(),
            spec_count: Some(1),
            inventory_error: None,
            specs,
        }))
    };

    // An existing output passes.
    let live = scratch.path().join("live.conf");
    std::fs::write(&live, b"live\n").expect("live output");
    let fresh = check(vec![merge_spec(
        "fixture",
        &script,
        vec![live.to_string_lossy().into_owned()],
    )]);
    assert!(fresh.contains("1 hook(s)"));
    assert!(fresh.contains("✓ merge-hook outputs exist (1 output(s) across 1 hook(s))"));
    assert!(!fresh.contains('✗'));

    // N2: an output older than its hook is still current. Hooks write
    // through `dot_write_text_if_changed`, which leaves an unchanged output
    // untouched, so after an edit to the hook that does not change what it
    // writes, the output is older than the hook and correct.
    backdate(&live, 3600);
    let older = check(vec![merge_spec(
        "fixture",
        &script,
        vec![live.to_string_lossy().into_owned()],
    )]);
    assert!(older.contains("✓ merge-hook outputs exist"), "{older}");
    assert!(!older.contains('✗'), "{older}");

    // A missing output fails, naming the hook and the path, with a next step.
    let absent = scratch.path().join("absent.conf");
    let missing = check(vec![merge_spec(
        "fixture",
        &script,
        vec![absent.to_string_lossy().into_owned()],
    )]);
    assert!(
        missing.contains(&format!(
            "✗ merge-hook output is missing\n    fixture: {}\n    → run dot update",
            absent.display()
        )),
        "{missing}"
    );

    // No declared outputs files nothing past the hook count: a permanent
    // "unverified" row could never be acted on.
    let undeclared = check(vec![merge_spec("fixture", &script, vec![])]);
    assert_eq!(
        undeclared, "\nExtensions\n  ✓ merge-hook extensions (1 hook(s))\n",
        "{undeclared}"
    );

    // Healthy hooks collapse into one summary row; problems keep a row each.
    let mut many: Vec<MergeSpec> = (0..30)
        .map(|index| merge_spec(&format!("bare{index}"), &script, vec![]))
        .collect();
    for name in ["live-a", "live-b"] {
        many.push(merge_spec(
            name,
            &script,
            vec![live.to_string_lossy().into_owned()],
        ));
    }
    many.push(merge_spec(
        "gone",
        &script,
        vec![absent.to_string_lossy().into_owned()],
    ));
    let collapsed = check(many);
    assert!(!collapsed.contains("unverified"), "{collapsed}");
    assert!(collapsed.contains("✓ merge-hook outputs exist (2 output(s) across 2 hook(s))"));
    assert!(collapsed.contains("✗ merge-hook output is missing\n    gone: "));
    assert_eq!(collapsed.lines().count(), 7, "{collapsed}");

    // A relative declaration fails outright.
    let mut bad = merge_spec("fixture", &script, vec![]);
    bad.invalid = vec!["relative/path.conf".to_string()];
    let invalid = check(vec![bad]);
    assert!(invalid.contains("✗ merge-hook output declaration is invalid"));
}

#[test]
fn lifecycle_branch_matrix() {
    let check = |present, load, eligible, records, extensions| {
        check_profile_lifecycle(&LifecycleInputs {
            profiles_present: present,
            load_ok: load,
            eligible,
            active: vec![],
            records,
            extensions_enabled: extensions,
            deactivation_ok: &|_| true,
        })
    };
    assert!(check(false, true, vec![], vec![], true).is_empty());
    assert!(render(&check(true, false, vec![], vec![], true)).contains("state unsafe"));
    assert!(render(&check(true, true, vec![], vec![], true)).contains("no pending deactivations"));
    let pending = render(&check(true, true, vec![], vec!["old|record".into()], false));
    assert!(pending.contains("extensions are disabled"));
    assert!(pending.contains("profile deactivation pending"));
    assert_eq!(
        hint_of(&pending, "extensions are disabled"),
        Some("set extension_api=1 and extensions_dir in dot's config, then run dot update"),
        "{pending}"
    );
}

#[test]
fn lifecycle_active_retained_and_retiring_authority_matrix() {
    let bad = |record: &str| !record.contains("bad");
    let records = check_profile_lifecycle(&LifecycleInputs {
        profiles_present: true,
        load_ok: true,
        eligible: vec!["active".into(), "retained".into()],
        active: vec!["active|bad".into()],
        records: vec![
            "active|ledger".into(),
            "retained|bad".into(),
            "retiring|bad".into(),
        ],
        extensions_enabled: true,
        deactivation_ok: &bad,
    });
    let output = render(&records);
    assert!(output.contains("active profile deactivation authority unsafe"));
    assert!(output.contains("retained profile deactivation authority unavailable"));
    assert!(output.contains("retiring overlay authority unsafe"));
    assert!(output.contains("profile deactivation pending"));
}

fn overlays<'a>(
    manifest: String,
    config_error: Option<&'a str>,
    present: bool,
) -> OverlayInputs<'a> {
    OverlayInputs {
        home: "/home/test",
        profile_config_error: config_error,
        profiles_present: present,
        profile_user: None,
        profile_host: None,
        selected_profile: None,
        selection_state: None,
        included_profiles: vec![],
        phase_one: vec![],
        selectors: vec![],
        unknown_keys: vec![],
        lifecycle: LifecycleInputs {
            profiles_present: present,
            load_ok: true,
            eligible: vec![],
            active: vec![],
            records: vec![],
            extensions_enabled: false,
            deactivation_ok: &|_| true,
        },
        configured_count: 0,
        manifest,
        discovery_error: None,
        active_records: vec![],
        overlay_lifecycle: vec![],
        local_validate: &|_| Ok(()),
    }
}

#[test]
fn overlays_report_config_legacy_and_empty_inventory() {
    let scratch = TempDir::new("doctor-overlays-native").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let invalid = render(&check_overlays(&overlays(
        manifest.clone(),
        Some("bad profile"),
        true,
    )));
    assert!(invalid.contains("profile configuration invalid"));
    assert!(invalid.contains("bad profile"));
    let legacy = render(&check_overlays(&overlays(manifest.clone(), None, false)));
    assert!(legacy.contains("profile selection disabled"));
    assert!(legacy.contains("no overlays to check"));
    let profiles = render(&check_overlays(&overlays(manifest, None, true)));
    assert!(profiles.contains("no pending deactivations"));
    assert!(profiles.contains("no overlays to check"));
}

#[test]
fn profile_selection_is_one_informational_row() {
    // N1: identity, selection, included profiles, phase-one overlays, and
    // matching selectors used to take five rows that never change.
    let scratch = TempDir::new("doctor-profile-row").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let mut input = overlays(manifest, None, true);
    input.profile_user = Some("alice");
    input.profile_host = Some("box");
    input.selected_profile = Some("dev");
    input.selection_state = Some("agreed-match");
    input.included_profiles = vec!["base".into(), "dev".into()];
    input.phase_one = vec!["personal".into()];
    input.selectors = vec![
        "root|/home/test/.config/dot/profile-selectors.d/00-default.conf|||dev|true".into(),
        "local|/home/test/.config/dot/profile-selectors.local.d/10-other.conf|bob||web|false"
            .into(),
    ];
    let rendered = render(&check_overlays(&input));
    assert!(
        rendered.starts_with(
            "\nProfiles\n  › profile dev (agreed-match; includes base dev; phase-one overlays personal; root selector 00-default.conf -> dev; for alice@box)\n  ✓ profile lifecycle state"
        ),
        "{rendered}"
    );
    assert_eq!(rendered.matches('›').count(), 1, "{rendered}");
}

#[test]
fn overlays_discovery_lifecycle_local_and_sync_matrix() {
    let scratch = TempDir::new("doctor-overlays-matrix").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let local = scratch.path().join("local").to_string_lossy().into_owned();
    let missing = scratch
        .path()
        .join("missing-git")
        .to_string_lossy()
        .into_owned();
    let mut input = overlays(manifest, None, true);
    input.configured_count = 7;
    input.discovery_error = Some("bad descriptor");
    input.overlay_lifecycle = vec![
        "off|not-selected|d".into(),
        "wrong-host|selected-ineligible|d".into(),
        "optional-gone|selected-optional-unavailable|d".into(),
        "required-gone|selected-unavailable|d".into(),
        "unknown|future-state|d".into(),
        "orphan|active|d".into(),
        "local|active|d".into(),
        "required|active|d".into(),
        "optional|active|d".into(),
    ];
    input.active_records = vec![
        format!("local|{local}|||false|none"),
        format!("required|{missing}|||false|git"),
        format!("optional|{missing}|||true|git"),
    ];
    input.local_validate = &|path| {
        if path.ends_with("local") {
            Err("local diagnostic".into())
        } else {
            Ok(())
        }
    };
    let output = render(&check_overlays(&input));
    for expected in [
        "overlay descriptor invalid",
        "off: not selected",
        "host/platform ineligible",
        "selected optional but unavailable",
        "selected but unavailable",
        "unknown overlay lifecycle state",
        "active lifecycle record missing",
        "local: local source unavailable",
        "required: not cloned",
        "optional overlay not cloned",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?} in {output}"
        );
    }
    assert_eq!(
        hint_of(&output, "required: not cloned"),
        Some("run dot update to clone it"),
        "{output}"
    );
}

#[test]
fn overlays_report_keys_from_a_newer_dot_as_warnings() {
    // Doctor shows every key a newer Dot introduced, and what it cost,
    // as a warning row instead of a stderr line.
    use dot::unknown_keys::{DataKey, Effect};
    let scratch = TempDir::new("doctor-overlays-newer").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let key = |path: &str, line: usize, effect: Effect| DataKey {
        path: path.to_string(),
        line,
        key: "future_key".to_string(),
        effect,
    };
    let mut input = overlays(manifest.clone(), None, true);
    input.configured_count = 1;
    input.unknown_keys = vec![
        key(
            "/home/test/.config/dot/profiles.d/base.conf",
            3,
            Effect::Ignored,
        ),
        key(
            "/home/test/.config/dot/profile-selectors.d/10-host.conf",
            4,
            Effect::SelectorSkipped,
        ),
        key(
            "/home/test/.config/dot/profile-selectors.d/20-shared.conf",
            2,
            Effect::SelectorFallback,
        ),
        // The descriptor names its overlay the legacy way (`beta.local`);
        // the lifecycle record, matched by file, says `beta`.
        key(
            "/home/test/.config/dot/overlays.d/20-beta.local.conf",
            2,
            Effect::OverlaySkipped("beta.local".to_string()),
        ),
        key(
            "/home/test/.config/dot/overlays.d/10-personal.conf",
            5,
            Effect::SelectorsUnread("personal".to_string()),
        ),
    ];
    input.overlay_lifecycle = vec![
        "beta|selected-unsupported|/home/test/.config/dot/overlays.d/20-beta.local.conf".into(),
    ];
    let records = check_overlays(&input);
    let output = render(&records);
    for expected in [
        "unknown profile key ignored\n    ~/.config/dot/profiles.d/base.conf:3 future_key (newer dot?)",
        "selector skipped: unknown key\n    ~/.config/dot/profile-selectors.d/10-host.conf:4 future_key (newer dot?)",
        "selector skipped: unknown key; profile base selected\n    ~/.config/dot/profile-selectors.d/20-shared.conf:2 future_key (newer dot?)",
        // The keys are items, one per line.
        "beta: selected but skipped: unknown descriptor key\n    - ~/.config/dot/overlays.d/20-beta.local.conf:2 future_key (newer dot?)",
        "personal selectors unread: overlay skipped; profile base selected\n    ~/.config/dot/overlays.d/10-personal.conf:5 future_key (newer dot?)",
        // `dot update` holds the installed set, so the links are not judged
        // against this reading.
        "overlay set held: newer keys need a newer dot",
        "overlay symlinks not checked while the overlay set is held",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?} in {output}"
        );
    }
    assert_eq!(
        records
            .iter()
            .filter(|record| record.kind == dot::doctor_runtime::Kind::Warn)
            .count(),
        6,
        "{output}"
    );

    // Legacy discovery does not count a skipped `sync=none` descriptor as
    // configured; its row must still appear.
    let mut legacy = overlays(manifest, None, false);
    legacy.unknown_keys = vec![key(
        "/home/test/.config/dot/overlays.d/10-local.local.conf",
        3,
        Effect::OverlaySkipped("local".to_string()),
    )];
    legacy.overlay_lifecycle = vec![
        "local|selected-unsupported|/home/test/.config/dot/overlays.d/10-local.local.conf".into(),
    ];
    let output = render(&check_overlays(&legacy));
    assert!(!output.contains("no overlays to check"), "{output}");
    assert!(
        output.contains("local: selected but skipped: unknown descriptor key"),
        "{output}"
    );
}

#[test]
fn keys_that_cannot_change_the_overlay_set_do_not_report_a_hold() {
    use dot::unknown_keys::{DataKey, Effect};
    let scratch = TempDir::new("doctor-overlays-no-hold").expect("scratch");
    let mut input = overlays(
        scratch
            .path()
            .join("missing")
            .to_string_lossy()
            .into_owned(),
        None,
        true,
    );
    input.unknown_keys = [Effect::Ignored, Effect::SelectorSkipped]
        .into_iter()
        .map(|effect| DataKey {
            path: "/home/test/.config/dot/profiles.d/base.conf".to_string(),
            line: 2,
            key: "future_key".to_string(),
            effect,
        })
        .collect();
    let output = render(&check_overlays(&input));
    assert!(!output.contains("overlay set held"), "{output}");
}

#[test]
fn overlays_git_origin_and_manifest_health_matrix() {
    let scratch = TempDir::new("doctor-overlays-git").expect("scratch");
    let home = scratch.path().join("home");
    let repo = home.join(".dotfiles-git");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&repo).expect("overlay");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &["remote", "add", "origin", "https://actual.invalid/repo.git"],
    );
    let rel = ".config/demo";
    let source = repo.join("home").join(rel);
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("source parent");
    std::fs::write(&source, b"demo").expect("source");
    let destination = home.join(rel);
    std::fs::create_dir_all(destination.parent().expect("destination parent"))
        .expect("destination parent");
    let link_target =
        dot::repos_overlays::record_link_target(rel, "git", &repo.to_string_lossy(), Some("git"))
            .expect("git link target");
    std::os::unix::fs::symlink(&link_target, &destination).expect("link");
    let manifest = scratch.path().join("manifest");
    std::fs::write(&manifest, format!("{rel}\tgit\t{link_target}\n")).expect("manifest");
    let input = OverlayInputs {
        home: home.to_str().expect("home utf8"),
        profile_config_error: None,
        profiles_present: false,
        profile_user: None,
        profile_host: None,
        selected_profile: None,
        selection_state: None,
        included_profiles: vec![],
        phase_one: vec![],
        selectors: vec![],
        unknown_keys: vec![],
        lifecycle: LifecycleInputs {
            profiles_present: false,
            load_ok: true,
            eligible: vec![],
            active: vec![],
            records: vec![],
            extensions_enabled: false,
            deactivation_ok: &|_| true,
        },
        configured_count: 1,
        manifest: manifest.to_string_lossy().into_owned(),
        discovery_error: None,
        active_records: vec![format!(
            "git|{}|https://expected.invalid/repo.git||false|git",
            repo.display()
        )],
        overlay_lifecycle: vec!["git|active|d".into()],
        local_validate: &valid_local,
    };
    let drift = render(&check_overlays(&input));
    assert!(drift.contains("git: cloned"));
    // `dot update` refuses to pull or link a drifted overlay and exits 1,
    // so doctor fails on it rather than warning.
    assert!(drift.contains("✗ git: remote URL drift"), "{drift}");
    assert!(
        drift.contains("verify the checkout, then adopt it with: git -C"),
        "{drift}"
    );
    assert!(drift.contains("overlay symlinks healthy"));

    std::fs::write(&manifest, b"malformed\n").expect("bad manifest");
    let malformed = render(&check_overlays(&input));
    assert!(
        malformed.contains(&format!(
            "⚠ 1 overlay symlink issue(s)\n    - {} line 1: unreadable record\n    → run dot update to re-link\n",
            manifest.display()
        )),
        "{malformed}"
    );

    // K8: each issue names its link (under `~`) and why, as an item.
    let missing_rel = ".config/gone";
    let foreign_rel = ".config/foreign";
    std::fs::write(home.join(foreign_rel), b"a file").expect("foreign file");
    let dangling_rel = ".config/dangling";
    std::os::unix::fs::symlink(home.join("nowhere"), home.join(dangling_rel)).expect("dangling");
    let orphan_rel = ".config/orphan";
    std::os::unix::fs::symlink(&source, home.join(orphan_rel)).expect("orphan link");
    std::fs::write(
        &manifest,
        format!(
            "{rel}\tgit\t{link_target}\n{missing_rel}\tgit\tx\n{foreign_rel}\tgit\tx\n{dangling_rel}\tgit\tx\n{orphan_rel}\tretired\tx\n"
        ),
    )
    .expect("manifest with issues");
    let issues = render(&check_overlays(&input));
    for item in [
        "⚠ 4 overlay symlink issue(s)\n",
        "    - ~/.config/dangling (dangling)\n",
        "    - ~/.config/foreign (not a symlink)\n",
        "    - ~/.config/gone (missing)\n",
        "    - ~/.config/orphan (owner retired is not active)\n",
        "    → run dot update to re-link\n",
    ] {
        assert!(issues.contains(item), "missing {item:?}: {issues}");
    }
}

#[test]
fn status_v2_parses_branch_upstream_distance_and_entries() {
    let status = parse_status_v2(
        "# branch.oid 0123\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -3\n1 .M N... 100644 100644 100644 a b tracked\n2 R. N... 100644 100644 100644 a b R100 new\told\nu UU N... 100644 100644 100644 100644 a b c conflict\n# branch.future value\n",
    );
    assert_eq!(status.head.as_deref(), Some("main"));
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!(status.ahead_behind, Some((2, 3)));
    assert_eq!((status.changed, status.unmerged), (2, 1));
    let detached = parse_status_v2("# branch.oid 0123\n# branch.head (detached)\n");
    assert_eq!(detached.head, None);
    assert_eq!(detached.upstream, None);
    // An upstream whose ref is gone has no distance line.
    let gone = parse_status_v2("# branch.head main\n# branch.upstream origin/gone\n");
    assert_eq!(gone.ahead_behind, None);
    assert_eq!(parse_status_v2("# branch.ab +x -1\n").ahead_behind, None);
}

#[test]
fn overlay_branch_upstream_and_dirt_follow_update_severities() {
    // M7: overlays used to get only "cloned" and "URL matches"; the base
    // client also got branch, upstream, and tracked changes.
    let scratch = TempDir::new("doctor-overlay-state").expect("scratch");
    let remote = scratch.path().join("remote.git");
    std::fs::create_dir_all(&remote).expect("remote");
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let seed = scratch.path().join("seed");
    std::fs::create_dir_all(&seed).expect("seed");
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("file"), b"one\n").expect("file");
    git(&seed, &["add", "file"]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    let remote_text = remote.to_str().expect("remote utf8").to_string();
    git(&seed, &["push", "-q", &remote_text, "main"]);
    let repo = scratch.path().join("overlay");
    let status = dot_test_support::git()
        .args(["clone", "-q"])
        .arg(&remote)
        .arg(&repo)
        .stdin(Stdio::null())
        .status()
        .expect("clone overlay");
    assert!(status.success());
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let mut input = overlays(manifest, None, false);
    input.configured_count = 1;
    input.active_records = vec![format!("git|{}|{remote_text}||false|git", repo.display())];
    input.overlay_lifecycle = vec!["git|active|d".into()];
    let rows = || render(&check_overlays(&input));

    // N1: a clean overlay on its current upstream is one row.
    let clean = rows();
    assert!(
        clean.contains(&format!(
            "\n  ✓ git ({}, main, current with origin/main)\n",
            repo.display()
        )),
        "{clean}"
    );
    assert!(!clean.contains("remote.origin.url"), "{clean}");
    assert!(!clean.contains('⚠') && !clean.contains('✗'), "{clean}");

    std::fs::write(repo.join("file"), b"two\n").expect("dirty");
    // Any problem expands the overlay to every row, passing ones included.
    let dirty = rows();
    assert!(dirty.contains("⚠ git: 1 tracked change(s)"), "{dirty}");
    for row in [
        "✓ git: cloned",
        "✓ git: remote.origin.url matches conf",
        "✓ git: upstream (origin/main (current))",
    ] {
        assert!(dirty.contains(row), "missing {row:?}: {dirty}");
    }
    git(&repo, &["checkout", "-q", "--", "file"]);

    git(&repo, &["commit", "-q", "--allow-empty", "-m", "local"]);
    let ahead = rows();
    assert!(ahead.contains("⚠ git: ahead of upstream"), "{ahead}");
    assert!(ahead.contains("origin/main: 1 commit(s) ahead"), "{ahead}");

    git(&repo, &["checkout", "-q", "--detach"]);
    let detached = rows();
    assert!(detached.contains("⚠ git: HEAD is detached"), "{detached}");
    assert!(
        detached.contains("dot update skips this overlay"),
        "{detached}"
    );

    git(&repo, &["checkout", "-q", "-b", "side"]);
    let untracked = rows();
    assert!(
        untracked.contains(
            "⚠ git: upstream is not configured\n    dot update skips pulling this overlay\n"
        ),
        "{untracked}"
    );
    assert!(
        untracked.contains(&format!(
            "    → set one: `git -C {} branch --set-upstream-to=origin/side`\n",
            repo.display()
        )),
        "{untracked}"
    );

    // A conflicted merge leaves unmerged entries: every pull refuses.
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["reset", "-q", "--hard", "origin/main"]);
    git(&repo, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.join("file"), b"theirs\n").expect("theirs");
    git(&repo, &["commit", "-q", "-am", "theirs"]);
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("file"), b"ours\n").expect("ours");
    git(&repo, &["commit", "-q", "-am", "ours"]);
    let merge = dot_test_support::git()
        .arg("-C")
        .arg(&repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "theirs",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("conflicting merge");
    assert!(!merge.success(), "the merge must conflict");
    // Mid-merge, update warns and skips the overlay, so doctor warns too.
    let merging = rows();
    assert!(
        merging.contains("⚠ git: 1 unmerged path(s)\n    a merge or rebase is in progress"),
        "{merging}"
    );
    assert!(merging.contains("⚠ git: 1 tracked change(s)"), "{merging}");
    assert!(!merging.contains('✗'), "{merging}");
    // Unmerged entries outside any session make every pull fail.
    for leftover in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
        let _ = std::fs::remove_file(repo.join(".git").join(leftover));
    }
    let conflicted = rows();
    assert!(
        conflicted.contains(
            "✗ git: 1 unmerged path(s)\n    dot update fails until the conflict is resolved"
        ),
        "{conflicted}"
    );
    // Without a branch to pull, update skips the overlay before it looks at
    // the index, so the same sessionless entries only warn.
    let head = String::from_utf8(
        dot_test_support::git()
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("HEAD")
            .stdout,
    )
    .expect("utf8 HEAD");
    git(&repo, &["update-ref", "--no-deref", "HEAD", head.trim()]);
    let detached = rows();
    assert!(
        detached.contains(
            "⚠ git: 1 unmerged path(s)\n    resolve them; without a branch and upstream to pull"
        ),
        "{detached}"
    );
    assert!(detached.contains("⚠ git: HEAD is detached"), "{detached}");
    assert!(!detached.contains('✗'), "{detached}");
}

#[test]
fn frozen_overlay_rebase_fails_until_rebased_by_hand() {
    // `dot update` refuses to retry a rebase that conflicted while HEAD and
    // the upstream tip stay put; doctor matches the recorded HEAD.
    let scratch = TempDir::new("doctor-overlay-frozen").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 1);
    let repo = &clones[0];
    let head = String::from_utf8(
        dot_test_support::git()
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("HEAD")
            .stdout,
    )
    .expect("utf8 HEAD");
    let head = head.trim();
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let input = clone_inputs(manifest, &remote, &clones);
    let marker = repo.join(".git/dot-rebase-failed");
    std::fs::write(&marker, format!("{head} {head}\n")).expect("frozen marker");
    let frozen = render(&check_overlays(&input));
    assert!(
        frozen.contains("✗ ov0: the last rebase onto origin/main conflicted; rebase manually"),
        "{frozen}"
    );
    // One strike still allows a retry, and a moved HEAD is a new attempt.
    std::fs::write(&marker, format!("{head} {head} retry\n")).expect("one strike");
    assert!(!render(&check_overlays(&input)).contains("conflicted"));
    std::fs::write(&marker, format!("{} {head}\n", "0".repeat(40))).expect("old head");
    assert!(!render(&check_overlays(&input)).contains("conflicted"));
    // Update skips a detached HEAD before it consults the marker.
    std::fs::write(&marker, format!("{head} {head}\n")).expect("frozen marker");
    git(repo, &["checkout", "-q", "--detach"]);
    assert!(!render(&check_overlays(&input)).contains("conflicted"));
    git(repo, &["checkout", "-q", "main"]);
    // An optional overlay's frozen pull only leaves it empty: a warning.
    let mut optional = input;
    optional.active_records = vec![format!("ov0|{}|{remote}||true|git", repo.display())];
    let rows = render(&check_overlays(&optional));
    assert!(
        rows.contains("⚠ ov0: the last rebase onto origin/main conflicted; rebase manually"),
        "{rows}"
    );
    assert!(rows.contains("skips this optional overlay"), "{rows}");
}

/// `git` in `repo`, stdout trimmed.
fn git_out(repo: &Path, args: &[&str]) -> String {
    let output = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {}",
        repo.display()
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

/// Leave `repo` mid-rebase onto `origin/main` with HEAD detached, the way a
/// killed `dot update` leaves it: stopped after its first pick, or, with
/// `conflict`, stopped on a conflicting pick (unmerged entries). With `dots`,
/// the rebase is recorded as dot's own first, exactly as update records it
/// before running it.
fn strand_rebase(repo: &Path, remote: &str, dots: bool, conflict: bool) {
    let peer = repo.with_extension("peer");
    let status = dot_test_support::git()
        .args(["clone", "-q", remote])
        .arg(&peer)
        .stdin(Stdio::null())
        .status()
        .expect("clone peer");
    assert!(status.success());
    let (upstream_file, local_file) = if conflict {
        ("file", "file")
    } else {
        ("upstream", "local")
    };
    std::fs::write(peer.join(upstream_file), b"up\n").expect("upstream file");
    git(&peer, &["add", upstream_file]);
    git(&peer, &["commit", "-q", "-m", "upstream"]);
    git(&peer, &["push", "-q", "origin", "main"]);
    std::fs::write(repo.join(local_file), b"local\n").expect("local file");
    git(repo, &["add", local_file]);
    git(repo, &["commit", "-q", "-m", "local"]);
    git(repo, &["fetch", "-q", "origin"]);
    if dots {
        let head = git_out(repo, &["rev-parse", "HEAD"]);
        let onto = git_out(repo, &["rev-parse", "origin/main"]);
        std::fs::write(
            repo.join(".git/dot-rebase-inflight"),
            format!("{head} {onto}\n"),
        )
        .expect("inflight record");
    }
    let mut rebase = vec!["rebase"];
    if !conflict {
        rebase.extend(["--exec", "false"]);
    }
    rebase.push("origin/main");
    let stopped = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(&rebase)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("rebase");
    assert!(!stopped.success(), "the rebase must stop");
    assert!(repo.join(".git/rebase-merge").is_dir());
}

#[test]
fn interrupted_rebase_reads_like_update_not_as_detached() {
    // K4: a rebase detaches HEAD. Doctor used to call every such checkout a
    // plain detached HEAD, even dot's own interrupted rebase that fails
    // every update. It now classifies it the way update does.
    let scratch = TempDir::new("doctor-overlay-interrupted").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 1);
    let repo = &clones[0];
    strand_rebase(repo, &remote, true, false);
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let input = clone_inputs(manifest, &remote, &clones);
    let rows = || render(&check_overlays(&input));

    // Nothing uncommitted: the next update aborts it and pulls.
    let healable = rows();
    assert!(
        healable.contains(
            "⚠ ov0: has an interrupted dot rebase\n    the next dot update aborts it, which discards nothing, and pulls\n    → run dot update to finish now\n"
        ),
        "{healable}"
    );
    assert!(!healable.contains("detached"), "{healable}");
    assert!(!healable.contains('✗'), "{healable}");

    // A tracked edit: update refuses to abort it and fails every run, so
    // doctor fails with the abort and continue commands for this checkout.
    std::fs::write(repo.join("file"), b"edited\n").expect("tracked edit");
    let stuck = rows();
    assert!(
        stuck.contains(
            "✗ ov0: has an interrupted dot rebase\n    dot update fails until it is aborted"
        ),
        "{stuck}"
    );
    assert!(
        stuck.contains(&format!(
            "    → run `git -C {repo} rebase --abort`, or resolve it and run `git -C {repo} rebase --continue`\n",
            repo = repo.display()
        )),
        "{stuck}"
    );
    git(repo, &["checkout", "-q", "--", "file"]);

    // Not dot's (no in-flight record): the user's session, which update
    // skips.
    std::fs::remove_file(repo.join(".git/dot-rebase-inflight")).expect("drop record");
    let session = rows();
    assert!(
        session.contains(
            "⚠ ov0: has an unfinished merge, rebase, cherry-pick, revert, or am\n    dot update skips this overlay until it is finished\n"
        ),
        "{session}"
    );
    assert!(!session.contains("detached"), "{session}");

    // Nothing in progress: a plain detached HEAD.
    git(repo, &["rebase", "--abort"]);
    git(repo, &["checkout", "-q", "--detach"]);
    let detached = rows();
    assert!(
        detached.contains(&format!(
            "⚠ ov0: HEAD is detached\n    dot update skips this overlay until it is back on a branch\n    → check out its branch: `git -C {} switch BRANCH`\n",
            repo.display()
        )),
        "{detached}"
    );
}

#[test]
fn interrupted_dot_rebase_fails_an_optional_overlay_too() {
    // Update fails a dot rebase it cannot abort even for an optional overlay:
    // its mid-rebase files are live and would be linked.
    let scratch = TempDir::new("doctor-optional-interrupted").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 1);
    let repo = &clones[0];
    strand_rebase(repo, &remote, true, false);
    std::fs::write(repo.join("file"), b"edited\n").expect("tracked edit");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let mut input = clone_inputs(manifest, &remote, &clones);
    input.active_records = vec![format!("ov0|{}|{remote}||true|git", repo.display())];
    let rows = render(&check_overlays(&input));
    assert!(
        rows.contains("✗ ov0: has an interrupted dot rebase"),
        "{rows}"
    );
}

#[test]
fn conflicted_rebase_reports_one_row_for_its_unmerged_entries() {
    // A rebase stopped on a conflict leaves unmerged entries. The row for
    // the rebase speaks for them, in update's severity; the generic
    // "without a branch to pull, update skips" unmerged row would contradict
    // it for dot's own rebase, which update fails.
    let scratch = TempDir::new("doctor-conflicted-rebase").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 1);
    let repo = &clones[0];
    strand_rebase(repo, &remote, true, true);
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let input = clone_inputs(manifest, &remote, &clones);
    let rows = || render(&check_overlays(&input));
    let dots = rows();
    assert!(
        dots.contains("✗ ov0: has an interrupted dot rebase"),
        "{dots}"
    );
    assert!(!dots.contains("unmerged"), "{dots}");
    // The user's own conflicted rebase: one warning, update skips it.
    std::fs::remove_file(repo.join(".git/dot-rebase-inflight")).expect("drop record");
    let users = rows();
    assert!(
        users.contains("⚠ ov0: has an unfinished merge, rebase, cherry-pick, revert, or am"),
        "{users}"
    );
    assert!(!users.contains("unmerged"), "{users}");
    assert!(!users.contains('✗'), "{users}");
}

#[test]
fn interrupted_client_rebase_reads_like_update() {
    // K4 and K8 for the client: the same classification and wording as an
    // overlay, with commands that carry the separate Git directory.
    let scratch = TempDir::new("doctor-base-interrupted").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 1);
    let home = &clones[0];
    strand_rebase(home, &remote, true, false);
    let git_dir = home.join(".git");
    // The explicit-worktree layout the separate client uses.
    git(
        home,
        &["config", "core.worktree", home.to_str().expect("utf8")],
    );
    let rows = || render(&check_base_repo(&base("separate", &git_dir, home)));
    let healable = rows();
    assert!(
        healable.contains(
            "⚠ client has an interrupted dot rebase\n    the next dot update aborts it, which discards nothing, and pulls\n"
        ),
        "{healable}"
    );
    assert!(!healable.contains('✗'), "{healable}");
    std::fs::write(home.join("file"), b"edited\n").expect("tracked edit");
    let stuck = rows();
    assert!(
        stuck.contains("✗ client has an interrupted dot rebase\n    dot update fails"),
        "{stuck}"
    );
    assert!(
        stuck.contains(&format!(
            "`git --git-dir={} --work-tree={} rebase --abort`",
            git_dir.display(),
            home.display()
        )),
        "{stuck}"
    );
    // The headless row speaks for the rest: no "upstream is not
    // configured" on top of it.
    assert!(!stuck.contains("upstream"), "{stuck}");
    assert!(!stuck.contains("worktree identity"), "{stuck}");
    git(home, &["checkout", "-q", "--", "file"]);
    git(home, &["rebase", "--abort"]);

    git(home, &["checkout", "-q", "--detach"]);
    let detached = rows();
    assert!(
        detached.contains(
            "⚠ client HEAD is detached\n    dot update skips the client until it is back on a branch\n"
        ),
        "{detached}"
    );
    assert!(!detached.contains("upstream"), "{detached}");

    git(home, &["checkout", "-q", "-b", "side"]);
    let untracked = rows();
    assert!(
        untracked.contains(
            "⚠ client upstream is not configured\n    dot update skips pulling the client\n"
        ),
        "{untracked}"
    );

    git(home, &["checkout", "-q", "main"]);
    git(home, &["branch", "-q", "--set-upstream-to=origin/main"]);
    git(home, &["update-ref", "-d", "refs/remotes/origin/main"]);
    let gone = rows();
    assert!(
        gone.contains(
            "⚠ client upstream could not be compared\n    origin/main; dot update skips pulling the client\n"
        ),
        "{gone}"
    );
}

/// A bare remote with one commit on `main`, plus `count` clones of it.
fn overlay_clones(root: &Path, count: usize) -> (String, Vec<std::path::PathBuf>) {
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&remote).expect("remote");
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).expect("seed");
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("file"), b"one\n").expect("file");
    git(&seed, &["add", "file"]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    let remote_text = remote.to_str().expect("remote utf8").to_string();
    git(&seed, &["push", "-q", &remote_text, "main"]);
    let clones = (0..count)
        .map(|index| {
            let repo = root.join(format!("overlay{index}"));
            let status = dot_test_support::git()
                .args(["clone", "-q"])
                .arg(&remote)
                .arg(&repo)
                .stdin(Stdio::null())
                .status()
                .expect("clone overlay");
            assert!(status.success());
            repo
        })
        .collect();
    (remote_text, clones)
}

/// Overlay inputs naming each clone `ov<index>`.
fn clone_inputs<'a>(
    manifest: String,
    remote: &str,
    clones: &[std::path::PathBuf],
) -> OverlayInputs<'a> {
    let mut input = overlays(manifest, None, false);
    input.configured_count = clones.len();
    input.active_records = clones
        .iter()
        .enumerate()
        .map(|(index, repo)| format!("ov{index}|{}|{remote}||false|git", repo.display()))
        .collect();
    input.overlay_lifecycle = (0..clones.len())
        .map(|index| format!("ov{index}|active|d"))
        .collect();
    input
}

#[test]
fn concurrent_overlay_statuses_land_on_their_own_overlay() {
    // The statuses run in parallel; each result must reach its own row.
    let scratch = TempDir::new("doctor-overlay-state-stress").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 8);
    for (index, repo) in clones.iter().enumerate() {
        match index % 4 {
            1 => std::fs::write(repo.join("file"), b"dirty\n").expect("dirty"),
            2 => git(repo, &["commit", "-q", "--allow-empty", "-m", "ahead"]),
            3 => git(repo, &["checkout", "-q", "--detach"]),
            _ => {}
        }
    }
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let input = clone_inputs(manifest, &remote, &clones);
    for _ in 0..5 {
        let rows = render(&check_overlays(&input));
        for (index, repo) in clones.iter().enumerate() {
            let expected = match index % 4 {
                0 => format!(
                    "✓ ov{index} ({}, main, current with origin/main)",
                    repo.display()
                ),
                1 => format!("⚠ ov{index}: 1 tracked change(s)"),
                2 => format!("⚠ ov{index}: ahead of upstream"),
                _ => format!("⚠ ov{index}: HEAD is detached"),
            };
            assert!(rows.contains(&expected), "missing {expected:?}: {rows}");
        }
        assert_eq!(rows.matches("tracked change(s)").count(), 2, "{rows}");
    }
}

#[test]
fn concurrent_overlay_statuses_use_the_bound_host_git() {
    // Worker threads do not inherit the thread-local host Git binding; the
    // probes must carry it rather than fall back to `git` on PATH.
    let scratch = TempDir::new_exec("doctor-overlay-state-host-git").expect("scratch");
    let (remote, clones) = overlay_clones(scratch.path(), 3);
    let log = scratch.path().join("git.log");
    let wrapper = scratch.path().join("host-git");
    dot_test_support::install_fixture_executable(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexec '{}' \"$@\"\n",
            log.display(),
            dot_test_support::real_tool("git").display()
        ),
        0o755,
    )
    .expect("wrapper");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let input = clone_inputs(manifest, &remote, &clones);
    let rows = {
        let _bound = dot::init_client_identity::bind_host_git_for_scope(&wrapper);
        render(&check_overlays(&input))
    };
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    for repo in &clones {
        let call = format!(
            "-C {} --no-optional-locks status --porcelain=v2 --branch --untracked-files=no",
            repo.display()
        );
        assert!(calls.contains(&call), "missing {call:?} in {calls}\n{rows}");
    }
}

#[test]
fn shdeps_selection_precedence_and_fallback() {
    let scratch = TempDir::new_exec("doctor-shdeps-native").expect("scratch");
    let explicit = scratch.path().join("chosen");
    executable(&explicit);
    let installer = scratch.path().join("install.sh");
    assert_eq!(shdeps_binary(Some(&explicit), &installer), Some(explicit));
    let sibling = scratch.path().join("shdeps");
    executable(&sibling);
    assert_eq!(shdeps_binary(None, &installer), Some(sibling));
    assert_eq!(shdeps_binary(None, Path::new("missing/install.sh")), None);
}

fn provider<'a>(
    kind: Option<&'a str>,
    installer: Option<ProviderInstaller<'a>>,
    binary: Option<&'a str>,
    expected: Option<&'a str>,
    actual: Option<&'a str>,
) -> ProviderInputs<'a> {
    ProviderInputs {
        home: "/home/test",
        dependency_provider: kind,
        policy: "pinned",
        configure_ok: true,
        dev_dir: "/dev",
        development_exists: false,
        development_valid: false,
        installer,
        locked_revision: None,
        development_revision: None,
        binary,
        expected_abi: expected,
        actual_abi: actual,
        cancellation_capability: true,
        prompt_handshake_capability: true,
    }
}

#[test]
fn provider_disabled_unsupported_and_healthy() {
    assert!(
        render(&check_provider(&provider(None, None, None, None, None))).contains("no dependency")
    );
    assert!(
        render(&check_provider(&provider(
            Some("other"),
            None,
            None,
            None,
            None
        )))
        .contains("unsupported")
    );
    let installer = ProviderInstaller {
        path: "/managed/install.sh",
        source: "managed",
    };
    let output = render(&check_provider(&provider(
        Some("shdeps"),
        Some(installer),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    )));
    assert!(output.contains("Shdeps provider ABI (abi:1)"));

    let mut missing_prompt = provider(
        Some("shdeps"),
        Some(ProviderInstaller {
            path: "/managed/install.sh",
            source: "managed",
        }),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    );
    missing_prompt.prompt_handshake_capability = false;
    assert!(
        render(&check_provider(&missing_prompt))
            .contains("Shdeps provider prompt handshake capability is unavailable")
    );
}

#[test]
fn provider_failure_source_development_and_abi_matrix() {
    let mut configure = provider(Some("shdeps"), None, None, None, None);
    configure.configure_ok = false;
    assert!(render(&check_provider(&configure)).contains("provider is unavailable"));

    let no_installer = provider(Some("shdeps"), None, None, None, None);
    assert!(render(&check_provider(&no_installer)).contains("provider is unavailable"));

    let unknown = ProviderInstaller {
        path: "/install",
        source: "unknown",
    };
    assert!(
        render(&check_provider(&provider(
            Some("shdeps"),
            Some(unknown),
            None,
            None,
            None
        )))
        .contains("source is unavailable")
    );

    let explicit = ProviderInstaller {
        path: "/install",
        source: "explicit",
    };
    let explicit_output = render(&check_provider(&provider(
        Some("shdeps"),
        Some(explicit),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:2"),
    )));
    assert!(explicit_output.contains("caller-selected reviewed installer"));
    assert!(explicit_output.contains("ABI mismatch"));

    let mut dev = provider(
        Some("shdeps"),
        Some(ProviderInstaller {
            path: "/dev/shdeps/install.sh",
            source: "pinned-dev",
        }),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    );
    dev.policy = "latest";
    dev.development_exists = true;
    dev.development_valid = true;
    dev.locked_revision = Some("1234567890abcdef");
    dev.development_revision = Some("1234567890abcdef");
    let dev_output = render(&check_provider(&dev));
    // N1: policy, source, and revision are one informational row.
    assert!(
        dev_output.contains(
            "  › Shdeps provider (latest policy; development checkout /dev/shdeps selected by Dot lock; revision 1234567890ab (matches Dot lock))\n"
        ),
        "{dev_output}"
    );
    assert_eq!(dev_output.matches('›').count(), 1, "{dev_output}");

    // Under the latest policy an unpinned revision is expected: no full SHA,
    // no "differs from Dot lock" every run.
    let mut unpinned = provider(
        Some("shdeps"),
        Some(ProviderInstaller {
            path: "/dev/shdeps/install.sh",
            source: "latest-dev",
        }),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    );
    unpinned.policy = "latest";
    unpinned.development_exists = true;
    unpinned.development_valid = true;
    unpinned.locked_revision = Some("1234567890abcdef");
    unpinned.development_revision = Some("fedcba0987654321fedcba0987654321fedcba09");
    let unpinned_output = render(&check_provider(&unpinned));
    assert!(
        unpinned_output.contains(
            "  › Shdeps provider (latest policy; trusted development checkout /dev/shdeps; unpinned revision fedcba098765)\n"
        ),
        "{unpinned_output}"
    );
    assert!(
        !unpinned_output.contains("fedcba0987654321"),
        "{unpinned_output}"
    );

    // The pinned default says only its policy.
    let pinned = render(&check_provider(&provider(
        Some("shdeps"),
        Some(ProviderInstaller {
            path: "/managed/install.sh",
            source: "managed",
        }),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    )));
    assert!(
        pinned.starts_with("\nDependency provider\n  › Shdeps provider (pinned policy)\n"),
        "{pinned}"
    );

    let mut invalid_dev = dev;
    invalid_dev.development_valid = false;
    invalid_dev.installer = Some(ProviderInstaller {
        path: "/managed/install.sh",
        source: "managed",
    });
    assert!(render(&check_provider(&invalid_dev)).contains("development checkout ignored"));

    let managed = ProviderInstaller {
        path: "/managed/install.sh",
        source: "managed",
    };
    let missing_binary = provider(Some("shdeps"), Some(managed), None, Some("1"), None);
    assert!(render(&check_provider(&missing_binary)).contains("binary is unavailable"));

    let mut missing_capability = provider(
        Some("shdeps"),
        Some(ProviderInstaller {
            path: "/managed/install.sh",
            source: "managed",
        }),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    );
    missing_capability.cancellation_capability = false;
    assert!(
        render(&check_provider(&missing_capability))
            .contains("provider cancellation capability is unavailable")
    );
}

#[test]
fn completed_identity_is_plain_and_last_wins() {
    let scratch = TempDir::new("doctor-identity-native").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let marker = scratch.path().join("completed");
    std::fs::write(
        &marker,
        format!(
            "git_dir=/bad\nworktree=/bad\ngit_dir={}/.git\nworktree={}\n",
            home.display(),
            home.display()
        ),
    )
    .expect("marker");
    assert!(completed_identity_matches_home(
        Some(&marker),
        &home.to_string_lossy()
    ));
    assert!(!completed_identity_matches_home(
        None,
        &home.to_string_lossy()
    ));
}

#[test]
fn completed_identity_rejects_malformed_mismatched_and_symlink_markers() {
    let scratch = TempDir::new("doctor-identity-invalid").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let marker = scratch.path().join("completed");
    for body in [
        "",
        "git_dir=/bad\nworktree=/bad\n",
        "git_dir=/home/test/.git\n",
    ] {
        std::fs::write(&marker, body).expect("marker");
        assert!(!completed_identity_matches_home(
            Some(&marker),
            &home.to_string_lossy()
        ));
    }
    let target = scratch.path().join("target");
    std::fs::write(
        &target,
        format!(
            "git_dir={}/.git\nworktree={}\n",
            home.display(),
            home.display()
        ),
    )
    .expect("target");
    std::fs::remove_file(&marker).expect("remove marker");
    std::os::unix::fs::symlink(&target, &marker).expect("symlink marker");
    assert!(!completed_identity_matches_home(
        Some(&marker),
        &home.to_string_lossy()
    ));
}

#[test]
fn client_checkout_needs_root_and_identity_or_flag() {
    let scratch = TempDir::new("doctor-client-native").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    assert!(!is_client_checkout(&home, None));
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["config", "dot.clientRepository", "true"]);
    assert!(is_client_checkout(&home, None));
}

#[test]
fn nested_directory_is_not_the_client_checkout_root() {
    let scratch = TempDir::new("doctor-client-nested").expect("scratch");
    let home = scratch.path().join("home");
    let nested = home.join("nested");
    std::fs::create_dir_all(&nested).expect("nested");
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["config", "dot.clientRepository", "true"]);
    assert!(!is_client_checkout(&nested, None));
}

#[test]
fn base_repo_missing_recognized_and_unrecognized() {
    let missing = BaseRepoInputs {
        topology: "missing",
        client_git_dir: "/home/test/.git",
        home: "/home/test",
        is_client_checkout: false,
    };
    assert!(render(&check_base_repo(&missing)).contains("client repository is missing"));
    let recognized = BaseRepoInputs {
        is_client_checkout: true,
        ..missing
    };
    assert!(render(&check_base_repo(&recognized)).contains("ordinary checkout rooted at $HOME"));
}

fn base<'a>(topology: &'a str, git_dir: &'a Path, home: &'a Path) -> BaseRepoInputs<'a> {
    BaseRepoInputs {
        topology,
        client_git_dir: git_dir.to_str().expect("git dir utf8"),
        home: home.to_str().expect("home utf8"),
        is_client_checkout: false,
    }
}

#[test]
fn base_repo_ordinary_dirty_detached_upstream_and_mismatch_matrix() {
    let scratch = TempDir::new("doctor-base-ordinary").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    let clean = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &home,
    )));
    assert!(clean.contains("ordinary client layout"));
    assert!(clean.contains("no tracked client changes"));
    assert!(clean.contains("client HEAD on branch (main)"));
    assert!(clean.contains("upstream is not configured"));

    std::fs::write(home.join("tracked"), b"one\n").expect("tracked");
    git(&home, &["add", "tracked"]);
    git(&home, &["commit", "-q", "-m", "tracked"]);
    std::fs::write(home.join("tracked"), b"two\n").expect("dirty");
    git(&home, &["checkout", "-q", "--detach"]);
    let dirty = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &home,
    )));
    assert!(dirty.contains("1 tracked client change(s)"));
    assert!(dirty.contains("client HEAD is detached"));

    let nested = home.join("nested");
    std::fs::create_dir_all(&nested).expect("nested");
    let mismatch = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &nested,
    )));
    assert!(mismatch.contains("client worktree mismatch"));
    assert_eq!(
        hint_of(&mismatch, "client worktree mismatch"),
        Some(
            format!(
                "point it at $HOME: git --git-dir={} config core.worktree {}",
                home.join(".git").display(),
                nested.display()
            )
            .as_str()
        ),
        "{mismatch}"
    );

    // Unmerged entries make every pull refuse: a failure, not dirt.
    git(&home, &["checkout", "-q", "main"]);
    git(&home, &["checkout", "-q", "--", "tracked"]);
    git(&home, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(home.join("tracked"), b"theirs\n").expect("theirs");
    git(&home, &["commit", "-q", "-am", "theirs"]);
    git(&home, &["checkout", "-q", "main"]);
    std::fs::write(home.join("tracked"), b"ours\n").expect("ours");
    git(&home, &["commit", "-q", "-am", "ours"]);
    let merge = dot_test_support::git()
        .arg("-C")
        .arg(&home)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "theirs",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("conflicting merge");
    assert!(!merge.success(), "the merge must conflict");
    let conflicted = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &home,
    )));
    // Without an upstream update skips the client, so this only warns, and
    // the unmerged entry still counts as a tracked change.
    assert!(
        conflicted.contains("⚠ 1 unmerged client path(s)"),
        "{conflicted}"
    );
    assert!(
        conflicted.contains("⚠ 1 tracked client change(s)"),
        "{conflicted}"
    );
    assert!(
        !conflicted.contains("no tracked client changes"),
        "{conflicted}"
    );
}

#[test]
fn base_repo_separate_and_unrecognized_topology_matrix() {
    let scratch = TempDir::new("doctor-base-separate").expect("scratch");
    let home = scratch.path().join("home");
    let bare = scratch.path().join("bare.git");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&bare).expect("bare");
    git(&bare, &["init", "-q", "--bare"]);
    let bare_output = render(&check_base_repo(&base("separate", &bare, &home)));
    assert!(bare_output.contains("legacy bare client layout"));

    git(&bare, &["config", "core.bare", "false"]);
    git(
        &bare,
        &["config", "core.worktree", home.to_str().expect("home utf8")],
    );
    let explicit = render(&check_base_repo(&base("separate", &bare, &home)));
    assert!(explicit.contains("explicit-worktree client layout"));

    git(&bare, &["config", "--unset", "core.worktree"]);
    let unidentified = render(&check_base_repo(&base("separate", &bare, &home)));
    assert!(unidentified.contains("no worktree identity"));

    let unknown = render(&check_base_repo(&base("liminal", &bare, &home)));
    assert!(unknown.contains("client worktree mismatch"));
    // No status at all is reported as such, not as a clean, detached
    // checkout without an upstream.
    assert!(
        unknown.contains("⚠ client repository status is unavailable"),
        "{unknown}"
    );
    assert!(!unknown.contains("no tracked client changes"), "{unknown}");
}

#[test]
fn base_repo_upstream_current_ahead_behind_and_diverged() {
    let scratch = TempDir::new("doctor-base-upstream").expect("scratch");
    let remote = scratch.path().join("remote.git");
    std::fs::create_dir_all(&remote).expect("remote");
    git(&remote, &["init", "-q", "--bare"]);
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(
        &home,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("remote utf8"),
        ],
    );
    git(&home, &["push", "-q", "-u", "origin", "main"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let git_dir = home.join(".git");
    let inputs = || base("ordinary", &git_dir, &home);
    // N1: a healthy client folds into one row naming its layout and branch.
    let healthy = render(&check_base_repo(&inputs()));
    assert_eq!(
        healthy,
        "\nClient repository\n  ✓ client repository (~/.git, ordinary layout, main, current with origin/main)\n"
    );
    // A frozen rebase of this HEAD makes every update refuse.
    let head = String::from_utf8(
        dot_test_support::git()
            .arg("-C")
            .arg(&home)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("HEAD")
            .stdout,
    )
    .expect("utf8 HEAD");
    let marker = git_dir.join("dot-rebase-failed");
    std::fs::write(&marker, format!("{} {}\n", head.trim(), head.trim())).expect("marker");
    let frozen = render(&check_base_repo(&inputs()));
    assert!(
        frozen.contains("✗ the last client rebase onto origin/main conflicted; rebase manually"),
        "{frozen}"
    );
    std::fs::remove_file(&marker).expect("remove marker");

    git(&home, &["commit", "-q", "--allow-empty", "-m", "ahead"]);
    assert!(render(&check_base_repo(&inputs())).contains("1 commit(s) ahead"));

    git(&home, &["reset", "-q", "--hard", "origin/main"]);
    let peer = scratch.path().join("peer");
    let status = dot_test_support::git()
        .args(["clone", "-q"])
        .arg(&remote)
        .arg(&peer)
        .status()
        .expect("clone peer");
    assert!(status.success());
    git(&peer, &["commit", "-q", "--allow-empty", "-m", "behind"]);
    git(&peer, &["push", "-q", "origin", "main"]);
    git(&home, &["fetch", "-q", "origin"]);
    assert!(render(&check_base_repo(&inputs())).contains("1 commit(s) behind"));

    git(&home, &["commit", "-q", "--allow-empty", "-m", "local"]);
    assert!(render(&check_base_repo(&inputs())).contains("1 ahead, 1 behind"));
}

/// The `→` next step attached to the first row whose line contains `title`,
/// or `None` when that row has none (or there is no such row).
fn hint_of<'a>(rendered: &'a str, title: &str) -> Option<&'a str> {
    let mut lines = rendered.lines().skip_while(|line| !line.contains(title));
    lines.next()?;
    lines
        .take_while(|line| line.starts_with("    "))
        .find_map(|line| line.strip_prefix("    → "))
}

/// Every warn or fail row in `rendered` that carries no `→` next step: the
/// severity contract says each one must say what to do.
fn problems_without_a_step(rendered: &str) -> Vec<&str> {
    let lines: Vec<&str> = rendered.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("  ⚠ ") || line.starts_with("  ✗ "))
        .filter(|(index, _)| {
            !lines[index + 1..]
                .iter()
                .take_while(|line| line.starts_with("    "))
                .any(|line| line.starts_with("    → "))
        })
        .map(|(_, line)| *line)
        .collect()
}

#[test]
fn update_rows_put_the_next_step_on_its_own_line() {
    // N1: the Update rows used to end their detail with the step
    // ("…; run shdeps health, …") while every other section files it as a
    // `→` hint; now the detail is the cause and the hint is the step.
    const NOW: i64 = 1_800_000_000;
    let degraded = last_run(NOW - 600, "degraded", "cron", "tools");
    let tools = failure_for(&degraded, &[("tools", "ripgrep", "network")], 0);
    let rows = cron_rows(Some(NOW - 6000), Some(degraded), Some(tools), NOW);
    assert!(
        rows.contains(
            "    10m ago; last success 1h40m ago; failing: tools: ripgrep (network)\n    → run shdeps health, or dot update for the full output\n"
        ),
        "{rows}"
    );
    let skip = last_run(NOW - 60, "skip", "cron", "");
    let edits = failure_for(&skip, &[("dirty", ".bashrc", "")], 0);
    let rows = cron_rows(Some(NOW - 600), Some(skip), Some(edits), NOW);
    assert!(
        rows.contains(
            "    last cron run 1m ago; last success 10m ago; edited: .bashrc\n    → run dot status, then commit, stash, or resolve the edits\n"
        ),
        "{rows}"
    );
    let stopped = cron_rows(Some(NOW - 9 * 3600), None, None, NOW);
    assert!(
        stopped.contains(
            "    last success 9h0m ago\n    → check that dot update --cron is scheduled (crontab -l), or run dot update\n"
        ),
        "{stopped}"
    );
    let manual = last_run(NOW - 3 * 3600, "ok", "init", "");
    let never = cron_rows(None, Some(manual), None, NOW);
    assert_eq!(
        hint_of(&never, "cron update has never run"),
        Some("schedule dot update --cron to keep this host current"),
        "{never}"
    );
}

#[test]
fn every_update_warning_carries_a_next_step() {
    const NOW: i64 = 1_800_000_000;
    let fail = last_run(NOW - 60, "fail", "cron", "");
    let hook = failure_for(&fail, &[("configs", "40-claude", "exit 3")], 0);
    let skip = last_run(NOW - 60, "skip", "cron", "");
    let old_skip = last_run(NOW - 48 * 3600, "skip", "cron", "");
    let manual = last_run(NOW - 60, "fail", "manual", "");
    let degraded_manual = last_run(NOW - 60, "degraded", "manual", "prune");
    let mut rendered = vec![
        cron_rows(Some(NOW - 600), Some(fail.clone()), Some(hook.clone()), NOW),
        cron_rows(Some(NOW - 600), Some(fail.clone()), None, NOW),
        cron_rows(
            Some(NOW - 9 * 3600),
            Some(fail.clone()),
            Some(hook.clone()),
            NOW,
        ),
        cron_rows(None, Some(fail.clone()), Some(hook), NOW),
        cron_rows(Some(NOW - 600), Some(skip.clone()), None, NOW),
        cron_rows(Some(NOW - 9 * 3600), Some(skip), None, NOW),
        cron_rows(Some(NOW - 72 * 3600), Some(old_skip), None, NOW),
        cron_rows(Some(NOW - 9 * 3600), None, None, NOW),
        cron_rows(None, Some(manual), None, NOW),
        cron_rows(None, Some(degraded_manual), None, NOW),
    ];
    for (converged_at, run) in [(NOW - 60, None), (NOW - 5400, Some(fail))] {
        rendered.push(render(&check_cron_freshness(&CronInputs {
            last_success: Some(NOW - 5 * 3600),
            last_converged: Some(dot::update_status::Converged {
                at: converged_at,
                failing: "tools".to_string(),
            }),
            last_run: run,
            last_failure: None,
            cron_available: true,
            now: NOW,
        })));
    }
    for rows in &rendered {
        assert!(rows.contains('⚠'), "{rows}");
        assert!(problems_without_a_step(rows).is_empty(), "{rows}");
        // The step is never repeated inline.
        assert!(!rows.contains("; run "), "{rows}");
    }
}

#[test]
fn update_causes_drop_the_error_prefix_and_shorten_at_a_word() {
    // N2: the cause started with Shdeps' own `error:` prefix and was cut at
    // 100 bytes in the middle of a word.
    const NOW: i64 = 1_800_000_000;
    let reason = "failed to configure tmux: the configure script could not find a usable ncurses installation in any standard library directory on this host";
    let run = last_run(NOW - 60, "degraded", "manual", "tools");
    let failure = failure_for(&run, &[("tools", "tmux", &format!("error: {reason}"))], 0);
    let rows = cron_rows(None, Some(run), Some(failure), NOW);
    let line = rows
        .lines()
        .find(|line| line.contains("failing:"))
        .expect("cause line");
    assert!(
        line.contains("tools: tmux (failed to configure tmux: "),
        "{line}"
    );
    assert!(!line.contains("error:"), "{line}");
    let shown = line
        .split_once("tmux (")
        .and_then(|(_, rest)| rest.strip_suffix("…)"))
        .expect("a shortened cause");
    // Cut after a whole word, at a sane length.
    assert!(reason.starts_with(shown), "{shown}");
    assert!(reason[shown.len()..].starts_with(' '), "{shown}");
    assert!((60..=160).contains(&shown.len()), "{shown}");
    // A cause that fits is kept whole, and only a leading prefix goes.
    let run = last_run(NOW - 60, "fail", "manual", "");
    let failure = failure_for(
        &run,
        &[("repos", "dotfiles", "error: pull failed: error: x")],
        0,
    );
    let rows = cron_rows(None, Some(run), Some(failure), NOW);
    assert!(
        rows.contains("repos: dotfiles (pull failed: error: x)"),
        "{rows}"
    );
}

#[test]
fn update_lock_rows_carry_a_next_step() {
    let scratch = TempDir::new("doctor-lock-steps").expect("scratch");
    let mut rendered = vec![render(&check_update_lock(None))];
    let file = scratch.path().join("file");
    std::fs::write(&file, b"unsafe").expect("write");
    rendered.push(render(&check_update_lock(Some(&file))));
    let fresh = scratch.path().join("fresh");
    std::fs::create_dir_all(&fresh).expect("fresh lock");
    rendered.push(render(&check_update_lock(Some(&fresh))));
    let aged = scratch.path().join("aged");
    std::fs::create_dir_all(&aged).expect("aged lock");
    let status = Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(&aged)
        .status()
        .expect("age lock");
    assert!(status.success());
    rendered.push(render(&check_update_lock(Some(&aged))));
    let stale = scratch.path().join("stale");
    std::fs::create_dir_all(&stale).expect("stale lock");
    std::fs::write(
        stale.join("owner"),
        "pid\t42424242\nstart\tproc:1\ntoken\tstale\n",
    )
    .expect("owner");
    rendered.push(render(&check_update_lock(Some(&stale))));
    let state = scratch.path().join("state");
    let log = dot::log::Log::new(false, false);
    let guard =
        dot::update_lock::acquire(&state, false, &log, None, &mut Vec::new()).expect("lock");
    rendered.push(render(&check_update_lock(Some(
        &dot::update_lock::lock_path(&state),
    ))));
    drop(guard);
    for rows in &rendered {
        assert!(rows.contains('⚠') || rows.contains('✗'), "{rows}");
        assert!(problems_without_a_step(rows).is_empty(), "{rows}");
    }
}

#[test]
fn reexec_checkpoint_rows_carry_their_step_as_a_hint() {
    use dot::shdeps::CheckpointState;

    let path = Path::new("/home/u/.local/state/dot/provider-reexec-failed");
    for state in [
        CheckpointState::Pending,
        CheckpointState::Unreadable,
        CheckpointState::Mismatch {
            pinned: "a".repeat(40),
            active: "b".repeat(40),
        },
    ] {
        let rows = render(&check_reexec_checkpoint(&state, path, "/home/u"));
        assert!(problems_without_a_step(&rows).is_empty(), "{rows}");
        assert!(
            !rows.contains("; the next") && !rows.contains("; inspect"),
            "{rows}"
        );
    }
}

#[test]
fn profile_and_client_failures_name_a_next_step() {
    // N1: these rows failed with no detail at all.
    let lifecycle = render(&check_profile_lifecycle(&LifecycleInputs {
        profiles_present: true,
        load_ok: true,
        eligible: vec!["work".into()],
        active: vec!["work|/home/test/.dotfiles-work|url|d|false|git".into()],
        records: vec!["work|/home/test/.dotfiles-work|url|d|false|git".into()],
        extensions_enabled: true,
        deactivation_ok: &|_| false,
    }));
    let step = hint_of(&lifecycle, "active profile deactivation authority unsafe")
        .unwrap_or_else(|| panic!("no step in {lifecycle}"));
    assert!(
        step.contains("~/.dotfiles-work/dot/profile-deactivate")
            && step.contains("chmod go-w")
            && step.contains("dot update"),
        "{step}"
    );

    let scratch = TempDir::new("doctor-overlay-steps").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let mut input = overlays(manifest, None, true);
    input.configured_count = 2;
    input.overlay_lifecycle = vec![
        "required-gone|selected-unavailable|/home/test/.config/dot/overlays.d/20-gone.conf".into(),
        "orphan|active|d".into(),
        "beta|active|/home/test/.config/dot/overlays.d/20-beta.local.conf".into(),
    ];
    input.active_records = vec!["beta.local|/home/test/.dotfiles-beta|url|d|false|git".into()];
    let output = render(&check_overlays(&input));
    let gone = hint_of(&output, "required-gone: selected but unavailable")
        .unwrap_or_else(|| panic!("no step in {output}"));
    assert!(
        gone.contains("dot update") && gone.contains("~/.config/dot/overlays.d/20-gone.conf"),
        "{gone}"
    );
    assert!(
        hint_of(&output, "orphan: active lifecycle record missing").is_some(),
        "{output}"
    );
    // An invalid descriptor drops every active record: its own row is the fix.
    input.discovery_error = Some("bad descriptor");
    let invalid = render(&check_overlays(&input));
    assert_eq!(
        hint_of(&invalid, "orphan: active lifecycle record missing"),
        Some("fix the invalid overlay descriptor reported above, then rerun dot doctor"),
        "{invalid}"
    );
    input.discovery_error = None;
    let local = hint_of(&output, "beta: active lifecycle record missing")
        .unwrap_or_else(|| panic!("no step in {output}"));
    // `sync=none` would reject the descriptor's `url=`: renaming is the fix,
    // and the old checkout is left behind.
    assert!(
        local.starts_with("rename ~/.config/dot/overlays.d/20-beta.local.conf without .local"),
        "{local}"
    );
    assert!(
        local.contains("~/.dotfiles-beta.local checkout stays behind")
            && !local.contains("sync=none"),
        "{local}"
    );

    let home = scratch.path().join("home");
    let bare = scratch.path().join("bare.git");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&bare).expect("bare");
    git(&bare, &["init", "-q", "--bare"]);
    git(&bare, &["config", "core.bare", "false"]);
    let unidentified = render(&check_base_repo(&base("separate", &bare, &home)));
    let step = hint_of(&unidentified, "no worktree identity")
        .unwrap_or_else(|| panic!("no step in {unidentified}"));
    assert_eq!(
        step,
        format!(
            "restore it with: git --git-dir={} config core.worktree {}",
            bare.display(),
            home.display()
        )
    );
}
