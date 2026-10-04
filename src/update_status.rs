//! Cron observability state under `$XDG_STATE_HOME/dot`.
//!
//! Handoff findings #1 (cron dirty-skip observability) and #6 (quiet
//! runner status plus retained logs) share one state layout, owned
//! here so writers (the update engine, the quiet runner) and readers
//! (`dot doctor`) cannot drift:
//!
//! - `update.log`: one outcome line per cron run, appended:
//!   `<epoch> <outcome> <stage>[ <detail>]` with `outcome` one of
//!   `ok`, `degraded`, `fail`, or `skip`. `skip` lines carry the
//!   capped dirty file list as their detail; `ok`/`degraded`/`fail`
//!   lines name the `update` stage, and `degraded` lines add the
//!   failing stages ([`Degraded::detail`], e.g. `config,tools,prune`). A
//!   development checkout's provider re-exec performs two engine runs
//!   in one process, so one cron invocation can append two lines (both
//!   carry the continuation's classification). A packaged release hands
//!   off to its new binary instead ([`crate::handoff`]): only the exec'd
//!   continuation appends, or the handoff appends `fail` when it cannot
//!   exec.
//! - `update.last-success`: the epoch of the last fully clean cron
//!   run (exit 0), overwritten. Its meaning is unchanged from before
//!   the degraded outcome existed, so an older `dot doctor` reading a
//!   newer Dot's state still reports exactly what it always did.
//! - `update.last-converged`: `<epoch>[ <stages>]` for the last cron
//!   run whose repository sync, overlay links, profile deactivation,
//!   and config (merge) hooks succeeded and whose profile lifecycle
//!   commit did not fail (a failed Tools stage skips it, as before).
//!   Overwritten on `ok` (no stages) and `degraded` (the failing
//!   stages). `dot doctor` uses it to tell a host that keeps
//!   converging while Tools or Prune fails, or while the config
//!   carries a likely misspelled key, from one that stopped
//!   converging; stamps older than [`CRON_STALE_AFTER_SECS`] read as
//!   not converging. Older Dot releases neither write nor read it, so
//!   after a downgrade the stamp only ages out.
//! - `update.last-run`: `<epoch> <outcome> <trigger>[ <stages>]` for
//!   the last update run of any kind, overwritten: `outcome` is `ok`,
//!   `degraded`, `fail`, or (cron only) `skip`, classified exactly like
//!   the cron outcome line; `trigger` is `cron`, `manual`, or `init`;
//!   and `stages` follows a `degraded` outcome. Hosts updated by hand never write the
//!   cron stamps, so this is how `dot doctor` sees their last outcome
//!   (and notices a cron entry that never ran). Older Dot releases
//!   neither write nor read it.
//! - `update.last-failure`: why the last non-ok run (any trigger) did not
//!   succeed, overwritten by every such run and removed by a clean one.
//!   The first line repeats that run's `<epoch> <outcome> <trigger>` from
//!   `update.last-run`; doctor shows the cause only while the two match, so
//!   a record a later run (or an older Dot that does not write it) has
//!   superseded never explains the wrong run. Then one line per failing
//!   item, `item<TAB><stage><TAB><name><TAB><detail>` (a failed Shdeps
//!   item, a merge-hook key, a repository, a dirty file, or the stage's
//!   own reason), at most [`MAX_FAILURE_ITEMS_PER_STAGE`] per stage, and
//!   `more<TAB><stage><TAB><count>` for the rest. Fields are
//!   control-sanitized and capped, and the whole body stays within
//!   [`FAILURE_MAX_BYTES`]. A separate file rather than more fields in
//!   `update.last-run`, whose older readers reject anything but words.
//! - `logs/`: retained failure logs from the quiet runner, pruned to
//!   the newest [`MAX_RETAINED_LOGS`].
//!
//! Every writer is best-effort: observability must never fail an
//! update or a command, so filesystem errors are swallowed and
//! callers keep their historical exit codes. Non-cron updates write
//! only `update.last-run`: the cron log and stamps deliberately measure
//! cron-slot health rather than interactive use.

