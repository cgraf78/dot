//! Native `dot update` engine driver (engine update lane).
//!
//! Executes `_dot_update` (`lib/dot/update.sh`) without the shell:
//! flag parsing, `_ui_begin`, the cron dirty gate, repo sync
//! (installed-link snapshot, base/overlay pull, policy reload,
//! overlay converge, lifecycle prepare), the defensive config
//! reload, and finalize (provider checkpoint, link phase,
//! lifecycle retire, shdeps branch, the shdeps prune stage,
//! merges, lifecycle commit, worktree normalize, `_ui_done`). Pure
//! sequencing folds live in [`crate::update`]; this module owns the
//! impure step execution,
//! composing [`crate::repos_pull_fleet`], [`crate::repos_link_all`],
//! [`crate::profile_lifecycle`], [`crate::pre_sync`],
//! [`crate::merges`], and [`crate::shdeps`].
//!
//! Every `update` and `pull` invocation runs this engine. Configuration is
//! parsed before entry, so the provider is a closed enum rather than an
//! open-ended shell value that needs a fallback lane.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

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

/// Command driving one engine run.
///
/// Dependency pruning is maintenance owned by `dot update`/`dot pull`. `dot
/// init` reuses the engine for first convergence, where a prune failure would
/// fail (and roll back) the installation itself, so init never prunes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// `dot update` or `dot pull`.
    Update,
    /// `dot init` convergence.
    Init,
}

/// Environment variable selecting when `dot update` prunes orphaned Shdeps
/// dependencies.
///
/// This is deliberately not a config-file key: the client config is shared
/// through the base repository, and Dot releases from before unknown-key
/// tolerance (see [`crate::config`]) reject unknown keys, so a key would
/// brick every client still running one of them. Older releases ignore an
/// unknown environment variable instead (they simply do not prune).
pub const PRUNE_ENV: &str = "DOT_SHDEPS_PRUNE";

/// The update lock claim a provider continuation re-enters the held lock
/// with. Hook workers receive it explicitly ([`EngineInputs::update_lock_token`]),
/// never through the runtime environment.
const LOCK_TOKEN_ENV: &str = "DOT_UPDATE_LOCK_TOKEN";

/// Set in a provider continuation's environment: a second Dot change there
/// publishes the provider checkpoint instead of continuing again.
const REEXEC_ONCE_ENV: &str = "DOT_REEXEC_ONCE";

/// The revision a provider continuation must observe at startup.
const REEXEC_EXPECTED_ENV: &str = "DOT_REEXEC_EXPECTED_REVISION";

/// Epoch seconds at which the first half of a handed-off update started, so
/// the continuation's final `Done in` and cron outcome cover the whole run.
const REEXEC_STARTED_ENV: &str = "DOT_REEXEC_STARTED";

/// Test-only bound in seconds for the release handoff probe (default
/// [`crate::cleanup::REVISION_PROBE_TIMEOUT`]).
const PROBE_TIMEOUT_ENV: &str = "_DOT_REEXEC_PROBE_TIMEOUT_SECONDS";

/// Handoff variables a continuation consumes itself. Like the lock claim
/// they never reach providers, hooks, or other children: a long-lived process
/// started from a hook (or a Git helper started by a pull) would otherwise
/// carry `DOT_REEXEC_EXPECTED_REVISION` into every later `dot` it runs, which
/// fails the startup guard after the next upgrade. The engine strips them
/// from its runtime and the binary entry from the process environment
/// ([`crate::handoff::scrub_process_env`]).
pub(crate) const CONTINUATION_ENV: [&str; 5] = [
    LOCK_TOKEN_ENV,
    REEXEC_ONCE_ENV,
    REEXEC_EXPECTED_ENV,
    REEXEC_STARTED_ENV,
    WARNED_ENV,
];

/// Whether `runtime` is a provider continuation: the second half of an
/// update whose first half already reported its command-boundary warnings.
/// Read from the command's own environment; the engine strips the variable
/// from what its children inherit.
pub(crate) fn is_continuation(runtime: &crate::app::Runtime) -> bool {
    runtime.value(REEXEC_ONCE_ENV).and_then(OsStr::to_str) == Some("1")
}

/// Record a `fail` cron outcome for a release continuation that stops
/// before its engine records the run (for example when it cannot re-enter
/// the update lock). The first half skipped its own line on handoff, so
/// without this the run would leave no outcome at all.
pub(crate) fn record_continuation_failure(
    continuation: bool,
    args: &[OsString],
    state_home: &Path,
) {
    // Cancellation is not an outcome, as everywhere else in the cron log.
    if continuation && parse_flags(args).0.cron && !cancelled() {
        crate::update_status::append_outcome(state_home, now_secs(), "fail", "update", "");
    }
}

/// Whether a release root may hand the rest of a run off to a new binary:
/// only `dot update` itself, and only from the binary entry point, which
/// owns the process image and runs the handoff. `dot init` converges inside
/// its own transaction and must finish in place; an embedded runtime cannot
/// replace its process.
fn release_hands_off(caller: Caller, can_exec: bool) -> bool {
    caller == Caller::Update && can_exec
}

/// When `dot update` removes orphaned Shdeps dependencies itself.
///
/// Pruning is destructive (uninstall hooks run and managed payloads are
/// deleted), so it never runs from `dot init`, never from a hand-run update
/// unless asked for, and an explicit `never` turns it off entirely. Whatever
/// the mode, the engine prunes only after a converged generation whose
/// dependency config it already trusted enough to run the Tools stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneMode {
    /// Never prune (explicit opt-out, and the reading of an unknown value).
    Never,
    /// Prune only on `dot update --cron` (the default).
    Cron,
    /// Prune on every `dot update`/`dot pull`.
    Always,
}

impl PruneMode {
    /// Parse a [`PRUNE_ENV`] value. Unset or empty reads as [`Cron`]
    /// (`${VAR:-}` convention); an unrecognized value is `None` so the
    /// caller can warn and fall back to `Never` instead of failing the
    /// update: a value a newer client understands must never stop an
    /// older one from converging (and upgrading itself).
    ///
    /// The default used to be `Never`, an opt-in chosen because prune is
    /// destructive and because clients float to the latest release
    /// independently, so a default flip reaches every host on its own
    /// schedule. In practice the owner's fleet opted every host in to
    /// `cron` through its cron entry, and cron auto-prune is the intended
    /// design, so the opt-in only cost a line every crontab had to carry,
    /// and a host whose entry lacked it silently accumulated orphans. Hand
    /// runs and `dot pull` still never prune by default, and an unknown
    /// value still reads as `Never` rather than as the default: a typo
    /// must never be what enables deletion.
    ///
    /// [`Cron`]: PruneMode::Cron
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value.unwrap_or_default() {
            "" => Some(PruneMode::Cron),
            "never" => Some(PruneMode::Never),
            "cron" => Some(PruneMode::Cron),
            "always" => Some(PruneMode::Always),
            _ => None,
        }
    }

    /// Whether an update invocation (`cron` = `--cron` was given) prunes.
    /// Only the explicit cron flag counts: `--quiet` or `DOT_QUIET` change
    /// output, never what an update does.
    pub fn applies(self, cron: bool) -> bool {
        match self {
            PruneMode::Never => false,
            PruneMode::Cron => cron,
            PruneMode::Always => true,
        }
    }
}

/// Shared inputs for the native update driver: every global the
/// shell `_dot_update` tree reads, plus the UI/logger handles the
/// pull and link lanes thread the same way.
pub struct EngineInputs<'a> {
    /// Immutable process boundary for leaf workers launched during this update.
    pub runtime: &'a crate::app::Runtime,
    /// Command driving this engine run.
    pub caller: Caller,
    /// Parsed [`PRUNE_ENV`] for this run (always `Never` for `dot init`).
    pub prune_mode: PruneMode,
    /// Native update lock claim exposed only to transactional hook workers.
    pub update_lock_token: Option<&'a str>,
    /// Command environment after the update flag exports and the lock
    /// claim. A provider continuation starts from it, so it keeps the claim
    /// and the flag exports that [`Self::runtime`] does not carry.
    pub env: &'a BTreeMap<OsString, OsString>,
    /// Whether this run is a provider continuation (`DOT_REEXEC_ONCE=1` in
    /// the command environment). A further Dot change publishes the provider
    /// checkpoint instead of continuing again.
    pub continuation: bool,
    /// Parsed client configuration for this update generation.
    pub config: &'a crate::config::Config,
    /// Unknown config keys already reported during this invocation (see
    /// `warn_reloaded_keys`). Seeded from [`Self::config`], whose keys
    /// the caller reported before the engine started.
    pub config_warned: &'a std::cell::RefCell<Vec<String>>,
    /// Unknown keys in profile, selector, and descriptor files already
    /// reported during this invocation (see `warn_data_keys`).
    pub data_warned: &'a std::cell::RefCell<Vec<crate::unknown_keys::DataKey>>,
    /// Whether this invocation already printed [`HOLD_WARNING`].
    pub hold_warned: &'a std::cell::Cell<bool>,
    /// Warning lines an earlier process of this invocation already
    /// printed, handed over in [`WARNED_ENV`]; never printed again.
    pub handed_warnings: &'a BTreeSet<String>,
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
    err: &mut dyn std::io::Write,
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
fn warn_row(err: &mut dyn std::io::Write, palette: &Palette, message: &str) {
    let _ = err.write_all(&crate::progress_ui::warn_line(palette, message.as_bytes()));
}

/// Report what a mid-run config reload newly ignored: each unknown key
/// that is a near miss of a known key and was not reported yet in this
/// invocation, with the same line the command boundary prints.
///
/// The rest of the run uses the reloaded values, so a misspelled
/// `dependency_provider` or `default_profile` arriving with this run's
/// pull would otherwise skip Tools or deactivate overlays with no word
/// until the next invocation. A key without a suggestion stays silent
/// here on purpose: it is most likely from a newer Dot, which this
/// run's Tools stage may install (keys a newer Dot adds are kept more
/// than two edits from existing ones, so they never get a suggestion).
fn warn_reloaded_keys(
    inputs: &EngineInputs<'_>,
    config: &crate::config::Config,
    err: &mut dyn std::io::Write,
) {
    let mut warned = inputs.config_warned.borrow_mut();
    for unknown in &config.unknown_keys {
        if unknown.suggestion().is_some() && !warned.contains(&unknown.key) {
            let line = unknown.warning();
            if !inputs.handed_warnings.contains(&line) {
                let _ = writeln!(err, "{line}");
            }
            warned.push(unknown.key.clone());
        }
    }
}

