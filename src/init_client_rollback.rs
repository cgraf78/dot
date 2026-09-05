//! The init rollback family of `lib/dot/init-client.sh`: undoing a
//! published transaction before it commits.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the four contiguous functions from
//! `_dot_init_rollback_entry` through `_dot_init_rollback` in file
//! order (lines 1574-1710): one published entry rolls back
//! ([`rollback_entry`]), every reserved parent directory rolls back
//! in reverse order ([`rollback_parents`]), the whole published
//! generation plus the staged git directory and its container roll
//! back ([`rollback_published`]), and the top-level command
//! validates the journal, rolls back, restores backups, and drops
//! the transaction ([`rollback`]).
//!
//! Lane map, so the integrator can stack without overlap: the
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family on
//! `rust-port-slice-41` (`init_client_identity`), the git-generation
//! binding on `rust-port-slice-43` (`init_client_generation`), the
//! per-entry staging family on `rust-port-slice-46`
//! (`init_client_entry`), the candidate planning family on
//! `rust-port-slice-48` (`init_client_candidate`), the transaction
//! record journal on `rust-port-slice-51` (`init_client_records`)
//! and `rust-port-slice-54` (`init_client_record`), the
//! deletion-parking family on `rust-port-slice-55`
//! (`init_client_delete`), and the plan review plus conflict
//! safekeeping on `rust-port-slice-62` (`init_client_plan`). The
//! file-generic `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state): its three
//! messages surface as [`Error::Usage`] text, the way earlier slices
//! absorb engine diagnostics. The publish, resume, status, and
//! command-dispatch families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals and the worktree root from `HOME`. Library
//! code must not read process environment behind the engine, so the
//! journal-derived identity crosses here as [`RecordCtx`] (built by
//! the `read_record` closure, which runs the real
//! `_dot_init_read_record`) and the worktree root crosses as
//! `home`. Every out-of-scope helper the four functions call
//! crosses as a boxed closure in [`RollbackDeps`], one per shell
//! call site with the verifier arguments already bound the way the
//! delete lane binds its verifier — so this module ports exactly
//! the four functions' own control flow and byte handling, nothing
//! above line 1574 and nothing below line 1710. `REPLY`-carried
//! outputs surface as return values. Probes run in engine mode
//! (`set -euo pipefail` around a bare call, the `--rollback` call
//! shape), where the first failing statement stops the function —
//! exactly the first-`Err` return below.
//!
//! Byte-fidelity boundary: every `$HOME/$path` join concatenates
//! bytes like the shell, preserving a doubled separator on
//! trailing-slash inputs instead of normalizing it away (the delete
//! lane precedent). `${var#prefix}` keeps the whole string on a
//! miss, also like the shell. Journal and intent text parses with
//! `IFS=$'\t' read -r` semantics — leading tabs stripped, tab runs
//! collapsing between fields, the last variable keeping the raw
//! remainder, missing variables reading empty — not a plain tab
//! split (the plan lane precedent). `LC_ALL=C` is pinned around
//! every child process so git output reads English on both engines.
//! Scalar journal words cross the UTF-8 boundary with
//! `from_utf8_lossy` (the candidate lane precedent); paths never
//! do — they stay byte-exact `PathBuf`s. `git hash-object --stdin`
//! is a direct `git` call, not a shell helper, so it runs natively
//! here like the delete lane's twin.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};

/// Run identity for the rollback chapter: the journal fields the
/// four functions read. Built by the [`ReadRecord`] closure, which
/// runs the real `_dot_init_read_record` and echoes these six
/// globals back tab-joined (all are tab/newline-free by the
/// journal's `safe_value` gate, so the join is exact).
pub struct RecordCtx {
    /// `DOT_INIT_PHASE`: gates the top-level rollback.
    pub phase: String,
    /// `DOT_INIT_BACKUP`: restore root, or `-` when there is none.
    pub backup: PathBuf,
    /// `DOT_INIT_NONCE`: names parks and the git-stage container.
    pub nonce: String,
    /// `DOT_INIT_GIT_DIR`: the staged git directory.
    pub git_dir: PathBuf,
    /// `DOT_INIT_COMMIT`: the generation the verifiers expect.
    pub commit: String,
    /// `DOT_INIT_GIT_DEV:INO`: the recorded git identity text.
    pub git_identity: String,
}

