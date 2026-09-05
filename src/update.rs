//! `dot update` leaf layer from `lib/dot/update.sh`: shdeps job
//! preparation, the skip-inputs stage rows, the deferred repo-stage
//! finish summary, the no-base pull and pull-overlay-phase kernels,
//! and the control-flow folds for converge, sync, and finalize.
//! Step execution (pull, profiles, merges, lifecycle) stays in shell
//! until its slices land: the folds take each step's observed
//! outcome as data and pin the order, branches, and statuses.
//! `_dot_update` sequencing (flag parsing, the cron dirty gate, and
//! the sync/reload/finalize order) lives here as data kernels; only
//! step execution and process exit stay with the caller — the cron
//! `exit 0` arrives as a [`CronGate::ExitSilent`] decision.

use crate::progress_ui::{
    Palette, Stage, arith_value, count_phrase, done, join_comma, progress_detail, warn_line,
};

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

/// `_dot_update_no_base_pull`: the update path without a base
/// checkout. Emits the `Repos` checking row, then closes it from
/// the pull outcome: `failed` unless the overlay failure count
/// reads zero, with `, {reply}` appended while the pull left one.
/// Returns the rows plus whether the caller continues (`true`
/// exactly when the shell's final `[[ ... -eq 0 ]]` passes).
/// `_ensure_repo_config` and `_pull_overlays` run in the caller.
pub fn no_base_pull(
    stage: &mut Stage,
    reply: Option<&[u8]>,
    overlay_failed: Option<&str>,
    now_secs: i64,
    verbose: Option<&str>,
) -> (Vec<u8>, bool) {
    let mut out = stage.start(
        b"Repos",
        Some(b"checking repositories".as_slice()),
        now_secs,
        verbose,
    );
    // Unset reads as zero, like `${DOT_PULL_OVERLAY_FAILED:-0}`;
    // producers always emit canonical decimals.
    let failed = overlay_failed.and_then(arith_value).unwrap_or(0);
    let status = if failed == 0 {
        b"ok".as_slice()
    } else {
        b"failed".as_slice()
    };
    let mut detail = b"no base repo".to_vec();
    if let Some(reply) = reply {
        if !reply.is_empty() {
            detail.extend_from_slice(b", ");
            detail.extend_from_slice(reply);
        }
    }
    out.extend_from_slice(&stage.finish(status, &detail, now_secs));
    (out, failed == 0)
}

/// Inputs for [`pull_overlay_phase`]: raw `${VAR:-}` spellings,
/// where `None` reads as unset. The pull step itself runs in the
/// caller; only its observed outcome enters here.
pub struct PullOverlayPhase<'a> {
    /// `DOT_REPO_STAGE_DEFERRED_ACTIVE == 1`.
    pub deferred_active: bool,
    /// The phase label (`overlays`, `phase-one`, `selected`).
    pub label: &'a [u8],
    /// `_pull_overlay_count` for this phase.
    pub count: i64,
    /// `DOT_REPO_PROGRESS_DONE`; unset or empty reads as `1`.
    pub done: Option<&'a str>,
    /// `DOT_VERBOSE`.
    pub verbose: Option<&'a str>,
    /// Progress-bar width (`DOT_UI_PROGRESS_WIDTH`, default `8`).
    pub bar_width: &'a str,
    /// ASCII bars (`DOT_UI_ASCII`, or a `C`/`POSIX` locale).
    pub ascii: bool,
    /// Multibyte cells for the label.
    pub multibyte: bool,
    /// The pull step's exit status.
    pub pull_rc: i32,
    /// `DOT_PULL_OVERLAY_CURRENT`, defaulting to `0`.
    pub pull_current: Option<&'a str>,
    /// `DOT_PULL_OVERLAY_CHANGED`, defaulting to `0`.
    pub pull_changed: Option<&'a str>,
    /// `DOT_PULL_OVERLAY_FAILED`, defaulting to `0`.
    pub pull_failed: Option<&'a str>,
    /// `DOT_PULL_OVERLAY_SKIPPED`, defaulting to `0`.
    pub pull_skipped: Option<&'a str>,
    /// `DOT_PULL_OVERLAY_CHANGED_ITEMS`.
    pub pull_changed_items: &'a [u8],
    /// Pre-pull `DOT_REPO_AGG_CURRENT`, defaulting to `0`.
    pub agg_current: Option<&'a str>,
    /// Pre-pull `DOT_REPO_AGG_CHANGED`, defaulting to `0`.
    pub agg_changed: Option<&'a str>,
    /// Pre-pull `DOT_REPO_AGG_FAILED`, defaulting to `0`.
    pub agg_failed: Option<&'a str>,
    /// Pre-pull `DOT_REPO_AGG_SKIPPED`, defaulting to `0`.
    pub agg_skipped: Option<&'a str>,
    /// Pre-pull `DOT_REPO_AGG_CHANGED_ITEMS`.
    pub agg_changed_items: &'a [u8],
}

