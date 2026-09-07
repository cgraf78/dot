//! Native `dot doctor` application coordinator.
//!
//! The check modules own individual health decisions. This module owns the
//! production boundary: capture one immutable Runtime, tolerate inspect-mode
//! overlay discovery failure, build the typed check inputs, authorize doctor
//! extensions, execute them through the versioned worker, and render every
//! result through one recorder.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::doctor_checks::{
    BaseRepoInputs, LifecycleInputs, MergeInputs, OverlayInputs, ProviderInputs, ProviderInstaller,
};
use crate::doctor_orchestrator::{EngineSnapshot, Recorder, RuntimeSnapshot};

/// Run all core and configured extension health checks.
pub fn run(runtime: &crate::app::Runtime, streams: &mut crate::app::Streams<'_>) -> i32 {
    let config = match crate::startup::check(runtime) {
        Ok(config) => config,
        Err(failure) => {
            let _ = writeln!(streams.stderr, "{}", failure.line());
            return failure.code();
        }
    };
    run_configured(runtime, &config, streams)
}

fn run_configured(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    streams: &mut crate::app::Streams<'_>,
) -> i32 {
    let stdout_terminal = streams.stdout_is_terminal();
    let home = text(runtime.value("HOME"));
    let source = runtime.source_root().to_string_lossy().into_owned();
    let state = runtime.state_home().to_string_lossy().into_owned();
    let constants = match crate::constants::resolve(
        &home,
        &state,
        &source,
        runtime.value("DOT_QUIET").and_then(OsStr::to_str),
        runtime.value("DOT_VERBOSE").and_then(OsStr::to_str),
    ) {
        Ok(constants) => constants,
        Err(_) => return 1,
    };
    let Some(euid) = current_uid(runtime) else {
        return 1;
    };
    let base = match crate::repos_base::select(runtime, &home, &state, streams.stderr) {
        Ok(base) => base,
        Err(()) => return 1,
    };
    let resolution = resolve(runtime, config, &home, euid, streams.stderr);
    let (overlays, profiles) = resolution;
    let renderer = renderer(runtime, stdout_terminal);
    if crate::ui::title(streams.stdout, &renderer, "dot doctor").is_err() {
        return 1;
    }
    let mut recorder = Recorder::new();
    let runtime_snapshot = runtime_snapshot(runtime, &source);
    let engine = engine_snapshot(runtime, &source, &home);
    crate::doctor_orchestrator::check_runtime(
        &mut recorder,
        &runtime_snapshot,
        &engine,
        home.as_bytes(),
    );

    let topology = topology_name(base.topology);
    let git_dir = base.client_git_dir.as_str();
    let marker = Path::new(&state).join("dot/init/completed");
    append(
        &mut recorder,
        crate::doctor_checks::check_base_repo(&BaseRepoInputs {
            topology,
            client_git_dir: git_dir,
            home: &home,
            is_client_checkout: crate::doctor_checks::is_client_checkout(
                Path::new(&home),
                Some(&marker),
            ),
        }),
    );
    append(
        &mut recorder,
        crate::doctor_checks::check_update_lock(Some(&crate::update_lock::lock_path(
            runtime.state_home(),
        ))),
    );
    append(
        &mut recorder,
        provider_records(runtime, config, &home, &source),
    );

    let mut lifecycle_records = Vec::new();
    let log = crate::log::Log::from_env(
        stdout_terminal,
        runtime.value("NO_COLOR").and_then(OsStr::to_str),
        runtime.value("DOT_QUIET").and_then(OsStr::to_str),
    );
    let lifecycle_ok = crate::profile_lifecycle::load(
        Some(Path::new(&constants.profile_lifecycle_ledger)),
        &home,
        euid,
        &log,
        streams.stderr,
        &mut lifecycle_records,
    );
    let extensions_enabled = crate::config::extensions_enabled(config);
    let local_overlays = overlays.overlays.clone();
    let local_validate =
        |path: &str| crate::overlays::source_validate(path, &local_overlays, &home);
    let deactivation =
        |record: &str| crate::profile_lifecycle::deactivation_script(record, &home, euid).is_ok();
    append(
        &mut recorder,
        crate::doctor_checks::check_overlays(&OverlayInputs {
            home: &home,
            profile_config_error: profiles.config_error.as_deref(),
            profiles_present: profiles.present,
            profile_user: Some(&profiles.current_user),
            profile_host: Some(&profiles.current_host),
            selected_profile: Some(&profiles.selected),
            selection_state: Some(&profiles.selection_state),
            included_profiles: profiles.included.clone(),
            phase_one: overlays.phase_one_selected.clone(),
            selectors: profiles.selector_records.clone(),
            lifecycle: LifecycleInputs {
                profiles_present: profiles.present,
                load_ok: lifecycle_ok,
                eligible: overlays.eligible_names.clone(),
                active: overlays.active.clone(),
                records: lifecycle_records,
                extensions_enabled,
                deactivation_ok: &deactivation,
            },
            configured_count: overlays.configured.len(),
            manifest: constants.overlay_manifest.clone(),
            discovery_error: overlays.discovery_error.as_deref(),
            active_records: overlays.active.clone(),
            overlay_lifecycle: overlays.lifecycle.clone(),
            local_validate: &local_validate,
        }),
    );
    let merge_specs = extensions_enabled.then(|| {
        merge_count(
            config,
            &home,
            &overlays.active,
            &constants.overlay_manifest,
            euid,
            streams.stderr,
        )
    });
    append(
        &mut recorder,
        crate::doctor_checks::check_merges(&MergeInputs {
            enabled: extensions_enabled,
            extensions_dir: config.extensions_dir.clone().unwrap_or_default(),
            spec_count: merge_specs.flatten(),
        }),
    );

    let extension_status = extensions(
        runtime,
        config,
        euid,
        &overlays.active,
        &constants.overlay_manifest,
        streams.stderr,
        &mut recorder,
    );
    let palette = crate::doctor_runtime::resolve_palette(
        stdout_terminal,
        runtime.value("NO_COLOR").and_then(OsStr::to_str),
    );
    if streams
        .stdout
        .write_all(&recorder.render_with(&palette))
        .is_err()
    {
        return 1;
    }
    let counts = recorder.counts();
    let summary = crate::doctor_coordinator::summary_line(counts.pass, counts.warn, counts.fail);
    let color = crate::doctor_coordinator::summary_color(counts.fail, counts.warn);
    if streams.stdout.write_all(b"\n").is_err()
        || crate::ui::summary_box(streams.stdout, &renderer, color.name(), &summary).is_err()
    {
        return 1;
    }
    i32::from(!crate::doctor_coordinator::overall_ok(
        counts.fail,
        extension_status,
    ))
}

