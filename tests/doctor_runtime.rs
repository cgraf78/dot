//! Differential parity tests for `src/doctor_runtime.rs` against the
//! live shell (`lib/dot/doctor/runtime.sh`): the `ok` / `warn` /
//! `fail` / `skip` result lines, the `section` titles, and the
//! pass/warn/fail counters. Every case runs the live shell function
//! and its Rust twin on identical inputs and compares stdout bytes
//! exactly, plus the counter deltas.
//!
//! Arity contract: the shell tests `$#`, so one-argument calls pass
//! `None` as the detail while two-argument calls pass `Some` (even
//! empty) — mirroring how `_dot_doctor_render_records` always
//! forwards `$detail`. Palette slots default to empty (a piped
//! harness never takes the `[[ -t 1 ]]` color arm); cases needing
//! escapes set `_DR_*` markers in the snippet, like the progress-UI
//! harness sets `_C_*`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::doctor_runtime::{Counts, Palette, fail, ok, resolve_palette, section, skip, warn};
use dot::test_support::TempDir;

/// Sources for the doctor-runtime cluster.
const SOURCES: &str = ". \"$1/lib/dot/doctor/runtime.sh\"\n";

/// Marker assignments proving palette-slot selection.
const MARKERS: &str = "_DR_GREEN='<G>'; _DR_YELLOW='<Y>'; _DR_RED='<E>'; \
     _DR_DIM='<D>'; _DR_BOLD='<B>'; _DR_RESET='<R>'; ";

/// Messages exercised for every rendering function, as raw bytes:
/// plain, empty, spaced, percent/paren (printf-hostile), multibyte,
/// tab, and embedded newline.
const MESSAGES: &[&[u8]] = &[
    b"hello",
    b"",
    b"a b",
    b"100% (done)",
    "héllo ✓".as_bytes(),
    b"a\tb",
    b"line1\nline2",
];

/// Detail arities: absent (one-argument call), empty, short, spaced.
const DETAILS: &[Option<&[u8]>] = &[None, Some(b""), Some(b"d"), Some(b"de tail")];

