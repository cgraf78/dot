//! `dot update` end-to-end execution (slice 80).
//!
//! Ports the `update`/`pull` arm of `dot_command_dispatch`
//! (`lib/dot/commands.sh`): owner-trap installation, native
//! [`crate::update_lock`] acquisition (failure returns its
//! status, e.g. lock-busy `75`), then `_dot_update "$@"` whose status
//! becomes the process exit code (`0` on success).
//!
//! The flag side effects are captured in [`crate::cli`] through
//! [`parse_update_flags`](crate::update::parse_update_flags): the shell
//! loop's exports are passed to the adapter child before anything else
//! runs. The lock is fully native too ([`update_lock::acquire`]
//! with the `--cron` scan over all arguments, exactly like the
//! shell): the guard is held across the engine and released
//! explicitly, so a stolen lock is never removed and removal
//! failures warn exactly like the shell's EXIT-trap release.
//!
//! Step execution (repo sync, converge, lifecycle, links, tools,
//! merges, normalize) stays shell-owned until its slices land, so the
//! engine runs as a `bash` adapter subprocess that mirrors
//! `lib/dot/main.sh` line for line — trampoline umask, `CDPATH`,
//! `nocasematch`, `temp.sh`, `DOT_ORIGINAL_ARGV` (rebuilt as
//! `"$0" "$@"` so index zero still names the invoked spelling, which
//! `runtime.sh` and `repos/model.sh` match against `init`),
//! provider re-exec guard, API/XDG/UI/config sources,
//! `dot_config_load || exit 2`, `runtime.sh`, owner-trap
//! installation, then `_dot_update "$@"` — with the dispatcher lock
//! wrapper deliberately omitted (this process already holds the
//! lock). The child's stdout/stderr are forwarded byte for byte into
//! the injected streams and its exit code is reported, so piped runs
//! are indistinguishable from `bin/dot update`. Two interim gaps are
//! documented, not hidden:
//!
//! - The adapter uses `${DOT_BASH:-bash}` from `PATH` instead of the
//!   checkout-bash resolver: a fully-native later slice removes the
//!   subprocess entirely.
//! - Colors and live progress follow the child's pipes (never a tty),
//!   so interactive-terminal cosmetics match a piped shell run rather
//!   than a direct-to-tty one; rows and codes are unaffected.
//!
//! Overlay pull parallelism rides along unchanged: the child's
//! `_pull_overlays` fans checkouts out within the `_dot_update_jobs`
//! bound (`DOT_UPDATE_JOBS`, else the CPU count), and the native
//! equivalent ([`crate::repos_pull_fleet::pull_overlays`], scoped
//! threads under [`crate::merges::update_jobs`]) already pins the
//! same ordered replay and tally. `tests/update_parpull.rs` holds the
//! end-to-end differential contract — exit codes `0`/`1`/`2`/`75`,
//! byte-identical converged trees, and wall-clock medians — that the
//! final native wiring must preserve.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::Error;
use crate::log::Log;
use crate::update_lock;
use crate::xdg;

/// Adapter `argv[0]` for the engine subprocess: unused by the script
/// itself except to rebuild `DOT_ORIGINAL_ARGV` (see below).
const ENGINE_ARGV0: &str = "update";

/// Engine adapter script: mirrors the `*)` arm of `lib/dot/main.sh`
/// with the final `dot_command_dispatch` replaced by the lockless
/// `_dot_update` call (the caller holds the update lock natively).
/// `$0` is the invoked command spelling (`update` or `pull`), `$@`
/// is the residue after it, so `DOT_ORIGINAL_ARGV=("$0" "$@")`
/// reproduces the production original argv exactly.
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
_dot_cleanup_install_owner_traps
_dot_update "$@"
"#;

