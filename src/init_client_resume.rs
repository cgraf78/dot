//! Resume and live-git verification for `lib/dot/init-client.sh`:
//! the transaction resume orchestrator and the live-git guard.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the two contiguous functions from
//! `_dot_init_live_git_matches_record` through
//! `_dot_init_resume_transaction` in file order (lines 1711-1788):
//! the predicate that re-verifies a live git dir against the
//! transaction record ([`live_git_matches_record`]) and the dispatcher
//! that replays a transaction forward from any recorded phase
//! ([`resume_transaction`]).
//!
//! Rollback and command dispatch remain separate from the record
//! journal (`init_client_record`), the
//! plan-review family (`init_client_plan`), the git-stage pair and
//! converge tail (`init_client_git`, `init_client_publish`), and the
//! identity, generation, candidate, entry, and delete families owned
//! by their corresponding modules.
//!
//! The implementation stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals and the worktree root from `HOME`. Library
//! code must not read process environment behind the engine, so the
//! phase, backup root, nonce, git identity triple, branch, and home
//! cross here as explicit parameters, bundled the way the git lane
//! bundles its stage inputs (flat parameters would trip
//! `clippy::too_many_arguments`). All cross-lane helpers the shell
//! calls by name — the path identity, generation-marker, and repo
//! identity predicates plus the record, move, stage, publish,
//! converge, and completion steps — cross as closures to preserve those
//! ownership boundaries. `REPLY`-carried outputs
//! surface as return values; the resume wrapper itself is silent like
//! the shell (sub-step bytes belong to the step closures, which own
//! their file descriptors the way the engine does).
//!
//! Byte-fidelity boundary: `$HOME/<leaf>` spellings concatenate bytes
//! like the shell, preserving a doubled separator on trailing-slash
//! inputs instead of normalizing it away (the plan lane precedent for
//! compared spellings; joins that only feed syscalls use
//! [`Path::join`], which names the same file either way).
//! `LC_ALL=C` is pinned around every child process so git output
//! reads English and byte-ordered on both engines. Command
//! substitution strips every trailing newline (`chomp`); `mapfile -t`
//! framing keeps a bare tail but adds no phantom element for a
//! trailing newline (`split_lines`), and NUL bytes are scrubbed the
//! way the plan lane's read loops scrub them (git cannot emit NUL in
//! config output — its own parser reads C strings — so this is
//! defensive, never load-bearing). `grep -Fqx` semantics match line
//! for line including an unterminated tail, and any unreadable
//! identity file refuses exactly like the shell's failed `grep`.
//! Physical directories come from [`std::fs::canonicalize`], the
//! `cd -P && pwd -P` equivalent for existing paths; both fail closed
//! on missing inputs. Removal mirrors the shell's `rm -rf` (missing
//! paths succeed, symlinks remove the link, directories recurse),
//! twinning `cleanup::remove_one`, which is private to its module.
//! The one known stream divergence: a failing `rm` prints through the
//! shell's stderr while this module only reports the verdict —
//! `cleanup` documents the same boundary for its own removals.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::errors::{Error, Result};

/// Path identity provision: the live device, inode, and birth time
/// (`stat`, following links like `_dot_path_identity`), which
/// [`crate::persisted_identity::matches`] weighs against the recorded
/// `dev:ino`. Injected so tests can stand in for a reboot that renumbers
/// the device.
pub type PathIdentity<'a> = dyn Fn(&Path) -> Result<crate::persisted_identity::LiveIdentity> + 'a;

/// Generation-marker check: `_dot_init_generation_marker_matches`,
/// owned by the generation lane. Skipped for adopted runs, exactly
/// like the shell's nonce gate.
pub type GenerationMatches<'a> = dyn Fn(&Path) -> bool + 'a;

/// Repository identity normalization: `_dot_init_repo_identity`,
/// owned by the identity lane. Refuses origins it cannot normalize,
/// like the shell's `|| return 1` does.
pub type RepoIdentity<'a> = dyn Fn(&str) -> Result<String> + 'a;

/// Journal advance: `_dot_init_record_phase`, owned by the record
/// lane. Sets the engine phase as a side effect there; test harnesses
/// thread it explicitly (see the parity tests).
pub type RecordPhase<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// Conflict safekeeping: `_dot_init_move_conflicts`, owned by the
/// plan lane. Takes the manifest and the backup root, the shell's two
/// positionals.
pub type MoveConflicts<'a> = dyn Fn(&Path, &Path) -> Result<()> + 'a;

