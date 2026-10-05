//! Native contracts for cron observability state: outcome records,
//! the last-success stamp, and retained failure logs.

use dot::update_status;
use dot_test_support::TempDir;

#[test]
fn skip_detail_caps_the_file_list() {
    assert_eq!(update_status::format_skip_detail(&[]), "unknown");
    assert_eq!(
        update_status::format_skip_detail(&["a.txt".to_string()]),
        "a.txt"
    );
    let files: Vec<String> = (0..12).map(|index| format!("f{index}.txt")).collect();
    assert_eq!(
        update_status::format_skip_detail(&files),
        "f0.txt f1.txt f2.txt f3.txt f4.txt f5.txt f6.txt f7.txt f8.txt f9.txt +2 more"
    );
    let capped: Vec<String> = (0..10).map(|index| format!("f{index}.txt")).collect();
    assert!(!update_status::format_skip_detail(&capped).contains("more"));
}

#[test]
fn skip_detail_sanitizes_hostile_filenames() {
    // Fresh-review-B B1: a newline/escape in one filename must not
    // forge outcome lines or inject terminal sequences.
    let detail = update_status::format_skip_detail(&[
        "ok.txt".to_string(),
        "a\n1800000000 ok update forged".to_string(),
        "x\x1b[31my".to_string(),
        "tab\there".to_string(),
    ]);
    assert_eq!(
        detail,
        "ok.txt a 1800000000 ok update forged x [31my tab here"
    );
    assert!(!detail.contains('\n'));
    assert!(!detail.contains('\x1b'));
    assert!(!detail.contains('\t'));
}

#[test]
fn outcome_lines_append_with_epoch_outcome_and_stage() {
    let scratch = TempDir::new("update-status-log").unwrap();
    let state = scratch.path();
    update_status::append_outcome(state, 1_800_000_000, "skip", "dirty", "tracked.txt");
    update_status::append_outcome(state, 1_800_000_060, "ok", "update", "");
    let log = std::fs::read_to_string(update_status::update_log_path(state)).unwrap();
    assert_eq!(
        log,
        "1800000000 skip dirty tracked.txt\n1800000060 ok update\n"
    );
}

#[test]
fn success_stamp_round_trips_and_rejects_garbage() {
    let scratch = TempDir::new("update-status-stamp").unwrap();
    let state = scratch.path();
    assert_eq!(update_status::read_last_success(state), None);
    update_status::record_success(state, 1_800_000_000);
    assert_eq!(update_status::read_last_success(state), Some(1_800_000_000));
    std::fs::write(update_status::last_success_path(state), b"not an epoch\n").unwrap();
    assert_eq!(update_status::read_last_success(state), None);
}

#[test]
fn convergence_stamp_round_trips_failing_stages() {
    let scratch = TempDir::new("update-status-converged").unwrap();
    let state = scratch.path();
    assert_eq!(update_status::read_last_converged(state), None);
    let degraded = update_status::Degraded {
        config: false,
        tools: true,
        prune: true,
    };
    assert_eq!(degraded.detail(), "tools,prune");
    update_status::record_converged(state, 1_800_000_000, degraded);
    assert_eq!(
        std::fs::read_to_string(update_status::last_converged_path(state)).unwrap(),
        "1800000000 tools,prune\n"
    );
    assert_eq!(
        update_status::read_last_converged(state),
        Some(update_status::Converged {
            at: 1_800_000_000,
            failing: "tools,prune".to_string(),
        })
    );
    // A clean run overwrites the list away, leaving the success format.
    update_status::record_converged(state, 1_800_000_060, update_status::Degraded::default());
    assert_eq!(
        std::fs::read_to_string(update_status::last_converged_path(state)).unwrap(),
        "1800000060\n"
    );
    assert_eq!(
        update_status::read_last_converged(state),
        Some(update_status::Converged {
            at: 1_800_000_060,
            failing: String::new(),
        })
    );
}

