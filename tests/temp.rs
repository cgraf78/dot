//! Explicit native contracts for crash-safe file transactions.

use dot::temp::{self, LockCtx, MoveCache};
use dot_test_support::TempDir;
use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

fn lock() -> LockCtx {
    LockCtx {
        test_mode: true,
        token_present: false,
    }
}

fn file(root: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn sibling_temp_is_private_empty_and_beside_destination() {
    let dir = TempDir::new("sibling-temp").unwrap();
    let dst = dir.path().join("fresh/sub/app.conf");
    let first = temp::sibling_tmp_for(&dst).unwrap();
    let second = temp::sibling_tmp_for(&dst).unwrap();
    assert_ne!(first, second);
    for path in [first, second] {
        assert_eq!(path.parent(), dst.parent());
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("app.conf.tmp."));
        assert_eq!(name["app.conf.tmp.".len()..].len(), 6);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }
}

#[test]
fn stat_helpers_and_private_validators_reject_unsafe_shapes() {
    let dir = TempDir::new("temp-stat").unwrap();
    let regular = file(dir.path(), "regular", b"data", 0o600);
    let metadata = std::fs::metadata(&regular).unwrap();
    assert_eq!(
        temp::path_identity(&regular).unwrap(),
        (metadata.dev(), metadata.ino())
    );
    assert_eq!(temp::file_mode(&regular).unwrap(), 0o600);
    assert_eq!(temp::file_size(&regular).unwrap(), 4);
    assert_eq!(temp::path_uid(&regular).unwrap(), metadata.uid());
    assert_eq!(temp::path_nlink(&regular).unwrap(), 1);
    assert_eq!(temp::identity_string((12, 34)), "12:34");
    assert!(temp::path_identity(&dir.path().join("missing")).is_err());

    let private = dir.path().join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(temp::private_dir_validate(&private).is_ok());
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(temp::private_dir_validate(&private).is_err());
    assert!(temp::private_dir_validate(&regular).is_err());
    assert!(temp::private_control_file_validate(&regular).is_ok());
    std::fs::set_permissions(&regular, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(temp::private_control_file_validate(&regular).is_err());
    std::os::unix::fs::symlink(&regular, dir.path().join("link")).unwrap();
    assert!(temp::private_control_file_validate(&dir.path().join("link")).is_err());
}

#[test]
fn tracked_modes_and_umask_ceiling_are_deterministic() {
    let dir = TempDir::new("temp-modes").unwrap();
    for (git_mode, mask, expected) in [
        ("100644", 0o022, 0o644),
        ("100755", 0o022, 0o755),
        ("100755", 0o077, 0o700),
        ("100644", 0o077, 0o600),
    ] {
        let path = file(dir.path(), &format!("f-{git_mode}-{mask}"), b"x", 0o777);
        temp::apply_tracked_file_mode(&path, git_mode, mask).unwrap();
        assert_eq!(mode(&path), expected);
    }
    let path = file(dir.path(), "ceiling", b"x", 0o777);
    temp::apply_umask_ceiling(&path, None, 0o027).unwrap();
    assert_eq!(mode(&path), 0o750);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    temp::apply_umask_ceiling(&path, Some(0o777), 0o027).unwrap();
    assert_eq!(mode(&path), 0o640, "ceiling never adds permissions");
    assert!(temp::apply_tracked_file_mode(&path, "120000", 0o022).is_err());
}

#[test]
fn read_umask_observes_the_inherited_process_mask() {
    const CHILD: &str = "DOT_TEST_READ_UMASK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(temp::read_umask().unwrap(), 0o077);
        return;
    }

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg("read_umask_observes_the_inherited_process_mask")
        .env(CHILD, "1");
    // SAFETY: `umask` is async-signal-safe and this closure performs no other
    // work between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    assert!(command.status().unwrap().success());
}

#[test]
fn git_digests_equality_and_hash_pair_contracts_are_byte_exact() {
    let dir = TempDir::new("temp-digest").unwrap();
    let a = file(dir.path(), "a", b"same\n", 0o644);
    let b = file(dir.path(), "b", b"same\n", 0o644);
    let c = file(dir.path(), "c", b"different\n", 0o644);
    let digest = temp::file_digest(dir.path(), &a).unwrap();
    assert!(matches!(digest.len(), 40 | 64));
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(
        digest,
        temp::file_text_digest(dir.path(), b"same\n").unwrap()
    );
    assert!(temp::files_equal(dir.path(), &a, &b).unwrap());
    assert!(!temp::files_equal(dir.path(), &a, &c).unwrap());
    assert!(temp::stdin_matches_file(dir.path(), b"same\n", &a).unwrap());
    assert!(temp::hash_pair_equal(&format!("{digest}\n{digest}\n")));
    assert!(!temp::hash_pair_equal(&format!("{digest}\n0{digest}")));
    assert!(!temp::hash_pair_equal("not-a-hash\nnot-a-hash"));
}

