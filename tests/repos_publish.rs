//! Native contracts for overlay authority and publication primitives.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot_test_support::TempDir;

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn overlay(name: &str, path: &Path, sync: &str) -> String {
    format!("{name}|{}|url|conf|false|{sync}", path.display())
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
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
        .arg(root)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn destination(root: &Path) -> repos_overlays::DestinationInputs {
    let home = root.to_string_lossy().into_owned();
    repos_overlays::DestinationInputs {
        pwd: home.clone(),
        home,
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: Vec::new(),
        init_backup: None,
    }
}

#[test]
fn recorded_targets_cover_git_local_default_invalid_and_empty_owner_rows() {
    for (relative, owner, path, sync, expected) in [
        (
            "app.conf",
            "web",
            "/ov",
            None,
            Some(".dotfiles-web/home/app.conf"),
        ),
        (
            "app.conf",
            "web",
            "/ov",
            Some("git"),
            Some(".dotfiles-web/home/app.conf"),
        ),
        (
            "app.conf",
            "web",
            "/ov/path",
            Some("none"),
            Some("/ov/path/home/app.conf"),
        ),
        ("app.conf", "web", "/ov", Some("bogus"), None),
        (
            "app.conf",
            "",
            "/ov",
            Some("git"),
            Some(".dotfiles-/home/app.conf"),
        ),
    ] {
        assert_eq!(
            repos_overlays::record_link_target(relative, owner, path, sync).as_deref(),
            expected
        );
    }
}

#[test]
fn active_provides_and_link_matching_preserve_order_sync_and_dangling_targets() {
    let scope = TempDir::new("publish-active").unwrap();
    let root = scope.path();
    let one = root.join("one");
    let two = root.join("two");
    stage(&one, "home/app.conf", b"one\n");
    stage(&two, "home/other.conf", b"two\n");
    let overlays = vec![overlay("one", &one, "git"), overlay("two", &two, "none")];
    assert!(repos_overlays::active_provides(&overlays, "app.conf"));
    assert!(repos_overlays::active_provides(&overlays, "other.conf"));
    assert!(!repos_overlays::active_provides(&overlays, "missing"));
    assert!(!repos_overlays::active_provides(&overlays, "../escape"));
    std::os::unix::fs::symlink(".dotfiles-one/home/app.conf", root.join("app.conf")).unwrap();
    std::os::unix::fs::symlink("elsewhere", root.join("other.conf")).unwrap();
    let home = root.to_string_lossy();
    assert!(repos_overlays::active_link_matches(
        &home, &overlays, "app.conf"
    ));
    assert!(!repos_overlays::active_link_matches(
        &home,
        &overlays,
        "other.conf"
    ));
    assert!(!repos_overlays::active_link_matches(
        &home, &overlays, "missing"
    ));
}

#[test]
fn exact_authority_and_general_link_matching_pin_target_bytes() {
    let scope = TempDir::new("publish-link-matches").unwrap();
    let root = scope.path();
    stage(root, "target", b"x");
    std::os::unix::fs::symlink("target", root.join("owned")).unwrap();
    std::os::unix::fs::symlink("target\n", root.join("newline")).unwrap();
    let home = root.to_string_lossy();
    let authority = vec![("owned".into(), "target".into())];
    assert!(repos_overlays::authority_link_matches(
        &home, &authority, "owned"
    ));
    assert!(!repos_overlays::authority_link_matches(
        &home, &authority, "newline"
    ));
    assert!(!repos_overlays::authority_link_matches(
        &home, &authority, "missing"
    ));
    assert!(repos_overlays::link_matches(
        &home,
        "owned",
        "web",
        Some("target")
    ));
    assert!(!repos_overlays::link_matches(
        &home,
        "owned",
        "web",
        Some("other")
    ));
    assert!(!repos_overlays::link_matches(&home, "owned", "web", None));
    assert!(repos_overlays::link_matches(
        &home,
        "newline",
        "web",
        Some("target")
    ));
    assert!(!repos_overlays::link_matches(
        &home,
        "missing",
        "web",
        Some("target")
    ));
}

#[test]
fn pending_manifest_path_is_a_literal_suffix_even_for_empty_input() {
    assert_eq!(
        repos_overlays::pending_manifest_path("/state/manifest.tsv"),
        "/state/manifest.tsv.pending"
    );
    assert_eq!(repos_overlays::pending_manifest_path(""), ".pending");
}

#[test]
fn authority_path_matrix_and_cache_cover_live_snapshot_and_local_selector_roots() {
    let scope = TempDir::new("publish-authority-path").unwrap();
    let root = scope.path();
    let home = root.to_string_lossy().into_owned();
    let manifest = format!("{home}/manifest.tsv");
    let legacy = format!("{home}/legacy.tsv");
    let inputs = destination(root);
    for (relative, snapshot, expected) in [
        ("app.conf", None, false),
        (".config/dot/profiles.d/x", None, true),
        ("manifest.tsv", None, true),
        (".dotfiles-evil/x", None, true),
        (
            ".config/dot/profile-selectors.local.d/host.conf",
            None,
            true,
        ),
        ("sub/x", Some(format!("{home}/sub")), true),
        ("elsewhere/y", Some(format!("{home}/sub")), false),
    ] {
        assert_eq!(
            repos_overlays::path_is_authority(
                &home,
                relative,
                &manifest,
                &legacy,
                &inputs,
                snapshot.as_deref(),
                &mut repos_overlays::AuthorityCache::disabled()
            ),
            expected,
            "{relative}"
        );
    }
    let hit = format!("{home}/sub");
    let mut cache = repos_overlays::AuthorityCache::enabled();
    assert!(repos_overlays::path_is_authority(
        &home,
        "sub/x",
        &manifest,
        &legacy,
        &inputs,
        Some(&hit),
        &mut cache
    ));
    assert!(repos_overlays::path_is_authority(
        &home,
        "sub/x",
        &manifest,
        &legacy,
        &inputs,
        Some(""),
        &mut cache
    ));
    let mut uncached = repos_overlays::AuthorityCache::disabled();
    assert!(repos_overlays::path_is_authority(
        &home,
        "sub/x",
        &manifest,
        &legacy,
        &inputs,
        Some(&hit),
        &mut uncached
    ));
    assert!(!repos_overlays::path_is_authority(
        &home,
        "sub/x",
        &manifest,
        &legacy,
        &inputs,
        Some(""),
        &mut uncached
    ));
}

#[test]
fn skip_worktree_and_clean_matrix_covers_tracked_dirty_untracked_and_missing_base() {
    let scope = TempDir::new("publish-git-gates").unwrap();
    let root = scope.path();
    git(root, &["init", "-q"]);
    for name in ["clean", "skip", "dirty"] {
        stage(root, name, format!("{name}\n").as_bytes());
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "seed"]);
    git(root, &["update-index", "--skip-worktree", "skip"]);
    stage(root, "dirty", b"changed\n");
    stage(root, "untracked", b"new\n");
    let base = dot::repos_base::Base {
        topology: dot::repos_base::Topology::Ordinary,
        client_git_dir: String::new(),
        home: root.to_string_lossy().into_owned(),
    };
    for (relative, skip, clean) in [
        ("clean", false, true),
        ("skip", true, false),
        ("dirty", false, false),
        ("untracked", false, true),
    ] {
        assert_eq!(repos_overlays::skip_worktree(&base, relative), skip);
        assert_eq!(repos_overlays::tracked_path_clean(&base, relative), clean);
    }
    let missing = dot::repos_base::Base {
        topology: dot::repos_base::Topology::Missing,
        ..base
    };
    assert!(!repos_overlays::skip_worktree(&missing, "clean"));
    assert!(!repos_overlays::tracked_path_clean(&missing, "clean"));
}

