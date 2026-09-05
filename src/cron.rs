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
//! [`crate::repos_pull_support::pull_cmd`]'s `program`, so
//! differential tests can point at fixture scripts; the dispatcher
//! passes `"crontab"`, resolved through `PATH` exactly like the
//! shell. Dispatch wiring (`cli`) is a later slice: this module owns
//! only the branch behavior.

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
/// Returns 0 on every path, like the dispatcher: listing failures
/// are informational, never errors.
pub fn cron(program: &str, out: &mut dyn Write) -> i32 {
    match Command::new(program)
        .arg("-l")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => {
            let _ = out.write_all(&output.stdout);
            if !output.status.success() {
                let _ = out.write_all(NO_CRONTAB_MESSAGE.as_bytes());
            }
            0
        }
        Err(_) => {
            let _ = out.write_all(NO_CRONTAB_MESSAGE.as_bytes());
            0
        }
    }
}
