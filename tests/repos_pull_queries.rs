//! Differential parity tests for `src/repos_pull_queries.rs` against
//! the live shell (`lib/dot/repos/pull.sh`): the checked-out
//! generation query, upstream containment, generation identity, and
//! the candidate-tree validation cluster (adapter gate, entry
//! policy, full-tree and ahead-delta scans, generation acceptance).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_pull_queries::{
    CandidateEnv, EntryVerdict, accept_current_generation, candidate_adapter_allowed, repo_head,
    repo_head_contains_upstream, repo_head_is, validate_ahead_delta, validate_candidate_entry,
    validate_candidate_tree,
};

/// Non-tty logger: warning bytes match the shell exactly.
fn test_log() -> Log {
    Log::new(false, false)
}
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

// ---------------------------------------------------------------------------
// Candidate validation cluster.
// ---------------------------------------------------------------------------

/// Sources for the validation cluster: the base set plus XDG lookup,
/// logging, the safe-relative gate, and the reserved inventory.
const VALIDATE_SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the validation libraries sourced and
/// the reserved-inventory environment pinned to `home`.
fn shell_validate(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let home_text = home.to_string_lossy().into_owned();
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{VALIDATE_SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .env("XDG_STATE_HOME", format!("{home_text}/.local/state"))
        .env("SHDEPS_INSTALL_DIR", format!("{home_text}/.local/share"))
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

/// Validation fixture: an isolated `$HOME` plus the [`CandidateEnv`]
/// mirroring the shell environment above.
struct ValidateFixture {
    _dir: TempDir,
    home: PathBuf,
    env: CandidateEnv,
}

impl ValidateFixture {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("fixture home");
        let home_text = home.to_string_lossy().into_owned();
        let env = CandidateEnv {
            home: home_text.clone(),
            checkout: format!("{home_text}/.local/share/cgraf78/dot"),
            pwd: home_text.clone(),
            source_root: env!("CARGO_MANIFEST_DIR").to_string(),
            state_home: format!("{home_text}/.local/state"),
            install_root: format!("{home_text}/.local/share"),
            provider_state: format!("{home_text}/.local/state/shdeps"),
            overlay_paths: Vec::new(),
            init_backup: None,
        };
        ValidateFixture {
            _dir: dir,
            home,
            env,
        }
    }

    fn prefix(&self, repo: &Path) -> Vec<std::ffi::OsString> {
        ["-C".to_string(), repo.to_string_lossy().into_owned()]
            .iter()
            .map(std::ffi::OsString::from)
            .collect()
    }
}

/// Commit `files` (path, bytes, executable) into a fresh repo.
fn seed_tree(repo: &Path, files: &[(&str, &[u8], bool)]) {
    std::fs::create_dir_all(repo).expect("repo dir");
    git(repo, &["init", "-q"]);
    for (name, bytes, executable) in files {
        let path = repo.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture parents");
        }
        std::fs::write(&path, bytes).expect("seed file");
        if *executable {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
    }
    git(
        repo,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        repo,
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

/// Tracked symlink commit (mode 120000).
fn commit_symlink(repo: &Path, name: &str, target: &str) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, repo.join(name)).expect("symlink");
    git(
        repo,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "link",
        ],
    );
}

fn launcher_bytes() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("support/client-launcher.sh"))
        .expect("launcher fixture")
}

#[test]
fn candidate_adapter_allowed_matches_shell() {
    let fixture = ValidateFixture::build("pull-adapter");
    let repo = fixture.home.join("repo");
    let launcher = launcher_bytes();
    seed_tree(&repo, &[(".local/bin/dot", &launcher, true)]);
    let repo_text = repo.to_string_lossy().into_owned();
    let head = head_of(&repo);
    let prefix = fixture.prefix(&repo);
    // (path, mode, tamper, shell-expect): only the exact launcher
    // payload at 100755 passes.
    for (path, mode, tamper, want) in [
        (".local/bin/dot", "100755", false, true),
        (".local/bin/dot", "100644", false, false),
        (".local/bin/other", "100755", false, false),
        (".local/bin/dot", "100755", true, false),
    ] {
        if tamper {
            let tampered = [launcher.clone(), b"# tamper\n".to_vec()].concat();
            std::fs::write(repo.join(".local/bin/dot"), tampered).expect("tamper");
            git(
                &repo,
                &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
            );
            git(
                &repo,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "tamper",
                ],
            );
        }
        let current = head_of(&repo);
        assert_eq!(tamper, current != head, "tamper fixture state");
        let snippet = format!(
            "if _repo_candidate_adapter_allowed \"{current}\" \"{path}\" \"{mode}\" git -C {repo_text}; then echo yes; else echo no; fi\n"
        );
        let (status, out, err) = shell_validate(&fixture.home, &snippet);
        assert_eq!(status, 0, "harness exit");
        assert!(err.is_empty(), "shell stderr: {err:?}");
        let shell_yes = out.starts_with(b"yes\n");
        assert_eq!(shell_yes, want, "shell adapter for {path}@{mode}");
        let mut warnings = Vec::new();
        assert_eq!(
            candidate_adapter_allowed(&prefix, &current, path, mode, &fixture.env, &mut warnings),
            want,
            "rust adapter for {path}@{mode}"
        );
        assert!(warnings.is_empty(), "adapter warnings: {warnings:?}");
    }
}

