//! Merge-hook orchestration helpers (slices 7 and 45: merge layer).
//!
//! Ports the dependency-light majority of `lib/dot/merges.sh`:
//! label derivation, serial detection, job-count selection, result
//! summaries, result-file prefixes, progress details, hook-spec
//! collection (sort keys, identity checks, duplicate detection, and
//! the `LC_ALL=C` sort), and the merge-result parse plus render
//! halves of `_print_merge_result`. The capture decision kernel of
//! `_print_merge_capture` is here too, as a data-only outcome — the
//! logfile and warning rendering stays with the shell UI layer.
//!
//! The coordinator below owns the remaining update path: discovery through
//! the shared extension-trust checks, bounded batches, serial barriers, and
//! deterministic result replay. It deliberately keeps the worker itself in
//! the private `hook_worker` module, the only boundary allowed to execute hook
//! code.
//!
//! Everything here is a pure function of explicit inputs. Job
//! counts and verbosity knobs read no ambient state: callers pass
//! the already-read `DOT_MERGE_JOBS` / `DOT_UPDATE_JOBS` /
//! `DOT_VERBOSE` / `DOT_QUIET` values (empty when unset), UI widths
//! and renderer flags arrive explicitly like the [`crate::progress_ui`]
//! twins take them, and the CPU probe runs the same `getconf` /
//! `uname` / `sysctl` chain as the shell so differential tests
//! observe both sides on the same machine.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::merge_block::trim_shell_space;
use crate::progress_ui::{self, Palette, arith_value};

/// `_merge_trim`: strip shell whitespace from both ends.
pub fn trim(line: &str) -> &str {
    trim_shell_space(line)
}

/// Strip one trailing `suffix` from `name`, like the shell
/// `${_base%.sh}` / `${_base%.serial}` (exactly one occurrence).
fn strip_one<'a>(name: &'a [u8], suffix: &[u8]) -> &'a [u8] {
    name.strip_suffix(suffix).unwrap_or(name)
}

/// `_merge_hook_specs` sort key: basename without one trailing
/// `.sh`, then one trailing `.serial` (exactly one occurrence each,
/// like the shell `${var%.sh}` / `${var%.serial}`). Byte-oriented.
pub fn spec_key(script: &OsStr) -> OsString {
    let bytes = script.as_bytes();
    let base = match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &bytes[index + 1..],
        None => bytes,
    };
    OsString::from_vec(strip_one(strip_one(base, b".sh"), b".serial").to_vec())
}

