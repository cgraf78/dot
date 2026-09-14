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
//! Every `update` and `pull` invocation runs this engine. Configuration is
//! parsed before entry, so the provider is a closed enum rather than an
//! open-ended shell value that needs a fallback lane.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
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
    /// Immutable process boundary for leaf workers launched during this update.
    pub runtime: &'a crate::app::Runtime,
    /// Native update lock claim exposed only to transactional hook workers.
    pub update_lock_token: Option<&'a str>,
    /// Parsed client configuration for this update generation.
    pub config: &'a crate::config::Config,
    /// Parsed flags.
    pub flags: UpdateFlags,
    /// Residue after flags (forwarded to the pull phases).
    pub extra_args: &'a [std::ffi::OsString],
    /// Original update arguments, preserved for a provider-triggered re-entry.
    pub original_args: &'a [std::ffi::OsString],
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
    /// `DOT_MERGE_JOBS`: merge-hook parallel bound, else update jobs.
    pub merge_jobs: Option<&'a str>,
    /// `DOT_VERBOSE` (in addition to the flag; either enables).
    pub dot_verbose: Option<&'a str>,
    /// `DOT_QUIET` (in addition to the flag; either quiets).
    pub dot_quiet: Option<&'a str>,
    /// `DOT_UI_PROGRESS_WIDTH`: bar width, default `"8"`.
    pub bar_width: &'a str,
    /// `DOT_INIT_SKIP_PROVIDER == 1`.
    pub skip_provider: bool,
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
    /// `$PREFIX` for Termux overlay matching.
    pub prefix: &'a str,
    /// `DOT_UPDATE_RELOADS_SHELL` for the final reload hint.
    pub reloads_shell: Option<&'a str>,
    /// `$SHELL` for the final reload hint.
    pub shell: Option<&'a str>,
    /// `DOT_SHDEPS_UPDATE_POLICY` for the defensive config reload.
    pub shdeps_update_policy: Option<&'a str>,
    /// `DOT_REEXEC_EXPECTED_REVISION` for the defensive reload.
    pub reexec_expected: Option<&'a str>,
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
    /// One owner for the generation resolved during repository sync.
    state: UpdateState,
}

/// Profile-aware update data that must survive from repository sync through
/// linking, retirement, and ledger commit.
///
/// The shell stores these values in globals that are reset by each discovery
/// pass. Keeping them together makes the two-phase boundary explicit: base
/// policy selects phase one, a refreshed base resolves selectors, and only the
/// final set reaches lifecycle hooks and the link phase.
#[derive(Debug)]
struct UpdateState {
    /// Configuration reloaded after the accepted base generation.
    config: crate::config::Config,
    /// Profile parsing and selector result for this generation.
    profiles: crate::profiles::State,
    /// Names selected by the final profile expansion.
    selected: Vec<String>,
    /// Final eligible overlay names used by lifecycle retention policy.
    eligible_names: Vec<String>,
    /// Final active overlay records, used for links and cleanup.
    active: Vec<String>,
    /// Ledger records loaded before lifecycle preparation.
    prior: Vec<String>,
    /// Prepared ledger records retained through retirement and commit.
    retained: Vec<String>,
    /// Active records from the base-only discovery pass.
    phase_one: Vec<String>,
    /// Overlay identities from the base-only pass, used to isolate additions.
    /// Descriptor changes do not make an already-pulled overlay an addition.
    phase_one_names: BTreeSet<String>,
}

/// Mutable update streams shared by the sync, converge, and finalize phases.
/// Keeping the paired streams together prevents orchestration signatures from
/// growing a positional stdout/stderr tail.
struct UpdateIo<'a> {
    out: &'a mut Vec<u8>,
    err: &'a mut Vec<u8>,
}

impl UpdateState {
    fn new(config: crate::config::Config) -> Self {
        Self {
            config,
            profiles: crate::profiles::State::default(),
            selected: Vec::new(),
            eligible_names: Vec::new(),
            active: Vec::new(),
            prior: Vec::new(),
            retained: Vec::new(),
            phase_one: Vec::new(),
            phase_one_names: BTreeSet::new(),
        }
    }

    /// Publish exactly one discovery pass. Callers preserve `phase_one` before
    /// the selector refresh and overwrite the remaining records only after the
    /// final discovery, matching the shell's resettable global arrays.
    fn capture(&mut self, overlays: &crate::overlays::State) {
        self.selected = overlays.selected.clone();
        self.eligible_names = overlays.eligible_names.clone();
        self.active = overlays.active.clone();
    }

    fn ledger(&self, inputs: &EngineInputs<'_>) -> std::path::PathBuf {
        Path::new(inputs.state_home).join("dot/profile-overlay-lifecycle-v1")
    }

