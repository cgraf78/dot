//! Doctor orchestration: the runtime and engine-source checks, the
//! [`Recorder`] every check files through, and one extension run (scratch
//! lifecycle, overlay context, worker seam, and tail records).
//!
//! Neighboring pieces stay with their focused modules: extension discovery
//! and result dispatch (`doctor_coordinator`, `doctor_records`), row
//! rendering and colors (`doctor_runtime`), path abbreviation
//! (`doctor_paths`), the built-in checks (`doctor_checks`), the worker
//! spawn (`hook_worker`), and the production coordinator that sequences
//! all of them (`doctor`).
//!
//! - Results flow as canonical [`Record`] rows (kind, message, detail).
//!   [`Recorder`] keeps the pass/warn/fail counts (`ok`/`warn`/`fail`
//!   count; `skip`, `info`, and `section` do not).
//! - [`Recorder::render`] reproduces the deterministic pipe projection used
//!   by tests. Production renders filed prefixes through
//!   `doctor_runtime::render` with the invocation palette as checks stream.
//! - Text travels as bytes (`&[u8]` / `Vec<u8>`): messages carry paths that
//!   may be non-UTF8.
//! - Worker execution and result-file dispatch arrive as injected seams (the
//!   `worker` hook taking [`WorkerInvocation`] and the `render` hook); the
//!   temp lifecycle, context step, tail records, and cleanup sequencing here
//!   are real.
//!
//! The implementation stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::path::{Path, PathBuf};

pub use crate::doctor_runtime::{Counts, ITEM_LIMIT, Kind, Record};

/// Summary helpers owned by the coordinator lane
/// ([`crate::doctor_coordinator`]), re-exported here so orchestrator
/// callers keep one import path.
pub use crate::doctor_coordinator::{SummaryColor, overall_ok, summary_color, summary_line};

/// Collects [`Record`] rows and counts, mirroring the `_dr_*`
/// helpers' print-plus-count effects without touching stdout.
#[derive(Debug, Clone, Default)]
pub struct Recorder {
    /// Filed rows, in emission order.
    records: Vec<Record>,
    /// Running aggregates.
    counts: Counts,
}

impl Recorder {
    /// An empty recorder, like sourced `runtime.sh` counters at zero.
    pub fn new() -> Self {
        Recorder::default()
    }

    /// Filed rows, in emission order.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Current pass/warn/fail aggregates.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// `_dr_section`: file a section title; counts unchanged.
    pub fn section(&mut self, message: &[u8]) {
        self.push(Kind::Section, message, None);
    }

    /// `_dr_ok`: file a passing check and bump the pass count.
    /// `detail` is `None` for one-argument calls.
    pub fn ok(&mut self, message: &[u8], detail: Option<&[u8]>) {
        self.counts.pass += 1;
        self.push(Kind::Ok, message, detail);
    }

    /// `_dr_warn`: file a warning and bump the warn count.
    /// `detail` is `None` for one-argument calls.
    pub fn warn(&mut self, message: &[u8], detail: Option<&[u8]>) {
        self.counts.warn += 1;
        self.push(Kind::Warn, message, detail);
    }

    /// `_dr_fail`: file a failure and bump the fail count.
    /// `detail` is `None` for one-argument calls.
    pub fn fail(&mut self, message: &[u8], detail: Option<&[u8]>) {
        self.counts.fail += 1;
        self.push(Kind::Fail, message, detail);
    }

    /// `_dr_skip`: file a skipped check; counts unchanged.
    /// `detail` is `None` for one-argument calls.
    pub fn skip(&mut self, message: &[u8], detail: Option<&[u8]>) {
        self.push(Kind::Skip, message, detail);
    }

    /// File an informational row; counts unchanged.
    pub fn info(&mut self, message: &[u8], detail: Option<&[u8]>) {
        self.push(Kind::Info, message, detail);
    }

    /// File one already-built canonical record and update its aggregate.
    pub fn record(&mut self, record: Record) {
        match record.kind {
            Kind::Ok => self.counts.pass += 1,
            Kind::Warn => self.counts.warn += 1,
            Kind::Fail | Kind::Unknown => self.counts.fail += 1,
            Kind::Section | Kind::Skip | Kind::Info => {}
        }
        self.records.push(record);
    }

