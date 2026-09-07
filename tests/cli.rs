//! Native end-to-end contracts for the `dot` CLI.
//!
//! Every scenario invokes the shipped Rust binary once and checks a
//! hand-written process contract plus independently observable state. Bash is
//! exercised only at the supported public user-hook boundary.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dot_test_support::TempDir;

static PROCESS_ENV: Mutex<()> = Mutex::new(());

const EXPECTED_HELP: &str = concat!(
    "usage: dot <command> [<args>]\n",
    "\n",
    "Commands:\n",
    "  update           Converge the base repository, overlays, hooks, and provider\n",
    "  pull             Alias for update\n",
    "  fetch            Fetch the base repository and active Git overlays\n",
    "  push             Push the base repository and active Git overlays\n",
    "  status           Show base and overlay status\n",
    "  diff             Show base and overlay differences\n",
    "  cron             Show the installed user crontab\n",
    "  doctor           Run core and configured extension health checks\n",
    "  test             Run configured tests; provider suite is opt-in\n",
    "  init             Initialize or resume a client dotfiles repository\n",
    "  help             Show this command summary\n",
    "\n",
    "Run `dot init --help` for initialization and recovery syntax.\n",
);

const EXPECTED_INIT_USAGE: &[u8] = b"usage: dot init [--branch BRANCH] [--yes] REPOSITORY_URL\n       dot init --status\n       dot init --rollback\n";

fn process_env_guard() -> MutexGuard<'static, ()> {
    PROCESS_ENV
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dot"))
}

#[test]
fn native_update_flag_capture_does_not_mutate_parent_environment() {
    let _env = process_env_guard();
    // Provider `none` keeps `--force` on the native path. The explicit
    // embedded runtime must retain its process and repository contracts when
    // it re-execs the real binary.
    let parent = native_parent_snapshot();
    let client = stage_repos_client();
    let runtime = runtime_for_force_update(&client);
    let args = [
        OsString::from("update"),
        OsString::from("--quiet"),
        OsString::from("--force"),
    ];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = dot::app::run(
        &runtime,
        &args,
        &mut dot::app::Streams::new(&mut stdout, &mut stderr),
    );

    assert_eq!(code, 0);
    assert_eq!(stdout, b"");
    assert_eq!(stderr, b"");
    assert_eq!(
        std::fs::read(client.home.join("tracked.txt")).unwrap(),
        b"v1\n"
    );
    assert_eq!(
        std::fs::read(client.overlay.join("tracked.txt")).unwrap(),
        b"v1\n"
    );
    assert_clean_checkout(&client, "alpha");
    assert_eq!(base_snapshot(&client), clean_checkout());
    assert!(!runtime.state_home().join("dot/update.lock").exists());
    assert_eq!(native_parent_snapshot(), parent);
}

#[test]
fn app_runs_concurrent_native_contexts_without_mutating_process_environment() {
    let _env = process_env_guard();
    // Two embedded Runtime calls must become separate `dot` processes. Their
    // fake PATH entries hold actual overlay workers at the same test seam;
    // differing TMPDIR/WSL values prove the child inherits its Runtime map,
    // never this test process's ambient environment.
    let parent = native_parent_snapshot();
    let first_client = stage_repos_client();
    let second_client = stage_repos_client();
    let first_state = first_client.scope.path().join("state");
    let second_state = second_client.scope.path().join("state");
    let barrier = first_client.scope.path().join("runtime-barrier");
    std::fs::create_dir_all(&barrier).expect("barrier dir");
    let first_tmp = first_client.scope.path().join("runtime-first-tmp");
    let second_tmp = second_client.scope.path().join("runtime-second-tmp");
    std::fs::create_dir_all(&first_tmp).expect("first tmp dir");
    std::fs::create_dir_all(&second_tmp).expect("second tmp dir");
    let first_bin = first_client.scope.path().join("runtime-first-bin");
    let second_bin = second_client.scope.path().join("runtime-second-bin");
    let first_trace = first_client.scope.path().join("runtime-first.trace");
    let second_trace = second_client.scope.path().join("runtime-second.trace");
    install_runtime_shims(&first_bin);
    install_runtime_shims(&second_bin);
    let first = runtime_for_native_update_with_process(
        &first_client,
        &first_state,
        &first_bin,
        &first_tmp,
        "first",
        Some("first-wsl"),
        &barrier,
        &first_trace,
    );
    let second = runtime_for_native_update_with_process(
        &second_client,
        &second_state,
        &second_bin,
        &second_tmp,
        "second",
        None,
        &barrier,
        &second_trace,
    );

    let run = |runtime: dot::app::Runtime| {
        thread::spawn(move || {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = dot::app::run(
                &runtime,
                &[OsString::from("update")],
                &mut dot::app::Streams::new(&mut stdout, &mut stderr),
            );
            (runtime, code, stdout, stderr)
        })
    };
    let first = run(first);
    let second = run(second);
    let ready = wait_for_runtime_workers(&barrier, &["first", "second"]);
    let scratch = [(&first_tmp, "first"), (&second_tmp, "second")]
        .into_iter()
        .all(|(root, _name)| {
            std::fs::read_dir(root).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().starts_with("dot."))
            })
        });
    std::fs::write(barrier.join("release"), b"release\n").expect("release workers");
    let first = first.join().expect("first native invocation");
    let second = second.join().expect("second native invocation");

    assert!(ready, "embedded Runtime children missed the Git barrier");
    assert!(
        scratch,
        "fleet scratch did not use both Runtime TMPDIR values"
    );
    for (name, trace, tmp, wsl) in [
        ("first", &first_trace, &first_tmp, "first-wsl"),
        ("second", &second_trace, &second_tmp, ""),
    ] {
        let trace = std::fs::read_to_string(trace).expect("runtime trace");
        assert!(
            trace.contains(&format!("{name}|git|{}|{wsl}", tmp.display())),
            "{name} git context: {trace}"
        );
        assert!(
            trace.contains(&format!("{name}|uname|{}|{wsl}", tmp.display())),
            "{name} uname context: {trace}"
        );
        assert!(
            trace.contains(&format!("{name}|mv|{}|{wsl}", tmp.display())),
            "{name} mv context: {trace}"
        );
    }

    for (name, (runtime, code, stdout, stderr)) in [("first", first), ("second", second)] {
        assert_eq!(
            code,
            0,
            "{name} stderr: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            !stdout.is_empty(),
            "{name} native update produced no stage output"
        );
        assert!(
            stderr.is_empty(),
            "{name} stderr: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            runtime.state_home().join("dot").is_dir(),
            "{name} state root"
        );
        assert!(
            !runtime.state_home().join("dot/update.lock").exists(),
            "{name} released its own update lock"
        );
    }
    assert_eq!(native_parent_snapshot(), parent);
}

#[test]
fn embedded_runtime_reports_unresolvable_executable() {
    let env = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let cwd = std::env::current_dir().expect("test cwd");
    let runtime = embedded_runtime(&env, &cwd, Path::new("/nonexistent/dot-runtime-child"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = dot::app::run(
        &runtime,
        &[OsString::from("help")],
        &mut dot::app::Streams::new(&mut stdout, &mut stderr),
    );
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert_eq!(
        stderr,
        b"dot: cannot re-exec runtime executable: /nonexistent/dot-runtime-child\n"
    );
}

#[test]
fn embedded_runtime_requires_explicit_executable() {
    let env = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let cwd = std::env::current_dir().expect("test cwd");
    let runtime = dot::app::Runtime::from_env(&env, &cwd).expect("embedded runtime");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = dot::app::run(
        &runtime,
        &[OsString::from("help")],
        &mut dot::app::Streams::new(&mut stdout, &mut stderr),
    );
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert_eq!(stderr, b"dot: embedded runtime requires an executable\n");
}

#[test]
fn embedded_executable_rejects_relative_path() {
    let error = dot::app::RuntimeExecutable::new(PathBuf::from("dot"))
        .expect_err("relative executable must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "dot runtime executable must be an absolute path"
    );
}

#[test]
fn help_constant_has_the_public_byte_contract() {
    assert_eq!(dot::cli::HELP, EXPECTED_HELP);
}

#[test]
fn binary_help_has_the_public_byte_contract() {
    let output = bin().arg("help").output().expect("run dot help");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        EXPECTED_HELP
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn binary_default_command_is_help() {
    let output = bin().output().expect("run dot");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        EXPECTED_HELP
    );
}

#[test]
fn binary_version_shape() {
    for flag in ["version", "--version"] {
        let output = bin().arg(flag).output().expect("run dot version");
        assert!(output.status.success(), "flag: {flag}");
        let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
        assert!(stdout.starts_with("dot commit "), "flag {flag}: {stdout}");
        assert!(
            stdout.ends_with(" (config 1; extensions 1; library 1)\n"),
            "flag {flag}: {stdout}"
        );
        assert!(output.stderr.is_empty(), "flag: {flag}");
    }
}

#[test]
fn binary_version_is_stable_across_aliases() {
    let version = bin().arg("version").output().expect("run dot version");
    let flag = bin().arg("--version").output().expect("run dot --version");
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(flag.status.code(), Some(0));
    assert_eq!(version.stdout, flag.stdout);
    assert!(version.stderr.is_empty());
    assert!(flag.stderr.is_empty());
}

#[test]
fn binary_unknown_command_has_the_public_error_contract() {
    let output = bin()
        .arg("frobnicate")
        .output()
        .expect("run dot frobnicate");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("stderr UTF-8"),
        "dot: unknown command: frobnicate\n"
    );
}

#[test]
fn binary_help_flags_have_the_public_byte_contract() {
    for flag in ["-h", "--help"] {
        let output = bin().arg(flag).output().expect("run dot flag");
        assert!(output.status.success(), "flag: {flag}");
        assert_eq!(
            String::from_utf8(output.stdout).expect("stdout UTF-8"),
            EXPECTED_HELP,
            "flag: {flag}"
        );
        assert!(output.stderr.is_empty(), "flag: {flag}");
    }
}

#[test]
fn update_passes_flag_exports_to_child_without_mutating_parent() {
    let _env = process_env_guard();
    // The parsed flag values reach the native engine, which then runs for real
    // — exit `0` on the empty-HOME fixture, never the interim
    // diagnostic. The same exports must not leak back into this test
    // process, which may construct another runtime immediately.
    use dot::cli::run;
    use std::ffi::OsString;
    let keys = [
        "DOT_QUIET",
        "SHDEPS_QUIET",
        "DOT_FORCE",
        "SHDEPS_FORCE",
        "DOT_VERBOSE",
        "SHDEPS_LOG_LEVEL",
        "DOT_OVERLAY_LINKS_FROZEN",
        "HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "DOT_SOURCE_ROOT",
        "DOT_BASH",
        "DOT_UPDATE_LOCK_TOKEN",
    ];
    let saved: Vec<(String, Option<OsString>)> = keys
        .iter()
        .map(|key| (key.to_string(), std::env::var_os(key)))
        .collect();
    let restore = || {
        // `unsafe` in edition 2024; the case is the only writer of
        // these keys while it runs, and it restores entry state.
        unsafe {
            for (key, value) in &saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    };
    let cases = [
        (&["update", "--cron"][..], true),
        (&["pull", "-f", "--verbose"][..], false),
        (&["update", "--quiet", "-x"][..], true),
    ];
    for (argv, quiet) in cases {
        let home = TempDir::new("cli-update-home").expect("isolated home");
        let state = TempDir::new("cli-update-state").expect("isolated state");
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            std::env::set_var("DOT_OVERLAY_LINKS_FROZEN", "1");
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_STATE_HOME", state.path());
            std::env::set_var("XDG_CONFIG_HOME", "");
            std::env::set_var("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"));
        }
        let owned: Vec<OsString> = argv.iter().map(OsString::from).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let before: Vec<(&str, Option<OsString>)> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        let code = run(owned, &mut out, &mut err);
        let after: Vec<(&str, Option<OsString>)> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        restore();
        // An empty HOME has no base repo and nothing to converge, so the
        // command reports success without an interim diagnostic.
        assert_eq!(code, 0, "argv: {argv:?}");
        assert!(err.is_empty(), "argv: {argv:?}");
        assert!(
            !out.windows(19).any(|w| w == b"not yet implemented"),
            "argv: {argv:?}"
        );
        if quiet {
            assert!(out.is_empty(), "argv: {argv:?}");
        } else {
            assert!(
                out.windows(17).any(|w| w == b"Reload your shell"),
                "argv: {argv:?}"
            );
        }
        assert_eq!(after, before, "argv: {argv:?} leaked command environment");
    }
    restore();
}

/// Run the binary with an isolated home, state directory, environment, and
/// working directory.
fn isolated_run(
    home: &TempDir,
    state: &TempDir,
    argv: &[&str],
    extra: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    init_env(&mut cmd, home, state);
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run bin/dot")
}

/// Set fixture trust modes explicitly instead of depending on ambient umask.
#[cfg(unix)]
fn seal(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("seal fixture");
}

