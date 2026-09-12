//! Native contracts for destination creation and transactional link publication.

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use dot::repos_overlays;
use dot_test_support::TempDir;

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn private_dir(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn inputs(root: &Path) -> repos_overlays::DestinationInputs {
    let home = root.to_string_lossy().into_owned();
    repos_overlays::DestinationInputs {
        pwd: home.clone(),
        home,
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: Vec::new(),
        init_backup: None,
    }
}

fn state(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Err(_) => "absent".into(),
        Ok(meta) if meta.file_type().is_symlink() => {
            format!("link:{}", std::fs::read_link(path).unwrap().display())
        }
        Ok(meta) if meta.is_file() => format!(
            "file:{}",
            String::from_utf8_lossy(&std::fs::read(path).unwrap())
        ),
        Ok(_) => "other".into(),
    }
}

#[test]
fn ensure_destination_parent_enforces_home_relative_safe_components() {
    for (case, relative, setup, expected) in [
        ("at-home", "", "", true),
        ("nested", "a/b/c", "", true),
        ("existing", "a", "dir", true),
        ("blocked-file", "a", "file", false),
        ("blocked-link", "a", "link", false),
        ("outside", "/elsewhere/x", "", false),
        ("dotdot", "a/../b", "", false),
        ("git-dir", "a/.Git/b", "", false),
    ] {
        let scope = TempDir::new("publish-parent").unwrap();
        let root = scope.path();
        match setup {
            "dir" => std::fs::create_dir(root.join("a")).unwrap(),
            "file" => {
                stage(root, "a", b"x\n");
            }
            "link" => std::os::unix::fs::symlink("elsewhere", root.join("a")).unwrap(),
            _ => {}
        }
        let parent = if relative.starts_with('/') {
            relative.into()
        } else if relative.is_empty() {
            root.to_string_lossy().into_owned()
        } else {
            root.join(relative).to_string_lossy().into_owned()
        };
        assert_eq!(
            repos_overlays::ensure_destination_parent(&root.to_string_lossy(), &parent),
            expected,
            "{case}"
        );
        if expected {
            assert!(Path::new(&parent).is_dir(), "{case}");
        }
        if case == "blocked-file" {
            assert_eq!(std::fs::read(root.join("a")).unwrap(), b"x\n");
        }
        if case == "blocked-link" {
            assert_eq!(
                std::fs::read_link(root.join("a")).unwrap(),
                PathBuf::from("elsewhere")
            );
        }
    }
}

#[test]
fn ensure_destination_parent_clamps_default_acl_group_write() {
    let scope = TempDir::new("publish-parent-acl").unwrap();
    let root = scope.path();
    let Ok(acl) = std::process::Command::new("setfacl")
        .args(["-m", "d:u::rwx,d:g::rwx,d:o::rx"])
        .arg(root)
        .status()
    else {
        return;
    };
    if !acl.success() {
        return;
    }
    let parent = root.join(".local/lib/dotfiles");
    assert!(repos_overlays::ensure_destination_parent(
        &root.to_string_lossy(),
        &parent.to_string_lossy()
    ));
    for relative in [".local", ".local/lib", ".local/lib/dotfiles"] {
        assert_eq!(
            std::fs::metadata(root.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "{relative}"
        );
    }
}

#[test]
fn record_final_appends_exact_bytes_and_updates_current_only_after_success() {
    let scope = TempDir::new("publish-final").unwrap();
    let manifest = stage(scope.path(), "new.tsv", b"old\tbase\tt\n");
    let mut current = HashSet::new();
    assert!(repos_overlays::record_final(
        "app.conf",
        "web",
        ".config/app.conf",
        &manifest,
        &mut current
    ));
    assert_eq!(
        std::fs::read(&manifest).unwrap(),
        b"old\tbase\tt\napp.conf\tweb\t.config/app.conf\n"
    );
    assert_eq!(current, HashSet::from(["app.conf".to_string()]));

    let blocked = scope.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    let mut failed = HashSet::new();
    assert!(!repos_overlays::record_final(
        "x",
        "web",
        "target",
        &blocked,
        &mut failed
    ));
    assert!(failed.is_empty());
}

struct Crash {
    destination: String,
    physical: PathBuf,
    transaction: PathBuf,
    record: PathBuf,
    parent_identity: String,
    expected: String,
}

fn crash(root: &Path, relative: &str, manifest: &str) -> Crash {
    let destination = root.join(relative).to_string_lossy().into_owned();
    let stem = Path::new(relative)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let physical = stage(
        root,
        &if relative.contains('/') {
            "work/app.conf".to_string()
        } else {
            format!("work-{stem}/app.conf")
        },
        b"old",
    );
    let parent = physical.parent().unwrap();
    let transaction = parent.join(".app.conf.dot-overlay-replace-v1");
    let parent_meta = std::fs::symlink_metadata(parent).unwrap();
    let parent_identity = format!("{}:{}", parent_meta.dev(), parent_meta.ino());
    let expected = repos_overlays::replacement_identity(root, &physical).unwrap();
    let record = PathBuf::from(
        repos_overlays::replacement_record_path(&destination, manifest, root).unwrap(),
    );
    Crash {
        destination,
        physical,
        transaction,
        record,
        parent_identity,
        expected,
    }
}

fn record_body(f: &Crash, target: &str) -> Vec<u8> {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\n",
        f.destination,
        f.physical.display(),
        target,
        f.expected,
        f.transaction.display(),
        f.parent_identity
    )
    .into_bytes()
}

