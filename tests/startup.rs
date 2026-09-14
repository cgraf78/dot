//! Differential parity tests for the binary startup prelude (slice 84).
//!
//! The Rust entry path (`src/startup.rs`, wired through `cli::run` and
//! `main.rs`) must behave like the `lib/dot/main.sh` prelude composed
//! with the `bin/dot` entry contract: `DOT_SOURCE_ROOT` resolution,
//! the `DOT_REEXEC_EXPECTED_REVISION` mismatch guard (exit 1), and
//! `dot_config_load || exit 2` — which the forward contracts
//! (`docs/rust-port-spec.md`) order BEFORE dispatch for ANY command,
//! including `help`/`version` (the shell's `case` currently exempts
//! those two; the port follows the spec and pins that divergence
//! here). `umask g-w,o-w`, `shopt -u nocasematch`, and the Bash-4+
//! gate have no process-global Rust equivalent; their observable
//! contracts (mask ceiling bits, byte-exact dispatch, no interpreter
//! gate) are pinned differentially below.
//!
//! Every case runs the live shell implementation and its Rust twin on
//! identical inputs and compares exit code plus raw bytes exactly.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::test_support::{TempDir, bash};

/// Crate root: the shell tree under test.
fn repo() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dot"))
}

fn parent_path() -> std::ffi::OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

fn parent_tmpdir() -> std::ffi::OsString {
    std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"))
}

/// Home fixture with no config file: `dot_config_load` resolves the
/// XDG default, finds nothing, and yields shell defaults.
fn clean_home(label: &str) -> TempDir {
    TempDir::new(label).expect("home fixture")
}

/// Home fixture whose XDG-default config is unloadable: the shell
/// diagnostic and the Rust twin must agree byte for byte.
fn bad_config_home(label: &str, body: &[u8]) -> TempDir {
    let home = TempDir::new(label).expect("home fixture");
    let dir = home.path().join(".config/dot");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("config"), body).expect("bad config");
    home
}

