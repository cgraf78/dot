//! Worker decision kernels from `lib/dot/extension-worker.sh`.
//!
//! Ports the pure validation logic behind the four worker entry
//! points: the overlay-protocol whitelist from
//! `_dot_extension_worker_load_overlay_protocol`, the ordered API
//! file lists from `_dot_extension_worker_load_merge_api` and
//! `_dot_extension_worker_load_doctor_api`, and the argument,
//! source-root, result-path, retiring-set, and entry-point checks
//! from `_dot_extension_worker_main`.
//!
//! Sourcing and execution stay shell-side like other exec
//! boundaries (the hook-sourcing precedent in [`crate::merges`]):
//! the loaders source their files and unset non-protocol helpers,
//! and `_dot_extension_worker_main` sources client code, consumes
//! the one-use overlay context, and runs the lifecycle entry point
//! under traps and readonly guards. Only the decisions that can be
//! tested without side effects live here.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::path::{Path, PathBuf};

/// Worker mode selected by the first argument to
/// `_dot_extension_worker_main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `merge` mode, running the `merge` entry point.
    Merge,
    /// `pre-sync` mode, running the `prepare` entry point.
    PreSync,
    /// `deactivate` mode, running the `deactivate` entry point.
    Deactivate,
    /// `doctor` mode, running the `doctor` entry point.
    Doctor,
}

impl Mode {
    /// Parse a mode word, like `case $mode in merge | pre-sync |
    /// deactivate | doctor)`. Anything else is not a mode.
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "merge" => Some(Mode::Merge),
            "pre-sync" => Some(Mode::PreSync),
            "deactivate" => Some(Mode::Deactivate),
            "doctor" => Some(Mode::Doctor),
            _ => None,
        }
    }

    /// Canonical spelling of the mode, for diagnostics and dispatch.
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Merge => "merge",
            Mode::PreSync => "pre-sync",
            Mode::Deactivate => "deactivate",
            Mode::Doctor => "doctor",
        }
    }

    /// Lifecycle entry point for the mode: `merge` runs `merge`,
    /// `pre-sync` runs `prepare`, `deactivate` runs `deactivate`,
    /// and `doctor` runs `doctor`.
    pub fn entry_point(&self) -> &'static str {
        match self {
            Mode::Merge => "merge",
            Mode::PreSync => "prepare",
            Mode::Deactivate => "deactivate",
            Mode::Doctor => "doctor",
        }
    }
}

/// Silent worker failure: `Usage` is a caller-arity problem (shell
/// exit 2), `Refused` a failed check (shell exit 1). Neither prints;
/// callers report their own warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Wrong argument count or unknown mode (shell exit 2).
    Usage,
    /// Failed source-root, result-path, or set check (shell exit 1).
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
    /// Silent failures render as empty, like the shell.
    fn fmt(&self, _formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage | Error::Refused => Ok(()),
        }
    }
}

/// Canonical read-only authority/identity helpers retained by
/// `_dot_extension_worker_load_overlay_protocol`; every other
/// repository helper sourced from `repos/config.sh` and
/// `repos/overlays.sh` is unset before client code loads.
pub const OVERLAY_PROTOCOL_KEEP: [&str; 8] = [
    "_overlay_link_target",
    "_overlay_private_regular_file",
    "_overlay_parse_manifest_record",
    "_overlay_manifest_safe",
    "_overlay_is_worktree",
    "_overlay_effective_url",
    "_overlay_origin_matches",
    "_overlay_checkout_matches",
];

/// Whether `name` survives the protocol load, like the shell `case`
/// over the eight retained helpers.
pub fn overlay_protocol_keep(name: &str) -> bool {
    OVERLAY_PROTOCOL_KEEP.contains(&name)
}

/// Survivors of the protocol load in `after` order: pre-existing
/// functions always survive, while newly sourced helpers survive
/// only through the whitelist. Mirrors the loader loop that skips
/// `existing_functions` entries and unsets every other non-listed
/// name.
pub fn protocol_survivors(before: &[String], after: &[String]) -> Vec<String> {
    let mut survivors = Vec::new();
    for name in after {
        if before.iter().any(|known| known == name) || overlay_protocol_keep(name) {
            survivors.push(name.clone());
        }
    }
    survivors
}

