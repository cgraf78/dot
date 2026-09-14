//! Native coordinator for the external Shdeps provider.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::Runtime;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

const SIGKILL: i32 = 9;

/// Inputs retained across provider selection, bootstrap, ABI validation, and update.
pub(crate) struct Inputs<'a> {
    pub(crate) runtime: &'a Runtime,
    pub(crate) source_root: &'a Path,
    pub(crate) home: &'a str,
    pub(crate) config_home: &'a str,
    pub(crate) state_home: &'a str,
    pub(crate) policy: &'a str,
    pub(crate) force: bool,
    pub(crate) quiet: bool,
    pub(crate) verbose: bool,
    pub(crate) update_jobs: Option<&'a str>,
    pub(crate) palette: &'a crate::progress_ui::Palette,
    pub(crate) multibyte: bool,
    pub(crate) ascii: bool,
    pub(crate) bar_width: &'a str,
}

/// Provider update output consumed by Dot's owning update stage.
pub(crate) struct Outcome {
    pub(crate) status: i32,
    pub(crate) stage_status: Vec<u8>,
    pub(crate) summary: Vec<u8>,
    pub(crate) during: Vec<u8>,
    pub(crate) details: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) revision_change: Option<(String, String)>,
}

#[derive(Clone, Copy)]
enum Source {
    Explicit,
    PinnedDevelopment,
    LatestDevelopment,
    Managed,
    Downloaded,
}

struct Installer {
    path: PathBuf,
    source: Source,
    temporary: bool,
}

struct Ready {
    binary: PathBuf,
    directory: PathBuf,
    env: BTreeMap<OsString, OsString>,
}

struct PromptPipe {
    directory: PathBuf,
    path: PathBuf,
    file: std::fs::File,
}

