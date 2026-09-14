//! Doctor extension result records (slice 52: doctor layer, part 3).
//!
//! Ports the record family from `lib/dot/doctor-api.sh`: the private
//! `_dot_doctor_record` sink plus the five public wrappers built on
//! it — `dot_doctor_section`, `dot_doctor_ok`, `dot_doctor_warn`,
//! `dot_doctor_fail`, and `dot_doctor_skip`. Part 1 (`doctor_runtime`)
//! owns the coordinator-side rendering and counters, and part 2
//! (`doctor_paths`) owns the path abbreviators; this module owns how
//! extension workers file result rows for the coordinator to render.
//!
//! Parity decisions:
//! - The sink appends one `kind\tmessage\tdetail\n` row, exactly
//!   like the shell's `printf '%s\t%s\t%s\n' ... >>file`. `kind`
//!   travels unvalidated: an unknown kind still records (the
//!   coordinator's render step, not the sink, rejects it).
//! - The result-file guard mirrors the shell short-circuit
//!   (`[[ -n ${DOT_DOCTOR_RESULT_FILE:-} && -f ... ]]`): an unset or
//!   empty selection, a missing path, or a non-regular file (a
//!   directory included) all surface as [`Error::NoResultFile`].
//! - Field validation is byte-exact (`\t`, `\n`, `\r` rejected in
//!   message and detail, like the shell `case` patterns under
//!   `LC_ALL=C`), so inputs travel as `&[u8]`, matching the
//!   `doctor_runtime` precedent.
//! - Wrapper arity mirrors the shell `$#` tests and travels as an
//!   argument slice: [`section`] takes exactly one field, the four
//!   verdict wrappers take one or two. A one-field call records an
//!   empty detail, like the shell's `"${2:-}"`.

use std::path::Path;

/// Doctor record failure, carrying the shell's exit code.
///
/// Both sink failures surface as statuses, mirroring the
/// `doctor_paths::Error` convention.
/// The shell families share this module's two codes: the sink guard
/// fails first (status 1), field and arity validation second
/// (status 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No usable result file is selected (shell `return 1`): the
    /// selection is unset or empty, or it names no regular file.
    NoResultFile,
    /// A field carries a tab, newline, or carriage return, or a
    /// wrapper saw the wrong argument count (shell `return 2`).
    Invalid,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::NoResultFile => 1,
            Error::Invalid => 2,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoResultFile => write!(f, "doctor result file is unavailable"),
            Error::Invalid => write!(f, "invalid doctor result record"),
        }
    }
}

impl std::error::Error for Error {}

/// Forbidden field bytes: tab, newline, and carriage return.
///
/// The shell rejects a message or detail containing any of these so
/// the tab-separated record file stays one row per line.
const FORBIDDEN: &[u8] = b"\t\n\r";

/// True when `field` carries no tab, newline, or carriage return.
fn field_is_clean(field: &[u8]) -> bool {
    !field.iter().any(|byte| FORBIDDEN.contains(byte))
}

/// `_dot_doctor_record`: append one `kind\tmessage\tdetail` row.
///
/// `result_file` is the selected `DOT_DOCTOR_RESULT_FILE` (`None`
/// when unset or empty); it must name an existing regular file,
/// otherwise [`Error::NoResultFile`] surfaces. Message and detail
/// must be free of tab, newline, and carriage return, otherwise
/// [`Error::Invalid`] surfaces. `kind` is written verbatim.
pub fn record(
    result_file: Option<&Path>,
    kind: &[u8],
    message: &[u8],
    detail: &[u8],
) -> Result<(), Error> {
    let path = result_file.filter(|path| !path.as_os_str().is_empty());
    let path = match path {
        Some(path) if path.is_file() => path,
        _ => return Err(Error::NoResultFile),
    };
    if !field_is_clean(message) || !field_is_clean(detail) {
        return Err(Error::Invalid);
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|_| Error::NoResultFile)?;
    file.write_all(kind)
        .and_then(|()| file.write_all(b"\t"))
        .and_then(|()| file.write_all(message))
        .and_then(|()| file.write_all(b"\t"))
        .and_then(|()| file.write_all(detail))
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|_| Error::NoResultFile)?;
    Ok(())
}

/// `dot_doctor_section`: record a section row from exactly one field.
///
/// The detail column stays empty, like the shell's two-argument
/// `_dot_doctor_record section "$1"` call. Any other argument count
/// surfaces as [`Error::Invalid`] (shell `return 2`).
pub fn section(result_file: Option<&Path>, args: &[&[u8]]) -> Result<(), Error> {
    let [message] = args else {
        return Err(Error::Invalid);
    };
    record(result_file, b"section", message, b"")
}

/// `dot_doctor_ok`: record an ok row from one or two fields.
///
/// A single field records an empty detail, like the shell's
/// `"${2:-}"`. Any other argument count surfaces as
/// [`Error::Invalid`] (shell `return 2`).
pub fn ok(result_file: Option<&Path>, args: &[&[u8]]) -> Result<(), Error> {
    verdict(result_file, b"ok", args)
}

/// `dot_doctor_warn`: record a warn row from one or two fields.
///
/// Arity and detail rules mirror [`ok`].
pub fn warn(result_file: Option<&Path>, args: &[&[u8]]) -> Result<(), Error> {
    verdict(result_file, b"warn", args)
}

/// `dot_doctor_fail`: record a fail row from one or two fields.
///
/// Arity and detail rules mirror [`ok`].
pub fn fail(result_file: Option<&Path>, args: &[&[u8]]) -> Result<(), Error> {
    verdict(result_file, b"fail", args)
}

/// `dot_doctor_skip`: record a skip row from one or two fields.
///
/// Arity and detail rules mirror [`ok`].
pub fn skip(result_file: Option<&Path>, args: &[&[u8]]) -> Result<(), Error> {
    verdict(result_file, b"skip", args)
}

/// Shared verdict sink for [`ok`], [`warn`], [`fail`], and [`skip`]:
/// one or two fields, with the detail defaulting to empty.
fn verdict(result_file: Option<&Path>, kind: &[u8], args: &[&[u8]]) -> Result<(), Error> {
    let (message, detail) = match args {
        [message] => (*message, b"".as_slice()),
        [message, detail] => (*message, *detail),
        _ => return Err(Error::Invalid),
    };
    record(result_file, kind, message, detail)
}
