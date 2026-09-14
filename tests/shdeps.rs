//! Differential parity tests for `src/shdeps.rs` against the live
//! shell (`lib/dot/providers/shdeps.sh`, part 1): the lock reader,
//! the origin allowlist, the ownership gate, the digest helper, and
//! the installer-hash predicate.
//!
//! Separate binary because each lock row needs its own
//! `DOT_SOURCE_ROOT` (the lock lives at
//! `$DOT_SOURCE_ROOT/support/shdeps.lock`), which the shell reads
//! from the environment while the port takes it as a parameter.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::test_support::TempDir;

/// Sources for the lock/trust family: the provider binds caller
/// policy at load but needs no other library for these functions.
const SOURCES: &str = ". \"$1/lib/dot/providers/shdeps.sh\"\n";

/// Pinned revision value for valid-lock fixtures.
const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Pinned digest value for valid-lock fixtures.
const INSTALL_SHA256: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Pinned ABI value for valid-lock fixtures.
const ABI: &str = "12";

/// Valid three-line lock body, newline-terminated.
fn valid_lock() -> Vec<u8> {
    format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}\n").into_bytes()
}

/// Run one shell snippet with the provider sourced. The locale
/// stays pinned like the `repos_pull_base` harness; only
/// `DOT_SOURCE_ROOT` points at the row fixture instead of the repo.
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

/// One row fixture: a directory serving as both `HOME` and
/// `DOT_SOURCE_ROOT`, with an optional lock body staged.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn build(tag: &str, lock: Option<&[u8]>) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let root = dir.path().to_path_buf();
        if let Some(body) = lock {
            let support = root.join("support");
            std::fs::create_dir_all(&support).expect("support dir");
            std::fs::write(support.join("shdeps.lock"), body).expect("lock fixture");
        }
        Fixture { _dir: dir, root }
    }
}

/// Check one `lock_value` row: shell rc plus the captured value
/// against the port (command substitution strips the shell's
/// trailing newline, so both sides compare bare values).
fn check_lock(tag: &str, lock: Option<&[u8]>, key: &str) {
    let fixture = Fixture::build(&format!("lock-{tag}"), lock);
    let snippet = format!(
        "if value=$(_dot_shdeps_lock_value {key}); then code=0; else code=$?; value=''; fi; \
         printf 'rc=%s\\nvalue=%s\\n' \"$code\" \"$value\"\n"
    );
    let (code, out, err) = shell_run(&fixture.root, &fixture.root, &snippet);
    assert_eq!(code, 0, "harness exit for lock {tag}");
    let shell_err = String::from_utf8(err).expect("shell warnings");
    if lock.is_none() {
        // The shell logs its failed lock redirection while the
        // port folds that refusal into `None` for its callers.
        assert!(
            shell_err.contains("shdeps.lock: No such file or directory"),
            "lock {tag} warns: {shell_err:?}"
        );
    } else {
        assert_eq!(shell_err, "", "lock {tag} is silent");
    }
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rust_out = match dot::shdeps::lock_value(&fixture.root, key) {
        Some(value) => format!("rc=0\nvalue={value}\n"),
        None => "rc=1\nvalue=\n".to_string(),
    };
    assert_eq!(rust_out, shell_out, "lock_value for {tag}");
}

#[test]
fn lock_value_rows_agree() {
    let valid = valid_lock();
    let rows: Vec<(&str, Option<Vec<u8>>, &str)> = vec![
        ("revision", Some(valid.clone()), "revision"),
        ("digest", Some(valid.clone()), "install_sha256"),
        ("abi", Some(valid.clone()), "abi"),
        ("bogus-key", Some(valid.clone()), "bogus"),
        ("missing", None, "revision"),
        (
            "two-lines",
            Some(format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\n").into_bytes()),
            "revision",
        ),
        (
            "four-lines",
            Some(
                format!(
                    "revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}\nextra=1\n"
                )
                .into_bytes(),
            ),
            "revision",
        ),
        ("empty", Some(b"".to_vec()), "revision"),
        (
            "bad-revision",
            Some(
                format!(
                    "revision={}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}\n",
                    "A".repeat(40)
                )
                .into_bytes(),
            ),
            "revision",
        ),
        (
            "bad-digest",
            Some(format!("revision={REVISION}\ninstall_sha256=abc\nabi={ABI}\n").into_bytes()),
            "install_sha256",
        ),
        (
            "bad-abi",
            Some(
                format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi=0\n")
                    .into_bytes(),
            ),
            "abi",
        ),
        (
            "swapped-order",
            Some(
                format!("abi={ABI}\nrevision={REVISION}\ninstall_sha256={INSTALL_SHA256}\n")
                    .into_bytes(),
            ),
            "revision",
        ),
        (
            "no-trailing-newline",
            Some(
                format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}")
                    .into_bytes(),
            ),
            "abi",
        ),
        (
            "crlf",
            Some(
                format!("revision={REVISION}\r\ninstall_sha256={INSTALL_SHA256}\r\nabi={ABI}\r\n")
                    .into_bytes(),
            ),
            "revision",
        ),
    ];
    for (tag, lock, key) in &rows {
        check_lock(tag, lock.as_deref(), key);
    }
}

