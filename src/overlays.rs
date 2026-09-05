//! Overlay discovery and local-source validation (slice 9).
//!
//! Ports `lib/dot/overlays.sh`: filename-derived identities,
//! descriptor safety gates, single-descriptor parsing with the
//! strict/permissive split, physical-directory resolution, local
//! (`sync=none`) destination and inventory validation, the
//! configured/eligible/active lifecycle sets, and top-level
//! resolution across legacy and profile-aware discovery.
//!
//! Conventions follow the earlier ports: the shell's globals become
//! [`State`], environment-derived inputs arrive explicitly via
//! [`Inputs`], and stderr text (warnings, discovery errors) is
//! collected for engine callers to reproduce — the library never
//! prints. Exit codes mirror the shell: [`Error::code`] is 1 for a
//! filtered descriptor and 2 for usage errors and invalid
//! descriptors.
//!
//! Two deliberate shell quirks are replicated. Descriptor safety
//! shares the `od -An -t u1 | awk` repeat-marker fail-closed rule
//! documented on `profiles::file_safe`: any two consecutive
//! identical 16-byte chunks reject, exactly like the shell's
//! accidental strictness. Filename identity strips trailing
//! newlines, matching command-substitution capture.
//!
//! One documented boundary: descriptor values cross from bytes to
//! `String` via lossy conversion (the `profiles` precedent), so a
//! non-UTF8 descriptor compares lossy where the shell compares raw
//! bytes. The shell suite carries no such fixtures; realistic
//! descriptors are UTF-8.

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// Overlay failure, mirroring the shell exit codes and stderr
/// shapes. `Display` renders the exact stderr line the shell
/// prints on that path — empty means the shell prints nothing
/// and engine callers must not print either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity or mode, silent (shell exit 2).
    Usage,
    /// Valid descriptor filtered from this host, silent (shell
    /// exit 1). Surfaces only from parsing; discovery consumes
    /// it.
    Filtered,
    /// Invalid descriptor: `  warning: {message}` (shell exit 2).
    Warning(String),
    /// Announced discovery error: `dot: overlay: {message}`
    /// unless suppressed under `DOT_OVERLAY_DISCOVERY_SILENT`
    /// (shell exit 2).
    Announced(String),
    /// Verbatim failure line (shell exit 1): the preflight
    /// warning, or a profiles-domain message whose
    /// `dot: profile: ` prefix the caller reproduces.
    Failed(String),
    /// Silent code-1 propagation: an unresolvable XDG base in
    /// the profiles-load step, which the shell propagates
    /// without printing.
    Unresolvable,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(&self) -> i32 {
        match self {
            Error::Usage | Error::Warning(_) | Error::Announced(_) => 2,
            Error::Filtered | Error::Failed(_) | Error::Unresolvable => 1,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage | Error::Filtered | Error::Unresolvable => Ok(()),
            Error::Warning(message) => write!(formatter, "  warning: {message}"),
            Error::Announced(message) => write!(formatter, "dot: overlay: {message}"),
            Error::Failed(message) => write!(formatter, "{message}"),
        }
    }
}

/// Parse outcome for one descriptor: eligible with its
/// `name|path|url|conf|optional|sync` record, or filtered.
/// Invalid descriptors surface as [`Error::Warning`].
pub type ParseOutcome = Result<Option<String>, Error>;

/// Discovery state (the shell `OVERLAYS` / `CONFIGURED_*` /
/// `ELIGIBLE_*` / `ACTIVE_*` / `DOT_OVERLAY_*` /
/// `SELECTED_OVERLAY_NAMES` / `PHASE_ONE_*` globals).
#[derive(Debug, Clone, Default)]
pub struct State {
    /// `OVERLAYS`: the published record set.
    pub overlays: Vec<String>,
    /// `CONFIGURED_OVERLAY_NAMES`.
    pub configured: Vec<String>,
    /// `ELIGIBLE_OVERLAY_NAMES`.
    pub eligible_names: Vec<String>,
    /// `ELIGIBLE_OVERLAYS`.
    pub eligible: Vec<String>,
    /// `ACTIVE_OVERLAY_NAMES`.
    pub active_names: Vec<String>,
    /// `ACTIVE_OVERLAYS`.
    pub active: Vec<String>,
    /// `DOT_OVERLAY_LIFECYCLE` (`name|state|file`).
    pub lifecycle: Vec<String>,
    /// `SELECTED_OVERLAY_NAMES` (appended by legacy discovery;
    /// assigned verbatim from the inputs by [`discover`], like
    /// `_dot_profiles_load` keeps the shell array).
    pub selected: Vec<String>,
    /// `DOT_OVERLAY_DISCOVERY_ERROR`.
    pub discovery_error: Option<String>,
    /// Whether the discovery error is announced on stderr (false
    /// under `DOT_OVERLAY_DISCOVERY_SILENT=1`).
    pub discovery_announced: bool,
    /// Collected `  warning: ...` stderr lines, in order.
    pub warnings: Vec<String>,
    /// `PHASE_ONE_SELECTED_OVERLAY_NAMES`.
    pub phase_one_selected: Vec<String>,
    /// `PHASE_ONE_ELIGIBLE_OVERLAYS`.
    pub phase_one_eligible: Vec<String>,
    /// `PHASE_ONE_ACTIVE_OVERLAYS`.
    pub phase_one_active: Vec<String>,
}

