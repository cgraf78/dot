//! Explicit native contracts for progress rendering and parsing helpers.

use dot::progress_ui::*;

fn palette() -> Palette {
    Palette {
        reset: "<R>".into(),
        bold: "<B>".into(),
        dim: "<D>".into(),
        green: "<G>".into(),
        yellow: "<Y>".into(),
        red: "<E>".into(),
        blue: "<U>".into(),
        cyan: "<C>".into(),
        white: "<W>".into(),
    }
}

#[test]
fn colors_and_locale_modes_are_fixed() {
    let p = palette();
    for (name, expected) in [
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
        assert_eq!(color(name.as_bytes(), &p), expected);
    }
    for (name, expected) in [
        ("C", false),
        ("POSIX", false),
        ("", false),
        ("latin1", false),
        ("C.UTF-8", true),
        ("C.utf8", true),
        ("en_US.UTF-8", true),
        ("en_US.utf8", true),
    ] {
        assert_eq!(utf8_locale(name), expected);
    }
    for (flag, locale, multibyte, expected) in [
        (Some("1"), "C", false, true),
        (Some("0"), "en_US.UTF-8", true, false),
        (None, "C", true, true),
        (None, "POSIX", true, true),
        (None, "en_US.UTF-8", true, false),
        (None, "en_US.UTF-8", false, true),
        (Some("01"), "C", false, true),
        (Some("abc"), "C", false, true),
    ] {
        assert_eq!(ascii_mode(flag, locale, multibyte), expected);
    }
}

#[test]
fn fitting_preserves_byte_and_character_contracts() {
    for (text, width, truncate, expected) in [
        (b"hi".as_slice(), 8, true, b"hi      ".as_slice()),
        (b"12345678", 8, true, b"12345678"),
        (b"123456789", 8, true, b"12345678"),
        (b"123456789", 8, false, b"123456789"),
        (b"", 4, true, b"    "),
        (b"hi", 0, true, b""),
        (b"hi", 0, false, b"hi"),
    ] {
        assert_eq!(fit(text, width, truncate, false), expected);
    }
    assert_eq!(cell(b"hi", 4, false), b"hi  ");
    assert_eq!(pad(b"12345", 4, false), b"12345");
    assert_eq!(fit("━abc".as_bytes(), 3, true, true), "━ab".as_bytes());
    assert_eq!(
        fit(b"\xe2\x94\x81\xff\xfe", 4, true, false),
        b"\xe2\x94\x81\xff"
    );
}

#[test]
fn phrases_time_and_live_gates_cover_boundaries() {
    assert_eq!(join_comma(&[]), b"");
    assert_eq!(join_comma(&[b"", b"one", b"", b"two"]), b"one, two");
    assert_eq!(count_phrase(0, b"repo", None), b"0 repos");
    assert_eq!(count_phrase(1, b"repo", None), b"1 repo");
    assert_eq!(
        count_phrase(2, b"repo", Some(b"repositories")),
        b"2 repositories"
    );
    for (ms, expected) in [
        (0, "0ms"),
        (999, "999ms"),
        (1000, "1.0s"),
        (1499, "1.4s"),
        (1500, "1.5s"),
        (10_000, "10s"),
        (-1, "-1ms"),
    ] {
        assert_eq!(duration_ms(ms), expected.as_bytes());
    }
    assert_eq!(elapsed(10, 7), b"3s");
    assert_eq!(elapsed(7, 10), b"-3s");
    for (quiet, tty, force, expected) in [
        (false, true, None, true),
        (false, false, None, false),
        (true, true, None, false),
        (false, false, Some("1"), true),
        (true, false, Some("1"), false),
        (false, true, Some("0"), true),
    ] {
        assert_eq!(live_enabled(quiet, tty, force), expected);
    }
    assert_eq!(clear_live(false), (vec![], false));
    assert_eq!(clear_live(true), (b"\r\x1b[K".to_vec(), false));
}

