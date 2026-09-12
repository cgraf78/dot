//! Native coordinator for the external Shdeps provider.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::Runtime;

/// Inputs retained across provider selection, bootstrap, ABI validation, and update.
pub(crate) struct Inputs<'a> {
    pub(crate) runtime: &'a Runtime,
    pub(crate) source_root: &'a Path,
    pub(crate) home: &'a str,
    pub(crate) config_home: &'a str,
    pub(crate) state_home: &'a str,
    pub(crate) policy: &'a str,
    pub(crate) force: bool,
    pub(crate) quiet: bool,
    pub(crate) verbose: bool,
    pub(crate) update_jobs: Option<&'a str>,
    pub(crate) palette: &'a crate::progress_ui::Palette,
    pub(crate) multibyte: bool,
    pub(crate) ascii: bool,
    pub(crate) bar_width: &'a str,
}

/// Provider update output consumed by Dot's owning update stage.
pub(crate) struct Outcome {
    pub(crate) status: i32,
    pub(crate) interrupted: Option<i32>,
    pub(crate) abort: bool,
    pub(crate) stage_status: Vec<u8>,
    pub(crate) summary: Vec<u8>,
    pub(crate) details: Vec<u8>,
    pub(crate) revision_change: Option<(String, String)>,
}

#[derive(Clone, Copy)]
enum Source {
    Explicit,
    PinnedDevelopment,
    LatestDevelopment,
    Managed,
    Downloaded,
}

struct Installer {
    path: PathBuf,
    source: Source,
    temporary: bool,
}

struct Ready {
    binary: PathBuf,
    directory: PathBuf,
    env: BTreeMap<OsString, OsString>,
    _snapshot: Option<ProviderSnapshot>,
}

/// One private executable copy used for every validation and execution step.
///
/// Provider installation paths are mutable: an atomic rename after the final
/// capability probe must not change the bytes later executed by `update`.
/// Keeping this directory alive in `Ready` binds ABI, capabilities, and the
/// update invocation to one opened source-file snapshot.
struct ProviderSnapshot {
    directory: PathBuf,
    binary: PathBuf,
}

impl Drop for ProviderSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.binary);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Successfully prepared provider command and its sanitized execution state.
pub(crate) struct Prepared(Ready);

/// Failure metadata emitted before the provider's Tools stage can begin.
pub(crate) struct PrepareFailure {
    pub(crate) interrupted: Option<i32>,
    pub(crate) abort: bool,
    pub(crate) summary: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

struct PromptPipe {
    directory: PathBuf,
    path: PathBuf,
}

const CAPTURE_TICK_BYTES: usize = 64 * 1024;
const CAPTURE_FINAL_BYTES: usize = 1024 * 1024;
const PROVIDER_CAPTURE_LIMIT_BYTES: usize = 1024 * 1024;
const PROVIDER_FRAME_LIMIT_BYTES: usize = 1024 * 1024;
const PROVIDER_RUN_CAPTURE_LIMIT_BYTES: usize = 1024 * 1024;
const PROVIDER_RUN_EVENT_LIMIT: usize = 4096;
const PROVIDER_SNAPSHOT_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const PROVIDER_OUTPUT_LIMIT_ERROR: &str = "Shdeps provider output exceeded its safety limit";
const PROVIDER_DOWNLOAD_TIMEOUT_SECONDS: u64 = 35;
const OWNED_SUBPROCESS_CANCELLATION_CAPABILITY: &str = "owned-subprocess-cancellation-v1";
const PROMPT_FIFO_READER_BEFORE_EVENT_CAPABILITY: &str = "prompt-fifo-reader-before-event-v1";

struct ProviderDrainState {
    remaining_bytes: usize,
    failed: bool,
}

struct LimitedWriter<'a> {
    output: &'a mut dyn std::io::Write,
    remaining: usize,
}

impl<'a> LimitedWriter<'a> {
    fn new(output: &'a mut dyn std::io::Write, limit: usize) -> Self {
        Self {
            output,
            remaining: limit,
        }
    }

    fn remaining(&self) -> usize {
        self.remaining
    }
}

impl std::io::Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(PROVIDER_OUTPUT_LIMIT_ERROR));
        }
        let written = self.output.write(bytes)?;
        self.remaining = self.remaining.saturating_sub(written);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

struct CumulativeWriter<'a> {
    output: &'a mut dyn std::io::Write,
    remaining: &'a mut usize,
}

impl std::io::Write for CumulativeWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > *self.remaining {
            *self.remaining = 0;
            return Err(provider_output_limit());
        }
        let written = self.output.write(bytes)?;
        *self.remaining = self.remaining.saturating_sub(written);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

fn provider_output_limit() -> std::io::Error {
    std::io::Error::other(PROVIDER_OUTPUT_LIMIT_ERROR)
}

fn snapshot_provider(binary: &Path, state_home: &Path) -> std::io::Result<ProviderSnapshot> {
    let mut source = std::fs::File::open(binary)?;
    let before = source.metadata()?;
    if !before.is_file() || before.len() > PROVIDER_SNAPSHOT_LIMIT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "provider executable is not a bounded regular file",
        ));
    }
    std::fs::create_dir_all(state_home)?;
    let mut directory = None;
    for _ in 0..crate::temp::TMP_RETRIES {
        let candidate = state_home.join(format!(
            ".dot-provider-snapshot-{}",
            crate::temp::random_suffix()
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&candidate) {
            Ok(()) => {
                directory = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let directory = directory.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "provider snapshot names keep colliding",
        )
    })?;
    let path = directory.join("provider");
    let copied = (|| {
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)?;
        let copied = std::io::copy(
            &mut std::io::Read::by_ref(&mut source)
                .take(PROVIDER_SNAPSHOT_LIMIT_BYTES.saturating_add(1)),
            &mut target,
        )?;
        target.flush()?;
        target.sync_all()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let after = source.metadata()?;
        if copied != before.len()
            || copied > PROVIDER_SNAPSHOT_LIMIT_BYTES
            || before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(std::io::Error::other(
                "provider executable changed while it was snapshotted",
            ));
        }
        Ok(())
    })();
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&directory);
        return Err(error);
    }
    Ok(ProviderSnapshot {
        directory,
        binary: path,
    })
}

fn is_provider_output_limit(error: &std::io::Error) -> bool {
    error.to_string() == PROVIDER_OUTPUT_LIMIT_ERROR
}

#[derive(Clone, Copy)]
enum RelayTarget {
    Stdout,
    Stderr,
}

enum RelayMessage {
    Bytes(RelayTarget, Vec<u8>),
    Flush(RelayTarget, std::sync::mpsc::Sender<bool>),
}

struct RelaySinkFailure {
    intentional_abort: bool,
}

/// Child-supervision-side output adapter. It only enqueues bounded rendered
/// bytes; an arbitrarily slow caller sink can therefore never hold the thread
/// responsible for TERM/KILL/reap. Flush retains prompt ordering through a
/// cancellable acknowledgment rather than blocking teardown.
struct RelayWriter {
    target: RelayTarget,
    sender: std::sync::mpsc::Sender<RelayMessage>,
    sink_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::io::Write for RelayWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.sink_failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::other("provider output sink failed"));
        }
        self.sender
            .send(RelayMessage::Bytes(self.target, bytes.to_vec()))
            .map_err(|_| std::io::Error::other("provider output relay closed"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.sink_failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::other("provider output sink failed"));
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        self.sender
            .send(RelayMessage::Flush(self.target, sender))
            .map_err(|_| std::io::Error::other("provider output relay closed"))?;
        loop {
            if crate::cleanup::received_signal().is_some()
                || self.sink_failed.load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(std::io::Error::other("provider output relay interrupted"));
            }
            match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(true) => return Ok(()),
                Ok(false) => return Err(std::io::Error::other("provider output sink failed")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::other("provider output relay closed"));
                }
            }
        }
    }
}