#[test]
fn degraded_detail_lists_stages_in_run_order() {
    let only = |config, tools, prune| {
        update_status::Degraded {
            config,
            tools,
            prune,
        }
        .detail()
    };
    assert_eq!(only(false, false, false), "");
    assert_eq!(only(true, false, false), "config");
    assert_eq!(only(false, true, false), "tools");
    assert_eq!(only(false, false, true), "prune");
    assert_eq!(only(true, true, true), "config,tools,prune");
    assert!(update_status::Degraded::default().is_empty());
    for (config, prune) in [(false, true), (true, false)] {
        assert!(
            !update_status::Degraded {
                config,
                tools: false,
                prune,
            }
            .is_empty()
        );
    }
}

#[test]
fn convergence_stamp_rejects_garbage_and_hostile_stage_text() {
    let scratch = TempDir::new("update-status-converged-garbage").unwrap();
    let state = scratch.path();
    std::fs::create_dir_all(update_status::dot_dir(state)).unwrap();
    let path = update_status::last_converged_path(state);
    for body in [
        &b"not an epoch\n"[..],
        b"",
        b"1800000000 tools prune\n",
        b"1800000000 tools\x1b[31m\n",
        b"1800000000 \n",
        b"1800000000 ,\n",
        b"1800000000 tools,,prune\n",
        &[b'9'; 1024][..],
    ] {
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            update_status::read_last_converged(state),
            None,
            "{:?}",
            String::from_utf8_lossy(body)
        );
    }
    // A stage name only a newer Dot writes still reads (and renders).
    std::fs::write(&path, b"1800000000 tools,configs\n").unwrap();
    assert_eq!(
        update_status::read_last_converged(state),
        Some(update_status::Converged {
            at: 1_800_000_000,
            failing: "tools,configs".to_string(),
        })
    );
}

#[test]
fn success_stamp_rejects_oversized_and_extreme_values() {
    // Fresh-review-B B3: a corrupt multi-GB stamp must not OOM the
    // reader, and `i64::MIN` must read stale (never overflow).
    let scratch = TempDir::new("update-status-stamp-cap").unwrap();
    let state = scratch.path();
    std::fs::create_dir_all(update_status::dot_dir(state)).unwrap();
    std::fs::write(update_status::last_success_path(state), vec![b'9'; 1024]).unwrap();
    assert_eq!(update_status::read_last_success(state), None);
    std::fs::write(
        update_status::last_success_path(state),
        format!("{}\n", i64::MIN),
    )
    .unwrap();
    assert_eq!(update_status::read_last_success(state), Some(i64::MIN));
    assert!(update_status::is_stale(i64::MIN, 1_800_000_000));
    assert!(update_status::is_stale(i64::MIN, i64::MAX));
    assert!(!update_status::is_stale(i64::MAX, i64::MIN));
}

#[test]
fn staleness_is_strictly_older_than_two_hours() {
    assert_eq!(update_status::CRON_STALE_AFTER_SECS, 7200);
    assert!(!update_status::is_stale(
        1_800_000_000 - 7200,
        1_800_000_000
    ));
    assert!(update_status::is_stale(1_800_000_000 - 7201, 1_800_000_000));
    assert!(!update_status::is_stale(1_800_000_000 + 60, 1_800_000_000));
}

#[test]
fn age_format_scales_seconds_minutes_and_hours() {
    for (secs, expected) in [
        (-5, "0s"),
        (0, "0s"),
        (59, "59s"),
        (60, "1m"),
        (3599, "59m"),
        (3600, "1h0m"),
        (7500, "2h5m"),
    ] {
        assert_eq!(update_status::format_age(secs), expected);
    }
}

#[test]
fn label_sanitization_keeps_filenames_safe() {
    assert_eq!(update_status::sanitize_label("pull"), "pull");
    assert_eq!(update_status::sanitize_label("a/b c.log"), "a_b_c.log");
    assert_eq!(update_status::sanitize_label(""), "command");
}