/// Outcome of [`pull_overlay_phase`].
pub enum PullPhaseOutcome {
    /// Deferred staging idle: the pull ran unwrapped and the
    /// caller returns this status directly.
    Passthrough {
        /// The pull step's exit status.
        rc: i32,
    },
    /// Deferred staging active: the progress row plus the folded
    /// aggregation globals.
    Aggregated {
        /// The `_ui_stage_update` row (empty without progress).
        progress: Vec<u8>,
        /// `DOT_REPO_PROGRESS_DONE` after defaulting.
        done: i64,
        /// `DOT_REPO_PROGRESS_TOTAL`.
        total: i64,
        /// Folded `DOT_REPO_AGG_CURRENT`.
        current: i64,
        /// Folded `DOT_REPO_AGG_CHANGED`.
        changed: i64,
        /// Folded `DOT_REPO_AGG_FAILED`.
        failed: i64,
        /// Folded `DOT_REPO_AGG_SKIPPED`.
        skipped: i64,
        /// Folded `DOT_REPO_AGG_CHANGED_ITEMS`.
        changed_items: Vec<u8>,
        /// `[[ $rc -eq 0 && ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]]`.
        success: bool,
    },
}

/// `_dot_update_pull_overlay_phase`: run one overlay pull phase.
/// Idle staging passes the pull status straight through; active
/// staging advances the shared progress bar and folds the pull
/// counters into the deferred aggregates.
pub fn pull_overlay_phase(
    stage: &mut Stage,
    inputs: &PullOverlayPhase<'_>,
    now_secs: i64,
) -> PullPhaseOutcome {
    if !inputs.deferred_active {
        return PullPhaseOutcome::Passthrough { rc: inputs.pull_rc };
    }
    // `${DOT_REPO_PROGRESS_DONE:-1}`: unset or empty defaults.
    let done = match inputs.done {
        None | Some("") => 1,
        Some(value) => arith_value(value).unwrap_or(0),
    };
    let total = done + inputs.count;
    let mut progress = Vec::new();
    if inputs.count > 0 {
        let detail = progress_detail(
            inputs.label,
            done + 1,
            total,
            inputs.bar_width,
            inputs.ascii,
            inputs.multibyte,
        );
        progress.extend_from_slice(&stage.update(&detail, now_secs, inputs.verbose));
    }
    // Unset and malformed counts read as zero, like the shell
    // arithmetic defaults; producers always emit canonical decimals.
    let num = |value: Option<&str>| value.and_then(arith_value).unwrap_or(0);
    let pull_failed = num(inputs.pull_failed);
    let mut changed_items = inputs.agg_changed_items.to_vec();
    changed_items.extend_from_slice(inputs.pull_changed_items);
    PullPhaseOutcome::Aggregated {
        progress,
        done,
        total,
        current: num(inputs.agg_current) + num(inputs.pull_current),
        changed: num(inputs.agg_changed) + num(inputs.pull_changed),
        failed: num(inputs.agg_failed) + pull_failed,
        skipped: num(inputs.agg_skipped) + num(inputs.pull_skipped),
        changed_items,
        success: inputs.pull_rc == 0 && pull_failed == 0,
    }
}