use std::path::{Path, PathBuf};

/// Age in seconds after which `dot doctor` warns that cron
/// convergence may be frozen (about two hours: four missed
/// half-hour slots).
pub const CRON_STALE_AFTER_SECS: i64 = 7200;

/// Dirty files named on one `skip` line before the `+N more` cap.
pub const MAX_SKIP_FILES: usize = 10;

/// Retained failure logs kept per directory (newest win).
pub const MAX_RETAINED_LOGS: usize = 20;

/// Largest `update.last-success` or `update.last-converged` body
/// accepted: a valid stamp is an ASCII epoch (at most 20 bytes), an
/// optional short stage list, and a newline, so anything past this is
/// corrupt, never a stamp.
const STAMP_MAX_BYTES: u64 = 64;

/// Outcome words persisted in the cron log and `update.last-run`. Stable
/// vocabulary: readers accept any lowercase word, so a newer outcome still
/// renders on an older Dot.
pub const OUTCOME_OK: &str = "ok";
/// See [`OUTCOME_OK`].
pub const OUTCOME_DEGRADED: &str = "degraded";
/// See [`OUTCOME_OK`].
pub const OUTCOME_FAIL: &str = "fail";
/// A cron run skipped because local edits were unresolved (the cron log
/// records it with the `dirty` stage and the file list).
pub const OUTCOME_SKIP: &str = "skip";

/// What started an update run, persisted in `update.last-run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `dot update --cron`.
    Cron,
    /// `dot update` or `dot pull` run by hand.
    Manual,
    /// `dot init` convergence.
    Init,
}

impl Trigger {
    /// The persisted word.
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Cron => "cron",
            Trigger::Manual => "manual",
            Trigger::Init => "init",
        }
    }
}

/// Stage names persisted in `degraded` outcome lines and the
/// convergence stamp. Stable vocabulary: `dot doctor` renders them
/// (older releases too: their reader accepts any lowercase name).
pub const STAGE_CONFIG: &str = "config";
/// See [`STAGE_CONFIG`].
pub const STAGE_TOOLS: &str = "tools";
/// See [`STAGE_CONFIG`].
pub const STAGE_PRUNE: &str = "prune";

/// Stage names that only `update.last-failure` items carry (the degraded
/// stages above appear there too). Lowercase words like every persisted
/// stage, so an older reader of a newer record never drops one: the Repos
/// stage (a repository that failed to pull, or an overlay that could not be
/// resolved), the Overlays stage (linking or profile deactivation), the
/// Configs stage (a merge hook), local edits that skipped a cron run, and
/// the update as a whole (anything outside one stage, such as a release
/// handoff that could not start the new binary), and the dependency
/// provider itself when it could not be prepared (the Tools stage then
/// degrades, but `shdeps` may not even be runnable, so doctor must not
/// send the user to it).
pub const STAGE_REPOS: &str = "repos";
/// See [`STAGE_REPOS`].
pub const STAGE_OVERLAYS: &str = "overlays";
/// See [`STAGE_REPOS`].
pub const STAGE_CONFIGS: &str = "configs";
/// See [`STAGE_REPOS`]. Matches the `skip dirty` cron log line.
pub const STAGE_DIRTY: &str = "dirty";
/// See [`STAGE_REPOS`].
pub const STAGE_UPDATE: &str = "update";
/// See [`STAGE_REPOS`].
pub const STAGE_PROVIDER: &str = "provider";

/// Items kept per stage in `update.last-failure`; the rest are counted.
pub const MAX_FAILURE_ITEMS_PER_STAGE: usize = 5;

/// Largest `update.last-failure` body written or read. A record carries a
/// handful of short items, so this bounds both the write and every doctor
/// read to one small block.
pub const FAILURE_MAX_BYTES: usize = 4096;

/// Longest item name kept (bytes, cut on a character boundary).
const FAILURE_NAME_MAX_BYTES: usize = 120;

/// Longest item detail kept (bytes, cut on a character boundary).
const FAILURE_DETAIL_MAX_BYTES: usize = 240;

