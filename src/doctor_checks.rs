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
//! - `_dr_tilde` / `_dr_symlink_points_to` (`doctor/paths.sh`) are
//!   mirrored as private helpers: display-only glue the checks need
//!   to spell details, with display policy owned by `doctor_paths`.
//! - `local_validate` (`_overlay_local_source_validate`,
//!   `find`-walk plus per-entry checks), the profile deactivation
//!   probe, the shdeps installer selection, and the lifecycle ledger
//!   load stay caller concerns: they encode trust policy owned by
//!   other modules, so tests inject their outcomes.
//! - The `_dr_check_merges` "inventory is invalid" branch only
//!   fires when the `wc -l` pipeline itself fails (a bad inventory
//!   still prints zero lines through `sort`, whose exit status
//!   decides); the port keeps the branch with `spec_count: None`.
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

pub use crate::doctor_runtime::{Kind, Record};

/// Render records byte-identical to `doctor/runtime.sh` with color
/// disabled (piped stdout: every color variable is empty):
///
/// - section: `\n{message}\n`
/// - ok/skip: `  ✓/· {message}[ ({detail})]\n`
/// - warn/fail: `  ⚠/✗ {message}[\n    {detail}]\n`
pub fn render(records: &[Record]) -> String {
    String::from_utf8(crate::doctor_runtime::render(
        records,
        &crate::doctor_runtime::Palette::empty(),
    ))
    .expect("doctor checks emit UTF-8 text")
}

