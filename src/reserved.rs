//! Reserved control-plane path predicates (slice 2 foundations).
//!
//! Ports `lib/dot/reserved.sh` exactly: the lexical `_dot_path_within`
//! test, overlay control-path rules, init-recovery sentinels,
//! absolute-path normalization, physical (symlink-resolved) directory
//! and leaf candidates, per-root canonicalization, the reserved-roots
//! inventory, install-transient patterns, and the leaf/candidate
//! reservation checks. Everything is a pure function of explicit
//! inputs — environment assembly (`HOME`, `SHDEPS_*`, overlay
//! records) stays with the caller so tests inject fixtures
//! deterministically.
//!
//! The shell reports wrong arity with exit 2; Rust surfaces the same
//! split as [`Error`] so callers map to identical exit codes.

use std::path::{Path, PathBuf};

/// Reserved-path failure, mirroring the shell exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity or malformed value (shell exit 2).
    Usage,
    /// A path needed for the answer does not resolve (shell exit 1).
    Unresolvable,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Unresolvable => 1,
        }
    }
}

/// Inputs for the reserved-roots inventory (`_dot_reserved_roots`).
#[derive(Debug, Clone)]
pub struct RootsInput {
    /// Client `$HOME`.
    pub home: String,
    /// Resolved XDG state home (caller runs `xdg::base` first; the
    /// shell aborts the whole inventory when that lookup fails).
    pub state_home: String,
    /// `${SHDEPS_INSTALL_DIR:-$HOME/.local/share}`.
    pub install_root: String,
    /// `${SHDEPS_STATE_DIR:-$state_home/shdeps}`.
    pub provider_state: String,
    /// Overlay link paths (the `path` field of each `OVERLAYS` record).
    pub overlay_paths: Vec<String>,
    /// `$DOT_INIT_BACKUP` when set and not `-`.
    pub init_backup: Option<String>,
}

/// `_dot_path_within`: equal to the root or strictly beneath it.
pub fn path_within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `_dot_overlay_control_path_reserved`: overlay payloads may not own
/// the profile-selection policy directories. The check runs both
/// directions — a path inside a control root and a path that would
/// swallow one are both reserved — because the base repository owns
/// these legitimately while every overlay source type is rejected.
pub fn overlay_control_path_reserved(path: &str) -> bool {
    const ROOTS: [&str; 2] = [".config/dot/profiles.d", ".config/dot/profile-selectors.d"];
    ROOTS
        .iter()
        .any(|root| path == *root || path_within(path, root) || path_within(root, path))
}

/// `_dot_init_recovery_path_reserved`: any path component that is an
/// init-transaction sentinel (`.dot-init-entry.*` and siblings).
/// `home` is the client `$HOME`; a `/` home strips only the leading
/// slash, otherwise the path must live under `$HOME/`.
pub fn init_recovery_path_reserved(path: &str, home: &str) -> bool {
    if home.is_empty() || !path.starts_with('/') {
        return false;
    }
    let relative = if home == "/" {
        match path.strip_prefix('/') {
            Some(relative) => relative,
            None => return false,
        }
    } else {
        match path.strip_prefix(home) {
            Some(relative) if relative.starts_with('/') => &relative[1..],
            _ => return false,
        }
    };
    relative.split('/').any(|component| {
        component.starts_with(".dot-init-entry.")
            || component.starts_with(".dot-init-parent.")
            || component.starts_with(".dot-init-delete.")
    })
}

/// `_dot_normalize_absolute_path`: lexical normalization against
/// `pwd` for relative inputs (`.`/`..`/duplicate separators
/// collapsed, `..` past root absorbed). Rejects empty input and any
/// newline or carriage return, like the shell.
pub fn normalize_absolute_path(input: &str, pwd: &str) -> Result<String, Error> {
    if input.is_empty() || input.contains(['\n', '\r']) {
        return Err(Error::Usage);
    }
    let joined;
    let absolute = if input.starts_with('/') {
        input
    } else {
        joined = format!("{pwd}/{input}");
        &joined
    };
    let mut normalized: Vec<&str> = Vec::new();
    for component in absolute.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                normalized.pop();
            }
            _ => normalized.push(component),
        }
    }
    if normalized.is_empty() {
        return Ok("/".to_string());
    }
    let mut out = String::with_capacity(absolute.len());
    for part in normalized {
        out.push('/');
        out.push_str(part);
    }
    Ok(out)
}

