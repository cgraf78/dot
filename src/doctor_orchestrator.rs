//! Doctor orchestration (`lib/dot/doctor.sh`): the load boundary,
//! the runtime and engine-source checks, one extension run, and the
//! `_dot_doctor` coordinator.
//!
//! Ports exactly five functions and nothing else: `_dot_doctor_load`
//! (~line 17), `_dr_check_runtime` (~line 36),
//! `_dr_check_engine_source` (~line 62),
//! `_dot_doctor_run_extension` (~line 135), and `_dot_doctor`
//! (~line 179). The neighboring pieces stay with their own lanes:
//! `_dot_doctor_extension_specs` and `_dot_doctor_render_records`
//! (discovery and result dispatch), the `_dr_*` color/tty rendering
//! (`doctor_runtime` lane), `_dr_tilde` (`doctor_paths` lane), the
//! kernel checks (`repos`, `lock`, `provider`, `overlays`, `merges`
//! lanes), and the worker spawn (`extension-worker` lane).
//!
//! Parity decisions:
//! - Results flow as [`Record`] rows (kind, message, detail) with
//!   shell `$#` arity: `detail` is `None` for one-argument calls and
//!   `Some` (possibly empty) for two-argument calls. [`Recorder`]
//!   mirrors the `_DR_*_COUNT` effects (`ok`/`warn`/`fail` count,
//!   `skip`/`section` do not).
//! - [`Recorder::render`] reproduces only the deterministic pipe
//!   projection (empty palette, non-tty): the differential tests run
//!   the live shell under pipes with a gum-free `PATH`, so colors
//!   are empty and `dot_ui_title` / `dot_ui_summary_box` take their
//!   plain branches. Color/tty/gum styling stays with the
//!   `doctor_runtime` and `ui` lanes.
//! - Text travels as bytes (`&[u8]` / `Vec<u8]`): messages carry
//!   paths that may be non-UTF8, and `tr` / `printf` copy bytes
//!   verbatim.
//! - Sourcing itself is not portable: [`Loader`] models the
//!   `_DOT_DOCTOR_LOADED` idempotence plus the ordered section path
//!   list and a presence check, never the `.` builtin.
//! - Worker execution and result-file dispatch arrive as injected
//!   seams (the `worker` hook taking [`WorkerInvocation`] and the
//!   `render` hook): the worker spawn and the record-file parser
//!   belong to other lanes, but
//!   the temp lifecycle, context step, tail records, and cleanup
//!   sequencing here are real.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::path::{Path, PathBuf};

/// One doctor result row, mirroring a single `_dr_*` call.
///
/// `detail` mirrors the shell `$#` arity: `None` renders no
/// trailer (one-argument call), while `Some` — even empty —
/// renders the trailer (two-argument call), exactly like
/// `[[ $# -gt 1 ]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Which `_dr_*` helper filed this row.
    pub kind: Kind,
    /// The message verbatim (`$1`), as bytes.
    pub message: Vec<u8>,
    /// The detail verbatim (`$2`), or `None` for one-argument calls.
    pub detail: Option<Vec<u8>>,
}

/// The `_dr_*` helper family a [`Record`] was filed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `_dr_section`: a section title, never counted.
    Section,
    /// `_dr_ok`: a passing check, bumps the pass count.
    Ok,
    /// `_dr_warn`: a warning, bumps the warn count.
    Warn,
    /// `_dr_fail`: a failure, bumps the fail count.
    Fail,
    /// `_dr_skip`: a skipped check, never counted.
    Skip,
}

/// The `_DR_*_COUNT` aggregates: section modules report through
/// [`Recorder::ok`], [`Recorder::warn`], and [`Recorder::fail`];
/// [`Recorder::skip`] and [`Recorder::section`] leave the counts
/// alone, exactly like the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// `_DR_PASS_COUNT`, incremented by `ok`.
    pub pass: u64,
    /// `_DR_WARN_COUNT`, incremented by `warn`.
    pub warn: u64,
    /// `_DR_FAIL_COUNT`, incremented by `fail`.
    pub fail: u64,
}

