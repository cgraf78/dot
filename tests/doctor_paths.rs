//! Native contracts for leaf-preserving physical paths, one-hop symlink
//! targets, symlink identity, and display abbreviation, plus the public
//! `dot_doctor_display_path` helper extensions call.
//!
//! Relative inputs resolve against the process working directory, so every
//! filesystem row is absolute and the empty-input corner stays documented
//! in the module instead of matrixed.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use dot::doctor_paths::{physical_path, symlink_points_to, symlink_target_path, tilde};
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

/// Run `dot_doctor_display_path` once per path under `home`, printing each
/// result (or `<status>` when the helper refuses) on its own line.
fn display_paths(home: &str, calls: &str, paths: &[&str]) -> Vec<String> {
    let script = format!(
        ". \"$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/doctor-api.sh\"\n\
         for path in \"$@\"; do {calls}; done\n"
    );
    let output = std::process::Command::new(dot_test_support::bash())
        .args(["--noprofile", "--norc", "-c", &script, "display"])
        .args(paths)
        .env_clear()
        .env("HOME", home)
        .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
        .env("LC_ALL", "C")
        .output()
        .expect("run dot_doctor_display_path");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .expect("utf8 display")
        .lines()
        .map(str::to_string)
        .collect()
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
    for home in homes {
        let displayed = display_paths(home, "dot_doctor_display_path \"$path\"", &paths);
        assert_eq!(displayed.len(), paths.len(), "home {home:?}");
        for (path, displayed) in paths.iter().zip(displayed) {
            // Off a `/` home the public helper is the private rule; at the
            // root it abbreviates every absolute path instead.
            let expected = if home == "/" && path.starts_with('/') && *path != "/" {
                format!("~/{}", &path[1..])
            } else {
                tilde(path, home)
            };
            assert_eq!(displayed, expected, "home {home:?} path {path:?}");
        }
        // Anything but exactly one argument is status 2, printing nothing.
        assert_eq!(
            display_paths(
                home,
                "dot_doctor_display_path || echo \"<$?>\"; dot_doctor_display_path a b || echo \"<$?>\"",
                &["once"],
            ),
            ["<2>", "<2>"],
            "arity for home {home:?}"
        );
    }
}
