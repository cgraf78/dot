//! Native contracts for durable overlay replacement records.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays::{self, ReplaceIdentityKind};
use dot_test_support::TempDir;

fn stage(root: &Path, relative: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn hash(value: &str) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "hash-object",
            "--stdin",
        ])
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", "/tmp")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn replacement_record_path_binds_the_absolute_destination() {
    let scope = TempDir::new("replacement-path").unwrap();
    let manifest = scope.path().join("manifest.tsv");
    for relative in ["app.conf", "deep/nested.conf"] {
        let destination = scope.path().join(relative).to_string_lossy().into_owned();
        assert_eq!(
            repos_overlays::replacement_record_path(
                &destination,
                &manifest.to_string_lossy(),
                scope.path()
            ),
            Some(format!(
                "{}.replace.{}",
                manifest.display(),
                hash(&destination)
            ))
        );
    }
}

#[test]
fn replacement_hash_object_format_has_literal_git_blob_values() {
    let scope = TempDir::new("replacement-format").unwrap();
    for (format, value, expected) in [
        (
            "sha1",
            "alpha",
            Some("7e74e68b2a782a3aead46d987a63ca1c91091c13"),
        ),
        (
            "sha256",
            "alpha",
            Some("a127e6ce46f35284822de1324a3ed0d3430cb75e4417061c719749a26d59a364"),
        ),
        ("sha1", "", Some("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")),
        (
            "sha256",
            "",
            Some("473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"),
        ),
        ("bogus", "alpha", None),
        ("", "alpha", None),
    ] {
        assert_eq!(
            repos_overlays::replacement_hash_object_format(format, value, scope.path()).as_deref(),
            expected,
            "{format:?} {value:?}"
        );
    }
}

#[test]
fn legacy_record_path_accepts_only_the_alternate_object_format_for_the_destination() {
    let scope = TempDir::new("replacement-legacy").unwrap();
    let root = scope.path();
    let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
    let destination = root.join("app.conf").to_string_lossy().into_owned();
    let current = hash(&destination);
    let alternate =
        repos_overlays::replacement_hash_object_format("sha256", &destination, root).unwrap();
    let legacy = format!("{manifest}.replace.{alternate}");
    for (name, record, candidate, expected) in [
        ("legacy-sha256", legacy.clone(), destination.clone(), true),
        (
            "current-name",
            format!("{manifest}.replace.{current}"),
            destination.clone(),
            false,
        ),
        (
            "bad-prefix",
            format!("{manifest}.other.{alternate}"),
            destination.clone(),
            false,
        ),
        (
            "bad-suffix",
            format!("{manifest}.replace.{}", "g".repeat(64)),
            destination.clone(),
            false,
        ),
        (
            "wrong-target",
            legacy,
            root.join("other.conf").to_string_lossy().into_owned(),
            false,
        ),
    ] {
        assert_eq!(
            repos_overlays::replacement_legacy_record_path_matches(
                &record, &candidate, &current, &manifest, root
            ),
            expected,
            "{name}"
        );
    }
}

#[test]
fn replacement_generation_matches_content_and_nofollow_legacy_identity() {
    let scope = TempDir::new("replacement-generation").unwrap();
    let root = scope.path();
    let file = stage(root, "app.conf", b"body\n", 0o600);
    let link = root.join("link.conf");
    std::os::unix::fs::symlink("app.conf", &link).unwrap();
    let file_content = repos_overlays::replacement_identity(root, &file).unwrap();
    let link_content = repos_overlays::replacement_identity(root, &link).unwrap();
    let file_meta = std::fs::symlink_metadata(&file).unwrap();
    let link_meta = std::fs::symlink_metadata(&link).unwrap();
    let file_legacy = format!("{}:{}", file_meta.dev(), file_meta.ino());
    let link_legacy = format!("{}:{}", link_meta.dev(), link_meta.ino());
    for (name, path, expected, kind, answer) in [
        (
            "file-content",
            file.clone(),
            file_content.clone(),
            "content",
            true,
        ),
        (
            "link-content",
            link.clone(),
            link_content.clone(),
            "content",
            true,
        ),
        ("file-legacy", file.clone(), file_legacy, "legacy", true),
        ("link-legacy", link.clone(), link_legacy, "legacy", true),
        ("mismatch", file.clone(), link_content, "content", false),
        (
            "bogus-kind",
            file.clone(),
            file_content.clone(),
            "bogus",
            false,
        ),
        ("empty-kind", file.clone(), file_content.clone(), "", false),
        (
            "missing",
            root.join("absent.conf"),
            file_content,
            "content",
            false,
        ),
    ] {
        assert_eq!(
            repos_overlays::replacement_generation_matches(&path, &expected, kind, root),
            answer,
            "{name}"
        );
    }
}

