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
use std::path::Path;
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

/// Inspection runner beside [`run_git`]: stdin nulled and stdout
/// captured the same way, but git's own stderr bytes flow to
/// `warnings` in execution order. The shell's `ls-tree`,
/// `diff-tree`, and `show` here are unredirected, so their fatal
/// shares fd 2 with `_warn`; capturing into the same sink keeps
/// the byte stream identical. Returns success plus stdout.
fn run_validation_git(
    prefix: &[OsString],
    args: &[&str],
    warnings: &mut dyn std::io::Write,
) -> Option<(bool, Vec<u8>)> {
    let output = std::process::Command::new("git")
        .args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let _ = warnings.write_all(&output.stderr);
    Some((output.status.success(), output.stdout))
}

/// `_repo_candidate_adapter_allowed`: only the client launcher path
/// at 100755 carrying the exact tracked launcher payload may
/// overtake a reserved destination. The `show` payload must succeed
/// and byte-match, like the shell pipeline into
/// `_dot_stdin_matches_file`; its diagnostic flows to `warnings`
/// like the shell's unredirected `show`.
pub fn candidate_adapter_allowed(
    prefix: &[OsString],
    git_ref: &str,
    path: &str,
    mode: &str,
    env: &CandidateEnv,
    warnings: &mut dyn std::io::Write,
) -> bool {
    if path != ".local/bin/dot" || mode != "100755" {
        return false;
    }
    let spec = format!("{git_ref}:{path}");
    let (success, stdout) = match run_validation_git(prefix, &["show", &spec], warnings) {
        Some(result) => result,
        None => return false,
    };
    if !success {
        return false;
    }
    let launcher = Path::new(&env.source_root).join("support/client-launcher.sh");
    temp::stdin_matches_file(Path::new(&env.source_root), &stdout, &launcher).unwrap_or(false)
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

/// NUL-terminated records of one `git ls-tree -z` capture, mirroring
/// `while IFS= read -r -d '' entry`: only chunks followed by a NUL
/// are records, so a trailing unterminated tail is ignored.
fn terminated_records(output: &[u8]) -> Vec<&[u8]> {
    let mut records = Vec::new();
    let mut rest = output;
    while let Some(ix) = rest.iter().position(|byte| *byte == 0) {
        records.push(&rest[..ix]);
        rest = &rest[ix + 1..];
    }
    records
}

/// `_repo_validate_candidate_tree`: every leaf of the fetched
/// candidate must pass [`validate_candidate_entry`], with at most
/// 100000 counted leaves, and the reserved inventory must be
/// unchanged across the scan (the shell's before/after snapshot
/// comparison). The raw `ls-tree` capture stays in memory — the
/// shell's scratch file is an unobservable implementation detail.
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
    let (success, raw) = match run_validation_git(
        prefix,
        &["ls-tree", "-rz", "--full-tree", git_ref],
        warnings,
    ) {
        Some(result) => result,
        None => return false,
    };
    if !success {
        return false;
    }
    let mut count = 0;
    for entry in terminated_records(&raw) {
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
    match reserved::reserved_roots(&env.roots_input(), &env.pwd) {
        Ok(after) => after == roots,
        Err(_) => false,
    }
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
    let (success, raw) = match run_validation_git(
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
        warnings,
    ) {
        Some(result) => result,
        None => return false,
    };
    if !success {
        return false;
    }
    let records = terminated_records(&raw);
    if records.len() % 2 != 0 {
        return false;
    }
    let mut count = 0;
    for pair in records.chunks_exact(2) {
        let (header, path) = (
            String::from_utf8_lossy(pair[0]),
            String::from_utf8_lossy(pair[1]),
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
    match reserved::reserved_roots(&env.roots_input(), &env.pwd) {
        Ok(after) => after == roots,
        Err(_) => false,
    }
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
