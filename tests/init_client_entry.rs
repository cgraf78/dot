//! Differential parity tests for the init per-entry publication
//! staging (`lib/dot/init-client.sh`, the intent/claim family) and
//! the entry-stage validation/publication chapter
//! (`_dot_init_entry_stage_valid` through `_dot_init_publish_one`)
//! against the live shell: the mode-600 line publisher, the entry
//! stage path, the intent record validator, the five stage claim
//! helpers, the stage-directory and stage-content gates, the staged
//! `next` cleanup, and the pending / staged publication driver.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so
//! sibling temps and stage paths never collide. Home-relative
//! outputs (stage paths, intent replies) compare directly; live
//! identities (device/inode) only gate the verdict. Predicates
//! compare verdicts on shared fixtures; mutating rows compare exit
//! status plus the full directory tree as bytes. Journal lines carry
//! live device/inode numbers, so their content crosses as a marker
//! in the tree dump and is compared digit-masked by field position.

use std::os::unix::ffi::OsStrExt as _;
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

/// Fixed run nonce for every staging-chapter row.
const NONCE: &str = "test-nonce-46";
/// Fixed run nonce for the publication chapter's shell rows,
/// mirroring how the engine exports `DOT_INIT_NONCE` before calling
/// into this family. Kept under a distinct name because the staging
/// chapter already owns `NONCE` with a different fixed value; each
/// suite keeps its own bytes.
const PUBLISH_NONCE: &str = "test-nonce-67";
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