#[test]
fn retained_logs_move_and_prune_to_newest_twenty() {
    let scratch = TempDir::new("update-status-logs").unwrap();
    let logs = update_status::logs_dir(scratch.path());
    let now = std::time::SystemTime::now();
    for index in 0..25 {
        let path = logs.join(format!("seed-{index:02}.log"));
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(&path, b"seed\n").unwrap();
        let mtime = now - std::time::Duration::from_secs(3600 + index as u64);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
    std::fs::write(logs.join("notes.txt"), b"not a log\n").unwrap();
    let failed = scratch.path().join("scratch.log");
    std::fs::write(&failed, b"boom\n").unwrap();
    let retained = update_status::retain_failed_log(&logs, "pul/l", 1_800_000_000, &failed)
        .expect("retained log");
    assert!(!failed.exists());
    assert_eq!(std::fs::read(&retained).unwrap(), b"boom\n");
    assert!(
        retained
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pul_l-1800000000-")
    );
    let mut names: Vec<_> = std::fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    // Twenty logs survive (non-log files are never managed): the
    // retained failure plus the nineteen newest seeds, so the six
    // oldest seeds fall off.
    assert_eq!(names.len(), update_status::MAX_RETAINED_LOGS + 1);
    assert!(names.contains(&"notes.txt".to_string()));
    for pruned in ["seed-24.log", "seed-19.log"] {
        assert!(!names.contains(&pruned.to_string()), "{names:?}");
    }
    assert!(names.contains(&"seed-18.log".to_string()));
}

#[test]
fn planted_fifos_never_block_state_access() {
    // Fresh-review-B B4: a same-uid FIFO pre-planted at a state path
    // must be refused promptly, never wedge a cron run or doctor.
    // The join timeout keeps a regression from hanging the suite.
    let scratch = TempDir::new("update-status-fifo").unwrap();
    let state = scratch.path().to_path_buf();
    std::fs::create_dir_all(update_status::dot_dir(&state)).unwrap();
    for path in [
        update_status::update_log_path(&state),
        update_status::last_success_path(&state),
        update_status::last_converged_path(&state),
        update_status::last_failure_path(&state),
    ] {
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
    }
    let worker = std::thread::spawn(move || {
        update_status::append_outcome(&state, 1_800_000_000, "ok", "update", "");
        update_status::record_success(&state, 1_800_000_000);
        update_status::record_converged(&state, 1_800_000_000, update_status::Degraded::default());
        update_status::record_last_failure(
            &state,
            1_800_000_000,
            "fail",
            update_status::Trigger::Cron,
            &update_status::Failures::default(),
        );
        (
            update_status::read_last_success(&state),
            update_status::read_last_converged(&state),
            update_status::read_last_failure(&state),
        )
    });
    let deadline = std::time::Duration::from_secs(10);
    let start = std::time::Instant::now();
    let stamp = loop {
        if worker.is_finished() {
            break worker.join().expect("state worker");
        }
        assert!(
            start.elapsed() < deadline,
            "state access blocked on a planted FIFO"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(stamp, (None, None, None));
}

#[test]
fn cron_state_stays_owner_only() {
    // Fresh-review-B E1: `dot/`, `update.log`, the stamp, and `logs/`
    // are 0700/0600 even when pre-existing paths are wider.
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = TempDir::new("update-status-modes").unwrap();
    let state = scratch.path();
    let dot = update_status::dot_dir(state);
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = update_status::update_log_path(state);
    std::fs::write(&log, b"old\n").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
    let stamp = update_status::last_success_path(state);
    std::fs::write(&stamp, b"1\n").unwrap();
    std::fs::set_permissions(&stamp, std::fs::Permissions::from_mode(0o644)).unwrap();
    let logs = update_status::logs_dir(state);
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o755)).unwrap();

    update_status::append_outcome(state, 1_800_000_000, "ok", "update", "");
    update_status::record_success(state, 1_800_000_000);
    let converged = update_status::last_converged_path(state);
    std::fs::write(&converged, b"1\n").unwrap();
    std::fs::set_permissions(&converged, std::fs::Permissions::from_mode(0o644)).unwrap();
    update_status::record_converged(state, 1_800_000_000, update_status::Degraded::default());
    let cause = update_status::last_failure_path(state);
    std::fs::write(&cause, b"1 fail cron\n").unwrap();
    std::fs::set_permissions(&cause, std::fs::Permissions::from_mode(0o644)).unwrap();
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Cron,
        &update_status::Failures::default(),
    );
    let failed = state.join("scratch.log");
    std::fs::write(&failed, b"boom\n").unwrap();
    update_status::retain_failed_log(&logs, "cmd", 1_800_000_000, &failed).expect("retained log");

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path).expect("mode").permissions().mode() & 0o777
    };
    assert_eq!(mode(&dot), 0o700);
    assert_eq!(mode(&log), 0o600);
    assert_eq!(mode(&stamp), 0o600);
    assert_eq!(mode(&converged), 0o600);
    assert_eq!(mode(&cause), 0o600);
    assert_eq!(mode(&logs), 0o700);
}

