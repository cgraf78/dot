//! Ordered fragment-family discovery (slice 2 foundations).
//!
//! Ports `lib/dot/families.sh` exactly: the fragment-candidate filter
//! (regular files only, editor artifacts and dotfiles excluded),
//! caller-pattern filtering applied BEFORE `.replace` winner selection,
//! last-lexical-wins mutual exclusion inside immediate `<name>.replace/`
//! groups (one level deep), and the final `LC_ALL=C sort -u` stream of
//! `dir/key` paths. A missing family directory is a no-op so overlays
//! can add families incrementally.
//!
//! Byte-oriented throughout: the shell globs and sorts bytes under
//! `LC_ALL=C`, so keys sort and match as byte strings and listing
//! skips dotfiles exactly like an unexpanded `*` glob (no `dotglob`).
//!
//! Boundary: fragment names containing newlines are out of contract.
//! The shell stream is line-delimited, so a newline in a name corrupts
//! framing on that side (`dir/a` + `b` for key `a\nb`); Rust keeps the
//! name intact. Names that survive to the mutation boundary are
//! rejected there (`reserved.sh` refuses `\n` in paths).

use std::path::{Path, PathBuf};

use crate::glob;

/// Raw bytes of an `OsStr` for byte-oriented matching and sorting.
///
/// Unix (the only engine platform: lossless, the parity contract).
/// Elsewhere this falls back to lossy decoding so the crate still
/// compiles; output there is explicitly not byte-exact. Owned return
/// keeps both configurations behind one signature.
#[cfg(unix)]
fn os_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

/// Non-Unix fallback for [`os_bytes`]: lossy, never byte-exact.
#[cfg(not(unix))]
fn os_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// Rebuild a path from raw bytes (inverse of [`os_bytes`]).
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// Non-Unix fallback for [`path_from_bytes`]: lossy, never exact.
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Family discovery failure. The shell reports a missing directory
/// argument with exit 2 and treats every other hiccup (missing
/// directory, unreadable entries) as an empty stream; Rust mirrors
/// that split so only the arity case can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The required directory argument is missing (shell exit 2).
    Usage,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Usage => 2,
        }
    }
}

/// Editor-artifact and dotfile suffixes the shell rejects in `case`.
const REJECTED_PATTERNS: [&[u8]; 8] = [
    b".*",
    b"*~",
    b"*.tmp",
    b"*.tmp.*",
    b"*.bak",
    b"*.swp",
    b"*.swo",
    b"*.DS_Store",
];

/// Whether a fragment basename is consumable
/// (`_dot_family_is_file_candidate`'s name half; the caller checks
/// regular-file status, which needs the filesystem).
///
/// The shell matches the `case` arms in order, but they are disjoint
/// by construction, so any-match rejection is equivalent.
pub fn is_candidate_name(base: &[u8]) -> bool {
    !base.is_empty()
        && !REJECTED_PATTERNS
            .iter()
            .any(|pattern| glob::matches(pattern, base))
}

/// Whether a family-relative key survives the caller's optional shell
/// patterns (`_dot_family_key_matches`): no patterns means unfiltered.
pub fn key_matches(key: &[u8], patterns: &[&[u8]]) -> bool {
    patterns.is_empty() || patterns.iter().any(|pattern| glob::matches(pattern, key))
}

/// Whether a regular file (symlinks followed, like `[[ -f ]]`) with
/// this basename is a fragment candidate.
fn is_candidate_file(dir: &Path, base: &[u8]) -> bool {
    if !is_candidate_name(base) {
        return false;
    }
    let mut path = os_bytes(dir.as_os_str());
    path.push(b'/');
    path.extend_from_slice(base);
    std::fs::metadata(path_from_bytes(&path)).is_ok_and(|meta| meta.is_file())
}

/// Direct children of the family directory form aggregate layers.
/// Returns their relative keys (basenames passing the candidate and
/// pattern filters).
fn direct_keys(dir: &Path, patterns: &[&[u8]]) -> Vec<Vec<u8>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut keys = Vec::new();
    for entry in entries.flatten() {
        let base = os_bytes(&entry.file_name());
        let base = base.as_slice();
        // Hidden names never match the shell's `*` glob (`dotglob`
        // off); the candidate filter would reject dotfiles anyway,
        // but hidden `.replace` directories must not even compete.
        if base.first() == Some(&b'.') {
            continue;
        }
        if !is_candidate_file(dir, base) || !key_matches(base, patterns) {
            continue;
        }
        keys.push(base.to_vec());
    }
    keys
}

