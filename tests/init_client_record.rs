//! Differential parity tests for the init transaction record
//! journal (`lib/dot/init-client.sh`, the record family) against
//! the live shell: the journal publisher, the journal validator,
//! the phase advance, and the parent-intent and prior-snapshot
//! readers.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so
//! sibling temps and journal paths never collide. Journal bytes
//! embed the home, so cross-engine byte compares normalize the
//! rust home to the shell home first; the source revision comes
//! from the same manifest checkout on both sides and compares
//! directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_record as record;
use dot::init_client_record::{RecordFields, TransactionRecord};
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the record chapter: the shared temp helpers
/// (sibling temps, sanitized git, stdin hashing) and the init
/// client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixed run nonce for every row.
const NONCE: &str = "test-nonce-54";
/// Fixed 40-nibble commit for journal rows.
const COMMIT40: &str = "0123456789abcdef0123456789abcdef01234567";
/// Fixed 64-nibble commit proving the long form reads.
const COMMIT64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Fixed source revision for hand-crafted journals.
const REVISION40: &str = "abcdef0123456789abcdef0123456789abcdef01";
/// Fixed dot binary path for every row.
const DOT_BIN: &str = "/usr/local/bin/dot";

/// Run one shell snippet with the init runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that
/// verdict — not the process status, which only says the printer
/// ran. A snippet that never reports (a harness bug, never a pass)
/// yields 99.
///
/// The locale stays pinned: git diagnostics must read English on
/// both engines, and the port pins `LC_ALL=C` around every git
/// run. Run identity crosses as explicit environment entries,
/// mirroring how the engine exports it before calling into this
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

/// The run-identity environment for journal rows: commit, nonce,
/// device, inode, and dot binary.
fn record_env() -> [(&'static str, &'static str); 5] {
    [
        ("DOT_INIT_COMMIT", COMMIT40),
        ("DOT_INIT_NONCE", NONCE),
        ("DOT_INIT_GIT_DEV", "11"),
        ("DOT_INIT_GIT_INO", "22"),
        ("DOT_BIN", DOT_BIN),
    ]
}

