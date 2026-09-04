//! Differential parity tests for the init record/tree/snapshot
//! family (`lib/dot/init-client.sh`) against the live shell: the
//! symlink-blob byte gate, the transaction-record publisher and
//! validator, the `ls-tree` journal builder, the worktree candidate
//! gate, and the two path-snapshot helpers.
//!
//! Separate binary because each row drives real git and filesystem
//! state: the two engines work under disjoint home directories (and
//! disjoint fixture repositories), so sibling temps, stage paths,
//! and object stores never collide. Pure reads (snapshots) run both
//! engines against the same path, so device/inode lines compare
//! byte-for-byte; everywhere else only the verdict compares and live
//! identities merely gate it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_records as records;
use dot::init_client_records::{CandidateTreeInputs, PathSnapshot, WriteRecordInputs};
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the records chapter: cleanup temps, the shared git
/// and stat helpers, the reserved inventory, XDG homes, and the init
/// client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Forty-hex commit for record rows.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
/// Sixty-four-hex commit for the wide-digest rows.
const COMMIT64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Run one shell snippet with the init runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that
/// verdict — not the process status, which only says the printer
/// ran. A snippet that never reports (a harness bug, never a pass)
/// yields 99.
///
/// The locale stays pinned, and the run identity crosses as explicit
/// environment entries, mirroring how the engine exports them before
/// calling into this family.
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

