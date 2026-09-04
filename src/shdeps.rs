//! Provider re-exec checkpoint record, part 2 of
//! `lib/dot/providers/shdeps.sh`.
//!
//! This family is the durable one-generation guard the provider
//! update uses to detect a double dot change: the revision gate
//! (`_dot_provider_revision_valid`), the state path
//! (`_dot_reexec_checkpoint_path`), the active revision reader
//! (`_dot_active_revision`), and the record reader, writer, and
//! consumer (`_dot_provider_read_checkpoint`,
//! `_dot_provider_write_checkpoint`,
//! `_dot_provider_consume_checkpoint`). Part 1 (the `shdeps` lock
//! reader and installer trust predicates) lives on the unmerged
//! `rust-port-slice-37` lane; this module stacks on top of it once
//! both land.
//!
//! Later lanes own the remainder: the re-exec orchestration itself
//! (`_dot_provider_maybe_reexec`, which ends in `exec` and needs an
//! interpreter decision the record layer never makes), env
//! configuration, installer selection, bounded runs, downloads, and
//! ABI probes.
//!
//! Engine boundaries: the record parses as bytes (the shell's
//! `IFS= read -r` keeps carriage returns, so CRLF stays malformed
//! here too, exactly like the part-1 lock reader); the `before !=
//! after` comparison runs on the raw values before `after` is
//! lowercased, so mixed-case spellings of one revision still count
//! as a change, like the shell; the owner gate forks `id -u` via
//! [`crate::temp::current_uid`], matching the shell's
//! `[[ $uid == "$(id -u)" ]]` comparison rather than `$EUID` (the
//! two differ under `sudo`, and the shell chose `id -u` here);
//! sibling temps and the no-replace publish go through the already
//! ported [`crate::temp`] helpers, so the `mv -nT` / `mv -nh`
//! capability probe and its nesting recovery stay single-sourced;
//! and every shell `_warn` diagnostic folds into the boolean or
//! `None` refusal, like part 1 folded the lock redirection warning
//! — warnings are caller UI, the refusal is the contract.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::temp::MoveCache;

/// Exact bytes of the checkpoint magic first line, without the
/// trailing newline the shell's `read` strips.
const CHECKPOINT_MAGIC: &[u8] = b"cgraf78 dot provider reexec checkpoint v1";

/// Largest checkpoint the reader accepts, like the shell's
/// `$size -le 512` gate. A well-formed record tops out near 190
/// bytes (two 64-hex revisions), so the ceiling only ever bites on
/// foreign content.
const CHECKPOINT_MAX_SIZE: u64 = 512;

