//! Native contracts for scratch logs, execution capture, ticks, and quiet failures.

use dot::log::Log;
use dot::run::{Live, logfile_create, logfile_print, run_quiet_logged, run_to_log_with_ticks};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

fn argv(script: &str) -> Vec<OsString> {
    ["sh", "-c", script]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn logfile_create_allocates_unique_empty_files() {
    let first = logfile_create().unwrap();
    let second = logfile_create().unwrap();
    assert_ne!(first, second);
    for path in [first, second] {
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        std::fs::remove_file(path).ok();
    }
}

#[test]
fn run_to_log_preserves_combined_streams_exit_code_and_empty_argv() {
    let path = logfile_create().unwrap();
    assert_eq!(
        run_to_log_with_ticks(&path, &argv("echo out; echo err >&2; exit 3"), None),
        3
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"out\nerr\n");
    std::fs::write(&path, b"stale\n").unwrap();
    assert_eq!(run_to_log_with_ticks(&path, &[], None), 0);
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    std::fs::remove_file(path).ok();
}

#[test]
fn live_execution_ticks_and_preserves_failure_status() {
    for (script, expected) in [("sleep 0.2", 0), ("sleep 0.2; exit 2", 2)] {
        let path = logfile_create().unwrap();
        let mut stage = dot::progress_ui::Stage::begin(
            dot::progress_ui::Palette::empty(),
            "0",
            false,
            true,
            false,
            true,
        );
        let mut ticks = Vec::new();
        let rc = run_to_log_with_ticks(
            &path,
            &argv(script),
            Some(Live {
                stage: &mut stage,
                out: &mut ticks,
                tick_seconds: 0.02,
            }),
        );
        assert_eq!(rc, expected);
        assert!(!ticks.is_empty());
        std::fs::remove_file(path).ok();
    }
}

#[test]
fn live_ticks_clamp_zero_and_negative_quanta() {
    // Fresh-review-B C2: a zero or negative tick quantum must not
    // busy-spin `recv_timeout(ZERO)` — heartbeats render at the
    // 0.1s minimum instead. Each heartbeat starts with `\r\x1b[K`,
    // so the escape count bounds the tick count (a 0.3s command
    // yields ~3, never thousands).
    for tick_seconds in [0.0, -2.0] {
        let path = logfile_create().unwrap();
        let mut stage = dot::progress_ui::Stage::begin(
            dot::progress_ui::Palette::empty(),
            "0",
            false,
            true,
            false,
            true,
        );
        let mut ticks = Vec::new();
        let rc = run_to_log_with_ticks(
            &path,
            &argv("sleep 0.3"),
            Some(Live {
                stage: &mut stage,
                out: &mut ticks,
                tick_seconds,
            }),
        );
        assert_eq!(rc, 0);
        // With an empty palette and empty label, the only ESC byte
        // in the output is each heartbeat's `\r\x1b[K` prefix.
        let heartbeats = ticks.iter().filter(|byte| **byte == 0x1b).count();
        assert!(
            heartbeats <= 8,
            "tick {tick_seconds} spun {heartbeats} heartbeats in 0.3s"
        );
        std::fs::remove_file(path).ok();
    }
}

#[test]
fn logfile_print_has_exact_indentation_and_silent_empty_cases() {
    let dir = dot_test_support::TempDir::new("run-print").unwrap();
    let log = Log::new(false, false);
    for (name, bytes, expected) in [
        (
            "full",
            b"first\nsecond\n".as_slice(),
            b"  label output:\n    first\n    second\n".as_slice(),
        ),
        (
            "partial",
            b"first\nsecond".as_slice(),
            b"  label output:\n    first\n    second".as_slice(),
        ),
        ("empty", b"".as_slice(), b"".as_slice()),
    ] {
        let path = fixture(dir.path(), name, bytes);
        let mut warnings = Vec::new();
        logfile_print(&log, &mut warnings, "label", &path);
        assert_eq!(warnings, expected);
    }
    let mut warnings = Vec::new();
    logfile_print(&log, &mut warnings, "label", &dir.path().join("missing"));
    assert!(warnings.is_empty());
}

#[test]
fn quiet_logged_returns_command_status_and_warns_with_failed_output() {
    // Handoff finding #6 changed the historical always-0 contract
    // (verified: `run_quiet_logged` has no production callers in
    // `src/` or `lib/`, so no caller can depend on silent success;
    // see `quiet_logged_has_no_production_callers`): the runner now
    // reports the real command status.
    let log = Log::new(false, false);
    let mut warnings = Vec::new();
    assert_eq!(
        run_quiet_logged(&log, &mut warnings, "pull", "failed", &argv("exit 0"), None),
        0
    );
    assert!(warnings.is_empty());
    assert_eq!(
        run_quiet_logged(
            &log,
            &mut warnings,
            "pull",
            "failed",
            &argv("echo boom; exit 4"),
            None,
        ),
        4
    );
    assert_eq!(warnings, b"  pull output:\n    boom\n  warning: failed\n");
}

#[test]
fn quiet_logged_retains_failed_logs_and_prunes_to_newest_twenty() {
    let dir = dot_test_support::TempDir::new("run-retain").unwrap();
    let logs = dir.path().join("logs");
    let log = Log::new(false, false);

    // Passing commands clean up: no retained log, silent warnings.
    let mut warnings = Vec::new();
    assert_eq!(
        run_quiet_logged(
            &log,
            &mut warnings,
            "pull",
            "failed",
            &argv("exit 0"),
            Some(&logs)
        ),
        0
    );
    assert!(warnings.is_empty());
    assert!(!logs.exists());

    // Failing commands retain their log and report the real status.
    let mut warnings = Vec::new();
    assert_eq!(
        run_quiet_logged(
            &log,
            &mut warnings,
            "pull",
            "failed",
            &argv("echo boom; exit 4"),
            Some(&logs),
        ),
        4
    );
    assert_eq!(warnings, b"  pull output:\n    boom\n  warning: failed\n");
    let retained: Vec<_> = std::fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(retained.len(), 1);
    assert_eq!(std::fs::read(&retained[0]).unwrap(), b"boom\n");

    // Rotation keeps the newest twenty: seed older logs, retain one
    // more, and the oldest fall off.
    let now = std::time::SystemTime::now();
    for index in 0..25 {
        let path = logs.join(format!("seed-{index:02}.log"));
        std::fs::write(&path, b"seed\n").unwrap();
        let mtime = now - std::time::Duration::from_secs(3600 + index as u64);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
    let mut warnings = Vec::new();
    assert_eq!(
        run_quiet_logged(
            &log,
            &mut warnings,
            "pull",
            "failed",
            &argv("exit 7"),
            Some(&logs)
        ),
        7
    );
    let mut names: Vec<_> = std::fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), dot::update_status::MAX_RETAINED_LOGS);
    assert!(
        names
            .iter()
            .all(|name| name != "seed-24.log" && name != "seed-23.log"),
        "oldest seeds pruned: {names:?}"
    );
}

#[test]
fn quiet_logged_has_no_production_callers() {
    // Caller-audit evidence for handoff finding #6: the status
    // change is safe only while no production code relies on the old
    // always-0 behavior. This test fails if a `src/` call site
    // appears outside the `run.rs` definition itself.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut callers = Vec::new();
    for entry in std::fs::read_dir(&root).expect("source directory") {
        let path = entry.expect("source entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        if path.file_name().is_some_and(|name| name == "run.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read production source");
        for (index, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("run_quiet_logged") {
                callers.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }
    assert!(callers.is_empty(), "new quiet-runner callers: {callers:?}");
}