/// Git staging: `_dot_init_stage_git`, owned by the git lane. Takes
/// the record, the shell's single positional; refreshes the run's git
/// identity as a side effect there.
pub type StageGit<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Staged git publication: `_dot_init_publish_git`, owned by the git
/// lane. Takes the record, like the shell.
pub type PublishGit<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Worktree publication: `_dot_init_publish_worktree`, owned by the
/// publish lane. Takes the transaction directory, like the shell.
pub type PublishWorktree<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Forward convergence: `_dot_init_forward_converge`, owned by the
/// publish lane. Reads its configuration from the engine environment
/// there; here it takes no arguments, like the shell.
pub type ForwardConverge<'a> = dyn Fn() -> Result<()> + 'a;

/// Completion stamping: `_dot_init_publish_completed`, owned by the
/// plan lane. Takes the record, like the shell.
pub type PublishCompleted<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Inputs for [`live_git_matches_record`]: the shell's `DOT_INIT_*`
/// reads plus `HOME`, in the order the function consults them.
pub struct LiveGitInputs<'a> {
    /// Live git directory under test (`DOT_INIT_GIT_DIR`).
    pub git_dir: &'a Path,
    /// Recorded device (`DOT_INIT_GIT_DEV`).
    pub git_dev: &'a str,
    /// Recorded inode (`DOT_INIT_GIT_INO`).
    pub git_ino: &'a str,
    /// When the record holding `git_dev`/`git_ino` was written, captured
    /// before this run rewrote it (`TransactionRecord::journaled`): a
    /// device renumbered since then still matches an inode born before it.
    pub journaled: Option<std::time::SystemTime>,
    /// Run nonce (`DOT_INIT_NONCE`; `adopted` skips the marker gate).
    pub nonce: &'a str,
    /// Expected repository identity (`DOT_INIT_IDENTITY`).
    pub identity: &'a str,
    /// Expected branch (`DOT_INIT_BRANCH`).
    pub branch: &'a str,
    /// Client root (`HOME`): steers git children and anchors the
    /// topology spellings.
    pub home: &'a Path,
}

/// Cross-lane predicates for [`live_git_matches_record`].
pub struct LiveGitDeps<'a> {
    /// Live `_dot_path_identity`.
    pub path_identity: &'a PathIdentity<'a>,
    /// Live `_dot_init_generation_marker_matches`.
    pub generation_matches: &'a GenerationMatches<'a>,
    /// Live `_dot_init_repo_identity`.
    pub repo_identity: &'a RepoIdentity<'a>,
}

/// Inputs for [`resume_transaction`]: the shell's two positionals
/// plus the `DOT_INIT_*` reads, with the live-check inputs nested.
pub struct ResumeInputs<'a> {
    /// Transaction directory (`$1`).
    pub transaction: &'a Path,
    /// Transaction record (`$2`).
    pub record: &'a Path,
    /// Recorded phase (`DOT_INIT_PHASE`).
    pub phase: &'a str,
    /// Backup root (`DOT_INIT_BACKUP`).
    pub backup: &'a Path,
    /// Run nonce (`DOT_INIT_NONCE`).
    pub nonce: &'a str,
    /// Live-check context (git dir, identity triple, branch, home).
    pub git: LiveGitInputs<'a>,
}

