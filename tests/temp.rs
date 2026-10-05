//! Explicit native contracts for the engine's file-safety primitives. The
//! hook-facing generation tokens and file transactions are pinned in
//! `hook_runtime_generation.rs`.

use dot::temp::{self, MoveCache};
use dot_test_support::TempDir;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

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

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn reading_umask_never_changes_concurrent_creation_modes() {
    let dir = TempDir::new("umask-concurrency").unwrap();
    let mask = temp::read_umask().unwrap();
    let expected = 0o777 & !mask;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let reader_barrier = barrier.clone();
    let reader = std::thread::spawn(move || {
        reader_barrier.wait();
        for _ in 0..5_000 {
            assert_eq!(temp::read_umask().unwrap(), mask);
        }
    });
    barrier.wait();
    for index in 0..5_000 {
        let path = dir.path().join(format!("entry-{index}"));
        std::fs::create_dir(&path).unwrap();
        assert_eq!(mode(&path) & 0o777, expected, "entry {index}");
    }
    reader.join().unwrap();
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
}
