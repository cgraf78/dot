//! Dirty-worktree detection and normalization from
//! `lib/dot/repos/dirty.sh` for `dot update --cron`.
//!
//! Every function takes the git command prefix explicitly (a base
//! [`crate::repos_base::Base`] prefix or `["-C", path]` for an
//! overlay), mirroring the shell's `"$@"` prefix passing. Overlay
//! records parse through
//! [`crate::repos_base::overlay_path_sync`]; worktree checks reuse
//! [`crate::overlays::is_worktree`].
//!
//! Engine boundaries: [`crate::repos_base::run_git`] nulls stdin
//! and stderr and pipes stdout, matching the shell's `2>/dev/null`
//! inspection calls. `_normalize_filtered` evaluates sequentially:
//! the shell fans out across a cleanup-registry process group when
//! several repos qualify, but the probes are silent and every
//! failure is ignored, so the fan-out is unobservable (no output,
//! always success).

use std::ffi::OsString;
use std::path::Path;

use crate::overlays::is_worktree;
use crate::repos_base::{RepoKind, overlay_path_sync, run_git};

/// A quiet boolean git probe: success decides, output ignored.
fn git_quiet_ok(prefix: &[OsString], args: &[&str]) -> bool {
    run_git(prefix, args).is_some_and(|output| output.status.success())
}

