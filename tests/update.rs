//! Differential parity tests for the `dot update` leaf layer against
//! `lib/dot/update.sh`: shdeps job preparation, the skip-inputs stage
//! rows, and the deferred repo-stage finish summary. Every case runs
//! the live shell function and its Rust twin on identical inputs and
//! compares stdout bytes exactly.
//!
//! Stage rows carry sub-second elapsed stamps, so the UI cases retry
//! the comparison when a wall-clock second rolls over mid-snippet; a
//! genuine divergence fails every attempt.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::progress_ui::{Palette, Stage};
use dot::test_support::TempDir;

/// Run one shell snippet with `update.sh` sourced. `extra_env` sets
/// (`Some`) or removes (`None`) variables.
fn shell_run(fixture: &Path, extra_env: &[(&str, Option<&str>)], snippet: &str) -> (i32, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/update.sh\"\n_C_RESET='<R>'\n_C_BOLD='<B>'\n_C_DIM='<D>'\n_C_GREEN='<G>'\n_C_YELLOW='<Y>'\n_C_RED='<E>'\n_C_BLUE='<U>'\n_C_CYAN='<C>'\n_C_WHITE='<W>'\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
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

/// Repo-finish matrix row: forced, current, changed, failed,
/// skipped, items, verbose.
type FinishCase<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
    Option<&'a str>,
);

/// Re-run a byte comparison while a sub-second stamp straddles a
/// wall-clock second; a real divergence fails every attempt.
fn assert_bytes_stable(make: impl Fn() -> (Vec<u8>, Vec<u8>), what: &str) {
    let mut last = (Vec::new(), Vec::new());
    for _ in 0..3 {
        let (expected, out) = make();
        if expected == out {
            return;
        }
        last = (expected, out);
    }
    assert_eq!(last.0, last.1, "{what}");
}

#[test]
fn prepare_shdeps_jobs_sets_and_keeps() {
    // (preset SHDEPS_JOBS, DOT_UPDATE_JOBS): an already-set value
    // (even empty) is kept; otherwise the update-job count applies.
    for (shdeps, jobs) in [
        (None, Some("3")),
        (Some("9"), Some("3")),
        (Some(""), Some("3")),
        (None, Some("abc")),
        (None, None),
        (Some("7"), None),
    ] {
        let dir = TempDir::new("update-shdeps").expect("fixture dir");
        let preset = shdeps
            .map(|value| format!("SHDEPS_JOBS={value} "))
            .unwrap_or_default();
        let workload = jobs
            .map(|value| format!("DOT_UPDATE_JOBS={value} "))
            .unwrap_or_default();
        let (code, out) = shell_run(
            dir.path(),
            &[],
            &format!(
                "{preset}{workload}_dot_update_prepare_shdeps_jobs; printf '%s' \"$SHDEPS_JOBS\""
            ),
        );
        assert_eq!(code, 0, "shell shdeps jobs {shdeps:?} {jobs:?}");
        assert_eq!(
            dot::update::prepare_shdeps_jobs(shdeps, jobs),
            if shdeps.is_some() {
                None
            } else {
                Some(String::from_utf8(out).expect("shdeps jobs utf8"))
            },
            "shdeps jobs parity for {shdeps:?} {jobs:?}"
        );
    }
}

#[test]
fn skip_inputs_renders_four_stage_rows() {
    let palette = marker_palette();
    for (live, verbose) in [(false, None), (true, Some("1"))] {
        let mut env: Vec<(&str, Option<&str>)> = vec![("DOT_QUIET", None)];
        if live {
            env.push(("DOT_UI_FORCE_LIVE", Some("1")));
        }
        env.push(("DOT_VERBOSE", verbose));
        assert_bytes_stable(
            || {
                let dir = TempDir::new("update-skip").expect("fixture dir");
                let (code, out) = shell_run(
                    dir.path(),
                    &env,
                    "DOT_UI_INDEX=0; DOT_UI_TOTAL=0; _dot_update_skip_inputs 'repo sync failed'",
                );
                assert_eq!(code, 0, "shell skip inputs");
                // The C-locale shell takes ASCII paths; the stage
                // resolves ascii the same way.
                let mut stage = if live {
                    Stage::begin(palette.clone(), "0", false, true, false, true)
                } else {
                    Stage::begin(palette.clone(), "0", false, false, false, false)
                };
                let expected =
                    dot::update::skip_inputs(&mut stage, b"repo sync failed", 1000, verbose);
                (expected, out)
            },
            &format!("skip inputs parity for live {live} verbose {verbose:?}"),
        );
    }
}

