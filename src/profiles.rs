//! Profile definitions and user/host selector resolution (slice 8).
//!
//! Ports `lib/dot/profile-format.sh` and the loading/selection half
//! of `lib/dot/profiles.sh`: definition parsing, include expansion
//! with cycle detection, selector parsing and matching, and default
//! resolution. The profile lifecycle ledger
//! (`profile-lifecycle.sh`) lives in [`crate::profile_lifecycle`].
//!
//! Shell globals become an explicit [`State`]; every rule that reads
//! the filesystem takes paths, and every rule that reads the
//! environment takes the already-read values, so tests inject
//! fixtures deterministically. Content is handled as bytes (like the
//! shell): only validated-ASCII names, keys, and values ever become
//! `String`.
//!
//! Two deliberate determinism choices where the shell is vague:
//! profiles validate in byte-sorted filename order (the shell glob
//! order is locale-collated; tests pin `LC_ALL=C`), and the
//! validate-every-profile pass runs in sorted name order (bash
//! associative iteration order is hash-randomized, which only
//! affects WHICH error surfaces when several profiles are broken).

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// Profile failure: the shell `_dot_profile_error` (always exit 1)
/// records `DOT_PROFILE_CONFIGURATION_ERROR` and prints
/// `dot: profile: {message}` on stderr. The message is the payload;
/// engine callers reproduce the print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Diagnostic text, byte-identical to the shell message.
    pub message: String,
}

impl Error {
    /// Shell exit code for profile failures.
    pub fn code(self) -> i32 {
        1
    }

    fn named(message: String) -> Self {
        Error { message }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// Which selector source is being read (`root` selectors may omit
/// both user and host; every other class must name at least one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorClass {
    /// Overlay-root selectors (`$root_dir`).
    Root,
    /// Machine-local selectors (additionally ownership-checked).
    Local,
    /// Personal overlay selectors.
    Personal,
}

/// A parsed selector: the shell `user|host|profile` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    /// Empty when the selector matches any user.
    pub user: String,
    /// Empty when the selector matches any host (normalized).
    pub host: String,
    /// Required profile name.
    pub profile: String,
}

/// Loaded profile state (the shell `DOT_PROFILE_*` / `SELECTED_*` /
/// `_DOT_PROFILE_*` globals).
#[derive(Debug, Clone)]
pub struct State {
    /// `DOT_PROFILES_PRESENT` (a `profiles.d` directory exists).
    pub present: bool,
    /// `SELECTED_PROFILE`.
    pub selected: String,
    /// `DOT_PROFILE_SELECTION_STATE`: `legacy`, `implicit-default`,
    /// `agreed-match`, `conflict`, or `phase-one`.
    pub selection_state: String,
    /// `DOT_PROFILE_CURRENT_USER`.
    pub current_user: String,
    /// `DOT_PROFILE_CURRENT_HOST` (normalized).
    pub current_host: String,
    /// `INCLUDED_PROFILES` (flattened, parents first).
    pub included: Vec<String>,
    /// `SELECTED_OVERLAY_NAMES` (flattened, in expansion order).
    pub overlay_names: Vec<String>,
    /// `DOT_PROFILE_SELECTOR_MATCHES` (`class:file:profile`).
    pub selector_matches: Vec<String>,
    /// `DOT_PROFILE_SELECTOR_RECORDS`
    /// (`class|file|user|host|profile|matched`).
    pub selector_records: Vec<String>,
    /// Last `DOT_PROFILE_CONFIGURATION_ERROR`.
    pub config_error: Option<String>,
    /// `DOT_DEFAULT_PROFILE` captured at load time (`base` default).
    default_profile: String,
    parents: HashMap<String, String>,
    overlays: HashMap<String, String>,
    names: HashSet<String>,
    expansion: HashMap<String, Expansion>,
    candidates: Vec<(u64, String)>,
}

/// Include-expansion marker per profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expansion {
    Visiting,
    Resolved,
}