fn pump_relay(
    receiver: std::sync::mpsc::Receiver<RelayMessage>,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
    sink_failed: &std::sync::atomic::AtomicBool,
) -> Option<RelaySinkFailure> {
    let mut sink_error = None;
    let mut intentional_abort = false;
    while let Ok(message) = receiver.recv() {
        let delivered = match message {
            RelayMessage::Bytes(RelayTarget::Stdout, bytes) => {
                if sink_error.is_none() {
                    live_out.write_all(&bytes)
                } else {
                    Ok(())
                }
            }
            RelayMessage::Bytes(RelayTarget::Stderr, bytes) => {
                if sink_error.is_none() {
                    live_err.write_all(&bytes)
                } else {
                    Ok(())
                }
            }
            RelayMessage::Flush(target, reply) => {
                let flushed = if sink_error.is_none() {
                    match target {
                        RelayTarget::Stdout => live_out.flush(),
                        RelayTarget::Stderr => live_err.flush(),
                    }
                } else {
                    Ok(())
                };
                let ok = flushed.is_ok() && sink_error.is_none();
                let _ = reply.send(ok);
                flushed
            }
        };
        if let Err(error) = delivered {
            if sink_error.is_none() {
                intentional_abort = crate::cleanup::outward_write_aborted();
                sink_error = Some(error);
                sink_failed.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }
    sink_error.map(|_error| RelaySinkFailure { intentional_abort })
}

/// Error value for a panicking provider worker thread: a panicking
/// supervisor or deadline watcher must read as a failed update,
/// never crash the process (exit 101).
fn join_panic_error(what: &'static str) -> std::io::Error {
    std::io::Error::other(format!("provider {what} thread panicked"))
}

/// Supervise a deadline-bearing provider helper independently of outward
/// output delivery. The package binary's descriptor writer observes the abort
/// latch when the absolute deadline expires; an embedded arbitrary `Write`
/// remains synchronous, but can no longer delay TERM/KILL/reap of the child.
fn supervise_relayed_capture(
    command: Command,
    deadline: std::time::Instant,
    stdout: &mut CaptureStream,
    stderr: &mut CaptureStream,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
    remaining_bytes: &mut usize,
) -> (
    std::io::Result<crate::cleanup::SessionEnd>,
    Option<RelaySinkFailure>,
) {
    let (relay_sender, relay_receiver) = std::sync::mpsc::channel();
    let sink_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let relay_complete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = std::thread::scope(|scope| {
        let supervisor_failure = sink_failed.clone();
        let supervisor = scope.spawn(move || {
            let mut relay_out = RelayWriter {
                target: RelayTarget::Stdout,
                sender: relay_sender.clone(),
                sink_failed: supervisor_failure.clone(),
            };
            let mut relay_err = RelayWriter {
                target: RelayTarget::Stderr,
                sender: relay_sender,
                sink_failed: supervisor_failure,
            };
            let mut relay_failed = false;
            let supervised =
                crate::cleanup::supervise_session(command, Some(deadline), |final_pass| {
                    drain_cumulative_capture_streams(
                        stdout,
                        &mut relay_out,
                        stderr,
                        &mut relay_err,
                        final_pass,
                        remaining_bytes,
                        &mut relay_failed,
                    )
                });
            if std::time::Instant::now() >= deadline
                || matches!(
                    &supervised,
                    Ok(crate::cleanup::SessionEnd::TimedOut)
                        | Ok(crate::cleanup::SessionEnd::CleanupIncomplete)
                        | Err(_)
                )
            {
                crate::cleanup::abort_outward_writes();
            }
            supervised
        });
        let watchdog_complete = relay_complete.clone();
        let watchdog = scope.spawn(move || {
            while !watchdog_complete.load(std::sync::atomic::Ordering::Acquire) {
                let now = std::time::Instant::now();
                if now >= deadline {
                    crate::cleanup::abort_outward_writes();
                    return;
                }
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(now)
                        .min(std::time::Duration::from_millis(10)),
                );
            }
        });
        let sink_error = pump_relay(relay_receiver, live_out, live_err, sink_failed.as_ref());
        relay_complete.store(true, std::sync::atomic::Ordering::Release);
        let supervised = match supervisor.join() {
            Ok(supervised) => supervised,
            Err(_) => Err(join_panic_error("capture supervisor")),
        };
        // The deadline watcher has no error channel: by join time the
        // relay pump already completed (its whole purpose), and the
        // supervisor above aborts outward writes on timeout itself —
        // so a watcher panic is ignored, never a crash.
        let _ = watchdog.join();
        (supervised, sink_error)
    });
    crate::cleanup::resume_outward_writes();
    result
}

struct CaptureStream {
    reader: std::os::unix::net::UnixStream,
    writer: Option<std::os::unix::net::UnixStream>,
}

impl CaptureStream {
    fn new() -> std::io::Result<Self> {
        let (reader, writer) = crate::cleanup::internal_stream_pair()?;
        reader.set_nonblocking(true)?;
        Ok(Self {
            reader,
            writer: Some(writer),
        })
    }

    fn child_stdio(&mut self) -> std::io::Result<Stdio> {
        self.writer
            .take()
            .map(|writer| Stdio::from(OwnedFd::from(writer)))
            .ok_or_else(|| std::io::Error::other("Shdeps capture writer already taken"))
    }

    /// Drain currently available bytes without waiting for EOF. Each polling
    /// pass has a fixed byte budget so an escaped process that writes forever
    /// cannot monopolize the supervisor; dropping the reader after teardown
    /// releases socket backpressure without leaving an unlinked file behind.
    fn drain_snapshot(
        &mut self,
        output: &mut dyn std::io::Write,
        final_pass: bool,
    ) -> std::io::Result<()> {
        self.drain_with_budget(
            output,
            if final_pass {
                CAPTURE_FINAL_BYTES
            } else {
                CAPTURE_TICK_BYTES
            },
        )
    }

    fn drain_with_budget(
        &mut self,
        output: &mut dyn std::io::Write,
        budget: usize,
    ) -> std::io::Result<()> {
        let mut drained = 0;
        let mut output_error = None;
        while drained < budget {
            let mut chunk = [0u8; 8192];
            let available = (budget - drained).min(chunk.len());
            match self.reader.read(&mut chunk[..available]) {
                Ok(0) => break,
                Ok(count) => {
                    if output_error.is_none() {
                        if let Err(error) = output.write_all(&chunk[..count]) {
                            // Never replay a partially delivered chunk. Keep
                            // draining the child-facing socket so cooperative
                            // teardown remains possible, then report the first
                            // sink failure to the caller.
                            output_error = Some(error);
                        }
                    }
                    drained += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // Darwin reports a momentarily exhausted socket buffer as
                // ENOBUFS (raw 55, surfaced as `Uncategorized`), not
                // `WouldBlock`. Treat it like `WouldBlock` (end this pass,
                // retry on the next supervisor tick) instead of aborting
                // the drain; the send side already retries it the same way.
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => break,
                Err(error) => return Err(error),
            }
        }
        match output_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Service a second child-facing stream even when relaying the first one has
/// failed. Once an outward sink is known broken, route the other stream to a
/// sink: attempting another user-facing write could block forever before the
/// supervisor regains control to terminate and reap the child.
fn drain_second_after(
    first: std::io::Result<()>,
    second: &mut CaptureStream,
    second_output: &mut dyn std::io::Write,
    final_pass: bool,
) -> std::io::Result<()> {
    match first {
        Ok(()) => second.drain_snapshot(second_output, final_pass),
        Err(first_error) => {
            let _ = second.drain_snapshot(&mut std::io::sink(), final_pass);
            Err(first_error)
        }
    }
}

/// Relay a raw stdout/stderr pair until either outward sink fails, then remain
/// in drain-only mode for every later supervisor tick. Remembering the failure
/// matters: a later tick may find stdout empty and must not re-enter a blocked
/// stderr sink while the child is trying to finish cooperative teardown.
fn drain_capture_streams(
    first: &mut CaptureStream,
    first_output: &mut dyn std::io::Write,
    second: &mut CaptureStream,
    second_output: &mut dyn std::io::Write,
    final_pass: bool,
    relay_failed: &mut bool,
) -> std::io::Result<()> {
    if *relay_failed {
        let mut sink = std::io::sink();
        let first_result = first.drain_snapshot(&mut sink, final_pass);
        let second_result = second.drain_snapshot(&mut sink, final_pass);
        return first_result.and(second_result);
    }
    let first_result = first.drain_snapshot(first_output, final_pass);
    let result = drain_second_after(first_result, second, second_output, final_pass);
    if result.is_err() {
        *relay_failed = true;
    }
    result
}

/// Relay two preparation streams through one aggregate budget. After either
/// sink or budget fails, keep both child-facing sockets draining to a sink so
/// bounded TERM/KILL teardown cannot deadlock on output backpressure.
fn drain_cumulative_capture_streams(
    first: &mut CaptureStream,
    first_output: &mut dyn std::io::Write,
    second: &mut CaptureStream,
    second_output: &mut dyn std::io::Write,
    final_pass: bool,
    remaining_bytes: &mut usize,
    relay_failed: &mut bool,
) -> std::io::Result<()> {
    if *relay_failed {
        let mut sink = std::io::sink();
        let first_result = first.drain_snapshot(&mut sink, final_pass);
        let second_result = second.drain_snapshot(&mut sink, final_pass);
        return first_result.and(second_result);
    }
    let first_result = drain_cumulative_stream(first, first_output, remaining_bytes, final_pass);
    let result = match first_result {
        Ok(()) => drain_cumulative_stream(second, second_output, remaining_bytes, final_pass),
        Err(first_error) => {
            let _ = second.drain_snapshot(&mut std::io::sink(), final_pass);
            Err(first_error)
        }
    };
    if result.is_err() {
        *relay_failed = true;
    }
    result
}

impl Drop for PromptPipe {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Select, bootstrap, and validate Shdeps without invoking Dot's shell engine.
///
/// Preparation precedes the Tools stage in the shell contract. Its inherited
/// diagnostics therefore write directly to the owning update streams instead
/// of being replayed after the stage has finished.
pub(crate) fn prepare(
    inputs: &Inputs<'_>,
    preparation_stdout: &mut dyn std::io::Write,
    preparation_stderr: &mut dyn std::io::Write,
) -> Result<Prepared, PrepareFailure> {
    if inputs.runtime.is_process_entry() {
        if let Err(error) = crate::cleanup::adopt_descendants() {
            return Err(PrepareFailure {
                interrupted: crate::cleanup::received_signal(),
                abort: true,
                summary: b"dependency update failed".to_vec(),
                stderr: format!("dot: could not enable provider descendant reaping: {error}\n")
                    .into_bytes(),
            });
        }
    }
    match ensure(inputs, preparation_stdout, preparation_stderr) {
        Ok(ready) => Ok(Prepared(ready)),
        Err(EnsureFailure::Interrupted(signal)) => Err(PrepareFailure {
            interrupted: Some(signal),
            abort: true,
            summary: b"dependency update interrupted".to_vec(),
            stderr: Vec::new(),
        }),
        Err(failure) => {
            let name = match &failure {
                EnsureFailure::Unavailable => "Unavailable",
                EnsureFailure::Bash(error) => {
                    eprintln!("TEMP-DIAG-180 ENSURE-FAIL Bash: {error}");
                    "Bash"
                }
                EnsureFailure::AbiTimeout(_) => "AbiTimeout",
                EnsureFailure::CapabilityTimeout(_) => "CapabilityTimeout",
                EnsureFailure::DownloadTimeout(_) => "DownloadTimeout",
                EnsureFailure::Download(DownloadFailure::Transport) => "Download(Transport)",
                EnsureFailure::Download(DownloadFailure::Digest) => "Download(Digest)",
                EnsureFailure::Output => "Output",
                EnsureFailure::Interrupted(_) => "Interrupted",
            };
            eprintln!("TEMP-DIAG-180 ENSURE-FAIL {name}");
            Err(PrepareFailure {
                interrupted: None,
                abort: matches!(
                    failure,
                    EnsureFailure::Output | EnsureFailure::DownloadTimeout(_)
                ),
                summary: b"shdeps unavailable; dependency install skipped".to_vec(),
                stderr: match failure {
                    EnsureFailure::Unavailable => Vec::new(),
                    EnsureFailure::Bash(error) => inputs.runtime.bash_error_line_once(&error),
                    EnsureFailure::AbiTimeout(seconds) => crate::progress_ui::warn_line(
                        inputs.palette,
                        format!("  warning: Shdeps provider ABI probe timed out after {seconds}s")
                            .as_bytes(),
                    ),
                    EnsureFailure::CapabilityTimeout(seconds) => crate::progress_ui::warn_line(
                        inputs.palette,
                        format!(
                            "  warning: Shdeps provider capability probe timed out after {seconds}s"
                        )
                        .as_bytes(),
                    ),
                    EnsureFailure::DownloadTimeout(seconds) => crate::progress_ui::warn_line(
                        inputs.palette,
                        format!("  warning: Shdeps provider download timed out after {seconds}s")
                            .as_bytes(),
                    ),
                    EnsureFailure::Download(kind) => {
                        let mut out = crate::progress_ui::warn_line(
                        inputs.palette,
                        match kind {
                            DownloadFailure::Transport => {
                                b"  warning: Shdeps bootstrap download failed".as_slice()
                            }
                            DownloadFailure::Digest => b"  warning: downloaded Shdeps bootstrap did not match the release digest".as_slice(),
                        },
                    );
                        out.extend_from_slice(&crate::progress_ui::warn_line(
                            inputs.palette,
                            b"  warning: failed to fetch the reviewed Shdeps bootstrap",
                        ));
                        out
                    }
                    EnsureFailure::Output => Vec::new(),
                    EnsureFailure::Interrupted(_) => unreachable!("handled above"),
                },
            })
        }
    }
}

/// Run one prepared Shdeps update and return its rendered result metadata.
pub(crate) fn update(
    inputs: &Inputs<'_>,
    prepared: Prepared,
    stage: &mut crate::progress_ui::Stage,
    now_secs: i64,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> Outcome {
    run_update(inputs, &prepared.0, stage, now_secs, live_out, live_err)
}

enum EnsureFailure {
    Unavailable,
    Bash(crate::bash::Error),
    AbiTimeout(u64),
    CapabilityTimeout(u64),
    DownloadTimeout(u64),
    Download(DownloadFailure),
    Output,
    Interrupted(i32),
}

fn classify_download_deadline(
    end: &std::io::Result<crate::cleanup::SessionEnd>,
    deadline_expired: bool,
    timeout_seconds: u64,
) -> Option<EnsureFailure> {
    match end {
        Ok(crate::cleanup::SessionEnd::TimedOut) => {
            Some(EnsureFailure::DownloadTimeout(timeout_seconds))
        }
        Ok(crate::cleanup::SessionEnd::Exited(_)) if deadline_expired => {
            Some(EnsureFailure::DownloadTimeout(timeout_seconds))
        }
        Err(_) if deadline_expired => Some(EnsureFailure::DownloadTimeout(timeout_seconds)),
        _ => None,
    }
}

fn ensure(
    inputs: &Inputs<'_>,
    preparation_stdout: &mut dyn std::io::Write,
    preparation_stderr: &mut dyn std::io::Write,
) -> Result<Ready, EnsureFailure> {
    let configured =
        crate::shdeps_env_abi::configure_env(&crate::shdeps_env_abi::ConfigureInputs {
            xdg_config_home: inputs.config_home,
            home: inputs.home,
            install_dir: value(inputs.runtime, "SHDEPS_INSTALL_DIR"),
            bin_dir: value(inputs.runtime, "SHDEPS_BIN_DIR"),
            git_dev_dir: value(inputs.runtime, "SHDEPS_GIT_DEV_DIR"),
            dot_force: if inputs.force { "1" } else { "0" },
            dot_quiet: if inputs.quiet { "1" } else { "0" },
        })
        .ok_or(EnsureFailure::Unavailable)?;
    // Bash is a hard prerequisite for every provider source. Resolve it
    // before installer selection so an invalid strict override cannot trigger
    // a download that can never be used.
    let bash = inputs.runtime.bash().map_err(EnsureFailure::Bash)?;
    let selected = match installer(inputs, &configured) {
        Some(selected) => selected,
        None => download_installer(inputs, preparation_stdout, preparation_stderr)?,
    };
    let mut env = inputs.runtime.env().clone();
    set(&mut env, "HOME", inputs.home);
    set(&mut env, "XDG_CONFIG_HOME", inputs.config_home);
    set(&mut env, "XDG_STATE_HOME", inputs.state_home);
    set(&mut env, "SHDEPS_CONF_DIR", &configured.conf_dir);
    set(&mut env, "SHDEPS_HOOKS_DIR", &configured.hooks_dir);
    set(&mut env, "SHDEPS_INSTALL_DIR", &configured.install_dir);
    set(&mut env, "SHDEPS_BIN_DIR", &configured.bin_dir);
    set(&mut env, "SHDEPS_GIT_DEV_DIR", &configured.git_dev_dir);
    if configured.force {
        set(&mut env, "SHDEPS_FORCE", "1");
    }
    if configured.quiet {
        set(&mut env, "SHDEPS_QUIET", "1");
    }
    if inputs.policy == "latest" {
        set(&mut env, "SHDEPS_BOOTSTRAP_FORCE", "1");
    }
    if !matches!(
        selected.source,
        Source::PinnedDevelopment | Source::LatestDevelopment
    ) {
        set(&mut env, "SHDEPS_GIT_DEV_DIR", "/dev/null");
    }
    let binary = bootstrap(
        inputs.runtime,
        &bash,
        &selected.path,
        &env,
        preparation_stderr,
    );
    if selected.temporary {
        let _ = std::fs::remove_file(&selected.path);
    }
    let mut binary = binary?;
    let mut directory = binary
        .parent()
        .ok_or(EnsureFailure::Unavailable)?
        .to_path_buf();
    let mut snapshot = snapshot_provider(&binary, Path::new(inputs.state_home))
        .map_err(|_| EnsureFailure::Unavailable)?;
    let expected =
        crate::shdeps::lock_value(inputs.source_root, "abi").ok_or(EnsureFailure::Unavailable)?;
    require_abi(inputs.runtime, &snapshot.binary, &expected, &env)?;
    let mut capability = provider_capabilities(inputs.runtime, &snapshot.binary, &env);
    if matches!(capability, AbiResult::Mismatch)
        && !selected.temporary
        && !matches!(selected.source, Source::LatestDevelopment)
    {
        // A reviewed installer can coexist with a stale ABI-compatible binary.
        // Retry once with the installer's documented refresh control before
        // declaring the provider unavailable. The source remains the already
        // selected, digest-checked installer; no unreviewed fallback enters.
        set(&mut env, "SHDEPS_BOOTSTRAP_FORCE", "1");
        binary = bootstrap(
            inputs.runtime,
            &bash,
            &selected.path,
            &env,
            preparation_stderr,
        )?;
        directory = binary
            .parent()
            .ok_or(EnsureFailure::Unavailable)?
            .to_path_buf();
        snapshot = snapshot_provider(&binary, Path::new(inputs.state_home))
            .map_err(|_| EnsureFailure::Unavailable)?;
        require_abi(inputs.runtime, &snapshot.binary, &expected, &env)?;
        capability = provider_capabilities(inputs.runtime, &snapshot.binary, &env);
    }
    match capability {
        AbiResult::Match => {}
        AbiResult::Mismatch => return Err(EnsureFailure::Unavailable),
        AbiResult::Timeout(seconds) => return Err(EnsureFailure::CapabilityTimeout(seconds)),
        AbiResult::Interrupted(signal) => return Err(EnsureFailure::Interrupted(signal)),
    }
    env.remove(OsStr::new("SHDEPS_BOOTSTRAP_FORCE"));
    Ok(Ready {
        binary: snapshot.binary.clone(),
        directory,
        env,
        _snapshot: Some(snapshot),
    })
}

fn require_abi(
    runtime: &Runtime,
    binary: &Path,
    expected: &str,
    env: &BTreeMap<OsString, OsString>,
) -> Result<(), EnsureFailure> {
    match binary_abi(runtime, binary, expected, env) {
        AbiResult::Match => Ok(()),
        AbiResult::Mismatch => Err(EnsureFailure::Unavailable),
        AbiResult::Timeout(seconds) => Err(EnsureFailure::AbiTimeout(seconds)),
        AbiResult::Interrupted(signal) => Err(EnsureFailure::Interrupted(signal)),
    }
}

fn installer(
    inputs: &Inputs<'_>,
    configured: &crate::shdeps_env_abi::ConfiguredEnv,
) -> Option<Installer> {
    if let Some(lib) = inputs
        .runtime
        .value("SHDEPS_LIB")
        .filter(|value| !value.is_empty())
    {
        let candidate = Path::new(lib).parent()?.join("install.sh");
        if candidate.is_file()
            && crate::shdeps::installer_hash_matches(inputs.source_root, &candidate)
        {
            return Some(Installer {
                path: candidate,
                source: Source::Explicit,
                temporary: false,
            });
        }
    }
    let development = Path::new(&configured.git_dev_dir).join("shdeps");
    let development_installer = development.join("install.sh");
    let expected = crate::shdeps::lock_value(inputs.source_root, "revision")?;
    if development_installer.is_file()
        && development.join("shdeps.sh").is_file()
        && crate::shdeps::active_revision(&development) == expected
        && crate::shdeps::installer_hash_matches(inputs.source_root, &development_installer)
    {
        return Some(Installer {
            path: development_installer,
            source: Source::PinnedDevelopment,
            temporary: false,
        });
    }
    if inputs.policy == "latest" && development_checkout_valid(&development) {
        return Some(Installer {
            path: development_installer,
            source: Source::LatestDevelopment,
            temporary: false,
        });
    }
    let installed = inputs
        .runtime
        .value("SHDEPS_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(inputs.home).join(".local/share/shdeps"));
    let managed = installed.join("install.sh");
    if managed.is_file()
        && installed.join("shdeps.sh").is_file()
        && crate::shdeps::installer_hash_matches(inputs.source_root, &managed)
    {
        return Some(Installer {
            path: managed,
            source: Source::Managed,
            temporary: false,
        });
    }
    None
}

#[derive(Clone, Copy)]
enum DownloadFailure {
    Transport,
    Digest,
}

fn download_installer(
    inputs: &Inputs<'_>,
    preparation_stdout: &mut dyn std::io::Write,
    preparation_stderr: &mut dyn std::io::Write,
) -> Result<Installer, EnsureFailure> {
    if let Some(signal) = crate::cleanup::received_signal() {
        return Err(EnsureFailure::Interrupted(signal));
    }
    let revision = crate::shdeps::lock_value(inputs.source_root, "revision")
        .ok_or(EnsureFailure::Download(DownloadFailure::Transport))?;
    let path = inputs
        .runtime
        .value("PATH")
        .ok_or(EnsureFailure::Download(DownloadFailure::Transport))?;
    let curl = resolve_path(inputs.runtime, path, "curl")
        .ok_or(EnsureFailure::Download(DownloadFailure::Transport))?;
    let tmp = inputs
        .runtime
        .value("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut temporary = None;
    for nonce in 0..128u32 {
        if let Some(signal) = crate::cleanup::received_signal() {
            return Err(EnsureFailure::Interrupted(signal));
        }
        let candidate = tmp.join(format!(".dot-shdeps-{}-{nonce}", std::process::id()));
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate);
        if opened.is_ok() {
            temporary = Some(candidate);
            break;
        }
    }
    let temporary = temporary.ok_or(EnsureFailure::Download(DownloadFailure::Transport))?;
    let url = format!("https://raw.githubusercontent.com/cgraf78/shdeps/{revision}/install.sh");
    let timeout_seconds = value(inputs.runtime, "_DOT_SHDEPS_DOWNLOAD_TIMEOUT_SECONDS")
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .unwrap_or(PROVIDER_DOWNLOAD_TIMEOUT_SECONDS);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
    let mut remaining_bytes = PROVIDER_CAPTURE_LIMIT_BYTES;
    let mut succeeded = false;
    for attempt in 0..3 {
        let mut stdout = match CaptureStream::new() {
            Ok(capture) => capture,
            Err(_) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Download(DownloadFailure::Transport));
            }
        };
        let mut stderr = match CaptureStream::new() {
            Ok(capture) => capture,
            Err(_) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Download(DownloadFailure::Transport));
            }
        };
        let stdout_writer = match stdout.child_stdio() {
            Ok(writer) => writer,
            Err(_) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Download(DownloadFailure::Transport));
            }
        };
        let stderr_writer = match stderr.child_stdio() {
            Ok(writer) => writer,
            Err(_) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Download(DownloadFailure::Transport));
            }
        };
        let mut command = Command::new(&curl);
        command
            .args([
                "--connect-timeout",
                "10",
                "--max-time",
                "30",
                "--speed-limit",
                "1024",
                "--speed-time",
                "15",
                "-fsSL",
            ])
            .arg(&url)
            .arg("-o")
            .arg(&temporary)
            .env_clear()
            .envs(inputs.runtime.env())
            .current_dir(inputs.runtime.cwd())
            .stdin(Stdio::null())
            .stdout(stdout_writer)
            .stderr(stderr_writer);
        let (supervised, sink_error) = supervise_relayed_capture(
            command,
            deadline,
            &mut stdout,
            &mut stderr,
            preparation_stdout,
            preparation_stderr,
            &mut remaining_bytes,
        );
        if let Some(failure) = classify_download_deadline(
            &supervised,
            std::time::Instant::now() >= deadline,
            timeout_seconds,
        ) {
            let _ = std::fs::remove_file(&temporary);
            return Err(failure);
        }
        if sink_error
            .as_ref()
            .is_some_and(|failure| !failure.intentional_abort)
        {
            let _ = std::fs::remove_file(&temporary);
            return Err(EnsureFailure::Output);
        }
        // An intentionally aborted relay is a consequence of the supervised
        // outcome (the supervisor stops outward delivery once the download
        // is decided), never the outcome itself: slow hosts can still be
        // flushing flood bytes when the limit trips, and that race must not
        // shadow the limit diagnostic with a silent output failure. Fall
        // through to the supervised outcome below, mirroring the update
        // path's intentional-abort excuse.
        match supervised {
            Ok(crate::cleanup::SessionEnd::Exited(status)) if status.success() => {
                succeeded = true;
                break;
            }
            Ok(crate::cleanup::SessionEnd::Interrupted(signal)) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Interrupted(signal));
            }
            Ok(crate::cleanup::SessionEnd::TimedOut) => unreachable!("classified above"),
            Ok(crate::cleanup::SessionEnd::CleanupIncomplete) => {
                // A flood that exhausted the cumulative budget before
                // teardown failed still reports the output limit: the
                // budget state, not the teardown outcome, decides the
                // diagnostic, keeping it stable when teardown races the
                // limit on loaded hosts.
                if remaining_bytes == 0 {
                    let _ = preparation_stderr.write_all(PROVIDER_OUTPUT_LIMIT_ERROR.as_bytes());
                    let _ = preparation_stderr.write_all(b"\n");
                } else {
                    // Name the failure instead of failing silently: the
                    // remaining capture budget distinguishes an
                    // underflowing flood from a broken capture pipe.
                    let _ = preparation_stderr.write_all(
                        format!(
                            "Shdeps provider download cleanup incomplete with {remaining_bytes} capture bytes remaining\n"
                        )
                        .as_bytes(),
                    );
                }
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Output);
            }
            Ok(crate::cleanup::SessionEnd::Exited(_)) => {}
            Err(error) => {
                if is_provider_output_limit(&error) {
                    let _ = preparation_stderr.write_all(PROVIDER_OUTPUT_LIMIT_ERROR.as_bytes());
                    let _ = preparation_stderr.write_all(b"\n");
                } else {
                    // Name the failure instead of failing silently: a bare
                    // transport error leaves only the exit code, which
                    // cannot distinguish a broken capture pipe from a
                    // refused endpoint.
                    let _ = preparation_stderr.write_all(b"Shdeps provider download failed: ");
                    let _ = preparation_stderr.write_all(error.to_string().as_bytes());
                    let _ = preparation_stderr.write_all(b"\n");
                }
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Output);
            }
        }
        if attempt < 2 {
            let now = std::time::Instant::now();
            if now >= deadline {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::DownloadTimeout(timeout_seconds));
            }
            let delay = value(inputs.runtime, "_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS")
                .parse::<u64>()
                .unwrap_or(1);
            let delay =
                std::time::Duration::from_secs(delay).min(deadline.saturating_duration_since(now));
            if let Some(signal) = interruptible_delay(delay) {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::Interrupted(signal));
            }
            if std::time::Instant::now() >= deadline {
                let _ = std::fs::remove_file(&temporary);
                return Err(EnsureFailure::DownloadTimeout(timeout_seconds));
            }
        }
    }
    if !succeeded {
        let _ = std::fs::remove_file(&temporary);
        return Err(EnsureFailure::Download(DownloadFailure::Transport));
    }
    if !crate::shdeps::installer_hash_matches(inputs.source_root, &temporary) {
        let _ = std::fs::remove_file(&temporary);
        return Err(EnsureFailure::Download(DownloadFailure::Digest));
    }
    if let Some(signal) = crate::cleanup::received_signal() {
        let _ = std::fs::remove_file(&temporary);
        return Err(EnsureFailure::Interrupted(signal));
    }
    if std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700)).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(EnsureFailure::Download(DownloadFailure::Transport));
    }
    Ok(Installer {
        path: temporary,
        source: Source::Downloaded,
        temporary: true,
    })
}

