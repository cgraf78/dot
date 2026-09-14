//! Differential parity tests for the test-coordinator scheduling
//! decisions (`lib/dot/test.sh`, `lib/dot/test/runner.sh`,
//! `lib/dot/test/discovery.sh`) against the live shell: per-source
//! suite timeouts, result-record classification, worker-count
//! selection and validation, the early-wave marker, suite labels,
//! the summary line, suite-identity validation, and name-filter
//! matching.
//!
//! Separate binary because every row shells out to the live
//! coordinator. The nested helpers (`_classify_suite`,
//! `_default_jobs`, `_dot_test_runs_early`) live inside
//! `dot_test_command`, so the harness lifts them verbatim from the
//! worktree file with `sed` (dedent only) and `eval`s them — the
//! logic under test is byte-identical to the shipped coordinator.
//! The one-line inline gates (identity validation, filter match)
//! and the short top-level blocks (jobs validation, summary) are
//! embedded verbatim in the snippets for the same reason.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::test_suites::{
    SuiteClassification, classify_suite, default_jobs, filter_matches, format_summary,
    is_valid_suite_identity, resolve_jobs, runs_early, suite_label, suite_timeout,
};
use dot::test_support::TempDir;

/// Lift one nested `dot_test_command` helper verbatim from a shell
/// file: print its two-space-indented definition and strip exactly
/// that indent, leaving top-level code with identical logic.
const LIFT: &str = "lift() { sed -n \"/^  $1() {/,/^  }/p\" \"$2\" | sed 's/^  //'; }";

/// Run one shell snippet against the live coordinator sources. The
/// locale stays pinned (`LC_ALL=C`): count parsing and character
/// classes must read ASCII on both engines.
fn shell_run(
    home: &Path,
    snippet: &str,
    path: &OsString,
    vars: &[(&str, &str)],
) -> (i32, String, String) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(snippet);
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in vars {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8(output.stdout).expect("shell stdout"),
        String::from_utf8(output.stderr).expect("shell stderr"),
    )
}

/// Ambient PATH for rows without tool fakes.
fn ambient_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

/// Isolated client home for one test.
fn home(tag: &str) -> TempDir {
    TempDir::new(tag).expect("fixture home")
}

/// Executable fakes (`getconf`, `uname`, `sysctl`) shadowing the
/// ambient toolchain so `_default_jobs` probes canned values.
struct Fakes {
    dir: TempDir,
}

impl Fakes {
    /// Write each fake as a shell script with `body` (e.g.
    /// `echo 8` or `exit 1`) under an exec-capable directory.
    fn build(tag: &str, getconf: &str, uname: &str, sysctl: &str) -> Self {
        let dir = TempDir::new_exec(tag).expect("fakes dir");
        for (name, body) in [("getconf", getconf), ("uname", uname), ("sysctl", sysctl)] {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake");
            }
        }
        Self { dir }
    }

    /// PATH with the fakes ahead of the ambient toolchain.
    fn path(&self) -> OsString {
        let mut paths = vec![self.dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(&ambient_path()));
        std::env::join_paths(paths).expect("join PATH")
    }
}

#[test]
fn suite_timeout_rows_agree() {
    let dir = home("suite-timeout");
    let path = ambient_path();
    // (source, DOT_TEST_SUITE_TIMEOUT_SECONDS override, want)
    let rows: &[(&str, Option<&str>, &str)] = &[
        ("provider", None, "900"),
        ("local", None, "600"),
        ("extension", None, "300"),
        ("provider", Some("45"), "45"),
        ("local", Some(""), "600"),
        ("provider", Some("0"), "0"),
        ("local", Some("fast"), "fast"),
    ];
    let snippet = "REPO=$1; . \"$REPO/lib/dot/test.sh\"\n_dot_test_suite_timeout \"$SOURCE\"";
    for (source, override_value, want) in rows.iter().copied() {
        let mut vars: Vec<(&str, &str)> = vec![("SOURCE", source)];
        if let Some(value) = override_value {
            vars.push(("DOT_TEST_SUITE_TIMEOUT_SECONDS", value));
        }
        let (code, out, err) = shell_run(dir.path(), snippet, &path, &vars);
        assert_eq!(code, 0, "harness exit for {source:?}");
        assert!(err.is_empty(), "shell stderr for {source:?}: {err:?}");
        assert_eq!(out, format!("{want}\n"), "shell for {source:?}");
        assert_eq!(
            suite_timeout(source, override_value),
            want,
            "rust for {source:?}"
        );
    }
}

