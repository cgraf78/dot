//! Differential parity tests for `src/shdeps.rs` (part 2) against the
//! live shell (`lib/dot/providers/shdeps.sh`): the revision gate,
//! the checkpoint path, the active revision reader, and the guard
//! record reader, writer, and consumer.
//!
//! Separate binary because the checkpoint rows stage state paths
//! under per-row `HOME` fixtures (the record resolves via
//! `dot_xdg_path state`), while the consume rows additionally need
//! per-row git checkouts for the active-revision binding.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::test_support::TempDir;

/// Sources for the checkpoint family: the provider plus the XDG,
/// log, and temp libraries its record layer calls into.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/providers/shdeps.sh\"\n",
);

/// Forty-hex `a` revision for fixtures.
const R40A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Forty-hex `b` revision for fixtures.
const R40B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Sixty-four-hex `c` digest-shaped revision for fixtures.
const R64C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// Run one shell snippet with the provider sourced. The locale
/// stays pinned like the `repos_pull_base` harness; `HOME` and
/// `DOT_SOURCE_ROOT` point at the row fixtures.
fn shell_run(home: &Path, source_root: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
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
        .env("DOT_SOURCE_ROOT", source_root)
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
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
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// Valid record bytes for `before` / `after`, newline-terminated.
fn record(before: &str, after: &str) -> Vec<u8> {
    format!("cgraf78 dot provider reexec checkpoint v1\nbefore={before}\nafter={after}\n")
        .into_bytes()
}

/// Write `bytes` to `root/name` with `mode`, returning its path.
fn stage_mode(root: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Check one `revision_valid` row: shell rc against the port.
fn check_revision(tag: &str, revision: &str) {
    let home = TempDir::new(&format!("ckpt-rev-{tag}")).expect("fixture dir");
    let snippet = format!(
        "if _dot_provider_revision_valid {}; then code=0; else code=$?; fi; printf 'rc=%s\\n' \"$code\"\n",
        sq(revision)
    );
    let (code, out, err) = shell_run(home.path(), home.path(), &snippet);
    assert_eq!(code, 0, "harness exit for revision {tag}");
    assert_eq!(err, b"", "revision {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rc = if dot::shdeps::revision_valid(revision) {
        0
    } else {
        1
    };
    assert_eq!(format!("rc={rc}\n"), shell_out, "revision_valid for {tag}");
}

#[test]
fn revision_valid_rows_agree() {
    let upper40: String = R40A.to_ascii_uppercase();
    let mixed40 = "aAbB00112233445566778899ccDDeeFF00112233".to_string();
    assert_eq!(mixed40.len(), 40);
    let upper64: String = R64C.to_ascii_uppercase();
    let rows: Vec<(&str, String)> = vec![
        ("lower40", R40A.to_string()),
        ("upper40", upper40),
        ("mixed40", mixed40),
        ("lower64", R64C.to_string()),
        ("upper64", upper64),
        ("len41", "a".repeat(41)),
        ("len63", "b".repeat(63)),
        ("len39", "a".repeat(39)),
        ("len65", "a".repeat(65)),
        ("empty", String::new()),
        ("nonhex40", format!("{}g", "a".repeat(39))),
        ("beyond-f", "G".repeat(40)),
        ("0x-prefix", format!("0x{}", "a".repeat(38))),
        ("spaces", " ".repeat(40)),
        ("trailing-space", format!("{R40A} ")),
    ];
    for (tag, revision) in &rows {
        check_revision(tag, revision);
    }
}

/// Check one `checkpoint_path` row: shell rc plus `REPLY` against
/// the port. `setup` runs first inside the snippet to bind
/// `XDG_STATE_HOME` / `HOME` overrides; the port takes the same
/// values as parameters, defaulting to the row fixture exactly
/// like the harness `HOME`.
fn check_path(tag: &str, setup: &str, xdg_state_home: &str, home: Option<&Path>) {
    let cwd = TempDir::new(&format!("ckpt-path-{tag}")).expect("fixture dir");
    let home = home.unwrap_or(cwd.path());
    let snippet = format!(
        "{setup}_dot_reexec_checkpoint_path; code=$?; printf 'rc=%s\\npath=%s\\n' \"$code\" \"$REPLY\"\n"
    );
    let (code, out, err) = shell_run(cwd.path(), cwd.path(), &snippet);
    assert_eq!(code, 0, "harness exit for path {tag}");
    assert_eq!(err, b"", "path {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rust_out = match dot::shdeps::checkpoint_path(xdg_state_home, &home.to_string_lossy()) {
        Some(path) => format!("rc=0\npath={}\n", path.display()),
        None => "rc=1\npath=\n".to_string(),
    };
    assert_eq!(rust_out, shell_out, "checkpoint_path for {tag}");
}

#[test]
fn checkpoint_path_rows_agree() {
    let root = PathBuf::from("/");
    let relative = PathBuf::from("relative");
    let rows: Vec<(&str, String, String, Option<PathBuf>)> = vec![
        ("default", String::new(), String::new(), None),
        (
            "xdg-abs",
            "export XDG_STATE_HOME=/srv/state; ".to_string(),
            "/srv/state".to_string(),
            None,
        ),
        (
            "xdg-root",
            "export XDG_STATE_HOME=/; ".to_string(),
            "/".to_string(),
            None,
        ),
        (
            "xdg-relative",
            "export XDG_STATE_HOME=rel; ".to_string(),
            "rel".to_string(),
            None,
        ),
        (
            "home-root",
            "export HOME=/; ".to_string(),
            String::new(),
            Some(root),
        ),
        (
            "unresolvable",
            "export HOME=relative; ".to_string(),
            String::new(),
            Some(relative),
        ),
    ];
    for (tag, setup, xdg, home_path) in &rows {
        check_path(tag, setup, xdg, home_path.as_deref());
    }
}

/// Initialize a git checkout with one commit, returning `HEAD`.
fn init_repo(root: &Path) -> String {
    git(root, &["init", "-q"]);
    std::fs::write(root.join("file.txt"), b"checkpoint fixture\n").expect("seed file");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "seed"]);
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("read HEAD");
    assert!(output.status.success(), "rev-parse HEAD");
    String::from_utf8(output.stdout)
        .expect("HEAD utf8")
        .trim_end_matches('\n')
        .to_string()
}

