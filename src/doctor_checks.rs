//! Built-in doctor checks.
//!
//! Owns the health checks formerly grouped in `lib/dot/doctor/lock.sh`,
//! `lib/dot/doctor/merges.sh`, `lib/dot/doctor/overlays.sh`,
//! `lib/dot/doctor/provider.sh`, and `lib/dot/doctor/repos.sh`:
//! [`check_update_lock`], [`check_merges`],
//! [`check_profile_lifecycle`], [`check_overlays`],
//! [`shdeps_binary`], [`check_provider`],
//! [`completed_identity_matches_home`], [`is_client_checkout`],
//! [`check_base_repo`], and [`check_cron_freshness`].
//! [`crate::doctor`] owns orchestration.
//!
//! Everything here is a pure function of explicit inputs, the
//! established boundary: shell-era globals (`DOT_*`,
//! `ACTIVE_OVERLAYS`, lifecycle arrays) arrive as parameters, and
//! helper boundaries owned by other modules arrive either as data or
//! as small predicates documented per function. Filesystem and `git`
//! probes the check itself performs (`-e`/`-d`/`-L` tests,
//! `readlink`, `rev-parse`, manifest reads) run in-process so the
//! differential tests observe both engines on the same fixtures.
//!
//! Reused sibling modules (not reimplemented):
//!
//! - [`crate::update_lock`] backs [`check_update_lock`] (owner
//!   read, liveness, initializing window).
//! - [`crate::overlays`] backs [`check_overlays`] (`is_worktree`,
//!   `effective_url`, `origin_matches`).
//! - [`crate::repos_overlays`] backs [`check_overlays`]
//!   (`parse_manifest_record`, `record_link_target`, `stream_lines`).
//! - [`crate::repos_base`] backs [`check_base_repo`] (`git_prefix`
//!   shape, `run_git` spawn boundary).
//!
//! Parity decisions:
//!
//! - Checks emit the canonical [`Record`]s instead of calling the `_dr_*`
//!   emitters; [`render`] delegates to the runtime renderer with color
//!   disabled so parity tests can byte-compare against the live shell.
//! - `_dr_tilde` / `_dr_symlink_points_to` (`doctor/paths.sh`) come
//!   from [`crate::doctor_paths`], the one owner of doctor path display
//!   and resolution.
//! - `local_validate` (`_overlay_local_source_validate`,
//!   `find`-walk plus per-entry checks), the profile deactivation
//!   probe, the shdeps installer selection, and the lifecycle ledger
//!   load stay caller concerns: they encode trust policy owned by
//!   other modules, so tests inject their outcomes.
//! - The `_dr_check_merges` "inventory is invalid" branch fires with
//!   `spec_count: None`; [`MergeInputs::inventory_error`] carries the
//!   reason (an unsafe hook, a bad identity) so the row names it.
//! - [`check_cron_freshness`] and per-hook output verification are
//!   new observability (handoff findings #1 and #8), not shell
//!   ports: the shell counted merge specs only and kept no
//!   success stamp.
//! - `read` field splitting (`IFS='|'`, `-r`, last variable keeps
//!   the remainder, missing fields read empty) is mirrored by the
//!   private `read_fields` helper.
//! - Shell `$(...)` trailing-newline stripping is mirrored by
//!   trimming trailing `\n` from captured `git` output.
//! - `set -u` arrays (`CONFIGURED_OVERLAY_NAMES`, `INCLUDED_PROFILES`,
//!   ...) must exist shell-side; unset and empty both arrive as
//!   empty vectors.
//! - No `let`-chains: MSRV is Rust 1.85.
//! - No `Command::envs`: children receive single `env`/`env_remove`
//!   entries.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::doctor_paths::tilde;
pub use crate::doctor_runtime::{Kind, Record};

/// How long one core doctor probe may run.
///
/// A stalled Git or filesystem (a hung network mount under an overlay, say)
/// used to stall doctor indefinitely. Past this deadline the probe's
/// session is stopped and reaped like any other timeout, and the repository
/// rows it feeds read as a warning naming the stall. The bound sits well
/// above a slow but healthy host (a cold `git status` of a large work tree
/// on a network home), so only a real stall reaches it. The overlay
/// resolver's probes (shared with `dot update`, whose answers feed trust
/// decisions) stay unbounded: a timeout there would read as a missing
/// checkout and refuse that overlay's extensions. `dot doctor --help`
/// states this bound ([`crate::doctor::USAGE`]).
pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The deadline for a core probe starting now.
pub(crate) fn probe_deadline() -> Option<std::time::Instant> {
    std::time::Instant::now().checked_add(probe_timeout())
}

fn probe_timeout() -> std::time::Duration {
    #[cfg(test)]
    if let Some(timeout) = TEST_PROBE_TIMEOUT.with(std::cell::Cell::get) {
        return timeout;
    }
    PROBE_TIMEOUT
}

#[cfg(test)]
thread_local! {
    /// A shorter [`PROBE_TIMEOUT`] for this test thread.
    static TEST_PROBE_TIMEOUT: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// A core probe stopped at its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimedOut;

/// The detail of every row a timed-out probe turns into a warning.
pub(crate) fn timed_out_detail() -> String {
    format!(
        "Git did not answer within {}s; check for a stalled filesystem or Git process, then rerun dot doctor",
        PROBE_TIMEOUT.as_secs()
    )
}

/// Render records with color disabled (piped stdout: every color slot is
/// empty), through [`crate::doctor_runtime::render`]:
///
/// - section: `\n{message}\n`
/// - ok/skip/info: `  ✓/·/› {message}[ ({detail})]\n`
/// - warn/fail: `  ⚠/✗ {message}[\n    {detail}]\n`
/// - then any attachments: `    - {item}\n` (folding into `    +N more\n`
///   past the limit) and `    → {hint}\n`
pub fn render(records: &[Record]) -> String {
    String::from_utf8(crate::doctor_runtime::render(
        records,
        &crate::doctor_runtime::Palette::empty(),
    ))
    .expect("doctor checks emit UTF-8 text")
}

/// Shell `IFS='|' read -r` into `count` variables: split on `|`
/// (no trimming, `-r` keeps backslashes), the last variable keeps
/// the unsplit remainder, missing fields read empty. Single-line
/// records only (callers never pass embedded newlines, like the
/// shell herestrings the checks read from).
fn read_fields(record: &str, count: usize) -> Vec<String> {
    let mut fields: Vec<String> = record.split('|').map(str::to_string).collect();
    if fields.len() > count && count > 0 {
        let rest = fields[count - 1..].join("|");
        fields.truncate(count - 1);
        fields.push(rest);
    }
    while fields.len() < count {
        fields.push(String::new());
    }
    fields
}

/// The record name: `${record%%|*}`, the text before the first
/// `|` (the whole record when there is none).
fn record_name(record: &str) -> &str {
    match record.find('|') {
        Some(index) => &record[..index],
        None => record,
    }
}

/// Shell `$(...)` capture: strip every trailing newline.
fn captured(output: &str) -> String {
    output.trim_end_matches('\n').to_string()
}

/// Owner-execute-or-any-execute probe mirroring `[[ -x $path ]]`
/// for the cases the checks meet: any execute bit set. (Full
/// `access(2)` semantics for foreign-owned files are not modeled;
/// fixtures run as the file owner, where owner-bit and `-x`
/// agree, and root's any-bit rule matches this exactly.)
fn is_executable_bits(mode: u32) -> bool {
    mode & 0o111 != 0
}

/// One owner row for [`check_update_lock`]: the row must agree with
/// what `acquire` will do. `acquire` reclaims stale owners but
/// *refuses* when the owner probe cannot verify liveness, so `Unknown`
/// fails (every mutating command refuses until the probe succeeds) instead
/// of sharing the stale "will reclaim" promise. A live owner stays a
/// warning (the refusal ends when that update does), as does a probe
/// interrupted by a signal to doctor itself.
///
/// `age` is how long the owner has held the lock (seconds since its owner
/// record was written), when known. A live owner past the cron staleness
/// window is most likely hung rather than slow, so its row says so.
fn lock_owner_record(
    owner: &crate::update_lock::Owner,
    activity: crate::update_lock::OwnerActivity,
    age: Option<i64>,
) -> Record {
    use crate::update_lock::OwnerActivity as Activity;
    use crate::update_status::{CRON_STALE_AFTER_SECS, format_age};

    match activity {
        Activity::Active => match age {
            Some(age) if age > CRON_STALE_AFTER_SECS => Record::warn(
                format!("update has been running for {}", format_age(age)),
                Some(format!("pid {}", owner.pid)),
            )
            .with_hint(format!(
                "if it is hung, stop it (kill {pid}) and rerun dot update",
                pid = owner.pid
            )),
            Some(age) => Record::warn(
                "update is currently running",
                Some(format!(
                    "pid {}, running for {}",
                    owner.pid,
                    format_age(age)
                )),
            )
            .with_hint("wait for it to finish"),
            None => Record::warn(
                "update is currently running",
                Some(format!("pid {}", owner.pid)),
            )
            .with_hint("wait for it to finish"),
        },
        Activity::Stale => Record::warn(
            "update lock owner is stale",
            Some(format!("pid {} no longer holds it", owner.pid)),
        )
        .with_hint("run dot update; it reclaims the stale lock"),
        Activity::Unknown => Record::fail(
            "update lock owner cannot be verified",
            Some(format!(
                "pid {}; mutating commands refuse until the owner probe succeeds",
                owner.pid
            )),
        )
        .with_hint(format!(
            "check that ps -o lstart= -p {} prints a start time, then rerun dot doctor",
            owner.pid
        )),
        // Only this doctor run was interrupted mid-probe; nothing about the
        // lock is known to be wrong.
        Activity::Interrupted => Record::warn(
            "update lock owner cannot be verified",
            Some("the owner probe was interrupted".to_string()),
        )
        .with_hint("rerun dot doctor"),
    }
}

/// `_dr_check_update_lock` (`doctor/lock.sh`): the process-wide
/// mutation lock is either clear, held live, stale, initializing,
/// or incomplete.
///
/// `lock_dir` is the `_dot_update_lock_path` result (`None` when
/// path resolution fails). Owner liveness reuses
/// [`crate::update_lock`]: `read_owner` for
/// `_dot_update_lock_read_owner`, `owner_activity` for owner
/// classification (active, stale, or unverifiable), and
/// `is_initializing` for `_dot_update_lock_is_initializing`. The
/// `-e`/`-L`/`-d` probes read through `symlink_metadata` exactly
/// like the shell conditionals (a symlink to a directory is unsafe,
/// not clear).
pub fn check_update_lock(lock_dir: Option<&Path>) -> Vec<Record> {
    let mut out = vec![Record::section("Update lock")];
    let Some(dir) = lock_dir else {
        // The lock lives under the state directory, which comes from HOME
        // or XDG_STATE_HOME.
        out.push(
            Record::fail("update lock path cannot be resolved", None).with_hint(
                "set HOME (or XDG_STATE_HOME) to a usable directory, then rerun dot doctor",
            ),
        );
        return out;
    };
    let present = std::fs::symlink_metadata(dir).is_ok();
    if !present {
        out.push(Record::ok("update lock is clear", None));
        return out;
    }
    let meta = std::fs::symlink_metadata(dir).ok();
    let is_dir = meta.as_ref().is_some_and(|meta| meta.is_dir());
    let is_link = meta.as_ref().is_some_and(|meta| meta.is_symlink());
    // `[[ ! -d $lock_dir || -L $lock_dir ]]`: `-d` follows links,
    // so a symlink to a directory still fails the second arm.
    if !is_dir || is_link {
        out.push(
            Record::fail(
                "update lock path is unsafe",
                Some(dir.to_string_lossy().into_owned()),
            )
            .with_hint("it must be a real directory: remove it, then rerun dot update"),
        );
        return out;
    }
    if let Some(owner) = crate::update_lock::read_owner(dir) {
        // The owner record is written once at acquisition (a continuation
        // re-enters without rewriting it), so its mtime is when the run
        // took the lock. One stat; clock skew reads as just started.
        let age = std::fs::metadata(crate::update_lock::owner_file(dir))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|at| crate::update_engine::now_secs().saturating_sub(at.as_secs() as i64));
        out.push(lock_owner_record(
            &owner,
            crate::update_lock::owner_activity(&owner),
            age,
        ));
    } else if crate::update_lock::is_initializing(dir) {
        // An update is between creating the lock and writing its owner; a
        // crash there turns into the incomplete record below within seconds.
        out.push(
            Record::warn("update lock is being initialized", None)
                .with_hint("rerun dot doctor in a few seconds"),
        );
    } else {
        out.push(
            Record::warn(
                "update lock record is incomplete",
                Some("it has no owner record".to_string()),
            )
            .with_hint("run dot update; it recovers the lock"),
        );
    }
    out
}

/// One merge-hook output declaration for output verification
/// (handoff finding #8): the script and its `.outputs` sidecar arrive
/// trust-validated from [`crate::doctor`]; the outputs arrive expanded
/// to absolute paths.
pub struct MergeSpec {
    /// Hook identity (the spec label doctor reports).
    pub identity: String,
    /// Hook script path.
    pub script: String,
    /// Expanded absolute declared live outputs.
    pub outputs: Vec<String>,
    /// Declared outputs that are not absolute paths after
    /// expansion (raw lines).
    pub invalid: Vec<String>,
}

/// Inputs for [`check_merges`]: the extension boundary plus the
/// merge-hook inventory as explicit data.
pub struct MergeInputs {
    /// `_dot_extensions_enabled` (`DOT_EXTENSION_API == 1` with a
    /// non-empty `DOT_EXTENSIONS_DIR`).
    pub enabled: bool,
    /// `DOT_EXTENSIONS_DIR`, spelled exactly as configured: the
    /// check concatenates `$DOT_EXTENSIONS_DIR/merge-hooks.d`
    /// (an empty value probes `/merge-hooks.d`, but `enabled` is
    /// false then and the root is never built).
    pub extensions_dir: String,
    /// `_merge_hook_specs` output line count (`wc -l` semantics),
    /// or `None` when the inventory pipeline itself fails. The
    /// spec listing stays shell-side (`merges` slice); only its
    /// count crosses here.
    pub spec_count: Option<usize>,
    /// Why the inventory is invalid, with its next step, when
    /// `spec_count` is `None`; the row falls back to the directory.
    pub inventory_error: Option<String>,
    /// Per-hook output declarations for verification. Empty skips
    /// verification (the historical count-only behavior); production
    /// always passes one entry per inventoried hook.
    pub specs: Vec<MergeSpec>,
}

/// One merge spec's output verification result, before aggregation.
enum MergeVerdict {
    /// Problems to report row by row (invalid declarations, missing
    /// outputs).
    Problems(Vec<Record>),
    /// No declared outputs.
    Undeclared,
    /// Every declared output exists.
    Current(usize),
}

