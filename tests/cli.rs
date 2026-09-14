//! CLI parity tests: the Rust binary must behave like the shell dispatcher.
//!
//! `HELP` is pinned against the shell `dot_help` heredoc at compile time
//! via `include_str!`, so any drift in `lib/dot/main.sh` fails this suite
//! until the Rust constant is updated in the same commit.
//!
//! Slice 77 adds differential dispatch tests: the live
//! `dot_command_dispatch` (`lib/dot/commands.sh`) runs with stubbed
//! kernels as the oracle, and the Rust [`dispatch`](dot::cli::dispatch)
//! decision plus the binary's observable behavior must agree with it.
//! Kernel execution itself stays in shell until each kernel slice
//! lands, so for kernel-backed arms the tests pin the oracle's trace
//! and exit code (the contract the kernel slice inherits) alongside
//! the Rust interim "not yet implemented" behavior — never conflated.
//!
//! Slice 83 wires the last two arms (`doctor`, `test`) end to end:
//! the engine rows below compare the Rust binary against the live
//! `bin/dot` on fixtures — exit code plus both streams, byte for
//! byte — so the interim set is empty and no known command reports
//! "not yet implemented" anymore.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dot::cli::{Command as Decision, dispatch, init_acquires_lock};
use dot::test_support::TempDir;

static PROCESS_ENV: Mutex<()> = Mutex::new(());

fn process_env_guard() -> MutexGuard<'static, ()> {
    PROCESS_ENV
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Extract the `dot_help` heredoc body from the shell source.
fn shell_help() -> String {
    let source = include_str!("../lib/dot/main.sh");
    let marker = "cat <<'EOF'\n";
    let start = source.find(marker).expect("dot_help heredoc marker") + marker.len();
    let rest = &source[start..];
    let end = rest.find("\nEOF\n").expect("dot_help heredoc terminator");
    format!("{}\n", &rest[..end])
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dot"))
}

#[test]
fn native_update_flag_capture_does_not_mutate_parent_environment() {
    let _env = process_env_guard();
    // Provider `none` now keeps `--force` on the native path. The explicit
    // embedded runtime must still retain semantic stream, user-tree, and
    // state parity when it re-execs the real binary.
    let parent = native_parent_snapshot();
    let shell_client = stage_repos_client();
    let native_client = stage_repos_client();
    let runtime = runtime_for_force_fallback(&native_client);
    let args = [
        OsString::from("update"),
        OsString::from("--quiet"),
        OsString::from("--force"),
    ];
    let shell = repos_shell(&shell_client, &["update", "--quiet", "--force"]);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = dot::app::run(
        &runtime,
        &args,
        &mut dot::app::Streams::new(&mut stdout, &mut stderr),
    );

    assert_eq!(code, shell.status.code().expect("shell exit"));
    assert_eq!(
        scrub_twin(&stdout, native_client.scope.path()),
        scrub_twin(&shell.stdout, shell_client.scope.path()),
        "force fallback stdout",
    );
    assert_eq!(
        scrub_twin(&stderr, native_client.scope.path()),
        scrub_twin(&shell.stderr, shell_client.scope.path()),
        "force fallback stderr",
    );
    assert_eq!(
        semantic_tree(&native_client.home, true),
        semantic_tree(&shell_client.home, true),
        "force fallback user tree",
    );
    assert_eq!(
        semantic_tree(runtime.state_home(), false),
        semantic_tree(&shell_client.home.join(".local/state"), false),
        "force fallback state",
    );
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
fn help_constant_matches_shell_heredoc_byte_for_byte() {
    assert_eq!(dot::cli::HELP, shell_help());
}

#[test]
fn binary_help_matches_shell_help() {
    let output = bin().arg("help").output().expect("run dot help");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        shell_help()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn binary_default_command_is_help() {
    let output = bin().output().expect("run dot");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        shell_help()
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
fn binary_version_agrees_with_shell_in_same_checkout() {
    // Both implementations resolve the revision from the same checkout,
    // so their outputs must be identical here. Skips are LOUD (stderr):
    // a silent pass would hide a broken shell path or a stale baked
    // revision. Shell parity itself is owned by `bash tests/run`.
    // Known race: a commit landing between compile time (baked SHA) and
    // this run fails despite both sides being correct; likewise an
    // explicit DOT_BUILD_COMMIT/GITHUB_SHA stamping intentionally
    // disagrees with run-time `git rev-parse HEAD`.
    let shell = Command::new("bash")
        .arg("bin/dot")
        .arg("version")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output();
    let Ok(shell) = shell else {
        eprintln!("SKIP: cannot spawn bash for shell agreement check");
        return;
    };
    if !shell.status.success() {
        eprintln!("SKIP: shell `dot version` failed; shell parity is owned by tests/run");
        return;
    }
    let rust = bin().arg("version").output().expect("run dot version");
    assert!(rust.status.success());
    assert_eq!(rust.stdout, shell.stdout);
}

/// The exact `printf` format in the shell dispatcher. Unlike HELP (a
/// heredoc with stable boundaries), this is one line inside a function,
/// so the pin asserts the shell still contains the literal rather than
/// re-extracting it: a wording drift in `commands.sh` must fail here.
fn shell_unknown_command_format() -> &'static str {
    let source = include_str!("../lib/dot/commands.sh");
    assert!(
        source.contains("printf 'dot: unknown command: %s\\n'"),
        "shell dispatcher changed its unknown-command wording"
    );
    "dot: unknown command: frobnicate\n"
}

#[test]
fn binary_unknown_command_fails_like_shell() {
    let output = bin()
        .arg("frobnicate")
        .output()
        .expect("run dot frobnicate");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("stderr UTF-8"),
        shell_unknown_command_format()
    );
}

#[test]
fn binary_help_flags_match_shell() {
    let expected = shell_help();
    for flag in ["-h", "--help"] {
        let output = bin().arg(flag).output().expect("run dot flag");
        assert!(output.status.success(), "flag: {flag}");
        assert_eq!(
            String::from_utf8(output.stdout).expect("stdout UTF-8"),
            expected,
            "flag: {flag}"
        );
        assert!(output.stderr.is_empty(), "flag: {flag}");
    }
}

/// Stubbed kernels for the dispatch oracle: each prints a trace token
/// with its arguments (`"$*"` joins like the shell passes them on)
/// and exits with an overridable code, so every dispatch decision —
/// routing, argument forwarding, resolve/lock gating, and the
/// ignore-vs-propagate exit-code quirks — is observable without side
/// effects. `crontab` is overridden as a function (functions win over
/// PATH lookup), so `cron` needs no fixture binary on this side.
const ORACLE_STUBS: &str = concat!(
    "_dot_cleanup_install_owner_traps() { printf 'TRAPS\\n'; }\n",
    "_dot_update_lock_acquire() { printf 'LOCK-ACQUIRE:%s\\n' \"$*\"; ",
    "return \"${STUB_LOCK_RC:-0}\"; }\n",
    "_dot_update() { printf 'UPDATE:%s\\n' \"$*\"; ",
    "return \"${STUB_UPDATE_RC:-0}\"; }\n",
    "_dot_resolve_overlays() { printf 'RESOLVE:%s SILENT:%s\\n' \"$*\" ",
    "\"${DOT_OVERLAY_DISCOVERY_SILENT:-unset}\"; ",
    "return \"${STUB_RESOLVE_RC:-0}\"; }\n",
    "_repo_fetch_all() { printf 'FETCH-ALL:%s\\n' \"$*\"; ",
    "return \"${STUB_ALL_RC:-0}\"; }\n",
    "_repo_push_all() { printf 'PUSH-ALL:%s\\n' \"$*\"; ",
    "return \"${STUB_ALL_RC:-0}\"; }\n",
    "_repo_status_all() { printf 'STATUS-ALL:%s\\n' \"$*\"; ",
    "return \"${STUB_ALL_RC:-0}\"; }\n",
    "_repo_diff_all() { printf 'DIFF-ALL:%s\\n' \"$*\"; ",
    "return \"${STUB_ALL_RC:-0}\"; }\n",
    "_dot_doctor() { printf 'DOCTOR:%s\\n' \"$*\"; ",
    "return \"${STUB_DOCTOR_RC:-0}\"; }\n",
    "dot_test_command() { printf 'TEST-CMD:%s\\n' \"$*\"; ",
    "return \"${STUB_TEST_RC:-0}\"; }\n",
    "dot_init_command() { printf 'INIT-CMD:%s\\n' \"$*\"; ",
    "return \"${STUB_INIT_RC:-0}\"; }\n",
    "crontab() { if [ \"${STUB_CRONTAB_RC:-0}\" = 0 ]; then ",
    "printf '%s' \"${STUB_CRONTAB_OUT:-}\"; else return 1; fi; }\n",
);

/// Run the live `dot_command_dispatch` with stubbed kernels.
/// Returns (exit code, stdout trace, stderr). The trace's last line is
/// always the `ORACLE-RC=` trailer, split off so assertions read only
/// the kernels' tokens.
fn oracle(argv: &[&OsStr], extra_env: &[(&str, &str)]) -> (i32, Vec<u8>, Vec<u8>) {
    // Built with `push_str`, not `format!`: the shell `${...}`
    // expansions would read as format placeholders.
    let mut script = String::from(ORACLE_STUBS);
    script.push_str(". \"$1/lib/dot/commands.sh\"\n");
    script.push_str("shift\n");
    script.push_str("dot_command_dispatch \"$@\"\n");
    script.push_str("rc=$?\n");
    script.push_str("printf 'ORACLE-RC=%d\\n' \"$rc\"\n");
    let home = TempDir::new("cli-oracle").expect("oracle home");
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(script);
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    // One `.env` per variable (never `.envs`): each entry stays
    // auditable, matching the repos differential-test convention.
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home.path())
        .env("DOT_TEST", "1")
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn dispatch oracle");
    // Byte-level trailer split (never `str` slicing): the trace stays
    // raw bytes end to end, and only the `ORACLE-RC=` line is decoded.
    let mut trace = output.stdout;
    assert_eq!(
        trace.pop(),
        Some(b'\n'),
        "oracle output ends with newline: {trace:?}"
    );
    let split = trace
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let trailer = trace.split_off(split);
    let code: i32 = std::str::from_utf8(&trailer)
        .ok()
        .and_then(|line| line.strip_prefix("ORACLE-RC="))
        .unwrap_or_else(|| panic!("oracle lost its RC trailer: {trailer:?}"))
        .parse()
        .expect("oracle RC is numeric");
    (code, trace, output.stderr)
}