/// Stages whose failure leaves a cron run converged but degraded:
/// the dotfiles themselves (repositories, links, configs) are current,
/// but the client config misspells a known key (Config), dependency
/// convergence (Tools) failed, or orphan removal (Prune) failed.
/// Any other failure means the run did not converge and records `fail`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Degraded {
    /// The run's final config holds an unknown key that is a near miss
    /// of a known one ([`crate::config::UnknownKey::suggestion`]), so the
    /// setting it most likely meant kept its default for the whole run.
    pub config: bool,
    /// The Tools stage failed (provider unavailable, a dependency, or a
    /// post hook).
    pub tools: bool,
    /// The Prune stage ran and failed.
    pub prune: bool,
}

impl Degraded {
    /// True when no degradable stage failed.
    pub fn is_empty(self) -> bool {
        !self.config && !self.tools && !self.prune
    }

    /// The failing stages, comma-joined in run order
    /// (`config,tools,prune`); empty when none failed. Persisted as-is.
    pub fn detail(self) -> String {
        [
            (self.config, STAGE_CONFIG),
            (self.tools, STAGE_TOOLS),
            (self.prune, STAGE_PRUNE),
        ]
        .iter()
        .filter(|(failed, _)| *failed)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(",")
    }
}

/// The last converged cron run read back from `update.last-converged`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converged {
    /// Epoch seconds of that run.
    pub at: i64,
    /// Its failing stages (`tools,prune`), empty for a clean run. Kept as
    /// validated text rather than [`Degraded`] so a stage name only a newer
    /// Dot writes still renders instead of hiding the degraded state.
    pub failing: String,
}

/// The last update run read back from `update.last-run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    /// Epoch seconds of that run.
    pub at: i64,
    /// Its outcome word ([`OUTCOME_OK`], [`OUTCOME_DEGRADED`],
    /// [`OUTCOME_FAIL`], or a word only a newer Dot writes).
    pub outcome: String,
    /// Its trigger word ([`Trigger::as_str`], or a newer word).
    pub trigger: String,
    /// The failing stages of a degraded run, empty otherwise.
    pub failing: String,
}

impl LastRun {
    /// Whether the run was triggered by cron.
    pub fn is_cron(&self) -> bool {
        self.trigger == Trigger::Cron.as_str()
    }
}

/// The failing items one update run collects for `update.last-failure`, in
/// the order the run met them. Every field is cleaned on [`Failures::push`],
/// so whatever a provider, hook, or filename carries never reaches the
/// record or doctor's terminal as control text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Failures {
    items: Vec<FailureItem>,
}

/// One failing item: its stage, what failed (a package, a hook key, a
/// repository, a file), and why, when known (empty otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureItem {
    /// Stage word ([`STAGE_TOOLS`], [`STAGE_REPOS`], ..., or a word only a
    /// newer Dot writes).
    pub stage: String,
    /// What failed.
    pub name: String,
    /// Why it failed, empty when unknown.
    pub detail: String,
}

impl Failures {
    /// Record one failing item. Names and details are cleaned and capped
    /// here; an item with neither is dropped (it would explain nothing).
    pub fn push(&mut self, stage: &'static str, name: &str, detail: &str) {
        let name = clean_field(name, FAILURE_NAME_MAX_BYTES);
        let detail = clean_field(detail, FAILURE_DETAIL_MAX_BYTES);
        if name.is_empty() && detail.is_empty() {
            return;
        }
        self.items.push(FailureItem {
            stage: stage.to_string(),
            name,
            detail,
        });
    }

    /// Whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether `stage` has at least one item.
    pub fn has_stage(&self, stage: &str) -> bool {
        self.items.iter().any(|item| item.stage == stage)
    }

    /// The recorded items in run order.
    pub fn items(&self) -> &[FailureItem] {
        &self.items
    }
}

