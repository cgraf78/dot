//! Pull precondition queries from `lib/dot/repos/pull.sh`.
//!
//! Thin git-inspection wrappers over caller-provided command
//! prefixes: the checked-out generation, upstream containment, and
//! generation identity. All inspection runs through
//! [`crate::repos_base::run_git`], so stdout is piped and stderr
//! nulled exactly like the slice-11 callers.

use std::ffi::OsString;

use crate::repos_base::run_git;

/// `_repo_head`: the checked-out generation (`rev-parse --verify
/// HEAD`), or empty when unresolvable — the shell's `|| true` with
/// stderr silenced. Trailing newlines strip like command
/// substitution.
pub fn repo_head(prefix: &[OsString]) -> String {
    match run_git(prefix, &["rev-parse", "--verify", "HEAD"]) {
        Some(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
        _ => String::new(),
    }
}

/// `_repo_head_contains_upstream`: whether the checked-out `head`
/// already contains `upstream`, so no new tree can arrive. Empty
/// inputs refuse; equality short-circuits without git (the common
/// case stays fork-free); otherwise `merge-base --is-ancestor`
/// probes, with stderr silenced like the shell.
pub fn repo_head_contains_upstream(prefix: &[OsString], head: &str, upstream: &str) -> bool {
    if head.is_empty() || upstream.is_empty() {
        return false;
    }
    if head == upstream {
        return true;
    }
    run_git(prefix, &["merge-base", "--is-ancestor", upstream, head])
        .is_some_and(|output| output.status.success())
}

/// `_repo_head_is`: whether the checked-out generation is exactly
/// `expected`. An empty expectation never matches (the shell's
/// `-n` gate), even against an unborn HEAD.
pub fn repo_head_is(prefix: &[OsString], expected: &str) -> bool {
    !expected.is_empty() && repo_head(prefix) == expected
}
