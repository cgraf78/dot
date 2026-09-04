//! Differential parity tests for the init candidate planning family
//! (`lib/dot/init-client.sh`, the tree/snapshot/conflict chapter)
//! against the live shell: the symlink-blob byte gate, the
//! candidate-tree writer, the per-path candidate matcher, the
//! live-filesystem snapshot probe and its recheck, the conflict-root
//! walk, and the prior/conflicts publisher.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so journals
//! and live paths never collide. Content-derived outputs (tree rows,
//! blob digests) compare byte-for-byte off one shared fixture
//! repository, which both engines only ever read; live identities
//! (device/inode) only gate verdicts, never equality — journal rows
//! compare with those two fields normalized away.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_candidate::{self as candidate, CandidateScope};
use dot::reserved::RootsInput;
use dot::test_support::TempDir;

/// Sources for the candidate chapter: the resource runtime (cleanup
/// mktemp backing the tree scan), the shared temp helpers (stat
/// probes, stdin hashing), the XDG resolver and reserved inventory
/// behind the candidate gate, and the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the init runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that
/// verdict — not the process status, which only says the printer
/// ran. A snippet that never reports (a harness bug, never a pass)
/// yields 99.
///
/// The locale stays pinned: git diagnostics must read English on
/// both engines, and the port pins `LC_ALL=C` around every git run.
/// No run-identity globals reach this family, so the environment
/// carries only the home, source root, and test gate.
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

/// Twin homes: disjoint directories so journals and live paths never
/// collide across engines. Fixture git repositories stay shared —
/// both engines only read them.
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

    fn repo(&self, name: &str) -> PathBuf {
        let repo = self._dir.path().join(name);
        std::fs::create_dir_all(&repo).expect("repo dir");
        repo
    }
}

/// Run git for fixtures, with a pinned identity for commits and a
/// hermetic config: user-global settings (commit hooks, format
/// fixers, template directories) must not leak into fixture
/// assembly — a formatter hook rewrites shell-script fixtures and
/// fails their commits, which would make rows order- and
/// machine-dependent.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// Fresh `main`-branch repository, hermetic like [`git`].
fn git_init(repo: &Path) {
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("-b")
        .arg("main")
        .arg(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init {}", repo.display());
}

/// Stage one blob by hash: links whose targets carry bytes the
/// filesystem tools cannot spell (like a newline) still commit
/// through `--cacheinfo`.
fn stage_link(repo: &Path, name: &str, target: &[u8]) {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .as_mut()
        .expect("hash stdin")
        .write_all(target)
        .expect("feed hash-object");
    let output = child.wait_with_output().expect("wait hash-object");
    assert!(output.status.success(), "hash link target");
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    git(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{hash},{name}"),
        ],
    );
}

/// Commit everything with `message`.
fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Mirror one live file into both twin homes at the same mode.
fn mirror(twins: &Twins, rel: &str, bytes: &[u8], mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    for home in [&twins.shell_home, &twins.rust_home] {
        let path = home.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mirror parents");
        }
        std::fs::write(&path, bytes).expect("mirror file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("mirror mode");
    }
}

/// Mirror one live symlink into both twin homes.
fn mirror_link(twins: &Twins, rel: &str, target: &str) {
    for home in [&twins.shell_home, &twins.rust_home] {
        let path = home.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mirror parents");
        }
        std::os::unix::fs::symlink(target, &path).expect("mirror link");
    }
}

/// Client scope for one home, with the reserved snapshot built from
/// the same defaults the shell probe sees (no `XDG_*`, `SHDEPS_*`,
/// `OVERLAYS`, or `DOT_INIT_BACKUP` overrides on either engine).
fn scope_for(home: &Path) -> CandidateScope {
    let home_text = home.to_string_lossy().into_owned();
    let input = RootsInput {
        home: home_text.clone(),
        state_home: format!("{home_text}/.local/state"),
        install_root: format!("{home_text}/.local/share"),
        provider_state: format!("{home_text}/.local/state/shdeps"),
        overlay_paths: Vec::new(),
        init_backup: None,
    };
    let roots = dot::reserved::reserved_roots(&input, &home_text).expect("reserved roots");
    CandidateScope {
        home: home_text.clone(),
        checkout: format!("{home_text}/.local/share/cgraf78/dot"),
        pwd: home_text,
        source_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        roots,
    }
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

/// Normalize the device/inode fields of snapshot-shaped rows away:
/// `path\tkind\tdev\tino\tmode\tsize\tvalue` (journals) or the
/// bare six-field form (snapshots). Kind, mode, size, and value
/// compare exactly; the live identities only passed their numeric
/// shape check on the way in.
fn normalize_row(line: &str) -> String {
    let fields: Vec<&str> = line.split('\t').collect();
    let (head, kind, dev, ino, mode, size, value) = match fields.as_slice() {
        [path, kind, dev, ino, mode, size, value] => {
            (Some(*path), *kind, *dev, *ino, *mode, *size, *value)
        }
        [kind, dev, ino, mode, size, value] => (None, *kind, *dev, *ino, *mode, *size, *value),
        _ => return format!("MALFORMED:{line}"),
    };
    if kind == "absent" {
        assert_eq!(
            (dev, ino, mode, size, value),
            ("-", "-", "-", "-", "-"),
            "absent fields in {line}"
        );
        return line.to_string();
    }
    assert!(
        dev.bytes().all(|byte| byte.is_ascii_digit()) && !dev.is_empty(),
        "numeric dev in {line}"
    );
    assert!(
        ino.bytes().all(|byte| byte.is_ascii_digit()) && !ino.is_empty(),
        "numeric ino in {line}"
    );
    match head {
        Some(path) => format!("{path}\t{kind}\tD\tI\t{mode}\t{size}\t{value}"),
        None => format!("{kind}\tD\tI\t{mode}\t{size}\t{value}"),
    }
}

/// Normalized journal lines for one journal file.
fn journal_rows(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).expect("read journal");
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines.iter().map(|line| normalize_row(line)).collect()
}

