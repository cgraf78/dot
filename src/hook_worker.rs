//! Runtime-owned launcher for the versioned extension worker.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::Runtime;
use crate::profile_lifecycle::{WorkerOutcome, WorkerRun};

/// Executes lifecycle hooks through the one versioned shell worker.
///
/// This type owns no lifecycle policy. It merely supplies the worker seam
/// after `profile_lifecycle` has validated a fixed hook and minted its
/// one-use context. Its snapshot keeps child startup and hook-visible paths
/// tied to the invocation Runtime rather than the parent process.
pub(crate) struct Worker {
    runtime: Runtime,
    extensions_dir: Option<PathBuf>,
    overlay_manifest: Option<PathBuf>,
    update_lock_token: Option<String>,
    quiet: bool,
    force: bool,
    verbose: bool,
    bash: Option<PathBuf>,
}

/// Resolved update-generation values exported to public hook APIs.
pub(crate) struct UpdateEnvironment<'a> {
    pub(crate) extensions_dir: &'a str,
    pub(crate) overlay_manifest: &'a str,
    pub(crate) update_lock_token: Option<&'a str>,
    pub(crate) quiet: bool,
    pub(crate) force: bool,
    pub(crate) verbose: bool,
}

/// One pre-sync worker's separately routed process streams.
pub(crate) struct PreSyncOutcome {
    pub(crate) rc: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

enum CommandFailure {
    Invalid,
    Bash(crate::bash::Error),
}

impl Worker {
    /// Bind the refreshed configuration's extension root for a pre-sync
    /// worker. Runtime remains the source for every other child input.
    pub(crate) fn for_update(runtime: &Runtime, env: &UpdateEnvironment<'_>) -> Self {
        Self {
            runtime: runtime.clone(),
            extensions_dir: (!env.extensions_dir.is_empty())
                .then(|| PathBuf::from(env.extensions_dir)),
            overlay_manifest: (!env.overlay_manifest.is_empty())
                .then(|| PathBuf::from(env.overlay_manifest)),
            update_lock_token: env
                .update_lock_token
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            quiet: env.quiet,
            force: env.force,
            verbose: env.verbose,
            bash: None,
        }
    }

    /// Bind an already-resolved absolute Bash for doctor extensions. A native
    /// CLI process does not inherit Bash's non-exported `BASH` variable, while
    /// the former shell coordinator did; the coordinator therefore passes the
    /// interpreter capability it used for the runtime health probe.
    pub(crate) fn with_doctor(runtime: &Runtime, extensions_dir: &str, bash: PathBuf) -> Self {
        Self {
            runtime: runtime.clone(),
            extensions_dir: Some(PathBuf::from(extensions_dir)),
            overlay_manifest: None,
            update_lock_token: None,
            quiet: false,
            force: false,
            verbose: false,
            bash: Some(bash),
        }
    }

    /// Run the native pre-sync coordinator's one-use call through the same
    /// sanitized launcher used for lifecycle retirement.
    pub(crate) fn pre_sync(&mut self, call: &crate::pre_sync::Call) -> PreSyncOutcome {
        let command = match self.command(
            "pre-sync",
            &call.script,
            &call.temporary,
            &call.result,
            &call.context,
            &call.token,
        ) {
            Ok(command) => command,
            Err(CommandFailure::Invalid) => return failed_pre_sync(Vec::new()),
            Err(CommandFailure::Bash(error)) => {
                return failed_pre_sync(self.runtime.bash_error_line_once(&error));
            }
        };
        separate(command, &call.temporary)
    }