/// Verify one merge spec's declared live outputs: each must exist. Hooks
/// without declarations are not verified (documented, not a failure);
/// invalid declarations fail outright.
///
/// Existence is the whole rule. An output's mtime says nothing about
/// whether it is current: `dot_write_text_if_changed` and the managed-block
/// helpers deliberately leave an unchanged destination untouched, so after
/// any edit to a hook that does not change its output, the output is older
/// than the hook and an mtime rule would call a correct file stale. No hook
/// could declare outputs under that rule.
fn verify_merge_outputs(spec: &MergeSpec) -> MergeVerdict {
    if !spec.invalid.is_empty() {
        return MergeVerdict::Problems(
            spec.invalid
                .iter()
                .map(|raw| {
                    Record::fail(
                        "merge-hook output declaration is invalid",
                        Some(format!("{}: {raw}", spec.identity)),
                    )
                })
                .collect(),
        );
    }
    if spec.outputs.is_empty() {
        return MergeVerdict::Undeclared;
    }
    let problems: Vec<Record> = spec
        .outputs
        .iter()
        .filter(|output| std::fs::metadata(Path::new(output)).is_err())
        .map(|output| {
            Record::fail(
                "merge-hook output is missing",
                Some(format!("{}: {output}", spec.identity)),
            )
            .with_hint("run dot update; if it stays missing, check the hook")
        })
        .collect();
    if problems.is_empty() {
        MergeVerdict::Current(spec.outputs.len())
    } else {
        MergeVerdict::Problems(problems)
    }
}

/// Every spec's output verification, collapsed: problems keep one row each,
/// while healthy hooks fold into a single summary row (one row per hook
/// used to bury the problems among dozens of identical rows). Hooks that
/// declare no outputs file nothing: a permanent "unverified" row on every
/// run said nothing a user could act on.
fn merge_output_records(specs: &[MergeSpec]) -> Vec<Record> {
    let mut problems = Vec::new();
    let (mut current_hooks, mut current_outputs) = (0usize, 0usize);
    for spec in specs {
        match verify_merge_outputs(spec) {
            MergeVerdict::Problems(rows) => problems.extend(rows),
            MergeVerdict::Undeclared => {}
            MergeVerdict::Current(outputs) => {
                current_hooks += 1;
                current_outputs += outputs;
            }
        }
    }
    let mut out = problems;
    if current_hooks > 0 {
        out.push(Record::ok(
            "merge-hook outputs exist",
            Some(format!(
                "{current_outputs} output(s) across {current_hooks} hook(s)"
            )),
        ));
    }
    out
}

/// `_dr_check_merges` (`doctor/merges.sh`): merge-hook extension
/// discovery health. The `-e`/`-d`/`-L` root probes run in-process;
/// the inventory count arrives via [`MergeInputs::spec_count`].
/// Past discovery, each [`MergeInputs::specs`] entry verifies its
/// declared live outputs (handoff finding #8 goes beyond the shell
/// port, which counted specs only).
pub fn check_merges(inputs: &MergeInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Extensions")];
    if !inputs.enabled {
        out.push(Record::skip("no extension root configured", None));
        return out;
    }
    let root = format!("{}/merge-hooks.d", inputs.extensions_dir);
    let root_path = Path::new(&root);
    let present = std::fs::symlink_metadata(root_path).is_ok();
    if !present {
        out.push(Record::skip(
            "merge-hook extensions",
            Some("none configured".to_string()),
        ));
        return out;
    }
    let meta = std::fs::symlink_metadata(root_path).ok();
    let is_dir = meta.as_ref().is_some_and(|meta| meta.is_dir());
    let is_link = meta.as_ref().is_some_and(|meta| meta.is_symlink());
    if !is_dir || is_link {
        out.push(Record::fail(
            "merge-hook extension directory is unavailable",
            Some(root),
        ));
        return out;
    }
    let Some(count) = inputs.spec_count else {
        out.push(Record::fail(
            "merge-hook extension inventory is invalid",
            Some(inputs.inventory_error.clone().unwrap_or(root)),
        ));
        return out;
    };
    if count > 0 {
        out.push(Record::ok(
            "merge-hook extensions",
            Some(format!("{count} hook(s)")),
        ));
        out.extend(merge_output_records(&inputs.specs));
    } else {
        out.push(Record::skip(
            "merge-hook extensions",
            Some("none configured".to_string()),
        ));
    }
    out
}

/// Inputs for [`check_cron_freshness`]: the cron stamps plus the
/// current clock, all epoch seconds.
pub struct CronInputs {
    /// Epoch of the last fully clean cron run, or `None` when no
    /// clean run was ever recorded.
    pub last_success: Option<i64>,
    /// The last converged (clean or degraded) cron run, or `None` when
    /// none was recorded (including every run by a Dot older than the
    /// convergence stamp).
    pub last_converged: Option<crate::update_status::Converged>,
    /// The last update run of any trigger (`update.last-run`), or `None`
    /// when none was recorded (including every run by a Dot older than
    /// that stamp).
    pub last_run: Option<crate::update_status::LastRun>,
    /// Why the last non-ok run failed (`update.last-failure`), or `None`
    /// when none was recorded. Shown only beside the run it describes.
    pub last_failure: Option<crate::update_status::LastFailure>,
    /// Whether a `crontab` command is on `PATH`. Without one the host
    /// cannot schedule `dot update --cron`, so a cron that never ran skips
    /// instead of warning.
    pub cron_available: bool,
    /// Current epoch seconds.
    pub now: i64,
}

/// Cron convergence freshness (handoff finding #1): warns when the
/// last clean cron run is older than
/// [`crate::update_status::CRON_STALE_AFTER_SECS`]. Warn, not fail:
/// a stale stamp usually self-heals on the next slot, and sleeping
/// machines go stale with nothing broken. A missing stamp skips:
/// fresh installs have never converged.
///
/// A host whose runs keep converging while the Tools or Prune stage
/// fails, or while its config misspells a key, is reported as degraded with the failing stages instead of
/// "has not succeeded recently", which stays reserved for a host that
/// stopped converging.
///
/// A cron run that did not succeed and is newer than the last clean one
/// always warns, even inside the staleness window: a recent clean run used
/// to vouch for the host until it aged out, so a run that started failing
/// right after one read green for up to two hours. A run skipped for local
/// edits names that as the cause (with the files), since cron stays frozen
/// until the edits are resolved. Every row about a run that failed shows
/// its cause from `update.last-failure` when that record describes the run.
///
/// The any-trigger last-run stamp fills the gaps the cron stamps leave: a
/// host whose cron entry never ran warns once its last hand-run update is
/// older than the staleness window (instead of reading "unknown" forever),
/// and the last hand-run update is reported whenever it adds information:
/// always without a recent clean cron run, otherwise only when it did not
/// succeed.
pub fn check_cron_freshness(inputs: &CronInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Update")];
    let clean = cron_freshness(inputs, &mut out);
    if let Some(record) = last_run_record(inputs, clean) {
        out.push(record);
    }
    out
}

/// The cron row of [`check_cron_freshness`]; returns the epoch of the last
/// clean cron run when it is recent enough to vouch for the host.
fn cron_freshness(inputs: &CronInputs, out: &mut Vec<Record>) -> Option<i64> {
    use crate::update_status::{OUTCOME_OK, OUTCOME_SKIP, format_age, is_stale};

    let age = |at: i64| format_age(inputs.now.saturating_sub(at));
    let converged = inputs.last_converged.as_ref();
    // A clean convergence stamp is a success even if the last-success
    // write was lost; both are written by the same clean run. A clean cron
    // last run counts too, in case both stamp writes were lost.
    let clean_cron_run = inputs
        .last_run
        .as_ref()
        .filter(|last| last.is_cron() && last.outcome == OUTCOME_OK)
        .map(|last| last.at);
    let clean = converged
        .filter(|converged| converged.failing.is_empty())
        .map(|converged| converged.at)
        .into_iter()
        .chain(inputs.last_success)
        .chain(clean_cron_run)
        .max();
    // The newest cron run, when it did not succeed after the last clean one.
    // A clean stamp from the future (the clock stepped back) orders nothing,
    // and every run overwrites the last-run stamp, so that run wins then.
    let problem = inputs
        .last_run
        .as_ref()
        .filter(|last| last.is_cron() && last.outcome != OUTCOME_OK)
        .filter(|last| clean.is_none_or(|clean| last.at > clean || clean > inputs.now));
    let since = match clean {
        Some(clean) => format!("last success {} ago", age(clean)),
        None => "no successful cron update recorded".to_string(),
    };
    // Only a recent skip says cron is running and blocked; an old one means
    // cron stopped too, which the stale rows below report (with the edits).
    if let Some(last) =
        problem.filter(|last| last.outcome == OUTCOME_SKIP && !is_stale(last.at, inputs.now))
    {
        let cause = skip_cause(inputs, last);
        out.push(cause.attach(Record::warn(
            "cron update skipping: local edits block it",
            Some(cause.detail(format!("last cron run {} ago; {since}", age(last.at)))),
        )));
        return None;
    }
    let fresh_clean = clean.filter(|clean| !is_stale(*clean, inputs.now));
    if let Some(clean) = fresh_clean {
        let Some(last) = problem else {
            out.push(Record::ok(
                "cron update succeeded recently",
                Some(format!("{} ago", age(clean))),
            ));
            return Some(clean);
        };
        let cause = run_cause(inputs, last);
        out.push(cause.attach(Record::warn(
            format!("last cron run {}", outcome_phrase(last)),
            Some(cause.detail(format!("{} ago; {since}", age(last.at)))),
        )));
        return None;
    }
    if let Some(converged) = converged
        .filter(|converged| !converged.failing.is_empty() && !is_stale(converged.at, inputs.now))
    {
        // A cron run after that degraded convergence did not converge: it
        // is the news, and the degraded stages would mislabel its cause.
        if let Some(last) = problem.filter(|last| last.at > converged.at) {
            let cause = run_cause(inputs, last);
            out.push(cause.attach(Record::warn(
                format!("last cron run {}", outcome_phrase(last)),
                Some(cause.detail(format!(
                    "{} ago; {since}; last converged {} ago",
                    age(last.at),
                    age(converged.at)
                ))),
            )));
            return None;
        }
        let since = match inputs.last_success {
            Some(last) => format!("since last success {} ago", age(last)),
            None => "no clean cron update recorded".to_string(),
        };
        // The run that wrote the convergence explains it; without that run
        // (a later hand run, or an older Dot) its stages still pick the step.
        let cause = problem
            .filter(|last| last.at == converged.at)
            .map(|last| run_cause(inputs, last))
            .unwrap_or_else(|| Cause::step(next_step(stages_need_shdeps(&converged.failing))));
        out.push(cause.attach(Record::warn(
            format!("cron update degraded: {} failing", converged.failing),
            Some(cause.detail(format!("{since}; last converged {} ago", age(converged.at)))),
        )));
        return None;
    }
    // The newest failing cron run, named with its cause: the stale titles
    // below describe the host, not that run. Without one, cron itself may
    // have stopped running.
    let cause = match problem {
        Some(last) => last_cron_note(inputs, last),
        None => {
            Cause::step("check that dot update --cron is scheduled (crontab -l), or run dot update")
        }
    };
    // Stale from here on. `clean` (not just last-success) is the success
    // reference, so a lone clean convergence stamp reads as a success. Only
    // a degraded convergence newer than it adds information (an older one is
    // left behind by a downgrade to a Dot that does not write it).
    let converged_note = converged
        .filter(|converged| clean.is_none_or(|clean| converged.at > clean))
        .map(|converged| format!("; last converged {} ago", age(converged.at)))
        .unwrap_or_default();
    match clean {
        Some(clean) => out.push(cause.attach(Record::warn(
            "cron update has not succeeded recently",
            Some(cause.detail(format!("last success {} ago{converged_note}", age(clean)))),
        ))),
        // Only degraded runs ever converged, and they stopped too.
        None if converged.is_some() => out.push(cause.attach(Record::warn(
            "cron update has not succeeded recently",
            Some(cause.detail(format!(
                "no successful cron update recorded{converged_note}"
            ))),
        ))),
        None => out.push(never_converged_record(inputs)),
    }
    None
}

/// `last cron run <outcome> <age> ago[; <cause>]` plus that run's next
/// step, for rows whose title describes the host rather than that run.
fn last_cron_note(inputs: &CronInputs, last: &crate::update_status::LastRun) -> Cause {
    let why = if last.outcome == crate::update_status::OUTCOME_SKIP {
        skip_cause(inputs, last)
    } else {
        run_cause(inputs, last)
    };
    let note = why.detail(format!(
        "last cron run {} {} ago",
        outcome_phrase(last),
        crate::update_status::format_age(inputs.now.saturating_sub(last.at))
    ));
    Cause {
        listed: Some(note),
        step: why.step,
    }
}

/// `failed`, `degraded: <stages> failing`, `skipped for local edits`, or a
/// newer outcome word as is, for text about `run`.
fn outcome_phrase(run: &crate::update_status::LastRun) -> String {
    use crate::update_status::{OUTCOME_DEGRADED, OUTCOME_FAIL, OUTCOME_SKIP};

    if run.outcome == OUTCOME_FAIL {
        "failed".to_string()
    } else if run.outcome == OUTCOME_SKIP {
        "skipped for local edits".to_string()
    } else if run.outcome == OUTCOME_DEGRADED && !run.failing.is_empty() {
        format!("degraded: {} failing", run.failing)
    } else {
        run.outcome.clone()
    }
}

/// The `update.last-failure` record when it describes `run`.
fn failure_of<'a>(
    inputs: &'a CronInputs,
    run: &crate::update_status::LastRun,
) -> Option<&'a crate::update_status::LastFailure> {
    inputs
        .last_failure
        .as_ref()
        .filter(|failure| failure.describes(run))
}

/// Failing items shown in one row before `+N more`.
const SHOWN_FAILURE_ITEMS: usize = 3;

/// Longest item detail shown in a row (the record keeps more): room for a
/// typical one-line error, while three items still fit a wide terminal row.
const SHOWN_FAILURE_DETAIL_BYTES: usize = 120;

/// What a row about a run that did not succeed says beyond its title: the
/// cause, which joins the row's detail, and the next step, which renders as
/// the row's `→` hint like every other section's steps.
struct Cause {
    /// The cause (`failing: …`, `edited: …`, or a whole note about the
    /// last cron run), or `None` when nothing names it (an older Dot wrote
    /// the stamp, or the run recorded no item).
    listed: Option<String>,
    /// The next step.
    step: &'static str,
}

impl Cause {
    /// A cause that is only its next step.
    fn step(step: &'static str) -> Self {
        Cause { listed: None, step }
    }

    /// `facts` followed by the cause, when there is one.
    fn detail(&self, facts: String) -> String {
        match &self.listed {
            Some(listed) => format!("{facts}; {listed}"),
            None => facts,
        }
    }

    /// `record` with the next step attached as its hint.
    fn attach(&self, record: Record) -> Record {
        record.with_hint(self.step)
    }
}

