//! Native contracts for merge orchestration helpers, preserving the former
//! shell-oracle matrices as literal ordering, output, and failure expectations.

use dot::{merges, progress_ui::Palette};

type ProgressCase<'a> = (&'a str, i64, i64, &'a str, &'a str, &'a [u8]);
type HookProgressCase<'a> = (&'a str, i64, i64, bool, &'a str, &'a [u8]);
type RenderCase<'a> = (&'a [u8], &'a [u8], i64, &'a [&'a [u8]], &'a [u8]);
use std::ffi::{OsStr, OsString};
use std::process::Command;

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
fn hook_progress_matches_the_other_stages_outside_verbose() {
    let cases: &[HookProgressCase<'_>] = &[
        (
            "codex",
            3,
            33,
            false,
            "8",
            b"codex              [--------]  3/33",
        ),
        ("codex", 3, 33, true, "8", b"codex 3/33"),
        (
            "ssh",
            1,
            4,
            false,
            "8",
            b"ssh                [##------] 1/4",
        ),
        ("ssh", 1, 4, true, "8", b"ssh 1/4"),
        (
            "uni-hööks",
            1,
            2,
            false,
            "8",
            b"uni-h\xc3\xb6\xc3\xb6ks        [####----] 1/2",
        ),
        ("uni-hööks", 1, 2, true, "8", "uni-hööks 1/2".as_bytes()),
    ];
    for &(label, done, total, verbose, bar_width, expected) in cases {
        assert_eq!(
            merges::hook_progress_detail(
                label.as_bytes(),
                done,
                total,
                verbose,
                bar_width,
                true,
                false
            ),
            expected,
            "hook progress {label:?} verbose={verbose}"
        );
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
