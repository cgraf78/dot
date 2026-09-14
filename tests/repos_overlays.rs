//! Native filesystem and Git contracts for repository overlay primitives.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::repos_overlays;
use dot_test_support::TempDir;

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).expect("fixture parent");
    std::fs::write(&path, bytes).expect("fixture bytes");
    path
}

fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-C",
        ])
        .arg(repo)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn palette() -> dot::progress_ui::Palette {
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

#[test]
fn link_targets_preserve_depth_dots_and_empty_fields() {
    for (relative, owner, expected) in [
        ("app.conf", "web", ".dotfiles-web/home/app.conf"),
        ("a/b.conf", "web", "../.dotfiles-web/home/a/b.conf"),
        ("a/b/c.conf", "web", "../../.dotfiles-web/home/a/b/c.conf"),
        (".config/app", "x", "../.dotfiles-x/home/.config/app"),
        ("", "x", ".dotfiles-x/home/"),
        ("a", "", ".dotfiles-/home/a"),
    ] {
        assert_eq!(repos_overlays::link_target(relative, owner), expected);
    }
}

#[test]
fn manifest_record_matrix_pins_columns_and_path_bytes() {
    for (line, expected) in [
        (
            "app.conf\tweb",
            Some(("app.conf", "web", ".dotfiles-web/home/app.conf")),
        ),
        (
            "a/b.conf\tweb\tcustom-target",
            Some(("a/b.conf", "web", "custom-target")),
        ),
        (
            "ok\tweb\t.with-dots_and-dashes",
            Some(("ok", "web", ".with-dots_and-dashes")),
        ),
        ("app.conf\tweb\t", None),
        ("app.conf", None),
        ("", None),
        ("\tweb", None),
        ("app.conf\t", None),
        ("app.conf\tweb\tt1\textra", None),
        ("/abs\tweb", None),
        (".\tx", None),
        ("..\tx", None),
        ("a/../b\tx", None),
        ("a/./b\tx", None),
        ("a//b\tx", None),
        ("a/\tx", None),
        ("a/.\tx", None),
        ("a/..\tx", None),
        ("./a\tx", None),
        ("../a\tx", None),
        ("a\tx/y", None),
        ("a\t.", None),
        ("a\t..", None),
        ("a\tb\tc\rd", None),
        ("a\tb\rc", None),
    ] {
        let actual = repos_overlays::parse_manifest_record(line)
            .map(|record| (record.rel, record.owner, record.target));
        assert_eq!(
            actual
                .as_ref()
                .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())),
            expected,
            "line {line:?}"
        );
    }
}

#[test]
fn manifest_gates_pin_modes_links_hardlinks_nuls_and_explicit_targets() {
    let scope = TempDir::new("overlay-manifest-gates").unwrap();
    let root = scope.path();
    let uid = dot::temp::current_uid().unwrap();
    let cases = [
        (
            "two",
            b"app.conf\tweb\n".as_slice(),
            0o644,
            false,
            true,
            false,
        ),
        (
            "open-three",
            b"app.conf\tweb\ttarget\n",
            0o644,
            false,
            false,
            false,
        ),
        (
            "shut-three",
            b"app.conf\tweb\ttarget\n",
            0o600,
            true,
            true,
            true,
        ),
        (
            "group-three",
            b"app.conf\tweb\ttarget\n",
            0o640,
            false,
            false,
            false,
        ),
        (
            "bad",
            b"app.conf\tweb\nno-tabs\n",
            0o600,
            true,
            false,
            false,
        ),
        ("empty", b"", 0o644, false, true, false),
        ("nul", b"app.conf\tweb\0\n", 0o644, false, true, false),
    ];
    for (name, bytes, mode, private, manifest, pending) in cases {
        let path = stage(root, name, bytes);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            repos_overlays::private_regular_file(&path, uid),
            private,
            "private {name}"
        );
        assert_eq!(
            repos_overlays::manifest_safe(&path, uid),
            manifest,
            "manifest {name}"
        );
        assert_eq!(
            repos_overlays::pending_manifest_safe(&path, uid),
            pending,
            "pending {name}"
        );
    }
    let target = stage(root, "target", b"app.conf\tweb\n");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("target", root.join("link")).unwrap();
    std::fs::hard_link(&target, root.join("hardlink")).unwrap();
    std::fs::create_dir(root.join("directory")).unwrap();
    for path in [
        root.join("link"),
        root.join("hardlink"),
        root.join("directory"),
        root.join("missing"),
    ] {
        assert!(
            !repos_overlays::private_regular_file(&path, uid),
            "private {}",
            path.display()
        );
        assert!(
            !repos_overlays::manifest_safe(&path, uid),
            "manifest {}",
            path.display()
        );
    }
}

