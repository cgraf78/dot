//! Bounded native suite scheduler. File-backed output lets sequential display
//! follow progress without pipe-reader threads that escaped writers can hold
//! open. Indexed outcomes preserve discovery order when replaying output.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::app::Streams;
use crate::test_command::{Context, Options, Suite, environment, label, valid};
use crate::test_suites::{SuiteClassification, classify_suite, format_summary, suite_timeout};

const OWNER: &str = ".dot-suite-owner-v3";
const RULE: &str = "════════════════════════════════";

fn render_error(_: crate::ui::Error) -> std::io::Error {
    std::io::Error::other("could not render test output")
}

struct Invocation {
    root: PathBuf,
    cleanup: crate::cleanup::Registry,
}

impl Drop for Invocation {
    fn drop(&mut self) {
        self.cleanup.cleanup();
    }
}

impl Invocation {
    fn new(context: &Context<'_>) -> std::io::Result<Self> {
        let base = context
            .runtime
            .value("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let parent = base.join(format!("dot-suite-runs.{}", context.euid));
        match fs::DirBuilder::new().mode(0o700).create(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let meta = fs::symlink_metadata(&parent)?;
        if !meta.is_dir() || meta.uid() != context.euid {
            return Err(std::io::Error::other(format!(
                "unsafe temporary root: {}",
                parent.display()
            )));
        }
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_secs();
        prune(&parent, context.euid, now);
        // Atomic mkdir is the ownership boundary; concurrent calls never share
        // a run directory even when clocks or process IDs repeat.
        let mut nonce = 0u64;
        let root = loop {
            let path = parent.join(format!("run.{}.{now}.{nonce}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => nonce += 1,
                Err(error) => return Err(error),
            }
        };
        let mut cleanup = crate::cleanup::Registry::new();
        cleanup
            .register_path(&root)
            .map_err(std::io::Error::other)?;
        let invocation = Self { root, cleanup };
        let mut marker = create(&invocation.root.join(OWNER))?;
        writeln!(marker, "{}\t{now}", std::process::id())?;
        Ok(invocation)
    }

    fn remove(&mut self) -> bool {
        self.cleanup.remove_path(&self.root).is_ok()
    }
}

fn prune(parent: &Path, uid: u32, now: u64) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("run.") {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && meta.uid() == uid) {
            continue;
        }
        let marker = path.join(OWNER);
        if !fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file() && meta.uid() == uid) {
            continue;
        }
        let Ok(bytes) = fs::read_to_string(marker) else {
            continue;
        };
        let Some(line) = bytes.lines().next() else {
            continue;
        };
        let mut fields = line.split('\t').filter(|field| !field.is_empty());
        let Some(pid) = fields
            .next()
            .filter(|value| crate::cleanup::valid_pid(value) && value.len() <= 10)
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(started) = fields
            .next()
            .filter(|value| crate::cleanup::valid_pid(value) && value.len() <= 18)
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let live = i32::try_from(pid).ok().is_some_and(crate::cleanup::alive);
        if !live && now.checked_sub(started).is_some_and(|age| age > 86400) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

struct Worker {
    child: Option<Child>,
    index: usize,
    started: Instant,
    timeout: Duration,
    result_path: PathBuf,
    result: File,
    output: File,
    reader: File,
    offset: u64,
    status: Option<i32>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = crate::cleanup::stop_session(&mut child, libc::SIGTERM);
        }
    }
}

struct Outcome {
    classification: SuiteClassification,
    elapsed: u64,
    result: Vec<u8>,
    output: File,
}

struct Workers(Vec<Worker>);

impl Workers {
    fn stop(&mut self) {
        let mut children: Vec<_> = self
            .0
            .iter_mut()
            .filter_map(|worker| worker.child.take())
            .collect();
        let _ = crate::cleanup::stop_sessions(&mut children, libc::SIGTERM);
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.stop();
    }
}

