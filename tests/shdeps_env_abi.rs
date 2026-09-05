//! Differential parity tests for `src/shdeps_env_abi.rs` against the
//! live shell (`lib/dot/providers/shdeps.sh`, part 3): the caller-env
//! restore, the env configuration, the bounded runner, and the ABI
//! probe plus its comparison.
//!
//! Separate binary because the comparison rows need per-row
//! `DOT_SOURCE_ROOT` lock fixtures (the shell comparison reads the
//! pinned `abi` itself while the port takes the same value as a
//! parameter), and the probe binaries must live under an
//! exec-capable directory rather than the default temp root.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::test_support::TempDir;

/// Sources for the env half: the provider only needs the XDG library
/// for configuration; the restore reads plain variables.
const SOURCES_ENV: &str = concat!(
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/providers/shdeps.sh\"\n",
);

/// Sources for the runner half: the bounded runner supervises
/// through the cleanup job table, so the resources library joins.
const SOURCES_RUN: &str = concat!(
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/providers/shdeps.sh\"\n",
);

/// Run one shell snippet with extra environment rows. The locale
/// stays pinned like the `repos_pull_base` harness; `HOME` points at
/// the row fixture and `DOT_SOURCE_ROOT` at the lock fixture.
fn shell_run(
    home: &Path,
    source_root: &Path,
    extra: &[(&str, &str)],
    sources: &str,
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{sources}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", source_root);
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// One row fixture: a directory serving as `HOME`.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let root = dir.path().to_path_buf();
        Fixture { _dir: dir, root }
    }
}

/// Valid three-line lock body pinning `abi`, for the comparison rows
/// whose shell side reads the lock itself. Callers only pass abis
/// the lock gate accepts (leading nonzero digit, digits only); a
/// malformed lock refuses before probing on the shell side while the
/// port compares directly, so such rows could never agree.
fn lock_with_abi(abi: &str) -> Vec<u8> {
    format!(
        "revision=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
         install_sha256=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
         abi={abi}\n"
    )
    .into_bytes()
}

/// Fixture serving as both `HOME` and `DOT_SOURCE_ROOT`, with a lock
/// body staged under `support/`.
fn lock_fixture(tag: &str, lock: &[u8]) -> Fixture {
    let fixture = Fixture::build(tag);
    let support = fixture.root.join("support");
    std::fs::create_dir_all(&support).expect("support dir");
    std::fs::write(support.join("shdeps.lock"), lock).expect("lock fixture");
    fixture
}

/// Write an executable probe script under an exec-capable scratch
/// dir, returning the guard plus its path.
fn probe_script(tag: &str, body: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new_exec(tag).expect("exec dir");
    let path = dir.path().join("probe");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("probe fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let owned = path.clone();
    (dir, owned)
}

/// Render the shell-observed restore state for comparison.
fn render_restored(force: Option<&str>, lib: Option<&str>) -> Vec<u8> {
    let (force_set, force) = match force {
        Some(value) => ("x", value),
        None => ("", ""),
    };
    let (lib_set, lib) = match lib {
        Some(value) => ("x", value),
        None => ("", ""),
    };
    format!("force_set={force_set}\nforce={force}\nlib_set={lib_set}\nlib={lib}\n").into_bytes()
}

/// Check one restore row: the caller policy arrives as `SHDEPS_*`
/// environment (the provider binds its `_DOT_SHDEPS_CALLER_*` markers
/// from those at source time, so staging the markers directly would
/// be overwritten before the snippet runs). The snippet then
/// overwrites both variables with derived values, restores, and
/// reports the outcome.
fn check_restore(
    tag: &str,
    caller_force: Option<&str>,
    caller_lib: Option<&str>,
    mangled_force: &str,
    mangled_lib: &str,
) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = Fixture::build(&format!("restore-{tag}"));
    let mut extra = vec![];
    if let Some(policy) = caller_force {
        extra.push(("SHDEPS_FORCE", policy));
    }
    if let Some(policy) = caller_lib {
        extra.push(("SHDEPS_LIB", policy));
    }
    let snippet = format!(
        "SHDEPS_FORCE={} SHDEPS_LIB={}\n\
         _dot_shdeps_restore_caller_env\n\
         printf 'force_set=%s\\nforce=%s\\nlib_set=%s\\nlib=%s\\n' \
         \"${{SHDEPS_FORCE+x}}\" \"${{SHDEPS_FORCE-}}\" \
         \"${{SHDEPS_LIB+x}}\" \"${{SHDEPS_LIB-}}\"\n",
        sq(mangled_force),
        sq(mangled_lib),
    );
    let (code, out, err) = shell_run(&fixture.root, &repo, &extra, SOURCES_ENV, &snippet);
    assert_eq!(code, 0, "harness exit for restore {tag}");
    assert_eq!(err, b"", "restore {tag} is silent");
    // The source-time binding maps a set caller variable to the
    // `"x"` marker (whatever its value, even empty) and an unset
    // one to `""` with an empty value.
    let (force_set, force) = match caller_force {
        Some(policy) => ("x", policy),
        None => ("", ""),
    };
    let (lib_set, lib) = match caller_lib {
        Some(policy) => ("x", policy),
        None => ("", ""),
    };
    let restored = dot::shdeps_env_abi::restore_caller_env(force_set, force, lib_set, lib);
    let rust_out = render_restored(restored.force.as_deref(), restored.lib.as_deref());
    assert_eq!(rust_out, out, "restore_caller_env for {tag}");
}