/// `_merge_label_from_script`: the [spec key](spec_key), then the
/// text after a leading `<digits><-|_>` sequence
/// (`^[0-9]+[-_](.+)$`). Byte-oriented like the shell match.
pub fn label_from_script(path: &OsStr) -> OsString {
    let key = spec_key(path);
    let stem = key.as_bytes();
    let mut digits = 0;
    while digits < stem.len() && stem[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits > 0 && stem.len() > digits + 1 && (stem[digits] == b'-' || stem[digits] == b'_') {
        return OsString::from_vec(stem[digits + 1..].to_vec());
    }
    key
}

/// `_merge_hook_is_serial`: true when the script path ends in
/// `.serial.sh` (the shell tests `$2`, ignoring the key).
pub fn is_serial(script: &str) -> bool {
    script.ends_with(".serial.sh")
}

/// True for an all-ASCII-digit, non-empty job count (`case ''
/// | *[!0-9]*` rejects everything else, checked byte-wise under
/// `LC_ALL=C`).
fn is_count(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Normalize one `*_JOBS` value: valid counts pass through verbatim
/// (the shell prints `$_jobs` unchanged, leading zeros included);
/// anything else defers to `fallback`. Zero in any width means one
/// worker (`[[ $_jobs -lt 1 ]]`).
fn normalize_jobs(raw: &str, fallback: &str) -> String {
    if !is_count(raw) {
        return fallback.to_string();
    }
    if raw.bytes().all(|byte| byte == b'0') {
        return "1".to_string();
    }
    raw.to_string()
}

/// Pure kernel of `_dot_update_cpu_count`: pick from the already-run
/// `getconf _NPROCESSORS_ONLN` output, `uname -s`, and `sysctl -n
/// hw.ncpu` output. Unparseable means four workers.
pub fn cpu_count_select(getconf: &str, uname_s: &str, sysctl: &str) -> String {
    let mut probed = getconf;
    if probed.is_empty() && uname_s == "Darwin" {
        probed = sysctl;
    }
    normalize_jobs(probed, "4")
}

/// Run one helper binary, returning trimmed stdout (empty on any
/// failure, like `$(... || true)`).
fn probe(program: &str, args: &[&str]) -> String {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// `_dot_update_cpu_count`: `getconf`, Darwin `sysctl` fallback,
/// default four.
pub fn cpu_count() -> String {
    let getconf = probe("getconf", &["_NPROCESSORS_ONLN"]);
    let uname = if getconf.is_empty() {
        probe("uname", &["-s"])
    } else {
        String::new()
    };
    let sysctl = if getconf.is_empty() && uname == "Darwin" {
        probe("sysctl", &["-n", "hw.ncpu"])
    } else {
        String::new()
    };
    cpu_count_select(&getconf, &uname, &sysctl)
}

/// `_dot_update_jobs`: `DOT_UPDATE_JOBS` when numeric, else the CPU
/// count. Minimum one.
pub fn update_jobs(dot_update_jobs: &str) -> String {
    normalize_jobs(dot_update_jobs, &cpu_count())
}

/// `_merge_parallel_jobs`: `DOT_MERGE_JOBS` when numeric, else the
/// update-job count. Minimum one.
pub fn parallel_jobs(dot_merge_jobs: &str, dot_update_jobs: &str) -> String {
    let fallback = update_jobs(dot_update_jobs);
    normalize_jobs(dot_merge_jobs, &fallback)
}

/// `_merge_summary`: `1 config merged` / `N configs merged`.
/// Counts arrive canonical from shell arithmetic upstream, so only
/// the `== 1` singular needs matching.
pub fn summary(count: i64) -> String {
    if count == 1 {
        "1 config merged".to_string()
    } else {
        format!("{count} configs merged")
    }
}

/// `_merge_failure_summary`: `1 config hook failed` /
/// `N config hooks failed`.
pub fn failure_summary(count: i64) -> String {
    if count == 1 {
        "1 config hook failed".to_string()
    } else {
        format!("{count} config hooks failed")
    }
}

/// `_merge_warning_summary`: `<ok summary>, <failure summary>`.
/// The shell subtracts with `$(( ))`, so `total < failed` prints a
/// negative succeeded count rather than saturating.
pub fn warning_summary(total: i64, failed: i64) -> String {
    format!("{}, {}", summary(total - failed), failure_summary(failed))
}

/// `_merge_result_prefix`: `<dir>/<idx zero-padded to 3>`.
pub fn result_prefix(dir: &str, index: u64) -> String {
    format!("{dir}/{index:03}")
}

/// `_merge_progress_detail`: the hook label cell plus bar for
/// `done/total`. Merge hook filenames already define the durable
/// hook identity, so this stays generic over the label exactly like
/// the shell wrapper over `_ui_progress_detail_with_label` (no
/// suffix, caller-pinned widths and renderer flags).
pub fn progress_detail(
    label: &[u8],
    done: i64,
    total: i64,
    label_width: &str,
    bar_width: &str,
    ascii: bool,
    multibyte: bool,
) -> Vec<u8> {
    progress_ui::progress_detail_with_label(
        label,
        done,
        total,
        None,
        label_width,
        bar_width,
        ascii,
        multibyte,
    )
}

/// Split a captured hook log the way `_print_merge_result` reads
/// it: shell-whitespace-trimmed lines with empties dropped; the
/// first surviving line is the display label and the rest are
/// detail rows. Splits on `\n` only — the shell `read` sees `\r`
/// as content, so [`str::lines`] (which strips it) would diverge.
pub fn parse_result_log(log: &str) -> (Option<String>, Vec<String>) {
    let body = log.strip_suffix('\n').unwrap_or(log);
    if body.is_empty() {
        return (None, Vec::new());
    }
    let mut label = None;
    let mut details = Vec::new();
    for line in body.split('\n') {
        let trimmed = trim(line);
        if trimmed.is_empty() {
            continue;
        }
        if label.is_none() {
            label = Some(trimmed.to_string());
        } else {
            details.push(trimmed.to_string());
        }
    }
    (label, details)
}

/// `_print_merge_result` label resolution: the first non-blank log
/// line, or the [script-stem label](label_from_script) when the log
/// carries none. Verbose hooks own their first output line as the
/// display label; the runner stays generic.
pub fn result_label(script: &OsStr, log: &str) -> (OsString, Vec<String>) {
    let (label, details) = parse_result_log(log);
    match label {
        Some(label) => (OsString::from(label), details),
        None => (label_from_script(script), details),
    }
}

fn trim_bytes(line: &[u8]) -> &[u8] {
    let shell_space = |byte: &u8| matches!(*byte, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let start = line
        .iter()
        .position(|byte| !shell_space(byte))
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !shell_space(byte))
        .map_or(start, |index| index + 1);
    &line[start..end]
}

/// Byte-preserving counterpart of [`result_label`] for live worker output.
/// Bash variables retain every byte except NUL, so lossy UTF-8 conversion at
/// this boundary would silently rewrite user-authored hook diagnostics.
fn result_label_bytes(script: &OsStr, log: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    // `read -r` stores each shell line in a Bash variable, which discards NUL
    // bytes while preserving other non-UTF-8 bytes.
    let shell_bytes: Vec<u8> = log.iter().copied().filter(|byte| *byte != 0).collect();
    let mut rows = shell_bytes
        .split(|byte| *byte == b'\n')
        .map(trim_bytes)
        .filter(|line| !line.is_empty());
    let label = rows
        .next()
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| label_from_script(script).as_bytes().to_vec());
    (label, rows.map(<[u8]>::to_vec).collect())
}

/// `_print_merge_result` render half: one `_ui_item` row with the
/// [`progress_ui::duration_ms`] trailer, then one `_ui_detail` row
/// per detail line. Takes the already-resolved label plus details
/// from [`result_label`]; the log file read stays with the caller.
#[allow(clippy::too_many_arguments)] // positional parity with the ported shell function
pub fn render_result(
    palette: &Palette,
    quiet: bool,
    live_active: bool,
    status: &[u8],
    label: &[u8],
    elapsed_ms: i64,
    details: &[Vec<u8>],
    multibyte: bool,
) -> (Vec<u8>, bool) {
    let duration = progress_ui::duration_ms(elapsed_ms);
    let (mut out, mut live_active) = progress_ui::item(
        palette,
        quiet,
        live_active,
        status,
        label,
        Some(&duration),
        multibyte,
    );
    for line in details {
        let (chunk, live) = progress_ui::detail(palette, quiet, live_active, line, multibyte);
        out.extend_from_slice(&chunk);
        live_active = live;
    }
    (out, live_active)
}

/// Data-only outcome of `_print_merge_capture`: which branch the
/// coordinator takes after reading one hook's result records. The
/// `has_merge` / `rc` / `elapsed` inputs are the raw single-scalar
/// file bytes (`None` when unreadable, taking the shell defaults);
/// surrounding whitespace reads like command substitution. The
/// `_logfile_print` plus `_warn` rendering of the warning branches
/// stays shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureAction {
    /// `has_merge` is not exactly one: the hook defined no merge.
    /// The shell returns 1 without printing.
    Skipped,
    /// Verbose foreground row: render the parsed result with an
    /// `ok` or `warning` status.
    ShowResult {
        /// True unless the `rc` scalar reads exactly zero.
        warning: bool,
        /// Parsed `elapsed_ms` record (zero when missing or
        /// unrepresentable — the writer always emits canonical
        /// shell arithmetic).
        elapsed_ms: i64,
    },
    /// Quiet failure with captured output: print the log file,
    /// then warn.
    ShowLogWarning,
    /// Quiet failure without output: warn with the hook key.
    ShowEmptyWarning,
    /// The `rc` scalar is unrepresentable, so bash errors falsy on
    /// both the status and the nonzero comparisons: a quiet hook
    /// stays silent (verbose still shows it — that branch only
    /// reads the verbosity knobs). Counts as merged but never as
    /// failed, exactly like the shell batch tallies.
    Silent {
        /// Always true here: only a nonzero-or-unreadable `rc`
        /// reaches this variant quietly.
        warning: bool,
        /// Parsed `elapsed_ms` record, as in [`CaptureAction::ShowResult`].
        elapsed_ms: i64,
    },
}

/// `_print_merge_capture` decision kernel: skip hooks without a
/// merge record, show the result row in verbose mode, otherwise
/// warn on nonzero `rc` (with the log when it is nonempty).
/// `verbose` and `quiet` are the raw `DOT_VERBOSE` / `DOT_QUIET`
/// values. Scalars read through the shared `progress_ui` arithmetic
/// helper:
/// trimmed decimals literally, bare names and the empty string as
/// unset (zero), anything else unrepresentable (`None`, failing
/// both `-eq` and `-ne` like the shell `[[ ]]` errors).
pub fn capture_action(
    has_merge: Option<&str>,
    rc: Option<&str>,
    elapsed: Option<&str>,
    verbose: &str,
    quiet: &str,
    log_nonempty: bool,
) -> CaptureAction {
    if arith_value(has_merge.unwrap_or("0")) != Some(1) {
        return CaptureAction::Skipped;
    }
    let rc_value = arith_value(rc.unwrap_or("1"));
    let warning = rc_value != Some(0);
    let elapsed_ms = elapsed.and_then(arith_value).unwrap_or(0);
    let verbose_on = arith_value(verbose) == Some(1);
    let quiet_off = arith_value(quiet).is_some_and(|value| value != 1);
    if verbose_on && quiet_off {
        return CaptureAction::ShowResult {
            warning,
            elapsed_ms,
        };
    }
    match rc_value {
        Some(0) => CaptureAction::Silent {
            warning: false,
            elapsed_ms,
        },
        Some(_) => {
            if log_nonempty {
                CaptureAction::ShowLogWarning
            } else {
                CaptureAction::ShowEmptyWarning
            }
        }
        None => CaptureAction::Silent {
            warning: true,
            elapsed_ms,
        },
    }
}

/// Failures collecting hook specs before the `LC_ALL=C` sort. Both
/// abort discovery with exit 1 after one stderr line, like the
/// shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// A script whose sort key matches no hook identity; carries
    /// the script basename, which is what the shell prints.
    InvalidIdentity(OsString),
    /// Two scripts claiming one identity (group 2 of the match).
    DuplicateIdentity(OsString),
}

