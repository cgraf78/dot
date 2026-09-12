//! Native integration tests for pull-conflict backup and managed-link adoption.

use dot::log::Log;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::{DestinationInputs, QuarantineInputs, RollbackSnapshot};
use dot::repos_pull_backup::{BackupConflictsInputs, BackupOutcome, backup_pull_conflicts};
use dot_test_support::TempDir;
use std::path::{Path, PathBuf};

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

fn pull_log(rels: &[&str]) -> String {
    let mut log =
        String::from("error: untracked working tree files would be overwritten by merge:\n");
    for rel in rels {
        log.push_str(&format!("\t{rel}\n"));
    }
    log
}

struct Fixture {
    _dir: TempDir,
    home: PathBuf,
    root: PathBuf,
    home_text: String,
    root_text: String,
}
impl Fixture {
    fn new(tag: &str, root_is_home: bool) -> Self {
        let dir = TempDir::new(tag).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let root = if root_is_home {
            home.clone()
        } else {
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            root
        };
        Self {
            home_text: home.to_string_lossy().into_owned(),
            root_text: root.to_string_lossy().into_owned(),
            _dir: dir,
            home,
            root,
        }
    }

    fn run(&self, log: &str, snapshot: RollbackSnapshot) -> (BackupOutcome, Vec<u8>) {
        let log_path = self._dir.path().join("pull.log");
        std::fs::write(&log_path, log).unwrap();
        let dest = DestinationInputs {
            pwd: self.home_text.clone(),
            home: self.home_text.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![],
            init_backup: None,
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let quarantine = (self.root == self.home).then(|| QuarantineInputs {
            snapshot,
            context: dest.clone(),
            tool: tool.clone(),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
        });
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: self.home_text.clone(),
        };
        let logger = Log::new(false, false);
        let mut warnings = vec![];
        let outcome = backup_pull_conflicts(
            &BackupConflictsInputs {
                home: &self.home_text,
                root: &self.root_text,
                pull_log: &log_path,
                base: &base,
                quarantine,
                overlays: &[],
                dest: &dest,
                manifest: &format!("{}/manifest", self.home_text),
                legacy_manifest: &format!("{}/legacy", self.home_text),
                euid: dot::temp::current_uid().unwrap(),
                source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
                tmp: &self.home,
                log: &logger,
                tool: &tool,
            },
            &mut moves,
            &mut warnings,
        );
        (outcome, warnings)
    }
}

fn empty_snapshot() -> RollbackSnapshot {
    RollbackSnapshot {
        paths: vec![],
        targets: vec![],
    }
}

#[test]
fn missing_or_unrecognized_conflicts_do_nothing() {
    let fixture = Fixture::new("backup-empty", false);
    stage(&fixture.root, "note.txt", b"user\n");
    let (outcome, warnings) = fixture.run("error: unrelated failure\n", empty_snapshot());
    assert!(!outcome.succeeded);
    assert!(outcome.backup.is_none());
    assert!(warnings.is_empty());
    assert_eq!(
        std::fs::read(fixture.root.join("note.txt")).unwrap(),
        b"user\n"
    );

    let absent = Fixture::new("backup-absent", false);
    let (outcome, _) = absent.run(&pull_log(&["missing.txt"]), empty_snapshot());
    assert!(!outcome.succeeded);
    assert!(outcome.backup.is_none());
}

#[test]
fn files_and_dangling_links_are_moved_to_a_private_backup() {
    let fixture = Fixture::new("backup-files", false);
    stage(&fixture.root, "nested/note.txt", b"user data\n");
    std::os::unix::fs::symlink("gone", fixture.root.join("dangling")).unwrap();
    let (outcome, warnings) = fixture.run(
        &pull_log(&["nested/note.txt", "dangling"]),
        empty_snapshot(),
    );
    assert!(outcome.succeeded);
    let backup = outcome.backup.unwrap();
    assert!(!fixture.root.join("nested/note.txt").exists());
    assert!(fixture.root.join("dangling").symlink_metadata().is_err());
    assert_eq!(
        std::fs::read(backup.join("nested/note.txt")).unwrap(),
        b"user data\n"
    );
    assert_eq!(
        std::fs::read_link(backup.join("dangling")).unwrap(),
        Path::new("gone")
    );
    assert!(
        String::from_utf8(warnings)
            .unwrap()
            .contains("backed up 2 conflicting untracked files")
    );
}

#[test]
fn a_later_failure_restores_already_moved_conflicts() {
    let fixture = Fixture::new("backup-rollback", false);
    stage(&fixture.root, "sub/file", b"nested\n");
    let (outcome, _) = fixture.run(&pull_log(&["sub/file", "sub"]), empty_snapshot());
    assert!(!outcome.succeeded);
    assert_eq!(
        std::fs::read(fixture.root.join("sub/file")).unwrap(),
        b"nested\n"
    );
}

#[test]
fn managed_links_are_adopted_instead_of_backed_up() {
    let fixture = Fixture::new("backup-adopt", true);
    let target = stage(&fixture.root, "target.txt", b"managed\n");
    std::fs::create_dir_all(fixture.root.join("sub")).unwrap();
    std::os::unix::fs::symlink(&target, fixture.root.join("sub/anchor")).unwrap();
    let snapshot = RollbackSnapshot {
        paths: vec!["sub/anchor".into()],
        targets: vec![target.to_string_lossy().into_owned()],
    };
    let (outcome, warnings) = fixture.run(&pull_log(&["sub/anchor"]), snapshot);
    assert!(outcome.succeeded);
    assert!(outcome.backup.is_none());
    assert!(fixture.root.join("sub/anchor").symlink_metadata().is_err());
    assert!(
        String::from_utf8(warnings)
            .unwrap()
            .contains("adopted 1 managed overlay path")
    );
    assert!(!fixture.home.join(".dot-backup/pull").exists());
}

#[test]
fn reserved_paths_fail_without_moving_user_data() {
    let fixture = Fixture::new("backup-reserved", true);
    stage(&fixture.root, ".dotfiles-evil/x", b"user\n");
    let snapshot = RollbackSnapshot {
        paths: vec![".dotfiles-evil/x".into()],
        targets: vec!["whatever".into()],
    };
    let (outcome, _) = fixture.run(&pull_log(&[".dotfiles-evil/x"]), snapshot);
    assert!(!outcome.succeeded);
    assert_eq!(
        std::fs::read(fixture.root.join(".dotfiles-evil/x")).unwrap(),
        b"user\n"
    );
}
