//! Native behavioral tests for the doctor check family.

use std::path::Path;
use std::process::{Command, Stdio};

use dot::doctor_checks::{
    BaseRepoInputs, CronInputs, InstallInputs, LifecycleInputs, MergeInputs, MergeSpec,
    OverlayInputs, ProviderInputs, ProviderInstaller, Record, check_base_repo,
    check_cron_freshness, check_install_layout, check_merges, check_overlays,
    check_profile_lifecycle, check_provider, check_reexec_checkpoint, check_update_lock,
    completed_identity_matches_home, is_client_checkout, render, shdeps_binary,
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
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
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
        stale.contains("last update: init run 3h0m ago; schedule dot update --cron"),
        "{stale}"
    );
    assert!(!stale.contains('✗'), "{stale}");
    // A cron that keeps skipping for local edits is running, not missing.
    let skipping = check(Some(last_run(NOW - 60, "skip", "cron", "")));
    assert!(
        skipping.contains("⚠ cron update has not succeeded recently"),
        "{skipping}"
    );
    assert!(skipping.contains("last cron run skip 1m ago"), "{skipping}");
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
        failing.contains("no successful cron update recorded; last cron run fail 1m ago"),
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
        failed.contains("manual run 1m ago; rerun dot update to see what failed"),
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

fn layout(root: &Path, source: &Path, release_root: bool, shdeps: bool) -> String {
    render(&check_install_layout(&InstallInputs {
        home: "/nonexistent-home",
        source_real: source,
        release_root,
        managed_root: root,
        shdeps,
    }))
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
        alone.contains("✓ dot release layout (standalone installer"),
        "{alone}"
    );
    assert!(!alone.contains('✗'), "{alone}");
    let running = layout(&scratch.path().join("absent/dot"), &release, true, false);
    assert!(
        running.contains("✓ dot release layout (standalone installer"),
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
    assert_eq!(healthy, "  ✓ dot release layout (Shdeps release)\n");

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
    assert!(
        rows.contains("✓ dot release layout (Shdeps release)"),
        "{rows}"
    );
    assert!(
        rows.contains("⚠ interrupted Shdeps install left a backup"),
        "{rows}"
    );
    assert!(rows.contains("dot.shdeps-archive-backup-42-7"), "{rows}");
    assert!(rows.contains(".dot.shdeps-archive-backup-43-8"), "{rows}");
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
        sidecar: None,
        family_dir: None,
        outputs,
        invalid: vec![],
    }
}

#[test]
fn merge_outputs_verify_fresh_missing_and_stale() {
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
            specs,
        }))
    };

    // Fresh output (newer than the script) passes.
    let fresh_out = scratch.path().join("fresh.conf");
    backdate(&script, 100);
    std::fs::write(&fresh_out, b"live\n").expect("fresh output");
    let fresh = check(vec![merge_spec(
        "fixture",
        &script,
        vec![fresh_out.to_string_lossy().into_owned()],
    )]);
    assert!(fresh.contains("1 hook(s)"));
    assert!(fresh.contains("✓ merge-hook outputs are current"));
    assert!(!fresh.contains('✗'));

    // Missing output fails.
    let missing = check(vec![merge_spec(
        "fixture",
        &script,
        vec![
            scratch
                .path()
                .join("absent.conf")
                .to_string_lossy()
                .into_owned(),
        ],
    )]);
    assert!(missing.contains("✗ merge-hook output is missing"));

    // Stale output (older than the script) fails.
    let stale_out = scratch.path().join("stale.conf");
    std::fs::write(&stale_out, b"stale\n").expect("stale output");
    backdate(&stale_out, 200);
    let stale = check(vec![merge_spec(
        "fixture",
        &script,
        vec![stale_out.to_string_lossy().into_owned()],
    )]);
    assert!(stale.contains("✗ merge-hook output is stale"));

    // Equal mtimes fail: outputs must be strictly newer than inputs.
    let tied_out = scratch.path().join("tied.conf");
    std::fs::write(&tied_out, b"tied\n").expect("tied output");
    let script_mtime = std::fs::metadata(&script)
        .expect("script meta")
        .modified()
        .expect("mtime");
    std::fs::File::options()
        .write(true)
        .open(&tied_out)
        .expect("open tied output")
        .set_modified(script_mtime)
        .expect("tie mtime");
    let tied = check(vec![merge_spec(
        "fixture",
        &script,
        vec![tied_out.to_string_lossy().into_owned()],
    )]);
    assert!(tied.contains("✗ merge-hook output is stale"));

    // A newer family input also makes the output stale.
    let family = ext.join("merge-hooks.d/fixture");
    std::fs::create_dir_all(&family).expect("family");
    std::fs::write(family.join("input.conf"), b"input\n").expect("family input");
    let mut family_spec = merge_spec(
        "fixture",
        &script,
        vec![fresh_out.to_string_lossy().into_owned()],
    );
    family_spec.family_dir = Some(family.to_string_lossy().into_owned());
    let family_stale = check(vec![family_spec]);
    assert!(family_stale.contains("✗ merge-hook output is stale"));

    // No declared outputs skips (documented behavior, not a failure).
    let undeclared = check(vec![merge_spec("fixture", &script, vec![])]);
    assert!(undeclared.contains("· merge-hook outputs are unverified"));
    assert!(undeclared.contains("1 hook(s) declare no checkable outputs"));
    assert!(!undeclared.contains('✗'));

    // C1: healthy and unverified hooks each collapse into one summary row;
    // problems keep a row each.
    let mut many: Vec<MergeSpec> = (0..30)
        .map(|index| merge_spec(&format!("bare{index}"), &script, vec![]))
        .collect();
    many.push(merge_spec(
        "fresh-a",
        &script,
        vec![fresh_out.to_string_lossy().into_owned()],
    ));
    many.push(merge_spec(
        "fresh-b",
        &script,
        vec![fresh_out.to_string_lossy().into_owned()],
    ));
    many.push(merge_spec(
        "stale",
        &script,
        vec![stale_out.to_string_lossy().into_owned()],
    ));
    let collapsed = check(many);
    assert_eq!(
        collapsed
            .matches("merge-hook outputs are unverified")
            .count(),
        1
    );
    assert!(collapsed.contains("(30 hook(s) declare no checkable outputs)"));
    assert!(collapsed.contains("✓ merge-hook outputs are current (2 output(s) across 2 hook(s))"));
    assert!(collapsed.contains("✗ merge-hook output is stale\n    stale: "));
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
    assert!(render(&check_overlays(&input)).contains("1 overlay symlink issue(s)"));
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
    assert!(dev_output.contains("development checkout"));
    assert!(dev_output.contains("matches Dot lock: 1234567890ab"));

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
    assert!(unknown.contains("client upstream is not configured"));
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
    assert!(render(&check_base_repo(&inputs())).contains("origin/main (current)"));

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