#[test]
fn doctor_empty_home_reports_the_missing_client() {
    // No client checkout: the base-repo check fails, so doctor
    // reports the failure rows and exits 1 — nothing to stage.
    let home = TempDir::new("cli-doctor-empty").expect("fixture home");
    let state = TempDir::new("cli-doctor-empty-state").expect("fixture state");
    let output = isolated_run(&home, &state, &["doctor"], &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor must fail without a client: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("client repository is missing"),
        "doctor names the missing client: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(output.stderr.is_empty(), "doctor is silent on stderr");
}

/// Initialized file:// client for the doctor pass/extension rows.
fn stage_doctor_client() -> (TempDir, TempDir, TempDir) {
    let scope = TempDir::new("cli-doctor-origin").expect("origin scope");
    let (origin, _seed, _branch) = seed_bare_origin(scope.path(), "dotfiles");
    let home = TempDir::new("cli-doctor-client").expect("fixture home");
    let state = TempDir::new("cli-doctor-client-state").expect("fixture state");
    let url = format!("file://{}", origin.display());
    let staged = isolated_run(&home, &state, &["init", "--yes", &url], &[]);
    assert_eq!(
        staged.status.code(),
        Some(0),
        "init stages the client: {}",
        String::from_utf8_lossy(&staged.stderr),
    );
    (scope, home, state)
}

#[test]
fn doctor_initialized_client_reports_no_failures() {
    // Healthy client, no extensions: warnings stay (the worktree
    // checkout is outside the managed locations), failures clear,
    // exit 0.
    let (_scope, home, state) = stage_doctor_client();
    let output = isolated_run(&home, &state, &["doctor"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor passes on a healthy client: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("0 failed"),
        "doctor reports zero failures: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        output.stderr.is_empty(),
        "passing doctor is silent on stderr"
    );
}

/// Home whose overlay resolution fails: the dispatcher prints the
/// resolve warning on stderr, then doctor still runs (`|| true`)
/// while test refuses (`|| return 1`).
fn stage_bad_descriptor() -> (TempDir, TempDir) {
    let home = TempDir::new("cli-bad-desc").expect("fixture home");
    let state = TempDir::new("cli-bad-desc-state").expect("fixture state");
    let confd = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&confd).expect("overlay conf dir");
    std::fs::write(home.path().join(".config/dot/config"), b"version=1\n").expect("config");
    std::fs::write(confd.join("90-bad.conf"), b"url=x\nsync=hg\n").expect("bad descriptor");
    (home, state)
}

#[test]
fn doctor_resolve_failure_reports_the_warning_and_continues() {
    let (home, state) = stage_bad_descriptor();
    let output = isolated_run(&home, &state, &["doctor"], &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor still reports its checks: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown sync value: hg"),
        "doctor prints the resolve warning: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("dot runtime"),
        "doctor still ran: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

#[test]
fn test_resolve_failure_stops_before_running_suites() {
    let (home, state) = stage_bad_descriptor();
    let output = isolated_run(&home, &state, &["test"], &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "test refuses without resolution: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stdout.is_empty(),
        "refused test prints nothing on stdout"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown sync value: hg"),
        "test prints the resolve warning: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// One failing doctor extension on an initialized client (0700 extension
/// directories, sealed scripts, explicit extension config). The worker failure marks
/// `status=1`, so doctor exits 1 after the core rows.
fn stage_doctor_extension(home: &TempDir) {
    let extd = home.path().join("extensions/doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config dir");
    std::fs::create_dir_all(&extd).expect("extension dir");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("extension config");
    std::fs::write(
        extd.join("20-failing.sh"),
        b"doctor() {\n  dot_doctor_fail 'expected extension failure' 'fixture failure'\n  return 1\n}\n",
    )
    .expect("failing extension");
    #[cfg(unix)]
    {
        seal(&home.path().join("extensions"), 0o700);
        seal(&extd, 0o700);
        seal(&extd.join("20-failing.sh"), 0o644);
    }
}

#[test]
fn doctor_extension_failure_is_aggregated() {
    let (_scope, home, state) = stage_doctor_client();
    stage_doctor_extension(&home);
    let output = isolated_run(&home, &state, &["doctor"], &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor aggregates the extension failure: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("expected extension failure"),
        "doctor carries the extension record: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

#[test]
fn test_help_has_the_public_byte_contract() {
    let home = TempDir::new("cli-test-help").expect("fixture home");
    let state = TempDir::new("cli-test-help-state").expect("fixture state");
    let output = isolated_run(&home, &state, &["test", "--help"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "usage: dot test [-s|--sequential] [-v|--verbose] [-j N|--jobs N] [--list] [name ...]\n\
         \n\
         Set DOT_TEST_INCLUDE_PROVIDER=1 to include the provider suite in an\n\
         unfiltered run. Select `dot` by name to run only the provider suite.\n",
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn test_unknown_option_has_the_public_error_contract() {
    let home = TempDir::new("cli-test-opt").expect("fixture home");
    let state = TempDir::new("cli-test-opt-state").expect("fixture state");
    let output = isolated_run(&home, &state, &["test", "--bogus"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"unknown option: --bogus\n");
}

#[test]
fn test_list_prints_discovered_suites() {
    // No suites configured: only the provider identity lists.
    let home = TempDir::new("cli-test-list").expect("fixture home");
    let state = TempDir::new("cli-test-list-state").expect("fixture state");
    let output = isolated_run(&home, &state, &["test", "-l"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"dot\n");
    assert!(output.stderr.is_empty());
}

/// Local `*-test` suites for the propagation rows: one passing
/// (`complete` record, exit 0), one failing (exit 3, no record).
/// Discovery runs through `DOT_TEST_TESTS_DIR`, so no client
/// checkout is needed; the scope lives in an exec-capable dir (the
/// system temp dir may be `noexec`) with sealed modes.
struct SuiteFixture {
    /// Temp scope owning every path below (held for the test).
    #[allow(dead_code)]
    scope: TempDir,
    dir: PathBuf,
}

fn stage_suites() -> SuiteFixture {
    let scope = TempDir::new_exec("cli-test-suites").expect("suite scope");
    let dir = scope.path().join("suites");
    std::fs::create_dir_all(&dir).expect("suite dir");
    std::fs::write(
        dir.join("pass-test"),
        b"#!/usr/bin/env bash\nprintf 'complete\\t0\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"\nexit 0\n",
    )
    .expect("pass suite");
    std::fs::write(dir.join("fail-test"), b"#!/usr/bin/env bash\nexit 3\n").expect("fail suite");
    #[cfg(unix)]
    {
        seal(&dir, 0o700);
        seal(&dir.join("pass-test"), 0o755);
        seal(&dir.join("fail-test"), 0o755);
    }
    SuiteFixture { scope, dir }
}

/// Scrub suite elapsed marks (` (0s)`, ` (12s)`) from one stream
/// before asserting a literal stream contract. The digit run keeps `(1 total)`-style clauses
/// (digits followed by a space, never `s)`) intact.
fn scrub_elapsed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let mut end = index + 2;
        while bytes.get(end).is_some_and(|byte| byte.is_ascii_digit()) {
            end += 1;
        }
        let mark = bytes.get(index) == Some(&b' ')
            && bytes.get(index + 1) == Some(&b'(')
            && end > index + 2
            && bytes.get(end) == Some(&b's')
            && bytes.get(end + 1) == Some(&b')');
        if mark {
            out.extend_from_slice(b" (Ns)");
            index = end + 2;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

#[test]
fn scrub_elapsed_keeps_counts_but_not_wall_clock() {
    assert_eq!(
        scrub_elapsed("  ✓ pass-test (0s)\nSuites: 1 passed (1 total)\n".as_bytes()),
        "  ✓ pass-test (Ns)\nSuites: 1 passed (1 total)\n".as_bytes(),
    );
    assert_eq!(
        scrub_elapsed("  ✗ fail-test (12s)\n".as_bytes()),
        "  ✗ fail-test (Ns)\n".as_bytes(),
    );
    assert_eq!(scrub_elapsed(b"no marks here\n"), b"no marks here\n");
}

#[test]
fn test_suite_pass_reports_success() {
    // `DOT_TEST_NO_COLOR=1` selects deterministic plain rendering.
    let fixture = stage_suites();
    let home = TempDir::new("cli-test-pass").expect("fixture home");
    let state = TempDir::new("cli-test-pass-state").expect("fixture state");
    let dir = fixture.dir.to_string_lossy().into_owned();
    let extra = [
        ("DOT_TEST_NO_COLOR", "1"),
        ("DOT_TEST_TESTS_DIR", dir.as_str()),
    ];
    let output = isolated_run(&home, &state, &["test", "-s", "pass"], &extra);
    assert_eq!(
        output.status.code(),
        Some(0),
        "passing suite: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let expected = "\ndot test\n\n\nRunning 1 test suites...\n\n── pass-test ──\n  ✓ pass-test (Ns)\n════════════════════════════════\n✓ Suites: 1 passed (1 total)\n════════════════════════════════\n";
    assert_eq!(scrub_elapsed(&output.stdout), expected.as_bytes());
    assert_eq!(output.stderr, b"");
}

#[test]
fn test_suite_failure_reports_failure() {
    let fixture = stage_suites();
    let home = TempDir::new("cli-test-fail").expect("fixture home");
    let state = TempDir::new("cli-test-fail-state").expect("fixture state");
    let dir = fixture.dir.to_string_lossy().into_owned();
    let extra = [
        ("DOT_TEST_NO_COLOR", "1"),
        ("DOT_TEST_TESTS_DIR", dir.as_str()),
    ];
    let output = isolated_run(&home, &state, &["test", "-s", "fail"], &extra);
    assert_eq!(
        output.status.code(),
        Some(1),
        "failing suite: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let expected = "\ndot test\n\n\nRunning 1 test suites...\n\n── fail-test ──\n  ✗ fail-test (Ns)\n════════════════════════════════\n✗ Suites: 0 passed, 1 failed (1 total)\n════════════════════════════════\nFailed: fail-test\n";
    assert_eq!(scrub_elapsed(&output.stdout), expected.as_bytes());
    assert_eq!(output.stderr, b"");
}

#[test]
fn binary_doctor_test_wired_past_interim() {
    // Slice 83: the interim set is empty — every `Command` variant
    // has a dedicated arm in `run`, so no known command may report
    // "not yet implemented" (routing finality is pinned by
    // the dispatch tests and the end-to-end rows above). This smoke asserts the diagnostic is gone
    // on the cheapest deterministic rows.
    let home = TempDir::new("cli-wired").expect("fixture home");
    let state = TempDir::new("cli-wired-state").expect("fixture state");
    for argv in [&["test", "--help"][..], &["test", "--bogus"][..]] {
        let output = isolated_run(&home, &state, argv, &[]);
        let combined = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
        assert!(
            !combined.windows(19).any(|w| w == b"not yet implemented"),
            "argv: {argv:?}",
        );
    }
    let output = isolated_run(&home, &state, &["doctor"], &[]);
    let combined = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
    assert!(
        !combined.windows(19).any(|w| w == b"not yet implemented"),
        "doctor is wired",
    );
}

/// `dot init` under a controlled client: a cleared environment plus a
/// temporary home/state pair, so rows never touch the developer's own
/// checkout, provider state, or ambient variables.
fn init_env(cmd: &mut Command, home: &TempDir, state: &TempDir) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    // One `.env` per variable (never `.envs`).
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", state.path())
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(home.path());
    isolate_git(cmd);
}

/// The Rust binary's `init` with a controlled client.
fn init_bin(home: &TempDir, state: &TempDir) -> Command {
    let mut cmd = bin();
    init_env(&mut cmd, home, state);
    cmd
}

#[test]
fn binary_init_help_has_the_public_byte_contract() {
    let home = TempDir::new("cli-init-help").expect("test home");
    let state = TempDir::new("cli-init-help-state").expect("test state");
    assert_eq!(dot::init_client_adopt::usage(), EXPECTED_INIT_USAGE);
    for argv in [vec!["init", "--help"], vec!["init", "-h"]] {
        let output = init_bin(&home, &state)
            .args(argv)
            .output()
            .expect("init help");
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, EXPECTED_INIT_USAGE);
        assert!(output.stderr.is_empty());
    }
    let rust = init_bin(&home, &state)
        .args(["init", "--help"])
        .output()
        .expect("run dot init --help");
    assert_eq!(rust.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(rust.stdout).expect("stdout UTF-8"),
        std::str::from_utf8(EXPECTED_INIT_USAGE).expect("literal init usage UTF-8")
    );
    assert!(rust.stderr.is_empty());
}

#[test]
fn binary_init_early_paths_have_expected_statuses() {
    // Parsing, mode gates, the provider gate, and the resolvable
    // failures: none reach convergence (`--bogus` exits `1`, while
    // incomplete usage exits `2`).
    let home = TempDir::new("cli-init-early").expect("test home");
    let state = TempDir::new("cli-init-early-state").expect("test state");
    for (argv, code) in [
        (vec!["init", "--bogus"], 1),
        (vec!["init"], 2),
        (vec!["init", "--branch"], 2),
        (vec!["init", "--status", "some-origin"], 2),
        (vec!["init", "--status"], 0),
        (vec!["init", "--branch", "main", "notaurl"], 1),
        (vec!["init", "--branch", "bad..name", "notaurl"], 1),
    ] {
        let output = init_bin(&home, &state)
            .args(&argv)
            .output()
            .expect("init row");
        assert_eq!(output.status.code(), Some(code), "argv: {argv:?}");
    }
}

#[test]
fn binary_init_early_codes_have_expected_streams() {
    let home = TempDir::new("cli-init-codes").expect("fixture home");
    let state = TempDir::new("cli-init-codes-state").expect("fixture state");
    let cases: &[(&[&str], i32, &[u8])] = &[
        (
            &["init", "--bogus"],
            1,
            b"dot init: unknown option: --bogus\n",
        ),
        (&["init", "--branch"], 2, b""),
        (&["init", "--status"], 0, b""),
    ];
    for (argv, code, stderr) in cases {
        let rust = init_bin(&home, &state)
            .args(*argv)
            .output()
            .expect("run dot init");
        assert_eq!(rust.status.code(), Some(*code), "argv: {argv:?}");
        assert_eq!(rust.stderr, *stderr, "argv: {argv:?}");
    }
    let rust = init_bin(&home, &state)
        .args(["init", "--status"])
        .output()
        .expect("run dot init --status");
    assert_eq!(
        rust.stdout,
        b"initialization: not started\n".to_vec(),
        "status report"
    );
}

#[test]
fn binary_init_rollback_without_a_transaction_is_rejected() {
    let rust_home = TempDir::new("cli-init-rb").expect("rust home");
    let rust_state = TempDir::new("cli-init-rb-state").expect("rust state");
    let rust = init_bin(&rust_home, &rust_state)
        .args(["init", "--rollback"])
        .stdin(Stdio::null())
        .output()
        .expect("run dot init --rollback");
    assert_eq!(rust.status.code(), Some(1));
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: no recoverable transaction\n".to_vec()
    );
}

#[cfg(unix)]
fn poison_curl(scope: &Path) -> (OsString, PathBuf) {
    let poison_dir = scope.join("poison-path");
    let record = scope.join("provider-invoked");
    std::fs::create_dir_all(&poison_dir).expect("poison path");
    let executable = poison_dir.join("curl");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf invoked >'{}'\nexit 97\n",
            record.display()
        ),
    )
    .expect("poison curl");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("poison curl mode");
    let mut path = poison_dir.into_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    (path, record)
}

#[cfg(unix)]
#[test]
fn binary_init_fresh_converges_natively() {
    let scope = TempDir::new("cli-init-native-origin").expect("origin scope");
    let (origin, seed, branch) = seed_bare_origin(scope.path(), "dotfiles");
    std::fs::create_dir_all(seed.join(".config/dot")).expect("config parent");
    seed_advance(
        &seed,
        ".config/dot/config",
        b"version=1\ndependency_provider=shdeps\n",
    );
    let home = TempDir::new("cli-init-native-home").expect("home");
    let state = TempDir::new("cli-init-native-state").expect("state");
    let url = format!("file://{}", origin.display());
    let (path, poison_record) = poison_curl(scope.path());

    let output = init_bin(&home, &state)
        .args(["init", "--yes", "--branch", &branch, &url])
        .env("DOT_INIT_SKIP_PROVIDER", "1")
        .env("DOT_BASH", "/definitely/missing/fallback")
        .env("PATH", &path)
        .output()
        .expect("run native init");

    assert_eq!(
        output.status.code(),
        Some(0),
        "native init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("converge is not yet implemented"),
        "pending convergence escaped: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(state.path().join("dot/init/completed").is_file());
    assert!(!state.path().join("dot/init/transaction").exists());
    assert!(!poison_record.exists(), "Shdeps provider was not skipped");
    let first_home = semantic_tree(home.path(), true);
    let first_state = semantic_tree(state.path(), false);

    for argv in [
        vec!["init", "--yes", "--branch", &branch, &url],
        vec!["update"],
    ] {
        let repeated = init_bin(&home, &state)
            .args(&argv)
            .env("DOT_INIT_SKIP_PROVIDER", "1")
            .env("DOT_BASH", "/definitely/missing/fallback")
            .env("PATH", &path)
            .output()
            .expect("repeat native convergence");
        assert_eq!(
            repeated.status.code(),
            Some(0),
            "repeat {argv:?} failed: {}",
            String::from_utf8_lossy(&repeated.stderr)
        );
    }
    assert_eq!(semantic_tree(home.path(), true), first_home);
    assert_eq!(semantic_tree(state.path(), false), first_state);
    assert!(!poison_record.exists(), "repeat invoked Shdeps provider");
}

#[cfg(unix)]
#[test]
fn repo_fetch_discovers_the_base_from_native_init_identity() {
    let scope = TempDir::new("cli-fetch-after-init-origin").expect("origin scope");
    let (origin, seed, branch) = seed_bare_origin(scope.path(), "dotfiles");
    let home = TempDir::new("cli-fetch-after-init-home").expect("home");
    let state = TempDir::new("cli-fetch-after-init-state").expect("state");
    let url = format!("file://{}", origin.display());

    let initialized = init_bin(&home, &state)
        .args(["init", "--yes", "--branch", &branch, &url])
        .env("DOT_INIT_SKIP_PROVIDER", "1")
        .env("DOT_BASH", "/definitely/missing/fallback")
        .output()
        .expect("native init");
    assert_eq!(
        initialized.status.code(),
        Some(0),
        "native init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(state.path().join("dot/init/completed").is_file());

    seed_advance(&seed, "tracked.txt", b"origin-two\n");
    let expected = repos_git_line(&seed, &["rev-parse", "HEAD"]);
    let before = repos_prefix_line(
        &home.path().join(".dotfiles"),
        home.path(),
        &["rev-parse", &format!("refs/remotes/origin/{branch}")],
    );
    assert_ne!(before, expected, "fixture remote advanced");

    let fetched = init_bin(&home, &state)
        .arg("fetch")
        .output()
        .expect("native repository fetch");
    assert_eq!(
        fetched.status.code(),
        Some(0),
        "fetch failed: {}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_eq!(fetched.stdout, b"==> Fetching dotfiles...\n");
    assert_eq!(
        repos_prefix_line(
            &home.path().join(".dotfiles"),
            home.path(),
            &["rev-parse", &format!("refs/remotes/origin/{branch}")],
        ),
        expected
    );
    let fetch_head = home.path().join(".dotfiles/FETCH_HEAD");
    assert_eq!(
        std::fs::metadata(&fetch_head)
            .expect("FETCH_HEAD metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        std::fs::read(&fetch_head)
            .expect("FETCH_HEAD bytes")
            .starts_with(expected.as_bytes()),
        "FETCH_HEAD records the fetched generation"
    );
}

#[cfg(unix)]
#[test]
fn binary_init_reloads_the_configuration_it_just_cloned() {
    let scope = TempDir::new("cli-init-reload-origin").expect("origin scope");
    let (origin, seed, branch) = seed_bare_origin(scope.path(), "dotfiles");
    std::fs::create_dir_all(seed.join(".config/dot")).expect("config parent");
    seed_advance(
        &seed,
        ".config/dot/config",
        b"version=1\ndependency_provider=shdeps\n",
    );
    let home = TempDir::new("cli-init-reload-home").expect("home");
    let state = TempDir::new("cli-init-reload-state").expect("state");
    let url = format!("file://{}", origin.display());
    let (path, poison_record) = poison_curl(scope.path());

    let output = init_bin(&home, &state)
        .args(["init", "--yes", "--branch", &branch, &url])
        .env("DOT_BASH", "/definitely/missing/fallback")
        .env("PATH", path)
        .output()
        .expect("run native init with cloned config");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        poison_record.is_file(),
        "convergence retained the pre-clone provider=none configuration"
    );
    assert!(state.path().join("dot/init/transaction").is_dir());
    assert!(!state.path().join("dot/init/completed").exists());
}

#[test]
fn binary_init_holds_the_operation_lock_and_read_only_modes_skip_it() {
    let home = TempDir::new("cli-init-lock-home").expect("home");
    let state = TempDir::new("cli-init-lock-state").expect("state");
    let log = dot::log::Log::new(false, false);
    let mut warnings = Vec::new();
    let guard = dot::update_lock::acquire(state.path(), false, &log, None, &mut warnings)
        .expect("hold fixture lock");

    let blocked = init_bin(&home, &state)
        .args(["init", "--bogus"])
        .output()
        .expect("run locked init");
    assert_eq!(
        blocked.status.code(),
        Some(dot::update_lock::EXIT_LOCK_BUSY)
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("dot update already running"));

    for mode in ["--status", "--help", "-h"] {
        let probe = init_bin(&home, &state)
            .args(["init", mode])
            .output()
            .expect("run read-only init mode");
        assert_eq!(probe.status.code(), Some(0), "mode {mode}");
        assert!(
            !String::from_utf8_lossy(&probe.stderr).contains("already running"),
            "mode {mode} acquired the lock"
        );
    }
    guard.release(&log, &mut warnings);
}

#[test]
fn binary_init_adopts_and_converges_natively() {
    let scope = TempDir::new("cli-init-adopt-origin").expect("origin scope");
    let (origin, _seed, branch) = seed_bare_origin(scope.path(), "dotfiles");
    let home = TempDir::new("cli-init-adopt-home").expect("home");
    let state = TempDir::new("cli-init-adopt-state").expect("state");
    let url = format!("file://{}", origin.display());
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let clone = command
        .args(["clone", "-q", "--branch", &branch, &url])
        .arg(home.path())
        .status()
        .expect("clone adopt fixture");
    assert!(clone.success());

    let before = semantic_tree(home.path(), true);
    let output = init_bin(&home, &state)
        .args(["init", "--yes", "--branch", &branch, &url])
        .env("DOT_INIT_SKIP_PROVIDER", "1")
        .env("DOT_BASH", "/definitely/missing/fallback")
        .output()
        .expect("run native adopt");
    assert_eq!(
        output.status.code(),
        Some(0),
        "native adopt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after: Vec<_> = semantic_tree(home.path(), true)
        .into_iter()
        .filter(|(path, _)| path != ".cache/dotfiles/git-real")
        .collect();
    assert_eq!(after, before);
    assert!(state.path().join("dot/init/completed").is_file());
    assert!(!state.path().join("dot/init/transaction").exists());
}

#[test]
fn binary_init_adopts_legacy_separate_git_dir_natively() {
    let scope = TempDir::new("cli-init-adopt-separate-origin").expect("origin scope");
    let (origin, _seed, branch) = seed_bare_origin(scope.path(), "dotfiles");
    let home = TempDir::new("cli-init-adopt-separate-home").expect("home");
    let state = TempDir::new("cli-init-adopt-separate-state").expect("state");
    let url = format!("file://{}", origin.display());
    let git_dir = home.path().join(".dotfiles");
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let clone = command
        .args(["clone", "-q", "--bare", &url])
        .arg(&git_dir)
        .status()
        .expect("clone separate fixture");
    assert!(clone.success());
    std::fs::write(home.path().join("tracked.txt"), b"v1\n").expect("materialize worktree");

    let output = init_bin(&home, &state)
        .args(["init", "--yes", "--branch", &branch, &url])
        .env("DOT_INIT_SKIP_PROVIDER", "1")
        .output()
        .expect("run native separate adoption");
    assert_eq!(
        output.status.code(),
        Some(0),
        "native separate adoption failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(state.path().join("dot/init/completed").is_file());
    assert!(!state.path().join("dot/init/transaction").exists());
    assert!(git_dir.is_dir());
}

#[test]
fn binary_init_resumes_live_transaction_and_converges_natively() {
    let scope = TempDir::new("cli-init-resume-origin").expect("origin scope");
    let (origin, _seed, branch) = seed_bare_origin(scope.path(), "dotfiles");
    let home = TempDir::new("cli-init-resume-home").expect("home");
    let state = TempDir::new("cli-init-resume-state").expect("state");
    let url = format!("file://{}", origin.display());
    let argv = ["init", "--yes", "--branch", &branch, &url];

    let initial = init_bin(&home, &state)
        .args(argv)
        .env("DOT_INIT_SKIP_PROVIDER", "1")
        .env("DOT_BASH", "/definitely/missing/fallback")
        .output()
        .expect("initial native init");
    assert_eq!(initial.status.code(), Some(0));
    let completed = state.path().join("dot/init/completed");
    let transaction = state.path().join("dot/init/transaction");
    std::fs::create_dir_all(&transaction).expect("transaction dir");
    std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o700))
        .expect("transaction permissions");
    let complete_record = std::fs::read(&completed).expect("completion record");
    let checkout_record = complete_record
        .windows(b"phase=complete\n".len())
        .position(|window| window == b"phase=complete\n")
        .map(|at| {
            let mut bytes = complete_record.clone();
            bytes.splice(
                at..at + b"phase=complete\n".len(),
                b"phase=checkout\n".iter().copied(),
            );
            bytes
        })
        .expect("complete phase");
    let record = transaction.join("record");
    std::fs::write(&record, checkout_record).expect("checkout record");
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600))
        .expect("record permissions");
    std::fs::remove_file(&completed).expect("remove completed marker");
    let before = semantic_tree(home.path(), true);

    let resumed = init_bin(&home, &state)
        .args(argv)
        .env("DOT_INIT_SKIP_PROVIDER", "1")
        .env("DOT_BASH", "/definitely/missing/fallback")
        .output()
        .expect("resume native init");
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "native resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(semantic_tree(home.path(), true), before);
    assert!(completed.is_file());
    assert!(!transaction.exists());
    assert!(
        std::fs::read(completed)
            .expect("completed record")
            .windows(b"phase=complete\n".len())
            .any(|window| window == b"phase=complete\n")
    );
}

/// Synthetic file:// client for fetch/push/status/diff scenarios: a separate
/// base (`$HOME/.dotfiles`, bare,
/// one file:// origin, worktree materialized at `$HOME`) plus one
/// git overlay with a matching descriptor, all under one TempDir
/// scope. Origins and seed clones live beside — never inside — the
/// temporary home, so `status` sees only worktree files.
struct ReposClient {
    /// Temp scope owning every path below (held for the test).
    #[allow(dead_code)]
    scope: TempDir,
    home: PathBuf,
    xdg: PathBuf,
    base_git_dir: PathBuf,
    base_origin: PathBuf,
    base_seed: PathBuf,
    base_branch: String,
    overlay: PathBuf,
    overlay_origin: PathBuf,
    overlay_seed: PathBuf,
    overlay_branch: String,
}

/// Explicit native-update runtime for one staged repository fixture.
///
/// The map is deliberately complete for the native engine's environment
/// inputs. In particular, it points the topology publication and the XDG
/// state/config roots at this fixture rather than at the test process.
fn native_update_env(client: &ReposClient, state: &Path) -> BTreeMap<OsString, OsString> {
    let path = std::env::var_os("PATH").expect("test PATH");
    let tmp = std::env::var_os("TMPDIR").unwrap_or_else(|| OsString::from("/tmp"));
    BTreeMap::from([
        (OsString::from("HOME"), client.home.as_os_str().to_owned()),
        (
            OsString::from("XDG_CONFIG_HOME"),
            client.xdg.as_os_str().to_owned(),
        ),
        (
            OsString::from("XDG_STATE_HOME"),
            state.as_os_str().to_owned(),
        ),
        (OsString::from("PATH"), path),
        (
            OsString::from("BASH"),
            dot_test_support::bash().as_os_str().to_owned(),
        ),
        (OsString::from("TMPDIR"), tmp),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("SHELL"), OsString::from("/bin/sh")),
        (OsString::from("DOT_GIT_REAL"), OsString::from("1")),
        (OsString::from("GIT_CONFIG_COUNT"), OsString::from("3")),
        (
            OsString::from("GIT_CONFIG_KEY_0"),
            OsString::from("core.hooksPath"),
        ),
        (
            OsString::from("GIT_CONFIG_VALUE_0"),
            OsString::from("/dev/null"),
        ),
        (
            OsString::from("GIT_CONFIG_KEY_1"),
            OsString::from("commit.gpgSign"),
        ),
        (
            OsString::from("GIT_CONFIG_VALUE_1"),
            OsString::from("false"),
        ),
        (
            OsString::from("GIT_CONFIG_KEY_2"),
            OsString::from("tag.gpgSign"),
        ),
        (
            OsString::from("GIT_CONFIG_VALUE_2"),
            OsString::from("false"),
        ),
        (
            OsString::from("DOT_SOURCE_ROOT"),
            OsString::from(env!("CARGO_MANIFEST_DIR")),
        ),
        (
            OsString::from("DOT_DEPENDENCY_PROVIDER"),
            OsString::from("none"),
        ),
        (
            OsString::from("DOT_UPDATE_RELOADS_SHELL"),
            OsString::from("0"),
        ),
    ])
}

/// Build an embedding Runtime with an explicit production-binary capability.
/// `app::run` never consults a process environment variable for this authority.
fn embedded_runtime(
    env: &BTreeMap<OsString, OsString>,
    cwd: &Path,
    executable: &Path,
) -> dot::app::Runtime {
    dot::app::Runtime::from_env(env, cwd)
        .expect("embedded runtime")
        .with_executable(
            dot::app::RuntimeExecutable::new(executable.to_path_buf())
                .expect("absolute runtime executable"),
        )
}

/// Use the default state location while keeping Git's launcher cache outside
/// the isolated home.
fn runtime_for_force_update(client: &ReposClient) -> dot::app::Runtime {
    let state = client.home.join(".local/state");
    let mut env = native_update_env(client, &state);
    env.remove(OsStr::new("XDG_STATE_HOME"));
    env.insert(
        OsString::from("XDG_CACHE_HOME"),
        client.scope.path().join("cache").into_os_string(),
    );
    embedded_runtime(&env, &client.home, Path::new(env!("CARGO_BIN_EXE_dot")))
}

/// A Runtime-only child environment with the explicit production-binary
/// capability required by the embedding boundary.
#[allow(clippy::too_many_arguments)]
fn runtime_for_native_update_with_process(
    client: &ReposClient,
    state: &Path,
    bin: &Path,
    tmp: &Path,
    marker: &str,
    wsl: Option<&str>,
    barrier: &Path,
    trace: &Path,
) -> dot::app::Runtime {
    let mut env = native_update_env(client, state);
    let parent_path = env.get(OsStr::new("PATH")).expect("native PATH");
    let mut entries = vec![bin.to_path_buf()];
    entries.extend(std::env::split_paths(parent_path));
    env.insert(
        OsString::from("PATH"),
        std::env::join_paths(entries).expect("shim PATH"),
    );
    env.insert(OsString::from("TMPDIR"), tmp.as_os_str().to_owned());
    env.insert(OsString::from("DOT_RUNTIME_MARKER"), OsString::from(marker));
    env.insert(
        OsString::from("DOT_RUNTIME_BARRIER"),
        barrier.as_os_str().to_owned(),
    );
    env.insert(
        OsString::from("DOT_RUNTIME_TRACE"),
        trace.as_os_str().to_owned(),
    );
    env.insert(
        OsString::from("DOT_RUNTIME_OVERLAY_PATH"),
        client.overlay.as_os_str().to_owned(),
    );
    for tool in ["git", "uname", "mv"] {
        env.insert(
            OsString::from(format!("DOT_RUNTIME_REAL_{}", tool.to_ascii_uppercase())),
            real_tool(tool).into_os_string(),
        );
    }
    match wsl {
        Some(value) => {
            env.insert(OsString::from("WSL_DISTRO_NAME"), OsString::from(value));
        }
        None => {
            env.remove(OsStr::new("WSL_DISTRO_NAME"));
        }
    }
    embedded_runtime(&env, &client.home, Path::new(env!("CARGO_BIN_EXE_dot")))
}

/// Test-only command recorder. It blocks only a real overlay Git command,
/// after fleet scratch is allocated; production code gets no test hook.
fn install_runtime_shims(bin: &Path) {
    std::fs::create_dir_all(bin).expect("shim bin dir");
    for tool in ["git", "uname", "mv"] {
        let variable = tool.to_ascii_uppercase();
        let script = format!(
            r#"#!/bin/sh
printf '%s|{tool}|%s|%s\n' "${{DOT_RUNTIME_MARKER-}}" "${{TMPDIR-}}" "${{WSL_DISTRO_NAME-}}" >> "${{DOT_RUNTIME_TRACE}}"
if [ '{tool}' = git ] && [ -n "${{DOT_RUNTIME_OVERLAY_PATH-}}" ]; then
    case "$*" in
        *"${{DOT_RUNTIME_OVERLAY_PATH}}"*fetch*--no-write-fetch-head*)
            : > "${{DOT_RUNTIME_BARRIER}}/${{DOT_RUNTIME_MARKER}}-ready"
            while [ ! -e "${{DOT_RUNTIME_BARRIER}}/release" ]; do sleep 0.01; done
            ;;
    esac
fi
exec "${{DOT_RUNTIME_REAL_{variable}}}" "$@"
"#,
        );
        let path = bin.join(tool);
        std::fs::write(&path, script).expect("write shim");
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("mark shim executable");
    }
}

fn real_tool(tool: &str) -> PathBuf {
    let launcher = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/bin").join(tool));
    std::env::split_paths(&std::env::var_os("PATH").expect("test PATH"))
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file() && Some(candidate) != launcher.as_ref())
        .expect("real native tool")
}

/// Keep fixtures independent of developer Git hooks and signing policy.
fn isolate_git(command: &mut Command) {
    command
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "commit.gpgSign")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "tag.gpgSign")
        .env("GIT_CONFIG_VALUE_2", "false");
}

fn wait_for_runtime_workers(barrier: &Path, markers: &[&str]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if markers
            .iter()
            .all(|marker| barrier.join(format!("{marker}-ready")).exists())
        {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Snapshot semantic user/state files while excluding repository internals and
/// obsolete bootstrap metadata that are outside the tested operation.
fn semantic_tree(root: &Path, home_tree: bool) -> Vec<(String, Vec<u8>)> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("semantic tree dir") {
            let entry = entry.expect("semantic tree entry");
            let path = entry.path();
            let kind = entry.file_type().expect("semantic tree type");
            let relative = path
                .strip_prefix(root)
                .expect("semantic child")
                .to_string_lossy()
                .into_owned();
            let bootstrap = if home_tree {
                relative == ".local/state/dot/bash-v1"
            } else {
                relative == "dot/bash-v1"
            };
            let checkout = home_tree
                && path.file_name().is_some_and(|name| {
                    name == ".git" || name == ".dotfiles" || name == ".dot-backup"
                });
            if kind.is_dir() {
                if !checkout {
                    stack.push(path);
                }
            } else if (kind.is_file() || kind.is_symlink()) && !checkout && !bootstrap {
                entries.push((relative, std::fs::read(path).unwrap_or_default()));
            }
        }
    }
    entries.sort();
    entries
}

/// Ambient values that an embedded invocation must not capture or change. The
/// test never writes them: preserving this snapshot proves explicit runtimes
/// are isolated even while they run concurrently.
fn native_parent_snapshot() -> BTreeMap<OsString, Option<OsString>> {
    [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "DOT_QUIET",
        "DOT_FORCE",
        "DOT_VERBOSE",
        "DOT_OVERLAY_LINKS_FROZEN",
        "DOT_BASE_TOPOLOGY",
        "DOT_CLIENT_GIT_DIR",
        "PREFIX",
        "PATH",
        "TMPDIR",
        "SHELL",
        "WSL_DISTRO_NAME",
    ]
    .into_iter()
    .map(|key| (OsString::from(key), std::env::var_os(key)))
    .collect()
}

/// Run `git -C dir args` silenced, asserting success. Fixed
/// author/committer dates keep fixture SHAs deterministic;
/// `DOT_GIT_REAL` bypasses any machine-local git launcher shim
fn repos_git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let status = command
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn fixture git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

/// Run `git --git-dir=<git_dir> --work-tree=<work> args` silenced
/// (separate-topology base fixtures), asserting success.
fn repos_git_prefix(git_dir: &Path, work: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let status = command
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg(format!("--work-tree={}", work.display()))
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn fixture prefix git");
    assert!(
        status.success(),
        "prefix git {args:?} in {}",
        git_dir.display()
    );
}

/// Capture one `git -C dir args` stdout line, trimmed.
fn repos_git_line(dir: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let output = command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn fixture git");
    assert!(output.status.success(), "git {args:?} in {}", dir.display());
    String::from_utf8(output.stdout)
        .expect("git line UTF-8")
        .trim_end_matches('\n')
        .to_string()
}

/// Seed a bare file:// origin with one commit via a scratch clone.
/// Returns the origin path and its branch name (queried, never
/// assumed: the default branch depends on the machine git).
fn seed_bare_origin(scope: &Path, name: &str) -> (PathBuf, PathBuf, String) {
    let origin = scope.join(format!("{name}.git"));
    std::fs::create_dir_all(&origin).expect("origin dir");
    repos_git(&origin, &["init", "--bare", "-q"]);
    let seed = scope.join(format!("{name}-seed"));
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let status = command
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&seed)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone seed");
    assert!(status.success(), "clone seed {}", seed.display());
    std::fs::write(seed.join("tracked.txt"), b"v1\n").expect("seed file");
    repos_git(
        &seed,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    repos_git(
        &seed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "seed",
        ],
    );
    repos_git(&seed, &["push", "-q", "origin", "HEAD"]);
    let branch = repos_git_line(&seed, &["symbolic-ref", "--short", "HEAD"]);
    (origin, seed, branch)
}

/// Commit one more file revision on a seed clone and push it, so
/// the client falls behind its file:// origin.
fn seed_advance(seed: &Path, file: &str, body: &[u8]) {
    std::fs::write(seed.join(file), body).expect("advance file");
    repos_git(
        seed,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    repos_git(
        seed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "advance",
        ],
    );
    repos_git(seed, &["push", "-q", "origin", "HEAD"]);
}

/// Stage the full client: bare file:// origins, a separate
/// base cloned bare into `$HOME/.dotfiles` (single origin, valid
/// branch, worktree checked out at `$HOME` tracking its origin),
/// and one overlay clone with a matching descriptor under a temporary
/// XDG config home (kept outside `$HOME` so status stays clean).
fn stage_repos_client() -> ReposClient {
    let scope = TempDir::new("cli-repos").expect("repos scope");
    let home = scope.path().join("home");
    let xdg = scope.path().join("xdg");
    let origins = scope.path().join("origins");
    std::fs::create_dir_all(&home).expect("fixture home");
    let (base_origin, base_seed, base_branch) = seed_bare_origin(&origins, "dotfiles");
    let base_url = format!("file://{}", base_origin.display());
    let base_git_dir = home.join(".dotfiles");
    std::fs::create_dir_all(&base_git_dir).expect("base git dir");
    repos_git(&base_git_dir, &["init", "--bare", "-q"]);
    repos_git(&base_git_dir, &["config", "remote.origin.url", &base_url]);
    repos_git(
        &base_git_dir,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    repos_git_prefix(&base_git_dir, &home, &["fetch", "-q", "origin"]);
    repos_git_prefix(
        &base_git_dir,
        &home,
        &[
            "checkout",
            "-q",
            "-b",
            &base_branch,
            &format!("origin/{base_branch}"),
        ],
    );
    let (overlay_origin, overlay_seed, overlay_branch) = seed_bare_origin(&origins, "alpha");
    let overlay_url = format!("file://{}", overlay_origin.display());
    let confd = xdg.join("dot/overlays.d");
    std::fs::create_dir_all(&confd).expect("overlay conf dir");
    std::fs::write(confd.join("10-alpha.conf"), format!("url={overlay_url}\n"))
        .expect("overlay descriptor");
    let overlay = home.join(".dotfiles-alpha");
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let status = command
        .arg("clone")
        .arg("-q")
        .arg(&overlay_url)
        .arg(&overlay)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone overlay");
    assert!(status.success(), "clone overlay {}", overlay.display());
    ReposClient {
        scope,
        home,
        xdg,
        base_git_dir,
        base_origin,
        base_seed,
        base_branch,
        overlay,
        overlay_origin,
        overlay_seed,
        overlay_branch,
    }
}

/// Controlled environment for repo scenarios. The topology publication and
/// one `.env` entry per variable make every input explicit.
fn repos_env(cmd: &mut Command, client: &ReposClient) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let shim_cache = PathBuf::from(&tmpdir).join("dot-git-shim-cache");
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("BASH", dot_test_support::bash())
        .env("TMPDIR", &tmpdir)
        // Pin the reload hint independently of the machine's login shell.
        .env("SHELL", "/bin/sh")
        .env("HOME", &client.home)
        .env("XDG_CONFIG_HOME", &client.xdg)
        .env("XDG_CACHE_HOME", &shim_cache)
        .env("DOT_GIT_REAL", "1")
        // Status is read-only. Prevent Git from briefly creating index.lock
        // inside the worktree's separate .dotfiles directory, where another
        // status process can otherwise observe it as an untracked path.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(&client.home);
    isolate_git(cmd);
}

#[cfg(target_os = "macos")]
fn public_hook_command() -> Command {
    let mut cmd = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/lib/dot/public/test-timeout-v1"
    ));
    // The existing timeout supervisor owns, bounds, terminates, and reaps the
    // complete child session. Nested shell workers can therefore inherit that
    // group instead of asking macOS Bash to create another group after a
    // short-lived worker has crossed exec and racing with `setpgid`.
    cmd.arg("120s").arg(dot_test_support::bash());
    cmd
}

#[cfg(not(target_os = "macos"))]
#[test]
fn public_hook_command_uses_no_extra_runtime_dependency() {
    let cmd = public_hook_command();
    assert_eq!(cmd.get_program(), dot_test_support::bash());
    assert!(
        cmd.get_envs()
            .all(|(key, _)| key != "DOT_CLEANUP_INHERIT_GROUP")
    );
}

#[cfg(not(target_os = "macos"))]
fn public_hook_command() -> Command {
    Command::new(dot_test_support::bash())
}

#[cfg(target_os = "macos")]
fn inherit_supervised_group(cmd: &mut Command) {
    cmd.env("DOT_CLEANUP_INHERIT_GROUP", "1");
}

#[test]
fn repo_commands_disable_optional_git_locks() {
    let client = stage_repos_client();
    let mut command = bin();
    repos_env(&mut command, &client);
    let value = command
        .get_envs()
        .find(|(key, _)| *key == "GIT_OPTIONAL_LOCKS")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("0")));
}

#[test]
fn ambient_topology_cannot_authorize_an_uninitialized_checkout() {
    let scope = TempDir::new("repo-topology-injection").expect("fixture");
    let home = scope.path().join("home");
    let xdg = scope.path().join("xdg");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(xdg.join("dot/overlays.d")).expect("overlay config");
    std::fs::write(xdg.join("dot/overlays.d/bad.conf"), b"url=x\nsync=hg\n")
        .expect("invalid descriptor");
    repos_git(&home, &["init", "-q"]);
    std::fs::write(home.join("tracked"), b"content\n").expect("tracked file");
    repos_git(&home, &["add", "tracked"]);
    repos_git(&home, &["commit", "-qm", "seed"]);

    for command in ["fetch", "push", "status", "diff"] {
        let output = bin()
            .arg(command)
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
            .env("DOT_BASE_TOPOLOGY", "ordinary")
            .env("DOT_CLIENT_GIT_DIR", home.join(".git"))
            .current_dir(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run repo command");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{command} accepted injection"
        );
        assert_eq!(
            output.stderr, b"dot: ordinary HOME checkout requires a completed dot init identity\n",
            "{command} diagnostic"
        );
    }

    for command in ["cron", "frobnicate"] {
        let output = bin()
            .arg(command)
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
            .current_dir(&home)
            .output()
            .expect("run pre-dispatch command");
        assert_eq!(output.status.code(), Some(1), "{command} code");
        assert_eq!(
            output.stderr, b"dot: ordinary HOME checkout requires a completed dot init identity\n",
            "{command} identity precedence"
        );
    }
}

#[test]
fn init_help_validates_an_existing_identity_record() {
    let scope = TempDir::new("init-help-identity").expect("fixture");
    let home = scope.path().join("home");
    let state = scope.path().join("state");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(state.join("dot/init")).expect("init state");
    std::fs::write(state.join("dot/init/completed"), b"malformed\n").expect("record");
    let output = bin()
        .args(["init", "--help"])
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state)
        .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
        .current_dir(&home)
        .output()
        .expect("run init help");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"dot: malformed initialization identity record\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn public_hook_command_owns_one_inherited_process_group() {
    let mut cmd = public_hook_command();
    cmd.args([
        "-c",
        "parent=$(ps -o pgid= -p $$ | tr -d ' '); child=$(bash -c 'ps -o pgid= -p $$ | tr -d \" \"'); printf '%s|%s|%s|%s' \"$DOT_CLEANUP_INHERIT_GROUP\" \"$$\" \"$parent\" \"$child\"",
    ]);
    inherit_supervised_group(&mut cmd);
    let output = cmd.output().expect("run isolated hook process probe");
    assert!(
        output.status.success(),
        "probe status: {:?}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("probe UTF-8");
    let mut fields = text.split('|');
    assert_eq!(fields.next(), Some("1"), "inherit marker: {text}");
    let pid = fields.next().expect("shell pid");
    let parent = fields.next().expect("parent process group");
    let child = fields.next().expect("child process group");
    assert!(
        [pid, parent, child]
            .iter()
            .all(|value| value.parse::<u32>().is_ok_and(|value| value > 0)),
        "valid numeric process identities: {text}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(parent, pid, "hook shell must lead its group: {text}");
    assert_eq!(child, parent, "child process group: {text}");
    assert!(fields.next().is_none(), "unexpected probe fields: {text}");
}

/// Run one repository command in its isolated fixture.
fn repos_run(client: &ReposClient, argv: &[&str]) -> std::process::Output {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    repos_env(&mut cmd, client);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run repository command")
}

#[test]
fn repos_status_clean_reports_both_repositories() {
    let client = stage_repos_client();
    let output = repos_run(&client, &["status"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("==> dotfiles"),
        "status sees the base: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("==> alpha dotfiles"),
        "status sees the overlay: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn repos_status_uses_default_profile_from_loaded_config() {
    let client = stage_repos_client();
    let dot = client.xdg.join("dot");
    let profiles = dot.join("profiles.d");
    std::fs::create_dir_all(&profiles).expect("profile directory");
    std::fs::write(dot.join("config"), b"version=1\ndefault_profile=dev\n").expect("dot config");
    std::fs::write(profiles.join("base.conf"), b"version=1\noverlays=beta\n")
        .expect("base profile");
    std::fs::write(profiles.join("dev.conf"), b"version=1\noverlays=alpha\n").expect("dev profile");
    std::fs::write(
        dot.join("overlays.d/20-beta.conf"),
        format!("url=file://{}\n", client.overlay_origin.display()),
    )
    .expect("beta descriptor");

    let output = repos_run(&client, &["status"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("==> alpha dotfiles"),
        "configured dev profile must select alpha: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn repos_status_dirty_reports_base_and_overlay_changes() {
    let client = stage_repos_client();
    // Base: modified tracked file plus one untracked file.
    std::fs::write(client.home.join("tracked.txt"), b"v1-dirty\n").expect("dirty base");
    std::fs::write(client.home.join("new.txt"), b"untracked\n").expect("untracked base");
    // Overlay: modified tracked file.
    std::fs::write(client.overlay.join("tracked.txt"), b"v1-dirty\n").expect("dirty overlay");
    let output = repos_run(&client, &["status", "--short"]);
    assert_eq!(output.status.code(), Some(0), "status code");
    assert_eq!(
        output.stdout,
        b"==> dotfiles\n M tracked.txt\n?? .dotfiles-alpha/\n?? .dotfiles/\n?? new.txt\n\n==> alpha dotfiles\n M tracked.txt\n"
    );
    assert_eq!(
        output
            .stdout
            .windows(b" M tracked.txt\n".len())
            .filter(|row| *row == b" M tracked.txt\n")
            .count(),
        2
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn repos_status_reports_ahead_and_behind_branches() {
    let client = stage_repos_client();
    // Base moves ahead of its origin.
    std::fs::write(client.home.join("tracked.txt"), b"v1-ahead\n").expect("ahead file");
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "add",
            "tracked.txt",
        ],
    );
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "ahead",
        ],
    );
    // Overlay falls behind its origin (fetch refreshes the
    // remote-tracking ref so plain `status` reports behind).
    seed_advance(&client.overlay_seed, "tracked.txt", b"v1-origin\n");
    repos_git(&client.overlay, &["fetch", "-q", "origin"]);
    let output = repos_run(&client, &["status"]);
    assert_eq!(output.status.code(), Some(0), "status code");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("ahead"), "reports ahead: {text}");
    assert!(text.contains("behind"), "reports behind: {text}");
    assert!(
        !text.contains(".dotfiles/index"),
        "fixture must not track its own Git metadata: {text}",
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn repos_status_forwards_extra_arguments() {
    let client = stage_repos_client();
    std::fs::write(client.home.join("tracked.txt"), b"v1-dirty\n").expect("dirty base");
    let output = repos_run(&client, &["status", "--short", "--branch"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(has_bytes(&output.stdout, b"## "));
    assert!(has_bytes(&output.stdout, b" M tracked.txt\n"));
    assert!(output.stderr.is_empty());
}

#[test]
fn repos_diff_dirty_reports_base_and_overlay_hunks() {
    let client = stage_repos_client();
    // Base dirty (shows a hunks), overlay clean (header only).
    std::fs::write(client.home.join("tracked.txt"), b"v1\nv2\n").expect("dirty base");
    std::fs::write(client.overlay.join("tracked.txt"), b"v1\nv2\n").expect("dirty overlay");
    let output = repos_run(&client, &["diff"]);
    assert_eq!(output.status.code(), Some(0), "diff code");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("==> dotfiles"),
        "diff sees the base",
    );
    assert!(has_bytes(&output.stdout, b"==> alpha dotfiles\n"));
    assert_eq!(
        output
            .stdout
            .windows(b"+v2\n".len())
            .filter(|row| *row == b"+v2\n")
            .count(),
        2
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn repos_diff_clean_prints_headers_without_hunks() {
    let client = stage_repos_client();
    let output = repos_run(&client, &["diff"]);
    assert_eq!(output.status.code(), Some(0), "diff code");
    // No hunks anywhere: headers only, no git output.
    assert_eq!(output.stdout, b"==> dotfiles\n\n==> alpha dotfiles\n");
    assert!(output.stderr.is_empty(), "clean diff is silent on stderr");
}

/// Capture one separate-topology `git --git-dir/--work-tree` stdout
/// line, trimmed.
fn repos_prefix_line(git_dir: &Path, work: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let output = command
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg(format!("--work-tree={}", work.display()))
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn fixture prefix git");
    assert!(
        output.status.success(),
        "prefix git {args:?} in {}",
        git_dir.display()
    );
    String::from_utf8(output.stdout)
        .expect("git line UTF-8")
        .trim_end_matches('\n')
        .to_string()
}

#[test]
fn repos_fetch_updates_both_remote_tracking_refs() {
    let client = stage_repos_client();
    // Both origins advance while the client stays stale, so `fetch`
    // prints its update lines on both repos.
    seed_advance(&client.base_seed, "tracked.txt", b"v1-origin\n");
    seed_advance(&client.overlay_seed, "tracked.txt", b"v1-origin\n");
    let base_before = repos_prefix_line(
        &client.base_git_dir,
        &client.home,
        &[
            "rev-parse",
            &format!("refs/remotes/origin/{}", client.base_branch),
        ],
    );
    let overlay_before = repos_git_line(
        &client.overlay,
        &[
            "rev-parse",
            &format!("refs/remotes/origin/{}", client.overlay_branch),
        ],
    );
    let output = repos_run(&client, &["fetch"]);
    assert_eq!(output.status.code(), Some(0), "fetch code");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("From file://"),
        "fetch reports its remotes: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        output.stdout,
        b"==> Fetching dotfiles...\n==> Fetching alpha dotfiles...\n"
    );
    assert_ne!(
        repos_prefix_line(
            &client.base_git_dir,
            &client.home,
            &[
                "rev-parse",
                &format!("refs/remotes/origin/{}", client.base_branch)
            ]
        ),
        base_before
    );
    assert_ne!(
        repos_git_line(
            &client.overlay,
            &[
                "rev-parse",
                &format!("refs/remotes/origin/{}", client.overlay_branch)
            ]
        ),
        overlay_before
    );
}

#[test]
fn repos_push_updates_both_origins_and_tracking_refs() {
    let client = stage_repos_client();
    // Both repos move ahead of their origins.
    std::fs::write(client.home.join("tracked.txt"), b"v1-ahead\n").expect("ahead base");
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "ahead",
        ],
    );
    std::fs::write(client.overlay.join("tracked.txt"), b"v1-ahead\n").expect("ahead overlay");
    repos_git(
        &client.overlay,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    repos_git(
        &client.overlay,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "ahead",
        ],
    );
    // Save the origin and remote-tracking refs the push advances.
    let base_origin_before = repos_git_line(
        &client.base_origin,
        &["rev-parse", &format!("refs/heads/{}", client.base_branch)],
    );
    let base_tracking_before = repos_prefix_line(
        &client.base_git_dir,
        &client.home,
        &[
            "rev-parse",
            &format!("refs/remotes/origin/{}", client.base_branch),
        ],
    );
    let overlay_origin_before = repos_git_line(
        &client.overlay_origin,
        &[
            "rev-parse",
            &format!("refs/heads/{}", client.overlay_branch),
        ],
    );
    let overlay_tracking_before = repos_git_line(
        &client.overlay,
        &[
            "rev-parse",
            &format!("refs/remotes/origin/{}", client.overlay_branch),
        ],
    );
    let output = repos_run(&client, &["push"]);
    assert_eq!(output.status.code(), Some(0), "push code");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("To file://"),
        "push reports its remotes: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        output.stdout,
        b"==> Pushing dotfiles...\n==> Pushing alpha dotfiles...\n"
    );
    assert_ne!(
        repos_git_line(
            &client.base_origin,
            &["rev-parse", &format!("refs/heads/{}", client.base_branch)]
        ),
        base_origin_before
    );
    assert_ne!(
        repos_prefix_line(
            &client.base_git_dir,
            &client.home,
            &[
                "rev-parse",
                &format!("refs/remotes/origin/{}", client.base_branch)
            ]
        ),
        base_tracking_before
    );
    assert_ne!(
        repos_git_line(
            &client.overlay_origin,
            &[
                "rev-parse",
                &format!("refs/heads/{}", client.overlay_branch)
            ]
        ),
        overlay_origin_before
    );
    assert_ne!(
        repos_git_line(
            &client.overlay,
            &[
                "rev-parse",
                &format!("refs/remotes/origin/{}", client.overlay_branch)
            ]
        ),
        overlay_tracking_before
    );
}

#[test]
fn repos_push_rejects_non_fast_forward_base() {
    let client = stage_repos_client();
    // Diverge the base: a local commit plus an origin advance the
    // client never fetches, so the base push is rejected. The
    // dispatcher text ignores kernel status, but production runs
    // under `set -euo pipefail`, so the failing kernel exits the
    // process with its own code on both sides.
    std::fs::write(client.home.join("tracked.txt"), b"v1-local\n").expect("local base");
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "local",
        ],
    );
    seed_advance(&client.base_seed, "tracked.txt", b"v1-origin\n");
    let output = repos_run(&client, &["push"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "rejected base push exits 1: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rejected"),
        "reports the rejection: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, b"==> Pushing dotfiles...\n");
}

#[test]
fn repos_resolve_failure_reports_invalid_sync_value() {
    let client = stage_repos_client();
    // An invalid descriptor fails overlay resolution before any
    // repository command runs.
    std::fs::write(
        client.xdg.join("dot/overlays.d/90-bad.conf"),
        b"url=x\nsync=hg\n",
    )
    .expect("bad descriptor");
    let output = repos_run(&client, &["status"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "resolve-failure code: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        scrub_scope(&output.stderr, client.scope.path()),
        b"  warning: invalid overlay descriptor @SCOPE@/xdg/dot/overlays.d/90-bad.conf: unknown sync value: hg\n"
    );
}

#[test]
fn repos_status_without_topology_is_silent() {
    // No base repo and no descriptors: resolution succeeds empty
    // and every repository operation no-ops. No topology is exported.
    let scope = TempDir::new("cli-repos-empty").expect("empty scope");
    let home = scope.path().join("home");
    let xdg = scope.path().join("xdg");
    std::fs::create_dir_all(&home).expect("fixture home");
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut command = bin();
    command.arg("status");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("DOT_GIT_REAL", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_git(&mut command);
    let output = command.output().expect("run dot status");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
}

#[test]
fn binary_init_fresh_failures_do_not_publish_state() {
    let home = TempDir::new("cli-init-failures").expect("test home");
    let state = TempDir::new("cli-init-failures-state").expect("test state");
    for argv in [
        &["init", "--branch", "main", "file:///nonexistent-origin.git"][..],
        &["init", "--bogus"][..],
    ] {
        let output = init_bin(&home, &state)
            .args(argv)
            .output()
            .expect("init failure");
        assert!(!output.status.success(), "argv: {argv:?}");
        assert!(output.stdout.is_empty(), "argv: {argv:?}");
    }
}

/// The Rust binary over the same fixture with the native update driver.
fn repos_rust_native(client: &ReposClient, argv: &[&str]) -> std::process::Output {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    repos_env(&mut cmd, client);
    cmd.env("DOT_BASH", client.scope.path().join("absent-fallback"));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run dot binary")
}

/// A native-update client with an executable sentinel for any unsupported
/// fallback. Native scenarios must never execute it.
struct NativeUpdateFixture {
    client: ReposClient,
    fallback_sentinel: PathBuf,
}

impl NativeUpdateFixture {
    fn stage() -> Self {
        let client = stage_repos_client();
        let fallback_sentinel = client.scope.path().join("unexpected-fallback");
        let fixture = Self {
            client,
            fallback_sentinel,
        };
        fixture.reject_fallback();
        fixture
    }

    /// Install one trusted pre-sync entry point in the fixture's configured
    /// extension root. The native update must relay the hook's stdout and
    /// stderr to its process streams.
    fn with_pre_sync(self, script: &[u8]) -> Self {
        let extensions = self.client.home.join("extensions");
        let hooks = extensions.join("pre-sync.d");
        std::fs::create_dir_all(&hooks).expect("pre-sync directory");
        let hook = hooks.join("10-streams.sh");
        std::fs::write(&hook, script).expect("pre-sync hook");
        std::fs::create_dir_all(self.client.xdg.join("dot")).expect("config directory");
        std::fs::write(
            self.client.xdg.join("dot/config"),
            b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\n",
        )
        .expect("extension config");
        #[cfg(unix)]
        {
            for path in [&extensions, &hooks] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .expect("private extension directory");
            }
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))
                .expect("private pre-sync hook");
        }
        self
    }

    /// Install one trusted merge hook in the configured extension root. The
    /// hook runs through the worker's one-use active-overlay context during
    /// finalization, after the native link pass.
    fn with_merge(self, script: &[u8]) -> Self {
        self.with_merge_files(&[("10-config.sh", script)])
    }

    /// Install an ordered merge-hook fixture. Names are part of the merge
    /// scheduler contract, so callers supply them explicitly for barriers and
    /// deterministic replay cases.
    fn with_merge_files(self, files: &[(&str, &[u8])]) -> Self {
        let extensions = self.client.home.join("extensions");
        let hooks = extensions.join("merge-hooks.d");
        std::fs::create_dir_all(&hooks).expect("merge-hook directory");
        for (name, script) in files {
            std::fs::write(hooks.join(name), script).expect("merge hook");
        }
        std::fs::create_dir_all(self.client.xdg.join("dot")).expect("config directory");
        std::fs::write(
            self.client.xdg.join("dot/config"),
            b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\n",
        )
        .expect("extension config");
        #[cfg(unix)]
        {
            for path in [&extensions, &hooks] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .expect("private extension directory");
            }
            for (name, _) in files {
                std::fs::set_permissions(hooks.join(name), std::fs::Permissions::from_mode(0o700))
                    .expect("private merge hook");
            }
        }
        self
    }

    /// Add the smallest profile-aware policy to the otherwise clean native
    /// repository fixture. The existing `alpha` descriptor stays selected in
    /// phase one, so the fallback sentinel distinguishes an unsupported branch
    /// from a missing descriptor or repository error.
    fn with_base_profile(self) -> Self {
        let profiles = self.client.xdg.join("dot/profiles.d");
        std::fs::create_dir_all(&profiles).expect("profile directory");
        std::fs::write(profiles.join("base.conf"), b"version=1\noverlays=alpha\n")
            .expect("base profile");
        std::fs::create_dir_all(self.client.overlay_seed.join("home/.config/profile"))
            .expect("alpha profile tree");
        seed_advance(
            &self.client.overlay_seed,
            "home/.config/profile/value",
            b"alpha\n",
        );
        self
    }

    /// Create two matching selectors that disagree after the base-only pass.
    /// The update must fail natively and preserve the existing generation;
    /// reaching the sentinel instead would mean profiles escaped native execution.
    fn with_conflicting_profile_selectors(self) -> Self {
        let profiles = self.client.xdg.join("dot/profiles.d");
        let root = self.client.xdg.join("dot/profile-selectors.d");
        let local = self.client.xdg.join("dot/profile-selectors.local.d");
        std::fs::create_dir_all(&profiles).expect("profile directory");
        std::fs::create_dir_all(&root).expect("root selector directory");
        std::fs::create_dir_all(&local).expect("local selector directory");
        let user = dot::profiles::current_user().expect("current user");
        std::fs::write(profiles.join("base.conf"), b"version=1\noverlays=alpha\n")
            .expect("base profile");
        std::fs::write(profiles.join("dev.conf"), b"version=1\noverlays=alpha\n")
            .expect("dev profile");
        std::fs::write(
            root.join("base.conf"),
            format!("version=1\nuser={user}\nprofile=base\n"),
        )
        .expect("root selector");
        std::fs::write(
            local.join("dev.conf"),
            format!("version=1\nuser={user}\nprofile=dev\n"),
        )
        .expect("local selector");
        #[cfg(unix)]
        {
            for dir in [&root, &local] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .expect("private selector directory");
            }
            for file in [root.join("base.conf"), local.join("dev.conf")] {
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
                    .expect("private selector file");
            }
        }
        self
    }

    /// Stage a base-to-dev profile transition whose alpha hook leaves a
    /// durable marker. The second profile has no selected alpha descriptor,
    /// so a successful native update must retire alpha and remove it from the
    /// lifecycle ledger after linking the new generation.
    fn with_profile_retirement(self) -> Self {
        let profiles = self.client.xdg.join("dot/profiles.d");
        let config = self.client.xdg.join("dot/config");
        let extensions = self.client.home.join("extensions");
        std::fs::create_dir_all(&profiles).expect("profile directory");
        std::fs::create_dir_all(&extensions).expect("extensions directory");
        std::fs::write(profiles.join("base.conf"), b"version=1\noverlays=alpha\n")
            .expect("base profile");
        std::fs::write(profiles.join("dev.conf"), b"version=1\noverlays=beta\n")
            .expect("dev profile");
        std::fs::write(
            &config,
            b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndefault_profile=base\n",
        )
        .expect("extension config");
        std::fs::create_dir_all(self.client.overlay_seed.join("dot")).expect("alpha hook dir");
        seed_advance(
            &self.client.overlay_seed,
            "dot/profile-deactivate",
            b"deactivate() { printf '%s|%s' \"$DOT_RETIRING_OVERLAY\" \"${#OVERLAYS[@]}\" >\"$HOME/alpha-retired\"; }\n",
        );
        let origins = self.client.scope.path().join("origins");
        let (beta_origin, _beta_seed, _branch) = seed_bare_origin(&origins, "beta");
        std::fs::write(
            self.client.xdg.join("dot/overlays.d/20-beta.conf"),
            format!("url=file://{}\n", beta_origin.display()),
        )
        .expect("beta descriptor");
        #[cfg(unix)]
        std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o700))
            .expect("private extension directory");
        self
    }

    /// Publish profile policy only in the next base generation. The process
    /// starts with no policy at its XDG root, so this proves that the native
    /// driver reloads config and discovers an additions-only overlay phase
    /// *after* the base pull rather than relying on startup's stale config.
    fn with_base_discovered_profile_addition(self) -> Self {
        let base_dot = self.client.base_seed.join(".config/dot");
        std::fs::create_dir_all(base_dot.join("profiles.d")).expect("base profiles directory");
        std::fs::create_dir_all(base_dot.join("overlays.d")).expect("base overlays directory");
        std::fs::create_dir_all(self.client.base_seed.join(".config/profile"))
            .expect("base profile tree");
        std::fs::create_dir_all(self.client.base_seed.join("extensions/pre-sync.d"))
            .expect("base pre-sync directory");
        std::fs::create_dir_all(self.client.overlay_seed.join("home/.config/profile"))
            .expect("alpha profile tree");
        std::fs::write(
            self.client.overlay_seed.join("home/.config/profile/value"),
            b"alpha\n",
        )
        .expect("alpha profile value");
        seed_advance(
            &self.client.overlay_seed,
            "home/.config/profile/value",
            b"alpha\n",
        );

        let origins = self.client.scope.path().join("origins");
        let (beta_origin, beta_seed, _branch) = seed_bare_origin(&origins, "beta");
        std::fs::create_dir_all(beta_seed.join("home/.config/profile")).expect("beta profile tree");
        seed_advance(&beta_seed, "home/.config/profile/value", b"beta\n");

        seed_advance(&self.client.base_seed, ".config/profile/value", b"base\n");
        seed_advance(
            &self.client.base_seed,
            ".config/dot/config",
            b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndefault_profile=dev\n",
        );
        seed_advance(
            &self.client.base_seed,
            "extensions/pre-sync.d/10-after-base.sh",
            b"# shellcheck shell=bash\nprepare() { printf '%s' \"$DOT_PRE_SYNC_STAGE\" >\"$HOME/post-base-pre-sync\"; }\n",
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/profiles.d/base.conf",
            b"version=1\noverlays=alpha\n",
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/profiles.d/dev.conf",
            b"version=1\noverlays=beta\n",
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/overlays.d/10-alpha.conf",
            // Deliberately differs from the entry-generation descriptor.
            // The second phase must identify alpha by name, not re-pull it
            // because the refreshed record makes this source optional.
            format!(
                "url=file://{}\noptional=true\n",
                self.client.overlay_origin.display()
            )
            .as_bytes(),
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/overlays.d/20-beta.conf",
            format!("url=file://{}\n", beta_origin.display()).as_bytes(),
        );
        self
    }

    /// Publish a base profile that initially selects alpha. A private local
    /// selector agrees with that default; the caller can later publish a
    /// conflicting base selector to verify snapshot restoration after pull.
    fn with_base_profile_rollback(self) -> Self {
        let base_dot = self.client.base_seed.join(".config/dot");
        std::fs::create_dir_all(base_dot.join("profiles.d")).expect("base profiles directory");
        std::fs::create_dir_all(base_dot.join("overlays.d")).expect("base overlays directory");
        std::fs::create_dir_all(self.client.base_seed.join(".config/profile"))
            .expect("base profile tree");
        std::fs::create_dir_all(self.client.overlay_seed.join("home/.config/profile"))
            .expect("alpha profile tree");
        seed_advance(
            &self.client.overlay_seed,
            "home/.config/profile/value",
            b"overlay\n",
        );
        seed_advance(&self.client.base_seed, ".config/profile/value", b"base\n");
        seed_advance(
            &self.client.base_seed,
            ".config/dot/config",
            b"version=1\ndefault_profile=base\n",
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/profiles.d/base.conf",
            b"version=1\noverlays=alpha\n",
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/profiles.d/dev.conf",
            b"version=1\noverlays=alpha\n",
        );
        seed_advance(
            &self.client.base_seed,
            ".config/dot/overlays.d/10-alpha.conf",
            format!("url=file://{}\n", self.client.overlay_origin.display()).as_bytes(),
        );
        let selectors = self
            .client
            .home
            .join(".config/dot/profile-selectors.local.d");
        std::fs::create_dir_all(&selectors).expect("local selector directory");
        let selector = selectors.join("base.conf");
        let user = dot::profiles::current_user().expect("current user");
        std::fs::write(&selector, format!("version=1\nuser={user}\nprofile=base\n"))
            .expect("local base selector");
        #[cfg(unix)]
        {
            std::fs::set_permissions(&selectors, std::fs::Permissions::from_mode(0o700))
                .expect("private selector directory");
            std::fs::set_permissions(&selector, std::fs::Permissions::from_mode(0o600))
                .expect("private selector file");
        }
        self
    }

    fn reject_fallback(&self) {
        std::fs::write(
            &self.fallback_sentinel,
            b"#!/bin/sh\nprintf 'UNEXPECTED-FALLBACK\n' >&2\nexit 97\n",
        )
        .expect("write fallback sentinel");
        #[cfg(unix)]
        std::fs::set_permissions(
            &self.fallback_sentinel,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("mark fallback sentinel executable");
    }

    fn rust_dot(&self, argv: &[&str]) -> std::process::Output {
        self.rust_dot_with(argv, |_| {})
    }

    fn rust_dot_with(
        &self,
        argv: &[&str],
        configure: impl FnOnce(&mut Command),
    ) -> std::process::Output {
        let mut cmd = bin();
        for arg in argv {
            cmd.arg(arg);
        }
        repos_env(&mut cmd, &self.client);
        if self.fallback_sentinel.exists() {
            cmd.env("DOT_BASH", &self.fallback_sentinel);
        }
        configure(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().expect("run native update")
    }
}

fn assert_native_silent(output: &std::process::Output, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{label} status; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "{label} stdout");
    assert!(output.stderr.is_empty(), "{label} stderr");
}

