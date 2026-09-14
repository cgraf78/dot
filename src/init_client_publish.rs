//! Published-state recovery and worktree publication for `lib/dot/init-client.sh`.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the six contiguous functions from
//! `_dot_init_published_stage_matches` through `_dot_init_single_origin`
//! in file order (lines 1279-1417): the leaf-stage validator
//! ([`published_stage_matches`]), the prepared-intent validator
//! ([`published_intent_matches`]), the published-stage reaper
//! ([`cleanup_published_stage`]), the per-entry worktree publisher
//! ([`publish_worktree`]), the update convergence entry
//! ([`forward_converge`]), and the single-origin reader
//! ([`single_origin`]).
//!
//! Lane map, so the integrator can stack without overlap: the
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family on
//! `rust-port-slice-41` (`init_client_identity`), the git-generation
//! binding on `rust-port-slice-43` (`init_client_generation`), the
//! per-entry staging family on `rust-port-slice-46`
//! (`init_client_entry`: `entry_intent`, `entry_stage_only_next`,
//! the stage-claim readers, and the private-directory matchers this
//! module takes as closures), the candidate planning family on
//! `rust-port-slice-48` (`init_client_candidate`:
//! `candidate_matches_git`, `path_state_matches`, `prior_record`),
//! the transaction record journal on `rust-port-slice-51`
//! (`init_client_records`) and `rust-port-slice-54`
//! (`init_client_record`), the deletion-parking family on
//! `rust-port-slice-55` (`init_client_delete`), and the plan review
//! and conflict safekeeping on `rust-port-slice-62`
//! (`init_client_plan`). The publishing siblings `publish_intent`
//! and `publish_one` stay for later slices, as do the rollback,
//! resume, status, and command-dispatch families. The file-generic
//! `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state, absorbed into
//! [`Result`] the way earlier slices absorb engine diagnostics).
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals, the worktree root from `HOME`, and the
//! process working directory from the caller. Library code must not
//! read process environment behind the engine, so the transaction
//! directory, home, backup root, and git binding cross here as
//! explicit parameters ([`PublishGit`] carries the git directory,
//! commit, branch, and working directory together), and
//! `REPLY`-carried outputs surface as return values. Cross-lane
//! predicates the shell calls by name cross as closures
//! ([`StageHooks`], [`PublishHooks`], [`ConvergeHooks`]), the way
//! the plan lane takes its verifier. `LC_ALL=C` is pinned around
//! every child process so git output reads English and byte-ordered
//! on both engines, and `HOME` is steered at the test home the way
//! the plan lane steers its probes.
//!
//! Error boundary: every shell refusal in these six functions is a
//! bare `return 1` with no diagnostic of its own, so every refusal
//! here surfaces as [`Error::Usage`]; diagnostics printed by callees
//! (`_dot_client_select`, the update engine) stay owned by their
//! lanes. Nothing here prints: the two rendered outputs
//! ([`published_intent_matches`], [`single_origin`]) return their
//! bytes for the caller to emit, keeping this module free of ambient
//! file descriptors.
//!
//! Byte-fidelity boundary: every `$HOME/$path` join concatenates
//! bytes like the shell, preserving a doubled separator on
//! trailing-slash inputs instead of normalizing it away (the plan
//! lane precedent). Manifest text crosses the UTF-8 boundary with
//! `from_utf8_lossy`, the plan lane precedent, so non-UTF8 journal
//! bytes can diverge from the shell exactly the way they do on
//! sibling lanes.
//!
//! `read` exactness: tree and conflict rows parse with the shell's
//! `IFS=$'\t' read -r` semantics — leading tabs stripped, tab runs
//! collapsing between fields, the last variable keeping the raw
//! remainder with its tabs intact, missing variables reading empty —
//! not a plain tab split (see `read_row`). Both loops read their
//! journals directly (`done <file`), so an unterminated final line
//! never runs its body (see `read_loop_lines`); the conflicts
//! journal is re-read per fresh entry, exactly like the shell's
//! inner redirect.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::temp;

/// Stage directory mode the publish family requires: the shell's
/// literal `700` argument on every private-directory gate below.
const STAGE_MODE: &str = "700";

/// Claim-marker file name inside a stage directory: the shell's
/// `_dot_init_stage_claim_file` body verbatim.
const STAGE_CLAIM_NAME: &str = ".dot-init-stage-claim-v1";

