//! Manifest, replacement-identity, and quarantine helpers from
//! `lib/dot/repos/overlays.sh`: link-target derivation, manifest
//! record parsing, the manifest safety gate, the managed-generation
//! fingerprint, the destination context, the quarantine orchestrator,
//! the restore/commit halves of quarantined links, the publish leaf
//! layer (link recording and matching, ownership gates, and private
//! writers), and the pending/fallback publishers (authority
//! discovery and loading, candidate appending, and the pending and
//! fallback-authority publishers).
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

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};
use crate::repos_base;
use crate::reserved;
use crate::temp;
use crate::xdg;

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

/// True when the host `stat` speaks GNU: `stat -c '%f'` renders the
/// raw mode as lowercase hex, while the BSD `stat -f '%p'` fallback
/// renders it as octal. Probed once per process in the shell's own
/// `||` order (like [`temp::MoveCache`] probes the move tool), so the
/// branch decision agrees with the shell on every host — including a
/// host with neither flavor, which fails exactly where the shell's
/// probe chain fails.
fn gnu_stat_flavor() -> Result<bool> {
    static FLAVOR: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    match FLAVOR.get_or_init(|| {
        if stat_probe(&["-c", "%f"]) {
            Some(true)
        } else if stat_probe(&["-f", "%p"]) {
            Some(false)
        } else {
            None
        }
    }) {
        Some(gnu) => Ok(*gnu),
        None => Err(Error::Usage {
            message: "no working stat flavor",
        }),
    }
}

/// One branch of the `stat` probe against `/`, quietly: success
/// decides, exactly like the shell's `2>/dev/null ||` chain.
fn stat_probe(args: &[&str]) -> bool {
    std::process::Command::new("stat")
        .args(args)
        .arg("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Leaf identity plus `mode:size` for one path: the two halves
/// `_overlay_replacement_identity` rechecks after hashing.
fn live_generation(path: &Path) -> Result<(String, String)> {
    // No `-L` anywhere in this domain: plain `stat` reports the leaf
    // itself, so a link carries its own device, inode, raw mode, and
    // target-byte length — never its target's. The mode rendering
    // follows the probed flavor: GNU hex, BSD octal.
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat replacement generation",
        source,
    })?;
    let mode = if gnu_stat_flavor()? {
        format!("{:x}", meta.mode())
    } else {
        format!("{:o}", meta.mode())
    };
    Ok((
        format!("{}:{}", meta.dev(), meta.ino()),
        format!("{mode}:{}", meta.size()),
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
/// Move verification lstates like the shell, so a parked link whose
/// target dangles still restores (its own identity verifies).
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

/// Env-derived inputs for [`destination_context`], replacing the
/// shell's `HOME`, `OVERLAYS`, and reserved-roots environment with
/// explicit values (the [`RollbackSnapshot`] precedent). Every field
/// mirrors one shell lookup: `home` is `$HOME`, the three `Option`s
/// are `$XDG_STATE_HOME` / `$SHDEPS_INSTALL_DIR` / `$SHDEPS_STATE_DIR`
/// (`None` selects the same `$HOME` fallbacks), `overlay_paths` are
/// the path fields of the `OVERLAYS` records, `init_backup` is
/// `$DOT_INIT_BACKUP` (`None` covers unset and `-`), and `pwd` is
/// the physical working directory for candidate resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationInputs {
    /// Client home directory.
    pub home: String,
    /// `$XDG_STATE_HOME` when exported.
    pub xdg_state_home: Option<String>,
    /// `$SHDEPS_INSTALL_DIR` when exported.
    pub install_dir: Option<String>,
    /// `$SHDEPS_STATE_DIR` when exported.
    pub state_dir: Option<String>,
    /// Overlay link paths.
    pub overlay_paths: Vec<String>,
    /// `$DOT_INIT_BACKUP` when set and not `-`.
    pub init_backup: Option<String>,
    /// Physical working directory.
    pub pwd: String,
}

/// The resolved destination: `_overlay_destination_context`'s
/// `OVERLAY_PHYSICAL_DESTINATION`, `OVERLAY_PHYSICAL_PARENT`, and
/// `OVERLAY_PARENT_IDENTITY` as one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationContext {
    /// Guarded destination path.
    pub physical: PathBuf,
    /// Its physical parent directory.
    pub parent: PathBuf,
    /// `dev:ino` of the physical parent.
    pub parent_identity: String,
}

/// `_overlay_destination_context`: the guarded destination for a
/// home-relative path, or `None` wherever the shell returns 1 — a
/// reserved destination or an unresolvable leaf. A failed
/// reserved-roots snapshot reads reserved (fail closed), exactly
/// like `_dot_reserved_roots_snapshot || return 0`.
pub fn destination_context(rel: &str, inputs: &DestinationInputs) -> Option<DestinationContext> {
    destination_context_for(&format!("{}/{}", inputs.home, rel), inputs)
}

/// One quarantined generation, replacing the shell's
/// `OVERLAY_ADOPTION_*` dynamically scoped outputs with an explicit
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    /// The managed path the link was parked from.
    pub physical: PathBuf,
    /// `stage/previous`: the parked link.
    pub parked: PathBuf,
    /// The quarantine stage directory.
    pub stage: PathBuf,
    /// Fingerprint the parked generation must still match.
    pub expected: String,
}

/// `_overlay_quarantine_rollback_link` outcome: adopted (shell 0),
/// the leaf is not the snapshotted generation (shell 1), or an
/// authorized generation raced or could not be quarantined safely
/// (shell 2 — the caller must not retry Git).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineOutcome {
    /// Parked safely; carries the adoption.
    Adopt(Adoption),
    /// Not the snapshotted managed generation: user-owned.
    NotManaged,
    /// Raced or unsafe: do not retry.
    Unsafe,
}

impl QuarantineOutcome {
    /// Shell exit code for this outcome.
    pub fn code(&self) -> i32 {
        match self {
            QuarantineOutcome::Adopt(_) => 0,
            QuarantineOutcome::NotManaged => 1,
            QuarantineOutcome::Unsafe => 2,
        }
    }
}

/// Owned inputs for [`quarantine_rollback_link`]: the installed-link
/// snapshot, the destination environment, the probed move tool (the
/// shell caches `DOT_MOVE_BIN`/`DOT_MOVE_MODE`), and the source root
/// for the sanitized `hash-object` binding.
#[derive(Debug, Clone)]
pub struct QuarantineInputs {
    /// Installed managed links and their generations.
    pub snapshot: RollbackSnapshot,
    /// Home, reserved-roots env, and working directory.
    pub context: DestinationInputs,
    /// Probed move tool.
    pub tool: temp::MoveTool,
    /// Source root for the sanitized `hash-object` binding.
    pub source_root: PathBuf,
}

/// `$(readlink)` bytes with every trailing newline stripped, or
/// `None` when the leaf is not a readable link — command
/// substitution strips trailing newlines before the shell compares.
fn readlink_stripped(path: &Path) -> Option<Vec<u8>> {
    let target = std::fs::read_link(path).ok()?;
    let mut bytes = target.as_os_str().as_bytes();
    while let Some(rest) = bytes.strip_suffix(b"\n".as_slice()) {
        bytes = rest;
    }
    Some(bytes.to_vec())
}

