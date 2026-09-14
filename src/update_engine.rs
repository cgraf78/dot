//! Native `dot update` engine driver (engine update lane).
//!
//! Executes `_dot_update` (`lib/dot/update.sh`) without the shell:
//! flag parsing, `_ui_begin`, the cron dirty gate, repo sync
//! (installed-link snapshot, base/overlay pull, policy reload,
//! overlay converge, lifecycle prepare), the defensive config
//! reload, and finalize (provider checkpoint, link phase,
//! lifecycle retire, shdeps branch, merges, lifecycle commit,
//! worktree normalize, `_ui_done`). Pure sequencing folds live in
//! [`crate::update`]; this module owns the impure step execution,
//! composing [`crate::repos_pull_fleet`], [`crate::repos_link_all`],
//! [`crate::profile_lifecycle`], [`crate::pre_sync`],
//! [`crate::merges`], and [`crate::shdeps`].
//!
//! Coverage boundary (documented, not hidden): the v1 driver goes
//! native only inside a conservative envelope, and the caller
//! falls back to the shell adapter outside it:
//!
//! - `--cron` stays shell (the dirty-tree resolver has no native
//!   port yet).
//! - A `profiles.d` directory stays shell (the two-phase profile
//!   converge has no native port yet).
//! - `DOT_DEPENDENCY_PROVIDER=shdeps` stays shell (ensure plus the
//!   updater UI have no native ports yet; `none` runs natively).
//! - A non-empty `merge-hooks.d` stays shell (the merge driver has
//!   no native port yet; the empty case renders its stage rows
//!   natively).
//!
//! Nothing is wired yet: the update command still drives the
//! shell adapter, so this lane changes no behavior (the integrator
//! owns the wiring, starting with [`should_go_native`]).

use std::path::Path;

use crate::log::Log;
use crate::progress_ui::{Palette, Stage};
use crate::repos_base::Base;
use crate::repos_overlays::{self, DestinationInputs};
use crate::temp::MoveTool;

/// Parsed `update`/`pull` flags (the shell loop exports; the
/// native driver takes them as values).
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateFlags {
    /// `--cron` (implies quiet).
    pub cron: bool,
    /// `--quiet`.
    pub quiet: bool,
    /// `-f`/`--force`.
    pub force: bool,
    /// `-v`/`--verbose`.
    pub verbose: bool,
}

/// Shared inputs for the native update driver: every global the
/// shell `_dot_update` tree reads, plus the UI/logger handles the
/// pull and link lanes thread the same way.
pub struct EngineInputs<'a> {
    /// Parsed flags.
    pub flags: UpdateFlags,
    /// Residue after flags (forwarded to the pull phases).
    pub extra_args: &'a [std::ffi::OsString],
    /// Client `$HOME`.
    pub home: &'a str,
    /// Resolved XDG state home.
    pub state_home: &'a str,
    /// Resolved XDG config home (empty reads unset).
    pub config_home: &'a str,
    /// Overlay records at entry (`OVERLAYS`, usually empty: the
    /// converge step rediscovers).
    pub entries: &'a [String],
    /// `DOT_UPDATE_JOBS`: numeric bound, else the CPU count.
    pub update_jobs: Option<&'a str>,
    /// `DOT_VERBOSE` (in addition to the flag; either enables).
    pub dot_verbose: Option<&'a str>,
    /// `DOT_QUIET` (in addition to the flag; either quiets).
    pub dot_quiet: Option<&'a str>,
    /// `DOT_UI_PROGRESS_WIDTH`: bar width, default `"8"`.
    pub bar_width: &'a str,
    /// `DOT_INIT_SKIP_PROVIDER == 1`.
    pub skip_provider: bool,
    /// `DOT_DEPENDENCY_PROVIDER` (`"none"` or `"shdeps"`).
    pub provider: &'a str,
    /// Selected manifest (`$DOT_OVERLAY_MANIFEST`).
    pub manifest: &'a str,
    /// Legacy manifest (`$DOT_OVERLAY_LEGACY_MANIFEST`).
    pub legacy_manifest: &'a str,
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a DestinationInputs,
    /// Base client repository (`None` without one).
    pub base: Option<&'a Base>,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root_git: &'a Path,
    /// Source checkout root (`$DOT_SOURCE_ROOT`, home of
    /// `support/client-launcher.sh`).
    pub checkout_root: &'a str,
    /// Precomputed `_ui_live_enabled` for the stage.
    pub live: bool,
    /// Base for throwaway repositories.
    pub tmp: &'a Path,
    /// Probed move tool.
    pub tool: &'a MoveTool,
    /// Logger palette for rows and warnings.
    pub palette: &'a Palette,
    /// Whether to count UTF-8 characters for status cells.
    pub multibyte: bool,
    /// Whether to render ASCII progress glyphs.
    pub ascii: bool,
    /// Logger for headers and `_log` rows.
    pub log: &'a Log,
    /// Extensions root (`$DOT_EXTENSIONS_DIR`) for hook discovery.
    pub extensions_dir: &'a str,
}

/// Why the v1 driver declines a run (the caller runs the shell
/// adapter instead). Every reason names the missing native port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// `--cron`: the dirty-tree resolver is shell-only.
    Cron,
    /// `--force` on pull: the fetch remotes reset is shell-only.
    ForcePull,
    /// Missing or relative `$HOME`: trust checks need an
    /// absolute client root.
    NoHome,
    /// `profiles.d` exists: two-phase profile converge is shell-only.
    Profiles,
    /// Present `merge-hooks.d`: the merge runner is shell-only.
    MergeHooks,
    /// Present `pre-sync.d`: the reconcile runner is shell-only.
    PreSyncHooks,
    /// `DOT_DEPENDENCY_PROVIDER` is not `none`: ensure plus the
    /// updater UI are shell-only (anything but `none` also covers
    /// the shell's `shdeps unavailable` close).
    ShdepsProvider,
    /// A cron dirty tree the resolver could not cleanly discard:
    /// silent exit 0 stays shell-side.
    CronDirty,
    /// `DOT_INIT_SKIP_PROVIDER` set without the `1` spelling: the
    /// provider is unknown, so the shell's `shdeps unavailable`
    /// close owns it.
    ProviderUnavailable,
    /// Present `merges.sh` hooks: the merge runner is shell-only.
    MergesPresent,
    /// `DOT_OVERLAY_LINKS_FROZEN` already set: a sourced caller is
    /// mid-flight, so the shell owns the frozen close.
    FsReplaceBlocked,
}

