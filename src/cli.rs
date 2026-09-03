//! Command dispatch for the `dot` CLI (slice 1).
//!
//! Hand-rolled parsing over `std::env::args_os`, no CLI framework: the
//! shell dispatcher matches on `${1:-help}` with a small fixed command
//! set, and startup latency is a first-class budget (see
//! `tests/perf_budget.rs`). Streams are injected so parity tests capture
//! output without subprocesses.
//!
//! Slice 1 owns `help` and `version` only. Every other command is a
//! not-yet-ported error until its slice lands; the shell `bin/dot`
//! remains the entry point and never routes here yet.

use std::ffi::OsString;
use std::io::Write;

use crate::version;

/// Exact bytes of the shell `dot_help` heredoc, including trailing newline.
///
/// Help text is user-visible contract: wrappers, docs, and the shell
/// test suite quote it, so the Rust port must emit it byte-for-byte
/// rather than "improving" alignment or wording. One literal per line
/// via `concat!`: a `\`-continued literal would strip the two-space
/// command indent (continuations eat leading whitespace), which is
/// exactly the kind of silent drift this constant exists to prevent.
/// Pinned by `tests/cli.rs` against the shell source: any drift in
/// `lib/dot/main.sh` must update this constant in the same commit.
pub const HELP: &str = concat!(
    "usage: dot <command> [<args>]\n",
    "\n",
    "Commands:\n",
    "  update           Converge the base repository, overlays, hooks, and provider\n",
    "  pull             Alias for update\n",
    "  fetch            Fetch the base repository and active Git overlays\n",
    "  push             Push the base repository and active Git overlays\n",
    "  status           Show base and overlay status\n",
    "  diff             Show base and overlay differences\n",
    "  cron             Show the installed user crontab\n",
    "  doctor           Run core and configured extension health checks\n",
    "  test             Run configured tests; provider suite is opt-in\n",
    "  init             Initialize or resume a client dotfiles repository\n",
    "  help             Show this command summary\n",
    "\n",
    "Run `dot init --help` for initialization and recovery syntax.\n",
);

/// Shell exit-code contract (`lib/dot/commands.sh`, `lib/dot/main.sh`):
/// `0` success, `1` error/unknown command, `2` usage, `75` lock busy.
/// Numeric codes cross the process boundary into scripts and CI gates,
/// so they are named constants — never inline literals — and new codes
/// arrive only with their owning slice (the lock's `75` is not defined
/// until the lock module lands).
pub const EXIT_SUCCESS: i32 = 0;
/// Generic failure: unknown command today; ports of `fetch`/`push`/
/// `status`/`diff`/`doctor`/`init` repo failures reuse it per the shell
/// `return 1` paths. Kept distinct from `EXIT_SUCCESS` even though no
/// slice-1 caller needs the name yet, so later slices cannot silently
/// invent a second "error" value.
pub const EXIT_ERROR: i32 = 1;

/// Run the CLI writing to the given streams; returns the exit code.
///
/// Streams are parameters — not captured globals — so parity tests feed
/// `Vec<u8>` buffers and assert exact bytes without spawning subprocesses
/// (subprocess-per-assertion would make the suite slow and flaky under
/// load, and would hide the byte contract behind shell quoting).
/// The first argument is the command (`${1:-help}` in shell terms);
/// remaining arguments are accepted and ignored by `help`/`version`,
/// matching the shell dispatcher, which shifts once and never inspects
/// `$@` for these commands. Later slices give each command its own
/// parser; nothing here may grow flags implicitly.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let mut args = args.into_iter();
    let command = args.next().unwrap_or_default();
    // Lossy conversion mirrors the shell, which matches on raw bytes
    // without validating encoding: an undecodable argv element can never
    // equal a command name, so it falls through to "unknown command"
    // instead of aborting. `to_string_lossy` produces exactly that
    // fall-through (replacement chars never match ASCII arms).
    let command = command.to_string_lossy();
    // Shell matches `${1:-help}`: empty or missing command shows help.
    // The empty-string arm matters because `run([])` (no argv at all,
    // as in tests) must behave like bare `dot`, not like an error.
    match command.as_ref() {
        "" | "help" | "-h" | "--help" => {
            let _ = stdout.write_all(HELP.as_bytes());
            EXIT_SUCCESS
        }
        "version" | "--version" => {
            let _ = writeln!(stdout, "{}", version::version_line());
            EXIT_SUCCESS
        }
        _ => {
            let _ = writeln!(stderr, "dot: unknown command: {command}");
            EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_text(args: &[&str]) -> (i32, String, String) {
        let owned: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(owned, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("stdout is UTF-8"),
            String::from_utf8(err).expect("stderr is UTF-8"),
        )
    }

    #[test]
    fn no_command_prints_help_successfully() {
        let (code, out, err) = run_text(&[]);
        assert_eq!(code, EXIT_SUCCESS);
        assert_eq!(out, HELP);
        assert!(err.is_empty());
    }

    #[test]
    fn help_flags_print_help_successfully() {
        for flag in ["help", "-h", "--help"] {
            let (code, out, err) = run_text(&[flag]);
            assert_eq!(code, EXIT_SUCCESS, "flag: {flag}");
            assert_eq!(out, HELP, "flag: {flag}");
            assert!(err.is_empty(), "flag: {flag}");
        }
    }

    #[test]
    fn help_ignores_trailing_args_like_shell() {
        let (code, out, _) = run_text(&["help", "update", "--verbose"]);
        assert_eq!(code, EXIT_SUCCESS);
        assert_eq!(out, HELP);
    }

    #[test]
    fn version_prints_single_line_to_stdout() {
        let (code, out, err) = run_text(&["version"]);
        assert_eq!(code, EXIT_SUCCESS);
        assert_eq!(out, format!("{}\n", version::version_line()));
        assert!(err.is_empty());
    }

    #[test]
    fn unknown_command_fails_on_stderr() {
        let (code, out, err) = run_text(&["frobnicate"]);
        assert_eq!(code, EXIT_ERROR);
        assert!(out.is_empty());
        assert_eq!(err, "dot: unknown command: frobnicate\n");
    }
}
