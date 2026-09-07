//! Command dispatch for the native `dot` CLI.
//!
//! Hand-rolled parsing over `std::env::args_os`, no CLI framework: the
//! command set, and startup latency is a first-class budget (see
//! `tests/perf_budget.rs`). Streams are injected so tests capture output
//! without subprocesses. [`dispatch`] is the single routing table and its
//! exhaustive match keeps every public command wired.
//!
//! [`Command::Update`] uses the sequencer's flag parser
//! ([`crate::update::parse_update_flags`]) and runs end to end through
//! [`update_run::run`](crate::update_run::run), with a native lock and
//! native engine. `init` runs through [`init_client_command::run`];
//! `fetch`/`push`/`status`/`diff` through overlay resolution
//! ([`crate::overlays::resolve`]) plus the matching
//! [`crate::repos_commands`] kernel. Native [`Command::Test`] owns discovery,
//! scheduling, result collection, timeout and cancellation directly.
//! The startup prelude ([`crate::startup`]) runs at the top of [`run`]: the re-exec
//! guard (exit 1) then `dot_config_load || exit 2` before dispatch
//! for every command.

use std::ffi::{OsStr, OsString};
use std::io::IsTerminal as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::init_client_command;
use crate::init_client_engine;
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

/// Exact help bytes, including the trailing newline. One literal per line:
/// a `\`-continued literal would strip the two-space command indent.
/// Pinned directly by `tests/cli.rs`.
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

/// Public process exit-code contract:
/// `0` success, `1` error/unknown command, `2` usage/config failure,
/// `75` lock busy. Numeric codes cross the process boundary into
/// scripts and CI gates, so they are named constants — never inline
/// literals.
pub const EXIT_SUCCESS: i32 = 0;
/// Generic failure (unknown command today; repo-failure paths reuse it
/// per the historical shell `return 1` sites).
pub const EXIT_ERROR: i32 = 1;
/// Config/usage failure (`dot_config_load || exit 2` in
/// `lib/dot/main.sh`, owned by the [`crate::startup`] prelude).
/// Named so the startup gate shares one value with later usage errors.
pub const EXIT_USAGE: i32 = 2;

