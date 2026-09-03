//! Merge-hook orchestration helpers (slice 7: merge layer).
//!
//! Ports the dependency-light half of `lib/dot/merges.sh`: label
//! derivation, serial detection, job-count selection, result
//! summaries, and result-file prefixes. The parallel batch runner,
//! worker capture, and top-level `_run_merges` stay shell until the
//! progress-UI, extension-worker, and overlay-context slices land —
//! they are process-group orchestration, not portable logic.
//!
//! Everything here is a pure function of explicit inputs. Parallel
//! counts read no ambient state: callers pass the already-read
//! `DOT_MERGE_JOBS` / `DOT_UPDATE_JOBS` values (empty when unset),
//! and the CPU probe runs the same `getconf` / `uname` / `sysctl`
//! chain as the shell so differential tests observe both sides on
//! the same machine.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

use crate::merge_block::trim_shell_space;

/// `_merge_trim`: strip shell whitespace from both ends.
pub fn trim(line: &str) -> &str {
    trim_shell_space(line)
}

/// Strip one trailing `suffix` from `name`, like the shell
/// `${_base%.sh}` / `${_base%.serial}` (exactly one occurrence).
fn strip_one<'a>(name: &'a [u8], suffix: &[u8]) -> &'a [u8] {
    name.strip_suffix(suffix).unwrap_or(name)
}

/// `_merge_label_from_script`: basename without `.sh` / `.serial`,
/// then the text after a leading `<digits><-|_>` sequence
/// (`^[0-9]+[-_](.+)$`). Byte-oriented like the shell match.
pub fn label_from_script(path: &OsStr) -> OsString {
    let bytes = path.as_bytes();
    let base = match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &bytes[index + 1..],
        None => bytes,
    };
    let stem = strip_one(strip_one(base, b".sh"), b".serial");
    let mut digits = 0;
    while digits < stem.len() && stem[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits > 0 && stem.len() > digits + 1 && (stem[digits] == b'-' || stem[digits] == b'_') {
        return OsString::from_vec(stem[digits + 1..].to_vec());
    }
    OsString::from_vec(stem.to_vec())
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