/// The shell arm structure the Rust table mirrors: every `case` label,
/// the `pull` recursion, the resolve modes, the `test` rc handoff, the
/// `init` lock-skip flags, and the exact `cron`/unknown spellings. A
/// wording or routing drift in `commands.sh` fails here before any
/// behavioral assertion can silently pass against the wrong shape.
#[test]
fn shell_source_still_has_every_dispatch_arm() {
    let source = include_str!("../lib/dot/commands.sh");
    for arm in [
        "update)", "pull)", "fetch)", "push)", "status)", "diff)", "cron)", "doctor)", "test)",
        "init)",
    ] {
        assert!(source.contains(arm), "shell lost its {arm} arm");
    }
    for line in [
        "dot_command_dispatch update \"$@\"",
        "_dot_resolve_overlays fetch",
        "_dot_resolve_overlays inspect",
        "dot_test_command \"$@\" || rc=$?",
        "--status | --help | -h",
        "crontab -l 2>/dev/null || printf '  no crontab installed\\n'",
        "printf 'dot: unknown command: %s\\n'",
        "DOT_OVERLAY_DISCOVERY_SILENT=1",
        "return \"$rc\"",
    ] {
        assert!(source.contains(line), "shell lost: {line}");
    }
}

#[test]
fn oracle_update_runs_traps_lock_then_update() {
    assert_eq!(dispatch(b"update"), Decision::Update);
    let (code, trace, err) = oracle(
        &[OsStr::new("update"), OsStr::new("a"), OsStr::new("b")],
        &[],
    );
    assert_eq!(code, 0);
    assert_eq!(trace, b"TRAPS\nLOCK-ACQUIRE:a b\nUPDATE:a b\n");
    assert!(err.is_empty());
    // A failing kernel is ignored: the dispatcher returns `rc` (0),
    // not the kernel's status. Kernel slices must preserve this.
    let (code, _, _) = oracle(&[OsStr::new("update")], &[("STUB_UPDATE_RC", "3")]);
    assert_eq!(code, 0);
    // A failing lock short-circuits with its own status (e.g. 75 busy).
    let (code, trace, _) = oracle(&[OsStr::new("update")], &[("STUB_LOCK_RC", "75")]);
    assert_eq!(code, 75);
    assert_eq!(trace, b"TRAPS\nLOCK-ACQUIRE:\n");
}

#[test]
fn oracle_pull_aliases_update_exactly() {
    assert_eq!(dispatch(b"pull"), Decision::Update);
    assert_eq!(dispatch(b"pull"), dispatch(b"update"));
    let pull = oracle(&[OsStr::new("pull"), OsStr::new("x")], &[]);
    let update = oracle(&[OsStr::new("update"), OsStr::new("x")], &[]);
    assert_eq!(pull, update);
    assert_eq!(pull.0, 0);
    assert_eq!(pull.1, b"TRAPS\nLOCK-ACQUIRE:x\nUPDATE:x\n");
}

#[test]
fn oracle_fetch_resolves_fetch_mode_then_fetches() {
    assert_eq!(dispatch(b"fetch"), Decision::Fetch);
    let (code, trace, err) = oracle(&[OsStr::new("fetch")], &[]);
    assert_eq!(code, 0);
    assert_eq!(trace, b"RESOLVE:fetch SILENT:unset\nFETCH-ALL:\n");
    assert!(err.is_empty());
    let (code, trace, _) = oracle(&[OsStr::new("fetch")], &[("STUB_RESOLVE_RC", "1")]);
    assert_eq!(code, 1);
    assert_eq!(trace, b"RESOLVE:fetch SILENT:unset\n");
}

#[test]
fn oracle_push_status_diff_resolve_inspect_mode() {
    let cases: &[(&str, Decision, &[u8])] = &[
        ("push", Decision::Push, b"PUSH-ALL:\n"),
        ("status", Decision::Status, b"STATUS-ALL:\n"),
        ("diff", Decision::Diff, b"DIFF-ALL:\n"),
    ];
    for (name, expected, token) in cases {
        assert_eq!(dispatch(name.as_bytes()), *expected, "command: {name}");
        let arg = OsStr::new(name);
        let (code, trace, err) = oracle(&[arg], &[]);
        assert_eq!(code, 0, "command: {name}");
        let mut full = b"RESOLVE:inspect SILENT:unset\n".to_vec();
        full.extend_from_slice(token);
        assert_eq!(trace, full, "command: {name}");
        assert!(err.is_empty(), "command: {name}");
        let (code, _, _) = oracle(&[arg], &[("STUB_RESOLVE_RC", "1")]);
        assert_eq!(code, 1, "command: {name}");
    }
}

#[test]
fn oracle_doctor_succeeds_despite_any_failure() {
    assert_eq!(dispatch(b"doctor"), Decision::Doctor);
    // Resolve failure AND doctor failure: `|| true` plus the ignored
    // kernel status keep the dispatcher at 0, with discovery silenced.
    let (code, trace, err) = oracle(
        &[OsStr::new("doctor")],
        &[("STUB_RESOLVE_RC", "1"), ("STUB_DOCTOR_RC", "5")],
    );
    assert_eq!(code, 0);
    assert_eq!(trace, b"TRAPS\nRESOLVE:inspect SILENT:1\nDOCTOR:\n");
    assert!(err.is_empty());
}

#[test]
fn oracle_test_propagates_test_status() {
    assert_eq!(dispatch(b"test"), Decision::Test);
    let (code, trace, err) = oracle(
        &[OsStr::new("test"), OsStr::new("t1")],
        &[("STUB_TEST_RC", "3")],
    );
    assert_eq!(code, 3);
    assert_eq!(trace, b"TRAPS\nRESOLVE:inspect SILENT:unset\nTEST-CMD:t1\n");
    assert!(err.is_empty());
    let (code, _, _) = oracle(&[OsStr::new("test")], &[("STUB_RESOLVE_RC", "1")]);
    assert_eq!(code, 1);
}

#[test]
fn oracle_init_lock_branches_on_first_arg() {
    assert_eq!(dispatch(b"init"), Decision::Init);
    // No argument acquires the lock (`${1:-}` is empty → `*`).
    assert!(init_acquires_lock(None));
    let (code, trace, _) = oracle(&[OsStr::new("init")], &[]);
    assert_eq!(code, 0);
    assert_eq!(trace, b"TRAPS\nLOCK-ACQUIRE:\nINIT-CMD:\n");
    // Read-only probes skip the lock but still run init.
    for flag in ["--status", "--help", "-h"] {
        assert!(!init_acquires_lock(Some(flag.as_bytes())), "flag: {flag}");
        let (code, trace, _) = oracle(&[OsStr::new("init"), OsStr::new(flag)], &[]);
        assert_eq!(code, 0, "flag: {flag}");
        assert_eq!(
            trace,
            format!("TRAPS\nINIT-CMD:{flag}\n").into_bytes(),
            "flag: {flag}"
        );
    }
    // Anything else acquires, and init's own failure is ignored.
    assert!(init_acquires_lock(Some(b"--other")));
    let (code, trace, _) = oracle(
        &[OsStr::new("init"), OsStr::new("--other")],
        &[("STUB_INIT_RC", "4")],
    );
    assert_eq!(code, 0);
    assert_eq!(trace, b"TRAPS\nLOCK-ACQUIRE:\nINIT-CMD:--other\n");
    let (code, _, _) = oracle(&[OsStr::new("init")], &[("STUB_LOCK_RC", "75")]);
    assert_eq!(code, 75);
}

#[test]
fn oracle_bare_dispatch_reports_help_as_unknown() {
    // `dot_command_dispatch` with no argument defaults to `help`,
    // which has no arm there (`main.sh` handles it first): unknown.
    let (code, trace, err) = oracle(&[], &[]);
    assert_eq!(code, 1);
    assert!(trace.is_empty());
    assert_eq!(err, b"dot: unknown command: help\n");
    assert_eq!(dispatch(b"help"), Decision::Unknown);
}

/// A `crontab` fixture printing `$FAKE_CRONTAB_BODY` for `crontab
/// -l` (the body travels by environment, never embedded in the
/// script, so quoting cannot mangle it). Lives in an exec-capable dir
/// (the system temp dir may be `noexec`); resolution runs through
/// PATH, never a hardcoded path.
fn fake_crontab() -> TempDir {
    let dir = TempDir::new_exec("fake-crontab").expect("exec dir");
    let script = dir.write(
        "crontab",
        b"#!/bin/sh\nprintf '%s' \"$FAKE_CRONTAB_BODY\"\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake crontab");
    }
    dir
}

fn prepend_path(dir: &Path) -> std::ffi::OsString {
    let orig = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&orig)))
        .expect("join PATH")
}

#[test]
fn cron_matches_oracle_on_both_branches() {
    assert_eq!(dispatch(b"cron"), Decision::Cron);
    // Success branch: the listing passes through byte for byte.
    let listing = "CRON-LINE-1\nCRON-LINE-2\n";
    let fixture = fake_crontab();
    let (ocode, otrace, oerr) = oracle(
        &[OsStr::new("cron")],
        &[("STUB_CRONTAB_OUT", listing), ("STUB_CRONTAB_RC", "0")],
    );
    assert_eq!(ocode, 0);
    assert_eq!(otrace, listing.as_bytes());
    assert!(oerr.is_empty());
    let rust = bin()
        .arg("cron")
        .env("PATH", prepend_path(fixture.path()))
        .env("FAKE_CRONTAB_BODY", listing)
        .output()
        .expect("run dot cron");
    assert_eq!(rust.status.code(), Some(ocode));
    assert_eq!(rust.stdout, otrace);
    assert_eq!(rust.stderr, oerr);
    // Failure branch (no crontab at all): the fallback line, code 0.
    let (ocode, otrace, oerr) = oracle(&[OsStr::new("cron")], &[("STUB_CRONTAB_RC", "1")]);
    assert_eq!(ocode, 0);
    assert_eq!(otrace, b"  no crontab installed\n");
    assert!(oerr.is_empty());
    let empty = TempDir::new("cli-cron-empty").expect("empty PATH dir");
    let rust = bin()
        .arg("cron")
        .env("PATH", empty.path())
        .output()
        .expect("run dot cron without crontab");
    assert_eq!(rust.status.code(), Some(ocode));
    assert_eq!(rust.stdout, otrace);
    assert_eq!(rust.stderr, oerr);
}

#[test]
fn binary_unknown_matches_oracle_byte_for_byte() {
    let (ocode, otrace, oerr) = oracle(&[OsStr::new("frobnicate")], &[]);
    assert_eq!(ocode, 1);
    assert!(otrace.is_empty());
    assert_eq!(oerr, b"dot: unknown command: frobnicate\n");
    let rust = bin()
        .arg("frobnicate")
        .output()
        .expect("run dot frobnicate");
    assert_eq!(rust.status.code(), Some(ocode));
    assert_eq!(rust.stdout, otrace);
    assert_eq!(rust.stderr, oerr);
}

#[cfg(unix)]
#[test]
fn binary_unknown_non_utf8_matches_oracle() {
    use std::os::unix::ffi::OsStringExt as _;
    let raw = vec![0x66u8, 0x6F, 0xFF, 0x62]; // "fo\xFFb"
    let arg = std::ffi::OsString::from_vec(raw.clone());
    let (ocode, otrace, oerr) = oracle(&[arg.as_os_str()], &[]);
    let rust = bin().arg(&arg).output().expect("run dot non-UTF8");
    assert_eq!(rust.status.code(), Some(ocode));
    assert_eq!(rust.stdout, otrace);
    assert_eq!(rust.stderr, oerr);
    let mut expected = b"dot: unknown command: ".to_vec();
    expected.extend_from_slice(&raw);
    expected.push(b'\n');
    assert_eq!(oerr, expected);
}