#[test]
fn failed_copy_removes_its_partial_retained_log() {
    // Fresh-review-A nit-1: when the cross-filesystem copy fallback
    // fails midway, no partial entry may consume a retained slot.
    // Permission-gated (root reads through 0o000, so the forced
    // failure cannot trigger there).
    if dot::temp::current_uid().is_some_and(|uid| uid == 0) {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = TempDir::new("update-status-partial").unwrap();
    let logs = update_status::logs_dir(scratch.path());
    std::fs::create_dir_all(&logs).unwrap();
    // The exact retained name for this process, label, and epoch.
    let retained = logs.join(format!("cmd-1800000000-{}.log", std::process::id()));
    std::fs::write(&retained, b"stale\n").unwrap();
    // An unreadable scratch file inside an unmodifiable directory:
    // rename fails (no source-dir write), the copy truncates the
    // pre-existing entry and then fails reading the source.
    let hold = scratch.path().join("hold");
    std::fs::create_dir(&hold).unwrap();
    let locked = hold.join("locked.log");
    std::fs::write(&locked, b"data\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&hold, std::fs::Permissions::from_mode(0o555)).unwrap();

    let result = update_status::retain_failed_log(&logs, "cmd", 1_800_000_000, &locked);

    std::fs::set_permissions(&hold, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_none());
    assert!(
        !retained.exists(),
        "failed copy left a partial retained log"
    );
    assert!(locked.exists());
}

#[test]
fn state_paths_live_under_the_dot_directory() {
    let state = std::path::Path::new("/tmp/state-fixture");
    assert_eq!(
        update_status::update_log_path(state),
        state.join("dot/update.log")
    );
    assert_eq!(
        update_status::last_success_path(state),
        state.join("dot/update.last-success")
    );
    assert_eq!(
        update_status::last_converged_path(state),
        state.join("dot/update.last-converged")
    );
    assert_eq!(update_status::logs_dir(state), state.join("dot/logs"));
}

#[test]
fn last_run_stamp_round_trips_every_trigger_and_outcome() {
    use std::os::unix::fs::PermissionsExt as _;
    use update_status::{
        Degraded, LastRun, OUTCOME_DEGRADED, OUTCOME_FAIL, OUTCOME_OK, OUTCOME_SKIP, Trigger,
    };

    let scratch = TempDir::new("update-status-last-run").unwrap();
    let state = scratch.path();
    assert_eq!(update_status::read_last_run(state), None);
    let degraded = Degraded {
        tools: true,
        prune: true,
        ..Degraded::default()
    };
    for (outcome, trigger, stages, failing) in [
        (OUTCOME_OK, Trigger::Manual, Degraded::default(), ""),
        (OUTCOME_FAIL, Trigger::Cron, Degraded::default(), ""),
        (OUTCOME_SKIP, Trigger::Cron, Degraded::default(), ""),
        (OUTCOME_DEGRADED, Trigger::Init, degraded, "tools,prune"),
        // Stages only travel with a degraded outcome.
        (OUTCOME_FAIL, Trigger::Manual, degraded, ""),
    ] {
        update_status::record_last_run(state, 1_800_000_000, outcome, trigger, stages);
        assert_eq!(
            update_status::read_last_run(state),
            Some(LastRun {
                at: 1_800_000_000,
                outcome: outcome.to_string(),
                trigger: trigger.as_str().to_string(),
                failing: failing.to_string(),
            })
        );
    }
    assert_eq!(
        std::fs::read_to_string(state.join("dot/update.last-run")).unwrap(),
        "1800000000 fail manual\n",
        "the on-disk layout is the contract other Dot versions read"
    );
    assert_eq!(
        std::fs::metadata(update_status::last_run_path(state))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn last_run_stamp_accepts_newer_words_and_rejects_garbage() {
    let scratch = TempDir::new("update-status-last-run-garbage").unwrap();
    let state = scratch.path();
    let path = update_status::last_run_path(state);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A newer Dot's vocabulary still reads.
    std::fs::write(&path, "1800000000 aborted remote stage\n").unwrap();
    let newer = update_status::read_last_run(state).expect("newer words");
    assert_eq!(
        (
            newer.outcome.as_str(),
            newer.trigger.as_str(),
            newer.failing.as_str()
        ),
        ("aborted", "remote", "stage")
    );
    for garbage in [
        "",
        "not-an-epoch ok manual\n",
        "1800000000\n",
        "1800000000 ok\n",
        "1800000000 OK manual\n",
        "1800000000 ok manual tools,,prune\n",
        "1800000000 ok manual tools extra\n",
        "1800000000 ok \x1b[31m\n",
    ] {
        std::fs::write(&path, garbage).unwrap();
        assert_eq!(update_status::read_last_run(state), None, "{garbage:?}");
    }
    std::fs::write(&path, vec![b'9'; 1024]).unwrap();
    assert_eq!(update_status::read_last_run(state), None);
}

fn failures(items: &[(&'static str, &str, &str)]) -> update_status::Failures {
    let mut failures = update_status::Failures::default();
    for (stage, name, detail) in items {
        failures.push(stage, name, detail);
    }
    failures
}

#[test]
fn last_failure_round_trips_its_header_and_items() {
    let scratch = TempDir::new("update-status-failure").unwrap();
    let state = scratch.path();
    assert_eq!(update_status::read_last_failure(state), None);
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "degraded",
        update_status::Trigger::Cron,
        &failures(&[
            ("tools", "watchexec/watchexec", "blocked transition"),
            ("prune", "shdeps prune", ""),
        ]),
    );
    assert_eq!(
        std::fs::read_to_string(update_status::last_failure_path(state)).unwrap(),
        "1800000000 degraded cron\nitem\ttools\twatchexec/watchexec\tblocked transition\nitem\tprune\tshdeps prune\t\n"
    );
    let failure = update_status::read_last_failure(state).expect("record");
    assert_eq!(
        (
            failure.at,
            failure.outcome.as_str(),
            failure.trigger.as_str()
        ),
        (1_800_000_000, "degraded", "cron")
    );
    assert_eq!(failure.items.len(), 2);
    assert_eq!(failure.items[0].name, "watchexec/watchexec");
    assert_eq!(failure.items[1].detail, "");
    assert_eq!(failure.omitted, 0);
    // It describes exactly the run whose stamp shares its header.
    let run = |at, outcome: &str, trigger: &str| update_status::LastRun {
        at,
        outcome: outcome.to_string(),
        trigger: trigger.to_string(),
        failing: "tools,prune".to_string(),
    };
    assert!(failure.describes(&run(1_800_000_000, "degraded", "cron")));
    assert!(!failure.describes(&run(1_800_000_060, "degraded", "cron")));
    assert!(!failure.describes(&run(1_800_000_000, "fail", "cron")));
    assert!(!failure.describes(&run(1_800_000_000, "degraded", "manual")));
}

#[test]
fn last_failure_without_items_still_records_its_run() {
    // "Cause unknown" for this run must not fall back to an older cause.
    let scratch = TempDir::new("update-status-failure-empty").unwrap();
    let state = scratch.path();
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Manual,
        &update_status::Failures::default(),
    );
    let failure = update_status::read_last_failure(state).expect("record");
    assert!(failure.items.is_empty());
    assert_eq!(failure.trigger, "manual");
}

#[test]
fn last_failure_caps_items_per_stage_and_counts_the_rest() {
    let scratch = TempDir::new("update-status-failure-cap").unwrap();
    let state = scratch.path();
    let names: Vec<String> = (0..8).map(|index| format!("pkg{index}")).collect();
    let mut many = update_status::Failures::default();
    for name in &names {
        many.push(update_status::STAGE_TOOLS, name, "failed");
    }
    many.push(update_status::STAGE_CONFIGS, "10-hook", "exit 3");
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Cron,
        &many,
    );
    let failure = update_status::read_last_failure(state).expect("record");
    let kept: Vec<&str> = failure
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(kept, ["pkg0", "pkg1", "pkg2", "pkg3", "pkg4", "10-hook"]);
    assert_eq!(failure.omitted, 3);
}

#[test]
fn last_failure_stays_within_its_byte_cap() {
    // Long details across many items: the body never exceeds the cap, and
    // whatever did not fit is still counted.
    let scratch = TempDir::new("update-status-failure-bytes").unwrap();
    let state = scratch.path();
    let detail = "d".repeat(1000);
    let name = "n".repeat(1000);
    let stages = [
        update_status::STAGE_REPOS,
        update_status::STAGE_OVERLAYS,
        update_status::STAGE_TOOLS,
        update_status::STAGE_PRUNE,
        update_status::STAGE_CONFIGS,
        update_status::STAGE_CONFIG,
        update_status::STAGE_UPDATE,
    ];
    let mut many = update_status::Failures::default();
    for stage in stages {
        for _ in 0..5 {
            many.push(stage, &name, &detail);
        }
    }
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Cron,
        &many,
    );
    let body = std::fs::read(update_status::last_failure_path(state)).unwrap();
    assert!(
        body.len() <= update_status::FAILURE_MAX_BYTES,
        "{}",
        body.len()
    );
    let failure = update_status::read_last_failure(state).expect("record");
    assert!(!failure.items.is_empty());
    assert_eq!(failure.items.len() + failure.omitted, stages.len() * 5);
    // Fields are cut on a character boundary and say so.
    assert!(
        failure.items[0].name.ends_with('…'),
        "{:?}",
        failure.items[0]
    );
    assert!(failure.items[0].name.len() <= 120);
    assert!(failure.items[0].detail.len() <= 240);
}

#[test]
fn last_failure_keeps_every_more_count_when_the_body_is_nearly_full() {
    // Kept items that bring the body within a few bytes of the cap, then
    // several stages that are only ever omitted: each of their `more` lines
    // must still fit, or the reader would cut them and doctor would
    // undercount "+N more".
    let scratch = TempDir::new("update-status-failure-more").unwrap();
    let state = scratch.path();
    let full = "d".repeat(240);
    let mut many = update_status::Failures::default();
    for stage in [
        update_status::STAGE_TOOLS,
        update_status::STAGE_CONFIGS,
        update_status::STAGE_OVERLAYS,
    ] {
        for index in 0..5 {
            many.push(stage, &format!("{index}"), &full);
        }
    }
    // Sized to leave the body just under the cap under a reserve that only
    // covers one omitted stage.
    many.push(update_status::STAGE_UPDATE, "n", &"u".repeat(150));
    let late = [
        update_status::STAGE_REPOS,
        update_status::STAGE_DIRTY,
        update_status::STAGE_PRUNE,
        update_status::STAGE_CONFIG,
        update_status::STAGE_TOOLS,
        update_status::STAGE_CONFIGS,
    ];
    for stage in late {
        many.push(stage, "late", &full);
    }
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Cron,
        &many,
    );
    let body = std::fs::read(update_status::last_failure_path(state)).unwrap();
    assert!(
        body.len() <= update_status::FAILURE_MAX_BYTES,
        "{}",
        body.len()
    );
    let failure = update_status::read_last_failure(state).expect("record");
    assert_eq!(failure.items.len() + failure.omitted, 22, "{failure:?}");
}

#[test]
fn last_failure_fields_are_control_sanitized() {
    // Provider events, hook output, and filenames are untrusted: none may
    // forge record lines or reach doctor's terminal as escape sequences.
    let scratch = TempDir::new("update-status-failure-hostile").unwrap();
    let state = scratch.path();
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Cron,
        &failures(&[(
            "tools",
            "evil\tname\nitem\ttools\tforged\tline",
            "red \x1b[31mtext\u{9b}31m \r\n tail",
        )]),
    );
    let body = std::fs::read_to_string(update_status::last_failure_path(state)).unwrap();
    assert_eq!(body.lines().count(), 2, "{body:?}");
    let failure = update_status::read_last_failure(state).expect("record");
    assert_eq!(failure.items.len(), 1);
    assert_eq!(failure.items[0].name, "evil name item tools forged line");
    // Color sequences go whole, not just their escape byte.
    assert_eq!(failure.items[0].detail, "red text tail");
    // A cap too small for the ellipsis keeps nothing instead of panicking.
    assert_eq!(update_status::clean_field("abcdef", 2), "");
    assert_eq!(update_status::clean_field("abcdef", 4), "a…");
    // An item with nothing to say is dropped.
    let mut empty = update_status::Failures::default();
    empty.push(update_status::STAGE_TOOLS, " \t", "\n");
    assert!(empty.is_empty());
}