/// Environment-derived discovery inputs. Strictness is forced
/// per branch by discovery itself (legacy permissive, selected
/// strict), never taken from the environment.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// `$HOME` (tilde expansion, git-descriptor home paths).
    pub home: String,
    /// `$XDG_CONFIG_HOME` (empty means the default).
    pub xdg_config: String,
    /// `DOT_OVERLAY_DISCOVERY_SILENT=1`.
    pub discovery_silent: bool,
    /// `DOT_PROFILES_PRESENT=1`.
    pub profiles_present: bool,
    /// `SELECTED_OVERLAY_NAMES` (profile-aware discovery).
    pub selected: Vec<String>,
    /// Detected platform, or `None` when detection failed.
    pub platform: Option<String>,
    /// Termux dual `linux`+`android` identity.
    pub termux: bool,
    /// Detected short hostname, or `None` when detection failed.
    pub host: Option<String>,
    /// Current euid for ownership-gated checks.
    pub euid: u32,
}

/// `_overlay_name`: identity from a descriptor filename —
/// `10-work.conf` becomes `work`. `sync=none` also strips a
/// `.local` suffix, and a leading `NN-` run is removed.
pub fn overlay_name(file: &str, sync: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    let base = base.strip_suffix(".conf").unwrap_or(base);
    let base = if sync == "none" {
        base.strip_suffix(".local").unwrap_or(base)
    } else {
        base
    };
    strip_numeric_prefix(base).to_string()
}

/// `_overlay_profile_name`: profile-aware spelling — `.local` is
/// always normalized before the numeric prefix is removed.
pub fn overlay_profile_name(file: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    let base = base.strip_suffix(".conf").unwrap_or(base);
    let base = base.strip_suffix(".local").unwrap_or(base);
    strip_numeric_prefix(base).to_string()
}

/// Strip one leading `NN-` run (`^[0-9]+-(.+)$`, bash `=~`
/// semantics: `$` also anchors before a single trailing newline,
/// which stays out of the capture; anything else newline-shaped
/// fails the match and the base is kept whole, newlines included).
fn strip_numeric_prefix(base: &str) -> &str {
    match base.find('-') {
        Some(dash) if dash > 0 && base[..dash].bytes().all(|b| b.is_ascii_digit()) => {
            let rest = &base[dash + 1..];
            if rest.is_empty() {
                return base;
            }
            if let Some(stripped) = rest.strip_suffix('\n') {
                if stripped.is_empty() || stripped.contains('\n') {
                    return base;
                }
                return stripped;
            }
            if rest.contains('\n') {
                return base;
            }
            rest
        }
        _ => base,
    }
}

/// `_overlay_descriptor_value_safe`: the shared
/// `_dot_overlay_field_safe` gate — no pipe, C0 control, or DEL —
/// plus its `od` repeat-marker fail-closed quirk (see
/// [`descriptor_file_safe`]), since values travel through the same
/// `od -An -t u1 | awk` scan.
pub fn descriptor_value_safe(value: &[u8]) -> bool {
    crate::overlay_context::field_safe(value)
}

/// `_overlay_relative_path_safe`: a representable relative path —
/// non-empty, never absolute, with no empty, `.`, or `..` segments.
pub fn relative_path_safe(rel: &[u8]) -> bool {
    if !descriptor_value_safe(rel) || rel.is_empty() {
        return false;
    }
    let mut segments = rel.split(|byte| *byte == b'/');
    segments.all(|segment| !segment.is_empty() && segment != b"." && segment != b"..")
}

/// `_overlay_conf_invalid`: an invalid-descriptor diagnostic —
/// `REPLY` becomes `invalid overlay descriptor {file}: {detail}`
/// with the `  warning: ...` line on stderr (exit 2). The library
/// never prints: returns [`Error::Warning`] carrying the message,
/// which [`Error::code`] maps to 2 and [`Error`] `Display` renders
/// as the stderr line for engine callers to reproduce.
pub fn conf_invalid(file: &str, detail: &str) -> Error {
    Error::Warning(format!("invalid overlay descriptor {file}: {detail}"))
}

/// Mirror of the shell `od -An -t u1 | awk` descriptor-file scan:
/// regular non-symlink file, at most 65536 bytes, newline the only
/// accepted control byte, DEL rejected — and, like the shell,
/// fail-closed on `od`'s own `*` repeat marker for two consecutive
/// identical 16-byte chunks.
pub fn descriptor_file_safe(path: &Path) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    if bytes.len() > 65536 {
        return false;
    }
    if bytes
        .iter()
        .any(|byte| *byte != b'\n' && (*byte < 32 || *byte == 127))
    {
        return false;
    }
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset..offset + 16] == bytes[offset + 16..offset + 32] {
            return false;
        }
        offset += 16;
    }
    true
}

/// `_overlay_conf_dir`: the `dot/overlays.d` configuration
/// directory. Resolution failure reads as an absent directory —
/// the shell's `[[ -d ... ]] || return 0` swallows it.
pub fn conf_dir(xdg_config: &str, home: &str) -> Option<String> {
    crate::xdg::path(crate::xdg::Kind::Config, "dot/overlays.d", xdg_config, home).ok()
}

/// Live matching inputs for one parse: detected platform (plus
/// the Termux dual identity) and short hostname. `None` on either
/// side means detection failed, which filters like the shell.
#[derive(Debug, Clone)]
pub struct MatchInputs {
    /// Detected platform.
    pub platform: Option<String>,
    /// Termux `PREFIX` dual identity.
    pub termux: bool,
    /// Detected short hostname.
    pub host: Option<String>,
}

