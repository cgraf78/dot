//! Native contracts for safe initialization deletion.
use dot::init_client_delete as d;
use dot::temp::{self, MoveCache};
use dot_test_support::TempDir;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
fn fixture(t: &str) -> (TempDir, PathBuf) {
    let x = TempDir::new(t).unwrap();
    let h = x.path().join("home");
    std::fs::create_dir(&h).unwrap();
    (x, h)
}
fn ident(p: &Path) -> String {
    temp::identity_string(temp::path_identity(p).unwrap())
}
fn mode(p: &Path) -> String {
    format!("{:o}", temp::file_mode(p).unwrap())
}
fn git_init(p: &Path) {
    std::fs::create_dir_all(p).unwrap();
    let s = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["init", "-q", "-b", "main"])
        .status()
        .unwrap();
    assert!(s.success())
}
fn hash(g: &Path, p: &Path) -> String {
    let o = Command::new("git")
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
        &h
    ));
    assert!(!d::leaf_delete_matches(
        &p,
        "0:0",
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        &h
    ));
    assert!(!d::leaf_delete_matches(
        &h.join("missing"),
        &id,
        &g.join(".git"),
        "ignored",
        "100644",
        &oid,
        &h
    ));
}
#[test]
fn private_directory_parity() {
    let (_root, h) = fixture("delete-private");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = ident(&p);
    assert!(d::private_directory_matches(&p, None, None));
    assert!(d::private_directory_matches(&p, Some(&id), Some("700")));
    assert!(!d::private_directory_matches(&p, Some("0:0"), None));
    assert!(!d::private_directory_matches(&p, None, Some("0700")));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!d::private_directory_matches(&p, None, None));
    let l = h.join("link");
    symlink(&p, &l).unwrap();
    assert!(!d::private_directory_matches(&l, None, None));
}
#[test]
fn private_empty_directory_parity() {
    let (_root, h) = fixture("delete-empty");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(d::private_empty_directory_matches(&p, None, None));
    std::fs::write(p.join(".hidden"), b"x").unwrap();
    assert!(!d::private_empty_directory_matches(&p, None, None));
}
#[test]
fn parent_delete_parity() {
    let (_root, h) = fixture("delete-parent");
    let p = h.join("dir");
    std::fs::create_dir(&p).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = ident(&p);
    let m = mode(&p);
    assert!(d::parent_delete_matches(&p, &id, &m));
    std::fs::write(p.join("x"), b"x").unwrap();
    assert!(!d::parent_delete_matches(&p, &id, &m));
    assert!(!d::parent_delete_matches(&p, "0:0", &m));
}
#[test]
fn git_delete_parity() {
    let (_root, h) = fixture("delete-git");
    let g = h.join("repo");
    git_init(&g);
    let git = g.join(".git");
    std::fs::write(g.join("file"), b"x").unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(&g)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "add", "file"])
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .arg("-C")
        .arg(&g)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "x",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let o = Command::new("git")
        .arg("-C")
        .arg(&g)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(o.status.success());
    let commit = String::from_utf8(o.stdout).unwrap().trim().to_owned();
    let id = ident(&git);
    let mut cache = MoveCache::default();
    dot::init_client_generation::write_generation_marker(
        &git, "n1", &commit, "identity", &mut cache,
    )
    .unwrap();
    assert!(d::git_delete_matches(
        &git, &id, "n1", &commit, "identity", "main"
    ));
    assert!(!d::git_delete_matches(
        &git, &id, "n1", "0", "identity", "main"
    ));
    assert!(!d::git_delete_matches(
        &git, "0:0", "n1", "0", "identity", "main"
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
        &|p| d::leaf_delete_matches(p, &id, &g.join(".git"), "ignored", "100644", &oid, &h),
        &mut c
    ));
    assert!(!target.exists() && !park.exists());
}