/// Shell probe ending in the `code=` verdict for
/// `_dot_init_symlink_blob_safe`.
fn blob_snippet(repo: &Path, branch: &str, path: &str) -> String {
    format!(
        "if _dot_init_symlink_blob_safe {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&repo.to_string_lossy()),
        sq(branch),
        sq(path),
    )
}

/// One blob row across both engines.
fn check_blob(repo: &Path, twins: &Twins, branch: &str, path: &str, want: i32) {
    let (shell_code, _, _) = shell_run(&twins.shell_home, &[], &blob_snippet(repo, branch, path));
    let rust_code = if candidate::symlink_blob_safe(repo, branch, path) {
        0
    } else {
        1
    };
    assert_eq!(shell_code, want, "shell blob verdict for {path}");
    assert_eq!(rust_code, want, "rust blob verdict for {path}");
}

/// Repository of small blobs covering the byte gate. The clean blob
/// carries no newline: `0a` is itself a forbidden byte, since a
/// link target holding one would break the tab-separated journals.
fn blob_repo(twins: &Twins) -> PathBuf {
    let repo = twins.repo("blobs");
    git_init(&repo);
    stage(&repo, "good.txt", b"hello");
    stage(&repo, "empty.txt", b"");
    stage(&repo, "big.bin", &vec![b'x'; 5000]);
    stage(&repo, "nul.bin", b"a\x00b");
    stage(&repo, "tab.bin", b"a\tb");
    stage(&repo, "nl.bin", b"a\nb");
    stage(&repo, "cr.bin", b"a\rb");
    commit_all(&repo, "blobs");
    repo
}

#[test]
fn blob_safe_matrix() {
    let twins = Twins::build("blob-matrix");
    let repo = blob_repo(&twins);
    for (path, want) in [
        ("good.txt", 0),
        ("empty.txt", 1),
        ("big.bin", 1),
        ("nul.bin", 1),
        ("tab.bin", 1),
        ("nl.bin", 1),
        ("cr.bin", 1),
        ("missing.txt", 1),
    ] {
        check_blob(&repo, &twins, "main", path, want);
    }
    check_blob(&repo, &twins, "no-such-branch", "good.txt", 1);
}

#[test]
fn blob_safe_size_edges() {
    let twins = Twins::build("blob-edges");
    let repo = twins.repo("edges");
    git_init(&repo);
    stage(&repo, "exact.bin", &vec![b'y'; 4096]);
    stage(&repo, "over.bin", &vec![b'y'; 4097]);
    commit_all(&repo, "edges");
    check_blob(&repo, &twins, "main", "exact.bin", 0);
    check_blob(&repo, &twins, "main", "over.bin", 1);
}

/// Shell probe ending in the `code=` verdict for
/// `_dot_init_candidate_tree`.
fn tree_snippet(repo: &Path, branch: &str, output: &Path) -> String {
    format!(
        "if _dot_init_candidate_tree {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&repo.to_string_lossy()),
        sq(branch),
        sq(&output.to_string_lossy()),
    )
}

/// One tree row across both engines, comparing verdicts and the
/// emitted inventory bytes.
fn check_tree(repo: &Path, twins: &Twins, branch: &str, want: i32) -> (Vec<u8>, Vec<u8>) {
    let shell_out = twins.shell_home.join("tree.tsv");
    let rust_out = twins.rust_home.join("tree.tsv");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &tree_snippet(repo, branch, &shell_out),
    );
    let scope = scope_for(&twins.rust_home);
    let rust_code = match candidate::candidate_tree(repo, branch, &rust_out, &scope) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    assert_eq!(shell_code, want, "shell tree verdict for {branch}");
    assert_eq!(rust_code, want, "rust tree verdict for {branch}");
    let shell_bytes = std::fs::read(&shell_out).expect("read shell tree");
    let rust_bytes = std::fs::read(&rust_out).expect("read rust tree");
    assert_eq!(
        shell_bytes, rust_bytes,
        "tree bytes agree for branch {branch}"
    );
    (shell_bytes, rust_bytes)
}

/// Repository with one regular file, one executable, and one good
/// symlink.
fn simple_repo(twins: &Twins) -> PathBuf {
    let repo = twins.repo("simple");
    git_init(&repo);
    stage(&repo, "hello.txt", b"hello\n");
    stage(&repo, "run.sh", b"#!/bin/sh\necho hi\n");
    stage(&repo, "data.bin", b"payload");
    set_mode(&repo.join("run.sh"), 0o755);
    std::os::unix::fs::symlink("hello.txt", repo.join("link")).expect("repo link");
    commit_all(&repo, "simple");
    repo
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn tree_accepts_simple() {
    let twins = Twins::build("tree-simple");
    let repo = simple_repo(&twins);
    let (shell_bytes, _) = check_tree(&repo, &twins, "main", 0);
    let text = String::from_utf8(shell_bytes).expect("tree is UTF-8");
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines.len(), 4, "four accepted leaves");
    assert!(
        lines[0].starts_with("100644\t"),
        "regular row: {}",
        lines[0]
    );
    assert!(
        lines.iter().any(|line| line.starts_with("100755\t")),
        "executable row survives"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("120000\t")),
        "symlink row survives"
    );
    assert!(
        lines.iter().all(|line| line.split('\t').count() == 3),
        "three tab fields per row"
    );
}