#[test]
fn validate_candidate_entry_matches_shell() {
    let fixture = ValidateFixture::build("pull-entry");
    let repo = fixture.home.join("repo");
    seed_tree(
        &repo,
        &[
            ("clean.txt", b"v1\n", false),
            (".local/bin/dot", &launcher_bytes(), true),
        ],
    );
    let head = head_of(&repo);
    let prefix = fixture.prefix(&repo);
    let shell_roots = String::from_utf8(
        shell_validate(
            &fixture.home,
            "_dot_reserved_roots_snapshot || exit 99\nprintf '%s' \"$REPLY\"\n",
        )
        .1,
    )
    .expect("roots utf8");
    assert!(!shell_roots.is_empty(), "shell roots snapshot");
    // (kind, mode, type, oid-shape, path, shell-expect): verdict and
    // warning bytes must match exactly.
    let clean_oid = "a".repeat(40);
    let rows: Vec<(&str, &str, &str, String, &str, bool)> = vec![
        (
            "base",
            "100644",
            "blob",
            clean_oid.clone(),
            "clean.txt",
            true,
        ),
        (
            "base",
            "100755",
            "blob",
            clean_oid.clone(),
            "clean.txt",
            true,
        ),
        (
            "base",
            "120000",
            "blob",
            clean_oid.clone(),
            "clean.txt",
            true,
        ),
        (
            "base",
            "100644",
            "blob",
            clean_oid.clone(),
            ".dotfiles/evil",
            false,
        ),
        (
            "base",
            "100644",
            "commit",
            clean_oid.clone(),
            "clean.txt",
            false,
        ),
        (
            "base",
            "100600",
            "blob",
            clean_oid.clone(),
            "clean.txt",
            false,
        ),
        (
            "base",
            "100644",
            "blob",
            "xyz".to_string(),
            "clean.txt",
            false,
        ),
        (
            "base",
            "100644",
            "blob",
            clean_oid.clone(),
            "../escape",
            false,
        ),
        (
            "base",
            "100644",
            "blob",
            clean_oid.clone(),
            ".git/evil",
            false,
        ),
        (
            "overlay",
            "100644",
            "blob",
            clean_oid.clone(),
            "home/clean.txt",
            true,
        ),
        (
            "overlay",
            "100644",
            "blob",
            clean_oid.clone(),
            "outside.txt",
            true,
        ),
        // Overlay metadata outside home/ skips with an empty reply.
        (
            "overlay",
            "100644",
            "blob",
            clean_oid.clone(),
            ".config/dot/profiles.d/x",
            true,
        ),
        // The control-plane gate applies beneath home/.
        (
            "overlay",
            "100644",
            "blob",
            clean_oid.clone(),
            "home/.config/dot/profiles.d/x",
            false,
        ),
        (
            "overlay",
            "100644",
            "blob",
            clean_oid.clone(),
            "home/.dotfiles/evil",
            false,
        ),
        (
            "overlay",
            "100644",
            "blob",
            clean_oid.clone(),
            "home",
            false,
        ),
    ];
    for (kind, mode, entry_type, oid, path, want_ok) in &rows {
        let snippet = format!(
            "if _repo_validate_candidate_entry \"{kind}\" \"{head}\" \"{mode}\" \"{entry_type}\" \"{oid}\" \"{path}\" \"$2\" git -C \"$3\"; then echo \"rc=0 reply=$REPLY\"; else echo \"rc=1 reply=$REPLY\"; fi\n"
        );
        let roots_os = std::ffi::OsString::from(&shell_roots);
        let repo_os = std::ffi::OsString::from(repo.to_string_lossy().as_ref());
        let mut cmd = Command::new(dot::test_support::bash());
        cmd.arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(format!("{VALIDATE_SOURCES}{snippet}"));
        cmd.arg("dot-test-sh")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .arg(roots_os)
            .arg(repo_os);
        let home_text = fixture.home.to_string_lossy().into_owned();
        cmd.env_clear()
            .env("LC_ALL", "C")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", std::env::var_os("TMPDIR").unwrap_or_default())
            .env("HOME", &fixture.home)
            .env("DOT_TEST", "1")
            .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
            .env("XDG_STATE_HOME", format!("{home_text}/.local/state"))
            .env("SHDEPS_INSTALL_DIR", format!("{home_text}/.local/share"))
            .current_dir(&fixture.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().expect("spawn bash");
        assert_eq!(output.status.code(), Some(0), "harness exit for {path}");
        let rust_roots: Vec<String> = shell_roots.lines().map(str::to_string).collect();
        let mut warnings = Vec::new();
        let rust = validate_candidate_entry(
            &prefix,
            kind,
            &head,
            mode,
            entry_type,
            oid,
            path,
            &rust_roots,
            &fixture.env,
            &test_log(),
            &mut warnings,
        );
        let rust_line = match &rust {
            EntryVerdict::Accept(relative) => format!("rc=0 reply={relative}\n"),
            EntryVerdict::Skip => "rc=0 reply=\n".to_string(),
            EntryVerdict::Reject => "rc=1 reply=\n".to_string(),
        };
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            rust_line,
            "verdict parity for {kind}:{path}"
        );
        assert_eq!(output.stderr, warnings, "warning parity for {kind}:{path}");
        assert_eq!(
            rust != EntryVerdict::Reject,
            *want_ok,
            "rust verdict for {kind}:{path}"
        );
    }
}

