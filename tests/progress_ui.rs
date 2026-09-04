//! Differential parity tests for live-progress helpers against
//! `lib/dot/progress-ui.sh` (plus the `_dot_progress_detail`
//! wrapper from `lib/dot/repos/pull.sh`): status colors, ASCII
//! detection, cell fitting, stage lines, bars, and the summary
//! phrases the update stages report through. Every case runs the
//! live shell function and its Rust twin on identical inputs and
//! compares stdout bytes exactly.
//!
//! Locale contract: bash counts string length in characters under a
//! working UTF-8 locale and in bytes otherwise, so the cell functions
//! take an explicit `multibyte` flag. Tests resolve the flag from the
//! shell itself (`${#glyph}` probe) so expectations stay hermetic;
//! production resolves it with [`dot::progress_ui::utf8_locale`].

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log;
use dot::progress_ui::{
    Palette, ascii_mode, cell, clear_live, color, count_phrase, detail, duration_ms, elapsed, fit,
    item, join_comma, json_get, json_num, line, live_enabled, live_line, now_ms, pad, progress_bar,
    progress_detail, progress_detail_with_label, section, status, utf8_locale,
};
use dot::test_support::TempDir;

/// Environment overlay for [`shell_run`]: each entry sets (`Some`)
/// or removes (`None`) one variable.
type ShellEnv<'a> = Vec<(&'a str, Option<&'a str>)>;

/// Cell-fitting oracle selected per truncation mode.
type FitOracle = fn(&[u8], usize, bool) -> Vec<u8>;

/// Run one shell snippet with `progress-ui.sh` sourced. `argv` arrives
/// as `$2..`; `extra_env` sets (`Some`) or removes (`None`) variables.
/// Palette slots default to distinctive markers so tests prove slot
/// selection; cases needing real escapes set `_C_*` in the snippet.
fn shell_run(
    fixture: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
". \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/repos/pull.sh\"\n. \"$1/lib/dot/log.sh\"\n_C_RESET='<R>'\n_C_BOLD='<B>'\n_C_DIM='<D>'\n_C_GREEN='<G>'\n_C_YELLOW='<Y>'\n_C_RED='<E>'\n_C_BLUE='<U>'\n_C_CYAN='<C>'\n_C_WHITE='<W>'\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("DOT_TEST", "1")
        .env("HOME", fixture)
        .current_dir(fixture)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
    (output.status.code().unwrap_or(99), output.stdout)
}

/// Marker palette matching the harness preamble.
fn marker_palette() -> Palette {
    Palette {
        reset: "<R>".to_string(),
        bold: "<B>".to_string(),
        dim: "<D>".to_string(),
        green: "<G>".to_string(),
        yellow: "<Y>".to_string(),
        red: "<E>".to_string(),
        blue: "<U>".to_string(),
        cyan: "<C>".to_string(),
        white: "<W>".to_string(),
    }
}

/// Ask the shell how it counts characters under `locale_env`: the
/// `${#glyph}` probe from `_ui_ascii_mode` (1 = multibyte works).
/// `locale_env` sets (`Some`) or removes (`None`) `LC_ALL`, `LC_CTYPE`,
/// and `LANG` together, exactly like the mode tests below.
fn shell_multibyte(fixture: &Path, locale_env: &[(&str, Option<&str>)]) -> bool {
    let (code, out) = shell_run(
        fixture,
        &[],
        locale_env,
        "glyph=\"\u{2501}\"; printf '%s' \"${#glyph}\"",
    );
    assert_eq!(code, 0, "shell probe");
    String::from_utf8(out).expect("probe digits").trim() == "1"
}

/// First `locale -a` entry naming a usable non-`C` UTF-8 charmap, if
/// the platform has one. `C`-prefixed names (`C.utf8`, `C.UTF-8`)
/// always take the shell ASCII branch, so only a non-`C` name
/// exercises the unicode glyph paths. Lets those cases run wherever
/// such a locale exists instead of hardcoding a name no platform
/// guarantees.
fn utf8_locale_name() -> Option<String> {
    let output = Command::new("locale")
        .arg("-a")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let listing = String::from_utf8_lossy(&output.stdout);
    listing
        .lines()
        .map(str::trim)
        .filter(|name| !name.starts_with('C') && *name != "POSIX")
        .find(|name| {
            let flat: String = name
                .chars()
                .filter(|c| *c != '-')
                .flat_map(|c| c.to_lowercase())
                .collect();
            flat.contains("utf8")
        })
        .map(str::to_string)
}

#[test]
fn color_selects_palette_slot() {
    let dir = TempDir::new("ui-color").expect("fixture dir");
    let palette = marker_palette();
    for (status, expected) in [
        ("ok", "<G>"),
        ("changed", "<U>"),
        ("running", "<C>"),
        ("warning", "<Y>"),
        ("failed", "<E>"),
        ("detail", "<D>"),
        ("hint", "<D>"),
        ("bogus", ""),
        ("", ""),
        ("OK", ""),
    ] {
        let (code, out) = shell_run(dir.path(), &[OsStr::new(status)], &[], "_ui_color \"$2\"");
        assert_eq!(code, 0, "shell color {status:?}");
        let shell = String::from_utf8(out).expect("color text");
        assert_eq!(
            color(status.as_bytes(), &palette).as_bytes(),
            shell.as_bytes()
        );
        assert_eq!(
            color(status.as_bytes(), &palette).as_bytes(),
            expected.as_bytes()
        );
    }
}

#[test]
fn ascii_mode_matrix_agrees() {
    let dir = TempDir::new("ui-ascii").expect("fixture dir");
    // (DOT_UI_ASCII env, locale env): exercised locales are C (always
    // present), POSIX (exact match, no glob), a discovered UTF-8
    // locale when the platform has one, and fully unset.
    let mut cases: Vec<(Option<&str>, ShellEnv<'_>)> = vec![
        (Some("1"), vec![("LC_ALL", Some("C"))]),
        (Some("0"), vec![("LC_ALL", Some("C"))]),
        (None, vec![("LC_ALL", Some("C"))]),
        (None, vec![("LC_ALL", Some("POSIX"))]),
        (None, vec![("LC_ALL", Some("C.UTF-8"))]),
        (
            None,
            vec![("LC_ALL", None), ("LC_CTYPE", None), ("LANG", None)],
        ),
        (Some("01"), vec![("LC_ALL", Some("C"))]),
        (Some("abc"), vec![("LC_ALL", Some("C"))]),
    ];
    if let Some(utf8) = utf8_locale_name() {
        eprintln!("ascii cases use UTF-8 locale {utf8}");
        cases.push((
            None,
            vec![("LC_ALL", Some(Box::leak(utf8.into_boxed_str()) as &str))],
        ));
    } else {
        eprintln!("no UTF-8 locale available; skipping ascii-false case");
    }
    for (flag, locale) in &cases {
        let flag: Option<&str> = *flag;
        let mut env: Vec<(&str, Option<&str>)> = locale.clone();
        match flag {
            Some(value) => env.push(("DOT_UI_ASCII", Some(value))),
            None => env.push(("DOT_UI_ASCII", None)),
        }
        let multibyte = shell_multibyte(dir.path(), locale);
        let flag_value = flag.map(OsStr::new);
        let mut argv: Vec<&OsStr> = Vec::new();
        if let Some(value) = flag_value {
            argv.push(value);
        }
        let snippet = if flag.is_some() {
            "DOT_UI_ASCII=\"$2\"; if _ui_ascii_mode; then echo yes; else echo no; fi"
        } else {
            "if _ui_ascii_mode; then echo yes; else echo no; fi"
        };
        let (code, out) = shell_run(dir.path(), &argv, &env, snippet);
        assert_eq!(code, 0, "shell ascii {flag:?} {locale:?}");
        let shell = String::from_utf8(out).expect("ascii text");
        let locale_string = locale
            .iter()
            .find(|(key, _)| *key == "LC_ALL")
            .and_then(|(_, value)| *value)
            .or_else(|| {
                locale
                    .iter()
                    .find(|(key, _)| *key == "LC_CTYPE")
                    .and_then(|(_, value)| *value)
            })
            .or_else(|| {
                locale
                    .iter()
                    .find(|(key, _)| *key == "LANG")
                    .and_then(|(_, value)| *value)
            })
            .unwrap_or("");
        let rust = ascii_mode(flag, locale_string, multibyte);
        assert_eq!(
            if rust { "yes\n" } else { "no\n" },
            shell,
            "ascii parity for flag {flag:?} locale {locale:?} multibyte {multibyte}"
        );
    }
}