/// Native envelope check for one run: `Ok(())` runs
/// [`run_update`], `Err(reason)` runs the shell adapter.
pub fn should_go_native(inputs: &EngineInputs<'_>) -> Result<(), Fallback> {
    if inputs.flags.cron {
        return Err(Fallback::Cron);
    }
    if inputs.flags.force {
        return Err(Fallback::ForcePull);
    }
    if inputs.home.is_empty() || !inputs.home.starts_with('/') {
        return Err(Fallback::NoHome);
    }
    if profiles_present(inputs.config_home) {
        return Err(Fallback::Profiles);
    }
    if inputs.provider != "none" {
        return Err(Fallback::ShdepsProvider);
    }
    if has_hook_dir(inputs.extensions_dir, "merge-hooks.d") {
        return Err(Fallback::MergeHooks);
    }
    if has_hook_dir(inputs.extensions_dir, "pre-sync.d") {
        return Err(Fallback::PreSyncHooks);
    }
    if has_hook_dir(inputs.home, ".config/dot/merges") {
        return Err(Fallback::MergesPresent);
    }
    if std::env::var("DOT_OVERLAY_LINKS_FROZEN").ok().as_deref() == Some("1") {
        return Err(Fallback::FsReplaceBlocked);
    }
    Ok(())
}

/// `profiles.d` presence: `_dot_profiles_load_default` sets
/// `DOT_PROFILES_PRESENT=1` exactly when `$config/dot/profiles.d`
/// exists (any type; the directory check errors later).
fn profiles_present(config_home: &str) -> bool {
    if config_home.is_empty() {
        return false;
    }
    let dir = Path::new(config_home).join("dot/profiles.d");
    std::fs::symlink_metadata(&dir).is_ok()
}

/// Hook directory presence: `_merge_hook_specs` and
/// `_dot_pre_sync_specs` yield nothing when their directories are
/// absent, so only a present directory needs the shell driver.
/// Any symlink metadata (even to a missing target) counts, like
/// the shell's existence test before listing.
fn has_hook_dir(root: &str, name: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    std::fs::symlink_metadata(Path::new(root).join(name)).is_ok()
}

/// Installed-link generation snapshot (`DOT_OVERLAY_ROLLBACK_PATHS`
/// / `DOT_OVERLAY_ROLLBACK_TARGETS`) for the base pull's adoption
/// walk and the failure-path restore.
struct InstalledSnapshot {
    rels: Vec<String>,
    targets: Vec<String>,
}

/// `_overlay_snapshot_installed_links`: recover, snapshot the
/// reserved roots, then record every live managed link the
/// authority manifests still own. `None` is the bare `return 1`
/// (the manifest-unsafe warning travels in `err`).
fn snapshot_installed_links(
    inputs: &EngineInputs<'_>,
    err: &mut Vec<u8>,
) -> Option<InstalledSnapshot> {
    if let Err(record) = repos_overlays::recover_replacements(
        inputs.manifest,
        inputs.euid,
        inputs.source_root_git,
        inputs.tmp,
        &inputs.dest.pwd,
        inputs.tool,
    ) {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: unsafe overlay replacement recovery record: {record}"),
        );
        return None;
    }
    let mut overlay_paths = Vec::new();
    for entry in inputs.entries {
        let path = entry.split('|').nth(1).unwrap_or("");
        if !path.is_empty() {
            overlay_paths.push(path.to_string());
        }
    }
    let snapshot = repos_overlays::reserved_snapshot_vec(inputs.home, inputs.dest, &overlay_paths)?;
    let snapshot_text = snapshot.join("\n");
    let found =
        match repos_overlays::authority_files(inputs.manifest, inputs.legacy_manifest, inputs.euid)
        {
            Ok(found) => found,
            Err(reply) => {
                warn_row(
                    err,
                    inputs.palette,
                    &format!("  warning: unsafe installed overlay manifest: {reply}"),
                );
                return None;
            }
        };
    let mut cache = repos_overlays::AuthorityCache::enabled();
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut snapshot_out = InstalledSnapshot {
        rels: Vec::new(),
        targets: Vec::new(),
    };
    for manifest in &found.manifests {
        let content = match std::fs::read(manifest) {
            Ok(content) => content,
            Err(_) => return None,
        };
        for line in repos_overlays::stream_lines(&content) {
            let record = repos_overlays::parse_manifest_record(&line)?;
            if repos_overlays::path_is_authority(
                inputs.home,
                &record.rel,
                inputs.manifest,
                inputs.legacy_manifest,
                inputs.dest,
                Some(&snapshot_text),
                &mut cache,
            ) {
                continue;
            }
            let dst = format!("{}/{}", inputs.home, record.rel);
            if !std::fs::symlink_metadata(&dst).is_ok_and(|meta| meta.file_type().is_symlink()) {
                continue;
            }
            let live = match std::fs::read_link(&dst) {
                Ok(target) => target.to_string_lossy().into_owned(),
                Err(_) => return None,
            };
            if live != record.target {
                continue;
            }
            if let Some(known) = seen.get(&record.rel) {
                if *known != record.target {
                    return None;
                }
                continue;
            }
            seen.insert(record.rel.clone(), record.target.clone());
            snapshot_out.rels.push(record.rel);
            snapshot_out.targets.push(record.target);
        }
    }
    Some(snapshot_out)
}

/// Append one `_warn` row to the stderr stream.
fn warn_row(err: &mut Vec<u8>, palette: &Palette, message: &str) {
    err.extend_from_slice(&crate::progress_ui::warn_line(palette, message.as_bytes()));
}

/// Result of the native repo-sync phase.
pub struct SyncDone {
    /// `_dot_update_sync_repos` exit status.
    pub rc: i32,
    /// `DOT_OVERLAY_LINKS_FROZEN=1` on the way out.
    pub frozen: bool,
    /// Active `OVERLAYS` for the link phase.
    pub entries: Vec<String>,
}

/// Combined repo counts behind one deferred stage close: the base
/// pull stash plus the overlay phase tally (the shell folds both
/// into `DOT_REPO_AGG_*` before `_dot_update_repo_stage_finish`).
struct Agg {
    current: i64,
    changed: i64,
    failed: i64,
    skipped: i64,
    changed_items: Vec<u8>,
}

impl Agg {
    fn zero() -> Self {
        Agg {
            current: 0,
            changed: 0,
            failed: 0,
            skipped: 0,
            changed_items: Vec::new(),
        }
    }

