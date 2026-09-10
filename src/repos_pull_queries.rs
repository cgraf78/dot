//! Pull precondition queries from `lib/dot/repos/pull.sh`.
//!
//! Thin git-inspection wrappers over caller-provided command
//! prefixes: the checked-out generation, upstream containment,
//! generation identity, and the candidate-tree validation cluster
//! (adapter gate, entry policy, full-tree and ahead-delta scans,
//! generation acceptance). Quiet probes run through
//! [`crate::repos_base::run_git`]; the tree scans use an identical
//! runner that additionally forwards git's own stderr beside
//! `_warn`, exactly like the shell's unredirected `ls-tree`.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::log::Log;
use crate::repos_base::run_git;
use crate::{repos_overlays, reserved, temp};

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

/// Client environment for candidate validation: the reserved-roots
/// inventory inputs plus the checkout, working directory, and
/// source root the shell reads from globals (`$HOME`, XDG/SHDEPS
/// overrides, `$DOT_SOURCE_ROOT`, and the process cwd).
#[derive(Debug, Clone)]
pub struct CandidateEnv {
    /// Client `$HOME`.
    pub home: String,
    /// Client checkout (`$install_root/cgraf78/dot`).
    pub checkout: String,
    /// Working directory the reserved probe runs from.
    pub pwd: String,
    /// Repository root holding `support/client-launcher.sh`.
    pub source_root: String,
    /// Resolved XDG state home.
    pub state_home: String,
    /// `${SHDEPS_INSTALL_DIR:-$HOME/.local/share}`.
    pub install_root: String,
    /// `${SHDEPS_STATE_DIR:-$state_home/shdeps}`.
    pub provider_state: String,
    /// Overlay link paths (the `path` field of each `OVERLAYS` record).
    pub overlay_paths: Vec<String>,
    /// `$DOT_INIT_BACKUP` when set and not `-`.
    pub init_backup: Option<String>,
}

impl CandidateEnv {
    /// The reserved-roots inventory input for this client.
    fn roots_input(&self) -> reserved::RootsInput {
        reserved::RootsInput {
            home: self.home.clone(),
            state_home: self.state_home.clone(),
            install_root: self.install_root.clone(),
            provider_state: self.provider_state.clone(),
            overlay_paths: self.overlay_paths.clone(),
            init_backup: self.init_backup.clone(),
        }
    }
}

/// Whether `oid` is a well-formed object id for candidate policy:
/// 40 to 64 hexadecimal digits (the shell's `{40,64}` range, not
/// just the two modern lengths).
fn is_candidate_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A private scratch file removed when its validation scope ends.
struct ValidationOutput {
    path: PathBuf,
}

fn validation_output_target(temp_root: &Path, current_dir: &Path) -> PathBuf {
    let root = if temp_root.is_absolute() {
        temp_root.to_path_buf()
    } else {
        current_dir.join(temp_root)
    };
    root.join("dot-validation-output")
}

fn validation_temp_root(tmpdir: Option<OsString>) -> PathBuf {
    tmpdir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

impl ValidationOutput {
    fn create() -> Option<(Self, File)> {
        let temp_root = validation_temp_root(std::env::var_os("TMPDIR"));
        Self::create_in(&temp_root, &std::env::current_dir().ok()?)
    }

    fn create_in(temp_root: &Path, current_dir: &Path) -> Option<(Self, File)> {
        let target = validation_output_target(temp_root, current_dir);
        let path = temp::sibling_tmp_for_existing_parent(&target).ok()?;
        let output = Self { path };
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&output.path)
            .ok()?;
        Some((output, file))
    }

    fn reader(&self) -> Option<BufReader<File>> {
        File::open(&self.path).ok().map(BufReader::new)
    }

    fn cleanup(mut self) -> bool {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
        self.path.clear();
        true
    }
}