fn interruptible_delay(duration: std::time::Duration) -> Option<i32> {
    let deadline = std::time::Instant::now() + duration;
    loop {
        if let Some(signal) = crate::cleanup::received_signal() {
            return Some(signal);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(std::time::Duration::from_millis(20)),
        );
    }
}

fn bootstrap(
    runtime: &Runtime,
    bash: &crate::bash::Resolved,
    installer: &Path,
    env: &BTreeMap<OsString, OsString>,
    preparation_stderr: &mut dyn std::io::Write,
) -> Result<PathBuf, EnsureFailure> {
    // The reviewed installer is the authority that selects the CLI. Keep its
    // shell-local `_SHDEPSW_BIN` across the process boundary with a strict,
    // NUL-framed protocol; installer stdout is intentionally not protocol.
    let mut command = Command::new(bash.path());
    crate::bash::sanitized_env(&mut command, env);
    let mut output = CaptureStream::new().map_err(|_| EnsureFailure::Unavailable)?;
    let mut stderr = CaptureStream::new().map_err(|_| EnsureFailure::Unavailable)?;
    let stdout = output
        .child_stdio()
        .map_err(|_| EnsureFailure::Unavailable)?;
    let stderr_writer = stderr
        .child_stdio()
        .map_err(|_| EnsureFailure::Unavailable)?;
    command
        .args([
            "--noprofile",
            "--norc",
            "-c",
            ". \"$1\" --bootstrap >/dev/null || exit; [[ -n ${_SHDEPSW_BIN:-} ]] || exit 1; printf 'dot-shdeps-bootstrap-v1\\0%s\\0' \"$_SHDEPSW_BIN\"",
        ])
        .arg("dot-shdeps-bootstrap")
        .arg(installer)
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr_writer);
    let mut bytes = Vec::new();
    let mut bounded_output = LimitedWriter::new(&mut bytes, PROVIDER_CAPTURE_LIMIT_BYTES);
    let mut bounded_stderr = LimitedWriter::new(preparation_stderr, PROVIDER_CAPTURE_LIMIT_BYTES);
    let mut relay_failed = false;
    let result = crate::cleanup::supervise_session(command, None, |final_pass| {
        drain_capture_streams(
            &mut output,
            &mut bounded_output,
            &mut stderr,
            &mut bounded_stderr,
            final_pass,
            &mut relay_failed,
        )
    })
    .map_err(|error| {
        // Name the failure instead of failing silently: a bare output
        // error leaves only the exit code, which cannot distinguish a
        // tripped output limit from a broken capture pipe.
        let _ = preparation_stderr.write_all(b"Shdeps provider bootstrap failed: ");
        let _ = preparation_stderr.write_all(error.to_string().as_bytes());
        let _ = preparation_stderr.write_all(b"\n");
        EnsureFailure::Output
    })?;
    let status = match result {
        crate::cleanup::SessionEnd::Exited(status) => status,
        crate::cleanup::SessionEnd::Interrupted(signal) => {
            return Err(EnsureFailure::Interrupted(signal));
        }
        crate::cleanup::SessionEnd::TimedOut | crate::cleanup::SessionEnd::CleanupIncomplete => {
            return Err(EnsureFailure::Unavailable);
        }
    };
    if !status.success() {
        return Err(EnsureFailure::Unavailable);
    }
    let prefix = b"dot-shdeps-bootstrap-v1\0";
    let path = bytes
        .as_slice()
        .strip_prefix(prefix)
        .ok_or(EnsureFailure::Unavailable)?;
    let path = path.strip_suffix(b"\0").ok_or(EnsureFailure::Unavailable)?;
    if path.is_empty() || path.contains(&0) {
        return Err(EnsureFailure::Unavailable);
    }
    let path = PathBuf::from(OsStr::from_bytes(path));
    if !path.is_absolute() || !executable(&path) {
        return Err(EnsureFailure::Unavailable);
    }
    Ok(path)
}

