//! Differential parity tests for `src/repos_pull_queries.rs` against
//! the live shell (`lib/dot/repos/pull.sh`): the checked-out
//! generation query, upstream containment, and generation identity.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::repos_pull_queries::{repo_head, repo_head_contains_upstream, repo_head_is};
use dot::test_support::TempDir;

/// Sources plus the init stub `model.sh` needs at source time.
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the repos libraries sourced.
fn shell_run(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
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
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Run `git -C dir args`, silenced, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

/// `git init` plus one tracked committed file.
fn seed_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("tracked.txt"), b"v1\n").expect("seed file");
    git(
        dir,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "seed",
        ],
    );
}

/// HEAD sha of a fixture, via direct git.
fn head_of(dir: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "rev-parse HEAD");
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

#[test]
fn repo_head_reports_generation_or_empty() {
    let dir = TempDir::new("pull-head").expect("fixture dir");
    let repo = dir.path().join("repo");
    seed_repo(&repo);
    let repo_text = repo.to_string_lossy().into_owned();
    let missing = dir.path().join("gone");
    let missing_text = missing.to_string_lossy().into_owned();
    let snippet = format!(
        "echo \"have=$(_repo_head git -C {repo_text})\"\n\
         echo \"missing=$(_repo_head git -C {missing_text})\"\n"
    );
    let (shell_status, shell_out, shell_err) = shell_run(dir.path(), &[], &[], &snippet);
    assert_eq!(shell_status, 0, "harness exit");
    assert!(shell_err.is_empty(), "shell stderr: {shell_err:?}");
    let shell_text = String::from_utf8_lossy(&shell_out).into_owned();
    let prefix = ["-C".to_string(), repo_text.clone()];
    let prefix_os: Vec<std::ffi::OsString> = prefix.iter().map(std::ffi::OsString::from).collect();
    let rust_head = repo_head(&prefix_os);
    assert_eq!(rust_head, head_of(&repo), "head sha parity");
    assert!(
        shell_text.contains(&format!("have={rust_head}\n")),
        "shell reports same sha: {shell_text:?}"
    );
    assert!(
        shell_text.contains("missing=\n"),
        "shell reports empty for missing repo: {shell_text:?}"
    );
    let missing_prefix: Vec<std::ffi::OsString> = ["-C", &missing_text]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    assert_eq!(
        repo_head(&missing_prefix),
        "",
        "rust reports empty for missing repo"
    );
}

#[test]
fn head_contains_upstream_matches_shell_gate() {
    let dir = TempDir::new("pull-contains").expect("fixture dir");
    let repo = dir.path().join("repo");
    seed_repo(&repo);
    let head = head_of(&repo);
    // Second commit: the first generation is a true ancestor.
    std::fs::write(repo.join("tracked.txt"), b"v2\n").expect("advance file");
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qam",
            "second",
        ],
    );
    let child = head_of(&repo);
    assert_ne!(head, child, "fixture must advance");
    let repo_text = repo.to_string_lossy().into_owned();
    let prefix = ["-C".to_string(), repo_text.clone()];
    let prefix_os: Vec<std::ffi::OsString> = prefix.iter().map(std::ffi::OsString::from).collect();
    // (head, upstream, shell-expect): equality short-circuits true,
    // empties refuse, ancestry probes git, strangers fail.
    for (have, upstream, want) in [
        (child.clone(), child.clone(), true),
        (String::new(), child.clone(), false),
        (child.clone(), String::new(), false),
        (child.clone(), head.clone(), true),
        (head.clone(), child.clone(), false),
    ] {
        let snippet = format!(
            "if _repo_head_contains_upstream \"{have}\" \"{upstream}\" git -C {repo_text}; then echo yes; else echo no; fi\n"
        );
        let (shell_status, shell_out, shell_err) = shell_run(dir.path(), &[], &[], &snippet);
        assert_eq!(shell_status, 0, "harness exit");
        assert!(shell_err.is_empty(), "shell stderr: {shell_err:?}");
        let shell_yes = shell_out.starts_with(b"yes\n");
        assert_eq!(shell_yes, want, "shell gate for {have}/{upstream}");
        assert_eq!(
            repo_head_contains_upstream(&prefix_os, &have, &upstream),
            want,
            "rust gate for {have}/{upstream}"
        );
    }
    // Identity pins through `_repo_head_is` on both sides.
    let snippet = format!(
        "if _repo_head_is \"{child}\" git -C {repo_text}; then echo yes; else echo no; fi\n\
         if _repo_head_is \"{head}\" git -C {repo_text}; then echo yes; else echo no; fi\n\
         if _repo_head_is \"\" git -C {repo_text}; then echo yes; else echo no; fi\n"
    );
    let (shell_status, shell_out, _) = shell_run(dir.path(), &[], &[], &snippet);
    assert_eq!(shell_status, 0, "harness exit");
    assert_eq!(shell_out, b"yes\nno\nno\n", "shell identity rows");
    assert!(repo_head_is(&prefix_os, &child));
    assert!(!repo_head_is(&prefix_os, &head));
    assert!(!repo_head_is(&prefix_os, ""));
}
