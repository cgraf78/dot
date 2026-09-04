//! Differential parity tests for the init safe-deletion family
//! (`lib/dot/init-client.sh`) against the live shell: the delete-park
//! path, the candidate/git matcher, the leaf and parent delete
//! matchers, the private-directory matchers, and the
//! parked-generation remover.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so staged
//! parks and git stores never collide.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_delete as delete;
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the init deletion chapter: the resource runtime, the
/// shared temp helpers (moves, stat probes), the XDG bindings, and
/// the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixed run nonce for the park-path rows, crossing as
/// `DOT_INIT_NONCE` on the shell side and as an explicit argument on
/// the Rust side.
const NONCE: &str = "test-nonce-58";

/// Run one shell snippet with the init runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that
/// verdict — not the process status, which only says the printer
/// ran. A snippet that never reports (a harness bug, never a pass)
/// yields 99.
///
/// The locale stays pinned: git diagnostics must read English on
/// both engines, and the port pins `LC_ALL=C` around every git run.
/// Run-identity globals cross as explicit environment entries,
/// mirroring how the engine exports them before calling into this
/// family.
fn shell_run(home: &Path, env: &[(&str, &str)], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
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
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    let verdict = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("code=")
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(99);
    (verdict, output.stdout, output.stderr)
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so staged parks never collide
/// across engines.
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

    fn root(&self) -> &Path {
        self._dir.path()
    }
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// `dev:ino` of one path, following symlinks like both engines.
fn identity_of(path: &Path) -> String {
    dot::temp::identity_string(dot::temp::path_identity(path).expect("stat fixture"))
}

/// Present in any form, like the shell's `-e`/`-L` test.
fn present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Run git for fixtures, with a pinned identity for commits.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// Capture one git query for fixtures.
fn git_out(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
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
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

/// Committed fixture repo plus the tree facts the matchers check.
struct Repo {
    git_dir: PathBuf,
    commit: String,
    regular_oid: String,
    exec_oid: String,
    link_oid: String,
}

/// Build a repo with a regular file, an executable, and a symlink.
fn build_repo(root: &Path) -> Repo {
    const REGULAR: &[u8] = b"hello init\n";
    const EXEC: &[u8] = b"#!/bin/sh\necho hi\n";
    const LINK: &str = "link-target";
    let work = root.join("origin");
    std::fs::create_dir_all(&work).expect("repo dir");
    std::fs::write(work.join("file.txt"), REGULAR).expect("write fixture");
    chmod(&work.join("file.txt"), 0o644);
    std::fs::write(work.join("run.sh"), EXEC).expect("write fixture");
    chmod(&work.join("run.sh"), 0o755);
    std::os::unix::fs::symlink(LINK, work.join("link")).expect("symlink fixture");
    git(&work, &["init", "-qb", "main"]);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "init"]);
    let mut regular_oid = String::new();
    let mut exec_oid = String::new();
    let mut link_oid = String::new();
    for line in git_out(&work, &["ls-files", "-s"]).lines() {
        let mut words = line.split_whitespace();
        let (mode, oid) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        let name = line.split('\t').nth(1).unwrap_or("");
        match name {
            "file.txt" => {
                assert_eq!(mode, "100644");
                regular_oid = oid.to_string();
            }
            "run.sh" => {
                assert_eq!(mode, "100755");
                exec_oid = oid.to_string();
            }
            "link" => {
                assert_eq!(mode, "120000");
                link_oid = oid.to_string();
            }
            _ => {}
        }
    }
    assert!(!regular_oid.is_empty() && !exec_oid.is_empty() && !link_oid.is_empty());
    Repo {
        git_dir: work.join(".git"),
        commit: git_out(&work, &["rev-parse", "HEAD"]),
        regular_oid,
        exec_oid,
        link_oid,
    }
}

/// Mirror the repo leaves under one home directory.
fn build_home(home: &Path) {
    std::fs::write(home.join("file.txt"), b"hello init\n").expect("write fixture");
    chmod(&home.join("file.txt"), 0o644);
    std::fs::write(home.join("run.sh"), b"#!/bin/sh\necho hi\n").expect("write fixture");
    chmod(&home.join("run.sh"), 0o755);
    std::os::unix::fs::symlink("link-target", home.join("link")).expect("symlink fixture");
}

/// The `reply=` line of a snippet's stdout, if the probe printed one.
fn reply_of(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|line| line.strip_prefix("reply=").map(str::to_string))
}

/// Remainder of `path` after the home prefix: park comparisons
/// normalize the twin roots away while keeping every byte after,
/// including the hash segment.
fn aftermath(path: &str, home: &Path) -> String {
    let bytes = path.as_bytes();
    let mut prefix = home.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    assert!(bytes.starts_with(&prefix), "park escaped its home");
    String::from_utf8_lossy(&bytes[prefix.len() - 1..]).to_string()
}

/// Run the park-path probe on one side and return its verdict plus
/// the home-relative `REPLY`.
fn shell_park(home: &Path, target: &Path, kind: &str, key: &str) -> (bool, Option<String>) {
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" {} {} 2>/dev/null; code=$?; printf 'code=%s\\n' \"$code\"; printf 'reply=%s\\n' \"$REPLY\"\n",
        sq(&target.to_string_lossy()),
        sq(kind),
        sq(key),
    );
    let (verdict, stdout, _stderr) = shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    let reply = reply_of(&stdout).and_then(|text| {
        if text.is_empty() {
            None
        } else {
            Some(aftermath(&text, home))
        }
    });
    (verdict == 0, reply)
}

