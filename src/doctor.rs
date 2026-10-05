//! Native `dot doctor` application coordinator.
//!
//! The check modules own individual health decisions. This module owns the
//! production boundary: capture one immutable Runtime, tolerate inspect-mode
//! overlay discovery failure, build the typed check inputs, authorize doctor
//! extensions, execute them through the versioned worker, and render every
//! result through one recorder.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::doctor_runtime::Record;

use crate::doctor_checks::{
    BaseRepoInputs, CronInputs, InstallInputs, LifecycleInputs, MergeInputs, MergeSpec,
    OverlayInputs, ProviderInputs, ProviderInstaller,
};
use crate::doctor_orchestrator::{EngineSnapshot, Recorder, RuntimeSnapshot};

/// `dot doctor --help` output.
pub const USAGE: &str = "usage: dot doctor [-h|--help]

Run the core health checks, then every configured doctor.d extension, and
report each finding. Exits 1 when a check fails or an extension fails, times
out, or is refused, and 2 for an argument doctor does not take; warnings do
not change the exit status. A client or overlay repository status check
whose Git does not answer within 30 seconds is stopped and reported as a
warning.

  -h, --help  print this usage

Environment:
  DOT_DOCTOR_JOBS     extensions run concurrently (default: DOT_UPDATE_JOBS,
                      else the CPU count; 1 runs them serially)
  DOT_DOCTOR_TIMEOUT  seconds each extension may run before it is stopped
                      and reported as timed out (default 60; 0 disables)
";

/// What `dot doctor`'s arguments ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Run the checks.
    Run,
    /// Print [`USAGE`].
    Help,
    /// An argument doctor does not take (reported, exit 2).
    Unexpected(Vec<u8>),
}

/// Parse `dot doctor`'s arguments. Doctor takes none besides help; it used to
/// ignore everything, so `dot doctor --help` ran every check instead of
/// explaining them. Help wins wherever it appears.
pub fn parse_args(args: &[&[u8]]) -> Request {
    if args
        .iter()
        .any(|arg| *arg == b"-h".as_slice() || *arg == b"--help".as_slice())
    {
        return Request::Help;
    }
    match args.first() {
        Some(arg) => Request::Unexpected(arg.to_vec()),
        None => Request::Run,
    }
}

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
    let Some(euid) = crate::temp::current_uid() else {
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
    let palette = crate::doctor_runtime::resolve_palette(
        stdout_terminal,
        runtime.value("NO_COLOR").and_then(OsStr::to_str),
    );
    // Extensions depend only on the resolved overlays and trust inputs
    // above, so they start now and run alongside the core checks instead of
    // after them; their rows are still filed after every core row.
    let mut extension_phase = ExtensionPhase::start(ExtensionJob {
        runtime: runtime.clone(),
        config: config.clone(),
        euid,
        overlays: overlays.active.clone(),
        overlays_unresolved: overlays.discovery_error.is_some(),
        manifest: constants.overlay_manifest.clone(),
        palette: palette.clone(),
    });
    let mut emit = Emitter::new(Recorder::new(), &palette, &mut *streams.stdout);
    let bash_required = crate::config::extensions_enabled(config)
        || config.provider == crate::config::Provider::Shdeps;
    if bash_required {
        if let Err(error) = runtime.bash() {
            let _ = streams
                .stderr
                .write_all(&runtime.bash_error_line_once(&error));
        }
    }
    let mut runtime_snapshot = runtime_snapshot(runtime, config, &source, bash_required);
    let engine = engine_snapshot(runtime, &source, &home);
    // The layout runs first so a healthy one is named in the version row;
    // its problem rows still follow the runtime rows.
    let layout = crate::doctor_checks::check_install_layout(&InstallInputs {
        home: &home,
        source_real: Path::new(OsStr::from_bytes(&engine.source_real)),
        release_root: runtime_snapshot.release_root,
        managed_root: Path::new(OsStr::from_bytes(&engine.managed_raw)),
        shdeps: config.provider == crate::config::Provider::Shdeps,
    });
    runtime_snapshot.install_kind = layout.kind.map(str::to_string);
    crate::doctor_orchestrator::check_runtime(
        emit.recorder(),
        &runtime_snapshot,
        &engine,
        home.as_bytes(),
    );
    append(emit.recorder(), layout.records);
    emit.emit();

    let topology = topology_name(base.topology);
    let git_dir = base.client_git_dir.as_str();
    let marker = Path::new(&state).join("dot/init/completed");
    append(
        emit.recorder(),
        crate::doctor_checks::check_base_repo(&BaseRepoInputs {
            topology,
            client_git_dir: git_dir,
            home: &home,
            // Consulted only without a separate client: skip its Git probes on
            // every other host.
            is_client_checkout: base.topology == crate::repos_base::Topology::Missing
                && crate::doctor_checks::is_client_checkout(Path::new(&home), Some(&marker)),
        }),
    );
    emit.emit();
    append(
        emit.recorder(),
        crate::doctor_checks::check_update_lock(&crate::update_lock::lock_path(
            runtime.state_home(),
        )),
    );
    emit.emit();
    append(
        emit.recorder(),
        crate::doctor_checks::check_cron_freshness(&CronInputs {
            last_success: crate::update_status::read_last_success(runtime.state_home()),
            last_converged: crate::update_status::read_last_converged(runtime.state_home()),
            last_run: crate::update_status::read_last_run(runtime.state_home()),
            last_failure: crate::update_status::read_last_failure(runtime.state_home()),
            cron_available: runtime.find_on_path("crontab").is_some(),
            now: crate::update_engine::now_secs(),
        }),
    );
    let checkpoint = crate::shdeps::checkpoint_in(runtime.state_home());
    append(
        emit.recorder(),
        crate::doctor_checks::check_reexec_checkpoint(
            &crate::shdeps::checkpoint_state(&checkpoint, runtime.source_root()),
            &checkpoint,
            &home,
        ),
    );
    emit.emit();
    append(
        emit.recorder(),
        provider_records(runtime, config, &home, &source),
    );
    emit.emit();
    if crate::cleanup::received_signal().is_some() {
        return 1;
    }

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
        emit.recorder(),
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
            unknown_keys: crate::overlays::unknown_keys(&overlays, &profiles)
                .cloned()
                .collect(),
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
    emit.emit();
    let merge_specs = if extensions_enabled {
        merge_inventory(
            config,
            &home,
            &overlays.active,
            &constants.overlay_manifest,
            euid,
            streams.stderr,
        )
    } else {
        Ok(Vec::new())
    };
    let (merge_specs, inventory_error) = match merge_specs {
        Ok(specs) => (Some(specs), None),
        Err(reason) => (None, Some(reason)),
    };
    append(
        emit.recorder(),
        crate::doctor_checks::check_merges(&MergeInputs {
            enabled: extensions_enabled,
            extensions_dir: config.extensions_dir.clone().unwrap_or_default(),
            spec_count: merge_specs.as_ref().map(Vec::len),
            inventory_error,
            specs: merge_specs.unwrap_or_default(),
        }),
    );
    emit.emit();

    let (extension_status, extension_counts) = extension_phase.finish(&mut emit);
    // Cancellation owns the final status at the CLI boundary. Do not render a
    // normal report after an interrupted extension is reaped; rows already
    // streamed stay visible so the interruption point is diagnosable.
    if crate::cleanup::received_signal().is_some() {
        return 1;
    }
    emit.emit();
    if emit.failed() {
        return 1;
    }
    let recorder = emit.finish();
    let core_counts = recorder.counts();
    let counts = crate::doctor_runtime::Counts {
        pass: core_counts.pass + extension_counts.pass,
        warn: core_counts.warn + extension_counts.warn,
        fail: core_counts.fail + extension_counts.fail,
    };
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

/// Streams filed doctor records to stdout as checks complete, so a slow
/// extension never holds already-known rows hostage. Rendering stays
/// byte-identical to the end-of-run report: the row renderer is linear over
/// concatenation, so emitting filed prefixes in order reproduces it exactly.
struct Emitter<'a> {
    recorder: Recorder,
    emitted: usize,
    failed: bool,
    unforwarded: Vec<u8>,
    palette: &'a crate::doctor_runtime::Palette,
    stdout: &'a mut dyn std::io::Write,
}

