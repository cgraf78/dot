//! Manifest, replacement-identity, and quarantine helpers from
//! `lib/dot/repos/overlays.sh`: link-target derivation, manifest
//! record parsing, the manifest safety gate, the managed-generation
//! fingerprint, and the restore/commit halves of quarantined links.
//!
//! Two engine boundaries apply. Values cross from bytes to `String`
//! via lossy conversion (the `profiles` precedent), so a non-UTF8
//! manifest compares lossy where the shell compares raw bytes; the
//! shape rules only test ASCII delimiters, so validation agrees and
//! only exact value equality can differ. And `manifest_safe`
//! mirrors the shell's fail-open quirk: when the gated file cannot
//! be opened for reading, the shell `while read` loop runs zero
//! times and the trailing `exact_targets == 0` test passes, so an
//! existing owned unreadable manifest reads safe (with bash's own
//! redirect error on stderr, which carries no engine meaning).

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use crate::errors::{Error, Result};
use crate::temp;

/// `_overlay_link_target`: the generated symlink target for `rel`
/// inside overlay `name`. One `../` per `/` in `rel`, so deeper
/// entries climb back out before descending into the overlay tree.
pub fn link_target(rel: &str, name: &str) -> String {
    let depth = rel.bytes().filter(|byte| *byte == b'/').count();
    format!("{}.dotfiles-{name}/home/{rel}", "../".repeat(depth))
}

/// One parsed manifest record: `rel<TAB>owner[<TAB>target]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRecord {
    /// Home-relative path (`a/b`), never empty or absolute.
    pub rel: String,
    /// Owning overlay name.
    pub owner: String,
    /// Link target: explicit in three-column files, derived by
    /// [`link_target`] in two-column files.
    pub target: String,
}

/// `_overlay_parse_manifest_record`: split one manifest line. The
/// two-column form derives the target; anything else shaped returns
/// `None` exactly where the shell returns 1. A `\n` inside a field
/// is accepted, like the shell `case` arms (only the stream split
/// and `\r` carry meaning); NUL bytes never reach this function
/// from a file because the shell `read` strips them first.
pub fn parse_manifest_record(line: &str) -> Option<ManifestRecord> {
    let mut fields = line.split('\t');
    let rel = fields.next().unwrap_or("");
    // No tab at all: `[[ $line == *TAB* ]] || return 1`.
    let second = fields.next()?;
    let (owner, target) = match fields.next() {
        // Two fields: `rel<TAB>owner`, derived target.
        None => (second.to_string(), link_target(rel, second)),
        // Three fields: explicit non-empty target; a fourth field
        // would fail the target shape below, like the shell.
        Some(third) => {
            if fields.next().is_some() || third.is_empty() {
                return None;
            }
            (second.to_string(), third.to_string())
        }
    };
    if !rel_shape_ok(rel) || !owner_shape_ok(&owner) {
        return None;
    }
    if target.contains(['\r', '\n']) {
        return None;
    }
    Some(ManifestRecord {
        rel: rel.to_string(),
        owner,
        target,
    })
}

/// The shell `case` arms for the `rel` field, byte for byte.
fn rel_shape_ok(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    if rel == "." || rel == ".." {
        return false;
    }
    if rel.starts_with('/') || rel.starts_with("./") || rel.starts_with("../") {
        return false;
    }
    if rel.ends_with('/') || rel.ends_with("/.") || rel.ends_with("/..") {
        return false;
    }
    if rel.contains("//") || rel.contains("/./") || rel.contains("/../") {
        return false;
    }
    true
}

/// The shell `case` arms for the `owner` field: non-empty, never
/// `.`/`..`, never holding a slash.
fn owner_shape_ok(owner: &str) -> bool {
    !owner.is_empty() && owner != "." && owner != ".." && !owner.contains('/')
}

/// `_overlay_private_regular_file`: owned regular non-symlink with
/// exactly one link and owner-only permission bits.
pub fn private_regular_file(path: &Path, euid: u32) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.file_type().is_file() || meta.uid() != euid {
        return false;
    }
    // `stat -c %a` prints the permission bits; every digit is octal
    // by construction, and `8#mode & 077` must be zero.
    if meta.mode() & 0o077 != 0 {
        return false;
    }
    meta.nlink() == 1
}

