//! Native startup contracts for configuration, checkout/release discovery,
//! re-exec protection, command bytes, permissions, and provenance.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot_test_support::TempDir;

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

fn repo() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

fn clean_home(label: &str) -> TempDir {
    TempDir::new(label).expect("home fixture")
}

fn bad_config_home(label: &str, body: &[u8]) -> TempDir {
    let home = clean_home(label);
    let config = home.path().join(".config/dot/config");
    std::fs::create_dir_all(config.parent().expect("config parent")).expect("config parent");
    std::fs::write(config, body).expect("config fixture");
    home
}

fn parent_path() -> std::ffi::OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

fn run(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot"));
    command
        .args(argv)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("TMPDIR", "/tmp")
        .env("HOME", home)
        .env("DOT_SOURCE_ROOT", repo())
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    let output = command.output().expect("run dot");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

fn version_bytes() -> Vec<u8> {
    format!(
        "dot commit {} (config 1; extensions 1; library 1)\n",
        env!("DOT_BUILD_SHORT_COMMIT")
    )
    .into_bytes()
}

#[test]
fn runtime_snapshots_xdg_homes_from_explicit_environment() {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    let home = clean_home("runtime-home");
    let state = clean_home("runtime-state");
    let config = clean_home("runtime-config");
    let mut env = BTreeMap::new();
    env.insert(OsString::from("HOME"), home.path().as_os_str().to_owned());
    env.insert(
        OsString::from("XDG_STATE_HOME"),
        state.path().as_os_str().to_owned(),
    );
    env.insert(
        OsString::from("XDG_CONFIG_HOME"),
        config.path().as_os_str().to_owned(),
    );
    let runtime = dot::app::Runtime::from_env(&env, home.path()).expect("runtime");
    assert_eq!(runtime.home(), home.path());
    assert_eq!(runtime.state_home(), state.path());
    assert_eq!(runtime.config_home(), config.path());
    assert_eq!(runtime.cwd(), home.path());
}

#[test]
fn informational_commands_ignore_unloadable_config() {
    let home = bad_config_home("bad-config", b"version=1\nbogus=1\n");
    for argv in [
        vec![],
        vec![OsStr::new("help")],
        vec![OsStr::new("-h")],
        vec![OsStr::new("--help")],
    ] {
        assert_eq!(
            run(home.path(), &argv, &[]),
            (0, EXPECTED_HELP.as_bytes().to_vec(), Vec::new()),
            "help bypasses config for {argv:?}"
        );
    }
    for argv in [vec![OsStr::new("version")], vec![OsStr::new("--version")]] {
        assert_eq!(
            run(home.path(), &argv, &[]),
            (0, version_bytes(), Vec::new())
        );
    }
}

#[test]
fn unloadable_config_exits_2_for_operational_and_unknown_commands() {
    let home = bad_config_home("bad-config-operations", b"version=1\nbogus=1\n");
    for word in [
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
        assert_eq!(
            run(home.path(), &[OsStr::new(word)], &[]),
            (2, Vec::new(), b"dot: config: unknown key: bogus\n".to_vec()),
            "config precedes operational dispatch for {word}"
        );
    }
}

#[test]
fn bad_env_policy_exits_2_for_unknown_command_only() {
    // Environment policy validation wins before the deliberately bad file.
    let home = bad_config_home("bad-policy", b"version=1\nbogus=1\n");
    let env = [("DOT_SHDEPS_UPDATE_POLICY", Some("bogus"))];
    assert_eq!(
        run(home.path(), &[OsStr::new("frobnicate")], &env),
        (
            2,
            Vec::new(),
            b"dot: config: DOT_SHDEPS_UPDATE_POLICY must be pinned or latest, found: bogus\n"
                .to_vec()
        )
    );
}

#[test]
fn unresolvable_home_exits_2_with_config_root_diagnostic() {
    let fixture = clean_home("bad-home");
    let env = [
        ("HOME", Some("relative-dot-home")),
        ("XDG_CONFIG_HOME", None),
    ];
    assert_eq!(
        run(fixture.path(), &[OsStr::new("update")], &env),
        (
            2,
            Vec::new(),
            b"dot: config: HOME does not provide an absolute config root\n".to_vec()
        )
    );
}

#[test]
fn loadable_config_leaves_wired_commands_byte_exact() {
    let home = clean_home("clean");
    for argv in [Vec::new(), vec![OsStr::new("help")]] {
        assert_eq!(
            run(home.path(), &argv, &[]),
            (0, EXPECTED_HELP.as_bytes().to_vec(), Vec::new()),
            "help bytes {argv:?}"
        );
    }
    assert_eq!(
        run(home.path(), &[OsStr::new("version")], &[]),
        (0, version_bytes(), Vec::new())
    );
}

#[test]
fn reexec_mismatch_message_matches_shell_byte_for_byte() {
    let observed = dot::startup::observed_revision(Path::new(repo())).expect("checkout revision");
    assert_eq!(
        dot::startup::check_reexec_revision(Some("deadbeef"), Some(&observed)),
        Err(format!(
            "dot: re-exec revision mismatch: expected deadbeef, found {observed}"
        ))
    );
    assert_eq!(
        dot::startup::check_reexec_revision(Some(&observed), Some(&observed)),
        Ok(())
    );
    assert_eq!(
        dot::startup::check_reexec_revision(None, Some(&observed)),
        Ok(())
    );
    assert_eq!(
        dot::startup::check_reexec_revision(Some(""), Some(&observed)),
        Ok(())
    );
}

#[test]
fn reexec_missing_checkout_reports_missing_on_both_sides() {
    let empty = clean_home("missing-checkout");
    assert_eq!(dot::startup::observed_revision(empty.path()), None);
    let expected =
        Err("dot: re-exec revision mismatch: expected deadbeef, found <missing>".to_string());
    assert_eq!(
        dot::startup::check_reexec_revision(Some("deadbeef"), None),
        expected
    );
    assert_eq!(
        dot::startup::check_reexec_revision(Some("deadbeef"), Some("")),
        expected
    );
}

#[test]
fn reexec_guard_precedes_dispatch_in_the_binary() {
    let home = clean_home("reexec-binary");
    let observed = dot::startup::observed_revision(Path::new(repo())).expect("checkout revision");
    let mismatch = [("DOT_REEXEC_EXPECTED_REVISION", Some("deadbeef"))];
    assert_eq!(
        run(home.path(), &[OsStr::new("version")], &mismatch),
        (
            1,
            Vec::new(),
            format!("dot: re-exec revision mismatch: expected deadbeef, found {observed}\n")
                .into_bytes()
        )
    );
    let matched = [("DOT_REEXEC_EXPECTED_REVISION", Some(observed.as_str()))];
    assert_eq!(
        run(home.path(), &[OsStr::new("version")], &matched),
        (0, version_bytes(), Vec::new())
    );
}

#[test]
fn binary_ignores_caller_supplied_source_root() {
    let home = clean_home("untrusted-source-root");
    let untrusted = clean_home("untrusted-checkout");
    let observed = dot::startup::observed_revision(Path::new(repo())).expect("checkout revision");
    let untrusted_text = untrusted.path().to_str().expect("ASCII fixture path");
    let env = [
        ("DOT_SOURCE_ROOT", Some(untrusted_text)),
        ("TERMUX_EXEC__PROC_SELF_EXE", Some(untrusted_text)),
        ("DOT_REEXEC_EXPECTED_REVISION", Some(observed.as_str())),
    ];

    assert_eq!(
        run(home.path(), &[OsStr::new("version")], &env),
        (0, version_bytes(), Vec::new())
    );
}

#[test]
fn binary_fails_closed_outside_an_owned_source_root() {
    let fixture = TempDir::new_exec("unowned-binary").expect("executable fixture");
    let binary = fixture.path().join("dot");
    dot_test_support::copy_dot_binary(Path::new(env!("CARGO_BIN_EXE_dot")), &binary)
        .expect("copy binary");

    let output = Command::new(&binary)
        .arg("version")
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("HOME", fixture.path())
        .env("DOT_SOURCE_ROOT", repo())
        .current_dir(repo())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run unowned binary");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"dot: startup: cannot resolve source root from executable\n"
    );
}

