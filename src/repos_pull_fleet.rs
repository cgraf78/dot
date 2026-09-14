//! Overlay worker fleet (`lib/dot/repos/pull.sh`): the parallel
//! pull fan-out with parent-owned scratch files and the top-level
//! synchronized-set pull.
//!
//! Ports `_pull_overlay_capture`, `_pull_overlay_drain_workers`,
//! `_pull_overlays_serial`, `_pull_overlays`, and `_repo_pull_all`.
//! The single-overlay orchestrator ([`crate::repos_pull_overlay`])
//! and the accounting leaves ([`crate::repos_pull_support`]) already
//! own their behavior; this layer only fans out, replays, and
//! aggregates on top of them.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`. Process-group orchestration has no Rust
//! equivalent: the shell's job-launch isolation, stdin-fd plumbing,
//! PID registration, and subshell trap reset are owned by scoped
//! thread joins and [`crate::cleanup::Registry`] instead. Only the
//! observable contract is preserved: active overlays fan out within
//! the [`crate::merges::update_jobs`] bound, each worker writes
//! indexed log/status/rc files under a parent-owned scratch
//! directory, the parent replays declaration order for stable UI and
//! tallies structured statuses, and scratch-allocation failure falls
//! back to the serial path. Worker `TMPDIR` containment differs:
//! threads share the process environment, so worker temps stay in
//! the ambient temp dir while the indexed result files stay
//! parent-owned; the differential rows pin only the shared
//! observable output.
//!
//! The overlay-override unstash (`_unstash_overlay_overrides`) stays
//! shell: [`pull_all`] covers the no-replacement common path where
//! it succeeds silently (every differential row).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cleanup::Registry;
use crate::log::Log;
use crate::merges::update_jobs;
use crate::progress_ui::{Palette, Stage, count_phrase, join_comma, progress_detail};
use crate::repos_base::Base;
use crate::repos_config::ensure_repo_config;
use crate::repos_dirty::normalize_filtered;
use crate::repos_overlays::{DestinationInputs, QuarantineInputs};
use crate::repos_pull::{PullBaseInputs, pull_base};
use crate::repos_pull_overlay::{PullOverlayInputs, pull_overlay};
use crate::repos_pull_queries::CandidateEnv;
use crate::repos_pull_support::{PullTally, overlay_active, record_status, result_prefix};
use crate::temp::{MoveCache, MoveTool};

/// Allocation counter behind [`alloc_result_dir`].
static FLEET_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One `OVERLAYS` record split like `IFS='|' read -r name path url
/// conf optional sync`: the first five fields take the first five
/// columns and `sync` takes the unsplit remainder (surplus `|`
/// sections fold into it, like the shell `read`). An empty sync
/// reads `"git"`, like `${sync:-git}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedOverlay<'a> {
    /// Overlay name for messages.
    pub name: &'a str,
    /// Checkout path.
    pub path: &'a str,
    /// Configured URL (before `~`/relative resolution).
    pub url: &'a str,
    /// Raw optional flag (`"true"` enables the quiet path).
    pub optional_raw: &'a str,
    /// Sync mode (`"git"`, or the remainder with `|` preserved).
    pub sync: &'a str,
}

/// Split one `OVERLAYS` record into its six logical fields.
pub fn parse_overlay(entry: &str) -> ParsedOverlay<'_> {
    let mut parts = entry.split('|');
    let name = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let url = parts.next().unwrap_or("");
    let _conf = parts.next().unwrap_or("");
    let optional_raw = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let sync = if rest.is_empty() {
        ""
    } else {
        // The remainder starts right after the fifth delimiter. Find
        // that offset by walking five `|` separators; the slice from
        // there is exactly `f|g|...` like the shell `read`.
        let mut seen = 0usize;
        let mut offset = entry.len();
        for (index, byte) in entry.bytes().enumerate() {
            if byte == b'|' {
                seen += 1;
                if seen == 5 {
                    offset = index + 1;
                    break;
                }
            }
        }
        &entry[offset..]
    };
    let sync = if sync.is_empty() { "git" } else { sync };
    ParsedOverlay {
        name,
        path,
        url,
        optional_raw,
        sync,
    }
}