/// `_dr_tilde`: abbreviate `path` under `home` with `~`. Mirrors
/// the shell `case` arms literally, including the empty-`HOME`
/// corner (`"$HOME"/*` with empty `HOME` matches `/*`).
fn tilde(path: &str, home: &str) -> String {
    if path == home {
        return "~".to_string();
    }
    if home.is_empty() {
        if let Some(rest) = path.strip_prefix('/') {
            return format!("~/{rest}");
        }
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(home) {
        if let Some(rest) = rest.strip_prefix('/') {
            return format!("~/{rest}");
        }
    }
    path.to_string()
}

/// `_dr_physical_path`: resolve the parent directory physically
/// (`cd dir && pwd -P`), keeping the leaf name. Trailing slashes
/// strip (except root); `/` maps to `//` like the shell
/// `printf '%s/%s\n' / /`. Returns `None` when the parent is not a
/// directory or cannot be resolved.
fn physical_path(path: &str) -> Option<String> {
    let mut rest = path;
    while rest != "/" && rest.ends_with('/') {
        rest = &rest[..rest.len() - 1];
    }
    let (dir, base) = if rest == "/" {
        ("/", "/")
    } else if let Some(index) = rest.rfind('/') {
        let (dir, base) = rest.split_at(index);
        (if dir.is_empty() { "/" } else { dir }, &base[1..])
    } else {
        (".", rest)
    };
    if !Path::new(dir).is_dir() {
        return None;
    }
    let canonical = std::fs::canonicalize(dir).ok()?;
    Some(format!("{}/{}", canonical.display(), base))
}

/// `_dr_symlink_target_path` plus `_dr_symlink_points_to`: whether
/// `link` resolves (through a possibly relative `readlink` target)
/// to the same physical path as `expected`. A missing `expected`
/// (`[[ -e ]]`, links followed) or an unreadable link fails.
fn symlink_points_to(link: &Path, expected: &str) -> bool {
    if !Path::new(expected).exists() {
        return false;
    }
    let target = match std::fs::read_link(link) {
        Ok(target) => target,
        Err(_) => return false,
    };
    let joined: PathBuf = if target.is_absolute() {
        target
    } else {
        let dir = link.parent().unwrap_or_else(|| Path::new("."));
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        dir.join(&target)
    };
    let actual = match physical_path(&joined.to_string_lossy()) {
        Some(actual) => actual,
        None => return false,
    };
    match physical_path(expected) {
        Some(expected_physical) => actual == expected_physical,
        None => false,
    }
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

/// Shell `[[ $value =~ ^[0-9]+$ ]]`.
fn is_uint(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
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
fn lock_owner_record(
    owner: &crate::update_lock::Owner,
    activity: crate::update_lock::OwnerActivity,
) -> Record {
    use crate::update_lock::OwnerActivity as Activity;

    match activity {
        Activity::Active => Record::warn(
            "update is currently running",
            Some(format!("pid {}", owner.pid)),
        ),
        Activity::Stale => Record::warn(
            "update lock owner is stale",
            Some("the next mutating command will reclaim it".to_string()),
        ),
        Activity::Unknown => Record::fail(
            "update lock owner cannot be verified",
            Some("mutating commands refuse until the owner probe succeeds".to_string()),
        ),
        // Only this doctor run was interrupted mid-probe; nothing about the
        // lock is known to be wrong.
        Activity::Interrupted => Record::warn(
            "update lock owner cannot be verified",
            Some("the owner probe was interrupted".to_string()),
        ),
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
        out.push(Record::fail("update lock path cannot be resolved", None));
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
        out.push(Record::fail(
            "update lock path is unsafe",
            Some(dir.to_string_lossy().into_owned()),
        ));
        return out;
    }
    if let Some(owner) = crate::update_lock::read_owner(dir) {
        out.push(lock_owner_record(
            &owner,
            crate::update_lock::owner_activity(&owner),
        ));
    } else if crate::update_lock::is_initializing(dir) {
        out.push(Record::warn("update lock is being initialized", None));
    } else {
        out.push(Record::warn(
            "update lock record is incomplete",
            Some("the next mutating command will attempt recovery".to_string()),
        ));
    }
    out
}

/// One merge-hook output declaration for freshness
/// verification (handoff finding #8): the script and its
/// `.outputs` sidecar arrive trust-validated from
/// [`crate::doctor`]; the outputs arrive expanded to absolute
/// paths. Freshness inputs are the script, the sidecar, and the
/// identity-named family directory when one exists.
pub struct MergeSpec {
    /// Hook identity (the spec label doctor reports).
    pub identity: String,
    /// Hook script path (a freshness input).
    pub script: String,
    /// Outputs-declaration sidecar path, when one exists (a
    /// freshness input).
    pub sidecar: Option<String>,
    /// Identity-named family directory, when one exists
    /// (freshness inputs root).
    pub family_dir: Option<String>,
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
    /// Per-hook output declarations for freshness verification.
    /// Empty skips verification (the historical count-only
    /// behavior); production always passes one entry per
    /// inventoried hook.
    pub specs: Vec<MergeSpec>,
}

/// Bounds for the family-directory walk below: a merge family is
/// a handful of hook sources, so thousands of files or deep
/// nesting means a pathological tree, not freshness inputs.
const FAMILY_WALK_MAX_FILES: usize = 4096;
/// Maximum descent below the family directory itself.
const FAMILY_WALK_MAX_DEPTH: usize = 32;

/// Newest mtime across a merge spec's freshness inputs: the
/// hook script, its `.outputs` sidecar, and every regular file
/// under the identity-named family directory (bounded by
/// [`FAMILY_WALK_MAX_FILES`] files and [`FAMILY_WALK_MAX_DEPTH`]
/// levels). Missing inputs are ignored; `None` means nothing was
/// comparable.
fn newest_input_mtime(spec: &MergeSpec) -> Option<std::time::SystemTime> {
    newest_input_mtime_bounded(spec, FAMILY_WALK_MAX_FILES, FAMILY_WALK_MAX_DEPTH)
}

fn newest_input_mtime_bounded(
    spec: &MergeSpec,
    max_files: usize,
    max_depth: usize,
) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut consider = |path: &Path| {
        let mtime = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok();
        if let Some(mtime) = mtime {
            if newest.is_none_or(|best| mtime > best) {
                newest = Some(mtime);
            }
        }
    };
    consider(Path::new(&spec.script));
    if let Some(sidecar) = spec.sidecar.as_deref() {
        consider(Path::new(sidecar));
    }
    if let Some(dir) = spec.family_dir.as_deref() {
        // Symlinks never descend (dirent types only), so no cycle
        // risk; the file/depth budgets bound hostile breadth.
        let mut stack = vec![(PathBuf::from(dir), 0usize)];
        let mut files = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                if files >= max_files {
                    break;
                }
                let path = entry.path();
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => {
                        if depth < max_depth {
                            stack.push((path, depth + 1));
                        }
                    }
                    Ok(kind) if kind.is_file() => {
                        files += 1;
                        consider(&path);
                    }
                    _ => {}
                }
            }
            if files >= max_files {
                break;
            }
        }
    }
    newest
}

