//! Differential parity tests for `src/repos_pull_normalize.rs`
//! against the live shell (`lib/dot/repos/pull.sh`): the parent
//! snapshot inventory, commit path typing, and the updated-path
//! normalization walk (single path, parents, whole tree).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_pull_normalize::{
    CommitPathType, ParentStatus, commit_path_type, normalize_updated_path,
    normalize_updated_path_parents, normalize_updated_paths, snapshot_parent_status,
    snapshot_updated_path_parents,
};
use dot::test_support::TempDir;

/// Sources for the normalization cluster.
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the normalization libraries sourced.
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

/// Commit the worktree with `message`.
fn commit(dir: &Path, message: &str) {
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
            message,
        ],
    );
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
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

/// Blob oid of worktree `name`, via direct git.
fn blob_of(dir: &Path, name: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["hash-object", "--no-filters", "--", name])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "hash-object {name}");
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

/// `git -C dir` prefix for the Rust side.
fn prefix_of(dir: &Path) -> Vec<std::ffi::OsString> {
    ["-C".to_string(), dir.to_string_lossy().into_owned()]
        .iter()
        .map(std::ffi::OsString::from)
        .collect()
}

/// Process umask both sides observe.
fn mask() -> u32 {
    dot::temp::read_umask().expect("read umask")
}

/// Fixture: isolated `$HOME`, a repo with an A→B generation pair,
/// and the worktree checked out at B.
struct Fixture {
    _dir: TempDir,
    home: PathBuf,
    repo: PathBuf,
    before: String,
    after: String,
}

impl Fixture {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(&repo, &["init", "-q"]);
        stage(&repo, "top.txt", b"a1\n");
        stage(&repo, "sub/keep.txt", b"k\n");
        commit(&repo, "before");
        let before = head_of(&repo);
        stage(&repo, "top.txt", b"a2\n");
        stage(&repo, "sub/new.txt", b"n\n");
        stage(&repo, "newdir/n.txt", b"d\n");
        commit(&repo, "after");
        let after = head_of(&repo);
        Fixture {
            _dir: dir,
            home,
            repo,
            before,
            after,
        }
    }

    fn repo_text(&self) -> String {
        self.repo.to_string_lossy().into_owned()
    }
}

#[test]
fn snapshot_updated_path_parents_matches_shell() {
    let fixture = Fixture::build("pull-snapshot");
    let repo = fixture.repo_text();
    let root = fixture.repo.clone();
    let root_text = root.to_string_lossy().into_owned();
    let snippet = format!(
        "if _repo_snapshot_updated_path_parents \"$2\" \"{b}\" \"{a}\" git -C \"$3\"; then cat \"$REPLY\"; else echo SNAPSHOT-FAILED; fi\n",
        b = fixture.before,
        a = fixture.after,
    );
    let repo_os = std::ffi::OsString::from(&repo);
    let root_os = std::ffi::OsString::from(&root_text);
    let (status, out, err) = shell_run(&fixture.home, &[&root_os, &repo_os], &[], &snippet);
    assert_eq!(status, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    let shell_text = String::from_utf8(out).expect("snapshot utf8");
    assert!(
        !shell_text.contains("SNAPSHOT-FAILED"),
        "shell snapshot failed"
    );
    let prefix = prefix_of(&fixture.repo);
    let rust = snapshot_updated_path_parents(&prefix, &root_text, &fixture.before, &fixture.after)
        .expect("rust snapshot");
    assert_eq!(rust, shell_text, "snapshot content parity");
    // Unknown `after` fails on both sides.
    let snippet = format!(
        "if _repo_snapshot_updated_path_parents \"$2\" \"{b}\" no-such-ref git -C \"$3\"; then echo ok; else echo reject; fi\n",
        b = fixture.before,
    );
    let (status, out, _) = shell_run(&fixture.home, &[&root_os, &repo_os], &[], &snippet);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"reject\n", "shell rejects unknown ref");
    assert!(
        snapshot_updated_path_parents(&prefix, &root_text, &fixture.before, "no-such-ref")
            .is_none(),
        "rust rejects unknown ref"
    );
}