/// One pull-eligible overlay: a `git`-synced record that is active
/// (a live worktree or any configured URL). Borrows the record it
/// was filtered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveOverlay<'a> {
    /// Overlay name for messages.
    pub name: &'a str,
    /// Checkout path.
    pub path: &'a str,
    /// Configured URL.
    pub url: &'a str,
    /// True exactly when the raw flag is `"true"`.
    pub optional: bool,
}

/// Filter `entries` (`OVERLAYS`) to the pull-eligible overlays in
/// declaration order, like the `_active_entries` build in
/// `_pull_overlays`.
pub fn active_overlays(entries: &[String]) -> Vec<ActiveOverlay<'_>> {
    let mut active = Vec::new();
    for entry in entries {
        let parsed = parse_overlay(entry);
        if parsed.sync != "git" {
            continue;
        }
        if !overlay_active(Path::new(parsed.path), parsed.url) {
            continue;
        }
        active.push(ActiveOverlay {
            name: parsed.name,
            path: parsed.path,
            url: parsed.url,
            optional: parsed.optional_raw == "true",
        });
    }
    active
}

/// Shared inputs for the fleet: every pull context the shell reads
/// from globals, plus the raw UI/job/progress spellings so
/// arithmetic failures read exactly like the shell's.
pub struct PullOverlaysInputs<'a> {
    /// Overlay records (`OVERLAYS`).
    pub entries: &'a [String],
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
    /// `DOT_UPDATE_JOBS`: numeric bound, else the CPU count.
    pub update_jobs: Option<&'a str>,
    /// `DOT_REPO_PROGRESS_DONE`: starting completed count.
    pub progress_done: Option<&'a str>,
    /// `DOT_REPO_PROGRESS_TOTAL`: total for progress details.
    pub progress_total: Option<&'a str>,
    /// `DOT_UI_PROGRESS_WIDTH`: bar width, default `"8"`.
    pub bar_width: &'a str,
    /// Palette for `_ui_status` rows.
    pub palette: &'a Palette,
    /// Whether to count UTF-8 characters for cells.
    pub multibyte: bool,
    /// Whether to render ASCII progress glyphs.
    pub ascii: bool,
    /// Candidate validation environment.
    pub candidate: &'a CandidateEnv,
    /// Base checkout for the installed-link restore walk.
    pub base: &'a Base,
    /// Quarantine support (`None` backs everything as user data).
    pub quarantine: Option<QuarantineInputs>,
    /// Overlay records for the restore walk (usually `entries`).
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

/// Outcome of [`pull_overlays`] and [`pull_overlays_serial`]: the
/// worker-fleet tally, the `"<name> <status>"` summaries in
/// declaration order, their comma join (`REPLY`), the final progress
/// count (`DOT_REPO_PROGRESS_DONE`), and the shell return code (1
/// only when worker plumbing itself fails; overlay `failed`
/// statuses still return 0).
pub struct PullOverlaysOutcome {
    /// Per-status counters plus the changed-items accumulator.
    pub tally: PullTally,
    /// `"<name> <status>"` lines in declaration order.
    pub summaries: Vec<String>,
    /// Comma-joined summaries (`REPLY`).
    pub reply: String,
    /// Final completed count (`DOT_REPO_PROGRESS_DONE`).
    pub done: i64,
    /// Shell return code (0, or 1 on worker-plumbing failure).
    pub rc: i32,
}

/// Whether `DOT_QUIET` silences quiet-gated rows.
fn is_quiet_flag(dot_quiet: Option<&str>) -> bool {
    crate::log::is_quiet(dot_quiet)
}

/// Whether `DOT_VERBOSE` enables running/changed/ok rows
/// (arithmetic 1, like the shell).
fn is_verbose(dot_verbose: Option<&str>) -> bool {
    crate::progress_ui::arith_value(dot_verbose.unwrap_or("0")) == Some(1)
}

/// Numeric progress bound, reading malformed input as zero like the
/// shell arithmetic defaults in `update.rs`.
fn progress_number(value: Option<&str>) -> i64 {
    value.and_then(crate::progress_ui::arith_value).unwrap_or(0)
}

/// Job bound from `DOT_UPDATE_JOBS` (numeric, else the CPU count,
/// minimum one). The verbatim shell spelling may carry leading
/// zeros; only the numeric value drives the Rust bound.
fn jobs_bound(raw: Option<&str>) -> usize {
    let text = update_jobs(raw.unwrap_or(""));
    text.parse::<usize>().unwrap_or(1).max(1)
}

