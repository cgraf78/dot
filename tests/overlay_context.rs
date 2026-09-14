//! Differential parity tests for one-use overlay authorization
//! contexts against `lib/dot/overlay-context.sh`: field, path,
//! record, and matrix validators, ownership-gated file safety,
//! and NUL-framed create/consume round trips — including
//! cross-engine frame compatibility in both directions.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::overlay_context;
use dot::test_support::TempDir;

/// Run one shell snippet with only the context library sourced.
fn shell_run(
    home: &Path,
    argv: &[&std::ffi::OsStr],
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
        .arg(format!(". \"$1/lib/dot/overlay-context.sh\"\n{snippet}"));
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

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Current euid for ownership-gated checks.
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

/// Live `date +%s` instant for the freshness window.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

/// A private context directory: 0700 and owned, like production.
fn context_dir(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("context dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

/// Render a context error exactly like the shell's stderr.
fn render(error: &overlay_context::Error) -> String {
    match error {
        overlay_context::Error::Refused => String::new(),
        overlay_context::Error::Invalid(message) => {
            format!("dot: overlay context: {message}\n")
        }
    }
}

#[test]
fn validators_agree() {
    let dir = TempDir::new("ovctx-valid").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    // Field gate: the shared rule plus the `od` repeat-marker
    // fail-closed quirk.
    let sixteen = "A".repeat(16);
    let thirty_two = "A".repeat(32);
    let values: &[&[u8]] = &[
        b"ok",
        b"",
        b"a|b",
        b"a\tb",
        b"a\nb",
        b"a\x7fb",
        b"\xc3\xa9",
        sixteen.as_bytes(),
        thirty_two.as_bytes(),
    ];
    for value in values {
        let lossy = String::from_utf8_lossy(value);
        let (code, _, _) = shell_run(
            home,
            &[lossy.as_ref().as_ref()],
            &[],
            "_dot_overlay_field_safe \"$2\"",
        );
        assert_eq!(
            overlay_context::field_safe(value),
            code == 0,
            "field for {value:?}"
        );
    }
    // Absolute canonical paths: lexical only, never resolved.
    let paths = [
        "/a/b", "/a/.../b", "/", "", "a/b", "/a/", "/a//b", "/a/./b", "/a/.", "/a/../b", "/a/..",
        "/a|b",
    ];
    for path in paths {
        let (code, _, _) = shell_run(
            home,
            &[path.as_ref()],
            &[],
            "_dot_overlay_context_absolute_canonical \"$2\"",
        );
        assert_eq!(
            overlay_context::absolute_canonical(path.as_bytes()),
            code == 0,
            "canonical path for {path:?}"
        );
    }
    // Mode/set/stage matrix: the exact triple table.
    for (mode, set_kind, stage, valid) in [
        ("pre-sync", "eligible", "prepare", true),
        ("pre-sync", "eligible", "reconcile", true),
        ("merge", "active", "none", true),
        ("deactivate", "retiring", "none", true),
        ("doctor", "active", "none", true),
        ("merge", "eligible", "none", false),
        ("pre-sync", "active", "prepare", false),
        ("merge", "active", "prepare", false),
        ("", "", "", false),
    ] {
        let (code, _, _) = shell_run(
            home,
            &[mode.as_ref(), set_kind.as_ref(), stage.as_ref()],
            &[],
            "_dot_overlay_context_matrix_valid \"$2\" \"$3\" \"$4\"",
        );
        assert_eq!(
            overlay_context::matrix_valid(mode, set_kind, stage),
            code == 0,
            "matrix for {mode}/{set_kind}/{stage}"
        );
        assert_eq!(
            code == 0,
            valid,
            "matrix oracle for {mode}/{set_kind}/{stage}"
        );
    }
    // Record validation across every shape rule.
    let records = [
        "g|/h/.dotfiles-g|https://x/y.git|/d/10-g.conf|false|git",
        "w|/srv/w||/d/10-w.local.conf|false|none",
        "g|/h/.dotfiles-g|https://x/y.git|/d/10-g.conf|false",
        "a|b|c|d|e|f|g",
        "|/h/.dotfiles-|x|/d/10-.conf|false|git",
        "G|/h/.dotfiles-G|x|/d/10-G.conf|false|git",
        "1a|/h/.dotfiles-1a|x|/d/10-1a.conf|false|git",
        "dotfiles|/h/.dotfiles-dotfiles|x|/d/10-dotfiles.conf|false|git",
        "a_b|/h/.dotfiles-a_b|x|/d/10-a_b.conf|false|git",
        "g|rel|x|/d/10-g.conf|false|git",
        "g|/h/.dotfiles-g/|x|/d/10-g.conf|false|git",
        "g|/h/.dotfiles-g|x|/d/10-g.txt|false|git",
        "g|/h/.dotfiles-g|x|/d/10-other.conf|false|git",
        "g|/h/.dotfiles-g|x|/d/10-g.conf|maybe|git",
        "g|/h/.dotfiles-g||/d/10-g.conf|false|git",
        "g|/x|x|/d/10-g.conf|false|git",
        "g|/h/.dotfiles-g|x|/d/10-g.conf|false|hg",
        "w|/srv/w|u|/d/10-w.conf|false|none",
        "w|/srv/w||/d/10-w.conf|true|none",
        "g|/h/.dotfiles-g|x|/d/10-g.conf|false|git|extra",
    ];
    for record in records {
        // The git-home convention anchors on the live HOME both
        // sides: rewrite the placeholder.
        let record = record.replace("/h/", &format!("{home_text}/"));
        let (code, _, _) = shell_run(
            home,
            &[record.as_ref()],
            &[],
            "_dot_overlay_record_validate \"$2\"",
        );
        assert_eq!(
            overlay_context::record_validate(record.as_bytes(), &home_text),
            code == 0,
            "record for {record:?}"
        );
    }
}

#[test]
fn ownership_gates_agree() {
    let dir = TempDir::new("ovctx-gates").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    // Directory gate across modes, links, and kinds.
    let good = context_dir(home, "good");
    std::fs::create_dir(home.join("open")).expect("open dir");
    std::os::unix::fs::symlink("good", home.join("linkdir")).expect("symlink");
    stage(home, "afile", b"x");
    for (label, path) in [
        ("strict", good.clone()),
        ("open", home.join("open")),
        ("link", home.join("linkdir")),
        ("file", home.join("afile")),
        ("missing", home.join("gone")),
    ] {
        let (code, _, _) = shell_run(
            home,
            &[path.as_os_str()],
            &[],
            "_dot_overlay_context_directory_safe \"$2\"",
        );
        assert_eq!(
            overlay_context::directory_safe(&path, uid),
            code == 0,
            "directory gate for {label}"
        );
    }
    // File gate: mode, links, size, and the freshness window.
    let fresh = stage(home, "fresh.ctx", b"payload\n");
    std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let wide = stage(home, "wide.ctx", b"payload\n");
    std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let big = vec![b'x'; (overlay_context::MAX_BYTES + 1) as usize];
    let oversize = stage(home, "big.ctx", &big);
    std::fs::set_permissions(&oversize, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let stale = stage(home, "stale.ctx", b"payload\n");
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let future = stage(home, "future.ctx", b"payload\n");
    std::fs::set_permissions(&future, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    // Portable mtimes via the standard library (`touch -d` is
    // GNU-only): ten minutes stale, one minute in the future.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .expect("open stale")
        .set_modified(past)
        .expect("age stale");
    std::fs::File::options()
        .write(true)
        .open(&future)
        .expect("open future")
        .set_modified(ahead)
        .expect("age future");
    std::fs::hard_link(&fresh, home.join("alias.ctx")).expect("hard link");
    for (label, path) in [
        ("fresh", fresh.clone()),
        ("wide", wide.clone()),
        ("oversize", oversize.clone()),
        ("stale", stale.clone()),
        ("future", future.clone()),
        ("alias", home.join("fresh.ctx")),
        ("dir", good.clone()),
        ("missing", home.join("gone.ctx")),
    ] {
        let (code, _, _) = shell_run(
            home,
            &[path.as_os_str()],
            &[],
            "_dot_overlay_context_file_safe \"$2\"",
        );
        assert_eq!(
            overlay_context::file_safe(&path, uid, now_secs()),
            code == 0,
            "file gate for {label}"
        );
    }
}

#[test]
fn token_format_agrees() {
    let dir = TempDir::new("ovctx-token").expect("fixture dir");
    let home = dir.path();
    let (code, out, _) = shell_run(home, &[], &[], "_dot_overlay_context_token");
    assert_eq!(code, 0, "shell token code");
    let shell = String::from_utf8(out).expect("token text");
    let shell = shell.trim_end_matches(['\r', '\n']);
    assert!(is_token_text(shell), "shell token shape");
    for _ in 0..4 {
        let token = overlay_context::token().expect("rust token");
        assert!(is_token_text(&token), "rust token shape");
    }
}

/// `^[0-9a-f]{64}$` without a regex dependency.
fn is_token_text(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Valid fixture records anchored on `home`.
fn fixture_records(home: &str) -> Vec<Vec<u8>> {
    vec![
        format!(
            "web|{home}/.dotfiles-web|https://example.com/web.git|{home}/conf/10-web.conf|false|git"
        )
        .into_bytes(),
        format!("loc|/srv/loc||{home}/conf/10-loc.local.conf|false|none").into_bytes(),
    ]
}

/// Frame bytes in the on-disk layout, for crafted inputs.
fn frame_bytes(mode: &str, set_kind: &str, stage: &str, token: &str, records: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for field in [
        overlay_context::MAGIC.as_bytes(),
        overlay_context::VERSION.as_bytes(),
        token.as_bytes(),
        mode.as_bytes(),
        set_kind.as_bytes(),
        stage.as_bytes(),
    ] {
        body.extend_from_slice(field);
        body.push(0);
    }
    body.extend_from_slice(records.len().to_string().as_bytes());
    body.push(0);
    for record in records {
        for field in record.split(|byte| *byte == b'|') {
            body.extend_from_slice(field);
            body.push(0);
        }
    }
    body
}

/// Stage a consuming-ready file: correct mode, fresh mtime.
fn stage_context(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let path = stage(dir, name, body);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path
}

#[test]
fn create_consume_roundtrip() {
    let dir = TempDir::new("ovctx-roundtrip").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let ctx = context_dir(home, "ctx");
    let uid = euid();
    let records = fixture_records(&home_text);
    let (path, token) = overlay_context::create(
        &ctx,
        "merge",
        "active",
        "none",
        &records,
        &home_text,
        uid,
        now_secs(),
    )
    .expect("create");
    assert!(is_token_text(&token), "created token shape");
    assert!(
        overlay_context::file_safe(&path, uid, now_secs()),
        "fresh artifact passes the file gate"
    );
    let decoded = overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs())
        .expect("consume");
    assert_eq!(
        decoded.records,
        records
            .iter()
            .map(|record| String::from_utf8_lossy(record).into_owned())
            .collect::<Vec<_>>(),
        "roundtrip records"
    );
    assert_eq!(decoded.set_kind, "active", "roundtrip set");
    assert_eq!(decoded.stage, "none", "roundtrip stage");
    // Single-use: the pathname is gone and replay refuses.
    assert!(!path.exists(), "context unlinked");
    assert!(
        overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs()).is_err(),
        "replay refuses"
    );
    // A well-formed wrong token refuses (and still unlinks first).
    let (path, _token) = overlay_context::create(
        &ctx,
        "merge",
        "active",
        "none",
        &records,
        &home_text,
        uid,
        now_secs(),
    )
    .expect("second create");
    let wrong = "0".repeat(64);
    assert!(
        overlay_context::consume(&path, &wrong, "merge", &home_text, uid, now_secs()).is_err(),
        "wrong token refuses"
    );
    assert!(!path.exists(), "failed consume still unlinks");
    // A wrong expected mode refuses the same way.
    let (path, token) = overlay_context::create(
        &ctx,
        "merge",
        "active",
        "none",
        &records,
        &home_text,
        uid,
        now_secs(),
    )
    .expect("third create");
    assert!(
        overlay_context::consume(&path, &token, "doctor", &home_text, uid, now_secs()).is_err(),
        "wrong mode refuses"
    );
    assert!(!path.exists(), "mode mismatch still unlinks");
    // An empty record set decodes fine: zero records out.
    let (path, token) = overlay_context::create(
        &ctx,
        "merge",
        "active",
        "none",
        &[],
        &home_text,
        uid,
        now_secs(),
    )
    .expect("empty create");
    let decoded = overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs())
        .expect("empty consume");
    assert!(decoded.records.is_empty(), "empty roundtrip");
}

#[test]
fn consume_tamper_refuses() {
    let dir = TempDir::new("ovctx-tamper").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let ctx = context_dir(home, "ctx");
    let uid = euid();
    let good_record = format!(
        "web|{home_text}/.dotfiles-web|https://example.com/web.git|{home_text}/conf/10-web.conf|false|git"
    );
    let good = good_record.as_bytes();
    // Well-formed token generator for crafted frames.
    let token = overlay_context::token().expect("token");
    let craft = |dir: &Path, label: &str, body: &[u8]| stage_context(dir, label, body);
    // Count byte offset: magic\0 1\0 token\0 merge\0 active\0
    // none\0 then the single count byte.
    let prefix: usize = "DOT_OVERLAY_CONTEXT".len()
        + 1
        + 1
        + 1
        + token.len()
        + 1
        + "merge".len()
        + 1
        + "active".len()
        + 1
        + "none".len()
        + 1;
    let bad_token = "z".repeat(64);
    let mut bad_magic = frame_bytes("merge", "active", "none", &token, &[good]);
    bad_magic[0] = b'X';
    let mut truncated = frame_bytes("merge", "active", "none", &token, &[good]);
    truncated.pop();
    assert_eq!(truncated.last(), Some(&b't'), "truncate cuts the frame");
    // Count 3 with two records, and count 2 with one record.
    let mut count_hi = frame_bytes("merge", "active", "none", &token, &[good, good]);
    count_hi[prefix] = b'3';
    let mut count_lo = frame_bytes("merge", "active", "none", &token, &[good]);
    assert_eq!(count_lo[prefix], b'1', "count offset");
    count_lo[prefix] = b'2';
    let mut leading_zero = frame_bytes("merge", "active", "none", &token, &[good]);
    leading_zero.insert(prefix, b'0');
    // (label, frame, presented token, presented mode). A zero
    // count decodes fine on both engines, so it is pinned in the
    // roundtrip test instead of here.
    let cases: Vec<(&str, Vec<u8>, String, &str)> = vec![
        ("bad-magic", bad_magic, token.clone(), "merge"),
        ("truncated", truncated, token.clone(), "merge"),
        ("count-hi", count_hi, token.clone(), "merge"),
        ("count-lo", count_lo, token.clone(), "merge"),
        (
            "bad-record",
            frame_bytes(
                "merge",
                "active",
                "none",
                &token,
                &[b"G|/x|u|/d/10-g.conf|false|git"],
            ),
            token.clone(),
            "merge",
        ),
        (
            "dup-names",
            frame_bytes("merge", "active", "none", &token, &[good, good]),
            token.clone(),
            "merge",
        ),
        (
            "bad-mode-frame",
            frame_bytes("merge", "eligible", "none", &token, &[good]),
            token.clone(),
            "merge",
        ),
        ("empty", Vec::new(), token.clone(), "merge"),
        ("no-nul", b"merge".to_vec(), token.clone(), "merge"),
        (
            "bad-token-shape",
            frame_bytes("merge", "active", "none", &bad_token, &[good]),
            bad_token.clone(),
            "merge",
        ),
        ("leading-zero-count", leading_zero, token.clone(), "merge"),
    ];
    for (label, body, presented, mode) in &cases {
        let path = craft(&ctx, &format!("{label}.ctx"), body);
        let result = overlay_context::consume(&path, presented, mode, &home_text, uid, now_secs());
        assert!(result.is_err(), "{label} refuses");
    }
    // Environmental refusals: hard links, open modes, stale
    // files, unsafe parents, and missing or relative paths.
    let path = craft(
        &ctx,
        "linked.ctx",
        &frame_bytes("merge", "active", "none", &token, &[good]),
    );
    std::fs::hard_link(&path, ctx.join("linked-alias.ctx")).expect("hard link");
    assert!(
        overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs()).is_err(),
        "linked refuses"
    );
    let path = craft(
        &ctx,
        "wide.ctx",
        &frame_bytes("merge", "active", "none", &token, &[good]),
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    assert!(
        overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs()).is_err(),
        "open mode refuses"
    );
    let path = craft(
        &ctx,
        "stale.ctx",
        &frame_bytes("merge", "active", "none", &token, &[good]),
    );
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open stale")
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600))
        .expect("age stale");
    assert!(
        overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs()).is_err(),
        "stale refuses"
    );
    assert!(
        overlay_context::consume(
            &ctx.join("gone.ctx"),
            &token,
            "merge",
            &home_text,
            uid,
            now_secs()
        )
        .is_err(),
        "missing refuses"
    );
    assert!(
        overlay_context::consume(
            Path::new("relative.ctx"),
            &token,
            "merge",
            &home_text,
            uid,
            now_secs()
        )
        .is_err(),
        "relative refuses"
    );
    let open = context_dir(home, "open");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let exposed = frame_bytes("merge", "active", "none", &token, &[good]);
    let exposed_path = stage(&open, "exposed.ctx", &exposed);
    std::fs::set_permissions(&exposed_path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert!(
        overlay_context::consume(&exposed_path, &token, "merge", &home_text, uid, now_secs())
            .is_err(),
        "unsafe parent refuses"
    );
}