/// The last non-ok update run's cause, read back from `update.last-failure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastFailure {
    /// Epoch seconds of that run: the same value its `update.last-run` stamp
    /// carries, which is how doctor ties the cause to the run it reports.
    pub at: i64,
    /// Its outcome word (never [`OUTCOME_OK`]).
    pub outcome: String,
    /// Its trigger word.
    pub trigger: String,
    /// The kept items in run order.
    pub items: Vec<FailureItem>,
    /// Items the writer counted but did not keep.
    pub omitted: usize,
}

impl LastFailure {
    /// Whether this record describes `run`: same epoch, outcome, and
    /// trigger. A stamp written by a Dot that does not write this record
    /// never matches an older record, so stale causes never attach to it.
    pub fn describes(&self, run: &LastRun) -> bool {
        self.at == run.at && self.outcome == run.outcome && self.trigger == run.trigger
    }
}

/// Make untrusted text safe for a one-line record field: color and other
/// CSI escape sequences (`ESC [ ... final`, or C1 CSI) are dropped whole,
/// every other control character (C0, DEL, and C1, so tabs and newlines
/// too) becomes a space, runs of spaces collapse, and the result is cut to
/// `max` bytes on a character boundary with a trailing `…` when cut.
/// Doctor uses it too, to shorten a kept detail for its one-line rows.
pub fn clean_field(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // Past the cap plus room for the ellipsis cut, the rest is dropped
        // anyway; stop scanning so huge input costs only `max`.
        if out.len() > max + 4 {
            break;
        }
        let csi = c == '\u{9b}' || (c == '\u{1b}' && chars.peek() == Some(&'['));
        if csi {
            if c == '\u{1b}' {
                chars.next();
            }
            // Parameters and intermediates, up to the final byte.
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        let c = if c.is_control() { ' ' } else { c };
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_end();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let ellipsis = '…';
    // A cap too small for the ellipsis keeps nothing rather than underflow.
    let Some(mut cut) = max.checked_sub(ellipsis.len_utf8()) else {
        return String::new();
    };
    while !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut capped = trimmed[..cut].trim_end().to_string();
    capped.push(ellipsis);
    capped
}

/// The last non-empty line of untrusted output, or `""`. A failing command
/// (Shdeps, a merge hook) prints its fatal error last, after any warnings,
/// so this is the line most likely to name the cause. Only the trailing
/// [`FAILURE_MAX_BYTES`] are scanned, which bounds the work on huge output.
pub fn last_line(output: &[u8]) -> String {
    let tail = &output[output.len().saturating_sub(FAILURE_MAX_BYTES)..];
    let line = tail
        .split(|byte| *byte == b'\n' || *byte == b'\r')
        .map(<[u8]>::trim_ascii)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    clean_field(&String::from_utf8_lossy(line), FAILURE_DETAIL_MAX_BYTES)
}

/// Create `path` as a private directory, tightening pre-existing
/// ones: cron state holds dirty filenames plus absolute home paths,
/// so it stays owner-only whatever the ambient umask was.
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Open a cron state file without ever blocking on a pre-planted
/// FIFO: `O_NONBLOCK` keeps the open/read non-blocking, and the
/// opened fd must stat to a regular file (checked post-open, so no
/// TOCTOU between the check and the read).
fn open_state_file(options: &std::fs::OpenOptions, path: &Path) -> Option<std::fs::File> {
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    Some(file)
}

/// `$XDG_STATE_HOME/dot`, the directory owning this module's files.
pub fn dot_dir(state_home: &Path) -> PathBuf {
    state_home.join("dot")
}

/// The cron outcome log (`dot/update.log`).
pub fn update_log_path(state_home: &Path) -> PathBuf {
    dot_dir(state_home).join("update.log")
}

/// The last-success stamp (`dot/update.last-success`).
pub fn last_success_path(state_home: &Path) -> PathBuf {
    dot_dir(state_home).join("update.last-success")
}

/// The convergence stamp (`dot/update.last-converged`).
pub fn last_converged_path(state_home: &Path) -> PathBuf {
    dot_dir(state_home).join("update.last-converged")
}

/// The any-trigger last-run stamp (`dot/update.last-run`).
pub fn last_run_path(state_home: &Path) -> PathBuf {
    dot_dir(state_home).join("update.last-run")
}

/// The last non-ok run's cause (`dot/update.last-failure`).
pub fn last_failure_path(state_home: &Path) -> PathBuf {
    dot_dir(state_home).join("update.last-failure")
}

/// The retained failure-log directory (`dot/logs`).
pub fn logs_dir(state_home: &Path) -> PathBuf {
    dot_dir(state_home).join("logs")
}

/// Cap a dirty file list for one `skip` line: the first
/// [`MAX_SKIP_FILES`] entries joined with spaces, plus `+N more`
/// when entries remain. Empty input reads `unknown`. Control bytes
/// in filenames sanitize to spaces so one hostile name cannot forge
/// outcome lines or inject terminal escapes into the cron warning.
pub fn format_skip_detail(files: &[String]) -> String {
    if files.is_empty() {
        return "unknown".to_string();
    }
    let mut detail = files
        .iter()
        .take(MAX_SKIP_FILES)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if files.len() > MAX_SKIP_FILES {
        detail.push_str(&format!(" +{} more", files.len() - MAX_SKIP_FILES));
    }
    // The sanitizer maps ASCII controls to spaces, which preserves
    // UTF-8 validity by construction (multi-byte sequences pass
    // through untouched).
    String::from_utf8(crate::progress_ui::sanitize_untrusted_text(
        detail.as_bytes(),
    ))
    .expect("control-sanitized text stays UTF-8")
}

/// Append one cron outcome line (`<epoch> <outcome> <stage>` plus an
/// optional detail). Creates the state directory on demand; every
/// error is swallowed (observability never fails the run). The log
/// stays owner-only, and a pre-planted FIFO is refused rather than
/// blocking the run.
pub fn append_outcome(state_home: &Path, now: i64, outcome: &str, stage: &str, detail: &str) {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let path = update_log_path(state_home);
    if let Some(parent) = path.parent() {
        let _ = ensure_private_dir(parent);
    }
    let mut line = format!("{now} {outcome} {stage}");
    if !detail.is_empty() {
        line.push(' ');
        line.push_str(detail);
    }
    line.push('\n');
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK);
    let Some(mut file) = open_state_file(&options, &path) else {
        return;
    };
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    let _ = file.write_all(line.as_bytes());
}