/// `dev:ino` identity for TOCTOU re-validation. `temp.sh` owns
/// `_dot_path_identity`; this private copy keeps the foundations PR
/// self-contained until the transactions module lands and re-homes it.
#[cfg(unix)]
fn dev_ino(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(format!("{}:{}", meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn dev_ino(_path: &Path) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "identity needs Unix metadata",
    ))
}

/// `_dot_physical_directory_candidate`: resolve the deepest existing
/// ancestor physically (`cd -P`, i.e. symlinks resolved) and re-append
/// the missing suffix. Non-directories are walked upward; reaching `/`
/// without finding one fails.
pub fn physical_directory_candidate(candidate: &str, pwd: &str) -> Result<String, Error> {
    let mut candidate = normalize_absolute_path(candidate, pwd)?;
    let mut suffix = String::new();
    loop {
        if Path::new(&candidate).is_dir() {
            break;
        }
        if candidate == "/" {
            return Err(Error::Unresolvable);
        }
        let part = candidate
            .rsplit('/')
            .next()
            .filter(|part| !part.is_empty())
            .ok_or(Error::Unresolvable)?;
        suffix = format!("/{part}{suffix}");
        // Normalized paths are absolute, so the parent is the text
        // before the last slash (`/` itself for top-level entries).
        let parent = match candidate.rfind('/') {
            Some(0) | None => "/",
            Some(ix) => &candidate[..ix],
        };
        if parent == candidate {
            return Err(Error::Unresolvable);
        }
        candidate = parent.to_string();
    }
    // `std::fs::canonicalize` is the `cd -P && pwd -P` equivalent for
    // an existing directory: every symlink on the route resolves.
    let physical = std::fs::canonicalize(&candidate).map_err(|_| Error::Unresolvable)?;
    let physical = physical.to_string_lossy().into_owned();
    if physical == "/" {
        Ok(format!("/{}", suffix.trim_start_matches('/')))
    } else {
        Ok(format!("{physical}{suffix}"))
    }
}

/// A physically resolved leaf: the reply path, its physical parent,
/// and that parent's `dev:ino` identity
/// (`_dot_physical_leaf_candidate`'s `REPLY`, `REPLY_PHYSICAL_PARENT`,
/// `REPLY_PARENT_IDENTITY`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafCandidate {
    /// Fully resolved path.
    pub path: String,
    /// Resolved parent directory.
    pub physical_parent: String,
    /// `dev:ino` of the physical parent.
    pub parent_identity: String,
}

/// `_dot_physical_leaf_candidate`.
pub fn physical_leaf_candidate(path: &str, pwd: &str) -> Result<LeafCandidate, Error> {
    let path = normalize_absolute_path(path, pwd)?;
    let (parent, base) = match path.rfind('/') {
        Some(0) => ("/".to_string(), path[1..].to_string()),
        Some(ix) => (path[..ix].to_string(), path[ix + 1..].to_string()),
        None => ("/".to_string(), path.clone()),
    };
    let physical_parent = physical_directory_candidate(&parent, pwd)?;
    let resolved = if physical_parent == "/" {
        format!("/{base}")
    } else {
        format!("{physical_parent}/{base}")
    };
    let parent_identity = dev_ino(Path::new(&physical_parent)).map_err(|_| Error::Unresolvable)?;
    Ok(LeafCandidate {
        path: resolved,
        physical_parent,
        parent_identity,
    })
}