    /// Run one merge hook through the same authenticated worker boundary. Merge
    /// output deliberately keeps Bash's ordered `2>&1` capture contract.
    pub(crate) fn merge(
        &mut self,
        script: &Path,
        temporary: &Path,
        result: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome {
        self.launch("merge", script, temporary, result, context, token)
    }

    /// Run one doctor extension through the same sanitized, authenticated
    /// worker boundary used by lifecycle and merge hooks.
    pub(crate) fn doctor(
        &mut self,
        script: &Path,
        temporary: &Path,
        result: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome {
        self.launch("doctor", script, temporary, result, context, token)
    }

    fn command(
        &self,
        mode: &str,
        script: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> Result<Command, CommandFailure> {
        let source_root = self.runtime.source_root();
        let source_root_text = source_root.to_str().ok_or(CommandFailure::Invalid)?;
        let result_text = result_file.to_str().ok_or(CommandFailure::Invalid)?;
        if crate::extension_worker::main_precheck(5, mode, source_root_text, result_text).is_err()
            || (mode == "deactivate"
                && !crate::extension_worker::deactivate_set_valid("retiring", 1))
        {
            return Err(CommandFailure::Invalid);
        }
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|span| i64::try_from(span.as_secs()).ok())
            .ok_or(CommandFailure::Invalid)?;
        let euid = crate::temp::current_uid().ok_or(CommandFailure::Invalid)?;
        let home = self
            .runtime
            .home()
            .to_str()
            .ok_or(CommandFailure::Invalid)?;
        let decoded = crate::overlay_context::consume(context, token, mode, home, euid, now_secs)
            .map_err(|_| CommandFailure::Invalid)?;
        let manifest = self
            .overlay_manifest
            .clone()
            .unwrap_or_else(|| self.runtime.state_home().join("dot/overlay-links"));
        let extensions_dir = self.extensions_dir.as_deref().unwrap_or(Path::new(""));
        let trust = crate::extension_trust::Inputs {
            euid,
            home: home.to_string(),
            extensions_dir: extensions_dir.to_string_lossy().into_owned(),
            manifest: manifest.to_string_lossy().into_owned(),
            retiring_root: String::new(),
        };
        let (retiring_name, retiring_root) = if mode == "deactivate" {
            let record = decoded.records.first().ok_or(CommandFailure::Invalid)?;
            let script = script.to_str().ok_or(CommandFailure::Invalid)?;
            crate::extension_trust::deactivation_validate(record, script, home, euid)
                .map_err(|_| CommandFailure::Invalid)?;
            let mut fields = record.split('|');
            (
                fields.next().unwrap_or("").to_string(),
                fields.next().unwrap_or("").to_string(),
            )
        } else {
            if !crate::extension_trust::file_validate(script, &trust, &decoded.records) {
                return Err(CommandFailure::Invalid);
            }
            (String::new(), String::new())
        };
        let decoded_path = result_dir.join("worker-context");
        let record_count = decoded.records.len().to_string();
        let mut decoded_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&decoded_path)
            .map_err(|_| CommandFailure::Invalid)?;
        for field in [
            decoded.stage.as_str(),
            decoded.set_kind.as_str(),
            retiring_name.as_str(),
            retiring_root.as_str(),
            record_count.as_str(),
        ] {
            decoded_file
                .write_all(field.as_bytes())
                .map_err(|_| CommandFailure::Invalid)?;
            decoded_file
                .write_all(&[0])
                .map_err(|_| CommandFailure::Invalid)?;
        }
        for record in &decoded.records {
            decoded_file
                .write_all(record.as_bytes())
                .map_err(|_| CommandFailure::Invalid)?;
            decoded_file
                .write_all(&[0])
                .map_err(|_| CommandFailure::Invalid)?;
        }
        decoded_file.flush().map_err(|_| CommandFailure::Invalid)?;
        drop(decoded_file);
        let bash = match self.bash.clone() {
            Some(bash) => bash,
            None => self
                .runtime
                .bash()
                .map_err(CommandFailure::Bash)?
                .path()
                .to_path_buf(),
        };
        let (cache, data) = (
            xdg_home(&self.runtime, "XDG_CACHE_HOME", ".cache"),
            xdg_home(&self.runtime, "XDG_DATA_HOME", ".local/share"),
        );
        let (cache, data) = (
            cache.ok_or(CommandFailure::Invalid)?,
            data.ok_or(CommandFailure::Invalid)?,
        );
        let worker = source_root.join("lib/dot/public/hook-runtime-v1/worker.sh");
        let mut command = Command::new(bash);
        crate::bash::sanitized_env(&mut command, self.runtime.env());
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg(worker)
            .arg(mode)
            .arg(script)
            .arg(result_file)
            .arg(decoded_path)
            .env("HOME", self.runtime.home())
            .env("DOT_SOURCE_ROOT", source_root)
            .env("TMPDIR", result_dir)
            .env("XDG_CONFIG_HOME", self.runtime.config_home())
            .env("XDG_STATE_HOME", self.runtime.state_home())
            .env("XDG_CACHE_HOME", cache)
            .env("XDG_DATA_HOME", data)
            // Public hook helpers use the manifest to validate overlay links.
            // Reassert it after `env_clear` so hooks see the same resolved
            // path the native engine used to build their context.
            .env("DOT_OVERLAY_MANIFEST", manifest)
            .current_dir(self.runtime.cwd())
            .stdin(Stdio::null());
        if let Some(extensions_dir) = &self.extensions_dir {
            command.env("DOT_EXTENSIONS_DIR", extensions_dir);
        }
        if let Some(token) = &self.update_lock_token {
            command.env("DOT_UPDATE_LOCK_TOKEN", token);
        }
        if self.quiet {
            command.env("DOT_QUIET", "1").env("SHDEPS_QUIET", "1");
        }
        if self.force {
            command.env("DOT_FORCE", "1").env("SHDEPS_FORCE", "1");
        }
        if self.verbose {
            command.env("DOT_VERBOSE", "1").env("SHDEPS_LOG_LEVEL", "2");
        }
        Ok(command)
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
        let command = match self.command(mode, script, result_dir, result_file, context, token) {
            Ok(command) => command,
            Err(CommandFailure::Invalid) => {
                // TEMP-DIAG-180: remove after the macOS hooks-test diagnosis.
                if std::env::var_os("DOT_TEST_DIAG_HOOKS").is_some() {
                    eprintln!("TEMP-DIAG-180 HOOKS-INVALID-COMMAND {mode} {script:?}");
                }
                return WorkerOutcome {
                    rc: 1,
                    output: Vec::new(),
                };
            }
            Err(CommandFailure::Bash(error)) => {
                // TEMP-DIAG-180: remove after the macOS hooks-test diagnosis.
                if std::env::var_os("DOT_TEST_DIAG_HOOKS").is_some() {
                    eprintln!("TEMP-DIAG-180 HOOKS-BASH-FAIL {mode} {script:?}: {error}");
                }
                return WorkerOutcome {
                    rc: 1,
                    output: self.runtime.bash_error_line_once(&error),
                };
            }
        };
        combined(command, result_dir)
    }
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
fn combined(mut command: Command, result_dir: &Path) -> WorkerOutcome {
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
    let status = wait(command);
    let output = std::fs::read(&path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    WorkerOutcome {
        rc: status.unwrap_or(1),
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
fn separate(mut command: Command, result_dir: &Path) -> PreSyncOutcome {
    let (stdout_path, stdout) = match stream_file(result_dir, "worker-stdout") {
        Ok(capture) => capture,
        Err(_) => return failed_pre_sync(Vec::new()),
    };
    let (stderr_path, stderr) = match stream_file(result_dir, "worker-stderr") {
        Ok(capture) => capture,
        Err(_) => {
            let _ = std::fs::remove_file(stdout_path);
            return failed_pre_sync(Vec::new());
        }
    };
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let status = wait(command);
    let stdout = std::fs::read(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read(&stderr_path).unwrap_or_default();
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
    PreSyncOutcome {
        rc: status.unwrap_or(1),
        stdout,
        stderr,
    }
}

/// Run one user hook in its own session and retain the leader until every
/// descendant is gone. The CLI owns the signal handler; parallel hook threads
/// only observe its atomic result and perform teardown for their own session.
fn wait(command: Command) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;

    match crate::cleanup::supervise_session(command, None, |_| Ok(())).ok()? {
        crate::cleanup::SessionEnd::Exited(status) => Some(
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
        ),
        crate::cleanup::SessionEnd::Interrupted(signal) => Some(128 + signal),
        crate::cleanup::SessionEnd::TimedOut | crate::cleanup::SessionEnd::CleanupIncomplete => {
            None
        }
    }
}

fn failed_pre_sync(stderr: Vec<u8>) -> PreSyncOutcome {
    PreSyncOutcome {
        rc: 1,
        stdout: Vec::new(),
        stderr,
    }
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

    use super::{UpdateEnvironment, Worker, wait};
    use crate::app::Runtime;
    use crate::log::Log;
    use crate::profile_lifecycle;
    use dot_test_support::TempDir;

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
                dot_test_support::bash().as_os_str().to_owned(),
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
        let manifest = runtime.state_home().join("dot/overlay-links");
        let mut worker = Worker::for_update(
            runtime,
            &UpdateEnvironment {
                extensions_dir: "",
                overlay_manifest: manifest.to_str().expect("manifest text"),
                update_lock_token: None,
                quiet: false,
                force: false,
                verbose: false,
            },
        );
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        let tmpdir = home.join("scratch");
        std::fs::create_dir(&tmpdir).expect("scratch directory");
        let inputs = profile_lifecycle::RunInputs {
            record,
            home: home.to_str().expect("home text"),
            euid: crate::temp::current_uid().expect("current uid"),
            tmpdir: &tmpdir,
            verbose: true,
            log: &log,
        };
        let rc = profile_lifecycle::run_one(&inputs, &mut worker, &mut out, &mut warnings);
        (rc, out, warnings)
    }

    #[test]
    fn cancelled_worker_returns_the_parent_signal_status() {
        const HELPER: &str = "DOT_HOOK_SIGNAL_STATUS_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hook_worker::tests::cancelled_worker_returns_the_parent_signal_status",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "hook-signal helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let scope = TempDir::new("hook-worker-signal-status").expect("scope");
        let ready = scope.path().join("ready");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "trap 'exit 0' TERM; : >\"$1\"; while :; do sleep 1; done",
                "hook-signal",
            ])
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let signals = crate::cleanup::Signals::install().unwrap();
        let sender = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(ready.exists(), "hook signal fixture did not start");
            // SAFETY: getpid returns this live helper and SIGHUP is handled.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGHUP) }, 0);
        });
        let status = wait(command);
        sender.join().unwrap();
        assert_eq!(status, Some(128 + libc::SIGHUP));
        let later = scope.path().join("later");
        let mut later_command = Command::new(dot_test_support::bash());
        later_command
            .args(["-c", ": >\"$1\"", "later-hook"])
            .arg(&later)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        assert_eq!(wait(later_command), Some(128 + libc::SIGHUP));
        assert!(!later.exists(), "a hook started after cancellation");
        assert_eq!(signals.finish(0), 128 + libc::SIGHUP);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn signal_during_completed_worker_teardown_owns_status() {
        const HELPER: &str = "DOT_HOOK_TEARDOWN_SIGNAL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hook_worker::tests::signal_during_completed_worker_teardown_owns_status",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "hook-teardown helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let scope = TempDir::new("hook-worker-teardown-signal").expect("scope");
        let marker = scope.path().join("descendant");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "set -m; (trap 'kill -HUP \"$DOT_PARENT_PID\"; exit 0' TERM; echo $BASHPID >\"$1\"; while :; do sleep 1; done) </dev/null >/dev/null 2>&1 & until [[ -s $1 ]]; do sleep 0.01; done",
                "hook-teardown-signal",
            ])
            .arg(&marker)
            .env("DOT_PARENT_PID", std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let signals = crate::cleanup::Signals::install().unwrap();
        assert_eq!(wait(command), Some(128 + libc::SIGHUP));
        assert_eq!(signals.finish(0), 128 + libc::SIGHUP);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn completed_worker_stops_escaped_session_descendants() {
        const HELPER: &str = "DOT_HOOK_DESCENDANT_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hook_worker::tests::completed_worker_stops_escaped_session_descendants",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "hook-descendant helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let scope = TempDir::new("hook-worker-descendant").expect("scope");
        let marker = scope.path().join("descendant");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "set -m; (trap '' TERM; echo $BASHPID >\"$1\"; sleep 4) </dev/null >/dev/null 2>&1 & until [[ -s $1 ]]; do sleep 0.01; done",
                "hook-descendant",
            ])
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = wait(command);
        let pid = std::fs::read_to_string(&marker)
            .expect("descendant marker")
            .trim()
            .parse::<i32>()
            .expect("descendant pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_live(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let survived = process_live(pid);
        assert_eq!(status, Some(0));
        assert!(!survived, "completed hook left its descendant running");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn completed_worker_with_unpinned_descendant_fails_closed() {
        let scope = TempDir::new("hook-worker-unpinned-descendant").expect("scope");
        let marker = scope.path().join("descendant");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "set -m; (trap '' TERM; echo $BASHPID >\"$1\"; sleep 3) </dev/null >/dev/null 2>&1 & until [[ -s $1 ]]; do sleep 0.01; done",
                "hook-descendant",
            ])
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = std::time::Instant::now();
        let status = wait(command);
        let pid = std::fs::read_to_string(&marker)
            .expect("descendant marker")
            .trim()
            .parse::<i32>()
            .expect("descendant pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_live(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert_eq!(status, None, "unsafe cleanup was reported as success");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(6),
            "incomplete cleanup was not bounded"
        );
        assert!(!process_live(pid), "self-bounded descendant survived");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn process_live(pid: i32) -> bool {
        match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let end = stat
                    .windows(2)
                    .rposition(|part| part == b") ")
                    .expect("well-formed proc stat");
                stat.get(end + 2) != Some(&b'Z')
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("could not inspect process {pid}: {error}"),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn process_live(pid: i32) -> bool {
        let output = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap_or_else(|error| panic!("could not inspect process {pid}: {error}"));
        if output.status.success() {
            return !output.stdout.is_empty()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z');
        }
        // SAFETY: a positive PID and signal zero only test existence.
        if unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return false;
        }
        panic!(
            "ps could not inspect live process {pid}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
    fn worker_uses_strict_dot_bash_for_probe_and_execution() {
        let scope = TempDir::new_exec("hook-worker-dot-bash").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b":\n").expect("BASH_ENV");
        let marker = scope.path().join("bash-invocations");
        let wrapper = scope.path().join("bash-wrapper");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf x >>'{}'\nexec '{}' \"$@\"\n",
                marker.display(),
                dot_test_support::bash().display()
            ),
        )
        .expect("Bash wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("Bash wrapper mode");
        let record = checkout(&home, b"deactivate() { :; }\n");
        let mut env = runtime(&home, &state, &poison).env().clone();
        env.insert(OsString::from("DOT_BASH"), wrapper.as_os_str().to_owned());
        let runtime = Runtime::from_env(&env, &home).expect("runtime");

        let (rc, out, warnings) = run(&runtime, &home, &record);

        assert_eq!(rc, 0);
        assert!(out.is_empty());
        assert!(warnings.is_empty());
        assert_eq!(std::fs::read(marker).expect("Bash marker"), b"xx");
    }