/// `mktemp -d "$parent/.$base.dot-overlay-adopt.XXXXXXXX"` plus the
/// enforcing `chmod 0700`, with `/dev/urandom` suffixes retried like
/// the overlay-context staging. The suffix alphabet differs
/// (hex vs `mktemp` alphanumerics); only the prefix, mode, and
/// uniqueness are contractual.
fn create_stage(parent: &Path, base: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;
    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        let random = std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut random| random.read_exact(&mut suffix).ok())
            .is_some();
        if !random {
            return None;
        }
        let mut name = std::ffi::OsString::from(".");
        name.push(base);
        name.push(".dot-overlay-adopt.");
        for byte in suffix {
            name.push(format!("{byte:02x}"));
        }
        let stage = parent.join(&name);
        match std::fs::create_dir(&stage) {
            Ok(()) => {
                if std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).is_ok()
                {
                    return Some(stage);
                }
                let _ = std::fs::remove_dir(&stage);
                return None;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// `_overlay_quarantine_rollback_link`: park the exact
/// rollback-authorized link with a same-parent rename before a base
/// pull adopts its path. `NotManaged` means the current leaf is not
/// the snapshotted managed generation; `Unsafe` means an authorized
/// generation raced or could not be quarantined safely.
///
/// The shell warns on the stranded/quarantine-lost paths; warnings
/// carry no engine meaning and neither branch retries, so only the
/// filesystem aftermath is contractual there.
pub fn quarantine_rollback_link(rel: &str, inputs: &QuarantineInputs) -> QuarantineOutcome {
    use QuarantineOutcome::{Adopt, NotManaged, Unsafe};
    let target = match rollback_target(&inputs.snapshot, rel) {
        Some(target) => target,
        None => return NotManaged,
    };
    let context = match destination_context(rel, &inputs.context) {
        Some(context) => context,
        None => return Unsafe,
    };
    let physical = &context.physical;
    let managed = std::fs::symlink_metadata(physical)
        .is_ok_and(|meta| meta.file_type().is_symlink())
        && readlink_stripped(physical).is_some_and(|link| link == target.as_bytes());
    if !managed {
        return NotManaged;
    }
    let expected = match replacement_identity(&inputs.source_root, physical) {
        Ok(expected) => expected,
        Err(_) => return Unsafe,
    };
    let base = physical.file_name().unwrap_or_default().to_os_string();
    let stage = match create_stage(&context.parent, &base) {
        Some(stage) => stage,
        None => return Unsafe,
    };
    let parked = stage.join("previous");
    if temp::move_noreplace_with(physical, &parked, &inputs.tool).is_err() {
        // The move reports failure, but a raced `mv` may still have
        // landed the source: only the fingerprint decides.
        if replacement_identity(&inputs.source_root, &parked).unwrap_or_default() != expected {
            if replacement_identity(&inputs.source_root, physical).unwrap_or_default() == expected {
                let _ = std::fs::remove_dir(&stage);
            }
            return Unsafe;
        }
    }
    let stable = replacement_identity(&inputs.source_root, &parked).unwrap_or_default() == expected
        && std::fs::symlink_metadata(&parked).is_ok_and(|meta| meta.file_type().is_symlink())
        && readlink_stripped(&parked).is_some_and(|link| link == target.as_bytes())
        && matches!(
            destination_context(rel, &inputs.context),
            Some(next)
                if next.physical == context.physical
                    && next.parent == context.parent
                    && next.parent_identity == context.parent_identity
        );
    if !stable {
        // Move the parked generation back when the physical path is
        // still free; either way the caller must not proceed.
        if std::fs::symlink_metadata(physical).is_err()
            && temp::move_noreplace_with(&parked, physical, &inputs.tool).is_ok()
        {
            let _ = std::fs::remove_dir(&stage);
        }
        return Unsafe;
    }
    Adopt(Adoption {
        physical: physical.clone(),
        parked,
        stage,
        expected,
    })
}

/// `_overlay_record_link_target`: the link target one overlay
/// publishes for `rel`. `sync` selects the derivation (`None`
/// applies the shell's `git` default); anything else is not a
/// publishable source.
pub fn record_link_target(rel: &str, name: &str, path: &str, sync: Option<&str>) -> Option<String> {
    match sync.unwrap_or("git") {
        // Like `_overlay_link_target`, the derivation never fails.
        "git" => Some(link_target(rel, name)),
        "none" => Some(format!("{path}/home/{rel}")),
        _ => None,
    }
}

/// `_overlay_link_matches`: the home link for `rel` reads back
/// `target` — the explicit expectation, or the overlay derivation
/// when `None`. Like `$(readlink)`, trailing newlines in the link
/// bytes compare stripped.
pub fn link_matches(home: &str, rel: &str, name: &str, target: Option<&str>) -> bool {
    if name.is_empty() {
        return false;
    }
    let expected = match target {
        Some(target) => target.to_string(),
        None => link_target(rel, name),
    };
    if expected.is_empty() {
        return false;
    }
    let destination = format!("{home}/{rel}");
    std::fs::symlink_metadata(&destination).is_ok_and(|meta| meta.file_type().is_symlink())
        && readlink_stripped(Path::new(&destination))
            .is_some_and(|link| link == expected.as_bytes())
}

/// `_overlay_active_provides`: some overlay ships `rel` as a file or
/// link, independent of any manifest. The sync discipline is
/// deliberately ignored here, exactly like the shell loop.
pub fn active_provides(overlays: &[String], rel: &str) -> bool {
    overlays.iter().any(|entry| {
        let (path, _) = repos_base::overlay_path_sync(entry);
        let shipped = format!("{path}/home/{rel}");
        std::fs::symlink_metadata(&shipped)
            .is_ok_and(|meta| meta.is_file() || meta.file_type().is_symlink())
    })
}

/// `_overlay_active_link_matches`: some overlay both ships `rel` and
/// owns the live home link for it.
pub fn active_link_matches(home: &str, overlays: &[String], rel: &str) -> bool {
    overlays.iter().any(|entry| {
        let name = entry.split('|').next().unwrap_or("");
        let (path, sync) = repos_base::overlay_path_sync(entry);
        let target = match record_link_target(rel, name, &path, Some(&sync)) {
            Some(target) => target,
            None => return false,
        };
        let shipped = format!("{path}/home/{rel}");
        std::fs::symlink_metadata(&shipped)
            .is_ok_and(|meta| meta.is_file() || meta.file_type().is_symlink())
            && link_matches(home, rel, name, Some(&target))
    })
}

/// `_overlay_authority_link_matches`: the live home link reads back
/// a recorded `(rel, target)` authority pair.
pub fn authority_link_matches(home: &str, targets: &[(String, String)], rel: &str) -> bool {
    let destination = format!("{home}/{rel}");
    let link = match readlink_stripped(Path::new(&destination)) {
        Some(link) => link,
        None => return false,
    };
    // `readlink` on a non-link fails, matching `[[ -L ]]` gating
    // the shell's read.
    if !std::fs::symlink_metadata(&destination).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return false;
    }
    targets
        .iter()
        .any(|(path, target)| path == rel && target.as_bytes() == link.as_slice())
}

/// `_overlay_pending_manifest_path`: the deterministic pending path
/// beside the manifest, discoverable after an unclean exit.
pub fn pending_manifest_path(manifest: &str) -> String {
    format!("{manifest}.pending")
}

/// The per-rel authority verdict cache behind
/// `_overlay_path_authority_cache`: while enabled, the first verdict
/// pins later queries; while disabled, every query re-evaluates.
#[derive(Debug, Clone, Default)]
pub struct AuthorityCache {
    enabled: bool,
    entries: HashMap<String, bool>,
}

impl AuthorityCache {
    /// Cache that re-evaluates every query.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            entries: HashMap::new(),
        }
    }

    /// Cache that pins the first verdict per rel.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            entries: HashMap::new(),
        }
    }
}