#[test]
fn update_passes_flag_exports_to_child_without_mutating_parent() {
    let _env = process_env_guard();
    // Slice 80 runs `Command::Update` end to end: the shell loop's
    // values reach the native engine, which then runs for real
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
        // An empty HOME has no base repo and nothing to converge:
        // the shell succeeds with its no-base rows (pinned against
        // `bin/dot`), so the wired arm reports `0` — never the
        // interim diagnostic.
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

/// The production shell over the twin home (unlike [`shell_dot`],
/// whose checkout cwd would change test source selection): the
/// strongest oracle for the wired doctor/test arms, comparing
/// process observables end to end on the same env and cwd.
fn engine_shell(
    home: &TempDir,
    state: &TempDir,
    argv: &[&str],
    extra: &[(&str, &str)],
) -> std::process::Output {
    // Absolute launcher path: `cwd` is the twin home below (not
    // the checkout like `shell_dot`), so a relative script would
    // not resolve.
    let launcher = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/dot");
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg(&launcher);
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

/// The Rust binary over the same fixture.
fn engine_rust(
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run dot binary")
}

/// One engine-arm row: the shell runs first (the oracle), then the
/// Rust binary on the same home/state — the rows below are read-only
/// (doctor) or hermetic to their own suite dirs (test), so the
/// second run observes the same client — and exit code plus both
/// streams must agree byte for byte.
fn run_engine_pair(
    home: &TempDir,
    state: &TempDir,
    argv: &[&str],
    extra: &[(&str, &str)],
) -> (std::process::Output, std::process::Output) {
    let shell = engine_shell(home, state, argv, extra);
    let rust = engine_rust(home, state, argv, extra);
    (shell, rust)
}

/// Assert one engine-arm row: exit code and both streams, byte for byte.
fn check_engine_pair(shell: &std::process::Output, rust: &std::process::Output, argv: &[&str]) {
    assert_eq!(
        rust.status.code(),
        shell.status.code(),
        "argv: {argv:?}\n shell stdout: {}\n shell stderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert_eq!(rust.stdout, shell.stdout, "argv: {argv:?} stdout");
    assert_eq!(rust.stderr, shell.stderr, "argv: {argv:?} stderr");
}

/// Trust one fixture path exactly like the shell suites do (`umask
/// 077` there): the ambient test umask may be permissive, so modes
/// are set explicitly rather than inherited.
#[cfg(unix)]
fn seal(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("seal fixture");
}

#[test]
fn doctor_empty_home_matches_shell() {
    // No client checkout: the base-repo check fails, so doctor
    // reports the failure rows and exits 1 — nothing to stage.
    let home = TempDir::new("cli-doctor-empty").expect("twin home");
    let state = TempDir::new("cli-doctor-empty-state").expect("twin state");
    let (shell, rust) = run_engine_pair(&home, &state, &["doctor"], &[]);
    assert_eq!(
        shell.status.code(),
        Some(1),
        "oracle fails without a client: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("client repository is missing"),
        "oracle names the missing client: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    assert!(shell.stderr.is_empty(), "clean doctor is silent on stderr");
    check_engine_pair(&shell, &rust, &["doctor"]);
}

/// Initialized file:// client for the doctor pass/extension rows: a
/// one-commit bare origin plus a shell `init --yes` (the oracle
/// stages the client, like the shell suites do — the arm under test
/// below is doctor, compared row by row).
fn stage_doctor_client() -> (TempDir, TempDir, TempDir) {
    let scope = TempDir::new("cli-doctor-origin").expect("origin scope");
    let (origin, _seed, _branch) = seed_bare_origin(scope.path(), "dotfiles");
    let home = TempDir::new("cli-doctor-client").expect("twin home");
    let state = TempDir::new("cli-doctor-client-state").expect("twin state");
    let url = format!("file://{}", origin.display());
    let staged = engine_shell(&home, &state, &["init", "--yes", &url], &[]);
    assert_eq!(
        staged.status.code(),
        Some(0),
        "oracle stages the client: {}",
        String::from_utf8_lossy(&staged.stderr),
    );
    (scope, home, state)
}

#[test]
fn doctor_init_client_matches_shell() {
    // Healthy client, no extensions: warnings stay (the worktree
    // checkout is outside the managed locations), failures clear,
    // exit 0.
    let (_scope, home, state) = stage_doctor_client();
    let (shell, rust) = run_engine_pair(&home, &state, &["doctor"], &[]);
    assert_eq!(
        shell.status.code(),
        Some(0),
        "oracle passes on a healthy client: {} {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("0 failed"),
        "oracle reports zero failures: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    assert!(
        shell.stderr.is_empty(),
        "passing doctor is silent on stderr"
    );
    check_engine_pair(&shell, &rust, &["doctor"]);
}

/// Home whose overlay resolution fails: the dispatcher prints the
/// resolve warning on stderr, then doctor still runs (`|| true`)
/// while test refuses (`|| return 1`).
fn stage_bad_descriptor() -> (TempDir, TempDir) {
    let home = TempDir::new("cli-bad-desc").expect("twin home");
    let state = TempDir::new("cli-bad-desc-state").expect("twin state");
    let confd = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&confd).expect("overlay conf dir");
    std::fs::write(home.path().join(".config/dot/config"), b"version=1\n").expect("config");
    std::fs::write(confd.join("90-bad.conf"), b"url=x\nsync=hg\n").expect("bad descriptor");
    (home, state)
}

#[test]
fn doctor_resolve_failure_matches_shell() {
    let (home, state) = stage_bad_descriptor();
    let (shell, rust) = run_engine_pair(&home, &state, &["doctor"], &[]);
    assert_eq!(
        shell.status.code(),
        Some(1),
        "oracle doctor still reports its checks: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("unknown sync value: hg"),
        "oracle prints the resolve warning: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("dot runtime"),
        "oracle doctor still ran: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    check_engine_pair(&shell, &rust, &["doctor"]);
}

#[test]
fn test_resolve_failure_matches_shell() {
    let (home, state) = stage_bad_descriptor();
    let (shell, rust) = run_engine_pair(&home, &state, &["test"], &[]);
    assert_eq!(
        shell.status.code(),
        Some(1),
        "oracle test refuses without resolution: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    assert!(
        shell.stdout.is_empty(),
        "refused test prints nothing on stdout"
    );
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("unknown sync value: hg"),
        "oracle prints the resolve warning: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    check_engine_pair(&shell, &rust, &["test"]);
}

/// One failing doctor extension on an initialized client, mirroring
/// the shell-suite trust recipe (0700 extension dirs, sealed
/// scripts, explicit extension config): the worker failure marks
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
fn doctor_extension_failure_matches_shell() {
    let (_scope, home, state) = stage_doctor_client();
    stage_doctor_extension(&home);
    let (shell, rust) = run_engine_pair(&home, &state, &["doctor"], &[]);
    assert_eq!(
        shell.status.code(),
        Some(1),
        "oracle aggregates the extension failure: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("expected extension failure"),
        "oracle carries the extension record: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    check_engine_pair(&shell, &rust, &["doctor"]);
}

#[test]
fn test_help_matches_shell() {
    // Static pin beside the oracle comparison, so a future wording
    // drift reads as an explicit contract change.
    let home = TempDir::new("cli-test-help").expect("twin home");
    let state = TempDir::new("cli-test-help-state").expect("twin state");
    let (shell, rust) = run_engine_pair(&home, &state, &["test", "--help"], &[]);
    assert_eq!(shell.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&shell.stdout),
        "usage: dot test [-s|--sequential] [-v|--verbose] [-j N|--jobs N] [--list] [name ...]\n\
         \n\
         Set DOT_TEST_INCLUDE_PROVIDER=1 to include the provider suite in an\n\
         unfiltered run. Select `dot` by name to run only the provider suite.\n",
    );
    assert!(shell.stderr.is_empty());
    check_engine_pair(&shell, &rust, &["test", "--help"]);
}

#[test]
fn test_unknown_option_matches_shell() {
    let home = TempDir::new("cli-test-opt").expect("twin home");
    let state = TempDir::new("cli-test-opt-state").expect("twin state");
    let (shell, rust) = run_engine_pair(&home, &state, &["test", "--bogus"], &[]);
    assert_eq!(shell.status.code(), Some(2));
    assert!(shell.stdout.is_empty());
    assert_eq!(shell.stderr, b"unknown option: --bogus\n");
    check_engine_pair(&shell, &rust, &["test", "--bogus"]);
}

#[test]
fn test_list_matches_shell() {
    // No suites configured: only the provider identity lists.
    let home = TempDir::new("cli-test-list").expect("twin home");
    let state = TempDir::new("cli-test-list-state").expect("twin state");
    let (shell, rust) = run_engine_pair(&home, &state, &["test", "-l"], &[]);
    assert_eq!(shell.status.code(), Some(0));
    assert_eq!(shell.stdout, b"dot\n");
    assert!(shell.stderr.is_empty());
    check_engine_pair(&shell, &rust, &["test", "-l"]);
}

/// Local `*-test` suites for the propagation rows: one passing
/// (`complete` record, exit 0), one failing (exit 3, no record).
/// Discovery runs through `DOT_TEST_TESTS_DIR`, so no client
/// checkout is needed; the scope lives in an exec-capable dir (the
/// system temp dir may be `noexec`) with sealed modes, mirroring
/// the shell-suite trust recipe.
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
/// before comparing: the runner stamps `$SECONDS` (integer
/// precision), so a loaded machine can tip a mark across a second
/// boundary on one side only. Codes, glyphs, labels, ordering, and
/// summaries still compare exactly — only wall-clock is excluded
/// from byte parity. The digit run keeps `(1 total)`-style clauses
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

/// One suite-propagation row: exit code plus stderr compare exactly;
/// stdout compares with elapsed marks scrubbed (see
/// [`scrub_elapsed`]).
fn check_suite_pair(shell: &std::process::Output, rust: &std::process::Output, argv: &[&str]) {
    assert_eq!(
        rust.status.code(),
        shell.status.code(),
        "argv: {argv:?}\n shell stdout: {}\n shell stderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert_eq!(
        scrub_elapsed(&rust.stdout),
        scrub_elapsed(&shell.stdout),
        "argv: {argv:?} stdout",
    );
    assert_eq!(rust.stderr, shell.stderr, "argv: {argv:?} stderr");
}

#[test]
fn test_suite_pass_matches_shell() {
    // `DOT_TEST_NO_COLOR=1` selects the plain rendering both sides
    // share regardless of gum.
    let fixture = stage_suites();
    let home = TempDir::new("cli-test-pass").expect("twin home");
    let state = TempDir::new("cli-test-pass-state").expect("twin state");
    let dir = fixture.dir.to_string_lossy().into_owned();
    let extra = [
        ("DOT_TEST_NO_COLOR", "1"),
        ("DOT_TEST_TESTS_DIR", dir.as_str()),
    ];
    let (shell, rust) = run_engine_pair(&home, &state, &["test", "-s", "pass"], &extra);
    // Skips are LOUD (stderr): suite execution needs the timeout
    // supervisor (`python3` plus `test-timeout-v1`), which some
    // platforms (e.g. the Debian CI image, whose test prerequisites
    // the rust jobs skip) do not provide. The oracle reports the
    // missing prerequisite itself — via the suite fifo on stdout,
    // or stderr — so a silent pass can never hide behind it (the
    // `binary_version_agrees_with_shell_in_same_checkout` precedent
    // for environment-dependent oracles).
    if String::from_utf8_lossy(&shell.stdout).contains("suite timeout requires python3")
        || String::from_utf8_lossy(&shell.stderr).contains("suite timeout requires python3")
    {
        eprintln!(
            "SKIP: no python3 suite supervisor here; suite parity is owned by platforms with the test prerequisites"
        );
        return;
    }
    assert_eq!(
        shell.status.code(),
        Some(0),
        "oracle passes the passing suite: {} {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("Suites: 1 passed (1 total)"),
        "oracle prints the pass summary: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    check_suite_pair(&shell, &rust, &["test", "-s", "pass"]);
}

#[test]
fn test_suite_fail_matches_shell() {
    let fixture = stage_suites();
    let home = TempDir::new("cli-test-fail").expect("twin home");
    let state = TempDir::new("cli-test-fail-state").expect("twin state");
    let dir = fixture.dir.to_string_lossy().into_owned();
    let extra = [
        ("DOT_TEST_NO_COLOR", "1"),
        ("DOT_TEST_TESTS_DIR", dir.as_str()),
    ];
    let (shell, rust) = run_engine_pair(&home, &state, &["test", "-s", "fail"], &extra);
    // Same loud skip as the pass row above: without the timeout
    // supervisor neither side can execute a suite, so there is no
    // propagation to compare (a coincidental code match would hide
    // the missing coverage). Either stream carries the oracle's
    // report (see above).
    if String::from_utf8_lossy(&shell.stdout).contains("suite timeout requires python3")
        || String::from_utf8_lossy(&shell.stderr).contains("suite timeout requires python3")
    {
        eprintln!(
            "SKIP: no python3 suite supervisor here; suite parity is owned by platforms with the test prerequisites"
        );
        return;
    }
    assert_eq!(
        shell.status.code(),
        Some(1),
        "oracle fails the failing suite: {} {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("1 failed"),
        "oracle prints the fail summary: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    check_suite_pair(&shell, &rust, &["test", "-s", "fail"]);
}

#[test]
fn binary_doctor_test_wired_past_interim() {
    // Slice 83: the interim set is empty — every `Command` variant
    // has a dedicated arm in `run`, so no known command may report
    // "not yet implemented" (routing finality is pinned by
    // `dispatch_names_every_shell_arm`, execution parity by the
    // engine rows above). This smoke asserts the diagnostic is gone
    // on the cheapest deterministic rows.
    let home = TempDir::new("cli-wired").expect("twin home");
    let state = TempDir::new("cli-wired-state").expect("twin state");
    for argv in [&["test", "--help"][..], &["test", "--bogus"][..]] {
        let rust = engine_rust(&home, &state, argv, &[]);
        let combined = [rust.stdout.as_slice(), rust.stderr.as_slice()].concat();
        assert!(
            !combined.windows(19).any(|w| w == b"not yet implemented"),
            "argv: {argv:?}",
        );
    }
    let rust = engine_rust(&home, &state, &["doctor"], &[]);
    let combined = [rust.stdout.as_slice(), rust.stderr.as_slice()].concat();
    assert!(
        !combined.windows(19).any(|w| w == b"not yet implemented"),
        "doctor is wired",
    );
}

/// Extract the `_dot_init_usage` heredoc body from the shell source,
/// like [`shell_help`] does for the dispatcher help.
fn shell_init_usage() -> String {
    let source = include_str!("../lib/dot/init-client.sh");
    let marker = "_dot_init_usage() {\n  cat <<'EOF'\n";
    let start = source.find(marker).expect("init usage marker") + marker.len();
    let rest = &source[start..];
    let end = rest.find("\nEOF\n").expect("init usage terminator");
    format!("{}\n", &rest[..end])
}

/// `dot init` under a controlled client: a cleared environment plus a
/// twin home/state pair, so rows never touch the developer's own
/// checkout, provider state, or ambient variables.
fn init_env(cmd: &mut Command, home: &TempDir, state: &TempDir) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    // One `.env` per variable (never `.envs`), matching the oracle
    // convention above.
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", state.path())
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(home.path());
}

/// The Rust binary's `init` with a controlled client.
fn init_bin(home: &TempDir, state: &TempDir) -> Command {
    let mut cmd = bin();
    init_env(&mut cmd, home, state);
    cmd
}

/// The production shell binary (`bin/dot`, under its own
/// `set -euo pipefail`) with the same controlled client: the
/// strongest oracle for the wired arm, comparing process observables
/// end to end rather than function text.
fn shell_dot(argv: &[&str], home: &TempDir, state: &TempDir) -> std::process::Output {
    let mut cmd = Command::new("bash");
    cmd.arg("bin/dot");
    for arg in argv {
        cmd.arg(arg);
    }
    init_env(&mut cmd, home, state);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run bin/dot")
}

/// One wired-arm row: the Rust binary and the production shell agree
/// on exit code and both streams byte for byte.
fn check_init(argv: &[&str]) {
    let home = TempDir::new("cli-init-rust").expect("twin home");
    let state = TempDir::new("cli-init-state").expect("twin state");
    let rust = init_bin(&home, &state)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run dot init");
    let shell = shell_dot(argv, &home, &state);
    assert_eq!(rust.status.code(), shell.status.code(), "argv: {argv:?}");
    assert_eq!(rust.stdout, shell.stdout, "argv: {argv:?}");
    assert_eq!(rust.stderr, shell.stderr, "argv: {argv:?}");
}

#[test]
fn binary_init_help_matches_shell_usage() {
    assert_eq!(
        dot::init_client_adopt::usage(),
        shell_init_usage().into_bytes()
    );
    for argv in [vec!["init", "--help"], vec!["init", "-h"]] {
        check_init(&argv);
    }
    let home = TempDir::new("cli-init-help").expect("twin home");
    let state = TempDir::new("cli-init-help-state").expect("twin state");
    let rust = init_bin(&home, &state)
        .args(["init", "--help"])
        .output()
        .expect("run dot init --help");
    assert_eq!(rust.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(rust.stdout).expect("stdout UTF-8"),
        shell_init_usage()
    );
    assert!(rust.stderr.is_empty());
}

#[test]
fn binary_init_matches_production_on_early_paths() {
    // Parsing, mode gates, the provider gate, and the resolvable
    // failures: none reach the interim closures, so the production
    // shell is the exact oracle — including the errexit-shaped codes
    // (`--bogus` exits `1`, never the dead `return 2`).
    for argv in [
        vec!["init", "--bogus"],
        vec!["init"],
        vec!["init", "--branch"],
        vec!["init", "--status", "some-origin"],
        vec!["init", "--status"],
        vec!["init", "--branch", "main", "notaurl"],
        vec!["init", "--branch", "bad..name", "notaurl"],
    ] {
        check_init(&argv);
    }
}

#[test]
fn binary_init_early_codes_match_production_shape() {
    // Static pins beside the oracle comparison above, so a future
    // drift reads as an explicit contract change, not a silent
    // byte shift.
    let home = TempDir::new("cli-init-codes").expect("twin home");
    let state = TempDir::new("cli-init-codes-state").expect("twin state");
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

/// One stateful wired-arm row: the Rust binary and the production
/// shell agree on exit code and both streams byte for byte, each on
/// its own twin home/state pair (unlike [`check_init`], whose
/// shared pair only suits stateless rows).
fn check_init_twins(argv: &[&str]) {
    let rust_home = TempDir::new("cli-init-rust").expect("rust home");
    let rust_state = TempDir::new("cli-init-rust-state").expect("rust state");
    let shell_home = TempDir::new("cli-init-shell").expect("shell home");
    let shell_state = TempDir::new("cli-init-shell-state").expect("shell state");
    let rust = init_bin(&rust_home, &rust_state)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run dot init");
    let shell = shell_dot(argv, &shell_home, &shell_state);
    assert_eq!(rust.status.code(), shell.status.code(), "argv: {argv:?}");
    assert_eq!(rust.stdout, shell.stdout, "argv: {argv:?}");
    assert_eq!(rust.stderr, shell.stderr, "argv: {argv:?}");
}

#[test]
fn binary_init_rollback_matches_production() {
    // The rollback tree runs the real ports now: refusal rows and
    // the journal-free success row agree with `bin/dot` end to
    // end (rollback never converges, so streams compare exactly).
    check_init_twins(&["init", "--rollback"]);
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

/// Synthetic file:// client for the fetch/push/status/diff wiring
/// rows (slice 82): a legacy-separate base (`$HOME/.dotfiles`, bare,
/// one file:// origin, worktree materialized at `$HOME`) plus one
/// git overlay with a matching descriptor, all under one TempDir
/// scope. Origins and seed clones live beside — never inside — the
/// twin home, so `status` sees only worktree files.
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
            dot::test_support::bash().as_os_str().to_owned(),
        ),
        (OsString::from("TMPDIR"), tmp),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("SHELL"), OsString::from("/bin/sh")),
        (OsString::from("DOT_GIT_REAL"), OsString::from("1")),
        (
            OsString::from("DOT_SOURCE_ROOT"),
            OsString::from(env!("CARGO_MANIFEST_DIR")),
        ),
        (
            OsString::from("DOT_BASE_TOPOLOGY"),
            OsString::from("separate"),
        ),
        (
            OsString::from("DOT_CLIENT_GIT_DIR"),
            client.base_git_dir.as_os_str().to_owned(),
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

/// The shell harness defaults state beneath HOME and keeps Git's launcher
/// cache outside it. Mirror that for the force-fallback oracle.
fn runtime_for_force_fallback(client: &ReposClient) -> dot::app::Runtime {
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

/// Snapshot semantic user/state files. The production shell launcher writes
/// only `dot/bash-v1` bootstrap metadata before dispatch; the Rust binary
/// intentionally does not own that obsolete launcher cache, so this exact
/// one path is classified rather than recreated or broadly normalized.
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

/// Ambient values the old engine accidentally captured or changed. The test
/// never writes them: preserving this snapshot proves two explicit runtimes
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
/// (the `shell_run` convention in tests/repos_commands.rs).
fn repos_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
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
    let status = Command::new("git")
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
    let output = Command::new("git")
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
    let status = Command::new("git")
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

/// Stage the full client: bare file:// origins, a legacy-separate
/// base cloned bare into `$HOME/.dotfiles` (single origin, valid
/// branch, worktree checked out at `$HOME` tracking its origin),
/// and one overlay clone with a matching descriptor under a twin
/// XDG config home (kept outside `$HOME` so status stays clean).
fn stage_repos_client() -> ReposClient {
    let scope = TempDir::new("cli-repos").expect("repos scope");
    let home = scope.path().join("home");
    let xdg = scope.path().join("xdg");
    let origins = scope.path().join("origins");
    std::fs::create_dir_all(&home).expect("twin home");
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
    let status = Command::new("git")
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

/// Controlled environment for the repo wiring rows: a cleared
/// environment plus the twin home/XDG pair, so rows never touch the
/// developer's own checkout or ambient variables. The Rust side
/// additionally receives the shell-computed topology publication
/// (`_dot_client_select` stays shell-owned; see `base_from_env`);
/// the shell side computes it from the fixture itself. One `.env`
/// per variable (never `.envs`), matching the oracle convention.
fn repos_env(cmd: &mut Command, client: &ReposClient, topology: bool) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let shim_cache = PathBuf::from(&tmpdir).join("dot-git-shim-cache");
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("BASH", dot::test_support::bash())
        .env("TMPDIR", &tmpdir)
        // Pin an unknown shell: production always exports `SHELL`
        // (and bash backfills it from the login shell when it does
        // not), but this cleared harness env has none — without the
        // pin the reload hint would read each machine's login shell
        // on the shell side and nothing on the Rust side. `/bin/sh`
        // keeps both twins on the rc-files fallback deterministically.
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
    if topology {
        cmd.env("DOT_BASE_TOPOLOGY", "separate").env(
            "DOT_CLIENT_GIT_DIR",
            client.base_git_dir.to_string_lossy().into_owned(),
        );
    }
}

#[cfg(target_os = "macos")]
fn shell_oracle_command() -> Command {
    let mut cmd = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/lib/dot/public/test-timeout-v1"
    ));
    // The existing timeout supervisor owns, bounds, terminates, and reaps the
    // complete child session. Nested shell workers can therefore inherit that
    // group instead of asking macOS Bash to create another group after a
    // short-lived worker has crossed exec and racing with `setpgid`.
    cmd.arg("120s").arg(dot::test_support::bash());
    cmd
}

#[cfg(not(target_os = "macos"))]
#[test]
fn shell_oracle_uses_no_extra_runtime_dependency() {
    let cmd = shell_oracle_command();
    assert_eq!(cmd.get_program(), dot::test_support::bash());
    assert!(
        cmd.get_envs()
            .all(|(key, _)| key != "DOT_CLEANUP_INHERIT_GROUP")
    );
}

#[cfg(not(target_os = "macos"))]
fn shell_oracle_command() -> Command {
    Command::new(dot::test_support::bash())
}

#[cfg(target_os = "macos")]
fn inherit_supervised_group(cmd: &mut Command) {
    cmd.env("DOT_CLEANUP_INHERIT_GROUP", "1");
}

#[test]
fn repos_twins_disable_optional_git_locks() {
    let client = stage_repos_client();
    let mut shell = Command::new(dot::test_support::bash());
    let mut rust = bin();
    repos_env(&mut shell, &client, false);
    repos_env(&mut rust, &client, true);
    let shell_value = shell
        .get_envs()
        .find(|(key, _)| *key == "GIT_OPTIONAL_LOCKS")
        .and_then(|(_, value)| value);
    let rust_value = rust
        .get_envs()
        .find(|(key, _)| *key == "GIT_OPTIONAL_LOCKS")
        .and_then(|(_, value)| value);
    assert_eq!(shell_value, Some(OsStr::new("0")));
    assert_eq!(rust_value, shell_value);
}

#[cfg(target_os = "macos")]
#[test]
fn shell_oracle_owns_one_inherited_process_group() {
    let mut cmd = shell_oracle_command();
    cmd.args([
        "-c",
        "parent=$(ps -o pgid= -p $$ | tr -d ' '); child=$(bash -c 'ps -o pgid= -p $$ | tr -d \" \"'); printf '%s|%s|%s|%s' \"$DOT_CLEANUP_INHERIT_GROUP\" \"$$\" \"$parent\" \"$child\"",
    ]);
    inherit_supervised_group(&mut cmd);
    let output = cmd.output().expect("run isolated shell oracle probe");
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
    assert_eq!(parent, pid, "oracle shell must lead its group: {text}");
    assert_eq!(child, parent, "child process group: {text}");
    assert!(fields.next().is_none(), "unexpected probe fields: {text}");
}

/// The production shell (`bin/dot` under `set -euo pipefail`) over
/// the fixture: the strongest oracle for the wired arms, comparing
/// process observables end to end.
fn repos_shell(client: &ReposClient, argv: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("bin/dot");
    for arg in argv {
        cmd.arg(arg);
    }
    repos_env(&mut cmd, client, false);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run bin/dot")
}

/// The Rust binary over the same fixture.
fn repos_rust(client: &ReposClient, argv: &[&str]) -> std::process::Output {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    repos_env(&mut cmd, client, true);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run dot binary")
}

/// One wired-arm row: the Rust binary and the production shell agree
/// on exit code and both streams byte for byte.
fn check_repos(client: &ReposClient, argv: &[&str]) {
    let shell = repos_shell(client, argv);
    let rust = repos_rust(client, argv);
    assert_eq!(
        rust.status.code(),
        shell.status.code(),
        "argv: {argv:?}\n shell stdout: {}\n shell stderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert_eq!(rust.stdout, shell.stdout, "argv: {argv:?} stdout");
    assert_eq!(rust.stderr, shell.stderr, "argv: {argv:?} stderr");
}

#[test]
fn repos_status_clean_matches_shell() {
    let client = stage_repos_client();
    let shell = repos_shell(&client, &["status"]);
    assert_eq!(shell.status.code(), Some(0), "oracle status code");
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("==> dotfiles"),
        "oracle sees the base: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("==> alpha dotfiles"),
        "oracle sees the overlay: {}",
        String::from_utf8_lossy(&shell.stdout),
    );
    check_repos(&client, &["status"]);
}

#[test]
fn repos_status_dirty_matches_shell() {
    let client = stage_repos_client();
    // Base: modified tracked file plus one untracked file.
    std::fs::write(client.home.join("tracked.txt"), b"v1-dirty\n").expect("dirty base");
    std::fs::write(client.home.join("new.txt"), b"untracked\n").expect("untracked base");
    // Overlay: modified tracked file.
    std::fs::write(client.overlay.join("tracked.txt"), b"v1-dirty\n").expect("dirty overlay");
    let shell = repos_shell(&client, &["status"]);
    assert_eq!(shell.status.code(), Some(0), "oracle status code");
    check_repos(&client, &["status"]);
}

#[test]
fn repos_status_ahead_behind_matches_shell() {
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
    let shell = repos_shell(&client, &["status"]);
    assert_eq!(shell.status.code(), Some(0), "oracle status code");
    let text = String::from_utf8_lossy(&shell.stdout).into_owned();
    assert!(text.contains("ahead"), "oracle reports ahead: {text}");
    assert!(text.contains("behind"), "oracle reports behind: {text}");
    assert!(
        !text.contains(".dotfiles/index"),
        "fixture must not track its own Git metadata: {text}",
    );
    check_repos(&client, &["status"]);
}

#[test]
fn repos_status_extra_args_forwarded_like_shell() {
    let client = stage_repos_client();
    std::fs::write(client.home.join("tracked.txt"), b"v1-dirty\n").expect("dirty base");
    check_repos(&client, &["status", "--short", "--branch"]);
}

#[test]
fn repos_diff_dirty_matches_shell() {
    let client = stage_repos_client();
    // Base dirty (shows a hunks), overlay clean (header only).
    std::fs::write(client.home.join("tracked.txt"), b"v1\nv2\n").expect("dirty base");
    let shell = repos_shell(&client, &["diff"]);
    assert_eq!(shell.status.code(), Some(0), "oracle diff code");
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("==> dotfiles"),
        "oracle sees the base",
    );
    check_repos(&client, &["diff"]);
    // Both dirty: the overlay section carries its own hunk.
    std::fs::write(client.overlay.join("tracked.txt"), b"v1\nv2\n").expect("dirty overlay");
    check_repos(&client, &["diff"]);
}

#[test]
fn repos_diff_clean_matches_shell() {
    let client = stage_repos_client();
    let shell = repos_shell(&client, &["diff"]);
    assert_eq!(shell.status.code(), Some(0), "oracle diff code");
    // No hunks anywhere: headers only, no git output.
    assert!(shell.stderr.is_empty(), "clean diff is silent on stderr");
    check_repos(&client, &["diff"]);
}

/// Capture one separate-topology `git --git-dir/--work-tree` stdout
/// line, trimmed.
fn repos_prefix_line(git_dir: &Path, work: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
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
fn repos_fetch_matches_shell() {
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
    let shell = repos_shell(&client, &["fetch"]);
    assert_eq!(shell.status.code(), Some(0), "oracle fetch code");
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("From file://"),
        "oracle fetch reports its remotes: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    // Rewind the oracle's fetch so the Rust run sees the same update.
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &[
            "update-ref",
            &format!("refs/remotes/origin/{}", client.base_branch),
            &base_before,
        ],
    );
    repos_git(
        &client.overlay,
        &[
            "update-ref",
            &format!("refs/remotes/origin/{}", client.overlay_branch),
            &overlay_before,
        ],
    );
    let rust = repos_rust(&client, &["fetch"]);
    assert_eq!(rust.status.code(), shell.status.code(), "fetch code");
    assert_eq!(rust.stdout, shell.stdout, "fetch stdout");
    assert_eq!(rust.stderr, shell.stderr, "fetch stderr");
}

#[test]
fn repos_push_matches_shell() {
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
    let shell = repos_shell(&client, &["push"]);
    assert_eq!(shell.status.code(), Some(0), "oracle push code");
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("To file://"),
        "oracle push reports its remotes: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    // Rewind the oracle's push so the Rust run publishes the same update.
    repos_git(
        &client.base_origin,
        &[
            "update-ref",
            &format!("refs/heads/{}", client.base_branch),
            &base_origin_before,
        ],
    );
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &[
            "update-ref",
            &format!("refs/remotes/origin/{}", client.base_branch),
            &base_tracking_before,
        ],
    );
    repos_git(
        &client.overlay_origin,
        &[
            "update-ref",
            &format!("refs/heads/{}", client.overlay_branch),
            &overlay_origin_before,
        ],
    );
    repos_git(
        &client.overlay,
        &[
            "update-ref",
            &format!("refs/remotes/origin/{}", client.overlay_branch),
            &overlay_tracking_before,
        ],
    );
    let rust = repos_rust(&client, &["push"]);
    assert_eq!(rust.status.code(), shell.status.code(), "push code");
    assert_eq!(rust.stdout, shell.stdout, "push stdout");
    assert_eq!(rust.stderr, shell.stderr, "push stderr");
}

#[test]
fn repos_push_rejected_matches_shell() {
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
    let shell = repos_shell(&client, &["push"]);
    assert_eq!(
        shell.status.code(),
        Some(1),
        "rejected base push exits 1 under errexit: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("rejected"),
        "oracle reports the rejection: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    // Rejected pushes mutate nothing, so the oracle output compares directly.
    let rust = repos_rust(&client, &["push"]);
    assert_eq!(rust.status.code(), shell.status.code(), "push code");
    assert_eq!(rust.stdout, shell.stdout, "push stdout");
    assert_eq!(rust.stderr, shell.stderr, "push stderr");
}

#[test]
fn repos_resolve_failure_matches_shell() {
    let client = stage_repos_client();
    // An invalid descriptor fails overlay resolution before any
    // kernel runs: both sides exit 1 with the same diagnostics.
    std::fs::write(
        client.xdg.join("dot/overlays.d/90-bad.conf"),
        b"url=x\nsync=hg\n",
    )
    .expect("bad descriptor");
    let shell = repos_shell(&client, &["status"]);
    assert_eq!(
        shell.status.code(),
        Some(1),
        "oracle resolve-failure code: {}",
        String::from_utf8_lossy(&shell.stderr),
    );
    let rust = repos_rust(&client, &["status"]);
    assert_eq!(rust.status.code(), shell.status.code(), "status code");
    assert_eq!(rust.stdout, shell.stdout, "status stdout");
    assert_eq!(rust.stderr, shell.stderr, "status stderr");
}

#[test]
fn repos_status_missing_topology_matches_shell() {
    // No base repo and no descriptors: resolution succeeds empty
    // and every kernel no-ops, so both sides exit 0 silently. The
    // Rust side exports no topology at all here, pinning the
    // missing default end to end.
    let scope = TempDir::new("cli-repos-empty").expect("empty scope");
    let home = scope.path().join("home");
    let xdg = scope.path().join("xdg");
    std::fs::create_dir_all(&home).expect("twin home");
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut shell_cmd = Command::new(dot::test_support::bash());
    shell_cmd.arg("bin/dot").arg("status");
    shell_cmd
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
    let shell = shell_cmd.output().expect("run bin/dot");
    assert_eq!(shell.status.code(), Some(0), "oracle empty status code");
    assert!(shell.stdout.is_empty(), "oracle empty status is silent");
    let mut rust_cmd = bin();
    rust_cmd.arg("status");
    rust_cmd
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("DOT_GIT_REAL", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rust = rust_cmd.output().expect("run dot binary");
    assert_eq!(rust.status.code(), shell.status.code(), "status code");
    assert_eq!(rust.stdout, shell.stdout, "status stdout");
    assert_eq!(rust.stderr, shell.stderr, "status stderr");
}

#[test]
fn binary_init_fresh_failures_match_production() {
    // Fresh-tail failures before convergence agree with `bin/dot`
    // byte for byte: the missing-repository clone fails silently,
    // and the unknown option keeps its errexit-shaped code.
    check_init_twins(&["init", "--branch", "main", "file:///nonexistent-origin.git"]);
    check_init_twins(&["init", "--bogus"]);
}

/// The Rust binary over the same fixture with the native update driver.
fn repos_rust_native(client: &ReposClient, argv: &[&str]) -> std::process::Output {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    repos_env(&mut cmd, client, true);
    cmd.env(
        "DOT_BASH",
        client.scope.path().join("absent-old-update-engine"),
    );
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run dot binary")
}

/// A native-update client whose former shell engine is poisoned.
///
/// The sentinel is deliberately an executable rather than a missing command:
/// a fallback then leaves an unambiguous byte in stderr instead of looking
/// like an unrelated launcher failure. Native rows must never execute it.
struct NativeUpdateFixture {
    client: ReposClient,
    shell_poison: PathBuf,
}

impl NativeUpdateFixture {
    fn stage() -> Self {
        let client = stage_repos_client();
        let shell_poison = client.scope.path().join("old-update-engine");
        let fixture = Self {
            client,
            shell_poison,
        };
        fixture.break_shell_engine();
        fixture
    }

    /// Install one trusted pre-sync entry point in the fixture's configured
    /// extension root. The shell and native update lanes must relay the hook's
    /// stdout and stderr to their respective update streams.
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
    /// phase one, so an old-engine failure identifies an accidental legacy
    /// branch rather than a missing descriptor or repository error.
    fn with_base_profile(self) -> Self {
        let profiles = self.client.xdg.join("dot/profiles.d");
        std::fs::create_dir_all(&profiles).expect("profile directory");
        std::fs::write(profiles.join("base.conf"), b"version=1\noverlays=alpha\n")
            .expect("base profile");
        self
    }

    /// Create two matching selectors that disagree after the base-only pass.
    /// The update must fail natively and preserve the existing generation;
    /// reaching the poison instead would mean profiles still escaped to Bash.
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
            b"deactivate() { printf retired >\"$HOME/alpha-retired\"; }\n",
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

    fn break_shell_engine(&self) {
        std::fs::write(
            &self.shell_poison,
            b"#!/bin/sh\nprintf 'OLD-UPDATE-ENGINE\n' >&2\nexit 97\n",
        )
        .expect("write shell poison");
        #[cfg(unix)]
        std::fs::set_permissions(&self.shell_poison, std::fs::Permissions::from_mode(0o755))
            .expect("mark shell poison executable");
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
        repos_env(&mut cmd, &self.client, true);
        if self.shell_poison.exists() {
            cmd.env("DOT_BASH", &self.shell_poison);
        }
        configure(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().expect("run native update")
    }

    fn shell_dot_with(
        &self,
        argv: &[&str],
        configure: impl FnOnce(&mut Command),
    ) -> std::process::Output {
        let mut cmd = shell_oracle_command();
        cmd.arg("bin/dot");
        for arg in argv {
            cmd.arg(arg);
        }
        repos_env(&mut cmd, &self.client, false);
        #[cfg(target_os = "macos")]
        inherit_supervised_group(&mut cmd);
        configure(&mut cmd);
        cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().expect("run shell update")
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
fn update_native_entry_edges_do_not_invoke_shell_engine() {
    // Each row poisons only update's former shell engine. A passing assertion is
    // therefore native evidence, not an accidentally-green shell oracle.
    let clean = NativeUpdateFixture::stage();
    clean.break_shell_engine();
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
    mtime.break_shell_engine();
    assert_native_silent(&mtime.rust_dot(&["update", "--cron"]), "mtime cron");

    let unresolved = NativeUpdateFixture::stage();
    std::fs::write(unresolved.client.home.join("tracked.txt"), b"local edit\n")
        .expect("make unresolved edit");
    unresolved.break_shell_engine();
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
    force.break_shell_engine();
    assert_native_silent(
        &force.rust_dot(&["update", "--quiet", "--force"]),
        "force with provider none",
    );

    let frozen = NativeUpdateFixture::stage();
    frozen.break_shell_engine();
    assert_native_silent(
        &frozen.rust_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("DOT_OVERLAY_LINKS_FROZEN", "1");
        }),
        "stale frozen marker",
    );
}

#[test]
fn update_native_invalid_home_never_runs_shell_engine() {
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
        fixture.break_shell_engine();
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
    // validated its one-use context. Poisoning the old update engine makes a
    // fallback unmistakable while the marker proves the hook actually ran.
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
    fixture.break_shell_engine();
    let output = fixture.rust_dot(&["update", "--quiet"]);
    assert_native_silent(&output, "configured pre-sync hook");
    assert_eq!(
        std::fs::read(fixture.client.home.join("pre-sync-ran")).expect("pre-sync marker"),
        b"prepared"
    );
}

#[test]
fn update_native_pre_sync_success_streams_match_shell() {
    let shell = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { printf 'pre-sync stdout\\n'; printf 'pre-sync stderr\\n' >&2; }\n",
    );
    let native = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { printf 'pre-sync stdout\\n'; printf 'pre-sync stderr\\n' >&2; }\n",
    );

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(shell_run.status.code(), Some(0), "shell pre-sync success");
    assert!(has_bytes(&shell_run.stdout, b"pre-sync stdout\n"));
    assert!(has_bytes(&shell_run.stderr, b"pre-sync stderr\n"));
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_update_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "successful pre-sync",
    );
}

#[test]
fn update_native_pre_sync_failure_streams_match_shell() {
    let shell = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { printf 'pre-sync stdout\\n'; printf 'pre-sync stderr\\n' >&2; return 7; }\n",
    );
    let native = NativeUpdateFixture::stage().with_pre_sync(
        b"prepare() { printf 'pre-sync stdout\\n'; printf 'pre-sync stderr\\n' >&2; return 7; }\n",
    );

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(shell_run.status.code(), Some(1), "shell pre-sync failure");
    assert!(has_bytes(&shell_run.stdout, b"pre-sync stdout\n"));
    assert!(has_bytes(&shell_run.stderr, b"pre-sync stderr\n"));
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_update_pair(&shell_run, &native_run, &shell, &native, "failed pre-sync");
}

#[test]
fn update_native_merge_hook_matches_shell_without_the_legacy_adapter() {
    // This catches reinstating a legacy merge-hook branch: the shell oracle must
    // run one actual hook, while the native half has no usable old engine.
    let shell = NativeUpdateFixture::stage()
        .with_merge(b"merge() { printf merged >\"$HOME/merge-hook-ran\"; }\n");
    let native = NativeUpdateFixture::stage()
        .with_merge(b"merge() { printf merged >\"$HOME/merge-hook-ran\"; }\n");

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |cmd| {
        cmd.env("DOT_MERGE_JOBS", "1000000000");
    });
    assert_eq!(shell_run.status.code(), Some(0), "shell merge-hook status");
    assert_eq!(
        std::fs::read(shell.client.home.join("merge-hook-ran")).expect("shell merge marker"),
        b"merged",
        "shell merge hook ran"
    );
    native.break_shell_engine();
    let native_run = native.rust_dot_with(&["update", "--quiet"], |cmd| {
        cmd.env("DOT_MERGE_JOBS", "1000000000");
    });

    assert_update_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "merge-hook success",
    );
    assert_eq!(
        std::fs::read(native.client.home.join("merge-hook-ran")).expect("native merge marker"),
        b"merged",
        "native merge hook ran"
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
    let shell = NativeUpdateFixture::stage().with_merge_files(hooks);
    let native = NativeUpdateFixture::stage().with_merge_files(hooks);

    let shell_run = shell.shell_dot_with(&["update", "--verbose"], |cmd| {
        // Force a two-worker batch even on a one-core runner. The serial hook
        // proves the whole batch joined before its barrier starts.
        cmd.env("DOT_UPDATE_JOBS", "1");
        cmd.env("DOT_MERGE_JOBS", "2");
    });
    assert_eq!(
        shell_run.status.code(),
        Some(0),
        "shell verbose merge status"
    );
    assert!(has_bytes(&shell_run.stdout, b"Alpha result"));
    assert!(has_bytes(&shell_run.stdout, b"Beta result"));
    assert!(has_bytes(&shell_run.stdout, b"Gamma result"));
    assert!(has_bytes(&shell_run.stdout, b"Barrier result"));
    assert!(has_bytes(&shell_run.stdout, b"Delta result"));
    native.break_shell_engine();
    let native_run = native.rust_dot_with(&["update", "--verbose"], |cmd| {
        cmd.env("DOT_UPDATE_JOBS", "1");
        cmd.env("DOT_MERGE_JOBS", "2");
    });

    assert_eq!(native_run.status.code(), shell_run.status.code());
    let shell_stdout = scrub_twin(
        &scrub_merge_durations(&shell_run.stdout),
        shell.client.scope.path(),
    );
    let native_stdout = scrub_twin(
        &scrub_merge_durations(&native_run.stdout),
        native.client.scope.path(),
    );
    assert_eq!(native_stdout, shell_stdout, "verbose ordered merge stdout");
    for output in [&shell_stdout, &native_stdout] {
        let positions = [
            "Alpha result",
            "Beta result",
            "Gamma result",
            "Barrier result",
            "Delta result",
        ]
        .map(|label| {
            output
                .windows(label.len())
                .position(|window| window == label.as_bytes())
                .expect("merge result label")
        });
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "parallel captures replay in declaration order before and after the serial barrier: {output:?}"
        );
    }
    assert_eq!(
        scrub_twin(&native_run.stderr, native.client.scope.path()),
        scrub_twin(&shell_run.stderr, shell.client.scope.path()),
        "verbose ordered merge stderr",
    );
    for fixture in [&shell, &native] {
        assert_eq!(
            std::fs::read(fixture.client.home.join("delta-done")).expect("post-barrier marker"),
            b"delta",
            "post-barrier hook ran after the serial hook"
        );
    }
}