/// Cross-lane steps for [`resume_transaction`], including the
/// same-module live check's predicates.
pub struct ResumeDeps<'a> {
    /// Predicates for the checkout/complete live check.
    pub live: &'a LiveGitDeps<'a>,
    /// Live `_dot_init_record_phase`.
    pub record_phase: &'a RecordPhase<'a>,
    /// Live `_dot_init_move_conflicts`.
    pub move_conflicts: &'a MoveConflicts<'a>,
    /// Live `_dot_init_stage_git`.
    pub stage_git: &'a StageGit<'a>,
    /// Live `_dot_init_publish_git`.
    pub publish_git: &'a PublishGit<'a>,
    /// Live `_dot_init_publish_worktree`.
    pub publish_worktree: &'a PublishWorktree<'a>,
    /// Live `_dot_init_forward_converge`.
    pub forward_converge: &'a ForwardConverge<'a>,
    /// Live `_dot_init_publish_completed`.
    pub publish_completed: &'a PublishCompleted<'a>,
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`. `symlink_metadata` never follows,
/// so a link reports itself (the plan lane precedent).
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A regular file reached through any non-symlink chain: the shell's
/// bare `[[ -f $path ]]`, which follows symlinks. Used for the
/// journal gate and the stage-identity gate, where the shell tests
/// exactly this (the plan lane's restore-gate precedent).
fn is_file_following(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// A directory reached through any chain: the shell's bare
/// `[[ -d $path ]]` in the stage-cleanup gate, which — unlike the
/// predicate's real-directory gate — follows symlinks.
fn is_dir_following(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Raw bytes of a path, so `$HOME/` prefix work behaves like shell
/// string operations even on non-UTF8 inputs (the plan lane
/// precedent).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Append one `/`-separated leaf, like the shell's `"$HOME/$leaf"`.
/// Byte concatenation, so a `home` with a trailing slash keeps its
/// doubled separator exactly like the shell's expansion does (the
/// plan lane precedent for compared spellings).
fn home_child(home: &Path, leaf: &str) -> Vec<u8> {
    let mut joined = path_bytes(home).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(leaf.as_bytes());
    joined
}

/// The configuration keys [`live_git_matches_record`] reads, as one
/// `git config --get-regexp` pattern over Git's canonical key names (the
/// section and variable lowercased, the `origin` subsection verbatim).
const IDENTITY_CONFIG_KEYS: &str = r"^(remote\.origin\.url|core\.bare|core\.worktree)$";

/// The identity keys from one `git config -z --get-regexp` read.
///
/// The predicate used to spend three or four Git children on
/// `config --get-all remote.origin.url`, `config --bool core.bare`, and
/// `config core.worktree`. Each answer is reproduced from the one read:
/// every origin URL in order, and the last `core.bare` and `core.worktree`
/// values (Git's `--get` semantics). A key written without `=` has no value
/// (`-z` prints only its name), which `--get-all` prints as an empty line
/// and `--bool` reads as true.
#[derive(Debug, Default, PartialEq, Eq)]
struct IdentityConfig {
    origin_urls: Vec<Option<Vec<u8>>>,
    bare: Option<Option<Vec<u8>>>,
    worktree: Option<Vec<u8>>,
}

impl IdentityConfig {
    fn parse(raw: &[u8]) -> Self {
        let mut config = IdentityConfig::default();
        for entry in raw
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let (key, value) = match entry.iter().position(|byte| *byte == b'\n') {
                Some(split) => (&entry[..split], Some(entry[split + 1..].to_vec())),
                None => (entry, None),
            };
            match key {
                b"remote.origin.url" => config.origin_urls.push(value),
                b"core.bare" => config.bare = Some(value),
                // A valueless `core.worktree` is a config error Git
                // refuses before answering at all, so it never gets here.
                b"core.worktree" => config.worktree = Some(value.unwrap_or_default()),
                _ => {}
            }
        }
        config
    }

    /// The bytes `git config --get-all remote.origin.url` prints: one
    /// newline-terminated line per value. Rebuilding them (instead of
    /// counting entries) keeps the old framing exactly, so a URL holding a
    /// newline still splits into two lines and fails the one-URL rule.
    fn origin_urls_as_get_all(&self) -> Vec<u8> {
        let mut printed = Vec::new();
        for url in &self.origin_urls {
            printed.extend_from_slice(url.as_deref().unwrap_or_default());
            printed.push(b'\n');
        }
        printed
    }

    /// `git config --bool core.bare`: `None` when the key is unset or its
    /// last value is not a boolean (both made the old probe fail).
    fn bare(&self) -> Option<bool> {
        git_bool(self.bare.as_ref()?.as_deref())
    }
}

/// Git's boolean reading of one configuration value: no value is true, an
/// empty one false, then `true`/`yes`/`on` and `false`/`no`/`off` in any
/// case, then a decimal `int` (nonzero is true).
///
/// Git also accepts hexadecimal, octal, and `k`/`m`/`g`-suffixed integers;
/// those read as `None` here, so such a hand-written `core.bare` fails
/// closed (the client reads as foreign) instead of being trusted. Like Git,
/// a number outside the 32-bit `int` range is not a boolean.
fn git_bool(value: Option<&[u8]>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    if value.is_empty() {
        return Some(false);
    }
    let text = std::str::from_utf8(value).ok()?.to_ascii_lowercase();
    match text.as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => {
            // Git reads a leading zero as octal (`010` is 8, `08` is an
            // error); only the truth value matters here.
            let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
            let number = match digits.strip_prefix('0') {
                Some(octal) if !octal.is_empty() => i32::from_str_radix(octal, 8).ok()?,
                _ => text.parse::<i32>().ok()?,
            };
            Some(number != 0)
        }
    }
}

/// The identity keys' raw `--get-regexp -z` output: empty when none is set
/// (Git's exit 1), `None` when the config cannot be read at all (any other
/// failure, or the probe could not run).
fn git_dir_config(git_dir: &Path, home: &Path) -> Option<Vec<u8>> {
    crate::cancellation::check().ok()?;
    let mut command = crate::init_client_identity::host_git_command();
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(["config", "-z", "--get-regexp", IDENTITY_CONFIG_KEYS])
        .env("LC_ALL", "C")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    )
    .ok()?;
    crate::cancellation::check().ok()?;
    match output.status.code() {
        Some(0) => Some(output.stdout),
        Some(1) => Some(Vec::new()),
        _ => None,
    }
}

/// `HEAD`'s commit and branch from one child, as the predicate used to read
/// them from `rev-parse HEAD` and `symbolic-ref --short HEAD`.
///
/// `--abbrev-ref=loose` shortens with the same loose rule `symbolic-ref
/// --short` uses, so an ambiguous name keeps the same prefix. A detached
/// `HEAD` prints the literal `HEAD`, which no checked-out branch can be
/// called (Git refuses that branch name); it is rejected here as
/// `symbolic-ref` used to fail. An unborn branch fails the whole call, as
/// its `rev-parse HEAD` did.
///
/// A ref that makes the bare name `HEAD` ambiguous (`refs/heads/HEAD`,
/// `refs/tags/HEAD`, which `update-ref` or a `push origin HEAD:HEAD` slip
/// can create) makes `--abbrev-ref` print only an "ambiguous" warning and
/// no branch line while still exiting 0, so every command would read the
/// client as foreign. `HEAD` itself always resolves to `$GIT_DIR/HEAD`
/// first, so silencing that warning (`core.warnAmbiguousRefs=false`)
/// restores the line `symbolic-ref --short` used to print.
fn head_commit_and_branch(git_dir: &Path, home: &Path) -> Option<(Vec<u8>, Vec<u8>)> {
    let output = git_dir_output(
        git_dir,
        home,
        &[
            "-c",
            "core.warnAmbiguousRefs=false",
            "rev-parse",
            "HEAD",
            "--abbrev-ref=loose",
            "HEAD",
        ],
    )?;
    let output = chomp(output);
    let mut lines = output.split(|byte| *byte == b'\n');
    let (Some(commit), Some(branch), None) = (lines.next(), lines.next(), lines.next()) else {
        return None;
    };
    (branch != b"HEAD").then(|| (commit.to_vec(), branch.to_vec()))
}

