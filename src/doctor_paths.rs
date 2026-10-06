//! Doctor path resolution and display.
//!
//! Owns the four path helpers formerly defined in
//! `lib/dot/doctor/paths.sh` — `_dr_physical_path`,
//! `_dr_symlink_target_path`, `_dr_symlink_points_to`, `_dr_tilde`.
//! Part 1 (`doctor_runtime`) owns the result lines and counters; this
//! module owns how section checks name filesystem locations.
//!
//! Parity decisions:
//! - `_dr_physical_path` canonicalizes the *directory* only (`cd`
//!   plus `pwd -P`) and appends the leaf verbatim, so symlinked
//!   parents resolve while the managed leaf keeps its identity. The
//!   output is a raw `dir/base` concatenation — `/` plus `foo` stays
//!   `//foo`, `/` plus `/` stays `///` — so the port concatenates
//!   bytes instead of joining `Path`s, which would collapse those
//!   corners.
//! - `readlink` reads one hop only (no `-f`): chains report the
//!   neighbor text, exactly like the shell.
//! - `_dr_symlink_points_to` conflates every failure (missing
//!   expected path, unreadable link, unresolvable side, mismatch)
//!   into one nonzero status, so Rust returns `bool`.
//! - `_dr_tilde` shares the `HOME` prefix rule with the hook API's
//!   `dot_doctor_display_path` but differs at the root: with `HOME=/`
//!   the private helper's `"$HOME"/*` pattern is literally `//*` and
//!   leaves `/foo` alone, while the public helper special-cases `/` and
//!   abbreviates `/foo` to `~/foo`. The display matrix pins both arms.
//! - Filesystem inputs travel as `&Path` (byte-exact on Unix, like
//!   the shell); the display helper takes `&str`, matching the
//!   crate's `xdg` precedent — display text is abbreviated, never
//!   probed on disk.
//! - Relative inputs resolve against the process working directory on
//!   both sides (`canonicalize` mirrors `cd` plus `pwd -P`), so the
//!   differential rows stay absolute; the empty-input corner (`""`
//!   means directory `.` with an empty leaf, printing `$PWD/`) falls
//!   out of the same code path by construction.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// Root marker, as bytes: the one path the trailing-slash strip must
/// not touch (`[[ "$path" != / && "$path" == */ ]]`).
const ROOT: &[u8] = b"/";

/// Current-directory marker, as bytes: the directory half of a
/// slash-free input (`dir=.`).
const DOT: &[u8] = b".";

/// Doctor path failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A directory, link, or expected path could not be resolved
    /// (shell `return 1`).
    Unresolvable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unresolvable => write!(f, "doctor path cannot be resolved"),
        }
    }
}

impl std::error::Error for Error {}

/// `_dr_physical_path`: resolve directory indirection without
/// dereferencing the final component.
///
/// Trailing slashes strip (except root `/` itself), the last `/`
/// splits directory from leaf (`a` means directory `.` with leaf
/// `a`; an empty directory half means `/`), a non-directory
/// directory fails, and otherwise the directory canonicalizes (`cd`
/// plus `pwd -P`) while the leaf appends verbatim — `dir/base` by
/// byte concatenation, so `/` plus `foo` stays `//foo`, exactly like
/// the shell's `printf '%s/%s'`. Returns [`Error::Unresolvable`]
/// where the shell returns 1.
pub fn physical_path(path: &Path) -> Result<PathBuf, Error> {
    let raw = path.as_os_str().as_bytes();
    let mut text = raw;
    while text != ROOT && text.last() == Some(&b'/') {
        text = &text[..text.len() - 1];
    }
    let (dir, base): (&[u8], &[u8]) = if text == ROOT {
        (ROOT, ROOT)
    } else if let Some(slash) = text.iter().rposition(|byte| *byte == b'/') {
        let dir = &text[..slash];
        (if dir.is_empty() { ROOT } else { dir }, &text[slash + 1..])
    } else {
        (DOT, text)
    };
    let dir_path = Path::new(OsStr::from_bytes(dir));
    if !dir_path.is_dir() {
        return Err(Error::Unresolvable);
    }
    let canonical = std::fs::canonicalize(dir_path).map_err(|_| Error::Unresolvable)?;
    let mut out = canonical.into_os_string();
    out.push("/");
    out.push(OsStr::from_bytes(base));
    Ok(PathBuf::from(out))
}

