//! Differential parity tests for the update lock against
//! `lib/dot/update-lock.sh`: owner-format interop (each engine reads
//! the other's lock), contention exit codes (75 busy), stale reclaim,
//! initializing detection, cron-mode silence, and re-entry.
//!
//! Like `tests/cleanup.rs`, these compare EXIT CODES, streams, and
//! filesystem OUTCOMES — never internals (tokens are opaque).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use dot::log::Log;

/// Absolute bash (see `tests/ui.rs`).
/// Oracle interpreter, shared with the other differential harnesses (see
/// `dot::test_support::bash`).
fn bash_bin() -> &'static std::path::Path {
    dot::test_support::bash()
}

/// Shell lock prelude: sources the log module (owns `_warn`) then the
/// lock module (which pulls xdg and resources) inside a scrubbed env
/// with isolated XDG roots. Stdout is piped, so `log.sh` disables
/// color exactly like the Rust `Log::new(false, false)` side.
fn lock_script(state: &Path, body: &str) -> String {
    format!(
        "set -u\nXDG_STATE_HOME=\"{state}\"\nexport XDG_STATE_HOME\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/update-lock.sh\"\n{body}\n",
        state = state.display(),
    )
}

/// Run one shell lock snippet; returns (exit code, stdout, stderr).
fn shell_lock(state: &Path, body: &str) -> (i32, String, String) {
    let output = Command::new(bash_bin())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(lock_script(state, body))
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn test_log() -> Log {
    Log::new(false, false)
}

fn lock_dir_of(state: &Path) -> PathBuf {
    state
        .join(dot::update_lock::DOT_DIR_NAME)
        .join(dot::update_lock::LOCK_DIR_NAME)
}

/// Owner-format interop: each engine reads the other's live lock and
/// agrees it is active. Compared by OUTCOME (parsed pid, ACTIVE
/// verdict, busy contender code) — tokens stay opaque, and everything
/// is exchanged through the lock dir under the scratch state, never
/// through the checkout.
#[test]
fn shell_lock_is_readable_by_rust_and_vice_versa() {
    let scratch = dot::test_support::TempDir::new("lock-interop").expect("scratch");
    let log = test_log();

    // Shell holds (background sleeper); Rust reads the owner record.
    let shell_state = scratch.path().join("from-shell");
    std::fs::create_dir_all(shell_state.join("home")).expect("home");
    let mut holder = spawn_shell_holder(&shell_state);
    wait_for_lock(&shell_state);
    let owner =
        dot::update_lock::read_owner(&lock_dir_of(&shell_state)).expect("rust reads shell owner");
    assert_eq!(owner.pid, holder.id(), "rust parsed the wrong owner pid");
    assert!(
        dot::update_lock::owner_is_active(&owner),
        "live shell holder looks stale to rust"
    );
    // A Rust contender agrees the shell holder is live: busy outcome
    // with the pid warning through the warn sink (`_warn` parity).
    let mut warnings = Vec::new();
    match dot::update_lock::acquire(&shell_state, false, &log, None, &mut warnings) {
        Err(dot::errors::Error::LockBusy { message }) => {
            assert!(
                message.contains(&format!("(pid {})", owner.pid)),
                "wrong busy pid: {message:?}"
            );
            assert!(
                String::from_utf8_lossy(&warnings).contains(&format!("(pid {})", owner.pid)),
                "busy warning missing from sink: {warnings:?}"
            );
        }
        other => panic!("expected busy against shell holder, got {other:?}"),
    }
    kill_holder(&mut holder);

    // Rust holds; shell reads the owner record back.
    let rust_state = scratch.path().join("from-rust");
    std::fs::create_dir_all(rust_state.join("home")).expect("home");
    let mut rust_warnings = Vec::new();
    let guard = dot::update_lock::acquire(&rust_state, false, &log, None, &mut rust_warnings)
        .expect("rust acquires");
    assert!(
        rust_warnings.is_empty(),
        "fresh acquire warned: {rust_warnings:?}"
    );
    let me = std::process::id();
    let (code, stdout, stderr) = shell_lock(
        &rust_state,
        "_dot_update_lock_path\nlock=$REPLY\n_dot_update_lock_read_owner \"$lock\" || exit 1\necho \"pid=$DOT_UPDATE_LOCK_OWNER_PID\"\n_dot_update_lock_owner_is_active && echo ACTIVE || echo STALE\n_dot_update_lock_acquire\ncontend=$?\necho \"contend=$contend\"\n",
    );
    assert_eq!(code, 0, "shell read failed: {stderr:?}");
    assert!(
        stdout.contains(&format!("pid={me}")),
        "shell parsed the wrong owner pid: {stdout:?}"
    );
    assert!(
        stdout.contains("ACTIVE"),
        "live rust holder looks stale to shell: {stdout:?}"
    );
    assert!(
        stdout.contains("contend=75"),
        "shell contender did not see busy: {stdout:?}"
    );
    assert!(
        stderr.contains(&format!("(pid {me})")),
        "shell busy warning missing: {stderr:?}"
    );
    drop(guard);

    // After the Rust guard releases, the shell acquires the same path.
    let (code, _, stderr) = shell_lock(&rust_state, "_dot_update_lock_acquire");
    assert_eq!(
        code, 0,
        "shell failed to acquire after rust release: {stderr:?}"
    );
}

/// Acquire/release round trip on each side, compared by outcome:
/// exit code, owner-file shape, and directory lifecycle.
#[test]
fn acquire_release_round_trip() {
    let scratch = dot::test_support::TempDir::new("lock-roundtrip").expect("scratch");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(state.join("home")).expect("home");

    let (code, stdout, stderr) = shell_lock(
        &state,
        "_dot_update_lock_acquire\ncode=$?\n_dot_update_lock_path\nlock=$REPLY\n_dot_update_lock_read_owner \"$lock\"\necho \"owner=$DOT_UPDATE_LOCK_OWNER_PID\"\n_dot_update_lock_release\ntest -d \"$lock\" && echo PRESENT || echo GONE\nexit $code\n",
    );
    assert_eq!(code, 0, "shell acquire failed: {stderr:?}");
    assert!(
        stdout.contains("owner="),
        "shell never read its owner: {stdout:?}"
    );
    assert!(
        stdout.contains("GONE"),
        "shell left the lock behind: {stdout:?}"
    );

    let log = test_log();
    let mut warnings = Vec::new();
    let guard =
        dot::update_lock::acquire(&state, false, &log, None, &mut warnings).expect("rust acquire");
    assert!(warnings.is_empty(), "fresh acquire warned: {warnings:?}");
    assert!(guard.is_current());
    let dir = guard.lock_dir().to_path_buf();
    assert!(guard.release(&log, &mut Vec::new()));
    assert!(!dir.exists());
}

/// Live contention: a holder sleeps with the lock; a contender must
/// see exit 75 with the pid warning (both engines, both directions).
/// Cron mode keeps the same busy OUTCOME but stays silent on the warn
/// sink / stderr.
#[test]
fn live_contention_reports_busy_75() {
    let scratch = dot::test_support::TempDir::new("lock-contend").expect("scratch");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(state.join("home")).expect("home");

    // Rust holds; shell contends.
    let log = test_log();
    let mut hold_warnings = Vec::new();
    let guard = dot::update_lock::acquire(&state, false, &log, None, &mut hold_warnings)
        .expect("rust holds");
    assert!(
        hold_warnings.is_empty(),
        "fresh acquire warned: {hold_warnings:?}"
    );
    let (code, _, stderr) = shell_lock(&state, "_dot_update_lock_acquire");
    assert_eq!(code, 75, "shell contender saw {code}");
    assert!(
        stderr.contains("already running (pid "),
        "shell warning missing: {stderr:?}"
    );
    drop(guard);

    // Shell holds (background sleeper); Rust contends.
    let mut holder = spawn_shell_holder(&state);
    wait_for_lock(&state);
    let mut warnings = Vec::new();
    match dot::update_lock::acquire(&state, false, &log, None, &mut warnings) {
        Err(dot::errors::Error::LockBusy { message }) => {
            assert!(message.contains("already running (pid "), "{message:?}");
            assert!(
                String::from_utf8_lossy(&warnings).contains("already running (pid "),
                "busy warning missing from sink: {warnings:?}"
            );
        }
        other => panic!("expected busy, got {other:?}"),
    }
    // Cron mode: same busy outcome, silent warn sink.
    let mut cron_warnings = Vec::new();
    match dot::update_lock::acquire(&state, true, &log, None, &mut cron_warnings) {
        Err(dot::errors::Error::LockBusy { message }) => {
            assert!(message.contains("already running (pid "), "{message:?}");
            assert!(cron_warnings.is_empty(), "cron warned: {cron_warnings:?}");
        }
        other => panic!("expected cron-busy, got {other:?}"),
    }
    // Shell cron parity: exit 75, stderr silent.
    let (cron_code, _, cron_stderr) = shell_lock(&state, "_dot_update_lock_acquire --cron");
    assert_eq!(cron_code, 75, "shell cron contender saw {cron_code}");
    assert!(cron_stderr.is_empty(), "shell cron warned: {cron_stderr:?}");
    kill_holder(&mut holder);
}

/// Stale lock (dead owner pid) is reclaimed by the next acquirer.
#[test]
fn stale_lock_is_reclaimed() {
    let scratch = dot::test_support::TempDir::new("lock-stale").expect("scratch");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(state.join("home")).expect("home");

    // Shell-held lock, holder killed: Rust must reclaim it.
    let mut holder = spawn_shell_holder(&state);
    wait_for_lock(&state);
    kill_holder(&mut holder);
    let log = test_log();
    let mut warnings = Vec::new();
    let guard =
        dot::update_lock::acquire(&state, false, &log, None, &mut warnings).expect("rust reclaims");
    assert!(guard.is_current());
    drop(guard);

    // Rust-held lock, process gone (drop releases... so instead plant a
    // stale owner record with a dead pid directly): shell must reclaim.
    let dir = lock_dir_of(&state);
    std::fs::create_dir_all(&dir).expect("lock dir");
    std::fs::write(
        dir.join("owner"),
        "pid\t42424242\nstart\tproc:1\ntoken\tstale.0.0\n",
    )
    .expect("stale owner");
    let (code, _, stderr) = shell_lock(&state, "_dot_update_lock_acquire");
    assert_eq!(code, 0, "shell failed to reclaim: {stderr:?}");
}

/// Fresh ownerless lock dir reports initializing (75); an aged one is
/// reclaimed.
#[test]
fn initializing_vs_aged_empty_lock() {
    let scratch = dot::test_support::TempDir::new("lock-aging").expect("scratch");
    let log = test_log();

    let fresh_state = scratch.path().join("fresh");
    std::fs::create_dir_all(fresh_state.join("home")).expect("home");
    let fresh_dir = lock_dir_of(&fresh_state);
    std::fs::create_dir_all(&fresh_dir).expect("empty lock");
    let mut warnings = Vec::new();
    match dot::update_lock::acquire(&fresh_state, false, &log, None, &mut warnings) {
        Err(dot::errors::Error::LockBusy { message }) => {
            assert!(message.contains("initializing"), "{message:?}");
            assert!(
                String::from_utf8_lossy(&warnings).contains("initializing"),
                "initializing warning missing from sink: {warnings:?}"
            );
        }
        other => panic!("expected initializing-busy, got {other:?}"),
    }

    // Aged via POSIX `touch -t` (portable, unlike `-d`).
    let aged_state = scratch.path().join("aged");
    std::fs::create_dir_all(aged_state.join("home")).expect("home");
    let aged_dir = lock_dir_of(&aged_state);
    std::fs::create_dir_all(&aged_dir).expect("empty lock");
    let status = Command::new("touch")
        .arg("-t")
        .arg("200001010000")
        .arg(&aged_dir)
        .status()
        .expect("touch");
    assert!(status.success());
    let mut aged_warnings = Vec::new();
    let guard = dot::update_lock::acquire(&aged_state, false, &log, None, &mut aged_warnings)
        .expect("reclaim aged");
    assert!(guard.is_current());
    drop(guard);
}

/// A regular file at the lock path is a hard error (exit 1), not busy.
#[test]
fn file_at_lock_path_is_error() {
    let scratch = dot::test_support::TempDir::new("lock-filepath").expect("scratch");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(state.join("home")).expect("home");
    let dir = lock_dir_of(&state);
    std::fs::create_dir_all(dir.parent().expect("parent")).expect("parent");
    std::fs::write(&dir, b"not a dir").expect("plant file");

    let (code, _, _) = shell_lock(&state, "_dot_update_lock_acquire");
    assert_eq!(code, 1, "shell saw {code}");

    let log = test_log();
    let mut warnings = Vec::new();
    match dot::update_lock::acquire(&state, false, &log, None, &mut warnings) {
        Err(dot::errors::Error::LockBusy { .. }) => {
            panic!("a file is an error, not busy")
        }
        Err(_) => {}
        Ok(_) => panic!("acquired over a file"),
    }
}

/// Spawn `bash` that acquires the lock under `state` and sleeps while
/// holding it. The child inherits a ready-signal: it creates
/// `<state>/ready` after acquiring.
fn spawn_shell_holder(state: &Path) -> Child {
    let body = lock_script(
        state,
        &format!(
            "_dot_update_lock_acquire || exit $?\ntouch \"{ready}\"\nsleep 30\n",
            ready = state.join("ready").display(),
        ),
    );
    Command::new(bash_bin())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(body)
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", state.join("home"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn holder")
}

/// Wait (bounded) for the holder's ready file.
fn wait_for_lock(state: &Path) {
    let ready = state.join("ready");
    for _ in 0..200 {
        if ready.exists() {
            // The owner write lands before the ready touch (same shell,
            // sequential commands), so presence implies acquisition.
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("holder never acquired");
}

/// Kill a holder and reap it.
fn kill_holder(holder: &mut Child) {
    let _ = holder.kill();
    let _ = holder.wait();
}