/// Shell oracle for the startup config gate: the exact
/// `dot_config_load || exit 2` sequence, ignoring argv (a failed load
/// exits before any dispatch on both sides).
fn shell_config_gate(home: &Path, extra_env: &[(&str, Option<&str>)]) -> (i32, Vec<u8>, Vec<u8>) {
    let script = concat!(
        ". \"$1/lib/dot/public/xdg.sh\"\n",
        ". \"$1/lib/dot/config.sh\"\n",
        "dot_config_load || exit 2\n",
    );
    let mut cmd = Command::new(bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(script);
    cmd.arg("dot-test-sh").arg(repo());
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("TMPDIR", parent_tmpdir())
        .env("HOME", home)
        .env("DOT_SOURCE_ROOT", repo())
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn config oracle");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Rust twin for the config gate: the real binary under the same
/// environment. `argv` is forwarded so every command spelling proves
/// the gate runs before dispatch.
fn rust_gate(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut cmd = bin();
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("TMPDIR", parent_tmpdir())
        .env("HOME", home)
        .env("DOT_SOURCE_ROOT", repo())
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("run dot binary");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

fn commands() -> Vec<Vec<&'static OsStr>> {
    let mut cases: Vec<Vec<&'static OsStr>> = vec![vec![]];
    for word in [
        "help",
        "-h",
        "--help",
        "version",
        "--version",
        "frobnicate",
        "update",
        "pull",
        "fetch",
        "push",
        "status",
        "diff",
        "cron",
        "doctor",
        "test",
        "init",
    ] {
        cases.push(vec![OsStr::new(word)]);
    }
    cases
}

#[test]
fn unloadable_config_exits_2_for_every_command() {
    // Forward-contract order: config loads before dispatch, so even
    // `help`/`version` exit 2 here. The shell `case` in `main.sh`
    // currently exempts those two (verified by hand against
    // `bin/dot`); the port follows `docs/rust-port-spec.md` ("an
    // unloadable config exits 2 for ANY command") and this test pins
    // the divergence: the oracle below is the spec-ordered
    // `dot_config_load || exit 2` composition, not the `case` order.
    let home = bad_config_home("startup-bad-config", b"version=1\nbogus=1\n");
    let expected_stderr = b"dot: config: unknown key: bogus\n".to_vec();
    for argv in commands() {
        let (code, out, err) = shell_config_gate(home.path(), &[]);
        assert_eq!(code, 2, "oracle argv: {argv:?}");
        assert!(out.is_empty(), "oracle argv: {argv:?}");
        assert_eq!(err, expected_stderr, "oracle argv: {argv:?}");
        let (code, out, err) = rust_gate(home.path(), &argv, &[]);
        assert_eq!(code, 2, "argv: {argv:?}");
        assert!(out.is_empty(), "argv: {argv:?}");
        assert_eq!(err, expected_stderr, "argv: {argv:?}");
    }
}

#[test]
fn bad_env_policy_exits_2_for_help_version_and_unknown() {
    // The env override validates before the file is even touched, so
    // no config file is needed for this rejection.
    let home = clean_home("startup-bad-policy");
    let env = &[("DOT_SHDEPS_UPDATE_POLICY", Some("bogus"))];
    let expected_stderr =
        b"dot: config: DOT_SHDEPS_UPDATE_POLICY must be pinned or latest, found: bogus\n".to_vec();
    for word in ["help", "version", "frobnicate"] {
        let argv = [OsStr::new(word)];
        let (code, out, err) = shell_config_gate(home.path(), env);
        assert_eq!(
            (code, out.as_slice(), err.as_slice()),
            (2, b"".as_slice(), expected_stderr.as_slice()),
            "oracle: {word}"
        );
        let (code, out, err) = rust_gate(home.path(), &argv, env);
        assert_eq!(code, 2, "argv: {word}");
        assert!(out.is_empty(), "argv: {word}");
        assert_eq!(err, expected_stderr, "argv: {word}");
    }
}

#[test]
fn unresolvable_home_exits_2_with_config_root_diagnostic() {
    // A relative HOME resolves no absolute config root on either
    // side; `XDG_CONFIG_HOME` stays unset so the fallback cannot save
    // the lookup.
    let fixture = TempDir::new("startup-bad-home").expect("fixture dir");
    let env: &[(&str, Option<&str>)] = &[
        ("HOME", Some("relative-dot-home")),
        ("XDG_CONFIG_HOME", None),
    ];
    let expected_stderr = b"dot: config: HOME does not provide an absolute config root\n".to_vec();
    let (code, out, err) = shell_config_gate(fixture.path(), env);
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert_eq!(err, expected_stderr);
    let argv = [OsStr::new("help")];
    let (code, out, err) = rust_gate(fixture.path(), &argv, env);
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert_eq!(err, expected_stderr);
}

#[test]
fn loadable_config_leaves_wired_commands_byte_exact() {
    // The gate must be invisible when config loads: `help` still
    // prints the exact heredoc and `version` still agrees with the
    // shell `bin/dot` in the same checkout.
    let home = clean_home("startup-clean");
    for argv in [Vec::new(), vec![OsStr::new("help")]] {
        let (code, out, err) = rust_gate(home.path(), &argv, &[]);
        assert_eq!(code, 0, "argv: {argv:?}");
        assert!(!out.is_empty(), "argv: {argv:?}");
        assert!(err.is_empty(), "argv: {argv:?}");
    }
    let shell = Command::new(bash())
        .arg("--noprofile")
        .arg("--norc")
        .arg(repo().to_string() + "/bin/dot")
        .arg("version")
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("TMPDIR", parent_tmpdir())
        .env("HOME", home.path())
        .env("DOT_SOURCE_ROOT", repo())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("shell dot version");
    let rust = rust_gate(home.path(), &[OsStr::new("version")], &[]);
    assert_eq!(rust.0, 0);
    assert_eq!(rust.1, shell.stdout);
    assert_eq!(rust.2, shell.stderr);
}

/// Shell oracle for the re-exec guard excerpt (`lib/dot/main.sh`
/// lines 29-37 with `temp.sh` sourced): prints the exact mismatch
/// line to stderr and exits 1, silent with code 0 on a match or when
/// no revision is expected.
fn shell_reexec(
    source_root: &Path,
    expected: Option<&str>,
    workdir: &Path,
) -> (i32, Vec<u8>, Vec<u8>) {
    let script = concat!(
        ". \"$1/lib/dot/temp.sh\"\n",
        "if [[ -n ${DOT_REEXEC_EXPECTED_REVISION:-} ]]; then\n",
        "  _dot_reexec_observed=$(_dot_source_git rev-parse HEAD 2>/dev/null || true)\n",
        "  if [[ $_dot_reexec_observed != \"$DOT_REEXEC_EXPECTED_REVISION\" ]]; then\n",
        "    printf 'dot: re-exec revision mismatch: expected %s, found %s\\n' \\\n",
        "      \"$DOT_REEXEC_EXPECTED_REVISION\" \"${_dot_reexec_observed:-<missing>}\" >&2\n",
        "    exit 1\n",
        "  fi\n",
        "fi\n",
    );
    let mut cmd = Command::new(bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(script);
    cmd.arg("dot-test-sh").arg(repo());
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("TMPDIR", parent_tmpdir())
        .env("HOME", workdir)
        .env("DOT_SOURCE_ROOT", source_root)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match expected {
        Some(value) => {
            cmd.env("DOT_REEXEC_EXPECTED_REVISION", value);
        }
        None => {
            cmd.env_remove("DOT_REEXEC_EXPECTED_REVISION");
        }
    }
    let output = cmd.output().expect("spawn reexec oracle");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Rust twin for the re-exec guard at function level: the pure
/// decision over an explicitly supplied observed revision, so the
/// message bytes compare without forking git on either side.
fn rust_reexec_line(expected: Option<&str>, observed: Option<&str>) -> (i32, Vec<u8>) {
    match dot::startup::check_reexec_revision(expected, observed) {
        Ok(()) => (0, Vec::new()),
        Err(line) => {
            let mut stderr = line.into_bytes();
            stderr.push(b'\n');
            (1, stderr)
        }
    }
}

#[test]
fn reexec_mismatch_message_matches_shell_byte_for_byte() {
    let workdir = TempDir::new("startup-reexec").expect("workdir");
    let repo_path = Path::new(repo());
    // Mismatch against the live checkout: the observed revision comes
    // from the same `git rev-parse HEAD`, so both lines agree.
    let (code, out, err) = shell_reexec(repo_path, Some("deadbeef"), workdir.path());
    assert_eq!(code, 1);
    assert!(out.is_empty());
    let observed = dot::startup::observed_revision(repo_path);
    let observed_text = observed.clone().unwrap_or_default();
    assert!(
        err.starts_with(b"dot: re-exec revision mismatch: expected deadbeef, found "),
        "oracle line: {err:?}"
    );
    assert!(
        err.ends_with(b"\n") && !observed_text.is_empty(),
        "checkout must yield an observed revision: {err:?}"
    );
    let (rust_code, rust_err) = rust_reexec_line(Some("deadbeef"), observed.as_deref());
    assert_eq!(rust_code, code);
    assert_eq!(rust_err, err);
    // Match: silence and success on both sides.
    let head = String::from_utf8_lossy(&err)
        .trim()
        .rsplit(' ')
        .next()
        .expect("observed revision in oracle line")
        .to_string();
    let (code, out, err) = shell_reexec(repo_path, Some(&head), workdir.path());
    assert_eq!((code, out.len(), err.len()), (0, 0, 0));
    let (rust_code, rust_err) = rust_reexec_line(Some(&head), observed.as_deref());
    assert_eq!((rust_code, rust_err.len()), (0, 0));
    // Unset expectation: the guard is skipped entirely.
    let (code, out, err) = shell_reexec(repo_path, None, workdir.path());
    assert_eq!((code, out.len(), err.len()), (0, 0, 0));
    let (rust_code, rust_err) = rust_reexec_line(None, observed.as_deref());
    assert_eq!((rust_code, rust_err.len()), (0, 0));
}

#[test]
fn reexec_missing_checkout_reports_missing_on_both_sides() {
    // Outside any git checkout the observed revision is empty, which
    // `${var:-<missing>}` spells literally — the Rust twin maps empty
    // to `<missing>` the same way.
    let empty = TempDir::new("startup-reexec-empty").expect("empty dir");
    let (code, out, err) = shell_reexec(empty.path(), Some("deadbeef"), empty.path());
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert_eq!(
        err,
        b"dot: re-exec revision mismatch: expected deadbeef, found <missing>\n".to_vec()
    );
    assert_eq!(dot::startup::observed_revision(empty.path()), None);
    let (rust_code, rust_err) = rust_reexec_line(Some("deadbeef"), None);
    assert_eq!(rust_code, 1);
    assert_eq!(rust_err, err);
    // An empty observed string (not just absent) spells `<missing>` too.
    let (rust_code, rust_err) = rust_reexec_line(Some("deadbeef"), Some(""));
    assert_eq!(rust_code, 1);
    assert_eq!(rust_err, err);
}

#[test]
fn reexec_guard_precedes_dispatch_in_the_binary() {
    // End to end: a bogus expectation fails even `version` (exit 1,
    // exact line, no version output); the correct HEAD lets it
    // through byte-exact.
    let home = clean_home("startup-reexec-binary");
    let output = Command::new(repo().to_string() + "/bin/dot")
        .arg("version")
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("TMPDIR", parent_tmpdir())
        .env("HOME", home.path())
        .env("DOT_SOURCE_ROOT", repo())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("shell version baseline");
    assert_eq!(output.status.code(), Some(0));
    let head = String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("dot commit ")
        .and_then(|rest| rest.split(' ').next())
        .expect("shell version revision")
        .to_string();
    let full_head = std::process::Command::new("git")
        .arg("-C")
        .arg(repo())
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("git HEAD");
    assert!(full_head.status.success());
    let full_head = String::from_utf8_lossy(&full_head.stdout)
        .trim()
        .to_string();
    assert!(
        head.len() == 12 || head == "unknown",
        "shell revision: {head}"
    );
    let bogus_argv = [OsStr::new("version")];
    let bogus_env: &[(&str, Option<&str>)] = &[("DOT_REEXEC_EXPECTED_REVISION", Some("deadbeef"))];
    let (code, out, err) = rust_gate(home.path(), &bogus_argv, bogus_env);
    assert_eq!(code, 1);
    assert!(out.is_empty());
    let observed = if full_head.is_empty() {
        "<missing>".to_string()
    } else {
        full_head.clone()
    };
    assert_eq!(
        err,
        format!("dot: re-exec revision mismatch: expected deadbeef, found {observed}\n")
            .into_bytes()
    );
    let good_env: &[(&str, Option<&str>)] =
        &[("DOT_REEXEC_EXPECTED_REVISION", Some(full_head.as_str()))];
    let (code, out, err) = rust_gate(home.path(), &bogus_argv, good_env);
    assert_eq!(code, 0);
    assert_eq!(out, output.stdout);
    assert_eq!(err, output.stderr);
}

#[test]
fn umask_ceiling_matches_shell_g_w_o_w() {
    // `umask g-w,o-w` ORs group-write and other-write into the mask
    // while retaining stricter caller bits (0077 stays 0077); the
    // Rust twin is that same OR, so no process-global mutation is
    // needed (and none happens: mutating the mask would be visible to
    // every thread).
    for (start, expected) in [
        ("022", 0o022),
        ("002", 0o022),
        ("027", 0o027),
        ("077", 0o077),
        ("000", 0o022),
        ("007", 0o027),
        ("026", 0o026),
    ] {
        let output = Command::new(bash())
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(format!("umask {start}; umask g-w,o-w; umask"))
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", parent_path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("spawn umask oracle");
        assert!(output.status.success(), "mask: {start}");
        let text = String::from_utf8_lossy(&output.stdout);
        let shell_mask = u32::from_str_radix(text.trim(), 8).expect("oracle mask is octal");
        assert_eq!(shell_mask, expected, "mask: {start}");
        assert_eq!(
            dot::startup::ensure_umask_ceiling(
                u32::from_str_radix(start, 8).expect("fixture mask is octal")
            ),
            expected,
            "mask: {start}"
        );
    }
}

#[test]
fn source_root_resolution_matches_shell_derivation() {
    // The shell binds `DOT_SOURCE_ROOT` from its own path
    // (`dirname lib/dot/main.sh` up two, physical): the oracle below
    // evaluates that exact expression against a fixture tree, and the
    // Rust probe must answer the same root for the co-located entry
    // point. An explicit `DOT_SOURCE_ROOT` wins verbatim on the Rust
    // side (the hermetic-test and embedding hook the shell's
    // unconditional bind cannot offer); with neither, the cwd applies
    // (mirroring `${DOT_SOURCE_ROOT:-$PWD}` in `_dot_source_git`).
    let root = TempDir::new("startup-root").expect("fixture root");
    let lib_dot = root.path().join("lib/dot");
    let bin_dir = root.path().join("bin");
    std::fs::create_dir_all(&lib_dot).expect("lib/dot");
    std::fs::create_dir_all(&bin_dir).expect("bin");
    std::fs::write(lib_dot.join("main.sh"), b"# fixture\n").expect("main.sh");
    std::fs::write(bin_dir.join("dot"), b"# fixture\n").expect("dot");
    let main_sh = lib_dot.join("main.sh");
    let output = Command::new(bash())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg("s=$1; DOT_SOURCE_ROOT=$(cd -P -- \"$(dirname \"$s\")/../..\" && pwd -P); printf '%s' \"$DOT_SOURCE_ROOT\"")
        .arg("dot-test-sh")
        .arg(&main_sh)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn source-root oracle");
    assert!(output.status.success());
    let shell_root = PathBuf::from(String::from_utf8(output.stdout).expect("root UTF-8"));
    let canonical_root = root.path().canonicalize().expect("canonical root");
    assert_eq!(shell_root, canonical_root);
    assert_eq!(
        dot::startup::resolve_source_root(&main_sh, None, Path::new("/nonexistent-cwd")),
        canonical_root
    );
    assert_eq!(
        dot::startup::resolve_source_root(
            &bin_dir.join("dot"),
            None,
            Path::new("/nonexistent-cwd")
        ),
        canonical_root
    );
    let deep_exe = root.path().join("target/debug/dot");
    std::fs::create_dir_all(deep_exe.parent().expect("debug dir")).expect("debug dir");
    std::fs::write(&deep_exe, b"# fixture\n").expect("debug exe");
    assert_eq!(
        dot::startup::resolve_source_root(&deep_exe, None, Path::new("/nonexistent-cwd")),
        canonical_root
    );
    assert_eq!(
        dot::startup::resolve_source_root(
            Path::new("/nonexistent/exe"),
            Some(std::ffi::OsStr::new("/custom/root")),
            Path::new("/nonexistent-cwd"),
        ),
        PathBuf::from("/custom/root")
    );
    assert_eq!(
        dot::startup::resolve_source_root(
            Path::new("/nonexistent/exe"),
            Some(std::ffi::OsStr::new("")),
            Path::new("/fallback-cwd"),
        ),
        PathBuf::from("/fallback-cwd")
    );
    assert_eq!(
        dot::startup::resolve_source_root(
            Path::new("/nonexistent/exe"),
            None,
            Path::new("/fallback-cwd"),
        ),
        PathBuf::from("/fallback-cwd")
    );
}

#[test]
fn shell_prelude_still_pins_case_exactness() {
    // Both entry files must keep `shopt -u nocasematch`: command
    // matching stays byte-exact, and the Rust `match` on argv bytes
    // needs no locale-sensitive equivalent (documented in
    // `src/startup.rs`).
    for file in ["bin/dot", "lib/dot/main.sh"] {
        let text = std::fs::read_to_string(Path::new(repo()).join(file)).expect("entry source");
        assert!(
            text.contains("shopt -u nocasematch"),
            "{file} lost its nocasematch pin"
        );
    }
    for word in [
        "UPDATE", "Update", "HELP", "Help", "VERSION", "Version", " Help",
    ] {
        assert_eq!(
            dot::cli::dispatch(word.as_bytes()),
            dot::cli::Command::Unknown,
            "command: {word:?}"
        );
    }
}

#[test]
fn binary_needs_no_bash_interpreter_gate() {
    // The shell refuses to run below Bash 4
    // (`dot: Bash 4 or newer is required`) and `bin/dot` honors a
    // strict `DOT_BASH` override; the compiled binary has no
    // interpreter to version-gate, so even a bogus `DOT_BASH` leaves
    // `help`/`version` green. (Kernel paths that fork shell helpers
    // resolve their interpreter separately; see `test_support`.)
    let home = clean_home("startup-no-bash");
    let env: &[(&str, Option<&str>)] = &[("DOT_BASH", Some("/nonexistent-bash"))];
    for word in ["help", "version"] {
        let argv = [OsStr::new(word)];
        let (code, _, err) = rust_gate(home.path(), &argv, env);
        assert_eq!(code, 0, "argv: {word}");
        assert!(err.is_empty(), "argv: {word}");
    }
}

#[test]
fn startup_registers_no_new_provenance_path_beyond_this_suite() {
    // The lane adds exactly one tracked test file; the manifest must
    // carry it as `standalone:new` (checked by
    // `bash tests/provenance-test`, pinned again here so a missing
    // row fails the suite that added the file).
    let manifest = std::fs::read_to_string(Path::new(repo()).join("docs/source-provenance-v1.tsv"))
        .expect("provenance manifest");
    assert!(
        manifest
            .lines()
            .any(|line| line == "tests/startup.rs\tstandalone:new"),
        "tests/startup.rs must be registered as standalone:new"
    );
}