#[test]
fn utf8_locale_rule_pins_production_mapping() {
    // Production resolves `multibyte` from the locale name alone; the
    // shell twin lives inside bash, so this pins the rule the
    // differential cases above assume. A locale name that merely
    // claims UTF-8 without being installed is a documented corner:
    // bash falls back to byte counting while this rule trusts the
    // name (see `utf8_locale`).
    for (name, expected) in [
        ("C", false),
        ("POSIX", false),
        ("", false),
        ("latin1", false),
        ("en_US.ISO-8859-1", false),
        ("C.UTF-8", true),
        ("C.utf8", true),
        ("en_US.UTF-8", true),
        ("en_US.utf8", true),
    ] {
        assert_eq!(utf8_locale(name), expected, "rule for {name:?}");
    }
}

#[test]
fn fit_pads_truncates_and_handles_zero_width() {
    let dir = TempDir::new("ui-fit").expect("fixture dir");
    // C locale: bash counts bytes, and every case here is ASCII so
    // both counting modes agree.
    for (text, width, truncate) in [
        ("hi", 8, true),
        ("hi", 8, false),
        ("12345678", 8, true),
        ("12345678", 8, false),
        ("123456789", 8, true),
        ("123456789", 8, false),
        ("", 4, true),
        ("", 4, false),
        ("hi", 0, true),
        ("hi", 0, false),
        ("", 0, true),
    ] {
        let (code, out) = shell_run(
            dir.path(),
            &[OsStr::new(text)],
            &[],
            &format!("_ui_fit \"$2\" {width} {}", if truncate { 1 } else { 0 }),
        );
        assert_eq!(code, 0, "shell fit {text:?} {width} {truncate}");
        assert_eq!(
            fit(text.as_bytes(), width, truncate, false),
            out,
            "fit parity for {text:?} width {width} truncate {truncate}"
        );
        let (kind, expected_fn): (&str, FitOracle) = if truncate {
            ("cell", |t, w, m| cell(t, w, m))
        } else {
            ("pad", |t, w, m| pad(t, w, m))
        };
        let (code, out) = shell_run(
            dir.path(),
            &[OsStr::new(text)],
            &[],
            &format!("_ui_{kind} \"$2\" {width}"),
        );
        assert_eq!(code, 0, "shell {kind} {text:?}");
        assert_eq!(
            expected_fn(text.as_bytes(), width, false),
            out,
            "{kind} parity for {text:?} width {width}"
        );
    }
}

#[test]
fn fit_matches_shell_counting_per_locale() {
    let dir = TempDir::new("ui-fit-locale").expect("fixture dir");
    // Double bar (3 bytes, 1 char) plus invalid UTF-8 bytes: under a
    // byte-counting locale the shell measures 3 + N bytes, under a
    // working UTF-8 locale 1 + N chars (each bad byte counts once).
    let text = b"\xE2\x94\x81\xFF\xFE";
    let mut locales: Vec<Vec<(&str, Option<&str>)>> = vec![vec![("LC_ALL", Some("C"))]];
    if let Some(utf8) = utf8_locale_name() {
        eprintln!("multibyte fit cases use {utf8}");
        locales.push(vec![(
            "LC_ALL",
            Some(Box::leak(utf8.into_boxed_str()) as &str),
        )]);
    } else {
        eprintln!("no UTF-8 locale available; byte-counting cases only");
    }
    for locale in &locales {
        let multibyte = shell_multibyte(dir.path(), locale);
        for (width, truncate) in [(8, true), (8, false), (2, true), (4, true)] {
            let os_text = OsStr::from_bytes(text);
            let (code, out) = shell_run(
                dir.path(),
                &[os_text],
                locale,
                &format!("_ui_fit \"$2\" {width} {}", if truncate { 1 } else { 0 }),
            );
            assert_eq!(code, 0, "shell fit {locale:?}");
            assert_eq!(
                fit(text, width, truncate, multibyte),
                out,
                "fit parity for {locale:?} width {width} truncate {truncate}"
            );
        }
    }
}

#[test]
fn join_comma_skips_empties() {
    let shell_cases: &[&[&str]] = &[
        &[],
        &[""],
        &["a"],
        &["a", "b"],
        &["", "a", "", "b", ""],
        &["1 repo changed", "2 repos current"],
    ];
    for items in shell_cases {
        let dir = TempDir::new("ui-join").expect("fixture dir");
        let argv: Vec<&OsStr> = items.iter().map(OsStr::new).collect();
        let indices: Vec<String> = (2..2 + argv.len()).map(|i| format!("\"${i}\"")).collect();
        let (code, out) = shell_run(
            dir.path(),
            &argv,
            &[],
            &format!("_join_comma {}", indices.join(" ")),
        );
        assert_eq!(code, 0, "shell join {items:?}");
        let rust_items: Vec<&[u8]> = items.iter().map(|item| item.as_bytes()).collect();
        assert_eq!(join_comma(&rust_items), out, "join parity for {items:?}");
    }
}

#[test]
fn count_phrase_singular_and_plural() {
    let dir = TempDir::new("ui-phrase").expect("fixture dir");
    for (count, singular, plural) in [
        (1_i64, "repo", None),
        (0_i64, "repo", None),
        (2_i64, "repo", None),
        (1_i64, "entry", Some("entries")),
        (5_i64, "entry", Some("entries")),
    ] {
        let plural_arg = plural.unwrap_or("");
        let (code, out) = shell_run(
            dir.path(),
            &[OsStr::new(singular), OsStr::new(plural_arg)],
            &[],
            &format!("_ui_count_phrase {count} \"$2\" \"$3\""),
        );
        assert_eq!(code, 0, "shell phrase {count}");
        let plural_bytes = plural.map(str::as_bytes);
        assert_eq!(
            count_phrase(count, singular.as_bytes(), plural_bytes),
            out,
            "phrase parity for {count}"
        );
    }
}

