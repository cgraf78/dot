//! Differential parity tests for platform predicates against
//! `lib/dot/platform.sh`: WSL/platform/host detection, comma-spec
//! matching (inclusions, exclusions, case modes, Termux dual
//! identity), tool lookup, and the sudo ladder.

use std::process::{Command, Stdio};

use dot::platform;

/// Absolute bash: some children override PATH, and `execvp` lookup
/// would use that same PATH — so resolve the interpreter first.
fn bash_bin() -> &'static str {
    for candidate in ["/usr/bin/bash", "/bin/bash"] {
        if std::path::Path::new(candidate).is_file() {
            return candidate;
        }
    }
    panic!("no bash interpreter found");
}

/// Run one shell platform function; `extra_env` sets (`Some`) or
/// removes (`None`) variables, `path_override` replaces PATH.
/// Returns (exit code, stdout).
fn shell_platform(
    function: &str,
    args: &[&str],
    extra_env: &[(&str, Option<&str>)],
    path_override: Option<&str>,
) -> (i32, String) {
    let mut cmd = Command::new(bash_bin());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/platform.sh\"\n{function} \"${{@:2}}\"\n",
    ));
    cmd.arg("dot-test-sh").arg(env!("CARGO_MANIFEST_DIR"));
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env_clear();
    // Pinned C locale: `${var,,}` lowercasing and `sort` order would
    // otherwise depend on the ambient locale.
    cmd.env("LC_ALL", "C");
    match path_override {
        Some(path) => {
            cmd.env("PATH", path);
        }
        None => {
            cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        }
    }
    // `env -i`-style scrub: only the knobs under test survive.
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
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn ambient_shell(function: &str, args: &[&str]) -> (i32, String) {
    shell_platform(function, args, &[], None)
}

#[test]
fn rust_matches_shell_on_live_platform_and_host() {
    // Both engines read the same machine (ambient environment,
    // osrelease file, `uname -s`/`hostname`) with nothing overridden.
    let shell = ambient_shell("_dot_platform", &[]);
    let rust = match platform::detect_platform() {
        Ok(name) => (0, format!("{name}\n")),
        Err(err) => (err.code(), String::new()),
    };
    assert_eq!(
        rust, shell,
        "platform divergence: rust={rust:?} shell={shell:?}"
    );

    let shell = ambient_shell("_dot_host", &[]);
    let rust = match platform::detect_host() {
        Ok(name) => (0, format!("{name}\n")),
        Err(err) => (err.code(), String::new()),
    };
    assert_eq!(
        rust, shell,
        "host divergence: rust={rust:?} shell={shell:?}"
    );
}

#[test]
fn rust_matches_shell_on_wsl_markers() {
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    // (distro, interop) marker matrix; the shell child gets the same
    // markers, the Rust side composes the same inputs explicitly, and
    // `uname -s` comes from the shared machine on both sides.
    let uname = Command::new("uname").arg("-s").output().expect("uname -s");
    assert!(uname.status.success());
    let uname = String::from_utf8_lossy(&uname.stdout);
    let uname = uname.trim_end_matches(['\r', '\n']);
    for (distro, interop) in [("", ""), ("Ubuntu", ""), ("", "x"), ("Ubuntu", "x")] {
        let shell = shell_platform(
            "_dot_platform",
            &[],
            &[
                ("WSL_DISTRO_NAME", Some(distro)),
                ("WSL_INTEROP", Some(interop)),
            ],
            None,
        );
        let rust = (
            0,
            format!(
                "{}\n",
                platform::platform_name(
                    uname,
                    platform::is_wsl(distro, interop, osrelease.as_deref()),
                )
            ),
        );
        assert_eq!(
            rust, shell,
            "wsl divergence distro={distro:?} interop={interop:?}: rust={rust:?} shell={shell:?}"
        );
    }
}

#[test]
fn rust_matches_shell_on_spec_matrix() {
    let platform = platform::detect_platform().expect("live platform");
    let specs = [
        "",
        ",",
        "nomatch",
        "!nomatch",
        "!",
        "*,!*",
        "linux,macos,wsl",
        "!linux,!macos,!wsl",
        "LINUX",
        "!LINUX",
    ];
    for prefix in [None, Some("/data/data/com.termux/files/usr")] {
        let extra: Vec<(&str, Option<&str>)> = match prefix {
            Some(prefix) => vec![("PREFIX", Some(prefix))],
            None => vec![("PREFIX", None)],
        };
        let termux = prefix.is_some_and(|prefix| prefix.contains("/com.termux/"));
        for spec in specs {
            let shell = shell_platform("dot_platform_match", &[spec], &extra, None);
            let rust = match platform::platform_matches(Some(spec), &platform, termux) {
                Ok(true) => (0, String::new()),
                Ok(false) => (1, String::new()),
                Err(err) => (err.code(), String::new()),
            };
            assert_eq!(
                rust, shell,
                "platform-spec divergence spec={spec:?} termux={termux}: rust={rust:?} shell={shell:?}"
            );
        }
    }
    // Arity: zero or two specs are exit 2 on both sides.
    for argv in [vec![], vec!["a", "b"]] {
        let shell = ambient_shell("dot_platform_match", &argv);
        assert_eq!(shell.0, 2, "argv={argv:?} shell={shell:?}");
    }
    assert_eq!(
        platform::platform_matches(None, &platform, false),
        Err(platform::Error::Usage)
    );
}