impl Drop for PromptPipe {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Select, bootstrap, validate, and run Shdeps without invoking Dot's shell engine.
pub(crate) fn update(
    inputs: &Inputs<'_>,
    stage: &mut crate::progress_ui::Stage,
    now_secs: i64,
) -> Outcome {
    match ensure(inputs) {
        Ok(ready) => run_update(inputs, &ready, stage, now_secs),
        Err(failure) => Outcome {
            status: 1,
            stage_status: b"failed".to_vec(),
            summary: b"shdeps unavailable; dependency install skipped".to_vec(),
            during: Vec::new(),
            details: Vec::new(),
            stderr: match failure {
                EnsureFailure::Unavailable => Vec::new(),
                EnsureFailure::AbiTimeout(seconds) => crate::progress_ui::warn_line(
                    inputs.palette,
                    format!("  warning: Shdeps provider ABI probe timed out after {seconds}s")
                        .as_bytes(),
                ),
                EnsureFailure::Download(kind) => {
                    let mut out = crate::progress_ui::warn_line(
                        inputs.palette,
                        match kind {
                            DownloadFailure::Transport => {
                                b"  warning: Shdeps bootstrap download failed".as_slice()
                            }
                            DownloadFailure::Digest => b"  warning: downloaded Shdeps bootstrap did not match the release digest".as_slice(),
                        },
                    );
                    out.extend_from_slice(&crate::progress_ui::warn_line(
                        inputs.palette,
                        b"  warning: failed to fetch the reviewed Shdeps bootstrap",
                    ));
                    out
                }
            },
            revision_change: None,
        },
    }
}

enum EnsureFailure {
    Unavailable,
    AbiTimeout(u64),
    Download(DownloadFailure),
}

fn ensure(inputs: &Inputs<'_>) -> Result<Ready, EnsureFailure> {
    let configured =
        crate::shdeps_env_abi::configure_env(&crate::shdeps_env_abi::ConfigureInputs {
            xdg_config_home: inputs.config_home,
            home: inputs.home,
            install_dir: value(inputs.runtime, "SHDEPS_INSTALL_DIR"),
            bin_dir: value(inputs.runtime, "SHDEPS_BIN_DIR"),
            git_dev_dir: value(inputs.runtime, "SHDEPS_GIT_DEV_DIR"),
            dot_force: if inputs.force { "1" } else { "0" },
            dot_quiet: if inputs.quiet { "1" } else { "0" },
        })
        .ok_or(EnsureFailure::Unavailable)?;
    let selected = match installer(inputs, &configured) {
        Some(selected) => selected,
        None => download_installer(inputs).map_err(EnsureFailure::Download)?,
    };
    let mut env = inputs.runtime.env().clone();
    set(&mut env, "HOME", inputs.home);
    set(&mut env, "XDG_CONFIG_HOME", inputs.config_home);
    set(&mut env, "XDG_STATE_HOME", inputs.state_home);
    set(&mut env, "SHDEPS_CONF_DIR", &configured.conf_dir);
    set(&mut env, "SHDEPS_HOOKS_DIR", &configured.hooks_dir);
    set(&mut env, "SHDEPS_INSTALL_DIR", &configured.install_dir);
    set(&mut env, "SHDEPS_BIN_DIR", &configured.bin_dir);
    set(&mut env, "SHDEPS_GIT_DEV_DIR", &configured.git_dev_dir);
    if configured.force {
        set(&mut env, "SHDEPS_FORCE", "1");
    }
    if configured.quiet {
        set(&mut env, "SHDEPS_QUIET", "1");
    }
    if inputs.policy == "latest" {
        set(&mut env, "SHDEPS_BOOTSTRAP_FORCE", "1");
    }
    if !matches!(
        selected.source,
        Source::PinnedDevelopment | Source::LatestDevelopment
    ) {
        set(&mut env, "SHDEPS_GIT_DEV_DIR", "/dev/null");
    }
    let bootstrapped = bootstrap(inputs.runtime, &selected.path, &env);
    if selected.temporary {
        let _ = std::fs::remove_file(&selected.path);
    }
    let binary = bootstrapped.map_err(|_| EnsureFailure::Unavailable)?;
    let directory = binary
        .parent()
        .ok_or(EnsureFailure::Unavailable)?
        .to_path_buf();
    let expected =
        crate::shdeps::lock_value(inputs.source_root, "abi").ok_or(EnsureFailure::Unavailable)?;
    match binary_abi(inputs.runtime, &binary, &expected, &env) {
        AbiResult::Match => {}
        AbiResult::Mismatch => return Err(EnsureFailure::Unavailable),
        AbiResult::Timeout(seconds) => return Err(EnsureFailure::AbiTimeout(seconds)),
    }
    Ok(Ready {
        binary,
        directory,
        env,
    })
}

fn installer(
    inputs: &Inputs<'_>,
    configured: &crate::shdeps_env_abi::ConfiguredEnv,
) -> Option<Installer> {
    if let Some(lib) = inputs
        .runtime
        .value("SHDEPS_LIB")
        .filter(|value| !value.is_empty())
    {
        let candidate = Path::new(lib).parent()?.join("install.sh");
        if candidate.is_file()
            && crate::shdeps::installer_hash_matches(inputs.source_root, &candidate)
        {
            return Some(Installer {
                path: candidate,
                source: Source::Explicit,
                temporary: false,
            });
        }
    }
    let development = Path::new(&configured.git_dev_dir).join("shdeps");
    let development_installer = development.join("install.sh");
    let expected = crate::shdeps::lock_value(inputs.source_root, "revision")?;
    if development_installer.is_file()
        && development.join("shdeps.sh").is_file()
        && crate::shdeps::active_revision(&development) == expected
        && crate::shdeps::installer_hash_matches(inputs.source_root, &development_installer)
    {
        return Some(Installer {
            path: development_installer,
            source: Source::PinnedDevelopment,
            temporary: false,
        });
    }
    if inputs.policy == "latest" && development_checkout_valid(&development) {
        return Some(Installer {
            path: development_installer,
            source: Source::LatestDevelopment,
            temporary: false,
        });
    }
    let installed = inputs
        .runtime
        .value("SHDEPS_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(inputs.home).join(".local/share/shdeps"));
    let managed = installed.join("install.sh");
    if managed.is_file()
        && installed.join("shdeps.sh").is_file()
        && crate::shdeps::installer_hash_matches(inputs.source_root, &managed)
    {
        return Some(Installer {
            path: managed,
            source: Source::Managed,
            temporary: false,
        });
    }
    None
}

#[derive(Clone, Copy)]
enum DownloadFailure {
    Transport,
    Digest,
}

fn download_installer(inputs: &Inputs<'_>) -> Result<Installer, DownloadFailure> {
    let revision = crate::shdeps::lock_value(inputs.source_root, "revision")
        .ok_or(DownloadFailure::Transport)?;
    let path = inputs
        .runtime
        .value("PATH")
        .ok_or(DownloadFailure::Transport)?;
    let curl = resolve_path(inputs.runtime, path, "curl").ok_or(DownloadFailure::Transport)?;
    let tmp = inputs
        .runtime
        .value("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut temporary = None;
    for nonce in 0..128u32 {
        let candidate = tmp.join(format!(".dot-shdeps-{}-{nonce}", std::process::id()));
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate);
        if opened.is_ok() {
            temporary = Some(candidate);
            break;
        }
    }
    let temporary = temporary.ok_or(DownloadFailure::Transport)?;
    let url = format!("https://raw.githubusercontent.com/cgraf78/shdeps/{revision}/install.sh");
    let mut succeeded = false;
    for attempt in 0..3 {
        let status = Command::new(&curl)
            .args([
                "--connect-timeout",
                "10",
                "--max-time",
                "30",
                "--speed-limit",
                "1024",
                "--speed-time",
                "15",
                "-fsSL",
            ])
            .arg(&url)
            .arg("-o")
            .arg(&temporary)
            .env_clear()
            .envs(inputs.runtime.env())
            .current_dir(inputs.runtime.cwd())
            .stdin(Stdio::null())
            .status();
        if status.is_ok_and(|status| status.success()) {
            succeeded = true;
            break;
        }
        if attempt < 2 {
            let delay = value(inputs.runtime, "_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS")
                .parse::<u64>()
                .unwrap_or(1);
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
    }
    if !succeeded {
        let _ = std::fs::remove_file(&temporary);
        return Err(DownloadFailure::Transport);
    }
    if !crate::shdeps::installer_hash_matches(inputs.source_root, &temporary) {
        let _ = std::fs::remove_file(&temporary);
        return Err(DownloadFailure::Digest);
    }
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| DownloadFailure::Transport)?;
    Ok(Installer {
        path: temporary,
        source: Source::Downloaded,
        temporary: true,
    })
}

fn bootstrap(
    runtime: &Runtime,
    installer: &Path,
    env: &BTreeMap<OsString, OsString>,
) -> Result<PathBuf, ()> {
    let bash = runtime.value("BASH").map(PathBuf::from).ok_or(())?;
    if !bash.is_absolute() || !executable(&bash) {
        return Err(());
    }
    // The reviewed installer is the authority that selects the CLI. Keep its
    // shell-local `_SHDEPSW_BIN` across the process boundary with a strict,
    // NUL-framed protocol; installer stdout is intentionally not protocol.
    let output = Command::new(bash)
        .args([
            "--noprofile",
            "--norc",
            "-c",
            ". \"$1\" --bootstrap >/dev/null || exit; [[ -n ${_SHDEPSW_BIN:-} ]] || exit 1; printf 'dot-shdeps-bootstrap-v1\\0%s\\0' \"$_SHDEPSW_BIN\"",
        ])
        .arg("dot-shdeps-bootstrap")
        .arg(installer)
        .env_clear()
        .envs(env)
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    let prefix = b"dot-shdeps-bootstrap-v1\0";
    let path = output.stdout.strip_prefix(prefix).ok_or(())?;
    let path = path.strip_suffix(b"\0").ok_or(())?;
    if path.is_empty() || path.contains(&0) {
        return Err(());
    }
    let path = PathBuf::from(OsStr::from_bytes(path));
    if !path.is_absolute() || !executable(&path) {
        return Err(());
    }
    Ok(path)
}

fn binary_abi(
    runtime: &Runtime,
    binary: &Path,
    expected: &str,
    env: &BTreeMap<OsString, OsString>,
) -> AbiResult {
    let timeout = value(runtime, "_DOT_SHDEPS_ABI_TIMEOUT_SECONDS")
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .unwrap_or(10);
    let mut command = Command::new(binary);
    command
        .args(["__api", "version"])
        .env_clear()
        .envs(env)
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return AbiResult::Mismatch,
    };
    let Some(mut stdout) = child.stdout.take() else {
        kill_group(child.id());
        let _ = child.wait();
        return AbiResult::Mismatch;
    };
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_group(child.id());
                let bytes = reader.join().unwrap_or_default();
                if status.success()
                    && String::from_utf8_lossy(&bytes).trim_end_matches('\n')
                        == format!("abi:{expected}")
                {
                    return AbiResult::Match;
                }
                return AbiResult::Mismatch;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(None) => {
                kill_group(child.id());
                let _ = child.wait();
                let _ = reader.join();
                return AbiResult::Timeout(timeout);
            }
            Err(_) => {
                kill_group(child.id());
                let _ = child.wait();
                let _ = reader.join();
                return AbiResult::Mismatch;
            }
        }
    }
}

fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: a negative, nonzero pid addresses only the process group created
    // by `CommandExt::process_group(0)` for this owned child. SIGKILL is the
    // POSIX value 9 on every supported Unix target. Delivery failure is normal
    // when the group has already exited, so it has no additional error path.
    let _ = unsafe { kill(-pid, SIGKILL) };
}

enum AbiResult {
    Match,
    Mismatch,
    Timeout(u64),
}

fn run_update(
    inputs: &Inputs<'_>,
    ready: &Ready,
    stage: &mut crate::progress_ui::Stage,
    now_secs: i64,
) -> Outcome {
    let before = crate::shdeps::active_revision(inputs.source_root);
    let mut env = ready.env.clone();
    set(&mut env, "SHDEPS_DIR", ready.directory.as_os_str());
    let path = env.get(OsStr::new("PATH")).cloned().unwrap_or_default();
    let mut provider_path = env
        .get(OsStr::new("SHDEPS_BIN_DIR"))
        .cloned()
        .unwrap_or_default();
    provider_path.push(OsStr::new(":"));
    provider_path.push(path);
    set(&mut env, "PATH", provider_path);
    set(&mut env, "SHDEPS_NESTED", "1");
    set(&mut env, "SHDEPS_PROGRESS", "jsonl");
    let mut prompt = prompt_pipe(inputs.runtime);
    if let Some(pipe) = &prompt {
        set(&mut env, "SHDEPS_PROGRESS_PROMPT_ACK", &pipe.path);
    }
    if inputs.runtime.value("SHDEPS_JOBS").is_none() {
        let jobs = crate::merges::update_jobs(inputs.update_jobs.unwrap_or_default());
        set(&mut env, "SHDEPS_JOBS", jobs);
    }
    if inputs
        .runtime
        .value("DOT_SHDEPS_ALLOW_GH_AUTH_TOKEN")
        .and_then(OsStr::to_str)
        == Some("1")
        && inputs.runtime.value("SHDEPS_ALLOW_GH_AUTH_TOKEN").is_none()
    {
        set(&mut env, "SHDEPS_ALLOW_GH_AUTH_TOKEN", "1");
    }
    let child = Command::new(&ready.binary)
        .arg("update")
        .env_clear()
        .envs(&env)
        .current_dir(&ready.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let Ok(mut child) = child else {
        return failed_update();
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return failed_update();
    };
    let stderr = child.stderr.take();
    let stderr = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut bytes);
        }
        bytes
    });
    let mut state = crate::shdeps_ui::State::new();
    let mut session = crate::shdeps_ui_render::reset(false);
    let mut during = Vec::new();
    let mut live = false;
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if sender.send(line).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    let status = loop {
        if let Ok(line) = receiver.recv_timeout(std::time::Duration::from_millis(20)) {
            provider_line(
                line,
                &mut state,
                &mut session,
                stage,
                inputs,
                now_secs,
                &mut during,
                &mut live,
                prompt.as_mut().map(|pipe| &mut pipe.file),
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // A provider may leave grandchildren holding stdout. Once the
                // owned leader exits, terminate its process group so output
                // supervision and cleanup cannot wait on an unrelated lifetime.
                kill_group(child.id());
                break status.code().unwrap_or(1);
            }
            Ok(None) => {}
            Err(_) => {
                kill_group(child.id());
                let _ = child.wait();
                break 1;
            }
        }
    };
    while let Ok(line) = receiver.recv_timeout(std::time::Duration::from_millis(20)) {
        provider_line(
            line,
            &mut state,
            &mut session,
            stage,
            inputs,
            now_secs,
            &mut during,
            &mut live,
            prompt.as_mut().map(|pipe| &mut pipe.file),
        );
    }
    join_and_drain(reader, receiver, |line| {
        provider_line(
            line,
            &mut state,
            &mut session,
            stage,
            inputs,
            now_secs,
            &mut during,
            &mut live,
            prompt.as_mut().map(|pipe| &mut pipe.file),
        );
    });
    let stderr = stderr.join().unwrap_or_default();
    let ui = crate::shdeps_ui_render::Ui {
        palette: inputs.palette,
        quiet: inputs.quiet,
        multibyte: inputs.multibyte,
    };
    let (verbose_rows, live) = crate::shdeps_ui_render::print_verbose_items(
        &ui,
        false,
        inputs.verbose,
        state.order(),
        state.items(),
        state.labels(),
        &crate::shdeps_ui::group_label,
    );
    during.extend_from_slice(&verbose_rows);
    let threshold = inputs
        .runtime
        .value("DOT_UPDATE_SUBPHASE_THRESHOLD_MS")
        .map(OsStr::as_encoded_bytes);
    let (details, _) = crate::shdeps_ui_render::print_group_summaries(
        &ui,
        live,
        inputs.verbose,
        threshold,
        state.order(),
        state.summaries(),
        state.items(),
    );
    let after = crate::shdeps::active_revision(inputs.source_root);
    let revision_change = (status == 0 && before != after).then_some((before, after));
    if status == 0 {
        Outcome {
            status,
            stage_status: session.status,
            summary: session.summary,
            during,
            details,
            stderr,
            revision_change,
        }
    } else {
        Outcome {
            status,
            stage_status: b"failed".to_vec(),
            summary: if session.summary == b"dependencies checked" {
                b"dependency update failed".to_vec()
            } else {
                session.summary
            },
            during,
            details,
            stderr,
            revision_change: None,
        }
    }
}