/// Manifest checkout binding for the park hash (the shell's bare
/// `git` runs from the test's own checkout-neutral temp home).
fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Compare one park-path row: both engines agree on the verdict,
/// and on success on the home-relative `REPLY` (which carries the
/// `kind\tkey` hash).
fn check_park(target_rel: &str, kind: &str, key: &str) {
    let twins = Twins::build("delete-park");
    let root = source_root();
    let shell_target = twins.shell_home.join(target_rel);
    let rust_target = twins.rust_home.join(target_rel);
    let (shell_ok, shell_reply) = shell_park(&twins.shell_home, &shell_target, kind, key);
    let rust = delete::delete_park_path(&rust_target, kind, key, NONCE, &root);
    assert_eq!(rust.is_ok(), shell_ok, "verdict for {target_rel:?}/{kind}");
    match rust {
        Ok(park) => {
            let rendered = park.to_string_lossy().into_owned();
            assert_eq!(Some(aftermath(&rendered, &twins.rust_home)), shell_reply);
        }
        Err(_) => assert_eq!(None, shell_reply),
    }
}

#[test]
fn park_path_leaf_matches_shell() {
    check_park("sub/file.txt", "leaf", "sub/file.txt");
}

#[test]
fn park_path_parent_and_git_kinds() {
    check_park("sub/dir", "parent", "sub/dir");
    let twins = Twins::build("delete-park-git");
    let root = source_root();
    let shell_target = twins.shell_home.join(".dotfiles");
    let rust_target = twins.rust_home.join(".dotfiles");
    let (shell_ok, shell_reply) = shell_park(&twins.shell_home, &shell_target, "git", ".dotfiles");
    let rust = delete::delete_park_path(&rust_target, "git", ".dotfiles", NONCE, &root);
    assert!(shell_ok && rust.is_ok());
    let rendered = rust.expect("rust park").to_string_lossy().into_owned();
    assert_eq!(Some(aftermath(&rendered, &twins.rust_home)), shell_reply);
}

#[test]
fn park_path_rejects_bad_kind() {
    check_park("sub/file.txt", "tree", "sub/file.txt");
    check_park("sub/file.txt", "LEAF", "sub/file.txt");
    check_park("sub/file.txt", "", "sub/file.txt");
}

#[test]
fn park_path_rejects_parentless_target() {
    // `${target%/*}` leaves a bare name unchanged, and the shell
    // rejects the self-parent.
    let twins = Twins::build("delete-park-noparent");
    let root = source_root();
    let (shell_ok, shell_reply) = shell_park(&twins.shell_home, Path::new("lonely"), "leaf", "k");
    assert!(!shell_ok && shell_reply.is_none());
    assert!(delete::delete_park_path(Path::new("lonely"), "leaf", "k", NONCE, &root).is_err());
}

#[test]
fn park_path_rejects_root_target() {
    // `/` strips to an empty parent, which the shell refuses.
    let twins = Twins::build("delete-park-root");
    let root = source_root();
    let (shell_ok, _) = shell_park(&twins.shell_home, Path::new("/"), "leaf", "k");
    assert!(!shell_ok);
    assert!(delete::delete_park_path(Path::new("/"), "leaf", "k", NONCE, &root).is_err());
}

#[test]
fn park_path_trailing_slash_target() {
    // `${target%/*}` drops one trailing slash level: `sub/` parks
    // beside `sub`.
    check_park("sub/", "parent", "sub");
}

#[test]
fn park_path_doubled_separator() {
    // No normalization anywhere: the doubled separator survives
    // into `REPLY` on both engines.
    check_park("a//b", "leaf", "a//b");
}

#[test]
fn park_path_special_key_bytes() {
    check_park("sub/file.txt", "leaf", "key with spaces");
    check_park("sub/file.txt", "leaf", "tab\tseparated");
}