/// Build one [`PullOverlayInputs`] from the shared fleet inputs
/// plus the per-overlay identity. The live flag always starts
/// cleared inside workers and serial runs alike (the shell's
/// subshells inherit a cleared live line under pipes, and the
/// parent owns its own flag).
fn overlay_inputs<'a>(
    inputs: &'a PullOverlaysInputs<'a>,
    active: &ActiveOverlay<'a>,
) -> PullOverlayInputs<'a> {
    PullOverlayInputs {
        name: active.name,
        path: active.path,
        url: active.url,
        optional: active.optional,
        extra_args: inputs.extra_args,
        home: inputs.home,
        ui_total: inputs.ui_total,
        dot_quiet: inputs.dot_quiet,
        dot_verbose: inputs.dot_verbose,
        palette: inputs.palette,
        live_active: false,
        multibyte: inputs.multibyte,
        candidate: inputs.candidate,
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
        log: inputs.log,
    }
}

/// Scratch directory for worker result files
/// (`${TMPDIR:-/tmp}/dot.XXXXXX` semantics): a unique leaf under
/// the system temp dir, `None` when nothing is creatable — the
/// shell's failed `_dot_cleanup_mktemp -d` plus serial fallback.
fn alloc_result_dir() -> Option<PathBuf> {
    let root = std::env::temp_dir();
    for _ in 0..100 {
        let serial = FLEET_COUNTER.fetch_add(1, Ordering::Relaxed);
        let leaf = format!("dot.{}.{serial:016x}", std::process::id());
        let candidate = root.join(leaf);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Some(candidate),
            Err(_) => continue,
        }
    }
    None
}