fn join_and_drain<T>(
    reader: std::thread::JoinHandle<()>,
    receiver: std::sync::mpsc::Receiver<T>,
    mut consume: impl FnMut(T),
) {
    let _ = reader.join();
    for item in receiver {
        consume(item);
    }
}

#[allow(clippy::too_many_arguments)]
fn provider_line(
    mut line: Vec<u8>,
    state: &mut crate::shdeps_ui::State,
    session: &mut crate::shdeps_ui_render::Session,
    stage: &mut crate::progress_ui::Stage,
    inputs: &Inputs<'_>,
    now_secs: i64,
    during: &mut Vec<u8>,
    live: &mut bool,
    prompt: Option<&mut std::fs::File>,
) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    handle_event(
        &line, state, session, stage, inputs, now_secs, during, live, prompt,
    );
}

fn failed_update() -> Outcome {
    Outcome {
        status: 1,
        stage_status: b"failed".to_vec(),
        summary: b"dependency update failed".to_vec(),
        during: Vec::new(),
        details: Vec::new(),
        stderr: Vec::new(),
        revision_change: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    line: &[u8],
    state: &mut crate::shdeps_ui::State,
    session: &mut crate::shdeps_ui_render::Session,
    stage: &mut crate::progress_ui::Stage,
    inputs: &Inputs<'_>,
    now_secs: i64,
    during: &mut Vec<u8>,
    live: &mut bool,
    prompt: Option<&mut std::fs::File>,
) {
    let Some(fields) = parse_event(inputs.runtime, line) else {
        return;
    };
    match text(&fields, "event") {
        b"prompt" => {
            let (out, active, ack) = crate::shdeps_ui_render::prompt_pause(session, *live, "1");
            during.extend_from_slice(&out);
            *live = active;
            if let (Some(prompt), Some(ack)) = (prompt, ack) {
                let _ = prompt.write_all(&ack);
                let _ = prompt.flush();
            }
        }
        b"item" => {
            crate::shdeps_ui_render::prompt_resume(session);
            state.record_item(
                text(&fields, "group"),
                text(&fields, "status"),
                text(&fields, "name"),
                text(&fields, "detail"),
            );
        }
        b"group_summary" => {
            crate::shdeps_ui_render::prompt_resume(session);
            state.record_group_summary(
                text(&fields, "group"),
                text(&fields, "label"),
                text(&fields, "status"),
                number(&fields, "changed"),
                number(&fields, "current"),
                number(&fields, "skipped"),
                number(&fields, "failed"),
                text(&fields, "elapsed_ms"),
                number(&fields, "warnings"),
            );
        }
        b"summary" => {
            crate::shdeps_ui_render::prompt_resume(session);
            session.status = text(&fields, "status").to_vec();
            session.summary = crate::shdeps_ui::summary_text(
                number(&fields, "changed"),
                number(&fields, "current"),
                number(&fields, "skipped"),
                number(&fields, "failed"),
                number(&fields, "warnings"),
            );
        }
        b"phase" => {
            crate::shdeps_ui_render::prompt_resume(session);
            let done = number(&fields, "done");
            let total = number(&fields, "total");
            let detail = if total > 0 {
                crate::progress_ui::progress_detail_with_label(
                    text(&fields, "label"),
                    done,
                    total,
                    None,
                    "18",
                    inputs.bar_width,
                    inputs.ascii,
                    inputs.multibyte,
                )
            } else {
                text(&fields, "label").to_vec()
            };
            during.extend_from_slice(&stage.update(
                &detail,
                now_secs,
                inputs.verbose.then_some("1"),
            ));
        }
        b"warning" | b"detail" | b"hint" => {
            crate::shdeps_ui_render::prompt_resume(session);
            let event = text(&fields, "event");
            let status = match text(&fields, "status") {
                b"" => event,
                status => status,
            };
            during.extend_from_slice(&stage.note(status, text(&fields, "detail")));
        }
        _ => {}
    }
}

fn prompt_pipe(runtime: &Runtime) -> Option<PromptPipe> {
    let tmp = runtime
        .value("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mkfifo = resolve_path(runtime, runtime.value("PATH")?, "mkfifo")?;
    for nonce in 0..128u32 {
        let directory = tmp.join(format!(".dot-shdeps-ui-{}-{nonce}", std::process::id()));
        if std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .is_err()
        {
            continue;
        }
        let path = directory.join("prompt-ack");
        let status = Command::new(&mkfifo)
            .arg(&path)
            .env_clear()
            .envs(runtime.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok_and(|status| status.success()) {
            if let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                return Some(PromptPipe {
                    directory,
                    path,
                    file,
                });
            }
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&directory);
    }
    None
}

/// Parse JSONL exactly through the shell's available parser branch. The shell
/// deliberately retains raw escaped strings in bootstrap environments without
/// jq; rendering decoded Unicode there would make native output drift.
fn parse_event(runtime: &Runtime, line: &[u8]) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
    let jq = runtime
        .value("PATH")
        .is_some_and(|path| resolve_path(runtime, path, "jq").is_some());
    parse_object(line, jq)
}

fn parse_object(line: &[u8], decode_strings: bool) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut map = BTreeMap::new();
    let mut index = 0;
    skip_space(line, &mut index);
    if line.get(index)? != &b'{' {
        return None;
    }
    index += 1;
    loop {
        skip_space(line, &mut index);
        if line.get(index) == Some(&b'}') {
            return Some(map);
        }
        let key = json_string(line, &mut index)?;
        skip_space(line, &mut index);
        if line.get(index)? != &b':' {
            return None;
        }
        index += 1;
        skip_space(line, &mut index);
        let value = if line.get(index) == Some(&b'"') {
            if decode_strings {
                json_string(line, &mut index)?
            } else {
                json_string_raw(line, &mut index)?
            }
        } else {
            let start = index;
            while line.get(index).is_some_and(|b| *b != b',' && *b != b'}') {
                index += 1;
            }
            line[start..index]
                .iter()
                .copied()
                .take_while(|b| !b.is_ascii_whitespace())
                .collect()
        };
        map.insert(key, value);
        skip_space(line, &mut index);
        match line.get(index)? {
            b',' => index += 1,
            b'}' => return Some(map),
            _ => return None,
        }
    }
}