/// Compare one candidate/git row: the shell verdict against the
/// Rust verdict for the same mode/oid/path triple.
fn check_git(
    twins: &Twins,
    repo: &Repo,
    name: &str,
    mode: &str,
    oid: &str,
    home_mutate: Option<fn(&Path)>,
) -> (bool, bool) {
    if let Some(mutate) = home_mutate {
        mutate(&twins.shell_home);
        mutate(&twins.rust_home);
    }
    let snippet = format!(
        "_dot_init_candidate_matches_git {} {} {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&repo.git_dir.to_string_lossy()),
        sq(&repo.commit),
        sq(mode),
        sq(oid),
        sq(name),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    let rust = delete::candidate_matches_git(
        &repo.git_dir,
        &repo.commit,
        mode,
        oid,
        &twins.rust_home,
        Path::new(name),
    );
    (verdict == 0, rust)
}

/// Both engines must agree, expecting `want`.
fn assert_git(
    tag: &str,
    name: &str,
    mode: &str,
    oid: &RepoOid,
    want: bool,
    mutate: Option<fn(&Path)>,
) {
    let twins = Twins::build(tag);
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    let oid = oid.get(&repo);
    let (shell_ok, rust_ok) = check_git(&twins, &repo, name, mode, &oid, mutate);
    assert_eq!(shell_ok, want, "shell verdict for {name}/{mode}");
    assert_eq!(rust_ok, want, "rust verdict for {name}/{mode}");
}

/// Selector for one of the fixture oids.
enum RepoOid {
    Regular,
    Exec,
    Link,
    Bogus,
}

impl RepoOid {
    fn get(&self, repo: &Repo) -> String {
        match self {
            RepoOid::Regular => repo.regular_oid.clone(),
            RepoOid::Exec => repo.exec_oid.clone(),
            RepoOid::Link => repo.link_oid.clone(),
            RepoOid::Bogus => "0000000000000000000000000000000000000000".to_string(),
        }
    }
}

#[test]
fn git_regular_file_matches() {
    assert_git(
        "delete-git-ok",
        "file.txt",
        "100644",
        &RepoOid::Regular,
        true,
        None,
    );
}

#[test]
fn git_oid_mismatch_rejected() {
    assert_git(
        "delete-git-oid",
        "file.txt",
        "100644",
        &RepoOid::Exec,
        false,
        None,
    );
    assert_git(
        "delete-git-bogus",
        "file.txt",
        "100644",
        &RepoOid::Bogus,
        false,
        None,
    );
}

#[test]
fn git_executable_matches_755() {
    assert_git(
        "delete-git-exec",
        "run.sh",
        "100755",
        &RepoOid::Exec,
        true,
        None,
    );
}

#[test]
fn git_executable_rejected_as_644() {
    assert_git(
        "delete-git-exec644",
        "run.sh",
        "100644",
        &RepoOid::Exec,
        false,
        None,
    );
}

#[test]
fn git_regular_rejected_as_755() {
    // No execute bit on disk: the 755 class fails on both engines.
    assert_git(
        "delete-git-noexec",
        "file.txt",
        "100755",
        &RepoOid::Regular,
        false,
        None,
    );
}

#[test]
fn git_exec_bit_breaks_644() {
    // A late `chmod +x` breaks the 644 class without changing the
    // content oid.
    fn mutate(home: &Path) {
        chmod(&home.join("file.txt"), 0o755);
    }
    assert_git(
        "delete-git-chmod",
        "file.txt",
        "100644",
        &RepoOid::Regular,
        false,
        Some(mutate),
    );
}

#[test]
fn git_symlink_real_oid_rejected() {
    // The shell hashes the link target plus readlink's own trailing
    // newline (the `.`-sentinel capture strips only the dot), so a
    // real blob oid never matches: both engines must agree on
    // failure here (see the companion acceptance row below).
    assert_git(
        "delete-git-link",
        "link",
        "120000",
        &RepoOid::Link,
        false,
        None,
    );
}

#[test]
fn git_symlink_newline_oid_accepted() {
    // ...while the oid of `target + "\n"` passes on both engines,
    // pinning the quirk byte-for-byte.
    let twins = Twins::build("delete-git-linknl");
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    let mut target = b"link-target".to_vec();
    target.push(b'\n');
    let oid = dot::temp::file_text_digest(&source_root(), &target).expect("hash target");
    let (shell_ok, rust_ok) = check_git(&twins, &repo, "link", "120000", &oid, None);
    assert!(shell_ok, "shell accepts newline-suffixed oid");
    assert!(rust_ok, "rust accepts newline-suffixed oid");
}

#[test]
fn git_missing_target_rejected() {
    assert_git(
        "delete-git-missing",
        "absent.txt",
        "100644",
        &RepoOid::Regular,
        false,
        None,
    );
    assert_git(
        "delete-git-missinglink",
        "absent",
        "120000",
        &RepoOid::Link,
        false,
        None,
    );
}

#[test]
fn git_bad_mode_rejected() {
    assert_git(
        "delete-git-mode",
        "file.txt",
        "100777",
        &RepoOid::Regular,
        false,
        None,
    );
    assert_git(
        "delete-git-mode2",
        "file.txt",
        "040000",
        &RepoOid::Regular,
        false,
        None,
    );
    assert_git(
        "delete-git-mode3",
        "file.txt",
        "",
        &RepoOid::Regular,
        false,
        None,
    );
}

#[test]
fn git_directory_rejected() {
    // A directory is neither a link nor a regular file.
    fn mutate(home: &Path) {
        std::fs::create_dir_all(home.join("subdir")).expect("mkdir fixture");
    }
    let twins = Twins::build("delete-git-dir");
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    mutate(&twins.shell_home);
    mutate(&twins.rust_home);
    let (shell_ok, rust_ok) = check_git(&twins, &repo, "subdir", "100644", &repo.regular_oid, None);
    assert!(!shell_ok && !rust_ok);
}

#[test]
fn git_symlink_rejected_as_regular() {
    assert_git(
        "delete-git-linkreg",
        "link",
        "100644",
        &RepoOid::Regular,
        false,
        None,
    );
}

/// Compare one leaf row: each engine checks its own side's
/// candidate against that side's prepared identity (twin files share
/// bytes but never inodes, so one identity cannot serve both).
fn check_leaf(
    twins: &Twins,
    repo: &Repo,
    name: &str,
    shell_identity: &str,
    rust_identity: &str,
    mode: &str,
    oid: &str,
) -> (bool, bool) {
    let shell_candidate = twins.shell_home.join(name);
    let rust_candidate = twins.rust_home.join(name);
    let snippet = format!(
        "_dot_init_leaf_delete_matches {} {} {} {} {} {}; code=$?;",
        sq(&shell_candidate.to_string_lossy()),
        sq(shell_identity),
        sq(&repo.git_dir.to_string_lossy()),
        sq(&repo.commit),
        sq(mode),
        sq(oid),
    );
    let snippet = format!("{snippet} printf 'code=%s\\n' \"$code\"\n");
    let (verdict, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    let rust = delete::leaf_delete_matches(
        &rust_candidate,
        rust_identity,
        &twins.rust_home,
        &repo.git_dir,
        &repo.commit,
        mode,
        oid,
    );
    (verdict == 0, rust)
}

#[test]
fn leaf_match_accepted() {
    let twins = Twins::build("delete-leaf-ok");
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    let shell_identity = identity_of(&twins.shell_home.join("file.txt"));
    let rust_identity = identity_of(&twins.rust_home.join("file.txt"));
    let (shell_ok, rust_ok) = check_leaf(
        &twins,
        &repo,
        "file.txt",
        &shell_identity,
        &rust_identity,
        "100644",
        &repo.regular_oid,
    );
    assert!(shell_ok && rust_ok);
}

/// Rename-replace one leaf per side, keeping the bytes.
fn rename_replace_both(twins: &Twins, name: &str) {
    for home in [&twins.shell_home, &twins.rust_home] {
        let sibling = home.join("sibling");
        std::fs::write(&sibling, b"hello init\n").expect("write fixture");
        chmod(&sibling, 0o644);
        std::fs::rename(&sibling, home.join(name)).expect("rename fixture");
    }
}

#[test]
fn leaf_identity_mismatch_rejected() {
    // Rename-replace keeps the bytes but moves a live sibling inode
    // over the leaf, so each side's prepared identity no longer
    // matches its own live file.
    let twins = Twins::build("delete-leaf-ino");
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    let shell_old = identity_of(&twins.shell_home.join("file.txt"));
    let rust_old = identity_of(&twins.rust_home.join("file.txt"));
    rename_replace_both(&twins, "file.txt");
    assert_ne!(shell_old, identity_of(&twins.shell_home.join("file.txt")));
    assert_ne!(rust_old, identity_of(&twins.rust_home.join("file.txt")));
    let (shell_ok, rust_ok) = check_leaf(
        &twins,
        &repo,
        "file.txt",
        &shell_old,
        &rust_old,
        "100644",
        &repo.regular_oid,
    );
    assert!(!shell_ok && !rust_ok);
}

#[test]
fn leaf_outside_home_rejected() {
    // The right inode and content outside `HOME/` still fail the
    // prefix gate.
    let twins = Twins::build("delete-leaf-out");
    let repo = build_repo(twins.root());
    let foreign = twins.root().join("foreign.txt");
    std::fs::write(&foreign, b"hello init\n").expect("write fixture");
    chmod(&foreign, 0o644);
    let identity = identity_of(&foreign);
    let snippet = format!(
        "_dot_init_leaf_delete_matches {} {} {} {} {} {}; printf 'code=%s\\n' \"$?\"\n",
        sq(&foreign.to_string_lossy()),
        sq(&identity),
        sq(&repo.git_dir.to_string_lossy()),
        sq(&repo.commit),
        sq("100644"),
        sq(&repo.regular_oid),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    let rust = delete::leaf_delete_matches(
        &foreign,
        &identity,
        &twins.rust_home,
        &repo.git_dir,
        &repo.commit,
        "100644",
        &repo.regular_oid,
    );
    assert_eq!(verdict, 1);
    assert!(!rust);
}

#[test]
fn leaf_oid_mismatch_rejected() {
    let twins = Twins::build("delete-leaf-oid");
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    let shell_identity = identity_of(&twins.shell_home.join("file.txt"));
    let rust_identity = identity_of(&twins.rust_home.join("file.txt"));
    let (shell_ok, rust_ok) = check_leaf(
        &twins,
        &repo,
        "file.txt",
        &shell_identity,
        &rust_identity,
        "100644",
        &repo.exec_oid,
    );
    assert!(!shell_ok && !rust_ok);
}

#[test]
fn leaf_directory_rejected() {
    let twins = Twins::build("delete-leaf-dir");
    let repo = build_repo(twins.root());
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::create_dir_all(home.join("subdir")).expect("mkdir fixture");
        chmod(&home.join("subdir"), 0o755);
    }
    let shell_identity = identity_of(&twins.shell_home.join("subdir"));
    let rust_identity = identity_of(&twins.rust_home.join("subdir"));
    let (shell_ok, rust_ok) = check_leaf(
        &twins,
        &repo,
        "subdir",
        &shell_identity,
        &rust_identity,
        "100644",
        &repo.regular_oid,
    );
    assert!(!shell_ok && !rust_ok);
}

/// Compare one directory-matcher row: `build` raises the same shape
/// under each home, `shell_probe` renders the matcher call for one
/// side's absolute dir, `rust_probe` runs the port on the other
/// side, and both verdicts must equal `want`.
fn check_dir_pair(
    tag: &str,
    rel: &str,
    build: fn(&Path),
    want: bool,
    shell_probe: impl Fn(&str) -> String,
    rust_probe: impl Fn(&Path) -> bool,
) {
    let twins = Twins::build(tag);
    build(&twins.shell_home);
    build(&twins.rust_home);
    let shell_dir = twins.shell_home.join(rel).to_string_lossy().into_owned();
    let snippet = format!("{}; printf 'code=%s\\n' \"$?\"\n", shell_probe(&shell_dir));
    let (verdict, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    assert_eq!(verdict == 0, want, "shell verdict for {rel}");
    assert_eq!(rust_probe(&twins.rust_home.join(rel)), want, "rust verdict");
}

/// Raise an empty directory with exact `mode` under `home`.
fn mk_dir_mode(home: &Path, rel: &str, mode: u32) {
    let dir = home.join(rel);
    std::fs::create_dir_all(&dir).expect("mkdir fixture");
    chmod(&dir, mode);
}

fn build_700_empty(home: &Path) {
    mk_dir_mode(home, "stage", 0o700);
}

fn build_755_empty(home: &Path) {
    mk_dir_mode(home, "stage", 0o755);
}

fn build_770_empty(home: &Path) {
    mk_dir_mode(home, "stage", 0o770);
}

fn build_4700_empty(home: &Path) {
    mk_dir_mode(home, "stage", 0o4700);
}

fn build_700_with_file(home: &Path) {
    mk_dir_mode(home, "stage", 0o700);
    std::fs::write(home.join("stage/inner.txt"), b"x\n").expect("write fixture");
}

fn build_700_with_dotfile(home: &Path) {
    mk_dir_mode(home, "stage", 0o700);
    std::fs::write(home.join("stage/.dot-hidden"), b"x\n").expect("write fixture");
}

fn build_700_with_subdir(home: &Path) {
    mk_dir_mode(home, "stage", 0o700);
    std::fs::create_dir_all(home.join("stage/inner")).expect("mkdir fixture");
}

fn build_700_unreadable(home: &Path) {
    mk_dir_mode(home, "stage", 0o700);
    chmod(&home.join("stage"), 0o000);
}

fn build_plain_file(home: &Path) {
    std::fs::write(home.join("stage"), b"x\n").expect("write fixture");
    chmod(&home.join("stage"), 0o644);
}

fn build_symlink_to_dir(home: &Path) {
    mk_dir_mode(home, "real", 0o700);
    std::os::unix::fs::symlink("real", home.join("stage")).expect("symlink fixture");
}

fn build_nothing(_home: &Path) {}

/// Probe the private-directory matcher with no filters.
fn probe_private_bare(dir: &str) -> String {
    format!("_dot_init_private_directory_matches {}", sq(dir))
}

/// Probe the private-directory matcher with a mode filter, keeping
/// the identity default.
fn probe_private_mode(dir: &str, mode: &str) -> String {
    format!(
        "_dot_init_private_directory_matches {} '' {}",
        sq(dir),
        sq(mode)
    )
}

/// Probe the private-directory matcher with this side's own
/// identity plus a mode filter.
fn probe_private_self(dir: &str, mode: &str) -> String {
    format!(
        "d={}; _dot_init_private_directory_matches \"$d\" \"$(_dot_path_identity \"$d\")\" {}",
        sq(dir),
        sq(mode)
    )
}

/// Probe the private-directory matcher with a sibling's identity
/// (same device, provably different inode).
fn probe_private_sibling(dir: &str, mode: &str) -> String {
    format!(
        "d={}; o=\"$d-sibling\"; mkdir -p \"$o\"; _dot_init_private_directory_matches \"$d\" \"$(_dot_path_identity \"$o\")\" {}",
        sq(dir),
        sq(mode)
    )
}

#[test]
fn private_dir_accepted_bare() {
    check_dir_pair(
        "delete-priv-bare",
        "stage",
        build_700_empty,
        true,
        probe_private_bare,
        |path| delete::private_directory_matches(path, None, None),
    );
}

#[test]
fn private_dir_accepted_with_filters() {
    check_dir_pair(
        "delete-priv-self",
        "stage",
        build_700_empty,
        true,
        |dir| probe_private_self(dir, "700"),
        |path| delete::private_directory_matches(path, Some(&identity_of(path)), Some("700")),
    );
}

#[test]
fn private_dir_mode_filter_mismatch() {
    check_dir_pair(
        "delete-priv-mode",
        "stage",
        build_700_empty,
        false,
        |dir| probe_private_mode(dir, "755"),
        |path| delete::private_directory_matches(path, None, Some("755")),
    );
}

#[test]
fn private_dir_identity_mismatch() {
    check_dir_pair(
        "delete-priv-ident",
        "stage",
        build_700_empty,
        false,
        |dir| probe_private_sibling(dir, "700"),
        |path| {
            let sibling = path.parent().expect("stage parent").join("stage-sibling");
            std::fs::create_dir_all(&sibling).expect("mkdir fixture");
            let verdict =
                delete::private_directory_matches(path, Some(&identity_of(&sibling)), Some("700"));
            std::fs::remove_dir(&sibling).expect("cleanup sibling");
            verdict
        },
    );
}

#[test]
fn private_dir_permissive_modes_rejected() {
    for (tag, build) in [
        ("delete-priv-755", build_755_empty as fn(&Path)),
        ("delete-priv-770", build_770_empty as fn(&Path)),
    ] {
        check_dir_pair(tag, "stage", build, false, probe_private_bare, |path| {
            delete::private_directory_matches(path, None, None)
        });
    }
}

#[test]
fn private_dir_setuid_only_accepted() {
    // Group/other bits are clear, so the `077` mask passes; the
    // mode string renders `4700` on both engines.
    check_dir_pair(
        "delete-priv-suid",
        "stage",
        build_4700_empty,
        true,
        |dir| probe_private_mode(dir, "4700"),
        |path| delete::private_directory_matches(path, None, Some("4700")),
    );
    check_dir_pair(
        "delete-priv-suidbare",
        "stage",
        build_4700_empty,
        true,
        probe_private_bare,
        |path| delete::private_directory_matches(path, None, None),
    );
}

#[test]
fn private_dir_symlink_and_file_rejected() {
    check_dir_pair(
        "delete-priv-link",
        "stage",
        build_symlink_to_dir,
        false,
        probe_private_bare,
        |path| delete::private_directory_matches(path, None, None),
    );
    check_dir_pair(
        "delete-priv-file",
        "stage",
        build_plain_file,
        false,
        probe_private_bare,
        |path| delete::private_directory_matches(path, None, None),
    );
}

#[test]
fn private_dir_missing_rejected() {
    check_dir_pair(
        "delete-priv-missing",
        "stage",
        build_nothing,
        false,
        probe_private_bare,
        |path| delete::private_directory_matches(path, None, None),
    );
}

/// Probe the empty-directory matcher with no filters.
fn probe_empty_bare(dir: &str) -> String {
    format!("_dot_init_private_empty_directory_matches {}", sq(dir))
}

#[test]
fn empty_dir_accepted() {
    check_dir_pair(
        "delete-empty-ok",
        "stage",
        build_700_empty,
        true,
        probe_empty_bare,
        |path| delete::private_empty_directory_matches(path, None, None),
    );
}

#[test]
fn empty_dir_rejects_entries() {
    // A plain file, a dotfile, and a subdirectory all count: the
    // shell globs with `dotglob`, so nothing hides.
    for (tag, build) in [
        ("delete-empty-file", build_700_with_file as fn(&Path)),
        ("delete-empty-dot", build_700_with_dotfile as fn(&Path)),
        ("delete-empty-sub", build_700_with_subdir as fn(&Path)),
    ] {
        check_dir_pair(tag, "stage", build, false, probe_empty_bare, |path| {
            delete::private_empty_directory_matches(path, None, None)
        });
    }
}

#[test]
fn empty_dir_unreadable_reads_empty() {
    // The shell's `nullglob` drops an unreadable expansion to
    // nothing, so mode-000 reads as empty on both engines (root
    // runners truly see an empty dir; same verdict either way).
    check_dir_pair(
        "delete-empty-noperm",
        "stage",
        build_700_unreadable,
        true,
        probe_empty_bare,
        |path| delete::private_empty_directory_matches(path, None, None),
    );
}

#[test]
fn empty_dir_missing_rejected() {
    check_dir_pair(
        "delete-empty-missing",
        "stage",
        build_nothing,
        false,
        probe_empty_bare,
        |path| delete::private_empty_directory_matches(path, None, None),
    );
}

/// Probe the parent matcher with this side's own identity and mode.
fn probe_parent_self(dir: &str) -> String {
    format!(
        "d={}; _dot_init_parent_delete_matches \"$d\" \"$(_dot_path_identity \"$d\")\" \"$(stat -c '%a' \"$d\" 2>/dev/null || stat -f '%Lp' \"$d\")\"",
        sq(dir),
    )
}

/// Probe the parent matcher with a fixed identity and mode.
fn probe_parent_fixed(dir: &str, identity: &str, mode: &str) -> String {
    format!(
        "_dot_init_parent_delete_matches {} {} {}",
        sq(dir),
        sq(identity),
        sq(mode)
    )
}

#[test]
fn parent_match_accepted() {
    check_dir_pair(
        "delete-parent-ok",
        "stage",
        build_700_empty,
        true,
        probe_parent_self,
        |path| delete::parent_delete_matches(path, &identity_of(path), "700"),
    );
}

#[test]
fn parent_nonempty_rejected() {
    check_dir_pair(
        "delete-parent-full",
        "stage",
        build_700_with_file,
        false,
        probe_parent_self,
        |path| delete::parent_delete_matches(path, &identity_of(path), "700"),
    );
}

#[test]
fn parent_identity_mismatch_rejected() {
    // A live directory with a stranger's identity fails.
    let twins = Twins::build("delete-parent-ident");
    build_700_empty(&twins.shell_home);
    build_700_empty(&twins.rust_home);
    let shell_dir = twins.shell_home.join("stage");
    let rust_dir = twins.rust_home.join("stage");
    let snippet = format!(
        "{}; printf 'code=%s\\n' \"$?\"\n",
        probe_parent_fixed(&shell_dir.to_string_lossy(), "999999999:999999999", "700")
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    assert_eq!(verdict, 1);
    assert!(!delete::parent_delete_matches(
        &rust_dir,
        "999999999:999999999",
        "700"
    ));
}

#[test]
fn parent_not_a_directory_rejected() {
    check_dir_pair(
        "delete-parent-file",
        "stage",
        build_plain_file,
        false,
        probe_parent_self,
        |path| {
            // A file has an identity but no directory shape; the
            // mode filter is fixed so both sides read the same row.
            delete::parent_delete_matches(path, &identity_of(path), "644")
        },
    );
}

/// Stub verifiers for the parked-generation rows: the shell side
/// defines the same shapes inline in each snippet.
fn rust_true(_park: &Path) -> bool {
    true
}

fn rust_false(_park: &Path) -> bool {
    false
}

/// Drive one parked-generation row on both engines with the same
/// verifier shape: a plain file target under each home, the park
/// derived per side, and the verdicts compared against `want`.
/// Returns the twins for aftermath assertions.
fn check_parked(
    tag: &str,
    target_rel: &str,
    remover: &str,
    shell_defs: &str,
    shell_name: &str,
    rust_verifier: &dyn Fn(&Path) -> bool,
    want: bool,
) -> Twins {
    let twins = Twins::build(tag);
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::write(home.join(target_rel), b"hello init\n").expect("write fixture");
        chmod(&home.join(target_rel), 0o644);
    }
    let shell_target = twins.shell_home.join(target_rel);
    let rust_target = twins.rust_home.join(target_rel);
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" leaf {}; park=$REPLY; {} _dot_init_delete_parked_generation \"$target\" \"$park\" {} {}; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq(target_rel),
        shell_defs,
        sq(remover),
        shell_name,
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(
        verdict == 0,
        want,
        "shell verdict for {target_rel}/{remover}"
    );
    let root = source_root();
    let park = delete::delete_park_path(&rust_target, "leaf", target_rel, NONCE, &root)
        .expect("rust park");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, remover, rust_verifier, &mut cache)
            .is_ok();
    assert_eq!(rust, want, "rust verdict for {target_rel}/{remover}");
    twins
}

#[test]
fn parked_leaf_true_verifier_removes() {
    // Stub verifier passes: the target parks, verifies, and both
    // names are gone on both engines.
    let twins = check_parked(
        "delete-parked-leaf",
        "gone.txt",
        "leaf",
        "verifier_true() { return 0; };",
        "verifier_true",
        &rust_true,
        true,
    );
    for home in [&twins.shell_home, &twins.rust_home] {
        assert!(!present(&home.join("gone.txt")));
    }
}

#[test]
fn parked_verify_failure_restores() {
    // Stub verifier fails: the moved generation comes back with its
    // bytes, and the verdict still fails on both engines.
    let twins = check_parked(
        "delete-parked-restore",
        "back.txt",
        "leaf",
        "verifier_false() { return 1; };",
        "verifier_false",
        &rust_false,
        false,
    );
    for home in [&twins.shell_home, &twins.rust_home] {
        let target = home.join("back.txt");
        assert!(present(&target));
        assert_eq!(std::fs::read(&target).expect("read back"), b"hello init\n");
    }
}

#[test]
fn parked_noop_when_both_absent() {
    // Neither name exists: success without creating anything.
    let twins = Twins::build("delete-parked-noop");
    let root = source_root();
    let shell_target = twins.shell_home.join("absent.txt");
    let rust_target = twins.rust_home.join("absent.txt");
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" leaf {}; park=$REPLY; verifier_true() {{ return 0; }}; _dot_init_delete_parked_generation \"$target\" \"$park\" leaf verifier_true; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("absent.txt"),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 0);
    let park =
        delete::delete_park_path(&rust_target, "leaf", "absent.txt", NONCE, &root).expect("park");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, "leaf", &rust_true, &mut cache)
            .is_ok();
    assert!(rust);
    assert!(!present(&rust_target) && !present(&park));
}

#[test]
fn parked_resume_with_preexisting_park() {
    // Crash-resume shape: the park already holds the generation and
    // the target is vacant, so no move happens before verification.
    let twins = Twins::build("delete-parked-resume");
    let root = source_root();
    for home in [&twins.shell_home, &twins.rust_home] {
        let target = home.join("resume.txt");
        std::fs::write(&target, b"hello init\n").expect("write fixture");
        chmod(&target, 0o644);
        let park =
            delete::delete_park_path(&target, "leaf", "resume.txt", NONCE, &root).expect("park");
        std::fs::rename(&target, &park).expect("pre-park fixture");
    }
    let shell_target = twins.shell_home.join("resume.txt");
    let rust_target = twins.rust_home.join("resume.txt");
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" leaf {}; park=$REPLY; verifier_true() {{ return 0; }}; _dot_init_delete_parked_generation \"$target\" \"$park\" leaf verifier_true; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("resume.txt"),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 0);
    let park =
        delete::delete_park_path(&rust_target, "leaf", "resume.txt", NONCE, &root).expect("park");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, "leaf", &rust_true, &mut cache)
            .is_ok();
    assert!(rust);
    assert!(!present(&rust_target) && !present(&park));
}