#[test]
fn validate_candidate_tree_matches_shell() {
    // Clean tree: regular file, executable, symlink, and the exact
    // launcher payload all validate.
    let fixture = ValidateFixture::build("pull-tree-clean");
    let repo = fixture.home.join("repo");
    seed_tree(
        &repo,
        &[
            ("clean.txt", b"v1\n", false),
            ("run.sh", b"#!/bin/sh\n", true),
            (".local/bin/dot", &launcher_bytes(), true),
        ],
    );
    commit_symlink(&repo, "link", "clean.txt");
    let repo_text = repo.to_string_lossy().into_owned();
    let snippet = format!(
        "if _repo_validate_candidate_tree base HEAD git -C {repo_text}; then echo ok; else echo reject; fi\n"
    );
    let (status, out, err) = shell_validate(&fixture.home, &snippet);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"ok\n", "shell accepts clean tree: {out:?}");
    assert!(err.is_empty(), "shell warnings: {err:?}");
    let prefix = fixture.prefix(&repo);
    let mut warnings = Vec::new();
    assert!(
        validate_candidate_tree(
            &prefix,
            "base",
            "HEAD",
            &fixture.env,
            &test_log(),
            &mut warnings
        ),
        "rust accepts clean tree"
    );
    assert!(warnings.is_empty(), "rust warnings: {warnings:?}");

    // Reserved destination inside the tree rejects with a warning.
    let dirty = ValidateFixture::build("pull-tree-dirty");
    let dirty_repo = dirty.home.join("repo");
    seed_tree(
        &dirty_repo,
        &[
            ("clean.txt", b"v1\n", false),
            (".dotfiles/evil", b"x\n", false),
        ],
    );
    let dirty_text = dirty_repo.to_string_lossy().into_owned();
    let snippet = format!(
        "if _repo_validate_candidate_tree base HEAD git -C {dirty_text}; then echo ok; else echo reject; fi\n"
    );
    let (status, out, shell_warnings) = shell_validate(&dirty.home, &snippet);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"reject\n", "shell rejects reserved tree");
    assert!(!shell_warnings.is_empty(), "shell warns");
    let dirty_prefix = dirty.prefix(&dirty_repo);
    let mut warnings = Vec::new();
    assert!(
        !validate_candidate_tree(
            &dirty_prefix,
            "base",
            "HEAD",
            &dirty.env,
            &test_log(),
            &mut warnings
        ),
        "rust rejects reserved tree"
    );
    assert_eq!(warnings, shell_warnings, "warning bytes parity");

    // Unknown ref rejects without warnings.
    let snippet = format!(
        "if _repo_validate_candidate_tree base no-such-ref git -C {repo_text}; then echo ok; else echo reject; fi\n"
    );
    let (status, out, err) = shell_validate(&fixture.home, &snippet);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"reject\n", "shell rejects unknown ref");
    let mut warnings = Vec::new();
    assert!(
        !validate_candidate_tree(
            &prefix,
            "base",
            "no-such-ref",
            &fixture.env,
            &test_log(),
            &mut warnings
        ),
        "rust rejects unknown ref"
    );
    assert_eq!(warnings, err, "no warnings either side");
}