/// `_dot_converge_overlays` phase-two selection: the eligible
/// entries whose `name|...` head never appeared in the phase-one
/// set. The discovery, extension, and pull steps run in the
/// caller; this pins the set difference between the phases.
pub fn converge_additions<'a>(phase_one: &[&'a str], eligible: &[&'a str]) -> Vec<&'a str> {
    eligible
        .iter()
        .filter(|entry| {
            let want = converge_head(entry);
            !phase_one.iter().any(|seen| converge_head(seen) == want)
        })
        .copied()
        .collect()
}

/// `${entry%%|*}`: the overlay name head, or the whole entry when
/// it holds no bar.
fn converge_head(entry: &str) -> &str {
    entry.split('|').next().unwrap_or(entry)
}

/// One pull phase's contribution to the converge status:
/// `[[ $rc -eq 0 && ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]]`.
/// Covers the `overlays`, `phase-one`, and `selected` phases;
/// unset failure counts read as zero.
pub fn overlay_phase_ok(pull_rc: i32, overlay_failed: Option<&str>) -> bool {
    pull_rc == 0 && overlay_failed.and_then(arith_value).unwrap_or(0) == 0
}

/// `_dot_converge_overlays` final fold:
/// `[[ $phase_status -eq 0 && $final_status -eq 0 ]]`.
pub fn converge_status(phase_ok: bool, final_ok: bool) -> bool {
    phase_ok && final_ok
}

/// Inputs for [`sync_repos_fold`]: the observed outcome of each
/// `_dot_update_sync_repos` step, in call order. Step execution
/// stays in shell until its slices land.
pub struct SyncReposInputs {
    /// `_base_repo_exists`.
    pub base_exists: bool,
    /// `_overlay_snapshot_installed_links` (base path only).
    pub snapshot_ok: bool,
    /// `_repo_pull_all` exit status (base path only).
    pub pull_rc: i32,
    /// `dot_config_load`.
    pub config_ok: bool,
    /// `DOT_INIT_SKIP_PROVIDER == 1`.
    pub skip_provider: bool,
    /// `_dot_converge_overlays`.
    pub converge_ok: bool,
    /// `_dot_profile_lifecycle_prepare`.
    pub lifecycle_ok: bool,
    /// `_overlay_restore_installed_links`, when attempted.
    pub restore_ok: bool,
}

/// Outcome of [`sync_repos_fold`].
pub struct SyncReposOutcome {
    /// The function exit status.
    pub rc: i32,
    /// Argument for the closing `_dot_update_repo_stage_finish`;
    /// `None` when the snapshot failure returns before it.
    pub finish_arg: Option<i32>,
    /// Whether the previous overlay-link generation was restored.
    pub restore_attempted: bool,
    /// `DOT_OVERLAY_LINKS_FROZEN=1` on the way out.
    pub frozen: bool,
    /// `DOT_DEPENDENCY_PROVIDER=none` was exported.
    pub provider_none: bool,
    /// The stderr warnings (the restore warning when a restore
    /// was attempted and failed).
    pub warnings: Vec<u8>,
}

/// `_dot_update_sync_repos` control flow: reset the rollback
/// authority, pull the base generation, reload policy, converge
/// the overlays, and prepare the profile lifecycle. Every failure
/// closes the deferred repo stage as failed, restores the
/// previous link generation, and freezes overlay linking.
pub fn sync_repos_fold(palette: &Palette, inputs: &SyncReposInputs) -> SyncReposOutcome {
    // A failed restore warns exactly like the shell's `_warn`
    // after `_overlay_restore_installed_links`.
    let restore_warnings = || {
        if inputs.restore_ok {
            Vec::new()
        } else {
            warn_line(
                palette,
                b"  warning: could not restore the previous overlay-link generation",
            )
        }
    };
    // The provider export runs only after the config reload, so
    // the pull and config failures leave the provider alone.
    let early = |rc: i32| SyncReposOutcome {
        rc,
        finish_arg: Some(1),
        restore_attempted: true,
        frozen: true,
        provider_none: false,
        warnings: restore_warnings(),
    };
    let failed = |rc: i32, restore_attempted: bool| SyncReposOutcome {
        rc,
        finish_arg: Some(1),
        restore_attempted,
        frozen: true,
        provider_none: inputs.skip_provider,
        warnings: if restore_attempted {
            restore_warnings()
        } else {
            Vec::new()
        },
    };
    if inputs.base_exists && !inputs.snapshot_ok {
        return SyncReposOutcome {
            rc: 1,
            finish_arg: None,
            restore_attempted: false,
            frozen: true,
            provider_none: false,
            warnings: Vec::new(),
        };
    }
    // The no-base path only ensures repo configuration, which
    // cannot fail; the base path carries the pull status forward.
    if inputs.base_exists && inputs.pull_rc != 0 {
        return early(inputs.pull_rc);
    }
    if !inputs.config_ok {
        return early(1);
    }
    if !inputs.converge_ok {
        return failed(1, true);
    }
    if !inputs.lifecycle_ok {
        // The lifecycle failure restores only with a base
        // checkout behind it.
        return failed(1, inputs.base_exists);
    }
    SyncReposOutcome {
        rc: 0,
        finish_arg: Some(0),
        restore_attempted: false,
        frozen: false,
        provider_none: inputs.skip_provider,
        warnings: Vec::new(),
    }
}

/// Inputs for [`finalize_fold`]: the incoming status plus the
/// observed outcome of each `_dot_update_finalize` step, in call
/// order. Step execution stays in shell until its slices land;
/// callee output the shell would print (link rows, merge rows,
/// group summaries) enters as bytes so the fold pins the row
/// order. Statuses are canonical decimals, like every producer
/// emits.
pub struct FinalizeInputs<'a> {
    /// `$1`, defaulting to `0`.
    pub update_status: i32,
    /// `DOT_UI_TOTAL`; unset or non-positive opens `_ui_begin 4`.
    pub ui_total: Option<&'a str>,
    /// `DOT_UI_STARTED` when no begin runs.
    pub ui_started: i64,
    /// `_dot_provider_consume_checkpoint` rows.
    pub checkpoint_output: &'a [u8],
    /// `_dot_provider_consume_checkpoint`.
    pub checkpoint_ok: bool,
    /// `DOT_OVERLAY_LINKS_FROZEN == 1`.
    pub links_frozen: bool,
    /// `_link_overlays` rows (unfrozen path only).
    pub link_output: &'a [u8],
    /// `_link_overlays`.
    pub link_ok: bool,
    /// `_dot_profile_lifecycle_retire` rows.
    pub retire_output: &'a [u8],
    /// `_dot_profile_lifecycle_retire`.
    pub retire_ok: bool,
    /// `DOT_DEPENDENCY_PROVIDER`; unset reads as `none`.
    pub provider: Option<&'a str>,
    /// `_ensure_shdeps` rows.
    pub ensure_output: &'a [u8],
    /// `_ensure_shdeps`.
    pub shdeps_ok: bool,
    /// `declare -f shdeps_update`.
    pub shdeps_has_update_fn: bool,
    /// `_run_shdeps_update_ui` rows.
    pub shdeps_update_output: &'a [u8],
    /// `_run_shdeps_update_ui`.
    pub shdeps_update_ok: bool,
    /// `DOT_UI_SHDEPS_STATUS`; unset or empty reads as `ok`.
    pub shdeps_status: Option<&'a [u8]>,
    /// `DOT_UI_SHDEPS_SUMMARY`; unset or empty reads as
    /// `dependencies checked` (ok) or `dependency update failed`.
    pub shdeps_summary: Option<&'a [u8]>,
    /// `_shdeps_print_group_summaries` rows (shdeps-update paths).
    pub group_summaries: &'a [u8],
    /// `_dot_provider_maybe_reexec` rows.
    pub reexec_output: &'a [u8],
    /// `_dot_provider_maybe_reexec`.
    pub reexec_ok: bool,
    /// `_run_merges` rows.
    pub merges_output: &'a [u8],
    /// `_run_merges`.
    pub merges_ok: bool,
    /// `_dot_profile_lifecycle_commit` rows.
    pub commit_output: &'a [u8],
    /// `_dot_profile_lifecycle_commit`.
    pub commit_ok: bool,
    /// `_normalize_filtered` rows (base path only).
    pub normalize_output: &'a [u8],
    /// `_base_repo_exists`.
    pub base_exists: bool,
    /// `DOT_VERBOSE`.
    pub verbose: Option<&'a str>,
    /// `_ui_shell_reload_hint` text for the closing `_ui_done`.
    pub reload_hint: &'a [u8],
}

