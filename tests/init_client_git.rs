//! Native integration contracts for Git staging and publication.
use dot::errors::{Error, Result};
use dot::init_client_generation as generation;
use dot::init_client_git::{self as stage, GitStageDeps, GitStageInputs};
use dot::temp::{self, MoveCache};
use dot_test_support::TempDir;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("HOME", cwd)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().into()
}

struct Case {
    _dir: TempDir,
    home: PathBuf,
    backup: PathBuf,
    live: PathBuf,
    origin: PathBuf,
    record: PathBuf,
    commit: String,
    branch: String,
    identity: String,
    nonce: String,
}
impl Case {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag).unwrap();
        let home = dir.path().join("home");
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--quiet"]);
        std::fs::write(origin.join("file.txt"), b"seed\n").unwrap();
        git(&origin, &["add", "file.txt"]);
        git(&origin, &["commit", "--quiet", "-m", "seed"]);
        git(&origin, &["branch", "-M", "main"]);
        let commit = git(&origin, &["rev-parse", "HEAD"]);
        let backup = home.join(".dot-backup");
        let live = home.join(".dotfiles");
        let record = dir.path().join("record");
        Self {
            _dir: dir,
            home,
            backup,
            live,
            origin,
            record,
            commit,
            branch: "main".into(),
            identity: "github.com/a/b".into(),
            nonce: "n-1".into(),
        }
    }
    fn staged(&self) -> PathBuf {
        self.backup.join("git-stage/repo")
    }
    fn marker(&self) -> PathBuf {
        self.backup.join("git-stage/identity")
    }
    fn inputs(&self) -> GitStageInputs<'_> {
        GitStageInputs {
            record: &self.record,
            backup: &self.backup,
            git_dir: &self.live,
            origin: &self.origin,
            branch: &self.branch,
            commit: &self.commit,
            identity: &self.identity,
            nonce: &self.nonce,
            home: &self.home,
        }
    }
}

