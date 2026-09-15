//! Native contracts for post-pull path and mode normalization.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_pull_normalize::{
    CommitPathType, ParentStatus, commit_path_type, normalize_updated_path,
    normalize_updated_path_parents, normalize_updated_paths, snapshot_parent_status,
    snapshot_updated_path_parents,
};
use dot_test_support::TempDir;

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ],
    );
}

struct Fixture {
    _scope: TempDir,
    home: PathBuf,
    repo: PathBuf,
    before: String,
    after: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scope = TempDir::new(name).unwrap();
        let home = scope.path().join("home");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        stage(&repo, "top.txt", b"a1\n");
        stage(&repo, "sub/keep.txt", b"k\n");
        commit(&repo, "before");
        let before = text(git(&repo, &["rev-parse", "HEAD"]));
        stage(&repo, "top.txt", b"a2\n");
        stage(&repo, "sub/new.txt", b"n\n");
        stage(&repo, "newdir/n.txt", b"d\n");
        commit(&repo, "after");
        let after = text(git(&repo, &["rev-parse", "HEAD"]));
        Self {
            _scope: scope,
            home,
            repo,
            before,
            after,
        }
    }

    fn prefix(&self) -> Vec<OsString> {
        vec![OsString::from("-C"), self.repo.as_os_str().to_owned()]
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap().trim_end().into()
}
fn mask() -> u32 {
    dot::temp::read_umask().unwrap()
}
fn blob(root: &Path, relative: &str) -> String {
    text(git(root, &["hash-object", "--no-filters", "--", relative]))
}

#[test]
fn snapshot_updated_path_parents_records_each_existing_parent_and_rejects_bad_commits() {
    let fixture = Fixture::new("normalize-snapshot");
    let root = fixture.repo.to_string_lossy();
    let snapshot =
        snapshot_updated_path_parents(&fixture.prefix(), &root, &fixture.before, &fixture.after)
            .unwrap();
    let rows: Vec<Vec<&str>> = snapshot
        .lines()
        .map(|line| line.split('\t').collect())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().map(|row| row[1]).collect::<Vec<_>>(),
        vec!["newdir", "sub"]
    );
    for row in rows {
        assert_eq!(row.len(), 2);
        let meta = std::fs::symlink_metadata(fixture.repo.join(row[1])).unwrap();
        assert_eq!(row[0], format!("{}:{}", meta.dev(), meta.ino()));
    }
    assert!(
        snapshot_updated_path_parents(&fixture.prefix(), &root, &fixture.before, "no-such-ref")
            .is_none()
    );
}

#[test]
fn commit_path_type_distinguishes_trees_blobs_and_missing_objects() {
    let fixture = Fixture::new("normalize-path-type");
    for (commit, relative, expected) in [
        (&fixture.after, "sub", Some(CommitPathType::Tree)),
        (&fixture.after, "top.txt", Some(CommitPathType::Blob)),
        (&fixture.after, "missing", Some(CommitPathType::Missing)),
        (&fixture.before, "newdir", Some(CommitPathType::Missing)),
        (
            &"no-such".to_string(),
            "top.txt",
            Some(CommitPathType::Missing),
        ),
    ] {
        assert_eq!(
            commit_path_type(&fixture.prefix(), commit, relative),
            expected,
            "{commit}:{relative}"
        );
    }
}

#[test]
fn snapshot_parent_status_rejects_wrong_identity_and_malformed_rows() {
    for (snapshot, relative, identity, expected) in [
        ("1:2\tsub\n", "sub", "1:2", ParentStatus::Recorded),
        ("1:2\tsub\n", "other", "1:2", ParentStatus::Absent),
        ("1:2\tsub\n", "sub", "0:0", ParentStatus::Malformed),
        ("garbage\n", "sub", "1:2", ParentStatus::Malformed),
        ("1:2\tsub\textra\n", "sub", "1:2", ParentStatus::Malformed),
        ("1:2\tsub", "sub", "1:2", ParentStatus::Absent),
    ] {
        assert_eq!(
            snapshot_parent_status(snapshot, relative, identity),
            expected
        );
    }
}