#[test]
fn tree_rejects_tab_name() {
    let twins = Twins::build("tree-tab");
    let repo = twins.repo("tabbed");
    git_init(&repo);
    // A tab cannot appear in a safe relative path; git tracks the
    // file fine, so the planner must refuse the whole inventory.
    stage(&repo, "a\tb", b"x\n");
    commit_all(&repo, "tabbed");
    let (shell_bytes, rust_bytes) = check_tree(&repo, &twins, "main", 1);
    assert!(shell_bytes.is_empty(), "shell truncates on reject");
    assert!(rust_bytes.is_empty(), "rust truncates on reject");
}

#[test]
fn tree_rejects_reserved_dotfiles() {
    let twins = Twins::build("tree-reserved");
    let repo = twins.repo("reserved");
    git_init(&repo);
    // `$HOME/.dotfiles` is a reserved root under both twin homes, so
    // a candidate owning that name is unsafe on either engine.
    stage(&repo, ".dotfiles", b"overtake\n");
    commit_all(&repo, "reserved");
    let (shell_bytes, rust_bytes) = check_tree(&repo, &twins, "main", 1);
    assert!(shell_bytes.is_empty(), "shell truncates on reject");
    assert!(rust_bytes.is_empty(), "rust truncates on reject");
}

/// Repository exercising the generated-adapter exception on three
/// branches: exact launcher bytes at 100755 (allowed), altered
/// bytes (refused), and exact bytes at the wrong mode (refused).
fn adapter_repo(twins: &Twins) -> PathBuf {
    let repo = twins.repo("adapter");
    git_init(&repo);
    let launcher =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("support/client-launcher.sh"))
            .expect("launcher bytes");
    stage(&repo, "seed.txt", b"seed\n");
    commit_all(&repo, "seed");
    git(&repo, &["checkout", "-qb", "good"]);
    stage(&repo, ".local/bin/dot", &launcher);
    set_mode(&repo.join(".local/bin/dot"), 0o755);
    commit_all(&repo, "adapter good");
    git(&repo, &["checkout", "-qb", "bad-bytes", "main"]);
    let mut forged = launcher.clone();
    forged.push(b'\n');
    stage(&repo, ".local/bin/dot", &forged);
    set_mode(&repo.join(".local/bin/dot"), 0o755);
    commit_all(&repo, "adapter forged");
    git(&repo, &["checkout", "-qb", "bad-mode", "main"]);
    stage(&repo, ".local/bin/dot", &launcher);
    set_mode(&repo.join(".local/bin/dot"), 0o644);
    commit_all(&repo, "adapter wrong mode");
    repo
}

#[test]
fn tree_adapter_exception() {
    let twins = Twins::build("tree-adapter");
    let repo = adapter_repo(&twins);
    let (good_bytes, _) = check_tree(&repo, &twins, "good", 0);
    let good_text = String::from_utf8(good_bytes).expect("tree is UTF-8");
    assert!(
        good_text
            .lines()
            .any(|line| line.ends_with("\t.local/bin/dot") && line.starts_with("100755\t")),
        "adapter row survives"
    );
    check_tree(&repo, &twins, "bad-bytes", 1);
    check_tree(&repo, &twins, "bad-mode", 1);
}

#[test]
fn tree_rejects_bad_symlink() {
    let twins = Twins::build("tree-symlink");
    let repo = twins.repo("badlink");
    git_init(&repo);
    stage(&repo, "plain.txt", b"plain\n");
    commit_all(&repo, "plain");
    // The gate checks the link TARGET bytes (`git show` of the
    // symlink blob), not the file the target names: a newline in
    // the target would break the tab-separated inventory. The
    // commit takes the index as staged - a later `add -A` would
    // stage-delete the worktree-less entry.
    stage_link(&repo, "badlink", b"a\nb");
    git(&repo, &["commit", "-qm", "badlink"]);
    let (shell_bytes, rust_bytes) = check_tree(&repo, &twins, "main", 1);
    assert!(shell_bytes.is_empty(), "shell truncates on reject");
    assert!(rust_bytes.is_empty(), "rust truncates on reject");
}

#[test]
fn tree_accepts_dangling_symlink() {
    let twins = Twins::build("tree-dangle");
    let repo = twins.repo("dangle");
    git_init(&repo);
    // Liveness is never probed here: a dangling link with clean
    // target bytes is a valid candidate row.
    std::os::unix::fs::symlink("nowhere", repo.join("dangle")).expect("repo link");
    commit_all(&repo, "dangle");
    let (shell_bytes, _) = check_tree(&repo, &twins, "main", 0);
    let text = String::from_utf8(shell_bytes).expect("tree is UTF-8");
    assert!(
        text.lines()
            .any(|line| line.starts_with("120000\t") && line.ends_with("\tdangle")),
        "dangling link row survives"
    );
}

#[test]
fn tree_empty_and_missing() {
    let twins = Twins::build("tree-empty");
    let repo = twins.repo("empty");
    git_init(&repo);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "empty"]);
    let (shell_bytes, rust_bytes) = check_tree(&repo, &twins, "main", 1);
    assert!(shell_bytes.is_empty(), "shell truncates an empty tree");
    assert!(rust_bytes.is_empty(), "rust truncates an empty tree");
    check_tree(&repo, &twins, "no-such-branch", 1);
}

