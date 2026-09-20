//! Native contracts for repository generation and candidate-tree queries.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::log::Log;
use dot::repos_pull_queries::{
    CandidateEnv, EntryVerdict, accept_current_generation, candidate_adapter_allowed, repo_head,
    repo_head_contains_upstream, repo_head_is, validate_ahead_delta, validate_candidate_entry,
    validate_candidate_tree,
};
use dot::reserved::{RootsInput, reserved_roots};
use dot_test_support::TempDir;

fn log() -> Log {
    Log::new(false, true)
}

fn git_program() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("host git")
}

fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new(git_program())
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
        .arg(repo)
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
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn prefix(repo: &Path) -> Vec<OsString> {
    [
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
    ]
    .into_iter()
    .map(OsString::from)
    .chain(std::iter::once(repo.as_os_str().to_owned()))
    .collect()
}

fn head(repo: &Path) -> String {
    String::from_utf8(git(repo, &["rev-parse", "HEAD"]).stdout)
        .expect("ASCII oid")
        .trim()
        .to_string()
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

fn seed(repo: &Path, files: &[(&str, &[u8], bool)]) {
    std::fs::create_dir_all(repo).expect("repo dir");
    git(repo, &["init", "-q"]);
    for (relative, bytes, executable) in files {
        let path = repo.join(relative);
        std::fs::create_dir_all(path.parent().expect("file parent")).expect("file parent");
        std::fs::write(&path, bytes).expect("fixture file");
        if *executable {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable mode");
        }
    }
    commit_all(repo, "seed");
}

fn commit_symlink(repo: &Path, relative: &str, target: &str) {
    let path = repo.join(relative);
    std::fs::create_dir_all(path.parent().expect("link parent")).expect("link parent");
    std::os::unix::fs::symlink(target, path).expect("fixture symlink");
    commit_all(repo, "link");
}

struct Fixture {
    dir: TempDir,
    env: CandidateEnv,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        let state = home.join(".local/state");
        let install = home.join(".local/share");
        let checkout = install.join("cgraf78/dot");
        std::fs::create_dir_all(&checkout).expect("checkout");
        std::fs::create_dir_all(&state).expect("state");
        let text = |path: &Path| path.to_string_lossy().into_owned();
        let env = CandidateEnv {
            home: text(&home),
            checkout: text(&checkout),
            pwd: text(dir.path()),
            source_root: env!("CARGO_MANIFEST_DIR").to_string(),
            state_home: text(&state),
            install_root: text(&install),
            provider_state: text(&state.join("shdeps")),
            overlay_paths: Vec::new(),
            init_backup: None,
        };
        Self { dir, env }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn roots(&self) -> Vec<String> {
        reserved_roots(
            &RootsInput {
                home: self.env.home.clone(),
                state_home: self.env.state_home.clone(),
                install_root: self.env.install_root.clone(),
                provider_state: self.env.provider_state.clone(),
                overlay_paths: self.env.overlay_paths.clone(),
                init_backup: self.env.init_backup.clone(),
            },
            &self.env.pwd,
        )
        .expect("reserved roots")
    }
}

fn launcher_bytes() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("support/client-launcher.sh"))
        .expect("launcher bytes")
}

fn write_program(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write fixture program");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("fixture program mode");
}

