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

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::cli::{Command as Decision, dispatch, init_acquires_lock};
use dot::test_support::TempDir;

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
fn update_applies_flag_exports_before_pending_engine() {
    // Slice 78 drives `Command::Update` through the sequencer's flag
    // parser: the shell loop's exports land in the process
    // environment, while the engine (sync/finalize) stays
    // shell-owned, so the interim diagnostic and generic-failure
    // code remain. Process env is shared with sibling threads, so
    // the case captures every touched variable, then restores the
    // entry state before asserting.
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
    // (argv, expected exports): `None` reads as unset.
    type FlagCase<'a> = (&'a [&'a str], &'a [(&'a str, Option<&'a str>)]);
    let cases: &[FlagCase<'_>] = &[
        (
            &["update", "--cron"],
            &[
                ("DOT_QUIET", Some("1")),
                ("SHDEPS_QUIET", Some("1")),
                ("DOT_FORCE", None),
                ("SHDEPS_FORCE", None),
                ("DOT_VERBOSE", None),
                ("SHDEPS_LOG_LEVEL", None),
            ],
        ),
        (
            &["pull", "-f", "--verbose"],
            &[
                ("DOT_QUIET", None),
                ("SHDEPS_QUIET", None),
                ("DOT_FORCE", Some("1")),
                ("SHDEPS_FORCE", Some("1")),
                ("DOT_VERBOSE", Some("1")),
                ("SHDEPS_LOG_LEVEL", Some("2")),
            ],
        ),
        (
            &["update", "--quiet", "-x"],
            &[
                ("DOT_QUIET", Some("1")),
                ("SHDEPS_QUIET", Some("1")),
                ("DOT_FORCE", None),
                ("SHDEPS_FORCE", None),
                ("DOT_VERBOSE", None),
                ("SHDEPS_LOG_LEVEL", None),
            ],
        ),
    ];
    for (argv, expected) in cases {
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            std::env::set_var("DOT_OVERLAY_LINKS_FROZEN", "1");
        }
        let owned: Vec<OsString> = argv.iter().map(OsString::from).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(owned, &mut out, &mut err);
        let observed: Vec<(&str, Option<OsString>)> = expected
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        let frozen = std::env::var_os("DOT_OVERLAY_LINKS_FROZEN");
        restore();
        assert_eq!(code, 1, "argv: {argv:?}");
        assert!(out.is_empty(), "argv: {argv:?}");
        assert_eq!(
            err,
            format!("dot: command '{}' is not yet implemented\n", argv[0]).into_bytes(),
            "argv: {argv:?}"
        );
        for ((key, want), (_, got)) in expected.iter().zip(observed) {
            assert_eq!(
                got.as_deref(),
                want.map(OsString::from).as_deref(),
                "argv: {argv:?} var: {key}"
            );
        }
        // The sequencer clears rollback authority on entry, like the
        // shell's `unset DOT_OVERLAY_LINKS_FROZEN`.
        assert_eq!(frozen, None, "argv: {argv:?} frozen link generation");
    }
    restore();
}

#[test]
fn binary_kernel_arms_report_not_implemented() {
    // Interim contract until each kernel slice lands: known commands
    // exit generic-failure with their own diagnostic — never the
    // unknown-command one, and never success. (`init` left this set
    // when its slice wired it; see the `binary_init_*` tests below.)
    for command in [
        "update", "pull", "fetch", "push", "status", "diff", "doctor", "test",
    ] {
        let rust = bin().arg(command).output().expect("run dot command");
        assert_eq!(rust.status.code(), Some(1), "command: {command}");
        assert!(rust.stdout.is_empty(), "command: {command}");
        assert_eq!(
            rust.stderr,
            format!("dot: command '{command}' is not yet implemented\n").into_bytes(),
            "command: {command}"
        );
    }
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

#[test]
fn binary_init_rollback_names_the_gap() {
    // The deep rollback tree is the next slice: the interim closure
    // names the gap with the legacy dispatcher-level text, keeping
    // the kernel's failure code per the production contract.
    let home = TempDir::new("cli-init-gap").expect("twin home");
    let state = TempDir::new("cli-init-gap-state").expect("twin state");
    let rust = init_bin(&home, &state)
        .args(["init", "--rollback"])
        .output()
        .expect("run dot init --rollback");
    assert_eq!(rust.status.code(), Some(1));
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: initialization rollback is not yet implemented\n".to_vec()
    );
}