/// Shell probe ending in the `code=` verdict for
/// `_dot_init_candidate_matches_path`.
fn matches_snippet(repo: &Path, branch: &str, mode: &str, path: &str) -> String {
    format!(
        "if _dot_init_candidate_matches_path {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&repo.to_string_lossy()),
        sq(branch),
        sq(mode),
        sq(path),
    )
}

/// One matcher row across both engines.
fn check_matches(repo: &Path, twins: &Twins, branch: &str, mode: &str, path: &str, want: i32) {
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &matches_snippet(repo, branch, mode, path),
    );
    let scope = scope_for(&twins.rust_home);
    let rust_code = if candidate::candidate_matches_path(repo, branch, mode, path, &scope) {
        0
    } else {
        1
    };
    assert_eq!(shell_code, want, "shell match verdict for {path}@{mode}");
    assert_eq!(rust_code, want, "rust match verdict for {path}@{mode}");
}

/// Repository backing the matcher rows: one regular file, one
/// executable, one symlink.
fn match_repo(twins: &Twins) -> PathBuf {
    let repo = twins.repo("match");
    git_init(&repo);
    stage(&repo, "plain.txt", b"plain\n");
    stage(&repo, "tool.sh", b"#!/bin/sh\ntrue\n");
    set_mode(&repo.join("tool.sh"), 0o755);
    std::os::unix::fs::symlink("plain.txt", repo.join("ptr")).expect("repo link");
    commit_all(&repo, "match");
    repo
}

#[test]
fn matches_regular_matrix() {
    let twins = Twins::build("match-regular");
    let repo = match_repo(&twins);
    // Live homes start from the same bytes; the executable bit is
    // set per row below.
    mirror(&twins, "plain.txt", b"plain\n", 0o644);
    mirror(&twins, "tool.sh", b"#!/bin/sh\ntrue\n", 0o755);
    check_matches(&repo, &twins, "main", "100644", "plain.txt", 0);
    check_matches(&repo, &twins, "main", "100755", "tool.sh", 0);
    // Content drift breaks the match.
    mirror(&twins, "plain.txt", b"changed\n", 0o644);
    check_matches(&repo, &twins, "main", "100644", "plain.txt", 1);
    // Executable-bit drift breaks the match in both directions.
    mirror(&twins, "plain.txt", b"plain\n", 0o644);
    set_mode(&twins.shell_home.join("plain.txt"), 0o755);
    set_mode(&twins.rust_home.join("plain.txt"), 0o755);
    check_matches(&repo, &twins, "main", "100644", "plain.txt", 1);
    set_mode(&twins.shell_home.join("tool.sh"), 0o644);
    set_mode(&twins.rust_home.join("tool.sh"), 0o644);
    check_matches(&repo, &twins, "main", "100755", "tool.sh", 1);
    // Absent paths never match.
    check_matches(&repo, &twins, "main", "100644", "missing.txt", 1);
}

#[test]
fn matches_symlink_matrix() {
    let twins = Twins::build("match-link");
    let repo = match_repo(&twins);
    mirror(&twins, "plain.txt", b"plain\n", 0o644);
    mirror_link(&twins, "ptr", "plain.txt");
    check_matches(&repo, &twins, "main", "120000", "ptr", 0);
    // A retargeted link carries different bytes.
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::remove_file(home.join("ptr")).expect("remove link");
        std::os::unix::fs::symlink("other", home.join("ptr")).expect("retarget");
    }
    check_matches(&repo, &twins, "main", "120000", "ptr", 1);
    // A regular file is not a link match even with equal bytes.
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::remove_file(home.join("ptr")).expect("remove link");
        std::fs::write(home.join("ptr"), b"plain.txt").expect("plain file");
    }
    check_matches(&repo, &twins, "main", "120000", "ptr", 1);
    // A mode-120000 probe of a regular candidate also fails.
    check_matches(&repo, &twins, "main", "120000", "plain.txt", 1);
    // Unknown modes never match.
    check_matches(&repo, &twins, "main", "100600", "plain.txt", 1);
}

/// Shell probe reporting `code=` plus the frozen `out=` line for
/// `_dot_init_snapshot_path`.
fn snapshot_snippet(path: &Path) -> String {
    format!(
        "out=$(_dot_init_snapshot_path {}); code=$?; printf 'code=%s\\nout=%s\\n' \"$code\" \"$out\"\n",
        sq(&path.to_string_lossy()),
    )
}

/// One snapshot row across both engines: verdicts agree and, on
/// success, the frozen lines agree up to the live identities.
fn check_snapshot(twins: &Twins, rel: &str, want: i32) {
    let shell_path = twins.shell_home.join(rel);
    let rust_path = twins.rust_home.join(rel);
    let (shell_code, shell_out, _) =
        shell_run(&twins.shell_home, &[], &snapshot_snippet(&shell_path));
    let rust_result = candidate::snapshot_path(&rust_path);
    let rust_code = if rust_result.is_ok() { 0 } else { 1 };
    assert_eq!(shell_code, want, "shell snapshot verdict for {rel}");
    assert_eq!(rust_code, want, "rust snapshot verdict for {rel}");
    if want == 0 {
        let shell_line = String::from_utf8_lossy(&shell_out)
            .lines()
            .find_map(|line| line.strip_prefix("out=").map(str::to_string))
            .expect("shell out line");
        let rust_line = rust_result.expect("rust snapshot line");
        assert_eq!(
            normalize_row(&shell_line),
            normalize_row(&rust_line),
            "snapshot lines agree for {rel}"
        );
    }
}

