//! Native contracts for doctor orchestration.

use std::os::unix::fs::PermissionsExt as _;

use dot::doctor_orchestrator::{
    EngineLocation, EngineSnapshot, FailureNote, Recorder, RuntimeSnapshot, WorkerExit,
    check_engine_source, check_runtime, create_result_file, engine_location, extension_tail,
    failure_path, log_lines, result_paths, run_extension_for,
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
        git_stalled: false,
        git_path: b"/usr/bin/git".to_vec(),
        version: b"20261003-000000-abcdef12".to_vec(),
        install_kind: None,
        config_path: b"/home/u/.config/dot/config".to_vec(),
        unknown_config_keys: Vec::new(),
    };
    check_runtime(&mut rec, &runtime, &engine(b"/src"), b"/home/u");
    // The version row (naming the checkout), Bash, and Git pass; no
    // constant configuration-version row is filed.
    assert_eq!(rec.counts().pass, 3);
    let rendered = String::from_utf8(rec.render()).expect("utf8");
    assert!(
        rendered.starts_with("\ndot runtime\n  ✓ dot 20261003-000000-abcdef12 (/src, checkout)\n"),
        "{rendered}"
    );
    assert!(!rendered.contains("configuration version"), "{rendered}");
    assert_eq!(rec.counts().warn, 1);
    assert_eq!(rec.counts().fail, 0);
    let mut old = runtime.clone();
    old.bash_major = 3;
    old.checkout_root = None;
    old.git_version = None;
    let mut failures = Recorder::new();
    check_runtime(&mut failures, &old, &engine(b"/src"), b"");
    assert_eq!(failures.counts().fail, 3);
    let rendered = String::from_utf8(failures.render()).expect("utf8");
    assert!(
        rendered.contains(
            "  ✗ Bash runtime is too old\n    Bash 4 or newer is required\n    → install Bash 4 or newer, or point DOT_BASH at one\n"
        ),
        "{rendered}"
    );
}