    /// Deferred stash after the base pull (`DOT_REPO_AGG_*`).
    fn base(outcome: &crate::repos_pull_fleet::PullAllOutcome) -> Self {
        let mut changed_items = Vec::new();
        for item in &outcome.changed_items {
            if !changed_items.is_empty() {
                changed_items.push(b'\n');
            }
            changed_items.extend_from_slice(item.as_bytes());
        }
        Agg {
            current: outcome.current,
            changed: outcome.changed,
            failed: outcome.failed,
            skipped: outcome.skipped,
            changed_items,
        }
    }

    /// Fold one overlay phase tally
    /// (`_dot_update_pull_overlay_phase`).
    fn fold_overlay(&mut self, outcome: &crate::repos_pull_fleet::PullOverlaysOutcome) {
        let tally = &outcome.tally;
        self.current += tally.current as i64;
        self.changed += tally.changed as i64;
        self.failed += tally.failed as i64;
        self.skipped += tally.skipped as i64;
        if !tally.changed_items.is_empty() {
            if !self.changed_items.is_empty() {
                self.changed_items.push(b'\n');
            }
            self.changed_items
                .extend_from_slice(tally.changed_items.as_bytes());
        }
    }

    /// Merge a phase aggregate into this one.
    fn fold_agg(&mut self, other: &Agg) {
        self.current += other.current;
        self.changed += other.changed;
        self.failed += other.failed;
        self.skipped += other.skipped;
        if !other.changed_items.is_empty() {
            if !self.changed_items.is_empty() {
                self.changed_items.push(b'\n');
            }
            self.changed_items.extend_from_slice(&other.changed_items);
        }
    }

    /// The single deferred close
    /// (`_dot_update_repo_stage_finish`).
    fn close(
        &self,
        stage: &mut Stage,
        forced: &str,
        verbose: Option<&str>,
        now_secs: i64,
    ) -> Vec<u8> {
        repo_finish(
            stage,
            forced,
            &self.current.to_string(),
            &self.changed.to_string(),
            &self.failed.to_string(),
            &self.skipped.to_string(),
            &self.changed_items,
            verbose,
            now_secs,
        )
    }
}

/// Outcome of one `_dot_converge_overlays` run: the phase status,
/// the current `OVERLAYS` set, and the overlay counts for the
/// deferred close. The close itself renders once in [`sync_tail`].
struct ConvergeOut {
    rc: i32,
    entries: Vec<String>,
    overlay: Agg,
}

/// Build the pull-phase candidate environment with the overlay
/// link paths parsed from `entries` (the shell re-derives them
/// per call).
fn pull_candidate(
    inputs: &EngineInputs<'_>,
    entries: &[String],
) -> crate::repos_pull_queries::CandidateEnv {
    let mut overlay_paths = Vec::new();
    for entry in entries {
        let path = entry.split('|').nth(1).unwrap_or("");
        if !path.is_empty() {
            overlay_paths.push(path.to_string());
        }
    }
    candidate_env(inputs, overlay_paths)
}

/// Render the deferred repo-stage close (no-op unless a deferred
/// pull left it active, like `_dot_update_repo_stage_finish`).
#[allow(clippy::too_many_arguments)]
fn repo_finish(
    stage: &mut Stage,
    forced: &str,
    current: &str,
    changed: &str,
    failed: &str,
    skipped: &str,
    changed_items: &[u8],
    verbose: Option<&str>,
    now_secs: i64,
) -> Vec<u8> {
    crate::update::repo_stage_finish(
        stage,
        &crate::update::RepoStageFinish {
            deferred_active: true,
            forced_failure: Some(forced),
            agg_current: Some(current),
            agg_changed: Some(changed),
            agg_failed: Some(failed),
            agg_skipped: Some(skipped),
            changed_items,
            verbose,
        },
        now_secs,
    )
}

/// Restore the pre-pull link generation, warning exactly like the
/// shell when the restore itself fails. Returns whether the
/// caller continues.
fn restore_generation(
    inputs: &EngineInputs<'_>,
    base: &Base,
    snapshot: &InstalledSnapshot,
    entries: &[String],
    err: &mut Vec<u8>,
) -> bool {
    let ok = crate::repos_overlays::restore_installed_links(
        &crate::repos_overlays::RestoreInstalledInputs {
            base,
            home: inputs.home,
            rels: &snapshot.rels,
            targets: &snapshot.targets,
            overlays: entries,
            dest: inputs.dest,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            euid: inputs.euid,
            source_root: inputs.source_root_git,
            tmp: inputs.tmp,
            tool: inputs.tool,
        },
    );
    if !ok {
        warn_row(
            err,
            inputs.palette,
            "  warning: could not restore the previous overlay-link generation",
        );
    }
    ok
}

/// `_dot_update_sync_repos` natively: snapshot the installed
/// links, pull the base generation, reload policy, converge the
/// overlays, and prepare the profile lifecycle. Every failure
/// closes the deferred repo stage as failed, restores the
/// previous link generation, and freezes overlay linking.
pub fn sync_repos(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    moves: &mut crate::temp::MoveCache,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
) -> SyncDone {
    use std::io::Write as _;
    let base = inputs.base.filter(|found| found.exists());
    let Some(base) = base else {
        // No base checkout: config only, then the shared tail
        // with a zero stash and no restore authority (the shell
        // falls through to converge the same way).
        crate::repos_config::ensure_repo_config(None);
        return sync_tail(
            inputs,
            stage,
            moves,
            out,
            err,
            now_secs,
            Agg::zero(),
            None,
            None,
            false,
        );
    };
    // Capture the already-installed generation before pull
    // restores its shadowed base paths (the snapshot failure
    // returns before the stage close, like the shell).
    let snapshot = match snapshot_installed_links(inputs, err) {
        Some(snapshot) => snapshot,
        None => {
            return SyncDone {
                rc: 1,
                frozen: true,
                entries: Vec::new(),
            };
        }
    };
    // `OVERLAYS=()` plus the deferred base pull.
    let candidate = pull_candidate(inputs, &[]);
    let pull_inputs = crate::repos_pull_fleet::PullAllInputs {
        entries: &[],
        extra_args: inputs.extra_args,
        home: inputs.home,
        dot_quiet: inputs.dot_quiet,
        dot_verbose: inputs.dot_verbose,
        ui_total: Some("5"),
        update_jobs: inputs.update_jobs,
        bar_width: inputs.bar_width,
        defer_finish: Some("1"),
        palette: inputs.palette,
        multibyte: inputs.multibyte,
        ascii: inputs.ascii,
        candidate: &candidate,
        base,
        quarantine: Some(quarantine_inputs(inputs, &snapshot)),
        overlays: &[],
        dest: inputs.dest,
        manifest: inputs.manifest,
        legacy_manifest: inputs.legacy_manifest,
        euid: inputs.euid,
        source_root: inputs.source_root_git,
        tmp: inputs.tmp,
        tool: inputs.tool,
        log: inputs.log,
    };
    let outcome = crate::repos_pull_fleet::pull_all(&pull_inputs, stage, moves, out, err, now_secs);
    if outcome.rc != 0 || outcome.failed > 0 {
        let close = Agg::base(&outcome).close(stage, "1", inputs.dot_verbose, now_secs);
        let _ = out.write_all(&close);
        restore_generation(inputs, base, &snapshot, &[], err);
        return SyncDone {
            rc: 1,
            frozen: true,
            entries: Vec::new(),
        };
    }
    // A base pull may replace policy: reload before either phase
    // resolves or any transport preparation runs (the loader
    // prints its own diagnostic on failure).
    if let Err(failure) = crate::startup::check_ambient() {
        err.extend_from_slice(failure.line().as_bytes());
        err.push(b'\n');
        let close = Agg::base(&outcome).close(stage, "1", inputs.dot_verbose, now_secs);
        let _ = out.write_all(&close);
        restore_generation(inputs, base, &snapshot, &[], err);
        return SyncDone {
            rc: 1,
            frozen: true,
            entries: Vec::new(),
        };
    }
    sync_tail(
        inputs,
        stage,
        moves,
        out,
        err,
        now_secs,
        Agg::base(&outcome),
        Some(base),
        Some(snapshot),
        outcome.deferred,
    )
}