/// Check one `active_revision` row: shell output against the port.
/// `setup` runs first inside the snippet (for env-poisoning rows);
/// `root` is the checkout both sides read.
fn check_active(tag: &str, setup: &str, root: &Path) {
    let home = TempDir::new(&format!("ckpt-active-{tag}")).expect("fixture dir");
    let snippet = format!("{setup}rev=$(_dot_active_revision); printf 'rev=%s\\n' \"$rev\"\n");
    let (code, out, err) = shell_run(home.path(), root, &snippet);
    assert_eq!(code, 0, "harness exit for active {tag}");
    assert_eq!(err, b"", "active {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rust_out = format!("rev={}\n", dot::shdeps::active_revision(root));
    assert_eq!(rust_out, shell_out, "active_revision for {tag}");
}

#[test]
fn active_revision_rows_agree() {
    let repo = TempDir::new("ckpt-active-repo").expect("fixture dir");
    let head = init_repo(repo.path());
    assert!(dot::shdeps::revision_valid(&head), "HEAD is a revision");
    let plain = TempDir::new("ckpt-active-plain").expect("fixture dir");
    let missing = plain.path().join("missing");
    let file = stage_mode(plain.path(), "file.txt", b"not a repo\n", 0o644);
    // The poisoned row proves the sanitized binding: a caller
    // `GIT_DIR` override reaches neither engine.
    let rows: Vec<(&str, String, PathBuf)> = vec![
        ("repo", String::new(), repo.path().to_path_buf()),
        ("plain", String::new(), plain.path().to_path_buf()),
        ("missing", String::new(), missing),
        ("file", String::new(), file),
        (
            "poisoned",
            "export GIT_DIR=/nonexistent; ".to_string(),
            repo.path().to_path_buf(),
        ),
    ];
    for (tag, setup, root) in &rows {
        check_active(tag, setup, root);
    }
}

/// How a read row stages its checkpoint path.
enum ReadStage {
    /// Regular file with exact bytes and mode.
    File(Vec<u8>, u32),
    /// Symlink to a valid mode-600 record.
    LinkToValid,
    /// Symlink to a missing target.
    Dangling,
    /// Directory (mode 700).
    Dir,
    /// Nothing staged.
    Missing,
}

/// Check one `read_checkpoint` row: shell rc plus the
/// `DOT_PROVIDER_CHECKPOINT_AFTER` capture against the port (the
/// capture spells `unset` when the record refuses on either side).
fn check_read(tag: &str, stage: ReadStage) {
    let home = TempDir::new(&format!("ckpt-read-{tag}")).expect("fixture dir");
    let root = home.path();
    let target = root.join("checkpoint");
    match &stage {
        ReadStage::File(bytes, mode) => {
            stage_mode(root, "checkpoint", bytes, *mode);
        }
        ReadStage::LinkToValid => {
            let valid = stage_mode(root, "valid", &record(R40A, R40B), 0o600);
            std::os::unix::fs::symlink(&valid, &target).expect("symlink");
        }
        ReadStage::Dangling => {
            std::os::unix::fs::symlink(root.join("nowhere"), &target).expect("symlink");
        }
        ReadStage::Dir => {
            std::fs::create_dir(&target).expect("dir fixture");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }
        ReadStage::Missing => {}
    }
    let snippet = format!(
        "if _dot_provider_read_checkpoint {}; then code=0; else code=$?; fi; printf 'rc=%s\\nafter=%s\\n' \"$code\" \"${{DOT_PROVIDER_CHECKPOINT_AFTER-unset}}\"\n",
        sq(&target.to_string_lossy())
    );
    let (code, out, err) = shell_run(root, root, &snippet);
    assert_eq!(code, 0, "harness exit for read {tag}");
    assert_eq!(err, b"", "read {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rust_out = match dot::shdeps::read_checkpoint(&target) {
        Some(after) => format!("rc=0\nafter={after}\n"),
        None => "rc=1\nafter=unset\n".to_string(),
    };
    assert_eq!(rust_out, shell_out, "read_checkpoint for {tag}");
}

#[test]
fn read_checkpoint_rows_agree() {
    let upper40a: String = R40A.to_ascii_uppercase();
    let upper40b: String = R40B.to_ascii_uppercase();
    let mixed64 = "aAbB00112233445566778899ccDDeeFF00112233445566778899aAbB00112233";
    assert_eq!(mixed64.len(), 64);
    let valid = record(R40A, R40B);
    let oversize = {
        let mut bytes = valid.clone();
        bytes.extend(std::iter::repeat_n(b'x', 600));
        bytes
    };
    let rows: Vec<(&str, ReadStage)> = vec![
        ("valid", ReadStage::File(valid.clone(), 0o600)),
        (
            "upper",
            ReadStage::File(record(&upper40a, &upper40b), 0o600),
        ),
        (
            "mixed64",
            ReadStage::File(record(mixed64, R64C), 0o600),
        ),
        (
            "no-trailing-newline",
            ReadStage::File(valid[..valid.len() - 1].to_vec(), 0o600),
        ),
        (
            "crlf",
            ReadStage::File(
                format!(
                    "cgraf78 dot provider reexec checkpoint v1\r\nbefore={R40A}\r\nafter={R40B}\r\n"
                )
                .into_bytes(),
                0o600,
            ),
        ),
        ("bad-magic", ReadStage::File(record(R40A, R40B).iter().map(|b| if *b == b'v' {b'w'} else {*b}).collect(), 0o600)),
        (
            "swapped",
            ReadStage::File(
                format!("cgraf78 dot provider reexec checkpoint v1\nafter={R40B}\nbefore={R40A}\n")
                    .into_bytes(),
                0o600,
            ),
        ),
        (
            "two-lines",
            ReadStage::File(
                format!("cgraf78 dot provider reexec checkpoint v1\nbefore={R40A}\n").into_bytes(),
                0o600,
            ),
        ),
        (
            "dup-before",
            ReadStage::File(
                format!(
                    "cgraf78 dot provider reexec checkpoint v1\nbefore={R40A}\nbefore={R40A}\nafter={R40B}\n"
                )
                .into_bytes(),
                0o600,
            ),
        ),
        ("empty", ReadStage::File(b"".to_vec(), 0o600)),
        (
            "same",
            ReadStage::File(record(R40A, R40A), 0o600),
        ),
        // Raw values differ only by case, so the record still counts
        // as a change; the reported `after` is lowercased.
        (
            "same-diffcase",
            ReadStage::File(record(&upper40a, R40A), 0o600),
        ),
        (
            "bad-after",
            ReadStage::File(record(R40A, "xyz"), 0o600),
        ),
        (
            "bad-before",
            ReadStage::File(record("0", R40B), 0o600),
        ),
        (
            "empty-before",
            ReadStage::File(record("", R40B), 0o600),
        ),
        (
            "equals-in-value",
            ReadStage::File(record("ab=ab", R40B), 0o600),
        ),
        (
            "mode-644",
            ReadStage::File(record(R40A, R40B), 0o644),
        ),
        (
            "mode-400",
            ReadStage::File(record(R40A, R40B), 0o400),
        ),
        (
            "mode-660",
            ReadStage::File(record(R40A, R40B), 0o660),
        ),
        ("oversize", ReadStage::File(oversize, 0o600)),
        ("dir", ReadStage::Dir),
        ("link", ReadStage::LinkToValid),
        ("dangling", ReadStage::Dangling),
        ("missing", ReadStage::Missing),
    ];
    for (tag, stage) in rows {
        check_read(tag, stage);
    }
}

/// How a write row pre-stages the record path (`None` leaves it absent).
#[derive(Clone)]
enum WritePre {
    /// Regular file with exact bytes and mode.
    File(Vec<u8>, u32),
    /// Symlink to a scratch target.
    Link,
    /// Directory (mode 700).
    Dir,
}

/// Snapshot of the record path after a write attempt: its kind,
/// file bytes, and permission bits.
#[derive(Debug, PartialEq, Eq)]
struct WriteAfter {
    kind: String,
    bytes: Option<Vec<u8>>,
    mode: Option<u32>,
}

/// Count stray sibling temps in `dir` (a publish must leave none;
/// a never-created parent counts as none).
fn stray_tmps(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count()
}

/// Snapshot the record path plus its parent for leftover temps.
fn snapshot_write(path: &Path) -> (WriteAfter, usize) {
    let after = match std::fs::symlink_metadata(path) {
        Err(_) => WriteAfter {
            kind: "missing".to_string(),
            bytes: None,
            mode: None,
        },
        Ok(meta) => {
            let kind = if meta.file_type().is_symlink() {
                "link"
            } else if meta.is_dir() {
                "dir"
            } else if meta.is_file() {
                "file"
            } else {
                "other"
            };
            let bytes = if meta.file_type().is_symlink() || !meta.is_file() {
                None
            } else {
                Some(std::fs::read(path).expect("read record"))
            };
            WriteAfter {
                kind: kind.to_string(),
                bytes,
                mode: Some(meta.mode() & 0o7777),
            }
        }
    };
    let strays = path.parent().map(stray_tmps).unwrap_or(0);
    (after, strays)
}

/// Check one `write_checkpoint` row with twin fixtures: shell rc
/// plus the post-write snapshot against the port. Both sides
/// resolve the record path from their own `HOME`, so only the
/// observable outcome (never the absolute path) is compared.
fn check_write(tag: &str, before: &str, after: &str, pre: Option<WritePre>) {
    let stage = |root: &Path| -> PathBuf {
        if let Some(shape) = &pre {
            let target = root.join(".local/state/dot/provider-reexec-failed");
            match shape {
                WritePre::File(bytes, mode) => {
                    stage_mode(
                        root,
                        ".local/state/dot/provider-reexec-failed",
                        bytes,
                        *mode,
                    );
                }
                WritePre::Link => {
                    let valid = stage_mode(root, "valid", &record(R40A, R40B), 0o600);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).expect("fixture parents");
                    }
                    std::os::unix::fs::symlink(&valid, &target).expect("symlink");
                }
                WritePre::Dir => {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).expect("fixture parents");
                    }
                    std::fs::create_dir(&target).expect("dir fixture");
                }
            }
        }
        root.join(".local/state/dot/provider-reexec-failed")
    };
    let shell_home = TempDir::new(&format!("ckpt-write-sh-{tag}")).expect("fixture dir");
    let shell_path = stage(shell_home.path());
    let snippet = format!(
        "if _dot_provider_write_checkpoint {} {}; then code=0; else code=$?; fi; printf 'rc=%s\\n' \"$code\"\n",
        sq(before),
        sq(after)
    );
    let (code, out, err) = shell_run(shell_home.path(), shell_home.path(), &snippet);
    assert_eq!(code, 0, "harness exit for write {tag}");
    assert_eq!(err, b"", "write {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let (shell_after, shell_strays) = snapshot_write(&shell_path);

    let rust_home = TempDir::new(&format!("ckpt-write-rs-{tag}")).expect("fixture dir");
    let rust_path = stage(rust_home.path());
    let mut moves = dot::temp::MoveCache::default();
    let rust_rc = if dot::shdeps::write_checkpoint(before, after, &rust_path, &mut moves) {
        0
    } else {
        1
    };
    let (rust_after, rust_strays) = snapshot_write(&rust_path);

    assert_eq!(format!("rc={rust_rc}\n"), shell_out, "write rc for {tag}");
    assert_eq!(rust_after, shell_after, "write outcome for {tag}");
    assert_eq!(rust_strays, shell_strays, "write leftovers for {tag}");
    assert_eq!(rust_strays, 0, "write {tag} leaves no sibling temps");
    if rust_rc == 0 {
        let expected = record(&before.to_ascii_lowercase(), &after.to_ascii_lowercase());
        assert_eq!(rust_after.bytes.as_deref(), Some(expected.as_slice()));
        assert_eq!(rust_after.mode, Some(0o600));
    }
}