/// `_dot_init_entry_intent` by position (`intent mode oid path`),
/// returning the raw `$REPLY` bytes this module parses into the
/// six intent fields.
pub type EntryIntent<'a> = dyn Fn(&Path, &str, &str, &Path) -> Result<Vec<u8>> + 'a;

/// `_dot_init_delete_park_path` by position (`target kind key`),
/// returning the `$REPLY` park path. The key crosses as raw bytes
/// so entry paths, parent spellings, and git directories all keep
/// their exact octets.
pub type DeleteParkPath<'a> = dyn Fn(&Path, &str, &[u8]) -> Result<PathBuf> + 'a;

/// `_dot_init_delete_parked_generation` plus its leaf verifier by
/// position (`target park identity git_dir commit mode oid`).
pub type RemoveParkedLeaf<'a> =
    dyn Fn(&Path, &Path, &str, &Path, &str, &str, &str) -> Result<()> + 'a;

/// `_dot_init_entry_stage_valid` by position (`stage identity?`),
/// with `None` for the shell's absent second argument.
pub type EntryStageValid<'a> = dyn Fn(&Path, Option<&str>) -> Result<()> + 'a;

/// `_dot_init_stage_claim_matches` by position
/// (`stage kind path`).
pub type StageClaimMatches<'a> = dyn Fn(&Path, &str, &Path) -> Result<()> + 'a;

/// `_dot_init_entry_stage_only_next` by position (`stage`).
pub type EntryStageOnlyNext<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// `_dot_init_discard_staged_next` by position (`stage`).
pub type DiscardStagedNext<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// `_dot_path_identity` by position (`path`), with a failed stat
/// reading empty like the shell's `$(... || true)`.
pub type PathIdentity<'a> = dyn Fn(&Path) -> String + 'a;

/// `_dot_init_candidate_matches_git` by position
/// (`git_dir commit mode oid relative`).
pub type CandidateMatchesGit<'a> = dyn Fn(&Path, &str, &str, &str, &str) -> Result<()> + 'a;

/// `_dot_init_stage_claim_remove` by position
/// (`stage kind path`).
pub type StageClaimRemove<'a> = dyn Fn(&Path, &str, &Path) -> Result<()> + 'a;

/// `_dot_init_parent_record` by position
/// (`transaction parent`), returning the raw `$REPLY` bytes this
/// module parses into the five record fields.
pub type ParentRecord<'a> = dyn Fn(&Path, &Path) -> Result<Vec<u8>> + 'a;

/// `_dot_init_safe_relative_path` by position (`parent`).
pub type SafeRelativePath<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// `_dot_init_delete_parked_generation` plus its parent verifier by
/// position (`target park identity mode`).
pub type RemoveParkedParent<'a> = dyn Fn(&Path, &Path, &str, &str) -> Result<()> + 'a;

/// `_dot_init_private_directory_matches` by position
/// (`stage identity? mode?`), with `None` for absent arguments.
pub type PrivateDirectoryMatches<'a> = dyn Fn(&Path, Option<&str>, Option<&str>) -> Result<()> + 'a;

/// `_dot_init_stage_claim_only` by position (`stage`).
pub type StageClaimOnly<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// `_dot_init_private_empty_directory_matches` by position
/// (`stage identity? mode?`), with `None` for absent arguments.
pub type PrivateEmptyDirectoryMatches<'a> =
    dyn Fn(&Path, Option<&str>, Option<&str>) -> Result<()> + 'a;

/// `_dot_init_delete_parked_generation` plus its git verifier by
/// position (`git_dir park identity`).
pub type RemoveParkedTree<'a> = dyn Fn(&Path, &Path, &str) -> Result<()> + 'a;

/// `_dot_init_transaction_dir`, returning the `$REPLY` directory.
pub type TransactionDir<'a> = dyn Fn() -> Result<PathBuf> + 'a;

/// `_dot_init_read_record` by position (`record`), returning the
/// parsed run identity.
pub type ReadRecord<'a> = dyn Fn(&Path) -> Result<RecordCtx> + 'a;