#[test]
fn snapshot_shapes() {
    let twins = Twins::build("snapshot-shapes");
    mirror(&twins, "file.txt", b"content\n", 0o644);
    mirror(&twins, "exec.sh", b"#!/bin/sh\n", 0o755);
    mirror_link(&twins, "ptr", "file.txt");
    std::fs::create_dir_all(twins.shell_home.join("sub")).expect("shell dir");
    std::fs::create_dir_all(twins.rust_home.join("sub")).expect("rust dir");
    for rel in ["file.txt", "exec.sh", "ptr", "sub", "missing"] {
        check_snapshot(&twins, rel, 0);
    }
    // The mode travels in the frozen line: 644 stays 644, 755 stays
    // 755, links freeze 777 over the target-name length.
    let line = candidate::snapshot_path(&twins.rust_home.join("exec.sh")).expect("exec line");
    assert!(
        line.starts_with("regular\t") && line.contains("\t755\t"),
        "exec mode frozen: {line}"
    );
    let line = candidate::snapshot_path(&twins.rust_home.join("ptr")).expect("link line");
    assert!(
        line.starts_with("symlink\t") && line.ends_with("\t777\t8\tfile.txt"),
        "link lstat shape frozen: {line}"
    );
}

#[test]
fn snapshot_rejects_specials() {
    let twins = Twins::build("snapshot-specials");
    for home in [&twins.shell_home, &twins.rust_home] {
        let fifo = home.join("pipe");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo {}", fifo.display());
    }
    // Links whose target carries a newline can never be journaled.
    std::os::unix::fs::symlink("a\nb", twins.shell_home.join("badlink")).expect("bad link");
    std::os::unix::fs::symlink("a\nb", twins.rust_home.join("badlink")).expect("bad link");
    check_snapshot(&twins, "pipe", 1);
    check_snapshot(&twins, "badlink", 1);
}

#[test]
fn snapshot_dangling_and_chomped_links() {
    let twins = Twins::build("snapshot-links");
    // Dangling links freeze like live ones: every probe lstates, so
    // no target is ever followed.
    mirror_link(&twins, "dangle", "nowhere");
    check_snapshot(&twins, "dangle", 0);
    let line = candidate::snapshot_path(&twins.rust_home.join("dangle")).expect("dangle line");
    assert!(
        line.ends_with("\tnowhere"),
        "dangling target frozen: {line}"
    );
    // A trailing newline in the target chomps before the safety
    // gate, exactly like the shell's `$(readlink)`.
    for home in [&twins.shell_home, &twins.rust_home] {
        let target = home.join("chomped-target");
        std::fs::write(&target, b"x").expect("target file");
        let link = home.join("chomped");
        let link_arg = link.to_string_lossy().into_owned();
        let status = Command::new("ln")
            .arg("-s")
            .arg("chomped-target\n")
            .arg(&link_arg)
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn ln");
        assert!(status.success(), "ln {}", link.display());
    }
    check_snapshot(&twins, "chomped", 0);
    let line = candidate::snapshot_path(&twins.rust_home.join("chomped")).expect("chomped line");
    assert!(
        line.ends_with("\tchomped-target"),
        "trailing newline chomped: {line}"
    );
}

/// Shell probe ending in the `code=` verdict for
/// `_dot_init_path_state_matches`.
fn state_snippet(path: &Path, fields: &[&str]) -> String {
    let quoted: Vec<String> = fields.iter().map(|field| sq(field)).collect();
    format!(
        "if _dot_init_path_state_matches {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&path.to_string_lossy()),
        quoted.join(" "),
    )
}

/// Split a frozen six-field line for the recheck probe.
fn split_snapshot(line: &str) -> Vec<String> {
    line.split('\t').map(str::to_string).collect()
}

/// One recheck row: freeze `rel` on both engines, then replay the
/// frozen fields (optionally mutated) against the live path.
fn check_state(
    twins: &Twins,
    rel: &str,
    disturb: fn(&Twins),
    mutate: fn(&mut Vec<String>),
    want: i32,
) {
    let shell_path = twins.shell_home.join(rel);
    let rust_path = twins.rust_home.join(rel);
    let shell_line = {
        let (_, shell_out, _) = shell_run(&twins.shell_home, &[], &snapshot_snippet(&shell_path));
        String::from_utf8_lossy(&shell_out)
            .lines()
            .find_map(|line| line.strip_prefix("out=").map(str::to_string))
            .expect("shell out line")
    };
    let rust_line = candidate::snapshot_path(&rust_path).expect("rust snapshot line");
    assert_eq!(
        normalize_row(&shell_line),
        normalize_row(&rust_line),
        "frozen lines agree for {rel}"
    );
    disturb(twins);
    // Each engine replays its OWN frozen line: the dev:ino fields
    // are live per home, so crossing them would fail every row.
    // The same mutation applies to both field sets.
    let mut shell_fields = split_snapshot(&shell_line);
    mutate(&mut shell_fields);
    let shell_refs: Vec<&str> = shell_fields.iter().map(String::as_str).collect();
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &state_snippet(&shell_path, &shell_refs),
    );
    let mut rust_fields = split_snapshot(&rust_line);
    mutate(&mut rust_fields);
    let rust_code = if candidate::path_state_matches(
        &rust_path,
        &rust_fields[0],
        &rust_fields[1],
        &rust_fields[2],
        &rust_fields[3],
        &rust_fields[4],
        &rust_fields[5],
    ) {
        0
    } else {
        1
    };
    assert_eq!(shell_code, want, "shell recheck verdict for {rel}");
    assert_eq!(rust_code, want, "rust recheck verdict for {rel}");
}