/// Quiet resolution shared by the line tests: mirrors how
/// production will resolve `DOT_QUIET` through `log::is_quiet`.
fn quiet_of(value: Option<&str>) -> bool {
    log::is_quiet(value)
}

#[test]
fn elapsed_formats_second_differences() {
    // `_ui_elapsed` reads `$SECONDS` itself, which no harness can
    // pin; this pins the `'%ss'` format the stage methods reuse.
    for (now, started, expected) in [(100, 100, "0s"), (112, 100, "12s"), (50, 100, "-50s")] {
        assert_eq!(elapsed(now, started), expected.as_bytes());
    }
}

#[test]
fn live_enabled_matrix_agrees() {
    let dir = TempDir::new("ui-live").expect("fixture dir");
    // Piped harness stdout is never a tty on any platform, so the
    // force flag alone drives the live branch here.
    for (quiet, force) in [
        (None, None),
        (None, Some("0")),
        (None, Some("1")),
        (None, Some("01")),
        (None, Some("abc")),
        (Some("0"), Some("1")),
        (Some("1"), Some("1")),
        (Some("1"), None),
    ] {
        let mut env: Vec<(&str, Option<&str>)> = vec![("DOT_QUIET", quiet)];
        env.push(("DOT_UI_FORCE_LIVE", force));
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &env,
            "if _ui_live_enabled; then echo yes; else echo no; fi",
        );
        assert_eq!(code, 0, "shell live {quiet:?} {force:?}");
        let shell = String::from_utf8(out).expect("live text");
        let rust = live_enabled(quiet_of(quiet), false, force);
        assert_eq!(
            if rust { "yes\n" } else { "no\n" },
            shell,
            "live parity for quiet {quiet:?} force {force:?}"
        );
    }
}

#[test]
fn clear_live_emits_escape_once() {
    let dir = TempDir::new("ui-clear").expect("fixture dir");
    for active in [true, false] {
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &[],
            &format!(
                "DOT_UI_LIVE_ACTIVE={}; _ui_clear_live; printf 'active=%s' \"$DOT_UI_LIVE_ACTIVE\"",
                if active { 1 } else { 0 }
            ),
        );
        assert_eq!(code, 0, "shell clear {active}");
        let (esc, now_active) = clear_live(active);
        let mut expected = esc;
        expected.extend_from_slice(format!("active={}", if now_active { 1 } else { 0 }).as_bytes());
        assert_eq!(expected, out, "clear parity for {active}");
    }
}

#[test]
fn line_renders_cells() {
    let dir = TempDir::new("ui-line").expect("fixture dir");
    let palette = marker_palette();
    for (label, status, detail, quiet) in [
        ("Repos", "changed", "working", None),
        ("Tools", "ok", "no dependency provider", None),
        ("Cleanup", "bogus", "x", None),
        ("Repos", "ok", "working", Some("1")),
    ] {
        let env: Vec<(&str, Option<&str>)> = vec![("DOT_QUIET", quiet)];
        let (code, out) = shell_run(
            dir.path(),
            &[OsStr::new(label), OsStr::new(status), OsStr::new(detail)],
            &env,
            "_ui_line 2 5 \"$2\" \"$3\" \"$4\" \"3s\"",
        );
        assert_eq!(code, 0, "shell line {label:?}");
        assert_eq!(
            line(
                &palette,
                quiet_of(quiet),
                2,
                "5",
                label.as_bytes(),
                status.as_bytes(),
                detail.as_bytes(),
                b"3s",
                false,
            ),
            out,
            "line parity for {label:?} {status:?} quiet {quiet:?}"
        );
    }
}

#[test]
fn duration_ms_boundaries_agree() {
    let dir = TempDir::new("ui-duration").expect("fixture dir");
    for ms in [
        -5_i64, 0, 1, 999, 1000, 1099, 1500, 1999, 9999, 10000, 10499, 10500, 61500,
    ] {
        let (code, out) = shell_run(dir.path(), &[], &[], &format!("_ui_duration_ms {ms}"));
        assert_eq!(code, 0, "shell duration {ms}");
        assert_eq!(duration_ms(ms), out, "duration parity for {ms}");
    }
}

#[test]
fn live_line_spinner_cycles_ascii_frames() {
    let dir = TempDir::new("ui-spinner").expect("fixture dir");
    let palette = marker_palette();
    let env: Vec<(&str, Option<&str>)> = vec![("DOT_UI_ASCII", Some("1"))];
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &env,
        "DOT_UI_SPINNER_INDEX=0; for _ in 1 2 3 4; do _ui_live_line 1 4 Repos running working 0s; done; printf 'idx=%s' \"$DOT_UI_SPINNER_INDEX\"",
    );
    assert_eq!(code, 0, "shell spinner");
    let mut spinner = 0u64;
    let mut expected = Vec::new();
    for _ in 0..4 {
        expected.extend_from_slice(&live_line(
            &palette,
            false,
            1,
            "4",
            b"Repos",
            b"running",
            b"working",
            b"0s",
            &mut spinner,
            true,
            false,
        ));
    }
    expected.extend_from_slice(format!("idx={spinner}").as_bytes());
    assert_eq!(expected, out, "ascii spinner parity");
}

#[test]
fn live_line_static_status_keeps_spinner() {
    let dir = TempDir::new("ui-spinner-static").expect("fixture dir");
    let palette = marker_palette();
    // Non-running statuses print literally and never advance the
    // spinner; quiet suppresses output without advancing either.
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("DOT_UI_ASCII", Some("1"))],
        "DOT_UI_SPINNER_INDEX=5; _ui_live_line 1 4 Repos ok done 1s; printf 'idx=%s' \"$DOT_UI_SPINNER_INDEX\"",
    );
    assert_eq!(code, 0, "shell static");
    let mut spinner = 5u64;
    let mut expected = live_line(
        &palette,
        false,
        1,
        "4",
        b"Repos",
        b"ok",
        b"done",
        b"1s",
        &mut spinner,
        true,
        false,
    );
    expected.extend_from_slice(format!("idx={spinner}").as_bytes());
    assert_eq!(expected, out, "static spinner parity");
    assert_eq!(spinner, 5, "static status keeps spinner");

    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("DOT_UI_ASCII", Some("1")), ("DOT_QUIET", Some("1"))],
        "DOT_UI_SPINNER_INDEX=5; _ui_live_line 1 4 Repos running working 0s; printf 'idx=%s' \"$DOT_UI_SPINNER_INDEX\"",
    );
    assert_eq!(code, 0, "shell quiet spinner");
    let mut spinner = 5u64;
    let mut expected = live_line(
        &palette,
        true,
        1,
        "4",
        b"Repos",
        b"running",
        b"working",
        b"0s",
        &mut spinner,
        true,
        false,
    );
    expected.extend_from_slice(format!("idx={spinner}").as_bytes());
    assert_eq!(expected, out, "quiet spinner parity");
    assert_eq!(spinner, 5, "quiet keeps spinner");
}