    fn extensions_enabled(&self) -> bool {
        crate::config::extensions_enabled(&self.config)
    }
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
    state: UpdateState,
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
            UpdateState::new(inputs.config.clone()),
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
                state: UpdateState::new(inputs.config.clone()),
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
            state: UpdateState::new(inputs.config.clone()),
        };
    }
    // A base pull may replace policy: reload before either phase
    // resolves or any transport preparation runs (the loader
    // prints its own diagnostic on failure).
    let startup = startup_inputs(inputs);
    let config = match crate::startup::preflight(&startup) {
        Ok(config) => config,
        Err(failure) => {
            err.extend_from_slice(failure.line().as_bytes());
            err.push(b'\n');
            let close = Agg::base(&outcome).close(stage, "1", inputs.dot_verbose, now_secs);
            let _ = out.write_all(&close);
            restore_generation(inputs, base, &snapshot, &[], err);
            return SyncDone {
                rc: 1,
                frozen: true,
                state: UpdateState::new(inputs.config.clone()),
            };
        }
    };
    sync_tail(
        inputs,
        UpdateState::new(config),
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
    state: UpdateState,
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
    let mut io = UpdateIo { out, err };
    let mut conv = converge_overlays(inputs, state, stage, moves, &mut io, now_secs);
    agg.fold_agg(&conv.overlay);
    if conv.rc != 0 {
        if close_active {
            let close = agg.close(stage, "1", inputs.dot_verbose, now_secs);
            let _ = io.out.write_all(&close);
        }
        if let (Some(base), Some(snapshot)) = (base, snapshot.as_ref()) {
            restore_generation(inputs, base, snapshot, &conv.state.active, io.err);
        }
        return SyncDone {
            rc: 1,
            frozen: true,
            state: conv.state,
        };
    }
    // Lifecycle preparation owns the freshly resolved profile state, not the
    // invocation's pre-pull config. Its retained records must be the same
    // values later handed to retirement and commit.
    let ledger = conv.state.ledger(inputs);
    let prepared = crate::profile_lifecycle::prepare(
        &crate::profile_lifecycle::PrepareInputs {
            present: conv.state.profiles.present,
            extensions_enabled: conv.state.extensions_enabled(),
            eligible: &conv.state.eligible_names,
            phase_one: &conv.state.phase_one,
            active: &conv.state.active,
            prior: &conv.state.prior,
            ledger: Some(&ledger),
            home: inputs.home,
            euid: inputs.euid,
            log: inputs.log,
        },
        io.err,
    );
    let prepared_ok = prepared.succeeded;
    conv.state.prior = prepared.prior;
    conv.state.retained = prepared.records;
    if !prepared_ok {
        if close_active {
            let close = agg.close(stage, "1", inputs.dot_verbose, now_secs);
            let _ = io.out.write_all(&close);
        }
        if let (Some(base), Some(snapshot)) = (base, snapshot.as_ref()) {
            restore_generation(inputs, base, snapshot, &conv.state.active, io.err);
        }
        return SyncDone {
            rc: 1,
            frozen: true,
            state: conv.state,
        };
    }
    if close_active {
        let close = agg.close(stage, "0", inputs.dot_verbose, now_secs);
        let _ = io.out.write_all(&close);
    }
    SyncDone {
        rc: 0,
        frozen: false,
        state: conv.state,
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

/// `_dot_converge_overlays` before profile selection: discover,
/// preflight, pre-sync reconcile, the eligible pull phase,
/// rediscovery, and the active set. Profile-aware runs hand off to
/// [`converge_profiles`] after loading policy. The shell always
/// rediscovers before returning, even on a failed phase. The
/// deferred close renders once in [`sync_tail`], so this only
/// reports the phase status, the current set, and the overlay
/// counts.
fn converge_overlays(
    inputs: &EngineInputs<'_>,
    mut update: UpdateState,
    stage: &mut Stage,
    moves: &mut crate::temp::MoveCache,
    io: &mut UpdateIo<'_>,
    now_secs: i64,
) -> ConvergeOut {
    use std::io::Write as _;
    let fail = |state: UpdateState, overlay: Agg| ConvergeOut {
        rc: 1,
        state,
        overlay,
    };
    let mut overlay = Agg::zero();
    if let Err(error) = update.profiles.load_default(
        inputs.config_home,
        inputs.home,
        Some(&update.config.default_profile),
    ) {
        io.err
            .extend_from_slice(format!("dot: profile: {}\n", error.message).as_bytes());
        return fail(update, overlay);
    }
    if update.profiles.present {
        return converge_profiles(inputs, stage, moves, io, now_secs, update, overlay);
    }
    let mut dstate = crate::overlays::State::default();
    if discover_active(inputs, &mut dstate, io.err).is_err() {
        return fail(update, overlay);
    }
    let entries = use_set(&mut dstate, "eligible");
    update.capture(&dstate);
    let mut preflight_state = crate::overlays::State {
        overlays: entries.clone(),
        ..Default::default()
    };
    if let Err(warning) = crate::overlays::preflight(&mut preflight_state, inputs.home) {
        io.err.extend_from_slice(warning.as_bytes());
        io.err.push(b'\n');
        update.active = entries;
        return fail(update, overlay);
    }
    if pre_sync(
        inputs,
        update.config.extensions_dir.as_deref(),
        &entries,
        "reconcile",
        io.out,
        io.err,
    )
    .is_err()
    {
        update.active = entries;
        return fail(update, overlay);
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
        let _ = io
            .out
            .write_all(&stage.update(&detail, now_secs, inputs.dot_verbose));
    }
    let candidate = pull_candidate(inputs, &entries);
    let outcome = pull_overlays_only(
        inputs,
        stage,
        moves,
        &candidate,
        inputs.base.filter(|found| found.exists()),
        &entries,
        io.out,
        io.err,
        if inputs.base.is_some_and(|base| base.exists()) {
            "1"
        } else {
            "0"
        },
        &(1 + count).to_string(),
    );
    overlay.fold_overlay(&outcome);
    let failed = outcome.tally.failed;
    let phase_ok = crate::update::overlay_phase_ok(outcome.rc, Some(&failed.to_string()));
    // Rediscover before returning, even on a failed phase.
    if discover_active(inputs, &mut dstate, io.err).is_err() {
        update.active = entries;
        return fail(update, overlay);
    }
    let _ = use_set(&mut dstate, "active");
    update.capture(&dstate);
    ConvergeOut {
        rc: if phase_ok { 0 } else { 1 },
        state: update,
        overlay,
    }
}

/// Two-phase profile convergence. Phase one deliberately sees only `base`;
/// selector resolution waits until those repositories are refreshed, so a
/// personal selector added by the base pull cannot influence its own fetch.
fn converge_profiles(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    moves: &mut crate::temp::MoveCache,
    io: &mut UpdateIo<'_>,
    now_secs: i64,
    mut update: UpdateState,
    mut overlay: Agg,
) -> ConvergeOut {
    use std::io::Write as _;
    let fail = |state: UpdateState, overlay: Agg| ConvergeOut {
        rc: 1,
        state,
        overlay,
    };
    if let Err(error) = update.profiles.select_base() {
        io.err
            .extend_from_slice(format!("dot: profile: {}\n", error.message).as_bytes());
        return fail(update, overlay);
    }
    let mut state = crate::overlays::State::default();
    if discover_selected(inputs, &mut state, &update.profiles.overlay_names, io.err).is_err() {
        return fail(update, overlay);
    }
    update.phase_one_names = state
        .eligible
        .iter()
        .filter_map(|record| record_name(record).map(str::to_owned))
        .collect();
    update.phase_one = state.active.clone();
    update.capture(&state);
    let mut entries = use_set(&mut state, "eligible");
    let mut preflight_state = crate::overlays::State {
        overlays: entries.clone(),
        ..Default::default()
    };
    if let Err(warning) = crate::overlays::preflight(&mut preflight_state, inputs.home) {
        io.err.extend_from_slice(warning.as_bytes());
        io.err.push(b'\n');
        update.active = entries;
        return fail(update, overlay);
    }
    if pre_sync(
        inputs,
        update.config.extensions_dir.as_deref(),
        &entries,
        "prepare",
        io.out,
        io.err,
    )
    .is_err()
    {
        update.active = entries;
        return fail(update, overlay);
    }
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
        let _ = io
            .out
            .write_all(&stage.update(&detail, now_secs, inputs.dot_verbose));
    }
    let candidate = pull_candidate(inputs, &entries);
    let outcome = pull_overlays_only(
        inputs,
        stage,
        moves,
        &candidate,
        inputs.base.filter(|found| found.exists()),
        &entries,
        io.out,
        io.err,
        if inputs.base.is_some_and(|base| base.exists()) {
            "1"
        } else {
            "0"
        },
        &(1 + count).to_string(),
    );
    overlay.fold_overlay(&outcome);
    let phase_ok =
        crate::update::overlay_phase_ok(outcome.rc, Some(&outcome.tally.failed.to_string()));
    // Shell re-discovers the phase-one state even after a failed pull.
    if discover_selected(inputs, &mut state, &update.profiles.overlay_names, io.err).is_err() {
        update.active = entries;
        return fail(update, overlay);
    }
    let phase_one_active = state.active.clone();
    update.phase_one = phase_one_active.clone();
    update.capture(&state);
    if !phase_ok {
        let _ = use_set(&mut state, "active");
        update.capture(&state);
        return fail(update, overlay);
    }
    let user = match crate::profiles::current_user() {
        Some(user) => user,
        None => {
            io.err
                .extend_from_slice(b"dot: profile: cannot determine current user\n");
            update.active = entries;
            return fail(update, overlay);
        }
    };
    let host = match crate::platform::detect_host() {
        Ok(host) => host,
        Err(_) => {
            io.err
                .extend_from_slice(b"dot: profile: cannot determine current short hostname\n");
            update.active = entries;
            return fail(update, overlay);
        }
    };
    let phase_refs: Vec<&str> = phase_one_active.iter().map(String::as_str).collect();
    if let Err(error) = update.profiles.resolve_default(
        inputs.config_home,
        inputs.home,
        &phase_refs,
        &user,
        &host,
        inputs.euid,
    ) {
        io.err
            .extend_from_slice(format!("dot: profile: {}\n", error.message).as_bytes());
        update.active = entries;
        return fail(update, overlay);
    }
    if discover_selected(inputs, &mut state, &update.profiles.overlay_names, io.err).is_err() {
        update.active = entries;
        return fail(update, overlay);
    }
    entries = use_set(&mut state, "eligible");
    update.capture(&state);
    let mut preflight_state = crate::overlays::State {
        overlays: entries.clone(),
        ..Default::default()
    };
    if let Err(warning) = crate::overlays::preflight(&mut preflight_state, inputs.home) {
        io.err.extend_from_slice(warning.as_bytes());
        io.err.push(b'\n');
        update.active = entries;
        return fail(update, overlay);
    }
    if pre_sync(
        inputs,
        update.config.extensions_dir.as_deref(),
        &entries,
        "reconcile",
        io.out,
        io.err,
    )
    .is_err()
    {
        update.active = entries;
        return fail(update, overlay);
    }
    let additions: Vec<String> = entries
        .iter()
        .filter(|record| {
            !record_name(record).is_some_and(|name| update.phase_one_names.contains(name))
        })
        .cloned()
        .collect();
    let candidate = pull_candidate(inputs, &additions);
    let outcome = pull_overlays_only(
        inputs,
        stage,
        moves,
        &candidate,
        inputs.base.filter(|found| found.exists()),
        &additions,
        io.out,
        io.err,
        if inputs.base.is_some_and(|base| base.exists()) {
            "1"
        } else {
            "0"
        },
        &(1 + pull_overlay_count(&additions)).to_string(),
    );
    overlay.fold_overlay(&outcome);
    let additions_ok =
        crate::update::overlay_phase_ok(outcome.rc, Some(&outcome.tally.failed.to_string()));
    if discover_selected(inputs, &mut state, &update.profiles.overlay_names, io.err).is_err() {
        update.active = entries;
        return fail(update, overlay);
    }
    let _ = use_set(&mut state, "active");
    update.capture(&state);
    if additions_ok {
        ConvergeOut {
            rc: 0,
            state: update,
            overlay,
        }
    } else {
        fail(update, overlay)
    }
}

/// Discover the eligible or active set into `entries`, mirroring
/// `_discover_overlays` plus `_dot_overlay_use_set`.
fn use_set(state: &mut crate::overlays::State, kind: &str) -> Vec<String> {
    let _ = crate::overlays::use_set(state, kind);
    state.overlays.clone()
}

/// Run `_discover_overlays` natively for the profiles-absent branch.
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
    let discover_inputs = crate::overlays::Inputs {
        home: inputs.home.to_string(),
        xdg_config,
        discovery_silent: false,
        profiles_present: false,
        selected: Vec::new(),
        platform: crate::platform::detect_platform().ok(),
        termux: crate::hook_api::is_termux(inputs.prefix),
        host: crate::platform::detect_host().ok(),
        euid: inputs.euid,
    };
    let matches = crate::overlays::MatchInputs {
        platform: crate::platform::detect_platform().ok(),
        termux: crate::hook_api::is_termux(inputs.prefix),
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

/// Profile-aware discovery with a caller-owned selected list. The discovery
/// kernel still owns descriptor parsing, eligibility, active-state and its
/// diagnostics; this driver owns only which phase supplies the names.
fn discover_selected(
    inputs: &EngineInputs<'_>,
    state: &mut crate::overlays::State,
    selected: &[String],
    err: &mut Vec<u8>,
) -> Result<(), ()> {
    let xdg_config = inputs.config_home.to_string();
    let conf_dir = crate::overlays::conf_dir(&xdg_config, inputs.home);
    let conf_path = match conf_dir {
        Some(dir) if Path::new(&dir).is_dir() => dir,
        _ => return Ok(()),
    };
    let platform = crate::platform::detect_platform().ok();
    let host = crate::platform::detect_host().ok();
    let matches = crate::overlays::MatchInputs {
        platform: platform.clone(),
        termux: crate::hook_api::is_termux(inputs.prefix),
        host: host.clone(),
    };
    let discover_inputs = crate::overlays::Inputs {
        home: inputs.home.to_string(),
        xdg_config,
        discovery_silent: false,
        profiles_present: true,
        selected: selected.to_vec(),
        platform,
        termux: crate::hook_api::is_termux(inputs.prefix),
        host,
        euid: inputs.euid,
    };
    match crate::overlays::discover(state, Path::new(&conf_path), "", &discover_inputs, &matches) {
        Ok(()) => Ok(()),
        Err(error) => {
            err.extend_from_slice(format!("{error}\n").as_bytes());
            Err(())
        }
    }
}

/// Run pre-sync hooks through the same worker boundary as lifecycle hooks.
/// The listing and context protocol remain owned by `pre_sync`; this engine
/// layer supplies only the Runtime-bound process runner and warning stream.
fn pre_sync(
    inputs: &EngineInputs<'_>,
    configured_root: Option<&str>,
    eligible: &[String],
    stage: &str,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> Result<(), ()> {
    let extensions_dir = configured_root.unwrap_or(inputs.extensions_dir);
    let trust = crate::extension_trust::Inputs {
        euid: inputs.euid,
        home: inputs.home.to_string(),
        extensions_dir: extensions_dir.to_string(),
        manifest: inputs.manifest.to_string(),
        retiring_root: String::new(),
    };
    let records: Vec<Vec<u8>> = eligible
        .iter()
        .map(|record| record.as_bytes().to_vec())
        .collect();
    let mut worker = crate::hook_worker::Worker::for_update(
        inputs.runtime,
        &crate::hook_worker::UpdateEnvironment {
            extensions_dir,
            overlay_manifest: inputs.manifest,
            update_lock_token: inputs.update_lock_token,
            quiet: quiet(inputs),
            force: inputs.flags.force,
            verbose: inputs.flags.verbose || crate::log::is_quiet(inputs.dot_verbose),
        },
    );
    let mut runner = |call: &crate::pre_sync::Call| {
        let outcome = worker.pre_sync(call);
        out.extend_from_slice(&outcome.stdout);
        err.extend_from_slice(&outcome.stderr);
        outcome.rc == 0
    };
    match crate::pre_sync::run(stage, &records, &trust, eligible, inputs.tmp, &mut runner) {
        Ok(outcome) if outcome.status == 0 => Ok(()),
        Ok(outcome) => {
            for warning in outcome.warnings {
                inputs.log.warn(err, &warning);
            }
            Err(())
        }
        Err(crate::pre_sync::Error::Invalid(message)) => {
            err.extend_from_slice(message.as_bytes());
            err.push(b'\n');
            Err(())
        }
        Err(crate::pre_sync::Error::Usage | crate::pre_sync::Error::Refused) => Err(()),
    }
}

/// Overlay records are stable by name across a descriptor refresh. The shell
/// records `entry%%|*` in an associative set before choosing additions; using
/// the same identity prevents a changed URL or descriptor path from fetching
/// an already-processed overlay twice.
fn record_name(record: &str) -> Option<&str> {
    record
        .split_once('|')
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
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
fn finalize(
    inputs: &EngineInputs<'_>,
    state: &mut UpdateState,
    stage: &mut Stage,
    io: &mut UpdateIo<'_>,
    now_secs: i64,
    update_status: i32,
    frozen: bool,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> i32 {
    use std::io::Write as _;
    if cancelled() {
        return 1;
    }
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
        let _ = io.out.write_all(&close);
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
        let _ = io.out.write_all(&open);
        let close = stage.finish(
            b"warning",
            b"profile resolution or repository sync failed",
            now_secs,
        );
        let _ = io.out.write_all(&close);
        status = 1;
        inputs_ready = false;
    } else {
        let link_inputs = crate::repos_link_all::Inputs {
            entries: &state.active,
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
        let outcome =
            crate::repos_link_all::link_overlays(&link_inputs, stage, io.out, io.err, now_secs);
        if outcome.rc != 0 {
            status = 1;
            inputs_ready = false;
        }
    }
    if cancelled() {
        return 1;
    }
    if !inputs_ready {
        skip_inputs_rows(stage, io.out, "repository synchronization failed", now_secs);
    } else {
        let extensions_dir = state
            .config
            .extensions_dir
            .as_deref()
            .unwrap_or(inputs.extensions_dir);
        let mut worker = crate::hook_worker::Worker::for_update(
            inputs.runtime,
            &crate::hook_worker::UpdateEnvironment {
                extensions_dir,
                overlay_manifest: inputs.manifest,
                update_lock_token: inputs.update_lock_token,
                quiet: quiet(inputs),
                force: inputs.flags.force,
                verbose: inputs.flags.verbose || crate::log::is_quiet(inputs.dot_verbose),
            },
        );
        let retired = crate::profile_lifecycle::retire(
            &crate::profile_lifecycle::RetireInputs {
                present: state.profiles.present,
                extensions_enabled: state.extensions_enabled(),
                retained: &state.retained,
                eligible: &state.eligible_names,
                home: inputs.home,
                euid: inputs.euid,
                tmpdir: inputs.tmp,
                verbose: inputs.flags.verbose,
                log: inputs.log,
            },
            &mut worker,
            io.out,
            io.err,
        );
        if retired != 0 {
            status = 1;
            skip_inputs_rows(stage, io.out, "profile deactivation failed", now_secs);
        } else {
            if cancelled() {
                return 1;
            }
            let provider_enabled =
                !inputs.skip_provider && state.config.provider == crate::config::Provider::Shdeps;
            if !provider_enabled {
                let open = stage.start(
                    b"Tools",
                    Some(b"checking configured dependencies"),
                    now_secs,
                    inputs.dot_verbose,
                );
                let _ = io.out.write_all(&open);
                let close = stage.finish(b"ok", b"no dependency provider", now_secs);
                let _ = io.out.write_all(&close);
            } else {
                // The shell prepares Shdeps before opening the Tools stage.
                // Flush completed earlier stages first so bootstrap/download
                // diagnostics retain their stream and execution-point order.
                if !flush_pending(io, live_out, live_err) {
                    return 1;
                }
                let policy = match state.config.shdeps_update_policy {
                    crate::config::UpdatePolicy::Pinned => "pinned",
                    crate::config::UpdatePolicy::Latest => "latest",
                };
                let provider_inputs = crate::shdeps_provider::Inputs {
                    runtime: inputs.runtime,
                    source_root: inputs.source_root_git,
                    home: inputs.home,
                    config_home: inputs.config_home,
                    state_home: inputs.state_home,
                    policy,
                    force: inputs.flags.force,
                    quiet: quiet(inputs),
                    verbose: inputs.flags.verbose || crate::log::is_quiet(inputs.dot_verbose),
                    update_jobs: inputs.update_jobs,
                    palette: inputs.palette,
                    multibyte: inputs.multibyte,
                    ascii: inputs.ascii,
                    bar_width: inputs.bar_width,
                };
                match crate::shdeps_provider::prepare(&provider_inputs, live_out, live_err) {
                    Err(provider) if provider.interrupted.is_some() || cancelled() => {
                        return interruption_status(provider.interrupted);
                    }
                    Err(provider) if provider.abort => {
                        let _ = live_err.write_all(&provider.stderr);
                        return 1;
                    }
                    Err(provider) => {
                        if live_err.write_all(&provider.stderr).is_err() {
                            return 1;
                        }
                        let open = stage.start(
                            b"Tools",
                            Some(b"checking configured dependencies"),
                            now_secs,
                            inputs.dot_verbose,
                        );
                        let _ = io.out.write_all(&open);
                        let close = stage.finish(b"failed", &provider.summary, now_secs);
                        let _ = io.out.write_all(&close);
                        status = 1;
                    }
                    Ok(prepared) => {
                        let open = stage.start(
                            b"Tools",
                            Some(b"checking configured dependencies"),
                            now_secs,
                            inputs.dot_verbose,
                        );
                        let _ = io.out.write_all(&open);
                        if !flush_pending(io, live_out, live_err) {
                            return 1;
                        }
                        let provider = crate::shdeps_provider::update(
                            &provider_inputs,
                            prepared,
                            stage,
                            now_secs,
                            live_out,
                            live_err,
                        );
                        if provider.interrupted.is_some() {
                            return interruption_status(provider.interrupted);
                        }
                        if provider.abort || cancelled() {
                            return interruption_status(None);
                        }
                        let close =
                            stage.finish(&provider.stage_status, &provider.summary, now_secs);
                        let _ = io.out.write_all(&close);
                        io.out.extend_from_slice(&provider.details);
                        if provider.status != 0 {
                            status = 1;
                        } else if let Some((before, after)) = provider.revision_change {
                            if cancelled() {
                                return 1;
                            }
                            return provider_reexec(
                                inputs, io, &before, &after, now_secs, live_out, live_err,
                            );
                        }
                    }
                }
            }
            if cancelled() {
                return 1;
            }
            let extensions_dir = state
                .config
                .extensions_dir
                .as_deref()
                .unwrap_or(inputs.extensions_dir);
            let merged = crate::merges::run(
                &crate::merges::RunInputs {
                    runtime: inputs.runtime,
                    update_lock_token: inputs.update_lock_token,
                    extension_inputs: crate::extension_trust::Inputs {
                        euid: inputs.euid,
                        home: inputs.home.to_string(),
                        extensions_dir: extensions_dir.to_string(),
                        manifest: inputs.manifest.to_string(),
                        retiring_root: String::new(),
                    },
                    extensions_enabled: crate::config::extensions_enabled(&state.config),
                    overlays: &state.active,
                    tmp: inputs.tmp,
                    update_jobs: inputs.update_jobs,
                    merge_jobs: inputs.merge_jobs,
                    verbose: inputs.flags.verbose || crate::log::is_quiet(inputs.dot_verbose),
                    quiet: quiet(inputs),
                    force: inputs.flags.force,
                    palette: inputs.palette,
                    multibyte: inputs.multibyte,
                    log: inputs.log,
                },
                stage,
                io.out,
                io.err,
                now_secs,
            );
            if merged.status != 0 {
                status = 1;
            }
            if cancelled() {
                return 1;
            }
        }
    }
    match lifecycle_publish_decision(inputs_ready, status, cancelled()) {
        LifecyclePublish::Interrupted => return 1,
        LifecyclePublish::Skip => {}
        LifecyclePublish::Commit => {
            let ledger = state.ledger(inputs);
            let committed = crate::profile_lifecycle::commit_guarded(
                &crate::profile_lifecycle::CommitInputs {
                    present: state.profiles.present,
                    extensions_enabled: state.extensions_enabled(),
                    retained: &state.retained,
                    eligible: &state.eligible_names,
                    active: &state.active,
                    ledger: Some(&ledger),
                    home: inputs.home,
                    euid: inputs.euid,
                },
                || !cancelled(),
            );
            match committed {
                crate::profile_lifecycle::WriteOutcome::Committed => {}
                crate::profile_lifecycle::WriteOutcome::Cancelled => return 1,
                crate::profile_lifecycle::WriteOutcome::Failed => {
                    warn_row(
                        io.err,
                        inputs.palette,
                        "  warning: could not commit profile lifecycle state",
                    );
                    status = 1;
                }
            }
        }
    }
    if cancelled() {
        return 1;
    }
    let based = inputs.base.is_some_and(|base| base.exists());
    if based {
        let open = stage.start(
            b"Cleanup",
            Some(b"normalizing worktree"),
            now_secs,
            inputs.dot_verbose,
        );
        let _ = io.out.write_all(&open);
        crate::repos_dirty::normalize_filtered(base_prefix.as_deref(), &state.active);
        let close = stage.finish(b"ok", b"worktree normalized", now_secs);
        let _ = io.out.write_all(&close);
    } else {
        let open = stage.start(
            b"Cleanup",
            Some(b"normalizing worktree"),
            now_secs,
            inputs.dot_verbose,
        );
        let _ = io.out.write_all(&open);
        let close = stage.finish(b"ok", b"no base repo", now_secs);
        let _ = io.out.write_all(&close);
    }
    if cancelled() {
        return 1;
    }
    let close = crate::progress_ui::done(
        inputs.palette,
        quiet(inputs),
        Some(&status.to_string()),
        now_secs,
        now_secs,
        &reload_hint(inputs),
    );
    let _ = io.out.write_all(&close);
    status
}

/// Continue one provider-driven source-generation transition without touching
/// the process-global environment or reacquiring the already-held update lock.
fn provider_reexec(
    inputs: &EngineInputs<'_>,
    io: &mut UpdateIo<'_>,
    before: &str,
    after: &str,
    now_secs: i64,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> i32 {
    use std::io::Write as _;
    if cancelled() {
        return 1;
    }
    if !crate::shdeps::revision_valid(before) {
        warn_row(
            io.err,
            inputs.palette,
            "  warning: active dot revision was invalid before provider update",
        );
        let close = crate::progress_ui::done(
            inputs.palette,
            quiet(inputs),
            Some("1"),
            now_secs,
            now_secs,
            &reload_hint(inputs),
        );
        let _ = io.out.write_all(&close);
        return 1;
    }
    if !crate::shdeps::revision_valid(after) {
        warn_row(
            io.err,
            inputs.palette,
            "  warning: active dot revision is unavailable after provider update",
        );
        let close = crate::progress_ui::done(
            inputs.palette,
            quiet(inputs),
            Some("1"),
            now_secs,
            now_secs,
            &reload_hint(inputs),
        );
        let _ = io.out.write_all(&close);
        return 1;
    }
    if inputs
        .runtime
        .value("DOT_REEXEC_ONCE")
        .and_then(OsStr::to_str)
        == Some("1")
    {
        let path = Path::new(inputs.state_home).join("dot/provider-reexec-failed");
        let mut moves = crate::temp::MoveCache::default();
        if crate::shdeps::write_checkpoint(before, after, &path, &mut moves) {
            warn_row(
                io.err,
                inputs.palette,
                "  warning: dot changed twice during one update; rerun to validate the provider checkpoint",
            );
        } else {
            warn_row(
                io.err,
                inputs.palette,
                "  warning: dot changed twice and its provider checkpoint could not be published",
            );
        }
        let close = crate::progress_ui::done(
            inputs.palette,
            quiet(inputs),
            Some("1"),
            now_secs,
            now_secs,
            &reload_hint(inputs),
        );
        let _ = io.out.write_all(&close);
        return 1;
    }
    let mut env = inputs.runtime.env().clone();
    env.insert(OsString::from("DOT_REEXEC_ONCE"), OsString::from("1"));
    env.insert(
        OsString::from("DOT_REEXEC_EXPECTED_REVISION"),
        OsString::from(after),
    );
    let runtime = match crate::app::Runtime::from_env(&env, inputs.runtime.cwd()) {
        Ok(runtime) => runtime,
        Err(_) => return 1,
    };
    let policy = env_value(&env, "DOT_SHDEPS_UPDATE_POLICY");
    let startup = crate::startup::Inputs {
        home: inputs.home,
        xdg_config_home: inputs.config_home,
        env_policy: policy.as_deref(),
        reexec_expected: Some(after),
        source_root: inputs.source_root_git,
    };
    let config = match crate::startup::preflight(&startup) {
        Ok(config) => config,
        Err(failure) => {
            io.err.extend_from_slice(failure.line().as_bytes());
            io.err.push(b'\n');
            return 1;
        }
    };
    if cancelled() {
        return 1;
    }
    let gathered = match gather(
        inputs.original_args,
        &runtime,
        &config,
        inputs.source_root_git,
        inputs.state_home,
        &env,
        runtime.cwd(),
    ) {
        Ok(gathered) => gathered,
        _ => return 1,
    };
    let nested = gathered.inputs();
    if cancelled() {
        return 1;
    }
    run_gathered(&nested, io.out, io.err, now_secs, live_out, live_err)
}

fn flush_pending(
    io: &mut UpdateIo<'_>,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> bool {
    let stdout_ok = live_out.write_all(io.out).is_ok();
    let stderr_ok = live_err.write_all(io.err).is_ok();
    io.out.clear();
    io.err.clear();
    stdout_ok && stderr_ok
}

fn cancelled() -> bool {
    crate::cleanup::received_signal().is_some()
}

fn interruption_status(signal: Option<i32>) -> i32 {
    signal
        .or_else(crate::cleanup::received_signal)
        .map_or(1, |signal| 128 + signal)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecyclePublish {
    Interrupted,
    Commit,
    Skip,
}

/// Decide whether staged lifecycle authority may become durable. Keeping the
/// post-hook cancellation sample in this decision makes the narrow race where
/// every hook succeeded but a signal arrived before commit explicit and
/// independently testable.
fn lifecycle_publish_decision(
    inputs_ready: bool,
    status: i32,
    interrupted: bool,
) -> LifecyclePublish {
    if interrupted {
        LifecyclePublish::Interrupted
    } else if inputs_ready && status == 0 {
        LifecyclePublish::Commit
    } else {
        LifecyclePublish::Skip
    }
}

/// Effective quiet for rows the shell gates on `DOT_QUIET` (the
/// `--quiet`/`--cron` flag exports join the variable here).
fn quiet(inputs: &EngineInputs<'_>) -> bool {
    inputs.flags.quiet || inputs.flags.cron || crate::log::is_quiet(inputs.dot_quiet)
}

/// `_ui_shell_reload_hint` inputs from this invocation.
fn reload_hint(inputs: &EngineInputs<'_>) -> Vec<u8> {
    let shell_name = inputs.shell.and_then(|shell| {
        Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    });
    let home = Path::new(inputs.home);
    crate::progress_ui::reload_hint(
        inputs.reloads_shell,
        shell_name.as_deref(),
        home.join(".bashrc").exists(),
        home.join(".zshrc").exists(),
    )
}

/// Defensive config reload inputs from the immutable native invocation.
fn startup_inputs<'a>(inputs: &EngineInputs<'a>) -> crate::startup::Inputs<'a> {
    crate::startup::Inputs {
        home: inputs.home,
        xdg_config_home: inputs.config_home,
        env_policy: inputs.shdeps_update_policy,
        reexec_expected: inputs.reexec_expected,
        source_root: inputs.source_root_git,
    }
}

/// Owned invocation capture for [`run_update`]: every constant and flag value
/// is resolved before the first stage opens. [`Gathered::inputs`]
/// from here, so one value lives through the whole run.
pub struct Gathered {
    runtime: crate::app::Runtime,
    update_lock_token: Option<String>,
    config: crate::config::Config,
    flags: UpdateFlags,
    args: Vec<std::ffi::OsString>,
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
    merge_jobs: Option<String>,
    skip_provider: bool,
    live: bool,
    multibyte: bool,
    ascii: bool,
    euid: u32,
    tmp: std::path::PathBuf,
    source_root_git: std::path::PathBuf,
    checkout_root: String,
    extensions_dir: String,
    prefix: String,
    reloads_shell: Option<String>,
    shell: Option<String>,
    shdeps_update_policy: Option<String>,
    reexec_expected: Option<String>,
}

impl Gathered {
    /// Borrow the driver inputs from this capture.
    pub fn inputs(&self) -> EngineInputs<'_> {
        EngineInputs {
            runtime: &self.runtime,
            update_lock_token: self.update_lock_token.as_deref(),
            config: &self.config,
            flags: self.flags,
            original_args: &self.args,
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
            skip_provider: self.skip_provider,
            update_jobs: self.update_jobs.as_deref(),
            merge_jobs: self.merge_jobs.as_deref(),
            live: self.live,
            multibyte: self.multibyte,
            ascii: self.ascii,
            bar_width: &self.bar_width,
            extensions_dir: &self.extensions_dir,
            checkout_root: &self.checkout_root,
            prefix: &self.prefix,
            reloads_shell: self.reloads_shell.as_deref(),
            shell: self.shell.as_deref(),
            shdeps_update_policy: self.shdeps_update_policy.as_deref(),
            reexec_expected: self.reexec_expected.as_deref(),
        }
    }
}

/// Non-empty environment value (unset and empty read the same,
/// like `${VAR:-}` defaults).
fn env_value(env: &BTreeMap<OsString, OsString>, name: &str) -> Option<String> {
    env.get(OsStr::new(name))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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
/// fails closed — the checks must never run under a guessed identity.
fn resolve_euid(env: &BTreeMap<OsString, OsString>) -> Option<u32> {
    if let Some(euid) = env_value(env, "EUID").and_then(|value| value.parse::<u32>().ok()) {
        return Some(euid);
    }
    Some(unsafe { libc::geteuid() })
}

/// Locale for the ASCII probe: `${LC_ALL:-${LC_CTYPE:-${LANG:-}}}`,
/// like `_ui_ascii_mode`.
fn locale_name(env: &BTreeMap<OsString, OsString>) -> String {
    env_value(env, "LC_ALL")
        .or_else(|| env_value(env, "LC_CTYPE"))
        .or_else(|| env_value(env, "LANG"))
        .unwrap_or_default()
}

/// Resolve the base repository publication that `repos/model.sh` used to
/// place in globals before dispatch. A completed init record is authoritative;
/// the legacy separate checkout remains the record-free compatibility shape.
fn base_client(
    runtime: &crate::app::Runtime,
    home: &str,
    state_home: &str,
) -> Result<Base, GatherError> {
    let mut diagnostic = Vec::new();
    crate::repos_base::select(runtime, home, state_home, &mut diagnostic)
        .map_err(|()| GatherError::Diagnostic(diagnostic))
}

/// A complete request for one native update invocation.
pub struct UpdateRequest<'a> {
    /// Parsed client configuration.
    pub config: &'a crate::config::Config,
    /// Command environment after update flag side effects.
    pub env: &'a BTreeMap<OsString, OsString>,
    /// Original update arguments, including flags.
    pub args: &'a [OsString],
    /// Trampoline-normalized XDG state home.
    pub state_home: &'a Path,
}

#[derive(Debug, Clone)]
enum GatherError {
    Xdg(crate::xdg::Error),
    Diagnostic(Vec<u8>),
    Unavailable,
}

impl GatherError {
    fn code(&self) -> i32 {
        match self {
            Self::Xdg(error) => error.code(),
            Self::Diagnostic(_) => 1,
            Self::Unavailable => 1,
        }
    }

    fn write(&self, stderr: &mut dyn std::io::Write) {
        if let Self::Diagnostic(line) = self {
            let _ = stderr.write_all(line);
        }
    }
}

impl From<crate::xdg::Error> for GatherError {
    fn from(error: crate::xdg::Error) -> Self {
        Self::Xdg(error)
    }
}

/// Capture one native invocation from the command's derived environment.
/// The dispatcher already applies the shell flag exports before this point;
/// `state_home` is its trampoline-normalized XDG state dir and `source_root`
/// is `$DOT_SOURCE_ROOT`. Capture failures are terminal: there is no legacy
/// engine whose ambient process state can safely substitute for these values.
fn gather(
    args: &[std::ffi::OsString],
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    source_root: &std::path::Path,
    state_home: &str,
    env: &BTreeMap<OsString, OsString>,
    cwd: &Path,
) -> Result<Gathered, GatherError> {
    use std::io::IsTerminal as _;
    let (flags, extra) = parse_flags(args);
    let home = env_value(env, "HOME").unwrap_or_default();
    // `dot_xdg_home state` is the canonical HOME validity check. It keeps
    // this entry error in the same typed XDG vocabulary as the lock path.
    crate::xdg::base(crate::xdg::Kind::State, "", &home)?;
    let config_value = env_value(env, "XDG_CONFIG_HOME").unwrap_or_default();
    let config_home = crate::xdg::base(crate::xdg::Kind::Config, &config_value, &home)?;
    let manifest = env_value(env, "DOT_OVERLAY_MANIFEST")
        .unwrap_or_else(|| format!("{state_home}/dot/overlay-links"));
    let legacy_manifest = env_value(env, "DOT_OVERLAY_LEGACY_MANIFEST")
        .unwrap_or_else(|| format!("{home}/.local/state/dot/overlay-links"));
    let mut moves = crate::temp::MoveCache::default();
    let tool = match moves.tool() {
        Ok(tool) => tool,
        Err(_) => return Err(GatherError::Unavailable),
    };
    let stdout_tty = std::io::stdout().is_terminal();
    let no_color = env_value(env, "NO_COLOR");
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
    let dot_quiet = env_value(env, "DOT_QUIET");
    let dot_verbose = env_value(env, "DOT_VERBOSE");
    let log = crate::log::Log::from_env(stdout_tty, no_color.as_deref(), dot_quiet.as_deref());
    let quiet = flags.quiet || flags.cron || crate::log::is_quiet(dot_quiet.as_deref());
    let live = crate::progress_ui::live_enabled(
        quiet,
        stdout_tty,
        env_value(env, "DOT_UI_FORCE_LIVE").as_deref(),
    );
    let locale = locale_name(env);
    let multibyte = crate::progress_ui::utf8_locale(&locale);
    let ascii = crate::progress_ui::ascii_mode(
        env_value(env, "DOT_UI_ASCII").as_deref(),
        &locale,
        multibyte,
    );
    let skip_provider = env_value(env, "DOT_INIT_SKIP_PROVIDER").as_deref() == Some("1");
    let pwd = cwd.to_str().unwrap_or(&home).to_string();
    let dest = crate::repos_overlays::DestinationInputs {
        home: home.clone(),
        xdg_state_home: env_value(env, "XDG_STATE_HOME"),
        install_dir: env_value(env, "SHDEPS_INSTALL_DIR"),
        state_dir: env_value(env, "SHDEPS_STATE_DIR"),
        overlay_paths: Vec::new(),
        init_backup: env_value(env, "DOT_INIT_BACKUP").filter(|value| value != "-"),
        pwd,
    };
    let euid = match resolve_euid(env) {
        Some(euid) => euid,
        None => return Err(GatherError::Unavailable),
    };
    let checkout_root = match source_root.to_str() {
        Some(root) => root.to_string(),
        None => return Err(GatherError::Unavailable),
    };
    Ok(Gathered {
        runtime: runtime.clone(),
        update_lock_token: env_value(env, "DOT_UPDATE_LOCK_TOKEN"),
        config: config.clone(),
        flags,
        args: args.to_vec(),
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
        base: Some(base_client(runtime, &home, state_home)?),
        bar_width: env_value(env, "DOT_UI_PROGRESS_WIDTH").unwrap_or_else(|| "8".to_string()),
        dot_verbose,
        dot_quiet,
        update_jobs: env_value(env, "DOT_UPDATE_JOBS"),
        merge_jobs: env_value(env, "DOT_MERGE_JOBS"),
        skip_provider,
        live,
        multibyte,
        ascii,
        euid,
        tmp: env
            .get(OsStr::new("TMPDIR"))
            .cloned()
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp")),
        source_root_git: source_root.to_path_buf(),
        checkout_root,
        // `dot_config_load` publishes this path before the shell chooses its
        // update lane. Native gathering receives the parsed config directly,
        // so do not require a duplicate ambient export to notice configured
        // hook directories.
        extensions_dir: config
            .extensions_dir
            .clone()
            .unwrap_or_else(|| env_value(env, "DOT_EXTENSIONS_DIR").unwrap_or_default()),
        prefix: env_value(env, "PREFIX").unwrap_or_default(),
        reloads_shell: env_value(env, "DOT_UPDATE_RELOADS_SHELL"),
        shell: env_value(env, "SHELL"),
        shdeps_update_policy: env_value(env, "DOT_SHDEPS_UPDATE_POLICY"),
        reexec_expected: env_value(env, "DOT_REEXEC_EXPECTED_REVISION"),
    })
}

/// Wall-clock seconds for stage rows (`date +%s` equivalent).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Execute one native update through the typed runtime and stream boundary.
pub fn run_update(
    runtime: &crate::app::Runtime,
    request: &UpdateRequest<'_>,
    streams: &mut crate::app::Streams<'_>,
) -> i32 {
    let state_home = match request.state_home.to_str() {
        Some(state_home) => state_home,
        None => return 1,
    };
    let gathered = match gather(
        request.args,
        runtime,
        request.config,
        runtime.source_root(),
        state_home,
        request.env,
        runtime.cwd(),
    ) {
        Ok(gathered) => gathered,
        Err(error) => {
            error.write(streams.stderr);
            return error.code();
        }
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_gathered(
        &gathered.inputs(),
        &mut out,
        &mut err,
        now_secs(),
        streams.stdout,
        streams.stderr,
    );
    let stdout_failed = streams.stdout.write_all(&out).is_err();
    let stderr_failed = streams.stderr.write_all(&err).is_err();
    if stdout_failed || stderr_failed {
        return 1;
    }
    code
}

/// `_dot_update` natively: the cron dirty gate plus one outcome
/// record per cron run around [`run_gathered_inner`].
///
/// The shell resolves cron dirt before `_ui_begin`: unresolved edits
/// return 0 with no rows, while matching-upstream files are repaired
/// before sync. Handoff finding #1 keeps that contract (exit 0,
/// never fight active edits) but ends the silence: a skip appends a
/// `skip` line to the cron outcome log and warns on stderr even in
/// cron mode, so a frozen slot is visible without re-running.
/// Finding #6 records `ok`/`fail` the same way on the way out and
/// refreshes the last-success stamp on success. Interrupted runs
/// record nothing (cancellation is not an outcome), and non-cron
/// runs write nothing (the history-tree tests pin the state
/// directory across plain updates).
fn run_gathered(
    inputs: &EngineInputs<'_>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> i32 {
    if cancelled() {
        return 1;
    }
    let base = inputs
        .base
        .filter(|base| base.exists())
        .and_then(crate::repos_base::Base::git_prefix);
    if inputs.flags.cron
        && crate::repos_dirty::is_worktree_dirty(base.as_deref(), inputs.entries)
        && !crate::repos_dirty::try_resolve_dirty(inputs.home, base.as_deref(), inputs.entries)
    {
        let files = crate::repos_dirty::dirty_file_list(base.as_deref(), inputs.entries);
        let detail = crate::update_status::format_skip_detail(&files);
        crate::update_status::append_outcome(
            Path::new(inputs.state_home),
            now_secs,
            "skip",
            "dirty",
            &detail,
        );
        warn_row(
            err,
            inputs.palette,
            &format!(
                "  warning: cron update skipped with unresolved local edits ({detail}); commit, stash, or resolve them to resume convergence"
            ),
        );
        return 0;
    }
    let rc = run_gathered_inner(inputs, out, err, now_secs, live_out, live_err);
    if inputs.flags.cron && !cancelled() {
        let state_home = Path::new(inputs.state_home);
        if rc == 0 {
            crate::update_status::append_outcome(state_home, now_secs, "ok", "update", "");
            crate::update_status::record_success(state_home, now_secs);
        } else {
            crate::update_status::append_outcome(state_home, now_secs, "fail", "update", "");
        }
    }
    rc
}

/// `_dot_update` natively: flag-driven stages around [`sync_repos`]
/// and finalization with the defensive policy reload between them.
fn run_gathered_inner(
    inputs: &EngineInputs<'_>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> i32 {
    use std::io::Write as _;
    if cancelled() {
        return 1;
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
    let mut sync = sync_repos(inputs, &mut stage, &mut moves, out, err, now_secs);
    if cancelled() {
        return 1;
    }
    if sync.rc != 0 {
        let mut io = UpdateIo { out, err };
        let rc = finalize(
            inputs,
            &mut sync.state,
            &mut stage,
            &mut io,
            now_secs,
            1,
            sync.frozen,
            live_out,
            live_err,
        );
        return rc;
    }
    // Defensive reload before provider selection continues (a
    // failure closes without finalizing, like the shell: the
    // loader prints its own diagnostic, then `_ui_done 1`).
    let startup = startup_inputs(inputs);
    match crate::startup::preflight(&startup) {
        Ok(config) => sync.state.config = config,
        Err(failure) => {
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
            return 1;
        }
    }
    if cancelled() {
        return 1;
    }
    let mut io = UpdateIo { out, err };
    finalize(
        inputs,
        &mut sync.state,
        &mut stage,
        &mut io,
        now_secs,
        0,
        sync.frozen,
        live_out,
        live_err,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed stdout"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn signal_after_successful_hooks_prevents_lifecycle_publish() {
        assert_eq!(
            lifecycle_publish_decision(true, 0, true),
            LifecyclePublish::Interrupted
        );
    }

    #[test]
    fn additions_identity_ignores_same_name_descriptor_mutation() {
        let phase_one = BTreeSet::from([String::from("alpha")]);
        let refreshed = [
            String::from("alpha|/new/path|file:///new|/new/descriptor|false|git"),
            String::from("beta|/beta|file:///beta|/beta/descriptor|false|git"),
        ];
        let additions: Vec<&str> = refreshed
            .iter()
            .filter(|record| !record_name(record).is_some_and(|name| phase_one.contains(name)))
            .map(String::as_str)
            .collect();

        assert_eq!(
            additions,
            ["beta|/beta|file:///beta|/beta/descriptor|false|git"]
        );
    }

    #[test]
    fn stale_frozen_marker_does_not_decline_native_capture() {
        // `_dot_update` clears this command-local marker before a new run.
        // Capturing it as a top-level fallback would incorrectly preserve a
        // prior failed run instead of letting the fresh generation replace it.
        let env = BTreeMap::from([
            (OsString::from("HOME"), OsString::from("/tmp")),
            (OsString::from("EUID"), OsString::from("0")),
            (
                OsString::from("DOT_DEPENDENCY_PROVIDER"),
                OsString::from("none"),
            ),
            (
                OsString::from("DOT_OVERLAY_LINKS_FROZEN"),
                OsString::from("1"),
            ),
        ]);
        let runtime =
            crate::app::Runtime::from_env(&env, Path::new("/tmp")).expect("absolute fixture cwd");
        let config = crate::config::Config {
            version: 1,
            extension_api: false,
            extensions_dir: None,
            provider: crate::config::Provider::None,
            default_profile: "base".to_string(),
            shdeps_update_policy: crate::config::UpdatePolicy::Pinned,
            policy_from_env: false,
        };
        let gathered = gather(
            &[],
            &runtime,
            &config,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            "/tmp",
            &env,
            Path::new("/tmp"),
        )
        .expect("valid native inputs");
        assert!(matches!(
            gathered.inputs().config.provider,
            crate::config::Provider::None
        ));
    }

    #[test]
    fn malformed_completed_identity_fails_instead_of_becoming_legacy() {
        let scratch = dot_test_support::TempDir::new("update-base-malformed")
            .expect("create temporary directory");
        let home = scratch.path().join("home");
        let state = scratch.path().join("state");
        std::fs::create_dir_all(home.join(".dotfiles")).expect("legacy-looking git directory");
        std::fs::create_dir_all(state.join("dot/init")).expect("init state");
        std::fs::write(state.join("dot/init/completed"), b"not-a-record\n")
            .expect("malformed completed record");
        let env = BTreeMap::from([
            (OsString::from("HOME"), home.as_os_str().to_owned()),
            (
                OsString::from("XDG_STATE_HOME"),
                state.as_os_str().to_owned(),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, scratch.path()).expect("runtime");
        let error = base_client(
            &runtime,
            home.to_str().expect("UTF-8 home"),
            state.to_str().expect("UTF-8 state"),
        )
        .expect_err("completed identity stays authoritative");
        assert!(matches!(error, GatherError::Diagnostic(_)));
    }

    #[test]
    fn stderr_is_delivered_even_when_stdout_delivery_fails() {
        let scratch = dot_test_support::TempDir::new("update-stream-failure")
            .expect("create temporary directory");
        let home = scratch.path().join("home");
        let state = scratch.path().join("state");
        let config_home = scratch.path().join("config");
        std::fs::create_dir_all(config_home.join("dot")).expect("config directory");
        std::fs::create_dir_all(&state).expect("state directory");
        std::fs::write(config_home.join("dot/config"), b"version=broken\n")
            .expect("rejected reload config");
        let env = BTreeMap::from([
            (OsString::from("HOME"), home.as_os_str().to_owned()),
            (
                OsString::from("XDG_STATE_HOME"),
                state.as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_CONFIG_HOME"),
                config_home.as_os_str().to_owned(),
            ),
            (OsString::from("EUID"), OsString::from("0")),
            (
                OsString::from("DOT_BASE_TOPOLOGY"),
                OsString::from("missing"),
            ),
            (
                OsString::from("DOT_SOURCE_ROOT"),
                OsString::from(env!("CARGO_MANIFEST_DIR")),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, scratch.path()).expect("runtime");
        let config = crate::config::Config {
            version: 1,
            extension_api: false,
            extensions_dir: None,
            provider: crate::config::Provider::None,
            default_profile: "base".to_string(),
            shdeps_update_policy: crate::config::UpdatePolicy::Pinned,
            policy_from_env: false,
        };
        let mut stdout = FailingWriter;
        let mut stderr = Vec::new();
        let code = run_update(
            &runtime,
            &UpdateRequest {
                config: &config,
                env: &env,
                args: &[],
                state_home: &state,
            },
            &mut crate::app::Streams::new(&mut stdout, &mut stderr),
        );
        assert_eq!(code, 1);
        assert_eq!(stderr, b"dot: config: unsupported version: broken\n");
    }
}
