//! Differential parity tests for the init per-entry publication
//! staging (`lib/dot/init-client.sh`, the intent/claim family)
//! against the live shell: the mode-600 line publisher, the
//! entry stage path, the intent record validator, and the five
//! stage claim helpers.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so
//! sibling temps and stage paths never collide. Home-relative
//! outputs (stage paths, intent replies) compare directly; live
//! identities (device/inode) only gate the verdict.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_entry as entry;
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the entry chapter: the shared temp helpers
/// (sibling temps, stat probes, moves, stdin hashing) and the
/// init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixed run nonce for every row.
const NONCE: &str = "test-nonce-46";
/// Fixed candidate mode and object id for intent rows.
const MODE: &str = "100644";
const OID: &str = "0123456789abcdef0123456789abcdef01234567";

/// Run one shell snippet with the init runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that
/// verdict — not the process status, which only says the printer
/// ran. A snippet that never reports (a harness bug, never a pass)
/// yields 99.
///
/// The locale stays pinned: git diagnostics must read English on
/// both engines, and the port pins `LC_ALL=C` around every git
/// run. The run nonce crosses as an explicit environment entry,
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

/// The run-nonce environment for claim/intent rows.
fn nonce_env() -> [(&'static str, &'static str); 1] {
    [("DOT_INIT_NONCE", NONCE)]
}

/// The crate root backing the hash subprocesses.
fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so sibling temps and stage
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

/// Permission bits of one path, `stat -c '%a'` style.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

/// Sibling-temp residue in one directory: entries carrying the
/// `.tmp.` infix both engines leave behind on a failed publish.
fn tmp_residue(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("list dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count()
}

/// Shell probe ending in the `code=` verdict for
/// `_dot_init_write_private_line`, with an optional third
/// (replace) argument.
fn line_snippet(file: &Path, line: &str, replace: Option<&str>) -> String {
    let mut call = format!(
        "_dot_init_write_private_line {} {}",
        sq(&file.to_string_lossy()),
        sq(line)
    );
    if let Some(flag) = replace {
        call.push(' ');
        call.push_str(&sq(flag));
    }
    format!("{call}; code=$?; printf 'code=%s\\n' \"$code\"")
}

/// Write one line through both engines and report the verdicts.
/// Homes differ per side, so only the verdict compares here;
/// callers pin bytes and modes themselves.
fn check_line(tag: &str, name: &str, line: &str, replace: Option<&str>) -> (i32, i32) {
    let twins = Twins::build(tag);
    let shell_file = twins.shell_home.join(name);
    let rust_file = twins.rust_home.join(name);
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &line_snippet(&shell_file, line, replace),
    );
    let mut cache = MoveCache::default();
    let rust_ok =
        entry::write_private_line(&rust_file, line, replace == Some("true"), &mut cache).is_ok();
    assert_eq!(
        shell_code == 0,
        rust_ok,
        "shell/rust private-line verdict parity"
    );
    (shell_code, i32::from(rust_ok))
}

#[test]
fn private_line_creates_mode_600() {
    for (tag, replace) in [
        ("init-entry-line-new", None),
        ("init-entry-line-noreplace", Some("false")),
    ] {
        let twins = Twins::build(tag);
        let shell_file = twins.shell_home.join("intent");
        let rust_file = twins.rust_home.join("intent");
        let (shell_code, _, _) = shell_run(
            &twins.shell_home,
            &nonce_env(),
            &line_snippet(&shell_file, "pending\ta\tb", replace),
        );
        let mut cache = MoveCache::default();
        let rust = entry::write_private_line(&rust_file, "pending\ta\tb", false, &mut cache);
        assert_eq!((shell_code, rust.is_ok()), (0, true), "create {tag}");
        assert_eq!(
            std::fs::read(&shell_file).expect("shell bytes"),
            std::fs::read(&rust_file).expect("rust bytes"),
            "created bytes for {tag}"
        );
        assert_eq!(mode_of(&shell_file), 0o600, "shell mode for {tag}");
        assert_eq!(mode_of(&rust_file), 0o600, "rust mode for {tag}");
    }
    let (shell_code, rust_code) = check_line("init-entry-line-tab", "line", "a\tb c", None);
    assert_eq!((shell_code, rust_code), (0, 1), "tabbed line verdicts");
}