/// `_pull_overlay_capture`: run one overlay through [`pull_overlay`]
/// with both streams combined into `<prefix>.log` (like `>log
/// 2>&1`), then record `<prefix>.rc` and `<prefix>.status` with the
/// shell's `printf '%s'` (no newline) spellings. The prefix is
/// [`result_prefix`] (`<dir>/<idx %03d>`). Returns the pull status
/// word and rc for convenience; the files remain the contract the
/// ordered replay reads.
pub fn overlay_capture(
    idx: i64,
    result_dir: &Path,
    active: &ActiveOverlay<'_>,
    inputs: &PullOverlaysInputs<'_>,
    moves: &mut MoveCache,
) -> (String, i32) {
    let prefix = result_prefix(&result_dir.to_string_lossy(), idx);
    let log_path = PathBuf::from(format!("{prefix}.log"));
    let rc_path = PathBuf::from(format!("{prefix}.rc"));
    let status_path = PathBuf::from(format!("{prefix}.status"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(&log_path);
    let mut out_file;
    let mut err_file;
    match file {
        Ok(file) => match file.try_clone() {
            Ok(clone) => {
                out_file = file;
                err_file = clone;
            }
            Err(_) => {
                let _ = std::fs::write(&rc_path, "1");
                let _ = std::fs::write(&status_path, "");
                return (String::new(), 1);
            }
        },
        Err(_) => {
            let _ = std::fs::write(&rc_path, "1");
            let _ = std::fs::write(&status_path, "");
            return (String::new(), 1);
        }
    }
    let single = overlay_inputs(inputs, active);
    let outcome = pull_overlay(&single, moves, &mut out_file, &mut err_file);
    drop(out_file);
    drop(err_file);
    let _ = std::fs::write(&rc_path, outcome.rc.to_string());
    let _ = std::fs::write(&status_path, outcome.status.as_str());
    (outcome.status.as_str().to_string(), outcome.rc)
}

/// `_pull_overlay_drain_workers`: replay every non-empty
/// `<dir>/*.log` to `out` in sorted order, then remove the scratch
/// directory best-effort (`|| true`). Thread joins own the shell's
/// `wait`/unregister step (there are no PIDs in Rust), so only the
/// result directory plus the log replay live here.
pub fn drain_result_dir(result_dir: &Path, out: &mut dyn Write) {
    let mut logs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(result_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("log") {
                logs.push(path);
            }
        }
    }
    logs.sort();
    for log in &logs {
        if let Ok(meta) = std::fs::metadata(log) {
            if meta.len() == 0 {
                continue;
            }
        } else {
            continue;
        }
        if let Ok(bytes) = std::fs::read(log) {
            let _ = out.write_all(&bytes);
        }
    }
    let mut cleanup = Registry::new();
    let _ = cleanup.remove_path(result_dir);
}

/// Best-effort scratch removal, like `|| true`.
fn remove_scratch(path: &Path) {
    let mut cleanup = Registry::new();
    let _ = cleanup.remove_path(path);
}

/// Read one worker result: the status word plus rc, mapping an
/// empty status with a nonzero rc to `failed` (the shell's
/// coordinator fallback). Missing files read as `("", 1)` so the
/// caller fails closed like a failed `wait`.
fn read_worker_result(prefix: &str) -> (String, i32) {
    let status = std::fs::read_to_string(format!("{prefix}.status")).unwrap_or_default();
    let rc_text = std::fs::read_to_string(format!("{prefix}.rc")).unwrap_or_default();
    let rc = rc_text.trim().parse::<i32>().unwrap_or(1);
    if status.is_empty() && rc != 0 {
        return ("failed".to_string(), rc);
    }
    (status, rc)
}

/// Comma-join summaries, like `_join_comma`.
fn join_summaries(summaries: &[String]) -> String {
    let refs: Vec<&[u8]> = summaries.iter().map(|line| line.as_bytes()).collect();
    String::from_utf8_lossy(&join_comma(&refs)).into_owned()
}

/// `_pull_overlays_serial`: pull each active overlay in declaration
/// order straight to `out`/`warnings`, bumping `done` and the tally
/// per overlay. Progress details render through `stage` only for a
/// positive total with an unset-or-zero verbose flag (silent under
/// pipes, like the shell).
pub fn pull_overlays_serial(
    inputs: &PullOverlaysInputs<'_>,
    stage: &mut Stage,
    moves: &mut MoveCache,
    out: &mut dyn Write,
    warnings: &mut dyn Write,
) -> PullOverlaysOutcome {
    let active = active_overlays(inputs.entries);
    let total = progress_number(inputs.progress_total);
    let mut done = progress_number(inputs.progress_done);
    let mut tally = PullTally::default();
    let mut summaries: Vec<String> = Vec::new();
    for entry in &active {
        done += 1;
        // The shell gates on `DOT_UI_TOTAL > 0` and `DOT_VERBOSE ==
        // 0` before `_ui_stage_update`; `Stage::maybe_progress`
        // owns exactly that gate (quiet included, silent under
        // pipes like the shell).
        let rendered = stage.maybe_progress(
            entry.name.as_bytes(),
            done,
            total,
            0,
            inputs.dot_verbose,
            inputs.bar_width,
        );
        let _ = out.write_all(&rendered);
        let single = overlay_inputs(inputs, entry);
        let outcome = pull_overlay(&single, moves, out, warnings);
        let status = outcome.status.as_str();
        if let Some(line) = record_status(entry.name, status, &mut tally) {
            summaries.push(line);
        }
    }
    let reply = join_summaries(&summaries);
    PullOverlaysOutcome {
        tally,
        summaries,
        reply,
        done,
        rc: 0,
    }
}

/// Run one chunk of workers in parallel under a scoped thread
/// borrow, each with its own [`MoveCache`] and its own indexed
/// result files. A panicking worker leaves its status file missing,
/// which the ordered replay reads as a plumbing failure.
fn run_chunk(
    chunk: &[ActiveOverlay<'_>],
    base_idx: i64,
    result_dir: &Path,
    inputs: &PullOverlaysInputs<'_>,
) {
    std::thread::scope(|scope| {
        for (offset, entry) in chunk.iter().enumerate() {
            let idx = base_idx + offset as i64 + 1;
            let dir = result_dir;
            let item = *entry;
            scope.spawn(move || {
                let mut moves = MoveCache::default();
                // Each thread builds its own borrowed overlay inputs
                // from the shared fleet inputs; the scope guarantees
                // the borrows outlive the workers.
                let single = PullOverlayInputs {
                    name: item.name,
                    path: item.path,
                    url: item.url,
                    optional: item.optional,
                    extra_args: inputs.extra_args,
                    home: inputs.home,
                    ui_total: inputs.ui_total,
                    dot_quiet: inputs.dot_quiet,
                    dot_verbose: inputs.dot_verbose,
                    palette: inputs.palette,
                    live_active: false,
                    multibyte: inputs.multibyte,
                    candidate: inputs.candidate,
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
                    log: inputs.log,
                };
                let prefix = result_prefix(&dir.to_string_lossy(), idx);
                let log_path = PathBuf::from(format!("{prefix}.log"));
                let rc_path = PathBuf::from(format!("{prefix}.rc"));
                let status_path = PathBuf::from(format!("{prefix}.status"));
                let opened = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(&log_path);
                let Ok(file) = opened else {
                    let _ = std::fs::write(&rc_path, "1");
                    let _ = std::fs::write(&status_path, "");
                    return;
                };
                let Ok(clone) = file.try_clone() else {
                    let _ = std::fs::write(&rc_path, "1");
                    let _ = std::fs::write(&status_path, "");
                    return;
                };
                let mut out_file = file;
                let mut err_file = clone;
                let outcome = pull_overlay(&single, &mut moves, &mut out_file, &mut err_file);
                drop(out_file);
                drop(err_file);
                let _ = std::fs::write(&rc_path, outcome.rc.to_string());
                let _ = std::fs::write(&status_path, outcome.status.as_str());
            });
        }
    });
}

/// `_pull_overlays`: fan the active overlays out within the job
/// bound with parent-owned scratch files, then replay declaration
/// order for stable UI and tally the structured statuses. Falls
/// back to [`pull_overlays_serial`] when scratch allocation fails.
/// Progress bumps render through `stage` during the launch pass;
/// worker logs replay to `out` after the joins (worker warnings
/// ride the logs to stdout, like the shell's `cat`). Returns 1
/// only when worker plumbing itself fails (missing rc/status
/// files); overlay `failed` statuses still return 0.
pub fn pull_overlays(
    inputs: &PullOverlaysInputs<'_>,
    stage: &mut Stage,
    moves: &mut MoveCache,
    out: &mut dyn Write,
    warnings: &mut dyn Write,
) -> PullOverlaysOutcome {
    let active = active_overlays(inputs.entries);
    let total = progress_number(inputs.progress_total);
    let mut done = progress_number(inputs.progress_done);
    if active.is_empty() {
        return PullOverlaysOutcome {
            tally: PullTally::default(),
            summaries: Vec::new(),
            reply: String::new(),
            done,
            rc: 0,
        };
    }
    let Some(result_dir) = alloc_result_dir() else {
        return pull_overlays_serial(inputs, stage, moves, out, warnings);
    };
    let bound = jobs_bound(inputs.update_jobs).max(1);
    // Launch pass: bump progress per entry in declaration order,
    // exactly like the shell's pre-fork `_done`/`_dot_maybe_stage_progress`.
    for entry in &active {
        done += 1;
        let rendered = stage.maybe_progress(
            entry.name.as_bytes(),
            done,
            total,
            0,
            inputs.dot_verbose,
            inputs.bar_width,
        );
        let _ = out.write_all(&rendered);
    }
    let final_done = done;
    // Parallel pass in bound-sized chunks (the shell slides one
    // worker at a time; chunks keep at most `bound` in flight with
    // the same ordered replay and the same cap).
    let mut base_idx: i64 = 0;
    for chunk in active.chunks(bound) {
        run_chunk(chunk, base_idx, &result_dir, inputs);
        base_idx += chunk.len() as i64;
    }
    // Ordered replay: collect non-empty logs plus structured
    // statuses first, then emit once. Buffering keeps the
    // plumbing-failure path (missing rc/status files, like the
    // shell's failed `wait`) to a single drain replay instead of
    // double-printing what the loop already emitted.
    let mut tally = PullTally::default();
    let mut summaries: Vec<String> = Vec::new();
    let mut ordered_logs: Vec<Vec<u8>> = Vec::with_capacity(active.len());
    let mut plumbing_failed = false;
    for (position, entry) in active.iter().enumerate() {
        let idx = position as i64 + 1;
        let prefix = result_prefix(&result_dir.to_string_lossy(), idx);
        let log_path = PathBuf::from(format!("{prefix}.log"));
        let status_path = PathBuf::from(format!("{prefix}.status"));
        let rc_path = PathBuf::from(format!("{prefix}.rc"));
        let log_bytes = match std::fs::metadata(&log_path) {
            Ok(meta) if meta.len() == 0 => Vec::new(),
            Ok(_) => match std::fs::read(&log_path) {
                Ok(bytes) => bytes,
                Err(_) => {
                    plumbing_failed = true;
                    Vec::new()
                }
            },
            Err(_) => {
                plumbing_failed = true;
                Vec::new()
            }
        };
        ordered_logs.push(log_bytes);
        // A missing status/rc pair is a plumbing failure, not an
        // overlay outcome: the shell's failed `wait` drains and
        // returns 1 the same way.
        if std::fs::metadata(&status_path).is_err() || std::fs::metadata(&rc_path).is_err() {
            plumbing_failed = true;
        }
        let (status, _rc) = read_worker_result(&prefix);
        if let Some(line) = record_status(entry.name, &status, &mut tally) {
            summaries.push(line);
        }
    }
    if plumbing_failed {
        drain_result_dir(&result_dir, out);
        let reply = join_summaries(&summaries);
        return PullOverlaysOutcome {
            tally,
            summaries,
            reply,
            done: final_done,
            rc: 1,
        };
    }
    for bytes in &ordered_logs {
        let _ = out.write_all(bytes);
    }
    remove_scratch(&result_dir);
    let reply = join_summaries(&summaries);
    PullOverlaysOutcome {
        tally,
        summaries,
        reply,
        done: final_done,
        rc: 0,
    }
}

/// `_repo_pull_all` status (`ok`/`changed`/`failed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoPullStatus {
    /// Every repo is current or skipped.
    Ok,
    /// At least one repo moved.
    Changed,
    /// At least one repo failed.
    Failed,
}

impl RepoPullStatus {
    /// The `_ui_stage_finish` status word.
    pub fn as_str(&self) -> &'static str {
        match self {
            RepoPullStatus::Ok => "ok",
            RepoPullStatus::Changed => "changed",
            RepoPullStatus::Failed => "failed",
        }
    }
}