#[test]
fn target_generation_and_signature_bind_path_parent_and_content() {
    let dir = TempDir::new("temp-generation").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"v1\n", 0o640);
    let target = temp::file_target_resolve(dir.path(), &dst).unwrap();
    assert_eq!(target.path, dst.canonicalize().unwrap());
    assert_eq!(target.parent, dst.parent().unwrap().canonicalize().unwrap());
    assert!(
        target
            .transaction
            .ends_with(".app.conf.dot-file-transaction-v1")
    );
    let signature = temp::file_signature(dir.path(), &dst).unwrap();
    assert_eq!(signature.mode, 0o640);
    assert_eq!(signature.size, 3);
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let expected = temp::generation_validate(dir.path(), &token).unwrap();
    assert_eq!(expected.state, "file");
    assert_eq!(expected.path_digest, target.path_digest);
    assert_eq!(expected.parent_id, target.parent_id);
    assert_eq!(
        expected.signature.as_deref(),
        Some(signature.to_string().as_str())
    );
    let absent = dir.path().join("home/absent");
    assert_eq!(
        temp::generation_validate(
            dir.path(),
            &temp::file_generation_raw(dir.path(), &absent).unwrap()
        )
        .unwrap()
        .state,
        "absent"
    );
    for bad in [
        "",
        "v2|bad",
        &(token.clone() + "\n"),
        &(token[..token.len() - 1].to_string() + "0"),
    ] {
        assert!(temp::generation_validate(dir.path(), bad).is_err());
    }
    assert!(temp::file_target_resolve(dir.path(), Path::new("relative")).is_err());
    assert!(temp::file_target_resolve(dir.path(), &dir.path().join("bad\nname")).is_err());
}

#[test]
fn move_contracts_never_replace_or_nest_unowned_targets() {
    let dir = TempDir::new("temp-move").unwrap();
    let mut cache = MoveCache::default();
    let source = file(dir.path(), "source", b"source", 0o600);
    let target = dir.path().join("target");
    temp::move_noreplace_cached(&source, &target, &mut cache).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"source");
    let blocked = file(dir.path(), "blocked-source", b"new", 0o600);
    assert!(temp::move_noreplace_cached(&blocked, &target, &mut cache).is_err());
    assert_eq!(std::fs::read(&blocked).unwrap(), b"new");
    assert_eq!(std::fs::read(&target).unwrap(), b"source");
    let replacement = file(dir.path(), "replacement", b"replacement", 0o600);
    temp::move_replace_nodir_cached(&replacement, &target, &mut cache).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
    let directory = dir.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    let another = file(dir.path(), "another", b"x", 0o600);
    assert!(temp::move_replace_nodir_cached(&another, &directory, &mut cache).is_err());
    assert!(another.exists());
}

#[test]
fn replace_and_remove_lifecycle_commit_only_matching_generation() {
    let dir = TempDir::new("temp-lifecycle").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"v1\n", 0o644);
    let mut cache = MoveCache::default();
    let token = temp::file_generation(dir.path(), lock(), &dst, &mut cache).unwrap();
    let source = file(dir.path(), "home/app.conf.new", b"v2\n", 0o600);
    temp::commit_tmp_if_generation(dir.path(), lock(), &source, &dst, &token, &mut cache).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"v2\n");
    assert!(!source.exists());
    let stale = token;
    let source = file(dir.path(), "home/app.conf.new", b"v3\n", 0o600);
    assert!(
        temp::commit_tmp_if_generation(dir.path(), lock(), &source, &dst, &stale, &mut cache)
            .is_err()
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"v2\n");
    let current = temp::file_generation(dir.path(), lock(), &dst, &mut cache).unwrap();
    temp::remove_if_generation(dir.path(), lock(), &dst, &current, &mut cache).unwrap();
    assert!(!dst.exists());
}