/// Shared tail of `_dot_update_sync_repos`: converge the overlays,
/// prepare the lifecycle, and render the single deferred stage
/// close exactly once. `base` and `snapshot` travel together (both
/// `Some` past a base pull); without them there is nothing to
/// restore, like the shell's `_base_repo_exists` guard.
/// `close_active` mirrors `DOT_REPO_STAGE_DEFERRED_ACTIVE`: without
/// a deferred pull no close renders at all (the shell returns
/// before the first row).
#[allow(clippy::too_many_arguments)]
fn sync_tail(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    moves: &mut crate::temp::MoveCache,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
    mut agg: Agg,
    base: Option<&Base>,
    snapshot: Option<InstalledSnapshot>,
    close_active: bool,
) -> SyncDone {
    use std::io::Write as _;
    let conv = converge_overlays(inputs, stage, moves, out, err, now_secs, base.is_some());
    agg.fold_agg(&conv.overlay);
    if conv.rc != 0 {
        if close_active {
            let close = agg.close(stage, "1", inputs.dot_verbose, now_secs);
            let _ = out.write_all(&close);
        }
        if let (Some(base), Some(snapshot)) = (base, snapshot.as_ref()) {
            restore_generation(inputs, base, snapshot, &conv.entries, err);
        }
        return SyncDone {
            rc: 1,
            frozen: true,
            entries: conv.entries,
        };
    }
    // Lifecycle prepare records the post-converge set; without
    // profiles it keeps `prior` and succeeds (the shell returns
    // before touching the ledger).
    let prepared = crate::profile_lifecycle::prepare(
        &crate::profile_lifecycle::PrepareInputs {
            present: false,
            extensions_enabled: false,
            eligible: &[],
            phase_one: &[],
            active: &[],
            prior: &[],
            ledger: None,
            home: inputs.home,
            euid: inputs.euid,
            log: inputs.log,
        },
        err,
    );
    if !prepared.succeeded {
        if close_active {
            let close = agg.close(stage, "1", inputs.dot_verbose, now_secs);
            let _ = out.write_all(&close);
        }
        if let (Some(base), Some(snapshot)) = (base, snapshot.as_ref()) {
            restore_generation(inputs, base, snapshot, &conv.entries, err);
        }
        return SyncDone {
            rc: 1,
            frozen: true,
            entries: conv.entries,
        };
    }
    if close_active {
        let close = agg.close(stage, "0", inputs.dot_verbose, now_secs);
        let _ = out.write_all(&close);
    }
    SyncDone {
        rc: 0,
        frozen: false,
        entries: conv.entries,
    }
}

/// Overlay-only pull for a converge phase (the shell
/// `_pull_overlays` with the ambient `OVERLAYS`).
#[allow(clippy::too_many_arguments)]
fn pull_overlays_only(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    moves: &mut crate::temp::MoveCache,
    candidate: &crate::repos_pull_queries::CandidateEnv,
    base: Option<&Base>,
    entries: &[String],
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    progress_done: &str,
    progress_total: &str,
) -> crate::repos_pull_fleet::PullOverlaysOutcome {
    // The overlay lanes need a base for the restore walk; without
    // one the missing topology reads untracked, like the shell's
    // failing `_base_git` (empty pulls return before touching it).
    let fallback;
    let base = match base {
        Some(base) => base,
        None => {
            fallback = Base {
                topology: crate::repos_base::Topology::Missing,
                client_git_dir: format!("{}/.dotfiles", inputs.home),
                home: inputs.home.to_string(),
            };
            &fallback
        }
    };
    crate::repos_pull_fleet::pull_overlays(
        &crate::repos_pull_fleet::PullOverlaysInputs {
            entries,
            extra_args: inputs.extra_args,
            home: inputs.home,
            ui_total: Some("5"),
            dot_quiet: inputs.dot_quiet,
            dot_verbose: inputs.dot_verbose,
            update_jobs: inputs.update_jobs,
            progress_done: Some(progress_done),
            progress_total: Some(progress_total),
            bar_width: inputs.bar_width,
            palette: inputs.palette,
            multibyte: inputs.multibyte,
            ascii: inputs.ascii,
            candidate,
            base,
            quarantine: None,
            overlays: entries,
            dest: inputs.dest,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            euid: inputs.euid,
            source_root: inputs.source_root_git,
            tmp: inputs.tmp,
            tool: inputs.tool,
            log: inputs.log,
        },
        stage,
        moves,
        out,
        err,
    )
}

