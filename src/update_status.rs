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
//!   failing stages ([`Degraded::detail`], e.g. `tools,prune`). A
//!   provider re-exec performs two engine runs, so one cron
//!   invocation can append two lines (both carry the continuation's
//!   classification).
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
//!   converging while Tools or Prune fails from one that stopped
//!   converging; stamps older than [`CRON_STALE_AFTER_SECS`] read as
//!   not converging. Older Dot releases neither write nor read it, so
//!   after a downgrade the stamp only ages out.
//! - `logs/`: retained failure logs from the quiet runner, pruned to
//!   the newest [`MAX_RETAINED_LOGS`].
//!
//! Every writer is best-effort: observability must never fail an
//! update or a command, so filesystem errors are swallowed and
//! callers keep their historical exit codes. Non-cron updates write
//! nothing here: the history-tree tests pin the state directory
//! across plain `update` runs, and the stamp deliberately measures
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

/// Stage names persisted in `degraded` outcome lines and the
/// convergence stamp. Stable vocabulary: `dot doctor` renders them.
pub const STAGE_TOOLS: &str = "tools";
/// See [`STAGE_TOOLS`].
pub const STAGE_PRUNE: &str = "prune";

/// Stages whose failure leaves a cron run converged but degraded:
/// the dotfiles themselves (repositories, links, configs) are current,
/// but dependency convergence (Tools) or orphan removal (Prune) failed.
/// Any other failure means the run did not converge and records `fail`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Degraded {
    /// The Tools stage failed (provider unavailable, a dependency, or a
    /// post hook).
    pub tools: bool,
    /// The Prune stage ran and failed.
    pub prune: bool,
}

impl Degraded {
    /// True when no degradable stage failed.
    pub fn is_empty(self) -> bool {
        !self.tools && !self.prune
    }

    /// The failing stages, comma-joined in run order (`tools,prune`);
    /// empty when none failed. Persisted as-is.
    pub fn detail(self) -> String {
        [(self.tools, STAGE_TOOLS), (self.prune, STAGE_PRUNE)]
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
