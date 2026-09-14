//! Differential parity tests for `src/doctor_records.rs` against the live
//! shell (`lib/dot/doctor-api.sh`): the `_dot_doctor_record` sink plus
//! the five wrappers (`dot_doctor_section`, `dot_doctor_ok`,
//! `dot_doctor_warn`, `dot_doctor_fail`, `dot_doctor_skip`).
//!
//! Same harness shape as `tests/repos_pull_base.rs`: a fresh `bash`
//! per case with `env_clear` plus `LC_ALL=C`, values traveling as
//! `$2..` argv (byte-exact, so tab/newline/CR and non-UTF8 fixtures
//! need no quoting), and the result-file selection exported from `$2`
//! inside the snippet (`unset` rows drop it first). Each row compares
//! the exit status and the full result-file bytes; error rows pin
//! that the file is left untouched.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::doctor_records::{fail, ok, record, section, skip, warn};
use dot::test_support::TempDir;

/// Sources for the doctor-records cluster: the extension API only.
/// Sourcing it defines functions with no side effects.
const SOURCES: &str = concat!(".", " \"$1/lib/dot/doctor-api.sh\"\n");

/// Run one shell snippet with the records runtime sourced. `argv`
/// arrives as `$2..` (byte-exact, for hostile field values).
fn shell_run(home: &Path, argv: &[&OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Which shell function the row invokes, with byte-exact arguments.
enum Call {
    /// `_dot_doctor_record kind message detail` (always three fields).
    Record(Vec<u8>, Vec<u8>, Vec<u8>),
    /// `dot_doctor_section` with N fields.
    Section(Vec<Vec<u8>>),
    /// `dot_doctor_ok` with N fields.
    Ok(Vec<Vec<u8>>),
    /// `dot_doctor_warn` with N fields.
    Warn(Vec<Vec<u8>>),
    /// `dot_doctor_fail` with N fields.
    Fail(Vec<Vec<u8>>),
    /// `dot_doctor_skip` with N fields.
    Skip(Vec<Vec<u8>>),
}

/// Shell function name for a call.
fn func_name(call: &Call) -> &'static str {
    match call {
        Call::Record(..) => "_dot_doctor_record",
        Call::Section(_) => "dot_doctor_section",
        Call::Ok(_) => "dot_doctor_ok",
        Call::Warn(_) => "dot_doctor_warn",
        Call::Fail(_) => "dot_doctor_fail",
        Call::Skip(_) => "dot_doctor_skip",
    }
}

/// Field arguments of a call, in order.
fn func_args(call: &Call) -> Vec<&[u8]> {
    match call {
        Call::Record(kind, message, detail) => vec![kind, message, detail],
        Call::Section(args)
        | Call::Ok(args)
        | Call::Warn(args)
        | Call::Fail(args)
        | Call::Skip(args) => args.iter().map(Vec::as_slice).collect(),
    }
}

/// How the result file starts the row.
enum Setup {
    /// Empty regular file.
    Empty,
    /// Regular file already holding these bytes (append order pin).
    WithContent(Vec<u8>),
    /// Path names nothing at all.
    Missing,
    /// Path is a directory (shell `-f` fails).
    Dir,
}

/// How the snippet selects the result file.
enum Select {
    /// `export DOT_DOCTOR_RESULT_FILE="$2"` (argv carries the path).
    Export,
    /// `unset DOT_DOCTOR_RESULT_FILE` (argv `$2` unused).
    Unset,
    /// `export DOT_DOCTOR_RESULT_FILE=""` (argv `$2` unused).
    Empty,
}

/// Read the result-file state the way the comparison needs it: file
/// bytes when a regular file is present, a sentinel otherwise (the
/// shell never creates, truncates, or replaces the selection on any
/// path, so error rows must round-trip the sentinel or the setup
/// bytes unchanged).
fn file_state(path: &Path) -> Vec<u8> {
    match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => b"<unreadable>\n".to_vec(),
    }
}