/// `_dot_init_restore_backups` by position (`backup`).
pub type RestoreBackups<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// The twenty out-of-scope call sites the rollback chapter needs,
/// one boxed closure each. Boxed (not borrowed) so rows can build
/// the whole set in a helper and move it into the call.
pub struct RollbackDeps<'a> {
    /// Runs `_dot_init_entry_intent`.
    pub entry_intent: Box<EntryIntent<'a>>,
    /// Runs `_dot_init_delete_park_path`.
    pub delete_park_path: Box<DeleteParkPath<'a>>,
    /// Runs `_dot_init_delete_parked_generation` with
    /// `_dot_init_leaf_delete_matches`.
    pub remove_parked_leaf: Box<RemoveParkedLeaf<'a>>,
    /// Runs `_dot_init_entry_stage_valid`.
    pub entry_stage_valid: Box<EntryStageValid<'a>>,
    /// Runs `_dot_init_stage_claim_matches`.
    pub stage_claim_matches: Box<StageClaimMatches<'a>>,
    /// Runs `_dot_init_entry_stage_only_next`.
    pub entry_stage_only_next: Box<EntryStageOnlyNext<'a>>,
    /// Runs `_dot_init_discard_staged_next`.
    pub discard_staged_next: Box<DiscardStagedNext<'a>>,
    /// Runs `_dot_path_identity`.
    pub path_identity: Box<PathIdentity<'a>>,
    /// Runs `_dot_init_candidate_matches_git`.
    pub candidate_matches_git: Box<CandidateMatchesGit<'a>>,
    /// Runs `_dot_init_stage_claim_remove`.
    pub stage_claim_remove: Box<StageClaimRemove<'a>>,
    /// Runs `_dot_init_parent_record`.
    pub parent_record: Box<ParentRecord<'a>>,
    /// Runs `_dot_init_safe_relative_path`.
    pub safe_relative_path: Box<SafeRelativePath<'a>>,
    /// Runs `_dot_init_delete_parked_generation` with
    /// `_dot_init_parent_delete_matches`.
    pub remove_parked_parent: Box<RemoveParkedParent<'a>>,
    /// Runs `_dot_init_private_directory_matches`.
    pub private_directory_matches: Box<PrivateDirectoryMatches<'a>>,
    /// Runs `_dot_init_stage_claim_only`.
    pub stage_claim_only: Box<StageClaimOnly<'a>>,
    /// Runs `_dot_init_private_empty_directory_matches`.
    pub private_empty_directory_matches: Box<PrivateEmptyDirectoryMatches<'a>>,
    /// Runs `_dot_init_delete_parked_generation` with
    /// `_dot_init_git_delete_matches`.
    pub remove_parked_tree: Box<RemoveParkedTree<'a>>,
    /// Runs `_dot_init_transaction_dir`.
    pub transaction_dir: Box<TransactionDir<'a>>,
    /// Runs `_dot_init_read_record`.
    pub read_record: Box<ReadRecord<'a>>,
    /// Runs `_dot_init_restore_backups`.
    pub restore_backups: Box<RestoreBackups<'a>>,
}

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// A regular file reached through any non-symlink chain: the
/// shell's bare `[[ -f $path ]]`, which follows symlinks. Used only
/// for the tree and container gates, where the shell tests exactly
/// this.
fn is_file_following(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Raw bytes of a path, so `$HOME/` joins and prefix strips behave
/// like shell string operations even when `home` has a trailing
/// slash (the doubled separator is preserved, never normalized
/// away).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// `$HOME/$path` by byte concatenation, like the shell's
/// `target=$HOME/$4` (the delete lane precedent).
fn join_home(home: &Path, rel: &[u8]) -> PathBuf {
    let mut joined = path_bytes(home).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(rel);
    PathBuf::from(OsString::from_vec(joined))
}

/// Append one `/`-separated leaf, like the shell's
/// `"$base/$leaf"`. Byte concatenation, so a `base` with a
/// trailing slash keeps its doubled separator exactly like the
/// shell's expansion does (the plan lane precedent).
fn join2(base: &Path, leaf: &str) -> PathBuf {
    let mut joined = path_bytes(base).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(leaf.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// The shell's `${path#"$HOME"/}`: strip the prefix only when the
/// bytes match, otherwise keep the whole string — the expansion
/// never fails, it just stops matching.
fn strip_home_prefix<'a>(home: &Path, candidate: &'a [u8]) -> &'a [u8] {
    let home = path_bytes(home);
    if candidate.len() > home.len()
        && candidate[home.len()] == b'/'
        && candidate[..home.len()] == *home
    {
        &candidate[home.len() + 1..]
    } else {
        candidate
    }
}

/// Mirror of `IFS=$'\t' read -r` over bytes: leading tabs are
/// stripped, tab runs collapse between fields, the last slot keeps
/// the raw remainder with its tabs intact, and missing slots read
/// empty (the plan lane precedent, kept byte-exact so paths never
/// cross the UTF-8 boundary here).
fn read_fields(line: &[u8], out: &mut [Vec<u8>]) {
    let Some((last, head)) = out.split_last_mut() else {
        return;
    };
    let mut rest = line;
    while rest.first() == Some(&b'\t') {
        rest = &rest[1..];
    }
    for slot in head {
        match rest.iter().position(|byte| *byte == b'\t') {
            Some(position) => {
                *slot = rest[..position].to_vec();
                rest = &rest[position..];
                while rest.first() == Some(&b'\t') {
                    rest = &rest[1..];
                }
            }
            None => {
                *slot = rest.to_vec();
                rest = b"";
            }
        }
    }
    *last = rest.to_vec();
}

/// Frame `$(<file)` plus a herestring `read` the way the chapter's
/// intent loops see them: NUL bytes drop (bash cannot hold them),
/// every trailing newline chomps (command substitution), and only
/// the first line parses (a single `read` call).
fn first_line_fields(content: &[u8], slots: usize) -> Vec<Vec<u8>> {
    let owned: Vec<u8> = content
        .iter()
        .copied()
        .filter(|byte| *byte != b'\0')
        .collect();
    let mut text = owned.as_slice();
    while text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    let line = match text.iter().position(|byte| *byte == b'\n') {
        Some(position) => &text[..position],
        None => text,
    };
    let mut out = vec![Vec::new(); slots];
    read_fields(line, &mut out);
    out
}

/// Strip the NUL bytes a shell `read` silently drops.
fn strip_nuls(line: &[u8]) -> Vec<u8> {
    line.iter().copied().filter(|byte| *byte != b'\0').collect()
}

/// Split raw file bytes the way `LC_ALL=C sort` ingests them:
/// lines divide on `\n` and an unterminated tail still counts
/// (sort terminates it before printing), while a trailing newline
/// adds no phantom line. NUL bytes stay: `sort` compares them as
/// data, and only the later `read` strips them.
fn sort_input_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// Third tab field of a tree row, or empty when the row holds fewer
/// than three fields: the key `LC_ALL=C sort -r -t $'\t' -k3,3`
/// compares.
fn tree_key(line: &[u8]) -> &[u8] {
    let mut fields = line.split(|byte| *byte == b'\t');
    fields.next();
    fields.next();
    fields.next().unwrap_or(b"")
}

/// Descending byte order by tree key with a descending full-line
/// tiebreak: the shell's `LC_ALL=C sort -r -t $'\t' -k3,3`, where
/// `-r` reverses the key comparison and the last-resort full-line
/// comparison alike (probed against GNU sort, including short rows
/// and unterminated tails).
fn sort_tree_rows_desc(content: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = sort_input_lines(content);
    lines.sort_by(|a, b| tree_key(b).cmp(tree_key(a)).then(b.cmp(a)));
    lines.iter().map(|line| line.to_vec()).collect()
}

/// Strip every trailing newline, exactly like command substitution.
fn chomp_newlines(bytes: &[u8]) -> &[u8] {
    let mut text = bytes;
    while text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    text
}

/// `git hash-object --stdin` over raw bytes (content hashing is
/// store-independent, so no `--git-dir`): the shell's
/// `printf '%s' ... | git hash-object --stdin` in
/// `_dot_init_rollback_published`, run natively like the delete
/// lane's twin. `LC_ALL=C` is pinned, never `envs`.
fn hash_stdin_bytes(payload: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| Error::Io {
            context: "spawn git hash-object",
            source,
        })?;
    child
        .stdin
        .as_mut()
        .ok_or(Error::Usage {
            message: "git hash-object has no stdin",
        })?
        .write_all(payload)
        .map_err(|source| Error::Io {
            context: "feed git hash-object",
            source,
        })?;
    let output = child.wait_with_output().map_err(|source| Error::Io {
        context: "reap git hash-object",
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "git hash-object --stdin".to_string(),
            status: Some(output.status.to_string()),
        });
    }
    Ok(String::from_utf8_lossy(chomp_newlines(&output.stdout)).into_owned())
}