#[test]
fn last_failure_reader_tolerates_newer_and_hostile_records() {
    let scratch = TempDir::new("update-status-failure-reader").unwrap();
    let state = scratch.path();
    let path = update_status::last_failure_path(state);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Newer line kinds and extra fields are skipped or ignored; items keep
    // reading; hostile text written by something else is cleaned again.
    std::fs::write(
        &path,
        "1800000000 aborted cron\nnote\tsomething new\nitem\ttools\tpkg\twhy\textra\nitem\tBad Stage\tx\ty\nitem\tnewstage\tn\t\x1b[2Jd\nmore\ttools\t2\nmore\ttools\tnot-a-number\n",
    )
    .unwrap();
    let failure = update_status::read_last_failure(state).expect("record");
    assert_eq!(failure.outcome, "aborted");
    let items: Vec<(&str, &str, &str)> = failure
        .items
        .iter()
        .map(|item| {
            (
                item.stage.as_str(),
                item.name.as_str(),
                item.detail.as_str(),
            )
        })
        .collect();
    assert_eq!(items, [("tools", "pkg", "why"), ("newstage", "n", "d")]);
    assert_eq!(failure.omitted, 2);
    // A malformed header is no record at all; extra header fields from a
    // newer Dot are not malformed.
    std::fs::write(
        &path,
        "1800000000 fail cron future-field\nitem\ttools\tpkg\twhy\n",
    )
    .unwrap();
    assert_eq!(
        update_status::read_last_failure(state)
            .expect("newer header")
            .items
            .len(),
        1
    );
    for header in ["", "x fail cron", "1800000000 Fail cron", "1800000000 fail"] {
        std::fs::write(&path, format!("{header}\nitem\ttools\tpkg\twhy\n")).unwrap();
        assert_eq!(update_status::read_last_failure(state), None, "{header:?}");
    }
    // An oversized record from a newer Dot yields its leading whole lines.
    let mut big = String::from("1800000000 fail cron\n");
    while big.len() < update_status::FAILURE_MAX_BYTES + 500 {
        big.push_str("item\ttools\tpkg\t");
        big.push_str(&"x".repeat(100));
        big.push('\n');
    }
    std::fs::write(&path, &big).unwrap();
    let failure = update_status::read_last_failure(state).expect("leading lines");
    assert!(!failure.items.is_empty());
    assert!(
        failure
            .items
            .iter()
            .all(|item| item.detail == "x".repeat(100))
    );
}