impl Default for State {
    fn default() -> Self {
        State {
            present: false,
            selected: String::new(),
            selection_state: "legacy".to_string(),
            current_user: String::new(),
            current_host: String::new(),
            included: Vec::new(),
            overlay_names: Vec::new(),
            selector_matches: Vec::new(),
            selector_records: Vec::new(),
            config_error: None,
            default_profile: "base".to_string(),
            parents: HashMap::new(),
            overlays: HashMap::new(),
            names: HashSet::new(),
            expansion: HashMap::new(),
            candidates: Vec::new(),
        }
    }
}

impl State {
    /// Record a configuration error and fail, like
    /// `_dot_profile_error` (minus the stderr print, which engine
    /// callers reproduce as `dot: profile: {message}`).
    fn fail(&mut self, message: String) -> Error {
        self.config_error = Some(message.clone());
        Error::named(message)
    }

    /// Names known to this state, sorted (see the module docs for
    /// why the shell hash order is not reproduced).
    fn sorted_names(&self) -> Vec<&String> {
        let mut names: Vec<&String> = self.names.iter().collect();
        names.sort();
        names
    }
}

/// `_dot_profile_identifier_valid`: `^[a-z][a-z0-9-]*$` on bytes.
pub fn identifier_valid(name: &[u8]) -> bool {
    let mut bytes = name.iter();
    match bytes.next() {
        Some(first) if first.is_ascii_lowercase() => (),
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// `_dot_profile_value_safe`: no pipe, tab, newline, or CR.
pub fn value_safe(value: &[u8]) -> bool {
    !value
        .iter()
        .any(|byte| matches!(byte, b'|' | b'\t' | b'\n' | b'\r'))
}

/// `_dot_profile_file_safe`: regular non-symlink file, at most
/// 65536 bytes, no control bytes except newline (DEL rejected too).
pub fn file_safe(path: &Path) -> Result<(), Error> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| Error::named(format!("not a regular file: {}", path.display())))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::named(format!(
            "not a regular file: {}",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|_| Error::named(format!("cannot size file: {}", path.display())))?;
    if bytes.len() > 65536 {
        return Err(Error::named(format!(
            "file exceeds 65536 bytes: {}",
            path.display()
        )));
    }
    if has_control_bytes(&bytes) {
        return Err(Error::named(format!(
            "contains control bytes: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Mirror of the shell `od -An -t u1 | awk` control scan, quirk and
/// all: bytes below 32 (except newline) and DEL reject — and so does
/// any file whose dump contains `od`'s own `*` repeat marker (two
/// consecutive identical 16-byte chunks, which `awk` reads as the
/// number 0). A validator must stay fail-closed with the shell even
/// where the shell is accidentally strict (e.g. a 48-dash separator
/// run), so the quirk is replicated deliberately.
fn has_control_bytes(bytes: &[u8]) -> bool {
    if bytes
        .iter()
        .any(|byte| *byte != b'\n' && (*byte < 32 || *byte == 127))
    {
        return true;
    }
    let mut offset = 0;
    while offset + 32 <= bytes.len() {
        if bytes[offset..offset + 16] == bytes[offset + 16..offset + 32] {
            return true;
        }
        offset += 16;
    }
    false
}

/// Is `meta` owned by `euid` with no group/other permission bits?
/// Combines the shell `stat` owner/mode checks (GNU `-c` and BSD
/// `-f` spellings agree on `%u` plus an octal mode).
fn owner_private(meta: &std::fs::Metadata, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    meta.uid() == euid && meta.mode() & 0o7777 & 0o077 == 0
}

/// `_dot_profile_private_path_safe`: owned by us, never a symlink,
/// mode 600-ish (group/other bits clear).
pub fn private_path_safe(path: &Path, euid: u32) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    owner_private(&meta, euid)
}

/// `_dot_profile_owned_directory_safe`: an owned real directory.
pub fn owned_directory_safe(path: &Path, euid: u32) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    use std::os::unix::fs::MetadataExt as _;
    meta.uid() == euid
}

/// Which member list is being validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    /// `profiles=` (any identifier).
    Profile,
    /// `overlays=` (identifiers, never `dotfiles`).
    Overlay,
}

/// `_dot_profile_list_validate`: non-empty comma list of identifiers
/// (`dotfiles` is not a valid overlay member).
pub fn list_valid(value: &[u8], kind: MemberKind) -> bool {
    if value.is_empty() {
        return false;
    }
    value
        .split(|byte| *byte == b',')
        .all(|item| identifier_valid(item) && (kind != MemberKind::Overlay || item != b"dotfiles"))
}

/// Split content into lines the way `while IFS= read -r` sees them:
/// `\n`-separated with `\r` preserved, a missing final newline
/// still yielding its line, and a single trailing newline NOT
/// yielding an extra empty line.
fn content_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

/// Parse one `key=value` line: the key runs to the first `=`
/// (`${line%%=*}`), the value follows it (`${line#*=}`).
fn split_setting(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let offset = line.iter().position(|byte| *byte == b'=')?;
    Some((&line[..offset], &line[offset + 1..]))
}

/// True for a lowercase-`[a-z_]`-only non-empty key.
fn key_valid(key: &[u8]) -> bool {
    !key.is_empty()
        && key
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || *byte == b'_')
}

/// How one `key=value` line can fail the shared shape check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingFailure {
    /// Line ends with a backslash.
    Continuation,
    /// No `=` present.
    NotKeyValue,
    /// Empty or non-`[a-z_]` key.
    BadKey,
    /// Value carries `|`, tab, newline, or CR.
    UnsafeValue,
}

/// One surviving `key=value` line: number, key, value.
type Setting<'a> = (usize, &'a [u8], &'a [u8]);

/// Shared shape check for definition and selector files: comments
/// and blanks skipped; each surviving line yields its number, key,
/// and value. Callers translate [`SettingFailure`] into their own
/// `path:number wording` messages.
fn setting_lines(content: &[u8]) -> Result<Vec<Setting<'_>>, (usize, SettingFailure)> {
    let mut settings = Vec::new();
    for (index, line) in content_lines(content).iter().enumerate() {
        let number = index + 1;
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if line.ends_with(b"\\") {
            return Err((number, SettingFailure::Continuation));
        }
        let Some((key, value)) = split_setting(line) else {
            return Err((number, SettingFailure::NotKeyValue));
        };
        if !key_valid(key) {
            return Err((number, SettingFailure::BadKey));
        }
        if !value_safe(value) {
            return Err((number, SettingFailure::UnsafeValue));
        }
        settings.push((number, key, value));
    }
    Ok(settings)
}