/// The crate root backing the revision subprocesses.
fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so sibling temps and journal
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

    /// Home-relative text of one home, for journal crafting.
    fn shell_text(&self) -> String {
        self.shell_home.to_string_lossy().into_owned()
    }

    /// Home-relative text of one home, for journal crafting.
    fn rust_text(&self) -> String {
        self.rust_home.to_string_lossy().into_owned()
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

/// Replace every `from` byte run with `to`: journals embed the
/// home, so rust-side bytes normalize to the shell home before
/// comparing. The twin names share only their parent, so the
/// replacement never nests.
fn normalize(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let (probe, replacement) = (from.as_bytes(), to.as_bytes());
    let mut cursor = 0;
    while cursor + probe.len() <= out.len() {
        if out[cursor..cursor + probe.len()] == *probe {
            out.splice(cursor..cursor + probe.len(), replacement.iter().copied());
            cursor += replacement.len();
        } else {
            cursor += 1;
        }
    }
    out
}

/// Canonical fourteen-line journal for one home: every gate on
/// the happy path, so mutations start here. Thirteen fields ride
/// one bundle each, hence the arity.
#[allow(clippy::too_many_arguments)]
fn journal_bytes(
    home: &str,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    commit: &str,
    git_dir: &str,
    backup: &str,
    dot: &str,
    revision: &str,
    nonce: &str,
    dev: &str,
    ino: &str,
) -> Vec<u8> {
    format!(
        "cgraf78 dot initialization transaction v1\n\
         phase={phase}\norigin={origin}\nidentity={identity}\nbranch={branch}\n\
         commit={commit}\ngit_dir={git_dir}\nworktree={home}\nbackup={backup}\n\
         dot={dot}\ndot_revision={revision}\nnonce={nonce}\n\
         git_dev={dev}\ngit_ino={ino}\n"
    )
    .into_bytes()
}

/// The standard valid journal for one home.
fn valid_journal(home: &str) -> Vec<u8> {
    journal_bytes(
        home,
        "prepared",
        "git://example.com/owner/repo",
        "git://example.com/owner/repo",
        "main",
        COMMIT40,
        &format!("{home}/.dotfiles"),
        &format!("{home}/.dot-backup/20240101"),
        DOT_BIN,
        REVISION40,
        NONCE,
        "-",
        "-",
    )
}

/// Write one valid journal per side, at mode 600 like the
/// publisher leaves them.
fn craft_valid(twins: &Twins, name: &str) -> (PathBuf, PathBuf) {
    let shell_file = twins.shell_home.join(name);
    let rust_file = twins.rust_home.join(name);
    std::fs::write(&shell_file, valid_journal(&twins.shell_text())).expect("shell journal");
    std::fs::write(&rust_file, valid_journal(&twins.rust_text())).expect("rust journal");
    chmod(&shell_file, 0o600);
    chmod(&rust_file, 0o600);
    (shell_file, rust_file)
}

/// Shell probe for `_dot_init_read_record`: prints the verdict
/// plus the thirteen globals, so success rows compare field for
/// field and failure rows compare the verdict only (the shell
/// leaves partial globals behind on failure while the port
/// returns no record at all).
fn read_snippet(file: &Path) -> String {
    // `${VAR-}` spells the globals nounset-proof: the failure
    // paths under test leave some of them unset, and the verdict
    // printer must still run.
    format!(
        "_dot_init_read_record {}; code=$?; printf 'code=%s\\nphase=%s\\norigin=%s\\nidentity=%s\\nbranch=%s\\ncommit=%s\\ngit_dir=%s\\nworktree=%s\\nbackup=%s\\ndot=%s\\ndot_revision=%s\\nnonce=%s\\ngit_dev=%s\\ngit_ino=%s\\n' \"$code\" \"${{DOT_INIT_PHASE-}}\" \"${{DOT_INIT_ORIGIN-}}\" \"${{DOT_INIT_IDENTITY-}}\" \"${{DOT_INIT_BRANCH-}}\" \"${{DOT_INIT_COMMIT-}}\" \"${{DOT_INIT_GIT_DIR-}}\" \"${{DOT_INIT_WORKTREE-}}\" \"${{DOT_INIT_BACKUP-}}\" \"${{DOT_INIT_DOT-}}\" \"${{DOT_INIT_DOT_REVISION-}}\" \"${{DOT_INIT_NONCE-}}\" \"${{DOT_INIT_GIT_DEV-}}\" \"${{DOT_INIT_GIT_INO-}}\"",
        sq(&file.to_string_lossy())
    )
}

/// Serialize one validated record exactly like the shell probe
/// prints it, for byte comparison after home normalization.
fn serialize(record: &TransactionRecord) -> String {
    format!(
        "code=0\nphase={}\norigin={}\nidentity={}\nbranch={}\ncommit={}\ngit_dir={}\nworktree={}\nbackup={}\ndot={}\ndot_revision={}\nnonce={}\ngit_dev={}\ngit_ino={}\n",
        record.phase,
        record.origin,
        record.identity,
        record.branch,
        record.commit,
        record.git_dir,
        record.worktree,
        record.backup,
        record.dot,
        record.dot_revision,
        record.nonce,
        record.git_dev,
        record.git_ino,
    )
}

/// Read one journal through both engines and compare the full
/// probe output: verdict plus every field on success, verdict
/// only on failure.
fn check_read(tag: &str, name: &str, mutate: impl Fn(&str, Vec<u8>) -> Vec<u8>) {
    let twins = Twins::build(tag);
    let (shell_file, rust_file) = craft_valid(&twins, name);
    let shell_bytes = mutate(
        &twins.shell_text(),
        std::fs::read(&shell_file).expect("shell journal"),
    );
    let rust_bytes = mutate(
        &twins.rust_text(),
        std::fs::read(&rust_file).expect("rust journal"),
    );
    std::fs::write(&shell_file, &shell_bytes).expect("rewrite shell");
    std::fs::write(&rust_file, &rust_bytes).expect("rewrite rust");
    chmod(&shell_file, 0o600);
    chmod(&rust_file, 0o600);
    let (shell_code, shell_out, _) = shell_run(&twins.shell_home, &[], &read_snippet(&shell_file));
    assert_ne!(shell_code, 99, "probe printed no verdict for {tag}");
    let rust = record::read_record(&rust_file, &twins.rust_home);
    match rust {
        Ok(found) => {
            let normalized = normalize(
                serialize(&found).as_bytes(),
                &twins.rust_text(),
                &twins.shell_text(),
            );
            assert_eq!(shell_code, 0, "shell accepts for {tag}");
            assert_eq!(
                String::from_utf8_lossy(&shell_out).into_owned(),
                String::from_utf8_lossy(&normalized).into_owned(),
                "read fields for {tag}"
            );
        }
        Err(_) => {
            assert_eq!(shell_code, 1, "shell rejects for {tag}");
        }
    }
}

/// Shell probe for `_dot_init_write_record`, with an optional
/// seventh (git directory) argument.
fn write_snippet(
    file: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    git_dir: Option<&str>,
) -> String {
    let mut call = format!(
        "_dot_init_write_record {} {phase} {} {} {branch} {}",
        sq(&file.to_string_lossy()),
        sq(origin),
        sq(identity),
        sq(backup),
    );
    if let Some(dir) = git_dir {
        call.push(' ');
        call.push_str(&sq(dir));
    }
    format!("{call}; code=$?; printf 'code=%s\\n' \"$code\"")
}

/// The shared field bundle for journal rows under one home: full
/// identity by default, matching [`record_env`].
#[allow(clippy::too_many_arguments)]
fn fields_for<'a>(
    home: &'a Path,
    source_root: &'a Path,
    backup: &'a str,
    git_dir: Option<&'a Path>,
    commit: Option<&'a str>,
    nonce: Option<&'a str>,
    dev: Option<&'a str>,
    ino: Option<&'a str>,
) -> RecordFields<'a> {
    RecordFields {
        origin: "git://example.com/owner/repo",
        identity: "git://example.com/owner/repo",
        branch: "main",
        backup,
        git_dir,
        commit,
        nonce,
        git_dev: dev,
        git_ino: ino,
        dot_bin: DOT_BIN,
        home,
        source_root,
    }
}

