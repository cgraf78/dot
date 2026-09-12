//! `dot`: fast declarative dotfiles manager.
//!
//! The Rust crate owns the CLI and engine implementation. Shell retained under
//! `lib/dot/public/` is a versioned compatibility boundary for user hooks and
//! executable test suites, not an alternate engine or fallback. Native unit
//! and integration tests own engine regression coverage; retained shell suites
//! validate only actual shell-facing interfaces, packaging, and bootstrap.

#![deny(missing_docs)]

pub mod app;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod constants;
pub mod cron;
pub mod doctor;
pub mod doctor_checks;
pub mod doctor_coordinator;
pub mod doctor_orchestrator;
pub mod doctor_paths;
pub mod doctor_records;
pub mod doctor_runtime;
pub mod errors;
pub mod extension_trust;
pub mod extension_worker;
pub mod families;
pub mod glob;
pub mod hook_api;
pub(crate) mod hook_worker;
pub mod init_client_adopt;
pub mod init_client_candidate;
pub mod init_client_command;
pub mod init_client_delete;
pub mod init_client_engine;
pub mod init_client_entry;
pub mod init_client_generation;
pub mod init_client_git;
pub mod init_client_identity;
pub mod init_client_parent;
pub mod init_client_plan;
pub mod init_client_publish;
pub mod init_client_publish_intent;
pub mod init_client_record;
pub mod init_client_resume;
pub mod init_client_rollback;
pub mod init_client_safe_path;
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
pub mod repos_link_all;
pub mod repos_link_exec;
pub mod repos_link_prep;
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
pub(crate) mod shdeps_provider;
pub mod shdeps_ui;
pub mod shdeps_ui_render;
pub mod startup;
pub mod temp;
pub(crate) mod test_command;
pub(crate) mod test_runner;
pub mod test_suites;
pub mod ui;
pub mod update;
pub mod update_engine;
pub mod update_lock;
pub mod update_run;
pub mod version;
pub mod xdg;

pub use errors::{Error, Result};