#[test]
fn lines_and_spinner_render_contracts() {
    let p = palette();
    let rendered = line(&p, false, 1, "3", b"pull", b"ok", b"done", b"2s", false);
    assert!(rendered.starts_with(b"<C>[1/3]<R> pull"));
    assert!(rendered.ends_with(b"    2s\n"));
    assert!(line(&p, true, 1, "3", b"pull", b"ok", b"done", b"2s", false).is_empty());
    let mut spinner = 0;
    for expected in [b"/".as_slice(), b"-", b"\\", b"|", b"/"] {
        let out = live_line(
            &p,
            false,
            1,
            "1",
            b"pull",
            b"running",
            b"work",
            b"0s",
            &mut spinner,
            true,
            false,
        );
        assert!(out.windows(expected.len()).any(|w| w == expected));
    }
    let before = spinner;
    let out = live_line(
        &p,
        false,
        1,
        "1",
        b"pull",
        b"waiting",
        b"work",
        b"0s",
        &mut spinner,
        true,
        false,
    );
    assert!(out.windows(7).any(|w| w == b"waiting"));
    assert_eq!(spinner, before);
    let mut unicode = 0;
    let out = live_line(
        &p,
        false,
        1,
        "1",
        b"pull",
        b"running",
        b"work",
        b"0s",
        &mut unicode,
        false,
        true,
    );
    assert!(out.windows("⠋".len()).any(|w| w == "⠋".as_bytes()));
}

#[test]
fn status_section_detail_and_item_are_exact() {
    let p = palette();
    assert_eq!(
        status(&p, false, true, b"ok", b"ready", false),
        (b"\r\x1b[K  <G>ok      <R> ready\n".to_vec(), false)
    );
    assert_eq!(
        section(&p, false, false, b"Repos", false),
        (b"  <B><W>Repos<R>\n".to_vec(), false)
    );
    assert_eq!(
        detail(&p, false, false, b"more", false),
        (b"    <D>more<R>\n".to_vec(), false)
    );
    assert_eq!(
        item(&p, false, false, b"ok", b"dot", None, false),
        (b"  <G>ok      <R> dot\n".to_vec(), false)
    );
    let (out, active) = item(&p, false, true, b"changed", b"dot", Some(b"updated"), false);
    assert!(out.starts_with(b"\r\x1b[K  <U>changed <R> dot"));
    assert!(out.ends_with(b"<D>updated<R>\n"));
    assert!(!active);
    assert_eq!(
        status(&p, true, true, b"ok", b"ready", false),
        (vec![], true)
    );
}

#[test]
fn progress_bars_and_details_cover_numeric_edges() {
    for (done, total, width, ascii, expected) in [
        (0, 4, "4", true, "[----] 0/4"),
        (1, 4, "4", true, "[#---] 1/4"),
        (4, 4, "4", true, "[####] 4/4"),
        (5, 4, "4", true, "[####] 5/4"),
        // Negative progress renders an empty bar at exactly `width` cells
        // (moved from the over-wide `[-----]` by the P2-1 clamp: the bar
        // must never exceed its width, whatever `done` arrives).
        (-1, 4, "4", true, "[----] -1/4"),
        (1, 4, "0", true, "[] 1/4"),
        (1, 4, "+4", true, "[#---] 1/4"),
        (1, 4, "bad", true, ""),
        (1, 0, "4", true, ""),
        (1, -1, "4", true, ""),
        (2, 4, "4", false, "[━━··] 2/4"),
    ] {
        assert_eq!(progress_bar(done, total, width, ascii), expected.as_bytes());
    }
    assert_eq!(
        progress_detail_with_label(b"pull", 2, 4, Some(b"two"), "6", "4", true, false),
        b"pull   [##--] 2/4 two"
    );
    assert_eq!(
        progress_detail_with_label(b"pull", 2, 4, None, "bad", "4", true, false),
        b" [##--] 2/4"
    );
    assert_eq!(
        progress_detail(b"pull", 2, 4, "4", true, false),
        b"pull               [##--] 2/4"
    );
    assert!(progress_detail(b"pull", 0, 0, "4", true, false).is_empty());
}