/// One `grep -Fqx` line probe: some line of the file is byte-equal
/// to the pattern. Newline framing follows the shell's line reader
/// (a missing trailing newline still yields its final line);
/// binary-only mismatches cannot occur because the pattern never
/// holds a NUL while any NUL-bearing line differs from it.
fn file_has_exact_line(content: &[u8], pattern: &[u8]) -> bool {
    sort_input_lines(content).contains(&pattern)
}

/// Lossy scalar for word compares and diagnostics: intent words are
/// engine vocabulary the journal gates to tab/newline-free text
/// (the candidate lane precedent).
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `_dot_init_rollback_entry`: roll back one published entry
/// (`intent mode oid path`). A live target or park only rolls back
/// in the `prepared` phase; then the stage rolls back by phase —
/// `pending` demands an empty claim-only stage, `staged` discards
/// its next generation, and `prepared` revalidates a present next
/// generation against the recorded identity and the git store
/// before removing it. Anything else refuses, like the shell's
/// trailing `*)` arm.
pub fn rollback_entry(
    deps: &RollbackDeps<'_>,
    home: &Path,
    record: &RecordCtx,
    intent: &Path,
    mode: &str,
    oid: &str,
    path: &Path,
) -> Result<()> {
    let reply = (deps.entry_intent)(intent, mode, oid, path)?;
    let fields = first_line_fields(&reply, 6);
    let phase = lossy(&fields[0]);
    let stage = join_home(home, &fields[1]);
    let identity = format!("{}:{}", lossy(&fields[2]), lossy(&fields[3]));
    let next_identity = format!("{}:{}", lossy(&fields[4]), lossy(&fields[5]));
    let target = join_home(home, path_bytes(path));
    let park = (deps.delete_park_path)(&target, "leaf", path_bytes(path))?;
    if exists_lexical(&target) || exists_lexical(&park) {
        if phase != "prepared" {
            return Err(Error::Usage {
                message: "rollback entry target is not a prepared generation",
            });
        }
        (deps.remove_parked_leaf)(
            &target,
            &park,
            &next_identity,
            &record.git_dir,
            &record.commit,
            mode,
            oid,
        )?;
    }
    if exists_lexical(&stage) {
        match phase.as_str() {
            "pending" => {
                (deps.entry_stage_valid)(&stage, None)?;
                (deps.stage_claim_matches)(&stage, "entry", path)?;
                (deps.entry_stage_only_next)(&stage)?;
                if exists_lexical(&join2(&stage, "next")) {
                    return Err(Error::Usage {
                        message: "rollback entry stage holds a next generation",
                    });
                }
            }
            "staged" => {
                (deps.entry_stage_valid)(&stage, Some(&identity))?;
                (deps.stage_claim_matches)(&stage, "entry", path)?;
                (deps.discard_staged_next)(&stage)?;
            }
            "prepared" => {
                (deps.entry_stage_valid)(&stage, Some(&identity))?;
                (deps.stage_claim_matches)(&stage, "entry", path)?;
                (deps.entry_stage_only_next)(&stage)?;
                let next = join2(&stage, "next");
                if exists_lexical(&next) {
                    if (deps.path_identity)(&next) != next_identity {
                        return Err(Error::Usage {
                            message: "rollback entry next generation changed",
                        });
                    }
                    let mut rel = strip_home_prefix(home, path_bytes(&stage)).to_vec();
                    rel.extend_from_slice(b"/next");
                    (deps.candidate_matches_git)(
                        &record.git_dir,
                        &record.commit,
                        mode,
                        oid,
                        &lossy(&rel),
                    )?;
                    if let Err(source) = std::fs::remove_file(&next) {
                        if source.kind() != std::io::ErrorKind::NotFound {
                            return Err(Error::Io {
                                context: "remove staged next",
                                source,
                            });
                        }
                    }
                }
            }
            _ => {
                return Err(Error::Usage {
                    message: "rollback entry phase refused",
                });
            }
        }
        (deps.stage_claim_remove)(&stage, "entry", path)?;
        std::fs::remove_dir(&stage).map_err(|source| Error::Io {
            context: "remove entry stage",
            source,
        })?;
    }
    Ok(())
}

