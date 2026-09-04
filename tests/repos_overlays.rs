//! Differential parity tests for the `repos/overlays.sh` manifest
//! subset against the live shell: link-target derivation, manifest
//! record parsing, and the safety gates — including the unreadable
//! fail-open quirk and bash redirect noise.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the manifest library sourced.
fn shell_run(home: &Path, argv: &[&std::ffi::OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(". \"$1/lib/dot/repos/overlays.sh\"\n{snippet}"));
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
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Current euid for ownership-gated checks.
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

#[test]
fn link_target_agrees() {
    let dir = TempDir::new("ovlink-target").expect("fixture dir");
    let home = dir.path();
    // (rel, name): one `../` per slash, dots and empties intact.
    for (rel, name) in [
        ("app.conf", "web"),
        ("a/b.conf", "web"),
        ("a/b/c.conf", "web"),
        (".config/app", "x"),
        ("", "x"),
        ("a", ""),
    ] {
        let (code, out, serr) = shell_run(
            home,
            &[rel.as_ref(), name.as_ref()],
            "_overlay_link_target \"$2\" \"$3\"; printf 'rc=%s\\nreply=%s\\n' \"$?\" \"$REPLY\"",
        );
        assert_eq!(code, 0, "shell harness link {rel:?}");
        assert_eq!(serr, b"", "link stderr for {rel:?}");
        let rust = repos_overlays::link_target(rel, name);
        assert_eq!(
            format!("rc=0\nreply={rust}\n"),
            String::from_utf8(out).expect("link dump"),
            "link target for {rel:?}/{name:?}"
        );
    }
}

#[test]
fn parse_record_agrees() {
    let dir = TempDir::new("ovlink-parse").expect("fixture dir");
    let home = dir.path();
    // Every shape rule on both column counts, plus carriage
    // returns and empty fields. (NUL bytes cannot cross `execve`
    // argv, so NUL stripping is pinned at the stream level in the
    // gate fixtures below instead.)
    let lines = [
        "app.conf\tweb",
        "a/b.conf\tweb",
        "app.conf\tweb\tcustom-target",
        "app.conf\tweb\t",
        "app.conf",
        "",
        "\tweb",
        "app.conf\t",
        "app.conf\tweb\tt1\textra",
        "/abs\tweb",
        "app.conf\t",
        ".\tx",
        "..\tx",
        "a/../b\tx",
        "a/./b\tx",
        "a//b\tx",
        "a/\tx",
        "a/.\tx",
        "a/..\tx",
        "./a\tx",
        "../a\tx",
        "a\tx/y",
        "a\t.",
        "a\t..",
        "a\t",
        "a\tb\tc\rd",
        "a\tb\rc",
        "ok\tweb\t.with-dots_and-dashes",
    ];
    for line in lines {
        let (code, out, serr) = shell_run(
            home,
            &[line.as_ref()],
            "if _overlay_parse_manifest_record \"$2\"; then printf 'rc=0\\nrel=%s\\nowner=%s\\ntarget=%s\\n' \"$REPLY_REL\" \"$REPLY_OWNER\" \"$REPLY_TARGET\"; else printf 'rc=1\\n'; fi",
        );
        assert_eq!(code, 0, "shell harness parse {line:?}");
        assert_eq!(serr, b"", "parse stderr for {line:?}");
        let shell = String::from_utf8(out).expect("parse dump");
        let rust = match repos_overlays::parse_manifest_record(line) {
            Some(record) => {
                format!(
                    "rc=0\nrel={}\nowner={}\ntarget={}\n",
                    record.rel, record.owner, record.target
                )
            }
            None => "rc=1\n".to_string(),
        };
        assert_eq!(rust, shell, "parse record for {line:?}");
    }
}

/// Stage a manifest with `mode`, returning its path.
fn manifest(home: &Path, name: &str, body: &[u8], mode: u32) -> PathBuf {
    let path = stage(home, name, body);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

#[test]
fn gates_agree() {
    let dir = TempDir::new("ovlink-gates").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    let two_col = b"app.conf\tweb\na/b.conf\tweb\n";
    let three_col = b"app.conf\tweb\tcustom-target\n";
    let bad_line = b"app.conf\tweb\nno-tabs-here\n";
    // (label, body, mode, private_expected, manifest_expected):
    // two-column files skip the private rule; three-column files
    // require owner-only bits; garbage never passes.
    let good_two = manifest(home, "two.conf", two_col, 0o644);
    let open_three = manifest(home, "open.conf", three_col, 0o644);
    let shut_three = manifest(home, "shut.conf", three_col, 0o600);
    let group_three = manifest(home, "group.conf", three_col, 0o640);
    let bad = manifest(home, "bad.conf", bad_line, 0o600);
    let empty = manifest(home, "empty.conf", b"", 0o644);
    // The shell `read` strips NUL bytes from the stream, so this
    // parses as a clean two-column record on both sides.
    let nul = manifest(home, "nul.conf", b"app.conf\tweb\x00\n", 0o644);
    let locked = manifest(home, "locked.conf", two_col, 0o000);
    let cases: &[(&str, PathBuf)] = &[
        ("two", good_two),
        ("open-three", open_three),
        ("shut-three", shut_three),
        ("group-three", group_three),
        ("bad", bad),
        ("empty", empty),
        ("nul", nul),
        ("locked", locked),
        ("missing", home.join("gone.conf")),
    ];
    for (label, path) in cases {
        for (gate, snippet, rust) in [
            (
                "private",
                "_overlay_private_regular_file \"$2\"",
                repos_overlays::private_regular_file(path, uid),
            ),
            (
                "manifest",
                "_overlay_manifest_safe \"$2\"",
                repos_overlays::manifest_safe(path, uid),
            ),
            (
                "pending",
                "_overlay_pending_manifest_safe \"$2\"",
                repos_overlays::pending_manifest_safe(path, uid),
            ),
        ] {
            let (code, out, serr) = shell_run(
                home,
                &[path.as_os_str()],
                &format!("if {snippet}; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi"),
            );
            assert_eq!(code, 0, "shell harness {gate} {label}");
            let shell = String::from_utf8(out).expect("gate dump");
            assert_eq!(
                format!("rc={}\n", i32::from(!rust)),
                shell,
                "{gate} code for {label}"
            );
            let serr = String::from_utf8(serr).expect("gate stderr");
            // An unreadable gated file makes bash itself report the
            // failed redirect; the engine prints nothing there.
            let serr = if *label == "locked" {
                serr.lines()
                    .filter(|line| !line.ends_with("Permission denied"))
                    .map(|line| format!("{line}\n"))
                    .collect::<String>()
            } else {
                serr
            };
            assert_eq!(serr, "", "{gate} stderr for {label}");
        }
    }
    // Links and directories never pass either gate, on either side.
    let target = manifest(home, "target.conf", two_col, 0o600);
    std::os::unix::fs::symlink("target.conf", home.join("link.conf")).expect("symlink");
    std::fs::create_dir(home.join("plain")).expect("plain dir");
    std::fs::hard_link(&target, home.join("alias.conf")).expect("hard link");
    for (label, path) in [
        ("link", home.join("link.conf")),
        ("dir", home.join("plain")),
        ("alias", home.join("target.conf")),
    ] {
        for (gate, snippet, rust) in [
            (
                "private",
                "_overlay_private_regular_file \"$2\"",
                repos_overlays::private_regular_file(&path, uid),
            ),
            (
                "manifest",
                "_overlay_manifest_safe \"$2\"",
                repos_overlays::manifest_safe(&path, uid),
            ),
        ] {
            let (code, out, serr) = shell_run(
                home,
                &[path.as_os_str()],
                &format!("if {snippet}; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi"),
            );
            assert_eq!(code, 0, "shell harness {gate} {label}");
            assert_eq!(
                format!("rc={}\n", i32::from(!rust)),
                String::from_utf8(out).expect("gate dump"),
                "{gate} code for {label}"
            );
            assert_eq!(serr, b"", "{gate} stderr for {label}");
        }
    }
}

/// Drive the shell `_overlay_rollback_target` against explicit
/// snapshot arrays, reporting `rc` plus `REPLY`.
fn shell_rollback_target(home: &Path, paths: &[&str], targets: &[&str], rel: &str) -> String {
    let setup = paths
        .iter()
        .map(|path| format!("'{path}'"))
        .collect::<Vec<_>>()
        .join(" ");
    let setup_targets = targets
        .iter()
        .map(|target| format!("'{target}'"))
        .collect::<Vec<_>>()
        .join(" ");
    let snippet = format!(
        "DOT_OVERLAY_ROLLBACK_PATHS=({setup}); \
         DOT_OVERLAY_ROLLBACK_TARGETS=({setup_targets}); \
         if _overlay_rollback_target '{rel}'; then printf 'rc=0 reply=%s\\n' \"$REPLY\"; else printf 'rc=1 reply=%s\\n' \"$REPLY\"; fi\n"
    );
    let (code, out, serr) = shell_run(home, &[], &snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(
        serr.is_empty(),
        "snippet stderr: {:?}",
        String::from_utf8_lossy(&serr)
    );
    String::from_utf8(out).expect("rollback dump")
}

#[test]
fn rollback_target_looks_up_snapshot_or_refuses() {
    let home = TempDir::new("rollback-target").expect("fixture dir");
    let snapshot = repos_overlays::RollbackSnapshot {
        paths: vec!["a/link".to_string(), "b/link".to_string()],
        targets: vec![".files/a".to_string(), ".files/b".to_string()],
    };
    // Hit, miss, and ragged arrays (the shell length guard refuses).
    for (rel, want) in [
        ("a/link", Some(".files/a")),
        ("b/link", Some(".files/b")),
        ("c/link", None),
        ("", None),
    ] {
        let shell_dump = shell_rollback_target(
            home.path(),
            &["a/link", "b/link"],
            &[".files/a", ".files/b"],
            rel,
        );
        let rust = repos_overlays::rollback_target(&snapshot, rel);
        assert_eq!(rust, want, "rust lookup for {rel:?}");
        match want {
            Some(target) => assert_eq!(
                shell_dump,
                format!("rc=0 reply={target}\n"),
                "shell lookup for {rel:?}"
            ),
            None => assert!(
                shell_dump.starts_with("rc=1 reply="),
                "shell refuses {rel:?}: {shell_dump:?}"
            ),
        }
    }
    let ragged = repos_overlays::RollbackSnapshot {
        paths: vec!["a/link".to_string()],
        targets: vec![],
    };
    assert_eq!(repos_overlays::rollback_target(&ragged, "a/link"), None);
    let shell_dump = shell_rollback_target(home.path(), &["a/link"], &[], "a/link");
    assert!(
        shell_dump.starts_with("rc=1 reply="),
        "shell refuses ragged arrays: {shell_dump:?}"
    );
}

#[test]
fn link_target_available_checks_file_or_link() {
    let home = TempDir::new("link-target").expect("fixture dir");
    let home_text = home.path().to_string_lossy().into_owned();
    std::fs::create_dir_all(home.path().join("sub")).expect("subdir");
    std::fs::write(home.path().join("abs.txt"), b"x\n").expect("abs file");
    std::fs::write(home.path().join("sub/rel.txt"), b"y\n").expect("rel file");
    std::os::unix::fs::symlink("rel.txt", home.path().join("sub/anchor")).expect("symlink");
    // Absolute targets hit anything file-or-link; relative ones
    // resolve against the link's own parent directory.
    let abs = home.path().join("abs.txt").to_string_lossy().into_owned();
    for (rel, target, want) in [
        ("sub/anchor", abs.as_str(), true),
        ("sub/anchor", "/nonexistent-dot-test-target", false),
        ("sub/anchor", "rel.txt", true),
        ("sub/anchor", "missing.txt", false),
        ("sub/anchor", "../abs.txt", true),
    ] {
        let snippet = format!(
            "if _overlay_link_target_available '{rel}' '{target}'; then echo yes; else echo no; fi\n"
        );
        let (code, out, serr) = shell_run(home.path(), &[], &snippet);
        assert_eq!(code, 0, "harness exit");
        assert!(
            serr.is_empty(),
            "snippet stderr: {:?}",
            String::from_utf8_lossy(&serr)
        );
        let shell_yes = out.starts_with(b"yes\n");
        assert_eq!(shell_yes, want, "shell for {rel}/{target}");
        assert_eq!(
            repos_overlays::link_target_available(rel, target, &home_text),
            want,
            "rust for {rel}/{target}"
        );
    }
}