/// Resolve the XDG state home exactly like the shell bootstrap:
/// `bin/dot` unsets a relative `$XDG_STATE_HOME` before the
/// resolver runs, so relative reads as unset (HOME fallback) here
/// too. The typed XDG error carries the shell's silent status when
/// neither value yields an absolute base.
fn state_dir(env: &BTreeMap<OsString, OsString>) -> Result<PathBuf, crate::xdg::Error> {
    let raw = env
        .get(OsStr::new("XDG_STATE_HOME"))
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let xdg_value = if raw.starts_with('/') { raw } else { "" };
    let home = env
        .get(OsStr::new("HOME"))
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    xdg::base(xdg::Kind::State, xdg_value, home).map(PathBuf::from)
}

/// Whether the update runs in cron mode: the shell lock acquisition
/// scans every argument for `--cron` (not just the leading flags),
/// so this scan does the same over the raw residue.
fn is_cron(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "--cron")
}

/// Run `update`/`pull` end to end: acquire the process-wide update
/// lock natively, execute the engine adapter, release the lock, and
/// report the engine's exit code (`0` on success).
///
/// Original command spelling and its residue. Keeping argv together prevents
/// lock/stream orchestration from growing a positional parameter list.
pub struct Request<'a> {
    /// Invoked spelling for `DOT_ORIGINAL_ARGV`.
    pub command: &'a [u8],
    /// Residue after the command.
    pub args: &'a [OsString],
}

/// The adapter receives all command environment changes explicitly, so
/// concurrent callers cannot observe a half-applied update.
pub fn run(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    env: &BTreeMap<OsString, OsString>,
    request: Request<'_>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    // Trampoline normalization first (like `bin/dot`): a relative
    // state root must read as unset for both the native lock path
    // and the engine child inheriting this environment.
    let mut child_env = env.clone();
    let relative_state = child_env
        .get(OsStr::new("XDG_STATE_HOME"))
        .is_some_and(|value| !value.is_empty() && !Path::new(value).is_absolute());
    if relative_state {
        child_env.remove(OsStr::new("XDG_STATE_HOME"));
    }
    let state = match state_dir(&child_env) {
        Ok(state) => state,
        Err(error) => return error.code(),
    };
    // Lock warnings are never quiet-gated (`_warn` semantics) and the
    // injected streams are never a tty, so color is always off here —
    // exactly what the shell renders into a pipe.
    let log = Log::new(false, false);
    let prior = child_env
        .get(OsStr::new("DOT_UPDATE_LOCK_TOKEN"))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty());
    let guard = match update_lock::acquire(&state, is_cron(request.args), &log, prior, stderr) {
        Ok(guard) => guard,
        Err(Error::LockBusy { .. }) => return update_lock::EXIT_LOCK_BUSY,
        Err(_) => return crate::cli::EXIT_ERROR,
    };
    // The shell publishes the claim for nested engine steps. The adapter gets
    // the same value explicitly, without changing its parent's environment.
    child_env.insert(
        OsString::from("DOT_UPDATE_LOCK_TOKEN"),
        OsString::from(guard.token()),
    );
    let context = UpdateContext {
        runtime,
        config,
        state: &state,
        env: &child_env,
    };
    let code = run_update_or_engine(
        &context,
        request.command,
        request.args,
        stdout,
        stderr,
        failed,
    );
    // Explicit verified release (never silent removal of a lock that
    // no longer names us): removal failures warn through `log` into
    // stderr, like the shell's EXIT-trap release.
    guard.release(&log, stderr);
    code
}

/// Native update behind `DOT_UPDATE_NATIVE=1`, shell adapter
/// otherwise — and whenever the native envelope declines (the
/// engine returns `None`) or the ambient cannot be captured. The
/// flag is opt-in until differential runs prove the native driver
/// byte-identical; the shell path stays the default so behavior
/// never changes silently.
struct UpdateContext<'a> {
    runtime: &'a crate::app::Runtime,
    config: &'a crate::config::Config,
    state: &'a Path,
    env: &'a BTreeMap<OsString, OsString>,
}

