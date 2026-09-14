//! The parent-directory publisher of `lib/dot/init-client.sh`:
//! ensuring every ancestor of an entry exists before publication.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only `_dot_init_parent_directories` (lines
//! 1062-1130): walk the entry's ancestors top-down, reserve each
//! missing level with a pending intent, prepare a claimed stage
//! directory bound to its device, inode, and mode, then rename the
//! stage into place. Levels that already hold real directories are
//! kept; anything else refuses.
//!
//! Lane map, so the integrator can stack without overlap: the
//! parent record reader lives on `rust-port-slice-54`
//! (`init_client_record`), the private-line publisher and the four
//! stage-claim helpers on `rust-port-slice-46` (`init_client_entry`),
//! and the two private-directory gates on `rust-port-slice-55`
//! (`init_client_delete`) — all unmerged here, so those eight call
//! sites cross as closures in [`ParentHooks`], one per shell call
//! site with the verifier arguments bound positionally, the way the
//! rollback lane binds its verifiers. The `git hash-object --stdin`
//! digest, the `mkdir`/`chmod` provisioning, the identity and mode
//! reads, and the exclusive rename are engine mechanics, not ported
//! functions: they run natively here through [`crate::temp`], like
//! the sibling lanes' twins. Nothing above line 1062 and nothing
//! below line 1130 is ported here.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_NONCE` global and the worktree root from `HOME`.
//! Library code must not read process environment behind the
//! engine, so the nonce crosses as a parameter and the worktree
//! root crosses as `home`. `REPLY`-carried outputs surface as
//! return values. Every shell refusal in this function is a bare
//! `return 1` with no diagnostic of its own, so every refusal here
//! surfaces as [`Error::Usage`]; filesystem failures keep their
//! [`Error::Io`] context and git failures stay [`Error::Command`].
//!
//! Byte-fidelity boundary: every `$HOME/$path` join and the
//! `$transaction/parent-intent.$hash` name concatenate bytes like
//! the shell, preserving a doubled separator on trailing-slash
//! inputs instead of normalizing it away (the plan lane precedent).
//! `${var#prefix}` and `${var%/*}` keep the whole string on a miss,
//! also like the shell. The ancestor split mirrors the shell's
//! `IFS=/ read -r -a` framing — first line only, NUL bytes dropped,
//! exactly one trailing empty dropped (all probed against bash) —
//! and intent replies parse with `IFS=$'\t' read -r` semantics via
//! `read_fields`. Scalar record words cross the UTF-8 boundary
//! with `from_utf8_lossy` (the candidate lane precedent); paths
//! never do — they stay byte-exact. `LC_ALL=C` is pinned around the
//! hash child so diagnostics read English on both engines.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::temp;

/// Claim kind this chapter stamps and verifies: the shell's literal
/// `parent` argument on every claim call below.
const STAGE_CLAIM_KIND: &str = "parent";

/// Claim-marker leaf inside a stage directory: the shell's
/// `.dot-init-stage-claim-v1` spelling.
const STAGE_CLAIM_NAME: &str = ".dot-init-stage-claim-v1";

/// `_dot_init_parent_record` by position (`transaction parent`),
/// returning the raw `$REPLY` bytes this module parses into its
/// five record fields.
pub type ParentRecord<'a> = dyn Fn(&Path, &[u8]) -> Result<Vec<u8>> + 'a;

/// `_dot_init_write_private_line` by position
/// (`file line replace`), with `replace` set for the prepared
/// upgrade and clear for the pending reservation.
pub type WritePrivateLine<'a> = dyn Fn(&Path, &[u8], bool) -> Result<()> + 'a;

/// `_dot_init_stage_claim_write` by position
/// (`stage kind path`).
pub type StageClaimWrite<'a> = dyn Fn(&Path, &str, &[u8]) -> Result<()> + 'a;

/// `_dot_init_stage_claim_matches` by position
/// (`stage kind path`).
pub type StageClaimMatches<'a> = dyn Fn(&Path, &str, &[u8]) -> Result<()> + 'a;

/// `_dot_init_stage_claim_only` by position (`stage`).
pub type StageClaimOnly<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// `_dot_init_stage_claim_remove` by position
/// (`stage kind path`).
pub type StageClaimRemove<'a> = dyn Fn(&Path, &str, &[u8]) -> Result<()> + 'a;

