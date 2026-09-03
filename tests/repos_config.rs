//! Differential parity tests for the slice-11 config track against
//! the live `lib/dot/repos/config.sh`: upstream detection, worktree
//! detection, effective-URL resolution, origin comparison, and
//! repo-config enforcement — exit codes, exact stdout bytes, and
//! empty stderr throughout.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::repos_config;
use dot::test_support::TempDir;

/// Run one shell snippet with ONLY the config library sourced.
fn shell_run(home: &Path, argv: &[OsString], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(". \"$1/lib/dot/repos/config.sh\"\n{snippet}"));
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

/// Run `git` silently; the fixture command must succeed.
fn git(args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// `git init -q` plus an optional origin.
fn git_repo(path: &Path, origin: Option<&str>) {
    let dir = path.to_string_lossy().into_owned();
    git(&["init", "-q", &dir]);
    if let Some(url) = origin {
        git(&["-C", &dir, "remote", "add", "origin", url]);
    }
}

/// Empty commit with pinned identity (`-c` flags, no global state).
fn git_commit(dir: &Path) {
    let dir = dir.to_string_lossy().into_owned();
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "-c",
        "commit.gpgsign=false",
        "-C",
        &dir,
        "commit",
        "--allow-empty",
        "-qm",
        "init",
    ]);
}

/// `git config` read with shell `$(... || true)` semantics: "" on failure.
fn config_get(repo: &Path, extra: &[&str], key: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("config")
        .args(extra)
        .arg(key)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("git config get");
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    } else {
        String::new()
    }
}

/// Run the shell `_ensure_repo_config` against `repo`
/// (`base_present` selects the `_base_repo_exists` stub outcome).
fn shell_ensure(home: &Path, repo: &Path, base_present: bool) -> (i32, Vec<u8>, Vec<u8>) {
    let gate = if base_present { "return 0" } else { "return 1" };
    let argv = [OsString::from(repo)];
    shell_run(
        home,
        &argv,
        &format!(
            "fix=\"$2\"\n_base_repo_exists() {{ {gate}; }}\n_base_git() {{ git -C \"$fix\" \"$@\"; }}\n_ensure_repo_config\nprintf 'rc=%s\\n' \"$?\""
        ),
    )
}

/// Run the Rust [`repos_config::ensure_repo_config`] against `repo`
/// (`None` mirrors a missing base topology).
fn rust_ensure(repo: Option<&Path>) {
    let prefix = repo.map(|dir| vec![OsString::from("-C"), OsString::from(dir)]);
    repos_config::ensure_repo_config(prefix.as_deref());
}

#[test]
fn has_upstream_agrees() {
    let dir = TempDir::new("cfg-upstream").expect("fixture dir");
    let home = dir.path();
    // With upstream: bare repo, seed clone committing with `-c`
    // identity flags, pushed, then a fresh clone (tracks origin).
    let bare = home.join("bare.git");
    let seed = home.join("seed");
    let tracked = home.join("tracked");
    let bare_s = bare.to_string_lossy().into_owned();
    let seed_s = seed.to_string_lossy().into_owned();
    let tracked_s = tracked.to_string_lossy().into_owned();
    git(&["init", "--bare", "-q", &bare_s]);
    git(&["clone", "-q", &bare_s, &seed_s]);
    git_commit(&seed);
    git(&["-C", &seed_s, "push", "-q", "-u", "origin", "HEAD"]);
    git(&["clone", "-q", &bare_s, &tracked_s]);
    // Without upstream: local commits, no remote.
    let lonely = home.join("lonely");
    git_repo(&lonely, None);
    git_commit(&lonely);
    let missing = home.join("missing");
    // The shell propagates git's raw code (128 for no-upstream),
    // so failure cases assert nonzero rather than a fixed number.
    for (repo, want_ok) in [
        (tracked.as_path(), true),
        (lonely.as_path(), false),
        (missing.as_path(), false),
    ] {
        let argv = [OsString::from(repo)];
        let (code, out, err) = shell_run(
            home,
            &argv,
            "_repo_has_upstream git -C \"$2\"; printf 'rc=%s\\n' \"$?\"",
        );
        assert_eq!(code, 0, "shell harness {}", repo.display());
        assert_eq!(err, b"", "upstream stderr {}", repo.display());
        let dump = String::from_utf8(out).expect("upstream dump");
        let shell_rc: i32 = dump
            .strip_prefix("rc=")
            .and_then(|rest| rest.trim_end().parse().ok())
            .unwrap_or(-1);
        if want_ok {
            assert_eq!(dump, "rc=0\n", "shell upstream {}", repo.display());
        } else {
            assert_ne!(shell_rc, 0, "shell upstream {}", repo.display());
            assert_eq!(dump, format!("rc={shell_rc}\n"), "upstream bytes");
        }
        let prefix = [OsString::from("-C"), OsString::from(repo)];
        assert_eq!(
            repos_config::has_upstream(&prefix),
            want_ok,
            "rust upstream {} (shell rc={shell_rc})",
            repo.display()
        );
    }
}

