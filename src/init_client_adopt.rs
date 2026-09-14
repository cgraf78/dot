//! The init adopt/status chapter of `lib/dot/init-client.sh`: the
//! legacy-client adoption, the init usage text, and the init status
//! report.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the three contiguous functions from
//! `_dot_init_adopt_existing` through `_dot_init_status` in file
//! order (lines 1418-1501): the existing-client adoption
//! ([`adopt_existing`]), the command usage ([`usage`]), and the
//! transaction status report ([`status`]).
//!
//! Lane map, so the integrator can stack without overlap: the
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family
//! (including `_dot_init_repo_identity`) on `rust-port-slice-41`
//! (`init_client_identity`), the transaction record journal on
//! `rust-port-slice-51` (`init_client_records`) and
//! `rust-port-slice-54` (`init_client_record`), the confirmation and
//! completion publication (including `_dot_init_publish_completed`)
//! on `rust-port-slice-62` (`init_client_plan`), and the
//! published-state recovery plus update convergence (including
//! `_dot_init_forward_converge` and `_dot_init_single_origin`) on
//! `rust-port-slice-65` (`init_client_publish`). The neighbor
//! `_dot_init_delete_park_path` below this chapter lives on
//! `rust-port-slice-55` (`init_client_delete`). The file-generic
//! `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state, absorbed into
//! [`StatusReport`] the way earlier slices absorb engine
//! diagnostics).
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals, the selected base topology from
//! `DOT_BASE_TOPOLOGY`, and the worktree root from `HOME`. Library
//! code must not read process environment behind the engine, so the
//! origin, identity, branch, home, and selected topology cross as
//! explicit parameters, and every cross-lane call the shell makes by
//! name crosses as a closure ([`AdoptEngine`], [`StatusEngine`]).
//! `REPLY`-carried outputs surface as return values, and the two
//! rendered reports ([`usage`], [`status`]) return their bytes for
//! the caller to emit, keeping this module free of ambient file
//! descriptors. The `git` invocations below are engine mechanics,
//! not ported functions: they run the exact `_base_git` argv
//! (`--git-dir=<dir> --work-tree=<home>` for a separate client,
//! `-C <home>` for an ordinary one) with `LC_ALL=C` pinned and the
//! home steered at the fixture, like the shell inherits from its
//! harness.
//!
//! Byte-fidelity boundary: every `$HOME/...` join concatenates bytes
//! like the shell, preserving a doubled separator on trailing-slash
//! inputs instead of normalizing it away (the delete lane
//! precedent). Command substitution chomps every trailing newline
//! before the shell compares, so git output is chomped the same way
//! here. Journal and identity text crosses the UTF-8 boundary as
//! `&str`, the candidate lane precedent, so non-UTF8 run values can
//! diverge from the shell exactly the way they do on sibling lanes.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::repos_base::Topology;
use crate::temp;

/// Single-origin reader: the `rust-port-slice-65` lane's
/// `_dot_init_single_origin` by detected topology, injected because
/// that lane is unmerged. `None` is any failure, the shell's
/// `|| return 2`.
pub type SingleOrigin<'a> = dyn Fn(Topology) -> Option<String> + 'a;

/// Repository-identity canonicalizer: the `rust-port-slice-41`
/// lane's `_dot_init_repo_identity`, injected because that lane is
/// unmerged. `None` is any failure, the shell's `|| return 2`.
pub type RepoIdentity<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Transaction-directory derivation: the `rust-port-slice-35`
/// lane's `_dot_init_transaction_dir`, injected because that lane is
/// unmerged. `None` is any failure.
pub type TransactionDir<'a> = dyn Fn() -> Option<PathBuf> + 'a;

/// Completion-record derivation: the `rust-port-slice-35` lane's
/// `_dot_init_completed_file`, injected because that lane is
/// unmerged. `None` is any failure.
pub type CompletedFile<'a> = dyn Fn() -> Option<PathBuf> + 'a;

/// Transaction stager: the `rust-port-slice-35` lane's
/// `_dot_init_prepare_transaction`, injected because that lane is
/// unmerged. `None` is any failure, the shell's `|| return 3`.
pub type PrepareTransaction<'a> = dyn Fn(&Path) -> Option<PathBuf> + 'a;

