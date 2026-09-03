//! `dot`: fast declarative dotfiles manager.
//!
//! The Rust crate owns the implementation. The shell tree under `lib/`
//! remains the behavior owner until each slice cuts over; `tests/*-test`
//! (run via `bash tests/run`) is the parity oracle and must stay green.
//! Public shell API boundaries (`lib/dot/public/*`, `hook-api-v1.tsv`,
//! `doctor-api-v1.tsv`, `test-api-v1.tsv`) are compatibility constraints,
//! not implementation details to mirror.

#![deny(missing_docs)]

pub mod cleanup;
pub mod cli;
pub mod config;
pub mod errors;
pub mod log;
pub mod test_support;
pub mod ui;
pub mod version;
pub mod xdg;

pub use errors::{Error, Result};