#[test]
fn create_errors_agree() {
    let dir = TempDir::new("ovctx-create-err").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let ctx = context_dir(home, "ctx");
    let uid = euid();
    let good = format!(
        "web|{home_text}/.dotfiles-web|https://example.com/web.git|{home_text}/conf/10-web.conf|false|git"
    );
    let bad_record = "G|/x|u|/d/10-g.conf|false|git".to_string();
    // (label, directory, mode, set, stage, records): every row
    // fails on both sides with the same message.
    let dup = vec![good.clone(), good.clone()];
    let many: Vec<String> = (0..257)
        .map(|index| {
            format!(
                "o{index:03}|{home_text}/.dotfiles-o{index:03}|https://example.com/o.git|{home_text}/conf/10-o{index:03}.conf|false|git"
            )
        })
        .collect();
    let rel = PathBuf::from("relative");
    type ErrorCase = (
        &'static str,
        PathBuf,
        &'static str,
        &'static str,
        &'static str,
        Vec<String>,
    );
    let cases: Vec<ErrorCase> = vec![
        (
            "relative-dir",
            rel.clone(),
            "merge",
            "active",
            "none",
            vec![good.clone()],
        ),
        (
            "bad-matrix",
            ctx.clone(),
            "merge",
            "eligible",
            "none",
            vec![good.clone()],
        ),
        (
            "bad-record",
            ctx.clone(),
            "merge",
            "active",
            "none",
            vec![bad_record.clone()],
        ),
        ("dup-records", ctx.clone(), "merge", "active", "none", dup),
        ("too-many", ctx.clone(), "merge", "active", "none", many),
    ];
    for (label, directory, mode, set_kind, stage, records) in cases {
        let mut argv: Vec<&std::ffi::OsStr> = vec![
            directory.as_os_str(),
            mode.as_ref(),
            set_kind.as_ref(),
            stage.as_ref(),
        ];
        for record in &records {
            argv.push(record.as_ref());
        }
        // The harness shell always exits 0 (the dump `printf`
        // runs last), so the helper status comes from the
        // printed `rc=` line, not the process code.
        let (_, sout, serr) = shell_run(
            home,
            &argv,
            &[],
            "_dot_overlay_context_create \"$2\" \"$3\" \"$4\" \"$5\" \"${@:6}\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
        );
        let record_bytes: Vec<Vec<u8>> = records
            .iter()
            .map(|record| record.as_bytes().to_vec())
            .collect();
        let rust = overlay_context::create(
            &directory,
            mode,
            set_kind,
            stage,
            &record_bytes,
            &home_text,
            uid,
            now_secs(),
        );
        assert_eq!(
            String::from_utf8(sout).expect("create dump"),
            "rc=1\n",
            "{label} shell code"
        );
        let error = rust.expect_err(&format!("{label} rust fails"));
        assert_eq!(error.code(), 1, "{label} rust code");
        assert_eq!(
            String::from_utf8(serr).expect("create stderr"),
            render(&error),
            "{label} stderr"
        );
    }
    // The 256-record boundary itself succeeds.
    let boundary: Vec<String> = (0..256)
        .map(|index| {
            format!(
                "o{index:03}|{home_text}/.dotfiles-o{index:03}|https://example.com/o.git|{home_text}/conf/10-o{index:03}.conf|false|git"
            )
        })
        .collect();
    let record_bytes: Vec<Vec<u8>> = boundary
        .iter()
        .map(|record| record.as_bytes().to_vec())
        .collect();
    let (path, token) = overlay_context::create(
        &ctx,
        "merge",
        "active",
        "none",
        &record_bytes,
        &home_text,
        uid,
        now_secs(),
    )
    .expect("256 records create");
    let decoded = overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs())
        .expect("256 records consume");
    assert_eq!(decoded.records.len(), 256, "boundary count");
}