#[test]
fn classify_rows_agree() {
    let dir = home("classify");
    let path = ambient_path();
    // (exit code, result record bytes or missing file, want)
    let rows: Vec<(i32, Option<Vec<u8>>, &str)> = vec![
        (1, Some(b"complete\t3\t0\n".to_vec()), "fail"),
        (2, None, "fail"),
        (0, None, "incomplete"),
        (0, Some(b"".to_vec()), "incomplete"),
        (0, Some(b"complete\t3\t0\n".to_vec()), "pass"),
        (0, Some(b"complete\t0\t0\n".to_vec()), "pass"),
        (0, Some(b"complete\t3\t2\n".to_vec()), "fail"),
        (0, Some(b"skip\ttoo slow\n".to_vec()), "skip"),
        (0, Some(b"skip\n".to_vec()), "skip"),
        (0, Some(b"complete\t3\n".to_vec()), "invalid"),
        (0, Some(b"complete\t01\t0\n".to_vec()), "invalid"),
        (0, Some(b"complete\t-1\t0\n".to_vec()), "invalid"),
        (0, Some(b"complete\t3\t0".to_vec()), "invalid"),
        (0, Some(b"complete\t3\t0\n\n".to_vec()), "invalid"),
        (0, Some(b"bogus\t1\t2\n".to_vec()), "invalid"),
        (0, Some(b"complete\t3\t0\textra\n".to_vec()), "invalid"),
        (0, Some(b"\n".to_vec()), "invalid"),
        (0, Some(b"complete\t3\t2\t\n".to_vec()), "fail"),
        (0, Some(b"skip\ta\tb\n".to_vec()), "invalid"),
        (
            0,
            Some(b"complete\t99999999999999999999999\t0\n".to_vec()),
            "pass",
        ),
    ];
    let snippet = format!(
        "REPO=$1; . \"$REPO/lib/dot/test.sh\"\n{LIFT}\n\
         eval \"$(lift _classify_suite \"$REPO/lib/dot/test.sh\")\"\n\
         _classify_suite \"$RC\" \"$RESULT\""
    );
    for (index, (rc, record, want)) in rows.iter().enumerate() {
        let target: PathBuf = match record {
            Some(bytes) => dir.write(&format!("case-{index:02}.result"), bytes),
            None => dir.path().join(format!("case-{index:02}.missing")),
        };
        let target_text = target.to_string_lossy().into_owned();
        let rc_text = rc.to_string();
        let (code, out, err) = shell_run(
            dir.path(),
            &snippet,
            &path,
            &[("RC", &rc_text), ("RESULT", &target_text)],
        );
        assert_eq!(code, 0, "harness exit for row {index}");
        assert!(err.is_empty(), "shell stderr for row {index}: {err:?}");
        assert_eq!(out, format!("{want}\n"), "shell for row {index}");
        let rust: SuiteClassification = classify_suite(*rc, record.as_deref());
        assert_eq!(rust.as_str(), *want, "rust for row {index}");
    }
}