#[test]
fn remove_refuses_an_absent_generation_after_late_creation() {
    let dir = TempDir::new("temp-remove-late-create").unwrap();
    let dst = dir.path().join("home/app.conf");
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    let mut cache = MoveCache::default();
    let absent = temp::file_generation(dir.path(), lock(), &dst, &mut cache).unwrap();
    std::fs::write(&dst, b"late creation\n").unwrap();
    assert!(temp::remove_if_generation(dir.path(), lock(), &dst, &absent, &mut cache).is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), b"late creation\n");
}

#[test]
fn malformed_generation_cannot_remove_live_content() {
    let dir = TempDir::new("temp-remove-malformed").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"late creation\n", 0o644);
    let mut cache = MoveCache::default();
    assert!(temp::remove_if_generation(dir.path(), lock(), &dst, "v1|forged", &mut cache).is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), b"late creation\n");
}

#[test]
fn generation_capture_rejects_a_symlink_destination() {
    let dir = TempDir::new("temp-generation-symlink").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let link = dir.path().join("home/link");
    std::os::unix::fs::symlink(&dst, &link).unwrap();
    assert!(temp::file_generation(dir.path(), lock(), &link, &mut MoveCache::default()).is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n");
}

#[test]
fn generation_capture_requires_an_outer_update_lock() {
    let dir = TempDir::new("temp-generation-lock").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    assert!(
        temp::file_generation(
            dir.path(),
            LockCtx {
                test_mode: false,
                token_present: false
            },
            &dst,
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n");
}

#[test]
fn generation_token_cannot_follow_a_replaced_parent_symlink() {
    let dir = TempDir::new("temp-parent-swap").unwrap();
    let parent_a = dir.path().join("parent-a");
    let parent_b = dir.path().join("parent-b");
    let selected = dir.path().join("selected-parent");
    let first = file(&parent_a, "config", b"parent a\n", 0o644);
    let second = file(&parent_b, "config", b"parent b\n", 0o644);
    std::os::unix::fs::symlink(&parent_a, &selected).unwrap();
    let logical = selected.join("config");
    let mut cache = MoveCache::default();
    let generation = temp::file_generation(dir.path(), lock(), &logical, &mut cache).unwrap();
    std::fs::remove_file(&selected).unwrap();
    std::os::unix::fs::symlink(&parent_b, &selected).unwrap();
    let candidate = file(&parent_b, "candidate", b"redirected update\n", 0o600);
    assert!(
        temp::commit_tmp_if_generation(
            dir.path(),
            lock(),
            &candidate,
            &logical,
            &generation,
            &mut cache,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(first).unwrap(), b"parent a\n");
    assert_eq!(std::fs::read(second).unwrap(), b"parent b\n");
    assert_eq!(std::fs::read(candidate).unwrap(), b"redirected update\n");
}

#[test]
fn publication_conflict_preserves_the_late_winner_and_cleans_the_journal() {
    let dir = TempDir::new("temp-publish-conflict").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"candidate\n", 0o600);
    let mut cache = MoveCache::default();
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let prepared = temp::transaction_prepare(
        dir.path(),
        lock(),
        "replace",
        Some(&source),
        &dst,
        &token,
        &mut cache,
    )
    .unwrap();
    temp::transaction_quarantine(dir.path(), &prepared, &mut cache).unwrap();
    std::fs::write(&dst, b"late winner\n").unwrap();
    assert!(
        temp::move_noreplace_cached(&prepared.transaction.join("candidate"), &dst, &mut cache)
            .is_err()
    );
    temp::transaction_recover(
        dir.path(),
        &dst,
        &prepared.transaction,
        &prepared.target,
        &mut cache,
    )
    .unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"late winner\n");
    assert!(!prepared.transaction.exists());
}

#[test]
fn quarantine_conflict_preserves_the_replacement_and_cleans_the_journal() {
    let dir = TempDir::new("temp-quarantine-conflict").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"candidate\n", 0o600);
    let mut cache = MoveCache::default();
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let prepared = temp::transaction_prepare(
        dir.path(),
        lock(),
        "replace",
        Some(&source),
        &dst,
        &token,
        &mut cache,
    )
    .unwrap();
    std::fs::write(&dst, b"quarantine winner\n").unwrap();
    // Model the winner arriving after quarantine's generation recheck but
    // immediately before its no-replace move. Recovery must identify the
    // moved file as foreign by signature and restore it to the live name.
    temp::move_noreplace_cached(&dst, &prepared.transaction.join("previous"), &mut cache).unwrap();
    temp::transaction_recover(
        dir.path(),
        &dst,
        &prepared.transaction,
        &prepared.target,
        &mut cache,
    )
    .unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"quarantine winner\n");
    assert!(!source.exists());
    assert!(!prepared.transaction.exists());
}