/// Frame compatibility across engines: a shell-written context
/// decodes under Rust and vice versa, with identical records.
#[test]
fn cross_engine_frames_agree() {
    let dir = TempDir::new("ovctx-cross").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let ctx = context_dir(home, "ctx");
    let uid = euid();
    let records = fixture_records(&home_text);
    let strings: Vec<String> = records
        .iter()
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect();
    // Shell create, Rust consume.
    let mut argv: Vec<&std::ffi::OsStr> = vec![
        ctx.as_os_str(),
        "merge".as_ref(),
        "active".as_ref(),
        "none".as_ref(),
    ];
    for record in &strings {
        argv.push(record.as_ref());
    }
    let (scode, sout, serr) = shell_run(
        home,
        &argv,
        &[],
        "_dot_overlay_context_create \"$2\" \"$3\" \"$4\" \"$5\" \"${@:6}\"; rc=$?; printf 'rc=%s\\npath=%s\\ntoken=%s\\n' \"$rc\" \"$REPLY_PATH\" \"$REPLY_TOKEN\"",
    );
    assert_eq!(scode, 0, "shell create code");
    assert_eq!(serr, b"", "shell create stderr");
    let shell = String::from_utf8(sout).expect("create dump");
    let mut lines = shell.lines();
    assert_eq!(lines.next(), Some("rc=0"), "shell create rc");
    let path = PathBuf::from(lines.next().unwrap_or_default().trim_start_matches("path="));
    let token = lines
        .next()
        .unwrap_or_default()
        .trim_start_matches("token=")
        .to_string();
    assert!(is_token_text(&token), "shell token shape");
    let decoded = overlay_context::consume(&path, &token, "merge", &home_text, uid, now_secs())
        .expect("rust consumes shell frame");
    assert_eq!(decoded.records, strings, "shell frame records");
    assert_eq!(decoded.set_kind, "active", "shell frame set");
    assert_eq!(decoded.stage, "none", "shell frame stage");
    // Rust create, shell consume.
    let record_bytes: Vec<Vec<u8>> = strings
        .iter()
        .map(|record| record.as_bytes().to_vec())
        .collect();
    let (path, token) = overlay_context::create(
        &ctx,
        "pre-sync",
        "eligible",
        "prepare",
        &record_bytes,
        &home_text,
        uid,
        now_secs(),
    )
    .expect("rust create");
    let (scode, sout, serr) = shell_run(
        home,
        &[path.as_os_str(), token.as_ref(), "pre-sync".as_ref()],
        &[],
        "_dot_overlay_context_consume \"$2\" \"$3\" \"$4\"; rc=$?; printf 'rc=%s\\n' \"$rc\"; for e in ${OVERLAYS[@]+\"${OVERLAYS[@]}\"}; do printf 'R|%s\\n' \"$e\"; done; printf 'kind=%s\\nstage=%s\\n' \"$REPLY_SET_KIND\" \"$REPLY_STAGE\"",
    );
    assert_eq!(scode, 0, "shell consume code");
    assert_eq!(serr, b"", "shell consume stderr");
    let shell = String::from_utf8(sout).expect("consume dump");
    let mut expected = vec!["rc=0".to_string()];
    for record in &strings {
        expected.push(format!("R|{record}"));
    }
    expected.push("kind=eligible".to_string());
    expected.push("stage=prepare".to_string());
    assert_eq!(
        shell.lines().collect::<Vec<_>>(),
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "shell consumes rust frame"
    );
    assert!(!path.exists(), "shell consume unlinks");
}

