//! `dot update` end-to-end execution.
//!
//! Owns the `update`/`pull` command dispatch
//! (`lib/dot/commands.sh`): owner-trap installation, native
//! [`crate::update_lock`] acquisition (failure returns its
//! status, e.g. lock-busy `75`), then `_dot_update "$@"` whose status
//! becomes the process exit code (`0` on success).
//!
//! The flag side effects are captured in [`crate::cli`] through
//! [`parse_update_flags`](crate::update::parse_update_flags). The lock is
//! fully native ([`update_lock::acquire`]
//! with the `--cron` scan over all arguments, exactly like the
//! shell): the guard is held across the native engine and released explicitly,
//! so a stolen lock is never removed and removal failures warn exactly like
//! the shell's EXIT-trap release.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::log::Log;
use crate::update_lock;
use crate::xdg;

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
/// lock natively, execute the native engine, release the lock, and
/// report the engine's exit code (`0` on success).
///
/// Original command spelling and its residue. Keeping argv together prevents
/// lock/stream orchestration from growing a positional parameter list.
pub struct Request<'a> {
    /// Residue after the command.
    pub args: &'a [OsString],
}

/// The engine receives all command environment changes explicitly, so
/// concurrent callers cannot observe a half-applied update.
pub fn run(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    env: &BTreeMap<OsString, OsString>,
    request: Request<'_>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
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
    // Publish the claim explicitly for nested native steps without changing
    // the parent process environment.
    child_env.insert(
        OsString::from("DOT_UPDATE_LOCK_TOKEN"),
        OsString::from(guard.token()),
    );
    let mut streams = crate::app::Streams::new(stdout, stderr);
    let code = crate::update_engine::run_update(
        runtime,
        &crate::update_engine::UpdateRequest {
            config,
            env: &child_env,
            args: request.args,
            state_home: &state,
        },
        &mut streams,
    );
    // Explicit verified release (never silent removal of a lock that
    // no longer names us): removal failures warn through `log` into
    // stderr, like the shell's EXIT-trap release.
    guard.release(&log, stderr);
    code
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
}