impl<'a> Emitter<'a> {
    fn new(
        recorder: Recorder,
        palette: &'a crate::doctor_runtime::Palette,
        stdout: &'a mut dyn std::io::Write,
    ) -> Self {
        Emitter {
            recorder,
            emitted: 0,
            failed: false,
            unforwarded: Vec::new(),
            palette,
            stdout,
        }
    }

    fn recorder(&mut self) -> &mut Recorder {
        &mut self.recorder
    }

    /// Write every record filed since the last emission. A delivery failure
    /// is remembered, not fatal: the run continues so extensions still
    /// execute and stderr diagnostics are still delivered, and the caller
    /// converts [`Emitter::failed`] into the exit status at the end, exactly
    /// like the old end-of-run render. The cursor advances only on success,
    /// so a transient failure retries the same rows next time.
    fn emit(&mut self) {
        let pending = &self.recorder.records()[self.emitted..];
        if !pending.is_empty() {
            let bytes = crate::doctor_runtime::render(pending, self.palette);
            if self.stdout.write_all(&bytes).is_ok() {
                self.emitted = self.recorder.records().len();
            } else {
                self.failed = true;
                return;
            }
        }
        // Rows forwarded from the extension phase follow every filed row.
        if !self.unforwarded.is_empty() {
            if self.stdout.write_all(&self.unforwarded).is_ok() {
                self.unforwarded.clear();
            } else {
                self.failed = true;
            }
        }
    }

    fn failed(&self) -> bool {
        self.failed
    }

    /// Write rows another emitter already rendered, after this one's rows.
    /// A failed write keeps them, and the next emit retries them after any
    /// row still pending here, so the order never changes.
    fn forward(&mut self, bytes: &[u8]) {
        self.unforwarded.extend_from_slice(bytes);
        self.emit();
    }

    fn finish(self) -> Recorder {
        self.recorder
    }
}

/// What the extension phase needs, owned so it can run on its own thread
/// while the core checks run on the caller's.
struct ExtensionJob {
    runtime: crate::app::Runtime,
    config: crate::config::Config,
    euid: u32,
    overlays: Vec<String>,
    overlays_unresolved: bool,
    manifest: String,
    palette: crate::doctor_runtime::Palette,
}

impl ExtensionJob {
    /// Run [`extensions`] for this job.
    fn run(&self, abort: &AtomicBool, emit: &mut Emitter<'_>) -> i32 {
        extensions(
            &self.runtime,
            &self.config,
            self.euid,
            &self.overlays,
            self.overlays_unresolved,
            &self.manifest,
            abort,
            emit,
        )
    }
}

/// Status and counts of a finished extension phase.
struct ExtensionResult {
    status: i32,
    counts: crate::doctor_runtime::Counts,
    failed: bool,
}

/// The doctor extension phase, started before the core checks.
///
/// The phase runs [`extensions`] unchanged on a thread of its own. Its
/// rendered rows go to a channel instead of stdout; they wait there until
/// the core checks finish, and then [`ExtensionPhase::finish`] writes them
/// in order, still streaming the rows of extensions that are running.
/// Stdout is byte-identical to running the phase after the core.
///
/// Cancellation and cleanup stay the dispatcher's: every extension session
/// observes a received signal and is stopped and reaped by its own
/// supervisor. Dropping an unfinished phase joins the thread, so no session
/// outlives `dot doctor`: after a signal (the only early return once the
/// phase starts) the sessions are already stopping, and while a core check
/// panics the phase's abort flag stops them instead of waiting them out.
///
/// The phase runs inline, after the core checks as before, when the
/// extension window is one job (`DOT_DOCTOR_JOBS=1` keeps a fully serial
/// mode for debugging) or the thread cannot be spawned.
enum ExtensionPhase {
    Running {
        output: std::sync::mpsc::Receiver<Vec<u8>>,
        abort: std::sync::Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<ExtensionResult>>,
    },
    Inline(Box<ExtensionJob>),
}