/// `_parse_overlay_conf`: parse one descriptor into its
/// `name|path|url|conf|optional|sync` record. `Ok(Some)` is
/// eligible, `Ok(None)` is valid-but-filtered (exit 1), and
/// `Err(Error::Warning)` carries the `invalid overlay descriptor
/// ...` message (exit 2). Only non-fatal unknown-keyेष्ठ warnings
/// accumulate in `warnings`; the fatal line travels in the error
/// so engine callers print it exactly once. Unreadable files read
/// as empty, matching the shell's failed redirect.
pub fn parse_conf(
    file: &Path,
    file_text: &str,
    strict: bool,
    home: &str,
    matches: &MatchInputs,
    warnings: &mut Vec<String>,
) -> ParseOutcome {
    let invalid =
        |detail: &str| Error::Warning(format!("invalid overlay descriptor {file_text}: {detail}"));
    if strict && !descriptor_file_safe(file) {
        return Err(invalid("unsafe descriptor file"));
    }
    let bytes = std::fs::read(file).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    let mut url = String::new();
    let mut path = String::new();
    let mut platforms = String::new();
    let mut hosts = String::new();
    let mut optional = String::new();
    let mut sync = "git".to_string();
    let mut seen = [0u32; 6];
    let mut strict_error: Option<String> = None;
    let mut unknown_lines: Vec<String> = Vec::new();
    // url, path, platforms, hosts, optional, sync: first key
    // whose prefix matches wins (the keys are pairwise
    // prefix-distinct, so order is irrelevant).
    for line in lines {
        let slot = [
            "url=",
            "path=",
            "platforms=",
            "hosts=",
            "optional=",
            "sync=",
        ]
        .iter()
        .enumerate()
        .find_map(|(index, key)| line.strip_prefix(key).map(|value| (index, value)));
        match slot {
            Some((index, value)) => {
                if seen[index] > 0 && strict_error.is_none() {
                    let key = ["url", "path", "platforms", "hosts", "optional", "sync"][index];
                    strict_error = Some(format!("duplicate {key}"));
                }
                seen[index] += 1;
                match index {
                    0 => url = value.to_string(),
                    1 => path = value.to_string(),
                    2 => platforms = value.to_string(),
                    3 => hosts = value.to_string(),
                    4 => optional = value.to_string(),
                    _ => sync = value.to_string(),
                }
            }
            None => {
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                unknown_lines.push(line.to_string());
                if strict_error.is_none() {
                    let key = line.split('=').next().unwrap_or(line);
                    strict_error = Some(format!("unknown key: {key}"));
                }
            }
        }
    }
    if seen[5] > 1 {
        return Err(invalid("duplicate sync"));
    }
    if sync != "git" && sync != "none" {
        return Err(invalid(&format!("unknown sync value: {sync}")));
    }
    let name = overlay_name(file_text, &sync);
    if sync == "none" || seen[1] > 0 {
        if let Some(detail) = strict_error {
            return Err(invalid(&detail));
        }
        if sync != "none" {
            return Err(invalid("path requires sync=none"));
        }
        if seen[1] != 1 || path.is_empty() {
            return Err(invalid("missing path"));
        }
        if seen[0] != 0 {
            return Err(invalid("url is not valid with sync=none"));
        }
        if seen[4] != 0 {
            return Err(invalid("optional is not valid with sync=none"));
        }
        if !descriptor_value_safe(name.as_bytes()) || !descriptor_value_safe(file_text.as_bytes()) {
            return Err(invalid("unrepresentable name or path"));
        }
        if let Some(rest) = path.strip_prefix("~/") {
            path = format!("{home}/{rest}");
        } else if !path.starts_with('/') {
            return Err(invalid("path must be absolute or begin with ~/"));
        }
        if !descriptor_value_safe(path.as_bytes()) {
            return Err(invalid("unrepresentable path"));
        }
        if path == "/"
            || path.ends_with('/')
            || path.contains("//")
            || path.contains("/./")
            || path.ends_with("/.")
            || path.contains("/../")
            || path.ends_with("/..")
        {
            return Err(invalid("path must be normalized"));
        }
        optional = "false".to_string();
    } else {
        if strict {
            if let Some(detail) = strict_error {
                return Err(invalid(&detail));
            }
        }
        for line in &unknown_lines {
            warnings.push(format!("  warning: unknown key in {file_text}: {line}"));
        }
        if url.is_empty() {
            if strict {
                return Err(invalid("missing url"));
            }
            return Ok(None);
        }
        if !descriptor_value_safe(url.as_bytes()) {
            return Err(invalid("unrepresentable url"));
        }
        match optional.as_str() {
            "" | "true" | "false" => {}
            _ => {
                if strict {
                    return Err(invalid(&format!("unknown optional value: {optional}")));
                }
                warnings.push(format!(
                    "  warning: unknown optional value in {file_text}: {optional}"
                ));
            }
        }
    }
    if !platforms.is_empty() {
        let current = matches.platform.clone().unwrap_or_default();
        let matched = matches.platform.is_some()
            && crate::platform::platform_matches(Some(&platforms), &current, matches.termux)
                .unwrap_or(false);
        if !matched {
            return Ok(None);
        }
    }
    if !hosts.is_empty() {
        let current = matches.host.clone().unwrap_or_default();
        let matched = matches.host.is_some()
            && crate::platform::host_matches(Some(&hosts), &current).unwrap_or(false);
        if !matched {
            return Ok(None);
        }
    }
    if sync == "git" {
        path = format!("{home}/.dotfiles-{name}");
    }
    let optional = if optional.is_empty() {
        "false"
    } else {
        &optional
    };
    Ok(Some(format!(
        "{name}|{path}|{url}|{file_text}|{optional}|{sync}"
    )))
}