#[test]
fn parked_target_won_reports_failure() {
    // The park verifies but the target reappeared: removal already
    // happened, yet the verdict fails on both engines while the
    // target bytes stay intact.
    let twins = Twins::build("delete-parked-won");
    let root = source_root();
    for home in [&twins.shell_home, &twins.rust_home] {
        let target = home.join("won.txt");
        std::fs::write(&target, b"hello init\n").expect("write fixture");
        chmod(&target, 0o644);
        let park =
            delete::delete_park_path(&target, "leaf", "won.txt", NONCE, &root).expect("park");
        std::fs::write(&park, b"stale\n").expect("pre-park fixture");
    }
    let shell_target = twins.shell_home.join("won.txt");
    let rust_target = twins.rust_home.join("won.txt");
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" leaf {}; park=$REPLY; verifier_true() {{ return 0; }}; _dot_init_delete_parked_generation \"$target\" \"$park\" leaf verifier_true; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("won.txt"),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 1);
    let park =
        delete::delete_park_path(&rust_target, "leaf", "won.txt", NONCE, &root).expect("park");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, "leaf", &rust_true, &mut cache)
            .is_ok();
    assert!(!rust);
    for home in [&twins.shell_home, &twins.rust_home] {
        assert_eq!(
            std::fs::read(home.join("won.txt")).expect("target intact"),
            b"hello init\n"
        );
    }
}