fn text(value: Option<&OsStr>) -> String {
    value
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_string()
}

fn renderer(runtime: &crate::app::Runtime, stdout_terminal: bool) -> crate::ui::Renderer {
    let path = text(runtime.value("PATH"));
    crate::ui::Renderer::select(
        crate::ui::find_gum(&path),
        stdout_terminal,
        runtime.value("NO_COLOR").and_then(OsStr::to_str),
    )
}

fn resolve(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    home: &str,
    euid: u32,
    stderr: &mut dyn std::io::Write,
) -> (crate::overlays::State, crate::profiles::State) {
    let prefix = text(runtime.value("PREFIX"));
    let inputs = crate::overlays::ResolveInputs {
        home: home.to_string(),
        xdg_config: text(runtime.value("XDG_CONFIG_HOME")),
        discovery_silent: true,
        default_profile: Some(config.default_profile.clone()),
        user: current_user(runtime),
        host: current_host(runtime),
        platform: current_platform(runtime),
        termux: prefix.contains("/com.termux/"),
        euid,
    };
    let mut overlays = crate::overlays::State::default();
    let mut profiles = crate::profiles::State::default();
    if let Err(error) = crate::overlays::resolve(&mut overlays, &mut profiles, "inspect", &inputs) {
        for warning in &overlays.warnings {
            let _ = writeln!(stderr, "{warning}");
        }
        let line = error.to_string();
        if !line.is_empty() {
            let _ = writeln!(stderr, "{line}");
        }
    }
    (overlays, profiles)
}