#[test]
fn commit_path_type_matches_shell() {
    let fixture = Fixture::build("pull-pathtype");
    let repo = fixture.repo_text();
    let prefix = prefix_of(&fixture.repo);
    for (commit, relative, want) in [
        (
            fixture.after.clone(),
            "sub".to_string(),
            Some(CommitPathType::Tree),
        ),
        (
            fixture.after.clone(),
            "top.txt".to_string(),
            Some(CommitPathType::Blob),
        ),
        (
            fixture.after.clone(),
            "missing".to_string(),
            Some(CommitPathType::Missing),
        ),
        (
            fixture.before.clone(),
            "newdir".to_string(),
            Some(CommitPathType::Missing),
        ),
        (
            "no-such".to_string(),
            "top.txt".to_string(),
            Some(CommitPathType::Missing),
        ),
    ] {
        let snippet = format!(
            "if _repo_commit_path_type \"{commit}\" \"{relative}\" git -C {repo}; then echo \"rc=0 reply=$REPLY\"; else echo \"rc=1 reply=$REPLY\"; fi\n"
        );
        let (status, out, err) = shell_run(&fixture.home, &[], &[], &snippet);
        assert_eq!(status, 0, "harness exit");
        assert!(err.is_empty(), "shell stderr: {err:?}");
        let shell_line = String::from_utf8(out).expect("utf8");
        let rust = commit_path_type(&prefix, &commit, &relative);
        let rust_line = match &rust {
            Some(CommitPathType::Blob) => "rc=0 reply=blob\n".to_string(),
            Some(CommitPathType::Tree) => "rc=0 reply=tree\n".to_string(),
            Some(CommitPathType::Missing) => "rc=0 reply=missing\n".to_string(),
            None => "rc=1 reply=\n".to_string(),
        };
        assert_eq!(shell_line, rust_line, "parity for {commit}:{relative}");
        assert_eq!(rust, want, "verdict for {commit}:{relative}");
    }
}

#[test]
fn snapshot_parent_status_matches_shell() {
    let fixture = Fixture::build("pull-snapstatus");
    let prefix = prefix_of(&fixture.repo);
    let root_text = fixture.repo.to_string_lossy().into_owned();
    let snapshot =
        snapshot_updated_path_parents(&prefix, &root_text, &fixture.before, &fixture.after)
            .expect("snapshot");
    assert!(!snapshot.is_empty(), "snapshot records parents");
    let first: Vec<&str> = snapshot.lines().next().expect("line").split('\t').collect();
    assert_eq!(first.len(), 2, "record shape");
    let (identity, relative) = (first[0], first[1]);
    // (snapshot, relative, identity, shell-expect): found is true,
    // absent is false, malformed is an error.
    let rows: Vec<(String, String, String, ParentStatus)> = vec![
        (
            snapshot.clone(),
            relative.to_string(),
            identity.to_string(),
            ParentStatus::Recorded,
        ),
        (
            snapshot.clone(),
            "elsewhere".to_string(),
            identity.to_string(),
            ParentStatus::Absent,
        ),
        (
            snapshot.clone(),
            relative.to_string(),
            "0:0".to_string(),
            ParentStatus::Malformed,
        ),
        (
            "garbage\n".to_string(),
            relative.to_string(),
            identity.to_string(),
            ParentStatus::Malformed,
        ),
        (
            "1:2\textra\tfield\n".to_string(),
            relative.to_string(),
            identity.to_string(),
            ParentStatus::Malformed,
        ),
    ];
    for (content, rel, ident, want) in &rows {
        let snap_path = fixture.home.join("snap.txt");
        std::fs::write(&snap_path, content).expect("snapshot file");
        let snap_text = snap_path.to_string_lossy().into_owned();
        let probe = format!(
            "_repo_snapshot_parent_status \"{snap_text}\" \"{rel}\" \"{ident}\"; echo \"rc=$?\"\n"
        );
        let (status, out, err) = shell_run(&fixture.home, &[], &[], &probe);
        assert_eq!(status, 0, "harness exit");
        assert!(err.is_empty(), "shell stderr: {err:?}");
        let shell_rc = String::from_utf8(out).expect("utf8");
        let rust = snapshot_parent_status(content, rel, ident);
        let rust_rc = match &rust {
            ParentStatus::Recorded => "rc=0\n",
            ParentStatus::Absent => "rc=1\n",
            ParentStatus::Malformed => "rc=2\n",
        };
        assert_eq!(shell_rc, rust_rc, "parity for {rel}");
        assert_eq!(&rust, want, "verdict for {rel}");
    }
}