#[test]
fn last_failure_redacts_url_credentials_when_written() {
    // A pull error or hook line can echo a token-bearing remote; the record
    // outlives the run, and doctor prints it to a terminal or transcript.
    let scratch = TempDir::new("update-status-failure-redact").unwrap();
    let state = scratch.path();
    update_status::record_last_failure(
        state,
        1_800_000_000,
        "fail",
        update_status::Trigger::Cron,
        &failures(&[(
            "repos",
            "https://bot:s3cret@git.example/o/r.git",
            "fatal: unable to access 'https://bot:s3cret@git.example/o/r.git/': 403",
        )]),
    );
    let body = std::fs::read_to_string(update_status::last_failure_path(state)).unwrap();
    assert!(!body.contains("s3cret") && !body.contains("bot:"), "{body}");
    let failure = update_status::read_last_failure(state).expect("record");
    assert_eq!(failure.items[0].name, "https://***@git.example/o/r.git");
    assert_eq!(
        failure.items[0].detail,
        "fatal: unable to access 'https://***@git.example/o/r.git/': 403"
    );
}

#[test]
fn last_failure_redacts_url_credentials_an_older_dot_wrote() {
    // A record written before redaction existed must not print its secret.
    let scratch = TempDir::new("update-status-failure-redact-old").unwrap();
    let state = scratch.path();
    let path = update_status::last_failure_path(state);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "1800000000 fail cron\nitem\tconfigs\t40-hook\tpush to https://u:tok@host/x failed\n",
    )
    .unwrap();
    let failure = update_status::read_last_failure(state).expect("record");
    assert_eq!(failure.items[0].detail, "push to https://***@host/x failed");
}