/// Match the shell's bootstrap `sed` capture while consuming one valid JSON
/// string. Its `[^\"]*` capture stops at the quote byte in `\"`, after keeping
/// the slash. We must still scan past that escaped quote to locate the real
/// JSON terminator and continue parsing later fields.
fn json_string_raw(input: &[u8], index: &mut usize) -> Option<Vec<u8>> {
    if input.get(*index)? != &b'"' {
        return None;
    }
    *index += 1;
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(byte) = input.get(*index).copied() {
        *index += 1;
        match byte {
            b'"' => return Some(out),
            b'\\' => {
                if !truncated {
                    out.push(byte);
                }
                let escaped = input.get(*index).copied()?;
                *index += 1;
                if escaped == b'"' {
                    truncated = true;
                } else if !truncated {
                    out.push(escaped);
                }
                if escaped == b'u' {
                    let digits = input.get(*index..*index + 4)?;
                    if !digits.iter().all(u8::is_ascii_hexdigit) {
                        return None;
                    }
                    if !truncated {
                        out.extend_from_slice(digits);
                    }
                    *index += 4;
                }
            }
            0..=31 => return None,
            _ if !truncated => out.push(byte),
            _ => {}
        }
    }
    None
}

fn json_string(input: &[u8], index: &mut usize) -> Option<Vec<u8>> {
    if input.get(*index)? != &b'"' {
        return None;
    }
    *index += 1;
    let mut out = Vec::new();
    while let Some(byte) = input.get(*index).copied() {
        *index += 1;
        match byte {
            b'"' => return Some(out),
            b'\\' => {
                let escaped = input.get(*index).copied()?;
                *index += 1;
                match escaped {
                    b'"' | b'\\' | b'/' => out.push(escaped),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let first = hex4(input, index)?;
                        let scalar = if (0xd800..=0xdbff).contains(&first) {
                            if input.get(*index..*index + 2)? != b"\\u" {
                                return None;
                            }
                            *index += 2;
                            let second = hex4(input, index)?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return None;
                            }
                            0x1_0000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
                        } else {
                            first as u32
                        };
                        let character = char::from_u32(scalar)?;
                        let mut encoded = [0; 4];
                        out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                    }
                    _ => return None,
                }
            }
            0..=31 => return None,
            _ => out.push(byte),
        }
    }
    None
}