    /// Push one row without touching the counts.
    fn push(&mut self, kind: Kind, message: &[u8], detail: Option<&[u8]>) {
        self.records.push(Record::bytes(kind, message, detail));
    }

    /// Render every filed row in order using the deterministic pipe
    /// projection: empty palette (no ANSI spans) on one line per
    /// row, warn/fail details on the following indented line —
    /// exactly what the live `_dr_*` helpers print when stdout is
    /// not a terminal. Production renders filed prefixes the same way
    /// through [`crate::doctor_runtime::render`] with the invocation
    /// palette as checks stream.
    pub fn render(&self) -> Vec<u8> {
        crate::doctor_runtime::render(&self.records, &crate::doctor_runtime::Palette::empty())
    }
}

/// Resolved inputs for [`check_runtime`], mirroring the shell locals
/// of `_dr_check_runtime`: the Bash probe, the canonicalized
/// checkout/source roots, the `git --version` line, and what the
/// running build is and how it is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    /// `$BASH_VERSION` verbatim.
    pub bash_version: Vec<u8>,
    /// `${BASH_VERSINFO[0]}` for the Bash 4 gate.
    pub bash_major: u64,
    /// Whether configured user hooks or the Shdeps provider make Bash part of
    /// this invocation's runtime. The native engine itself has no Bash
    /// dependency.
    pub bash_required: bool,
    /// Canonicalized `rev-parse --show-toplevel`, `None` when the
    /// shell would leave `checkout_root` empty.
    pub checkout_root: Option<Vec<u8>>,
    /// Whether the selected source is a packaged native release root.
    pub release_root: bool,
    /// `$DOT_SOURCE_ROOT` verbatim (the fail detail uses this raw).
    pub source_raw: Vec<u8>,
    /// Canonicalized `$DOT_SOURCE_ROOT` (possibly empty on failure,
    /// like the shell's `|| true`).
    pub source_root: Vec<u8>,
    /// `git --version` output, `None` when empty (unavailable).
    pub git_version: Option<Vec<u8>>,
    /// Whether that probe missed its deadline: Git exists but did not
    /// answer (a stalled filesystem or Git process), which warns like every
    /// other stalled core probe instead of failing as a missing Git.
    pub git_stalled: bool,
    /// The Git program the probe ran (resolved from `PATH`), for the
    /// stalled row: a host with several Gits must see which one hung.
    pub git_path: Vec<u8>,
    /// The running build's version stamp (`dot version`).
    pub version: Vec<u8>,
    /// How a release is installed (`Shdeps release`, a standalone install)
    /// when the install-layout check found an owner that can upgrade it;
    /// `None` when that check reports the layout itself as a problem row.
    /// Checkouts ignore it and name their location instead.
    pub install_kind: Option<String>,
    /// The client config file (`$XDG_CONFIG_HOME/dot/config`), which an
    /// unknown key's step names.
    pub config_path: Vec<u8>,
    /// Config keys this release ignored; each becomes a warning so a
    /// typo or a Dot that lags the client repository stays visible.
    pub unknown_config_keys: Vec<crate::config::UnknownKey>,
}