#[test]
fn progress_bar_bounds_hostile_done_values() {
    // Fresh-review-B P2-1 RED pin: `done` arrives from untrusted provider
    // JSONL with the full i64 range. The bar must stay within `width`
    // cells: no overflow panic, no hang, no terabyte allocation.
    assert_eq!(
        progress_bar(i64::MIN, 1, "10", true),
        b"[----------] -9223372036854775808/1"
    );
    assert_eq!(progress_bar(-1, 1, "10", true), b"[----------] -1/1");
    assert_eq!(progress_bar(-1, 4, "4", true), b"[----] -1/4");
    for done in [i64::MIN, i64::MIN + 1, -9_000_000_000_000, -1, 0] {
        let bar = progress_bar(done, 1, "18", true);
        assert!(
            bar.len() < 100,
            "negative done {done} escaped the width bound ({} bytes)",
            bar.len()
        );
    }
    // Overfull progress still saturates at a full bar (existing contract).
    assert_eq!(
        progress_bar(i64::MAX, 1, "4", true),
        b"[####] 9223372036854775807/1"
    );
}

#[test]
fn stages_preserve_gating_state_and_sequence() {
    let mut stage = Stage::begin(palette(), "2", false, true, false, true);
    let start = stage.start(b"pull", None, 10, None);
    assert!(start.starts_with(b"\r\x1b[K"));
    assert!(start.windows(7).any(|w| w == b"working"));
    assert!(!stage.update(b"half", 12, None).is_empty());
    assert!(!stage.tick(13).is_empty());
    let finish = stage.finish(b"ok", b"done", 14);
    assert!(finish.starts_with(b"\r\x1b[K"));
    assert!(stage.note(b"warning", b"note").ends_with(b"\n"));
    let mut nonlive = Stage::begin(palette(), "2", false, false, false, true);
    assert!(!nonlive.start(b"pull", Some(b"work"), 10, None).is_empty());
    assert!(nonlive.update(b"half", 11, None).is_empty());
    assert!(!nonlive.update(b"half", 11, Some("1")).is_empty());
    assert!(nonlive.tick(12).is_empty());
    assert!(!nonlive.header_text(b"next").is_empty());
    assert!(
        nonlive
            .maybe_progress(b"pull", 1, 2, 12, None, "4")
            .is_empty()
    );
    assert!(
        nonlive
            .maybe_progress(b"pull", 1, 2, 12, Some("1"), "4")
            .is_empty()
    );
    let mut quiet = Stage::begin(palette(), "2", true, true, false, true);
    assert!(quiet.start(b"pull", None, 0, None).is_empty());
}