#[test]
fn write_record_creates_journal() {
    let twins = Twins::build("init-record-write-new");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env = record_env();
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &write_snippet(
            &shell_file,
            "prepared",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            &shell_backup,
            None,
        ),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "prepared", &fields, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "create verdicts");
    let rust_bytes = std::fs::read(&rust_file).expect("rust bytes");
    let normalized = normalize(&rust_bytes, &twins.rust_text(), &twins.shell_text());
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        normalized,
        "created bytes agree"
    );
    assert_eq!(mode_of(&shell_file), 0o600, "shell mode");
    assert_eq!(mode_of(&rust_file), 0o600, "rust mode");
    // The journal round-trips through the validator on both sides.
    let (reread_code, _, _) = shell_run(&twins.shell_home, &[], &read_snippet(&shell_file));
    assert_eq!(reread_code, 0, "shell rereads its journal");
    let found = record::read_record(&rust_file, &twins.rust_home).expect("rust rereads");
    assert_eq!(found.phase, "prepared");
    assert_eq!(found.nonce, NONCE);
    assert_eq!(found.git_dev, "11");
    assert_eq!(found.git_ino, "22");
}

#[test]
fn write_record_defaults() {
    let twins = Twins::build("init-record-write-defaults");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env: [(&str, &str); 1] = [("DOT_BIN", DOT_BIN)];
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &write_snippet(
            &shell_file,
            "backing-up",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            &shell_backup,
            None,
        ),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        None,
        None,
        None,
        None,
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "backing-up", &fields, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "default verdicts");
    let rust_bytes = std::fs::read(&rust_file).expect("rust bytes");
    let normalized = normalize(&rust_bytes, &twins.rust_text(), &twins.shell_text());
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        normalized,
        "default bytes agree"
    );
    let text = String::from_utf8_lossy(&normalized).into_owned();
    assert!(
        text.contains(&format!("commit={}\n", record::ZERO_COMMIT)),
        "zero commit default"
    );
    assert!(text.contains("nonce=legacy\n"), "legacy nonce default");
    assert!(
        text.contains("git_dev=-\ngit_ino=-\n"),
        "dash device default"
    );
}

#[test]
fn write_record_explicit_git_dir() {
    let twins = Twins::build("init-record-write-gitdir");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    let shell_git = format!("{}/.git", twins.shell_text());
    let rust_git = format!("{}/.git", twins.rust_text());
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env = record_env();
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &write_snippet(
            &shell_file,
            "prepared",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            &shell_backup,
            Some(&shell_git),
        ),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let rust_git_path = PathBuf::from(&rust_git);
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        Some(&rust_git_path),
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "prepared", &fields, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "git-dir verdicts");
    let normalized = normalize(
        &std::fs::read(&rust_file).expect("rust bytes"),
        &twins.rust_text(),
        &twins.shell_text(),
    );
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        normalized,
        "explicit git-dir bytes agree"
    );
}

#[test]
fn write_record_replaces_live_file() {
    let twins = Twins::build("init-record-write-replace");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    std::fs::write(&shell_file, b"live\n").expect("shell live");
    std::fs::write(&rust_file, b"live\n").expect("rust live");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env = record_env();
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &write_snippet(
            &shell_file,
            "prepared",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            &shell_backup,
            None,
        ),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "prepared", &fields, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "replace verdicts");
    let normalized = normalize(
        &std::fs::read(&rust_file).expect("rust bytes"),
        &twins.rust_text(),
        &twins.shell_text(),
    );
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        normalized,
        "replaced bytes agree"
    );
}