#[test]
fn write_checkpoint_rows_agree() {
    let upper40a: String = R40A.to_ascii_uppercase();
    let upper40b: String = R40B.to_ascii_uppercase();
    let valid = record(R40A, R40B);
    let rows: Vec<(&str, String, String, Option<WritePre>)> = vec![
        ("fresh", R40A.to_string(), R40B.to_string(), None),
        ("upper", upper40a.clone(), upper40b.clone(), None),
        (
            "mixed64",
            "aAbB00112233445566778899ccDDeeFF00112233".to_string(),
            R64C.to_string(),
            None,
        ),
        ("same", R40A.to_string(), R40A.to_string(), None),
        // Lowercased first, so mixed-case spellings of one revision
        // refuse without staging anything.
        ("same-diffcase", upper40a, R40A.to_string(), None),
        ("bad-before", "xyz".to_string(), R40B.to_string(), None),
        ("bad-after", R40A.to_string(), String::new(), None),
        (
            "occupied-file",
            R40A.to_string(),
            R40B.to_string(),
            Some(WritePre::File(valid.clone(), 0o600)),
        ),
        (
            "occupied-empty",
            R40A.to_string(),
            R40B.to_string(),
            Some(WritePre::File(b"".to_vec(), 0o644)),
        ),
        (
            "occupied-link",
            R40A.to_string(),
            R40B.to_string(),
            Some(WritePre::Link),
        ),
        (
            "occupied-dir",
            R40A.to_string(),
            R40B.to_string(),
            Some(WritePre::Dir),
        ),
    ];
    for (tag, before, after, pre) in &rows {
        check_write(tag, before, after, pre.clone());
    }
}

