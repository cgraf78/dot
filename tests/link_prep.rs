//! Native behavioral coverage for overlay inventory preparation.

use dot::repos_link_prep;
use dot_test_support::TempDir;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn stage(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}
fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn git_overlay(root: &Path, home: &Path, name: &str) -> (PathBuf, String) {
    let source = root.join(format!("{name}-source"));
    stage(&source, "home/a.conf", b"a\n");
    stage(&source, "home/sub/b.conf", b"b\n");
    stage(&source, "home/stale.~1~", b"backup\n");
    stage(&source, "home/tilde~", b"kept\n");
    std::os::unix::fs::symlink("a.conf", source.join("home/link.conf")).unwrap();
    git(&source, home, &["init", "-b", "main"]);
    git(&source, home, &["add", "-A"]);
    git(&source, home, &["commit", "-qm", "seed"]);
    let checkout = root.join(name);
    git(
        root,
        home,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    (checkout, source.to_string_lossy().into_owned())
}
fn entry(name: &str, path: &Path, url: &str, sync: &str) -> String {
    format!("{name}|{}|{url}|||{sync}", path.display())
}
fn records(path: &Path) -> Vec<PathBuf> {
    let bytes = std::fs::read(path).unwrap();
    let mut paths: Vec<_> = bytes
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| PathBuf::from(std::ffi::OsStr::from_bytes(r)))
        .collect();
    paths.sort();
    paths
}

#[test]
fn prepares_git_inventory_with_private_mode_and_filters_backups() {
    let scope = TempDir::new("link-prep-git").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    let entries = vec![entry("overlay", &checkout, &url, "git")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: Some("2"),
        },
        &root,
    )
    .unwrap();
    let inventory = &got.inventories["overlay"];
    assert_eq!(inventory.file_name().unwrap(), "1");
    assert_eq!(
        std::fs::metadata(inventory).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        records(inventory),
        vec![
            checkout.join("home/a.conf"),
            checkout.join("home/link.conf"),
            checkout.join("home/sub/b.conf"),
            checkout.join("home/tilde~")
        ]
    );
    assert!(got.source_roots.is_empty());
    assert!(got.source_identities.is_empty());
    assert!(
        std::fs::read_dir(&root).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .as_bytes()
            .starts_with(b".build-"))
    );
}

#[test]
fn local_inventory_freezes_physical_source_identity() {
    let scope = TempDir::new("link-prep-local").unwrap();
    let home = scope.path().join("home");
    let source = scope.path().join("local");
    stage(&source, "home/local.conf", b"local\n");
    std::fs::create_dir(&home).unwrap();
    let entries = vec![entry("local", &source, "", "none")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: None,
        },
        &root,
    )
    .unwrap();
    assert_eq!(
        records(&got.inventories["local"]),
        vec![source.join("home/local.conf")]
    );
    assert_eq!(
        Path::new(&got.source_roots["local"]),
        std::fs::canonicalize(source.join("home")).unwrap()
    );
    assert!(!got.source_identities["local"].is_empty());
}

#[test]
fn skips_invalid_overlays_without_numbering_gaps() {
    let scope = TempDir::new("link-prep-skips").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (good, url) = git_overlay(scope.path(), &home, "good");
    let plain = scope.path().join("plain");
    stage(&plain, "home/x", b"x");
    let entries = vec![
        entry("missing", &scope.path().join("missing"), "", "git"),
        entry("plain", &plain, "ignored", "git"),
        entry("good", &good, &url, "git"),
    ];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: Some("bogus"),
        },
        &root,
    )
    .unwrap();
    assert_eq!(got.inventories.len(), 1);
    assert_eq!(got.inventories["good"].file_name().unwrap(), "1");
}

#[test]
fn parallel_preparation_is_repeatable() {
    let scope = TempDir::new("link-prep-parallel").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut entries = Vec::new();
    for name in ["one", "two", "three"] {
        let (checkout, url) = git_overlay(scope.path(), &home, name);
        entries.push(entry(name, &checkout, &url, "git"));
    }
    for round in 0..10 {
        let root = scope.path().join(format!("inventories-{round}"));
        std::fs::create_dir(&root).unwrap();
        let got = repos_link_prep::prepare_inventories(
            &repos_link_prep::Inputs {
                entries: &entries,
                home: home.to_str().unwrap(),
                update_jobs: Some("2"),
            },
            &root,
        )
        .unwrap();
        assert_eq!(got.inventories.len(), 3, "round {round}");
        for (name, index) in [("one", "1"), ("two", "2"), ("three", "3")] {
            assert_eq!(
                got.inventories[name].file_name().unwrap(),
                index,
                "round {round}: {name}"
            );
        }
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".build-")
        }));
    }
}

#[test]
fn missing_output_root_fails() {
    let scope = TempDir::new("link-prep-failure").unwrap();
    let home = scope.path().join("home");
    let source = scope.path().join("local");
    stage(&source, "home/a", b"a");
    std::fs::create_dir(&home).unwrap();
    let entries = vec![entry("local", &source, "", "none")];
    assert!(
        repos_link_prep::prepare_inventories(
            &repos_link_prep::Inputs {
                entries: &entries,
                home: home.to_str().unwrap(),
                update_jobs: None,
            },
            &scope.path().join("absent")
        )
        .is_none()
    );
}