/// `_dr_symlink_target_path`: resolve one `readlink` hop, then
/// physicalize.
///
/// Fails where the shell returns 1: `link` is not a symlink (or is
/// unreadable), or the joined target does not physicalize. Absolute
/// targets physicalize directly; relative targets join onto the
/// link's directory (`${link%/*}`, `/` when that is empty, `.` when
/// the link has no slash) before physicalizing. Chains report the
/// neighbor text — only one hop reads, like `readlink` without `-f`.
/// Returns [`Error::Unresolvable`] where the shell returns 1.
pub fn symlink_target_path(link: &Path) -> Result<PathBuf, Error> {
    let target = std::fs::read_link(link).map_err(|_| Error::Unresolvable)?;
    if target.is_absolute() {
        return physical_path(&target);
    }
    let raw = link.as_os_str().as_bytes();
    let dir: &[u8] = match raw.iter().rposition(|byte| *byte == b'/') {
        None => DOT,
        Some(slash) => {
            let dir = &raw[..slash];
            if dir.is_empty() { ROOT } else { dir }
        }
    };
    let mut joint = OsStr::from_bytes(dir).to_os_string();
    joint.push("/");
    joint.push(target.as_os_str());
    physical_path(&PathBuf::from(joint))
}

/// `_dr_symlink_points_to`: whether `link` resolves to `expected`.
///
/// True only when the expected path exists (`-e`, links followed),
/// both sides physicalize, and the bytes match. Every other case —
/// missing expected path, unreadable link, unresolvable side,
/// mismatch — is false, matching the shell's single nonzero status.
pub fn symlink_points_to(link: &Path, expected: &Path) -> bool {
    if std::fs::metadata(expected).is_err() {
        return false;
    }
    let actual = match symlink_target_path(link) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let want = match physical_path(expected) {
        Ok(path) => path,
        Err(_) => return false,
    };
    actual.as_os_str() == want.as_os_str()
}

/// `_dr_tilde`: abbreviate `HOME` for display.
///
/// Exactly `~` for `HOME` itself, `~/rest` for paths under it, and
/// anything else verbatim. The prefix is the literal `HOME` plus
/// `/`, so with `HOME=/` only `//`-led paths take the second arm
/// and `/foo` passes through — the shell's `"$HOME"/*` pattern
/// behaves the same way. The hook API's `dot_doctor_display_path`
/// special-cases a `/` home instead.
pub fn tilde(path: &str, home: &str) -> String {
    // The abbreviation only ever cuts at the ASCII `/` after `home`, so
    // UTF-8 input stays UTF-8; the lossy conversion never replaces anything.
    String::from_utf8_lossy(&tilde_bytes(path.as_bytes(), home.as_bytes())).into_owned()
}

/// [`tilde`] over raw bytes, for paths that may not be UTF-8 (the runtime
/// and engine-source rows carry them verbatim). The one implementation every
/// doctor display path goes through.
pub fn tilde_bytes(path: &[u8], home: &[u8]) -> Vec<u8> {
    if path == home {
        return b"~".to_vec();
    }
    let mut prefix = home.to_vec();
    prefix.push(b'/');
    if let Some(rest) = path.strip_prefix(prefix.as_slice()) {
        let mut out = b"~/".to_vec();
        out.extend_from_slice(rest);
        return out;
    }
    path.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_abbreviates_home_prefix() {
        assert_eq!(tilde("/home/u", "/home/u"), "~");
        assert_eq!(tilde("/home/u/docs", "/home/u"), "~/docs");
        assert_eq!(tilde("/home/u2", "/home/u"), "/home/u2");
        assert_eq!(tilde("/etc", "/home/u"), "/etc");
        assert_eq!(tilde("rel/path", "/home/u"), "rel/path");
        assert_eq!(tilde("", "/home/u"), "");
    }

    #[test]
    fn tilde_root_home_keeps_single_slash_paths() {
        // `"$HOME"/*` with `HOME=/` is literally `//*`: `/foo` passes
        // through while `//foo` abbreviates. `dot_doctor_display_path`
        // differs here on purpose (`tests/doctor_paths.rs`).
        assert_eq!(tilde("/", "/"), "~");
        assert_eq!(tilde("/foo", "/"), "/foo");
        assert_eq!(tilde("//foo", "/"), "~/foo");
    }

    #[test]
    fn tilde_empty_home_matches_shell_glob() {
        // Empty `HOME` makes the second arm `/*`, so absolute paths
        // abbreviate; the equality arm still catches `""` itself.
        assert_eq!(tilde("", ""), "~");
        assert_eq!(tilde("/etc", ""), "~/etc");
        assert_eq!(tilde("rel", ""), "rel");
    }

    #[test]
    fn tilde_bytes_keeps_non_utf8_paths_verbatim() {
        assert_eq!(tilde_bytes(b"/home/u/\xff", b"/home/u"), b"~/\xff");
        assert_eq!(tilde_bytes(b"/srv/\xff", b"/home/u"), b"/srv/\xff");
        assert_eq!(tilde_bytes(b"/home/u", b"/home/u"), b"~");
    }
}