/// One merge spec's output verification result, before aggregation.
enum MergeVerdict {
    /// Problems to report row by row (invalid declarations, missing or stale
    /// outputs).
    Problems(Vec<Record>),
    /// No declared outputs, or nothing to compare them against.
    Unverified,
    /// Every declared output exists and is newer than every input.
    Current(usize),
}

/// Verify one merge spec's declared live outputs: each must exist
/// and be strictly newer than its newest input. Hooks without
/// declarations are unverified (documented, not a failure); invalid
/// declarations fail outright.
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
        return MergeVerdict::Unverified;
    }
    let Some(newest) = newest_input_mtime(spec) else {
        return MergeVerdict::Unverified;
    };
    let mut problems = Vec::new();
    for output in &spec.outputs {
        let mtime = std::fs::metadata(Path::new(output))
            .and_then(|meta| meta.modified())
            .ok();
        match mtime {
            None => problems.push(Record::fail(
                "merge-hook output is missing",
                Some(format!("{}: {output}", spec.identity)),
            )),
            Some(mtime) if mtime <= newest => problems.push(Record::fail(
                "merge-hook output is stale",
                Some(format!("{}: {output}", spec.identity)),
            )),
            Some(_) => {}
        }
    }
    if problems.is_empty() {
        MergeVerdict::Current(spec.outputs.len())
    } else {
        MergeVerdict::Problems(problems)
    }
}