/// Basename of a transaction file, for the temporary-intent skip:
/// the shell's `${file##*/}`.
fn file_basename(file: &[u8]) -> &[u8] {
    match file.iter().rposition(|byte| *byte == b'/') {
        Some(position) => &file[position + 1..],
        None => file,
    }
}

/// The shell's `[[ ${file##*/} != parent-intent.*.tmp.* ]]` skip:
/// a `parent-intent.` name whose remainder holds a `.tmp.` field.
fn is_temporary_intent(base: &[u8]) -> bool {
    base.strip_prefix(b"parent-intent.")
        .is_some_and(|rest| rest.windows(5).any(|window| window == b".tmp."))
}

/// `_dot_init_rollback_parents`: roll back every reserved parent
/// directory (`transaction`). Intent journals collect in glob order
/// (an unreadable transaction reads empty, like the shell's vacant
/// `nullglob` expansion), then process in reverse byte order — the
/// shell's `LC_ALL=C sort -r` — so nested parents release
/// inside-out. A live target or park only rolls back against a
/// `prepared` record with no stage; then the stage rolls back by
/// phase, and only an absent target lets a stage go.
pub fn rollback_parents(deps: &RollbackDeps<'_>, home: &Path, transaction: &Path) -> Result<()> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(transaction) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|file| file_basename(path_bytes(file)).starts_with(b"parent-intent."))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    let mut records: Vec<Vec<u8>> = Vec::new();
    for file in &files {
        if is_temporary_intent(file_basename(path_bytes(file))) {
            continue;
        }
        let content = std::fs::read(file).map_err(|source| Error::Io {
            context: "read parent intent",
            source,
        })?;
        let fields = first_line_fields(&content, 3);
        let phase = lossy(&fields[0]);
        if phase != "pending" && phase != "prepared" {
            return Err(Error::Usage {
                message: "rollback parent intent phase refused",
            });
        }
        let parent = PathBuf::from(OsString::from_vec(fields[1].clone()));
        (deps.safe_relative_path)(&parent)?;
        let mut record = path_bytes(&parent).to_vec();
        record.push(b'\t');
        record.extend_from_slice(path_bytes(file));
        records.push(record);
    }
    records.sort_by(|a, b| b.cmp(a));
    for row in &records {
        let mut slots = [Vec::new(), Vec::new()];
        read_fields(row, &mut slots);
        if slots[0].is_empty() || slots[1].is_empty() {
            continue;
        }
        let parent = PathBuf::from(OsString::from_vec(slots[0].clone()));
        let _file = PathBuf::from(OsString::from_vec(slots[1].clone()));
        let reply = (deps.parent_record)(transaction, &parent)?;
        let fields = first_line_fields(&reply, 5);
        let phase = lossy(&fields[0]);
        let stage = join_home(home, &fields[1]);
        let identity = format!("{}:{}", lossy(&fields[2]), lossy(&fields[3]));
        let mode = lossy(&fields[4]);
        let target = join_home(home, path_bytes(&parent));
        let park = (deps.delete_park_path)(&target, "parent", path_bytes(&parent))?;
        if exists_lexical(&target) || exists_lexical(&park) {
            if !(phase == "prepared" && !exists_lexical(&stage)) {
                return Err(Error::Usage {
                    message: "rollback parent target is not a prepared generation",
                });
            }
            (deps.remove_parked_parent)(&target, &park, &identity, &mode)?;
        }
        if exists_lexical(&stage) {
            if exists_lexical(&target) {
                return Err(Error::Usage {
                    message: "rollback parent target won over its stage",
                });
            }
            if phase == "prepared" {
                (deps.private_directory_matches)(&stage, Some(&identity), Some(&mode))?;
                if exists_lexical(&join2(&stage, ".dot-init-stage-claim-v1")) {
                    (deps.stage_claim_only)(&stage)?;
                    (deps.stage_claim_remove)(&stage, "parent", &parent)?;
                }
                (deps.private_empty_directory_matches)(&stage, Some(&identity), Some(&mode))?;
            } else {
                (deps.private_directory_matches)(&stage, None, None)?;
                (deps.stage_claim_only)(&stage)?;
                (deps.stage_claim_remove)(&stage, "parent", &parent)?;
                (deps.private_empty_directory_matches)(&stage, None, None)?;
            }
            std::fs::remove_dir(&stage).map_err(|source| Error::Io {
                context: "remove parent stage",
                source,
            })?;
        }
    }
    Ok(())
}