#[test]
fn write_record_directory_destination_fails() {
    let twins = Twins::build("init-record-write-isdir");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    std::fs::create_dir_all(&shell_file).expect("shell dir");
    std::fs::create_dir_all(&rust_file).expect("rust dir");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env = record_env();
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &write_snippet(
            &shell_file,
            "prepared",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            &shell_backup,
            None,
        ),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "prepared", &fields, &mut cache);
    assert_ne!(shell_code, 0, "shell refuses a directory");
    assert!(rust.is_err(), "rust refuses a directory");
    assert!(shell_file.is_dir(), "shell keeps the directory");
    assert!(rust_file.is_dir(), "rust keeps the directory");
}

#[test]
fn write_record_missing_source_revision_fails() {
    let twins = Twins::build("init-record-write-norev");
    let empty = twins.shell_home.join("empty-source");
    std::fs::create_dir_all(&empty).expect("empty source");
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(&empty)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init fixture");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let empty_text = empty.to_string_lossy().into_owned();
    let env: [(&str, &str); 6] = [
        ("DOT_INIT_COMMIT", COMMIT40),
        ("DOT_INIT_NONCE", NONCE),
        ("DOT_INIT_GIT_DEV", "11"),
        ("DOT_INIT_GIT_INO", "22"),
        ("DOT_BIN", DOT_BIN),
        ("DOT_SOURCE_ROOT", empty_text.as_str()),
    ];
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &write_snippet(
            &shell_file,
            "prepared",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            &shell_backup,
            None,
        ),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let fields = fields_for(
        &twins.rust_home,
        &empty,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "prepared", &fields, &mut cache);
    assert_ne!(shell_code, 0, "shell fails without HEAD");
    assert!(rust.is_err(), "rust fails without HEAD");
}

#[test]
fn write_record_umask_077_stays_600() {
    let twins = Twins::build("init-record-write-umask");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env = record_env();
    let mut snippet = String::from("umask 077; ");
    snippet.push_str(&write_snippet(
        &shell_file,
        "prepared",
        "git://example.com/owner/repo",
        "git://example.com/owner/repo",
        "main",
        &shell_backup,
        None,
    ));
    let (shell_code, _, _) = shell_run(&twins.shell_home, &env, &snippet);
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::write_record(&rust_file, "prepared", &fields, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "umask verdicts");
    assert_eq!(mode_of(&shell_file), 0o600, "shell mode under umask 077");
    assert_eq!(mode_of(&rust_file), 0o600, "rust mode under umask 077");
}

#[test]
fn read_record_round_trip() {
    check_read("init-record-read-ok", "record", |_, bytes| bytes);
}

#[test]
fn read_record_all_phases() {
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
            &format!("init-record-phase-{phase}"),
            "record",
            |_, bytes| {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                text.replacen("phase=prepared\n", &format!("phase={phase}\n"), 1)
                    .into_bytes()
            },
        );
    }
}

/// One journal mutation: per-side home plus the current bytes.
type Mutate = fn(&str, Vec<u8>) -> Vec<u8>;

#[test]
fn read_record_rejects_malformed_shapes() {
    // Each mutation keeps fourteen lines of plausible bytes unless
    // the case says otherwise; both engines must refuse.
    let cases: &[(&str, Mutate)] = &[
        ("header", |_, mut bytes| {
            bytes[0] = b'X';
            bytes
        }),
        ("short", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("git_ino=-\n", "", 1).into_bytes()
        }),
        ("long", |_, bytes| {
            let mut out = bytes;
            out.extend_from_slice(b"backup2=x\n");
            out
        }),
        ("unknown-key", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("nonce=", "frobnicate=", 1).into_bytes()
        }),
        ("duplicate-key", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("nonce=", "phase=dup\nnonce=", 1).into_bytes()
        }),
        ("no-equals", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("nonce=test-nonce-54\n", "nonce\n", 1)
                .into_bytes()
        }),
        ("empty-origin", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("origin=git://example.com/owner/repo\n", "origin=\n", 1)
                .into_bytes()
        }),
        ("tab-in-value", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("nonce=test-nonce-54\n", "nonce=a\tb\n", 1)
                .into_bytes()
        }),
        ("cr-in-value", |_, bytes| {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.replacen("nonce=test-nonce-54\n", "nonce=a\rb\n", 1)
                .into_bytes()
        }),
    ];
    for (case, mutate) in cases {
        check_read(&format!("init-record-shape-{case}"), "record", *mutate);
    }
}

