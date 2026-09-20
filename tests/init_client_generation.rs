//! Native contracts for the init git-generation binding: marker
//! publication and validation, branch-tip checks, git identity
//! capture, and git metadata mode setup.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so sibling
//! temp names and git stores never collide.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_generation as generation;
use dot::temp::{self, MoveCache};
use dot_test_support::TempDir;

/// Fixed run identity for the marker-only rows (a 40-hex stand-in;
/// the branch-tip rows use a real fixture commit instead).
const NONCE: &str = "test-nonce-01";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const IDENTITY: &str = "github.com/example/dot";
const BRANCH: &str = "main";

/// Run one shell snippet with the init runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// Twin homes: disjoint directories so sibling temps and git stores
/// never collide across engines.
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&shell_home).expect("shell home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        Self {
            _dir: dir,
            shell_home,
            rust_home,
        }
    }

    /// Fresh real directories standing in for git stores.
    fn git_dirs(&self) -> (PathBuf, PathBuf) {
        let shell_git = self.shell_home.join("gitdir");
        let rust_git = self.rust_home.join("gitdir");
        std::fs::create_dir_all(&shell_git).expect("shell gitdir");
        std::fs::create_dir_all(&rust_git).expect("rust gitdir");
        (shell_git, rust_git)
    }
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Permission bits of one path, `stat -c '%a'` style.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

/// Exact marker bytes for one run identity.
fn good_body(nonce: &str, commit: &str, identity: &str) -> Vec<u8> {
    format!(
        "cgraf78 dot client generation v1\nnonce={nonce}\ncommit={commit}\nidentity={identity}\n"
    )
    .into_bytes()
}

/// Write `body` as the marker under both twin git dirs, then report
/// `(shell exit code, rust match)` for the fixed test identity.
/// Callers assert the parity core — exit 0 exactly when the port
/// matches — plus the absolute direction they pinned.
fn check_marker_body(tag: &str, body: &[u8]) -> (i32, bool) {
    let twins = Twins::build(tag);
    let (_, rust_git) = twins.git_dirs();
    std::fs::write(generation::generation_marker(&rust_git), body).expect("rust marker");
    let matched = generation::generation_marker_matches(&rust_git, NONCE, COMMIT, IDENTITY);
    let code = i32::from(!matched);
    (code, matched)
}

#[test]
fn marker_path_shapes() {
    for plain in ["/fake/git", "/fake/git/"] {
        let want = generation::generation_marker(Path::new(plain));
        assert_eq!(want, Path::new(plain).join("dot-init-generation-v1"));
    }
}

#[test]
fn write_marker_publishes_exact_bytes() {
    let twins = Twins::build("init-gen-write");
    let (_, rust_git) = twins.git_dirs();
    let mut cache = MoveCache::default();
    generation::write_generation_marker(&rust_git, NONCE, COMMIT, IDENTITY, &mut cache)
        .expect("rust write marker");
    let rust_bytes = std::fs::read(generation::generation_marker(&rust_git)).expect("rust bytes");
    assert_eq!(rust_bytes, good_body(NONCE, COMMIT, IDENTITY));
    assert_eq!(mode_of(&generation::generation_marker(&rust_git)), 0o600);
}

#[test]
fn write_marker_keeps_live_marker() {
    let twins = Twins::build("init-gen-write-noreplace");
    let (_, rust_git) = twins.git_dirs();
    std::fs::write(generation::generation_marker(&rust_git), b"live\n").expect("rust sentinel");
    let mut cache = MoveCache::default();
    let rust = generation::write_generation_marker(&rust_git, NONCE, COMMIT, IDENTITY, &mut cache);
    assert!(rust.is_err(), "rust refuses to replace a live marker");
    assert_eq!(
        std::fs::read(generation::generation_marker(&rust_git)).expect("rust sentinel intact"),
        b"live\n"
    );
}

#[test]
fn marker_matches_accepts_good_marker() {
    let (code, matched) =
        check_marker_body("init-gen-match-good", &good_body(NONCE, COMMIT, IDENTITY));
    assert_eq!((code, matched), (0, true));
}