#[test]
fn rollback_lookup_requires_parallel_arrays_and_exact_relative_bytes() {
    let snapshot = repos_overlays::RollbackSnapshot {
        paths: vec!["a/link".into(), "b/link".into()],
        targets: vec![".files/a".into(), ".files/b".into()],
    };
    assert_eq!(
        repos_overlays::rollback_target(&snapshot, "a/link"),
        Some(".files/a")
    );
    assert_eq!(
        repos_overlays::rollback_target(&snapshot, "b/link"),
        Some(".files/b")
    );
    assert_eq!(repos_overlays::rollback_target(&snapshot, "c/link"), None);
    assert_eq!(repos_overlays::rollback_target(&snapshot, ""), None);
    assert_eq!(
        repos_overlays::rollback_target(
            &repos_overlays::RollbackSnapshot {
                paths: vec!["a/link".into()],
                targets: vec![]
            },
            "a/link"
        ),
        None
    );
}

#[test]
fn target_availability_resolves_relative_to_destination_parent_and_keeps_dangling_semantics() {
    let scope = TempDir::new("overlay-target-available").unwrap();
    let home = scope.path().to_string_lossy().into_owned();
    stage(scope.path(), "absolute", b"a");
    stage(scope.path(), "sub/relative", b"r");
    std::os::unix::fs::symlink("missing", scope.path().join("sub/dangling")).unwrap();
    let absolute = scope.path().join("absolute").to_string_lossy().into_owned();
    for (relative, target, expected) in [
        ("sub/link", absolute.as_str(), true),
        ("sub/link", "/definitely/missing", false),
        ("sub/link", "relative", true),
        ("sub/link", "missing", false),
        ("sub/link", "../absolute", true),
        ("sub/link", "dangling", true),
    ] {
        assert_eq!(
            repos_overlays::link_target_available(relative, target, &home),
            expected
        );
    }
}

#[test]
fn replacement_identity_pins_file_and_symlink_generations_and_rejects_other_types() {
    let scope = TempDir::new("overlay-identities").unwrap();
    let root = scope.path();
    let file = stage(root, "file", b"payload\n");
    std::os::unix::fs::symlink("file", root.join("link")).unwrap();
    std::os::unix::fs::symlink("file\n", root.join("newline-link")).unwrap();
    std::os::unix::fs::symlink("gone", root.join("dangling")).unwrap();
    std::fs::create_dir(root.join("directory")).unwrap();
    for path in [
        &file,
        &root.join("link"),
        &root.join("newline-link"),
        &root.join("dangling"),
    ] {
        let identity = repos_overlays::replacement_identity(root, path).expect("identity");
        assert_eq!(
            identity.split(':').count(),
            5,
            "identity shape for {}",
            path.display()
        );
    }
    let newline = repos_overlays::replacement_identity(root, &root.join("newline-link")).unwrap();
    assert_eq!(
        newline.rsplit(':').next().unwrap(),
        dot::temp::file_text_digest(root, b"file").unwrap()
    );
    assert_ne!(
        newline.rsplit(':').next().unwrap(),
        dot::temp::file_text_digest(root, b"file\n").unwrap()
    );
    assert!(repos_overlays::replacement_identity(root, &root.join("directory")).is_err());
    assert!(repos_overlays::replacement_identity(root, &root.join("missing")).is_err());
}