/// Run one row on both engines and compare status plus file bytes.
fn check_row(name: &str, call: &Call, setup: Setup, select: Select) {
    let dir = TempDir::new("doctor-records").expect("fixture dir");
    // Twin selections: each engine appends to its own file so the
    // two runs never share bytes; the comparison is across twins.
    let shell_results = dir.path().join("shell-results");
    let rust_results = dir.path().join("rust-results");
    for results in [&shell_results, &rust_results] {
        match &setup {
            Setup::Empty => {
                std::fs::write(results, b"").expect("empty results");
            }
            Setup::WithContent(bytes) => {
                std::fs::write(results, bytes).expect("seeded results");
            }
            Setup::Missing => {}
            Setup::Dir => {
                std::fs::create_dir_all(results).expect("results dir");
            }
        }
    }
    let before = file_state(&shell_results);

    let args = func_args(call);
    let mut argv: Vec<&OsStr> = Vec::with_capacity(args.len() + 1);
    let results_text = shell_results.to_string_lossy().into_owned();
    argv.push(OsStr::new(&results_text));
    for arg in &args {
        argv.push(OsStr::from_bytes(arg));
    }
    let mut refs = String::new();
    for index in 0..args.len() {
        refs.push_str(&format!(" \"${}\"", index + 3));
    }
    let prefix = match select {
        Select::Export => "export DOT_DOCTOR_RESULT_FILE=\"$2\"; ",
        Select::Unset => "unset DOT_DOCTOR_RESULT_FILE; ",
        Select::Empty => "export DOT_DOCTOR_RESULT_FILE=\"\"; ",
    };
    let snippet = format!(
        "{prefix}{} {refs}; code=$?; printf 'rc=%s\\n' \"$code\"\n",
        func_name(call),
    );
    let (code, out, err) = shell_run(dir.path(), &argv, &snippet);
    assert_eq!(code, 0, "harness exit for {name}");
    assert!(
        err.is_empty(),
        "shell stderr for {name}: {}",
        String::from_utf8_lossy(&err)
    );
    let shell_out = String::from_utf8(out).expect("shell dump");
    let shell_rc: i32 = shell_out
        .strip_prefix("rc=")
        .and_then(|rest| rest.trim_end().parse().ok())
        .unwrap_or_else(|| panic!("shell rc line for {name}: {shell_out:?}"));
    let shell_after = file_state(&shell_results);

    let selected: Option<&Path> = match select {
        Select::Export => Some(&rust_results),
        Select::Unset => None,
        Select::Empty => Some(Path::new("")),
    };
    let borrowed: Vec<&[u8]> = args.clone();
    let rust_rc = match call {
        Call::Record(kind, message, detail) => record(selected, kind, message, detail)
            .err()
            .map(|err| err.code()),
        Call::Section(_) => section(selected, &borrowed).err().map(|err| err.code()),
        Call::Ok(_) => ok(selected, &borrowed).err().map(|err| err.code()),
        Call::Warn(_) => warn(selected, &borrowed).err().map(|err| err.code()),
        Call::Fail(_) => fail(selected, &borrowed).err().map(|err| err.code()),
        Call::Skip(_) => skip(selected, &borrowed).err().map(|err| err.code()),
    }
    .unwrap_or(0);
    let rust_after = file_state(&rust_results);

    assert_eq!(rust_rc, shell_rc, "status for {name}");
    assert_eq!(rust_after, shell_after, "result bytes for {name}");
    if rust_rc != 0 {
        assert_eq!(shell_after, before, "error row leaves file alone: {name}");
    }
}