/// Strip every trailing newline, like the shell's `$(...)`: the
/// substitution drops all trailing newlines and nothing else.
fn chomp(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

/// Frame scrubbed bytes as the shell's `mapfile -t` sees them: bytes
/// divide on `\n`, a missing trailing newline still yields its final
/// line, and a trailing newline adds no phantom element. Empty input
/// yields no elements at all.
fn split_lines(scrubbed: &[u8]) -> Vec<&[u8]> {
    if scrubbed.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = scrubbed.split(|byte| *byte == b'\n').collect();
    if scrubbed.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

/// Whether `commit` is a well-formed object id: the shell's
/// `[[ $commit =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]]`, covering
/// SHA-1 and SHA-256 generations (the pull-normalize lane precedent).
fn commit_valid(commit: &[u8]) -> bool {
    (commit.len() == 40 || commit.len() == 64) && commit.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// Run `git --git-dir <dir> <args>` with `LC_ALL=C` pinned and `HOME`
/// steered at the test home, like the shell probe inherits from its
/// harness. Captures stdout; `None` when git cannot start or reports
/// failure, like the shell's `|| return 1` on the substitution — and,
/// for the bare substitution in the `case`, like its empty expansion.
/// Git's own stderr is silenced (the candidate lane precedent).
fn git_dir_output(git_dir: &Path, home: &Path, args: &[&str]) -> Option<Vec<u8>> {
    crate::cancellation::check().ok()?;
    let mut command = crate::init_client_identity::host_git_command();
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        // Pinned deterministic leaf: Detach skips the host-wide completion scan;
        // cancellation still takes the strict path.
        crate::cleanup::LingerPolicy::Detach,
    )
    .ok()?;
    crate::cancellation::check().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Run `git -C <home> <args>` for the toplevel probe, which needs a
/// worktree context the `--git-dir` form cannot provide. Same
/// pinning and silencing as [`git_dir_output`].
fn git_home_output(home: &Path, args: &[&str]) -> Option<Vec<u8>> {
    crate::cancellation::check().ok()?;
    let mut command = crate::init_client_identity::host_git_command();
    command
        .arg("-C")
        .arg(home)
        .args(args)
        .env("LC_ALL", "C")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        // Pinned deterministic leaf: Detach skips the host-wide completion scan;
        // cancellation still takes the strict path.
        crate::cleanup::LingerPolicy::Detach,
    )
    .ok()?;
    crate::cancellation::check().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Physical directory, like the shell's `cd -P -- "$path" && pwd -P`.
/// `None` when the path is missing or unresolvable, like the shell's
/// failed `cd`.
fn physical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Whether any line of the file at `path` equals `needle`: the
/// shell's `grep -Fqx`. A missing trailing newline still yields its
/// final line for matching, and an unreadable file refuses — the
/// shell folds `grep`'s no-match and error exits into the same
/// `return 1`.
fn file_has_exact_line(path: &Path, needle: &[u8]) -> bool {
    let Ok(content) = std::fs::read(path) else {
        return false;
    };
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines.contains(&needle)
}

/// `rm -rf` for one path: missing paths succeed, symlinks remove the
/// link (never the target), directories recurse, anything else
/// unlinks. Twin of `cleanup::remove_one`, which is private to its
/// module; kept local so this lane does not reach into the cleanup
/// lane's internals.
fn remove_forced(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            context: "remove path",
            source,
        }),
        Ok(meta) => {
            if meta.file_type().is_symlink() || meta.file_type().is_file() {
                std::fs::remove_file(path).map_err(|source| Error::Io {
                    context: "remove path",
                    source,
                })
            } else if meta.file_type().is_dir() {
                std::fs::remove_dir_all(path).map_err(|source| Error::Io {
                    context: "remove path",
                    source,
                })
            } else {
                // Sockets, fifos, devices: unlink like `rm`.
                std::fs::remove_file(path).map_err(|source| Error::Io {
                    context: "remove path",
                    source,
                })
            }
        }
    }
}