#[test]
fn normalize_updated_path_parents_matches_shell() {
    let fixture = Fixture::build("pull-normparents");
    let prefix = prefix_of(&fixture.repo);
    // Detached root: plain directory mirroring the `after` tree top.
    let root = fixture.home.join("client");
    std::fs::create_dir_all(root.join("sub")).expect("client sub");
    std::fs::create_dir_all(root.join("newdir")).expect("client newdir");
    let root_text = root.to_string_lossy().into_owned();
    let repo = fixture.repo_text();
    let snapshot =
        snapshot_updated_path_parents(&prefix, &root_text, &fixture.before, &fixture.after)
            .expect("snapshot");
    let snap_path = fixture.home.join("parents-snap.txt");
    std::fs::write(&snap_path, &snapshot).expect("snapshot file");
    let snap_text = snap_path.to_string_lossy().into_owned();
    let mask = mask();
    // (relative, mutate, shell-expect): the recorded parents pass,
    // a replaced directory fails, a file where the tree wants a
    // directory fails.
    for (relative, mutate, want) in [
        ("sub/new.txt", "keep", true),
        ("newdir/n.txt", "keep", true),
        ("top.txt", "keep", true),
        ("sub/new.txt", "file", false),
        ("sub/new.txt", "link", false),
        ("P/f.txt", "keep", false),
    ] {
        let target = root.join(relative.split('/').next().unwrap_or(relative));
        match mutate {
            "file" => {
                std::fs::remove_dir_all(&target).ok();
                std::fs::write(&target, b"blocking file\n").ok();
            }
            "link" => {
                std::fs::remove_dir_all(&target).ok();
                #[cfg(unix)]
                std::os::unix::fs::symlink("sub", &target).expect("symlink");
            }
            _ => {}
        }
        let probe = format!(
            "if _repo_normalize_updated_path_parents \"$2\" \"{b}\" \"{a}\" \"{relative}\" \"{snap_text}\" git -C \"$3\"; then echo ok; else echo reject; fi\n",
            b = fixture.before,
            a = fixture.after,
        );
        let root_os = std::ffi::OsString::from(&root_text);
        let repo_os = std::ffi::OsString::from(&repo);
        let (status, out, err) = shell_run(&fixture.home, &[&root_os, &repo_os], &[], &probe);
        assert_eq!(status, 0, "harness exit for {relative}/{mutate}");
        assert!(err.is_empty(), "shell stderr: {err:?}");
        let shell_ok = out == b"ok\n";
        // Rust observes the same mutated shape; restore afterwards.
        let rust_ok = normalize_updated_path_parents(
            &prefix,
            &root_text,
            &fixture.before,
            &fixture.after,
            relative,
            &snapshot,
            mask,
        );
        if mutate != "keep" {
            std::fs::remove_file(&target).ok();
            std::fs::remove_dir_all(&target).ok();
            std::fs::create_dir_all(&target).expect("restore dir");
        }
        assert_eq!(shell_ok, want, "shell for {relative}/{mutate}");
        assert_eq!(rust_ok, want, "rust for {relative}/{mutate}");
    }
    // Snapshot variants over the restored tree: a missing record
    // still passes but clamps the directory; a mismatched identity
    // fails outright.
    use std::os::unix::fs::PermissionsExt as _;
    let set_mode = |path: &Path| {
        let mut perms = std::fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o777);
        std::fs::set_permissions(path, perms).expect("chmod");
    };
    let dir_mode =
        |path: &Path| std::fs::metadata(path).expect("meta").permissions().mode() & 0o7777;
    // `newdir` is new in `after`, so its record decides the
    // snapshot branch; `sub` exists in both and always continues.
    let fresh_dir = root.join("newdir");
    let no_fresh: String = snapshot
        .lines()
        .filter(|line| !line.ends_with("\tnewdir"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(!no_fresh.is_empty(), "sub record survives");
    let wrong_fresh: String = snapshot
        .lines()
        .map(|line| {
            if line.ends_with("\tnewdir") {
                "0:0\tnewdir\n".to_string()
            } else {
                format!("{line}\n")
            }
        })
        .collect::<Vec<_>>()
        .join("");
    for (content, want, clamp) in [(&no_fresh, true, true), (&wrong_fresh, false, false)] {
        let variant_path = fixture.home.join("parents-variant.txt");
        std::fs::write(&variant_path, content).expect("variant snapshot");
        let variant_text = variant_path.to_string_lossy().into_owned();
        set_mode(&fresh_dir);
        let probe = format!(
            "if _repo_normalize_updated_path_parents \"$2\" \"{b}\" \"{a}\" \"newdir/n.txt\" \"{variant_text}\" git -C \"$3\"; then echo ok; else echo reject; fi\n",
            b = fixture.before,
            a = fixture.after,
        );
        let root_os = std::ffi::OsString::from(&root_text);
        let repo_os = std::ffi::OsString::from(&repo);
        let (status, out, err) = shell_run(&fixture.home, &[&root_os, &repo_os], &[], &probe);
        assert_eq!(status, 0, "harness exit");
        assert!(err.is_empty(), "shell stderr: {err:?}");
        let shell_ok = out == b"ok\n";
        let shell_mode = dir_mode(&fresh_dir);
        set_mode(&fresh_dir);
        let rust_ok = normalize_updated_path_parents(
            &prefix,
            &root_text,
            &fixture.before,
            &fixture.after,
            "newdir/n.txt",
            content,
            mask,
        );
        let rust_mode = dir_mode(&fresh_dir);
        assert_eq!(shell_ok, want, "shell variant (clamp={clamp})");
        assert_eq!(rust_ok, want, "rust variant (clamp={clamp})");
        if clamp {
            assert_eq!(rust_mode, shell_mode, "ceiling mode parity");
            if 0o777u32 & !(mask & 0o777) != 0o777 {
                assert_ne!(rust_mode, 0o777, "ceiling clamped");
            }
        }
    }
}

#[test]
fn normalize_updated_path_matches_shell() {
    let fixture = Fixture::build("pull-normpath");
    let prefix = prefix_of(&fixture.repo);
    let root = fixture.home.join("client");
    std::fs::create_dir_all(root.join("sub")).expect("client sub");
    let root_text = root.to_string_lossy().into_owned();
    let snapshot =
        snapshot_updated_path_parents(&prefix, &root_text, &fixture.before, &fixture.after)
            .expect("snapshot");
    let top_oid = blob_of(&fixture.repo, "top.txt");
    stage(&root, "top.txt", b"a2\n");
    stage(&root, "sub/new.txt", b"n\n");
    let mask = mask();
    let home_text = fixture.home.to_string_lossy().into_owned();
    // (kind, relative, mode, oid): symlinks short-circuit true,
    // bad modes and content mismatches fail, clean files pass.
    let rows: Vec<(&str, &str, &str, String)> = vec![
        ("base", "sub/new.txt", "120000", "0".repeat(40)),
        ("base", "top.txt", "100600", top_oid.clone()),
        ("base", "top.txt", "100644", "0".repeat(40)),
        ("base", "top.txt", "100644", top_oid.clone()),
        ("base", "top.txt", "100755", top_oid.clone()),
        ("base", "gone.txt", "100644", top_oid.clone()),
        ("base", "a/.GIT/b", "100644", top_oid.clone()),
    ];
    for (kind, relative, mode, oid) in &rows {
        let probe = format!(
            "if _repo_normalize_updated_path \"$2\" \"{kind}\" \"{relative}\" \"{mode}\" \"{oid}\" \"{b}\" \"{a}\" \"$3\" git -C \"$4\"; then echo ok; else echo reject; fi\n",
            b = fixture.before,
            a = fixture.after,
        );
        let root_os = std::ffi::OsString::from(&root_text);
        let repo_os = std::ffi::OsString::from(fixture.repo_text());
        let snap_path = fixture.home.join("path-snap.txt");
        std::fs::write(&snap_path, &snapshot).expect("snapshot file");
        let snap_text = snap_path.to_string_lossy().into_owned();
        let snap_arg = std::ffi::OsString::from(&snap_text);
        let (status, out, err) =
            shell_run(&fixture.home, &[&root_os, &snap_arg, &repo_os], &[], &probe);
        assert_eq!(status, 0, "harness exit for {relative}");
        assert!(err.is_empty(), "shell stderr: {err:?}");
        let shell_ok = out == b"ok\n";
        let rust_ok = normalize_updated_path(
            &prefix,
            &root_text,
            kind,
            relative,
            mode,
            oid,
            &fixture.before,
            &fixture.after,
            &snapshot,
            &home_text,
            &[],
            mask,
        );
        assert_eq!(rust_ok, shell_ok, "parity for {kind}:{relative}@{mode}");
    }
    // A live overlay link owns its path for base checkouts: the
    // content hash never runs.
    let overlay = fixture.home.join("overlay");
    stage(&overlay, "home/owned.txt", b"owned\n");
    // The live home link carries the overlay's canonical relative
    // target (`.dotfiles-<name>/home/<rel>` for git-synced
    // overlays); the shipped checkout file must exist too.
    #[cfg(unix)]
    std::os::unix::fs::symlink(".dotfiles-o/home/owned.txt", fixture.home.join("owned.txt"))
        .expect("live link");
    let opath = overlay.to_string_lossy().into_owned();
    let record = format!("o|{opath}|https://example.invalid/x|git||git");
    let probe = format!(
        "OVERLAYS=('{record}')\nif _repo_normalize_updated_path \"$2\" base owned.txt 100644 0000000000000000000000000000000000000000 \"{b}\" \"{a}\" \"$3\" git -C \"$4\"; then echo ok; else echo reject; fi\n",
        b = fixture.before,
        a = fixture.after,
    );
    let root_os = std::ffi::OsString::from(&root_text);
    let snap_path = fixture.home.join("path-snap.txt");
    std::fs::write(&snap_path, &snapshot).expect("snapshot file");
    let snap_text = snap_path.to_string_lossy().into_owned();
    let snap_arg = std::ffi::OsString::from(&snap_text);
    let repo_os = std::ffi::OsString::from(fixture.repo_text());
    let (status, out, err) =
        shell_run(&fixture.home, &[&root_os, &snap_arg, &repo_os], &[], &probe);
    assert_eq!(status, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    assert_eq!(out, b"ok\n", "shell honors the live link");
    assert!(
        normalize_updated_path(
            &prefix,
            &root_text,
            "base",
            "owned.txt",
            "100644",
            "0000000000000000000000000000000000000000",
            &fixture.before,
            &fixture.after,
            &snapshot,
            &home_text,
            &[record],
            mask,
        ),
        "rust honors the live link"
    );
}

#[test]
fn normalize_updated_paths_matches_shell() {
    let fixture = Fixture::build("pull-normpaths");
    let prefix = prefix_of(&fixture.repo);
    let root_text = fixture.repo.to_string_lossy().into_owned();
    let snapshot =
        snapshot_updated_path_parents(&prefix, &root_text, &fixture.before, &fixture.after)
            .expect("snapshot");
    let home_text = fixture.home.to_string_lossy().into_owned();
    let mask = mask();
    // Clean checkout at `after` validates.
    let probe = format!(
        "if _repo_normalize_updated_paths \"$2\" base \"{b}\" \"{a}\" \"$3\" git -C \"$4\"; then echo ok; else echo reject; fi\n",
        b = fixture.before,
        a = fixture.after,
    );
    let root_os = std::ffi::OsString::from(&root_text);
    let snap_path = fixture.home.join("paths-snap.txt");
    std::fs::write(&snap_path, &snapshot).expect("snapshot file");
    let snap_text = snap_path.to_string_lossy().into_owned();
    let snap_os = std::ffi::OsString::from(&snap_text);
    let repo_os = std::ffi::OsString::from(fixture.repo_text());
    let (status, out, err) = shell_run(&fixture.home, &[&root_os, &snap_os, &repo_os], &[], &probe);
    assert_eq!(status, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    assert_eq!(out, b"ok\n", "shell accepts clean tree");
    assert!(
        normalize_updated_paths(
            &prefix,
            &root_text,
            "base",
            &fixture.before,
            &fixture.after,
            &snapshot,
            &home_text,
            &[],
            mask,
        ),
        "rust accepts clean tree"
    );
    // A dirty worktree file fails on both sides.
    std::fs::write(fixture.repo.join("sub/new.txt"), b"dirty\n").expect("dirty file");
    let (status, out, _) = shell_run(&fixture.home, &[&root_os, &snap_os, &repo_os], &[], &probe);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"reject\n", "shell rejects dirty tree");
    assert!(
        !normalize_updated_paths(
            &prefix,
            &root_text,
            "base",
            &fixture.before,
            &fixture.after,
            &snapshot,
            &home_text,
            &[],
            mask,
        ),
        "rust rejects dirty tree"
    );
    // A staged change trips the cached inventory on both sides.
    git(&fixture.repo, &["checkout", "-q", "--", "sub/new.txt"]);
    std::fs::write(fixture.repo.join("sub/new.txt"), b"staged\n").expect("stage file");
    git(&fixture.repo, &["add", "--", "sub/new.txt"]);
    let (status, out, _) = shell_run(&fixture.home, &[&root_os, &snap_os, &repo_os], &[], &probe);
    assert_eq!(status, 0, "harness exit");
    assert_eq!(out, b"reject\n", "shell rejects staged tree");
    assert!(
        !normalize_updated_paths(
            &prefix,
            &root_text,
            "base",
            &fixture.before,
            &fixture.after,
            &snapshot,
            &home_text,
            &[],
            mask,
        ),
        "rust rejects staged tree"
    );
    git(&fixture.repo, &["reset", "-q"]);
    git(&fixture.repo, &["checkout", "-q", "--", "."]);
}
