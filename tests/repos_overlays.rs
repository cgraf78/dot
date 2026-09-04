//! Differential parity tests for `repos/overlays.sh` against the
//! live shell: link-target derivation, manifest record parsing, the
//! safety gates — including the unreadable fail-open quirk and bash
//! redirect noise — plus inventory revalidation, replacement
//! hashing, and tracked-path restore with real git fixtures.

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

/// Run one shell snippet with both overlay libraries sourced:
/// the link layer plus the local-source validators that live
/// in `lib/dot/overlays.sh`.
fn shell_run_links(
    home: &Path,
    argv: &[&std::ffi::OsStr],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(
                ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/overlays.sh\"\n. \"$1/lib/dot/log.sh\"\n{snippet}",
            ));
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

/// Drive `_overlay_replacement_identity` on `$2`, dumping
/// `rc=<code>` plus the identity line (empty on failure).
fn shell_identity(home: &Path, path: &Path) -> (i32, Vec<u8>, Vec<u8>) {
    shell_run(
        home,
        &[path.as_os_str()],
        "out=$(_overlay_replacement_identity \"$2\"); code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$out\"\n",
    )
}

#[test]
fn replacement_identity_agrees() {
    let dir = TempDir::new("ovlink-identity").expect("fixture dir");
    let home = dir.path();
    stage(home, "doc.txt", b"payload\n");
    std::os::unix::fs::symlink("doc.txt", home.join("link")).expect("symlink");
    // A symlink target with a trailing newline: the shell hashes the
    // `$(readlink)` value with newlines stripped, not the raw bytes.
    std::os::unix::fs::symlink("doc.txt\n", home.join("nl-link")).expect("nl symlink");
    std::os::unix::fs::symlink("gone-target", home.join("dangling")).expect("dangling");
    std::fs::create_dir_all(home.join("subdir")).expect("subdir");
    for name in [
        "doc.txt", "link", "nl-link", "dangling", "subdir", "missing",
    ] {
        let path = home.join(name);
        let (code, out, serr) = shell_identity(home, &path);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "identity stderr for {name:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("identity dump");
        let rust = match repos_overlays::replacement_identity(home, &path) {
            Ok(identity) => format!("rc=0\nreply={identity}\n"),
            Err(_) => "rc=1\nreply=\n".to_string(),
        };
        assert_eq!(rust, shell, "replacement identity for {name:?}");
    }
}

#[test]
fn replacement_identity_strips_readlink_newline() {
    // Absolute pin on the `$(readlink)` normalization: the digest
    // half of a newline-target symlink equals the digest of the
    // stripped bytes, not the raw link bytes.
    let dir = TempDir::new("ovlink-identity-nl").expect("fixture dir");
    let home = dir.path();
    stage(home, "doc.txt", b"payload\n");
    std::os::unix::fs::symlink("doc.txt\n", home.join("nl-link")).expect("nl symlink");
    let identity = repos_overlays::replacement_identity(home, &home.join("nl-link"))
        .expect("newline-target identity");
    let digest = identity.rsplit(':').next().expect("digest half");
    assert_eq!(
        digest,
        dot::temp::file_text_digest(home, b"doc.txt").expect("stripped digest"),
        "newline-target digest pins stripped bytes"
    );
    let raw = dot::temp::file_text_digest(home, "doc.txt\n".as_bytes()).expect("raw digest");
    assert_ne!(digest, raw, "stripped digest must differ from raw bytes");
}

/// Build a quarantine fixture: `stage/parked` symlinking to the
/// absolute `root/target.txt`, with `physical` absent — the stage
/// holds only the parked link, like the shell's quarantine staging,
/// and the target resolves from both the stage and the physical
/// location, like a link restored to its managed home. Returns
/// `(physical, parked, stage, target)`.
fn quarantine_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let stage_dir = root.join("stage");
    std::fs::create_dir_all(&stage_dir).expect("stage dir");
    let target = stage(root, "target.txt", b"managed\n");
    std::os::unix::fs::symlink(&target, stage_dir.join("parked")).expect("parked link");
    (
        root.join("physical"),
        stage_dir.join("parked"),
        stage_dir,
        target,
    )
}

