//! Doctor coordinator helpers.
//!
//! Owns extension discovery, result dispatch, and run summaries. Neighboring
//! modules remain authoritative for their focused responsibilities:
//! `doctor_runtime`
//! owns the `_dr_*` result rendering and counters, part 2
//! (`doctor_paths`) owns the path abbreviators, and part 3
//! (`doctor_records`) owns the extension-side record sink.
//!
//! Discovery decisions:
//! - An invalid or duplicate identity refuses that one file
//!   ([`Discovery::invalid`]) and discovery continues. The shell-era loop
//!   stopped at the first such name, and the coordinator then ran no
//!   extension at all: one leftover file after a renumbering hid every
//!   section behind a detail-less failure. For a duplicate, the first
//!   claimant in glob order keeps the identity and runs; each later one is
//!   refused, naming the file it collides with.
//! - Per-file trust validation (`_dot_extension_file_validate`) belongs to the
//!   extension-trust module. [`collect_specs_with`] accepts that predicate and
//!   refuses each untrusted file on its own ([`Discovery::rejected`]) instead
//!   of abandoning discovery: a dangling overlay link left between a pull
//!   that renamed an extension and the link phase must not hide every other
//!   extension. [`collect_specs`] supplies the trusted test seam used by
//!   focused rows.
//! - Names travel as `&[u8]` throughout (byte sort is `LC_ALL=C`
//!   sort; the identity character classes are ASCII ranges), so
//!   non-UTF8 entry names behave like the shell's.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// One discovered doctor extension: the sort key derived from the
/// file name plus the full script path, mirroring one
/// `printf '%s\t%s\n' "$key" "$script"` line of
/// `_dot_doctor_extension_specs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// `key`: the file basename minus one `.sh` suffix
    /// (`${script##*/}` then `${key%.sh}`).
    pub key: Vec<u8>,
    /// Full script path (`$root/$file_name`, unnormalized like the
    /// shell's glob expansion).
    pub script: PathBuf,
}

/// Why a trusted extension file was refused for its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// The key matches neither the bare nor the numerically prefixed
    /// identity shape (`Foo.sh`, `1a.sh`).
    InvalidIdentity,
    /// An earlier file in glob order already claimed this identity
    /// (`20-foo.sh` and then `21-foo.sh`, say).
    DuplicateIdentity {
        /// The twice-claimed identity (the regex's second group).
        identity: Vec<u8>,
        /// File name of the earlier file that keeps the identity.
        claimed_by: Vec<u8>,
    },
}

impl SpecError {
    /// The reason clause for a refusal row about `shown` (the file as
    /// displayed), ending in the next step.
    pub fn reason(&self, shown: &str) -> String {
        match self {
            SpecError::InvalidIdentity => format!(
                "{shown} has an invalid name; rename it to NN-name using lowercase letters, digits, and hyphens"
            ),
            SpecError::DuplicateIdentity {
                identity,
                claimed_by,
            } => format!(
                "{shown} repeats identity {} of {}; remove or rename one of them",
                String::from_utf8_lossy(identity),
                String::from_utf8_lossy(claimed_by),
            ),
        }
    }
}

/// Outcome of [`collect_specs`]: the runnable listing plus every file
/// refused for its trust or its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Sorted runnable specs.
    pub specs: Vec<Spec>,
    /// Trusted scripts refused for their identity, in glob order. They
    /// never run; callers report each one.
    pub invalid: Vec<(PathBuf, SpecError)>,
    /// Scripts the trust predicate refused, in glob order. They never run
    /// and never claim an identity; callers report each one.
    pub rejected: Vec<PathBuf>,
}

/// Rendered `key\tscript` bytes of one spec: the unit the shell
/// pipeline sorts and prints.
fn spec_line(spec: &Spec) -> Vec<u8> {
    let mut line = spec.key.clone();
    line.push(b'\t');
    line.extend_from_slice(spec.script.as_os_str().as_encoded_bytes());
    line
}

/// `key=${script##*/}; key=${key%.sh}`: basename, then one `.sh`
/// suffix stripped when present.
///
/// Total like the shell expansion (a name without the suffix keeps
/// itself, e.g. `a.sh.sh` yields `a.sh`); [`collect_specs`] only
/// feeds it `*.sh` entry names, where the strip always fires.
pub fn extension_key(script: &[u8]) -> &[u8] {
    let base = match script.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &script[index + 1..],
        None => script,
    };
    match base.strip_suffix(b".sh") {
        Some(stripped) => stripped,
        None => base,
    }
}