#[test]
fn live_line_unicode_frames_agree() {
    let Some(utf8) = utf8_locale_name() else {
        eprintln!("no UTF-8 locale available; skipping unicode spinner");
        return;
    };
    // Guard against a locale name the libc cannot actually use: the
    // shell must count the probe glyph as one character here, or the
    // frames would be ASCII on its side only.
    let dir = TempDir::new("ui-spinner-uni").expect("fixture dir");
    let leaked: &'static str = Box::leak(utf8.into_boxed_str());
    let locale = vec![("LC_ALL", Some(leaked))];
    if !shell_multibyte(dir.path(), &locale) {
        eprintln!("UTF-8 locale {leaked} is unusable; skipping unicode spinner");
        return;
    }
    let palette = marker_palette();
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("LC_ALL", Some(leaked)), ("DOT_UI_ASCII", None)],
        "DOT_UI_SPINNER_INDEX=0; _ui_live_line 1 4 Repos running working 0s; _ui_live_line 1 4 Repos running working 0s; printf 'idx=%s' \"$DOT_UI_SPINNER_INDEX\"",
    );
    assert_eq!(code, 0, "shell unicode spinner");
    let mut spinner = 0u64;
    let mut expected = Vec::new();
    for _ in 0..2 {
        expected.extend_from_slice(&live_line(
            &palette,
            false,
            1,
            "4",
            b"Repos",
            b"running",
            b"working",
            b"0s",
            &mut spinner,
            false,
            true,
        ));
    }
    expected.extend_from_slice(format!("idx={spinner}").as_bytes());
    assert_eq!(expected, out, "unicode spinner parity");
}

#[test]
fn status_section_detail_item_render() {
    let dir = TempDir::new("ui-rows").expect("fixture dir");
    let palette = marker_palette();
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[],
        "_ui_status ok done; _ui_status bogus huh; _ui_section Title; _ui_detail some-detail; _ui_item failed name extra; _ui_item ok plain",
    );
    assert_eq!(code, 0, "shell rows");
    // Row helpers clear a live line first, so each call threads the
    // live flag through exactly like the shell globals.
    let mut expected = Vec::new();
    let (bytes, live) = status(&palette, false, false, b"ok", b"done", false);
    expected.extend_from_slice(&bytes);
    let (bytes, live) = status(&palette, false, live, b"bogus", b"huh", false);
    expected.extend_from_slice(&bytes);
    let (bytes, live) = section(&palette, false, live, b"Title", false);
    expected.extend_from_slice(&bytes);
    let (bytes, live) = detail(&palette, false, live, b"some-detail", false);
    expected.extend_from_slice(&bytes);
    let (bytes, live) = item(
        &palette,
        false,
        live,
        b"failed",
        b"name",
        Some(b"extra".as_slice()),
        false,
    );
    expected.extend_from_slice(&bytes);
    let (bytes, _) = item(&palette, false, live, b"ok", b"plain", None, false);
    expected.extend_from_slice(&bytes);
    assert_eq!(expected, out, "row parity");

    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("DOT_QUIET", Some("1"))],
        "_ui_status ok done; _ui_section Title; _ui_detail d; _ui_item ok n",
    );
    assert_eq!(code, 0, "shell quiet rows");
    let mut expected = Vec::new();
    let (bytes, live) = status(&palette, true, false, b"ok", b"done", false);
    expected.extend_from_slice(&bytes);
    let (bytes, live) = section(&palette, true, live, b"Title", false);
    expected.extend_from_slice(&bytes);
    let (bytes, live) = detail(&palette, true, live, b"d", false);
    expected.extend_from_slice(&bytes);
    let (bytes, _) = item(&palette, true, live, b"ok", b"n", None, false);
    expected.extend_from_slice(&bytes);
    assert_eq!(expected, out, "quiet row parity");
}

#[test]
fn progress_bar_matrix_agrees() {
    let dir = TempDir::new("ui-bar").expect("fixture dir");
    // (done, total, width env, ascii): total zero or negative prints
    // nothing; over-full clamps; width zero draws an empty frame.
    let cases = [
        (0, 5, None, true),
        (3, 5, None, true),
        (5, 5, None, true),
        (7, 5, None, true),
        (1, 3, Some("12"), true),
        (0, 5, Some("0"), true),
        (5, 0, None, true),
        (5, -3, None, true),
        (25, 100, Some("10"), true),
    ];
    for (done, total, width, ascii) in cases {
        let mut env: Vec<(&str, Option<&str>)> = vec![("DOT_UI_ASCII", Some("1"))];
        match width {
            Some(value) => env.push(("DOT_UI_PROGRESS_WIDTH", Some(value))),
            None => env.push(("DOT_UI_PROGRESS_WIDTH", None)),
        }
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &env,
            &format!("_ui_progress_bar {done} {total}"),
        );
        assert_eq!(code, 0, "shell bar {done}/{total}");
        assert_eq!(
            progress_bar(done, total, width.unwrap_or("8"), ascii),
            out,
            "bar parity for {done}/{total}"
        );
    }
}

#[test]
fn progress_bar_unicode_agrees() {
    let Some(utf8) = utf8_locale_name() else {
        eprintln!("no UTF-8 locale available; skipping unicode bar");
        return;
    };
    let dir = TempDir::new("ui-bar-uni").expect("fixture dir");
    let leaked: &'static str = Box::leak(utf8.into_boxed_str());
    let locale = vec![("LC_ALL", Some(leaked))];
    if !shell_multibyte(dir.path(), &locale) {
        eprintln!("UTF-8 locale {leaked} is unusable; skipping unicode bar");
        return;
    }
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("LC_ALL", Some(leaked)), ("DOT_UI_ASCII", None)],
        "_ui_progress_bar 3 5",
    );
    assert_eq!(code, 0, "shell unicode bar");
    assert_eq!(progress_bar(3, 5, "8", false), out, "unicode bar parity");
}

#[test]
fn progress_detail_with_label_agrees() {
    let dir = TempDir::new("ui-detail").expect("fixture dir");
    let env: Vec<(&str, Option<&str>)> = vec![("DOT_UI_ASCII", Some("1"))];
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &env,
        "_ui_progress_detail_with_label overlays 2 5; _ui_progress_detail_with_label overlays 2 5 done; _ui_progress_detail_with_label overlays 2 5 done 10",
    );
    assert_eq!(code, 0, "shell labeled detail");
    let mut expected = Vec::new();
    expected.extend_from_slice(&progress_detail_with_label(
        b"overlays",
        2,
        5,
        None,
        "18",
        "8",
        true,
        false,
    ));
    expected.extend_from_slice(&progress_detail_with_label(
        b"overlays",
        2,
        5,
        Some(b"done".as_slice()),
        "18",
        "8",
        true,
        false,
    ));
    expected.extend_from_slice(&progress_detail_with_label(
        b"overlays",
        2,
        5,
        Some(b"done".as_slice()),
        "10",
        "8",
        true,
        false,
    ));
    assert_eq!(expected, out, "labeled detail parity");
}