#[test]
fn private_line_noreplace_keeps_live_file() {
    let twins = Twins::build("init-entry-line-lived");
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    std::fs::write(&shell_file, b"live\n").expect("shell live");
    std::fs::write(&rust_file, b"live\n").expect("rust live");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &line_snippet(&shell_file, "pending", None),
    );
    let mut cache = MoveCache::default();
    let rust = entry::write_private_line(&rust_file, "pending", false, &mut cache);
    assert_eq!((shell_code, rust.is_err()), (1, true), "lived verdicts");
    assert_eq!(
        std::fs::read(&shell_file).expect("shell kept"),
        b"live\n",
        "shell keeps the live file"
    );
    assert_eq!(
        std::fs::read(&rust_file).expect("rust kept"),
        b"live\n",
        "rust keeps the live file"
    );
    assert!(
        tmp_residue(&twins.shell_home) >= 1,
        "shell leaves its sibling behind"
    );
    assert!(
        tmp_residue(&twins.rust_home) >= 1,
        "rust leaves its sibling behind"
    );
}

#[test]
fn private_line_replace_swaps() {
    let twins = Twins::build("init-entry-line-replace");
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    std::fs::write(&shell_file, b"live\n").expect("shell live");
    std::fs::write(&rust_file, b"live\n").expect("rust live");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &line_snippet(&shell_file, "staged", Some("true")),
    );
    let mut cache = MoveCache::default();
    let rust = entry::write_private_line(&rust_file, "staged", true, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "replace verdicts");
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        b"staged\n",
        "shell replaces the live file"
    );
    assert_eq!(
        std::fs::read(&rust_file).expect("rust bytes"),
        b"staged\n",
        "rust replaces the live file"
    );
    let (fresh_shell, fresh_rust) =
        check_line("init-entry-line-replace-new", "new", "x", Some("true"));
    assert_eq!(
        (fresh_shell, fresh_rust),
        (0, 1),
        "replace on a missing file"
    );
}

#[test]
fn private_line_umask_077_stays_600() {
    let twins = Twins::build("init-entry-line-umask");
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    let mut snippet = String::from("umask 077; ");
    snippet.push_str(&line_snippet(&shell_file, "pending", None));
    let (shell_code, _, _) = shell_run(&twins.shell_home, &nonce_env(), &snippet);
    let mut cache = MoveCache::default();
    let rust = entry::write_private_line(&rust_file, "pending", false, &mut cache);
    assert_eq!((shell_code, rust.is_ok()), (0, true), "umask verdicts");
    assert_eq!(mode_of(&shell_file), 0o600, "shell mode under umask 077");
    assert_eq!(mode_of(&rust_file), 0o600, "rust mode under umask 077");
}

/// Shell probe for `_dot_init_entry_stage`: prints the verdict
/// plus the derived `REPLY`.
fn stage_snippet(path: &str) -> String {
    format!(
        "_dot_init_entry_stage {}; code=$?; out=$REPLY; printf 'code=%s\\nout=%s\\n' \"$code\" \"$out\"",
        sq(path)
    )
}

/// Derive the stage path through the shell under one home and
/// compare it byte for byte against the port's derivation for
/// that same home (the shell's `git hash-object` plus string
/// concatenation versus the port's).
fn check_stage(shell_home: &Path, path: &str) {
    let (shell_code, shell_out, _) = shell_run(shell_home, &nonce_env(), &stage_snippet(path));
    let want = entry::entry_stage(shell_home, path, NONCE, &source_root()).expect("stage");
    assert_eq!(
        (shell_code, String::from_utf8_lossy(&shell_out).into_owned()),
        (0, format!("code=0\nout={}\n", want.to_string_lossy())),
        "stage path for {path}"
    );
}

#[test]
fn entry_stage_shapes() {
    let twins = Twins::build("init-entry-stage");
    for path in ["dotfile", "a/b", "a/b/c"] {
        check_stage(&twins.shell_home, path);
        check_stage(&twins.rust_home, path);
    }
}

/// A `HOME` with a trailing slash keeps its doubled separator on
/// both engines (plain byte concatenation, never normalized).
#[test]
fn entry_stage_trailing_slash_home() {
    let twins = Twins::build("init-entry-stage-slash");
    let shell_home = PathBuf::from(format!("{}/", twins.shell_home.to_string_lossy()));
    let rust_home = PathBuf::from(format!("{}/", twins.rust_home.to_string_lossy()));
    check_stage(&shell_home, "a/b");
    let rust_stage = entry::entry_stage(&rust_home, "top", NONCE, &source_root()).expect("stage");
    assert!(
        rust_stage.to_string_lossy().contains("//.dot-init-entry."),
        "rust keeps the doubled separator"
    );
}

/// Home-relative stage path for intent fixtures: identical across
/// twin homes, so one fixture body serves both sides.
fn stage_rel(home: &Path, path: &str) -> String {
    let full = entry::entry_stage(home, path, NONCE, &source_root()).expect("stage");
    full.strip_prefix(home)
        .expect("stage under home")
        .to_string_lossy()
        .into_owned()
}

