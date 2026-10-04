//! Native contracts for doctor extension result records.

use dot::doctor_records::{Error, fail, hint, info, item, ok, read, record, section, skip, warn};
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
    section(Some(&path), &[b"Section"]).expect("section");
    ok(Some(&path), &[b"ok"]).expect("ok");
    warn(Some(&path), &[b"warn", b"detail"]).expect("warn");
    fail(Some(&path), &[b"fail"]).expect("fail");
    skip(Some(&path), &[b"skip", b""]).expect("skip");
    info(Some(&path), &[b"info", b"fact"]).expect("info");
    record(Some(&path), b"future", b"message", b"detail").expect("unknown kind");
    let records = read(&path).expect("read");
    assert_eq!(records.len(), 7);
    assert_eq!(records[0].kind, Kind::Section);
    assert_eq!(records[5].kind, Kind::Info);
    assert_eq!(records[5].detail, Some(b"fact".to_vec()));
    assert_eq!(records[6].kind, Kind::Fail);
    assert_eq!(info(Some(&path), &[]), Err(Error::Invalid));
    assert_eq!(record(None, b"ok", b"m", b"d"), Err(Error::NoResultFile));
    assert_eq!(ok(Some(&path), &[]), Err(Error::Invalid));
    assert_eq!(section(Some(&path), &[b"a", b"b"]), Err(Error::Invalid));
    assert_eq!(
        record(Some(&path), b"ok", b"bad\nfield", b""),
        Err(Error::Invalid)
    );
}

#[test]
fn attachments_fold_into_the_row_before_them() {
    let (_dir, path) = fixture("records-attachments", b"");
    section(Some(&path), &[b"Section"]).expect("section");
    warn(Some(&path), &[b"stale", b"merged"]).expect("warn");
    item(Some(&path), &[b"one"]).expect("item");
    hint(Some(&path), &[b"run cleanup"]).expect("hint");
    item(Some(&path), &[b"two"]).expect("item after hint");
    info(Some(&path), &[b"fact"]).expect("info");
    item(Some(&path), &[b"detail"]).expect("info item");
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
    assert_eq!(item(Some(&path), &[]), Err(Error::Invalid));
    assert_eq!(item(Some(&path), &[b"a", b"b"]), Err(Error::Invalid));
    assert_eq!(hint(Some(&path), &[]), Err(Error::Invalid));
    assert_eq!(item(Some(&path), &[b"bad\nline"]), Err(Error::Invalid));
    assert_eq!(hint(Some(&path), &[b"bad\ttab"]), Err(Error::Invalid));
    assert_eq!(item(None, &[b"a"]), Err(Error::NoResultFile));
    item(Some(&path), &[b""]).expect("empty item records");
    assert_eq!(std::fs::read(&path).expect("raw"), b"item\t\t\n");
}