/// Whether `bytes` are a revision the checkpoint layer accepts:
/// 40 to 64 hex digits of either case, like the shell
/// `^[0-9a-fA-F]{40,64}$` gate. The upper bound is a range, not an
/// alternation: 41- through 63-digit strings pass on both sides.
fn is_revision(bytes: &[u8]) -> bool {
    (40..=64).contains(&bytes.len()) && bytes.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// `_dot_provider_revision_valid`: whether `revision` is a usable
/// checkpoint revision (40-64 hex digits, either case), like the
/// shell exit 0/1.
pub fn revision_valid(revision: &str) -> bool {
    is_revision(revision.as_bytes())
}

/// `_dot_reexec_checkpoint_path`: the durable guard record path
/// (`dot_xdg_path state dot/provider-reexec-failed`), or `None`
/// when the state base is unresolvable, like the shell leaving
/// `REPLY` empty with a nonzero exit. `xdg_state_home` is raw
/// `$XDG_STATE_HOME` (empty when unset) and `home` is raw `$HOME`.
pub fn checkpoint_path(xdg_state_home: &str, home: &str) -> Option<PathBuf> {
    let path = crate::xdg::path(
        crate::xdg::Kind::State,
        "dot/provider-reexec-failed",
        xdg_state_home,
        home,
    )
    .ok()?;
    Some(PathBuf::from(path))
}

/// `_dot_active_revision`: the selected checkout's `HEAD` via the
/// same sanitized `git -C` binding the shell's `_dot_source_git`
/// uses (caller `GIT_*` overrides scrubbed, system and global
/// config ignored). Always succeeds: an unresolvable checkout
/// yields the empty string, like the shell's trailing `|| true`.
pub fn active_revision(source_root: &Path) -> String {
    let output = crate::temp::sanitized_git(source_root, &["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(produced) if produced.status.success() => String::from_utf8_lossy(&produced.stdout)
            .trim_end_matches('\n')
            .to_string(),
        _ => String::new(),
    }
}

/// `_dot_provider_read_checkpoint`: the lowercased `after` revision
/// from the guard record at `path`, or `None` for every shell
/// refusal: a missing, symlinked, or non-regular file, a wrong
/// owner, any mode but `600`, a link count above one, a size above
/// 512 bytes, any line count but three (a missing trailing newline
/// still counts its line, exactly like the shell `read ... ||
/// [[ -n $line ]]` fallback), a wrong magic line or misplaced
/// `before=`/`after=` lines, a malformed revision, or equal
/// revisions.
pub fn read_checkpoint(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    // `symlink_metadata` reports a final link itself, so a passing
    // `is_file` already excludes links — the `[[ -f $path && ! -L
    // $path ]]` shape, where `-O` is subsumed by the `id -u`
    // comparison below under the test-relevant `euid == uid`
    // identity (and the stricter `id -u` half is what both sides
    // enforce).
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    let uid = crate::temp::current_uid()?;
    if meta.uid() != uid {
        return None;
    }
    // GNU `%a` and BSD `%Lp` both print minimal octal without
    // leading zeros, so `600` is the exact-match gate.
    if meta.mode() & 0o7777 != 0o600 {
        return None;
    }
    if meta.nlink() != 1 {
        return None;
    }
    if meta.len() > CHECKPOINT_MAX_SIZE {
        return None;
    }
    let content = std::fs::read(path).ok()?;
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    // `read` consumes one trailing newline as its delimiter without
    // producing a field; without it the tail counts as a line.
    if content.ends_with(b"\n") {
        lines.pop();
    }
    if lines.len() != 3 {
        return None;
    }
    if lines[0] != CHECKPOINT_MAGIC {
        return None;
    }
    // The shell's count-qualified `case` arms reject misplaced or
    // repeated keys outright; stripping each line's own prefix is
    // the same check.
    let before = lines[1].strip_prefix(b"before=")?;
    let after = lines[2].strip_prefix(b"after=")?;
    if !is_revision(before) || !is_revision(after) {
        return None;
    }
    // Compared raw, before lowercasing: mixed-case spellings of one
    // revision still count as a change, like the shell.
    if before == after {
        return None;
    }
    // Revisions passed the ASCII hex gate, so UTF-8 always holds.
    String::from_utf8(after.to_ascii_lowercase()).ok()
}

/// `_dot_provider_write_checkpoint`: publish the guard record for
/// the `before` -> `after` transition at `path`, lowercasing both
/// revisions like the shell's `${,,}` expansions. `false` mirrors
/// every shell refusal: a malformed revision, equal revisions, an
/// unmakable parent, an already-present path in any form
/// (`[[ ! -e $path && ! -L $path ]]`, so even a dangling link
/// refuses), or a stage/chmod/publish failure — in which case the
/// sibling temp is removed, like the shell's `rm -f`. The parent
/// directory keeps its `mkdir -p` plus best-effort `0700` shape,
/// and the publish goes through the shared no-replace move, so a
/// late path still refuses without replacing it.
pub fn write_checkpoint(before: &str, after: &str, path: &Path, moves: &mut MoveCache) -> bool {
    if !revision_valid(before) || !revision_valid(after) {
        return false;
    }
    let before = before.to_ascii_lowercase();
    let after = after.to_ascii_lowercase();
    if before == after {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    // Best effort, like the shell's `chmod ... || true`.
    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    // `symlink_metadata` succeeds exactly when the shell's
    // `-e $path || -L $path` holds (a dangling link reports via
    // `-L`), so any success here refuses.
    if path.symlink_metadata().is_ok() {
        return false;
    }
    let temporary = match crate::temp::sibling_tmp_for(path) {
        Ok(staged) => staged,
        Err(_) => return false,
    };
    let body = format!(
        "{}\nbefore={before}\nafter={after}\n",
        String::from_utf8_lossy(CHECKPOINT_MAGIC)
    );
    if std::fs::write(&temporary, body.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    if std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    if crate::temp::move_noreplace_cached(&temporary, path, moves).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    true
}

/// `_dot_provider_consume_checkpoint`: remove the guard record at
/// `path` after binding it to the checkout at `source_root`.
/// `true` covers both shell exit-0 shapes: no record present
/// (nothing to consume) and a consumed record whose `after`
/// revision matches the active revision with the device/inode
/// identity stable across validation. Every mismatch, malformed
/// record, identity change, or removal failure is `false`, with the
/// record left in place exactly like the shell — a mismatch means
/// the user must inspect provider state, never silently delete the
/// only explanation. The shell's `_warn` lines stay with the shell
/// caller; the refusal is the contract.
pub fn consume_checkpoint(path: &Path, source_root: &Path) -> bool {
    // Absent in every form is success, like the shell's
    // `[[ -e $path || -L $path ]] || return 0`.
    if path.symlink_metadata().is_err() {
        return true;
    }
    // Plain `stat` semantics (links followed), like
    // `_dot_path_identity`, whose `%d:%i` render the string compare
    // below reproduces.
    let identity = match crate::temp::path_identity(path) {
        Ok(pair) => crate::temp::identity_string(pair),
        Err(_) => return false,
    };
    let after = match read_checkpoint(path) {
        Some(pinned) => pinned,
        None => return false,
    };
    let active = active_revision(source_root);
    // The record's `after` is already lowercase; the active side
    // folds case, like `${active,,}`.
    if !revision_valid(&active) || active.to_ascii_lowercase() != after {
        return false;
    }
    let current = match crate::temp::path_identity(path) {
        Ok(pair) => crate::temp::identity_string(pair),
        Err(_) => return false,
    };
    if current != identity {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}
