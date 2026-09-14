//! Owned resources and POSIX process lifecycle.
//!
//! Registry owns explicitly registered children, files and temporary paths.
//! Callers invoke its idempotent cleanup; scoped owners provide Drop where
//! required. Native suite supervision additionally reserves each session
//! leader until descendant teardown completes, then reaps through Child.
//!
//! std owns spawn, file I/O and reaping. This module centralizes the small
//! libc boundary for signal handlers, session identity, non-reaping waitid,
//! descendant adoption, and TERM/group delivery, which std does not expose. Handler callbacks
//! only publish an atomic cancellation flag; resource cleanup runs normally.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::errors::{Error, Result};

/// TERM grace: 20 attempts × 50ms, mirroring
/// `DOT_CLEANUP_GRACE_ATTEMPTS=20` and `sleep 0.05`.
pub const GRACE_ATTEMPTS: u32 = 20;
/// Milliseconds between grace polls.
pub const GRACE_INTERVAL_MS: u64 = 50;

/// Shell validation rule for registry PIDs (`^[1-9][0-9]*$`): positive,
/// no leading zero, all digits. Ported as a pure predicate so the
/// update-lock slice (which deals in FOREIGN pids) shares the rule.
pub fn valid_pid(text: &str) -> bool {
    !text.is_empty() && text.as_bytes()[0] != b'0' && text.bytes().all(|b| b.is_ascii_digit())
}

/// Shell rule for the optional launch group: empty, or exactly the
/// leader PID (arbitrary numeric PGIDs never enter the registry).
pub fn valid_group(pid_text: &str, group_text: &str) -> bool {
    group_text.is_empty() || group_text == pid_text
}

/// Owned resources awaiting teardown.
#[derive(Debug, Default)]
pub struct Registry {
    children: Vec<Child>,
    paths: Vec<PathBuf>,
    files: Vec<File>,
    running: bool,
}

impl Registry {
    /// Empty registry.
    pub fn new() -> Self {
        Registry::default()
    }

    /// Track an owned child for TERM/KILL escalation and reaping.
    pub fn track_child(&mut self, child: Child) {
        self.children.push(child);
    }

    /// Stop tracking the child with this pid. Returns whether one was
    /// present (the shell unregisters every match; PIDs are unique here
    /// by handle construction).
    pub fn untrack_child(&mut self, pid: u32) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id() != pid);
        self.children.len() != before
    }

    /// Number of tracked children (for tests).
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Register a temp path for removal at cleanup. Empty paths are a
    /// usage error (shell exit 2), like the shell registry.
    pub fn register_path(&mut self, path: &Path) -> Result<()> {
        if path.as_os_str().is_empty() {
            return Err(Error::Usage {
                message: "cleanup path must not be empty",
            });
        }
        self.paths.push(path.to_path_buf());
        Ok(())
    }

    /// Forget a path without removing it.
    pub fn unregister_path(&mut self, path: &Path) {
        self.paths.retain(|owned| owned != path);
    }

    /// Number of registered paths (for tests).
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// Hold an open file for closing at cleanup (drop closes it).
    pub fn hold_file(&mut self, file: File) {
        self.files.push(file);
    }

    /// Remove one owned path now, mirroring
    /// `_dot_cleanup_remove_path`: unregister first so a path recreated
    /// after removal cannot be deleted by a later cleanup as though it
    /// were still the original object; on failure RE-REGISTER (the path
    /// is still ours and must not leak) and report the error.
    pub fn remove_path(&mut self, path: &Path) -> Result<()> {
        self.unregister_path(path);
        match remove_one(path) {
            Ok(()) => Ok(()),
            Err(source) => {
                self.paths.push(path.to_path_buf());
                Err(Error::Io {
                    context: "cleanup could not remove path",
                    source,
                })
            }
        }
    }

    /// Run the full teardown exactly once: TERM grace for children, KILL
    /// escalation, reap, close files, remove paths (individual path
    /// failures do not abort the pass, like the shell's `|| true`).
    /// Reentrant calls while running, or after completion, return
    /// immediately (shell: `[[ RUNNING -eq 0 ]] || return 0`).
    pub fn cleanup(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
        terminate_children(&mut self.children);
        // Files close on drop; drain explicitly so descriptor release
        // precedes path removal, matching the shell's fd-then-path order.
        self.files.clear();
        let paths = std::mem::take(&mut self.paths);
        for path in &paths {
            let _ = remove_one(path);
        }
        self.running = false;
    }
}

