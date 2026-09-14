//! Explicit native contracts for update-lock ownership and lifecycle.

use dot::errors::Error;
use dot::log::Log;
use dot::update_lock::{self, Owner};
use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

fn log() -> Log {
    Log::new(false, false)
}

#[test]
fn owner_format_and_parser_interoperate_with_the_stable_wire_contract() {
    let owner = Owner {
        pid: 1234,
        start: "proc:98765".into(),
        token: "opaque.token".into(),
    };
    let text = "pid\t1234\nstart\tproc:98765\ntoken\topaque.token\n";
    assert_eq!(update_lock::format_owner(&owner), text);
    assert_eq!(update_lock::parse_owner(text), Some(owner));
    for malformed in [
        "",
        "pid\t0\nstart\tx\ntoken\ty\n",
        "pid\t012\nstart\tx\ntoken\ty\n",
        "pid\tx\nstart\tx\ntoken\ty\n",
        "pid\t12\nstart\t\ntoken\ty\n",
        "pid\t12\nstart\tx\ntoken\t\n",
        "pid 12\nstart\tx\ntoken\ty\n",
    ] {
        assert_eq!(update_lock::parse_owner(malformed), None, "{malformed:?}");
    }
    let live = Owner {
        pid: std::process::id(),
        start: update_lock::process_start(std::process::id(), None).unwrap(),
        token: update_lock::mint_token(),
    };
    assert!(update_lock::owner_is_active(&live));
    assert!(!update_lock::owner_is_active(&Owner {
        start: "proc:0".into(),
        ..live
    }));
}

