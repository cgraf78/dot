//! `dot update` end-to-end execution (slice 80).
//!
//! Ports the `update`/`pull` arm of `dot_command_dispatch`
//! (`lib/dot/commands.sh`): owner-trap installation, native
//! [`crate::update_lock`] acquisition (failure returns its
//! status, e.g. lock-busy `75`), then `_dot_update "$@"` whose status
//! becomes the process exit code (`0` on success).
//!
//! The flag side effects stay native in [`crate::cli`] through
//! [`parse_update_flags`](crate::update::parse_update_flags): the shell
//! loop's exports land in the process environment before anything
//! else runs. The lock is fully native too ([`update_lock::acquire`]
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

use std::ffi::OsString;
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

/// Resolve the source checkout carrying `bin/dot` and `lib/dot`:
/// `$DOT_SOURCE_ROOT` when non-empty, otherwise the checkout this
/// binary was built from. Tests set `DOT_SOURCE_ROOT` explicitly;
/// production follows the built-in checkout until the install
/// layout owns the engine natively.
pub fn source_root() -> PathBuf {
    let from_env = std::env::var_os("DOT_SOURCE_ROOT").unwrap_or_default();
    if from_env.is_empty() {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    } else {
        PathBuf::from(from_env)
    }
}

/// Resolve the XDG state home exactly like the shell bootstrap:
/// `bin/dot` unsets a relative `$XDG_STATE_HOME` before the
/// resolver runs, so relative reads as unset (HOME fallback) here
/// too. Returns `None` when neither yields an absolute base, which
/// the shell's `_dot_update_lock_path` reports as a silent `1`.
fn state_dir() -> Option<PathBuf> {
    let raw = std::env::var("XDG_STATE_HOME").unwrap_or_default();
    let xdg_value = if raw.starts_with('/') {
        raw
    } else {
        String::new()
    };
    let home = std::env::var("HOME").unwrap_or_default();
    xdg::base(xdg::Kind::State, &xdg_value, &home)
        .ok()
        .map(PathBuf::from)
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
/// `command` names the invoked spelling for `DOT_ORIGINAL_ARGV`;
/// `args` is the residue after it. Process environment mutation is
/// `unsafe` in edition 2024; this runs on the single-flight command
/// entry path (like the shell's own exports), so no other thread
/// observes a half-applied update.
pub fn run(
    command: &[u8],
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    // Trampoline normalization first (like `bin/dot`): a relative
    // state root must read as unset for both the native lock path
    // and the engine child inheriting this environment.
    let relative_state = std::env::var("XDG_STATE_HOME")
        .ok()
        .is_some_and(|value| !value.is_empty() && !value.starts_with('/'));
    if relative_state {
        // `unsafe` (see above): single-flight entry path.
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
    let Some(state) = state_dir() else {
        return crate::cli::EXIT_ERROR;
    };
    // Lock warnings are never quiet-gated (`_warn` semantics) and the
    // injected streams are never a tty, so color is always off here —
    // exactly what the shell renders into a pipe.
    let log = Log::new(false, false);
    let prior_token = std::env::var("DOT_UPDATE_LOCK_TOKEN").unwrap_or_default();
    let prior = if prior_token.is_empty() {
        None
    } else {
        Some(prior_token.as_str())
    };
    let guard = match update_lock::acquire(&state, is_cron(args), &log, prior, stderr) {
        Ok(guard) => guard,
        Err(Error::LockBusy { .. }) => return update_lock::EXIT_LOCK_BUSY,
        Err(_) => return crate::cli::EXIT_ERROR,
    };
    // Publish the claim for nested engine steps (the shell exports
    // `DOT_UPDATE_LOCK_TOKEN` on acquisition); the child inherits
    // this environment.
    unsafe {
        std::env::set_var("DOT_UPDATE_LOCK_TOKEN", guard.token());
    }
    let root = source_root();
    let code = run_update_or_engine(command, args, &root, &state, stdout, stderr, failed);
    // Explicit verified release (never silent removal of a lock that
    // no longer names us): removal failures warn through `log` into
    // stderr, like the shell's EXIT-trap release.
    guard.release(&log, stderr);
    unsafe {
        std::env::remove_var("DOT_UPDATE_LOCK_TOKEN");
    }
    code
}

/// Native update behind `DOT_UPDATE_NATIVE=1`, shell adapter
/// otherwise — and whenever the native envelope declines (the
/// engine returns `None`) or the ambient cannot be captured. The
/// flag is opt-in until differential runs prove the native driver
/// byte-identical; the shell path stays the default so behavior
/// never changes silently.
fn run_update_or_engine(
    command: &[u8],
    args: &[OsString],
    root: &Path,
    state: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let native = std::env::var("DOT_UPDATE_NATIVE").ok().as_deref() == Some("1");
    if native {
        if let Some(state_home) = state.to_str() {
            if let Some(gathered) = crate::update_engine::gather(args, root, state_home) {
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
        }
    }
    run_engine(command, args, root, stdout, stderr, failed)
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
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    failed: &mut bool,
) -> i32 {
    let spelling = if command == b"pull" {
        "pull"
    } else {
        ENGINE_ARGV0
    };
    let program = std::env::var("DOT_BASH")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "bash".to_string());
    let mut cmd = Command::new(program);
    cmd.arg("--noprofile");
    cmd.arg("--norc");
    cmd.arg("-c");
    cmd.arg(ENGINE_SCRIPT);
    cmd.arg(spelling);
    for arg in args {
        cmd.arg(arg);
    }
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
        let saved_xdg = std::env::var_os("XDG_STATE_HOME");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", "relative/state");
            std::env::set_var("HOME", "/home/fixture");
        }
        let resolved = state_dir();
        unsafe {
            match saved_xdg {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            match saved_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(resolved, Some(PathBuf::from("/home/fixture/.local/state")));
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
