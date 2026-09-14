//! Slice-11 config track: `lib/dot/repos/config.sh` port.
//!
//! `is_worktree`, `effective_url`, and the origin-comparison logic
//! are owned by [`crate::overlays`] (reused here, not duplicated);
//! this module adds the remaining `config.sh` helpers
//! (`_repo_has_upstream`, `_overlay_origin_matches` shape,
//! `_ensure_repo_config`) on [`crate::repos_base::run_git`].

use std::ffi::OsString;
use std::path::Path;

/// `_overlay_is_worktree` / `_overlay_effective_url`, owned by
/// [`crate::overlays`] and re-exported for config-track callers.
pub use crate::overlays::{effective_url, is_worktree};

/// `_repo_has_upstream`: `"$@" rev-parse --abbrev-ref
/// --symbolic-full-name '@{u}'`, true iff git exits 0 (stdout
/// ignored; both engines silence it).
pub fn has_upstream(prefix: &[OsString]) -> bool {
    crate::repos_base::run_git(
        prefix,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_some_and(|output| output.status.success())
}

/// `_overlay_origin_matches` adapter: `Ok(url)` reads `(true, url)`
/// and `Err(diagnostic)` reads `(false, diagnostic)`, exactly the
/// shell `REPLY`-plus-exit-status shape (`<missing>` when origin is
/// absent, the recorded URL on a single entry whether or not it
/// matches, `<multiple origin URLs>` when ambiguous).
pub fn origin_matches(path: &Path, expected: &str) -> (bool, String) {
    match crate::overlays::origin_matches(path, expected) {
        Ok(url) => (true, url),
        Err(diagnostic) => (false, diagnostic),
    }
}

/// `_ensure_repo_config`: always succeeds and prints nothing. A
/// `None` base (shell: missing topology, `_base_repo_exists`
/// false) does nothing. Otherwise each key is read first —
/// `config --bool core.fsmonitor` but plain `config
/// status.showUntrackedFiles` (the missing `--bool` on the second
/// key is shell-faithful, not an oversight) — a failed read counts
/// as `""` like the shell `$(... || true)`, and a value other than
/// the target is rewritten with `config <key> <target>`, ignoring
/// all errors like the shell `|| true`.
pub fn ensure_repo_config(base: Option<&[OsString]>) {
    let Some(prefix) = base else {
        return;
    };
    for (key, target, boolean) in [
        ("core.fsmonitor", "false", true),
        ("status.showUntrackedFiles", "no", false),
    ] {
        let current = if boolean {
            crate::repos_base::run_git(prefix, &["config", "--bool", key])
        } else {
            crate::repos_base::run_git(prefix, &["config", key])
        }
        .filter(|output| output.status.success())
        .map(|output| {
            // Shell `$(...)` strips trailing newlines only.
            String::from_utf8_lossy(&output.stdout)
                .trim_end_matches('\n')
                .to_string()
        })
        .unwrap_or_default();
        if current != target {
            let _ = crate::repos_base::run_git(prefix, &["config", key, target]);
        }
    }
}
