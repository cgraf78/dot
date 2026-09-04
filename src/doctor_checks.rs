//! Doctor check family (slice 72).
//!
//! Ports the nine check functions in `lib/dot/doctor/lock.sh`,
//! `lib/dot/doctor/merges.sh`, `lib/dot/doctor/overlays.sh`,
//! `lib/dot/doctor/provider.sh`, and `lib/dot/doctor/repos.sh`:
//! [`check_update_lock`], [`check_merges`],
//! [`check_profile_lifecycle`], [`check_overlays`],
//! [`shdeps_binary`], [`check_provider`],
//! [`completed_identity_matches_home`], [`is_client_checkout`],
//! and [`check_base_repo`]. The `doctor.sh` orchestrator
//! (`_dot_doctor`) stays shell-side in another lane.
//!
//! Everything here is a pure function of explicit inputs, the
//! established slice convention: shell globals (`DOT_*`,
//! `ACTIVE_OVERLAYS`, lifecycle arrays) arrive as parameters, and
//! helper boundaries owned by other slices arrive either as data or
//! as small predicates documented per function. Filesystem and `git`
//! probes the check itself performs (`-e`/`-d`/`-L` tests,
//! `readlink`, `rev-parse`, manifest reads) run in-process so the
//! differential tests observe both engines on the same fixtures.
//!
//! Reused sibling ports (not reimplemented):
//!
//! - [`crate::update_lock`] backs [`check_update_lock`] (owner
//!   read, liveness, initializing window).
//! - [`crate::overlays`] backs [`check_overlays`] (`is_worktree`,
//!   `effective_url`, `origin_matches`).
//! - [`crate::repos_overlays`] backs [`check_overlays`]
//!   (`parse_manifest_record`, `record_link_target`, `stream_lines`).
//! - [`crate::repos_base`] backs [`check_base_repo`] (`git_prefix`
//!   shape, `run_git` spawn boundary).
//!
//! Parity decisions:
//!
//! - Checks emit [`Record`]s instead of calling the `_dr_*`
//!   emitters; [`render`] formats them byte-identical to
//!   `doctor/runtime.sh` with color disabled (piped stdout, where
//!   `[[ -t 1 ]]` is false). The canonical renderer stays with the
//!   runtime slice; this copy exists so parity tests can
//!   byte-compare against the live shell functions.
//! - `_dr_tilde` / `_dr_symlink_points_to` (`doctor/paths.sh`) are
//!   mirrored as private helpers: display-only glue the checks need
//!   to spell details, owned by the paths slice when it lands.
//! - `local_validate` (`_overlay_local_source_validate`,
//!   `find`-walk plus per-entry checks), the profile deactivation
//!   probe, the shdeps installer selection, and the lifecycle ledger
//!   load stay caller concerns: they encode trust policy owned by
//!   other slices, so tests inject their outcomes.
//! - The `_dr_check_merges` "inventory is invalid" branch only
//!   fires when the `wc -l` pipeline itself fails (a bad inventory
//!   still prints zero lines through `sort`, whose exit status
//!   decides); the port keeps the branch with `spec_count: None`.
//! - `read` field splitting (`IFS='|'`, `-r`, last variable keeps
//!   the remainder, missing fields read empty) is mirrored by the
//!   private `read_fields` helper.
//! - Shell `$(...)` trailing-newline stripping is mirrored by
//!   trimming trailing `\n` from captured `git` output.
//! - `set -u` arrays (`CONFIGURED_OVERLAY_NAMES`, `INCLUDED_PROFILES`,
//!   ...) must exist shell-side; unset and empty both arrive as
//!   empty vectors.
//! - No `let`-chains: MSRV is Rust 1.85.
//! - No `Command::envs`: children receive single `env`/`env_remove`
//!   entries.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// Which `_dr_*` emitter produced a [`Record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `_dr_section`: a section heading, never carrying a detail.
    Section,
    /// `_dr_ok`: a passing check.
    Ok,
    /// `_dr_warn`: a warning check.
    Warn,
    /// `_dr_fail`: a failing check.
    Fail,
    /// `_dr_skip`: a skipped check.
    Skip,
}

/// One doctor result line: the emitter plus its `$1` label and
/// optional `$2` detail. `detail: None` means the shell call passed
/// exactly one argument (`[[ $# -gt 1 ]]` false).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Emitting `_dr_*` function.
    pub kind: Kind,
    /// The `$1` label.
    pub message: String,
    /// The optional `$2` detail.
    pub detail: Option<String>,
}

impl Record {
    /// A section heading (`_dr_section "$message"`).
    pub fn section(message: impl Into<String>) -> Self {
        Self {
            kind: Kind::Section,
            message: message.into(),
            detail: None,
        }
    }

    /// A passing check (`_dr_ok "$message" ["$detail"]`).
    pub fn ok(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind: Kind::Ok,
            message: message.into(),
            detail,
        }
    }

    /// A warning check (`_dr_warn "$message" ["$detail"]`).
    pub fn warn(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind: Kind::Warn,
            message: message.into(),
            detail,
        }
    }

    /// A failing check (`_dr_fail "$message" ["$detail"]`).
    pub fn fail(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind: Kind::Fail,
            message: message.into(),
            detail,
        }
    }

    /// A skipped check (`_dr_skip "$message" ["$detail"]`).
    pub fn skip(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind: Kind::Skip,
            message: message.into(),
            detail,
        }
    }
}

/// Render records byte-identical to `doctor/runtime.sh` with color
/// disabled (piped stdout: every color variable is empty):
///
/// - section: `\n{message}\n`
/// - ok/skip: `  ✓/· {message}[ ({detail})]\n`
/// - warn/fail: `  ⚠/✗ {message}[\n    {detail}]\n`
pub fn render(records: &[Record]) -> String {
    let mut out = String::new();
    for record in records {
        match record.kind {
            Kind::Section => {
                out.push('\n');
                out.push_str(&record.message);
                out.push('\n');
            }
            Kind::Ok => {
                out.push_str("  \u{2713} ");
                out.push_str(&record.message);
                if let Some(detail) = &record.detail {
                    out.push_str(" (");
                    out.push_str(detail);
                    out.push(')');
                }
                out.push('\n');
            }
            Kind::Warn => {
                out.push_str("  \u{26a0} ");
                out.push_str(&record.message);
                if let Some(detail) = &record.detail {
                    out.push_str("\n    ");
                    out.push_str(detail);
                }
                out.push('\n');
            }
            Kind::Fail => {
                out.push_str("  \u{2717} ");
                out.push_str(&record.message);
                if let Some(detail) = &record.detail {
                    out.push_str("\n    ");
                    out.push_str(detail);
                }
                out.push('\n');
            }
            Kind::Skip => {
                out.push_str("  \u{b7} ");
                out.push_str(&record.message);
                if let Some(detail) = &record.detail {
                    out.push_str(" (");
                    out.push_str(detail);
                    out.push(')');
                }
                out.push('\n');
            }
        }
    }
    out
}

