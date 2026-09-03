//! Base client repository dispatch from `lib/dot/repos/model.sh`.
//!
//! The topology globals (`DOT_BASE_TOPOLOGY`, `DOT_CLIENT_GIT_DIR`,
//! `$HOME`) arrive as explicit parameters. Only the dispatch half
//! of `model.sh` lives here: `_dot_client_select` stays
//! shell-side because it reads the init identity (`init.sh` is not
//! ported yet). Missing topology refuses like the shell exit 128.
//!
//! Engine boundaries: git inspection commands never read stdin, so
//! [`run_git`] nulls it; every slice-11 caller redirects stderr to
//! `/dev/null`, so it is always nulled and stdout is always piped.

use std::ffi::OsString;
use std::process::{Command, Output, Stdio};

/// `_base_repo_exists` shape: which `git` command form addresses
/// the base client repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// `missing`: no base repository selected.
    Missing,
    /// `separate`: detached git dir with `$HOME` as the work tree.
    Separate,
    /// `ordinary`: plain checkout rooted at `$HOME`.
    Ordinary,
}

/// `_normalize_repo` dispatch: base repository or one overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    /// The base client repository.
    Base,
    /// One overlay checkout.
    Overlay,
}

/// Explicit caller state for base dispatch: the selected topology,
/// the client git directory, and home.
#[derive(Debug, Clone)]
pub struct Base {
    /// Selected topology (`DOT_BASE_TOPOLOGY`).
    pub topology: Topology,
    /// Client git directory (`DOT_CLIENT_GIT_DIR`).
    pub client_git_dir: String,
    /// Home directory (`HOME`), the work tree for both topologies.
    pub home: String,
}

impl Base {
    /// `_base_repo_exists`: any topology but `missing`.
    pub fn exists(&self) -> bool {
        !matches!(self.topology, Topology::Missing)
    }

    /// `_base_git` argv prefix for `git`, or `None` when the
    /// topology is missing (shell exit 128). The `--opt=value`
    /// spelling equals the shell's `--opt value` spelling.
    pub fn git_prefix(&self) -> Option<Vec<OsString>> {
        match self.topology {
            Topology::Missing => None,
            Topology::Separate => Some(vec![
                OsString::from(format!("--git-dir={}", self.client_git_dir)),
                OsString::from(format!("--work-tree={}", self.home)),
            ]),
            Topology::Ordinary => Some(vec![OsString::from("-C"), OsString::from(&self.home)]),
        }
    }
}

/// `(path, sync)` from an overlay record (`name|path|url|...|sync`).
/// Missing fields read empty like shell `read`; `sync` defaults to
/// `git` like `${sync:-git}`. A seventh field stays glued to `sync`
/// (`read` parks the remainder in the last variable), so a record
/// like `n|p|u|d|o|git|x` does not match `git`.
pub fn overlay_path_sync(entry: &str) -> (String, String) {
    let fields: Vec<&str> = entry.split('|').collect();
    let path = fields.get(1).copied().unwrap_or("").to_string();
    let rest = if fields.len() > 5 {
        fields[5..].join("|")
    } else {
        String::new()
    };
    let sync = if rest.is_empty() {
        "git".to_string()
    } else {
        rest
    };
    (path, sync)
}

/// Run `git` with `prefix` plus `args`: stdout piped, stderr
/// nulled, stdin null. `None` on spawn failure (callers treat that
/// like any other git failure).
pub fn run_git(prefix: &[OsString], args: &[&str]) -> Option<Output> {
    let mut cmd = Command::new("git");
    cmd.args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.output().ok()
}