/// Reserved roots for [`path_is_authority`]: the
/// `_overlay_reserved_roots` newline-joined snapshot string, or the
/// live inventory computed from `inputs` like
/// `dot_candidate_path_is_reserved`.
fn authority_roots(inputs: &DestinationInputs, roots: Option<&str>) -> Option<Vec<String>> {
    match roots {
        Some(snapshot) => Some(
            snapshot
                .split('\n')
                .filter(|root| !root.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        None => reserved_roots_for(inputs),
    }
}

/// The reserved-roots inventory behind both [`destination_context`]
/// and [`path_is_authority`]: `None` reads reserved (fail closed),
/// exactly like `_dot_reserved_roots_snapshot || return 0`.
fn reserved_roots_for(inputs: &DestinationInputs) -> Option<Vec<String>> {
    let state_home = xdg::base(
        xdg::Kind::State,
        inputs.xdg_state_home.as_deref().unwrap_or(""),
        &inputs.home,
    )
    .ok()?;
    let install_root = inputs
        .install_dir
        .clone()
        .unwrap_or_else(|| format!("{}/.local/share", inputs.home));
    let provider_state = inputs
        .state_dir
        .clone()
        .unwrap_or_else(|| format!("{state_home}/shdeps"));
    reserved::reserved_roots(
        &reserved::RootsInput {
            home: inputs.home.clone(),
            state_home,
            install_root,
            provider_state,
            overlay_paths: inputs.overlay_paths.clone(),
            init_backup: inputs.init_backup.clone(),
        },
        &inputs.pwd,
    )
    .ok()
}

/// Checkout root for the install-reserved check, shared by the
/// destination and authority gates.
fn checkout_for(inputs: &DestinationInputs) -> String {
    let install_root = inputs
        .install_dir
        .clone()
        .unwrap_or_else(|| format!("{}/.local/share", inputs.home));
    format!("{install_root}/cgraf78/dot")
}

/// `_overlay_path_is_authority`: `rel` is overlay-owned — an overlay
/// control path, the manifests themselves, or a reserved candidate.
/// `roots` selects the `_overlay_reserved_roots` snapshot string or
/// the live inventory; the shell's nonzero error shapes all read
/// unauthoritative, so the verdict is a plain bool.
pub fn path_is_authority(
    home: &str,
    rel: &str,
    manifest: &str,
    legacy_manifest: &str,
    inputs: &DestinationInputs,
    roots: Option<&str>,
    cache: &mut AuthorityCache,
) -> bool {
    if cache.enabled {
        if let Some(verdict) = cache.entries.get(rel) {
            return *verdict;
        }
    }
    let destination = format!("{home}/{rel}");
    let pending = pending_manifest_path(manifest);
    let verdict = if reserved::overlay_control_path_reserved(rel)
        || destination == manifest
        || destination == legacy_manifest
        || destination == pending
    {
        true
    } else {
        match authority_roots(inputs, roots) {
            Some(roots) => reserved::candidate_path_is_reserved_from_roots(
                &destination,
                &roots,
                home,
                &checkout_for(inputs),
                &inputs.pwd,
            ),
            None => true,
        }
    };
    if cache.enabled {
        cache.entries.insert(rel.to_string(), verdict);
    }
    verdict
}

/// `_overlay_skip_worktree`: the base index carries a skip-worktree
/// bit for `rel`. A missing topology (or failed `git`) reads
/// unskipped, like the shell's empty `ls-files` capture.
pub fn skip_worktree(base: &repos_base::Base, rel: &str) -> bool {
    let prefix = match base.git_prefix() {
        Some(prefix) => prefix,
        None => return false,
    };
    repos_base::run_git(&prefix, &["ls-files", "-v", "--", rel])
        .is_some_and(|output| output.stdout.get(..2) == Some(b"S ".as_slice()))
}

/// `_overlay_tracked_path_clean`: `rel` is visible to Git and
/// unchanged from the index — no skip-worktree bit and a quiet
/// diff. A failed `git` reads dirty.
pub fn tracked_path_clean(base: &repos_base::Base, rel: &str) -> bool {
    if skip_worktree(base, rel) {
        return false;
    }
    let prefix = match base.git_prefix() {
        Some(prefix) => prefix,
        None => return false,
    };
    repos_base::run_git(&prefix, &["diff", "--quiet", "--", rel])
        .is_some_and(|output| output.status.success())
}

/// `_overlay_write_private_line`: atomically create `destination`
/// holding `line` plus a newline, mode `0600`, refusing existing
/// files and links. The temporary sibling plus noreplace rename
/// mirrors `mktemp` plus `_dot_move_noreplace`; the suffix alphabet
/// differs, which is not contractual.
pub fn write_private_line(
    destination: &Path,
    line: &str,
    euid: u32,
    tool: &temp::MoveTool,
) -> bool {
    if std::fs::symlink_metadata(destination).is_ok() {
        return false;
    }
    let mut contents = line.to_string();
    contents.push('\n');
    let temporary = match stage_sibling(destination, contents.as_bytes()) {
        Some(temporary) => temporary,
        None => return false,
    };
    let done = temp::move_noreplace_with(&temporary, destination, tool).is_ok()
        && private_regular_file(destination, euid);
    if !done {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    true
}

/// `mktemp "${destination}.tmp.XXXXXX"` plus `chmod 0600` and the
/// line write: an exclusively-created `0600` sibling. Creation,
/// chmod, then write matches the shell order, so no signal window
/// leaves content at umask permissions.
fn stage_sibling(destination: &Path, contents: &[u8]) -> Option<PathBuf> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let parent = destination.parent()?;
    let mut prefix = destination.file_name().unwrap_or_default().to_os_string();
    prefix.push(".tmp.");
    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut random| random.read_exact(&mut suffix).ok())?;
        let mut name = prefix.clone();
        for byte in suffix {
            name.push(format!("{byte:02x}"));
        }
        let candidate = parent.join(&name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(&candidate) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
        if std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600)).is_err() {
            let _ = std::fs::remove_file(&candidate);
            return None;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&candidate)
            .ok()?;
        if file.write_all(contents).is_err() {
            let _ = std::fs::remove_file(&candidate);
            return None;
        }
        return Some(candidate);
    }
    None
}

/// `_overlay_private_directory`: an owned real directory with no
/// group/other permission bits. The shell's octal-digit guard is
/// subsumed by computing the bits directly.
pub fn private_directory(path: &Path, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    meta.is_dir() && meta.uid() == euid && meta.mode() & 0o077 == 0
}

/// `_overlay_authority_files` selection: the pending path plus
/// the existing regular manifests, in candidate order with
/// duplicates removed (`OVERLAY_AUTHORITY_MANIFESTS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityFiles {
    /// The write-ahead manifest path (`REPLY`).
    pub pending: String,
    /// Existing safe manifests, selected/legacy/pending order.
    pub manifests: Vec<String>,
}