/// Leave a replacement journal at the selected crash boundary, then trigger
/// recovery through the next public generation capture.
fn recover_replace_after_crash(label: &str, publish: bool, expected: &[u8]) {
    let dir = TempDir::new(&format!("temp-replace-crash-{label}")).unwrap();
    let dst = file(dir.path(), "home/app.conf", b"before crash\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"after crash\n", 0o600);
    let mut cache = MoveCache::default();
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let prepared = temp::transaction_prepare(
        dir.path(),
        lock(),
        "replace",
        Some(&source),
        &dst,
        &token,
        &mut cache,
    )
    .unwrap();
    temp::transaction_quarantine(dir.path(), &prepared, &mut cache).unwrap();
    if publish {
        temp::move_noreplace_cached(&prepared.transaction.join("candidate"), &dst, &mut cache)
            .unwrap();
    }
    temp::file_generation(dir.path(), lock(), &dst, &mut cache).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), expected);
    assert!(!prepared.transaction.exists());
}

#[test]
fn replace_recovery_restores_previous_content_after_prepublication_crash() {
    recover_replace_after_crash("before-publication", false, b"before crash\n");
}

#[test]
fn replace_recovery_keeps_candidate_after_postpublication_crash() {
    recover_replace_after_crash("after-publication", true, b"after crash\n");
}

/// Leave a removal journal at the selected crash boundary, optionally create
/// a late live winner, then trigger recovery through generation capture.
fn recover_remove_after_crash(
    label: &str,
    committed: bool,
    late: Option<&[u8]>,
    expected: Option<&[u8]>,
) {
    let dir = TempDir::new(&format!("temp-remove-crash-{label}")).unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let mut cache = MoveCache::default();
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let prepared =
        temp::transaction_prepare(dir.path(), lock(), "remove", None, &dst, &token, &mut cache)
            .unwrap();
    temp::transaction_quarantine(dir.path(), &prepared, &mut cache).unwrap();
    if committed {
        temp::record_write(
            &prepared.transaction,
            "remove",
            "committed",
            &token,
            None,
            &mut cache,
        )
        .unwrap();
    }
    if let Some(bytes) = late {
        std::fs::write(&dst, bytes).unwrap();
    }
    temp::file_generation(dir.path(), lock(), &dst, &mut cache).unwrap();
    assert_eq!(std::fs::read(&dst).ok().as_deref(), expected);
    assert!(!prepared.transaction.exists());
}

#[test]
fn remove_recovery_restores_file_after_precommit_crash() {
    recover_remove_after_crash("before-commit", false, None, Some(b"managed\n"));
}

#[test]
fn remove_recovery_keeps_file_absent_after_postcommit_crash() {
    recover_remove_after_crash("after-commit", true, None, None);
}

#[test]
fn remove_recovery_preserves_a_late_creation() {
    recover_remove_after_crash(
        "late-creation",
        false,
        Some(b"late winner\n"),
        Some(b"late winner\n"),
    );
}

#[test]
fn committed_content_survives_cleanup_failure_and_next_capture_retries_cleanup() {
    let dir = TempDir::new("temp-cleanup-retry").unwrap();
    let parent = dir.path().join("home");
    let dst = file(&parent, "app.conf", b"managed\n", 0o644);
    let source = file(&parent, "candidate", b"cleanup retry\n", 0o600);
    let mut cache = MoveCache::default();
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let prepared = temp::transaction_prepare(
        dir.path(),
        lock(),
        "replace",
        Some(&source),
        &dst,
        &token,
        &mut cache,
    )
    .unwrap();
    temp::transaction_quarantine(dir.path(), &prepared, &mut cache).unwrap();
    temp::move_noreplace_cached(&prepared.transaction.join("candidate"), &dst, &mut cache).unwrap();
    temp::record_write(
        &prepared.transaction,
        "replace",
        "committed",
        &token,
        prepared.candidate.as_ref(),
        &mut cache,
    )
    .unwrap();
    let unexpected = prepared.transaction.join("unexpected");
    std::fs::write(&unexpected, b"invalid transaction entry\n").unwrap();
    assert!(temp::transaction_cleanup(&prepared.transaction, &mut cache).is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), b"cleanup retry\n");
    assert!(prepared.transaction.is_dir());
    std::fs::remove_file(unexpected).unwrap();
    temp::file_generation(dir.path(), lock(), &dst, &mut cache).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"cleanup retry\n");
    assert!(!prepared.transaction.exists());
}