/// Overwrite the last-success stamp with `now` (epoch seconds).
/// Best-effort like [`append_outcome`], owner-only, FIFO-refusing.
pub fn record_success(state_home: &Path, now: i64) {
    write_stamp(&last_success_path(state_home), &format!("{now}\n"));
}

/// Overwrite the convergence stamp with `now` and the failing stages
/// (none for a clean run). Best-effort like [`record_success`].
pub fn record_converged(state_home: &Path, now: i64, degraded: Degraded) {
    let mut body = now.to_string();
    if !degraded.is_empty() {
        body.push(' ');
        body.push_str(&degraded.detail());
    }
    body.push('\n');
    write_stamp(&last_converged_path(state_home), &body);
}

/// Overwrite the last-run stamp. `degraded` is persisted only with the
/// [`OUTCOME_DEGRADED`] outcome. Best-effort like [`record_success`].
pub fn record_last_run(
    state_home: &Path,
    now: i64,
    outcome: &str,
    trigger: Trigger,
    degraded: Degraded,
) {
    let mut body = format!("{now} {outcome} {}", trigger.as_str());
    if outcome == OUTCOME_DEGRADED && !degraded.is_empty() {
        body.push(' ');
        body.push_str(&degraded.detail());
    }
    body.push('\n');
    write_stamp(&last_run_path(state_home), &body);
}