/// `rm -rf` for one path: symlinks remove the LINK (never the target),
/// directories remove recursively, anything else removes as a file.
/// Missing paths are success (shell `rm -rf` semantics).
fn remove_one(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
        Ok(meta) => {
            if meta.file_type().is_symlink() || meta.file_type().is_file() {
                std::fs::remove_file(path)
            } else if meta.file_type().is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                // Sockets, fifos, devices: unlink like `rm`.
                std::fs::remove_file(path)
            }
        }
    }
}

/// Send TERM to a registered child before bounded grace and KILL escalation.
fn terminate(pid: u32) {
    signal_pid(pid, libc::SIGTERM);
}

/// Deliver a signal to a positive process identity held by the caller.
pub(crate) fn signal_pid(pid: u32, signal: i32) {
    if let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) {
        // SAFETY: positive PID, no pointer arguments. Callers retain ownership.
        unsafe { libc::kill(pid, signal) };
    }
}

/// Deliver a signal to the group of an unreaped owned leader.
pub(crate) fn signal_group(pid: u32, signal: i32) {
    if let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) {
        // SAFETY: only the explicitly owned group is addressed.
        unsafe { libc::kill(-pid, signal) };
    }
}

/// Effective process identity, never an environment-provided value.
pub(crate) fn euid() -> u32 {
    // SAFETY: get effective uid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Conservative foreign-process liveness for stale-directory pruning.
pub(crate) fn alive(pid: i32) -> bool {
    // SAFETY: positive PID and signal zero only probe existence.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

const SIGNAL_CLOSED: i32 = -1;
static INTERRUPTED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static ACTIVE_HANDLERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SIGNAL_OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
const HANDLED_SIGNALS: [i32; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

extern "C" fn interrupted(signal: i32) {
    // A signal handler must neither allocate nor lock nor touch owned resources.
    ACTIVE_HANDLERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _ = INTERRUPTED.compare_exchange(
        0,
        signal,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    );
    ACTIVE_HANDLERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
}

/// Process-local handler guard. Process-wide dispositions admit only one owner
/// even when public command entry points are invoked by concurrent threads.
pub(crate) struct Signals {
    previous: Vec<(i32, libc::sigaction)>,
    _owner: Option<std::sync::MutexGuard<'static, ()>>,
    restore: bool,
    active: bool,
}

impl Signals {
    #[cfg(test)]
    pub(crate) fn install() -> std::io::Result<Self> {
        Self::install_with_restore(true)
    }

    /// Own signals only for a real process entry. Embedded invocations are
    /// signal-neutral and isolate through [`crate::app::run`] instead.
    pub(crate) fn for_runtime(runtime: &crate::app::Runtime) -> std::io::Result<Self> {
        if runtime.is_process_entry() {
            Self::install_with_restore(false)
        } else {
            Ok(Self {
                previous: Vec::new(),
                _owner: None,
                restore: false,
                active: false,
            })
        }
    }

    fn install_with_restore(restore: bool) -> std::io::Result<Self> {
        let owner = SIGNAL_OWNER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        INTERRUPTED.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut guard = Self {
            previous: Vec::new(),
            _owner: Some(owner),
            // A partial install must restore what it changed on error. Switch
            // to process-lifetime ownership only after all actions succeed.
            restore: true,
            active: true,
        };
        for signal in HANDLED_SIGNALS {
            // SAFETY: zero initialization is valid for sigaction, the mask is
            // initialized explicitly, and saved actions outlive each call.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = interrupted as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut previous) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                guard.previous.push((signal, previous));
            }
        }
        guard.restore = restore;
        Ok(guard)
    }

    pub(crate) fn received(&self) -> Option<i32> {
        self.active.then(received_signal).flatten()
    }

    /// Wrap an output sink so a handled signal can break a blocked write.
    pub(crate) fn writer<'a>(&'a self, inner: &'a mut dyn std::io::Write) -> SignalWriter<'a> {
        SignalWriter {
            inner,
            signals: self,
        }
    }

    /// Restore the caller's dispositions and return the final command status
    /// without losing a signal in the load-versus-restore window.
    pub(crate) fn finish(self, code: i32) -> i32 {
        self.finish_signal().map_or(code, |signal| 128 + signal)
    }

    /// Preserve an operational error unless cancellation owns the result.
    /// This keeps fallible rendering and cleanup paths from dropping a signal.
    pub(crate) fn finish_result<E>(
        self,
        result: std::result::Result<i32, E>,
    ) -> std::result::Result<i32, E> {
        match self.finish_signal() {
            Some(signal) => Ok(128 + signal),
            None => result,
        }
    }

    fn finish_signal(mut self) -> Option<i32> {
        self.close()
    }

    fn close(&mut self) -> Option<i32> {
        if !self.active {
            return None;
        }
        // Stop new callbacks before closing the latch. A callback already in
        // flight remains allowed to publish the first signal, so a delivery
        // immediately before the disposition change cannot disappear.
        self.ignore();
        // `sigaction(SIG_IGN)` prevents new callbacks and discards pending
        // instances. Wait for callbacks that were already executing before a
        // subsequent guard can reset the process-global latch.
        while ACTIVE_HANDLERS.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
        let received = match INTERRUPTED.swap(SIGNAL_CLOSED, std::sync::atomic::Ordering::SeqCst) {
            signal if signal > 0 => Some(signal),
            _ => None,
        };
        if self.restore {
            self.restore();
        } else {
            // The binary exits immediately after returning the final status.
            // Keep later handled signals ignored instead of reopening a race
            // between restored dispositions and `process::exit`.
            self.previous.clear();
        }
        self.active = false;
        received
    }

    fn ignore(&self) {
        for &(signal, _) in &self.previous {
            // SAFETY: zero initialization is valid for sigaction; SIG_IGN is
            // the standard disposition and every signal is handled above.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }

    fn restore(&mut self) {
        for (signal, action) in self.previous.drain(..) {
            // SAFETY: restore the exact initialized action saved at install.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        }
    }
}

/// A writer that turns a latched signal into a non-retriable I/O error.
///
/// `write_all` retries `Interrupted`, so returning that error kind would leave
/// a command stuck when its consumer stops reading. `Other` escapes the retry
/// loop; the owning [`Signals`] guard then converts the command result to the
/// conventional signal status.
pub(crate) struct SignalWriter<'a> {
    inner: &'a mut dyn std::io::Write,
    signals: &'a Signals,
}

impl std::io::Write for SignalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.signals.received().is_some() {
            return Err(signal_io_error());
        }
        match self.inner.write(bytes) {
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted
                    && self.signals.received().is_some() =>
            {
                Err(signal_io_error())
            }
            result => result,
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.signals.received().is_some() {
            return Err(signal_io_error());
        }
        match self.inner.flush() {
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted
                    && self.signals.received().is_some() =>
            {
                Err(signal_io_error())
            }
            result => result,
        }
    }
}

fn signal_io_error() -> std::io::Error {
    std::io::Error::other("interrupted by signal")
}

/// Return the signal captured by the invocation owner, if any.
///
/// Worker threads cannot borrow the guard that owns the process handler, but
/// they must still stop their isolated child sessions before the top-level
/// command returns the conventional `128 + signal` status.
pub(crate) fn received_signal() -> Option<i32> {
    match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        signal if signal > 0 => Some(signal),
        _ => None,
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Start a child session before exec; all work in this post-fork closure is
/// async-signal-safe. No allocation or Rust runtime operation runs there.
pub(crate) fn isolate(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: setsid has no memory arguments and is safe between fork/exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Adopt orphaned suite descendants so their lifecycle never depends on an
/// outer init process reaping promptly. Other Unix platforms reap through
/// their init process because Linux's subreaper facility is not available.
pub(crate) fn adopt_descendants() -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: prctl has no pointer arguments for PR_SET_CHILD_SUBREAPER.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Observe a child's exit without releasing its PID/session identity. A
/// retained zombie leader prevents reuse throughout descendant teardown.
pub(crate) fn exited(child: &Child) -> std::io::Result<bool> {
    // SAFETY: siginfo is initialized; waitid writes only this local value.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(info.si_pid() != 0)
    }
}

/// Snapshot all live processes once per polling pass, so a busy first session
/// cannot consume the discovery budget allocated to later workers.
fn process_snapshot(deadline: Instant) -> Option<Vec<(u32, u32, bool)>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Ok(entries) = std::fs::read_dir("/proc") {
        let mut members = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read(entry.path().join("stat")) else {
                continue;
            };
            let end = stat.windows(2).rposition(|part| part == b") ")?;
            let fields: Vec<_> = stat[end + 2..]
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty())
                .collect();
            let sid = fields
                .get(3)
                .and_then(|field| std::str::from_utf8(field).ok())
                .and_then(|field| field.parse::<u32>().ok());
            if let Some(sid) = sid {
                members.push((pid, sid, fields.first() != Some(&b"Z".as_slice())));
            }
        }
        return Some(members);
    }
    // This is an OS process-table interface, not a caller-selected tool.
    #[cfg(target_os = "android")]
    let mut command = Command::new("/system/bin/ps");
    #[cfg(not(target_os = "android"))]
    let mut command = Command::new("/bin/ps");
    command.args(["-A", "-o", "pid=,stat="]);
    let bytes = snapshot(command, deadline)?;
    Some(
        String::from_utf8(bytes)
            .ok()?
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse::<u32>().ok()?;
                let live = !fields.next()?.starts_with('Z');
                session_id(pid).map(|sid| (pid, sid, live))
            })
            .collect(),
    )
}

