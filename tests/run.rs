//! Differential parity tests for `src/run.rs` against the live
//! shell (`lib/dot/run.sh`): scratch log allocation, the
//! tick-while-running executor, and the quiet logged runner.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::run::{Live, logfile_create, logfile_print, run_quiet_logged, run_to_log_with_ticks};
use dot::test_support::TempDir;

/// Sources for the run cluster.
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/progress-ui.sh\"\n",
    ". \"$1/lib/dot/run.sh\"\n",
);

/// Run one shell snippet with the run libraries sourced.
fn shell_run(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Non-tty logger matching the harness pipes.
fn test_log() -> Log {
    Log::new(false, false)
}

/// `sh -c script` argv for the Rust side.
fn sh_argv(script: &str) -> Vec<std::ffi::OsString> {
    ["sh", "-c", script]
        .iter()
        .map(std::ffi::OsString::from)
        .collect()
}

#[test]
fn logfile_create_makes_empty_file() {
    let first = logfile_create().expect("first log");
    let second = logfile_create().expect("second log");
    assert_ne!(first, second, "allocations are unique");
    for path in [&first, &second] {
        assert!(path.is_file(), "log exists: {}", path.display());
        assert_eq!(std::fs::metadata(path).expect("meta").len(), 0, "log empty");
        std::fs::remove_file(path).ok();
    }
    // The shell publishes a usable REPLY path too.
    let dir = TempDir::new("run-logfile").expect("fixture dir");
    let (status, out, err) = shell_run(
        dir.path(),
        &[],
        &[],
        "_logfile_create || echo FAILED\ntest -f \"$REPLY\" && echo usable\n",
    );
    assert_eq!(status, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    assert_eq!(out, b"usable\n", "shell log usable");
}

#[test]
fn run_to_log_captures_streams_and_rc() {
    let dir = TempDir::new("run-capture").expect("fixture dir");
    let script = "echo out; echo err >&2; exit 3";
    let snippet = format!(
        "_logfile_create || exit 99\nlog=\"$REPLY\"\n_run_to_log_with_ticks \"$log\" sh -c \"{script}\"; echo \"rc=$?\"\ncat \"$log\"\n"
    );
    let (status, out, err) = shell_run(dir.path(), &[], &[], &snippet);
    assert_eq!(status, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    let shell_text = String::from_utf8(out).expect("utf8");
    assert!(shell_text.starts_with("rc=3\n"), "shell rc: {shell_text:?}");
    let shell_log = shell_text["rc=3\n".len()..].to_string();
    assert_eq!(shell_log, "out\nerr\n", "shell log content");
    let rust_log = logfile_create().expect("rust log");
    let rc = run_to_log_with_ticks(&rust_log, &sh_argv(script), None);
    assert_eq!(rc, 3, "rust rc");
    let rust_content = std::fs::read_to_string(&rust_log).expect("read log");
    assert_eq!(rust_content, shell_log, "log content parity");
    std::fs::remove_file(&rust_log).ok();
    // Empty argv runs redirections only: success with a truncated log.
    let snippet = "_logfile_create || exit 99\nlog=\"$REPLY\"\necho stale >\"$log\"\n_run_to_log_with_ticks \"$log\"; echo \"rc=$?\"\ncat \"$log\"\n";
    let (status, out, err) = shell_run(dir.path(), &[], &[], snippet);
    assert_eq!(status, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    assert_eq!(out, b"rc=0\n", "shell empty argv");
    let rust_log = logfile_create().expect("rust log");
    std::fs::write(&rust_log, b"stale\n").expect("stale content");
    assert_eq!(
        run_to_log_with_ticks(&rust_log, &[], None),
        0,
        "rust empty argv"
    );
    assert_eq!(
        std::fs::read(&rust_log).expect("read log"),
        b"",
        "log truncated"
    );
    std::fs::remove_file(&rust_log).ok();
}

#[test]
fn run_to_log_ticks_while_live() {
    let dir = TempDir::new("run-live").expect("fixture dir");
    let live_env: [(&str, Option<&str>); 4] = [
        ("DOT_UI_FORCE_LIVE", Some("1")),
        ("DOT_UI_TICK_SECONDS", Some("0.05")),
        ("DOT_UI_STAGE_LABEL", Some("pull")),
        ("DOT_QUIET", None),
    ];
    let snippet = "_logfile_create || exit 99\nlog=\"$REPLY\"\n_run_to_log_with_ticks \"$log\" sleep 0.5; echo \"rc=$?\"\ncat \"$log\"\n";
    let (status, out, _) = shell_run(dir.path(), &[], &live_env, snippet);
    assert_eq!(status, 0, "harness exit");
    let shell_text = String::from_utf8(out).expect("utf8");
    // Ticks precede the marker on stdout (live lines end with \r,
    // not \n); the sleep log is empty.
    let marker = shell_text.rfind("rc=0\n").expect("shell rc marker");
    assert!(marker > 0, "shell ticks while live: {shell_text:?}");
    assert_eq!(&shell_text[marker..], "rc=0\n", "shell rc and empty log");
    let rust_log = logfile_create().expect("rust log");
    let palette = dot::progress_ui::Palette::empty();
    let mut stage = dot::progress_ui::Stage::begin(palette, "0", false, true, false, true);
    let mut ticks = Vec::new();
    let rc = run_to_log_with_ticks(
        &rust_log,
        &sh_argv("sleep 0.5"),
        Some(Live {
            stage: &mut stage,
            out: &mut ticks,
            tick_seconds: 0.05,
        }),
    );
    assert_eq!(rc, 0, "rust rc");
    assert!(!ticks.is_empty(), "rust ticks while live");
    std::fs::remove_file(&rust_log).ok();
    // A live failure still reports its exit code on both sides.
    let snippet = "_logfile_create || exit 99\nlog=\"$REPLY\"\n_run_to_log_with_ticks \"$log\" sh -c 'sleep 0.3; exit 2'; echo \"rc=$?\"\n";
    let (status, out, _) = shell_run(dir.path(), &[], &live_env, snippet);
    assert_eq!(status, 0, "harness exit");
    let shell_text = String::from_utf8(out).expect("utf8");
    let marker = shell_text.rfind("rc=2\n").expect("shell rc marker");
    assert!(marker > 0, "shell ticks on failure: {shell_text:?}");
    let rust_log = logfile_create().expect("rust log");
    let palette = dot::progress_ui::Palette::empty();
    let mut stage = dot::progress_ui::Stage::begin(palette, "0", false, true, false, true);
    let mut ticks = Vec::new();
    let rc = run_to_log_with_ticks(
        &rust_log,
        &sh_argv("sleep 0.3; exit 2"),
        Some(Live {
            stage: &mut stage,
            out: &mut ticks,
            tick_seconds: 0.05,
        }),
    );
    assert_eq!(rc, 2, "rust live failure rc");
    assert!(!ticks.is_empty(), "rust ticks on failure");
    std::fs::remove_file(&rust_log).ok();
}

#[test]
fn logfile_print_matches_shell() {
    let dir = TempDir::new("run-print").expect("fixture dir");
    let home = dir.path();
    let content = fixture_file(home, "content.log", b"first\nsecond\n");
    let partial = fixture_file(home, "partial.log", b"first\nsecond");
    let empty = fixture_file(home, "empty.log", b"");
    let missing = home.join("missing.log");
    for (path, want_output) in [
        (content.clone(), true),
        (partial.clone(), true),
        (empty.clone(), false),
        (missing.clone(), false),
    ] {
        let path_text = path.to_string_lossy().into_owned();
        let snippet = "_logfile_print label \"$2\"\n".to_string();
        let path_os = std::ffi::OsString::from(&path_text);
        let (status, _, shell_err) = shell_run(home, &[&path_os], &[], &snippet);
        assert_eq!(status, 0, "harness exit");
        let mut warnings = Vec::new();
        logfile_print(&test_log(), &mut warnings, "label", &path);
        assert_eq!(warnings, shell_err, "print parity for {}", path.display());
        assert_eq!(
            !warnings.is_empty(),
            want_output,
            "output for {}",
            path.display()
        );
    }
}

/// Write `bytes` to `dir/name`.
fn fixture_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

#[test]
fn run_quiet_logged_matches_shell() {
    let dir = TempDir::new("run-quiet").expect("fixture dir");
    // Success stays silent and removes the log.
    let snippet = "_run_quiet_logged pull failed sh -c 'exit 0'; echo \"rc=$?\"\n";
    let (status, out, err) = shell_run(dir.path(), &[], &[], snippet);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"rc=0\n", "shell success rc");
    assert!(err.is_empty(), "shell silent on success");
    // Failure warns with the labeled log and still reports zero.
    let snippet = "_run_quiet_logged pull failed sh -c 'echo boom; exit 4'; echo \"rc=$?\"\n";
    let (status, out, shell_err) = shell_run(dir.path(), &[], &[], snippet);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"rc=0\n", "shell failure rc");
    assert!(!shell_err.is_empty(), "shell warns on failure");
    let mut warnings = Vec::new();
    run_quiet_logged(
        &test_log(),
        &mut warnings,
        "pull",
        "failed",
        &sh_argv("echo boom; exit 4"),
    );
    assert_eq!(warnings, shell_err, "warning parity");
}
