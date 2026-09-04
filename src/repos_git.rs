//! Repo-set iteration and Git invocation helpers (`lib/dot/repos/git.sh`).
//!
//! The base client may use a separate Git directory with `$HOME` as its
//! work tree or an ordinary checkout rooted at `$HOME`, while overlays
//! are ordinary Git repositories. This module centralizes that topology
//! dispatch so higher-level operations work with repo records instead
//! of reimplementing those command shapes.
//!
//! Engine boundaries: iteration records cross from shell globals as
//! explicit parameters; streaming commands inherit stdout/stderr so
//! push/diff/status/fetch output reaches the terminal (unlike
//! [`repos_base::run_git`](crate::repos_base::run_git), which pipes
//! stdout and nulls stderr for inspection commands).

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::repos_base::{Base, RepoKind, overlay_path_sync};

/// `_repo_each_existing`: run `callback` for every repo that already
/// exists locally — the base client repo first, then cloned overlays
/// in discovery order.
///
/// The base record is `(Base, "dotfiles", home, "")` and is emitted
/// only when [`Base::exists`] holds. Each overlay entry parses as
/// `name|path|url|...|sync` (missing fields read empty, like the
/// shell `read`; `sync` follows the [`overlay_path_sync`] remainder
/// rule and defaults to `git`); entries whose sync is not exactly
/// `git` and paths failing [`is_worktree`](crate::overlays::is_worktree)
/// are skipped, so missing overlays never reach the callback. The
/// first nonzero callback return short-circuits (shell `|| return $?`);
/// returns 0 when every record succeeds or is skipped.
///
/// The callback spells out the shell's five fixed operands
/// (`kind name path url extra`) rather than hiding them behind an
/// alias, so each call site reads like the shell invocation.
#[allow(clippy::type_complexity)]
pub fn each_existing(
    base: &Base,
    overlays: &[String],
    home: &str,
    args: &[OsString],
    callback: &mut dyn FnMut(RepoKind, &str, &str, &str, &[OsString]) -> i32,
) -> i32 {
    if base.exists() {
        let rc = callback(RepoKind::Base, "dotfiles", home, "", args);
        if rc != 0 {
            return rc;
        }
    }
    for entry in overlays {
        let fields: Vec<&str> = entry.split('|').collect();
        let name = fields.first().copied().unwrap_or("");
        let url = fields.get(2).copied().unwrap_or("");
        let (path, sync) = overlay_path_sync(entry);
        if sync != "git" {
            continue;
        }
        if !crate::overlays::is_worktree(Path::new(&path)) {
            continue;
        }
        let rc = callback(RepoKind::Overlay, name, &path, url, args);
        if rc != 0 {
            return rc;
        }
    }
    0
}

/// Run `git` with `prefix` plus `args`, streaming to the terminal:
/// stdin null, stdout/stderr inherited. No `run_git`-style capture
/// exists with inherited stdio anywhere in `src/`, so this runner
/// lives here beside its callers. Returns the exit code; a spawn
/// failure (no `git` on `PATH`) returns 127.
pub fn run_git_streaming(prefix: &[OsString], args: &[&str]) -> i32 {
    let mut cmd = Command::new("git");
    cmd.args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(127),
        Err(_) => 127,
    }
}

/// `_repo_git`: execute `git` for one repo record. A base record
/// dispatches through [`Base::git_prefix`] (a missing topology has
/// no prefix, like the shell's exit 128); an overlay record runs
/// `git -C path`. The shell's `*) -> return 2` arm is unreachable
/// through the [`RepoKind`] enum, so it has no representation here.
pub fn repo_git(base: &Base, kind: RepoKind, path: &str, args: &[&str]) -> i32 {
    match kind {
        RepoKind::Base => match base.git_prefix() {
            Some(prefix) => run_git_streaming(&prefix, args),
            None => 128,
        },
        RepoKind::Overlay => run_git_streaming(&[OsString::from("-C"), OsString::from(path)], args),
    }
}

/// `_repo_git_fetch`: run `fetch` plus `extra` for one repo record,
/// then close the `FETCH_HEAD` side effect Git leaves behind (Git
/// does not apply `core.sharedRepository` to that scratch file).
///
/// The fetch exit code is recorded and returned last: every later
/// failure (unresolvable git dir, a rejected `FETCH_HEAD`, a failed
/// clamp) returns 1 and discards it, exactly like the shell. The git
/// dir resolves through `rev-parse --absolute-git-dir` with stderr
/// nulled and stdout captured (a fetch failure there still resolves:
/// only a rev-parse failure returns 1); trailing newlines strip like
/// shell command substitution, with the usual lossy-conversion
/// boundary for non-UTF8 paths. When `$gitdir/FETCH_HEAD`
/// exists-or-is-a-symlink (shell `[[ -e || -L ]]`, read here with one
/// `symlink_metadata` covering live paths and dangling links alike),
/// it must be a regular file, never a symlink, and owned by the
/// caller (the euid comes from [`current_uid`](crate::temp::current_uid),
/// the same `id -u` source the crate's other ownership gates use) —
/// anything else returns 1. The clamp is
/// [`apply_umask_ceiling`](crate::temp::apply_umask_ceiling) to
/// `0600` under the caller's `mask` (the shell reads its own umask;
/// callers pass [`read_umask`](crate::temp::read_umask)); a clamp
/// failure returns 1.
pub fn repo_git_fetch(base: &Base, kind: RepoKind, path: &str, extra: &[&str], mask: u32) -> i32 {
    let mut fetch: Vec<&str> = Vec::with_capacity(extra.len() + 1);
    fetch.push("fetch");
    fetch.extend_from_slice(extra);
    let rc = repo_git(base, kind, path, &fetch);
    let prefix: Vec<OsString> = match kind {
        RepoKind::Base => match base.git_prefix() {
            Some(prefix) => prefix,
            None => return 1,
        },
        RepoKind::Overlay => vec![OsString::from("-C"), OsString::from(path)],
    };
    let output = match crate::repos_base::run_git(&prefix, &["rev-parse", "--absolute-git-dir"]) {
        Some(output) if output.status.success() => output,
        _ => return 1,
    };
    let git_dir = String::from_utf8_lossy(&output.stdout);
    let fetch_head = Path::new(git_dir.trim_end_matches('\n')).join("FETCH_HEAD");
    match std::fs::symlink_metadata(&fetch_head) {
        // Absent entirely: no side effect to close, keep fetch's rc.
        Err(_) => rc,
        Ok(meta) => {
            let owned = match crate::temp::current_uid() {
                Some(uid) => meta.uid() == uid,
                // Owner unknowable: fail closed, like a failed `-O`.
                None => return 1,
            };
            if !meta.file_type().is_file() || meta.file_type().is_symlink() || !owned {
                return 1;
            }
            if crate::temp::apply_umask_ceiling(&fetch_head, Some(0o600), mask).is_err() {
                return 1;
            }
            rc
        }
    }
}
