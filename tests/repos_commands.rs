//! Native contracts for fetch, push, diff, and status repository commands.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::log::Log;
use dot::repos_base::{Base, RepoKind, Topology};
use dot::repos_commands::{
    diff_all, diff_one, fetch_all, fetch_one, header_text, push_all, push_one, status_all,
    status_one,
};
use dot_test_support::TempDir;

fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("host git")
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new(real_git())
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_wrapper(scope: &Path) -> PathBuf {
    let wrapper = scope.join("git-wrapper");
    let stage = scope.join(".git-wrapper.stage");
    let real = real_git().to_string_lossy().replace('\'', "'\\''");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)
        .expect("stage git wrapper");
    write!(
        file,
        "#!/bin/sh\nexec '{real}' -c core.hooksPath=/dev/null -c commit.gpgsign=false -c tag.gpgsign=false -c user.name=fixture -c user.email=fixture@example.invalid \"$@\"\n"
    )
    .expect("write git wrapper");
    file.flush().expect("flush git wrapper");
    drop(file);
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o755)).expect("wrapper mode");
    std::fs::rename(stage, &wrapper).expect("publish git wrapper");
    wrapper
}

fn clone_repo(scope: &Path, name: &str) -> (PathBuf, PathBuf) {
    let origin = scope.join(format!("{name}.git"));
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(&origin, &["init", "--bare", "-q"]);
    let seed = scope.join(format!("{name}-seed"));
    git(
        scope,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    std::fs::write(seed.join("tracked"), b"one\n").expect("seed bytes");
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-qm", "seed"]);
    git(&seed, &["push", "-q", "origin", "HEAD"]);
    let work = scope.join(format!("{name}-work"));
    git(
        scope,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    (work, origin)
}

fn base(path: &Path) -> Base {
    Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: path.to_string_lossy().into_owned(),
    }
}

fn overlay(name: &str, path: &Path, sync: &str) -> String {
    format!(
        "{name}|{}|file:///unused|/tmp/{name}.conf|false|{sync}",
        path.display()
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn bound<T>(scope: &Path, run: impl FnOnce() -> T) -> T {
    dot::init_client_identity::with_host_git(&write_wrapper(scope), run)
}

#[test]
fn header_table_is_literal_and_unknown_operations_are_silent() {
    for (operation, kind, name, expected) in [
        (
            "fetch",
            RepoKind::Base,
            "dotfiles",
            "==> Fetching dotfiles...\n",
        ),
        (
            "fetch",
            RepoKind::Overlay,
            "web",
            "==> Fetching web dotfiles...\n",
        ),
        (
            "push",
            RepoKind::Base,
            "dotfiles",
            "==> Pushing dotfiles...\n",
        ),
        (
            "push",
            RepoKind::Overlay,
            "web",
            "==> Pushing web dotfiles...\n",
        ),
        ("diff", RepoKind::Base, "dotfiles", "==> dotfiles\n"),
        ("diff", RepoKind::Overlay, "web", "\n==> web dotfiles\n"),
        ("status", RepoKind::Base, "dotfiles", "==> dotfiles\n"),
        ("status", RepoKind::Overlay, "web", "\n==> web dotfiles\n"),
    ] {
        assert_eq!(
            header_text(operation, kind, name).as_deref(),
            Some(expected)
        );
    }
    for operation in ["", "bogus", "FETCH", "fetch "] {
        assert_eq!(header_text(operation, RepoKind::Base, "dotfiles"), None);
        assert_eq!(header_text(operation, RepoKind::Overlay, "web"), None);
    }
}

#[test]
fn fetch_one_prints_exact_headers_and_clamps_fetch_head_for_both_kinds() {
    let scope = TempDir::new_exec("commands-fetch-one").unwrap();
    let (base_repo, _) = clone_repo(scope.path(), "base");
    let (overlay_repo, _) = clone_repo(scope.path(), "overlay");
    let model = base(&base_repo);
    bound(scope.path(), || {
        for (kind, name, path, expected) in [
            (
                RepoKind::Base,
                "dotfiles",
                &base_repo,
                "==> Fetching dotfiles...\n",
            ),
            (
                RepoKind::Overlay,
                "web",
                &overlay_repo,
                "==> Fetching web dotfiles...\n",
            ),
        ] {
            let mut out = Vec::new();
            assert_eq!(
                fetch_one(
                    &Log::new(false, false),
                    &mut out,
                    &model,
                    kind,
                    name,
                    path.to_str().unwrap(),
                    &[],
                    0o022
                ),
                0
            );
            assert_eq!(out, expected.as_bytes());
            assert_eq!(mode(&path.join(".git/FETCH_HEAD")), 0o600);
        }
    });
}

#[test]
fn push_one_distinguishes_hard_base_failure_from_soft_overlay_warning() {
    let scope = TempDir::new_exec("commands-push-failure").unwrap();
    let repo = scope.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let model = base(&repo);
    bound(scope.path(), || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            push_one(
                &Log::new(false, false),
                &mut out,
                &mut err,
                &model,
                RepoKind::Base,
                "dotfiles",
                repo.to_str().unwrap(),
                &[]
            ),
            1
        );
        assert_eq!(out, b"==> Pushing dotfiles...\n");
        assert!(err.is_empty());

        out.clear();
        assert_eq!(
            push_one(
                &Log::new(false, false),
                &mut out,
                &mut err,
                &model,
                RepoKind::Overlay,
                "web",
                repo.to_str().unwrap(),
                &[]
            ),
            0
        );
        assert_eq!(out, b"==> Pushing web dotfiles...\n");
        assert_eq!(err, b"  warning: web dotfiles push failed\n");
    });
}

#[test]
fn diff_one_propagates_exit_code_and_status_one_prints_even_when_quiet() {
    let scope = TempDir::new_exec("commands-inspect-one").unwrap();
    let (repo, _) = clone_repo(scope.path(), "base");
    let model = base(&repo);
    bound(scope.path(), || {
        let mut out = Vec::new();
        assert_eq!(
            diff_one(
                &Log::new(false, false),
                &mut out,
                &model,
                RepoKind::Base,
                "dotfiles",
                repo.to_str().unwrap(),
                &["--exit-code"]
            ),
            0
        );
        assert_eq!(out, b"==> dotfiles\n");
        std::fs::write(repo.join("tracked"), b"two\n").unwrap();
        out.clear();
        assert_eq!(
            diff_one(
                &Log::new(false, false),
                &mut out,
                &model,
                RepoKind::Base,
                "dotfiles",
                repo.to_str().unwrap(),
                &["--exit-code"]
            ),
            1
        );
        assert_eq!(out, b"==> dotfiles\n");
        out.clear();
        assert_eq!(
            status_one(
                &Log::new(false, true),
                &mut out,
                &model,
                RepoKind::Overlay,
                "web",
                repo.to_str().unwrap(),
                &["--short"]
            ),
            0
        );
        assert_eq!(out, b"\n==> web dotfiles\n");
    });
}

#[test]
fn all_commands_keep_base_then_overlay_order_and_skip_non_git_records() {
    let scope = TempDir::new_exec("commands-all-order").unwrap();
    let (base_repo, _) = clone_repo(scope.path(), "base");
    let (one, _) = clone_repo(scope.path(), "one");
    let (two, _) = clone_repo(scope.path(), "two");
    let model = base(&base_repo);
    let records = vec![
        overlay("one", &one, "git"),
        overlay("local", &scope.path().join("local"), "none"),
        overlay("missing", &scope.path().join("missing"), "git"),
        overlay("two", &two, "git"),
    ];
    let expected_fetch =
        b"==> Fetching dotfiles...\n==> Fetching one dotfiles...\n==> Fetching two dotfiles...\n";
    let expected_inspect = b"==> dotfiles\n\n==> one dotfiles\n\n==> two dotfiles\n";
    let expected_push =
        b"==> Pushing dotfiles...\n==> Pushing one dotfiles...\n==> Pushing two dotfiles...\n";
    bound(scope.path(), || {
        let log = Log::new(false, false);
        let mut out = Vec::new();
        assert_eq!(
            fetch_all(
                &log,
                &mut out,
                &model,
                &records,
                model.home.as_str(),
                &[],
                0o022
            ),
            0
        );
        assert_eq!(out, expected_fetch);
        out.clear();
        assert_eq!(
            status_all(
                &log,
                &mut out,
                &model,
                &records,
                model.home.as_str(),
                &[OsString::from("--short")]
            ),
            0
        );
        assert_eq!(out, expected_inspect);
        out.clear();
        assert_eq!(
            diff_all(
                &log,
                &mut out,
                &model,
                &records,
                model.home.as_str(),
                &[OsString::from("--exit-code")]
            ),
            0
        );
        assert_eq!(out, expected_inspect);
        out.clear();
        let mut err = Vec::new();
        assert_eq!(
            push_all(
                &log,
                &mut out,
                &mut err,
                &model,
                &records,
                model.home.as_str(),
                &[]
            ),
            0
        );
        assert_eq!(out, expected_push);
        assert!(err.is_empty());
        for repo in [&base_repo, &one, &two] {
            assert_eq!(mode(&repo.join(".git/FETCH_HEAD")), 0o600);
        }
    });
}

#[test]
fn push_all_stops_after_base_failure_before_overlay_side_effects() {
    let scope = TempDir::new_exec("commands-push-stop").unwrap();
    let base_repo = scope.path().join("base");
    std::fs::create_dir(&base_repo).unwrap();
    git(&base_repo, &["init", "-q"]);
    let (overlay_repo, origin) = clone_repo(scope.path(), "overlay");
    let before = String::from_utf8(git(&origin, &["rev-parse", "HEAD"]).stdout).unwrap();
    std::fs::write(overlay_repo.join("tracked"), b"two\n").unwrap();
    git(&overlay_repo, &["add", "-A"]);
    git(&overlay_repo, &["commit", "-qm", "ahead"]);
    let model = base(&base_repo);
    let records = vec![overlay("web", &overlay_repo, "git")];
    bound(scope.path(), || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            push_all(
                &Log::new(false, false),
                &mut out,
                &mut err,
                &model,
                &records,
                model.home.as_str(),
                &[]
            ),
            1
        );
        assert_eq!(out, b"==> Pushing dotfiles...\n");
        assert!(err.is_empty());
    });
    assert_eq!(
        String::from_utf8(git(&origin, &["rev-parse", "HEAD"]).stdout).unwrap(),
        before
    );
}