#[test]
fn acquire_release_round_trip_has_exact_owner_and_cleanup() {
    let state = TempDir::new("lock-roundtrip").unwrap();
    let mut warnings = Vec::new();
    let guard = update_lock::acquire(state.path(), false, &log(), None, &mut warnings).unwrap();
    assert!(warnings.is_empty());
    assert!(guard.is_current());
    let lock = guard.lock_dir().to_path_buf();
    let owner = update_lock::read_owner(&lock).unwrap();
    assert_eq!(owner.pid, std::process::id());
    assert_eq!(owner.token, guard.token());
    assert_eq!(
        std::fs::metadata(update_lock::owner_file(&lock))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(guard.release(&log(), &mut warnings));
    assert!(!lock.exists());
    assert!(warnings.is_empty());
}

#[test]
fn acquire_secures_shared_dot_state_directory() {
    let state = TempDir::new("lock-private-parent").unwrap();
    let dot = state.path().join(update_lock::DOT_DIR_NAME);
    std::fs::create_dir(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o777)).unwrap();
    let guard = update_lock::acquire(state.path(), false, &log(), None, &mut Vec::new()).unwrap();
    assert_eq!(
        std::fs::metadata(&dot).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(guard.lock_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    drop(guard);
}

#[test]
fn live_contention_is_busy_75_with_exact_warning_and_cron_silence() {
    let state = TempDir::new("lock-contention").unwrap();
    let guard = update_lock::acquire(state.path(), false, &log(), None, &mut Vec::new()).unwrap();
    let expected = format!(
        "  warning: dot update already running (pid {})",
        std::process::id()
    );
    let mut warnings = Vec::new();
    match update_lock::acquire(state.path(), false, &log(), None, &mut warnings) {
        Err(Error::LockBusy { message }) => assert_eq!(message, expected),
        other => panic!("expected busy, got {other:?}"),
    }
    assert_eq!(warnings, format!("{expected}\n").as_bytes());
    assert_eq!(update_lock::EXIT_LOCK_BUSY, 75);
    let mut cron = Vec::new();
    match update_lock::acquire(state.path(), true, &log(), None, &mut cron) {
        Err(Error::LockBusy { message }) => assert_eq!(message, expected),
        other => panic!("expected cron busy, got {other:?}"),
    }
    assert!(cron.is_empty());
    drop(guard);
}

#[test]
fn stale_owner_is_reclaimed_and_replaced() {
    let state = TempDir::new("lock-stale").unwrap();
    let lock = update_lock::lock_path(state.path());
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(
        update_lock::owner_file(&lock),
        "pid\t42424242\nstart\tproc:1\ntoken\tstale\n",
    )
    .unwrap();
    let guard = update_lock::acquire(state.path(), false, &log(), None, &mut Vec::new()).unwrap();
    assert!(guard.is_current());
    let owner = update_lock::read_owner(&lock).unwrap();
    assert_eq!(owner.pid, std::process::id());
    assert_ne!(owner.token, "stale");
    assert_eq!(
        std::fs::read_dir(&lock).unwrap().count(),
        1,
        "reclaim artifacts cleaned"
    );
    drop(guard);
}

#[test]
fn fresh_empty_lock_is_initializing_but_aged_empty_lock_is_reclaimed() {
    let fresh = TempDir::new("lock-fresh").unwrap();
    let fresh_lock = update_lock::lock_path(fresh.path());
    std::fs::create_dir_all(&fresh_lock).unwrap();
    assert!(update_lock::is_initializing(&fresh_lock));
    let mut warnings = Vec::new();
    match update_lock::acquire(fresh.path(), false, &log(), None, &mut warnings) {
        Err(Error::LockBusy { message }) => {
            assert_eq!(message, "  warning: dot update lock is initializing")
        }
        other => panic!("expected initializing, got {other:?}"),
    }
    assert_eq!(warnings, b"  warning: dot update lock is initializing\n");

    let aged = TempDir::new("lock-aged").unwrap();
    let aged_lock = update_lock::lock_path(aged.path());
    std::fs::create_dir_all(&aged_lock).unwrap();
    assert!(
        Command::new("touch")
            .args(["-t", "200001010000"])
            .arg(&aged_lock)
            .status()
            .unwrap()
            .success()
    );
    assert!(!update_lock::is_initializing(&aged_lock));
    let guard = update_lock::acquire(aged.path(), false, &log(), None, &mut Vec::new()).unwrap();
    assert!(guard.is_current());
    drop(guard);
}

#[test]
fn file_at_lock_path_is_hard_error_with_exact_warning_and_no_cleanup() {
    let state = TempDir::new("lock-file").unwrap();
    let lock = update_lock::lock_path(state.path());
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, b"not a directory").unwrap();
    let mut warnings = Vec::new();
    match update_lock::acquire(state.path(), false, &log(), None, &mut warnings) {
        Err(Error::Io { context, .. }) => {
            assert_eq!(context, "dot update lock path is not a directory")
        }
        other => panic!("expected hard error, got {other:?}"),
    }
    assert_eq!(
        warnings,
        format!(
            "  warning: dot update lock path is not a directory: {}\n",
            lock.display()
        )
        .as_bytes()
    );
    assert_eq!(std::fs::read(&lock).unwrap(), b"not a directory");
}

#[test]
fn paths_and_reentry_are_exact_and_release_is_owner_verified() {
    let state = TempDir::new("lock-path-reentry").unwrap();
    let lock = update_lock::lock_path(state.path());
    assert_eq!(lock, state.path().join("dot/update.lock.d"));
    assert_eq!(
        update_lock::owner_file(&lock),
        state.path().join("dot/update.lock.d/owner")
    );
    let guard = update_lock::acquire(state.path(), false, &log(), None, &mut Vec::new()).unwrap();
    assert!(update_lock::try_reenter(&lock, "").is_none());
    assert!(update_lock::try_reenter(&lock, "wrong").is_none());
    let reentered = update_lock::acquire(
        state.path(),
        false,
        &log(),
        Some(guard.token()),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(reentered.token(), guard.token());
    drop(reentered);
    assert!(!lock.exists());
    drop(guard);
}