/// `-e || -L`: any filesystem presence, dangling links included.
fn any_presence(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `-f && ! -L`: a real regular file, never a symlink (the lstat
/// view already excludes links, so no follow is needed).
fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// `_overlay_authority_files`: the pending path plus every existing
/// safe manifest. The error carries the unsafe path (`REPLY` on
/// failure): the selected manifest, then the pending one, then
/// whichever candidate fails the second pass.
pub fn authority_files(
    manifest: &str,
    legacy_manifest: &str,
    euid: u32,
) -> std::result::Result<AuthorityFiles, String> {
    let pending = pending_manifest_path(manifest);
    let manifest_path = Path::new(manifest);
    let pending_path = Path::new(&pending);
    if any_presence(manifest_path) && !manifest_safe(manifest_path, euid) {
        return Err(manifest.to_string());
    }
    if any_presence(pending_path) && !pending_manifest_safe(pending_path, euid) {
        return Err(pending.clone());
    }
    let mut manifests = Vec::new();
    for candidate in [manifest, legacy_manifest, pending.as_str()] {
        let path = Path::new(candidate);
        if !regular_file(path) {
            continue;
        }
        let safe = if candidate == pending {
            pending_manifest_safe(path, euid)
        } else {
            manifest_safe(path, euid)
        };
        if !safe {
            return Err(candidate.to_string());
        }
        if !manifests.iter().any(|seen| seen == candidate) {
            manifests.push(candidate.to_string());
        }
    }
    Ok(AuthorityFiles { pending, manifests })
}

/// Shared authority context behind the publish entry points,
/// replacing the shell's `HOME` / `DOT_OVERLAY_MANIFEST` /
/// `DOT_OVERLAY_LEGACY_MANIFEST` / reserved-roots environment and
/// the dynamically scoped authority maps with explicit values.
#[derive(Debug)]
pub struct AuthorityCtx<'a> {
    /// Client home directory (`$HOME`).
    pub home: &'a str,
    /// Selected manifest (`$DOT_OVERLAY_MANIFEST`).
    pub manifest: &'a str,
    /// Legacy manifest (`$DOT_OVERLAY_LEGACY_MANIFEST`).
    pub legacy_manifest: &'a str,
    /// Reserved-roots inputs for authority verdicts.
    pub inputs: &'a DestinationInputs,
    /// `_overlay_reserved_roots` snapshot, or live inventory.
    pub roots: Option<&'a str>,
    /// Per-rel authority verdict cache.
    pub cache: &'a mut AuthorityCache,
    /// Caller uid for the manifest safety gates.
    pub euid: u32,
}

/// `_overlay_load_authority` accumulation: live non-authority rels
/// (`_overlay_authority_paths`) and their recorded `(rel, target)`
/// pairs (`_overlay_authority_targets`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorityData {
    /// Live non-authority rels.
    pub paths: HashSet<String>,
    /// Recorded `(rel, target)` pairs for live rels.
    pub targets: HashSet<(String, String)>,
}

/// `_overlay_load_authority`: union every authority manifest into
/// owned sets, skipping authority-owned rels. A missing manifest
/// read or an unparsable record fails with the pending path (the
/// shell's untouched `REPLY`).
pub fn load_authority(ctx: &mut AuthorityCtx<'_>) -> std::result::Result<AuthorityData, String> {
    let found = authority_files(ctx.manifest, ctx.legacy_manifest, ctx.euid)?;
    let mut data = AuthorityData::default();
    for manifest in &found.manifests {
        let content = std::fs::read(manifest).map_err(|_| found.pending.clone())?;
        for line in stream_lines(&content) {
            let record = parse_manifest_record(&line).ok_or_else(|| found.pending.clone())?;
            if path_is_authority(
                ctx.home,
                &record.rel,
                ctx.manifest,
                ctx.legacy_manifest,
                ctx.inputs,
                ctx.roots,
                ctx.cache,
            ) {
                continue;
            }
            data.paths.insert(record.rel.clone());
            data.targets.insert((record.rel, record.target));
        }
    }
    Ok(data)
}

/// Lazily-opened append handle: the shell `>>` creates the
/// destination on the first record, so an empty source never
/// touches it (and a missing parent with no records still reads
/// success, like the shell loop).
fn append_line(state: &mut Option<std::fs::File>, path: &Path, line: &str) -> bool {
    use std::io::Write as _;
    if state.is_none() {
        *state = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
    }
    match state {
        Some(file) => file
            .write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .is_ok(),
        None => false,
    }
}

/// `_overlay_append_manifest_records`: copy every non-authority
/// record from `source` to `destination`, re-printed from the
/// parse (which derives two-column targets, like the shell
/// `printf`). A bad record or a failed write stops after the
/// lines already appended, exactly like the shell `return 1`; a
/// missing source fails without touching the destination (the
/// failed shell redirect).
pub fn append_manifest_records(
    source: &Path,
    destination: &Path,
    ctx: &mut AuthorityCtx<'_>,
) -> bool {
    let content = match std::fs::read(source) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let mut out = None;
    for line in stream_lines(&content) {
        let record = match parse_manifest_record(&line) {
            Some(record) => record,
            None => return false,
        };
        if path_is_authority(
            ctx.home,
            &record.rel,
            ctx.manifest,
            ctx.legacy_manifest,
            ctx.inputs,
            ctx.roots,
            ctx.cache,
        ) {
            continue;
        }
        if !append_line(
            &mut out,
            destination,
            &format!("{}\t{}\t{}", record.rel, record.owner, record.target),
        ) {
            return false;
        }
    }
    true
}

/// NUL-delimited inventory entries: every terminator-delimited
/// chunk counts (even empty ones, like the shell `read -d ''`),
/// while an unterminated tail drops with the final split piece —
/// the shell loop's partial-read skip.
fn nul_records(content: &[u8]) -> Vec<&[u8]> {
    let mut pieces: Vec<&[u8]> = content.split(|byte| *byte == 0).collect();
    pieces.pop();
    pieces
}

/// `_overlay_append_candidates`: record one overlay's inventory
/// into `destination`. `sync` selects the target derivation
/// (`None` applies the shell's `git` default). An authority-owned
/// entry or an unpublishable sync fails the whole call (the
/// shell's immediate `return 1`); a bad derived record or a
/// failed write stops after earlier lines, like the shell break.
pub fn append_candidates(
    destination: &Path,
    name: &str,
    path: &str,
    inventory: &Path,
    sync: Option<&str>,
    ctx: &mut AuthorityCtx<'_>,
) -> bool {
    if !regular_file(inventory) {
        return false;
    }
    let content = match std::fs::read(inventory) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let prefix = format!("{path}/home/");
    let mut out = None;
    for src in nul_records(&content) {
        let src = String::from_utf8_lossy(src);
        let rel = src.strip_prefix(prefix.as_str()).unwrap_or(&src);
        if path_is_authority(
            ctx.home,
            rel,
            ctx.manifest,
            ctx.legacy_manifest,
            ctx.inputs,
            ctx.roots,
            ctx.cache,
        ) {
            return false;
        }
        let target = match record_link_target(rel, name, path, sync) {
            Some(target) => target,
            None => return false,
        };
        let line = format!("{rel}\t{name}\t{target}");
        if parse_manifest_record(&line).is_none() || !append_line(&mut out, destination, &line) {
            return false;
        }
    }
    true
}

/// `_overlay_publish_pending`: freeze old authority plus every
/// exact link target this run may create into the pending
/// manifest before the first mutation. Returns the pending path,
/// or `None` wherever the shell returns 1. Failure carries no
/// reply: the shell `$REPLY` residue after a failed publish is
/// whatever helper ran last (a reserved-path probe, not the
/// pending path), and neither in-engine caller reads it — both
/// return without touching `REPLY`. A failed build is always
/// removed.
pub fn publish_pending(
    ctx: &mut AuthorityCtx<'_>,
    euid: u32,
    overlays: &[String],
    inventories: &HashMap<String, PathBuf>,
    tool: &temp::MoveTool,
) -> Option<String> {
    let found = match authority_files(ctx.manifest, ctx.legacy_manifest, euid) {
        Ok(found) => found,
        Err(_) => return None,
    };
    let pending = PathBuf::from(&found.pending);
    let pending_exists = any_presence(&pending);
    // `mktemp "${pending}.tmp.XXXXXX"` plus `chmod 600`: the
    // exclusive sibling `stage_sibling` already stages.
    let build = stage_sibling(&pending, b"")?;
    for manifest in &found.manifests {
        if !append_manifest_records(Path::new(manifest), &build, ctx) {
            let _ = std::fs::remove_file(&build);
            return None;
        }
    }
    for entry in overlays {
        let name = entry.split('|').next().unwrap_or("");
        let (path, sync) = repos_base::overlay_path_sync(entry);
        let inventory = match inventories.get(name) {
            Some(inventory) => inventory,
            None => continue,
        };
        if !append_candidates(&build, name, &path, inventory, Some(&sync), ctx) {
            let _ = std::fs::remove_file(&build);
            return None;
        }
    }
    let moved = if pending_exists {
        temp::move_replace_nodir_with(&build, &pending, tool)
    } else {
        temp::move_noreplace_with(&build, &pending, tool)
    };
    if moved.is_err() {
        let _ = std::fs::remove_file(&build);
        return None;
    }
    if !pending_manifest_safe(&pending, euid) {
        return None;
    }
    Some(found.pending)
}