fn create(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn duration(raw: &str) -> Option<Duration> {
    let (number, multiplier) = match raw.as_bytes().last() {
        Some(b's') => (&raw[..raw.len() - 1], 1.),
        Some(b'm') => (&raw[..raw.len() - 1], 60.),
        Some(b'h') => (&raw[..raw.len() - 1], 3600.),
        Some(b'd') => (&raw[..raw.len() - 1], 86400.),
        _ => (raw, 1.),
    };
    let seconds = number.trim().parse::<f64>().ok()? * multiplier;
    if seconds <= 0. {
        return None;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

fn start(
    context: &Context<'_>,
    options: &Options,
    suite: &Suite,
    index: usize,
    root: &Path,
) -> std::io::Result<Worker> {
    let name = label(suite);
    let temporary = root.join(format!("{name}.tmp"));
    fs::DirBuilder::new().mode(0o700).create(&temporary)?;
    let result = root.join(format!("{name}.result"));
    let result_file = create(&result)?;
    let output = root.join(format!("{name}.out"));
    let mut writer = create(&output)?;
    let reader = File::open(&output)?;
    let mut worker = Worker {
        child: None,
        index,
        started: Instant::now(),
        timeout: Duration::ZERO,
        result_path: result,
        result: result_file,
        output: writer.try_clone()?,
        reader,
        offset: 0,
        status: None,
    };
    // Revalidation occurs at launch, after queued predecessors have run. The
    // trust predicate is shared with discovery rather than copied here.
    if !valid(context, suite) {
        writeln!(writer, "dot: test suite changed after discovery: {name}")?;
        worker.status = Some(126);
        return Ok(worker);
    }
    let raw = suite_timeout(
        suite.source.name(),
        context
            .runtime
            .value("DOT_TEST_SUITE_TIMEOUT_SECONDS")
            .and_then(|value| value.to_str()),
    );
    let Some(timeout) = duration(&raw) else {
        writeln!(writer, "test-timeout-v1: invalid timeout: {raw}")?;
        worker.status = Some(2);
        return Ok(worker);
    };
    worker.timeout = timeout;
    let env = environment(
        context,
        options,
        suite,
        root,
        &temporary,
        &worker.result_path,
    )
    .map_err(|(_, message)| std::io::Error::other(message))?;
    let mut command = Command::new(&suite.path);
    command
        .env_clear()
        .envs(env)
        .current_dir(context.runtime.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .stderr(Stdio::from(writer));
    crate::cleanup::isolate(&mut command);
    match command.spawn() {
        Ok(child) => worker.child = Some(child),
        Err(error) => {
            worker.status = Some(if error.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            })
        }
    }
    Ok(worker)
}

/// Read only the currently observed file length, so a prolific or escaped
/// writer cannot turn one display tick into an unbounded read-to-EOF.
fn drain(worker: &mut Worker, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let length = worker.reader.metadata()?.len();
    worker.reader.seek(SeekFrom::Start(worker.offset))?;
    let count = copy_interruptible(
        &mut (&mut worker.reader).take(length.saturating_sub(worker.offset)),
        out,
    )?;
    worker.offset += count;
    out.flush()
}

/// Copy file-backed suite output while retaining cancellation as an escape
/// from a blocked stdout. The installed handlers omit `SA_RESTART`, so a
/// signal interrupts the underlying write and lets this loop observe the
/// process-wide latch instead of retrying forever like `write_all`/`copy`.
fn copy_interruptible(
    input: &mut dyn std::io::Read,
    output: &mut dyn std::io::Write,
) -> std::io::Result<u64> {
    let mut copied = 0;
    let mut buffer = [0; 8192];
    loop {
        if crate::cleanup::received_signal().is_some() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let count = match input.read(&mut buffer) {
            Ok(0) => return Ok(copied),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let mut remaining = &buffer[..count];
        while !remaining.is_empty() {
            if crate::cleanup::received_signal().is_some() {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            match output.write(remaining) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(written) => {
                    copied += written as u64;
                    remaining = &remaining[written..];
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn finish(worker: &mut Worker, options: &Options) -> std::io::Result<Outcome> {
    let code = worker.status.unwrap_or(1);
    worker.result.seek(SeekFrom::Start(0))?;
    let length = worker.result.metadata()?.len();
    let mut result = Vec::new();
    (&mut worker.result).take(length).read_to_end(&mut result)?;
    let mut classification = classify_suite(code, Some(&result));
    if options.parallel {
        // Preserve the worker-publication failure contract, although native
        // scheduling itself uses typed outcomes and never polls these files.
        let status_path = worker.result_path.with_extension("rc");
        if create(&status_path)
            .and_then(|mut file| writeln!(file, "{code}"))
            .is_err()
        {
            writeln!(
                worker.output,
                "dot test: worker exited before publishing a result (status {code})"
            )?;
            classification = SuiteClassification::Fail;
        }
    }
    Ok(Outcome {
        classification,
        elapsed: worker.started.elapsed().as_secs(),
        result,
        output: worker.reader.try_clone()?,
    })
}

/// Schedule at most the requested number of workers and own teardown through
/// every return path, including display errors and cancellation during startup.
pub(crate) fn run(
    context: &Context<'_>,
    options: &Options,
    suites: &[Suite],
    streams: &mut Streams<'_>,
) -> i32 {
    match execute(context, options, suites, streams) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(streams.stderr, "dot test: {error}");
            1
        }
    }
}

fn execute(
    context: &Context<'_>,
    options: &Options,
    suites: &[Suite],
    streams: &mut Streams<'_>,
) -> std::io::Result<i32> {
    crate::cleanup::adopt_descendants()?;
    let signals = crate::cleanup::Signals::for_runtime(context.runtime)?;
    let stdout_terminal = streams.stdout_is_terminal();
    let mut stdout = signals.writer(streams.stdout);
    let mut stderr = signals.writer(streams.stderr);
    let mut guarded_streams = Streams::with_terminal(&mut stdout, &mut stderr, stdout_terminal);
    let streams = &mut guarded_streams;
    let result = (|| -> std::io::Result<i32> {
        let mut invocation = Invocation::new(context)?;
        let renderer = if options.color {
            crate::ui::Renderer::select(
                crate::ui::find_gum(
                    &context
                        .runtime
                        .value("PATH")
                        .unwrap_or_default()
                        .to_string_lossy(),
                ),
                streams.stdout_is_terminal(),
                None,
            )
        } else {
            crate::ui::Renderer::Plain
        };
        crate::ui::title(streams.stdout, &renderer, "dot test").map_err(render_error)?;
        writeln!(streams.stdout)?;
        styled(
            streams.stdout,
            options,
            "dim",
            &if options.parallel {
                format!(
                    "Running {} test suites with up to {} jobs...",
                    suites.len(),
                    options.jobs
                )
            } else {
                format!("Running {} test suites...", suites.len())
            },
        )?;
        if options.parallel {
            writeln!(streams.stdout)?;
        }
        let mut workers = Workers(Vec::new());
        let mut outcomes: Vec<Option<Outcome>> = (0..suites.len()).map(|_| None).collect();
        let mut next = 0;
        let mut completed = 0;
        let limit = if options.parallel { options.jobs } else { 1 };
        while completed < suites.len() {
            if signals.received().is_some() {
                workers.stop();
                return Ok(1);
            }
            while workers.0.len() < limit && next < suites.len() && signals.received().is_none() {
                if !options.parallel {
                    writeln!(streams.stdout)?;
                    header(streams.stdout, options, &label(&suites[next]), false)?;
                }
                workers.0.push(start(
                    context,
                    options,
                    &suites[next],
                    next,
                    &invocation.root,
                )?);
                next += 1;
            }
            let mut index = 0;
            while index < workers.0.len() {
                let worker = &mut workers.0[index];
                if !options.parallel {
                    drain(worker, streams.stdout)?;
                }
                if let Some(child) = worker.child.as_ref() {
                    let exited = crate::cleanup::exited(child)?;
                    let expired = worker.started.elapsed() >= worker.timeout;
                    if exited || expired {
                        let mut child = worker.child.take().expect("observed owned child");
                        let status = crate::cleanup::stop_session(&mut child, libc::SIGTERM)?;
                        worker.status = Some(if !exited && expired {
                            124
                        } else {
                            status
                                .code()
                                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
                        });
                    }
                }
                if worker.status.is_none() {
                    index += 1;
                    continue;
                }
                if !options.parallel {
                    drain(worker, streams.stdout)?;
                }
                let outcome = finish(worker, options)?;
                mark(streams, options, &label(&suites[worker.index]), &outcome)?;
                if !options.parallel && options.ci {
                    writeln!(streams.stdout, "::endgroup::")?;
                }
                outcomes[worker.index] = Some(outcome);
                workers.0.remove(index);
                completed += 1;
            }
            if completed < suites.len() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let mut passed = 0;
        let mut skipped = 0;
        let mut failed = Vec::new();
        for (suite, outcome) in suites.iter().zip(outcomes.iter().flatten()) {
            match outcome.classification {
                SuiteClassification::Pass => passed += 1,
                SuiteClassification::Skip => skipped += 1,
                _ => failed.push(label(suite)),
            }
            if options.parallel
                && (options.verbose
                    || !matches!(
                        outcome.classification,
                        SuiteClassification::Pass | SuiteClassification::Skip
                    ))
            {
                writeln!(streams.stdout)?;
                header(streams.stdout, options, &label(suite), true)?;
                let mut reader = &outcome.output;
                reader.seek(SeekFrom::Start(0))?;
                let length = reader.metadata()?.len();
                copy_interruptible(&mut reader.take(length), streams.stdout)?;
                if options.ci {
                    writeln!(streams.stdout, "::endgroup::")?;
                }
            }
        }
        if !invocation.remove() {
            writeln!(
                streams.stderr,
                "dot test: could not remove temporary directory: {}",
                invocation.root.display()
            )?;
            failed.push("cleanup".into());
        }
        let summary = format_summary(passed, skipped, failed.len() as u64, suites.len());
        let (color, glyph) = if failed.is_empty() {
            ("green", "✓")
        } else {
            ("red", "✗")
        };
        if options.color {
            writeln!(streams.stdout)?;
            crate::ui::summary_box(
                streams.stdout,
                &renderer,
                color,
                &format!("{glyph} {summary}"),
            )
            .map_err(render_error)?;
        } else {
            writeln!(streams.stdout, "{RULE}\n{glyph} {summary}\n{RULE}")?;
        }
        if !failed.is_empty() {
            styled(
                streams.stdout,
                options,
                "red",
                &format!("Failed: {}", failed.join(" ")),
            )?;
        }
        Ok(i32::from(!failed.is_empty()))
    })();
    signals.finish_result(result)
}

fn styled(
    out: &mut dyn std::io::Write,
    options: &Options,
    color: &str,
    text: &str,
) -> std::io::Result<()> {
    if options.color {
        if color == "bold" {
            return writeln!(out, "\x1b[1m{text}\x1b[0m");
        }
        let hex = crate::ui::color_hex(color).map_err(render_error)?;
        let (r, g, b) = crate::ui::hex_to_rgb(&hex).map_err(render_error)?;
        writeln!(out, "\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
    } else {
        writeln!(out, "{text}")
    }
}

fn header(
    out: &mut dyn std::io::Write,
    options: &Options,
    name: &str,
    output: bool,
) -> std::io::Result<()> {
    let suffix = if output { " output" } else { "" };
    if options.ci {
        writeln!(out, "::group::{name}{suffix}")
    } else {
        styled(out, options, "bold", &format!("── {name}{suffix} ──"))
    }
}

fn mark(
    streams: &mut Streams<'_>,
    options: &Options,
    name: &str,
    outcome: &Outcome,
) -> std::io::Result<()> {
    let (color, glyph) = match outcome.classification {
        SuiteClassification::Pass => ("green", "✓"),
        SuiteClassification::Skip => ("yellow", "○"),
        _ => ("red", "✗"),
    };
    write!(streams.stdout, "  ")?;
    if options.color {
        let hex = crate::ui::color_hex(color).map_err(render_error)?;
        let (r, g, b) = crate::ui::hex_to_rgb(&hex).map_err(render_error)?;
        write!(streams.stdout, "\x1b[38;2;{r};{g};{b}m{glyph}\x1b[0m")?;
    } else {
        write!(streams.stdout, "{glyph}")?;
    }
    write!(streams.stdout, " {name} ({}s)", outcome.elapsed)?;
    if outcome.classification == SuiteClassification::Skip {
        let line = outcome
            .result
            .strip_suffix(b"\n")
            .unwrap_or(&outcome.result);
        let detail = line
            .splitn(2, |byte| *byte == b'\t')
            .nth(1)
            .unwrap_or_default();
        let mut detail = detail;
        while detail.first() == Some(&b'\t') {
            detail = &detail[1..];
        }
        while detail.last() == Some(&b'\t') {
            detail = &detail[..detail.len() - 1];
        }
        if !detail.is_empty() {
            streams.stdout.write_all(b": ")?;
            streams.stdout.write_all(detail)?;
        }
    }
    writeln!(streams.stdout)?;
    match outcome.classification {
        SuiteClassification::Incomplete => writeln!(
            streams.stderr,
            "  {name}: completed without a structured result"
        ),
        SuiteClassification::Invalid => writeln!(
            streams.stderr,
            "  {name}: emitted an invalid structured result"
        ),
        _ => Ok(()),
    }
}