/// One recorded item detail as a row shows it: a leading `error: ` (the
/// recording tool's own prefix, which says nothing the row's warning does
/// not) dropped, then shortened after a whole word with an ellipsis. The
/// record itself keeps the full, already sanitized text.
fn shown_detail(detail: &str) -> String {
    let detail = detail.trim();
    let detail = detail
        .strip_prefix("error: ")
        .unwrap_or(detail)
        .trim_start();
    shorten(detail, SHOWN_FAILURE_DETAIL_BYTES)
}

/// `text` cut to at most `max` bytes, ellipsis included, after the last
/// whole word that fits: a cut mid-word reads as a typo.
/// Trailing clause punctuation goes too, so a cut at a clause boundary
/// reads cleanly. A single word longer than the budget (a path, a URL) is
/// cut at a character boundary instead, since no word boundary exists.
fn shorten(text: &str, max: usize) -> String {
    const ELLIPSIS: char = '…';
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max.saturating_sub(ELLIPSIS.len_utf8());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // Ending exactly before a space or clause punctuation keeps the whole
    // last word; otherwise back off to the previous space, unless that would
    // drop more than half of the budget.
    let end = if text[cut..].starts_with([' ', ',', ';', ':']) {
        cut
    } else {
        match text[..cut].rfind(' ') {
            Some(space) if space >= cut / 2 => space,
            _ => cut,
        }
    };
    let mut out = text[..end]
        .trim_end_matches([' ', ',', ';', ':'])
        .to_string();
    out.push(ELLIPSIS);
    out
}

/// Why `run` failed plus the next step: `failing: tools: ripgrep (network
/// unavailable)` for the row detail, and the step for its hint. Without a
/// matching record (an older Dot wrote the stamp, or the run named nothing)
/// only the next step remains. A failing Tools or Prune stage points at
/// `shdeps health`, which explains dependency state without a rerun.
fn run_cause(inputs: &CronInputs, run: &crate::update_status::LastRun) -> Cause {
    use crate::update_status::{STAGE_PRUNE, STAGE_TOOLS};

    let failure = failure_of(inputs, run);
    let items = failure.map_or(&[][..], |failure| failure.items.as_slice());
    let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
    for item in items.iter().take(SHOWN_FAILURE_ITEMS) {
        let detail = shown_detail(&item.detail);
        let text = match (item.name.is_empty(), detail.is_empty()) {
            (false, false) => format!("{} ({detail})", item.name),
            (false, true) => item.name.clone(),
            (true, _) => detail,
        };
        match groups.iter_mut().find(|(stage, _)| *stage == item.stage) {
            Some((_, texts)) => texts.push(text),
            None => groups.push((&item.stage, vec![text])),
        }
    }
    let more = items.len().saturating_sub(SHOWN_FAILURE_ITEMS)
        + failure.map_or(0, |failure| failure.omitted);
    // The record says which stage failed; without one (an older Dot), the
    // degraded stages do. A provider that could not even be prepared is
    // not `shdeps health`'s to explain.
    let dependency = if items.is_empty() {
        stages_need_shdeps(&run.failing)
    } else {
        items
            .iter()
            .any(|item| item.stage == STAGE_TOOLS || item.stage == STAGE_PRUNE)
    };
    let step = next_step(dependency);
    if groups.is_empty() {
        return Cause::step(step);
    }
    let mut listed = groups
        .iter()
        .map(|(stage, texts)| format!("{stage}: {}", texts.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    if more > 0 {
        listed.push_str(&format!(" +{more} more"));
    }
    Cause {
        listed: Some(format!("failing: {listed}")),
        step,
    }
}

/// Whether a comma-separated degraded stage list names a dependency stage
/// (Tools or Prune), which `shdeps health` explains without a rerun.
fn stages_need_shdeps(failing: &str) -> bool {
    use crate::update_status::{STAGE_PRUNE, STAGE_TOOLS};

    failing
        .split(',')
        .any(|stage| stage == STAGE_TOOLS || stage == STAGE_PRUNE)
}

/// The next step for a failed run: `shdeps health` when a dependency stage
/// failed, otherwise the update's own output.
fn next_step(dependency: bool) -> &'static str {
    if dependency {
        "run shdeps health, or dot update for the full output"
    } else {
        "run dot update for the full output"
    }
}

/// The edited files that skipped `run` (a cron skip), `edited: .bashrc,
/// .zshrc`, plus the next step. The record keeps the first few; without one
/// (an older Dot) only the next step remains.
fn skip_cause(inputs: &CronInputs, run: &crate::update_status::LastRun) -> Cause {
    let step = "run dot status, then commit, stash, or resolve the edits";
    let Some(failure) = failure_of(inputs, run) else {
        return Cause::step(step);
    };
    let files: Vec<&str> = failure
        .items
        .iter()
        .filter(|item| item.stage == crate::update_status::STAGE_DIRTY)
        .map(|item| item.name.as_str())
        .collect();
    if files.is_empty() {
        return Cause::step(step);
    }
    let mut listed = files
        .iter()
        .take(SHOWN_FAILURE_ITEMS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let more = files.len().saturating_sub(SHOWN_FAILURE_ITEMS) + failure.omitted;
    if more > 0 {
        listed.push_str(&format!(" +{more} more"));
    }
    Cause {
        listed: Some(format!("edited: {listed}")),
        step,
    }
}

/// The cron row when no cron run ever converged. Before the last-run stamp
/// this always read "unknown"; with it, a cron run that only ever failed
/// warns, and a host updated only by hand warns once its last update is
/// older than the staleness window (a cron entry would have run by then),
/// while a host that has never updated at all still reads unknown.
fn never_converged_record(inputs: &CronInputs) -> Record {
    use crate::update_status::{format_age, is_stale};

    let age = |at: i64| format_age(inputs.now.saturating_sub(at));
    let Some(last) = inputs.last_run.as_ref() else {
        return Record::skip(
            "cron update success is unknown",
            Some("no successful cron update recorded".to_string()),
        );
    };
    if last.is_cron() {
        let note = last_cron_note(inputs, last);
        return note.attach(Record::warn(
            "cron update has not succeeded recently",
            Some(note.detail("no successful cron update recorded".to_string())),
        ));
    }
    if !inputs.cron_available {
        // No `crontab` (Termux, containers): this host is updated by hand
        // by design, so a lasting warning would only be noise.
        return Record::skip(
            "cron update has never run",
            Some(format!(
                "no crontab on PATH; last update: {} run {} ago",
                last.trigger,
                age(last.at)
            )),
        );
    }
    if is_stale(last.at, inputs.now) {
        return Record::warn(
            "cron update has never run",
            Some(format!(
                "last update: {} run {} ago",
                last.trigger,
                age(last.at)
            )),
        )
        .with_hint("schedule dot update --cron to keep this host current");
    }
    Record::skip(
        "cron update has not run yet",
        Some(format!(
            "last update: {} run {} ago",
            last.trigger,
            age(last.at)
        )),
    )
}

/// The last hand-run (`manual` or `init`) update, when it adds information
/// beyond the cron row: always without a recent clean cron run (the cron
/// row cannot vouch for this host then), otherwise only when it did not
/// succeed and is newer than that clean run. A failed or degraded run
/// warns rather than fails: the conditions that make every update exit 1
/// have rows of their own, and a one-off failure (a network blip) heals on
/// the next run.
fn last_run_record(inputs: &CronInputs, clean_cron: Option<i64>) -> Option<Record> {
    use crate::update_status::{OUTCOME_DEGRADED, OUTCOME_OK, format_age};

    let last = inputs.last_run.as_ref().filter(|last| !last.is_cron())?;
    let succeeded = last.outcome == OUTCOME_OK;
    if let Some(clean) = clean_cron {
        if succeeded || last.at <= clean {
            return None;
        }
    }
    let detail = format!(
        "{} run {} ago",
        last.trigger,
        format_age(inputs.now.saturating_sub(last.at))
    );
    if succeeded {
        return Some(Record::ok("last update succeeded", Some(detail)));
    }
    let title = if last.outcome == OUTCOME_DEGRADED {
        let failing = if last.failing.is_empty() {
            "a stage"
        } else {
            last.failing.as_str()
        };
        format!("last update degraded: {failing} failing")
    } else {
        "last update failed".to_string()
    };
    let cause = run_cause(inputs, last);
    Some(cause.attach(Record::warn(title, Some(cause.detail(detail)))))
}

/// The provider re-exec checkpoint row for the `Update` section (none when
/// the record is absent). `dot update` refuses to proceed past a record it
/// cannot consume, so those states fail; a record the next update will
/// validate and remove only warns. `state` comes from
/// [`crate::shdeps::checkpoint_state`], which shares the consume logic.
pub fn check_reexec_checkpoint(
    state: &crate::shdeps::CheckpointState,
    path: &Path,
    home: &str,
) -> Vec<Record> {
    use crate::shdeps::CheckpointState;

    let shown = tilde(&path.to_string_lossy(), home);
    let short = |revision: &str| -> String {
        if revision.is_empty() {
            "<unavailable>".to_string()
        } else {
            revision.chars().take(12).collect()
        }
    };
    match state {
        CheckpointState::Absent => Vec::new(),
        CheckpointState::Pending => vec![
            Record::warn(
                "provider re-exec checkpoint pending",
                Some(format!("{shown}: dot changed twice during the last update")),
            )
            .with_hint("run dot update; it validates and removes the checkpoint"),
        ],
        CheckpointState::Unreadable => vec![
            Record::fail(
                "provider re-exec checkpoint blocks dot update",
                Some(format!("{shown} is unsafe or malformed")),
            )
            .with_hint("inspect it, remove it, then run dot update"),
        ],
        CheckpointState::Mismatch { pinned, active } => vec![
            Record::fail(
                "provider re-exec checkpoint blocks dot update",
                Some(format!(
                    "{shown} pins {} but dot is at {}",
                    short(pinned),
                    short(active)
                )),
            )
            .with_hint("inspect the provider state, remove the record, then run dot update"),
        ],
    }
}

/// Inputs for [`check_profile_lifecycle`]: the profile lifecycle
/// arrays plus the two helper boundaries as explicit data.
pub struct LifecycleInputs<'a> {
    /// `DOT_PROFILES_PRESENT == 1`. When false the shell function
    /// returns silently (no section: the `Profiles` heading belongs
    /// to [`check_overlays`]).
    pub profiles_present: bool,
    /// `_dot_profile_lifecycle_load` exit status.
    pub load_ok: bool,
    /// `ELIGIBLE_OVERLAY_NAMES`.
    pub eligible: Vec<String>,
    /// `ACTIVE_OVERLAYS` raw `name|...` records (later entries win,
    /// like the shell associative assignment).
    pub active: Vec<String>,
    /// `DOT_PROFILE_LIFECYCLE_RECORDS` raw `name|...` records.
    pub records: Vec<String>,
    /// `_dot_extensions_enabled`.
    pub extensions_enabled: bool,
    /// `_dot_profile_deactivation_script "$record" >/dev/null`
    /// exit status per record: the deactivation-authority probe
    /// (trust policy owned by the profile slice).
    pub deactivation_ok: &'a dyn Fn(&str) -> bool,
}

/// `_dr_check_profile_lifecycle` (`doctor/overlays.sh`): pending
/// profile deactivations must each retain a usable deactivation
/// authority. Emits no section of its own; [`check_overlays`]
/// appends these records under its `Profiles` heading, and tests
/// drive this function directly for the lifecycle matrix.
pub fn check_profile_lifecycle(inputs: &LifecycleInputs) -> Vec<Record> {
    let mut out = Vec::new();
    if !inputs.profiles_present {
        return out;
    }
    if !inputs.load_ok {
        out.push(Record::fail(
            "profile lifecycle state unsafe",
            Some("run dot update after repairing the lifecycle ledger".to_string()),
        ));
        return out;
    }
    let eligible: HashSet<&str> = inputs.eligible.iter().map(String::as_str).collect();
    let mut active: BTreeMap<&str, &str> = BTreeMap::new();
    for record in &inputs.active {
        active.insert(record_name(record), record.as_str());
    }
    let mut pending: Vec<&str> = Vec::new();
    for record in &inputs.records {
        let name = record_name(record);
        if eligible.contains(name) {
            if let Some(active_record) = active.get(name) {
                if !(inputs.deactivation_ok)(active_record) {
                    // The rules of `profile_lifecycle::deactivation_script` and
                    // `extension_trust::deactivation_validate`, which refuse
                    // this script until they all hold.
                    let script = format!(
                        "{}/dot/profile-deactivate",
                        read_fields(active_record, 3)[1]
                    );
                    out.push(
                        Record::fail(
                            format!("{name}: active profile deactivation authority unsafe"),
                            None,
                        )
                        .with_hint(format!(
                            "{script} must be a regular file you own, in your git overlay clone at ~/.dotfiles-{name} whose origin matches its descriptor, with no parent directory others can write; fix that, then run dot update"
                        )),
                    );
                }
            } else if !(inputs.deactivation_ok)(record) {
                out.push(Record::warn(
                    format!("{name}: retained profile deactivation authority unavailable"),
                    Some("selected optional overlay is not currently active".to_string()),
                ));
            }
            continue;
        }
        pending.push(name);
        if !inputs.extensions_enabled {
            out.push(Record::fail(
                "profile deactivation pending while extensions are disabled",
                Some(name.to_string()),
            ));
            continue;
        }
        if !(inputs.deactivation_ok)(record) {
            out.push(Record::fail(
                format!("{name}: retiring overlay authority unsafe"),
                Some("restore the recorded checkout identity, then run dot update".to_string()),
            ));
            continue;
        }
    }
    if pending.is_empty() {
        out.push(Record::ok(
            "profile lifecycle state",
            Some("no pending deactivations".to_string()),
        ));
    } else {
        out.push(Record::fail(
            "profile deactivation pending",
            Some(format!("{} (run dot update to retry)", pending.join(" "))),
        ));
    }
    out
}

/// Inputs for [`check_overlays`]: every profile/overlay global as
/// explicit data plus the one trust-policy probe.
pub struct OverlayInputs<'a> {
    /// `$HOME`, for `_dr_tilde` display only.
    pub home: &'a str,
    /// `DOT_PROFILE_CONFIGURATION_ERROR` (unset or empty: absent).
    pub profile_config_error: Option<&'a str>,
    /// `DOT_PROFILES_PRESENT == 1`.
    pub profiles_present: bool,
    /// `DOT_PROFILE_CURRENT_USER` (identity needs the host too).
    pub profile_user: Option<&'a str>,
    /// `DOT_PROFILE_CURRENT_HOST`.
    pub profile_host: Option<&'a str>,
    /// `SELECTED_PROFILE` (unset or empty: absent).
    pub selected_profile: Option<&'a str>,
    /// `DOT_PROFILE_SELECTION_STATE` (defaults to `unknown`).
    pub selection_state: Option<&'a str>,
    /// `INCLUDED_PROFILES`.
    pub included_profiles: Vec<String>,
    /// `PHASE_ONE_SELECTED_OVERLAY_NAMES`.
    pub phase_one: Vec<String>,
    /// `DOT_PROFILE_SELECTOR_RECORDS` raw
    /// `class|path|user|host|profile|matched` records.
    pub selectors: Vec<String>,
    /// Keys the profile, selector, and selected descriptor files hold
    /// that this release does not know (warning rows, never stderr).
    pub unknown_keys: Vec<crate::unknown_keys::DataKey>,
    /// Nested [`LifecycleInputs`] for the profiles branch.
    pub lifecycle: LifecycleInputs<'a>,
    /// `${#CONFIGURED_OVERLAY_NAMES[@]}` (names are unused beyond
    /// the count).
    pub configured_count: usize,
    /// `DOT_OVERLAY_MANIFEST` path, spelled as configured.
    pub manifest: String,
    /// `DOT_OVERLAY_DISCOVERY_ERROR` (unset or empty: absent).
    pub discovery_error: Option<&'a str>,
    /// `ACTIVE_OVERLAYS` raw `name|path|url|descriptor|optional|sync`
    /// records (later entries win).
    pub active_records: Vec<String>,
    /// `DOT_OVERLAY_LIFECYCLE` raw `name|state|descriptor` records.
    pub overlay_lifecycle: Vec<String>,
    /// `_overlay_local_source_validate "$path"` per overlay path:
    /// `Ok` when the local source is available, `Err(reply)` with
    /// the shell `REPLY` diagnostic otherwise (empty `REPLY`
    /// falls back to `$path/home`, like `${REPLY:-$path/home}`).
    /// Trust policy owned by the overlay slice.
    pub local_validate: &'a dyn Fn(&str) -> Result<(), String>,
}