/// `_dr_check_runtime`: file the `dot runtime` section: one row naming the
/// running build, where it runs from, and how it is installed; the Bash gate
/// (`-ge 4`); the Git probe; ignored configuration keys; then the
/// engine-source warnings via [`check_engine_source`]. `home` feeds the
/// display abbreviations.
///
/// A healthy runtime used to take six rows (release exists, engine source,
/// release layout, a constant configuration version, ...) that never
/// changed; they fold into the version row, while every warning and failure
/// keeps a row of its own.
pub fn check_runtime(
    rec: &mut Recorder,
    snapshot: &RuntimeSnapshot,
    engine: &EngineSnapshot,
    home: &[u8],
) {
    rec.section(b"dot runtime");
    let checkout_ok = match &snapshot.checkout_root {
        Some(root) => !root.is_empty() && *root == snapshot.source_root,
        None => false,
    };
    let location = engine_location(engine);
    if snapshot.release_root || checkout_ok {
        // A release reads as its install kind, never as a "checkout": the
        // managed root holds a Shdeps archive or a standalone install.
        let kind = if snapshot.release_root {
            snapshot.install_kind.as_deref().unwrap_or(match location {
                EngineLocation::Managed => "managed install",
                _ => "release",
            })
        } else {
            match location {
                EngineLocation::Development => "development checkout",
                EngineLocation::Managed => "managed checkout",
                EngineLocation::Outside => "checkout",
            }
        };
        let mut message = b"dot ".to_vec();
        message.extend_from_slice(&snapshot.version);
        let mut detail = crate::doctor_paths::tilde_bytes(&snapshot.source_raw, home);
        detail.extend_from_slice(b", ");
        detail.extend_from_slice(kind.as_bytes());
        rec.ok(&message, Some(&detail));
    } else {
        rec.fail(b"dot checkout is unavailable", Some(&snapshot.source_raw));
    }
    if !snapshot.bash_required {
        rec.skip(b"Bash runtime is not required", None);
    } else if snapshot.bash_major >= 4 {
        rec.ok(b"Bash runtime", Some(&snapshot.bash_version));
    } else {
        rec.record(
            Record::fail(
                "Bash runtime is too old",
                Some("Bash 4 or newer is required".to_string()),
            )
            .with_hint("install Bash 4 or newer, or point DOT_BASH at one"),
        );
    }
    match &snapshot.git_version {
        Some(version) => {
            rec.ok(b"Git runtime", Some(version));
        }
        None if snapshot.git_stalled => rec.record(
            Record::warn(
                "Git runtime did not answer",
                Some(format!(
                    "{} --version did not answer within {}s",
                    String::from_utf8_lossy(&snapshot.git_path),
                    crate::doctor_checks::PROBE_TIMEOUT.as_secs()
                )),
            )
            .with_hint(crate::doctor_checks::STALLED_STEP),
        ),
        None => rec.record(Record::fail("Git runtime is unavailable", None).with_hint(
            "install Git or put it on PATH; if it is installed, check that 'git --version' works",
        )),
    }
    for unknown in &snapshot.unknown_config_keys {
        let detail = format!(
            "{} on line {} ({})",
            unknown.key,
            unknown.line,
            unknown.hint()
        );
        // Severity follows `dot update`: a likely misspelling makes every
        // update exit 1 (the meant setting kept its default), so doctor
        // fails on it too; a key from a newer Dot is safe to miss.
        let row = if unknown.degrades_update() {
            Record::fail("unknown configuration key ignored", Some(detail))
        } else {
            Record::warn("unknown configuration key ignored", Some(detail))
        };
        rec.record(row.with_hint(match unknown.suggestion() {
            Some(known) => format!(
                "rename it to '{known}' in {}",
                String::from_utf8_lossy(&crate::doctor_paths::tilde_bytes(
                    &snapshot.config_path,
                    home
                ))
            ),
            None => crate::doctor_checks::UNKNOWN_KEY_STEP.to_string(),
        }));
    }
    check_engine_source(rec, engine);
}

/// Resolved inputs for [`check_engine_source`], mirroring the shell
/// locals of `_dr_check_engine_source`: the raw display spellings
/// plus the physical paths (empty/`None` exactly where the shell
/// leaves them empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSnapshot {
    /// `$DOT_SOURCE_ROOT` verbatim.
    pub source_raw: Vec<u8>,
    /// `${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot`.
    pub managed_raw: Vec<u8>,
    /// `${SHDEPS_GIT_DEV_DIR:-$HOME/git}/dot`.
    pub development_raw: Vec<u8>,
    /// `source` resolved (`cd -P` or the raw fallback).
    pub source_real: Vec<u8>,
    /// `managed` resolved, `None` when absent or unresolvable.
    pub managed_real: Option<Vec<u8>>,
    /// `development` resolved, `None` when absent or unresolvable.
    pub development_real: Option<Vec<u8>>,
    /// `${DOT_IGNORE_DEV_CHECKOUT:-0} == 1`.
    pub ignore_dev_checkout: bool,
}

/// Where the running engine lives, relative to the two locations Dot's
/// provider manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineLocation {
    /// The development checkout (`${SHDEPS_GIT_DEV_DIR:-$HOME/git}/dot`).
    Development,
    /// The managed install (`${SHDEPS_INSTALL_DIR:-...}/cgraf78/dot`).
    Managed,
    /// Anywhere else (a test checkout, an unpacked archive).
    Outside,
}