fn shell_word(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[test]
fn repo_head_reports_exact_generation_and_empty_failures() {
    let fixture = Fixture::new("repo-head");
    let repo = fixture.repo();
    seed(&repo, &[("tracked", b"one\n", false)]);
    assert_eq!(repo_head(&prefix(&repo)), head(&repo));
    assert_eq!(repo_head(&prefix(&fixture.dir.path().join("missing"))), "");
    let unborn = fixture.dir.path().join("unborn");
    std::fs::create_dir(&unborn).expect("unborn dir");
    git(&unborn, &["init", "-q"]);
    assert_eq!(repo_head(&prefix(&unborn)), "");
}

#[test]
fn containment_and_identity_cover_equal_ancestor_unrelated_and_empty() {
    let fixture = Fixture::new("repo-containment");
    let repo = fixture.repo();
    seed(&repo, &[("tracked", b"one\n", false)]);
    let base = head(&repo);
    std::fs::write(repo.join("tracked"), b"two\n").expect("advance");
    commit_all(&repo, "advance");
    let ahead = head(&repo);
    git(&repo, &["checkout", "-q", "--orphan", "unrelated"]);
    git(&repo, &["rm", "-q", "-rf", "."]);
    std::fs::write(repo.join("other"), b"other\n").expect("other file");
    commit_all(&repo, "unrelated");
    let unrelated = head(&repo);
    git(&repo, &["checkout", "-q", &ahead]);
    let p = prefix(&repo);
    for (have, upstream, expected) in [
        (ahead.as_str(), ahead.as_str(), true),
        (&ahead, &base, true),
        (&base, &ahead, false),
        (&ahead, &unrelated, false),
        ("", &base, false),
        (&ahead, "", false),
    ] {
        assert_eq!(repo_head_contains_upstream(&p, have, upstream), expected);
    }
    assert!(repo_head_is(&p, &ahead));
    assert!(!repo_head_is(&p, &base));
    assert!(!repo_head_is(&p, ""));
}

#[test]
fn adapter_requires_exact_path_mode_and_launcher_bytes() {
    let fixture = Fixture::new("repo-adapter");
    let repo = fixture.repo();
    let launcher = launcher_bytes();
    seed(&repo, &[(".local/bin/dot", &launcher, true)]);
    let p = prefix(&repo);
    let generation = head(&repo);
    for (path, mode, expected) in [
        (".local/bin/dot", "100755", true),
        (".local/bin/dot", "100644", false),
        (".local/bin/other", "100755", false),
    ] {
        let mut warnings = Vec::new();
        assert_eq!(
            candidate_adapter_allowed(&p, &generation, path, mode, &fixture.env, &mut warnings),
            expected
        );
        assert!(warnings.is_empty());
    }
    std::fs::write(
        repo.join(".local/bin/dot"),
        [launcher, b"# modified\n".to_vec()].concat(),
    )
    .expect("modify launcher");
    commit_all(&repo, "modify launcher");
    assert!(!candidate_adapter_allowed(
        &p,
        &head(&repo),
        ".local/bin/dot",
        "100755",
        &fixture.env,
        &mut Vec::new(),
    ));
}

#[test]
fn entry_policy_pins_modes_profiles_and_reserved_roots() {
    let mut fixture = Fixture::new("repo-entry");
    let repo = fixture.repo();
    seed(&repo, &[("clean", b"one\n", false)]);
    let generation = head(&repo);
    let p = prefix(&repo);
    let oid = "a".repeat(40);
    let rows = [
        (
            "base",
            "100644",
            "blob",
            oid.as_str(),
            "clean",
            EntryVerdict::Accept("clean".into()),
        ),
        (
            "base",
            "100755",
            "blob",
            oid.as_str(),
            "run",
            EntryVerdict::Accept("run".into()),
        ),
        (
            "base",
            "120000",
            "blob",
            oid.as_str(),
            "link",
            EntryVerdict::Accept("link".into()),
        ),
        (
            "base",
            "100600",
            "blob",
            oid.as_str(),
            "bad-mode",
            EntryVerdict::Reject,
        ),
        (
            "base",
            "100644",
            "commit",
            oid.as_str(),
            "submodule",
            EntryVerdict::Reject,
        ),
        (
            "base",
            "100644",
            "blob",
            "xyz",
            "bad-oid",
            EntryVerdict::Reject,
        ),
        (
            "base",
            "100644",
            "blob",
            oid.as_str(),
            "../escape",
            EntryVerdict::Reject,
        ),
        (
            "base",
            "100644",
            "blob",
            oid.as_str(),
            ".config/dot/profiles.d/base.conf",
            EntryVerdict::Accept(".config/dot/profiles.d/base.conf".into()),
        ),
        (
            "base",
            "100644",
            "blob",
            oid.as_str(),
            ".config/dot/profile-selectors.d/host.conf",
            EntryVerdict::Accept(".config/dot/profile-selectors.d/host.conf".into()),
        ),
        (
            "base",
            "100644",
            "blob",
            oid.as_str(),
            ".config/dot/profile-selectors.local.d/host.conf",
            EntryVerdict::Reject,
        ),
        (
            "overlay",
            "100644",
            "blob",
            oid.as_str(),
            ".config/dot/profiles.d/base.conf",
            EntryVerdict::Skip,
        ),
        (
            "overlay",
            "100644",
            "blob",
            oid.as_str(),
            "home/.config/dot/profiles.d/base.conf",
            EntryVerdict::Reject,
        ),
        (
            "overlay",
            "100644",
            "blob",
            oid.as_str(),
            "home/.config/dot/profile-selectors.d/host.conf",
            EntryVerdict::Reject,
        ),
        (
            "overlay",
            "100644",
            "blob",
            oid.as_str(),
            "home/.config/dot/profile-selectors.local.d/host.conf",
            EntryVerdict::Reject,
        ),
        (
            "overlay",
            "100644",
            "blob",
            oid.as_str(),
            "home/ordinary",
            EntryVerdict::Accept("ordinary".into()),
        ),
        (
            "overlay",
            "100644",
            "blob",
            oid.as_str(),
            "home",
            EntryVerdict::Reject,
        ),
    ];
    let roots = fixture.roots();
    for (kind, mode, entry_type, object, path, expected) in rows {
        let actual = validate_candidate_entry(
            &p,
            kind,
            &generation,
            mode,
            entry_type,
            object,
            path,
            &roots,
            &fixture.env,
            &log(),
            &mut Vec::new(),
        );
        assert_eq!(actual, expected, "candidate {kind}:{path}");
    }

    let provider = PathBuf::from(&fixture.env.provider_state);
    std::fs::create_dir_all(&provider).expect("provider state");
    let alias = PathBuf::from(&fixture.env.home).join("provider-alias");
    std::os::unix::fs::symlink(&provider, &alias).expect("provider alias");
    fixture
        .env
        .overlay_paths
        .push(alias.to_string_lossy().into_owned());
    let roots = fixture.roots();
    for relative in [
        ".local/state/shdeps/cache",
        "provider-alias/cache",
        ".dotfiles/config",
        ".local/share/cgraf78/dot/source",
    ] {
        let mut warnings = Vec::new();
        assert_eq!(
            validate_candidate_entry(
                &p,
                "base",
                &generation,
                "100644",
                "blob",
                &oid,
                relative,
                &roots,
                &fixture.env,
                &log(),
                &mut warnings,
            ),
            EntryVerdict::Reject
        );
        assert_eq!(
            String::from_utf8(warnings).expect("warning UTF-8"),
            format!("  warning: candidate repository owns reserved path: {relative}\n")
        );
    }
}

#[test]
fn tree_scan_accepts_git_shapes_and_rejects_unsafe_or_failed_producers() {
    let fixture = Fixture::new("repo-tree");
    let repo = fixture.repo();
    let launcher = launcher_bytes();
    seed(
        &repo,
        &[
            ("plain", b"plain\n", false),
            ("run", b"#!/bin/sh\n", true),
            (".local/bin/dot", &launcher, true),
        ],
    );
    commit_symlink(&repo, "link", "plain");
    let p = prefix(&repo);
    let mut warnings = Vec::new();
    assert!(validate_candidate_tree(
        &p,
        "base",
        "HEAD",
        &fixture.env,
        &log(),
        &mut warnings
    ));
    assert!(warnings.is_empty());

    std::fs::create_dir_all(repo.join(".dotfiles")).expect("reserved dir");
    std::fs::write(repo.join(".dotfiles/evil"), b"unsafe\n").expect("reserved file");
    commit_all(&repo, "unsafe");
    let mut warnings = Vec::new();
    assert!(!validate_candidate_tree(
        &p,
        "base",
        "HEAD",
        &fixture.env,
        &log(),
        &mut warnings
    ));
    assert_eq!(
        String::from_utf8(warnings).unwrap(),
        "  warning: candidate repository owns reserved path: .dotfiles/evil\n"
    );

    let wrapper = fixture.dir.path().join("partial-git");
    write_program(
        &wrapper,
        &format!(
            "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = ls-tree ]; then\n    printf '100644 blob {}\\tplain\\0'\n    exit 1\n  fi\ndone\nexec {} \"$@\"\n",
            "a".repeat(40),
            shell_word(&git_program()),
        ),
    );
    let accepted = dot::init_client_identity::with_host_git(&wrapper, || {
        validate_candidate_tree(&p, "base", "HEAD", &fixture.env, &log(), &mut Vec::new())
    });
    assert!(!accepted, "valid prefix cannot authorize failed producer");
}

#[test]
fn ahead_delta_accepts_clean_changes_and_rejects_reserved_control_paths() {
    let fixture = Fixture::new("repo-delta");
    let repo = fixture.repo();
    seed(&repo, &[("plain", b"one\n", false)]);
    let base = head(&repo);
    std::fs::write(repo.join("next"), b"two\n").expect("clean delta");
    commit_all(&repo, "clean delta");
    let clean = head(&repo);
    std::fs::create_dir_all(repo.join(".dotfiles")).expect("reserved dir");
    std::fs::write(repo.join(".dotfiles/evil"), b"unsafe\n").expect("reserved file");
    commit_all(&repo, "reserved delta");
    let p = prefix(&repo);
    assert!(validate_ahead_delta(
        &p,
        "base",
        &base,
        &clean,
        &fixture.env,
        &log(),
        &mut Vec::new()
    ));
    let mut warnings = Vec::new();
    assert!(!validate_ahead_delta(
        &p,
        "base",
        &base,
        &head(&repo),
        &fixture.env,
        &log(),
        &mut warnings
    ));
    assert!(String::from_utf8_lossy(&warnings).contains(".dotfiles/evil"));

    let overlay = fixture.dir.path().join("overlay");
    seed(&overlay, &[("home/ordinary", b"one\n", false)]);
    let old = head(&overlay);
    std::fs::create_dir_all(overlay.join("home/.config/dot/profiles.d")).unwrap();
    std::fs::write(
        overlay.join("home/.config/dot/profiles.d/unsafe.conf"),
        b"version=1\n",
    )
    .unwrap();
    commit_all(&overlay, "control delta");
    let mut warnings = Vec::new();
    assert!(!validate_ahead_delta(
        &prefix(&overlay),
        "overlay",
        &old,
        &head(&overlay),
        &fixture.env,
        &log(),
        &mut warnings,
    ));
    assert_eq!(
        String::from_utf8(warnings).unwrap(),
        "  warning: overlay candidate owns reserved control-plane path: .config/dot/profiles.d/unsafe.conf\n"
    );
}

#[test]
fn generation_acceptance_pins_equal_ahead_unrelated_empty_and_head_race() {
    let fixture = Fixture::new("repo-accept");
    let repo = fixture.repo();
    seed(&repo, &[("plain", b"one\n", false)]);
    let base = head(&repo);
    std::fs::write(repo.join("next"), b"two\n").expect("ahead file");
    commit_all(&repo, "ahead");
    let ahead = head(&repo);
    git(&repo, &["checkout", "-q", "--orphan", "unrelated"]);
    git(&repo, &["rm", "-q", "-rf", "."]);
    std::fs::write(repo.join("other"), b"other\n").expect("other file");
    commit_all(&repo, "unrelated");
    let unrelated = head(&repo);
    git(&repo, &["checkout", "-q", &ahead]);
    let p = prefix(&repo);
    for (have, upstream, expected) in [
        (ahead.as_str(), ahead.as_str(), 0),
        (&ahead, &base, 0),
        (&ahead, &unrelated, 1),
        ("", &base, 2),
        (&ahead, "", 2),
    ] {
        assert_eq!(
            accept_current_generation(
                &p,
                "base",
                have,
                upstream,
                &fixture.env,
                &log(),
                &mut Vec::new()
            ),
            expected
        );
    }

    let tree = String::from_utf8(git(&repo, &["rev-parse", "HEAD^{tree}"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let moved_output = Command::new(git_program())
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-C",
        ])
        .arg(&repo)
        .args(["commit-tree", &tree, "-p", &ahead, "-m", "moved"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .expect("commit-tree");
    assert!(moved_output.status.success());
    let moved = String::from_utf8(moved_output.stdout)
        .unwrap()
        .trim()
        .to_string();
    let wrapper = fixture.dir.path().join("racing-git");
    write_program(
        &wrapper,
        &format!(
            "#!/bin/sh\nrace=0\nfor arg do [ \"$arg\" = merge-base ] && race=1; done\n{} \"$@\"\nrc=$?\nif [ \"$rc\" -eq 0 ] && [ \"$race\" -eq 1 ]; then\n  {} -c core.hooksPath=/dev/null -C {} update-ref HEAD {moved} {ahead}\nfi\nexit \"$rc\"\n",
            shell_word(&git_program()),
            shell_word(&git_program()),
            shell_word(&repo),
        ),
    );
    let outcome = dot::init_client_identity::with_host_git(&wrapper, || {
        accept_current_generation(
            &p,
            "base",
            &ahead,
            &base,
            &fixture.env,
            &log(),
            &mut Vec::new(),
        )
    });
    assert_eq!(outcome, 2, "moved HEAD invalidates fast path");
    assert_eq!(head(&repo), moved);
}
