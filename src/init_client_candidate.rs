//! Candidate enumeration, live snapshots, and conflict planning for
//! `lib/dot/init-client.sh`.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the seven planning primitives from
//! `_dot_init_symlink_blob_safe` through
//! `_dot_init_build_prior_and_conflicts`: the symlink-blob byte gate,
//! the candidate-tree writer, the per-path candidate matcher, the
//! live-filesystem snapshot probe and its recheck, the conflict-root
//! walk, and the prior/conflicts publisher. The file-generic
//! `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state, absorbed into
//! [`Result`] the way earlier slices absorb engine diagnostics). The
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family on
//! `rust-port-slice-41` (`init_client_identity`), the git-generation
//! binding on `rust-port-slice-43` (`init_client_generation`), the
//! per-entry staging family on `rust-port-slice-46`
//! (`init_client_entry`), and the record, git-staging, publish,
//! delete, and rollback families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the client root from `HOME`, the
//! checkout from `DOT_SOURCE_ROOT`, and the reserved-roots inventory
//! from the live environment (`XDG_*`, `SHDEPS_*`, `OVERLAYS`,
//! `DOT_INIT_BACKUP`). Library code must not read that ambient state
//! behind the engine, so callers pass a [`CandidateScope`] carrying
//! the same values explicitly. The `REPLY`-carried outputs
//! (`_dot_init_snapshot_path`, `_dot_init_conflict_root`) return
//! their values instead.
//!
//! Byte boundary: tree records and link targets cross as raw bytes on
//! the shell side. This module validates shapes on the lossy render
//! (the `repos_pull_queries` precedent) but writes the original bytes
//! back out, so only exact value equality on non-UTF8 names can
//! differ — and the shared [`crate::repos_overlays`] path gate only
//! tests ASCII delimiters, so validation still agrees.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::repos_overlays::init_safe_relative_path;
use crate::{reserved, temp};

/// Words in one `ls-tree` header (`mode`, `type`, `object`): the
/// shell's `read -r mode type oid` assigns exactly three variables,
/// so any other word count rejects without further checks.
const HEADER_WORD_COUNT: usize = 3;

/// Largest candidate inventory the planner accepts: the shell counts
/// leaves and fails past 100 000, so a hostile repository cannot make
/// the transaction stage unbounded work.
const MAX_TREE_ENTRIES: usize = 100_000;

/// Largest symlink blob the planner trusts: empty and oversized blobs
/// fail, so a hostile repository cannot smuggle control bytes (or a
/// multi-megabyte payload) through a link target into the journals.
const MAX_BLOB_BYTES: usize = 4096;

/// Client context for the git-touching planners: the values the shell
/// reads from `HOME`, the install checkout, the process working
/// directory, `DOT_SOURCE_ROOT`, and the reserved-roots snapshot.
/// Callers build the snapshot with
/// [`reserved::reserved_roots`] from the same environment the shell
/// probe sees, exactly like the pull-side `CandidateEnv` does.
#[derive(Debug, Clone)]
pub struct CandidateScope {
    /// Client `$HOME`: every candidate resolves beneath it.
    pub home: String,
    /// Client checkout (`$install_root/cgraf78/dot`) for the
    /// installer-transient leg of the reserved check.
    pub checkout: String,
    /// Working directory the reserved route-mapping runs from (the
    /// shell's `$PWD` when the probe fires).
    pub pwd: String,
    /// Repository root holding `support/client-launcher.sh`, the one
    /// generated payload allowed to overtake a reserved destination.
    pub source_root: PathBuf,
    /// Reserved-roots snapshot lines for
    /// [`reserved::candidate_path_is_reserved_from_roots`].
    pub roots: Vec<String>,
}

/// `_dot_init_safe_value`: nonempty with no tab, newline, or
/// carriage-return bytes. The same rule already guards
/// [`crate::repos_overlays`]; it is repeated here (not imported)
/// because that copy is private to its own call sites, the
/// `init_client_identity` lane precedent.
fn safe_value(value: &str) -> bool {
    !value.is_empty() && !value.contains(['\t', '\n', '\r'])
}