#[test]
fn recover_replacement_converges_settled_parked_and_linked_crash_states() {
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    for case in [
        "settled-no-transaction",
        "settled-mismatch",
        "restore-previous",
        "converged-link",
        "diverted-link",
        "previous-mismatch",
        "parent-changed",
        "transaction-unsafe",
        "next-wrong",
        "bad-record",
        "stale-transaction-cleanup",
        "link-cleanup",
    ] {
        let scope = TempDir::new("publish-recover").unwrap();
        let root = scope.path();
        let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
        let relative = if case == "diverted-link" {
            "alias/app.conf"
        } else if case == "converged-link" {
            "work/app.conf"
        } else {
            "dest/app.conf"
        };
        let fixture = crash(root, relative, &manifest);
        if case == "diverted-link" {
            std::os::unix::fs::symlink("elsewhere", root.join("alias")).unwrap();
            std::fs::create_dir(root.join("elsewhere")).unwrap();
        }
        match case {
            "settled-mismatch" => std::fs::write(&fixture.physical, b"new").unwrap(),
            "restore-previous" => {
                private_dir(&fixture.transaction);
                std::fs::rename(&fixture.physical, fixture.transaction.join("previous")).unwrap();
            }
            "converged-link" => {
                private_dir(&fixture.transaction);
                std::fs::rename(&fixture.physical, fixture.transaction.join("previous")).unwrap();
                std::os::unix::fs::symlink("want-target", &fixture.physical).unwrap();
            }
            "diverted-link" => {
                private_dir(&fixture.transaction);
                std::fs::rename(&fixture.physical, fixture.transaction.join("previous")).unwrap();
                std::os::unix::fs::symlink("want-target", &fixture.physical).unwrap();
            }
            "previous-mismatch" => {
                private_dir(&fixture.transaction);
                std::fs::remove_file(&fixture.physical).unwrap();
                stage(&fixture.transaction, "previous", b"changed");
            }
            "parent-changed" => {
                let parent = fixture.physical.parent().unwrap().to_path_buf();
                let swap = root.join("work-swap");
                std::fs::create_dir(&swap).unwrap();
                std::fs::write(swap.join("app.conf"), b"old").unwrap();
                std::fs::remove_file(&fixture.physical).unwrap();
                std::fs::remove_dir(&parent).unwrap();
                std::fs::rename(swap, parent).unwrap();
            }
            "transaction-unsafe" => {
                private_dir(&fixture.transaction);
                stage(&fixture.transaction, "stray", b"x");
            }
            "next-wrong" => {
                private_dir(&fixture.transaction);
                std::os::unix::fs::symlink("other", fixture.transaction.join("next")).unwrap();
            }
            "stale-transaction-cleanup" => private_dir(&fixture.transaction),
            "link-cleanup" => {
                private_dir(&fixture.transaction);
                std::fs::remove_file(&fixture.physical).unwrap();
                std::os::unix::fs::symlink("want-target", &fixture.physical).unwrap();
            }
            _ => {}
        }
        std::fs::write(
            &fixture.record,
            if case == "bad-record" {
                b"garbage\n".to_vec()
            } else {
                record_body(&fixture, "want-target")
            },
        )
        .unwrap();
        std::fs::set_permissions(&fixture.record, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ok = repos_overlays::recover_replacement(
            &fixture.record,
            &manifest,
            dot::temp::current_uid().unwrap(),
            root,
            root,
            &root.to_string_lossy(),
            &tool,
        );
        let expected_ok = matches!(
            case,
            "settled-no-transaction"
                | "restore-previous"
                | "converged-link"
                | "diverted-link"
                | "stale-transaction-cleanup"
                | "link-cleanup"
        );
        assert_eq!(ok, expected_ok, "{case}");
        assert_eq!(fixture.record.exists(), !expected_ok, "record {case}");
        if expected_ok {
            assert!(!fixture.transaction.exists(), "transaction {case}");
        }
        let expected_state = match case {
            "settled-no-transaction"
            | "restore-previous"
            | "diverted-link"
            | "stale-transaction-cleanup" => "file:old",
            "converged-link" | "link-cleanup" => "link:want-target",
            "settled-mismatch" => "file:new",
            "previous-mismatch" => "absent",
            _ => "file:old",
        };
        assert_eq!(state(&fixture.physical), expected_state, "physical {case}");
    }
}

#[test]
fn recover_replacements_uses_byte_order_and_stops_at_the_first_bad_record() {
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    for (case, bad_at) in [
        ("all-settled", None),
        ("first-bad", Some(0)),
        ("second-bad", Some(1)),
    ] {
        let scope = TempDir::new("publish-recover-all").unwrap();
        let root = scope.path();
        let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
        let mut fixtures = [
            crash(root, "aaa.conf", &manifest),
            crash(root, "zzz.conf", &manifest),
        ];
        fixtures.sort_by(|a, b| a.record.cmp(&b.record));
        for (index, fixture) in fixtures.iter().enumerate() {
            let body = if bad_at == Some(index) {
                b"garbage\n".to_vec()
            } else {
                record_body(fixture, "target")
            };
            std::fs::write(&fixture.record, body).unwrap();
            std::fs::set_permissions(&fixture.record, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let result = repos_overlays::recover_replacements(
            &manifest,
            dot::temp::current_uid().unwrap(),
            root,
            root,
            &root.to_string_lossy(),
            &tool,
        );
        match bad_at {
            None => assert_eq!(result, Ok(()), "{case}"),
            Some(index) => assert_eq!(
                result,
                Err(fixtures[index].record.to_string_lossy().into_owned()),
                "{case}"
            ),
        }
        for (index, fixture) in fixtures.iter().enumerate() {
            let remains = bad_at.is_some_and(|bad| index >= bad);
            assert_eq!(fixture.record.exists(), remains, "{case} record {index}");
        }
    }
}

#[test]
fn publish_link_preserves_fresh_and_replacement_failure_boundaries() {
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    for case in [
        "fresh-absent",
        "fresh-parent-missing",
        "fresh-occupied",
        "replace-ok",
        "replace-stale",
        "replace-transaction-blocked",
        "replace-recover-leftover",
        "replace-record-unrecoverable",
    ] {
        let scope = TempDir::new("publish-link").unwrap();
        let root = scope.path();
        let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
        let parent = root.join("sub/dir");
        let destination = parent.join("app.conf");
        let replacing = case.starts_with("replace-");
        if case != "fresh-parent-missing" {
            std::fs::create_dir_all(&parent).unwrap();
        }
        if case == "fresh-occupied" {
            std::fs::write(&destination, b"user\n").unwrap();
        }
        if replacing {
            std::fs::write(&destination, b"old").unwrap();
        }
        let expected =
            replacing.then(|| repos_overlays::replacement_identity(root, &destination).unwrap());
        if case == "replace-stale" {
            std::fs::write(&destination, b"new").unwrap();
        }
        let transaction = parent.join(".app.conf.dot-overlay-replace-v1");
        if case == "replace-transaction-blocked" {
            std::fs::create_dir(&transaction).unwrap();
        }
        let record = PathBuf::from(
            repos_overlays::replacement_record_path(
                &destination.to_string_lossy(),
                &manifest,
                root,
            )
            .unwrap(),
        );
        if matches!(
            case,
            "replace-recover-leftover" | "replace-record-unrecoverable"
        ) {
            if case == "replace-record-unrecoverable" {
                std::fs::write(&record, b"garbage\n").unwrap();
            } else {
                let parent_meta = std::fs::symlink_metadata(&parent).unwrap();
                let identity = repos_overlays::replacement_identity(root, &destination).unwrap();
                std::fs::write(
                    &record,
                    format!(
                        "{}\t{}\twant-target\t{}\t{}\t{}:{}\n",
                        destination.display(),
                        destination.display(),
                        identity,
                        transaction.display(),
                        parent_meta.dev(),
                        parent_meta.ino()
                    ),
                )
                .unwrap();
            }
            std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let destination_text = destination.to_string_lossy().into_owned();
        let environment = inputs(root);
        let ok = repos_overlays::publish_link(&repos_overlays::PublishLinkInputs {
            target: "want-target",
            destination: &destination_text,
            expected: expected.as_deref(),
            inputs: &environment,
            manifest: &manifest,
            euid: dot::temp::current_uid().unwrap(),
            source_root: root,
            tmp: root,
            tool: &tool,
        });
        let expected_ok = matches!(
            case,
            "fresh-absent" | "replace-ok" | "replace-recover-leftover"
        );
        assert_eq!(ok, expected_ok, "{case}");
        let expected_destination = match case {
            "fresh-absent" | "replace-ok" | "replace-recover-leftover" => "link:want-target",
            "fresh-parent-missing" => "absent",
            "fresh-occupied" => "file:user\n",
            "replace-stale" => "file:new",
            _ => "file:old",
        };
        assert_eq!(
            state(&destination),
            expected_destination,
            "destination {case}"
        );
        assert_eq!(
            record.exists(),
            case == "replace-record-unrecoverable",
            "record {case}"
        );
        assert_eq!(
            transaction.exists(),
            case == "replace-transaction-blocked",
            "transaction {case}"
        );
        let stages = std::fs::read_dir(&parent)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".app.conf.overlay-link.")
                    })
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(
            stages,
            usize::from(matches!(
                case,
                "replace-transaction-blocked" | "replace-record-unrecoverable"
            )),
            "stages {case}"
        );
    }
}