/// Environment variable that hands the warning lines one `dot update`
/// invocation already printed to a continuation running as another
/// process (a release handoff), so each warning still prints once per
/// invocation. The value is the lines joined by `\n`; a line holding a
/// newline is left out (it would merely print again). The engine reads it
/// at entry and never prints a listed line; like the other handoff
/// variables (the lock claim, `DOT_REEXEC_*`) it is scrubbed from the
/// environment hooks and providers see and, at the binary entry, from the
/// process environment. The release handoff builds the value from
/// [`EngineInputs::warned_handoff`] plus the boundary's prune-policy
/// warning. Lines are compared as printed, so a continuation whose release
/// words a warning differently (or knows the key) is unaffected.
pub const WARNED_ENV: &str = "DOT_UPDATE_WARNED";

/// Parse a [`WARNED_ENV`] value (absent or empty: nothing handed over).
pub fn handed_warnings(value: Option<&OsStr>) -> BTreeSet<String> {
    value
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

impl EngineInputs<'_> {
    /// The [`WARNED_ENV`] value for a continuation process: every unknown
    /// key, data-file key, and hold warning this invocation has printed so
    /// far (or was handed).
    pub fn warned_handoff(&self) -> OsString {
        warned_value(
            self.handed_warnings,
            &self.config_warned.borrow(),
            &self.data_warned.borrow(),
            self.hold_warned.get(),
        )
    }
}

/// Encode the printed warnings for [`WARNED_ENV`] (see
/// [`EngineInputs::warned_handoff`]).
fn warned_value(
    handed: &BTreeSet<String>,
    config_keys: &[String],
    data_keys: &[crate::unknown_keys::DataKey],
    held: bool,
) -> OsString {
    let mut lines = handed.clone();
    for key in config_keys {
        let unknown = crate::config::UnknownKey {
            key: key.clone(),
            line: 0,
        };
        lines.insert(unknown.warning());
    }
    for key in data_keys {
        lines.insert(key.warning());
    }
    if held {
        lines.insert(HOLD_WARNING.to_string());
    }
    let lines: Vec<String> = lines
        .into_iter()
        .filter(|line| !line.contains('\n'))
        .collect();
    OsString::from(lines.join("\n"))
}

/// Report keys this release does not know in the profile, selector, and
/// descriptor files it just read, once per file and key per invocation.
///
/// Unlike a mid-run config reload, which stays quiet about keys from a
/// newer Dot because this run's Tools stage may install one that knows
/// them, these files act on such keys before Tools runs: a selector stops
/// matching and an overlay is skipped in this very run. So every run that
/// reads a key warns, including the one that pulls it. Discovery runs
/// several times per update, hence the invocation-wide record.
fn warn_data_keys(
    inputs: &EngineInputs<'_>,
    keys: &[crate::unknown_keys::DataKey],
    err: &mut dyn std::io::Write,
) {
    let mut warned = inputs.data_warned.borrow_mut();
    // One key can warn twice with different effects: a skipped `base`
    // overlay, then the selection that fell back because of it.
    for key in keys {
        if !warned.contains(key) {
            let line = key.warning();
            if !inputs.handed_warnings.contains(&line) {
                let _ = writeln!(err, "{line}");
            }
            warned.push(key.clone());
        }
    }
}

/// The one line a held update prints; see "Held overlay sets" in
/// docs/configuration.md.
pub const HOLD_WARNING: &str = "dot: overlay: warning: overlay set held: newer keys need a newer dot (installed overlays, links, and config hooks left as they are)";

/// Whether keys this run read leave it unsure which overlays the newer
/// Dot would activate (see [`crate::unknown_keys::Effect::holds`]).
fn skew_holds(keys: &[crate::unknown_keys::DataKey]) -> bool {
    keys.iter().any(|key| key.effect.holds())
}

/// Hold the installed overlay set: converge nothing about overlays this
/// run.
///
/// Converging to this release's partial reading would deactivate what a
/// newer Dot keeps: a skipped descriptor drops its overlay, and a skipped
/// selector (or the unread selectors of a skipped `base` overlay) can
/// drop the host to `base`, unlinking every other overlay and running
/// their `profile-deactivate` entry points. A held run instead keeps the
/// installed links exactly as they are, pulls and activates nothing more,
/// runs no deactivation or config hooks, and commits no lifecycle change,
/// but still runs Tools, so the host can install the Dot that knows the
/// keys and converge on the next run. A fresh host with nothing installed
/// stays empty: nothing is activated from a selection this release cannot
/// read. Prune is skipped like any run with such keys.
fn hold(
    inputs: &EngineInputs<'_>,
    mut update: UpdateState,
    overlay: Agg,
    err: &mut dyn std::io::Write,
) -> ConvergeOut {
    if !inputs.hold_warned.replace(true) && !inputs.handed_warnings.contains(HOLD_WARNING) {
        let _ = writeln!(err, "{HOLD_WARNING}");
    }
    update.held = true;
    ConvergeOut {
        rc: 0,
        state: update,
        overlay,
    }
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
    /// Keys from a newer Dot hold the installed overlay set (see [`hold`]).
    held: bool,
}

/// Mutable update streams shared by the sync, converge, and finalize phases.
/// Keeping the paired streams together prevents orchestration signatures from
/// growing a positional stdout/stderr tail.
struct UpdateIo<'a> {
    out: &'a mut dyn std::io::Write,
    err: &'a mut dyn std::io::Write,
}

/// A live update stream that forwards every row immediately while remembering
/// delivery failure. Results pass through untouched, so explicit delivery
/// checks behave exactly as with the raw stream; the engine converts a
/// remembered failure into its exit-1 delivery contract at the end, exactly
/// like the old end-of-run flush.
///
/// A write rejected by the outward-write abort latch is not remembered. The
/// latch is raised only by a supervisor that has already decided the terminal
/// outcome (a trusted provider's 128+signal exit, a deadline, or incomplete
/// cleanup) and deliberately discards its queued rows so an unread caller
/// pipe cannot hold teardown. That owner reports its own status; recording
/// the discard here would rewrite a clean provider cancellation (130) into
/// the ordinary delivery failure (1) whenever the provider's rows happened to
/// be queued behind the blocked pipe when it exited.
struct LiveSink<'a> {
    inner: &'a mut dyn std::io::Write,
    failed: bool,
}

impl LiveSink<'_> {
    fn failed(&self) -> bool {
        self.failed
    }

    /// Remember a delivery failure unless it is an intentional abort. The
    /// latch is sampled at the failing call: it is raised before the aborted
    /// write returns and cleared only after the aborting supervisor has
    /// joined its relay, so no aborted write can observe it already cleared.
    fn record(&mut self, error: std::io::Error) -> std::io::Error {
        if !crate::cleanup::outward_write_aborted() {
            self.failed = true;
        }
        error
    }
}