/// Claim kind this family stamps and verifies: the shell's literal
/// `entry` argument on every claim call below.
const STAGE_CLAIM_KIND: &str = "entry";

/// Progress denominator the convergence entry announces: the
/// shell's `_ui_begin 5` literal.
const CONVERGE_TOTAL: u32 = 5;

/// A published entry intent: the shell's `REPLY` from
/// `_dot_init_entry_intent` split into its six tab fields. This is a
/// byte-local twin of the entry lane's `EntryIntent`, kept local
/// because that lane is unmerged; the fields mirror it case for
/// case, including the `-` spellings the `pending` phase carries.
pub struct IntentRecord {
    /// `pending`, `staged`, or `prepared`: how far publication got.
    pub phase: String,
    /// Home-relative stage directory bound to this entry.
    pub stage: String,
    /// Stage device (`-` while pending).
    pub dev: String,
    /// Stage inode (`-` while pending).
    pub ino: String,
    /// Published-file device (`-` until prepared).
    pub next_dev: String,
    /// Published-file inode (`-` until prepared).
    pub next_ino: String,
}

/// A prior-worktree record: the shell's `REPLY` from
/// `_dot_init_prior_record` split into its six tab fields. This is a
/// byte-local twin of the candidate lane's record, kept local
/// because that lane is unmerged.
pub struct PriorRecord {
    /// `absent`, `regular`, `symlink`, or `directory`.
    pub kind: String,
    /// Recorded device.
    pub dev: String,
    /// Recorded inode.
    pub ino: String,
    /// Recorded permission bits.
    pub mode: String,
    /// Recorded size.
    pub size: String,
    /// Content identity (blob oid, link target, or `-`).
    pub value: String,
}

/// Git binding for one publication run: the shell's `DOT_INIT_*`
/// globals the publisher reads, plus the working directory the
/// shell inherits from its caller for the closing git sequence.
pub struct PublishGit<'a> {
    /// The shell's `DOT_INIT_GIT_DIR`.
    pub git_dir: &'a Path,
    /// The shell's `DOT_INIT_COMMIT`.
    pub commit: &'a str,
    /// The shell's `DOT_INIT_BRANCH`.
    pub branch: &'a str,
    /// Working directory for the closing git sequence (the shell's
    /// inherited cwd, pinned by the engine so both sides agree).
    pub work_dir: &'a Path,
}

/// Private-directory gate: the entry lane's
/// `_dot_init_private_directory_matches` by position
/// (`path identity mode`), injected because that lane is unmerged.
pub type PrivateDirectoryMatches<'a> = dyn Fn(&Path, &str, &str) -> bool + 'a;

/// Stage-content gate: the entry lane's
/// `_dot_init_entry_stage_only_next`, injected because that lane is
/// unmerged.
pub type StageOnlyNext<'a> = dyn Fn(&Path) -> bool + 'a;

/// Claim-content gate: the entry lane's
/// `_dot_init_stage_claim_matches` by position
/// (`stage kind path`), injected because that lane is unmerged.
pub type StageClaimMatches<'a> = dyn Fn(&Path, &str, &str) -> bool + 'a;

/// Empty-directory gate: the entry lane's
/// `_dot_init_private_empty_directory_matches` by position
/// (`path identity mode`), injected because that lane is unmerged.
pub type PrivateEmptyDirectoryMatches<'a> = dyn Fn(&Path, &str, &str) -> bool + 'a;

/// Claim reaper: the entry lane's `_dot_init_stage_claim_remove` by
/// position (`stage kind path`), injected because that lane is
/// unmerged.
pub type StageClaimRemove<'a> = dyn Fn(&Path, &str, &str) -> Result<()> + 'a;

/// Intent-record reader: the entry lane's `_dot_init_entry_intent`
/// by position (`file mode oid path`), injected because that lane
/// is unmerged. Tests feed either a stub or a closure that runs the
/// live shell predicate, so the orchestration below stays
/// differentially covered either way.
pub type EntryIntentFn<'a> = dyn Fn(&Path, &str, &str, &str) -> Result<IntentRecord> + 'a;

/// Prior-record reader: the candidate lane's `_dot_init_prior_record`
/// by position (`prior path`), injected because that lane is
/// unmerged.
pub type PriorRecordFn<'a> = dyn Fn(&Path, &str) -> Result<PriorRecord> + 'a;