fn same_session(pid: u32, sid: u32) -> bool {
    session_id(pid) == Some(sid)
}

fn session_id(pid: u32) -> Option<u32> {
    // SAFETY: getsid is an observation with no pointer arguments.
    let sid = unsafe { libc::getsid(pid as i32) };
    u32::try_from(sid).ok()
}

fn process_group(pid: u32) -> Option<u32> {
    // SAFETY: getpgid is an observation with no pointer arguments.
    let group = unsafe { libc::getpgid(pid as i32) };
    u32::try_from(group).ok()
}

/// Capture a portable process snapshot without a blocking pipe reader thread.
fn snapshot(mut command: Command, deadline: Instant) -> Option<Vec<u8>> {
    if Instant::now() >= deadline {
        return None;
    }
    let (reader, writer) = std::os::unix::net::UnixStream::pair().ok()?;
    reader.set_nonblocking(true).ok()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Command retains configured descriptors after spawn; release our writer
    // so child EOF is observable instead of timing out every valid snapshot.
    drop(command);
    let mut bytes = Vec::new();
    let mut reader = reader;
    use std::io::Read as _;
    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let mut chunk = [0; 8192];
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    // EOF is independent of process exit: a helper may close stdout early.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then_some(bytes),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Stop all still-owned session members, including descendants that created
/// another process group. Keep the leader unreaped until the final signal.
pub(crate) fn stop_session(
    child: &mut Child,
    first_signal: i32,
) -> std::io::Result<std::process::ExitStatus> {
    stop_sessions(std::slice::from_mut(child), first_signal)
        .pop()
        .expect("one owned child")
}

/// Stop a worker wave under one shared grace deadline. Each retained child
/// reserves its own SID until every final signal has been delivered.
pub(crate) fn stop_sessions(
    children: &mut [Child],
    first_signal: i32,
) -> Vec<std::io::Result<std::process::ExitStatus>> {
    // The budget starts before discovery: degraded BSD ps probes must not
    // allocate a fresh second for every worker or every polling pass.
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut sessions: Vec<_> = children
        .iter()
        .map(|child| Session {
            leader: child.id(),
            members: std::collections::BTreeSet::new(),
            signaled: std::collections::BTreeSet::new(),
            group_signaled: false,
        })
        .collect();
    // Start graceful shutdown immediately. Session discovery must not consume
    // the grace budget before the owned leader and its original group see the
    // first signal.
    for session in &mut sessions {
        session.signal_group_once(first_signal);
    }
    let mut consecutive_empty = 0;
    loop {
        let observed = observe_sessions(&mut sessions, deadline);
        if sessions_stably_empty(observed, &mut consecutive_empty) {
            break;
        }
        if Instant::now() >= deadline {
            for session in &sessions {
                session.signal_all(libc::SIGKILL);
            }
            break;
        }
        if observed.is_some() {
            for session in &mut sessions {
                session.signal_new(first_signal);
            }
        } else {
            for session in &mut sessions {
                session.signal_group_once(first_signal);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let results = children.iter_mut().map(Child::wait).collect();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let pending = sessions
            .iter()
            .map(|session| {
                (
                    session.leader,
                    session
                        .members
                        .iter()
                        .copied()
                        .filter(|&pid| pid != session.leader)
                        .collect(),
                )
            })
            .collect();
        let reap_deadline =
            Instant::now() + Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS);
        reap_sessions(
            pending,
            reap_deadline,
            |leader, pid| same_session(pid, leader),
            wait_member,
        );
    }
    results
}

/// Require two complete snapshots without a live session member. Process-table
/// enumeration is not atomic: a member can fork a replacement and exit between
/// entries, making one pass appear empty even though the owned session remains
/// active. An unavailable snapshot is likewise not evidence of quiescence.
fn sessions_stably_empty(observed: Option<bool>, consecutive_empty: &mut u8) -> bool {
    if observed == Some(true) {
        *consecutive_empty = consecutive_empty.saturating_add(1);
    } else {
        *consecutive_empty = 0;
    }
    *consecutive_empty >= 2
}

fn observe_sessions(sessions: &mut [Session], deadline: Instant) -> Option<bool> {
    let processes = process_snapshot(deadline)?;
    let mut finished = true;
    for session in sessions {
        for &(pid, sid, live) in &processes {
            if sid == session.leader {
                session.members.insert(pid);
                finished &= !live;
            }
        }
    }
    Some(finished)
}

/// A retained leader reserves the SID; remembered members keep KILL delivery
/// independent of a degraded or exhausted portable snapshot helper budget.
struct Session {
    leader: u32,
    members: std::collections::BTreeSet<u32>,
    signaled: std::collections::BTreeSet<u32>,
    group_signaled: bool,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
enum WaitState {
    Reaped,
    Running,
    Interrupted,
    NotChild,
    Terminal,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn wait_member(pid: u32) -> WaitState {
    // SAFETY: waitpid receives no status pointer, never blocks, and targets
    // only an exact cached session member.
    let waited = unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
    if waited > 0 {
        WaitState::Reaped
    } else if waited == 0 {
        WaitState::Running
    } else {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => WaitState::Interrupted,
            Some(libc::ECHILD) => WaitState::NotChild,
            _ => WaitState::Terminal,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_sessions(
    mut sessions: Vec<(u32, Vec<u32>)>,
    deadline: Instant,
    mut belongs: impl FnMut(u32, u32) -> bool,
    mut wait: impl FnMut(u32) -> WaitState,
) {
    while !sessions.is_empty() && Instant::now() < deadline {
        for (leader, pending) in &mut sessions {
            let mut next = Vec::new();
            let mut not_children = Vec::new();
            let mut progress = false;
            let mut child_pending = false;
            let mut interrupted = false;
            for pid in pending.drain(..) {
                if !belongs(*leader, pid) {
                    continue;
                }
                match wait(pid) {
                    WaitState::Reaped => progress = true,
                    WaitState::Running => {
                        child_pending = true;
                        next.push(pid);
                    }
                    WaitState::Interrupted => {
                        interrupted = true;
                        next.push(pid);
                    }
                    // A grandchild can become ours only after its intermediate
                    // parent is reaped later in this pass.
                    WaitState::NotChild => not_children.push(pid),
                    WaitState::Terminal => {}
                }
            }
            if progress || child_pending || interrupted {
                next.extend(not_children);
            }
            *pending = next;
        }
        sessions.retain(|(_, pending)| !pending.is_empty());
        if !sessions.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Session {
    fn signal_group_once(&mut self, signal: i32) {
        if !self.group_signaled {
            signal_group(self.leader, signal);
            self.group_signaled = true;
            self.signaled.insert(self.leader);
        }
    }

    fn signal_new(&mut self, signal: i32) {
        let pending: Vec<_> = self.members.difference(&self.signaled).copied().collect();
        for pid in pending {
            if same_session(pid, self.leader) {
                // The initial group delivery already reached every member of
                // the leader's original process group. Signal only members
                // that escaped into another group, exactly once.
                match process_group(pid) {
                    Some(group) if group == self.leader => {
                        self.signaled.insert(pid);
                    }
                    Some(_) => {
                        signal_pid(pid, signal);
                        self.signaled.insert(pid);
                    }
                    None => {}
                }
            }
        }
    }

    fn signal_all(&self, signal: i32) {
        signal_group(self.leader, signal);
        for &pid in &self.members {
            // Duplicate KILL is harmless; revalidate only the session so a
            // member that changed process groups during grace cannot escape.
            if same_session(pid, self.leader) {
                signal_pid(pid, signal);
            }
        }
    }
}

/// TERM grace, then KILL escalation, then reap — mirroring
/// `_dot_cleanup_owned` for owned (non-group) children.
fn terminate_children(children: &mut Vec<Child>) {
    for child in children.iter() {
        terminate(child.id());
    }
    let deadline_grace =
        Instant::now() + Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS);
    loop {
        let mut all_done = true;
        for child in children.iter_mut() {
            // Still running: keep waiting on it. Exited or stale
            // handles need nothing further.
            if let Ok(None) = child.try_wait() {
                all_done = false;
            }
        }
        if all_done || Instant::now() >= deadline_grace {
            break;
        }
        std::thread::sleep(Duration::from_millis(GRACE_INTERVAL_MS));
    }
    for child in children.iter_mut() {
        // SIGKILL for stragglers; already-exited handles report an
        // error here, which is fine (shell: `|| true`).
        let _ = child.kill();
    }
    // Reap every handle so no zombie survives cleanup (shell `wait`).
    // Drain into a temp vec: `Child::wait` needs `&mut`, and the
    // registry drops the handles either way.
    for mut child in children.drain(..) {
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_finish_keeps_first_signal_during_a_late_burst() {
        const HELPER: &str = "DOT_SIGNAL_FINISH_BURST_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::signal_finish_keeps_first_signal_during_a_late_burst",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "signal-finalization helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // A sender can observe the old latch immediately before close and
        // issue one final TERM after restoration. Make that post-boundary
        // delivery harmless; the test is about signals owned by the guard.
        // SAFETY: SIG_IGN is a valid process disposition in this helper.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        let signals = Signals::install().unwrap();
        // SAFETY: this process installed a handler for this valid signal.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        assert_eq!(signals.received(), Some(libc::SIGHUP));

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let sender_barrier = barrier.clone();
        let sender = std::thread::spawn(move || {
            sender_barrier.wait();
            while INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) != SIGNAL_CLOSED {
                // SAFETY: getpid returns this live process and SIGTERM is valid.
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            }
        });
        barrier.wait();
        let code = signals.finish(0);
        sender.join().unwrap();
        assert_eq!(code, 128 + libc::SIGHUP);
    }

    #[test]
    fn signal_guards_serialize_process_wide_ownership() {
        const HELPER: &str = "DOT_SIGNAL_OWNER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::signal_guards_serialize_process_wide_ownership",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "signal-owner helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let first = Signals::install().unwrap();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let (installed_tx, installed_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let guard = Signals::install().unwrap();
            installed_tx.send(()).unwrap();
            drop(guard);
        });
        attempted_rx.recv().unwrap();
        let installed_while_owned = installed_rx
            .recv_timeout(Duration::from_millis(250))
            .is_ok();
        drop(first);
        if !installed_while_owned {
            installed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        second.join().unwrap();
        assert!(
            !installed_while_owned,
            "overlapping guards replaced the process-global signal owner"
        );
    }

    #[test]
    fn process_signal_guard_keeps_handlers_closed_until_exit() {
        const HELPER: &str = "DOT_PROCESS_SIGNAL_OWNER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::process_signal_guard_keeps_handlers_closed_until_exit",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "process-signal helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let signals = Signals::install_with_restore(false).unwrap();
        // SAFETY: the guard owns both valid handled signals.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        assert_eq!(signals.finish(0), 128 + libc::SIGHUP);
        // A second signal in the small return-to-process::exit window must not
        // replace the first status or invoke a restored default disposition.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
    }

    #[test]
    fn snapshot_observes_every_session_when_first_remains_active() {
        let mut children = Vec::new();
        for _ in 0..2 {
            let mut command = Command::new("sleep");
            command.arg("30");
            isolate(&mut command);
            children.push(command.spawn().unwrap());
        }
        let mut sessions: Vec<_> = children
            .iter()
            .map(|child| Session {
                leader: child.id(),
                members: std::collections::BTreeSet::new(),
                signaled: std::collections::BTreeSet::new(),
                group_signaled: false,
            })
            .collect();
        assert_eq!(
            observe_sessions(&mut sessions, Instant::now() + Duration::from_secs(1)),
            Some(false)
        );
        let observed: Vec<_> = sessions
            .iter()
            .map(|session| session.members.contains(&session.leader))
            .collect();
        for child in &mut children {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(observed, [true, true]);
    }

    #[test]
    fn session_shutdown_requires_stable_empty_snapshots() {
        let mut consecutive_empty = 0;

        assert!(!sessions_stably_empty(Some(true), &mut consecutive_empty));
        assert_eq!(consecutive_empty, 1);
        assert!(!sessions_stably_empty(Some(false), &mut consecutive_empty));
        assert_eq!(consecutive_empty, 0);
        assert!(!sessions_stably_empty(Some(true), &mut consecutive_empty));
        assert!(!sessions_stably_empty(None, &mut consecutive_empty));
        assert_eq!(consecutive_empty, 0);
        assert!(!sessions_stably_empty(Some(true), &mut consecutive_empty));
        assert!(sessions_stably_empty(Some(true), &mut consecutive_empty));
    }

    #[test]
    fn snapshot_wave_consumes_one_absolute_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(250);
        for _ in 0..3 {
            let mut command = Command::new("sleep");
            command.arg("30");
            assert!(snapshot(command, deadline).is_none());
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "each snapshot renewed the cancellation budget"
        );
    }

    #[test]
    fn snapshot_collects_successful_process_output() {
        let mut command = Command::new(dot_test_support::bash());
        command.args(["-c", "printf 'snapshot\\n'"]);
        assert_eq!(
            snapshot(command, Instant::now() + Duration::from_secs(1)),
            Some(b"snapshot\n".to_vec())
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_deadline_does_not_wait_for_a_still_running_member() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let leader = session_id(pid).unwrap();
        let started = Instant::now();
        reap_sessions(
            vec![(leader, vec![pid])],
            started + Duration::from_millis(50),
            |leader, pid| same_session(pid, leader),
            wait_member,
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_retries_not_child_after_adoption_progress() {
        let mut calls = Vec::new();
        let mut grandchild_calls = 0;
        reap_sessions(
            vec![(1, vec![10, 20])],
            Instant::now() + Duration::from_secs(1),
            |_, _| true,
            |pid| {
                calls.push(pid);
                if pid == 10 {
                    grandchild_calls += 1;
                    if grandchild_calls == 1 {
                        WaitState::NotChild
                    } else {
                        WaitState::Reaped
                    }
                } else {
                    WaitState::Reaped
                }
            },
        );
        assert_eq!(calls, [10, 20, 10]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_retries_interrupted_wait() {
        let mut calls = 0;
        reap_sessions(
            vec![(1, vec![10])],
            Instant::now() + Duration::from_secs(1),
            |_, _| true,
            |_| {
                calls += 1;
                if calls == 1 {
                    WaitState::Interrupted
                } else {
                    WaitState::Reaped
                }
            },
        );
        assert_eq!(calls, 2);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_pass_visits_later_sessions_when_an_earlier_member_runs() {
        let mut calls = Vec::new();
        reap_sessions(
            vec![(1, vec![10]), (2, vec![20])],
            Instant::now() + Duration::from_millis(30),
            |_, _| true,
            |pid| {
                calls.push(pid);
                if pid == 10 {
                    WaitState::Running
                } else {
                    WaitState::Reaped
                }
            },
        );
        assert!(calls.starts_with(&[10, 20]));
    }

    #[test]
    fn snapshot_deadline_covers_child_that_closes_stdout_early() {
        let root = dot_test_support::TempDir::new("snapshot-deadline").unwrap();
        let marker = root.path().join("pid");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "echo $$ >\"$1\"; exec >/dev/null; exec sleep 30",
                "snapshot",
            ])
            .arg(&marker);
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(snapshot(command, Instant::now() + Duration::from_secs(1)))
                .unwrap();
        });
        let observed = receive.recv_timeout(Duration::from_secs(3));
        if observed.is_err() {
            // The regression must clean the exact helper even on RED.
            let pid = std::fs::read_to_string(&marker)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            signal_pid(pid, libc::SIGKILL);
        }
        worker.join().unwrap();
        assert!(
            observed.is_ok(),
            "snapshot EOF bypassed the bounded child wait"
        );
        assert!(observed.unwrap().is_none());
    }

    #[test]
    fn pid_and_group_rules_match_shell() {
        // `^[1-9][0-9]*$`: positive, no leading zero.
        for good in ["1", "9", "123", "9773"] {
            assert!(valid_pid(good), "{good:?}");
        }
        for bad in ["", "0", "01", "007", "-1", "12a", " 1", "1 "] {
            assert!(!valid_pid(bad), "{bad:?}");
        }
        // Group must be empty or exactly the leader pid.
        assert!(valid_group("123", ""));
        assert!(valid_group("123", "123"));
        assert!(!valid_group("123", "456"));
        assert!(!valid_group("123", "0"));
    }

    #[test]
    fn empty_path_is_usage_error() {
        let mut registry = Registry::new();
        let err = registry
            .register_path(Path::new(""))
            .expect_err("empty path");
        assert!(matches!(err, Error::Usage { .. }), "{err:?}");
        assert_eq!(registry.path_count(), 0);
    }

    #[test]
    fn remove_missing_path_succeeds_like_rm_rf() {
        let mut registry = Registry::new();
        let missing = PathBuf::from("dot-cleanup-definitely-missing-xyz");
        assert!(!missing.exists());
        registry.remove_path(&missing).expect("missing ok");
    }

    #[test]
    fn remove_unregisters_first_then_deletes() {
        let dir = std::env::temp_dir().join(format!("dot-cleanup-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("setup");
        std::fs::write(dir.join("sub").join("f"), b"x").expect("setup");
        let mut registry = Registry::new();
        registry.register_path(&dir).expect("register");
        registry.remove_path(&dir).expect("remove");
        assert!(!dir.exists());
        assert_eq!(registry.path_count(), 0);
    }

    #[test]
    fn symlink_removes_link_not_target() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let base =
                std::env::temp_dir().join(format!("dot-cleanup-link-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("setup");
            let target = base.join("target");
            std::fs::write(&target, b"data").expect("setup");
            let link = base.join("link");
            symlink(&target, &link).expect("symlink");
            let mut registry = Registry::new();
            registry.remove_path(&link).expect("remove link");
            assert!(std::fs::symlink_metadata(&link).is_err());
            assert_eq!(std::fs::read(&target).expect("target survives"), b"data");
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn failed_removal_re_registers() {
        // Make removal fail portably: a file inside a read-only dir.
        // (As root the removal may still succeed; the contract then is
        // the mirror image. Assert the disjunction so the test holds
        // for both.)
        let base = std::env::temp_dir().join(format!("dot-cleanup-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("setup");
        let inner = base.join("inner");
        std::fs::write(&inner, b"x").expect("setup");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        }
        let mut registry = Registry::new();
        registry.register_path(&inner).expect("register");
        let removed = registry.remove_path(&inner).is_ok();
        // Restore permissions so the temp dir always cleans up.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755));
        }
        if removed {
            // Succeeded (e.g. running as root): unregistered, gone.
            assert!(!inner.exists());
            assert_eq!(registry.path_count(), 0);
        } else {
            // Failed: still owned (re-registered), still present.
            assert!(inner.exists());
            assert_eq!(registry.path_count(), 1);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cleanup_is_idempotent_and_drains() {
        let mut registry = Registry::new();
        let dir = std::env::temp_dir().join(format!("dot-cleanup-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("setup");
        registry.register_path(&dir).expect("register");
        registry.cleanup();
        registry.cleanup();
        assert!(!dir.exists());
        assert_eq!(registry.path_count(), 0);
    }

    #[test]
    fn cleanup_terminates_and_reaps_child() {
        let child = Command::new("sleep")
            .arg("300")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut registry = Registry::new();
        registry.track_child(child);
        assert_eq!(registry.child_count(), 1);
        registry.cleanup();
        assert_eq!(registry.child_count(), 0);
        assert!(process_gone(pid), "child {pid} must be reaped");
    }

    /// `kill -0` probe (test-only): true when the pid is gone or
    /// permission-denied-detached; mirrors the shell's reap check.
    #[cfg(unix)]
    fn process_gone(pid: u32) -> bool {
        // Reaped children vanish from the table; a tiny race between
        // wait() and table teardown is impossible (waited == reaped).
        // Confirm via /proc when present, else assume reaped.
        let proc = PathBuf::from(format!("/proc/{pid}"));
        !proc.exists()
    }

    #[cfg(not(unix))]
    fn process_gone(_pid: u32) -> bool {
        true
    }
}