/// Run one shell snippet with the init runtime sourced, under
/// `home`, with the run nonce exported. Returns the process
/// (status, stdout, stderr); verdict rows read their own `code=`
/// line off stdout.
fn shell_eval(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
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
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .env("DOT_INIT_NONCE", PUBLISH_NONCE)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No pinned locale: forked tools with locale-sensitive
    // diagnostics (`git`) must speak the same ambient locale on
    // both engines, so pass it through. Blob bytes are
    // locale-free, so parsing stays deterministic.
    for (key, value) in locale_passthrough() {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Ambient locale passthrough (the staged-clone lane precedent).
fn locale_passthrough() -> Vec<(String, String)> {
    ["LANG", "LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Verdict probe: run `snippet` (which must end by setting
/// `$code`) and report that verdict. A snippet that never
/// reports (a harness bug, never a pass) yields 99.
fn verdict(home: &Path, snippet: &str) -> i32 {
    let (_, stdout, _) = shell_eval(home, &format!("{snippet}; printf 'code=%s\\n' \"$code\""));
    String::from_utf8_lossy(&stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("code=")
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(99)
}

/// Single-quote a path for snippet embedding.
fn sq_path(path: &Path) -> String {
    sq(&path.to_string_lossy())
}

/// Write fixture `bytes` at `home/rel`, creating parents.
fn write_rel(home: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = home.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// lstat mode bits (never follows symlinks): the publication
/// chapter's `mode_of`, kept under a distinct name because the
/// staging chapter's `mode_of` follows symlinks
/// (`std::fs::metadata`) while this one reads the link itself
/// (`std::fs::symlink_metadata`). Both stay so neither suite
/// changes behavior.
fn mode_of_nofollow(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

/// `stat -c '%d:%i'` identity string of one path.
fn identity_of(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).expect("stat fixture");
    format!("{}:{}", meta.dev(), meta.ino())
}

/// One `_dot_init_entry_stage_valid` row on a shared fixture:
///
/// the shell verdict and the Rust verdict must agree.
fn valid_row(tag: &str, build: &dyn Fn(&Path), identity: Option<&str>, shell_extra: &str) {
    let dir = TempDir::new(tag).expect("temp dir");
    build(dir.path());
    let identity_arg = match identity {
        // Portable identity read (mirrors `_dot_path_identity`):
        // bare GNU `stat -c` fails on BSD macOS, which silently
        // empties the identity and vacuous-passes this row.
        Some("live") => {
            "\"$(stat -c '%d:%i' stage 2>/dev/null || stat -f '%d:%i' stage 2>/dev/null)\""
                .to_string()
        }
        Some(fixed) => sq(fixed),
        None => String::new(),
    };
    let shell = verdict(
        dir.path(),
        &format!(
            "if _dot_init_entry_stage_valid stage {identity_arg} {shell_extra}; then code=0; else code=1; fi"
        ),
    );
    let expected = match identity {
        Some("live") => Some(identity_of(&dir.path().join("stage"))),
        Some(fixed) => Some(fixed.to_string()),
        None => None,
    };
    let rust = entry::entry_stage_valid(&dir.path().join("stage"), expected.as_deref());
    assert_eq!(shell == 0, rust, "entry_stage_valid disagrees on {tag}");
}

#[test]
fn stage_valid_missing_is_false() {
    valid_row("valid-missing", &|_| {}, None, "");
}

#[test]
fn stage_valid_regular_file_is_false() {
    valid_row(
        "valid-file",
        &|home| {
            write_rel(home, "stage", b"not a dir");
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_symlink_to_dir_is_false() {
    valid_row(
        "valid-link",
        &|home| {
            let target = home.join("target");
            std::fs::create_dir_all(&target).expect("fixture dir");
            chmod(&target, 0o700);
            std::os::unix::fs::symlink(&target, home.join("stage")).expect("fixture link");
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_dangling_symlink_is_false() {
    valid_row(
        "valid-dangling",
        &|home| {
            std::os::unix::fs::symlink("nowhere", home.join("stage")).expect("fixture link");
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_owned_700_is_true() {
    valid_row(
        "valid-700",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_755_is_false() {
    valid_row(
        "valid-755",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o755);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_777_is_false() {
    valid_row(
        "valid-777",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o777);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_750_is_false() {
    valid_row(
        "valid-750",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o750);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_600_passes_the_mask() {
    // `0600 & 077 == 0`: the gate masks bits, it does not compare
    // against `0700`.
    valid_row(
        "valid-600",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o600);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_setuid_only_passes_the_mask() {
    // `04700 & 077 == 0`: setuid alone does not fail the gate.
    valid_row(
        "valid-setuid",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o4700);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_setuid_with_group_fails_the_mask() {
    valid_row(
        "valid-setuid-group",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o4750);
        },
        None,
        "",
    );
}

#[test]
fn stage_valid_matching_identity_is_true() {
    valid_row(
        "valid-id-match",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        Some("live"),
        "",
    );
}

#[test]
fn stage_valid_wrong_identity_is_false() {
    valid_row(
        "valid-id-wrong",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        Some("0:0"),
        "",
    );
}

#[test]
fn stage_valid_empty_identity_skips_the_check() {
    valid_row(
        "valid-id-empty",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        Some(""),
        "",
    );
}

#[test]
fn stage_valid_fifo_is_false() {
    valid_row(
        "valid-fifo",
        &|home| {
            let status = Command::new("mkfifo")
                .arg(home.join("stage"))
                .status()
                .expect("spawn mkfifo");
            assert!(status.success(), "mkfifo fixture");
        },
        None,
        "",
    );
}

/// One `_dot_init_entry_stage_only_next` row on a shared
/// fixture: the shell verdict and the Rust verdict must agree.
fn only_next_row(tag: &str, build: &dyn Fn(&Path)) {
    let dir = TempDir::new(tag).expect("temp dir");
    build(dir.path());
    let shell = verdict(
        dir.path(),
        "if _dot_init_entry_stage_only_next stage; then code=0; else code=1; fi",
    );
    let rust = entry::entry_stage_only_next(&dir.path().join("stage"));
    assert_eq!(shell == 0, rust, "entry_stage_only_next disagrees on {tag}");
}

#[test]
fn only_next_missing_stage_passes_vacuously() {
    // `nullglob`: a missing stage expands to nothing.
    only_next_row("only-missing", &|_| {});
}

#[test]
fn only_next_file_stage_passes_vacuously() {
    only_next_row("only-file", &|home| {
        write_rel(home, "stage", b"not a dir");
    });
}

#[test]
fn only_next_empty_dir_passes() {
    only_next_row("only-empty", &|home| {
        std::fs::create_dir_all(home.join("stage")).expect("fixture dir");
    });
}

#[test]
fn only_next_next_file_passes() {
    only_next_row("only-next", &|home| {
        write_rel(home, "stage/next", b"candidate");
    });
}

#[test]
fn only_next_claim_passes() {
    only_next_row("only-claim", &|home| {
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn only_next_next_plus_claim_passes() {
    only_next_row("only-both", &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn only_next_symlink_next_passes() {
    // The gate matches basenames, never types.
    only_next_row("only-link", &|home| {
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        std::os::unix::fs::symlink("anywhere", stage.join("next")).expect("fixture link");
    });
}

#[test]
fn only_next_claim_dir_passes() {
    only_next_row("only-claim-dir", &|home| {
        std::fs::create_dir_all(home.join("stage/.dot-init-stage-claim-v1")).expect("fixture dir");
    });
}

#[test]
fn only_next_extra_file_fails() {
    only_next_row("only-extra", &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/stray", b"intruder");
    });
}

#[test]
fn only_next_hidden_extra_fails() {
    // `dotglob`: hidden entries count too.
    only_next_row("only-hidden", &|home| {
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
        write_rel(home, "stage/.stray", b"intruder");
    });
}

#[test]
fn only_next_extra_dir_fails() {
    only_next_row("only-dir", &|home| {
        std::fs::create_dir_all(home.join("stage/next")).expect("fixture dir");
        std::fs::create_dir_all(home.join("stage/other")).expect("fixture dir");
    });
}

/// Whether `rel` (home-relative, slash-separated) is a journal
/// file carrying live device/inode numbers: its content crosses
/// as a marker in the tree dump and is compared masked.
fn is_journal(rel: &[u8]) -> bool {
    rel == b"tx/intent" || rel.starts_with(b"tx/parent-intent.")
}

/// Full recursive listing of `dir` as bytes: sorted relative
/// paths with kind, octal mode, link target, and content.
/// Journal files cross as the marker `JOURNAL` (see
/// [`journals_masked`]).
fn tree_dump(dir: &Path) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut names: Vec<_> = std::fs::read_dir(&current)
            .expect("read fixture")
            .map(|item| item.expect("dir entry"))
            .collect();
        names.sort_by_key(|a| a.file_name());
        for item in names {
            let full = item.path();
            let rel = full
                .strip_prefix(dir)
                .expect("relative")
                .as_os_str()
                .as_bytes()
                .to_vec();
            let meta = std::fs::symlink_metadata(&full).expect("stat fixture");
            let kind: &[u8] = if meta.file_type().is_symlink() {
                b"L"
            } else if meta.is_dir() {
                b"D"
            } else if meta.is_file() {
                b"F"
            } else {
                b"O"
            };
            let mut row = rel.clone();
            row.extend_from_slice(b"\0");
            row.extend_from_slice(kind);
            row.extend_from_slice(b"\0");
            row.extend_from_slice(format!("{:o}", meta.permissions().mode() & 0o7777).as_bytes());
            row.extend_from_slice(b"\0");
            if kind == b"L" {
                row.extend_from_slice(
                    std::fs::read_link(&full)
                        .expect("read link")
                        .as_os_str()
                        .as_bytes(),
                );
            }
            row.extend_from_slice(b"\0");
            if kind == b"F" {
                if is_journal(&rel) {
                    row.extend_from_slice(b"JOURNAL");
                } else {
                    row.extend_from_slice(&std::fs::read(&full).expect("read file"));
                }
            }
            rows.push((rel, row));
            if kind == b"D" {
                stack.push(full);
            }
        }
    }
    rows.sort();
    let mut out = Vec::new();
    for (_, row) in rows {
        out.extend_from_slice(&row);
        out.push(0);
    }
    out
}

/// Mask the digit-bound fields of one journal line by position:
/// every all-digit field at a masked index becomes `N`, so live
/// bindings compare as shapes while every other byte compares
/// exactly. A dash where digits belong (or vice versa) still
/// diverges the mask.
fn mask_fields(line: &[u8], masked: &[usize]) -> Vec<u8> {
    let mut line = line.to_vec();
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    line.split(|byte| *byte == b'\t')
        .enumerate()
        .map(|(index, field)| {
            if masked.contains(&index)
                && !field.is_empty()
                && field.iter().all(|byte| byte.is_ascii_digit())
            {
                b"N".as_slice()
            } else {
                field
            }
        })
        .collect::<Vec<_>>()
        .join(b"\t".as_slice())
}

/// Masked journal contents under `home/tx`: the intent record
/// (nine fields, device/inode positions masked) plus any
/// parent-intent records (six fields, device/inode masked).
/// Returns (name, masked bytes) pairs sorted by name.
fn journals_masked(home: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let tx = home.join("tx");
    let mut names: Vec<_> = std::fs::read_dir(&tx)
        .expect("read tx")
        .map(|item| item.expect("dir entry").file_name())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let raw = name.as_os_str().as_bytes();
        let masked = if raw == b"intent" {
            Some(mask_fields(
                &std::fs::read(tx.join(&name)).expect("read intent"),
                &[5, 6, 7, 8],
            ))
        } else if raw.starts_with(b"parent-intent.") {
            Some(mask_fields(
                &std::fs::read(tx.join(&name)).expect("read parent intent"),
                &[3, 4],
            ))
        } else {
            None
        };
        if let Some(masked) = masked {
            out.push((raw.to_vec(), masked));
        }
    }
    out
}

/// Raw tab fields of the intent record at `home/tx/intent`.
fn intent_fields(home: &Path) -> Vec<Vec<u8>> {
    let mut bytes = std::fs::read(home.join("tx/intent")).expect("read intent");
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
        .split(|byte| *byte == b'\t')
        .map(<[u8]>::to_vec)
        .collect()
}

/// One `_dot_init_discard_staged_next` row on twin fixtures: the
/// shell verdict and the Rust verdict must agree, and the two
/// home trees must read byte-identical afterwards.
fn discard_row(tag: &str, build: &dyn Fn(&Path)) {
    let twins = Twins::build(tag);
    for home in [&twins.shell_home, &twins.rust_home] {
        build(home);
    }
    let shell = verdict(
        &twins.shell_home,
        "if _dot_init_discard_staged_next stage; then code=0; else code=1; fi",
    );
    let rust = entry::discard_staged_next(&twins.rust_home.join("stage")).is_ok();
    assert_eq!(
        shell == 0,
        rust,
        "discard_staged_next verdict disagrees on {tag}"
    );
    assert_eq!(
        tree_dump(&twins.shell_home),
        tree_dump(&twins.rust_home),
        "discard_staged_next tree disagrees on {tag}"
    );
}

#[test]
fn discard_claim_only_without_next_passes() {
    discard_row("discard-claim", &|home| {
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn discard_next_file_is_removed() {
    discard_row("discard-file", &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn discard_dangling_symlink_is_removed() {
    discard_row("discard-dangling", &|home| {
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        std::os::unix::fs::symlink("nowhere", stage.join("next")).expect("fixture link");
    });
}

#[test]
fn discard_live_symlink_is_removed() {
    discard_row("discard-link", &|home| {
        let pointed = write_rel(home, "pointed", b"pointed-to");
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        std::os::unix::fs::symlink(&pointed, stage.join("next")).expect("fixture link");
    });
}

#[test]
fn discard_extra_file_refuses() {
    discard_row("discard-extra", &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/stray", b"intruder");
    });
}

#[test]
fn discard_next_dir_refuses() {
    discard_row("discard-dir", &|home| {
        std::fs::create_dir_all(home.join("stage/next")).expect("fixture dir");
    });
}

#[test]
fn discard_next_fifo_refuses() {
    discard_row("discard-fifo", &|home| {
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        let status = Command::new("mkfifo")
            .arg(stage.join("next"))
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo fixture");
    });
}

#[test]
fn discard_missing_stage_passes_vacuously() {
    // The content gate passes on a missing stage and the missing
    // candidate is already discarded.
    discard_row("discard-missing", &|_| {});
}

/// Run `git` for fixtures with a pinned identity and no commit
/// hooks: the ambient user config (`core.hooksPath` pointing at
/// the dotfiles hook entry) must not slow down or reject
/// fixture commits. Only the shared fixture repo commits this
/// way; both engines run the same `git show`/`hash-object` the
/// shell would, hooks included.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// Capture one git stdout line for fixtures.
fn git_line(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// One publish fixture: a git repo outside both homes plus twin
/// homes with empty transaction directories.
struct PublishWorld {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    git_dir: String,
    commit: String,
    app_oid: String,
    run_oid: String,
    link_oid: String,
    newline_oid: String,
    empty_oid: String,
}

fn publish_world(tag: &str) -> PublishWorld {
    let dir = TempDir::new(tag).expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q"]);
    let app = write_rel(&repo, "cfg/app.conf", b"app-config-bytes\n");
    chmod(&app, 0o644);
    let run = write_rel(&repo, "run.sh", b"#!/bin/sh\necho hi\n");
    chmod(&run, 0o755);
    std::os::unix::fs::symlink("app-target", repo.join("cfg/link")).expect("fixture link");
    std::os::unix::fs::symlink("bad\ntarget", repo.join("cfg/nl-link")).expect("fixture link");
    let empty = git_line(&repo, &["hash-object", "-w", "--stdin"]);
    assert!(!empty.is_empty(), "empty blob oid");
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{empty},cfg/empty-link"),
        ],
    );
    git(&repo, &["commit", "-qm", "fixture"]);
    let commit = git_line(&repo, &["rev-parse", "HEAD"]);
    let shell_home = dir.path().join("sh-home");
    let rust_home = dir.path().join("rs-home");
    for home in [&shell_home, &rust_home] {
        std::fs::create_dir_all(home.join("tx")).expect("tx dir");
    }
    // `rev-parse HEAD:<path>`: `hash-object <path>` follows
    // symlinks and fails on dangling targets, while rev-parse
    // reports the committed blob whatever its mode.
    PublishWorld {
        _dir: dir,
        shell_home,
        rust_home,
        git_dir: repo.join(".git").to_string_lossy().into_owned(),
        commit,
        app_oid: git_line(&repo, &["rev-parse", "HEAD:cfg/app.conf"]),
        run_oid: git_line(&repo, &["rev-parse", "HEAD:run.sh"]),
        link_oid: git_line(&repo, &["rev-parse", "HEAD:cfg/link"]),
        newline_oid: git_line(&repo, &["rev-parse", "HEAD:cfg/nl-link"]),
        empty_oid: empty,
    }
}

/// Seed a pending intent in `home` through the live shell, so the
/// stage path binds the run nonce exactly like production.
fn seed_pending(home: &Path, mode: &str, oid: &str, path: &str) {
    let snippet = [
        "if _dot_init_publish_intent tx/intent ",
        &sq(mode),
        " ",
        &sq(oid),
        " ",
        &sq(path),
        "; then code=0; else code=1; fi",
    ]
    .concat();
    assert_eq!(verdict(home, &snippet), 0, "seed pending intent for {path}");
}

/// Seed a staged intent in `home`: a pending intent plus a
/// claimed stage bound to its identity, through the live shell.
fn seed_staged(home: &Path, mode: &str, oid: &str, path: &str) {
    seed_pending(home, mode, oid, path);
    // The plain `mkdir` below needs the parent the publication
    // itself would provision first.
    let snippet = [
        "code=99; _dot_init_parent_directories tx ",
        &sq(path),
        " && _dot_init_entry_stage ",
        &sq(path),
        " && stage=$REPLY && mkdir \"$stage\" && chmod 0700 \"$stage\"",
        " && _dot_init_stage_claim_write \"$stage\" entry ",
        &sq(path),
        " && id=$(stat -c '%d:%i' \"$stage\" 2>/dev/null || stat -f '%d:%i' \"$stage\" 2>/dev/null) && dev=${id%%:*} && ino=${id#*:}",
        " && line=$(printf 'staged\\t%s\\t%s\\t%s\\t%s\\t%s\\t%s\\t-\\t-' ",
        &sq(mode),
        " ",
        &sq(oid),
        " ",
        &sq(path),
        " \"${stage#\"$HOME\"/}\" \"$dev\" \"$ino\")",
        " && _dot_init_write_private_line tx/intent \"$line\" true && code=0",
    ]
    .concat();
    assert_eq!(verdict(home, &snippet), 0, "stage the intent for {path}");
}

/// Shell-backed closures for [`entry::PublishOneInputs`]: every
/// unmerged lane runs as the live shell function, so the rows pin
/// this chapter's orchestration (ordering, phase transitions,
/// blob handling, moves) rather than the neighbors.
struct Oracle {
    home: PathBuf,
}

impl Oracle {
    fn ensure_parents(&self, transaction: &Path, path: &str) -> dot::errors::Result<()> {
        let snippet = [
            "if _dot_init_parent_directories ",
            &sq_path(transaction),
            " ",
            &sq(path),
            "; then code=0; else code=1; fi",
        ]
        .concat();
        if verdict(&self.home, &snippet) == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "oracle parents failed",
            })
        }
    }

    fn read_intent(
        &self,
        intent: &Path,
        mode: &str,
        oid: &str,
        path: &str,
    ) -> dot::errors::Result<entry::EntryIntent> {
        let snippet = [
            "if _dot_init_entry_intent ",
            &sq_path(intent),
            " ",
            &sq(mode),
            " ",
            &sq(oid),
            " ",
            &sq(path),
            "; then code=0; else code=1; fi; printf 'code=%s\\n' \"$code\"; printf 'reply=%s\\n' \"$REPLY\"",
        ]
        .concat();
        let (_, stdout, _) = shell_eval(&self.home, &snippet);
        let text = String::from_utf8_lossy(&stdout);
        let code = text
            .lines()
            .find_map(|line| {
                line.strip_prefix("code=")
                    .and_then(|code| code.parse::<i32>().ok())
            })
            .unwrap_or(99);
        if code != 0 {
            return Err(dot::errors::Error::Usage {
                message: "oracle intent failed",
            });
        }
        let reply = text
            .lines()
            .find_map(|line| line.strip_prefix("reply="))
            .ok_or(dot::errors::Error::Usage {
                message: "oracle intent printed no reply",
            })?;
        let fields: Vec<&str> = reply.split('\t').collect();
        if fields.len() != 6 {
            return Err(dot::errors::Error::Usage {
                message: "oracle intent has the wrong shape",
            });
        }
        Ok(entry::EntryIntent {
            phase: fields[0].to_string(),
            stage: fields[1].to_string(),
            dev: fields[2].to_string(),
            ino: fields[3].to_string(),
            next_dev: fields[4].to_string(),
            next_ino: fields[5].to_string(),
        })
    }

    fn claim_matches(&self, stage: &Path, path: &str) -> bool {
        let snippet = [
            "if _dot_init_stage_claim_matches ",
            &sq_path(stage),
            " entry ",
            &sq(path),
            "; then code=0; else code=1; fi",
        ]
        .concat();
        verdict(&self.home, &snippet) == 0
    }

    fn claim_write(&self, stage: &Path, path: &str) -> dot::errors::Result<()> {
        let snippet = [
            "if _dot_init_stage_claim_write ",
            &sq_path(stage),
            " entry ",
            &sq(path),
            "; then code=0; else code=1; fi",
        ]
        .concat();
        if verdict(&self.home, &snippet) == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "oracle claim write failed",
            })
        }
    }

    fn claim_remove(&self, stage: &Path, path: &str) -> dot::errors::Result<()> {
        let snippet = [
            "if _dot_init_stage_claim_remove ",
            &sq_path(stage),
            " entry ",
            &sq(path),
            "; then code=0; else code=1; fi",
        ]
        .concat();
        if verdict(&self.home, &snippet) == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "oracle claim remove failed",
            })
        }
    }

    fn write_line(&self, file: &Path, line: &str, replace: bool) -> dot::errors::Result<()> {
        let snippet = [
            "if _dot_init_write_private_line ",
            &sq_path(file),
            " ",
            &sq(line),
            " ",
            if replace { "true" } else { "false" },
            "; then code=0; else code=1; fi",
        ]
        .concat();
        if verdict(&self.home, &snippet) == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "oracle line write failed",
            })
        }
    }

    fn candidate_matches(
        &self,
        git_dir: &str,
        commit: &str,
        mode: &str,
        oid: &str,
        path: &str,
    ) -> bool {
        let snippet = [
            "if _dot_init_candidate_matches_git ",
            &sq(git_dir),
            " ",
            &sq(commit),
            " ",
            &sq(mode),
            " ",
            &sq(oid),
            " ",
            &sq(path),
            "; then code=0; else code=1; fi",
        ]
        .concat();
        verdict(&self.home, &snippet) == 0
    }
}

/// Run [`entry::publish_one`] in `home` with shell-backed
/// neighbor closures.
fn rust_publish(home: &Path, world: &PublishWorld, mode: &str, oid: &str, path: &str) -> bool {
    let oracle = Oracle {
        home: home.to_path_buf(),
    };
    let tx = home.join("tx");
    let intent = tx.join("intent");
    let ensure = |transaction: &Path, entry: &str| oracle.ensure_parents(transaction, entry);
    let read = |file: &Path, gimode: &str, gioid: &str, gipath: &str| {
        oracle.read_intent(file, gimode, gioid, gipath)
    };
    let matches = |stage: &Path, entry: &str| oracle.claim_matches(stage, entry);
    let write = |stage: &Path, entry: &str| oracle.claim_write(stage, entry);
    let remove = |stage: &Path, entry: &str| oracle.claim_remove(stage, entry);
    let line = |file: &Path, body: &str, replace: bool| oracle.write_line(file, body, replace);
    let candidate = |git_dir: &str, commit: &str, gimode: &str, gioid: &str, gipath: &str| {
        oracle.candidate_matches(git_dir, commit, gimode, gioid, gipath)
    };
    let inputs = entry::PublishOneInputs {
        home,
        transaction: &tx,
        intent: &intent,
        git_dir: world.git_dir.as_str(),
        commit: world.commit.as_str(),
        mode,
        oid,
        path,
        mask: dot::temp::read_umask().expect("read umask"),
        ensure_parents: &ensure,
        read_intent: &read,
        claim_matches: &matches,
        claim_write: &write,
        claim_remove: &remove,
        write_line: &line,
        candidate_matches: &candidate,
    };
    let mut moves = MoveCache::default();
    entry::publish_one(&inputs, &mut moves).is_ok()
}

/// Run the live `_dot_init_publish_one` in `home`.
fn shell_publish(home: &Path, world: &PublishWorld, mode: &str, oid: &str, path: &str) -> i32 {
    let snippet = [
        "if _dot_init_publish_one tx tx/intent ",
        &sq(&world.git_dir),
        " ",
        &sq(&world.commit),
        " ",
        &sq(mode),
        " ",
        &sq(oid),
        " ",
        &sq(path),
        "; then code=0; else code=1; fi",
    ]
    .concat();
    verdict(home, &snippet)
}

/// Stage directories left behind under `home`, at any depth:
/// nested entries stage under their parent directory
/// (`cfg/.dot-init-entry.<nonce>.<hash>`), not under the root.
fn stage_leftovers(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(current) = stack.pop() {
        for item in std::fs::read_dir(&current).expect("read home") {
            let item = item.expect("dir entry");
            let full = item.path();
            if item
                .file_name()
                .as_os_str()
                .as_bytes()
                .starts_with(b".dot-init-entry.")
            {
                out.push(full.clone());
            }
            let meta = full.symlink_metadata().expect("stat home");
            if meta.is_dir() && !meta.file_type().is_symlink() {
                stack.push(full);
            }
        }
    }
    out.sort();
    out
}

/// One `_dot_init_publish_one` row on twin worlds: exit status,
/// home trees, and masked journals must agree across engines.
fn publish_row(tag: &str, world: &PublishWorld, mode: &str, oid: &str, path: &str) -> (i32, bool) {
    let shell = shell_publish(&world.shell_home, world, mode, oid, path);
    let rust = rust_publish(&world.rust_home, world, mode, oid, path);
    assert_eq!(shell == 0, rust, "publish_one verdict disagrees on {tag}");
    assert_eq!(
        tree_dump(&world.shell_home),
        tree_dump(&world.rust_home),
        "publish_one tree disagrees on {tag}"
    );
    assert_eq!(
        journals_masked(&world.shell_home),
        journals_masked(&world.rust_home),
        "publish_one journals disagree on {tag}"
    );
    (shell, rust)
}

/// The prepared intent must bind the published target on both
/// sides: the intent's `next` device/inode equals the live
/// target identity.
fn assert_target_bound(home: &Path, path: &str) {
    let fields = intent_fields(home);
    assert_eq!(fields.len(), 9, "prepared intent has nine fields");
    assert_eq!(fields[0], b"prepared", "intent reached prepared");
    let live = dot::temp::identity_string(
        dot::temp::path_identity(&home.join(path)).expect("stat target"),
    );
    let bound = format!(
        "{}:{}",
        String::from_utf8_lossy(&fields[7]),
        String::from_utf8_lossy(&fields[8])
    );
    assert_eq!(live, bound, "intent binds the published target");
}

#[test]
fn publish_regular_file_nested() {
    let world = publish_world("publish-regular");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100644", &world.app_oid, "cfg/app.conf");
    }
    let (shell, _) = publish_row(
        "publish-regular",
        &world,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    assert_eq!(shell, 0, "regular publish succeeds");
    for home in [&world.shell_home, &world.rust_home] {
        assert_eq!(
            std::fs::read(home.join("cfg/app.conf")).expect("read target"),
            b"app-config-bytes\n",
            "published bytes"
        );
        assert_eq!(
            mode_of_nofollow(&home.join("cfg/app.conf")) & 0o111,
            0,
            "no exec bit on 644"
        );
        assert!(stage_leftovers(home).is_empty(), "stage released");
        assert_target_bound(home, "cfg/app.conf");
    }
}

#[test]
fn publish_executable_top_level() {
    let world = publish_world("publish-exec");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100755", &world.run_oid, "run.sh");
    }
    let (shell, _) = publish_row("publish-exec", &world, "100755", &world.run_oid, "run.sh");
    assert_eq!(shell, 0, "executable publish succeeds");
    for home in [&world.shell_home, &world.rust_home] {
        assert_eq!(
            std::fs::read(home.join("run.sh")).expect("read target"),
            b"#!/bin/sh\necho hi\n",
            "published bytes"
        );
        assert_ne!(
            mode_of_nofollow(&home.join("run.sh")) & 0o111,
            0,
            "exec bit on 755"
        );
        assert!(stage_leftovers(home).is_empty(), "stage released");
        assert_target_bound(home, "run.sh");
    }
}

#[test]
fn publish_symlink_candidate_check_refuses() {
    // The candidate gate hashes `readlink` output with its
    // trailing newline still attached, so it never matches the
    // blob: symlink publication fails on both engines after
    // linking the candidate, leaving the staged intent and the
    // linked `next` behind. The port mirrors the oracle quirk
    // for quirk, including the leftover shape.
    let world = publish_world("publish-link");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "120000", &world.link_oid, "cfg/link");
    }
    let (shell, _) = publish_row(
        "publish-link",
        &world,
        "120000",
        &world.link_oid,
        "cfg/link",
    );
    assert_eq!(shell, 1, "symlink candidate check refuses");
    for home in [&world.shell_home, &world.rust_home] {
        assert_stage_bound(home);
        let stages = stage_leftovers(home);
        let next = stages[0].join("next");
        assert_eq!(
            std::fs::read_link(&next)
                .expect("read next")
                .to_string_lossy(),
            "app-target",
            "linked candidate kept"
        );
        assert!(
            !home.join("cfg/link").exists()
                && std::fs::symlink_metadata(home.join("cfg/link")).is_err(),
            "no target published"
        );
    }
}

#[test]
fn publish_from_staged_intent() {
    // A previous run bound the stage but never generated the
    // candidate: publication resumes from the staged record.
    let world = publish_world("publish-staged");
    for home in [&world.shell_home, &world.rust_home] {
        seed_staged(home, "100644", &world.app_oid, "cfg/app.conf");
    }
    let (shell, _) = publish_row(
        "publish-staged",
        &world,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    assert_eq!(shell, 0, "staged publish succeeds");
    for home in [&world.shell_home, &world.rust_home] {
        assert_eq!(
            std::fs::read(home.join("cfg/app.conf")).expect("read target"),
            b"app-config-bytes\n",
            "published bytes"
        );
        assert!(stage_leftovers(home).is_empty(), "stage released");
        assert_target_bound(home, "cfg/app.conf");
    }
}

#[test]
fn publish_rejects_intent_mismatch() {
    // The call's oid differs from the recorded one: the intent
    // gate refuses before any stage exists, leaving the pending
    // record untouched.
    let bogus = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let world = publish_world("publish-bad-oid");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100644", &world.app_oid, "cfg/app.conf");
    }
    let shell = shell_publish(&world.shell_home, &world, "100644", bogus, "cfg/app.conf");
    let rust = rust_publish(&world.rust_home, &world, "100644", bogus, "cfg/app.conf");
    assert_eq!(shell, 1, "mismatched oid refuses");
    assert_eq!(shell == 0, rust, "verdict disagrees");
    assert_eq!(
        tree_dump(&world.shell_home),
        tree_dump(&world.rust_home),
        "mismatch tree disagrees"
    );
    assert_eq!(
        journals_masked(&world.shell_home),
        journals_masked(&world.rust_home),
        "mismatch journals disagree"
    );
    for home in [&world.shell_home, &world.rust_home] {
        assert!(!home.join("cfg/app.conf").exists(), "no target published");
        assert!(stage_leftovers(home).is_empty(), "no stage claimed");
        assert_eq!(intent_fields(home)[0], b"pending", "intent stays pending");
    }
}

#[test]
fn publish_rejects_blob_mismatch() {
    // The recorded oid matches the call but not the commit: the
    // candidate gate refuses after staging, leaving the staged
    // intent, the claimed stage, and the generated candidate.
    let bogus = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let world = publish_world("publish-bad-blob");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100644", bogus, "cfg/app.conf");
    }
    let (shell, _) = publish_row("publish-bad-blob", &world, "100644", bogus, "cfg/app.conf");
    assert_eq!(shell, 1, "mismatched blob refuses");
    for home in [&world.shell_home, &world.rust_home] {
        assert!(!home.join("cfg/app.conf").exists(), "no target published");
        assert_stage_bound(home);
        let stages = stage_leftovers(home);
        let next = stages[0].join("next");
        assert_eq!(
            std::fs::read(&next).expect("read next"),
            b"app-config-bytes\n",
            "generated candidate kept"
        );
    }
}

#[test]
fn publish_failed_blob_leaves_partial_next() {
    // `git show` fails: the shell's redirect still creates the
    // candidate file, so both engines leave the empty bytes
    // behind for rollback to sweep.
    let world = publish_world("publish-bad-commit");
    let commit = world.commit.clone();
    let missing = "0".repeat(40);
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100644", &world.app_oid, "cfg/app.conf");
    }
    let shell_home = world.shell_home.clone();
    let rust_home = world.rust_home.clone();
    let shell = shell_publish_at(
        &shell_home,
        &world.git_dir,
        &missing,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    let rust = rust_publish_at(
        &rust_home,
        &world.git_dir,
        &missing,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    assert_eq!(commit.len(), 40, "fixture commit recorded");
    assert_eq!(shell, 1, "failed blob refuses");
    assert_eq!(shell == 0, rust, "verdict disagrees");
    assert_eq!(
        tree_dump(&world.shell_home),
        tree_dump(&world.rust_home),
        "partial-next tree disagrees"
    );
    assert_eq!(
        journals_masked(&world.shell_home),
        journals_masked(&world.rust_home),
        "partial-next journals disagree"
    );
    for home in [&world.shell_home, &world.rust_home] {
        assert_stage_bound(home);
        let stages = stage_leftovers(home);
        let next = stages[0].join("next");
        assert_eq!(
            std::fs::read(&next).expect("read next"),
            b"",
            "empty partial next"
        );
    }
}

/// Run the live `_dot_init_publish_one` in `home` against an
/// explicit commit (for the missing-blob row).
fn shell_publish_at(
    home: &Path,
    git_dir: &str,
    commit: &str,
    mode: &str,
    oid: &str,
    path: &str,
) -> i32 {
    let snippet = [
        "if _dot_init_publish_one tx tx/intent ",
        &sq(git_dir),
        " ",
        &sq(commit),
        " ",
        &sq(mode),
        " ",
        &sq(oid),
        " ",
        &sq(path),
        "; then code=0; else code=1; fi",
    ]
    .concat();
    verdict(home, &snippet)
}

/// Run [`entry::publish_one`] in `home` against an explicit
/// commit, with shell-backed neighbor closures.
fn rust_publish_at(
    home: &Path,
    git_dir: &str,
    commit: &str,
    mode: &str,
    oid: &str,
    path: &str,
) -> bool {
    let oracle = Oracle {
        home: home.to_path_buf(),
    };
    let tx = home.join("tx");
    let intent = tx.join("intent");
    let ensure = |transaction: &Path, entry: &str| oracle.ensure_parents(transaction, entry);
    let read = |file: &Path, gimode: &str, gioid: &str, gipath: &str| {
        oracle.read_intent(file, gimode, gioid, gipath)
    };
    let matches = |stage: &Path, entry: &str| oracle.claim_matches(stage, entry);
    let write = |stage: &Path, entry: &str| oracle.claim_write(stage, entry);
    let remove = |stage: &Path, entry: &str| oracle.claim_remove(stage, entry);
    let line = |file: &Path, body: &str, replace: bool| oracle.write_line(file, body, replace);
    let candidate = |dir: &str, rev: &str, gimode: &str, gioid: &str, gipath: &str| {
        oracle.candidate_matches(dir, rev, gimode, gioid, gipath)
    };
    let inputs = entry::PublishOneInputs {
        home,
        transaction: &tx,
        intent: &intent,
        git_dir,
        commit,
        mode,
        oid,
        path,
        mask: dot::temp::read_umask().expect("read umask"),
        ensure_parents: &ensure,
        read_intent: &read,
        claim_matches: &matches,
        claim_write: &write,
        claim_remove: &remove,
        write_line: &line,
        candidate_matches: &candidate,
    };
    let mut moves = MoveCache::default();
    entry::publish_one(&inputs, &mut moves).is_ok()
}

/// The staged intent must bind its stage on both sides: fields
/// five/six equal the live stage identity, and the `next` pair
/// is still `-`.
fn assert_stage_bound(home: &Path) {
    let fields = intent_fields(home);
    assert_eq!(fields.len(), 9, "staged intent has nine fields");
    assert_eq!(fields[0], b"staged", "intent reached staged");
    assert_eq!(
        (&fields[7], &fields[8]),
        (&b"-".to_vec(), &b"-".to_vec()),
        "next unbound"
    );
    let stages = stage_leftovers(home);
    assert_eq!(stages.len(), 1, "one stage left behind");
    let live = identity_of(&stages[0]);
    let bound = format!(
        "{}:{}",
        String::from_utf8_lossy(&fields[5]),
        String::from_utf8_lossy(&fields[6])
    );
    assert_eq!(live, bound, "intent binds the leftover stage");
}

#[test]
fn publish_rejects_unsupported_mode() {
    // The mode gate runs after the staged cleanup: the intent is
    // staged, the stage is claimed, and no candidate exists.
    let world = publish_world("publish-bad-mode");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100666", &world.app_oid, "cfg/app.conf");
    }
    let (shell, _) = publish_row(
        "publish-bad-mode",
        &world,
        "100666",
        &world.app_oid,
        "cfg/app.conf",
    );
    assert_eq!(shell, 1, "unsupported mode refuses");
    for home in [&world.shell_home, &world.rust_home] {
        assert_stage_bound(home);
        let stages = stage_leftovers(home);
        assert!(!stages[0].join("next").exists(), "no candidate generated");
    }
}

#[test]
fn publish_rejects_newline_link_target() {
    // Command substitution preserves the blob's trailing newline
    // past the `printf .` guard, so the value gate refuses: the
    // port must not strip newlines before the check.
    let world = publish_world("publish-nl-link");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "120000", &world.newline_oid, "cfg/nl-link");
    }
    let (shell, _) = publish_row(
        "publish-nl-link",
        &world,
        "120000",
        &world.newline_oid,
        "cfg/nl-link",
    );
    assert_eq!(shell, 1, "newline target refuses");
    for home in [&world.shell_home, &world.rust_home] {
        assert_stage_bound(home);
        let stages = stage_leftovers(home);
        assert!(!stages[0].join("next").exists(), "no link created");
    }
}

#[test]
fn publish_rejects_empty_link_target() {
    let world = publish_world("publish-empty-link");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "120000", &world.empty_oid, "cfg/empty-link");
    }
    let (shell, _) = publish_row(
        "publish-empty-link",
        &world,
        "120000",
        &world.empty_oid,
        "cfg/empty-link",
    );
    assert_eq!(shell, 1, "empty target refuses");
    for home in [&world.shell_home, &world.rust_home] {
        assert_stage_bound(home);
        let stages = stage_leftovers(home);
        assert!(!stages[0].join("next").exists(), "no link created");
    }
}

#[test]
fn publish_prepared_rerun_refuses() {
    // A second run reads the prepared record, skips both
    // transitions, and fails the final verification on the
    // released stage — on both engines, without touching the
    // published target.
    let world = publish_world("publish-rerun");
    for home in [&world.shell_home, &world.rust_home] {
        seed_pending(home, "100644", &world.app_oid, "cfg/app.conf");
    }
    let (first, _) = publish_row(
        "publish-rerun",
        &world,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    assert_eq!(first, 0, "first publish succeeds");
    let before = tree_dump(&world.shell_home);
    let shell = shell_publish(
        &world.shell_home,
        &world,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    let rust = rust_publish(
        &world.rust_home,
        &world,
        "100644",
        &world.app_oid,
        "cfg/app.conf",
    );
    assert_eq!(shell, 1, "rerun refuses");
    assert_eq!(shell == 0, rust, "rerun verdict disagrees");
    assert_eq!(
        tree_dump(&world.shell_home),
        tree_dump(&world.rust_home),
        "rerun tree disagrees"
    );
    assert_eq!(
        tree_dump(&world.shell_home),
        before,
        "rerun changed nothing"
    );
    for home in [&world.shell_home, &world.rust_home] {
        assert_target_bound(home, "cfg/app.conf");
    }
}