/// Command dispatch decision.
///
/// One variant per shell `case` arm. Each variant names the kernel that
/// executes it plus the shell's exit-code contract. The headline
/// contract: the dispatcher returns `0` unless an arm says otherwise —
/// `update`/`fetch`/`push`/`status`/`diff`/`doctor`/`init`/`cron` ignore
/// their kernels' statuses and succeed whenever setup does; only the
/// early `return` sites (lock/resolve failures) and `test` (which
/// records its runner status) propagate nonzero codes. Pinned directly in
/// `tests/cli.rs` with stubbed kernels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `update`, plus `pull` (the shell recurses into the `update`
    /// arm): native update-lock acquisition (failure returns its status,
    /// e.g. lock-busy `75`), then the native update engine, wired to
    /// [`update_run::run`](crate::update_run::run).
    ///
    /// Exit-code note: the dispatcher text ignores the kernel status
    /// (`0` regardless), but production runs under `set -euo pipefail`
    /// (`bin/dot`, `lib/dot/main.sh`), so a failing kernel exits the
    /// process with its own code before the dispatcher resumes —
    /// `dot update` reports a failure as `1`, pinned against
    /// `bin/dot`; see the [`Command::Init`] contract. Lock
    /// acquisition is native here and for `init`.
    Update,
    /// `fetch`: `_dot_resolve_overlays fetch` (failure returns `1`),
    /// then `_repo_fetch_all "$@"`. Wired through `run_repos` using
    /// [`crate::overlays::resolve`] plus
    /// [`crate::repos_commands::fetch_all`].
    ///
    /// Exit-code note: the dispatcher text ignores the kernel status
    /// (`0` regardless), but production runs under `set -euo
    /// pipefail` (`bin/dot`, `lib/dot/main.sh`), so a failing kernel
    /// exits the process with its own code before the dispatcher
    /// resumes — a rejected base push exits `1`, pinned against
    /// `bin/dot`. `run_repos` therefore reports the kernel's code
    /// directly (the [`Command::Init`] precedent).
    Fetch,
    /// `push`: `_dot_resolve_overlays inspect` (failure returns `1`),
    /// then `_repo_push_all "$@"`. Wired through `run_repos` using
    /// [`crate::overlays::resolve`] plus
    /// [`crate::repos_commands::push_all`]. Kernel codes cross the
    /// process boundary under `set -euo pipefail` (see
    /// [`Command::Fetch`]).
    Push,
    /// `status`: `_dot_resolve_overlays inspect` (failure returns
    /// `1`), then `_repo_status_all "$@"`. Wired through `run_repos` using
    /// [`crate::overlays::resolve`] plus
    /// [`crate::repos_commands::status_all`]. Kernel codes cross the
    /// process boundary under `set -euo pipefail` (see
    /// [`Command::Fetch`]).
    Status,
    /// `diff`: `_dot_resolve_overlays inspect` (failure returns `1`),
    /// then `_repo_diff_all "$@"`. Wired through `run_repos` using
    /// [`crate::overlays::resolve`] plus
    /// [`crate::repos_commands::diff_all`]. Kernel codes cross the
    /// process boundary under `set -euo pipefail` (see
    /// [`Command::Fetch`]).
    Diff,
    /// `cron`: `crontab -l`, falling back to `no crontab installed`
    /// (always `0`). Executed in [`run`] — no kernel, owned here.
    Cron,
    /// `doctor`: owner traps, `DOT_OVERLAY_DISCOVERY_SILENT=1`,
    /// `_dot_resolve_overlays inspect` (`|| true` — failure ignored),
    /// then `_dot_doctor` (the dispatcher text ignores the status
    /// with `return "$rc"`, but production runs under `set -euo
    /// pipefail`, so a failing kernel exits the process with its own
    /// code before the dispatcher resumes — the [`Command::Init`]
    /// precedent, pinned against `bin/dot`). The native coordinator owns the
    /// complete check, extension, and rendering path.
    Doctor,
    /// Native suite supervisor: inspect resolution gates discovery; argument
    /// validation, trusted launch, bounded scheduling and owned cancellation
    /// report their aggregate status directly.
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
    /// [`run`] therefore reports the kernel's code directly. Native
    /// initialization holds one operation lock through convergence.
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
/// `init` uses this to decide whether the operation lock is required.
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
/// `$@` for these commands. Each command owns its own
/// parser; nothing here may grow flags implicitly.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let env = std::env::vars_os().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let runtime = crate::app::Runtime::from_env(&env, &cwd)
        .expect("the current directory fallback is absolute");
    let args = args.into_iter().collect::<Vec<_>>();
    crate::app::run_direct(
        &runtime,
        &args,
        &mut crate::app::Streams::new(stdout, stderr),
    )
}

