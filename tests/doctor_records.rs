//! Native contracts for doctor extension result records.

use dot::doctor_records::{Error, fail, ok, read, record, section, skip, warn};
use dot::doctor_runtime::{Kind, Palette, render};
use dot_test_support::TempDir;

fn fixture(tag: &str, bytes: &[u8]) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new(tag).expect("fixture");
    let path = dir.write("results", bytes);
    (dir, path)
}

#[test]
fn render_empty_message_agrees() {
    let (_dir, path) = fixture("records-empty", b"ok\t\t\n");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 1);
    assert_eq!(render(&records, &Palette::empty()), b"  \xe2\x9c\x93  ()\n");
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
    section(Some(&path), &[b"Section"]).expect("section");
    ok(Some(&path), &[b"ok"]).expect("ok");
    warn(Some(&path), &[b"warn", b"detail"]).expect("warn");
    fail(Some(&path), &[b"fail"]).expect("fail");
    skip(Some(&path), &[b"skip", b""]).expect("skip");
    record(Some(&path), b"future", b"message", b"detail").expect("unknown kind");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 6);
    assert_eq!(records[0].kind, Kind::Section);
    assert_eq!(records[5].kind, Kind::Fail);
    assert_eq!(record(None, b"ok", b"m", b"d"), Err(Error::NoResultFile));
    assert_eq!(ok(Some(&path), &[]), Err(Error::Invalid));
    assert_eq!(section(Some(&path), &[b"a", b"b"]), Err(Error::Invalid));
    assert_eq!(
        record(Some(&path), b"ok", b"bad\nfield", b""),
        Err(Error::Invalid)
    );
}
