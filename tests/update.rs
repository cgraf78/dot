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
use dot::update::{FinalizeInputs, PullOverlayPhase, PullPhaseOutcome, SyncReposInputs};

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
        ". \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/update.sh\"\n. \"$1/lib/dot/repos/pull.sh\"\n_C_RESET='<R>'\n_C_BOLD='<B>'\n_C_DIM='<D>'\n_C_GREEN='<G>'\n_C_YELLOW='<Y>'\n_C_RED='<E>'\n_C_BLUE='<U>'\n_C_CYAN='<C>'\n_C_WHITE='<W>'\n{snippet}"
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

/// Re-run a byte comparison while load stalls a shell past a stamp
/// boundary; a real divergence fails every attempt. Eight tries ride
/// out the fork-storm stalls of a parallel suite without hiding a
/// genuine mismatch.
fn assert_bytes_stable(make: impl Fn() -> (Vec<u8>, Vec<u8>), what: &str) {
    let mut last = (Vec::new(), Vec::new());
    for _ in 0..8 {
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

/// Run one shell snippet capturing stderr too, for the folds
/// whose warnings land on fd 2.
fn shell_run_full(
    fixture: &Path,
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/update.sh\"\n. \"$1/lib/dot/repos/pull.sh\"\n_C_RESET='<R>'\n_C_BOLD='<B>'\n_C_DIM='<D>'\n_C_GREEN='<G>'\n_C_YELLOW='<Y>'\n_C_RED='<E>'\n_C_BLUE='<U>'\n_C_CYAN='<C>'\n_C_WHITE='<W>'\n{snippet}"
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

#[test]
fn no_base_pull_agrees() {
    let palette = marker_palette();
    // (reply, overlay failed, pull rc, verbose): the pull rc never
    // reaches the caller; only the failure count decides.
    for (reply, failed, stub_rc, verbose) in [
        (None, None, "0", None),
        (Some("2 overlays pulled"), None, "0", None),
        (Some("2 overlays pulled"), Some("2"), "0", None),
        (None, Some("1"), "0", Some("1")),
        (Some("stale lock"), Some("0"), "3", None),
        (None, Some(""), "0", None),
    ] {
        assert_bytes_stable(
            || {
                let dir = TempDir::new("update-nobase").expect("fixture dir");
                let mut env: Vec<(&str, Option<&str>)> =
                    vec![("DOT_QUIET", None), ("DOT_VERBOSE", verbose)];
                env.push(("STUB_REPLY", Some(reply.unwrap_or(""))));
                env.push(("STUB_FAILED", failed));
                env.push(("STUB_RC", Some(stub_rc)));
                let (code, out) = shell_run(
                    dir.path(),
                    &env,
                    "_ensure_repo_config() { :; }\n_pull_overlays() { REPLY=\"$STUB_REPLY\"; DOT_PULL_OVERLAY_FAILED=\"$STUB_FAILED\"; return \"$STUB_RC\"; }\nDOT_UI_INDEX=0; DOT_UI_TOTAL=0; _dot_update_no_base_pull; rc=$?; printf 'rc=%s' \"$rc\"",
                );
                let mut stage = Stage::begin(palette.clone(), "0", false, false, false, false);
                let (mut expected, success) = dot::update::no_base_pull(
                    &mut stage,
                    reply.map(str::as_bytes),
                    failed,
                    0,
                    verbose,
                );
                expected
                    .extend_from_slice(format!("rc={}", if success { 0 } else { 1 }).as_bytes());
                // The C-locale shell errors its `-eq` gates to fd 2;
                // the harness drops stderr like the cases above.
                assert_eq!(code, 0, "shell harness no base pull");
                (expected, out)
            },
            &format!("no base pull parity for {reply:?} {failed:?} {stub_rc} {verbose:?}"),
        );
    }
}

/// Default pull-phase inputs: deferred staging idle, everything
/// unset, the pull step clean.
fn phase_inputs<'a>() -> PullOverlayPhase<'a> {
    PullOverlayPhase {
        deferred_active: false,
        label: b"overlays",
        count: 0,
        done: None,
        verbose: None,
        bar_width: "8",
        ascii: true,
        multibyte: false,
        pull_rc: 0,
        pull_current: None,
        pull_changed: None,
        pull_failed: None,
        pull_skipped: None,
        pull_changed_items: b"",
        agg_current: None,
        agg_changed: None,
        agg_failed: None,
        agg_skipped: None,
        agg_changed_items: b"",
    }
}

#[test]
fn pull_overlay_phase_passthrough() {
    let palette = marker_palette();
    for pull_rc in ["0", "2", "5"] {
        let dir = TempDir::new("update-phase-idle").expect("fixture dir");
        let (code, out) = shell_run(
            dir.path(),
            &[("DOT_QUIET", None)],
            &format!(
                "_pull_overlays() {{ return {pull_rc}; }}\n_pull_overlay_count() {{ printf '%s' '9'; }}\nDOT_UI_INDEX=0; DOT_UI_TOTAL=0; _dot_update_pull_overlay_phase overlays; rc=$?; printf 'rc=%s' \"$rc\""
            ),
        );
        assert_eq!(code, 0, "shell harness phase idle");
        let mut stage = Stage::begin(palette.clone(), "0", false, false, false, false);
        let mut inputs = phase_inputs();
        inputs.pull_rc = pull_rc.parse().expect("stub rc");
        match dot::update::pull_overlay_phase(&mut stage, &inputs, 0) {
            PullPhaseOutcome::Passthrough { rc } => {
                assert_eq!(
                    format!("rc={rc}"),
                    String::from_utf8(out).expect("idle dump")
                );
            }
            PullPhaseOutcome::Aggregated { .. } => panic!("idle phase aggregated"),
        }
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

/// Aggregated phase matrix row: count, done, pull rc, pull
/// current/changed/failed/skipped, pull items, pre-pull
/// current/changed/failed/skipped, pre-pull items, verbose.
#[allow(clippy::too_many_arguments)]
struct PhaseCase<'a> {
    count: &'a str,
    done: Option<&'a str>,
    pull_rc: &'a str,
    pull_current: Option<&'a str>,
    pull_changed: Option<&'a str>,
    pull_failed: Option<&'a str>,
    pull_skipped: Option<&'a str>,
    pull_items: &'a str,
    agg_current: Option<&'a str>,
    agg_changed: Option<&'a str>,
    agg_failed: Option<&'a str>,
    agg_skipped: Option<&'a str>,
    agg_items: &'a str,
    verbose: Option<&'a str>,
}

#[test]
fn pull_overlay_phase_aggregates() {
    let palette = marker_palette();
    let cases = [
        PhaseCase {
            count: "0",
            done: None,
            pull_rc: "0",
            pull_current: Some("2"),
            pull_changed: None,
            pull_failed: None,
            pull_skipped: None,
            pull_items: "",
            agg_current: None,
            agg_changed: None,
            agg_failed: None,
            agg_skipped: None,
            agg_items: "",
            verbose: None,
        },
        PhaseCase {
            count: "3",
            done: None,
            pull_rc: "0",
            pull_current: Some("1"),
            pull_changed: Some("2"),
            pull_failed: Some("0"),
            pull_skipped: Some("1"),
            pull_items: "alpha",
            agg_current: Some("4"),
            agg_changed: None,
            agg_failed: None,
            agg_skipped: Some("2"),
            agg_items: "base",
            verbose: Some("1"),
        },
        PhaseCase {
            count: "2",
            done: Some("4"),
            pull_rc: "1",
            pull_current: None,
            pull_changed: Some("1"),
            pull_failed: Some("2"),
            pull_skipped: None,
            pull_items: "x/y\nz",
            agg_current: Some("1"),
            agg_changed: Some("1"),
            agg_failed: Some("1"),
            agg_skipped: None,
            agg_items: "",
            verbose: Some("1"),
        },
        PhaseCase {
            count: "1",
            done: Some(""),
            pull_rc: "0",
            pull_current: None,
            pull_changed: None,
            pull_failed: Some("0"),
            pull_skipped: None,
            pull_items: "",
            agg_current: None,
            agg_changed: None,
            agg_failed: None,
            agg_skipped: None,
            agg_items: "",
            verbose: None,
        },
    ];
    for case in &cases {
        assert_bytes_stable(
            || {
                let dir = TempDir::new("update-phase").expect("fixture dir");
                let mut env: Vec<(&str, Option<&str>)> =
                    vec![("DOT_QUIET", None), ("DOT_VERBOSE", case.verbose)];
                env.push(("STUB_COUNT", Some(case.count)));
                env.push(("STUB_RC", Some(case.pull_rc)));
                env.push(("STUB_CUR", case.pull_current));
                env.push(("STUB_CHG", case.pull_changed));
                env.push(("STUB_FAIL", case.pull_failed));
                env.push(("STUB_SKIP", case.pull_skipped));
                env.push(("STUB_PULL_ITEMS", Some(case.pull_items)));
                env.push(("STUB_DONE", case.done));
                env.push(("STUB_AGG_CUR", case.agg_current));
                env.push(("STUB_AGG_CHG", case.agg_changed));
                env.push(("STUB_AGG_FAIL", case.agg_failed));
                env.push(("STUB_AGG_SKIP", case.agg_skipped));
                env.push(("STUB_AGG_ITEMS", Some(case.agg_items)));
                let (code, out) = shell_run(
                    dir.path(),
                    &env,
                    "_pull_overlays() { DOT_PULL_OVERLAY_CURRENT=\"$STUB_CUR\"; DOT_PULL_OVERLAY_CHANGED=\"$STUB_CHG\"; DOT_PULL_OVERLAY_FAILED=\"$STUB_FAIL\"; DOT_PULL_OVERLAY_SKIPPED=\"$STUB_SKIP\"; DOT_PULL_OVERLAY_CHANGED_ITEMS=\"$STUB_PULL_ITEMS\"; return \"$STUB_RC\"; }\n_pull_overlay_count() { printf '%s' \"$STUB_COUNT\"; }\nDOT_UI_INDEX=0; DOT_UI_TOTAL=5; _ui_stage_start \"Overlays\" \"pulling overlays\"\nDOT_REPO_PROGRESS_DONE=\"$STUB_DONE\"; DOT_REPO_AGG_CURRENT=\"$STUB_AGG_CUR\"; DOT_REPO_AGG_CHANGED=\"$STUB_AGG_CHG\"; DOT_REPO_AGG_FAILED=\"$STUB_AGG_FAIL\"; DOT_REPO_AGG_SKIPPED=\"$STUB_AGG_SKIP\"; DOT_REPO_AGG_CHANGED_ITEMS=\"$STUB_AGG_ITEMS\";\nDOT_REPO_STAGE_DEFERRED_ACTIVE=1; _dot_update_pull_overlay_phase overlays; rc=$?\nprintf 'rc=%s done=%s total=%s cur=%s chg=%s fail=%s skip=%s\\n' \"$rc\" \"$DOT_REPO_PROGRESS_DONE\" \"$DOT_REPO_PROGRESS_TOTAL\" \"$DOT_REPO_AGG_CURRENT\" \"$DOT_REPO_AGG_CHANGED\" \"$DOT_REPO_AGG_FAILED\" \"$DOT_REPO_AGG_SKIPPED\"\nprintf 'items<%s>\\n' \"$DOT_REPO_AGG_CHANGED_ITEMS\"",
                );
                assert_eq!(code, 0, "shell harness phase aggregate");
                // Split the trailing variable dump off the stage
                // rows: the items payload rides last so embedded
                // newlines survive.
                let marker = b"\nitems<";
                let pos = out
                    .windows(marker.len())
                    .rposition(|window| window == marker)
                    .expect("items dump");
                let (rows, dump) = out.split_at(pos);
                let dump = &dump[marker.len()..];
                let items = dump.strip_suffix(b">\n").expect("items close");
                let header = rows
                    .rsplit(|byte| *byte == b'\n')
                    .next()
                    .expect("dump line");
                let header = String::from_utf8(header.to_vec()).expect("dump utf8");
                let mut fields = header.split(' ');
                let mut field = || {
                    fields
                        .next()
                        .expect("dump field")
                        .split('=')
                        .nth(1)
                        .expect("dump value")
                };
                let (rc, done, total, cur, chg, fail, skip) = (
                    field(),
                    field(),
                    field(),
                    field(),
                    field(),
                    field(),
                    field(),
                );
                let mut stage = Stage::begin(palette.clone(), "5", false, false, false, false);
                let mut prefix = stage.start(
                    b"Overlays",
                    Some(b"pulling overlays".as_slice()),
                    0,
                    case.verbose,
                );
                let mut inputs = phase_inputs();
                inputs.deferred_active = true;
                inputs.count = case.count.parse().expect("stub count");
                inputs.done = case.done;
                inputs.verbose = case.verbose;
                inputs.pull_rc = case.pull_rc.parse().expect("stub rc");
                inputs.pull_current = case.pull_current;
                inputs.pull_changed = case.pull_changed;
                inputs.pull_failed = case.pull_failed;
                inputs.pull_skipped = case.pull_skipped;
                inputs.pull_changed_items = case.pull_items.as_bytes();
                inputs.agg_current = case.agg_current;
                inputs.agg_changed = case.agg_changed;
                inputs.agg_failed = case.agg_failed;
                inputs.agg_skipped = case.agg_skipped;
                inputs.agg_changed_items = case.agg_items.as_bytes();
                match dot::update::pull_overlay_phase(&mut stage, &inputs, 0) {
                    PullPhaseOutcome::Aggregated {
                        progress,
                        done: got_done,
                        total: got_total,
                        current,
                        changed,
                        failed,
                        skipped,
                        changed_items,
                        success,
                    } => {
                        prefix.extend_from_slice(&progress);
                        assert_eq!(rc, if success { "0" } else { "1" }, "phase rc");
                        assert_eq!(done, got_done.to_string(), "progress done");
                        assert_eq!(total, got_total.to_string(), "progress total");
                        assert_eq!(cur, current.to_string(), "agg current");
                        assert_eq!(chg, changed.to_string(), "agg changed");
                        assert_eq!(fail, failed.to_string(), "agg failed");
                        assert_eq!(skip, skipped.to_string(), "agg skipped");
                        assert_eq!(items, changed_items.as_slice(), "agg items");
                    }
                    PullPhaseOutcome::Passthrough { .. } => panic!("active phase idle"),
                }
                // The rows still carry the variable dump line;
                // only the stage bytes compare against the fold.
                let cut = rows
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map(|pos| pos + 1)
                    .unwrap_or(0);
                (prefix, rows[..cut].to_vec())
            },
            &format!(
                "phase parity for count {} done {:?} rc {}",
                case.count, case.done, case.pull_rc
            ),
        );
    }
}

/// Quote one overlay entry for the converge repro snippet (test
/// data stays free of single quotes).
fn quote_entry(entry: &str) -> String {
    format!("'{entry}'")
}

#[test]
fn converge_additions_agrees() {
    // (phase-one entries, eligible entries).
    let cases: Vec<(Vec<&str>, Vec<&str>)> = vec![
        (
            vec!["a|/p/a|u|x|0|git", "b|/p/b|u|x|0|git"],
            vec!["b|/p/b2|u|x|0|git", "c|/p/c|u|x|0|none"],
        ),
        (vec![], vec!["a|/p/a|u", "b|/p/b|u"]),
        (vec!["a|/p/a|u"], vec![]),
        (vec![], vec![]),
        (vec!["lonely"], vec!["lonely", "other"]),
        (vec!["a|/one", "a|/two"], vec!["a|/three", "b|x"]),
        (
            vec!["a|/p/a|u", "b|/p/b|u", "c|/p/c|u"],
            vec!["c|/p/c|u", "b|/p/b|u", "a|/p/a|u", "d|/p/d|u"],
        ),
    ];
    for (phase_one, eligible) in &cases {
        let dir = TempDir::new("update-converge").expect("fixture dir");
        // The repro runs the exact selection lines from
        // `_dot_converge_overlays`, not the orchestrator around
        // them: discovery and pulls stay in shell.
        let phase_list = phase_one
            .iter()
            .map(|entry| quote_entry(entry))
            .collect::<Vec<_>>()
            .join(" ");
        let eligible_list = eligible
            .iter()
            .map(|entry| quote_entry(entry))
            .collect::<Vec<_>>()
            .join(" ");
        // Plain concatenation, not `format!`: the repro is all
        // shell braces, which need no escaping this way.
        let mut repro = String::from("PHASE_ONE=(");
        repro.push_str(&phase_list);
        repro.push_str("); ELIGIBLE=(");
        repro.push_str(&eligible_list);
        repro.push_str(
            "); declare -A phase_one_names=(); for entry in \"${PHASE_ONE[@]+\"${PHASE_ONE[@]}\"}\"; do name=${entry%%|*}; phase_one_names[\"$name\"]=1; done; additions=(); for entry in \"${ELIGIBLE[@]+\"${ELIGIBLE[@]}\"}\"; do name=${entry%%|*}; [[ -n ${phase_one_names[$name]+x} ]] || additions+=(\"$entry\"); done; printf 'n=%s\\n' \"${#additions[@]}\"; if ((${#additions[@]})); then printf '%s\\n' \"${additions[@]}\"; fi",
        );
        let (code, out) = shell_run(dir.path(), &[("DOT_QUIET", None)], &repro);
        assert_eq!(code, 0, "shell harness converge");
        let text = String::from_utf8(out).expect("converge dump");
        let mut lines = text.lines();
        let count: usize = lines
            .next()
            .expect("count line")
            .strip_prefix("n=")
            .expect("count prefix")
            .parse()
            .expect("count number");
        let shell_additions: Vec<&str> = lines.collect();
        assert_eq!(count, shell_additions.len(), "converge count");
        let rust = dot::update::converge_additions(phase_one, eligible);
        assert_eq!(rust, shell_additions, "converge additions");
    }
}

#[test]
fn overlay_phase_and_converge_status_agree() {
    for (pull_rc, failed) in [
        ("0", None),
        ("0", Some("0")),
        ("0", Some("2")),
        ("1", Some("0")),
        ("3", None),
        ("0", Some("x")),
    ] {
        let dir = TempDir::new("update-phaseok").expect("fixture dir");
        let env: Vec<(&str, Option<&str>)> =
            vec![("STUB_RC", Some(pull_rc)), ("STUB_FAIL", failed)];
        let (code, out) = shell_run(
            dir.path(),
            &env,
            "if [[ \"$STUB_RC\" -eq 0 && \"${STUB_FAIL:-0}\" -eq 0 ]]; then printf 'ok=1'; else printf 'ok=0'; fi",
        );
        assert_eq!(code, 0, "shell harness phase ok");
        let shell_ok = out == b"ok=1";
        let pull_rc_num: i32 = pull_rc.parse().expect("stub rc");
        assert_eq!(
            dot::update::overlay_phase_ok(pull_rc_num, failed),
            shell_ok,
            "phase ok for rc {pull_rc} failed {failed:?}"
        );
    }
    for (phase, final_ok) in [(false, false), (false, true), (true, false), (true, true)] {
        let dir = TempDir::new("update-convstatus").expect("fixture dir");
        let (code, out) = shell_run(
            dir.path(),
            &[
                ("STUB_PHASE", Some(if phase { "0" } else { "1" })),
                ("STUB_FINAL", Some(if final_ok { "0" } else { "1" })),
            ],
            "if [[ \"$STUB_PHASE\" -eq 0 && \"$STUB_FINAL\" -eq 0 ]]; then printf 'ok=1'; else printf 'ok=0'; fi",
        );
        assert_eq!(code, 0, "shell harness converge status");
        assert_eq!(
            dot::update::converge_status(phase, final_ok),
            out == b"ok=1",
            "converge status for {phase} {final_ok}"
        );
    }
}

/// Sync matrix row: base-exists, snapshot, pull, config,
/// skip-provider, converge, lifecycle, and restore outcomes.
struct SyncCase {
    base: &'static str,
    snapshot: &'static str,
    pull: &'static str,
    config: &'static str,
    skip: &'static str,
    converge: &'static str,
    lifecycle: &'static str,
    restore: &'static str,
}

#[test]
fn sync_repos_fold_agrees() {
    let palette = marker_palette();
    let cases = [
        // Happy paths, with and without the provider skip.
        SyncCase {
            base: "0",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "0",
            converge: "0",
            lifecycle: "0",
            restore: "0",
        },
        SyncCase {
            base: "0",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "1",
            converge: "0",
            lifecycle: "0",
            restore: "0",
        },
        // No base checkout at all.
        SyncCase {
            base: "1",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "0",
            converge: "0",
            lifecycle: "0",
            restore: "0",
        },
        // Snapshot failure returns before the stage closes.
        SyncCase {
            base: "0",
            snapshot: "3",
            pull: "0",
            config: "0",
            skip: "1",
            converge: "0",
            lifecycle: "0",
            restore: "0",
        },
        // Pull failure carries its status; the provider export
        // has not run yet even with the skip set.
        SyncCase {
            base: "0",
            snapshot: "0",
            pull: "3",
            config: "0",
            skip: "1",
            converge: "0",
            lifecycle: "0",
            restore: "0",
        },
        // Config failure, plus a restore that warns.
        SyncCase {
            base: "0",
            snapshot: "0",
            pull: "0",
            config: "2",
            skip: "1",
            converge: "0",
            lifecycle: "0",
            restore: "1",
        },
        // Converge failure with the provider skip exported.
        SyncCase {
            base: "0",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "1",
            converge: "1",
            lifecycle: "0",
            restore: "0",
        },
        // Lifecycle failure restores only behind a base checkout.
        SyncCase {
            base: "0",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "0",
            converge: "0",
            lifecycle: "1",
            restore: "0",
        },
        SyncCase {
            base: "1",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "1",
            converge: "0",
            lifecycle: "1",
            restore: "0",
        },
        // No-base converge failure.
        SyncCase {
            base: "1",
            snapshot: "0",
            pull: "0",
            config: "0",
            skip: "0",
            converge: "1",
            lifecycle: "0",
            restore: "0",
        },
    ];
    for case in &cases {
        let dir = TempDir::new("update-sync").expect("fixture dir");
        let env: Vec<(&str, Option<&str>)> = vec![
            ("DOT_QUIET", None),
            ("DOT_DEPENDENCY_PROVIDER", None),
            ("DOT_OVERLAY_LINKS_FROZEN", None),
            ("STUB_BASE", Some(case.base)),
            ("STUB_SNAPSHOT", Some(case.snapshot)),
            ("STUB_PULL", Some(case.pull)),
            ("STUB_CONFIG", Some(case.config)),
            ("STUB_SKIP", Some(case.skip)),
            ("STUB_CONVERGE", Some(case.converge)),
            ("STUB_LIFECYCLE", Some(case.lifecycle)),
            ("STUB_RESTORE", Some(case.restore)),
        ];
        let (code, out, err) = shell_run_full(
            dir.path(),
            &env,
            "_base_repo_exists() { return \"$STUB_BASE\"; }\n_overlay_snapshot_installed_links() { return \"$STUB_SNAPSHOT\"; }\n_repo_pull_all() { return \"$STUB_PULL\"; }\ndot_config_load() { return \"$STUB_CONFIG\"; }\n_dot_converge_overlays() { return \"$STUB_CONVERGE\"; }\n_dot_profile_lifecycle_prepare() { return \"$STUB_LIFECYCLE\"; }\n_dot_update_repo_stage_finish() { FINISH_CALLS=\"${FINISH_CALLS:+$FINISH_CALLS,}$1\"; }\n_overlay_restore_installed_links() { RESTORE_CALLS=$((RESTORE_CALLS + 1)); return \"$STUB_RESTORE\"; }\n_ensure_repo_config() { :; }\nDOT_OVERLAY_ROLLBACK_PATHS=(seed); DOT_OVERLAY_ROLLBACK_TARGETS=(seed); DOT_INIT_SKIP_PROVIDER=\"$STUB_SKIP\";\n_dot_update_sync_repos; rc=$?;\nprintf 'rc=%s\\nfinish=%s\\nrestore=%s\\nfrozen=%s\\nprovider=%s\\nrollback=%s\\n' \"$rc\" \"${FINISH_CALLS:-<none>}\" \"${RESTORE_CALLS:-0}\" \"${DOT_OVERLAY_LINKS_FROZEN:-<unset>}\" \"${DOT_DEPENDENCY_PROVIDER:-<unset>}\" \"${#DOT_OVERLAY_ROLLBACK_PATHS[@]}\"",
        );
        assert_eq!(code, 0, "shell harness sync");
        let text = String::from_utf8(out).expect("sync dump");
        let mut lines = text.lines();
        let mut value = || {
            lines
                .next()
                .expect("dump line")
                .split('=')
                .nth(1)
                .expect("dump value")
        };
        let (rc, finish, restore, frozen, provider, rollback) =
            (value(), value(), value(), value(), value(), value());
        let outcome = dot::update::sync_repos_fold(
            &palette,
            &SyncReposInputs {
                base_exists: case.base == "0",
                snapshot_ok: case.snapshot == "0",
                pull_rc: case.pull.parse().expect("stub pull"),
                config_ok: case.config == "0",
                skip_provider: case.skip == "1",
                converge_ok: case.converge == "0",
                lifecycle_ok: case.lifecycle == "0",
                restore_ok: case.restore == "0",
            },
        );
        assert_eq!(rc, outcome.rc.to_string(), "sync rc");
        assert_eq!(
            finish,
            outcome
                .finish_arg
                .map(|arg| arg.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            "sync finish arg"
        );
        assert_eq!(
            restore,
            if outcome.restore_attempted { "1" } else { "0" },
            "sync restore"
        );
        assert_eq!(
            frozen,
            if outcome.frozen { "1" } else { "<unset>" },
            "sync frozen"
        );
        assert_eq!(
            provider,
            if outcome.provider_none {
                "none"
            } else {
                "<unset>"
            },
            "sync provider"
        );
        assert_eq!(rollback, "0", "rollback authority reset");
        assert_eq!(err, outcome.warnings, "sync warnings");
    }
}

/// Finalize matrix row: every stubbed step outcome plus the UI
/// totals. String knobs name the stub env values (`"0"` reads as
/// success); `None` unsets the variable.
struct FinalizeCase {
    arg: &'static str,
    ui_total: Option<&'static str>,
    ui_started: &'static str,
    checkpoint_out: &'static str,
    checkpoint_rc: &'static str,
    frozen: bool,
    link_out: &'static str,
    link_rc: &'static str,
    retire_out: &'static str,
    retire_rc: &'static str,
    provider: Option<&'static str>,
    ensure_out: &'static str,
    ensure_rc: &'static str,
    has_fn: &'static str,
    update_out: &'static str,
    update_rc: &'static str,
    set_status: &'static str,
    shdeps_status: &'static str,
    set_summary: &'static str,
    shdeps_summary: &'static str,
    groups_out: &'static str,
    reexec_out: &'static str,
    reexec_rc: &'static str,
    merges_out: &'static str,
    merges_rc: &'static str,
    commit_out: &'static str,
    commit_rc: &'static str,
    normalize_out: &'static str,
    base: &'static str,
    verbose: Option<&'static str>,
}

/// Non-empty stub outputs mark each callee's row order; empty
/// strings keep the byte comparison focused on the fold itself.
fn finalize_cases() -> Vec<FinalizeCase> {
    vec![
        // Happy shdeps run with a base checkout.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "1",
            update_out: "",
            update_rc: "0",
            set_status: "1",
            shdeps_status: "changed",
            set_summary: "1",
            shdeps_summary: "1 group updated",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Checkpoint refusal closes before anything else runs.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "CKPT",
            checkpoint_rc: "1",
            frozen: false,
            link_out: "LINK",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "1",
            update_out: "",
            update_rc: "0",
            set_status: "1",
            shdeps_status: "changed",
            set_summary: "1",
            shdeps_summary: "s",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Frozen links preserve, then skip the inputs.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: true,
            link_out: "LINK",
            link_rc: "0",
            retire_out: "RET",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "1",
            update_out: "",
            update_rc: "0",
            set_status: "1",
            shdeps_status: "changed",
            set_summary: "1",
            shdeps_summary: "s",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "MERGES",
            merges_rc: "0",
            commit_out: "COMMIT",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Failed link rows still print before the skip.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "LINK",
            link_rc: "1",
            retire_out: "",
            retire_rc: "0",
            provider: Some("none"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: None,
        },
        // Incoming failure skips inputs without retiring.
        FinalizeCase {
            arg: "1",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "LINK",
            link_rc: "0",
            retire_out: "RET",
            retire_rc: "0",
            provider: Some("none"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Retire failure names the deactivation.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "RET",
            retire_rc: "1",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "1",
            update_out: "",
            update_rc: "0",
            set_status: "1",
            shdeps_status: "changed",
            set_summary: "1",
            shdeps_summary: "s",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // No provider, no base: the cleanup takes the spare row.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: None,
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "M",
            merges_rc: "0",
            commit_out: "C",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: None,
        },
        // Broken shdeps install falls back to unavailable.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "ENS",
            ensure_rc: "1",
            has_fn: "1",
            update_out: "",
            update_rc: "0",
            set_status: "1",
            shdeps_status: "changed",
            set_summary: "1",
            shdeps_summary: "s",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: None,
        },
        // Missing update entry point, same fallback.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: None,
        },
        // Failed shdeps update keeps its summaries.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "1",
            update_out: "UPD",
            update_rc: "1",
            set_status: "0",
            shdeps_status: "",
            set_summary: "1",
            shdeps_summary: "net down",
            groups_out: "G1",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Reexec refusal stops after the mid-run done row.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("shdeps"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "1",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "G",
            reexec_out: "RE",
            reexec_rc: "1",
            merges_out: "MERGES",
            merges_rc: "0",
            commit_out: "COMMIT",
            commit_rc: "0",
            normalize_out: "NORM",
            base: "0",
            verbose: None,
        },
        // Failed merges skip the commit attempt.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("none"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "M",
            merges_rc: "1",
            commit_out: "COMMIT",
            commit_rc: "0",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Failed commit warns on stderr.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("none"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "C",
            commit_rc: "1",
            normalize_out: "",
            base: "0",
            verbose: None,
        },
        // Unknown provider names the same fallback.
        FinalizeCase {
            arg: "0",
            ui_total: None,
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("npm"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: None,
        },
        // Kept totals skip the begin; empty totals open one.
        FinalizeCase {
            arg: "0",
            ui_total: Some("7"),
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("none"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: None,
        },
        FinalizeCase {
            arg: "0",
            ui_total: Some(""),
            ui_started: "0",
            checkpoint_out: "",
            checkpoint_rc: "0",
            frozen: false,
            link_out: "",
            link_rc: "0",
            retire_out: "",
            retire_rc: "0",
            provider: Some("none"),
            ensure_out: "",
            ensure_rc: "0",
            has_fn: "0",
            update_out: "",
            update_rc: "0",
            set_status: "0",
            shdeps_status: "",
            set_summary: "0",
            shdeps_summary: "",
            groups_out: "",
            reexec_out: "",
            reexec_rc: "0",
            merges_out: "",
            merges_rc: "0",
            commit_out: "",
            commit_rc: "0",
            normalize_out: "",
            base: "1",
            verbose: Some("1"),
        },
    ]
}

#[test]
fn finalize_fold_agrees() {
    let palette = marker_palette();
    for (index, case) in finalize_cases().iter().enumerate() {
        assert_bytes_stable(
            || {
                let dir = TempDir::new("update-finalize").expect("fixture dir");
                let env: Vec<(&str, Option<&str>)> = vec![
                    ("DOT_QUIET", None),
                    ("DOT_VERBOSE", case.verbose),
                    ("DOT_UI_TOTAL", case.ui_total),
                    ("DOT_UI_STARTED", Some(case.ui_started)),
                    ("DOT_UI_INDEX", Some("0")),
                    ("DOT_UPDATE_RELOADS_SHELL", Some("1")),
                    ("DOT_DEPENDENCY_PROVIDER", case.provider),
                    (
                        "DOT_OVERLAY_LINKS_FROZEN",
                        if case.frozen { Some("1") } else { None },
                    ),
                    ("STUB_ARG", Some(case.arg)),
                    ("STUB_CHECKPOINT_OUT", Some(case.checkpoint_out)),
                    ("STUB_CHECKPOINT_RC", Some(case.checkpoint_rc)),
                    ("STUB_LINK_OUT", Some(case.link_out)),
                    ("STUB_LINK_RC", Some(case.link_rc)),
                    ("STUB_RETIRE_OUT", Some(case.retire_out)),
                    ("STUB_RETIRE_RC", Some(case.retire_rc)),
                    ("STUB_ENSURE_OUT", Some(case.ensure_out)),
                    ("STUB_ENSURE_RC", Some(case.ensure_rc)),
                    ("STUB_HAS_FN", Some(case.has_fn)),
                    ("STUB_UPDATE_OUT", Some(case.update_out)),
                    ("STUB_UPDATE_RC", Some(case.update_rc)),
                    ("STUB_SET_STATUS", Some(case.set_status)),
                    ("STUB_SHDEPS_STATUS", Some(case.shdeps_status)),
                    ("STUB_SET_SUMMARY", Some(case.set_summary)),
                    ("STUB_SHDEPS_SUMMARY", Some(case.shdeps_summary)),
                    ("STUB_GROUPS_OUT", Some(case.groups_out)),
                    ("STUB_REEXEC_OUT", Some(case.reexec_out)),
                    ("STUB_REEXEC_RC", Some(case.reexec_rc)),
                    ("STUB_MERGES_OUT", Some(case.merges_out)),
                    ("STUB_MERGES_RC", Some(case.merges_rc)),
                    ("STUB_COMMIT_OUT", Some(case.commit_out)),
                    ("STUB_COMMIT_RC", Some(case.commit_rc)),
                    ("STUB_NORMALIZE_OUT", Some(case.normalize_out)),
                    ("STUB_BASE", Some(case.base)),
                ];
                let (code, out, err) = shell_run_full(
                    dir.path(),
                    &env,
                    "_dot_provider_consume_checkpoint() { printf '%s' \"$STUB_CHECKPOINT_OUT\"; return \"$STUB_CHECKPOINT_RC\"; }\n_ensure_repo_config() { :; }\n_link_overlays() { printf '%s' \"$STUB_LINK_OUT\"; return \"$STUB_LINK_RC\"; }\n_dot_profile_lifecycle_retire() { printf '%s' \"$STUB_RETIRE_OUT\"; return \"$STUB_RETIRE_RC\"; }\n_dot_profile_lifecycle_commit() { printf '%s' \"$STUB_COMMIT_OUT\"; return \"$STUB_COMMIT_RC\"; }\n_ensure_shdeps() { printf '%s' \"$STUB_ENSURE_OUT\"; return \"$STUB_ENSURE_RC\"; }\n_dot_active_revision() { printf 'rev0'; }\n_run_shdeps_update_ui() { printf '%s' \"$STUB_UPDATE_OUT\"; if [[ \"$STUB_SET_STATUS\" == 1 ]]; then DOT_UI_SHDEPS_STATUS=\"$STUB_SHDEPS_STATUS\"; else unset DOT_UI_SHDEPS_STATUS; fi; if [[ \"$STUB_SET_SUMMARY\" == 1 ]]; then DOT_UI_SHDEPS_SUMMARY=\"$STUB_SHDEPS_SUMMARY\"; else unset DOT_UI_SHDEPS_SUMMARY; fi; return \"$STUB_UPDATE_RC\"; }\n_shdeps_print_group_summaries() { printf '%s' \"$STUB_GROUPS_OUT\"; }\n_dot_provider_maybe_reexec() { printf '%s' \"$STUB_REEXEC_OUT\"; return \"$STUB_REEXEC_RC\"; }\n_run_merges() { printf '%s' \"$STUB_MERGES_OUT\"; return \"$STUB_MERGES_RC\"; }\n_base_repo_exists() { return \"$STUB_BASE\"; }\n_normalize_filtered() { printf '%s' \"$STUB_NORMALIZE_OUT\"; }\nif [[ \"$STUB_HAS_FN\" == 1 ]]; then shdeps_update() { :; }; fi\nDOT_UI_INDEX=0;\n_dot_update_finalize \"$STUB_ARG\"; rc=$?; printf 'rc=%s' \"$rc\"",
                );
                assert_eq!(code, 0, "shell harness finalize {index}");
                let marker = b"rc=";
                let pos = out
                    .windows(marker.len())
                    .rposition(|window| window == marker)
                    .expect("rc dump");
                let (rows, dump) = out.split_at(pos);
                let rc_text = String::from_utf8(dump.to_vec()).expect("rc utf8");
                let shell_rc: i32 = rc_text
                    .strip_prefix("rc=")
                    .expect("rc prefix")
                    .parse()
                    .expect("rc number");
                // An unset stub variable and an empty one both fall
                // back to the shdeps defaults, like `${VAR:-...}`.
                let shdeps_status = if case.set_status == "1" && !case.shdeps_status.is_empty() {
                    Some(case.shdeps_status.as_bytes())
                } else {
                    None
                };
                let shdeps_summary = if case.set_summary == "1" && !case.shdeps_summary.is_empty() {
                    Some(case.shdeps_summary.as_bytes())
                } else {
                    None
                };
                let inputs = FinalizeInputs {
                    update_status: case.arg.parse().expect("case arg"),
                    ui_total: case.ui_total,
                    ui_started: case.ui_started.parse().expect("case started"),
                    checkpoint_output: case.checkpoint_out.as_bytes(),
                    checkpoint_ok: case.checkpoint_rc == "0",
                    links_frozen: case.frozen,
                    link_output: case.link_out.as_bytes(),
                    link_ok: case.link_rc == "0",
                    retire_output: case.retire_out.as_bytes(),
                    retire_ok: case.retire_rc == "0",
                    provider: case.provider,
                    ensure_output: case.ensure_out.as_bytes(),
                    shdeps_ok: case.ensure_rc == "0",
                    shdeps_has_update_fn: case.has_fn == "1",
                    shdeps_update_output: case.update_out.as_bytes(),
                    shdeps_update_ok: case.update_rc == "0",
                    shdeps_status,
                    shdeps_summary,
                    group_summaries: case.groups_out.as_bytes(),
                    reexec_output: case.reexec_out.as_bytes(),
                    reexec_ok: case.reexec_rc == "0",
                    merges_output: case.merges_out.as_bytes(),
                    merges_ok: case.merges_rc == "0",
                    commit_output: case.commit_out.as_bytes(),
                    commit_ok: case.commit_rc == "0",
                    normalize_output: case.normalize_out.as_bytes(),
                    base_exists: case.base == "0",
                    verbose: case.verbose,
                    reload_hint: b"",
                };
                // An unset stub variable and an empty one both fall
                // back to the shdeps defaults, like `${VAR:-...}`.
                let (expected, expected_err, outcome) =
                    dot::update::finalize_fold(&palette, false, false, false, true, &inputs, 0);
                assert_eq!(shell_rc, outcome.rc, "finalize rc {index}");
                assert_eq!(expected_err, err, "finalize stderr {index}");
                let mut rows = rows.to_vec();
                rows.extend_from_slice(format!("rc={shell_rc}").as_bytes());
                let mut want = expected;
                want.extend_from_slice(format!("rc={}", outcome.rc).as_bytes());
                assert_eq!(want, rows, "finalize stdout {index}");
                (want, rows)
            },
            &format!("finalize parity case {index}"),
        );
    }
}
