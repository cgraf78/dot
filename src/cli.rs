//! Command dispatch for the `dot` CLI (slices 1 + 77).
//!
//! Hand-rolled parsing over `std::env::args_os`, no CLI framework: the
//! shell dispatcher matches on `${1:-help}` with a small fixed command
//! set, and startup latency is a first-class budget (see
//! `tests/perf_budget.rs`). Streams are injected so parity tests capture
//! output without subprocesses.
//!
//! Slice 1 owned `help` and `version` only. Slice 77 adds the full
//! [`dispatch`] table for `dot_command_dispatch`
//! (`lib/dot/commands.sh`): every command name decides a [`Command`]
//! exactly like the shell `case`. `help`/`version`/`cron`/`unknown`
//! execute here with byte-exact shell parity; the remaining commands
//! report "not yet implemented" until their kernel slices land (their
//! dispatch decision is final — a later slice only fills in the call,
//! never re-decides the routing), except `init`, which slice 79
//! drives through [`init_client_command::run`]. The shell `bin/dot`
//! remains the entry point and never routes here yet.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::init_client_command;
use crate::init_client_identity as identity;
use crate::init_client_record::TransactionRecord;
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

/// `dot_command_dispatch` decision (`lib/dot/commands.sh`).
///
/// One variant per shell `case` arm. Each variant names the kernel that
/// executes it plus the shell's exit-code contract, so the owning slice
/// wires the call without re-deriving the plumbing. The headline
/// contract: the dispatcher returns `0` unless an arm says otherwise —
/// `update`/`fetch`/`push`/`status`/`diff`/`doctor`/`init`/`cron` ignore
/// their kernels' statuses and succeed whenever setup does; only the
/// early `return` sites (lock/resolve failures) and `test` (which
/// records `rc=$?`) propagate nonzero codes. Pinned differentially in
/// `tests/cli.rs` against the live shell with stubbed kernels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `update`, plus `pull` (the shell recurses into the `update`
    /// arm): owner traps, `_dot_update_lock_acquire "$@"` (failure
    /// returns its status, e.g. lock-busy), then `_dot_update "$@"`
    /// whose status is ignored — success is `0` regardless.
    /// Kernels live in other lanes (`update`, `update_lock`,
    /// `cleanup`).
    Update,
    /// `fetch`: `_dot_resolve_overlays fetch` (failure returns `1`),
    /// then `_repo_fetch_all "$@"` (status ignored → `0`).
    /// Kernel [`crate::repos_commands::fetch_all`] is ported; overlay
    /// resolution lives in another lane.
    Fetch,
    /// `push`: `_dot_resolve_overlays inspect` (failure returns `1`),
    /// then `_repo_push_all "$@"` (status ignored → `0`).
    /// Kernel [`crate::repos_commands::push_all`] is ported.
    Push,
    /// `status`: `_dot_resolve_overlays inspect` (failure returns
    /// `1`), then `_repo_status_all "$@"` (status ignored → `0`).
    /// Kernel [`crate::repos_commands::status_all`] is ported.
    Status,
    /// `diff`: `_dot_resolve_overlays inspect` (failure returns `1`),
    /// then `_repo_diff_all "$@"` (status ignored → `0`).
    /// Kernel [`crate::repos_commands::diff_all`] is ported.
    Diff,
    /// `cron`: `crontab -l`, falling back to `no crontab installed`
    /// (always `0`). Executed in [`run`] — no kernel, owned here.
    Cron,
    /// `doctor`: owner traps, `DOT_OVERLAY_DISCOVERY_SILENT=1`,
    /// `_dot_resolve_overlays inspect` (`|| true` — failure ignored),
    /// then `_dot_doctor` (status ignored → always `0`). Kernel lives
    /// in another lane.
    Doctor,
    /// `test`: owner traps, `_dot_resolve_overlays inspect` (failure
    /// returns `1`), then `dot_test_command "$@"` whose status becomes
    /// the dispatcher's code (`rc=$?`). Kernel lives in another lane.
    Test,
    /// `init`: owner traps, then [`init_acquires_lock`] decides the
    /// nested `case ${1:-}` — `_dot_update_lock_acquire` unless the
    /// first argument is `--status`, `--help`, or `-h` (lock failure
    /// returns its status) — then `dot_init_command "$@"`, wired to
    /// [`init_client_command::run`] (see `run_init` below).
    ///
    /// Exit-code note: the dispatcher text ignores the kernel status
    /// (`0` regardless), but production runs under `set -euo pipefail`
    /// (`bin/dot`, `lib/dot/main.sh`), so a failing kernel exits the
    /// process with its own code before the dispatcher resumes —
    /// `dot init --bogus` exits `1`, pinned against `bin/dot`.
    /// [`run`] therefore reports the kernel's code directly. Lock
    /// acquisition still arrives with its slice: until then every
    /// `init` proceeds without locking, like every other arm here.
    Init,
    /// Anything else: `dot: unknown command: %s` on stderr, `1`.
    Unknown,
}

