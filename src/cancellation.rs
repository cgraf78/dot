//! Process-wide cancellation checkpoints for durable operations.
//!
//! The executable installs one signal owner around operational dispatch.
//! Subprocess supervisors observe the same latch while they run; filesystem
//! workflows call [`check`] immediately before publishing durable state so a
//! signal already accepted by the process cannot release additional work.

/// A handled signal that owns the invocation's final status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Interrupted(i32);

impl Interrupted {
    /// Shell-compatible `128 + signal` status.
    pub(crate) fn status(self) -> i32 {
        128 + self.0
    }
}

/// Refuse the next operation after the process signal owner has latched a
/// handled signal.
pub(crate) fn check() -> Result<(), Interrupted> {
    match crate::cleanup::received_signal() {
        Some(signal) => Err(Interrupted(signal)),
        None => Ok(()),
    }
}

/// Refuse a filesystem mutation through the engine's common error channel.
pub(crate) fn check_mutation() -> crate::errors::Result<()> {
    check().map_err(|_| crate::errors::Error::Usage {
        message: "operation interrupted",
    })
}