impl ExtensionPhase {
    fn start(job: ExtensionJob) -> Self {
        if serial_extensions(&job.runtime) {
            return ExtensionPhase::Inline(Box::new(job));
        }
        let (sender, output) = std::sync::mpsc::channel();
        let abort = std::sync::Arc::new(AtomicBool::new(false));
        let thread_abort = std::sync::Arc::clone(&abort);
        // Thread-local bindings do not cross threads; carry the caller's
        // pinned host Git like `dispatch_extensions` does for its workers.
        let host_git = crate::init_client_identity::carry_host_git();
        // Shared so a failed spawn (which drops the closure) gives it back.
        let job = std::sync::Arc::new(std::sync::Mutex::new(Some(job)));
        let thread_job = std::sync::Arc::clone(&job);
        let spawned = std::thread::Builder::new()
            .name("doctor-extensions".to_string())
            .spawn(move || {
                let _host_git = host_git.bind();
                let job = thread_job
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("extension job is taken once");
                let mut rows = RowChannel(sender);
                let mut emit = Emitter::new(Recorder::new(), &job.palette, &mut rows);
                let status = job.run(&thread_abort, &mut emit);
                emit.emit();
                let failed = emit.failed();
                ExtensionResult {
                    status,
                    counts: emit.finish().counts(),
                    failed,
                }
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(_) => {
                let job = job
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("an unspawned thread never took the job");
                return ExtensionPhase::Inline(Box::new(job));
            }
        };
        ExtensionPhase::Running {
            output,
            abort,
            thread: Some(thread),
        }
    }

    /// Wait for the phase, writing its rows through `emit` after every row
    /// `emit` already wrote, and return its status and counts.
    fn finish(&mut self, emit: &mut Emitter<'_>) -> (i32, crate::doctor_runtime::Counts) {
        let (output, thread) = match self {
            ExtensionPhase::Running { output, thread, .. } => (output, thread),
            // The old serial order: the core's own emitter and recorder,
            // whose counts already include these rows.
            ExtensionPhase::Inline(job) => {
                let abort = AtomicBool::new(false);
                let status = job.run(&abort, emit);
                return (status, crate::doctor_runtime::Counts::default());
            }
        };
        // The sender drops when the phase ends, closing the channel.
        for rows in output.iter() {
            emit.forward(&rows);
        }
        let thread = thread.take().expect("an extension phase finishes once");
        let result = match thread.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        if result.failed {
            emit.failed = true;
        }
        (result.status, result.counts)
    }
}

impl Drop for ExtensionPhase {
    fn drop(&mut self) {
        if let ExtensionPhase::Running {
            output,
            abort,
            thread,
        } = self
        {
            if let Some(thread) = thread.take() {
                if std::thread::panicking() {
                    // The dispatcher treats an abort without a worker panic
                    // as a broken invariant and panics too; the caller is
                    // already unwinding, so that payload is dropped below.
                    abort.store(true, Ordering::SeqCst);
                }
                // Keep draining so the thread never waits on the channel.
                for _ in output.iter() {}
                let _ = thread.join();
            }
        }
    }
}

/// Whether the extension window is one job by explicit setting
/// (`DOT_DOCTOR_JOBS`, else `DOT_UPDATE_JOBS`, as [`extension_jobs`] reads
/// them), without the CPU-count probe that an unset pair would need.
fn serial_extensions(runtime: &crate::app::Runtime) -> bool {
    let count = |name: &str| {
        let value = text(runtime.value(name));
        crate::merges::is_count(&value).then_some(value)
    };
    count("DOT_DOCTOR_JOBS")
        .or_else(|| count("DOT_UPDATE_JOBS"))
        .is_some_and(|jobs| jobs_count(&jobs) == 1)
}

/// The extension thread's stdout: each rendered batch becomes one message,
/// written by the core's thread once the core rows are out.
struct RowChannel(std::sync::mpsc::Sender<Vec<u8>>);

impl std::io::Write for RowChannel {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(bytes.to_vec())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
        host: current_host(),
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

fn runtime_snapshot(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    source: &str,
    bash_required: bool,
) -> RuntimeSnapshot {
    let (bash_version, bash_major) = if bash_required {
        bash_version(runtime)
    } else {
        (Vec::new(), 0)
    };
    let release_root = release_root(Path::new(source));
    // Only a development checkout is compared with its Git top level; a
    // release is reported from its install metadata, so its probe would be
    // a wasted child on every run.
    let checkout_root = (!release_root)
        .then(|| {
            git_output(
                runtime,
                Path::new(source),
                &["rev-parse", "--show-toplevel"],
            )
        })
        .flatten()
        .and_then(|root| std::fs::canonicalize(root).ok())
        .map(|root| root.as_os_str().as_bytes().to_vec());
    let source_root = std::fs::canonicalize(source)
        .map(|path| path.as_os_str().as_bytes().to_vec())
        .unwrap_or_default();
    // Report the Git that Dot's own inspection children run. A probe that
    // misses its deadline means Git did not answer, not that it is missing.
    let git_program = runtime.git_program();
    let git_path = git_program
        .as_ref()
        .map(|git| git.as_os_str().as_bytes().to_vec())
        .unwrap_or_default();
    let (git_version, git_stalled) = match git_program.map(|git| {
        program_output_typed(
            runtime,
            &git,
            &["--version"],
            crate::cleanup::LingerPolicy::Detach,
            crate::doctor_checks::probe_deadline(),
        )
    }) {
        Some(Ok(version)) => (version, false),
        Some(Err(crate::doctor_checks::TimedOut)) => (None, true),
        None => (None, false),
    };
    RuntimeSnapshot {
        bash_version,
        bash_major,
        bash_required,
        checkout_root,
        release_root,
        source_raw: source.as_bytes().to_vec(),
        source_root,
        git_version,
        git_stalled,
        git_path,
        version: crate::version::VERSION.as_bytes().to_vec(),
        install_kind: None,
        config_path: runtime
            .config_home()
            .join("dot/config")
            .as_os_str()
            .as_bytes()
            .to_vec(),
        unknown_config_keys: config.unknown_keys.clone(),
    }
}

fn release_root(root: &Path) -> bool {
    let metadata = root.join(".dot-install.json");
    let public = root.join("lib/dot/public");
    !root.join("Cargo.toml").is_file()
        && std::fs::symlink_metadata(metadata).is_ok_and(|entry| entry.file_type().is_file())
        && std::fs::symlink_metadata(public).is_ok_and(|entry| entry.file_type().is_dir())
}

fn bash_version(runtime: &crate::app::Runtime) -> (Vec<u8>, u64) {
    let Ok(bash) = runtime.bash() else {
        return (Vec::new(), 0);
    };
    (bash.version().to_vec(), bash.major())
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
        crate::config::Provider::None => {
            return crate::doctor_checks::check_provider(&ProviderInputs {
                home,
                dependency_provider: None,
                policy: "",
                configure_ok: true,
                dev_dir: "",
                development_exists: false,
                development_valid: false,
                installer: None,
                locked_revision: None,
                development_revision: None,
                binary: None,
                expected_abi: None,
                actual_abi: None,
                cancellation_capability: false,
                prompt_handshake_capability: false,
            });
        }
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
    // The installer choice and the provider check both read these; each is
    // a batch of Git children against the same checkout, so probe once.
    let development_valid = crate::shdeps_provider::development_checkout_valid(&development);
    let development_revision = crate::shdeps::active_revision(&development);
    let selected = configured.as_ref().and_then(|configured| {
        installer(
            runtime,
            Path::new(source),
            home,
            policy,
            configured,
            &Development {
                valid: development_valid,
                revision: &development_revision,
            },
        )
    });
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
    let expected_abi = crate::shdeps::lock_value(Path::new(source), "abi");
    let actual = binary
        .as_ref()
        .and_then(|binary| crate::shdeps_provider::doctor_abi_version(runtime, binary));
    let abi_matches = expected_abi.as_ref().is_some_and(|expected| {
        actual
            .as_deref()
            .is_some_and(|actual| actual == format!("abi:{expected}"))
    });
    let cancellation_capability = abi_matches
        && binary.as_ref().is_some_and(|binary| {
            crate::shdeps_provider::doctor_has_cancellation_capability(runtime, binary)
        });
    let prompt_handshake_capability = abi_matches
        && binary.as_ref().is_some_and(|binary| {
            crate::shdeps_provider::doctor_has_prompt_capability(runtime, binary)
        });
    crate::doctor_checks::check_provider(&ProviderInputs {
        home,
        dependency_provider: provider,
        policy,
        configure_ok: configured.is_some(),
        dev_dir,
        development_exists: std::fs::symlink_metadata(&development).is_ok(),
        development_valid,
        installer,
        locked_revision: crate::shdeps::lock_value(Path::new(source), "revision").as_deref(),
        development_revision: Some(&development_revision),
        binary: binary.as_ref().and_then(|path| path.to_str()),
        expected_abi: expected_abi.as_deref(),
        actual_abi: actual.as_deref(),
        cancellation_capability,
        prompt_handshake_capability,
    })
}

/// Probed facts about the Shdeps development checkout
/// (`<git_dev_dir>/shdeps`) shared by [`installer`] and the provider check.
struct Development<'a> {
    valid: bool,
    revision: &'a str,
}

fn installer(
    runtime: &crate::app::Runtime,
    source_root: &Path,
    home: &str,
    policy: &str,
    configured: &crate::shdeps_env_abi::ConfiguredEnv,
    probed: &Development<'_>,
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
        && probed.revision == crate::shdeps::lock_value(source_root, "revision")?
        && crate::shdeps::installer_hash_matches(source_root, &path)
    {
        return Some((path, "pinned-dev"));
    }
    if policy == "latest" && probed.valid {
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

/// Largest `.outputs` sidecar accepted: declarations are short
/// path lines, so 1 MiB bounds a corrupt sidecar without
/// constraining legitimate inventories.
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;

/// Read one `.outputs` sidecar: blank lines and `#` comments
/// skipped, the rest expanded through [`expand_home`]. Absolute results
/// are live outputs; anything else is invalid (raw line kept for the
/// diagnostic). `None` means the sidecar exists but cannot be read
/// (oversized, unreadable, or non-UTF-8).
fn read_outputs_sidecar(sidecar: &Path, home: &str) -> Option<(Vec<String>, Vec<String>)> {
    use std::io::Read as _;

    let file = std::fs::File::open(sidecar).ok()?;
    let mut content = String::new();
    file.take(SIDECAR_MAX_BYTES + 1)
        .read_to_string(&mut content)
        .ok()?;
    if content.len() as u64 > SIDECAR_MAX_BYTES {
        return None;
    }
    let mut outputs = Vec::new();
    let mut invalid = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let expanded = expand_home(line, home);
        if Path::new(&expanded).is_absolute() {
            outputs.push(expanded);
        } else {
            invalid.push(line.to_string());
        }
    }
    Some((outputs, invalid))
}

/// Expand the home placeholders a `.outputs` sidecar may use, exactly like
/// the hook runtime's `dot_expand_home`: replace `${HOME}` then `$HOME`
/// (single pass, no rescan — like bash `//`), then a leading `~` (`~` alone
/// or `~/...`; `~otheruser/...` is untouched, never resolved to another
/// user's home).
fn expand_home(value: &str, home: &str) -> String {
    let replaced = value.replace("${HOME}", home).replace("$HOME", home);
    if replaced == "~" {
        return home.to_string();
    }
    if let Some(rest) = replaced.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    replaced
}

/// The merge-hook inventory for output verification, or why it is invalid:
/// the reason, with its next step, becomes the row's detail rather than a
/// stderr line the report never mentions.
fn merge_inventory(
    config: &crate::config::Config,
    home: &str,
    overlays: &[String],
    manifest: &str,
    euid: u32,
    stderr: &mut dyn std::io::Write,
) -> Result<Vec<MergeSpec>, crate::doctor_checks::InventoryError> {
    let Some(root) = config.extensions_dir.as_deref() else {
        return Ok(Vec::new());
    };
    let directory = Path::new(root).join("merge-hooks.d");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let shown = |path: &Path| crate::doctor_paths::tilde(&path.to_string_lossy(), home);
    let fault =
        |detail: String, step: &'static str| crate::doctor_checks::InventoryError { detail, step };
    let untrusted = |path: &Path| fault(untrusted_detail(path, home), TRUST_STEP);
    let meta = std::fs::symlink_metadata(&directory).map_err(|error| {
        fault(
            format!("cannot inspect {}: {error}", shown(&directory)),
            TRUST_STEP,
        )
    })?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Ok(Vec::new());
    }
    let trust = crate::extension_trust::Inputs {
        euid,
        home: home.to_string(),
        extensions_dir: root.to_string(),
        manifest: manifest.to_string(),
    };
    if !crate::extension_trust::root_validate(root, euid) {
        return Err(untrusted(Path::new(root)));
    }
    if !crate::extension_trust::directory_validate(&directory, root, euid) {
        return Err(untrusted(&directory));
    }
    let entries = std::fs::read_dir(&directory).map_err(|error| {
        fault(
            format!("cannot read {}: {error}", shown(&directory)),
            TRUST_STEP,
        )
    })?;
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
    let mut rows: Vec<(OsString, OsString)> = Vec::new();
    for index in 0..scripts.len() {
        let path = &scripts[index];
        if !crate::extension_trust::file_validate(path, &trust, overlays) {
            return Err(untrusted(path));
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
            Ok(specs) => rows = specs,
            Err(crate::merges::SpecError::InvalidIdentity(_)) => {
                let error = crate::doctor_coordinator::SpecError::InvalidIdentity;
                return Err(fault(error.reason(&shown(path)), error.step()));
            }
            Err(crate::merges::SpecError::DuplicateIdentity(identity)) => {
                // The earlier claimant is the validated script whose key
                // maps to the same identity.
                let claimed_by = scripts[..index].iter().find_map(|script| {
                    let key = crate::merges::spec_key(script.as_os_str());
                    (crate::merges::spec_identity(&key).as_ref() == Some(&identity))
                        .then(|| script.file_name().unwrap_or_default().as_bytes().to_vec())
                });
                let error = crate::doctor_coordinator::SpecError::DuplicateIdentity {
                    identity: identity.into_vec(),
                    claimed_by: claimed_by.unwrap_or_default(),
                };
                return Err(fault(error.reason(&shown(path)), error.step()));
            }
        }
    }
    // Script validation precedes sidecar discovery, so script
    // diagnostics keep their historical precedence.
    let mut inventory = Vec::new();
    for (key, script) in &rows {
        // `collect_specs` accepted every key above, so each has an identity.
        let Some(identity) = crate::merges::spec_identity(key) else {
            continue;
        };
        let mut sidecar_name = key.as_bytes().to_vec();
        sidecar_name.extend_from_slice(b".outputs");
        let sidecar = directory.join(OsStr::from_bytes(&sidecar_name));
        let mut spec = MergeSpec {
            identity: identity.to_string_lossy().into_owned(),
            script: script.to_string_lossy().into_owned(),
            outputs: Vec::new(),
            invalid: Vec::new(),
        };
        if std::fs::symlink_metadata(&sidecar).is_ok() {
            // One bad sidecar degrades its own spec, never the whole
            // inventory: the hook fails verification (fail-closed for
            // that hook) while healthy siblings still verify.
            if !crate::extension_trust::file_validate(&sidecar, &trust, overlays) {
                let _ = writeln!(
                    stderr,
                    "dot: unsafe merge-hook outputs: {}",
                    sidecar.display()
                );
                spec.invalid.push(format!(
                    "{}: untrusted outputs declaration",
                    sidecar.display()
                ));
            } else if let Some((outputs, invalid)) = read_outputs_sidecar(&sidecar, home) {
                spec.outputs = outputs;
                spec.invalid = invalid;
            } else {
                let _ = writeln!(
                    stderr,
                    "dot: cannot read merge-hook outputs: {}",
                    sidecar.display()
                );
                spec.invalid.push(format!(
                    "{}: unreadable outputs declaration",
                    sidecar.display()
                ));
            }
        }
        inventory.push(spec);
    }
    Ok(inventory)
}

#[allow(clippy::too_many_arguments)]
fn extensions(
    runtime: &crate::app::Runtime,
    config: &crate::config::Config,
    euid: u32,
    overlays: &[String],
    overlays_unresolved: bool,
    manifest: &str,
    abort: &AtomicBool,
    emit: &mut Emitter<'_>,
) -> i32 {
    if !crate::config::extensions_enabled(config) {
        return 0;
    }
    let root = config.extensions_dir.as_deref().unwrap_or_default();
    let home = text(runtime.value("HOME"));
    // Every early return files a row saying what failed and what to do:
    // a bare nonzero status would leave the summary at "0 failed".
    let untrusted = |path: &Path| untrusted_detail(path, &home);
    if !crate::extension_trust::root_validate(root, euid) {
        let detail = untrusted(Path::new(root));
        emit.recorder().record(
            Record::fail("doctor extension discovery failed", Some(detail)).with_hint(TRUST_STEP),
        );
        return 1;
    }
    let directory = Path::new(root).join("doctor.d");
    if std::fs::symlink_metadata(&directory).is_err() {
        return 0;
    }
    if !crate::extension_trust::directory_validate(&directory, root, euid) {
        let detail = untrusted(&directory);
        emit.recorder().record(
            Record::fail("doctor extension discovery failed", Some(detail)).with_hint(TRUST_STEP),
        );
        return 1;
    }
    let trust = crate::extension_trust::Inputs {
        euid,
        home: home.clone(),
        extensions_dir: root.to_string(),
        manifest: manifest.to_string(),
    };
    let discovery = match crate::doctor_coordinator::collect_specs_with(&directory, |script| {
        crate::extension_trust::file_validate(script, &trust, overlays)
    }) {
        Ok(discovery) => discovery,
        Err(error) => {
            let shown = crate::doctor_paths::tilde(&directory.to_string_lossy(), &home);
            let detail = format!("cannot read {shown}: {error}");
            emit.recorder().fail(
                b"doctor extension discovery failed",
                Some(detail.as_bytes()),
            );
            return 1;
        }
    };
    // Refused scripts fail; the rest still run.
    let status = i32::from(!discovery.rejected.is_empty() || !discovery.invalid.is_empty());
    record_rejected(
        emit.recorder(),
        &discovery.rejected,
        &trust,
        overlays,
        overlays_unresolved,
    );
    record_invalid(emit.recorder(), &discovery.invalid, &home);
    if discovery.specs.is_empty() {
        return status;
    }
    let context_overlays: Vec<Vec<u8>> = overlays
        .iter()
        .map(|entry| entry.as_bytes().to_vec())
        .collect();
    let Ok(bash) = doctor_bash(runtime) else {
        // The Bash runtime row above already failed and says why; this row
        // names the consequence without counting the same cause twice.
        emit.recorder().skip(
            b"doctor extensions did not run",
            Some(b"they need Bash 4 or newer; see the Bash runtime row"),
        );
        return 1;
    };
    let worker = crate::hook_worker::Worker::with_doctor(runtime, root, bash.path().to_path_buf());
    let temporary = temporary_root(runtime);
    let timeout = extension_timeout(runtime);
    let execute = |spec: &crate::doctor_coordinator::Spec| {
        let mut launch = |call: &crate::doctor_orchestrator::WorkerInvocation<'_>| {
            // The deadline starts when the worker launches, not while it
            // waits for a slot in the window.
            // A limit too far out to represent is no deadline at all.
            let deadline = timeout.and_then(|limit| std::time::Instant::now().checked_add(limit));
            let outcome = worker.doctor(
                call.script,
                call.temporary,
                call.result,
                call.context,
                call.token,
                abort,
                deadline,
            );
            let _ = std::fs::write(call.log, &outcome.output);
            crate::doctor_orchestrator::WorkerExit {
                rc: outcome.rc,
                timed_out: timeout.filter(|_| outcome.timed_out),
            }
        };
        crate::doctor_orchestrator::execute_extension_for(
            &spec.key,
            &spec.script,
            &context_overlays,
            &home,
            euid,
            now_secs(),
            &temporary,
            &mut launch,
        )
    };
    let jobs = if discovery.specs.len() > 1 {
        extension_jobs(runtime)
    } else {
        1
    };
    dispatch_extensions(&discovery.specs, jobs, &execute, abort, emit).max(status)
}