fn run_update_or_engine(
    context: &UpdateContext<'_>,
    command: &[u8],
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let native = context
        .env
        .get(OsStr::new("DOT_UPDATE_NATIVE"))
        .and_then(|value| value.to_str())
        == Some("1");
    if native {
        if let Some(state_home) = context.state.to_str() {
            match crate::update_engine::gather(
                args,
                context.runtime,
                context.config,
                context.runtime.source_root(),
                state_home,
                context.env,
                context.runtime.cwd(),
            ) {
                Ok(Some(gathered)) => {
                    let inputs = gathered.inputs();
                    let now = crate::update_engine::now_secs();
                    let mut out = Vec::new();
                    let mut err = Vec::new();
                    if let Some(code) =
                        crate::update_engine::run_update(&inputs, &mut out, &mut err, now)
                    {
                        if stdout.write_all(&out).is_err() {
                            *failed = true;
                        }
                        if stderr.write_all(&err).is_err() {
                            *failed = true;
                        }
                        return code;
                    }
                }
                Ok(None) => {}
                Err(error) => return error.code(),
            }
        }
    }
    run_engine(
        command,
        args,
        context.runtime.source_root(),
        context.env,
        stdout,
        stderr,
        failed,
    )
}

/// Execute the shell engine adapter and forward its streams byte for
/// byte. A closed pipe must not report success for undelivered
/// output, so forwarding failures flip the code to generic failure
/// (the shell dies on SIGPIPE; Rust reports failure via exit code —
/// same signal to the caller, different mechanism).
fn run_engine(
    command: &[u8],
    args: &[OsString],
    root: &Path,
    env: &BTreeMap<OsString, OsString>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let spelling = if command == b"pull" {
        "pull"
    } else {
        ENGINE_ARGV0
    };
    let program = env
        .get(OsStr::new("DOT_BASH"))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("bash");
    let mut cmd = Command::new(program);
    cmd.arg("--noprofile");
    cmd.arg("--norc");
    cmd.arg("-c");
    cmd.arg(ENGINE_SCRIPT);
    cmd.arg(spelling);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env_clear();
    cmd.envs(env);
    cmd.env("DOT_SOURCE_ROOT", root);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let output = match cmd.output() {
        Ok(output) => output,
        Err(_) => return crate::cli::EXIT_ERROR,
    };
    if stdout.write_all(&output.stdout).is_err() {
        *failed = true;
    }
    if stderr.write_all(&output.stderr).is_err() {
        *failed = true;
    }
    output.status.code().unwrap_or(crate::cli::EXIT_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_scan_covers_every_argument_like_shell() {
        // The shell lock arm scans all of `"$@"`, not just the
        // leading flags the update loop consumes.
        let flagged = |parts: &[&str]| parts.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(is_cron(&flagged(&["--cron"])));
        assert!(is_cron(&flagged(&["--quiet", "--cron"])));
        assert!(is_cron(&flagged(&["extra", "--cron"])));
        assert!(!is_cron(&flagged(&[])));
        assert!(!is_cron(&flagged(&["--quiet"])));
        assert!(!is_cron(&flagged(&["--cronish"])));
    }

    #[test]
    fn relative_state_home_reads_as_unset_like_trampoline() {
        // `bin/dot` unsets a relative `$XDG_STATE_HOME` before the
        // resolver runs; anything else would make lock ownership
        // depend on cwd.
        let env = BTreeMap::from([
            (
                OsString::from("XDG_STATE_HOME"),
                OsString::from("relative/state"),
            ),
            (OsString::from("HOME"), OsString::from("/home/fixture")),
        ]);
        let resolved = state_dir(&env).expect("relative state falls back to home");
        assert_eq!(resolved, PathBuf::from("/home/fixture/.local/state"));
    }

    #[test]
    fn engine_script_calls_update_without_the_lock_wrapper() {
        // The native guard owns the lock across the child, so the
        // adapter must never re-acquire (a second pid would read the
        // live owner and refuse with 75).
        assert!(ENGINE_SCRIPT.contains("\n_dot_update \"$@\"\n"));
        assert!(!ENGINE_SCRIPT.contains("_dot_update_lock_acquire"));
        assert!(ENGINE_SCRIPT.contains("dot_config_load || exit 2"));
        assert!(ENGINE_SCRIPT.contains("DOT_ORIGINAL_ARGV=(\"$0\" \"$@\")"));
    }
}