/// Run the CLI with a preconstructed immutable runtime.
pub(crate) fn run_with_runtime(
    runtime: &crate::app::Runtime,
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let mut args = args.iter().cloned();
    let command = args.next().unwrap_or_default();
    let command = argv_bytes(&command);
    let command = command.as_slice();
    // Shell matches `${1:-help}`: empty or missing command shows help.
    // The empty-string arm matters because `run([])` (no argv at all,
    // as in tests) must behave like bare `dot`, not like an error.
    // The re-exec guard precedes every command. Informational commands then
    // return without reading user configuration, matching the shell entry
    // point; operational commands load configuration before dispatch.
    if matches!(
        command,
        b"" | b"help" | b"-h" | b"--help" | b"version" | b"--version"
    ) {
        if let Err(failure) = crate::startup::check_reexec(runtime) {
            let _ = stderr.write_all(failure.line().as_bytes());
            let _ = stderr.write_all(b"\n");
            return failure.code();
        }
        return match command {
            b"" | b"help" | b"-h" | b"--help" => {
                if stdout.write_all(HELP.as_bytes()).is_err() {
                    EXIT_ERROR
                } else {
                    EXIT_SUCCESS
                }
            }
            _ => {
                if writeln!(stdout, "{}", version::version_line()).is_err() {
                    EXIT_ERROR
                } else {
                    EXIT_SUCCESS
                }
            }
        };
    }
    let config = match crate::startup::check(runtime) {
        Ok(config) => config,
        Err(failure) => {
            let _ = stderr.write_all(failure.line().as_bytes());
            let _ = stderr.write_all(b"\n");
            return failure.code();
        }
    };
    let selected = dispatch(command);
    let home = runtime.home().to_string_lossy();
    let state = runtime.state_home().to_string_lossy();
    let identity = if selected == Command::Init {
        crate::repos_base::select_for_init(runtime, &home, &state, stderr)
    } else {
        crate::repos_base::select(runtime, &home, &state, stderr)
    };
    if identity.is_err() {
        return EXIT_ERROR;
    }

    let mut failed = false;
    let code = match command {
        b"" | b"help" | b"-h" | b"--help" | b"version" | b"--version" => unreachable!(),
        // `main.sh` loads config and the runtime before dispatch, so
        // everything else is `dot_command_dispatch` (`commands.sh`).
        other => match selected {
            Command::Cron => run_cron(stdout, &mut failed),
            Command::Update => {
                let rest: Vec<OsString> = args.collect();
                run_update(runtime, &config, &rest, stdout, stderr)
            }
            Command::Init => {
                let rest: Vec<Vec<u8>> = args.map(|arg| argv_bytes(&arg)).collect();
                run_init(runtime, &rest, stdout, stderr, &mut failed)
            }
            command @ (Command::Fetch | Command::Push | Command::Status | Command::Diff) => {
                let rest: Vec<OsString> = args.collect();
                run_repos(runtime, &config, command, &rest, stdout, stderr)
            }
            Command::Doctor => {
                let mut streams = crate::app::Streams::new(stdout, stderr);
                crate::doctor::run(runtime, &mut streams)
            }
            Command::Test => {
                let rest: Vec<OsString> = args.collect();
                crate::test_command::run(
                    runtime,
                    &rest,
                    &mut crate::app::Streams::new(stdout, stderr),
                )
            }
            Command::Unknown => {
                // A closed stderr here leaves nothing to report to; the
                // exit code still carries the failure.
                let _ = stderr.write_all(b"dot: unknown command: ");
                let _ = stderr.write_all(other);
                let _ = stderr.write_all(b"\n");
                EXIT_ERROR
            }
        },
    };
    // A closed pipe must not report success for undelivered output.
    // (The shell dies on SIGPIPE; Rust reports failure via exit code —
    // same signal to the caller, different mechanism, pinned by test.)
    if failed { EXIT_ERROR } else { code }
}