/// `_dot_converge_overlays` for the profiles-absent envelope:
/// discover, preflight, pre-sync reconcile, the eligible pull
/// phase, rediscovery, and the active set. The shell always
/// rediscovers before returning, even on a failed phase. The
/// deferred close renders once in [`sync_tail`], so this only
/// reports the phase status, the current set, and the overlay
/// counts.
fn converge_overlays(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    moves: &mut crate::temp::MoveCache,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
    based: bool,
) -> ConvergeOut {
    use std::io::Write as _;
    let fail = |entries: Vec<String>, overlay: Agg| ConvergeOut {
        rc: 1,
        entries,
        overlay,
    };
    let mut overlay = Agg::zero();
    let mut dstate = crate::overlays::State::default();
    if discover_active(inputs, &mut dstate, err).is_err() {
        return fail(Vec::new(), overlay);
    }
    let mut entries = use_set(&mut dstate, "eligible");
    let mut preflight_state = crate::overlays::State {
        overlays: entries.clone(),
        ..Default::default()
    };
    if let Err(warning) = crate::overlays::preflight(&mut preflight_state, inputs.home) {
        err.extend_from_slice(warning.as_bytes());
        err.push(b'\n');
        return fail(entries, overlay);
    }
    if pre_sync_empty(inputs, &entries).is_err() {
        return fail(entries, overlay);
    }
    // The eligible pull phase: the shell bumps `DONE` past the
    // base row first (a fresh process starts at zero without
    // one), refreshing the deferred detail while any git-synced
    // overlay is eligible.
    let count = pull_overlay_count(&entries);
    if count > 0 {
        let detail = crate::progress_ui::progress_detail(
            b"overlays",
            2,
            1 + count,
            inputs.bar_width,
            inputs.ascii,
            inputs.multibyte,
        );
        let _ = out.write_all(&stage.update(&detail, now_secs, inputs.dot_verbose));
    }
    let candidate = pull_candidate(inputs, &entries);
    let outcome = pull_overlays_only(
        inputs,
        stage,
        moves,
        &candidate,
        inputs.base.filter(|found| found.exists()),
        &entries,
        out,
        err,
        if based { "1" } else { "0" },
        &(1 + count).to_string(),
    );
    overlay.fold_overlay(&outcome);
    let failed = outcome.tally.failed;
    let phase_ok = crate::update::overlay_phase_ok(outcome.rc, Some(&failed.to_string()));
    // Rediscover before returning, even on a failed phase.
    if discover_active(inputs, &mut dstate, err).is_err() {
        return fail(entries, overlay);
    }
    entries = use_set(&mut dstate, "active");
    ConvergeOut {
        rc: if phase_ok { 0 } else { 1 },
        entries,
        overlay,
    }
}

/// Discover the eligible or active set into `entries`, mirroring
/// `_discover_overlays` plus `_dot_overlay_use_set`.
fn use_set(state: &mut crate::overlays::State, kind: &str) -> Vec<String> {
    let _ = crate::overlays::use_set(state, kind);
    state.overlays.clone()
}

/// Run `_discover_overlays` natively (legacy path; profiles stay
/// shell-backed through [`should_go_native`]).
fn discover_active(
    inputs: &EngineInputs<'_>,
    state: &mut crate::overlays::State,
    err: &mut Vec<u8>,
) -> Result<(), ()> {
    let xdg_config = if inputs.config_home.is_empty() {
        String::new()
    } else {
        inputs.config_home.to_string()
    };
    let conf_dir = crate::overlays::conf_dir(&xdg_config, inputs.home);
    let conf_path = match conf_dir {
        Some(dir) if Path::new(&dir).is_dir() => dir,
        _ => return Ok(()),
    };
    let prefix = std::env::var("PREFIX").unwrap_or_default();
    let discover_inputs = crate::overlays::Inputs {
        home: inputs.home.to_string(),
        xdg_config,
        discovery_silent: false,
        profiles_present: false,
        selected: Vec::new(),
        platform: crate::platform::detect_platform().ok(),
        termux: crate::hook_api::is_termux(&prefix),
        host: crate::platform::detect_host().ok(),
        euid: inputs.euid,
    };
    let matches = crate::overlays::MatchInputs {
        platform: crate::platform::detect_platform().ok(),
        termux: crate::hook_api::is_termux(&prefix),
        host: crate::platform::detect_host().ok(),
    };
    match crate::overlays::discover(state, Path::new(&conf_path), "", &discover_inputs, &matches) {
        Ok(()) => Ok(()),
        Err(error) => {
            err.extend_from_slice(format!("{error:?}\n").as_bytes());
            Err(())
        }
    }
}

/// Pre-sync reconcile gate: empty specs run nothing (the envelope
/// guarantees no `pre-sync.d`; a listing failure still fails).
fn pre_sync_empty(inputs: &EngineInputs<'_>, eligible: &[String]) -> Result<(), ()> {
    let trust = crate::extension_trust::Inputs {
        euid: inputs.euid,
        home: inputs.home.to_string(),
        extensions_dir: inputs.extensions_dir.to_string(),
        manifest: inputs.manifest.to_string(),
        retiring_root: String::new(),
    };
    match crate::pre_sync::specs(&trust, eligible) {
        Ok(found) if found.is_empty() => Ok(()),
        Ok(_) => Err(()),
        Err(_) => Err(()),
    }
}

/// Null lifecycle worker: retire short-circuits before running
/// anything without profiles, but the trait object still travels.
struct NullWorker;

impl crate::profile_lifecycle::WorkerRun for NullWorker {
    fn run(
        &mut self,
        _script: &Path,
        _result_dir: &Path,
        _result_file: &Path,
        _context: &Path,
        _token: &str,
    ) -> crate::profile_lifecycle::WorkerOutcome {
        crate::profile_lifecycle::WorkerOutcome {
            rc: 1,
            output: Vec::new(),
        }
    }
}

/// `_dot_update_skip_inputs`: the Tools/Configs warning close for
/// a failed input side.
fn skip_inputs_rows(stage: &mut Stage, out: &mut Vec<u8>, reason: &str, now_secs: i64) {
    use std::io::Write as _;
    let open = stage.start(
        b"Tools",
        Some(b"skipping configured dependencies"),
        now_secs,
        None,
    );
    let _ = out.write_all(&open);
    let close = stage.finish(
        b"warning",
        format!("{reason}; dependencies skipped").as_bytes(),
        now_secs,
    );
    let _ = out.write_all(&close);
    let open = stage.start(b"Configs", Some(b"skipping config hooks"), now_secs, None);
    let _ = out.write_all(&open);
    let close = stage.finish(
        b"warning",
        format!("{reason}; config hooks skipped").as_bytes(),
        now_secs,
    );
    let _ = out.write_all(&close);
}