#[test]
fn restore_caller_policy_matrix() {
    check_restore(
        "both-set",
        Some("1"),
        Some("/caller/lib.sh"),
        "9",
        "/mangled.sh",
    );
    check_restore("both-unset", None, None, "9", "/mangled.sh");
    check_restore("force-only", Some("0"), None, "9", "/mangled.sh");
    check_restore("lib-only", None, Some("/caller/lib.sh"), "9", "/mangled.sh");
    check_restore("empty-caller", Some(""), Some(""), "9", "/mangled.sh");
    check_restore("mangled-empty", Some("1"), Some("/caller/lib.sh"), "", "");
}

/// One configure row: preset provider directories plus the
/// force/quiet flags. `home` is `"fixture"` for the row directory or
/// a literal path override.
struct ConfigureRow<'a> {
    home: &'a str,
    xdg_config: Option<&'a str>,
    install: Option<&'a str>,
    bin: Option<&'a str>,
    gitdev: Option<&'a str>,
    dot_force: Option<&'a str>,
    dot_quiet: Option<&'a str>,
}

impl Default for ConfigureRow<'_> {
    fn default() -> Self {
        ConfigureRow {
            home: "fixture",
            xdg_config: None,
            install: None,
            bin: None,
            gitdev: None,
            dot_force: None,
            dot_quiet: None,
        }
    }
}