fn binary_abi(
    runtime: &Runtime,
    binary: &Path,
    expected: &str,
    env: &BTreeMap<OsString, OsString>,
) -> AbiResult {
    match probe_abi(runtime, binary, env) {
        AbiProbe::Output(bytes)
            if String::from_utf8_lossy(&bytes).trim_end_matches('\n')
                == format!("abi:{expected}") =>
        {
            AbiResult::Match
        }
        AbiProbe::Output(_) | AbiProbe::Mismatch => AbiResult::Mismatch,
        AbiProbe::Timeout(seconds) => AbiResult::Timeout(seconds),
        AbiProbe::Interrupted(signal) => AbiResult::Interrupted(signal),
    }
}

/// Require the provider's behavioral cancellation contract independently of
/// its wrapper ABI. The capability predicate is what lets Dot trust
/// `128+signal` and leave descendant ownership to
/// Shdeps without silently accepting a partial implementation.
fn provider_capability(
    runtime: &Runtime,
    binary: &Path,
    capability: &str,
    env: &BTreeMap<OsString, OsString>,
) -> AbiResult {
    match probe_api(runtime, binary, env, &["__api", "capability", capability]) {
        AbiProbe::Output(bytes) if bytes.is_empty() => AbiResult::Match,
        AbiProbe::Output(_) | AbiProbe::Mismatch => AbiResult::Mismatch,
        AbiProbe::Timeout(seconds) => AbiResult::Timeout(seconds),
        AbiProbe::Interrupted(signal) => AbiResult::Interrupted(signal),
    }
}

fn provider_capabilities(
    runtime: &Runtime,
    binary: &Path,
    env: &BTreeMap<OsString, OsString>,
) -> AbiResult {
    let mut mismatch = false;
    for capability in [
        OWNED_SUBPROCESS_CANCELLATION_CAPABILITY,
        PROMPT_FIFO_READER_BEFORE_EVENT_CAPABILITY,
    ] {
        match provider_capability(runtime, binary, capability, env) {
            AbiResult::Match => {}
            AbiResult::Mismatch => mismatch = true,
            result @ (AbiResult::Timeout(_) | AbiResult::Interrupted(_)) => return result,
        }
    }
    if mismatch {
        AbiResult::Mismatch
    } else {
        AbiResult::Match
    }
}

/// Probe a selected provider for doctor using only the captured Runtime.
pub(crate) fn doctor_abi_version(runtime: &Runtime, binary: &Path) -> Option<String> {
    match probe_abi(runtime, binary, runtime.env()) {
        AbiProbe::Output(bytes) => Some(
            String::from_utf8_lossy(&bytes)
                .trim_end_matches('\n')
                .to_string(),
        ),
        AbiProbe::Mismatch | AbiProbe::Timeout(_) | AbiProbe::Interrupted(_) => None,
    }
}

/// Whether the selected provider advertises the cancellation ownership that
/// the native update coordinator requires before it can trust signal exits.
pub(crate) fn doctor_has_cancellation_capability(runtime: &Runtime, binary: &Path) -> bool {
    matches!(
        provider_capability(
            runtime,
            binary,
            OWNED_SUBPROCESS_CANCELLATION_CAPABILITY,
            runtime.env(),
        ),
        AbiResult::Match
    )
}

/// Whether prompt acknowledgement has the reader-before-event handshake that
/// makes per-prompt nonblocking FIFO writes authoritative.
pub(crate) fn doctor_has_prompt_capability(runtime: &Runtime, binary: &Path) -> bool {
    matches!(
        provider_capability(
            runtime,
            binary,
            PROMPT_FIFO_READER_BEFORE_EVENT_CAPABILITY,
            runtime.env(),
        ),
        AbiResult::Match
    )
}

fn probe_abi(runtime: &Runtime, binary: &Path, env: &BTreeMap<OsString, OsString>) -> AbiProbe {
    probe_api(runtime, binary, env, &["__api", "version"])
}

fn probe_api(
    runtime: &Runtime,
    binary: &Path,
    env: &BTreeMap<OsString, OsString>,
    args: &[&str],
) -> AbiProbe {
    let timeout = value(runtime, "_DOT_SHDEPS_ABI_TIMEOUT_SECONDS")
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .unwrap_or(10);
    let mut output = match CaptureStream::new() {
        Ok(output) => output,
        Err(_) => return AbiProbe::Mismatch,
    };
    let stdout = match output.child_stdio() {
        Ok(stdout) => stdout,
        Err(_) => return AbiProbe::Mismatch,
    };
    let mut command = Command::new(binary);
    crate::bash::sanitized_env(&mut command, env);
    command
        .args(args)
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut bytes = Vec::new();
    let mut bounded_output = LimitedWriter::new(&mut bytes, PROVIDER_CAPTURE_LIMIT_BYTES);
    let result = crate::cleanup::supervise_session(command, Some(deadline), |final_pass| {
        output.drain_snapshot(&mut bounded_output, final_pass)
    });
    match result {
        Ok(crate::cleanup::SessionEnd::Exited(status)) if status.success() => {
            AbiProbe::Output(bytes)
        }
        Ok(crate::cleanup::SessionEnd::Interrupted(signal)) => AbiProbe::Interrupted(signal),
        Ok(crate::cleanup::SessionEnd::TimedOut) => AbiProbe::Timeout(timeout),
        Ok(crate::cleanup::SessionEnd::Exited(_))
        | Ok(crate::cleanup::SessionEnd::CleanupIncomplete)
        | Err(_) => AbiProbe::Mismatch,
    }
}

enum AbiResult {
    Match,
    Mismatch,
    Timeout(u64),
    Interrupted(i32),
}

enum AbiProbe {
    Output(Vec<u8>),
    Mismatch,
    Timeout(u64),
    Interrupted(i32),
}

fn run_update(
    inputs: &Inputs<'_>,
    ready: &Ready,
    stage: &mut crate::progress_ui::Stage,
    now_secs: i64,
    live_out: &mut dyn std::io::Write,
    live_err: &mut dyn std::io::Write,
) -> Outcome {
    let before = crate::shdeps::active_revision(inputs.source_root);
    let mut env = ready.env.clone();
    set(&mut env, "SHDEPS_DIR", ready.directory.as_os_str());
    let path = env.get(OsStr::new("PATH")).cloned().unwrap_or_default();
    let mut provider_path = env
        .get(OsStr::new("SHDEPS_BIN_DIR"))
        .cloned()
        .unwrap_or_default();
    provider_path.push(OsStr::new(":"));
    provider_path.push(path);
    set(&mut env, "PATH", provider_path);
    set(&mut env, "SHDEPS_NESTED", "1");
    set(&mut env, "SHDEPS_PROGRESS", "jsonl");
    let mut prompt = prompt_pipe(inputs.runtime);
    if let Some(pipe) = &prompt {
        set(&mut env, "SHDEPS_PROGRESS_PROMPT_ACK", &pipe.path);
    }
    if inputs.runtime.value("SHDEPS_JOBS").is_none() {
        let jobs = crate::merges::update_jobs(inputs.update_jobs.unwrap_or_default());
        set(&mut env, "SHDEPS_JOBS", jobs);
    }
    if inputs
        .runtime
        .value("DOT_SHDEPS_ALLOW_GH_AUTH_TOKEN")
        .and_then(OsStr::to_str)
        == Some("1")
        && inputs.runtime.value("SHDEPS_ALLOW_GH_AUTH_TOKEN").is_none()
    {
        set(&mut env, "SHDEPS_ALLOW_GH_AUTH_TOKEN", "1");
    }
    let mut command = Command::new(&ready.binary);
    crate::bash::sanitized_env(&mut command, &env);
    let Ok(mut stdout) = CaptureStream::new() else {
        return failed_update();
    };
    let Ok(mut stderr_reader) = CaptureStream::new() else {
        return failed_update();
    };
    let Ok(stdout_writer) = stdout.child_stdio() else {
        return failed_update();
    };
    let Ok(stderr_writer) = stderr_reader.child_stdio() else {
        return failed_update();
    };
    command
        .arg("update")
        .current_dir(&ready.directory)
        .stdin(Stdio::null())
        .stdout(stdout_writer)
        .stderr(stderr_writer);
    let mut state = crate::shdeps_ui::State::new();
    let mut session = crate::shdeps_ui_render::reset(false);
    let mut live = false;
    let mut pending = Vec::new();
    let mut deferred_stdout = Vec::new();
    let mut final_stderr = Vec::new();
    let mut drain_state = ProviderDrainState {
        remaining_bytes: PROVIDER_RUN_CAPTURE_LIMIT_BYTES,
        failed: false,
    };
    let mut remaining_events = PROVIDER_RUN_EVENT_LIMIT;
    // The shell used a JSONL FIFO for stdout while stderr stayed inherited, so
    // it never promised a total order between the two descriptors. Preserve
    // each descriptor's order and every byte already relayed. Once a signal or
    // sink failure is observed, queued bytes are drained only to keep provider
    // teardown bounded; they are not interpreted, acknowledged, or replayed.
    // ABI-compatible providers own their dependency-manager descendants. Dot
    // retains only the exact provider PID so Shdeps keeps the caller's terminal
    // session while both layers can complete their respective cleanup.
    let (relay_sender, relay_receiver) = std::sync::mpsc::channel();
    let sink_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (supervised, sink_error) = std::thread::scope(|scope| {
        let relay_failure = sink_failed.clone();
        let supervisor = {
            // Move only these scoped references into the supervisor. The
            // parsed provider state remains available to the caller after the
            // thread has joined, while `command` itself is transferred to the
            // one thread that owns its complete lifecycle.
            let stdout = &mut stdout;
            let stderr_reader = &mut stderr_reader;
            let pending = &mut pending;
            let deferred_stdout = &mut deferred_stdout;
            let final_stderr = &mut final_stderr;
            let drain_state = &mut drain_state;
            let state = &mut state;
            let session = &mut session;
            let stage = &mut *stage;
            let live = &mut live;
            let prompt = &mut prompt;
            let remaining_events = &mut remaining_events;
            scope.spawn(move || {
                let mut bounded_deferred_stdout =
                    LimitedWriter::new(deferred_stdout, PROVIDER_FRAME_LIMIT_BYTES);
                let mut bounded_final_stderr =
                    LimitedWriter::new(final_stderr, PROVIDER_CAPTURE_LIMIT_BYTES);
                let mut relay_out = RelayWriter {
                    target: RelayTarget::Stdout,
                    sender: relay_sender.clone(),
                    sink_failed: relay_failure.clone(),
                };
                let mut relay_err = RelayWriter {
                    target: RelayTarget::Stderr,
                    sender: relay_sender,
                    sink_failed: relay_failure,
                };
                let mut bounded_live_out =
                    LimitedWriter::new(&mut relay_out, PROVIDER_CAPTURE_LIMIT_BYTES);
                let mut bounded_live_err =
                    LimitedWriter::new(&mut relay_err, PROVIDER_CAPTURE_LIMIT_BYTES);
                let finished = crate::cleanup::supervise_child(command, None, |final_pass| {
                    // `supervise_child` learns the normal exit status immediately
                    // before its final drain. Defer that drain's interpretation
                    // until the status is available below: the provider uses a
                    // normal `128+signal` exit after cleanup, and acknowledging a
                    // queued prompt before recognizing that status could release
                    // new work after cancellation.
                    if final_pass && !drain_state.failed {
                        let stdout_result = drain_cumulative_stream(
                            stdout,
                            &mut bounded_deferred_stdout,
                            &mut drain_state.remaining_bytes,
                            true,
                        );
                        let result = match stdout_result {
                            Ok(()) => drain_cumulative_stream(
                                stderr_reader,
                                &mut bounded_final_stderr,
                                &mut drain_state.remaining_bytes,
                                true,
                            ),
                            Err(error) => {
                                let _ = stderr_reader.drain_snapshot(&mut std::io::sink(), true);
                                Err(error)
                            }
                        };
                        if result.is_err() {
                            drain_state.failed = true;
                        }
                        return result;
                    }
                    drain_provider_streams(
                        stdout,
                        pending,
                        stderr_reader,
                        final_pass,
                        &mut bounded_live_err,
                        drain_state,
                        |line| {
                            // Teardown still drains the bounded socket so a TERM
                            // handler cannot deadlock on backpressure. Cancellation
                            // never interprets or acknowledges another event.
                            if crate::cleanup::received_signal().is_none() {
                                return provider_line(
                                    line,
                                    state,
                                    session,
                                    stage,
                                    inputs,
                                    now_secs,
                                    &mut bounded_live_out,
                                    live,
                                    prompt.as_ref().map(|pipe| pipe.path.as_path()),
                                    remaining_events,
                                );
                            }
                            Ok(())
                        },
                    )
                });
                let abort_output = match &finished {
                    Ok(crate::cleanup::SessionEnd::Exited(status)) => {
                        status.code().and_then(provider_interruption).is_some()
                    }
                    Ok(crate::cleanup::SessionEnd::Interrupted(_))
                    | Ok(crate::cleanup::SessionEnd::TimedOut)
                    | Ok(crate::cleanup::SessionEnd::CleanupIncomplete)
                    | Err(_) => true,
                };
                if abort_output {
                    crate::cleanup::abort_outward_writes();
                }
                (
                    finished,
                    bounded_live_out.remaining(),
                    bounded_live_err.remaining(),
                )
            })
        };

        let sink_error = pump_relay(relay_receiver, live_out, live_err, sink_failed.as_ref());
        let supervised = supervisor.join().ok();
        (supervised, sink_error)
    });
    crate::cleanup::resume_outward_writes();
    // A panicking supervisor reads as a failed update, never a crash
    // (the outward-write latch above still resumes first).
    let Some((finished, live_out_remaining, live_err_remaining)) = supervised else {
        return failed_output();
    };
    let mut bounded_live_out = LimitedWriter {
        output: live_out,
        remaining: live_out_remaining,
    };
    let mut bounded_live_err = LimitedWriter {
        output: live_err,
        remaining: live_err_remaining,
    };
    let (status, mut interrupted) = match finished {
        Ok(crate::cleanup::SessionEnd::Exited(status)) => {
            let status = status.code().unwrap_or(1);
            (status, provider_interruption(status))
        }
        Ok(crate::cleanup::SessionEnd::Interrupted(signal)) => (1, Some(signal)),
        Ok(crate::cleanup::SessionEnd::TimedOut) => (1, None),
        Ok(crate::cleanup::SessionEnd::CleanupIncomplete) => {
            return Outcome {
                status: crate::cleanup::CLEANUP_INCOMPLETE_STATUS,
                interrupted: None,
                abort: true,
                stage_status: b"failed".to_vec(),
                summary: b"dependency update cleanup incomplete".to_vec(),
                details: Vec::new(),
                revision_change: None,
            };
        }
        Err(error) => {
            if let Some(signal) = crate::cleanup::received_signal() {
                (1, Some(signal))
            } else {
                report_provider_output_limit(&mut bounded_live_err, &error);
                return failed_output();
            }
        }
    };
    if interrupted.is_some()
        && sink_error
            .as_ref()
            .is_none_or(|failure| failure.intentional_abort)
    {
        // A capability-validated 128+signal status is authoritative. The
        // supervisor intentionally aborts queued outward writes after that
        // exit; a relay rejection caused by that abort must not rewrite a
        // clean cancellation into ordinary status 1.
        return Outcome {
            status,
            interrupted,
            abort: true,
            stage_status: b"failed".to_vec(),
            summary: b"dependency update interrupted".to_vec(),
            details: Vec::new(),
            revision_change: None,
        };
    }
    if sink_error.is_some() {
        return failed_output();
    }
    let final_stdout = if interrupted.is_none() {
        let deferred = consume_provider_bytes(
            &mut pending,
            &deferred_stdout,
            false,
            PROVIDER_FRAME_LIMIT_BYTES,
            |line| {
                provider_line(
                    line,
                    &mut state,
                    &mut session,
                    stage,
                    inputs,
                    now_secs,
                    &mut bounded_live_out,
                    &mut live,
                    prompt.as_ref().map(|pipe| pipe.path.as_path()),
                    &mut remaining_events,
                )
            },
        );
        deferred.and_then(|()| {
            drain_provider_output(
                &mut stdout,
                &mut pending,
                true,
                &mut drain_state.remaining_bytes,
                |line| {
                    provider_line(
                        line,
                        &mut state,
                        &mut session,
                        stage,
                        inputs,
                        now_secs,
                        &mut bounded_live_out,
                        &mut live,
                        prompt.as_ref().map(|pipe| pipe.path.as_path()),
                        &mut remaining_events,
                    )
                },
            )
        })
    } else {
        Ok(())
    };
    if let Err(error) = final_stdout {
        report_provider_output_limit(&mut bounded_live_err, &error);
        return failed_output();
    }
    if interrupted.is_none() {
        let final_stderr_result = bounded_live_err.write_all(&final_stderr).and_then(|()| {
            drain_cumulative_stream(
                &mut stderr_reader,
                &mut bounded_live_err,
                &mut drain_state.remaining_bytes,
                true,
            )
        });
        if let Err(error) = final_stderr_result {
            report_provider_output_limit(&mut bounded_live_err, &error);
            return failed_output();
        }
    }
    interrupted = interrupted.or_else(crate::cleanup::received_signal);
    if interrupted.is_some() {
        return Outcome {
            status,
            interrupted,
            abort: true,
            stage_status: b"failed".to_vec(),
            summary: b"dependency update interrupted".to_vec(),
            details: Vec::new(),
            revision_change: None,
        };
    }
    let ui = crate::shdeps_ui_render::Ui {
        palette: inputs.palette,
        quiet: inputs.quiet,
        multibyte: inputs.multibyte,
    };
    let (verbose_rows, live) = crate::shdeps_ui_render::print_verbose_items(
        &ui,
        false,
        inputs.verbose,
        state.order(),
        state.items(),
        state.labels(),
        &crate::shdeps_ui::group_label,
    );
    if bounded_live_out.write_all(&verbose_rows).is_err() {
        return failed_output();
    }
    let threshold = inputs
        .runtime
        .value("DOT_UPDATE_SUBPHASE_THRESHOLD_MS")
        .map(OsStr::as_encoded_bytes);
    let (details, _) = crate::shdeps_ui_render::print_group_summaries(
        &ui,
        live,
        inputs.verbose,
        threshold,
        state.order(),
        state.summaries(),
        state.items(),
    );
    let after = crate::shdeps::active_revision(inputs.source_root);
    let revision_change = (status == 0 && before != after).then_some((before, after));
    if status == 0 {
        Outcome {
            status,
            interrupted,
            abort: false,
            stage_status: session.status,
            summary: session.summary,
            details,
            revision_change,
        }
    } else {
        Outcome {
            status,
            interrupted,
            abort: false,
            stage_status: b"failed".to_vec(),
            summary: if session.summary == b"dependencies checked" {
                b"dependency update failed".to_vec()
            } else {
                session.summary
            },
            details,
            revision_change: None,
        }
    }
}