/// `realpath` for a leaf that may dangle: GNU `realpath` resolves
/// every symlink on the route textually and only requires the PARENT
/// chain to exist, so a dangling leaf resolves to its target's
/// physical location (never to the link's own name). `canonicalize`
/// instead demands full existence, which would collapse the shell's
/// two-line root listing for dangling links into one. Loops and
/// unresolvable parents yield `None`, and the caller falls back to
/// the ancestor walk — exactly the shell's `realpath ... || fallback`
/// shape.
fn realpath_leaf(path: &str) -> Option<String> {
    let mut current = PathBuf::from(path);
    // Follow chains textually (`a -> b -> c`), resolving each relative
    // target against its link's parent, like the kernel would.
    for _ in 0..40 {
        let target = std::fs::read_link(&current).ok()?;
        current = if target.is_absolute() {
            target
        } else {
            match current.parent() {
                Some(parent) => parent.join(&target),
                None => target,
            }
        };
        if !std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink()) {
            break;
        }
    }
    if std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return None;
    }
    // Collapse `.`/`..` textually (a target may end in `..`, which has
    // no file name). Deliberately byte-blind: link targets bypass the
    // newline filter that guards requested paths.
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in current.components() {
        use std::path::Component;
        match component {
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_os_string()),
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }
    let mut cleaned = PathBuf::from("/");
    cleaned.extend(parts);
    let base = cleaned.file_name().map(|base| base.to_os_string())?;
    let parent = cleaned.parent()?;
    let mut resolved = std::fs::canonicalize(parent).ok()?;
    resolved.push(base);
    Some(resolved.to_string_lossy().into_owned())
}

/// `_dot_reserved_root`: the normalized request plus, when different,
/// its physical resolution. Leaf symlinks resolve like `realpath`
/// (dangling leaves included); ordinary roots resolve their deepest
/// existing ancestor.
pub fn reserved_root(requested: &str, pwd: &str) -> Result<Vec<String>, Error> {
    let normalized = normalize_absolute_path(requested, pwd)?;
    let mut roots = vec![normalized.clone()];
    let physical =
        if std::fs::symlink_metadata(&normalized).is_ok_and(|meta| meta.file_type().is_symlink()) {
            realpath_leaf(&normalized)
        } else {
            physical_directory_candidate(&normalized, pwd).ok()
        };
    // No let-chains (MSRV 1.85): nested `if`s instead.
    if let Some(physical) = physical {
        if physical != normalized {
            roots.push(physical);
        }
    }
    Ok(roots)
}

/// `_dot_reserved_roots`: the full control-plane inventory for one
/// client — state dir, provider state, client git dirs, install
/// staging paths, the checkout and its transient siblings, every
/// overlay link path, and the init backup when configured.
pub fn reserved_roots(input: &RootsInput, pwd: &str) -> Result<Vec<String>, Error> {
    let checkout = format!("{}/cgraf78/dot", input.install_root);
    let parent = checkout
        .rfind('/')
        .map(|ix| checkout[..ix].to_string())
        .unwrap_or_default();
    let name = checkout
        .rfind('/')
        .map(|ix| checkout[ix + 1..].to_string())
        .unwrap_or_default();
    let mut fixed = vec![
        format!("{}/dot", input.state_home),
        input.provider_state.clone(),
        format!("{}/.dotfiles", input.home),
        format!("{}/.dot-backup", input.home),
        format!("{}/.local/bin/.dot.dot-install-stage-v1", input.home),
        format!("{}/.local/lib/.dot.dot-install-stage-v1", input.home),
        format!("{}/.local/bin/dot", input.home),
        format!("{}/.local/lib/dot", input.home),
        format!("{}/.config/dot/profile-selectors.local.d", input.home),
        checkout.clone(),
        format!("{parent}/.{name}.install.lock"),
        format!("{parent}/.{name}.install.transaction"),
        format!("{parent}/.{name}.shdeps-repo-transition-v1"),
    ];
    fixed.extend(input.overlay_paths.iter().cloned());
    if let Some(backup) = &input.init_backup {
        fixed.push(backup.clone());
    }
    let mut roots = Vec::new();
    for path in fixed {
        roots.extend(reserved_root(&path, pwd)?);
    }
    Ok(roots)
}