/// One nine-field intent record body.
#[allow(clippy::too_many_arguments)]
fn intent_body(
    phase: &str,
    mode: &str,
    oid: &str,
    path: &str,
    stage: &str,
    dev: &str,
    ino: &str,
    next_dev: &str,
    next_ino: &str,
) -> Vec<u8> {
    format!("{phase}\t{mode}\t{oid}\t{path}\t{stage}\t{dev}\t{ino}\t{next_dev}\t{next_ino}\n")
        .into_bytes()
}

/// Shell probe for `_dot_init_entry_intent`: prints the verdict
/// plus the extracted `REPLY` (empty on rejection).
fn intent_snippet(file: &Path, mode: &str, oid: &str, path: &str) -> String {
    format!(
        "if _dot_init_entry_intent {} {} {} {}; then code=0; reply=$REPLY; else code=$?; reply=''; fi; printf 'code=%s\\nreply=%s\\n' \"$code\" \"$reply\"",
        sq(&file.to_string_lossy()),
        sq(mode),
        sq(oid),
        sq(path)
    )
}

/// Validate one intent body on both sides and report the two
/// `(verdict, reply)` pairs. Fixture bytes are identical across
/// sides (stage-relative, shape-only identities), so the full
/// reply compares too.
fn check_intent(
    tag: &str,
    body: &[u8],
    mode: &str,
    oid: &str,
    path: &str,
) -> ((i32, String), (i32, String)) {
    let twins = Twins::build(tag);
    let shell_file = twins.shell_home.join("publish-intent.1");
    let rust_file = twins.rust_home.join("publish-intent.1");
    std::fs::write(&shell_file, body).expect("shell intent");
    std::fs::write(&rust_file, body).expect("rust intent");
    let (shell_code, shell_out, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &intent_snippet(&shell_file, mode, oid, path),
    );
    let rust = entry::entry_intent(
        &rust_file,
        mode,
        oid,
        path,
        &twins.rust_home,
        NONCE,
        &source_root(),
    );
    let (rust_code, rust_reply) = match rust {
        Ok(intent) => (
            0,
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                intent.phase,
                intent.stage,
                intent.dev,
                intent.ino,
                intent.next_dev,
                intent.next_ino
            ),
        ),
        Err(_) => (1, String::new()),
    };
    let shell_text = String::from_utf8_lossy(&shell_out).into_owned();
    assert_eq!(
        shell_code == 0,
        rust_code == 0,
        "shell/rust intent verdict parity"
    );
    ((shell_code, shell_text), (rust_code, rust_reply))
}

/// Accept one intent body on both sides and pin the extracted
/// reply byte for byte.
fn accept_intent(tag: &str, body: &[u8], path: &str, want_reply: &str) {
    let ((shell_code, shell_text), (rust_code, rust_reply)) =
        check_intent(tag, body, MODE, OID, path);
    assert_eq!(
        (shell_code, shell_text),
        (0, format!("code=0\nreply={want_reply}\n"),),
        "shell accepts {tag}"
    );
    assert_eq!(
        (rust_code, rust_reply),
        (0, want_reply.to_string()),
        "rust accepts {tag}"
    );
}

/// Reject one intent body on both sides.
fn reject_intent(tag: &str, body: &[u8], path: &str) {
    let ((shell_code, _), (rust_code, _)) = check_intent(tag, body, MODE, OID, path);
    assert_eq!((shell_code, rust_code), (1, 1), "both reject {tag}");
}

#[test]
fn entry_intent_accepts_phases() {
    let twins = Twins::build("init-entry-intent-ok");
    let stage = stage_rel(&twins.shell_home, "a/b");
    accept_intent(
        "init-entry-intent-pending",
        &intent_body("pending", MODE, OID, "a/b", &stage, "-", "-", "-", "-"),
        "a/b",
        &format!("pending\t{stage}\t-\t-\t-\t-"),
    );
    accept_intent(
        "init-entry-intent-staged",
        &intent_body("staged", MODE, OID, "a/b", &stage, "11", "22", "-", "-"),
        "a/b",
        &format!("staged\t{stage}\t11\t22\t-\t-"),
    );
    accept_intent(
        "init-entry-intent-prepared",
        &intent_body("prepared", MODE, OID, "a/b", &stage, "11", "22", "33", "44"),
        "a/b",
        &format!("prepared\t{stage}\t11\t22\t33\t44"),
    );
}