#[test]
fn progress_detail_total_gate_agrees() {
    let dir = TempDir::new("ui-progress").expect("fixture dir");
    let env: Vec<(&str, Option<&str>)> = vec![("DOT_UI_ASCII", Some("1"))];
    for total in [0, -2, 4] {
        // `_dot_progress_detail` is the four-line `repos/pull.sh`
        // wrapper; the harness sources that file too.
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &env,
            &format!("_dot_progress_detail overlays 1 {total}"),
        );
        assert_eq!(code, 0, "shell progress {total}");
        assert_eq!(
            progress_detail(b"overlays", 1, total, "8", true, false),
            out,
            "progress parity for total {total}"
        );
    }
}

/// Replace rendered elapsed stamps (`in 12s.`, `    3s` before
/// end-of-line or a carriage return) with placeholders. Stage
/// outputs embed `$SECONDS` deltas no harness can pin, so both
/// engines normalize before comparing structure.
fn normalize_elapsed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if let Some(tail) = rest.strip_prefix(b"in ") {
            let (sign, tail) = split_sign(tail);
            if let Some(len) = take_digits(tail) {
                // Stamps render as `in 10s` followed by punctuation, a
                // palette marker, or end of output; a trailing letter
                // (as in `in 5star`) is prose, not a stamp.
                let after_s = tail.get(len + 1);
                if tail.get(len..len + 1) == Some(b"s".as_slice())
                    && !after_s.is_some_and(|b| b.is_ascii_alphanumeric())
                {
                    out.extend_from_slice(b"in Ns");
                    i += 3 + sign + len + 1;
                    continue;
                }
            }
        }
        if rest[0] == b' ' {
            let (sign, tail) = split_sign(&rest[1..]);
            if let Some(len) = take_digits(tail) {
                let end = 1 + sign + len;
                if rest.get(end..end + 1) == Some(b"s".as_slice())
                    && matches!(rest.get(end + 1), None | Some(b'\r') | Some(b'\n'))
                {
                    out.extend_from_slice(b" Ns");
                    i += end + 1;
                    continue;
                }
            }
        }
        out.push(rest[0]);
        i += 1;
    }
    out
}

/// Optional leading minus plus the digit run that follows; the
/// shell renders negative `SECONDS` deltas like `-90s`.
fn split_sign(bytes: &[u8]) -> (usize, &[u8]) {
    if bytes.first() == Some(&b'-') {
        (1, &bytes[1..])
    } else {
        (0, bytes)
    }
}

#[test]
fn normalize_elapsed_exact_done_bytes() {
    let input = b"<B><W>Done in 10s<R>\n";
    assert_eq!(
        normalize_elapsed(input),
        b"<B><W>Done in Ns<R>\n".as_slice()
    );
    let negative = b"<B><W>Done in -90s<R>\n";
    assert_eq!(
        normalize_elapsed(negative),
        b"<B><W>Done in Ns<R>\n".as_slice()
    );
}

#[test]
fn normalize_elapsed_replaces_stamps() {
    assert_eq!(
        normalize_elapsed(b"Done in 10s."),
        b"Done in Ns.".as_slice()
    );
    assert_eq!(
        normalize_elapsed(b"Done in -90s."),
        b"Done in Ns.".as_slice()
    );
    assert_eq!(normalize_elapsed(b"    0s\n"), b"    Ns\n".as_slice());
    assert_eq!(
        normalize_elapsed(b"no stamps [1/2] here"),
        b"no stamps [1/2] here".as_slice()
    );
}

/// Leading ASCII-digit run length, if any.
fn take_digits(bytes: &[u8]) -> Option<usize> {
    let len = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if len == 0 { None } else { Some(len) }
}

#[test]
fn stage_start_sequence_agrees() {
    let dir = TempDir::new("ui-start").expect("fixture dir");
    let palette = marker_palette();
    // (live, verbose): start prints a plain line, or a verbose line
    // plus the live redraw.
    for (live, verbose) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut env: Vec<(&str, Option<&str>)> = vec![("DOT_QUIET", None)];
        if live {
            env.push(("DOT_UI_FORCE_LIVE", Some("1")));
        }
        if verbose {
            env.push(("DOT_VERBOSE", Some("1")));
        }
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &env,
            "_ui_begin 5; _ui_stage_start Repos; printf 'idx=%s/%s' \"$DOT_UI_INDEX\" \"$DOT_UI_TOTAL\"",
        );
        assert_eq!(code, 0, "shell start {live} {verbose}");
        // The C-locale shell takes the ASCII spinner branch.
        let mut stage =
            dot::progress_ui::Stage::begin(palette.clone(), "5", false, live, false, true);
        let mut expected = stage.start(b"Repos", None, 1000, verbose.then_some("1"));
        expected.extend_from_slice(b"idx=1/5");
        assert_eq!(
            normalize_elapsed(&expected),
            normalize_elapsed(&out),
            "start parity for live {live} verbose {verbose}"
        );
    }
}

#[test]
fn stage_update_tick_finish_note_agree() {
    let dir = TempDir::new("ui-stage").expect("fixture dir");
    let palette = marker_palette();
    let live_env: Vec<(&str, Option<&str>)> =
        vec![("DOT_QUIET", None), ("DOT_UI_FORCE_LIVE", Some("1"))];
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &live_env,
        "_ui_begin 4; _ui_stage_start Repos working; _ui_stage_update still; _ui_stage_tick; _ui_stage_finish ok done; _ui_stage_note changed item",
    );
    assert_eq!(code, 0, "shell stage sequence");
    // Frames render here, and the C-locale shell takes ASCII.
    let mut stage = dot::progress_ui::Stage::begin(palette.clone(), "4", false, true, false, true);
    let mut expected = stage.start(b"Repos", Some(b"working".as_slice()), 1000, None);
    expected.extend_from_slice(&stage.update(b"still", 1001, None));
    expected.extend_from_slice(&stage.tick(1002));
    expected.extend_from_slice(&stage.finish(b"ok", b"done", 1003));
    expected.extend_from_slice(&stage.note(b"changed", b"item"));
    assert_eq!(
        normalize_elapsed(&expected),
        normalize_elapsed(&out),
        "live stage sequence parity"
    );

    // Non-live, non-verbose: updates and ticks stay silent.
    let plain_env: Vec<(&str, Option<&str>)> = vec![("DOT_QUIET", None)];
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &plain_env,
        "_ui_begin 4; _ui_stage_start Repos working; _ui_stage_update still; _ui_stage_tick; _ui_stage_finish ok done",
    );
    assert_eq!(code, 0, "shell plain sequence");
    let mut stage =
        dot::progress_ui::Stage::begin(palette.clone(), "4", false, false, false, false);
    let mut expected = stage.start(b"Repos", Some(b"working".as_slice()), 1000, None);
    expected.extend_from_slice(&stage.update(b"still", 1001, None));
    expected.extend_from_slice(&stage.tick(1002));
    expected.extend_from_slice(&stage.finish(b"ok", b"done", 1003));
    assert_eq!(
        normalize_elapsed(&expected),
        normalize_elapsed(&out),
        "plain stage sequence parity"
    );

    // Non-live verbose: updates render as newline progress.
    let verbose_env: Vec<(&str, Option<&str>)> =
        vec![("DOT_QUIET", None), ("DOT_VERBOSE", Some("1"))];
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &verbose_env,
        "_ui_begin 4; _ui_stage_start Repos working; _ui_stage_update still",
    );
    assert_eq!(code, 0, "shell verbose update");
    let mut stage =
        dot::progress_ui::Stage::begin(palette.clone(), "4", false, false, false, false);
    let mut expected = stage.start(b"Repos", Some(b"working".as_slice()), 1000, Some("1"));
    expected.extend_from_slice(&stage.update(b"still", 1001, Some("1")));
    assert_eq!(
        normalize_elapsed(&expected),
        normalize_elapsed(&out),
        "verbose update parity"
    );
}