/// Aftermath probe shared by the restore tests: `rc`, whether the
/// physical link reads back the expected target (`match`, `MISSING`,
/// or `other`), and parked/stage presence. `$5` carries the expected
/// target, which differs per side, so only the verdict crosses.
fn aftermath_dump(snippet: &str) -> String {
    format!(
        "{snippet}\ncode=$?; phys=$(readlink \"$2\" 2>/dev/null || echo MISSING); if [ \"$phys\" = \"$5\" ]; then phys=match; elif [ \"$phys\" != MISSING ]; then phys=other; fi; parked=absent; [ -L \"$3\" ] && parked=present; stage=absent; [ -d \"$4\" ] && stage=present; printf 'rc=%s\\nphys=%s\\nparked=%s\\nslot=%s\\n' \"$code\" \"$phys\" \"$parked\" \"$stage\"\n"
    )
}

fn aftermath_rust(
    code: i32,
    physical: &Path,
    parked: &Path,
    stage: &Path,
    target: &Path,
) -> String {
    let phys = match std::fs::read_link(physical) {
        Err(_) => "MISSING".to_string(),
        Ok(link) if link == target => "match".to_string(),
        Ok(_) => "other".to_string(),
    };
    let parked = if std::fs::symlink_metadata(parked).is_ok() {
        "present"
    } else {
        "absent"
    };
    let stage = if stage.is_dir() { "present" } else { "absent" };
    format!("rc={code}\nphys={phys}\nparked={parked}\nslot={stage}\n")
}

#[test]
fn restore_quarantined_link_agrees() {
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    for case in ["happy", "wrong-expected", "physical-present"] {
        let dir = TempDir::new("ovlink-restore").expect("fixture dir");
        let home = dir.path();
        // Shell and Rust each work their own identical fixture: the
        // inode-bound identities only compare within one side.
        let shell_root = home.join("shell");
        let rust_root = home.join("rust");
        let (shell_phys, shell_parked, shell_stage, shell_target) = quarantine_fixture(&shell_root);
        let (rust_phys, rust_parked, rust_stage, rust_target) = quarantine_fixture(&rust_root);
        if case == "physical-present" {
            stage(&shell_root, "physical", b"user file\n");
            stage(&rust_root, "physical", b"user file\n");
        }
        let snippet = if case == "wrong-expected" {
            aftermath_dump(
                "_overlay_restore_quarantined_link \"$2\" \"$3\" \"$4\" \"bogus-expected\"",
            )
        } else {
            aftermath_dump(
                "expected=$(_overlay_replacement_identity \"$3\"); _overlay_restore_quarantined_link \"$2\" \"$3\" \"$4\" \"$expected\"",
            )
        };
        let (code, out, serr) = shell_run(
            home,
            &[
                shell_phys.as_os_str(),
                shell_parked.as_os_str(),
                shell_stage.as_os_str(),
                shell_target.as_os_str(),
            ],
            &snippet,
        );
        assert_eq!(code, 0, "harness exit for {case}");
        assert!(serr.is_empty(), "restore stderr for {case}: {serr:?}");
        let shell = String::from_utf8(out).expect("restore dump");
        let expected = if case == "wrong-expected" {
            "bogus-expected".to_string()
        } else {
            repos_overlays::replacement_identity(home, &rust_parked).expect("rust expected")
        };
        let rust_code = match repos_overlays::restore_quarantined_link(
            home,
            &rust_phys,
            &rust_parked,
            &rust_stage,
            &expected,
            &tool,
        ) {
            Ok(()) => 0,
            Err(_) => 1,
        };
        assert_eq!(
            aftermath_rust(
                rust_code,
                &rust_phys,
                &rust_parked,
                &rust_stage,
                &rust_target
            ),
            shell,
            "restore aftermath for {case}"
        );
    }
}