#[test]
fn update_native_entry_edges_reject_fallback() {
    // Each row installs the unsupported-fallback sentinel, so success proves
    // the native entry handled the edge.
    let clean = NativeUpdateFixture::stage();
    clean.reject_fallback();
    assert_native_silent(&clean.rust_dot(&["update", "--cron"]), "clean cron");

    let mtime = NativeUpdateFixture::stage();
    let tracked = mtime.client.home.join("tracked.txt");
    let modified = std::fs::metadata(&tracked)
        .expect("stat tracked file")
        .modified()
        .expect("tracked mtime");
    std::fs::File::options()
        .write(true)
        .open(&tracked)
        .expect("open tracked file")
        .set_modified(modified + Duration::from_secs(2))
        .expect("bump tracked mtime");
    mtime.reject_fallback();
    assert_native_silent(&mtime.rust_dot(&["update", "--cron"]), "mtime cron");

    let unresolved = NativeUpdateFixture::stage();
    std::fs::write(unresolved.client.home.join("tracked.txt"), b"local edit\n")
        .expect("make unresolved edit");
    unresolved.reject_fallback();
    assert_native_silent(
        &unresolved.rust_dot(&["update", "--cron"]),
        "unresolved cron",
    );
    assert_eq!(
        std::fs::read(unresolved.client.home.join("tracked.txt")).expect("read unresolved edit"),
        b"local edit\n",
        "cron must leave a real local edit alone",
    );

    let force = NativeUpdateFixture::stage();
    force.reject_fallback();
    assert_native_silent(
        &force.rust_dot(&["update", "--quiet", "--force"]),
        "force with provider none",
    );

    let frozen = NativeUpdateFixture::stage();
    frozen.reject_fallback();
    assert_native_silent(
        &frozen.rust_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("DOT_OVERLAY_LINKS_FROZEN", "1");
        }),
        "stale frozen marker",
    );
}

