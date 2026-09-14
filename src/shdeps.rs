//! Shdeps lock reader and installer trust predicates, part 1 of
//! `lib/dot/providers/shdeps.sh`.
//!
//! This family is the self-contained trust root the rest of the
//! provider builds on: the pinned `support/shdeps.lock` reader
//! (`_dot_shdeps_lock_value`), the digest helper
//! (`_dot_shdeps_sha256`), the installer-hash predicate
//! (`_dot_shdeps_installer_hash_matches`), the origin allowlist
//! (`_dot_shdeps_origin_allowed`), and the ownership gate
//! (`_dot_shdeps_path_owned`). Later lanes own the stateful
//! remainder (env configuration, installer selection, bounded runs,
//! remainder (env configuration, installer selection, bounded runs, downloads,
//! ABI probes, and the re-exec orchestration).
//!
//! Engine boundaries: the lock parses as bytes (the shell's
//! `IFS= read -r` keeps carriage returns, so CRLF stays malformed
//! here too); digests come from the same `sha256sum` / `shasum -a
//! 256` baseline the shell shells out to (adding a hash crate for
//! one predicate would trade the pinned-oracle parity the slice
//! pattern requires); and ownership reads `symlink_metadata`,
//! which refuses a final symlink exactly like the shell's `stat`
//! without `-L` (the shared gate shape with
//! [`crate::extension_trust`], whose link counts this predicate
//! never needed).

//!
//! Part 2 in this module is the durable one-generation guard the
//! provider update uses to detect a double dot change: the revision
//! gate (`_dot_provider_revision_valid`, 40-64 hex digits of either
//! case per `^[0-9a-fA-F]{40,64}$` — distinct from the lock
//! reader's exact-40 lowercase gate), the state path
//! (`_dot_reexec_checkpoint_path`), the active revision reader
//! (`_dot_active_revision`), and the record reader, writer, and
//! consumer (`_dot_provider_read_checkpoint`,
//! `_dot_provider_write_checkpoint`,
//! `_dot_provider_consume_checkpoint`).

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::temp::MoveCache;

/// Whether `bytes` are exactly 40 lowercase hex digits, like the
/// shell `^[0-9a-f]{40}$` revision gate (uppercase stays invalid).
fn is_revision(bytes: &[u8]) -> bool {
    bytes.len() == 40
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Whether `bytes` are exactly 64 lowercase hex digits, like the
/// shell `^[0-9a-f]{64}$` digest gate.
fn is_install_sha256(bytes: &[u8]) -> bool {
    bytes.len() == 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Whether `bytes` are a positive decimal integer without a leading
/// zero, like the shell `^[1-9][0-9]*$` ABI gate.
fn is_abi(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if !matches!(bytes[0], b'1'..=b'9') {
        return false;
    }
    bytes.iter().all(|byte| byte.is_ascii_digit())
}

/// Read the pinned `(revision, install_sha256, abi)` triple from
/// `$source_root/support/shdeps.lock`, or `None` for every shell
/// refusal: an unreadable file, any line count but three (a missing
/// trailing newline still counts its line, exactly like the shell
/// `read ... || [[ -n $line ]]` fallback), unordered or wrongly
/// prefixed lines, or a malformed value.
fn lock_fields(source_root: &Path) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let content = std::fs::read(source_root.join("support/shdeps.lock")).ok()?;
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    // `read` consumes one trailing newline as its delimiter without
    // producing a field; without it the tail counts as a line.
    if content.ends_with(b"\n") {
        lines.pop();
    }
    if lines.len() != 3 {
        return None;
    }
    let revision = lines[0].strip_prefix(b"revision=")?;
    let install_sha256 = lines[1].strip_prefix(b"install_sha256=")?;
    let abi = lines[2].strip_prefix(b"abi=")?;
    if !is_revision(revision) || !is_install_sha256(install_sha256) || !is_abi(abi) {
        return None;
    }
    Some((revision.to_vec(), install_sha256.to_vec(), abi.to_vec()))
}

/// `_dot_shdeps_lock_value`: the pinned value for `key`
/// (`revision`, `install_sha256`, or `abi`), or `None` for an
/// unknown key or any malformed lock, like the shell exit 1.
pub fn lock_value(source_root: &Path, key: &str) -> Option<String> {
    let (revision, install_sha256, abi) = lock_fields(source_root)?;
    let value = match key {
        "revision" => revision,
        "install_sha256" => install_sha256,
        "abi" => abi,
        _ => return None,
    };
    // Values passed the ASCII gates above, so UTF-8 always holds.
    String::from_utf8(value).ok()
}

/// `_dot_shdeps_origin_allowed`: whether `origin` is one of the six
/// official Shdeps remote spellings (three transports with and
/// without the `.git` suffix), like the shell `case` arms.
pub fn origin_allowed(origin: &str) -> bool {
    matches!(
        origin,
        "https://github.com/cgraf78/shdeps"
            | "https://github.com/cgraf78/shdeps.git"
            | "git@github.com:cgraf78/shdeps"
            | "git@github.com:cgraf78/shdeps.git"
            | "ssh://git@github.com/cgraf78/shdeps"
            | "ssh://git@github.com/cgraf78/shdeps.git"
    )
}

/// `_dot_shdeps_path_owned`: whether `path` stats to the caller
/// with octal-only permission bits carrying no group/other write
/// bit. `stat` without `-L` reports a symlink argument itself
/// (mode `0777`, tripping the write-bit gate), so
/// `symlink_metadata` reproduces that refusal exactly — a link to
/// a clean file still fails, like the shell. GNU `%a` and BSD
/// `%Lp` both report permission bits only, so masking `st_mode`
/// with `0o022` reproduces the shell `((8#$mode & 022))` gate;
/// file-type bits never intersect it.
pub fn path_owned(path: &Path, euid: u32) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.uid() == euid && meta.mode() & 0o022 == 0,
        Err(_) => false,
    }
}

/// `_dot_shdeps_sha256`: the hex digest of `path` via `sha256sum`,
/// falling back to `shasum -a 256`, like the shell. `None` mirrors
/// every failure the installer-hash predicate can observe (missing
/// tool, unreadable file, unparsable output). One intentional
/// hardening: the shell pipeline prints an empty digest with a
/// success status when the file is missing (`awk` masks the
/// failure); this reports `None` instead, which still refuses
/// through [`installer_hash_matches`] exactly like the shell.
pub fn sha256_file(path: &Path) -> Option<String> {
    let output = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            std::process::Command::new("shasum")
                .args(["-a", "256"])
                .arg(path)
                .output()
        })
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let digest = text.split_whitespace().next()?;
    if is_install_sha256(digest.as_bytes()) {
        Some(digest.to_string())
    } else {
        None
    }
}

/// `_dot_shdeps_installer_hash_matches`: whether `path` digests to
/// the lock's pinned `install_sha256`. A malformed lock, a missing
/// digest, or any mismatch refuses, like the shell.
pub fn installer_hash_matches(source_root: &Path, path: &Path) -> bool {
    match (lock_value(source_root, "install_sha256"), sha256_file(path)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    }
}

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
fn is_provider_revision(bytes: &[u8]) -> bool {
    (40..=64).contains(&bytes.len()) && bytes.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// `_dot_provider_revision_valid`: whether `revision` is a usable
/// checkpoint revision (40-64 hex digits, either case), like the
/// shell exit 0/1.
pub fn revision_valid(revision: &str) -> bool {
    is_provider_revision(revision.as_bytes())
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
    if !is_provider_revision(before) || !is_provider_revision(after) {
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
