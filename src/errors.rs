//! Infrastructure errors for the `dot` engine.
//!
//! The CLI exit codes own user failures (`0` success, `1` error/unknown
//! command, `2` usage, `75` lock busy). This type is for infrastructure
//! failures that cannot produce their own structured output.

use std::fmt;
use std::io;

/// Shorthand for fallible engine operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Infrastructure failure kinds.
///
/// Scaffolding for slice 2+: no slice-1 path constructs these yet (only
/// the self-tests below touch them). The shape is settled now so the
/// lock/config/git-callers arriving in slices 2-3 share one error type
/// instead of each inventing their own.
#[derive(Debug)]
pub enum Error {
    /// Filesystem or process I/O failure with context.
    Io {
        /// What the engine was trying to do.
        context: &'static str,
        /// The underlying failure.
        source: io::Error,
    },
    /// An external command exited non-zero or could not start.
    Command {
        /// Program name plus arguments for diagnostics.
        command: String,
        /// Exit description when the process ran.
        status: Option<String>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Command { command, status } => match status {
                Some(status) => write!(f, "command failed ({status}): {command}"),
                None => write!(f, "command could not start: {command}"),
            },
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Command { .. } => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io {
            context: "I/O error",
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_displays_context_and_source() {
        let err = Error::Io {
            context: "lock file",
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        };
        assert_eq!(format!("{err}"), "lock file: missing");
    }

    #[test]
    fn command_error_with_and_without_status() {
        let ran = Error::Command {
            command: "git rev-parse".to_string(),
            status: Some("exit status: 128".to_string()),
        };
        assert_eq!(
            format!("{ran}"),
            "command failed (exit status: 128): git rev-parse"
        );
        let missing = Error::Command {
            command: "crontab -l".to_string(),
            status: None,
        };
        assert_eq!(format!("{missing}"), "command could not start: crontab -l");
    }
}