fn hex4(input: &[u8], index: &mut usize) -> Option<u16> {
    let digits = input.get(*index..*index + 4)?;
    let mut value = 0u16;
    for digit in digits {
        value = value.checked_mul(16)?;
        value = value.checked_add(match digit {
            b'0'..=b'9' => u16::from(*digit - b'0'),
            b'a'..=b'f' => u16::from(*digit - b'a' + 10),
            b'A'..=b'F' => u16::from(*digit - b'A' + 10),
            _ => return None,
        })?;
    }
    *index += 4;
    Some(value)
}

fn skip_space(input: &[u8], index: &mut usize) {
    while input.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn text<'a>(fields: &'a BTreeMap<Vec<u8>, Vec<u8>>, key: &str) -> &'a [u8] {
    fields
        .get(key.as_bytes())
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn number(fields: &BTreeMap<Vec<u8>, Vec<u8>>, key: &str) -> i64 {
    std::str::from_utf8(text(fields, key))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn value<'a>(runtime: &'a Runtime, key: &str) -> &'a str {
    runtime
        .value(key)
        .and_then(OsStr::to_str)
        .unwrap_or_default()
}

fn set<K: AsRef<OsStr>, V: AsRef<OsStr>>(env: &mut BTreeMap<OsString, OsString>, key: K, value: V) {
    env.insert(key.as_ref().to_os_string(), value.as_ref().to_os_string());
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn resolve_path(runtime: &Runtime, raw: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(raw)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                runtime.cwd().to_path_buf()
            } else {
                dir
            }
        })
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
}