/// `~/path:line key (newer dot?)`: where an unknown data-file key sits.
fn data_key_detail(key: &crate::unknown_keys::DataKey, home: &str) -> String {
    format!(
        "{}:{} {} (newer dot?)",
        tilde(&key.path, home),
        key.line,
        key.key
    )
}

/// A non-empty option: shell `[[ -n ${var:-} ]]` treats unset and
/// empty identically.
fn present(value: Option<&str>) -> Option<&str> {
    match value {
        Some(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// The profile selection as one informational row: the selected profile
/// and how it was chosen, the profiles it includes, the phase-one
/// overlays, every matching selector, and the identity selectors matched
/// against. These are configuration facts that never change between runs,
/// and they used to take five rows; `None` when there is nothing to say.
fn profile_record(inputs: &OverlayInputs) -> Option<Record> {
    let mut facts = Vec::new();
    let selected = present(inputs.selected_profile);
    if selected.is_some() {
        facts.push(
            present(inputs.selection_state)
                .unwrap_or("unknown")
                .to_string(),
        );
    }
    if !inputs.included_profiles.is_empty() {
        facts.push(format!("includes {}", inputs.included_profiles.join(" ")));
    }
    if !inputs.phase_one.is_empty() {
        facts.push(format!("phase-one overlays {}", inputs.phase_one.join(" ")));
    }
    for selector in &inputs.selectors {
        let fields = read_fields(selector, 6);
        if fields[5] != "true" {
            continue;
        }
        let source: String = match fields[0].as_str() {
            "root" => "root".to_string(),
            "local" => "machine-local".to_string(),
            "personal" => "active personal overlay".to_string(),
            other => other.to_string(),
        };
        let leaf = match fields[1].rsplit('/').next() {
            Some(leaf) => leaf,
            None => fields[1].as_str(),
        };
        facts.push(format!("{source} selector {leaf} -> {}", fields[4]));
    }
    if let (Some(user), Some(host)) = (present(inputs.profile_user), present(inputs.profile_host)) {
        facts.push(format!("for {user}@{host}"));
    }
    if selected.is_none() && facts.is_empty() {
        return None;
    }
    let message = match selected {
        Some(selected) => format!("profile {selected}"),
        None => "profile".to_string(),
    };
    Some(Record::info(message, Some(facts.join("; "))))
}

/// `_dr_check_overlays` (`doctor/overlays.sh`): profile selection
/// reporting, per-overlay lifecycle and source health, and overlay
/// symlink ownership validation. The profile identity, selection, and
/// matching selector are configuration facts, so they render as one
/// informational row that is never counted. A healthy overlay folds into
/// one row; any problem keeps all of its rows.
///
/// Worktree, URL, and origin probes reuse [`crate::overlays`];
/// manifest parsing and link-target derivation reuse
/// [`crate::repos_overlays`]; the manifest file and `$HOME` links
/// are read in-process (the shell `readlink` batch is a
/// performance shape with no observable difference). Only
/// [`OverlayInputs::local_validate`] is injected.
pub fn check_overlays(inputs: &OverlayInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Profiles")];
    // `dot update` holds the installed overlays while this release cannot
    // tell which ones a newer Dot would activate; the selection reported
    // here is not what is linked.
    let held = inputs.unknown_keys.iter().any(|key| key.effect.holds());
    if let Some(error) = present(inputs.profile_config_error) {
        out.push(Record::fail(
            "profile configuration invalid",
            Some(error.to_string()),
        ));
    } else if !inputs.profiles_present {
        out.push(Record::skip(
            "profile selection disabled",
            Some("no profiles.d directory; using legacy overlay discovery".to_string()),
        ));
    } else {
        out.extend(profile_record(inputs));
        for key in &inputs.unknown_keys {
            let message = match key.effect {
                crate::unknown_keys::Effect::Ignored => "unknown profile key ignored",
                crate::unknown_keys::Effect::SelectorSkipped => "selector skipped: unknown key",
                crate::unknown_keys::Effect::SelectorFallback => {
                    "selector skipped: unknown key; profile base selected"
                }
                crate::unknown_keys::Effect::SelectorsUnread(_) => {
                    "personal selectors unread: overlay skipped; profile base selected"
                }
                crate::unknown_keys::Effect::OverlaySkipped(_) => continue,
            };
            out.push(Record::warn(
                message,
                Some(data_key_detail(key, inputs.home)),
            ));
        }
        if held {
            // Held overlays stay installed and keep their lifecycle
            // authority: judge every ledger record as still selected, so
            // nothing reads as a pending deactivation that `dot update`
            // will not (and must not) run, while trust checks still apply.
            let lifecycle = &inputs.lifecycle;
            let mut eligible = lifecycle.eligible.clone();
            eligible.extend(
                lifecycle
                    .records
                    .iter()
                    .map(|record| record_name(record).to_string()),
            );
            out.extend(check_profile_lifecycle(&LifecycleInputs {
                profiles_present: lifecycle.profiles_present,
                load_ok: lifecycle.load_ok,
                eligible,
                active: lifecycle.active.clone(),
                records: lifecycle.records.clone(),
                extensions_enabled: lifecycle.extensions_enabled,
                deactivation_ok: lifecycle.deactivation_ok,
            }));
        } else {
            out.extend(check_profile_lifecycle(&inputs.lifecycle));
        }
    }

    out.push(Record::section(format!(
        "Overlays ({} configured)",
        inputs.configured_count
    )));
    if let Some(error) = present(inputs.discovery_error) {
        out.push(Record::fail(
            "overlay descriptor invalid",
            Some(error.to_string()),
        ));
    }
    if held {
        out.push(Record::warn(
            "overlay set held: newer keys need a newer dot",
            Some(
                "dot update keeps the installed overlays until a dot that knows the keys runs"
                    .to_string(),
            ),
        ));
    }
    // Legacy discovery gives a skipped `sync=none` descriptor a lifecycle
    // record but does not count it as configured; still report it.
    if inputs.configured_count == 0
        && inputs.overlay_lifecycle.is_empty()
        && !Path::new(&inputs.manifest).is_file()
    {
        out.push(Record::skip("no overlays to check", None));
        return out;
    } else if inputs.configured_count == 0 {
        out.push(Record::skip("no active overlay descriptors", None));
    }

    let mut active: BTreeMap<&str, &str> = BTreeMap::new();
    for entry in &inputs.active_records {
        active.insert(record_name(entry), entry.as_str());
    }
    // Status every active Git overlay that has its own `.git` up front, in
    // parallel. The `.git` probe keeps a missing or non-repository path from
    // resolving to an enclosing repository (an ordinary client rooted at
    // `$HOME` would answer for the whole home); results are used only after
    // the worktree check below confirms the checkout.
    let status_paths: Vec<String> = active
        .values()
        .map(|entry| read_fields(entry, 6))
        .filter(|fields| fields[5] != "none")
        .map(|fields| fields[1].clone())
        .filter(|path| std::fs::symlink_metadata(Path::new(path).join(".git")).is_ok())
        .collect();
    let statuses = overlay_statuses(status_paths);
    // Overlay paths by name, for the manifest symlink ownership
    // pass (`overlay_paths` / `overlay_syncs` in the shell).
    let mut overlay_paths: BTreeMap<String, String> = BTreeMap::new();
    let mut overlay_syncs: BTreeMap<String, String> = BTreeMap::new();
    for lifecycle in &inputs.overlay_lifecycle {
        let fields = read_fields(lifecycle, 3);
        let name = fields[0].clone();
        let state = fields[1].as_str();
        match state {
            "not-selected" => {
                out.push(Record::skip(format!("{name}: not selected"), None));
                continue;
            }
            "selected-ineligible" => {
                out.push(Record::skip(
                    format!("{name}: selected but host/platform ineligible"),
                    None,
                ));
                continue;
            }
            "selected-unsupported" => {
                // Every key that kept this overlay off, matched by file (the
                // descriptor names its overlay the legacy way, which can
                // differ from the profile-aware lifecycle name), or a bare
                // row if the records ever disagree.
                let keys: Vec<String> = inputs
                    .unknown_keys
                    .iter()
                    .filter(|key| {
                        matches!(key.effect, crate::unknown_keys::Effect::OverlaySkipped(_))
                            && key.path == fields[2]
                    })
                    .map(|key| data_key_detail(key, inputs.home))
                    .collect();
                out.push(
                    Record::warn(
                        format!("{name}: selected but skipped: unknown descriptor key"),
                        None,
                    )
                    .with_items(keys),
                );
                continue;
            }
            "selected-optional-unavailable" => {
                out.push(Record::skip(
                    format!("{name}: selected optional but unavailable"),
                    None,
                ));
                continue;
            }
            "selected-unavailable" => {
                // Its checkout (or local source) is missing or does not match
                // the descriptor; dot update clones a missing checkout.
                out.push(
                    Record::fail(format!("{name}: selected but unavailable"), None).with_hint(
                        format!(
                            "run dot update to set it up; if it stays unavailable, check its source against {}",
                            tilde(&fields[2], inputs.home)
                        ),
                    ),
                );
                continue;
            }
            "active" => {}
            _ => {
                out.push(Record::fail(
                    format!("{name}: unknown overlay lifecycle state"),
                    Some(state.to_string()),
                ));
                continue;
            }
        }
        let entry = match active.get(name.as_str()) {
            Some(entry) => *entry,
            None => {
                // Two known causes. An invalid descriptor makes discovery drop
                // every active record while earlier lifecycle lines stay, so
                // its own row above is the fix. Otherwise the lifecycle names
                // a descriptor the profile-aware way and its record the legacy
                // way (`overlays::overlay_name`), which differ only for a
                // `.local` descriptor without `sync=none`.
                let descriptor = &fields[2];
                let step = if inputs.discovery_error.is_some() {
                    "fix the invalid overlay descriptor reported above, then rerun dot doctor"
                        .to_string()
                } else if descriptor.ends_with(".local.conf") {
                    format!(
                        "{} is a .local descriptor without sync=none; set sync=none or drop .local from its name, then run dot update",
                        tilde(descriptor, inputs.home)
                    )
                } else {
                    "rerun dot doctor; if it persists, report it as a dot bug".to_string()
                };
                out.push(
                    Record::fail(format!("{name}: active lifecycle record missing"), None)
                        .with_hint(step),
                );
                continue;
            }
        };
        // `name|path|url|descriptor|optional|sync`: `read` parks a
        // seventh field in `sync`, so only an exact `git`/`none`
        // spelling selects those arms.
        let entry_fields = read_fields(entry, 6);
        let path = entry_fields[1].clone();
        let url = entry_fields[2].clone();
        let optional = entry_fields[4].clone();
        let mut sync = entry_fields[5].clone();
        if sync.is_empty() {
            sync = "git".to_string();
        }
        overlay_paths.insert(name.clone(), path.clone());
        overlay_syncs.insert(name.clone(), sync.clone());
        if sync == "none" {
            match (inputs.local_validate)(&path) {
                Ok(()) => {
                    out.push(Record::ok(
                        format!("{name}: local source available"),
                        Some(tilde(&path, inputs.home)),
                    ));
                }
                Err(reply) => {
                    let diagnostic = if reply.is_empty() {
                        format!("{path}/home")
                    } else {
                        reply
                    };
                    out.push(Record::fail(
                        format!("{name}: local source unavailable"),
                        Some(tilde(&diagnostic, inputs.home)),
                    ));
                }
            }
            continue;
        }
        if !crate::overlays::is_worktree(Path::new(&path)) {
            if optional == "true" {
                out.push(Record::skip(
                    name,
                    Some("optional overlay not cloned".to_string()),
                ));
                continue;
            }
            out.push(Record::fail(
                format!("{name}: not cloned"),
                Some(format!("expected at {}", tilde(&path, inputs.home))),
            ));
            continue;
        }
        // This overlay's rows, folded into one when every one passes.
        let start = out.len();
        out.push(Record::ok(
            format!("{name}: cloned"),
            Some(tilde(&path, inputs.home)),
        ));
        let expected = crate::overlays::effective_url(&url, inputs.home);
        match crate::overlays::origin_matches(Path::new(&path), &expected) {
            Ok(_) => {
                out.push(Record::ok(
                    format!("{name}: remote.origin.url matches conf"),
                    None,
                ));
            }
            Err(actual) => {
                // `dot update` refuses to pull or link an overlay whose
                // origin differs from its descriptor and exits 1, so the
                // drift is a failure here too.
                let adopt = crate::repos_pull_support::adopt_command(&path, &expected, &actual);
                out.push(Record::fail(
                    format!("{name}: remote URL drift"),
                    Some(format!(
                        "conf={expected} vs actual={actual}; verify the checkout, then adopt it with: {adopt}"
                    )),
                ));
            }
        }
        let status = statuses.get(&path);
        overlay_state_records(&name, &path, optional == "true", status, &mut out);
        // Only a clean overlay on its current upstream folds, so the branch
        // facts exist whenever the fold uses them.
        let branch = status
            .and_then(|status| status.as_ref().ok()?.as_ref())
            .and_then(|status| {
                Some(current_fact(
                    status.head.as_deref()?,
                    status.upstream.as_deref()?,
                ))
            })
            .unwrap_or_default();
        let rows = out.split_off(start);
        out.extend(fold_healthy(
            rows,
            &name,
            format!("{}, {branch}", tilde(&path, inputs.home)),
        ));
    }

    if held {
        // The links belong to the held generation, which the reading above
        // does not describe, so ownership cannot be judged against it.
        out.push(Record::skip(
            "overlay symlinks not checked while the overlay set is held",
            None,
        ));
    } else if Path::new(&inputs.manifest).is_file() {
        check_overlay_links(inputs, &overlay_paths, &overlay_syncs, &mut out);
    }
    out
}

/// The manifest symlink ownership pass of [`check_overlays`]:
/// every manifest record must still resolve to the owning
/// overlay's current link target. `overlay_paths`/`overlay_syncs`
/// hold the active overlays seen above; anything else (missing or
/// broken link, unknown owner, derivation failure, target drift)
/// counts one issue.
fn check_overlay_links(
    inputs: &OverlayInputs,
    overlay_paths: &BTreeMap<String, String>,
    overlay_syncs: &BTreeMap<String, String>,
    out: &mut Vec<Record>,
) {
    let content = std::fs::read(&inputs.manifest).unwrap_or_default();
    // `stream_lines` mirrors the shell `while read` loop: NULs
    // stripped, `\n`-split, final partial line kept.
    let mut issues: Vec<String> = Vec::new();
    let mut owners: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    for (index, line) in crate::repos_overlays::stream_lines(&content)
        .into_iter()
        .enumerate()
    {
        let Some(parsed) = crate::repos_overlays::parse_manifest_record(&line) else {
            issues.push(format!(
                "{} line {}: unreadable record",
                tilde(&inputs.manifest, inputs.home),
                index + 1
            ));
            continue;
        };
        // Three-column records carry the literal link target as
        // part of the authority contract; two-column records fall
        // back to the physical comparison below.
        let exact = line.split('\t').count() >= 3;
        owners.insert(parsed.rel, (parsed.owner, parsed.target, exact));
    }
    for (rel, (owner, expected_lexical, exact)) in &owners {
        let dst = format!("{}/{}", inputs.home, rel);
        let dst_path = Path::new(&dst);
        // Each issue names the link and why, so the row says which links
        // `dot update` will touch instead of only how many.
        let mut issue =
            |reason: &str| issues.push(format!("{} ({reason})", tilde(&dst, inputs.home)));
        let link_meta = std::fs::symlink_metadata(dst_path).ok();
        match link_meta {
            None => {
                issue("missing");
                continue;
            }
            Some(meta) if !meta.file_type().is_symlink() => {
                issue("not a symlink");
                continue;
            }
            Some(_) => {}
        }
        if !dst_path.exists() {
            issue("dangling");
            continue;
        }
        let (Some(path), Some(sync)) = (overlay_paths.get(owner), overlay_syncs.get(owner)) else {
            issue(&format!("owner {owner} is not active"));
            continue;
        };
        let actual_bytes = std::fs::read_link(dst_path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .unwrap_or_default();
        let actual = String::from_utf8_lossy(&actual_bytes).into_owned();
        let current = match crate::repos_overlays::record_link_target(
            rel,
            owner,
            path,
            Some(sync.as_str()),
        ) {
            Some(current) => current,
            None => {
                issue("target cannot be derived");
                continue;
            }
        };
        if *exact {
            if expected_lexical != &current || actual != current {
                issue("points elsewhere");
            }
            continue;
        }
        if actual == current {
            continue;
        }
        let expected = format!("{path}/home/{rel}");
        if !crate::doctor_paths::symlink_points_to(dst_path, Path::new(&expected)) {
            issue("points elsewhere");
        }
    }
    if issues.is_empty() {
        out.push(Record::ok("overlay symlinks healthy", None));
    } else {
        out.push(
            Record::warn(format!("{} overlay symlink issue(s)", issues.len()), None)
                .with_items(issues)
                .with_hint("run dot update to re-link"),
        );
    }
}

/// Control directory the standalone installer (`install.sh`) keeps beside
/// the stable release root: `<data>/cgraf78/dot -> .dot-standalone/current
/// -> releases/<version>-<platform>`, with `lock` held while it runs.
const STANDALONE_CONTROL: &str = ".dot-standalone";
/// The standalone installer's versioned release directory parent.
const STANDALONE_RELEASES: &str = "releases";
/// Archive ownership marker Shdeps writes into a `github:release` root it
/// installed (`.shdeps-release-layout` holding exactly `v1 archive`).
const SHDEPS_LAYOUT_FILE: &str = ".shdeps-release-layout";
/// The only marker content Shdeps accepts.
const SHDEPS_LAYOUT_CONTENT: &[u8] = b"v1 archive\n";
/// Infix of the sibling Shdeps parks the prior root under while it swaps
/// in a new archive (`<root>.shdeps-archive-backup-<pid>-<nanos>`); one
/// left behind means an install was interrupted.
const SHDEPS_BACKUP_INFIX: &str = ".shdeps-archive-backup-";

/// Inputs for [`check_install_layout`].
pub struct InstallInputs<'a> {
    /// `$HOME`, for display only.
    pub home: &'a str,
    /// The running engine's source root, physically resolved.
    pub source_real: &'a Path,
    /// Whether that root is a packaged release (`.dot-install.json`), not
    /// a checkout.
    pub release_root: bool,
    /// `${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot`, the root
    /// Shdeps upgrades, spelled as configured.
    pub managed_root: &'a Path,
    /// Whether Shdeps is the configured dependency provider (and so owns
    /// Dot's upgrade).
    pub shdeps: bool,
}

/// The standalone control directory owning `release`, when `release` (a
/// physical path) is one of the standalone installer's versioned releases.
fn standalone_control(release: &Path) -> Option<&Path> {
    let releases = release.parent()?;
    let control = releases.parent()?;
    (releases.file_name()? == STANDALONE_RELEASES && control.file_name()? == STANDALONE_CONTROL)
        .then_some(control)
}

/// Release-install layout health (stat-level, no processes): which
/// installer owns the Dot release Shdeps would upgrade, and whether that
/// owner can still upgrade it.
///
/// Under the Shdeps provider a managed root that links into the standalone
/// installer's control directory (`<root> -> .dot-standalone/current`)
/// warns: Shdeps adopts that layout in place on its next update of Dot
/// (Shdeps without adoption fails that update, which its own health check
/// reports). The installer's lock fails, because Shdeps refuses to adopt
/// while it exists, and an interrupted adoption (the root link parked as
/// `<root>.shdeps-parked-root`) warns until the next update finishes it.
/// Doctor stops at this one row; `shdeps health` owns the rest. The verdict
/// is anchored
/// on the managed root rather than the running binary, so a test harness
/// or development checkout running beside a healthy Shdeps install stays
/// quiet. Without a provider, the standalone installer is the upgrade
/// path. A Shdeps-installed root must carry the archive marker Shdeps
/// validates before it touches the root (a wrong marker fails; a missing
/// one warns because Shdeps backfills it when the public command proves
/// ownership). Leftover install state is reported too: the standalone
/// installer refuses to run while its lock exists, and an archive backup
/// sibling means a Shdeps install was interrupted. Checkouts report
/// nothing: their updates do not go through either installer.
///
/// A healthy layout files no row: its kind travels back in
/// [`InstallLayout::kind`] for the runtime's version row to name, so only
/// problems take rows of their own.
pub fn check_install_layout(inputs: &InstallInputs) -> InstallLayout {
    let mut layout = InstallLayout::default();
    let out = &mut layout.records;
    let managed_real = std::fs::canonicalize(inputs.managed_root).ok();
    let managed_standalone = installer_link(inputs.managed_root);
    let running_standalone = if inputs.release_root {
        standalone_control(inputs.source_real)
    } else {
        None
    };
    if inputs.shdeps {
        if let Some(control) = managed_standalone {
            out.push(Record::warn(
                "dot is standalone-installed",
                Some(format!(
                    "{}: Shdeps adopts it on its next update of dot; if this persists, run shdeps health",
                    tilde(&inputs.managed_root.to_string_lossy(), inputs.home)
                )),
            ));
            check_adoption_lock(inputs, &control, out);
            return layout;
        }
        // An adoption whose fallback switch was interrupted leaves no root,
        // only the installer's link parked beside it.
        let parked = parked_root_link(inputs.managed_root);
        let root_missing = std::fs::symlink_metadata(inputs.managed_root).is_err();
        if let Some(control) = installer_link(&parked).filter(|_| root_missing) {
            out.push(Record::warn(
                "Shdeps adoption of the standalone install was interrupted",
                Some(format!(
                    "{}: the next Shdeps update of dot finishes it",
                    tilde(&parked.to_string_lossy(), inputs.home)
                )),
            ));
            check_adoption_lock(inputs, &control, out);
            return layout;
        }
    } else if let Some(control) = managed_standalone.or(running_standalone.map(Path::to_path_buf)) {
        layout.kind = Some(STANDALONE_KIND);
        // A warning, not a failure: without a provider the lock blocks only a
        // manual `install.sh` rerun (never `dot update`), and it is
        // legitimately present while an installer runs.
        let lock = control.join("lock");
        if std::fs::symlink_metadata(&lock).is_ok() {
            out.push(Record::warn(
                "standalone installer lock is present",
                Some(format!(
                    "{}: install.sh refuses to run while it exists; remove it if no installer is running",
                    tilde(&lock.to_string_lossy(), inputs.home)
                )),
            ));
        }
        return layout;
    }
    let managed_dir =
        std::fs::symlink_metadata(inputs.managed_root).is_ok_and(|meta| meta.file_type().is_dir());
    let running_managed = managed_real.as_deref() == Some(inputs.source_real);
    if !inputs.shdeps {
        return layout;
    }
    if inputs.release_root && managed_dir && running_managed && check_layout_marker(inputs, out) {
        layout.kind = Some(SHDEPS_KIND);
    }
    // Leftovers are reported whichever Dot runs: a failed swap whose rollback
    // also failed leaves only the backup, with no root to run from.
    check_install_leftovers(inputs, managed_dir, out);
    layout
}

/// [`InstallLayout::kind`] of a release the standalone installer owns.
pub const STANDALONE_KIND: &str = "standalone install; rerun install.sh to upgrade";
/// [`InstallLayout::kind`] of a release Shdeps owns and can upgrade.
pub const SHDEPS_KIND: &str = "Shdeps release";

/// What [`check_install_layout`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallLayout {
    /// How the running release is installed, when its owner can upgrade it
    /// (see [`STANDALONE_KIND`] and [`SHDEPS_KIND`]); `None` when the layout
    /// itself is a problem row. Leftovers beside a healthy layout (an
    /// installer lock, an archive backup) keep their own rows and the kind.
    pub kind: Option<&'static str>,
    /// Rows for every layout problem and leftover found.
    pub records: Vec<Record>,
}

/// `<root>.shdeps-parked-root`: where Shdeps parks the installer's root
/// link while a fallback (non-atomic) adoption switch runs, mirroring
/// Shdeps' `parked_root_link`. A link left there means the switch was
/// interrupted; Shdeps finishes it on its next update of the dependency.
fn parked_root_link(root: &Path) -> PathBuf {
    let mut name = root
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".shdeps-parked-root");
    root.with_file_name(name)
}

/// The installer's control directory when `link` is the standalone
/// installer's own root link: a symlink whose target is exactly
/// `.dot-standalone/current`. This is the same no-follow evidence Shdeps
/// requires before it adopts the layout, so doctor never promises an
/// adoption Shdeps would refuse (a link straight into `releases/`, or an
/// absolute target, is someone else's).
fn installer_link(link: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(link).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::read_link(link).ok()?;
    (target == Path::new(STANDALONE_CONTROL).join("current"))
        .then(|| link.with_file_name(STANDALONE_CONTROL))
}

/// The standalone installer's lock under the Shdeps provider: Shdeps checks
/// it before anything else in the layout and refuses to adopt (or finish
/// adopting) the install while any entry exists there, so its updates of dot
/// fail until it is gone.
fn check_adoption_lock(inputs: &InstallInputs, control: &Path, out: &mut Vec<Record>) {
    let lock = control.join("lock");
    if std::fs::symlink_metadata(&lock).is_ok() {
        let shown = tilde(&lock.to_string_lossy(), inputs.home);
        out.push(Record::fail(
            "standalone installer lock blocks Shdeps adoption",
            Some(format!(
                "{shown}: Shdeps will not adopt the install while it exists; remove it if no install.sh is running"
            )),
        ));
    }
}

/// The Shdeps archive marker of the managed release root Dot runs from:
/// `true` when it is valid, otherwise a problem row is filed.
fn check_layout_marker(inputs: &InstallInputs, out: &mut Vec<Record>) -> bool {
    let marker = inputs.managed_root.join(SHDEPS_LAYOUT_FILE);
    let marker_shown = tilde(&marker.to_string_lossy(), inputs.home);
    match std::fs::symlink_metadata(&marker) {
        Ok(meta) if meta.file_type().is_file() => {
            if std::fs::read(&marker).is_ok_and(|content| content == SHDEPS_LAYOUT_CONTENT) {
                return true;
            }
            out.push(Record::fail(
                "dot release layout marker is invalid",
                Some(format!(
                    "{marker_shown}: Shdeps refuses to upgrade until it holds 'v1 archive'"
                )),
            ));
        }
        Ok(_) => out.push(Record::fail(
            "dot release layout marker is invalid",
            Some(format!(
                "{marker_shown} is not a regular file; Shdeps refuses to upgrade until it is"
            )),
        )),
        Err(_) => out.push(Record::warn(
            "dot release has no Shdeps layout marker",
            Some(format!(
                "{marker_shown}: Shdeps records it when the public command proves ownership and otherwise refuses to upgrade"
            )),
        )),
    }
    false
}

/// Install state left behind beside the managed root: an interrupted
/// Shdeps archive swap's backup sibling and the standalone installer's lock.
fn check_install_leftovers(inputs: &InstallInputs, managed_dir: bool, out: &mut Vec<Record>) {
    if let (Some(parent), Some(name)) = (
        inputs.managed_root.parent(),
        inputs.managed_root.file_name(),
    ) {
        let prefix = format!("{}{SHDEPS_BACKUP_INFIX}", name.to_string_lossy());
        let mut backups: Vec<String> = std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let entry_name = entry.file_name().to_string_lossy().into_owned();
                entry_name
                    .strip_prefix('.')
                    .unwrap_or(&entry_name)
                    .starts_with(&prefix)
                    .then(|| tilde(&entry.path().to_string_lossy(), inputs.home))
            })
            .collect();
        backups.sort();
        if !backups.is_empty() {
            let next = if managed_dir {
                "remove it once dot runs from the current release"
            } else {
                "the install root is missing; run dot update to reinstall it"
            };
            out.push(
                Record::warn("interrupted Shdeps install left a backup", None)
                    .with_items(backups)
                    .with_hint(next),
            );
        }
        let lock = parent.join(STANDALONE_CONTROL).join("lock");
        if std::fs::symlink_metadata(&lock).is_ok() {
            out.push(Record::warn(
                "leftover standalone installer lock",
                Some(format!(
                    "{}: no longer used by this Shdeps install; remove it",
                    tilde(&lock.to_string_lossy(), inputs.home)
                )),
            ));
        }
    }
}