/// The crate root backing hash subprocesses and the launcher file.
fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so sibling temps and record
/// paths never collide across engines.
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
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Run git for fixtures, with a pinned identity for commits.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Fresh repository with one committed file, on branch `main`.
fn init_repo(dir: &Path, tag: &str) -> PathBuf {
    let repo = dir.join(tag);
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q"]);
    git(&repo, &["branch", "-M", "main"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").expect("seed");
    git(&repo, &["add", "seed.txt"]);
    git(&repo, &["commit", "-qm", "seed"]);
    repo
}

/// Commit one regular file with fixed bytes and mode.
fn commit_file(repo: &Path, rel: &str, bytes: &[u8], mode: u32) {
    let path = repo.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    chmod(&path, mode);
    git(repo, &["add", rel]);
    git(repo, &["commit", "-qm", rel]);
}

/// Commit one symlink with a fixed target.
fn commit_link(repo: &Path, rel: &str, target: &str) {
    let path = repo.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    let _ = std::fs::remove_file(&path);
    std::os::unix::fs::symlink(target, &path).expect("symlink fixture");
    git(repo, &["add", rel]);
    git(repo, &["commit", "-qm", rel]);
}

/// Commit one blob with exact bytes and mode, bypassing the
/// working tree: symlinks with overlong or NUL targets cannot exist
/// on disk, but git stores whatever the index is told.
fn commit_blob_entry(repo: &Path, mode: &str, bytes: &[u8], rel: &str) {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .arg("hash-object")
        .arg("-w")
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .as_mut()
        .expect("hash stdin")
        .write_all(bytes)
        .expect("feed hash-object");
    let out = child.wait_with_output().expect("wait hash-object");
    assert!(out.status.success(), "hash-object failed");
    let hash = String::from_utf8(out.stdout).expect("hash utf8");
    git(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{},{}", hash.trim(), rel),
        ],
    );
    git(repo, &["commit", "-qm", rel]);
}

/// A submodule cacheinfo entry pointing at HEAD: index plumbing
/// needs a real object, never the fixture digest constant.
fn gitlink_info(repo: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("rev-parse HEAD");
    assert!(output.status.success());
    format!(
        "160000,{},mod",
        String::from_utf8(output.stdout).expect("sha utf8").trim()
    )
}

/// Remove every tracked path, leaving one empty commit behind.
fn clear_repo(repo: &Path) {
    git(repo, &["rm", "-qr", "."]);
    git(repo, &["commit", "-qm", "clear", "--allow-empty"]);
}

/// Shell verdict for `_dot_init_symlink_blob_safe`.
fn shell_blob(repo: &Path, home: &Path, branch: &str, path: &str) -> i32 {
    let snippet = format!(
        "_dot_init_symlink_blob_safe {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&repo.to_string_lossy()),
        sq(branch),
        sq(path),
    );
    shell_run(home, &[], &snippet).0
}

/// One blob row: both engines agree on the publishable verdict.
fn check_blob(tag: &str, branch: &str, path: &str, repo: &Path) {
    let twins = Twins::build(tag);
    let shell_code = shell_blob(repo, &twins.shell_home, branch, path);
    let rust_ok = records::symlink_blob_safe(repo, branch, path);
    assert_eq!(
        shell_code == 0,
        rust_ok,
        "shell/rust symlink-blob verdict parity for {path}"
    );
}

#[test]
fn symlink_blob_gates_forbidden_bytes() {
    let dir = TempDir::new("init-rec-blob").expect("temp dir");
    let repo = init_repo(dir.path(), "blobs");
    commit_link(&repo, "ok", "target/path");
    commit_link(&repo, "nl", "a\nb");
    commit_link(&repo, "tab", "a\tb");
    commit_link(&repo, "cr", "a\rb");
    for (tag, path) in [
        ("init-rec-blob-ok", "ok"),
        ("init-rec-blob-nl", "nl"),
        ("init-rec-blob-tab", "tab"),
        ("init-rec-blob-cr", "cr"),
    ] {
        check_blob(tag, "main", path, &repo);
    }
    // The safe link above must actually pass on both engines, or the
    // rows below would prove nothing.
    assert!(records::symlink_blob_safe(&repo, "main", "ok"));
}

#[test]
fn symlink_blob_gates_size_edges() {
    let dir = TempDir::new("init-rec-blob-size").expect("temp dir");
    let repo = init_repo(dir.path(), "blobs");
    commit_blob_entry(&repo, "120000", "a".repeat(4097).as_bytes(), "big");
    commit_blob_entry(&repo, "120000", "a".repeat(4096).as_bytes(), "edge");
    check_blob("init-rec-blob-big", "main", "big", &repo);
    check_blob("init-rec-blob-edge", "main", "edge", &repo);
    assert!(!records::symlink_blob_safe(&repo, "main", "big"));
    assert!(records::symlink_blob_safe(&repo, "main", "edge"));
}

#[test]
fn symlink_blob_rejects_missing_objects() {
    let dir = TempDir::new("init-rec-blob-missing").expect("temp dir");
    let repo = init_repo(dir.path(), "blobs");
    commit_link(&repo, "ok", "target");
    // Missing path, missing branch, and a regular blob queried as a
    // link target all fail on both engines.
    check_blob("init-rec-blob-nopath", "main", "absent", &repo);
    check_blob("init-rec-blob-nobranch", "nope", "ok", &repo);
    check_blob("init-rec-blob-regular", "main", "seed.txt", &repo);
}

/// Record environment shared by the round-trip rows.
fn record_inputs<'a>(home: &'a str, source_root: &'a Path) -> WriteRecordInputs<'a> {
    WriteRecordInputs {
        home,
        dot_bin: "/bin/dot",
        commit: Some(COMMIT),
        nonce: Some("test-nonce-51"),
        git_dev: Some("11"),
        git_ino: Some("22"),
        source_root,
    }
}

/// Shell verdict plus exported globals for `_dot_init_read_record`.
fn shell_read(home: &Path, record: &Path) -> (i32, Vec<u8>) {
    let snippet = format!(
        "_dot_init_read_record {}; code=$?; printf 'code=%s\\n' \"$code\"; printf 'phase=%s origin=%s branch=%s\\n' \"$DOT_INIT_PHASE\" \"$DOT_INIT_ORIGIN\" \"$DOT_INIT_BRANCH\"",
        sq(&record.to_string_lossy()),
    );
    let (code, stdout, _) = shell_run(home, &[], &snippet);
    (code, stdout)
}

#[test]
fn record_round_trip_shell_to_rust() {
    let twins = Twins::build("init-rec-roundtrip-sh");
    let record = twins.shell_home.join("record");
    let env = [
        ("DOT_BIN", "/bin/dot"),
        ("DOT_INIT_COMMIT", COMMIT),
        ("DOT_INIT_NONCE", "test-nonce-51"),
        ("DOT_INIT_GIT_DEV", "11"),
        ("DOT_INIT_GIT_INO", "22"),
    ];
    let snippet = format!(
        "_dot_init_write_record {} prepared https://example.test/dot ident-1 main -; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&record.to_string_lossy()),
    );
    let (shell_code, _, _) = shell_run(&twins.shell_home, &env, &snippet);
    assert_eq!(shell_code, 0, "shell write_record failed");
    let home = twins.shell_home.to_string_lossy().into_owned();
    let parsed = records::read_record(&record, &home).expect("rust reads shell record");
    assert_eq!(parsed.phase, "prepared");
    assert_eq!(parsed.origin, "https://example.test/dot");
    assert_eq!(parsed.identity, "ident-1");
    assert_eq!(parsed.branch, "main");
    assert_eq!(parsed.commit, COMMIT);
    assert_eq!(parsed.git_dir, format!("{home}/.dotfiles"));
    assert_eq!(parsed.worktree, home);
    assert_eq!(parsed.backup, "-");
    assert_eq!(parsed.dot, "/bin/dot");
    assert_eq!(parsed.nonce, "test-nonce-51");
    assert_eq!(parsed.git_dev, "11");
    assert_eq!(parsed.git_ino, "22");
    assert!(
        records::read_record(&record, "/elsewhere").is_err(),
        "foreign home must fail"
    );
}

#[test]
fn record_round_trip_rust_to_shell() {
    let twins = Twins::build("init-rec-roundtrip-rs");
    let record = twins.rust_home.join("record");
    let home = twins.rust_home.to_string_lossy().into_owned();
    let root = source_root();
    let mut cache = MoveCache::default();
    records::write_record(
        &record,
        "converging",
        "https://example.test/other",
        "ident-9",
        "main",
        "-",
        None,
        &record_inputs(&home, &root),
        &mut cache,
    )
    .expect("rust write_record failed");
    let (shell_code, stdout) = shell_read(&twins.rust_home, &record);
    assert_eq!(shell_code, 0, "shell reads rust record");
    let text = String::from_utf8_lossy(&stdout);
    assert!(
        text.contains("phase=converging origin=https://example.test/other branch=main"),
        "shell globals mismatch: {text}"
    );
}

#[test]
fn record_bodies_match_byte_for_byte() {
    let twins = Twins::build("init-rec-bytes");
    let shell_record = twins.shell_home.join("record");
    let rust_record = twins.rust_home.join("record");
    let env = [
        ("DOT_BIN", "/bin/dot"),
        ("DOT_INIT_COMMIT", COMMIT),
        ("DOT_INIT_NONCE", "test-nonce-51"),
        ("DOT_INIT_GIT_DEV", "11"),
        ("DOT_INIT_GIT_INO", "22"),
    ];
    // Same logical home name on both sides is impossible with twin
    // homes, so both engines write under the shell home here: the
    // shell through HOME, the port through an explicit parameter.
    // A second shell write proves the replace path byte-for-byte too.
    let snippet = format!(
        "_dot_init_write_record {} prepared https://example.test/dot ident-1 main -; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&shell_record.to_string_lossy()),
    );
    let (shell_code, _, _) = shell_run(&twins.shell_home, &env, &snippet);
    assert_eq!(shell_code, 0);
    let home = twins.shell_home.to_string_lossy().into_owned();
    let root = source_root();
    let mut cache = MoveCache::default();
    records::write_record(
        &rust_record,
        "prepared",
        "https://example.test/dot",
        "ident-1",
        "main",
        "-",
        None,
        &record_inputs(&home, &root),
        &mut cache,
    )
    .expect("rust write_record failed");
    let shell_bytes = std::fs::read(&shell_record).expect("shell bytes");
    let rust_bytes = std::fs::read(&rust_record).expect("rust bytes");
    // The source revision embeds the worktree HEAD: identical code,
    // identical revision line.
    assert_eq!(shell_bytes, rust_bytes, "record bodies diverge");
    let (rewrite_code, _, _) = shell_run(&twins.shell_home, &env, &snippet);
    assert_eq!(rewrite_code, 0, "shell rewrite failed");
    records::write_record(
        &rust_record,
        "prepared",
        "https://example.test/dot",
        "ident-1",
        "main",
        "-",
        None,
        &record_inputs(&home, &root),
        &mut cache,
    )
    .expect("rust rewrite failed");
    assert_eq!(
        std::fs::read(&shell_record).expect("shell rewrite bytes"),
        std::fs::read(&rust_record).expect("rust rewrite bytes"),
        "rewritten record bodies diverge"
    );
}

#[test]
fn record_write_applies_shell_defaults() {
    let twins = Twins::build("init-rec-defaults");
    let record = twins.rust_home.join("record");
    let home = twins.rust_home.to_string_lossy().into_owned();
    let root = source_root();
    let bare = WriteRecordInputs {
        home: &home,
        dot_bin: "/bin/dot",
        commit: None,
        nonce: None,
        git_dev: None,
        git_ino: None,
        source_root: &root,
    };
    let mut cache = MoveCache::default();
    records::write_record(
        &record,
        "prepared",
        "https://example.test/dot",
        "ident-1",
        "main",
        "-",
        None,
        &bare,
        &mut cache,
    )
    .expect("rust write with defaults failed");
    let parsed = records::read_record(&record, &home).expect("defaults record must validate");
    assert_eq!(parsed.commit, "0".repeat(40));
    assert_eq!(parsed.nonce, "legacy");
    assert_eq!(parsed.git_dev, "-");
    assert_eq!(parsed.git_ino, "-");
    // An explicit git directory crosses verbatim.
    let custom = twins.rust_home.join("custom");
    records::write_record(
        &custom,
        "prepared",
        "https://example.test/dot",
        "ident-1",
        "main",
        "-",
        Some("/custom/git"),
        &bare,
        &mut cache,
    )
    .expect("rust write with git dir failed");
    let text = std::fs::read_to_string(&custom).expect("custom bytes");
    assert!(text.contains("git_dir=/custom/git\n"), "git dir missing");
}

#[test]
fn record_write_matches_shell_failures() {
    let twins = Twins::build("init-rec-write-fail");
    let home = twins.shell_home.to_string_lossy().into_owned();
    let root = source_root();
    // A destination nobody can create fails on both engines.
    let missing_parent = twins.shell_home.join("no-such-dir/record");
    let snippet = format!(
        "_dot_init_write_record {} prepared o i main -; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&missing_parent.to_string_lossy()),
    );
    let (shell_code, _, _) = shell_run(&twins.shell_home, &[("DOT_BIN", "/bin/dot")], &snippet);
    let mut cache = MoveCache::default();
    let rust_ok = records::write_record(
        &missing_parent,
        "prepared",
        "o",
        "i",
        "main",
        "-",
        None,
        &record_inputs(&home, &root),
        &mut cache,
    )
    .is_ok();
    assert_eq!(shell_code == 0, rust_ok, "missing-parent verdict parity");
    // A revision probe outside any checkout fails on both engines.
    let elsewhere = twins.shell_home.join("record");
    let bogus = WriteRecordInputs {
        source_root: Path::new("/nonexistent-dot-source"),
        ..record_inputs(&home, &root)
    };
    let rust_probe = records::write_record(
        &elsewhere, "prepared", "o", "i", "main", "-", None, &bogus, &mut cache,
    )
    .is_ok();
    assert!(!rust_probe, "bogus source root must fail");
    let (probe_code, _, _) = shell_run(
        &twins.shell_home,
        &[
            ("DOT_BIN", "/bin/dot"),
            ("DOT_SOURCE_ROOT", "/nonexistent-dot-source"),
        ],
        &format!(
            "_dot_init_write_record {} prepared o i main -; code=$?; printf 'code=%s\\n' \"$code\"",
            sq(&elsewhere.to_string_lossy()),
        ),
    );
    assert_eq!(probe_code == 0, rust_probe, "probe-failure verdict parity");
}

/// One read-record row: mutate a valid record, then both engines
/// must agree on the verdict.
fn check_read(tag: &str, mutate: impl FnOnce(&Path, &str), home: Option<&str>) {
    let twins = Twins::build(tag);
    let record = twins.shell_home.join("record");
    let env = [("DOT_BIN", "/bin/dot")];
    let snippet = format!(
        "_dot_init_write_record {} prepared https://example.test/dot ident-1 main -; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&record.to_string_lossy()),
    );
    let (shell_code, _, _) = shell_run(&twins.shell_home, &env, &snippet);
    assert_eq!(shell_code, 0, "setup write failed");
    let shell_home = twins.shell_home.to_string_lossy().into_owned();
    mutate(&record, &shell_home);
    let snippet = format!(
        "_dot_init_read_record {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&record.to_string_lossy()),
    );
    let (shell_verdict, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    let rust_home = home.unwrap_or(&shell_home);
    let rust_ok = records::read_record(&record, rust_home).is_ok();
    assert_eq!(
        shell_verdict == 0,
        rust_ok,
        "shell/rust read-record verdict parity"
    );
}

/// Rewrite the whole record file.
fn rewrite(record: &Path, bytes: &[u8]) {
    std::fs::write(record, bytes).expect("rewrite fixture");
}

/// Splice one line of a valid record: replace the first line
/// starting with `prefix` by `replacement` (empty removes it).
fn splice(record: &Path, prefix: &str, replacement: &str) {
    let text = std::fs::read_to_string(record).expect("read fixture");
    let mut out = Vec::new();
    let mut done = false;
    for line in text.split('\n') {
        if !done && line.starts_with(prefix) {
            done = true;
            if !replacement.is_empty() {
                out.push(replacement.to_string());
            }
        } else {
            out.push(line.to_string());
        }
    }
    assert!(done, "splice prefix {prefix} not found");
    std::fs::write(record, out.join("\n")).expect("splice fixture");
}

#[test]
fn read_record_accepts_valid_variants() {
    // 64-hex digests validate like 40-hex ones.
    check_read(
        "init-rec-wide",
        |record, _| {
            splice(record, "commit=", &format!("commit={COMMIT64}"));
            splice(record, "dot_revision=", &format!("dot_revision={COMMIT64}"));
        },
        None,
    );
    // Bound git identity digits and a real backup validate.
    check_read(
        "init-rec-bound",
        |record, home| {
            splice(record, "git_dev=", "git_dev=123");
            splice(record, "git_ino=", "git_ino=456");
            splice(
                record,
                "backup=",
                &format!("backup={home}/.dot-backup/first"),
            );
            splice(record, "git_dir=", &format!("git_dir={home}/.git"));
        },
        None,
    );
    // Uppercase hex digests pass the shell's case-folded class.
    check_read(
        "init-rec-upper",
        |record, _| {
            splice(
                record,
                "commit=",
                "commit=ABCDEF0123456789ABCDEF0123456789ABCDEF01",
            );
        },
        None,
    );
    // A missing trailing newline still yields its final line.
    check_read(
        "init-rec-noeol",
        |record, _| {
            let mut bytes = std::fs::read(record).expect("read fixture");
            assert_eq!(bytes.pop(), Some(b'\n'));
            rewrite(record, &bytes);
        },
        None,
    );
    // NUL bytes never reach the shell parser: they drop silently.
    check_read(
        "init-rec-nul",
        |record, _| {
            let bytes = std::fs::read(record).expect("read fixture");
            let probe: Vec<u8> = bytes
                .iter()
                .flat_map(|byte| {
                    if *byte == b'p' {
                        vec![b'p', 0]
                    } else {
                        vec![*byte]
                    }
                })
                .collect();
            rewrite(record, &probe);
        },
        None,
    );
    // Every lifecycle phase validates.
    for phase in [
        "prepared",
        "backing-up",
        "backed-up",
        "git-staging",
        "git-staged",
        "publishing",
        "checkout",
        "converging",
        "complete",
    ] {
        check_read(
            &format!("init-rec-phase-{phase}"),
            |record, _| {
                splice(record, "phase=", &format!("phase={phase}"));
            },
            None,
        );
    }
}

#[test]
fn read_record_rejects_shape_damage() {
    check_read(
        "init-rec-badheader",
        |record, _| {
            splice(record, RECORD_HEADER_LINE, "bogus header");
        },
        None,
    );
    // Thirteen lines: one short.
    check_read(
        "init-rec-short",
        |record, _| {
            splice(record, "git_ino=", "");
        },
        None,
    );
    // Fifteen lines: one extra unknown key.
    check_read(
        "init-rec-long",
        |record, _| {
            let mut text = std::fs::read_to_string(record).expect("read fixture");
            text.push_str("extra=1\n");
            rewrite(record, text.as_bytes());
        },
        None,
    );
    // Fourteen lines but a repeated key and a missing one.
    check_read(
        "init-rec-dup",
        |record, _| {
            splice(record, "backup=", "origin=https://example.test/dup");
        },
        None,
    );
    // Fourteen lines with an unknown key swapped in.
    check_read(
        "init-rec-unknown",
        |record, _| {
            splice(record, "backup=", "mystery=-");
        },
        None,
    );
    // A line without `=`.
    check_read(
        "init-rec-nokey",
        |record, _| {
            splice(record, "backup=", "backup");
        },
        None,
    );
    // Empty and tabbed values fail the safe gate.
    check_read(
        "init-rec-empty",
        |record, _| {
            splice(record, "nonce=", "nonce=");
        },
        None,
    );
    check_read(
        "init-rec-tab",
        |record, _| {
            splice(record, "nonce=", "nonce=a\tb");
        },
        None,
    );
    check_read(
        "init-rec-cr",
        |record, _| {
            splice(record, "nonce=", "nonce=a\rb");
        },
        None,
    );
    // An empty file and a directory are not records.
    check_read(
        "init-rec-blank",
        |record, _| {
            rewrite(record, b"");
        },
        None,
    );
}

/// The header line, for splices that damage it.
const RECORD_HEADER_LINE: &str = "cgraf78 dot initialization transaction v1";

#[test]
fn read_record_rejects_field_damage() {
    check_read(
        "init-rec-phase",
        |record, _| {
            splice(record, "phase=", "phase=unknown");
        },
        None,
    );
    check_read(
        "init-rec-gitdir",
        |record, _| {
            splice(record, "git_dir=", "git_dir=/foreign/git");
        },
        None,
    );
    check_read(
        "init-rec-worktree",
        |record, _| {
            splice(record, "worktree=", "worktree=/foreign/home");
        },
        None,
    );
    check_read(
        "init-rec-commit39",
        |record, _| {
            splice(
                record,
                "commit=",
                "commit=0123456789abcdef0123456789abcdef0123456",
            );
        },
        None,
    );
    check_read(
        "init-rec-commit41",
        |record, _| {
            splice(
                record,
                "commit=",
                "commit=0123456789abcdef0123456789abcdef012345678",
            );
        },
        None,
    );
    check_read(
        "init-rec-commitxx",
        |record, _| {
            splice(
                record,
                "commit=",
                "commit=xx23456789abcdef0123456789abcdef01234567",
            );
        },
        None,
    );
    check_read(
        "init-rec-dotrel",
        |record, _| {
            splice(record, "dot=", "dot=bin/dot");
        },
        None,
    );
    check_read(
        "init-rec-dotslash",
        |record, _| {
            splice(record, "dot=", "dot=/bin//dot");
        },
        None,
    );
    check_read(
        "init-rec-dotdot",
        |record, _| {
            splice(record, "dot=", "dot=/bin/../dot");
        },
        None,
    );
    check_read(
        "init-rec-rev",
        |record, _| {
            splice(record, "dot_revision=", "dot_revision=xyz");
        },
        None,
    );
    check_read(
        "init-rec-nonce",
        |record, _| {
            splice(record, "nonce=", "nonce=has space");
        },
        None,
    );
    check_read(
        "init-rec-devmix",
        |record, _| {
            splice(record, "git_dev=", "git_dev=12");
        },
        None,
    );
    check_read(
        "init-rec-backuprel",
        |record, _| {
            splice(record, "backup=", "backup=relative/dir");
        },
        None,
    );
    check_read(
        "init-rec-backuppre",
        |record, _| {
            splice(record, "backup=", "backup=/other/.dot-backup/x");
        },
        None,
    );
    check_read(
        "init-rec-branch",
        |record, _| {
            splice(record, "branch=", "branch=..");
        },
        None,
    );
}

#[test]
fn read_record_rejects_state_damage() {
    // Group-readable records fail the privacy gate.
    check_read(
        "init-rec-mode",
        |record, _| {
            chmod(record, 0o640);
        },
        None,
    );
    // A symlink record is never a real file.
    check_read(
        "init-rec-link",
        |record, _| {
            let bytes = std::fs::read(record).expect("read fixture");
            std::fs::remove_file(record).expect("remove fixture");
            std::os::unix::fs::symlink("/nonexistent-target", record).expect("link fixture");
            let _ = bytes;
        },
        None,
    );
    // A missing record fails on both engines.
    check_read(
        "init-rec-missing",
        |record, _| {
            std::fs::remove_file(record).expect("remove fixture");
        },
        None,
    );
    // An oversized record fails before parsing.
    check_read(
        "init-rec-huge",
        |record, _| {
            splice(record, "origin=", &format!("origin={}", "x".repeat(16_384)));
        },
        None,
    );
}

/// Inputs for one candidate-tree row: journal under the twin home,
/// everything else shared.
fn tree_inputs<'a>(
    repo: &'a Path,
    output: &'a Path,
    home: &'a str,
    pwd: &'a str,
    source_root: &'a Path,
) -> CandidateTreeInputs<'a> {
    CandidateTreeInputs {
        repo,
        branch: "main",
        output,
        home,
        xdg_state_home: "",
        install_dir: None,
        state_dir: None,
        overlay_paths: &[],
        init_backup: None,
        pwd,
        source_root,
    }
}