/// `_overlay_publish_fallback_authority`: record one fallback
/// `(rel, owner, target)` in the pending manifest. An exact hit
/// in current authority is a no-op success that writes nothing.
pub fn publish_fallback_authority(
    rel: &str,
    owner: &str,
    target: &str,
    ctx: &mut AuthorityCtx<'_>,
    euid: u32,
    tool: &temp::MoveTool,
) -> bool {
    let found = match authority_files(ctx.manifest, ctx.legacy_manifest, euid) {
        Ok(found) => found,
        Err(_) => return false,
    };
    for manifest in &found.manifests {
        let content = match std::fs::read(manifest) {
            Ok(content) => content,
            Err(_) => return false,
        };
        for line in stream_lines(&content) {
            let record = match parse_manifest_record(&line) {
                Some(record) => record,
                None => return false,
            };
            if record.rel == rel && record.owner == owner && record.target == target {
                return true;
            }
        }
    }
    let pending = PathBuf::from(&found.pending);
    let pending_exists = any_presence(&pending);
    let build = match stage_sibling(&pending, b"") {
        Some(build) => build,
        None => return false,
    };
    for manifest in &found.manifests {
        if !append_manifest_records(Path::new(manifest), &build, ctx) {
            let _ = std::fs::remove_file(&build);
            return false;
        }
    }
    if !append_line(&mut None, &build, &format!("{rel}\t{owner}\t{target}")) {
        let _ = std::fs::remove_file(&build);
        return false;
    }
    let moved = if pending_exists {
        temp::move_replace_nodir_with(&build, &pending, tool)
    } else {
        temp::move_noreplace_with(&build, &pending, tool)
    };
    if moved.is_err() {
        let _ = std::fs::remove_file(&build);
        return false;
    }
    pending_manifest_safe(&pending, euid)
}

/// `_overlay_active_fallback_target`: the last publishable target
/// any active overlay ships for `rel`, skipping `excluded` (the
/// just-lost generation). Returns the target with its owner
/// (`REPLY` with `REPLY_OWNER`).
pub fn active_fallback_target(
    rel: &str,
    excluded: &str,
    overlays: &[String],
) -> Option<(String, String)> {
    let mut candidate: Option<(String, String)> = None;
    for entry in overlays {
        let name = entry.split('|').next().unwrap_or("");
        let (path, sync) = repos_base::overlay_path_sync(entry);
        let shipped = format!("{path}/home/{rel}");
        // `[[ -f ... || -L ... ]]`: a file (links followed) or any
        // link, broken included.
        let ships = std::fs::metadata(&shipped).is_ok_and(|meta| meta.is_file())
            || std::fs::symlink_metadata(&shipped).is_ok_and(|meta| meta.file_type().is_symlink());
        if !ships {
            continue;
        }
        let target = match record_link_target(rel, name, &path, Some(&sync)) {
            Some(target) => target,
            None => continue,
        };
        if target == excluded {
            continue;
        }
        candidate = Some((target, name.to_string()));
    }
    candidate.filter(|(target, _)| !target.is_empty())
}

/// How a replacement record pins its generation: the full
/// content identity, or the weaker pre-content device/inode pair
/// that new publications never create (`OVERLAY_REPLACE_IDENTITY_KIND`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceIdentityKind {
    /// Full `dev:ino:mode:size:digest` fingerprint.
    Content,
    /// Legacy `dev:ino` pair only.
    Legacy,
}

impl ReplaceIdentityKind {
    /// The shell spelling carried in records and variables.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplaceIdentityKind::Content => "content",
            ReplaceIdentityKind::Legacy => "legacy",
        }
    }
}

/// One validated replacement record: the seven
/// `OVERLAY_REPLACE_*` values `_overlay_replacement_read`
/// publishes as globals, as one owned value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceRecord {
    /// Guarded destination path.
    pub destination: String,
    /// Physical leaf the destination resolved to.
    pub physical: String,
    /// Link target the transaction installs.
    pub target: String,
    /// Expected generation fingerprint.
    pub expected: String,
    /// Which fingerprint shape `expected` carries.
    pub identity_kind: ReplaceIdentityKind,
    /// The `.dot-overlay-replace-v1` directory.
    pub transaction: String,
    /// `dev:ino` of the physical parent at publish time.
    pub parent_identity: String,
}

/// `_overlay_replacement_record_path`: the record name for
/// `destination` beside `manifest`. The derivation never fails —
/// only a failed hash does.
pub fn replacement_record_path(
    destination: &str,
    manifest: &str,
    source_root: &Path,
) -> Option<String> {
    // `printf '%s' "$destination" | _overlay_replacement_hash_object --stdin`.
    let hash = temp::file_text_digest(source_root, destination.as_bytes()).ok()?;
    Some(format!("{manifest}.replace.{hash}"))
}