impl Counts {
    /// Zeroed counters, like a freshly sourced `runtime.sh`.
    pub fn new() -> Self {
        Counts::default()
    }
}

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

    /// Push one row without touching the counts.
    fn push(&mut self, kind: Kind, message: &[u8], detail: Option<&[u8]>) {
        self.records.push(Record {
            kind,
            message: message.to_vec(),
            detail: detail.map(<[u8]>::to_vec),
        });
    }

    /// Render every filed row in order using the deterministic pipe
    /// projection: empty palette (no ANSI spans) on one line per
    /// row, warn/fail details on the following indented line —
    /// exactly what the live `_dr_*` helpers print when stdout is
    /// not a terminal.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for record in &self.records {
            render_record(&mut out, record);
        }
        out
    }
}

/// The seven section files `_dot_doctor_load` sources, in order:
/// `runtime.sh`, `paths.sh`, `repos.sh`, `lock.sh`, `provider.sh`,
/// `overlays.sh`, `merges.sh`.
pub const SECTION_FILES: [&str; 7] = [
    "runtime.sh",
    "paths.sh",
    "repos.sh",
    "lock.sh",
    "provider.sh",
    "overlays.sh",
    "merges.sh",
];

/// The ordered section paths for `doctor_dir`
/// (`$_DOT_DOCTOR_DIR`), mirroring the seven `.` lines: sourcing
/// itself stays with the shell, so the port publishes the path
/// list the loader consumes.
pub fn section_paths(doctor_dir: &Path) -> [PathBuf; 7] {
    [
        doctor_dir.join(SECTION_FILES[0]),
        doctor_dir.join(SECTION_FILES[1]),
        doctor_dir.join(SECTION_FILES[2]),
        doctor_dir.join(SECTION_FILES[3]),
        doctor_dir.join(SECTION_FILES[4]),
        doctor_dir.join(SECTION_FILES[5]),
        doctor_dir.join(SECTION_FILES[6]),
    ]
}

/// True when every section file exists as a regular file under
/// `doctor_dir`, so a loader failure surfaces before any `.`
/// line runs.
pub fn sections_present(doctor_dir: &Path) -> bool {
    section_paths(doctor_dir).iter().all(|path| path.is_file())
}

/// Models `_DOT_DOCTOR_LOADED`: the first [`Loader::load`] publishes
/// the section paths and marks the loader; later calls are a no-op
/// returning `None`, like `[[ $_DOT_DOCTOR_LOADED -eq 0 ]] || return 0`.
#[derive(Debug, Clone, Default)]
pub struct Loader {
    /// Whether the section paths were already published.
    loaded: bool,
}

impl Loader {
    /// A fresh loader, like `_DOT_DOCTOR_LOADED=0` at source time.
    pub fn new() -> Self {
        Loader::default()
    }

    /// Whether [`Loader::load`] already published once.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Publish the ordered section paths for `doctor_dir` on the
    /// first call and mark the loader; return `None` afterwards.
    pub fn load(&mut self, doctor_dir: &Path) -> Option<[PathBuf; 7]> {
        if self.loaded {
            return None;
        }
        self.loaded = true;
        Some(section_paths(doctor_dir))
    }
}

/// `cd -P -- path && pwd -P || true` for directories: the physical
/// path with every component resolved, or `None` when the shell
/// would print nothing (empty input, missing path, non-directory,
/// or an unresolvable chain).
pub fn physical_dir(path: &[u8]) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;
    if path.is_empty() {
        return None;
    }
    let candidate = Path::new(std::ffi::OsStr::from_bytes(path));
    let resolved = std::fs::canonicalize(candidate).ok()?;
    if !resolved.is_dir() {
        return None;
    }
    Some(resolved.as_os_str().as_bytes().to_vec())
}

