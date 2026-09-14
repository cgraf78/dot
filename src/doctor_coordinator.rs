//! Doctor coordinator helpers (slice 56: doctor layer, part 4).
//!
//! Ports the pure decision points of the `_dot_doctor` pipeline from
//! `lib/dot/doctor.sh` plus the one unclaimed validator from
//! `lib/dot/doctor-api.sh`. Taken lanes own the neighboring pieces
//! and are deliberately not duplicated here: part 1 (`doctor_runtime`)
//! owns the `_dr_*` result rendering and counters, part 2
//! (`doctor_paths`) owns the path abbreviators, and part 3
//! (`doctor_records`) owns the extension-side record sink. This module
//! owns what sits between them: how the coordinator discovers
//! extension specs, how it dispatches result rows back to renderers,
//! and how it summarizes the run.
//!
//! Parity decisions:
//! - The discovery loop in `_dot_doctor_extension_specs` ends in
//!   `done | LC_ALL=C sort`, so the pipeline status is always
//!   `sort`'s: an invalid or duplicate identity prints its `dot: ...`
//!   line to stderr and truncates the listing, but the exit status
//!   stays 0. [`collect_specs`] mirrors that exactly — the identity
//!   failure travels as [`Discovery::error`] alongside the partial
//!   listing, never as an `Err` — instead of "fixing" the swallowed
//!   status the shell suite pins.
//! - Per-file trust validation (`_dot_extension_file_validate`) runs
//!   before key derivation in the shell but belongs to the
//!   extension-trust lane; [`collect_specs`] assumes a trusted
//!   listing the way the differential rows stub that check to
//!   success, and documents the seam.
//! - Names travel as `&[u8]` throughout (byte sort is `LC_ALL=C`
//!   sort; the identity character classes are ASCII ranges), so
//!   non-UTF8 entry names behave like the shell's.

use std::collections::HashSet;
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

/// Identity failure of `_dot_doctor_extension_specs`, carrying the
/// exact stderr line. Both spellings exit 1 in the shell loop; the
/// coordinator pipe in front of `sort` swallows that status (see the
/// module docs), so [`collect_specs`] reports these through
/// [`Discovery::error`] with [`SpecError::code`] pinned at 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// `dot: invalid doctor extension identity: <basename>` — the
    /// key matches neither the bare nor the numerically prefixed
    /// identity shape.
    InvalidIdentity {
        /// Offending file basename (`${script##*/}`).
        file_name: Vec<u8>,
    },
    /// `dot: duplicate doctor extension identity: <identity>` — two
    /// keys (`20-foo.sh` and `foo.sh`, say) claim one identity.
    DuplicateIdentity {
        /// Twice-claimed identity (the regex's second group).
        identity: Vec<u8>,
    },
}

impl SpecError {
    /// Shell loop status for this failure (always 1; the coordinator
    /// pipe swallows it before callers can see it).
    pub fn code(self) -> i32 {
        match self {
            SpecError::InvalidIdentity { .. } => 1,
            SpecError::DuplicateIdentity { .. } => 1,
        }
    }

    /// Exact stderr bytes the shell's `printf ... >&2` emits,
    /// trailing newline included.
    pub fn message(&self) -> Vec<u8> {
        match self {
            SpecError::InvalidIdentity { file_name } => {
                let mut line = b"dot: invalid doctor extension identity: ".to_vec();
                line.extend_from_slice(file_name);
                line.push(b'\n');
                line
            }
            SpecError::DuplicateIdentity { identity } => {
                let mut line = b"dot: duplicate doctor extension identity: ".to_vec();
                line.extend_from_slice(identity);
                line.push(b'\n');
                line
            }
        }
    }
}

/// Outcome of [`collect_specs`]: the sorted listing plus, when the
/// shell would have errored mid-loop, the truncating failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Sorted specs printed before any failure (the whole listing
    /// when [`error`](Discovery::error) is `None`).
    pub specs: Vec<Spec>,
    /// Identity failure that stopped the shell loop, if any. The
    /// shell still exits 0 through the `sort` pipe; callers surface
    /// [`SpecError::message`] on stderr to match.
    pub error: Option<SpecError>,
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
/// Entries stop at the first invalid or duplicate identity, exactly
/// like the shell loop's `return 1`: earlier specs stay listed
/// (re-sorted by the trailing `sort`) and the failure surfaces as
/// [`Discovery::error`], with the shell's 0-through-the-pipe status
/// left for the caller to mirror (the harness asserts it).
/// Per-file trust validation stays shell-side (see the module docs).
///
/// Only I/O failures (an unreadable `dir`) surface as `Err`: the
/// shell's missing-root early return runs before this logic, so
/// callers pass a directory they already know exists.
pub fn collect_specs(dir: &Path) -> std::io::Result<Discovery> {
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
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for name in &names {
        let key = extension_key(name).to_vec();
        let identity = match extension_identity(&key) {
            Some(identity) => identity.to_vec(),
            None => {
                return Ok(Discovery {
                    specs,
                    error: Some(SpecError::InvalidIdentity {
                        file_name: name.clone(),
                    }),
                });
            }
        };
        if !seen.insert(identity.clone()) {
            return Ok(Discovery {
                specs,
                error: Some(SpecError::DuplicateIdentity { identity }),
            });
        }
        let mut script = dir_bytes.to_vec();
        script.push(b'/');
        script.extend_from_slice(name);
        specs.push(Spec {
            key,
            script: PathBuf::from(std::ffi::OsStr::from_bytes(&script)),
        });
    }
    // The shell re-sorts the full rendered lines; sorting the
    // rendered bytes (not just keys) keeps `key`-prefix corners
    // byte-exact.
    specs.sort_by_key(spec_line);
    Ok(Discovery { specs, error: None })
}

/// Which `_dr_*` renderer `_dot_doctor_render_records` dispatches a
/// result-file row to, by its `kind` column. Known kinds render;
/// anything else (empty kinds included) fails as
/// `doctor extension emitted an invalid result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// `section`: `_dr_section "$message"`.
    Section,
    /// `ok`: `_dr_ok "$message" "$detail"`.
    Ok,
    /// `warn`: `_dr_warn "$message" "$detail"`.
    Warn,
    /// `fail`: `_dr_fail "$message" "$detail"`.
    Fail,
    /// `skip`: `_dr_skip "$message" "$detail"`.
    Skip,
    /// Any other kind: `_dr_fail 'doctor extension emitted an
    /// invalid result' "$kind"`.
    Unknown,
}

/// The `case $kind in ...` dispatch of
/// `_dot_doctor_render_records`: the five known kinds map to their
/// renderer, everything else to [`RecordKind::Unknown`].
pub fn record_kind(kind: &[u8]) -> RecordKind {
    match kind {
        b"section" => RecordKind::Section,
        b"ok" => RecordKind::Ok,
        b"warn" => RecordKind::Warn,
        b"fail" => RecordKind::Fail,
        b"skip" => RecordKind::Skip,
        _ => RecordKind::Unknown,
    }
}

/// The `case $relative in ...` guard of `dot_doctor_source`
/// (`lib/dot/doctor-api.sh`): rejects empty values, absolute paths,
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