/// True when `tail` matches the identity shape `[a-z][a-z0-9-]*`
/// (the regex's second group, ASCII under `LC_ALL=C`).
fn is_identity_tail(tail: &[u8]) -> bool {
    let first = match tail.first() {
        Some(first) => *first,
        None => return false,
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    tail.iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// The `[[ $key =~ ^([0-9]+[-_])?([a-z][a-z0-9-]*)$ ]]` test,
/// returning the identity (`${BASH_REMATCH[2]}`) on match.
///
/// Only one prefix split can ever match — the separator is a single
/// `[-_]` after a maximal digit run — so a leading-digits-plus-
/// separator key either yields its tail or is invalid outright (the
/// shell's backtrack then fails on the leading digit, e.g. `1a` or
/// `12_3a`); other keys must match whole.
pub fn extension_identity(key: &[u8]) -> Option<&[u8]> {
    let mut digits = 0;
    while digits < key.len() && key[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits > 0 && digits < key.len() && (key[digits] == b'-' || key[digits] == b'_') {
        let tail = &key[digits + 1..];
        if is_identity_tail(tail) {
            return Some(tail);
        }
        return None;
    }
    if is_identity_tail(key) {
        return Some(key);
    }
    None
}

/// Byte-substring probe for the `*...*` case arms of
/// [`source_relative_valid`].
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// `_dot_doctor_extension_specs` over a trusted `doctor.d`
/// directory: entry names ending in `.sh` (leading-dot names
/// excluded, like the shell's `*.sh` glob) become specs in
/// `LC_ALL=C sort` order of their rendered lines.
///
/// A file with an invalid or duplicate identity is set aside in
/// [`Discovery::invalid`] and the rest still list (see the module docs).
///
/// Only I/O failures (an unreadable `dir`) surface as `Err`: the
/// shell's missing-root early return runs before this logic, so
/// callers pass a directory they already know exists.
pub fn collect_specs(dir: &Path) -> std::io::Result<Discovery> {
    collect_specs_with(dir, |_| true)
}

/// Discover doctor extensions with the caller's trust predicate in the same
/// ordered loop as identity validation. A refused script is set aside in
/// [`Discovery::rejected`] and claims no identity; a trusted script with a
/// malformed or duplicate identity is set aside in [`Discovery::invalid`].
pub fn collect_specs_with(
    dir: &Path,
    mut trusted: impl FnMut(&Path) -> bool,
) -> std::io::Result<Discovery> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.as_os_str().as_encoded_bytes();
        if bytes.first() == Some(&b'.') || !bytes.ends_with(b".sh") {
            continue;
        }
        names.push(bytes.to_vec());
    }
    // Byte order is `LC_ALL=C` glob order, the shell loop's input
    // order before the final `sort`.
    names.sort();
    let dir_bytes = dir.as_os_str().as_encoded_bytes();
    let mut specs: Vec<Spec> = Vec::new();
    let mut rejected: Vec<PathBuf> = Vec::new();
    let mut invalid: Vec<(PathBuf, SpecError)> = Vec::new();
    let mut seen: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for name in &names {
        let mut script = dir_bytes.to_vec();
        script.push(b'/');
        script.extend_from_slice(name);
        let script = PathBuf::from(std::ffi::OsStr::from_bytes(&script));
        // Trust still gates every script; a refused one is reported on its
        // own and claims no identity, so a stale link left by a renamed
        // extension neither runs nor collides with its replacement.
        if !trusted(&script) {
            rejected.push(script);
            continue;
        }
        let key = extension_key(name).to_vec();
        let Some(identity) = extension_identity(&key).map(<[u8]>::to_vec) else {
            invalid.push((script, SpecError::InvalidIdentity));
            continue;
        };
        if let Some(claimed_by) = seen.get(&identity) {
            let claimed_by = claimed_by.clone();
            invalid.push((
                script,
                SpecError::DuplicateIdentity {
                    identity,
                    claimed_by,
                },
            ));
            continue;
        }
        seen.insert(identity, name.clone());
        specs.push(Spec { key, script });
    }
    // The shell re-sorts the full rendered lines; sorting the
    // rendered bytes (not just keys) keeps `key`-prefix corners
    // byte-exact.
    specs.sort_by_key(spec_line);
    Ok(Discovery {
        specs,
        invalid,
        rejected,
    })
}

/// The canonical record kind, retained under the coordinator's historical
/// name for callers that only perform extension dispatch.
pub use crate::doctor_runtime::Kind as RecordKind;

/// The `case $kind in ...` dispatch of
/// `_dot_doctor_render_records`: the six known row kinds map to their
/// renderer, everything else to [`RecordKind::Unknown`]. `info` is newer
/// than the other five: an older coordinator renders it as an invalid
/// result, which is why extensions probe for `dot_doctor_info` before
/// calling it (see `doctor-api-v1.tsv`). The `item` and `hint`
/// attachments are not rows; `doctor_records::read` folds them into the
/// row before them and never asks this dispatch about them.
pub fn record_kind(kind: &[u8]) -> RecordKind {
    match kind {
        b"section" => RecordKind::Section,
        b"ok" => RecordKind::Ok,
        b"warn" => RecordKind::Warn,
        b"fail" => RecordKind::Fail,
        b"skip" => RecordKind::Skip,
        b"info" => RecordKind::Info,
        _ => RecordKind::Unknown,
    }
}

/// The `case $relative in ...` guard of `dot_doctor_source`
/// (`lib/dot/public/hook-runtime-v1/doctor-api.sh`): rejects empty values, absolute paths,
/// bare `.`/`..`, any `./`, `../`, `/./`, `/../` segment games,
/// trailing slashes and dot segments, doubled slashes, and embedded
/// newlines or carriage returns. Tabs pass, like the shell.
///
/// Only the shape check is ported: joining under
/// `$DOT_EXTENSIONS_DIR`, the trust validation, and the actual
/// sourcing stay shell-side.
pub fn source_relative_valid(relative: &[u8]) -> bool {
    if relative.is_empty() {
        return false;
    }
    if relative.first() == Some(&b'/') {
        return false;
    }
    if relative == b"." || relative == b".." {
        return false;
    }
    if relative.starts_with(b"./") || relative.starts_with(b"../") {
        return false;
    }
    if relative.ends_with(b"/.") || relative.ends_with(b"/..") || relative.ends_with(b"/") {
        return false;
    }
    if contains(relative, b"/./") || contains(relative, b"/../") || contains(relative, b"//") {
        return false;
    }
    !relative.iter().any(|byte| *byte == b'\n' || *byte == b'\r')
}

/// The `_dot_doctor` summary box text:
/// `printf '%d passed · %d warnings · %d failed'`. The separators
/// are U+00B7 (`·`, bytes C2 B7), passed through verbatim.
pub fn summary_line(pass: u64, warn: u64, fail: u64) -> String {
    format!("{pass} passed · {warn} warnings · {fail} failed")
}

/// The `_dot_doctor` summary-box color: red while anything failed,
/// else yellow while anything warned, else green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryColor {
    /// Failures present (`dot_ui_summary_box red`).
    Red,
    /// Warnings but no failures (`dot_ui_summary_box yellow`).
    Yellow,
    /// Clean (`dot_ui_summary_box green`).
    Green,
}

impl SummaryColor {
    /// Exact `dot_ui_summary_box` color word.
    pub fn name(self) -> &'static str {
        match self {
            SummaryColor::Red => "red",
            SummaryColor::Yellow => "yellow",
            SummaryColor::Green => "green",
        }
    }
}

/// The `_dot_doctor` summary-box color rule: red while `fail_count`
/// is nonzero, else yellow while `warn_count` is nonzero, else
/// green.
pub fn summary_color(fail_count: u64, warn_count: u64) -> SummaryColor {
    if fail_count > 0 {
        SummaryColor::Red
    } else if warn_count > 0 {
        SummaryColor::Yellow
    } else {
        SummaryColor::Green
    }
}

/// The `_dot_doctor` exit contract:
/// `[[ $_DR_FAIL_COUNT -eq 0 && $status -eq 0 ]]` — clean counts and
/// every extension run clean. `extension_status` is the accumulated
/// extension rc (0..255, nonzero once any
/// `_dot_doctor_run_extension` fails).
pub fn overall_ok(fail_count: u64, extension_status: i32) -> bool {
    fail_count == 0 && extension_status == 0
}