#[test]
fn packaged_help_and_version_survive_removed_hook_assets() {
    let release = TempDir::new_exec("standalone-release").expect("executable fixture");
    let binary = release.path().join("dot");
    dot_test_support::copy_dot_binary(Path::new(env!("CARGO_BIN_EXE_dot")), &binary)
        .expect("copy binary");
    std::fs::write(release.path().join(".dot-install.json"), b"{}\n").expect("release metadata");

    for (command, expected) in [
        ("help", EXPECTED_HELP.as_bytes().to_vec()),
        ("version", version_bytes()),
    ] {
        let output = Command::new(&binary)
            .arg(command)
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", parent_path())
            .env("HOME", release.path())
            .env("DOT_SOURCE_ROOT", "/untrusted")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run standalone command");
        assert_eq!(output.status.code(), Some(0), "command: {command}");
        assert_eq!(output.stdout, expected, "command: {command}");
        assert!(output.stderr.is_empty(), "command: {command}");
    }

    let output = Command::new(&binary)
        .arg("doctor")
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", parent_path())
        .env("HOME", release.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run operational command");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"dot: startup: cannot resolve source root from executable\n"
    );
}

#[test]
fn umask_ceiling_matches_shell_g_w_o_w() {
    for (start, expected) in [
        (0o022, 0o022),
        (0o002, 0o022),
        (0o027, 0o027),
        (0o077, 0o077),
        (0o000, 0o022),
        (0o007, 0o027),
        (0o026, 0o026),
    ] {
        assert_eq!(
            dot::startup::ensure_umask_ceiling(start),
            expected,
            "mask {start:03o}"
        );
    }
}