/// `_dot_install_path_is_reserved`: installer transient siblings of
/// the checkout (lock/claim/clone/publish/tmp shapes). `parent` and
/// `name` are the checkout's dirname and basename; every shape is a
/// literal prefix plus an opaque suffix, so `starts_with` is exact.
pub fn install_path_is_reserved(path: &str, parent: &str, name: &str) -> bool {
    [
        format!("{parent}/.{name}.install.lock.owner."),
        format!("{parent}/.{name}.install.lock.claim."),
        format!("{parent}/.{name}.clone."),
        format!("{parent}/.{name}.publish."),
        format!("{parent}/{name}.tmp."),
        format!("{parent}/.{name}.tmp."),
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix.as_str()))
}

/// Checkout split helper for [`install_path_is_reserved`] callers.
/// Mirrors `${checkout%/*}` / `${checkout##*/}` exactly: without a
/// slash both expansions yield the whole string.
pub fn checkout_parent_name(checkout: &str) -> (String, String) {
    match checkout.rfind('/') {
        Some(ix) => (checkout[..ix].to_string(), checkout[ix + 1..].to_string()),
        None => (checkout.to_string(), checkout.to_string()),
    }
}

/// `_dot_path_is_reserved_from_roots`: a leaf path is unsafe when it
/// is an init-recovery sentinel, sits inside a reserved root, or
/// matches an installer transient. A snapshot failure upstream
/// reports "not reserved" (the shell's `|| return 0`), so this pure
/// core only answers from a given snapshot.
pub fn path_is_reserved_from_roots(
    path: &str,
    roots: &[String],
    home: &str,
    checkout: &str,
) -> bool {
    if init_recovery_path_reserved(path, home) {
        return true;
    }
    if roots
        .iter()
        .any(|root| !root.is_empty() && path_within(path, root))
    {
        return true;
    }
    let (parent, name) = checkout_parent_name(checkout);
    install_path_is_reserved(path, &parent, &name)
}