#[test]
fn entry_intent_rejects_mismatch() {
    let twins = Twins::build("init-entry-intent-mismatch");
    let stage = stage_rel(&twins.shell_home, "a/b");
    reject_intent(
        "init-entry-intent-mode",
        &intent_body("pending", "100755", OID, "a/b", &stage, "-", "-", "-", "-"),
        "a/b",
    );
    reject_intent(
        "init-entry-intent-oid",
        &intent_body(
            "pending",
            MODE,
            "ffffffffffffffffffffffffffffffffffffffff",
            "a/b",
            &stage,
            "-",
            "-",
            "-",
            "-",
        ),
        "a/b",
    );
    reject_intent(
        "init-entry-intent-path",
        &intent_body("pending", MODE, OID, "a/c", &stage, "-", "-", "-", "-"),
        "a/b",
    );
    let other = stage_rel(&twins.shell_home, "a/c");
    reject_intent(
        "init-entry-intent-stage",
        &intent_body("pending", MODE, OID, "a/b", &other, "-", "-", "-", "-"),
        "a/b",
    );
}

#[test]
fn entry_intent_rejects_phases_and_shapes() {
    let twins = Twins::build("init-entry-intent-shapes");
    let stage = stage_rel(&twins.shell_home, "a/b");
    let base = intent_body("pending", MODE, OID, "a/b", &stage, "-", "-", "-", "-");
    reject_intent(
        "init-entry-intent-phase",
        &intent_body("converging", MODE, OID, "a/b", &stage, "-", "-", "-", "-"),
        "a/b",
    );
    reject_intent(
        "init-entry-intent-prepared-dashes",
        &intent_body("prepared", MODE, OID, "a/b", &stage, "-", "-", "-", "-"),
        "a/b",
    );
    reject_intent(
        "init-entry-intent-staged-next",
        &intent_body("staged", MODE, OID, "a/b", &stage, "11", "22", "33", "44"),
        "a/b",
    );
    reject_intent(
        "init-entry-intent-pending-digits",
        &intent_body("pending", MODE, OID, "a/b", &stage, "11", "22", "-", "-"),
        "a/b",
    );
    let mut short = base.clone();
    short.truncate(short.len() - "-\n".len());
    reject_intent("init-entry-intent-short", &short, "a/b");
    let mut extra = base.clone();
    extra.pop();
    extra.extend_from_slice(b"\textra\n");
    reject_intent("init-entry-intent-extra", &extra, "a/b");
    let mut split = base.clone();
    split.pop();
    split.extend_from_slice(b"\nsecond\n");
    reject_intent("init-entry-intent-newline", &split, "a/b");
    reject_intent("init-entry-intent-empty", b"", "a/b");
}

#[test]
fn entry_intent_missing_and_directory() {
    let twins = Twins::build("init-entry-intent-missing");
    let shell_file = twins.shell_home.join("nope");
    let rust_file = twins.rust_home.join("nope");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &intent_snippet(&shell_file, MODE, OID, "a/b"),
    );
    let rust = entry::entry_intent(
        &rust_file,
        MODE,
        OID,
        "a/b",
        &twins.rust_home,
        NONCE,
        &source_root(),
    );
    assert_eq!(
        (shell_code, rust.is_err()),
        (1, true),
        "missing file verdicts"
    );
    let shell_dir = twins.shell_home.join("dir");
    let rust_dir = twins.rust_home.join("dir");
    std::fs::create_dir_all(&shell_dir).expect("shell dir");
    std::fs::create_dir_all(&rust_dir).expect("rust dir");
    let (dir_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &intent_snippet(&shell_dir, MODE, OID, "a/b"),
    );
    let rust_dir = entry::entry_intent(
        &rust_dir,
        MODE,
        OID,
        "a/b",
        &twins.rust_home,
        NONCE,
        &source_root(),
    );
    assert_eq!(
        (dir_code, rust_dir.is_err()),
        (1, true),
        "directory verdicts"
    );
}

#[test]
fn entry_intent_trailing_bytes() {
    let twins = Twins::build("init-entry-intent-trailing");
    let stage = stage_rel(&twins.shell_home, "top");
    // A trailing tab leaves the tenth `read` variable empty, which
    // the shell accepts: the port mirrors the nine-plus-empty
    // shape instead of demanding exactly nine fields.
    let mut tabbed = intent_body("pending", MODE, OID, "top", &stage, "-", "-", "-", "-");
    tabbed.pop();
    tabbed.extend_from_slice(b"\t\n");
    accept_intent(
        "init-entry-intent-trailing-tab",
        &tabbed,
        "top",
        &format!("pending\t{stage}\t-\t-\t-\t-"),
    );
    // Trailing blank lines strip like the shell's command
    // substitution before the single-line gate runs.
    let mut blanked = intent_body("pending", MODE, OID, "top", &stage, "-", "-", "-", "-");
    blanked.extend_from_slice(b"\n\n");
    accept_intent(
        "init-entry-intent-trailing-newlines",
        &blanked,
        "top",
        &format!("pending\t{stage}\t-\t-\t-\t-"),
    );
}