/// Inputs for [`pull_all`]: the synchronized repo set (base plus
/// `OVERLAYS`), the pull flags, and the deferred-stage switch. Raw
/// `DOT_*` spellings ride through so arithmetic failures read
/// exactly like the shell's.
pub struct PullAllInputs<'a> {
    /// Overlay records (`OVERLAYS`).
    pub entries: &'a [String],
    /// Extra git pull arguments after each upstream.
    pub extra_args: &'a [OsString],
    /// Client `$HOME`: backup parent and base work tree.
    pub home: &'a str,
    /// `DOT_QUIET`: quiet pulls and silent stages at arithmetic 1.
    pub dot_quiet: Option<&'a str>,
    /// `DOT_VERBOSE`: stage details and pull rows at arithmetic 1.
    pub dot_verbose: Option<&'a str>,
    /// `DOT_UI_TOTAL`: counted UI takes status rows when `> 0`.
    pub ui_total: Option<&'a str>,
    /// `DOT_UPDATE_JOBS`: overlay fan-out bound.
    pub update_jobs: Option<&'a str>,
    /// `DOT_UI_PROGRESS_WIDTH`: bar width, default `"8"`.
    pub bar_width: &'a str,
    /// `DOT_PULL_DEFER_FINISH`: literal `"1"` defers the stage
    /// finish to the update orchestrator.
    pub defer_finish: Option<&'a str>,
    /// Palette for stage and status rows.
    pub palette: &'a Palette,
    /// Whether to count UTF-8 characters for cells.
    pub multibyte: bool,
    /// Whether to render ASCII progress glyphs.
    pub ascii: bool,
    /// Candidate validation environment.
    pub candidate: &'a CandidateEnv,
    /// Base checkout.
    pub base: &'a Base,
    /// Quarantine support (`None` backs everything as user data).
    pub quarantine: Option<QuarantineInputs>,
    /// Overlay records for the restore walk (usually `entries`).
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