/// `_dot_init_rollback_published`: roll back the published
/// generation (`transaction`). Tree rows walk in reverse path order
/// — the shell's `LC_ALL=C sort -r -t $'\t' -k3,3` — and each row
/// with a live intent journal rolls back through [`rollback_entry`]
/// before the parents roll back through [`rollback_parents`]. An
/// unreadable tree past the file gate sorts to zero rows, like the
/// shell's failed `sort` feeding the loop empty input. Then the
/// staged git directory rolls back through its parked-generation
/// remover, and the git-stage container goes only with this run's
/// nonce line on it.
pub fn rollback_published(
    deps: &RollbackDeps<'_>,
    home: &Path,
    record: &RecordCtx,
    transaction: &Path,
) -> Result<()> {
    let tree = join2(transaction, "tree.tsv");
    if is_file_following(&tree) {
        if let Ok(content) = std::fs::read(&tree) {
            for row in sort_tree_rows_desc(&content) {
                let clean = strip_nuls(&row);
                let mut slots = [Vec::new(), Vec::new(), Vec::new()];
                read_fields(&clean, &mut slots);
                let mode = lossy(&slots[0]);
                let oid = lossy(&slots[1]);
                let path = PathBuf::from(OsString::from_vec(slots[2].clone()));
                let hash = hash_stdin_bytes(path_bytes(&path))?;
                let intent = join2(transaction, &format!("publish-intent.{hash}"));
                if !is_real_file(&intent) {
                    continue;
                }
                rollback_entry(deps, home, record, &intent, &mode, &oid, &path)?;
            }
        }
    }
    rollback_parents(deps, home, transaction)?;
    let park = (deps.delete_park_path)(&record.git_dir, "git", path_bytes(&record.git_dir))?;
    if exists_lexical(&record.git_dir) || exists_lexical(&park) {
        (deps.remove_parked_tree)(&record.git_dir, &park, &record.git_identity)?;
    }
    let container = join2(&record.backup, "git-stage");
    if exists_lexical(&container) {
        let identity = join2(&container, "identity");
        if !(is_real_dir(&container) && is_file_following(&identity)) {
            return Err(Error::Usage {
                message: "rollback git-stage container changed",
            });
        }
        let content = std::fs::read(&identity).map_err(|source| Error::Io {
            context: "read git-stage identity",
            source,
        })?;
        let mut nonce = b"nonce=".to_vec();
        nonce.extend_from_slice(record.nonce.as_bytes());
        if !file_has_exact_line(&content, &nonce) {
            return Err(Error::Usage {
                message: "rollback git-stage container changed",
            });
        }
        std::fs::remove_dir_all(&container).map_err(|source| Error::Io {
            context: "remove git-stage container",
            source,
        })?;
    }
    Ok(())
}