#[test]
fn completion_reload_shell_clock_and_json_contracts() {
    let p = palette();
    assert_eq!(
        done(&p, false, Some("0"), 10, 13, b""),
        b"<B><W>Done in 3s<R>\n"
    );
    assert_eq!(
        done(&p, false, Some("2"), 10, 13, b"reload"),
        b"<B><W>Done with errors in 3s.<R> reload\n"
    );
    assert!(done(&p, true, None, 0, 0, b"").is_empty());
    assert_eq!(reload_hint(Some("1"), Some("bash"), false, false), b"");
    assert_eq!(
        reload_hint(None, Some("bash"), false, false),
        b"Reload your shell: source ~/.bashrc"
    );
    assert_eq!(
        reload_hint(None, None, true, false),
        b"Reload your shell: source ~/.bashrc"
    );
    assert_eq!(
        reload_hint(None, None, false, false),
        b"Reload your shell: source ~/.zshrc"
    );
    for (path, expected) in [
        ("/bin/bash", Some("bash")),
        ("-zsh", Some("zsh")),
        ("/bin/fish", None),
        ("", None),
    ] {
        assert_eq!(normal_shell_name(path), expected);
    }
    assert_eq!(
        parent_shell_name(Some("bash\n"), Some("zsh")),
        Some("zsh".into())
    );
    assert_eq!(
        parent_shell_name(Some("fish"), Some("  zsh\nextra")),
        Some("zsh".into())
    );
    assert_eq!(warn_line(&p, b"careful"), b"<Y>careful<R>\n");
    assert_eq!(now_ms("1700000000123", 1), b"1700000000123");
    assert_eq!(now_ms("bad", 42), b"42000");
    let json = br#"{"name":"dot","count":12,"zero":0,"negative":-2}"#;
    let have_jq = dot::merge_hooks::jq_available();
    for jq in [false, true].into_iter().filter(|jq| !*jq || have_jq) {
        let expected = |value: &[u8]| [value, b"\n"].concat();
        assert_eq!(json_get("name", json, jq), expected(b"dot"));
        assert_eq!(json_num("count", json, jq), expected(b"12"));
        assert_eq!(json_num("zero", json, jq), expected(b"0"));
        assert_eq!(
            json_num("negative", json, jq),
            if jq { expected(b"-2") } else { vec![] }
        );
        assert_eq!(json_get("missing", json, jq), b"");
    }
    assert_eq!(json_num("count", b"not-json", false), b"");
}

#[test]
fn elapsed_normalization_changes_only_progress_stamps() {
    let input = b"row 3/4                         12s\nDone in 9s. reload\nDone with errors in 4s. reload\nin 5star\n";
    assert_eq!(
        normalize_elapsed(input),
        b"row 3/4                         12s\nDone in Ns. reload\nDone with errors in Ns. reload\nin 5star\n"
    );
}

#[test]
fn header_progress_and_reload_gate_matrices_are_complete() {
    for (total, has_counter) in [
        ("5", true),
        ("0", false),
        ("00", false),
        ("abc", false),
        ("+2", true),
        (" 3", true),
    ] {
        let mut stage = Stage::begin(palette(), total, false, false, false, true);
        let out = stage.header_text(b"Label");
        assert_eq!(
            out.windows(3).any(|w| w == b"[1/"),
            has_counter,
            "{total:?}"
        );
    }
    for (total, verbose, emits) in [
        ("4", None, true),
        ("4", Some("0"), true),
        ("4", Some("1"), false),
        ("4", Some("2"), false),
        ("4", Some("abc"), true),
        ("0", None, false),
        ("00", None, false),
        ("abc", None, false),
    ] {
        let mut stage = Stage::begin(palette(), total, false, true, false, true);
        stage.start(b"Repos", Some(b"working"), 10, verbose);
        assert_eq!(
            !stage
                .maybe_progress(b"overlays", 1, 2, 11, verbose, "8")
                .is_empty(),
            emits,
            "{total:?} {verbose:?}"
        );
    }
    for (bashrc, zshrc, shell, expected) in [
        (false, false, None, "Reload your shell: source ~/.zshrc"),
        (true, false, None, "Reload your shell: source ~/.bashrc"),
        (false, true, None, "Reload your shell: source ~/.zshrc"),
        (true, true, None, "Reload your shell: source ~/.zshrc"),
        (
            false,
            false,
            Some("bash"),
            "Reload your shell: source ~/.bashrc",
        ),
        (
            false,
            false,
            Some("zsh"),
            "Reload your shell: source ~/.zshrc",
        ),
    ] {
        assert_eq!(reload_hint(None, shell, bashrc, zshrc), expected.as_bytes());
    }
}

