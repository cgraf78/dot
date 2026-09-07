//! End-to-end native cutover tests. Every provider fixture poisons all shell
//! test-engine stages, so successful output cannot come from the old adapter.
#[path = "support/test_fixture.rs"]
mod fixture;
use fixture::{Fixture, success};

#[test]
fn native_help_and_list_do_not_load_test_engine() {
    let f = Fixture::new();
    f.suite("core", "exit 0");
    f.suite("core-extra", "exit 0");
    let help = f.run(&["--help"]);
    success(&help);
    assert!(String::from_utf8_lossy(&help.stdout).contains("DOT_TEST_INCLUDE_PROVIDER=1"));
    let list = f.run(&["--list"]);
    success(&list);
    assert_eq!(list.stdout, b"core-extra\ncore\n");
}

#[test]
fn native_argument_errors_precede_execution() {
    let f = Fixture::new();
    f.suite("core", "exit 0");
    for (args, expected) in [
        (vec!["-j"], "missing value for -j\n"),
        (vec!["-j", "01"], "invalid jobs value: 01\n"),
        (
            vec!["--jobs=1000000000"],
            "invalid jobs value: 1000000000\n",
        ),
        (vec!["--jobs=oops"], "invalid jobs value: oops\n"),
        (vec!["--bogus"], "unknown option: --bogus\n"),
        (vec!["missing"], "unknown test: missing\n"),
    ] {
        let output = f.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(output.stderr, expected.as_bytes(), "{args:?}");
    }
}

#[test]
fn native_filters_deduplicate_and_classify_results() {
    let f = Fixture::new();
    f.suite(
        "core",
        "printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    f.suite(
        "core-extra",
        "printf 'skip\\tfixture unavailable\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    f.suite("coreutils", "exit 99");
    let output = f.run(&["-s", "core", "core-extra"]);
    success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("1 passed, 1 skipped (2 total)"));
    assert!(text.contains("fixture unavailable"));
    assert!(!text.contains("coreutils"));
}

#[test]
fn native_malformed_results_fail_closed() {
    for (body, diagnostic) in [
        ("exit 0", "completed without a structured result"),
        (
            "printf 'complete\\t1\\t00\\n' >\"$DOT_TEST_RESULT_FILE\"",
            "emitted an invalid structured result",
        ),
        (
            "printf 'complete\\t1\\t0\\nextra\\n' >\"$DOT_TEST_RESULT_FILE\"",
            "emitted an invalid structured result",
        ),
    ] {
        let f = Fixture::new();
        f.suite("core", body);
        let output = f.run(&[]);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic));
    }
}

#[test]
fn native_source_home_cannot_borrow_spoofed_host_authority() {
    let f = Fixture::new();
    let source = f.scope.path().join("unrelated");
    std::fs::create_dir_all(source.join(".local/lib/dotfiles/tests")).unwrap();
    std::fs::write(source.join(".local/lib/dotfiles/tests/helpers.sh"), "").unwrap();
    let output = fixture::finish(
        f.command(&["--help"])
            .env("DOT_TEST_SOURCE_HOME", &source)
            .env("DOT_TEST_HOST_HOME", &source)
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("source home does not match the configured base repository")
    );
}

#[test]
fn native_suite_stdin_is_closed_in_both_modes() {
    let f = Fixture::new();
    f.suite(
        "stdin",
        "if read -r line; then exit 3; fi\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    for args in [vec![], vec!["-s"]] {
        success(&f.run(&args));
    }
}