#[test]
fn generation_rejects_unmarked_or_nonprivate_transaction_directories_in_place() {
    for (label, mode) in [("unmarked", 0o700), ("nonprivate", 0o755)] {
        let dir = TempDir::new(&format!("temp-unsafe-transaction-{label}")).unwrap();
        let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
        let target = temp::file_target_resolve(dir.path(), &dst).unwrap();
        std::fs::create_dir(&target.transaction).unwrap();
        std::fs::set_permissions(&target.transaction, std::fs::Permissions::from_mode(mode))
            .unwrap();
        assert!(
            temp::file_generation(dir.path(), lock(), &dst, &mut MoveCache::default()).is_err(),
            "{label}"
        );
        assert!(target.transaction.is_dir(), "{label}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n", "{label}");
    }
}

#[test]
fn rejected_transaction_setup_preserves_inputs_without_debris() {
    let dir = TempDir::new("temp-private-setup-failure").unwrap();
    let parent = dir.path().join("home");
    let dst = file(&parent, "app.conf", b"managed\n", 0o644);
    let source = file(dir.path(), "staging/candidate", b"candidate\n", 0o600);
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let result = temp::commit_tmp_if_generation(
        dir.path(),
        lock(),
        &source,
        &dst,
        &token,
        &mut MoveCache::default(),
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n");
    assert_eq!(std::fs::read(&source).unwrap(), b"candidate\n");
    let mut entries: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, vec![OsStr::new("app.conf").to_os_string()]);
    assert_eq!(
        std::fs::read_dir(source.parent().unwrap()).unwrap().count(),
        1
    );
}

#[test]
fn prepare_journal_and_recovery_restore_quarantined_file() {
    let dir = TempDir::new("temp-recover").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"old\n", 0o644);
    let source = file(dir.path(), "home/app.conf.new", b"new\n", 0o600);
    let mut cache = MoveCache::default();
    let token = temp::file_generation_raw(dir.path(), &dst).unwrap();
    let prepared = temp::transaction_prepare(
        dir.path(),
        lock(),
        "replace",
        Some(&source),
        &dst,
        &token,
        &mut cache,
    )
    .unwrap();
    let record = temp::record_read(dir.path(), &prepared.transaction).unwrap();
    assert_eq!(record.operation, "replace");
    assert_eq!(record.phase, "prepared");
    assert!(record.candidate.is_some());
    temp::transaction_quarantine(dir.path(), &prepared, &mut cache).unwrap();
    assert!(!dst.exists());
    assert_eq!(
        temp::record_read(dir.path(), &prepared.transaction)
            .unwrap()
            .phase,
        "quarantined"
    );
    temp::transaction_recover(
        dir.path(),
        &dst,
        &prepared.transaction,
        &prepared.target,
        &mut cache,
    )
    .unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"old\n");
    assert!(!prepared.transaction.exists());
}

#[test]
fn metadata_tree_is_clamped_without_following_symlinks() {
    let dir = TempDir::new("temp-metadata").unwrap();
    let root = dir.path().join("tree");
    std::fs::create_dir(&root).unwrap();
    let nested = file(&root, "dir/file", b"x", 0o777);
    std::fs::set_permissions(root.join("dir"), std::fs::Permissions::from_mode(0o777)).unwrap();
    temp::apply_git_metadata_modes(&root, 0o027).unwrap();
    assert_eq!(mode(&root), 0o750);
    assert_eq!(mode(&root.join("dir")), 0o750);
    assert_eq!(mode(&nested), 0o750);
    assert!(temp::apply_git_metadata_modes(&nested, 0o022).is_err());
}

#[test]
fn lock_context_truth_table_is_explicit() {
    for (test_mode, token_present, expected) in [
        (false, false, false),
        (true, false, true),
        (false, true, true),
        (true, true, true),
    ] {
        assert_eq!(
            LockCtx {
                test_mode,
                token_present
            }
            .valid(),
            expected
        );
    }
}