/// Overwrite the last-failure record for a non-ok run: a header matching
/// that run's `update.last-run` stamp, then up to
/// [`MAX_FAILURE_ITEMS_PER_STAGE`] `item` lines per stage and one `more`
/// line counting the rest, all within [`FAILURE_MAX_BYTES`]. A run with no
/// items still writes its header, so doctor can tell "cause unknown" from
/// a cause left behind by an earlier run. Best-effort like
/// [`record_success`].
pub fn record_last_failure(
    state_home: &Path,
    now: i64,
    outcome: &str,
    trigger: Trigger,
    failures: &Failures,
) {
    /// Per-stage counter in first-seen order (a handful of stages at most).
    fn count(counts: &[(&str, usize)], stage: &str) -> usize {
        counts
            .iter()
            .find(|(name, _)| *name == stage)
            .map_or(0, |e| e.1)
    }
    fn bump<'a>(counts: &mut Vec<(&'a str, usize)>, stage: &'a str) {
        match counts.iter_mut().find(|(name, _)| *name == stage) {
            Some(entry) => entry.1 += 1,
            None => counts.push((stage, 1)),
        }
    }

    let mut body = format!("{now} {outcome} {}\n", trigger.as_str());
    let mut kept: Vec<(&str, usize)> = Vec::new();
    let mut omitted: Vec<(&str, usize)> = Vec::new();
    // Any stage may end up with a `more` line, so reserve the longest one
    // each distinct stage could need (its count is at most the item count)
    // before keeping a single item: the counts then always fit the cap.
    let mut stages: Vec<&str> = Vec::new();
    for item in &failures.items {
        if !stages.contains(&item.stage.as_str()) {
            stages.push(&item.stage);
        }
    }
    let digits = failures.items.len().to_string().len();
    let reserve: usize = stages
        .iter()
        .map(|stage| "more\t\t\n".len() + stage.len() + digits)
        .sum();
    for item in &failures.items {
        let stage = item.stage.as_str();
        let line = format!("item\t{stage}\t{}\t{}\n", item.name, item.detail);
        let fits = body.len() + line.len() + reserve <= FAILURE_MAX_BYTES;
        if fits && count(&kept, stage) < MAX_FAILURE_ITEMS_PER_STAGE {
            body.push_str(&line);
            bump(&mut kept, stage);
        } else {
            bump(&mut omitted, stage);
        }
    }
    for (stage, count) in omitted {
        body.push_str(&format!("more\t{stage}\t{count}\n"));
    }
    write_stamp(&last_failure_path(state_home), &body);
}

/// Remove the last-failure record after a clean run: the cause no longer
/// describes anything, and it may hold a line of hook or provider output
/// that should not outlive the problem. Best-effort, and it only ever
/// unlinks: a directory planted at the path stays.
pub fn clear_last_failure(state_home: &Path) {
    let path = last_failure_path(state_home);
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_dir()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Record a run that failed outside the engine (a release handoff that
/// could not exec, or a continuation that could not start): the `fail` cron
/// line for a cron run, plus `update.last-run` and `update.last-failure` for
/// any trigger, so every writer of a failure leaves the same three records
/// the engine does. `stage`/`name`/`detail` form the one known cause.
pub fn record_failed_run(
    state_home: &Path,
    now: i64,
    trigger: Trigger,
    cause: (&'static str, &str, &str),
) {
    if trigger == Trigger::Cron {
        append_outcome(state_home, now, OUTCOME_FAIL, "update", "");
    }
    record_last_run(state_home, now, OUTCOME_FAIL, trigger, Degraded::default());
    let mut failures = Failures::default();
    failures.push(cause.0, cause.1, cause.2);
    record_last_failure(state_home, now, OUTCOME_FAIL, trigger, &failures);
}

/// Truncate-and-write one small stamp file. Not an atomic rename: the
/// stamps are advisory, written under the update lock, and every reader
/// treats a torn or empty body as missing, which only ever errs toward
/// the older (warning) answer for one doctor run.
fn write_stamp(path: &Path, body: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    if let Some(parent) = path.parent() {
        let _ = ensure_private_dir(parent);
    }
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK);
    let Some(mut file) = open_state_file(&options, path) else {
        return;
    };
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    let _ = file.write_all(body.as_bytes());
}

/// Read one stamp body capped at [`STAMP_MAX_BYTES`], or `None` when
/// missing, unreadable, oversized, or not UTF-8. Never blocks on a
/// pre-planted FIFO.
fn read_stamp(path: &Path) -> Option<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NONBLOCK);
    let file = open_state_file(&options, path)?;
    let mut content = String::new();
    file.take(STAMP_MAX_BYTES + 1)
        .read_to_string(&mut content)
        .ok()?;
    if content.len() as u64 > STAMP_MAX_BYTES {
        return None;
    }
    Some(content)
}

