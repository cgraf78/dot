//! Differential parity tests for UI helpers against
//! `lib/dot/public/ui.sh`: color maps, hex validation, and the
//! non-gum renderer branches (plain and ANSI, both deterministic
//! without a tty or gum binary).

use std::process::{Command, Stdio};

use dot::ui::{Renderer, color_hex, hex_to_rgb, summary_box, title};

/// Absolute bash: the child environment overrides PATH (to neutralize
/// `gum`), and `execvp` lookup would use that same PATH — so resolve
/// the interpreter before spawning.
fn bash_bin() -> &'static str {
    // Probe Linux and macOS locations (no PATH lookup: the child env
    // overrides PATH, and `Command` would inherit that for resolution).
    for candidate in ["/usr/bin/bash", "/bin/bash"] {
        if std::path::Path::new(candidate).is_file() {
            return candidate;
        }
    }
    panic!("no bash interpreter found");
}

/// Run one shell UI function with piped stdout (never a tty) and empty
/// environment color controls; returns (exit code, stdout).
fn shell_ui(function: &str, args: &[&str]) -> (i32, String) {
    // `NO_COLOR` variants cover the shell's `-z` three-way split.
    let mut cmd = Command::new(bash_bin());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        // $0 is the script name, $1 the tree root, $2+ the function
        // argv: slice off the harness prefix so arity matches exactly.
        .arg(format!(
            ". \"$1/lib/dot/public/ui.sh\"\n{function} \"${{@:2}}\"\n",
        ))
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"));
    for arg in args {
        cmd.arg(arg);
    }
    // Empty PATH neutralizes the local `gum` install: `type -P gum`
    // then fails and the shell takes the deterministic plain branch
    // under this piped stdout, exactly like `Renderer::Plain`.
    let output = cmd
        .env("NO_COLOR", "")
        .env("PATH", "")
        .env_remove("TERM")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn rust_ui_plain(function: &str, args: &[&str]) -> (i32, String) {
    // Plain renderer, no gum: matches piped shell stdout exactly.
    let renderer = Renderer::Plain;
    let mut out = Vec::new();
    let code = match (function, args) {
        ("dot_ui_color_hex", [name]) => match color_hex(name) {
            Ok(hex) => {
                out.extend_from_slice(hex.as_bytes());
                0
            }
            Err(err) => err.code(),
        },
        ("dot_ui_hex_to_rgb", [hex]) => match hex_to_rgb(hex) {
            Ok((r, g, b)) => {
                out.extend_from_slice(format!("{r};{g};{b}").as_bytes());
                0
            }
            Err(err) => err.code(),
        },
        ("dot_ui_title", [text]) => match title(&mut out, &renderer, text) {
            Ok(()) => 0,
            Err(err) => err.code(),
        },
        ("dot_ui_summary_box", [color, text]) => {
            match summary_box(&mut out, &renderer, color, text) {
                Ok(()) => 0,
                Err(err) => err.code(),
            }
        }
        _ => 2,
    };
    (code, String::from_utf8(out).expect("utf8"))
}

#[test]
fn rust_matches_shell_on_ui_matrix() {
    // NO_COLOR="" keeps the shell on its ANSI-capable path only when a
    // tty is present; piped here, both sides render Plain — except the
    // shell still prints ANSI when... no: `[[ -t 1 ]]` is false under a
    // pipe, so shell and Rust both take the plain branch. Deterministic.
    let cases: &[(&str, &[&str])] = &[
        ("dot_ui_color_hex", &["green"]),
        ("dot_ui_color_hex", &["red"]),
        ("dot_ui_color_hex", &["yellow"]),
        ("dot_ui_color_hex", &["magenta"]),
        ("dot_ui_color_hex", &["dim"]),
        ("dot_ui_color_hex", &["#Ab12Cd"]),
        ("dot_ui_color_hex", &["blue"]),
        ("dot_ui_color_hex", &["#abc"]),
        ("dot_ui_hex_to_rgb", &["#3fb950"]),
        ("dot_ui_hex_to_rgb", &["#ABCDEF"]),
        ("dot_ui_hex_to_rgb", &["zzzzzz"]),
        ("dot_ui_title", &["Hello world"]),
        ("dot_ui_summary_box", &["green", "All good"]),
        ("dot_ui_summary_box", &["#d29922", "Watch it"]),
        ("dot_ui_summary_box", &["chartreuse", "Nope"]),
    ];
    for (function, args) in cases {
        let shell = shell_ui(function, args);
        let rust = rust_ui_plain(function, args);
        assert_eq!(
            rust, shell,
            "divergence {function} {args:?}: rust={rust:?} shell={shell:?}"
        );
    }
}

/// Both engines must invoke `gum style` with identical argv: run each
/// through a fixture `gum` that logs its arguments and emits canned
/// output, then compare stdout and the logged argv.
#[test]
fn gum_branch_invokes_identical_argv() {
    use std::os::unix::fs::PermissionsExt;

    // Exec-capable scratch (see `dot::test_support::TempDir::new_exec`):
    // the fixture must RUN (the shell's `style --help` gate), and the
    // system temp dir is `noexec` on some CI images. The guard removes
    // it on drop.
    let scratch = dot::test_support::TempDir::new_exec("ui-gum").expect("fixture dir");
    let dir = scratch.path();
    let log = dir.join("argv.log");
    let fixture = dir.join("gum");
    // `style --help` must succeed (the shell's third gate); log only
    // real invocations.
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\nif [ \"$1\" = style ] && [ \"$2\" = --help ]; then exit 0; fi\nprintf '%s\\n' \"$*\" >>{log}\nprintf 'GUM:%s\\n' \"$*\"\n",
            log = log.display(),
        ),
    )
    .expect("fixture");
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let run_shell = |function: &str, args: &[&str]| -> (i32, String) {
        let mut cmd = Command::new(bash_bin());
        cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
            ". \"$1/lib/dot/public/ui.sh\"\n{function} \"${{@:2}}\"\n",
        ));
        cmd.arg("dot-test-sh").arg(env!("CARGO_MANIFEST_DIR"));
        for arg in args {
            cmd.arg(arg);
        }
        let output = cmd
            .env("PATH", dir)
            .env("NO_COLOR", "")
            .env_remove("TERM")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("spawn bash");
        (
            output.status.code().unwrap_or(99),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    };

    // Sanity: the fixture passes the shell's own usability gate.
    assert!(dot::ui::find_gum(&dir.to_string_lossy()).is_some());
    let gum = dot::ui::find_gum(&dir.to_string_lossy()).expect("fixture gum");
    let renderer = Renderer::Gum { binary: gum };

    for (function, args) in [
        ("dot_ui_title", vec!["Hello world"]),
        ("dot_ui_summary_box", vec!["green", "All good"]),
    ] {
        let _ = std::fs::remove_file(&log);
        let shell = run_shell(function, &args);
        let shell_argv = std::fs::read_to_string(&log).expect("shell argv");
        let _ = std::fs::remove_file(&log);
        let mut out = Vec::new();
        let code = match function {
            "dot_ui_title" => title(&mut out, &renderer, args[0])
                .map(|()| 0)
                .unwrap_or_else(|err| err.code()),
            _ => summary_box(&mut out, &renderer, args[0], args[1])
                .map(|()| 0)
                .unwrap_or_else(|err| err.code()),
        };
        let rust = (code, String::from_utf8(out).expect("utf8"));
        let rust_argv = std::fs::read_to_string(&log).expect("rust argv");
        assert_eq!(rust, shell, "gum output divergence {function}");
        assert_eq!(rust_argv, shell_argv, "gum argv divergence {function}");
    }
    // `scratch` drops here and removes the fixture dir.
}
