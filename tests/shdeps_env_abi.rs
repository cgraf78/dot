//! Native contracts for the Shdeps caller environment and binary ABI boundary.
//!
//! Executable fixtures below are genuine external-provider boundaries. Expected
//! statuses, captured bytes, and configured values are fixed by the public ABI;
//! no deleted provider implementation participates in these tests.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::shdeps_env_abi::{
    BoundedOutcome, ConfigureInputs, ConfiguredEnv, RestoredEnv, abi_timeout, abi_version,
    binary_abi, configure_env, restore_caller_env, run_bounded,
};
use dot_test_support::TempDir;

fn argv(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn assert_bounded(
    timeout: &str,
    label: &str,
    mode: &str,
    words: &[&str],
    status: i32,
    stdout: &[u8],
) {
    assert_eq!(
        run_bounded(timeout, label, mode, &argv(words)),
        BoundedOutcome {
            status,
            stdout: stdout.to_vec(),
        },
        "bounded command {words:?}",
    );
}

/// Write a fake external Shdeps binary under an executable scratch directory.
fn probe_script(tag: &str, body: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new_exec(tag).expect("exec dir");
    let path = dir.path().join("probe");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("probe fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (dir, path)
}

// A real update owns one provider operation under the update lock. Keep the
// process-supervision fixtures equally single-owner: overlapping timeout and
// ABI probes make freshly staged interpreter scripts flaky on some filesystems.
static EXEC_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exec_test() -> std::sync::MutexGuard<'static, ()> {
    EXEC_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn expect_config(inputs: ConfigureInputs<'_>, expected: Option<ConfiguredEnv>) {
    assert_eq!(configure_env(&inputs), expected, "inputs: {inputs:?}");
}

#[test]
fn restore_caller_policy_matrix() {
    for (tag, force_set, force, lib_set, lib, expected) in [
        (
            "both-set",
            "x",
            "1",
            "x",
            "/caller/lib.sh",
            RestoredEnv {
                force: Some("1".into()),
                lib: Some("/caller/lib.sh".into()),
            },
        ),
        (
            "both-unset",
            "",
            "",
            "",
            "",
            RestoredEnv {
                force: None,
                lib: None,
            },
        ),
        (
            "force-only",
            "x",
            "0",
            "",
            "",
            RestoredEnv {
                force: Some("0".into()),
                lib: None,
            },
        ),
        (
            "lib-only",
            "",
            "",
            "x",
            "/caller/lib.sh",
            RestoredEnv {
                force: None,
                lib: Some("/caller/lib.sh".into()),
            },
        ),
        (
            "empty-caller",
            "x",
            "",
            "x",
            "",
            RestoredEnv {
                force: Some(String::new()),
                lib: Some(String::new()),
            },
        ),
        (
            "non-x-markers",
            "X",
            "mangled",
            "set",
            "mangled",
            RestoredEnv {
                force: None,
                lib: None,
            },
        ),
    ] {
        assert_eq!(
            restore_caller_env(force_set, force, lib_set, lib),
            expected,
            "{tag}"
        );
    }
}

fn inputs<'a>(
    xdg: &'a str,
    home: &'a str,
    install: &'a str,
    bin: &'a str,
    git: &'a str,
    force: &'a str,
    quiet: &'a str,
) -> ConfigureInputs<'a> {
    ConfigureInputs {
        xdg_config_home: xdg,
        home,
        install_dir: install,
        bin_dir: bin,
        git_dev_dir: git,
        dot_force: force,
        dot_quiet: quiet,
    }
}

#[test]
fn configure_defaults_and_xdg() {
    expect_config(
        inputs("", "/home/tester", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/home/tester/.config/shdeps".into(),
            hooks_dir: "/home/tester/.config/shdeps/hooks.d".into(),
            install_dir: "/home/tester/.local/share".into(),
            bin_dir: "/home/tester/.local/bin".into(),
            git_dev_dir: "/home/tester/git".into(),
            force: false,
            quiet: false,
        }),
    );
    expect_config(
        inputs("/var/config", "/home/tester", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/var/config/shdeps".into(),
            hooks_dir: "/var/config/shdeps/hooks.d".into(),
            install_dir: "/home/tester/.local/share".into(),
            bin_dir: "/home/tester/.local/bin".into(),
            git_dev_dir: "/home/tester/git".into(),
            force: false,
            quiet: false,
        }),
    );
    expect_config(
        inputs("rel/conf", "/home/tester", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/home/tester/.config/shdeps".into(),
            hooks_dir: "/home/tester/.config/shdeps/hooks.d".into(),
            install_dir: "/home/tester/.local/share".into(),
            bin_dir: "/home/tester/.local/bin".into(),
            git_dev_dir: "/home/tester/git".into(),
            force: false,
            quiet: false,
        }),
    );
    expect_config(
        inputs("", "/", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/.config/shdeps".into(),
            hooks_dir: "/.config/shdeps/hooks.d".into(),
            install_dir: "//.local/share".into(),
            bin_dir: "//.local/bin".into(),
            git_dev_dir: "//git".into(),
            force: false,
            quiet: false,
        }),
    );
}

