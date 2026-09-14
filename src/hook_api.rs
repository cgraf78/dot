//! Public extension hook API (`lib/dot/hook-api.sh`).
//!
//! Thin worker-facing surface over the engine modules that already own
//! each behavior. Nothing here reimplements shell logic: every item
//! delegates to its owner and only adds the hook-API arity mapping or
//! the environment glue the shell reads from worker globals.
//!
//! Ownership map (`hook-api.sh` function to Rust owner):
//!
//! | Shell function | Rust owner |
//! |---|---|
//! | `dot_hook_file`, `dot_hook_source` | [`hook_file`], [`hook_source_path`] (shape gate here, trust check in [`crate::extension_trust`]) |
//! | `dot_hook_family`, `dot_hook_family_files`, `dot_hook_family_files_matching`, `dot_hook_family_relpath`, `dot_hook_family_marker_name` | [`hook_family_dir`], [`hook_family_files`], [`hook_family_files_matching`], [`hook_family_relpath`], [`hook_family_marker_name`] over [`crate::merge_hooks`] and [`crate::families`] |
//! | `dot_family_relpath` | [`family_relpath`] (identical twin of the hook variant, like the shell) |
//! | `dot_expand_home` | [`expand_home`] ([`crate::merge_hooks::expand_home`]) |
//! | `dot_sibling_tmp_for` | [`crate::temp::sibling_tmp_for`] |
//! | `dot_write_text_if_changed`, `dot_commit_tmp` | [`crate::merge_hooks::write_text_if_changed`], [`crate::merge_hooks::commit_tmp`] |
//! | `dot_file_generation`, `dot_commit_tmp_if_generation`, `dot_remove_if_generation` | [`crate::temp::file_generation`], [`crate::temp::commit_tmp_if_generation`], [`crate::temp::remove_if_generation`] |
//! | `dot_json_available`, `dot_json_layer` | [`crate::merge_hooks::jq_available`], [`crate::merge_hooks::jq_layer`] |
//! | `dot_managed_block_build`, `dot_managed_block_strip`, `dot_managed_block_strip_family`, `dot_managed_block_merge`, `dot_managed_block_merge_family` | [`crate::merge_block`] (`build`, `strip`, `strip_family`, `merge`, `merge_family`) |
//! | `dot_tool_present` | [`crate::platform::tool_present`] ([`tool_present_live`] reads `PATH` like the shell) |
//! | `dot_hook_platform_match`, `dot_hook_host_match` | [`hook_platform_match`], [`hook_host_match`] ([`crate::platform`] owns detection and spec matching) |
//! | `dot_hook_log`, `dot_hook_warn` | [`crate::log::Log`] (`log` / `warn`) |
//!
//! The shell reports wrong arity with exit 2 and failed checks with
//! exit 1; this layer reuses the owner error types, which carry the
//! same split (`Usage` for exit 2, refusal/unavailable for exit 1).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::{extension_trust, families, merge_hooks, platform};

/// Raw bytes of an `OsStr` for the byte-oriented shape gate (the
/// shell matches `case` patterns bytewise under `LC_ALL=C`).
#[cfg(unix)]
fn os_bytes(name: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    name.as_bytes().to_vec()
}

/// Non-Unix fallback for [`os_bytes`]: lossy, never byte-exact (the
/// engine is Unix-only, like the shell this ports).
#[cfg(not(unix))]
fn os_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// `dot_hook_file` shape gate: the shell `case` arms on the relative
/// path, byte for byte. Empty, absolute, dot-only, `./`/`../`
///-prefixed, `/./` or `/../` segments, trailing slash or dot
/// segments, doubled slashes, and embedded CR/LF are all usage
/// errors (shell exit 2); anything else is a well-formed relative
/// candidate the trust check then accepts or refuses.
pub fn relative_valid(relative: &OsStr) -> bool {
    let bytes = os_bytes(relative);
    if bytes.is_empty() || bytes.starts_with(b"/") {
        return false;
    }
    if bytes == b"." || bytes == b".." {
        return false;
    }
    if bytes.starts_with(b"./") || bytes.starts_with(b"../") {
        return false;
    }
    if bytes.ends_with(b"/") || bytes.ends_with(b"/.") || bytes.ends_with(b"/..") {
        return false;
    }
    let text = &bytes[..];
    if contains(text, b"//") || contains(text, b"/./") || contains(text, b"/../") {
        return false;
    }
    !bytes.iter().any(|byte| matches!(byte, b'\n' | b'\r'))
}

/// Byte-substring search for [`relative_valid`].
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && (0..=(haystack.len() - needle.len()))
            .any(|at| &haystack[at..at + needle.len()] == needle)
}

/// `dot_hook_file`: resolve one validated client support file without
/// sourcing it. A malformed relative is a usage error (shell exit 2);
/// an unsafe or unavailable file is refused (shell exit 1). The join
/// spells the shell `$DOT_EXTENSIONS_DIR/$relative` through
/// [`extension_trust::Inputs::extensions_dir`], and the trust check
/// is [`extension_trust::file_validate`] outright.
pub fn hook_file(
    relative: &OsStr,
    inputs: &extension_trust::Inputs,
    overlays: &[String],
) -> Result<PathBuf, extension_trust::Error> {
    if !relative_valid(relative) {
        return Err(extension_trust::Error::Usage);
    }
    let path = Path::new(&inputs.extensions_dir).join(relative);
    if extension_trust::file_validate(&path, inputs, overlays) {
        Ok(path)
    } else {
        Err(extension_trust::Error::Refused)
    }
}