/// Failure rows for refused doctor extensions. Links that are dangling or
/// that the overlay manifest does not authorize are almost always overlay
/// extensions waiting for the link phase: a pull renamed them, or the
/// overlays did not resolve this run. They share one row naming them all,
/// because one cause (and one `dot update`) covers every one; per-link rows
/// blamed owners and modes that are fine. An authorized link whose target
/// fails trust (a writable checkout, say), and a regular file that does, is
/// a real local problem that `dot update` will not fix: a row of its own.
fn record_rejected(
    recorder: &mut Recorder,
    rejected: &[PathBuf],
    trust: &crate::extension_trust::Inputs,
    overlays: &[String],
    overlays_unresolved: bool,
) {
    let home = trust.home.as_str();
    let key = |script: &Path| {
        let name = script.file_name().unwrap_or(script.as_os_str()).as_bytes();
        String::from_utf8_lossy(crate::doctor_coordinator::extension_key(name)).into_owned()
    };
    let (links, files): (Vec<&PathBuf>, Vec<&PathBuf>) = rejected.iter().partition(|script| {
        let link =
            std::fs::symlink_metadata(script).is_ok_and(|meta| meta.file_type().is_symlink());
        let dangling = std::fs::metadata(script).is_err();
        link && (dangling
            || !crate::extension_trust::symlink_authorized(
                script,
                home,
                &trust.manifest,
                overlays,
                trust.euid,
            ))
    });
    if !links.is_empty() {
        let message = match links.as_slice() {
            [only] => format!("{} doctor extension refused", key(only)),
            many => format!("{} doctor extensions refused", many.len()),
        };
        let subject = match links.as_slice() {
            [only] => format!(
                "{} is",
                crate::doctor_paths::tilde(&only.to_string_lossy(), home)
            ),
            many => format!(
                "{} are",
                many.iter()
                    .map(|link| key(link))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let next = if overlays_unresolved {
            "the overlays did not resolve; fix the overlay error above, then run 'dot update'"
        } else {
            "run 'dot update' to relink overlay extensions"
        };
        let kind = if links.len() == 1 {
            "a dangling or retired link"
        } else {
            "dangling or retired links"
        };
        let detail = format!("{subject} not linked from an active overlay ({kind})");
        recorder.record(Record::fail(message, Some(detail)).with_hint(next));
    }
    for script in files {
        let message = format!("{} doctor extension refused", key(script));
        let shown = crate::doctor_paths::tilde(&script.to_string_lossy(), home);
        let link =
            std::fs::symlink_metadata(script).is_ok_and(|meta| meta.file_type().is_symlink());
        let detail = if link {
            format!("{shown} links to a file that fails the extension trust checks")
        } else {
            untrusted_detail(script, home)
        };
        recorder.record(Record::fail(message, Some(detail)).with_hint(TRUST_STEP));
    }
}

/// The detail for an extension path (root, directory, or script) that fails
/// the trust checks; [`TRUST_STEP`] is its next step.
fn untrusted_detail(path: &Path, home: &str) -> String {
    let shown = crate::doctor_paths::tilde(&path.to_string_lossy(), home);
    format!("{shown} fails the extension trust checks")
}

/// The next step for an extension path that fails the trust checks.
pub(crate) const TRUST_STEP: &str = "check its owner and mode";

/// Failure rows for trusted doctor extensions refused for their file name: an
/// invalid identity, or one an earlier file already claimed. Each names its
/// file, the reason, and the fix, like a refused regular file, and the other
/// extensions still run.
fn record_invalid(
    recorder: &mut Recorder,
    invalid: &[(PathBuf, crate::doctor_coordinator::SpecError)],
    home: &str,
) {
    for (script, error) in invalid {
        let name = script.file_name().unwrap_or(script.as_os_str()).as_bytes();
        let mut message = crate::doctor_coordinator::extension_key(name).to_vec();
        message.extend_from_slice(b" doctor extension refused");
        let shown = crate::doctor_paths::tilde(&script.to_string_lossy(), home);
        recorder.record(
            Record::bytes(
                crate::doctor_runtime::Kind::Fail,
                &message,
                Some(error.reason(&shown).as_bytes()),
            )
            .with_hint(error.step()),
        );
    }
}

/// Default per-extension deadline: well above the slowest shipped extension
/// (an editor health check with its own 15s bound, on a loaded host running
/// every extension at once), yet a hung probe no longer holds every later
/// section hostage.
const DEFAULT_EXTENSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The per-extension deadline: `DOT_DOCTOR_TIMEOUT` seconds when it is a
/// whole number (`0` disables the deadline), otherwise the default.
fn extension_timeout(runtime: &crate::app::Runtime) -> Option<std::time::Duration> {
    timeout_from(runtime.value("DOT_DOCTOR_TIMEOUT").and_then(OsStr::to_str))
}

fn timeout_from(value: Option<&str>) -> Option<std::time::Duration> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) if value.bytes().all(|byte| byte.is_ascii_digit()) => match value.parse() {
            Ok(0) => None,
            Ok(secs) => Some(std::time::Duration::from_secs(secs)),
            // Too large to represent: effectively no deadline.
            Err(_) => None,
        },
        _ => Some(DEFAULT_EXTENSION_TIMEOUT),
    }
}

