//! Runtime-owned launcher for the versioned extension worker.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::Runtime;
use crate::profile_lifecycle::{WorkerOutcome, WorkerRun};

/// The shell-control variables that must not affect a noninteractive worker
/// before its versioned protocol has established its own baseline. This is
/// the narrow control-plane scrub in `extension-worker-launch.sh`; ordinary
/// runtime variables intentionally remain available to client hooks.
const STARTUP_CONTROLS: [&str; 8] = [
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
    "BASH_COMPAT",
    "POSIXLY_CORRECT",
    "BASH_XTRACEFD",
    "BASHOPTS",
];

/// Executes lifecycle hooks through the one versioned shell worker.
///
/// This type owns no lifecycle policy. It merely supplies the worker seam
/// after `profile_lifecycle` has validated a fixed hook and minted its
/// one-use context. Its snapshot keeps child startup and hook-visible paths
/// tied to the invocation Runtime rather than the parent process.
pub(crate) struct Worker {
    runtime: Runtime,
    extensions_dir: Option<PathBuf>,
}

/// One pre-sync worker's separately routed process streams.
pub(crate) struct PreSyncOutcome {
    pub(crate) rc: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl Worker {
    /// Bind this worker to one immutable invocation runtime.
    pub(crate) fn new(runtime: &Runtime) -> Self {
        Self {
            runtime: runtime.clone(),
            extensions_dir: None,
        }
    }

    /// Bind the refreshed configuration's extension root for a pre-sync
    /// worker. Runtime remains the source for every other child input.
    pub(crate) fn with_extensions(runtime: &Runtime, extensions_dir: &str) -> Self {
        Self {
            runtime: runtime.clone(),
            extensions_dir: (!extensions_dir.is_empty()).then(|| PathBuf::from(extensions_dir)),
        }
    }

    /// Run the native pre-sync coordinator's one-use call through the same
    /// sanitized launcher used for lifecycle retirement.
    pub(crate) fn pre_sync(&mut self, call: &crate::pre_sync::Call) -> PreSyncOutcome {
        let Some(mut command) = self.command(
            "pre-sync",
            &call.script,
            &call.temporary,
            &call.result,
            &call.context,
            &call.token,
        ) else {
            return PreSyncOutcome {
                rc: 1,
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
        };
        separate(&mut command, &call.temporary)
    }

    fn command(
        &self,
        mode: &str,
        script: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> Option<Command> {
        let source_root = self.runtime.source_root();
        let source_root_text = source_root.to_str()?;
        let result_text = result_file.to_str()?;
        if crate::extension_worker::main_precheck(5, mode, source_root_text, result_text).is_err()
            || (mode == "deactivate"
                && !crate::extension_worker::deactivate_set_valid("retiring", 1))
        {
            return None;
        }
        let bash = bash_path(&self.runtime)?;
        let (cache, data) = (
            xdg_home(&self.runtime, "XDG_CACHE_HOME", ".cache"),
            xdg_home(&self.runtime, "XDG_DATA_HOME", ".local/share"),
        );
        let (cache, data) = (cache?, data?);
        let worker = source_root.join("lib/dot/extension-worker.sh");
        let mut command = Command::new(bash);
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg(worker)
            .arg(mode)
            .arg(script)
            .arg(result_file)
            .arg(context)
            .arg(token)
            .env_clear()
            .envs(self.runtime.env())
            .env("HOME", self.runtime.home())
            .env("DOT_SOURCE_ROOT", source_root)
            .env("TMPDIR", result_dir)
            .env("XDG_CONFIG_HOME", self.runtime.config_home())
            .env("XDG_STATE_HOME", self.runtime.state_home())
            .env("XDG_CACHE_HOME", cache)
            .env("XDG_DATA_HOME", data)
            .current_dir(self.runtime.cwd())
            .stdin(Stdio::null());
        if let Some(extensions_dir) = &self.extensions_dir {
            command.env("DOT_EXTENSIONS_DIR", extensions_dir);
        }
        for key in STARTUP_CONTROLS {
            command.env_remove(key);
        }
        command.env_remove("SHELLOPTS");
        for key in self.runtime.env().keys() {
            if exported_function(key) {
                command.env_remove(key);
            }
        }
        Some(command)
    }

    fn launch(
        &self,
        mode: &str,
        script: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome {
        let Some(mut command) = self.command(mode, script, result_dir, result_file, context, token)
        else {
            return WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            };
        };
        combined(&mut command, result_dir)
    }
}

/// Resolve the shell-authorized absolute Bash executable from the Runtime.
/// The launcher rejects PATH lookup, a relative `$BASH`, and a non-executable
/// target before it ever starts the worker; the native boundary has the same
/// authority rule rather than silently selecting a different interpreter.
fn bash_path(runtime: &Runtime) -> Option<PathBuf> {
    let path = PathBuf::from(runtime.value("BASH")?);
    (path.is_absolute() && executable(&path)).then_some(path)
}

/// A regular executable file, matching the launcher’s absolute executable
/// gate without invoking the parent shell for resolution.
fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Resolve an XDG worker baseline exactly like `dot_xdg_home`: an absolute
/// explicit value wins, otherwise a usable absolute HOME receives its fixed
/// suffix. A malformed Runtime cannot safely launch the worker.
fn xdg_home(runtime: &Runtime, key: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(value) = runtime.value(key) {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Some(path);
        }
    }
    let home = runtime.home();
    if !home.is_absolute() {
        return None;
    }
    if home == Path::new("/") {
        Some(PathBuf::from(format!("/{fallback}")))
    } else {
        Some(home.join(fallback))
    }
}

/// Bash's `2>&1` gives both streams one open file description, so writes keep
/// their observable order. A private scratch file gives `Command` the same
/// property without racing independent stdout/stderr readers.
fn combined(command: &mut Command, result_dir: &Path) -> WorkerOutcome {
    let path = result_dir.join("worker-output");
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(_) => {
            return WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            };
        }
    };
    let stdout = match file.try_clone() {
        Ok(file) => file,
        Err(_) => {
            return WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            };
        }
    };
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(file));
    let status = command.status();
    let output = std::fs::read(&path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    WorkerOutcome {
        rc: status.ok().and_then(|status| status.code()).unwrap_or(1),
        output,
    }
}