/// `_dr_tilde` (owned by the `doctor_paths` lane, mirrored here only
/// as the display effect the runtime and engine-source checks call):
/// `$HOME` becomes `~`, `$HOME/...` becomes `~/...`, everything
/// else passes through verbatim.
fn tilde(path: &[u8], home: &[u8]) -> Vec<u8> {
    if path == home {
        return b"~".to_vec();
    }
    if home.is_empty() {
        // `"$HOME"/*` with an empty HOME is the `/*` pattern, and
        // `${p#"$HOME"/}` strips the leading slash, so absolute
        // paths still abbreviate.
        if let Some(rest) = path.strip_prefix(b"/") {
            let mut out = b"~/".to_vec();
            out.extend_from_slice(rest);
            return out;
        }
        return path.to_vec();
    }
    let mut prefix = home.to_vec();
    prefix.push(b'/');
    if let Some(rest) = path.strip_prefix(prefix.as_slice()) {
        let mut out = b"~/".to_vec();
        out.extend_from_slice(rest);
        return out;
    }
    path.to_vec()
}

/// Render one row under the pipe projection (see
/// [`Recorder::render`]).
fn render_record(out: &mut Vec<u8>, record: &Record) {
    match record.kind {
        Kind::Section => {
            out.push(b'\n');
            out.extend_from_slice(&record.message);
            out.push(b'\n');
        }
        Kind::Ok | Kind::Skip => {
            out.extend_from_slice(if record.kind == Kind::Ok {
                "  ✓ ".as_bytes()
            } else {
                "  · ".as_bytes()
            });
            out.extend_from_slice(&record.message);
            if let Some(detail) = &record.detail {
                out.extend_from_slice(b" (");
                out.extend_from_slice(detail);
                out.push(b')');
            }
            out.push(b'\n');
        }
        Kind::Warn | Kind::Fail => {
            out.extend_from_slice(if record.kind == Kind::Warn {
                "  ⚠ ".as_bytes()
            } else {
                "  ✗ ".as_bytes()
            });
            out.extend_from_slice(&record.message);
            out.push(b'\n');
            if let Some(detail) = &record.detail {
                out.extend_from_slice(b"    ");
                out.extend_from_slice(detail);
                out.push(b'\n');
            }
        }
    }
}

/// Resolved inputs for [`check_runtime`], mirroring the shell locals
/// of `_dr_check_runtime`: the Bash probe, the canonicalized
/// checkout/source roots, the `git --version` line, and the
/// defaulted configuration version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    /// `$BASH_VERSION` verbatim.
    pub bash_version: Vec<u8>,
    /// `${BASH_VERSINFO[0]}` for the Bash 4 gate.
    pub bash_major: u64,
    /// Canonicalized `rev-parse --show-toplevel`, `None` when the
    /// shell would leave `checkout_root` empty.
    pub checkout_root: Option<Vec<u8>>,
    /// `$DOT_SOURCE_ROOT` verbatim (the fail detail uses this raw).
    pub source_raw: Vec<u8>,
    /// Canonicalized `$DOT_SOURCE_ROOT` (possibly empty on failure,
    /// like the shell's `|| true`).
    pub source_root: Vec<u8>,
    /// `git --version` output, `None` when empty (unavailable).
    pub git_version: Option<Vec<u8>>,
    /// `${DOT_CONFIG_VERSION:-1}`, already defaulted by the caller.
    pub config_version: Vec<u8>,
}