#[test]
fn missing_base_and_empty_overlay_set_are_successful_noops() {
    let scope = TempDir::new_exec("commands-missing").unwrap();
    let model = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: scope.path().to_string_lossy().into_owned(),
    };
    bound(scope.path(), || {
        for operation in ["fetch", "status", "diff", "push"] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let rc = match operation {
                "fetch" => fetch_all(
                    &Log::new(false, false),
                    &mut out,
                    &model,
                    &[],
                    &model.home,
                    &[],
                    0o022,
                ),
                "status" => status_all(
                    &Log::new(false, false),
                    &mut out,
                    &model,
                    &[],
                    &model.home,
                    &[],
                ),
                "diff" => diff_all(
                    &Log::new(false, false),
                    &mut out,
                    &model,
                    &[],
                    &model.home,
                    &[],
                ),
                "push" => push_all(
                    &Log::new(false, false),
                    &mut out,
                    &mut err,
                    &model,
                    &[],
                    &model.home,
                    &[],
                ),
                _ => unreachable!(),
            };
            assert_eq!(rc, 0);
            assert!(out.is_empty());
            assert!(err.is_empty());
        }
    });
}

#[test]
fn extra_arguments_cross_as_distinct_git_words() {
    let scope = TempDir::new_exec("commands-extra-argv").unwrap();
    let (repo, _) = clone_repo(scope.path(), "base");
    let model = base(&repo);
    bound(scope.path(), || {
        let mut out = Vec::new();
        assert_eq!(
            status_all(
                &Log::new(false, false),
                &mut out,
                &model,
                &[],
                &model.home,
                &[
                    OsString::from("--short"),
                    OsString::from("--"),
                    OsString::from("tracked")
                ],
            ),
            0
        );
        assert_eq!(out, b"==> dotfiles\n");
    });
}

#[test]
fn overlay_record_helper_keeps_all_six_fields() {
    let path = Path::new("/tmp/example");
    assert_eq!(
        overlay("web", path, "git").split('|').collect::<Vec<_>>(),
        vec![
            "web",
            "/tmp/example",
            "file:///unused",
            "/tmp/web.conf",
            "false",
            "git"
        ]
    );
}