#[test]
fn is_worktree_agrees() {
    let dir = TempDir::new("cfg-worktree").expect("fixture dir");
    let home = dir.path();
    // Real checkout, `.git`-file checkout (hand-made gitfile at a
    // real git dir), plain dir, regular file, missing path, and a
    // symlink to a checkout (shell `cd -P` follows it).
    let real = home.join("real");
    git_repo(&real, None);
    git_commit(&real);
    let linked = home.join("linked");
    std::fs::create_dir_all(&linked).expect("linked dir");
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", real.join(".git").to_string_lossy()),
    )
    .expect("gitfile");
    let plain = home.join("plain");
    std::fs::create_dir_all(&plain).expect("plain dir");
    let file = home.join("file");
    std::fs::write(&file, "data").expect("file fixture");
    let missing = home.join("missing");
    let alias = home.join("alias");
    std::os::unix::fs::symlink(&real, &alias).expect("symlink");
    for (repo, want) in [
        (real.as_path(), true),
        (linked.as_path(), true),
        (plain.as_path(), false),
        (file.as_path(), false),
        (missing.as_path(), false),
        (alias.as_path(), true),
    ] {
        let argv = [OsString::from(repo)];
        let (code, out, err) = shell_run(
            home,
            &argv,
            "_overlay_is_worktree \"$2\"; printf 'rc=%s\\n' \"$?\"",
        );
        assert_eq!(code, 0, "shell harness {}", repo.display());
        assert_eq!(err, b"", "worktree stderr {}", repo.display());
        let want_rc = if want { 0 } else { 1 };
        assert_eq!(
            String::from_utf8(out).expect("worktree dump"),
            format!("rc={want_rc}\n"),
            "shell worktree {}",
            repo.display()
        );
        assert_eq!(
            repos_config::is_worktree(repo),
            want,
            "rust worktree {}",
            repo.display()
        );
    }
}

#[test]
fn effective_url_matrix_agrees() {
    let dir = TempDir::new("cfg-url").expect("fixture dir");
    let home = dir.path();
    let home_s = home.to_string_lossy().into_owned();
    for url in [
        "~",
        "~/x",
        "/abs",
        "C:/win",
        "C:\\win",
        "host:path",
        "relative",
        "./rel",
        "",
    ] {
        let argv = [OsString::from(url)];
        let (code, out, err) = shell_run(
            home,
            &argv,
            "_overlay_effective_url \"$2\"; printf 'rc=%s\\nreply=%s\\n' \"$?\" \"$REPLY\"",
        );
        assert_eq!(code, 0, "shell harness {url:?}");
        assert_eq!(err, b"", "url stderr {url:?}");
        let rust = repos_config::effective_url(url, &home_s);
        assert_eq!(
            out,
            format!("rc=0\nreply={rust}\n").into_bytes(),
            "effective url {url:?}"
        );
    }
}

#[test]
fn origin_matches_agrees() {
    let dir = TempDir::new("cfg-origin").expect("fixture dir");
    let home = dir.path();
    let url_a = "https://example.test/a.git";
    let url_b = "https://example.test/b.git";
    // 0 urls / 1 match / 1 mismatch / 2 urls.
    let bare = home.join("bare");
    git_repo(&bare, None);
    let same = home.join("same");
    git_repo(&same, Some(url_a));
    let diff = home.join("diff");
    git_repo(&diff, Some(url_b));
    let multi = home.join("multi");
    git_repo(&multi, None);
    let multi_s = multi.to_string_lossy().into_owned();
    git(&[
        "-C",
        &multi_s,
        "config",
        "--add",
        "remote.origin.url",
        url_a,
    ]);
    git(&[
        "-C",
        &multi_s,
        "config",
        "--add",
        "remote.origin.url",
        url_b,
    ]);
    for (repo, expected, want_rc, want_reply) in [
        (bare.as_path(), url_a, 1, "<missing>"),
        (same.as_path(), url_a, 0, url_a),
        (diff.as_path(), url_a, 1, url_b),
        (multi.as_path(), url_a, 1, "<multiple origin URLs>"),
    ] {
        let argv = [OsString::from(repo), OsString::from(expected)];
        let (code, out, err) = shell_run(
            home,
            &argv,
            "_overlay_origin_matches \"$2\" \"$3\"; printf 'rc=%s\\nreply=%s\\n' \"$?\" \"$REPLY\"",
        );
        assert_eq!(code, 0, "shell harness {}", repo.display());
        assert_eq!(err, b"", "origin stderr {}", repo.display());
        assert_eq!(
            out,
            format!("rc={want_rc}\nreply={want_reply}\n").into_bytes(),
            "shell origin {}",
            repo.display()
        );
        assert_eq!(
            repos_config::origin_matches(repo, expected),
            (want_rc == 0, want_reply.to_string()),
            "rust origin {}",
            repo.display()
        );
    }
}