/// Record fields for one `_dot_init_write_record` call: the
/// destination, the phase, the run identity quad, and the
/// commit/nonce/device/inode the shell threads through its
/// `DOT_INIT_*` globals.
#[derive(Debug)]
pub struct RecordFields<'a> {
    /// Destination record file (`$stage/record`, then
    /// `$transaction/record`).
    pub record: &'a Path,
    /// Record phase (`converging`, then `complete`).
    pub phase: &'a str,
    /// Requested repository URL (`$origin`).
    pub origin: &'a str,
    /// Canonical repository identity (`$identity`).
    pub identity: &'a str,
    /// Requested branch (`$branch`).
    pub branch: &'a str,
    /// Backup root (`-` here: adoption never backs up).
    pub backup: &'a str,
    /// Adopted git directory (`$git_dir`).
    pub git_dir: &'a Path,
    /// Adopted commit (`DOT_INIT_COMMIT`).
    pub commit: &'a str,
    /// Run nonce (`DOT_INIT_NONCE`, always `adopted` here).
    pub nonce: &'a str,
    /// Git-directory device (`DOT_INIT_GIT_DEV`).
    pub git_dev: &'a str,
    /// Git-directory inode (`DOT_INIT_GIT_INO`).
    pub git_ino: &'a str,
}

/// Record journal writer: the `rust-port-slice-51` /
/// `rust-port-slice-54` lanes' `_dot_init_write_record`, injected
/// because those lanes are unmerged. `false` is any failure, the
/// shell's `|| return 3`.
pub type WriteRecord<'a> = dyn Fn(&RecordFields<'_>) -> bool + 'a;

/// Transaction publisher: the `rust-port-slice-35` lane's
/// `_dot_init_publish_transaction`, injected because that lane is
/// unmerged. `false` is any failure, the shell's `|| return 3`.
pub type PublishTransaction<'a> = dyn Fn(&Path, &Path) -> bool + 'a;

/// Update convergence entry: the `rust-port-slice-65` lane's
/// `_dot_init_forward_converge`, injected because that lane is
/// unmerged (and because this chapter must not touch it). The shell
/// passes the detected topology and git directory through the
/// `DOT_BASE_TOPOLOGY` / `DOT_CLIENT_GIT_DIR` globals; here they
/// cross as arguments. `false` is any failure, the shell's
/// `|| return 3`.
pub type ForwardConverge<'a> = dyn Fn(Topology, &Path) -> bool + 'a;

/// Completion publisher: the `rust-port-slice-62` lane's
/// `_dot_init_publish_completed`, injected because that lane is
/// unmerged. `false` is any failure, the shell's `|| return 3`.
pub type PublishCompleted<'a> = dyn Fn(&Path) -> bool + 'a;

/// Record reader for the status report: the
/// `rust-port-slice-51` / `rust-port-slice-54` lanes'
/// `_dot_init_read_record`, projected onto the four fields the
/// status report prints, injected because those lanes are unmerged.
/// `None` is any failure (including a malformed record).
pub type ReadRecord<'a> = dyn Fn(&Path) -> Option<StatusRecord> + 'a;

/// Cross-lane engine for [`adopt_existing`]: one closure per shell
/// call by name, so tests feed either stubs or closures running the
/// live shell functions.
pub struct AdoptEngine<'a> {
    /// Lane-65 single-origin reader.
    pub single_origin: &'a SingleOrigin<'a>,
    /// Lane-41 repository-identity canonicalizer.
    pub repo_identity: &'a RepoIdentity<'a>,
    /// Lane-35 transaction-directory derivation.
    pub transaction_dir: &'a TransactionDir<'a>,
    /// Lane-35 transaction stager.
    pub prepare_transaction: &'a PrepareTransaction<'a>,
    /// Lanes-51/54 record journal writer.
    pub write_record: &'a WriteRecord<'a>,
    /// Lane-35 transaction publisher.
    pub publish_transaction: &'a PublishTransaction<'a>,
    /// Lane-65 update convergence entry.
    pub forward_converge: &'a ForwardConverge<'a>,
    /// Lane-62 completion publisher.
    pub publish_completed: &'a PublishCompleted<'a>,
}

/// Cross-lane engine for [`status`]: one closure per shell call by
/// name.
pub struct StatusEngine<'a> {
    /// Lane-35 transaction-directory derivation.
    pub transaction_dir: &'a TransactionDir<'a>,
    /// Lane-35 completion-record derivation.
    pub completed_file: &'a CompletedFile<'a>,
    /// Lanes-51/54 record reader (status projection).
    pub read_record: &'a ReadRecord<'a>,
}

/// A successfully adopted client: the detected topology and git
/// directory the shell exports as `DOT_BASE_TOPOLOGY` /
/// `DOT_CLIENT_GIT_DIR` before converging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    /// Detected topology (`separate` or `ordinary`).
    pub topology: Topology,
    /// Adopted git directory (`$HOME/.dotfiles` or `$HOME/.git`).
    pub git_dir: PathBuf,
}