/// `mktemp -d` under `tmp` with enforced `0700`, retrying
/// `/dev/urandom` names like the sibling stagers (the shell's
/// `umask 077 && mktemp -d`).
fn stage_private_dir(tmp: &Path, prefix: &str) -> Option<PathBuf> {
    use std::io::Read as _;
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut random| random.read_exact(&mut suffix).ok())?;
        let mut name = std::ffi::OsString::from(prefix);
        for byte in suffix {
            name.push(format!("{byte:02x}"));
        }
        let candidate = tmp.join(&name);
        if std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&candidate)
            .is_err()
        {
            continue;
        }
        if std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).is_err() {
            let _ = std::fs::remove_dir(&candidate);
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Feed `value` to `hash-object --stdin` inside the bare `repo`
/// and return the trimmed hash line.
fn hash_stdin_in(repo: &Path, value: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = temp::sanitized_git(repo, &["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(value.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `_overlay_replacement_hash_object_format`: the exact Git hash of
/// `value` under another object format, computed in a private
/// throwaway bare repository. Only the two formats Git supports
/// are accepted; the throwaway is always removed.
pub fn replacement_hash_object_format(
    object_format: &str,
    value: &str,
    tmp: &Path,
) -> Option<String> {
    if object_format != "sha1" && object_format != "sha256" {
        return None;
    }
    let temporary = stage_private_dir(tmp, "dot-overlay-record-hash.")?;
    // The subshell body as one closure so every path removes the
    // throwaway and reports failure, like `status=1` does.
    let outcome = (|| {
        let format = format!("--object-format={object_format}");
        let initialized = temp::sanitized_git(
            tmp,
            &[
                "init",
                "-q",
                "--bare",
                "--template=",
                format.as_str(),
                temporary.to_string_lossy().as_ref(),
            ],
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
        if !initialized {
            return None;
        }
        let hash = hash_stdin_in(&temporary, value)?;
        if hash.is_empty() {
            return None;
        }
        Some(hash)
    })();
    let _ = std::fs::remove_dir_all(&temporary);
    outcome
}

/// Lowercase hex only, like the shell `^[0-9a-f]{n}$` arms
/// (uppercase never matches, unlike `is_ascii_hexdigit`).
fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `_overlay_replacement_legacy_record_path_matches`: `record`
/// names `destination` under the *other* object format. A suffix
/// the length of the current hash is a current name, never a
/// legacy one.
pub fn replacement_legacy_record_path_matches(
    record: &str,
    destination: &str,
    current_hash: &str,
    manifest: &str,
    tmp: &Path,
) -> bool {
    let prefix = format!("{manifest}.replace.");
    let suffix = match record.strip_prefix(prefix.as_str()) {
        Some(suffix) => suffix,
        None => return false,
    };
    // Byte length is exact here: any multibyte suffix fails the
    // ASCII hex arms below, exactly like the shell.
    if suffix.len() == current_hash.len() {
        return false;
    }
    let object_format = if lower_hex(suffix, 40) {
        "sha1"
    } else if lower_hex(suffix, 64) {
        "sha256"
    } else {
        return false;
    };
    // The alternate hash derives from the destination itself,
    // so a same-length twin cannot collide.
    replacement_hash_object_format(object_format, destination, tmp)
        .is_some_and(|alternate| alternate == suffix)
}

/// `_overlay_replacement_generation_matches`: the live generation
/// at `path` still equals `expected` under `identity_kind`. A
/// failed fingerprint fails (the shell `|| return 1`), unlike the
/// quarantine halves' empty-compare shape.
pub fn replacement_generation_matches(
    path: &Path,
    expected: &str,
    identity_kind: &str,
    source_root: &Path,
) -> bool {
    match identity_kind {
        "content" => {
            replacement_identity(source_root, path).is_ok_and(|observed| observed == expected)
        }
        // Plain `stat` never takes `-L` in this domain: like the
        // content fingerprint's halves, the legacy pair is the
        // leaf's own device and inode, so a link answers itself —
        // never its target. (`temp::path_identity` follows and
        // does not apply here.)
        "legacy" => live_generation(path)
            .map(|(identity, _)| identity)
            .is_ok_and(|observed| observed == expected),
        _ => false,
    }
}

/// `_overlay_replacement_transaction_safe`: a private directory
/// holding only the `next`/`previous` staging links. Directory
/// listing includes dotfiles (the shell `dotglob`), but never
/// `.`/`..` on either side.
pub fn replacement_transaction_safe(transaction: &Path, euid: u32) -> bool {
    if !private_directory(transaction, euid) {
        return false;
    }
    std::fs::read_dir(transaction).is_ok_and(|entries| {
        entries.filter_map(|entry| entry.ok()).all(|entry| {
            matches!(
                entry.file_name().to_string_lossy().as_ref(),
                "next" | "previous"
            )
        })
    })
}

/// ASCII digits, nonempty.
fn digit_field(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// `dev:ino`: exactly two digit fields.
fn legacy_identity_shape(value: &str) -> bool {
    let mut fields = value.split(':');
    matches!((fields.next(), fields.next(), fields.next()), (Some(dev), Some(ino), None) if digit_field(dev) && digit_field(ino))
}

/// `dev:ino:modehex:size:digest`: the content fingerprint shape.
/// The metadata field accepts either hex case (like the shell
/// `[0-9A-Fa-f]+` arm); the digest is lowercase only.
fn content_identity_shape(value: &str) -> bool {
    let fields: Vec<&str> = value.split(':').collect();
    if fields.len() != 5 {
        return false;
    }
    digit_field(fields[0])
        && digit_field(fields[1])
        && !fields[2].is_empty()
        && fields[2].bytes().all(|byte| byte.is_ascii_hexdigit())
        && digit_field(fields[3])
        && (lower_hex(fields[4], 40) || lower_hex(fields[4], 64))
}

/// `_overlay_replacement_read`: validate one private record file
/// into its fields. `tmp` stages the throwaway repository behind
/// the legacy-name check. Every shape below mirrors one shell
/// gate, in order: private file, single line, six tab fields with
/// an empty remainder, absolute paths, a clean target, a shaped
/// parent identity, a shaped expectation, the record-name binding
/// (or its legacy proof), and the derived transaction path.
pub fn replacement_read(
    record: &Path,
    manifest: &str,
    euid: u32,
    source_root: &Path,
    tmp: &Path,
) -> Option<ReplaceRecord> {
    if !private_regular_file(record, euid) {
        return None;
    }
    let content = std::fs::read(record).ok()?;
    // `line=$(<"$record")`: every trailing newline stripped, any
    // other newline rejected.
    let line = String::from_utf8_lossy(&content);
    let line = line.trim_end_matches('\n');
    if line.contains('\n') {
        return None;
    }
    let mut fields = line.split('\t');
    let destination = fields.next().unwrap_or("");
    let physical = fields.next().unwrap_or("");
    let target = fields.next().unwrap_or("");
    let expected = fields.next().unwrap_or("");
    let transaction = fields.next().unwrap_or("");
    let parent_identity = fields.next().unwrap_or("");
    // `read` parks surplus words in the last name with their
    // delimiters, so the remainder must rejoin empty.
    if !fields.collect::<Vec<_>>().join("\t").is_empty() {
        return None;
    }
    if !destination.starts_with('/') || !physical.starts_with('/') || !transaction.starts_with('/')
    {
        return None;
    }
    if target.contains('\r') {
        return None;
    }
    if !legacy_identity_shape(parent_identity) {
        return None;
    }
    let identity_kind = if content_identity_shape(expected) {
        ReplaceIdentityKind::Content
    } else if legacy_identity_shape(expected) {
        ReplaceIdentityKind::Legacy
    } else {
        return None;
    };
    // The record name binds the destination under the current
    // hash; hashing failure fails like the shell `|| return 1`.
    let current = temp::file_text_digest(source_root, destination.as_bytes()).ok()?;
    let expected_record = format!("{manifest}.replace.{current}");
    if record.as_os_str().as_bytes() != expected_record.as_bytes() {
        if identity_kind != ReplaceIdentityKind::Legacy {
            return None;
        }
        if !replacement_legacy_record_path_matches(
            &record.to_string_lossy(),
            destination,
            &current,
            manifest,
            tmp,
        ) {
            return None;
        }
    }
    // `${physical%/*}/.${physical##*/}.dot-overlay-replace-v1`.
    let (parent, base) = physical.rsplit_once('/')?;
    if transaction != format!("{parent}/.{base}.dot-overlay-replace-v1") {
        return None;
    }
    Some(ReplaceRecord {
        destination: destination.to_string(),
        physical: physical.to_string(),
        target: target.to_string(),
        expected: expected.to_string(),
        identity_kind,
        transaction: transaction.to_string(),
        parent_identity: parent_identity.to_string(),
    })
}

/// `_overlay_replacement_cleanup`: drop a staged `next` link that
/// still names `target`, then require `previous` absent and remove
/// the transaction directory and the record. The final `rm -f`
/// succeeds on a missing record, but reports real removals.
pub fn replacement_cleanup(record: &Path, transaction: &Path, target: &str) -> bool {
    let next = transaction.join("next");
    if any_presence(&next) {
        // `[[ -L ... && $(readlink) == ... ]]`: a non-link, an
        // unreadable link, or a renamed target all fail.
        let staged = std::fs::symlink_metadata(&next)
            .is_ok_and(|meta| meta.file_type().is_symlink())
            && readlink_stripped(&next).is_some_and(|link| link == target.as_bytes());
        if !staged || std::fs::remove_file(&next).is_err() {
            return false;
        }
    }
    if any_presence(&transaction.join("previous")) {
        return false;
    }
    if std::fs::remove_dir(transaction).is_err() {
        return false;
    }
    match std::fs::remove_file(record) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// `_dot_init_safe_value`: nonempty with no tab or newline bytes.
fn init_safe_value(value: &str) -> bool {
    !value.is_empty() && !value.contains(['\t', '\n', '\r'])
}

/// `_dot_init_safe_relative_path`: a home-relative path with no
/// escapes and no `.git` component. Case folds ASCII-only like
/// the shell `${component,,}` under `LC_ALL=C` (never the Unicode
/// Kelvin-sign fold `to_lowercase` would add).
pub fn init_safe_relative_path(path: &str) -> bool {
    if !init_safe_value(path) {
        return false;
    }
    if path.starts_with('/')
        || path == "."
        || path == ".."
        || path.starts_with("./")
        || path.starts_with("../")
        || path.contains("/./")
        || path.contains("/../")
        || path.ends_with('/')
        || path.ends_with("/.")
        || path.ends_with("/..")
        || path.contains("//")
    {
        return false;
    }
    !path
        .split('/')
        .any(|component| component.eq_ignore_ascii_case(".git"))
}

/// `_overlay_ensure_destination_parent`: create every missing
/// component of `parent` under `home`. `mkdir -m '=rwx'` without
/// an explicit who honors the process umask, exactly like
/// `DirBuilder`. An existing directory (links followed, like
/// `mkdir -p`) is kept; any other occupant fails.
pub fn ensure_destination_parent(home: &str, parent: &str) -> bool {
    use std::os::unix::fs::DirBuilderExt as _;
    if parent == home {
        return true;
    }
    let relative = match parent.strip_prefix(home) {
        Some(relative) if relative.starts_with('/') => &relative[1..],
        _ => return false,
    };
    if !init_safe_relative_path(relative) {
        return false;
    }
    let mut current = PathBuf::from(home);
    for component in relative.split('/') {
        current.push(component);
        // `-d` follows symlinks, matching `mkdir -p` support for
        // user-owned parent indirection.
        if std::fs::metadata(&current).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        if current.symlink_metadata().is_ok() {
            return false;
        }
        if std::fs::DirBuilder::new()
            .mode(0o777)
            .create(&current)
            .is_err()
        {
            return false;
        }
    }
    true
}

/// `_overlay_record_final`: append one installed record and mark
/// its rel current. A failed append records nothing, like the
/// shell `|| return 1` before the map assignment.
pub fn record_final(
    rel: &str,
    owner: &str,
    target: &str,
    manifest_new: &Path,
    current: &mut HashSet<String>,
) -> bool {
    use std::io::Write as _;
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest_new)
    {
        Ok(file) => file,
        Err(_) => return false,
    };
    if writeln!(file, "{rel}\t{owner}\t{target}").is_err() {
        return false;
    }
    current.insert(rel.to_string());
    true
}

/// `rm -f`: a missing path still succeeds.
fn remove_optional(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// The guarded destination behind both [`destination_context`]
/// and the recovery publisher: reserved check plus physical-leaf
/// resolution for one absolute `destination`.
fn destination_context_for(
    destination: &str,
    inputs: &DestinationInputs,
) -> Option<DestinationContext> {
    let roots = reserved_roots_for(inputs)?;
    if reserved::candidate_path_is_reserved_from_roots(
        destination,
        &roots,
        &inputs.home,
        &checkout_for(inputs),
        &inputs.pwd,
    ) {
        return None;
    }
    let leaf = reserved::physical_leaf_candidate(destination, &inputs.pwd).ok()?;
    Some(DestinationContext {
        physical: PathBuf::from(leaf.path),
        parent: PathBuf::from(leaf.physical_parent),
        parent_identity: leaf.parent_identity,
    })
}

/// `_overlay_recover_replacement`: converge one crash record —
/// restore the parked generation or drop the settled one. Every
/// branch below mirrors one shell outcome, including which
/// artifacts each failure leaves behind. `pwd` resolves the
/// destination's physical leaf, like the shell working directory.
pub fn recover_replacement(
    record: &Path,
    manifest: &str,
    euid: u32,
    source_root: &Path,
    tmp: &Path,
    pwd: &str,
    tool: &temp::MoveTool,
) -> bool {
    let fields = match replacement_read(record, manifest, euid, source_root, tmp) {
        Some(fields) => fields,
        None => return false,
    };
    let physical = PathBuf::from(&fields.physical);
    let transaction = PathBuf::from(&fields.transaction);
    let previous = transaction.join("previous");
    let physical_parent = match physical.parent() {
        Some(parent) => parent.to_path_buf(),
        None => return false,
    };
    // The shell `stat` has no `-P`: the recorded parent identity
    // is compared following, and a vanished parent reads empty
    // (which never equals the shaped expectation).
    if temp::path_identity(&physical_parent)
        .map(temp::identity_string)
        .unwrap_or_default()
        != fields.parent_identity
    {
        return false;
    }
    // Lexical freshness: the destination still resolves at the
    // recorded physical leaf under the recorded parent.
    let mut lexical_parent_current = false;
    if let Ok(leaf) = reserved::physical_leaf_candidate(&fields.destination, pwd) {
        if leaf.path == fields.physical && leaf.parent_identity == fields.parent_identity {
            lexical_parent_current = true;
        }
    }
    if !any_presence(&transaction) {
        if !replacement_generation_matches(
            &physical,
            &fields.expected,
            fields.identity_kind.as_str(),
            source_root,
        ) {
            return false;
        }
        return remove_optional(record);
    }
    if !replacement_transaction_safe(&transaction, euid) {
        return false;
    }
    let next = transaction.join("next");
    if any_presence(&next) {
        let staged = std::fs::symlink_metadata(&next)
            .is_ok_and(|meta| meta.file_type().is_symlink())
            && readlink_stripped(&next).is_some_and(|link| link == fields.target.as_bytes());
        if !staged {
            return false;
        }
    }
    if any_presence(&previous) {
        if !replacement_generation_matches(
            &previous,
            &fields.expected,
            fields.identity_kind.as_str(),
            source_root,
        ) {
            return false;
        }
        let previous_identity = match replacement_identity(source_root, &previous) {
            Ok(identity) => identity,
            Err(_) => return false,
        };
        if !any_presence(&physical) {
            if temp::move_noreplace_with(&previous, &physical, tool).is_err() {
                return false;
            }
            // `$(... || true)`: a failed recheck reads empty and
            // only matches a degenerate empty parked identity.
            if replacement_identity(source_root, &physical).unwrap_or_default() != previous_identity
            {
                return false;
            }
            return replacement_cleanup(record, &transaction, &fields.target);
        }
        if std::fs::symlink_metadata(&physical).is_ok_and(|meta| meta.file_type().is_symlink())
            && readlink_stripped(&physical).is_some_and(|link| link == fields.target.as_bytes())
        {
            if lexical_parent_current {
                if std::fs::remove_file(&previous).is_err() {
                    return false;
                }
            } else {
                // The desired link landed under a stale lexical
                // parent: park it before restoring the old
                // generation, so a late third-party winner survives
                // every exclusive move.
                if any_presence(&next) {
                    return false;
                }
                if temp::move_noreplace_with(&physical, &next, tool).is_err() {
                    return false;
                }
                if !std::fs::symlink_metadata(&next).is_ok_and(|meta| meta.file_type().is_symlink())
                    || readlink_stripped(&next).is_some_and(|link| link != fields.target.as_bytes())
                {
                    let _ = temp::move_noreplace_with(&next, &physical, tool);
                    return false;
                }
                if temp::move_noreplace_with(&previous, &physical, tool).is_err() {
                    return false;
                }
            }
            return replacement_cleanup(record, &transaction, &fields.target);
        }
        return false;
    }
    if replacement_generation_matches(
        &physical,
        &fields.expected,
        fields.identity_kind.as_str(),
        source_root,
    ) || std::fs::symlink_metadata(&physical).is_ok_and(|meta| meta.file_type().is_symlink())
        && readlink_stripped(&physical).is_some_and(|link| link == fields.target.as_bytes())
    {
        return replacement_cleanup(record, &transaction, &fields.target);
    }
    false
}

/// `_overlay_recover_replacements`: converge every
/// `<manifest>.replace.*` record in byte-sorted glob order
/// (nullglob, no dotglob: a literal leading dot in the manifest
/// name still matches). The error names the first failing record
/// (`REPLY`), and later records stay untouched.
pub fn recover_replacements(
    manifest: &str,
    euid: u32,
    source_root: &Path,
    tmp: &Path,
    pwd: &str,
    tool: &temp::MoveTool,
) -> std::result::Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;
    let manifest_path = Path::new(manifest);
    let dir = match manifest_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let prefix = match manifest_path.file_name() {
        Some(name) => format!("{}.replace.", name.to_string_lossy()),
        None => return Ok(()),
    };
    let mut records: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name())
                .filter(|name| {
                    let bytes = name.as_bytes();
                    bytes.starts_with(prefix.as_bytes())
                        && (!bytes.starts_with(b".") || prefix.starts_with('.'))
                })
                .map(|name| dir.join(name))
                .collect()
        })
        .unwrap_or_default();
    records.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    for record in &records {
        if !recover_replacement(record, manifest, euid, source_root, tmp, pwd, tool) {
            return Err(record.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Owned inputs for [`publish_link`]: the shell's positional
/// parameters plus the environment the transactional publisher
/// reads (destination context, manifest, caller uid, Git source
/// root, hash throwaway base, and move tool).
#[derive(Debug)]
pub struct PublishLinkInputs<'a> {
    /// Link target to install.
    pub target: &'a str,
    /// Absolute destination path.
    pub destination: &'a str,
    /// Pinned generation, if the caller replaces one (an empty
    /// pin publishes fresh, like the shell `${3:-}`).
    pub expected: Option<&'a str>,
    /// Reserved-roots environment for destination resolution.
    pub inputs: &'a DestinationInputs,
    /// Selected manifest for the recovery record name.
    pub manifest: &'a str,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root: &'a Path,
    /// Base for the legacy-hash throwaway repository.
    pub tmp: &'a Path,
    /// Probed move tool.
    pub tool: &'a temp::MoveTool,
}

/// `_overlay_publish_link`: install the target at the destination,
/// transactionally when the inputs pin the generation being
/// replaced. Every early return below leaks exactly what the
/// shell leaves behind on that path: stage-only failures clean
/// the stage, while record/transaction failures after the write
/// keep the recovery trail.
pub fn publish_link(link: &PublishLinkInputs<'_>) -> bool {
    let target = link.target;
    let destination = link.destination;
    let inputs = link.inputs;
    let context = match destination_context_for(destination, inputs) {
        Some(context) => context,
        None => return false,
    };
    let physical = context.physical;
    let parent = context.parent;
    let parent_identity = context.parent_identity;
    let base = match Path::new(destination).file_name() {
        Some(base) => base.to_os_string(),
        None => return false,
    };
    let mut stage_name = std::ffi::OsString::from(".");
    stage_name.push(&base);
    stage_name.push(".overlay-link.");
    let stage = match stage_private_dir(&parent, &stage_name.to_string_lossy()) {
        Some(stage) => stage,
        None => return false,
    };
    let staged = stage.join("link");
    if std::os::unix::fs::symlink(target, &staged).is_err() {
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_dir(&stage);
        return false;
    }
    if let Some(expected) = link.expected.filter(|identity| !identity.is_empty()) {
        return publish_link_replace(
            link,
            expected,
            &physical,
            &parent,
            &parent_identity,
            &stage,
            &staged,
        );
    }
    if temp::move_noreplace_with(&staged, &physical, link.tool).is_err() {
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_dir(&stage);
        return false;
    }
    let _ = std::fs::remove_dir(&stage);
    let current = match destination_context_for(destination, inputs) {
        Some(current) => current,
        None => return false,
    };
    if current.parent != parent || current.parent_identity != parent_identity {
        if std::fs::symlink_metadata(&physical).is_ok_and(|meta| meta.file_type().is_symlink())
            && readlink_stripped(&physical).is_some_and(|link| link == target.as_bytes())
        {
            let _ = std::fs::remove_file(&physical);
        }
        return false;
    }
    std::fs::symlink_metadata(&physical).is_ok_and(|meta| meta.file_type().is_symlink())
        && readlink_stripped(&physical).is_some_and(|link| link == target.as_bytes())
}

/// The transactional half of [`publish_link`]: `expected` pins the
/// live generation, so every mutation is recoverable.
fn publish_link_replace(
    link: &PublishLinkInputs<'_>,
    expected: &str,
    physical: &Path,
    parent: &Path,
    parent_identity: &str,
    stage: &Path,
    staged: &Path,
) -> bool {
    let target = link.target;
    let destination = link.destination;
    let inputs = link.inputs;
    let manifest = link.manifest;
    let euid = link.euid;
    let source_root = link.source_root;
    let tmp = link.tmp;
    let tool = link.tool;
    let clean_stage = |stage: &Path| {
        let _ = std::fs::remove_dir(stage);
    };
    // `$(... || true)`: only a degenerate empty pin matches a
    // failed recheck, and pins are nonempty here.
    if replacement_identity(source_root, Path::new(destination)).unwrap_or_default() != expected {
        let _ = std::fs::remove_file(staged);
        clean_stage(stage);
        return false;
    }
    let record = match replacement_record_path(destination, manifest, source_root) {
        Some(record) => PathBuf::from(record),
        // Like the shell `|| return 1`, the stage leaks here.
        None => return false,
    };
    if any_presence(&record)
        && !recover_replacement(&record, manifest, euid, source_root, tmp, &inputs.pwd, tool)
    {
        return false;
    }
    let physical_base = match physical.file_name() {
        Some(base) => base.to_string_lossy().into_owned(),
        None => return false,
    };
    let transaction = parent.join(format!(".{physical_base}.dot-overlay-replace-v1"));
    if any_presence(&transaction) {
        return false;
    }
    let line = format!(
        "{destination}\t{}\t{target}\t{expected}\t{}\t{parent_identity}",
        physical.to_string_lossy(),
        transaction.to_string_lossy(),
    );
    if !write_private_line(&record, &line, euid, tool) {
        return false;
    }
    // The directory becomes durable recovery authority as soon as
    // creation succeeds: no cleanup past this point except through
    // recovery itself.
    use std::os::unix::fs::PermissionsExt as _;
    if std::fs::create_dir(&transaction).is_err()
        || std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o700)).is_err()
    {
        return false;
    }
    let next = transaction.join("next");
    if temp::move_noreplace_with(staged, &next, tool).is_err() {
        return false;
    }
    if std::fs::remove_dir(stage).is_err() {
        return false;
    }
    let previous = transaction.join("previous");
    if temp::move_noreplace_with(physical, &previous, tool).is_err() {
        let _ = recover_replacement(&record, manifest, euid, source_root, tmp, &inputs.pwd, tool);
        return false;
    }
    // A failed fingerprint aborts without recovery, like the
    // shell `|| return 1` (the parked generation stays).
    let parked_identity = match replacement_identity(source_root, &previous) {
        Ok(identity) => identity,
        Err(_) => return false,
    };
    if parked_identity != expected {
        if temp::move_noreplace_with(&previous, physical, tool).is_err() {
            return false;
        }
        let _ = replacement_cleanup(&record, &transaction, target);
        return false;
    }
    if temp::move_noreplace_with(&next, physical, tool).is_err() {
        let _ = recover_replacement(&record, manifest, euid, source_root, tmp, &inputs.pwd, tool);
        return false;
    }
    if !(std::fs::symlink_metadata(physical).is_ok_and(|meta| meta.file_type().is_symlink())
        && readlink_stripped(physical).is_some_and(|link| link == target.as_bytes()))
    {
        return false;
    }
    let current = match destination_context_for(destination, inputs) {
        Some(current) => current,
        None => return false,
    };
    if current.parent != *parent || current.parent_identity != *parent_identity {
        if temp::move_noreplace_with(physical, &next, tool).is_err() {
            return false;
        }
        if temp::move_noreplace_with(&previous, physical, tool).is_err() {
            return false;
        }
        let _ = replacement_cleanup(&record, &transaction, target);
        return false;
    }
    if std::fs::remove_file(&previous).is_err() {
        return false;
    }
    if !replacement_cleanup(&record, &transaction, target) {
        return false;
    }
    true
}
