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
fn quiet_logged_is_silent_on_success_and_warns_with_failed_output() {
    let log = Log::new(false, false);
    let mut warnings = Vec::new();
    run_quiet_logged(&log, &mut warnings, "pull", "failed", &argv("exit 0"));
    assert!(warnings.is_empty());
    run_quiet_logged(
        &log,
        &mut warnings,
        "pull",
        "failed",
        &argv("echo boom; exit 4"),
    );
    assert_eq!(warnings, b"  pull output:\n    boom\n  warning: failed\n");
}