/// Adoption failure codes of `_dot_init_adopt_existing`: the shell's
/// `return 1` (no repository), `return 2` (present but untrusted),
/// and `return 3` (matched but unfinished). The `dot init` caller
/// branches on these, so they stay distinct instead of collapsing
/// into one usage error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptError {
    /// No adoptable repository exists (`return 1`).
    NoRepository,
    /// A repository exists but its shape is untrusted
    /// (`return 2`).
    Mismatch,
    /// An exactly matched repository failed adoption (`return 3`).
    Failed,
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdoptError::NoRepository => write!(f, "no adoptable client repository"),
            AdoptError::Mismatch => write!(f, "existing client repository is untrusted"),
            AdoptError::Failed => write!(f, "existing client repository failed adoption"),
        }
    }
}

impl std::error::Error for AdoptError {}

/// The four record fields the status report prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRecord {
    /// Journal phase (`DOT_INIT_PHASE`).
    pub phase: String,
    /// Journal origin (`DOT_INIT_ORIGIN`).
    pub origin: String,
    /// Journal branch (`DOT_INIT_BRANCH`).
    pub branch: String,
    /// Journal backup (`DOT_INIT_BACKUP`).
    pub backup: String,
}

/// Rendered `_dot_init_status` result: the stdout report, the
/// stderr diagnostic (empty unless the journal is malformed), and
/// the shell exit code (0, or 1 for a malformed journal or an
/// underivable state path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// Standard-output report bytes.
    pub stdout: Vec<u8>,
    /// Standard-error diagnostic bytes.
    pub stderr: Vec<u8>,
    /// Process exit code.
    pub code: u8,
}