/// `_dr_tilde`: abbreviate `path` under `home` with `~`. Mirrors
/// the shell `case` arms literally, including the empty-`HOME`
/// corner (`"$HOME"/*` with empty `HOME` matches `/*`).
fn tilde(path: &str, home: &str) -> String {
    if path == home {
        return "~".to_string();
    }
    if home.is_empty() {
        if let Some(rest) = path.strip_prefix('/') {
            return format!("~/{rest}");
        }
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(home) {
        if let Some(rest) = rest.strip_prefix('/') {
            return format!("~/{rest}");
        }
    }
    path.to_string()
}

/// `_dr_physical_path`: resolve the parent directory physically
/// (`cd dir && pwd -P`), keeping the leaf name. Trailing slashes
/// strip (except root); `/` maps to `//` like the shell
/// `printf '%s/%s\n' / /`. Returns `None` when the parent is not a
/// directory or cannot be resolved.
fn physical_path(path: &str) -> Option<String> {
    let mut rest = path;
    while rest != "/" && rest.ends_with('/') {
        rest = &rest[..rest.len() - 1];
    }
    let (dir, base) = if rest == "/" {
        ("/", "/")
    } else if let Some(index) = rest.rfind('/') {
        let (dir, base) = rest.split_at(index);
        (if dir.is_empty() { "/" } else { dir }, &base[1..])
    } else {
        (".", rest)
    };
    if !Path::new(dir).is_dir() {
        return None;
    }
    let canonical = std::fs::canonicalize(dir).ok()?;
    Some(format!("{}/{}", canonical.display(), base))
}

/// `_dr_symlink_target_path` plus `_dr_symlink_points_to`: whether
/// `link` resolves (through a possibly relative `readlink` target)
/// to the same physical path as `expected`. A missing `expected`
/// (`[[ -e ]]`, links followed) or an unreadable link fails.
fn symlink_points_to(link: &Path, expected: &str) -> bool {
    if !Path::new(expected).exists() {
        return false;
    }
    let target = match std::fs::read_link(link) {
        Ok(target) => target,
        Err(_) => return false,
    };
    let joined: PathBuf = if target.is_absolute() {
        target
    } else {
        let dir = link.parent().unwrap_or_else(|| Path::new("."));
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        dir.join(&target)
    };
    let actual = match physical_path(&joined.to_string_lossy()) {
        Some(actual) => actual,
        None => return false,
    };
    match physical_path(expected) {
        Some(expected_physical) => actual == expected_physical,
        None => false,
    }
}

/// Shell `IFS='|' read -r` into `count` variables: split on `|`
/// (no trimming, `-r` keeps backslashes), the last variable keeps
/// the unsplit remainder, missing fields read empty. Single-line
/// records only (callers never pass embedded newlines, like the
/// shell herestrings the checks read from).
fn read_fields(record: &str, count: usize) -> Vec<String> {
    let mut fields: Vec<String> = record.split('|').map(str::to_string).collect();
    if fields.len() > count && count > 0 {
        let rest = fields[count - 1..].join("|");
        fields.truncate(count - 1);
        fields.push(rest);
    }
    while fields.len() < count {
        fields.push(String::new());
    }
    fields
}

/// The record name: `${record%%|*}`, the text before the first
/// `|` (the whole record when there is none).
fn record_name(record: &str) -> &str {
    match record.find('|') {
        Some(index) => &record[..index],
        None => record,
    }
}

/// Shell `$(...)` capture: strip every trailing newline.
fn captured(output: &str) -> String {
    output.trim_end_matches('\n').to_string()
}

/// Shell `[[ $value =~ ^[0-9]+$ ]]`.
fn is_uint(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Owner-execute-or-any-execute probe mirroring `[[ -x $path ]]`
/// for the cases the checks meet: any execute bit set. (Full
/// `access(2)` semantics for foreign-owned files are not modeled;
/// fixtures run as the file owner, where owner-bit and `-x`
/// agree, and root's any-bit rule matches this exactly.)
fn is_executable_bits(mode: u32) -> bool {
    mode & 0o111 != 0
}

/// `_dr_check_update_lock` (`doctor/lock.sh`): the process-wide
/// mutation lock is either clear, held live, stale, initializing,
/// or incomplete.
///
/// `lock_dir` is the `_dot_update_lock_path` result (`None` when
/// path resolution fails). Owner liveness reuses
/// [`crate::update_lock`]: `read_owner` for
/// `_dot_update_lock_read_owner`, `owner_is_active` for
/// `_dot_update_lock_owner_is_active`, and `is_initializing` for
/// `_dot_update_lock_is_initializing`. The `-e`/`-L`/`-d` probes
/// read through `symlink_metadata` exactly like the shell
/// conditionals (a symlink to a directory is unsafe, not clear).
pub fn check_update_lock(lock_dir: Option<&Path>) -> Vec<Record> {
    let mut out = vec![Record::section("Update lock")];
    let Some(dir) = lock_dir else {
        out.push(Record::fail("update lock path cannot be resolved", None));
        return out;
    };
    let present = std::fs::symlink_metadata(dir).is_ok();
    if !present {
        out.push(Record::ok("update lock is clear", None));
        return out;
    }
    let meta = std::fs::symlink_metadata(dir).ok();
    let is_dir = meta.as_ref().is_some_and(|meta| meta.is_dir());
    let is_link = meta.as_ref().is_some_and(|meta| meta.is_symlink());
    // `[[ ! -d $lock_dir || -L $lock_dir ]]`: `-d` follows links,
    // so a symlink to a directory still fails the second arm.
    if !is_dir || is_link {
        out.push(Record::fail(
            "update lock path is unsafe",
            Some(dir.to_string_lossy().into_owned()),
        ));
        return out;
    }
    if let Some(owner) = crate::update_lock::read_owner(dir) {
        if crate::update_lock::owner_is_active(&owner) {
            out.push(Record::warn(
                "update is currently running",
                Some(format!("pid {}", owner.pid)),
            ));
        } else {
            out.push(Record::warn(
                "update lock owner is stale",
                Some("the next mutating command will reclaim it".to_string()),
            ));
        }
    } else if crate::update_lock::is_initializing(dir) {
        out.push(Record::warn("update lock is being initialized", None));
    } else {
        out.push(Record::warn(
            "update lock record is incomplete",
            Some("the next mutating command will attempt recovery".to_string()),
        ));
    }
    out
}

/// Inputs for [`check_merges`]: the extension boundary plus the
/// merge-hook inventory as explicit data.
pub struct MergeInputs {
    /// `_dot_extensions_enabled` (`DOT_EXTENSION_API == 1` with a
    /// non-empty `DOT_EXTENSIONS_DIR`).
    pub enabled: bool,
    /// `DOT_EXTENSIONS_DIR`, spelled exactly as configured: the
    /// check concatenates `$DOT_EXTENSIONS_DIR/merge-hooks.d`
    /// (an empty value probes `/merge-hooks.d`, but `enabled` is
    /// false then and the root is never built).
    pub extensions_dir: String,
    /// `_merge_hook_specs` output line count (`wc -l` semantics),
    /// or `None` when the inventory pipeline itself fails. The
    /// spec listing stays shell-side (`merges` slice); only its
    /// count crosses here.
    pub spec_count: Option<usize>,
}

/// `_dr_check_merges` (`doctor/merges.sh`): merge-hook extension
/// discovery health. The `-e`/`-d`/`-L` root probes run in-process;
/// the inventory count arrives via [`MergeInputs::spec_count`].
pub fn check_merges(inputs: &MergeInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Extensions")];
    if !inputs.enabled {
        out.push(Record::skip("no extension root configured", None));
        return out;
    }
    let root = format!("{}/merge-hooks.d", inputs.extensions_dir);
    let root_path = Path::new(&root);
    let present = std::fs::symlink_metadata(root_path).is_ok();
    if !present {
        out.push(Record::skip(
            "merge-hook extensions",
            Some("none configured".to_string()),
        ));
        return out;
    }
    let meta = std::fs::symlink_metadata(root_path).ok();
    let is_dir = meta.as_ref().is_some_and(|meta| meta.is_dir());
    let is_link = meta.as_ref().is_some_and(|meta| meta.is_symlink());
    if !is_dir || is_link {
        out.push(Record::fail(
            "merge-hook extension directory is unavailable",
            Some(root),
        ));
        return out;
    }
    let Some(count) = inputs.spec_count else {
        out.push(Record::fail(
            "merge-hook extension inventory is invalid",
            Some(root),
        ));
        return out;
    };
    if count > 0 {
        out.push(Record::ok(
            "merge-hook extensions",
            Some(format!("{count} hook(s)")),
        ));
    } else {
        out.push(Record::skip(
            "merge-hook extensions",
            Some("none configured".to_string()),
        ));
    }
    out
}

/// Inputs for [`check_profile_lifecycle`]: the profile lifecycle
/// arrays plus the two helper boundaries as explicit data.
pub struct LifecycleInputs<'a> {
    /// `DOT_PROFILES_PRESENT == 1`. When false the shell function
    /// returns silently (no section: the `Profiles` heading belongs
    /// to [`check_overlays`]).
    pub profiles_present: bool,
    /// `_dot_profile_lifecycle_load` exit status.
    pub load_ok: bool,
    /// `ELIGIBLE_OVERLAY_NAMES`.
    pub eligible: Vec<String>,
    /// `ACTIVE_OVERLAYS` raw `name|...` records (later entries win,
    /// like the shell associative assignment).
    pub active: Vec<String>,
    /// `DOT_PROFILE_LIFECYCLE_RECORDS` raw `name|...` records.
    pub records: Vec<String>,
    /// `_dot_extensions_enabled`.
    pub extensions_enabled: bool,
    /// `_dot_profile_deactivation_script "$record" >/dev/null`
    /// exit status per record: the deactivation-authority probe
    /// (trust policy owned by the profile slice).
    pub deactivation_ok: &'a dyn Fn(&str) -> bool,
}

/// `_dr_check_profile_lifecycle` (`doctor/overlays.sh`): pending
/// profile deactivations must each retain a usable deactivation
/// authority. Emits no section of its own; [`check_overlays`]
/// appends these records under its `Profiles` heading, and tests
/// drive this function directly for the lifecycle matrix.
pub fn check_profile_lifecycle(inputs: &LifecycleInputs) -> Vec<Record> {
    let mut out = Vec::new();
    if !inputs.profiles_present {
        return out;
    }
    if !inputs.load_ok {
        out.push(Record::fail(
            "profile lifecycle state unsafe",
            Some("run dot update after repairing the lifecycle ledger".to_string()),
        ));
        return out;
    }
    let eligible: HashSet<&str> = inputs.eligible.iter().map(String::as_str).collect();
    let mut active: BTreeMap<&str, &str> = BTreeMap::new();
    for record in &inputs.active {
        active.insert(record_name(record), record.as_str());
    }
    let mut pending: Vec<&str> = Vec::new();
    for record in &inputs.records {
        let name = record_name(record);
        if eligible.contains(name) {
            if let Some(active_record) = active.get(name) {
                if !(inputs.deactivation_ok)(active_record) {
                    out.push(Record::fail(
                        format!("{name}: active profile deactivation authority unsafe"),
                        None,
                    ));
                }
            } else if !(inputs.deactivation_ok)(record) {
                out.push(Record::warn(
                    format!("{name}: retained profile deactivation authority unavailable"),
                    Some("selected optional overlay is not currently active".to_string()),
                ));
            }
            continue;
        }
        pending.push(name);
        if !inputs.extensions_enabled {
            out.push(Record::fail(
                "profile deactivation pending while extensions are disabled",
                Some(name.to_string()),
            ));
            continue;
        }
        if !(inputs.deactivation_ok)(record) {
            out.push(Record::fail(
                format!("{name}: retiring overlay authority unsafe"),
                Some("restore the recorded checkout identity, then run dot update".to_string()),
            ));
            continue;
        }
    }
    if pending.is_empty() {
        out.push(Record::ok(
            "profile lifecycle state",
            Some("no pending deactivations".to_string()),
        ));
    } else {
        out.push(Record::fail(
            "profile deactivation pending",
            Some(format!("{} (run dot update to retry)", pending.join(" "))),
        ));
    }
    out
}

/// Inputs for [`check_overlays`]: every profile/overlay global as
/// explicit data plus the one trust-policy probe.
pub struct OverlayInputs<'a> {
    /// `$HOME`, for `_dr_tilde` display only.
    pub home: &'a str,
    /// `DOT_PROFILE_CONFIGURATION_ERROR` (unset or empty: absent).
    pub profile_config_error: Option<&'a str>,
    /// `DOT_PROFILES_PRESENT == 1`.
    pub profiles_present: bool,
    /// `DOT_PROFILE_CURRENT_USER` (identity needs the host too).
    pub profile_user: Option<&'a str>,
    /// `DOT_PROFILE_CURRENT_HOST`.
    pub profile_host: Option<&'a str>,
    /// `SELECTED_PROFILE` (unset or empty: absent).
    pub selected_profile: Option<&'a str>,
    /// `DOT_PROFILE_SELECTION_STATE` (defaults to `unknown`).
    pub selection_state: Option<&'a str>,
    /// `INCLUDED_PROFILES`.
    pub included_profiles: Vec<String>,
    /// `PHASE_ONE_SELECTED_OVERLAY_NAMES`.
    pub phase_one: Vec<String>,
    /// `DOT_PROFILE_SELECTOR_RECORDS` raw
    /// `class|path|user|host|profile|matched` records.
    pub selectors: Vec<String>,
    /// Nested [`LifecycleInputs`] for the profiles branch.
    pub lifecycle: LifecycleInputs<'a>,
    /// `${#CONFIGURED_OVERLAY_NAMES[@]}` (names are unused beyond
    /// the count).
    pub configured_count: usize,
    /// `DOT_OVERLAY_MANIFEST` path, spelled as configured.
    pub manifest: String,
    /// `DOT_OVERLAY_DISCOVERY_ERROR` (unset or empty: absent).
    pub discovery_error: Option<&'a str>,
    /// `ACTIVE_OVERLAYS` raw `name|path|url|descriptor|optional|sync`
    /// records (later entries win).
    pub active_records: Vec<String>,
    /// `DOT_OVERLAY_LIFECYCLE` raw `name|state|descriptor` records.
    pub overlay_lifecycle: Vec<String>,
    /// `_overlay_local_source_validate "$path"` per overlay path:
    /// `Ok` when the local source is available, `Err(reply)` with
    /// the shell `REPLY` diagnostic otherwise (empty `REPLY`
    /// falls back to `$path/home`, like `${REPLY:-$path/home}`).
    /// Trust policy owned by the overlay slice.
    pub local_validate: &'a dyn Fn(&str) -> Result<(), String>,
}

