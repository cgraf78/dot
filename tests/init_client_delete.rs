//! Differential parity tests for the init deletion-parking family
//! (`lib/dot/init-client.sh`) against the live shell: the
//! same-parent park path, the worktree-content match gate, the three
//! per-kind delete validators, the two private-directory gates, and
//! the parked-generation remover.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so parks,
//! markers, and git stores never collide.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_delete as delete;
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the init deletion chapter: the resource runtime, the
/// shared temp helpers (path identity, exclusive moves), the XDG
/// root, and the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixed run identity for the deletion rows.
const NONCE: &str = "test-nonce-55";
const IDENTITY: &str = "github.com/example/dot";
const BRANCH: &str = "main";

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

/// Twin homes: disjoint directories so parks and git stores never
/// collide across engines.
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

/// Write `bytes` to `dir/name`, creating parents.
fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Run git for fixtures; asserts success, silences output.
fn git(args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

/// Run git for fixtures and capture chomped stdout.
fn git_out(args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// `dev:ino` identity text for one path, via the port's own stat
/// helper (both engines must agree on the value).
fn identity_of(path: &Path) -> String {
    dot::temp::identity_string(dot::temp::path_identity(path).expect("fixture identity"))
}

/// One verdict probe: the shell snippet must end by setting `code`.
fn probe(home: &Path, env: &[(&str, &str)], body: &str) -> i32 {
    let (code, _, _) = shell_run(
        home,
        env,
        &format!("{body}\nprintf 'code=%s\\n' \"$code\"\n"),
    );
    code
}

/// A tracked-content fixture repository (bare) plus the worktree
/// paths it was built from: one regular file, one executable, and
/// one symlink.
struct ContentRepo {
    dir: PathBuf,
    commit: String,
    file_oid: String,
    exec_oid: String,
    link_oid: String,
}

/// Build the content repo under `root/repo.git` from
/// `root/work`, and mirror the three worktree entries under both
/// twin homes.
fn build_content_repo(twins: &Twins) -> ContentRepo {
    let root = twins.root();
    let repo = root.join("repo.git");
    let work = root.join("work");
    git(&[
        "init",
        "--quiet",
        "--bare",
        "--initial-branch",
        BRANCH,
        repo.to_str().expect("repo path"),
    ]);
    std::fs::create_dir_all(&work).expect("work dir");
    write(&work, "file1", b"hello worktree\n");
    write(&work, "runme", b"#!/bin/sh\necho hi\n");
    chmod(&work.join("runme"), 0o755);
    std::os::unix::fs::symlink("mytarget", work.join("link1")).expect("fixture link");
    let work_text = work.to_str().expect("work path").to_string();
    let repo_text = repo.to_str().expect("repo path").to_string();
    git(&[
        "--git-dir",
        &repo_text,
        "--work-tree",
        &work_text,
        "add",
        "-A",
    ]);
    git(&[
        "--git-dir",
        &repo_text,
        "--work-tree",
        &work_text,
        "commit",
        "--quiet",
        "-m",
        "init",
    ]);
    let commit = git_out(&["--git-dir", &repo_text, "rev-parse", "HEAD"]);
    let tree = git_out(&["--git-dir", &repo_text, "ls-tree", "HEAD"]);
    let mut file_oid = String::new();
    let mut exec_oid = String::new();
    let mut link_oid = String::new();
    for line in tree.lines() {
        let Some((header, name)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = header.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let _kind = fields.next().unwrap_or_default();
        let oid = fields.next().unwrap_or_default();
        match (mode, name) {
            ("100644", "file1") => file_oid = oid.to_string(),
            ("100755", "runme") => exec_oid = oid.to_string(),
            ("120000", "link1") => link_oid = oid.to_string(),
            _ => {}
        }
    }
    assert!(!commit.is_empty(), "fixture commit");
    assert!(!file_oid.is_empty(), "fixture file oid");
    assert!(!exec_oid.is_empty(), "fixture exec oid");
    assert!(!link_oid.is_empty(), "fixture link oid");
    for home in [&twins.shell_home, &twins.rust_home] {
        write(home, "file1", b"hello worktree\n");
        write(home, "runme", b"#!/bin/sh\necho hi\n");
        chmod(&home.join("runme"), 0o755);
        std::os::unix::fs::symlink("mytarget", home.join("link1")).expect("home link");
    }
    ContentRepo {
        dir: repo,
        commit,
        file_oid,
        exec_oid,
        link_oid,
    }
}

/// The quirk oid: `git hash-object --stdin` over `"<target>\n"`,
/// computed with the git CLI as the independent oracle for what the
/// shell's `$(readlink; printf .)` trick feeds the hasher.
fn quirk_oid(target: &str) -> String {
    let mut link = target.as_bytes().to_vec();
    link.push(b'\n');
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git hash-object");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("hasher stdin")
        .write_all(&link)
        .expect("feed hasher");
    let output = child.wait_with_output().expect("reap hasher");
    assert!(output.status.success(), "hash quirk bytes");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// Lexical existence plus content: the observable end-state of a
/// removal row on one side.
fn shape(path: &Path) -> (bool, Vec<u8>) {
    let exists = std::fs::symlink_metadata(path).is_ok();
    let content = std::fs::read(path).unwrap_or_default();
    (exists, content)
}

#[test]
fn park_path_parity() {
    let twins = Twins::build("init-delete-park");
    let env = [("DOT_INIT_NONCE", NONCE)];
    let row = |target: &str, kind: &str, key: &str| {
        let body = format!(
            "if _dot_init_delete_park_path {} {} {}; then code=0; park=$REPLY; else code=$?; park=; fi\nprintf 'code=%s\\npark=%s\\n' \"$code\" \"$park\"\n",
            sq(target),
            sq(kind),
            sq(key)
        );
        let (shell_code, shell_out, _) = shell_run(&twins.shell_home, &env, &body);
        let shell_park = String::from_utf8_lossy(&shell_out)
            .lines()
            .find_map(|line| line.strip_prefix("park=").map(str::to_string))
            .unwrap_or_default();
        match delete::delete_park_path(Path::new(target), kind, key, NONCE) {
            Ok(park) => {
                assert_eq!(shell_code, 0, "park {target} {kind}");
                assert_eq!(shell_park, park.to_str().expect("park text"));
            }
            Err(_) => {
                assert_ne!(shell_code, 0, "park {target} {kind} must fail");
                assert_eq!(shell_park, "", "failed park prints nothing");
            }
        }
    };
    row("sub/dir/file", "leaf", "sub/dir/file");
    row("sub/dir/file", "parent", "sub/dir");
    row("repo.git", "git", "repo.git");
    row("sub/dir/file", "bogus", "sub/dir/file");
    row("sub/dir/file", "", "sub/dir/file");
    row("lone", "leaf", "lone");
    row("/x", "leaf", "x");
    row("sub/", "leaf", "sub");
    row("a//b", "parent", "a/b");
    row("sub/f", "leaf", "a b");
    row("sub/f", "leaf", "a\tb");
    row("sub/f", "leaf", "");
    row("it's/x", "leaf", "it's/x");
}

#[test]
fn candidate_content_parity() {
    let twins = Twins::build("init-delete-candidate");
    let repo = build_content_repo(&twins);
    let repo_text = repo.dir.to_str().expect("repo path").to_string();
    let quirks = quirk_oid("mytarget");
    let dangling_quirk = quirk_oid("nowhere");
    std::os::unix::fs::symlink("nowhere", twins.shell_home.join("tlink")).expect("shell dangling");
    std::os::unix::fs::symlink("nowhere", twins.rust_home.join("tlink")).expect("rust dangling");
    std::fs::create_dir_all(twins.shell_home.join("subdir")).expect("shell subdir");
    std::fs::create_dir_all(twins.rust_home.join("subdir")).expect("rust subdir");
    let row = |mode: &str, oid: &str, rel: &str| {
        let body = format!(
            "if _dot_init_candidate_matches_git {} {} {} {} {}; then code=0; else code=$?; fi",
            sq(&repo_text),
            sq(&repo.commit),
            sq(mode),
            sq(oid),
            sq(rel)
        );
        let shell_code = probe(&twins.shell_home, &[], &body);
        let rust = delete::candidate_matches_git(
            &repo.dir,
            &repo.commit,
            mode,
            oid,
            rel,
            &twins.rust_home,
        );
        assert_eq!(
            shell_code,
            if rust { 0 } else { 1 },
            "candidate mode={mode} rel={rel}"
        );
    };
    row("100644", &repo.file_oid, "file1");
    row("100644", &repo.exec_oid, "file1");
    row("100755", &repo.exec_oid, "runme");
    row("100644", &repo.exec_oid, "runme");
    row("100755", &repo.file_oid, "file1");
    // The live shell hashes `readlink` bytes plus the preserved
    // trailing newline, so a perfect symlink never matches its tree
    // blob — but does match the newline-carrying hash.
    row("120000", &repo.link_oid, "link1");
    row("120000", &quirks, "link1");
    row("120000", &dangling_quirk, "tlink");
    row("120000", &repo.link_oid, "tlink");
    row("100644", &repo.file_oid, "link1");
    row("120000", &repo.link_oid, "file1");
    row("100644", &repo.file_oid, "missing");
    row("100644", &repo.file_oid, "subdir");
    row("100666", &repo.file_oid, "file1");
    row("", &repo.file_oid, "file1");
    // Stale worktree content stops matching on both engines. Last:
    // it rewrites the shared fixtures.
    write(&twins.shell_home, "file1", b"changed\n");
    write(&twins.rust_home, "file1", b"changed\n");
    row("100644", &repo.file_oid, "file1");
}

#[test]
fn leaf_delete_parity() {
    let twins = Twins::build("init-delete-leaf");
    let repo = build_content_repo(&twins);
    let repo_text = repo.dir.to_str().expect("repo path").to_string();
    write(twins.root(), "outside", b"hello worktree\n");
    // The twins are distinct inodes by construction, so `live`
    // resolves to each side's own identity; literals go to both
    // sides verbatim.
    let row =
        |candidate_shell: &Path, candidate_rust: &Path, identity: &str, mode: &str, oid: &str| {
            let shell_id = if identity == "live" {
                identity_of(candidate_shell)
            } else {
                identity.to_string()
            };
            let rust_id = if identity == "live" {
                identity_of(candidate_rust)
            } else {
                identity.to_string()
            };
            let body = format!(
                "if _dot_init_leaf_delete_matches {} {} {} {} {} {}; then code=0; else code=$?; fi",
                sq(candidate_shell.to_str().expect("candidate path")),
                sq(&shell_id),
                sq(&repo_text),
                sq(&repo.commit),
                sq(mode),
                sq(oid)
            );
            let shell_code = probe(&twins.shell_home, &[], &body);
            let rust = delete::leaf_delete_matches(
                candidate_rust,
                &rust_id,
                &repo.dir,
                &repo.commit,
                mode,
                oid,
                &twins.rust_home,
            );
            assert_eq!(
                shell_code,
                if rust { 0 } else { 1 },
                "leaf {}",
                candidate_shell.display()
            );
        };
    let shell_file = twins.shell_home.join("file1");
    let rust_file = twins.rust_home.join("file1");
    row(&shell_file, &rust_file, "live", "100644", &repo.file_oid);
    row(&shell_file, &rust_file, "0:0", "100644", &repo.file_oid);
    row(&shell_file, &rust_file, "live", "100644", &repo.exec_oid);
    row(&shell_file, &rust_file, "", "100644", &repo.file_oid);
    // Outside the home root the prefix gate fails even with the
    // right identity and matching content.
    let outside = twins.root().join("outside");
    row(&outside, &outside, "live", "100644", &repo.file_oid);
    row(&outside, &outside, "0:0", "100644", &repo.file_oid);
    // Missing candidates fail on both engines, including against an
    // empty expectation (the failed stat counts as empty, but the
    // content gate still fails).
    let shell_missing = twins.shell_home.join("gone");
    let rust_missing = twins.rust_home.join("gone");
    row(
        &shell_missing,
        &rust_missing,
        "0:0",
        "100644",
        &repo.file_oid,
    );
    row(&shell_missing, &rust_missing, "", "100644", &repo.file_oid);
    // The symlink quirk flows through the leaf validator. The home
    // links dangle (like real staged links before publication), so
    // the followed identity stat fails on both engines and only
    // literal expectations are observable here.
    let shell_link = twins.shell_home.join("link1");
    let rust_link = twins.rust_home.join("link1");
    row(&shell_link, &rust_link, "0:0", "120000", &repo.link_oid);
    row(&shell_link, &rust_link, "", "120000", &repo.link_oid);
}

#[test]
fn private_directory_parity() {
    let twins = Twins::build("init-delete-privdir");
    for (home, tag) in [(&twins.shell_home, "sh"), (&twins.rust_home, "rs")] {
        for name in ["locked", "open", "plain-file", "empty600"] {
            let path = home.join(format!("{tag}-{name}"));
            if name == "plain-file" {
                write(home, &format!("{tag}-{name}"), b"x\n");
            } else {
                std::fs::create_dir_all(&path).expect("fixture dir");
            }
        }
        chmod(&home.join(format!("{tag}-open")), 0o755);
        chmod(&home.join(format!("{tag}-empty600")), 0o600);
        std::os::unix::fs::symlink(format!("{tag}-locked"), home.join(format!("{tag}-linkdir")))
            .expect("fixture dir link");
    }
    let row = |name: &str, identity: Option<&str>, mode: Option<&str>| {
        let shell_path = twins.shell_home.join(format!("sh-{name}"));
        let rust_path = twins.rust_home.join(format!("rs-{name}"));
        let shell_id = identity.as_ref().map(|id| {
            if *id == "live" {
                identity_of(&shell_path)
            } else {
                (*id).to_string()
            }
        });
        let rust_id = identity.as_ref().map(|id| {
            if *id == "live" {
                identity_of(&rust_path)
            } else {
                (*id).to_string()
            }
        });
        let mut body = format!(
            "if _dot_init_private_directory_matches {}",
            sq(shell_path.to_str().expect("shell path"))
        );
        if let Some(id) = &shell_id {
            body.push(' ');
            body.push_str(&sq(id));
            body.push(' ');
            body.push_str(&sq(mode.unwrap_or("")));
        } else if mode.is_some() {
            body.push_str(&format!(" '' {}", sq(mode.unwrap_or(""))));
        }
        body.push_str("; then code=0; else code=$?; fi");
        let shell_code = probe(&twins.shell_home, &[], &body);
        let rust = delete::private_directory_matches(&rust_path, rust_id.as_deref(), mode);
        assert_eq!(
            shell_code,
            if rust { 0 } else { 1 },
            "privdir {name} identity={identity:?} mode={mode:?}"
        );
    };
    row("locked", None, None);
    row("open", None, None);
    row("plain-file", None, None);
    row("linkdir", None, None);
    row("missing", None, None);
    row("empty600", None, None);
    row("locked", Some("live"), None);
    row("locked", Some("0:0"), None);
    row("locked", None, Some("700"));
    row("locked", None, Some("0700"));
    row("locked", None, Some("755"));
    row("locked", Some("live"), Some("700"));
    row("open", Some("live"), Some("755"));
    row("missing", Some(""), Some(""));
}

#[test]
fn private_empty_directory_parity() {
    let twins = Twins::build("init-delete-privempty");
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::create_dir_all(home.join("empty")).expect("empty dir");
        std::fs::create_dir_all(home.join("full")).expect("full dir");
        write(home, "full/file", b"x\n");
        std::fs::create_dir_all(home.join("dotted")).expect("dotted dir");
        write(home, "dotted/.hidden", b"x\n");
        std::fs::create_dir_all(home.join("open")).expect("open dir");
        chmod(&home.join("open"), 0o755);
    }
    let row = |name: &str| {
        let shell_path = twins.shell_home.join(name);
        let rust_path = twins.rust_home.join(name);
        let body = format!(
            "if _dot_init_private_empty_directory_matches {}; then code=0; else code=$?; fi",
            sq(shell_path.to_str().expect("shell path"))
        );
        let shell_code = probe(&twins.shell_home, &[], &body);
        let rust = delete::private_empty_directory_matches(&rust_path, None, None);
        assert_eq!(shell_code, if rust { 0 } else { 1 }, "privempty {name}");
    };
    row("empty");
    row("full");
    row("dotted");
    row("open");
    row("missing");
}

#[test]
fn parent_delete_parity() {
    let twins = Twins::build("init-delete-parent");
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::create_dir_all(home.join("stage")).expect("stage dir");
        std::fs::create_dir_all(home.join("busy")).expect("busy dir");
        write(home, "busy/claim", b"x\n");
    }
    let row = |name: &str, identity: &str, mode: &str| {
        let shell_path = twins.shell_home.join(name);
        let rust_path = twins.rust_home.join(name);
        let shell_id = if identity == "live" {
            identity_of(&shell_path)
        } else {
            identity.to_string()
        };
        let rust_id = if identity == "live" {
            identity_of(&rust_path)
        } else {
            identity.to_string()
        };
        let body = format!(
            "if _dot_init_parent_delete_matches {} {} {}; then code=0; else code=$?; fi",
            sq(shell_path.to_str().expect("shell path")),
            sq(&shell_id),
            sq(mode)
        );
        let shell_code = probe(&twins.shell_home, &[], &body);
        let rust = delete::parent_delete_matches(&rust_path, &rust_id, mode);
        assert_eq!(
            shell_code,
            if rust { 0 } else { 1 },
            "parent {name} mode={mode}"
        );
    };
    row("stage", "live", "700");
    row("stage", "0:0", "700");
    row("stage", "live", "0700");
    row("busy", "live", "700");
    row("missing", "0:0", "700");
}

/// One git store plus the worktree it commits from: a bare repo at
/// `dir` with branch `BRANCH` at `commit`, carrying a mode-600
/// generation marker for (`NONCE`, `commit`, `IDENTITY`).
struct GitStore {
    dir: PathBuf,
    work: PathBuf,
    commit: String,
}

/// Commit `message` in a fixture store with fixed author/committer
/// dates, so twin stores built from identical trees share one tip
/// hash — and therefore one marker body — across engines.
fn git_store_commit(dir: &str, work: &str, message: &str) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "--git-dir",
            dir,
            "--work-tree",
            work,
            "commit",
            "--quiet",
            "-m",
            message,
        ])
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13+00:00")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13+00:00")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git commit");
    assert!(status.success(), "git commit {message}");
}