#[test]
fn default_jobs_rows_agree() {
    let dir = home("default-jobs");
    // (getconf body, uname body, sysctl body, probe for Rust, want)
    let rows: &[(&str, &str, &str, Option<&str>, u32)] = &[
        ("echo 8", "echo Linux", "echo 0", Some("8"), 8),
        ("echo 0", "echo Linux", "echo 0", Some("0"), 1),
        ("echo 1", "echo Linux", "echo 0", Some("1"), 1),
        ("echo 007", "echo Linux", "echo 0", Some("007"), 7),
        ("echo 24", "echo Linux", "echo 0", Some("24"), 24),
        ("echo 25", "echo Linux", "echo 0", Some("25"), 24),
        ("echo 100", "echo Linux", "echo 0", Some("100"), 24),
        ("echo abc", "echo Linux", "echo 0", Some("abc"), 4),
        ("echo ' 8'", "echo Linux", "echo 0", Some(" 8"), 4),
        (
            "echo 1234567890",
            "echo Linux",
            "echo 0",
            Some("1234567890"),
            4,
        ),
        ("exit 1", "echo Linux", "echo 6", None, 4),
        ("exit 1", "echo Darwin", "echo 6", Some("6"), 6),
        ("exit 1", "echo Darwin", "exit 1", None, 4),
        ("exit 1", "echo Darwin", "echo 0", Some("0"), 1),
    ];
    let snippet = format!(
        "REPO=$1\n{LIFT}\n\
         eval \"$(lift _default_jobs \"$REPO/lib/dot/test.sh\")\"\n\
         _default_jobs"
    );
    for (index, (getconf, uname, sysctl, probe, want)) in rows.iter().copied().enumerate() {
        let fakes = Fakes::build(&format!("defjobs-{index:02}"), getconf, uname, sysctl);
        let (code, out, err) = shell_run(dir.path(), &snippet, &fakes.path(), &[]);
        assert_eq!(code, 0, "harness exit for row {index}");
        assert!(err.is_empty(), "shell stderr for row {index}: {err:?}");
        assert_eq!(out, format!("{want}\n"), "shell for row {index}");
        assert_eq!(default_jobs(probe), want, "rust for row {index}");
    }
}

#[test]
fn runs_early_rows_agree() {
    let dir = home("runs-early");
    let path = ambient_path();
    let mut marker_line_20 = Vec::new();
    for filler in 0..19 {
        marker_line_20.extend_from_slice(format!("# filler {filler}\n").as_bytes());
    }
    marker_line_20.extend_from_slice(b"# dot-suite-priority: early\n");
    let mut marker_line_21 = Vec::new();
    for filler in 0..20 {
        marker_line_21.extend_from_slice(format!("# filler {filler}\n").as_bytes());
    }
    marker_line_21.extend_from_slice(b"# dot-suite-priority: early\n");
    let rows: Vec<(Vec<u8>, bool)> = vec![
        (
            b"#!/usr/bin/env bash\n# dot-suite-priority: early\necho hi\n".to_vec(),
            true,
        ),
        (marker_line_20, true),
        (marker_line_21, false),
        (b"#!/usr/bin/env bash\necho hi\n".to_vec(), false),
        (b"  # dot-suite-priority: early\n".to_vec(), false),
        (b"echo '# dot-suite-priority: early'\n".to_vec(), false),
        (b"".to_vec(), false),
    ];
    let snippet = format!(
        "REPO=$1\n{LIFT}\n\
         eval \"$(lift _dot_test_runs_early \"$REPO/lib/dot/test.sh\")\"\n\
         if _dot_test_runs_early \"$SCRIPT\"; then printf 'early\\n'; \
         else printf 'ordinary\\n'; fi"
    );
    for (index, (bytes, want)) in rows.iter().enumerate() {
        let script = dir.write(&format!("suite-{index:02}"), bytes);
        let script_text = script.to_string_lossy().into_owned();
        let (code, out, err) = shell_run(dir.path(), &snippet, &path, &[("SCRIPT", &script_text)]);
        assert_eq!(code, 0, "harness exit for row {index}");
        assert!(err.is_empty(), "shell stderr for row {index}: {err:?}");
        let want_text = if *want { "early\n" } else { "ordinary\n" };
        assert_eq!(out, want_text, "shell for row {index}");
        assert_eq!(runs_early(bytes), *want, "rust for row {index}");
    }
}