#[test]
fn update_native_verbose_merge_preserves_non_utf8_output() {
    let script = b"merge() { printf 'Binary '; printf '\\377'; printf 'A\\0B\\n'; }\n";
    let shell = NativeUpdateFixture::stage().with_merge(script);
    let native = NativeUpdateFixture::stage().with_merge(script);

    let shell_run = shell.shell_dot_with(&["update", "--verbose"], |_| {});
    assert_eq!(
        shell_run.status.code(),
        Some(0),
        "shell binary-output status"
    );
    assert!(
        shell_run.stdout.contains(&0xff),
        "shell oracle preserves the non-UTF-8 byte"
    );
    assert!(
        !shell_run.stdout.contains(&0),
        "shell variables discard NUL bytes while parsing hook output"
    );
    assert!(has_bytes(&shell_run.stdout, b"AB"));
    native.break_shell_engine();
    let native_run = native.rust_dot(&["update", "--verbose"]);

    assert_eq!(native_run.status.code(), shell_run.status.code());
    assert_eq!(
        scrub_twin(
            &scrub_merge_durations(&native_run.stdout),
            native.client.scope.path(),
        ),
        scrub_twin(
            &scrub_merge_durations(&shell_run.stdout),
            shell.client.scope.path(),
        ),
        "verbose binary merge stdout",
    );
    assert_eq!(
        scrub_twin(&native_run.stderr, native.client.scope.path()),
        scrub_twin(&shell_run.stderr, shell.client.scope.path()),
        "verbose binary merge stderr",
    );
    assert!(
        native_run.stdout.contains(&0xff),
        "native replay preserves the non-UTF-8 byte"
    );
    assert!(
        !native_run.stdout.contains(&0),
        "native replay matches Bash variable NUL handling"
    );
}