/// Crafted frames refuse identically: staged bytes feed the shell
/// consumer while the sibling cases pin the Rust side above.
#[test]
fn crafted_frames_refuse_both() {
    let dir = TempDir::new("ovctx-crafted").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let uid = euid();
    let good_record = format!(
        "web|{home_text}/.dotfiles-web|https://example.com/web.git|{home_text}/conf/10-web.conf|false|git"
    );
    let good = good_record.as_bytes();
    let token = overlay_context::token().expect("token");
    let bad_token = "z".repeat(64);
    // (label, frame, presented token): every row refuses with no
    // output on either side.
    let cases: Vec<(&str, Vec<u8>, String)> = vec![
        (
            "bad-magic",
            {
                let mut body = frame_bytes("merge", "active", "none", &token, &[good]);
                body[0] = b'X';
                body
            },
            token.clone(),
        ),
        (
            "count-hi",
            {
                let mut body = frame_bytes("merge", "active", "none", &token, &[good]);
                // One record, count rewritten to 3.
                let prefix: usize = "DOT_OVERLAY_CONTEXT".len()
                    + 1
                    + 1
                    + 1
                    + token.len()
                    + 1
                    + "merge".len()
                    + 1
                    + "active".len()
                    + 1
                    + "none".len()
                    + 1;
                body[prefix] = b'3';
                body
            },
            token.clone(),
        ),
        (
            "dup-names",
            frame_bytes("merge", "active", "none", &token, &[good, good]),
            token.clone(),
        ),
        (
            "bad-token-shape",
            frame_bytes("merge", "active", "none", &bad_token, &[good]),
            bad_token.clone(),
        ),
    ];
    for (label, body, presented) in &cases {
        let ctx = context_dir(home, label);
        let path = stage_context(&ctx, &format!("{label}.ctx"), body);
        // Process code is always 0 (the dump `printf` runs
        // last); refusal is the printed `rc=1` line.
        let (_, sout, serr) = shell_run(
            home,
            &[path.as_os_str(), presented.as_ref(), "merge".as_ref()],
            &[],
            "_dot_overlay_context_consume \"$2\" \"$3\" \"$4\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
        );
        assert_eq!(
            String::from_utf8(sout).expect("dump"),
            "rc=1\n",
            "{label} shell refuses"
        );
        assert_eq!(serr, b"", "{label} shell stderr");
        assert!(
            overlay_context::consume(&path, presented, "merge", &home_text, uid, now_secs())
                .is_err(),
            "{label} rust refuses"
        );
    }
}