/// Decide the [`Command`] for raw command bytes, exactly like the
/// shell `case $command in`.
///
/// Byte matching (never decoded): command names are pure ASCII, and
/// the shell match is case-sensitive (`shopt -u nocasematch` in
/// `bin/dot`), so any non-listed bytes — including `help`, `version`,
/// flags, and the empty string — decide [`Command::Unknown`]. Those
/// never reach here from [`run`], which pre-handles them exactly like
/// `main.sh` does before calling `dot_command_dispatch`; the `Unknown`
/// decision documents what the shell function itself would do with
/// them (notably `dot: unknown command: help` for no argument).
pub fn dispatch(command: &[u8]) -> Command {
    match command {
        b"update" | b"pull" => Command::Update,
        b"fetch" => Command::Fetch,
        b"push" => Command::Push,
        b"status" => Command::Status,
        b"diff" => Command::Diff,
        b"cron" => Command::Cron,
        b"doctor" => Command::Doctor,
        b"test" => Command::Test,
        b"init" => Command::Init,
        _ => Command::Unknown,
    }
}

/// The `init` arm's nested `case ${1:-}`: whether `init` acquires the
/// update lock before running `dot_init_command`.
///
/// `first_arg` is the first argument after the command (post-`shift`
/// `$1`); `None` is no argument at all, which the shell's `${1:-}`
/// spells as empty and routes to `*` (acquire). Only the read-only
/// probes `--status`, `--help`, and `-h` skip the lock. The owning
/// `init` slice consumes this when it wires [`Command::Init`].
pub fn init_acquires_lock(first_arg: Option<&[u8]>) -> bool {
    match first_arg {
        Some(arg) => {
            arg != b"--status".as_slice() && arg != b"--help".as_slice() && arg != b"-h".as_slice()
        }
        None => true,
    }
}

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
        // `main.sh` loads config and the runtime before dispatch, so
        // everything else is `dot_command_dispatch` (`commands.sh`).
        other => match dispatch(other) {
            Command::Cron => run_cron(stdout, &mut failed),
            Command::Init => {
                let rest: Vec<Vec<u8>> = args.map(|arg| argv_bytes(&arg)).collect();
                run_init(&rest, stdout, stderr, &mut failed)
            }
            Command::Unknown => {
                // A closed stderr here leaves nothing to report to; the
                // exit code still carries the failure.
                let _ = stderr.write_all(b"dot: unknown command: ");
                let _ = stderr.write_all(other);
                let _ = stderr.write_all(b"\n");
                EXIT_ERROR
            }
            // Known command whose kernel lives in another lane: the
            // dispatch decision above is final (a later slice only
            // fills in the call), so this names the typed command
            // instead of falling through to "unknown command".
            _ => {
                let _ = stderr.write_all(b"dot: command '");
                let _ = stderr.write_all(other);
                let _ = stderr.write_all(b"' is not yet implemented\n");
                EXIT_ERROR
            }
        },
    };
    // A closed pipe must not report success for undelivered output.
    // (The shell dies on SIGPIPE; Rust reports failure via exit code —
    // same signal to the caller, different mechanism, pinned by test.)
    if failed { EXIT_ERROR } else { code }
}