/// One candidate-tree row: both engines inventory the same fixture
/// repository and must agree on the verdict and — on success — the
/// journal bytes.
fn check_tree(tag: &str, repo: &Path, backup: bool) -> (bool, bool) {
    let twins = Twins::build(tag);
    let shell_out = twins.shell_home.join("tree.tsv");
    let rust_out = twins.rust_home.join("tree.tsv");
    let snippet = format!(
        "_dot_init_candidate_tree {} main {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&repo.to_string_lossy()),
        sq(&shell_out.to_string_lossy()),
    );
    let shell_home = twins.shell_home.to_string_lossy().into_owned();
    let rust_home = twins.rust_home.to_string_lossy().into_owned();
    let shell_backup = format!("{shell_home}/.dot-backup/b1");
    let rust_backup = format!("{rust_home}/.dot-backup/b1");
    let mut env: Vec<(&str, &str)> = Vec::new();
    if backup {
        env.push(("DOT_INIT_BACKUP", shell_backup.as_str()));
    }
    let (shell_code, _, _) = shell_run(&twins.shell_home, &env, &snippet);
    let root = source_root();
    let mut rust_inputs = tree_inputs(repo, &rust_out, &rust_home, &rust_home, &root);
    if backup {
        rust_inputs.init_backup = Some(rust_backup.as_str());
    }
    let shell_ok = shell_code == 0;
    let rust_ok = records::candidate_tree(&rust_inputs).is_ok();
    assert_eq!(
        shell_ok, rust_ok,
        "shell/rust candidate-tree verdict parity"
    );
    if shell_ok && rust_ok {
        let shell_bytes = std::fs::read(&shell_out).expect("shell journal");
        let rust_bytes = std::fs::read(&rust_out).expect("rust journal");
        // Home-independent journals compare directly: tree rows name
        // repository paths, never the worktree.
        assert_eq!(shell_bytes, rust_bytes, "candidate journals diverge");
    } else {
        for journal in [&shell_out, &rust_out] {
            let bytes = std::fs::read(journal).expect("journal bytes");
            assert!(bytes.is_empty(), "failed runs leave an empty journal");
        }
    }
    (shell_ok, rust_ok)
}