/// Outcome of [`finalize_fold`].
pub struct FinalizeOutcome {
    /// The function exit status.
    pub rc: i32,
    /// The provider reexec refused: output stops after `_ui_done 1`.
    pub reexec_failed: bool,
}

/// `_dot_update_finalize` control flow: open the four-stage run,
/// link the overlays (or preserve them when frozen), retire the
/// profile, refresh dependencies, run the merges, commit the
/// lifecycle, normalize the worktree, and close with `_ui_done`.
/// Returns the stdout rows, the stderr warnings, and the outcome.
pub fn finalize_fold(
    palette: &Palette,
    quiet: bool,
    live: bool,
    multibyte: bool,
    ascii: bool,
    inputs: &FinalizeInputs<'_>,
    now_secs: i64,
) -> (Vec<u8>, Vec<u8>, FinalizeOutcome) {
    // `[[ "${DOT_UI_TOTAL:-0}" -le 0 ]]`: unset, empty, and
    // non-positive totals open a fresh four-stage run; malformed
    // arithmetic keeps the caller's totals, like the shell error.
    let begin = match inputs.ui_total {
        None | Some("") => true,
        Some(text) => arith_value(text).is_some_and(|value| value <= 0),
    };
    // A kept total interpolates literally into the stage header,
    // like `DOT_UI_TOTAL` itself.
    let total = if begin {
        "4".to_string()
    } else {
        inputs.ui_total.unwrap_or("0").to_string()
    };
    let started = if begin { now_secs } else { inputs.ui_started };
    let mut stage = Stage::begin(palette.clone(), &total, quiet, live, multibyte, ascii);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let done_row = |status: &str| {
        done(
            palette,
            quiet,
            Some(status),
            started,
            now_secs,
            inputs.reload_hint,
        )
    };
    let mut update_status = inputs.update_status;
    let mut inputs_ready = update_status == 0;
    out.extend_from_slice(inputs.checkpoint_output);
    if !inputs.checkpoint_ok {
        out.extend_from_slice(&done_row("1"));
        return (
            out,
            err,
            FinalizeOutcome {
                rc: 1,
                reexec_failed: false,
            },
        );
    }
    if inputs.links_frozen {
        out.extend_from_slice(&stage.start(
            b"Overlays",
            Some(b"preserving installed overlay links".as_slice()),
            now_secs,
            inputs.verbose,
        ));
        out.extend_from_slice(&stage.finish(
            b"warning",
            b"profile resolution or repository sync failed",
            now_secs,
        ));
        update_status = 1;
        inputs_ready = false;
    } else {
        out.extend_from_slice(inputs.link_output);
        if !inputs.link_ok {
            update_status = 1;
            inputs_ready = false;
        }
    }
    if !inputs_ready {
        out.extend_from_slice(&skip_inputs(
            &mut stage,
            b"repository synchronization failed",
            now_secs,
            inputs.verbose,
        ));
    } else {
        // The retire rows print before the status branch, like any
        // step whose output lands before its `if ! ...` test.
        out.extend_from_slice(inputs.retire_output);
        if !inputs.retire_ok {
            update_status = 1;
            inputs_ready = false;
            out.extend_from_slice(&skip_inputs(
                &mut stage,
                b"profile deactivation failed",
                now_secs,
                inputs.verbose,
            ));
        } else {
            let reexec_refused = tools_stage(
                &mut stage,
                palette,
                quiet,
                &mut out,
                &mut update_status,
                inputs,
                started,
                now_secs,
            );
            // The reexec refusal returns through `_ui_done 1`
            // before merges, commit, and cleanup run.
            if reexec_refused {
                return (
                    out,
                    err,
                    FinalizeOutcome {
                        rc: 1,
                        reexec_failed: true,
                    },
                );
            }
            out.extend_from_slice(inputs.merges_output);
            if !inputs.merges_ok {
                update_status = 1;
            }
        }
    }
    if inputs_ready && update_status == 0 {
        // The commit rows print only on the attempt, which needs
        // ready inputs and a clean status.
        out.extend_from_slice(inputs.commit_output);
        if !inputs.commit_ok {
            err.extend_from_slice(&warn_line(
                palette,
                b"  warning: could not commit profile lifecycle state",
            ));
            update_status = 1;
        }
    }
    // `[[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]` on the effective total;
    // malformed arithmetic stays silent, like the shell error.
    let cleanup_total = if begin {
        4
    } else {
        inputs.ui_total.and_then(arith_value).unwrap_or(0)
    };
    if inputs.base_exists {
        out.extend_from_slice(&stage.start(
            b"Cleanup",
            Some(b"normalizing worktree".as_slice()),
            now_secs,
            inputs.verbose,
        ));
        out.extend_from_slice(inputs.normalize_output);
        out.extend_from_slice(&stage.finish(b"ok", b"worktree normalized", now_secs));
    } else if cleanup_total > 0 {
        out.extend_from_slice(&stage.start(
            b"Cleanup",
            Some(b"normalizing worktree".as_slice()),
            now_secs,
            inputs.verbose,
        ));
        out.extend_from_slice(&stage.finish(b"ok", b"no base repo", now_secs));
    }
    let status = update_status.to_string();
    out.extend_from_slice(&done_row(&status));
    (
        out,
        err,
        FinalizeOutcome {
            rc: update_status,
            reexec_failed: false,
        },
    )
}