#[test]
fn source_root_resolution_matches_shell_derivation() {
    use std::os::unix::fs::symlink;

    let root = clean_home("checkout-root");
    let public = root.path().join("lib/dot/public");
    std::fs::create_dir_all(&public).expect("public API");
    std::fs::write(
        root.path().join("Cargo.toml"),
        b"[package]\nname='fixture'\n",
    )
    .expect("checkout metadata");
    for relative in ["target/debug/dot", "bin/dot"] {
        let executable = root.path().join(relative);
        std::fs::create_dir_all(executable.parent().expect("binary parent"))
            .expect("binary parent");
        std::fs::write(&executable, b"native executable").expect("binary");
        assert_eq!(
            dot::startup::resolve_source_root(&executable, None, Path::new("/wrong-cwd")),
            root.path().canonicalize().expect("canonical root")
        );
        assert_eq!(
            dot::startup::executable_source_root(&executable).expect("owned executable"),
            root.path().canonicalize().expect("canonical root")
        );
    }
    let link_home = clean_home("checkout-link");
    let link = link_home.path().join("dot");
    symlink(root.path().join("target/debug/dot"), &link).expect("binary symlink");
    assert_eq!(
        dot::startup::executable_source_root(&link).expect("linked executable"),
        root.path().canonicalize().expect("canonical root")
    );
    assert_eq!(
        dot::startup::resolve_source_root(
            Path::new("/missing"),
            Some(OsStr::new("/custom/root")),
            Path::new("/wrong")
        ),
        PathBuf::from("/custom/root")
    );
    assert_eq!(
        dot::startup::resolve_source_root(
            Path::new("/missing"),
            Some(OsStr::new("")),
            Path::new("/fallback")
        ),
        PathBuf::from("/fallback")
    );
    assert_eq!(
        dot::startup::resolve_source_root(Path::new("/missing"), None, Path::new("/fallback")),
        PathBuf::from("/fallback")
    );
}

#[test]
fn installed_binary_resolves_release_root_without_private_engine() {
    let root = clean_home("installed-root");
    let binary = root.path().join("dot");
    std::fs::write(&binary, b"native executable").expect("binary");
    std::fs::write(
        root.path().join(".dot-install.json"),
        b"{\"version\":\"20260907-000000-deadbeef\"}\n",
    )
    .expect("release metadata");
    std::fs::create_dir_all(root.path().join("lib/dot/public")).expect("public API");
    assert_eq!(
        dot::startup::resolve_source_root(&binary, None, Path::new("/wrong-cwd")),
        root.path().canonicalize().expect("canonical root")
    );

    for missing in ["metadata", "public-api"] {
        let incomplete = clean_home(missing);
        let binary = incomplete.path().join("dot");
        std::fs::write(&binary, b"native executable").expect("binary");
        if missing == "metadata" {
            std::fs::create_dir_all(incomplete.path().join("lib/dot/public")).expect("public API");
        } else {
            std::fs::write(incomplete.path().join(".dot-install.json"), b"{}\n").expect("metadata");
        }
        assert_eq!(
            dot::startup::resolve_source_root(&binary, None, Path::new("/fallback")),
            PathBuf::from("/fallback"),
            "incomplete installed metadata: {missing}"
        );
        assert_eq!(
            dot::startup::executable_source_root(&binary)
                .expect_err("incomplete roots must fail closed")
                .to_string(),
            "cannot resolve source root from executable",
            "incomplete installed metadata: {missing}"
        );
    }
}

#[test]
fn native_dispatch_still_pins_case_exactness() {
    for word in [
        "UPDATE", "Update", "HELP", "Help", "VERSION", "Version", " Help",
    ] {
        assert_eq!(
            dot::cli::dispatch(word.as_bytes()),
            dot::cli::Command::Unknown,
            "command {word:?}"
        );
    }
    for (word, expected) in [
        ("update", dot::cli::Command::Update),
        ("pull", dot::cli::Command::Update),
        ("help", dot::cli::Command::Unknown),
        ("version", dot::cli::Command::Unknown),
    ] {
        assert_eq!(
            dot::cli::dispatch(word.as_bytes()),
            expected,
            "command {word:?}"
        );
    }
}

#[test]
fn binary_needs_no_bash_interpreter_gate() {
    let home = clean_home("no-bash");
    let env = [("DOT_BASH", Some("/nonexistent-bash"))];
    assert_eq!(
        run(home.path(), &[OsStr::new("help")], &env),
        (0, EXPECTED_HELP.as_bytes().to_vec(), Vec::new())
    );
    assert_eq!(
        run(home.path(), &[OsStr::new("version")], &env),
        (0, version_bytes(), Vec::new())
    );
}

#[test]
fn startup_registers_no_new_provenance_path_beyond_this_suite() {
    let manifest = std::fs::read_to_string(Path::new(repo()).join("docs/source-provenance-v1.tsv"))
        .expect("provenance manifest");
    let rows = manifest
        .lines()
        .filter(|line| line.starts_with("tests/startup.rs\t"))
        .collect::<Vec<_>>();
    assert_eq!(rows, ["tests/startup.rs\tstandalone:new"]);
}