#[test]
fn parked_bad_remover_keeps_park() {
    // Unknown remover fails after verification, leaving the parked
    // generation in place on both engines.
    let twins = Twins::build("delete-parked-remover");
    let root = source_root();
    for home in [&twins.shell_home, &twins.rust_home] {
        let target = home.join("kept.txt");
        std::fs::write(&target, b"hello init\n").expect("write fixture");
        chmod(&target, 0o644);
        let park =
            delete::delete_park_path(&target, "leaf", "kept.txt", NONCE, &root).expect("park");
        std::fs::rename(&target, &park).expect("pre-park fixture");
    }
    let shell_target = twins.shell_home.join("kept.txt");
    let rust_target = twins.rust_home.join("kept.txt");
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" leaf {}; park=$REPLY; verifier_true() {{ return 0; }}; _dot_init_delete_parked_generation \"$target\" \"$park\" bogus verifier_true; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("kept.txt"),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 1);
    let park =
        delete::delete_park_path(&rust_target, "leaf", "kept.txt", NONCE, &root).expect("park");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, "bogus", &rust_true, &mut cache)
            .is_ok();
    assert!(!rust);
    let shell_park = delete::delete_park_path(&shell_target, "leaf", "kept.txt", NONCE, &root)
        .expect("shell park");
    assert!(present(&shell_park));
    assert!(present(&park));
}