/// Identity mutation: keep every frozen field.
fn keep(fields: &mut Vec<String>) {
    let _ = fields;
}

/// No live disturbance between freeze and replay.
fn calm(_twins: &Twins) {}

/// Disturbances for the roundtrip rows, each mirrored into both
/// twin homes.
fn disturb_chmod(twins: &Twins) {
    set_mode(&twins.shell_home.join("file.txt"), 0o600);
    set_mode(&twins.rust_home.join("file.txt"), 0o600);
}

fn disturb_append(twins: &Twins) {
    mirror(twins, "file.txt", b"content\nmore\n", 0o644);
}

fn disturb_retarget(twins: &Twins) {
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::remove_file(home.join("ptr")).expect("remove link");
        std::os::unix::fs::symlink("file.txt!", home.join("ptr")).expect("retarget");
    }
}

fn disturb_remove(twins: &Twins) {
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::remove_file(home.join("file.txt")).expect("remove file");
    }
}

fn disturb_file_over_dir(twins: &Twins) {
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::remove_dir(home.join("sub")).expect("remove dir");
        std::fs::write(home.join("sub"), b"x").expect("file over dir");
    }
}

#[test]
fn state_roundtrip() {
    // Each drift row gets fresh homes: the disturbance persists, so
    // the freeze must see the pre-drift state exactly once.
    type Row = (&'static str, &'static str, fn(&Twins), i32);
    let rows: [Row; 8] = [
        ("calm-file", "file.txt", calm, 0),
        ("calm-link", "ptr", calm, 0),
        ("calm-dir", "sub", calm, 0),
        ("chmod", "file.txt", disturb_chmod, 1),
        ("append", "file.txt", disturb_append, 1),
        ("retarget", "ptr", disturb_retarget, 1),
        ("remove", "file.txt", disturb_remove, 1),
        ("file-over-dir", "sub", disturb_file_over_dir, 1),
    ];
    for (tag, rel, disturb, want) in rows {
        let twins = Twins::build(tag);
        mirror(&twins, "file.txt", b"content\n", 0o644);
        mirror_link(&twins, "ptr", "file.txt");
        std::fs::create_dir_all(twins.shell_home.join("sub")).expect("shell dir");
        std::fs::create_dir_all(twins.rust_home.join("sub")).expect("rust dir");
        check_state(&twins, rel, disturb, keep, want);
    }
}

#[test]
fn state_absent_and_tampered() {
    let twins = Twins::build("state-tampered");
    // Absence rechecks against an absent-shaped line.
    check_state(&twins, "missing", calm, keep, 0);
    // A live path never matches an absent line.
    mirror(&twins, "live.txt", b"live\n", 0o644);
    check_state(
        &twins,
        "live.txt",
        calm,
        |fields| {
            fields[0] = "absent".to_string();
            fields[1] = "-".to_string();
            fields[2] = "-".to_string();
            fields[3] = "-".to_string();
            fields[4] = "-".to_string();
            fields[5] = "-".to_string();
        },
        1,
    );
    // Unknown kinds never match.
    check_state(
        &twins,
        "live.txt",
        calm,
        |fields| {
            fields[0] = "fifo".to_string();
        },
        1,
    );
    // A tampered identity never matches.
    check_state(
        &twins,
        "live.txt",
        calm,
        |fields| {
            fields[1] = "0".to_string();
        },
        1,
    );
    // A tampered mode spelling never matches, even for the same
    // bits: the shell compares the rendered string.
    check_state(
        &twins,
        "live.txt",
        calm,
        |fields| {
            fields[3] = format!("0{}", fields[3]);
        },
        1,
    );
}

/// Shell probe reporting `code=` plus the `root=` answer for
/// `_dot_init_conflict_root`.
fn root_snippet(path: &str) -> String {
    format!(
        "root=$(_dot_init_conflict_root {}); code=$?; printf 'code=%s\\nroot=%s\\n' \"$code\" \"$root\"\n",
        sq(path),
    )
}

/// One conflict-root row across both engines.
fn check_root(twins: &Twins, rel: &str, want: &str) {
    let (shell_code, shell_out, _) = shell_run(&twins.shell_home, &[], &root_snippet(rel));
    assert_eq!(shell_code, 0, "shell root verdict for {rel}");
    let shell_root = String::from_utf8_lossy(&shell_out)
        .lines()
        .find_map(|line| line.strip_prefix("root=").map(str::to_string))
        .expect("shell root line");
    let rust_root = candidate::conflict_root(rel, &twins.rust_home.to_string_lossy());
    assert_eq!(shell_root, want, "shell root answer for {rel}");
    assert_eq!(rust_root, want, "rust root answer for {rel}");
}

