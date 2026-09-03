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
    // so their outputs must be identical here. If the shell cannot run
    // (no Bash 4+), skip rather than fail: shell parity is owned by
    // `bash tests/run`, not this pin.
    let shell = Command::new("bash")
        .arg("bin/dot")
        .arg("version")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();
    let Ok(shell) = shell else { return };
    if !shell.status.success() {
        return;
    }
    let rust = bin().arg("version").output().expect("run dot version");
    assert!(rust.status.success());
    assert_eq!(rust.stdout, shell.stdout);
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
        "dot: unknown command: frobnicate\n"
    );
}