impl Drop for ValidationOutput {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Run a validation Git command with stdout in a private scratch file.
/// This mirrors the shell's bounded-memory tree captures. `warnings`
/// receives Git stderr for the unredirected tree commands; `None`
/// keeps the adapter's `git show ... 2>/dev/null` quiet.
fn capture_validation_git(
    prefix: &[OsString],
    args: &[&str],
    mut warnings: Option<&mut dyn std::io::Write>,
) -> Option<(bool, ValidationOutput)> {
    let (capture, stdout) = ValidationOutput::create()?;
    let mut command = std::process::Command::new("git");
    command
        .args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout));
    let stderr_capture = if warnings.is_some() {
        let (capture, stderr) = ValidationOutput::create()?;
        command.stderr(Stdio::from(stderr));
        Some(capture)
    } else {
        command.stderr(Stdio::null());
        None
    };
    let status = command.status().ok()?;
    if let (Some(warnings), Some(stderr_capture)) = (&mut warnings, stderr_capture) {
        let mut reader = stderr_capture.reader()?;
        if std::io::copy(&mut reader, warnings).is_err() {
            return None;
        }
        drop(reader);
        if !stderr_capture.cleanup() {
            return None;
        }
    }
    Some((status.success(), capture))
}

/// `_repo_candidate_adapter_allowed`: only the client launcher path
/// at 100755 carrying the exact tracked launcher payload may
/// overtake a reserved destination. The `show` payload must succeed
/// and byte-match, like the shell pipeline into
/// `_dot_stdin_matches_file`; Git's diagnostic is silenced like the
/// shell's explicit `2>/dev/null`.
pub fn candidate_adapter_allowed(
    prefix: &[OsString],
    git_ref: &str,
    path: &str,
    mode: &str,
    env: &CandidateEnv,
    _warnings: &mut dyn std::io::Write,
) -> bool {
    if path != ".local/bin/dot" || mode != "100755" {
        return false;
    }
    let spec = format!("{git_ref}:{path}");
    let (success, capture) = match capture_validation_git(prefix, &["show", &spec], None) {
        Some(result) => result,
        None => return false,
    };
    if !success {
        let _ = capture.cleanup();
        return false;
    }
    let launcher = Path::new(&env.source_root).join("support/client-launcher.sh");
    let matches =
        temp::files_equal(Path::new(&env.source_root), &capture.path, &launcher).unwrap_or(false);
    let cleaned = capture.cleanup();
    matches && cleaned
}

/// Verdict of [`validate_candidate_entry`] for one Git leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryVerdict {
    /// Accepted, with the path as it would appear beneath `$HOME`.
    Accept(String),
    /// Overlay metadata outside `home/`: the shell's `return 0`
    /// with an empty `REPLY`.
    Skip,
    /// Rejected; the warning is already emitted.
    Reject,
}

/// `_repo_validate_candidate_entry`: the candidate-tree policy for
/// one Git leaf. Rejections warn through `log` exactly like `_warn`.
#[allow(clippy::too_many_arguments)]
pub fn validate_candidate_entry(
    prefix: &[OsString],
    kind: &str,
    git_ref: &str,
    mode: &str,
    entry_type: &str,
    oid: &str,
    path: &str,
    roots: &[String],
    env: &CandidateEnv,
    log: &Log,
    warnings: &mut dyn std::io::Write,
) -> EntryVerdict {
    if entry_type != "blob"
        || !matches!(mode, "100644" | "100755" | "120000")
        || !is_candidate_oid(oid)
    {
        return EntryVerdict::Reject;
    }
    if !repos_overlays::init_safe_relative_path(path) {
        return EntryVerdict::Reject;
    }
    let relative = if kind == "overlay" {
        match path.strip_prefix("home/") {
            Some(rest) => rest.to_string(),
            None => {
                if path == "home" {
                    return EntryVerdict::Reject;
                }
                return EntryVerdict::Skip;
            }
        }
    } else {
        path.to_string()
    };
    if kind == "overlay" && reserved::overlay_control_path_reserved(&relative) {
        log.warn(
            warnings,
            &format!("  warning: overlay candidate owns reserved control-plane path: {relative}"),
        );
        return EntryVerdict::Reject;
    }
    let destination = format!("{}/{relative}", env.home);
    if reserved::candidate_path_is_reserved_from_roots(
        &destination,
        roots,
        &env.home,
        &env.checkout,
        &env.pwd,
    ) && !candidate_adapter_allowed(prefix, git_ref, path, mode, env, warnings)
    {
        log.warn(
            warnings,
            &format!("  warning: candidate repository owns reserved path: {relative}"),
        );
        return EntryVerdict::Reject;
    }
    EntryVerdict::Accept(relative)
}