/// `_dot_update_finalize` natively: checkpoint, link phase (or the
/// frozen preservation rows), lifecycle retire, the provider-none
/// tools stage, the empty merges close, lifecycle commit, worktree
/// normalize, and `_ui_done`. Returns the update status.
#[allow(clippy::too_many_arguments)]
pub fn finalize(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
    update_status: i32,
    frozen: bool,
    entries: &[String],
) -> i32 {
    use std::io::Write as _;
    let mut status = update_status;
    let mut inputs_ready = status == 0;
    let checkpoint = format!("{}/dot/provider-reexec-failed", inputs.state_home);
    if !crate::shdeps::consume_checkpoint(Path::new(&checkpoint), inputs.source_root_git) {
        let close = crate::progress_ui::done(
            inputs.palette,
            quiet(inputs),
            Some("1"),
            now_secs,
            now_secs,
            &reload_hint(inputs),
        );
        let _ = out.write_all(&close);
        return 1;
    }
    let base_prefix = inputs.base.as_ref().and_then(|base| base.git_prefix());
    crate::repos_config::ensure_repo_config(base_prefix.as_deref());
    if frozen {
        let open = stage.start(
            b"Overlays",
            Some(b"preserving installed overlay links"),
            now_secs,
            inputs.dot_verbose,
        );
        let _ = out.write_all(&open);
        let close = stage.finish(
            b"warning",
            b"profile resolution or repository sync failed",
            now_secs,
        );
        let _ = out.write_all(&close);
        status = 1;
        inputs_ready = false;
    } else {
        let link_inputs = crate::repos_link_all::Inputs {
            entries,
            home: inputs.home,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            update_jobs: inputs.update_jobs,
            ui_total: Some("5"),
            dot_verbose: inputs.dot_verbose,
            dot_quiet: inputs.dot_quiet,
            dest: inputs.dest,
            base: inputs.base,
            euid: inputs.euid,
            source_root_git: inputs.source_root_git,
            tmp: inputs.tmp,
            tool: inputs.tool,
            palette: inputs.palette,
            multibyte: inputs.multibyte,
            bar_width: inputs.bar_width,
            log: inputs.log,
        };
        let outcome = crate::repos_link_all::link_overlays(&link_inputs, stage, out, err, now_secs);
        if outcome.rc != 0 {
            status = 1;
            inputs_ready = false;
        }
    }
    if !inputs_ready {
        skip_inputs_rows(stage, out, "repository synchronization failed", now_secs);
    } else {
        let mut worker = NullWorker;
        let retired = crate::profile_lifecycle::retire(
            &crate::profile_lifecycle::RetireInputs {
                present: false,
                extensions_enabled: false,
                retained: &[],
                eligible: &[],
                home: inputs.home,
                euid: inputs.euid,
                tmpdir: inputs.tmp,
                now_secs,
                verbose: inputs.flags.verbose,
                log: inputs.log,
            },
            &mut worker,
            out,
            err,
        );
        if retired != 0 {
            status = 1;
            skip_inputs_rows(stage, out, "profile deactivation failed", now_secs);
        } else {
            // The shdeps provider stays shell-backed (see
            // `should_go_native`); `none` renders its stage here.
            let open = stage.start(
                b"Tools",
                Some(b"checking configured dependencies"),
                now_secs,
                inputs.dot_verbose,
            );
            let _ = out.write_all(&open);
            let close = stage.finish(b"ok", b"no dependency provider", now_secs);
            let _ = out.write_all(&close);
            // Merge hooks are absent by envelope, so the driver
            // renders the empty close directly.
            let open = stage.start(
                b"Configs",
                Some(b"checking config hooks"),
                now_secs,
                inputs.dot_verbose,
            );
            let _ = out.write_all(&open);
            let close = stage.finish(b"ok", b"no config hooks", now_secs);
            let _ = out.write_all(&close);
        }
    }
    if inputs_ready && status == 0 {
        let committed = crate::profile_lifecycle::commit(&crate::profile_lifecycle::CommitInputs {
            present: false,
            extensions_enabled: false,
            retained: &[],
            eligible: &[],
            active: &[],
            ledger: None,
            home: inputs.home,
            euid: inputs.euid,
        });
        if !committed {
            warn_row(
                err,
                inputs.palette,
                "  warning: could not commit profile lifecycle state",
            );
            status = 1;
        }
    }
    let based = inputs.base.is_some_and(|base| base.exists());
    if based {
        let open = stage.start(
            b"Cleanup",
            Some(b"normalizing worktree"),
            now_secs,
            inputs.dot_verbose,
        );
        let _ = out.write_all(&open);
        crate::repos_dirty::normalize_filtered(base_prefix.as_deref(), entries);
        let close = stage.finish(b"ok", b"worktree normalized", now_secs);
        let _ = out.write_all(&close);
    } else {
        let open = stage.start(
            b"Cleanup",
            Some(b"normalizing worktree"),
            now_secs,
            inputs.dot_verbose,
        );
        let _ = out.write_all(&open);
        let close = stage.finish(b"ok", b"no base repo", now_secs);
        let _ = out.write_all(&close);
    }
    let close = crate::progress_ui::done(
        inputs.palette,
        quiet(inputs),
        Some(&status.to_string()),
        now_secs,
        now_secs,
        &reload_hint(inputs),
    );
    let _ = out.write_all(&close);
    status
}

/// Effective quiet for rows the shell gates on `DOT_QUIET` (the
/// `--quiet`/`--cron` flag exports join the variable here).
fn quiet(inputs: &EngineInputs<'_>) -> bool {
    inputs.flags.quiet || inputs.flags.cron || crate::log::is_quiet(inputs.dot_quiet)
}

/// `_ui_shell_reload_hint` inputs from the live environment.
fn reload_hint(inputs: &EngineInputs<'_>) -> Vec<u8> {
    let reloads = std::env::var("DOT_UPDATE_RELOADS_SHELL").ok();
    let shell_name = std::env::var("SHELL").ok().and_then(|shell| {
        Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    });
    let home = Path::new(inputs.home);
    crate::progress_ui::reload_hint(
        reloads.as_deref(),
        shell_name.as_deref(),
        home.join(".bashrc").exists(),
        home.join(".zshrc").exists(),
    )
}

/// Owned ambient capture for [`run_update`]: everything the shell
/// adapter preamble exports (`constants.sh` defaults plus the flag
/// loop) resolved before the first stage opens. [`Gathered::inputs`]
/// from here, so one value lives through the whole run.
pub struct Gathered {
    flags: UpdateFlags,
    extra: Vec<std::ffi::OsString>,
    home: String,
    config_home: String,
    state_home: String,
    manifest: String,
    legacy_manifest: String,
    dest: crate::repos_overlays::DestinationInputs,
    tool: crate::temp::MoveTool,
    log: crate::log::Log,
    palette: crate::progress_ui::Palette,
    base: Option<crate::repos_base::Base>,
    bar_width: String,
    dot_verbose: Option<String>,
    dot_quiet: Option<String>,
    update_jobs: Option<String>,
    provider: String,
    skip_provider: bool,
    live: bool,
    multibyte: bool,
    ascii: bool,
    euid: u32,
    tmp: std::path::PathBuf,
    source_root_git: std::path::PathBuf,
    checkout_root: String,
    extensions_dir: String,
}