#[test]
fn update_native_invalid_home_rejects_fallback() {
    for (label, home, state) in [
        ("missing absolute state", None, Some("state")),
        (
            "relative absolute state",
            Some("relative-home"),
            Some("state"),
        ),
        ("missing absent state", None, None),
        ("relative absent state", Some("relative-home"), None),
    ] {
        let fixture = NativeUpdateFixture::stage();
        let state_path = fixture.client.scope.path().join("invalid-home-state");
        fixture.reject_fallback();
        let output = fixture.rust_dot_with(&["update", "--quiet"], |cmd| {
            match home {
                Some(home) => {
                    cmd.env("HOME", home);
                }
                None => {
                    cmd.env_remove("HOME");
                }
            }
            // Keep startup's config lookup independently resolvable. The
            // update entry itself, not config loading, owns this error row.
            cmd.env("XDG_CONFIG_HOME", &fixture.client.xdg);
            match state {
                Some(_) => {
                    cmd.env("XDG_STATE_HOME", &state_path);
                }
                None => {
                    cmd.env_remove("XDG_STATE_HOME");
                }
            }
        });
        assert_eq!(output.status.code(), Some(1), "{label} status");
        assert!(output.stdout.is_empty(), "{label} stdout");
        assert!(output.stderr.is_empty(), "{label} stderr");
    }
}