/// `_overlay_is_worktree`: a directory whose `.git` entry exists
/// and whose physical directory is the `git` top level.
pub fn is_worktree(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let dot_git = path.join(".git");
    if !(dot_git.is_dir() || dot_git.is_file()) {
        return false;
    }
    let checkout = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => return false,
    };
    let top = match std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
    {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
        _ => return false,
    };
    match std::fs::canonicalize(&top) {
        Ok(canonical) => canonical == checkout,
        Err(_) => false,
    }
}

/// `_overlay_effective_url`: resolve local relative URLs from
/// `$HOME` before comparison, keeping scp/colon, scheme,
/// absolute, and Windows-drive spellings as configured.
pub fn effective_url(url: &str, home: &str) -> String {
    if url == "~" {
        return home.to_string();
    }
    if let Some(rest) = url.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if url.starts_with('/')
        || url.contains(':')
        || (url.len() >= 3
            && url.as_bytes()[0].is_ascii_alphabetic()
            && url.as_bytes()[1] == b':'
            && (url.as_bytes()[2] == b'/' || url.as_bytes()[2] == b'\\'))
    {
        return url.to_string();
    }
    format!("{home}/{url}")
}

/// `_overlay_origin_matches`: the single authoritative origin URL
/// against the configured spelling. Returns the recorded URL on a
/// match, or the `<missing>` / `<multiple origin URLs>`
/// diagnostic the shell stores in `REPLY`.
pub fn origin_matches(path: &Path, expected: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("config")
        .arg("--get-all")
        .arg("remote.origin.url")
        .output();
    let mut urls: Vec<String> = Vec::new();
    if let Ok(output) = output {
        let text = String::from_utf8_lossy(&output.stdout);
        // Like `mapfile -t`: empty input reads zero lines, while a
        // blank line still reads one empty entry.
        if !text.is_empty() {
            let mut lines: Vec<&str> = text.split('\n').collect();
            if text.ends_with('\n') {
                lines.pop();
            }
            urls = lines.iter().map(|line| line.to_string()).collect();
        }
    }
    match urls.len() {
        0 => Err("<missing>".to_string()),
        1 => {
            if urls[0] == expected {
                Ok(urls[0].clone())
            } else {
                Err(urls[0].clone())
            }
        }
        _ => Err("<multiple origin URLs>".to_string()),
    }
}

/// `_overlay_checkout_matches` (from `repos/config.sh`, the small
/// overlay-scoped block discovery depends on): the worktree check
/// plus origin comparison. `Ok` carries the recorded URL;
/// `Err` carries the `REPLY` diagnostic.
pub fn checkout_matches(path: &Path, url: &str, home: &str) -> Result<String, String> {
    if !is_worktree(path) {
        return Err("<not a Git worktree>".to_string());
    }
    let expected = effective_url(url, home);
    origin_matches(path, &expected)
}

/// `_overlay_physical_dir_candidate`: resolve the existing
/// ancestor physically, then append the still-missing suffix
/// lexically. Returns the `REPLY` value.
pub fn physical_dir_candidate(candidate: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt as _;
    let raw = candidate.as_os_str().as_bytes();
    if !raw.starts_with(b"/") {
        return None;
    }
    // A trailing slash leaves `${candidate##*/}` empty, which the
    // shell rejects once the path is not an existing directory.
    if raw.len() > 1 && raw.ends_with(b"/") && !candidate.is_dir() {
        return None;
    }
    let mut candidate = candidate.to_path_buf();
    let mut suffix = String::new();
    loop {
        if candidate.is_dir() {
            break;
        }
        if candidate == Path::new("/") {
            return None;
        }
        let part = candidate
            .file_name()
            .map(|name| name.as_bytes().to_vec())
            .unwrap_or_default();
        if part.is_empty() {
            return None;
        }
        suffix = format!("/{}{suffix}", String::from_utf8_lossy(&part));
        let parent = candidate
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        if parent.as_os_str().is_empty() {
            candidate = Path::new("/").to_path_buf();
        } else if parent == candidate {
            return None;
        } else {
            candidate = parent;
        }
    }
    let physical = std::fs::canonicalize(&candidate).ok()?;
    let physical = physical.to_string_lossy().into_owned();
    if physical == "/" {
        Some(format!("/{}", suffix.strip_prefix('/').unwrap_or(&suffix)))
    } else {
        Some(format!("{physical}{suffix}"))
    }
}

/// Whether a file opens for reading — the `[[ -r ... ]]` probe as
/// the kernel answers it (root included), without libc bindings.
fn readable_file(path: &Path) -> bool {
    path.is_file() && std::fs::File::open(path).is_ok()
}

/// Whether a directory lists and searches — the combined
/// `[[ -r ... && -x ... ]]` probe as the kernel answers it
/// (verified: listing alone accepts read-only directories, so the
/// search bit is probed by resolving `.` beneath).
fn searchable_dir(path: &Path) -> bool {
    path.is_dir() && std::fs::read_dir(path).is_ok() && std::fs::metadata(path.join(".")).is_ok()
}