/// Read the last-success stamp: the trimmed file content as epoch
/// seconds, or `None` when missing, unreadable, oversized, or
/// malformed. Never blocks on a pre-planted FIFO.
pub fn read_last_success(state_home: &Path) -> Option<i64> {
    read_stamp(&last_success_path(state_home))?
        .trim()
        .parse::<i64>()
        .ok()
}

/// Read the convergence stamp, or `None` when missing, unreadable,
/// oversized, or malformed. The stage list must be comma-separated
/// lowercase ASCII names: anything else (spaces, control bytes, empty
/// names) is corrupt, so hostile text never reaches doctor output.
pub fn read_last_converged(state_home: &Path) -> Option<Converged> {
    let content = read_stamp(&last_converged_path(state_home))?;
    let line = content.strip_suffix('\n').unwrap_or(&content);
    let (epoch, failing) = match line.split_once(' ') {
        Some((epoch, failing)) => (epoch, failing),
        None => (line, ""),
    };
    let at = epoch.parse::<i64>().ok()?;
    if line.contains(' ') {
        let valid = failing
            .split(',')
            .all(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_lowercase()));
        if !valid {
            return None;
        }
    }
    Some(Converged {
        at,
        failing: failing.to_string(),
    })
}

/// Whether `word` is a non-empty lowercase ASCII word, the shape every
/// persisted outcome, trigger, and stage name takes.
fn is_word(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// Read the last-run stamp, or `None` when missing, unreadable,
/// oversized, or malformed. Outcome and trigger must be lowercase ASCII
/// words and the optional stage list comma-separated words, so hostile
/// text never reaches doctor output while newer vocabulary still reads.
pub fn read_last_run(state_home: &Path) -> Option<LastRun> {
    let content = read_stamp(&last_run_path(state_home))?;
    let line = content.strip_suffix('\n').unwrap_or(&content);
    let mut fields = line.split(' ');
    let at = fields.next()?.parse::<i64>().ok()?;
    let outcome = fields.next().filter(|word| is_word(word))?;
    let trigger = fields.next().filter(|word| is_word(word))?;
    let failing = match fields.next() {
        Some(stages) if stages.split(',').all(is_word) => stages,
        Some(_) => return None,
        None => "",
    };
    if fields.next().is_some() {
        return None;
    }
    Some(LastRun {
        at,
        outcome: outcome.to_string(),
        trigger: trigger.to_string(),
        failing: failing.to_string(),
    })
}

/// Read the last-failure record, or `None` when missing, unreadable, or its
/// header is malformed. Only the first [`FAILURE_MAX_BYTES`] are read, up to
/// the last whole line, so a larger record from a newer Dot still yields its
/// leading items. Lines of an unknown kind are skipped (newer vocabulary),
/// and every field is cleaned again, so hostile text never reaches doctor
/// output even if something else wrote the file.
pub fn read_last_failure(state_home: &Path) -> Option<LastFailure> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NONBLOCK);
    let file = open_state_file(&options, &last_failure_path(state_home))?;
    let mut bytes = Vec::new();
    file.take(FAILURE_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > FAILURE_MAX_BYTES {
        // Drop the cut line: a partial item must not read as a whole one.
        let end = bytes[..FAILURE_MAX_BYTES]
            .iter()
            .rposition(|byte| *byte == b'\n')?;
        bytes.truncate(end + 1);
    }
    let content = String::from_utf8_lossy(&bytes);
    let mut lines = content.lines();
    let mut header = lines.next()?.split(' ');
    let at = header.next()?.parse::<i64>().ok()?;
    let outcome = header.next().filter(|word| is_word(word))?;
    // Further header fields from a newer Dot are ignored: these three are
    // all `describes` needs.
    let trigger = header.next().filter(|word| is_word(word))?;
    let mut items = Vec::new();
    let mut omitted = 0usize;
    for line in lines {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next()) {
            (Some("item"), Some(stage)) if is_word(stage) => {
                let name = clean_field(fields.next().unwrap_or(""), FAILURE_NAME_MAX_BYTES);
                let detail = clean_field(fields.next().unwrap_or(""), FAILURE_DETAIL_MAX_BYTES);
                if !name.is_empty() || !detail.is_empty() {
                    items.push(FailureItem {
                        stage: stage.to_string(),
                        name,
                        detail,
                    });
                }
            }
            (Some("more"), Some(stage)) if is_word(stage) => {
                let count = fields.next().and_then(|n| n.parse::<usize>().ok());
                omitted = omitted.saturating_add(count.unwrap_or(0));
            }
            _ => {}
        }
    }
    Some(LastFailure {
        at,
        outcome: outcome.to_string(),
        trigger: trigger.to_string(),
        items,
        omitted,
    })
}