/// Check one configure row: the presets arrive as environment and
/// both sides report the exported values plus whether the flags were
/// set.
fn check_configure(tag: &str, row: &ConfigureRow<'_>) {
    let home = row.home;
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = Fixture::build(&format!("configure-{tag}"));
    // The row home is a value, not necessarily a directory: a
    // relative spelling exercises the unresolvable path while the
    // child still starts in the existing fixture directory.
    let home_dir = if home == "fixture" || !Path::new(home).is_absolute() {
        fixture.root.clone()
    } else {
        PathBuf::from(home)
    };
    let mut extra = vec![];
    if home != "fixture" && Path::new(&home_dir) != Path::new(home) {
        extra.push(("HOME", home));
    }
    if let Some(value) = row.xdg_config {
        extra.push(("XDG_CONFIG_HOME", value));
    }
    if let Some(value) = row.install {
        extra.push(("SHDEPS_INSTALL_DIR", value));
    }
    if let Some(value) = row.bin {
        extra.push(("SHDEPS_BIN_DIR", value));
    }
    if let Some(value) = row.gitdev {
        extra.push(("SHDEPS_GIT_DEV_DIR", value));
    }
    if let Some(value) = row.dot_force {
        extra.push(("DOT_FORCE", value));
    }
    if let Some(value) = row.dot_quiet {
        extra.push(("DOT_QUIET", value));
    }
    let snippet = "if _dot_shdeps_configure_env; then code=0; else code=$?; fi\n\
         printf 'rc=%s\\nconf=%s\\nhooks=%s\\ninst=%s\\nbin=%s\\ndev=%s\\nforce_set=%s\\nforce=%s\\nquiet_set=%s\\nquiet=%s\\n' \
         \"$code\" \"${SHDEPS_CONF_DIR-}\" \"${SHDEPS_HOOKS_DIR-}\" \
         \"${SHDEPS_INSTALL_DIR-}\" \"${SHDEPS_BIN_DIR-}\" \"${SHDEPS_GIT_DEV_DIR-}\" \
         \"${SHDEPS_FORCE+x}\" \"${SHDEPS_FORCE-}\" \
         \"${SHDEPS_QUIET+x}\" \"${SHDEPS_QUIET-}\"\n";
    let (code, out, err) = shell_run(&home_dir, &repo, &extra, SOURCES_ENV, snippet);
    assert_eq!(code, 0, "harness exit for configure {tag}");
    assert_eq!(err, b"", "configure {tag} is silent");
    // The port takes the row home as a parameter, exactly like the
    // shell reads `$HOME`: the fixture path for `"fixture"` rows,
    // the literal spelling otherwise.
    let home_str;
    let home_value = if home == "fixture" {
        home_str = home_dir.to_str().expect("row home is utf-8").to_string();
        home_str.as_str()
    } else {
        home
    };
    let inputs = dot::shdeps_env_abi::ConfigureInputs {
        xdg_config_home: row.xdg_config.unwrap_or(""),
        home: home_value,
        install_dir: row.install.unwrap_or(""),
        bin_dir: row.bin.unwrap_or(""),
        git_dev_dir: row.gitdev.unwrap_or(""),
        dot_force: row.dot_force.unwrap_or(""),
        dot_quiet: row.dot_quiet.unwrap_or(""),
    };
    let rust_out = match dot::shdeps_env_abi::configure_env(&inputs) {
        Some(env) => {
            let (force_set, force) = if env.force { ("x", "1") } else { ("", "") };
            let (quiet_set, quiet) = if env.quiet { ("x", "1") } else { ("", "") };
            format!(
                "rc=0\nconf={}\nhooks={}\ninst={}\nbin={}\ndev={}\n\
                 force_set={force_set}\nforce={force}\nquiet_set={quiet_set}\nquiet={quiet}\n",
                env.conf_dir, env.hooks_dir, env.install_dir, env.bin_dir, env.git_dev_dir,
            )
            .into_bytes()
        }
        None => b"rc=1\nconf=\nhooks=\ninst=\nbin=\ndev=\n\
              force_set=\nforce=\nquiet_set=\nquiet=\n"
            .to_vec(),
    };
    assert_eq!(
        rust_out, out,
        "configure_env for {tag} (home={home}, xdg={:?})",
        row.xdg_config,
    );
}

#[test]
fn configure_defaults_and_xdg() {
    check_configure("defaults", &ConfigureRow::default());
    check_configure(
        "xdg-absolute",
        &ConfigureRow {
            xdg_config: Some("/tmp/dot-xdg-conf"),
            ..Default::default()
        },
    );
    check_configure(
        "xdg-relative",
        &ConfigureRow {
            xdg_config: Some("rel/conf"),
            ..Default::default()
        },
    );
    check_configure(
        "root-home",
        &ConfigureRow {
            home: "/",
            ..Default::default()
        },
    );
}

#[test]
fn configure_overrides() {
    check_configure(
        "full-override",
        &ConfigureRow {
            install: Some("/opt/shdeps"),
            bin: Some("/opt/bin"),
            gitdev: Some("/opt/git"),
            ..Default::default()
        },
    );
    check_configure(
        "empty-means-default",
        &ConfigureRow {
            install: Some(""),
            bin: Some(""),
            gitdev: Some(""),
            ..Default::default()
        },
    );
    check_configure(
        "partial-override",
        &ConfigureRow {
            install: Some("/opt/shdeps"),
            gitdev: Some("/opt/git"),
            ..Default::default()
        },
    );
    check_configure(
        "unresolvable-home",
        &ConfigureRow {
            home: "relative-home",
            ..Default::default()
        },
    );
}

