//! Differential parity tests for `src/doctor_paths.rs` against the live
//! shell (`lib/dot/doctor/paths.sh` plus `dot_doctor_display_path` from
//! `lib/dot/doctor-api.sh`): leaf-preserving physical paths, one-hop
//! symlink targets, symlink-identity checks, and the two tilde
//! abbreviators (whose `HOME=/` arms deliberately differ).
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
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::doctor_paths::{
    display_path, physical_path, symlink_points_to, symlink_target_path, tilde,
};
use dot::test_support::TempDir;

/// Sources for the doctor-paths cluster.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/doctor/paths.sh\"\n",
    ". \"$1/lib/dot/doctor-api.sh\"\n",
);

/// Run one shell snippet with the paths runtime sourced. `argv`
/// arrives as `$2..` (byte-exact, for non-UTF8 paths); `extra_env`
/// sets (`Some`) or removes (`None`) variables after the hermetic
/// base (so a case `HOME` overrides the fixture home).
fn shell_run(
    home: &Path,
    cwd: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Rust twin of a printing shell call: `Ok` prints the value plus a
/// newline with status 0, `Err` prints nothing with the coded status.
fn rust_printed(result: Result<impl AsRef<[u8]>, dot::doctor_paths::Error>) -> (i32, Vec<u8>) {
    match result {
        Ok(value) => {
            let mut out = value.as_ref().to_vec();
            out.push(b'\n');
            (0, out)
        }
        Err(err) => (err.code(), Vec::new()),
    }
}

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

    /// Shell call plus Rust twin for one physical-path row; both
    /// sides must agree on status and stdout bytes, silently.
    fn check_physical(&self, label: &str, input: &OsStr) {
        let argv = [input];
        let (code, out, err) = shell_run(
            &self.root,
            &self.root,
            &argv,
            &[],
            "_dr_physical_path \"$2\"",
        );
        assert!(err.is_empty(), "shell must stay silent for {label}");
        let rust = rust_printed(
            physical_path(Path::new(input)).map(|path| path.into_os_string().into_vec()),
        );
        assert_eq!((code, out), (rust.0, rust.1), "physical parity for {label}");
    }

    /// Shell call plus Rust twin for one symlink-target row.
    fn check_target(&self, label: &str, link: &OsStr) {
        let argv = [link];
        let (code, out, err) = shell_run(
            &self.root,
            &self.root,
            &argv,
            &[],
            "_dr_symlink_target_path \"$2\"",
        );
        assert!(err.is_empty(), "shell must stay silent for {label}");
        let rust = rust_printed(
            symlink_target_path(Path::new(link)).map(|path| path.into_os_string().into_vec()),
        );
        assert_eq!((code, out), (rust.0, rust.1), "target parity for {label}");
    }

    /// Shell call plus Rust twin for one identity row: the shell
    /// prints nothing, so only the status compares.
    fn check_points_to(&self, label: &str, link: &OsStr, expected: &OsStr) {
        let argv = [link, expected];
        let (code, out, err) = shell_run(
            &self.root,
            &self.root,
            &argv,
            &[],
            "_dr_symlink_points_to \"$2\" \"$3\"",
        );
        assert!(out.is_empty(), "identity prints nothing for {label}");
        assert!(err.is_empty(), "shell must stay silent for {label}");
        let rust = symlink_points_to(Path::new(link), Path::new(expected));
        assert_eq!(rust, code == 0, "identity parity for {label}");
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
    let fixture_home = TempDir::new("doctor-display-home").expect("home dir");
    let cwd = fixture_home.path();
    let path_arg = OsStr::new(path);
    let argv = [path_arg];
    let env = [("HOME", Some(home))];
    let (tilde_code, tilde_out, tilde_err) = shell_run(cwd, cwd, &argv, &env, "_dr_tilde \"$2\"");
    assert!(tilde_err.is_empty(), "tilde must stay silent");
    let mut want_tilde = tilde(path, home).into_bytes();
    want_tilde.push(b'\n');
    assert_eq!(
        (tilde_code, tilde_out),
        (0, want_tilde),
        "tilde parity for home={home:?} path={path:?}"
    );
    let (display_code, display_out, display_err) =
        shell_run(cwd, cwd, &argv, &env, "dot_doctor_display_path \"$2\"");
    assert!(display_err.is_empty(), "display must stay silent");
    let rust_display = rust_printed(display_path(&[path], home).map(|text| text.into_bytes()));
    assert_eq!(
        (display_code, display_out),
        (rust_display.0, rust_display.1),
        "display parity for home={home:?} path={path:?}"
    );
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
        let cwd = TempDir::new("doctor-arity-home").expect("home dir");
        let (code, out, _) = shell_run(
            cwd.path(),
            cwd.path(),
            &[],
            &[("HOME", Some(home))],
            "dot_doctor_display_path",
        );
        assert_eq!((code, out), (2, Vec::new()), "arity-0 for {home:?}");
        assert_eq!(
            display_path(&[], home).map(|text| text.into_bytes()),
            Err(dot::doctor_paths::Error::Usage),
            "rust arity-0 for {home:?}"
        );
        let two = [OsStr::new("a"), OsStr::new("b")];
        let (code, out, _) = shell_run(
            cwd.path(),
            cwd.path(),
            &two,
            &[("HOME", Some(home))],
            "dot_doctor_display_path \"$2\" \"$3\"",
        );
        assert_eq!((code, out), (2, Vec::new()), "arity-2 for {home:?}");
        assert_eq!(
            display_path(&["a", "b"], home).map(|text| text.into_bytes()),
            Err(dot::doctor_paths::Error::Usage),
            "rust arity-2 for {home:?}"
        );
    }
}