#[test]
fn validate_ahead_delta_matches_shell() {
    let fixture = ValidateFixture::build("pull-delta");
    let repo = fixture.home.join("repo");
    seed_tree(&repo, &[("clean.txt", b"v1\n", false)]);
    let base = head_of(&repo);
    // Clean ahead commit validates; reserved-path commit does not.
    std::fs::write(repo.join("next.txt"), b"v2\n").expect("advance");
    git(
        &repo,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "ahead",
        ],
    );
    let ahead = head_of(&repo);
    std::fs::create_dir_all(repo.join(".dotfiles")).expect("reserved dir");
    std::fs::write(repo.join(".dotfiles/evil"), b"x\n").expect("reserved file");
    git(
        &repo,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "dirty",
        ],
    );
    let dirty = head_of(&repo);
    let repo_text = repo.to_string_lossy().into_owned();
    let prefix = fixture.prefix(&repo);
    for (upstream, head, want) in [
        (base.clone(), ahead.clone(), true),
        (base.clone(), dirty.clone(), false),
    ] {
        let snippet = format!(
            "if _repo_validate_ahead_delta base \"{upstream}\" \"{head}\" git -C {repo_text}; then echo ok; else echo reject; fi\n"
        );
        let (status, out, shell_warnings) = shell_validate(&fixture.home, &snippet);
        assert_eq!(status, 0, "harness exit");
        let shell_ok = out == b"ok\n";
        assert_eq!(shell_ok, want, "shell delta {upstream}..{head}");
        let mut warnings = Vec::new();
        let rust_ok = validate_ahead_delta(
            &prefix,
            "base",
            &upstream,
            &head,
            &fixture.env,
            &test_log(),
            &mut warnings,
        );
        assert_eq!(rust_ok, want, "rust delta {upstream}..{head}");
        assert_eq!(warnings, shell_warnings, "warning parity");
    }
}

#[test]
fn accept_current_generation_matches_shell() {
    let fixture = ValidateFixture::build("pull-accept");
    let repo = fixture.home.join("repo");
    seed_tree(&repo, &[("clean.txt", b"v1\n", false)]);
    let base = head_of(&repo);
    std::fs::write(repo.join("next.txt"), b"v2\n").expect("advance");
    git(
        &repo,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "ahead",
        ],
    );
    let ahead = head_of(&repo);
    git(&repo, &["checkout", "-q", "--orphan", "stranger"]);
    git(&repo, &["rm", "-q", "-rf", "."]);
    std::fs::write(repo.join("other.txt"), b"z\n").expect("stranger file");
    git(
        &repo,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "stranger",
        ],
    );
    let stranger = head_of(&repo);
    // Settle back on the ahead generation: the equal/contained rows
    // bind their final HEAD read to the live checkout.
    git(&repo, &["checkout", "-q", &ahead]);
    assert_eq!(head_of(&repo), ahead, "checkout settles on ahead");
    let repo_text = repo.to_string_lossy().into_owned();
    let prefix = fixture.prefix(&repo);
    // (head, upstream, shell-expect): equal is current, contained
    // clean delta is current, unrelated needs pull, empties fail.
    for (head, upstream, want) in [
        (ahead.clone(), ahead.clone(), 0),
        (ahead.clone(), base.clone(), 0),
        (ahead.clone(), stranger.clone(), 1),
        (String::new(), base.clone(), 2),
        (ahead.clone(), String::new(), 2),
    ] {
        let snippet = format!(
            "_repo_accept_current_generation base \"{head}\" \"{upstream}\" git -C {repo_text}; echo \"rc=$?\"\n"
        );
        let (status, out, shell_warnings) = shell_validate(&fixture.home, &snippet);
        assert_eq!(status, 0, "harness exit");
        assert_eq!(out, format!("rc={want}\n").into_bytes(), "shell rc");
        let mut warnings = Vec::new();
        let rust_rc = accept_current_generation(
            &prefix,
            "base",
            &head,
            &upstream,
            &fixture.env,
            &test_log(),
            &mut warnings,
        );
        assert_eq!(rust_rc, want, "rust rc");
        assert_eq!(warnings, shell_warnings, "warning parity");
    }
}
