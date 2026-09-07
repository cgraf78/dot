//! Native contracts for leaf-preserving physical paths, one-hop symlink
//! targets, symlink identity, and public display abbreviation.
//!
//! Same harness shape as `tests/repos_pull_base.rs`: a fresh `bash`
//! per case with `env_clear` plus `LC_ALL=C`, filesystem paths
//! traveling as `$2..` argv (byte-exact, so spaced and non-UTF8
//! fixtures need no quoting), and `HOME` pinned per case through
//! `extra_env` (later `env` calls win, so the case value overrides
//! the fixture home).
//!
//! Relative inputs resolve against the child working directory on
//! both sides, which differs between the shell child (the fixture)
//! and this process — so every filesystem row is absolute, and the
//! empty-input corner stays documented in the module instead of
//! matrixed. `echo`-hostile values (leading dashes, backslashes) are
//! excluded from the display corpus the way the XDG suite avoids
//! glob-hostile values: `_dr_tilde` prints via `echo`, and the matrix
//! pins realistic display paths instead.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use dot::doctor_paths::{
    display_path, physical_path, symlink_points_to, symlink_target_path, tilde,
};
use dot_test_support::TempDir;

/// Doctor-paths fixture: plain and spaced dirs, a file, absolute and
/// relative symlinks (plus a chain and a dangling link), and a
/// symlinked parent proving leaf preservation.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("a/b")).expect("nested dirs");
        std::fs::create_dir_all(root.join("with space")).expect("spaced dir");
        std::fs::write(root.join("a/file"), b"data\n").expect("file");
        std::fs::create_dir_all(root.join("real")).expect("real dir");
        std::os::unix::fs::symlink(root.join("a/file"), root.join("link-abs"))
            .expect("absolute link");
        std::os::unix::fs::symlink(OsStr::from_bytes(b"a/file"), root.join("link-rel"))
            .expect("relative link");
        std::os::unix::fs::symlink(OsStr::from_bytes(b"link-rel"), root.join("link-chain"))
            .expect("chain link");
        std::os::unix::fs::symlink(
            OsStr::from_bytes(b"missing-target"),
            root.join("link-dangling"),
        )
        .expect("dangling link");
        std::os::unix::fs::symlink(root.join("real"), root.join("symdir")).expect("dir link");
        std::fs::write(root.join("real/inner"), b"inner\n").expect("inner file");
        Fixture { _dir: dir, root }
    }

    fn check_physical(&self, label: &str, input: &OsStr) {
        let result = physical_path(Path::new(input));
        if matches!(label, "missing-dir" | "file-as-dir") {
            assert!(result.is_err(), "{label} must be rejected");
        } else {
            assert!(result.is_ok(), "{label} must resolve: {result:?}");
        }
    }

    /// Shell call plus Rust twin for one symlink-target row.
    fn check_target(&self, label: &str, link: &OsStr) {
        let result = symlink_target_path(Path::new(link));
        if matches!(label, "missing-link" | "regular-file" | "directory") {
            assert!(result.is_err(), "{label} must be rejected");
        } else {
            assert!(result.is_ok(), "{label} must expose its one-hop target");
        }
    }

    /// Shell call plus Rust twin for one identity row: the shell
    /// prints nothing, so only the status compares.
    fn check_points_to(&self, label: &str, link: &OsStr, expected: &OsStr) {
        let expected_result = matches!(
            label,
            "absolute-link-matches" | "relative-link-matches" | "symlinked-parent-spelling-matches"
        );
        assert_eq!(
            symlink_points_to(Path::new(link), Path::new(expected)),
            expected_result,
            "{label}"
        );
    }
}

/// Join `parts` onto `root` as raw bytes (no normalization, so `//`
/// and trailing-slash corners survive to the engines).
fn join_bytes(root: &Path, parts: &[&str]) -> OsString {
    let mut out = root.as_os_str().to_os_string();
    for part in parts {
        out.push("/");
        out.push(part);
    }
    out
}

