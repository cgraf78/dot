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
pub mod constants;
pub mod cron;
pub mod doctor_coordinator;
pub mod doctor_paths;
pub mod doctor_records;
pub mod doctor_runtime;
pub mod errors;
pub mod extension_trust;
pub mod extension_worker;
pub mod families;
pub mod glob;
pub mod hook_api;
pub mod init_client_candidate;
pub mod init_client_delete;
pub mod init_client_entry;
pub mod init_client_generation;
pub mod init_client_identity;
pub mod init_client_plan;
pub mod init_client_record;
pub mod init_client_records;
pub mod init_client_transaction;
pub mod log;
pub mod merge_block;
pub mod merge_hooks;
pub mod merges;
pub mod overlay_context;
pub mod overlays;
pub mod platform;
pub mod pre_sync;
pub mod profile_lifecycle;
pub mod profiles;
pub mod progress_ui;
pub mod repos_base;
pub mod repos_commands;
pub mod repos_config;
pub mod repos_dirty;
pub mod repos_git;
pub mod repos_overlays;
pub mod repos_pull;
pub mod repos_pull_backup;
pub mod repos_pull_clone;
pub mod repos_pull_fleet;
pub mod repos_pull_normalize;
pub mod repos_pull_overlay;
pub mod repos_pull_queries;
pub mod repos_pull_support;
pub mod reserved;
pub mod run;
pub mod shdeps;
pub mod shdeps_env_abi;
pub mod shdeps_ui;
pub mod shdeps_ui_render;
pub mod temp;
pub mod test_suites;
pub mod test_support;
pub mod ui;
pub mod update;
pub mod update_lock;
pub mod version;
pub mod xdg;

pub use errors::{Error, Result};