impl std::fmt::Display for SpecError {
    /// The shell stderr line for this failure.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::InvalidIdentity(name) => write!(
                formatter,
                "dot: invalid merge-hook identity: {}",
                name.to_string_lossy()
            ),
            SpecError::DuplicateIdentity(identity) => write!(
                formatter,
                "dot: duplicate merge-hook identity: {}",
                identity.to_string_lossy()
            ),
        }
    }
}

/// Basename of a script path, byte-oriented.
fn spec_basename(script: &OsStr) -> &OsStr {
    let bytes = script.as_bytes();
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(index) => OsStr::from_bytes(&bytes[index + 1..]),
        None => script,
    }
}

/// Strip one optional `<digits><-|_>` prefix, returning the
/// remainder. A digit run without its separator is not a prefix —
/// the regex backtracks the same way.
fn strip_count_prefix(key: &[u8]) -> Option<&[u8]> {
    let digits = key.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    match key.get(digits) {
        Some(b'-') | Some(b'_') => Some(&key[digits + 1..]),
        _ => None,
    }
}

/// True for `[a-z][a-z0-9-]*` over bytes (`LC_ALL=C`, ASCII only).
fn is_identity_tail(tail: &[u8]) -> bool {
    let mut bytes = tail.iter();
    match bytes.next() {
        Some(byte) if byte.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// `_merge_hook_specs` identity check:
/// `^([0-9]+[-_])?([a-z][a-z0-9-]*)$` over the [sort key](spec_key),
/// returning group 2. Byte-oriented like the shell match under
/// `LC_ALL=C`.
pub fn spec_identity(key: &OsStr) -> Option<OsString> {
    let bytes = key.as_bytes();
    if let Some(tail) = strip_count_prefix(bytes) {
        if is_identity_tail(tail) {
            return Some(OsString::from_vec(tail.to_vec()));
        }
    }
    if is_identity_tail(bytes) {
        return Some(key.to_os_string());
    }
    None
}

/// Pure kernel of `_merge_hook_specs`: sort keys, identity checks,
/// and duplicate detection over caller-supplied script paths, then
/// the `LC_ALL=C sort` over the `<key><tab><script>` lines.
/// Callers pass paths in glob order so first-offense errors match
/// the shell; native discovery performs the trust validation
/// (enablement, root, directory, and file checks) before this kernel.
pub fn collect_specs(scripts: &[&OsStr]) -> Result<Vec<(OsString, OsString)>, SpecError> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut rows: Vec<(Vec<u8>, OsString, OsString)> = Vec::new();
    for script in scripts {
        let key = spec_key(script);
        let identity = spec_identity(&key)
            .ok_or_else(|| SpecError::InvalidIdentity(spec_basename(script).to_os_string()))?;
        if !seen.insert(identity.as_bytes().to_vec()) {
            return Err(SpecError::DuplicateIdentity(identity));
        }
        let mut line = key.as_bytes().to_vec();
        line.push(b'\t');
        line.extend_from_slice(script.as_bytes());
        rows.push((line, key, script.to_os_string()));
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(rows
        .into_iter()
        .map(|(_, key, script)| (key, script))
        .collect())
}

/// A validated merge-hook entry point in declaration order.
#[derive(Debug, Clone)]
pub(crate) struct Hook {
    key: OsString,
    script: PathBuf,
}

/// Merge discovery fails before any hook runs. The string is already the
/// shell-compatible diagnostic emitted by `_merge_hook_specs`.
#[derive(Debug)]
pub(crate) struct DiscoveryError(String);

/// Native inputs for the one `Configs` stage. Discovery owns the extension
/// trust envelope; the worker owns only a previously validated entry point.
pub(crate) struct RunInputs<'a> {
    pub(crate) runtime: &'a crate::app::Runtime,
    pub(crate) extension_inputs: crate::extension_trust::Inputs,
    pub(crate) extensions_enabled: bool,
    pub(crate) overlays: &'a [String],
    pub(crate) tmp: &'a Path,
    pub(crate) update_jobs: Option<&'a str>,
    pub(crate) merge_jobs: Option<&'a str>,
    pub(crate) verbose: bool,
    pub(crate) quiet: bool,
    pub(crate) palette: &'a Palette,
    pub(crate) multibyte: bool,
    pub(crate) log: &'a crate::log::Log,
}

/// Result of the coordinator. A nonzero status means one or more discovered
/// hooks failed, matching `_run_merges` after it has rendered every result.
pub(crate) struct Outcome {
    pub(crate) status: i32,
}

/// `_merge_hook_specs`: validate the configured extension root and directory,
/// validate each visible `*.sh`, then preserve the shell's key sort and
/// identity checks. A missing directory is an empty successful inventory.
fn discover(inputs: &RunInputs<'_>) -> Result<Vec<Hook>, DiscoveryError> {
    if !inputs.extensions_enabled {
        return Ok(Vec::new());
    }
    let root = &inputs.extension_inputs.extensions_dir;
    if !crate::extension_trust::root_validate(root, inputs.extension_inputs.euid) {
        return Err(DiscoveryError(format!(
            "dot: unsafe extension root: {root}"
        )));
    }
    let hooks = Path::new(root).join("merge-hooks.d");
    let present = hooks.exists()
        || std::fs::symlink_metadata(&hooks).is_ok_and(|meta| meta.file_type().is_symlink());
    if !present {
        return Ok(Vec::new());
    }
    if !crate::extension_trust::directory_validate(&hooks, root, inputs.extension_inputs.euid) {
        return Err(DiscoveryError(format!(
            "dot: unsafe merge-hook directory: {}",
            hooks.display()
        )));
    }
    let entries = std::fs::read_dir(&hooks).map_err(|_| {
        DiscoveryError(format!(
            "dot: unsafe merge-hook directory: {}",
            hooks.display()
        ))
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let name = entry
            .map_err(|_| {
                DiscoveryError(format!(
                    "dot: unsafe merge-hook directory: {}",
                    hooks.display()
                ))
            })?
            .file_name();
        if !name.as_bytes().starts_with(b".") && name.as_bytes().ends_with(b".sh") {
            names.push(name);
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut scripts = Vec::with_capacity(names.len());
    for name in names {
        let script = hooks.join(name);
        if !crate::extension_trust::file_validate(
            &script,
            &inputs.extension_inputs,
            inputs.overlays,
        ) {
            return Err(DiscoveryError(format!(
                "dot: unsafe merge hook: {}",
                script.display()
            )));
        }
        scripts.push(script);
    }
    let refs: Vec<&OsStr> = scripts.iter().map(|script| script.as_os_str()).collect();
    let rows = collect_specs(&refs).map_err(|error| DiscoveryError(error.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|(key, script)| Hook {
            key,
            script: PathBuf::from(script),
        })
        .collect())
}

/// Allocate one coordinator-owned private directory. Its random suffix is
/// internal; only the 0700 ownership boundary crosses into worker validation.
fn scratch(root: &Path) -> Option<PathBuf> {
    for _ in 0..crate::temp::TMP_RETRIES {
        let path = root.join(format!("dot.{}", crate::temp::random_suffix()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).is_ok()
                    && crate::temp::private_dir_validate(&path).is_ok()
                {
                    return Some(path);
                }
                let _ = std::fs::remove_dir(&path);
                return None;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// One completed worker record. Its position in the vector is the durable
/// replay order, independent of scheduling completion order.
struct ResultRecord {
    hook: Hook,
    rc: i32,
    output: Vec<u8>,
    has_merge: bool,
    elapsed_ms: i64,
}

/// State shared by every batch in one Configs stage. Keeping scheduling state
/// together makes the serial-barrier loop explicit without a wide positional
/// argument list.
struct BatchState<'a> {
    root: &'a Path,
    now_secs: i64,
    merge_index: usize,
    total: usize,
    overlays: &'a [Vec<u8>],
}

fn run_one(
    inputs: &RunInputs<'_>,
    hook: Hook,
    index: usize,
    root: &Path,
    now_secs: i64,
    overlays: &[Vec<u8>],
) -> ResultRecord {
    let temporary = root.join(format!("worker.{index}"));
    let ready = std::fs::create_dir(&temporary)
        .and_then(|()| std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700)))
        .is_ok()
        && crate::temp::private_dir_validate(&temporary).is_ok();
    if !ready {
        return ResultRecord {
            hook,
            rc: 1,
            output: b"dot: cannot create private merge-hook TMPDIR\n".to_vec(),
            has_merge: false,
            elapsed_ms: 0,
        };
    }
    let result = root.join(format!("{index:03}.has_merge"));
    // Serial barriers reuse the batch-local worker numbers. A failed worker
    // must not inherit a prior batch's marker and look as though it ran.
    let _ = std::fs::remove_file(&result);
    let created = crate::overlay_context::create(
        &temporary,
        "merge",
        "active",
        "none",
        overlays,
        &inputs.extension_inputs.home,
        inputs.extension_inputs.euid,
        now_secs,
    );
    let started = Instant::now();
    let (rc, output) = match created {
        Ok((context, token)) => {
            let mut worker = crate::hook_worker::Worker::with_extensions(
                inputs.runtime,
                &inputs.extension_inputs.extensions_dir,
            );
            let outcome = worker.merge(&hook.script, &temporary, &result, &context, &token);
            (outcome.rc, outcome.output)
        }
        Err(_) => (1, Vec::new()),
    };
    let has_merge = std::fs::read_to_string(&result)
        .ok()
        .is_some_and(|value| value.trim() == "1");
    let _ = std::fs::remove_dir_all(&temporary);
    ResultRecord {
        hook,
        rc,
        output,
        has_merge,
        elapsed_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Run one parallel batch with a fixed ceiling, then return records in the
/// declaration order they were launched. The batch always joins before its
/// caller can pass a `.serial.sh` barrier.
fn run_batch(
    inputs: &RunInputs<'_>,
    hooks: &[Hook],
    state: &mut BatchState<'_>,
    stage: &mut crate::progress_ui::Stage,
    out: &mut Vec<u8>,
) -> Vec<ResultRecord> {
    use std::io::Write as _;
    for hook in hooks {
        state.merge_index += 1;
        let label = label_from_script(&hook.key);
        let detail = format!(
            "{} {}/{}",
            label.to_string_lossy(),
            state.merge_index,
            state.total
        );
        let _ = out.write_all(&stage.update(
            detail.as_bytes(),
            state.now_secs,
            inputs.verbose.then_some("1"),
        ));
    }
    let jobs = parallel_jobs(
        inputs.merge_jobs.unwrap_or_default(),
        inputs.update_jobs.unwrap_or_default(),
    )
    .parse::<usize>()
    .unwrap_or(1);
    let mut records = Vec::with_capacity(hooks.len());
    let root = state.root;
    let now_secs = state.now_secs;
    let overlays = state.overlays;
    let first_index = state.merge_index - hooks.len();
    std::thread::scope(|scope| {
        // The shell never retains more PIDs than there are hooks. Do not let a
        // user-controlled but valid numeric job limit reserve unrelated memory.
        let mut workers = VecDeque::with_capacity(hooks.len().min(jobs.max(1)));
        for (index, hook) in hooks.iter().cloned().enumerate() {
            let index = first_index + index + 1;
            let panic_hook = hook.clone();
            workers.push_back((
                panic_hook,
                scope.spawn(move || run_one(inputs, hook, index, root, now_secs, overlays)),
            ));
            // Bash waits for the oldest in-flight worker once the ceiling is
            // reached, then immediately launches the next hook. This is a FIFO
            // window rather than fixed chunks, so a fast oldest worker frees
            // capacity even while a later worker is still running.
            if workers.len() >= jobs.max(1) {
                let (hook, worker) = workers.pop_front().expect("nonempty worker queue");
                records.push(worker.join().unwrap_or_else(|_| ResultRecord {
                    hook,
                    rc: 1,
                    output: Vec::new(),
                    has_merge: false,
                    elapsed_ms: 0,
                }));
            }
        }
        for (hook, worker) in workers {
            records.push(worker.join().unwrap_or_else(|_| ResultRecord {
                hook,
                rc: 1,
                output: Vec::new(),
                has_merge: false,
                elapsed_ms: 0,
            }))
        }
    });
    records
}

fn replay(
    records: Vec<ResultRecord>,
    inputs: &RunInputs<'_>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> (i64, i64) {
    use std::io::Write as _;
    let mut merged = 0;
    let mut failed = 0;
    for record in records {
        // `_run_merge_hook_batch` tallies the worker exit record even when
        // `_print_merge_capture` classifies the hook as not-run. A sourced
        // helper without `merge()` therefore fails the aggregate stage while
        // remaining absent from the merged count and per-hook rendering.
        if record.rc != 0 {
            failed += 1;
        }
        if !record.has_merge {
            continue;
        }
        merged += 1;
        if inputs.verbose && !inputs.quiet {
            let (label, details) = result_label_bytes(&record.hook.key, &record.output);
            let (rendered, _) = render_result(
                inputs.palette,
                false,
                false,
                if record.rc == 0 { b"ok" } else { b"warning" },
                &label,
                record.elapsed_ms,
                &details,
                inputs.multibyte,
            );
            let _ = out.write_all(&rendered);
        } else if record.rc != 0 {
            if !record.output.is_empty() {
                inputs.log.warn(
                    err,
                    &format!("  {} output:", record.hook.key.to_string_lossy()),
                );
                for line in record.output.split_inclusive(|byte| *byte == b'\n') {
                    let _ = err.write_all(b"    ");
                    let _ = err.write_all(line);
                }
                inputs.log.warn(err, "  warning: merge failed");
            } else {
                inputs.log.warn(
                    err,
                    &format!(
                        "  warning: merge failed: {}",
                        record.hook.key.to_string_lossy()
                    ),
                );
            }
        }
    }
    (merged, failed)
}

/// `_run_merges` natively: discover trusted hooks, execute bounded parallel
/// batches with serial barriers, replay captures by declaration order, and
/// close the one update `Configs` stage.
pub(crate) fn run(
    inputs: &RunInputs<'_>,
    stage: &mut crate::progress_ui::Stage,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
) -> Outcome {
    use std::io::Write as _;
    let hooks = match discover(inputs) {
        Ok(hooks) => hooks,
        Err(error) => {
            let _ = writeln!(err, "{}", error.0);
            inputs
                .log
                .warn(err, "  warning: merge-hook discovery failed");
            return Outcome { status: 1 };
        }
    };
    if hooks.is_empty() {
        let _ =
            out.write_all(&stage.start(b"Configs", Some(b"checking config hooks"), now_secs, None));
        let _ = out.write_all(&stage.finish(b"ok", b"no config hooks", now_secs));
        return Outcome { status: 0 };
    }
    let _ = out.write_all(&stage.start(b"Configs", Some(b"running config hooks"), now_secs, None));
    let Some(root) = scratch(inputs.tmp) else {
        inputs.log.warn(
            err,
            "  warning: could not allocate merge-hook scratch storage",
        );
        let _ = out.write_all(&stage.finish(
            b"warning",
            warning_summary(0, i64::try_from(hooks.len()).unwrap_or(i64::MAX)).as_bytes(),
            now_secs,
        ));
        return Outcome { status: 1 };
    };
    let mut merged = 0;
    let mut failed = 0;
    // Overlay records are immutable hook context; encode them once for every
    // worker rather than allocating the same byte vectors per hook.
    let overlays: Vec<Vec<u8>> = inputs
        .overlays
        .iter()
        .map(|record| record.as_bytes().to_vec())
        .collect();
    let mut state = BatchState {
        root: &root,
        now_secs,
        merge_index: 0,
        total: hooks.len(),
        overlays: &overlays,
    };
    let mut batch = Vec::new();
    for hook in hooks {
        if is_serial_os(&hook.script) {
            let (completed, failures) = replay(
                run_batch(inputs, &batch, &mut state, stage, out),
                inputs,
                out,
                err,
            );
            merged += completed;
            failed += failures;
            batch.clear();
            let (completed, failures) = replay(
                run_batch(inputs, &[hook], &mut state, stage, out),
                inputs,
                out,
                err,
            );
            merged += completed;
            failed += failures;
        } else {
            batch.push(hook);
        }
    }
    let (completed, failures) = replay(
        run_batch(inputs, &batch, &mut state, stage, out),
        inputs,
        out,
        err,
    );
    merged += completed;
    failed += failures;
    let _ = std::fs::remove_dir_all(root);
    let detail = if failed > 0 {
        warning_summary(merged, failed)
    } else if merged > 0 {
        summary(merged)
    } else {
        "no config changes".to_string()
    };
    let _ = out.write_all(&stage.finish(
        if failed > 0 { b"warning" } else { b"ok" },
        detail.as_bytes(),
        now_secs,
    ));
    Outcome {
        status: if failed == 0 { 0 } else { 1 },
    }
}

fn is_serial_os(script: &Path) -> bool {
    script
        .file_name()
        .is_some_and(|name| name.as_bytes().ends_with(b".serial.sh"))
}

#[cfg(test)]
mod tests {
    use super::scratch;
    use dot_test_support::TempDir;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn scratch_is_private_and_refuses_a_non_directory_root() {
        let scope = TempDir::new("merge-scratch").expect("fixture");
        let allocated = scratch(scope.path()).expect("private scratch");
        assert_eq!(
            std::fs::metadata(&allocated)
                .expect("scratch metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let blocked = scope.path().join("blocked");
        std::fs::write(&blocked, b"not a directory\n").expect("blocked root");
        assert!(scratch(&blocked).is_none());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"not a directory\n");
    }
}