/// Direct `_dot_match_specs` oracle with hostile currents: glob
/// metacharacters in current values must stay inert on BOTH sides.
/// (An adversarial review caught the exclusion side modeled as a
/// pattern; the shell quotes both right-hand sides.)
#[test]
fn rust_matches_shell_on_raw_spec_matrix() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    struct Case {
        spec: &'static [u8],
        mode: &'static [u8],
        currents: &'static [&'static [u8]],
    }
    // (spec, case_mode, currents)
    let cases = [
        Case {
            spec: b"!anything",
            mode: b"exact",
            currents: &[b"*"],
        },
        Case {
            spec: b"!*",
            mode: b"exact",
            currents: &[b"*"],
        },
        Case {
            spec: b"!linux",
            mode: b"exact",
            currents: &[b"lin*"],
        },
        Case {
            spec: b"!lin*",
            mode: b"exact",
            currents: &[b"lin*"],
        },
        Case {
            spec: b"linux",
            mode: b"exact",
            currents: &[b"lin*"],
        },
        Case {
            spec: b"lin*",
            mode: b"exact",
            currents: &[b"linux"],
        },
        Case {
            spec: b"*,!*",
            mode: b"exact",
            currents: &[b"anything"],
        },
        Case {
            spec: b"[!a]",
            mode: b"exact",
            currents: &[b"b"],
        },
        Case {
            spec: b"[!a]",
            mode: b"exact",
            currents: &[b"[", b"a", b"]"],
        },
        Case {
            spec: b"?",
            mode: b"exact",
            currents: &[b"x"],
        },
        Case {
            spec: b"?",
            mode: b"exact",
            currents: &[b"??"],
        },
        Case {
            spec: b"LINUX,!other",
            mode: b"lowercase",
            currents: &[b"linux"],
        },
        Case {
            spec: b"!LINUX",
            mode: b"lowercase",
            currents: &[b"linux"],
        },
        Case {
            spec: b"a,b",
            mode: b"exact",
            currents: &[b"b", b"c"],
        },
        Case {
            spec: b"a,!b",
            mode: b"exact",
            currents: &[b"a", b"b"],
        },
        Case {
            spec: b"",
            mode: b"exact",
            currents: &[b"x"],
        },
        Case {
            spec: b"linux\nevil",
            mode: b"exact",
            currents: &[b"linux"],
        },
        Case {
            spec: b"nomatch\nlinux",
            mode: b"exact",
            currents: &[b"linux"],
        },
    ];
    for case in &cases {
        let (spec, mode, currents) = (case.spec, case.mode, case.currents);
        let mut cmd = Command::new(bash_bin());
        cmd.arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(". \"$1/lib/dot/platform.sh\"\n_dot_match_specs \"$2\" \"$3\" \"${@:4}\"\n");
        cmd.arg("dot-test-sh");
        cmd.arg(env!("CARGO_MANIFEST_DIR"));
        cmd.arg(OsStr::from_bytes(spec));
        cmd.arg(OsStr::from_bytes(mode));
        for current in currents {
            cmd.arg(OsStr::from_bytes(current));
        }
        let output = cmd
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("spawn bash");
        let shell = output.status.code().unwrap_or(99);
        let spec_str = std::str::from_utf8(spec).expect("spec utf8");
        let lowercase = mode == b"lowercase";
        let current_strs: Vec<&str> = currents
            .iter()
            .map(|c| std::str::from_utf8(c).expect("current utf8"))
            .collect();
        let rust = if platform::match_specs(spec_str, lowercase, &current_strs) {
            0
        } else {
            1
        };
        assert_eq!(
            rust, shell,
            "spec divergence spec={spec:?} mode={mode:?} currents={currents:?}"
        );
    }
}