#[test]
fn private_line_publication_is_mode_600_noreplace_and_byte_exact() {
    let scope = TempDir::new("publish-private-line").unwrap();
    let root = scope.path();
    let uid = dot::temp::current_uid().unwrap();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    let fresh = root.join("fresh");
    assert!(repos_overlays::write_private_line(
        &fresh, "a\tb", uid, &tool
    ));
    assert_eq!(std::fs::read(&fresh).unwrap(), b"a\tb\n");
    assert_eq!(
        std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let taken = stage(root, "taken", b"old\n");
    std::os::unix::fs::symlink("taken", root.join("linked")).unwrap();
    assert!(!repos_overlays::write_private_line(
        &taken, "new", uid, &tool
    ));
    assert!(!repos_overlays::write_private_line(
        &root.join("linked"),
        "new",
        uid,
        &tool
    ));
    assert_eq!(std::fs::read(&taken).unwrap(), b"old\n");
    assert_eq!(
        std::fs::read_link(root.join("linked")).unwrap(),
        Path::new("taken")
    );
}

#[test]
fn private_directory_requires_owned_real_directory_and_owner_only_bits() {
    let scope = TempDir::new("publish-private-directory").unwrap();
    let root = scope.path();
    let uid = dot::temp::current_uid().unwrap();
    std::fs::create_dir(root.join("locked")).unwrap();
    std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(root.join("open")).unwrap();
    std::fs::set_permissions(root.join("open"), std::fs::Permissions::from_mode(0o755)).unwrap();
    stage(root, "file", b"x");
    std::os::unix::fs::symlink("locked", root.join("linkdir")).unwrap();
    for (name, expected) in [
        ("locked", true),
        ("open", false),
        ("file", false),
        ("linkdir", false),
        ("missing", false),
    ] {
        assert_eq!(
            repos_overlays::private_directory(&root.join(name), uid),
            expected,
            "{name}"
        );
    }
}
