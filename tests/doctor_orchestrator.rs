//! Native contracts for doctor orchestration.

use std::os::unix::fs::PermissionsExt as _;

use dot::doctor_orchestrator::{
    EngineSnapshot, ExtensionRunner, Kernel, Loader, Recorder, RuntimeSnapshot, SECTION_FILES,
    check_engine_source, check_runtime, collapse_log, create_result_file, doctor_title,
    extension_tail, physical_dir, result_paths, run_doctor, run_extension_for, section_paths,
    sections_present, split_spec, summary_box,
};
use dot_test_support::TempDir;

#[test]
fn load_publishes_sections_once() {
    let dir = TempDir::new("doctor-load-native").expect("fixture");
    for file in SECTION_FILES {
        dir.write(file, b"");
    }
    assert!(sections_present(dir.path()));
    assert_eq!(section_paths(dir.path()).len(), 7);
    let mut loader = Loader::new();
    assert_eq!(loader.load(dir.path()), Some(section_paths(dir.path())));
    assert!(loader.is_loaded());
    assert_eq!(loader.load(dir.path()), None);
}

#[test]
fn physical_dir_matches_cd_p() {
    let dir = TempDir::new("doctor-physical-dir").expect("fixture");
    let real = dir.path().join("real");
    std::fs::create_dir_all(&real).expect("real");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    assert_eq!(
        physical_dir(link.as_os_str().as_encoded_bytes()),
        Some(
            std::fs::canonicalize(real)
                .expect("canonical")
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        )
    );
    assert_eq!(physical_dir(b""), None);
    assert_eq!(
        physical_dir(dir.path().join("missing").as_os_str().as_encoded_bytes()),
        None
    );
}

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
    };
    check_runtime(&mut rec, &runtime, &engine(b"/src"), b"/home/u");
    assert_eq!(rec.counts().pass, 4);
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
fn from_env_resolution_agrees() {
    let snapshot = EngineSnapshot::from_env(b"/definitely/missing", b"/home/u");
    assert_eq!(snapshot.managed_raw, b"/home/u/.local/share/cgraf78/dot");
    assert_eq!(snapshot.development_raw, b"/home/u/git/dot");
    assert_eq!(snapshot.source_real, b"/definitely/missing");
}

#[test]
fn summary_and_split_helpers_agree() {
    assert_eq!(doctor_title(), b"\ndot doctor\n\n");
    assert_eq!(split_spec(b""), None);
    assert_eq!(
        split_spec(b"key\tpath\twith-tab"),
        Some((&b"key"[..], &b"path\twith-tab"[..]))
    );
    let box_bytes = summary_box(b"1 passed");
    assert!(box_bytes.windows(8).any(|part| part == b"1 passed"));
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
        0
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
    extension_tail(&mut failed, b"demo", 7, b"bad\n");
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
fn doctor_skeleton_agrees() {
    let runtime = RuntimeSnapshot {
        bash_version: b"5".to_vec(),
        bash_major: 5,
        bash_required: true,
        checkout_root: Some(b"/src".to_vec()),
        release_root: false,
        source_raw: b"/src".to_vec(),
        source_root: b"/src".to_vec(),
        git_version: Some(b"git".to_vec()),
        config_version: b"1".to_vec(),
    };
    let mut kernels: Vec<Kernel<'_>> = vec![Box::new(|rec| {
        rec.section(b"Kernel");
        rec.ok(b"healthy", None);
        rec.warn(b"watch", Some(b"detail"));
    })];
    let mut runner: ExtensionRunner<'_> = Box::new(|rec, key, script| {
        assert_eq!(key, b"ext");
        assert_eq!(script, b"/script");
        rec.skip(b"extension", None);
        0
    });
    let mut out = Vec::new();
    let mut rec = Recorder::new();
    assert!(run_doctor(
        &mut out,
        &mut rec,
        &runtime,
        &engine(b"/src"),
        b"/home/u",
        &mut kernels,
        &Ok(vec![b"".to_vec(), b"ext\t/script".to_vec()]),
        &mut runner
    ));
    assert!(out.starts_with(&doctor_title()));
    assert!(out.windows(6).any(|part| part == b"passed"));

    let mut failed = Recorder::new();
    let mut no_kernels: Vec<Kernel<'_>> = vec![];
    let mut unused: ExtensionRunner<'_> = Box::new(|_, _, _| 0);
    let mut failed_out = Vec::new();
    assert!(!run_doctor(
        &mut failed_out,
        &mut failed,
        &runtime,
        &engine(b"/src"),
        b"",
        &mut no_kernels,
        &Err(()),
        &mut unused
    ));
    let discovery_failed = b"doctor extension discovery failed";
    assert!(
        failed
            .render()
            .windows(discovery_failed.len())
            .any(|part| part == discovery_failed)
    );
}