/// Bound on concurrently running doctor extensions: `DOT_DOCTOR_JOBS` when
/// numeric, else the shared update-job count (`DOT_UPDATE_JOBS`, else the CPU
/// count). Zero means one, so `DOT_DOCTOR_JOBS=1` restores serial execution.
fn extension_jobs(runtime: &crate::app::Runtime) -> usize {
    jobs_count(&crate::merges::parallel_jobs(
        &text(runtime.value("DOT_DOCTOR_JOBS")),
        &text(runtime.value("DOT_UPDATE_JOBS")),
    ))
}

/// Parse the normalized job count. The shared policy only yields digit
/// strings, so a parse failure is overflow: an absurdly large bound means
/// "unbounded" and saturates rather than silently collapsing to serial.
fn jobs_count(normalized: &str) -> usize {
    match normalized.parse::<usize>() {
        Ok(jobs) => jobs.max(1),
        Err(_) if !normalized.is_empty() && normalized.bytes().all(|b| b.is_ascii_digit()) => {
            usize::MAX
        }
        Err(_) => 1,
    }
}

/// Executes one discovered extension off the rendering thread. `Sync` because
/// every worker thread in the window shares the same closure.
type ExecuteExtension<'a> = dyn Fn(&crate::doctor_coordinator::Spec) -> crate::doctor_orchestrator::ExtensionOutcome
    + Sync
    + 'a;