fn runtime_snapshot(runtime: &crate::app::Runtime, source: &str) -> RuntimeSnapshot {
    let (bash_version, bash_major) = bash_version(runtime);
    let checkout_root = git_output(
        runtime,
        Path::new(source),
        &["rev-parse", "--show-toplevel"],
    )
    .and_then(|root| std::fs::canonicalize(root).ok())
    .map(|root| root.as_os_str().as_bytes().to_vec());
    let source_root = std::fs::canonicalize(source)
        .map(|path| path.as_os_str().as_bytes().to_vec())
        .unwrap_or_default();
    let git_version = command_output(runtime, "git", &["--version"]);
    RuntimeSnapshot {
        bash_version,
        bash_major,
        checkout_root,
        source_raw: source.as_bytes().to_vec(),
        source_root,
        git_version,
        config_version: b"1".to_vec(),
    }
}

fn bash_version(runtime: &crate::app::Runtime) -> (Vec<u8>, u64) {
    let bash = runtime
        .value("BASH")
        .filter(|path| Path::new(path).is_absolute())
        .map(PathBuf::from)
        .or_else(|| runtime.find_on_path("bash"));
    let Some(bash) = bash else {
        return (Vec::new(), 0);
    };
    // `bash -c` would evaluate caller-controlled startup state such as
    // BASH_ENV merely to render a health row. The version flag is a
    // non-script capability probe, and an empty environment keeps the
    // observation outside the extension execution boundary.
    let output = Command::new(bash)
        .arg("--version")
        .env_clear()
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let version = output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| bash_version_field(&output.stdout))
        .unwrap_or_default();
    let major = String::from_utf8_lossy(&version)
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0);
    (version, major)
}

fn bash_version_field(output: &[u8]) -> Option<Vec<u8>> {
    let line = output.split(|byte| *byte == b'\n').next()?;
    let marker = b"version ";
    let start = line.windows(marker.len()).position(|part| part == marker)? + marker.len();
    let field = line[start..].split(|byte| *byte == b' ').next()?;
    (!field.is_empty()).then(|| field.to_vec())
}

fn engine_snapshot(runtime: &crate::app::Runtime, source: &str, home: &str) -> EngineSnapshot {
    let managed_base = value_or(
        runtime,
        "SHDEPS_INSTALL_DIR",
        &format!("{home}/.local/share"),
    );
    let development_base = value_or(runtime, "SHDEPS_GIT_DEV_DIR", &format!("{home}/git"));
    let managed = format!("{managed_base}/cgraf78/dot");
    let development = format!("{development_base}/dot");
    EngineSnapshot {
        source_raw: source.as_bytes().to_vec(),
        managed_raw: managed.as_bytes().to_vec(),
        development_raw: development.as_bytes().to_vec(),
        source_real: canonical_or_raw(source),
        managed_real: canonical_dir(&managed),
        development_real: canonical_dir(&development),
        ignore_dev_checkout: runtime.value("DOT_IGNORE_DEV_CHECKOUT") == Some(OsStr::new("1")),
    }
}

fn canonical_or_raw(path: &str) -> Vec<u8> {
    canonical_dir(path).unwrap_or_else(|| path.as_bytes().to_vec())
}

fn canonical_dir(path: &str) -> Option<Vec<u8>> {
    let path = std::fs::canonicalize(path).ok()?;
    path.is_dir().then(|| path.as_os_str().as_bytes().to_vec())
}