#[test]
fn configure_force_quiet_flags() {
    check_configure(
        "quiet-path",
        &ConfigureRow {
            dot_force: Some("0"),
            dot_quiet: Some("0"),
            ..Default::default()
        },
    );
    check_configure(
        "force-set",
        &ConfigureRow {
            dot_force: Some("1"),
            ..Default::default()
        },
    );
    check_configure(
        "quiet-set",
        &ConfigureRow {
            dot_quiet: Some("1"),
            ..Default::default()
        },
    );
    check_configure(
        "both-set",
        &ConfigureRow {
            dot_force: Some("1"),
            dot_quiet: Some("1"),
            ..Default::default()
        },
    );
    check_configure(
        "empty-flags",
        &ConfigureRow {
            dot_force: Some(""),
            dot_quiet: Some(""),
            ..Default::default()
        },
    );
    check_configure(
        "non-one",
        &ConfigureRow {
            dot_force: Some("2"),
            dot_quiet: Some("yes"),
            ..Default::default()
        },
    );
    check_configure(
        "padded-one",
        &ConfigureRow {
            dot_force: Some(" 1 "),
            dot_quiet: Some("+1"),
            ..Default::default()
        },
    );
    check_configure(
        "leading-zero",
        &ConfigureRow {
            dot_force: Some("01"),
            dot_quiet: Some("00"),
            ..Default::default()
        },
    );
}

/// Check one bounded-run row: the command vector runs on both sides
/// and the harness compares the status plus the exact stdout bytes.
/// The capture file keeps trailing newlines intact, which command
/// substitution would strip. `expect_shell_stderr` is `None` when the
/// shell side must stay silent and `Some` text it must contain: the
/// timeout warning and the supervisor's job-control notices are
/// caller UI the port folds into its status.
fn check_bounded(
    tag: &str,
    timeout: &str,
    label: &str,
    mode: &str,
    argv: &[&str],
    expect_shell_stderr: Option<&str>,
) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = Fixture::build(&format!("bounded-{tag}"));
    let cap = sq(&fixture.root.join("cap").to_string_lossy());
    let mut snippet = format!(
        "_dot_shdeps_run_bounded {} {} {}",
        sq(timeout),
        sq(label),
        sq(mode),
    );
    for arg in argv {
        snippet.push(' ');
        snippet.push_str(&sq(arg));
    }
    snippet.push_str(&format!(
        " >{cap}; code=$?; printf 'RC=%s\\n' \"$code\"; command cat {cap}"
    ));
    let (code, out, err) = shell_run(&fixture.root, &repo, &[], SOURCES_RUN, &snippet);
    assert_eq!(code, 0, "harness exit for bounded {tag}");
    let shell_err = String::from_utf8_lossy(&err);
    match expect_shell_stderr {
        Some(warning) => assert!(
            shell_err.contains(warning),
            "bounded {tag} reports on stderr: {shell_err:?}",
        ),
        None => assert_eq!(shell_err, "", "bounded {tag} is silent: {shell_err:?}"),
    }
    let argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
    let outcome = dot::shdeps_env_abi::run_bounded(timeout, label, mode, &argv);
    let mut rust_out = format!("RC={}\n", outcome.status).into_bytes();
    rust_out.extend_from_slice(&outcome.stdout);
    assert_eq!(rust_out, out, "run_bounded for {tag}");
}

#[test]
fn bounded_run_passthrough() {
    check_bounded(
        "echo",
        "5",
        "echo label",
        "discard-stderr",
        &["echo", "hello"],
        None,
    );
    check_bounded(
        "two-lines",
        "5",
        "multiline",
        "discard-stderr",
        &["printf", "a\\nb\\n"],
        None,
    );
    check_bounded("true", "5", "quiet true", "discard-stderr", &["true"], None);
    check_bounded(
        "exit-code",
        "5",
        "exit three",
        "discard-stderr",
        &["bash", "-c", "echo partial; exit 3"],
        None,
    );
    check_bounded(
        "empty-output",
        "5",
        "silent",
        "discard-stderr",
        &["bash", "-c", "exit 0"],
        None,
    );
}

