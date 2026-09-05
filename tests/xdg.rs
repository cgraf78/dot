//! Differential parity tests for XDG resolution against
//! `lib/dot/public/xdg.sh`, same harness shape as `tests/config.rs`:
//! fresh `bash` per case, controlled env, compared end states.

use std::process::{Command, Stdio};

use dot::xdg::{Kind, base, path};

/// Run the shell function for one case; returns (exit code, REPLY).
///
/// All four `XDG_*_HOME` vars are removed first, then only the case's
/// own var is set — `Command` applies env calls in order, so the set
/// must come after the removes.
fn shell_case(kind: &str, suffix: Option<&str>, xdg: &str, home_dir: &str) -> (i32, String) {
    let var = match kind {
        "state" => "XDG_STATE_HOME",
        "cache" => "XDG_CACHE_HOME",
        "data" => "XDG_DATA_HOME",
        _ => "XDG_CONFIG_HOME",
    };
    let call = match suffix {
        Some(s) => format!("dot_xdg_path {kind} {s}"),
        // Unknown kinds exercise the `else → return 2` branch.
        None => format!("dot_xdg_home {kind}"),
    };
    // printf %s avoids trailing-newline ambiguity; the exit code plus
    // REPLY bytes are the whole contract (the shell prints nothing).
    let script = format!(
        ". \"$1/lib/dot/public/xdg.sh\"\n{call}\ncode=$?\nprintf '%s' \"$REPLY\"\nexit $code\n"
    );
    let output = Command::new("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(script)
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env(var, xdg)
        .env("HOME", home_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn rust_case(kind: &str, suffix: Option<&str>, xdg: &str, home_dir: &str) -> (i32, String) {
    let result = match Kind::parse(kind) {
        Ok(parsed) => match suffix {
            Some(s) => path(parsed, s, xdg, home_dir),
            None => base(parsed, xdg, home_dir),
        },
        Err(err) => Err(err),
    };
    match result {
        Ok(reply) => (0, reply),
        Err(err) => (err.code(), String::new()),
    }
}

#[test]
fn rust_matches_shell_on_xdg_matrix() {
    // Suffixes are passed through the shell unquoted on purpose: the
    // corpus avoids glob/whitespace-hostile values (spaces are covered
    // by the Rust unit matrix instead), keeping the harness legible.
    let homes = ["/home/u", "/", "relative", ""];
    let xdgs = ["", "/srv/x", "relative"];
    let kinds = ["config", "state", "cache", "data", "bogus"];
    let suffixes: [Option<&str>; 6] = [
        None,
        Some("dot/config"),
        Some("/abs"),
        Some("trail/"),
        Some("a/../b"),
        Some(".."),
    ];
    for kind in kinds {
        for suffix in suffixes {
            for xdg in xdgs {
                for home_dir in homes {
                    let shell = shell_case(kind, suffix, xdg, home_dir);
                    let rust = rust_case(kind, suffix, xdg, home_dir);
                    assert_eq!(
                        rust, shell,
                        "divergence kind={kind:?} suffix={suffix:?} xdg={xdg:?} home={home_dir:?}"
                    );
                }
            }
        }
    }
}