/// Differential parity for `_dot_overlay_context_stat` against the
/// live shell oracle: GNU `stat -c` else BSD `stat -f`, with the
/// `REPLY_*` identity tuple gated on owner and octal-only mode.
/// Portable: fixtures use only the standard library plus the shell
/// oracle itself — never a bare GNU `stat -c` spelling.
#[test]
fn stat_agrees() {
    let dir = TempDir::new("ovctx-stat").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    let file = stage(home, "a.txt", b"hello\n");
    let subdir = home.join("sub");
    std::fs::create_dir_all(&subdir).expect("subdir");
    std::os::unix::fs::symlink("a.txt", home.join("link.txt")).expect("symlink");
    std::os::unix::fs::symlink("gone-target", home.join("dangling.txt")).expect("dangling");
    let missing = home.join("missing.txt");
    let cases: Vec<(&str, PathBuf)> = vec![
        ("file", file.clone()),
        ("dir", subdir.clone()),
        ("symlink", home.join("link.txt")),
        ("dangling", home.join("dangling.txt")),
        ("missing", missing.clone()),
    ];
    for (label, path) in &cases {
        let (_, sout, serr) = shell_run(
            home,
            &[path.as_os_str()],
            &[],
            "_dot_overlay_context_stat \"$2\"; rc=$?; printf 'rc=%s\\nuid=%s\\nmode=%s\\nlinks=%s\\ndev=%s\\nino=%s\\n' \"$rc\" \"$REPLY_UID\" \"$REPLY_MODE\" \"$REPLY_LINKS\" \"$REPLY_DEV\" \"$REPLY_INO\"",
        );
        assert_eq!(serr, b"", "{label} shell stderr");
        let shell = String::from_utf8(sout).expect("stat dump");
        let mut lines = shell.lines();
        let rc: i32 = lines
            .next()
            .unwrap_or("")
            .trim_start_matches("rc=")
            .parse()
            .expect("rc");
        let get = |lines: &mut std::str::Lines<'_>, key: &str| {
            lines
                .next()
                .unwrap_or("")
                .trim_start_matches(&format!("{key}="))
                .to_string()
        };
        let shell_uid = get(&mut lines, "uid");
        let shell_mode = get(&mut lines, "mode");
        let shell_links = get(&mut lines, "links");
        let shell_dev = get(&mut lines, "dev");
        let shell_ino = get(&mut lines, "ino");
        let rust = overlay_context::stat(path, uid);
        assert_eq!(rust.is_some(), rc == 0, "stat code for {label}");
        if rc == 0 {
            let (uid, mode, links, dev, ino) = rust.expect("stat tuple");
            assert_eq!(uid.to_string(), shell_uid, "stat uid for {label}");
            assert_eq!(
                u32::from_str_radix(&shell_mode, 8).expect("octal mode"),
                mode,
                "stat mode for {label}"
            );
            assert_eq!(links.to_string(), shell_links, "stat links for {label}");
            assert_eq!(dev.to_string(), shell_dev, "stat dev for {label}");
            assert_eq!(ino.to_string(), shell_ino, "stat ino for {label}");
        }
    }
    // An empty path fails the `stat` probe on both sides.
    let (_, sout, _) = shell_run(
        home,
        &[],
        &[],
        "_dot_overlay_context_stat; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    assert_eq!(
        String::from_utf8(sout).expect("empty dump"),
        "rc=1\n",
        "empty shell refuses"
    );
    assert!(
        overlay_context::stat(Path::new(""), uid).is_none(),
        "empty rust refuses"
    );
    // A foreign owner refuses without parsing: Rust-only, since the
    // shell reads the live `$EUID` and no second uid exists here.
    assert!(
        overlay_context::stat(&file, uid.wrapping_add(1)).is_none(),
        "foreign owner refuses"
    );
}
