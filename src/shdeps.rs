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
//! downloads, ABI probes, and the re-exec checkpoint).
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

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

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
/// `read ... || [[ -n $line ]]` fallback), unordered or mis-prefixed
/// lines, or a malformed value.
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