#[test]
fn read_record_rejects_bad_files() {
    let twins = Twins::build("init-record-badfiles");
    let (shell_file, rust_file) = craft_valid(&twins, "record");
    chmod(&shell_file, 0o644);
    chmod(&rust_file, 0o644);
    let (shell_code, _, _) = shell_run(&twins.shell_home, &[], &read_snippet(&shell_file));
    assert_ne!(shell_code, 99, "probe printed no verdict");
    assert_eq!(shell_code, 1, "shell rejects mode 644");
    assert!(
        record::read_record(&rust_file, &twins.rust_home).is_err(),
        "rust rejects mode 644"
    );

    let twins = Twins::build("init-record-badfiles-link");
    let (shell_file, rust_file) = craft_valid(&twins, "record");
    let shell_link = twins.shell_home.join("link");
    let rust_link = twins.rust_home.join("link");
    std::os::unix::fs::symlink(&shell_file, &shell_link).expect("shell link");
    std::os::unix::fs::symlink(&rust_file, &rust_link).expect("rust link");
    let (shell_code, _, _) = shell_run(&twins.shell_home, &[], &read_snippet(&shell_link));
    assert_eq!(shell_code, 1, "shell rejects a symlink");
    assert!(
        record::read_record(&rust_link, &twins.rust_home).is_err(),
        "rust rejects a symlink"
    );

    let twins = Twins::build("init-record-badfiles-missing");
    let shell_missing = twins.shell_home.join("missing");
    let rust_missing = twins.rust_home.join("missing");
    let (shell_code, _, _) = shell_run(&twins.shell_home, &[], &read_snippet(&shell_missing));
    assert_eq!(shell_code, 1, "shell rejects a missing file");
    assert!(
        record::read_record(&rust_missing, &twins.rust_home).is_err(),
        "rust rejects a missing file"
    );
}

#[test]
fn read_record_rejects_bad_semantics() {
    // (case, line to replace, replacement): replacements keep the
    // line count at fourteen so the semantic gate decides.
    let cases: Vec<(String, String, String)> = vec![
        (
            "phase".to_string(),
            "phase=prepared\n".to_string(),
            "phase=landed\n".to_string(),
        ),
        (
            "branch".to_string(),
            "branch=main\n".to_string(),
            "branch=bad..branch\n".to_string(),
        ),
        (
            "commit".to_string(),
            format!("commit={COMMIT40}\n"),
            "commit=xyz\n".to_string(),
        ),
        (
            "dot-relative".to_string(),
            format!("dot={DOT_BIN}\n"),
            "dot=bin/dot\n".to_string(),
        ),
        (
            "dot-doubled".to_string(),
            format!("dot={DOT_BIN}\n"),
            "dot=/usr//local/bin/dot\n".to_string(),
        ),
        (
            "dot-dotseg".to_string(),
            format!("dot={DOT_BIN}\n"),
            "dot=/usr/local/../bin/dot\n".to_string(),
        ),
        (
            "revision".to_string(),
            format!("dot_revision={REVISION40}\n"),
            "dot_revision=xyz\n".to_string(),
        ),
        (
            "nonce".to_string(),
            "nonce=test-nonce-54\n".to_string(),
            "nonce=a b\n".to_string(),
        ),
        (
            "half-bound".to_string(),
            "git_ino=-\n".to_string(),
            "git_ino=22\n".to_string(),
        ),
        (
            "backup-outside".to_string(),
            ".dot-backup/20240101\n".to_string(),
            ".dot-backup-evil/x\n".to_string(),
        ),
        (
            "git-dir-other".to_string(),
            ".dotfiles\n".to_string(),
            ".dotfiles-evil\n".to_string(),
        ),
    ];
    for (case, needle, replacement) in &cases {
        check_read(&format!("init-record-sem-{case}"), "record", |_, bytes| {
            String::from_utf8_lossy(&bytes)
                .replacen(needle, replacement, 1)
                .into_bytes()
        });
    }
}

#[test]
fn read_record_long_commit_and_git_dir() {
    // The 64-nibble commit form and the `$HOME/.git` live dir are
    // the accepted alternates: both engines read them.
    check_read("init-record-alt", "record", |home, bytes| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        text.replacen(COMMIT40, COMMIT64, 1)
            .replacen(".dotfiles\n", ".git\n", 1)
            .replacen("git_dev=-\ngit_ino=-\n", "git_dev=11\ngit_ino=22\n", 1)
            .replacen(
                &format!("backup={home}/.dot-backup/20240101\n"),
                "backup=-\n",
                1,
            )
            .into_bytes()
    });
}

#[test]
fn read_record_size_gate() {
    // Fourteen well-shaped lines past 16384 bytes: the size gate
    // fires before any field is trusted.
    check_read("init-record-huge", "record", |_, bytes| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let padding = "p".repeat(17_000);
        text.replacen("origin=git:", &format!("origin={padding}git:"), 1)
            .into_bytes()
    });
}

#[test]
fn read_record_newline_edges() {
    // A missing trailing newline still yields its final line on
    // both engines.
    check_read("init-record-noeol", "record", |_, mut bytes| {
        assert_eq!(bytes.pop(), Some(b'\n'));
        bytes
    });
    // Carriage returns stay put, so the header never matches.
    check_read("init-record-crlf", "record", |_, bytes| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        text.replace('\n', "\r\n").into_bytes()
    });
}

