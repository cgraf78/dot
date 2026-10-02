//! Contracts for the shared fixture-Git isolation in `dot_test_support`.
//!
//! Every fixture repository in the suite is built through
//! `dot_test_support::git()`. These cases plant each known channel of
//! developer Git state on the command before isolation is applied (the
//! same map entries an inherited environment would occupy) and prove
//! that none of it reaches the fixture.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use dot_test_support::{GIT_USER_EMAIL, GIT_USER_NAME, TempDir, isolate_git, real_tool};

/// Developer state a fixture must never see: a global config that signs
/// with a failing program, rewrites URLs, and ignores `*.txt`; an XDG
/// ignore and attributes file; config injected through both environment
/// channels; SHA-256 and reftable repository defaults; a foreign
/// repository and index; and a foreign identity.
fn hostile(command: &mut Command, root: &Path) {
    let global = root.join("hostile.gitconfig");
    std::fs::write(
        &global,
        "[commit]\n\tgpgSign = true\n[tag]\n\tgpgSign = true\n[gpg]\n\tprogram = false\n\
         [url \"file:///nonexistent/\"]\n\tinsteadOf = file://\n\
         [core]\n\texcludesFile = ~/.hostile-ignore\n",
    )
    .expect("hostile global config");
    let xdg = root.join("xdg");
    std::fs::create_dir_all(xdg.join("git")).expect("hostile xdg");
    std::fs::write(xdg.join("git/ignore"), "*.txt\n").expect("hostile ignore");
    std::fs::write(xdg.join("git/attributes"), "* text eol=crlf\n").expect("hostile attributes");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("hostile home");
    std::fs::write(home.join(".hostile-ignore"), "*.txt\n").expect("hostile excludes");
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Hostile\n\temail = hostile@example.invalid\n",
    )
    .expect("hostile home config");
    command
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_PARAMETERS", "'commit.gpgsign'='true'")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgSign")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("GIT_DIR", root.join("foreign.git"))
        .env("GIT_WORK_TREE", root.join("foreign"))
        .env("GIT_INDEX_FILE", root.join("foreign.index"))
        .env("GIT_DEFAULT_HASH", "sha256")
        .env("GIT_DEFAULT_REF_FORMAT", "reftable")
        .env("GIT_AUTHOR_NAME", "Hostile")
        .env("GIT_COMMITTER_EMAIL", "hostile@example.invalid");
}

fn run(root: &Path, repo: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(real_tool("git"));
    hostile(&mut command, root);
    let output = isolate_git(&mut command)
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn fixture git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

#[test]
fn fixture_commits_ignore_developer_git_state() {
    let scope = TempDir::new("fixture-git-hostile").expect("scope");
    let repo = scope.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    run(scope.path(), &repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), b"one\ntwo\n").expect("fixture file");
    run(scope.path(), &repo, &["add", "-A"]);
    // Signing would fail through `gpg.program = false`, and a missing
    // identity would refuse the commit outright.
    run(scope.path(), &repo, &["commit", "-qm", "fixture"]);

    assert_eq!(
        stdout(&run(scope.path(), &repo, &["ls-files"])),
        "a.txt",
        "ignore rules from developer state must not drop fixture files"
    );
    assert_eq!(
        stdout(&run(
            scope.path(),
            &repo,
            &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]
        )),
        format!("{GIT_USER_NAME} <{GIT_USER_EMAIL}>|{GIT_USER_NAME} <{GIT_USER_EMAIL}>"),
    );
    std::fs::remove_file(repo.join("a.txt")).expect("drop worktree copy");
    run(scope.path(), &repo, &["checkout", "--", "a.txt"]);
    assert_eq!(
        std::fs::read(repo.join("a.txt")).expect("checked-out fixture"),
        b"one\ntwo\n",
        "attribute rules from developer state must not rewrite fixture bytes"
    );
    // The configured view is the repository's own config plus the
    // fixture's command scope; nothing from the planted sources.
    let config = stdout(&run(
        scope.path(),
        &repo,
        &["config", "--list", "--show-scope"],
    ));
    for line in config.lines() {
        assert!(
            line.starts_with("local\t") || line.starts_with("command\t"),
            "unexpected config source: {line}"
        );
    }
    assert!(!config.contains("url."), "{config}");
    assert!(
        !config.to_ascii_lowercase().contains("gpgsign=true"),
        "{config}"
    );
    assert_eq!(
        stdout(&run(scope.path(), &repo, &["rev-parse", "HEAD"])).len(),
        40,
        "fixtures keep SHA-1 object names"
    );
    assert!(
        repo.join(".git/refs/heads").is_dir(),
        "fixtures keep the files ref backend"
    );
    assert!(
        !scope.path().join("foreign.git").exists(),
        "an inherited GIT_DIR must not receive fixture writes"
    );
}

#[test]
fn call_site_identity_overrides_the_fixture_default() {
    // Existing fixtures pin their own identity with `-c user.*`; that
    // must keep winning over the shared default.
    let scope = TempDir::new("fixture-git-identity").expect("scope");
    let repo = scope.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    run(scope.path(), &repo, &["init", "-q"]);
    run(
        scope.path(),
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "site",
        ],
    );
    assert_eq!(
        stdout(&run(
            scope.path(),
            &repo,
            &["log", "-1", "--format=%an <%ae>"]
        )),
        "t <t@t>"
    );
}