#[test]
fn parked_leaf_round_trip_with_real_verifier() {
    // End to end with the real leaf matcher: the parked content
    // still matches the candidate, so the leaf publishes away.
    let twins = Twins::build("delete-parked-real");
    let repo = build_repo(twins.root());
    build_home(&twins.shell_home);
    build_home(&twins.rust_home);
    let shell_target = twins.shell_home.join("file.txt");
    let rust_target = twins.rust_home.join("file.txt");
    let shell_identity = identity_of(&shell_target);
    let rust_identity = identity_of(&rust_target);
    let snippet = format!(
        "target={}; key={}; _dot_init_delete_park_path \"$target\" leaf \"$key\"; park=$REPLY; _dot_init_delete_parked_generation \"$target\" \"$park\" leaf _dot_init_leaf_delete_matches {} {} {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("file.txt"),
        sq(&shell_identity),
        sq(&repo.git_dir.to_string_lossy()),
        sq(&repo.commit),
        sq("100644"),
        sq(&repo.regular_oid),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 0);
    let root = source_root();
    let park =
        delete::delete_park_path(&rust_target, "leaf", "file.txt", NONCE, &root).expect("park");
    let verifier = |park: &Path| {
        delete::leaf_delete_matches(
            park,
            &rust_identity,
            &twins.rust_home,
            &repo.git_dir,
            &repo.commit,
            "100644",
            &repo.regular_oid,
        )
    };
    let mut cache = MoveCache::default();
    let rust = delete::delete_parked_generation(&rust_target, &park, "leaf", &verifier, &mut cache)
        .is_ok();
    assert!(rust);
    assert!(!present(&rust_target) && !present(&park));
}

