//! The user-facing `cron` command from `lib/dot/commands.sh`.
//!
//! The shell branch is `crontab -l 2>/dev/null || printf '  no
//! crontab installed\n'` inside `dot_command_dispatch`, which returns
//! its own `rc` (zero) rather than the branch status. This command
//! therefore always exits 0: an empty listing, a failing `crontab`,
//! and a missing `crontab` binary all succeed, the last two printing
//! [`NO_CRONTAB_MESSAGE`]. `crontab` stderr is nulled (the shell
//! `2>/dev/null`); `crontab` stdout passes through untouched, even
//! the partial bytes from a run that then fails (the shell streams
//! stdout before the `||` fallback runs).
//!
//! The binary travels as a parameter, like
//! [`crate::repos_pull_support::pull_cmd`]'s `program`, so tests can point at
//! fixture scripts; the dispatcher passes `"crontab"`, resolved through
//! `PATH`.

use std::io::Write;
use std::process::{Command, Stdio};

/// Fallback line the `cron` branch prints when `crontab -l` fails or
/// the binary cannot start, including the trailing newline.
///
/// Two leading spaces, exactly like the shell `printf`: the message
/// aligns under the `dot doctor` finding indent the dispatcher
/// shares with the other read-only commands.
pub const NO_CRONTAB_MESSAGE: &str = "  no crontab installed\n";

/// Run the `cron` branch: list the user crontab through `program -l`
/// onto `out`.
///
/// `program` is the `crontab` binary (`"crontab"` from the
/// dispatcher, an absolute fixture path in tests). Stdin is
/// inherited and stderr nulled, matching the shell; stdout streams
/// through even on failure, followed by [`NO_CRONTAB_MESSAGE`].
/// A finished listing returns 0 (listing failures are
/// informational, never errors), as do timeout and spawn failures
/// (with the message); interruption reports `128 + signal`, an
/// unverifiable cleanup reports `125`, and a capture-limit breach
/// reports `1`.
pub fn cron(program: &str, out: &mut dyn Write) -> i32 {
    let mut command = Command::new(program);
    command
        .arg("-l")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match crate::cleanup::run_session_output_typed(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Strict,
    ) {
        Ok(output) => {
            let _ = out.write_all(&output.stdout);
            if !output.status.success() {
                let _ = out.write_all(NO_CRONTAB_MESSAGE.as_bytes());
            }
            0
        }
        Err(crate::cleanup::SessionOutputError::Interrupted(signal)) => 128 + signal,
        Err(crate::cleanup::SessionOutputError::CleanupIncomplete) => {
            crate::cleanup::CLEANUP_INCOMPLETE_STATUS
        }
        Err(crate::cleanup::SessionOutputError::CaptureLimit) => 1,
        Err(_) => {
            let _ = out.write_all(NO_CRONTAB_MESSAGE.as_bytes());
            0
        }
    }
}