#[test]
fn last_line_redacts_before_it_caps() {
    // The cap must not cut between the secret and its `@`, which would leave
    // the secret with nothing marking it as userinfo.
    let line = format!("{}https://user:{}@host/x", "p".repeat(200), "t".repeat(60));
    let shown = update_status::last_line(line.as_bytes());
    assert!(!shown.contains("ttttt"), "{shown}");
}

#[test]
fn last_line_finds_the_final_message() {
    assert_eq!(update_status::last_line(b""), "");
    assert_eq!(update_status::last_line(b"\n \n"), "");
    assert_eq!(
        update_status::last_line(b"warning: first\nerror: the cause\n\n"),
        "error: the cause"
    );
    assert_eq!(
        update_status::last_line(b"progress\rerror: cr\r\n"),
        "error: cr"
    );
    // Only the tail is scanned, and the line is capped.
    let mut huge = vec![b'x'; 1 << 20];
    huge.extend_from_slice(b"\nerror: tail\n");
    assert_eq!(update_status::last_line(&huge), "error: tail");
    let long = "y".repeat(10_000);
    assert!(update_status::last_line(long.as_bytes()).len() <= 240);
}

#[test]
fn failed_run_records_every_outcome_file() {
    // A failure outside the engine leaves the same records the engine does.
    let scratch = TempDir::new("update-status-failed-run").unwrap();
    let state = scratch.path();
    update_status::record_failed_run(
        state,
        1_800_000_000,
        update_status::Trigger::Cron,
        (
            update_status::STAGE_UPDATE,
            "dot",
            "cannot start the updated dot",
        ),
    );
    let log = std::fs::read_to_string(update_status::update_log_path(state)).unwrap();
    assert_eq!(log, "1800000000 fail update\n");
    let last = update_status::read_last_run(state).expect("last run");
    let failure = update_status::read_last_failure(state).expect("cause");
    assert!(failure.describes(&last));
    assert_eq!(failure.items[0].detail, "cannot start the updated dot");

    // A hand run writes no cron line.
    let manual = TempDir::new("update-status-failed-run-manual").unwrap();
    update_status::record_failed_run(
        manual.path(),
        1_800_000_000,
        update_status::Trigger::Manual,
        (update_status::STAGE_UPDATE, "dot", "reason"),
    );
    assert!(!update_status::update_log_path(manual.path()).exists());
    assert_eq!(
        update_status::read_last_run(manual.path())
            .expect("last run")
            .trigger,
        "manual"
    );
}