#[test]
fn rust_matches_shell_on_host_specs() {
    let host = match platform::detect_host() {
        Ok(host) => host,
        Err(_) => return,
    };
    let cases = [
        String::new(),
        host.clone(),
        "!other-host-xyz".to_string(),
        format!("!{host}"),
        "A,B".to_string(),
        host.to_ascii_uppercase(),
    ];
    for spec in &cases {
        let shell = ambient_shell("dot_host_match", &[spec]);
        let rust = match platform::host_matches(Some(spec), &host) {
            Ok(true) => (0, String::new()),
            Ok(false) => (1, String::new()),
            Err(err) => (err.code(), String::new()),
        };
        assert_eq!(
            rust, shell,
            "host-spec divergence spec={spec:?}: rust={rust:?} shell={shell:?}"
        );
    }
    assert_eq!(ambient_shell("dot_host_match", &[]).0, 2);
    assert_eq!(
        platform::host_matches(None, &host),
        Err(platform::Error::Usage)
    );
}

#[test]
fn rust_matches_shell_on_tool_lookup() {
    let dir = dot::test_support::TempDir::new("tool-present").expect("temp dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.write("mytool", b"#!/bin/sh\n");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        dir.write("plainfile", b"x");
        let nonexec = dir.write("nonexec", b"x");
        std::fs::set_permissions(&nonexec, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        std::fs::create_dir(dir.path().join("subdir")).expect("mkdir");
    }
    let path_value = dir.path().to_string_lossy().into_owned();
    // (name, expected-present): presence probes print nothing, so the
    // exit code is the whole contract.
    let present = dir.path().join("mytool").to_string_lossy().into_owned();
    let missing = dir.path().join("absent").to_string_lossy().into_owned();
    let subdir = dir.path().join("subdir").to_string_lossy().into_owned();
    // `command -v` needs no exec bit (pinned live): any stat-able
    // non-directory on PATH resolves; directories never do.
    let cases: &[(&str, bool)] = &[
        ("mytool", true),
        ("nonexec", true),
        ("plainfile", true),
        ("subdir", false),
        ("absent-tool-xyz", false),
        (&present, true),
        (&missing, false),
        (&subdir, true),
    ];
    for (name, expected) in cases {
        let shell = shell_platform("_dot_tool_present", &[name], &[], Some(&path_value));
        // Shell exit 0 means present; Rust answers bool.
        let rust = if platform::tool_present(Some(name), &path_value).expect("arity ok") {
            0
        } else {
            1
        };
        assert_eq!(
            rust, shell.0,
            "tool divergence name={name:?}: shell={shell:?}"
        );
        assert_eq!(rust == 0, *expected, "name={name:?}");
    }
    // Arity: zero names are exit 2 on both sides.
    let shell = shell_platform("_dot_tool_present", &[], &[], Some(&path_value));
    assert_eq!(shell.0, 2, "shell={shell:?}");
    assert_eq!(
        platform::tool_present(None, &path_value),
        Err(platform::Error::Usage)
    );
}

#[test]
fn rust_matches_shell_on_sudo_ladder() {
    // stdin is null on both sides: the interactive `sudo true`
    // branch fails fast instead of prompting, keeping the test
    // hermetic. The scrubbed-PATH variant pins the shell's
    // `[[ "" -eq 0 ]]` coercion (missing `id` takes the root
    // fast-path) against the Rust replica.
    let ambient_path = std::env::var("PATH").unwrap_or_default();
    for quiets in [None, Some(""), Some("0"), Some("1"), Some("2")] {
        let mut extra: Vec<(&str, Option<&str>)> = Vec::new();
        match quiets {
            Some(value) => extra.push(("DOT_QUIET", Some(value))),
            None => extra.push(("DOT_QUIET", None)),
        }
        let quiet = quiets.unwrap_or("");
        for path in [ambient_path.as_str(), ""] {
            let shell = shell_platform("_require_sudo", &[], &extra, Some(path));
            // `sudo` and `id` resolve via the process PATH, so run
            // the Rust probe under the same PATH via a serialized
            // swap. Shell exit 0 means escalation available.
            let rust = with_path(path, || if platform::require_sudo(quiet) { 0 } else { 1 });
            assert_eq!(
                rust,
                shell.0,
                "sudo divergence quiet={quiets:?} path-empty={}: shell={shell:?}",
                path.is_empty(),
            );
        }
    }
}

/// Run `f` with the process PATH set to `path`.
///
/// Serialized: PATH is process-global, so this mutex keeps parallel
/// tests from observing the swap.
fn with_path<R>(path: &str, f: impl FnOnce() -> R) -> R {
    static PATH_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = PATH_SERIAL.lock().expect("path mutex");
    let old = std::env::var_os("PATH");
    // SAFETY: the mutex above serializes every PATH swap in this
    // test binary, so no other thread observes the transition.
    unsafe {
        std::env::set_var("PATH", path);
    }
    let result = f();
    unsafe {
        match old {
            Some(old) => std::env::set_var("PATH", old),
            None => std::env::remove_var("PATH"),
        }
    }
    result
}