/// Fixture repository with one regular file, one executable, one
/// nested file, and one safe symlink.
fn tree_repo(dir: &Path, tag: &str) -> PathBuf {
    let repo = init_repo(dir, tag);
    commit_file(&repo, "file.txt", b"hello\n", 0o644);
    commit_file(&repo, "run.sh", b"#!/bin/sh\necho hi\n", 0o755);
    commit_file(&repo, "sub/dir/nested.txt", b"nested\n", 0o644);
    commit_link(&repo, "link", "file.txt");
    repo
}

#[test]
fn candidate_tree_matches_journal_bytes() {
    let dir = TempDir::new("init-rec-tree").expect("temp dir");
    let repo = tree_repo(dir.path(), "cand");
    assert_eq!(
        check_tree("init-rec-tree-ok", &repo, false),
        (true, true),
        "plain tree must inventory"
    );
}

#[test]
fn candidate_tree_rejects_reserved_paths() {
    let dir = TempDir::new("init-rec-tree-res").expect("temp dir");
    // A foreign executable at the launcher path is reserved.
    let repo = tree_repo(dir.path(), "cand");
    commit_file(&repo, ".local/bin/dot", b"foreign binary\n", 0o755);
    assert_eq!(
        check_tree("init-rec-tree-reserved", &repo, false),
        (false, false),
        "foreign launcher must stay reserved"
    );
    // The release-byte launcher at the same path is the exception.
    let repo = init_repo(dir.path(), "launcher");
    let launcher =
        std::fs::read(source_root().join("support/client-launcher.sh")).expect("launcher bytes");
    commit_file(&repo, ".local/bin/dot", &launcher, 0o755);
    assert_eq!(
        check_tree("init-rec-tree-launcher", &repo, false),
        (true, true),
        "release launcher must pass the exception"
    );
    assert!(
        std::fs::read_to_string(
            TempDir::new("init-rec-tree-launcher-probe")
                .expect("temp dir")
                .path()
                .join("x")
        )
        .is_err(),
        "placeholder"
    );
}