/// `_overlay_local_destination_safe`: a relative inventory path
/// must resolve outside the overlay's own source tree.
/// `source_root_real` skips the `home` resolution when the caller
/// already canonicalized it. `Ok` clears `REPLY`; `Err` carries
/// the diagnostic.
pub fn local_destination_safe(
    path: &str,
    rel: &str,
    source_root_real: Option<&str>,
    home: &str,
) -> Result<(), String> {
    let overlay_home = format!("{path}/home");
    if !relative_path_safe(rel.as_bytes()) {
        return Err(format!(
            "{overlay_home} (unrepresentable destination: {rel})"
        ));
    }
    let source_root_real = match source_root_real {
        Some(root) => root.to_string(),
        None => match std::fs::canonicalize(&overlay_home) {
            Ok(canonical) => canonical.to_string_lossy().into_owned(),
            Err(_) => return Err(overlay_home),
        },
    };
    let source_prefix = if source_root_real == "/" {
        "/".to_string()
    } else {
        format!("{}/", source_root_real.trim_end_matches('/'))
    };
    let dst_parent = match rel.rfind('/') {
        Some(index) => format!("{home}/{}", &rel[..index]),
        None => home.to_string(),
    };
    let destination_real = match physical_dir_candidate(Path::new(&dst_parent)) {
        Some(resolved) => resolved,
        None => {
            return Err(format!(
                "{overlay_home} (cannot resolve destination: {rel})"
            ));
        }
    };
    let candidate_prefix = if destination_real == "/" {
        "/".to_string()
    } else {
        format!("{}/", destination_real.trim_end_matches('/'))
    };
    if candidate_prefix.starts_with(&source_prefix) {
        return Err(format!(
            "{overlay_home} (destination resolves inside source: {rel})"
        ));
    }
    Ok(())
}

/// Split an `OVERLAYS` record into `(name, path, sync)` with the
/// shell's `read` remainder rule (a surplus `|` stays in `sync`).
fn record_fields(record: &str) -> (String, String, String) {
    let mut fields = record.split('|');
    let name = fields.next().unwrap_or("").to_string();
    let path = fields.next().unwrap_or("").to_string();
    let _ = fields.next();
    let _ = fields.next();
    let _ = fields.next();
    let sync = fields.collect::<Vec<_>>().join("|");
    (name, path, sync)
}

/// `_overlay_destination_outside_local_sources`: no writer may
/// reach an active filesystem overlay's source through a symlinked
/// destination parent. Checks every `sync=none` record, not just
/// the writer's own source.
pub fn destination_outside_local_sources(
    rel: &str,
    overlays: &[String],
    home: &str,
) -> Result<(), String> {
    for entry in overlays {
        let (_, path, sync) = record_fields(entry);
        let sync = if sync.is_empty() { "git" } else { &sync };
        if sync != "none" {
            continue;
        }
        local_destination_safe(&path, rel, None, home)?;
    }
    Ok(())
}

/// `_overlay_local_source_entry_validate`: revalidate one
/// inventory entry by source path, relative path, and the
/// pre-resolved source root. `Err` carries the `REPLY`
/// diagnostic.
pub fn source_entry_validate(
    path: &str,
    src: &Path,
    rel: &str,
    source_root_real: &str,
    overlays: &[String],
    home: &str,
) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;
    let overlay_home = format!("{path}/home");
    let expected = format!("{overlay_home}/{rel}");
    if src.as_os_str().as_bytes() != expected.as_bytes() || !relative_path_safe(rel.as_bytes()) {
        return Err(format!("{overlay_home} (unrepresentable entry)"));
    }
    let link = std::fs::symlink_metadata(src)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    if link {
        let target = std::fs::metadata(src);
        let target = match target {
            Ok(target) => target,
            Err(_) => return Err(format!("{overlay_home} (dangling symlink: {rel})")),
        };
        if target.is_dir() {
            if !searchable_dir(src) {
                return Err(format!("{overlay_home} (unreadable symlink target: {rel})"));
            }
        } else if !target.is_file() || std::fs::File::open(src).is_err() {
            return Err(format!("{overlay_home} (unreadable symlink target: {rel})"));
        }
    } else if !readable_file(src) {
        return Err(format!("{overlay_home} (unreadable entry: {rel})"));
    }
    local_destination_safe(path, rel, Some(source_root_real), home)?;
    destination_outside_local_sources(rel, overlays, home)?;
    Ok(())
}

/// `_overlay_local_source_validate`: validate one filesystem
/// overlay's readable inventory. `Err` carries the `REPLY`
/// diagnostic. The shell stages `find -print0` output through a
/// scratch file; the walk here is in memory, which is
/// unobservable apart from speed.
pub fn source_validate(path: &str, overlays: &[String], home: &str) -> Result<(), String> {
    let overlay_home = format!("{path}/home");
    if !searchable_dir(Path::new(&overlay_home)) {
        return Err(overlay_home);
    }
    let source_root_real = match std::fs::canonicalize(&overlay_home) {
        Ok(canonical) => canonical.to_string_lossy().into_owned(),
        Err(_) => return Err(overlay_home),
    };
    let mut stack = vec![PathBuf::from(&overlay_home)];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return Err(format!("could not read inventory for {overlay_home}")),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return Err(format!("could not read inventory for {overlay_home}")),
            };
            let src = entry.path();
            let rel = match src
                .to_string_lossy()
                .strip_prefix(&format!("{overlay_home}/"))
            {
                Some(rel) => rel.to_string(),
                None => {
                    return Err(format!("{overlay_home} (unrepresentable entry)"));
                }
            };
            // A file vanishing mid-walk fails its entry check
            // below, exactly like the shell's `find`-then-stat
            // sequence — only traversal failures are inventory
            // errors.
            let kind = std::fs::symlink_metadata(&src).map(|meta| meta.file_type());
            let kind = match kind {
                Ok(kind) => kind,
                Err(_) => {
                    source_entry_validate(path, &src, &rel, &source_root_real, overlays, home)?;
                    continue;
                }
            };
            if kind.is_symlink() || kind.is_file() {
                // `*.~[0-9]*~` editor backups never ship: a
                // `.~` run whose next byte is a digit, with a
                // trailing `~`.
                if let Some(base) = rel.rsplit('/').next() {
                    if base.ends_with('~')
                        && base.as_bytes().windows(3).any(|window| {
                            window[0] == b'.' && window[1] == b'~' && window[2].is_ascii_digit()
                        })
                    {
                        continue;
                    }
                }
                source_entry_validate(path, &src, &rel, &source_root_real, overlays, home)?;
            } else if kind.is_dir() {
                stack.push(src);
            }
        }
    }
    Ok(())
}

