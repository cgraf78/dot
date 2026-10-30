//! Regression coverage for `init_client_delete::candidate_matches_git`
//! on symlinks: a live symlink must match its tree blob.
//!
//! Git stores a symlink blob as exactly the target bytes, with no
//! trailing newline. The shell-era check (and the Rust port's first
//! version) hashed the target *plus* a newline — the shell's
//! `$(readlink; printf .)` trick preserved `readlink`'s own newline
//! — so no live symlink could ever match its blob and init
//! publication of any symlink always failed. The defect stayed
//! latent until a tracked tree first carried a symlink; these rows
//! pin the corrected behavior.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use dot::init_client_delete::candidate_matches_git;
use dot_test_support::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {}",
        repo.display()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Repository with one regular file and one symlink pointing at it.
fn match_repo(dir: &Path) -> (PathBuf, String) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let status = dot_test_support::git()
        .arg("init")
        .arg("-q")
        .arg("-b")
        .arg("main")
        .arg(&repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init {}", repo.display());
    std::fs::write(repo.join("plain.txt"), b"plain\n").expect("plain file");
    std::os::unix::fs::symlink("plain.txt", repo.join("ptr")).expect("repo link");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "match"]);
    let oid = git_output(&repo, &["rev-parse", "main:ptr"]);
    assert_eq!(oid.len(), 40, "symlink blob oid: {oid}");
    (repo, oid)
}

#[test]
fn symlink_matches_its_tree_blob() {
    let dir = TempDir::new("delete-matches-link").expect("temp dir");
    let (repo, oid) = match_repo(dir.path());
    let git_dir = repo.join(".git");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::write(home.join("plain.txt"), b"plain\n").expect("home plain");
    std::os::unix::fs::symlink("plain.txt", home.join("ptr")).expect("home link");

    // A live symlink carrying the tracked target matches its blob.
    assert!(
        candidate_matches_git(&git_dir, "main", "120000", &oid, "ptr", &home),
        "live symlink matches its tree blob"
    );

    // A retargeted link carries different bytes.
    std::fs::remove_file(home.join("ptr")).expect("remove link");
    std::os::unix::fs::symlink("other", home.join("ptr")).expect("retarget");
    assert!(
        !candidate_matches_git(&git_dir, "main", "120000", &oid, "ptr", &home),
        "retargeted link does not match"
    );

    // A regular file is not a link match even with equal bytes.
    std::fs::remove_file(home.join("ptr")).expect("remove link");
    std::fs::write(home.join("ptr"), b"plain.txt").expect("plain file");
    assert!(
        !candidate_matches_git(&git_dir, "main", "120000", &oid, "ptr", &home),
        "regular file does not match as a link"
    );

    // A mode-120000 probe of a regular candidate also fails.
    assert!(
        !candidate_matches_git(&git_dir, "main", "120000", &oid, "plain.txt", &home),
        "regular candidate does not match as a link"
    );

    // Unknown modes never match.
    assert!(
        !candidate_matches_git(&git_dir, "main", "100600", &oid, "ptr", &home),
        "unknown mode never matches"
    );
}