#[test]
fn update_native_merge_without_an_entry_point_matches_shell_failure() {
    // A helper-looking `*.sh` is discovered, but the worker writes a zero
    // merge record and exits nonzero. It must still fail the aggregate stage.
    let shell = NativeUpdateFixture::stage().with_merge(b"helper() { :; }\n");
    let native = NativeUpdateFixture::stage().with_merge(b"helper() { :; }\n");

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(
        shell_run.status.code(),
        Some(1),
        "shell missing-entry status"
    );
    native.break_shell_engine();
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_update_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "missing merge entry point",
    );
}

#[test]
fn update_native_unsafe_merge_hook_refusal_matches_shell() {
    // Discovery must reject an unsafe entry point before either engine runs it;
    // poisoning the old engine still proves native handling of that refusal.
    let shell = NativeUpdateFixture::stage().with_merge(b"merge() { :; }\n");
    let native = NativeUpdateFixture::stage().with_merge(b"merge() { :; }\n");
    for fixture in [&shell, &native] {
        std::fs::set_permissions(
            fixture
                .client
                .home
                .join("extensions/merge-hooks.d/10-config.sh"),
            std::fs::Permissions::from_mode(0o666),
        )
        .expect("make merge hook unsafe");
    }

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(shell_run.status.code(), Some(1), "shell unsafe-hook status");
    assert!(has_bytes(&shell_run.stderr, b"unsafe merge hook"));
    native.break_shell_engine();
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_update_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "unsafe merge hook",
    );
}

