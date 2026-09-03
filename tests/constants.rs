//! Differential parity tests for runtime constants against
//! `lib/dot/constants.sh` (plus the `DOT_LIBRARY_API` pin from
//! `lib/dot/public/api-version.sh`).

use std::process::{Command, Stdio};

use dot::constants::resolve;
use dot::version::LIBRARY_API;

/// Oracle interpreter, shared with the other differential harnesses (see
/// `dot::test_support::bash`).
fn bash_bin() -> &'static std::path::Path {
    dot::test_support::bash()
}

/// Source `xdg.sh` + `api-version.sh` + `constants.sh` under the given
/// environment and print the six constants plus the library ABI, one
/// per line. Returns (exit code, lines).
fn shell_constants(
    home: &str,
    xdg_state: Option<&str>,
    source_root: &str,
    quiet: Option<&str>,
    verbose: Option<&str>,
) -> (i32, Vec<String>) {
    let mut cmd = Command::new(bash_bin());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(
            ". \"$1/lib/dot/public/xdg.sh\"\n\
             . \"$1/lib/dot/public/api-version.sh\"\n\
             DOT_SOURCE_ROOT=\"$2\"\n\
             . \"$1/lib/dot/constants.sh\"\n\
             printf '%s\\n' \"$DOT_OVERLAY_MANIFEST\" \"$DOT_OVERLAY_LEGACY_MANIFEST\" \
             \"$DOT_PROFILE_LIFECYCLE_LEDGER\" \"$DOT_BIN\" \"$DOT_QUIET\" \"$DOT_VERBOSE\" \
             \"$DOT_LIBRARY_API\"\n",
        )
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(source_root);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("HOME", home);
    match xdg_state {
        Some(value) => {
            cmd.env("XDG_STATE_HOME", value);
        }
        None => {
            cmd.env_remove("XDG_STATE_HOME");
        }
    }
    match quiet {
        Some(value) => {
            cmd.env("DOT_QUIET", value);
        }
        None => {
            cmd.env_remove("DOT_QUIET");
        }
    }
    match verbose {
        Some(value) => {
            cmd.env("DOT_VERBOSE", value);
        }
        None => {
            cmd.env_remove("DOT_VERBOSE");
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
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

#[test]
fn rust_matches_shell_on_constants_matrix() {
    let home = "/home/fixture-user";
    let source_root = "/opt/dot-checkout";
    // (xdg_state, quiet, verbose); `None` means unset.
    struct Case {
        xdg_state: Option<&'static str>,
        quiet: Option<&'static str>,
        verbose: Option<&'static str>,
    }
    let cases = [
        Case {
            xdg_state: Some("/x/state"),
            quiet: Some("1"),
            verbose: Some("2"),
        },
        Case {
            xdg_state: Some("/x/state"),
            quiet: None,
            verbose: None,
        },
        Case {
            xdg_state: None,
            quiet: None,
            verbose: None,
        },
        Case {
            xdg_state: None,
            quiet: Some(""),
            verbose: Some("0"),
        },
        // Relative XDG values fall back to HOME on both sides.
        Case {
            xdg_state: Some("relative/state"),
            quiet: None,
            verbose: None,
        },
        Case {
            xdg_state: Some(""),
            quiet: None,
            verbose: None,
        },
    ];
    for case in &cases {
        let (xdg_state, quiet, verbose) = (case.xdg_state, case.quiet, case.verbose);
        let (code, lines) = shell_constants(home, xdg_state, source_root, quiet, verbose);
        assert_eq!(code, 0);
        assert_eq!(lines.len(), 7, "lines={lines:?}");
        let rust = resolve(home, xdg_state.unwrap_or(""), source_root, quiet, verbose)
            .expect("resolvable");
        let expected = [
            rust.overlay_manifest.as_str(),
            rust.overlay_legacy_manifest.as_str(),
            rust.profile_lifecycle_ledger.as_str(),
            rust.bin.as_str(),
            rust.quiet.as_str(),
            rust.verbose.as_str(),
            &LIBRARY_API.to_string(),
        ];
        assert_eq!(
            lines, expected,
            "constants divergence xdg={xdg_state:?} quiet={quiet:?} verbose={verbose:?}"
        );
    }
}

#[test]
fn unresolvable_home_fails_on_both_sides() {
    // Relative HOME with no usable XDG state: the shell's
    // `|| return` leaves the XDG-derived constants empty, and Rust
    // refuses to resolve at all.
    let (code, lines) = shell_constants("relative", None, "/src", None, None);
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 7, "lines={lines:?}");
    assert!(lines[0].is_empty(), "manifest must be empty: {lines:?}");
    assert!(lines[2].is_empty(), "ledger must be empty: {lines:?}");
    assert!(resolve("relative", "", "/src", None, None).is_err());
}
