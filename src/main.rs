//! `dot` binary entry point: thin adapter over the library crate.
//!
//! All behavior lives in `dot::cli` so integration tests exercise the
//! same code path as the installed binary — a bug fixed in the library
//! is fixed for every caller, and a behavior tested in-process holds on
//! the command line. The adapter owns only three things: skipping
//! `argv[0]`, locking stdout/stderr once (one lock acquisition instead
//! of per-write locking on every output call), and translating the
//! returned code into the process exit status. Write failures inside
//! `run` are ignored (`let _ =`) rather than panicking: a closed pipe
//! must surface as the command's normal exit path, never as a Rust
//! panic message, since panics would break the stderr byte contract.
//! This adapter itself performs no fallible setup, so it cannot fail
//! before `run` takes over.

use std::io::{Write, stderr, stdout};

fn main() {
    let mut out = stdout().lock();
    let mut err = stderr().lock();
    let code = dot::cli::run(std::env::args_os().skip(1), &mut out, &mut err);
    // `process::exit` runs no destructors and flushes nothing; `StdoutLock`
    // is line-buffered, so a future write without a trailing newline would
    // be silently truncated without this. A flush failure here means the
    // output did not land, which is itself a failure to report.
    let flushed = out.flush().is_ok() && err.flush().is_ok();
    std::process::exit(if flushed { code } else { 1 });
}
