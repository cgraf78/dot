//! Runtime-owned launcher for the versioned extension worker.

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
}

impl Worker {
    /// Bind this worker to one immutable invocation runtime.
    pub(crate) fn new(runtime: &Runtime) -> Self {
        Self {
            runtime: runtime.clone(),
        }
    }
}

/// Resolve a usable Bash executable exclusively from the Runtime snapshot.
/// Shell launchers normally supply their own absolute `$BASH`; the binary has
/// no such shell variable, so searching its snapshotted PATH is the matching
/// explicit authority. Relative PATH entries resolve under the captured cwd,
/// never the parent process's current directory.
fn bash_path(runtime: &Runtime) -> Option<PathBuf> {
    if let Some(value) = runtime.value("BASH") {
        let path = PathBuf::from(value);
        if executable(&path) {
            return Some(path);
        }
    }
    let path = runtime.value("PATH")?.to_str()?;
    for entry in path.split(':').filter(|entry| !entry.is_empty()) {
        let entry = Path::new(entry);
        let entry = if entry.is_absolute() {
            entry.to_path_buf()
        } else {
            runtime.cwd().join(entry)
        };
        let candidate = entry.join("bash");
        if executable(&candidate) {
            return Some(candidate);
        }
    }
    None
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

/// Append stderr after stdout, the portable approximation of the shell
/// launcher’s `2>&1` capture. The worker's protocol emits its own structured
/// result in `result_file`; lifecycle text itself is line-oriented and tests
/// cover both channels.
fn combined(output: std::process::Output) -> WorkerOutcome {
    let mut bytes = output.stdout;
    bytes.extend_from_slice(&output.stderr);
    WorkerOutcome {
        rc: output.status.code().unwrap_or(1),
        output: bytes,
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
        let source_root = self.runtime.source_root();
        let source_root_text = match source_root.to_str() {
            Some(text) => text,
            None => {
                return WorkerOutcome {
                    rc: 1,
                    output: Vec::new(),
                };
            }
        };
        let result_text = match result_file.to_str() {
            Some(text) => text,
            None => {
                return WorkerOutcome {
                    rc: 1,
                    output: Vec::new(),
                };
            }
        };
        if crate::extension_worker::main_precheck(5, "deactivate", source_root_text, result_text)
            .is_err()
            || !crate::extension_worker::deactivate_set_valid("retiring", 1)
        {
            return WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            };
        }
        let Some(bash) = bash_path(&self.runtime) else {
            return WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            };
        };
        let (Some(cache), Some(data)) = (
            xdg_home(&self.runtime, "XDG_CACHE_HOME", ".cache"),
            xdg_home(&self.runtime, "XDG_DATA_HOME", ".local/share"),
        ) else {
            return WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            };
        };
        let worker = source_root.join("lib/dot/extension-worker.sh");
        let mut command = Command::new(bash);
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg(worker)
            .arg("deactivate")
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in STARTUP_CONTROLS {
            command.env_remove(key);
        }
        command.env_remove("SHELLOPTS");
        match command.output() {
            Ok(output) => combined(output),
            Err(_) => WorkerOutcome {
                rc: 1,
                output: Vec::new(),
            },
        }
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
}