/// Classify the engine source physically. The development checkout wins
/// when both resolve to it, as it always has.
pub fn engine_location(snapshot: &EngineSnapshot) -> EngineLocation {
    let resolves = |real: &Option<Vec<u8>>| {
        real.as_ref()
            .is_some_and(|real| !real.is_empty() && *real == snapshot.source_real)
    };
    if resolves(&snapshot.development_real) {
        EngineLocation::Development
    } else if resolves(&snapshot.managed_real) {
        EngineLocation::Managed
    } else {
        EngineLocation::Outside
    }
}

/// `_dr_check_engine_source`: file the bypass notice when enabled, and warn
/// when the engine source is outside both managed locations (a warning,
/// never a failure — repository test checkouts must stay green). A managed
/// or development source has no row of its own: [`check_runtime`] names it
/// in the version row.
pub fn check_engine_source(rec: &mut Recorder, snapshot: &EngineSnapshot) {
    if snapshot.ignore_dev_checkout {
        rec.record(
            Record::warn(
                "development checkout bypass enabled",
                Some("the provider will use the managed checkout for this invocation".to_string()),
            )
            .with_hint("unset DOT_IGNORE_DEV_CHECKOUT to use the development checkout again"),
        );
    }
    if engine_location(snapshot) == EngineLocation::Outside {
        rec.warn(
            b"dot engine source is outside managed locations",
            Some(&snapshot.source_real),
        );
    }
}

/// Counter for unique doctor scratch directories (see
/// [`make_temp_dir`]).
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `$temporary/results` and `$temporary/output`, mirroring the
/// `result=` / `log=` locals of `_dot_doctor_run_extension`.
pub fn result_paths(temporary: &Path) -> (PathBuf, PathBuf) {
    (temporary.join("results"), temporary.join("output"))
}

/// Four random bytes for scratch names, like the `od` read behind
/// the overlay token helper; the pid plus [`TEMP_COUNTER`] keep
/// names unique even when urandom is unavailable.
fn random_suffix() -> u32 {
    use std::io::Read as _;
    let mut bytes = [0u8; 4];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if read.is_ok() {
        u32::from_le_bytes(bytes)
    } else {
        0
    }
}

/// Allocate one doctor scratch directory: a fresh `0700` directory
/// `<root>/dot.<pid>.<n>.<rand>` under a caller-captured temp root, so
/// embedded Runtime invocations never fall through to the parent process
/// environment. Creation races retry; other failures surface.
pub(crate) fn make_temp_dir_in(root: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::sync::atomic::Ordering;
    let pid = std::process::id();
    for _ in 0..100 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = root.join(format!("dot.{pid}.{n}.{:08x}", random_suffix()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "doctor temporary directory unavailable",
    ))
}

/// `: >"$result"` plus `chmod 0600`, mirroring the result-file
/// setup: truncate-or-create, then force the private mode whatever
/// the umask says. Callers ignore failures, like the shell (no
/// status check on either line).
pub fn create_result_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The captured output's non-empty lines, each one row item, so a
/// multi-line error stays readable instead of collapsing into one line.
pub fn log_lines(log: &[u8]) -> Vec<Vec<u8>> {
    log.split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Where the worker leaves its failure note: next to the result file, in the
/// extension's private scratch directory (`worker.sh` derives the same path
/// as `$result.failure`).
pub fn failure_path(result: &Path) -> PathBuf {
    let mut path = result.as_os_str().to_os_string();
    path.push(".failure");
    PathBuf::from(path)
}

/// The failing command the worker's ERR trap saw as the extension exited
/// nonzero on it: its status, whether the file was still loading, where it
/// ran (`doctor.d/<file>:<line>`), and its command text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureNote {
    /// Status of the failing command.
    pub status: i32,
    /// The extension file was still being sourced, before `doctor` ran.
    pub loading: bool,
    /// `<path>:<line>` relative to the extension root; empty when the
    /// failure surfaced at the worker's top level (the file failed to load,
    /// or `doctor` itself returned the status).
    pub location: Vec<u8>,
    /// The failing command as Bash reports it (`$BASH_COMMAND`), or the
    /// public helper that failed inside Dot's runtime; may be empty.
    pub command: Vec<u8>,
}

/// Bound on the command text a note shows: enough for any ordinary line,
/// while a here-document or a long generated command cannot flood the row.
const NOTE_COMMAND_LIMIT: usize = 200;