/// `_dot_init_private_directory_matches` by position
/// (`path identity? mode?`), with `None` for the shell's absent
/// arguments.
pub type PrivateDirectoryMatches<'a> = dyn Fn(&Path, Option<&str>, Option<&str>) -> Result<()> + 'a;

/// `_dot_init_private_empty_directory_matches` by position
/// (`path identity? mode?`), with `None` for absent arguments.
pub type PrivateEmptyDirectoryMatches<'a> =
    dyn Fn(&Path, Option<&str>, Option<&str>) -> Result<()> + 'a;

/// The eight out-of-scope call sites the parent publisher needs,
/// one boxed closure each. Boxed (not borrowed) so rows can build
/// the whole set in a helper and move it into the call.
pub struct ParentHooks<'a> {
    /// Runs `_dot_init_parent_record`.
    pub parent_record: Box<ParentRecord<'a>>,
    /// Runs `_dot_init_write_private_line`.
    pub write_private_line: Box<WritePrivateLine<'a>>,
    /// Runs `_dot_init_stage_claim_write`.
    pub stage_claim_write: Box<StageClaimWrite<'a>>,
    /// Runs `_dot_init_stage_claim_matches`.
    pub stage_claim_matches: Box<StageClaimMatches<'a>>,
    /// Runs `_dot_init_stage_claim_only`.
    pub stage_claim_only: Box<StageClaimOnly<'a>>,
    /// Runs `_dot_init_stage_claim_remove`.
    pub stage_claim_remove: Box<StageClaimRemove<'a>>,
    /// Runs `_dot_init_private_directory_matches`.
    pub private_directory_matches: Box<PrivateDirectoryMatches<'a>>,
    /// Runs `_dot_init_private_empty_directory_matches`.
    pub private_empty_directory_matches: Box<PrivateEmptyDirectoryMatches<'a>>,
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

/// Raw bytes of a path, so `$HOME/` joins and prefix strips behave
/// like shell string operations even when `home` has a trailing
/// slash (the doubled separator is preserved, never normalized
/// away).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// The shell's `${path%/*}`: up to the last slash, or the whole
/// string when there is no slash. The caller guarantees the input
/// holds a slash (only the join below feeds this helper).
fn strip_last_component(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(position) => &path[..position],
        None => path,
    }
}

/// The shell's `${stage#"$HOME"/}`: strip the worktree prefix only
/// when the bytes match, otherwise keep the whole path — the
/// expansion never fails, it just stops matching.
fn strip_home_prefix<'a>(home: &Path, stage: &'a [u8]) -> &'a [u8] {
    let mut prefix = path_bytes(home).to_vec();
    prefix.push(b'/');
    stage.strip_prefix(&prefix[..]).unwrap_or(stage)
}

