//! Native contracts for pre-sync extension discovery and orchestration.

use dot::extension_trust::Inputs;
use dot::pre_sync;
use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}
fn inputs(home: &Path, ext: &Path) -> Inputs {
    Inputs {
        euid: dot::temp::current_uid().expect("uid"),
        home: home.to_string_lossy().into_owned(),
        extensions_dir: ext.to_string_lossy().into_owned(),
        manifest: String::new(),
        retiring_root: String::new(),
    }
}
fn script(root: &Path, name: &str, mode: u32) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parents");
    std::fs::write(&path, b"#!/bin/sh\n").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
    path
}
fn fixture(tag: &str, names: &[&str]) -> (TempDir, PathBuf) {
    let dir = TempDir::new(tag).expect("fixture");
    let ext = dir.path().join("ext");
    let root = ext.join("pre-sync.d");
    std::fs::create_dir_all(&root).expect("root");
    for name in names {
        script(&root, name, 0o644);
    }
    (dir, ext)
}

#[test]
fn specs_agree() {
    let dir = TempDir::new("presync-specs-native").expect("fixture");
    assert_eq!(
        pre_sync::specs(&inputs(dir.path(), Path::new("")), &[]),
        Ok(vec![])
    );
    let missing = dir.path().join("missing");
    assert_eq!(
        pre_sync::specs(&inputs(dir.path(), &missing), &[]),
        Ok(vec![])
    );
    let ext = dir.path().join("ext");
    std::fs::create_dir_all(&ext).expect("ext");
    std::fs::write(ext.join("pre-sync.d"), b"file").expect("file");
    assert_eq!(
        pre_sync::specs(&inputs(dir.path(), &ext), &[])
            .unwrap_err()
            .error,
        pre_sync::Error::Refused
    );
    std::fs::remove_file(ext.join("pre-sync.d")).expect("remove");
    let root = ext.join("pre-sync.d");
    std::fs::create_dir_all(&root).expect("root");
    for name in [
        "10-a.sh",
        "20-b.sh",
        "30_c.sh",
        "plain.sh",
        "x.serial.sh",
        "README",
        ".hidden.sh",
    ] {
        script(&root, name, 0o644);
    }
    let found = pre_sync::specs(&inputs(dir.path(), &ext), &[]).expect("specs");
    assert_eq!(
        found.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        vec!["10-a", "20-b", "30_c", "plain", "x"]
    );
}

#[test]
fn specs_identity_failures_agree() {
    for (files, emitted, message) in [
        (vec!["Bogus.sh"], 0, "invalid pre-sync extension identity"),
        (vec!["10.sh"], 0, "invalid pre-sync extension identity"),
        (vec!["foo.bar.sh"], 0, "invalid pre-sync extension identity"),
        (
            vec!["10-foo.sh", "10_foo.sh"],
            1,
            "duplicate pre-sync extension identity",
        ),
        (
            vec!["10-a.sh", "Bogus.sh"],
            1,
            "invalid pre-sync extension identity",
        ),
    ] {
        let (dir, ext) = fixture("presync-identity-native", &files);
        let failed = pre_sync::specs(&inputs(dir.path(), &ext), &[]).unwrap_err();
        assert_eq!(failed.emitted.len(), emitted);
        assert!(
            matches!(failed.error,pre_sync::Error::Invalid(ref text) if text.contains(message))
        );
    }
}

#[test]
fn specs_unreadable_extension_agrees() {
    let (dir, ext) = fixture("presync-mode-native", &[]);
    script(&ext.join("pre-sync.d"), "bad.sh", 0o664);
    assert_eq!(
        pre_sync::specs(&inputs(dir.path(), &ext), &[])
            .unwrap_err()
            .error,
        pre_sync::Error::Refused
    );
    let (dir, ext) = fixture("presync-link-native", &[]);
    std::os::unix::fs::symlink("nowhere", ext.join("pre-sync.d/dead.sh")).expect("link");
    assert_eq!(
        pre_sync::specs(&inputs(dir.path(), &ext), &[])
            .unwrap_err()
            .error,
        pre_sync::Error::Refused
    );
}