    #[test]
    fn worker_reports_invalid_strict_dot_bash() {
        let scope = TempDir::new_exec("hook-worker-invalid-dot-bash").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b":\n").expect("BASH_ENV");
        let missing = scope.path().join("missing/bash");
        let record = checkout(&home, b"deactivate() { :; }\n");
        let mut env = runtime(&home, &state, &poison).env().clone();
        env.insert(OsString::from("DOT_BASH"), missing.as_os_str().to_owned());
        let runtime = Runtime::from_env(&env, &home).expect("runtime");

        let (rc, out, warnings) = run(&runtime, &home, &record);

        assert_eq!(rc, 1);
        assert!(out.is_empty());
        assert_eq!(
            warnings,
            format!(
                "checkout Bash resolver: explicit interpreter is not Bash 4 or newer: {}\n",
                missing.display()
            )
            .into_bytes()
        );
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
    fn worker_treats_relative_ambient_bash_as_a_soft_hint() {
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

        let bash = runtime.bash().expect("fallback Bash");
        assert!(bash.path().is_absolute());
        assert_ne!(bash.path(), Path::new("bash"));
    }

    #[test]
    fn worker_resolves_bash_from_the_snapshotted_path_when_unset() {
        let scope = TempDir::new("hook-worker-path-bash").expect("fixture directory");
        let home = scope.path().join("home");
        let state = scope.path().join("state");
        std::fs::create_dir(&home).expect("home directory");
        std::fs::create_dir(&state).expect("state directory");
        let poison = scope.path().join("bash-env");
        std::fs::write(&poison, b":\n").expect("BASH_ENV");
        let mut env = runtime(&home, &state, &poison).env().clone();
        env.remove(std::ffi::OsStr::new("BASH"));
        let runtime = Runtime::from_env(&env, &home).expect("runtime");

        let bash = runtime.bash().expect("PATH bash");
        assert!(bash.path().is_absolute());
    }
}