#[test]
fn restore_dangling_target_agrees() {
    // A parked link whose target resolves nowhere: both engines
    // verify the quarantine move with lstat, so both restore rc=0
    // (the link's own identity verifies). Move verification used to
    // follow and fail this shape closed; the transactional link
    // publisher stages dangling links by design, so parity won.
    let dir = TempDir::new("ovlink-dangling").expect("fixture dir");
    let home = dir.path();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    for (tag, root) in [("shell", home.join("shell")), ("rust", home.join("rust"))] {
        let stage_dir = root.join("stage");
        std::fs::create_dir_all(&stage_dir).expect("stage dir");
        std::os::unix::fs::symlink("gone-target", stage_dir.join("parked")).expect("parked link");
        let physical = root.join("physical");
        let parked = stage_dir.join("parked");
        if tag == "shell" {
            let (code, out, serr) = shell_run(
                home,
                &[
                    physical.as_os_str(),
                    parked.as_os_str(),
                    stage_dir.as_os_str(),
                ],
                "expected=$(_overlay_replacement_identity \"$3\"); _overlay_restore_quarantined_link \"$2\" \"$3\" \"$4\" \"$expected\"; printf 'rc=%s\\n' \"$?\"\n",
            );
            assert_eq!(code, 0, "harness exit");
            assert!(serr.is_empty(), "dangling shell stderr: {serr:?}");
            assert_eq!(out, b"rc=0\n", "shell restores a dangling link");
        } else {
            let expected =
                repos_overlays::replacement_identity(home, &parked).expect("dangling expected");
            assert!(
                repos_overlays::restore_quarantined_link(
                    home, &physical, &parked, &stage_dir, &expected, &tool
                )
                .is_ok(),
                "rust restores a dangling link like the shell"
            );
            assert_eq!(
                std::fs::read_link(&physical).expect("restored link"),
                PathBuf::from("gone-target"),
                "dangling target preserved"
            );
            assert!(
                stage_dir.symlink_metadata().is_err(),
                "emptied stage removed"
            );
        }
    }
}

#[test]
fn commit_quarantined_link_agrees() {
    for case in ["happy", "wrong-expected"] {
        let dir = TempDir::new("ovlink-commit").expect("fixture dir");
        let home = dir.path();
        let shell_root = home.join("shell");
        let rust_root = home.join("rust");
        // A commit fixture has no physical path; reuse the parked and
        // stage halves, probing `$3` as the parked link instead.
        let (_, shell_parked, shell_stage, _) = quarantine_fixture(&shell_root);
        let (_, rust_parked, rust_stage, _) = quarantine_fixture(&rust_root);
        let snippet = if case == "wrong-expected" {
            " _overlay_commit_quarantined_link \"$2\" \"$3\" \"bogus-expected\"; code=$?; parked=absent; [ -L \"$2\" ] && parked=present; stage=absent; [ -d \"$3\" ] && stage=present; printf 'rc=%s\\nparked=%s\\nslot=%s\\n' \"$code\" \"$parked\" \"$stage\"\n"
        } else {
            "expected=$(_overlay_replacement_identity \"$2\"); _overlay_commit_quarantined_link \"$2\" \"$3\" \"$expected\"; code=$?; parked=absent; [ -L \"$2\" ] && parked=present; stage=absent; [ -d \"$3\" ] && stage=present; printf 'rc=%s\\nparked=%s\\nslot=%s\\n' \"$code\" \"$parked\" \"$stage\"\n"
        };
        let (code, out, serr) = shell_run(
            home,
            &[shell_parked.as_os_str(), shell_stage.as_os_str()],
            snippet,
        );
        assert_eq!(code, 0, "harness exit for {case}");
        assert!(serr.is_empty(), "commit stderr for {case}: {serr:?}");
        let shell = String::from_utf8(out).expect("commit dump");
        let expected = if case == "wrong-expected" {
            "bogus-expected".to_string()
        } else {
            repos_overlays::replacement_identity(home, &rust_parked).expect("rust expected")
        };
        let rust_code = match repos_overlays::commit_quarantined_link(
            home,
            &rust_parked,
            &rust_stage,
            &expected,
        ) {
            Ok(()) => 0,
            Err(_) => 1,
        };
        let rust_parked_state = if std::fs::symlink_metadata(&rust_parked).is_ok() {
            "present"
        } else {
            "absent"
        };
        let rust_stage_state = if rust_stage.is_dir() {
            "present"
        } else {
            "absent"
        };
        assert_eq!(
            format!("rc={rust_code}\nparked={rust_parked_state}\nslot={rust_stage_state}\n"),
            shell,
            "commit aftermath for {case}"
        );
    }
}

#[test]
fn file_identity_agrees() {
    let dir = TempDir::new("ov-file-ident").expect("fixture dir");
    let home = dir.path();
    let file = stage(home, "a.txt", b"data");
    let sub = home.join("sub");
    std::fs::create_dir(&sub).expect("fixture dir");
    let link = home.join("link.txt");
    std::os::unix::fs::symlink(&file, &link).expect("fixture link");
    let missing = home.join("gone.txt");
    for candidate in [&file, &sub, &link, &missing] {
        let (code, out, serr) = shell_run(
            home,
            &[candidate.as_os_str()],
            "_overlay_file_identity \"$2\"; printf 'rc=%s reply=%s' \"$?\" \"$REPLY\"",
        );
        assert_eq!(code, 0, "shell harness identity");
        assert_eq!(serr, b"", "identity stderr");
        let want = match repos_overlays::file_identity(candidate) {
            Some(identity) => format!("rc=0 reply={identity}"),
            None => "rc=1 reply=".to_string(),
        };
        assert_eq!(
            want,
            String::from_utf8(out).expect("identity dump"),
            "identity parity",
        );
    }
}