#[test]
fn update_native_configured_pre_sync_hook_uses_the_hardened_worker() {
    // A configured hook must remain native and run only after the worker has
    // validated its one-use context. The sentinel makes a fallback
    // unmistakable while the marker proves the hook actually ran.
    let fixture = NativeUpdateFixture::stage();
    let extensions = fixture.client.home.join("extensions/pre-sync.d");
    std::fs::create_dir_all(&extensions).expect("pre-sync directory");
    let hook = extensions.join("10-hook.sh");
    std::fs::write(
        &hook,
        b"prepare() { printf prepared >\"$HOME/pre-sync-ran\"; }\n",
    )
    .expect("pre-sync hook");
    std::fs::create_dir_all(fixture.client.xdg.join("dot")).expect("config directory");
    std::fs::write(
        fixture.client.xdg.join("dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\n",
    )
    .expect("extension config");
    #[cfg(unix)]
    {
        std::fs::set_permissions(
            fixture.client.home.join("extensions"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("private extension root");
        std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o700))
            .expect("private pre-sync directory");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))
            .expect("executable pre-sync hook");
    }
    fixture.reject_fallback();
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_native_silent(&output, "configured pre-sync hook");
    assert_eq!(
        std::fs::read(fixture.client.home.join("pre-sync-ran")).expect("pre-sync marker"),
        b"prepared"
    );
}

