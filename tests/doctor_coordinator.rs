//! Differential parity tests for `src/doctor_coordinator.rs` against
//! the live shell (`lib/dot/doctor.sh` and the `dot_doctor_source`
//! validator from `lib/dot/doctor-api.sh`): extension spec-key and
//! identity derivation, spec discovery over a `doctor.d` directory,
//! result-record dispatch, extension source-path validation, and the
//! coordinator summary line, color, and exit contract.
//!
//! Same harness shape as `tests/repos_pull_base.rs`: a fresh `bash`
//! per row with `env_clear` plus `LC_ALL=C`, hostile values traveling
//! as `$2..` argv (byte-exact, so tabs, newlines, and non-UTF8 names
//! need no quoting). Rows compare exit status and both streams byte
//! for byte.
//!
//! Two shell quirks the rows pin rather than fix: discovery truncates
//! (but still exits 0) on the first bad identity because the loop
//! drains into `| LC_ALL=C sort`, and the trust-validator stubs stand
//! in for the extension-trust lane (`collect_specs` documents the
//! seam).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::doctor_coordinator::{
    Discovery, RecordKind, collect_specs, extension_identity, extension_key, overall_ok,
    record_kind, source_relative_valid, summary_color, summary_line,
};
use dot::test_support::TempDir;

/// Sources for the coordinator cluster: `doctor.sh` defines the
/// discovery and render functions with no source-time side effects.
const COORDINATOR_SOURCES: &str = concat!(".", " \"$1/lib/dot/doctor.sh\"\n");

/// Sources for the extension-API validator under test.
const API_SOURCES: &str = concat!(".", " \"$1/lib/dot/doctor-api.sh\"\n");

/// Trust-validator stubs: the extension-trust lane owns these;
/// discovery and sourcing rows stub them to success.
const TRUST_STUBS: &str = concat!(
    "_dot_extensions_enabled() { return 0; }\n",
    "_dot_extension_root_validate() { return 0; }\n",
    "_dot_extension_directory_validate() { return 0; }\n",
    "_dot_extension_file_validate() { return 0; }\n",
);

/// Renderer stubs: record the dispatch decision the way the real
/// `_dr_*` arity works (`render_records` always passes message and
/// detail, section only the message).
const RENDER_STUBS: &str = concat!(
    "_dr_section() { printf 'RENDER|section|%s|\\n' \"$1\"; }\n",
    "_dr_ok() { printf 'RENDER|ok|%s|%s\\n' \"$1\" \"$2\"; }\n",
    "_dr_warn() { printf 'RENDER|warn|%s|%s\\n' \"$1\" \"$2\"; }\n",
    "_dr_fail() { printf 'RENDER|fail|%s|%s\\n' \"$1\" \"$2\"; }\n",
    "_dr_skip() { printf 'RENDER|skip|%s|%s\\n' \"$1\" \"$2\"; }\n",
);