#[test]
fn stage_header_text_total_gate_agrees() {
    let dir = TempDir::new("ui-header").expect("fixture dir");
    let palette = marker_palette();
    for total in ["5", "0", "00", "abc", "+2", " 3"] {
        // The spaced total needs quoting through the snippet.
        let assignment = if total == " 3" {
            "DOT_UI_TOTAL=' 3'".to_string()
        } else {
            format!("DOT_UI_TOTAL={total}")
        };
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &[],
            &format!("DOT_UI_INDEX=2; {assignment}; _ui_stage Label; _ui_stage Label2"),
        );
        assert_eq!(code, 0, "shell header {total:?}");
        let mut stage =
            dot::progress_ui::Stage::begin(palette.clone(), total, false, false, false, false);
        // The snippet presets the counter to 2; advance past it.
        stage.header_text(b"Warm");
        stage.header_text(b"Warm");
        let mut expected = stage.header_text(b"Label");
        expected.extend_from_slice(&stage.header_text(b"Label2"));
        assert_eq!(expected, out, "header parity for total {total:?}");
    }
    // Quiet headers stay silent.
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("DOT_QUIET", Some("1"))],
        "DOT_UI_INDEX=2; DOT_UI_TOTAL=5; _ui_stage Label",
    );
    assert_eq!(code, 0, "shell quiet header");
    let mut stage = dot::progress_ui::Stage::begin(palette.clone(), "5", true, false, false, false);
    assert_eq!(stage.header_text(b"Label"), out, "quiet header parity");
}

#[test]
fn maybe_stage_progress_gates_agree() {
    let dir = TempDir::new("ui-maybe").expect("fixture dir");
    let palette = marker_palette();
    // (total, verbose): progress renders only with a positive total
    // and an unset-or-zero verbose flag; anything else stays silent.
    for (total, verbose) in [
        ("4", None),
        ("4", Some("0")),
        ("4", Some("1")),
        ("4", Some("2")),
        ("4", Some("abc")),
        ("0", Some("1")),
        ("00", Some("1")),
        ("abc", Some("1")),
    ] {
        let mut env: Vec<(&str, Option<&str>)> =
            vec![("DOT_QUIET", None), ("DOT_UI_FORCE_LIVE", Some("1"))];
        env.push(("DOT_VERBOSE", verbose));
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &env,
            &format!(
                "_ui_begin 4; DOT_UI_TOTAL={total}; _ui_stage_start Repos working; _dot_maybe_stage_progress overlays 1 2"
            ),
        );
        assert_eq!(code, 0, "shell maybe {total:?} {verbose:?}");
        // The shell renders ASCII bars under the C locale, so the
        // stage resolves ascii the same way.
        let mut stage =
            dot::progress_ui::Stage::begin(palette.clone(), total, false, true, false, true);
        // The snippet starts under the same DOT_VERBOSE, which adds a
        // newline row for verbose live callers.
        let mut expected = stage.start(b"Repos", Some(b"working".as_slice()), 1000, verbose);
        expected.extend_from_slice(&stage.maybe_progress(b"overlays", 1, 2, 1001, verbose, "8"));
        assert_eq!(
            normalize_elapsed(&expected),
            normalize_elapsed(&out),
            "maybe parity for total {total:?} verbose {verbose:?}"
        );
    }
}

#[test]
fn done_message_and_hint_agree() {
    let dir = TempDir::new("ui-done").expect("fixture dir");
    let palette = marker_palette();
    // Reload checkpoint short-circuits the hint deterministically.
    for (status, checkpoint) in [
        ("0", None),
        ("1", None),
        ("0", Some("1")),
        ("00", Some("1")),
    ] {
        let mut env: Vec<(&str, Option<&str>)> = vec![("DOT_QUIET", None)];
        env.push(("DOT_UPDATE_RELOADS_SHELL", checkpoint));
        // The harness scrubs SHELL and bash backfills it from the login
        // shell (host-dependent); empty it explicitly, which bash
        // preserves, so the hint takes the deterministic HOME branch.
        env.push(("SHELL", Some("")));
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &env,
            &format!("DOT_UI_STARTED=90; _ui_done {status}"),
        );
        assert_eq!(code, 0, "shell done {status}");
        assert_eq!(
            normalize_elapsed(&dot::progress_ui::done(
                &palette,
                false,
                Some(status),
                90,
                100,
                &dot::progress_ui::reload_hint(checkpoint, None, false, false),
            )),
            normalize_elapsed(&out),
            "done parity for status {status}"
        );
    }
}

#[test]
fn reload_hint_matrix_agrees() {
    let dir = TempDir::new("ui-hint").expect("fixture dir");
    // The harness parent process is never bash/zsh. An unset SHELL
    // would be backfilled from the login shell (host-dependent), so
    // the HOME file branch runs with SHELL explicitly emptied, which
    // bash preserves as empty.
    for (bashrc, zshrc, shell_env) in [
        (false, false, Some("")),
        (true, false, Some("")),
        (false, true, Some("")),
        (true, true, Some("")),
        (false, false, Some("/bin/bash")),
        (false, false, Some("/bin/zsh")),
        (false, false, Some("/bin/fish")),
    ] {
        if bashrc {
            std::fs::write(dir.path().join(".bashrc"), "# fixture\n").expect("bashrc");
        } else {
            let _ = std::fs::remove_file(dir.path().join(".bashrc"));
        }
        if zshrc {
            std::fs::write(dir.path().join(".zshrc"), "# fixture\n").expect("zshrc");
        } else {
            let _ = std::fs::remove_file(dir.path().join(".zshrc"));
        }
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &[("SHELL", shell_env)],
            "_ui_shell_reload_hint",
        );
        assert_eq!(code, 0, "shell hint");
        // Resolve the shell name exactly like production will: the
        // harness parent is the test binary, never bash/zsh.
        let shell_name = shell_env.and_then(dot::progress_ui::normal_shell_name);
        assert_eq!(
            dot::progress_ui::reload_hint(None, shell_name, bashrc, zshrc),
            out,
            "hint parity for bashrc {bashrc} zshrc {zshrc} shell {shell_env:?}"
        );
    }
    // Checkpoint short-circuits everything.
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[("DOT_UPDATE_RELOADS_SHELL", Some("1"))],
        "_ui_shell_reload_hint",
    );
    assert_eq!(code, 0, "shell checkpoint hint");
    assert_eq!(
        dot::progress_ui::reload_hint(Some("1"), None, false, false),
        out,
        "checkpoint hint parity"
    );
}

