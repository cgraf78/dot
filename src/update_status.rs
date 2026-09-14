//! Cron observability state under `$XDG_STATE_HOME/dot`.
//!
//! Handoff findings #1 (cron dirty-skip observability) and #6 (quiet
//! runner status plus retained logs) share one state layout, owned
//! here so writers (the update engine, the quiet runner) and readers
//! (`dot doctor`) cannot drift:
//!
//! - `update.log`: one outcome line per cron run, appended:
//!   `<epoch> <outcome> <stage>[ <detail>]` with `outcome` one of
//!   `ok`, `fail`, or `skip`. `skip` lines carry the capped dirty
//!   file list as their detail; `ok`/`fail` lines name the `update`
//!   stage. A provider re-exec performs two engine runs, so one cron
//!   invocation can append two lines.
//! - `update.last-success`: the epoch of the last successful cron
//!   run, overwritten. `dot doctor` warns when the stamp is older
//!   than [`CRON_STALE_AFTER_SECS`].
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

/// Largest `update.last-success` body accepted: a valid stamp is an
/// ASCII epoch plus newline (at most 21 bytes), so anything past
/// this is corrupt, never a stamp.
const STAMP_MAX_BYTES: u64 = 64;

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
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let path = last_success_path(state_home);
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
    let Some(mut file) = open_state_file(&options, &path) else {
        return;
    };
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    let _ = file.write_all(format!("{now}\n").as_bytes());
}

/// Read the last-success stamp: the trimmed file content as epoch
/// seconds, or `None` when missing, unreadable, oversized, or
/// malformed. Never blocks on a pre-planted FIFO.
pub fn read_last_success(state_home: &Path) -> Option<i64> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NONBLOCK);
    let file = open_state_file(&options, &last_success_path(state_home))?;
    let mut content = String::new();
    file.take(STAMP_MAX_BYTES + 1)
        .read_to_string(&mut content)
        .ok()?;
    if content.len() as u64 > STAMP_MAX_BYTES {
        return None;
    }
    content.trim().parse::<i64>().ok()
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
