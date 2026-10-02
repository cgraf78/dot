//! Native contracts for doctor orchestration.

use std::os::unix::fs::PermissionsExt as _;

use dot::doctor_orchestrator::{
    EngineSnapshot, Recorder, RuntimeSnapshot, WorkerExit, check_engine_source, check_runtime,
    collapse_log, create_result_file, extension_tail, result_paths, run_extension_for,
};
use dot_test_support::TempDir;

fn engine(source: &[u8]) -> EngineSnapshot {
    EngineSnapshot {
        source_raw: source.to_vec(),
        managed_raw: b"/managed".to_vec(),
        development_raw: b"/dev".to_vec(),
        source_real: source.to_vec(),
        managed_real: None,
        development_real: None,
        ignore_dev_checkout: false,
    }
}

#[test]
fn runtime_check_agrees() {
    let mut rec = Recorder::new();
    let runtime = RuntimeSnapshot {
        bash_version: b"5.2".to_vec(),
        bash_major: 5,
        bash_required: true,
        checkout_root: Some(b"/src".to_vec()),
        release_root: false,
        source_raw: b"/src".to_vec(),
        source_root: b"/src".to_vec(),
        git_version: Some(b"git version 2".to_vec()),
        config_version: b"1".to_vec(),
        unknown_config_keys: Vec::new(),
    };
    check_runtime(&mut rec, &runtime, &engine(b"/src"), b"/home/u");
    // Bash, checkout, and Git pass; the configuration version is an
    // informational row and never counts.
    assert_eq!(rec.counts().pass, 3);
    assert!(
        String::from_utf8(rec.render())
            .expect("utf8")
            .contains("  • configuration version (1)\n")
    );
    assert_eq!(rec.counts().warn, 1);
    assert_eq!(rec.counts().fail, 0);
    let mut old = runtime.clone();
    old.bash_major = 3;
    old.checkout_root = None;
    old.git_version = None;
    let mut failures = Recorder::new();
    check_runtime(&mut failures, &old, &engine(b"/src"), b"");
    assert_eq!(failures.counts().fail, 3);
}