/// `_dr_shdeps_binary` (`doctor/provider.sh`): resolve the
/// provider binary. A pre-selected executable `_SHDEPSW_BIN` wins;
/// otherwise the installer selects `shdeps` next to itself
/// (`${installer%/*}`: the text before the last `/`, or the whole
/// string when there is no slash), then the debug and release
/// build trees. Candidates must be plain executable files
/// (`-f && ! -L && -x`); the pre-selected path needs only `-x`,
/// like the shell. Returns the selected path, or `None` for the
/// shell `return 1`.
pub fn shdeps_binary(shdepsw_bin: Option<&Path>, installer: &Path) -> Option<PathBuf> {
    if let Some(selected) = shdepsw_bin {
        let executable = std::fs::symlink_metadata(selected)
            .ok()
            .as_ref()
            .is_some_and(|meta| is_executable_bits(meta.permissions().mode()));
        if executable {
            return Some(selected.to_path_buf());
        }
    }
    // `${installer%/*}` in bytes: strip from the last `/`, or
    // keep the whole spelling when there is none.
    let raw = installer.as_os_str().as_bytes();
    let root: Vec<u8> = match raw.iter().rposition(|byte| *byte == b'/') {
        Some(index) => raw[..index].to_vec(),
        None => raw.to_vec(),
    };
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    for suffix in ["shdeps", "target/debug/shdeps", "target/release/shdeps"] {
        let mut candidate = root.clone();
        if !candidate.is_empty() {
            candidate.push(b'/');
        }
        candidate.extend_from_slice(suffix.as_bytes());
        candidates.push(candidate);
    }
    for candidate in &candidates {
        let path = Path::new(std::ffi::OsStr::from_bytes(candidate));
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        // `[[ -f && ! -L && -x ]]`: `file_type().is_file()`
        // follows the `-f`/`-L` split (a symlink to a file fails
        // the `-L` arm).
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            continue;
        }
        if is_executable_bits(meta.permissions().mode()) {
            return Some(path.to_path_buf());
        }
    }
    None
}