/// Run doctor extensions through a FIFO window of at most `jobs` workers and
/// file their records strictly in discovery order.
///
/// Extensions report through private result files, so execution order is not
/// observable in the rendered output: each record set is filed only after
/// every earlier extension's, exactly as the serial loop produced it. Like
/// the merge-hook window, a full window waits for its *oldest* worker, which
/// is also the next one to render, so streaming output never stalls behind a
/// later extension.
///
/// Cancellation keeps the serial contract: no extension is launched after the
/// coordinator observes a signal, the in-flight extension being waited on is
/// still rendered, and every other in-flight worker is joined (its session
/// already received the forwarded signal) and discarded with its scratch.
///
/// A panicking worker is an engine bug and still unwinds the command, but
/// only after every sibling extension session has been stopped through
/// `abort` and joined; unwinding straight out of the scope would instead wait
/// on siblings that may never finish. The panicking worker raises `abort`
/// itself, wherever it sits in the window, so a hung *older* sibling that the
/// dispatcher is blocked on is released too. Outcomes that complete after an
/// abort are discarded unrendered, since their sessions were cut short. A
/// thread that cannot be spawned runs its extension inline under the same
/// panic handling, which degrades to serial behavior rather than failing
/// the extension.
fn dispatch_extensions(
    specs: &[crate::doctor_coordinator::Spec],
    jobs: usize,
    execute: &ExecuteExtension<'_>,
    abort: &AtomicBool,
    emit: &mut Emitter<'_>,
) -> i32 {
    // Worker threads do not inherit thread-local bindings, so carry the
    // caller's pinned host Git into each one explicitly.
    let host_git = crate::init_client_identity::carry_host_git();
    let mut status = 0;
    let mut panicked = None;
    std::thread::scope(|scope| {
        let mut pending = specs.iter();
        let mut in_flight = VecDeque::with_capacity(jobs.min(specs.len()));
        loop {
            while in_flight.len() < jobs && !abort.load(Ordering::SeqCst) {
                if crate::cleanup::received_signal().is_some() {
                    break;
                }
                let Some(spec) = pending.next() else {
                    break;
                };
                let host_git = host_git.clone();
                let spawned = std::thread::Builder::new().spawn_scoped(scope, move || {
                    let _host_git = host_git.bind();
                    execute_guarded(execute, spec, abort)
                });
                in_flight.push_back(match spawned {
                    Ok(handle) => InFlight::Running(handle),
                    Err(_) => InFlight::Done(execute_guarded(execute, spec, abort)),
                });
            }
            let Some(oldest) = in_flight.pop_front() else {
                break;
            };
            let outcome = match oldest.join() {
                Ok(outcome) if !abort.load(Ordering::SeqCst) => outcome,
                joined => {
                    // Some worker panicked: stop every session still
                    // running, then keep the first panic to re-raise
                    // outside the scope.
                    abort.store(true, Ordering::SeqCst);
                    panicked = joined.err();
                    for sibling in in_flight.drain(..) {
                        if let Err(panic) = sibling.join() {
                            panicked.get_or_insert(panic);
                        }
                    }
                    break;
                }
            };
            let mut render = |path: &Path, recorder: &mut Recorder| render_records(path, recorder);
            if crate::doctor_orchestrator::record_extension(emit.recorder(), outcome, &mut render)
                != 0
            {
                status = 1;
            }
            emit.emit();
            if crate::cleanup::received_signal().is_some() {
                for worker in in_flight.drain(..) {
                    // A panicking discarded worker must not mask the
                    // cancellation status; its scratch guard already ran.
                    drop(worker.join());
                }
                break;
            }
        }
    });
    settle(
        panicked,
        abort.load(Ordering::SeqCst),
        crate::cleanup::received_signal().is_some(),
        status,
    )
}

/// The dispatcher's final verdict, in precedence order: a panic collected
/// by the abort path re-raises; a received signal returns 1 even when a
/// worker drained during cancellation panicked and raised `abort` (the
/// drain discards that payload so it cannot mask the cancellation); an
/// abort with neither is a broken invariant.
fn settle(
    panicked: Option<Box<dyn std::any::Any + Send>>,
    aborted: bool,
    signalled: bool,
    status: i32,
) -> i32 {
    if let Some(panic) = panicked {
        std::panic::resume_unwind(panic);
    }
    if signalled {
        return 1;
    }
    if aborted {
        // Outside cancellation only a worker panic raises `abort`, and the
        // abort path always collects its payload.
        panic!("doctor extension worker aborted without a panic payload");
    }
    status
}

/// Run one extension, converting a panic into an abort of the whole window
/// before handing the payload back to the dispatcher.
fn execute_guarded(
    execute: &ExecuteExtension<'_>,
    spec: &crate::doctor_coordinator::Spec,
    abort: &AtomicBool,
) -> std::thread::Result<crate::doctor_orchestrator::ExtensionOutcome> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(spec)));
    if result.is_err() {
        abort.store(true, Ordering::SeqCst);
    }
    result
}

/// One slot of the extension window: a running worker thread, or a result
/// already produced inline because its thread could not be spawned.
enum InFlight<'scope> {
    Running(
        std::thread::ScopedJoinHandle<
            'scope,
            std::thread::Result<crate::doctor_orchestrator::ExtensionOutcome>,
        >,
    ),
    Done(std::thread::Result<crate::doctor_orchestrator::ExtensionOutcome>),
}

impl InFlight<'_> {
    fn join(self) -> std::thread::Result<crate::doctor_orchestrator::ExtensionOutcome> {
        match self {
            InFlight::Running(handle) => handle.join().and_then(|result| result),
            InFlight::Done(result) => result,
        }
    }
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

fn doctor_bash(runtime: &crate::app::Runtime) -> Result<crate::bash::Resolved, crate::bash::Error> {
    runtime.bash()
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
    // No deadline: the only caller is `id -un`, whose answer selects the
    // profile. A slow directory service must not make doctor resolve a
    // different profile than the unbounded lookup in `dot update`.
    program_output(
        runtime,
        &runtime.find_on_path(program)?,
        args,
        crate::cleanup::LingerPolicy::Strict,
        None,
    )
}

/// One read-only core probe with a captured, newline-trimmed stdout.
///
/// `linger` follows the supervisor's policy: Git builtins are deterministic
/// leaf tools whose only possible stragglers are a wrapper's short
/// telemetry helpers, so they detach on normal completion and skip the
/// host-wide process-table walk a strict stop pays (on a busy host one walk
/// costs about as much as the probe). Anything else stays strict. Both
/// policies stop and reap the whole session on timeout or cancellation.
fn program_output(
    runtime: &crate::app::Runtime,
    program: &Path,
    args: &[&str],
    linger: crate::cleanup::LingerPolicy,
    deadline: Option<std::time::Instant>,
) -> Option<Vec<u8>> {
    program_output_typed(runtime, program, args, linger, deadline)
        .ok()
        .flatten()
}

/// [`program_output`] that tells a probe which missed `deadline` apart
/// from one that failed or printed nothing.
fn program_output_typed(
    runtime: &crate::app::Runtime,
    program: &Path,
    args: &[&str],
    linger: crate::cleanup::LingerPolicy,
    deadline: Option<std::time::Instant>,
) -> Result<Option<Vec<u8>>, crate::doctor_checks::TimedOut> {
    let mut command = command(runtime, program);
    command.args(args);
    let output = match crate::cleanup::run_session_output_typed(
        command,
        deadline,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        linger,
    ) {
        Ok(output) => output,
        Err(crate::cleanup::SessionOutputError::TimedOut) => {
            return Err(crate::doctor_checks::TimedOut);
        }
        Err(_) => return Ok(None),
    };
    Ok(output.status.success().then(|| {
        output
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&output.stdout)
            .to_vec()
    }))
}

/// The login name, from `id -un` on the runtime's `PATH` like
/// `dot update`'s profile resolution ([`crate::profiles::current_user`]).
///
/// This one stays a child process on purpose: released binaries are static
/// musl builds, whose `getpwuid` reads only `/etc/passwd` (plus nscd when it
/// runs), so a directory-service account (LDAP, sssd) would have no name and
/// profile resolution would fail. `id` resolves names through the host's NSS.
fn current_user(runtime: &crate::app::Runtime) -> Option<String> {
    let output = command_output(runtime, "id", &["-un"])?;
    Some(
        String::from_utf8_lossy(&output)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    )
}

/// The short host name, read the way `dot update` reads it
/// ([`crate::platform::short_hostname`]) so both select the same profile.
fn current_host() -> Option<String> {
    crate::platform::short_hostname().map(|short| crate::platform::host_name(&short))
}