#[test]
fn local_source_snapshot_matches_agrees() {
    let dir = TempDir::new("ov-snapshot").expect("fixture dir");
    let home = dir.path();
    let ov = home.join("ov");
    let ov_home = ov.join("home");
    std::fs::create_dir_all(&ov_home).expect("fixture home");
    stage(&ov_home, "file.txt", b"x");
    let real = ov_home.canonicalize().expect("real root");
    let real_str = real.to_string_lossy().into_owned();
    let identity = repos_overlays::file_identity(&real).expect("root identity");
    let ov2 = home.join("ov2");
    std::fs::create_dir(&ov2).expect("fixture overlay");
    std::os::unix::fs::symlink(&real, ov2.join("home")).expect("fixture home link");
    let missing = home.join("nope");
    let slashed = format!("{real_str}/");
    // (overlay path, expected root, expected identity).
    let cases: Vec<(&Path, &str, &str)> = vec![
        (&ov, &real_str, &identity),
        (&ov, "", ""),
        (&ov, &real_str, ""),
        (&ov, "/elsewhere", &identity),
        (&ov, &slashed, &identity),
        (&ov, &real_str, "0:0"),
        (&missing, "/x", "1:2"),
        (&ov2, &real_str, &identity),
    ];
    for (path, root, ident) in &cases {
        let path_str = path.to_string_lossy().into_owned();
        let (code, out, serr) = shell_run(
            home,
            &[
                std::ffi::OsStr::new(&path_str),
                std::ffi::OsStr::new(root),
                std::ffi::OsStr::new(ident),
            ],
            "_overlay_local_source_snapshot_matches \"$2\" \"$3\" \"$4\"; printf 'rc=%s reply=%s' \"$?\" \"$REPLY\"",
        );
        assert_eq!(code, 0, "shell harness snapshot");
        assert_eq!(serr, b"", "snapshot stderr");
        let want = match repos_overlays::local_source_snapshot_matches(&path_str, root, ident) {
            Ok(()) => "rc=0 reply=".to_string(),
            Err(diagnostic) => format!("rc=1 reply={diagnostic}"),
        };
        assert_eq!(
            want,
            String::from_utf8(out).expect("snapshot dump"),
            "snapshot parity for {path_str:?}",
        );
    }
}

#[test]
fn local_inventory_entry_current_agrees() {
    let dir = TempDir::new("ov-entry").expect("fixture dir");
    let home = dir.path();
    let ov = home.join("ov");
    let ov_home = ov.join("home");
    std::fs::create_dir_all(&ov_home).expect("fixture home");
    let src = stage(&ov_home, "file.txt", b"x");
    let real = ov_home.canonicalize().expect("real root");
    let real_str = real.to_string_lossy().into_owned();
    let identity = repos_overlays::file_identity(&real).expect("root identity");
    let home_str = home.to_string_lossy().into_owned();
    let path_str = ov.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();
    // (src, rel, root, identity): the snapshot failure
    // short-circuits before the entry validator runs.
    let cases = [
        (
            src_str.as_str(),
            "file",
            real_str.as_str(),
            identity.as_str(),
        ),
        (src_str.as_str(), "file", real_str.as_str(), "9:9"),
        (
            src_str.as_str(),
            "other",
            real_str.as_str(),
            identity.as_str(),
        ),
    ];
    for (src_arg, rel, root, ident) in &cases {
        let (code, out, serr) = shell_run_links(
            home,
            &[
                std::ffi::OsStr::new(&path_str),
                std::ffi::OsStr::new(src_arg),
                std::ffi::OsStr::new(rel),
                std::ffi::OsStr::new(root),
                std::ffi::OsStr::new(ident),
            ],
            "OVERLAYS=(); _overlay_local_inventory_entry_current \"$2\" \"$3\" \"$4\" \"$5\" \"$6\"; printf 'rc=%s reply=%s' \"$?\" \"$REPLY\"",
        );
        assert_eq!(code, 0, "shell harness entry");
        assert_eq!(serr, b"", "entry stderr for {rel:?}");
        let rust = repos_overlays::local_inventory_entry_current(
            &path_str,
            std::path::Path::new(src_arg),
            rel,
            root,
            ident,
            &[],
            &home_str,
        );
        let want = match rust {
            Ok(()) => "rc=0 reply=".to_string(),
            Err(diagnostic) => format!("rc=1 reply={diagnostic}"),
        };
        assert_eq!(
            want,
            String::from_utf8(out).expect("entry dump"),
            "entry parity for {rel:?}",
        );
    }
}