/// `_preflight_local_overlays`: validate every active filesystem
/// overlay. `Err` carries the full
/// `  warning: {name} overlay source is unavailable: ...` line.
pub fn preflight(state: &mut State, home: &str) -> Result<(), String> {
    for entry in state.overlays.clone() {
        let (name, path, sync) = record_fields(&entry);
        if sync.is_empty() || sync == "git" {
            continue;
        }
        if sync != "none" {
            continue;
        }
        if let Err(diagnostic) = source_validate(&path, &state.overlays, home) {
            let warning = format!("  warning: {name} overlay source is unavailable: {diagnostic}");
            state.warnings.push(warning.clone());
            return Err(warning);
        }
    }
    Ok(())
}

/// `_overlay_record_active_existing`: a usable source without
/// network access or mutation.
pub fn record_active_existing(record: &str, home: &str) -> bool {
    let (_, _path, sync) = record_fields(record);
    if sync.is_empty() || sync == "git" {
        let mut fields = record.split('|');
        let _ = fields.next();
        let path = fields.next().unwrap_or("").to_string();
        let url = fields.next().unwrap_or("").to_string();
        return checkout_matches(Path::new(&path), &url, home).is_ok();
    }
    if sync == "none" {
        let mut fields = record.split('|');
        let _ = fields.next();
        let path = fields.next().unwrap_or("").to_string();
        return source_validate(&path, &[], home).is_ok();
    }
    false
}

/// `_dot_overlay_use_set`: publish exactly one set as `OVERLAYS`.
pub fn use_set(state: &mut State, kind: &str) -> Result<(), Error> {
    match kind {
        "eligible" => state.overlays = state.eligible.clone(),
        "active" => state.overlays = state.active.clone(),
        _ => return Err(Error::Usage),
    }
    Ok(())
}

/// Sorted `*.conf` descriptor files, like the shell glob.
fn descriptor_files(conf_dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(conf_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            // Shell `*.conf` glob semantics, byte-exact: the
            // basename must end in `.conf`, and glob `*` never
            // matches a leading dot (so `.conf` and `.x.conf`
            // are skipped even though the suffix matches).
            path.file_name().is_some_and(|base| {
                let base = base.as_bytes();
                !base.starts_with(b".") && base.ends_with(b".conf")
            }) && match std::fs::symlink_metadata(path) {
                Ok(meta) => {
                    meta.file_type().is_symlink()
                        || meta.file_type().is_file() && !meta.file_type().is_symlink()
                }
                Err(_) => false,
            }
        })
        .collect();
    files.sort();
    files
}

/// Whether a descriptor path passes the `[[ -f || -L ]]` gate.
fn descriptor_present(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_symlink() || meta.is_file(),
        Err(_) => false,
    }
}

/// Lifecycle state for one selected descriptor that parsed but
/// has no usable source.
fn unavailable_state(record: &str) -> &str {
    let optional = record.split('|').nth(4).unwrap_or("");
    if optional == "true" {
        "selected-optional-unavailable"
    } else {
        "selected-unavailable"
    }
}

