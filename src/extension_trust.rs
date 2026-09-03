//! Trust checks from `lib/dot/extension-trust.sh` for executable
//! extension entry points and support files, plus the retiring
//! resolver. The manifest, link-target, and checkout identity
//! helpers are the canonical read-only implementations from
//! [`crate::repos_overlays`] and [`crate::overlays`].
//!
//! Engine boundaries: paths cross into string logic via lossy
//! conversion (the `profiles` precedent), so non-UTF8 paths compare
//! lossy where the shell compares raw bytes; readability uses a
//! read-only open probe for `[[ -r ]]`; and `readlink` output drops
//! trailing newlines exactly like shell command substitution.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// Inputs threaded from shell globals: caller identity, home, the
/// extensions root, the overlay manifest, and the retiring root.
/// Empty strings read as unset (`${VAR:-}`).
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// `$EUID` for ownership checks.
    pub euid: u32,
    /// `$HOME` for home-relative paths and record validation.
    pub home: String,
    /// `$DOT_EXTENSIONS_DIR`.
    pub extensions_dir: String,
    /// `$DOT_OVERLAY_MANIFEST`.
    pub manifest: String,
    /// `$DOT_RETIRING_OVERLAY_ROOT`.
    pub retiring_root: String,
}

/// Silent trust failure: `Usage` is a caller-arity problem (shell
/// exit 2), `Refused` a failed check (shell exit 1). Neither prints;
/// callers report their own warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity, silent (shell exit 2).
    Usage,
    /// Failed validation, silent (shell exit 1).
    Refused,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(&self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Refused => 1,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, _formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage | Error::Refused => Ok(()),
        }
    }
}

/// Read-only open probe for `[[ -r path ]]`.
fn readable(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

/// `_dot_extension_stat_fields`: GNU `stat -c '%u %a %h'` else BSD
/// `stat -f '%u %Lp %l`, owned by the caller with octal-only mode
/// carrying no group/other write bit. Returns the link count; the
/// mode digits are octal by construction on both implementations.
/// `stat` reports a symlink argument itself (mode 0777, never
/// dereferenced without `-L`), so links always fail the write-bit
/// gate; `symlink_metadata` reproduces that refusal exactly.
fn stat_fields(path: &Path, euid: u32) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.uid() != euid {
        return None;
    }
    if meta.mode() & 0o022 != 0 {
        return None;
    }
    Some(meta.nlink())
}

/// `_dot_extension_file_stat`: the field gate plus a single link.
pub fn file_stat(path: &Path, euid: u32) -> bool {
    stat_fields(path, euid).is_some_and(|links| links == 1)
}

/// `_dot_extension_directory_stat`: the field gate; the link count
/// is meaningless on directories and ignored, like the shell.
pub fn directory_stat(path: &Path, euid: u32) -> bool {
    stat_fields(path, euid).is_some()
}

