//! Native contracts for merge orchestration helpers, preserving the former
//! shell-oracle matrices as literal ordering, output, and failure expectations.

use dot::{merges, progress_ui::Palette};

type ProgressCase<'a> = (&'a str, i64, i64, &'a str, &'a str, &'a [u8]);
type LabelCase<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], i64, &'a [u8]);
type RenderCase<'a> = (&'a [u8], &'a [u8], i64, &'a [&'a [u8]], &'a [u8]);
use std::ffi::{OsStr, OsString};
use std::process::Command;

#[test]
fn trim_cases_agree() {
    for (input, expected) in [
        ("  padded  ", "padded"),
        ("\t\ttabs\n", "tabs"),
        ("", ""),
        ("   ", ""),
        ("no-pad", "no-pad"),
        ("inner  space", "inner  space"),
        ("line\nbreak", "line\nbreak"),
    ] {
        assert_eq!(merges::trim(input), expected, "trim {input:?}");
    }
}

#[test]
fn label_cases_agree() {
    for (script, expected) in [
        ("10-foo.sh", "foo"),
        ("02_ssh.serial.sh", "ssh"),
        ("plain.sh", "plain"),
        ("noext", "noext"),
        ("/hooks/20-bar.sh", "bar"),
        ("10-.sh", "10-"),
        ("9-x", "x"),
        ("1_2-3.sh", "2-3"),
        ("-x.sh", "-x"),
        ("1x.sh", "1x"),
        ("007-z.serial.sh", "z"),
        ("10-UPPER.sh", "UPPER"),
        ("10-.serial.sh", "10-"),
    ] {
        assert_eq!(
            merges::label_from_script(OsStr::new(script)),
            OsStr::new(expected),
            "label {script:?}"
        );
    }
}

#[test]
fn serial_cases_agree() {
    for (script, expected) in [
        ("10-a.serial.sh", true),
        ("10-a.sh", false),
        (".serial.sh", true),
        ("serial.sh", false),
        ("10-a.SERIAL.SH", false),
        ("x.serial.sh.bak", false),
    ] {
        assert_eq!(
            merges::is_serial(script),
            expected,
            "serial barrier {script:?}"
        );
    }
}

#[test]
fn jobs_cases_agree() {
    let cpu = merges::cpu_count();
    for (merge_jobs, update_jobs, expected) in [
        ("", "", cpu.as_str()),
        ("4", "", "4"),
        ("0", "", "1"),
        ("00", "", "1"),
        ("007", "", "007"),
        ("abc", "", cpu.as_str()),
        (" 4", "", cpu.as_str()),
        ("4 ", "", cpu.as_str()),
        ("", "8", "8"),
        ("3", "8", "3"),
        ("0", "0", "1"),
        ("", "bogus", cpu.as_str()),
        ("2", "bogus", "2"),
    ] {
        assert_eq!(
            merges::parallel_jobs(merge_jobs, update_jobs),
            expected,
            "workers {merge_jobs:?}/{update_jobs:?}"
        );
    }
}

