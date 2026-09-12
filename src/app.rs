//! Application runtime and stream boundary.
//!
//! This module snapshots process inputs before command dispatch so callers can
//! run independent Dot contexts without changing the process environment.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal as _, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An absolute `dot` executable explicitly authorized for an embedded runtime.
///
/// [`run`] never resolves an executable from the host process. An embedding
/// caller must opt in to re-execution by constructing this capability and
/// attaching it with [`Runtime::with_executable`].
#[derive(Debug, Clone)]
pub struct RuntimeExecutable(PathBuf);

impl RuntimeExecutable {
    /// Validate an absolute executable path for embedded execution.
    pub fn new(path: PathBuf) -> std::io::Result<Self> {
        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "dot runtime executable must be an absolute path",
            ));
        }
        Ok(Self(path))
    }
}

/// Immutable process inputs for one Dot invocation.
#[derive(Debug, Clone)]
pub struct Runtime {
    home: PathBuf,
    state_home: PathBuf,
    config_home: PathBuf,
    cwd: PathBuf,
    source_root: PathBuf,
    env: BTreeMap<OsString, OsString>,
    executable: Option<RuntimeExecutable>,
}

impl Runtime {
    /// Snapshot trusted `env` and `cwd` for an embedded Dot invocation.
    ///
    /// The working directory must be absolute so every derived path remains
    /// stable if another caller later changes its own process context. An
    /// explicit `DOT_SOURCE_ROOT` is an embedding capability here; the shipped
    /// binary uses [`Self::from_process_args`] so ambient input cannot select
    /// executable hook code.
    pub fn from_env(env: &BTreeMap<OsString, OsString>, cwd: &Path) -> std::io::Result<Self> {
        validate_cwd(cwd)?;
        let source_root = crate::startup::resolve_source_root(
            &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/nonexistent-dot-exe")),
            value(env, "DOT_SOURCE_ROOT"),
            cwd,
        );
        Ok(Self::snapshot(env.clone(), cwd, source_root))
    }

    /// Snapshot the process environment while binding executable code to this
    /// binary's own checkout or release.
    ///
    /// Unlike [`Self::from_env`], this production entry point ignores ambient
    /// `DOT_SOURCE_ROOT` and republishes the derived root to descendants.
    #[doc(hidden)]
    pub fn from_process_env(
        env: &BTreeMap<OsString, OsString>,
        cwd: &Path,
    ) -> std::io::Result<Self> {
        validate_cwd(cwd)?;
        let argv0 = std::env::args_os().next();
        let executable = crate::startup::process_executable(
            argv0.as_deref(),
            value(env, crate::startup::TERMUX_EXECUTABLE_ENV),
        )?;
        let source_root = crate::startup::process_source_root(&executable)?;
        Ok(Self::process_snapshot(env, cwd, source_root))
    }