fn topology_name(topology: crate::repos_base::Topology) -> &'static str {
    match topology {
        crate::repos_base::Topology::Missing => "missing",
        crate::repos_base::Topology::Separate => "separate",
        crate::repos_base::Topology::Ordinary => "ordinary",
    }
}

fn provider_records(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    home: &str,
    source: &str,
) -> Vec<crate::doctor_checks::Record> {
    let provider = match config.provider {
        crate::config::Provider::None => None,
        crate::config::Provider::Shdeps => Some("shdeps"),
    };
    let policy = match config.shdeps_update_policy {
        crate::config::UpdatePolicy::Pinned => "pinned",
        crate::config::UpdatePolicy::Latest => "latest",
    };
    let configured =
        crate::shdeps_env_abi::configure_env(&crate::shdeps_env_abi::ConfigureInputs {
            xdg_config_home: &text(runtime.value("XDG_CONFIG_HOME")),
            home,
            install_dir: &text(runtime.value("SHDEPS_INSTALL_DIR")),
            bin_dir: &text(runtime.value("SHDEPS_BIN_DIR")),
            git_dev_dir: &text(runtime.value("SHDEPS_GIT_DEV_DIR")),
            dot_force: &text(runtime.value("DOT_FORCE")),
            dot_quiet: &text(runtime.value("DOT_QUIET")),
        });
    let dev_dir = configured
        .as_ref()
        .map(|configured| configured.git_dev_dir.as_str())
        .unwrap_or("");
    let development = Path::new(dev_dir).join("shdeps");
    let selected = configured
        .as_ref()
        .and_then(|configured| installer(runtime, Path::new(source), home, policy, configured));
    let installer_path = selected
        .as_ref()
        .map(|(path, _)| path.to_string_lossy().into_owned());
    let installer_source = selected.as_ref().map(|(_, source)| *source);
    let installer = installer_path
        .as_deref()
        .zip(installer_source)
        .map(|(path, source)| ProviderInstaller { path, source });
    let binary = installer_path.as_ref().and_then(|path| {
        crate::doctor_checks::shdeps_binary(
            runtime.value("_SHDEPSW_BIN").map(Path::new),
            Path::new(path),
        )
    });
    let actual = binary.as_ref().and_then(|binary| {
        crate::shdeps_env_abi::abi_version(
            binary,
            runtime
                .value("_DOT_SHDEPS_ABI_TIMEOUT_SECONDS")
                .and_then(OsStr::to_str)
                .unwrap_or("10"),
        )
    });
    crate::doctor_checks::check_provider(&ProviderInputs {
        home,
        dependency_provider: provider,
        policy,
        configure_ok: configured.is_some(),
        dev_dir,
        development_exists: std::fs::symlink_metadata(&development).is_ok(),
        development_valid: crate::shdeps_provider::development_checkout_valid(&development),
        installer,
        locked_revision: crate::shdeps::lock_value(Path::new(source), "revision").as_deref(),
        development_revision: Some(&crate::shdeps::active_revision(&development)),
        binary: binary.as_ref().and_then(|path| path.to_str()),
        expected_abi: crate::shdeps::lock_value(Path::new(source), "abi").as_deref(),
        actual_abi: actual.as_deref(),
    })
}