#[test]
fn reload_hint_absent_shell_reads_files() {
    // Absent `SHELL` cannot go through the shell (bash backfills it
    // from the login shell, which varies by host); this pins the
    // documented Rust contract that absent means the files branch.
    assert_eq!(
        dot::progress_ui::reload_hint(None, None, true, false),
        b"Reload your shell: source ~/.bashrc".as_slice()
    );
    assert_eq!(
        dot::progress_ui::reload_hint(None, None, false, false),
        b"Reload your shell: source ~/.zshrc".as_slice()
    );
}

#[test]
fn normal_shell_name_matrix_agrees() {
    let dir = TempDir::new("ui-shell-name").expect("fixture dir");
    for path in [
        "/bin/bash",
        "-zsh",
        "bash",
        "/usr/bin/fish",
        "",
        "--bash",
        "-",
    ] {
        let (code, out) = shell_run(
            dir.path(),
            &[OsStr::new(path)],
            &[],
            "if name=$(_ui_normal_shell_name \"$2\"); then echo \"yes:$name\"; else echo no; fi",
        );
        assert_eq!(code, 0, "shell name {path:?}");
        let shell = String::from_utf8(out).expect("name text");
        let rust = dot::progress_ui::normal_shell_name(path);
        let expected = match rust {
            Some(name) => format!("yes:{name}\n"),
            None => "no\n".to_string(),
        };
        assert_eq!(expected, shell, "name parity for {path:?}");
    }
}

/// Write an executable `date` shim replaying `$DOT_TEST_FAKE_DATE`
/// with exit `$DOT_TEST_FAKE_DATE_RC` (default 0), returning a bindir
/// holding only that shim. `_ui_now_ms` calls nothing but `date`, so
/// a shim-only PATH pins the clock read byte-exactly. The bindir is
/// exec-capable (see [`TempDir::new_exec`]).
fn fake_date_bindir(label: &str) -> TempDir {
    let dir = TempDir::new_exec(label).expect("bindir");
    let shim = dir.path().join("date");
    std::fs::write(
        &shim,
        "#!/bin/sh\nprintf '%s' \"$DOT_TEST_FAKE_DATE\"\nexit \"${DOT_TEST_FAKE_DATE_RC:-0}\"\n",
    )
    .expect("shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dir
}

#[test]
fn now_ms_pinned_clock_agrees() {
    // (fake `date` output, fake `date` exit, SECONDS seed). The shim
    // pins the `date +%s%3N` read; the shell falls back to
    // SECONDS*1000 whenever the read is not all digits (or `date`
    // fails). SECONDS is seeded in-snippet (a fresh child starts at 0
    // but sourcing takes milliseconds, so an inherited value would be
    // timing-dependent); the suffix reports the live value, which
    // absorbs any tick landing before the function's own read. Only
    // a tick in the microsecond gap after that read miscompares —
    // loudly, by exactly 1000 — instead of passing wrong.
    let cases: &[(&str, &str, i64)] = &[
        ("1757068800123", "0", 7),
        ("0", "0", 99),
        ("0007", "0", 3),
        ("", "0", 7),
        ("", "1", 7),
        ("abc", "0", 7),
        ("12a34", "0", 7),
        (" 12", "0", 7),
        ("12 ", "0", 7),
        ("+12", "0", 7),
        ("-12", "0", 7),
        ("1\n2", "0", 7),
        ("12.5", "0", 7),
    ];
    for (stamp, rc, seconds) in cases.iter().copied() {
        let dir = TempDir::new("ui-now").expect("fixture dir");
        let bindir = fake_date_bindir("ui-now-date");
        let bindir_text = bindir.path().to_string_lossy().into_owned();
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &[
                ("PATH", Some(bindir_text.as_str())),
                ("DOT_TEST_FAKE_DATE", Some(stamp)),
                ("DOT_TEST_FAKE_DATE_RC", Some(rc)),
            ],
            &format!("SECONDS={seconds}; _ui_now_ms; printf '|%s' \"$SECONDS\""),
        );
        assert_eq!(code, 0, "shell now for {stamp:?} rc {rc}");
        // The printed stamp is digits only, so the last `|` opens
        // the suffix.
        let split = out
            .iter()
            .rposition(|byte| *byte == b'|')
            .expect("seconds suffix");
        let (printed, suffix) = out.split_at(split);
        let reported: i64 = String::from_utf8(suffix[1..].to_vec())
            .expect("seconds digits")
            .parse()
            .expect("seconds number");
        let effective = if rc == "0" { stamp } else { "" };
        assert_eq!(
            now_ms(effective, reported),
            printed,
            "now parity for {stamp:?} rc {rc} seconds {reported}"
        );
    }
    // Nonzero fallback arithmetic without the shell clock (the
    // `'%ss'` precedent in `elapsed_formats_second_differences`):
    // the differential matrix above proves the branch, this pins
    // the `* 1000` the branch computes.
    assert_eq!(now_ms("", 7), b"7000".as_slice());
    assert_eq!(now_ms("12a", 100), b"100000".as_slice());
}

#[test]
fn now_ms_live_shaped_value_agrees() {
    // A real `date +%s%3N` stamp replayed through the shim: both twins
    // echo the same live-shaped digits with no race (one captured read
    // feeds both sides).
    let probe = Command::new("date")
        .arg("+%s%3N")
        .output()
        .expect("date probe");
    let stamp = String::from_utf8_lossy(&probe.stdout).trim_end().to_owned();
    if stamp.is_empty() || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
        // No GNU `date` here (BSD prints `%3N` through): the pinned
        // matrix above already covers the fallback arm.
        return;
    }
    let dir = TempDir::new("ui-now-live").expect("fixture dir");
    let bindir = fake_date_bindir("ui-now-live-date");
    let bindir_text = bindir.path().to_string_lossy().into_owned();
    let (code, out) = shell_run(
        dir.path(),
        &[],
        &[
            ("PATH", Some(bindir_text.as_str())),
            ("DOT_TEST_FAKE_DATE", Some(stamp.as_str())),
        ],
        "_ui_now_ms",
    );
    assert_eq!(code, 0, "shell live now");
    assert_eq!(now_ms(&stamp, 0), out, "live now parity");
}