/// The reviewed installer selection for [`check_provider`],
/// mirroring `_dot_shdeps_installer`'s `REPLY` plus
/// `_DOT_SHDEPS_INSTALLER_SOURCE`.
pub struct ProviderInstaller<'a> {
    /// Installer path (`REPLY`).
    pub path: &'a str,
    /// `_DOT_SHDEPS_INSTALLER_SOURCE`: `explicit`, `pinned-dev`,
    /// `latest-dev`, or `managed` (anything else reports the
    /// source unavailable, like the shell `*` arm).
    pub source: &'a str,
}

/// Inputs for [`check_provider`]: the provider globals plus every
/// shdeps helper outcome as explicit data.
pub struct ProviderInputs<'a> {
    /// `$HOME`, for `_dr_tilde` display only.
    pub home: &'a str,
    /// `DOT_DEPENDENCY_PROVIDER`: unset or empty selects `none`
    /// (like `${var:-none}`); `shdeps` proceeds; anything else is
    /// unsupported.
    pub dependency_provider: Option<&'a str>,
    /// `DOT_SHDEPS_UPDATE_POLICY` (empty selects `pinned`, like
    /// `${var:-pinned}`).
    pub policy: &'a str,
    /// `_dot_shdeps_configure_env` exit status.
    pub configure_ok: bool,
    /// `SHDEPS_GIT_DEV_DIR` (empty when unset: the development
    /// checkout then spells `/shdeps`).
    pub dev_dir: &'a str,
    /// `-e`/`-L` on the development checkout.
    pub development_exists: bool,
    /// `_dot_shdeps_development_checkout_valid` exit status
    /// (consulted only under the `latest` policy with an existing
    /// checkout, like the shell short-circuit).
    pub development_valid: bool,
    /// `_dot_shdeps_installer` result (`None` when it fails).
    pub installer: Option<ProviderInstaller<'a>>,
    /// `_dot_shdeps_lock_value revision` (failures read empty).
    /// Consulted only for `latest` plus a development source.
    pub locked_revision: Option<&'a str>,
    /// `git -C "$development" rev-parse HEAD` (failures read
    /// empty). Same gating as `locked_revision`.
    pub development_revision: Option<&'a str>,
    /// `_dr_shdeps_binary "$installer"` result: binary resolution
    /// stays a caller concern (see [`shdeps_binary`]); `None`
    /// reports the binary unavailable.
    pub binary: Option<&'a str>,
    /// `_dot_shdeps_lock_value abi` (failures read empty, which
    /// reports `<missing>`).
    pub expected_abi: Option<&'a str>,
    /// `_dot_shdeps_binary_abi_version` `REPLY` (`None` when the
    /// probe fails, which reports `<unavailable>`).
    pub actual_abi: Option<&'a str>,
    /// Whether the provider accepts the behavioral cancellation capability
    /// required by the native update coordinator.
    pub cancellation_capability: bool,
    /// Whether prompt FIFO readers are installed before prompt events.
    pub prompt_handshake_capability: bool,
}

/// The provider's update policy, source, and development revision as one
/// informational row (they used to take three, one of them a permanent
/// "differs from Dot lock" with a full SHA under the latest policy).
/// `development` is the development checkout path.
fn provider_record(inputs: &ProviderInputs, policy: &str, development: &str) -> Record {
    let mut facts = vec![format!("{policy} policy")];
    let latest = policy == "latest";
    let source = inputs.installer.as_ref().map(|installer| installer.source);
    match (&inputs.installer, source) {
        (Some(installer), Some("explicit")) => facts.push(format!(
            "caller-selected reviewed installer {}",
            tilde(installer.path, inputs.home)
        )),
        (_, Some("pinned-dev")) if latest => facts.push(format!(
            "development checkout {} selected by Dot lock",
            tilde(development, inputs.home)
        )),
        (_, Some("latest-dev")) => facts.push(format!(
            "trusted development checkout {}",
            tilde(development, inputs.home)
        )),
        (_, Some("managed")) if latest => {
            facts.push("managed release via reviewed bootstrap".to_string())
        }
        _ => {}
    }
    if latest && matches!(source, Some("pinned-dev" | "latest-dev")) {
        let locked = inputs.locked_revision.unwrap_or("");
        let current = inputs.development_revision.unwrap_or("");
        let short: String = current.chars().take(12).collect();
        if !locked.is_empty() && current == locked {
            facts.push(format!("revision {short} (matches Dot lock)"));
        } else if current.is_empty() {
            facts.push("revision <unavailable>".to_string());
        } else {
            // Expected under the latest policy, so no full SHA: the
            // checkout follows its branch, not the lock.
            facts.push(format!("unpinned revision {short}"));
        }
    }
    Record::info("Shdeps provider", Some(facts.join("; ")))
}

/// `_dr_check_provider` (`doctor/provider.sh`): the dependency
/// provider boundary (reviewed installer selection plus ABI
/// agreement). Helper outcomes arrive via [`ProviderInputs`];
/// only the `_dr_tilde` display runs in-process. The update policy,
/// provider source, and development revision are configuration facts, so
/// they render as one informational row that is never counted (see
/// `provider_record`).
pub fn check_provider(inputs: &ProviderInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Dependency provider")];
    match inputs.dependency_provider {
        None | Some("") => {
            out.push(Record::skip("no dependency provider configured", None));
            return out;
        }
        Some("shdeps") => {}
        Some(other) => {
            out.push(Record::fail(
                "dependency provider is unsupported",
                Some(other.to_string()),
            ));
            return out;
        }
    }
    let policy = if inputs.policy.is_empty() {
        "pinned"
    } else {
        inputs.policy
    };
    let development = format!("{}/shdeps", inputs.dev_dir);
    // The policy, source, and revision are configuration facts: one
    // informational row, whatever follows.
    out.push(provider_record(inputs, policy, &development));
    if !inputs.configure_ok {
        out.push(Record::fail(
            "Shdeps provider is unavailable",
            Some("run dot update to bootstrap the reviewed provider release".to_string()),
        ));
        return out;
    }
    let mut development_invalid = false;
    if policy == "latest" && inputs.development_exists && !inputs.development_valid {
        development_invalid = true;
    }
    let installer = match &inputs.installer {
        Some(installer) => installer,
        None => {
            if development_invalid {
                out.push(Record::warn(
                    "Shdeps development checkout ignored",
                    Some(format!(
                        "verify its owner, modes, Git root, and cgraf78/shdeps origin: {}",
                        tilde(&development, inputs.home)
                    )),
                ));
            }
            out.push(Record::fail(
                "Shdeps provider is unavailable",
                Some("run dot update to bootstrap the reviewed provider release".to_string()),
            ));
            return out;
        }
    };
    if development_invalid && installer.source == "managed" {
        out.push(Record::warn(
            "Shdeps development checkout ignored",
            Some(format!(
                "verify its owner, modes, Git root, and cgraf78/shdeps origin: {}",
                tilde(&development, inputs.home)
            )),
        ));
    }
    match installer.source {
        "explicit" | "pinned-dev" | "managed" => {
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        "latest-dev" => {}
        _ => {
            out.push(Record::fail(
                "Shdeps provider source is unavailable",
                Some("run dot update to restore provider selection metadata".to_string()),
            ));
            return out;
        }
    }
    if inputs.binary.is_none() {
        out.push(Record::fail(
            "Shdeps provider binary is unavailable",
            Some("run dot update to complete provider installation".to_string()),
        ));
        return out;
    }
    let expected = inputs.expected_abi.unwrap_or("");
    let actual = inputs.actual_abi.unwrap_or("");
    let abi_matches = !expected.is_empty() && actual == format!("abi:{expected}");
    if abi_matches {
        out.push(Record::ok("Shdeps provider ABI", Some(actual.to_string())));
    } else {
        let want = if expected.is_empty() {
            "<missing>"
        } else {
            expected
        };
        let found = if actual.is_empty() {
            "<unavailable>"
        } else {
            actual
        };
        out.push(Record::fail(
            "Shdeps provider ABI mismatch",
            Some(format!("expected abi:{want}, found {found}")),
        ));
    }
    // Preserve the healthy shell-era report shape: the capability is a
    // required predicate rather than another informational success row. A
    // missing predicate must nevertheless prevent doctor from blessing a
    // provider that `dot update` will reject.
    if abi_matches && !inputs.cancellation_capability {
        out.push(Record::fail(
            "Shdeps provider cancellation capability is unavailable",
            Some("run dot update to install the reviewed provider release".to_string()),
        ));
    }
    if abi_matches && !inputs.prompt_handshake_capability {
        out.push(Record::fail(
            "Shdeps provider prompt handshake capability is unavailable",
            Some("run dot update to install the reviewed provider release".to_string()),
        ));
    }
    out
}

/// `_dr_completed_identity_matches_home` (`doctor/repos.sh`):
/// whether the init-completed marker names this `$HOME` as both
/// worktree and git dir. `marker` is the resolved
/// `dot/init/completed` path (`None` when `dot_xdg_path` fails);
/// the marker must be a plain file (`-f && ! -L`) whose
/// `git_dir=`/`worktree=` lines (last wins, like the shell loop)
/// equal `$HOME/.git` and `$HOME`.
pub fn completed_identity_matches_home(marker: Option<&Path>, home: &str) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    let plain = std::fs::symlink_metadata(marker)
        .ok()
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_file() && !meta.file_type().is_symlink());
    if !plain {
        return false;
    }
    let content = std::fs::read(marker).unwrap_or_default();
    let mut git_dir = String::new();
    let mut worktree = String::new();
    for line in crate::repos_overlays::stream_lines(&content) {
        if let Some(value) = line.strip_prefix("git_dir=") {
            git_dir = value.to_string();
        } else if let Some(value) = line.strip_prefix("worktree=") {
            worktree = value.to_string();
        }
    }
    worktree == home && git_dir == format!("{home}/.git")
}

/// Run `git` for [`is_client_checkout`] with plain `git -C`:
/// stdout captured with `$(...)` newline stripping, stderr nulled,
/// stdin null (the [`crate::repos_base::run_git`] engine
/// boundary). `None` on spawn failure or non-zero exit.
fn git_capture(home: &Path, args: &[&str]) -> Option<String> {
    let full = [OsString::from("-C"), home.as_os_str().to_os_string()];
    let output = crate::repos_base::run_git(&full, args)?;
    if !output.status.success() {
        return None;
    }
    Some(captured(&String::from_utf8_lossy(&output.stdout)))
}

/// `_dr_is_client_checkout` (`doctor/repos.sh`): whether `$HOME`
/// is an ordinary checkout rooted at itself: `git -C HOME`
/// top-level resolves to the physical `$HOME`, and either the
/// init-completed identity matches or the local
/// `dot.clientRepository` flag reads `true`. `marker` is the
/// resolved `dot/init/completed` path (see
/// [`completed_identity_matches_home`]).
pub fn is_client_checkout(home: &Path, marker: Option<&Path>) -> bool {
    let root = match git_capture(home, &["rev-parse", "--show-toplevel"]) {
        Some(root) => root,
        None => return false,
    };
    let home_real = match std::fs::canonicalize(home) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let root_real = match std::fs::canonicalize(&root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if root_real != home_real {
        return false;
    }
    if completed_identity_matches_home(marker, &home.to_string_lossy()) {
        return true;
    }
    match git_capture(
        home,
        &["config", "--local", "--get", "dot.clientRepository"],
    ) {
        Some(value) => value == "true",
        None => false,
    }
}

/// Inputs for [`check_base_repo`]: the repository selector state
/// plus the client-checkout verdict as explicit data.
pub struct BaseRepoInputs<'a> {
    /// `DOT_BASE_TOPOLOGY`: `missing`, `separate`, `ordinary`, or
    /// (unrecognized, which the shell treats as existing with a
    /// failing `_base_git`, exit 128).
    pub topology: &'a str,
    /// `DOT_CLIENT_GIT_DIR` display path (separate topology only,
    /// but always carried like the shell global).
    pub client_git_dir: &'a str,
    /// `$HOME`: work tree, identity anchor, and tilde base.
    pub home: &'a str,
    /// `_dr_is_client_checkout` verdict (reused, not recomputed).
    pub is_client_checkout: bool,
}

/// The `_base_git` argv prefix for [`check_base_repo`]: `separate`
/// pins `--git-dir`/`--work-tree`, `ordinary` pins `-C $HOME`,
/// and anything else fails every `git` call (shell exit 128).
fn base_git_prefix(topology: &str, client_git_dir: &str, home: &str) -> Option<Vec<OsString>> {
    match topology {
        "separate" => Some(vec![
            OsString::from(format!("--git-dir={client_git_dir}")),
            OsString::from(format!("--work-tree={home}")),
        ]),
        "ordinary" => Some(vec![OsString::from("-C"), OsString::from(home)]),
        _ => None,
    }
}

/// One `_base_git` capture for [`check_base_repo`]: `None` on
/// spawn failure, non-zero exit, or unrecognized topology (the
/// shell `|| true` / `|| printf false` fallbacks apply at each
/// call site, not here).
fn base_git(
    topology: &str,
    client_git_dir: &str,
    home: &str,
    args: &[&str],
) -> Result<Option<String>, TimedOut> {
    let Some(prefix) = base_git_prefix(topology, client_git_dir, home) else {
        return Ok(None);
    };
    let Some(output) = inspect_git(&prefix, args, probe_deadline())? else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(captured(&String::from_utf8_lossy(&output.stdout))))
}