#[test]
fn parked_parent_remover_round_trip() {
    // An empty private directory parks and removes with the real
    // parent matcher.
    let twins = Twins::build("delete-parked-parent");
    for home in [&twins.shell_home, &twins.rust_home] {
        mk_dir_mode(home, "pdir", 0o700);
    }
    let shell_target = twins.shell_home.join("pdir");
    let rust_target = twins.rust_home.join("pdir");
    let shell_identity = identity_of(&shell_target);
    let rust_identity = identity_of(&rust_target);
    let snippet = format!(
        "target={}; key={}; _dot_init_delete_park_path \"$target\" parent \"$key\"; park=$REPLY; _dot_init_delete_parked_generation \"$target\" \"$park\" parent _dot_init_parent_delete_matches {} {}; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("pdir"),
        sq(&shell_identity),
        sq("700"),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 0);
    let root = source_root();
    let park =
        delete::delete_park_path(&rust_target, "parent", "pdir", NONCE, &root).expect("park");
    let verifier = |park: &Path| delete::parent_delete_matches(park, &rust_identity, "700");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, "parent", &verifier, &mut cache)
            .is_ok();
    assert!(rust);
    assert!(!present(&rust_target) && !present(&park));
}

#[test]
fn parked_tree_remover_round_trip() {
    // A directory tree parks and removes wholesale with a passing
    // stub verifier.
    let twins = Twins::build("delete-parked-tree");
    for home in [&twins.shell_home, &twins.rust_home] {
        let target = home.join("tree");
        std::fs::create_dir_all(target.join("a/b")).expect("mkdir fixture");
        std::fs::write(target.join("a/b/deep.txt"), b"deep\n").expect("write fixture");
    }
    let shell_target = twins.shell_home.join("tree");
    let rust_target = twins.rust_home.join("tree");
    let snippet = format!(
        "target={}; _dot_init_delete_park_path \"$target\" parent {}; park=$REPLY; verifier_true() {{ return 0; }}; _dot_init_delete_parked_generation \"$target\" \"$park\" tree verifier_true; code=$?; printf 'code=%s\\n' \"$code\"\n",
        sq(&shell_target.to_string_lossy()),
        sq("tree"),
    );
    let (verdict, _, _) = shell_run(&twins.shell_home, &[("DOT_INIT_NONCE", NONCE)], &snippet);
    assert_eq!(verdict, 0);
    let root = source_root();
    let park =
        delete::delete_park_path(&rust_target, "parent", "tree", NONCE, &root).expect("park");
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &park, "tree", &rust_true, &mut cache)
            .is_ok();
    assert!(rust);
    assert!(!present(&rust_target) && !present(&park));
}