/// The Tools stage of [`finalize_fold`]: provider dispatch over
/// the observed shdeps outcomes. Returns whether the provider
/// reexec refused, cutting the run short after the mid-run
/// `_ui_done 1`.
#[allow(clippy::too_many_arguments)]
fn tools_stage(
    stage: &mut Stage,
    palette: &Palette,
    quiet: bool,
    out: &mut Vec<u8>,
    update_status: &mut i32,
    inputs: &FinalizeInputs<'_>,
    started: i64,
    now_secs: i64,
) -> bool {
    match inputs.provider.unwrap_or("none") {
        "none" => {
            out.extend_from_slice(&stage.start(
                b"Tools",
                Some(b"checking configured dependencies".as_slice()),
                now_secs,
                inputs.verbose,
            ));
            out.extend_from_slice(&stage.finish(b"ok", b"no dependency provider", now_secs));
        }
        "shdeps" => {
            out.extend_from_slice(inputs.ensure_output);
            if !inputs.shdeps_ok {
                *update_status = 1;
            }
            if inputs.shdeps_ok && inputs.shdeps_has_update_fn {
                out.extend_from_slice(&stage.start(
                    b"Tools",
                    Some(b"checking configured dependencies".as_slice()),
                    now_secs,
                    inputs.verbose,
                ));
                out.extend_from_slice(inputs.shdeps_update_output);
                if inputs.shdeps_update_ok {
                    out.extend_from_slice(&stage.finish(
                        shdeps_or(inputs.shdeps_status, b"ok"),
                        shdeps_or(inputs.shdeps_summary, b"dependencies checked"),
                        now_secs,
                    ));
                    out.extend_from_slice(inputs.group_summaries);
                    out.extend_from_slice(inputs.reexec_output);
                    if !inputs.reexec_ok {
                        out.extend_from_slice(&done(
                            palette,
                            quiet,
                            Some("1"),
                            started,
                            now_secs,
                            inputs.reload_hint,
                        ));
                        return true;
                    }
                } else {
                    *update_status = 1;
                    out.extend_from_slice(&stage.finish(
                        b"failed",
                        shdeps_or(inputs.shdeps_summary, b"dependency update failed"),
                        now_secs,
                    ));
                    out.extend_from_slice(inputs.group_summaries);
                }
            } else {
                *update_status = 1;
                tools_unavailable(stage, out, inputs.verbose, now_secs);
            }
        }
        _ => {
            *update_status = 1;
            tools_unavailable(stage, out, inputs.verbose, now_secs);
        }
    }
    false
}