#[test]
fn repo_stage_finish_status_and_summary_agree() {
    let palette = marker_palette();
    // (forced, current, changed, failed, skipped, items, verbose).
    let cases: Vec<FinishCase<'_>> = vec![
        (None, None, None, None, None, "", None),
        (
            None,
            Some("1"),
            Some("2"),
            None,
            None,
            "alpha\nbeta",
            Some("0"),
        ),
        (None, None, None, Some("1"), None, "", None),
        (Some("1"), None, None, None, None, "", None),
        (None, None, None, None, Some("3"), "solo", Some("1")),
        (None, None, Some("abc"), Some("abc"), None, "\n\n", None),
        (
            Some("0"),
            Some("5"),
            None,
            None,
            Some("2"),
            "a\n\nb\n",
            Some("0"),
        ),
    ];
    for (forced, current, changed, failed, skipped, items, verbose) in cases {
        assert_bytes_stable(
            || {
                let dir = TempDir::new("update-finish").expect("fixture dir");
                let mut exports = String::from(
                    "DOT_UI_INDEX=0; DOT_UI_TOTAL=0; DOT_REPO_STAGE_DEFERRED_ACTIVE=1;",
                );
                for (name, value) in [
                    ("DOT_REPO_AGG_CURRENT", current),
                    ("DOT_REPO_AGG_CHANGED", changed),
                    ("DOT_REPO_AGG_FAILED", failed),
                    ("DOT_REPO_AGG_SKIPPED", skipped),
                ] {
                    if let Some(value) = value {
                        exports.push_str(&format!("{name}={value}; "));
                    }
                }
                let mut env: Vec<(&str, Option<&str>)> =
                    vec![("DOT_QUIET", None), ("DOT_VERBOSE", verbose)];
                env.push(("DOT_REPO_AGG_CHANGED_ITEMS", Some(items)));
                let forced_arg = forced.unwrap_or("0");
                let (code, out) = shell_run(
                    dir.path(),
                    &env,
                    &format!("{exports}_dot_update_repo_stage_finish {forced_arg}"),
                );
                assert_eq!(code, 0, "shell repo finish");
                let mut stage = Stage::begin(palette.clone(), "0", false, false, false, false);
                let expected = dot::update::repo_stage_finish(
                    &mut stage,
                    &dot::update::RepoStageFinish {
                        deferred_active: true,
                        forced_failure: forced,
                        agg_current: current,
                        agg_changed: changed,
                        agg_failed: failed,
                        agg_skipped: skipped,
                        changed_items: items.as_bytes(),
                        verbose,
                    },
                    0,
                );
                (expected, out)
            },
            &format!(
                "repo finish parity for {forced:?} {current:?} {changed:?} {failed:?} {skipped:?} {items:?} {verbose:?}"
            ),
        );
    }
}

#[test]
fn repo_stage_finish_silent_without_deferral() {
    let dir = TempDir::new("update-finish-idle").expect("fixture dir");
    let palette = marker_palette();
    let (code, out) = shell_run(
        dir.path(),
        &[("DOT_QUIET", None)],
        "DOT_UI_INDEX=0; DOT_UI_TOTAL=0; DOT_REPO_AGG_FAILED=9; _dot_update_repo_stage_finish 1",
    );
    assert_eq!(code, 0, "shell idle repo finish");
    let mut stage = Stage::begin(palette, "0", false, false, false, false);
    assert_eq!(
        dot::update::repo_stage_finish(
            &mut stage,
            &dot::update::RepoStageFinish {
                deferred_active: false,
                forced_failure: Some("1"),
                agg_current: None,
                agg_changed: None,
                agg_failed: Some("9"),
                agg_skipped: None,
                changed_items: b"",
                verbose: None,
            },
            0,
        ),
        out,
        "idle repo finish stays silent"
    );
}
