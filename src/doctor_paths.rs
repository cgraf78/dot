//! Doctor path resolution and display (slice 42: doctor layer, part 2).
//!
//! Ports the second doctor family exactly: the four path helpers from
//! `lib/dot/doctor/paths.sh` — `_dr_physical_path`,
//! `_dr_symlink_target_path`, `_dr_symlink_points_to`, `_dr_tilde` —
//! plus the public display twin `dot_doctor_display_path` from
//! `lib/dot/doctor-api.sh`. Part 1 (`doctor_runtime`) owns the result
//! lines and counters; this module owns how section checks name
//! filesystem locations.
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
//! - `_dr_tilde` and `dot_doctor_display_path` share the `HOME`
//!   prefix rule but differ at the root: with `HOME=/` the private
//!   helper's `"$HOME"/*` pattern is literally `//*` and leaves
//!   `/foo` alone, while the public helper special-cases `/` and
//!   abbreviates `/foo` to `~/foo`. The differential matrix pins
//!   both arms.
//! - Filesystem inputs travel as `&Path` (byte-exact on Unix, like
//!   the shell); the two display helpers take `&str`, matching the
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

/// Doctor path failure, carrying the shell's exit code.
///
/// Both resolution failures surface as status 1; the display-arity
/// failure surfaces as status 2, mirroring `xdg::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A directory, link, or expected path could not be resolved
    /// (shell `return 1`).
    Unresolvable,
    /// Wrong argument count for the display helper (shell `return 2`).
    Usage,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Unresolvable => 1,
            Error::Usage => 2,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unresolvable => write!(f, "doctor path cannot be resolved"),
            Error::Usage => write!(f, "invalid doctor display-path arguments"),
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
/// behaves the same way. See [`display_path`] for the public twin,
/// which special-cases a `/` home instead.
pub fn tilde(path: &str, home: &str) -> String {
    if path == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = path.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    path.to_string()
}

/// `dot_doctor_display_path`: abbreviate `HOME` for display, with an
/// arity gate.
///
/// The single argument abbreviates like [`tilde`], except a `/` home
/// takes its own branch: `/` becomes `~` and any other absolute path
/// loses one leading slash behind `~/`, so `/foo` becomes `~/foo`
/// where [`tilde`] would leave it alone. Any other argument count is
/// [`Error::Usage`] (shell `return 2`).
pub fn display_path(args: &[&str], home: &str) -> Result<String, Error> {
    if args.len() != 1 {
        return Err(Error::Usage);
    }
    let path = args[0];
    if home == "/" {
        if path == "/" {
            return Ok("~".to_string());
        }
        if let Some(rest) = path.strip_prefix('/') {
            return Ok(format!("~/{rest}"));
        }
        return Ok(path.to_string());
    }
    Ok(tilde(path, home))
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
        // differs here on purpose (see below).
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
    fn display_root_home_abbreviates_absolutes() {
        assert_eq!(display_path(&["/"], "/"), Ok("~".to_string()));
        assert_eq!(display_path(&["/foo"], "/"), Ok("~/foo".to_string()));
        assert_eq!(display_path(&["//x"], "/"), Ok("~//x".to_string()));
        assert_eq!(display_path(&["rel/path"], "/"), Ok("rel/path".to_string()));
        assert_eq!(display_path(&[""], "/"), Ok(String::new()));
    }

    #[test]
    fn display_delegates_off_root() {
        // Away from `/`, the public helper is the private rule: every
        // row must equal `tilde`.
        for home in ["/home/u", "", "/home/u/"] {
            for path in [
                "/home/u",
                "/home/u/docs",
                "/home/u2",
                "/etc",
                "/",
                "rel/path",
                "",
            ] {
                assert_eq!(
                    display_path(&[path], home),
                    Ok(tilde(path, home)),
                    "display must equal tilde for home={home:?} path={path:?}"
                );
            }
        }
    }

    #[test]
    fn display_arity_is_usage() {
        assert_eq!(display_path(&[], "/home/u"), Err(Error::Usage));
        assert_eq!(display_path(&["a", "b"], "/home/u"), Err(Error::Usage));
        assert_eq!(Error::Usage.code(), 2);
        assert_eq!(Error::Unresolvable.code(), 1);
        assert!(!Error::Unresolvable.to_string().is_empty());
        assert!(!Error::Usage.to_string().is_empty());
    }
}
