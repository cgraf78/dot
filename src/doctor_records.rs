//! Doctor extension result records: the coordinator-side reader.
//!
//! Extension workers file rows through the public doctor API
//! (`lib/dot/public/hook-runtime-v1/doctor-api.sh`): `dot_doctor_section`,
//! `dot_doctor_ok`, `dot_doctor_warn`, `dot_doctor_fail`,
//! `dot_doctor_skip`, `dot_doctor_info`, and the attachment helpers
//! `dot_doctor_item` and `dot_doctor_hint`, each appending one
//! `kind\tmessage\tdetail\n` row to the result file. Part 1
//! (`doctor_runtime`) owns the coordinator-side rendering and counters,
//! and part 2 (`doctor_paths`) owns the path abbreviators; this module
//! owns reading those rows back.
//!
//! - `kind` travels unvalidated through the writer: an unknown kind still
//!   records, and [`read`] turns it into an invalid-result row.
//! - `item` and `hint` rows are attachments, not rows: [`read`] folds each
//!   into the closest verdict row before it in the same result file. One
//!   with no verdict row before it (first in the file, or right after a
//!   section) is an authoring error and reads as an invalid result.

use std::path::Path;

use crate::doctor_runtime::{Kind, Record};

/// Wire kind of a `dot_doctor_item` attachment row.
const ITEM: &[u8] = b"item";
/// Wire kind of a `dot_doctor_hint` attachment row.
const HINT: &[u8] = b"hint";

/// Read and dispatch a worker result file into canonical coordinator records.
/// Known verdict rows retain their detail column, including an empty one;
/// the renderer omits an empty detail. Item and hint rows attach to the
/// verdict row before them. Unknown kinds, and attachments with no verdict
/// row to attach to, become the coordinator's visible failure record.
pub fn read(result_file: &Path) -> std::io::Result<Vec<Record>> {
    // Bash variables cannot retain NUL bytes. `read -r` silently drops them
    // before applying IFS, so normalize once before reproducing its fields.
    let content = std::fs::read(result_file)?;
    let content = content
        .into_iter()
        .filter(|byte| *byte != b'\0')
        .collect::<Vec<_>>();
    if content.is_empty() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    let final_line_unterminated = !content.ends_with(b"\n");
    let line_count = lines.len();
    for (index, line) in lines.into_iter().enumerate() {
        let (kind_bytes, message, detail) = read_fields(line);
        // `read` returns non-zero at EOF without a delimiter. The shell's
        // `|| [[ -n $kind ]]` rescues only an unterminated row whose parsed
        // first field is non-empty.
        if final_line_unterminated && index + 1 == line_count && kind_bytes.is_empty() {
            continue;
        }
        if kind_bytes == ITEM || kind_bytes == HINT {
            attach(&mut records, kind_bytes, message);
            continue;
        }
        let kind = crate::doctor_coordinator::record_kind(kind_bytes);
        if kind == Kind::Unknown {
            records.push(invalid(kind_bytes));
        } else {
            records.push(Record::bytes(
                kind,
                message,
                (kind != Kind::Section).then_some(detail),
            ));
        }
    }
    Ok(records)
}

/// The coordinator's row for a result line it cannot use.
fn invalid(detail: &[u8]) -> Record {
    Record::bytes(
        Kind::Fail,
        b"doctor extension emitted an invalid result",
        Some(detail),
    )
}

/// Fold one item or hint into the closest verdict row filed before it. A
/// section ends the previous row's scope, so an attachment right after one
/// (or first in the file) has nothing to attach to: report it rather than
/// guess, so the author sees the misplaced call.
fn attach(records: &mut Vec<Record>, kind: &[u8], text: &[u8]) {
    let target = records
        .last_mut()
        .filter(|record| record.kind != Kind::Section);
    match target {
        Some(record) if kind == ITEM => record.items.push(text.to_vec()),
        Some(record) => record.hints.push(text.to_vec()),
        None => {
            let mut detail = kind.to_vec();
            detail.extend_from_slice(b" has no check row before it: ");
            detail.extend_from_slice(text);
            records.push(invalid(&detail));
        }
    }
}

/// Parse `IFS=$'\t' read -r kind message detail`. Tab is IFS whitespace:
/// leading runs are ignored, separators before the first two assigned words
/// collapse, and the last variable receives the untouched remainder with
/// trailing IFS whitespace removed.
fn read_fields(line: &[u8]) -> (&[u8], &[u8], &[u8]) {
    let mut remaining = trim_tabs_start(line);
    let (kind, rest) = read_word(remaining);
    remaining = trim_tabs_start(rest);
    let (message, rest) = read_word(remaining);
    let detail = trim_tabs_end(trim_tabs_start(rest));
    (kind, message, detail)
}

fn read_word(value: &[u8]) -> (&[u8], &[u8]) {
    match value.iter().position(|byte| *byte == b'\t') {
        Some(index) => (&value[..index], &value[index..]),
        None => (value, b""),
    }
}

fn trim_tabs_start(value: &[u8]) -> &[u8] {
    &value[value.iter().take_while(|byte| **byte == b'\t').count()..]
}

fn trim_tabs_end(value: &[u8]) -> &[u8] {
    &value[..value
        .iter()
        .rposition(|byte| *byte != b'\t')
        .map_or(0, |index| index + 1)]
}
