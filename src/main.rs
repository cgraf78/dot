//! `dot` binary entry point: thin adapter over the library crate.
//!
//! All behavior lives in `dot::cli` so integration tests exercise the
//! same code path as the installed binary — a bug fixed in the library
//! is fixed for every caller, and a behavior tested in-process holds on
//! the command line. The adapter owns only four things: skipping
//! `argv[0]`, binding `DOT_SOURCE_ROOT` when the caller left it unset
//! (slice 84; the shell `main.sh` derives it from its own path — see
//! `dot::startup` for the full entry-contract map), locking
//! stdout/stderr once (one lock acquisition instead of per-write
//! locking on every output call), and translating the returned code
//! into the process exit status. Write failures inside `run` are
//! ignored (`let _ =`) rather than panicking: a closed pipe must
//! surface as the command's normal exit path, never as a Rust panic
//! message, since panics would break the stderr byte contract.
//! This adapter itself performs no fallible setup, so it cannot fail
//! before `run` takes over.

use std::io::{Write, stderr, stdout};

fn main() {
    // Slice 84: the shell always exports `DOT_SOURCE_ROOT` from its
    // own path; the binary reproduces the export only when the caller
    // left it unset or empty, so an explicit hook (tests, embeddings)
    // survives. Process environment mutation is `unsafe` in edition
    // 2024; `main` is the single-flight process entry (like the
    // shell's own export), so no other thread observes the change.
    let missing = match std::env::var_os("DOT_SOURCE_ROOT") {
        None => true,
        Some(value) => value.is_empty(),
    };
    if missing {
        let root = dot::startup::ambient_source_root();
        unsafe {
            std::env::set_var("DOT_SOURCE_ROOT", &root);
        }
    }
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