#[test]
fn a_stalled_git_probe_warns_and_a_missing_git_says_what_to_do() {
    // A `git --version` that misses the probe deadline is a stalled
    // filesystem or Git, like every other stalled core probe: a warning
    // with a next step, not the hard "unavailable" failure a missing Git
    // gets.
    let runtime = RuntimeSnapshot {
        bash_version: Vec::new(),
        bash_major: 0,
        bash_required: false,
        checkout_root: Some(b"/src".to_vec()),
        release_root: false,
        source_raw: b"/src".to_vec(),
        source_root: b"/src".to_vec(),
        git_version: None,
        git_stalled: true,
        git_path: b"/usr/bin/git".to_vec(),
        version: b"20261003-000000-abcdef12".to_vec(),
        install_kind: None,
        config_path: b"/home/u/.config/dot/config".to_vec(),
        unknown_config_keys: Vec::new(),
    };
    let mut stalled = Recorder::new();
    check_runtime(&mut stalled, &runtime, &engine(b"/src"), b"/home/u");
    let rendered = String::from_utf8(stalled.render()).expect("utf8");
    assert!(
        rendered.contains(
            "  ⚠ Git runtime did not answer\n    /usr/bin/git --version did not answer within 30s\n    → check for a stalled"
        ),
        "{rendered}"
    );
    assert_eq!(stalled.counts().fail, 0, "{rendered}");

    let mut missing = Recorder::new();
    let gone = RuntimeSnapshot {
        git_stalled: false,
        git_path: b"/usr/bin/git".to_vec(),
        ..runtime
    };
    check_runtime(&mut missing, &gone, &engine(b"/src"), b"/home/u");
    let rendered = String::from_utf8(missing.render()).expect("utf8");
    assert!(
        rendered.contains("  ✗ Git runtime is unavailable\n    → install Git"),
        "{rendered}"
    );
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
        git_stalled: false,
        git_path: b"/usr/bin/git".to_vec(),
        version: b"20261003-000000-abcdef12".to_vec(),
        install_kind: None,
        config_path: b"/home/u/.config/dot/config".to_vec(),
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
    // A likely typo names its fix; a newer key needs a newer dot.
    assert!(
        rendered.contains("    → rename it to 'default_profile' in ~/.config/dot/config\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("    → fix the key if it is a typo; otherwise update dot"),
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
        git_stalled: false,
        git_path: b"/usr/bin/git".to_vec(),
        version: b"20261003-000000-abcdef12".to_vec(),
        install_kind: None,
        config_path: b"/home/u/.config/dot/config".to_vec(),
        unknown_config_keys: Vec::new(),
    };
    check_runtime(
        &mut rec,
        &runtime,
        &engine(&runtime.source_root),
        b"/home/u",
    );
    assert_eq!(rec.counts().fail, 0);
    let output = String::from_utf8(rec.render()).expect("utf8");
    // Outside the managed root the release is a bare "release", and the
    // engine-source warning says so.
    assert!(
        output.contains(
            "✓ dot 20261003-000000-abcdef12 (/data/cgraf78/dot/releases/v1-linux-x86_64-musl, release)"
        ),
        "{output}"
    );
    assert!(output.contains("Bash runtime is not required"), "{output}");
}

#[test]
fn version_row_names_how_the_running_build_is_installed() {
    // The row that replaced "release exists", "engine source", and "release
    // layout": the layout's healthy kind wins for a release, a managed
    // release never reads as a "checkout", and a checkout names its location.
    let release = |install_kind: Option<&str>, managed: bool| {
        let runtime = RuntimeSnapshot {
            bash_version: Vec::new(),
            bash_major: 0,
            bash_required: false,
            checkout_root: None,
            release_root: true,
            source_raw: b"/home/u/.local/share/cgraf78/dot".to_vec(),
            source_root: b"/home/u/.local/share/cgraf78/dot".to_vec(),
            git_version: Some(b"git version 2".to_vec()),
            git_stalled: false,
            git_path: b"/usr/bin/git".to_vec(),
            version: b"v1".to_vec(),
            install_kind: install_kind.map(str::to_string),
            config_path: b"/home/u/.config/dot/config".to_vec(),
            unknown_config_keys: Vec::new(),
        };
        let mut snapshot = engine(&runtime.source_root);
        if managed {
            snapshot.managed_real = Some(runtime.source_root.clone());
        }
        let mut rec = Recorder::new();
        check_runtime(&mut rec, &runtime, &snapshot, b"/home/u");
        (String::from_utf8(rec.render()).expect("utf8"), rec.counts())
    };
    let (shdeps, counts) = release(Some("Shdeps release"), true);
    assert!(
        shdeps.contains("  ✓ dot v1 (~/.local/share/cgraf78/dot, Shdeps release)\n"),
        "{shdeps}"
    );
    assert_eq!((counts.warn, counts.fail), (0, 0), "{shdeps}");
    assert!(!shdeps.contains("checkout"), "{shdeps}");
    let (managed, _) = release(None, true);
    assert!(
        managed.contains("✓ dot v1 (~/.local/share/cgraf78/dot, managed install)"),
        "{managed}"
    );

    let checkout = |managed: bool| {
        let runtime = RuntimeSnapshot {
            bash_version: b"5.2".to_vec(),
            bash_major: 5,
            bash_required: true,
            checkout_root: Some(b"/home/u/git/dot".to_vec()),
            release_root: false,
            source_raw: b"/home/u/git/dot".to_vec(),
            source_root: b"/home/u/git/dot".to_vec(),
            git_version: Some(b"git version 2".to_vec()),
            git_stalled: false,
            git_path: b"/usr/bin/git".to_vec(),
            version: b"v2".to_vec(),
            install_kind: None,
            config_path: b"/home/u/.config/dot/config".to_vec(),
            unknown_config_keys: Vec::new(),
        };
        let mut snapshot = engine(&runtime.source_root);
        if managed {
            snapshot.managed_real = Some(runtime.source_root.clone());
        } else {
            snapshot.development_real = Some(runtime.source_root.clone());
        }
        let mut rec = Recorder::new();
        check_runtime(&mut rec, &runtime, &snapshot, b"/home/u");
        String::from_utf8(rec.render()).expect("utf8")
    };
    let development = checkout(false);
    assert!(
        development.contains("✓ dot v2 (~/git/dot, development checkout)"),
        "{development}"
    );
    assert!(!development.contains("engine source"), "{development}");
    assert!(checkout(true).contains("✓ dot v2 (~/git/dot, managed checkout)"));
}

#[test]
fn engine_source_check_agrees() {
    // A managed or development source files nothing here (the version row
    // names it); only the bypass and an outside source warn.
    for (managed, development, ignored, location, warning) in [
        (
            Some(b"/src".to_vec()),
            None,
            false,
            EngineLocation::Managed,
            None,
        ),
        (
            None,
            Some(b"/src".to_vec()),
            false,
            EngineLocation::Development,
            None,
        ),
        (
            None,
            None,
            false,
            EngineLocation::Outside,
            Some("outside managed locations"),
        ),
        (
            Some(b"/src".to_vec()),
            None,
            true,
            EngineLocation::Managed,
            Some("bypass enabled"),
        ),
    ] {
        let mut snapshot = engine(b"/src");
        snapshot.managed_real = managed;
        snapshot.development_real = development;
        snapshot.ignore_dev_checkout = ignored;
        assert_eq!(engine_location(&snapshot), location);
        let mut rec = Recorder::new();
        check_engine_source(&mut rec, &snapshot);
        let rendered = String::from_utf8(rec.render()).expect("utf8");
        match warning {
            Some(needle) => assert!(rendered.contains(needle), "{rendered}"),
            None => assert!(rendered.is_empty(), "{rendered}"),
        }
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
    assert_eq!(log_lines(b"a\nb\n"), vec![b"a".to_vec(), b"b".to_vec()]);
    let mut failed = Recorder::new();
    extension_tail(&mut failed, b"demo", WorkerExit::from(7), b"bad\n", None);
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
        None,
    );
    assert_eq!(rec.counts().fail, 1);
    let rendered = String::from_utf8(rec.render()).expect("utf8");
    assert_eq!(
        rendered,
        "  ✗ 20-slow doctor extension timed out\n    stopped after 20s\n    - partial\n    → set DOT_DOCTOR_TIMEOUT to raise the limit\n"
    );
    // A quiet failure still names its exit status (K3).
    let mut failed = Recorder::new();
    extension_tail(&mut failed, b"30-bad", WorkerExit::from(3), b"", None);
    assert_eq!(
        String::from_utf8(failed.render()).expect("utf8"),
        "  ✗ 30-bad doctor extension failed\n    exited with status 3\n    → fix it, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
}

#[test]
fn failure_note_names_where_the_extension_stopped() {
    let tail = |rc: i32, note: &[u8], log: &[u8]| {
        let mut rec = Recorder::new();
        let note = FailureNote::parse(note);
        extension_tail(&mut rec, b"10-x", WorkerExit::from(rc), log, note.as_ref());
        String::from_utf8(rec.render()).expect("utf8")
    };
    assert_eq!(
        tail(
            1,
            b"1\0run\0doctor.d/10-x.sh:3\0grep -q a b",
            b"grep: b: No such file\n"
        ),
        "  ✗ 10-x doctor extension failed\n    exited with status 1 at doctor.d/10-x.sh:3: grep -q a b\n    - grep: b: No such file\n    → fix doctor.d/10-x.sh:3, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
    // `doctor` itself returned the status: only the command is known.
    assert_eq!(
        tail(4, b"4\0run\0\0return 4", b""),
        "  ✗ 10-x doctor extension failed\n    exited with status 4; last command: return 4\n    → fix it, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
    // A note from an unrelated earlier failure must not point at the wrong
    // line: the worker exited with another status.
    assert_eq!(
        tail(3, b"1\0run\0doctor.d/10-x.sh:3\0false", b""),
        "  ✗ 10-x doctor extension failed\n    exited with status 3\n    → fix it, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
}

#[test]
fn failure_note_parse_rejects_malformed_notes_and_bounds_commands() {
    assert_eq!(FailureNote::parse(b""), None);
    assert_eq!(FailureNote::parse(b"x\0run\0a\0b"), None);
    assert_eq!(FailureNote::parse(b"1\0later\0a\0b"), None);
    assert_eq!(FailureNote::parse(b"1\0run\0only-three"), None);
    let note = FailureNote::parse(b"2\0run\0f.sh:1\0printf 'a\n\tb'").expect("note");
    assert_eq!(note.status, 2);
    assert!(!note.loading);
    assert_eq!(note.location, b"f.sh:1");
    assert_eq!(note.command, b"printf 'a  b'");
    // Long commands are cut on a character boundary.
    let mut long = b"1\0run\0f.sh:1\0".to_vec();
    long.extend("é".repeat(150).as_bytes());
    let note = FailureNote::parse(&long).expect("long note");
    let text = String::from_utf8(note.command).expect("still valid UTF-8");
    assert!(text.ends_with('…'), "{text}");
    assert!(text.len() <= 200 + '…'.len_utf8(), "{}", text.len());
    assert_eq!(
        failure_path(std::path::Path::new("/s/results")),
        std::path::Path::new("/s/results.failure")
    );
}

#[test]
fn stray_output_lists_each_line() {
    let mut rec = Recorder::new();
    extension_tail(&mut rec, b"10-x", WorkerExit::from(0), b"one\ntwo\n", None);
    assert_eq!(rec.counts().warn, 1);
    assert_eq!(
        String::from_utf8(rec.render()).expect("utf8"),
        "  ⚠ 10-x doctor extension wrote outside the result API\n    - one\n    - two\n"
    );
}

#[test]
fn failure_note_covers_load_failures_and_long_output() {
    let tail = |rc: i32, note: &[u8], log: &[u8]| {
        let mut rec = Recorder::new();
        let note = FailureNote::parse(note);
        extension_tail(&mut rec, b"10-x", WorkerExit::from(rc), log, note.as_ref());
        String::from_utf8(rec.render()).expect("utf8")
    };
    // The file failed while it was being sourced, with no line to blame.
    assert_eq!(
        tail(1, b"1\0load\0\0", b""),
        "  ✗ 10-x doctor extension failed\n    exited with status 1 while loading the extension file\n    → fix it, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
    // A line in the file's own top level is still named.
    assert_eq!(
        tail(1, b"1\0load\0doctor.d/10-x.sh:2\0false", b""),
        "  ✗ 10-x doctor extension failed\n    exited with status 1 at doctor.d/10-x.sh:2: false\n    → fix doctor.d/10-x.sh:2, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
    // A crash keeps its last output lines: the explanation comes last.
    let log: String = (1..=8).map(|n| format!("line {n}\n")).collect();
    assert_eq!(
        tail(2, b"", log.as_bytes()),
        "  ✗ 10-x doctor extension failed\n    exited with status 2; last 5 output lines shown\n    - line 4\n    - line 5\n    - line 6\n    - line 7\n    - line 8\n    → fix it, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'\n"
    );
    // Stray output made only of blank lines still says what it was.
    let mut rec = Recorder::new();
    extension_tail(&mut rec, b"10-x", WorkerExit::from(0), b"\n\n", None);
    assert_eq!(
        String::from_utf8(rec.render()).expect("utf8"),
        "  ⚠ 10-x doctor extension wrote outside the result API\n    blank lines only\n"
    );
}
