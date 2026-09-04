//! Manifest, replacement-identity, and quarantine helpers from
//! `lib/dot/repos/overlays.sh`: link-target derivation, manifest
//! record parsing, the manifest safety gate, the managed-generation
//! fingerprint, the destination context, the quarantine orchestrator,
//! the restore/commit halves of quarantined links, and the publish
//! leaf layer (link recording and matching, ownership gates, and
//! private writers).
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

use std::collections::HashMap;
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
    let destination = format!("{}/{}", inputs.home, rel);
    let roots = reserved_roots_for(inputs)?;
    if reserved::candidate_path_is_reserved_from_roots(
        &destination,
        &roots,
        &inputs.home,
        &checkout_for(inputs),
        &inputs.pwd,
    ) {
        return None;
    }
    let leaf = reserved::physical_leaf_candidate(&destination, &inputs.pwd).ok()?;
    Some(DestinationContext {
        physical: PathBuf::from(leaf.path),
        parent: PathBuf::from(leaf.physical_parent),
        parent_identity: leaf.parent_identity,
    })
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