/// True when `last_success` is older than
/// [`CRON_STALE_AFTER_SECS`] at `now`. Future stamps (clock skew)
/// read fresh; a hostile `i64::MIN` stamp saturates to stale instead
/// of overflowing.
pub fn is_stale(last_success: i64, now: i64) -> bool {
    now.saturating_sub(last_success) > CRON_STALE_AFTER_SECS
}

/// Render a non-negative age in seconds for doctor detail (`30s`,
/// `45m`, `2h5m`). Negative ages (clock skew) read `0s`.
pub fn format_age(age_secs: i64) -> String {
    let age = age_secs.max(0);
    if age < 60 {
        return format!("{age}s");
    }
    if age < 3600 {
        return format!("{}m", age / 60);
    }
    format!("{}h{}m", age / 3600, (age % 3600) / 60)
}

/// Keep `label` filename-safe for retained logs: ASCII alphanumerics
/// plus `-`, `_`, and `.` pass through, everything else becomes
/// `_`. Only the empty label reads `command` (a label of nothing
/// safe still maps to underscores, which is itself filename-safe).
pub fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "command".to_string()
    } else {
        cleaned
    }
}

/// Retain a failed scratch log under `logs_dir` as
/// `<label>-<epoch>-<pid>.log`, pruning to the newest
/// [`MAX_RETAINED_LOGS`]. Moves with rename, falling back to
/// copy-plus-remove across filesystems; returns the retained path,
/// or `None` when retention failed (the caller then removes the
/// scratch log itself). Best-effort throughout.
pub fn retain_failed_log(
    logs_dir: &Path,
    label: &str,
    now: i64,
    scratch: &Path,
) -> Option<PathBuf> {
    if ensure_private_dir(logs_dir).is_err() {
        return None;
    }
    let retained = logs_dir.join(format!(
        "{}-{now}-{}.log",
        sanitize_label(label),
        std::process::id()
    ));
    if std::fs::rename(scratch, &retained).is_err() {
        if std::fs::copy(scratch, &retained).is_err() {
            // A failed copy must not leave a partial entry behind to
            // consume one of the retained slots.
            let _ = std::fs::remove_file(&retained);
            return None;
        }
        let _ = std::fs::remove_file(scratch);
    }
    prune_logs(logs_dir);
    Some(retained)
}

/// Prune `logs_dir` to the newest [`MAX_RETAINED_LOGS`] `*.log`
/// files by (mtime, name), removing the oldest first. Best-effort:
/// unreadable directories and removal failures are ignored.
pub fn prune_logs(logs_dir: &Path) {
    let entries = match std::fs::read_dir(logs_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut logs: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".log") {
            continue;
        }
        let mtime = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        logs.push((mtime, name, path));
    }
    logs.sort();
    if logs.len() > MAX_RETAINED_LOGS {
        for (_, _, path) in logs.iter().take(logs.len() - MAX_RETAINED_LOGS) {
            let _ = std::fs::remove_file(path);
        }
    }
}