#[test]
fn update_native_failed_merge_hook_matches_shell() {
    // Failed hooks retain their ordered capture for a non-verbose update and
    // still make the aggregate Configs stage fail.
    let script = b"merge() { printf 'merge stdout\\n'; printf 'merge stderr\\n' >&2; return 7; }\n";
    let shell = NativeUpdateFixture::stage().with_merge(script);
    let native = NativeUpdateFixture::stage().with_merge(script);

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(
        shell_run.status.code(),
        Some(1),
        "shell merge failure status"
    );
    assert!(has_bytes(&shell_run.stderr, b"10-config output:"));
    assert!(has_bytes(&shell_run.stderr, b"merge stdout\n"));
    assert!(has_bytes(&shell_run.stderr, b"merge stderr\n"));
    native.break_shell_engine();
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_update_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "failed merge hook",
    );
}

#[test]
fn update_native_merge_context_matches_shell() {
    // Merge hooks receive the one-use active-overlay context used by Bash;
    // this oracle observes the values from inside the actual worker.
    let script = b"merge() {\n  [[ $REPLY_SET_KIND == active && $REPLY_STAGE == none && ${#OVERLAYS[@]} -eq 1 && ${OVERLAYS[0]} == alpha\\|* ]] || return 8\n  printf '%s:%s:%s' \"$REPLY_SET_KIND\" \"$REPLY_STAGE\" \"${OVERLAYS[0]%%|*}\" >\"$HOME/merge-context\"\n}\n";
    let shell = NativeUpdateFixture::stage().with_merge(script);
    let native = NativeUpdateFixture::stage().with_merge(script);

    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(
        shell_run.status.code(),
        Some(0),
        "shell merge context status"
    );
    assert_eq!(
        std::fs::read(shell.client.home.join("merge-context")).expect("shell merge context"),
        b"active:none:alpha"
    );
    native.break_shell_engine();
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_update_pair(&shell_run, &native_run, &shell, &native, "merge context");
    assert_eq!(
        std::fs::read(native.client.home.join("merge-context")).expect("native merge context"),
        b"active:none:alpha"
    );
}