/// Worktree-blob matcher: the candidate lane's
/// `_dot_init_candidate_matches_git` by position
/// (`mode oid path`, git binding curried by the engine), injected
/// because that lane is unmerged.
pub type CandidateMatchesGit<'a> = dyn Fn(&str, &str, &str) -> bool + 'a;

/// Worktree-state matcher: the candidate lane's
/// `_dot_init_path_state_matches` by position
/// (`target kind dev ino mode size value`), injected because that
/// lane is unmerged. Same shape as the plan lane's twin.
pub type StateMatches<'a> = dyn Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool + 'a;

/// Intent publisher: the later publish lane's `_dot_init_publish_intent`
/// by position (`file mode oid path`), injected because that lane is
/// unmerged.
pub type PublishIntentFn<'a> = dyn Fn(&Path, &str, &str, &str) -> Result<()> + 'a;

/// Single-entry publisher: the later publish lane's
/// `_dot_init_publish_one` by position
/// (`transaction intent mode oid path`, git binding curried by the
/// engine), injected because that lane is unmerged.
pub type PublishOneFn<'a> = dyn Fn(&Path, &Path, &str, &str, &str) -> Result<()> + 'a;

/// The entry-family gates [`published_stage_matches`] and
/// [`cleanup_published_stage`] verify through, bundled so the two
/// callers share one parameter.
pub struct StageHooks<'a> {
    /// See [`PrivateDirectoryMatches`].
    pub private_directory_matches: &'a PrivateDirectoryMatches<'a>,
    /// See [`StageOnlyNext`].
    pub stage_only_next: &'a StageOnlyNext<'a>,
    /// See [`StageClaimMatches`].
    pub stage_claim_matches: &'a StageClaimMatches<'a>,
    /// See [`PrivateEmptyDirectoryMatches`].
    pub private_empty_directory_matches: &'a PrivateEmptyDirectoryMatches<'a>,
    /// See [`StageClaimRemove`].
    pub stage_claim_remove: &'a StageClaimRemove<'a>,
}

/// The candidate/publish-family collaborators [`publish_worktree`]
/// orchestrates, bundled so the per-entry loop takes one parameter.
pub struct PublishHooks<'a> {
    /// See [`PriorRecordFn`].
    pub prior_record: &'a PriorRecordFn<'a>,
    /// See [`CandidateMatchesGit`].
    pub candidate_matches_git: &'a CandidateMatchesGit<'a>,
    /// See [`StateMatches`].
    pub path_state_matches: &'a StateMatches<'a>,
    /// See [`PublishIntentFn`].
    pub publish_intent: &'a PublishIntentFn<'a>,
    /// See [`PublishOneFn`].
    pub publish_one: &'a PublishOneFn<'a>,
    /// See [`EntryIntentFn`].
    pub entry_intent: &'a EntryIntentFn<'a>,
    /// The entry-family gates (shared with the stage validators).
    pub stages: StageHooks<'a>,
}

/// The update-engine collaborators [`forward_converge`] sequences,
/// bundled so the entry point takes one parameter. Diagnostics
/// printed by these callees stay owned by their lanes; only the
/// sequencing, the provider scoping, and the status threading live
/// here.
pub struct ConvergeHooks<'a> {
    /// The shell's `_dot_client_select` (runs bare: the verdict is
    /// sequenced past, never short-circuited).
    pub select_client: &'a (dyn Fn() -> Result<()> + 'a),
    /// The shell's `dot_config_load`.
    pub load_config: &'a (dyn Fn() -> Result<()> + 'a),
    /// The shell's `_ui_begin` (receives `CONVERGE_TOTAL`).
    pub begin_ui: &'a (dyn Fn(u32) + 'a),
    /// The shell's `_dot_update_sync_repos` (receives the
    /// skip-provider flag the shell scopes dynamically).
    pub sync_repos: &'a (dyn Fn(bool) -> Result<()> + 'a),
    /// The shell's `_dot_update_finalize` (receives the threaded
    /// `0`/`1` status plus the skip-provider flag: the shell's
    /// override stays in scope through finalize, so the flag
    /// crosses explicitly here too instead of mutating process
    /// environment behind the engine).
    pub finalize: &'a (dyn Fn(i32, bool) -> Result<()> + 'a),
}