/// The Tools fallback row when no usable shdeps update exists.
fn tools_unavailable(stage: &mut Stage, out: &mut Vec<u8>, verbose: Option<&str>, now_secs: i64) {
    out.extend_from_slice(&stage.start(
        b"Tools",
        Some(b"checking configured dependencies".as_slice()),
        now_secs,
        verbose,
    ));
    out.extend_from_slice(&stage.finish(
        b"failed",
        b"shdeps unavailable; dependency install skipped",
        now_secs,
    ));
}

/// `${VAR:-default}`: unset or empty falls back.
fn shdeps_or<'a>(value: Option<&'a [u8]>, fallback: &'a [u8]) -> &'a [u8] {
    match value {
        Some(text) if !text.is_empty() => text,
        _ => fallback,
    }
}

/// Leading-flag parse of `_dot_update`: the
/// `while [[ "${1:-}" == -* ]]` loop consumes `--cron`, `--quiet`,
/// `-f`/`--force`, and `-v`/`--verbose` exactly; anything else —
/// including `-`, `--`, and `--flag=value` spellings — stops the
/// loop with the residue forwarded to the repo sync. Callers apply
/// the exports (`DOT_QUIET`/`SHDEPS_QUIET`, `DOT_FORCE`/
/// `SHDEPS_FORCE`, `DOT_VERBOSE`/`SHDEPS_LOG_LEVEL=2`) and unset
/// `DOT_OVERLAY_LINKS_FROZEN` on entry, like the shell.
pub struct UpdateFlagParse {
    /// `--cron` was seen (implies quiet on both providers).
    pub cron_mode: bool,
    /// `--cron` or `--quiet` was seen.
    pub quiet: bool,
    /// `-f` or `--force` was seen.
    pub force: bool,
    /// `-v` or `--verbose` was seen.
    pub verbose: bool,
    /// Leading arguments consumed; the rest forwards to sync.
    pub consumed: usize,
}

