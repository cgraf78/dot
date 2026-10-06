//! Contracts for doctor extension result records: the public writers in
//! `lib/dot/public/hook-runtime-v1/doctor-api.sh` and the coordinator's
//! reader, [`dot::doctor_records::read`].

use std::path::Path;
use std::process::{Command, Stdio};

use dot::doctor_records::read;
use dot::doctor_runtime::{Kind, Palette, render};
use dot_test_support::TempDir;

fn fixture(tag: &str, bytes: &[u8]) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new(tag).expect("fixture");
    let path = dir.write("results", bytes);
    (dir, path)
}

/// Run `script` with the public doctor API sourced and `result` selected as
/// `DOT_DOCTOR_RESULT_FILE` (unset when `None`); returns stdout. `rc CMD...`
/// prints one call's status.
fn doctor_api(result: Option<&Path>, script: &str) -> String {
    let body = format!(
        ". \"$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/doctor-api.sh\"\n\
         rc() {{ if \"$@\"; then printf '0\\n'; else printf '%s\\n' \"$?\"; fi; }}\n{script}"
    );
    let mut command = Command::new(dot_test_support::bash());
    command
        .args(["--noprofile", "--norc", "-c", &body])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    if let Some(result) = result {
        command.env("DOT_DOCTOR_RESULT_FILE", result);
    }
    let output = command.output().expect("run the doctor API");
    assert!(
        output.status.success(),
        "doctor API script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

#[test]
fn render_empty_message_agrees() {
    let (_dir, path) = fixture("records-empty", b"ok\t\t\n");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 1);
    // An empty detail renders like an omitted one (no bare `()`).
    assert_eq!(render(&records, &Palette::empty()), b"  \xe2\x9c\x93 \n");
}

#[test]
fn render_repeated_and_leading_tabs_agrees() {
    let (_dir, path) = fixture("records-tabs", b"\t\tok\t\tmessage\tdeep\tdetail\t\t\n");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, Kind::Ok);
    assert_eq!(records[0].message, b"message");
    assert_eq!(records[0].detail, Some(b"deep\tdetail".to_vec()));
}

#[test]
fn render_unterminated_rows_agree() {
    let (_dir, path) = fixture("records-unterminated", b"warn\tmessage\tdetail");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, Kind::Warn);
    let (_dir, empty) = fixture("records-empty-tail", b"\t\t");
    assert!(read(&empty).expect("read empty kind").is_empty());
}

#[test]
fn render_whitespace_only_rows_agree() {
    let (_dir, path) = fixture("records-whitespace", b"\n\t\t\n");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|row| row.kind == Kind::Fail));
    assert!(
        records
            .iter()
            .all(|row| row.message == b"doctor extension emitted an invalid result")
    );
}

#[test]
fn doctor_record_rows_agree() {
    let (_dir, path) = fixture("records-writers", b"");
    let statuses = doctor_api(
        Some(&path),
        r#"
dot_doctor_section Section
dot_doctor_ok ok
dot_doctor_warn warn detail
dot_doctor_fail fail
dot_doctor_skip skip ''
dot_doctor_info info fact
_dot_doctor_record future message detail
rc dot_doctor_info
rc dot_doctor_ok
rc dot_doctor_ok one two three
rc dot_doctor_section a b
rc dot_doctor_warn $'bad\nfield'
rc dot_doctor_fail message $'bad\rdetail'
"#,
    );
    // Arity and forbidden bytes are usage errors that record nothing.
    assert_eq!(statuses, "2\n2\n2\n2\n2\n2\n");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 7);
    assert_eq!(records[0].kind, Kind::Section);
    assert_eq!(records[2].kind, Kind::Warn);
    assert_eq!(records[2].detail, Some(b"detail".to_vec()));
    assert_eq!(records[5].kind, Kind::Info);
    assert_eq!(records[5].detail, Some(b"fact".to_vec()));
    // An unknown kind records, and the reader turns it into a failure.
    assert_eq!(records[6].kind, Kind::Fail);
    assert_eq!(
        std::fs::read(&path)
            .expect("raw")
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        7,
        "rejected calls append nothing"
    );
    // Without a usable result file every writer refuses with status 1.
    let scratch = TempDir::new("records-no-file").expect("scratch");
    assert_eq!(
        doctor_api(None, "rc dot_doctor_ok m\n"),
        "1\n",
        "unset result file"
    );
    assert_eq!(
        doctor_api(Some(scratch.path()), "rc dot_doctor_ok m\n"),
        "1\n",
        "a directory is not a result file"
    );
}

#[test]
fn attachments_fold_into_the_row_before_them() {
    let (_dir, path) = fixture("records-attachments", b"");
    doctor_api(
        Some(&path),
        r#"
dot_doctor_section Section
dot_doctor_warn stale merged
dot_doctor_item one
dot_doctor_hint 'run cleanup'
dot_doctor_item two
dot_doctor_info fact
dot_doctor_item detail
"#,
    );
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 3, "attachments are not rows: {records:?}");
    assert_eq!(records[1].items, vec![b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(records[1].hints, vec![b"run cleanup".to_vec()]);
    assert_eq!(records[2].items, vec![b"detail".to_vec()]);
    // Hints render after every item, whatever the call order.
    assert_eq!(
        String::from_utf8(render(&records[1..2], &Palette::empty())).expect("utf8"),
        "  ⚠ stale\n    merged\n    - one\n    - two\n    → run cleanup\n"
    );
}

#[test]
fn orphan_attachments_read_as_invalid_results() {
    // First in the file, and right after a section: nothing to attach to.
    let (_dir, path) = fixture(
        "records-orphans",
        b"hint\tfirst\t\nsection\tS\t\nitem\tlost\t\n",
    );
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].kind, Kind::Fail);
    assert_eq!(
        records[0].detail,
        Some(b"hint has no check row before it: first".to_vec())
    );
    assert_eq!(records[1].kind, Kind::Section);
    assert_eq!(records[2].kind, Kind::Fail);
    assert_eq!(
        records[2].detail,
        Some(b"item has no check row before it: lost".to_vec())
    );
}

#[test]
fn attachment_writers_take_exactly_one_clean_field() {
    let (_dir, path) = fixture("records-attachment-arity", b"");
    let statuses = doctor_api(
        Some(&path),
        r#"
rc dot_doctor_item
rc dot_doctor_item a b
rc dot_doctor_hint
rc dot_doctor_item $'bad\nline'
rc dot_doctor_hint $'bad\ttab'
rc dot_doctor_item ''
"#,
    );
    assert_eq!(statuses, "2\n2\n2\n2\n2\n0\n");
    assert_eq!(std::fs::read(&path).expect("raw"), b"item\t\t\n");
    assert_eq!(doctor_api(None, "rc dot_doctor_item a\n"), "1\n");
}