#[test]
fn update_native_profile_base_selection_does_not_invoke_shell_engine() {
    // A native profile run must complete with the former shell engine
    // impossible to invoke; the poison makes any regression unmistakable.
    let fixture = NativeUpdateFixture::stage().with_base_profile();
    fixture.break_shell_engine();
    assert_native_silent(
        &fixture.rust_dot(&["update", "--quiet"]),
        "profile base selection",
    );
}

#[test]
fn update_native_profile_addition_discovered_after_base_pull_stays_native() {
    let fixture = NativeUpdateFixture::stage().with_base_discovered_profile_addition();
    fixture.break_shell_engine();
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
fn update_native_profile_selector_conflict_does_not_invoke_shell_engine() {
    // This is the post-base selector phase: both selectors are valid and
    // trusted, but their different profiles must freeze the generation with a
    // profile error rather than fall back to the legacy update engine.
    let fixture = NativeUpdateFixture::stage().with_conflicting_profile_selectors();
    fixture.break_shell_engine();
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
            .windows(b"OLD-UPDATE-ENGINE".len())
            .any(|window| { window == b"OLD-UPDATE-ENGINE" })
    );
}

#[test]
fn update_native_profile_downgrade_retires_lifecycle_state_without_shell_engine() {
    // First establish alpha's lifecycle authority. Then switch the persisted
    // default profile to beta: the native two-phase driver must link beta,
    // execute alpha's trusted deactivation hook, and commit the emptied ledger.
    let fixture = NativeUpdateFixture::stage().with_profile_retirement();
    fixture.break_shell_engine();
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
        b"retired"
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
fn update_native_profile_failed_retirement_matches_shell_twin() {
    let shell = NativeUpdateFixture::stage().with_profile_retirement();
    let native = NativeUpdateFixture::stage().with_profile_retirement();
    // The twin stays an end-to-end oracle while making a native fallback
    // unmistakable. The versioned lifecycle worker uses Runtime's absolute
    // BASH, not a legacy update-engine selector.
    native.break_shell_engine();
    assert_profile_twin(&shell, &native, "failed retirement setup");

    for fixture in [&shell, &native] {
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
    }
    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    assert_eq!(
        shell_run.status.code(),
        Some(1),
        "shell failed-retirement status"
    );
    assert!(
        String::from_utf8_lossy(&shell_run.stderr).contains("profile deactivation failed: alpha"),
        "shell failed-retirement stderr: {}",
        String::from_utf8_lossy(&shell_run.stderr)
    );
    assert!(
        shell.client.home.join(".dotfiles-beta/.git").is_dir(),
        "shell finalization reaches the selected beta generation before retirement fails"
    );
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_profile_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "failed retirement",
    );
    for fixture in [&shell, &native] {
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
    }
    assert_profile_tree_twin(
        &shell,
        &native,
        ".config/profile",
        &["alpha", "beta"],
        "failed retirement",
    );
}