/// Parse the leading `_dot_update` flags over raw argv bytes (never
/// decoded: names are pure ASCII, like the shell `case`).
pub fn parse_update_flags(args: &[&[u8]]) -> UpdateFlagParse {
    let mut parsed = UpdateFlagParse {
        cron_mode: false,
        quiet: false,
        force: false,
        verbose: false,
        consumed: 0,
    };
    while let Some(arg) = args.get(parsed.consumed) {
        if arg.is_empty() || arg[0] != b'-' {
            break;
        }
        match *arg {
            b"--cron" => {
                parsed.cron_mode = true;
                parsed.quiet = true;
                parsed.consumed += 1;
            }
            b"--quiet" => {
                parsed.quiet = true;
                parsed.consumed += 1;
            }
            b"-f" | b"--force" => {
                parsed.force = true;
                parsed.consumed += 1;
            }
            b"-v" | b"--verbose" => {
                parsed.verbose = true;
                parsed.consumed += 1;
            }
            _ => break,
        }
    }
    parsed
}

/// `_dot_update` cron dirty gate: cron runs must never fight active
/// local edits, so a dirty tree the resolver cannot clean ends the
/// run silently. The shell spells that `exit 0`; the kernel returns
/// a decision and the caller owns the exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronGate {
    /// The run ends silently with status `0` before repo sync.
    ExitSilent,
    /// The run continues into repo sync.
    Proceed,
}

/// Decide the cron dirty gate from the observed step outcomes:
/// `cron_mode` is the parsed `--cron` flag, `dirty` is
/// `_is_worktree_dirty`, and `resolved` is `_try_resolve_dirty`.
pub fn cron_gate(cron_mode: bool, dirty: bool, resolved: bool) -> CronGate {
    if cron_mode && dirty && !resolved {
        CronGate::ExitSilent
    } else {
        CronGate::Proceed
    }
}

/// Inputs for [`sequence_update`]: the observed outcome of each
/// `_dot_update` step after flag parsing and the cron gate, in call
/// order. Step execution stays in shell until its slices land.
pub struct SequenceInputs {
    /// `_dot_update_sync_repos`.
    pub sync_ok: bool,
    /// The defensive `dot_config_load` (sync-ok path only).
    pub config_ok: bool,
    /// `DOT_INIT_SKIP_PROVIDER == 1`.
    pub skip_provider: bool,
    /// `_dot_update_finalize` exit status (finalize paths only).
    pub finalize_rc: i32,
}

/// Outcome of [`sequence_update`].
pub struct SequenceOutcome {
    /// The function exit status.
    pub rc: i32,
    /// `DOT_DEPENDENCY_PROVIDER=none` was exported (sync-ok plus
    /// reloaded config plus the provider skip).
    pub provider_none: bool,
    /// Argument for `_dot_update_finalize`; `None` when the failed
    /// reload closes via `_ui_done 1` before finalizing.
    pub finalize_arg: Option<i32>,
}

/// `_dot_update` sequencing after the cron gate: run the repo sync,
/// defensively reload policy before provider selection continues,
/// export the provider skip, and finalize with the carried status.
/// A failed sync finalizes as failed; a failed reload closes
/// without finalizing; otherwise the finalize status decides.
pub fn sequence_update(inputs: &SequenceInputs) -> SequenceOutcome {
    if !inputs.sync_ok {
        return SequenceOutcome {
            rc: 1,
            provider_none: false,
            finalize_arg: Some(1),
        };
    }
    if !inputs.config_ok {
        return SequenceOutcome {
            rc: 1,
            provider_none: false,
            finalize_arg: None,
        };
    }
    SequenceOutcome {
        rc: if inputs.finalize_rc == 0 { 0 } else { 1 },
        provider_none: inputs.skip_provider,
        finalize_arg: Some(0),
    }
}