fn installer(
    runtime: &crate::app::Runtime,
    source_root: &Path,
    home: &str,
    policy: &str,
    configured: &crate::shdeps_env_abi::ConfiguredEnv,
) -> Option<(PathBuf, &'static str)> {
    if let Some(lib) = runtime.value("SHDEPS_LIB") {
        let path = Path::new(lib).parent()?.join("install.sh");
        if path.is_file() && crate::shdeps::installer_hash_matches(source_root, &path) {
            return Some((path, "explicit"));
        }
    }
    let development = Path::new(&configured.git_dev_dir).join("shdeps");
    let path = development.join("install.sh");
    if path.is_file()
        && development.join("shdeps.sh").is_file()
        && crate::shdeps::active_revision(&development)
            == crate::shdeps::lock_value(source_root, "revision")?
        && crate::shdeps::installer_hash_matches(source_root, &path)
    {
        return Some((path, "pinned-dev"));
    }
    if policy == "latest" && crate::shdeps_provider::development_checkout_valid(&development) {
        return Some((path, "latest-dev"));
    }
    let installed = runtime
        .value("SHDEPS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(home).join(".local/share/shdeps"));
    let path = installed.join("install.sh");
    if path.is_file()
        && installed.join("shdeps.sh").is_file()
        && crate::shdeps::installer_hash_matches(source_root, &path)
    {
        return Some((path, "managed"));
    }
    None
}

fn merge_count(
    config: &crate::config::Config,
    home: &str,
    overlays: &[String],
    manifest: &str,
    euid: u32,
    stderr: &mut dyn std::io::Write,
) -> Option<usize> {
    let root = config.extensions_dir.as_deref()?;
    let directory = Path::new(root).join("merge-hooks.d");
    if !directory.exists() {
        return Some(0);
    }
    let meta = std::fs::symlink_metadata(&directory).ok()?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Some(0);
    }
    let trust = crate::extension_trust::Inputs {
        euid,
        home: home.to_string(),
        extensions_dir: root.to_string(),
        manifest: manifest.to_string(),
        retiring_root: String::new(),
    };
    if !crate::extension_trust::root_validate(root, euid) {
        let _ = writeln!(stderr, "dot: unsafe extension root: {root}");
        return None;
    }
    if !crate::extension_trust::directory_validate(&directory, root, euid) {
        let _ = writeln!(
            stderr,
            "dot: unsafe merge-hook directory: {}",
            directory.display()
        );
        return None;
    }
    let entries = std::fs::read_dir(&directory).ok()?;
    let mut scripts = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.as_bytes().starts_with(b".") || !name.as_bytes().ends_with(b".sh") {
            continue;
        }
        scripts.push(entry.path());
    }
    scripts.sort_by(|left, right| {
        left.as_os_str()
            .as_bytes()
            .cmp(right.as_os_str().as_bytes())
    });
    let mut spec_count = 0;
    for index in 0..scripts.len() {
        let path = &scripts[index];
        if !crate::extension_trust::file_validate(path, &trust, overlays) {
            let _ = writeln!(stderr, "dot: unsafe merge hook: {}", path.display());
            return None;
        }
        // Identity belongs to the same glob-order iteration as trust in the
        // shell. Reusing the shared collector over the validated prefix keeps
        // one owner for identity and duplicate semantics while stopping at
        // the first condition for this entry.
        let refs: Vec<&OsStr> = scripts[..=index]
            .iter()
            .map(|script| script.as_os_str())
            .collect();
        match crate::merges::collect_specs(&refs) {
            Ok(specs) => spec_count = specs.len(),
            Err(error) => {
                let _ = writeln!(stderr, "{error}");
                return None;
            }
        }
    }
    Some(spec_count)
}