    /// Snapshot a process entry, using application `argv[0]` to corroborate
    /// executable identity under Termux and retaining standalone `help` and
    /// `version` support when a packaged release's hook assets have been
    /// removed.
    #[doc(hidden)]
    pub fn from_process_args(
        env: &BTreeMap<OsString, OsString>,
        cwd: &Path,
        argv0: Option<&OsStr>,
        args: &[OsString],
    ) -> std::io::Result<Self> {
        validate_cwd(cwd)?;
        let executable = crate::startup::process_executable(
            argv0,
            value(env, crate::startup::TERMUX_EXECUTABLE_ENV),
        )?;
        let command = args
            .first()
            .map(|arg| arg.as_os_str().as_encoded_bytes())
            .unwrap_or_default();
        let source_root = match crate::startup::process_source_root(&executable) {
            Ok(root) => root,
            Err(error) if crate::startup::informational_command(command) => {
                crate::startup::process_owner_root(&executable).map_err(|_| error)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self::process_snapshot(env, cwd, source_root))
    }

    fn process_snapshot(
        env: &BTreeMap<OsString, OsString>,
        cwd: &Path,
        source_root: PathBuf,
    ) -> Self {
        let mut env = env.clone();
        env.insert(
            OsString::from("DOT_SOURCE_ROOT"),
            source_root.as_os_str().to_os_string(),
        );
        Self::snapshot(env, cwd, source_root)
    }

    fn snapshot(env: BTreeMap<OsString, OsString>, cwd: &Path, source_root: PathBuf) -> Self {
        let home = value(&env, "HOME").map(PathBuf::from).unwrap_or_default();
        let state_home = xdg_home(&env, "XDG_STATE_HOME", &home, ".local/state");
        let config_home = xdg_home(&env, "XDG_CONFIG_HOME", &home, ".config");
        Self {
            home,
            state_home,
            config_home,
            cwd: cwd.to_path_buf(),
            source_root,
            env,
            executable: None,
        }
    }

    /// Attach the explicit executable capability required by [`run`].
    pub fn with_executable(mut self, executable: RuntimeExecutable) -> Self {
        self.executable = Some(executable);
        self
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

    pub(crate) fn find_on_path(&self, name: &str) -> Option<PathBuf> {
        let path = self.value("PATH")?;
        std::env::split_paths(path)
            .map(|directory| {
                if directory.is_absolute() {
                    directory.join(name)
                } else {
                    self.cwd().join(directory).join(name)
                }
            })
            .find(|path| {
                std::fs::metadata(path)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    }

    /// Resolve the Bash interpreter authorized by this invocation.
    ///
    /// An explicit `BASH` must already be absolute and executable. When it is
    /// absent, direct native invocations select the first executable `bash` on
    /// the snapshotted `PATH`; relative entries are anchored to [`Self::cwd`].
    pub(crate) fn bash(&self) -> Option<PathBuf> {
        match self.value("BASH") {
            Some(value) => {
                let path = PathBuf::from(value);
                (path.is_absolute() && executable(&path)).then_some(path)
            }
            None => self.find_on_path("bash"),
        }
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

fn validate_cwd(cwd: &Path) -> std::io::Result<()> {
    if cwd.is_absolute() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "dot runtime working directory must be absolute",
        ))
    }
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Borrowed stdout and stderr for one Dot invocation.
pub struct Streams<'a> {
    pub(crate) stdout: &'a mut dyn Write,
    pub(crate) stderr: &'a mut dyn Write,
    stdout_terminal: bool,
}

impl<'a> Streams<'a> {
    /// Bind stdout and stderr for one Dot invocation.
    pub fn new(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write) -> Self {
        Self {
            stdout,
            stderr,
            stdout_terminal: std::io::stdout().is_terminal(),
        }
    }

    /// Bind streams with an explicit stdout terminal observation.
    ///
    /// This is an embedding and test seam for callers whose writer is not the
    /// process-global stdout handle.
    #[doc(hidden)]
    pub fn with_terminal(
        stdout: &'a mut dyn Write,
        stderr: &'a mut dyn Write,
        stdout_terminal: bool,
    ) -> Self {
        Self {
            stdout,
            stderr,
            stdout_terminal,
        }
    }

    pub(crate) fn stdout_is_terminal(&self) -> bool {
        self.stdout_terminal
    }
}

/// Dispatch one command using only the supplied runtime and streams.
///
/// This public embedding boundary always isolates the invocation in a child
/// process. The ordinary binary entry calls [`run_direct`] after capturing its
/// Runtime once, so it never reads the environment or working directory again.
pub fn run(runtime: &Runtime, args: &[OsString], streams: &mut Streams<'_>) -> i32 {
    reexec(runtime, args, streams)
}

/// Dispatch an already-captured direct process entry without re-executing.
///
/// This is public only because the package binary is a separate Rust crate;
/// it is an implementation entry, not an embedding API.
#[doc(hidden)]
pub fn run_direct(runtime: &Runtime, args: &[OsString], streams: &mut Streams<'_>) -> i32 {
    let host_git = args
        .first()
        .filter(|arg| arg.as_os_str() == OsStr::new("init"))
        .and_then(|_| runtime.value("HOME").and_then(OsStr::to_str))
        .and_then(|home| {
            runtime
                .value("PATH")
                .and_then(OsStr::to_str)
                .and_then(|path| {
                    crate::init_client_identity::select_host_git(
                        home,
                        &runtime.source_root().to_string_lossy(),
                        path,
                    )
                })
        });
    match host_git {
        Some(git) => crate::init_client_identity::with_host_git(Path::new(&git), || {
            crate::cli::run_with_runtime(runtime, args, streams.stdout, streams.stderr)
        }),
        None => crate::cli::run_with_runtime(runtime, args, streams.stdout, streams.stderr),
    }
}

/// Execute an embedded Runtime in a child whose real ambient namespace is its
/// immutable snapshot. This lets existing native helpers retain ordinary
/// process semantics while concurrent Runtime callers cannot share PATH,
/// TMPDIR, WSL markers, or child environment inheritance.
fn reexec(runtime: &Runtime, args: &[OsString], streams: &mut Streams<'_>) -> i32 {
    let executable = match runtime.executable.as_ref() {
        Some(executable) => &executable.0,
        None => {
            let _ = streams
                .stderr
                .write_all(b"dot: embedded runtime requires an executable\n");
            return 1;
        }
    };
    let display = executable.to_string_lossy().into_owned();
    let output = match Command::new(executable)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_runtime_republishes_derived_source_root() {
        let cwd = std::env::current_dir().expect("absolute test cwd");
        let mut env = BTreeMap::new();
        env.insert(
            OsString::from("DOT_SOURCE_ROOT"),
            OsString::from("/untrusted"),
        );

        let runtime = Runtime::from_process_env(&env, &cwd).expect("owned test executable");

        assert_ne!(runtime.source_root(), Path::new("/untrusted"));
        assert_eq!(
            runtime.value("DOT_SOURCE_ROOT"),
            Some(runtime.source_root().as_os_str())
        );
    }
}
