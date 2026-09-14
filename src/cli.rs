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
//! execute here with byte-exact shell parity; every other command
//! started as "not yet implemented" until its kernel slice landed
//! (the dispatch decision is final — a later slice only fills in the
//! call, never re-decides the routing — and slice 83 wires the last
//! two, so the interim set is empty and the `run` match is
//! exhaustive). Slice 78 drives [`Command::Update`]
//! through the sequencer's flag parser
//! ([`crate::update::parse_update_flags`]): the shell loop's exports
//! land in the process environment while the engine (sync/finalize)
//! stays shell-owned, so the interim diagnostic remains; slice 79
//! drives `init` through [`init_client_command::run`]; slice 80
//! drives [`Command::Update`] end to end through
//! [`update_run::run`](crate::update_run::run) (native lock plus the
//! shell engine adapter — exit `0` on success); slice 82 drives
//! `fetch`/`push`/`status`/`diff` through overlay resolution
//! ([`crate::overlays::resolve`]) plus the matching
//! [`crate::repos_commands`] kernel. Slice 83 drives
//! [`Command::Doctor`] and [`Command::Test`] end to end through the
//! shared `ENGINE_SCRIPT` adapter below: the child mirrors the
//! `*)` arm of `lib/dot/main.sh` and calls `dot_command_dispatch`,
//! so the shell arm bodies (traps, resolve gating, kernels) run
//! exactly as production runs them — exit codes plus output parity,
//! resolve-failure paths included — while step execution stays
//! shell-owned until its slices land (the [`Command::Update`]
//! precedent through [`update_run::run`](crate::update_run::run)).
//! Slice 84 runs the startup
//! prelude ([`crate::startup`]) at the top of [`run`]: the re-exec
//! guard (exit 1) then `dot_config_load || exit 2` before dispatch
//! for every command, per the forward contracts (see
//! [`crate::startup`] for the deliberate help/version divergence
//! from the shell `case` order). The shell `bin/dot` remains the
//! entry point and never routes here yet.