/// A non-empty option: shell `[[ -n ${var:-} ]]` treats unset and
/// empty identically.
fn present(value: Option<&str>) -> Option<&str> {
    match value {
        Some(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// `_dr_check_overlays` (`doctor/overlays.sh`): profile selection
/// reporting, per-overlay lifecycle and source health, and overlay
/// symlink ownership validation.
///
/// Worktree, URL, and origin probes reuse [`crate::overlays`];
/// manifest parsing and link-target derivation reuse
/// [`crate::repos_overlays`]; the manifest file and `$HOME` links
/// are read in-process (the shell `readlink` batch is a
/// performance shape with no observable difference). Only
/// [`OverlayInputs::local_validate`] is injected.
pub fn check_overlays(inputs: &OverlayInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Profiles")];
    if let Some(error) = present(inputs.profile_config_error) {
        out.push(Record::fail(
            "profile configuration invalid",
            Some(error.to_string()),
        ));
    } else if !inputs.profiles_present {
        out.push(Record::skip(
            "profile selection disabled",
            Some("no profiles.d directory; using legacy overlay discovery".to_string()),
        ));
    } else {
        if let (Some(user), Some(host)) =
            (present(inputs.profile_user), present(inputs.profile_host))
        {
            out.push(Record::ok(
                "profile identity",
                Some(format!("{user}@{host}")),
            ));
        }
        if let Some(selected) = present(inputs.selected_profile) {
            let state = present(inputs.selection_state).unwrap_or("unknown");
            out.push(Record::ok(
                "selected profile",
                Some(format!("{selected} ({state})")),
            ));
        }
        if !inputs.included_profiles.is_empty() {
            out.push(Record::ok(
                "included profiles",
                Some(inputs.included_profiles.join(" ")),
            ));
        }
        if !inputs.phase_one.is_empty() {
            out.push(Record::ok(
                "phase-one overlays",
                Some(inputs.phase_one.join(" ")),
            ));
        }
        for selector in &inputs.selectors {
            let fields = read_fields(selector, 6);
            if fields[5] != "true" {
                continue;
            }
            let source: String = match fields[0].as_str() {
                "root" => "root".to_string(),
                "local" => "machine-local".to_string(),
                "personal" => "active personal overlay".to_string(),
                other => other.to_string(),
            };
            let leaf = match fields[1].rsplit('/').next() {
                Some(leaf) => leaf,
                None => fields[1].as_str(),
            };
            out.push(Record::ok(
                format!("matching selector ({source})"),
                Some(format!("{} -> {}", leaf, fields[4])),
            ));
        }
        out.extend(check_profile_lifecycle(&inputs.lifecycle));
    }

    out.push(Record::section(format!(
        "Overlays ({} configured)",
        inputs.configured_count
    )));
    if let Some(error) = present(inputs.discovery_error) {
        out.push(Record::fail(
            "overlay descriptor invalid",
            Some(error.to_string()),
        ));
    }
    if inputs.configured_count == 0 && !Path::new(&inputs.manifest).is_file() {
        out.push(Record::skip("no overlays to check", None));
        return out;
    } else if inputs.configured_count == 0 {
        out.push(Record::skip("no active overlay descriptors", None));
    }

    let mut active: BTreeMap<&str, &str> = BTreeMap::new();
    for entry in &inputs.active_records {
        active.insert(record_name(entry), entry.as_str());
    }
    // Overlay paths by name, for the manifest symlink ownership
    // pass (`overlay_paths` / `overlay_syncs` in the shell).
    let mut overlay_paths: BTreeMap<String, String> = BTreeMap::new();
    let mut overlay_syncs: BTreeMap<String, String> = BTreeMap::new();
    for lifecycle in &inputs.overlay_lifecycle {
        let fields = read_fields(lifecycle, 3);
        let name = fields[0].clone();
        let state = fields[1].as_str();
        match state {
            "not-selected" => {
                out.push(Record::skip(format!("{name}: not selected"), None));
                continue;
            }
            "selected-ineligible" => {
                out.push(Record::skip(
                    format!("{name}: selected but host/platform ineligible"),
                    None,
                ));
                continue;
            }
            "selected-optional-unavailable" => {
                out.push(Record::skip(
                    format!("{name}: selected optional but unavailable"),
                    None,
                ));
                continue;
            }
            "selected-unavailable" => {
                out.push(Record::fail(
                    format!("{name}: selected but unavailable"),
                    None,
                ));
                continue;
            }
            "active" => {}
            _ => {
                out.push(Record::fail(
                    format!("{name}: unknown overlay lifecycle state"),
                    Some(state.to_string()),
                ));
                continue;
            }
        }
        let entry = match active.get(name.as_str()) {
            Some(entry) => *entry,
            None => {
                out.push(Record::fail(
                    format!("{name}: active lifecycle record missing"),
                    None,
                ));
                continue;
            }
        };
        // `name|path|url|descriptor|optional|sync`: `read` parks a
        // seventh field in `sync`, so only an exact `git`/`none`
        // spelling selects those arms.
        let entry_fields = read_fields(entry, 6);
        let path = entry_fields[1].clone();
        let url = entry_fields[2].clone();
        let optional = entry_fields[4].clone();
        let mut sync = entry_fields[5].clone();
        if sync.is_empty() {
            sync = "git".to_string();
        }
        overlay_paths.insert(name.clone(), path.clone());
        overlay_syncs.insert(name.clone(), sync.clone());
        if sync == "none" {
            match (inputs.local_validate)(&path) {
                Ok(()) => {
                    out.push(Record::ok(
                        format!("{name}: local source available"),
                        Some(tilde(&path, inputs.home)),
                    ));
                }
                Err(reply) => {
                    let diagnostic = if reply.is_empty() {
                        format!("{path}/home")
                    } else {
                        reply
                    };
                    out.push(Record::fail(
                        format!("{name}: local source unavailable"),
                        Some(tilde(&diagnostic, inputs.home)),
                    ));
                }
            }
            continue;
        }
        if !crate::overlays::is_worktree(Path::new(&path)) {
            if optional == "true" {
                out.push(Record::skip(
                    name,
                    Some("optional overlay not cloned".to_string()),
                ));
                continue;
            }
            out.push(Record::fail(
                format!("{name}: not cloned"),
                Some(format!("expected at {}", tilde(&path, inputs.home))),
            ));
            continue;
        }
        out.push(Record::ok(
            format!("{name}: cloned"),
            Some(tilde(&path, inputs.home)),
        ));
        let expected = crate::overlays::effective_url(&url, inputs.home);
        match crate::overlays::origin_matches(Path::new(&path), &expected) {
            Ok(_) => {
                out.push(Record::ok(
                    format!("{name}: remote.origin.url matches conf"),
                    None,
                ));
            }
            Err(actual) => {
                out.push(Record::warn(
                    format!("{name}: remote URL drift"),
                    Some(format!("conf={expected} vs actual={actual}")),
                ));
            }
        }
    }

    if Path::new(&inputs.manifest).is_file() {
        check_overlay_links(inputs, &overlay_paths, &overlay_syncs, &mut out);
    }
    out
}

/// The manifest symlink ownership pass of [`check_overlays`]:
/// every manifest record must still resolve to the owning
/// overlay's current link target. `overlay_paths`/`overlay_syncs`
/// hold the active overlays seen above; anything else (missing or
/// broken link, unknown owner, derivation failure, target drift)
/// counts one issue.
fn check_overlay_links(
    inputs: &OverlayInputs,
    overlay_paths: &BTreeMap<String, String>,
    overlay_syncs: &BTreeMap<String, String>,
    out: &mut Vec<Record>,
) {
    let content = std::fs::read(&inputs.manifest).unwrap_or_default();
    // `stream_lines` mirrors the shell `while read` loop: NULs
    // stripped, `\n`-split, final partial line kept.
    let mut issues: u64 = 0;
    let mut owners: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    for line in crate::repos_overlays::stream_lines(&content) {
        let Some(parsed) = crate::repos_overlays::parse_manifest_record(&line) else {
            issues += 1;
            continue;
        };
        // Three-column records carry the literal link target as
        // part of the authority contract; two-column records fall
        // back to the physical comparison below.
        let exact = line.split('\t').count() >= 3;
        owners.insert(parsed.rel, (parsed.owner, parsed.target, exact));
    }
    for (rel, (owner, expected_lexical, exact)) in &owners {
        let dst = format!("{}/{}", inputs.home, rel);
        let dst_path = Path::new(&dst);
        let link_meta = std::fs::symlink_metadata(dst_path).ok();
        let is_link = link_meta
            .as_ref()
            .is_some_and(|meta| meta.file_type().is_symlink());
        if !is_link {
            issues += 1;
            continue;
        }
        if !dst_path.exists() {
            issues += 1;
            continue;
        }
        let Some(path) = overlay_paths.get(owner) else {
            issues += 1;
            continue;
        };
        let Some(sync) = overlay_syncs.get(owner) else {
            issues += 1;
            continue;
        };
        let actual_bytes = std::fs::read_link(dst_path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .unwrap_or_default();
        let actual = String::from_utf8_lossy(&actual_bytes).into_owned();
        let current = match crate::repos_overlays::record_link_target(
            rel,
            owner,
            path,
            Some(sync.as_str()),
        ) {
            Some(current) => current,
            None => {
                issues += 1;
                continue;
            }
        };
        if *exact {
            if expected_lexical != &current || actual != current {
                issues += 1;
            }
            continue;
        }
        if actual == current {
            continue;
        }
        let expected = format!("{path}/home/{rel}");
        if !symlink_points_to(dst_path, &expected) {
            issues += 1;
        }
    }
    if issues == 0 {
        out.push(Record::ok("overlay symlinks healthy", None));
    } else {
        out.push(Record::warn(
            format!("{issues} overlay symlink issue(s)"),
            Some("run 'dot update' to re-link".to_string()),
        ));
    }
}

/// `_dr_shdeps_binary` (`doctor/provider.sh`): resolve the
/// provider binary. A pre-selected executable `_SHDEPSW_BIN` wins;
/// otherwise the installer selects `shdeps` next to itself
/// (`${installer%/*}`: the text before the last `/`, or the whole
/// string when there is no slash), then the debug and release
/// build trees. Candidates must be plain executable files
/// (`-f && ! -L && -x`); the pre-selected path needs only `-x`,
/// like the shell. Returns the selected path, or `None` for the
/// shell `return 1`.
pub fn shdeps_binary(shdepsw_bin: Option<&Path>, installer: &Path) -> Option<PathBuf> {
    if let Some(selected) = shdepsw_bin {
        let executable = std::fs::symlink_metadata(selected)
            .ok()
            .as_ref()
            .is_some_and(|meta| is_executable_bits(meta.permissions().mode()));
        if executable {
            return Some(selected.to_path_buf());
        }
    }
    // `${installer%/*}` in bytes: strip from the last `/`, or
    // keep the whole spelling when there is none.
    let raw = installer.as_os_str().as_bytes();
    let root: Vec<u8> = match raw.iter().rposition(|byte| *byte == b'/') {
        Some(index) => raw[..index].to_vec(),
        None => raw.to_vec(),
    };
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    for suffix in ["shdeps", "target/debug/shdeps", "target/release/shdeps"] {
        let mut candidate = root.clone();
        if !candidate.is_empty() {
            candidate.push(b'/');
        }
        candidate.extend_from_slice(suffix.as_bytes());
        candidates.push(candidate);
    }
    for candidate in &candidates {
        let path = Path::new(std::ffi::OsStr::from_bytes(candidate));
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        // `[[ -f && ! -L && -x ]]`: `file_type().is_file()`
        // follows the `-f`/`-L` split (a symlink to a file fails
        // the `-L` arm).
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            continue;
        }
        if is_executable_bits(meta.permissions().mode()) {
            return Some(path.to_path_buf());
        }
    }
    None
}

/// The reviewed installer selection for [`check_provider`],
/// mirroring `_dot_shdeps_installer`'s `REPLY` plus
/// `_DOT_SHDEPS_INSTALLER_SOURCE`.
pub struct ProviderInstaller<'a> {
    /// Installer path (`REPLY`).
    pub path: &'a str,
    /// `_DOT_SHDEPS_INSTALLER_SOURCE`: `explicit`, `pinned-dev`,
    /// `latest-dev`, or `managed` (anything else reports the
    /// source unavailable, like the shell `*` arm).
    pub source: &'a str,
}

/// Inputs for [`check_provider`]: the provider globals plus every
/// shdeps helper outcome as explicit data.
pub struct ProviderInputs<'a> {
    /// `$HOME`, for `_dr_tilde` display only.
    pub home: &'a str,
    /// `DOT_DEPENDENCY_PROVIDER`: unset or empty selects `none`
    /// (like `${var:-none}`); `shdeps` proceeds; anything else is
    /// unsupported.
    pub dependency_provider: Option<&'a str>,
    /// `DOT_SHDEPS_UPDATE_POLICY` (empty selects `pinned`, like
    /// `${var:-pinned}`).
    pub policy: &'a str,
    /// `_dot_shdeps_configure_env` exit status.
    pub configure_ok: bool,
    /// `SHDEPS_GIT_DEV_DIR` (empty when unset: the development
    /// checkout then spells `/shdeps`).
    pub dev_dir: &'a str,
    /// `-e`/`-L` on the development checkout.
    pub development_exists: bool,
    /// `_dot_shdeps_development_checkout_valid` exit status
    /// (consulted only under the `latest` policy with an existing
    /// checkout, like the shell short-circuit).
    pub development_valid: bool,
    /// `_dot_shdeps_installer` result (`None` when it fails).
    pub installer: Option<ProviderInstaller<'a>>,
    /// `_dot_shdeps_lock_value revision` (failures read empty).
    /// Consulted only for `latest` plus a development source.
    pub locked_revision: Option<&'a str>,
    /// `git -C "$development" rev-parse HEAD` (failures read
    /// empty). Same gating as `locked_revision`.
    pub development_revision: Option<&'a str>,
    /// `_dr_shdeps_binary "$installer"` result: binary resolution
    /// stays a caller concern (see [`shdeps_binary`]); `None`
    /// reports the binary unavailable.
    pub binary: Option<&'a str>,
    /// `_dot_shdeps_lock_value abi` (failures read empty, which
    /// reports `<missing>`).
    pub expected_abi: Option<&'a str>,
    /// `_dot_shdeps_binary_abi_version` `REPLY` (`None` when the
    /// probe fails, which reports `<unavailable>`).
    pub actual_abi: Option<&'a str>,
}

/// `_dr_check_provider` (`doctor/provider.sh`): the dependency
/// provider boundary (reviewed installer selection plus ABI
/// agreement). Helper outcomes arrive via [`ProviderInputs`];
/// only the `_dr_tilde` display runs in-process.
pub fn check_provider(inputs: &ProviderInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Dependency provider")];
    match inputs.dependency_provider {
        None | Some("") => {
            out.push(Record::skip("no dependency provider configured", None));
            return out;
        }
        Some("shdeps") => {}
        Some(other) => {
            out.push(Record::fail(
                "dependency provider is unsupported",
                Some(other.to_string()),
            ));
            return out;
        }
    }
    let policy = if inputs.policy.is_empty() {
        "pinned"
    } else {
        inputs.policy
    };
    if !inputs.configure_ok {
        out.push(Record::ok("Shdeps update policy", Some(policy.to_string())));
        out.push(Record::fail(
            "Shdeps provider is unavailable",
            Some("run dot update to bootstrap the reviewed provider release".to_string()),
        ));
        return out;
    }
    out.push(Record::ok("Shdeps update policy", Some(policy.to_string())));
    let development = format!("{}/shdeps", inputs.dev_dir);
    let mut development_invalid = false;
    if policy == "latest" && inputs.development_exists && !inputs.development_valid {
        development_invalid = true;
    }
    let installer = match &inputs.installer {
        Some(installer) => installer,
        None => {
            if development_invalid {
                out.push(Record::warn(
                    "Shdeps development checkout ignored",
                    Some(format!(
                        "verify its owner, modes, Git root, and cgraf78/shdeps origin: {}",
                        tilde(&development, inputs.home)
                    )),
                ));
            }
            out.push(Record::fail(
                "Shdeps provider is unavailable",
                Some("run dot update to bootstrap the reviewed provider release".to_string()),
            ));
            return out;
        }
    };
    if development_invalid && installer.source == "managed" {
        out.push(Record::warn(
            "Shdeps development checkout ignored",
            Some(format!(
                "verify its owner, modes, Git root, and cgraf78/shdeps origin: {}",
                tilde(&development, inputs.home)
            )),
        ));
    }
    match installer.source {
        "explicit" => {
            out.push(Record::ok(
                "Shdeps provider source",
                Some(format!(
                    "caller-selected reviewed installer: {}",
                    tilde(installer.path, inputs.home)
                )),
            ));
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        "pinned-dev" => {
            if policy == "latest" {
                out.push(Record::ok(
                    "Shdeps provider source",
                    Some(format!(
                        "development checkout selected by Dot lock: {}",
                        tilde(&development, inputs.home)
                    )),
                ));
            }
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        "latest-dev" => {
            out.push(Record::ok(
                "Shdeps provider source",
                Some(format!(
                    "trusted development checkout: {}",
                    tilde(&development, inputs.home)
                )),
            ));
        }
        "managed" => {
            if policy == "latest" {
                out.push(Record::ok(
                    "Shdeps provider source",
                    Some("managed release via reviewed bootstrap".to_string()),
                ));
            }
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        _ => {
            out.push(Record::fail(
                "Shdeps provider source is unavailable",
                Some("run dot update to restore provider selection metadata".to_string()),
            ));
            return out;
        }
    }
    if policy == "latest" && (installer.source == "pinned-dev" || installer.source == "latest-dev")
    {
        let locked = inputs.locked_revision.unwrap_or("");
        let current = inputs.development_revision.unwrap_or("");
        if !locked.is_empty() && current == locked {
            let short: String = current.chars().take(12).collect();
            out.push(Record::ok(
                "Shdeps development revision",
                Some(format!("matches Dot lock: {short}")),
            ));
        } else {
            let shown = if current.is_empty() {
                "<unavailable>"
            } else {
                current
            };
            out.push(Record::ok(
                "Shdeps development revision",
                Some(format!(
                    "trusted unpinned revision differs from Dot lock; accepted by latest policy: {shown}"
                )),
            ));
        }
    }
    if inputs.binary.is_none() {
        out.push(Record::fail(
            "Shdeps provider binary is unavailable",
            Some("run dot update to complete provider installation".to_string()),
        ));
        return out;
    }
    let expected = inputs.expected_abi.unwrap_or("");
    let actual = inputs.actual_abi.unwrap_or("");
    if !expected.is_empty() && actual == format!("abi:{expected}") {
        out.push(Record::ok("Shdeps provider ABI", Some(actual.to_string())));
    } else {
        let want = if expected.is_empty() {
            "<missing>"
        } else {
            expected
        };
        let found = if actual.is_empty() {
            "<unavailable>"
        } else {
            actual
        };
        out.push(Record::fail(
            "Shdeps provider ABI mismatch",
            Some(format!("expected abi:{want}, found {found}")),
        ));
    }
    out
}

/// `_dr_completed_identity_matches_home` (`doctor/repos.sh`):
/// whether the init-completed marker names this `$HOME` as both
/// worktree and git dir. `marker` is the resolved
/// `dot/init/completed` path (`None` when `dot_xdg_path` fails);
/// the marker must be a plain file (`-f && ! -L`) whose
/// `git_dir=`/`worktree=` lines (last wins, like the shell loop)
/// equal `$HOME/.git` and `$HOME`.
pub fn completed_identity_matches_home(marker: Option<&Path>, home: &str) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    let plain = std::fs::symlink_metadata(marker)
        .ok()
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_file() && !meta.file_type().is_symlink());
    if !plain {
        return false;
    }
    let content = std::fs::read(marker).unwrap_or_default();
    let mut git_dir = String::new();
    let mut worktree = String::new();
    for line in crate::repos_overlays::stream_lines(&content) {
        if let Some(value) = line.strip_prefix("git_dir=") {
            git_dir = value.to_string();
        } else if let Some(value) = line.strip_prefix("worktree=") {
            worktree = value.to_string();
        }
    }
    worktree == home && git_dir == format!("{home}/.git")
}

/// Run `git` for [`is_client_checkout`] with plain `git -C`:
/// stdout captured with `$(...)` newline stripping, stderr nulled,
/// stdin null (the [`crate::repos_base::run_git`] engine
/// boundary). `None` on spawn failure or non-zero exit.
fn git_capture(home: &Path, args: &[&str]) -> Option<String> {
    let full = [OsString::from("-C"), home.as_os_str().to_os_string()];
    let output = crate::repos_base::run_git(&full, args)?;
    if !output.status.success() {
        return None;
    }
    Some(captured(&String::from_utf8_lossy(&output.stdout)))
}

/// `_dr_is_client_checkout` (`doctor/repos.sh`): whether `$HOME`
/// is an ordinary checkout rooted at itself: `git -C HOME`
/// top-level resolves to the physical `$HOME`, and either the
/// init-completed identity matches or the local
/// `dot.clientRepository` flag reads `true`. `marker` is the
/// resolved `dot/init/completed` path (see
/// [`completed_identity_matches_home`]).
pub fn is_client_checkout(home: &Path, marker: Option<&Path>) -> bool {
    let root = match git_capture(home, &["rev-parse", "--show-toplevel"]) {
        Some(root) => root,
        None => return false,
    };
    let home_real = match std::fs::canonicalize(home) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let root_real = match std::fs::canonicalize(&root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if root_real != home_real {
        return false;
    }
    if completed_identity_matches_home(marker, &home.to_string_lossy()) {
        return true;
    }
    match git_capture(
        home,
        &["config", "--local", "--get", "dot.clientRepository"],
    ) {
        Some(value) => value == "true",
        None => false,
    }
}

/// Inputs for [`check_base_repo`]: the repository selector state
/// plus the client-checkout verdict as explicit data.
pub struct BaseRepoInputs<'a> {
    /// `DOT_BASE_TOPOLOGY`: `missing`, `separate`, `ordinary`, or
    /// (unrecognized, which the shell treats as existing with a
    /// failing `_base_git`, exit 128).
    pub topology: &'a str,
    /// `DOT_CLIENT_GIT_DIR` display path (separate topology only,
    /// but always carried like the shell global).
    pub client_git_dir: &'a str,
    /// `$HOME`: work tree, identity anchor, and tilde base.
    pub home: &'a str,
    /// `_dr_is_client_checkout` verdict (reused, not recomputed).
    pub is_client_checkout: bool,
}

/// The `_base_git` argv prefix for [`check_base_repo`]: `separate`
/// pins `--git-dir`/`--work-tree`, `ordinary` pins `-C $HOME`,
/// and anything else fails every `git` call (shell exit 128).
fn base_git_prefix(topology: &str, client_git_dir: &str, home: &str) -> Option<Vec<OsString>> {
    match topology {
        "separate" => Some(vec![
            OsString::from(format!("--git-dir={client_git_dir}")),
            OsString::from(format!("--work-tree={home}")),
        ]),
        "ordinary" => Some(vec![OsString::from("-C"), OsString::from(home)]),
        _ => None,
    }
}

/// One `_base_git` capture for [`check_base_repo`]: `None` on
/// spawn failure, non-zero exit, or unrecognized topology (the
/// shell `|| true` / `|| printf false` fallbacks apply at each
/// call site, not here).
fn base_git(topology: &str, client_git_dir: &str, home: &str, args: &[&str]) -> Option<String> {
    let prefix = base_git_prefix(topology, client_git_dir, home)?;
    let output = crate::repos_base::run_git(&prefix, args)?;
    if !output.status.success() {
        return None;
    }
    Some(captured(&String::from_utf8_lossy(&output.stdout)))
}

/// `_dr_check_base_repo` (`doctor/repos.sh`): client repository
/// health (layout identity, worktree resolution, tracked dirt,
/// HEAD, upstream distance). `git` runs in-process through the
/// mirrored `_base_git` dispatch; only
/// [`BaseRepoInputs::is_client_checkout`] is injected.
pub fn check_base_repo(inputs: &BaseRepoInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Client repository")];
    // `_base_repo_exists`: any topology but `missing`.
    if inputs.topology == "missing" {
        if inputs.is_client_checkout {
            out.push(Record::ok(
                "client checkout exists",
                Some("ordinary checkout rooted at $HOME".to_string()),
            ));
        } else {
            out.push(Record::fail(
                "client repository is missing",
                Some("run dot init REPOSITORY_URL".to_string()),
            ));
        }
        return out;
    }
    out.push(Record::ok(
        "client Git directory exists",
        Some(tilde(inputs.client_git_dir, inputs.home)),
    ));
    if inputs.topology == "ordinary" {
        out.push(Record::ok("ordinary client layout", None));
    } else {
        let is_bare = base_git(
            inputs.topology,
            inputs.client_git_dir,
            inputs.home,
            &["config", "--get", "core.bare"],
        )
        .unwrap_or_else(|| "false".to_string());
        let has_worktree = base_git(
            inputs.topology,
            inputs.client_git_dir,
            inputs.home,
            &["config", "--get", "core.worktree"],
        )
        .unwrap_or_default();
        if is_bare == "true" {
            out.push(Record::ok("legacy bare client layout", None));
        } else if !has_worktree.is_empty() {
            out.push(Record::ok(
                "explicit-worktree client layout",
                Some(tilde(&has_worktree, inputs.home)),
            ));
        } else {
            out.push(Record::fail(
                "client Git directory has no worktree identity",
                None,
            ));
        }
    }
    let resolved = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["rev-parse", "--show-toplevel"],
    )
    .unwrap_or_default();
    if resolved == inputs.home {
        out.push(Record::ok("client worktree resolves to $HOME", None));
    } else {
        let got = if resolved.is_empty() {
            "<missing>"
        } else {
            resolved.as_str()
        };
        out.push(Record::fail(
            "client worktree mismatch",
            Some(format!("expected {}, got {got}", inputs.home)),
        ));
    }
    // `status --porcelain | grep -cvE '^\?\?'`: only non-`??`
    // lines count; any `git` failure reads `0` through `|| true`.
    let dirty: usize = match base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["status", "--porcelain"],
    ) {
        Some(status) if !status.is_empty() => status
            .split('\n')
            .filter(|line| !line.starts_with("??"))
            .count(),
        _ => 0,
    };
    if dirty == 0 {
        out.push(Record::ok("no tracked client changes", None));
    } else {
        out.push(Record::warn(
            format!("{dirty} tracked client change(s)"),
            Some("run dot status to inspect".to_string()),
        ));
    }
    let head = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["symbolic-ref", "--short", "HEAD"],
    )
    .unwrap_or_default();
    if head.is_empty() {
        out.push(Record::warn("client HEAD is detached", None));
    } else {
        out.push(Record::ok("client HEAD on branch", Some(head)));
    }
    let upstream = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .unwrap_or_default();
    if upstream.is_empty() {
        out.push(Record::warn("client upstream is not configured", None));
        return out;
    }
    let counts = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{upstream}"),
        ],
    )
    .unwrap_or_default();
    // `IFS=$'\t' read -r ahead behind`: tab-separated, the
    // second variable keeping the remainder; no tab at all reads
    // both empty (the shell guards with `== *tab*` first).
    let (ahead, behind) = match counts.split_once('\t') {
        Some((ahead, behind)) => (ahead.to_string(), behind.to_string()),
        None => (String::new(), String::new()),
    };
    if is_uint(&ahead) && is_uint(&behind) {
        let ahead_count: u64 = ahead.parse().unwrap_or(0);
        let behind_count: u64 = behind.parse().unwrap_or(0);
        if ahead_count == 0 && behind_count == 0 {
            out.push(Record::ok(
                "client upstream",
                Some(format!("{upstream} (current)")),
            ));
        } else if ahead_count == 0 {
            out.push(Record::warn(
                "client is behind upstream",
                Some(format!("{upstream}: {behind} commit(s) behind")),
            ));
        } else if behind_count == 0 {
            out.push(Record::warn(
                "client is ahead of upstream",
                Some(format!("{upstream}: {ahead} commit(s) ahead")),
            ));
        } else {
            out.push(Record::warn(
                "client upstream has diverged",
                Some(format!("{upstream}: {ahead} ahead, {behind} behind")),
            ));
        }
    } else {
        out.push(Record::warn(
            "client upstream could not be compared",
            Some(upstream),
        ));
    }
    out
}