/// The platform name from the kernel name, with the runtime's WSL markers.
fn current_platform(runtime: &crate::app::Runtime) -> Option<String> {
    let distro = text(runtime.value("WSL_DISTRO_NAME"));
    let interop = text(runtime.value("WSL_INTEROP"));
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let kernel = crate::platform::kernel_name()?;
    Some(crate::platform::platform_name(
        &kernel,
        crate::platform::is_wsl(&distro, &interop, osrelease.as_deref()),
    ))
}

fn git_output(runtime: &crate::app::Runtime, cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let program = runtime.git_program()?;
    let mut command = command(runtime, &program);
    crate::temp::sanitize_git_env(&mut command);
    crate::temp::bind_source_git(&mut command, cwd);
    command.args(args).stderr(Stdio::null());
    // A Git builtin: detach like `program_output` explains.
    let output = crate::cleanup::run_session_output(
        command,
        crate::doctor_checks::probe_deadline(),
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    )
    .ok()?;
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

    use super::run_configured;

    #[test]
    fn sidecar_home_expansion_replaces_only_supported_tokens() {
        let home = "/home/tester";
        for (input, expected) in [
            ("$HOME/.ssh", "/home/tester/.ssh"),
            ("${HOME}/.ssh", "/home/tester/.ssh"),
            ("~", "/home/tester"),
            ("~/doc", "/home/tester/doc"),
            ("~other", "~other"),
            ("/abs", "/abs"),
            ("rel", "rel"),
            ("", ""),
            ("$HOME", "/home/tester"),
            ("${HOME}", "/home/tester"),
            ("~/$HOME", "/home/tester//home/tester"),
            ("$HOME~", "/home/tester~"),
            ("$$HOME", "$/home/tester"),
        ] {
            assert_eq!(super::expand_home(input, home), expected, "{input}");
        }
    }

    #[test]
    fn timeout_parsing_defaults_disables_and_saturates() {
        use std::time::Duration;
        let default = Some(super::DEFAULT_EXTENSION_TIMEOUT);
        assert_eq!(super::timeout_from(None), default);
        assert_eq!(super::timeout_from(Some("")), default);
        assert_eq!(
            super::timeout_from(Some(" 5 ")),
            Some(Duration::from_secs(5))
        );
        assert_eq!(super::timeout_from(Some("0")), None);
        for malformed in ["+5", "1.5", "-1", "5s", "x"] {
            assert_eq!(super::timeout_from(Some(malformed)), default, "{malformed}");
        }
        // Too large for u64: no deadline. Fits u64 but not an Instant: the
        // launch closure's checked add also yields no deadline (no panic).
        assert_eq!(super::timeout_from(Some("99999999999999999999")), None);
        let huge = super::timeout_from(Some("18446744073709551615")).expect("fits u64");
        assert_eq!(std::time::Instant::now().checked_add(huge), None);
    }

    #[test]
    fn a_probe_past_its_deadline_reads_as_timed_out_not_missing() {
        // The Git runtime row warns on a stall and fails only on a missing
        // or broken Git, so the probe must keep the two apart.
        let scope = dot_test_support::TempDir::new_exec("doctor-stalled-version").expect("scope");
        let script = |name: &str, body: &str| {
            let path = scope.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
            path
        };
        let runtime =
            crate::app::Runtime::from_env(&BTreeMap::new(), scope.path()).expect("runtime");
        let probe = |program: &Path, budget: std::time::Duration| {
            super::program_output_typed(
                &runtime,
                program,
                &["--version"],
                crate::cleanup::LingerPolicy::Detach,
                Some(std::time::Instant::now() + budget),
            )
        };
        let short = std::time::Duration::from_millis(300);
        let long = std::time::Duration::from_secs(20);
        assert_eq!(
            probe(&script("stalled", "exec sleep 30"), short),
            Err(crate::doctor_checks::TimedOut)
        );
        assert_eq!(
            probe(&script("broken", "exit 3"), long),
            Ok(None),
            "a failing probe is not a stall"
        );
        assert_eq!(
            probe(&script("healthy", "echo 'git version 2'"), long),
            Ok(Some(b"git version 2".to_vec()))
        );
    }

    #[test]
    fn usage_states_the_core_probe_deadline() {
        let bound = crate::doctor_checks::PROBE_TIMEOUT.as_secs();
        assert!(super::USAGE.contains(&format!("within {bound} seconds")));
    }

    #[test]
    fn explicit_single_job_runs_extensions_after_the_core() {
        let runtime = |pairs: &[(&str, &str)]| {
            let env = pairs
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect::<BTreeMap<_, _>>();
            crate::app::Runtime::from_env(&env, &std::env::temp_dir()).expect("runtime")
        };
        for (pairs, serial) in [
            (&[][..], false),
            (&[("DOT_DOCTOR_JOBS", "1")][..], true),
            (&[("DOT_DOCTOR_JOBS", "0")][..], true),
            (&[("DOT_DOCTOR_JOBS", "4")][..], false),
            (
                &[("DOT_DOCTOR_JOBS", "x"), ("DOT_UPDATE_JOBS", "1")][..],
                true,
            ),
            (
                &[("DOT_DOCTOR_JOBS", "2"), ("DOT_UPDATE_JOBS", "1")][..],
                false,
            ),
        ] {
            assert_eq!(
                super::serial_extensions(&runtime(pairs)),
                serial,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn job_counts_saturate_instead_of_collapsing_to_serial() {
        assert_eq!(super::jobs_count("3"), 3);
        assert_eq!(super::jobs_count("0"), 1);
        assert_eq!(super::jobs_count("99999999999999999999"), usize::MAX);
        assert_eq!(super::jobs_count(""), 1);
        assert_eq!(super::jobs_count("x"), 1);
    }

    /// Dispatch one panicking and one hung extension through a two-worker
    /// window, with the panicking one at `panicking` (0 = oldest). The hung
    /// stand-in worker can only finish early through the abort flag, so a
    /// prompt unwind proves the dispatcher stopped it rather than waited.
    fn assert_panic_aborts_hung_sibling(panicking: usize) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = dot_test_support::TempDir::new("doctor-dispatch-panic").expect("root");
        let keys = if panicking == 0 {
            ["10-panics", "20-hangs"]
        } else {
            ["10-hangs", "20-panics"]
        };
        let specs: Vec<crate::doctor_coordinator::Spec> = keys
            .iter()
            .map(|key| crate::doctor_coordinator::Spec {
                key: key.as_bytes().to_vec(),
                script: root.path().join(format!("{key}.sh")),
            })
            .collect();
        let abort = AtomicBool::new(false);
        let sibling_aborted = AtomicBool::new(false);
        let sibling_started = AtomicBool::new(false);
        let execute = |spec: &crate::doctor_coordinator::Spec| {
            if spec.key.ends_with(b"-panics") {
                // Fail only once the sibling is provably running, so the
                // coordinator must actively stop it rather than find it done.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while !sibling_started.load(Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "sibling never started"
                    );
                    std::thread::yield_now();
                }
                panic!("injected worker failure");
            }
            let mut worker = |_: &crate::doctor_orchestrator::WorkerInvocation<'_>| {
                sibling_started.store(true, Ordering::SeqCst);
                // Stands in for a hung extension session: only the abort
                // flag can end it before the deadline.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while !abort.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                sibling_aborted.store(abort.load(Ordering::SeqCst), Ordering::SeqCst);
                crate::doctor_orchestrator::WorkerExit::from(1)
            };
            crate::doctor_orchestrator::execute_extension_for(
                &spec.key,
                &spec.script,
                &[],
                root.path().to_str().expect("home"),
                crate::temp::current_uid().expect("uid"),
                super::now_secs(),
                root.path(),
                &mut worker,
            )
        };
        let palette = crate::doctor_runtime::Palette::empty();
        let mut stdout = Vec::new();
        let started = std::time::Instant::now();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut emit = super::Emitter::new(
                crate::doctor_orchestrator::Recorder::new(),
                &palette,
                &mut stdout,
            );
            super::dispatch_extensions(&specs, 2, &execute, &abort, &mut emit)
        }));
        let payload = unwound.expect_err("worker panic must still unwind");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"injected worker failure"),
            "the original panic must be re-raised"
        );
        assert!(
            sibling_aborted.load(Ordering::SeqCst),
            "sibling was not aborted"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "unwinding waited for the hung sibling"
        );
        assert!(stdout.is_empty(), "no records may render after a panic");
        assert_eq!(
            std::fs::read_dir(root.path())
                .expect("root")
                .filter(|entry| entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with("dot.")))
                .count(),
            0,
            "aborted sibling left scratch state"
        );
    }

    #[test]
    fn oldest_worker_panic_aborts_hung_younger_sibling() {
        assert_panic_aborts_hung_sibling(0);
    }

    #[test]
    fn younger_worker_panic_aborts_hung_older_sibling() {
        assert_panic_aborts_hung_sibling(1);
    }

    #[test]
    fn cancellation_status_wins_over_a_drained_worker_panic() {
        // A worker drained after a signal may panic and raise `abort`; its
        // payload is discarded so the command still reports cancellation.
        assert_eq!(super::settle(None, true, true, 0), 1);
        assert_eq!(super::settle(None, false, true, 0), 1);
        assert_eq!(super::settle(None, false, false, 7), 7);
        let reraised =
            std::panic::catch_unwind(|| super::settle(Some(Box::new("collected")), true, true, 0))
                .expect_err("a collected panic re-raises");
        assert_eq!(reraised.downcast_ref::<&str>(), Some(&"collected"));
        std::panic::catch_unwind(|| super::settle(None, true, false, 0))
            .expect_err("abort without a payload or signal is a broken invariant");
    }

    #[test]
    fn guarded_execution_raises_abort_on_panic() {
        // The inline spawn-failure fallback shares this guard with the
        // worker threads.
        use std::sync::atomic::{AtomicBool, Ordering};

        let abort = AtomicBool::new(false);
        let spec = crate::doctor_coordinator::Spec {
            key: b"10-panics".to_vec(),
            script: std::path::PathBuf::from("/fixture/10-panics.sh"),
        };
        let execute =
            |_: &crate::doctor_coordinator::Spec| -> crate::doctor_orchestrator::ExtensionOutcome {
                panic!("injected inline failure")
            };
        assert!(super::execute_guarded(&execute, &spec, &abort).is_err());
        assert!(abort.load(Ordering::SeqCst));
    }

    /// P1: the core's Git probes (the dot checkout's top level and the Git
    /// version) are read-only builtins, so normal completion must not pay a
    /// host-wide process-table walk. On a devserver with thousands of
    /// processes each walk cost about as much as the probe itself.
    #[test]
    fn core_git_probes_skip_the_process_table_walk() {
        let home = dot_test_support::TempDir::new("doctor-git-probe-home").expect("home");
        let status = dot_test_support::git()
            .args(["init", "-q"])
            .current_dir(home.path())
            .status()
            .expect("git init fixture");
        assert!(status.success());
        let git = dot_test_support::real_tool("git");
        let env = BTreeMap::<OsString, OsString>::from([
            ("HOME".into(), home.path().as_os_str().to_os_string()),
            (
                "PATH".into(),
                git.parent().expect("git dir").as_os_str().to_os_string(),
            ),
            ("LC_ALL".into(), OsString::from("C")),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, home.path()).expect("runtime");
        crate::cleanup::reset_global_process_snapshot_calls();
        let top = super::git_output(&runtime, home.path(), &["rev-parse", "--show-toplevel"]);
        assert!(top.is_some(), "rev-parse answered");
        let version = super::program_output(
            &runtime,
            &git,
            &["--version"],
            crate::cleanup::LingerPolicy::Detach,
            None,
        );
        assert!(version.is_some_and(|version| version.starts_with(b"git version")));
        assert_eq!(
            crate::cleanup::global_process_snapshot_calls(),
            0,
            "read-only Git probes must not walk the process table"
        );
    }

    /// Identity comes from the kernel, read exactly as `dot update` reads it,
    /// so doctor and update select the same profile without spawning
    /// `hostname` or `uname`.
    #[test]
    fn host_and_platform_match_the_update_identity() {
        let env = BTreeMap::<OsString, OsString>::from([("PATH".into(), OsString::new())]);
        let cwd = std::env::temp_dir();
        let runtime = crate::app::Runtime::from_env(&env, &cwd).expect("runtime");
        assert_eq!(
            super::current_host(),
            crate::platform::detect_host().ok(),
            "an empty PATH no longer hides the host"
        );
        let platform = super::current_platform(&runtime).expect("platform");
        if std::env::var_os("WSL_DISTRO_NAME").is_none()
            && std::env::var_os("WSL_INTEROP").is_none()
        {
            assert_eq!(Some(platform), crate::platform::detect_platform().ok());
        }
    }

    /// A core check that panics while the extension phase runs must stop the
    /// running extensions, not wait for them.
    #[test]
    fn a_core_panic_stops_the_running_extension_phase() {
        let home = dot_test_support::TempDir::new("doctor-phase-drop-home").expect("home");
        let root = home.path().join("extensions");
        let directory = root.join("doctor.d");
        std::fs::create_dir_all(&directory).expect("doctor directory");
        let script = directory.join("10-hang.sh");
        std::fs::write(
            &script,
            b"doctor() {\n  printf '%s\\n' \"$BASHPID\" >\"$HOME/hang.pid\"\n  sleep 60\n}\n",
        )
        .expect("extension");
        for (path, mode) in [(&root, 0o700), (&directory, 0o700), (&script, 0o644)] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut config = crate::config::load(&crate::config::Request {
            config_path: None,
            home: home.path().to_str().expect("home text"),
            env_policy: None,
        })
        .expect("default config");
        config.extension_api = true;
        config.extensions_dir = Some(root.to_string_lossy().into_owned());
        let env = BTreeMap::<OsString, OsString>::from([
            ("HOME".into(), home.path().as_os_str().to_os_string()),
            ("DOT_SOURCE_ROOT".into(), repo.as_os_str().to_os_string()),
            ("PATH".into(), OsString::from("/usr/bin:/bin")),
            (
                "DOT_BASH".into(),
                OsStr::new(dot_test_support::bash()).to_os_string(),
            ),
            ("DOT_DOCTOR_JOBS".into(), OsString::from("2")),
            ("LC_ALL".into(), OsString::from("C")),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, home.path()).expect("runtime");
        let phase = super::ExtensionPhase::start(super::ExtensionJob {
            runtime,
            config,
            euid: crate::temp::current_uid().expect("uid"),
            overlays: Vec::new(),
            overlays_unresolved: false,
            manifest: String::new(),
            palette: crate::doctor_runtime::Palette::empty(),
        });
        assert!(matches!(phase, super::ExtensionPhase::Running { .. }));
        let pid_file = home.path().join("hang.pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "extension never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let started = std::time::Instant::now();
        // A core check panics while the phase runs.
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _phase = phase;
            panic!("injected core check failure");
        }));
        assert!(unwound.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "drop waited out the extension: {:?}",
            started.elapsed()
        );
        // SAFETY: signal 0 only checks existence; no signal is sent.
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "extension outlived the phase"
        );
    }

    #[test]
    fn disabled_extensions_do_not_inspect_configured_merge_root() {
        let home = dot_test_support::TempDir::new("doctor-disabled-merge-home").expect("home");
        let state = dot_test_support::TempDir::new("doctor-disabled-merge-state").expect("state");
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
                OsStr::new(dot_test_support::bash()).to_os_string(),
            ),
            ("LC_ALL".into(), OsString::from("C")),
        ]);
        let runtime = crate::app::Runtime::from_env(&env, home.path()).expect("runtime");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut streams = crate::app::Streams::with_terminal(&mut stdout, &mut stderr, false);
        let _ = run_configured(&runtime, &config, &mut streams);
        assert!(
            stderr.is_empty(),
            "disabled inventory diagnostics: {stderr:?}"
        );
    }
}