#[test]
fn journal_reader_rejects_every_corrupt_control_shape() {
    let dir = TempDir::new("temp-record-bad").unwrap();
    let live = file(dir.path(), "app.conf", b"v1\n", 0o644);
    let token = temp::file_generation_raw(dir.path(), &live).unwrap();
    for (label, body) in [
        ("bad-version", format!("v2\treplace\tprepared\t{token}\t-")),
        ("bad-op", format!("v1\trename\tprepared\t{token}\t-")),
        ("bad-phase", format!("v1\treplace\tstaged\t{token}\t-")),
        ("bad-token", "v1\treplace\tprepared\tbogus\t-".into()),
        (
            "bad-candidate",
            format!("v1\treplace\tprepared\t{token}\tbogus"),
        ),
        (
            "remove-candidate",
            format!(
                "v1\tremove\tprepared\t{token}\t1|2|644|3|{}",
                "0".repeat(40)
            ),
        ),
        ("four-fields", format!("v1\treplace\tprepared\t{token}")),
        ("empty", String::new()),
    ] {
        let txn = dir.path().join(label);
        std::fs::create_dir(&txn).unwrap();
        std::fs::set_permissions(&txn, std::fs::Permissions::from_mode(0o700)).unwrap();
        let record = file(&txn, "record", body.as_bytes(), 0o600);
        assert!(temp::record_read(dir.path(), &txn).is_err(), "{label}");
        assert!(record.exists());
    }
    let txn = dir.path().join("wedged");
    std::fs::create_dir(&txn).unwrap();
    std::fs::set_permissions(&txn, std::fs::Permissions::from_mode(0o700)).unwrap();
    file(&txn, "record.next", b"stale\n", 0o600);
    assert!(
        temp::record_write(
            &txn,
            "remove",
            "committed",
            &token,
            None,
            &mut MoveCache::default()
        )
        .is_err()
    );
}

#[test]
fn nonminimal_generation_validates_but_recovery_fails_closed() {
    let dir = TempDir::new("temp-forged").unwrap();
    let live = file(dir.path(), "home/app.conf", b"v1\n", 0o644);
    let token = temp::file_generation_raw(dir.path(), &live).unwrap();
    let owned: Vec<String> = token.split('|').map(str::to_string).collect();
    let mut fields: Vec<&str> = owned.iter().map(String::as_str).collect();
    fields[2] = "007";
    let payload = fields[..10].join("|");
    let checksum = temp::file_text_digest(
        dir.path(),
        format!("dot-file-generation-v1|{payload}").as_bytes(),
    )
    .unwrap();
    let forged = format!("{payload}|{checksum}");
    assert!(temp::generation_validate(dir.path(), &forged).is_ok());
    let target = temp::file_target_resolve(dir.path(), &live).unwrap();
    std::fs::create_dir(&target.transaction).unwrap();
    std::fs::set_permissions(&target.transaction, std::fs::Permissions::from_mode(0o700)).unwrap();
    let candidate = format!("1|2|644|3|{}", "0".repeat(40));
    file(
        &target.transaction,
        "record",
        format!("v1\treplace\tprepared\t{forged}\t{candidate}\n").as_bytes(),
        0o600,
    );
    assert!(temp::record_read(dir.path(), &target.transaction).is_ok());
    assert!(
        temp::transaction_recover(
            dir.path(),
            &live,
            &target.transaction,
            &target,
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&live).unwrap(), b"v1\n");
    assert!(target.transaction.exists());
}

#[test]
fn bsd_style_move_recovers_a_source_nested_by_late_directory() {
    let dir = TempDir::new("temp-bsd-move").unwrap();
    let source = file(dir.path(), "source", b"payload", 0o600);
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let tool = temp::MoveTool {
        bin: PathBuf::from("/bin/mv"),
        no_target_dir: false,
    };
    assert!(temp::move_noreplace_with(&source, &target, &tool).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), b"payload");
    assert!(!target.join("source").exists());
}

#[test]
fn git_commands_scrub_caller_repository_state_and_bind_source_root() {
    use std::ffi::OsStr;
    let dir = TempDir::new("temp-git-command").unwrap();
    let command = temp::sanitized_git(dir.path(), &["rev-parse", "--show-toplevel"]);
    let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
    ] {
        assert_eq!(
            env.get(OsStr::new(key)),
            Some(&None),
            "{key} must be removed"
        );
    }
    assert_eq!(
        env.get(OsStr::new("GIT_CONFIG_GLOBAL")),
        Some(&Some(OsStr::new("/dev/null")))
    );
    assert_eq!(
        env.get(OsStr::new("GIT_CONFIG_NOSYSTEM")),
        Some(&Some(OsStr::new("1")))
    );
    assert!(command.get_args().any(|arg| arg == dir.path().as_os_str()));
    let source = temp::source_git(&["rev-parse", "--show-toplevel"]).unwrap();
    assert!(
        source
            .get_args()
            .any(|arg| arg == std::env::current_dir().unwrap().as_os_str())
    );
}