/// `_dr_check_runtime`: file the `dot runtime` section, the Bash
/// gate (`-ge 4`), the checkout comparison, the Git probe, and the
/// configuration version, then the engine-source tail via
/// [`check_engine_source`], like the trailing
/// `_dr_check_engine_source` call. `home` feeds the display
/// abbreviations.
pub fn check_runtime(
    rec: &mut Recorder,
    snapshot: &RuntimeSnapshot,
    engine: &EngineSnapshot,
    home: &[u8],
) {
    rec.section(b"dot runtime");
    if snapshot.bash_major >= 4 {
        rec.ok(b"Bash runtime", Some(&snapshot.bash_version));
    } else {
        rec.fail(
            b"Bash runtime is too old",
            Some(b"Bash 4 or newer is required"),
        );
    }
    let checkout_ok = match &snapshot.checkout_root {
        Some(root) => !root.is_empty() && *root == snapshot.source_root,
        None => false,
    };
    if checkout_ok {
        let display = tilde(&snapshot.source_raw, home);
        rec.ok(b"dot checkout exists", Some(&display));
    } else {
        rec.fail(b"dot checkout is unavailable", Some(&snapshot.source_raw));
    }
    match &snapshot.git_version {
        Some(version) => {
            rec.ok(b"Git runtime", Some(version));
        }
        None => {
            rec.fail(b"Git runtime is unavailable", None);
        }
    }
    rec.ok(b"configuration version", Some(&snapshot.config_version));
    check_engine_source(rec, engine, home);
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

impl EngineSnapshot {
    /// Resolve an [`EngineSnapshot`] from the process environment,
    /// mirroring the shell defaulting and `cd -P` probes:
    /// `SHDEPS_INSTALL_DIR` / `SHDEPS_GIT_DEV_DIR` fall back to
    /// `$HOME/.local/share` / `$HOME/git` when unset or empty
    /// (`${var:-default}`), and unresolvable directories resolve to
    /// `None` (the source keeps its raw fallback, like the shell's
    /// `|| source_real=$source`).
    pub fn from_env(source_raw: &[u8], home: &[u8]) -> Self {
        let base = |name: &str, fallback_leaf: &[u8]| -> Vec<u8> {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    use std::os::unix::ffi::OsStrExt as _;
                    value.as_os_str().as_bytes().to_vec()
                })
                .unwrap_or_else(|| {
                    let mut root = home.to_vec();
                    root.extend_from_slice(fallback_leaf);
                    root
                })
        };
        let mut managed_raw = base("SHDEPS_INSTALL_DIR", b"/.local/share");
        managed_raw.extend_from_slice(b"/cgraf78/dot");
        let mut development_raw = base("SHDEPS_GIT_DEV_DIR", b"/git");
        development_raw.extend_from_slice(b"/dot");
        let source_real = physical_dir(source_raw).unwrap_or_else(|| source_raw.to_vec());
        let ignore_dev_checkout =
            std::env::var_os("DOT_IGNORE_DEV_CHECKOUT").is_some_and(|value| value == "1");
        EngineSnapshot {
            source_raw: source_raw.to_vec(),
            source_real,
            managed_real: physical_dir(&managed_raw),
            development_real: physical_dir(&development_raw),
            managed_raw,
            development_raw,
            ignore_dev_checkout,
        }
    }
}