impl Gathered {
    /// Borrow the driver inputs from this capture.
    pub fn inputs(&self) -> EngineInputs<'_> {
        EngineInputs {
            flags: self.flags,
            extra_args: &self.extra,
            base: self.base.as_ref(),
            entries: &[],
            home: &self.home,
            config_home: &self.config_home,
            state_home: &self.state_home,
            dest: &self.dest,
            manifest: &self.manifest,
            legacy_manifest: &self.legacy_manifest,
            euid: self.euid,
            source_root_git: &self.source_root_git,
            tmp: &self.tmp,
            tool: &self.tool,
            log: &self.log,
            palette: &self.palette,
            dot_verbose: self.dot_verbose.as_deref(),
            dot_quiet: self.dot_quiet.as_deref(),
            provider: &self.provider,
            skip_provider: self.skip_provider,
            update_jobs: self.update_jobs.as_deref(),
            live: self.live,
            multibyte: self.multibyte,
            ascii: self.ascii,
            bar_width: &self.bar_width,
            extensions_dir: &self.extensions_dir,
            checkout_root: &self.checkout_root,
        }
    }
}

/// Non-empty environment value (unset and empty read the same,
/// like `${VAR:-}` defaults).
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Parse the `_dot_update` leading-flag loop: `--cron`, `--quiet`,
/// `-f`/`--force`, `-v`/`--verbose` consume left to right; the
/// first anything else (including lone `-`) ends the loop and the
/// residue passes through to the sync phases as extra args.
fn parse_flags(args: &[std::ffi::OsString]) -> (UpdateFlags, Vec<std::ffi::OsString>) {
    let mut flags = UpdateFlags::default();
    let mut extra = Vec::new();
    let mut positional = false;
    for arg in args {
        let word = if positional { None } else { arg.to_str() };
        match word {
            Some("--cron") => flags.cron = true,
            Some("--quiet") => flags.quiet = true,
            Some("-f") | Some("--force") => flags.force = true,
            Some("-v") | Some("--verbose") => flags.verbose = true,
            Some(text) if text.starts_with('-') && text.len() > 1 => {
                positional = true;
                extra.push(arg.clone());
            }
            _ => {
                positional = true;
                extra.push(arg.clone());
            }
        }
    }
    (flags, extra)
}

/// Effective uid for the trust checks: `$EUID` when numeric (the
/// shell loop runs under bash), else the `id -u` equivalent. `None`
/// fails closed to the shell adapter — the checks must never run
/// under a guessed identity.
fn resolve_euid() -> Option<u32> {
    if let Some(euid) = env_value("EUID").and_then(|value| value.parse::<u32>().ok()) {
        return Some(euid);
    }
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse::<u32>().ok())
}

/// Locale for the ASCII probe: `${LC_ALL:-${LC_CTYPE:-${LANG:-}}}`,
/// like `_ui_ascii_mode`.
fn locale_name() -> String {
    env_value("LC_ALL")
        .or_else(|| env_value("LC_CTYPE"))
        .or_else(|| env_value("LANG"))
        .unwrap_or_default()
}

/// Capture the adapter ambient for one native run: flag exports
/// first (the shell loop exports before `_ui_begin`, so rows and
/// children read them), then every `constants.sh` default the
/// driver consumes. `state_home` is the resolved XDG state dir from
/// the caller (already trampoline-normalized); `source_root` is
/// `$DOT_SOURCE_ROOT`. Returns `None` whenever the surroundings
/// cannot support the native envelope — the caller runs the shell
/// adapter instead.
pub fn gather(
    args: &[std::ffi::OsString],
    source_root: &std::path::Path,
    state_home: &str,
) -> Option<Gathered> {
    use std::io::IsTerminal as _;
    let (flags, extra) = parse_flags(args);
    // Mirror the flag-loop exports (single-flight command entry,
    // like the lock-token publish in `update_run::run`).
    unsafe {
        if flags.cron || flags.quiet {
            std::env::set_var("DOT_QUIET", "1");
        }
        if flags.force {
            std::env::set_var("DOT_FORCE", "1");
        }
        if flags.verbose {
            std::env::set_var("DOT_VERBOSE", "1");
        }
    }
    let home = env_value("HOME")?;
    if !home.starts_with('/') {
        return None;
    }
    let config_home = env_value("XDG_CONFIG_HOME").unwrap_or_else(|| format!("{home}/.config"));
    let manifest = env_value("DOT_OVERLAY_MANIFEST")
        .unwrap_or_else(|| format!("{state_home}/dot/overlay-links"));
    let legacy_manifest = env_value("DOT_OVERLAY_LEGACY_MANIFEST")
        .unwrap_or_else(|| format!("{home}/.local/state/dot/overlay-links"));
    let mut moves = crate::temp::MoveCache::default();
    let tool = moves.tool().ok()?;
    let stdout_tty = std::io::stdout().is_terminal();
    let no_color = env_value("NO_COLOR");
    let no_color_ref = no_color.as_deref().filter(|value| !value.is_empty());
    let colored = stdout_tty && no_color_ref.is_none();
    let palette = if colored {
        crate::progress_ui::Palette {
            reset: "\x1b[0m".to_string(),
            bold: "\x1b[1m".to_string(),
            dim: "\x1b[0;90m".to_string(),
            green: "\x1b[32m".to_string(),
            yellow: "\x1b[33m".to_string(),
            red: "\x1b[31m".to_string(),
            blue: "\x1b[34m".to_string(),
            cyan: "\x1b[36m".to_string(),
            white: "\x1b[38;2;255;255;255m".to_string(),
        }
    } else {
        crate::progress_ui::Palette::empty()
    };
    let dot_quiet = env_value("DOT_QUIET");
    let dot_verbose = env_value("DOT_VERBOSE");
    let log = crate::log::Log::from_env(stdout_tty, no_color.as_deref(), dot_quiet.as_deref());
    let quiet = flags.quiet || flags.cron || crate::log::is_quiet(dot_quiet.as_deref());
    let live = crate::progress_ui::live_enabled(
        quiet,
        stdout_tty,
        env_value("DOT_UI_FORCE_LIVE").as_deref(),
    );
    let locale = locale_name();
    let multibyte = crate::progress_ui::utf8_locale(&locale);
    let ascii =
        crate::progress_ui::ascii_mode(env_value("DOT_UI_ASCII").as_deref(), &locale, multibyte);
    let mut provider = env_value("DOT_DEPENDENCY_PROVIDER").unwrap_or_else(|| "none".to_string());
    let skip_provider = env_value("DOT_INIT_SKIP_PROVIDER").as_deref() == Some("1");
    if skip_provider {
        provider = "none".to_string();
    }
    let pwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| home.clone());
    let dest = crate::repos_overlays::DestinationInputs {
        home: home.clone(),
        xdg_state_home: env_value("XDG_STATE_HOME"),
        install_dir: env_value("SHDEPS_INSTALL_DIR"),
        state_dir: env_value("SHDEPS_STATE_DIR"),
        overlay_paths: Vec::new(),
        init_backup: env_value("DOT_INIT_BACKUP").filter(|value| value != "-"),
        pwd,
    };
    Some(Gathered {
        flags,
        extra,
        home: home.clone(),
        config_home,
        state_home: state_home.to_string(),
        manifest,
        legacy_manifest,
        dest,
        tool,
        log,
        palette,
        base: Some(crate::cli::base_from_env(&home)),
        bar_width: env_value("DOT_UI_PROGRESS_WIDTH").unwrap_or_else(|| "8".to_string()),
        dot_verbose,
        dot_quiet,
        update_jobs: env_value("DOT_UPDATE_JOBS"),
        provider,
        skip_provider,
        live,
        multibyte,
        ascii,
        euid: resolve_euid()?,
        tmp: std::env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp")),
        source_root_git: source_root.to_path_buf(),
        checkout_root: source_root.to_str()?.to_string(),
        extensions_dir: env_value("DOT_EXTENSIONS_DIR").unwrap_or_default(),
    })
}

