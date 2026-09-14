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
        update_status::read_last_success(&state)
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
    assert_eq!(stamp, None);
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
    let failed = state.join("scratch.log");
    std::fs::write(&failed, b"boom\n").unwrap();
    update_status::retain_failed_log(&logs, "cmd", 1_800_000_000, &failed).expect("retained log");

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path).expect("mode").permissions().mode() & 0o777
    };
    assert_eq!(mode(&dot), 0o700);
    assert_eq!(mode(&log), 0o600);
    assert_eq!(mode(&stamp), 0o600);
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
    assert_eq!(update_status::logs_dir(state), state.join("dot/logs"));
}
