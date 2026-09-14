//! Differential parity tests for the init path guard
//! (`lib/dot/init-client.sh` lines 27-45,
//! `_dot_init_safe_relative_path`) against the live shell.
//!
//! The guard is a pure status predicate, so the oracle verdict is the
//! shell exit code and the port verdict is the returned bool. Paths
//! cross as bytes through the environment so non-UTF8 spellings probe
//! the same octets on both engines.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::process::{Command, Stdio};

use dot::init_client_safe_path::safe_relative_path;
use dot::test_support::TempDir;

/// Shell prelude: only the init client itself. The guard calls the
/// sibling `_dot_init_safe_value` in the same file and nothing else.
const SOURCES: &str = ". \"$1/lib/dot/init-client.sh\"\n";

/// Run the live guard over `path` (via the environment, so tabs,
/// newlines, and non-UTF8 octets survive intact) and report whether
/// it accepted the spelling.
fn shell_accepts(path: &[u8]) -> bool {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path_var = OsString::from_vec(path.to_vec());
    let output = Command::new(dot::test_support::bash())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(
            "{SOURCES}_dot_init_safe_relative_path \"$DOT_TEST_PATH\"\ncode=$?\nprintf 'code=%s\\n' \"$code\"\n"
        ))
        .arg("dot-test-sh")
        .arg(repo)
        .env_clear()
        .env("LC_ALL", "C")
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_default(),
        )
        .env(
            "TMPDIR",
            std::env::var_os("TMPDIR")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| OsString::from("/tmp")),
        )
        .env("DOT_TEST_PATH", &path_var)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn bash");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("code="))
        .is_some_and(|code| code == "0")
}

/// Every row drives both engines and pins the verdicts together.
/// Probed against the shell, not derived from the port: the trunk
/// slashes, dot components, `.git` folds, and control bytes below
/// each failed or passed there first.
#[test]
fn safe_relative_path_matches_shell() {
    let _dir = TempDir::new("init-client-safe-path").expect("temp dir");
    let rows: &[&[u8]] = &[
        b"a",
        b"a/b",
        b"a/b/c.txt",
        b".hidden",
        b"a/.hidden/b",
        b".gitignore",
        b"git",
        b"x.git",
        b".github",
        b"a b/c",
        b"-n",
        b"--help",
        b"a\\b",
        b"a/b\\",
        b"caf\xc3\xa9/x",
        b"",
        b"/",
        b"/a",
        b"a/",
        b".",
        b"..",
        b"./a",
        b"../a",
        b"a/./b",
        b"a/../b",
        b"a/.",
        b"a/..",
        b"a//b",
        b"//",
        b".git",
        b".GIT",
        b".Git",
        b".gIt",
        b"a/.git",
        b"a/.GIT/b",
        b"a/b/.Git",
        b".git/a",
        b"a/.github/b",
        b"a/x.git/b",
        b"a\tb",
        b"a\nb",
        b"a\rb",
        b"\ta",
        b"a/",
        b"a/.",
        b"a/.git ",
        b" .git",
        b"a/..git/b",
        b"a/git../b",
        // Non-UTF8 octets are ordinary path bytes to both engines.
        b"a/\xff/b",
        b"\xfe",
    ];
    for path in rows {
        let expected = shell_accepts(path);
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