impl FailureNote {
    /// Parse the worker's `status NUL phase NUL location NUL command` note
    /// (phase `load` or `run`). Anything else (a truncated write, an unknown
    /// shape) is no note at all: the row then shows only the exit status.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let mut fields = bytes.splitn(4, |byte| *byte == 0);
        let status = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
        let loading = match fields.next()? {
            b"load" => true,
            b"run" => false,
            _ => return None,
        };
        let location = fields.next()?.to_vec();
        let command = fields.next()?;
        Some(FailureNote {
            status,
            loading,
            location,
            command: display_command(command),
        })
    }

    /// Read the note a worker left for `result`, if any.
    pub fn read(result: &Path) -> Option<Self> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        std::fs::File::open(failure_path(result))
            .and_then(|file| file.take(4096).read_to_end(&mut bytes))
            .ok()?;
        Self::parse(&bytes)
    }
}

/// One display line for a command: control bytes (a multi-line command's
/// newlines, tabs) become spaces, and a long command is cut on a character
/// boundary with an ellipsis.
fn display_command(command: &[u8]) -> Vec<u8> {
    let mut line: Vec<u8> = command
        .iter()
        .map(|byte| if byte.is_ascii_control() { b' ' } else { *byte })
        .collect();
    if line.len() > NOTE_COMMAND_LIMIT {
        let mut cut = NOTE_COMMAND_LIMIT;
        while cut > 0 && line[cut] & 0xC0 == 0x80 {
            cut -= 1;
        }
        line.truncate(cut);
        line.extend_from_slice("…".as_bytes());
    }
    line
}

/// `exited with status N`, plus where the extension stopped when the
/// worker's note matches that status: `at <file>:<line>: <command>`; for a
/// failure surfacing at the worker's top level, `while loading the
/// extension file`, or the last command when `doctor` itself returned the
/// status. The worker drops a note that is not about the exiting command;
/// a status mismatch is a second guard against pointing at the wrong line.
fn exit_detail(rc: i32, note: Option<&FailureNote>) -> Vec<u8> {
    let mut detail = format!("exited with status {rc}").into_bytes();
    let Some(note) = note.filter(|note| note.status == rc) else {
        return detail;
    };
    if !note.location.is_empty() {
        detail.extend_from_slice(b" at ");
        detail.extend_from_slice(&note.location);
        if !note.command.is_empty() {
            detail.extend_from_slice(b": ");
            detail.extend_from_slice(&note.command);
        }
    } else if note.loading {
        detail.extend_from_slice(b" while loading the extension file");
    } else if !note.command.is_empty() {
        detail.extend_from_slice(b"; last command: ");
        detail.extend_from_slice(&note.command);
    }
    detail
}

/// The items for a stopped extension's captured output: the last
/// [`ITEM_LIMIT`] lines, because the line that explains a crash (a tool's
/// final error, Bash's own diagnostic) comes last. Also returns how many
/// earlier lines were left out.
fn output_tail(log: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let mut lines = log_lines(log);
    let omitted = lines.len().saturating_sub(ITEM_LIMIT);
    lines.drain(..omitted);
    (lines, omitted)
}

/// The tail of one extension run: a worker stopped at its deadline files
/// `<key> doctor extension timed out`, any other nonzero status files
/// `<key> doctor extension failed` with the exit status and, from `note`,
/// the failing line; stray log output files `<key> doctor extension wrote
/// outside the result API`, and a quiet success files nothing. Captured
/// output lines become the row's items, one per line: the last few for a
/// stopped extension, all of them (folded by the renderer) for stray output.
pub fn extension_tail(
    rec: &mut Recorder,
    key: &[u8],
    exit: WorkerExit,
    log: &[u8],
    note: Option<&FailureNote>,
) {
    let mut message = key.to_vec();
    if exit.timed_out.is_none() && exit.rc == 0 {
        if log.is_empty() {
            return;
        }
        message.extend_from_slice(b" doctor extension wrote outside the result API");
        let items = log_lines(log);
        let detail = items.is_empty().then_some(b"blank lines only".as_slice());
        let mut record = Record::bytes(Kind::Warn, &message, detail);
        record.items = items;
        rec.record(record);
        return;
    }
    let (items, omitted) = output_tail(log);
    let mut detail = match exit.timed_out {
        Some(limit) => {
            message.extend_from_slice(b" doctor extension timed out");
            format!("stopped after {}s", limit.as_secs()).into_bytes()
        }
        None => {
            message.extend_from_slice(b" doctor extension failed");
            exit_detail(exit.rc, note)
        }
    };
    if omitted > 0 {
        detail.extend_from_slice(format!("; last {ITEM_LIMIT} output lines shown").as_bytes());
    }
    let mut record = Record::bytes(Kind::Fail, &message, Some(&detail));
    record.items = items;
    if exit.timed_out.is_some() {
        record = record.with_hint("set DOT_DOCTOR_TIMEOUT to raise the limit");
    } else {
        // The extension ships with a dotfiles repository, which owns the
        // fix; a newer version may already be published.
        let at = note
            .filter(|note| note.status == exit.rc && !note.location.is_empty())
            .map(|note| String::from_utf8_lossy(&note.location).into_owned());
        let what = at.unwrap_or_else(|| "it".to_string());
        record = record.with_hint(format!(
            "fix {what}, or run 'dot update' if its repository has a fix; then rerun 'dot doctor'"
        ));
    }
    rec.record(record);
}