fn development_checkout_valid(checkout: &Path) -> bool {
    development_checkout(checkout).unwrap_or(false)
}

fn development_checkout(checkout: &Path) -> Option<bool> {
    let euid = crate::temp::current_uid().unwrap_or(u32::MAX);
    let checkout_meta = checkout.symlink_metadata().ok()?;
    if !checkout_meta.is_dir() || !owned(&checkout_meta, euid) {
        return Some(false);
    }
    for path in [checkout.join("install.sh"), checkout.join("shdeps.sh")] {
        let meta = path.symlink_metadata().ok()?;
        if !meta.is_file() || !owned(&meta, euid) {
            return Some(false);
        }
    }
    let dot_git = checkout.join(".git");
    let git_meta = dot_git.symlink_metadata().ok()?;
    if !(git_meta.is_dir() || git_meta.is_file()) || !owned(&git_meta, euid) {
        return Some(false);
    }
    let root = crate::temp::sanitized_git(checkout, &["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !root.status.success() {
        return Some(false);
    }
    let physical = checkout.canonicalize().ok()?;
    if Path::new(OsStr::from_bytes(
        root.stdout.strip_suffix(b"\n").unwrap_or(&root.stdout),
    ))
    .canonicalize()
    .ok()?
        != physical
    {
        return Some(false);
    }
    for args in [
        ["rev-parse", "--absolute-git-dir"].as_slice(),
        ["rev-parse", "--path-format=absolute", "--git-common-dir"].as_slice(),
    ] {
        let output = crate::temp::sanitized_git(checkout, args).output().ok()?;
        if !output.status.success() {
            return Some(false);
        }
        let path = Path::new(OsStr::from_bytes(
            output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
        ));
        let physical = path.canonicalize().ok()?;
        if !owned(&physical.symlink_metadata().ok()?, euid) {
            return Some(false);
        }
    }
    for args in [
        ["config", "--local", "--get-all", "remote.origin.url"].as_slice(),
        ["remote", "get-url", "--all", "origin"].as_slice(),
    ] {
        let origin = crate::temp::sanitized_git(checkout, args).output().ok()?;
        if !origin.status.success() {
            return Some(false);
        }
        let lines: Vec<&[u8]> = origin
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        if lines.len() != 1
            || !std::str::from_utf8(lines[0]).is_ok_and(crate::shdeps::origin_allowed)
        {
            return Some(false);
        }
    }
    Some(true)
}

fn owned(metadata: &std::fs::Metadata, euid: u32) -> bool {
    !metadata.file_type().is_symlink() && metadata.uid() == euid && metadata.mode() & 0o022 == 0
}

#[cfg(test)]
mod tests {
    use super::join_and_drain;

    #[test]
    fn reader_join_precedes_final_receiver_drain() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            sender.send(b"final-event".to_vec()).expect("late send");
        });
        let mut events = Vec::new();
        join_and_drain(reader, receiver, |event| events.push(event));
        assert_eq!(events, [b"final-event".to_vec()]);
    }
}
