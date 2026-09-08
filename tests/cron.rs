//! Differential parity tests for `dot::cron` against the live shell
//! (`lib/dot/commands.sh` `cron)` branch, driven through the real
//! `dot_command_dispatch`): the installed listing, the empty listing,
//! the failing run (partial stdout plus the fallback message), the
//! missing binary, suppressed stderr, and the live system crontab.
//!
//! Separate binary because the rows exec fixture `crontab` scripts:
//! each side resolves its own binary (the shell through a scrubbed
//! `PATH`, Rust through an absolute fixture path), so names
//! normalize before comparing.

use std::path::Path;
use std::process::{Command, Stdio};

use dot::cron::{NO_CRONTAB_MESSAGE, cron};
use dot::test_support::TempDir;

/// Sources for the cron chapter: only the dispatcher. The `cron`
/// branch calls no other `dot` function, so the oracle needs no
/// runtime preamble beyond the file under test.
const SOURCES: &str = ". \"$1/lib/dot/commands.sh\"\n";

/// Run the real dispatcher `cron` branch with the cron runtime
/// sourced. The locale stays pinned: `crontab` output must read
/// English on both engines. `PATH` is fully caller-controlled so
/// the missing-binary row can hide `crontab` without touching the
/// parent process environment.
fn shell_run(home: &Path, path: &str, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
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

/// Run one row on twin sides and compare the `rc=` trailer, stdout
/// bytes, and harness stderr. The shell resolves `crontab` through
/// `PATH=path`; Rust execs `program` directly (the resolved binary
/// in the installed rows, a dangling path in the missing row).
fn check_row(dir: &TempDir, program: &str, path: &str) {
    let home = dir.path();
    let snippet = "dot_command_dispatch cron\nprintf 'rc=%s\\n' \"$?\"\n";
    let (code, out, err) = shell_run(home, path, snippet);
    assert_eq!(code, 0, "harness exit");
    let shell_out = String::from_utf8(out).expect("shell dump");
    assert!(err.is_empty(), "shell harness stderr: {err:?}");

    let mut stdout = Vec::new();
    let rc = cron(program, &mut stdout);
    assert_eq!(rc, 0, "cron always succeeds like the dispatcher");
    stdout.extend_from_slice(format!("rc={rc}\n").as_bytes());
    let rust_out = String::from_utf8(stdout).expect("rust dump");
    assert_eq!(rust_out, shell_out, "cron bytes");
}

#[test]
fn cron_lists_installed_entries_verbatim() {
    let body = "#!/bin/sh\nprintf '0 5 * * * dot update\\n30 6 * * 1 dot pull\\n'\n";
    let dir = fixture_bin("cron-list", body);
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let path = dir.path().to_string_lossy().into_owned();
    check_row(&dir, &program, &path);
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    assert_eq!(
        stdout, b"0 5 * * * dot update\n30 6 * * 1 dot pull\n",
        "listing passes through byte-identical",
    );
}

#[test]
fn cron_empty_listing_prints_nothing() {
    let dir = fixture_bin("cron-empty", "#!/bin/sh\nexit 0\n");
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let path = dir.path().to_string_lossy().into_owned();
    check_row(&dir, &program, &path);
}

#[test]
fn cron_failure_keeps_partial_stdout_then_message() {
    let body = "#!/bin/sh\nprintf 'partial-line\\n'\nprintf 'oops-stderr\\n' >&2\nexit 3\n";
    let dir = fixture_bin("cron-fail", body);
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let path = dir.path().to_string_lossy().into_owned();
    check_row(&dir, &program, &path);
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    let mut want = b"partial-line\n".to_vec();
    want.extend_from_slice(NO_CRONTAB_MESSAGE.as_bytes());
    assert_eq!(stdout, want, "partial bytes precede the fallback");
}

#[test]
fn cron_missing_binary_prints_message() {
    let dir = TempDir::new_exec("cron-missing").expect("fixture dir");
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let path = dir.path().to_string_lossy().into_owned();
    check_row(&dir, &program, &path);
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
    let body = "#!/bin/sh\nprintf '0 5 * * * dot update\\n'\nprintf 'noise-stderr\\n' >&2\n";
    let dir = fixture_bin("cron-stderr", body);
    let program = dir.path().join("crontab").to_string_lossy().into_owned();
    let path = dir.path().to_string_lossy().into_owned();
    check_row(&dir, &program, &path);
    let mut stdout = Vec::new();
    assert_eq!(cron(&program, &mut stdout), 0);
    assert_eq!(
        stdout, b"0 5 * * * dot update\n",
        "stderr never reaches stdout",
    );
}

#[test]
fn cron_matches_live_system_crontab() {
    // Whatever this machine has installed (or not), both engines
    // read the same system state through the same `PATH` lookup, so
    // the bytes must agree without fixtures.
    let dir = TempDir::new("cron-live").expect("fixture dir");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = path.to_string_lossy().into_owned();
    check_row(&dir, "crontab", &path);
}