#[test]
fn update_native_profile_conflict_after_base_pull_restores_prior_generation() {
    let fixture = NativeUpdateFixture::stage().with_base_profile_rollback();
    fixture.break_shell_engine();
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
            .windows(b"OLD-UPDATE-ENGINE".len())
            .any(|row| row == b"OLD-UPDATE-ENGINE"),
        "profile conflict must not escape to the former shell engine"
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

/// Compare the process contract of independently staged shell and native
/// updates. Scope-specific paths and elapsed stamps normalize only after each
/// lane's exit status is established.
fn assert_update_pair(
    shell: &std::process::Output,
    native: &std::process::Output,
    shell_fixture: &NativeUpdateFixture,
    native_fixture: &NativeUpdateFixture,
    label: &str,
) {
    assert_eq!(
        native.status.code(),
        shell.status.code(),
        "{label} status\nshell stdout: {}\nshell stderr: {}\nnative stdout: {}\nnative stderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(
        scrub_twin(&native.stdout, native_fixture.client.scope.path()),
        scrub_twin(&shell.stdout, shell_fixture.client.scope.path()),
        "{label} stdout",
    );
    assert_eq!(
        scrub_twin(&native.stderr, native_fixture.client.scope.path()),
        scrub_twin(&shell.stderr, shell_fixture.client.scope.path()),
        "{label} stderr",
    );
}

/// A profile update also owns mutable manifest and lifecycle records, so its
/// oracle extends the process contract with those persisted generations.
fn assert_profile_pair(
    shell: &std::process::Output,
    native: &std::process::Output,
    shell_fixture: &NativeUpdateFixture,
    native_fixture: &NativeUpdateFixture,
    label: &str,
) {
    assert_update_pair(shell, native, shell_fixture, native_fixture, label);
    for relative in [
        ".local/state/dot/overlay-links",
        ".local/state/dot/profile-overlay-lifecycle-v1",
    ] {
        let shell_path = shell_fixture.client.home.join(relative);
        let native_path = native_fixture.client.home.join(relative);
        assert_eq!(
            native_path.exists(),
            shell_path.exists(),
            "{label} {relative} presence",
        );
        if shell_path.exists() {
            assert_eq!(
                scrub_scope(
                    &std::fs::read(&native_path).expect("native state"),
                    native_fixture.client.scope.path()
                ),
                scrub_scope(
                    &std::fs::read(&shell_path).expect("shell state"),
                    shell_fixture.client.scope.path()
                ),
                "{label} {relative}",
            );
        }
    }
}

fn assert_profile_twin(
    shell_fixture: &NativeUpdateFixture,
    native_fixture: &NativeUpdateFixture,
    label: &str,
) {
    let shell = repos_shell(&shell_fixture.client, &["update", "--quiet"]);
    let native = native_fixture.rust_dot(&["update", "--quiet"]);
    assert_profile_pair(&shell, &native, shell_fixture, native_fixture, label);
}

#[derive(Debug, PartialEq, Eq)]
struct CheckoutSnapshot {
    head_at_upstream: bool,
    porcelain: Vec<u8>,
}

fn git_base_output(client: &ReposClient, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
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
    let output = Command::new("git")
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
    // The separate bare Git directory, checkouts, and shell-only runtime
    // stamps all live under this synthetic worktree. Compare tracked changes
    // only: that is the Git state profile convergence owns.
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

fn assert_profile_tree_twin(
    shell_fixture: &NativeUpdateFixture,
    native_fixture: &NativeUpdateFixture,
    relative: &str,
    checkouts: &[&str],
    label: &str,
) {
    let shell = shell_fixture.client.home.join(relative);
    let native = native_fixture.client.home.join(relative);
    assert_eq!(
        normalized_managed_tree(&native, native_fixture.client.scope.path()),
        normalized_managed_tree(&shell, shell_fixture.client.scope.path()),
        "{label} managed tree"
    );
    assert_eq!(
        base_snapshot(&native_fixture.client),
        base_snapshot(&shell_fixture.client),
        "{label} base checkout"
    );
    for name in checkouts {
        assert_eq!(
            checkout_snapshot(&profile_checkout(&native_fixture.client, name)),
            checkout_snapshot(&profile_checkout(&shell_fixture.client, name)),
            "{label} {name} checkout"
        );
    }
}

#[test]
fn update_native_profile_selection_matches_shell_twin() {
    // This is deliberately not an old-engine poison test: each implementation
    // gets an independently staged client, then we compare process behavior
    // and the profile generation it published.
    let shell = NativeUpdateFixture::stage().with_base_profile();
    let native = NativeUpdateFixture::stage().with_base_profile();

    assert_profile_twin(&shell, &native, "base profile selection");
    assert_profile_tree_twin(
        &shell,
        &native,
        ".config/profile",
        &["alpha"],
        "base profile selection",
    );
}

#[test]
fn update_native_profile_selector_conflict_matches_shell_twin() {
    // An equally-specific conflict is a semantic profile error.  The native
    // lane must preserve the shell's failure code and diagnostic rather than
    // merely reporting any native failure.
    let shell = NativeUpdateFixture::stage().with_conflicting_profile_selectors();
    let native = NativeUpdateFixture::stage().with_conflicting_profile_selectors();

    assert_profile_twin(&shell, &native, "selector conflict");
    assert_profile_tree_twin(
        &shell,
        &native,
        ".config/profile",
        &["alpha"],
        "selector conflict",
    );
}

#[test]
fn update_native_profile_descriptor_refresh_matches_shell_twin() {
    // The base refresh changes alpha's descriptor before it selects beta.  A
    // full-record difference must not make alpha an additions-only pull; the
    // shell and native outputs plus generated state remain the oracle.
    let shell = NativeUpdateFixture::stage().with_base_discovered_profile_addition();
    let native = NativeUpdateFixture::stage().with_base_discovered_profile_addition();
    let shell_run = shell.shell_dot_with(&["update"], |cmd| {
        cmd.env("XDG_CONFIG_HOME", shell.client.home.join(".config"));
    });
    let native_run = native.rust_dot_with(&["update"], |cmd| {
        cmd.env("XDG_CONFIG_HOME", native.client.home.join(".config"));
    });

    assert_profile_pair(
        &shell_run,
        &native_run,
        &shell,
        &native,
        "descriptor refresh",
    );
    assert_profile_tree_twin(
        &shell,
        &native,
        ".config/profile",
        &["alpha", "beta"],
        "descriptor refresh",
    );
}

#[test]
fn update_native_profile_retirement_matches_shell_twin() {
    let shell = NativeUpdateFixture::stage().with_profile_retirement();
    let native = NativeUpdateFixture::stage().with_profile_retirement();
    assert_profile_twin(&shell, &native, "retirement setup");

    for fixture in [&shell, &native] {
        std::fs::write(
            fixture.client.xdg.join("dot/config"),
            b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndefault_profile=dev\n",
        )
        .expect("switch profile");
    }
    let shell_run = shell.shell_dot_with(&["update", "--quiet"], |_| {});
    let native_run = native.rust_dot(&["update", "--quiet"]);

    assert_profile_pair(&shell_run, &native_run, &shell, &native, "retirement");
    assert_eq!(
        std::fs::read(native.client.home.join("alpha-retired")).expect("native retirement marker"),
        std::fs::read(shell.client.home.join("alpha-retired")).expect("shell retirement marker"),
        "retirement hook result"
    );
    assert_eq!(
        native.client.home.join(".dotfiles-beta/.git").is_dir(),
        shell.client.home.join(".dotfiles-beta/.git").is_dir(),
        "retirement beta checkout"
    );
    assert_profile_tree_twin(
        &shell,
        &native,
        ".config/profile",
        &["alpha", "beta"],
        "retirement",
    );
}

#[test]
fn update_native_profile_rollback_matches_shell_twin() {
    let shell = NativeUpdateFixture::stage().with_base_profile_rollback();
    let native = NativeUpdateFixture::stage().with_base_profile_rollback();
    let run_shell = || {
        shell.shell_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("XDG_CONFIG_HOME", shell.client.home.join(".config"));
        })
    };
    let run_native = || {
        native.rust_dot_with(&["update", "--quiet"], |cmd| {
            cmd.env("XDG_CONFIG_HOME", native.client.home.join(".config"));
        })
    };
    let shell_setup = run_shell();
    let native_setup = run_native();
    assert_profile_pair(
        &shell_setup,
        &native_setup,
        &shell,
        &native,
        "rollback setup",
    );

    let user = dot::profiles::current_user().expect("current user");
    for fixture in [&shell, &native] {
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
    }
    let shell_conflict = run_shell();
    let native_conflict = run_native();

    assert_profile_pair(
        &shell_conflict,
        &native_conflict,
        &shell,
        &native,
        "rollback conflict",
    );
    assert_profile_tree_twin(
        &shell,
        &native,
        ".config/profile",
        &["alpha"],
        "rollback conflict",
    );
}

/// Scrub one twin's temp scope (every home, XDG, origin, and state
/// path lives under it) so twin outputs compare on behavior, not
/// machine paths.
fn scrub_scope(bytes: &[u8], scope: &std::path::Path) -> Vec<u8> {
    String::from_utf8_lossy(bytes)
        .replace(&scope.to_string_lossy().into_owned(), "@SCOPE@")
        .into_bytes()
}

fn has_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|row| row == needle)
}

/// Scrub update elapsed stamps (trailing `0s`, `1s`) after
/// asserting each is sane: wall-clock jitter between twins is
/// expected, but a garbage stamp (an unstarted stage clock) must
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

/// Worker durations are per-process timing fields, so verbose merge twins
/// compare their rendered shape after preserving the rest of each row exactly.
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

/// Twin outputs compared on behavior: scope paths, then elapsed
/// stamps, scrubbed identically on both sides.
fn scrub_twin(bytes: &[u8], scope: &std::path::Path) -> Vec<u8> {
    scrub_update_elapsed(&scrub_scope(bytes, scope))
}

#[test]
fn update_twin_duration_normalizers_cover_slow_ci_formats() {
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
fn update_native_matches_shell_byte_for_byte() {
    // Twin staged clients (base plus one overlay, both current):
    // the shell side runs the oracle engine and the Rust side the native
    // driver. Pulls are no-ops, so the run exercises the
    // deferred close with real counts, discovery, the link phase,
    // retire, the empty merges close, commit, and normalize.
    let shell_client = stage_repos_client();
    let shell = repos_shell(&shell_client, &["update"]);
    assert_eq!(
        shell.status.code(),
        Some(0),
        "oracle update code\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    let native_client = stage_repos_client();
    let native = repos_rust_native(&native_client, &["update"]);
    assert_eq!(
        native.status.code(),
        shell.status.code(),
        "update code\nshell stdout: {}\nshell stderr: {}\nnative stdout: {}\nnative stderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(
        scrub_twin(&native.stdout, native_client.scope.path()),
        scrub_twin(&shell.stdout, shell_client.scope.path()),
        "update stdout\ncodes: native={} shell={}\nshell stderr: {}\nnative stderr: {}",
        native.status.code().unwrap_or(-1),
        shell.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&shell.stderr),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(
        scrub_twin(&native.stderr, native_client.scope.path()),
        scrub_twin(&shell.stderr, shell_client.scope.path()),
        "update stderr",
    );
}

#[test]
fn update_native_failure_matches_shell_byte_for_byte() {
    // Twin staged clients with a broken base origin: the base
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
    let shell_client = stage_repos_client();
    break_origin(&shell_client);
    let shell = repos_shell(&shell_client, &["update"]);
    assert_ne!(
        shell.status.code(),
        Some(0),
        "oracle update must fail\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
    );
    let native_client = stage_repos_client();
    break_origin(&native_client);
    let native = repos_rust_native(&native_client, &["update"]);
    assert_eq!(
        native.status.code(),
        shell.status.code(),
        "failed update code\nshell stdout: {}\nshell stderr: {}\nnative stdout: {}\nnative stderr: {}",
        String::from_utf8_lossy(&shell.stdout),
        String::from_utf8_lossy(&shell.stderr),
        String::from_utf8_lossy(&native.stdout),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(
        scrub_twin(&native.stdout, native_client.scope.path()),
        scrub_twin(&shell.stdout, shell_client.scope.path()),
        "failed update stdout\ncodes: native={} shell={}\nshell stderr: {}\nnative stderr: {}",
        native.status.code().unwrap_or(-1),
        shell.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&shell.stderr),
        String::from_utf8_lossy(&native.stderr),
    );
    assert_eq!(
        scrub_twin(&native.stderr, native_client.scope.path()),
        scrub_twin(&shell.stderr, shell_client.scope.path()),
        "failed update stderr",
    );
}