/// Why a live client Git directory no longer matches its identity record,
/// as [`live_git_mismatch`] finds it: the first check that failed. Callers
/// that only need the verdict use [`live_git_matches_record`]; the
/// dispatcher names the reason and its recovery when it refuses the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityMismatch {
    /// The Git directory is missing or not a real directory (a symlink or
    /// a file now stands in its place).
    NotDirectory,
    /// Its inode changed, or its device did and it cannot be shown to
    /// predate the record: it was recreated, copied, or restored since
    /// `dot init` recorded it.
    Replaced,
    /// Its generation marker no longer names the recorded run.
    Generation,
    /// Its Git config could not be read (Git refused it, or the probe
    /// could not run).
    Config,
    /// It has no `origin` URL (`0`) or several (`n`), or one that is not
    /// UTF-8 (counted as `1`).
    OriginCount(usize),
    /// Its single `origin` URL names another repository (or none Dot
    /// recognizes); carries that URL.
    OriginRepository(String),
    /// `HEAD` is detached, unborn, or unreadable.
    Head,
    /// `HEAD` is on another branch; carries its name.
    Branch(String),
    /// `HEAD` resolved to a malformed commit id.
    Commit,
    /// `core.bare` is unset or not a boolean.
    Bare,
    /// A non-bare `$HOME/.dotfiles` does not name `$HOME` as its work tree;
    /// carries the configured value, if any.
    Worktree(Option<String>),
    /// An ordinary `$HOME/.git` checkout's top level is not `$HOME`.
    TopLevel,
    /// The Git directory is neither `$HOME/.dotfiles` nor `$HOME/.git`.
    Topology,
}

