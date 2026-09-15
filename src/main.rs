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
//!
//! Informational commands (`help`, `version`, and their aliases) bypass the
//! output relay: their output is a few static bytes that cannot backpressure,
//! while the relay's helper processes cost tens of milliseconds per
//! invocation. Dispatch, the re-exec guard, and the exit-status contract are
//! shared with the relay path.

use std::collections::BTreeMap;
use std::io::Write;

fn main() {
    dot::startup::apply_umask_ceiling();
    let env = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let mut process_args = std::env::args_os();
    let argv0 = process_args.next();
    let args = process_args.collect::<Vec<_>>();
    let command = args
        .first()
        .map(|arg| arg.as_encoded_bytes())
        .unwrap_or_default();
    if dot::startup::informational_command(command) {
        informational_main(&env, &cwd, argv0.as_deref(), &args);
    }
    // Snapshot stdout/stderr before any runtime open can reuse an inherited
    // closed descriptor. External writes happen only in the retained relay
    // process, so a blocked sink cannot hold cancellation or final teardown.
    let output_relay = match dot::cleanup::ProcessOutputRelay::start() {
        Ok(relay) => relay,
        Err(error) => {
            // The relay never started, so raw process stderr is the
            // only diagnostic channel — never exit silently here.
            let _ = writeln!(std::io::stderr(), "dot: output relay failed: {error}");
            std::process::exit(1);
        }
    };
    let mut out = output_relay.stdout();
    let mut err = output_relay.stderr();
    let runtime = match dot::app::Runtime::from_process_args(&env, &cwd, argv0.as_deref(), &args) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "dot: startup: {error}");
            let _ = err.flush();
            drop(out);
            drop(err);
            let finish = output_relay.finish(true);
            std::process::exit(dot::cleanup::process_output_status(1, finish));
        }
    };
    let mut streams = dot::app::Streams::new(&mut out, &mut err);
    let code = dot::app::run_direct(&runtime, &args, &mut streams);
    // Keep the explicit flush contract if a future descriptor adapter adds
    // buffering; an undelivered tail remains an error rather than success.
    let flushed = out.flush().is_ok() && err.flush().is_ok();
    drop(out);
    drop(err);
    // A logical write to one descriptor can fail because that descriptor was
    // closed at exec while the other stream still has valid queued output on
    // the shared relay. Drain whenever teardown itself was not interrupted;
    // `flushed` still controls the status, not whether healthy bytes survive.
    let relay_finished = output_relay.finish(
        !dot::cleanup::outward_write_interrupted() && !dot::cleanup::outward_write_aborted(),
    );
    let code = if code == 0 && !flushed { 1 } else { code };
    let code = dot::cleanup::process_output_status(code, relay_finished);
    dot::cleanup::exit_process(code);
}

/// A `Write` sink for an entry-closed descriptor: writes fail with the relay
/// path's closed-at-entry error so undelivered output surfaces as the
/// command's error status on both paths. Flushing an unwritten sink
/// succeeds, matching the relay (whose flush only fails after a failed
/// write, a failed channel, or cancellation — none reachable here with a
/// success code).
struct ClosedStdio;

impl Write for ClosedStdio {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "process output descriptor was closed at entry",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run an informational command on direct stdio without the output relay.
/// Dispatch and the re-exec guard are shared with the relay path, so output
/// bytes and exit codes match; only the transport differs. Descriptors
/// closed at exec fail instead of writing into a reused descriptor number.
fn informational_main(
    env: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    cwd: &std::path::Path,
    argv0: Option<&std::ffi::OsStr>,
    args: &[std::ffi::OsString],
) -> ! {
    let stdout_open = dot::cleanup::entry_stdio_open(libc::STDOUT_FILENO);
    let stderr_open = dot::cleanup::entry_stdio_open(libc::STDERR_FILENO);
    let runtime = match dot::app::Runtime::from_process_args(env, cwd, argv0, args) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "dot: startup: {error}");
            dot::cleanup::exit_process(1);
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let mut closed_out = ClosedStdio;
    let mut closed_err = ClosedStdio;
    let out: &mut dyn Write = if stdout_open {
        &mut out
    } else {
        &mut closed_out
    };
    let err: &mut dyn Write = if stderr_open {
        &mut err
    } else {
        &mut closed_err
    };
    let mut streams = dot::app::Streams::new(&mut *out, &mut *err);
    let code = dot::app::run_direct(&runtime, args, &mut streams);
    let flushed = out.flush().is_ok() && err.flush().is_ok();
    let code = if code == 0 && !flushed { 1 } else { code };
    dot::cleanup::exit_process(code);
}
