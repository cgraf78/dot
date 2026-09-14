//! Native contracts for `dot test` discovery and scheduling policy.

use dot::test_suites::{
    SuiteClassification, classify_suite, default_jobs, filter_matches, format_summary,
    is_valid_suite_identity, resolve_jobs, runs_early, suite_label, suite_timeout,
};

#[test]
fn suite_timeout_contract() {
    for (source, override_value, expected) in [
        ("provider", None, "900"),
        ("local", None, "600"),
        ("extension", None, "300"),
        ("provider", Some("45"), "45"),
        ("local", Some(""), "600"),
        ("provider", Some("0"), "0"),
        ("local", Some("fast"), "fast"),
    ] {
        assert_eq!(suite_timeout(source, override_value), expected);
    }
}

#[test]
fn result_classification_contract() {
    use SuiteClassification::{Fail, Incomplete, Invalid, Pass, Skip};
    let rows: Vec<(i32, Option<&[u8]>, SuiteClassification)> = vec![
        (1, Some(b"complete\t3\t0\n"), Fail),
        (2, None, Fail),
        (0, None, Incomplete),
        (0, Some(b""), Incomplete),
        (0, Some(b"complete\t3\t0\n"), Pass),
        (0, Some(b"complete\t0\t0\n"), Pass),
        (0, Some(b"complete\t3\t2\n"), Fail),
        (0, Some(b"skip\ttoo slow\n"), Skip),
        (0, Some(b"skip\n"), Skip),
        (0, Some(b"complete\t3\n"), Invalid),
        (0, Some(b"complete\t01\t0\n"), Invalid),
        (0, Some(b"complete\t-1\t0\n"), Invalid),
        (0, Some(b"complete\t3\t0"), Invalid),
        (0, Some(b"complete\t3\t0\n\n"), Invalid),
        (0, Some(b"bogus\t1\t2\n"), Invalid),
        (0, Some(b"complete\t3\t0\textra\n"), Invalid),
        (0, Some(b"\n"), Invalid),
        (0, Some(b"complete\t3\t2\t\n"), Fail),
        (0, Some(b"skip\ta\tb\n"), Invalid),
        (0, Some(b"complete\t99999999999999999999999\t0\n"), Pass),
    ];
    for (exit_code, record, expected) in rows {
        assert_eq!(classify_suite(exit_code, record), expected);
    }
}

#[test]
fn default_jobs_contract() {
    for (probe, expected) in [
        (Some("8"), 8),
        (Some("0"), 1),
        (Some("1"), 1),
        (Some("007"), 7),
        (Some("24"), 24),
        (Some("25"), 24),
        (Some("100"), 24),
        (Some("abc"), 4),
        (Some(" 8"), 4),
        (Some("1234567890"), 4),
        (None, 4),
    ] {
        assert_eq!(default_jobs(probe), expected);
    }
}

#[test]
fn early_priority_contract() {
    let mut line_20 = Vec::new();
    for index in 0..19 {
        line_20.extend_from_slice(format!("# filler {index}\n").as_bytes());
    }
    line_20.extend_from_slice(b"# dot-suite-priority: early\n");
    let mut line_21 = b"# extra\n".to_vec();
    line_21.extend_from_slice(&line_20);
    for (bytes, expected) in [
        (b"#!/bin/sh\n# dot-suite-priority: early\n".as_slice(), true),
        (line_20.as_slice(), true),
        (line_21.as_slice(), false),
        (b"  # dot-suite-priority: early\n".as_slice(), false),
        (b"# dot-suite-priority: early".as_slice(), false),
        (b"".as_slice(), false),
    ] {
        assert_eq!(runs_early(bytes), expected);
    }
}

#[test]
fn suite_label_contract() {
    assert_eq!(suite_label("dot"), "dot");
    assert_eq!(suite_label("core"), "core-test");
    assert_eq!(suite_label("my-suite-2"), "my-suite-2-test");
}

#[test]
fn jobs_resolution_contract() {
    for (raw, count, default, expected) in [
        ("4", 3, 8, Some(3)),
        ("9", 3, 8, Some(3)),
        ("0", 5, 8, Some(1)),
        ("10", 9, 8, Some(9)),
        ("007", 9, 8, None),
        ("1", 1, 8, Some(1)),
        ("3", 0, 8, Some(0)),
        ("", 3, 8, Some(3)),
        ("abc", 3, 8, None),
        ("01", 3, 8, None),
        ("00", 2, 8, None),
        ("1x", 2, 8, None),
        ("1234567890", 2, 8, None),
    ] {
        assert_eq!(resolve_jobs(raw, count, default), expected);
    }
}

#[test]
fn summary_contract() {
    for (passed, skipped, failed, total, expected) in [
        (0, 0, 5, 5, "Suites: 0 passed, 5 failed (5 total)"),
        (3, 1, 0, 4, "Suites: 3 passed, 1 skipped (4 total)"),
        (2, 0, 0, 2, "Suites: 2 passed (2 total)"),
        (
            1,
            2,
            3,
            6,
            "Suites: 1 passed, 2 skipped, 3 failed (6 total)",
        ),
    ] {
        assert_eq!(format_summary(passed, skipped, failed, total), expected);
    }
}

#[test]
fn suite_identity_contract() {
    for valid in ["core", "a", "a-b-9"] {
        assert!(is_valid_suite_identity(valid), "{valid:?}");
    }
    for invalid in ["dot", "Dot", "0abc", "a_b", "", "a b", "-a", "aBc"] {
        assert!(!is_valid_suite_identity(invalid), "{invalid:?}");
    }
}

#[test]
fn suite_filter_contract() {
    for (identity, filter, expected) in [
        ("core", "core", true),
        ("core-extra", "core", true),
        ("coreutils", "core", false),
        ("core", "core-extra", false),
        ("core2", "core", false),
        ("cored", "cor", false),
        ("a-b", "a", true),
    ] {
        assert_eq!(filter_matches(identity, filter), expected);
    }
}