/// `_dot_init_live_git_matches_record`: re-verify that the live git
/// directory still matches the transaction record — the same object
/// (same inode, and same device unless a remount renumbered it; see
/// [`crate::persisted_identity`]), same generation marker (unless
/// adopted), exactly one origin
/// URL normalizing to the recorded identity, the recorded branch
/// checked out at a well-formed commit, and a trusted topology (a
/// bare `$HOME/.dotfiles`, a non-bare one rooted at `$HOME`, or an
/// ordinary `$HOME/.git` checkout whose toplevel is `$HOME`).
///
/// Silent like the shell: every git diagnostic is silenced and the
/// verdict is the return value. Cross-lane predicates arrive through
/// `deps`; non-UTF8 origin URLs refuse (the shell would compare raw
/// bytes, but origins are engine vocabulary and UTF-8 in practice —
/// the one documented narrowing, matching the candidate lane's
/// `from_utf8_lossy` boundary in the strict direction).
pub fn live_git_matches_record(inputs: &LiveGitInputs<'_>, deps: &LiveGitDeps<'_>) -> bool {
    live_git_mismatch(inputs, deps).is_none()
}

/// The check behind [`live_git_matches_record`], naming the first one that
/// failed (`None` when the live directory matches). One implementation for
/// both, so the reason a refusal reports is always the reason it refused.
pub fn live_git_mismatch(
    inputs: &LiveGitInputs<'_>,
    deps: &LiveGitDeps<'_>,
) -> Option<IdentityMismatch> {
    use IdentityMismatch as Why;

    if !is_real_dir(inputs.git_dir) {
        return Some(Why::NotDirectory);
    }
    let live = match (deps.path_identity)(inputs.git_dir) {
        Ok(identity) => identity,
        Err(_) => return Some(Why::NotDirectory),
    };
    let recorded = format!("{}:{}", inputs.git_dev, inputs.git_ino);
    if !crate::persisted_identity::matches(&recorded, &live, inputs.journaled) {
        return Some(Why::Replaced);
    }
    if inputs.nonce != "adopted" && !(deps.generation_matches)(inputs.git_dir) {
        return Some(Why::Generation);
    }
    // Every identity key in one child: an absent key set reads exactly
    // like the empty `--get-all` it replaces.
    // `--get-regexp` exits 1 when no key matches, which reads like the empty
    // `--get-all` it replaces; any other failure is an unreadable config,
    // which fails this check as it always did, now with its own reason.
    let config = match git_dir_config(inputs.git_dir, inputs.home) {
        Some(config) => IdentityConfig::parse(&config),
        None => return Some(Why::Config),
    };
    let raw = config.origin_urls_as_get_all();
    // NUL bytes cannot live in shell variables; scrub them the way
    // the plan lane's read loops do before framing.
    let scrubbed: Vec<u8> = raw.iter().copied().filter(|byte| *byte != 0).collect();
    let urls = split_lines(&scrubbed);
    if urls.len() != 1 {
        return Some(Why::OriginCount(urls.len()));
    }
    let Ok(origin) = std::str::from_utf8(urls[0]) else {
        return Some(Why::OriginCount(1));
    };
    match (deps.repo_identity)(origin) {
        Ok(identity) if identity == inputs.identity => {}
        _ => return Some(Why::OriginRepository(origin.to_string())),
    }
    let Some((commit, branch)) = head_commit_and_branch(inputs.git_dir, inputs.home) else {
        return Some(Why::Head);
    };
    if branch.as_slice() != inputs.branch.as_bytes() {
        return Some(Why::Branch(String::from_utf8_lossy(&branch).into_owned()));
    }
    if !commit_valid(&commit) {
        return Some(Why::Commit);
    }
    if path_bytes(inputs.git_dir) == home_child(inputs.home, ".dotfiles") {
        match config.bare() {
            Some(true) => {}
            Some(false) => {
                // `git config core.worktree`: the last value, and an unset
                // key fails the probe.
                let Some(worktree) = config.worktree.as_ref() else {
                    return Some(Why::Worktree(None));
                };
                let worktree = chomp(worktree.clone());
                if worktree.as_slice() != path_bytes(inputs.home) {
                    return Some(Why::Worktree(Some(
                        String::from_utf8_lossy(&worktree).into_owned(),
                    )));
                }
            }
            None => return Some(Why::Bare),
        }
    } else if path_bytes(inputs.git_dir) == home_child(inputs.home, ".git") {
        let top = git_home_output(inputs.home, &["rev-parse", "--show-toplevel"]).map(chomp);
        let home_real = physical(inputs.home);
        let top_real = top.and_then(|top| physical(Path::new(&OsString::from_vec(top))));
        if home_real.is_none() || home_real != top_real {
            return Some(Why::TopLevel);
        }
    } else {
        return Some(Why::Topology);
    }
    None
}