/// Shell probe for `_dot_init_stage_claim_file`.
fn claim_file_snippet(stage: &Path) -> String {
    format!(
        "_dot_init_stage_claim_file {}; code=$?; out=$REPLY; printf 'code=%s\\nout=%s\\n' \"$code\" \"$out\"",
        sq(&stage.to_string_lossy())
    )
}

#[test]
fn claim_file_shapes() {
    let twins = Twins::build("init-entry-claim-file");
    for (tag, stage) in [
        ("plain", twins.shell_home.join("stage")),
        (
            "slash",
            PathBuf::from(format!("{}/", twins.shell_home.to_string_lossy())),
        ),
    ] {
        let (shell_code, shell_out, _) =
            shell_run(&twins.shell_home, &nonce_env(), &claim_file_snippet(&stage));
        let want = entry::stage_claim_file(&stage);
        assert_eq!(
            (shell_code, String::from_utf8_lossy(&shell_out).into_owned()),
            (0, format!("code=0\nout={}\n", want.to_string_lossy())),
            "claim file {tag}"
        );
    }
}

/// Exact claim-marker bytes both engines must publish.
fn claim_body(kind: &str, nonce: &str, path: &str) -> Vec<u8> {
    format!("cgraf78 dot publication stage claim v1\nkind={kind}\nnonce={nonce}\npath={path}\n")
        .into_bytes()
}

/// Shell probe for `_dot_init_stage_claim_matches`.
fn matches_snippet(stage: &Path, kind: &str, path: &str) -> String {
    format!(
        "if _dot_init_stage_claim_matches {} {} {}; then code=0; else code=$?; fi; printf 'code=%s\\n' \"$code\"",
        sq(&stage.to_string_lossy()),
        sq(kind),
        sq(path)
    )
}

/// Seed `stage` on one side with `marker` bytes at mode 600
/// (creating the stage), for claim-match rows: the mode matches
/// what the write path publishes, so each row pins its own
/// rejection reason instead of tripping the mode gate.
fn seed_stage(home: &Path, name: &str, marker: &[u8]) -> PathBuf {
    let stage = home.join(name);
    std::fs::create_dir_all(&stage).expect("stage dir");
    let marker_path = stage.join(entry::STAGE_CLAIM_NAME);
    std::fs::write(&marker_path, marker).expect("seed marker");
    chmod(&marker_path, 0o600);
    stage
}

/// Claim verdict on both sides for one (`kind`, `path`) probe
/// against identically seeded stages.
fn check_matches(tag: &str, kind: &str, path: &str, marker: &[u8]) -> (i32, bool) {
    let twins = Twins::build(tag);
    let shell_stage = seed_stage(&twins.shell_home, "stage", marker);
    let rust_stage = seed_stage(&twins.rust_home, "stage", marker);
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &matches_snippet(&shell_stage, kind, path),
    );
    let matched = entry::stage_claim_matches(&rust_stage, kind, path, NONCE, &source_root());
    assert_eq!(shell_code == 0, matched, "shell/rust claim verdict parity");
    (shell_code, matched)
}

#[test]
fn claim_matches_accepts() {
    for kind in ["entry", "parent"] {
        let (shell_code, matched) = check_matches(
            "init-entry-claim-ok",
            kind,
            "a/b",
            &claim_body(kind, NONCE, "a/b"),
        );
        assert_eq!((shell_code, matched), (0, true), "claim accepts {kind}");
    }
}

#[test]
fn claim_matches_rejects_shape() {
    let good = claim_body("entry", NONCE, "a/b");
    for (tag, kind, path) in [
        ("kind", "bogus", "a/b"),
        ("empty-kind", "", "a/b"),
        ("absolute", "entry", "/a/b"),
        ("dotdot", "entry", "a/../b"),
        ("git", "entry", "a/.git/b"),
        ("trailing-slash", "entry", "a/b/"),
    ] {
        let (shell_code, matched) = check_matches(tag, kind, path, &good);
        assert_eq!((shell_code, matched), (1, false), "claim rejects {tag}");
    }
}