#[test]
fn ensure_repo_config_sets_defaults() {
    let dir = TempDir::new("cfg-ensure-set").expect("fixture dir");
    let home = dir.path();
    let shell_repo = home.join("shell");
    let rust_repo = home.join("rust");
    for repo in [&shell_repo, &rust_repo] {
        git_repo(repo, None);
        let dir_s = repo.to_string_lossy().into_owned();
        git(&["-C", &dir_s, "config", "core.fsmonitor", "true"]);
        git(&["-C", &dir_s, "config", "status.showUntrackedFiles", "yes"]);
    }
    let (code, out, err) = shell_ensure(home, &shell_repo, true);
    assert_eq!(code, 0, "shell harness");
    assert_eq!(out, b"rc=0\n", "ensure dump");
    assert_eq!(err, b"", "ensure stderr");
    rust_ensure(Some(&rust_repo));
    for repo in [&shell_repo, &rust_repo] {
        assert_eq!(
            config_get(repo, &["--bool"], "core.fsmonitor"),
            "false",
            "fsmonitor {}",
            repo.display()
        );
        assert_eq!(
            config_get(repo, &[], "status.showUntrackedFiles"),
            "no",
            "untracked {}",
            repo.display()
        );
    }
}

#[test]
fn ensure_repo_config_idempotent() {
    let dir = TempDir::new("cfg-ensure-idem").expect("fixture dir");
    let home = dir.path();
    let shell_repo = home.join("shell");
    let rust_repo = home.join("rust");
    for repo in [&shell_repo, &rust_repo] {
        git_repo(repo, None);
        let dir_s = repo.to_string_lossy().into_owned();
        git(&["-C", &dir_s, "config", "core.fsmonitor", "false"]);
        git(&["-C", &dir_s, "config", "status.showUntrackedFiles", "no"]);
    }
    let (code, out, err) = shell_ensure(home, &shell_repo, true);
    assert_eq!(code, 0, "shell harness");
    assert_eq!(out, b"rc=0\n", "ensure dump");
    assert_eq!(err, b"", "ensure stderr");
    rust_ensure(Some(&rust_repo));
    for repo in [&shell_repo, &rust_repo] {
        assert_eq!(
            config_get(repo, &["--bool"], "core.fsmonitor"),
            "false",
            "fsmonitor {}",
            repo.display()
        );
        assert_eq!(
            config_get(repo, &[], "status.showUntrackedFiles"),
            "no",
            "untracked {}",
            repo.display()
        );
    }
}

#[test]
fn ensure_repo_config_missing_base_silent() {
    let dir = TempDir::new("cfg-ensure-none").expect("fixture dir");
    let home = dir.path();
    let shell_repo = home.join("shell");
    let rust_repo = home.join("rust");
    for repo in [&shell_repo, &rust_repo] {
        git_repo(repo, None);
        let dir_s = repo.to_string_lossy().into_owned();
        git(&["-C", &dir_s, "config", "core.fsmonitor", "true"]);
        git(&["-C", &dir_s, "config", "status.showUntrackedFiles", "yes"]);
    }
    let (code, out, err) = shell_ensure(home, &shell_repo, false);
    assert_eq!(code, 0, "shell harness");
    assert_eq!(out, b"rc=0\n", "ensure dump");
    assert_eq!(err, b"", "ensure stderr");
    rust_ensure(None);
    // Neither engine touches the repos: wrong values survive.
    for repo in [&shell_repo, &rust_repo] {
        assert_eq!(
            config_get(repo, &["--bool"], "core.fsmonitor"),
            "true",
            "fsmonitor {}",
            repo.display()
        );
        assert_eq!(
            config_get(repo, &[], "status.showUntrackedFiles"),
            "yes",
            "untracked {}",
            repo.display()
        );
    }
}