/// Render a shared shape failure with the file-specific wording.
fn setting_message(path: &Path, number: usize, failure: SettingFailure) -> String {
    let what = match failure {
        SettingFailure::Continuation => "uses a continuation",
        SettingFailure::NotKeyValue => "is not key=value",
        SettingFailure::BadKey => "has an invalid key",
        SettingFailure::UnsafeValue => "has an unsafe value",
    };
    format!("{}:{number} {what}", path.display())
}

/// A parsed profile definition: raw member lists exactly as stored
/// in the shell maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Raw `profiles=` value (possibly empty).
    pub parents: String,
    /// Raw `overlays=` value (possibly empty).
    pub overlays: String,
}

/// `_dot_profile_parse_definition`: validate one `.conf` file.
/// Returns the member lists; the caller stores them.
pub fn parse_definition(path: &Path, bytes: &[u8]) -> Result<Definition, Error> {
    let mut seen_version = false;
    let mut seen_profiles = false;
    let mut seen_overlays = false;
    let mut saw_setting = false;
    let mut parents = String::new();
    let mut overlays = String::new();
    let settings = setting_lines(bytes)
        .map_err(|(number, failure)| Error::named(setting_message(path, number, failure)))?;
    for (_number, key, value) in settings {
        if !saw_setting && key != b"version" {
            return Err(Error::named(format!(
                "{}: version=1 must be the first setting",
                path.display()
            )));
        }
        saw_setting = true;
        match key {
            b"version" => {
                if seen_version {
                    // No line number: the shell reports dups bare.
                    return Err(Error::named(format!(
                        "{}: duplicate version",
                        path.display()
                    )));
                }
                seen_version = true;
                if value != b"1" {
                    return Err(Error::named(format!(
                        "{}: unsupported version: {}",
                        path.display(),
                        String::from_utf8_lossy(value)
                    )));
                }
            }
            b"profiles" => {
                if seen_profiles {
                    return Err(Error::named(format!(
                        "{}: duplicate profiles",
                        path.display()
                    )));
                }
                seen_profiles = true;
                if !list_valid(value, MemberKind::Profile) {
                    return Err(Error::named(format!(
                        "{}: invalid profiles list",
                        path.display()
                    )));
                }
                parents = String::from_utf8_lossy(value).into_owned();
            }
            b"overlays" => {
                if seen_overlays {
                    return Err(Error::named(format!(
                        "{}: duplicate overlays",
                        path.display()
                    )));
                }
                seen_overlays = true;
                if !list_valid(value, MemberKind::Overlay) {
                    return Err(Error::named(format!(
                        "{}: invalid overlays list",
                        path.display()
                    )));
                }
                overlays = String::from_utf8_lossy(value).into_owned();
            }
            _ => {
                return Err(Error::named(format!(
                    "{}: unknown key: {}",
                    path.display(),
                    String::from_utf8_lossy(key)
                )));
            }
        }
    }
    if !seen_version {
        return Err(Error::named(format!(
            "{}: missing version=1",
            path.display()
        )));
    }
    if !seen_profiles && !seen_overlays {
        return Err(Error::named(format!(
            "{}: profile has no members",
            path.display()
        )));
    }
    // Values passed every ASCII gate above, so lossy text is exact.
    Ok(Definition { parents, overlays })
}