#[test]
fn conflict_root_shapes() {
    let twins = Twins::build("conflict-roots");
    // A clear route resolves to the path itself.
    check_root(&twins, "a/b/c", "a/b/c");
    // A live file on the route is the root.
    mirror(&twins, "block", b"x", 0o644);
    check_root(&twins, "block/leaf", "block");
    // The deepest blocker wins over a shallower one.
    mirror(&twins, "outer", b"x", 0o644);
    std::fs::create_dir_all(twins.shell_home.join("outer-dir")).expect("shell dir");
    std::fs::create_dir_all(twins.rust_home.join("outer-dir")).expect("rust dir");
    mirror(&twins, "outer-dir/inner", b"x", 0o644);
    check_root(&twins, "outer-dir/inner/leaf", "outer-dir/inner");
    // A symlink-to-directory still blocks: it is not a real
    // directory.
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::create_dir_all(home.join("realdir")).expect("real dir");
        std::os::unix::fs::symlink("realdir", home.join("linkdir")).expect("dir link");
    }
    check_root(&twins, "linkdir/leaf", "linkdir");
    // Real directories on the route are transparent.
    check_root(&twins, "realdir/leaf", "realdir/leaf");
    // A blocked grandparent past a clear parent is the root.
    mirror(&twins, "grand", b"x", 0o644);
    check_root(&twins, "grand/mid/leaf", "grand");
}

/// Shell probe ending in the `code=` verdict for
/// `_dot_init_build_prior_and_conflicts`.
fn plan_snippet(repo: &Path, branch: &str, tree: &Path, prior: &Path, conflicts: &Path) -> String {
    format!(
        "if _dot_init_build_prior_and_conflicts {} {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&repo.to_string_lossy()),
        sq(branch),
        sq(&tree.to_string_lossy()),
        sq(&prior.to_string_lossy()),
        sq(&conflicts.to_string_lossy()),
    )
}

/// Repository backing the planner rows: one file that stays put,
/// one that drifts, one that never lands, and two sharing a live
/// file as their conflict root.
fn plan_repo(twins: &Twins) -> PathBuf {
    let repo = twins.repo("plan");
    git_init(&repo);
    stage(&repo, "same.txt", b"same\n");
    stage(&repo, "drift.txt", b"candidate\n");
    stage(&repo, "ghost.txt", b"ghost\n");
    stage(&repo, "capped/one.txt", b"one\n");
    stage(&repo, "capped/two.txt", b"two\n");
    commit_all(&repo, "plan");
    repo
}

/// Emit the candidate tree for `branch` into both homes' tree files
/// and return the shared inventory bytes.
fn plan_trees(repo: &Path, twins: &Twins, branch: &str) -> Vec<u8> {
    let shell_tree = twins.shell_home.join("tree.tsv");
    let rust_tree = twins.rust_home.join("tree.tsv");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &tree_snippet(repo, branch, &shell_tree),
    );
    assert_eq!(shell_code, 0, "shell plans from a valid tree");
    let scope = scope_for(&twins.rust_home);
    candidate::candidate_tree(repo, branch, &rust_tree, &scope).expect("rust tree");
    let shell_bytes = std::fs::read(&shell_tree).expect("read shell tree");
    let rust_bytes = std::fs::read(&rust_tree).expect("read rust tree");
    assert_eq!(shell_bytes, rust_bytes, "shared inventory");
    shell_bytes
}

/// One planner row across both engines: verdicts agree, the prior
/// journals agree up to live identities, the conflict roots agree
/// exactly, and successful journals land at mode 600.
fn check_plan(
    repo: &Path,
    twins: &Twins,
    branch: &str,
    tree_bytes: &[u8],
    want: i32,
    want_conflicts: &[&str],
) {
    let shell_tree = twins.shell_home.join("tree.tsv");
    let rust_tree = twins.rust_home.join("tree.tsv");
    std::fs::write(&shell_tree, tree_bytes).expect("stage shell tree");
    std::fs::write(&rust_tree, tree_bytes).expect("stage rust tree");
    let shell_prior = twins.shell_home.join("prior.tsv");
    let shell_conflicts = twins.shell_home.join("conflicts.tsv");
    let rust_prior = twins.rust_home.join("prior.tsv");
    let rust_conflicts = twins.rust_home.join("conflicts.tsv");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &plan_snippet(repo, branch, &shell_tree, &shell_prior, &shell_conflicts),
    );
    let scope = scope_for(&twins.rust_home);
    let rust_code = match candidate::build_prior_and_conflicts(
        repo,
        branch,
        &rust_tree,
        &rust_prior,
        &rust_conflicts,
        &scope,
    ) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    assert_eq!(shell_code, want, "shell plan verdict");
    assert_eq!(rust_code, want, "rust plan verdict");
    assert_eq!(
        journal_rows(&shell_prior),
        journal_rows(&rust_prior),
        "prior journals agree"
    );
    let shell_conflict_roots: Vec<String> = journal_rows(&shell_conflicts)
        .iter()
        .map(|line| {
            line.split('\t')
                .next()
                .expect("conflict root field")
                .to_string()
        })
        .collect();
    let rust_conflict_roots: Vec<String> = journal_rows(&rust_conflicts)
        .iter()
        .map(|line| {
            line.split('\t')
                .next()
                .expect("conflict root field")
                .to_string()
        })
        .collect();
    assert_eq!(
        shell_conflict_roots, rust_conflict_roots,
        "conflict roots agree"
    );
    assert_eq!(
        shell_conflict_roots, want_conflicts,
        "expected conflict roots"
    );
    if want == 0 {
        for journal in [&shell_prior, &shell_conflicts, &rust_prior, &rust_conflicts] {
            assert_eq!(
                mode_of(journal),
                0o600,
                "journal mode for {}",
                journal.display()
            );
        }
    }
}