#[test]
fn bounded_run_failures() {
    check_bounded(
        "missing-command",
        "5",
        "no such binary",
        "discard-stderr",
        &["/nonexistent-dot-fixture-xyz"],
        None,
    );
    check_bounded(
        "timeout",
        "1",
        "slow sleeper",
        "discard-stderr",
        &["sleep", "30"],
        Some("timed out"),
    );
    check_bounded(
        "signaled",
        "5",
        "self kill",
        "discard-stderr",
        &["bash", "-c", "kill -9 $$"],
        Some("Killed"),
    );
    check_bounded(
        "stderr-discarded",
        "5",
        "noisy failure",
        "discard-stderr",
        &["bash", "-c", "echo out; echo err >&2; exit 4"],
        None,
    );
}

#[test]
fn bounded_run_usage_errors() {
    check_bounded(
        "zero-timeout",
        "0",
        "label",
        "discard-stderr",
        &["echo", "x"],
        None,
    );
    check_bounded(
        "empty-timeout",
        "",
        "label",
        "discard-stderr",
        &["echo", "x"],
        None,
    );
    check_bounded(
        "alpha-timeout",
        "soon",
        "label",
        "discard-stderr",
        &["echo", "x"],
        None,
    );
    check_bounded(
        "padded-timeout",
        " 5",
        "label",
        "discard-stderr",
        &["echo", "x"],
        None,
    );
    check_bounded(
        "empty-label",
        "5",
        "",
        "discard-stderr",
        &["echo", "x"],
        None,
    );
    check_bounded("no-command", "5", "label", "discard-stderr", &[], None);
    check_bounded(
        "bad-mode",
        "5",
        "label",
        "noisy-stderr",
        &["echo", "x"],
        None,
    );
}

/// Inherited diagnostics stay out of the compared stdout; the shell
/// side must show them on its stderr while the status still agrees.
#[test]
fn bounded_run_stderr_inherit() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = Fixture::build("bounded-inherit");
    let cap = sq(&fixture.root.join("cap").to_string_lossy());
    let argv = ["bash", "-c", "echo out; echo err >&2"];
    let mut snippet = format!(
        "_dot_shdeps_run_bounded {} {} {}",
        sq("5"),
        sq("loud"),
        sq("inherit-stderr"),
    );
    for arg in &argv {
        snippet.push(' ');
        snippet.push_str(&sq(arg));
    }
    snippet.push_str(&format!(
        " >{cap}; code=$?; printf 'RC=%s\\n' \"$code\"; command cat {cap}"
    ));
    let (code, out, err) = shell_run(&fixture.root, &repo, &[], SOURCES_RUN, &snippet);
    assert_eq!(code, 0, "harness exit for inherit");
    assert!(
        String::from_utf8_lossy(&err).contains("err"),
        "inherited diagnostics surface on shell stderr",
    );
    let argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
    let outcome = dot::shdeps_env_abi::run_bounded("5", "loud", "inherit-stderr", &argv);
    let mut rust_out = format!("RC={}\n", outcome.status).into_bytes();
    rust_out.extend_from_slice(&outcome.stdout);
    assert_eq!(rust_out, out, "run_bounded inherit-stderr");
}

/// Check one probe row: the fixture binary runs under the timeout
/// raw value on both sides; both report the stripped text or refuse.
fn check_abi_version(tag: &str, script: Option<&str>, timeout_raw: Option<&str>) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = Fixture::build(&format!("abiver-{tag}"));
    let (_exec_dir, binary) = match script {
        Some(body) => {
            let (dir, path) = probe_script(&format!("abiver-{tag}"), body);
            (Some(dir), path)
        }
        None => (None, fixture.root.join("missing-probe")),
    };
    let mut extra = vec![];
    if let Some(raw) = timeout_raw {
        extra.push(("_DOT_SHDEPS_ABI_TIMEOUT_SECONDS", raw));
    }
    // Invoke the probe uncaptured, exactly like the engine does:
    // capturing the probe itself nests its inner bounded run one
    // level deeper than any engine call shape, which reads back
    // empty on this bash (0/10 successes measured); the comparison
    // rows below call the sibling comparison uncaptured as well.
    let snippet = format!(
        "_dot_shdeps_binary_abi_version {}\n\
         code=$?\n\
         printf 'rc=%s\\nout=%s\\n' \"$code\" \"$REPLY\"\n",
        sq(&binary.to_string_lossy()),
    );
    let (code, out, err) = shell_run(&fixture.root, &repo, &extra, SOURCES_RUN, &snippet);
    assert_eq!(code, 0, "harness exit for abi_version {tag}");
    assert_eq!(err, b"", "abi_version {tag} is silent");
    let rust_out = match dot::shdeps_env_abi::abi_version(&binary, timeout_raw.unwrap_or("")) {
        Some(text) => format!("rc=0\nout={text}\n").into_bytes(),
        None => b"rc=1\nout=\n".to_vec(),
    };
    assert_eq!(rust_out, out, "abi_version for {tag}");
}

