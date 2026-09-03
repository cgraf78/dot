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

/// Native argv bytes for command matching.
///
/// On Unix this is the exact argv encoding: command names are pure
/// ASCII, so non-UTF8 input falls through to "unknown command" and the
/// diagnostic echoes the original bytes (not U+FFFD replacements,
/// which would break stderr byte parity with the shell). Elsewhere the
/// platform has no byte argv to be faithful to, so lossy text is the
/// only available behavior.
#[cfg(unix)]
fn argv_bytes(arg: &OsString) -> Vec<u8> {
    // `OsStr::as_encoded_bytes` is inherent (no trait import needed):
    // the exact argv bytes, no UTF-8 validation involved.
    arg.as_os_str().as_encoded_bytes().to_vec()
}

/// Fallback where argv has no byte form to preserve (see above).
#[cfg(not(unix))]
fn argv_bytes(arg: &OsString) -> Vec<u8> {
    arg.to_string_lossy().into_owned().into_bytes()
}

/// Exact bytes of the shell `dot_help` heredoc, including trailing newline.
///
/// Exact bytes of the shell `dot_help` heredoc. One literal per line:
/// a `\`-continued literal would strip the two-space command indent.
/// Pinned by `tests/cli.rs` against `lib/dot/main.sh`.
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
/// Generic failure (unknown command today; repo-failure paths reuse it
/// per the shell `return 1` sites). Named so later slices share one value.
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
    let command = argv_bytes(&command);
    let command = command.as_slice();
    // Shell matches `${1:-help}`: empty or missing command shows help.
    // The empty-string arm matters because `run([])` (no argv at all,
    // as in tests) must behave like bare `dot`, not like an error.
    // NOTE: the shell runs `dot_config_load || exit 2` BEFORE dispatch
    // (main.sh), so with an unloadable config even `frobnicate` exits 2.
    // This binary does not load config yet (slice 2); the exit-2 path is
    // specified in the forward contracts and tested when config lands.
    let mut failed = false;
    let code = match command {
        b"" | b"help" | b"-h" | b"--help" => {
            if stdout.write_all(HELP.as_bytes()).is_err() {
                failed = true;
            }
            EXIT_SUCCESS
        }
        b"version" | b"--version" => {
            if writeln!(stdout, "{}", version::version_line()).is_err() {
                failed = true;
            }
            EXIT_SUCCESS
        }
        _ => {
            // A closed stderr here leaves nothing to report to; the exit
            // code still carries the failure.
            let _ = stderr.write_all(b"dot: unknown command: ");
            let _ = stderr.write_all(command);
            let _ = stderr.write_all(b"\n");
            EXIT_ERROR
        }
    };
    // A closed pipe must not report success for undelivered output.
    // (The shell dies on SIGPIPE; Rust reports failure via exit code —
    // same signal to the caller, different mechanism, pinned by test.)
    if failed { EXIT_ERROR } else { code }
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

    #[test]
    fn explicit_empty_command_means_help() {
        // Distinct from no-arg only at the argv level (`$1` set-but-empty
        // hits the same `${1:-help}` default); pinned so a future
        // refactor cannot turn it into "unknown command".
        let (code, out, err) = run_text(&[""]);
        assert_eq!(code, EXIT_SUCCESS);
        assert_eq!(out, HELP);
        assert!(err.is_empty());
    }

    #[test]
    fn closed_stdout_reports_failure_not_success() {
        struct Failing;
        impl std::io::Write for Failing {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let owned = vec![OsString::from("help")];
        let mut err = Vec::new();
        let code = run(owned, &mut Failing, &mut err);
        assert_eq!(code, EXIT_ERROR);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_command_echoes_raw_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let raw = vec![0x66, 0x6F, 0xFF, 0x62]; // "fo\xFFb"
        let owned = vec![OsString::from_vec(raw.clone())];
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(owned, &mut out, &mut err);
        assert_eq!(code, EXIT_ERROR);
        assert!(out.is_empty());
        let mut expected = b"dot: unknown command: ".to_vec();
        expected.extend_from_slice(&raw);
        expected.push(b'\n');
        assert_eq!(err, expected);
    }
}
