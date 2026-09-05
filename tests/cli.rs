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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
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
fn update_applies_flag_exports_before_engine() {
    // Slice 80 runs `Command::Update` end to end: the shell loop's
    // exports land in the process environment (via the sequencer's
    // flag parser), then the engine runs for real — exit `0` on the
    // empty-HOME fixture, never the interim diagnostic. The engine
    // reads the ambient client, so the case redirects HOME, state,
    // and config at an isolated pair first (never the developer's
    // own checkout). Process env is shared with sibling threads, so
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
    // (argv, expected exports, quiet run): `None` reads as unset.
    type FlagCase<'a> = (&'a [&'a str], &'a [(&'a str, Option<&'a str>)], bool);
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
            true,
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
            false,
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
            true,
        ),
    ];
    for (argv, expected, quiet) in cases {
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
        let code = run(owned, &mut out, &mut err);
        let observed: Vec<(&str, Option<OsString>)> = expected
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        let frozen = std::env::var_os("DOT_OVERLAY_LINKS_FROZEN");
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
        if *quiet {
            assert!(out.is_empty(), "argv: {argv:?}");
        } else {
            assert!(
                out.windows(17).any(|w| w == b"Reload your shell"),
                "argv: {argv:?}"
            );
        }
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
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(&client.home);
    if topology {
        cmd.env("DOT_BASE_TOPOLOGY", "separate").env(
            "DOT_CLIENT_GIT_DIR",
            client.base_git_dir.to_string_lossy().into_owned(),
        );
    }
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
    // Overlay falls behind its origin (fetch refreshes the
    // remote-tracking ref so plain `status` reports behind).
    seed_advance(&client.overlay_seed, "tracked.txt", b"v1-origin\n");
    repos_git(&client.overlay, &["fetch", "-q", "origin"]);
    let shell = repos_shell(&client, &["status"]);
    assert_eq!(shell.status.code(), Some(0), "oracle status code");
    let text = String::from_utf8_lossy(&shell.stdout).into_owned();
    assert!(text.contains("ahead"), "oracle reports ahead: {text}");
    assert!(text.contains("behind"), "oracle reports behind: {text}");
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

/// The Rust binary over the same fixture with the native update
/// driver opted in (`DOT_UPDATE_NATIVE=1`): the flag flips to
/// default once every envelope lane proves out the same way.
fn repos_rust_native(client: &ReposClient, argv: &[&str]) -> std::process::Output {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    repos_env(&mut cmd, client, true);
    cmd.env("DOT_UPDATE_NATIVE", "1");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("run dot binary")
}

/// Scrub one twin's temp scope (every home, XDG, origin, and state
/// path lives under it) so twin outputs compare on behavior, not
/// machine paths.
fn scrub_scope(bytes: &[u8], scope: &std::path::Path) -> Vec<u8> {
    String::from_utf8_lossy(bytes)
        .replace(&scope.to_string_lossy().into_owned(), "@SCOPE@")
        .into_bytes()
}

/// Scrub update elapsed stamps (trailing `0s`, `1s`) after
/// asserting each is sane: wall-clock jitter between twins is
/// expected, but a garbage stamp (an unstarted stage clock) must
/// still fail loudly. (The suite `(Ns)` marks use the existing
/// [`scrub_elapsed`] instead.)
fn scrub_update_elapsed(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut scrubbed = String::with_capacity(text.len());
    let mut digits = String::new();
    for cell in text.chars() {
        if cell.is_ascii_digit() {
            digits.push(cell);
            continue;
        }
        if cell == 's' && !digits.is_empty() {
            let seconds: i64 = digits.parse().expect("elapsed digits parse");
            assert!(
                (0..10).contains(&seconds),
                "elapsed stamp out of sane range: {seconds}s",
            );
            scrubbed.push_str("@ELAPSED@s");
            digits.clear();
            continue;
        }
        scrubbed.push_str(&digits);
        digits.clear();
        scrubbed.push(cell);
    }
    scrubbed.push_str(&digits);
    scrubbed.into_bytes()
}

/// Twin outputs compared on behavior: scope paths, then elapsed
/// stamps, scrubbed identically on both sides.
fn scrub_twin(bytes: &[u8], scope: &std::path::Path) -> Vec<u8> {
    scrub_update_elapsed(&scrub_scope(bytes, scope))
}

#[test]
fn update_native_matches_shell_byte_for_byte() {
    // Twin staged clients (base plus one overlay, both current):
    // the shell side runs the default adapter, the Rust side the
    // native driver. Pulls are no-ops, so the run exercises the
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
