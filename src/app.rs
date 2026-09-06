//! Application runtime and stream boundary.
//!
//! This module snapshots process inputs before command dispatch so callers can
//! run independent Dot contexts without changing the process environment.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Immutable process inputs for one Dot invocation.
#[derive(Debug, Clone)]
pub struct Runtime {
    home: PathBuf,
    state_home: PathBuf,
    config_home: PathBuf,
    cwd: PathBuf,
    source_root: PathBuf,
    env: BTreeMap<OsString, OsString>,
}

impl Runtime {
    /// Snapshot `env` and `cwd` for an independent Dot invocation.
    ///
    /// The working directory must be absolute so every derived path remains
    /// stable if another caller later changes its own process context.
    pub fn from_env(env: &BTreeMap<OsString, OsString>, cwd: &Path) -> std::io::Result<Self> {
        if !cwd.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "dot runtime working directory must be absolute",
            ));
        }
        let home = value(env, "HOME").map(PathBuf::from).unwrap_or_default();
        let state_home = xdg_home(env, "XDG_STATE_HOME", &home, ".local/state");
        let config_home = xdg_home(env, "XDG_CONFIG_HOME", &home, ".config");
        let source_root = crate::startup::resolve_source_root(
            &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/nonexistent-dot-exe")),
            value(env, "DOT_SOURCE_ROOT"),
            cwd,
        );
        Ok(Self {
            home,
            state_home,
            config_home,
            cwd: cwd.to_path_buf(),
            source_root,
            env: env.clone(),
        })
    }

    /// Return the snapshotted home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Return the snapshotted XDG state directory.
    pub fn state_home(&self) -> &Path {
        &self.state_home
    }

    /// Return the snapshotted XDG configuration directory.
    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    /// Return the snapshotted working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub(crate) fn env(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }

    pub(crate) fn value(&self, key: &str) -> Option<&OsStr> {
        value(&self.env, key)
    }
}

/// Borrowed stdout and stderr for one Dot invocation.
pub struct Streams<'a> {
    pub(crate) stdout: &'a mut dyn Write,
    pub(crate) stderr: &'a mut dyn Write,
}

impl<'a> Streams<'a> {
    /// Bind stdout and stderr for one Dot invocation.
    pub fn new(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write) -> Self {
        Self { stdout, stderr }
    }
}

/// Dispatch one command using only the supplied runtime and streams.
pub fn run(runtime: &Runtime, args: &[OsString], streams: &mut Streams<'_>) -> i32 {
    crate::cli::run_with_runtime(runtime, args, streams.stdout, streams.stderr)
}

fn value<'a>(env: &'a BTreeMap<OsString, OsString>, key: &str) -> Option<&'a OsStr> {
    env.get(OsStr::new(key))
        .filter(|value| !value.is_empty())
        .map(OsString::as_os_str)
}

fn xdg_home(env: &BTreeMap<OsString, OsString>, key: &str, home: &Path, fallback: &str) -> PathBuf {
    value(env, key)
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(fallback))
}
