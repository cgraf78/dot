//! Application runtime and stream boundary.
//!
//! This module snapshots process inputs before command dispatch so callers can
//! run independent Dot contexts without changing the process environment.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Optional executable for an embedded Runtime. Production enters through
/// `main`, whose snapshot matches its process and never reads this override;
/// embedding tests and hosts can name the exact `dot` executable to re-exec.
const RUNTIME_EXECUTABLE: &str = "DOT_RUNTIME_EXECUTABLE";

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
    if !runtime.matches_process() {
        return reexec(runtime, args, streams);
    }
    crate::cli::run_with_runtime(runtime, args, streams.stdout, streams.stderr)
}

impl Runtime {
    /// Whether this Runtime is the direct process entry snapshot.
    fn matches_process(&self) -> bool {
        std::env::current_dir().is_ok_and(|cwd| cwd == self.cwd)
            && std::env::vars_os().collect::<BTreeMap<_, _>>() == self.env
    }
}

/// Execute an embedded Runtime in a child whose real ambient namespace is its
/// immutable snapshot. This lets existing native helpers retain ordinary
/// process semantics while concurrent Runtime callers cannot share PATH,
/// TMPDIR, WSL markers, or child environment inheritance.
fn reexec(runtime: &Runtime, args: &[OsString], streams: &mut Streams<'_>) -> i32 {
    let executable = runtime
        .value(RUNTIME_EXECUTABLE)
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_exe);
    let executable = match executable {
        Ok(path) => path,
        Err(_) => {
            let _ = streams
                .stderr
                .write_all(b"dot: cannot resolve runtime executable\n");
            return 1;
        }
    };
    let display = executable.to_string_lossy().into_owned();
    let output = match Command::new(&executable)
        .args(args)
        .env_clear()
        .envs(runtime.env())
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(_) => {
            let _ = streams.stderr.write_all(
                format!("dot: cannot re-exec runtime executable: {display}\n").as_bytes(),
            );
            return 1;
        }
    };
    let mut failed = streams.stdout.write_all(&output.stdout).is_err();
    failed |= streams.stderr.write_all(&output.stderr).is_err();
    if failed {
        return 1;
    }
    output.status.code().unwrap_or(1)
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