fn extensions(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    euid: u32,
    overlays: &[String],
    manifest: &str,
    stderr: &mut dyn std::io::Write,
    recorder: &mut Recorder,
) -> i32 {
    if !crate::config::extensions_enabled(config) {
        return 0;
    }
    let root = config.extensions_dir.as_deref().unwrap_or_default();
    if !crate::extension_trust::root_validate(root, euid) {
        recorder.fail(b"doctor extension discovery failed", None);
        return 1;
    }
    let directory = Path::new(root).join("doctor.d");
    if std::fs::symlink_metadata(&directory).is_err() {
        return 0;
    }
    if !crate::extension_trust::directory_validate(&directory, root, euid) {
        recorder.fail(b"doctor extension discovery failed", None);
        return 1;
    }
    let trust = crate::extension_trust::Inputs {
        euid,
        home: text(runtime.value("HOME")),
        extensions_dir: root.to_string(),
        manifest: manifest.to_string(),
        retiring_root: String::new(),
    };
    let discovery = match crate::doctor_coordinator::collect_specs_with(&directory, |script| {
        crate::extension_trust::file_validate(script, &trust, overlays)
    }) {
        Ok(discovery) => discovery,
        Err(_) => {
            recorder.fail(b"doctor extension discovery failed", None);
            return 1;
        }
    };
    if let Some(error) = discovery.error {
        let _ = stderr.write_all(&error.message());
        recorder.fail(b"doctor extension discovery failed", None);
        return 1;
    }
    let context_overlays: Vec<Vec<u8>> = overlays
        .iter()
        .map(|entry| entry.as_bytes().to_vec())
        .collect();
    let mut status = 0;
    let Some(bash) = doctor_bash(runtime) else {
        return 1;
    };
    let mut worker = crate::hook_worker::Worker::with_doctor(runtime, root, bash);
    for spec in discovery.specs {
        let mut launch = |call: &crate::doctor_orchestrator::WorkerInvocation<'_>| {
            let outcome = worker.doctor(
                call.script,
                call.temporary,
                call.result,
                call.context,
                call.token,
            );
            let _ = std::fs::write(call.log, &outcome.output);
            outcome.rc
        };
        let mut render = |path: &Path, recorder: &mut Recorder| render_records(path, recorder);
        if crate::doctor_orchestrator::run_extension_for(
            recorder,
            &spec.key,
            &spec.script,
            &context_overlays,
            &text(runtime.value("HOME")),
            euid,
            now_secs(),
            &temporary_root(runtime),
            &mut launch,
            &mut render,
        ) != 0
        {
            status = 1;
        }
    }
    status
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn temporary_root(runtime: &crate::app::Runtime) -> PathBuf {
    let root = runtime
        .value("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    if root.is_absolute() {
        root
    } else {
        runtime.cwd().join(root)
    }
}

fn render_records(path: &Path, recorder: &mut Recorder) {
    for record in crate::doctor_records::read(path).unwrap_or_default() {
        recorder.record(record);
    }
}

fn append(recorder: &mut Recorder, records: Vec<crate::doctor_checks::Record>) {
    for record in records {
        recorder.record(record);
    }
}