#[test]
fn cpu_count_kernel_agrees() {
    let live = String::from_utf8(
        Command::new("getconf")
            .arg("_NPROCESSORS_ONLN")
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    assert_eq!(
        merges::cpu_count_select(live.trim(), "Linux", ""),
        merges::cpu_count()
    );
    for (getconf, uname, sysctl, expected) in [
        ("8", "Linux", "", "8"),
        ("", "Linux", "", "4"),
        ("", "Darwin", "10", "10"),
        ("", "Darwin", "", "4"),
        ("bogus", "Linux", "", "4"),
        ("0", "Linux", "", "1"),
        ("00", "Darwin", "8", "1"),
        ("", "Darwin", "0", "1"),
        ("16", "Darwin", "4", "16"),
    ] {
        assert_eq!(
            merges::cpu_count_select(getconf, uname, sysctl),
            expected,
            "cpu {getconf:?}/{uname}/{sysctl:?}"
        );
    }
}

#[test]
fn summaries_agree() {
    for (count, merged, failed, warning) in [
        (
            0,
            "0 configs merged",
            "0 config hooks failed",
            "-1 configs merged, 1 config hook failed",
        ),
        (
            1,
            "1 config merged",
            "1 config hook failed",
            "0 configs merged, 1 config hook failed",
        ),
        (
            2,
            "2 configs merged",
            "2 config hooks failed",
            "1 config merged, 1 config hook failed",
        ),
        (
            17,
            "17 configs merged",
            "17 config hooks failed",
            "16 configs merged, 1 config hook failed",
        ),
    ] {
        assert_eq!(merges::summary(count), merged);
        assert_eq!(merges::failure_summary(count), failed);
        assert_eq!(merges::warning_summary(count, 1), warning);
    }
    assert_eq!(
        merges::warning_summary(5, 0),
        "5 configs merged, 0 config hooks failed"
    );
}

#[test]
fn result_prefix_agrees() {
    for (index, expected) in [
        (0, "/results/000"),
        (1, "/results/001"),
        (42, "/results/042"),
        (999, "/results/999"),
        (1000, "/results/1000"),
        (12345, "/results/12345"),
    ] {
        assert_eq!(merges::result_prefix("/results", index), expected);
    }
}

#[test]
fn progress_detail_cases_agree() {
    let cases: &[ProgressCase<'_>] = &[
        ("ssh", 1, 4, "18", "8", b"ssh                [##------] 1/4"),
        (
            "overlays",
            2,
            5,
            "10",
            "12",
            b"overlays   [####--------] 2/5",
        ),
        ("", 0, 3, "18", "8", b"                   [--------] 0/3"),
        (
            "done-hook",
            5,
            5,
            "18",
            "8",
            b"done-hook          [########] 5/5",
        ),
        (
            "overfull",
            9,
            5,
            "18",
            "8",
            b"overfull           [########] 9/5",
        ),
        (
            "uni-hööks",
            1,
            2,
            "18",
            "8",
            b"uni-h\xc3\xb6\xc3\xb6ks        [####----] 1/2",
        ),
        ("zero-total", 1, 0, "18", "8", b"zero-total         "),
    ];
    for &(label, done, total, label_width, bar_width, expected) in cases {
        assert_eq!(
            merges::progress_detail(
                label.as_bytes(),
                done,
                total,
                label_width,
                bar_width,
                true,
                false
            ),
            expected,
            "progress {label:?}"
        );
    }
}

#[test]
fn result_label_cases_agree() {
    let cases: &[LabelCase<'_>] = &[
        (
            "10-foo.sh",
            "Friendly Name\nsecond line\nthird\n",
            "Friendly Name",
            &["second line", "third"],
            250,
            b"250ms",
        ),
        (
            "02_ssh.serial.sh",
            "\n  \nSpaced Label  \n  detail one\n\n",
            "Spaced Label",
            &["detail one"],
            1500,
            b"1.5s",
        ),
        ("plain.sh", "", "plain", &[], 0, b"0ms"),
        ("10-UPPER.sh", "   \t  \n", "UPPER", &[], 10500, b"11s"),
        ("noext", "only\n", "only", &[], 999, b"999ms"),
        (
            "a.sh",
            "line without trailing newline",
            "line without trailing newline",
            &[],
            10000,
            b"10s",
        ),
        ("b.sh", "l1\r\nl2\r\n", "l1", &["l2"], 5, b"5ms"),
        (
            "c.sh",
            "hüüks target\n detail \n",
            "hüüks target",
            &["detail"],
            42,
            b"42ms",
        ),
    ];
    for &(script, log, expected_label, expected_details, elapsed, duration) in cases {
        let (label, details) = merges::result_label(OsStr::new(script), log);
        assert_eq!(label, OsStr::new(expected_label), "label {script:?}");
        assert_eq!(
            details.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_details,
            "details {script:?}"
        );
        assert_eq!(dot::progress_ui::duration_ms(elapsed), duration);
    }
}

fn marker_palette() -> Palette {
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
fn render_result_cases_agree() {
    let cases: &[RenderCase<'_>] = &[
        (b"ok", b"Friendly Name", 250, &[b"second line", b"third"], b"  <G>ok      <R> Friendly Name                <D>250ms<R>\n    <D>second line<R>\n    <D>third<R>\n"),
        (b"warning", b"UPPER", 10500, &[], b"  <Y>warning <R> UPPER                        <D>11s<R>\n"),
        (b"ok", b"solo", 999, &[], b"  <G>ok      <R> solo                         <D>999ms<R>\n"),
        (b"warning", b"l1", 1500, &[b"l2"], b"  <Y>warning <R> l1                           <D>1.5s<R>\n    <D>l2<R>\n"),
    ];
    for &(status, label, elapsed, detail_slices, expected) in cases {
        let details = detail_slices
            .iter()
            .map(|line| line.to_vec())
            .collect::<Vec<_>>();
        let (output, live) = merges::render_result(
            &marker_palette(),
            false,
            false,
            status,
            label,
            elapsed,
            &details,
            false,
        );
        assert_eq!(output, expected, "render {label:?}");
        assert!(!live);
    }
}

#[test]
fn capture_cases_agree() {
    use merges::CaptureAction::{
        ShowEmptyWarning as Empty, ShowLogWarning as Log, ShowResult as Show, Silent, Skipped,
    };
    type Row<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        &'a str,
        &'a str,
        bool,
        merges::CaptureAction,
    );
    let cases: [Row<'_>; 18] = [
        (None, None, None, "0", "0", false, Skipped),
        (Some("0"), Some("0"), Some("0"), "0", "0", false, Skipped),
        (
            Some("1"),
            Some("0"),
            Some("0"),
            "0",
            "0",
            false,
            Silent {
                warning: false,
                elapsed_ms: 0,
            },
        ),
        (
            Some("1"),
            Some("0"),
            Some("42"),
            "1",
            "0",
            false,
            Show {
                warning: false,
                elapsed_ms: 42,
            },
        ),
        (Some("1"), Some("1"), Some("250"), "0", "0", true, Log),
        (Some("1"), Some("1"), Some("250"), "0", "0", false, Empty),
        (
            Some("1"),
            Some("2"),
            Some("1500"),
            "1",
            "0",
            true,
            Show {
                warning: true,
                elapsed_ms: 1500,
            },
        ),
        (Some("1"), None, Some("0"), "0", "0", true, Log),
        (
            Some("1"),
            Some("bogus"),
            Some("7"),
            "0",
            "0",
            true,
            Silent {
                warning: false,
                elapsed_ms: 7,
            },
        ),
        (
            Some("1\n"),
            Some("0\n"),
            Some("9\n"),
            "0",
            "0",
            false,
            Silent {
                warning: false,
                elapsed_ms: 9,
            },
        ),
        (Some("1"), Some("1"), Some("0"), "1", "1", true, Log),
        (
            Some("1"),
            Some("0"),
            None,
            "1",
            "0",
            false,
            Show {
                warning: false,
                elapsed_ms: 0,
            },
        ),
        (Some("2"), Some("0"), Some("0"), "1", "0", false, Skipped),
        (
            Some("1"),
            Some("00"),
            Some("0"),
            "0",
            "0",
            false,
            Silent {
                warning: false,
                elapsed_ms: 0,
            },
        ),
        (Some("1"), Some("3"), Some("5"), "bogus", "0", false, Empty),
        (
            Some("1"),
            Some("0"),
            Some("0"),
            "1",
            "bogus",
            false,
            Show {
                warning: false,
                elapsed_ms: 0,
            },
        ),
        (
            Some("1"),
            Some("1abc"),
            Some("7"),
            "0",
            "0",
            true,
            Silent {
                warning: true,
                elapsed_ms: 7,
            },
        ),
        (
            Some("1"),
            Some("1abc"),
            Some("7"),
            "1",
            "0",
            false,
            Show {
                warning: true,
                elapsed_ms: 7,
            },
        ),
    ];
    for (index, (has_merge, rc, elapsed, verbose, quiet, log, expected)) in
        cases.into_iter().enumerate()
    {
        assert_eq!(
            merges::capture_action(has_merge, rc, elapsed, verbose, quiet, log),
            expected,
            "capture branch {index}"
        );
    }
}

#[test]
fn hook_specs_cases_agree() {
    let scripts = [
        OsStr::new("/safe/merge-hooks.d/10-foo.sh"),
        OsStr::new("/safe/merge-hooks.d/9-zzz.sh"),
        OsStr::new("/safe/merge-hooks.d/10-aaa.sh"),
        OsStr::new("/safe/merge-hooks.d/20-bar.serial.sh"),
        OsStr::new("/safe/merge-hooks.d/plain.sh"),
    ];
    let expected = vec![
        (
            OsString::from("10-aaa"),
            OsString::from("/safe/merge-hooks.d/10-aaa.sh"),
        ),
        (
            OsString::from("10-foo"),
            OsString::from("/safe/merge-hooks.d/10-foo.sh"),
        ),
        (
            OsString::from("20-bar"),
            OsString::from("/safe/merge-hooks.d/20-bar.serial.sh"),
        ),
        (
            OsString::from("9-zzz"),
            OsString::from("/safe/merge-hooks.d/9-zzz.sh"),
        ),
        (
            OsString::from("plain"),
            OsString::from("/safe/merge-hooks.d/plain.sh"),
        ),
    ];
    assert_eq!(
        merges::collect_specs(&scripts).expect("valid specs"),
        expected,
        "exact byte-sorted paths"
    );
    assert_eq!(merges::collect_specs(&[]).expect("empty specs"), Vec::new());

    let invalid = [
        OsStr::new("/safe/merge-hooks.d/BAD.sh"),
        OsStr::new("/safe/merge-hooks.d/zzz.sh"),
    ];
    let error = merges::collect_specs(&invalid).expect_err("invalid identity");
    assert_eq!(
        error,
        merges::SpecError::InvalidIdentity(OsString::from("BAD.sh"))
    );
    assert_eq!(
        error.to_string(),
        "dot: invalid merge-hook identity: BAD.sh"
    );

    let duplicate = [
        OsStr::new("/safe/merge-hooks.d/10-foo.sh"),
        OsStr::new("/safe/merge-hooks.d/foo.sh"),
    ];
    let error = merges::collect_specs(&duplicate).expect_err("duplicate identity");
    assert_eq!(
        error,
        merges::SpecError::DuplicateIdentity(OsString::from("foo"))
    );
    assert_eq!(error.to_string(), "dot: duplicate merge-hook identity: foo");
}