#[test]
fn native_revalidates_queued_suite_before_spawn() {
    let f = Fixture::new();
    f.suite("alpha", "touch \"$HOME/ready\"; until [[ -f $HOME/release ]]; do sleep 0.02; done\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    f.suite(
        "beta",
        "touch \"$HOME/beta-ran\"; printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let child = f.command(&["-j", "1"]).spawn().unwrap();
    fixture::poll(|| f.home.join("ready").exists());
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        f.suites.join("beta-test"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    std::fs::write(f.home.join("release"), "").unwrap();
    let output = fixture::finish(child);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("test suite changed after discovery: beta-test")
    );
    assert!(!f.home.join("beta-ran").exists());
}

#[test]
fn native_skip_detail_preserves_spaces() {
    let f = Fixture::new();
    f.suite(
        "skip",
        "printf 'skip\\t  reason  \\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let output = f.run(&[]);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains(":   reason  \n"));
}

#[test]
fn native_complete_shell_command_oracle_with_poisoned_engine() {
    let f = Fixture::new();
    let output = f.oracle("test-command-test");
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("test-command-test: ok"));
}

#[test]
fn native_base_identity_failure_has_one_diagnostic() {
    let f = Fixture::new();
    let record = f.home.join(".local/state/dot/init/completed");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(record, "malformed\n").unwrap();
    let output = f.run(&["--list"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stderr,
        b"dot: malformed initialization identity record\n"
    );
}

#[test]
fn native_nonregular_result_cannot_block_completion() {
    let f = Fixture::new();
    f.suite(
        "fifo",
        "rm \"$DOT_TEST_RESULT_FILE\"; mkfifo \"$DOT_TEST_RESULT_FILE\"",
    );
    let output = f.run(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("completed without a structured result")
    );
}

#[test]
fn native_output_replacement_cannot_redirect_the_reader() {
    let f = Fixture::new();
    f.suite("output", "path=${DOT_TEST_RESULT_FILE%.result}.out; rm \"$path\"; mkfifo \"$path\"; echo ORIGINAL; printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    let output = f.run(&["-v"]);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("ORIGINAL"));
}

#[test]
fn native_automatic_jobs_uses_runtime_darwin_fallback() {
    let f = Fixture::new();
    let bin = f.scope.path().join("tools");
    std::fs::create_dir(&bin).unwrap();
    fixture::executable(&bin.join("getconf"), "exit 0");
    fixture::executable(&bin.join("uname"), "printf 'Darwin\\n'");
    fixture::executable(&bin.join("sysctl"), "printf '7\\n'");
    for index in 0..8 {
        f.suite(
            &format!("jobs-{index}"),
            "[[ $DOT_TEST_JOBS == 7 ]]\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
        );
    }
    let output = fixture::finish(f.command(&[]).env("PATH", bin).spawn().unwrap());
    success(&output);
}

#[test]
fn native_automatic_jobs_preserves_probe_whitespace() {
    for getconf in ["printf ' 7 \\n'", "printf '   \\n'"] {
        let f = Fixture::new();
        let bin = f.scope.path().join("tools");
        std::fs::create_dir(&bin).unwrap();
        fixture::executable(&bin.join("getconf"), getconf);
        fixture::executable(&bin.join("uname"), "printf 'Darwin\\n'");
        fixture::executable(&bin.join("sysctl"), "printf '7\\n'");
        for index in 0..8 {
            f.suite(
                &format!("jobs-{index}"),
                "[[ $DOT_TEST_JOBS == 4 ]]\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
            );
        }
        success(&fixture::finish(
            f.command(&[]).env("PATH", bin).spawn().unwrap(),
        ));
    }
}

#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn native_registered_source_keeps_non_utf8_path_identity() {
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;
    let f = Fixture::new();
    let git = |args: Vec<std::ffi::OsString>| {
        let output = Command::new("git")
            .args(args)
            .env("HOME", &f.home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(vec![
        "init".into(),
        "-q".into(),
        "--separate-git-dir".into(),
        f.home.join(".dotfiles").into_os_string(),
        f.home.as_os_str().to_owned(),
    ]);
    for (key, value) in [
        ("user.name", "fixture".into()),
        ("user.email", "fixture@example.invalid".into()),
        ("core.worktree", f.home.as_os_str().to_owned()),
        (
            "remote.origin.url",
            f.scope.path().join("origin.git").into_os_string(),
        ),
    ] {
        git(vec![
            "-C".into(),
            f.home.as_os_str().to_owned(),
            "config".into(),
            key.into(),
            value,
        ]);
    }
    let tests = f.home.join(".local/lib/dotfiles/tests");
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(tests.join("helpers.sh"), "").unwrap();
    fixture::executable(
        &tests.join("source-test"),
        "printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    git(vec![
        "-C".into(),
        f.home.as_os_str().to_owned(),
        "add".into(),
        ".local".into(),
    ]);
    git(vec![
        "-C".into(),
        f.home.as_os_str().to_owned(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "commit".into(),
        "-qm".into(),
        "fixture".into(),
    ]);
    let source = f
        .scope
        .path()
        .join(std::ffi::OsString::from_vec(b"source-\xff".to_vec()));
    git(vec![
        "-C".into(),
        f.home.as_os_str().to_owned(),
        "worktree".into(),
        "add".into(),
        "-q".into(),
        "-b".into(),
        "source".into(),
        source.as_os_str().to_owned(),
    ]);
    let output = fixture::finish(
        f.command(&["--list"])
            .env_remove("DOT_TEST_TESTS_DIR")
            .env("DOT_TEST_SOURCE_HOME", source)
            .spawn()
            .unwrap(),
    );
    success(&output);
    assert_eq!(output.stdout, b"dot\nsource\n");
}