fn drive(
    home: &Path,
    ext: &Path,
    stage: &str,
    records: &[Vec<u8>],
    fail_at: Option<usize>,
) -> (Result<pre_sync::Outcome, pre_sync::Error>, Vec<String>) {
    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).expect("scratch");
    let timestamp = now();
    let mut seen = 0;
    let mut calls = vec![];
    let mut runner = |call: &pre_sync::Call| {
        seen += 1;
        if Some(seen) == fail_at {
            return false;
        }
        assert!(call.temporary.is_dir());
        assert_eq!(
            std::fs::metadata(&call.result)
                .expect("result")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let decoded = dot::overlay_context::consume(
            &call.context,
            &call.token,
            "pre-sync",
            home.to_str().expect("home"),
            dot::temp::current_uid().expect("uid"),
            timestamp,
        )
        .expect("consume");
        calls.push(format!(
            "{}:{}:{}:{}",
            call.key,
            decoded.set_kind,
            decoded.stage,
            decoded.records.join(" ")
        ));
        true
    };
    let outcome = pre_sync::run(
        stage,
        records,
        &inputs(home, ext),
        &[],
        &scratch,
        &mut runner,
    );
    assert_eq!(
        std::fs::read_dir(&scratch).expect("scratch list").count(),
        0
    );
    (outcome, calls)
}

#[test]
fn run_stage_and_empty_agree() {
    let (dir, ext) = fixture("presync-empty-native", &[]);
    for stage in ["", "bogus"] {
        let mut never = |_: &pre_sync::Call| panic!("runner");
        assert_eq!(
            pre_sync::run(
                stage,
                &[],
                &inputs(dir.path(), &ext),
                &[],
                dir.path(),
                &mut never
            ),
            Err(pre_sync::Error::Usage)
        );
    }
    for stage in ["prepare", "reconcile"] {
        assert_eq!(
            drive(dir.path(), &ext, stage, &[], None).0,
            Ok(pre_sync::Outcome {
                status: 0,
                warnings: vec![]
            })
        );
    }
}

#[test]
fn run_orchestration_agrees() {
    let (dir, ext) = fixture("presync-run-native", &["10-a.sh", "20-b.sh"]);
    let (ok, calls) = drive(dir.path(), &ext, "prepare", &[], None);
    assert_eq!(ok.expect("ok").status, 0);
    assert_eq!(
        calls.iter().map(|c| &c[..4]).collect::<Vec<_>>(),
        vec!["10-a", "20-b"]
    );
    let (dir, ext) = fixture("presync-fail-native", &["10-a.sh", "20-b.sh", "30-c.sh"]);
    let (failed, calls) = drive(dir.path(), &ext, "prepare", &[], Some(2));
    let failed = failed.expect("outcome");
    assert_eq!(failed.status, 1);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        failed.warnings,
        vec!["  warning: pre-sync extension failed: 20-b.sh"]
    );
}

#[test]
fn run_records_agree() {
    let (dir, ext) = fixture("presync-record-native", &["10-a.sh", "20-b.sh"]);
    let record = format!(
        "web|{}/.dotfiles-web|https://example.com/web.git|{}/conf/10-web.conf|false|git",
        dir.path().display(),
        dir.path().display()
    );
    let (out, calls) = drive(
        dir.path(),
        &ext,
        "reconcile",
        &[record.as_bytes().to_vec()],
        None,
    );
    assert_eq!(out.expect("outcome").status, 0);
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|call| call.contains(&format!("eligible:reconcile:{record}")))
    );
}

#[test]
fn run_create_failure_agrees() {
    let (dir, ext) = fixture("presync-context-fail-native", &["10-a.sh"]);
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).expect("scratch");
    let mut never = |_: &pre_sync::Call| panic!("must not run");
    let result = pre_sync::run(
        "prepare",
        &[b"bogus".to_vec()],
        &inputs(dir.path(), &ext),
        &[],
        &scratch,
        &mut never,
    );
    assert!(
        matches!(result,Err(pre_sync::Error::Invalid(ref text)) if text.contains("invalid overlay record"))
    );
    assert_eq!(std::fs::read_dir(&scratch).expect("list").count(), 1);
}