/// The [`Command::Update`] arm: parse the leading flags
/// through the sequencer kernel, apply the shell loop's exports,
/// then run the update end to end and report its exit code (`0` on
/// success).
///
/// `_dot_update` (`lib/dot/update.sh`) consumes `--cron --quiet
/// -f`/`--force -v`/`--verbose` up front — exporting the
/// quiet/force/verbose pairs and unsetting `DOT_OVERLAY_LINKS_FROZEN`
/// — before the repo sync and finalize steps run. This arm hands the
/// residue to [`update_run::run`](crate::update_run::run): native lock plus
/// the native engine.
fn run_update(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let signals = match crate::cleanup::Signals::install() {
        Ok(signals) => signals,
        Err(_) => return EXIT_ERROR,
    };
    let raw: Vec<Vec<u8>> = args.iter().map(argv_bytes).collect();
    let refs: Vec<&[u8]> = raw.iter().map(Vec::as_slice).collect();
    let parsed = crate::update::parse_update_flags(&refs);
    // The shell exports these values before the engine runs. Keep that
    // behavior inside this invocation's child environment instead of
    // publishing transient command state to other callers in this process.
    let mut env = runtime.env().clone();
    env.remove(OsStr::new("DOT_OVERLAY_LINKS_FROZEN"));
    if parsed.quiet {
        env.insert(OsString::from("DOT_QUIET"), OsString::from("1"));
        env.insert(OsString::from("SHDEPS_QUIET"), OsString::from("1"));
    }
    if parsed.force {
        env.insert(OsString::from("DOT_FORCE"), OsString::from("1"));
        env.insert(OsString::from("SHDEPS_FORCE"), OsString::from("1"));
    }
    if parsed.verbose {
        env.insert(OsString::from("DOT_VERBOSE"), OsString::from("1"));
        env.insert(OsString::from("SHDEPS_LOG_LEVEL"), OsString::from("2"));
    }
    // Preserve the kernel's codes (`0` success, `1` failure, `2` config
    // rejection, `75` lock busy) across the typed stream boundary.
    let code = crate::update_run::run(
        runtime,
        config,
        &env,
        crate::update_run::Request { args },
        stdout,
        stderr,
    );
    signals.received().map_or(code, |signal| 128 + signal)
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
/// Runtime inputs are read here — the dispatcher is the engine
/// boundary, so command modules receive explicit parameters (its
/// [`CommandEnv`][init_client_command::CommandEnv]).
/// Effect-free helpers run as the real ports inside the module; the
/// network default-branch probe binds its ported helper with a
/// `TMPDIR` scratch, and the resume, rollback, and fresh-tail steps
/// bind the production wiring
/// ([`init_client_engine::Production`]). Convergence calls the native
/// update application service directly while the init operation lock
/// remains held; it never recursively invokes the CLI.
/// The arm reports the kernel's own code, matching the production
/// process under `set -euo pipefail` (pinned against `bin/dot`;
/// see the [`Command::Init`] contract).
fn run_init(
    runtime: &crate::app::Runtime,
    args: &[Vec<u8>],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let home = runtime
        .value("HOME")
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let xdg_state_home = runtime
        .value("XDG_STATE_HOME")
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let skip_provider = runtime
        .value("DOT_INIT_SKIP_PROVIDER")
        .and_then(OsStr::to_str);
    // The command gate already rejected every spelling but `0` and
    // `1` before the engine runs; anything else never reaches it.
    let skip_provider_flag = skip_provider
        .filter(|value| !value.is_empty())
        .unwrap_or("0")
        == "1";
    let source_root = runtime.source_root();
    let Some(host_git) = runtime
        .value("PATH")
        .and_then(OsStr::to_str)
        .and_then(|path| identity::select_host_git(home, &source_root.to_string_lossy(), path))
    else {
        if writeln!(stderr, "dot init: {}", identity::NO_HOST_GIT).is_err() {
            *failed = true;
        }
        return EXIT_ERROR;
    };
    let scratch = runtime
        .value("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // The shell inherits its working directory; a deleted one can
    // never serve the reserved probe, so fall back to the client
    // root there (fail closed on the lookup, never on the run).
    let cwd = runtime.cwd();
    let remote_default_branch =
        |url: &str| -> Option<String> { identity::remote_default_branch(url, &scratch) };
    let log = crate::log::Log::new(false, false);
    let guard = if init_acquires_lock(args.first().map(Vec::as_slice)) {
        match crate::update_lock::acquire(runtime.state_home(), false, &log, None, stderr) {
            Ok(guard) => Some(guard),
            Err(Error::LockBusy { .. }) => return crate::update_lock::EXIT_LOCK_BUSY,
            Err(_) => return EXIT_ERROR,
        }
    } else {
        None
    };
    let mut update_env = runtime.env().clone();
    if let Some(guard) = &guard {
        update_env.insert(
            OsString::from("DOT_UPDATE_LOCK_TOKEN"),
            OsString::from(guard.token()),
        );
    }
    let converge_stdout = std::cell::RefCell::new(Vec::new());
    let converge_stderr = std::cell::RefCell::new(Vec::new());
    let converge_called = std::cell::Cell::new(false);
    let resume_converge_failed = std::cell::Cell::new(false);
    let converge = || -> Result<(), Error> {
        converge_called.set(true);
        let mut out = converge_stdout.borrow_mut();
        let mut err = converge_stderr.borrow_mut();
        let config = init_config(runtime, &mut *err)?;
        let mut streams = crate::app::Streams::new(&mut *out, &mut *err);
        let code = crate::update_engine::run_update(
            runtime,
            &crate::update_engine::UpdateRequest {
                config: &config,
                env: &update_env,
                args: &[],
                state_home: runtime.state_home(),
            },
            &mut streams,
        );
        if code == EXIT_SUCCESS {
            Ok(())
        } else {
            Err(Error::Command {
                command: "native init convergence".to_string(),
                status: Some(format!("exit status: {code}")),
            })
        }
    };
    let production = init_client_engine::Production::new(
        init_client_engine::EngineCtx {
            home,
            xdg_state_home,
            source_root,
            skip_provider: skip_provider_flag,
            shdeps_update_policy: runtime
                .value("DOT_SHDEPS_UPDATE_POLICY")
                .and_then(OsStr::to_str),
            cwd,
        },
        &converge,
    );
    let resume =
        |transaction: &Path, record: &Path, journal: &TransactionRecord| -> Result<(), Error> {
            let result = production.resume(transaction, record, journal);
            if result.is_err() && converge_called.get() {
                resume_converge_failed.set(true);
            }
            result
        };
    let rollback = |at: &Path| -> Result<(), Error> { production.rollback(at) };
    let fresh = |inputs: &init_client_command::FreshInputs| -> init_client_command::InitReport {
        production.run_fresh(inputs)
    };
    let env = init_client_command::CommandEnv {
        home,
        xdg_state_home,
        skip_provider,
        source_root,
    };
    let engine = init_client_command::CommandEngine {
        remote_default_branch: &remote_default_branch,
        resume: &resume,
        rollback: &rollback,
        fresh: &fresh,
    };
    let report = identity::with_host_git(Path::new(&host_git), || {
        init_client_command::run(&env, &engine, args)
    });
    if write_init_output(
        stdout,
        stderr,
        &report,
        &converge_stdout.into_inner(),
        &converge_stderr.into_inner(),
        resume_converge_failed.get(),
    )
    .is_err()
    {
        *failed = true;
    }
    if let Some(guard) = guard {
        guard.release(&log, stderr);
    }
    report.code
}

/// Reload configuration after init publishes the candidate worktree. The
/// Runtime freezes environment, not files, so this sees configuration cloned
/// after process startup just like `dot_config_load` in the shell path.
fn init_config(
    runtime: &crate::app::Runtime,
    stderr: &mut dyn Write,
) -> Result<crate::config::Config, Error> {
    crate::startup::check(runtime).map_err(|failure| {
        let _ = stderr.write_all(failure.line().as_bytes());
        let _ = stderr.write_all(b"\n");
        Error::Command {
            command: "native init configuration reload".to_string(),
            status: Some(format!("exit status: {}", failure.code())),
        }
    })
}

/// Emit init and convergence streams in execution order. A resumed
/// transaction prints the update failure before its wrapper diagnostic;
/// fresh and completed paths contain only pre-convergence init output.
fn write_init_output(
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    report: &init_client_command::InitReport,
    converge_stdout: &[u8],
    converge_stderr: &[u8],
    resume_converge_failed: bool,
) -> std::io::Result<()> {
    stdout.write_all(&report.stdout)?;
    stdout.write_all(converge_stdout)?;
    if resume_converge_failed {
        stderr.write_all(converge_stderr)?;
        stderr.write_all(&report.stderr)
    } else {
        stderr.write_all(&report.stderr)?;
        stderr.write_all(converge_stderr)
    }
}

pub(crate) fn base_from_values(
    home: &str,
    topology_value: Option<&str>,
    git_dir_value: Option<&str>,
) -> crate::repos_base::Base {
    let topology = match topology_value {
        Some("separate") => crate::repos_base::Topology::Separate,
        Some("ordinary") => crate::repos_base::Topology::Ordinary,
        _ => crate::repos_base::Topology::Missing,
    };
    let git_dir = git_dir_value
        .filter(|dir| !dir.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{home}/.dotfiles"));
    crate::repos_base::Base {
        topology,
        client_git_dir: git_dir,
        home: home.to_string(),
    }
}

/// The [`Command::Fetch`], [`Command::Push`], [`Command::Status`],
/// and [`Command::Diff`] arms: `_dot_resolve_overlays fetch` for
/// fetch, `inspect` for the rest (`|| return 1`), then the matching
/// [`crate::repos_commands`] kernel over `"$@"`.
///
/// The dispatcher text ignores the kernel status (`return "$rc"`
/// with `rc=0`), but production runs under `set -euo pipefail`, so
/// a failing kernel exits the process with its own code before the
/// dispatcher resumes — the arm reports the kernel's code directly
/// (the [`Command::Init`] precedent, pinned against `bin/dot`).
///
/// Runtime inputs are read here — the dispatcher is the engine
/// boundary, so kernel modules receive explicit parameters. Resolution diagnostics replay
/// the shell's stderr (collected warnings plus the failure line,
/// when the shell prints one); kernel headers go to `stdout`,
/// overlay push warnings to `stderr`, and git's own output streams
/// to the terminal through the kernel, exactly like the shell's
/// inherited stdio. Color follows fd 1 (`[[ -t 1 ]]`), not the
/// injected stream, so piped runs stay byte-identical on both
/// sides. Extra arguments pass through to `git` verbatim.
fn run_repos(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    command: Command,
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if !matches!(
        command,
        Command::Fetch | Command::Push | Command::Status | Command::Diff
    ) {
        return EXIT_ERROR;
    }
    let mode = if command == Command::Fetch {
        "fetch"
    } else {
        "inspect"
    };
    let home = runtime
        .value("HOME")
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let prefix = runtime.value("PREFIX").unwrap_or_default();
    let prefix = prefix.to_string_lossy();
    let state_home = runtime.state_home().to_string_lossy();
    let base = match crate::repos_base::select(runtime, home, &state_home, stderr) {
        Ok(base) => base,
        Err(()) => return EXIT_ERROR,
    };
    let inputs = crate::overlays::ResolveInputs {
        home: home.to_string(),
        xdg_config: runtime
            .value("XDG_CONFIG_HOME")
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_string(),
        discovery_silent: runtime.value("DOT_OVERLAY_DISCOVERY_SILENT") == Some(OsStr::new("1")),
        default_profile: Some(config.default_profile.clone()),
        user: crate::profiles::current_user(),
        host: crate::platform::detect_host().ok(),
        platform: crate::platform::detect_platform().ok(),
        termux: !prefix.is_empty() && prefix.contains("/com.termux/"),
        euid: match crate::temp::current_uid() {
            Some(uid) => uid,
            None => return EXIT_ERROR,
        },
    };
    let mut state = crate::overlays::State::default();
    let mut profiles = crate::profiles::State::default();
    if let Err(error) = crate::overlays::resolve(&mut state, &mut profiles, mode, &inputs) {
        for warning in &state.warnings {
            let _ = writeln!(stderr, "{warning}");
        }
        let rendered = error.to_string();
        if !rendered.is_empty() {
            let _ = writeln!(stderr, "{rendered}");
        }
        return EXIT_ERROR;
    }
    // The shell checks `[[ -t 1 && -z ${NO_COLOR:-} ]]` on the real
    // fd 1; the injected stream may be a capture buffer, so color
    // follows the process stdout instead.
    let log = crate::log::Log::from_env(
        std::io::stdout().is_terminal(),
        runtime.value("NO_COLOR").and_then(OsStr::to_str),
        runtime.value("DOT_QUIET").and_then(OsStr::to_str),
    );
    match command {
        Command::Fetch => {
            // The shell pays the same fork (`mask=$(umask)`); the
            // fallback is unreachable without a working `sh`, where
            // git is gone too.
            let mask =
                crate::startup::ensure_umask_ceiling(crate::temp::read_umask().unwrap_or(0o022));
            crate::repos_commands::fetch_all(&log, stdout, &base, &state.overlays, home, args, mask)
        }
        Command::Push => crate::repos_commands::push_all(
            &log,
            stdout,
            stderr,
            &base,
            &state.overlays,
            home,
            args,
        ),
        Command::Status => {
            crate::repos_commands::status_all(&log, stdout, &base, &state.overlays, home, args)
        }
        Command::Diff => {
            crate::repos_commands::diff_all(&log, stdout, &base, &state.overlays, home, args)
        }
        // Decided above; kept as generic failure, never a panic
        // (panics would break the stderr byte contract).
        _ => EXIT_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    fn run_init_text(args: &[&str]) -> (i32, String, String) {
        let home = dot_test_support::TempDir::new("cli-unit-init-home").expect("home");
        let state = dot_test_support::TempDir::new("cli-unit-init-state").expect("state");
        let env = BTreeMap::from([
            (
                OsString::from("HOME"),
                home.path().as_os_str().to_os_string(),
            ),
            (
                OsString::from("XDG_STATE_HOME"),
                state.path().as_os_str().to_os_string(),
            ),
            (
                OsString::from("DOT_SOURCE_ROOT"),
                OsString::from(env!("CARGO_MANIFEST_DIR")),
            ),
            (
                OsString::from("PATH"),
                std::env::var_os("PATH").expect("test PATH"),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, home.path()).expect("runtime");
        let owned: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_runtime(&runtime, &owned, &mut out, &mut err);
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
    fn base_from_values_honors_model_publication() {
        // The `model.sh` publication read at the dispatcher boundary:
        // known topologies pass through, anything else (unset,
        // `missing`, foreign) reads as missing, and an empty git dir
        // falls back to `$HOME/.dotfiles` like `${VAR:-...}`.
        let cases: &[(
            Option<&str>,
            Option<&str>,
            crate::repos_base::Topology,
            &str,
        )] = &[
            (
                Some("separate"),
                Some("/h/.dotfiles"),
                crate::repos_base::Topology::Separate,
                "/h/.dotfiles",
            ),
            (
                Some("ordinary"),
                Some("/h/.git"),
                crate::repos_base::Topology::Ordinary,
                "/h/.git",
            ),
            (
                None,
                None,
                crate::repos_base::Topology::Missing,
                "/h/.dotfiles",
            ),
            (
                Some("missing"),
                None,
                crate::repos_base::Topology::Missing,
                "/h/.dotfiles",
            ),
            (
                Some("bogus"),
                Some("/h/.git"),
                crate::repos_base::Topology::Missing,
                "/h/.git",
            ),
            (
                Some("separate"),
                Some(""),
                crate::repos_base::Topology::Separate,
                "/h/.dotfiles",
            ),
            (
                Some("separate"),
                None,
                crate::repos_base::Topology::Separate,
                "/h/.dotfiles",
            ),
        ];
        let mut observed = Vec::new();
        for (topology, git_dir, _, _) in cases {
            let base = base_from_values("/h", *topology, *git_dir);
            observed.push((base.topology, base.client_git_dir, base.home));
        }
        for (index, (_, _, want_topology, want_git_dir)) in cases.iter().enumerate() {
            let (got_topology, got_git_dir, got_home) = &observed[index];
            assert_eq!(got_topology, want_topology, "case: {index}");
            assert_eq!(got_git_dir, want_git_dir, "case: {index}");
            assert_eq!(got_home, "/h", "case: {index}");
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
        let (code, out, err) = run_init_text(&["init", "--bogus"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(err, "dot init: unknown option: --bogus\n");
    }

    #[test]
    fn init_identity_failure_reports_with_kernel_code() {
        // Past parsing, the first resolvable failure also crosses
        // with its code (identity here; the fresh tail stays
        // interim).
        let (code, out, err) = run_init_text(&["init", "--branch", "main", "notaurl"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(err, "dot init: unsupported repository URL: notaurl\n");
    }

    #[test]
    fn resume_convergence_failure_preserves_stderr_execution_order() {
        let report = init_client_command::InitReport {
            stdout: Vec::new(),
            stderr: b"dot init: initialization transaction could not be resumed safely\n".to_vec(),
            code: 1,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        write_init_output(
            &mut out,
            &mut err,
            &report,
            b"update stdout\n",
            b"update failed\n",
            true,
        )
        .expect("write ordered streams");
        assert_eq!(out, b"update stdout\n");
        assert_eq!(
            err,
            b"update failed\ndot init: initialization transaction could not be resumed safely\n"
        );
    }

    #[test]
    fn init_convergence_reloads_config_created_after_runtime_capture() {
        let home = dot_test_support::TempDir::new("cli-init-config-home").expect("home");
        let state = dot_test_support::TempDir::new("cli-init-config-state").expect("state");
        let env = BTreeMap::from([
            (
                OsString::from("HOME"),
                home.path().as_os_str().to_os_string(),
            ),
            (
                OsString::from("XDG_STATE_HOME"),
                state.path().as_os_str().to_os_string(),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, home.path()).expect("runtime");
        let config_dir = home.path().join(".config/dot");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("config"),
            b"version=1\ndependency_provider=shdeps\n",
        )
        .expect("post-capture config");

        let mut err = Vec::new();
        let config = init_config(&runtime, &mut err).expect("reload cloned config");
        assert_eq!(config.provider, crate::config::Provider::Shdeps);
        assert!(err.is_empty());
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