/// Manifest stream lines: NUL bytes stripped (like the shell
/// `read`), split on `\n` only (`read -r` keeps `\r`), final
/// partial line kept, and no manufactured trailing empty (like
/// `mapfile` on empty input).
pub fn stream_lines(content: &[u8]) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let content: Vec<u8> = content.iter().copied().filter(|byte| *byte != 0).collect();
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

/// `_overlay_manifest_safe`: the ownership/link gate, then every
/// line must parse; manifests with explicit (three-column) targets
/// additionally require the private-file invariant. An unreadable
/// file reads safe (the shell fail-open quirk documented above).
pub fn manifest_safe(path: &Path, euid: u32) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    // `-f && ! -L && -O`: a real regular file owned by the caller.
    if !meta.file_type().is_file() || meta.uid() != euid {
        return false;
    }
    // `stat -c %h` follows symlinks, but symlinks were excluded
    // above, so either metadata view agrees here.
    let linked_once = match std::fs::metadata(path) {
        Ok(meta) => meta.nlink() == 1,
        Err(_) => return false,
    };
    if !linked_once {
        return false;
    }
    let content = match std::fs::read(path) {
        Ok(content) => content,
        // Fail open like the shell: the `while read` loop over an
        // unreadable file runs zero times with `exact_targets == 0`.
        Err(_) => return true,
    };
    let mut exact_targets = false;
    for line in stream_lines(&content) {
        if line.matches('\t').count() == 2 {
            exact_targets = true;
        }
        if parse_manifest_record(&line).is_none() {
            return false;
        }
    }
    if exact_targets {
        return private_regular_file(path, euid);
    }
    true
}

/// `_overlay_pending_manifest_safe`: both invariants at once.
pub fn pending_manifest_safe(path: &Path, euid: u32) -> bool {
    private_regular_file(path, euid) && manifest_safe(path, euid)
}

/// Snapshot of installed managed links for rollback lookup,
/// replacing the shell's `DOT_OVERLAY_ROLLBACK_PATHS` /
/// `DOT_OVERLAY_ROLLBACK_TARGETS` globals with an explicit value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollbackSnapshot {
    /// Installed relative paths, parallel to [`RollbackSnapshot::targets`].
    pub paths: Vec<String>,
    /// Managed generation each path pointed at when snapshotted.
    pub targets: Vec<String>,
}

/// `_overlay_rollback_target`: the snapshotted managed generation
/// for `rel`, or `None` when absent — including ragged snapshots,
/// where the shell's length guard refuses before searching.
pub fn rollback_target<'a>(snapshot: &'a RollbackSnapshot, rel: &'a str) -> Option<&'a str> {
    if snapshot.paths.len() != snapshot.targets.len() {
        return None;
    }
    snapshot
        .paths
        .iter()
        .position(|path| path == rel)
        .map(|index| snapshot.targets[index].as_str())
}

/// `_overlay_link_target_available`: whether `target` names something
/// usable from the link at `home/rel` — a regular file or any
/// symlink for absolute targets, or the same resolved against the
/// link's own parent directory for relative ones.
pub fn link_target_available(rel: &str, target: &str, home: &str) -> bool {
    // `${destination%/*}` string semantics (not path parenting), so
    // an empty `HOME` still resolves against the filesystem root
    // exactly like the shell.
    let source = if Path::new(target).is_absolute() {
        target.to_string()
    } else {
        let destination = format!("{home}/{rel}");
        let parent = destination.rsplit_once('/').map_or("", |(dir, _)| dir);
        let parent = if parent.is_empty() { "/" } else { parent };
        format!("{parent}/{target}")
    };
    let source = Path::new(&source);
    std::fs::symlink_metadata(source).is_ok_and(|meta| meta.file_type().is_symlink())
        || std::fs::metadata(source).is_ok_and(|meta| meta.is_file())
}

/// Leaf identity plus `mode:size` for one path: the two halves
/// `_overlay_replacement_identity` rechecks after hashing.
fn live_generation(path: &Path) -> Result<(String, String)> {
    // No `-L` anywhere in this domain: plain `stat` reports the leaf
    // itself, so a link carries its own device, inode, raw mode, and
    // target-byte length — never its target's.
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat replacement generation",
        source,
    })?;
    Ok((
        format!("{}:{}", meta.dev(), meta.ino()),
        format!("{:x}:{}", meta.mode(), meta.size()),
    ))
}