#[test]
fn replacement_hash_object_agrees() {
    let dir = TempDir::new("ov-hashobj").expect("fixture dir");
    let home = dir.path();
    let doc = stage(home, "doc.txt", b"hash me\n");
    let doc_str = doc.to_string_lossy().into_owned();
    // (git args, stdin): the source root binds `$PWD`, like
    // `DOT_SOURCE_ROOT` falling back in the shell.
    let file_args = vec![
        "--no-filters".to_string(),
        "--".to_string(),
        doc_str.clone(),
    ];
    let cases: Vec<(Vec<String>, Option<&[u8]>)> = vec![
        (vec!["--stdin".to_string()], Some(b"hello".as_slice())),
        (vec!["--stdin".to_string()], Some(b"".as_slice())),
        (file_args, None),
    ];
    for (args, stdin) in &cases {
        let argv: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let stdin_text = std::str::from_utf8(stdin.unwrap_or_default()).expect("ascii stdin");
        let mut full_argv = vec![std::ffi::OsStr::new(stdin_text)];
        full_argv.extend(argv);
        let (code, out, serr) = shell_run(
            home,
            &full_argv,
            "DOT_STDIN=\"$2\"; shift 2; export DOT_SOURCE_ROOT=\"$PWD\"; printf '%s' \"$DOT_STDIN\" | _overlay_replacement_hash_object \"$@\" >hash.out; rc=$?; printf 'rc=%s hash=%s' \"$rc\" \"$(cat hash.out)\"",
        );
        assert_eq!(code, 0, "shell harness hash");
        assert_eq!(serr, b"", "hash stderr");
        let rust = repos_overlays::replacement_hash_object(home, args, *stdin);
        let want = match rust {
            Some(hash) => format!("rc=0 hash={hash}"),
            None => "rc=1 hash=".to_string(),
        };
        assert_eq!(
            want,
            String::from_utf8(out).expect("hash dump"),
            "hash parity for {args:?}",
        );
    }
}

/// Run git for fixtures with a pinned identity and no global
/// or system config, so the client hooks never load.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

/// A base repo with two tracked files; the work tree doubles
/// as `$HOME` for the ordinary topology.
fn restore_repo(dir: &Path) -> PathBuf {
    git(dir, &["init", "-q", "."]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("tracked.txt"), "tracked\n").expect("stage tracked");
    std::fs::write(dir.join("shadow.txt"), "shadow\n").expect("stage shadow");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "init"]);
    dir.to_path_buf()
}

/// An empty-warning palette: colors stay off under a pipe,
/// like `log.sh` with no terminal.
fn plain_palette() -> dot::progress_ui::Palette {
    dot::progress_ui::Palette {
        reset: String::new(),
        bold: String::new(),
        dim: String::new(),
        green: String::new(),
        yellow: String::new(),
        red: String::new(),
        blue: String::new(),
        cyan: String::new(),
        white: String::new(),
    }
}

