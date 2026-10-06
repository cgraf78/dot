//! Native contracts for safe initialization deletion.
use dot::init_client_delete as d;
use dot::temp::{self, MoveCache};
use dot_test_support::TempDir;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
fn fixture(t: &str) -> (TempDir, PathBuf) {
    let x = TempDir::new(t).unwrap();
    let h = x.path().join("home");
    std::fs::create_dir(&h).unwrap();
    (x, h)
}
fn ident(p: &Path) -> String {
    temp::identity_string(temp::path_identity(p).unwrap())
}
/// `p`'s identity as journaled before a reboot renumbered its mount: the
/// same inode under the device number the mount had then.
fn renumbered(p: &Path) -> String {
    let (dev, ino) = temp::path_identity(p).unwrap();
    format!("{}:{ino}", dev + 1)
}
/// `p`'s birth time, or `None` (after saying so) where this host reports
/// none: the exact device rule then still applies and a renumbering case
/// has nothing to prove.
fn birth(p: &Path) -> Option<SystemTime> {
    let birth = dot::persisted_identity::LiveIdentity::of(p).unwrap().birth;
    if birth.is_none() {
        eprintln!("skipping: no birth time on this host");
    }
    birth
}
/// Journal times just after and just before `born`.
fn around(born: SystemTime) -> (Option<SystemTime>, Option<SystemTime>) {
    let second = Duration::from_secs(1);
    (Some(born + second), born.checked_sub(second))
}
fn mode(p: &Path) -> String {
    format!("{:o}", temp::file_mode(p).unwrap())
}
fn git_init(p: &Path) {
    std::fs::create_dir_all(p).unwrap();
    let s = dot_test_support::git()
        .arg("-C")
        .arg(p)
        .args(["init", "-q", "-b", "main"])
        .status()
        .unwrap();
    assert!(s.success())
}
fn hash(g: &Path, p: &Path) -> String {
    let o = dot_test_support::git()
        .arg(format!("--git-dir={}", g.display()))
        .args(["hash-object", "--no-filters", "--"])
        .arg(p)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success());
    String::from_utf8(o.stdout).unwrap().trim().into()
}
#[test]
fn park_path_parity() {
    let (_root, h) = fixture("delete-park");
    for kind in ["leaf", "parent", "git"] {
        let p = d::delete_park_path(&h.join("a/file"), kind, "a/file", "n1").unwrap();
        assert_eq!(p.parent(), Some(h.join("a").as_path()));
        assert!(
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!(".dot-init-delete.n1.{kind}."))
        );
    }
    assert!(d::delete_park_path(Path::new("bare"), "leaf", "x", "n").is_err());
    assert!(d::delete_park_path(&h.join("x"), "bad", "x", "n").is_err());
}
#[test]
fn candidate_content_parity() {
    let (_root, h) = fixture("delete-content");
    let g = h.join("git");
    git_init(&g);
    let p = h.join("file");
    std::fs::write(&p, b"data\n").unwrap();
    let oid = hash(&g.join(".git"), &p);
    assert!(d::candidate_matches_git(
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        "file",
        &h
    ));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!d::candidate_matches_git(
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        "file",
        &h
    ));
    assert!(d::candidate_matches_git(
        &g.join(".git"),
        "ignored",
        "100755",
        &oid,
        "file",
        &h
    ));
    std::fs::write(&p, b"drift").unwrap();
    assert!(!d::candidate_matches_git(
        &g.join(".git"),
        "ignored",
        "100755",
        &oid,
        "file",
        &h
    ));
    assert!(!d::candidate_matches_git(
        &g.join(".git"),
        "ignored",
        "bad",
        &oid,
        "file",
        &h
    ));
}
#[test]
fn leaf_delete_parity() {
    let (_root, h) = fixture("delete-leaf");
    let g = h.join("git");
    git_init(&g);
    let p = h.join("file");
    std::fs::write(&p, b"data").unwrap();
    let oid = hash(&g.join(".git"), &p);
    let id = ident(&p);
    assert!(d::leaf_delete_matches(
        &p,
        &id,
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        &h,
        None
    ));
    assert!(!d::leaf_delete_matches(
        &p,
        "0:0",
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        &h,
        None
    ));
    assert!(!d::leaf_delete_matches(
        &h.join("missing"),
        &id,
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        &h,
        None
    ));
}
#[test]
fn private_directory_parity() {
    let (_root, h) = fixture("delete-private");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = ident(&p);
    assert!(d::private_directory_matches(&p, None, None, None));
    assert!(d::private_directory_matches(
        &p,
        Some(&id),
        Some("700"),
        None
    ));
    assert!(!d::private_directory_matches(&p, Some("0:0"), None, None));
    assert!(!d::private_directory_matches(&p, None, Some("0700"), None));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!d::private_directory_matches(&p, None, None, None));
    let l = h.join("link");
    symlink(&p, &l).unwrap();
    assert!(!d::private_directory_matches(&l, None, None, None));
}
#[test]
fn private_empty_directory_parity() {
    let (_root, h) = fixture("delete-empty");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(d::private_empty_directory_matches(&p, None, None, None));
    std::fs::write(p.join(".hidden"), b"x").unwrap();
    assert!(!d::private_empty_directory_matches(&p, None, None, None));
}
#[test]
fn parent_delete_parity() {
    let (_root, h) = fixture("delete-parent");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = ident(&p);
    let m = mode(&p);
    assert!(d::parent_delete_matches(&p, &id, &m, None));
    std::fs::write(p.join("x"), b"x").unwrap();
    assert!(!d::parent_delete_matches(&p, &id, &m, None));
    assert!(!d::parent_delete_matches(&p, "0:0", &m, None));
}
/// A repository under `h` with one commit on `main` and this run's
/// generation marker: what `git_delete_matches` accepts.
fn committed_repo(h: &Path) -> (PathBuf, String) {
    let g = h.join("repo");
    git_init(&g);
    let git = g.join(".git");
    std::fs::write(g.join("file"), b"x").unwrap();
    let status = dot_test_support::git()
        .arg("-C")
        .arg(&g)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "add", "file"])
        .status()
        .unwrap();
    assert!(status.success());
    let status = dot_test_support::git()
        .arg("-C")
        .arg(&g)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "x",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let o = dot_test_support::git()
        .arg("-C")
        .arg(&g)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(o.status.success());
    let commit = String::from_utf8(o.stdout).unwrap().trim().to_owned();
    let mut cache = MoveCache::default();
    dot::init_client_generation::write_generation_marker(
        &git, "n1", &commit, "identity", &mut cache,
    )
    .unwrap();
    (git, commit)
}
#[test]
fn git_delete_parity() {
    let (_root, h) = fixture("delete-git");
    let (git, commit) = committed_repo(&h);
    let id = ident(&git);
    assert!(d::git_delete_matches(
        &git, &id, "n1", &commit, "identity", "main", None
    ));
    assert!(!d::git_delete_matches(
        &git, &id, "n1", "0", "identity", "main", None
    ));
    assert!(!d::git_delete_matches(
        &git, "0:0", "n1", "0", "identity", "main", None
    ));
}
#[test]
fn parked_generation_parity() {
    for remover in ["leaf", "parent", "tree"] {
        let (_root, h) = fixture(remover);
        let target = h.join("target");
        match remover {
            "leaf" => std::fs::write(&target, b"x").unwrap(),
            _ => std::fs::create_dir(&target).unwrap(),
        };
        let park = h.join("park");
        let mut c = MoveCache::default();
        assert!(d::delete_parked_generation(
            &target,
            &park,
            remover,
            &|_| true,
            &mut c
        ));
        assert!(!target.exists() && !park.exists());
    }
    let (_root, h) = fixture("delete-refuse");
    let target = h.join("target");
    std::fs::write(&target, b"x").unwrap();
    let park = h.join("park");
    let mut c = MoveCache::default();
    assert!(!d::delete_parked_generation(
        &target,
        &park,
        "leaf",
        &|_| false,
        &mut c
    ));
    assert!(target.exists() && !park.exists());
}
#[test]
fn parked_leaf_integration_parity() {
    let (_root, h) = fixture("delete-integration");
    let g = h.join("git");
    git_init(&g);
    let target = h.join("file");
    std::fs::write(&target, b"data").unwrap();
    let oid = hash(&g.join(".git"), &target);
    let id = ident(&target);
    let park = d::delete_park_path(&target, "leaf", "file", "n1").unwrap();
    let mut c = MoveCache::default();
    assert!(d::delete_parked_generation(
        &target,
        &park,
        "leaf",
        &|p| d::leaf_delete_matches(p, &id, &g.join(".git"), "ignored", "100644", &oid, &h, None),
        &mut c
    ));
    assert!(!target.exists() && !park.exists());
}