fn quarantine(root: &Path, target: &str) -> (PathBuf, PathBuf, PathBuf) {
    let slot = root.join("slot");
    std::fs::create_dir_all(&slot).unwrap();
    let parked = slot.join("parked");
    std::os::unix::fs::symlink(target, &parked).unwrap();
    (root.join("physical"), parked, slot)
}

#[test]
fn quarantined_restore_preserves_exact_and_dangling_links_and_fails_closed_on_races() {
    for case in ["exact", "dangling", "wrong-identity", "destination-won"] {
        let scope = TempDir::new("overlay-quarantine-restore").unwrap();
        let root = scope.path();
        if case == "exact" {
            stage(root, "target", b"managed\n");
        }
        let target = if case == "exact" {
            root.join("target").to_string_lossy().into_owned()
        } else {
            "missing-target".into()
        };
        let (physical, parked, slot) = quarantine(root, &target);
        let expected = repos_overlays::replacement_identity(root, &parked).unwrap();
        if case == "destination-won" {
            stage(root, "physical", b"user\n");
        }
        let supplied = if case == "wrong-identity" {
            "wrong"
        } else {
            &expected
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let result = repos_overlays::restore_quarantined_link(
            root, &physical, &parked, &slot, supplied, &tool,
        );
        let success = matches!(case, "exact" | "dangling");
        assert_eq!(result.is_ok(), success, "case {case}");
        if success {
            assert_eq!(
                std::fs::read_link(&physical).unwrap(),
                PathBuf::from(&target)
            );
            assert!(!slot.exists());
        } else {
            assert!(parked.symlink_metadata().is_ok());
            if case == "destination-won" {
                assert_eq!(std::fs::read(&physical).unwrap(), b"user\n");
            }
        }
    }
}

#[test]
fn quarantined_commit_removes_only_the_authorized_parked_generation() {
    for case in ["exact", "wrong-identity"] {
        let scope = TempDir::new("overlay-quarantine-commit").unwrap();
        let root = scope.path();
        let (_, parked, slot) = quarantine(root, "target");
        let expected = repos_overlays::replacement_identity(root, &parked).unwrap();
        let supplied = if case == "exact" { &expected } else { "wrong" };
        let result = repos_overlays::commit_quarantined_link(root, &parked, &slot, supplied);
        assert_eq!(result.is_ok(), case == "exact");
        assert_eq!(parked.symlink_metadata().is_ok(), case != "exact");
        assert_eq!(slot.exists(), case != "exact");
    }
}

#[test]
fn local_source_and_entry_revalidation_report_stable_literal_failures() {
    let scope = TempDir::new("overlay-local-snapshot").unwrap();
    let root = scope.path();
    let overlay = root.join("overlay");
    let source = stage(&overlay, "home/file", b"x");
    let physical = overlay.join("home").canonicalize().unwrap();
    let physical_text = physical.to_string_lossy().into_owned();
    let identity = repos_overlays::file_identity(&physical).unwrap();
    let path = overlay.to_string_lossy().into_owned();
    assert_eq!(
        repos_overlays::local_source_snapshot_matches(&path, &physical_text, &identity),
        Ok(())
    );
    assert_eq!(
        repos_overlays::local_source_snapshot_matches(&path, "", ""),
        Err(format!("{path}/home (missing inventory identity)"))
    );
    assert_eq!(
        repos_overlays::local_source_snapshot_matches(&path, "/elsewhere", &identity),
        Err(format!("{path}/home (source root changed)"))
    );
    assert_eq!(
        repos_overlays::local_source_snapshot_matches(&path, &physical_text, "0:0"),
        Err(format!("{path}/home (source root replaced)"))
    );
    let home = root.to_string_lossy().into_owned();
    assert_eq!(
        repos_overlays::local_inventory_entry_current(
            &path,
            &source,
            "file",
            &physical_text,
            &identity,
            &[],
            &home
        ),
        Ok(())
    );
    assert!(
        repos_overlays::local_inventory_entry_current(
            &path,
            &source,
            "other",
            &physical_text,
            &identity,
            &[],
            &home
        )
        .is_err()
    );
}

#[test]
fn hash_object_pins_stdin_empty_and_filter_free_file_bytes() {
    let scope = TempDir::new("overlay-hash-object").unwrap();
    let file = stage(scope.path(), "file", b"hash me\n");
    let hello = repos_overlays::replacement_hash_object(scope.path(), &["--stdin"], Some(b"hello"))
        .unwrap();
    let empty =
        repos_overlays::replacement_hash_object(scope.path(), &["--stdin"], Some(b"")).unwrap();
    let file_hash = repos_overlays::replacement_hash_object(
        scope.path(),
        &["--no-filters", "--", file.to_str().unwrap()],
        None,
    )
    .unwrap();
    assert_eq!(hello, "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0");
    assert_eq!(empty, "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_eq!(file_hash, "0755c4d6308bfcba3e764535737ecc47c30cb26b");
}

fn restore_repo(root: &Path) {
    git(root, &["init", "-q"]);
    stage(root, "tracked", b"tracked\n");
    stage(root, "shadow", b"shadow\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "seed"]);
}

#[test]
fn tracked_restore_covers_success_clear_failure_checkout_failure_and_local_source_refusal() {
    for case in ["modified", "untracked", "missing-object", "local-source"] {
        let scope = TempDir::new("overlay-restore-tracked").unwrap();
        let root = scope.path();
        restore_repo(root);
        let home = root.to_string_lossy().into_owned();
        let base = dot::repos_base::Base {
            topology: dot::repos_base::Topology::Ordinary,
            client_git_dir: String::new(),
            home: home.clone(),
        };
        let mut overlays = Vec::new();
        let relative = match case {
            "modified" => {
                stage(root, "tracked", b"changed\n");
                "tracked"
            }
            "untracked" => {
                stage(root, "untracked", b"new\n");
                "untracked"
            }
            "missing-object" => {
                std::fs::remove_file(root.join("tracked")).unwrap();
                let oid =
                    String::from_utf8(git(root, &["rev-parse", "HEAD:tracked"]).stdout).unwrap();
                let oid = oid.trim();
                std::fs::remove_file(root.join(".git/objects").join(&oid[..2]).join(&oid[2..]))
                    .unwrap();
                "tracked"
            }
            _ => {
                std::fs::create_dir_all(root.join("local/home")).unwrap();
                overlays.push(format!("local|{home}/local|u|c|false|none"));
                "local/home/evil"
            }
        };
        let (ok, warnings) =
            repos_overlays::restore_tracked_path(&palette(), &base, &overlays, &home, relative);
        match case {
            "modified" => {
                assert!(ok);
                assert!(warnings.is_empty());
                assert_eq!(std::fs::read(root.join("tracked")).unwrap(), b"tracked\n");
            }
            "untracked" => {
                assert!(!ok);
                assert_eq!(
                    warnings,
                    b"  warning: could not clear overlay index state: untracked\n"
                );
            }
            "missing-object" => {
                assert!(!ok);
                assert_eq!(
                    warnings,
                    b"  warning: could not restore overlay base path: tracked\n"
                );
            }
            _ => {
                assert!(!ok);
                assert_eq!(warnings, format!("  warning: refusing to restore a base path inside a local overlay source: {home}/local/home (destination resolves inside source: local/home/evil)\n").into_bytes());
            }
        }
    }
}