#[test]
fn normalize_updated_path_parents_requires_plain_stable_commit_trees_and_clamps_new_dirs() {
    let fixture = Fixture::new("normalize-parents");
    let client = fixture.home.join("client");
    std::fs::create_dir_all(client.join("sub")).unwrap();
    std::fs::create_dir_all(client.join("newdir")).unwrap();
    let root = client.to_string_lossy();
    let snapshot =
        snapshot_updated_path_parents(&fixture.prefix(), &root, &fixture.before, &fixture.after)
            .unwrap();
    for (relative, expected) in [
        ("sub/new.txt", true),
        ("newdir/n.txt", true),
        ("top.txt", true),
        ("P/f.txt", false),
    ] {
        assert_eq!(
            normalize_updated_path_parents(
                &fixture.prefix(),
                &root,
                &fixture.before,
                &fixture.after,
                relative,
                &snapshot,
                mask()
            ),
            expected,
            "{relative}"
        );
    }
    std::fs::remove_dir_all(client.join("sub")).unwrap();
    stage(&client, "sub", b"blocking\n");
    assert!(!normalize_updated_path_parents(
        &fixture.prefix(),
        &root,
        &fixture.before,
        &fixture.after,
        "sub/new.txt",
        &snapshot,
        mask()
    ));
    std::fs::remove_file(client.join("sub")).unwrap();
    std::os::unix::fs::symlink("newdir", client.join("sub")).unwrap();
    assert!(!normalize_updated_path_parents(
        &fixture.prefix(),
        &root,
        &fixture.before,
        &fixture.after,
        "sub/new.txt",
        &snapshot,
        mask()
    ));

    let no_newdir = snapshot
        .lines()
        .filter(|line| !line.ends_with("\tnewdir"))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::set_permissions(
        client.join("newdir"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(normalize_updated_path_parents(
        &fixture.prefix(),
        &root,
        &fixture.before,
        &fixture.after,
        "newdir/n.txt",
        &no_newdir,
        mask()
    ));
    let ceiling = 0o777 & !(mask() & 0o777);
    assert_eq!(
        std::fs::metadata(client.join("newdir"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        ceiling
    );
}

#[test]
fn normalize_updated_path_stops_on_missing_content_mode_and_unsafe_path_failures() {
    let fixture = Fixture::new("normalize-path");
    let client = fixture.home.join("client");
    std::fs::create_dir_all(client.join("sub")).unwrap();
    stage(&client, "top.txt", b"a2\n");
    stage(&client, "sub/new.txt", b"n\n");
    let root = client.to_string_lossy();
    let snapshot =
        snapshot_updated_path_parents(&fixture.prefix(), &root, &fixture.before, &fixture.after)
            .unwrap();
    let oid = blob(&fixture.repo, "top.txt");
    for (case, relative, mode, object, expected) in [
        (
            "symlink-mode",
            "sub/new.txt",
            "120000",
            "0000000000000000000000000000000000000000".into(),
            true,
        ),
        ("bad-mode", "top.txt", "100600", oid.clone(), false),
        (
            "content-mismatch",
            "top.txt",
            "100644",
            "0000000000000000000000000000000000000000".into(),
            false,
        ),
        ("clean-644", "top.txt", "100644", oid.clone(), true),
        ("clean-755", "top.txt", "100755", oid.clone(), true),
        ("missing", "gone.txt", "100644", oid.clone(), false),
        ("unsafe", "a/.GIT/b", "100644", oid.clone(), false),
    ] {
        assert_eq!(
            normalize_updated_path(
                &fixture.prefix(),
                &root,
                "base",
                relative,
                mode,
                &object,
                &fixture.before,
                &fixture.after,
                &snapshot,
                &fixture.home.to_string_lossy(),
                &[],
                mask()
            ),
            expected,
            "{case}"
        );
    }
    let overlay = fixture.home.join("overlay");
    stage(&overlay, "home/owned.txt", b"owned\n");
    std::os::unix::fs::symlink(".dotfiles-o/home/owned.txt", fixture.home.join("owned.txt"))
        .unwrap();
    let record = format!("o|{}|https://example.invalid/x|git||git", overlay.display());
    assert!(normalize_updated_path(
        &fixture.prefix(),
        &root,
        "base",
        "owned.txt",
        "100644",
        "0000000000000000000000000000000000000000",
        &fixture.before,
        &fixture.after,
        &snapshot,
        &fixture.home.to_string_lossy(),
        &[record],
        mask()
    ));
}

#[test]
fn normalize_updated_paths_accepts_clean_generation_and_rejects_worktree_or_index_changes() {
    let fixture = Fixture::new("normalize-all");
    let root = fixture.repo.to_string_lossy();
    let snapshot =
        snapshot_updated_path_parents(&fixture.prefix(), &root, &fixture.before, &fixture.after)
            .unwrap();
    let check = || {
        normalize_updated_paths(
            &fixture.prefix(),
            &root,
            "base",
            &fixture.before,
            &fixture.after,
            &snapshot,
            &fixture.home.to_string_lossy(),
            &[],
            mask(),
        )
    };
    assert!(check());
    std::fs::write(fixture.repo.join("sub/new.txt"), b"dirty\n").unwrap();
    assert!(!check());
    git(&fixture.repo, &["checkout", "-q", "--", "sub/new.txt"]);
    std::fs::write(fixture.repo.join("sub/new.txt"), b"staged\n").unwrap();
    git(&fixture.repo, &["add", "--", "sub/new.txt"]);
    assert!(!check());
}