#[test]
fn suite_label_rows_agree() {
    let dir = home("suite-label");
    let path = ambient_path();
    let rows: &[(&str, &str)] = &[
        ("dot", "dot"),
        ("core", "core-test"),
        ("my-suite-2", "my-suite-2-test"),
    ];
    let snippet = "REPO=$1\n\
         eval \"$(sed -n '/^_dot_test_suite_label() {/,/^}/p' \"$REPO/lib/dot/test/discovery.sh\")\"\n\
         declare -A suite_names=()\n\
         suite_names[/s]=\"$IDENTITY\"\n\
         _dot_test_suite_label /s";
    for (identity, want) in rows.iter().copied() {
        let (code, out, err) = shell_run(dir.path(), snippet, &path, &[("IDENTITY", identity)]);
        assert_eq!(code, 0, "harness exit for {identity:?}");
        assert!(err.is_empty(), "shell stderr for {identity:?}: {err:?}");
        assert_eq!(out, format!("{want}\n"), "shell for {identity:?}");
        assert_eq!(suite_label(identity), want, "rust for {identity:?}");
    }
}

/// One jobs-validation row: raw request, suite count, getconf body
/// for the empty-request fill, fill value for Rust, want.
type JobsRow<'a> = (&'a str, usize, Option<&'a str>, u32, Option<usize>);

#[test]
fn resolve_jobs_rows_agree() {
    let dir = home("resolve-jobs");
    // (raw request, suite count, getconf body for the empty-request
    // fill, fill value for Rust, want resolved count). The fill value
    // is consumed only by the empty-request row, where it matches the
    // getconf fake (`echo 8`) through `default_jobs`.
    let rows: &[JobsRow<'_>] = &[
        ("4", 3, None, 8, Some(3)),
        ("9", 3, None, 8, Some(3)),
        ("0", 5, None, 8, Some(1)),
        ("10", 9, None, 8, Some(9)),
        ("007", 9, None, 8, None),
        ("1", 1, None, 8, Some(1)),
        ("3", 0, None, 8, Some(0)),
        ("", 3, Some("echo 8"), 8, Some(3)),
        ("abc", 3, None, 8, None),
        ("01", 3, None, 8, None),
        ("00", 2, None, 8, None),
        ("1x", 2, None, 8, None),
        ("1234567890", 2, None, 8, None),
    ];
    let snippet = r##"REPO=$1
lift() { sed -n "/^  $1() {/,/^  }/p" "$2" | sed 's/^  //'; }
eval "$(lift _default_jobs "$REPO/lib/dot/test.sh")"
JOBS_BLOCK=$(sed -n '/^if \[\[ -z "$max_jobs" \]\]; then/,/&& max_jobs=${#scripts\[@\]}$/p' "$REPO/lib/dot/test/runner.sh")
scripts=(); i=0; while [[ $i -lt $COUNT ]]; do scripts+=("s$i"); i=$((i + 1)); done
max_jobs="$RAW"
( eval "$JOBS_BLOCK"; printf 'jobs=%s\n' "$max_jobs" )
printf 'rc=%s\n' "$?""##;
    for (index, (raw, count, getconf, default, want)) in rows.iter().copied().enumerate() {
        if raw.is_empty() {
            assert_eq!(default_jobs(Some("8")), default, "fill for row {index}");
        }
        let tag = format!("jobs-{index:02}");
        let fakes = getconf.map(|body| Fakes::build(&tag, body, "echo Linux", "echo 0"));
        let path = fakes.as_ref().map_or_else(ambient_path, Fakes::path);
        let count_text = count.to_string();
        let (code, out, err) = shell_run(
            dir.path(),
            snippet,
            &path,
            &[("RAW", raw), ("COUNT", &count_text)],
        );
        assert_eq!(code, 0, "harness exit for row {index}");
        let want_err = match want {
            Some(_) => String::new(),
            None => format!("invalid jobs value: {raw}\n"),
        };
        assert_eq!(err, want_err, "shell stderr for row {index}");
        let want_text = match want {
            Some(jobs) => format!("jobs={jobs}\nrc=0\n"),
            None => "rc=2\n".to_string(),
        };
        assert_eq!(out, want_text, "shell for row {index}");
        let rust_text = match resolve_jobs(raw, count, default) {
            Some(jobs) => format!("jobs={jobs}\nrc=0\n"),
            None => "rc=2\n".to_string(),
        };
        assert_eq!(rust_text, want_text, "rust for row {index}");
    }
}