/// Bindir holding only a `sed` link, so `command -v jq` fails and the
/// shell takes its `sed` fallback. `sed` is located off the live PATH
/// (no hardcoded directories) and linked, never copied.
fn sed_only_bindir(label: &str) -> TempDir {
    let live_path = std::env::var_os("PATH").unwrap_or_default();
    let mut origin: Option<PathBuf> = None;
    for dir in std::env::split_paths(&live_path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("sed");
        if candidate.is_file() {
            origin = Some(candidate);
            break;
        }
    }
    let origin = origin.expect("sed on PATH for the fallback oracle");
    let dir = TempDir::new_exec(label).expect("bindir");
    std::os::unix::fs::symlink(&origin, dir.path().join("sed")).expect("link sed");
    dir
}

/// Assert the shell takes the expected JSON branch (`jq` present or
/// the `sed` fallback) so a silent branch mismatch cannot pass as
/// parity.
fn assert_json_branch(fixture: &Path, extra_env: &[(&str, Option<&str>)], present: bool) {
    let (code, out) = shell_run(
        fixture,
        &[],
        extra_env,
        "if command -v jq >/dev/null 2>&1; then echo present; else echo fallback; fi",
    );
    assert_eq!(code, 0, "branch probe runs");
    assert_eq!(
        out,
        if present {
            b"present\n".as_slice()
        } else {
            b"fallback\n".as_slice()
        },
        "json branch premise"
    );
}

#[test]
fn json_get_twins_agree() {
    let dir = TempDir::new("ui-json-get").expect("fixture dir");
    assert_json_branch(dir.path(), &[], true);
    let fallback = sed_only_bindir("ui-json-get-sed");
    let fallback_path = fallback.path().to_string_lossy().into_owned();
    let fallback_env = [("PATH", Some(fallback_path.as_str()))];
    assert_json_branch(dir.path(), &fallback_env, false);
    let cases: &[(&str, &str)] = &[
        ("{\"event\":\"done\",\"index\":3}", "event"),
        ("{\"event\":\"done\",\"index\":3}", "index"),
        ("{\"event\":\"done\",\"index\":3}", "missing"),
        ("{\"msg\":\"a\\\"b\"}", "msg"),
        ("{\"e\":\"\"}", "e"),
        ("{\"n\":42}", "n"),
        ("{\"n\":1.5}", "n"),
        ("{\"b\":true}", "b"),
        ("{\"v\":null}", "v"),
        ("{\"o\":{\"a\":1}}", "o"),
        ("{\"a\":[1,2]}", "a"),
        ("not json", "event"),
        ("", "event"),
        ("{\"k\":\"first\",\"k\":\"second\"}", "k"),
        ("{\"k\" : \"spaced\"}", "k"),
        ("{\"n\":-5}", "n"),
        ("[1,2]", "0"),
    ];
    for (line, key) in cases.iter().copied() {
        let argv = [OsStr::new(key), OsStr::from_bytes(line.as_bytes())];
        let (code, out) = shell_run(dir.path(), &argv, &[], "_json_get \"$2\" \"$3\"");
        // `jq` fails loudly (nonzero) on unparsable input or a type
        // error while printing nothing; every caller uses `$(...)`
        // and only reads stdout, so a bare nonzero-with-output would
        // be the real divergence. (`jq` exit codes vary by release,
        // so only the emptiness companion is pinned.)
        assert!(
            code == 0 || out.is_empty(),
            "shell json_get rc {code} with output for {line:?} key {key:?}"
        );
        assert_eq!(
            json_get(key, line.as_bytes(), true),
            out,
            "json_get jq parity for {line:?} key {key:?}"
        );
        let (code, out) = shell_run(dir.path(), &argv, &fallback_env, "_json_get \"$2\" \"$3\"");
        assert_eq!(code, 0, "shell json_get fallback for {line:?} key {key:?}");
        assert_eq!(
            json_get(key, line.as_bytes(), false),
            out,
            "json_get fallback parity for {line:?} key {key:?}"
        );
    }
}

#[test]
fn json_num_twins_agree() {
    let dir = TempDir::new("ui-json-num").expect("fixture dir");
    assert_json_branch(dir.path(), &[], true);
    let fallback = sed_only_bindir("ui-json-num-sed");
    let fallback_path = fallback.path().to_string_lossy().into_owned();
    let fallback_env = [("PATH", Some(fallback_path.as_str()))];
    assert_json_branch(dir.path(), &fallback_env, false);
    let cases: &[(&str, &str)] = &[
        ("{\"n\":42}", "n"),
        ("{\"n\":0}", "n"),
        ("{\"n\":1.5}", "n"),
        ("{\"n\":-5}", "n"),
        ("{\"n\":\"42\"}", "n"),
        ("{\"n\":true}", "n"),
        ("{\"n\":null}", "n"),
        ("{\"n\":007}", "n"),
        ("{\"n\":12345678901234567890123}", "n"),
        ("{\"event\":\"done\"}", "n"),
        ("{\"a\":1,\"n\":2}", "n"),
        ("{\"n\":1,\"n\":2}", "n"),
        ("not json", "n"),
        ("", "n"),
        ("{\"n\": 5}", "n"),
        ("{\"n\":5 }", "n"),
    ];
    for (line, key) in cases.iter().copied() {
        let argv = [OsStr::new(key), OsStr::from_bytes(line.as_bytes())];
        let (code, out) = shell_run(dir.path(), &argv, &[], "_json_num \"$2\" \"$3\"");
        // Same nonzero-on-empty contract as `_json_get` (see above):
        // callers only ever read stdout through `$(...)`.
        assert!(
            code == 0 || out.is_empty(),
            "shell json_num rc {code} with output for {line:?} key {key:?}"
        );
        assert_eq!(
            json_num(key, line.as_bytes(), true),
            out,
            "json_num jq parity for {line:?} key {key:?}"
        );
        let (code, out) = shell_run(dir.path(), &argv, &fallback_env, "_json_num \"$2\" \"$3\"");
        assert_eq!(code, 0, "shell json_num fallback for {line:?} key {key:?}");
        assert_eq!(
            json_num(key, line.as_bytes(), false),
            out,
            "json_num fallback parity for {line:?} key {key:?}"
        );
    }
}

#[test]
fn parent_shell_name_core_prefers_proc() {
    // `_ui_parent_shell_name` reads the live parent process, which no
    // harness can pin; this pins the priority and normalization the
    // production reader feeds on: `/proc` first, `ps` fallback.
    assert_eq!(
        dot::progress_ui::parent_shell_name(Some("bash"), Some("zsh")),
        Some("bash".to_string())
    );
    assert_eq!(
        dot::progress_ui::parent_shell_name(Some("fish"), Some("zsh")),
        Some("zsh".to_string())
    );
    assert_eq!(
        dot::progress_ui::parent_shell_name(Some(""), Some("zsh")),
        Some("zsh".to_string())
    );
    assert_eq!(
        dot::progress_ui::parent_shell_name(None, Some("  -bash  \nignored\n")),
        Some("bash".to_string())
    );
    assert_eq!(dot::progress_ui::parent_shell_name(None, None), None);
    assert_eq!(
        dot::progress_ui::parent_shell_name(Some("fish"), Some("dash")),
        None
    );
}
