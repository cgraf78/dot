//! Native contracts for result lines, section titles, palette selection, and
//! pass/warn/fail counters.
//!
//! One-field calls pass `None` as the detail while two-field calls pass
//! `Some`, including an empty detail. Both empty and colored palettes are
//! exercised over the complete message and detail matrices.

use dot::doctor_runtime::{Counts, Palette, fail, ok, resolve_palette, section, skip, warn};

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
    for func in ["ok", "warn", "fail", "skip"] {
        for message in MESSAGES {
            for detail in DETAILS {
                for markers in [false, true] {
                    let palette = if markers {
                        marker_palette()
                    } else {
                        Palette::empty()
                    };
                    let mut counts = Counts::new();
                    let rust = rust_render(func, &mut counts, &palette, message, *detail);
                    assert!(rust.ends_with(b"\n"));
                    if !message.is_empty() {
                        assert!(rust.windows(message.len()).any(|window| window == *message));
                    }
                    if let Some(detail) = detail {
                        if !detail.is_empty() {
                            assert!(rust.windows(detail.len()).any(|window| window == *detail));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn section_rows_agree() {
    for message in MESSAGES {
        for markers in [false, true] {
            let palette = if markers {
                marker_palette()
            } else {
                Palette::empty()
            };
            let mut counts = Counts::new();
            let output = rust_render("section", &mut counts, &palette, message, None);
            assert!(output.starts_with(b"\n"));
            assert!(output.ends_with(b"\n"));
            if !message.is_empty() {
                assert!(
                    output
                        .windows(message.len())
                        .any(|window| window == *message)
                );
            }
            assert_eq!(counts, Counts::new(), "sections leave counts alone");
        }
    }
}

#[test]
fn counter_sequence_agrees() {
    for markers in [false, true] {
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
        assert!(rust.ends_with(b"counts=2/2/2\n"));
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