impl std::io::Write for LiveSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.inner.write_all(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => Err(self.record(error)),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().map_err(|error| self.record(error))
    }
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
            held: false,
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
    fn close(&self, stage: &mut Stage, forced: &str, verbose: Option<&str>) -> Vec<u8> {
        repo_finish(
            stage,
            forced,
            &self.current.to_string(),
            &self.changed.to_string(),
            &self.failed.to_string(),
            &self.skipped.to_string(),
            &self.changed_items,
            verbose,
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
    err: &mut dyn std::io::Write,
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
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> SyncDone {
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
            Agg::zero(),
            None,
            None,
            false,
            None,
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
    // Probe already-cloned overlays while the base fetch runs; the overlay
    // rounds below skip only fetches a probe proves would change nothing.
    // The guard lives until this phase returns, abandoning unused probes.
    let prefetch = start_prefetch(inputs);
    // `OVERLAYS=()` plus the deferred base pull.
    let candidate = pull_candidate(inputs, &[]);
    let pull_inputs = crate::repos_pull_fleet::PullAllInputs {
        entries: &[],
        extra_args: inputs.extra_args,
        home: inputs.home,
        dot_quiet: inputs.dot_quiet,
        dot_verbose: inputs.dot_verbose,
        ui_total: Some(stage_total(inputs)),
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
    let outcome = crate::repos_pull_fleet::pull_all(&pull_inputs, stage, moves, out, err);
    // Pulls move HEAD (rebase, fast-forward, fresh clone) and the
    // failure path restores generations, so memoized revisions are
    // no longer trustworthy. Unconditional: invalidation is cheap,
    // and a failed pull may still have moved HEAD. Tracking names
    // survive clean pulls, but a failed pull can detach HEAD, so
    // upstream answers go too.
    crate::startup::invalidate_revision_cache();
    crate::repos_base::invalidate_client_match_cache();
    crate::overlays::invalidate_upstream_cache();
    if outcome.rc != 0 || outcome.failed > 0 {
        let close = Agg::base(&outcome).close(stage, "1", inputs.dot_verbose);
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
            let _ = err.write_all(failure.line().as_bytes());
            let _ = err.write_all(b"\n");
            let close = Agg::base(&outcome).close(stage, "1", inputs.dot_verbose);
            let _ = out.write_all(&close);
            restore_generation(inputs, base, &snapshot, &[], err);
            return SyncDone {
                rc: 1,
                frozen: true,
                state: UpdateState::new(inputs.config.clone()),
            };
        }
    };
    warn_reloaded_keys(inputs, &config, err);
    sync_tail(
        inputs,
        UpdateState::new(config),
        stage,
        moves,
        out,
        err,
        Agg::base(&outcome),
        Some(base),
        Some(snapshot),
        outcome.deferred,
        Some(&prefetch),
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
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    mut agg: Agg,
    base: Option<&Base>,
    snapshot: Option<InstalledSnapshot>,
    close_active: bool,
    prefetch: Option<&crate::repos_prefetch::Prefetch>,
) -> SyncDone {
    let mut io = UpdateIo { out, err };
    let mut conv = converge_overlays(inputs, state, stage, moves, &mut io, prefetch);
    agg.fold_agg(&conv.overlay);
    if conv.rc != 0 {
        if close_active {
            let close = agg.close(stage, "1", inputs.dot_verbose);
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
    if conv.state.held {
        // Nothing about the lifecycle changes this run. The base pull may
        // have restored base paths that installed links shadowed; put the
        // installed generation back, with no active set to fall back on
        // (nothing new may be linked).
        if let (Some(base), Some(snapshot)) = (base, snapshot.as_ref()) {
            restore_generation(inputs, base, snapshot, &[], io.err);
        }
        if close_active {
            let close = agg.close(stage, "0", inputs.dot_verbose);
            let _ = io.out.write_all(&close);
        }
        return SyncDone {
            rc: 0,
            frozen: false,
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
            let close = agg.close(stage, "1", inputs.dot_verbose);
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
        let close = agg.close(stage, "0", inputs.dot_verbose);
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
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    progress_done: &str,
    progress_total: &str,
    prefetch: Option<&crate::repos_prefetch::Prefetch>,
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
    // Let in-flight probes finish (or abandon them) before the lanes start,
    // so probes and fetches never exceed the job bound together.
    if let Some(probes) = prefetch {
        probes.settle();
    }
    crate::repos_pull_fleet::pull_overlays(
        &crate::repos_pull_fleet::PullOverlaysInputs {
            entries,
            extra_args: inputs.extra_args,
            home: inputs.home,
            ui_total: Some(stage_total(inputs)),
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
            prefetch,
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
    prefetch: Option<&crate::repos_prefetch::Prefetch>,
) -> ConvergeOut {
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
        let _ = io
            .err
            .write_all(format!("dot: profile: {}\n", error.message).as_bytes());
        return fail(update, overlay);
    }
    // Definition keys print after selection (`converge_profiles`), which
    // keeps only those in profiles this host includes.
    if update.profiles.present {
        return converge_profiles(inputs, stage, moves, io, update, overlay, prefetch);
    }
    let mut dstate = crate::overlays::State::default();
    if discover_active(inputs, &mut dstate, io.err, true).is_err() {
        return fail(update, overlay);
    }
    if skew_holds(&dstate.unknown_keys) {
        return hold(inputs, update, overlay, io.err);
    }
    let entries = use_set(&mut dstate, "eligible");
    update.capture(&dstate);
    let mut preflight_state = crate::overlays::State {
        overlays: entries.clone(),
        ..Default::default()
    };
    if let Err(warning) = crate::overlays::preflight(&mut preflight_state, inputs.home) {
        let _ = io.err.write_all(warning.as_bytes());
        let _ = io.err.write_all(b"\n");
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
        let _ = io.out.write_all(&stage.update(
            &detail,
            crate::update_engine::now_secs(),
            inputs.dot_verbose,
        ));
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
        prefetch,
    );
    overlay.fold_overlay(&outcome);
    let failed = outcome.tally.failed;
    let phase_ok = crate::update::overlay_phase_ok(outcome.rc, Some(&failed.to_string()));
    // Rediscover before returning, even on a failed phase.
    if discover_active(inputs, &mut dstate, io.err, true).is_err() {
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
    mut update: UpdateState,
    mut overlay: Agg,
    prefetch: Option<&crate::repos_prefetch::Prefetch>,
) -> ConvergeOut {
    let fail = |state: UpdateState, overlay: Agg| ConvergeOut {
        rc: 1,
        state,
        overlay,
    };
    if let Err(error) = update.profiles.select_base() {
        let _ = io
            .err
            .write_all(format!("dot: profile: {}\n", error.message).as_bytes());
        return fail(update, overlay);
    }
    let mut state = crate::overlays::State::default();
    if discover_selected(inputs, &mut state, &update.profiles.overlay_names, io.err).is_err() {
        return fail(update, overlay);
    }
    // A skipped `base` overlay already leaves the selection unknown (its
    // personal selectors go unread), so hold before pulling anything.
    if skew_holds(&state.unknown_keys) {
        return hold(inputs, update, overlay, io.err);
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
        let _ = io.err.write_all(warning.as_bytes());
        let _ = io.err.write_all(b"\n");
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
        let _ = io.out.write_all(&stage.update(
            &detail,
            crate::update_engine::now_secs(),
            inputs.dot_verbose,
        ));
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
        prefetch,
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
            let _ = io
                .err
                .write_all(b"dot: profile: cannot determine current user\n");
            update.active = entries;
            return fail(update, overlay);
        }
    };
    let host = match crate::platform::detect_host() {
        Ok(host) => host,
        Err(_) => {
            let _ = io
                .err
                .write_all(b"dot: profile: cannot determine current short hostname\n");
            update.active = entries;
            return fail(update, overlay);
        }
    };
    let phase_refs: Vec<&str> = phase_one_active.iter().map(String::as_str).collect();
    let resolved = update.profiles.resolve_default(
        inputs.config_home,
        inputs.home,
        &phase_refs,
        &state.unknown_keys,
        &user,
        &host,
        inputs.euid,
    );
    // Keys read before a failure still explain the selection.
    warn_data_keys(inputs, &update.profiles.unknown_keys, io.err);
    if let Err(error) = resolved {
        let _ = io
            .err
            .write_all(format!("dot: profile: {}\n", error.message).as_bytes());
        update.active = entries;
        return fail(update, overlay);
    }
    if discover_selected(inputs, &mut state, &update.profiles.overlay_names, io.err).is_err() {
        update.active = entries;
        return fail(update, overlay);
    }
    if skew_holds(&update.profiles.unknown_keys) || skew_holds(&state.unknown_keys) {
        return hold(inputs, update, overlay, io.err);
    }
    entries = use_set(&mut state, "eligible");
    update.capture(&state);
    let mut preflight_state = crate::overlays::State {
        overlays: entries.clone(),
        ..Default::default()
    };
    if let Err(warning) = crate::overlays::preflight(&mut preflight_state, inputs.home) {
        let _ = io.err.write_all(warning.as_bytes());
        let _ = io.err.write_all(b"\n");
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
        prefetch,
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

/// Start overlay probes for the checkouts the current descriptors already
/// activate. Discovery here is silent and best effort: the authoritative
/// discovery (and its diagnostics) still runs in each round, and a probe for
/// an overlay no round selects is simply abandoned.
fn start_prefetch(inputs: &EngineInputs<'_>) -> crate::repos_prefetch::Prefetch {
    let mut state = crate::overlays::State::default();
    let mut discarded = Vec::new();
    let paths: Vec<String> = if discover_active(inputs, &mut state, &mut discarded, false).is_ok() {
        state
            .active
            .iter()
            .map(|record| crate::repos_base::overlay_path_sync(record))
            .filter(|(path, sync)| sync == "git" && crate::overlays::is_worktree(Path::new(path)))
            .map(|(path, _)| path)
            .collect()
    } else {
        Vec::new()
    };
    let ssh_config = Path::new(inputs.home).join(".ssh/config");
    // `DOT_UPDATE_JOBS` bounds concurrent remote work. The base fetch holds
    // one slot while probes run, so a bound of one keeps the old strictly
    // serial behavior.
    let limit = crate::repos_pull_fleet::jobs_bound(inputs.update_jobs).saturating_sub(1);
    crate::repos_prefetch::start(&paths, &ssh_config, limit)
}

/// Discover the eligible or active set into `entries`, mirroring
/// `_discover_overlays` plus `_dot_overlay_use_set`.
fn use_set(state: &mut crate::overlays::State, kind: &str) -> Vec<String> {
    let _ = crate::overlays::use_set(state, kind);
    state.overlays.clone()
}

/// Run `_discover_overlays` natively for the profiles-absent branch.
/// `report` sends unknown descriptor keys to `err` (the silent prefetch
/// probe must not consume their one warning).
fn discover_active(
    inputs: &EngineInputs<'_>,
    state: &mut crate::overlays::State,
    err: &mut dyn std::io::Write,
    report: bool,
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
    let result =
        crate::overlays::discover(state, Path::new(&conf_path), "", &discover_inputs, &matches);
    if report {
        warn_data_keys(inputs, &state.unknown_keys, err);
    }
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = err.write_all(format!("{error:?}\n").as_bytes());
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
    err: &mut dyn std::io::Write,
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
    let result =
        crate::overlays::discover(state, Path::new(&conf_path), "", &discover_inputs, &matches);
    warn_data_keys(inputs, &state.unknown_keys, err);
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = err.write_all(format!("{error}\n").as_bytes());
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
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
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
        let _ = out.write_all(&outcome.stdout);
        let _ = err.write_all(&outcome.stderr);
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
            let _ = err.write_all(message.as_bytes());
            let _ = err.write_all(b"\n");
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
/// a failed input side, with the Prune skip row between them when this
/// run counts a Prune stage.
fn skip_inputs_rows(
    stage: &mut Stage,
    out: &mut dyn std::io::Write,
    reason: &str,
    prune: Option<PruneSkip>,
) {
    let open = stage.start(
        b"Tools",
        Some(b"skipping configured dependencies"),
        crate::update_engine::now_secs(),
        None,
    );
    let _ = out.write_all(&open);
    let close = stage.finish(
        b"warning",
        format!("{reason}; dependencies skipped").as_bytes(),
        crate::update_engine::now_secs(),
    );
    let _ = out.write_all(&close);
    if let Some(skip) = prune {
        prune_skip_row(stage, out, None, skip);
    }
    let open = stage.start(
        b"Configs",
        Some(b"skipping config hooks"),
        crate::update_engine::now_secs(),
        None,
    );
    let _ = out.write_all(&open);
    let close = stage.finish(
        b"warning",
        format!("{reason}; config hooks skipped").as_bytes(),
        crate::update_engine::now_secs(),
    );
    let _ = out.write_all(&close);
}

/// Why the Prune stage removed nothing this run. Every reason mirrors a
/// condition under which the Tools stage also did not converge the
/// dependency config, so prune never acts on a configuration Dot did not
/// trust enough to install from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PruneSkip {
    /// Profile resolution, repository sync, or overlay linking failed, or
    /// the generation is frozen: the checked-out config may be partial.
    Inputs,
    /// Profile deactivation failed, so the Tools stage never ran.
    Retire,
    /// No dependency provider is configured (or this invocation skips it).
    NoProvider,
    /// Shdeps could not be prepared, so there is nothing to prune with.
    Unavailable,
    /// This run met keys it does not know in profile, selector, or
    /// descriptor files, so the dependency configs it linked may lack
    /// some a newer Dot would link (see `warn_data_keys`).
    UnknownKeys,
}

impl PruneSkip {
    /// Stage row (status, detail) for this skip.
    fn row(self) -> (&'static [u8], &'static [u8]) {
        match self {
            PruneSkip::Inputs => (b"warning", b"repository sync failed; prune skipped"),
            PruneSkip::Retire => (b"warning", b"profile deactivation failed; prune skipped"),
            PruneSkip::NoProvider => (b"ok", b"no dependency provider"),
            PruneSkip::Unavailable => (b"warning", b"shdeps unavailable; prune skipped"),
            PruneSkip::UnknownKeys => (b"warning", b"keys from a newer dot; prune skipped"),
        }
    }
}

/// The provider that converged this generation's Tools stage, retained so
/// Prune runs with the same validated snapshot and environment.
struct PruneReady<'a> {
    provider: crate::shdeps_provider::Inputs<'a>,
    prepared: crate::shdeps_provider::Prepared,
}

/// Outcome of the Prune stage for the finalize status fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PruneStatus {
    /// Pruned cleanly or skipped.
    Ok,
    /// Prune ran and failed; later stages still run.
    Failed,
    /// Interrupted or teardown incomplete: stop with this status.
    Stop(i32),
}

/// Whether this run renders a Prune stage (and counts it in the stage
/// total). Known before the first row: the mode comes from the invocation
/// environment, never from a repository that sync may change.
fn prune_row(caller: Caller, mode: PruneMode, cron: bool) -> bool {
    caller == Caller::Update && mode.applies(cron)
}

/// Stage count for this invocation: Repos, Overlays, Tools, Configs, and
/// Cleanup, plus Prune when this run prunes.
fn stage_total(inputs: &EngineInputs<'_>) -> &'static str {
    if prune_row(inputs.caller, inputs.prune_mode, inputs.flags.cron) {
        "6"
    } else {
        "5"
    }
}

/// Render the Prune stage as skipped, naming the reason.
fn prune_skip_row(
    stage: &mut Stage,
    out: &mut dyn std::io::Write,
    verbose: Option<&str>,
    skip: PruneSkip,
) {
    let open = stage.start(
        b"Prune",
        Some(b"skipping dependency prune"),
        now_secs(),
        verbose,
    );
    let _ = out.write_all(&open);
    let (status, detail) = skip.row();
    let close = stage.finish(status, detail, now_secs());
    let _ = out.write_all(&close);
}

/// Run (or explain skipping) `shdeps prune -y` as its own stage.
///
/// Stdout (the removal report) renders as detail rows, which quiet and cron
/// runs drop like the old `shdeps prune -y > /dev/null` cron entry.
/// Provider stderr always reaches stderr, and a quiet run adds one failure
/// line, so cron reports warnings and failures only.
fn prune_stage(
    inputs: &EngineInputs<'_>,
    stage: &mut Stage,
    io: &mut UpdateIo<'_>,
    gate: Result<PruneReady<'_>, PruneSkip>,
) -> PruneStatus {
    let ready = match gate {
        Ok(ready) => ready,
        Err(skip) => {
            prune_skip_row(stage, io.out, inputs.dot_verbose, skip);
            return PruneStatus::Ok;
        }
    };
    let open = stage.start(
        b"Prune",
        Some(b"pruning orphaned dependencies"),
        now_secs(),
        inputs.dot_verbose,
    );
    let _ = io.out.write_all(&open);
    let outcome = crate::shdeps_provider::prune(&ready.provider, &ready.prepared);
    if outcome.interrupted.is_some() {
        return PruneStatus::Stop(interruption_status(outcome.interrupted));
    }
    if outcome.abort || cancelled() {
        return PruneStatus::Stop(interruption_status(None));
    }
    let close = if outcome.status == 0 {
        stage.finish(b"ok", b"orphaned dependencies checked", now_secs())
    } else {
        stage.finish(
            b"failed",
            format!("dependency prune failed (exit {})", outcome.status).as_bytes(),
            now_secs(),
        )
    };
    let _ = io.out.write_all(&close);
    for line in outcome.stdout.split(|byte| *byte == b'\n') {
        let text = crate::progress_ui::sanitize_untrusted_text(line.trim_ascii());
        if text.is_empty() {
            continue;
        }
        let (row, _) = crate::progress_ui::detail(
            inputs.palette,
            quiet(inputs),
            false,
            &text,
            inputs.multibyte,
        );
        let _ = io.out.write_all(&row);
    }
    let _ = io.err.write_all(&outcome.stderr);
    if outcome.status == 0 {
        return PruneStatus::Ok;
    }
    if quiet(inputs) {
        warn_row(
            io.err,
            inputs.palette,
            &format!("  warning: shdeps prune failed (exit {})", outcome.status),
        );
    }
    PruneStatus::Failed
}

/// Whether an update's final config degrades the run: it ignored a key
/// that is a near miss of a known key, so the setting that key most
/// likely meant kept its default (a misspelled `dependency_provider`
/// silently skips Shdeps, including Dot's own upgrade).
///
/// Like a failed Tools stage, this makes `dot update` exit 1 and records
/// `degraded update config` under `--cron`, so neither `update.log` nor
/// `dot doctor` reports the run as clean. Keys without a suggestion never
/// degrade: they are most likely from a newer Dot and safe to miss for a
/// cycle (see the key rules in docs/configuration.md). `dot init` is
/// exempt: it already warns about every key the clone brings, and a
/// failed convergence would fail the installation itself over a typo.
fn config_degraded(caller: Caller, config: &crate::config::Config) -> bool {
    caller == Caller::Update
        && config
            .unknown_keys
            .iter()
            .any(crate::config::UnknownKey::degrades_update)
}

/// `_dot_update_finalize` natively: checkpoint, link phase (or the
/// frozen preservation rows), lifecycle retire, the provider-none
/// tools stage, the prune stage, the empty merges close,
/// lifecycle commit, worktree normalize, and `_ui_done`. Returns the
/// update status.
///
/// `degraded` receives the degraded Config/Tools/Prune stages only when
/// they are the sole reason for a nonzero status (the run otherwise
/// converged); any other failure leaves it empty so the cron record reads
/// `fail`. After a provider re-exec it carries the continuation's value.
#[allow(clippy::too_many_arguments)]
fn finalize(
    inputs: &EngineInputs<'_>,
    state: &mut UpdateState,
    stage: &mut Stage,
    io: &mut UpdateIo<'_>,
    now_secs: i64,
    update_status: i32,
    frozen: bool,
    degraded: &mut crate::update_status::Degraded,
) -> i32 {
    if cancelled() {
        return 1;
    }
    let mut status = update_status;
    let mut inputs_ready = status == 0;
    // Prune trusts exactly what the Tools stage trusted: it runs only after
    // a prepared provider converged this generation (see the Tools branch).
    let prune_this_run = prune_row(inputs.caller, inputs.prune_mode, inputs.flags.cron);
    // Config, Tools, and Prune degradation is tracked apart from `status`
    // so the cron record can tell a converged-but-degraded run from one
    // that did not converge. Each still makes the update exit 1 (folded in
    // below). `state.config` is final here: the defensive reload ran.
    let mut stages = crate::update_status::Degraded {
        config: config_degraded(inputs.caller, &state.config),
        ..crate::update_status::Degraded::default()
    };
    let checkpoint = crate::shdeps::checkpoint_in(Path::new(inputs.state_home));
    if !crate::shdeps::consume_checkpoint(&checkpoint, inputs.source_root_git) {
        let close = crate::progress_ui::done(
            inputs.palette,
            quiet(inputs),
            Some("1"),
            now_secs,
            crate::update_engine::now_secs(),
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
            crate::update_engine::now_secs(),
            inputs.dot_verbose,
        );
        let _ = io.out.write_all(&open);
        let close = stage.finish(
            b"warning",
            b"profile resolution or repository sync failed",
            crate::update_engine::now_secs(),
        );
        let _ = io.out.write_all(&close);
        status = 1;
        inputs_ready = false;
    } else if state.held {
        // The installed links are the generation to keep (see `hold`).
        let open = stage.start(
            b"Overlays",
            Some(b"preserving installed overlay links"),
            crate::update_engine::now_secs(),
            inputs.dot_verbose,
        );
        let _ = io.out.write_all(&open);
        let close = stage.finish(
            b"warning",
            b"overlay set held for a newer dot",
            crate::update_engine::now_secs(),
        );
        let _ = io.out.write_all(&close);
    } else {
        let link_inputs = crate::repos_link_all::Inputs {
            entries: &state.active,
            home: inputs.home,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            update_jobs: inputs.update_jobs,
            ui_total: Some(stage_total(inputs)),
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
        let outcome = crate::repos_link_all::link_overlays(&link_inputs, stage, io.out, io.err);
        if outcome.rc != 0 {
            status = 1;
            inputs_ready = false;
        }
    }
    if cancelled() {
        return 1;
    }
    if !inputs_ready {
        skip_inputs_rows(
            stage,
            io.out,
            "repository synchronization failed",
            prune_this_run.then_some(PruneSkip::Inputs),
        );
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
        // A held run deactivates nothing: the overlays this release would
        // retire may be exactly the ones the newer Dot keeps.
        let retired = if state.held {
            0
        } else {
            crate::profile_lifecycle::retire(
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
            )
        };
        if retired != 0 {
            status = 1;
            skip_inputs_rows(
                stage,
                io.out,
                "profile deactivation failed",
                prune_this_run.then_some(PruneSkip::Retire),
            );
        } else {
            if cancelled() {
                return 1;
            }
            let provider_enabled =
                !inputs.skip_provider && state.config.provider == crate::config::Provider::Shdeps;
            let mut prune: Result<PruneReady<'_>, PruneSkip> = Err(PruneSkip::NoProvider);
            if !provider_enabled {
                let open = stage.start(
                    b"Tools",
                    Some(b"checking configured dependencies"),
                    crate::update_engine::now_secs(),
                    inputs.dot_verbose,
                );
                let _ = io.out.write_all(&open);
                let close = stage.finish(
                    b"ok",
                    b"no dependency provider",
                    crate::update_engine::now_secs(),
                );
                let _ = io.out.write_all(&close);
            } else {
                // The shell prepares Shdeps before opening the Tools stage.
                // Engine rows already stream live, so bootstrap/download
                // diagnostics keep their stream and execution-point order
                // without a handoff flush.
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
                match crate::shdeps_provider::prepare(&provider_inputs, &mut *io.out, &mut *io.err)
                {
                    Err(provider) if provider.interrupted.is_some() || cancelled() => {
                        return interruption_status(provider.interrupted);
                    }
                    Err(provider) if provider.abort => {
                        let _ = io.err.write_all(&provider.stderr);
                        return 1;
                    }
                    Err(provider) => {
                        if io.err.write_all(&provider.stderr).is_err() {
                            return 1;
                        }
                        let open = stage.start(
                            b"Tools",
                            Some(b"checking configured dependencies"),
                            crate::update_engine::now_secs(),
                            inputs.dot_verbose,
                        );
                        let _ = io.out.write_all(&open);
                        let close = stage.finish(
                            b"failed",
                            &provider.summary,
                            crate::update_engine::now_secs(),
                        );
                        let _ = io.out.write_all(&close);
                        stages.tools = true;
                        prune = Err(PruneSkip::Unavailable);
                    }
                    Ok(prepared) => {
                        let open = stage.start(
                            b"Tools",
                            Some(b"checking configured dependencies"),
                            crate::update_engine::now_secs(),
                            inputs.dot_verbose,
                        );
                        let _ = io.out.write_all(&open);
                        let provider = crate::shdeps_provider::update(
                            &provider_inputs,
                            &prepared,
                            stage,
                            &mut *io.out,
                            &mut *io.err,
                        );
                        if provider.interrupted.is_some() {
                            return interruption_status(provider.interrupted);
                        }
                        if provider.abort || cancelled() {
                            return interruption_status(None);
                        }
                        let close = stage.finish(
                            &provider.stage_status,
                            &provider.summary,
                            crate::update_engine::now_secs(),
                        );
                        let _ = io.out.write_all(&close);
                        let _ = io.out.write_all(&provider.details);
                        if provider.status != 0 {
                            stages.tools = true;
                        }
                        if let Some(change) = provider.revision_change {
                            if cancelled() {
                                return 1;
                            }
                            if let Some(code) =
                                provider_reexec(inputs, io, &change, now_secs, degraded)
                            {
                                return code;
                            }
                        }
                        // A failed update (a dependency or post hook) still
                        // leaves this generation's config trusted: prune on.
                        // Otherwise release the provider snapshot now.
                        if prune_this_run {
                            prune = Ok(PruneReady {
                                provider: provider_inputs,
                                prepared,
                            });
                        }
                    }
                }
            }
            if cancelled() {
                return 1;
            }
            // Pruning uninstalls whatever no linked config declares. A run
            // whose overlay set a key from a newer Dot may have changed (a
            // skipped overlay, a fallback to `base`, an ignored key in an
            // included profile) may be missing configs that come back once
            // Dot upgrades, so it must not remove their packages in between.
            // A skipped selector that could not have won changed nothing.
            if prune.is_ok()
                && (state.held
                    || inputs
                        .data_warned
                        .borrow()
                        .iter()
                        .any(|key| key.effect != crate::unknown_keys::Effect::SelectorSkipped))
            {
                prune = Err(PruneSkip::UnknownKeys);
            }
            // Prune directly after Tools so it reads the same Shdeps config
            // the provider just converged, before any merge hook runs.
            if prune_this_run {
                match prune_stage(inputs, stage, io, prune) {
                    PruneStatus::Ok => {}
                    PruneStatus::Failed => stages.prune = true,
                    PruneStatus::Stop(code) => return code,
                }
                if cancelled() {
                    return 1;
                }
            }
            let extensions_dir = state
                .config
                .extensions_dir
                .as_deref()
                .unwrap_or(inputs.extensions_dir);
            // Config hooks read the active overlay set; a held run's set is
            // this release's partial reading, not what is installed.
            let merged = if state.held {
                let open = stage.start(
                    b"Configs",
                    Some(b"skipping config hooks"),
                    crate::update_engine::now_secs(),
                    inputs.dot_verbose,
                );
                let _ = io.out.write_all(&open);
                let close = stage.finish(
                    b"warning",
                    b"overlay set held; config hooks skipped",
                    crate::update_engine::now_secs(),
                );
                let _ = io.out.write_all(&close);
                crate::merges::Outcome { status: 0 }
            } else {
                crate::merges::run(
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
                        ascii: inputs.ascii,
                        ui_total: Some(stage_total(inputs)),
                        bar_width: inputs.bar_width,
                        log: inputs.log,
                    },
                    stage,
                    io.out,
                    io.err,
                )
            };
            if merged.status != 0 {
                status = 1;
            }
            if cancelled() {
                return 1;
            }
        }
    }
    // A failed Tools stage withholds the lifecycle commit exactly as it did
    // when it set `status` directly; Prune is deliberately not part of this.
    let lifecycle_status = if stages.tools { 1 } else { status };
    // A held run commits no lifecycle change (see `hold`).
    match lifecycle_publish_decision(inputs_ready && !state.held, lifecycle_status, cancelled()) {
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
    // Converged but degraded only when nothing else failed; the exit status
    // stays 1 either way, so callers and scripts see no change.
    *degraded = if status == 0 {
        stages
    } else {
        crate::update_status::Degraded::default()
    };
    if !stages.is_empty() {
        status = 1;
    }
    // Tools and Prune explain their exit 1 with a failed row; the config
    // stage has none, and its key warnings print before every stage row, so
    // repeat the reason beside "Done with errors". Quiet and cron runs skip
    // this: they print no rows, so the key warnings stand alone.
    if stages.config && !quiet(inputs) {
        for unknown in &state.config.unknown_keys {
            if let Some(known) = unknown.suggestion() {
                warn_row(
                    io.err,
                    inputs.palette,
                    &format!(
                        "  warning: update degraded: config key '{}' is ignored; did you mean '{known}'?",
                        unknown.key
                    ),
                );
            }
        }
    }
    let based = inputs.base.is_some_and(|base| base.exists());
    if based {
        let open = stage.start(
            b"Cleanup",
            Some(b"normalizing worktree"),
            crate::update_engine::now_secs(),
            inputs.dot_verbose,
        );
        let _ = io.out.write_all(&open);
        crate::repos_dirty::normalize_filtered(base_prefix.as_deref(), &state.active);
        let close = stage.finish(
            b"ok",
            b"worktree normalized",
            crate::update_engine::now_secs(),
        );
        let _ = io.out.write_all(&close);
    } else {
        let open = stage.start(
            b"Cleanup",
            Some(b"normalizing worktree"),
            crate::update_engine::now_secs(),
            inputs.dot_verbose,
        );
        let _ = io.out.write_all(&open);
        let close = stage.finish(b"ok", b"no base repo", crate::update_engine::now_secs());
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
        crate::update_engine::now_secs(),
        &reload_hint(inputs),
    );
    let _ = io.out.write_all(&close);
    status
}

/// Continue one provider-driven source-generation transition without touching
/// the process-global environment or reacquiring the already-held update lock.
///
/// `Some(status)` ends this run with `status`: a development checkout
/// continues in a nested in-process run, and a packaged release hands off to
/// its new binary ([`release_handoff`]). `None` finishes this run in place,
/// which is the fallback whenever a release cannot be handed off.
fn provider_reexec(
    inputs: &EngineInputs<'_>,
    io: &mut UpdateIo<'_>,
    change: &crate::shdeps_provider::RevisionChange,
    now_secs: i64,
    degraded: &mut crate::update_status::Degraded,
) -> Option<i32> {
    if cancelled() {
        return Some(1);
    }
    let (before, after, release) = (
        change.before.as_str(),
        change.after.as_str(),
        change.release,
    );
    // `dot init` converges inside its own transaction and an embedded
    // runtime cannot replace its process: on a release root both finish in
    // place, exactly as before releases could hand off.
    if release && !release_hands_off(inputs.caller, inputs.runtime.can_exec()) {
        return None;
    }
    if release && !(crate::shdeps::revision_valid(before) && crate::shdeps::revision_valid(after)) {
        // Releases never re-executed before, so unreadable metadata must not
        // turn into a new failure: keep the old finish-in-place behavior.
        warn_row(
            io.err,
            inputs.palette,
            "  warning: dot changed during the update but its release metadata is unreadable; finishing this update on the running dot",
        );
        return None;
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
            crate::update_engine::now_secs(),
            &reload_hint(inputs),
        );
        let _ = io.out.write_all(&close);
        return Some(1);
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
            crate::update_engine::now_secs(),
            &reload_hint(inputs),
        );
        let _ = io.out.write_all(&close);
        return Some(1);
    }
    if inputs.continuation {
        let path = crate::shdeps::checkpoint_in(Path::new(inputs.state_home));
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
            crate::update_engine::now_secs(),
            &reload_hint(inputs),
        );
        let _ = io.out.write_all(&close);
        return Some(1);
    }
    if release {
        return release_handoff(inputs, io, after, now_secs);
    }
    // The nested runtime keeps this run's child environment (no lock claim,
    // prune policy, or continuation variables: see `CONTINUATION_ENV`),
    // while the nested capture starts from the command environment so it
    // keeps the lock claim and flag exports this run was gathered from, plus
    // the continuation markers it reads as values.
    let mut env = inputs.env.clone();
    env.insert(OsString::from(REEXEC_ONCE_ENV), OsString::from("1"));
    env.insert(OsString::from(REEXEC_EXPECTED_ENV), OsString::from(after));
    let runtime = match crate::app::Runtime::from_env(inputs.runtime.env(), inputs.runtime.cwd()) {
        Ok(runtime) => runtime,
        Err(_) => return Some(1),
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
            let _ = io.err.write_all(failure.line().as_bytes());
            let _ = io.err.write_all(b"\n");
            return Some(1);
        }
    };
    warn_reloaded_keys(inputs, &config, io.err);
    if cancelled() {
        return Some(1);
    }
    // The continuation is the same command with the same prune mode. The
    // mode travels as a value because the runtime environment was scrubbed
    // of `DOT_SHDEPS_PRUNE` at entry.
    let mut gathered = match gather(
        inputs.caller,
        inputs.prune_mode,
        inputs.original_args,
        &runtime,
        &config,
        inputs.source_root_git,
        inputs.state_home,
        &env,
        runtime.cwd(),
    ) {
        Ok(gathered) => gathered,
        _ => return Some(1),
    };
    // One invocation, one warning per key: the continuation inherits
    // every key reported so far, not only the ones its config holds.
    gathered
        .config_warned
        .borrow_mut()
        .extend(inputs.config_warned.borrow().iter().cloned());
    gathered
        .data_warned
        .borrow_mut()
        .extend(inputs.data_warned.borrow().iter().cloned());
    gathered.hold_warned.set(inputs.hold_warned.get());
    gathered
        .handed_warnings
        .extend(inputs.handed_warnings.iter().cloned());
    let nested = gathered.inputs();
    if cancelled() {
        return Some(1);
    }
    // The continuation records its own cron outcome; handing its degraded
    // stages back lets the outer record match instead of reading `fail`.
    Some(run_gathered(
        &nested,
        &mut *io.out,
        &mut *io.err,
        now_secs,
        degraded,
    ))
}

/// Hand the rest of this `dot update` to the release binary the Tools stage
/// just installed: the old engine must not drive the new release's hook
/// runtime and library files. The binary entry point execs the continuation
/// after this run unwinds (see [`crate::handoff`]); `Some(0)` ends this half.
///
/// The continuation is the same command line under this process's own
/// environment plus what the command boundary consumed from it (the prune
/// policy, which the new process parses again) and the handoff contract: the
/// warning lines already printed ([`WARNED_ENV`], so none prints twice), the
/// lock claim it re-enters the held lock with, `DOT_REEXEC_ONCE` (a second
/// Dot change publishes the provider checkpoint instead of handing off
/// again), and `DOT_REEXEC_EXPECTED_REVISION` (the new binary proves its
/// identity at startup). Flag exports are not carried: the new process
/// derives them from the same arguments.
///
/// `None` finishes this run in place, as releases always did before: a new
/// binary that is missing or fails its startup probe must not stop the
/// update, only warn.
fn release_handoff(
    inputs: &EngineInputs<'_>,
    io: &mut UpdateIo<'_>,
    after: &str,
    started: i64,
) -> Option<i32> {
    let mut env = inputs.runtime.env().clone();
    if let Some(prune) = inputs.env.get(OsStr::new(PRUNE_ENV)) {
        env.insert(OsString::from(PRUNE_ENV), prune.clone());
    }
    if let Some(token) = inputs.update_lock_token {
        env.insert(OsString::from(LOCK_TOKEN_ENV), OsString::from(token));
    }
    env.insert(OsString::from(REEXEC_ONCE_ENV), OsString::from("1"));
    env.insert(OsString::from(REEXEC_EXPECTED_ENV), OsString::from(after));
    env.insert(
        OsString::from(REEXEC_STARTED_ENV),
        OsString::from(started.to_string()),
    );
    // Each warning prints once per invocation, across the exec too.
    env.insert(OsString::from(WARNED_ENV), continuation_warned(inputs));
    let binary = inputs.source_root_git.join("dot");
    let probe = probe_release_binary(inputs.runtime, &binary, &env);
    if cancelled() {
        return Some(1);
    }
    if let Err(problem) = probe {
        let short = after.get(..12).unwrap_or(after);
        warn_row(
            io.err,
            inputs.palette,
            &format!(
                "  warning: dot {short} was installed but {problem}; finishing this update on the running dot"
            ),
        );
        return None;
    }
    let mut args = vec![OsString::from("update")];
    args.extend(inputs.original_args.iter().cloned());
    let cron_state = inputs.flags.cron.then(|| PathBuf::from(inputs.state_home));
    let handoff = crate::handoff::Handoff::new(binary, args, env, cron_state);
    if !inputs.runtime.request_exec(handoff) {
        return None;
    }
    // The continuation's stage counter restarts at 1; say why.
    if !quiet(inputs) {
        let short = after.get(..12).unwrap_or(after);
        let _ = writeln!(
            io.out,
            "{}  continuing with dot {short}{}",
            inputs.palette.dim, inputs.palette.reset
        );
    }
    Some(0)
}

/// Check that the new release binary can run the continuation before this
/// process commits to replacing itself; an exec cannot be undone. `dot update
/// --help` under the continuation's exact environment takes the same startup
/// path as the continuation (strict release-root resolution, output relay,
/// re-exec guard, config load, client identity) and returns before the update
/// lock, so a wrong-platform, truncated, or mismatched binary, or one that
/// rejects this config, is caught while the old engine can still finish.
fn probe_release_binary(
    runtime: &crate::app::Runtime,
    binary: &Path,
    env: &BTreeMap<OsString, OsString>,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata =
        std::fs::symlink_metadata(binary).map_err(|_| "its binary is missing".to_string())?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("its binary is not an executable file".to_string());
    }
    let mut command = std::process::Command::new(binary);
    command
        .args(["update", "--help"])
        .env_clear()
        .envs(env)
        .current_dir(runtime.cwd())
        .stdin(std::process::Stdio::null());
    let timeout = runtime
        .value(PROBE_TIMEOUT_ENV)
        .and_then(OsStr::to_str)
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(
            crate::cleanup::REVISION_PROBE_TIMEOUT,
            std::time::Duration::from_secs,
        );
    let output = crate::cleanup::run_session_output(
        command,
        {
            // A test knob too large to add keeps the default bound.
            let now = std::time::Instant::now();
            Some(
                now.checked_add(timeout)
                    .unwrap_or(now + crate::cleanup::REVISION_PROBE_TIMEOUT),
            )
        },
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    );
    let failed = "its binary did not pass its startup check";
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            // The new binary's own diagnostic says why (for example a
            // revision mismatch); keep its first line, safe for a terminal.
            let line = output
                .stderr
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            let reason = crate::progress_ui::sanitize_untrusted_text(line);
            let reason = String::from_utf8_lossy(&reason);
            if reason.is_empty() {
                Err(failed.to_string())
            } else {
                Err(format!("{failed} ({reason})"))
            }
        }
        Err(_) => Err(failed.to_string()),
    }
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
    caller: Caller,
    prune_mode: PruneMode,
    update_lock_token: Option<String>,
    env: BTreeMap<OsString, OsString>,
    continuation: bool,
    config: crate::config::Config,
    config_warned: std::cell::RefCell<Vec<String>>,
    data_warned: std::cell::RefCell<Vec<crate::unknown_keys::DataKey>>,
    hold_warned: std::cell::Cell<bool>,
    handed_warnings: BTreeSet<String>,
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
            caller: self.caller,
            prune_mode: self.prune_mode,
            update_lock_token: self.update_lock_token.as_deref(),
            env: &self.env,
            continuation: self.continuation,
            config: &self.config,
            config_warned: &self.config_warned,
            data_warned: &self.data_warned,
            hold_warned: &self.hold_warned,
            handed_warnings: &self.handed_warnings,
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
    /// Command driving this run.
    pub caller: Caller,
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
#[allow(clippy::too_many_arguments)]
fn gather(
    caller: Caller,
    prune_mode: PruneMode,
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
        caller,
        prune_mode,
        update_lock_token: env_value(env, LOCK_TOKEN_ENV),
        env: env.clone(),
        continuation: env_value(env, REEXEC_ONCE_ENV).as_deref() == Some("1"),
        config: config.clone(),
        config_warned: std::cell::RefCell::new(
            config
                .unknown_keys
                .iter()
                .map(|unknown| unknown.key.clone())
                .collect(),
        ),
        data_warned: std::cell::RefCell::new(Vec::new()),
        hold_warned: std::cell::Cell::new(false),
        handed_warnings: handed_warnings(env.get(OsStr::new(WARNED_ENV)).map(OsString::as_os_str)),
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
        reexec_expected: env_value(env, REEXEC_EXPECTED_ENV),
    })
}

/// Wall-clock seconds for stage rows (`date +%s` equivalent).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve [`PRUNE_ENV`] once at the command boundary and return a runtime
/// whose environment no longer carries it.
///
/// The Shdeps provider and merge, lifecycle, and extension hooks run with the
/// runtime environment, and none of them should act on Dot's prune policy;
/// the parsed mode travels to the provider re-exec continuation as a value.
/// Plain helpers spawned without the runtime environment (for example
/// `git pull`) still inherit the process environment; they never read it.
/// `dot init` ignores the variable. An unrecognized value warns (even under
/// cron) and reads as `never`, so it can never stop an update, and never as
/// the unset default, so a typo also turns cron prune off instead of on.
fn prune_mode(
    runtime: &crate::app::Runtime,
    request: &UpdateRequest<'_>,
    stderr: &mut dyn std::io::Write,
) -> (PruneMode, crate::app::Runtime) {
    let raw = request
        .env
        .get(OsStr::new(PRUNE_ENV))
        .map(|value| value.to_string_lossy().into_owned());
    let scrubbed = runtime.without_env(PRUNE_ENV);
    if request.caller != Caller::Update {
        return (PruneMode::Never, scrubbed);
    }
    match PruneMode::parse(raw.as_deref()) {
        Some(mode) => (mode, scrubbed),
        None => {
            let line = invalid_prune_warning(raw.as_deref().unwrap_or_default());
            // A release continuation inherits the value; its first half
            // hands the line over once printed.
            let handed = handed_warnings(
                request
                    .env
                    .get(OsStr::new(WARNED_ENV))
                    .map(OsString::as_os_str),
            );
            if !handed.contains(&line) {
                let _ = stderr.write_all(&crate::progress_ui::warn_line(
                    &crate::progress_ui::Palette::empty(),
                    line.as_bytes(),
                ));
            }
            (PruneMode::Never, scrubbed)
        }
    }
}

/// The warning line (as printed, without its newline) for an unrecognized
/// [`PRUNE_ENV`] value.
fn invalid_prune_warning(raw: &str) -> String {
    let value = crate::progress_ui::sanitize_untrusted_text(raw.as_bytes());
    format!(
        "  warning: ignoring {PRUNE_ENV}={}; expected never, cron, or always",
        String::from_utf8_lossy(&value)
    )
}

/// The [`WARNED_ENV`] value a release continuation receives: every warning
/// this invocation printed so far ([`EngineInputs::warned_handoff`]) plus the
/// invalid prune-policy warning, which the command boundary prints before the
/// engine and which the continuation would otherwise repeat.
fn continuation_warned(inputs: &EngineInputs<'_>) -> OsString {
    let mut lines = handed_warnings(Some(&inputs.warned_handoff()));
    if inputs.caller == Caller::Update {
        if let Some(raw) = inputs.env.get(OsStr::new(PRUNE_ENV)) {
            let raw = raw.to_string_lossy();
            if PruneMode::parse(Some(&raw)).is_none() {
                lines.insert(invalid_prune_warning(&raw));
            }
        }
    }
    // `handed_warnings` already dropped empty lines; none hold a newline.
    OsString::from(lines.into_iter().collect::<Vec<_>>().join("\n"))
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
    let continuation = is_continuation(runtime);
    let (prune_mode, mut runtime) = prune_mode(runtime, request, streams.stderr);
    // The lock claim reaches hook workers explicitly, and a continuation
    // reads its markers from the command environment. A release
    // continuation inherits all of them in its process environment, so keep
    // them out of what providers and hooks inherit, like any other run.
    // `CONTINUATION_ENV` includes the handed-over warnings, which concern
    // this engine only.
    for key in CONTINUATION_ENV {
        runtime = runtime.without_env(key);
    }
    let runtime = &runtime;
    // A release continuation's run started with its first half.
    let started = continuation
        .then(|| env_value(request.env, REEXEC_STARTED_ENV))
        .flatten()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .filter(|epoch| *epoch > 0 && *epoch <= now_secs())
        .unwrap_or_else(now_secs);
    let gathered = match gather(
        request.caller,
        prune_mode,
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
            record_continuation_failure(continuation, request.args, request.state_home);
            return error.code();
        }
    };
    // Every engine row streams through these sinks as its phase files it;
    // either sink remembers a delivery failure so the exit status still
    // reports undelivered output exactly like the old end-of-run flush.
    let mut out = LiveSink {
        inner: &mut *streams.stdout,
        failed: false,
    };
    let mut err = LiveSink {
        inner: &mut *streams.stderr,
        failed: false,
    };
    let mut degraded = crate::update_status::Degraded::default();
    let code = run_gathered(
        &gathered.inputs(),
        &mut out,
        &mut err,
        started,
        &mut degraded,
    );
    if out.failed() || err.failed() {
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
/// refreshes the last-success stamp on success. A run that converged
/// (sync, links, deactivation, configs; no lifecycle commit failure)
/// but whose Tools or Prune stage failed, or whose config holds a likely
/// misspelled key ([`config_degraded`]), records `degraded` with those
/// stages and refreshes only the convergence stamp, so `dot doctor`
/// can tell it from a frozen host; its exit status stays 1.
/// Interrupted runs record nothing (cancellation is not an outcome).
/// Every other run, cron or not, also overwrites the any-trigger
/// `update.last-run` stamp with the same classification; non-cron runs
/// write nothing else. `degraded` reports this run's degraded stages to
/// a provider re-exec's outer run.
fn run_gathered(
    inputs: &EngineInputs<'_>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    now_secs: i64,
    degraded: &mut crate::update_status::Degraded,
) -> i32 {
    *degraded = crate::update_status::Degraded::default();
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
            crate::update_status::OUTCOME_SKIP,
            "dirty",
            &detail,
        );
        // Without this, a host whose cron keeps skipping would read "cron
        // update has never run" once its last hand-run update aged out.
        crate::update_status::record_last_run(
            Path::new(inputs.state_home),
            now_secs,
            crate::update_status::OUTCOME_SKIP,
            crate::update_status::Trigger::Cron,
            crate::update_status::Degraded::default(),
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
    let rc = run_gathered_inner(inputs, out, err, now_secs, degraded);
    // A release handoff ends this half before the run's outcome is known;
    // the exec'd continuation records the run (or the handoff records its
    // failure when it cannot exec), so this half records nothing.
    if cancelled() || inputs.runtime.exec_pending() {
        return rc;
    }
    use crate::update_status::{OUTCOME_DEGRADED, OUTCOME_FAIL, OUTCOME_OK};
    let state_home = Path::new(inputs.state_home);
    let outcome = if rc == 0 {
        OUTCOME_OK
    } else if !degraded.is_empty() {
        OUTCOME_DEGRADED
    } else {
        OUTCOME_FAIL
    };
    if inputs.flags.cron {
        let detail = if outcome == OUTCOME_DEGRADED {
            degraded.detail()
        } else {
            String::new()
        };
        crate::update_status::append_outcome(state_home, now_secs, outcome, "update", &detail);
        if outcome == OUTCOME_OK {
            crate::update_status::record_success(state_home, now_secs);
            // Exit 0 implies no failed stage: a clean convergence.
            crate::update_status::record_converged(
                state_home,
                now_secs,
                crate::update_status::Degraded::default(),
            );
        } else if outcome == OUTCOME_DEGRADED {
            crate::update_status::record_converged(state_home, now_secs, *degraded);
        }
    }
    // Every run, cron or not, leaves its outcome for `dot doctor`, so a host
    // updated by hand reports its last result instead of "unknown".
    crate::update_status::record_last_run(
        state_home,
        now_secs,
        outcome,
        trigger(inputs),
        *degraded,
    );
    rc
}

/// What started this run, as recorded in `update.last-run`.
fn trigger(inputs: &EngineInputs<'_>) -> crate::update_status::Trigger {
    use crate::update_status::Trigger;
    if inputs.flags.cron {
        Trigger::Cron
    } else if inputs.caller == Caller::Init {
        Trigger::Init
    } else {
        Trigger::Manual
    }
}

/// `_dot_update` natively: flag-driven stages around [`sync_repos`]
/// and finalization with the defensive policy reload between them.
fn run_gathered_inner(
    inputs: &EngineInputs<'_>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    now_secs: i64,
    degraded: &mut crate::update_status::Degraded,
) -> i32 {
    if cancelled() {
        return 1;
    }
    // `_ui_begin 5` (6 with a Prune stage): the update always runs counted
    // (the assignment overwrites any ambient total, like the shell).
    let mut stage = Stage::begin(
        inputs.palette.clone(),
        stage_total(inputs),
        quiet(inputs),
        inputs.live,
        inputs.multibyte,
        inputs.ascii,
    );
    let mut moves = crate::temp::MoveCache::default();
    let mut sync = sync_repos(inputs, &mut stage, &mut moves, out, err);
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
            degraded,
        );
        return rc;
    }
    // Defensive reload before provider selection continues (a
    // failure closes without finalizing, like the shell: the
    // loader prints its own diagnostic, then `_ui_done 1`).
    let startup = startup_inputs(inputs);
    match crate::startup::preflight(&startup) {
        Ok(config) => {
            warn_reloaded_keys(inputs, &config, err);
            sync.state.config = config;
        }
        Err(failure) => {
            let _ = err.write_all(failure.line().as_bytes());
            let _ = err.write_all(b"\n");
            let close = crate::progress_ui::done(
                inputs.palette,
                quiet(inputs),
                Some("1"),
                now_secs,
                crate::update_engine::now_secs(),
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
        degraded,
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

    #[test]
    fn warned_value_round_trips_every_printed_warning() {
        let data = crate::unknown_keys::DataKey {
            path: "/h/.config/dot/overlays.d/20-beta.conf".to_string(),
            line: 2,
            key: "future_key".to_string(),
            effect: crate::unknown_keys::Effect::OverlaySkipped("beta".to_string()),
        };
        let handed: BTreeSet<String> = ["earlier line".to_string()].into();
        let value = warned_value(
            &handed,
            &["future_key".to_string()],
            std::slice::from_ref(&data),
            true,
        );
        let parsed = handed_warnings(Some(&value));
        let expected: BTreeSet<String> = [
            "earlier line".to_string(),
            "dot: config: warning: unknown key 'future_key' ignored (newer dot?)".to_string(),
            data.warning(),
            HOLD_WARNING.to_string(),
        ]
        .into();
        assert_eq!(parsed, expected);
        assert!(handed_warnings(None).is_empty());
        assert!(handed_warnings(Some(OsStr::new(""))).is_empty());
        let odd: BTreeSet<String> = ["a\nb".to_string()].into();
        assert!(handed_warnings(Some(&warned_value(&odd, &[], &[], false))).is_empty());
    }

    #[test]
    fn a_continuation_warns_an_invalid_prune_policy_nobody_handed_over() {
        // Only a handed line is skipped; being a continuation is not enough.
        let request_env = |handed: Option<&str>| {
            let mut env = BTreeMap::from([
                (OsString::from("HOME"), OsString::from("/tmp")),
                (OsString::from(REEXEC_ONCE_ENV), OsString::from("1")),
                (OsString::from(PRUNE_ENV), OsString::from("weekly")),
            ]);
            if let Some(line) = handed {
                env.insert(OsString::from(WARNED_ENV), OsString::from(line));
            }
            env
        };
        let config = crate::config::Config {
            version: 1,
            extension_api: false,
            extensions_dir: None,
            provider: crate::config::Provider::None,
            default_profile: "base".to_string(),
            shdeps_update_policy: crate::config::UpdatePolicy::Pinned,
            policy_from_env: false,
            unknown_keys: Vec::new(),
        };
        let line = invalid_prune_warning("weekly");
        for (handed, expected) in [
            (None, format!("{line}\n")),
            (Some(line.as_str()), String::new()),
        ] {
            let env = request_env(handed);
            let runtime =
                crate::app::Runtime::from_env(&env, Path::new("/tmp")).expect("absolute cwd");
            let request = UpdateRequest {
                caller: Caller::Update,
                config: &config,
                env: &env,
                args: &[],
                state_home: Path::new("/tmp"),
            };
            let mut stderr = Vec::new();
            let (mode, _) = prune_mode(&runtime, &request, &mut stderr);
            assert_eq!(mode, PruneMode::Never);
            assert_eq!(
                String::from_utf8_lossy(&stderr),
                expected,
                "handed {handed:?}"
            );
        }
    }

    #[test]
    fn unset_prune_policy_defaults_to_cron_for_update_only() {
        // The command boundary, not just the parser: unset means cron for
        // `dot update`, init ignores every value, and a typo reads as
        // `never` (never as the default) so it cannot enable deletion.
        let config = crate::config::Config {
            version: 1,
            extension_api: false,
            extensions_dir: None,
            provider: crate::config::Provider::None,
            default_profile: "base".to_string(),
            shdeps_update_policy: crate::config::UpdatePolicy::Pinned,
            policy_from_env: false,
            unknown_keys: Vec::new(),
        };
        for (caller, value, expected) in [
            (Caller::Update, None, PruneMode::Cron),
            (Caller::Update, Some(""), PruneMode::Cron),
            (Caller::Update, Some("never"), PruneMode::Never),
            (Caller::Update, Some("weekly"), PruneMode::Never),
            (Caller::Init, None, PruneMode::Never),
            (Caller::Init, Some("always"), PruneMode::Never),
        ] {
            let mut env = BTreeMap::from([(OsString::from("HOME"), OsString::from("/tmp"))]);
            if let Some(value) = value {
                env.insert(OsString::from(PRUNE_ENV), OsString::from(value));
            }
            let runtime =
                crate::app::Runtime::from_env(&env, Path::new("/tmp")).expect("absolute cwd");
            let request = UpdateRequest {
                caller,
                config: &config,
                env: &env,
                args: &[],
                state_home: Path::new("/tmp"),
            };
            let (mode, scrubbed) = prune_mode(&runtime, &request, &mut Vec::new());
            assert_eq!(mode, expected, "{caller:?} {value:?}");
            assert!(scrubbed.value(PRUNE_ENV).is_none(), "{caller:?} {value:?}");
        }
    }

    #[test]
    fn only_update_from_the_binary_entry_hands_a_release_off() {
        assert!(release_hands_off(Caller::Update, true));
        // `dot init` finishes in place even where the process could exec:
        // its transaction owns the run, and a handoff would exec `dot
        // update` after init returned.
        assert!(!release_hands_off(Caller::Init, true));
        assert!(!release_hands_off(Caller::Update, false));
        assert!(!release_hands_off(Caller::Init, false));
    }

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
    fn prune_row_follows_mode_and_never_appears_for_init() {
        for (caller, mode, cron, expected) in [
            (Caller::Update, PruneMode::Never, true, false),
            (Caller::Update, PruneMode::Cron, false, false),
            (Caller::Update, PruneMode::Cron, true, true),
            (Caller::Update, PruneMode::Always, false, true),
            // Init convergence failure rolls back an installation; a prune
            // failure must never be able to cause that.
            (Caller::Init, PruneMode::Cron, true, false),
            (Caller::Init, PruneMode::Always, false, false),
            (Caller::Init, PruneMode::Always, true, false),
        ] {
            assert_eq!(
                prune_row(caller, mode, cron),
                expected,
                "{caller:?} {mode:?} cron={cron}"
            );
        }
    }

    #[test]
    fn prune_mode_parses_env_values_and_rejects_unknown_ones_softly() {
        for (value, expected) in [
            // Unset and empty default to cron prune; `never` opts out.
            (None, Some(PruneMode::Cron)),
            (Some(""), Some(PruneMode::Cron)),
            (Some("never"), Some(PruneMode::Never)),
            (Some("cron"), Some(PruneMode::Cron)),
            (Some("always"), Some(PruneMode::Always)),
            (Some("Cron"), None),
            (Some("weekly"), None),
        ] {
            assert_eq!(PruneMode::parse(value), expected, "{value:?}");
        }
    }

    #[test]
    fn prune_mode_applies_by_invocation_kind() {
        for (mode, cron, expected) in [
            (PruneMode::Never, false, false),
            (PruneMode::Never, true, false),
            (PruneMode::Cron, false, false),
            (PruneMode::Cron, true, true),
            (PruneMode::Always, false, true),
            (PruneMode::Always, true, true),
        ] {
            assert_eq!(mode.applies(cron), expected, "{mode:?} cron={cron}");
        }
    }

    #[test]
    fn live_sink_forwards_rows() {
        use std::io::Write as _;
        // Failure memory depends on the process-wide abort latch, which
        // in-process provider tests raise concurrently; it is pinned in the
        // latch-owning helper below instead of here.
        let mut inner = Vec::new();
        let mut sink = LiveSink {
            inner: &mut inner,
            failed: false,
        };
        sink.write_all(b"row\n").expect("forward rows");
        assert!(!sink.failed());
        assert_eq!(inner, b"row\n");
    }

    #[test]
    fn live_sink_does_not_remember_an_intentionally_aborted_write() {
        const HELPER: &str = "DOT_LIVE_SINK_ABORT_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "update_engine::tests::live_sink_does_not_remember_an_intentionally_aborted_write",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "live sink abort helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use std::io::Write as _;

        // The helper owns the process-wide outward-write-abort latch
        // exclusively: a parallel relay abort or resume would flip which
        // failure this test observes (loaded-host flake).
        let mut failing = FailingWriter;
        let mut sink = LiveSink {
            inner: &mut failing,
            failed: false,
        };
        // A supervisor that already owns the terminal status (e.g. a
        // provider's 128+signal exit) discards its queued rows; that must
        // not become the engine's exit-1 delivery failure.
        crate::cleanup::abort_outward_writes();
        assert!(sink.write_all(b"row\n").is_err());
        assert!(sink.flush().is_ok());
        assert!(!sink.failed());
        crate::cleanup::resume_outward_writes();
        // Once writes resume, an ordinary failure is remembered and later
        // becomes the engine's exit-1 delivery failure.
        assert!(sink.write_all(b"row\n").is_err());
        assert!(sink.failed());
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
            unknown_keys: Vec::new(),
        };
        let gathered = gather(
            Caller::Update,
            PruneMode::Never,
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

    /// Run one update whose pre-finalize reload reads `body`, starting
    /// from a boundary config that already reported `boundary` keys.
    /// `DOT_BASE_TOPOLOGY=missing` skips the base pull, so this exercises
    /// only the defensive reload before finalize; the post-base-pull
    /// reload runs end to end in `tests/cli.rs` (`update_pulling_*` and
    /// `update_warns_about_a_pulled_typo_*`). Returns the exit code and
    /// stderr.
    fn reload_case(label: &str, body: &[u8], caller: Caller, boundary: &[&str]) -> (i32, String) {
        let scratch = dot_test_support::TempDir::new(label).expect("create temporary directory");
        let home = scratch.path().join("home");
        let state = scratch.path().join("state");
        let config_home = scratch.path().join("config");
        std::fs::create_dir_all(config_home.join("dot")).expect("config directory");
        std::fs::create_dir_all(&state).expect("state directory");
        std::fs::write(config_home.join("dot/config"), body).expect("reload config");
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
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
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
            unknown_keys: boundary
                .iter()
                .map(|key| crate::config::UnknownKey {
                    key: key.to_string(),
                    line: 2,
                })
                .collect(),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_update(
            &runtime,
            &UpdateRequest {
                caller,
                config: &config,
                env: &env,
                args: &[],
                state_home: &state,
            },
            &mut crate::app::Streams::new(&mut stdout, &mut stderr),
        );
        (code, String::from_utf8_lossy(&stderr).into_owned())
    }

    #[test]
    fn reload_ignores_unknown_config_keys_silently() {
        // A key with no suggestion is most likely from a newer Dot that
        // this run's Tools stage may install: the reload must neither
        // fail the run (that stranded older Dot releases before Tools
        // could upgrade them) nor warn mid-run.
        let (code, stderr) = reload_case(
            "update-reload-unknown",
            b"version=1\nfuture_key=1\n",
            Caller::Update,
            &[],
        );
        assert!(!stderr.contains("dot: config:"), "{stderr}");
        assert_eq!(code, 0, "{stderr}");
    }

    #[test]
    fn reload_warns_once_about_a_new_typo_and_degrades_the_update() {
        // The rest of the run uses the reloaded default, so a near miss
        // the reload brings warns right away (once) and fails the update
        // as degraded instead of reporting a clean run.
        let (code, stderr) = reload_case(
            "update-reload-typo",
            b"version=1\ndefualt_profile=dev\nfuture_key=1\n",
            Caller::Update,
            &[],
        );
        assert_eq!(
            stderr.matches("dot: config:").collect::<Vec<_>>().len(),
            1,
            "{stderr}"
        );
        assert!(
            stderr.contains(
                "dot: config: warning: unknown key 'defualt_profile' ignored (did you mean 'default_profile'?)\n"
            ),
            "{stderr}"
        );
        // An interactive run also names the reason for its exit 1 at the end.
        assert!(
            stderr.ends_with(
                "  warning: update degraded: config key 'defualt_profile' is ignored; did you mean 'default_profile'?\n"
            ),
            "{stderr}"
        );
        assert!(!stderr.contains("future_key"), "{stderr}");
        assert_eq!(code, 1, "{stderr}");
    }

    #[test]
    fn reload_skips_typos_the_boundary_reported_but_still_degrades() {
        let (code, stderr) = reload_case(
            "update-reload-typo-reported",
            b"version=1\ndefualt_profile=dev\n",
            Caller::Update,
            &["defualt_profile"],
        );
        assert!(!stderr.contains("dot: config:"), "{stderr}");
        assert!(stderr.contains("update degraded"), "{stderr}");
        assert_eq!(code, 1, "{stderr}");
    }

    #[test]
    fn init_convergence_is_not_degraded_by_a_typo() {
        // Init's own reload already warned (`cli::init_config`); failing
        // its convergence would fail the installation over a typo.
        let (code, stderr) = reload_case(
            "update-reload-typo-init",
            b"version=1\ndefualt_profile=dev\n",
            Caller::Init,
            &["defualt_profile"],
        );
        assert!(!stderr.contains("dot: config:"), "{stderr}");
        assert_eq!(code, 0, "{stderr}");
    }

    #[test]
    fn config_degrades_only_updates_with_a_suggested_key() {
        let config = |keys: &[&str]| crate::config::Config {
            version: 1,
            extension_api: false,
            extensions_dir: None,
            provider: crate::config::Provider::None,
            default_profile: "base".to_string(),
            shdeps_update_policy: crate::config::UpdatePolicy::Pinned,
            policy_from_env: false,
            unknown_keys: keys
                .iter()
                .map(|key| crate::config::UnknownKey {
                    key: key.to_string(),
                    line: 2,
                })
                .collect(),
        };
        assert!(!config_degraded(Caller::Update, &config(&[])));
        assert!(!config_degraded(Caller::Update, &config(&["future_key"])));
        assert!(config_degraded(
            Caller::Update,
            &config(&["future_key", "dependency_provder"])
        ));
        assert!(!config_degraded(
            Caller::Init,
            &config(&["dependency_provder"])
        ));
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
            unknown_keys: Vec::new(),
        };
        let mut stdout = FailingWriter;
        let mut stderr = Vec::new();
        let code = run_update(
            &runtime,
            &UpdateRequest {
                caller: Caller::Update,
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