/// Read one NUL-terminated record, mirroring `read -r -d ''`:
/// an unterminated trailing fragment is ignored.
fn read_terminated_record(
    reader: &mut impl BufRead,
    record: &mut Vec<u8>,
) -> std::io::Result<bool> {
    record.clear();
    let read = reader.read_until(0, record)?;
    if read == 0 || record.last() != Some(&0) {
        record.clear();
        return Ok(false);
    }
    record.pop();
    Ok(true)
}

/// `_repo_validate_candidate_tree`: every leaf of the fetched
/// candidate must pass [`validate_candidate_entry`], with at most
/// 100000 counted leaves, and the reserved inventory must be
/// unchanged across the scan (the shell's before/after snapshot
/// comparison). The raw `ls-tree` capture stays in a private scratch
/// file, preserving the shell's bounded-memory behavior.
pub fn validate_candidate_tree(
    prefix: &[OsString],
    kind: &str,
    git_ref: &str,
    env: &CandidateEnv,
    log: &Log,
    warnings: &mut dyn std::io::Write,
) -> bool {
    let roots = match reserved::reserved_roots(&env.roots_input(), &env.pwd) {
        Ok(roots) => roots,
        Err(_) => return false,
    };
    let (success, capture) = match capture_validation_git(
        prefix,
        &["ls-tree", "-rz", "--full-tree", git_ref],
        Some(warnings),
    ) {
        Some(result) => result,
        None => return false,
    };
    if !success {
        return false;
    }
    let Some(mut reader) = capture.reader() else {
        return false;
    };
    let mut count = 0;
    let mut entry = Vec::new();
    loop {
        match read_terminated_record(&mut reader, &mut entry) {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => return false,
        }
        let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
            return false;
        };
        let (header, path) = (&entry[..tab], &entry[tab + 1..]);
        let header = String::from_utf8_lossy(header);
        let path = String::from_utf8_lossy(path);
        let mut fields = header.split_ascii_whitespace();
        let (mode, entry_type, oid) = match (fields.next(), fields.next(), fields.next()) {
            (Some(mode), Some(entry_type), Some(oid)) => (mode, entry_type, oid),
            _ => return false,
        };
        // `read -r mode type oid` folds extra header words into the
        // last variable, which then fails the oid gate below.
        let oid = if fields.next().is_some() {
            format!("{oid} ")
        } else {
            oid.to_string()
        };
        match validate_candidate_entry(
            prefix, kind, git_ref, mode, entry_type, &oid, &path, &roots, env, log, warnings,
        ) {
            EntryVerdict::Accept(_) => {
                count += 1;
                if count > 100_000 {
                    return false;
                }
            }
            EntryVerdict::Skip => {}
            EntryVerdict::Reject => return false,
        }
    }
    let valid = match reserved::reserved_roots(&env.roots_input(), &env.pwd) {
        Ok(after) => after == roots,
        Err(_) => false,
    };
    drop(reader);
    let cleaned = capture.cleanup();
    valid && cleaned
}

/// `_repo_validate_ahead_delta`: the local-ahead fast path — only
/// the `upstream..head` delta leaves validate, in `diff-tree -z`
/// header/path pairs. A header without a terminated path, a header
/// outside `:old new old-oid new-oid status` shape, or a rejected
/// leaf fails the delta; a trailing tail after complete pairs is
/// ignored like the shell's final failed `read`.
pub fn validate_ahead_delta(
    prefix: &[OsString],
    kind: &str,
    upstream: &str,
    head: &str,
    env: &CandidateEnv,
    log: &Log,
    warnings: &mut dyn std::io::Write,
) -> bool {
    let roots = match reserved::reserved_roots(&env.roots_input(), &env.pwd) {
        Ok(roots) => roots,
        Err(_) => return false,
    };
    let (success, capture) = match capture_validation_git(
        prefix,
        &[
            "diff-tree",
            "-r",
            "--no-commit-id",
            "--raw",
            "-z",
            "--no-renames",
            "--diff-filter=ACMT",
            upstream,
            head,
        ],
        Some(warnings),
    ) {
        Some(result) => result,
        None => return false,
    };
    if !success {
        return false;
    }
    let Some(mut reader) = capture.reader() else {
        return false;
    };
    let mut count = 0;
    let mut header = Vec::new();
    let mut path = Vec::new();
    loop {
        match read_terminated_record(&mut reader, &mut header) {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => return false,
        }
        match read_terminated_record(&mut reader, &mut path) {
            Ok(true) => {}
            Ok(false) | Err(_) => return false,
        }
        let (header, path) = (
            String::from_utf8_lossy(&header),
            String::from_utf8_lossy(&path),
        );
        let Some(bare) = header.strip_prefix(':') else {
            return false;
        };
        let mut fields = bare.split_ascii_whitespace();
        let (old_mode, mode, old_oid, oid, status) = match (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) {
            (Some(old_mode), Some(mode), Some(old_oid), Some(oid), Some(status)) => {
                (old_mode, mode, old_oid, oid, status)
            }
            _ => return false,
        };
        if fields.next().is_some()
            || old_mode.len() != 6
            || !old_mode.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
            || !is_candidate_oid(old_oid)
            || !matches!(status, "A" | "C" | "M" | "T")
        {
            return false;
        }
        match validate_candidate_entry(
            prefix, kind, head, mode, "blob", oid, &path, &roots, env, log, warnings,
        ) {
            EntryVerdict::Accept(_) => {
                count += 1;
                if count > 100_000 {
                    return false;
                }
            }
            EntryVerdict::Skip => {}
            EntryVerdict::Reject => return false,
        }
    }
    let valid = match reserved::reserved_roots(&env.roots_input(), &env.pwd) {
        Ok(after) => after == roots,
        Err(_) => false,
    };
    drop(reader);
    let cleaned = capture.cleanup();
    valid && cleaned
}