/// One consume scenario. Record bytes derive from each side's own
/// `HEAD` (twin checkouts hash differently), so rows name a
/// strategy rather than fixed bytes.
#[derive(Clone, Copy)]
enum ConsumeKind {
    /// No record staged: consumption succeeds trivially.
    Absent,
    /// `after` equals `HEAD`: the record is consumed.
    Match,
    /// Uppercase `after` spelling of `HEAD`: still consumed, since
    /// the reader lowercases before comparing.
    UpperMatch,
    /// `after` names another revision: the record survives.
    Mismatch,
    /// Corrupt magic line: the record survives.
    Malformed,
    /// `before` equals `after`: the record survives.
    Same,
    /// Matching content at mode 644: the record survives.
    Mode644,
    /// Symlink to a matching record: the record survives.
    Link,
    /// Dangling symlink: nothing is removed.
    Dangling,
    /// Matching record, but the source root is not a checkout: the
    /// empty active revision matches nothing.
    Detached,
}

/// Outcome of one consume side: rc plus record survival.
#[derive(Debug, PartialEq, Eq)]
struct ConsumeAfter {
    rc: i32,
    kept: bool,
}

/// Stage one consume side at `home` for `kind`, returning the
/// record path plus the source root the side validates against
/// (`plain` backs the detached row).
fn stage_consume_side(home: &TempDir, plain: &TempDir, kind: ConsumeKind) -> (PathBuf, PathBuf) {
    let root = home.path();
    let head = init_repo(root);
    let target = root.join(".local/state/dot/provider-reexec-failed");
    let source = match kind {
        ConsumeKind::Detached => plain.path().to_path_buf(),
        _ => root.to_path_buf(),
    };
    let name = ".local/state/dot/provider-reexec-failed";
    match kind {
        ConsumeKind::Absent => {}
        ConsumeKind::Match | ConsumeKind::Detached => {
            stage_mode(root, name, &record(R40A, &head), 0o600);
        }
        ConsumeKind::UpperMatch => {
            stage_mode(root, name, &record(R40A, &head.to_ascii_uppercase()), 0o600);
        }
        ConsumeKind::Mismatch => {
            stage_mode(root, name, &record(R40A, R40B), 0o600);
        }
        ConsumeKind::Malformed => {
            stage_mode(root, name, b"not a checkpoint\n", 0o600);
        }
        ConsumeKind::Same => {
            stage_mode(root, name, &record(&head, &head), 0o600);
        }
        ConsumeKind::Mode644 => {
            stage_mode(root, name, &record(R40A, &head), 0o644);
        }
        ConsumeKind::Link => {
            let valid = stage_mode(root, "valid", &record(R40A, &head), 0o600);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("fixture parents");
            }
            std::os::unix::fs::symlink(&valid, &target).expect("symlink");
        }
        ConsumeKind::Dangling => {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("fixture parents");
            }
            std::os::unix::fs::symlink(root.join("nowhere"), &target).expect("symlink");
        }
    }
    (target, source)
}

