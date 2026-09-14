//! Differential parity tests for the replacement record layer of
//! `lib/dot/repos/overlays.sh`: the record path derivation, the
//! legacy-format hash, legacy record matching, generation
//! matching, transaction safety, record reading, and cleanup.
//!
//! Every case runs the live shell function and its Rust twin on
//! identical fixtures and compares exit status and selection.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the replacement runtime sourced
/// (overlays.sh pulls in temp.sh for the Git boundary itself).
fn shell_run(home: &Path, argv: &[&std::ffi::OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(
            ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/reserved.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n{snippet}"
        ));
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

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// chmod a fixture to an exact mode.
fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// `git hash-object --stdin` for setup only (an independent oracle
/// for the record-name fixtures, not the implementation).
fn hash_stdin(value: &str) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(value.as_bytes())
        .expect("feed git");
    let output = child.wait_with_output().expect("wait git");
    assert!(output.status.success(), "setup hash");
    String::from_utf8(output.stdout)
        .expect("hash")
        .trim()
        .to_string()
}

#[test]
fn replacement_record_path_agrees() {
    let dir = TempDir::new("ovrepl-path").expect("fixture dir");
    let home = dir.path();
    for destination in [
        home.join("app.conf").to_string_lossy().into_owned(),
        home.join("deep/nested.conf").to_string_lossy().into_owned(),
    ] {
        let manifest = home.join("manifest.tsv").to_string_lossy().into_owned();
        let snippet = format!(
            "DOT_OVERLAY_MANIFEST={} _overlay_replacement_record_path {}; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n",
            sq(&manifest),
            sq(&destination),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {destination:?}");
        assert!(serr.is_empty(), "record path stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("record path dump");
        let rust = match repos_overlays::replacement_record_path(&destination, &manifest, home) {
            Some(path) => format!("rc=0\nreply={path}\n"),
            None => String::from("rc=1\nreply=\n"),
        };
        assert_eq!(rust, shell, "record path for {destination:?}");
    }
}

#[test]
fn replacement_hash_object_format_agrees() {
    let dir = TempDir::new("ovrepl-format").expect("fixture dir");
    let home = dir.path();
    for (format, value) in [
        ("sha1", "alpha"),
        ("sha256", "alpha"),
        ("sha1", ""),
        ("bogus", "alpha"),
        ("", "alpha"),
    ] {
        let snippet = format!(
            "out=$(_overlay_replacement_hash_object_format {} {} 2>/dev/null); code=$?; printf 'rc=%s\\nhash=%s\\n' \"$code\" \"$out\"\n",
            sq(format),
            sq(value),
        );
        let (code, out, _serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {format:?}");
        let shell = String::from_utf8(out).expect("format dump");
        let rust = match repos_overlays::replacement_hash_object_format(format, value, home) {
            Some(hash) => format!("rc=0\nhash={hash}\n"),
            None => String::from("rc=1\nhash=\n"),
        };
        assert_eq!(rust, shell, "hash format for {format:?}");
        // The sha1 spelling must equal the plain hash oracle.
        if format == "sha1" {
            assert_eq!(
                repos_overlays::replacement_hash_object_format(format, value, home),
                Some(hash_stdin(value)),
                "sha1 equals oracle"
            );
        }
    }
}

/// Legacy record fixture: the record name carries the alternate
/// (sha256) hash of the destination while the current hash is
/// sha1, which is exactly the length-mismatch legacy shape.
fn legacy_record(manifest: &str, destination: &str, home: &Path) -> (String, String) {
    let current = hash_stdin(destination);
    assert_eq!(current.len(), 40, "current format is sha1");
    let alternate = repos_overlays::replacement_hash_object_format("sha256", destination, home)
        .expect("alternate hash");
    (format!("{manifest}.replace.{alternate}"), current)
}

#[test]
fn replacement_legacy_record_path_matches_agrees() {
    let dir = TempDir::new("ovrepl-legacy").expect("fixture dir");
    let home = dir.path();
    let manifest = home.join("manifest.tsv").to_string_lossy().into_owned();
    let destination = home.join("app.conf").to_string_lossy().into_owned();
    let (legacy, current) = legacy_record(&manifest, &destination, home);
    let current_named = format!("{manifest}.replace.{current}");
    let cases: &[(&str, String, String)] = &[
        ("legacy-sha256", legacy.clone(), destination.clone()),
        ("current-name", current_named, destination.clone()),
        (
            "bad-prefix",
            format!("{manifest}.other.{current}"),
            destination.clone(),
        ),
        (
            "bad-suffix",
            format!("{manifest}.replace.{}", "g".repeat(64)),
            destination.clone(),
        ),
        (
            "wrong-target",
            legacy.clone(),
            home.join("other.conf").to_string_lossy().into_owned(),
        ),
    ];
    for (name, record, destination) in cases {
        let snippet = format!(
            "DOT_OVERLAY_MANIFEST={} _overlay_replacement_legacy_record_path_matches {} {} {}; printf 'rc=%s\\n' \"$?\"\n",
            sq(&manifest),
            sq(record),
            sq(destination),
            sq(&current),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "legacy stderr for {name:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("legacy dump");
        let rust_code = if repos_overlays::replacement_legacy_record_path_matches(
            record,
            destination,
            &current,
            &manifest,
            home,
        ) {
            0
        } else {
            1
        };
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "legacy match for {name:?}"
        );
    }
}

#[test]
fn replacement_generation_matches_agrees() {
    let dir = TempDir::new("ovrepl-generation").expect("fixture dir");
    let home = dir.path();
    let file = stage(home, "app.conf", b"body\n");
    let link = home.join("link.conf");
    std::os::unix::fs::symlink("app.conf", &link).expect("link");
    let content_file = repos_overlays::replacement_identity(home, &file).expect("file identity");
    let content_link = repos_overlays::replacement_identity(home, &link).expect("link identity");
    let legacy_id =
        dot::temp::identity_string(dot::temp::path_identity(&file).expect("path identity"));
    // No-follow leaf identity: the link answers its own dev:ino.
    use std::os::unix::fs::MetadataExt as _;
    let link_meta = std::fs::symlink_metadata(&link).expect("link meta");
    let link_legacy_id = format!("{}:{}", link_meta.dev(), link_meta.ino());
    for (name, path, expected, kind) in [
        (
            "file-content",
            file.clone(),
            content_file.clone(),
            "content",
        ),
        (
            "link-content",
            link.clone(),
            content_link.clone(),
            "content",
        ),
        ("file-legacy", file.clone(), legacy_id.clone(), "legacy"),
        // Plain `stat` takes no `-L` here: the link answers its
        // own pair, not its target's.
        (
            "link-legacy",
            link.clone(),
            link_legacy_id.clone(),
            "legacy",
        ),
        ("mismatch", file.clone(), content_link.clone(), "content"),
        ("bogus-kind", file.clone(), content_file.clone(), "bogus"),
        ("empty-kind", file.clone(), content_file.clone(), ""),
        (
            "missing",
            home.join("absent.conf"),
            content_file.clone(),
            "content",
        ),
    ] {
        let snippet = format!(
            "if _overlay_replacement_generation_matches {} {} {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
            sq(&path.to_string_lossy()),
            sq(&expected),
            sq(kind),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "generation stderr for {name:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("generation dump");
        let rust_code =
            if repos_overlays::replacement_generation_matches(&path, &expected, kind, home) {
                0
            } else {
                1
            };
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "generation for {name:?}"
        );
    }
}

#[test]
fn replacement_transaction_safe_agrees() {
    let dir = TempDir::new("ovrepl-transaction").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    // (name, setup): each case stages one transaction directory.
    let cases = [
        "empty",
        "next-only",
        "previous-only",
        "both",
        "extra-file",
        "extra-hidden",
        "open-mode",
        "as-file",
        "as-link",
        "missing",
    ];
    for name in cases {
        let root = home.join(name);
        std::fs::create_dir_all(&root).expect("case dir");
        let transaction = root.join("txn");
        match name {
            "empty" | "next-only" | "previous-only" | "both" | "extra-file" | "extra-hidden" => {
                std::fs::create_dir_all(&transaction).expect("txn dir");
                chmod(&transaction, 0o700);
                if name == "next-only" || name == "both" {
                    std::os::unix::fs::symlink("target", transaction.join("next")).expect("next");
                }
                if name == "previous-only" || name == "both" {
                    std::os::unix::fs::symlink("target", transaction.join("previous"))
                        .expect("previous");
                }
                if name == "extra-file" {
                    stage(&transaction, "stray", b"x\n");
                }
                if name == "extra-hidden" {
                    stage(&transaction, ".hidden", b"x\n");
                }
            }
            "open-mode" => {
                std::fs::create_dir_all(&transaction).expect("txn dir");
                chmod(&transaction, 0o755);
            }
            "as-file" => {
                stage(&root, "txn", b"x\n");
            }
            "as-link" => {
                std::os::unix::fs::symlink("elsewhere", &transaction).expect("link");
            }
            _ => {}
        }
        let snippet = format!(
            "if _overlay_replacement_transaction_safe {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
            sq(&transaction.to_string_lossy()),
        );
        let (code, out, serr) = shell_run(&root, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "transaction stderr for {name:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("transaction dump");
        let rust_code = if repos_overlays::replacement_transaction_safe(&transaction, euid) {
            0
        } else {
            1
        };
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "transaction for {name:?}"
        );
    }
}

/// One replacement fixture: a physical file plus its transaction
/// directory, with content-kind and legacy-kind identities.
struct RecordFixture {
    destination: String,
    physical: PathBuf,
    transaction: PathBuf,
    content_expected: String,
    legacy_expected: String,
    parent_identity: String,
}

fn record_fixture(root: &Path, rel: &str) -> RecordFixture {
    let destination = root.join(rel).to_string_lossy().into_owned();
    let physical = stage(root, "physical/app.conf", b"body\n");
    let parent = physical.parent().expect("parent").to_path_buf();
    let transaction = parent.join(".app.conf.dot-overlay-replace-v1");
    std::fs::create_dir_all(&transaction).expect("txn dir");
    let content_expected =
        repos_overlays::replacement_identity(root, &physical).expect("content identity");
    let legacy_expected =
        dot::temp::identity_string(dot::temp::path_identity(&physical).expect("identity"));
    let parent_identity =
        dot::temp::identity_string(dot::temp::path_identity(&parent).expect("parent identity"));
    RecordFixture {
        destination,
        physical,
        transaction,
        content_expected,
        legacy_expected,
        parent_identity,
    }
}

/// The six-field record line.
fn record_line(fixture: &RecordFixture, target: &str, expected: &str) -> Vec<u8> {
    format!(
        "{}\t{}\t{target}\t{expected}\t{}\t{}\n",
        fixture.destination,
        fixture.physical.to_string_lossy(),
        fixture.transaction.to_string_lossy(),
        fixture.parent_identity,
    )
    .into_bytes()
}

/// Dump a read: rc plus the seven record fields (empty on failure).
fn dump_read(record: Option<repos_overlays::ReplaceRecord>) -> String {
    match record {
        Some(fields) => format!(
            "rc=0\ndestination={}\nphysical={}\ntarget={}\nexpected={}\nkind={}\ntransaction={}\nparent={}\n",
            fields.destination,
            fields.physical,
            fields.target,
            fields.expected,
            fields.identity_kind.as_str(),
            fields.transaction,
            fields.parent_identity,
        ),
        None => String::from("rc=1\n"),
    }
}

#[test]
fn replacement_read_agrees() {
    let dir = TempDir::new("ovrepl-read").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    for (name, body, rename) in [
        ("content", "content", ""),
        ("legacy", "legacy", ""),
        ("wrong-name", "content", "renamed"),
        ("two-line", "two-line", ""),
        ("relative-dest", "relative-dest", ""),
        ("open-record", "content", ""),
        ("transaction-mismatch", "transaction-mismatch", ""),
        ("garbage-expected", "garbage-expected", ""),
        ("extra-field", "extra-field", ""),
    ] {
        let root = home.join(name);
        std::fs::create_dir_all(&root).expect("case dir");
        let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
        let fixture = record_fixture(&root, "app.conf");
        let target = ".dotfiles-web/home/app.conf";
        let record_name = if rename.is_empty() {
            match body {
                "legacy" => legacy_record(&manifest, &fixture.destination, &root).0,
                _ => format!("{manifest}.replace.{}", hash_stdin(&fixture.destination)),
            }
        } else {
            root.join(rename).to_string_lossy().into_owned()
        };
        let expected = match body {
            "legacy" => fixture.legacy_expected.clone(),
            "garbage-expected" => "zzz".to_string(),
            _ => fixture.content_expected.clone(),
        };
        let mut line = record_line(&fixture, target, &expected);
        match body {
            "two-line" => {
                line.extend_from_slice(b"second\tline\there\n");
            }
            "relative-dest" => {
                line = record_line(
                    &RecordFixture {
                        destination: "relative/app.conf".to_string(),
                        ..fixture
                    },
                    target,
                    &expected,
                );
            }
            "transaction-mismatch" => {
                line = format!(
                    "{}\t{}\t{target}\t{expected}\t{}\t{}\n",
                    fixture.destination,
                    fixture.physical.to_string_lossy(),
                    root.join("elsewhere").to_string_lossy(),
                    fixture.parent_identity,
                )
                .into_bytes();
            }
            "extra-field" => {
                line.pop();
                line.extend_from_slice(b"\textra\n");
            }
            _ => {}
        }
        let record = PathBuf::from(&record_name);
        if let Some(parent) = record.parent() {
            std::fs::create_dir_all(parent).expect("record parent");
        }
        std::fs::write(&record, &line).expect("record");
        chmod(&record, if name == "open-record" { 0o644 } else { 0o600 });
        let snippet = format!(
            "DOT_OVERLAY_MANIFEST={} _overlay_replacement_read {}; code=$?; printf 'rc=%s\\n' \"$code\"; if [[ $code -eq 0 ]]; then printf 'destination=%s\\nphysical=%s\\ntarget=%s\\nexpected=%s\\nkind=%s\\ntransaction=%s\\nparent=%s\\n' \"$OVERLAY_REPLACE_DESTINATION\" \"$OVERLAY_REPLACE_PHYSICAL\" \"$OVERLAY_REPLACE_TARGET\" \"$OVERLAY_REPLACE_EXPECTED\" \"$OVERLAY_REPLACE_IDENTITY_KIND\" \"$OVERLAY_REPLACE_TRANSACTION\" \"$OVERLAY_REPLACE_PARENT_IDENTITY\"; fi\n",
            sq(&manifest),
            sq(&record_name),
        );
        let (code, out, serr) = shell_run(&root, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "read stderr for {name:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("read dump");
        let rust = dump_read(repos_overlays::replacement_read(
            &record, &manifest, euid, &root, &root,
        ));
        assert_eq!(rust, shell, "replacement read for {name:?}");
    }
}

#[test]
fn replacement_cleanup_agrees() {
    let dir = TempDir::new("ovrepl-cleanup").expect("fixture dir");
    let home = dir.path();
    // (name, stage next, stage previous, next target): `next` is a
    // symlink except in the file case; `previous` is a file.
    for (name, next, previous, next_target) in [
        ("empty", false, false, "wanted"),
        ("next-match", true, false, "wanted"),
        ("next-mismatch", true, false, "other"),
        ("previous-present", false, true, "wanted"),
        ("next-file", false, false, "wanted"),
    ] {
        let root = home.join(name);
        std::fs::create_dir_all(&root).expect("case dir");
        let transaction = root.join("txn");
        std::fs::create_dir_all(&transaction).expect("txn dir");
        if next {
            std::os::unix::fs::symlink(next_target, transaction.join("next")).expect("next");
        }
        if name == "next-file" {
            stage(&transaction, "next", b"x\n");
        }
        if previous {
            stage(&transaction, "previous", b"x\n");
        }
        let record = stage(&root, "record", b"line\n");
        let snippet = format!(
            "_overlay_replacement_cleanup {} {} wanted; code=$?; printf 'rc=%s\\n' \"$code\"; printf 'record='; cat {} 2>/dev/null || true; printf 'txn='; ls -A {} 2>/dev/null || echo MISSING\n",
            sq(&record.to_string_lossy()),
            sq(&transaction.to_string_lossy()),
            sq(&record.to_string_lossy()),
            sq(&transaction.to_string_lossy()),
        );
        let (code, out, serr) = shell_run(&root, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "cleanup stderr for {name:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("cleanup dump");
        std::fs::write(home.join(format!("{name}.shell.out")), shell).expect("stash");
        // The Rust twin runs on a mirrored layout (same names, fresh
        // root) so mutations never collide with the shell side.
        // (Shell and Rust roots differ, but cleanup dumps carry no
        // paths, so no scrubbing is needed.)
        let rust_root = home.join(format!("{name}-rust"));
        std::fs::create_dir_all(&rust_root).expect("rust dir");
        let rust_transaction = rust_root.join("txn");
        std::fs::create_dir_all(&rust_transaction).expect("rust txn");
        if next {
            std::os::unix::fs::symlink(next_target, rust_transaction.join("next")).expect("next");
        }
        if name == "next-file" {
            stage(&rust_transaction, "next", b"x\n");
        }
        if previous {
            stage(&rust_transaction, "previous", b"x\n");
        }
        let rust_record = stage(&rust_root, "record", b"line\n");
        let ok = repos_overlays::replacement_cleanup(&rust_record, &rust_transaction, "wanted");
        let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
        rust.push_str("record=");
        rust.push_str(&std::fs::read_to_string(&rust_record).unwrap_or_default());
        // `ls -A` lists byte-sorted; mirror it for the twin.
        rust.push_str("txn=");
        if rust_transaction.symlink_metadata().is_err() {
            rust.push_str("MISSING\n");
        } else {
            let mut entries: Vec<String> = std::fs::read_dir(&rust_transaction)
                .expect("scan txn")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            entries.sort();
            for entry in entries {
                rust.push_str(&entry);
                rust.push('\n');
            }
        }
        let shell = std::fs::read_to_string(home.join(format!("{name}.shell.out"))).expect("stash");
        assert_eq!(rust, shell, "replacement cleanup for {name:?}");
    }
}