/// Ordered files sourced by `_dot_extension_worker_load_merge_api`,
/// relative to `$DOT_SOURCE_ROOT`.
pub const MERGE_API_RELPATHS: [&str; 6] = [
    "lib/dot/log.sh",
    "lib/dot/temp.sh",
    "lib/dot/merge-block.sh",
    "lib/dot/families.sh",
    "lib/dot/merge-hooks.sh",
    "lib/dot/hook-api.sh",
];

/// File sourced by `_dot_extension_worker_load_doctor_api`,
/// relative to `$DOT_SOURCE_ROOT`.
pub const DOCTOR_API_RELPATH: &str = "lib/dot/doctor-api.sh";

/// Join the merge-API list onto `source_root`, like the six `.`
/// lines in the loader (`"$DOT_SOURCE_ROOT/<rel>"` string
/// concatenation, so an empty root yields `/lib/...` like the
/// shell).
pub fn merge_api_paths(source_root: &str) -> Vec<PathBuf> {
    MERGE_API_RELPATHS
        .iter()
        .map(|rel| PathBuf::from(format!("{source_root}/{rel}")))
        .collect()
}

/// Join the doctor-API file onto `source_root`, like the loader.
pub fn doctor_api_path(source_root: &str) -> PathBuf {
    PathBuf::from(format!("{source_root}/{DOCTOR_API_RELPATH}"))
}

/// Whether `source_root` has the `/*` shape from
/// `_dot_extension_worker_main` (`${DOT_SOURCE_ROOT:-}` must start
/// with `/`; empty fails).
pub fn source_root_shape_ok(source_root: &str) -> bool {
    !source_root.is_empty() && source_root.starts_with('/')
}

/// A directory that is not itself a symlink, like
/// `[[ -d $path && ! -L $path ]]`.
fn dir_not_link(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.is_dir() && !meta.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Whether `$source_root/lib/dot` is a real directory, like the
/// `[[ -d $DOT_SOURCE_ROOT/lib/dot && ! -L $DOT_SOURCE_ROOT/lib/dot ]]`
/// gate (string concatenation, so an empty root probes `/lib/dot`
/// like the shell).
pub fn lib_dot_dir_ok(source_root: &str) -> bool {
    dir_not_link(&PathBuf::from(format!("{source_root}/lib/dot")))
}

/// Whether `source_root` passes both main gates: the `/*` shape and
/// the `lib/dot` directory check.
pub fn source_root_valid(source_root: &str) -> bool {
    source_root_shape_ok(source_root) && lib_dot_dir_ok(source_root)
}

/// Whether the engine result path is non-empty, like
/// `[[ -n $_dot_extension_worker_result ]]`.
pub fn result_path_valid(result: &str) -> bool {
    !result.is_empty()
}

/// Whether the deactivate retiring set holds, like
/// `[[ $REPLY_SET_KIND == retiring && ${#OVERLAYS[@]} -eq 1 ]]`.
pub fn deactivate_set_valid(set_kind: &str, overlay_count: usize) -> bool {
    set_kind == "retiring" && overlay_count == 1
}

/// Combined early precheck mirroring `_dot_extension_worker_main`:
/// exactly five arguments (shell `$#`), a known mode, a valid
/// source root, and a non-empty result path. Arity and mode map to
/// `Usage` (shell exit 2); source-root and result failures map to
/// `Refused` (shell exit 1), in shell check order.
pub fn main_precheck(
    argc: usize,
    mode: &str,
    source_root: &str,
    result: &str,
) -> Result<Mode, Error> {
    if argc != 5 {
        return Err(Error::Usage);
    }
    let parsed = match Mode::parse(mode) {
        Some(parsed) => parsed,
        None => return Err(Error::Usage),
    };
    if !source_root_valid(source_root) {
        return Err(Error::Refused);
    }
    if !result_path_valid(result) {
        return Err(Error::Refused);
    }
    Ok(parsed)
}