/// The shell's `IFS=/ read -r -a parts <<<"$parent"`: the first
/// line only (a herestring feeds one `read`), NUL bytes dropped
/// (bash cannot hold them), split on `/` with exactly one trailing
/// empty dropped. Interior empties survive, so `a//b` still walks
/// three levels exactly like the shell.
fn split_parts(parent: &[u8]) -> Vec<Vec<u8>> {
    let line = match parent.iter().position(|byte| *byte == b'\n') {
        Some(index) => &parent[..index],
        None => parent,
    };
    let cleaned: Vec<u8> = line.iter().copied().filter(|byte| *byte != b'\0').collect();
    let mut parts: Vec<Vec<u8>> = cleaned
        .split(|byte| *byte == b'/')
        .map(|part| part.to_vec())
        .collect();
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    parts
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
/// `printf '%s' ... | git hash-object --stdin`, run natively like
/// the rollback lane's twin. `LC_ALL=C` is pinned, never `envs`.
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

/// Lossy scalar for record words: intent words are engine
/// vocabulary the record gates to tab/newline-free text (the
/// candidate lane precedent).
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `_dot_init_parent_directories`: publish every ancestor of
/// `relative` under `home`, top-down. `transaction` holds the
/// parent-intent journal, `nonce` names the derived stages, and
/// `cache` backs the exclusive renames. A top-level entry (no
/// parent) is a successful no-op, like the shell's opening gate.
///
/// Each missing level reserves its stage with a pending record,
/// prepares a claimed `0700` stage bound to its live device, inode,
/// and mode, strips the claim, and renames the stage into place.
/// A level that already holds a real directory is kept; a level
/// whose record names another run's stage, whose stage is
/// occupied, or whose occupant is no directory refuses.
pub fn parent_directories(
    hooks: &ParentHooks<'_>,
    transaction: &Path,
    relative: &[u8],
    home: &Path,
    nonce: &str,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    // parent=${relative%/*}; [[ $parent != "$relative" ]] || return 0.
    let parent = match relative.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &relative[..index],
        None => relative,
    };
    if parent == relative {
        return Ok(());
    }
    let home_raw = path_bytes(home);
    let transaction_raw = path_bytes(transaction);
    let mut parent_rel: Vec<u8> = Vec::new();
    for component in split_parts(parent) {
        // parent_rel=${parent_rel:+$parent_rel/}$component.
        if !parent_rel.is_empty() {
            parent_rel.push(b'/');
        }
        parent_rel.extend_from_slice(&component);
        // current=$HOME/$parent_rel, by byte concatenation.
        let mut current_raw = home_raw.to_vec();
        current_raw.push(b'/');
        current_raw.extend_from_slice(&parent_rel);
        let current = PathBuf::from(OsString::from_vec(current_raw));
        let hash = hash_stdin_bytes(&parent_rel)?;
        // intent=$transaction/parent-intent.$hash.
        let mut intent_raw = transaction_raw.to_vec();
        intent_raw.extend_from_slice(b"/parent-intent.");
        intent_raw.extend_from_slice(hash.as_bytes());
        let intent = PathBuf::from(OsString::from_vec(intent_raw));
        // stage=${current%/*}/.dot-init-parent.$DOT_INIT_NONCE.$hash.
        let current_joined = path_bytes(&current).to_vec();
        let stage_dir = strip_last_component(&current_joined);
        let mut stage_raw = stage_dir.to_vec();
        stage_raw.push(b'/');
        stage_raw.extend_from_slice(b".dot-init-parent.");
        stage_raw.extend_from_slice(nonce.as_bytes());
        stage_raw.push(b'.');
        stage_raw.extend_from_slice(hash.as_bytes());
        let stage = PathBuf::from(OsString::from_vec(stage_raw));
        // stage_rel=${stage#"$HOME"/}.
        let stage_rel = strip_home_prefix(home, path_bytes(&stage)).to_vec();
        let mut phase: Vec<u8>;
        let mut dev: Vec<u8>;
        let mut ino: Vec<u8>;
        let mut mode: Vec<u8>;
        if exists_lexical(&intent) {
            let reply = (hooks.parent_record)(transaction, &parent_rel)?;
            let mut fields = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
            read_fields(&reply, &mut fields);
            let [record_phase, record, record_dev, record_ino, record_mode] = fields;
            if record != stage_rel {
                return Err(Error::Usage {
                    message: "parent stage moved",
                });
            }
            phase = record_phase;
            dev = record_dev;
            ino = record_ino;
            mode = record_mode;
        } else if exists_lexical(&current) {
            if !is_real_dir(&current) {
                return Err(Error::Usage {
                    message: "parent occupant is not a directory",
                });
            }
            continue;
        } else {
            if exists_lexical(&stage) {
                return Err(Error::Usage {
                    message: "parent stage is occupied",
                });
            }
            let mut line = b"pending".to_vec();
            line.push(b'\t');
            line.extend_from_slice(&parent_rel);
            line.push(b'\t');
            line.extend_from_slice(&stage_rel);
            line.extend_from_slice(b"\t-\t-\t-");
            (hooks.write_private_line)(&intent, &line, false)?;
            phase = b"pending".to_vec();
            dev = b"-".to_vec();
            ino = b"-".to_vec();
            mode = b"-".to_vec();
        }
        if phase == b"pending" {
            if exists_lexical(&current) {
                return Err(Error::Usage {
                    message: "parent appeared during publication",
                });
            }
            if !exists_lexical(&stage) {
                std::fs::create_dir(&stage).map_err(|source| Error::Io {
                    context: "create parent stage",
                    source,
                })?;
                std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).map_err(
                    |source| Error::Io {
                        context: "chmod parent stage",
                        source,
                    },
                )?;
                (hooks.stage_claim_write)(&stage, STAGE_CLAIM_KIND, &parent_rel)?;
            } else {
                // The pending record reserves this pathname, but a
                // directory that won after record publication is
                // still foreign unless it carries the exact claim
                // written by the engine that created it.
                (hooks.stage_claim_matches)(&stage, STAGE_CLAIM_KIND, &parent_rel)?;
            }
            (hooks.private_directory_matches)(&stage, None, None)?;
            (hooks.stage_claim_only)(&stage)?;
            let (live_dev, live_ino) = temp::path_identity(&stage)?;
            let live_mode = temp::file_mode(&stage)?;
            dev = live_dev.to_string().into_bytes();
            ino = live_ino.to_string().into_bytes();
            mode = format!("{live_mode:o}").into_bytes();
            let mut line = b"prepared".to_vec();
            line.push(b'\t');
            line.extend_from_slice(&parent_rel);
            line.push(b'\t');
            line.extend_from_slice(&stage_rel);
            line.push(b'\t');
            line.extend_from_slice(&dev);
            line.push(b'\t');
            line.extend_from_slice(&ino);
            line.push(b'\t');
            line.extend_from_slice(&mode);
            (hooks.write_private_line)(&intent, &line, true)?;
            phase = b"prepared".to_vec();
        }
        if phase != b"prepared" {
            return Err(Error::Usage {
                message: "parent record has an unknown phase",
            });
        }
        let identity = format!("{}:{}", lossy(&dev), lossy(&ino));
        let mode_text = lossy(&mode);
        if exists_lexical(&current) {
            if exists_lexical(&stage) {
                return Err(Error::Usage {
                    message: "parent stage survived publication",
                });
            }
            (hooks.private_directory_matches)(&current, Some(&identity), Some(&mode_text))?;
            continue;
        }
        (hooks.private_directory_matches)(&stage, Some(&identity), Some(&mode_text))?;
        let mut marker_raw = path_bytes(&stage).to_vec();
        marker_raw.push(b'/');
        marker_raw.extend_from_slice(STAGE_CLAIM_NAME.as_bytes());
        let marker = PathBuf::from(OsString::from_vec(marker_raw));
        if exists_lexical(&marker) {
            (hooks.stage_claim_only)(&stage)?;
            (hooks.stage_claim_remove)(&stage, STAGE_CLAIM_KIND, &parent_rel)?;
        }
        (hooks.private_empty_directory_matches)(&stage, Some(&identity), Some(&mode_text))?;
        temp::move_noreplace_cached(&stage, &current, cache)?;
        (hooks.private_directory_matches)(&current, Some(&identity), Some(&mode_text))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_matches_shell_read_framing() {
        // Probed against `IFS=/ read -r -a parts`: empty input
        // reads no fields, one trailing empty drops, interior
        // empties survive, newlines end the line, NULs vanish.
        assert!(split_parts(b"").is_empty());
        assert_eq!(split_parts(b"a"), vec![b"a".to_vec()]);
        assert_eq!(split_parts(b"a/"), vec![b"a".to_vec()]);
        assert_eq!(
            split_parts(b"a//b"),
            vec![b"a".to_vec(), b"".to_vec(), b"b".to_vec()],
        );
        assert_eq!(split_parts(b"/"), vec![b"".to_vec()]);
        assert_eq!(split_parts(b"/a/"), vec![b"".to_vec(), b"a".to_vec()],);
        assert_eq!(split_parts(b"a\nb/c"), vec![b"a".to_vec()]);
        assert_eq!(split_parts(b"x\0y"), vec![b"xy".to_vec()]);
    }

    #[test]
    fn fields_match_shell_read_semantics() {
        let mut out = [Vec::new(), Vec::new(), Vec::new()];
        read_fields(b"a\tb\tc", &mut out);
        assert_eq!(out, [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        // Leading tabs strip, runs collapse, the last slot keeps
        // the raw remainder.
        let mut out = [Vec::new(), Vec::new()];
        read_fields(b"\t\ta\t\tb\tc", &mut out);
        assert_eq!(out[0], b"a".to_vec());
        assert_eq!(out[1], b"b\tc".to_vec());
        // Missing slots read empty.
        let mut out = [Vec::new(), Vec::new(), Vec::new()];
        read_fields(b"a", &mut out);
        assert_eq!(out, [b"a".to_vec(), Vec::new(), Vec::new()]);
    }

    #[test]
    fn home_prefix_strips_only_on_match() {
        let home = Path::new("/home/op");
        assert_eq!(
            strip_home_prefix(home, b"/home/op/.dot-init-parent.n.h"),
            b".dot-init-parent.n.h",
        );
        assert_eq!(strip_home_prefix(home, b"/home/other/x"), b"/home/other/x",);
        assert_eq!(strip_home_prefix(home, b"/home/op"), b"/home/op");
    }
}