#[test]
fn update_hook_runs_with_only_public_hook_assets() {
    fn copy_tree(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).expect("copy destination");
        for entry in std::fs::read_dir(source).expect("copy source") {
            let entry = entry.expect("source entry");
            let target = destination.join(entry.file_name());
            if entry.file_type().expect("source type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy public asset");
            }
        }
    }

    let fixture = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { [[ $# -eq 0 ]] || return 91; command_line=$(ps -o command= -p $$) || return; [[ $command_line != *file://* ]] || return 92; ! declare -F _ensure_repo_config >/dev/null || return 93; ! declare -F _overlay_record_link_target >/dev/null || return 94; printf prepared >\"$HOME/pre-sync-ran\"; }\n",
    );
    let release_root = fixture.client.home.join("native-release");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/dot/public"),
        &release_root.join("lib/dot/public"),
    );
    std::fs::write(release_root.join(".dot-install.json"), b"{}\n").expect("release metadata");

    fixture.reject_fallback();
    let output = fixture.rust_dot_with(&["update", "--quiet"], |command| {
        command.env("DOT_SOURCE_ROOT", &release_root);
    });
    assert_native_silent(&output, "release-only pre-sync hook");
    assert_eq!(
        std::fs::read(fixture.client.home.join("pre-sync-ran")).expect("pre-sync marker"),
        b"prepared"
    );
}

#[test]
fn update_native_pre_sync_success_relays_both_streams() {
    let fixture = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { printf 'pre-sync stdout\\n'; printf 'pre-sync stderr\\n' >&2; }\n",
    );
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(has_bytes(&output.stdout, b"pre-sync stdout\n"));
    assert!(has_bytes(&output.stderr, b"pre-sync stderr\n"));
}

#[test]
fn update_native_pre_sync_failure_relays_both_streams() {
    let fixture = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { printf 'pre-sync stdout\\n'; printf 'pre-sync stderr\\n' >&2; return 7; }\n",
    );
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(has_bytes(&output.stdout, b"pre-sync stdout\n"));
    assert!(has_bytes(&output.stderr, b"pre-sync stderr\n"));
}