/// `_dot_init_rollback`: roll back the recoverable transaction.
/// Anything but a readable journal refuses with the shell's
/// diagnostic; a committed phase (checkout, converging, complete)
/// refuses too. A failing published rollback maps to the shell's
/// diagnostic and stops before the backups move; otherwise the
/// backups restore and the transaction directory goes.
pub fn rollback(deps: &RollbackDeps<'_>, home: &Path) -> Result<()> {
    let transaction = (deps.transaction_dir)()?;
    let record = match (deps.read_record)(&join2(&transaction, "record")) {
        Ok(record) => record,
        Err(_) => {
            return Err(Error::Usage {
                message: "no recoverable transaction",
            });
        }
    };
    if record.phase == "checkout" || record.phase == "converging" || record.phase == "complete" {
        return Err(Error::Usage {
            message: "checkout is committed; rerun the original init command to resume",
        });
    }
    if rollback_published(deps, home, &record, &transaction).is_err() {
        return Err(Error::Usage {
            message: "transaction-owned paths changed; refusing rollback",
        });
    }
    (deps.restore_backups)(&record.backup)?;
    std::fs::remove_dir_all(&transaction).map_err(|source| Error::Io {
        context: "remove transaction",
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(line: &[u8], slots: usize) -> Vec<Vec<u8>> {
        let mut out = vec![Vec::new(); slots];
        read_fields(line, &mut out);
        out
    }

    #[test]
    fn tab_read_collapses_runs_and_keeps_remainder() {
        assert_eq!(
            fields(b"a\tb\tc", 3),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(fields(b"\ta\t\tb", 2), vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(
            fields(b"a\tb\tc\td", 3),
            vec![b"a".to_vec(), b"b".to_vec(), b"c\td".to_vec()]
        );
        assert_eq!(fields(b"a", 3), vec![b"a".to_vec(), Vec::new(), Vec::new()]);
        assert_eq!(fields(b"", 2), vec![Vec::new(), Vec::new()]);
    }

    #[test]
    fn first_line_chomps_and_drops_nuls() {
        assert_eq!(
            first_line_fields(b"a\tb\n\n", 2),
            vec![Vec::from(b"a"), Vec::from(b"b")]
        );
        assert_eq!(
            first_line_fields(b"a\0\tb\nsecond\n", 2),
            vec![Vec::from(b"a"), Vec::from(b"b")]
        );
        assert_eq!(first_line_fields(b"", 1), vec![Vec::new()]);
    }

    #[test]
    fn tree_sort_matches_gnu_key_reverse() {
        let content = b"k1\ta\tm\nk2\tb\tm\nk3\tc\nk4\nk5\td\tz\tW\nk6\te\tz\n";
        let rows = sort_tree_rows_desc(content);
        let texts: Vec<String> = rows.iter().map(|row| lossy(row)).collect();
        assert_eq!(
            texts,
            vec![
                "k6\te\tz",
                "k5\td\tz\tW",
                "k2\tb\tm",
                "k1\ta\tm",
                "k4",
                "k3\tc",
            ]
        );
        let bare = b"k1\ta\tm\nk2\tb\tm\nk3\tc\nk4\nk5\td\tz\tW\nk6\te\tz";
        assert_eq!(sort_tree_rows_desc(bare), rows);
    }

    #[test]
    fn home_prefix_strip_keeps_misses() {
        let home = Path::new("/h");
        assert_eq!(strip_home_prefix(home, b"/h/a/next"), b"a/next");
        assert_eq!(strip_home_prefix(home, b"/other/a"), b"/other/a");
        assert_eq!(strip_home_prefix(home, b"/h"), b"/h");
        let slash_home = Path::new("/h/");
        assert_eq!(strip_home_prefix(slash_home, b"/h//a"), b"a");
    }

    #[test]
    fn exact_line_matches_terminated_tails() {
        assert!(file_has_exact_line(b"a\nnonce=x\n", b"nonce=x"));
        assert!(file_has_exact_line(b"a\nnonce=x", b"nonce=x"));
        assert!(!file_has_exact_line(b"a\nnonce=xy\n", b"nonce=x"));
        assert!(!file_has_exact_line(b"a\nnonce =x\n", b"nonce=x"));
        assert!(!file_has_exact_line(b"", b"nonce=x"));
    }

    #[test]
    fn temporary_intent_names_skip() {
        assert!(is_temporary_intent(b"parent-intent.ab.tmp.9"));
        assert!(is_temporary_intent(b"parent-intent..tmp."));
        assert!(!is_temporary_intent(b"parent-intent.ab"));
        assert!(!is_temporary_intent(b"parent-intent.tmp9"));
        assert!(!is_temporary_intent(b"other.ab.tmp.9"));
    }

    #[test]
    fn home_join_preserves_doubled_separator() {
        let home = Path::new("/h/");
        assert_eq!(join_home(home, b"a"), PathBuf::from("/h//a"));
        let plain = Path::new("/h");
        assert_eq!(join_home(plain, b"a"), PathBuf::from("/h/a"));
    }
}