/// `dot_hook_source` path half: the same validated resolution as
/// [`hook_file`]. Sourcing itself stays shell-side — the worker
/// sources the file into its own global scope with emptied
/// positional parameters and `readonly` path locals, none of which
/// has a Rust spelling — so the engine resolves here and the worker
/// sources the returned path.
pub fn hook_source_path(
    relative: &OsStr,
    inputs: &extension_trust::Inputs,
    overlays: &[String],
) -> Result<PathBuf, extension_trust::Error> {
    hook_file(relative, inputs, overlays)
}

/// `_merge_hook_family` as seen through the hook API
/// (`dot_hook_family`): join one family name to the hooks root. The
/// shell validates nothing here (callers pass configured names), so
/// neither does this; the root itself comes from
/// [`merge_hooks::hook_dir`].
pub fn hook_family_dir(hooks_root: &Path, family: &OsStr) -> PathBuf {
    hooks_root.join(family)
}

/// `dot_hook_family_files`: the ordered aggregate and `.replace`
/// winner stream for one client family under the hooks root.
pub fn hook_family_files(
    hooks_root: &Path,
    family: &OsStr,
) -> Result<Vec<PathBuf>, families::Error> {
    let dir = hook_family_dir(hooks_root, family);
    families::family_files(Some(&dir), &[])
}

/// `dot_hook_family_files_matching`: the family stream filtered by
/// caller shell patterns over the family-relative path, before
/// `.replace` winner selection (patterns owned by
/// [`crate::families`]).
pub fn hook_family_files_matching(
    hooks_root: &Path,
    family: &OsStr,
    patterns: &[&[u8]],
) -> Result<Vec<PathBuf>, families::Error> {
    let dir = hook_family_dir(hooks_root, family);
    families::family_files(Some(&dir), patterns)
}

/// `dot_hook_family_relpath` (and its identical twin
/// `dot_family_relpath` via [`family_relpath`]): strip the
/// `family/` prefix, or return the path unchanged when it lies
/// outside the family (the shell `${file#"$dir/"}`).
pub fn hook_family_relpath(hooks_root: &Path, family: &OsStr, file: &Path) -> OsString {
    let dir = hook_family_dir(hooks_root, family);
    merge_hooks::family_relpath(&dir, file)
}

/// `dot_hook_family_marker_name`: the family-relative path with
/// slashes flattened to underscores for marker comments.
pub fn hook_family_marker_name(hooks_root: &Path, family: &OsStr, file: &Path) -> OsString {
    let relative = hook_family_relpath(hooks_root, family, file);
    merge_hooks::family_marker_name(&relative)
}

/// `dot_family_relpath`: the same `${file#"$dir/"}` strip against an
/// explicit family directory (the shell routes both spellings
/// through `_merge_hook_family_relpath`).
pub fn family_relpath(family_dir: &Path, file: &Path) -> OsString {
    merge_hooks::family_relpath(family_dir, file)
}

/// `dot_expand_home`: the documented `${HOME}` / `$HOME` / `~`
/// expansion ([`merge_hooks::expand_home`]).
pub fn expand_home(value: &str, home: &str) -> String {
    merge_hooks::expand_home(value, home)
}

/// Whether `PREFIX` selects the Termux dual identity: non-empty and
/// containing `/com.termux/`, exactly the shell
/// `[[ -n ${PREFIX:-} && $PREFIX == */com.termux/* ]]` (`*` matches
/// `/` in bash globs, so containment is the whole test).
pub fn is_termux(prefix: &str) -> bool {
    !prefix.is_empty() && prefix.contains("/com.termux/")
}

/// `dot_hook_platform_match`: match one spec against the current
/// normalized platform, plus `android` under Termux. A missing spec
/// is a usage error (shell exit 2); matching itself is literal
/// ([`platform::platform_matches`]).
pub fn hook_platform_match(
    spec: Option<&str>,
    plat: &str,
    prefix: &str,
) -> Result<bool, platform::Error> {
    platform::platform_matches(spec, plat, is_termux(prefix))
}

/// `dot_hook_host_match`: match one spec against the lowercase
/// current host (callers lowercase via [`platform::host_name`], like
/// the shell `_dot_hook_host`). A missing spec is a usage error
/// (shell exit 2).
pub fn hook_host_match(spec: Option<&str>, host: &str) -> Result<bool, platform::Error> {
    platform::host_matches(spec, host)
}

/// Live `dot_hook_platform_match`: `PREFIX` from the environment,
/// platform from [`platform::detect_platform`], then
/// [`hook_platform_match`]. Detection failure is
/// [`platform::Error::Unavailable`] (shell exit 1).
pub fn live_platform_match(spec: Option<&str>) -> Result<bool, platform::Error> {
    let prefix = std::env::var("PREFIX").unwrap_or_default();
    let detected = platform::detect_platform()?;
    hook_platform_match(spec, &detected, &prefix)
}

/// Live `dot_hook_host_match`: host from [`platform::detect_host`]
/// (already lowercased), then [`hook_host_match`]. Detection failure
/// is [`platform::Error::Unavailable`] (shell exit 1).
pub fn live_host_match(spec: Option<&str>) -> Result<bool, platform::Error> {
    let detected = platform::detect_host()?;
    hook_host_match(spec, &detected)
}

/// Live `dot_tool_present`: `PATH` from the environment, then
/// [`platform::tool_present`]. Only external commands resolve here;
/// shell builtins and functions satisfy the shell `command -v` but
/// are out of contract (the [`platform::tool_present`] boundary).
pub fn tool_present_live(name: Option<&str>) -> Result<bool, platform::Error> {
    let path = std::env::var("PATH").unwrap_or_default();
    platform::tool_present(name, &path)
}