/// Which git binding reads the origin URL: the shell's
/// `separate` branch (`_base_git`, a `--git-dir`/`--work-tree`
/// invocation) versus any other `command_kind` (`git -C "$HOME"`).
pub enum OriginScope<'a> {
    /// Separate topology: read through this git directory with the
    /// client root as the work tree.
    Separate {
        /// The shell's `DOT_CLIENT_GIT_DIR`.
        git_dir: &'a Path,
    },
    /// Ordinary topology (and every other `command_kind` spelling):
    /// read through `git -C` at home.
    Ordinary,
}

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A regular file reached through any non-symlink chain: the shell's
/// bare `[[ -f $path ]]`, which follows symlinks. Used only for the
/// journal gate, where the shell tests exactly this.
fn is_file_following(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Raw bytes of a path, so `$HOME/` prefix work and `$HOME/$path`
/// joins behave like shell string operations even when `home` has a
/// trailing slash (the doubled separator is preserved, never
/// normalized away).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Append one `/`-separated leaf, like the shell's `"$base/$leaf"`.
/// Byte concatenation, so a `base` with a trailing slash keeps its
/// doubled separator exactly like the shell's expansion does.
fn join2(base: &Path, leaf: &str) -> PathBuf {
    let mut joined = path_bytes(base).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(leaf.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// Append a dotted suffix with no separator, like the shell's
/// `$transaction/publish-intent.$hash`.
fn suffixed(base: &Path, suffix: &str) -> PathBuf {
    let mut joined = path_bytes(base).to_vec();
    joined.extend_from_slice(suffix.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// Mirror of `IFS=$'\t' read -r v1..vN`: leading tabs are stripped,
/// tab runs collapse between fields, the last slot keeps the raw
/// remainder with its tabs intact, and missing slots read empty. A
/// plain `splitn` misassigns rows with leading or doubled tabs
/// (probed against bash on the plan lane), so the manifest loops
/// use this instead. `out` must hold exactly the loop's variable
/// count; the remainder arm makes an oversized row land in the last
/// slot, the way extra words append to the shell's final variable.
fn read_row<'line>(line: &'line str, out: &mut [&'line str]) {
    let Some((last, head)) = out.split_last_mut() else {
        return;
    };
    let mut rest = line.trim_start_matches('\t');
    for slot in head {
        match rest.find('\t') {
            Some(position) => {
                *slot = &rest[..position];
                rest = rest[position..].trim_start_matches('\t');
            }
            None => {
                *slot = rest;
                rest = "";
            }
        }
    }
    *last = rest;
}

/// Frame file bytes as a shell `while read` loop over a direct
/// `done <file` redirect iterates them: bytes divide on `\n` and the
/// final chunk is always dropped — it is either the phantom after a
/// trailing newline or an unterminated tail whose variables `read`
/// assigns but whose body never runs (probed against bash on the
/// plan lane). Feeds the tree and conflicts loops. NUL bytes never
/// survive `read` (bash drops them silently, probed), so they are
/// stripped up front.
fn read_loop_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    lines.pop();
    lines.iter().map(|line| line.replace('\0', "")).collect()
}

/// The shell's `_dot_init_safe_value`: nonempty with no tab,
/// newline, or carriage return.
fn safe_value(value: &[u8]) -> bool {
    !value.is_empty()
        && !value.contains(&b'\t')
        && !value.contains(&b'\n')
        && !value.contains(&b'\r')
}

/// Frame command-substitution bytes as `mapfile -t` reports them:
/// bytes divide on `\n`, a missing trailing newline still yields its
/// final line (unlike `read`, `mapfile` keeps the unterminated
/// tail), and a trailing newline adds no phantom empty line. Feeds
/// [`single_origin`], where the shell counts exactly these rows.
fn mapfile_lines(output: &[u8]) -> Vec<&[u8]> {
    if output.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = output.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// Shell prefix dispatch for the conflict scan: the shell's
/// `[[ $path == "$root" || $path == "$root"/* ]]`, where the quoted
/// root matches literally. A relative path never equals or dives
/// under an empty root (the `/*` arm wants a leading slash), so
/// empty roots refuse on both engines.
fn under_root(path: &str, root: &str) -> bool {
    if path == root {
        return true;
    }
    if root.is_empty() {
        return false;
    }
    path.len() > root.len() && path.starts_with(root) && path.as_bytes()[root.len()] == b'/'
}

/// Claim-marker path: the shell's `_dot_init_stage_claim_file` body
/// (`REPLY=$1/.dot-init-stage-claim-v1`) as bytes, so trailing-slash
/// stages keep their doubled separator.
fn stage_claim_file(stage: &Path) -> PathBuf {
    join2(stage, STAGE_CLAIM_NAME)
}

/// `git hash-object --stdin` over `input`, with `LC_ALL=C` pinned
/// and `HOME` steered at the client root like the plan lane's
/// probes. Reports the bare oid: command substitution strips the
/// trailing newline on both engines. Any failure (missing git, a
/// broken pipe) is a publication refusal, exactly like the shell's
/// `|| return 1` on the substitution.
fn git_hash_stdin(input: &[u8], home: &Path, work_dir: &Path) -> Result<String> {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("hash-object")
        .arg("--stdin")
        .env("LC_ALL", "C")
        .env("HOME", home)
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::Usage {
            message: "cannot hash publish path",
        })?;
    let status_ok = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(input).is_ok())
        .unwrap_or(false);
    let output = child.wait_with_output().map_err(|_| Error::Usage {
        message: "cannot hash publish path",
    })?;
    if !status_ok || !output.status.success() {
        return Err(Error::Usage {
            message: "cannot hash publish path",
        });
    }
    let mut oid = output.stdout;
    while oid.last() == Some(&b'\n') {
        oid.pop();
    }
    String::from_utf8(oid).map_err(|_| Error::Usage {
        message: "cannot hash publish path",
    })
}

/// `git --git-dir=<dir> <args>` with `LC_ALL=C` pinned and `HOME`
/// steered at the client root, run from the engine-pinned working
/// directory. Captures stdout; any failure is a publication refusal,
/// like the shell's `|| return 1` after each closing command. Git's
/// own stderr is silenced (the candidate lane precedent): these
/// commands print nothing on success, and their diagnostics stay
/// owned by later lanes.
fn git_dir_run(git: &PublishGit<'_>, home: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git.git_dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("HOME", home)
        .current_dir(git.work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| Error::Usage {
            message: "publish git sequence failed",
        })?;
    if !output.status.success() {
        return Err(Error::Usage {
            message: "publish git sequence failed",
        });
    }
    Ok(output.stdout)
}

/// `_dot_init_published_stage_matches`: a published leaf stage is a
/// mode-700 private directory holding nothing but an optional claim
/// marker and never the in-progress `next` file. With a claim, the
/// claim must verify for this path; without one, the directory must
/// be empty. A pure predicate: every gate answers through the
/// entry-family hooks, so tests can feed the live shell functions.
pub fn published_stage_matches(
    stage: &Path,
    expected_identity: &str,
    path: &str,
    hooks: &StageHooks<'_>,
) -> bool {
    if !(hooks.private_directory_matches)(stage, expected_identity, STAGE_MODE) {
        return false;
    }
    if !(hooks.stage_only_next)(stage) {
        return false;
    }
    if exists_lexical(&join2(stage, "next")) {
        return false;
    }
    let marker = stage_claim_file(stage);
    if exists_lexical(&marker) {
        (hooks.stage_claim_matches)(stage, STAGE_CLAIM_KIND, path)
    } else {
        (hooks.private_empty_directory_matches)(stage, expected_identity, STAGE_MODE)
    }
}

/// `_dot_init_published_intent_matches`: a prepared intent whose
/// recorded `next` identity still names the live `$HOME/$path`
/// proves this path already published. A present stage must still
/// verify; a consumed stage (removed after publication) skips that
/// gate, exactly like the shell's existence check. Returns the
/// `REPLY` bytes (`stage\tidentity`, no trailing newline) for the
/// caller to split, so the parity tests can compare them byte for
/// byte against the live shell.
///
/// A failed identity read refuses rather than errors: the shell's
/// `$(... || true)` compares empty against the recorded identity.
pub fn published_intent_matches(
    intent: &Path,
    mode: &str,
    oid: &str,
    path: &str,
    home: &Path,
    entry_intent: &EntryIntentFn<'_>,
    hooks: &StageHooks<'_>,
) -> Result<Vec<u8>> {
    let record = entry_intent(intent, mode, oid, path)?;
    if record.phase != "prepared" {
        return Err(Error::Usage {
            message: "published intent is not prepared",
        });
    }
    let target = join2(home, path);
    let live = temp::path_identity(&target)
        .ok()
        .map(temp::identity_string)
        .unwrap_or_default();
    if live != format!("{}:{}", record.next_dev, record.next_ino) {
        return Err(Error::Usage {
            message: "published file changed under its intent",
        });
    }
    let stage = join2(home, &record.stage);
    let identity = format!("{}:{}", record.dev, record.ino);
    if exists_lexical(&stage) && !published_stage_matches(&stage, &identity, path, hooks) {
        return Err(Error::Usage {
            message: "published stage changed under its intent",
        });
    }
    let mut reply = path_bytes(&stage).to_vec();
    reply.push(b'\t');
    reply.extend_from_slice(identity.as_bytes());
    Ok(reply)
}

/// `_dot_init_cleanup_published_stage`: reap a verified published
/// stage — drop its consumed-or-live claim, require the directory
/// empty, and remove it. A missing stage is a successful no-op, like
/// the shell's opening gate.
pub fn cleanup_published_stage(
    stage: &Path,
    expected_identity: &str,
    path: &str,
    hooks: &StageHooks<'_>,
) -> Result<()> {
    if !exists_lexical(stage) {
        return Ok(());
    }
    if !published_stage_matches(stage, expected_identity, path, hooks) {
        return Err(Error::Usage {
            message: "published stage does not match",
        });
    }
    if exists_lexical(&stage_claim_file(stage)) {
        (hooks.stage_claim_remove)(stage, STAGE_CLAIM_KIND, path)?;
    }
    if !(hooks.private_empty_directory_matches)(stage, expected_identity, STAGE_MODE) {
        return Err(Error::Usage {
            message: "published stage is not empty",
        });
    }
    std::fs::remove_dir(stage).map_err(|_| Error::Usage {
        message: "cannot remove published stage",
    })
}

/// `_dot_init_publish_worktree`: publish every tree entry whose
/// worktree state moved since the prior snapshot. Entries already
/// matching the prior snapshot skip; entries already matching the
/// candidate with a verified prepared intent reap their stage and
/// skip; anything else must be absent at home, must prove its
/// backup lineage through the conflicts journal (unless brand new),
/// and flows through intent publication and single-entry
/// publication. The closing git sequence then advances the branch,
/// HEAD, and index to the published commit; a stale index refresh
/// is tolerated, exactly like the shell's `|| true`.
///
/// Both journals must be real files up front. Every per-row refusal
/// is silent (`return 1` in the shell), so every refusal here is
/// [`Error::Usage`]; fallible collaborators propagate through `?`
/// the way the plan lane propagates its provisioner.
pub fn publish_worktree(
    transaction: &Path,
    home: &Path,
    backup: &Path,
    git: &PublishGit<'_>,
    hooks: &PublishHooks<'_>,
) -> Result<()> {
    let tree = join2(transaction, "tree.tsv");
    let prior = join2(transaction, "prior.tsv");
    if !(is_file_following(&tree) && is_file_following(&prior)) {
        return Err(Error::Usage {
            message: "publish journals are missing",
        });
    }
    let content = std::fs::read(&tree).map_err(|_| Error::Usage {
        message: "cannot read publish tree",
    })?;
    let text = String::from_utf8_lossy(&content);
    let intents = join2(transaction, "publish-intent");
    for line in read_loop_lines(&text) {
        let mut fields = ["", "", ""];
        read_row(&line, &mut fields);
        let [mode, oid, path] = fields;
        let record = (hooks.prior_record)(&prior, path)?;
        let hash = git_hash_stdin(path.as_bytes(), home, git.work_dir)?;
        let intent_file = suffixed(&intents, &format!(".{hash}"));
        if (hooks.candidate_matches_git)(mode, oid, path) {
            if (hooks.path_state_matches)(
                &join2(home, path),
                &record.kind,
                &record.dev,
                &record.ino,
                &record.mode,
                &record.size,
                &record.value,
            ) {
                continue;
            }
            let reply = published_intent_matches(
                &intent_file,
                mode,
                oid,
                path,
                home,
                hooks.entry_intent,
                &hooks.stages,
            )
            .map_err(|_| Error::Usage {
                message: "published entry cannot be recovered",
            })?;
            let divider = reply
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or(Error::Usage {
                    message: "published entry cannot be recovered",
                })?;
            let stage = PathBuf::from(OsString::from_vec(reply[..divider].to_vec()));
            let identity =
                std::str::from_utf8(&reply[divider + 1..]).map_err(|_| Error::Usage {
                    message: "published entry cannot be recovered",
                })?;
            cleanup_published_stage(&stage, identity, path, &hooks.stages)?;
            continue;
        }
        if exists_lexical(&join2(home, path)) {
            return Err(Error::Usage {
                message: "publish target is occupied",
            });
        }
        if record.kind != "absent" {
            let conflicts = join2(transaction, "conflicts.tsv");
            let backup_content = std::fs::read(&conflicts).map_err(|_| Error::Usage {
                message: "cannot read publish conflicts",
            })?;
            let backup_text = String::from_utf8_lossy(&backup_content);
            let mut found = false;
            for conflict in read_loop_lines(&backup_text) {
                let mut conflict_fields = ["", "", "", "", "", "", ""];
                read_row(&conflict, &mut conflict_fields);
                let [root, kind, dev, ino, conflict_mode, size, value] = conflict_fields;
                if !under_root(path, root) {
                    continue;
                }
                if !(hooks.path_state_matches)(
                    &join2(backup, root),
                    kind,
                    dev,
                    ino,
                    conflict_mode,
                    size,
                    value,
                ) {
                    return Err(Error::Usage {
                        message: "backup lineage changed during publish",
                    });
                }
                found = true;
                break;
            }
            if !found {
                return Err(Error::Usage {
                    message: "publish entry has no backup lineage",
                });
            }
        }
        (hooks.publish_intent)(&intent_file, mode, oid, path)?;
        (hooks.publish_one)(transaction, &intent_file, mode, oid, path)?;
    }
    git_dir_run(git, home, &["read-tree", git.commit])?;
    let head = format!("refs/heads/{}", git.branch);
    git_dir_run(git, home, &["update-ref", &head, git.commit])?;
    git_dir_run(git, home, &["symbolic-ref", "HEAD", &head])?;
    let _ = git_dir_run(git, home, &["update-index", "--refresh"]);
    Ok(())
}

/// `_dot_init_forward_converge`: select the client binding, load the
/// committed config, announce the five-stage convergence, sync the
/// repositories, and finalize with the threaded status. With
/// `skip_provider`, the dependency provider reads `none` from the
/// announcement through finalize, but the committed config still
/// parses first and stays authoritative later — the shell scopes a
/// local override, and here the flag crosses explicitly to the sync
/// and finalize collaborators instead of mutating process
/// environment behind the engine. (The announcement itself provably
/// ignores the provider — `_ui_begin` only assigns progress
/// counters — so it takes no flag.) A failed sync threads status
/// `1` into finalize (the sync diagnostic itself stays owned by its
/// lane); the config load short-circuits, and the return code is
/// finalize's.
///
/// The selection runs bare, exactly like the shell: its verdict
/// never short-circuits the sequencing (both `|| return` call sites
/// run with errexit suppressed, so a selection failure flows into
/// config and beyond there too; only the bare call site leans on
/// ambient errexit, which the engine owns). Selection diagnostics
/// stay owned by the selecting lane — it emits before refusing, the
/// acquire precedent — so the ignored verdict drops no bytes here.
pub fn forward_converge(skip_provider: bool, hooks: &ConvergeHooks<'_>) -> Result<()> {
    let _ = (hooks.select_client)();
    (hooks.load_config)()?;
    (hooks.begin_ui)(CONVERGE_TOTAL);
    let mut status = 0;
    if (hooks.sync_repos)(skip_provider).is_err() {
        status = 1;
    }
    (hooks.finalize)(status, skip_provider)?;
    Ok(())
}

/// `_dot_init_single_origin`: report the lone `remote.origin.url`
/// (plus its newline) or refuse. A `separate` command kind reads
/// through the base git binding; every other spelling reads through
/// `git -C` at home. Zero or several URLs, an unreadable config,
/// and an unsafe value (empty or carrying tab, newline, or carriage
/// return) all refuse with no output, like the shell's bare
/// `return 1`s — git's own stderr is silenced.
pub fn single_origin(scope: &OriginScope<'_>, home: &Path) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    match scope {
        OriginScope::Separate { git_dir } => {
            command
                .arg("--git-dir")
                .arg(git_dir)
                .arg("--work-tree")
                .arg(home);
        }
        OriginScope::Ordinary => {
            command.arg("-C").arg(home);
        }
    }
    let output = command
        .arg("config")
        .arg("--local")
        .arg("--get-all")
        .arg("remote.origin.url")
        .env("LC_ALL", "C")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| Error::Usage {
            message: "cannot read origin url",
        })?;
    if !output.status.success() {
        return Err(Error::Usage {
            message: "cannot read origin url",
        });
    }
    let urls = mapfile_lines(&output.stdout);
    if urls.len() != 1 {
        return Err(Error::Usage {
            message: "origin url is not single",
        });
    }
    if !safe_value(urls[0]) {
        return Err(Error::Usage {
            message: "origin url is unsafe",
        });
    }
    let mut line = urls[0].to_vec();
    line.push(b'\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::{mapfile_lines, read_loop_lines, read_row, safe_value, under_root};

    /// Split one line into exactly `out.len()` variables.
    fn split<const N: usize>(line: &str) -> [String; N] {
        let mut out: [&str; N] = [""; N];
        read_row(line, &mut out);
        out.map(str::to_string)
    }

    #[test]
    fn read_row_matches_shell_ifs_tab() {
        // Probed against bash `IFS=$'\t' read -r a b c` on the plan
        // lane: leading tabs strip, tab runs collapse, the last
        // variable keeps the raw remainder, missing variables read
        // empty.
        assert_eq!(
            split::<3>("100644\toid\tp").as_slice(),
            ["100644", "oid", "p"]
        );
        assert_eq!(split::<3>("m\to").as_slice(), ["m", "o", ""]);
        assert_eq!(split::<3>("a\tb\tc\td").as_slice(), ["a", "b", "c\td"]);
        assert_eq!(split::<3>("\ta\tb").as_slice(), ["a", "b", ""]);
        assert_eq!(split::<3>("a\t\tb").as_slice(), ["a", "b", ""]);
        assert_eq!(
            split::<7>("p\tk").as_slice(),
            ["p", "k", "", "", "", "", ""]
        );
    }

    #[test]
    fn read_loop_lines_drops_terminated_tail() {
        // `while read` bodies never run for the unterminated tail,
        // and NUL bytes never survive `read`.
        assert_eq!(
            read_loop_lines("a\nb\n"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(read_loop_lines("a\nb"), vec!["a".to_string()]);
        assert!(read_loop_lines("").is_empty());
        assert!(read_loop_lines("tail-only").is_empty());
        assert_eq!(read_loop_lines("a\0b\n"), vec!["ab".to_string()]);
    }

    #[test]
    fn safe_value_matches_shell_gate() {
        assert!(safe_value(b"https://example.test/dot"));
        assert!(!safe_value(b""));
        assert!(!safe_value(b"a\tb"));
        assert!(!safe_value(b"a\nb"));
        assert!(!safe_value(b"a\rb"));
    }

    #[test]
    fn mapfile_lines_matches_mapfile_t() {
        // `mapfile -t` keeps the unterminated tail but drops the
        // phantom after a trailing newline; empty output reads zero
        // rows, never one empty row.
        assert!(mapfile_lines(b"").is_empty());
        assert_eq!(mapfile_lines(b"one\n"), vec![b"one".as_slice()]);
        assert_eq!(
            mapfile_lines(b"one\ntwo\n"),
            vec![b"one".as_slice(), b"two".as_slice()]
        );
        assert_eq!(
            mapfile_lines(b"one\ntwo"),
            vec![b"one".as_slice(), b"two".as_slice()]
        );
        assert_eq!(mapfile_lines(b"\n"), vec![b"".as_slice()]);
    }

    #[test]
    fn under_root_matches_shell_glob() {
        // `[[ $path == "$root" || $path == "$root"/* ]]` with a
        // literal root: equality or a slash-bound descent. Pattern
        // metacharacters in the root stay literal.
        assert!(under_root("a", "a"));
        assert!(under_root("a/b", "a"));
        assert!(under_root("a/b/c", "a"));
        assert!(!under_root("ab", "a"));
        assert!(!under_root("a", "a/b"));
        assert!(!under_root("a", ""));
        assert!(!under_root("x", ""));
        assert!(under_root("a*b", "a*b"));
        assert!(!under_root("axb", "a*b"));
    }
}
