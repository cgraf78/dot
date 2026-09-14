//! Native behavioral tests for cron listing.

use dot::cron::{NO_CRONTAB_MESSAGE, cron};
use dot_test_support::TempDir;

static EXEC_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exec_test() -> std::sync::MutexGuard<'static, ()> {
    EXEC_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write `body` as an executable `crontab` fixture inside an
/// exec-capable directory and return the guard. The target dir is
/// exec-capable by construction (see [`TempDir::new_exec`]); the
/// caller chmods explicitly because the harness must exec the byte
/// it just wrote.
fn fixture_bin(tag: &str, body: &str) -> TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TempDir::new_exec(tag).expect("fixture dir");
    dir.write("crontab", body.as_bytes());
    std::fs::set_permissions(
        dir.path().join("crontab"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod fixture");
    dir
}

#[test]
fn cron_lists_installed_entries_verbatim() {
    let _guard = exec_test();
    let body = "#!/bin/sh\nprintf '0 5 * * * dot update\\n30 6 * * 1 dot pull\\n'\n";
    let dir = fixture_bin("cron-list", body);
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    assert_eq!(
        stdout, b"0 5 * * * dot update\n30 6 * * 1 dot pull\n",
        "listing passes through byte-identical",
    );
}

#[test]
fn cron_empty_listing_prints_nothing() {
    let _guard = exec_test();
    let dir = fixture_bin("cron-empty", "#!/bin/sh\nexit 0\n");
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    assert!(stdout.is_empty());
}

#[test]
fn cron_failure_keeps_partial_stdout_then_message() {
    let _guard = exec_test();
    let body = "#!/bin/sh\nprintf 'partial-line\\n'\nprintf 'oops-stderr\\n' >&2\nexit 3\n";
    let dir = fixture_bin("cron-fail", body);
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    let mut want = b"partial-line\n".to_vec();
    want.extend_from_slice(NO_CRONTAB_MESSAGE.as_bytes());
    assert_eq!(stdout, want, "partial bytes precede the fallback");
}

#[test]
fn cron_missing_binary_prints_message() {
    let _guard = exec_test();
    let dir = TempDir::new_exec("cron-missing").expect("fixture dir");
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    assert_eq!(
        stdout,
        NO_CRONTAB_MESSAGE.as_bytes(),
        "unstartable binary falls back",
    );
}

#[test]
fn cron_suppresses_crontab_stderr() {
    let _guard = exec_test();
    let body = "#!/bin/sh\nprintf '0 5 * * * dot update\\n'\nprintf 'noise-stderr\\n' >&2\n";
    let dir = fixture_bin("cron-stderr", body);
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    assert_eq!(
        stdout, b"0 5 * * * dot update\n",
        "stderr never reaches stdout",
    );
}