/// Capable providers advertise that these statuses mean cancellation only after
/// all synchronously owned subprocesses have been stopped, drained, and
/// reaped. Keeping this mapping at the provider boundary avoids treating an
/// arbitrary command's exit 130 as a signal elsewhere in Dot.
fn provider_interruption(status: i32) -> Option<i32> {
    match status {
        129 | 130 | 131 | 143 => Some(status - 128),
        _ => None,
    }
}

fn drain_provider_streams(
    stdout: &mut CaptureStream,
    pending: &mut Vec<u8>,
    stderr: &mut CaptureStream,
    final_pass: bool,
    stderr_output: &mut dyn std::io::Write,
    state: &mut ProviderDrainState,
    consume: impl FnMut(Vec<u8>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if state.failed {
        pending.clear();
        let mut sink = std::io::sink();
        let stdout_result = stdout.drain_snapshot(&mut sink, final_pass);
        let stderr_result = stderr.drain_snapshot(&mut sink, final_pass);
        return stdout_result.and(stderr_result);
    }

    let stdout_result = drain_provider_output(
        stdout,
        pending,
        final_pass,
        &mut state.remaining_bytes,
        consume,
    );
    // Always service both sockets. A provider may need to finish a TERM trap
    // and reap its own descendants after either outward sink has failed.
    let result = match stdout_result {
        Ok(()) => drain_cumulative_stream(
            stderr,
            stderr_output,
            &mut state.remaining_bytes,
            final_pass,
        ),
        Err(error) => {
            let _ = stderr.drain_snapshot(&mut std::io::sink(), final_pass);
            Err(error)
        }
    };
    if result.is_err() {
        state.failed = true;
    }
    result
}

fn drain_provider_output(
    reader: &mut CaptureStream,
    pending: &mut Vec<u8>,
    finish: bool,
    remaining_run_bytes: &mut usize,
    consume: impl FnMut(Vec<u8>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut incoming = Vec::new();
    reader.drain_snapshot(&mut incoming, finish)?;
    charge_provider_run_bytes(remaining_run_bytes, incoming.len())?;
    consume_provider_bytes(
        pending,
        &incoming,
        finish,
        PROVIDER_FRAME_LIMIT_BYTES,
        consume,
    )
}

fn drain_cumulative_stream(
    stream: &mut CaptureStream,
    output: &mut dyn std::io::Write,
    remaining_run_bytes: &mut usize,
    final_pass: bool,
) -> std::io::Result<()> {
    let mut output = CumulativeWriter {
        output,
        remaining: remaining_run_bytes,
    };
    stream.drain_snapshot(&mut output, final_pass)
}

fn charge_provider_run_bytes(remaining: &mut usize, count: usize) -> std::io::Result<()> {
    if count > *remaining {
        *remaining = 0;
        return Err(provider_output_limit());
    }
    *remaining -= count;
    Ok(())
}

fn consume_provider_bytes(
    pending: &mut Vec<u8>,
    incoming: &[u8],
    finish: bool,
    frame_limit: usize,
    mut consume: impl FnMut(Vec<u8>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut start = 0;
    while start < incoming.len() {
        let newline = incoming[start..].iter().position(|byte| *byte == b'\n');
        let end = newline.map_or(incoming.len(), |offset| start + offset + 1);
        let segment = &incoming[start..end];
        if pending.len().saturating_add(segment.len()) > frame_limit {
            return Err(std::io::Error::other(PROVIDER_OUTPUT_LIMIT_ERROR));
        }
        pending.extend_from_slice(segment);
        if newline.is_some() {
            consume(std::mem::take(pending))?;
        }
        start = end;
    }
    if finish && !pending.is_empty() {
        consume(std::mem::take(pending))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn provider_line(
    mut line: Vec<u8>,
    state: &mut crate::shdeps_ui::State,
    session: &mut crate::shdeps_ui_render::Session,
    stage: &mut crate::progress_ui::Stage,
    inputs: &Inputs<'_>,
    now_secs: i64,
    output: &mut dyn std::io::Write,
    live: &mut bool,
    prompt: Option<&Path>,
    remaining_events: &mut usize,
) -> std::io::Result<()> {
    if *remaining_events == 0 {
        return Err(provider_output_limit());
    }
    *remaining_events -= 1;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    handle_event(
        &line, state, session, stage, inputs, now_secs, output, live, prompt,
    )
}

fn report_provider_output_limit(output: &mut LimitedWriter<'_>, error: &std::io::Error) {
    if is_provider_output_limit(error) {
        let _ = output
            .output
            .write_all(PROVIDER_OUTPUT_LIMIT_ERROR.as_bytes());
        let _ = output.output.write_all(b"\n");
    }
}

fn failed_update() -> Outcome {
    Outcome {
        status: 1,
        interrupted: None,
        abort: false,
        stage_status: b"failed".to_vec(),
        summary: b"dependency update failed".to_vec(),
        details: Vec::new(),
        revision_change: None,
    }
}

fn failed_output() -> Outcome {
    Outcome {
        abort: true,
        ..failed_update()
    }
}

fn acknowledge_prompt(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};

    let mut prompt = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(prompt) => prompt,
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENXIO) | Some(libc::EPIPE)) => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let metadata = prompt.metadata()?;
    if !metadata.file_type().is_fifo()
        || metadata.uid() != crate::cleanup::euid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "provider prompt acknowledgement path changed identity",
        ));
    }
    match prompt.write_all(bytes).and_then(|()| prompt.flush()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    line: &[u8],
    state: &mut crate::shdeps_ui::State,
    session: &mut crate::shdeps_ui_render::Session,
    stage: &mut crate::progress_ui::Stage,
    inputs: &Inputs<'_>,
    now_secs: i64,
    output: &mut dyn std::io::Write,
    live: &mut bool,
    prompt: Option<&Path>,
) -> std::io::Result<()> {
    let Some(fields) = parse_event(inputs.runtime, line) else {
        return Ok(());
    };
    match text(&fields, "event") {
        b"prompt" => {
            let (bytes, active, ack) = crate::shdeps_ui_render::prompt_pause(session, *live, "1");
            output.write_all(&bytes)?;
            output.flush()?;
            *live = active;
            if crate::cleanup::received_signal().is_none() {
                if let (Some(prompt), Some(ack)) = (prompt, ack) {
                    acknowledge_prompt(prompt, &ack)?;
                }
            }
        }
        b"item" => {
            crate::shdeps_ui_render::prompt_resume(session);
            state
                .record_item(
                    text(&fields, "group"),
                    text(&fields, "status"),
                    text(&fields, "name"),
                    text(&fields, "detail"),
                )
                .map_err(|_| provider_output_limit())?;
        }
        b"group_summary" => {
            crate::shdeps_ui_render::prompt_resume(session);
            state
                .record_group_summary(
                    text(&fields, "group"),
                    text(&fields, "label"),
                    text(&fields, "status"),
                    number(&fields, "changed"),
                    number(&fields, "current"),
                    number(&fields, "skipped"),
                    number(&fields, "failed"),
                    text(&fields, "elapsed_ms"),
                    number(&fields, "warnings"),
                )
                .map_err(|_| provider_output_limit())?;
        }
        b"summary" => {
            crate::shdeps_ui_render::prompt_resume(session);
            session.status = text(&fields, "status").to_vec();
            session.summary = crate::shdeps_ui::summary_text(
                number(&fields, "changed"),
                number(&fields, "current"),
                number(&fields, "skipped"),
                number(&fields, "failed"),
                number(&fields, "warnings"),
            );
        }
        b"phase" => {
            crate::shdeps_ui_render::prompt_resume(session);
            let done = number(&fields, "done");
            let total = number(&fields, "total");
            // Ingestion gate (defense layer 2 beside the renderer's own
            // clamp): a negative `done` is a buggy-provider sentinel, not
            // progress, so it renders label-only exactly like `total <= 0`.
            let detail = if total > 0 && done >= 0 {
                crate::progress_ui::progress_detail_with_label(
                    text(&fields, "label"),
                    done,
                    total,
                    None,
                    "18",
                    inputs.bar_width,
                    inputs.ascii,
                    inputs.multibyte,
                )
            } else {
                text(&fields, "label").to_vec()
            };
            let detail = crate::progress_ui::sanitize_untrusted_text(&detail);
            output.write_all(&stage.update(&detail, now_secs, inputs.verbose.then_some("1")))?;
        }
        b"warning" | b"detail" | b"hint" => {
            crate::shdeps_ui_render::prompt_resume(session);
            let event = text(&fields, "event");
            let status = match text(&fields, "status") {
                b"" => event,
                status => status,
            };
            // Render-boundary sanitize: decoded provider strings must not
            // carry newlines or ANSI escapes into the rendered rows.
            let status = crate::progress_ui::sanitize_untrusted_text(status);
            let detail = crate::progress_ui::sanitize_untrusted_text(text(&fields, "detail"));
            output.write_all(&stage.note(&status, &detail))?;
        }
        _ => {}
    }
    Ok(())
}