/// Wall-clock seconds for stage rows (`date +%s` equivalent).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// `_dot_update` natively: flag-driven stages around [`sync_repos`]
/// and [`finalize`] with the defensive policy reload between them.
/// Returns `None` outside the [`should_go_native`] envelope (the
/// caller runs the shell adapter instead).
pub fn run_update(
    inputs: &EngineInputs<'_>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
) -> Option<i32> {
    use std::io::Write as _;
    if should_go_native(inputs).is_err() {
        return None;
    }
    // `_ui_begin 5`: the update always runs counted (the assignment
    // overwrites any ambient total, like the shell).
    let mut stage = Stage::begin(
        inputs.palette.clone(),
        "5",
        quiet(inputs),
        inputs.live,
        inputs.multibyte,
        inputs.ascii,
    );
    let mut moves = crate::temp::MoveCache::default();
    let sync = sync_repos(inputs, &mut stage, &mut moves, out, err, now_secs);
    if sync.rc != 0 {
        let rc = finalize(
            inputs,
            &mut stage,
            out,
            err,
            now_secs,
            1,
            sync.frozen,
            &sync.entries,
        );
        return Some(rc);
    }
    // Defensive reload before provider selection continues (a
    // failure closes without finalizing, like the shell: the
    // loader prints its own diagnostic, then `_ui_done 1`).
    if let Err(failure) = crate::startup::check_ambient() {
        err.extend_from_slice(failure.line().as_bytes());
        err.push(b'\n');
        let close = crate::progress_ui::done(
            inputs.palette,
            quiet(inputs),
            Some("1"),
            now_secs,
            now_secs,
            &reload_hint(inputs),
        );
        let _ = out.write_all(&close);
        return Some(1);
    }
    let rc = finalize(
        inputs,
        &mut stage,
        out,
        err,
        now_secs,
        0,
        sync.frozen,
        &sync.entries,
    );
    Some(rc)
}

/// `IFS='|' read -r name path url _ _ sync`: six fields, the
/// remainder collapsing into the last like the shell builtin.
fn split_entry(entry: &str) -> (String, String, String, String) {
    let mut parts = entry.splitn(6, '|');
    let name = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let url = parts.next().unwrap_or("").to_string();
    let _ = parts.next();
    let _ = parts.next();
    let sync = parts.next().unwrap_or("").to_string();
    (name, path, url, sync)
}

/// `_pull_overlay_count`: git-synced entries that are already
/// worktrees or name a remote (the progress-total population).
fn pull_overlay_count(entries: &[String]) -> i64 {
    let mut count = 0;
    for entry in entries {
        let (_, path, url, sync) = split_entry(entry);
        let sync = if sync.is_empty() {
            "git"
        } else {
            sync.as_str()
        };
        if sync != "git" {
            continue;
        }
        if crate::overlays::is_worktree(Path::new(&path)) || !url.is_empty() {
            count += 1;
        }
    }
    count
}

/// Candidate validation environment shared by the pull phases
/// (the shell rebuilds these from the same globals each time).
fn candidate_env(
    inputs: &EngineInputs<'_>,
    overlay_paths: Vec<String>,
) -> crate::repos_pull_queries::CandidateEnv {
    let install_root = inputs
        .dest
        .install_dir
        .clone()
        .unwrap_or_else(|| format!("{}/.local/share", inputs.home));
    crate::repos_pull_queries::CandidateEnv {
        home: inputs.home.to_string(),
        checkout: format!("{install_root}/cgraf78/dot"),
        pwd: inputs.dest.pwd.clone(),
        source_root: inputs.checkout_root.to_string(),
        state_home: inputs.state_home.to_string(),
        install_root,
        provider_state: inputs
            .dest
            .state_dir
            .clone()
            .unwrap_or_else(|| format!("{}/shdeps", inputs.state_home)),
        overlay_paths,
        init_backup: inputs.dest.init_backup.clone(),
    }
}

/// Quarantine support from the installed-link snapshot (the shell
/// quarantines whenever the rollback maps exist, which is every
/// base run — empty on a fresh client).
fn quarantine_inputs(
    inputs: &EngineInputs<'_>,
    snapshot: &InstalledSnapshot,
) -> crate::repos_overlays::QuarantineInputs {
    crate::repos_overlays::QuarantineInputs {
        snapshot: crate::repos_overlays::RollbackSnapshot {
            paths: snapshot.rels.clone(),
            targets: snapshot.targets.clone(),
        },
        context: inputs.dest.clone(),
        tool: inputs.tool.clone(),
        source_root: inputs.source_root_git.to_path_buf(),
    }
}