#[test]
fn configure_overrides() {
    for (install, bin, git, expected_install, expected_bin, expected_git) in [
        (
            "/opt/shdeps",
            "/opt/bin",
            "/opt/git",
            "/opt/shdeps",
            "/opt/bin",
            "/opt/git",
        ),
        (
            "",
            "",
            "",
            "/home/tester/.local/share",
            "/home/tester/.local/bin",
            "/home/tester/git",
        ),
        (
            "/opt/shdeps",
            "",
            "/opt/git",
            "/opt/shdeps",
            "/home/tester/.local/bin",
            "/opt/git",
        ),
    ] {
        expect_config(
            inputs("", "/home/tester", install, bin, git, "", ""),
            Some(ConfiguredEnv {
                conf_dir: "/home/tester/.config/shdeps".into(),
                hooks_dir: "/home/tester/.config/shdeps/hooks.d".into(),
                install_dir: expected_install.into(),
                bin_dir: expected_bin.into(),
                git_dev_dir: expected_git.into(),
                force: false,
                quiet: false,
            }),
        );
    }
    expect_config(inputs("", "relative-home", "", "", "", "", ""), None);
}

#[test]
fn configure_force_quiet_flags() {
    for (force, quiet, expected_force, expected_quiet) in [
        ("0", "0", false, false),
        ("1", "", true, false),
        ("", "1", false, true),
        ("1", "1", true, true),
        ("", "", false, false),
        ("2", "yes", false, false),
        (" 1 ", "+1", true, true),
        ("01", "00", true, false),
        ("0x1", "1.0", false, false),
    ] {
        let configured = configure_env(&inputs("", "/home/tester", "", "", "", force, quiet))
            .expect("absolute home configures");
        assert_eq!(configured.force, expected_force, "force {force:?}");
        assert_eq!(configured.quiet, expected_quiet, "quiet {quiet:?}");
    }
}

#[test]
fn bounded_run_passthrough() {
    let _guard = exec_test();
    assert_bounded(
        "5",
        "echo",
        "discard-stderr",
        &["/bin/sh", "-c", "printf 'hello\\n'"],
        0,
        b"hello\n",
    );
    assert_bounded(
        "5",
        "multiline",
        "discard-stderr",
        &["/bin/sh", "-c", "printf 'a\\nb\\n'"],
        0,
        b"a\nb\n",
    );
    assert_bounded("5", "true", "discard-stderr", &["/bin/true"], 0, b"");
    assert_bounded(
        "5",
        "exit three",
        "discard-stderr",
        &["/bin/sh", "-c", "printf partial; exit 3"],
        3,
        b"partial",
    );
    assert_bounded(
        "5",
        "silent",
        "discard-stderr",
        &["/bin/sh", "-c", "exit 0"],
        0,
        b"",
    );
}

#[test]
fn bounded_run_failures() {
    let _guard = exec_test();
    assert_bounded(
        "5",
        "missing",
        "discard-stderr",
        &["/nonexistent-dot-fixture-xyz"],
        127,
        b"",
    );
    assert_bounded(
        "1",
        "timeout",
        "discard-stderr",
        &["/bin/sh", "-c", "printf partial; sleep 30"],
        124,
        b"",
    );
    assert_bounded(
        "5",
        "signaled",
        "discard-stderr",
        &["/bin/sh", "-c", "kill -9 $$"],
        137,
        b"",
    );
    assert_bounded(
        "5",
        "discard diagnostic",
        "discard-stderr",
        &["/bin/sh", "-c", "printf out; printf err >&2; exit 4"],
        4,
        b"out",
    );
}