/// Short byte-string helper for the row table.
fn b(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

#[test]
fn doctor_record_rows_agree() {
    // Sink happy paths: verbatim rows, empty fields, unknown kinds.
    check_row(
        "record-ok",
        &Call::Record(b("ok"), b("all good"), b("detail here")),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "record-section-empty-detail",
        &Call::Record(b("section"), b("Title"), b("")),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "record-custom-kind-passes-through",
        &Call::Record(b("bogus"), b("k"), b("v")),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "record-empty-message",
        &Call::Record(b("ok"), b(""), b("d")),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "record-empty-both",
        &Call::Record(b("skip"), b(""), b("")),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "record-nonutf8",
        &Call::Record(b("warn"), vec![0xff, 0xfe, b' ', b'k'], vec![b'd', 0xff]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "record-appends-in-order",
        &Call::Record(b("warn"), b("second"), b("d2")),
        Setup::WithContent(b"ok\tfirst\t\n".to_vec()),
        Select::Export,
    );
    // Sink field validation: tab, newline, and CR rejected per field.
    for (name, kind, message, detail) in [
        ("record-msg-tab", "ok", b("a\tb"), b("d")),
        ("record-msg-newline", "ok", b("a\nb"), b("d")),
        ("record-msg-cr", "ok", b("a\rb"), b("d")),
        ("record-det-tab", "warn", b("m"), b("a\tb")),
        ("record-det-newline", "warn", b("m"), b("a\nb")),
        ("record-det-cr", "warn", b("m"), b("a\rb")),
    ] {
        check_row(
            name,
            &Call::Record(b(kind), message, detail),
            Setup::Empty,
            Select::Export,
        );
    }
    // Sink file guard: unset, empty, missing, and directory.
    check_row(
        "record-unset-file",
        &Call::Record(b("ok"), b("m"), b("d")),
        Setup::Empty,
        Select::Unset,
    );
    check_row(
        "record-empty-filevar",
        &Call::Record(b("ok"), b("m"), b("d")),
        Setup::Empty,
        Select::Empty,
    );
    check_row(
        "record-missing-path",
        &Call::Record(b("ok"), b("m"), b("d")),
        Setup::Missing,
        Select::Export,
    );
    check_row(
        "record-dir-path",
        &Call::Record(b("ok"), b("m"), b("d")),
        Setup::Dir,
        Select::Export,
    );
    // Wrapper happy paths: section takes one field, verdicts one or two.
    check_row(
        "section-one",
        &Call::Section(vec![b("Title")]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "ok-one",
        &Call::Ok(vec![b("m")]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "ok-two",
        &Call::Ok(vec![b("m"), b("d")]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "warn-two",
        &Call::Warn(vec![b("m"), b("d")]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "fail-two",
        &Call::Fail(vec![b("m"), b("d")]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "skip-two",
        &Call::Skip(vec![b("m"), b("d")]),
        Setup::Empty,
        Select::Export,
    );
    check_row(
        "ok-nonutf8",
        &Call::Ok(vec![vec![0xff, b'k'], b("d")]),
        Setup::Empty,
        Select::Export,
    );
    // Wrapper arity: section takes exactly one, verdicts one or two.
    for (name, call) in [
        ("section-zero", Call::Section(vec![])),
        ("section-two", Call::Section(vec![b("a"), b("b")])),
        ("ok-zero", Call::Ok(vec![])),
        ("ok-three", Call::Ok(vec![b("a"), b("b"), b("c")])),
        ("warn-zero", Call::Warn(vec![])),
        ("warn-three", Call::Warn(vec![b("a"), b("b"), b("c")])),
        ("fail-zero", Call::Fail(vec![])),
        ("fail-three", Call::Fail(vec![b("a"), b("b"), b("c")])),
        ("skip-zero", Call::Skip(vec![])),
        ("skip-three", Call::Skip(vec![b("a"), b("b"), b("c")])),
    ] {
        check_row(name, &call, Setup::Empty, Select::Export);
    }
    // Wrapper validation flows through the sink guard.
    check_row(
        "fail-msg-tab",
        &Call::Fail(vec![b("a\tb"), b("d")]),
        Setup::Empty,
        Select::Export,
    );
}