/// Allocate one private capture file under the worker's already-private
/// temporary directory. `create_new` keeps a malicious hook from replacing a
/// stream path before the parent opens it.
fn stream_file(result_dir: &Path, name: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let path = result_dir.join(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((path, file))
}

/// Capture pre-sync stdout and stderr independently, matching the shell's
/// inherited streams. Files avoid pipe backpressure deadlock; the surrounding
/// update engine already buffers its command streams, so this adds no output
/// limit beyond that established update boundary.
fn separate(command: &mut Command, result_dir: &Path) -> PreSyncOutcome {
    let (stdout_path, stdout) = match stream_file(result_dir, "worker-stdout") {
        Ok(capture) => capture,
        Err(_) => return failed_pre_sync(),
    };
    let (stderr_path, stderr) = match stream_file(result_dir, "worker-stderr") {
        Ok(capture) => capture,
        Err(_) => {
            let _ = std::fs::remove_file(stdout_path);
            return failed_pre_sync();
        }
    };
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let status = command.status();
    let stdout = std::fs::read(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read(&stderr_path).unwrap_or_default();
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
    PreSyncOutcome {
        rc: status.ok().and_then(|status| status.code()).unwrap_or(1),
        stdout,
        stderr,
    }
}

fn failed_pre_sync() -> PreSyncOutcome {
    PreSyncOutcome {
        rc: 1,
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

/// True for Bash's serialized exported-function environment keys. They are
/// evaluated while Bash initializes, before the versioned worker can establish
/// its own protocol, so the launch boundary must remove every such record.
fn exported_function(key: &OsStr) -> bool {
    let bytes = key.as_bytes();
    bytes.starts_with(b"BASH_FUNC_") && bytes.ends_with(b"%%")
}

impl WorkerRun for Worker {
    fn run(
        &mut self,
        script: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome {
        self.launch(
            "deactivate",
            script,
            result_dir,
            result_file,
            context,
            token,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::Worker;
    use crate::app::Runtime;
    use crate::log::Log;
    use crate::profile_lifecycle;
    use crate::test_support::TempDir;

    fn git_repo(path: &Path, origin: &str) {
        std::fs::create_dir_all(path).expect("repo directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("repo directory mode");
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git init");
        assert!(status.success(), "git init {}", path.display());
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(origin)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git remote add");
        assert!(status.success(), "git remote add {}", path.display());
    }

    fn checkout(home: &Path, script: &[u8]) -> String {
        let repo = home.join(".dotfiles-web");
        git_repo(&repo, "file:///repo/web.git");
        let hook_dir = repo.join("dot");
        std::fs::create_dir(&hook_dir).expect("hook directory");
        std::fs::set_permissions(&hook_dir, std::fs::Permissions::from_mode(0o755))
            .expect("hook directory mode");
        let hook = hook_dir.join("profile-deactivate");
        std::fs::write(&hook, script).expect("hook script");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o600))
            .expect("hook script mode");
        format!(
            "web|{}|file:///repo/web.git|{}/conf/10-web.conf|false|git",
            repo.display(),
            home.display()
        )
    }

    fn runtime(home: &Path, state: &Path, bash_env: &Path) -> Runtime {
        let path = std::env::var_os("PATH").expect("test PATH");
        let env = BTreeMap::from([
            (OsString::from("HOME"), home.as_os_str().to_owned()),
            (
                OsString::from("XDG_STATE_HOME"),
                state.as_os_str().to_owned(),
            ),
            (OsString::from("PATH"), path),
            (
                OsString::from("BASH"),
                crate::test_support::bash().as_os_str().to_owned(),
            ),
            (OsString::from("BASH_ENV"), bash_env.as_os_str().to_owned()),
            (
                OsString::from("DOT_SOURCE_ROOT"),
                OsString::from(env!("CARGO_MANIFEST_DIR")),
            ),
            (OsString::from("LC_ALL"), OsString::from("C")),
        ]);
        Runtime::from_env(&env, home).expect("runtime")
    }

    fn run(runtime: &Runtime, home: &Path, record: &str) -> (i32, Vec<u8>, Vec<u8>) {
        let log = Log::new(false, false);
        let mut worker = Worker::new(runtime);
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        let tmpdir = home.join("scratch");
        std::fs::create_dir(&tmpdir).expect("scratch directory");
        let inputs = profile_lifecycle::RunInputs {
            record,
            home: home.to_str().expect("home text"),
            euid: crate::temp::current_uid().expect("current uid"),
            tmpdir: &tmpdir,
            now_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_secs() as i64,
            verbose: true,
            log: &log,
        };
        let rc = profile_lifecycle::run_one(&inputs, &mut worker, &mut out, &mut warnings);
        (rc, out, warnings)
    }

    #[test]
    fn worker_scrubs_startup_controls_and_uses_runtime_context() {
        let scope = TempDir::new("hook-worker-context").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        let marker = scope.path().join("startup-control-ran");
        std::fs::write(&poison, format!("touch {}\n", marker.display())).expect("BASH_ENV");
        let record = checkout(
            &home,
            b"deactivate() { printf 'retiring=%s home=%s state=%s tmp=%s\\n' \\
  \"$DOT_RETIRING_OVERLAY\" \"$HOME\" \"$XDG_STATE_HOME\" \"$TMPDIR\"; }\n",
        );
        let runtime = runtime(&home, &state, &poison);

        let (rc, out, warnings) = run(&runtime, &home, &record);

        assert_eq!(rc, 0);
        assert!(warnings.is_empty());
        assert!(!marker.exists(), "BASH_ENV must not reach the worker");
        let text = String::from_utf8(out).expect("worker output");
        assert!(text.starts_with(&format!(
            "retiring=web home={} state={} tmp={}",
            home.display(),
            state.display(),
            home.join("scratch").display()
        )));
    }

    #[test]
    fn worker_relays_hook_failure_without_treating_it_as_success() {
        let scope = TempDir::new("hook-worker-failure").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b"exit 99\n").expect("BASH_ENV");
        let record = checkout(
            &home,
            b"deactivate() { printf 'deactivation-failed\\n' >&2; return 7; }\n",
        );
        let runtime = runtime(&home, &state, &poison);

        let (rc, out, warnings) = run(&runtime, &home, &record);

        assert_eq!(rc, 7);
        assert!(out.is_empty());
        assert_eq!(warnings, b"deactivation-failed\n");
    }

    #[test]
    fn worker_scrubs_exported_function_and_command_shadow_records() {
        let scope = TempDir::new("hook-worker-functions").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b":\n").expect("BASH_ENV");
        let record = checkout(&home, b"deactivate() { helper; printf 'safe\\n'; }\n");
        let mut env = runtime(&home, &state, &poison).env().clone();
        env.insert(
            OsString::from("BASH_FUNC_helper%%"),
            OsString::from(format!(
                "() {{ touch {}/function-imported; }}",
                home.display()
            )),
        );
        env.insert(
            OsString::from("BASH_FUNC_printf%%"),
            OsString::from(format!(
                "() {{ touch {}/command-shadowed; }}",
                home.display()
            )),
        );
        let runtime = Runtime::from_env(&env, &home).expect("runtime");

        let (rc, out, warnings) = run(&runtime, &home, &record);

        assert_eq!(rc, 127, "missing helper must not be imported");
        assert!(out.is_empty());
        assert!(
            warnings
                .windows(b"helper: command not found".len())
                .any(|row| { row == b"helper: command not found" })
        );
        assert!(!home.join("function-imported").exists());
        assert!(!home.join("command-shadowed").exists());
    }

    #[test]
    fn worker_keeps_stdout_and_stderr_interleaved() {
        let scope = TempDir::new("hook-worker-interleave").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b":\n").expect("BASH_ENV");
        let record = checkout(
            &home,
            b"deactivate() { printf one; printf two >&2; printf three; printf four >&2; }\n",
        );
        let runtime = runtime(&home, &state, &poison);

        let (rc, out, warnings) = run(&runtime, &home, &record);

        assert_eq!(rc, 0);
        assert!(warnings.is_empty());
        assert_eq!(out, b"onetwothreefour\n");
    }

    #[test]
    fn worker_requires_the_shell_authorized_absolute_bash() {
        let scope = TempDir::new("hook-worker-bash").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b":\n").expect("BASH_ENV");
        let mut env = runtime(&home, &state, &poison).env().clone();
        env.insert(OsString::from("BASH"), OsString::from("bash"));
        let runtime = Runtime::from_env(&env, &home).expect("runtime");

        assert!(super::bash_path(&runtime).is_none());
    }
}