/// Check one `origin_allowed` row: shell rc against the port.
fn check_origin(tag: &str, origin: &str) {
    let fixture = Fixture::build(&format!("origin-{tag}"), None);
    let snippet = format!(
        "if _dot_shdeps_origin_allowed {}; then code=0; else code=$?; fi; printf 'rc=%s\\n' \"$code\"\n",
        sq(origin)
    );
    let (code, out, err) = shell_run(&fixture.root, &fixture.root, &snippet);
    assert_eq!(code, 0, "harness exit for origin {tag}");
    assert_eq!(err, b"", "origin {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rc = if dot::shdeps::origin_allowed(origin) {
        0
    } else {
        1
    };
    assert_eq!(format!("rc={rc}\n"), shell_out, "origin_allowed for {tag}");
}

#[test]
fn origin_allowed_rows_agree() {
    for (tag, origin) in [
        ("https", "https://github.com/cgraf78/shdeps"),
        ("https-git", "https://github.com/cgraf78/shdeps.git"),
        ("scp", "git@github.com:cgraf78/shdeps"),
        ("scp-git", "git@github.com:cgraf78/shdeps.git"),
        ("ssh", "ssh://git@github.com/cgraf78/shdeps"),
        ("ssh-git", "ssh://git@github.com/cgraf78/shdeps.git"),
        ("http", "http://github.com/cgraf78/shdeps"),
        ("trailing-slash", "https://github.com/cgraf78/shdeps/"),
        ("suffixed", "https://github.com/cgraf78/shdeps.git/extra"),
        ("other-repo", "git@github.com:cgraf78/other.git"),
        ("uppercase", "HTTPS://github.com/cgraf78/shdeps"),
        ("empty", ""),
    ] {
        check_origin(tag, origin);
    }
}

