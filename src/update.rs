//! `dot update` leaf layer from `lib/dot/update.sh`: shdeps job
//! preparation, the skip-inputs stage rows, and the deferred
//! repo-stage finish summary. Orchestrators needing unported layers
//! (pull, profiles, merges) stay in shell until their slices land.

use crate::progress_ui::{Stage, arith_value, count_phrase, join_comma};

/// `_dot_update_prepare_shdeps_jobs`: keep an already-set
/// `SHDEPS_JOBS` (even empty — `[[ -n "${var+x}" ]]`); otherwise the
/// update-job count. Returns the value to export, or `None` to keep.
pub fn prepare_shdeps_jobs(
    shdeps_jobs: Option<&str>,
    dot_update_jobs: Option<&str>,
) -> Option<String> {
    if shdeps_jobs.is_some() {
        return None;
    }
    Some(crate::merges::update_jobs(dot_update_jobs.unwrap_or("")))
}

/// `_dot_update_skip_inputs`: the Tools/Configs warning rows when
/// inputs never became ready.
pub fn skip_inputs(
    stage: &mut Stage,
    reason: &[u8],
    now_secs: i64,
    verbose: Option<&str>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&stage.start(
        b"Tools",
        Some(b"skipping configured dependencies".as_slice()),
        now_secs,
        verbose,
    ));
    let mut detail = reason.to_vec();
    detail.extend_from_slice(b"; dependencies skipped");
    out.extend_from_slice(&stage.finish(b"warning", &detail, now_secs));
    out.extend_from_slice(&stage.start(
        b"Configs",
        Some(b"skipping config hooks".as_slice()),
        now_secs,
        verbose,
    ));
    let mut detail = reason.to_vec();
    detail.extend_from_slice(b"; config hooks skipped");
    out.extend_from_slice(&stage.finish(b"warning", &detail, now_secs));
    out
}

/// Inputs for [`repo_stage_finish`]: raw `${VAR:-}` spellings, where
/// `None` reads as unset (zero for arithmetic, like the shell
/// defaults). `deferred_active` mirrors the exact
/// `DOT_REPO_STAGE_DEFERRED_ACTIVE == 1` string check.
pub struct RepoStageFinish<'a> {
    /// `DOT_REPO_STAGE_DEFERRED_ACTIVE == 1`.
    pub deferred_active: bool,
    /// `$1`, defaulting to `0`.
    pub forced_failure: Option<&'a str>,
    /// `DOT_REPO_AGG_CURRENT`, defaulting to `0`.
    pub agg_current: Option<&'a str>,
    /// `DOT_REPO_AGG_CHANGED`, defaulting to `0`.
    pub agg_changed: Option<&'a str>,
    /// `DOT_REPO_AGG_FAILED`, defaulting to `0`.
    pub agg_failed: Option<&'a str>,
    /// `DOT_REPO_AGG_SKIPPED`, defaulting to `0`.
    pub agg_skipped: Option<&'a str>,
    /// `DOT_REPO_AGG_CHANGED_ITEMS`, newline-separated.
    pub changed_items: &'a [u8],
    /// `DOT_VERBOSE`, defaulting to `0`.
    pub verbose: Option<&'a str>,
}

/// `_dot_update_repo_stage_finish`: close the deferred repo stage
/// with the aggregated status and comma-joined summary, then note
/// each changed item for non-verbose callers. Silent without
/// deferral. The shell's trailing `unset` of the aggregation globals
/// has no Rust equivalent: nothing owns globals here.
pub fn repo_stage_finish(
    stage: &mut Stage,
    inputs: &RepoStageFinish<'_>,
    now_secs: i64,
) -> Vec<u8> {
    if !inputs.deferred_active {
        return Vec::new();
    }
    // Unset and malformed counts read as zero, like the shell
    // arithmetic defaults; producers always emit canonical decimals.
    let num = |value: Option<&str>| value.and_then(arith_value).unwrap_or(0);
    let forced = arith_value(inputs.forced_failure.unwrap_or("0")) == Some(1);
    let current = num(inputs.agg_current);
    let changed = num(inputs.agg_changed);
    let failed = num(inputs.agg_failed);
    let skipped = num(inputs.agg_skipped);
    let status = if forced || failed > 0 {
        b"failed".as_slice()
    } else if changed > 0 {
        b"changed".as_slice()
    } else {
        b"ok".as_slice()
    };
    // Each part is the count phrase plus its state word, like
    // `"$(...) changed"` in the shell.
    let part = |count: i64, state: &[u8]| {
        let mut out = count_phrase(count, b"repo", Some(b"repos".as_slice()));
        out.push(b' ');
        out.extend_from_slice(state);
        out
    };
    let mut parts: Vec<Vec<u8>> = Vec::new();
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
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let summary = join_comma(&refs);
    let mut out = stage.finish(status, &summary, now_secs);
    if arith_value(inputs.verbose.unwrap_or("0")) == Some(0) {
        for item in inputs.changed_items.split(|byte| *byte == b'\n') {
            if item.is_empty() {
                continue;
            }
            out.extend_from_slice(&stage.note(b"changed", item));
        }
    }
    out
}
