//! Native contracts for update helpers shared across engine layers.

use dot::progress_ui::{Palette, Stage};
use dot::update::{RepoStageFinish, UpdateFlagParse};

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
fn leading_flags_stop_at_the_first_unknown_or_positional_word() {
    for (args, expected) in [
        (
            vec![b"--cron".as_slice(), b"--force", b"-v", b"tail"],
            UpdateFlagParse {
                cron_mode: true,
                quiet: true,
                force: true,
                verbose: true,
                consumed: 3,
            },
        ),
        (
            vec![b"--quiet".as_slice(), b"--", b"--force"],
            UpdateFlagParse {
                cron_mode: false,
                quiet: true,
                force: false,
                verbose: false,
                consumed: 1,
            },
        ),
        (
            vec![b"--quiet=x".as_slice(), b"--force"],
            UpdateFlagParse {
                cron_mode: false,
                quiet: false,
                force: false,
                verbose: false,
                consumed: 0,
            },
        ),
        (
            vec![b"-f".as_slice(), b"--verbose", b""],
            UpdateFlagParse {
                cron_mode: false,
                quiet: false,
                force: true,
                verbose: true,
                consumed: 2,
            },
        ),
    ] {
        assert_eq!(dot::update::parse_update_flags(&args), expected);
    }
}

#[test]
fn overlay_phase_requires_a_zero_command_and_failure_count() {
    for (rc, failed, expected) in [
        (0, None, true),
        (0, Some(""), true),
        (0, Some("0"), true),
        (0, Some("00"), true),
        (0, Some("1"), false),
        (1, Some("0"), false),
        (0, Some("invalid"), true),
    ] {
        assert_eq!(dot::update::overlay_phase_ok(rc, failed), expected);
    }
}

#[test]
fn repository_stage_is_silent_until_deferred() {
    let mut stage = Stage::begin(palette(), "4", false, false, false, true);
    let output = dot::update::repo_stage_finish(
        &mut stage,
        &RepoStageFinish {
            deferred_active: false,
            forced_failure: Some("1"),
            agg_current: Some("9"),
            agg_changed: Some("9"),
            agg_failed: Some("9"),
            agg_skipped: Some("9"),
            changed_items: b"must-not-render",
            verbose: None,
        },
        100,
    );
    assert!(output.is_empty());
}

#[test]
fn repository_stage_renders_all_statuses_counts_and_notes() {
    struct Case<'a> {
        forced: Option<&'a str>,
        current: Option<&'a str>,
        changed: Option<&'a str>,
        failed: Option<&'a str>,
        skipped: Option<&'a str>,
        items: &'a [u8],
        verbose: Option<&'a str>,
        status: &'a str,
        fragments: &'a [&'a str],
        absent: &'a [&'a str],
    }
    let cases = [
        Case {
            forced: None,
            current: None,
            changed: None,
            failed: None,
            skipped: None,
            items: b"",
            verbose: None,
            status: "ok",
            fragments: &["0 repos current"],
            absent: &[],
        },
        Case {
            forced: None,
            current: Some("1"),
            changed: Some("2"),
            failed: None,
            skipped: Some("3"),
            items: b"base\noverlay\n",
            verbose: Some("0"),
            status: "changed",
            fragments: &["2 repos changed", "1 repo current", "base", "overlay"],
            absent: &[],
        },
        Case {
            forced: None,
            current: None,
            changed: None,
            failed: None,
            skipped: Some("3"),
            items: b"",
            verbose: None,
            status: "ok",
            fragments: &["3 repos skipped"],
            absent: &[],
        },
        Case {
            forced: None,
            current: Some("4"),
            changed: Some("1"),
            failed: Some("2"),
            skipped: None,
            items: b"hidden",
            verbose: Some("1"),
            status: "failed",
            fragments: &["1 repo changed", "4 repos current"],
            absent: &["hidden"],
        },
        Case {
            forced: None,
            current: None,
            changed: None,
            failed: Some("2"),
            skipped: None,
            items: b"",
            verbose: None,
            status: "failed",
            fragments: &["2 repos failed"],
            absent: &[],
        },
        Case {
            forced: Some("1"),
            current: None,
            changed: None,
            failed: None,
            skipped: None,
            items: b"",
            verbose: None,
            status: "failed",
            fragments: &["0 repos current"],
            absent: &[],
        },
    ];
    for case in cases {
        let mut stage = Stage::begin(palette(), "4", false, false, false, true);
        let output = dot::update::repo_stage_finish(
            &mut stage,
            &RepoStageFinish {
                deferred_active: true,
                forced_failure: case.forced,
                agg_current: case.current,
                agg_changed: case.changed,
                agg_failed: case.failed,
                agg_skipped: case.skipped,
                changed_items: case.items,
                verbose: case.verbose,
            },
            100,
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(case.status), "status in {text:?}");
        for fragment in case.fragments {
            assert!(text.contains(fragment), "missing {fragment:?} in {text:?}");
        }
        for fragment in case.absent {
            assert!(
                !text.contains(fragment),
                "unexpected {fragment:?} in {text:?}"
            );
        }
    }
}