/// `_discover_overlays`: reset and rediscover from the
/// configuration directory. A missing directory is a clean pass;
/// profile-aware discovery additionally needs the selected names
/// from the profiles state.
pub fn discover(
    state: &mut State,
    conf_dir: &Path,
    conf_text: &str,
    inputs: &Inputs,
    matches: &MatchInputs,
) -> Result<(), Error> {
    *state = State::default();
    if !conf_dir.is_dir() {
        return Ok(());
    }
    if !inputs.profiles_present {
        return discover_legacy(state, conf_dir, conf_text, inputs, matches);
    }
    // The selection echoes back verbatim on every path, including
    // the error returns below (the shell threads
    // `SELECTED_OVERLAY_NAMES` through untouched).
    state.selected = inputs.selected.clone();
    let mut descriptors: HashMap<String, String> = HashMap::new();
    for file in descriptor_files(conf_dir) {
        if !descriptor_present(&file) {
            continue;
        }
        let text = file.to_string_lossy().into_owned();
        let name = overlay_profile_name(&text);
        if !crate::profiles::identifier_valid(name.as_bytes()) {
            let base = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            return discover_error(
                state,
                inputs.discovery_silent,
                format!("invalid overlay descriptor filename: {base}"),
            );
        }
        if let Some(first) = descriptors.get(&name) {
            return discover_error(
                state,
                inputs.discovery_silent,
                format!("duplicate overlay name '{name}' in {first} and {text}"),
            );
        }
        descriptors.insert(name.clone(), text);
        state.configured.push(name);
    }
    let selected: HashSet<String> = inputs.selected.iter().cloned().collect();
    for name in &inputs.selected {
        if !descriptors.contains_key(name) {
            return discover_error(
                state,
                inputs.discovery_silent,
                format!("selected overlay has no descriptor: {name}"),
            );
        }
    }
    for name in state.configured.clone() {
        let file = descriptors[&name].clone();
        if !selected.contains(&name) {
            state.lifecycle.push(format!("{name}|not-selected|{file}"));
            continue;
        }
        match parse_conf(
            Path::new(&file),
            &file,
            true,
            &inputs.home,
            matches,
            &mut state.warnings,
        ) {
            Ok(Some(record)) => {
                state.eligible_names.push(name.clone());
                state.eligible.push(record.clone());
                if record_active_existing(&record, &inputs.home) {
                    state.active_names.push(name.clone());
                    state.active.push(record);
                    state.lifecycle.push(format!("{name}|active|{file}"));
                } else {
                    let lifecycle = unavailable_state(&record);
                    state.lifecycle.push(format!("{name}|{lifecycle}|{file}"));
                }
            }
            Ok(None) => {
                state
                    .lifecycle
                    .push(format!("{name}|selected-ineligible|{file}"));
            }
            Err(error) => {
                // Like the legacy loop: the warning text becomes
                // the discovery error while the status passes
                // through unchanged — a warning stays a
                // `  warning:` line, never a `dot: overlay:`
                // announcement (so `discovery_announced` stays
                // false); a filter lands in the lifecycle.
                let message = match error {
                    Error::Warning(message) => message,
                    Error::Filtered => {
                        state
                            .lifecycle
                            .push(format!("{name}|selected-ineligible|{file}"));
                        continue;
                    }
                    other => return Err(other),
                };
                state.eligible.clear();
                state.active.clear();
                state.discovery_error = Some(if message.is_empty() {
                    format!("invalid selected overlay descriptor: {file}")
                } else {
                    message.clone()
                });
                return Err(Error::Warning(message));
            }
        }
    }
    use_set(state, "eligible")
}

/// Record a discovery error: set `DOT_OVERLAY_DISCOVERY_ERROR`
/// and announce unless silent. Always fails like the shell.
fn discover_error(state: &mut State, silent: bool, message: String) -> Result<(), Error> {
    state.discovery_error = Some(message);
    state.discovery_announced = !silent;
    Err(Error::Announced(
        state.discovery_error.clone().unwrap_or_default(),
    ))
}

/// Legacy discovery (profiles absent): every descriptor is
/// implicitly selected, duplicates warn and skip.
fn discover_legacy(
    state: &mut State,
    conf_dir: &Path,
    conf_text: &str,
    inputs: &Inputs,
    matches: &MatchInputs,
) -> Result<(), Error> {
    let _ = conf_text;
    let mut seen: HashSet<String> = HashSet::new();
    for file in descriptor_files(conf_dir) {
        if !descriptor_present(&file) {
            continue;
        }
        let text = file.to_string_lossy().into_owned();
        match parse_conf(
            Path::new(&file),
            &text,
            false,
            &inputs.home,
            matches,
            &mut state.warnings,
        ) {
            Ok(Some(record)) => {
                let name = record.split('|').next().unwrap_or("").to_string();
                if !seen.insert(name.clone()) {
                    state.warnings.push(format!(
                        "  warning: duplicate overlay name '{name}' in {text} — skipping"
                    ));
                    continue;
                }
                state.configured.push(name.clone());
                state.selected.push(name.clone());
                state.eligible_names.push(name.clone());
                state.eligible.push(record.clone());
                if record_active_existing(&record, &inputs.home) {
                    state.active_names.push(name.clone());
                    state.active.push(record);
                    state.lifecycle.push(format!("{name}|active|{text}"));
                } else {
                    let lifecycle = unavailable_state(&record);
                    state.lifecycle.push(format!("{name}|{lifecycle}|{text}"));
                }
            }
            Ok(None) => {}
            Err(error) => {
                // The shell records the warning text as the
                // discovery error and returns the parse status
                // unchanged: a warning stays a `  warning:` line
                // (never a `dot: overlay:` announcement), a filter
                // skips, anything else propagates as-is.
                let message = match error {
                    Error::Warning(message) => message,
                    Error::Filtered => continue,
                    other => return Err(other),
                };
                state.discovery_error = Some(if message.is_empty() {
                    format!("invalid overlay descriptor: {text}")
                } else {
                    message.clone()
                });
                state.eligible.clear();
                state.active.clear();
                return Err(Error::Warning(message));
            }
        }
    }
    use_set(state, "eligible")
}

/// Environment-derived resolution inputs for [`resolve`].
#[derive(Debug, Clone)]
pub struct ResolveInputs {
    /// `$HOME` (tilde expansion, git-descriptor home paths).
    pub home: String,
    /// `$XDG_CONFIG_HOME` (empty means the default).
    pub xdg_config: String,
    /// `DOT_OVERLAY_DISCOVERY_SILENT=1`.
    pub discovery_silent: bool,
    /// `DOT_DEFAULT_PROFILE` (`None` means unset, defaulting to
    /// `base` at load time).
    pub default_profile: Option<String>,
    /// Login name for selector resolution (`None` reproduces a
    /// failed `id -un` determination).
    pub user: Option<String>,
    /// Short hostname for selector resolution and descriptor
    /// matching (`None` filters gated descriptors and reproduces a
    /// failed hostname determination).
    pub host: Option<String>,
    /// Detected platform for descriptor matching (`None` filters,
    /// like failed detection on the shell side).
    pub platform: Option<String>,
    /// Termux dual `linux`+`android` identity for matching.
    pub termux: bool,
    /// Current euid for ownership-gated checks.
    pub euid: u32,
}

