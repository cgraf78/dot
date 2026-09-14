//! `_pull_overlay` (`lib/dot/repos/pull.sh`): the single-overlay pull
//! orchestrator (missing-checkout clone, worktree/origin guards,
//! upstream fetch, generation fast path, candidate validation,
//! parent snapshot, pull with backup retry, mode normalization).
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`. Unlike `_pull_base`, the shell always returns 0
//! here; every outcome rides `REPLY_STATUS`, including the empty
//! status for quiet optional short-circuits.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use crate::cleanup::Registry;
use crate::log::{Log, is_quiet};
use crate::overlays::{effective_url, is_worktree};
use crate::progress_ui::Palette;
use crate::repos_base::Base;
use crate::repos_config::{has_upstream, origin_matches};
use crate::repos_overlays::{DestinationInputs, QuarantineInputs};
use crate::repos_pull::{PullRepoInputs, pull_repo};
use crate::repos_pull_clone::{CloneOverlayInputs, clone_overlay_staged};
use crate::repos_pull_normalize::{normalize_updated_paths, snapshot_updated_path_parents};
use crate::repos_pull_queries::{
    CandidateEnv, accept_current_generation, repo_head, repo_head_is, validate_candidate_tree,
};
use crate::repos_pull_support::{OriginMismatch, origin_mismatch, prepare_overlay_upstream};
use crate::temp::{MoveCache, MoveTool, read_umask};

/// `_pull_overlay` outcome status (`REPLY_STATUS`). `Empty` is the
/// shell's `REPLY_STATUS=""`: a quiet optional clone failure, a
/// quiet optional prepare/validation failure, or a quiet optional
/// pull failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOverlayStatus {
    /// No status word (quiet optional short-circuit).
    Empty,
    /// A missing checkout was cloned.
    Cloned,
    /// No upstream tracks the checkout.
    Skipped,
    /// The checkout already matches upstream.
    Current,
    /// The pull moved the checkout.
    Changed,
    /// Anything else went wrong.
    Failed,
}

impl PullOverlayStatus {
    /// The `REPLY_STATUS` word (`""` for [`PullOverlayStatus::Empty`]).
    pub fn as_str(&self) -> &'static str {
        match self {
            PullOverlayStatus::Empty => "",
            PullOverlayStatus::Cloned => "cloned",
            PullOverlayStatus::Skipped => "skipped",
            PullOverlayStatus::Current => "current",
            PullOverlayStatus::Changed => "changed",
            PullOverlayStatus::Failed => "failed",
        }
    }
}

/// Outcome of [`pull_overlay`]: the status plus the shell exit code
/// (always 0, like `_pull_overlay`) and the trailing live-line flag
/// for subsequent `_ui_status` rows.
pub struct PullOverlayOutcome {
    /// The `REPLY_STATUS` decision.
    pub status: PullOverlayStatus,
    /// The shell return code (always 0).
    pub rc: i32,
    /// The `_ui_clear_live` flag after the last status row.
    pub live_active: bool,
}

/// Inputs for [`pull_overlay`]: the overlay identity, the raw UI
/// flags, the candidate environment, and the pull context shared
/// with [`crate::repos_pull::pull_base`]. Raw `DOT_UI_TOTAL`,
/// `DOT_QUIET`, and `DOT_VERBOSE` spellings ride through so
/// arithmetic failures read exactly like the shell's.
pub struct PullOverlayInputs<'a> {
    /// Overlay name for messages.
    pub name: &'a str,
    /// Checkout path.
    pub path: &'a str,
    /// Configured URL (before `~`/relative resolution).
    pub url: &'a str,
    /// Optional overlay: failures stay quiet and statusless.
    pub optional: bool,
    /// Extra git pull arguments after the upstream.
    pub extra_args: &'a [OsString],
    /// Client `$HOME`: effective-URL base and backup parent.
    pub home: &'a str,
    /// `DOT_UI_TOTAL`: counted UI takes status rows when `> 0`.
    pub ui_total: Option<&'a str>,
    /// `DOT_QUIET`: status rows stay silent at arithmetic 1, and
    /// pulls run quiet.
    pub dot_quiet: Option<&'a str>,
    /// `DOT_VERBOSE`: running/changed/ok rows print at arithmetic 1.
    pub dot_verbose: Option<&'a str>,
    /// Palette for `_ui_status` rows.
    pub palette: &'a Palette,
    /// The `_ui_clear_live` flag on entry.
    pub live_active: bool,
    /// Multibyte cell widths for `_ui_status` rows.
    pub multibyte: bool,
    /// Candidate validation environment.
    pub candidate: &'a CandidateEnv,
    /// Base checkout for the installed-link restore walk.
    pub base: &'a Base,
    /// Quarantine support (`None` backs everything as user data).
    pub quarantine: Option<QuarantineInputs>,
    /// Overlay records (`OVERLAYS`) for the restore walk.
    pub overlays: &'a [String],
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a DestinationInputs,
    /// Selected manifest (`$DOT_OVERLAY_MANIFEST`).
    pub manifest: &'a str,
    /// Legacy manifest (`$DOT_OVERLAY_LEGACY_MANIFEST`).
    pub legacy_manifest: &'a str,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root: &'a Path,
    /// Base for the legacy-hash throwaway repository.
    pub tmp: &'a Path,
    /// Probed move tool for the restore walk.
    pub tool: &'a MoveTool,
    /// Logger for headers and warnings.
    pub log: &'a Log,
}

/// One `_ui_status` row to `out`, returning the next live flag.
fn ui_row(
    palette: &Palette,
    quiet: bool,
    live_active: bool,
    multibyte: bool,
    status: &str,
    detail: &str,
    out: &mut dyn Write,
) -> bool {
    let (bytes, live_active) = crate::progress_ui::status(
        palette,
        quiet,
        live_active,
        status.as_bytes(),
        detail.as_bytes(),
        multibyte,
    );
    let _ = out.write_all(&bytes);
    live_active
}

/// The staged clone with both streams discarded, like the shell's
/// `>/dev/null 2>&1` redirect. An unreadable umask fails the clone
/// like any other staging failure.
fn clone_suppressed(
    inputs: &PullOverlayInputs<'_>,
    effective: &str,
    moves: &mut MoveCache,
) -> bool {
    let Ok(mask) = read_umask() else {
        return false;
    };
    let clone_inputs = CloneOverlayInputs {
        url: effective,
        path: inputs.path,
        candidate: inputs.candidate,
        mask,
        log: inputs.log,
    };
    let mut sink = Vec::new();
    clone_overlay_staged(&clone_inputs, moves, &mut sink)
}

/// Best-effort snapshot removal, like `|| true`.
fn remove_snapshot(snapshot: &Path) {
    let mut cleanup = Registry::new();
    let _ = cleanup.remove_path(snapshot);
}

/// Checked snapshot removal: a failure flips the status to failed.
fn remove_snapshot_checked(snapshot: &Path) -> bool {
    let mut cleanup = Registry::new();
    cleanup.remove_path(snapshot).is_ok()
}

/// `_pull_overlay`: clone a missing checkout or pull an existing
/// one through the same fetch, fast-path, validation, snapshot,
/// pull, and normalization stages as the base. Stdout carries
/// headers and counted-UI rows; warnings carry the plain warnings.
/// The return code is always 0; only the status varies.
pub fn pull_overlay(
    inputs: &PullOverlayInputs<'_>,
    moves: &mut MoveCache,
    out: &mut dyn Write,
    warnings: &mut dyn Write,
) -> PullOverlayOutcome {
    let name = inputs.name;
    let quiet = is_quiet(inputs.dot_quiet);
    let verbose = crate::progress_ui::arith_value(inputs.dot_verbose.unwrap_or("0")) == Some(1);
    let counted = inputs
        .ui_total
        .and_then(crate::progress_ui::arith_value)
        .is_some_and(|total| total > 0);
    let mut live = inputs.live_active;
    let done = |status: PullOverlayStatus, live_active: bool| PullOverlayOutcome {
        status,
        rc: 0,
        live_active,
    };
    let effective = effective_url(inputs.url, inputs.home);
    let prefix = [OsString::from("-C"), OsString::from(inputs.path)];

    // The effective URL rewrites first (an empty URL becomes
    // `$HOME/`), so the shell's emptiness short-circuit never fires
    // and a missing checkout always attempts the staged clone.
    if Path::new(inputs.path).symlink_metadata().is_err() {
        if inputs.optional {
            if clone_suppressed(inputs, &effective, moves) {
                return done(PullOverlayStatus::Cloned, live);
            }
            return done(PullOverlayStatus::Empty, live);
        }
        if counted {
            if verbose {
                live = ui_row(
                    inputs.palette,
                    quiet,
                    live,
                    inputs.multibyte,
                    "running",
                    &format!("{name} dotfiles: cloning"),
                    out,
                );
            }
        } else {
            inputs
                .log
                .log_header(out, &format!("==> Cloning {name} dotfiles..."));
        }
        if !clone_suppressed(inputs, &effective, moves) {
            if counted {
                live = ui_row(
                    inputs.palette,
                    quiet,
                    live,
                    inputs.multibyte,
                    "warning",
                    &format!("{name} dotfiles clone failed"),
                    out,
                );
            } else {
                inputs.log.warn(
                    warnings,
                    &format!("  warning: {name} dotfiles clone failed"),
                );
            }
            return done(PullOverlayStatus::Failed, live);
        }
        if counted && verbose {
            live = ui_row(
                inputs.palette,
                quiet,
                live,
                inputs.multibyte,
                "changed",
                &format!("{name} dotfiles cloned"),
                out,
            );
        }
        return done(PullOverlayStatus::Cloned, live);
    }

    // Do not replace existing paths during unattended updates. A
    // linked worktree has a `.git` file, and any other path may
    // contain user-owned data.
    if !is_worktree(Path::new(inputs.path)) {
        if counted {
            live = ui_row(
                inputs.palette,
                quiet,
                live,
                inputs.multibyte,
                "warning",
                &format!("{name} overlay path exists but is not a Git worktree"),
                out,
            );
        } else {
            inputs.log.warn(
                warnings,
                &format!(
                    "  warning: {name} overlay path exists but is not a Git worktree; leaving it untouched: {}",
                    inputs.path
                ),
            );
        }
        return done(PullOverlayStatus::Failed, live);
    }

    let (matched, actual) = origin_matches(Path::new(inputs.path), &effective);
    if !matched {
        let (rows, errs, next) = origin_mismatch(
            inputs.palette,
            live,
            inputs.multibyte,
            &OriginMismatch {
                name,
                path: inputs.path,
                expected: &effective,
                actual: &actual,
                ui_total: inputs.ui_total,
                quiet: inputs.dot_quiet,
            },
        );
        let _ = out.write_all(&rows);
        let _ = warnings.write_all(&errs);
        return done(PullOverlayStatus::Failed, next);
    }

    if !has_upstream(&prefix) {
        if counted && verbose {
            live = ui_row(
                inputs.palette,
                quiet,
                live,
                inputs.multibyte,
                "skipped",
                &format!("{name} dotfiles pull skipped (no upstream)"),
                out,
            );
        }
        return done(PullOverlayStatus::Skipped, live);
    }
    let upstream = match prepare_overlay_upstream(Path::new(inputs.path), inputs.optional) {
        Ok(upstream) => upstream,
        Err(_) => {
            if inputs.optional {
                return done(PullOverlayStatus::Empty, live);
            }
            if counted {
                live = ui_row(
                    inputs.palette,
                    quiet,
                    live,
                    inputs.multibyte,
                    "warning",
                    &format!("{name} dotfiles pull failed"),
                    out,
                );
            } else {
                inputs
                    .log
                    .warn(warnings, &format!("  warning: {name} dotfiles pull failed"));
            }
            return done(PullOverlayStatus::Failed, live);
        }
    };
    let head_before = repo_head(&prefix);
    // Match the base fast path, including local-delta policy and a
    // final HEAD generation check before accepting the checkout.
    match accept_current_generation(
        &prefix,
        "overlay",
        &head_before,
        &upstream,
        inputs.candidate,
        inputs.log,
        warnings,
    ) {
        0 => return done(PullOverlayStatus::Current, live),
        1 => {}
        _ => {
            inputs.log.warn(
                warnings,
                &format!(
                    "  warning: {name} overlay local generation failed validation or changed during synchronization"
                ),
            );
            return done(PullOverlayStatus::Failed, live);
        }
    }
    if !validate_candidate_tree(
        &prefix,
        "overlay",
        &upstream,
        inputs.candidate,
        inputs.log,
        warnings,
    ) {
        if inputs.optional {
            return done(PullOverlayStatus::Empty, live);
        }
        inputs.log.warn(
            warnings,
            &format!("  warning: {name} overlay candidate failed reserved-path validation"),
        );
        return done(PullOverlayStatus::Failed, live);
    }
    let snapshot =
        match snapshot_updated_path_parents(&prefix, inputs.path, &head_before, &upstream) {
            Some(snapshot) => snapshot,
            None => return done(PullOverlayStatus::Failed, live),
        };
    if !repo_head_is(&prefix, &head_before) {
        remove_snapshot(Path::new(&snapshot));
        inputs.log.warn(
            warnings,
            &format!("  warning: {name} overlay changed during synchronization"),
        );
        return done(PullOverlayStatus::Failed, live);
    }

    // The prefix carries `-C <path>`; the pull command supplies the
    // `git` binary itself.
    let mut command: Vec<OsString> = vec![
        OsString::from("git"),
        OsString::from("-C"),
        OsString::from(inputs.path),
        OsString::from("rebase"),
        OsString::from("--autostash"),
        OsString::from(&upstream),
    ];
    command.extend(inputs.extra_args.iter().cloned());

    if inputs.optional {
        // The optional pull runs under `DOT_QUIET=1`, restored by
        // dropping the flag instead of mutating shared state.
        let repo_inputs = PullRepoInputs {
            home: inputs.home,
            root: inputs.path,
            base: inputs.base,
            quarantine: inputs.quarantine.clone(),
            overlays: inputs.overlays,
            dest: inputs.dest,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            euid: inputs.euid,
            source_root: inputs.source_root,
            tmp: inputs.tmp,
            tool: inputs.tool,
            command: &command,
            quiet: true,
            verbose,
            log: inputs.log,
        };
        if pull_repo(&repo_inputs, moves, out, warnings) != 0 {
            remove_snapshot(Path::new(&snapshot));
            return done(PullOverlayStatus::Empty, live);
        }
        let head_after = repo_head(&prefix);
        let mut status = PullOverlayStatus::Current;
        if !head_before.is_empty() && !head_after.is_empty() && head_before != head_after {
            let normalized = read_umask().is_ok_and(|mask| {
                normalize_updated_paths(
                    &prefix,
                    inputs.path,
                    "overlay",
                    &head_before,
                    &head_after,
                    &snapshot,
                    inputs.home,
                    inputs.overlays,
                    mask,
                )
            });
            if !normalized {
                remove_snapshot(Path::new(&snapshot));
                return done(PullOverlayStatus::Failed, live);
            }
            status = PullOverlayStatus::Changed;
        }
        if !remove_snapshot_checked(Path::new(&snapshot)) {
            status = PullOverlayStatus::Failed;
        }
        return done(status, live);
    }

    if counted {
        if verbose {
            live = ui_row(
                inputs.palette,
                quiet,
                live,
                inputs.multibyte,
                "running",
                &format!("{name} dotfiles: pulling"),
                out,
            );
        }
    } else {
        inputs
            .log
            .log_header(out, &format!("==> Pulling {name} dotfiles..."));
    }
    let repo_inputs = PullRepoInputs {
        home: inputs.home,
        root: inputs.path,
        base: inputs.base,
        quarantine: inputs.quarantine.clone(),
        overlays: inputs.overlays,
        dest: inputs.dest,
        manifest: inputs.manifest,
        legacy_manifest: inputs.legacy_manifest,
        euid: inputs.euid,
        source_root: inputs.source_root,
        tmp: inputs.tmp,
        tool: inputs.tool,
        command: &command,
        quiet,
        verbose,
        log: inputs.log,
    };
    if pull_repo(&repo_inputs, moves, out, warnings) != 0 {
        remove_snapshot(Path::new(&snapshot));
        if counted {
            live = ui_row(
                inputs.palette,
                quiet,
                live,
                inputs.multibyte,
                "warning",
                &format!("{name} dotfiles pull failed"),
                out,
            );
        } else {
            inputs
                .log
                .warn(warnings, &format!("  warning: {name} dotfiles pull failed"));
        }
        return done(PullOverlayStatus::Failed, live);
    }
    let head_after = repo_head(&prefix);
    let mut status = PullOverlayStatus::Current;
    if !head_before.is_empty() && !head_after.is_empty() && head_before != head_after {
        let normalized = read_umask().is_ok_and(|mask| {
            normalize_updated_paths(
                &prefix,
                inputs.path,
                "overlay",
                &head_before,
                &head_after,
                &snapshot,
                inputs.home,
                inputs.overlays,
                mask,
            )
        });
        if !normalized {
            remove_snapshot(Path::new(&snapshot));
            inputs.log.warn(
                warnings,
                &format!("  warning: {name} overlay mode normalization failed"),
            );
            return done(PullOverlayStatus::Failed, live);
        }
        if counted && verbose {
            live = ui_row(
                inputs.palette,
                quiet,
                live,
                inputs.multibyte,
                "changed",
                &format!("{name} dotfiles updated"),
                out,
            );
        }
        status = PullOverlayStatus::Changed;
    } else if counted && verbose {
        live = ui_row(
            inputs.palette,
            quiet,
            live,
            inputs.multibyte,
            "ok",
            &format!("{name} dotfiles current"),
            out,
        );
    }
    if !remove_snapshot_checked(Path::new(&snapshot)) {
        status = PullOverlayStatus::Failed;
    }
    done(status, live)
}