/// A directory that is not itself a symlink, like
/// `[[ -d $path && ! -L $path ]]`.
fn dir_not_link(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.is_dir() && !meta.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// `_dot_extension_root_validate`: an absolute, normalized,
/// newline-free extensions root that is a stat-clean directory.
pub fn root_validate(dir: &str, euid: u32) -> bool {
    if !root_shape_ok(dir) {
        return false;
    }
    let path = Path::new(dir);
    dir_not_link(path) && directory_stat(path, euid)
}

/// The shell `case` arms for the extensions root, byte for byte.
fn root_shape_ok(dir: &str) -> bool {
    if dir.is_empty() || dir == "/" {
        return false;
    }
    if !dir.starts_with('/') {
        return false;
    }
    if dir.ends_with('/') || dir.ends_with("/.") || dir.ends_with("/..") {
        return false;
    }
    if dir.contains("//") || dir.contains("/./") || dir.contains("/../") {
        return false;
    }
    if dir.contains(['\n', '\r']) {
        return false;
    }
    true
}

/// Walk every parent component of `path` under `root`, requiring a
/// stat-clean non-symlink directory at each level.
fn walk_parent_components(root: &str, path: &str, euid: u32) -> bool {
    let relative = match path.strip_prefix(&format!("{root}/")) {
        Some(relative) => relative,
        None => return false,
    };
    if !relative.contains('/') {
        return true;
    }
    let parent = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
    let mut current = PathBuf::from(root);
    for component in parent.split('/') {
        current.push(component);
        if !dir_not_link(&current) || !directory_stat(&current, euid) {
            return false;
        }
    }
    true
}

/// The shell `case` arms for a path relative to the extensions
/// root, byte for byte.
fn relative_shape_ok(relative: &str) -> bool {
    if relative.is_empty() {
        return false;
    }
    if relative == "." || relative == ".." {
        return false;
    }
    if relative.starts_with("./") || relative.starts_with("../") {
        return false;
    }
    if relative.starts_with('/') {
        return false;
    }
    if relative.ends_with('/') || relative.ends_with("/.") || relative.ends_with("/..") {
        return false;
    }
    if relative.contains("//") || relative.contains("/./") || relative.contains("/../") {
        return false;
    }
    if relative.contains(['\n', '\r']) {
        return false;
    }
    true
}

/// `_dot_extension_parent_components_validate`: `path` under the
/// extensions root with clean shapes and a clean component walk.
pub fn parent_components_validate(path: &Path, extensions_dir: &str, euid: u32) -> bool {
    if !root_validate(extensions_dir, euid) {
        return false;
    }
    let text = path.to_string_lossy().into_owned();
    let relative = match text.strip_prefix(&format!("{extensions_dir}/")) {
        Some(relative) => relative,
        None => return false,
    };
    if !relative_shape_ok(relative) {
        return false;
    }
    walk_parent_components(extensions_dir, &text, euid)
}

/// `_dot_extension_owned_parent_components_validate`: the same walk
/// under an explicit root (which carries its own dir/stat gate
/// instead of the extensions-root shape rules).
pub fn owned_parent_components_validate(root: &str, path: &str, euid: u32) -> bool {
    let root_path = Path::new(root);
    if !dir_not_link(root_path) || !directory_stat(root_path, euid) {
        return false;
    }
    let relative = match path.strip_prefix(&format!("{root}/")) {
        Some(relative) => relative,
        None => return false,
    };
    if !relative.contains('/') {
        return true;
    }
    walk_parent_components(root, path, euid)
}

/// Read a symlink target the way `$(command readlink ...)` sees it:
/// trailing newlines stripped.
fn read_link_stripped(path: &Path) -> Option<String> {
    let target = std::fs::read_link(path).ok()?;
    Some(target.to_string_lossy().trim_end_matches('\n').to_string())
}

/// A stat-clean non-symlink directory plus its own directory stat.
fn directory_ok(path: &Path, euid: u32) -> bool {
    dir_not_link(path) && directory_stat(path, euid)
}

/// `_dot_extension_symlink_authorized`: a HOME link is authorized
/// only when the manifest records it for its target, a currently
/// active identity-matching Git checkout owns that name, the target
/// equals the generated link target, and the resolved path stays
/// under the checkout's `home/` tree with a clean component walk.
/// The first name-matching overlay entry decides; anything failing
/// there refuses the link outright, like the shell.
pub fn symlink_authorized(
    path: &Path,
    home: &str,
    manifest: &str,
    overlays: &[String],
    euid: u32,
) -> bool {
    // Home-relative path, with the `$HOME == /` spelling.
    let rel = if home == "/" {
        let text = path.to_string_lossy().into_owned();
        match text.strip_prefix('/') {
            Some(rel) => rel.to_string(),
            None => return false,
        }
    } else {
        let text = path.to_string_lossy().into_owned();
        match text.strip_prefix(&format!("{home}/")) {
            Some(rel) => rel.to_string(),
            None => return false,
        }
    };
    if std::fs::symlink_metadata(path)
        .map(|meta| !meta.file_type().is_symlink())
        .unwrap_or(true)
    {
        return false;
    }
    // `-f` follows symlinks: a manifest link to a regular file
    // passes here (its own gate decides symlinks separately).
    if manifest.is_empty() || !std::fs::metadata(manifest).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    if !crate::repos_overlays::manifest_safe(Path::new(manifest), euid) {
        return false;
    }
    let content = match std::fs::read(manifest) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let mut owner = String::new();
    let mut target = String::new();
    for line in crate::repos_overlays::stream_lines(&content) {
        let record = match crate::repos_overlays::parse_manifest_record(&line) {
            Some(record) => record,
            None => return false,
        };
        if record.rel != rel {
            continue;
        }
        // `readlink` runs only once the rel matches (`&&`
        // short-circuits), and its newlines strip.
        let seen = match read_link_stripped(path) {
            Some(seen) => seen,
            None => continue,
        };
        if seen == record.target {
            owner = record.owner;
            target = record.target;
            break;
        }
    }
    if owner.is_empty() || target.is_empty() {
        return false;
    }
    for entry in overlays {
        let fields: Vec<&str> = entry.split('|').collect();
        let name = fields.first().copied().unwrap_or("");
        if name != owner {
            continue;
        }
        let overlay_path = fields.get(1).copied().unwrap_or("");
        let url = fields.get(2).copied().unwrap_or("");
        let sync = fields.get(5).copied().unwrap_or("");
        let sync = if sync.is_empty() { "git" } else { sync };
        if sync != "git" {
            return false;
        }
        if crate::overlays::checkout_matches(Path::new(overlay_path), url, home).is_err() {
            return false;
        }
        let overlay_home = PathBuf::from(format!("{overlay_path}/home"));
        if !directory_ok(Path::new(overlay_path), euid) {
            return false;
        }
        if !directory_ok(&overlay_home, euid) {
            return false;
        }
        if crate::repos_overlays::link_target(&rel, &owner) != target {
            return false;
        }
        // `cd -P` plus `realpath`, compared as a string prefix like
        // `case $resolved in "$source_root"/*)`.
        let source_root = match overlay_home.canonicalize() {
            Ok(root) => root.to_string_lossy().into_owned(),
            Err(_) => return false,
        };
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved.to_string_lossy().into_owned(),
            Err(_) => return false,
        };
        if !resolved.starts_with(&format!("{source_root}/")) {
            return false;
        }
        if !owned_parent_components_validate(&source_root, &resolved, euid) {
            return false;
        }
        return true;
    }
    false
}