/// Run one read-only repository inspection for doctor: the bound host Git,
/// `prefix` then `args`, stdout captured, stderr and stdin closed. Inherited
/// repository selectors (`GIT_INDEX_FILE` from a Git hook, say) are removed
/// so the probe inspects the repository it names, while the user's Git
/// configuration still applies. `Ok(None)` means Git could not be run.
fn inspect_git(
    prefix: &[OsString],
    args: &[&str],
    deadline: Option<std::time::Instant>,
) -> Result<Option<std::process::Output>, TimedOut> {
    let mut command = crate::init_client_identity::host_git_command();
    crate::temp::scrub_repository_selectors(&mut command);
    command
        .args(prefix)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    match crate::cleanup::run_session_output_typed(
        command,
        deadline,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    ) {
        Ok(output) => Ok(Some(output)),
        Err(crate::cleanup::SessionOutputError::TimedOut) => Err(TimedOut),
        Err(_) => Ok(None),
    }
}

/// The single status invocation doctor spends per repository. It answers
/// branch, upstream distance, and tracked changes at once (one process
/// instead of four through a possibly slow `git` wrapper).
/// `--untracked-files=no` is explicit: the base client's work tree is all of
/// `$HOME`, and a host whose repository config lacks
/// `status.showUntrackedFiles=no` would otherwise scan it.
/// `--no-optional-locks` (a global option, so it precedes the subcommand)
/// keeps status from taking `index.lock` to write its refresh: doctor is
/// read-only, and a concurrent cron `dot update` must not lose the lock to
/// it.
pub const STATUS_ARGS: [&str; 5] = [
    "--no-optional-locks",
    "status",
    "--porcelain=v2",
    "--branch",
    "--untracked-files=no",
];

/// One repository's state from one [`STATUS_ARGS`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoStatus {
    /// `# branch.oid`: the HEAD commit, or `None` on an unborn branch.
    pub oid: Option<String>,
    /// `# branch.head`: the branch, or `None` when HEAD is detached.
    pub head: Option<String>,
    /// `# branch.upstream`, when one is configured.
    pub upstream: Option<String>,
    /// `# branch.ab` as (ahead, behind); Git prints it only when the
    /// upstream resolves.
    pub ahead_behind: Option<(u64, u64)>,
    /// Changed tracked entries (`1` and `2` lines).
    pub changed: usize,
    /// Unmerged entries (`u` lines): every pull refuses to rebase over them.
    pub unmerged: usize,
}

/// Parse `git status --porcelain=v2 --branch` output. Paths are quoted by
/// Git when they hold a newline, so every entry is one line; unknown header
/// and entry kinds (a newer Git) are ignored.
pub fn parse_status_v2(text: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    for line in text.lines() {
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            status.oid = (oid != "(initial)").then(|| oid.to_string());
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            status.head = (head != "(detached)").then(|| head.to_string());
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            status.upstream = Some(upstream.to_string());
        } else if let Some(counts) = line.strip_prefix("# branch.ab ") {
            let mut parts = counts.split(' ');
            let ahead = parts.next().and_then(|part| part.strip_prefix('+'));
            let behind = parts.next().and_then(|part| part.strip_prefix('-'));
            if let (Some(Ok(ahead)), Some(Ok(behind))) =
                (ahead.map(str::parse), behind.map(str::parse))
            {
                status.ahead_behind = Some((ahead, behind));
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            status.changed += 1;
        } else if line.starts_with("u ") {
            status.unmerged += 1;
        }
    }
    status
}

/// The row for unmerged entries, in `dot update`'s severity. Update fails
/// only on a branch with an upstream and outside a merge, rebase, or `am`
/// session ([`crate::repos_pull::check_index`] decides, so doctor and update
/// share one rule); inside a session, or without a branch and upstream to
/// pull, it skips the repository and so do we (a warning). `None` when the
/// entries vanished since the status ran.
fn unmerged_record(message: String, prefix: &[OsString], pullable: bool) -> Option<Record> {
    if !pullable {
        return Some(Record::warn(
            message,
            Some(
                "resolve them; without a branch and upstream to pull, dot update skips this repository"
                    .to_string(),
            ),
        ));
    }
    match crate::repos_pull::check_index(prefix) {
        crate::repos_pull::IndexCheck::Clean => None,
        crate::repos_pull::IndexCheck::Session(_) => Some(Record::warn(
            message,
            Some("a merge or rebase is in progress; dot update skips this repository until it is finished".to_string()),
        )),
        crate::repos_pull::IndexCheck::Unmerged(_) => Some(Record::fail(
            message,
            Some("dot update fails until the conflict is resolved; run dot status".to_string()),
        )),
    }
}

/// How a branch relates to its upstream when it is not current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Distance {
    /// Only upstream has new commits.
    Behind,
    /// Only the branch has new commits.
    Ahead,
    /// Both have.
    Diverged,
}

/// The [`Distance`] and its detail (`origin/main: 2 commit(s) behind`), or
/// `None` when current. Callers word the message in their own row style.
fn distance(upstream: &str, ahead: u64, behind: u64) -> Option<(Distance, String)> {
    match (ahead, behind) {
        (0, 0) => None,
        (0, behind) => Some((
            Distance::Behind,
            format!("{upstream}: {behind} commit(s) behind"),
        )),
        (ahead, 0) => Some((
            Distance::Ahead,
            format!("{upstream}: {ahead} commit(s) ahead"),
        )),
        (ahead, behind) => Some((
            Distance::Diverged,
            format!("{upstream}: {ahead} ahead, {behind} behind"),
        )),
    }
}

/// The per-worktree Git directory of the checkout at `path`: `.git` itself,
/// or the target a linked worktree's `.git` file names. Stat-level.
fn worktree_git_dir(path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.strip_prefix("gitdir: ")?.trim_end();
    Some(path.join(target))
}

/// The frozen-rebase failure row: `dot update` refuses to retry a rebase
/// that conflicted (or failed twice) while the same HEAD and upstream tip
/// stay put. Doctor matches the recorded HEAD against `branch.oid`, which
/// costs no process; an upstream that moved since makes update retry, which
/// the next update shows.
fn frozen_rebase_record(
    message: impl Fn(&str) -> String,
    git_dir: Option<&Path>,
    status: &RepoStatus,
    optional: bool,
) -> Option<Record> {
    // Update consults the marker only on a branch with an upstream; a
    // detached HEAD or a missing upstream is skipped (and reported) first.
    let upstream = status
        .upstream
        .as_deref()
        .filter(|_| status.head.is_some())?;
    let head = crate::repos_pull::frozen_rebase_head(git_dir?)?;
    if status.oid.as_deref() != Some(head.as_str()) {
        return None;
    }
    Some(if optional {
        // An optional overlay's frozen pull leaves it empty for that run
        // without failing the update.
        Record::warn(
            message(upstream),
            Some("dot update skips this optional overlay until you rebase by hand".to_string()),
        )
    } else {
        Record::fail(
            message(upstream),
            Some(
                "dot update refuses to retry it until you rebase by hand or either side moves"
                    .to_string(),
            ),
        )
    })
}

/// `_dr_check_base_repo` (`doctor/repos.sh`): client repository
/// health (layout identity, worktree resolution, tracked dirt,
/// HEAD, upstream distance). `git` runs in-process through the
/// mirrored `_base_git` dispatch; only
/// [`BaseRepoInputs::is_client_checkout`] is injected.
pub fn check_base_repo(inputs: &BaseRepoInputs) -> Vec<Record> {
    let mut out = vec![Record::section("Client repository")];
    // `_base_repo_exists`: any topology but `missing`.
    if inputs.topology == "missing" {
        if inputs.is_client_checkout {
            out.push(Record::ok(
                "client checkout exists",
                Some("ordinary checkout rooted at $HOME".to_string()),
            ));
        } else {
            out.push(Record::fail(
                "client repository is missing",
                Some("run dot init REPOSITORY_URL".to_string()),
            ));
        }
        return out;
    }
    out.push(Record::ok(
        "client Git directory exists",
        Some(tilde(inputs.client_git_dir, inputs.home)),
    ));
    // What the folded healthy row says about the layout.
    let mut layout_fact = "ordinary layout".to_string();
    let git = |args: &[&str]| base_git(inputs.topology, inputs.client_git_dir, inputs.home, args);
    // One stalled probe ends the section with a warning: the rows after it
    // would only restate that Git did not answer, and wrongly as failures.
    let stalled = |mut out: Vec<Record>| {
        out.push(Record::warn(
            "client repository did not answer",
            Some(timed_out_detail()),
        ));
        out
    };
    if inputs.topology == "ordinary" {
        out.push(Record::ok("ordinary client layout", None));
    } else {
        // Both layout keys in one process; `--get` semantics (last value
        // wins) and the old defaults (`false`, empty) when unset.
        let (mut is_bare, mut has_worktree) = ("false".to_string(), String::new());
        let layout = match git(&["config", "-z", "--get-regexp", r"^core\.(bare|worktree)$"]) {
            Ok(layout) => layout.unwrap_or_default(),
            Err(TimedOut) => return stalled(out),
        };
        for entry in layout.split('\0') {
            match entry.split_once('\n') {
                Some(("core.bare", value)) => is_bare = value.to_string(),
                Some(("core.worktree", value)) => has_worktree = value.to_string(),
                _ => {}
            }
        }
        if is_bare == "true" {
            layout_fact = "legacy bare layout".to_string();
            out.push(Record::ok("legacy bare client layout", None));
        } else if !has_worktree.is_empty() {
            layout_fact = format!("worktree {}", tilde(&has_worktree, inputs.home));
            out.push(Record::ok(
                "explicit-worktree client layout",
                Some(tilde(&has_worktree, inputs.home)),
            ));
        } else {
            // Neither bare nor bound to a worktree: `dot init` records the
            // home directory as `core.worktree`, which this restores.
            let quote = |text: &str| crate::repos_pull_support::shell_quote(text.as_bytes());
            out.push(
                Record::fail("client Git directory has no worktree identity", None).with_hint(
                    format!(
                        "restore it with: git --git-dir={} config core.worktree {}",
                        quote(inputs.client_git_dir),
                        quote(inputs.home)
                    ),
                ),
            );
        }
    }
    let resolved = match git(&["rev-parse", "--show-toplevel"]) {
        Ok(resolved) => resolved.unwrap_or_default(),
        Err(TimedOut) => return stalled(out),
    };
    if resolved == inputs.home {
        out.push(Record::ok("client worktree resolves to $HOME", None));
    } else {
        let got = if resolved.is_empty() {
            "<missing>"
        } else {
            resolved.as_str()
        };
        out.push(Record::fail(
            "client worktree mismatch",
            Some(format!("expected {}, got {got}", inputs.home)),
        ));
    }
    let status = match git(&STATUS_ARGS) {
        Ok(status) => status,
        Err(TimedOut) => return stalled(out),
    };
    let Some(status) = status.map(|text| parse_status_v2(&text)) else {
        out.push(Record::warn(
            "client repository status is unavailable",
            Some("run dot status to inspect".to_string()),
        ));
        return out;
    };
    let prefix =
        base_git_prefix(inputs.topology, inputs.client_git_dir, inputs.home).unwrap_or_default();
    let git_dir = if inputs.topology == "separate" {
        Some(PathBuf::from(inputs.client_git_dir))
    } else {
        worktree_git_dir(Path::new(inputs.home))
    };
    let interrupted = interruption(&status, git_dir.as_deref());
    // Inside a session the headless row below speaks for the unmerged
    // entries too, in update's severity.
    if status.unmerged > 0 && interrupted.is_none() {
        let pullable = status.head.is_some() && status.upstream.is_some();
        out.extend(unmerged_record(
            format!("{} unmerged client path(s)", status.unmerged),
            &prefix,
            pullable,
        ));
    }
    out.extend(frozen_rebase_record(
        |upstream| format!("the last client rebase onto {upstream} conflicted; rebase manually"),
        git_dir.as_deref(),
        &status,
        false,
    ));
    // Unmerged entries are tracked changes too, as the old porcelain-v1
    // count had it.
    let tracked = status.changed + status.unmerged;
    if tracked == 0 {
        out.push(Record::ok("no tracked client changes", None));
    } else {
        out.push(Record::warn(
            format!("{tracked} tracked client change(s)"),
            Some("run dot status to inspect".to_string()),
        ));
    }
    let Some(head) = status.head.as_deref() else {
        out.push(headless_record(
            &Subject::client(&prefix),
            interrupted,
            tracked == 0,
        ));
        return out;
    };
    out.push(Record::ok("client HEAD on branch", Some(head.to_string())));
    let Some(upstream) = status.upstream.as_deref() else {
        out.push(no_upstream_record(&Subject::client(&prefix), head));
        return out;
    };
    match status.ahead_behind {
        Some((ahead, behind)) => match distance(upstream, ahead, behind) {
            None => out.push(Record::ok(
                "client upstream",
                Some(format!("{upstream} (current)")),
            )),
            Some((kind, detail)) => {
                // The client keeps its established wording.
                let message = match kind {
                    Distance::Behind => "client is behind upstream",
                    Distance::Ahead => "client is ahead of upstream",
                    Distance::Diverged => "client upstream has diverged",
                };
                out.push(Record::warn(message, Some(detail)));
            }
        },
        None => out.push(gone_upstream_record(&Subject::client(&prefix), upstream)),
    }
    let healthy = format!(
        "{}, {layout_fact}, {}",
        tilde(inputs.client_git_dir, inputs.home),
        current_fact(head, upstream)
    );
    fold_healthy(out, "client repository", healthy)
}