/// Read + gate a definition file, like the top of
/// `_dot_profile_parse_definition`.
pub fn load_definition(path: &Path) -> Result<Definition, Error> {
    file_safe(path)?;
    let bytes = std::fs::read(path)
        .map_err(|_| Error::named(format!("cannot size file: {}", path.display())))?;
    parse_definition(path, &bytes)
}

impl State {
    /// Store one parsed definition under `name`.
    fn insert(&mut self, name: &str, definition: Definition) {
        if !definition.parents.is_empty() {
            self.parents.insert(name.to_string(), definition.parents);
        }
        if !definition.overlays.is_empty() {
            self.overlays.insert(name.to_string(), definition.overlays);
        }
        self.names.insert(name.to_string());
    }

    /// Append unless already present (the shell `_dot_profile_append_unique`).
    fn append_unique(target: &mut Vec<String>, value: &str) {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.to_string());
        }
    }

    /// `_dot_profile_expand`: depth-first include expansion with
    /// cycle detection (`visiting` fails, `resolved` short-circuits).
    /// Parents expand before the profile itself; overlays append in
    /// first-seen order.
    pub fn expand(&mut self, name: &str) -> Result<(), Error> {
        if !self.names.contains(name) {
            let message = format!("unknown profile: {name}");
            return Err(self.fail(message));
        }
        match self.expansion.get(name) {
            Some(Expansion::Visiting) => {
                let message = format!("profile inclusion cycle at: {name}");
                return Err(self.fail(message));
            }
            Some(Expansion::Resolved) => return Ok(()),
            None => (),
        }
        self.expansion.insert(name.to_string(), Expansion::Visiting);
        if let Some(parents) = self.parents.get(name).cloned() {
            for parent in parents.split(',') {
                self.expand(parent)?;
            }
        }
        Self::append_unique(&mut self.included, name);
        if let Some(overlays) = self.overlays.get(name).cloned() {
            for overlay in overlays.split(',') {
                Self::append_unique(&mut self.overlay_names, overlay);
            }
        }
        self.expansion.insert(name.to_string(), Expansion::Resolved);
        Ok(())
    }

    /// `_dot_profile_flatten`: expand one profile into the published
    /// lists; an expansion with no overlays fails.
    pub fn flatten(&mut self, name: &str) -> Result<(), Error> {
        self.included.clear();
        self.overlay_names.clear();
        self.expansion.clear();
        self.expand(name)?;
        if self.overlay_names.is_empty() {
            let message = format!("profile expansion is empty: {name}");
            return Err(self.fail(message));
        }
        Ok(())
    }

    /// Byte-sorted `*.conf` files directly inside `dir`.
    fn conf_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
        let mut names: Vec<Vec<u8>> = Vec::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|_| Error::named(format!("not a directory: {}", dir.display())))?;
        for entry in entries {
            let entry =
                entry.map_err(|_| Error::named(format!("not a directory: {}", dir.display())))?;
            let name = entry.file_name();
            let bytes = name.as_os_str().as_bytes();
            if bytes.ends_with(b".conf") {
                names.push(bytes.to_vec());
            }
        }
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| {
                let mut path = dir.as_os_str().as_bytes().to_vec();
                path.push(b'/');
                path.extend_from_slice(&name);
                PathBuf::from(std::ffi::OsStr::from_bytes(&path))
            })
            .collect())
    }

    /// `_dot_profiles_load`: reset, read every definition, require
    /// `base`, validate the default, and validate every profile
    /// expands. A missing directory is a clean empty state.
    pub fn load(
        &mut self,
        profiles_dir: Option<&Path>,
        xdg_config: &str,
        home: &str,
        default_profile: Option<&str>,
    ) -> Result<(), Error> {
        *self = State::default();
        let dir: PathBuf = match profiles_dir {
            Some(dir) => dir.to_path_buf(),
            None => crate::xdg::path(crate::xdg::Kind::Config, "dot/profiles.d", xdg_config, home)
                .map(PathBuf::from)
                .map_err(|_| self.fail("cannot resolve profiles directory".to_string()))?,
        };
        match std::fs::symlink_metadata(&dir) {
            Err(_) => return Ok(()),
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => (),
            _ => {
                let message = format!("not a directory: {}", dir.display());
                return Err(self.fail(message));
            }
        }
        self.present = true;
        for file in Self::conf_files(&dir)? {
            let raw = file
                .file_name()
                .expect("conf file name")
                .as_bytes()
                .to_vec();
            let stem = &raw[..raw.len() - ".conf".len()];
            if !identifier_valid(stem) {
                let message = format!(
                    "invalid profile filename: {}",
                    String::from_utf8_lossy(&raw)
                );
                return Err(self.fail(message));
            }
            let name = String::from_utf8_lossy(stem).into_owned();
            match load_definition(&file) {
                Ok(definition) => self.insert(&name, definition),
                Err(error) => {
                    let message = error.message.clone();
                    return Err(self.fail(message));
                }
            }
        }
        if !self.names.contains("base") {
            return Err(self.fail("profiles.d must define base".to_string()));
        }
        let default = default_profile.unwrap_or("base");
        if !identifier_valid(default.as_bytes()) {
            let message = format!("invalid default profile: {default}");
            return Err(self.fail(message));
        }
        if !self.names.contains(default) {
            let message = format!("unknown default profile: {default}");
            return Err(self.fail(message));
        }
        self.default_profile = default.to_string();
        for name in self.sorted_names().into_iter().cloned().collect::<Vec<_>>() {
            self.flatten(&name)?;
        }
        self.included.clear();
        self.overlay_names.clear();
        self.expansion.clear();
        Ok(())
    }

    /// `_dot_profiles_load_default`: resolve the default profiles
    /// directory through XDG (`dot/profiles.d` under the config base)
    /// and [`State::load`] it. A thin composition of those ported
    /// pieces (`crate::xdg::path` plus [`State::load`]): the resolve
    /// happens before `load`'s reset, because the shell returns out
    /// of `dot_xdg_path` before `_dot_profiles_load` touches any
    /// state — an unresolvable base leaves prior state (and the
    /// configuration error) intact. The failure message reuses
    /// `load`'s vocabulary for engine callers but is deliberately
    /// not recorded, matching the shell's silent return.
    pub fn load_default(
        &mut self,
        xdg_config: &str,
        home: &str,
        default_profile: Option<&str>,
    ) -> Result<(), Error> {
        let dir =
            match crate::xdg::path(crate::xdg::Kind::Config, "dot/profiles.d", xdg_config, home) {
                Ok(dir) => PathBuf::from(dir),
                Err(_) => {
                    return Err(Error::named(
                        "cannot resolve profiles directory".to_string(),
                    ));
                }
            };
        self.load(Some(&dir), xdg_config, home, default_profile)
    }

    /// Parse one selector file into [`Selector`].
    pub fn selector_parse(
        &mut self,
        path: &Path,
        bytes: &[u8],
        class: SelectorClass,
    ) -> Result<Selector, Error> {
        let mut seen_version = false;
        let mut seen_user = false;
        let mut seen_host = false;
        let mut seen_profile = false;
        let mut saw_setting = false;
        let mut user = String::new();
        let mut host = String::new();
        let mut profile = String::new();
        let fail =
            |number: usize, what: &str| Error::named(format!("{}:{number} {what}", path.display()));
        let settings = setting_lines(bytes)
            .map_err(|(number, failure)| Error::named(setting_message(path, number, failure)))?;
        for (number, key, value) in settings {
            let _ = number;
            if !saw_setting && key != b"version" {
                return Err(Error::named(format!(
                    "{}: version=1 must be the first setting",
                    path.display()
                )));
            }
            saw_setting = true;
            match key {
                b"version" => {
                    if seen_version {
                        return Err(fail(number, "duplicate version"));
                    }
                    seen_version = true;
                    if value != b"1" {
                        return Err(Error::named(format!(
                            "{}: unsupported version: {}",
                            path.display(),
                            String::from_utf8_lossy(value)
                        )));
                    }
                }
                b"user" => {
                    if seen_user {
                        return Err(fail(number, "duplicate user"));
                    }
                    seen_user = true;
                    if !user_valid(value) {
                        return Err(Error::named(format!(
                            "{}: invalid user: {}",
                            path.display(),
                            String::from_utf8_lossy(value)
                        )));
                    }
                    user = String::from_utf8_lossy(value).into_owned();
                }
                b"host" => {
                    if seen_host {
                        return Err(fail(number, "duplicate host"));
                    }
                    seen_host = true;
                    match host_normalize(value) {
                        Some(normalized) => host = normalized,
                        None => {
                            return Err(Error::named(format!(
                                "{}: invalid host: {}",
                                path.display(),
                                String::from_utf8_lossy(value)
                            )));
                        }
                    }
                }
                b"profile" => {
                    if seen_profile {
                        return Err(fail(number, "duplicate profile"));
                    }
                    seen_profile = true;
                    if !identifier_valid(value) {
                        return Err(Error::named(format!(
                            "{}: invalid profile: {}",
                            path.display(),
                            String::from_utf8_lossy(value)
                        )));
                    }
                    profile = String::from_utf8_lossy(value).into_owned();
                }
                _ => {
                    return Err(Error::named(format!(
                        "{}: unknown key: {}",
                        path.display(),
                        String::from_utf8_lossy(key)
                    )));
                }
            }
        }
        if !seen_version {
            return Err(Error::named(format!(
                "{}: missing version=1",
                path.display()
            )));
        }
        if !seen_profile {
            return Err(Error::named(format!("{}: missing profile", path.display())));
        }
        if class != SelectorClass::Root && user.is_empty() && host.is_empty() {
            return Err(Error::named(format!(
                "{}: non-root selector requires user or host",
                path.display()
            )));
        }
        if !self.names.contains(&profile) {
            return Err(Error::named(format!(
                "{}: unknown profile: {profile}",
                path.display()
            )));
        }
        Ok(Selector {
            user,
            host,
            profile,
        })
    }

    /// Read + gate one selector file (ownership rules for `local`).
    pub fn load_selector(
        &mut self,
        path: &Path,
        class: SelectorClass,
        euid: u32,
    ) -> Result<Selector, Error> {
        file_safe(path).map_err(|error| self.fail(error.message.clone()))?;
        if class == SelectorClass::Local && !private_path_safe(path, euid) {
            let message = format!("unsafe machine-local selector file: {}", path.display());
            return Err(self.fail(message));
        }
        let bytes = std::fs::read(path)
            .map_err(|_| self.fail(format!("cannot size file: {}", path.display())))?;
        self.selector_parse(path, &bytes, class)
            .map_err(|error| self.fail(error.message.clone()))
    }

    /// `_dot_profile_read_selector_dir`: match every selector in one
    /// directory, recording records, matches, and candidates.
    pub fn read_selector_dir(
        &mut self,
        class: SelectorClass,
        class_name: &str,
        directory: &Path,
        euid: u32,
    ) -> Result<(), Error> {
        if directory.as_os_str().is_empty() {
            return Ok(());
        }
        match std::fs::symlink_metadata(directory) {
            Err(_) => return Ok(()),
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => (),
            _ => {
                let message = format!("not a selector directory: {}", directory.display());
                return Err(self.fail(message));
            }
        }
        if class == SelectorClass::Local && !private_path_safe(directory, euid) {
            let message = format!(
                "unsafe machine-local selector directory: {}",
                directory.display()
            );
            return Err(self.fail(message));
        }
        for file in Self::conf_files(directory)? {
            let raw = file
                .file_name()
                .expect("selector file name")
                .as_bytes()
                .to_vec();
            if !value_safe(&raw) {
                let message = format!(
                    "unsafe selector filename: {}",
                    String::from_utf8_lossy(&raw)
                );
                return Err(self.fail(message));
            }
            let selector = self.load_selector(&file, class, euid)?;
            let matched = (selector.user.is_empty() || selector.user == self.current_user)
                && (selector.host.is_empty() || selector.host == self.current_host);
            let short = String::from_utf8_lossy(&raw).into_owned();
            self.selector_records.push(format!(
                "{class_name}|{}|{}|{}|{}|{matched}",
                file.display(),
                selector.user,
                selector.host,
                selector.profile
            ));
            if matched {
                self.selector_matches
                    .push(format!("{class_name}:{short}:{}", selector.profile));
                let mut specificity = 0;
                if !selector.user.is_empty() {
                    specificity += 1;
                }
                if !selector.host.is_empty() {
                    specificity += 1;
                }
                self.candidates.push((specificity, selector.profile));
            }
        }
        Ok(())
    }

    /// `_dot_profile_choose_selector`: most specific match wins; two
    /// different profiles at the top specificity conflict.
    pub fn choose_selector(&mut self) -> Result<(), Error> {
        self.selected.clear();
        let top = self
            .candidates
            .iter()
            .map(|(score, _)| *score)
            .max()
            .unwrap_or(0);
        for (score, profile) in self.candidates.clone() {
            if score != top {
                continue;
            }
            if self.selected.is_empty() {
                self.selected = profile;
            } else if self.selected != profile {
                // The shell publishes `conflict` before failing.
                self.selection_state = "conflict".to_string();
                let message = format!(
                    "equally specific selectors choose {} and {profile}",
                    self.selected
                );
                return Err(self.fail(message));
            }
        }
        Ok(())
    }

    /// `_dot_profile_resolve` with the identity already determined
    /// (pure; the thin [`State::resolve`] wrapper runs `id -un` and
    /// the platform hostname first).
    pub fn resolve_with(
        &mut self,
        root: &Path,
        local: &Path,
        personals: &[&Path],
        user: &str,
        host: &str,
        euid: u32,
    ) -> Result<(), Error> {
        if !self.present {
            return Ok(());
        }
        if !user_valid(user.as_bytes()) {
            let message = format!("invalid current user: {user}");
            return Err(self.fail(message));
        }
        let Some(normalized) = host_normalize(host.as_bytes()) else {
            let message = format!("invalid current short hostname: {host}");
            return Err(self.fail(message));
        };
        self.current_user = user.to_string();
        self.current_host = normalized;
        self.selected.clear();
        self.selection_state = "implicit-default".to_string();
        self.selector_matches.clear();
        self.selector_records.clear();
        self.candidates.clear();
        self.read_selector_dir(SelectorClass::Root, "root", root, euid)?;
        self.read_selector_dir(SelectorClass::Local, "local", local, euid)?;
        for personal in personals {
            self.read_selector_dir(SelectorClass::Personal, "personal", personal, euid)?;
        }
        self.choose_selector()?;
        if self.selected.is_empty() {
            self.selected = self.default_profile_fallback().to_string();
        } else {
            self.selection_state = "agreed-match".to_string();
        }
        let selected = self.selected.clone();
        self.flatten(&selected)
    }

    /// Default profile captured at load time (shell
    /// `DOT_DEFAULT_PROFILE`, defaulting to `base`).
    fn default_profile_fallback(&self) -> &str {
        &self.default_profile
    }

    /// `_dot_profile_resolve`: determine the login user and short
    /// hostname, then [`State::resolve_with`]. Skipped entirely when
    /// no `profiles.d` was loaded.
    pub fn resolve(
        &mut self,
        root: &Path,
        local: &Path,
        personals: &[&Path],
        euid: u32,
    ) -> Result<(), Error> {
        if !self.present {
            return Ok(());
        }
        let Some(user) = current_user() else {
            return Err(self.fail("cannot determine current user".to_string()));
        };
        let Ok(host) = crate::platform::detect_host() else {
            return Err(self.fail("cannot determine current short hostname".to_string()));
        };
        self.resolve_with(root, local, personals, &user, &host, euid)
    }

    /// `_dot_profile_select_base`: phase-one selection of `base`.
    pub fn select_base(&mut self) -> Result<(), Error> {
        if !self.present {
            return Ok(());
        }
        self.selected = "base".to_string();
        self.selection_state = "phase-one".to_string();
        self.selector_matches.clear();
        self.selector_records.clear();
        self.flatten("base")
    }
}