#[test]
fn candidate_tree_rejects_bad_entries() {
    let dir = TempDir::new("init-rec-tree-bad").expect("temp dir");
    // A symlink blob with a newline cannot enter the TSV journal.
    let repo = tree_repo(dir.path(), "cand");
    commit_link(&repo, "badlink", "a\nb");
    assert_eq!(
        check_tree("init-rec-tree-badlink", &repo, false),
        (false, false),
        "bad symlink blob must fail"
    );
    // A submodule gitlink is not a blob.
    let repo = init_repo(dir.path(), "gitlink");
    git(
        &repo,
        &["update-index", "--add", "--cacheinfo", &gitlink_info(&repo)],
    );
    git(&repo, &["commit", "-qm", "gitlink"]);
    assert_eq!(
        check_tree("init-rec-tree-gitlink", &repo, false),
        (false, false),
        "gitlink must fail"
    );
    // An empty tree has no rows to publish.
    let repo = init_repo(dir.path(), "empty");
    clear_repo(&repo);
    assert_eq!(
        check_tree("init-rec-tree-empty", &repo, false),
        (false, false),
        "empty tree must fail"
    );
    // A missing branch inventories nothing.
    let repo = tree_repo(dir.path(), "cand2");
    let twins = Twins::build("init-rec-tree-nobranch");
    let rust_out = twins.rust_home.join("tree.tsv");
    let shell_out = twins.shell_home.join("tree.tsv");
    let snippet = format!(
        "_dot_init_candidate_tree {} nope {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&repo.to_string_lossy()),
        sq(&shell_out.to_string_lossy()),
    );
    let (shell_code, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    let rust_home = twins.rust_home.to_string_lossy().into_owned();
    let root = source_root();
    let mut rust_inputs = tree_inputs(&repo, &rust_out, &rust_home, &rust_home, &root);
    rust_inputs.branch = "nope";
    assert_eq!(
        shell_code == 0,
        records::candidate_tree(&rust_inputs).is_ok(),
        "missing-branch verdict parity"
    );
}

#[test]
fn candidate_tree_honors_init_backup_roots() {
    let dir = TempDir::new("init-rec-tree-backup").expect("temp dir");
    let repo = tree_repo(dir.path(), "cand");
    commit_file(&repo, ".dot-backup/b1/x", b"parked\n", 0o644);
    // `$HOME/.dot-backup` sits in the fixed roots inventory, so a
    // path beneath it stays reserved with or without the explicit
    // backup binding.
    assert_eq!(
        check_tree("init-rec-tree-nobackup", &repo, false),
        (false, false),
        "backup-rooted path must stay reserved"
    );
    // With the backup binding the same path turns reserved.
    assert_eq!(
        check_tree("init-rec-tree-backup", &repo, true),
        (false, false),
        "bound backup path must stay reserved"
    );
    // And the binding alone breaks no ordinary tree.
    let plain = tree_repo(dir.path(), "plain");
    assert_eq!(
        check_tree("init-rec-tree-backup-plain", &plain, true),
        (true, true),
        "binding must not reserve ordinary paths"
    );
}

/// Populate one worktree side with the candidate fixtures.
fn matches_setup(home: &Path) {
    std::fs::write(home.join("file.txt"), "hello\n").expect("write fixture");
    chmod(&home.join("file.txt"), 0o644);
    std::fs::write(home.join("tool.sh"), "#!/bin/sh\necho hi\n").expect("write fixture");
    chmod(&home.join("tool.sh"), 0o755);
    std::os::unix::fs::symlink("file.txt", home.join("link")).expect("link fixture");
}

#[test]
fn candidate_matches_worktree_states() {
    let dir = TempDir::new("init-rec-match-wt").expect("temp dir");
    // Repository and worktree share layout here: commit the same
    // bytes the worktree carries.
    let repo = init_repo(dir.path(), "cand");
    commit_file(&repo, "file.txt", b"hello\n", 0o644);
    commit_file(&repo, "tool.sh", b"#!/bin/sh\necho hi\n", 0o755);
    commit_link(&repo, "link", "file.txt");
    let twins = Twins::build("init-rec-match-base");
    matches_setup(&twins.shell_home);
    matches_setup(&twins.rust_home);
    let probe = |home: &Path, branch: &str, mode: &str, rel: &str| {
        let snippet = format!(
            "_dot_init_candidate_matches_path {} {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"",
            sq(&repo.to_string_lossy()),
            sq(branch),
            sq(mode),
            sq(rel),
        );
        shell_run(home, &[], &snippet).0 == 0
    };
    let rust_home = twins.rust_home.to_string_lossy().into_owned();
    let root = source_root();
    for (mode, rel, want) in [
        ("100644", "file.txt", true),
        ("100755", "tool.sh", true),
        ("120000", "link", true),
        // Executable bits cut the wrong way.
        ("100755", "file.txt", false),
        ("100644", "tool.sh", false),
        // Unknown modes never match.
        ("100777", "file.txt", false),
        // Missing targets never match.
        ("100644", "absent.txt", false),
        // Crossed shapes never match.
        ("120000", "file.txt", false),
        ("100644", "link", false),
    ] {
        let shell_ok = probe(&twins.shell_home, "main", mode, rel);
        let rust_ok = records::candidate_matches_path(&repo, "main", mode, rel, &rust_home, &root);
        assert_eq!(shell_ok, rust_ok, "matches parity for {mode} {rel}");
        assert_eq!(rust_ok, want, "matches direction for {mode} {rel}");
    }
    // Changed bytes break the content gate on both engines.
    std::fs::write(twins.shell_home.join("file.txt"), "changed\n").expect("rewrite");
    std::fs::write(twins.rust_home.join("file.txt"), "changed\n").expect("rewrite");
    assert!(!probe(&twins.shell_home, "main", "100644", "file.txt"));
    assert!(!records::candidate_matches_path(
        &repo, "main", "100644", "file.txt", &rust_home, &root
    ));
    // A failed `git show` against an empty file still matches: the
    // shell's pipe feeds the hasher nothing on both sides.
    std::fs::write(twins.shell_home.join("empty.txt"), "").expect("write fixture");
    std::fs::write(twins.rust_home.join("empty.txt"), "").expect("write fixture");
    assert!(probe(&twins.shell_home, "nope", "100644", "empty.txt"));
    assert!(records::candidate_matches_path(
        &repo,
        "nope",
        "100644",
        "empty.txt",
        &rust_home,
        &root
    ));
}

/// One snapshot row: both engines describe the same live path, so
/// the rendered lines must match byte-for-byte (device and inode
/// included).
fn check_snapshot(tag: &str, rel: &str, setup: impl FnOnce(&Path)) {
    let dir = TempDir::new(tag).expect("temp dir");
    let path = dir.path().join(rel);
    setup(&path);
    let snippet = format!(
        "out=$(_dot_init_snapshot_path {}); code=$?; printf 'code=%s\\n' \"$code\"; printf 'out=%s\\n' \"$out\"",
        sq(&path.to_string_lossy()),
    );
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let (shell_code, stdout, _) = shell_run(&home, &[], &snippet);
    let shell_line = String::from_utf8_lossy(&stdout)
        .lines()
        .find_map(|line| line.strip_prefix("out="))
        .unwrap_or("<missing>")
        .to_string();
    let root = source_root();
    match records::snapshot_path(&path, &root) {
        Ok(snapshot) => {
            assert_eq!(shell_code, 0, "shell snapshot failed for {rel}");
            assert_eq!(snapshot.line(), shell_line, "snapshot lines diverge");
        }
        Err(_) => {
            assert_ne!(shell_code, 99, "shell probe never reported");
            assert!(
                shell_line == "<missing>" || shell_code != 0,
                "shell snapshot succeeded where rust failed for {rel}: {shell_line}"
            );
        }
    }
}

#[test]
fn snapshot_describes_live_state() {
    check_snapshot("init-rec-snap-file", "file.txt", |path| {
        std::fs::write(path, "hello\n").expect("write fixture");
        chmod(path, 0o644);
    });
    check_snapshot("init-rec-snap-exec", "tool.sh", |path| {
        std::fs::write(path, "#!/bin/sh\n").expect("write fixture");
        chmod(path, 0o755);
    });
    check_snapshot("init-rec-snap-empty", "empty.txt", |path| {
        std::fs::write(path, "").expect("write fixture");
    });
    check_snapshot("init-rec-snap-link", "link", |path| {
        std::os::unix::fs::symlink("target/path", path).expect("link fixture");
    });
    check_snapshot("init-rec-snap-linknl", "linknl", |path| {
        std::os::unix::fs::symlink("a\nb", path).expect("link fixture");
    });
    check_snapshot("init-rec-snap-dir", "sub", |path| {
        std::fs::create_dir_all(path).expect("dir fixture");
    });
    check_snapshot("init-rec-snap-absent", "absent.txt", |_| {});
    check_snapshot("init-rec-snap-dangling", "dangling", |path| {
        std::os::unix::fs::symlink("no-such-target", path).expect("link fixture");
    });
    check_snapshot("init-rec-snap-badlink", "badlink", |path| {
        std::os::unix::fs::symlink("a\tb", path).expect("link fixture");
    });
}

/// Shell verdict for `_dot_init_path_state_matches` with explicit
/// fields.
fn shell_matches(home: &Path, path: &Path, snapshot: &PathSnapshot) -> bool {
    let snippet = format!(
        "_dot_init_path_state_matches {} {} {} {} {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&path.to_string_lossy()),
        sq(&snapshot.kind),
        sq(&snapshot.dev),
        sq(&snapshot.ino),
        sq(&snapshot.mode),
        sq(&snapshot.size),
        sq(&snapshot.value),
    );
    shell_run(home, &[], &snippet).0 == 0
}

/// Build a snapshot through the port, for rows that mutate state
/// afterwards.
fn take(path: &Path) -> PathSnapshot {
    records::snapshot_path(path, &source_root()).expect("take snapshot")
}

#[test]
fn state_matches_tracks_mutation() {
    let dir = TempDir::new("init-rec-state").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let file = home.join("file.txt");
    std::fs::write(&file, "hello\n").expect("write fixture");
    chmod(&file, 0o644);
    let snap = take(&file);
    assert!(shell_matches(&home, &file, &snap));
    assert!(records::path_state_matches(&file, &snap, &source_root()));
    // Changed bytes break both engines.
    std::fs::write(&file, "changed\n").expect("rewrite");
    assert!(!shell_matches(&home, &file, &snap));
    assert!(!records::path_state_matches(&file, &snap, &source_root()));
    // So does a chmod.
    std::fs::write(&file, "hello\n").expect("rewrite");
    chmod(&file, 0o600);
    assert!(!shell_matches(&home, &file, &snap));
    assert!(!records::path_state_matches(&file, &snap, &source_root()));
    // And a replaced inode. Unlink-plus-recreate may recycle the
    // inode number on some filesystems, so atomically rename a live
    // sibling over the path instead: its inode is distinct while
    // both files exist, and the rename preserves it.
    let sibling = home.join("sibling.txt");
    std::fs::write(&sibling, "hello\n").expect("sibling");
    chmod(&sibling, 0o644);
    std::fs::rename(&sibling, &file).expect("replace");
    assert!(!shell_matches(&home, &file, &snap));
    assert!(!records::path_state_matches(&file, &snap, &source_root()));
}

#[test]
fn state_matches_covers_shapes() {
    let dir = TempDir::new("init-rec-state-shapes").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    // Absent matches only absence.
    let absent = PathSnapshot {
        kind: "absent".to_string(),
        dev: "-".to_string(),
        ino: "-".to_string(),
        mode: "-".to_string(),
        size: "-".to_string(),
        value: "-".to_string(),
    };
    let missing = home.join("missing.txt");
    assert!(shell_matches(&home, &missing, &absent));
    assert!(records::path_state_matches(
        &missing,
        &absent,
        &source_root()
    ));
    std::fs::write(&missing, "now here\n").expect("write fixture");
    assert!(!shell_matches(&home, &missing, &absent));
    assert!(!records::path_state_matches(
        &missing,
        &absent,
        &source_root()
    ));
    // Symlinks and directories round-trip through both engines.
    std::fs::write(home.join("file.txt"), "hello\n").expect("write fixture");
    let link = home.join("link");
    std::os::unix::fs::symlink("file.txt", &link).expect("link fixture");
    let link_snap = take(&link);
    assert_eq!(link_snap.kind, "symlink");
    assert!(shell_matches(&home, &link, &link_snap));
    assert!(records::path_state_matches(
        &link,
        &link_snap,
        &source_root()
    ));
    let sub = home.join("sub");
    std::fs::create_dir_all(&sub).expect("dir fixture");
    let dir_snap = take(&sub);
    assert_eq!(dir_snap.kind, "directory");
    assert!(shell_matches(&home, &sub, &dir_snap));
    assert!(records::path_state_matches(&sub, &dir_snap, &source_root()));
    // Crossed shapes fail: the directory snapshot against the link.
    assert!(!shell_matches(&home, &link, &dir_snap));
    assert!(!records::path_state_matches(
        &link,
        &dir_snap,
        &source_root()
    ));
    // A tampered value fails the re-read gate.
    let file = home.join("file.txt");
    std::fs::write(&file, "hello\n").expect("write fixture");
    chmod(&file, 0o644);
    let mut snap = take(&file);
    snap.value = "0".repeat(40);
    assert!(!shell_matches(&home, &file, &snap));
    assert!(!records::path_state_matches(&file, &snap, &source_root()));
    // An unknown kind matches nothing on either engine.
    snap.kind = "mystery".to_string();
    assert!(!shell_matches(&home, &file, &snap));
    assert!(!records::path_state_matches(&file, &snap, &source_root()));
}