#[test]
fn plan_clean_match() {
    let twins = Twins::build("plan-clean");
    let repo = plan_repo(&twins);
    // Every candidate already lives at the wanted generation: the
    // prior records them all and no conflict row appears.
    mirror(&twins, "same.txt", b"same\n", 0o644);
    mirror(&twins, "drift.txt", b"candidate\n", 0o644);
    let tree_bytes = plan_trees(&repo, &twins, "main");
    // Build a two-row inventory (same + drift, both matching) for
    // the clean shape.
    let mut clean: Vec<u8> = Vec::new();
    for line in tree_bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let path = text.split('\t').nth(2).unwrap_or("");
        if path == "same.txt" || path == "drift.txt" {
            clean.extend_from_slice(line);
            clean.push(b'\n');
        }
    }
    check_plan(&repo, &twins, "main", &clean, 0, &[]);
    let shell_prior = journal_rows(&twins.shell_home.join("prior.tsv"));
    assert_eq!(shell_prior.len(), 2, "two prior rows");
}

#[test]
fn plan_conflicts_and_skips() {
    let twins = Twins::build("plan-conflicts");
    let repo = plan_repo(&twins);
    let tree_bytes = plan_trees(&repo, &twins, "main");
    // same.txt matches (prior only); drift.txt differs (conflict at
    // its own name); ghost.txt is absent at the top level (skipped:
    // an absent path is its own root); capped/* sit under a live
    // file, so both collapse onto the single `capped` root.
    mirror(&twins, "same.txt", b"same\n", 0o644);
    mirror(&twins, "drift.txt", b"stale\n", 0o644);
    mirror(&twins, "capped", b"i am a file\n", 0o644);
    // Tree order rules: `capped/*` sorts before `drift.txt`, so
    // the shared blocker lands first on both engines.
    check_plan(
        &repo,
        &twins,
        "main",
        &tree_bytes,
        0,
        &["capped", "drift.txt"],
    );
    let shell_prior = journal_rows(&twins.shell_home.join("prior.tsv"));
    assert_eq!(shell_prior.len(), 5, "every candidate lands in prior");
}

#[test]
fn plan_missing_tree_plans_empty() {
    let twins = Twins::build("plan-missing");
    let repo = plan_repo(&twins);
    // Prefill both journals with junk: the truncation still runs
    // first, like the shell's opening `: >` pair - and a missing
    // tree then plans zero rows successfully, because the shell's
    // failed tree redirect only skips the loop while the trailing
    // chmod succeeds.
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::write(home.join("prior.tsv"), b"junk\n").expect("prefill prior");
        std::fs::write(home.join("conflicts.tsv"), b"junk\n").expect("prefill conflicts");
    }
    let shell_prior = twins.shell_home.join("prior.tsv");
    let shell_conflicts = twins.shell_home.join("conflicts.tsv");
    let rust_prior = twins.rust_home.join("prior.tsv");
    let rust_conflicts = twins.rust_home.join("conflicts.tsv");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &plan_snippet(
            &repo,
            "main",
            &twins.shell_home.join("tree.tsv"),
            &shell_prior,
            &shell_conflicts,
        ),
    );
    let scope = scope_for(&twins.rust_home);
    let rust_code = match candidate::build_prior_and_conflicts(
        &repo,
        "main",
        &twins.rust_home.join("tree.tsv"),
        &rust_prior,
        &rust_conflicts,
        &scope,
    ) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    assert_eq!(shell_code, 0, "shell plans empty without a tree");
    assert_eq!(rust_code, 0, "rust plans empty without a tree");
    for journal in [&shell_prior, &shell_conflicts, &rust_prior, &rust_conflicts] {
        assert_eq!(
            std::fs::read(journal).expect("read journal"),
            b"",
            "truncated journal at {}",
            journal.display()
        );
        assert_eq!(
            mode_of(journal),
            0o600,
            "journal mode for {}",
            journal.display()
        );
    }
}

#[test]
fn trailing_slash_home() {
    // A `HOME` with a trailing slash keeps its doubled separator in
    // `$HOME/$path` on both engines; probes and answers still agree.
    let twins = Twins::build("trailing-slash");
    let repo = match_repo(&twins);
    mirror(&twins, "plain.txt", b"plain\n", 0o644);
    let shell_home = format!("{}/", twins.shell_home.to_string_lossy());
    let rust_home = format!("{}/", twins.rust_home.to_string_lossy());
    let (shell_code, _, _) = shell_run(
        Path::new(&shell_home),
        &[],
        &matches_snippet(&repo, "main", "100644", "plain.txt"),
    );
    let mut scope = scope_for(&twins.rust_home);
    scope.home = rust_home;
    let rust_code =
        if candidate::candidate_matches_path(&repo, "main", "100644", "plain.txt", &scope) {
            0
        } else {
            1
        };
    assert_eq!(shell_code, 0, "shell matches under slashed home");
    assert_eq!(rust_code, 0, "rust matches under slashed home");
    let (root_code, root_out, _) =
        shell_run(Path::new(&shell_home), &[], &root_snippet("plain.txt"));
    assert_eq!(root_code, 0, "shell root verdict");
    let shell_root = String::from_utf8_lossy(&root_out)
        .lines()
        .find_map(|line| line.strip_prefix("root=").map(str::to_string))
        .expect("shell root line");
    assert_eq!(shell_root, "plain.txt");
    assert_eq!(
        candidate::conflict_root("plain.txt", &scope.home),
        "plain.txt"
    );
}
