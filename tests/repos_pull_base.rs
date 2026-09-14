//! Native contracts for base-repository pull orchestration.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull::{PullBaseInputs, PullStatus, pull_base};
use dot::repos_pull_queries::CandidateEnv;
use dot_test_support::TempDir;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stage(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ],
    );
}

struct Side {
    _scope: TempDir,
    home: PathBuf,
    origin: PathBuf,
    manifest: String,
    legacy: String,
}

impl Side {
    fn new(case: &str) -> Self {
        let scope = TempDir::new("pull-base").unwrap();
        let origin = scope.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        stage(&origin, "base.txt", b"v1\n");
        commit(&origin, "seed");
        let home = scope.path().join("home");
        let output = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "clone", "-q"])
            .arg(&origin)
            .arg(&home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        match case {
            "skipped" => git(&home, &["branch", "--unset-upstream"]),
            "changed" => {
                stage(&origin, "newfile.txt", b"from origin\n");
                commit(&origin, "add newfile");
            }
            "conflict-backup" => {
                stage(&origin, "clash.txt", b"origin clash\n");
                commit(&origin, "add clash");
                stage(&home, "clash.txt", b"user clash\n");
            }
            "diverged" => {
                stage(&home, "base.txt", b"home change\n");
                commit(&home, "home change");
                stage(&origin, "base.txt", b"origin change\n");
                commit(&origin, "origin change");
            }
            "invalid-candidate" => {
                stage(&origin, ".dotfiles/evil", b"x\n");
                commit(&origin, "add evil");
            }
            "current" | "current-quiet" => {}
            _ => unreachable!(),
        }
        let home_text = home.to_string_lossy();
        let manifest = format!("{home_text}/manifest.tsv");
        let legacy = format!("{home_text}/legacy.tsv");
        Self {
            _scope: scope,
            home,
            origin,
            manifest,
            legacy,
        }
    }
}

#[test]
fn pull_base_preserves_status_failure_backup_and_candidate_safety_rows() {
    for (case, quiet, verbose, expected_status, expected_rc) in [
        ("skipped", false, false, PullStatus::Skipped, 0),
        ("current", false, true, PullStatus::Current, 0),
        ("current-quiet", true, false, PullStatus::Current, 0),
        ("changed", false, false, PullStatus::Changed, 0),
        ("conflict-backup", false, false, PullStatus::Changed, 0),
        ("diverged", false, false, PullStatus::Failed, 1),
        ("invalid-candidate", false, false, PullStatus::Failed, 1),
    ] {
        let side = Side::new(case);
        let home = side.home.to_string_lossy().into_owned();
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home.clone(),
        };
        let candidate = CandidateEnv {
            home: home.clone(),
            checkout: format!("{home}/.local/share/cgraf78/dot"),
            pwd: home.clone(),
            source_root: env!("CARGO_MANIFEST_DIR").into(),
            state_home: format!("{home}/.local/state"),
            install_root: format!("{home}/.local/share"),
            provider_state: format!("{home}/.local/state/shdeps"),
            overlay_paths: Vec::new(),
            init_backup: None,
        };
        let dest = DestinationInputs {
            pwd: home.clone(),
            home: home.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: Vec::new(),
            init_backup: None,
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let log = Log::new(false, false);
        let inputs = PullBaseInputs {
            base: &base,
            candidate: &candidate,
            quarantine: None,
            overlays: &[],
            dest: &dest,
            manifest: &side.manifest,
            legacy_manifest: &side.legacy,
            euid: dot::temp::current_uid().unwrap(),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            tmp: &side.home,
            tool: &tool,
            extra_args: &[] as &[OsString],
            quiet,
            verbose,
            log: &log,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = pull_base(&inputs, &mut moves, &mut stdout, &mut stderr);
        assert_eq!(outcome.status, expected_status, "{case}");
        assert_eq!(outcome.rc, expected_rc, "{case}");
        match case {
            "changed" => assert_eq!(
                std::fs::read(side.home.join("newfile.txt")).unwrap(),
                b"from origin\n"
            ),
            "conflict-backup" => {
                assert_eq!(
                    std::fs::read(side.home.join("clash.txt")).unwrap(),
                    b"origin clash\n"
                );
                let backups = std::fs::read_dir(side.home.join(".dot-backup/pull"))
                    .unwrap()
                    .flatten()
                    .collect::<Vec<_>>();
                assert_eq!(backups.len(), 1);
            }
            "invalid-candidate" => assert!(!side.home.join(".dotfiles/evil").exists()),
            "diverged" => {
                let body = std::fs::read_to_string(side.home.join("base.txt")).unwrap();
                assert!(body.starts_with("<<<<<<< HEAD\norigin change\n"));
                if body.contains("||||||| parent of ") {
                    assert!(body.contains("\nv1\n=======\n"));
                }
                assert!(body.contains("=======\nhome change\n>>>>>>> "));
                assert!(body.ends_with(" (home change)\n"));
            }
            _ => {}
        }
        assert!(!side.manifest.ends_with(".pending"));
        assert!(side.origin.is_dir());
    }
}