/// `$(prefix args)` as lines: trailing newlines stripped like
/// command substitution, empty output reads zero lines (the
/// `mapfile`-empty precedent), other lines kept verbatim.
fn git_lines(prefix: &[OsString], args: &[&str]) -> Vec<String> {
    let output = match run_git(prefix, args) {
        Some(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('\n').map(str::to_string).collect()
    }
}

/// `_is_worktree_dirty`: uncommitted changes in the base repository
/// or in any git-synchronized overlay worktree.
pub fn is_worktree_dirty(base: Option<&[OsString]>, overlays: &[String]) -> bool {
    if let Some(prefix) = base {
        if !git_quiet_ok(prefix, &["diff-index", "--quiet", "HEAD"]) {
            return true;
        }
    }
    for entry in overlays {
        let (path, sync) = overlay_path_sync(entry);
        if sync != "git" {
            continue;
        }
        if !is_worktree(Path::new(&path)) {
            continue;
        }
        let prefix = [OsString::from("-C"), OsString::from(&path)];
        if !git_quiet_ok(&prefix, &["diff-index", "--quiet", "HEAD"]) {
            return true;
        }
    }
    false
}

/// `_checkout_dirty_files`: revert exactly the currently-dirty
/// tracked files, one at a time, ignoring per-file errors. A failed
/// listing reads empty, so nothing happens.
pub fn checkout_dirty_files(prefix: &[OsString]) {
    for file in git_lines(prefix, &["diff-index", "--name-only", "HEAD"]) {
        if file.is_empty() {
            continue;
        }
        let _ = run_git(prefix, &["checkout", "--", file.as_str()]);
    }
}

/// Resolve one dirty repository: fetch its upstream and check out
/// the dirty files only when every one matches the remote.
/// Returns true when the repository is clean afterwards.
fn resolve_one(worktree: &str, prefix: &[OsString]) -> bool {
    if git_quiet_ok(prefix, &["diff-index", "--quiet", "HEAD"]) {
        return true;
    }
    let upstream = configured_upstream(prefix).unwrap_or_default();
    if !upstream.is_empty() {
        let remote = upstream.split('/').next().unwrap_or("");
        let _ = run_git(
            prefix,
            &["fetch", "--quiet", "--no-write-fetch-head", remote],
        );
    }
    if !upstream.is_empty() && dirty_files_match_ref(worktree, &upstream, prefix) {
        checkout_dirty_files(prefix);
        true
    } else {
        false
    }
}

/// `_try_resolve_dirty`: repair dirty worktrees whose files exactly
/// match the configured upstream, across the base repository and
/// git-synchronized overlays. Real local edits keep the tree dirty.
/// Returns true when every repository is clean afterwards.
pub fn try_resolve_dirty(home: &str, base: Option<&[OsString]>, overlays: &[String]) -> bool {
    let mut clean = match base {
        Some(prefix) => resolve_one(home, prefix),
        None => true,
    };
    for entry in overlays {
        let (path, sync) = overlay_path_sync(entry);
        if sync != "git" {
            continue;
        }
        if !is_worktree(Path::new(&path)) {
            continue;
        }
        let prefix = [OsString::from("-C"), OsString::from(&path)];
        if !resolve_one(&path, &prefix) {
            clean = false;
        }
    }
    clean
}

/// `_repo_configured_upstream`: the `remote/branch` upstream, or
/// `None` when absent, bare, or unparseable. The remote is the part
/// before the first `/` (`${upstream%%/*}`), which must differ from
/// the whole.
pub fn configured_upstream(prefix: &[OsString]) -> Option<String> {
    let output = run_git(
        prefix,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let upstream = text.trim_end_matches('\n');
    let remote = upstream.split('/').next().unwrap_or("");
    if remote.is_empty() || remote == upstream {
        return None;
    }
    Some(upstream.to_string())
}

/// `_dirty_files_match_ref`: every dirty file's worktree content
/// hashes equal to its content on `remote_ref`. A failed listing or
/// an unverifiable ref refuses. Empty lines are NOT skipped, and an
/// empty listing still runs one empty iteration (`<<<` always feeds
/// a newline): hashing `"$worktree/"` fails, so clean trees refuse
/// like the shell.
pub fn dirty_files_match_ref(worktree: &str, remote_ref: &str, prefix: &[OsString]) -> bool {
    let files = match run_git(prefix, &["diff-index", "--name-only", "HEAD"]) {
        Some(output) if output.status.success() => output.stdout,
        _ => return false,
    };
    if !git_quiet_ok(prefix, &["rev-parse", "--verify", remote_ref]) {
        return false;
    }
    let text = String::from_utf8_lossy(&files);
    let trimmed = text.trim_end_matches('\n');
    let files: Vec<&str> = if trimmed.is_empty() {
        // One empty iteration, exactly like the shell.
        vec![""]
    } else {
        trimmed.split('\n').collect()
    };
    for file in files {
        let target = format!("{worktree}/{file}");
        let work = run_git(prefix, &["hash-object", target.as_str()]);
        let remote = run_git(
            prefix,
            &["rev-parse", format!("{remote_ref}:{file}").as_str()],
        );
        match (work, remote) {
            (Some(work), Some(remote)) if work.status.success() && remote.status.success() => {
                if work.stdout != remote.stdout {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// `_dirty_files_match_remote`: the base repository's dirty files
/// against its configured upstream. No base or no upstream refuses.
pub fn dirty_files_match_remote(home: &str, base: Option<&[OsString]>) -> bool {
    let prefix = match base {
        Some(prefix) => prefix,
        None => return false,
    };
    let upstream = match configured_upstream(prefix) {
        Some(upstream) => upstream,
        None => return false,
    };
    dirty_files_match_ref(home, &upstream, prefix)
}

/// `_normalize_dirty_files`: re-checkout the stat-dirty-but-content-
/// clean (mtime-only) files. Files with real content differences are
/// left alone; listing failures and empty listings do nothing.
pub fn normalize_dirty_files(prefix: &[OsString]) {
    for file in git_lines(prefix, &["diff-files", "--name-only"]) {
        if file.is_empty() {
            continue;
        }
        if git_quiet_ok(prefix, &["diff", "--quiet", "--", file.as_str()]) {
            let _ = run_git(prefix, &["checkout", "--", file.as_str()]);
        }
    }
}

/// `_normalize_repo`: normalize one repository by kind. A missing
/// base is a no-op (the shell's `_base_git` fails, failing the
/// listing the same way).
pub fn normalize_repo(kind: RepoKind, path: &str, base: Option<&[OsString]>) {
    match kind {
        RepoKind::Base => {
            if let Some(prefix) = base {
                normalize_dirty_files(prefix);
            }
        }
        RepoKind::Overlay => {
            let prefix = [OsString::from("-C"), OsString::from(path)];
            normalize_dirty_files(&prefix);
        }
    }
}

/// `_normalize_filtered`: normalize the base repository and every
/// git-synchronized overlay worktree. Always succeeds silently.
pub fn normalize_filtered(base: Option<&[OsString]>, overlays: &[String]) {
    if base.is_some() {
        normalize_repo(RepoKind::Base, "", base);
    }
    for entry in overlays {
        let (path, sync) = overlay_path_sync(entry);
        if sync != "git" {
            continue;
        }
        if !is_worktree(Path::new(&path)) {
            continue;
        }
        normalize_repo(RepoKind::Overlay, &path, base);
    }
}
