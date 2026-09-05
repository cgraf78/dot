//! Updated-path normalization from `lib/dot/repos/pull.sh`.
//!
//! After a pull moves a generation, every updated path re-validates
//! against a pre-pull parent-identity snapshot: new parent
//! directories must either match the snapshot or take a umask
//! ceiling, and every touched file must still hash to its recorded
//! object id. Raw `diff` captures stay in memory — the shell's
//! scratch files are an unobservable implementation detail.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;

use crate::repos_base::run_git;
use crate::repos_pull_queries::terminated_records;
use crate::{repos_overlays, temp};

/// Whether `mode` is a tree-leaf mode the raw inventory accepts.
fn is_inventory_mode(mode: &str) -> bool {
    matches!(mode, "000000" | "100644" | "100755" | "120000")
}

/// Whether `oid` is a well-formed object id for the raw inventory:
/// exactly 40 or 64 hexadecimal digits (unlike the candidate
/// `{40,64}` range, the shell pins both lengths here).
fn is_inventory_oid(oid: &str) -> bool {
    (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Successful command stdout with trailing newlines stripped, like
/// shell command substitution.
fn command_text(prefix: &[OsString], args: &[&str]) -> Option<String> {
    match run_git(prefix, args) {
        Some(output) if output.status.success() => Some(
            String::from_utf8_lossy(&output.stdout)
                .trim_end_matches('\n')
                .to_string(),
        ),
        _ => None,
    }
}

/// Whether `path` is a real directory reached without a symlink,
/// like `[[ -d $path && ! -L $path ]]` (`symlink_metadata` never
/// follows a final link, so a linked directory reports false).
fn is_plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

/// Identity string of `path`, or `None` when unstatable.
fn identity_of(path: &Path) -> Option<String> {
    temp::path_identity(path).ok().map(temp::identity_string)
}

/// `_repo_snapshot_updated_path_parents`: record the device/inode
/// identity of every existing parent directory of the
/// added/modified/type-changed paths between `before` and `after`,
/// as `identity\trelative\n` lines. `None` mirrors every shell
/// failure: unresolvable `after`, a failed inventory diff, an
/// unsafe path, or a directory that moves mid-scan.
pub fn snapshot_updated_path_parents(
    prefix: &[OsString],
    root: &str,
    before: &str,
    after: &str,
) -> Option<String> {
    let after = command_text(
        prefix,
        &["rev-parse", "--verify", &format!("{after}^{{commit}}")],
    )?;
    let inventory = match run_git(
        prefix,
        &[
            "diff",
            "--name-only",
            "--diff-filter=AMT",
            "--no-renames",
            "-z",
            before,
            &after,
            "--",
        ],
    ) {
        Some(output) if output.status.success() => output.stdout,
        _ => return None,
    };
    let mut snapshot = String::new();
    for record in terminated_records(&inventory) {
        let relative = String::from_utf8_lossy(record);
        if !repos_overlays::init_safe_relative_path(&relative) {
            return None;
        }
        let Some(parent) = relative.rsplit_once('/').map(|(parent, _)| parent) else {
            continue;
        };
        let mut current = root.to_string();
        let mut relative_current = String::new();
        for component in parent.split('/') {
            current.push('/');
            current.push_str(component);
            if !relative_current.is_empty() {
                relative_current.push('/');
            }
            relative_current.push_str(component);
            let path = Path::new(&current);
            if !is_plain_dir(path) {
                break;
            }
            let identity = identity_of(path)?;
            snapshot.push_str(&identity);
            snapshot.push('\t');
            snapshot.push_str(&relative_current);
            snapshot.push('\n');
            if identity_of(path).as_deref() != Some(identity.as_str()) {
                return None;
            }
        }
    }
    Some(snapshot)
}

/// Verdict of [`snapshot_parent_status`] for one parent directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentStatus {
    /// The snapshot records the live identity (rc 0).
    Recorded,
    /// No record for this parent (rc 1): the caller clamps it.
    Absent,
    /// A malformed record (rc 2): the scan fails.
    Malformed,
}

/// `_repo_snapshot_parent_status`: whether `snapshot` records
/// `expected_relative` at `expected_identity`. A trailing
/// unterminated line is ignored like the shell's final failed `read`.
pub fn snapshot_parent_status(
    snapshot: &str,
    expected_relative: &str,
    expected_identity: &str,
) -> ParentStatus {
    // Only newline-terminated chunks are records.
    let mut found = false;
    for chunk in snapshot.split_inclusive('\n') {
        let Some(line) = chunk.strip_suffix('\n') else {
            continue;
        };
        let mut fields = line.split('\t');
        let (identity, relative, extra) = (fields.next(), fields.next(), fields.next());
        if extra.is_some_and(|extra| !extra.is_empty()) {
            return ParentStatus::Malformed;
        }
        let (identity, relative) = (identity.unwrap_or(""), relative.unwrap_or(""));
        if !is_snapshot_identity(identity) || !repos_overlays::init_safe_relative_path(relative) {
            return ParentStatus::Malformed;
        }
        if relative != expected_relative {
            continue;
        }
        if identity != expected_identity {
            return ParentStatus::Malformed;
        }
        found = true;
    }
    if found {
        ParentStatus::Recorded
    } else {
        ParentStatus::Absent
    }
}

/// Whether `identity` matches the snapshot's `dev:ino` shape.
fn is_snapshot_identity(identity: &str) -> bool {
    let Some((dev, ino)) = identity.split_once(':') else {
        return false;
    };
    !dev.is_empty()
        && !ino.is_empty()
        && dev.bytes().all(|byte| byte.is_ascii_digit())
        && ino.bytes().all(|byte| byte.is_ascii_digit())
}

/// Committed object type behind [`commit_path_type`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPathType {
    /// A file payload.
    Blob,
    /// A directory.
    Tree,
    /// No such path at this commit (rc 0 with `missing`).
    Missing,
}