/// `_dr_check_engine_source`: file the bypass notice when enabled,
/// then classify the engine source as a development checkout, a
/// managed checkout, or an outside location (a warning, never a
/// failure — repository test checkouts must stay green). `home`
/// feeds the checkout display abbreviation.
pub fn check_engine_source(rec: &mut Recorder, snapshot: &EngineSnapshot, home: &[u8]) {
    if snapshot.ignore_dev_checkout {
        rec.warn(
            b"development checkout bypass enabled",
            Some(b"the provider will use the managed checkout for this invocation"),
        );
    }
    let development_ok = match &snapshot.development_real {
        Some(real) => !real.is_empty() && *real == snapshot.source_real,
        None => false,
    };
    if development_ok {
        let mut detail = b"development checkout: ".to_vec();
        detail.extend_from_slice(&tilde(&snapshot.development_raw, home));
        rec.ok(b"dot engine source", Some(&detail));
        return;
    }
    let managed_ok = match &snapshot.managed_real {
        Some(real) => !real.is_empty() && *real == snapshot.source_real,
        None => false,
    };
    if managed_ok {
        let mut detail = b"managed checkout: ".to_vec();
        detail.extend_from_slice(&tilde(&snapshot.managed_raw, home));
        rec.ok(b"dot engine source", Some(&detail));
        return;
    }
    rec.warn(
        b"dot engine source is outside managed locations",
        Some(&snapshot.source_real),
    );
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

/// `_dot_cleanup_mktemp -d` for one extension run: a fresh `0700`
/// directory `${TMPDIR:-/tmp}/dot.<pid>.<n>.<rand>`, mirroring the
/// mktemp template root and directory mode. Creation races retry;
/// other failures surface like the shell's allocator failure.
pub fn make_temp_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::sync::atomic::Ordering;
    let root = std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
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

/// `$(tr '\n' ' ' <"$log")`: every newline becomes a space;
/// command substitution then strips trailing newlines, of which
/// none remain, so trailing newlines surface as trailing spaces.
pub fn collapse_log(log: &[u8]) -> Vec<u8> {
    log.iter()
        .map(|byte| if *byte == b'\n' { b' ' } else { *byte })
        .collect()
}

/// The tail of `_dot_doctor_run_extension`: a nonzero worker status
/// files `<key> doctor extension failed`, stray log output files
/// `<key> doctor extension wrote outside the result API`, and a
/// quiet success files nothing. The detail is the collapsed log,
/// always passed (possibly empty) on the failure/warn paths.
pub fn extension_tail(rec: &mut Recorder, key: &[u8], rc: i32, log: &[u8]) {
    if rc != 0 {
        let mut message = key.to_vec();
        message.extend_from_slice(b" doctor extension failed");
        rec.fail(&message, Some(&collapse_log(log)));
    } else if !log.is_empty() {
        let mut message = key.to_vec();
        message.extend_from_slice(b" doctor extension wrote outside the result API");
        rec.warn(&message, Some(&collapse_log(log)));
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

/// `_dot_overlay_context_create "$temporary" doctor active none`
/// with the caller's overlay records, resolved against the live
/// `HOME`, euid, and clock. Returns the context path and token
/// (`REPLY_PATH` / `REPLY_TOKEN`), or `None` exactly where the
/// shell takes the `context unavailable` branch.
pub fn create_context(temporary: &Path, overlays: &[Vec<u8>]) -> Option<(PathBuf, String)> {
    let home = std::env::var_os("HOME")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let euid = crate::temp::current_uid()?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    crate::overlay_context::create(
        temporary, "doctor", "active", "none", overlays, &home, euid, now_secs,
    )
    .ok()
}

/// `_dot_doctor_run_extension`: allocate scratch, create the result
/// file, build the overlay context, run the worker through
/// `worker`, dispatch the filed rows through `render` (the
/// `_dot_doctor_render_records` seam, owned by another lane), file
/// the tail record, remove the scratch directory best-effort, and
/// return the worker status. The temp/context failure records and
/// the early `return 1` paths mirror the shell line for line.
pub fn run_extension(
    rec: &mut Recorder,
    key: &[u8],
    script: &Path,
    overlays: &[Vec<u8>],
    worker: &mut dyn FnMut(&WorkerInvocation<'_>) -> i32,
    render: &mut dyn FnMut(&Path, &mut Recorder),
) -> i32 {
    let temporary = match make_temp_dir() {
        Ok(dir) => dir,
        Err(_) => {
            let mut message = key.to_vec();
            message.extend_from_slice(b" doctor extension temporary directory unavailable");
            rec.fail(&message, Some(b"check TMPDIR permissions and free space"));
            return 1;
        }
    };
    let (result, log) = result_paths(&temporary);
    let _ = create_result_file(&result);
    let (context, token) = match create_context(&temporary, overlays) {
        Some(pair) => pair,
        None => {
            let mut message = key.to_vec();
            message.extend_from_slice(b" doctor extension context unavailable");
            rec.fail(&message, None);
            let _ = std::fs::remove_dir_all(&temporary);
            return 1;
        }
    };
    let invocation = WorkerInvocation {
        script,
        temporary: &temporary,
        result: &result,
        context: &context,
        token: &token,
        log: &log,
    };
    let rc = worker(&invocation);
    render(&result, rec);
    let log_bytes = std::fs::read(&log).unwrap_or_default();
    extension_tail(rec, key, rc, &log_bytes);
    let _ = std::fs::remove_dir_all(&temporary);
    rc
}

/// `dot_ui_title 'dot doctor'` under the pipe projection (no gum,
/// non-tty): a blank line, the title, and a trailing blank line.
pub fn doctor_title() -> Vec<u8> {
    b"\ndot doctor\n\n".to_vec()
}

/// The summary rule of `_dot_doctor`: `%d passed · %d warnings ·
/// %d failed` over the final counters.
pub fn summary_line(pass: u64, warn: u64, fail: u64) -> String {
    format!("{pass} passed · {warn} warnings · {fail} failed")
}

/// The box color selector of `_dot_doctor`: red when anything
/// failed, yellow on warnings only, green when clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryColor {
    /// `_DR_FAIL_COUNT -gt 0`.
    Red,
    /// No failures, `_DR_WARN_COUNT -gt 0`.
    Yellow,
    /// No failures and no warnings.
    Green,
}

impl SummaryColor {
    /// The color name `_dot_doctor` hands to `dot_ui_summary_box`.
    pub fn name(self) -> &'static str {
        match self {
            SummaryColor::Red => "red",
            SummaryColor::Yellow => "yellow",
            SummaryColor::Green => "green",
        }
    }
}

/// Select the summary [`SummaryColor`] from the final counters.
pub fn summary_color(fail_count: u64, warn_count: u64) -> SummaryColor {
    if fail_count > 0 {
        SummaryColor::Red
    } else if warn_count > 0 {
        SummaryColor::Yellow
    } else {
        SummaryColor::Green
    }
}

/// `dot_ui_summary_box` under the pipe projection (no gum,
/// non-tty): the 32-wide `═` rule, the summary line, and the rule
/// again. The color travels only to gum/tty styling, owned by the
/// `ui` lane.
pub fn summary_box(summary: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..32 {
        out.extend_from_slice("═".as_bytes());
    }
    out.push(b'\n');
    out.extend_from_slice(summary);
    out.push(b'\n');
    for _ in 0..32 {
        out.extend_from_slice("═".as_bytes());
    }
    out.push(b'\n');
    out
}

/// The exit rule of `_dot_doctor`:
/// `[[ $_DR_FAIL_COUNT -eq 0 && $status -eq 0 ]]`.
pub fn overall_ok(fail_count: u64, status: i32) -> bool {
    fail_count == 0 && status == 0
}

/// One raw discovery line of `_dot_doctor`'s specs loop:
/// `[[ -n $spec ]] || continue`, then `IFS=$'\t' read -r key
/// script`, where the last variable keeps the remainder, so the
/// split happens at the FIRST tab (`script` may still contain
/// tabs). Empty lines yield `None` (skipped).
pub fn split_spec(line: &[u8]) -> Option<(&[u8], &[u8])> {
    if line.is_empty() {
        return None;
    }
    match line.iter().position(|byte| *byte == b'\t') {
        Some(tab) => Some((&line[..tab], &line[tab + 1..])),
        None => Some((line, b"".as_slice())),
    }
}

/// One kernel check of `_dot_doctor` (`_dr_check_base_repo`,
/// `_dr_check_update_lock`, `_dr_check_provider`,
/// `_dr_check_overlays`, `_dr_check_merges`, owned by other lanes):
/// files its records and returns nothing. Callers pass the five in
/// `_dot_doctor` order.
pub type Kernel<'a> = Box<dyn FnMut(&mut Recorder) + 'a>;

/// One extension dispatch of `_dot_doctor`'s specs loop (the
/// `_dot_doctor_run_extension` seam): files the extension records
/// and returns the worker status; nonzero marks `status=1`.
pub type ExtensionRunner<'a> = Box<dyn FnMut(&mut Recorder, &[u8], &[u8]) -> i32 + 'a>;

/// `_dot_doctor`: write the title, file the runtime check (with its
/// engine-source tail), run each kernel check in order, dispatch
/// every discovered spec line (`key\tscript`, skipping blanks), and
/// close with the blank line plus summary box. `discovery` is the
/// `_dot_doctor_extension_specs` seam: `Err` files `doctor
/// extension discovery failed` and marks `status=1` without looping.
/// Returns the `[[ ... ]]` exit rule via [`overall_ok`].
#[allow(clippy::too_many_arguments)]
pub fn run_doctor(
    out: &mut dyn std::io::Write,
    rec: &mut Recorder,
    runtime: &RuntimeSnapshot,
    engine: &EngineSnapshot,
    home: &[u8],
    kernels: &mut [Kernel<'_>],
    discovery: &Result<Vec<Vec<u8>>, ()>,
    runner: &mut ExtensionRunner<'_>,
) -> bool {
    let _ = out.write_all(&doctor_title());
    check_runtime(rec, runtime, engine, home);
    for kernel in kernels.iter_mut() {
        kernel(rec);
    }
    let mut status = 0;
    match discovery {
        Err(()) => {
            rec.fail(b"doctor extension discovery failed", None);
            status = 1;
        }
        Ok(specs) => {
            for line in specs {
                let Some((key, script)) = split_spec(line) else {
                    continue;
                };
                if runner(rec, key, script) != 0 {
                    status = 1;
                }
            }
        }
    }
    let counts = rec.counts();
    let summary = summary_line(counts.pass, counts.warn, counts.fail);
    let _ = out.write_all(&rec.render());
    let _ = out.write_all(b"\n");
    let _ = out.write_all(&summary_box(summary.as_bytes()));
    overall_ok(counts.fail, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_covers_home_corners() {
        assert_eq!(tilde(b"/home/u/proj", b"/home/u"), b"~/proj");
        assert_eq!(tilde(b"/home/u", b"/home/u"), b"~");
        assert_eq!(tilde(b"/home/u2/x", b"/home/u"), b"/home/u2/x");
        assert_eq!(tilde(b"/home/u", b"/home/u/"), b"/home/u");
        assert_eq!(tilde(b"/etc/dot", b"/home/u"), b"/etc/dot");
        // Empty HOME: only the empty path abbreviates to `~`;
        // absolute paths lose the leading slash under `~/`.
        assert_eq!(tilde(b"", b""), b"~");
        assert_eq!(tilde(b"/a", b""), b"~/a");
        assert_eq!(tilde(b"rel", b""), b"rel");
        // HOME `/`: only a doubled-slash prefix abbreviates.
        assert_eq!(tilde(b"/", b"/"), b"~");
        assert_eq!(tilde(b"//a", b"/"), b"~/a");
        assert_eq!(tilde(b"/a", b"/"), b"/a");
    }

    #[test]
    fn collapse_log_maps_every_newline() {
        assert_eq!(collapse_log(b""), b"");
        assert_eq!(collapse_log(b"a\nb\n"), b"a b ");
        assert_eq!(collapse_log(b"\n"), b" ");
        assert_eq!(collapse_log(b"a\rb\n"), b"a\rb ");
    }

    #[test]
    fn split_spec_skips_blanks_and_splits_first_tab() {
        assert_eq!(split_spec(b""), None);
        assert_eq!(
            split_spec(b"a-one\t/fake/a.sh"),
            Some((b"a-one".as_slice(), b"/fake/a.sh".as_slice()))
        );
        assert_eq!(
            split_spec(b"key\tscript\twith\ttabs"),
            Some((b"key".as_slice(), b"script\twith\ttabs".as_slice()))
        );
        assert_eq!(
            split_spec(b"no-tab"),
            Some((b"no-tab".as_slice(), b"".as_slice()))
        );
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