/// Make `repo` read-only (`lock`) or writable again.
fn set_readonly(repo: &Path, readonly: bool) {
    let mode = if readonly { 0o555 } else { 0o755 };
    std::fs::set_permissions(repo, std::fs::Permissions::from_mode(mode)).expect("fixture modes");
}
#[test]
fn restore_tracked_path_agrees() {
    let palette = plain_palette();
    for setup in [
        "restore-modified",
        "clear-fails",
        "checkout-fails",
        "refused",
    ] {
        // Each engine gets its own repo: the restore mutates
        // the work tree it runs in.
        let shell_dir = TempDir::new("ov-restore-sh").expect("fixture dir");
        let shell_repo = restore_repo(shell_dir.path());
        let rust_dir = TempDir::new("ov-restore-rs").expect("fixture dir");
        let rust_repo = restore_repo(rust_dir.path());
        let shell_home = shell_repo.to_string_lossy().into_owned();
        let rust_home = rust_repo.to_string_lossy().into_owned();
        let mut shell_overlays: Vec<String> = Vec::new();
        let mut rust_overlays: Vec<String> = Vec::new();
        let rel: &str;
        let mut want_content: Option<&str> = None;
        match setup {
            "restore-modified" => {
                rel = "tracked.txt";
                std::fs::write(shell_repo.join(rel), "changed\n").expect("dirty shell");
                std::fs::write(rust_repo.join(rel), "changed\n").expect("dirty rust");
                want_content = Some("tracked\n");
            }
            "clear-fails" => {
                rel = "untracked.txt";
                std::fs::write(shell_repo.join(rel), "new\n").expect("stage shell");
                std::fs::write(rust_repo.join(rel), "new\n").expect("stage rust");
            }
            "checkout-fails" => {
                rel = "tracked.txt";
                for repo in [&shell_repo, &rust_repo] {
                    std::fs::remove_file(repo.join(rel)).expect("remove tracked");
                    set_readonly(repo, true);
                }
            }
            _ => {
                rel = "loc/home/evil";
                for (repo, store) in [
                    (&shell_repo, &mut shell_overlays),
                    (&rust_repo, &mut rust_overlays),
                ] {
                    let loc = repo.join("loc");
                    std::fs::create_dir_all(loc.join("home")).expect("local source");
                    store.push(format!("o|{}/loc|u|x|x|none", repo.to_string_lossy()));
                }
            }
        }
        let mut argv: Vec<&std::ffi::OsStr> =
            vec![std::ffi::OsStr::new(&shell_home), std::ffi::OsStr::new(rel)];
        argv.extend(shell_overlays.iter().map(std::ffi::OsStr::new));
        let (code, out, serr) = shell_run_links(
            &shell_repo,
            &argv,
            "REPO=\"$2\"; REL=\"$3\"; OVERLAYS=(\"${@:4}\"); _base_git() { command git -C \"$REPO\" \"$@\"; }; _overlay_restore_tracked_path \"$REL\"; rc=$?; chmod u+w \"$REPO\" 2>/dev/null; printf 'rc=%s' \"$rc\"",
        );
        assert_eq!(code, 0, "shell harness restore {setup}");
        let shell_text = String::from_utf8(out).expect("restore dump");
        let shell_rc: i32 = shell_text
            .strip_prefix("rc=")
            .expect("rc prefix")
            .parse()
            .expect("rc number");
        let base = dot::repos_base::Base {
            topology: dot::repos_base::Topology::Ordinary,
            client_git_dir: String::new(),
            home: rust_home.clone(),
        };
        let (ok, warnings) =
            repos_overlays::restore_tracked_path(&palette, &base, &rust_overlays, &rust_home, rel);
        // Warnings name only the relative path, except the
        // refusal diagnostic, which carries each tree root:
        // compare each engine against its own tree.
        let refusing = format!(
            "  warning: refusing to restore a base path inside a local overlay source: {}/loc/home (destination resolves inside source: loc/home/evil)\n",
            rust_home
        );
        let want_warnings = match setup {
            "restore-modified" => Vec::new(),
            "clear-fails" => {
                b"  warning: could not clear overlay index state: untracked.txt\n".to_vec()
            }
            "checkout-fails" => {
                b"  warning: could not restore overlay base path: tracked.txt\n".to_vec()
            }
            _ => refusing.into_bytes(),
        };
        let shell_refusing = format!(
            "  warning: refusing to restore a base path inside a local overlay source: {}/loc/home (destination resolves inside source: loc/home/evil)\n",
            shell_home
        );
        let shell_want = match setup {
            "restore-modified" => Vec::new(),
            "clear-fails" => {
                b"  warning: could not clear overlay index state: untracked.txt\n".to_vec()
            }
            "checkout-fails" => {
                b"  warning: could not restore overlay base path: tracked.txt\n".to_vec()
            }
            _ => shell_refusing.into_bytes(),
        };
        assert_eq!(shell_rc == 0, ok, "restore rc {setup}");
        assert_eq!(serr, shell_want, "shell warnings {setup}");
        assert_eq!(warnings, want_warnings, "restore warnings {setup}");
        if let Some(content) = want_content {
            let shell_back = std::fs::read_to_string(shell_repo.join(rel)).expect("shell content");
            let rust_back = std::fs::read_to_string(rust_repo.join(rel)).expect("rust content");
            assert_eq!(shell_back, content, "shell restores {setup}");
            assert_eq!(rust_back, content, "rust restores {setup}");
        }
    }
}