/// `_dot_candidate_path_is_reserved_from_roots`: candidate inventories
/// hold leaves, so a leaf is additionally unsafe when it would replace
/// an ancestor directory on the route to a reserved root (a tracked
/// `.local` symlink, for example). The route check maps the nearest
/// existing ancestor physically first; when that mapping fails the
/// leaf reports reserved (fail closed). A `$HOME/.dotfiles-*` path is
/// always reserved.
pub fn candidate_path_is_reserved_from_roots(
    path: &str,
    roots: &[String],
    home: &str,
    checkout: &str,
    pwd: &str,
) -> bool {
    if init_recovery_path_reserved(path, home) {
        return true;
    }
    let within_either = |probe: &str| {
        roots
            .iter()
            .any(|root| !root.is_empty() && (path_within(probe, root) || path_within(root, probe)))
    };
    if within_either(path) {
        return true;
    }
    match physical_directory_candidate(path, pwd) {
        Ok(physical) => {
            if init_recovery_path_reserved(&physical, home) {
                return true;
            }
            if within_either(&physical) {
                return true;
            }
        }
        Err(_) => return true,
    }
    if path.starts_with(format!("{home}/.dotfiles-").as_str()) {
        return true;
    }
    let (parent, name) = checkout_parent_name(checkout);
    install_path_is_reserved(path, &parent, &name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_needs_a_boundary() {
        assert!(path_within("/a/b", "/a/b"));
        assert!(path_within("/a/b/c", "/a/b"));
        assert!(!path_within("/a/bc", "/a/b"));
        assert!(!path_within("/a", "/a/b"));
        assert!(path_within("/", "/"));
    }

    #[test]
    fn overlay_control_roots_run_both_directions() {
        assert!(overlay_control_path_reserved(".config/dot/profiles.d"));
        assert!(overlay_control_path_reserved(".config/dot/profiles.d/host"));
        assert!(overlay_control_path_reserved(".config/dot"));
        assert!(!overlay_control_path_reserved(".config/dot/profiles.d2"));
        assert!(!overlay_control_path_reserved(".config/other"));
        assert!(overlay_control_path_reserved(
            ".config/dot/profile-selectors.d/x"
        ));
    }

    #[test]
    fn recovery_sentinels() {
        assert!(init_recovery_path_reserved(
            "/home/u/.dot-init-entry.123",
            "/home/u"
        ));
        assert!(init_recovery_path_reserved(
            "/home/u/sub/.dot-init-parent.1/x",
            "/home/u"
        ));
        assert!(init_recovery_path_reserved(
            "/home/u/.dot-init-delete.9",
            "/home/u"
        ));
        assert!(!init_recovery_path_reserved(
            "/home/u/.dot-init-entry",
            "/home/u"
        ));
        assert!(!init_recovery_path_reserved(
            "/home/u/.dot-init-entry.1",
            "/home/other"
        ));
        assert!(!init_recovery_path_reserved("relative", "/home/u"));
        assert!(!init_recovery_path_reserved("/x", ""));
        // Root home strips one slash.
        assert!(init_recovery_path_reserved("/.dot-init-entry.1", "/"));
        assert!(!init_recovery_path_reserved("/other/x", "/"));
    }

    #[test]
    fn normalization_matrix() {
        assert_eq!(normalize_absolute_path("/", "/pwd"), Ok("/".to_string()));
        assert_eq!(
            normalize_absolute_path("/a//b/./c", "/pwd"),
            Ok("/a/b/c".to_string())
        );
        assert_eq!(
            normalize_absolute_path("/a/b/../c", "/pwd"),
            Ok("/a/c".to_string())
        );
        assert_eq!(
            normalize_absolute_path("/../..", "/pwd"),
            Ok("/".to_string())
        );
        assert_eq!(
            normalize_absolute_path("rel/path", "/h"),
            Ok("/h/rel/path".to_string())
        );
        assert_eq!(
            normalize_absolute_path("../up", "/h/sub"),
            Ok("/h/up".to_string())
        );
        assert_eq!(normalize_absolute_path("", "/h"), Err(Error::Usage));
        assert_eq!(normalize_absolute_path("/a\nb", "/h"), Err(Error::Usage));
        assert_eq!(normalize_absolute_path("/a\rb", "/h"), Err(Error::Usage));
        assert_eq!(Error::Usage.code(), 2);
        assert_eq!(Error::Unresolvable.code(), 1);
    }

    #[test]
    fn reserved_root_resolves_leaf_symlinks_like_realpath() {
        let dir = crate::test_support::TempDir::new("reserved-root").expect("temp");
        let pwd = dir.path().to_string_lossy().into_owned();
        let link_name = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("symlink");
            std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("dangling"))
                .expect("symlink");
        }
        // Valid leaf link: normalized request plus the target.
        assert_eq!(
            reserved_root(&link_name("link"), &pwd).expect("ok"),
            vec![link_name("link"), link_name("real")]
        );
        // Dangling leaf link: normalized request plus the TARGET's
        // location — never the link's own name (GNU `realpath` only
        // requires the parent chain to exist).
        assert_eq!(
            reserved_root(&link_name("dangling"), &pwd).expect("ok"),
            vec![link_name("dangling"), link_name("missing")]
        );
        // Ordinary directory: just itself.
        assert_eq!(
            reserved_root(&link_name("real"), &pwd).expect("ok"),
            vec![link_name("real")]
        );
    }

    #[test]
    fn install_transients() {
        let parent = "/h/.local/share/cgraf78";
        assert!(install_path_is_reserved(
            &format!("{parent}/.dot.install.lock.owner.1"),
            parent,
            "dot"
        ));
        assert!(install_path_is_reserved(
            &format!("{parent}/.dot.install.lock.claim.2"),
            parent,
            "dot"
        ));
        assert!(install_path_is_reserved(
            &format!("{parent}/.dot.clone.3"),
            parent,
            "dot"
        ));
        assert!(install_path_is_reserved(
            &format!("{parent}/.dot.publish.4"),
            parent,
            "dot"
        ));
        assert!(install_path_is_reserved(
            &format!("{parent}/dot.tmp.5"),
            parent,
            "dot"
        ));
        assert!(install_path_is_reserved(
            &format!("{parent}/.dot.tmp.6"),
            parent,
            "dot"
        ));
        assert!(!install_path_is_reserved(
            &format!("{parent}/dot"),
            parent,
            "dot"
        ));
        assert!(!install_path_is_reserved(
            &format!("{parent}/.dot.install.lock"),
            parent,
            "dot"
        ));
        assert!(!install_path_is_reserved("/other/x", parent, "dot"));
    }
}