/// Stage `name` under `root` with `mode`, returning its path.
fn stage_mode(root: &Path, name: &str, mode: u32) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, b"owned\n").expect("mode fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Check one `path_owned` row: shell rc against the port under the
/// real uid (foreign-uid refusal is a Rust-only unit below: `$EUID`
/// is readonly, so the shell side cannot bind a foreign caller).
fn check_owned(tag: &str, path: &Path) {
    let fixture = Fixture::build(&format!("owned-{tag}"), None);
    let snippet = format!(
        "if _dot_shdeps_path_owned {}; then code=0; else code=$?; fi; printf 'rc=%s\\n' \"$code\"\n",
        sq(&path.to_string_lossy())
    );
    let (code, out, err) = shell_run(&fixture.root, &fixture.root, &snippet);
    assert_eq!(code, 0, "harness exit for owned {tag}");
    assert_eq!(err, b"", "owned {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let euid = dot::temp::current_uid().expect("uid");
    let rc = if dot::shdeps::path_owned(path, euid) {
        0
    } else {
        1
    };
    assert_eq!(format!("rc={rc}\n"), shell_out, "path_owned for {tag}");
}

#[test]
fn path_owned_rows_agree() {
    let home = TempDir::new("owned-home").expect("fixture dir");
    let euid = dot::temp::current_uid().expect("uid");
    let clean = stage_mode(home.path(), "clean", 0o644);
    let locked = stage_mode(home.path(), "locked", 0o600);
    let group = stage_mode(home.path(), "group", 0o664);
    let other = stage_mode(home.path(), "other", 0o602);
    let missing = home.path().join("missing");
    let dir = home.path().join("dir");
    std::fs::create_dir(&dir).expect("dir fixture");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let link_clean = home.path().join("link-clean");
    std::os::unix::fs::symlink(&clean, &link_clean).expect("symlink");
    let link_group = home.path().join("link-group");
    std::os::unix::fs::symlink(&group, &link_group).expect("symlink");
    let dangling = home.path().join("dangling");
    std::os::unix::fs::symlink(home.path().join("nowhere"), &dangling).expect("symlink");
    // Sanity: the fixture really runs as its owner.
    assert!(dot::shdeps::path_owned(&clean, euid));
    for (tag, path) in [
        ("clean", clean.clone()),
        ("locked", locked),
        ("group", group),
        ("other", other),
        ("missing", missing),
        ("dir", dir),
        ("link-clean", link_clean),
        ("link-group", link_group),
        ("dangling", dangling),
    ] {
        check_owned(tag, &path);
    }
}

/// Ownership inequality needs no shell twin: the uid gate is one
/// comparison, and the shell side cannot fake `$EUID`.
#[test]
fn path_owned_rejects_foreign_uid() {
    let home = TempDir::new("owned-foreign").expect("fixture dir");
    let euid = dot::temp::current_uid().expect("uid");
    let clean = stage_mode(home.path(), "clean", 0o644);
    assert!(!dot::shdeps::path_owned(&clean, euid.wrapping_add(1)));
}

/// Check one `sha256_file` row with a present file: shell rc plus
/// digest against the port.
fn check_digest(tag: &str, bytes: &[u8]) {
    let home = TempDir::new(&format!("digest-{tag}")).expect("fixture dir");
    let path = home.path().join("payload.bin");
    std::fs::write(&path, bytes).expect("payload");
    let snippet = format!(
        "digest=$(_dot_shdeps_sha256 {}); code=$?; printf 'rc=%s\\ndigest=%s\\n' \"$code\" \"$digest\"\n",
        sq(&path.to_string_lossy())
    );
    let (code, out, err) = shell_run(home.path(), home.path(), &snippet);
    assert_eq!(code, 0, "harness exit for digest {tag}");
    assert_eq!(err, b"", "digest {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rust_out = match dot::shdeps::sha256_file(&path) {
        Some(digest) => format!("rc=0\ndigest={digest}\n"),
        None => "rc=1\ndigest=\n".to_string(),
    };
    assert_eq!(rust_out, shell_out, "sha256_file for {tag}");
}

#[test]
fn sha256_rows_agree() {
    check_digest("text", b"hello shdeps\n");
    check_digest("empty", b"");
    check_digest("binary", &[0, 1, 2, 250, 255, 10, 13]);
}

/// A missing file documents the one intentional divergence: the
/// shell pipeline prints an empty digest with success (`awk` masks
/// the failure) while the port reports `None`, which still refuses
/// through `installer_hash_matches` exactly like the shell.
#[test]
fn sha256_missing_file_divergence() {
    let home = TempDir::new("digest-missing").expect("fixture dir");
    let path = home.path().join("missing.bin");
    let snippet = format!(
        "digest=$(_dot_shdeps_sha256 {}); code=$?; printf 'rc=%s\\ndigest=%s\\n' \"$code\" \"$digest\"\n",
        sq(&path.to_string_lossy())
    );
    let (code, out, _) = shell_run(home.path(), home.path(), &snippet);
    assert_eq!(code, 0, "harness exit");
    assert_eq!(
        String::from_utf8(out).expect("shell dump"),
        "rc=0\ndigest=\n"
    );
    assert_eq!(dot::shdeps::sha256_file(&path), None);
}

/// Check one `installer_hash_matches` row: shell rc against the
/// port. The matching row pins the lock digest from the port's own
/// digest of the payload; the predicate comparison itself stays
/// fully differential.
fn check_installer_hash(tag: &str, lock: Option<&[u8]>, payload: Option<&[u8]>) {
    let fixture = Fixture::build(&format!("installer-{tag}"), lock);
    let path = fixture.root.join("install.sh");
    if let Some(bytes) = payload {
        std::fs::write(&path, bytes).expect("payload");
    }
    let snippet = format!(
        "if _dot_shdeps_installer_hash_matches {}; then code=0; else code=$?; fi; printf 'rc=%s\\n' \"$code\"\n",
        sq(&path.to_string_lossy())
    );
    let (code, out, err) = shell_run(&fixture.root, &fixture.root, &snippet);
    assert_eq!(code, 0, "harness exit for installer {tag}");
    assert_eq!(err, b"", "installer {tag} is silent");
    let shell_out = String::from_utf8(out).expect("shell dump");
    let rc = if dot::shdeps::installer_hash_matches(&fixture.root, &path) {
        0
    } else {
        1
    };
    assert_eq!(format!("rc={rc}\n"), shell_out, "installer_hash for {tag}");
}

#[test]
fn installer_hash_rows_agree() {
    let payload = b"#!/usr/bin/env bash\nprintf 'fixture installer\\n'\n";
    let home = TempDir::new("installer-digest").expect("fixture dir");
    let probe = home.path().join("probe.sh");
    std::fs::write(&probe, payload).expect("probe");
    let digest = dot::shdeps::sha256_file(&probe).expect("probe digest");
    let matching =
        format!("revision={REVISION}\ninstall_sha256={digest}\nabi={ABI}\n").into_bytes();
    let valid = valid_lock();
    let corrupt = format!("revision={REVISION}\ninstall_sha256={digest}\nabi=0\n").into_bytes();
    check_installer_hash("match", Some(&matching), Some(payload));
    check_installer_hash("mismatch", Some(&valid), Some(payload));
    check_installer_hash("missing-file", Some(&matching), None);
    check_installer_hash("corrupt-lock", Some(&corrupt), Some(payload));
}