#[test]
fn claim_matches_rejects_marker() {
    // Missing marker: no seeding at all.
    let twins = Twins::build("init-entry-claim-missing");
    let shell_stage = twins.shell_home.join("stage");
    let rust_stage = twins.rust_home.join("stage");
    std::fs::create_dir_all(&shell_stage).expect("shell stage");
    std::fs::create_dir_all(&rust_stage).expect("rust stage");
    let (missing_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &matches_snippet(&shell_stage, "entry", "a/b"),
    );
    assert_eq!(
        (
            missing_code,
            entry::stage_claim_matches(&rust_stage, "entry", "a/b", NONCE, &source_root())
        ),
        (1, false),
        "missing marker verdicts"
    );
    // Wrong nonce, wrong bytes, wrong mode, extra link, symlink.
    let (nonce_code, nonce_matched) = check_matches(
        "init-entry-claim-nonce",
        "entry",
        "a/b",
        &claim_body("entry", "other-nonce", "a/b"),
    );
    assert_eq!((nonce_code, nonce_matched), (1, false), "wrong nonce");
    let (body_code, body_matched) =
        check_matches("init-entry-claim-body", "entry", "a/b", b"forged\n");
    assert_eq!((body_code, body_matched), (1, false), "wrong bytes");
    let twins = Twins::build("init-entry-claim-modes");
    let shell_stage = seed_stage(
        &twins.shell_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    let rust_stage = seed_stage(
        &twins.rust_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    chmod(&shell_stage.join(entry::STAGE_CLAIM_NAME), 0o640);
    chmod(&rust_stage.join(entry::STAGE_CLAIM_NAME), 0o640);
    let (mode_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &matches_snippet(&shell_stage, "entry", "a/b"),
    );
    assert_eq!(
        (
            mode_code,
            entry::stage_claim_matches(&rust_stage, "entry", "a/b", NONCE, &source_root())
        ),
        (1, false),
        "mode 640 verdicts"
    );
    let twins = Twins::build("init-entry-claim-links");
    let shell_stage = seed_stage(
        &twins.shell_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    let rust_stage = seed_stage(
        &twins.rust_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    std::fs::hard_link(
        shell_stage.join(entry::STAGE_CLAIM_NAME),
        shell_stage.join("alias"),
    )
    .expect("shell link");
    std::fs::hard_link(
        rust_stage.join(entry::STAGE_CLAIM_NAME),
        rust_stage.join("alias"),
    )
    .expect("rust link");
    // The alias beside the marker breaks `stage_claim_only` but
    // the link count already fails the match first; probe the
    // match directly.
    let (link_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &matches_snippet(&shell_stage, "entry", "a/b"),
    );
    assert_eq!(
        (
            link_code,
            entry::stage_claim_matches(&rust_stage, "entry", "a/b", NONCE, &source_root())
        ),
        (1, false),
        "linked marker verdicts"
    );
    let twins = Twins::build("init-entry-claim-symlink");
    let shell_stage = twins.shell_home.join("stage");
    let rust_stage = twins.rust_home.join("stage");
    std::fs::create_dir_all(&shell_stage).expect("shell stage");
    std::fs::create_dir_all(&rust_stage).expect("rust stage");
    std::fs::write(
        shell_stage.join("target"),
        claim_body("entry", NONCE, "a/b"),
    )
    .expect("shell target");
    std::fs::write(rust_stage.join("target"), claim_body("entry", NONCE, "a/b"))
        .expect("rust target");
    std::os::unix::fs::symlink(
        shell_stage.join("target"),
        shell_stage.join(entry::STAGE_CLAIM_NAME),
    )
    .expect("shell symlink");
    std::os::unix::fs::symlink(
        rust_stage.join("target"),
        rust_stage.join(entry::STAGE_CLAIM_NAME),
    )
    .expect("rust symlink");
    let (link_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &matches_snippet(&shell_stage, "entry", "a/b"),
    );
    assert_eq!(
        (
            link_code,
            entry::stage_claim_matches(&rust_stage, "entry", "a/b", NONCE, &source_root())
        ),
        (1, false),
        "symlink marker verdicts"
    );
}

/// Shell probe for `_dot_init_stage_claim_write`.
fn write_snippet(stage: &Path, kind: &str, path: &str) -> String {
    format!(
        "_dot_init_stage_claim_write {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&stage.to_string_lossy()),
        sq(kind),
        sq(path)
    )
}

#[test]
fn claim_write_publishes() {
    let twins = Twins::build("init-entry-claim-write");
    let shell_stage = twins.shell_home.join("stage");
    let rust_stage = twins.rust_home.join("stage");
    std::fs::create_dir_all(&shell_stage).expect("shell stage");
    std::fs::create_dir_all(&rust_stage).expect("rust stage");
    let (shell_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &write_snippet(&shell_stage, "entry", "a/b"),
    );
    let mut cache = MoveCache::default();
    let rust = entry::stage_claim_write(
        &rust_stage,
        "entry",
        "a/b",
        NONCE,
        &source_root(),
        &mut cache,
    );
    assert_eq!((shell_code, rust.is_ok()), (0, true), "write verdicts");
    let want = claim_body("entry", NONCE, "a/b");
    assert_eq!(
        std::fs::read(shell_stage.join(entry::STAGE_CLAIM_NAME)).expect("shell marker"),
        want,
        "shell publishes exact bytes"
    );
    assert_eq!(
        std::fs::read(rust_stage.join(entry::STAGE_CLAIM_NAME)).expect("rust marker"),
        want,
        "rust publishes exact bytes"
    );
    assert_eq!(
        mode_of(&shell_stage.join(entry::STAGE_CLAIM_NAME)),
        0o600,
        "shell mode"
    );
    assert_eq!(
        mode_of(&rust_stage.join(entry::STAGE_CLAIM_NAME)),
        0o600,
        "rust mode"
    );
    assert!(
        entry::stage_claim_matches(&rust_stage, "entry", "a/b", NONCE, &source_root()),
        "written marker verifies"
    );
}

#[test]
fn claim_write_rejects_existing_and_bad_kind() {
    // A live marker wins: neither engine replaces it, and neither
    // mints a sibling temp on the way out.
    let twins = Twins::build("init-entry-claim-lived");
    let shell_stage = seed_stage(
        &twins.shell_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    let rust_stage = seed_stage(
        &twins.rust_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    let (lived_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &write_snippet(&shell_stage, "entry", "a/b"),
    );
    let mut cache = MoveCache::default();
    let lived = entry::stage_claim_write(
        &rust_stage,
        "entry",
        "a/b",
        NONCE,
        &source_root(),
        &mut cache,
    );
    assert_eq!((lived_code, lived.is_err()), (1, true), "lived verdicts");
    assert_eq!(tmp_residue(&shell_stage), 0, "shell leaves no sibling");
    assert_eq!(tmp_residue(&rust_stage), 0, "rust leaves no sibling");
    // An unknown kind still publishes its bytes before the
    // trailing gate fails, on both engines.
    let twins = Twins::build("init-entry-claim-bad-kind");
    let shell_stage = twins.shell_home.join("stage");
    let rust_stage = twins.rust_home.join("stage");
    std::fs::create_dir_all(&shell_stage).expect("shell stage");
    std::fs::create_dir_all(&rust_stage).expect("rust stage");
    let (bad_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &write_snippet(&shell_stage, "bogus", "a/b"),
    );
    let mut cache = MoveCache::default();
    let bad = entry::stage_claim_write(
        &rust_stage,
        "bogus",
        "a/b",
        NONCE,
        &source_root(),
        &mut cache,
    );
    assert_eq!((bad_code, bad.is_err()), (1, true), "bad-kind verdicts");
    assert_eq!(
        std::fs::read(shell_stage.join(entry::STAGE_CLAIM_NAME)).expect("shell residue"),
        std::fs::read(rust_stage.join(entry::STAGE_CLAIM_NAME)).expect("rust residue"),
        "bad-kind residue matches"
    );
}

/// Shell probe for `_dot_init_stage_claim_only`.
fn only_snippet(stage: &Path) -> String {
    format!(
        "if _dot_init_stage_claim_only {}; then code=0; else code=$?; fi; printf 'code=%s\\n' \"$code\"",
        sq(&stage.to_string_lossy())
    )
}

#[test]
fn claim_only_matrix() {
    let twins = Twins::build("init-entry-claim-only");
    let shell_only = seed_stage(
        &twins.shell_home,
        "only",
        &claim_body("entry", NONCE, "a/b"),
    );
    let rust_only = seed_stage(&twins.rust_home, "only", &claim_body("entry", NONCE, "a/b"));
    let (only_code, _, _) = shell_run(&twins.shell_home, &nonce_env(), &only_snippet(&shell_only));
    assert_eq!(
        (only_code, entry::stage_claim_only(&rust_only)),
        (0, true),
        "lone claim verdicts"
    );
    for (tag, setup) in [
        ("empty", "empty"),
        ("extra", "extra"),
        ("dotfile", "dotfile"),
    ] {
        let shell_stage = twins.shell_home.join(tag);
        let rust_stage = twins.rust_home.join(tag);
        std::fs::create_dir_all(&shell_stage).expect("shell stage");
        std::fs::create_dir_all(&rust_stage).expect("rust stage");
        if setup != "empty" {
            let name = if setup == "dotfile" {
                ".other"
            } else {
                "other"
            };
            std::fs::write(shell_stage.join(entry::STAGE_CLAIM_NAME), "x").expect("shell seed");
            std::fs::write(rust_stage.join(entry::STAGE_CLAIM_NAME), "x").expect("rust seed");
            std::fs::write(shell_stage.join(name), "x").expect("shell extra");
            std::fs::write(rust_stage.join(name), "x").expect("rust extra");
        }
        let (code, _, _) = shell_run(&twins.shell_home, &nonce_env(), &only_snippet(&shell_stage));
        assert_eq!(
            (code, entry::stage_claim_only(&rust_stage)),
            (1, false),
            "{tag} verdicts"
        );
    }
    let (missing_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &only_snippet(&twins.shell_home.join("nope")),
    );
    assert_eq!(
        (
            missing_code,
            entry::stage_claim_only(&twins.rust_home.join("nope"))
        ),
        (1, false),
        "missing stage verdicts"
    );
}

/// Shell probe for `_dot_init_stage_claim_remove`.
fn remove_snippet(stage: &Path, kind: &str, path: &str) -> String {
    format!(
        "_dot_init_stage_claim_remove {} {} {}; code=$?; printf 'code=%s\\n' \"$code\"",
        sq(&stage.to_string_lossy()),
        sq(kind),
        sq(path)
    )
}

#[test]
fn claim_remove_lifecycle() {
    // Write, remove, remove again: the second removal finds no
    // marker and fails on both engines.
    let twins = Twins::build("init-entry-claim-remove");
    let shell_stage = twins.shell_home.join("stage");
    let rust_stage = twins.rust_home.join("stage");
    std::fs::create_dir_all(&shell_stage).expect("shell stage");
    std::fs::create_dir_all(&rust_stage).expect("rust stage");
    let (write_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &write_snippet(&shell_stage, "parent", "p/q"),
    );
    let mut cache = MoveCache::default();
    entry::stage_claim_write(
        &rust_stage,
        "parent",
        "p/q",
        NONCE,
        &source_root(),
        &mut cache,
    )
    .expect("rust write");
    assert_eq!(write_code, 0, "shell writes the parent claim");
    let (remove_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &remove_snippet(&shell_stage, "parent", "p/q"),
    );
    let removed = entry::stage_claim_remove(&rust_stage, "parent", "p/q", NONCE, &source_root());
    assert_eq!((remove_code, removed.is_ok()), (0, true), "remove verdicts");
    assert!(
        !shell_stage.join(entry::STAGE_CLAIM_NAME).exists(),
        "shell drops the marker"
    );
    assert!(
        !rust_stage.join(entry::STAGE_CLAIM_NAME).exists(),
        "rust drops the marker"
    );
    let (again_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &remove_snippet(&shell_stage, "parent", "p/q"),
    );
    let again = entry::stage_claim_remove(&rust_stage, "parent", "p/q", NONCE, &source_root());
    assert_eq!(
        (again_code, again.is_err()),
        (1, true),
        "second removal verdicts"
    );
    // A mismatched removal fails and keeps the marker.
    let twins = Twins::build("init-entry-claim-remove-no");
    let shell_stage = seed_stage(
        &twins.shell_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    let rust_stage = seed_stage(
        &twins.rust_home,
        "stage",
        &claim_body("entry", NONCE, "a/b"),
    );
    let (no_code, _, _) = shell_run(
        &twins.shell_home,
        &nonce_env(),
        &remove_snippet(&shell_stage, "entry", "a/c"),
    );
    let kept = entry::stage_claim_remove(&rust_stage, "entry", "a/c", NONCE, &source_root());
    assert_eq!((no_code, kept.is_err()), (1, true), "mismatch verdicts");
    assert!(
        shell_stage.join(entry::STAGE_CLAIM_NAME).exists(),
        "shell keeps it"
    );
    assert!(
        rust_stage.join(entry::STAGE_CLAIM_NAME).exists(),
        "rust keeps it"
    );
}

/// Cross-engine interop: each side verifies the other's freshly
/// written claim, pinning the shared byte layout.
#[test]
fn claim_interop() {
    let dir = TempDir::new("init-entry-claim-interop").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let shell_stage = home.join("sh-stage");
    let rust_stage = home.join("rs-stage");
    std::fs::create_dir_all(&shell_stage).expect("shell stage");
    std::fs::create_dir_all(&rust_stage).expect("rust stage");
    let (write_code, _, _) = shell_run(
        &home,
        &nonce_env(),
        &write_snippet(&shell_stage, "entry", "shared"),
    );
    assert_eq!(write_code, 0, "shell writes");
    assert!(
        entry::stage_claim_matches(&shell_stage, "entry", "shared", NONCE, &source_root()),
        "rust verifies the shell claim"
    );
    let mut cache = MoveCache::default();
    entry::stage_claim_write(
        &rust_stage,
        "entry",
        "shared",
        NONCE,
        &source_root(),
        &mut cache,
    )
    .expect("rust writes");
    let (verify_code, _, _) = shell_run(
        &home,
        &nonce_env(),
        &matches_snippet(&rust_stage, "entry", "shared"),
    );
    assert_eq!(verify_code, 0, "shell verifies the rust claim");
}