#[test]
fn runtime_check_warns_once_per_unknown_config_key() {
    let unknown = |key: &str, line| dot::config::UnknownKey {
        key: key.to_string(),
        line,
    };
    let runtime = RuntimeSnapshot {
        bash_version: Vec::new(),
        bash_major: 0,
        bash_required: false,
        checkout_root: Some(b"/src".to_vec()),
        release_root: false,
        source_raw: b"/src".to_vec(),
        source_root: b"/src".to_vec(),
        git_version: Some(b"git version 2".to_vec()),
        config_version: b"1".to_vec(),
        unknown_config_keys: vec![unknown("future_key", 2), unknown("defualt_profile", 3)],
    };
    let mut rec = Recorder::new();
    check_runtime(&mut rec, &runtime, &engine(b"/src"), b"/home/u");
    // A key from a newer Dot is a warning (beside the engine-source
    // warning this fixture always produces); a likely misspelling fails,
    // because it makes every `dot update` exit 1.
    assert_eq!(rec.counts().fail, 1);
    assert_eq!(rec.counts().warn, 2);
    let rendered = String::from_utf8(rec.render()).expect("utf8 render");
    assert!(
        rendered.contains("unknown configuration key ignored")
            && rendered.contains("future_key on line 2 (newer dot?)")
            && rendered.contains("defualt_profile on line 3 (did you mean 'default_profile'?)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("  ⚠ unknown configuration key ignored\n    future_key")
            && rendered.contains("  ✗ unknown configuration key ignored\n    defualt_profile"),
        "{rendered}"
    );
}

#[test]
fn packaged_runtime_does_not_require_checkout_or_bash() {
    let mut rec = Recorder::new();
    let runtime = RuntimeSnapshot {
        bash_version: Vec::new(),
        bash_major: 0,
        bash_required: false,
        checkout_root: None,
        release_root: true,
        source_raw: b"/data/cgraf78/dot/releases/v1-linux-x86_64-musl".to_vec(),
        source_root: b"/data/cgraf78/dot/releases/v1-linux-x86_64-musl".to_vec(),
        git_version: Some(b"git version 2".to_vec()),
        config_version: b"1".to_vec(),
        unknown_config_keys: Vec::new(),
    };
    check_runtime(
        &mut rec,
        &runtime,
        &engine(&runtime.source_root),
        b"/home/u",
    );
    assert_eq!(rec.counts().fail, 0);
    let output = rec.render();
    assert!(
        output
            .windows(b"dot release exists".len())
            .any(|part| part == b"dot release exists")
    );
    assert!(
        output
            .windows(b"Bash runtime is not required".len())
            .any(|part| part == b"Bash runtime is not required")
    );
}

#[test]
fn engine_source_check_agrees() {
    for (managed, development, ignored, needle) in [
        (
            Some(b"/src".to_vec()),
            None,
            false,
            b"managed checkout".as_slice(),
        ),
        (
            None,
            Some(b"/src".to_vec()),
            false,
            b"development checkout".as_slice(),
        ),
        (None, None, false, b"outside managed locations".as_slice()),
        (
            Some(b"/src".to_vec()),
            None,
            true,
            b"bypass enabled".as_slice(),
        ),
    ] {
        let mut snapshot = engine(b"/src");
        snapshot.managed_real = managed;
        snapshot.development_real = development;
        snapshot.ignore_dev_checkout = ignored;
        let mut rec = Recorder::new();
        check_engine_source(&mut rec, &snapshot, b"/home/u");
        assert!(
            rec.render()
                .windows(needle.len())
                .any(|part| part == needle)
        );
    }
}

#[test]
fn temp_and_result_file_modes_agree() {
    let dir = TempDir::new("doctor-results-native").expect("fixture");
    let (results, log) = result_paths(dir.path());
    assert_eq!(log, dir.path().join("output"));
    create_result_file(&results).expect("result");
    assert_eq!(
        std::fs::metadata(&results)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::fs::write(&results, b"old").expect("old");
    create_result_file(&results).expect("truncate");
    assert_eq!(std::fs::read(&results).expect("read"), b"");
}

#[test]
fn extension_run_agrees() {
    let root = TempDir::new_exec("doctor-extension-native").expect("fixture");
    let script = root.write("extension.sh", b"");
    let mut rec = Recorder::new();
    let mut worker = |inv: &dot::doctor_orchestrator::WorkerInvocation<'_>| {
        std::fs::write(inv.log, b"noise\nline\n").expect("log");
        WorkerExit::from(0)
    };
    let mut render = |_: &std::path::Path, _: &mut Recorder| {};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let rc = run_extension_for(
        &mut rec,
        b"demo",
        &script,
        &[],
        root.path().to_str().expect("home utf8"),
        dot::temp::current_uid().expect("uid"),
        now,
        root.path(),
        &mut worker,
        &mut render,
    );
    assert_eq!(rc, 0);
    let outside = b"wrote outside the result API";
    assert!(
        rec.render()
            .windows(outside.len())
            .any(|part| part == outside)
    );
    assert_eq!(collapse_log(b"a\nb\n"), b"a b ");
    let mut failed = Recorder::new();
    extension_tail(&mut failed, b"demo", WorkerExit::from(7), b"bad\n");
    assert_eq!(failed.counts().fail, 1);

    let missing_root = root.path().join("missing").join("nested");
    let mut unavailable = Recorder::new();
    let rc = run_extension_for(
        &mut unavailable,
        b"demo",
        &script,
        &[],
        root.path().to_str().expect("home utf8"),
        dot::temp::current_uid().expect("uid"),
        now,
        &missing_root,
        &mut worker,
        &mut render,
    );
    assert_eq!(rc, 1);
    let unavailable_message = b"temporary directory unavailable";
    assert!(
        unavailable
            .render()
            .windows(unavailable_message.len())
            .any(|part| part == unavailable_message)
    );
}

#[test]
fn timed_out_extension_files_one_failure_with_its_limit() {
    let mut rec = Recorder::new();
    extension_tail(
        &mut rec,
        b"20-slow",
        WorkerExit {
            rc: 1,
            timed_out: Some(std::time::Duration::from_secs(20)),
        },
        b"partial\n",
    );
    assert_eq!(rec.counts().fail, 1);
    let rendered = String::from_utf8(rec.render()).expect("utf8");
    assert_eq!(
        rendered,
        "  ✗ 20-slow doctor extension timed out\n    stopped after 20s; set DOT_DOCTOR_TIMEOUT to raise the limit; output: partial \n"
    );
    // A clean failure keeps its historical row; quiet output adds no
    // empty detail line.
    let mut failed = Recorder::new();
    extension_tail(&mut failed, b"30-bad", WorkerExit::from(3), b"");
    assert_eq!(
        String::from_utf8(failed.render()).expect("utf8"),
        "  ✗ 30-bad doctor extension failed\n"
    );
}
