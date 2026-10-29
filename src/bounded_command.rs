//! Versioned process supervision independent of Dot configuration and startup.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cleanup::{self, SessionEnd};

/// Dispatch the public low-level boundary before normal application startup.
///
/// Returns `None` for other Dot commands. Invalid arguments and unavailable
/// supervision return 125 without synthesizing child stdout or stderr.
pub fn dispatch(arguments: &[OsString]) -> Option<i32> {
    if arguments.first().map(OsString::as_os_str) != Some(OsStr::new("run-bounded-v1")) {
        return None;
    }
    Some(run(&arguments[1..]))
}

/// Parse the deliberately small decimal grammar without accepting float syntax
/// such as NaN, exponents, signs, unit suffixes, or a missing integer/fraction.
fn duration(value: &OsStr) -> Result<Option<Duration>, ()> {
    let value = value.to_str().ok_or(())?;
    let mut fields = value.split('.');
    let integer = fields.next().ok_or(())?;
    if integer.is_empty() || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    if let Some(fraction) = fields.next() {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(());
        }
    }
    if fields.next().is_some() {
        return Err(());
    }
    let seconds = value.parse::<f64>().map_err(|_| ())?;
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| ())?;
    // Only a textually zero deadline disables supervision. Float conversion
    // can round positive subnanosecond values (or underflow) to zero; those
    // remain finite deadlines at the smallest representable resolution.
    if value.bytes().all(|byte| byte == b'0' || byte == b'.') {
        Ok(None)
    } else {
        Ok(Some(duration.max(Duration::from_nanos(1))))
    }
}

fn run(arguments: &[OsString]) -> i32 {
    let (Some(raw_duration), Some(program)) = (arguments.first(), arguments.get(1)) else {
        return 125;
    };
    let limit = match duration(raw_duration) {
        Ok(limit) => limit,
        Err(()) => return 125,
    };
    let deadline = match limit {
        Some(limit) => match Instant::now().checked_add(limit) {
            Some(deadline) => Some(deadline),
            None => return 125,
        },
        None => None,
    };
    let signals = match cleanup::Signals::for_process_entry() {
        Ok(signals) => signals,
        Err(_) => return 125,
    };
    let mut command = Command::new(program);
    command
        .args(&arguments[2..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let stdio = std::array::from_fn::<_, 3, _>(|index| cleanup::entry_stdio_open(index as i32));
    // Rust may reserve an entry-closed standard descriptor on /dev/null before
    // main. Restore absence only in the child; internal supervisor descriptors
    // are already allocated above stdio and arbitrary caller FDs stay intact.
    // SAFETY: close is async-signal-safe and the callback owns only child stdio.
    unsafe {
        command.pre_exec(move || {
            for (descriptor, open) in stdio.iter().enumerate() {
                if !open {
                    libc::close(descriptor as i32);
                }
            }
            Ok(())
        });
    }
    let status = match cleanup::supervise_session(command, deadline, |_| Ok(())) {
        Ok(SessionEnd::Exited(status)) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
        Ok(SessionEnd::Interrupted(signal)) => 128 + signal,
        Ok(SessionEnd::TimedOut) => 124,
        Ok(SessionEnd::CleanupIncomplete) => 125,
        Err(error) => cleanup::target_exec_status(&error).unwrap_or(125),
    };
    signals.finish(status)
}