/// The rest of [`State`]: fields the resolve path fills from
/// environment-derived inputs.
impl State {
    /// `_dot_profile_resolve_default`: XDG selector roots plus one
    /// personal directory per active overlay whose ancestry is
    /// owned, then [`State::resolve_with`].
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_default(
        &mut self,
        xdg_config: &str,
        home: &str,
        overlay_entries: &[&str],
        user: &str,
        host: &str,
        euid: u32,
    ) -> Result<(), Error> {
        let root = crate::xdg::path(
            crate::xdg::Kind::Config,
            "dot/profile-selectors.d",
            xdg_config,
            home,
        )
        .map(PathBuf::from)
        .map_err(|_| self.fail("cannot resolve selector directory".to_string()))?;
        let local = crate::xdg::path(
            crate::xdg::Kind::Config,
            "dot/profile-selectors.local.d",
            xdg_config,
            home,
        )
        .map(PathBuf::from)
        .map_err(|_| self.fail("cannot resolve selector directory".to_string()))?;
        let mut personals: Vec<PathBuf> = Vec::new();
        for entry in overlay_entries {
            let mut fields = entry.split('|');
            let path = fields.nth(1).unwrap_or("");
            if path.is_empty() {
                continue;
            }
            let dot = format!("{path}/dot");
            let dot_path = Path::new(&dot);
            let exists = std::fs::symlink_metadata(dot_path).is_ok();
            if !exists {
                continue;
            }
            if !owned_directory_safe(Path::new(path), euid) || !owned_directory_safe(dot_path, euid)
            {
                let message = format!("unsafe personal selector ancestry: {dot}");
                return Err(self.fail(message));
            }
            let selector = format!("{dot}/profile-selectors.d");
            let selector_path = Path::new(&selector);
            if std::fs::symlink_metadata(selector_path).is_err() {
                continue;
            }
            if !owned_directory_safe(selector_path, euid) {
                let message = format!("unsafe personal selector directory: {selector}");
                return Err(self.fail(message));
            }
            personals.push(selector_path.to_path_buf());
        }
        let personal_refs: Vec<&Path> = personals.iter().map(PathBuf::as_path).collect();
        self.resolve_with(&root, &local, &personal_refs, user, host, euid)
    }
}

/// `_dot_profile_host_normalize`: strip one trailing dot, require a
/// valid hostname, lowercase (ASCII, like `${host,,}` under `C`).
pub fn host_normalize(host: &[u8]) -> Option<String> {
    let stripped = host.strip_suffix(b".").unwrap_or(host);
    if stripped.is_empty() {
        return None;
    }
    let valid = stripped[0].is_ascii_alphanumeric()
        && stripped
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
    if !valid {
        return None;
    }
    Some(
        stripped
            .iter()
            .map(|byte| byte.to_ascii_lowercase() as char)
            .collect(),
    )
}

/// `_dot_profile_user_valid`: `^[A-Za-z_][A-Za-z0-9_.-]*$`.
pub fn user_valid(user: &[u8]) -> bool {
    let mut bytes = user.iter();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || *first == b'_' => (),
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Current login name (`id -un`), like `_dot_profile_resolve`.
pub fn current_user() -> Option<String> {
    let output = std::process::Command::new("id")
        .arg("-un")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout);
    Some(name.trim_end_matches(['\r', '\n']).to_string())
}
