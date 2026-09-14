//! `dot` binary entry point: thin adapter over the library crate.
//!
//! All behavior lives in `dot::cli` so integration tests exercise the
//! same code path as the installed binary — a bug fixed in the library
//! is fixed for every caller, and a behavior tested in-process holds on
//! the command line. The adapter owns only five things: applying the inherited
//! permission ceiling, preserving `argv[0]` for executable-identity validation
//! while excluding it from command dispatch, snapshotting the ambient runtime
//! (including the resolved source root; the shell `main.sh` derives it from its
//! own path — see
//! `dot::startup` for the full entry-contract map), exposing stdout/stderr as
//! unbuffered descriptors so handled signals interrupt backpressured writes,
//! and translating the returned code into the process exit status. Write failures inside `run` are
//! ignored (`let _ =`) rather than panicking: a closed pipe must
//! surface as the command's normal exit path, never as a Rust panic
//! message, since panics would break the stderr byte contract.
//! Source-root discovery can fail before `run` takes over; that path emits one
//! stable startup diagnostic instead of trusting ambient executable code.

use std::collections::BTreeMap;
use std::io::Write;

/// Unbuffered process descriptor output. Keeping each write at the syscall
/// boundary lets the library's signal-aware adapter observe EINTR instead of
/// having `StdoutLock`'s line buffer retry a blocked write internally.
struct ProcessWriter(libc::c_int);

impl Write for ProcessWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // SAFETY: stdout/stderr remain process-owned for the program lifetime;
        // the byte slice is valid for this synchronous syscall.
        let written = unsafe { libc::write(self.0, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn main() {
    dot::startup::apply_umask_ceiling();
    let env = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let mut process_args = std::env::args_os();
    let argv0 = process_args.next();
    let args = process_args.collect::<Vec<_>>();
    // Resolve the standard descriptors once; `ProcessWriter` deliberately
    // bypasses Rust's retrying line buffer so handled signals can cancel a
    // write whose consumer has stopped reading.
    let mut out = ProcessWriter(libc::STDOUT_FILENO);
    let mut err = ProcessWriter(libc::STDERR_FILENO);
    let runtime = match dot::app::Runtime::from_process_args(&env, &cwd, argv0.as_deref(), &args) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "dot: startup: {error}");
            let _ = err.flush();
            std::process::exit(1);
        }
    };
    let mut streams = dot::app::Streams::new(&mut out, &mut err);
    let code = dot::app::run_direct(&runtime, &args, &mut streams);
    // Keep the explicit flush contract if a future descriptor adapter adds
    // buffering; an undelivered tail remains an error rather than success.
    let flushed = out.flush().is_ok() && err.flush().is_ok();
    std::process::exit(if flushed { code } else { 1 });
}
