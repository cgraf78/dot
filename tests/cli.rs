//! CLI parity tests: the Rust binary must behave like the shell dispatcher.
//!
//! `HELP` is pinned against the shell `dot_help` heredoc at compile time
//! via `include_str!`, so any drift in `lib/dot/main.sh` fails this suite
//! until the Rust constant is updated in the same commit.

use std::process::Command;

/// Extract the `dot_help` heredoc body from the shell source.
fn shell_help() -> String {
    let source = include_str!("../lib/dot/main.sh");
    let marker = "cat <<'EOF'\n";
    let start = source.find(marker).expect("dot_help heredoc marker") + marker.len();
    let rest = &source[start..];
    let end = rest.find("\nEOF\n").expect("dot_help heredoc terminator");
    format!("{}\n", &rest[..end])
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dot"))
}

#[test]
fn help_constant_matches_shell_heredoc_byte_for_byte() {
    assert_eq!(dot::cli::HELP, shell_help());
}

#[test]
fn binary_help_matches_shell_help() {
    let output = bin().arg("help").output().expect("run dot help");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        shell_help()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn binary_default_command_is_help() {
    let output = bin().output().expect("run dot");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        shell_help()
    );
}

#[test]
fn binary_version_shape() {
    for flag in ["version", "--version"] {
        let output = bin().arg(flag).output().expect("run dot version");
        assert!(output.status.success(), "flag: {flag}");
        let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
        assert!(stdout.starts_with("dot commit "), "flag {flag}: {stdout}");
        assert!(
            stdout.ends_with(" (config 1; extensions 1; library 1)\n"),
            "flag {flag}: {stdout}"
        );
        assert!(output.stderr.is_empty(), "flag: {flag}");
    }
}

#[test]
fn binary_version_agrees_with_shell_in_same_checkout() {
    // Both implementations resolve the revision from the same checkout,
    // so their outputs must be identical here. Skips are LOUD (stderr):
    // a silent pass would hide a broken shell path or a stale baked
    // revision. Shell parity itself is owned by `bash tests/run`.
    // Known race: a commit landing between compile time (baked SHA) and
    // this run fails despite both sides being correct; likewise an
    // explicit DOT_BUILD_COMMIT/GITHUB_SHA stamping intentionally
    // disagrees with run-time `git rev-parse HEAD`.
    let shell = Command::new("bash")
        .arg("bin/dot")
        .arg("version")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();
    let Ok(shell) = shell else {
        eprintln!("SKIP: cannot spawn bash for shell agreement check");
        return;
    };
    if !shell.status.success() {
        eprintln!("SKIP: shell `dot version` failed; shell parity is owned by tests/run");
        return;
    }
    let rust = bin().arg("version").output().expect("run dot version");
    assert!(rust.status.success());
    assert_eq!(rust.stdout, shell.stdout);
}

/// The exact `printf` format in the shell dispatcher. Unlike HELP (a
/// heredoc with stable boundaries), this is one line inside a function,
/// so the pin asserts the shell still contains the literal rather than
/// re-extracting it: a wording drift in `commands.sh` must fail here.
fn shell_unknown_command_format() -> &'static str {
    let source = include_str!("../lib/dot/commands.sh");
    assert!(
        source.contains("printf 'dot: unknown command: %s\\n'"),
        "shell dispatcher changed its unknown-command wording"
    );
    "dot: unknown command: frobnicate\n"
}

#[test]
fn binary_unknown_command_fails_like_shell() {
    let output = bin()
        .arg("frobnicate")
        .output()
        .expect("run dot frobnicate");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("stderr UTF-8"),
        shell_unknown_command_format()
    );
}

#[test]
fn binary_help_flags_match_shell() {
    let expected = shell_help();
    for flag in ["-h", "--help"] {
        let output = bin().arg(flag).output().expect("run dot flag");
        assert!(output.status.success(), "flag: {flag}");
        assert_eq!(
            String::from_utf8(output.stdout).expect("stdout UTF-8"),
            expected,
            "flag: {flag}"
        );
        assert!(output.stderr.is_empty(), "flag: {flag}");
    }
}