#[test]
fn replacement_transaction_requires_a_private_directory_with_only_staging_names() {
    let scope = TempDir::new("replacement-transaction").unwrap();
    for (name, expected) in [
        ("empty", true),
        ("next-only", true),
        ("previous-only", true),
        ("both", true),
        ("extra-file", false),
        ("extra-hidden", false),
        ("open-mode", false),
        ("as-file", false),
        ("as-link", false),
        ("missing", false),
    ] {
        let root = scope.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let transaction = root.join("txn");
        match name {
            "empty" | "next-only" | "previous-only" | "both" | "extra-file" | "extra-hidden" => {
                std::fs::create_dir(&transaction).unwrap();
                std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                if matches!(name, "next-only" | "both") {
                    std::os::unix::fs::symlink("target", transaction.join("next")).unwrap();
                }
                if matches!(name, "previous-only" | "both") {
                    std::os::unix::fs::symlink("target", transaction.join("previous")).unwrap();
                }
                if name == "extra-file" {
                    stage(&transaction, "stray", b"x", 0o600);
                }
                if name == "extra-hidden" {
                    stage(&transaction, ".hidden", b"x", 0o600);
                }
            }
            "open-mode" => {
                std::fs::create_dir(&transaction).unwrap();
                std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            "as-file" => {
                stage(&root, "txn", b"x", 0o600);
            }
            "as-link" => {
                std::os::unix::fs::symlink("elsewhere", &transaction).unwrap();
            }
            _ => {}
        }
        assert_eq!(
            repos_overlays::replacement_transaction_safe(
                &transaction,
                dot::temp::current_uid().unwrap()
            ),
            expected,
            "{name}"
        );
    }
}

struct RecordFixture {
    destination: String,
    physical: PathBuf,
    transaction: PathBuf,
    expected: String,
    legacy: String,
    parent_identity: String,
}

fn record_fixture(root: &Path) -> RecordFixture {
    let destination = root.join("app.conf").to_string_lossy().into_owned();
    let physical = stage(root, "physical/app.conf", b"body\n", 0o600);
    let parent = physical.parent().unwrap();
    let transaction = parent.join(".app.conf.dot-overlay-replace-v1");
    std::fs::create_dir(&transaction).unwrap();
    let expected = repos_overlays::replacement_identity(root, &physical).unwrap();
    let meta = std::fs::symlink_metadata(&physical).unwrap();
    let legacy = format!("{}:{}", meta.dev(), meta.ino());
    let parent_meta = std::fs::symlink_metadata(parent).unwrap();
    let parent_identity = format!("{}:{}", parent_meta.dev(), parent_meta.ino());
    RecordFixture {
        destination,
        physical,
        transaction,
        expected,
        legacy,
        parent_identity,
    }
}

fn line(f: &RecordFixture, expected: &str) -> Vec<u8> {
    format!(
        "{}\t{}\t.dotfiles-web/home/app.conf\t{}\t{}\t{}\n",
        f.destination,
        f.physical.display(),
        expected,
        f.transaction.display(),
        f.parent_identity
    )
    .into_bytes()
}

#[test]
fn replacement_read_validates_privacy_shape_name_identity_and_transaction_binding() {
    for case in [
        "content",
        "legacy",
        "wrong-name",
        "two-line",
        "relative-dest",
        "open-record",
        "transaction-mismatch",
        "garbage-expected",
        "extra-field",
    ] {
        let scope = TempDir::new("replacement-read").unwrap();
        let root = scope.path();
        let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
        let mut fixture = record_fixture(root);
        let expected = if case == "legacy" {
            fixture.legacy.clone()
        } else {
            fixture.expected.clone()
        };
        let record_name = if case == "legacy" {
            let alternate = repos_overlays::replacement_hash_object_format(
                "sha256",
                &fixture.destination,
                root,
            )
            .unwrap();
            format!("{manifest}.replace.{alternate}")
        } else if case == "wrong-name" {
            root.join("renamed").to_string_lossy().into_owned()
        } else {
            format!("{manifest}.replace.{}", hash(&fixture.destination))
        };
        if case == "relative-dest" {
            fixture.destination = "relative/app.conf".into();
        }
        if case == "transaction-mismatch" {
            fixture.transaction = root.join("elsewhere");
        }
        let mut body = line(
            &fixture,
            if case == "garbage-expected" {
                "zzz"
            } else {
                &expected
            },
        );
        if case == "two-line" {
            body.extend_from_slice(b"second\tline\there\n");
        }
        if case == "extra-field" {
            body.pop();
            body.extend_from_slice(b"\textra\n");
        }
        let record = PathBuf::from(record_name);
        std::fs::write(&record, body).unwrap();
        std::fs::set_permissions(
            &record,
            std::fs::Permissions::from_mode(if case == "open-record" { 0o644 } else { 0o600 }),
        )
        .unwrap();
        let read = repos_overlays::replacement_read(
            &record,
            &manifest,
            dot::temp::current_uid().unwrap(),
            root,
            root,
        );
        let accepted = matches!(case, "content" | "legacy");
        assert_eq!(read.is_some(), accepted, "{case}");
        if let Some(record) = read {
            assert_eq!(record.destination, fixture.destination);
            assert_eq!(record.physical, fixture.physical.to_string_lossy());
            assert_eq!(record.target, ".dotfiles-web/home/app.conf");
            assert_eq!(record.expected, expected);
            assert_eq!(
                record.identity_kind,
                if case == "legacy" {
                    ReplaceIdentityKind::Legacy
                } else {
                    ReplaceIdentityKind::Content
                }
            );
            assert_eq!(record.transaction, fixture.transaction.to_string_lossy());
            assert_eq!(record.parent_identity, fixture.parent_identity);
        }
    }
}

#[test]
fn replacement_cleanup_removes_only_the_expected_next_link_and_empty_transaction() {
    for (case, next, previous, target, expected) in [
        ("empty", None, false, "wanted", true),
        ("next-match", Some("wanted"), false, "wanted", true),
        ("next-mismatch", Some("other"), false, "wanted", false),
        ("previous-present", None, true, "wanted", false),
        ("next-file", Some("FILE"), false, "wanted", false),
    ] {
        let scope = TempDir::new("replacement-cleanup").unwrap();
        let root = scope.path();
        let transaction = root.join("txn");
        std::fs::create_dir(&transaction).unwrap();
        if let Some(next) = next {
            if next == "FILE" {
                stage(&transaction, "next", b"x", 0o600);
            } else {
                std::os::unix::fs::symlink(next, transaction.join("next")).unwrap();
            }
        }
        if previous {
            stage(&transaction, "previous", b"x", 0o600);
        }
        let record = stage(root, "record", b"line\n", 0o600);
        assert_eq!(
            repos_overlays::replacement_cleanup(&record, &transaction, target),
            expected,
            "{case}"
        );
        assert_eq!(record.exists(), !expected, "record {case}");
        assert_eq!(transaction.exists(), !expected, "transaction {case}");
        if !expected {
            assert_eq!(std::fs::read(&record).unwrap(), b"line\n");
        }
    }
}