/// Shell probe for `_dot_init_record_phase`: the run globals
/// cross as environment, exactly like the engine exports them.
fn phase_snippet(file: &Path, phase: &str) -> String {
    format!(
        "_dot_init_record_phase {} {phase}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&file.to_string_lossy())
    )
}

#[test]
fn record_phase_advances() {
    let twins = Twins::build("init-record-phase-advance");
    let shell_file = twins.shell_home.join("record");
    let rust_file = twins.rust_home.join("record");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let write_env = record_env();
    let write_call = |file: &Path, backup: &str| {
        write_snippet(
            file,
            "prepared",
            "git://example.com/owner/repo",
            "git://example.com/owner/repo",
            "main",
            backup,
            None,
        )
    };
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &write_env,
        &write_call(&shell_file, &shell_backup),
    );
    assert_eq!(shell_code, 0, "shell writes the journal");
    let root = source_root();
    let rust_fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    record::write_record(&rust_file, "prepared", &rust_fields, &mut cache)
        .expect("rust writes the journal");
    // Advance to backing-up on both engines; the phase advance
    // re-resolves every other field from the live run globals, so
    // the advance environment carries the full identity. The
    // phase line is the only byte that may change.
    let phase_env: [(&str, &str); 9] = [
        ("DOT_INIT_ORIGIN", "git://example.com/owner/repo"),
        ("DOT_INIT_IDENTITY", "git://example.com/owner/repo"),
        ("DOT_INIT_BRANCH", "main"),
        ("DOT_INIT_BACKUP", &shell_backup),
        ("DOT_INIT_COMMIT", COMMIT40),
        ("DOT_INIT_NONCE", NONCE),
        ("DOT_INIT_GIT_DEV", "11"),
        ("DOT_INIT_GIT_INO", "22"),
        ("DOT_BIN", DOT_BIN),
    ];
    let (shell_advance, _, _) = shell_run(
        &twins.shell_home,
        &phase_env,
        &phase_snippet(&shell_file, "backing-up"),
    );
    assert_ne!(shell_advance, 99, "probe printed no verdict");
    let rust_advance = record::record_phase(&rust_file, "backing-up", &rust_fields, &mut cache);
    assert_eq!(
        (shell_advance, rust_advance.is_ok()),
        (0, true),
        "advance verdicts"
    );
    let normalized = normalize(
        &std::fs::read(&rust_file).expect("rust bytes"),
        &twins.rust_text(),
        &twins.shell_text(),
    );
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        normalized,
        "advanced bytes agree"
    );
    let found = record::read_record(&rust_file, &twins.rust_home).expect("rust rereads");
    assert_eq!(found.phase, "backing-up");
}

#[test]
fn record_phase_missing_parent_fails() {
    // A sibling temp cannot be created when the parent slot is a
    // file: `mkdir -p` fails on both engines.
    let twins = Twins::build("init-record-phase-blocked");
    std::fs::write(twins.shell_home.join("slot"), b"x\n").expect("shell slot");
    std::fs::write(twins.rust_home.join("slot"), b"x\n").expect("rust slot");
    let shell_file = twins.shell_home.join("slot/record");
    let rust_file = twins.rust_home.join("slot/record");
    let shell_backup = format!("{}/.dot-backup/20240101", twins.shell_text());
    let rust_backup = format!("{}/.dot-backup/20240101", twins.rust_text());
    let env = record_env();
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &env,
        &phase_snippet(&shell_file, "backing-up"),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict");
    let root = source_root();
    let fields = fields_for(
        &twins.rust_home,
        &root,
        &rust_backup,
        None,
        Some(COMMIT40),
        Some(NONCE),
        Some("11"),
        Some("22"),
    );
    let mut cache = MoveCache::default();
    let rust = record::record_phase(&rust_file, "backing-up", &fields, &mut cache);
    assert_ne!(shell_code, 0, "shell fails past a file parent");
    assert!(rust.is_err(), "rust fails past a file parent");
    let _ = shell_backup;
}

/// Shell probe for `_dot_init_parent_record`: prints the verdict
/// plus `REPLY`, nounset-proof like the journal probe.
fn parent_snippet(transaction: &Path, relative: &str) -> String {
    format!(
        "_dot_init_parent_record {} {}; code=$?; printf 'code=%s\\nreply=%s\\n' \"$code\" \"${{REPLY-}}\"",
        sq(&transaction.to_string_lossy()),
        sq(relative)
    )
}

/// Serialize one parent record like the shell probe prints it.
fn serialize_parent(found: &record::ParentRecord) -> String {
    format!(
        "code=0\nreply={}\t{}\t{}\t{}\t{}\n",
        found.phase, found.stage, found.dev, found.ino, found.mode
    )
}