use std::ffi::OsString;
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
/// `0` success, `1` error/unknown command, `2` usage/config failure,
/// `75` lock busy. Numeric codes cross the process boundary into
/// scripts and CI gates, so they are named constants — never inline
/// literals — and new codes arrive only with their owning slice (the
/// lock's `75` is not defined until the lock module lands).
pub const EXIT_SUCCESS: i32 = 0;
/// Generic failure (unknown command today; repo-failure paths reuse it
/// per the shell `return 1` sites). Named so later slices share one value.
pub const EXIT_ERROR: i32 = 1;
/// Config/usage failure (`dot_config_load || exit 2` in
/// `lib/dot/main.sh`, owned by the [`crate::startup`] prelude).
/// Named so the startup gate shares one value with later usage errors.
pub const EXIT_USAGE: i32 = 2;

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
    /// arm): owner traps, native `_dot_update_lock_acquire` (failure
    /// returns its status, e.g. lock-busy `75`), then `_dot_update`
    /// through the engine adapter, wired to
    /// [`update_run::run`](crate::update_run::run).
    ///
    /// Exit-code note: the dispatcher text ignores the kernel status
    /// (`0` regardless), but production runs under `set -euo pipefail`
    /// (`bin/dot`, `lib/dot/main.sh`), so a failing kernel exits the
    /// process with its own code before the dispatcher resumes —
    /// `dot update` reports a failure as `1`, pinned against
    /// `bin/dot`; see the [`Command::Init`] contract. Lock
    /// acquisition is native here (unlike `init`, whose lock still
    /// arrives with its slice).
    Update,
    /// `fetch`: `_dot_resolve_overlays fetch` (failure returns `1`),
    /// then `_repo_fetch_all "$@"`. Wired by slice 82 through
    /// `run_repos`: the ported [`crate::overlays::resolve`] plus
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
    /// then `_repo_push_all "$@"`. Wired by slice 82 through
    /// `run_repos`: the ported [`crate::overlays::resolve`] plus
    /// [`crate::repos_commands::push_all`]. Kernel codes cross the
    /// process boundary under `set -euo pipefail` (see
    /// [`Command::Fetch`]).
    Push,
    /// `status`: `_dot_resolve_overlays inspect` (failure returns
    /// `1`), then `_repo_status_all "$@"`. Wired by slice 82 through
    /// `run_repos`: the ported [`crate::overlays::resolve`] plus
    /// [`crate::repos_commands::status_all`]. Kernel codes cross the
    /// process boundary under `set -euo pipefail` (see
    /// [`Command::Fetch`]).
    Status,
    /// `diff`: `_dot_resolve_overlays inspect` (failure returns `1`),
    /// then `_repo_diff_all "$@"`. Wired by slice 82 through
    /// `run_repos`: the ported [`crate::overlays::resolve`] plus
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
    /// precedent, pinned against `bin/dot`). Wired by slice 83
    /// through `run_engine_arm`: the adapter child runs the shell
    /// arm body exactly as production does, while step execution
    /// (checks, workers) stays shell-owned until its slices land.
    Doctor,
    /// `test`: owner traps, `_dot_resolve_overlays inspect` (failure
    /// returns `1`), then `dot_test_command "$@"` whose status becomes
    /// the dispatcher's code (`rc=$?`). Wired by slice 83 through
    /// `run_engine_arm`: the adapter child runs the shell arm body
    /// exactly as production does (the `|| rc=$?` handoff already
    /// suppresses `errexit`, so the code crosses directly), while
    /// suite scheduling stays shell-owned until its slice lands.
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
    // Slice 84 startup prelude: `dot_config_load || exit 2` runs
    // before dispatch for every command (forward-contract order from
    // `docs/rust-port-spec.md` — the shell `case` exempts
    // help/version, but the spec requires ANY command to exit 2
    // here), preceded by the re-exec guard (exit 1). A loaded config
    // is otherwise invisible (validated, then discarded until the
    // slices consuming each field land), so wired commands behave
    // exactly as before whenever config loads.
    match crate::startup::check_ambient() {
        Ok(_) => {}
        Err(failure) => {
            let _ = stderr.write_all(failure.line().as_bytes());
            let _ = stderr.write_all(b"\n");
            return failure.code();
        }
    }
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
            Command::Update => {
                let rest: Vec<OsString> = args.collect();
                run_update(other, &rest, stdout, stderr, &mut failed)
            }
            Command::Init => {
                let rest: Vec<Vec<u8>> = args.map(|arg| argv_bytes(&arg)).collect();
                run_init(&rest, stdout, stderr, &mut failed)
            }
            command @ (Command::Fetch | Command::Push | Command::Status | Command::Diff) => {
                let rest: Vec<OsString> = args.collect();
                run_repos(command, &rest, stdout, stderr)
            }
            Command::Doctor | Command::Test => {
                let rest: Vec<OsString> = args.collect();
                run_engine_arm(other, &rest, stdout, stderr, &mut failed)
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

/// The [`Command::Update`] arm (slice 80): parse the leading flags
/// through the sequencer kernel, apply the shell loop's exports,
/// then run the update end to end and report its exit code (`0` on
/// success).
///
/// `_dot_update` (`lib/dot/update.sh`) consumes `--cron --quiet
/// -f`/`--force -v`/`--verbose` up front — exporting the
/// quiet/force/verbose pairs and unsetting `DOT_OVERLAY_LINKS_FROZEN`
/// — before the repo sync and finalize steps run. Step execution
/// stays shell-owned until its slices land, so after the flag side
/// effects this arm hands the residue to
/// [`update_run::run`](crate::update_run::run): native lock plus the
/// shell engine adapter. `command` names the invoked spelling
/// (`update` or its `pull` alias) for the adapter's original argv.
fn run_update(
    command: &[u8],
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let raw: Vec<Vec<u8>> = args.iter().map(argv_bytes).collect();
    let refs: Vec<&[u8]> = raw.iter().map(Vec::as_slice).collect();
    let parsed = crate::update::parse_update_flags(&refs);
    // Entry side effects, in shell order: rollback authority first,
    // then the flag exports. One `set_var` per variable (never a
    // batch): each entry stays auditable, matching the repo
    // differential-test convention. Process env mutation is
    // `unsafe` in edition 2024; `run` is the single-flight command
    // entry path (like the shell's own exports), so no other thread
    // observes a half-applied flag set.
    unsafe {
        std::env::remove_var("DOT_OVERLAY_LINKS_FROZEN");
        if parsed.quiet {
            std::env::set_var("DOT_QUIET", "1");
            std::env::set_var("SHDEPS_QUIET", "1");
        }
        if parsed.force {
            std::env::set_var("DOT_FORCE", "1");
            std::env::set_var("SHDEPS_FORCE", "1");
        }
        if parsed.verbose {
            std::env::set_var("DOT_VERBOSE", "1");
            std::env::set_var("SHDEPS_LOG_LEVEL", "2");
        }
    }
    // The engine's own code crosses the dispatcher: production runs
    // under `set -euo pipefail`, so the shell exits with the
    // kernel's code (`0` success, `1` failure, `2` config rejection,
    // `75` lock busy — pinned against `bin/dot`), and so does this
    // arm. Only undelivered output flips `failed`, which [`run`]
    // turns into [`EXIT_ERROR`] like the other arms.
    crate::update_run::run(command, args, stdout, stderr, failed)
}

/// Engine adapter script shared by the [`Command::Doctor`] and
/// [`Command::Test`] arms (slice 83): mirrors the `*)` arm of
/// `lib/dot/main.sh` with the final `dot_command_dispatch` kept, so
/// the shell arm bodies — owner traps, resolve gating
/// (`DOT_OVERLAY_DISCOVERY_SILENT=1` plus `_dot_resolve_overlays
/// inspect` for doctor, plain inspect for test), kernels — run
/// exactly as production runs them. `$0` is the invoked command
/// spelling (`doctor` or `test`), `$@` is the residue after it, so
/// `DOT_ORIGINAL_ARGV=("$0" "$@")` reproduces the production
/// original argv exactly (the [`update_run`](crate::update_run)
/// adapter precedent) — and the dispatch call reads the spelling
/// back out of `$0`, which `bash -c` consumes outside `"$@"`.
///
/// Two interim gaps are documented, not hidden:
///
/// - The adapter uses `${DOT_BASH:-bash}` from `PATH` instead of the
///   checkout-bash resolver: a fully-native later slice removes the
///   subprocess entirely (that slice assembles
///   [`doctor_orchestrator::run_doctor`](crate::doctor_orchestrator::run_doctor)
///   with the ported [`crate::doctor_checks`] kernels plus the
///   [`crate::test_suites`] scheduler, reusing
///   [`crate::overlay_context`] for resolution).
/// - Colors and live progress follow the child's pipes (never a tty),
///   so interactive-terminal cosmetics match a piped shell run rather
///   than a direct-to-tty one; rows and codes are unaffected.
const ENGINE_SCRIPT: &str = r#"set -euo pipefail
CDPATH=
shopt -u nocasematch
umask g-w,o-w
. "$DOT_SOURCE_ROOT/lib/dot/temp.sh"
DOT_ORIGINAL_ARGV=("$0" "$@")
if [[ -n ${DOT_REEXEC_EXPECTED_REVISION:-} ]]; then
  _dot_reexec_observed=$(_dot_source_git rev-parse HEAD 2>/dev/null || true)
  if [[ $_dot_reexec_observed != "$DOT_REEXEC_EXPECTED_REVISION" ]]; then
    printf 'dot: re-exec revision mismatch: expected %s, found %s\n' "$DOT_REEXEC_EXPECTED_REVISION" "${_dot_reexec_observed:-<missing>}" >&2
    exit 1
  fi
  unset _dot_reexec_observed
fi
. "$DOT_SOURCE_ROOT/lib/dot/public/api-version.sh"
. "$DOT_SOURCE_ROOT/lib/dot/public/xdg.sh"
. "$DOT_SOURCE_ROOT/lib/dot/public/ui.sh"
. "$DOT_SOURCE_ROOT/lib/dot/config.sh"
dot_config_load || exit 2
. "$DOT_SOURCE_ROOT/lib/dot/runtime.sh"
. "$DOT_SOURCE_ROOT/lib/dot/commands.sh"
# `bash -c` consumes the argv0-style name into `$0`, outside `"$@"`:
# dispatch takes the spelling from `$0` so the residue still forwards
# exactly like production's `dot_command_dispatch "$@"` (whose `$1`
# is the command). `DOT_ORIGINAL_ARGV` above keeps the production
# shape (`$0` first), so the `[0] == init` gates and the shdeps argv
# replay observe the invoked spelling once, never doubled.
dot_command_dispatch "$0" "$@"
"#;

/// The [`Command::Doctor`] and [`Command::Test`] arms: execute the
/// engine adapter and report its exit code.
///
/// `command` names the invoked spelling for `DOT_ORIGINAL_ARGV`;
/// `args` is the residue after it (ignored by the doctor arm, parsed
/// by `dot_test_command`). A closed pipe must not report success for
/// undelivered output, so forwarding failures flip `failed`, which
/// [`run`] turns into [`EXIT_ERROR`] like the other arms (the shell
/// dies on SIGPIPE; Rust reports failure via exit code — same signal
/// to the caller, different mechanism).
fn run_engine_arm(
    command: &[u8],
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let spelling = if command == b"test" { "test" } else { "doctor" };
    // Trampoline normalization (like `bin/dot`): a relative state
    // root must read as unset for the engine child inheriting this
    // environment. Unlike [`update_run`](crate::update_run), no
    // native step here reads XDG state, so the removal stays on the
    // child command instead of mutating the parent process.
    let relative_state = std::env::var("XDG_STATE_HOME")
        .ok()
        .is_some_and(|value| !value.is_empty() && !value.starts_with('/'));
    let program = std::env::var("DOT_BASH")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "bash".to_string());
    let root = crate::update_run::source_root();
    let mut cmd = std::process::Command::new(program);
    cmd.arg("--noprofile");
    cmd.arg("--norc");
    cmd.arg("-c");
    cmd.arg(ENGINE_SCRIPT);
    cmd.arg(spelling);
    for arg in args {
        cmd.arg(arg);
    }
    // One `env` per variable (never `envs`): each entry stays
    // auditable, matching the repo differential-test convention.
    cmd.env("DOT_SOURCE_ROOT", &root);
    if relative_state {
        cmd.env_remove("XDG_STATE_HOME");
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let output = match cmd.output() {
        Ok(output) => output,
        Err(_) => return EXIT_ERROR,
    };
    if stdout.write_all(&output.stdout).is_err() {
        *failed = true;
    }
    if stderr.write_all(&output.stderr).is_err() {
        *failed = true;
    }
    output.status.code().unwrap_or(EXIT_ERROR)
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
/// `TMPDIR` scratch, and the resume, rollback, and fresh-tail steps
/// bind the production wiring
/// ([`init_client_engine::Production`]). Only the update-engine
/// convergence stays behind its fail-closed boundary (see
/// [`init_client_engine::CONVERGE_PENDING`]) until its lanes land.
/// The arm reports the kernel's own code, matching the production
/// process under `set -euo pipefail` (pinned against `bin/dot`;
/// see the [`Command::Init`] contract).
fn run_init(
    args: &[Vec<u8>],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg_state_home = std::env::var("XDG_STATE_HOME").unwrap_or_default();
    let skip_provider = std::env::var("DOT_INIT_SKIP_PROVIDER").ok();
    // The command gate already rejected every spelling but `0` and
    // `1` before the engine runs; anything else never reaches it.
    let skip_provider_flag = skip_provider
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("0")
        == "1";
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
    // The shell inherits its working directory; a deleted one can
    // never serve the reserved probe, so fall back to the client
    // root there (fail closed on the lookup, never on the run).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(&home));
    let remote_default_branch =
        |url: &str| -> Option<String> { identity::remote_default_branch(url, &scratch) };
    let converge_pending = || -> Result<(), Error> {
        Err(Error::Usage {
            message: init_client_engine::CONVERGE_PENDING,
        })
    };
    let production = init_client_engine::Production::new(
        init_client_engine::EngineCtx {
            home: &home,
            xdg_state_home: &xdg_state_home,
            source_root,
            skip_provider: skip_provider_flag,
            cwd: &cwd,
        },
        &converge_pending,
    );
    let resume = |transaction: &Path,
                  record: &Path,
                  journal: &TransactionRecord|
     -> Result<(), Error> { production.resume(transaction, record, journal) };
    let rollback = |at: &Path| -> Result<(), Error> { production.rollback(at) };
    let fresh = |inputs: &init_client_command::FreshInputs| -> init_client_command::InitReport {
        production.run_fresh(inputs)
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

/// Base topology for the repo arms, read off the `model.sh`
/// publication at the dispatcher boundary (exactly like
/// [`run_init`] reads its ambient inputs).
///
/// `_dot_client_select` itself stays shell-owned — it reads the
/// init identity, which has no topology port yet — so this consumes
/// only what `model.sh` exports: `DOT_BASE_TOPOLOGY` (an unset or
/// foreign value reads as missing, the shell's
/// `DOT_BASE_TOPOLOGY=missing` default) and `DOT_CLIENT_GIT_DIR`
/// (empty falls back to `$HOME/.dotfiles`, like the shell's
/// `${DOT_CLIENT_GIT_DIR:-...}`). The topology slice fills in the
/// computation; until then the arm honors the environment.
fn base_from_env(home: &str) -> crate::repos_base::Base {
    let topology = match std::env::var("DOT_BASE_TOPOLOGY").ok().as_deref() {
        Some("separate") => crate::repos_base::Topology::Separate,
        Some("ordinary") => crate::repos_base::Topology::Ordinary,
        _ => crate::repos_base::Topology::Missing,
    };
    let git_dir = std::env::var("DOT_CLIENT_GIT_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
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
/// Process environment is read here — the dispatcher is the engine
/// boundary, so ambient reads live in this arm while the kernel
/// modules take explicit parameters. Resolution diagnostics replay
/// the shell's stderr (collected warnings plus the failure line,
/// when the shell prints one); kernel headers go to `stdout`,
/// overlay push warnings to `stderr`, and git's own output streams
/// to the terminal through the kernel, exactly like the shell's
/// inherited stdio. Color follows fd 1 (`[[ -t 1 ]]`), not the
/// injected stream, so piped runs stay byte-identical on both
/// sides. Extra arguments pass through to `git` verbatim.
fn run_repos(
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
    let home = std::env::var("HOME").unwrap_or_default();
    let prefix = std::env::var_os("PREFIX").unwrap_or_default();
    let prefix = prefix.to_string_lossy();
    let inputs = crate::overlays::ResolveInputs {
        home: home.clone(),
        xdg_config: std::env::var("XDG_CONFIG_HOME").unwrap_or_default(),
        discovery_silent: std::env::var("DOT_OVERLAY_DISCOVERY_SILENT")
            .map(|value| value == "1")
            .unwrap_or(false),
        default_profile: std::env::var("DOT_DEFAULT_PROFILE")
            .ok()
            .filter(|value| !value.is_empty()),
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
    let base = base_from_env(&home);
    // The shell checks `[[ -t 1 && -z ${NO_COLOR:-} ]]` on the real
    // fd 1; the injected stream may be a capture buffer, so color
    // follows the process stdout instead.
    let log = crate::log::Log::from_env(
        std::io::stdout().is_terminal(),
        std::env::var("NO_COLOR").ok().as_deref(),
        std::env::var("DOT_QUIET").ok().as_deref(),
    );
    match command {
        Command::Fetch => {
            // The shell pays the same fork (`mask=$(umask)`); the
            // fallback is unreachable without a working `sh`, where
            // git is gone too.
            let mask = crate::temp::read_umask().unwrap_or(0o022);
            crate::repos_commands::fetch_all(
                &log,
                stdout,
                &base,
                &state.overlays,
                &home,
                args,
                mask,
            )
        }
        Command::Push => crate::repos_commands::push_all(
            &log,
            stdout,
            stderr,
            &base,
            &state.overlays,
            &home,
            args,
        ),
        Command::Status => {
            crate::repos_commands::status_all(&log, stdout, &base, &state.overlays, &home, args)
        }
        Command::Diff => {
            crate::repos_commands::diff_all(&log, stdout, &base, &state.overlays, &home, args)
        }
        // Decided above; kept as generic failure, never a panic
        // (panics would break the stderr byte contract).
        _ => EXIT_ERROR,
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
    fn base_from_env_honors_model_publication() {
        // The `model.sh` publication read at the dispatcher boundary:
        // known topologies pass through, anything else (unset,
        // `missing`, foreign) reads as missing, and an empty git dir
        // falls back to `$HOME/.dotfiles` like `${VAR:-...}`.
        // Process env is shared with sibling threads, so the case
        // captures the entry state, then restores it before asserting.
        let keys = ["DOT_BASE_TOPOLOGY", "DOT_CLIENT_GIT_DIR"];
        let saved: Vec<(String, Option<OsString>)> = keys
            .iter()
            .map(|key| (key.to_string(), std::env::var_os(key)))
            .collect();
        let restore = || {
            // `unsafe` in edition 2024; the case is the only writer
            // of these keys while it runs, and it restores entry state.
            unsafe {
                for (key, value) in &saved {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        };
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
        unsafe {
            for (topology, git_dir, _, _) in cases {
                match topology {
                    Some(value) => std::env::set_var("DOT_BASE_TOPOLOGY", value),
                    None => std::env::remove_var("DOT_BASE_TOPOLOGY"),
                }
                match git_dir {
                    Some(value) => std::env::set_var("DOT_CLIENT_GIT_DIR", value),
                    None => std::env::remove_var("DOT_CLIENT_GIT_DIR"),
                }
                let base = base_from_env("/h");
                observed.push((base.topology, base.client_git_dir, base.home));
            }
        }
        restore();
        for (index, (_, _, want_topology, want_git_dir)) in cases.iter().enumerate() {
            let (got_topology, got_git_dir, got_home) = &observed[index];
            assert_eq!(got_topology, want_topology, "case: {index}");
            assert_eq!(got_git_dir, want_git_dir, "case: {index}");
            assert_eq!(got_home, "/h", "case: {index}");
        }
    }

    #[test]
    fn doctor_test_arms_execute_past_interim() {
        // Slice 83 wires the last two arms, so the interim
        // "not yet implemented" set is empty: the fallback write is
        // gone and the `run` match is exhaustive over [`Command`]
        // (the compiler rejects a new variant without a dedicated
        // arm). Execution parity lives in `tests/cli.rs` (subprocess,
        // controlled env); what stays unit-testable here is the
        // adapter contract both arms share.
        // `"$0"` carries the spelling `bash -c` consumed out of
        // `"$@"` (see the script comment); the residue still
        // forwards exactly like production.
        assert!(ENGINE_SCRIPT.contains("\ndot_command_dispatch \"$0\" \"$@\"\n"));
        assert!(ENGINE_SCRIPT.contains(". \"$DOT_SOURCE_ROOT/lib/dot/commands.sh\""));
        assert!(ENGINE_SCRIPT.contains(". \"$DOT_SOURCE_ROOT/lib/dot/runtime.sh\""));
        assert!(ENGINE_SCRIPT.contains("dot_config_load || exit 2"));
        assert!(ENGINE_SCRIPT.contains("DOT_ORIGINAL_ARGV=(\"$0\" \"$@\")"));
        // Neither arm acquires the update lock (no `init`-style
        // nested gate, no lock-busy `75`): traps and resolve gating
        // run inside the dispatched arm, like production.
        assert!(!ENGINE_SCRIPT.contains("_dot_update_lock_acquire"));
        // The silent-discovery export stays inside the shell `doctor`
        // arm (the oracle pins `SILENT:1` there); the shared prelude
        // must not leak it into `test` (`SILENT:unset`).
        assert!(!ENGINE_SCRIPT.contains("DOT_OVERLAY_DISCOVERY_SILENT"));
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