#[test]
fn marker_matches_accepts_missing_trailing_newline() {
    let mut body = good_body(NONCE, COMMIT, IDENTITY);
    body.pop();
    let (code, matched) = check_marker_body("init-gen-match-noeol", &body);
    assert_eq!((code, matched), (0, true));
}

#[test]
fn marker_matches_rejects_bad_header() {
    let body = format!(
        "cgraf78 dot client generation v2\nnonce={NONCE}\ncommit={COMMIT}\nidentity={IDENTITY}\n"
    )
    .into_bytes();
    let (code, matched) = check_marker_body("init-gen-match-header", &body);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_unknown_key() {
    let mut body = good_body(NONCE, COMMIT, IDENTITY);
    body.extend_from_slice(b"extra=yes\n");
    let (code, matched) = check_marker_body("init-gen-match-unknown", &body);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_duplicate_key() {
    let mut body = good_body(NONCE, COMMIT, IDENTITY);
    body.extend_from_slice(format!("nonce={NONCE}\n").as_bytes());
    let (code, matched) = check_marker_body("init-gen-match-dup", &body);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_short_file() {
    let body =
        format!("cgraf78 dot client generation v1\nnonce={NONCE}\ncommit={COMMIT}\n").into_bytes();
    let (code, matched) = check_marker_body("init-gen-match-short", &body);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_long_file() {
    let mut body = good_body(NONCE, COMMIT, IDENTITY);
    body.extend_from_slice(b"nonce=second\n");
    // Over-long shape: the duplicate trips the key gate before the
    // count gate on both engines, and both must agree either way.
    let (code, matched) = check_marker_body("init-gen-match-long", &body);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_line_without_equals() {
    let body = b"cgraf78 dot client generation v1\nnonce\ncommit=x\nidentity=y\n".to_vec();
    let (code, matched) = check_marker_body("init-gen-match-noeq", &body);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_empty_file() {
    let (code, matched) = check_marker_body("init-gen-match-empty", b"");
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_wrong_nonce() {
    let (code, matched) = check_marker_body(
        "init-gen-match-nonce",
        &good_body("other-nonce", COMMIT, IDENTITY),
    );
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_wrong_commit() {
    let (code, matched) = check_marker_body(
        "init-gen-match-commit",
        &good_body(NONCE, "ffffffffffffffffffffffffffffffffffffffff", IDENTITY),
    );
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_wrong_identity() {
    let (code, matched) = check_marker_body(
        "init-gen-match-identity",
        &good_body(NONCE, COMMIT, "example.com/other"),
    );
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn marker_matches_rejects_missing_marker() {
    let twins = Twins::build("init-gen-match-absent");
    let (_, rust_git) = twins.git_dirs();
    assert!(!generation::generation_marker_matches(
        &rust_git, NONCE, COMMIT, IDENTITY
    ));
}

#[test]
fn marker_matches_rejects_marker_symlink() {
    let twins = Twins::build("init-gen-match-link");
    let (shell_git, rust_git) = twins.git_dirs();
    for (git_dir, home) in [
        (&shell_git, &twins.shell_home),
        (&rust_git, &twins.rust_home),
    ] {
        let target = home.join("real-marker");
        std::fs::write(&target, good_body(NONCE, COMMIT, IDENTITY)).expect("link target");
        std::os::unix::fs::symlink(&target, generation::generation_marker(git_dir))
            .expect("symlink");
    }
    assert!(!generation::generation_marker_matches(
        &rust_git, NONCE, COMMIT, IDENTITY
    ));
}

#[test]
fn marker_matches_rejects_symlinked_git_dir() {
    let twins = Twins::build("init-gen-match-dirlink");
    let (shell_git, rust_git) = twins.git_dirs();
    for (git_dir, home) in [
        (&shell_git, &twins.shell_home),
        (&rust_git, &twins.rust_home),
    ] {
        let target = home.join("real-gitdir");
        std::fs::create_dir_all(&target).expect("link target dir");
        std::fs::write(
            generation::generation_marker(&target),
            good_body(NONCE, COMMIT, IDENTITY),
        )
        .expect("target marker");
        let away = home.join("gitdir-away");
        std::fs::rename(git_dir, &away).expect("move gitdir aside");
        std::os::unix::fs::symlink(&target, git_dir).expect("symlink gitdir");
    }
    assert!(!generation::generation_marker_matches(
        &rust_git, NONCE, COMMIT, IDENTITY
    ));
}

#[test]
fn marker_matches_rejects_missing_git_dir() {
    let twins = Twins::build("init-gen-match-nodir");
    let rust_git = twins.rust_home.join("no-such-dir");
    assert!(!generation::generation_marker_matches(
        &rust_git, NONCE, COMMIT, IDENTITY
    ));
}

#[test]
fn marker_matches_rejects_file_git_dir() {
    let twins = Twins::build("init-gen-match-filedir");
    let shell_git = twins.shell_home.join("file-gitdir");
    let rust_git = twins.rust_home.join("file-gitdir");
    std::fs::write(&shell_git, b"not a dir\n").expect("shell file");
    std::fs::write(&rust_git, b"not a dir\n").expect("rust file");
    assert!(!generation::generation_marker_matches(
        &rust_git, NONCE, COMMIT, IDENTITY
    ));
}

/// Build a deterministic repo at `path` on `main` and return its
/// HEAD commit. Fixed identity, dates, and content keep the hash
/// identical across twin sides, so both engines bind the same
/// branch tip.
fn fixture_repo(path: &Path) -> String {
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("-b")
        .arg(BRANCH)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git init");
    assert!(status.success(), "git init {}", path.display());
    let file = path.join("file.txt");
    std::fs::write(&file, b"init generation fixture\n").expect("fixture file");
    let fixed: &[(&str, &str)] = &[
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@t"),
        ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@t"),
        ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ];
    let mut add = Command::new("git");
    add.arg("-C")
        .arg(path)
        .args(["add", "-A"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in fixed {
        add.env(key, value);
    }
    assert!(add.status().expect("git add").success(), "git add");
    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            "fixture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in fixed {
        commit.env(key, value);
    }
    assert!(commit.status().expect("git commit").success(), "git commit");
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("git rev-parse");
    assert!(output.status.success(), "git rev-parse HEAD");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Twin deterministic repos plus their `.git` stores. Both sides
/// share one HEAD hash by construction.
struct TwinRepos {
    _twins: Twins,
    shell_git: PathBuf,
    rust_git: PathBuf,
    commit: String,
}

impl TwinRepos {
    fn build(tag: &str) -> Self {
        let twins = Twins::build(tag);
        let shell_repo = twins.shell_home.join("repo");
        let rust_repo = twins.rust_home.join("repo");
        let shell_commit = fixture_repo(&shell_repo);
        let rust_commit = fixture_repo(&rust_repo);
        assert_eq!(shell_commit, rust_commit, "twin fixtures share HEAD");
        Self {
            _twins: twins,
            shell_git: shell_repo.join(".git"),
            rust_git: rust_repo.join(".git"),
            commit: shell_commit,
        }
    }

    /// Publish a marker for `marker_commit` on each side with each
    /// engine's own writer, probe both for `wanted`, and report
    /// `(shell exit code, rust match)`. The shell writer binds the
    /// snippet env while the probe rebinds `DOT_INIT_COMMIT` first,
    /// so mismatched rows exercise both halves on each engine.
    fn check_generation(&self, marker_commit: &str, wanted: &str) -> (i32, bool) {
        let mut cache = MoveCache::default();
        generation::write_generation_marker(
            &self.rust_git,
            NONCE,
            marker_commit,
            IDENTITY,
            &mut cache,
        )
        .expect("rust write marker");
        let matched =
            generation::generation_matches(&self.rust_git, BRANCH, NONCE, wanted, IDENTITY);
        (i32::from(!matched), matched)
    }
}

#[test]
fn generation_matches_when_bound() {
    let repos = TwinRepos::build("init-gen-bound");
    let commit = repos.commit.clone();
    let (code, matched) = repos.check_generation(&commit, &commit);
    assert_eq!((code, matched), (0, true));
}

#[test]
fn generation_matches_rejects_wrong_commit() {
    let repos = TwinRepos::build("init-gen-wrong-commit");
    let commit = repos.commit.clone();
    let other = "ffffffffffffffffffffffffffffffffffffffff".to_string();
    let (code, matched) = repos.check_generation(&commit, &other);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn generation_matches_rejects_wrong_marker() {
    let repos = TwinRepos::build("init-gen-wrong-marker");
    let commit = repos.commit.clone();
    let other = "ffffffffffffffffffffffffffffffffffffffff".to_string();
    let (code, matched) = repos.check_generation(&other, &commit);
    assert_ne!(code, 0);
    assert!(!matched);
}

#[test]
fn generation_matches_rejects_missing_ref() {
    let repos = TwinRepos::build("init-gen-missing-ref");
    let commit = repos.commit.clone();
    // Bind the real tip, then ask for a branch that does not exist.
    let mut cache = MoveCache::default();
    generation::write_generation_marker(&repos.rust_git, NONCE, &commit, IDENTITY, &mut cache)
        .expect("rust write marker");
    let matched =
        generation::generation_matches(&repos.rust_git, "no-such-branch", NONCE, &commit, IDENTITY);
    assert!(!matched);
}

#[test]
fn generation_matches_rejects_missing_store() {
    let twins = Twins::build("init-gen-missing-store");
    let rust_git = twins.rust_home.join("no-such-gitdir");
    assert!(!generation::generation_matches(
        &rust_git, BRANCH, NONCE, COMMIT, IDENTITY
    ));
}

#[test]
fn set_git_identity_reports_dev_ino() {
    let twins = Twins::build("init-gen-identity");
    let (shell_git, rust_git) = twins.git_dirs();
    // Same physical directory on both engines: `dev:ino` must agree
    // exactly, formatted like `stat -c '%d:%i'`.
    for dir in [&shell_git, &rust_git] {
        let want = temp::identity_string(generation::set_git_identity(dir).expect("rust identity"));
        assert_eq!(want.split(':').count(), 2, "identity of {}", dir.display());
    }
}

#[test]
fn set_git_identity_rejects_missing_dir() {
    let twins = Twins::build("init-gen-identity-absent");
    let missing = twins.shell_home.join("no-such-dir");
    assert!(generation::set_git_identity(&missing).is_err());
}

/// Probe one configured store: the stored `core.sharedRepository`
/// value plus the clamped modes of two metadata files. Paths never
/// leave the harness, so twin sides compare directly.
fn configured_probe(git_dir: &Path) -> (String, u32, u32) {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(["config", "--local", "--get", "core.sharedRepository"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("git config probe");
    assert!(output.status.success(), "sharedRepository is set");
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let config_mode = mode_of(&git_dir.join("config"));
    let head_mode = mode_of(&git_dir.join("HEAD"));
    (value, config_mode, head_mode)
}

#[test]
fn configure_modes_pins_shared_repository_and_clamps() {
    let repos = TwinRepos::build("init-gen-modes");
    // Loosen two metadata files first: the walk must clamp them
    // back identically on both engines under the runner's umask.
    for git_dir in [&repos.shell_git, &repos.rust_git] {
        chmod(&git_dir.join("config"), 0o666);
        chmod(&git_dir.join("HEAD"), 0o666);
    }
    generation::configure_git_metadata_modes(&repos.shell_git).expect("fixture configure modes");
    generation::configure_git_metadata_modes(&repos.rust_git).expect("rust configure modes");
    assert_eq!(
        configured_probe(&repos.shell_git),
        configured_probe(&repos.rust_git),
        "sharedRepository value and clamped modes agree"
    );
    // Absolute pin: the stored policy reads back as the octal the
    // shell wrote, on both sides.
    assert_eq!(configured_probe(&repos.shell_git).0, "0700");
}

#[test]
fn configure_modes_rejects_missing_store() {
    let twins = Twins::build("init-gen-modes-nodir");
    let rust_git = twins.rust_home.join("no-such-gitdir");
    assert!(generation::configure_git_metadata_modes(&rust_git).is_err());
}

#[test]
fn configure_modes_rejects_file_store() {
    let twins = Twins::build("init-gen-modes-file");
    let shell_git = twins.shell_home.join("file-gitdir");
    let rust_git = twins.rust_home.join("file-gitdir");
    std::fs::write(&shell_git, b"not a store\n").expect("shell file");
    std::fs::write(&rust_git, b"not a store\n").expect("rust file");
    assert!(generation::configure_git_metadata_modes(&rust_git).is_err());
}
