//! Process-replacement handoff from a running `dot update` to the Dot binary
//! its Tools stage just installed.
//!
//! When Shdeps upgrades a packaged Dot release in the middle of an update,
//! the rest of that run must not mix the running (old) engine with the new
//! release's hook runtime and library files. The engine records a
//! [`Handoff`] instead of continuing in place, the update command parks its
//! lock in it, and the binary entry point replaces the process with the new
//! binary through [`Handoff::run`] once its output relay has drained.
//!
//! `execve` keeps the PID and the process start time, which is exactly the
//! identity the update lock's re-entry check binds: the continuation finds
//! `DOT_UPDATE_LOCK_TOKEN` in its environment and re-enters the lock this
//! process already holds, so no other update can slip in between the two
//! halves of the run. Every path that does not exec drops the parked lock,
//! whose guard releases it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;

use crate::update_lock::LockGuard;

/// Remove the handoff variables from this process's environment, which every
/// child spawned without an explicit environment inherits.
///
/// The binary entry calls this right after snapshotting the environment into
/// its runtime (which keeps them for the command boundary and the engine) and
/// before any thread exists. A release continuation receives the variables
/// through `execve`, so without this a `git pull` and the helpers it starts
/// (credential helpers, SSH control masters) would carry them.
#[doc(hidden)]
pub fn scrub_process_env() {
    for key in crate::update_engine::CONTINUATION_ENV {
        if std::env::var_os(key).is_some() {
            // SAFETY: called once at process entry, before any thread is
            // spawned, so no other thread can read the environment.
            unsafe { std::env::remove_var(key) };
        }
    }
}

/// A pending replacement of this process with an upgraded Dot binary.
#[derive(Debug)]
pub struct Handoff {
    program: PathBuf,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    /// Cron state home when the outer run is a `--cron` run. The outer run
    /// skips its own outcome line because the continuation records the run;
    /// a handoff that never execs records the `fail` here instead.
    cron_state: Option<PathBuf>,
    lock: Option<LockGuard>,
}

impl Handoff {
    /// Describe the continuation: `program` with `args` (`argv[1..]`) under
    /// exactly `env`.
    pub(crate) fn new(
        program: PathBuf,
        args: Vec<OsString>,
        env: BTreeMap<OsString, OsString>,
        cron_state: Option<PathBuf>,
    ) -> Self {
        Self {
            program,
            args,
            env,
            cron_state,
            lock: None,
        }
    }

    /// Keep the update lock held across the exec instead of releasing it
    /// when the command returns.
    pub(crate) fn hold(&mut self, guard: LockGuard) {
        self.lock = Some(guard);
    }

    /// Replace this process with the continuation. `status` is the outer
    /// half's final status: the engine ended that half with 0, so anything
    /// else came from the process boundary. A cancellation or an unproven
    /// cleanup stops here with that status. An output delivery failure (1)
    /// still execs: the run converges in the new binary, which reports its
    /// own delivery, just as an in-place run kept converging after one.
    /// Returns only when no exec happened (`status`, or 1 when the exec
    /// failed); the parked lock is then released and a cron run records its
    /// `fail`.
    #[doc(hidden)]
    pub fn run(self, status: i32) -> i32 {
        if crate::cleanup::received_signal().is_some()
            || status == crate::cleanup::CLEANUP_INCOMPLETE_STATUS
        {
            self.abandon();
            return status;
        }
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args).env_clear().envs(&self.env);
        let error = crate::cleanup::exec_process(&mut command);
        let program = self.program.clone();
        // Record and release before the diagnostic: the write below is the
        // one that can fail.
        self.abandon();
        // A latched signal is a cancellation, not a failed handoff: the
        // entry point's exit maps it to `128 + signal`.
        if crate::cleanup::received_signal().is_none()
            && !crate::cleanup::cleanup_incomplete()
            && crate::cleanup::entry_stdio_open(libc::STDERR_FILENO)
        {
            // The output relay has already finished, so this is the one
            // write that goes straight to the entry stderr descriptor.
            let _ = writeln!(
                std::io::stderr(),
                "dot: cannot start the updated dot at {}: {error}; rerun `dot update` to finish",
                program.display()
            );
        }
        1
    }

    /// Give up the handoff: a cron run that was not cancelled records the
    /// failed run, and dropping the parked guard releases the lock.
    fn abandon(self) {
        if crate::cleanup::received_signal().is_some() {
            return;
        }
        if let Some(state) = &self.cron_state {
            crate::update_status::append_outcome(
                state,
                crate::update_engine::now_secs(),
                "fail",
                "update",
                "",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use dot_test_support::TempDir;

    /// Build a handoff holding the update lock under `state`. Callers hold
    /// the signal owner: `run` reads the process-wide signal latch, which
    /// signal-raising unit tests set while they own it.
    fn parked(state: &std::path::Path, cron: bool) -> Handoff {
        let log = crate::log::Log::new(false, false);
        let guard = crate::update_lock::acquire(state, false, &log, None, &mut Vec::new())
            .expect("update lock");
        let mut handoff = Handoff::new(
            PathBuf::from("/nonexistent/dot"),
            vec![OsString::from("update")],
            BTreeMap::new(),
            cron.then(|| state.to_path_buf()),
        );
        handoff.hold(guard);
        handoff
    }

    #[test]
    fn unproven_cleanup_abandons_without_exec_and_releases_the_lock() {
        // 125 is the one boundary status that must not exec (helpers may
        // still run); the program path would fail loudly if it were tried.
        let _signals = crate::cleanup::hold_signal_ownership_for_test();
        let state = TempDir::new("handoff-abandon").expect("state");
        let handoff = parked(state.path(), true);
        assert!(crate::update_lock::lock_path(state.path()).exists());
        let status = handoff.run(crate::cleanup::CLEANUP_INCOMPLETE_STATUS);
        assert_eq!(status, crate::cleanup::CLEANUP_INCOMPLETE_STATUS);
        assert!(!crate::update_lock::lock_path(state.path()).exists());
        let log = std::fs::read_to_string(crate::update_status::update_log_path(state.path()))
            .expect("cron outcome");
        let fields: Vec<&str> = log.trim_end().split(' ').skip(1).collect();
        assert_eq!(fields, ["fail", "update"]);
    }

    #[test]
    fn abandoning_a_plain_run_writes_no_cron_outcome() {
        let _signals = crate::cleanup::hold_signal_ownership_for_test();
        let state = TempDir::new("handoff-abandon-plain").expect("state");
        let handoff = parked(state.path(), false);
        assert_eq!(handoff.run(crate::cleanup::CLEANUP_INCOMPLETE_STATUS), 125);
        assert!(!crate::update_lock::lock_path(state.path()).exists());
        assert!(!crate::update_status::update_log_path(state.path()).exists());
    }
}
