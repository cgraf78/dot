//! Native contracts for dirty detection, repair, and normalization.
use dot::repos_base::{Base, RepoKind, Topology};
use dot::repos_dirty;
use dot_test_support::TempDir;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
fn git(p: &Path, a: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .arg("-C")
            .arg(p)
            .args(a)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    )
}
fn repo(tag: &str) -> TempDir {
    let d = TempDir::new(tag).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["init", "-q"])
            .arg(d.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(d.path().join("tracked"), b"v1\n").unwrap();
    git(d.path(), &["add", "tracked"]);
    git(
        d.path(),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "seed",
        ],
    );
    d
}
fn prefix(p: &Path) -> Vec<OsString> {
    vec!["-C".into(), p.into()]
}
fn filetime_touch(path: &Path) {
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for mtime")
        .set_modified(later)
        .expect("set mtime");
}
fn stage_listed_matching(work: &Path, upstream: &str) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .arg("-C")
        .arg(work)
        .args(["show", &format!("{upstream}:tracked")])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("show upstream file");
    assert!(output.status.success());
    std::fs::write(work.join("tracked"), b"v2\n").unwrap();
    git(work, &["add", "tracked"]);
    git(
        work,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "advance local head",
        ],
    );
    std::fs::write(work.join("tracked"), &output.stdout).unwrap();
    assert!(repos_dirty::is_worktree_dirty(Some(&prefix(work)), &[]));
    output.stdout
}
#[test]
fn dirty_matrix_covers_base_overlays_sync_and_missing() {
    let b = repo("dirty-base");
    let o = repo("dirty-overlay");
    let skipped = repo("dirty-skipped");
    std::fs::write(skipped.path().join("tracked"), b"ignored edit").unwrap();
    let records: Vec<String> = vec![];
    assert!(!repos_dirty::is_worktree_dirty(
        Some(&prefix(b.path())),
        &records
    ));
    std::fs::write(b.path().join("tracked"), b"base edit").unwrap();
    assert!(repos_dirty::is_worktree_dirty(
        Some(&prefix(b.path())),
        &records
    ));
    git(b.path(), &["checkout", "--", "tracked"]);
    std::fs::write(o.path().join("tracked"), b"overlay edit").unwrap();
    assert!(repos_dirty::is_worktree_dirty(
        Some(&prefix(b.path())),
        &[format!("o|{}||||git", o.path().display())]
    ));
    for records in [
        vec![format!("local|{}||||none", skipped.path().display())],
        vec!["missing|/missing||||git".into()],
        vec![format!("remainder|{}||||git|extra", o.path().display())],
    ] {
        assert!(!repos_dirty::is_worktree_dirty(
            Some(&prefix(b.path())),
            &records
        ));
    }
}
fn upstream(tag: &str) -> (TempDir, TempDir, PathBuf) {
    let remote = TempDir::new(&format!("{tag}-remote")).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["init", "--bare", "-q"])
            .arg(remote.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let work = repo(&format!("{tag}-work"));
    git(
        work.path(),
        &["remote", "add", "origin", &remote.path().to_string_lossy()],
    );
    git(work.path(), &["push", "-qu", "origin", "HEAD"]);
    let branch = String::from_utf8(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .arg("-C")
            .arg(work.path())
            .args(["branch", "--show-current"])
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    (
        remote,
        work,
        PathBuf::from(format!("origin/{}", branch.trim())),
    )
}
#[test]
fn configured_upstream_matrix() {
    let (_r, w, up) = upstream("dirty-upstream");
    assert_eq!(
        repos_dirty::configured_upstream(&prefix(w.path())).as_deref(),
        up.to_str()
    );
    let lonely = repo("dirty-lonely");
    assert_eq!(
        repos_dirty::configured_upstream(&prefix(lonely.path())),
        None
    );
    assert_eq!(
        repos_dirty::configured_upstream(&prefix(&lonely.path().join("missing"))),
        None
    );
    let home = TempDir::new("dirty-separate-home").unwrap();
    let git_dir = TempDir::new("dirty-separate-git").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["init", "--bare", "-q"])
            .arg(git_dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let separate = Base {
        topology: Topology::Separate,
        client_git_dir: git_dir.path().to_string_lossy().into(),
        home: home.path().to_string_lossy().into(),
    };
    assert_eq!(
        repos_dirty::configured_upstream(&separate.git_prefix().unwrap()),
        None
    );
}
#[test]
fn dirty_files_match_ref_requires_dirty_equal_content_and_valid_ref() {
    let (_r, w, up) = upstream("dirty-ref");
    let p = prefix(w.path());
    assert!(
        !repos_dirty::dirty_files_match_ref(&w.path().to_string_lossy(), up.to_str().unwrap(), &p),
        "clean tree intentionally refuses"
    );
    let remote_bytes = stage_listed_matching(w.path(), up.to_str().unwrap());
    assert!(repos_dirty::dirty_files_match_ref(
        &w.path().to_string_lossy(),
        up.to_str().unwrap(),
        &p
    ));
    std::fs::write(w.path().join("tracked"), b"real edit\n").unwrap();
    assert!(!repos_dirty::dirty_files_match_ref(
        &w.path().to_string_lossy(),
        up.to_str().unwrap(),
        &p
    ));
    std::fs::write(w.path().join("tracked"), remote_bytes).unwrap();
    assert!(!repos_dirty::dirty_files_match_ref(
        &w.path().to_string_lossy(),
        "missing",
        &p
    ));
}
#[test]
fn dirty_files_match_remote_covers_missing_base_and_upstream() {
    let (_r, w, up) = upstream("dirty-remote");
    let p = prefix(w.path());
    assert!(!repos_dirty::dirty_files_match_remote(
        &w.path().to_string_lossy(),
        None
    ));
    let lonely = repo("dirty-remote-lonely");
    assert!(!repos_dirty::dirty_files_match_remote(
        &lonely.path().to_string_lossy(),
        Some(&prefix(lonely.path()))
    ));
    stage_listed_matching(w.path(), up.to_str().unwrap());
    assert!(repos_dirty::dirty_files_match_remote(
        &w.path().to_string_lossy(),
        Some(&p)
    ));
    std::fs::write(w.path().join("tracked"), b"real edit").unwrap();
    assert!(!repos_dirty::dirty_files_match_remote(
        &w.path().to_string_lossy(),
        Some(&p)
    ));
}
#[test]
fn try_resolve_dirty_restores_only_remote_matching_content() {
    let (_r, w, up) = upstream("dirty-resolve");
    stage_listed_matching(w.path(), up.to_str().unwrap());
    assert!(repos_dirty::try_resolve_dirty(
        &w.path().to_string_lossy(),
        Some(&prefix(w.path())),
        &[]
    ));
    assert!(!repos_dirty::is_worktree_dirty(
        Some(&prefix(w.path())),
        &[]
    ));
    std::fs::write(w.path().join("tracked"), b"mine\n").unwrap();
    assert!(!repos_dirty::try_resolve_dirty(
        &w.path().to_string_lossy(),
        Some(&prefix(w.path())),
        &[]
    ));
    assert_eq!(std::fs::read(w.path().join("tracked")).unwrap(), b"mine\n");
    assert!(up.to_str().unwrap().starts_with("origin/"));
    let plain = repo("dirty-resolve-plain");
    std::fs::write(plain.path().join("tracked"), b"mine\n").unwrap();
    assert!(!repos_dirty::try_resolve_dirty(
        &plain.path().to_string_lossy(),
        Some(&prefix(plain.path())),
        &[]
    ));
    assert_eq!(
        std::fs::read(plain.path().join("tracked")).unwrap(),
        b"mine\n"
    );
    let (_clean_remote, clean, _) = upstream("dirty-resolve-clean");
    assert!(repos_dirty::try_resolve_dirty(
        &clean.path().to_string_lossy(),
        Some(&prefix(clean.path())),
        &[]
    ));
    let (_overlay_remote, overlay, overlay_up) = upstream("dirty-resolve-overlay");
    stage_listed_matching(overlay.path(), overlay_up.to_str().unwrap());
    assert!(repos_dirty::try_resolve_dirty(
        "",
        None,
        &[format!("web|{}||||git", overlay.path().display())]
    ));
    assert!(!repos_dirty::is_worktree_dirty(
        None,
        &[format!("web|{}||||git", overlay.path().display())]
    ));
}
#[test]
fn checkout_dirty_files_restores_exact_tracked_paths_not_untracked() {
    let d = repo("dirty-checkout");
    std::fs::write(d.path().join("tracked"), b"changed\n").unwrap();
    std::fs::write(d.path().join("untracked"), b"keep\n").unwrap();
    repos_dirty::checkout_dirty_files(&prefix(d.path()));
    assert_eq!(std::fs::read(d.path().join("tracked")).unwrap(), b"v1\n");
    assert_eq!(
        std::fs::read(d.path().join("untracked")).unwrap(),
        b"keep\n"
    );
    repos_dirty::checkout_dirty_files(&prefix(&d.path().join("missing")));
}
#[test]
fn normalize_dirty_files_clears_stat_noise_but_preserves_content_edits() {
    let d = repo("dirty-normalize");
    filetime_touch(&d.path().join("tracked"));
    git(d.path(), &["update-index", "--refresh"]);
    repos_dirty::normalize_dirty_files(&prefix(d.path()));
    assert!(!repos_dirty::is_worktree_dirty(
        Some(&prefix(d.path())),
        &[]
    ));
    std::fs::write(d.path().join("tracked"), b"changed\n").unwrap();
    repos_dirty::normalize_repo(RepoKind::Base, "", Some(&prefix(d.path())));
    assert_eq!(
        std::fs::read(d.path().join("tracked")).unwrap(),
        b"changed\n"
    );
}
#[test]
fn normalize_filtered_visits_base_and_git_overlays_only() {
    let b = repo("dirty-filter-base");
    let o = repo("dirty-filter-overlay");
    let local = repo("dirty-filter-local");
    std::fs::write(o.path().join("other"), b"v1\n").unwrap();
    git(o.path(), &["add", "other"]);
    git(
        o.path(),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "add second file",
        ],
    );
    filetime_touch(&b.path().join("tracked"));
    filetime_touch(&o.path().join("tracked"));
    filetime_touch(&local.path().join("tracked"));
    std::fs::write(o.path().join("other"), b"real edit\n").unwrap();
    let records = vec![
        format!("o|{}||||git", o.path().display()),
        format!("local|{}||||none", local.path().display()),
        "missing|/missing||||git".into(),
    ];
    repos_dirty::normalize_filtered(Some(&prefix(b.path())), &records);
    assert!(!repos_dirty::is_worktree_dirty(
        Some(&prefix(b.path())),
        &[]
    ));
    assert_eq!(
        std::fs::read(o.path().join("other")).unwrap(),
        b"real edit\n"
    );
    let local_noise = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .arg("-C")
        .arg(local.path())
        .args(["diff-files", "--name-only"])
        .stderr(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(local_noise.stdout, b"tracked\n");
}
#[test]
fn dirty_file_list_names_base_files_and_prefixes_overlays() {
    let b = repo("dirty-list-base");
    let o = repo("dirty-list-overlay");
    assert!(repos_dirty::dirty_file_list(Some(&prefix(b.path())), &[]).is_empty());
    assert!(repos_dirty::dirty_file_list(None, &[]).is_empty());
    std::fs::write(b.path().join("tracked"), b"base edit").unwrap();
    assert_eq!(
        repos_dirty::dirty_file_list(Some(&prefix(b.path())), &[]),
        vec!["tracked".to_string()]
    );
    std::fs::write(o.path().join("tracked"), b"overlay edit").unwrap();
    let records = vec![format!("o|{}||||git", o.path().display())];
    assert_eq!(
        repos_dirty::dirty_file_list(Some(&prefix(b.path())), &records),
        vec![
            "tracked".to_string(),
            format!("{}/tracked", o.path().display()),
        ]
    );
    // Non-git overlays and missing worktrees contribute nothing.
    for records in [
        vec![format!("local|{}||||none", o.path().display())],
        vec!["missing|/missing||||git".to_string()],
    ] {
        assert_eq!(
            repos_dirty::dirty_file_list(None, &records),
            Vec::<String>::new(),
        );
    }
}
