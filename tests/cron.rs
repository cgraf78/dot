//! End-to-end contracts for `dot cron`: list the user crontab through the
//! `crontab` on PATH, falling back to one fixed line when there is none.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use dot_test_support::TempDir;

const NO_CRONTAB: &[u8] = b"  no crontab installed\n";

/// Run `dot cron` with only `bin` on PATH and a scratch HOME.
fn dot_cron(scratch: &TempDir, bin: &Path) -> Output {
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    Command::new(env!("CARGO_BIN_EXE_dot"))
        .arg("cron")
        .env_clear()
        .env("HOME", &home)
        .env("PATH", bin)
        .current_dir(&home)
        .stdin(Stdio::null())
        .output()
        .expect("run dot cron")
}

/// A scratch directory whose `bin/crontab` runs `body`.
fn fixture(tag: &str, body: &str) -> (TempDir, std::path::PathBuf) {
    let scratch = TempDir::new_exec(tag).expect("fixture dir");
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).expect("bin");
    dot_test_support::install_fixture_executable(bin.join("crontab"), body, 0o755)
        .expect("crontab fixture");
    (scratch, bin)
}

#[test]
fn cron_lists_installed_entries_verbatim() {
    let (scratch, bin) = fixture(
        "cron-list",
        "#!/bin/sh\n[ \"$1\" = -l ] || exit 9\nprintf '0 5 * * * dot update\\n30 6 * * 1 dot pull\\n'\n",
    );
    let output = dot_cron(&scratch, &bin);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        output.stdout,
        b"0 5 * * * dot update\n30 6 * * 1 dot pull\n"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn cron_empty_listing_prints_nothing() {
    let (scratch, bin) = fixture("cron-empty", "#!/bin/sh\nexit 0\n");
    let output = dot_cron(&scratch, &bin);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn cron_failure_prints_only_the_fallback_line() {
    let (scratch, bin) = fixture(
        "cron-fail",
        "#!/bin/sh\nprintf 'partial-line\\n'\nprintf 'oops-stderr\\n' >&2\nexit 3\n",
    );
    let output = dot_cron(&scratch, &bin);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, NO_CRONTAB);
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn cron_missing_binary_prints_the_fallback_line() {
    let scratch = TempDir::new_exec("cron-missing").expect("fixture dir");
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).expect("bin");
    let output = dot_cron(&scratch, &bin);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, NO_CRONTAB);
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn cron_suppresses_crontab_stderr() {
    let (scratch, bin) = fixture(
        "cron-stderr",
        "#!/bin/sh\nprintf '0 5 * * * dot update\\n'\nprintf 'noise-stderr\\n' >&2\n",
    );
    let output = dot_cron(&scratch, &bin);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"0 5 * * * dot update\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}