/// Run one shell snippet with `runtime.sh` sourced. `argv` arrives
/// as `$2..`; `extra_env` sets (`Some`) or removes (`None`)
/// variables.
fn shell_run(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
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
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
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
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Marker palette matching the harness preamble.
fn marker_palette() -> Palette {
    Palette {
        green: "<G>".to_string(),
        yellow: "<Y>".to_string(),
        red: "<E>".to_string(),
        dim: "<D>".to_string(),
        bold: "<B>".to_string(),
        reset: "<R>".to_string(),
    }
}

/// Shell call for `func` with the message in `$2` and — when
/// `detail` is `Some` — the detail in `$3`.
fn snippet_for(func: &str, detail: Option<&[u8]>, markers: bool) -> String {
    let mut snippet = String::new();
    if markers {
        snippet.push_str(MARKERS);
    }
    let call = match (func, detail) {
        ("ok", None) => "_dr_ok \"$2\"",
        ("ok", Some(_)) => "_dr_ok \"$2\" \"$3\"",
        ("warn", None) => "_dr_warn \"$2\"",
        ("warn", Some(_)) => "_dr_warn \"$2\" \"$3\"",
        ("fail", None) => "_dr_fail \"$2\"",
        ("fail", Some(_)) => "_dr_fail \"$2\" \"$3\"",
        ("skip", None) => "_dr_skip \"$2\"",
        ("skip", Some(_)) => "_dr_skip \"$2\" \"$3\"",
        ("section", _) => "_dr_section \"$2\"",
        (other, _) => panic!("unknown doctor function {other}"),
    };
    snippet.push_str(call);
    snippet
}

/// Rust twin of the shell call: renders and returns stdout bytes.
fn rust_render(
    func: &str,
    counts: &mut Counts,
    palette: &Palette,
    message: &[u8],
    detail: Option<&[u8]>,
) -> Vec<u8> {
    match func {
        "ok" => ok(counts, palette, message, detail),
        "warn" => warn(counts, palette, message, detail),
        "fail" => fail(counts, palette, message, detail),
        "skip" => skip(palette, message, detail),
        "section" => section(palette, message),
        other => panic!("unknown doctor function {other}"),
    }
}

#[test]
fn result_rows_agree() {
    let dir = TempDir::new("doctor-runtime-rows").expect("fixture dir");
    for func in ["ok", "warn", "fail", "skip"] {
        for message in MESSAGES {
            for detail in DETAILS {
                for markers in [false, true] {
                    let argv: Vec<&OsStr> = match detail {
                        None => vec![OsStr::from_bytes(message)],
                        Some(detail) => vec![OsStr::from_bytes(message), OsStr::from_bytes(detail)],
                    };
                    let (code, out, err) =
                        shell_run(dir.path(), &argv, &[], &snippet_for(func, *detail, markers));
                    assert_eq!(code, 0, "harness exit for {func} {message:?}");
                    assert!(
                        err.is_empty(),
                        "shell stderr for {func} {message:?}: {err:?}"
                    );
                    let palette = if markers {
                        marker_palette()
                    } else {
                        Palette::empty()
                    };
                    let mut counts = Counts::new();
                    let rust = rust_render(func, &mut counts, &palette, message, *detail);
                    assert_eq!(
                        rust, out,
                        "parity for {func} {message:?} {detail:?} {markers}"
                    );
                }
            }
        }
    }
}

#[test]
fn section_rows_agree() {
    let dir = TempDir::new("doctor-runtime-section").expect("fixture dir");
    for message in MESSAGES {
        for markers in [false, true] {
            let argv = [OsStr::from_bytes(message)];
            let (code, out, err) = shell_run(
                dir.path(),
                &argv,
                &[],
                &snippet_for("section", None, markers),
            );
            assert_eq!(code, 0, "harness exit for section {message:?}");
            assert!(
                err.is_empty(),
                "shell stderr for section {message:?}: {err:?}"
            );
            let palette = if markers {
                marker_palette()
            } else {
                Palette::empty()
            };
            let mut counts = Counts::new();
            assert_eq!(
                rust_render("section", &mut counts, &palette, message, None),
                out,
                "section parity for {message:?} {markers}"
            );
            assert_eq!(counts, Counts::new(), "sections leave counts alone");
        }
    }
}

#[test]
fn counter_sequence_agrees() {
    let dir = TempDir::new("doctor-runtime-counts").expect("fixture dir");
    for markers in [false, true] {
        let mut snippet = String::new();
        if markers {
            snippet.push_str(MARKERS);
        }
        snippet.push_str(
            "_dr_ok \"a\"; _dr_ok \"b\" \"d\"; _dr_warn \"w\"; _dr_warn \"w2\" \"wd\"; \
             _dr_fail \"f\"; _dr_fail \"f2\" \"fd\"; _dr_skip \"s\" \"sd\"; _dr_section \"t\"; \
             printf 'counts=%s/%s/%s\\n' \"$_DR_PASS_COUNT\" \"$_DR_WARN_COUNT\" \"$_DR_FAIL_COUNT\"",
        );
        let (code, out, err) = shell_run(dir.path(), &[], &[], &snippet);
        assert_eq!(code, 0, "harness exit for markers={markers}");
        assert!(
            err.is_empty(),
            "shell stderr for markers={markers}: {err:?}"
        );

        let palette = if markers {
            marker_palette()
        } else {
            Palette::empty()
        };
        let mut counts = Counts::new();
        let mut rust = Vec::new();
        rust.extend_from_slice(&ok(&mut counts, &palette, b"a", None));
        rust.extend_from_slice(&ok(&mut counts, &palette, b"b", Some(b"d")));
        rust.extend_from_slice(&warn(&mut counts, &palette, b"w", None));
        rust.extend_from_slice(&warn(&mut counts, &palette, b"w2", Some(b"wd")));
        rust.extend_from_slice(&fail(&mut counts, &palette, b"f", None));
        rust.extend_from_slice(&fail(&mut counts, &palette, b"f2", Some(b"fd")));
        rust.extend_from_slice(&skip(&palette, b"s", Some(b"sd")));
        rust.extend_from_slice(&section(&palette, b"t"));
        rust.extend_from_slice(
            format!("counts={}/{}/{}\n", counts.pass, counts.warn, counts.fail).as_bytes(),
        );
        assert_eq!(rust, out, "sequence parity for markers={markers}");
        assert_eq!(
            counts,
            Counts {
                pass: 2,
                warn: 2,
                fail: 2,
            },
            "skips and sections leave counts alone"
        );
    }
}

#[test]
fn palette_resolution_pins_shell_rule() {
    // `[[ -t 1 && -z "${NO_COLOR:-}" ]]`: colors exactly on a
    // terminal with `NO_COLOR` unset or empty. Any non-empty value —
    // even `"0"` — disables them, and a pipe never colors.
    for (tty, no_color, colored) in [
        (false, None, false),
        (false, Some(""), false),
        (false, Some("1"), false),
        (true, None, true),
        (true, Some(""), true),
        (true, Some("1"), false),
        (true, Some("0"), false),
    ] {
        assert_eq!(
            resolve_palette(tty, no_color),
            if colored {
                Palette::ansi()
            } else {
                Palette::empty()
            },
            "palette for tty={tty} no_color={no_color:?}"
        );
    }
}

#[test]
fn ansi_slots_pin_shell_escapes() {
    // The exact escapes `runtime.sh` installs under `[[ -t 1 ]]`
    // without `NO_COLOR`.
    let palette = Palette::ansi();
    assert_eq!(palette.green, "\x1b[32m");
    assert_eq!(palette.yellow, "\x1b[33m");
    assert_eq!(palette.red, "\x1b[31m");
    assert_eq!(palette.dim, "\x1b[2m");
    assert_eq!(palette.bold, "\x1b[1m");
    assert_eq!(palette.reset, "\x1b[0m");
    assert_eq!(Counts::new(), Counts::default());
    assert_eq!(
        Counts::new(),
        Counts {
            pass: 0,
            warn: 0,
            fail: 0,
        }
    );
}
