//! Shdeps lock reader and installer trust predicates, part 1 of
//! `lib/dot/providers/shdeps.sh`.
//!
//! This family is the self-contained trust root the rest of the
//! provider builds on: the pinned `support/shdeps.lock` reader
//! (`_dot_shdeps_lock_value`), the digest helper
//! (`_dot_shdeps_sha256`), the installer-hash predicate
//! (`_dot_shdeps_installer_hash_matches`), the origin allowlist
//! (`_dot_shdeps_origin_allowed`), and the ownership gate
//! (`_dot_shdeps_path_owned`). This module also owns the stateful
//! provider flow: environment configuration, installer selection,
//! bounded runs, downloads, ABI probes, and re-exec orchestration.
//!
//! Engine boundaries: the lock parses as bytes (the shell's
//! `IFS= read -r` keeps carriage returns, so CRLF stays malformed
//! here too); digests use a small streaming SHA-256 implementation so
//! verification has neither an ambient-PATH dependency nor an unowned helper
//! process; ownership reads `symlink_metadata`,
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
/// Maximum `support/shdeps.lock` size read: a valid lock is three short
/// lines (~150 bytes), so anything past 4 KiB is corrupt, not pinned.
const LOCK_MAX_BYTES: u64 = 4096;

fn lock_fields(source_root: &Path) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    use std::io::Read as _;

    let path = source_root.join("support/shdeps.lock");
    let file = std::fs::File::open(path).ok()?;
    let mut content = Vec::new();
    file.take(LOCK_MAX_BYTES + 1)
        .read_to_end(&mut content)
        .ok()?;
    if content.len() as u64 > LOCK_MAX_BYTES {
        return None;
    }
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
/// bit. `stat` without `-L` reports a symlink argument itself, so
/// `symlink_metadata` preserves the host contract: GNU reports
/// symlinks as `0777` while macOS BSD reports owner-only bits. The
/// enclosing checkout trust gate rejects symlinks independently.
/// Masking `st_mode` with `0o022` reproduces the shell
/// `((8#$mode & 022))` gate; file-type bits never intersect it.
pub fn path_owned(path: &Path, euid: u32) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.uid() == euid && meta.mode() & 0o022 == 0,
        Err(_) => false,
    }
}

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    used: usize,
    bytes: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            used: 0,
            bytes: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.bytes = self.bytes.wrapping_add(input.len() as u64);
        if self.used != 0 {
            let count = (64 - self.used).min(input.len());
            self.block[self.used..self.used + count].copy_from_slice(&input[..count]);
            self.used += count;
            input = &input[count..];
            if self.used == 64 {
                let block = self.block;
                Self::compress(&mut self.state, &block);
                self.used = 0;
            }
        }
        while input.len() >= 64 {
            let block: &[u8; 64] = input[..64].try_into().expect("fixed SHA-256 block");
            Self::compress(&mut self.state, block);
            input = &input[64..];
        }
        // Invariant: the partial-fill branch above compressed whenever `used`
        // reached 64, and the loop consumed every full block, so `used + len`
        // is strictly below 64 here — the tail appends at the buffered
        // offset, preserving bytes from earlier `update` calls.
        let used = self.used;
        self.block[used..used + input.len()].copy_from_slice(input);
        self.used = used + input.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.bytes.wrapping_mul(8);
        self.block[self.used] = 0x80;
        self.used += 1;
        if self.used > 56 {
            self.block[self.used..].fill(0);
            let block = self.block;
            Self::compress(&mut self.state, &block);
            self.block = [0; 64];
            self.used = 0;
        }
        self.block[self.used..56].fill(0);
        self.block[56..].copy_from_slice(&bits.to_be_bytes());
        let block = self.block;
        Self::compress(&mut self.state, &block);
        let mut digest = [0; 32];
        for (chunk, word) in digest.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().expect("four-byte SHA-256 word"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let first = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// `_dot_shdeps_sha256`: the lowercase SHA-256 digest of `path`.
/// `None` mirrors every unreadable-file failure the installer-hash predicate
/// can observe. Hashing streams fixed-size blocks and launches no helper.
pub fn sha256_file(path: &Path) -> Option<String> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Some(
        hasher
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
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
    let mut command = crate::temp::sanitized_git(source_root, &["rev-parse", "HEAD"]);
    command.stdin(Stdio::null()).stderr(Stdio::null());
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    );
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
    if crate::cancellation::check().is_err() {
        return false;
    }
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
    if crate::cancellation::check().is_err() || std::fs::write(&temporary, body.as_bytes()).is_err()
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_hex(digest: [u8; 32]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sha256_update_accumulates_across_split_feeds() {
        // Fresh-review-B P2-2 RED pin: the streaming contract must hold for
        // multi-call feeds, not just single-shot ones. The buggy tail
        // overwrote the buffered prefix and produced `02db4ca4…` here.
        let mut split = Sha256::new();
        split.update(b"hello sh");
        split.update(b"deps\n");
        let mut single = Sha256::new();
        single.update(b"hello shdeps\n");
        assert_eq!(split.finish(), single.finish());
        assert_eq!(
            digest_hex({
                let mut hasher = Sha256::new();
                hasher.update(b"hello sh");
                hasher.update(b"deps\n");
                hasher.finish()
            }),
            "f990fdf034b96b1ce03e80928a7a7bd50889fe3a22c3c8da6011b61396e7ea80"
        );
    }

    #[test]
    fn sha256_matches_a_byte_at_a_time_mib_vector() {
        // 1 MiB of `i % 251` fed one byte per `update` call, digest from
        // `hashlib.sha256` (independent oracle, not this implementation).
        let mut hasher = Sha256::new();
        for index in 0..1024 * 1024 {
            hasher.update(&[(index % 251) as u8]);
        }
        assert_eq!(
            digest_hex(hasher.finish()),
            "631b84027d6b9e52b539c4e8373622d23032dfadc64d60af87339c9037e4f769"
        );
    }
}
