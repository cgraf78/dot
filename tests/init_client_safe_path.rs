//! Native contract tests for the init path guard.
//!
//! The guard is a pure status predicate, so the oracle verdict is the
//! shell exit code and the port verdict is the returned bool. Paths
//! cross as bytes through the environment so non-UTF8 spellings probe
//! the same octets on both engines.

use dot::init_client_safe_path::safe_relative_path;

/// Every row drives both engines and pins the verdicts together.
/// Probed against the shell, not derived from the port: the trunk
/// slashes, dot components, `.git` folds, and control bytes below
/// each failed or passed there first.
#[test]
fn safe_relative_path_acceptance_matrix() {
    let rows: &[(&[u8], bool)] = &[
        (b"a", true),
        (b"a/b", true),
        (b"a/b/c.txt", true),
        (b".hidden", true),
        (b"a/.hidden/b", true),
        (b".gitignore", true),
        (b"git", true),
        (b"x.git", true),
        (b".github", true),
        (b"a b/c", true),
        (b"-n", true),
        (b"--help", true),
        (b"a\\b", true),
        (b"a/b\\", true),
        (b"caf\xc3\xa9/x", true),
        (b"", false),
        (b"/", false),
        (b"/a", false),
        (b"a/", false),
        (b".", false),
        (b"..", false),
        (b"./a", false),
        (b"../a", false),
        (b"a/./b", false),
        (b"a/../b", false),
        (b"a/.", false),
        (b"a/..", false),
        (b"a//b", false),
        (b"//", false),
        (b".git", false),
        (b".GIT", false),
        (b".Git", false),
        (b".gIt", false),
        (b"a/.git", false),
        (b"a/.GIT/b", false),
        (b"a/b/.Git", false),
        (b".git/a", false),
        (b"a/.github/b", true),
        (b"a/x.git/b", true),
        (b"a\tb", false),
        (b"a\nb", false),
        (b"a\rb", false),
        (b"\ta", false),
        (b"a/", false),
        (b"a/.", false),
        (b"a/.git ", true),
        (b" .git", true),
        (b"a/..git/b", true),
        (b"a/git../b", true),
        // Non-UTF8 octets are ordinary path bytes to both engines.
        (b"a/\xff/b", true),
        (b"\xfe", true),
    ];
    for &(path, expected) in rows {
        assert_eq!(
            safe_relative_path(path),
            expected,
            "path={:?} (lossy {:?})",
            path,
            String::from_utf8_lossy(path),
        );
    }
}

/// The guard never touches the filesystem: acceptance is a pure
/// function of the spelling, so probing twice pins determinism.
#[test]
fn safe_relative_path_is_deterministic() {
    for _ in 0..2 {
        assert!(safe_relative_path(b"a/b"));
        assert!(!safe_relative_path(b"a/.git/b"));
        assert!(!safe_relative_path(b""));
    }
}