/// The [`Command::Cron`] arm: `crontab -l`, falling back to the
/// shell's `no crontab installed` line.
///
/// Fully owned here — the shell arm calls no kernel, so there is no
/// neighboring implementation to wait for:
/// `crontab -l 2>/dev/null || printf '  no crontab installed\n'`.
/// A missing `crontab` binary fails the spawn exactly like the
/// shell's `127` feeds the `||`, and crontab diagnostics are dropped
/// on both sides (the shell's `2>/dev/null`; here by capturing and
/// ignoring stderr). The arm always succeeds — the shell's `rc` stays
/// `0` either way — and only undelivered output flips `failed`, which
/// [`run`] turns into [`EXIT_ERROR`] like the other arms.
fn run_cron(stdout: &mut dyn Write, failed: &mut bool) -> i32 {
    let listed = std::process::Command::new("crontab").arg("-l").output();
    let show: Vec<u8> = match listed {
        Ok(output) if output.status.success() => output.stdout,
        _ => b"  no crontab installed\n".to_vec(),
    };
    if stdout.write_all(&show).is_err() {
        *failed = true;
    }
    EXIT_SUCCESS
}

/// The [`Command::Init`] arm: `dot_init_command "$@"` through
/// [`init_client_command::run`].
///
/// Process environment is read here — the dispatcher is the engine
/// boundary, so ambient reads live in this arm while the command
/// module itself takes explicit parameters (its [`CommandEnv`][init_client_command::CommandEnv]).
/// Effect-free helpers run as the real ports inside the module; the
/// network default-branch probe binds its ported helper with a
/// `TMPDIR` scratch, while the deep resume and rollback trees plus
/// the line-1872+ fresh tail stay interim "not yet implemented"
/// closures until their slices fill them in. The arm reports the
/// kernel's own code, matching the production process under
/// `set -euo pipefail` (pinned against `bin/dot`; see the
/// [`Command::Init`] contract).
fn run_init(
    args: &[Vec<u8>],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg_state_home = std::env::var("XDG_STATE_HOME").unwrap_or_default();
    let skip_provider = std::env::var("DOT_INIT_SKIP_PROVIDER").ok();
    let source_root = std::env::var_os("DOT_SOURCE_ROOT").map(PathBuf::from);
    // Degraded-env ownership root: without a source checkout the
    // stage-ownership hash cannot verify, so recovery preserves
    // every stage (fail closed), like the shell's failed probe.
    let fallback = PathBuf::from("/nonexistent-dot-source-root");
    let source_root = source_root.as_deref().unwrap_or(&fallback);
    let scratch = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let remote_default_branch =
        |url: &str| -> Option<String> { identity::remote_default_branch(url, &scratch) };
    let resume = |_: &Path, _: &Path, _: &TransactionRecord| -> Result<(), Error> {
        Err(Error::Usage {
            message: "initialization resume is not yet implemented",
        })
    };
    let rollback = |_: &Path| -> Result<(), Error> {
        Err(Error::Usage {
            message: "initialization rollback is not yet implemented",
        })
    };
    let fresh = |_: &init_client_command::FreshInputs| -> init_client_command::InitReport {
        init_client_command::InitReport {
            stdout: Vec::new(),
            stderr: b"dot: command 'init' is not yet implemented\n".to_vec(),
            code: EXIT_ERROR,
        }
    };
    let env = init_client_command::CommandEnv {
        home: &home,
        xdg_state_home: &xdg_state_home,
        skip_provider: skip_provider.as_deref(),
        source_root,
    };
    let engine = init_client_command::CommandEngine {
        remote_default_branch: &remote_default_branch,
        resume: &resume,
        rollback: &rollback,
        fresh: &fresh,
    };
    let report = init_client_command::run(&env, &engine, args);
    if stdout.write_all(&report.stdout).is_err() {
        *failed = true;
    }
    if stderr.write_all(&report.stderr).is_err() {
        *failed = true;
    }
    report.code
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

    #[test]
    fn dispatch_names_every_shell_arm() {
        // One entry per `case` arm in `lib/dot/commands.sh`; `pull`
        // recurses into the `update` arm, so both names decide `Update`.
        let cases: &[(&[u8], Command)] = &[
            (b"update", Command::Update),
            (b"pull", Command::Update),
            (b"fetch", Command::Fetch),
            (b"push", Command::Push),
            (b"status", Command::Status),
            (b"diff", Command::Diff),
            (b"cron", Command::Cron),
            (b"doctor", Command::Doctor),
            (b"test", Command::Test),
            (b"init", Command::Init),
            (b"frobnicate", Command::Unknown),
        ];
        for (name, expected) in cases {
            assert_eq!(dispatch(name), *expected, "command: {name:?}");
        }
    }

    #[test]
    fn dispatch_matches_bytes_like_shell_case() {
        // The shell `case` is byte-exact and case-sensitive
        // (`shopt -u nocasematch` in `bin/dot`): near-misses are
        // unknown, never folded onto a known command.
        for name in [
            b"Update".as_slice(),
            b"UPDATE".as_slice(),
            b" update".as_slice(),
            b"update ".as_slice(),
            b"updat".as_slice(),
            b"updates".as_slice(),
            b"--help".as_slice(),
            b"help".as_slice(),
            b"version".as_slice(),
            b"".as_slice(),
        ] {
            assert_eq!(dispatch(name), Command::Unknown, "command: {name:?}");
        }
    }

    #[test]
    fn init_lock_skips_only_status_and_help_flags() {
        // Mirrors the `init` arm's nested `case ${1:-}`: the three
        // read-only probes skip the lock, while any other first
        // argument — including none at all — acquires it.
        for flag in [
            b"--status".as_slice(),
            b"--help".as_slice(),
            b"-h".as_slice(),
        ] {
            assert!(!init_acquires_lock(Some(flag)), "flag: {flag:?}");
        }
        assert!(init_acquires_lock(None));
        for arg in [b"".as_slice(), b"--other".as_slice(), b"update".as_slice()] {
            assert!(init_acquires_lock(Some(arg)), "arg: {arg:?}");
        }
    }

    #[test]
    fn kernel_commands_report_not_implemented_not_unknown() {
        // Known commands whose kernels live in other lanes: the
        // dispatch decision is final, so they must never fall through
        // to the unknown-command diagnostic. Interim exit is the
        // generic failure until the owning slice wires the kernel.
        // (`init` left this set when slice 79 wired it; see the
        // `init_*` tests below.)
        for command in [
            "update", "pull", "fetch", "push", "status", "diff", "doctor", "test",
        ] {
            let (code, out, err) = run_text(&[command]);
            assert_eq!(code, EXIT_ERROR, "command: {command}");
            assert!(out.is_empty(), "command: {command}");
            assert_eq!(
                err,
                format!("dot: command '{command}' is not yet implemented\n"),
                "command: {command}"
            );
        }
    }

    #[test]
    fn init_help_drives_usage_successfully() {
        // Wired slice: `init --help` prints the init usage (not the
        // dispatcher help, not "not yet implemented"). These paths
        // never consult process environment values, so they stay
        // deterministic in-process.
        for argv in [vec!["init", "--help"], vec!["init", "-h"]] {
            let (code, out, err) = run_text(&argv);
            assert_eq!(code, EXIT_SUCCESS, "argv: {argv:?}");
            assert_eq!(
                out,
                String::from_utf8(crate::init_client_adopt::usage()).expect("usage UTF-8"),
                "argv: {argv:?}"
            );
            assert!(err.is_empty(), "argv: {argv:?}");
        }
    }

    #[test]
    fn init_unknown_option_reports_with_kernel_code() {
        // The kernel's own code crosses the dispatcher: production
        // runs under `set -euo pipefail`, so the shell exits `1`
        // inside `_dot_init_error` (pinned against `bin/dot`), and
        // so does this arm — never the dispatcher's ignore-status
        // default, never the interim text.
        let (code, out, err) = run_text(&["init", "--bogus"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(err, "dot init: unknown option: --bogus\n");
    }

    #[test]
    fn init_identity_failure_reports_with_kernel_code() {
        // Past parsing, the first resolvable failure also crosses
        // with its code (identity here; the fresh tail stays
        // interim).
        let (code, out, err) = run_text(&["init", "--branch", "main", "notaurl"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(err, "dot init: unsupported repository URL: notaurl\n");
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