struct Deps {
    phases: RefCell<Vec<String>>,
    fail: Option<&'static str>,
}
impl Deps {
    fn new() -> Self {
        Self {
            phases: RefCell::new(vec![]),
            fail: None,
        }
    }
    fn failing(slot: &'static str) -> Self {
        Self {
            phases: RefCell::new(vec![]),
            fail: Some(slot),
        }
    }
    fn with<R>(&self, c: &Case, f: impl FnOnce(&GitStageDeps<'_>) -> R) -> R {
        let ensure = |p: &Path| {
            if self.fail == Some("private") || !dot::init_client_transaction::private_directory(p) {
                Err(Error::Usage {
                    message: "private refused",
                })
            } else {
                Ok(())
            }
        };
        let matches = |p: &Path| {
            generation::generation_matches(p, &c.branch, &c.nonce, &c.commit, &c.identity)
        };
        let modes = |p: &Path| {
            if self.fail == Some("modes") {
                Err(Error::Usage {
                    message: "modes refused",
                })
            } else {
                generation::configure_git_metadata_modes(p)
            }
        };
        let identity = |p: &Path| {
            if self.fail == Some("identity") {
                Err(Error::Usage {
                    message: "identity refused",
                })
            } else {
                generation::set_git_identity(p).map(|_| ())
            }
        };
        let marker = |p: &Path| {
            if self.fail == Some("marker") {
                Err(Error::Usage {
                    message: "marker refused",
                })
            } else {
                generation::write_generation_marker(
                    p,
                    &c.nonce,
                    &c.commit,
                    &c.identity,
                    &mut MoveCache::default(),
                )
            }
        };
        let move_path = |a: &Path, b: &Path| {
            if self.fail == Some("move") {
                Err(Error::Usage {
                    message: "move refused",
                })
            } else {
                temp::move_noreplace_cached(a, b, &mut MoveCache::default())
            }
        };
        let phase = |_: &Path, value: &str| -> Result<()> {
            if self.fail == Some("record") {
                return Err(Error::Usage {
                    message: "record refused",
                });
            }
            self.phases.borrow_mut().push(value.into());
            std::fs::write(&c.record, format!("phase={value}\n"))?;
            Ok(())
        };
        f(&GitStageDeps {
            ensure_private_dir: &ensure,
            generation_matches: &matches,
            configure_metadata_modes: &modes,
            set_git_identity: &identity,
            write_generation_marker: &marker,
            move_noreplace: &move_path,
            record_phase: &phase,
        })
    }
}
fn run_stage(c: &Case, d: &Deps) -> Result<()> {
    d.with(c, |deps| stage::stage_git(&c.inputs(), deps))
}
fn publish(c: &Case, d: &Deps) -> Result<()> {
    d.with(c, |deps| stage::publish_git(&c.inputs(), deps))
}
fn tip(p: &Path, branch: &str) -> String {
    git(p, &["rev-parse", &format!("refs/heads/{branch}")])
}
fn assert_stage(c: &Case) {
    assert_eq!(tip(&c.staged(), &c.branch), c.commit);
    assert_eq!(
        git(&c.staged(), &["config", "core.worktree"]),
        c.home.display().to_string()
    );
    assert_eq!(
        std::fs::read(c.staged().join("dot-init-generation-v1")).unwrap(),
        format!(
            "cgraf78 dot client generation v1\nnonce={}\ncommit={}\nidentity={}\n",
            c.nonce, c.commit, c.identity
        )
        .as_bytes()
    );
}

#[test]
fn stage_git_clones_fresh_stage() {
    let c = Case::new("fresh");
    let d = Deps::new();
    run_stage(&c, &d).unwrap();
    assert_stage(&c);
    assert_eq!(&*d.phases.borrow(), &["git-staging", "git-staged"]);
    assert_eq!(
        std::fs::read(c.marker()).unwrap(),
        format!(
            "cgraf78 dot Git stage v1\nnonce={}\ncommit={}\nidentity={}\n",
            c.nonce, c.commit, c.identity
        )
        .as_bytes()
    );
    assert_eq!(git(&c.staged(), &["config", "core.bare"]), "false");
}
#[test]
fn stage_git_reuses_live_git_dir() {
    let c = Case::new("live");
    let d = Deps::new();
    run_stage(&c, &d).unwrap();
    std::fs::rename(c.staged(), &c.live).unwrap();
    run_stage(&c, &d).unwrap();
    assert!(!c.staged().exists());
    assert_eq!(tip(&c.live, "main"), c.commit);
}
#[test]
fn stage_git_reclones_stale_repo() {
    let c = Case::new("stale");
    let d = Deps::new();
    run_stage(&c, &d).unwrap();
    git(&c.staged(), &["update-ref", "-d", "refs/heads/main"]);
    run_stage(&c, &d).unwrap();
    assert_stage(&c);
    std::fs::remove_dir_all(c.staged()).unwrap();
    run_stage(&c, &d).unwrap();
    assert_stage(&c);
}
#[test]
fn stage_git_refuses_bad_stage() {
    for shape in ["wrong-marker", "container-file", "marker-link"] {
        let c = Case::new(shape);
        match shape {
            "wrong-marker" => {
                std::fs::create_dir_all(c.marker().parent().unwrap()).unwrap();
                std::fs::write(c.marker(), b"wrong\n").unwrap();
            }
            "container-file" => {
                std::fs::create_dir_all(&c.backup).unwrap();
                std::fs::write(c.backup.join("git-stage"), b"x").unwrap();
            }
            _ => {
                std::fs::create_dir_all(c.marker().parent().unwrap()).unwrap();
                std::os::unix::fs::symlink("missing", c.marker()).unwrap();
            }
        }
        assert!(run_stage(&c, &Deps::new()).is_err(), "{shape}");
        assert!(!c.record.exists());
    }
    let c = Case::new("private");
    assert!(run_stage(&c, &Deps::failing("private")).is_err());
    assert!(!c.backup.exists());

    // Git failures remain errors and never publish a live checkout:
    // missing remote, missing branch, and unavailable locked commit.
    for kind in ["remote", "branch", "commit"] {
        let mut c = Case::new(kind);
        match kind {
            "remote" => c.origin = c._dir.path().join("missing-origin"),
            "branch" => c.branch = "missing-branch".into(),
            "commit" => c.commit = "0123456789012345678901234567890123456789".into(),
            _ => unreachable!(),
        }
        let error = run_stage(&c, &Deps::new()).unwrap_err();
        if kind == "commit" {
            assert!(matches!(
                error,
                Error::Usage {
                    message: "staged tip is not the locked commit"
                }
            ));
        } else {
            assert!(matches!(error, Error::Command { .. }), "{kind}: {error:?}");
        }
        assert!(!c.live.exists(), "{kind}");
        assert!(
            !c.record.exists() || std::fs::read(&c.record).unwrap() == b"phase=git-staging\n",
            "{kind}"
        );
    }
}
#[test]
fn stage_git_records_staging_on_late_failure() {
    for slot in ["modes", "identity", "record", "marker"] {
        let c = Case::new(slot);
        let d = Deps::failing(slot);
        assert!(run_stage(&c, &d).is_err(), "{slot}");
        if slot == "record" {
            assert!(!c.record.exists());
        } else if slot == "marker" {
            assert_eq!(&*d.phases.borrow(), &["git-staging"]);
            assert_eq!(tip(&c.staged(), "main"), c.commit);
        }
    }
}
#[test]
fn stage_git_marks_special_values() {
    let mut c = Case::new("special");
    c.identity = "id with spaces = and % 'quote'".into();
    c.nonce = "n c=1%2'3".into();
    run_stage(&c, &Deps::new()).unwrap();
    assert_stage(&c);
}
#[test]
fn stage_git_matches_adversarial_marker() {
    let mut c = Case::new("lines");
    c.nonce = "a\nb".into();
    run_stage(&c, &Deps::new()).unwrap();
    assert_stage(&c);
    let c = Case::new("nul");
    std::fs::create_dir_all(c.marker().parent().unwrap()).unwrap();
    let mut b = format!("cgraf78 dot Git stage v1\nnonce={}", c.nonce).into_bytes();
    b.extend_from_slice(b"\0junk\n");
    b.extend_from_slice(format!("commit={}\nidentity={}\n", c.commit, c.identity).as_bytes());
    std::fs::write(c.marker(), b).unwrap();
    run_stage(&c, &Deps::new()).unwrap();
    assert_stage(&c);
}
#[test]
fn publish_git_moves_staged_live() {
    let c = Case::new("move");
    let d = Deps::new();
    run_stage(&c, &d).unwrap();
    publish(&c, &d).unwrap();
    assert!(!c.staged().exists());
    assert_eq!(tip(&c.live, "main"), c.commit);
    assert_eq!(d.phases.borrow().last().unwrap(), "publishing");
    publish(&c, &d).unwrap();
}
#[test]
fn publish_git_uses_existing_live() {
    let c = Case::new("existing");
    let d = Deps::new();
    run_stage(&c, &d).unwrap();
    git(
        &c.home,
        &[
            "clone",
            "--quiet",
            "--bare",
            c.staged().to_str().unwrap(),
            c.live.to_str().unwrap(),
        ],
    );
    std::fs::write(
        c.live.join("dot-init-generation-v1"),
        std::fs::read(c.staged().join("dot-init-generation-v1")).unwrap(),
    )
    .unwrap();
    publish(&c, &d).unwrap();
    assert!(c.staged().exists());
    assert_eq!(tip(&c.live, "main"), c.commit);
}
#[test]
fn publish_git_refuses() {
    let c = Case::new("empty");
    assert!(publish(&c, &Deps::new()).is_err());
    assert!(!c.live.exists());
    let c = Case::new("move-fail");
    run_stage(&c, &Deps::new()).unwrap();
    assert!(publish(&c, &Deps::failing("move")).is_err());
    assert!(c.staged().exists());
    assert!(!c.live.exists());
    let c = Case::new("record-fail");
    run_stage(&c, &Deps::new()).unwrap();
    assert!(publish(&c, &Deps::failing("record")).is_err());
    assert!(c.live.exists());
    assert!(!c.staged().exists());
}