/// Whether `phase` is past the checkout: the client's Git directory and work
/// tree are published, so only convergence and the completion stamp remain
/// (`checkout`, `converging`, `complete`). Resuming such a transaction only
/// re-verifies the live client before converging, rollback refuses it, and a
/// clean `dot update` may finish it
/// ([`crate::init_client_engine::finish_converged`]).
pub fn committed(phase: &str) -> bool {
    matches!(phase, "checkout" | "converging" | "complete")
}

/// `_dot_init_resume_transaction`: replay a transaction forward from
/// its recorded phase. Early phases (`prepared` through `publishing`)
/// require the three journals, re-run backup, staging, and
/// publication, and drop a surviving stage whose identity still
/// carries this run's nonce; `checkout` and `converging` only
/// re-verify the live git dir; `complete` re-verifies, stamps
/// completion, removes the transaction, and returns without
/// converging. Anything else refuses. Every other path converges,
/// stamps completion, and removes the transaction, whose removal
/// verdict is the return value — except `complete`, where a failed
/// removal is ignored like the shell's `return 0`.
///
/// Silent like the shell: step diagnostics belong to the step
/// closures. Refusals (bad phase, missing journals, git mismatch,
/// changed stage identity) report [`Error::Usage`], the sibling-lane
/// convention for shell `return 1` gates; step failures propagate
/// from the injected closures unchanged.
pub fn resume_transaction(inputs: &ResumeInputs<'_>, deps: &ResumeDeps<'_>) -> Result<()> {
    match inputs.phase {
        "prepared" | "backing-up" | "backed-up" | "git-staging" | "git-staged" | "publishing" => {
            let tree = inputs.transaction.join("tree.tsv");
            let prior = inputs.transaction.join("prior.tsv");
            let conflicts = inputs.transaction.join("conflicts.tsv");
            if !(is_file_following(&tree)
                && is_file_following(&prior)
                && is_file_following(&conflicts))
            {
                return Err(Error::Usage {
                    message: "transaction journals are missing",
                });
            }
            (deps.record_phase)(inputs.record, "backing-up")?;
            (deps.move_conflicts)(&conflicts, inputs.backup)?;
            (deps.record_phase)(inputs.record, "backed-up")?;
            (deps.stage_git)(inputs.record)?;
            (deps.publish_git)(inputs.record)?;
            (deps.publish_worktree)(inputs.transaction)?;
            (deps.record_phase)(inputs.record, "checkout")?;
            let stage = inputs.backup.join("git-stage");
            if is_dir_following(&stage) && is_file_following(&stage.join("identity")) {
                let mut needle = b"nonce=".to_vec();
                needle.extend_from_slice(inputs.nonce.as_bytes());
                if !file_has_exact_line(&stage.join("identity"), &needle) {
                    return Err(Error::Usage {
                        message: "staged git identity changed",
                    });
                }
                remove_forced(&stage)?;
            }
        }
        "checkout" | "converging" => {
            if !live_git_matches_record(&inputs.git, deps.live) {
                return Err(Error::Usage {
                    message: "live git does not match record",
                });
            }
        }
        "complete" => {
            if !live_git_matches_record(&inputs.git, deps.live) {
                return Err(Error::Usage {
                    message: "live git does not match record",
                });
            }
            (deps.publish_completed)(inputs.record)?;
            // The shell's `rm -rf -- "$transaction"; return 0`
            // ignores removal failures (its bytes still diverge; see
            // the module docs), so discard the verdict here while the
            // tail below propagates it.
            remove_forced(inputs.transaction).ok();
            return Ok(());
        }
        _ => {
            return Err(Error::Usage {
                message: "unknown resume phase",
            });
        }
    }

    (deps.record_phase)(inputs.record, "converging")?;
    (deps.forward_converge)()?;
    (deps.record_phase)(inputs.record, "complete")?;
    (deps.publish_completed)(inputs.record)?;
    remove_forced(inputs.transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_test_support::TempDir;

    #[test]
    fn git_probe_runs_without_host_process_snapshot() {
        let dir = TempDir::new("leaf-git-no-scan").expect("scratch");
        let status = dot_test_support::git()
            .arg("init")
            .arg("-q")
            .current_dir(dir.path())
            .status()
            .expect("git init fixture");
        assert!(status.success());
        crate::cleanup::reset_global_process_snapshot_calls();
        let version = git_dir_output(&dir.path().join(".git"), dir.path(), &["--version"]);
        assert!(version.is_some_and(|out| !out.is_empty()));
        let home_version = git_home_output(dir.path(), &["--version"]);
        assert!(home_version.is_some_and(|out| !out.is_empty()));
        assert_eq!(
            crate::cleanup::global_process_snapshot_calls(),
            0,
            "read-only git probes must not pay a host-wide /proc walk"
        );
    }

    #[test]
    fn commit_ids_cover_both_generations() {
        assert!(commit_valid(b"3ce11c19d1469cd46267998d20816247697a363d"));
        assert!(commit_valid(b"3CE11C19D1469CD46267998D20816247697A363D"));
        assert!(commit_valid("f".repeat(64).as_bytes()));
    }

    #[test]
    fn commit_rejects_short_long_and_nonhex() {
        assert!(!commit_valid(b""));
        assert!(!commit_valid(b"3ce11c19d1469cd46267998d20816247697a363"));
        assert!(!commit_valid(b"3ce11c19d1469cd46267998d20816247697a363dd"));
        assert!(!commit_valid("g".repeat(40).as_bytes()));
        assert!(!commit_valid("0".repeat(63).as_bytes()));
        assert!(!commit_valid("0".repeat(65).as_bytes()));
        assert!(commit_valid("0".repeat(64).as_bytes()));
    }

    #[test]
    fn chomp_strips_newlines_only() {
        assert_eq!(chomp(b"true\n".to_vec()), b"true");
        assert_eq!(chomp(b"a\n\n\n".to_vec()), b"a");
        assert_eq!(chomp(b"a".to_vec()), b"a");
        assert_eq!(chomp(b"".to_vec()), b"".to_vec());
        assert_eq!(chomp(b"a\r\n".to_vec()), b"a\r");
        assert_eq!(chomp(b"a\nb\n".to_vec()), b"a\nb");
    }

    #[test]
    fn split_lines_frames_mapfile() {
        assert!(split_lines(b"").is_empty());
        assert_eq!(split_lines(b"a\n"), vec![b"a".as_slice()]);
        assert_eq!(split_lines(b"a"), vec![b"a".as_slice()]);
        assert_eq!(split_lines(b"a\n\n"), vec![b"a".as_slice(), b"".as_slice()]);
    }

    #[test]
    fn exact_line_matches_grep_x() {
        let dir = TempDir::new("resume-lines").expect("temp dir");
        let file = dir.path().join("identity");
        std::fs::write(&file, "nonce=abc\ncommit=def\n").expect("write");
        assert!(file_has_exact_line(&file, b"nonce=abc"));
        assert!(!file_has_exact_line(&file, b"nonce=ab"));
        assert!(!file_has_exact_line(&file, b"nonce=abc\ncommit"));
        std::fs::write(&file, "nonce=abc").expect("write");
        assert!(file_has_exact_line(&file, b"nonce=abc"));
        std::fs::write(&file, "").expect("write");
        assert!(!file_has_exact_line(&file, b"nonce=abc"));
        assert!(!file_has_exact_line(
            &dir.path().join("missing"),
            b"nonce=abc"
        ));
    }

    #[test]
    fn remove_forced_mirrors_rm_rf() {
        let dir = TempDir::new("resume-rm").expect("temp dir");
        // Missing paths succeed.
        remove_forced(&dir.path().join("missing")).expect("missing");
        // Files unlink.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").expect("write");
        remove_forced(&file).expect("file");
        assert!(!file.exists());
        // Trees recurse.
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("sub")).expect("mkdir");
        std::fs::write(tree.join("sub").join("f"), "x").expect("write");
        remove_forced(&tree).expect("tree");
        assert!(!tree.exists());
        // Symlinks remove the link, never the target.
        let target = dir.path().join("target");
        std::fs::write(&target, "x").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        remove_forced(&link).expect("link");
        assert!(!link.exists() || !std::fs::symlink_metadata(&link).is_ok());
        assert!(target.is_file());
    }
}
