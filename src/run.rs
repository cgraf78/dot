//! Command execution from `lib/dot/run.sh`: scratch log
//! allocation, the tick-while-running executor, and the quiet
//! logged runner.
//!
//! The live executor runs the command on a worker thread and ticks
//! the stage from the caller until it finishes, like the shell's
//! background subshell plus status-file poll. Command output lands
//! in the log through one shared-offset handle pair, exactly like
//! `>"$log" 2>&1`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::log::Log;
use crate::progress_ui::Stage;

/// Allocation counter behind [`logfile_create`].
static LOG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Scratch directory for allocated logs (`${TMPDIR:-/tmp}`).
fn log_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `_logfile_create`: allocate an empty scratch log. `None` mirrors
/// the silenced mktemp failure (with `REPLY` cleared).
pub fn logfile_create() -> Option<PathBuf> {
    for _ in 0..100 {
        let serial = LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = log_dir().join(format!("dot.{}.{serial:016x}.log", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => return Some(path),
            Err(_) => continue,
        }
    }
    None
}

/// `_logfile_print`: warn with the labeled, indented log. Missing
/// or empty logs stay silent, like the `-n`/`-s` gate. Indentation
/// prefixes every newline-terminated line plus any trailing
/// partial, exactly like `sed 's/^/    /'`.
pub fn logfile_print(log: &Log, warnings: &mut dyn std::io::Write, label: &str, path: &Path) {
    if path.as_os_str().is_empty() {
        return;
    }
    let content = match std::fs::read(path) {
        Ok(content) if !content.is_empty() => content,
        _ => return,
    };
    log.warn(warnings, &format!("  {label} output:"));
    for chunk in content.split_inclusive(|byte| *byte == b'\n') {
        let _ = warnings.write_all(b"    ");
        let _ = warnings.write_all(chunk);
    }
}

/// Minimum live-tick quantum in seconds: a zero or negative
/// `$DOT_UI_TICK_SECONDS` must render heartbeats at this rate, never
/// busy-spin `recv_timeout(ZERO)` at 100% CPU.
const MIN_TICK_QUANTUM_SECS: f64 = 0.1;

/// Live ticking for [`run_to_log_with_ticks`]: the stage renders on
/// `out` every `tick_seconds` while the command runs.
pub struct Live<'a> {
    /// Stage rendering heartbeat lines.
    pub stage: &'a mut Stage,
    /// Tick output sink (the shell's stdout).
    pub out: &'a mut dyn std::io::Write,
    /// Poll interval (`$DOT_UI_TICK_SECONDS`).
    pub tick_seconds: f64,
}

/// Spawn `argv` with both streams sharing one log handle pair.
/// Empty argv runs redirections only (success), like `"$@"` with
/// nothing to expand to. Spawn failure reads 127, like a missing
/// command.
fn run_to_file(log: &Path, argv: &[OsString]) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(log);
        return 0;
    };
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(log)
    {
        Ok(file) => file,
        Err(_) => return 127,
    };
    let stream = match file.try_clone() {
        Ok(stream) => stream,
        Err(_) => return 127,
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stream));
    crate::cleanup::run_session_status(command, crate::cleanup::LingerPolicy::Strict)
}

/// Current epoch seconds for heartbeat stamps.
fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// `_run_to_log_with_ticks`: run `argv` into `log`, ticking the
/// stage while it runs when `live` is present. Returns the command
/// exit code (1 when the worker vanishes without reporting, like
/// the shell's unreadable status file).
pub fn run_to_log_with_ticks(log: &Path, argv: &[OsString], live: Option<Live<'_>>) -> i32 {
    let Some(live) = live else {
        return run_to_file(log, argv);
    };
    let Live {
        stage,
        out,
        tick_seconds,
    } = live;
    let tick = Duration::from_secs_f64(tick_seconds.max(MIN_TICK_QUANTUM_SECS));
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let rc = run_to_file(log, argv);
            let _ = done_tx.send(rc);
        });
        loop {
            match done_rx.recv_timeout(tick) {
                Ok(rc) => return rc,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let rendered = stage.tick(epoch_secs());
                    let _ = out.write_all(&rendered);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return 1,
            }
        }
    })
}

/// Null-running helper behind the logless fallback: the exit code
/// of `argv` with both streams discarded (127 when spawning fails).
fn run_to_null(argv: &[OsString]) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        return 0;
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::cleanup::run_session_status(command, crate::cleanup::LingerPolicy::Strict)
}

/// `_run_quiet_logged`: run `argv`, warning with the labeled log on
/// failure. Reports the real command status (handoff finding #6
/// retired the shell's fixed `return 0`; the caller audit in
/// `tests/run.rs` pins the zero-production-caller precondition).
/// Without a scratch log the command runs silently and only a
/// nonzero exit warns. A failing run retains its log under
/// `retain_dir` (the [`crate::update_status::logs_dir`] layout)
/// with bounded rotation; `None` keeps the historical
/// delete-always behavior. Successful runs always clean up.
pub fn run_quiet_logged(
    log: &Log,
    warnings: &mut dyn std::io::Write,
    label: &str,
    warning: &str,
    argv: &[OsString],
    retain_dir: Option<&Path>,
) -> i32 {
    let Some(path) = logfile_create() else {
        let rc = run_to_null(argv);
        if rc != 0 {
            log.warn(warnings, &format!("  warning: {warning}"));
        }
        return rc;
    };
    let rc = run_to_log_with_ticks(&path, argv, None);
    if rc == 0 {
        std::fs::remove_file(&path).ok();
        return 0;
    }
    logfile_print(log, warnings, label, &path);
    match retain_dir {
        Some(dir) => {
            if crate::update_status::retain_failed_log(dir, label, epoch_secs(), &path).is_none() {
                std::fs::remove_file(&path).ok();
            }
        }
        None => {
            std::fs::remove_file(&path).ok();
        }
    }
    log.warn(warnings, &format!("  warning: {warning}"));
    rc
}