/// `_overlay_replacement_identity`: the `dev:ino:modehex:size:digest`
/// fingerprint binding one managed path to its exact generation.
/// Symlinks digest their target bytes; regular files digest content
/// filter-free. Device and inode alone would miss an unlink-and-reuse
/// race, so the before/after metadata recheck rejects a generation
/// that changes while its fingerprint is being computed.
pub fn replacement_identity(source_root: &Path, path: &Path) -> Result<String> {
    let (identity, metadata) = live_generation(path)?;
    let kind = std::fs::symlink_metadata(path)
        .map_err(|source| Error::Io {
            context: "stat replacement file type",
            source,
        })?
        .file_type();
    let digest = if kind.is_symlink() {
        let target = std::fs::read_link(path).map_err(|source| Error::Io {
            context: "read replacement link target",
            source,
        })?;
        // `$(readlink)` strips every trailing newline before the
        // target bytes reach `hash-object --stdin`.
        let mut bytes = target.as_os_str().as_bytes();
        while let Some(rest) = bytes.strip_suffix(b"\n".as_slice()) {
            bytes = rest;
        }
        temp::file_text_digest(source_root, bytes)?
    } else if kind.is_file() {
        // `_dot_source_git hash-object --no-filters -- path`: the
        // `--` separator is unreachable for the absolute engine paths
        // here, so [`temp::file_digest`] hashes identically.
        temp::file_digest(source_root, path)?
    } else {
        return Err(Error::Usage {
            message: "replacement identity needs a file or symlink",
        });
    };
    let (identity_after, metadata_after) = live_generation(path)?;
    if identity_after != identity || metadata_after != metadata {
        return Err(Error::Usage {
            message: "replacement path changed during fingerprinting",
        });
    }
    Ok(format!("{identity}:{metadata}:{digest}"))
}

/// The fingerprint check both quarantine halves share:
/// `$(... 2>/dev/null || true)` reads a failed fingerprint as empty,
/// which only matches a degenerate empty expectation — compare the
/// same way instead of failing early.
fn quarantined_unchanged(source_root: &Path, parked: &Path, expected: &str) -> Result<()> {
    let actual = replacement_identity(source_root, parked).unwrap_or_default();
    if actual != expected {
        return Err(Error::Usage {
            message: "quarantined link generation changed",
        });
    }
    Ok(())
}

/// `_overlay_restore_quarantined_link`: move the parked generation
/// back only when it still matches `expected` and the physical path
/// is still fully absent (no file and no link — a late writer wins).
/// A late or non-empty stage directory fails the restore.
///
/// Known safe-direction divergence: the shell verifies the quarantine
/// move with lstat, so it restores a link whose target dangles;
/// [`temp::move_noreplace_with`] verifies by following, so that shape
/// reports failure (after the rename lands) instead of validating an
/// unresolvable generation.
pub fn restore_quarantined_link(
    source_root: &Path,
    physical: &Path,
    parked: &Path,
    stage: &Path,
    expected: &str,
    tool: &temp::MoveTool,
) -> Result<()> {
    quarantined_unchanged(source_root, parked, expected)?;
    if std::fs::symlink_metadata(physical).is_ok() {
        return Err(Error::Usage {
            message: "quarantine destination reappeared",
        });
    }
    temp::move_noreplace_with(parked, physical, tool)?;
    // `rmdir ... 2>/dev/null`: only the emptied stage removes.
    std::fs::remove_dir(stage).map_err(|source| Error::Io {
        context: "remove quarantine stage",
        source,
    })?;
    Ok(())
}

/// `_overlay_commit_quarantined_link`: drop the parked generation
/// once it still matches `expected`. Like `rm -f`, a missing parked
/// link still removes; like `rmdir`, a non-empty stage fails loudly.
pub fn commit_quarantined_link(
    source_root: &Path,
    parked: &Path,
    stage: &Path,
    expected: &str,
) -> Result<()> {
    quarantined_unchanged(source_root, parked, expected)?;
    match std::fs::remove_file(parked) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Io {
                context: "remove quarantined link",
                source,
            });
        }
    }
    std::fs::remove_dir(stage).map_err(|source| Error::Io {
        context: "remove quarantine stage",
        source,
    })?;
    Ok(())
}
