//! Interactive Git owns a foreground job while its output crosses the relay.
//!
//! A cooperative foreground child alone cannot own a pager's lifetime: Git
//! waits for an interactive pager in its signal handler. An anchored process
//! group lets targeted cancellation reach both, without signalling Dot, its
//! shell, or sibling pipeline members. The leader remains unreaped until the
//! last group operation, including resize notifications, has finished.

use super::*;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Forward one interactive command through a separately owned foreground job.
pub(super) fn run(
    command: Command,
    sink: &mut dyn Write,
    reader: ForwardReader,
    writer: Stdio,
) -> i32 {
    let result = supervise(command, sink, reader, writer);
    if let Err(error) = &result {
        // Terminal handoff and job-control failures otherwise collapse to a
        // generic 127, hiding the OS evidence needed to diagnose the failure.
        eprintln!("dot: foreground Git job failed: {error}");
    }
    session_end_status(result)
}

fn supervise(
    mut command: Command,
    sink: &mut dyn Write,
    reader: ForwardReader,
    writer: Stdio,
) -> std::io::Result<SessionEnd> {
    let mut terminal = Terminal::open()?;
    command.stdout(writer);
    // SAFETY: setpgid is async-signal-safe. This callback precedes the existing
    // registration barrier, so the parent hands over the terminal before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let tty = terminal.file.as_ref().map(File::try_clone).transpose()?;
    let group = terminal.group.clone();
    let parent_group = terminal.parent_group;
    let mut owned = match spawn_owned_child_authorized(command, move |pid| {
        group.store(pid, Ordering::SeqCst);
        if let Some(tty) = tty {
            handoff(tty.as_raw_fd(), parent_group, pid as i32)?;
        }
        Ok(())
    })? {
        OwnedLaunch::Live(child) => child,
        OwnedLaunch::Cancelled(signal) => return Ok(SessionEnd::Interrupted(signal)),
    };
    let mut job = Job {
        child: owned.child.take(),
        _registration: owned.registration.take(),
        lease: owned.lease.take().expect("foreground job lease"),
    };
    let mut output = Forwarder {
        reader,
        sink,
        scratch: Vec::new(),
        failed: false,
        group: job.pid(),
    };
    loop {
        if let Some(signal) = received_signal() {
            let stopped = job.stop(&mut output);
            terminal.finish(true)?;
            return Ok(end_after_cleanup(stopped, SessionEnd::Interrupted(signal)));
        }
        let observed = (|| {
            if exited(job.child())? {
                output.tick(true)?;
                return Ok(true);
            }
            if job.stopped().map_err(|error| {
                std::io::Error::new(error.kind(), format!("observe job stop: {error}"))
            })? {
                output.tick(false)?;
                terminal.suspend().map_err(|error| {
                    std::io::Error::new(error.kind(), format!("suspend terminal: {error}"))
                })?;
                // SIGSTOP also works for orphaned groups and callers that
                // inherited SIGTSTP ignored. Only Dot stops here; its shell
                // and pipeline siblings keep their original job ownership.
                loop {
                    unsafe { libc::raise(libc::SIGSTOP) };
                    if received_signal().is_some() {
                        break;
                    }
                    if terminal.resume().map_err(|error| {
                        std::io::Error::new(error.kind(), format!("resume terminal: {error}"))
                    })? {
                        job.signal(libc::SIGCONT)?;
                        break;
                    }
                    // `bg` did not give the pager its input terminal. Keep
                    // both halves stopped until `fg`, without changing the
                    // current foreground job's terminal attributes.
                }
            }
            output.tick(false)?;
            Ok(false)
        })();
        match observed {
            Ok(true) if received_signal().is_none() => {
                terminal.finish(false)?;
                return job.reap(false).map(SessionEnd::Exited);
            }
            Ok(_) => {}
            Err(error) => {
                let stopped = job.stop(&mut output);
                terminal.finish(true)?;
                return match stopped {
                    Ok(_) => Err(error),
                    Err(cleanup) => Ok(end_after_cleanup(
                        Err(cleanup),
                        SessionEnd::CleanupIncomplete,
                    )),
                };
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Bounded output storage continues draining after its sink has failed.
struct Forwarder<'a> {
    reader: ForwardReader,
    sink: &'a mut dyn Write,
    scratch: Vec<u8>,
    failed: bool,
    group: u32,
}

impl Forwarder<'_> {
    fn tick(&mut self, final_pass: bool) -> std::io::Result<()> {
        let budget = if final_pass {
            COMMAND_CAPTURE_FINAL_BYTES
        } else {
            COMMAND_CAPTURE_TICK_BYTES
        };
        self.reader
            .drain_for_group(&mut self.scratch, budget, Some(self.group))?;
        let delivered = if self.failed || self.scratch.is_empty() {
            Ok(())
        } else {
            self.sink
                .write_all(&self.scratch)
                .and_then(|()| self.sink.flush())
        };
        self.scratch.clear();
        self.failed |= delivered.is_err();
        delivered
    }
}

/// The unreaped leader anchors every process-group operation.
struct Job {
    child: Option<Child>,
    _registration: Option<StatusChildRegistration>,
    lease: SessionLease,
}

impl Job {
    fn child(&self) -> &Child {
        self.child.as_ref().expect("unreaped foreground job")
    }

    fn pid(&self) -> u32 {
        self.child().id()
    }

    fn signal(&self, signal: i32) -> std::io::Result<()> {
        signal_group_result(self.pid(), signal).map(|_| ())
    }

    /// Consume only stop/continue notifications, preserving terminal exit.
    fn stopped(&self) -> std::io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: the registered direct child is retained and siginfo is live.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid(),
                &mut info,
                libc::WSTOPPED | libc::WCONTINUED | libc::WNOHANG,
            )
        };
        if waited == 0 {
            Ok(info.si_code == libc::CLD_STOPPED)
        } else {
            let error = std::io::Error::last_os_error();
            // Exit may race the preceding WEXITED observation. A retained
            // zombie has no matching stop/continue event on Darwin; the next
            // exit poll still owns and observes that same child identity.
            if error.kind() == std::io::ErrorKind::Interrupted
                || waitid_retryable(&error)
                || error.raw_os_error() == Some(libc::ECHILD)
            {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }

    /// Give the entire job graceful termination, then force it within a bound.
    fn stop(&mut self, output: &mut Forwarder<'_>) -> std::io::Result<std::process::ExitStatus> {
        self.signal(libc::SIGTERM)?;
        self.signal(libc::SIGCONT)?;
        for force in [false, true] {
            if force {
                self.signal(libc::SIGKILL)?;
            }
            let deadline = cleanup_deadline();
            loop {
                // Delivery errors cannot prevent cancellation or reintroduce
                // backpressure; Forwarder permanently discards after failure.
                let _ = output.tick(false);
                if exited(self.child())? && self.lease.closed()? {
                    let _ = output.tick(true);
                    return self.reap(true);
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "foreground job retained a live leader or descendant lease after SIGKILL",
        ))
    }

    /// Reap only after the caller has finished every possible group signal.
    fn reap(&mut self, cancelled: bool) -> std::io::Result<std::process::ExitStatus> {
        let child = self.child.as_mut().expect("unreaped foreground job");
        let pid = child.id();
        let status = wait_child_until(child, cleanup_deadline())?;
        self.child.take();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if cancelled {
            // The subreaper may adopt the pager between Git's death and reap.
            // Restrict waits to this killed job; never sweep sibling children.
            drain_retained_group_zombies(pid);
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let _ = (pid, cancelled);
        Ok(status)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.signal(libc::SIGKILL);
            let _ = self.reap(true);
        }
    }
}

/// Terminal state is restored on launch failure, stop, cancellation and unwind.
struct Terminal {
    file: Option<File>,
    initial: libc::termios,
    parent_group: i32,
    group: Arc<AtomicU32>,
    suspended: Option<libc::termios>,
    finished: bool,
}

impl Terminal {
    fn open() -> std::io::Result<Self> {
        // A terminal stdout need not be the controlling tty (e.g. an attached
        // PTY used only for display). The owned group still works without one.
        let raw = unsafe {
            libc::open(
                c"/dev/tty".as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        let file = if raw < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(libc::ENXIO | libc::ENODEV | libc::ENOTTY)
            ) {
                None
            } else {
                return Err(error);
            }
        } else {
            Some(File::from(normalize_internal_fd(raw)?))
        };
        let mut initial = unsafe { std::mem::zeroed() };
        if let Some(file) = &file {
            if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut initial) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let mut terminal = Self {
            file,
            initial,
            parent_group: unsafe { libc::getpgrp() },
            group: Arc::new(AtomicU32::new(0)),
            suspended: None,
            finished: false,
        };
        // The ordered output relay remains in Dot's group while Git owns the
        // foreground. TOSTOP must not suspend that background writer; restore
        // the user's setting after each job, including failure and unwind.
        terminal.allow_output()?;
        Ok(terminal)
    }

    fn allow_output(&mut self) -> std::io::Result<()> {
        if self.initial.c_lflag & libc::TOSTOP != 0 {
            let mut attributes = self.initial;
            attributes.c_lflag &= !libc::TOSTOP;
            self.attributes(&attributes)?;
        }
        Ok(())
    }

    fn attributes(&self, attributes: &libc::termios) -> std::io::Result<()> {
        if let Some(file) = &self.file {
            without_ttou(|| {
                // A continued background job must not put its pager's raw
                // modes onto a terminal now used by the shell or another job.
                if unsafe { libc::tcgetpgrp(file.as_raw_fd()) } != self.parent_group {
                    return Ok(());
                }
                if unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, attributes) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            })?;
        }
        Ok(())
    }

    fn release(&self) -> std::io::Result<()> {
        if let Some(file) = &self.file {
            let group = self.group.load(Ordering::SeqCst) as i32;
            handoff(file.as_raw_fd(), group, self.parent_group)?;
        }
        Ok(())
    }

    fn suspend(&mut self) -> std::io::Result<()> {
        if let Some(file) = &self.file {
            if unsafe { libc::tcgetpgrp(file.as_raw_fd()) }
                != self.group.load(Ordering::SeqCst) as i32
            {
                return Ok(());
            }
            let mut attributes = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut attributes) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            self.suspended = Some(attributes);
        }
        self.release()?;
        self.attributes(&self.initial)?;
        Ok(())
    }

    fn resume(&mut self) -> std::io::Result<bool> {
        if let Some(file) = &self.file {
            if unsafe { libc::tcgetpgrp(file.as_raw_fd()) } != self.parent_group {
                // `bg` may continue Dot without giving it the terminal. Keep
                // the saved pager modes until a later foreground continuation.
                return Ok(false);
            }
            let mut attributes = match self.suspended.take() {
                Some(attributes) => attributes,
                None => {
                    let mut attributes = unsafe { std::mem::zeroed() };
                    if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut attributes) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    attributes
                }
            };
            attributes.c_lflag &= !libc::TOSTOP;
            self.attributes(&attributes)?;
            handoff(
                file.as_raw_fd(),
                self.parent_group,
                self.group.load(Ordering::SeqCst) as i32,
            )?;
        }
        Ok(true)
    }

    fn finish(&mut self, abnormal: bool) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.release()?;
        if abnormal {
            self.attributes(&self.initial)?;
        } else if let Some(file) = &self.file {
            let mut attributes = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(file.as_raw_fd(), &mut attributes) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            attributes.c_lflag =
                (attributes.c_lflag & !libc::TOSTOP) | (self.initial.c_lflag & libc::TOSTOP);
            self.attributes(&attributes)?;
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.finish(true);
    }
}

/// Transfer only from the expected foreground job; never steal another job.
fn handoff(fd: i32, expected: i32, target: i32) -> std::io::Result<()> {
    without_ttou(|| {
        if unsafe { libc::tcgetpgrp(fd) } != expected {
            return Ok(());
        }
        if unsafe { libc::tcsetpgrp(fd, target) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
}

/// Foreground handoffs necessarily run while Dot is a background job.
fn without_ttou(action: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut blocked = unsafe { std::mem::zeroed() };
    let mut previous = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
    }
    let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error));
    }
    let result = action();
    let error =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error));
    }
    result
}