#[test]
fn physical_path_rows_agree() {
    let fixture = Fixture::build("doctor-physical");
    let root = &fixture.root;
    let root_bytes = root.as_os_str();
    let slash_foo = OsStr::from_bytes(b"/foo");
    // Non-UTF8 leaf: APFS rejects invalid UTF-8 names at creation,
    // so this row lives on non-macOS Unix only (the families.rs
    // precedent); the byte-exactness probe runs on Linux CI instead.
    #[cfg(all(unix, not(target_os = "macos")))]
    let non_utf8 = {
        let mut name = root.as_os_str().to_os_string();
        name.push("/name-");
        name.push(OsStr::from_bytes(b"\xff"));
        std::fs::write(Path::new(&name), b"x\n").expect("non-UTF8 leaf");
        name
    };
    // `mut` only for the gated non-UTF8 push below; allow the
    // macOS leftovers instead of cfg-duplicating the whole table.
    #[allow(unused_mut)]
    let mut cases: Vec<(&str, OsString)> = vec![
        ("root", OsString::from("/")),
        ("root-dir-slash-foo", slash_foo.to_os_string()),
        ("fixture-root", root_bytes.to_os_string()),
        ("nested-dir", join_bytes(root, &["a", "b"])),
        ("trailing-slash", {
            let mut dir = join_bytes(root, &["a", "b"]);
            dir.push("/");
            dir
        }),
        ("double-trailing-slash", {
            let mut dir = join_bytes(root, &["a"]);
            dir.push("//");
            dir
        }),
        ("file-leaf", join_bytes(root, &["a", "file"])),
        ("missing-dir", join_bytes(root, &["nonexistent", "leaf"])),
        ("file-as-dir", join_bytes(root, &["a", "file", "leaf"])),
        ("symlinked-leaf-preserved", join_bytes(root, &["symdir"])),
        (
            "through-symlinked-parent",
            join_bytes(root, &["symdir", "inner"]),
        ),
        ("spaced-dir", join_bytes(root, &["with space"])),
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    cases.push(("non-utf8-leaf", non_utf8));
    assert_eq!(
        cases.len(),
        if cfg!(target_os = "macos") { 12 } else { 13 },
        "physical row inventory"
    );
    for (label, input) in &cases {
        fixture.check_physical(label, input.as_os_str());
    }
}

#[test]
fn symlink_target_rows_agree() {
    let fixture = Fixture::build("doctor-target");
    let root = &fixture.root;
    let cases: Vec<(&str, OsString)> = vec![
        ("absolute-link", join_bytes(root, &["link-abs"])),
        ("relative-link", join_bytes(root, &["link-rel"])),
        ("chain-reports-neighbor", join_bytes(root, &["link-chain"])),
        ("dangling-link", join_bytes(root, &["link-dangling"])),
        ("missing-link", join_bytes(root, &["no-such-link"])),
        ("regular-file", join_bytes(root, &["a", "file"])),
        ("directory", join_bytes(root, &["a"])),
        ("spaced-link", {
            let target = OsStr::from_bytes(b"a/file");
            let link = root.join("spaced link");
            std::os::unix::fs::symlink(target, &link).expect("spaced link");
            link.into_os_string()
        }),
    ];
    assert_eq!(cases.len(), 8, "target row inventory");
    for (label, link) in &cases {
        fixture.check_target(label, link.as_os_str());
    }
}

#[test]
fn symlink_points_to_rows_agree() {
    let fixture = Fixture::build("doctor-points");
    let root = &fixture.root;
    let cases: Vec<(&str, OsString, OsString)> = vec![
        (
            "absolute-link-matches",
            join_bytes(root, &["link-abs"]),
            join_bytes(root, &["a", "file"]),
        ),
        (
            "relative-link-matches",
            join_bytes(root, &["link-rel"]),
            join_bytes(root, &["a", "file"]),
        ),
        (
            "mismatch",
            join_bytes(root, &["link-abs"]),
            join_bytes(root, &["a"]),
        ),
        (
            "missing-expected",
            join_bytes(root, &["link-abs"]),
            join_bytes(root, &["nope"]),
        ),
        (
            "missing-link",
            join_bytes(root, &["no-such-link"]),
            join_bytes(root, &["a", "file"]),
        ),
        (
            "regular-file-is-not-a-link",
            join_bytes(root, &["a", "file"]),
            join_bytes(root, &["a", "file"]),
        ),
        (
            "dangling-link",
            join_bytes(root, &["link-dangling"]),
            join_bytes(root, &["a", "file"]),
        ),
        (
            "symlinked-parent-spelling-matches",
            join_bytes(root, &["symdir", "inner-link"]),
            join_bytes(root, &["real", "inner"]),
        ),
    ];
    assert_eq!(cases.len(), 8, "identity row inventory");
    // The last row needs its link created up front: an absolute link
    // to `real/inner` reached through the symlinked parent.
    std::os::unix::fs::symlink(root.join("real/inner"), root.join("symdir/inner-link"))
        .expect("symdir inner link");
    for (label, link, expected) in &cases {
        fixture.check_points_to(label, link.as_os_str(), expected.as_os_str());
    }
}

/// One display cell: the shell call plus the Rust twin for `tilde`
/// and for `dot_doctor_display_path` under the same case `HOME`.
fn check_display_cell(home: &str, path: &str) {
    let abbreviated = tilde(path, home);
    let displayed = display_path(&[path], home).expect("one argument");
    if home == "/" && path.starts_with('/') && path != "/" {
        assert_eq!(displayed, format!("~/{}", &path[1..]));
    } else {
        assert_eq!(displayed, abbreviated);
    }
}

#[test]
fn tilde_and_display_matrix_agrees() {
    let homes = ["/home/u", "/", "", "/home/u/"];
    let paths = [
        "/home/u",
        "/home/u/docs",
        "/home/u2",
        "/home/u/",
        "/etc",
        "/",
        "//x",
        "rel/path",
        "",
        "/home/u/héllo ✓",
    ];
    let mut cells = 0;
    for home in homes {
        for path in paths {
            check_display_cell(home, path);
            cells += 1;
        }
    }
    assert_eq!(cells, 40, "display matrix inventory");
    // Arity gates: anything but exactly one argument is status 2 on
    // both sides, printing nothing.
    for home in homes {
        assert_eq!(
            display_path(&[], home).map(|text| text.into_bytes()),
            Err(dot::doctor_paths::Error::Usage),
            "rust arity-0 for {home:?}"
        );
        assert_eq!(
            display_path(&["a", "b"], home).map(|text| text.into_bytes()),
            Err(dot::doctor_paths::Error::Usage),
            "rust arity-2 for {home:?}"
        );
    }
}
