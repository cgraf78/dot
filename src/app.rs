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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

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
    bash: Arc<OnceLock<Result<crate::bash::Resolved, crate::bash::Error>>>,
    bash_error_reported: Arc<AtomicBool>,
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
            bash: Arc::new(OnceLock::new()),
            bash_error_reported: Arc::new(AtomicBool::new(false)),
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

    /// Resolve and cache the Bash 4+ capability for retained shell boundaries.
    pub(crate) fn bash(&self) -> Result<crate::bash::Resolved, crate::bash::Error> {
        self.bash
            .get_or_init(|| crate::bash::resolve(&self.env, &self.cwd))
            .clone()
    }

    /// Return the cached Bash failure diagnostic at most once per invocation.
    pub(crate) fn bash_error_line_once(&self, error: &crate::bash::Error) -> Vec<u8> {
        if self.bash_error_reported.swap(true, Ordering::AcqRel) {
            Vec::new()
        } else {
            error.line()
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
    use std::os::unix::fs::symlink;

    use dot_test_support::TempDir;

    fn test_runtime(cwd: &Path, entries: &[(&str, &OsStr)]) -> Runtime {
        let mut env = BTreeMap::from([
            (OsString::from("HOME"), cwd.as_os_str().to_owned()),
            (
                OsString::from("DOT_SOURCE_ROOT"),
                OsString::from(env!("CARGO_MANIFEST_DIR")),
            ),
        ]);
        for (key, value) in entries {
            env.insert(OsString::from(key), (*value).to_os_string());
        }
        Runtime::from_env(&env, cwd).expect("runtime")
    }

    fn bash_link(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("bash parent")).expect("bash parent");
        symlink(dot_test_support::bash(), &path).expect("bash link");
        path
    }

    fn fake_bash(root: &Path, relative: &str, body: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("bash parent")).expect("bash parent");
        let body = body
            .strip_prefix("#!/bin/sh\n")
            .expect("fake Bash uses the fixture shell");
        let body = format!("#!/bin/sh\n[ \"${{1-}}\" != --dot-fixture-ready ] || exit 0\n{body}");
        std::fs::write(&path, body).expect("fake Bash");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("fake Bash mode");
        dot_test_support::wait_until_executable(&path, &["--dot-fixture-ready"])
            .expect("fake Bash executable");
        path
    }

    #[test]
    fn bash_prefers_strict_dot_bash_override() {
        let scope = TempDir::new_exec("bash-dot-override").expect("scope");
        let explicit = bash_link(scope.path(), "explicit/bash");
        let ambient = bash_link(scope.path(), "ambient/bash");
        let runtime = test_runtime(
            scope.path(),
            &[
                ("DOT_BASH", explicit.as_os_str()),
                ("BASH", ambient.as_os_str()),
                ("PATH", OsStr::new("")),
            ],
        );

        assert_eq!(runtime.bash().expect("selected Bash").path(), explicit);
    }

    #[test]
    fn bash_rejects_invalid_dot_bash_without_fallback() {
        let scope = TempDir::new_exec("bash-invalid-dot-override").expect("scope");
        let ambient = bash_link(scope.path(), "ambient/bash");
        let missing = scope.path().join("missing/bash");
        let runtime = test_runtime(
            scope.path(),
            &[
                ("DOT_BASH", missing.as_os_str()),
                ("BASH", ambient.as_os_str()),
                ("PATH", OsStr::new("")),
            ],
        );

        assert!(runtime.bash().is_err());
    }

    #[test]
    fn bash_treats_invalid_ambient_bash_as_a_soft_hint() {
        let scope = TempDir::new_exec("bash-soft-ambient").expect("scope");
        let path_bash = bash_link(scope.path(), "path/bash");
        let missing = scope.path().join("missing/bash");
        let path = path_bash.parent().expect("PATH directory");
        let runtime = test_runtime(
            scope.path(),
            &[("BASH", missing.as_os_str()), ("PATH", path.as_os_str())],
        );

        assert_eq!(runtime.bash().expect("PATH Bash").path(), path_bash);
    }

    #[test]
    fn bash_rejects_pre_v4_ambient_hint_and_uses_path() {
        let scope = TempDir::new_exec("bash-old-ambient").expect("scope");
        let old = fake_bash(
            scope.path(),
            "old/bash",
            "#!/bin/sh\nprintf 'cgraf78-dot-bash-v1:3:3.2.57(1)-release\\n'\n",
        );
        let path_bash = bash_link(scope.path(), "path/bash");
        let runtime = test_runtime(
            scope.path(),
            &[
                ("BASH", old.as_os_str()),
                (
                    "PATH",
                    path_bash.parent().expect("PATH directory").as_os_str(),
                ),
            ],
        );

        assert_eq!(runtime.bash().expect("PATH Bash").path(), path_bash);
    }

    #[test]
    fn bash_ignores_relative_path_entries() {
        let scope = TempDir::new_exec("bash-relative-path").expect("scope");
        let relative = bash_link(scope.path(), "relative/bin/bash");
        let absolute = bash_link(scope.path(), "absolute/bin/bash");
        let path = std::env::join_paths([
            Path::new("relative/bin"),
            absolute.parent().expect("absolute PATH directory"),
        ])
        .expect("PATH");
        let runtime = test_runtime(scope.path(), &[("PATH", path.as_os_str())]);

        assert_eq!(runtime.bash().expect("absolute PATH Bash").path(), absolute);
        assert_ne!(runtime.bash().expect("cached Bash").path(), relative);
    }

    #[test]
    fn bash_uses_prefix_after_path_candidates() {
        let scope = TempDir::new_exec("bash-prefix").expect("scope");
        let prefix = scope.path().join("prefix");
        let prefix_bash = bash_link(&prefix, "bin/bash");
        let runtime = test_runtime(
            scope.path(),
            &[
                ("PATH", OsStr::new("/definitely/missing")),
                ("PREFIX", prefix.as_os_str()),
            ],
        );

        assert_eq!(runtime.bash().expect("PREFIX Bash").path(), prefix_bash);
    }

    #[test]
    fn bash_selection_is_lazy_and_shared_by_runtime_clones() {
        let scope = TempDir::new_exec("bash-selection-cache").expect("scope");
        let marker = scope.path().join("probe-count");
        let real = dot_test_support::bash();
        let wrapper = fake_bash(
            scope.path(),
            "wrapper/bash",
            &format!(
                "#!/bin/sh\nprintf x >>'{}'\nexec '{}' \"$@\"\n",
                marker.display(),
                real.display()
            ),
        );
        let poison = scope.path().join("bash-env");
        let poison_marker = scope.path().join("bash-env-ran");
        std::fs::write(
            &poison,
            format!("printf poison >'{}'\n", poison_marker.display()),
        )
        .expect("BASH_ENV");
        let runtime = test_runtime(
            scope.path(),
            &[
                ("DOT_BASH", wrapper.as_os_str()),
                ("BASH_ENV", poison.as_os_str()),
            ],
        );
        let clone = runtime.clone();

        assert!(!marker.exists(), "Runtime construction must not probe Bash");
        assert_eq!(runtime.bash().expect("selected Bash").path(), wrapper);
        assert_eq!(clone.bash().expect("shared selected Bash").path(), wrapper);
        assert_eq!(std::fs::read(&marker).expect("probe marker"), b"x");
        assert!(
            !poison_marker.exists(),
            "BASH_ENV must not run during probe"
        );
    }

    #[test]
    fn failed_bash_selection_is_cached_across_runtime_clones() {
        let scope = TempDir::new_exec("bash-negative-cache").expect("scope");
        let marker = scope.path().join("probe-count");
        let old = fake_bash(
            scope.path(),
            "old/bash",
            &format!(
                "#!/bin/sh\nprintf x >>'{}'\nprintf 'cgraf78-dot-bash-v1:3:3.2.57(1)-release\\n'\n",
                marker.display()
            ),
        );
        let runtime = test_runtime(scope.path(), &[("DOT_BASH", old.as_os_str())]);
        let clone = runtime.clone();

        assert!(runtime.bash().is_err());
        assert!(clone.bash().is_err());
        assert_eq!(std::fs::read(&marker).expect("probe marker"), b"x");
    }

    #[test]
    fn bash_failure_diagnostic_is_shared_once_across_runtime_clones() {
        let scope = TempDir::new_exec("bash-error-once").expect("scope");
        let missing = scope.path().join("missing/bash");
        let runtime = test_runtime(scope.path(), &[("DOT_BASH", missing.as_os_str())]);
        let clone = runtime.clone();
        let error = runtime.bash().expect_err("invalid Bash");

        assert_eq!(runtime.bash_error_line_once(&error), error.line());
        assert!(clone.bash_error_line_once(&error).is_empty());
    }

    #[test]
    fn concurrent_runtime_clones_share_one_bash_probe() {
        let scope = TempDir::new_exec("bash-concurrent-cache").expect("scope");
        let marker = scope.path().join("probe-count");
        let real = dot_test_support::bash();
        let wrapper = fake_bash(
            scope.path(),
            "wrapper/bash",
            &format!(
                "#!/bin/sh\nprintf x >>'{}'\nexec '{}' \"$@\"\n",
                marker.display(),
                real.display()
            ),
        );
        let runtime = test_runtime(scope.path(), &[("DOT_BASH", wrapper.as_os_str())]);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let runtime = runtime.clone();
                let expected = wrapper.clone();
                scope.spawn(move || assert_eq!(runtime.bash().expect("Bash").path(), expected));
            }
        });
        assert_eq!(std::fs::read(&marker).expect("probe marker"), b"x");
    }

    #[test]
    fn independent_runtimes_keep_independent_bash_caches() {
        let scope = TempDir::new_exec("bash-independent-cache").expect("scope");
        let first = bash_link(scope.path(), "first/bash");
        let second = bash_link(scope.path(), "second/bash");
        let first_runtime = test_runtime(scope.path(), &[("DOT_BASH", first.as_os_str())]);
        let second_runtime = test_runtime(scope.path(), &[("DOT_BASH", second.as_os_str())]);

        assert_eq!(first_runtime.bash().expect("first Bash").path(), first);
        assert_eq!(second_runtime.bash().expect("second Bash").path(), second);
    }

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
