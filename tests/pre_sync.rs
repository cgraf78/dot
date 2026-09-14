//! Differential parity tests for client pre-sync extensions
//! (`lib/dot/pre-sync.sh`) against the live shell: spec
//! enumeration (empty roots, glob order, key derivation, and
//! identity errors) and the extension run loop (stage gate,
//! per-extension scratch plus one-use context, worker-failure
//! warnings with break, and context-creation refusal).
//!
//! The worker spawn (`_dot_extension_worker_run`) belongs to a
//! later slice, so both engines run a recording stub: the shell
//! side redefines the function after sourcing the launch
//! library, while the Rust side injects the
//! [`dot::pre_sync::Runner`] closure. Each stub consumes the
//! offered context and dumps the decoded frame, so the
//! comparison covers context creation as well as orchestration.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::extension_trust::Inputs;
use dot::pre_sync;
use dot::test_support::TempDir;

/// Shell sources for the pre-sync runtime, in dependency order.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/platform.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/overlay-context.sh\"\n",
    ". \"$1/lib/dot/overlays.sh\"\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/extension-trust.sh\"\n",
    ". \"$1/lib/dot/extension-worker-launch.sh\"\n",
    ". \"$1/lib/dot/pre-sync.sh\"\n",
);

/// Recording stub for `_dot_extension_worker_run`, defined after
/// the launch library so it wins. Logs one line per call to
/// `$WORKER_LOG`, records every scratch directory in `$TMP_LOG`,
/// fails the `$WORKER_FAIL_AT`-th call, and otherwise consumes
/// the offered context and dumps the decoded frame.
const STUB: &str = r#"_dot_extension_worker_run() {
  local _mode=$1 _script=$2 _temporary=$3 _result=$4 _context=$5 _token=$6
  local _n _result_ok=0 _temporary_ok=0
  _n=$(($(wc -l <"$WORKER_LOG" 2>/dev/null || echo 0) + 1))
  printf '%s\n' "$_temporary" >>"$TMP_LOG"
  [[ -f $_result && ! -L $_result ]] && _result_ok=1
  [[ -d $_temporary && ! -L $_temporary ]] && _temporary_ok=1
  if [[ ${WORKER_FAIL_AT:-} == "$_n" ]]; then return 1; fi
  if _dot_overlay_context_consume "$_context" "$_token" pre-sync; then
    printf 'call=%s script=%s result_ok=%s temporary_ok=%s records=%s set=%s stage=%s\n' "$_n" "${_script##*/}" "$_result_ok" "$_temporary_ok" "${OVERLAYS[*]:-}" "$REPLY_SET_KIND" "$REPLY_STAGE" >>"$WORKER_LOG"
  else
    printf 'call=%s script=%s consume-failed\n' "$_n" "${_script##*/}" >>"$WORKER_LOG"
    return 1
  fi
}
"#;

/// Run one shell snippet with the pre-sync runtime sourced.
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

/// Current euid for ownership-gated checks.
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

/// Live `date +%s` instant for the context freshness window.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

/// Base [`Inputs`] with the extensions root at `extensions_dir`.
fn inputs(home: &Path, extensions_dir: &str) -> Inputs {
    Inputs {
        euid: euid(),
        home: home.to_string_lossy().into_owned(),
        extensions_dir: extensions_dir.to_string(),
        manifest: String::new(),
        retiring_root: String::new(),
    }
}

/// Render a pre-sync error exactly like the shell's stderr: silent
/// failures print nothing, announced ones print their line.
fn render(error: &pre_sync::Error) -> Vec<u8> {
    match error {
        pre_sync::Error::Usage | pre_sync::Error::Refused => Vec::new(),
        pre_sync::Error::Invalid(message) => format!("{message}\n").into_bytes(),
    }
}