/// Branch, upstream, and tracked-change rows for one cloned overlay, in the
/// severities `dot update` gives them: unmerged entries fail where update
/// refuses to pull over them (see [`unmerged_record`]), while tracked
/// changes, a detached HEAD, a missing or gone upstream (update skips
/// pulling that overlay), and upstream distance warn. A clean overlay on
/// its current upstream reads as one row.
fn overlay_state_records(
    name: &str,
    path: &str,
    optional: bool,
    status: Option<&OverlayStatus>,
    out: &mut Vec<Record>,
) {
    let status = match status {
        Some(Ok(Some(status))) => status,
        Some(Err(TimedOut)) => {
            out.push(Record::warn(
                format!("{name}: repository did not answer"),
                Some(timed_out_detail()),
            ));
            return;
        }
        Some(Ok(None)) | None => {
            out.push(Record::warn(
                format!("{name}: repository status is unavailable"),
                Some("run dot status to inspect".to_string()),
            ));
            return;
        }
    };
    let prefix = [OsString::from("-C"), OsString::from(path)];
    let git_dir = worktree_git_dir(Path::new(path));
    let interrupted = interruption(status, git_dir.as_deref());
    if status.unmerged > 0 && interrupted.is_none() {
        let pullable = status.head.is_some() && status.upstream.is_some();
        out.extend(unmerged_record(
            format!("{name}: {} unmerged path(s)", status.unmerged),
            &prefix,
            pullable,
        ));
    }
    out.extend(frozen_rebase_record(
        |upstream| format!("{name}: the last rebase onto {upstream} conflicted; rebase manually"),
        git_dir.as_deref(),
        status,
        optional,
    ));
    let tracked = status.changed + status.unmerged;
    if tracked > 0 {
        out.push(Record::warn(
            format!("{name}: {tracked} tracked change(s)"),
            Some("run dot status to inspect".to_string()),
        ));
    }
    let subject = Subject::overlay(name, &prefix);
    let Some(head) = status.head.as_deref() else {
        out.push(headless_record(&subject, interrupted, tracked == 0));
        return;
    };
    let Some(upstream) = status.upstream.as_deref() else {
        out.push(no_upstream_record(&subject, head));
        return;
    };
    match status.ahead_behind {
        Some((ahead, behind)) => match distance(upstream, ahead, behind) {
            None => out.push(Record::ok(
                format!("{name}: upstream"),
                Some(format!("{upstream} (current)")),
            )),
            Some((kind, detail)) => {
                let predicate = match kind {
                    Distance::Behind => "behind upstream",
                    Distance::Ahead => "ahead of upstream",
                    Distance::Diverged => "diverged from upstream",
                };
                out.push(Record::warn(format!("{name}: {predicate}"), Some(detail)));
            }
        },
        // Git names the upstream but cannot resolve it (a gone branch);
        // update's upstream probe fails the same way and skips the pull.
        None => out.push(gone_upstream_record(&subject, upstream)),
    }
}

/// Who a repository row speaks for, in the wording each section has always
/// used, so the client and overlay rows for one state read alike.
struct Subject<'a> {
    /// Message prefix: `client ` or `<name>: `.
    prefix: String,
    /// What `dot update` skips: `the client` or `this overlay`.
    object: &'static str,
    /// Git argv prefix that selects this repository, for copy-pasteable
    /// commands (the base needs its separate Git directory and work tree).
    git: &'a [OsString],
}

impl<'a> Subject<'a> {
    fn client(git: &'a [OsString]) -> Self {
        Subject {
            prefix: "client ".to_string(),
            object: "the client",
            git,
        }
    }

    fn overlay(name: &str, git: &'a [OsString]) -> Self {
        Subject {
            prefix: format!("{name}: "),
            object: "this overlay",
            git,
        }
    }

    fn message(&self, predicate: &str) -> String {
        format!("{}{predicate}", self.prefix)
    }
}

/// What a repository whose HEAD is on no branch is in the middle of, read
/// from its per-worktree Git directory (stat-level); `None` on a branch or
/// when nothing is in progress.
fn interruption(
    status: &RepoStatus,
    git_dir: Option<&Path>,
) -> Option<crate::repos_pull::Interruption> {
    if status.head.is_some() {
        return None;
    }
    git_dir.and_then(crate::repos_pull::interruption)
}

/// The row for a repository whose HEAD is on no branch, in `dot update`'s
/// severity for the same state (`repos_pull::stranded_head`). A rebase
/// leaves HEAD detached, so this used to read as a plain detached HEAD with
/// the wrong next step, even for dot's own interrupted rebase that fails
/// every update:
///
/// - dot's own interrupted rebase with nothing uncommitted: the next update
///   aborts it and pulls (a warning);
/// - the same with uncommitted changes: every update fails, optional
///   overlays included, until it is aborted or finished (a failure);
/// - a session the user started, or a plain detached HEAD: update skips the
///   repository (a warning).
fn headless_record(
    subject: &Subject<'_>,
    interrupted: Option<crate::repos_pull::Interruption>,
    clean: bool,
) -> Record {
    use crate::repos_pull::{Interruption, git_hint};
    match interrupted {
        Some(Interruption::DotRebase) if clean => Record::warn(
            subject.message("has an interrupted dot rebase"),
            Some("the next dot update aborts it, which discards nothing, and pulls".to_string()),
        )
        .with_hint("run dot update to finish now"),
        Some(Interruption::DotRebase) => Record::fail(
            subject.message("has an interrupted dot rebase"),
            Some(
                "dot update fails until it is aborted; it aborts it itself only with nothing uncommitted"
                    .to_string(),
            ),
        )
        .with_hint(format!(
            "run {}, or resolve it and run {}",
            git_hint(subject.git, "rebase --abort"),
            git_hint(subject.git, "rebase --continue")
        )),
        Some(Interruption::UserSession) => Record::warn(
            subject.message("has an unfinished merge, rebase, cherry-pick, revert, or am"),
            Some(format!(
                "dot update skips {} until it is finished",
                subject.object
            )),
        )
        .with_hint(format!(
            "finish or abort it (see {})",
            git_hint(subject.git, "status")
        )),
        None => Record::warn(
            subject.message("HEAD is detached"),
            Some(format!(
                "dot update skips {} until it is back on a branch",
                subject.object
            )),
        )
        .with_hint(format!(
            "check out its branch: {}",
            git_hint(subject.git, "switch BRANCH")
        )),
    }
}

/// The row for a branch with no upstream: update skips pulling it.
fn no_upstream_record(subject: &Subject<'_>, head: &str) -> Record {
    let target = crate::repos_pull_support::shell_quote(format!("origin/{head}").as_bytes());
    Record::warn(
        subject.message("upstream is not configured"),
        Some(format!("dot update skips pulling {}", subject.object)),
    )
    .with_hint(format!(
        "set one: {}",
        crate::repos_pull::git_hint(subject.git, &format!("branch --set-upstream-to={target}"))
    ))
}

/// The row for an upstream Git names but cannot resolve (a deleted remote
/// branch): update's upstream probe fails the same way and skips the pull.
fn gone_upstream_record(subject: &Subject<'_>, upstream: &str) -> Record {
    Record::warn(
        subject.message("upstream could not be compared"),
        Some(format!(
            "{upstream}; dot update skips pulling {}",
            subject.object
        )),
    )
    .with_hint(format!(
        "if it was deleted, point the branch at another: {}",
        crate::repos_pull::git_hint(subject.git, "branch --set-upstream-to=REMOTE/BRANCH")
    ))
}

/// How a healthy branch reads in a folded row.
fn current_fact(head: &str, upstream: &str) -> String {
    format!("{head}, current with {upstream}")
}

/// Fold a repository's rows into one ok row carrying `detail` when every
/// row passed: a healthy repository used to take three to six identical
/// checkmarks. Any warning, failure, or skip keeps every row, so a problem
/// is always shown with its context. Section titles are kept.
fn fold_healthy(rows: Vec<Record>, message: &str, detail: String) -> Vec<Record> {
    let passed = |record: &Record| record.kind == Kind::Ok;
    let healthy = rows.iter().any(passed)
        && rows
            .iter()
            .all(|record| passed(record) || record.kind == Kind::Section);
    if !healthy {
        return rows;
    }
    let mut out: Vec<Record> = rows
        .into_iter()
        .filter(|record| record.kind == Kind::Section)
        .collect();
    out.push(Record::ok(message, Some(detail)));
    out
}

/// [`STATUS_ARGS`] for one overlay checkout, `None` when Git fails.
fn overlay_status(
    path: &str,
    deadline: Option<std::time::Instant>,
) -> Result<Option<RepoStatus>, TimedOut> {
    let prefix = [OsString::from("-C"), OsString::from(path)];
    let Some(output) = inspect_git(&prefix, &STATUS_ARGS, deadline)? else {
        return Ok(None);
    };
    Ok(output
        .status
        .success()
        .then(|| parse_status_v2(&String::from_utf8_lossy(&output.stdout))))
}

/// One overlay's [`overlay_status`] answer.
type OverlayStatus = Result<Option<RepoStatus>, TimedOut>;

/// Run [`overlay_status`] for every path concurrently: each is an
/// independent read-only Git process, and serially they would add one Git
/// round trip per overlay to every doctor run.
fn overlay_statuses(paths: Vec<String>) -> BTreeMap<String, OverlayStatus> {
    let host_git = crate::init_client_identity::carry_host_git();
    // One deadline for the batch: the probes run at once.
    let deadline = probe_deadline();
    std::thread::scope(|scope| {
        // A thread the OS refuses runs its probe inline instead; a probe
        // that panics leaves its path out, which reads as "unavailable".
        let mut results = BTreeMap::new();
        let mut handles = Vec::new();
        for path in paths {
            let carried = host_git.clone();
            let spawned = std::thread::Builder::new().spawn_scoped(scope, {
                let path = path.clone();
                move || {
                    let _host_git = carried.bind();
                    let status = overlay_status(&path, deadline);
                    (path, status)
                }
            });
            match spawned {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    let status = overlay_status(&path, deadline);
                    results.insert(path, status);
                }
            }
        }
        results.extend(handles.into_iter().filter_map(|handle| handle.join().ok()));
        results
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update_lock::OwnerActivity as Activity;

    /// A host Git that records its pid, then hangs far past any probe
    /// deadline: a stalled filesystem stand-in.
    fn hanging_git(scope: &dot_test_support::TempDir) -> (PathBuf, PathBuf) {
        let pids = scope.path().join("pids");
        let shim = scope.path().join("git");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\necho $$ >> '{}'\nexec sleep 60\n",
                pids.display()
            ),
        )
        .expect("hanging git");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("mode");
        (shim, pids)
    }

    /// Run `body` with a short probe deadline on this thread.
    fn with_short_probes<R>(body: impl FnOnce() -> R) -> R {
        TEST_PROBE_TIMEOUT.with(|slot| slot.set(Some(std::time::Duration::from_millis(300))));
        let result = body();
        TEST_PROBE_TIMEOUT.with(|slot| slot.set(None));
        result
    }

    /// Every probe the hanging Git started must be gone: the deadline stops
    /// and reaps the probe's session instead of abandoning it.
    fn assert_probes_reaped(pids: &Path) {
        let recorded = std::fs::read_to_string(pids).expect("probe pids");
        assert!(!recorded.trim().is_empty(), "the probe never ran");
        for pid in recorded.split_whitespace() {
            let pid: libc::pid_t = pid.parse().expect("pid");
            // SAFETY: signal 0 only checks existence; no signal is sent.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            assert!(!alive, "probe {pid} outlived its deadline");
        }
    }

    #[test]
    fn stalled_client_probe_warns_instead_of_hanging() {
        let scope = dot_test_support::TempDir::new_exec("doctor-stalled-client").expect("scope");
        let (shim, pids) = hanging_git(&scope);
        let home = scope.path().join("home");
        let git_dir = home.join(".dotfiles");
        std::fs::create_dir_all(&git_dir).expect("git dir");
        let started = std::time::Instant::now();
        let records = crate::init_client_identity::with_host_git(&shim, || {
            with_short_probes(|| {
                check_base_repo(&BaseRepoInputs {
                    topology: "separate",
                    client_git_dir: git_dir.to_str().expect("git dir"),
                    home: home.to_str().expect("home"),
                    is_client_checkout: false,
                })
            })
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "one stalled probe ended the section: {:?}",
            started.elapsed()
        );
        let rendered = render(&records);
        assert!(
            rendered.contains("⚠ client repository did not answer"),
            "{rendered}"
        );
        assert!(rendered.contains("did not answer within 30s"), "{rendered}");
        assert!(
            !rendered.contains('✗'),
            "a stall is not a failure: {rendered}"
        );
        assert_probes_reaped(&pids);
    }

    #[test]
    fn stalled_overlay_status_warns_instead_of_hanging() {
        let scope = dot_test_support::TempDir::new_exec("doctor-stalled-overlay").expect("scope");
        let (shim, pids) = hanging_git(&scope);
        let overlay = scope.path().join("overlay");
        std::fs::create_dir_all(&overlay).expect("overlay");
        let path = overlay.to_str().expect("overlay").to_string();
        let statuses = crate::init_client_identity::with_host_git(&shim, || {
            with_short_probes(|| overlay_statuses(vec![path.clone()]))
        });
        assert_eq!(statuses.get(&path), Some(&Err(TimedOut)));
        let mut out = Vec::new();
        overlay_state_records("dev", &path, false, statuses.get(&path), &mut out);
        let rendered = render(&out);
        assert!(
            rendered.contains("⚠ dev: repository did not answer"),
            "{rendered}"
        );
        assert_probes_reaped(&pids);
    }

    #[test]
    fn shorten_keeps_whole_words_and_drops_clause_punctuation() {
        assert_eq!(shorten("short", 10), "short");
        assert_eq!(shorten("exactly10b", 10), "exactly10b");
        // A word followed by clause punctuation at the cut is whole.
        assert_eq!(shorten("aa, bb: cc dd", 9), "aa, bb…");
        assert_eq!(shorten("one two three four", 12), "one two…");
        // No usable space: cut on a character boundary.
        assert_eq!(shorten("abcdefghijklmnop", 8), "abcde…");
        assert_eq!(shorten("ééééééé", 8), "éé…");
        // A space too early would waste the budget, so cut mid-word then.
        assert_eq!(shorten("a bcdefghijklmnop", 10), "a bcdef…");
    }

    #[test]
    fn lock_owner_rows_match_acquire_behavior() {
        // Fresh-review-B B6: only the stale row may promise
        // reclamation; `acquire` refuses on `Unknown`/`Interrupted`,
        // so those rows must say refuse, never reclaim.
        let owner = crate::update_lock::Owner {
            pid: 4242,
            start: "proc:99".to_string(),
            token: "4242.0.0".to_string(),
        };
        let rendered = |activity| render(&[lock_owner_record(&owner, activity, None)]);
        assert!(rendered(Activity::Active).contains("currently running"));
        assert!(rendered(Activity::Active).contains("pid 4242"));
        let stale = rendered(Activity::Stale);
        assert!(stale.contains("owner is stale"));
        assert!(stale.contains("reclaims the stale lock"));
        assert!(rendered(Activity::Active).contains('⚠'));
        assert!(stale.contains('⚠'));
        // `acquire` refuses on an unverifiable owner, so doctor fails; a
        // probe interrupted by a signal to doctor itself only warns.
        let unknown = rendered(Activity::Unknown);
        assert!(
            unknown.contains("✗ update lock owner cannot be verified"),
            "{unknown}"
        );
        assert!(unknown.contains("refuse"));
        let interrupted = rendered(Activity::Interrupted);
        assert!(
            interrupted.contains("⚠ update lock owner cannot be verified"),
            "{interrupted}"
        );
        assert!(
            unknown.contains("→ check that ps -o lstart= -p 4242 prints a start time"),
            "{unknown}"
        );
        for row in [&unknown, &interrupted] {
            assert!(!row.contains("reclaim"));
        }
        // Every owner row, warn or fail, says what to do next.
        for activity in [
            Activity::Active,
            Activity::Stale,
            Activity::Unknown,
            Activity::Interrupted,
        ] {
            let row = rendered(activity);
            assert!(row.contains("\n    → "), "{row}");
        }
    }
}