#[test]
fn abi_version_probes() {
    check_abi_version("pinned", Some("echo 'abi:12'"), None);
    check_abi_version("two-lines", Some("printf 'abi:12\\nextra\\n'"), None);
    check_abi_version("unrelated-output", Some("echo 'hello world'"), Some(""));
    check_abi_version("failing", Some("echo 'abi:12'; exit 7"), None);
    check_abi_version("missing", None, None);
    check_abi_version("slow-default-timeout", Some("echo 'abi:12'"), Some("soon"));
    check_abi_version("explicit-timeout", Some("echo 'abi:9'"), Some("5"));
}

/// Non-executable regular files refuse on both sides without
/// spawning.
#[test]
fn abi_version_non_executable() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = Fixture::build("abiver-noexec");
    let path = fixture.root.join("probe");
    std::fs::write(&path, "#!/bin/sh\necho 'abi:12'\n").expect("probe fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let snippet = format!(
        "_dot_shdeps_binary_abi_version {}\n\
         code=$?\n\
         printf 'rc=%s\\nout=%s\\n' \"$code\" \"$REPLY\"\n",
        sq(&path.to_string_lossy()),
    );
    let (code, out, err) = shell_run(&fixture.root, &repo, &[], SOURCES_RUN, &snippet);
    assert_eq!(code, 0, "harness exit for non-executable");
    assert_eq!(err, b"", "non-executable probe is silent");
    assert_eq!(
        dot::shdeps_env_abi::abi_version(&path, ""),
        None,
        "rust refuses the non-executable probe",
    );
    assert_eq!(
        out, b"rc=1\nout=\n",
        "shell refuses the non-executable probe",
    );
}

/// Check one comparison row: the shell reads the expected ABI from
/// its own lock fixture while the port takes the same value as a
/// parameter (the part-1 reader stays the single lock owner), so
/// `expected` always equals the staged lock abi here.
fn check_binary_abi(tag: &str, lock_abi: &str, script: Option<&str>, expected: &str) {
    let fixture = lock_fixture(&format!("binabi-{tag}"), &lock_with_abi(lock_abi));
    let (_exec_dir, binary) = match script {
        Some(body) => {
            let (dir, path) = probe_script(&format!("binabi-{tag}"), body);
            (Some(dir), path)
        }
        None => (None, fixture.root.join("missing-probe")),
    };
    let snippet = "if _dot_shdeps_binary_abi; then code=0; else code=$?; fi\n\
         printf 'rc=%s\\n' \"$code\"\n";
    let binary_str = binary.to_string_lossy().into_owned();
    let extra = [("_SHDEPSW_BIN", binary_str.as_str())];
    let (code, out, err) = shell_run(&fixture.root, &fixture.root, &extra, SOURCES_RUN, snippet);
    assert_eq!(code, 0, "harness exit for binary_abi {tag}");
    assert_eq!(err, b"", "binary_abi {tag} is silent");
    let rust_ok = dot::shdeps_env_abi::binary_abi(&binary, expected, "");
    let rust_out = if rust_ok {
        b"rc=0\n".to_vec()
    } else {
        b"rc=1\n".to_vec()
    };
    assert_eq!(rust_out, out, "binary_abi for {tag}");
}

#[test]
fn binary_abi_matrix() {
    check_binary_abi("match", "12", Some("echo 'abi:12'"), "12");
    check_binary_abi("mismatch", "12", Some("echo 'abi:13'"), "12");
    check_binary_abi("probe-fails", "12", Some("echo 'abi:12'; exit 3"), "12");
    check_binary_abi("missing-binary", "12", None, "12");
    check_binary_abi(
        "long-abi",
        "9876543210",
        Some("echo 'abi:9876543210'"),
        "9876543210",
    );
    check_binary_abi("empty-output", "12", Some("exit 0"), "12");
}
