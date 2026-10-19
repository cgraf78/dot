//! End-to-end native cutover tests. Every provider fixture poisons all shell
//! test-engine stages, so successful output cannot come from the old adapter.
#[path = "support/test_fixture.rs"]
mod fixture;
use fixture::{Fixture, success};
use std::collections::BTreeMap;

#[test]
fn native_suites_do_not_inherit_shell_startup_files() {
    for variable in ["BASH_ENV", "ENV"] {
        let f = Fixture::new();
        let startup = f.home.join("startup.sh");
        std::fs::write(&startup, "if [[ ${DOT_TEST:-} == 1 ]]; then\n  touch \"$HOME/startup-ran\"\n  export PATH=/unexpected:$PATH\nfi\n").unwrap();
        f.suite("startup", "[[ ! -v BASH_ENV && ! -v ENV ]] || exit 21\n[[ ${PATH%%:*} == \"$HOME/.local/bin\" ]] || exit 22\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
        let output = fixture::finish(f.command(&[]).env(variable, &startup).spawn().unwrap());
        success(&output);
        assert!(!f.home.join("startup-ran").exists(), "{variable} executed");
    }
}

#[test]
fn native_suites_can_explicitly_enable_child_startup_files() {
    let f = Fixture::new();
    std::fs::write(
        f.home.join("startup.sh"),
        "touch \"$HOME/child-startup-ran\"\n",
    )
    .unwrap();
    f.suite("startup", "BASH_ENV=\"$HOME/startup.sh\" bash -c ':'\n[[ -f $HOME/child-startup-ran ]] || exit 23\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    success(&f.run(&[]));
}

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
fn native_provider_suite_is_explicitly_opted_in() {
    let f = Fixture::new();
    let extensions = f.scope.path().join("extensions");
    fixture::executable(
        &extensions.join("tests/client-test"),
        "touch \"$HOME/client-ran\"; printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    std::fs::create_dir_all(f.home.join(".config/dot")).unwrap();
    std::fs::write(
        f.home.join(".config/dot/config"),
        format!(
            "version=1\nextension_api=1\nextensions_dir={}\ndependency_provider=none\n",
            extensions.display()
        ),
    )
    .unwrap();
    fixture::executable(
        &f.root.join("tests/run"),
        "touch \"$HOME/provider-ran\"; printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );

    let output = fixture::finish(
        f.command(&[])
            .env_remove("DOT_TEST_TESTS_DIR")
            .spawn()
            .unwrap(),
    );
    success(&output);
    assert!(f.home.join("client-ran").is_file());
    assert!(!f.home.join("provider-ran").exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("dot ("));

    let output = fixture::finish(
        f.command(&["dot"])
            .env_remove("DOT_TEST_TESTS_DIR")
            .spawn()
            .unwrap(),
    );
    success(&output);
    assert!(f.home.join("provider-ran").is_file());
    assert!(String::from_utf8_lossy(&output.stdout).contains("dot ("));

    std::fs::remove_file(f.home.join("provider-ran")).unwrap();
    let output = fixture::finish(
        f.command(&[])
            .env_remove("DOT_TEST_TESTS_DIR")
            .env("DOT_TEST_INCLUDE_PROVIDER", "1")
            .spawn()
            .unwrap(),
    );
    success(&output);
    assert!(f.home.join("client-ran").is_file());
    assert!(f.home.join("provider-ran").is_file());

    let output = fixture::finish(
        f.command(&[])
            .env_remove("DOT_TEST_TESTS_DIR")
            .env("DOT_TEST_INCLUDE_PROVIDER", "yes")
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stderr, b"DOT_TEST_INCLUDE_PROVIDER must be 0 or 1\n");
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
fn native_discovery_refuses_an_unsafe_suite_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = Fixture::new();
    let extensions = f.scope.path().join("extensions");
    fixture::executable(&extensions.join("tests/unsafe-test"), "exit 0");
    std::fs::create_dir_all(f.home.join(".config/dot")).unwrap();
    std::fs::write(
        f.home.join(".config/dot/config"),
        format!(
            "version=1\nextension_api=1\nextensions_dir={}\ndependency_provider=none\n",
            extensions.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        extensions.join("tests/unsafe-test"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let output = fixture::finish(
        f.command(&["--list"])
            .env_remove("DOT_TEST_TESTS_DIR")
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stderr,
        format!(
            "dot: unsafe test extension: {}\n",
            extensions.join("tests/unsafe-test").display()
        )
        .as_bytes()
    );
}

#[test]
fn public_result_reporter_rejects_noncanonical_counts_without_writing() {
    let f = Fixture::new();
    let result = f.scope.path().join("result");
    std::fs::write(&result, b"").unwrap();
    let output = std::process::Command::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/dot/public/test-reporter-v1"),
    )
    .args(["complete", "1", "08"])
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap())
    .env("DOT_TEST_RESULT_FILE", &result)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
    assert_eq!(std::fs::read(result).unwrap(), b"");
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
            "[[ $DOT_TEST_JOBS == 7 ]] || exit 1\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
        );
    }
    let output = fixture::finish(f.command(&[]).env("PATH", bin).spawn().unwrap());
    success(&output);
}

/// Runs eight suites that each assert the effective `DOT_TEST_JOBS`, with a
/// processor probe that reports 64, under the given `PREFIX` and request.
fn assert_jobs_under_prefix(prefix: &str, request: &[(&str, &str)], args: &[&str], expected: u32) {
    let f = Fixture::new();
    let bin = f.scope.path().join("tools");
    std::fs::create_dir(&bin).unwrap();
    fixture::executable(&bin.join("getconf"), "printf '64\\n'");
    for index in 0..8 {
        f.suite(
            &format!("jobs-{index}"),
            &format!(
                "[[ $DOT_TEST_JOBS == {expected} ]] || exit 1\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\""
            ),
        );
    }
    let mut command = f.command(args);
    command.env("PATH", &bin).env("PREFIX", prefix);
    for (name, value) in request {
        command.env(name, value);
    }
    let output = fixture::finish(command.spawn().unwrap());
    success(&output);
}

#[test]
fn native_automatic_jobs_run_one_at_a_time_on_termux() {
    // A parallel run gets the whole Termux app killed on a phone, so the
    // default ignores the processor probe there.
    assert_jobs_under_prefix("/data/data/com.termux/files/usr", &[], &[], 1);
}

#[test]
fn native_explicit_jobs_still_win_on_termux() {
    let termux = "/data/data/com.termux/files/usr";
    assert_jobs_under_prefix(termux, &[("DOT_TEST_JOBS", "3")], &[], 3);
    assert_jobs_under_prefix(termux, &[], &["-j", "2"], 2);
}

#[test]
fn native_automatic_jobs_ignore_a_non_termux_prefix() {
    // Only Termux's `PREFIX` changes the default; Homebrew-style or empty
    // values keep the probe (capped at 8 here by the eight suites).
    assert_jobs_under_prefix("/opt/homebrew", &[], &[], 8);
    assert_jobs_under_prefix("", &[], &[], 8);
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
                "[[ $DOT_TEST_JOBS == 4 ]] || exit 1\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
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
    let f = Fixture::new();
    let git = |args: Vec<std::ffi::OsString>| {
        let output = dot_test_support::git()
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
        "printf 'HOME=%s\\nSOURCE=%s\\nHOST=%s\\nPATH=%s\\n' \"$HOME\" \"$DOT_TEST_SOURCE_HOME\" \"$DOT_TEST_HOST_HOME\" \"$PATH\"\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
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
            .env("DOT_TEST_SOURCE_HOME", &source)
            .spawn()
            .unwrap(),
    );
    success(&output);
    assert_eq!(output.stdout, b"dot\nsource\n");

    let output = fixture::finish(
        f.command(&["-s", "-v", "source"])
            .env_remove("DOT_TEST_TESTS_DIR")
            .env("DOT_TEST_SOURCE_HOME", &source)
            .spawn()
            .unwrap(),
    );
    success(&output);
    let mut home_line = b"HOME=".to_vec();
    home_line.extend_from_slice(source.as_os_str().as_encoded_bytes());
    home_line.push(b'\n');
    assert!(
        output
            .stdout
            .windows(home_line.len())
            .any(|part| part == home_line)
    );
    let mut source_line = b"SOURCE=".to_vec();
    source_line.extend_from_slice(source.as_os_str().as_encoded_bytes());
    source_line.push(b'\n');
    assert!(
        output
            .stdout
            .windows(source_line.len())
            .any(|part| part == source_line)
    );
    let host_line = format!("HOST={}\n", f.home.display());
    assert!(
        output
            .stdout
            .windows(host_line.len())
            .any(|part| part == host_line.as_bytes())
    );
    let mut source_bin = b"PATH=".to_vec();
    source_bin.extend_from_slice(source.as_os_str().as_encoded_bytes());
    source_bin.extend_from_slice(b"/.local/bin:");
    assert!(
        output
            .stdout
            .windows(source_bin.len())
            .any(|part| part == source_bin)
    );
}

#[test]
fn native_suite_observes_pre_override_system_tmpdir() {
    let f = Fixture::new();
    f.suite(
        "core",
        "printf '%s\\n%s\\n' \"$DOT_TEST_SYSTEM_TMPDIR\" \"$TMPDIR\" >\"$HOME/observed\"; printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let output = f.run(&[]);
    success(&output);
    let observed = std::fs::read_to_string(f.home.join("observed")).unwrap();
    let mut lines = observed.lines();
    let system = lines.next().unwrap();
    let tmpdir = lines.next().unwrap();
    assert_eq!(system, f.scope.path().join("tmp").to_str().unwrap());
    assert_ne!(system, tmpdir);
    assert!(tmpdir.starts_with(system));
}

/// Caller variables that address the live session and must not reach a suite.
const SCRUBBED: [&str; 9] = [
    "TMUX",
    "TMUX_PANE",
    "VSCODE_IPC_HOOK_CLI",
    "VSCODE_GIT_IPC_HANDLE",
    "WEZTERM_UNIX_SOCKET",
    "WEZTERM_PANE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "SSH_AUTH_SOCK",
];

/// Fail-closed overrides and a kept terminal identity.
const OBSERVED: [&str; 4] = [
    "TMUX_TMPDIR",
    "BROWSER",
    "DBUS_SESSION_BUS_ADDRESS",
    "TERM_PROGRAM",
];

/// Which suite path `observe_session` launches through.
enum Launch {
    /// A client suite from `DOT_TEST_TESTS_DIR`.
    Client,
    /// The provider-owned `dot` suite, launched through its `tests/run`.
    Provider,
}

/// Run one suite that records `SCRUBBED` and `OBSERVED` as `NAME=value`
/// lines, or `NAME` alone when unset, under the given caller environment.
fn observe_session(launch: Launch, caller: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
    let f = Fixture::new();
    let body = format!(
        "for name in {}; do\n  if [[ -n ${{!name+x}} ]]; then printf '%s=%s\\n' \"$name\" \"${{!name}}\"; else printf '%s\\n' \"$name\"; fi\ndone >\"$HOME/observed\"\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
        [SCRUBBED.as_slice(), OBSERVED.as_slice()]
            .concat()
            .join(" ")
    );
    let mut command = match launch {
        Launch::Client => {
            f.suite("session", &body);
            f.command(&[])
        }
        Launch::Provider => {
            fixture::executable(&f.root.join("tests/run"), &body);
            let mut command = f.command(&["dot"]);
            command.env_remove("DOT_TEST_TESTS_DIR");
            command
        }
    };
    command.envs(caller.iter().copied());
    success(&fixture::finish(command.spawn().unwrap()));
    std::fs::read_to_string(f.home.join("observed"))
        .unwrap()
        .lines()
        .map(|line| match line.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (line.to_owned(), None),
        })
        .collect()
}

/// Assert every pointer is unset and every override holds its fail-closed value.
fn assert_fail_closed(observed: &BTreeMap<String, Option<String>>) {
    for name in SCRUBBED {
        assert_eq!(observed[name], None, "{name} reached the suite");
    }
    assert_eq!(observed["TMUX_TMPDIR"].as_deref(), Some("/dev/null"));
    assert_eq!(observed["BROWSER"].as_deref(), Some("false"));
    assert_eq!(
        observed["DBUS_SESSION_BUS_ADDRESS"].as_deref(),
        Some("disabled:")
    );
}

/// A caller session with every scrubbed pointer and override set to a fake.
/// Built from `SCRUBBED` so a name added there is always exercised.
fn fake_session() -> Vec<(&'static str, &'static str)> {
    let mut caller: Vec<_> = SCRUBBED
        .iter()
        .map(|name| (*name, "/nonexistent/live-session"))
        .collect();
    caller.extend([
        ("TMUX_TMPDIR", "/nonexistent/tmux-root"),
        ("BROWSER", "/nonexistent/browser"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus"),
    ]);
    caller
}

#[test]
fn native_suites_do_not_inherit_the_live_session() {
    assert_fail_closed(&observe_session(Launch::Client, &fake_session()));
}

#[test]
fn native_provider_suite_does_not_inherit_the_live_session() {
    assert_fail_closed(&observe_session(Launch::Provider, &fake_session()));
}

#[test]
fn native_suites_fail_closed_without_a_caller_session() {
    assert_fail_closed(&observe_session(Launch::Client, &[]));
}

#[test]
fn native_suites_keep_terminal_identity() {
    // TERM_PROGRAM names a terminal but addresses nothing; suites that branch
    // on it still see the caller's.
    let observed = observe_session(Launch::Client, &[("TERM_PROGRAM", "tmux")]);
    assert_eq!(observed["TERM_PROGRAM"].as_deref(), Some("tmux"));
}