/// `_dot_resolve_overlays`: top-level overlay resolution across
/// legacy and profile-aware discovery. `mode` is one of
/// `converge`, `inspect`, or `fetch` — empty takes the shell
/// `${1:-inspect}` default; anything else is [`Error::Usage`]
/// (silent, exit 2).
///
/// Legacy clients (no `profiles.d`) discover once, then publish
/// the eligible set under converge but the active set otherwise.
/// Profile clients select `base`, discover, snapshot the phase-one
/// sets, publish `active`, resolve selectors from the active
/// personal directories, rediscover with the final selection, and
/// publish eligible under converge but active otherwise. The
/// second discovery resets the published sets like the shell, so
/// the phase-one snapshot is carried across it — on the shell
/// side those variables live outside the rediscovered globals.
/// Profile failures surface as [`Error::Failed`] carrying the full
/// `dot: profile: ...` line (exit 1); an unresolvable profiles
/// base stays silent as [`Error::Unresolvable`] (exit 1), like the
/// shell's `dot_xdg_path ... || return` propagation.
pub fn resolve(
    state: &mut State,
    profiles: &mut crate::profiles::State,
    mode: &str,
    inputs: &ResolveInputs,
) -> Result<(), Error> {
    let mode = if mode.is_empty() { "inspect" } else { mode };
    match mode {
        "converge" | "inspect" | "fetch" => (),
        _ => return Err(Error::Usage),
    }
    let profiles_dir = match crate::xdg::path(
        crate::xdg::Kind::Config,
        "dot/profiles.d",
        &inputs.xdg_config,
        &inputs.home,
    ) {
        Ok(dir) => dir,
        Err(crate::xdg::Error::Unresolvable) => return Err(Error::Unresolvable),
        Err(crate::xdg::Error::Usage) => return Err(Error::Usage),
    };
    profiles
        .load(
            Some(Path::new(&profiles_dir)),
            &inputs.xdg_config,
            &inputs.home,
            inputs.default_profile.as_deref(),
        )
        .map_err(|error| Error::Failed(format!("dot: profile: {}", error.message)))?;
    let matches = MatchInputs {
        platform: inputs.platform.clone(),
        termux: inputs.termux,
        host: inputs.host.clone(),
    };
    let mut discover_inputs = Inputs {
        home: inputs.home.clone(),
        xdg_config: inputs.xdg_config.clone(),
        discovery_silent: inputs.discovery_silent,
        profiles_present: profiles.present,
        selected: profiles.overlay_names.clone(),
        platform: inputs.platform.clone(),
        termux: inputs.termux,
        host: inputs.host.clone(),
        euid: inputs.euid,
    };
    let conf = conf_dir(&inputs.xdg_config, &inputs.home);
    // A missing base reads as an absent directory, which discovery
    // passes cleanly like the shell's `[[ -d ... ]] || return 0`.
    let (conf_path, conf_text) = match &conf {
        Some(dir) => (PathBuf::from(dir), dir.clone()),
        None => (PathBuf::new(), String::new()),
    };
    if !profiles.present {
        discover(state, &conf_path, &conf_text, &discover_inputs, &matches)?;
        // Legacy discovery appends every name to the shared
        // `SELECTED_OVERLAY_NAMES` global, which the profiles load
        // above cleared — mirror that publication on the profiles
        // side, where the global is declared.
        profiles.overlay_names = state.selected.clone();
        if mode != "converge" {
            use_set(state, "active")?;
        }
        return Ok(());
    }
    profiles
        .select_base()
        .map_err(|error| Error::Failed(format!("dot: profile: {}", error.message)))?;
    discover_inputs.selected = profiles.overlay_names.clone();
    discover(state, &conf_path, &conf_text, &discover_inputs, &matches)?;
    state.phase_one_selected = profiles.overlay_names.clone();
    state.phase_one_eligible = state.eligible.clone();
    state.phase_one_active = state.active.clone();
    use_set(state, "active")?;
    let user = match &inputs.user {
        Some(user) => user.clone(),
        None => {
            return Err(Error::Failed(
                "dot: profile: cannot determine current user".to_string(),
            ));
        }
    };
    let host = match &inputs.host {
        Some(host) => host.clone(),
        None => {
            return Err(Error::Failed(
                "dot: profile: cannot determine current short hostname".to_string(),
            ));
        }
    };
    let active: Vec<&str> = state.active.iter().map(String::as_str).collect();
    profiles
        .resolve_default(
            &inputs.xdg_config,
            &inputs.home,
            &active,
            &user,
            &host,
            inputs.euid,
        )
        .map_err(|error| Error::Failed(format!("dot: profile: {}", error.message)))?;
    discover_inputs.selected = profiles.overlay_names.clone();
    let phase_one_selected = state.phase_one_selected.clone();
    let phase_one_eligible = state.phase_one_eligible.clone();
    let phase_one_active = state.phase_one_active.clone();
    discover(state, &conf_path, &conf_text, &discover_inputs, &matches)?;
    state.phase_one_selected = phase_one_selected;
    state.phase_one_eligible = phase_one_eligible;
    state.phase_one_active = phase_one_active;
    if mode == "converge" {
        use_set(state, "eligible")?;
    } else {
        use_set(state, "active")?;
    }
    Ok(())
}