/// Every spec's output verification, collapsed: problems keep one row each,
/// while healthy and unverified hooks each fold into a single summary row
/// (one row per hook used to bury the problems among dozens of identical
/// skips).
fn merge_output_records(specs: &[MergeSpec]) -> Vec<Record> {
    let mut problems = Vec::new();
    let mut unverified = 0usize;
    let (mut current_hooks, mut current_outputs) = (0usize, 0usize);
    for spec in specs {
        match verify_merge_outputs(spec) {
            MergeVerdict::Problems(rows) => problems.extend(rows),
            MergeVerdict::Unverified => unverified += 1,
            MergeVerdict::Current(outputs) => {
                current_hooks += 1;
                current_outputs += outputs;
            }
        }
    }
    let mut out = problems;
    if current_hooks > 0 {
        out.push(Record::ok(
            "merge-hook outputs are current",
            Some(format!(
                "{current_outputs} output(s) across {current_hooks} hook(s)"
            )),
        ));
    }
    if unverified > 0 {
        out.push(Record::skip(
            "merge-hook outputs are unverified",
            Some(format!("{unverified} hook(s) declare no checkable outputs")),
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
            Some(root),
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
/// stopped converging. Both use the same staleness window, so a
/// transient failure after a recent clean run still reads ok, exactly
/// as before the degraded outcome existed.
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
    use crate::update_status::{format_age, is_stale};

    let age = |at: i64| format_age(inputs.now.saturating_sub(at));
    let converged = inputs.last_converged.as_ref();
    // A clean convergence stamp is a success even if the last-success
    // write was lost; both are written by the same clean run. A clean cron
    // last run counts too, in case both stamp writes were lost.
    let clean_cron_run = inputs
        .last_run
        .as_ref()
        .filter(|last| last.is_cron() && last.outcome == crate::update_status::OUTCOME_OK)
        .map(|last| last.at);
    let clean = converged
        .filter(|converged| converged.failing.is_empty())
        .map(|converged| converged.at)
        .into_iter()
        .chain(inputs.last_success)
        .chain(clean_cron_run)
        .max();
    if let Some(clean) = clean.filter(|clean| !is_stale(*clean, inputs.now)) {
        out.push(Record::ok(
            "cron update succeeded recently",
            Some(format!("{} ago", age(clean))),
        ));
        return Some(clean);
    }
    if let Some(converged) = converged
        .filter(|converged| !converged.failing.is_empty() && !is_stale(converged.at, inputs.now))
    {
        let since = match inputs.last_success {
            Some(last) => format!("since last success {} ago", age(last)),
            None => "no clean cron update recorded".to_string(),
        };
        out.push(Record::warn(
            format!("cron update degraded: {} failing", converged.failing),
            Some(format!("{since}; last converged {} ago", age(converged.at))),
        ));
        return None;
    }
    // Stale from here on. `clean` (not just last-success) is the success
    // reference, so a lone clean convergence stamp reads as a success. Only
    // a degraded convergence newer than it adds information (an older one is
    // left behind by a downgrade to a Dot that does not write it).
    let converged_note = converged
        .filter(|converged| clean.is_none_or(|clean| converged.at > clean))
        .map(|converged| format!("; last converged {} ago", age(converged.at)))
        .unwrap_or_default();
    match clean {
        Some(clean) => out.push(Record::warn(
            "cron update has not succeeded recently",
            Some(format!("last success {} ago{converged_note}", age(clean))),
        )),
        // Only degraded runs ever converged, and they stopped too.
        None if converged.is_some() => out.push(Record::warn(
            "cron update has not succeeded recently",
            Some(format!(
                "no successful cron update recorded{converged_note}"
            )),
        )),
        None => out.push(never_converged_record(inputs)),
    }
    None
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
        return Record::warn(
            "cron update has not succeeded recently",
            Some(format!(
                "no successful cron update recorded; last cron run {} {} ago",
                last.outcome,
                age(last.at)
            )),
        );
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
                "last update: {} run {} ago; schedule dot update --cron to keep this host current",
                last.trigger,
                age(last.at)
            )),
        );
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
    Some(if succeeded {
        Record::ok("last update succeeded", Some(detail))
    } else if last.outcome == OUTCOME_DEGRADED {
        let failing = if last.failing.is_empty() {
            "a stage"
        } else {
            last.failing.as_str()
        };
        Record::warn(
            format!("last update degraded: {failing} failing"),
            Some(format!(
                "{detail}; rerun dot update to see the failing stage"
            )),
        )
    } else {
        Record::warn(
            "last update failed",
            Some(format!("{detail}; rerun dot update to see what failed")),
        )
    })
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
        CheckpointState::Pending => vec![Record::warn(
            "provider re-exec checkpoint pending",
            Some(format!(
                "{shown}: dot changed twice during the last update; the next dot update validates and removes it"
            )),
        )],
        CheckpointState::Unreadable => vec![Record::fail(
            "provider re-exec checkpoint blocks dot update",
            Some(format!(
                "{shown} is unsafe or malformed; inspect it, remove it, then run dot update"
            )),
        )],
        CheckpointState::Mismatch { pinned, active } => vec![Record::fail(
            "provider re-exec checkpoint blocks dot update",
            Some(format!(
                "{shown} pins {} but dot is at {}; inspect the provider state, remove the record, then run dot update",
                short(pinned),
                short(active)
            )),
        )],
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
                    out.push(Record::fail(
                        format!("{name}: active profile deactivation authority unsafe"),
                        None,
                    ));
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

/// `_dr_check_overlays` (`doctor/overlays.sh`): profile selection
/// reporting, per-overlay lifecycle and source health, and overlay
/// symlink ownership validation. The profile identity, selection, and
/// matching selector are configuration facts, so they render as
/// informational rows that are never counted.
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
        if let (Some(user), Some(host)) =
            (present(inputs.profile_user), present(inputs.profile_host))
        {
            out.push(Record::info(
                "profile identity",
                Some(format!("{user}@{host}")),
            ));
        }
        if let Some(selected) = present(inputs.selected_profile) {
            let state = present(inputs.selection_state).unwrap_or("unknown");
            out.push(Record::info(
                "selected profile",
                Some(format!("{selected} ({state})")),
            ));
        }
        if !inputs.included_profiles.is_empty() {
            out.push(Record::info(
                "included profiles",
                Some(inputs.included_profiles.join(" ")),
            ));
        }
        if !inputs.phase_one.is_empty() {
            out.push(Record::info(
                "phase-one overlays",
                Some(inputs.phase_one.join(" ")),
            ));
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
            out.push(Record::info(
                format!("matching selector ({source})"),
                Some(format!("{} -> {}", leaf, fields[4])),
            ));
        }
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
                out.push(Record::warn(
                    format!("{name}: selected but skipped: unknown descriptor key"),
                    (!keys.is_empty()).then(|| keys.join("; ")),
                ));
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
                out.push(Record::fail(
                    format!("{name}: selected but unavailable"),
                    None,
                ));
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
                out.push(Record::fail(
                    format!("{name}: active lifecycle record missing"),
                    None,
                ));
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
    let mut issues: u64 = 0;
    let mut owners: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    for line in crate::repos_overlays::stream_lines(&content) {
        let Some(parsed) = crate::repos_overlays::parse_manifest_record(&line) else {
            issues += 1;
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
        let link_meta = std::fs::symlink_metadata(dst_path).ok();
        let is_link = link_meta
            .as_ref()
            .is_some_and(|meta| meta.file_type().is_symlink());
        if !is_link {
            issues += 1;
            continue;
        }
        if !dst_path.exists() {
            issues += 1;
            continue;
        }
        let Some(path) = overlay_paths.get(owner) else {
            issues += 1;
            continue;
        };
        let Some(sync) = overlay_syncs.get(owner) else {
            issues += 1;
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
                issues += 1;
                continue;
            }
        };
        if *exact {
            if expected_lexical != &current || actual != current {
                issues += 1;
            }
            continue;
        }
        if actual == current {
            continue;
        }
        let expected = format!("{path}/home/{rel}");
        if !symlink_points_to(dst_path, &expected) {
            issues += 1;
        }
    }
    if issues == 0 {
        out.push(Record::ok("overlay symlinks healthy", None));
    } else {
        out.push(Record::warn(
            format!("{issues} overlay symlink issue(s)"),
            Some("run 'dot update' to re-link".to_string()),
        ));
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
pub fn check_install_layout(inputs: &InstallInputs) -> Vec<Record> {
    let mut out = Vec::new();
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
            check_adoption_lock(inputs, &control, &mut out);
            return out;
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
            check_adoption_lock(inputs, &control, &mut out);
            return out;
        }
    } else if let Some(control) = managed_standalone.or(running_standalone.map(Path::to_path_buf)) {
        out.push(Record::ok(
            "dot release layout",
            Some("standalone installer (rerun install.sh to upgrade)".to_string()),
        ));
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
        return out;
    }
    let managed_dir =
        std::fs::symlink_metadata(inputs.managed_root).is_ok_and(|meta| meta.file_type().is_dir());
    let running_managed = managed_real.as_deref() == Some(inputs.source_real);
    if !inputs.shdeps {
        return out;
    }
    if inputs.release_root && managed_dir && running_managed {
        check_layout_marker(inputs, &mut out);
    }
    // Leftovers are reported whichever Dot runs: a failed swap whose rollback
    // also failed leaves only the backup, with no root to run from.
    check_install_leftovers(inputs, managed_dir, &mut out);
    out
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

/// The Shdeps archive marker of the managed release root Dot runs from.
fn check_layout_marker(inputs: &InstallInputs, out: &mut Vec<Record>) {
    let marker = inputs.managed_root.join(SHDEPS_LAYOUT_FILE);
    let marker_shown = tilde(&marker.to_string_lossy(), inputs.home);
    match std::fs::symlink_metadata(&marker) {
        Ok(meta) if meta.file_type().is_file() => {
            if std::fs::read(&marker).is_ok_and(|content| content == SHDEPS_LAYOUT_CONTENT) {
                out.push(Record::ok("dot release layout", Some("Shdeps release".to_string())));
            } else {
                out.push(Record::fail(
                    "dot release layout marker is invalid",
                    Some(format!(
                        "{marker_shown}: Shdeps refuses to upgrade until it holds 'v1 archive'"
                    )),
                ));
            }
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
            out.push(Record::warn(
                "interrupted Shdeps install left a backup",
                Some(format!("{}; {next}", backups.join(" "))),
            ));
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

/// `_dr_check_provider` (`doctor/provider.sh`): the dependency
/// provider boundary (reviewed installer selection plus ABI
/// agreement). Helper outcomes arrive via [`ProviderInputs`];
/// only the `_dr_tilde` display runs in-process. The update policy,
/// provider source, and development revision are configuration facts, so
/// they render as informational rows that are never counted.
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
    if !inputs.configure_ok {
        out.push(Record::info(
            "Shdeps update policy",
            Some(policy.to_string()),
        ));
        out.push(Record::fail(
            "Shdeps provider is unavailable",
            Some("run dot update to bootstrap the reviewed provider release".to_string()),
        ));
        return out;
    }
    out.push(Record::info(
        "Shdeps update policy",
        Some(policy.to_string()),
    ));
    let development = format!("{}/shdeps", inputs.dev_dir);
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
        "explicit" => {
            out.push(Record::info(
                "Shdeps provider source",
                Some(format!(
                    "caller-selected reviewed installer: {}",
                    tilde(installer.path, inputs.home)
                )),
            ));
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        "pinned-dev" => {
            if policy == "latest" {
                out.push(Record::info(
                    "Shdeps provider source",
                    Some(format!(
                        "development checkout selected by Dot lock: {}",
                        tilde(&development, inputs.home)
                    )),
                ));
            }
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        "latest-dev" => {
            out.push(Record::info(
                "Shdeps provider source",
                Some(format!(
                    "trusted development checkout: {}",
                    tilde(&development, inputs.home)
                )),
            ));
        }
        "managed" => {
            if policy == "latest" {
                out.push(Record::info(
                    "Shdeps provider source",
                    Some("managed release via reviewed bootstrap".to_string()),
                ));
            }
            out.push(Record::ok(
                "Shdeps installer is reviewed",
                Some(tilde(installer.path, inputs.home)),
            ));
        }
        _ => {
            out.push(Record::fail(
                "Shdeps provider source is unavailable",
                Some("run dot update to restore provider selection metadata".to_string()),
            ));
            return out;
        }
    }
    if policy == "latest" && (installer.source == "pinned-dev" || installer.source == "latest-dev")
    {
        let locked = inputs.locked_revision.unwrap_or("");
        let current = inputs.development_revision.unwrap_or("");
        if !locked.is_empty() && current == locked {
            let short: String = current.chars().take(12).collect();
            out.push(Record::info(
                "Shdeps development revision",
                Some(format!("matches Dot lock: {short}")),
            ));
        } else {
            let shown = if current.is_empty() {
                "<unavailable>"
            } else {
                current
            };
            out.push(Record::info(
                "Shdeps development revision",
                Some(format!(
                    "trusted unpinned revision differs from Dot lock; accepted by latest policy: {shown}"
                )),
            ));
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
fn base_git(topology: &str, client_git_dir: &str, home: &str, args: &[&str]) -> Option<String> {
    let prefix = base_git_prefix(topology, client_git_dir, home)?;
    let output = crate::repos_base::run_git(&prefix, args)?;
    if !output.status.success() {
        return None;
    }
    Some(captured(&String::from_utf8_lossy(&output.stdout)))
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
    if inputs.topology == "ordinary" {
        out.push(Record::ok("ordinary client layout", None));
    } else {
        let is_bare = base_git(
            inputs.topology,
            inputs.client_git_dir,
            inputs.home,
            &["config", "--get", "core.bare"],
        )
        .unwrap_or_else(|| "false".to_string());
        let has_worktree = base_git(
            inputs.topology,
            inputs.client_git_dir,
            inputs.home,
            &["config", "--get", "core.worktree"],
        )
        .unwrap_or_default();
        if is_bare == "true" {
            out.push(Record::ok("legacy bare client layout", None));
        } else if !has_worktree.is_empty() {
            out.push(Record::ok(
                "explicit-worktree client layout",
                Some(tilde(&has_worktree, inputs.home)),
            ));
        } else {
            out.push(Record::fail(
                "client Git directory has no worktree identity",
                None,
            ));
        }
    }
    let resolved = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["rev-parse", "--show-toplevel"],
    )
    .unwrap_or_default();
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
    // `status --porcelain | grep -cvE '^\?\?'`: only non-`??`
    // lines count; any `git` failure reads `0` through `|| true`.
    let dirty: usize = match base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["status", "--porcelain"],
    ) {
        Some(status) if !status.is_empty() => status
            .split('\n')
            .filter(|line| !line.starts_with("??"))
            .count(),
        _ => 0,
    };
    if dirty == 0 {
        out.push(Record::ok("no tracked client changes", None));
    } else {
        out.push(Record::warn(
            format!("{dirty} tracked client change(s)"),
            Some("run dot status to inspect".to_string()),
        ));
    }
    let head = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["symbolic-ref", "--short", "HEAD"],
    )
    .unwrap_or_default();
    if head.is_empty() {
        out.push(Record::warn("client HEAD is detached", None));
    } else {
        out.push(Record::ok("client HEAD on branch", Some(head)));
    }
    let upstream = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .unwrap_or_default();
    if upstream.is_empty() {
        out.push(Record::warn("client upstream is not configured", None));
        return out;
    }
    let counts = base_git(
        inputs.topology,
        inputs.client_git_dir,
        inputs.home,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{upstream}"),
        ],
    )
    .unwrap_or_default();
    // `IFS=$'\t' read -r ahead behind`: tab-separated, the
    // second variable keeping the remainder; no tab at all reads
    // both empty (the shell guards with `== *tab*` first).
    let (ahead, behind) = match counts.split_once('\t') {
        Some((ahead, behind)) => (ahead.to_string(), behind.to_string()),
        None => (String::new(), String::new()),
    };
    if is_uint(&ahead) && is_uint(&behind) {
        let ahead_count: u64 = ahead.parse().unwrap_or(0);
        let behind_count: u64 = behind.parse().unwrap_or(0);
        if ahead_count == 0 && behind_count == 0 {
            out.push(Record::ok(
                "client upstream",
                Some(format!("{upstream} (current)")),
            ));
        } else if ahead_count == 0 {
            out.push(Record::warn(
                "client is behind upstream",
                Some(format!("{upstream}: {behind} commit(s) behind")),
            ));
        } else if behind_count == 0 {
            out.push(Record::warn(
                "client is ahead of upstream",
                Some(format!("{upstream}: {ahead} commit(s) ahead")),
            ));
        } else {
            out.push(Record::warn(
                "client upstream has diverged",
                Some(format!("{upstream}: {ahead} ahead, {behind} behind")),
            ));
        }
    } else {
        out.push(Record::warn(
            "client upstream could not be compared",
            Some(upstream),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update_lock::OwnerActivity as Activity;

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
        let rendered = |activity| render(&[lock_owner_record(&owner, activity)]);
        assert!(rendered(Activity::Active).contains("currently running"));
        assert!(rendered(Activity::Active).contains("pid 4242"));
        let stale = rendered(Activity::Stale);
        assert!(stale.contains("owner is stale"));
        assert!(stale.contains("will reclaim it"));
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
        for row in [unknown, interrupted] {
            assert!(!row.contains("reclaim"));
        }
    }

    #[test]
    fn family_walk_honors_file_and_depth_budgets() {
        // Fresh-review-A nit-11: the freshness walk must terminate on
        // pathological trees. Tiny budgets pin both cutoffs without
        // building a 4096-file fixture.
        let scratch = dot_test_support::TempDir::new("doctor-family-bounds").expect("scratch");
        let family = scratch.path().join("family");
        std::fs::create_dir_all(family.join("sub/deep")).expect("family tree");
        let old = family.join("old.txt");
        let deep = family.join("sub/deep/new.txt");
        std::fs::write(&old, b"old").expect("old input");
        std::fs::write(&deep, b"new").expect("deep input");
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .expect("open old")
            .set_modified(past)
            .expect("age old");
        let spec = MergeSpec {
            identity: "bound".to_string(),
            script: scratch
                .path()
                .join("missing.sh")
                .to_string_lossy()
                .into_owned(),
            sidecar: None,
            family_dir: Some(family.to_string_lossy().into_owned()),
            outputs: Vec::new(),
            invalid: Vec::new(),
        };
        // Unbounded: the deep file is newest.
        let newest = newest_input_mtime_bounded(&spec, usize::MAX, usize::MAX).expect("newest");
        assert_eq!(
            newest,
            std::fs::metadata(&deep)
                .expect("deep meta")
                .modified()
                .expect("deep mtime")
        );
        // Depth 0 never descends: only the top-level old file counts
        // (compared through a metadata read-back so timestamp
        // truncation cannot perturb the pin).
        let shallow = newest_input_mtime_bounded(&spec, usize::MAX, 0).expect("shallow");
        assert_eq!(
            shallow,
            std::fs::metadata(&old)
                .expect("old meta")
                .modified()
                .expect("old mtime")
        );
        // Zero file budget: nothing under the family counts (the
        // script is missing, so no input is comparable at all).
        assert_eq!(newest_input_mtime_bounded(&spec, 0, usize::MAX), None);
    }
}