#[test]
fn update_native_merge_hook_runs_with_a_large_job_limit() {
    let fixture = NativeUpdateFixture::stage()
        .with_merge(b"merge() { printf merged >\"$HOME/merge-hook-ran\"; }\n");
    let output = fixture.rust_dot_with(&["update", "--quiet"], |cmd| {
        cmd.env("DOT_MERGE_JOBS", "1000000000");
    });
    assert_native_silent(&output, "merge-hook success");
    assert_eq!(
        std::fs::read(fixture.client.home.join("merge-hook-ran")).expect("merge marker"),
        b"merged",
        "merge hook ran"
    );
}

#[test]
fn update_native_verbose_merge_replays_hook_output_in_declaration_order() {
    // A missing stage update or replay would make the quiet happy-path pass
    // while losing the human-visible hook result contract.
    let hooks: &[(&str, &[u8])] = &[
        (
            "10-alpha.sh",
            b"merge() { i=0; while [[ ! -e $HOME/beta-ready ]]; do (( i += 1 )); (( i < 200 )) || return 9; sleep 0.01; done; printf 'Alpha result\\nalpha detail\\n'; printf alpha >\"$HOME/alpha-done\"; }\n",
        ),
        (
            "11-beta.sh",
            b"merge() { printf ready >\"$HOME/beta-ready\"; i=0; while [[ ! -e $HOME/gamma-ready ]]; do (( i += 1 )); (( i < 200 )) || return 9; sleep 0.01; done; printf 'Beta result\\nbeta detail\\n'; printf beta >\"$HOME/beta-done\"; }\n",
        ),
        (
            "12-gamma.sh",
            b"merge() { printf ready >\"$HOME/gamma-ready\"; printf 'Gamma result\\ngamma detail\\n'; printf gamma >\"$HOME/gamma-done\"; }\n",
        ),
        (
            "20-barrier.serial.sh",
            b"merge() { [[ $(<\"$HOME/alpha-done\") == alpha && $(<\"$HOME/beta-done\") == beta && $(<\"$HOME/gamma-done\") == gamma ]] || return 9; printf 'Barrier result\\n'; printf barrier >\"$HOME/barrier-done\"; }\n",
        ),
        (
            "30-delta.sh",
            b"merge() { [[ $(<\"$HOME/barrier-done\") == barrier ]] || return 9; printf 'Delta result\\n'; printf delta >\"$HOME/delta-done\"; }\n",
        ),
    ];
    let fixture = NativeUpdateFixture::stage().with_merge_files(hooks);
    let output = fixture.rust_dot_with(&["update", "--verbose"], |cmd| {
        // Force a two-worker batch even on a one-core runner. The serial hook
        // proves the whole batch joined before its barrier starts.
        cmd.env("DOT_UPDATE_JOBS", "1");
        cmd.env("DOT_MERGE_JOBS", "2");
    });
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stderr, b"");
    let positions = [
        b"Alpha result".as_slice(),
        b"Beta result".as_slice(),
        b"Gamma result".as_slice(),
        b"Barrier result".as_slice(),
        b"Delta result".as_slice(),
    ]
    .map(|expected| {
        output
            .stdout
            .windows(expected.len())
            .position(|window| window == expected)
            .expect("literal merge output")
    });
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "parallel captures replay in declaration order before and after the serial barrier: {:?}",
        output.stdout
    );
    for detail in [
        b"\n    alpha detail\n".as_slice(),
        b"\n    beta detail\n".as_slice(),
        b"\n    gamma detail\n".as_slice(),
    ] {
        assert!(has_bytes(&output.stdout, detail));
    }
    for (name, expected) in [
        ("alpha-done", b"alpha".as_slice()),
        ("beta-done", b"beta".as_slice()),
        ("gamma-done", b"gamma".as_slice()),
        ("barrier-done", b"barrier".as_slice()),
        ("delta-done", b"delta".as_slice()),
    ] {
        assert_eq!(
            std::fs::read(fixture.client.home.join(name)).expect("merge marker"),
            expected,
            "{name}"
        );
    }
}

#[test]
fn update_native_verbose_merge_preserves_non_utf8_output() {
    let script = b"merge() { printf 'Binary '; printf '\\377'; printf 'A\\0B\\n'; }\n";
    let fixture = NativeUpdateFixture::stage().with_merge(script);
    let output = fixture.rust_dot(&["update", "--verbose"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stderr, b"");
    assert!(
        output.stdout.contains(&0xff),
        "native replay preserves the non-UTF-8 byte"
    );
    assert!(
        !output.stdout.contains(&0),
        "the public hook boundary discards NUL bytes while parsing output"
    );
    assert!(has_bytes(&output.stdout, b"Binary \xffAB"));
}

#[test]
fn update_native_merge_without_an_entry_point_fails() {
    // A helper-looking `*.sh` is discovered, but the worker writes a zero
    // merge record and exits nonzero. It must still fail the aggregate stage.
    let fixture = NativeUpdateFixture::stage().with_merge(b"helper() { :; }\n");
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
}

#[test]
fn update_native_unsafe_merge_hook_is_rejected() {
    // Discovery rejects an unsafe entry point before invoking it.
    let fixture = NativeUpdateFixture::stage().with_merge(b"merge() { :; }\n");
    std::fs::set_permissions(
        fixture
            .client
            .home
            .join("extensions/merge-hooks.d/10-config.sh"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert!(has_bytes(&output.stderr, b"unsafe merge hook"));
}

#[test]
fn update_native_failed_merge_hook_reports_its_capture() {
    // Failed hooks retain their ordered capture for a non-verbose update and
    // still make the aggregate Configs stage fail.
    let script = b"merge() { printf 'merge stdout\\n'; printf 'merge stderr\\n' >&2; return 7; }\n";
    let fixture = NativeUpdateFixture::stage().with_merge(script);
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert!(has_bytes(&output.stderr, b"10-config output:"));
    assert!(has_bytes(&output.stderr, b"merge stdout\n"));
    assert!(has_bytes(&output.stderr, b"merge stderr\n"));
}

#[test]
fn update_native_merge_hook_receives_active_overlay_context() {
    // The hook observes the one-use active-overlay context inside the public
    // worker boundary.
    let script = b"merge() {\n  [[ $REPLY_SET_KIND == active && $REPLY_STAGE == none && ${#OVERLAYS[@]} -eq 1 && ${OVERLAYS[0]} == alpha\\|* ]] || return 8\n  printf '%s:%s:%s' \"$REPLY_SET_KIND\" \"$REPLY_STAGE\" \"${OVERLAYS[0]%%|*}\" >\"$HOME/merge-context\"\n}\n";
    let fixture = NativeUpdateFixture::stage().with_merge(script);
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_native_silent(&output, "merge context");
    assert_eq!(
        std::fs::read(fixture.client.home.join("merge-context")).expect("merge context"),
        b"active:none:alpha"
    );
}

#[test]
fn update_native_profile_base_selection_rejects_fallback() {
    let fixture = NativeUpdateFixture::stage().with_base_profile();
    fixture.reject_fallback();
    assert_native_silent(
        &fixture.rust_dot(&["update", "--quiet"]),
        "profile base selection",
    );
}

#[test]
fn update_native_profile_addition_discovered_after_base_pull_stays_native() {
    let fixture = NativeUpdateFixture::stage().with_base_discovered_profile_addition();
    fixture.reject_fallback();
    assert_native_silent(
        &fixture.rust_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("XDG_CONFIG_HOME", fixture.client.home.join(".config"));
        }),
        "profile addition after base pull",
    );
    let target = fixture.client.home.join(".config/profile/value");
    assert!(target.is_symlink(), "final profile value is a managed link");
    assert_eq!(
        std::fs::read(&target).expect("final profile value"),
        b"beta\n"
    );
    assert!(
        fixture.client.home.join(".dotfiles-beta/.git").is_dir(),
        "additions phase cloned beta"
    );
    let manifest = fixture.client.home.join(".local/state/dot/overlay-links");
    assert!(
        std::fs::read_to_string(manifest)
            .expect("overlay manifest")
            .contains("beta"),
        "final manifest names the post-base addition"
    );
    assert_eq!(
        std::fs::read(fixture.client.home.join("post-base-pre-sync"))
            .expect("post-base hook marker"),
        b"reconcile",
        "a hook introduced by the base refresh runs in final reconcile"
    );
}

#[test]
fn update_native_profile_selector_conflict_rejects_fallback() {
    // This is the post-base selector phase: both selectors are valid and
    // trusted, but their different profiles must freeze the generation with a
    // profile error rather than leave native execution.
    let fixture = NativeUpdateFixture::stage().with_conflicting_profile_selectors();
    fixture.reject_fallback();
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1), "selector conflict status");
    assert!(
        output
            .stderr
            .starts_with(b"dot: profile: equally specific selectors choose base and dev"),
        "selector conflict stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output
            .stderr
            .windows(b"UNEXPECTED-FALLBACK".len())
            .any(|window| window == b"UNEXPECTED-FALLBACK")
    );
}

#[test]
fn update_native_profile_downgrade_retires_lifecycle_state() {
    // First establish alpha's lifecycle authority. Then switch the persisted
    // default profile to beta: the native two-phase driver must link beta,
    // execute alpha's trusted deactivation hook, and commit the emptied ledger.
    let fixture = NativeUpdateFixture::stage().with_profile_retirement();
    fixture.reject_fallback();
    assert_native_silent(&fixture.rust_dot(&["update", "--quiet"]), "profile setup");
    let ledger = fixture
        .client
        .home
        .join(".local/state/dot/profile-overlay-lifecycle-v1");
    assert!(
        std::fs::read_to_string(&ledger)
            .expect("setup ledger")
            .contains("alpha|")
    );
    std::fs::write(
        fixture.client.xdg.join("dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndefault_profile=dev\n",
    )
    .expect("switch profile");

    assert_native_silent(
        &fixture.rust_dot(&["update", "--quiet"]),
        "profile downgrade",
    );
    assert_eq!(
        std::fs::read(fixture.client.home.join("alpha-retired")).expect("retirement marker"),
        b"alpha|0",
        "retirement publishes saved identity without active overlays"
    );
    assert!(
        !std::fs::read_to_string(&ledger)
            .expect("committed ledger")
            .contains("alpha|")
    );
    assert!(
        fixture.client.home.join(".dotfiles-beta/.git").is_dir(),
        "beta checkout"
    );
}

#[test]
fn update_native_profile_failed_retirement_preserves_lifecycle_authority() {
    let fixture = NativeUpdateFixture::stage().with_profile_retirement();
    assert_native_silent(&fixture.rust_dot(&["update", "--quiet"]), "profile setup");
    seed_advance(
        &fixture.client.overlay_seed,
        "dot/profile-deactivate",
        b"deactivate() { printf failed >&2; return 7; }\n",
    );
    std::fs::write(
        fixture.client.xdg.join("dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndefault_profile=dev\n",
    )
    .expect("switch profile");

    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert!(has_bytes(
        &output.stderr,
        b"profile deactivation failed: alpha"
    ));
    assert!(has_bytes(&output.stderr, b"failed"));
    let ledger = fixture
        .client
        .home
        .join(".local/state/dot/profile-overlay-lifecycle-v1");
    assert!(
        std::fs::read_to_string(ledger)
            .expect("retained ledger")
            .contains("alpha|"),
        "failed retirement keeps alpha in the lifecycle ledger"
    );
    assert_eq!(base_snapshot(&fixture.client), clean_checkout());
    assert_clean_checkout(&fixture.client, "alpha");
    assert_clean_checkout(&fixture.client, "beta");
}

#[test]
fn update_native_profile_conflict_after_base_pull_restores_prior_generation() {
    let fixture = NativeUpdateFixture::stage().with_base_profile_rollback();
    fixture.reject_fallback();
    let run = || {
        fixture.rust_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("XDG_CONFIG_HOME", fixture.client.home.join(".config"));
        })
    };
    assert_native_silent(&run(), "rollback fixture setup");
    let target = fixture.client.home.join(".config/profile/value");
    assert!(target.is_symlink(), "setup profile value is a managed link");
    assert_eq!(
        std::fs::read(&target).expect("setup profile value"),
        b"overlay\n"
    );
    let manifest = fixture.client.home.join(".local/state/dot/overlay-links");
    let manifest_before = std::fs::read(&manifest).expect("setup manifest");

    let selectors = fixture
        .client
        .base_seed
        .join(".config/dot/profile-selectors.d");
    std::fs::create_dir_all(&selectors).expect("base selector directory");
    let user = dot::profiles::current_user().expect("current user");
    seed_advance(
        &fixture.client.base_seed,
        ".config/dot/profile-selectors.d/conflict.conf",
        format!("version=1\nuser={user}\nprofile=dev\n").as_bytes(),
    );

    let output = run();
    assert_eq!(
        output.status.code(),
        Some(1),
        "new selector conflict status"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("equally specific selectors choose dev and base"),
        "native conflict stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output
            .stderr
            .windows(b"UNEXPECTED-FALLBACK".len())
            .any(|row| row == b"UNEXPECTED-FALLBACK"),
        "profile conflict must not leave native execution"
    );
    assert!(target.is_symlink(), "rollback restores the managed link");
    assert_eq!(
        std::fs::read(&target).expect("restored profile value"),
        b"overlay\n"
    );
    assert_eq!(
        std::fs::read(&manifest).expect("restored manifest"),
        manifest_before,
        "rollback keeps the prior manifest generation"
    );
}

#[derive(Debug, PartialEq, Eq)]
struct CheckoutSnapshot {
    head_at_upstream: bool,
    porcelain: Vec<u8>,
}

fn clean_checkout() -> CheckoutSnapshot {
    CheckoutSnapshot {
        head_at_upstream: true,
        porcelain: Vec::new(),
    }
}

fn assert_clean_checkout(client: &ReposClient, name: &str) {
    assert_eq!(
        checkout_snapshot(&profile_checkout(client, name)),
        Some(clean_checkout()),
        "{name} checkout"
    );
}