#[test]
fn clock_and_json_edge_matrices_are_explicit() {
    for (stamp, seconds, expected) in [
        ("1757068800123", 7, "1757068800123"),
        ("0", 99, "0"),
        ("0007", 3, "0007"),
        ("", 7, "7000"),
        ("abc", 7, "7000"),
        ("12a34", 7, "7000"),
        (" 12", 7, "7000"),
        ("12 ", 7, "7000"),
        ("+12", 7, "7000"),
        ("-12", 7, "7000"),
        ("1\n2", 7, "7000"),
        ("12.5", 7, "7000"),
    ] {
        assert_eq!(now_ms(stamp, seconds), expected.as_bytes(), "{stamp:?}");
    }
    let have_jq = dot::merge_hooks::jq_available();
    for (line, key, fallback, jq_expected) in [
        (
            r#"{"event":"done","index":3}"#,
            "event",
            b"done\n".as_slice(),
            b"done\n".as_slice(),
        ),
        (r#"{"event":"done","index":3}"#, "index", b"", b"3\n"),
        (r#"{"e":""}"#, "e", b"\n", b"\n"),
        (
            r#"{"k":"first","k":"second"}"#,
            "k",
            b"second\n",
            b"second\n",
        ),
        (r#"{"k" : "spaced"}"#, "k", b"", b"spaced\n"),
        ("not json", "event", b"", b""),
    ] {
        assert_eq!(json_get(key, line.as_bytes(), false), fallback);
        if have_jq {
            assert_eq!(json_get(key, line.as_bytes(), true), jq_expected);
        }
    }
    for (line, fallback, jq_expected) in [
        (r#"{"n":42}"#, b"42\n".as_slice(), b"42\n".as_slice()),
        (r#"{"n":0}"#, b"0\n", b"0\n"),
        (r#"{"n":1.5}"#, b"1\n", b"1.5\n"),
        (r#"{"n":-5}"#, b"", b"-5\n"),
        (r#"{"n":"42"}"#, b"", b""),
        (r#"{"n":true}"#, b"", b""),
        (r#"{"n":null}"#, b"", b""),
        ("not json", b"", b""),
    ] {
        assert_eq!(json_num("n", line.as_bytes(), false), fallback);
        if have_jq {
            assert_eq!(json_num("n", line.as_bytes(), true), jq_expected);
        }
    }
}

#[test]
fn heartbeat_production_cadence_is_one_second() {
    // Whole-second stamps mean a render per elapsed second is exactly
    // "at least every second" for the live seconds counter.
    assert_eq!(HEARTBEAT_INTERVAL_SECS, 1);
}

#[test]
fn heartbeat_renders_at_most_once_per_interval() {
    let mut beat = Heartbeat::new(100, 1);
    assert!(!beat.poll(100));
    assert!(!beat.poll(100));
    assert!(beat.poll(101));
    assert!(!beat.poll(101));
    // A multi-second jump renders once, never a catch-up burst.
    assert!(beat.poll(104));
    assert!(!beat.poll(104));
}

#[test]
fn heartbeat_noted_event_restarts_the_interval() {
    let mut beat = Heartbeat::new(100, 1);
    beat.noted(100);
    assert!(!beat.poll(100));
    assert!(beat.poll(101));
    // A later event render pushes the next heartbeat out, so the
    // heartbeat never duplicates an event render within its second.
    beat.noted(101);
    assert!(!beat.poll(101));
    assert!(beat.poll(102));
}

#[test]
fn heartbeat_backward_clock_relatches_without_rendering() {
    let mut beat = Heartbeat::new(100, 1);
    assert!(!beat.poll(99));
    // Relatched at 99: no render storm while the clock sits behind.
    assert!(!beat.poll(99));
    assert!(!beat.poll(98));
    assert!(beat.poll(100));
}

#[test]
fn heartbeat_zero_interval_renders_every_poll() {
    // Tests force every wait-loop poll to render without sleeping out
    // a production interval.
    let mut beat = Heartbeat::new(50, 0);
    assert!(beat.poll(50));
    assert!(beat.poll(50));
    // Negative intervals clamp to the same always-due behavior.
    let mut beat = Heartbeat::new(50, -3);
    assert!(beat.poll(50));
}

#[test]
fn indented_lines_indent_child_output_however_it_is_chunked() {
    use std::io::Write as _;
    let mut sink = Vec::new();
    {
        let mut indented = IndentedLines::new(&mut sink);
        indented.write_all(b"fatal: one\nfatal: ").unwrap();
        indented.write_all(b"two\n\nPlease ").unwrap();
        indented.write_all(b"retry\n").unwrap();
    }
    assert_eq!(
        String::from_utf8_lossy(&sink),
        "    fatal: one\n    fatal: two\n\n    Please retry\n"
    );
    assert_eq!(CHILD_OUTPUT_INDENT, b"    ");
}

#[test]
fn stage_redraws_nothing_before_its_first_stage_opens() {
    // Progress reported before any stage starts (an overlay pull on a client
    // without a base checkout) used to render `[0/N]` rows with no label and
    // an elapsed time counted from the epoch.
    for live in [true, false] {
        let mut stage = Stage::begin(palette(), "5", false, live, false, true);
        assert!(
            stage
                .update(b"overlays", 1_700_000_000, Some("1"))
                .is_empty()
        );
        assert!(stage.tick(1_700_000_000).is_empty());
        assert!(
            stage
                .maybe_progress(b"overlay-0", 1, 2, 1_700_000_000, None, "8")
                .is_empty()
        );
        // The static row printed before a required overlay pull, which on
        // such a client also runs before any stage opens.
        assert!(stage.freeze(b"pulling 1 overlay", 1_700_000_000).is_empty());
        assert!(!stage.start(b"Repos", None, 10, None).is_empty());
        assert!(!stage.update(b"overlay-0", 11, Some("1")).is_empty());
        assert_eq!(!stage.freeze(b"pulling 1 overlay", 11).is_empty(), live);
    }
}

#[test]
fn terminal_rows_fit_the_width_and_cut_at_words() {
    // Rows were a fixed 75 columns: narrower terminals wrapped every redraw.
    let visible = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .trim_start_matches("\r\x1b[K")
            .trim_end_matches('\n')
            .chars()
            .count()
    };
    let detail = b"repository synchronization failed; dependency install skipped";
    for columns in [40, 60, 74] {
        let mut stage = Stage::begin(Palette::empty(), "5", false, true, false, true)
            .with_width(RowWidth::Columns(columns));
        let start = stage.start(b"Tools", Some(detail), 0, None);
        assert!(visible(&start) < columns, "{columns}: {start:?}");
        let close = stage.finish(b"warning", detail, 1);
        assert!(visible(&close) < columns, "{columns}: {close:?}");
    }
    let mut stage = Stage::begin(Palette::empty(), "5", false, true, false, true)
        .with_width(RowWidth::Columns(50));
    let close = String::from_utf8(stage.finish(b"warning", detail, 1)).unwrap();
    // No clause fits 16 columns: blank rather than a leading fragment.
    assert_eq!(
        close, "[0/5]            warning                       1s\n",
        "kept a fragment"
    );
    // Wide terminals keep the 42-column cell, cut at a word boundary.
    let mut wide = Stage::begin(Palette::empty(), "5", false, true, false, true)
        .with_width(RowWidth::Columns(120));
    let close = String::from_utf8(wide.finish(b"warning", detail, 1)).unwrap();
    assert!(
        close.contains("warning  repository synchronization failed              1s"),
        "{close:?}"
    );
    // Without a terminal width the historical fixed cell stays byte-for-byte.
    let mut fixed = Stage::begin(Palette::empty(), "5", false, false, false, true);
    let close = String::from_utf8(fixed.finish(b"warning", detail, 1)).unwrap();
    assert_eq!(
        close,
        "[0/5]            warning  repository synchronization failed; depende     1s\n"
    );
}

#[test]
fn terminal_rows_drop_whole_clauses_and_keep_failures() {
    // Narrow rows cut summaries mid-word (`2 repo`, `profil`) and could drop
    // the failure (`2 repos current, 1 repo` without "failed").
    let close = |columns: usize, detail: &[u8]| {
        let mut stage = Stage::begin(Palette::empty(), "5", false, true, false, true)
            .with_width(RowWidth::Columns(columns));
        String::from_utf8(stage.finish(b"failed", detail, 1)).unwrap()
    };
    let summary = b"2 repos current, 1 repo failed";
    assert!(
        close(60, summary).contains("failed   1 repo failed "),
        "{}",
        close(60, summary)
    );
    assert!(close(80, summary).contains("2 repos current, 1 repo failed"));
    // Not even one word fits: blank, not a fragment.
    let narrow = close(40, b"profile resolution or repository sync failed");
    assert!(narrow.contains("failed   "), "{narrow:?}");
    assert!(!narrow.contains("profil"), "{narrow:?}");
}

#[test]
fn terminal_rows_show_no_word_fragments() {
    // At 40 columns single leading words survived (`Tools ok no`,
    // `Configs ok no`) and read as contradictions; a cell either holds a
    // whole clause or nothing.
    let close = |detail: &[u8]| {
        let mut stage = Stage::begin(Palette::empty(), "5", false, true, false, true)
            .with_width(RowWidth::Columns(40));
        String::from_utf8(stage.finish(b"ok", detail, 1)).unwrap()
    };
    for detail in [&b"no dependency provider"[..], b"3 repos current"] {
        let row = close(detail);
        assert!(row.contains("ok          "), "{row:?}");
        let first =
            String::from_utf8_lossy(detail.split(|b| *b == b' ').next().unwrap()).into_owned();
        assert!(
            !row.contains(&format!("ok       {first}")),
            "fragment in {row:?}"
        );
    }
}

#[test]
fn static_lines_are_skipped_when_their_text_cannot_show() {
    // At 40 columns the static Repos lines printed as blank `running` rows,
    // two identical-looking lines; with no room for the text the row is only
    // cleared, which is all a child that may prompt needs.
    let mut narrow = Stage::begin(Palette::empty(), "5", false, true, false, true)
        .with_width(RowWidth::Columns(40));
    let _ = narrow.start(b"Repos", None, 0, None);
    assert_eq!(narrow.freeze(b"fetching dotfiles", 0), b"\r\x1b[K");
    let mut wide = Stage::begin(Palette::empty(), "5", false, true, false, true)
        .with_width(RowWidth::Columns(80));
    let _ = wide.start(b"Repos", None, 0, None);
    let frozen = String::from_utf8(wide.freeze(b"fetching dotfiles", 0)).unwrap();
    assert!(frozen.contains("running  fetching dotfiles"), "{frozen:?}");
    assert!(frozen.ends_with('\n'), "{frozen:?}");
}

#[test]
fn terminal_progress_rows_keep_their_label_and_bar_whole() {
    // A progress detail is a label and its bar: narrow rows keep whichever
    // whole pieces fit, never a cut label.
    let live = |columns: usize| {
        let mut stage = Stage::begin(Palette::empty(), "5", false, true, false, true)
            .with_width(RowWidth::Columns(columns));
        let _ = stage.start(b"Repos", None, 0, None);
        String::from_utf8(stage.update(b"overlay-0          [####----] 1/2", 0, None)).unwrap()
    };
    assert!(
        live(60).contains("overlay-0 [####----] 1/2"),
        "{}",
        live(60)
    );
    assert!(live(48).contains("overlay-0 "), "{}", live(48));
    assert!(!live(48).contains("[#"), "{}", live(48));
    assert!(!live(40).contains("overlay"), "{}", live(40));
}