fn prompt_pipe(runtime: &Runtime) -> Option<PromptPipe> {
    let tmp = runtime
        .value("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    for nonce in 0..128u32 {
        let directory = tmp.join(format!(".dot-shdeps-ui-{}-{nonce}", std::process::id()));
        if std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .is_err()
        {
            continue;
        }
        let path = directory.join("prompt-ack");
        let fifo = std::ffi::CString::new(path.as_os_str().as_bytes()).ok();
        let created = fifo
            .as_ref()
            // SAFETY: `fifo` is a NUL-terminated pathname and mode has no
            // pointer semantics. The containing directory was just created
            // private to this process, so no other user can replace the leaf.
            .is_some_and(|fifo| unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } == 0);
        if created
            && path.symlink_metadata().is_ok_and(|meta| {
                use std::os::unix::fs::FileTypeExt as _;
                meta.file_type().is_fifo()
                    && meta.uid() == crate::cleanup::euid()
                    && meta.mode() & 0o077 == 0
            })
        {
            return Some(PromptPipe { directory, path });
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&directory);
    }
    None
}

/// Parse JSONL exactly through the shell's available parser branch. The shell
/// deliberately retains raw escaped strings in bootstrap environments without
/// jq; rendering decoded Unicode there would make native output drift.
fn parse_event(runtime: &Runtime, line: &[u8]) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
    let jq = runtime
        .value("PATH")
        .is_some_and(|path| resolve_path(runtime, path, "jq").is_some());
    parse_object(line, jq)
}

fn parse_object(line: &[u8], decode_strings: bool) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut map = BTreeMap::new();
    let mut index = 0;
    skip_space(line, &mut index);
    if line.get(index)? != &b'{' {
        return None;
    }
    index += 1;
    loop {
        skip_space(line, &mut index);
        if line.get(index) == Some(&b'}') {
            return Some(map);
        }
        let key = json_string(line, &mut index)?;
        skip_space(line, &mut index);
        if line.get(index)? != &b':' {
            return None;
        }
        index += 1;
        skip_space(line, &mut index);
        let value = if line.get(index) == Some(&b'"') {
            if decode_strings {
                json_string(line, &mut index)?
            } else {
                json_string_raw(line, &mut index)?
            }
        } else {
            let start = index;
            while line.get(index).is_some_and(|b| *b != b',' && *b != b'}') {
                index += 1;
            }
            line[start..index]
                .iter()
                .copied()
                .take_while(|b| !b.is_ascii_whitespace())
                .collect()
        };
        map.insert(key, value);
        skip_space(line, &mut index);
        match line.get(index)? {
            b',' => index += 1,
            b'}' => return Some(map),
            _ => return None,
        }
    }
}

/// Match the shell's bootstrap `sed` capture while consuming one valid JSON
/// string. Its `[^\"]*` capture stops at the quote byte in `\"`, after keeping
/// the slash. We must still scan past that escaped quote to locate the real
/// JSON terminator and continue parsing later fields.
fn json_string_raw(input: &[u8], index: &mut usize) -> Option<Vec<u8>> {
    if input.get(*index)? != &b'"' {
        return None;
    }
    *index += 1;
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(byte) = input.get(*index).copied() {
        *index += 1;
        match byte {
            b'"' => return Some(out),
            b'\\' => {
                if !truncated {
                    out.push(byte);
                }
                let escaped = input.get(*index).copied()?;
                *index += 1;
                if escaped == b'"' {
                    truncated = true;
                } else if !truncated {
                    out.push(escaped);
                }
                if escaped == b'u' {
                    let digits = input.get(*index..*index + 4)?;
                    if !digits.iter().all(u8::is_ascii_hexdigit) {
                        return None;
                    }
                    if !truncated {
                        out.extend_from_slice(digits);
                    }
                    *index += 4;
                }
            }
            0..=31 => return None,
            _ if !truncated => out.push(byte),
            _ => {}
        }
    }
    None
}

fn json_string(input: &[u8], index: &mut usize) -> Option<Vec<u8>> {
    if input.get(*index)? != &b'"' {
        return None;
    }
    *index += 1;
    let mut out = Vec::new();
    while let Some(byte) = input.get(*index).copied() {
        *index += 1;
        match byte {
            b'"' => return Some(out),
            b'\\' => {
                let escaped = input.get(*index).copied()?;
                *index += 1;
                match escaped {
                    b'"' | b'\\' | b'/' => out.push(escaped),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let first = hex4(input, index)?;
                        let scalar = if (0xd800..=0xdbff).contains(&first) {
                            if input.get(*index..*index + 2)? != b"\\u" {
                                return None;
                            }
                            *index += 2;
                            let second = hex4(input, index)?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return None;
                            }
                            0x1_0000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
                        } else {
                            first as u32
                        };
                        let character = char::from_u32(scalar)?;
                        let mut encoded = [0; 4];
                        out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                    }
                    _ => return None,
                }
            }
            0..=31 => return None,
            _ => out.push(byte),
        }
    }
    None
}

fn hex4(input: &[u8], index: &mut usize) -> Option<u16> {
    let digits = input.get(*index..*index + 4)?;
    let mut value = 0u16;
    for digit in digits {
        value = value.checked_mul(16)?;
        value = value.checked_add(match digit {
            b'0'..=b'9' => u16::from(*digit - b'0'),
            b'a'..=b'f' => u16::from(*digit - b'a' + 10),
            b'A'..=b'F' => u16::from(*digit - b'A' + 10),
            _ => return None,
        })?;
    }
    *index += 4;
    Some(value)
}

fn skip_space(input: &[u8], index: &mut usize) {
    while input.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn text<'a>(fields: &'a BTreeMap<Vec<u8>, Vec<u8>>, key: &str) -> &'a [u8] {
    fields
        .get(key.as_bytes())
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn number(fields: &BTreeMap<Vec<u8>, Vec<u8>>, key: &str) -> i64 {
    std::str::from_utf8(text(fields, key))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn value<'a>(runtime: &'a Runtime, key: &str) -> &'a str {
    runtime
        .value(key)
        .and_then(OsStr::to_str)
        .unwrap_or_default()
}

fn set<K: AsRef<OsStr>, V: AsRef<OsStr>>(env: &mut BTreeMap<OsString, OsString>, key: K, value: V) {
    env.insert(key.as_ref().to_os_string(), value.as_ref().to_os_string());
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn resolve_path(runtime: &Runtime, raw: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(raw)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                runtime.cwd().to_path_buf()
            } else {
                dir
            }
        })
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
}

pub(crate) fn development_checkout_valid(checkout: &Path) -> bool {
    development_checkout(checkout).unwrap_or(false)
}

fn development_checkout(checkout: &Path) -> Option<bool> {
    let euid = crate::temp::current_uid().unwrap_or(u32::MAX);
    let checkout_meta = checkout.symlink_metadata().ok()?;
    if !checkout_meta.is_dir() || !owned(&checkout_meta, euid) {
        return Some(false);
    }
    for path in [checkout.join("install.sh"), checkout.join("shdeps.sh")] {
        let meta = path.symlink_metadata().ok()?;
        if !meta.is_file() || !owned(&meta, euid) {
            return Some(false);
        }
    }
    let dot_git = checkout.join(".git");
    let git_meta = dot_git.symlink_metadata().ok()?;
    if !(git_meta.is_dir() || git_meta.is_file()) || !owned(&git_meta, euid) {
        return Some(false);
    }
    let root = development_git_output(checkout, &["rev-parse", "--show-toplevel"])?;
    if !root.status.success() {
        return Some(false);
    }
    let physical = checkout.canonicalize().ok()?;
    if Path::new(OsStr::from_bytes(
        root.stdout.strip_suffix(b"\n").unwrap_or(&root.stdout),
    ))
    .canonicalize()
    .ok()?
        != physical
    {
        return Some(false);
    }
    for args in [
        ["rev-parse", "--absolute-git-dir"].as_slice(),
        ["rev-parse", "--path-format=absolute", "--git-common-dir"].as_slice(),
    ] {
        let output = development_git_output(checkout, args)?;
        if !output.status.success() {
            return Some(false);
        }
        let path = Path::new(OsStr::from_bytes(
            output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
        ));
        let physical = path.canonicalize().ok()?;
        if !owned(&physical.symlink_metadata().ok()?, euid) {
            return Some(false);
        }
    }
    for args in [
        ["config", "--local", "--get-all", "remote.origin.url"].as_slice(),
        ["remote", "get-url", "--all", "origin"].as_slice(),
    ] {
        let origin = development_git_output(checkout, args)?;
        if !origin.status.success() {
            return Some(false);
        }
        let lines: Vec<&[u8]> = origin
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        if lines.len() != 1
            || !std::str::from_utf8(lines[0]).is_ok_and(crate::shdeps::origin_allowed)
        {
            return Some(false);
        }
    }
    Some(true)
}

fn development_git_output(checkout: &Path, args: &[&str]) -> Option<std::process::Output> {
    let mut command = crate::temp::sanitized_git(checkout, args);
    command.stdin(Stdio::null());
    crate::cleanup::run_session_output(
        command,
        None,
        PROVIDER_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    )
    .ok()
}