/// One selected key per non-empty `.replace` group: the last lexical
/// (byte-order) candidate whose `group/file` key passes the patterns.
/// Groups are immediate children only; deeper nesting belongs to the
/// consumer, not the structural policy.
fn replace_keys(dir: &Path, patterns: &[&[u8]]) -> Vec<Vec<u8>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut groups: Vec<Vec<u8>> = Vec::new();
    for entry in entries.flatten() {
        let base = os_bytes(&entry.file_name());
        let base = base.as_slice();
        if base.first() == Some(&b'.') || !base.ends_with(b".replace") {
            continue;
        }
        let mut group = os_bytes(dir.as_os_str());
        group.push(b'/');
        group.extend_from_slice(base);
        let group_path = path_from_bytes(&group);
        // `[[ -d ]]` follows symlinks, like the file check above.
        if !group_path.is_dir() {
            continue;
        }
        groups.push(base.to_vec());
    }
    let mut selected = Vec::new();
    for group in groups {
        let mut group_path = os_bytes(dir.as_os_str());
        group_path.push(b'/');
        group_path.extend_from_slice(&group);
        let group_dir = path_from_bytes(&group_path);
        let members = match std::fs::read_dir(&group_dir) {
            Ok(members) => members,
            Err(_) => continue,
        };
        let mut winner: Option<Vec<u8>> = None;
        for member in members.flatten() {
            let base = os_bytes(&member.file_name());
            let base = base.as_slice();
            if base.first() == Some(&b'.') {
                continue;
            }
            if !is_candidate_file(&group_dir, base) {
                continue;
            }
            let mut key = group.clone();
            key.push(b'/');
            key.extend_from_slice(base);
            if !key_matches(&key, patterns) {
                continue;
            }
            // Candidates arrive in directory order; keep the maximum
            // under byte order, which is the shell's "last of
            // `LC_ALL=C sort`" winner without buffering the stream.
            if winner.as_ref().is_none_or(|best| best.as_slice() < base) {
                winner = Some(base.to_vec());
            }
        }
        if let Some(winner) = winner {
            let mut key = group;
            key.push(b'/');
            key.extend_from_slice(&winner);
            selected.push(key);
        }
    }
    selected
}

/// The ordered source stream for a fragment family
/// (`dot_family_files` / `dot_family_files_matching`): direct keys
/// plus selected `.replace` winners, byte-sorted and deduplicated
/// (`LC_ALL=C sort -u`), each joined to the directory verbatim. A
/// missing (or unreadable) directory yields an empty stream.
pub fn family_files(dir: Option<&Path>, patterns: &[&[u8]]) -> Result<Vec<PathBuf>, Error> {
    let dir = dir.ok_or(Error::Usage)?;
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut keys = direct_keys(dir, patterns);
    keys.extend(replace_keys(dir, patterns));
    keys.sort();
    keys.dedup();
    let prefix = os_bytes(dir.as_os_str());
    Ok(keys
        .into_iter()
        .filter(|key| !key.is_empty())
        .map(|key| {
            let mut full = prefix.clone();
            full.push(b'/');
            full.extend_from_slice(&key);
            path_from_bytes(&full)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_names_reject_artifacts() {
        for good in ["hook.sh", "10-first", "a.b", "DS_Store-x", "~lead", "tmp"] {
            assert!(is_candidate_name(good.as_bytes()), "good: {good}");
        }
        for bad in [
            "",
            ".hidden",
            ".replace",
            "notes~",
            "frag.tmp",
            "frag.tmp.1",
            "old.bak",
            "x.swp",
            "y.swo",
            ".DS_Store",
        ] {
            assert!(!is_candidate_name(bad.as_bytes()), "bad: {bad}");
        }
    }

    #[test]
    fn key_patterns_filter_before_selection() {
        assert!(key_matches(b"a/b", &[]));
        assert!(key_matches(b"frag.sh", &[b"*.sh"]));
        assert!(key_matches(b"grp/frag.sh", &[b"grp/*"]));
        assert!(!key_matches(b"a/b", &[b"*.sh"]));
        assert!(!key_matches(b"a/b", &[b"*.conf"]));
        assert!(key_matches(b"a/b", &[b"*.conf", b"a/*"]));
    }

    #[test]
    fn missing_directory_is_an_empty_stream() {
        assert_eq!(
            family_files(Some(Path::new("/nonexistent-family-dir")), &[]),
            Ok(Vec::new())
        );
        assert_eq!(family_files(None, &[]), Err(Error::Usage));
        assert_eq!(Error::Usage.code(), 2);
    }
}