/// Parent-intent file name for one relative path: the shared
/// text digest both engines hash with.
fn intent_name(relative: &str) -> String {
    let hash = dot::temp::file_text_digest(&source_root(), relative.as_bytes()).expect("hash");
    format!("parent-intent.{hash}")
}

/// Craft matching parent intents per side: the stage embeds the
/// side home, everything else is identical.
fn craft_parent(twins: &Twins, relative: &str, line_for: &dyn Fn(&str, &str) -> String) {
    let name = intent_name(relative);
    let shell_line = line_for(&twins.shell_text(), relative);
    let rust_line = line_for(&twins.rust_text(), relative);
    std::fs::write(twins.shell_home.join(&name), shell_line.as_bytes()).expect("shell intent");
    std::fs::write(twins.rust_home.join(&name), rust_line.as_bytes()).expect("rust intent");
}

/// Stage path the shell derives for one home and relative path.
fn parent_stage(home: &str, relative: &str, hash: &str) -> String {
    let base = match relative.rfind('/') {
        Some(mark) => format!("{home}/{}", &relative[..mark]),
        None => home.to_string(),
    };
    let base = base.strip_suffix('/').unwrap_or(&base);
    format!("{base}/.dot-init-parent.{NONCE}.{hash}")
}

/// Check one parent intent through both engines: verdict plus
/// the five reply fields on success, verdict only on failure.
fn check_parent(tag: &str, relative: &str, line_for: &dyn Fn(&str, &str) -> String) {
    let twins = Twins::build(tag);
    craft_parent(&twins, relative, line_for);
    let (shell_code, shell_out, _) = shell_run(
        &twins.shell_home,
        &[("DOT_INIT_NONCE", NONCE)],
        &parent_snippet(&twins.shell_home, relative),
    );
    assert_ne!(shell_code, 99, "probe printed no verdict for {tag}");
    let rust = record::parent_record(
        &twins.rust_home,
        relative,
        &twins.rust_home,
        NONCE,
        &source_root(),
    );
    match rust {
        Ok(found) => {
            let normalized = normalize(
                serialize_parent(&found).as_bytes(),
                &twins.rust_text(),
                &twins.shell_text(),
            );
            assert_eq!(shell_code, 0, "shell accepts for {tag}");
            assert_eq!(
                String::from_utf8_lossy(&shell_out).into_owned(),
                String::from_utf8_lossy(&normalized).into_owned(),
                "parent fields for {tag}"
            );
        }
        Err(_) => {
            assert_eq!(shell_code, 1, "shell rejects for {tag}");
        }
    }
}

#[test]
fn parent_record_pending_and_prepared() {
    let pending = |home: &str, relative: &str| {
        let hash = dot::temp::file_text_digest(&source_root(), relative.as_bytes()).expect("hash");
        format!(
            "pending\t{relative}\t{}\t-\t-\t-\n",
            parent_stage(home, relative, &hash)
        )
    };
    check_parent("init-record-parent-pending", "a/b", &|home, relative| {
        pending(home, relative)
    });
    check_parent("init-record-parent-top", "top", &|home, relative| {
        pending(home, relative)
    });
    check_parent("init-record-parent-prepared", "a/b", &|home, relative| {
        let hash = dot::temp::file_text_digest(&source_root(), relative.as_bytes()).expect("hash");
        format!(
            "prepared\t{relative}\t{}\t11\t22\t700\n",
            parent_stage(home, relative, &hash)
        )
    });
}

#[test]
fn parent_record_rejects() {
    let pending = |home: &str, relative: &str| {
        let hash = dot::temp::file_text_digest(&source_root(), relative.as_bytes()).expect("hash");
        format!(
            "pending\t{relative}\t{}\t-\t-\t-\n",
            parent_stage(home, relative, &hash)
        )
    };
    // Wrong recorded parent.
    check_parent("init-record-parent-other", "a/b", &|home, _| {
        pending(home, "a/c")
    });
    // Stage bound to another nonce.
    check_parent("init-record-parent-stage", "a/b", &|home, relative| {
        pending(home, relative).replacen(NONCE, "other-nonce", 1)
    });
    // Prepared shape with pending generation.
    check_parent("init-record-parent-unbound", "a/b", &|home, relative| {
        pending(home, relative).replacen("pending", "prepared", 1)
    });
    // Pending phase with bound generation.
    check_parent("init-record-parent-bound", "a/b", &|home, relative| {
        pending(home, relative).replacen("-	-	-", "11	22	700", 1)
    });
    // Seventh field is not empty.
    check_parent("init-record-parent-extra", "a/b", &|home, relative| {
        pending(home, relative).replacen("\n", "\textra\n", 1)
    });
    // Unknown phase.
    check_parent("init-record-parent-phase", "a/b", &|home, relative| {
        pending(home, relative).replacen("pending", "staged", 1)
    });
    // Present but empty intent file.
    check_parent("init-record-parent-empty", "a/b", &|_, _| String::new());
}