/// `_repo_commit_path_type`: the object type of `relative` at
/// `commit`. `None` is any other failure (rc 1).
pub fn commit_path_type(
    prefix: &[OsString],
    commit: &str,
    relative: &str,
) -> Option<CommitPathType> {
    let spec = format!("{commit}:{relative}");
    let oid = match command_text(prefix, &["rev-parse", "--verify", &spec]) {
        Some(oid) => oid,
        None => return Some(CommitPathType::Missing),
    };
    match command_text(prefix, &["cat-file", "-t", &oid]).as_deref() {
        Some("blob") => Some(CommitPathType::Blob),
        Some("tree") => Some(CommitPathType::Tree),
        _ => None,
    }
}

/// `_repo_normalize_updated_path_parents`: every parent of
/// `relative` must be a plain directory whose `after` tree entry is
/// a tree; parents absent from `before` must either match the
/// snapshot or take a umask ceiling under `mask`.
#[allow(clippy::too_many_arguments)]
pub fn normalize_updated_path_parents(
    prefix: &[OsString],
    root: &str,
    before: &str,
    after: &str,
    relative: &str,
    snapshot: &str,
    mask: u32,
) -> bool {
    let Some(parent) = relative.rsplit_once('/').map(|(parent, _)| parent) else {
        return true;
    };
    let mut current = root.to_string();
    let mut relative_parent = String::new();
    for component in parent.split('/') {
        current.push('/');
        current.push_str(component);
        if !relative_parent.is_empty() {
            relative_parent.push('/');
        }
        relative_parent.push_str(component);
        let path = Path::new(&current);
        if !is_plain_dir(path) {
            return false;
        }
        match commit_path_type(prefix, after, &relative_parent) {
            Some(CommitPathType::Tree) => {}
            _ => return false,
        }
        match commit_path_type(prefix, before, &relative_parent) {
            Some(CommitPathType::Tree) => continue,
            None => return false,
            _ => {}
        }
        let identity = match identity_of(path) {
            Some(identity) => identity,
            None => return false,
        };
        match snapshot_parent_status(snapshot, &relative_parent, &identity) {
            ParentStatus::Recorded => {}
            ParentStatus::Absent => {
                if temp::apply_umask_ceiling(path, None, mask).is_err() {
                    return false;
                }
            }
            ParentStatus::Malformed => return false,
        }
    }
    true
}

