//! Small shared kernels for the native `dot update` command.
//!
//! The end-to-end state machine lives in [`crate::update_engine`]. This module
//! contains only behavior reused at another boundary: leading flag parsing,
//! the overlay-phase verdict, and deferred repository-stage rendering.

use crate::progress_ui::{Stage, arith_value, count_phrase, join_comma};

/// Inputs for closing the deferred repository stage.
pub struct RepoStageFinish<'a> {
    /// Whether the repository stage was opened for deferred rendering.
    pub deferred_active: bool,
    /// Explicit coordinator failure flag.
    pub forced_failure: Option<&'a str>,
    /// Number of repositories already current.
    pub agg_current: Option<&'a str>,
    /// Number of repositories changed.
    pub agg_changed: Option<&'a str>,
    /// Number of repositories that failed.
    pub agg_failed: Option<&'a str>,
    /// Number of repositories skipped.
    pub agg_skipped: Option<&'a str>,
    /// Newline-delimited labels for changed repositories.
    pub changed_items: &'a [u8],
    /// Verbose flag; verbose output already reported individual changes.
    pub verbose: Option<&'a str>,
}

/// Close a deferred repository stage with its aggregate status and summary.
pub fn repo_stage_finish(
    stage: &mut Stage,
    inputs: &RepoStageFinish<'_>,
    now_secs: i64,
) -> Vec<u8> {
    if !inputs.deferred_active {
        return Vec::new();
    }
    let number = |value: Option<&str>| value.and_then(arith_value).unwrap_or(0);
    let forced = arith_value(inputs.forced_failure.unwrap_or("0")) == Some(1);
    let current = number(inputs.agg_current);
    let changed = number(inputs.agg_changed);
    let failed = number(inputs.agg_failed);
    let skipped = number(inputs.agg_skipped);
    let status = if forced || failed > 0 {
        b"failed".as_slice()
    } else if changed > 0 {
        b"changed".as_slice()
    } else {
        b"ok".as_slice()
    };
    let part = |count: i64, state: &[u8]| {
        let mut text = count_phrase(count, b"repo", Some(b"repos"));
        text.push(b' ');
        text.extend_from_slice(state);
        text
    };
    let mut parts = Vec::new();
    if changed != 0 {
        parts.push(part(changed, b"changed"));
    }
    if current != 0 || (failed == 0 && skipped == 0) {
        parts.push(part(current, b"current"));
    }
    if failed != 0 {
        parts.push(part(failed, b"failed"));
    }
    if skipped != 0 {
        parts.push(part(skipped, b"skipped"));
    }
    let fields: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let mut output = stage.finish(status, &join_comma(&fields), now_secs);
    if arith_value(inputs.verbose.unwrap_or("0")) == Some(0) {
        for item in inputs.changed_items.split(|byte| *byte == b'\n') {
            if !item.is_empty() {
                output.extend_from_slice(&stage.note(b"changed", item));
            }
        }
    }
    output
}

/// Whether one native overlay pull phase completed without failed entries.
pub fn overlay_phase_ok(pull_rc: i32, overlay_failed: Option<&str>) -> bool {
    pull_rc == 0 && overlay_failed.and_then(arith_value).unwrap_or(0) == 0
}

/// Parsed leading update flags and the first unconsumed argument index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateFlagParse {
    /// Whether `--cron` was consumed.
    pub cron_mode: bool,
    /// Whether quiet output was requested or implied by cron mode.
    pub quiet: bool,
    /// Whether forced synchronization was requested.
    pub force: bool,
    /// Whether verbose output was requested.
    pub verbose: bool,
    /// Number of leading option words consumed.
    pub consumed: usize,
}

/// Parse the exact leading flags accepted by `dot update`.
pub fn parse_update_flags(args: &[&[u8]]) -> UpdateFlagParse {
    let mut parsed = UpdateFlagParse {
        cron_mode: false,
        quiet: false,
        force: false,
        verbose: false,
        consumed: 0,
    };
    while let Some(arg) = args.get(parsed.consumed) {
        match *arg {
            b"--cron" => {
                parsed.cron_mode = true;
                parsed.quiet = true;
            }
            b"--quiet" => parsed.quiet = true,
            b"-f" | b"--force" => parsed.force = true,
            b"-v" | b"--verbose" => parsed.verbose = true,
            _ => break,
        }
        parsed.consumed += 1;
    }
    parsed
}