/// Sequence for store names: every row builds fresh twin stores,
/// and rebuilding the same names would leave the second commit
/// with an empty tree (commit rejects it), so names never repeat.
static STORE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Build a store under `root/name-<seq>.git` (committing from its
/// own worktree), with a fresh valid marker.
fn build_git_store(root: &Path, name: &str) -> GitStore {
    let seq = STORE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = root.join(format!("{name}-{seq}.git"));
    let work = root.join(format!("{name}-{seq}-work"));
    git(&[
        "init",
        "--quiet",
        "--bare",
        "--initial-branch",
        BRANCH,
        dir.to_str().expect("store path"),
    ]);
    std::fs::create_dir_all(&work).expect("store work");
    write(&work, "seed", b"seed\n");
    let dir_text = dir.to_str().expect("store path").to_string();
    let work_text = work.to_str().expect("work path").to_string();
    git(&[
        "--git-dir",
        &dir_text,
        "--work-tree",
        &work_text,
        "add",
        "-A",
    ]);
    git_store_commit(&dir_text, &work_text, "seed");
    let commit = git_out(&["--git-dir", &dir_text, "rev-parse", "HEAD"]);
    write_marker(&dir, &commit);
    GitStore { dir, work, commit }
}

/// Write a valid mode-600 generation marker for (`NONCE`,
/// `commit`, `IDENTITY`) into a store.
fn write_marker(store: &Path, commit: &str) {
    let body = format!(
        "cgraf78 dot client generation v1\nnonce={NONCE}\ncommit={commit}\nidentity={IDENTITY}\n"
    );
    let marker = store.join("dot-init-generation-v1");
    write(store, "dot-init-generation-v1", body.as_bytes());
    chmod(&marker, 0o600);
}

#[test]
fn git_delete_parity() {
    let twins = Twins::build("init-delete-git");
    // Each row gets a fresh twin store pair so marker and branch-tip
    // mutations never leak across rows.
    let row = |template: Option<&str>,
               mode: Option<u32>,
               link_marker: bool,
               advance_tip: bool,
               identity: &str| {
        let shell_store = build_git_store(twins.root(), "sh-store");
        let rust_store = build_git_store(twins.root(), "rs-store");
        assert_eq!(shell_store.commit, rust_store.commit, "twin tips agree");
        for store in [&shell_store, &rust_store] {
            match template {
                Some(case) => {
                    write(
                        &store.dir,
                        "dot-init-generation-v1",
                        case.replace("COMMIT", &store.commit).as_bytes(),
                    );
                }
                None => {
                    std::fs::remove_file(store.dir.join("dot-init-generation-v1"))
                        .expect("drop marker");
                }
            }
            if link_marker {
                let marker = store.dir.join("dot-init-generation-v1");
                std::fs::remove_file(&marker).expect("drop marker file");
                std::os::unix::fs::symlink("elsewhere", &marker).expect("link marker");
            } else if let Some(bits) = mode {
                chmod(&store.dir.join("dot-init-generation-v1"), bits);
            }
            if advance_tip {
                write(&store.work, "second", b"second\n");
                let dir_text = store.dir.to_str().expect("store path").to_string();
                let work_text = store.work.to_str().expect("work path").to_string();
                git(&[
                    "--git-dir",
                    &dir_text,
                    "--work-tree",
                    &work_text,
                    "add",
                    "-A",
                ]);
                git_store_commit(&dir_text, &work_text, "second");
            }
        }
        let shell_id = if identity == "live" {
            identity_of(&shell_store.dir)
        } else {
            identity.to_string()
        };
        let rust_id = if identity == "live" {
            identity_of(&rust_store.dir)
        } else {
            identity.to_string()
        };
        let body = format!(
            "if _dot_init_git_delete_matches {} {} {} {} {}; then code=0; else code=$?; fi",
            sq(shell_store.dir.to_str().expect("shell store")),
            sq(&shell_id),
            sq(&shell_store.commit),
            sq(IDENTITY),
            sq(BRANCH),
        );
        let env = [
            ("DOT_INIT_NONCE", NONCE),
            ("DOT_INIT_COMMIT", shell_store.commit.as_str()),
            ("DOT_INIT_IDENTITY", IDENTITY),
            ("DOT_INIT_BRANCH", BRANCH),
        ];
        let shell_code = probe(&twins.shell_home, &env, &body);
        let rust = delete::git_delete_matches(
            &rust_store.dir,
            &rust_id,
            NONCE,
            &rust_store.commit,
            IDENTITY,
            BRANCH,
        );
        assert_eq!(
            shell_code,
            if rust { 0 } else { 1 },
            "git template={template:?} mode={mode:?} link={link_marker} advance={advance_tip} identity={identity}"
        );
    };
    // `COMMIT` in a template renders as the row's own tip on each
    // side (the twins share one tip hash by construction).
    let valid = "cgraf78 dot client generation v1\nnonce=test-nonce-55\ncommit=COMMIT\nidentity=github.com/example/dot\n";
    row(Some(valid), None, false, false, "live");
    // The marker gate never checks permission bits: a world-readable
    // marker still matches on both engines.
    row(Some(valid), Some(0o644), false, false, "live");
    row(Some(valid), None, false, false, "0:0");
    let other_nonce = valid.replace("test-nonce-55", "other-nonce");
    row(Some(&other_nonce), None, false, false, "live");
    let other_commit = valid.replace("COMMIT", "0123456789abcdef0123456789abcdef01234567");
    row(Some(&other_commit), None, false, false, "live");
    let other_identity = valid.replace("github.com/example/dot", "github.com/other/dot");
    row(Some(&other_identity), None, false, false, "live");
    let five_lines = format!("{valid}extra=1\n");
    row(Some(&five_lines), None, false, false, "live");
    let dup_nonce = valid.replace(
        "nonce=test-nonce-55\n",
        "nonce=test-nonce-55\nnonce=test-nonce-55\n",
    );
    row(Some(&dup_nonce), None, false, false, "live");
    row(Some("not a marker\n"), None, false, false, "live");
    row(Some(""), None, false, false, "live");
    row(None, None, false, false, "live");
    row(Some(valid), None, true, false, "live");
    row(Some(valid), None, false, true, "live");
    row(Some(valid), None, false, false, "");
    // Missing candidates fail on both engines, even against an empty
    // expectation (the failed stat counts as empty, but the
    // generation gate still fails).
    for missing_identity in ["0:0", ""] {
        let shell_missing = twins.shell_home.join("gone-git");
        let rust_missing = twins.rust_home.join("gone-git");
        let missing_commit = "0123456789abcdef0123456789abcdef01234567";
        let body = format!(
            "if _dot_init_git_delete_matches {} {} {} {} {}; then code=0; else code=$?; fi",
            sq(shell_missing.to_str().expect("shell path")),
            sq(missing_identity),
            sq(missing_commit),
            sq(IDENTITY),
            sq(BRANCH),
        );
        let env = [
            ("DOT_INIT_NONCE", NONCE),
            ("DOT_INIT_COMMIT", missing_commit),
            ("DOT_INIT_IDENTITY", IDENTITY),
            ("DOT_INIT_BRANCH", BRANCH),
        ];
        let shell_code = probe(&twins.shell_home, &env, &body);
        let rust = delete::git_delete_matches(
            &rust_missing,
            missing_identity,
            NONCE,
            missing_commit,
            IDENTITY,
            BRANCH,
        );
        assert_eq!(
            shell_code,
            if rust { 0 } else { 1 },
            "git missing identity={missing_identity:?}"
        );
    }
}

#[test]
fn parked_generation_parity() {
    let twins = Twins::build("init-delete-parked");
    // One orchestration row: build the requested shapes on both
    // sides, run the shell against stub verifiers and the port
    // against matching closures, then compare verdicts and every
    // observable end-state. `target`/`park` are home-relative names;
    // a `None` shape stays absent.
    let row = |label: &str,
               target: &str,
               target_shape: Option<&str>,
               park: &str,
               park_shape: Option<&str>,
               verifier: &str,
               remover: &str| {
        for home in [&twins.shell_home, &twins.rust_home] {
            for (name, shape) in [(target, target_shape), (park, park_shape)] {
                let path = home.join(name);
                match shape {
                    Some("file") => write(home, name, b"payload\n"),
                    Some("dir") => std::fs::create_dir_all(&path).expect("park dir"),
                    Some("tree") => {
                        std::fs::create_dir_all(&path).expect("park tree");
                        write(home, &format!("{name}/inner"), b"inner\n");
                    }
                    Some(other) => panic!("unknown shape {other}"),
                    None => {}
                }
            }
        }
        let shell_target = twins.shell_home.join(target);
        let shell_park = twins.shell_home.join(park);
        let rust_target = twins.rust_home.join(target);
        let rust_park = twins.rust_home.join(park);
        let flag = twins.shell_home.join("flaky-flag");
        let _ = std::fs::remove_file(&flag);
        let stubs = "v_ok() { return 0; }\nv_ko() { return 1; }\nv_once() { if [[ -e $V_FLAG ]]; then return 1; else : >\"$V_FLAG\"; return 0; fi; }\n";
        let body = format!(
            "{stubs}if _dot_init_delete_parked_generation {} {} {} {verifier}; then code=0; else code=$?; fi",
            sq(shell_target.to_str().expect("shell target")),
            sq(shell_park.to_str().expect("shell park")),
            sq(remover),
        );
        let shell_env = [
            ("DOT_INIT_NONCE", NONCE),
            ("V_FLAG", flag.to_str().expect("flag path")),
        ];
        let shell_code = probe(&twins.shell_home, &shell_env, &body);
        let flipped = std::cell::Cell::new(false);
        let v_once = |_: &Path| {
            let first = !flipped.get();
            flipped.set(true);
            first
        };
        let verifier_fn: &dyn Fn(&Path) -> bool = match verifier {
            "v_ok" => &|_: &Path| true,
            "v_ko" => &|_: &Path| false,
            "v_once" => &v_once,
            other => panic!("unknown verifier {other}"),
        };
        let mut cache = MoveCache::default();
        let rust = delete::delete_parked_generation(
            &rust_target,
            &rust_park,
            remover,
            verifier_fn,
            &mut cache,
        );
        assert_eq!(shell_code, if rust { 0 } else { 1 }, "parked {label}");
        assert_eq!(
            shape(&shell_target),
            shape(&rust_target),
            "parked {label} target end-state"
        );
        assert_eq!(
            shape(&shell_park),
            shape(&rust_park),
            "parked {label} park end-state"
        );
    };
    // Same names per row would collide across rows, so each row gets
    // its own object names.
    row("both-absent", "a1", None, "a1.park", None, "v_ok", "leaf");
    row(
        "remove-file",
        "a2",
        Some("file"),
        "a2.park",
        None,
        "v_ok",
        "leaf",
    );
    row(
        "restore-file",
        "a3",
        Some("file"),
        "a3.park",
        None,
        "v_ko",
        "leaf",
    );
    row(
        "target-won",
        "a4",
        Some("file"),
        "a4.park",
        Some("file"),
        "v_ok",
        "leaf",
    );
    row(
        "stale-park",
        "a5",
        Some("file"),
        "a5.park",
        Some("file"),
        "v_ko",
        "leaf",
    );
    row(
        "bogus-remover",
        "a6",
        Some("file"),
        "a6.park",
        None,
        "v_ok",
        "bogus",
    );
    row(
        "remove-dir",
        "a7",
        Some("dir"),
        "a7.park",
        None,
        "v_ok",
        "parent",
    );
    row(
        "busy-dir",
        "a8",
        Some("tree"),
        "a8.park",
        None,
        "v_ok",
        "parent",
    );
    row(
        "remove-tree",
        "a9",
        Some("tree"),
        "a9.park",
        None,
        "v_ok",
        "tree",
    );
    row(
        "missing-parent",
        "a10",
        Some("file"),
        "nodir/a10.park",
        None,
        "v_ok",
        "leaf",
    );
    row(
        "flaky-verifier",
        "a11",
        Some("file"),
        "a11.park",
        None,
        "v_once",
        "leaf",
    );
    row(
        "rm-file-onto-dir",
        "a12",
        Some("file"),
        "a12.park",
        Some("dir"),
        "v_ok",
        "leaf",
    );
    row(
        "empty-remover",
        "a13",
        Some("file"),
        "a13.park",
        None,
        "v_ok",
        "",
    );
}

#[test]
fn parked_leaf_integration_parity() {
    let twins = Twins::build("init-delete-parked-leaf");
    let repo = build_content_repo(&twins);
    let repo_text = repo.dir.to_str().expect("repo path").to_string();
    // End-to-end through the real leaf validator: each engine
    // derives its own park path, then parks, validates, and removes
    // the tracked file.
    let shell_body = format!(
        "v_leaf() {{ _dot_init_leaf_delete_matches \"$1\" \"$V_ID\" {} {} 100644 {}; }}\nT=$HOME/file1\nV_ID=$(_dot_path_identity \"$T\")\n_dot_init_delete_park_path \"$T\" leaf file1 || {{ code=97; }}\npark=$REPLY\nif [[ -z ${{code+x}} ]] && _dot_init_delete_parked_generation \"$T\" \"$park\" leaf v_leaf; then code=0; elif [[ -z ${{code+x}} ]]; then code=$?; fi",
        sq(&repo_text),
        sq(&repo.commit),
        sq(&repo.file_oid),
    );
    let shell_env = [("DOT_INIT_NONCE", NONCE)];
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &shell_env,
        &format!("{shell_body}\nprintf 'code=%s\\n' \"$code\"\n"),
    );
    let rust_target = twins.rust_home.join("file1");
    let rust_id = identity_of(&rust_target);
    let rust_park =
        delete::delete_park_path(&rust_target, "leaf", "file1", NONCE).expect("rust park path");
    let leaf_ok = |park: &Path| {
        delete::leaf_delete_matches(
            park,
            &rust_id,
            &repo.dir,
            &repo.commit,
            "100644",
            &repo.file_oid,
            &twins.rust_home,
        )
    };
    let mut cache = MoveCache::default();
    let rust =
        delete::delete_parked_generation(&rust_target, &rust_park, "leaf", &leaf_ok, &mut cache);
    assert_eq!(
        shell_code,
        if rust { 0 } else { 1 },
        "parked leaf integration"
    );
    assert_eq!(
        shape(&twins.shell_home.join("file1")),
        shape(&rust_target),
        "integration target end-state"
    );
    assert!(!shape(&rust_target).0, "tracked file removed");
}