fn owned(metadata: &std::fs::Metadata, euid: u32) -> bool {
    !metadata.file_type().is_symlink() && metadata.uid() == euid && metadata.mode() & 0o022 == 0
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;

    use super::{
        CaptureStream, EnsureFailure, Inputs, LimitedWriter, PROVIDER_OUTPUT_LIMIT_ERROR,
        ProviderDrainState, Ready, classify_download_deadline, consume_provider_bytes,
        download_installer, drain_capture_streams, drain_provider_output, drain_provider_streams,
        handle_event, interruptible_delay, run_update,
    };

    fn render_event(line: &str) -> Vec<u8> {
        let scratch = dot_test_support::TempDir::new("provider-phase-ingest").expect("scratch");
        let home = scratch.path().join("home");
        let root = scratch.path().to_path_buf();
        // A stub executable `jq` pins the JSON-decoding branch
        // deterministically: escapes decode to real control bytes
        // regardless of the host's PATH.
        let bin = scratch.path().join("bin");
        std::fs::create_dir(&bin).expect("stub bin");
        let stub = bin.join("jq");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("stub jq");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("stub mode");
        let env = std::collections::BTreeMap::from([
            (
                std::ffi::OsString::from("HOME"),
                home.as_os_str().to_owned(),
            ),
            (std::ffi::OsString::from("PATH"), bin.as_os_str().to_owned()),
            (
                std::ffi::OsString::from("DOT_SOURCE_ROOT"),
                root.as_os_str().to_owned(),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, scratch.path()).expect("runtime");
        let palette = crate::progress_ui::Palette::empty();
        let inputs = Inputs {
            runtime: &runtime,
            source_root: &root,
            home: home.to_str().expect("UTF-8 home"),
            config_home: home.to_str().expect("UTF-8 config"),
            state_home: home.to_str().expect("UTF-8 state"),
            policy: "pinned",
            force: false,
            quiet: false,
            verbose: false,
            update_jobs: None,
            palette: &palette,
            multibyte: false,
            ascii: true,
            bar_width: "8",
        };
        let mut state = crate::shdeps_ui::State::default();
        let mut session = crate::shdeps_ui_render::reset(false);
        let mut stage =
            crate::progress_ui::Stage::begin(palette.clone(), "5", false, true, false, true);
        let mut output = Vec::new();
        let mut live = true;
        handle_event(
            line.as_bytes(),
            &mut state,
            &mut session,
            &mut stage,
            &inputs,
            0,
            &mut output,
            &mut live,
            None,
        )
        .expect("phase event renders");
        output
    }

    fn render_phase_event(done: i64, total: i64) -> Vec<u8> {
        render_event(&format!(
            r#"{{"event":"phase","label":"Resolving","done":{done},"total":{total}}}"#
        ))
    }

    #[test]
    fn negative_phase_done_falls_back_to_label_only() {
        // Fresh-review-B P2-1 ingestion pin: a negative `done` from
        // untrusted provider JSONL must render exactly like the
        // `total <= 0` label-only fallback — no bar math on hostile
        // values. Each render uses a fresh stage so the live spinner
        // state cannot perturb the differential comparison.
        let fallback = render_phase_event(1, 0);
        assert!(fallback.windows(9).any(|window| window == b"Resolving"));
        for done in [i64::MIN, -1] {
            assert_eq!(render_phase_event(done, 1), fallback);
        }
    }

    #[test]
    fn provider_event_strings_render_without_controls_or_escapes() {
        // Fresh-review-B C7: decoded `\n`/ANSI in provider event strings
        // must not reach the rendered rows (the stub `jq` above makes the
        // JSON decoder turn `\\n`/`\\u001b` into real control bytes).
        // Assertions target the injected payloads, not raw framing bytes
        // (live lines legitimately start with `\r\x1b[K`).
        let warning = render_event(
            r#"{"event":"warning","status":"warn\u001b[31ming","detail":"line one\nline two"}"#,
        );
        assert!(
            warning
                .windows(b"line one line two".len())
                .any(|window| window == b"line one line two")
        );
        assert!(
            !warning
                .windows(b"line one\nline two".len())
                .any(|window| window == b"line one\nline two")
        );
        // The status cell truncates to width 8, so the full sanitized
        // status cannot appear; its 8-cell prefix proves ESC became a
        // space (`warn [31`, never `warn\x1b[31`).
        assert!(warning.windows(8).any(|window| window == b"warn [31"));
        assert!(!warning.contains(&0x1b));
        let phase = render_event(r#"{"event":"phase","label":"Bad\nLabel","done":1,"total":2}"#);
        assert!(phase.windows(9).any(|window| window == b"Bad Label"));
        assert!(!phase.windows(9).any(|window| window == b"Bad\nLabel"));
    }

    #[test]
    fn worker_thread_panics_become_error_values() {
        // Fresh-review-B C6: a panicking supervisor or deadline
        // watcher must read as a failed update, never crash the
        // process. The join sites map `Err` payloads through this
        // helper (a real worker panic cannot be injected
        // deterministically, so the helper pins the mapping).
        let error = super::join_panic_error("capture supervisor");
        assert_eq!(
            error.to_string(),
            "provider capture supervisor thread panicked"
        );
    }

    #[test]
    fn terminal_download_result_at_the_deadline_is_a_timeout() {
        use std::os::unix::process::ExitStatusExt as _;

        for raw_status in [0, 22 << 8] {
            let end = Ok::<_, std::io::Error>(crate::cleanup::SessionEnd::Exited(
                std::process::ExitStatus::from_raw(raw_status),
            ));
            assert!(matches!(
                classify_download_deadline(&end, true, 7),
                Some(EnsureFailure::DownloadTimeout(7))
            ));
        }
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed output"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct PanicWriter;

    impl std::io::Write for PanicWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            panic!("a failed first relay must not block on the second outward sink")
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct BlockingWriterState {
        entered: bool,
        release: bool,
    }

    struct BlockingWriter {
        state: std::sync::Arc<(std::sync::Mutex<BlockingWriterState>, std::sync::Condvar)>,
    }

    struct AbortAwareBlockingWriter {
        entered: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        saw_abort: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl std::io::Write for AbortAwareBlockingWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            let (lock, condition) = &*self.entered;
            *lock.lock().expect("abort-aware writer lock") = true;
            condition.notify_all();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !crate::cleanup::outward_write_aborted() {
                let guard = lock.lock().expect("abort-aware writer wait lock");
                let _ = condition
                    .wait_timeout(guard, std::time::Duration::from_millis(10))
                    .expect("abort-aware writer wait");
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "provider relay abort was never published",
                    ));
                }
            }
            // The relay pump cannot finish until this write returns, and the
            // abort latch is cleared only after the pump finishes, so this
            // observation cannot miss: the latch window stays open until the
            // blocked relay delivery itself witnesses the abort.
            self.saw_abort
                .store(true, std::sync::atomic::Ordering::Release);
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "intentional post-provider-exit relay abort",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn trusted_provider_cancellation_wins_over_intentional_queued_relay_abort() {
        let scratch = dot_test_support::TempDir::new_exec("provider-cancel-relay-abort")
            .expect("fixture directory");
        let root = scratch.path().join("source");
        let home = scratch.path().join("home");
        let config = scratch.path().join("config");
        let state_home = scratch.path().join("state");
        let tmp = scratch.path().join("tmp");
        for directory in [&root, &home, &config, &state_home, &tmp] {
            std::fs::create_dir_all(directory).expect("fixture directory");
        }
        let provider = scratch.path().join("provider");
        let release = scratch.path().join("provider-release");
        std::fs::write(
            &provider,
            b"#!/bin/sh\nprintf '%s\\n' 'queued before cancellation' >&2\ni=0\nwhile [ \"$i\" -lt 6000 ] && [ ! -e \"$DOT_TEST_PROVIDER_CANCEL_RELEASE\" ]; do i=$((i + 1)); sleep 0.01; done\nexit 130\n",
        )
        .expect("provider fixture");
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755))
            .expect("provider fixture mode");
        let env = std::collections::BTreeMap::from([
            (
                std::ffi::OsString::from("HOME"),
                home.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("PATH"),
                std::ffi::OsString::from("/usr/bin:/bin"),
            ),
            (
                std::ffi::OsString::from("TMPDIR"),
                tmp.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("DOT_SOURCE_ROOT"),
                root.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("DOT_TEST_PROVIDER_CANCEL_RELEASE"),
                release.as_os_str().to_owned(),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, scratch.path()).expect("runtime");
        let palette = crate::progress_ui::Palette::empty();
        let inputs = Inputs {
            runtime: &runtime,
            source_root: &root,
            home: home.to_str().expect("UTF-8 home"),
            config_home: config.to_str().expect("UTF-8 config"),
            state_home: state_home.to_str().expect("UTF-8 state"),
            policy: "pinned",
            force: false,
            quiet: false,
            verbose: false,
            update_jobs: None,
            palette: &palette,
            multibyte: false,
            ascii: true,
            bar_width: "8",
        };
        let ready = Ready {
            binary: provider,
            directory: scratch.path().to_path_buf(),
            env,
            _snapshot: None,
        };
        let mut stage =
            crate::progress_ui::Stage::begin(palette.clone(), "5", false, false, false, true);
        let entered =
            std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut output = Vec::new();
        let saw_abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut errors = AbortAwareBlockingWriter {
            entered: entered.clone(),
            saw_abort: saw_abort.clone(),
        };
        let release_after_block = {
            let entered = entered.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                let (lock, condition) = &*entered;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                let mut observed = lock.lock().expect("writer state");
                while !*observed && std::time::Instant::now() < deadline {
                    let (next, _) = condition
                        .wait_timeout(observed, std::time::Duration::from_millis(10))
                        .expect("writer-state wait");
                    observed = next;
                }
                let entered = *observed;
                drop(observed);
                if entered {
                    std::fs::write(release, b"exit").expect("release provider");
                }
                entered
            })
        };
        let watchdog_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog_saw_abort = saw_abort.clone();
        let watchdog_fired_by_watchdog = watchdog_fired.clone();
        let watchdog = std::thread::spawn(move || {
            // The blocked relay delivery witnesses the abort causally: the
            // latch window cannot close until its write returns. A polling
            // observer can still miss a sub-millisecond window under load,
            // so the backstop fires only when neither the latch nor the
            // writer has shown an abort.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if crate::cleanup::outward_write_aborted()
                    || watchdog_saw_abort.load(std::sync::atomic::Ordering::Acquire)
                {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if crate::cleanup::outward_write_aborted() {
                // Healthy product, starved observer: the latch is set but
                // the writer thread has not been scheduled yet. Its
                // observation is inevitable (the window stays open), so
                // wait it out instead of republishing a cleared latch.
                let extended = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::time::Instant::now() < extended
                    && !watchdog_saw_abort.load(std::sync::atomic::Ordering::Acquire)
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                return;
            }
            // Bound a RED implementation that leaves the pump waiting after
            // the provider has already terminated.
            watchdog_fired_by_watchdog.store(true, std::sync::atomic::Ordering::Release);
            crate::cleanup::abort_outward_writes();
        });

        let outcome = run_update(&inputs, &ready, &mut stage, 0, &mut output, &mut errors);

        assert!(release_after_block.join().expect("provider release thread"));
        watchdog.join().expect("provider abort watchdog");
        assert!(
            saw_abort.load(std::sync::atomic::Ordering::Acquire),
            "trusted provider cancellation did not abort queued relay output"
        );
        assert!(
            !watchdog_fired.load(std::sync::atomic::Ordering::Acquire),
            "trusted provider cancellation was slower than its abort bound"
        );
        assert_eq!(outcome.status, 130);
        assert_eq!(outcome.interrupted, Some(libc::SIGINT));
        assert!(outcome.abort);
        assert_eq!(outcome.summary, b"dependency update interrupted");
    }

    impl std::io::Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let (lock, condition) = &*self.state;
            let mut state = lock.lock().expect("blocking writer lock");
            state.entered = true;
            condition.notify_all();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !state.release {
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "blocking writer was never released",
                    ));
                };
                let (next, timed) = condition
                    .wait_timeout(state, remaining)
                    .expect("blocking writer wait");
                state = next;
                if timed.timed_out() && !state.release {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "blocking writer was never released",
                    ));
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocking_outward_sink_does_not_block_provider_cancellation() {
        use std::os::unix::fs::PermissionsExt as _;

        const HELPER: &str = "DOT_PROVIDER_BLOCKING_WRITER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            #[cfg(not(target_os = "android"))]
            let executable = std::env::current_exe().expect("test executable");
            #[cfg(target_os = "android")]
            let executable = std::path::PathBuf::from(
                std::env::args_os().next().expect("test executable argv[0]"),
            );
            let mut child = std::process::Command::new(executable)
                .args([
                    "--exact",
                    "shdeps_provider::tests::blocking_outward_sink_does_not_block_provider_cancellation",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .expect("blocking-writer helper");
            // Sender budget is 10s to enter plus 6s to clean; keep the
            // parent watchdog above their sum.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
            let status = loop {
                if let Some(status) = child.try_wait().expect("observe helper") {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().expect("stop stuck helper");
                    child.wait().expect("reap stuck helper");
                    panic!("blocking-writer helper exceeded its deadline");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            assert!(
                status.success(),
                "blocking-writer helper failed: {status:?}"
            );
            return;
        }

        let scratch = dot_test_support::TempDir::new_exec("provider-blocking-writer")
            .expect("fixture directory");
        let root = scratch.path().join("source");
        let home = scratch.path().join("home");
        let config = scratch.path().join("config");
        let state_home = scratch.path().join("state");
        let tmp = scratch.path().join("tmp");
        for directory in [&root, &home, &config, &state_home, &tmp] {
            std::fs::create_dir_all(directory).expect("fixture directory");
        }
        let stopped = scratch.path().join("provider-stopped");
        let provider = scratch.path().join("provider");
        std::fs::write(
            &provider,
            b"#!/bin/sh\ntrap ': >\"$DOT_TEST_PROVIDER_STOPPED\"; exit 143' TERM\nprintf '%s\\n' '{\"event\":\"warning\",\"status\":\"warning\",\"detail\":\"blocking sink\"}'\nwhile :; do sleep 0.05; done\n",
        )
        .expect("provider fixture");
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755))
            .expect("provider fixture mode");
        let env = std::collections::BTreeMap::from([
            (
                std::ffi::OsString::from("HOME"),
                home.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("PATH"),
                std::ffi::OsString::from("/usr/bin:/bin"),
            ),
            (
                std::ffi::OsString::from("TMPDIR"),
                tmp.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("DOT_SOURCE_ROOT"),
                root.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("DOT_TEST_PROVIDER_STOPPED"),
                stopped.as_os_str().to_owned(),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, scratch.path()).expect("runtime");
        let palette = crate::progress_ui::Palette::empty();
        let inputs = Inputs {
            runtime: &runtime,
            source_root: &root,
            home: home.to_str().expect("UTF-8 home"),
            config_home: config.to_str().expect("UTF-8 config"),
            state_home: state_home.to_str().expect("UTF-8 state"),
            policy: "pinned",
            force: false,
            quiet: false,
            verbose: false,
            update_jobs: None,
            palette: &palette,
            multibyte: false,
            ascii: true,
            bar_width: "8",
        };
        let ready = Ready {
            binary: provider,
            directory: scratch.path().to_path_buf(),
            env,
            _snapshot: None,
        };
        let mut stage =
            crate::progress_ui::Stage::begin(palette.clone(), "5", false, false, false, true);
        let shared = std::sync::Arc::new((
            std::sync::Mutex::new(BlockingWriterState::default()),
            std::sync::Condvar::new(),
        ));
        let sender_state = shared.clone();
        let stopped_for_sender = stopped.clone();
        let signals = crate::cleanup::Signals::install().expect("signal owner");
        let sender = std::thread::spawn(move || {
            let (lock, condition) = &*sender_state;
            // Provider spawn plus first-event delivery runs under the full
            // parallel suite on hosted runners; macOS needs more than the
            // original 4s tail budget.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut state = lock.lock().expect("sender lock");
            while !state.entered && std::time::Instant::now() < deadline {
                let (next, _) = condition
                    .wait_timeout(state, std::time::Duration::from_millis(20))
                    .expect("sender wait");
                state = next;
            }
            let entered = state.entered;
            drop(state);
            if entered {
                // SAFETY: this helper installed a synchronous SIGINT owner.
                assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);
            }
            let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while !stopped_for_sender.exists() && std::time::Instant::now() < cleanup_deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let cleaned_while_blocked = entered && stopped_for_sender.exists();
            let mut state = lock.lock().expect("release lock");
            state.release = true;
            condition.notify_all();
            (entered, stopped_for_sender.exists(), cleaned_while_blocked)
        });
        let mut output = BlockingWriter { state: shared };
        let mut errors = Vec::new();

        let outcome = run_update(&inputs, &ready, &mut stage, 0, &mut output, &mut errors);

        let (entered, stopped, cleaned_while_blocked) = sender.join().expect("signal sender");
        assert!(
            entered,
            "provider never delivered its first event to the outward sink"
        );
        assert!(
            stopped,
            "provider trap did not run while the outward sink was blocked"
        );
        assert!(
            cleaned_while_blocked,
            "provider cleanup waited for the outward sink to unblock"
        );
        assert!(outcome.abort, "interrupted provider update did not abort");
        assert_eq!(signals.finish(outcome.status), 128 + libc::SIGINT);
    }

    fn assert_capture_empty(capture: &mut CaptureStream) {
        let mut byte = [0];
        match capture.reader.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(count) => panic!("capture retained {count} byte(s) after the failing tick"),
            Err(error) => panic!("could not inspect drained capture: {error}"),
        }
    }

    #[derive(Default)]
    struct SucceedThenFail {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl std::io::Write for SucceedThenFail {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            } else {
                Err(std::io::Error::other("closed output"))
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn download_output_failure_does_not_retry() {
        let scratch =
            dot_test_support::TempDir::new_exec("shdeps-download-output").expect("scratch");
        let root = scratch.path().join("source");
        let home = scratch.path().join("home");
        let config = scratch.path().join("config");
        let state = scratch.path().join("state");
        let tmp = scratch.path().join("tmp");
        let bin = scratch.path().join("bin");
        for path in [
            root.join("support"),
            home.clone(),
            config.clone(),
            state.clone(),
            tmp.clone(),
            bin.clone(),
        ] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        std::fs::write(
            root.join("support/shdeps.lock"),
            b"revision=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ninstall_sha256=0000000000000000000000000000000000000000000000000000000000000000\nabi=1\n",
        )
        .expect("provider lock");
        let attempts = scratch.path().join("attempts");
        let curl = bin.join("curl");
        std::fs::write(
            &curl,
            b"#!/bin/sh\nprintf 'attempt\\n' >>\"$DOT_TEST_CURL_RECORD\"\nprintf 'diagnostic\\n' >&2\nexit 22\n",
        )
        .expect("curl fixture");
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).expect("curl mode");
        let env = std::collections::BTreeMap::from([
            (
                std::ffi::OsString::from("HOME"),
                home.as_os_str().to_owned(),
            ),
            (std::ffi::OsString::from("PATH"), bin.as_os_str().to_owned()),
            (
                std::ffi::OsString::from("TMPDIR"),
                tmp.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("DOT_TEST_CURL_RECORD"),
                attempts.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS"),
                std::ffi::OsString::from("0"),
            ),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, scratch.path()).expect("runtime");
        let palette = crate::progress_ui::Palette::empty();
        let inputs = Inputs {
            runtime: &runtime,
            source_root: &root,
            home: home.to_str().expect("UTF-8 home"),
            config_home: config.to_str().expect("UTF-8 config"),
            state_home: state.to_str().expect("UTF-8 state"),
            policy: "pinned",
            force: false,
            quiet: false,
            verbose: false,
            update_jobs: None,
            palette: &palette,
            multibyte: false,
            ascii: true,
            bar_width: "8",
        };
        let mut stdout = Vec::new();
        let mut stderr = FailingWriter;

        assert!(download_installer(&inputs, &mut stdout, &mut stderr).is_err());
        assert_eq!(
            std::fs::read(&attempts).expect("curl attempts"),
            b"attempt\n",
            "output delivery failure retried a mutating network operation"
        );
    }

    #[test]
    fn retry_delay_observes_a_signal_after_waiting_begins() {
        const HELPER: &str = "DOT_SHDEPS_RETRY_SIGNAL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            #[cfg(not(target_os = "android"))]
            let executable = std::env::current_exe().unwrap();
            #[cfg(target_os = "android")]
            let executable = std::path::PathBuf::from(
                std::env::args_os().next().expect("test executable argv[0]"),
            );
            let output = std::process::Command::new(executable)
                .args([
                    "--exact",
                    "shdeps_provider::tests::retry_delay_observes_a_signal_after_waiting_begins",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "retry-delay helper failed with {:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let signals = crate::cleanup::Signals::install().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let sender_barrier = barrier.clone();
        let sender = std::thread::spawn(move || {
            sender_barrier.wait();
            std::thread::sleep(std::time::Duration::from_millis(50));
            // SAFETY: this helper installed a handler for SIGINT.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);
        });
        barrier.wait();
        let started = std::time::Instant::now();
        assert_eq!(
            interruptible_delay(std::time::Duration::from_secs(30)),
            Some(libc::SIGINT)
        );
        sender.join().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(signals.finish(0), 128 + libc::SIGINT);
    }

    #[test]
    fn final_output_drain_does_not_require_writer_eof() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("capture pair");
        reader.set_nonblocking(true).expect("nonblocking capture");
        let mut capture = CaptureStream {
            reader,
            writer: None,
        };
        writer.write_all(b"complete\nfinal-event").expect("events");
        let mut pending = Vec::new();
        let mut events = Vec::new();
        let mut remaining = 1024;
        drain_provider_output(&mut capture, &mut pending, true, &mut remaining, |event| {
            events.push(event);
            Ok(())
        })
        .expect("final drain");
        assert_eq!(events, [b"complete\n".to_vec(), b"final-event".to_vec()]);
    }

    #[test]
    fn provider_relay_drains_both_streams_and_never_replays_after_failure() {
        let (stdout_reader, mut stdout_writer) =
            std::os::unix::net::UnixStream::pair().expect("stdout capture pair");
        stdout_reader
            .set_nonblocking(true)
            .expect("nonblocking stdout capture");
        let mut stdout = CaptureStream {
            reader: stdout_reader,
            writer: None,
        };
        let (stderr_reader, mut stderr_writer) =
            std::os::unix::net::UnixStream::pair().expect("stderr capture pair");
        stderr_reader
            .set_nonblocking(true)
            .expect("nonblocking stderr capture");
        let mut stderr = CaptureStream {
            reader: stderr_reader,
            writer: None,
        };
        stdout_writer
            .write_all(b"first\nsecond\nthird\n")
            .expect("stdout events");
        stderr_writer
            .write_all(b"cleanup diagnostic\n")
            .expect("stderr event");
        drop(stdout_writer);
        drop(stderr_writer);
        let mut pending = Vec::new();
        let mut output = SucceedThenFail::default();
        let mut stderr_output = PanicWriter;
        let mut drain_state = ProviderDrainState {
            remaining_bytes: 1024,
            failed: false,
        };

        let result = drain_provider_streams(
            &mut stdout,
            &mut pending,
            &mut stderr,
            true,
            &mut stderr_output,
            &mut drain_state,
            |line| output.write_all(&line),
        );

        assert_eq!(result.unwrap_err().to_string(), "closed output");
        assert!(drain_state.failed);
        assert_eq!(output.bytes, b"first\n");
        assert_capture_empty(&mut stderr);
        let mut replayed = Vec::new();
        drain_provider_streams(
            &mut stdout,
            &mut pending,
            &mut stderr,
            true,
            &mut std::io::sink(),
            &mut drain_state,
            |line| {
                replayed.push(line);
                Ok(())
            },
        )
        .expect("discard after relay failure");
        assert!(replayed.is_empty(), "failed events were replayed");
        assert!(pending.is_empty(), "failed relay retained pending events");
    }

    #[test]
    fn provider_run_budget_is_cumulative_across_stdout_and_stderr() {
        let (stdout_reader, mut stdout_writer) =
            std::os::unix::net::UnixStream::pair().expect("stdout capture pair");
        stdout_reader
            .set_nonblocking(true)
            .expect("nonblocking stdout capture");
        let mut stdout = CaptureStream {
            reader: stdout_reader,
            writer: None,
        };
        let (stderr_reader, mut stderr_writer) =
            std::os::unix::net::UnixStream::pair().expect("stderr capture pair");
        stderr_reader
            .set_nonblocking(true)
            .expect("nonblocking stderr capture");
        let mut stderr = CaptureStream {
            reader: stderr_reader,
            writer: None,
        };
        stdout_writer.write_all(b"event\n").expect("stdout event");
        stderr_writer.write_all(b"detail").expect("stderr bytes");
        drop(stdout_writer);
        drop(stderr_writer);
        let mut pending = Vec::new();
        let mut stderr_output = Vec::new();
        let mut drain_state = ProviderDrainState {
            remaining_bytes: 10,
            failed: false,
        };

        let error = drain_provider_streams(
            &mut stdout,
            &mut pending,
            &mut stderr,
            true,
            &mut stderr_output,
            &mut drain_state,
            |_| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), PROVIDER_OUTPUT_LIMIT_ERROR);
        assert!(drain_state.failed);
        assert_eq!(drain_state.remaining_bytes, 0);
        assert!(stderr_output.is_empty());
        assert_capture_empty(&mut stderr);
    }

    #[test]
    fn raw_relay_never_reenters_outputs_after_first_failure() {
        let (stdout_reader, mut stdout_writer) =
            std::os::unix::net::UnixStream::pair().expect("stdout capture pair");
        stdout_reader
            .set_nonblocking(true)
            .expect("nonblocking stdout capture");
        let mut stdout = CaptureStream {
            reader: stdout_reader,
            writer: None,
        };
        let (stderr_reader, mut stderr_writer) =
            std::os::unix::net::UnixStream::pair().expect("stderr capture pair");
        stderr_reader
            .set_nonblocking(true)
            .expect("nonblocking stderr capture");
        let mut stderr = CaptureStream {
            reader: stderr_reader,
            writer: None,
        };
        stdout_writer.write_all(b"first failure").expect("stdout");
        stderr_writer
            .write_all(b"first diagnostic")
            .expect("stderr");
        let mut failed = false;

        let error = drain_capture_streams(
            &mut stdout,
            &mut FailingWriter,
            &mut stderr,
            &mut PanicWriter,
            false,
            &mut failed,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "closed output");
        assert!(failed);
        assert_capture_empty(&mut stderr);

        stderr_writer.write_all(b"late diagnostic").expect("stderr");
        drain_capture_streams(
            &mut stdout,
            &mut PanicWriter,
            &mut stderr,
            &mut PanicWriter,
            true,
            &mut failed,
        )
        .expect("drain-only retry");
    }

    #[test]
    fn capture_budget_yields_before_writer_eof() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("capture pair");
        reader.set_nonblocking(true).expect("nonblocking capture");
        let mut capture = CaptureStream {
            reader,
            writer: None,
        };
        writer.write_all(b"overflow").expect("captured bytes");
        let mut bytes = Vec::new();

        capture
            .drain_with_budget(&mut bytes, 4)
            .expect("bounded drain");
        assert_eq!(bytes, b"over");

        capture
            .drain_with_budget(&mut bytes, 4)
            .expect("next bounded drain");
        assert_eq!(bytes, b"overflow");
    }

    #[test]
    fn newline_free_provider_frame_has_a_stable_size_failure() {
        let mut pending = Vec::new();
        let error =
            consume_provider_bytes(&mut pending, b"oversized", false, 4, |_| Ok(())).unwrap_err();

        assert_eq!(error.to_string(), PROVIDER_OUTPUT_LIMIT_ERROR);
        assert!(pending.len() <= 4);
    }

    #[test]
    fn deferred_provider_bytes_are_split_once_in_order() {
        let mut pending = Vec::new();
        let mut frames = Vec::new();
        consume_provider_bytes(&mut pending, b"first\nsecond\nthird", true, 64, |frame| {
            frames.push(frame);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            frames,
            [b"first\n".to_vec(), b"second\n".to_vec(), b"third".to_vec()]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn aggregate_probe_capture_has_a_stable_size_failure() {
        let mut bytes = Vec::new();
        let mut output = LimitedWriter::new(&mut bytes, 4);

        output.write_all(b"1234").unwrap();
        assert_eq!(
            output.write_all(b"5").unwrap_err().to_string(),
            PROVIDER_OUTPUT_LIMIT_ERROR
        );
        assert_eq!(bytes, b"1234");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_discards_already_read_bytes_after_output_failure() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("capture pair");
        reader.set_nonblocking(true).expect("nonblocking capture");
        let mut capture = CaptureStream {
            reader,
            writer: None,
        };
        writer
            .write_all(&vec![b'x'; 16 * 1024])
            .expect("captured bytes");
        drop(writer);

        assert!(
            capture
                .drain_with_budget(&mut FailingWriter, 32 * 1024)
                .is_err(),
            "closed output sink must remain an operation failure"
        );
        let mut replay = Vec::new();
        capture
            .drain_with_budget(&mut replay, 32 * 1024)
            .expect("drain after failed delivery");
        assert!(
            replay.is_empty(),
            "bytes consumed after a partial delivery failure must not replay"
        );
    }
}
