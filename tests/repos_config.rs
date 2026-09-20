//! Native contracts for repository configuration and identity helpers.
use dot::repos_config;
use dot_test_support::TempDir;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};
fn git(args: &[&std::ffi::OsStr]) {
    assert!(
        Command::new("git")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    )
}
fn repo(path: &Path, origin: Option<&str>) {
    git(&["init".as_ref(), "-q".as_ref(), path.as_os_str()]);
    if let Some(url) = origin {
        git(&[
            "-C".as_ref(),
            path.as_os_str(),
            "remote".as_ref(),
            "add".as_ref(),
            "origin".as_ref(),
            url.as_ref(),
        ])
    }
}
fn commit(path: &Path) {
    std::fs::write(path.join("tracked"), b"x").unwrap();
    git(&[
        "-C".as_ref(),
        path.as_os_str(),
        "add".as_ref(),
        "tracked".as_ref(),
    ]);
    git(&[
        "-C".as_ref(),
        path.as_os_str(),
        "-c".as_ref(),
        "user.name=Test".as_ref(),
        "-c".as_ref(),
        "user.email=test@example.invalid".as_ref(),
        "commit".as_ref(),
        "-qm".as_ref(),
        "seed".as_ref(),
    ])
}
fn prefix(path: &Path) -> Vec<OsString> {
    vec!["-C".into(), path.into()]
}
fn config(path: &Path, key: &str) -> String {
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["config", key])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim_end()
    .into()
}

#[test]
fn has_upstream_matrix() {
    let d = TempDir::new("config-upstream").unwrap();
    let bare = d.path().join("bare.git");
    repo(&bare, None);
    git(&[
        "-C".as_ref(),
        bare.as_os_str(),
        "config".as_ref(),
        "core.bare".as_ref(),
        "true".as_ref(),
    ]);
    let seed = d.path().join("seed");
    repo(&seed, Some(&bare.to_string_lossy()));
    commit(&seed);
    git(&[
        "-C".as_ref(),
        seed.as_os_str(),
        "push".as_ref(),
        "-qu".as_ref(),
        "origin".as_ref(),
        "HEAD".as_ref(),
    ]);
    let branch = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&seed)
            .args(["branch", "--show-current"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let tracked = d.path().join("tracked");
    git(&[
        "clone".as_ref(),
        "-q".as_ref(),
        "-b".as_ref(),
        branch.trim().as_ref(),
        bare.as_os_str(),
        tracked.as_os_str(),
    ]);
    let lonely = d.path().join("lonely");
    repo(&lonely, None);
    commit(&lonely);
    assert!(repos_config::has_upstream(&prefix(&tracked)));
    assert!(!repos_config::has_upstream(&prefix(&lonely)));
    assert!(!repos_config::has_upstream(&prefix(
        &d.path().join("missing")
    )));
}

#[test]
fn is_worktree_matrix() {
    let d = TempDir::new("config-worktree").unwrap();
    let real = d.path().join("real");
    repo(&real, None);
    commit(&real);
    let linked = d.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", real.join(".git").display()),
    )
    .unwrap();
    let plain = d.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let file = d.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    let alias = d.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    for (p, w) in [
        (&real, true),
        (&linked, true),
        (&plain, false),
        (&file, false),
        (&d.path().join("missing"), false),
        (&alias, true),
    ] {
        assert_eq!(repos_config::is_worktree(p), w)
    }
}

#[test]
fn effective_url_matrix() {
    let home = "/home/test";
    for (url, want) in [
        ("~", "/home/test"),
        ("~/x", "/home/test/x"),
        ("/abs", "/abs"),
        ("C:/win", "C:/win"),
        ("C:\\win", "C:\\win"),
        ("host:path", "host:path"),
        ("relative", "/home/test/relative"),
        ("./rel", "/home/test/./rel"),
        ("", "/home/test/"),
    ] {
        assert_eq!(repos_config::effective_url(url, home), want)
    }
}

#[test]
fn origin_matches_matrix() {
    let d = TempDir::new("config-origin").unwrap();
    let a = "https://example/a.git";
    let b = "https://example/b.git";
    let missing = d.path().join("missing");
    repo(&missing, None);
    let same = d.path().join("same");
    repo(&same, Some(a));
    let diff = d.path().join("diff");
    repo(&diff, Some(b));
    let multi = d.path().join("multi");
    repo(&multi, None);
    git(&[
        "-C".as_ref(),
        multi.as_os_str(),
        "config".as_ref(),
        "--add".as_ref(),
        "remote.origin.url".as_ref(),
        a.as_ref(),
    ]);
    git(&[
        "-C".as_ref(),
        multi.as_os_str(),
        "config".as_ref(),
        "--add".as_ref(),
        "remote.origin.url".as_ref(),
        b.as_ref(),
    ]);
    for (p, expected, want) in [
        (&missing, a, (false, "<missing>")),
        (&same, a, (true, a)),
        (&diff, a, (false, b)),
        (&multi, a, (false, "<multiple origin URLs>")),
    ] {
        assert_eq!(
            repos_config::origin_matches(p, expected),
            (want.0, want.1.into())
        )
    }
}

#[test]
fn ensure_repo_config_sets_defaults() {
    let d = TempDir::new("config-set").unwrap();
    repo(d.path(), None);
    git(&[
        "-C".as_ref(),
        d.path().as_os_str(),
        "config".as_ref(),
        "core.fsmonitor".as_ref(),
        "true".as_ref(),
    ]);
    git(&[
        "-C".as_ref(),
        d.path().as_os_str(),
        "config".as_ref(),
        "status.showUntrackedFiles".as_ref(),
        "yes".as_ref(),
    ]);
    repos_config::ensure_repo_config(Some(&prefix(d.path())));
    assert_eq!(config(d.path(), "core.fsmonitor"), "false");
    assert_eq!(config(d.path(), "status.showUntrackedFiles"), "no")
}

#[test]
fn ensure_repo_config_is_idempotent() {
    let d = TempDir::new("config-idempotent").unwrap();
    repo(d.path(), None);
    git(&[
        "-C".as_ref(),
        d.path().as_os_str(),
        "config".as_ref(),
        "core.fsmonitor".as_ref(),
        "false".as_ref(),
    ]);
    git(&[
        "-C".as_ref(),
        d.path().as_os_str(),
        "config".as_ref(),
        "status.showUntrackedFiles".as_ref(),
        "no".as_ref(),
    ]);
    repos_config::ensure_repo_config(Some(&prefix(d.path())));
    repos_config::ensure_repo_config(Some(&prefix(d.path())));
    assert_eq!(config(d.path(), "core.fsmonitor"), "false");
    assert_eq!(config(d.path(), "status.showUntrackedFiles"), "no")
}

#[test]
fn ensure_repo_config_missing_base_is_noop() {
    let d = TempDir::new("config-none").unwrap();
    repo(d.path(), None);
    git(&[
        "-C".as_ref(),
        d.path().as_os_str(),
        "config".as_ref(),
        "core.fsmonitor".as_ref(),
        "true".as_ref(),
    ]);
    git(&[
        "-C".as_ref(),
        d.path().as_os_str(),
        "config".as_ref(),
        "status.showUntrackedFiles".as_ref(),
        "yes".as_ref(),
    ]);
    repos_config::ensure_repo_config(None);
    assert_eq!(config(d.path(), "core.fsmonitor"), "true");
    assert_eq!(config(d.path(), "status.showUntrackedFiles"), "yes")
}