#[test]
fn summary_rows_agree() {
    let dir = home("summary");
    let path = ambient_path();
    // (passed, skipped, failed, total)
    let rows: &[(u64, u64, u64, usize)] = &[(0, 0, 5, 5), (3, 1, 0, 4), (2, 0, 0, 2), (1, 2, 3, 6)];
    let snippet = r##"REPO=$1
SUMMARY_BLOCK=$(sed -n '/^summary="Suites:/,/^summary+=" (/p' "$REPO/lib/dot/test/runner.sh")
scripts=(); i=0; while [[ $i -lt $COUNT ]]; do scripts+=("s$i"); i=$((i + 1)); done
passed=$P; skipped=$S; failed=$F
eval "$SUMMARY_BLOCK"
printf '%s\n' "$summary""##;
    for (index, (passed, skipped, failed, total)) in rows.iter().copied().enumerate() {
        let passed_text = passed.to_string();
        let skipped_text = skipped.to_string();
        let failed_text = failed.to_string();
        let total_text = total.to_string();
        let (code, out, err) = shell_run(
            dir.path(),
            snippet,
            &path,
            &[
                ("P", &passed_text),
                ("S", &skipped_text),
                ("F", &failed_text),
                ("COUNT", &total_text),
            ],
        );
        assert_eq!(code, 0, "harness exit for row {index}");
        assert!(err.is_empty(), "shell stderr for row {index}: {err:?}");
        let want = format!("{}\n", format_summary(passed, skipped, failed, total));
        assert_eq!(out, want, "shell for row {index}");
    }
}

#[test]
fn identity_rows_agree() {
    let dir = home("identity");
    let path = ambient_path();
    let rows: &[(&str, bool)] = &[
        ("core", true),
        ("a", true),
        ("a-b-9", true),
        ("dot", false),
        ("Dot", false),
        ("0abc", false),
        ("a_b", false),
        ("", false),
        ("a b", false),
        ("-a", false),
        ("aBc", false),
    ];
    // Verbatim copy of the one-line inline gate in discovery.sh: the
    // discovery loop owns this predicate, so the oracle embeds its
    // exact condition rather than lifting a function.
    let snippet = "if [[ $CAND =~ ^[a-z][a-z0-9-]*$ && $CAND != dot ]]; \
         then printf 'valid\\n'; else printf 'invalid\\n'; fi";
    for (candidate, want) in rows.iter().copied() {
        let (code, out, err) = shell_run(dir.path(), snippet, &path, &[("CAND", candidate)]);
        assert_eq!(code, 0, "harness exit for {candidate:?}");
        assert!(err.is_empty(), "shell stderr for {candidate:?}: {err:?}");
        let want_text = if want { "valid\n" } else { "invalid\n" };
        assert_eq!(out, want_text, "shell for {candidate:?}");
        assert_eq!(
            is_valid_suite_identity(candidate),
            want,
            "rust for {candidate:?}"
        );
    }
}

#[test]
fn filter_rows_agree() {
    let dir = home("filter");
    let path = ambient_path();
    let rows: &[(&str, &str, bool)] = &[
        ("core", "core", true),
        ("core-extra", "core", true),
        ("coreutils", "core", false),
        ("core", "core-extra", false),
        ("core2", "core", false),
        ("cored", "cor", false),
        ("a-b", "a", true),
    ];
    // Verbatim copy of the one-line inline match in discovery.sh (see
    // the identity test above for why the oracle embeds it).
    let snippet = "if [[ $IDENTITY == \"$FILTER\" || $IDENTITY == \"$FILTER-\"* ]]; \
         then printf 'yes\\n'; else printf 'no\\n'; fi";
    for (identity, filter, want) in rows.iter().copied() {
        let (code, out, err) = shell_run(
            dir.path(),
            snippet,
            &path,
            &[("IDENTITY", identity), ("FILTER", filter)],
        );
        assert_eq!(code, 0, "harness exit for {identity:?}/{filter:?}");
        assert!(
            err.is_empty(),
            "shell stderr for {identity:?}/{filter:?}: {err:?}"
        );
        let want_text = if want { "yes\n" } else { "no\n" };
        assert_eq!(out, want_text, "shell for {identity:?}/{filter:?}");
        assert_eq!(
            filter_matches(identity, filter),
            want,
            "rust for {identity:?}/{filter:?}"
        );
    }
}