/// Outcome of [`pull_all`]: the aggregated status, the shell return
/// code (0 unless failed), the stage summary (empty when deferred),
/// the per-repo tallies, and the changed-item notes. Deferred runs
/// skip the stage finish and report the aggregation for the update
/// orchestrator instead.
pub struct PullAllOutcome {
    /// Aggregated `ok`/`changed`/`failed`.
    pub status: RepoPullStatus,
    /// Shell return code (0 unless failed).
    pub rc: i32,
    /// Comma-joined stage summary (empty when deferred).
    pub summary: String,
    /// Changed-item notes (`dotfiles updated`, `<name> dotfiles
    /// updated|cloned`) in pull order.
    pub changed_items: Vec<String>,
    /// Repos already current.
    pub current: i64,
    /// Repos that moved (base plus overlay `changed`/`cloned`).
    pub changed: i64,
    /// Repos that failed (plus one when the overlay fan-out itself
    /// fails plumbing).
    pub failed: i64,
    /// Repos skipped (no upstream).
    pub skipped: i64,
    /// True when the stage finish stays deferred.
    pub deferred: bool,
}

/// `_repo_pull_all`: pull the base first, then the pull-eligible
/// overlays, aggregating tallies into one stage. `stage` renders
/// the `Repos` open/close (or only the open when deferred).
/// `now_secs` pins the elapsed stamps (tests pass matching
/// clocks; production passes its own now/started pair).
pub fn pull_all(
    inputs: &PullAllInputs<'_>,
    stage: &mut Stage,
    moves: &mut MoveCache,
    out: &mut dyn Write,
    warnings: &mut dyn Write,
    now_secs: i64,
) -> PullAllOutcome {
    let quiet = is_quiet_flag(inputs.dot_quiet);
    let verbose = is_verbose(inputs.dot_verbose);
    // The shell owns repository synchronization only through this
    // entry: config, filtering, then base plus overlays.
    let prefix = inputs.base.git_prefix();
    ensure_repo_config(prefix.as_deref());
    normalize_filtered(prefix.as_deref(), inputs.overlays);
    // `_unstash_overlay_overrides` stays shell (see module docs);
    // rows without replacement records succeed silently on both
    // sides, which is exactly the covered path.
    let overlay_total = active_overlays(inputs.entries).len() as i64;
    let repo_total = 1 + overlay_total;
    let detail = if inputs
        .dot_verbose
        .is_none_or(|text| crate::progress_ui::arith_value(text).is_some_and(|value| value == 0))
    {
        progress_detail(
            b"dotfiles",
            1,
            repo_total,
            inputs.bar_width,
            inputs.ascii,
            inputs.multibyte,
        )
    } else {
        b"pulling repositories".to_vec()
    };
    let start_bytes = stage.start(b"Repos", Some(&detail), now_secs, inputs.dot_verbose);
    let _ = out.write_all(&start_bytes);
    let mut current: i64 = 0;
    let mut changed: i64 = 0;
    let mut failed: i64 = 0;
    let mut skipped: i64 = 0;
    let mut changed_items: Vec<String> = Vec::new();
    if verbose {
        let (bytes, _) = crate::progress_ui::status(
            inputs.palette,
            quiet,
            false,
            b"running",
            b"dotfiles: pulling",
            inputs.multibyte,
        );
        let _ = out.write_all(&bytes);
    }
    let base_inputs = PullBaseInputs {
        base: inputs.base,
        candidate: inputs.candidate,
        quarantine: inputs.quarantine.clone(),
        overlays: inputs.overlays,
        dest: inputs.dest,
        manifest: inputs.manifest,
        legacy_manifest: inputs.legacy_manifest,
        euid: inputs.euid,
        source_root: inputs.source_root,
        tmp: inputs.tmp,
        tool: inputs.tool,
        extra_args: inputs.extra_args,
        quiet,
        verbose,
        log: inputs.log,
    };
    let base_outcome = pull_base(&base_inputs, moves, out, warnings);
    match base_outcome.status.as_str() {
        "skipped" => {
            if verbose {
                let (bytes, _) = crate::progress_ui::status(
                    inputs.palette,
                    quiet,
                    false,
                    b"skipped",
                    b"dotfiles pull skipped (no upstream)",
                    inputs.multibyte,
                );
                let _ = out.write_all(&bytes);
            }
            skipped += 1;
        }
        "changed" => {
            if verbose {
                let (bytes, _) = crate::progress_ui::status(
                    inputs.palette,
                    quiet,
                    false,
                    b"changed",
                    b"dotfiles updated",
                    inputs.multibyte,
                );
                let _ = out.write_all(&bytes);
            }
            changed += 1;
            changed_items.push("dotfiles updated".to_string());
        }
        "failed" => {
            // The shell warns without tallying when quiet and the
            // status is not `blocked`; `PullStatus` never emits
            // `blocked`, so every quiet base failure lands in the
            // warn-only branch and every loud one tallies failed.
            if quiet {
                inputs.log.warn(warnings, "  warning: dotfiles pull failed");
            } else {
                if verbose {
                    let (bytes, _) = crate::progress_ui::status(
                        inputs.palette,
                        quiet,
                        false,
                        b"failed",
                        b"dotfiles: pull failed",
                        inputs.multibyte,
                    );
                    let _ = out.write_all(&bytes);
                }
                failed += 1;
            }
        }
        _ => {
            if verbose {
                let (bytes, _) = crate::progress_ui::status(
                    inputs.palette,
                    quiet,
                    false,
                    b"ok",
                    b"dotfiles current",
                    inputs.multibyte,
                );
                let _ = out.write_all(&bytes);
            }
            current += 1;
        }
    }
    // Seed the overlay progress with the already-rendered base so
    // non-verbose live progress stays on the same dashboard row.
    let done_text = "1".to_string();
    let total_text = repo_total.to_string();
    let overlay_inputs = PullOverlaysInputs {
        entries: inputs.entries,
        extra_args: inputs.extra_args,
        home: inputs.home,
        ui_total: inputs.ui_total,
        dot_quiet: inputs.dot_quiet,
        dot_verbose: inputs.dot_verbose,
        update_jobs: inputs.update_jobs,
        progress_done: Some(&done_text),
        progress_total: Some(&total_text),
        bar_width: inputs.bar_width,
        palette: inputs.palette,
        multibyte: inputs.multibyte,
        ascii: inputs.ascii,
        candidate: inputs.candidate,
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
        log: inputs.log,
    };
    let overlays_outcome = pull_overlays(&overlay_inputs, stage, moves, out, warnings);
    current += overlays_outcome.tally.current as i64;
    changed += overlays_outcome.tally.changed as i64;
    failed += overlays_outcome.tally.failed as i64;
    skipped += overlays_outcome.tally.skipped as i64;
    if overlays_outcome.rc != 0 {
        failed += 1;
    }
    for line in overlays_outcome.tally.changed_items.lines() {
        if line.is_empty() {
            continue;
        }
        changed_items.push(line.to_string());
    }
    let mut status = RepoPullStatus::Ok;
    if failed != 0 {
        status = RepoPullStatus::Failed;
    } else if changed > 0 {
        status = RepoPullStatus::Changed;
    }
    if inputs.defer_finish == Some("1") {
        let rc = i32::from(status == RepoPullStatus::Failed);
        return PullAllOutcome {
            status,
            rc,
            summary: String::new(),
            changed_items,
            current,
            changed,
            failed,
            skipped,
            deferred: true,
        };
    }
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if changed > 0 {
        let mut part = count_phrase(changed, b"repo", Some(b"repos".as_slice()));
        part.extend_from_slice(b" changed");
        parts.push(part);
    }
    if current > 0 || (failed == 0 && skipped == 0) {
        let mut part = count_phrase(current, b"repo", Some(b"repos".as_slice()));
        part.extend_from_slice(b" current");
        parts.push(part);
    }
    if failed > 0 {
        let mut part = count_phrase(failed, b"repo", Some(b"repos".as_slice()));
        part.extend_from_slice(b" failed");
        parts.push(part);
    }
    if skipped > 0 {
        let mut part = count_phrase(skipped, b"repo", Some(b"repos".as_slice()));
        part.extend_from_slice(b" skipped");
        parts.push(part);
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let summary = String::from_utf8_lossy(&join_comma(&refs)).into_owned();
    let finish_bytes = stage.finish(status.as_str().as_bytes(), summary.as_bytes(), now_secs);
    let _ = out.write_all(&finish_bytes);
    if !verbose {
        for item in &changed_items {
            let note = stage.note(b"changed", item.as_bytes());
            let _ = out.write_all(&note);
        }
    }
    let rc = i32::from(status == RepoPullStatus::Failed);
    PullAllOutcome {
        status,
        rc,
        summary,
        changed_items,
        current,
        changed,
        failed,
        skipped,
        deferred: false,
    }
}