/// `_repo_normalize_updated_path`: re-validate one updated path
/// after the pull. Symlinks validate by parents alone; regular
/// files must still hash to `oid`, take their git-mode ceiling,
/// and prove stable across both reads. A live overlay link owns
/// its path for base checkouts.
#[allow(clippy::too_many_arguments)]
pub fn normalize_updated_path(
    prefix: &[OsString],
    root: &str,
    kind: &str,
    relative: &str,
    mode: &str,
    oid: &str,
    before: &str,
    after: &str,
    snapshot: &str,
    home: &str,
    overlays: &[String],
    mask: u32,
) -> bool {
    if !repos_overlays::init_safe_relative_path(relative) {
        return false;
    }
    if !normalize_updated_path_parents(prefix, root, before, after, relative, snapshot, mask) {
        return false;
    }
    if mode == "120000" {
        return true;
    }
    if mode != "100644" && mode != "100755" {
        return false;
    }
    if kind == "base" && repos_overlays::active_link_matches(home, overlays, relative) {
        return true;
    }
    let target = Path::new(root).join(relative);
    if !std::fs::symlink_metadata(&target)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
    {
        return false;
    }
    let identity = match identity_of(&target) {
        Some(identity) => identity,
        None => return false,
    };
    match command_text(
        prefix,
        &["hash-object", "--no-filters", "--", &target_string(&target)],
    ) {
        Some(current) if current == oid => {}
        _ => return false,
    }
    let ceiling = if mode == "100755" { 0o777 } else { 0o666 };
    if temp::apply_umask_ceiling(&target, Some(ceiling), mask).is_err() {
        return false;
    }
    if identity_of(&target).as_deref() != Some(identity.as_str()) {
        return false;
    }
    matches!(
        command_text(prefix, &["hash-object", "--no-filters", "--", &target_string(&target)]),
        Some(current) if current == oid
    )
}

/// Lossy path text for git argv, like the shell's unquoted expansion.
fn target_string(target: &Path) -> String {
    target.to_string_lossy().into_owned()
}

/// `_repo_normalize_updated_paths`: validate the whole
/// `before..after` delta after the pull. Raw inventory records must
/// be well-formed, no updated path may be dirty against `after`,
/// every surviving leaf re-validates, and HEAD must still read
/// `after` at both ends.
#[allow(clippy::too_many_arguments)]
pub fn normalize_updated_paths(
    prefix: &[OsString],
    root: &str,
    kind: &str,
    before: &str,
    after: &str,
    snapshot: &str,
    home: &str,
    overlays: &[String],
    mask: u32,
) -> bool {
    if crate::repos_pull_queries::repo_head(prefix) != after {
        return false;
    }
    let inventory = match run_git(
        prefix,
        &[
            "diff",
            "--raw",
            "--no-renames",
            "--abbrev=64",
            "-z",
            before,
            after,
            "--",
        ],
    ) {
        Some(output) if output.status.success() => output.stdout,
        _ => return false,
    };
    let staged = match run_git(
        prefix,
        &["diff", "--cached", "--name-only", "-z", after, "--"],
    ) {
        Some(output) if output.status.success() => output.stdout,
        _ => return false,
    };
    let worktree = match run_git(prefix, &["diff", "--name-only", "-z", after, "--"]) {
        Some(output) if output.status.success() => output.stdout,
        _ => return false,
    };
    let staged_dirty: HashSet<&[u8]> = terminated_records(&staged).into_iter().collect();
    let worktree_dirty: HashSet<&[u8]> = terminated_records(&worktree).into_iter().collect();
    let records = terminated_records(&inventory);
    if records.len() % 2 != 0 {
        return false;
    }
    for pair in records.chunks_exact(2) {
        let header = String::from_utf8_lossy(pair[0]);
        let relative = String::from_utf8_lossy(pair[1]);
        let Some(bare) = header.strip_prefix(':') else {
            return false;
        };
        let fields: Vec<&str> = bare.split_ascii_whitespace().collect();
        if fields.len() != 5 {
            return false;
        }
        let (old_mode, new_mode, old_oid, new_oid, status) =
            (fields[0], fields[1], fields[2], fields[3], fields[4]);
        if !is_inventory_mode(old_mode)
            || !is_inventory_mode(new_mode)
            || !is_inventory_oid(old_oid)
            || !is_inventory_oid(new_oid)
            || old_oid.len() != new_oid.len()
            || status.len() != 1
            || !matches!(status, "A" | "M" | "D" | "T")
        {
            return false;
        }
        if new_mode == "000000" {
            continue;
        }
        if staged_dirty.contains(pair[1]) || worktree_dirty.contains(pair[1]) {
            return false;
        }
        if !normalize_updated_path(
            prefix, root, kind, &relative, new_mode, new_oid, before, after, snapshot, home,
            overlays, mask,
        ) {
            return false;
        }
    }
    crate::repos_pull_queries::repo_head(prefix) == after
}
