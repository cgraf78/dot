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
//! Still shell: `_run_merge_hook_capture`, `_run_merge_hook_batch`,
//! and top-level `_run_merges` are process-group orchestration, not
//! portable logic, and the `_merge_hook_specs` discovery envelope
//! (extension enablement plus root/directory/file trust validation)
//! stays shell with [`crate::extension_trust`].
//!
//! Everything here is a pure function of explicit inputs. Job
//! counts and verbosity knobs read no ambient state: callers pass
//! the already-read `DOT_MERGE_JOBS` / `DOT_UPDATE_JOBS` /
//! `DOT_VERBOSE` / `DOT_QUIET` values (empty when unset), UI widths
//! and renderer flags arrive explicitly like the [`crate::progress_ui`]
//! twins take them, and the CPU probe runs the same `getconf` /
//! `uname` / `sysctl` chain as the shell so differential tests
//! observe both sides on the same machine.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

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
/// the shell; the trust validation (enablement, root, directory,
/// and file checks) stays shell and runs before this kernel.
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
