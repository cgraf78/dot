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
//! `dot::startup` for the full entry-contract map), locking
//! stdout/stderr once (one lock acquisition instead of per-write
//! locking on every output call), and translating the returned code
//! into the process exit status. Write failures inside `run` are
//! ignored (`let _ =`) rather than panicking: a closed pipe must
//! surface as the command's normal exit path, never as a Rust panic
//! message, since panics would break the stderr byte contract.
//! Source-root discovery can fail before `run` takes over; that path emits one
//! stable startup diagnostic instead of trusting ambient executable code.

use std::collections::BTreeMap;
use std::io::{Write, stderr, stdout};

fn main() {
    dot::startup::apply_umask_ceiling();
    let env = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let mut process_args = std::env::args_os();
    let argv0 = process_args.next();
    let args = process_args.collect::<Vec<_>>();
    let mut out = stdout().lock();
    let mut err = stderr().lock();
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
    // `process::exit` runs no destructors and flushes nothing; `StdoutLock`
    // is line-buffered, so a future write without a trailing newline would
    // be silently truncated without this. A flush failure here means the
    // output did not land, which is itself a failure to report.
    let flushed = out.flush().is_ok() && err.flush().is_ok();
    std::process::exit(if flushed { code } else { 1 });
}