/// `_repo_accept_current_generation`: 0 when the live generation is
/// safely current (equal, or a contained generation with a valid
/// local-ahead delta and a stable final HEAD read), 1 when the
/// fetched upstream is not contained and needs the ordinary pull
/// path, and 2 when inputs are empty or a contained generation is
/// invalid or moved during inspection.
pub fn accept_current_generation(
    prefix: &[OsString],
    kind: &str,
    head: &str,
    upstream: &str,
    env: &CandidateEnv,
    log: &Log,
    warnings: &mut dyn std::io::Write,
) -> i32 {
    if head.is_empty() || upstream.is_empty() {
        return 2;
    }
    if head != upstream {
        if !repo_head_contains_upstream(prefix, head, upstream) {
            return 1;
        }
        if !validate_ahead_delta(prefix, kind, upstream, head, env, log, warnings) {
            return 2;
        }
    }
    if repo_head_is(prefix, head) { 0 } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_output_target_is_absolute() {
        assert_eq!(
            validation_output_target(Path::new("scratch"), Path::new("/work")),
            Path::new("/work/scratch/dot-validation-output")
        );
        assert_eq!(
            validation_output_target(Path::new("/scratch"), Path::new("/work")),
            Path::new("/scratch/dot-validation-output")
        );
    }

    #[test]
    fn validation_temp_root_matches_shell_default() {
        assert_eq!(validation_temp_root(None), Path::new("/tmp"));
        assert_eq!(
            validation_temp_root(Some(OsString::new())),
            Path::new("/tmp")
        );
        assert_eq!(
            validation_temp_root(Some(OsString::from("scratch"))),
            Path::new("scratch")
        );
    }

    #[test]
    fn validation_output_cleanup_matches_rm_force() {
        let path = temp::sibling_tmp_for(&std::env::temp_dir().join("dot-validation-cleanup-test"))
            .expect("unique test path");
        std::fs::remove_file(&path).expect("remove capture first");
        assert!(
            ValidationOutput { path }.cleanup(),
            "an already-missing capture is clean"
        );
    }

    #[test]
    fn validation_output_cleanup_reports_unlink_failure() {
        let path = temp::sibling_tmp_for(&std::env::temp_dir().join("dot-validation-cleanup-test"))
            .expect("unique test path");
        std::fs::remove_file(&path).expect("replace capture with directory");
        std::fs::create_dir(&path).expect("test directory");
        assert!(
            !ValidationOutput { path: path.clone() }.cleanup(),
            "an unlink failure must fail closed"
        );
        std::fs::remove_dir(path).expect("remove test directory");
    }

    #[test]
    fn validation_output_requires_existing_temp_root() {
        let root = temp::sibling_tmp_for(&std::env::temp_dir().join("dot-validation-missing-root"))
            .expect("unique test path");
        std::fs::remove_file(&root).expect("leave temp root absent");
        assert!(
            ValidationOutput::create_in(&root, Path::new("/work")).is_none(),
            "a missing temp root must stay missing"
        );
        assert!(!root.exists(), "validation must not create the temp root");
    }
}