/// `_dot_extension_file_validate`: an entry point under the
/// extensions root that is readable, resolves to a stat-clean
/// regular file, and — for links — is manifest-authorized.
pub fn file_validate(path: &Path, inputs: &Inputs, overlays: &[String]) -> bool {
    if !parent_components_validate(path, &inputs.extensions_dir, inputs.euid) {
        return false;
    }
    if !readable(path) {
        return false;
    }
    let resolved = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if !symlink_authorized(path, &inputs.home, &inputs.manifest, overlays, inputs.euid) {
                return false;
            }
            match std::fs::canonicalize(path) {
                Ok(resolved) => resolved,
                Err(_) => return false,
            }
        }
        Ok(meta) if meta.is_file() => path.to_path_buf(),
        _ => return false,
    };
    let meta = match std::fs::symlink_metadata(&resolved) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.file_type().is_file() {
        return false;
    }
    file_stat(&resolved, inputs.euid)
}

/// `_dot_profile_deactivation_validate`: the retiring overlay's
/// fixed entry point, authorized by its saved Git identity. The
/// two-argument shape is static, so only refusal is representable.
pub fn deactivation_validate(
    record: &str,
    script: &str,
    home: &str,
    euid: u32,
) -> Result<(), Error> {
    if !crate::overlay_context::record_validate(record.as_bytes(), home) {
        return Err(Error::Refused);
    }
    let fields: Vec<&str> = record.split('|').collect();
    let name = fields.first().copied().unwrap_or("");
    let path = fields.get(1).copied().unwrap_or("");
    let url = fields.get(2).copied().unwrap_or("");
    let sync = fields.get(5).copied().unwrap_or("");
    // No `:-git` default here: the shell demands `git` literally.
    if sync != "git" || path != format!("{home}/.dotfiles-{name}") {
        return Err(Error::Refused);
    }
    if script != format!("{path}/dot/profile-deactivate") {
        return Err(Error::Refused);
    }
    if crate::overlays::checkout_matches(Path::new(path), url, home).is_err() {
        return Err(Error::Refused);
    }
    let script_path = Path::new(script);
    match std::fs::symlink_metadata(script_path) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return Err(Error::Refused),
    }
    if !owned_parent_components_validate(path, script, euid) {
        return Err(Error::Refused);
    }
    if !file_stat(script_path, euid) {
        return Err(Error::Refused);
    }
    Ok(())
}

/// The shell `case` arms for a retiring-relative path: malformed
/// shapes are caller errors (shell exit 2).
fn retiring_shape_ok(relative: &str) -> bool {
    if relative.is_empty() {
        return false;
    }
    if relative.starts_with('/') {
        return false;
    }
    if relative == "." || relative == ".." {
        return false;
    }
    if relative.starts_with("./") || relative.starts_with("../") {
        return false;
    }
    if relative.ends_with('/') || relative.ends_with("/.") || relative.ends_with("/..") {
        return false;
    }
    if relative.contains("//") || relative.contains("/./") || relative.contains("/../") {
        return false;
    }
    if relative.contains(['\n', '\r']) {
        return false;
    }
    true
}

/// `dot_retiring_overlay_file`: resolve a support file from the
/// already-validated retiring checkout. Malformed relatives are
/// usage errors (shell exit 2); failed checks refuse (exit 1).
pub fn retiring_overlay_file(relative: &str, inputs: &Inputs) -> Result<String, Error> {
    if !retiring_shape_ok(relative) {
        return Err(Error::Usage);
    }
    if inputs.retiring_root.is_empty() {
        return Err(Error::Refused);
    }
    let root_path = Path::new(&inputs.retiring_root);
    match std::fs::symlink_metadata(root_path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        _ => return Err(Error::Refused),
    }
    let path = format!("{}/{relative}", inputs.retiring_root);
    if !owned_parent_components_validate(&inputs.retiring_root, &path, inputs.euid) {
        return Err(Error::Refused);
    }
    let path = PathBuf::from(path);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return Err(Error::Refused),
    }
    if !readable(&path) || !file_stat(&path, inputs.euid) {
        return Err(Error::Refused);
    }
    Ok(path.to_string_lossy().into_owned())
}