/// Render specs exactly like the shell's `key<TAB>script` listing.
fn specs_bytes(found: &[pre_sync::Spec]) -> Vec<u8> {
    let mut out = Vec::new();
    for spec in found {
        out.extend_from_slice(spec.key.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(spec.script.as_os_str().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Create `{home}/ext/pre-sync.d` and return the extensions root.
fn ext_root(home: &Path) -> String {
    let root = home.join("ext").join("pre-sync.d");
    std::fs::create_dir_all(&root).expect("ext root");
    home.join("ext").to_string_lossy().into_owned()
}

/// Write `name` under `dir` with `mode`.
fn stage_script(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Read a log file as lines without trailing newlines.
fn read_lines(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("read log");
    text.lines().map(str::to_string).collect()
}

/// Count entries directly under `dir`.
fn scratch_entries(dir: &Path) -> usize {
    std::fs::read_dir(dir).expect("read scratch").count()
}

#[test]
fn specs_agree() {
    let dir = TempDir::new("presync-specs").expect("fixture dir");
    let home = dir.path();
    // An unset extensions directory lists nothing.
    let (code, out, err) = shell_run(
        home,
        &[],
        &[("DOT_EXTENSIONS_DIR", None)],
        "_dot_pre_sync_specs",
    );
    assert_eq!(code, 0, "unset root code");
    assert!(out.is_empty(), "unset root stdout");
    assert!(err.is_empty(), "unset root stderr");
    assert_eq!(pre_sync::specs(&inputs(home, ""), &[]), Ok(Vec::new()));
    // A missing root lists nothing either.
    let missing = home.join("ext").to_string_lossy().into_owned();
    let (code, out, err) = shell_run(
        home,
        &[],
        &[("DOT_EXTENSIONS_DIR", Some(missing.as_str()))],
        "_dot_pre_sync_specs",
    );
    assert_eq!(code, 0, "missing root code");
    assert!(out.is_empty(), "missing root stdout");
    assert!(err.is_empty(), "missing root stderr");
    assert_eq!(
        pre_sync::specs(&inputs(home, &missing), &[]),
        Ok(Vec::new())
    );
    // A `pre-sync.d` that is a plain file refuses silently.
    let ext_only = home.join("ext");
    std::fs::create_dir_all(&ext_only).expect("ext dir");
    std::fs::write(ext_only.join("pre-sync.d"), b"x").expect("file root");
    let file_text = ext_only.to_string_lossy().into_owned();
    let (code, out, err) = shell_run(
        home,
        &[],
        &[("DOT_EXTENSIONS_DIR", Some(file_text.as_str()))],
        "_dot_pre_sync_specs",
    );
    assert_eq!(code, 1, "file root code");
    assert!(out.is_empty(), "file root stdout");
    assert!(err.is_empty(), "file root stderr");
    assert_eq!(
        pre_sync::specs(&inputs(home, &file_text), &[]),
        Err(pre_sync::SpecsError {
            emitted: Vec::new(),
            error: pre_sync::Error::Refused,
        })
    );
    // A populated root lists entry points in glob order with
    // derived keys; anything else is ignored.
    let _ = std::fs::remove_file(home.join("ext").join("pre-sync.d"));
    let root_text = ext_root(home);
    let root = Path::new(&root_text).join("pre-sync.d");
    for name in [
        "10-a.sh",
        "20-b.sh",
        "30_c.sh",
        "plain.sh",
        "x.serial.sh",
        "README",
        "notes.txt",
        ".hidden.sh",
    ] {
        stage_script(&root, name, 0o644);
    }
    std::fs::create_dir_all(root.join("sub")).expect("subdir");
    stage_script(&root.join("sub"), "inner.sh", 0o644);
    let (code, out, err) = shell_run(
        home,
        &[],
        &[("DOT_EXTENSIONS_DIR", Some(root_text.as_str()))],
        "_dot_pre_sync_specs",
    );
    let found = pre_sync::specs(&inputs(home, &root_text), &[]).expect("specs");
    assert_eq!(code, 0, "populated code");
    assert!(err.is_empty(), "populated stderr");
    assert_eq!(out, specs_bytes(&found), "populated listing agrees");
    // Pin the exact listing, not just agreement.
    let mut expected = Vec::new();
    for (key, file) in [
        ("10-a", "10-a.sh"),
        ("20-b", "20-b.sh"),
        ("30_c", "30_c.sh"),
        ("plain", "plain.sh"),
        ("x", "x.serial.sh"),
    ] {
        expected.extend_from_slice(format!("{key}\t{root_text}/pre-sync.d/{file}\n").as_bytes());
    }
    assert_eq!(out, expected, "pinned listing");
}

#[test]
fn specs_identity_failures_agree() {
    // `emitted` pins the rows the shell streams before the
    // failure: later errors keep earlier rows on stdout.
    for (label, files, emitted, message) in [
        (
            "uppercase",
            vec!["Bogus.sh"],
            vec![],
            "dot: invalid pre-sync extension identity: Bogus.sh\n",
        ),
        (
            "bare-number",
            vec!["10.sh"],
            vec![],
            "dot: invalid pre-sync extension identity: 10.sh\n",
        ),
        (
            "dotted",
            vec!["foo.bar.sh"],
            vec![],
            "dot: invalid pre-sync extension identity: foo.bar.sh\n",
        ),
        (
            "duplicate",
            vec!["10-foo.sh", "10_foo.sh"],
            vec![("10-foo", "10-foo.sh")],
            "dot: duplicate pre-sync extension identity: foo\n",
        ),
        (
            "late-invalid",
            vec!["10-a.sh", "Bogus.sh"],
            vec![("10-a", "10-a.sh")],
            "dot: invalid pre-sync extension identity: Bogus.sh\n",
        ),
    ] {
        let dir = TempDir::new("presync-identity").expect("fixture dir");
        let home = dir.path();
        let root_text = ext_root(home);
        let root = Path::new(&root_text).join("pre-sync.d");
        for file in &files {
            stage_script(&root, file, 0o644);
        }
        let (code, out, err) = shell_run(
            home,
            &[],
            &[("DOT_EXTENSIONS_DIR", Some(root_text.as_str()))],
            "_dot_pre_sync_specs",
        );
        let failed = pre_sync::specs(&inputs(home, &root_text), &[]).unwrap_err();
        let mut expected_out = Vec::new();
        for (key, file) in &emitted {
            expected_out
                .extend_from_slice(format!("{key}\t{root_text}/pre-sync.d/{file}\n").as_bytes());
        }
        assert_eq!(code, 1, "{label} code");
        assert_eq!(out, expected_out, "{label} partial stdout");
        assert_eq!(err, message.as_bytes(), "{label} stderr");
        assert_eq!(
            specs_bytes(&failed.emitted),
            expected_out,
            "{label} emitted"
        );
        assert_eq!(err, render(&failed.error), "{label} error agrees");
    }
}

#[test]
fn specs_unreadable_extension_agrees() {
    // A group-writable entry fails the extension stat gate on
    // both engines (readable or not, the mode refuses).
    let dir = TempDir::new("presync-unreadable").expect("fixture dir");
    let home = dir.path();
    let root_text = ext_root(home);
    let root = Path::new(&root_text).join("pre-sync.d");
    stage_script(&root, "10-a.sh", 0o664);
    let (code, out, err) = shell_run(
        home,
        &[],
        &[("DOT_EXTENSIONS_DIR", Some(root_text.as_str()))],
        "_dot_pre_sync_specs",
    );
    assert_eq!(code, 1, "bad mode code");
    assert!(out.is_empty(), "bad mode stdout");
    assert!(err.is_empty(), "bad mode stderr");
    assert_eq!(
        pre_sync::specs(&inputs(home, &root_text), &[]),
        Err(pre_sync::SpecsError {
            emitted: Vec::new(),
            error: pre_sync::Error::Refused,
        })
    );
    // A dangling symlink is unreadable before any manifest
    // authorization runs, so both engines refuse silently.
    let dir = TempDir::new("presync-dangling").expect("fixture dir");
    let home = dir.path();
    let root_text = ext_root(home);
    let root = Path::new(&root_text).join("pre-sync.d");
    std::os::unix::fs::symlink("nowhere", root.join("dead.sh")).expect("dangling link");
    let (code, out, err) = shell_run(
        home,
        &[],
        &[("DOT_EXTENSIONS_DIR", Some(root_text.as_str()))],
        "_dot_pre_sync_specs",
    );
    assert_eq!(code, 1, "dangling code");
    assert!(out.is_empty(), "dangling stdout");
    assert!(err.is_empty(), "dangling stderr");
    assert_eq!(
        pre_sync::specs(&inputs(home, &root_text), &[]),
        Err(pre_sync::SpecsError {
            emitted: Vec::new(),
            error: pre_sync::Error::Refused,
        })
    );
}

/// Run `stage` with `records` on both engines against one
/// fixture (the caller stages `pre-sync.d` first), failing the
/// `fail_at`-th worker call when set. Compares status, streams,
/// stub logs, and scratch cleanup, then returns the Rust outcome
/// plus its stub log for case-specific pins.
fn drive_run(
    home: &Path,
    stage: &str,
    records: &[Vec<u8>],
    fail_at: Option<usize>,
    expect_shell_scratch: usize,
    expect_rust_scratch: usize,
) -> (Result<pre_sync::Outcome, pre_sync::Error>, Vec<String>) {
    let root_text = ext_root(home);
    let worker_log = home.join("worker.log");
    let tmp_log = home.join("tmp.log");
    std::fs::write(&worker_log, b"").expect("worker log");
    std::fs::write(&tmp_log, b"").expect("tmp log");
    let scratch_sh = home.join("scratch-sh");
    let scratch_rs = home.join("scratch-rs");
    std::fs::create_dir_all(&scratch_sh).expect("shell scratch");
    std::fs::create_dir_all(&scratch_rs).expect("rust scratch");
    let worker_text = worker_log.to_string_lossy().into_owned();
    let tmp_text = tmp_log.to_string_lossy().into_owned();
    let scratch_text = scratch_sh.to_string_lossy().into_owned();
    let fail_text = fail_at.map(|at| at.to_string());
    let env: Vec<(&str, Option<&str>)> = vec![
        ("DOT_EXTENSIONS_DIR", Some(root_text.as_str())),
        ("WORKER_LOG", Some(worker_text.as_str())),
        ("TMP_LOG", Some(tmp_text.as_str())),
        ("TMPDIR", Some(scratch_text.as_str())),
        ("WORKER_FAIL_AT", fail_text.as_deref()),
    ];
    let mut record_args: Vec<&std::ffi::OsStr> = vec![stage.as_ref()];
    for record in records {
        record_args.push(std::ffi::OsStr::from_bytes(record));
    }
    let snippet = format!("{STUB}_run_pre_sync_extensions \"$2\" \"${{@:3}}\"\n");
    let (code, out, err) = shell_run(home, &record_args, &env, &snippet);
    let home_text = home.to_string_lossy().into_owned();
    let uid = euid();
    let now = now_secs();
    let mut calls: Vec<String> = Vec::new();
    let mut temporaries: Vec<PathBuf> = Vec::new();
    let mut seen = 0usize;
    let mut runner = |call: &pre_sync::Call| -> bool {
        seen += 1;
        temporaries.push(call.temporary.clone());
        if Some(seen) == fail_at {
            return false;
        }
        let result_ok = std::fs::symlink_metadata(&call.result)
            .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
            as u8;
        let temporary_ok = std::fs::symlink_metadata(&call.temporary)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
            as u8;
        let script = call
            .script
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match dot::overlay_context::consume(
            &call.context,
            &call.token,
            "pre-sync",
            &home_text,
            uid,
            now,
        ) {
            Ok(decoded) => {
                calls.push(format!(
                    "call={seen} script={script} result_ok={result_ok} \
                     temporary_ok={temporary_ok} records={} set={} stage={}",
                    decoded.records.join(" "),
                    decoded.set_kind,
                    decoded.stage
                ));
                true
            }
            Err(_) => {
                calls.push(format!("call={seen} script={script} consume-failed"));
                false
            }
        }
    };
    let outcome = pre_sync::run(
        stage,
        records,
        &inputs(home, &root_text),
        &[],
        now,
        &scratch_rs,
        &mut runner,
    );
    let status = match &outcome {
        Ok(done) => done.status,
        Err(error) => error.code(),
    };
    assert_eq!(code, status, "status for stage {stage}");
    assert!(out.is_empty(), "shell stdout for stage {stage}");
    let mut expected_err = Vec::new();
    if let Ok(done) = &outcome {
        for warning in &done.warnings {
            expected_err.extend_from_slice(format!("{warning}\n").as_bytes());
        }
    } else if let Err(error) = &outcome {
        expected_err = render(error);
    }
    assert_eq!(err, expected_err, "stderr for stage {stage}");
    assert_eq!(read_lines(&worker_log), calls, "stub log for stage {stage}");
    for tmp in read_lines(&tmp_log) {
        assert!(!Path::new(&tmp).exists(), "shell scratch removed: {tmp}");
    }
    for tmp in &temporaries {
        assert!(!tmp.exists(), "rust scratch removed: {}", tmp.display());
    }
    assert_eq!(
        scratch_entries(&scratch_sh),
        expect_shell_scratch,
        "shell leftovers"
    );
    assert_eq!(
        scratch_entries(&scratch_rs),
        expect_rust_scratch,
        "rust leftovers"
    );
    (outcome, calls)
}

#[test]
fn run_stage_and_empty_agree() {
    let dir = TempDir::new("presync-stage").expect("fixture dir");
    let home = dir.path();
    // Unknown stages refuse with exit 2 before touching specs,
    // so the runner must never run.
    for stage in ["", "bogus"] {
        let (code, out, err) = shell_run(
            home,
            &[stage.as_ref()],
            &[("DOT_EXTENSIONS_DIR", None)],
            "_run_pre_sync_extensions \"$2\"\n",
        );
        assert_eq!(code, 2, "stage {stage:?} code");
        assert!(out.is_empty(), "stage {stage:?} stdout");
        assert!(err.is_empty(), "stage {stage:?} stderr");
        let scratch = home.join(if stage.is_empty() {
            "scratch-empty"
        } else {
            "scratch"
        });
        std::fs::create_dir_all(&scratch).expect("scratch");
        let mut runner = |_: &pre_sync::Call| -> bool {
            panic!("no extension runs for stage {stage:?}");
        };
        let result = pre_sync::run(
            stage,
            &[],
            &inputs(home, ""),
            &[],
            now_secs(),
            &scratch,
            &mut runner,
        );
        assert_eq!(result, Err(pre_sync::Error::Usage));
    }
    // Both stages succeed quietly with no extensions.
    for stage in ["prepare", "reconcile"] {
        let dir = TempDir::new("presync-run-empty").expect("fixture dir");
        let home = dir.path();
        ext_root(home);
        let (outcome, calls) = drive_run(home, stage, &[], None, 0, 0);
        assert_eq!(
            outcome,
            Ok(pre_sync::Outcome {
                status: 0,
                warnings: Vec::new(),
            }),
            "empty {stage}"
        );
        assert!(calls.is_empty(), "empty {stage} calls");
    }
}

/// Stage the two-extension fixture `drive_run` consumes.
fn stage_pair(home: &Path) {
    let root_text = ext_root(home);
    let root = Path::new(&root_text).join("pre-sync.d");
    stage_script(&root, "10-a.sh", 0o644);
    stage_script(&root, "20-b.sh", 0o644);
}

#[test]
fn run_orchestration_agrees() {
    // Every worker succeeds: both extensions run in glob order
    // with verified channels, contexts, and cleanup.
    let dir = TempDir::new("presync-run-ok").expect("fixture dir");
    let home = dir.path();
    stage_pair(home);
    let (outcome, calls) = drive_run(home, "prepare", &[], None, 0, 0);
    assert_eq!(
        outcome,
        Ok(pre_sync::Outcome {
            status: 0,
            warnings: Vec::new(),
        })
    );
    assert_eq!(
        calls,
        vec![
            "call=1 script=10-a.sh result_ok=1 temporary_ok=1 \
             records= set=eligible stage=prepare"
                .to_string(),
            "call=2 script=20-b.sh result_ok=1 temporary_ok=1 \
             records= set=eligible stage=prepare"
                .to_string(),
        ]
    );
    // The second worker fails: one warning, no third call, and
    // both scratch directories still come back clean.
    let dir = TempDir::new("presync-run-fail").expect("fixture dir");
    let home = dir.path();
    stage_pair(home);
    let (outcome, calls) = drive_run(home, "prepare", &[], Some(2), 0, 0);
    assert_eq!(
        outcome,
        Ok(pre_sync::Outcome {
            status: 1,
            warnings: vec!["  warning: pre-sync extension failed: 20-b.sh".to_string()],
        })
    );
    assert_eq!(
        calls,
        vec![
            "call=1 script=10-a.sh result_ok=1 temporary_ok=1 \
             records= set=eligible stage=prepare"
                .to_string(),
        ]
    );
}

#[test]
fn run_records_agree() {
    // A sealed record round-trips through each worker's context
    // on both engines, under the second stage spelling.
    let dir = TempDir::new("presync-run-records").expect("fixture dir");
    let home = dir.path();
    stage_pair(home);
    let home_text = home.to_string_lossy().into_owned();
    let record = format!(
        "web|{home_text}/.dotfiles-web|https://example.com/web.git|\
         {home_text}/conf/10-web.conf|false|git"
    );
    let (outcome, calls) = drive_run(home, "reconcile", &[record.as_bytes().to_vec()], None, 0, 0);
    assert_eq!(
        outcome,
        Ok(pre_sync::Outcome {
            status: 0,
            warnings: Vec::new(),
        })
    );
    assert_eq!(calls.len(), 2, "both extensions ran");
    for (position, call) in calls.iter().enumerate() {
        assert!(
            call.contains(&format!("records={record} set=eligible stage=reconcile")),
            "call {} seals the record: {call}",
            position + 1
        );
    }
}

#[test]
fn run_create_failure_agrees() {
    // An invalid record refuses context creation on both
    // engines with the context error, before any worker runs;
    // the allocated scratch directory stays registered (one
    // leftover entry per engine) exactly like the shell.
    let dir = TempDir::new("presync-run-badrecord").expect("fixture dir");
    let home = dir.path();
    stage_pair(home);
    let (outcome, calls) = drive_run(home, "prepare", &[b"bogus".to_vec()], None, 1, 1);
    assert_eq!(
        outcome,
        Err(pre_sync::Error::Invalid(
            "dot: overlay context: invalid overlay record".to_string()
        ))
    );
    assert!(calls.is_empty(), "no worker ran");
}