fn git_base_output(client: &ReposClient, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let output = command
        .arg(format!("--git-dir={}", client.base_git_dir.display()))
        .arg(format!("--work-tree={}", client.home.display()))
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("read profile base checkout");
    assert!(
        output.status.success(),
        "profile base checkout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_checkout_output(checkout: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    let output = command
        .arg("-C")
        .arg(checkout)
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("read profile checkout");
    assert!(
        output.status.success(),
        "profile checkout {}: {}",
        checkout.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn base_snapshot(client: &ReposClient) -> CheckoutSnapshot {
    // Compare tracked changes only: that is the Git state profile convergence owns.
    let head = git_base_output(client, &["rev-parse", "HEAD"]);
    let upstream = git_base_output(client, &["rev-parse", "@{upstream}"]);
    CheckoutSnapshot {
        head_at_upstream: head == upstream,
        porcelain: git_base_output(
            client,
            &["status", "--porcelain=v1", "--untracked-files=no"],
        ),
    }
}

fn checkout_snapshot(checkout: &Path) -> Option<CheckoutSnapshot> {
    checkout.join(".git").is_dir().then(|| {
        let head = git_checkout_output(checkout, &["rev-parse", "HEAD"]);
        let upstream = git_checkout_output(checkout, &["rev-parse", "@{upstream}"]);
        CheckoutSnapshot {
            head_at_upstream: head == upstream,
            porcelain: git_checkout_output(
                checkout,
                &["status", "--porcelain=v1", "--untracked-files=no"],
            ),
        }
    })
}

fn profile_checkout(client: &ReposClient, name: &str) -> PathBuf {
    match name {
        "alpha" => client.overlay.clone(),
        "beta" => client.home.join(".dotfiles-beta"),
        _ => panic!("unknown profile fixture checkout: {name}"),
    }
}

fn managed_tree(root: &Path) -> Vec<(String, bool, Vec<u8>)> {
    if !root.exists() {
        return Vec::new();
    }
    let mut tree = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("managed tree directory") {
            let entry = entry.expect("managed tree entry");
            let path = entry.path();
            let kind = std::fs::symlink_metadata(&path).expect("managed tree metadata");
            let relative = path
                .strip_prefix(root)
                .expect("managed tree child")
                .to_string_lossy()
                .into_owned();
            if kind.is_dir() && !kind.file_type().is_symlink() {
                stack.push(path);
            } else if kind.file_type().is_symlink() {
                tree.push((
                    relative,
                    true,
                    std::fs::read_link(path)
                        .expect("managed link target")
                        .into_os_string()
                        .into_encoded_bytes(),
                ));
            } else if kind.is_file() {
                tree.push((relative, false, std::fs::read(path).expect("managed file")));
            }
        }
    }
    tree.sort();
    tree
}

fn normalized_managed_tree(root: &Path, scope: &Path) -> Vec<(String, bool, Vec<u8>)> {
    managed_tree(root)
        .into_iter()
        .map(|(relative, link, bytes)| (relative, link, scrub_scope(&bytes, scope)))
        .collect()
}

#[test]
fn update_native_profile_selection_publishes_the_base_generation() {
    let fixture = NativeUpdateFixture::stage().with_base_profile();
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_native_silent(&output, "base profile selection");
    assert_eq!(base_snapshot(&fixture.client), clean_checkout());
    assert_clean_checkout(&fixture.client, "alpha");
    assert_eq!(
        checkout_snapshot(&profile_checkout(&fixture.client, "beta")),
        None
    );
    assert_eq!(
        normalized_managed_tree(
            &fixture.client.home.join(".config/profile"),
            fixture.client.scope.path(),
        ),
        vec![(
            "value".to_string(),
            true,
            b"../../.dotfiles-alpha/home/.config/profile/value".to_vec(),
        )]
    );
    assert_eq!(
        std::fs::read(fixture.client.home.join(".config/profile/value")).unwrap(),
        b"alpha\n"
    );
    let manifest =
        std::fs::read_to_string(fixture.client.home.join(".local/state/dot/overlay-links"))
            .expect("overlay manifest");
    assert!(manifest.contains("alpha"));
    assert!(
        !fixture
            .client
            .home
            .join(".local/state/dot/profile-overlay-lifecycle-v1")
            .exists()
    );
}

#[test]
fn update_native_profile_selector_conflict_preserves_unpublished_state() {
    let fixture = NativeUpdateFixture::stage().with_conflicting_profile_selectors();
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert!(
        output
            .stderr
            .starts_with(b"dot: profile: equally specific selectors choose base and dev")
    );
    assert_eq!(base_snapshot(&fixture.client), clean_checkout());
    assert_clean_checkout(&fixture.client, "alpha");
    assert!(
        !fixture
            .client
            .home
            .join(".local/state/dot/overlay-links")
            .exists()
    );
}

#[test]
fn update_native_profile_descriptor_refresh_selects_the_new_overlay() {
    // The base refresh changes alpha's descriptor before it selects beta.  A
    // full-record difference must not make alpha an additions-only pull.
    let fixture = NativeUpdateFixture::stage().with_base_discovered_profile_addition();
    let output = fixture.rust_dot_with(&["update", "--quiet"], |cmd| {
        cmd.env("XDG_CONFIG_HOME", fixture.client.home.join(".config"));
    });
    assert_native_silent(&output, "descriptor refresh");
    assert_eq!(
        normalized_managed_tree(
            &fixture.client.home.join(".config/profile"),
            fixture.client.scope.path(),
        ),
        vec![(
            "value".to_string(),
            true,
            b"../../.dotfiles-beta/home/.config/profile/value".to_vec(),
        )]
    );
    assert_eq!(
        std::fs::read(fixture.client.home.join(".config/profile/value")).unwrap(),
        b"beta\n"
    );
    assert_eq!(base_snapshot(&fixture.client), clean_checkout());
    assert_clean_checkout(&fixture.client, "alpha");
    assert_clean_checkout(&fixture.client, "beta");
    assert_eq!(
        std::fs::read(fixture.client.home.join("post-base-pre-sync")).unwrap(),
        b"reconcile"
    );
}

#[test]
fn update_native_profile_retirement_commits_the_selected_generation() {
    let fixture = NativeUpdateFixture::stage().with_profile_retirement();
    assert_native_silent(
        &fixture.rust_dot(&["update", "--quiet"]),
        "retirement setup",
    );
    std::fs::write(
        fixture.client.xdg.join("dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndefault_profile=dev\n",
    )
    .expect("switch profile");
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_native_silent(&output, "retirement");
    assert_eq!(
        std::fs::read(fixture.client.home.join("alpha-retired")).unwrap(),
        b"alpha|0",
        "retirement publishes saved identity without active overlays"
    );
    let ledger = std::fs::read_to_string(
        fixture
            .client
            .home
            .join(".local/state/dot/profile-overlay-lifecycle-v1"),
    )
    .unwrap();
    assert!(!ledger.contains("alpha|"));
    assert_eq!(base_snapshot(&fixture.client), clean_checkout());
    assert_clean_checkout(&fixture.client, "alpha");
    assert_clean_checkout(&fixture.client, "beta");
}

#[test]
fn update_native_profile_rollback_keeps_the_prior_generation() {
    let fixture = NativeUpdateFixture::stage().with_base_profile_rollback();
    let run = || {
        fixture.rust_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("XDG_CONFIG_HOME", fixture.client.home.join(".config"));
        })
    };
    assert_native_silent(&run(), "rollback setup");
    let target = fixture.client.home.join(".config/profile/value");
    let manifest = fixture.client.home.join(".local/state/dot/overlay-links");
    let manifest_before = std::fs::read(&manifest).unwrap();

    let user = dot::profiles::current_user().expect("current user");
    std::fs::create_dir_all(
        fixture
            .client
            .base_seed
            .join(".config/dot/profile-selectors.d"),
    )
    .expect("base selector directory");
    seed_advance(
        &fixture.client.base_seed,
        ".config/dot/profile-selectors.d/conflict.conf",
        format!("version=1\nuser={user}\nprofile=dev\n").as_bytes(),
    );
    let output = run();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert!(has_bytes(
        &output.stderr,
        b"equally specific selectors choose dev and base"
    ));
    assert_eq!(std::fs::read(&target).unwrap(), b"overlay\n");
    assert_eq!(std::fs::read(&manifest).unwrap(), manifest_before);
    assert_eq!(base_snapshot(&fixture.client), clean_checkout());
    assert_clean_checkout(&fixture.client, "alpha");
}

/// Normalize the fixture's temporary scope in a process or state contract.
fn scrub_scope(bytes: &[u8], scope: &std::path::Path) -> Vec<u8> {
    String::from_utf8_lossy(bytes)
        .replace(&scope.to_string_lossy().into_owned(), "@SCOPE@")
        .into_bytes()
}

fn has_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|row| row == needle)
}

/// Scrub update elapsed stamps (trailing `0s`, `1s`) after
/// asserting each is sane. A garbage stamp from an unstarted stage clock must
/// still fail loudly. (The suite `(Ns)` marks use the existing
/// [`scrub_elapsed`] instead.)
fn scrub_update_elapsed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let body_len = line.strip_suffix(b"\n").map_or(line.len(), <[u8]>::len);
        let body = &line[..body_len];
        let completion_prefix = [b"Done in ".as_slice(), b"Done with errors in ".as_slice()]
            .into_iter()
            .find(|prefix| body.starts_with(prefix));
        let range = if let Some(prefix) = completion_prefix {
            let tail = &body[prefix.len()..];
            let digits = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
            let suffix = &tail[digits..];
            let valid_suffix = suffix == b"s"
                || suffix == b"s. Reload your shell: source ~/.bashrc"
                || suffix == b"s. Reload your shell: source ~/.zshrc";
            (digits > 0 && valid_suffix).then_some((prefix.len(), prefix.len() + digits, None))
        } else if body.starts_with(b"[") {
            let close = body.iter().position(|byte| *byte == b']');
            let valid_prefix = close.is_some_and(|close| {
                let mut counts = body[1..close].split(|byte| *byte == b'/');
                let done = counts.next().unwrap_or_default();
                let total = counts.next().unwrap_or_default();
                !done.is_empty()
                    && done.iter().all(u8::is_ascii_digit)
                    && !total.is_empty()
                    && total.iter().all(u8::is_ascii_digit)
                    && counts.next().is_none()
                    && body.get(close + 1) == Some(&b' ')
            });
            let start = body
                .iter()
                .rposition(|byte| *byte == b' ')
                .map_or(0, |index| index + 1);
            let token = &body[start..];
            token
                .strip_suffix(b"s")
                .filter(|digits| {
                    valid_prefix && !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
                })
                .map(|digits| {
                    let gap_start = body[..start]
                        .iter()
                        .rposition(|byte| *byte != b' ')
                        .map_or(0, |index| index + 1);
                    (start, start + digits.len(), Some(gap_start))
                })
        } else {
            None
        };
        if let Some((start, end, gap_start)) = range {
            let seconds = std::str::from_utf8(&body[start..end])
                .expect("elapsed ASCII digits")
                .parse::<i64>()
                .expect("elapsed digits parse");
            assert!(
                (0..=120).contains(&seconds),
                "elapsed stamp out of sane range: {seconds}s",
            );
            out.extend_from_slice(&body[..gap_start.unwrap_or(start)]);
            if gap_start.is_some() {
                out.push(b' ');
            }
            out.extend_from_slice(b"@ELAPSED@");
            out.extend_from_slice(&body[end..]);
        } else {
            out.extend_from_slice(body);
        }
        if body_len != line.len() {
            out.push(b'\n');
        }
    }
    out
}

/// Normalize per-process worker durations while preserving each result row.
fn scrub_merge_durations(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let body_len = line.strip_suffix(b"\n").map_or(line.len(), <[u8]>::len);
        let body = &line[..body_len];
        let is_result = body.starts_with(b"  ok ") || body.starts_with(b"  warning ");
        let start = body
            .iter()
            .rposition(|byte| *byte == b' ')
            .map_or(0, |index| index + 1);
        let token = &body[start..];
        let number = token
            .strip_suffix(b"ms")
            .or_else(|| token.strip_suffix(b"s"));
        let valid = number.is_some_and(|number| {
            let mut pieces = number.split(|byte| *byte == b'.');
            let whole = pieces.next().unwrap_or_default();
            let fraction = pieces.next();
            !whole.is_empty()
                && whole.iter().all(u8::is_ascii_digit)
                && fraction
                    .is_none_or(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
                && pieces.next().is_none()
        });
        if is_result && valid {
            out.extend_from_slice(&body[..start]);
            out.extend_from_slice(b"@ELAPSED@s");
        } else {
            out.extend_from_slice(body);
        }
        if body_len != line.len() {
            out.push(b'\n');
        }
    }
    out
}

#[test]
fn update_duration_normalizers_cover_slow_ci_formats() {
    assert_eq!(
        scrub_update_elapsed(
            b"Done in 10s\nDone with errors in 10s\nDone in 10s. Reload your shell: source ~/.zshrc\n"
        ),
        b"Done in @ELAPSED@s\nDone with errors in @ELAPSED@s\nDone in @ELAPSED@s. Reload your shell: source ~/.zshrc\n"
    );
    assert_eq!(
        scrub_update_elapsed(
            b"[1/5] Repos      changed  3 repos changed, 0 repos current               9s\n\
              [1/5] Repos      changed  3 repos changed, 0 repos current              10s\n"
        ),
        b"[1/5] Repos      changed  3 repos changed, 0 repos current @ELAPSED@s\n\
          [1/5] Repos      changed  3 repos changed, 0 repos current @ELAPSED@s\n"
    );
    assert_eq!(
        scrub_merge_durations(b"  ok Alpha 999ms\n  ok Beta 1.2s\n"),
        b"  ok Alpha @ELAPSED@s\n  ok Beta @ELAPSED@s\n"
    );
    let semantic = b"[hook diagnostic] 10s\nDone in 10s ago\n[1/5] Repos 1 passed 10s ago\n    retry after 1.2s\nSuites: 10 passed\n\xff\n";
    assert_eq!(scrub_update_elapsed(semantic), semantic);
    assert_eq!(scrub_merge_durations(semantic), semantic);
}

#[test]
fn update_native_success_reports_completion_and_commits_state() {
    // The base plus one overlay are current. Pulls are no-ops, so the run exercises the
    // deferred close with real counts, discovery, the link phase,
    // retire, the empty merges close, commit, and normalize.
    let client = stage_repos_client();
    let output = repos_rust_native(&client, &["update"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "update code\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        has_bytes(&output.stdout, b"Reload your shell"),
        "successful update completion: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.stderr, b"");
    assert_eq!(base_snapshot(&client), clean_checkout());
    assert_clean_checkout(&client, "alpha");
    assert!(!client.home.join(".local/state/dot/overlay-links").exists());
}

#[test]
fn update_native_failure_reports_errors_and_restores_state() {
    // A staged client has a broken base origin: the base
    // pull fails, so the run exercises the failed deferred close
    // with real counts, the generation restore, the frozen
    // preservation rows, and the skipped-inputs close. The dead
    // target must EXIST: client selection canonicalizes the origin
    // with `realpath`, and BSD `realpath` (macOS) rejects missing
    // paths that GNU tolerates — an existing non-repo directory
    // fails the fetch identically everywhere instead.
    let break_origin = |client: &ReposClient| {
        let dead = client.scope.path().join("dead-origin");
        std::fs::create_dir_all(&dead).expect("dead origin dir");
        repos_git(
            &client.base_git_dir,
            &[
                "config",
                "remote.origin.url",
                &format!("file://{}", dead.display()),
            ],
        );
    };
    let fixture = NativeUpdateFixture::stage()
        .with_merge(b"merge() { printf 'ran\n' >\"$HOME/.failed-sync-hook\"; }\n");
    let client = &fixture.client;
    break_origin(client);
    let output = repos_rust_native(client, &["update"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "update must fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        has_bytes(&output.stdout, b"Done with errors"),
        "failed completion: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(has_bytes(&output.stderr, b"fatal:"));
    assert_eq!(
        std::fs::read(client.home.join("tracked.txt")).unwrap(),
        b"v1\n"
    );
    assert_eq!(base_snapshot(client), clean_checkout());
    assert_clean_checkout(client, "alpha");
    assert!(
        !client.home.join(".failed-sync-hook").exists(),
        "repository failure must stop before newly eligible merge hooks"
    );
}