fn value_or(runtime: &crate::app::Runtime, key: &str, fallback: &str) -> String {
    runtime
        .value(key)
        .and_then(OsStr::to_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn doctor_bash(runtime: &crate::app::Runtime) -> Option<PathBuf> {
    runtime
        .value("BASH")
        .filter(|path| Path::new(path).is_absolute())
        .map(PathBuf::from)
        .or_else(|| runtime.find_on_path("bash"))
}

fn command(runtime: &crate::app::Runtime, program: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(runtime.env())
        .current_dir(runtime.cwd())
        .stdin(Stdio::null());
    command
}

fn command_output(runtime: &crate::app::Runtime, program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let program = runtime.find_on_path(program)?;
    let output = command(runtime, &program).args(args).output().ok()?;
    output.status.success().then(|| {
        output
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&output.stdout)
            .to_vec()
    })
}

fn current_uid(runtime: &crate::app::Runtime) -> Option<u32> {
    let output = command_output(runtime, "id", &["-u"])?;
    String::from_utf8_lossy(&output).trim().parse().ok()
}

fn current_user(runtime: &crate::app::Runtime) -> Option<String> {
    let output = command_output(runtime, "id", &["-un"])?;
    Some(
        String::from_utf8_lossy(&output)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    )
}

fn current_host(runtime: &crate::app::Runtime) -> Option<String> {
    for args in [&["-s"][..], &[][..]] {
        if let Some(output) = command_output(runtime, "hostname", args) {
            let value = String::from_utf8_lossy(&output);
            return Some(crate::platform::host_name(
                value.trim_end_matches(['\r', '\n']),
            ));
        }
    }
    None
}

fn current_platform(runtime: &crate::app::Runtime) -> Option<String> {
    let distro = text(runtime.value("WSL_DISTRO_NAME"));
    let interop = text(runtime.value("WSL_INTEROP"));
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let output = command_output(runtime, "uname", &["-s"])?;
    let value = String::from_utf8_lossy(&output);
    Some(crate::platform::platform_name(
        value.trim_end_matches(['\r', '\n']),
        crate::platform::is_wsl(&distro, &interop, osrelease.as_deref()),
    ))
}

fn git_output(runtime: &crate::app::Runtime, cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let program = runtime.find_on_path("git")?;
    let mut command = command(runtime, &program);
    crate::temp::sanitize_git_env(&mut command);
    crate::temp::bind_source_git(&mut command, cwd);
    let output = command.args(args).stderr(Stdio::null()).output().ok()?;
    output.status.success().then(|| {
        PathBuf::from(OsString::from_vec(
            output
                .stdout
                .strip_suffix(b"\n")
                .unwrap_or(&output.stdout)
                .to_vec(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::run_configured;

    #[test]
    fn disabled_extensions_do_not_inspect_configured_merge_root() {
        let home = crate::test_support::TempDir::new("doctor-disabled-merge-home").expect("home");
        let state =
            crate::test_support::TempDir::new("doctor-disabled-merge-state").expect("state");
        let extension_root = home.path().join("extensions");
        let hooks = extension_root.join("merge-hooks.d");
        std::fs::create_dir_all(&hooks).expect("hook directory");
        let unsafe_hook = hooks.join("10-unsafe.sh");
        std::fs::write(&unsafe_hook, b"merge() { :; }\n").expect("hook");
        for (path, mode) in [
            (extension_root.as_path(), 0o700),
            (hooks.as_path(), 0o700),
            (unsafe_hook.as_path(), 0o666),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
        }

        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let snippet = concat!(
            ". \"$1/lib/dot/config.sh\"\n",
            ". \"$1/lib/dot/doctor/runtime.sh\"\n",
            ". \"$1/lib/dot/doctor/merges.sh\"\n",
            "DOT_EXTENSION_API= DOT_EXTENSIONS_DIR=$2 _dr_check_merges\n",
        );
        let shell = Command::new(crate::test_support::bash())
            .args(["--noprofile", "--norc", "-c", snippet, "dot-test-sh"])
            .arg(repo)
            .arg(&extension_root)
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("shell merge check");
        assert!(shell.status.success(), "shell status");
        assert!(shell.stderr.is_empty(), "shell stderr: {:?}", shell.stderr);
        assert!(String::from_utf8_lossy(&shell.stdout).contains("no extension root configured"));

        let mut config = crate::config::load(&crate::config::Request {
            config_path: None,
            home: home.path().to_str().expect("home text"),
            env_policy: None,
        })
        .expect("default config");
        config.extension_api = false;
        config.extensions_dir = Some(extension_root.to_string_lossy().into_owned());
        let env = BTreeMap::<OsString, OsString>::from([
            ("HOME".into(), home.path().as_os_str().to_os_string()),
            (
                "XDG_STATE_HOME".into(),
                state.path().as_os_str().to_os_string(),
            ),
            ("DOT_SOURCE_ROOT".into(), repo.as_os_str().to_os_string()),
            ("PATH".into(), OsString::from("/usr/bin:/bin")),
            (
                "BASH".into(),
                OsStr::new(crate::test_support::bash()).to_os_string(),
            ),
            ("LC_ALL".into(), OsString::from("C")),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, home.path()).expect("runtime");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut streams = crate::app::Streams::with_terminal(&mut stdout, &mut stderr, false);
        let _ = run_configured(&runtime, &config, &mut streams);
        assert_eq!(stderr, shell.stderr, "disabled inventory diagnostics");
    }
}