/// Run one shell snippet. `argv` arrives as `$2..` (byte-exact).
fn shell_run(home: &Path, argv: &[&OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(snippet);
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

/// Split a byte row for argv embedding.
fn arg(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

// ---- identity rows: key derivation plus the identity regex ----

/// Shell oracle for one file name: the exact `key=${...}` derivation
/// plus the `=~` test, rendered `key\tident` or `key\tINVALID`.
fn identity_snippet() -> String {
    format!(
        "{COORDINATOR_SOURCES}\
         name=$2; key=${{name##*/}}; key=${{key%.sh}}\n\
         if [[ $key =~ ^([0-9]+[-_])?([a-z][a-z0-9-]*)$ ]]; then \
         printf 'key=%s\\tident=%s\\n' \"$key\" \"${{BASH_REMATCH[2]}}\"; \
         else printf 'key=%s\\tINVALID\\n' \"$key\"; fi\n"
    )
}

/// Rust rendering of the same decision for comparison.
fn identity_render(name: &[u8]) -> Vec<u8> {
    let key = extension_key(name);
    let mut out = b"key=".to_vec();
    out.extend_from_slice(key);
    out.extend_from_slice(b"\t");
    match extension_identity(key) {
        Some(identity) => {
            out.extend_from_slice(b"ident=");
            out.extend_from_slice(identity);
        }
        None => out.extend_from_slice(b"INVALID"),
    }
    out.push(b'\n');
    out
}

#[test]
fn identity_rows_agree() {
    // (file name bytes,): the matrix covers bare and numerically
    // prefixed identities, every rejection shape, suffix corners,
    // and non-UTF8 names.
    let rows: &[&[u8]] = &[
        b"foo.sh",
        b"10-foo.sh",
        b"10_foo.sh",
        b"007-x-9.sh",
        b"0-a.sh",
        b"9_bar.sh",
        b"z-.sh",
        b"1a.sh",
        b"12-_abc.sh",
        b"9-.sh",
        b"a.sh.sh",
        b"A.sh",
        b"-a.sh",
        b"a b.sh",
        b"1-2-x.sh",
        b"12_3a.sh",
        b".sh",
        b"\xff.sh",
        b"ab\xffc.sh",
    ];
    for name in rows {
        let dir = TempDir::new("doctor-ident").expect("fixture dir");
        let (code, out, err) = shell_run(dir.path(), &[arg(name)], &identity_snippet());
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(err.is_empty(), "stderr for {name:?}: {err:?}");
        assert_eq!(out, identity_render(name), "identity for {name:?}");
    }
}

// ---- discovery rows: `_dot_doctor_extension_specs` over doctor.d ----

/// Build a `doctor.d` fixture from `(name bytes, kind)` entries:
/// `f` regular file, `d` directory, `l` dangling symlink.
fn build_doctor_d(tag: &str, entries: &[(&[u8], u8)]) -> (TempDir, PathBuf) {
    let dir = TempDir::new(tag).expect("fixture dir");
    let doctor_d = dir.path().join("doctor.d");
    std::fs::create_dir_all(&doctor_d).expect("doctor.d");
    for (name, kind) in entries {
        let path = Path::new(OsStr::from_bytes(name));
        let full = doctor_d.join(path);
        match kind {
            b'f' => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("fixture parents");
                }
                std::fs::write(&full, b"#!/bin/sh\n").expect("write fixture");
            }
            b'd' => std::fs::create_dir_all(&full).expect("fixture dir entry"),
            b'l' => std::os::unix::fs::symlink("nowhere", &full).expect("fixture symlink"),
            _ => unreachable!("unknown entry kind"),
        }
    }
    (dir, doctor_d)
}

/// Shell oracle: stubbed validators, real discovery with specs
/// redirected to a file (so spec bytes never mix with the harness
/// trailer), then the exit-status trailer on stdout.
fn specs_snippet() -> String {
    format!(
        "{COORDINATOR_SOURCES}{TRUST_STUBS}\
         export DOT_EXTENSIONS_DIR=\"$2\"\n\
         _dot_doctor_extension_specs >\"$3\"\n\
         rc=$?\n\
         printf 'rc=%d\\n' \"$rc\"\n"
    )
}

/// Render a [`Discovery`] the way the shell pipeline prints it:
/// one `key\tscript\n` row per spec, plus the stderr bytes.
fn discovery_render(discovery: &Discovery) -> (Vec<u8>, Vec<u8>) {
    let mut stdout = Vec::new();
    for spec in &discovery.specs {
        stdout.extend_from_slice(&spec.key);
        stdout.push(b'\t');
        stdout.extend_from_slice(spec.script.as_os_str().as_encoded_bytes());
        stdout.push(b'\n');
    }
    let stderr = discovery
        .error
        .as_ref()
        .map(|error| error.message())
        .unwrap_or_default();
    (stdout, stderr)
}

/// Run one discovery row and compare the spec file bytes, stderr,
/// and the (always zero, pipe-swallowed) status.
fn check_specs_row(tag: &str, entries: &[(&[u8], u8)]) {
    let (dir, doctor_d) = build_doctor_d(tag, entries);
    let out_path = dir.path().join("out.txt");
    std::fs::write(&out_path, b"").expect("out file");
    // The shell appends `/doctor.d` itself (`root=$DOT_EXTENSIONS_DIR/doctor.d`),
    // so the extension root (not the `doctor.d` dir) travels as `$2`.
    let doctor_arg = dir.path().as_os_str().as_encoded_bytes().to_vec();
    let out_arg = out_path.as_os_str().as_encoded_bytes().to_vec();
    let (code, out, err) = shell_run(
        dir.path(),
        &[arg(&doctor_arg), arg(&out_arg)],
        &specs_snippet(),
    );
    assert_eq!(code, 0, "harness exit for {tag}");
    assert_eq!(out, b"rc=0\n", "swallowed status for {tag}");
    let discovery = collect_specs(&doctor_d).expect("collect specs");
    let (want_out, want_err) = discovery_render(&discovery);
    assert_eq!(
        std::fs::read(&out_path).expect("read spec file"),
        want_out,
        "specs for {tag}"
    );
    assert_eq!(err, want_err, "stderr for {tag}");
}

#[test]
fn specs_rows_agree() {
    // Basic byte ordering (digits before lowercase, `-` before the
    // longer `x-y` key, short keys before their extensions). A bare
    // underscore is not an identity character (only the numeric
    // prefix separator), so `x_y.sh` would truncate here — it gets
    // its own invalid-identity row below.
    check_specs_row(
        "specs-basic",
        &[
            (b"20-foo.sh", b'f'),
            (b"05-bar.sh", b'f'),
            (b"plain.sh", b'f'),
            (b"aardvark.sh", b'f'),
            (b"x.sh", b'f'),
            (b"x-y.sh", b'f'),
            (b"10_zed.sh", b'f'),
        ],
    );
    // Byte sort, not numeric: `10-` precedes `9-`.
    check_specs_row("specs-bytes", &[(b"9-bar.sh", b'f'), (b"10-foo.sh", b'f')]);
    // Glob-shaped filtering: dotfiles and non-`.sh` names vanish,
    // while a directory or dangling symlink named `*.sh` still
    // lists (the shell only probes `-e`/`-L`).
    check_specs_row(
        "specs-filter",
        &[
            (b"ok.sh", b'f'),
            (b".hidden.sh", b'f'),
            (b"notes.txt", b'f'),
            (b"noext", b'f'),
            (b"weird.sh", b'd'),
            (b"dangling.sh", b'l'),
        ],
    );
    // Dangling symlink after a valid entry, proving `-L` inclusion.
    check_specs_row(
        "specs-dangling",
        &[(b"aa.sh", b'f'), (b"dangling.sh", b'l')],
    );
    // Empty directory: empty listing, silent success.
    check_specs_row("specs-empty", &[]);
    // Invalid identity truncates the listing but still exits 0
    // through the `sort` pipe.
    check_specs_row(
        "specs-invalid",
        &[
            (b"05-bar.sh", b'f'),
            (b"20-foo.sh", b'f'),
            (b"Bad Name.sh", b'f'),
        ],
    );
    // Duplicate identity across separator spellings truncates too.
    check_specs_row(
        "specs-dup",
        &[
            (b"05-bar.sh", b'f'),
            (b"20-foo.sh", b'f'),
            (b"foo.sh", b'f'),
        ],
    );
    check_specs_row(
        "specs-dup-sep",
        &[(b"10-foo.sh", b'f'), (b"10_foo.sh", b'f')],
    );
    // Non-UTF8 invalid name: the stderr message carries raw bytes.
    check_specs_row("specs-nonutf8", &[(b"aa.sh", b'f'), (b"\xff.sh", b'f')]);
}

// ---- dispatch rows: `_dot_doctor_render_records` ----

/// Shell oracle: stubbed renderers plus the real dispatch over the
/// record file at `$2`, with an `rc=` trailer.
fn dispatch_snippet() -> String {
    format!(
        "{COORDINATOR_SOURCES}{RENDER_STUBS}\
         _dot_doctor_render_records \"$2\"\n\
         rc=$?\n\
         printf 'rc=%d\\n' \"$rc\"\n"
    )
}

/// `read`-shaped line split: one trailing newline is the terminator,
/// anything else (including a missing final newline) is content.
fn split_records(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

/// Rust rendering of the dispatch decision per row content.
fn dispatch_render(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in split_records(content) {
        let mut fields = line.splitn(3, |byte| *byte == b'\t');
        let kind = fields.next().unwrap_or(b"");
        let message = fields.next().unwrap_or(b"");
        let detail = fields.next().unwrap_or(b"");
        match record_kind(kind) {
            RecordKind::Section => {
                out.extend_from_slice(b"RENDER|section|");
                out.extend_from_slice(message);
                out.extend_from_slice(b"|\n");
            }
            RecordKind::Ok => render_verdict(&mut out, b"ok", message, detail),
            RecordKind::Warn => render_verdict(&mut out, b"warn", message, detail),
            RecordKind::Fail => render_verdict(&mut out, b"fail", message, detail),
            RecordKind::Skip => render_verdict(&mut out, b"skip", message, detail),
            RecordKind::Unknown => {
                out.extend_from_slice(b"RENDER|fail|doctor extension emitted an invalid result|");
                out.extend_from_slice(kind);
                out.extend_from_slice(b"\n");
            }
        }
    }
    out
}

/// Shared `RENDER|kind|message|detail` line.
fn render_verdict(out: &mut Vec<u8>, kind: &[u8], message: &[u8], detail: &[u8]) {
    out.extend_from_slice(b"RENDER|");
    out.extend_from_slice(kind);
    out.extend_from_slice(b"|");
    out.extend_from_slice(message);
    out.extend_from_slice(b"|");
    out.extend_from_slice(detail);
    out.extend_from_slice(b"\n");
}

/// Run one dispatch row and compare the stub transcript.
fn check_dispatch_row(tag: &str, content: &[u8]) {
    let dir = TempDir::new("doctor-dispatch").expect("fixture dir");
    let record = dir.path().join("results");
    std::fs::write(&record, content).expect("record file");
    let record_arg = record.as_os_str().as_encoded_bytes().to_vec();
    let (code, out, err) = shell_run(dir.path(), &[arg(&record_arg)], &dispatch_snippet());
    assert_eq!(code, 0, "harness exit for {tag}");
    assert!(err.is_empty(), "stderr for {tag}: {err:?}");
    let mut want = dispatch_render(content);
    want.extend_from_slice(b"rc=0\n");
    assert_eq!(out, want, "dispatch for {tag}");
}

#[test]
fn dispatch_rows_agree() {
    check_dispatch_row(
        "dispatch-kinds",
        b"section\ts\td\textra\nok\tmsg\tdet\nwarn\tm\td\nfail\tm\td\nskip\tm\td\n",
    );
    check_dispatch_row("dispatch-empty-detail", b"ok\tm\t\nwarn\tm\t\n");
    check_dispatch_row("dispatch-unknown", b"bogus\tm\td\n");
    check_dispatch_row("dispatch-empty-kind", b"\t\t\n");
    check_dispatch_row("dispatch-no-trailing-nl", b"ok\tm\td");
    check_dispatch_row("dispatch-blank-middle", b"ok\ta\tb\n\nskip\tc\td\n");
    check_dispatch_row("dispatch-extra-tabs", b"ok\tm\td1\td2\n");
    check_dispatch_row("dispatch-case", b"OK\tm\td\nSection\tm\td\n");
    check_dispatch_row("dispatch-empty", b"");
    // IFS is tab-only: a leading space is data, not trimming.
    check_dispatch_row("dispatch-space-kind", b" \tm\td\n");
}

// ---- source rows: `dot_doctor_source` relative validation ----

/// Shell oracle: stubbed trust check, real sourcing of `$2` under
/// `$3`, with an `rc=` trailer. Positional params are copied first:
/// the function itself runs `set --`.
fn source_snippet() -> String {
    format!(
        "{API_SOURCES}{TRUST_STUBS}\
         rel=$2; ext=$3; export DOT_EXTENSIONS_DIR=\"$ext\"\n\
         dot_doctor_source \"$rel\"\n\
         rc=$?\n\
         printf 'rc=%d\\n' \"$rc\"\n"
    )
}

/// Run one source row: valid shapes source the fixture (expecting
/// its `SOURCED` marker and rc 0, or rc 1 when the validator passes
/// but no file exists); invalid shapes expect rc 2 and silence.
fn check_source_row(tag: &str, relative: &[u8], fixture: bool, want_rc: i32) {
    let dir = TempDir::new("doctor-source").expect("fixture dir");
    let ext = dir.path().join("ext");
    if fixture {
        let target = Path::new(OsStr::from_bytes(relative));
        let full = ext.join(target);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("fixture parents");
        }
        std::fs::write(&full, b"printf 'SOURCED\\n'\n").expect("write fixture");
    }
    let rel_arg = relative.to_vec();
    let ext_arg = ext.as_os_str().as_encoded_bytes().to_vec();
    let (code, out, err) = shell_run(
        dir.path(),
        &[arg(&rel_arg), arg(&ext_arg)],
        &source_snippet(),
    );
    assert_eq!(code, 0, "harness exit for {tag}");
    let valid = source_relative_valid(relative);
    if want_rc == 0 {
        assert!(valid, "Rust must accept {relative:?}");
        assert!(err.is_empty(), "stderr for {tag}: {err:?}");
        assert_eq!(out, b"SOURCED\nrc=0\n", "source for {tag}");
    } else if want_rc == 1 {
        // Validator stub passes but no file exists: the shell's
        // sourcing step (not the shape check) fails. Rust still
        // reports the shape valid.
        assert!(valid, "Rust must accept the shape of {relative:?}");
        assert_eq!(out, b"rc=1\n", "missing-file rc for {tag}");
    } else {
        assert!(!valid, "Rust must reject {relative:?}");
        assert!(err.is_empty(), "stderr for {tag}: {err:?}");
        assert_eq!(out, b"rc=2\n", "rejection rc for {tag}");
    }
}

#[test]
fn source_rows_agree() {
    // Valid shapes with fixtures behind them.
    for (tag, relative) in [
        ("source-plain", &b"foo"[..]),
        ("source-nested", &b"a/b"[..]),
        ("source-dots", &b"..."[..]),
        ("source-dotfile", &b".foo"[..]),
        ("source-dotdot-prefix", &b"..foo"[..]),
        ("source-trailing-dot", &b"foo."[..]),
        ("source-triple-mid", &b"a/.../b"[..]),
        ("source-space", &b"a b"[..]),
        ("source-tab", &b"a\tb"[..]),
        ("source-ext", &b"a.sh"[..]),
        ("source-nonutf8", &b"a\xffb"[..]),
    ] {
        check_source_row(tag, relative, true, 0);
    }
    // Rejected shapes (no fixture: rejection precedes any I/O).
    for (tag, relative) in [
        ("source-empty", &b""[..]),
        ("source-absolute", &b"/a"[..]),
        ("source-dot", &b"."[..]),
        ("source-dotdot", &b".."[..]),
        ("source-dot-slash", &b"./a"[..]),
        ("source-dotdot-slash", &b"../a"[..]),
        ("source-slash-dot-slash", &b"a/./b"[..]),
        ("source-slash-dotdot-slash", &b"a/../b"[..]),
        ("source-trail-dot", &b"a/."[..]),
        ("source-trail-dotdot", &b"a/.."[..]),
        ("source-trail-slash", &b"a/"[..]),
        ("source-double-slash", &b"a//b"[..]),
        ("source-newline", &b"a\nb"[..]),
        ("source-cr", &b"a\rb"[..]),
        ("source-root", &b"/"[..]),
        ("source-double-root", &b"//"[..]),
    ] {
        check_source_row(tag, relative, false, 2);
    }
    // Valid shape, validator stubbed to pass, but nothing to source.
    check_source_row("source-missing", b"ghost", false, 1);
}

// ---- summary rows: line, color, and exit contract ----

#[test]
fn summary_rows_agree() {
    // (pass, warn, fail, extension status, want color)
    let rows: &[(u64, u64, u64, i32, &str)] = &[
        (0, 0, 0, 0, "green"),
        (2, 0, 0, 0, "green"),
        (3, 1, 0, 0, "yellow"),
        (0, 4, 0, 7, "yellow"),
        (3, 1, 2, 0, "red"),
        (0, 0, 5, 0, "red"),
        (0, 0, 0, 1, "green"),
    ];
    for (pass, warn, fail, status, color) in rows {
        let dir = TempDir::new("doctor-summary").expect("fixture dir");
        let snippet = format!(
            "summary=$(printf '%d passed · %d warnings · %d failed' {pass} {warn} {fail})\n\
             if [[ {fail} -gt 0 ]]; then color=red; \
             elif [[ {warn} -gt 0 ]]; then color=yellow; else color=green; fi\n\
             printf '%s\\ncolor=%s\\n' \"$summary\" \"$color\"\n\
             [[ {fail} -eq 0 && {status} -eq 0 ]]; printf 'rc=%d\\n' \"$?\"\n"
        );
        let (code, out, err) = shell_run(dir.path(), &[], &snippet);
        assert_eq!(code, 0, "harness exit for {pass}/{warn}/{fail}/{status}");
        assert!(err.is_empty(), "stderr: {err:?}");
        let want_color = summary_color(*fail, *warn);
        assert_eq!(want_color.name(), *color, "color name");
        let want_rc = i32::from(!overall_ok(*fail, *status));
        let want = format!(
            "{}\ncolor={}\nrc={}\n",
            summary_line(*pass, *warn, *fail),
            color,
            want_rc
        );
        assert_eq!(
            out,
            want.as_bytes(),
            "summary for {pass}/{warn}/{fail}/{status}"
        );
    }
}