/// How one extension worker ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerExit {
    /// Worker status (1 when it was stopped or could not run).
    pub rc: i32,
    /// The per-extension deadline that stopped the worker, if it did.
    pub timed_out: Option<std::time::Duration>,
}

impl From<i32> for WorkerExit {
    /// A worker that ended on its own with `rc`.
    fn from(rc: i32) -> Self {
        WorkerExit {
            rc,
            timed_out: None,
        }
    }
}

/// What the worker seam receives: the exact argument positions of
/// `_dot_extension_worker_exec doctor script temporary result
/// context token`, plus the log file the redirection owns.
#[derive(Debug)]
pub struct WorkerInvocation<'a> {
    /// The extension script (`$2`).
    pub script: &'a Path,
    /// The scratch directory (`$3`, also the worker `TMPDIR`).
    pub temporary: &'a Path,
    /// The result file (`$4`, the record channel).
    pub result: &'a Path,
    /// The overlay context file (`$5`).
    pub context: &'a Path,
    /// The context token (`$6`).
    pub token: &'a str,
    /// The captured stdout/stderr file (`>"$log" 2>&1`).
    pub log: &'a Path,
}

/// Create a doctor worker context from one invocation's immutable Runtime
/// values instead of process-global environment.
pub fn create_context_for(
    temporary: &Path,
    overlays: &[Vec<u8>],
    home: &str,
    euid: u32,
    now_secs: i64,
) -> Option<(PathBuf, String)> {
    crate::overlay_context::create(
        temporary, "doctor", "active", "none", overlays, home, euid, now_secs,
    )
    .ok()
}