#[test]
fn private_directory_survives_a_renumbered_device() {
    let (_root, h) = fixture("delete-private-renumbered");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Some(born) = birth(&p) else {
        return;
    };
    let (after, _) = around(born);
    let id = renumbered(&p);
    assert!(d::private_directory_matches(
        &p,
        Some(&id),
        Some("700"),
        after
    ));
    assert!(d::private_empty_directory_matches(
        &p,
        Some(&id),
        Some("700"),
        after
    ));
    assert!(d::parent_delete_matches(&p, &id, &mode(&p), after));
}
#[test]
fn private_directory_born_after_its_journal_is_refused_when_renumbered() {
    let (_root, h) = fixture("delete-private-born-after");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Some(born) = birth(&p) else {
        return;
    };
    let (_, before) = around(born);
    let id = renumbered(&p);
    assert!(!d::private_directory_matches(
        &p,
        Some(&id),
        Some("700"),
        before
    ));
    assert!(!d::private_directory_matches(
        &p,
        Some(&id),
        Some("700"),
        None
    ));
    assert!(!d::parent_delete_matches(&p, &id, &mode(&p), before));
}
#[test]
fn leaf_delete_survives_a_renumbered_device_only_for_an_older_leaf() {
    let (_root, h) = fixture("delete-leaf-renumbered");
    let g = h.join("git");
    git_init(&g);
    let p = h.join("file");
    std::fs::write(&p, b"data").unwrap();
    let Some(born) = birth(&p) else {
        return;
    };
    let (after, before) = around(born);
    let oid = hash(&g.join(".git"), &p);
    let id = renumbered(&p);
    let leaf = |journaled| {
        d::leaf_delete_matches(
            &p,
            &id,
            &g.join(".git"),
            "ignored",
            "100644",
            &oid,
            &h,
            journaled,
        )
    };
    assert!(leaf(after));
    assert!(!leaf(before));
    assert!(!leaf(None));
}
#[test]
fn git_delete_survives_a_renumbered_device_only_for_an_older_git_dir() {
    let (_root, h) = fixture("delete-git-renumbered");
    let (git, commit) = committed_repo(&h);
    let Some(born) = birth(&git) else {
        return;
    };
    let (after, before) = around(born);
    let id = renumbered(&git);
    let tree =
        |journaled| d::git_delete_matches(&git, &id, "n1", &commit, "identity", "main", journaled);
    assert!(tree(after));
    assert!(!tree(before));
    assert!(!tree(None));
}