/// Object id shape for candidate policy: 40 to 64 hexadecimal digits
/// (the shell's `{40,64}` range, not just the two modern lengths).
/// Twin of the pull-side gate, which answers for `pull.sh`; this copy
/// stays local because that module is a sibling owner, not a shared
/// helper.
fn is_candidate_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Commit-shape object id: exactly 40 or 64 hexadecimal digits, the
/// shell's `^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$` alternation for live
/// content hashes.
fn is_commit_oid(oid: &str) -> bool {
    (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`. `symlink_metadata` never follows,
/// so a link reports its own type and fails the gate on both
/// engines — including a link pointing at a directory, which the
/// shell also rejects here.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`, same no-follow reasoning as
/// [`is_real_dir`].
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Any existing filesystem occupant: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling links that
/// `-e` alone would miss.
fn exists_or_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Append one path component with a plain `/` separator, like the
/// shell's `"$HOME/$path"`: a `home` with a trailing slash keeps its
/// doubled separator instead of being normalized away (the
/// `init_client_entry` lane precedent).
fn home_join(home: &str, relative: &str) -> PathBuf {
    let mut out = std::ffi::OsString::from(home);
    out.push("/");
    out.push(relative);
    PathBuf::from(out)
}

/// Run `git -C <repo> <args>` with `LC_ALL=C` pinned, capturing
/// stdout. `None` when git cannot start or reports failure, like the
/// shell's `|| return 1` on the substitution — git's own stderr is
/// silenced (the `candidate_matches` precedent; the tree scan's
/// unredirected fd 2 is an unobservable sink in tests).
fn run_repo_git(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// `git hash-object --no-filters --stdin` over in-memory bytes: the
/// digest half of `_dot_stdin_matches_file` without a file leg, so
/// link targets (which live only in memory here) hash with the exact
/// flags the shell's combined call uses. `None` when git fails.
fn hash_stdin_bytes(payload: &[u8]) -> Option<String> {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(["hash-object", "--no-filters", "--stdin"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .as_mut()?
        .write_all(payload)
        .map_err(|_| ())
        .ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git hash-object --no-filters -- <path>`: the live-file digest
/// for [`snapshot_path`]. Paths cross as raw bytes (never through a
/// shell), so non-UTF8 names hash exactly like the shell's.
/// `None` when git fails.
fn hash_live_file(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["hash-object", "--no-filters", "--"])
        .arg(path.as_os_str())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `_dot_init_symlink_blob_safe`: the blob at `branch:path` is small
/// (1 to 4096 bytes, both bounds inclusive — empty and oversized both
/// fail) and carries no NUL, tab, newline, or carriage-return byte,
/// so no client path or git directory built from it can smuggle TSV
/// framing into the journals. The shell stages the blob through a
/// cleanup scratch file and an `od | awk` scan; the bytes stay in
/// memory here (the pull-side `terminated_records` precedent: the
/// scratch file is an unobservable implementation detail).
pub fn symlink_blob_safe(repo: &Path, branch: &str, path: &str) -> bool {
    let spec = format!("{branch}:{path}");
    let bytes = match run_repo_git(repo, &["show", &spec]) {
        Some(bytes) => bytes,
        None => return false,
    };
    if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
        return false;
    }
    !bytes
        .iter()
        .any(|byte| matches!(byte, 0x00 | 0x09 | 0x0a | 0x0d))
}

/// Split one `ls-tree -z` record at its first tab into header and
/// path bytes, mirroring `${entry%%\t*}` / `${entry#*\t}`. `None`
/// when the record carries no tab at all.
fn split_tree_record(entry: &[u8]) -> Option<(&[u8], &[u8])> {
    let tab = entry.iter().position(|byte| *byte == b'\t')?;
    Some((&entry[..tab], &entry[tab + 1..]))
}

/// The client-launcher exception: only `.local/bin/dot` at 100755
/// carrying the exact tracked launcher payload may overtake a
/// reserved destination. The `show` payload must succeed and
/// byte-match through [`temp::stdin_matches_file`], like the shell
/// pipeline into `_dot_stdin_matches_file`.
fn adapter_exception_allowed(
    repo: &Path,
    branch: &str,
    mode: &str,
    path: &str,
    scope: &CandidateScope,
) -> bool {
    if path != ".local/bin/dot" || mode != "100755" {
        return false;
    }
    let spec = format!("{branch}:{path}");
    let payload = match run_repo_git(repo, &["show", &spec]) {
        Some(payload) => payload,
        None => return false,
    };
    let launcher = scope.source_root.join("support/client-launcher.sh");
    temp::stdin_matches_file(&scope.source_root, &payload, &launcher).unwrap_or(false)
}

/// `_dot_init_candidate_tree`: enumerate `branch` into `output` as
/// `mode\tobject\tpath` rows. Every leaf must be a blob at a
/// supported mode with a well-formed id, live at a safe relative
/// path, use a trusted symlink payload, and stay clear of reserved
/// roots (save the launcher exception); the inventory must also be
/// nonempty and fit `MAX_TREE_ENTRIES`. Any violation truncates
/// `output` back to empty and reports failure, exactly like the
/// shell's trailing `: >"$output"; return 1`.
///
/// Like the shell, `output` is truncated up front, so an early git
/// failure still leaves an empty file behind — callers must treat an
/// error as "no inventory", never as "empty repository".
pub fn candidate_tree(
    repo: &Path,
    branch: &str,
    output: &Path,
    scope: &CandidateScope,
) -> Result<()> {
    use std::io::Write as _;
    // Truncate first, like the shell's opening `: >"$output"`.
    std::fs::write(output, b"").map_err(|source| Error::Io {
        context: "truncate candidate tree",
        source,
    })?;
    let fail = |message: &'static str| -> Result<()> {
        // Mirror the trailing truncation: no partial inventory
        // survives a rejection.
        let _ = std::fs::write(output, b"");
        Err(Error::Usage { message })
    };
    let raw = match run_repo_git(repo, &["ls-tree", "-rz", "--full-tree", branch]) {
        Some(raw) => raw,
        None => return fail("candidate tree is not listable"),
    };
    // Only NUL-terminated chunks are records, so a trailing
    // unterminated tail is ignored — the pull-side
    // `terminated_records` parity rule for `read -d ''`.
    let mut records: Vec<&[u8]> = Vec::new();
    let mut rest = raw.as_slice();
    while let Some(ix) = rest.iter().position(|byte| *byte == 0) {
        records.push(&rest[..ix]);
        rest = &rest[ix + 1..];
    }
    let mut out = Vec::new();
    let mut count = 0usize;
    for entry in &records {
        let Some((header, path_bytes)) = split_tree_record(entry) else {
            return fail("candidate tree entry has no path");
        };
        // `read -r mode type oid` folds extra header words into the
        // last variable, which then fails the id gate below — so any
        // word count but three rejects, with no need to model the
        // fold itself.
        let header_text = String::from_utf8_lossy(header);
        let words: Vec<&str> = header_text.split_ascii_whitespace().collect();
        if words.len() != HEADER_WORD_COUNT {
            return fail("candidate tree header is malformed");
        }
        let (mode, entry_type, oid) = (words[0], words[1], words[2]);
        if entry_type != "blob"
            || !matches!(mode, "100644" | "100755" | "120000")
            || !is_candidate_oid(oid)
        {
            return fail("candidate tree entry is unsupported");
        }
        let path = String::from_utf8_lossy(path_bytes);
        if !init_safe_relative_path(&path) {
            return fail("candidate path is unsafe");
        }
        if mode == "120000" && !symlink_blob_safe(repo, branch, &path) {
            return fail("candidate symlink payload is unsafe");
        }
        let destination = format!("{}/{path}", scope.home);
        if reserved::candidate_path_is_reserved_from_roots(
            &destination,
            &scope.roots,
            &scope.home,
            &scope.checkout,
            &scope.pwd,
        ) && !adapter_exception_allowed(repo, branch, mode, &path, scope)
        {
            return fail("candidate owns a reserved path");
        }
        out.extend_from_slice(mode.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(oid.as_bytes());
        out.push(b'\t');
        // Original bytes go back out (see the module byte-boundary
        // note); validation above already agreed on the shape.
        out.extend_from_slice(path_bytes);
        out.push(b'\n');
        count += 1;
        if count > MAX_TREE_ENTRIES {
            return fail("candidate tree is too large");
        }
    }
    if records.is_empty() {
        return fail("candidate tree is empty");
    }
    let mut file = std::fs::File::create(output).map_err(|source| Error::Io {
        context: "rewrite candidate tree",
        source,
    })?;
    file.write_all(&out).map_err(|source| Error::Io {
        context: "write candidate tree",
        source,
    })?;
    Ok(())
}

/// `_dot_init_candidate_matches_path`: the live `$HOME/path` already
/// carries the candidate generation — byte-identical content plus
/// the matching executable bit. Symlinks compare through the blob
/// hash of the link target (the shell hashes `readlink` bytes
/// against `git show`, never the link inode itself).
pub fn candidate_matches_path(
    repo: &Path,
    branch: &str,
    mode: &str,
    path: &str,
    scope: &CandidateScope,
) -> bool {
    let target = home_join(&scope.home, path);
    if !exists_or_link(&target) {
        return false;
    }
    let spec = format!("{branch}:{path}");
    match mode {
        "120000" => {
            if !std::fs::symlink_metadata(&target).is_ok_and(|meta| meta.file_type().is_symlink()) {
                return false;
            }
            // `readlink` bytes, not the inode: `$(...)` chomping
            // cannot apply (targets stay trusted only through
            // `symlink_blob_safe`), so hash the raw target bytes.
            let link = match std::fs::read_link(&target) {
                Ok(link) => link,
                Err(_) => return false,
            };
            let (Some(want), Some(got)) = (
                run_repo_git(repo, &["show", &spec]).and_then(|bytes| hash_stdin_bytes(&bytes)),
                hash_stdin_bytes(link.as_os_str().as_bytes()),
            ) else {
                return false;
            };
            want == got
        }
        "100644" | "100755" => {
            if !is_real_file(&target) {
                return false;
            }
            let shown = match run_repo_git(repo, &["show", &spec]) {
                Some(shown) => shown,
                None => return false,
            };
            if !temp::stdin_matches_file(&scope.source_root, &shown, &target).unwrap_or(false) {
                return false;
            }
            let mode_bits = match temp::file_mode(&target) {
                Ok(mode_bits) => mode_bits,
                Err(_) => return false,
            };
            if mode == "100755" {
                mode_bits & 0o111 != 0
            } else {
                mode_bits & 0o111 == 0
            }
        }
        _ => false,
    }
}

/// `_dot_init_snapshot_path`: freeze one live path as a six-field
/// `kind\tdev\tino\tmode\tsize\tvalue` line. Missing paths
/// freeze as `absent` with `-` fields; symlinks freeze their
/// newline-chomped target (the shell's `$(readlink)` strips trailing
/// newlines before the safety gate, so chomp-then-gate here too);
/// regular files freeze their content hash; directories freeze `-`.
/// Anything else (fifos, sockets, devices) fails. Dangling links
/// freeze like live ones: every probe here lstates, so no target is
/// ever followed.
pub fn snapshot_path(path: &Path) -> Result<String> {
    if !exists_or_link(path) {
        return Ok("absent\t-\t-\t-\t-\t-".to_string());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat snapshot path",
        source,
    })?;
    if meta.file_type().is_symlink() {
        // The shell's `stat` lstates a link operand (the ceiling
        // module documents the same rule), so a link freezes its
        // OWN identity, 0777 mode, and target-name length — never
        // the target's. Dangling links freeze fine here; only the
        // identity `stat` of a vanished target could fail, and
        // lstat never follows.
        let raw = std::fs::read_link(path).map_err(|source| Error::Io {
            context: "read snapshot link",
            source,
        })?;
        // Shell `$(...)` chomping: every trailing newline is gone
        // before `_dot_init_safe_value` sees the target.
        let mut bytes = raw.as_os_str().as_bytes().to_vec();
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        let value = String::from_utf8_lossy(&bytes);
        if !safe_value(&value) {
            return Err(Error::Usage {
                message: "snapshot link target is unsafe",
            });
        }
        // Six tab fields: kind, dev, ino, octal mode, size, value —
        // dev and ino stay separate like the shell's `printf`
        // `%s` per variable (the recheck below splits the same
        // way).
        return Ok(format!(
            "symlink\t{}\t{}\t{:o}\t{}\t{value}",
            meta.dev(),
            meta.ino(),
            meta.mode() & 0o7777,
            meta.size(),
        ));
    }
    // Regular files and directories are never links here, so the
    // following stat sees the same bytes lstat would.
    let (device, inode) = temp::path_identity(path)?;
    let mode_bits = temp::file_mode(path)?;
    let size = temp::file_size(path)?;
    if meta.is_file() {
        let digest = match hash_live_file(path) {
            Some(digest) if is_commit_oid(&digest) => digest,
            _ => {
                return Err(Error::Usage {
                    message: "snapshot file digest is malformed",
                });
            }
        };
        return Ok(format!(
            "regular\t{device}\t{inode}\t{:o}\t{size}\t{digest}",
            mode_bits & 0o7777,
        ));
    }
    if meta.is_dir() {
        return Ok(format!(
            "directory\t{device}\t{inode}\t{:o}\t{size}\t-",
            mode_bits & 0o7777,
        ));
    }
    Err(Error::Usage {
        message: "snapshot path is not a file, link, or directory",
    })
}

/// `_dot_init_path_state_matches`: recheck a frozen snapshot line
/// against the live path. `absent` matches only continued absence;
/// anything else rechecks type, `dev:ino` identity, octal mode and
/// size as rendered strings (so a `0644` spelling never equals the
/// `644` a live `stat` prints, on either engine), plus the content
/// hash for regular files or the chomped target for symlinks.
/// Directories match on identity and mode alone.
#[allow(clippy::too_many_arguments)]
pub fn path_state_matches(
    path: &Path,
    kind: &str,
    device: &str,
    inode: &str,
    mode: &str,
    size: &str,
    value: &str,
) -> bool {
    if kind == "absent" {
        return !exists_or_link(path);
    }
    match kind {
        "regular" => {
            if !is_real_file(path) {
                return false;
            }
        }
        "symlink" => {
            if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
                return false;
            }
        }
        "directory" => {
            if !is_real_dir(path) {
                return false;
            }
            // Directories carry no content token: identity and mode
            // below are the whole check, like the shell's early
            // `return 0`.
            let identity = match temp::path_identity(path) {
                Ok(identity) => temp::identity_string(identity),
                Err(_) => return false,
            };
            if identity != format!("{device}:{inode}") {
                return false;
            }
            let current_mode = match temp::file_mode(path) {
                Ok(current_mode) => format!("{:o}", current_mode & 0o7777),
                Err(_) => return false,
            };
            return current_mode == mode;
        }
        _ => return false,
    }
    // Links recheck through lstat, files through stat — the same
    // operand rule as the snapshot above. Both spellings agree for
    // real files, so only the link leg branches.
    let (identity, current_mode, current_size) = if kind == "symlink" {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => return false,
        };
        (
            temp::identity_string((meta.dev(), meta.ino())),
            format!("{:o}", meta.mode() & 0o7777),
            meta.size().to_string(),
        )
    } else {
        let identity = match temp::path_identity(path) {
            Ok(identity) => temp::identity_string(identity),
            Err(_) => return false,
        };
        let current_mode = match temp::file_mode(path) {
            Ok(current_mode) => format!("{:o}", current_mode & 0o7777),
            Err(_) => return false,
        };
        let current_size = match temp::file_size(path) {
            Ok(current_size) => current_size.to_string(),
            Err(_) => return false,
        };
        (identity, current_mode, current_size)
    };
    if identity != format!("{device}:{inode}") {
        return false;
    }
    if current_mode != mode {
        return false;
    }
    if current_size != size {
        return false;
    }
    if kind == "regular" {
        match hash_live_file(path) {
            Some(digest) => digest == value,
            None => false,
        }
    } else {
        let raw = match std::fs::read_link(path) {
            Ok(raw) => raw,
            Err(_) => return false,
        };
        let mut bytes = raw.as_os_str().as_bytes().to_vec();
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        String::from_utf8_lossy(&bytes) == value
    }
}

/// `_dot_init_conflict_root`: the path (or nearest ancestor) the
/// backup must move — the deepest live occupant that is not a real
/// directory, or `path` itself when every ancestor is a directory or
/// absent. A symlink-to-directory still blocks (it is not a real
/// directory), and `home` only scopes the probes, never the answer.
pub fn conflict_root(path: &str, home: &str) -> String {
    let mut current = path;
    while let Some(slash) = current.rfind('/') {
        let parent = &current[..slash];
        if exists_or_link(&home_join(home, parent)) && !is_real_dir(&home_join(home, parent)) {
            return parent.to_string();
        }
        current = parent;
    }
    path.to_string()
}

/// `_dot_init_build_prior_and_conflicts`: plan one candidate `tree`
/// into `prior` (every candidate plus its live snapshot) and
/// `conflicts` (the deduplicated backup roots). Candidates already
/// live at the wanted generation need no backup; absent paths whose
/// own name is the root need none either. Both journals truncate up
/// front and land at mode 600 — and a mid-plan failure keeps the
/// partial rows unchmodded, exactly like the shell's early
/// `return 1` past the trailing `chmod`.
pub fn build_prior_and_conflicts(
    repo: &Path,
    branch: &str,
    tree: &Path,
    prior: &Path,
    conflicts: &Path,
    scope: &CandidateScope,
) -> Result<()> {
    use std::io::Write as _;
    // Truncate first: the shell's opening `: >` pair runs before the
    // tree is even opened, so a missing tree still empties both.
    std::fs::write(prior, b"").map_err(|source| Error::Io {
        context: "truncate prior journal",
        source,
    })?;
    std::fs::write(conflicts, b"").map_err(|source| Error::Io {
        context: "truncate conflicts journal",
        source,
    })?;
    // A missing or unreadable tree plans nothing: the shell's
    // `done <"$tree"` redirect fails, the loop body never runs, and
    // the trailing `chmod` still succeeds — so the journals land
    // truncated at mode 600 with a zero status. The redirect's
    // stderr diagnostic has no file-leg equivalent here.
    let content = std::fs::read(tree).unwrap_or_default();
    let text = String::from_utf8_lossy(&content).into_owned();
    // Shell `read` framing: bytes divide on `\n`, a missing trailing
    // newline still yields its final line, and a trailing newline
    // adds no phantom empty line (the generation-marker precedent).
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    let mut prior_file = std::fs::File::create(prior).map_err(|source| Error::Io {
        context: "rewrite prior journal",
        source,
    })?;
    let mut conflicts_file = std::fs::File::create(conflicts).map_err(|source| Error::Io {
        context: "rewrite conflicts journal",
        source,
    })?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in lines {
        // `IFS=\t read mode oid path`: the first two fields split,
        // the last takes the unsplit remainder (tabs included).
        let mut fields = line.splitn(3, '\t');
        // The object id rides along unread, like the shell's `read
        // -r mode oid path`, which never expands `$oid` in this loop.
        let (mode, _oid, path) = (
            fields.next().unwrap_or(""),
            fields.next().unwrap_or(""),
            fields.next().unwrap_or(""),
        );
        let live = home_join(&scope.home, path);
        let state = snapshot_path(&live)?;
        writeln!(prior_file, "{path}\t{state}").map_err(|source| Error::Io {
            context: "append prior journal",
            source,
        })?;
        // Flush per row so a later failure leaves the same partial
        // journals the shell's mid-loop `return 1` leaves behind.
        prior_file.flush().map_err(|source| Error::Io {
            context: "flush prior journal",
            source,
        })?;
        if !state.starts_with("absent\t") && candidate_matches_path(repo, branch, mode, path, scope)
        {
            continue;
        }
        let root = conflict_root(path, &scope.home);
        if state.starts_with("absent\t") && root == path {
            continue;
        }
        if !seen.insert(root.clone()) {
            continue;
        }
        let root_state = snapshot_path(&home_join(&scope.home, &root))?;
        if root_state.starts_with("absent\t") {
            return Err(Error::Usage {
                message: "conflict root is absent",
            });
        }
        writeln!(conflicts_file, "{root}\t{root_state}").map_err(|source| Error::Io {
            context: "append conflicts journal",
            source,
        })?;
        conflicts_file.flush().map_err(|source| Error::Io {
            context: "flush conflicts journal",
            source,
        })?;
    }
    drop(prior_file);
    drop(conflicts_file);
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(prior, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        Error::Io {
            context: "chmod prior journal",
            source,
        }
    })?;
    std::fs::set_permissions(conflicts, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| Error::Io {
            context: "chmod conflicts journal",
            source,
        },
    )?;
    Ok(())
}