#[test]
fn parent_record_missing_intent_fails() {
    // No intent file crafted on either side.
    let twins = Twins::build("init-record-parent-missing");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[("DOT_INIT_NONCE", NONCE)],
        &parent_snippet(&twins.shell_home, "a/b"),
    );
    assert_eq!(shell_code, 1, "shell rejects a missing intent");
    assert!(
        record::parent_record(
            &twins.rust_home,
            "a/b",
            &twins.rust_home,
            NONCE,
            &source_root()
        )
        .is_err(),
        "rust rejects a missing intent"
    );
}

/// Shell probe for `_dot_init_prior_record`: prints the verdict
/// plus `REPLY`, nounset-proof like the other probes.
fn prior_snippet(prior: &Path, wanted: &str) -> String {
    format!(
        "_dot_init_prior_record {} {}; code=$?; printf 'code=%s\\nreply=%s\\n' \"$code\" \"${{REPLY-}}\"",
        sq(&prior.to_string_lossy()),
        sq(wanted)
    )
}

/// Serialize one prior entry like the shell probe prints it.
fn serialize_prior(found: &record::PriorEntry) -> String {
    format!(
        "code=0\nreply={}\t{}\t{}\t{}\t{}\t{}\n",
        found.kind, found.dev, found.ino, found.mode, found.size, found.value
    )
}

/// Check one prior lookup through both engines: verdict plus the
/// six reply fields on success, verdict only on failure.
fn check_prior(tag: &str, body: &[u8], wanted: &str) {
    let twins = Twins::build(tag);
    let shell_prior = twins.shell_home.join("prior.tsv");
    let rust_prior = twins.rust_home.join("prior.tsv");
    std::fs::write(&shell_prior, body).expect("shell prior");
    std::fs::write(&rust_prior, body).expect("rust prior");
    let (shell_code, shell_out, _) =
        shell_run(&twins.shell_home, &[], &prior_snippet(&shell_prior, wanted));
    assert_ne!(shell_code, 99, "probe printed no verdict for {tag}");
    let rust = record::prior_record(&rust_prior, wanted);
    match rust {
        Ok(found) => {
            assert_eq!(shell_code, 0, "shell accepts for {tag}");
            assert_eq!(
                String::from_utf8_lossy(&shell_out).into_owned(),
                serialize_prior(&found),
                "prior fields for {tag}"
            );
        }
        Err(_) => {
            assert_eq!(shell_code, 1, "shell rejects for {tag}");
        }
    }
}

#[test]
fn prior_record_first_match_wins() {
    // The second `dotfile` line must not shadow the first, and a
    // tabbed tail stays inside the value.
    let body = b"other\tregular\t1\t2\t644\t3\thash-a\n\
                 dotfile\tregular\t11\t22\t644\t5\thash-first\n\
                 dotfile\tsymlink\t33\t44\t777\t9\thash-second\n\
                 link\tsymlink\t55\t66\t777\t7\ta\tb\n";
    check_prior("init-record-prior-first", body, "dotfile");
    check_prior("init-record-prior-tabbed", body, "link");
    check_prior("init-record-prior-other", body, "other");
}

#[test]
fn prior_record_miss_and_edges() {
    let body = b"dotfile\tregular\t11\t22\t644\t5\thash-first\n";
    check_prior("init-record-prior-miss", body, "nope");
    check_prior("init-record-prior-empty", b"", "dotfile");
    // Short lines pad with empty fields, like the shell's `read`.
    check_prior("init-record-prior-short", b"dotfile\tregular\n", "dotfile");
    // A final line without its newline never runs the shell loop
    // body, so the lone partial line matches nothing ...
    check_prior(
        "init-record-prior-noeol-miss",
        b"dotfile\tregular\t11\t22\t644\t5\thash-first",
        "dotfile",
    );
    // ... while an earlier terminated line still matches.
    check_prior(
        "init-record-prior-noeol-hit",
        b"other\tregular\t1\t2\t644\t3\thash-a\ndotfile\tregular\t11\t22\t644\t5\thash-first",
        "other",
    );
    let twins = Twins::build("init-record-prior-missing");
    let shell_missing = twins.shell_home.join("missing.tsv");
    let rust_missing = twins.rust_home.join("missing.tsv");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &prior_snippet(&shell_missing, "dotfile"),
    );
    assert_eq!(shell_code, 1, "shell rejects a missing file");
    assert!(
        record::prior_record(&rust_missing, "dotfile").is_err(),
        "rust rejects a missing file"
    );
}