/// `$base/$leaf` by byte concatenation, like the shell's
/// `git_dir=$HOME/.dotfiles` and `record=$stage/record`: a `base`
/// with a trailing slash keeps its doubled separator exactly like
/// the shell's expansion does.
fn join_leaf(base: &Path, leaf: &str) -> PathBuf {
    let mut joined = base.as_os_str().as_bytes().to_vec();
    joined.push(b'/');
    joined.extend_from_slice(leaf.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Command-substitution chomp: the shell strips every trailing
/// newline from `$(...)` before comparing, so git output is framed
/// the same way here.
fn chomped(output: &[u8]) -> &[u8] {
    let mut text = output;
    while text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    text
}

/// The shell's
/// `[[ $commit =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]]`: a full
/// 40- or 64-hex object id, nothing else.
fn valid_commit(commit: &[u8]) -> bool {
    (commit.len() == 40 || commit.len() == 64) && commit.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// Scrub the ambient `GIT_*` overrides the shell oracle never sees
/// (it runs under `env_clear`), so a developer's exported `GIT_DIR`
/// cannot steer one engine and not the other. Twin of the
/// `temp::sanitized_git` unset list, without its `-c`/`-C` source
/// binding: this chapter runs the plain `command git` argv of
/// `_base_git`.
fn unset_git_env(cmd: &mut Command) {
    const UNSET: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_INDEX_FILE",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_DEFAULT_HASH",
    ];
    for var in UNSET {
        cmd.env_remove(var);
    }
}

/// Run `git` with an explicit `_base_git` prefix and a pinned
/// locale: stdout piped, stderr nulled, stdin null. `None` when git
/// cannot start or reports failure, like the shell's `|| return` on
/// the substitution — git's own stderr is silenced.
fn base_git(home: &Path, prefix: &[OsString], args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    unset_git_env(&mut cmd);
    cmd.env("LC_ALL", "C")
        .env("HOME", home)
        .args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// `_base_git` argv for a separate client:
/// `git --git-dir=<git_dir> --work-tree=<home>`. Built from raw
/// bytes (never a lossy `display`), like the shell's expansions.
fn separate_prefix(git_dir: &Path, home: &Path) -> [OsString; 2] {
    let mut dir = OsString::from("--git-dir=");
    dir.push(git_dir);
    let mut worktree = OsString::from("--work-tree=");
    worktree.push(home);
    [dir, worktree]
}

/// `_base_git` argv for an ordinary client: `git -C <home>`.
fn ordinary_prefix(home: &Path) -> [OsString; 2] {
    let flag = OsString::from("-C");
    let mut dir = OsString::new();
    dir.push(home);
    [flag, dir]
}

/// `rm -rf` over one path: missing names succeed, symlinks unlink,
/// directories recurse, anything else unlinks. Adoption always
/// removes the real directory it published, but the twin keeps the
/// removal total like `rm -rf` does.
fn remove_tree(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path).is_ok(),
        Ok(_) => std::fs::remove_file(path).is_ok(),
    }
}

/// `_dot_init_adopt_existing`: adopt a previously supported client
/// layout after the exact origin and active branch have been bound
/// to the requested initialization identity.
///
/// `selected` is the source-time `DOT_BASE_TOPOLOGY`
/// (`_base_repo_exists` trusts it: anything but `missing` takes the
/// separate shape without a filesystem probe). Returns
/// [`AdoptError::NoRepository`] when no repository exists,
/// [`AdoptError::Mismatch`] for a present but untrusted shape, and
/// [`AdoptError::Failed`] when an exactly matched repository could
/// not finish the adoption workflow. Success reports the detected
/// shape the shell exports as `DOT_BASE_TOPOLOGY` /
/// `DOT_CLIENT_GIT_DIR` before converging.
///
/// The function itself is silent: the trailing `rm -rf` status is
/// the verdict, so a failed removal reports
/// [`AdoptError::NoRepository`] exactly like the shell's exit code
/// does (its caller falls through to a fresh initialization on
/// anything but the mismatch and failure codes).
pub fn adopt_existing(
    home: &Path,
    selected: Topology,
    origin: &str,
    identity: &str,
    branch: &str,
    engine: &AdoptEngine<'_>,
) -> Result<Adopted, AdoptError> {
    let (topology, git_dir) = if selected != Topology::Missing {
        (Topology::Separate, join_leaf(home, ".dotfiles"))
    } else {
        let git_dir = join_leaf(home, ".git");
        if !is_real_dir(&git_dir) {
            return Err(AdoptError::NoRepository);
        }
        // `rev-parse --show-toplevel` must name the worktree root
        // itself: a `.git` directory serving another worktree is no
        // repository for this home.
        let toplevel = base_git(
            home,
            &ordinary_prefix(home),
            &["rev-parse", "--show-toplevel"],
        );
        let rooted = toplevel
            .as_deref()
            .is_some_and(|top| chomped(top) == home.as_os_str().as_bytes());
        if !rooted {
            return Err(AdoptError::NoRepository);
        }
        (Topology::Ordinary, git_dir)
    };
    let recorded_origin = (engine.single_origin)(topology).ok_or(AdoptError::Mismatch)?;
    let recorded_identity = (engine.repo_identity)(&recorded_origin).ok_or(AdoptError::Mismatch)?;
    if recorded_identity != identity {
        return Err(AdoptError::Mismatch);
    }
    // The shell's `if [[ $topology == separate ]]` gates: only the
    // two detected shapes reach this point.
    let prefix: Vec<OsString> = if topology == Topology::Separate {
        separate_prefix(&git_dir, home).to_vec()
    } else {
        ordinary_prefix(home).to_vec()
    };
    // `$(... || true)`: a failed branch read compares empty, which
    // the literal comparison below then rejects (unless the caller
    // asked for an empty branch, exactly like the shell).
    let active_branch = base_git(home, &prefix, &["symbolic-ref", "--short", "HEAD"])
        .map(|out| chomped(&out).to_vec())
        .unwrap_or_default();
    if active_branch != branch.as_bytes() {
        return Err(AdoptError::Mismatch);
    }
    // `commit=$(... ) || return 2`: a failed object read distrusts
    // the shape, as does a non-id.
    let commit = base_git(home, &prefix, &["rev-parse", "HEAD"]).ok_or(AdoptError::Mismatch)?;
    let commit = chomped(&commit).to_vec();
    if !valid_commit(&commit) {
        return Err(AdoptError::Mismatch);
    }
    // Hex-only, so the lossy conversion is exact; `&str` is what
    // the record closure takes.
    let commit = String::from_utf8_lossy(&commit).into_owned();
    // `$(_dot_path_identity "$git_dir") || return 2`: `stat`
    // follows the directory exactly like the shell's.
    let git_identity = temp::path_identity(&git_dir).map_err(|_| AdoptError::Mismatch)?;
    let git_identity = temp::identity_string(git_identity);
    let Some((git_dev, git_ino)) = git_identity.split_once(':') else {
        return Err(AdoptError::Mismatch);
    };
    let transaction = (engine.transaction_dir)().ok_or(AdoptError::Failed)?;
    let stage = (engine.prepare_transaction)(&transaction).ok_or(AdoptError::Failed)?;
    let stage_record = join_leaf(&stage, "record");
    let converging = RecordFields {
        record: &stage_record,
        phase: "converging",
        origin,
        identity,
        branch,
        backup: "-",
        git_dir: &git_dir,
        commit: &commit,
        nonce: "adopted",
        git_dev,
        git_ino,
    };
    if !(engine.write_record)(&converging) {
        return Err(AdoptError::Failed);
    }
    if !(engine.publish_transaction)(&stage, &transaction) {
        return Err(AdoptError::Failed);
    }
    let transaction_record = join_leaf(&transaction, "record");
    if !(engine.forward_converge)(topology, &git_dir) {
        return Err(AdoptError::Failed);
    }
    let complete = RecordFields {
        record: &transaction_record,
        phase: "complete",
        origin,
        identity,
        branch,
        backup: "-",
        git_dir: &git_dir,
        commit: &commit,
        nonce: "adopted",
        git_dev,
        git_ino,
    };
    if !(engine.write_record)(&complete) {
        return Err(AdoptError::Failed);
    }
    if !(engine.publish_completed)(&transaction_record) {
        return Err(AdoptError::Failed);
    }
    if !remove_tree(&transaction) {
        return Err(AdoptError::NoRepository);
    }
    Ok(Adopted { topology, git_dir })
}

/// `_dot_init_usage`: the exact three-line command synopsis.
pub fn usage() -> Vec<u8> {
    const TEXT: &[u8] = b"usage: dot init [--branch BRANCH] [--yes] REPOSITORY_URL\n       dot init --status\n       dot init --rollback\n";
    TEXT.to_vec()
}

/// Empty report with an exit code and no streams: the shell's bare
/// `return 1` when a state path is underivable.
fn silent(code: u8) -> StatusReport {
    StatusReport {
        stdout: Vec::new(),
        stderr: Vec::new(),
        code,
    }
}

/// `_dot_init_status`: report the durable initialization state. A
/// live transaction journal reports `incomplete` with its phase,
/// origin, branch, and backup; a completion record reports
/// `complete` with its origin and branch; otherwise the client was
/// never started. A malformed journal keeps the shell's
/// `dot init: malformed ...` stderr diagnostic and exit code 1.
pub fn status(engine: &StatusEngine<'_>) -> StatusReport {
    let Some(transaction) = (engine.transaction_dir)() else {
        return silent(1);
    };
    let Some(completed) = (engine.completed_file)() else {
        return silent(1);
    };
    if exists_lexical(&transaction) {
        let record = join_leaf(&transaction, "record");
        match (engine.read_record)(&record) {
            Some(entry) => {
                let mut stdout = b"initialization: incomplete\nphase: ".to_vec();
                stdout.extend_from_slice(entry.phase.as_bytes());
                stdout.extend_from_slice(b"\norigin: ");
                stdout.extend_from_slice(entry.origin.as_bytes());
                stdout.extend_from_slice(b"\nbranch: ");
                stdout.extend_from_slice(entry.branch.as_bytes());
                stdout.extend_from_slice(b"\nbackup: ");
                stdout.extend_from_slice(entry.backup.as_bytes());
                stdout.push(b'\n');
                StatusReport {
                    stdout,
                    stderr: Vec::new(),
                    code: 0,
                }
            }
            None => {
                let mut stderr = b"dot init: malformed initialization transaction: ".to_vec();
                stderr.extend_from_slice(transaction.as_os_str().as_bytes());
                stderr.push(b'\n');
                StatusReport {
                    stdout: Vec::new(),
                    stderr,
                    code: 1,
                }
            }
        }
    } else if exists_lexical(&completed) {
        match (engine.read_record)(&completed) {
            Some(entry) => {
                let mut stdout = b"initialization: complete\norigin: ".to_vec();
                stdout.extend_from_slice(entry.origin.as_bytes());
                stdout.extend_from_slice(b"\nbranch: ");
                stdout.extend_from_slice(entry.branch.as_bytes());
                stdout.push(b'\n');
                StatusReport {
                    stdout,
                    stderr: Vec::new(),
                    code: 0,
                }
            }
            None => {
                let mut stderr = b"dot init: malformed completion record: ".to_vec();
                stderr.extend_from_slice(completed.as_os_str().as_bytes());
                stderr.push(b'\n');
                StatusReport {
                    stdout: Vec::new(),
                    stderr,
                    code: 1,
                }
            }
        }
    } else {
        StatusReport {
            stdout: b"initialization: not started\n".to_vec(),
            stderr: Vec::new(),
            code: 0,
        }
    }
}
