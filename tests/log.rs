//! Differential parity tests for log helpers against
//! `lib/dot/log.sh`: quiet gating, stream routing, and the piped
//! (uncolored) output layout. The colored branch needs a tty, so it is
//! pinned byte-exact by unit tests in `src/log.rs` instead.

use std::process::{Command, Stdio};

/// Oracle interpreter, shared with the other differential harnesses (see
/// `dot::test_support::bash`): the child environment is scrubbed, and
/// `execvp` lookup would use that scrubbed PATH.
fn bash_bin() -> &'static std::path::Path {
    dot::test_support::bash()
}

/// Run one shell log function; `quiet`/`no_color` of `None` mean unset.
/// Returns (exit code, stdout, stderr).
fn shell_log(
    function: &str,
    args: &[&str],
    quiet: Option<&str>,
    no_color: Option<&str>,
) -> (i32, String, String) {
    let mut cmd = Command::new(bash_bin());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/log.sh\"\n{function} \"${{@:2}}\"\n",
    ));
    cmd.arg("dot-test-sh").arg(env!("CARGO_MANIFEST_DIR"));
    for arg in args {
        cmd.arg(arg);
    }
    // Scrubbed environment: only the two knobs under test are set.
    // PATH is emptied because nothing external is invoked; the
    // absolute interpreter path above keeps the spawn working.
    cmd.env_clear().env("PATH", "");
    match quiet {
        Some(value) => {
            cmd.env("DOT_QUIET", value);
        }
        None => {
            cmd.env_remove("DOT_QUIET");
        }
    }
    match no_color {
        Some(value) => {
            cmd.env("NO_COLOR", value);
        }
        None => {
            cmd.env_remove("NO_COLOR");
        }
    }
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn rust_log(
    function: &str,
    text: &str,
    quiet: Option<&str>,
    no_color: Option<&str>,
) -> (i32, String, String) {
    // Piped stdout on both sides: colors disabled, like the shell.
    let log = dot::log::Log::from_env(false, no_color, quiet);
    let mut out = Vec::new();
    let mut err = Vec::new();
    match function {
        "_log" => log.log(&mut out, text),
        "_header" => log.header(&mut out, text),
        "_log_header" => log.log_header(&mut out, text),
        "_log_ok" => log.ok(&mut out, text),
        "_log_dim" => log.dim(&mut out, text),
        "_warn" => log.warn(&mut err, text),
        _ => panic!("unknown function {function}"),
    }
    (
        0,
        String::from_utf8(out).expect("utf8"),
        String::from_utf8(err).expect("utf8"),
    )
}

#[test]
fn rust_matches_shell_on_log_matrix() {
    let functions = [
        "_log",
        "_header",
        "_log_header",
        "_log_ok",
        "_log_dim",
        "_warn",
    ];
    // Decimal spellings both engines agree on (exotic bash-arithmetic
    // forms like `0x1` are out of contract; see `is_quiet` docs).
    let quiets: &[Option<&str>] = &[None, Some(""), Some("0"), Some("1"), Some("2")];
    let no_colors: &[Option<&str>] = &[None, Some(""), Some("1")];
    // Two-word message locks the shell `echo "$@"` join contract
    // against the pre-joined Rust `&str`.
    for function in functions {
        for quiet in quiets {
            for no_color in no_colors {
                let shell = shell_log(function, &["hello", "world"], *quiet, *no_color);
                let rust = rust_log(function, "hello world", *quiet, *no_color);
                assert_eq!(
                    rust, shell,
                    "divergence {function} quiet={quiet:?} no_color={no_color:?}: \
                     rust={rust:?} shell={shell:?}"
                );
            }
        }
    }
}
