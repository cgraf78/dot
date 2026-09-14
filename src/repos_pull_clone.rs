//! The staged-clone family (`lib/dot/repos/pull.sh`): cloned path
//! modes, cloned mode normalization, the matches-commit gate, and
//! the staged clone orchestrator.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::cleanup::Registry;
use crate::log::Log;
use crate::repos_base::run_git;
use crate::repos_overlays::init_safe_relative_path;
use crate::repos_pull_queries::{
    CandidateEnv, repo_head, terminated_records, validate_candidate_tree,
};
use crate::temp::{
    MoveCache, apply_git_metadata_modes, apply_tracked_file_mode, apply_umask_ceiling, file_digest,
    file_text_digest, move_noreplace_cached,
};

/// `git -C root` prefix for the staged helpers.
fn stage_prefix(root: &str) -> [OsString; 2] {
    [OsString::from("-C"), OsString::from(root)]
}

/// Whether `mode` is a tree-leaf mode the clone inventory accepts.
fn is_clone_mode(mode: &str) -> bool {
    matches!(mode, "100644" | "100755" | "120000")
}

/// Whether `oid` is a well-formed object id (40 or 64 hex digits).
fn is_clone_oid(oid: &str) -> bool {
    (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Link-target bytes for `hash-object --stdin`, like command
/// substitution stripping every trailing newline before the target
/// reaches git.
fn link_bytes(target: &Path) -> Option<Vec<u8>> {
    let raw = std::fs::read_link(target).ok()?;
    let mut bytes = raw.as_os_str().as_bytes();
    while let Some(rest) = bytes.strip_suffix(b"\n".as_slice()) {
        bytes = rest;
    }
    Some(bytes.to_vec())
}

/// `_repo_cloned_overlay_path_modes`: check one staged worktree
/// path against its committed mode and object id, repairing parent
/// ceilings and tracked file modes along the way. Every ancestor
/// directory must exist as a real directory first.
pub fn cloned_overlay_path_modes(
    root: &str,
    relative: &str,
    mode: &str,
    oid: &str,
    mask: u32,
) -> bool {
    if !init_safe_relative_path(relative) {
        return false;
    }
    let root_path = Path::new(root);
    if let Some((parent, _)) = relative.rsplit_once('/') {
        let mut current = PathBuf::new();
        for component in parent.split('/') {
            current.push(component);
            let dir = root_path.join(&current);
            if !dir.is_dir() || dir.is_symlink() {
                return false;
            }
            if apply_umask_ceiling(&dir, None, mask).is_err() {
                return false;
            }
        }
    }
    let target = root_path.join(relative);
    if mode == "120000" {
        if !target.is_symlink() {
            return false;
        }
        let bytes = match link_bytes(&target) {
            Some(bytes) => bytes,
            None => return false,
        };
        return file_text_digest(root_path, &bytes).is_ok_and(|digest| digest == oid);
    }
    if !target.is_file() || target.is_symlink() {
        return false;
    }
    match file_digest(root_path, &target) {
        Ok(digest) if digest == oid => {}
        _ => return false,
    }
    if apply_tracked_file_mode(&target, mode, mask).is_err() {
        return false;
    }
    file_digest(root_path, &target).is_ok_and(|digest| digest == oid)
}

/// Parse one NUL-terminated `ls-tree` record into
/// (mode, type, oid, relative), like the shell's tab split plus
/// `read` over the header.
fn parse_inventory_record(record: &[u8]) -> Option<(&str, &str, &str, &str)> {
    let tab = record.iter().position(|byte| *byte == b'\t')?;
    let (header, relative) = record.split_at(tab);
    let relative = std::str::from_utf8(&relative[1..]).ok()?;
    let header = std::str::from_utf8(header).ok()?;
    let mut fields = header.split_whitespace();
    let record = Some((fields.next()?, fields.next()?, fields.next()?, relative));
    // `read` rejects a fourth field (`[[ -z $extra ]]`).
    if fields.next().is_some() {
        return None;
    }
    record
}

/// `_repo_normalize_cloned_overlay_modes`: reapply the retained
/// umask across a staged checkout once, then check every tracked
/// leaf against the validated commit. The inventory stays in
/// memory — the shell's scratch file is unobservable.
pub fn normalize_cloned_overlay_modes(root: &str, commit: &str, mask: u32) -> bool {
    let root_path = Path::new(root);
    let stage: [OsString; 2] = stage_prefix(root);
    if !run_git(&stage, &["config", "core.sharedRepository", "0700"])
        .is_some_and(|output| output.status.success())
    {
        return false;
    }
    if apply_git_metadata_modes(&root_path.join(".git"), mask).is_err() {
        return false;
    }
    if apply_umask_ceiling(root_path, None, mask).is_err() {
        return false;
    }
    if !cached_matches(&stage, commit) {
        return false;
    }
    let inventory = match run_git(&stage, &["ls-tree", "-rz", "--full-tree", commit]) {
        Some(output) if output.status.success() => output.stdout,
        _ => return false,
    };
    for record in terminated_records(&inventory) {
        let Some((mode, kind, oid, relative)) = parse_inventory_record(record) else {
            return false;
        };
        if kind != "blob" || !is_clone_mode(mode) || !is_clone_oid(oid) {
            return false;
        }
        if !cloned_overlay_path_modes(root, relative, mode, oid, mask) {
            return false;
        }
    }
    cached_matches(&stage, commit)
}

/// `git diff --cached --quiet`, true when the index matches.
fn cached_matches(stage: &[OsString], commit: &str) -> bool {
    run_git(stage, &["diff", "--cached", "--quiet", commit, "--"])
        .is_some_and(|output| output.status.success())
}

/// `_repo_cloned_overlay_matches_commit`: the staged checkout is
/// exactly the commit — clean index, clean worktree, no untracked
/// files.
pub fn cloned_overlay_matches_commit(root: &str, commit: &str) -> bool {
    let stage = stage_prefix(root);
    if !cached_matches(&stage, commit) {
        return false;
    }
    if !run_git(&stage, &["diff", "--quiet", commit, "--"])
        .is_some_and(|output| output.status.success())
    {
        return false;
    }
    run_git(&stage, &["ls-files", "--others", "-z"])
        .is_some_and(|output| output.status.success() && output.stdout.is_empty())
}

/// Remove a staging directory, best effort like the shell's
/// `|| true` cleanup.
fn remove_stage(stage_root: &Path) {
    let mut cleanup = Registry::new();
    let _ = cleanup.remove_path(stage_root);
}

/// Inputs for [`clone_overlay_staged`]: the clone source and
/// destination plus the validation context.
pub struct CloneOverlayInputs<'a> {
    /// Clone source URL.
    pub url: &'a str,
    /// Destination path (string `${path%/*}` semantics apply).
    pub path: &'a str,
    /// Candidate validation environment.
    pub candidate: &'a CandidateEnv,
    /// Mode ceiling mask (the caller reads the live umask, like
    /// the shell's inline `umask` call).
    pub mask: u32,
    /// Logger for validation warnings.
    pub log: &'a Log,
}

/// `_repo_clone_overlay_staged`: clone into a quarantine sibling,
/// validate and normalize the checkout, then move it into place.
/// Failures remove the staging directory and report false; only the
/// final staging removal propagates its own status.
pub fn clone_overlay_staged(
    inputs: &CloneOverlayInputs<'_>,
    moves: &mut MoveCache,
    warnings: &mut dyn Write,
) -> bool {
    let Some((parent, name)) = inputs.path.rsplit_once('/') else {
        return false;
    };
    if parent.is_empty() || parent == inputs.path {
        return false;
    }
    if !crate::temp::mkdir_forwarded(Path::new(parent), warnings) {
        return false;
    }
    let template = format!("{parent}/.{name}.clone.XXXXXX");
    let stage_root = match std::process::Command::new("mktemp")
        .arg("-d")
        .arg(&template)
        .output()
    {
        Ok(output) if output.status.success() => PathBuf::from(
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string(),
        ),
        _ => return false,
    };
    let stage = stage_root.join("checkout");
    let stage_text = stage.to_string_lossy().into_owned();
    // Unsuppressed like the shell: only the `_pull_overlay` caller
    // redirects, so clone diagnostics reach the caller's stderr
    // (`warnings` here). A `--quiet` clone writes no stdout in
    // either outcome, so only stderr forwards.
    let clone = std::process::Command::new("git")
        .args([
            "-c",
            "core.sharedRepository=0700",
            "clone",
            "--quiet",
            "--no-hardlinks",
            "--",
            inputs.url,
            stage_text.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match clone {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let _ = warnings.write_all(&output.stderr);
            remove_stage(&stage_root);
            return false;
        }
        Err(_) => {
            remove_stage(&stage_root);
            return false;
        }
    }
    let stage_prefix = stage_prefix(&stage_text);
    let commit = repo_head(&stage_prefix);
    if commit.is_empty()
        || !validate_candidate_tree(
            &stage_prefix,
            "overlay",
            &commit,
            inputs.candidate,
            inputs.log,
            warnings,
        )
        || !cloned_overlay_matches_commit(&stage_text, &commit)
    {
        remove_stage(&stage_root);
        return false;
    }
    if !normalize_cloned_overlay_modes(&stage_text, &commit, inputs.mask)
        || repo_head(&stage_prefix) != commit
        || !cloned_overlay_matches_commit(&stage_text, &commit)
    {
        remove_stage(&stage_root);
        return false;
    }
    if move_noreplace_cached(&stage, Path::new(inputs.path), moves).is_err() {
        remove_stage(&stage_root);
        return false;
    }
    let mut cleanup = Registry::new();
    cleanup.remove_path(&stage_root).is_ok()
}