/// Run one extension end to end: allocate scratch, create the result file,
/// build the overlay context from the invocation's identity (never
/// process-global state), run the worker through `worker`, dispatch the
/// filed rows through `render`, file the tail record, remove the scratch
/// directory, and return the worker status (1 for allocation failures).
#[allow(clippy::too_many_arguments)]
pub fn run_extension_for(
    rec: &mut Recorder,
    key: &[u8],
    script: &Path,
    overlays: &[Vec<u8>],
    home: &str,
    euid: u32,
    now_secs: i64,
    temporary_root: &Path,
    worker: &mut dyn FnMut(&WorkerInvocation<'_>) -> WorkerExit,
    render: &mut dyn FnMut(&Path, &mut Recorder),
) -> i32 {
    let mut context =
        |temporary: &Path| create_context_for(temporary, overlays, home, euid, now_secs);
    run_extension_with_context(
        rec,
        key,
        script,
        temporary_root,
        worker,
        render,
        &mut context,
    )
}

fn run_extension_with_context(
    rec: &mut Recorder,
    key: &[u8],
    script: &Path,
    temporary_root: &Path,
    worker: &mut dyn FnMut(&WorkerInvocation<'_>) -> WorkerExit,
    render: &mut dyn FnMut(&Path, &mut Recorder),
    context: &mut dyn FnMut(&Path) -> Option<(PathBuf, String)>,
) -> i32 {
    let outcome = execute_extension_with_context(key, script, temporary_root, worker, context);
    record_extension(rec, outcome, render)
}

/// Owns one extension's scratch directory and removes it when dropped.
///
/// Parallel dispatch separates execution from ordered rendering, so the
/// directory can outlive the worker thread. Tying removal to `Drop` keeps
/// cleanup idempotent on every path: a normal render, a discarded outcome
/// after cancellation, and a worker thread that unwinds before rendering.
#[derive(Debug)]
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
enum OutcomeState {
    TemporaryUnavailable,
    ContextUnavailable,
    Ran {
        scratch: Scratch,
        result: PathBuf,
        log: PathBuf,
        exit: WorkerExit,
    },
}

/// One executed doctor extension whose records have not been filed yet.
///
/// Execution touches only the extension's private scratch directory and
/// never the shared [`Recorder`], so outcomes may be produced concurrently
/// and filed later, strictly in discovery order, by [`record_extension`].
/// Dropping an unrecorded outcome discards its records and scratch state.
#[derive(Debug)]
pub(crate) struct ExtensionOutcome {
    key: Vec<u8>,
    state: OutcomeState,
}

/// Run one extension's worker against a private scratch directory without
/// touching the recorder: allocate scratch, create the result file, build the
/// overlay context, and run `worker`. Allocation failures are captured in the
/// outcome and reported, in order, by [`record_extension`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_extension_for(
    key: &[u8],
    script: &Path,
    overlays: &[Vec<u8>],
    home: &str,
    euid: u32,
    now_secs: i64,
    temporary_root: &Path,
    worker: &mut dyn FnMut(&WorkerInvocation<'_>) -> WorkerExit,
) -> ExtensionOutcome {
    let mut context =
        |temporary: &Path| create_context_for(temporary, overlays, home, euid, now_secs);
    execute_extension_with_context(key, script, temporary_root, worker, &mut context)
}

fn execute_extension_with_context(
    key: &[u8],
    script: &Path,
    temporary_root: &Path,
    worker: &mut dyn FnMut(&WorkerInvocation<'_>) -> WorkerExit,
    context: &mut dyn FnMut(&Path) -> Option<(PathBuf, String)>,
) -> ExtensionOutcome {
    let outcome = |state| ExtensionOutcome {
        key: key.to_vec(),
        state,
    };
    let scratch = match make_temp_dir_in(temporary_root) {
        Ok(dir) => Scratch(dir),
        Err(_) => return outcome(OutcomeState::TemporaryUnavailable),
    };
    let (result, log) = result_paths(&scratch.0);
    let _ = create_result_file(&result);
    let (context, token) = match context(&scratch.0) {
        Some(pair) => pair,
        // Dropping `scratch` removes the directory before the failure is
        // reported, as the serial shell loop did.
        None => return outcome(OutcomeState::ContextUnavailable),
    };
    let invocation = WorkerInvocation {
        script,
        temporary: &scratch.0,
        result: &result,
        context: &context,
        token: &token,
        log: &log,
    };
    let exit = worker(&invocation);
    outcome(OutcomeState::Ran {
        scratch,
        result,
        log,
        exit,
    })
}

/// File one executed extension's records, then its failure or stray-output
/// tail, exactly as the serial loop did immediately after the worker
/// returned. Returns the worker status (1 for allocation failures).
pub(crate) fn record_extension(
    rec: &mut Recorder,
    outcome: ExtensionOutcome,
    render: &mut dyn FnMut(&Path, &mut Recorder),
) -> i32 {
    let key = outcome.key;
    match outcome.state {
        OutcomeState::TemporaryUnavailable => {
            let mut message = key;
            message.extend_from_slice(b" doctor extension temporary directory unavailable");
            rec.record(
                Record::bytes(Kind::Fail, &message, None)
                    .with_hint("check TMPDIR permissions and free space"),
            );
            1
        }
        OutcomeState::ContextUnavailable => {
            let mut message = key;
            message.extend_from_slice(b" doctor extension context unavailable");
            rec.fail(&message, None);
            1
        }
        OutcomeState::Ran {
            scratch,
            result,
            log,
            exit,
        } => {
            render(&result, rec);
            let log_bytes = std::fs::read(&log).unwrap_or_default();
            let note = (exit.rc != 0).then(|| FailureNote::read(&result)).flatten();
            extension_tail(rec, &key, exit, &log_bytes, note.as_ref());
            drop(scratch);
            exit.rc
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_keep_each_non_empty_line() {
        assert!(log_lines(b"").is_empty());
        assert_eq!(log_lines(b"a\n\nb\r\n"), vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(log_lines(b"tail"), vec![b"tail".to_vec()]);
    }

    fn execute_in(root: &Path, key: &[u8], log: &'static [u8]) -> (ExtensionOutcome, PathBuf) {
        let seen = std::cell::RefCell::new(PathBuf::new());
        let mut worker = |call: &WorkerInvocation<'_>| {
            *seen.borrow_mut() = call.temporary.to_path_buf();
            std::fs::write(call.result, b"").expect("result");
            std::fs::write(call.log, log).expect("log");
            WorkerExit::from(0)
        };
        let mut context = |temporary: &Path| Some((temporary.join("context"), "token".into()));
        let outcome = execute_extension_with_context(
            key,
            Path::new("/fixture/extension.sh"),
            root,
            &mut worker,
            &mut context,
        );
        let scratch = seen.into_inner();
        (outcome, scratch)
    }

    #[test]
    fn discarded_outcome_removes_scratch_without_recording() {
        let root = dot_test_support::TempDir::new("doctor-discard-outcome").expect("root");
        let (outcome, scratch) = execute_in(root.path(), b"demo", b"");
        assert!(scratch.is_dir(), "scratch must survive until recording");
        drop(outcome);
        assert!(!scratch.exists(), "discarded outcome leaked scratch");
    }

    #[test]
    fn outcomes_record_in_caller_order_and_remove_scratch() {
        let root = dot_test_support::TempDir::new("doctor-ordered-outcome").expect("root");
        let (first, first_scratch) = execute_in(root.path(), b"first", b"one\n");
        let (second, second_scratch) = execute_in(root.path(), b"second", b"two\n");
        let mut rec = Recorder::new();
        let mut render = |_: &Path, _: &mut Recorder| {};
        // Execution order is irrelevant: filing follows the caller's order.
        assert_eq!(record_extension(&mut rec, second, &mut render), 0);
        assert_eq!(record_extension(&mut rec, first, &mut render), 0);
        let rendered = String::from_utf8(rec.render()).expect("utf8");
        let second_at = rendered.find("second doctor extension").expect("second");
        let first_at = rendered.find("first doctor extension").expect("first");
        assert!(second_at < first_at, "{rendered}");
        assert!(!first_scratch.exists() && !second_scratch.exists());
    }

    #[test]
    fn allocation_failures_record_when_filed() {
        let root = dot_test_support::TempDir::new("doctor-failed-outcome").expect("root");
        let missing = root.path().join("missing/nested");
        let mut worker = |_: &WorkerInvocation<'_>| panic!("worker must not run");
        let mut context = |_: &Path| None;
        let unavailable = execute_extension_with_context(
            b"gone",
            Path::new("/fixture/extension.sh"),
            &missing,
            &mut worker,
            &mut context,
        );
        let no_context = execute_extension_with_context(
            b"lost",
            Path::new("/fixture/extension.sh"),
            root.path(),
            &mut worker,
            &mut context,
        );
        assert_eq!(
            std::fs::read_dir(root.path()).expect("root").count(),
            0,
            "context failure must remove its scratch before filing"
        );
        let mut rec = Recorder::new();
        let mut render = |_: &Path, _: &mut Recorder| {};
        assert!(rec.render().is_empty(), "execution must not record");
        assert_eq!(record_extension(&mut rec, unavailable, &mut render), 1);
        assert_eq!(record_extension(&mut rec, no_context, &mut render), 1);
        let rendered = String::from_utf8(rec.render()).expect("utf8");
        assert!(rendered.contains("gone doctor extension temporary directory unavailable"));
        assert!(rendered.contains("lost doctor extension context unavailable"));
        assert_eq!(rec.counts().fail, 2);
    }

    #[test]
    fn summary_helpers_match_shell_rules() {
        assert_eq!(summary_line(6, 1, 2), "6 passed · 1 warnings · 2 failed");
        assert_eq!(summary_color(1, 0), SummaryColor::Red);
        assert_eq!(summary_color(0, 3), SummaryColor::Yellow);
        assert_eq!(summary_color(0, 0), SummaryColor::Green);
        assert_eq!(summary_color(2, 5).name(), "red");
        assert!(!overall_ok(1, 0));
        assert!(!overall_ok(0, 1));
        assert!(overall_ok(0, 0));
    }
}