#[test]
fn bounded_run_usage_errors() {
    let _guard = exec_test();
    for (tag, timeout, label, mode, words) in [
        (
            "zero",
            "0",
            "label",
            "discard-stderr",
            &["/bin/echo", "x"][..],
        ),
        (
            "empty",
            "",
            "label",
            "discard-stderr",
            &["/bin/echo", "x"][..],
        ),
        (
            "alpha",
            "soon",
            "label",
            "discard-stderr",
            &["/bin/echo", "x"][..],
        ),
        (
            "padded",
            " 5",
            "label",
            "discard-stderr",
            &["/bin/echo", "x"][..],
        ),
        (
            "empty-label",
            "5",
            "",
            "discard-stderr",
            &["/bin/echo", "x"][..],
        ),
        ("no-command", "5", "label", "discard-stderr", &[][..]),
        (
            "bad-mode",
            "5",
            "label",
            "noisy-stderr",
            &["/bin/echo", "x"][..],
        ),
    ] {
        assert_eq!(
            run_bounded(timeout, label, mode, &argv(words)),
            BoundedOutcome {
                status: 2,
                stdout: Vec::new()
            },
            "usage row {tag}",
        );
    }
}

#[test]
fn bounded_run_stderr_inherit() {
    let _guard = exec_test();
    if std::env::var_os("DOT_SHDEPS_INHERIT_HELPER").is_some() {
        assert_bounded(
            "5",
            "loud",
            "inherit-stderr",
            &[
                "/bin/sh",
                "-c",
                "printf out; printf inherited-diagnostic >&2",
            ],
            0,
            b"out",
        );
        return;
    }
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("bounded_run_stderr_inherit")
        .arg("--nocapture")
        .env_clear()
        .env("DOT_SHDEPS_INHERIT_HELPER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn isolated helper test");
    assert!(
        output.status.success(),
        "helper status: {:?}",
        output.status
    );
    assert_eq!(output.stderr, b"inherited-diagnostic");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test bounded_run_stderr_inherit ... ok")
    );
}

#[test]
fn abi_version_probes() {
    let _guard = exec_test();
    assert_eq!(abi_timeout(""), 10);
    assert_eq!(abi_timeout("soon"), 10);
    assert_eq!(abi_timeout("0"), 10);
    assert_eq!(abi_timeout("05"), 10);
    assert_eq!(abi_timeout("5"), 5);

    for (tag, body, timeout, expected) in [
        ("pinned", "printf 'abi:12\\n'", "", Some("abi:12")),
        (
            "two-lines",
            "printf 'abi:12\\nextra\\n'",
            "",
            Some("abi:12\nextra"),
        ),
        (
            "unrelated",
            "printf 'hello world\\n'",
            "",
            Some("hello world"),
        ),
        ("failing", "printf 'abi:12\\n'; exit 7", "", None),
        (
            "invalid-timeout-defaults",
            "printf 'abi:12\\n'",
            "soon",
            Some("abi:12"),
        ),
        ("explicit-timeout", "printf 'abi:9\\n'", "5", Some("abi:9")),
        ("empty-success", "exit 0", "", Some("")),
    ] {
        let (_dir, binary) = probe_script(&format!("abi-version-{tag}"), body);
        assert_eq!(abi_version(&binary, timeout).as_deref(), expected, "{tag}");
    }
    assert_eq!(
        abi_version(Path::new("/nonexistent-dot-abi-probe"), ""),
        None
    );
}

#[test]
fn abi_version_non_executable() {
    let _guard = exec_test();
    let dir = TempDir::new("abi-noexec").expect("fixture dir");
    let path = dir.path().join("probe");
    std::fs::write(&path, "#!/bin/sh\nprintf 'abi:12\\n'\n").expect("probe fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    assert_eq!(abi_version(&path, ""), None);
    assert_eq!(abi_version(Path::new(""), ""), None);
}

#[test]
fn binary_abi_matrix() {
    let _guard = exec_test();
    for (tag, body, expected_abi, expected) in [
        ("match", "printf 'abi:12\\n'", "12", true),
        ("mismatch", "printf 'abi:13\\n'", "12", false),
        ("probe-fails", "printf 'abi:12\\n'; exit 3", "12", false),
        ("long", "printf 'abi:9876543210\\n'", "9876543210", true),
        ("empty", "exit 0", "12", false),
        ("extra-line", "printf 'abi:12\\nextra\\n'", "12", false),
    ] {
        let (_dir, binary) = probe_script(&format!("binary-abi-{tag}"), body);
        assert_eq!(binary_abi(&binary, expected_abi, ""), expected, "{tag}");
    }
    assert!(!binary_abi(
        Path::new("/nonexistent-dot-abi-probe"),
        "12",
        ""
    ));
}