/// Check one `consume_checkpoint` row with twin checkouts: shell rc
/// plus record survival against the port. Success rows are silent
/// on the shell side; failure rows must carry a checkpoint warning
/// (proving the refusal came from the record layer, not the
/// harness — exact text embeds the side-specific path, so only its
/// stable middle is pinned).
fn check_consume(tag: &str, kind: ConsumeKind) {
    let shell_home = TempDir::new(&format!("ckpt-consume-sh-{tag}")).expect("fixture dir");
    let shell_plain = TempDir::new(&format!("ckpt-consume-shp-{tag}")).expect("fixture dir");
    let (shell_target, shell_source) = stage_consume_side(&shell_home, &shell_plain, kind);
    let snippet = "if _dot_provider_consume_checkpoint; then code=0; else code=$?; fi; printf 'rc=%s\\n' \"$code\"\n";
    let (code, out, err) = shell_run(shell_home.path(), &shell_source, snippet);
    assert_eq!(code, 0, "harness exit for consume {tag}");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let shell_rc: i32 = shell_out
        .strip_prefix("rc=")
        .and_then(|rest| rest.strip_suffix('\n'))
        .and_then(|digits| digits.parse().ok())
        .expect("shell rc line");
    let shell_after = ConsumeAfter {
        rc: shell_rc,
        kept: shell_target.symlink_metadata().is_ok(),
    };

    let rust_home = TempDir::new(&format!("ckpt-consume-rs-{tag}")).expect("fixture dir");
    let rust_plain = TempDir::new(&format!("ckpt-consume-rsp-{tag}")).expect("fixture dir");
    let (rust_target, rust_source) = stage_consume_side(&rust_home, &rust_plain, kind);
    let rust_rc = if dot::shdeps::consume_checkpoint(&rust_target, &rust_source) {
        0
    } else {
        1
    };
    let rust_after = ConsumeAfter {
        rc: rust_rc,
        kept: rust_target.symlink_metadata().is_ok(),
    };

    assert_eq!(rust_after, shell_after, "consume outcome for {tag}");
    if shell_after.rc == 0 {
        assert_eq!(err, b"", "consume {tag} is silent");
    } else {
        let warnings = String::from_utf8_lossy(&err);
        assert!(
            warnings.contains("provider re-exec checkpoint"),
            "consume {tag} warns: {warnings:?}"
        );
    }
}

#[test]
fn consume_checkpoint_rows_agree() {
    for (tag, kind) in [
        ("absent", ConsumeKind::Absent),
        ("match", ConsumeKind::Match),
        ("upper-match", ConsumeKind::UpperMatch),
        ("mismatch", ConsumeKind::Mismatch),
        ("malformed", ConsumeKind::Malformed),
        ("same", ConsumeKind::Same),
        ("mode-644", ConsumeKind::Mode644),
        ("link", ConsumeKind::Link),
        ("dangling", ConsumeKind::Dangling),
        ("detached", ConsumeKind::Detached),
    ] {
        check_consume(tag, kind);
    }
}
