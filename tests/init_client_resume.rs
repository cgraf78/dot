//! Native behavioral contracts for init transaction resumption.

use std::cell::RefCell;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::errors::{Error, Result};
use dot::init_client_resume as resume;
use dot_test_support::TempDir;

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "Dot Test")
        .env("GIT_AUTHOR_EMAIL", "dot@example.invalid")
        .env("GIT_COMMITTER_NAME", "Dot Test")
        .env("GIT_COMMITTER_EMAIL", "dot@example.invalid")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

struct Repo {
    _tmp: TempDir,
    home: PathBuf,
    git: PathBuf,
    origin: PathBuf,
    identity: String,
    dev: String,
    ino: String,
}
impl Repo {
    fn new(tag: &str, ordinary: bool) -> Self {
        Self::with_format(tag, ordinary, false)
    }

    fn with_format(tag: &str, ordinary: bool, sha256: bool) -> Self {
        let tmp = TempDir::new(tag).unwrap();
        let seed = tmp.path().join("seed");
        let origin = tmp.path().join("origin.git");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let mut init = vec!["init", "--quiet", "--initial-branch", "main"];
        if sha256 {
            init.push("--object-format=sha256");
        }
        git(&seed, &init);
        std::fs::write(seed.join("file"), b"data\n").unwrap();
        git(&seed, &["add", "file"]);
        git(&seed, &["commit", "--quiet", "-m", "initial"]);
        git(
            tmp.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let url = format!("file://{}", origin.display());
        let gitdir = if ordinary {
            git(&home, &["clone", "--quiet", &url, "."]);
            home.join(".git")
        } else {
            let p = home.join(".dotfiles");
            git(
                tmp.path(),
                &["clone", "--quiet", "--bare", &url, p.to_str().unwrap()],
            );
            p
        };
        let meta = std::fs::metadata(&gitdir).unwrap();
        let identity = dot::init_client_identity::repo_identity(&url).unwrap();
        Self {
            _tmp: tmp,
            home,
            git: gitdir,
            origin,
            identity,
            dev: meta.dev().to_string(),
            ino: meta.ino().to_string(),
        }
    }
    fn inputs<'a>(&'a self, nonce: &'a str) -> resume::LiveGitInputs<'a> {
        resume::LiveGitInputs {
            git_dir: &self.git,
            git_dev: &self.dev,
            git_ino: &self.ino,
            nonce,
            identity: &self.identity,
            branch: "main",
            home: &self.home,
        }
    }
}

fn matches(repo: &Repo, nonce: &str, generation: bool) -> bool {
    let commit = if repo.git.is_dir() {
        git(
            &repo.home,
            &["--git-dir", repo.git.to_str().unwrap(), "rev-parse", "HEAD"],
        )
    } else {
        String::new()
    };
    if generation {
        let marker = format!(
            "cgraf78 dot client generation v1\nnonce={nonce}\ncommit={commit}\nidentity={}\n",
            repo.identity
        );
        std::fs::write(repo.git.join("dot-init-generation-v1"), marker).unwrap();
    }
    let path = |p: &Path| dot::temp::path_identity(p).map(|(d, i)| format!("{d}:{i}"));
    let marker = |path: &Path| {
        dot::init_client_generation::generation_marker_matches(path, nonce, &commit, &repo.identity)
    };
    let identity = |u: &str| {
        dot::init_client_identity::repo_identity(u).ok_or(Error::Usage {
            message: "identity",
        })
    };
    resume::live_git_matches_record(
        &repo.inputs(nonce),
        &resume::LiveGitDeps {
            path_identity: &path,
            generation_matches: &marker,
            repo_identity: &identity,
        },
    )
}
fn set(repo: &Repo, args: &[&str]) {
    let mut all = vec!["--git-dir", repo.git.to_str().unwrap()];
    all.extend_from_slice(args);
    git(&repo.home, &all);
}

#[test]
fn pred_healthy_dotfiles() {
    assert!(matches(&Repo::new("healthy", false), "nonce", true));
}
#[test]
fn pred_bare_true() {
    let r = Repo::new("bare", false);
    assert_eq!(
        git(
            &r.home,
            &[
                "--git-dir",
                r.git.to_str().unwrap(),
                "config",
                "--bool",
                "core.bare"
            ]
        ),
        "true"
    );
    assert!(matches(&r, "adopted", false));
}
#[test]
fn pred_worktree_wrong() {
    let r = Repo::new("wrong-worktree", false);
    set(&r, &["config", "core.bare", "false"]);
    set(&r, &["config", "core.worktree", "/wrong"]);
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_bare_unset() {
    let r = Repo::new("bare-unset", false);
    set(&r, &["config", "--unset", "core.bare"]);
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_git_topology() {
    assert!(matches(&Repo::new("ordinary", true), "adopted", false));
}
#[test]
fn pred_two_origin_urls() {
    let r = Repo::new("two-origin", false);
    set(
        &r,
        &["config", "--add", "remote.origin.url", "file:///second"],
    );
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_zero_urls() {
    let r = Repo::new("zero-origin", false);
    set(&r, &["config", "--unset-all", "remote.origin.url"]);
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_wrong_identity() {
    let mut r = Repo::new("identity", false);
    r.identity = "file:///wrong".into();
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_wrong_branch() {
    let r = Repo::new("branch", false);
    let path = |p: &Path| dot::temp::path_identity(p).map(|(d, i)| format!("{d}:{i}"));
    let marker = |_: &Path| true;
    let identity = |u: &str| {
        dot::init_client_identity::repo_identity(u).ok_or(Error::Usage {
            message: "identity",
        })
    };
    let base = r.inputs("adopted");
    let input = resume::LiveGitInputs {
        branch: "other",
        ..base
    };
    assert!(!resume::live_git_matches_record(
        &input,
        &resume::LiveGitDeps {
            path_identity: &path,
            generation_matches: &marker,
            repo_identity: &identity
        }
    ));
}
#[test]
fn pred_detached_head() {
    let r = Repo::new("detached", false);
    let head = git(
        &r.home,
        &["--git-dir", r.git.to_str().unwrap(), "rev-parse", "HEAD"],
    );
    std::fs::write(r.git.join("HEAD"), format!("{head}\n")).unwrap();
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_ino_mismatch() {
    let mut r = Repo::new("inode", false);
    r.ino = (r.ino.parse::<u64>().unwrap() + 1).to_string();
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_missing_dir() {
    let r = Repo::new("missing", false);
    std::fs::remove_dir_all(&r.git).unwrap();
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_symlinked_dir() {
    let r = Repo::new("symlink", false);
    let real = r.home.join("real");
    std::fs::rename(&r.git, &real).unwrap();
    symlink(&real, &r.git).unwrap();
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_adopted_garbage_marker() {
    assert!(matches(&Repo::new("skip-marker", false), "adopted", false));
}
#[test]
fn pred_tampered_marker() {
    assert!(!matches(&Repo::new("bad-marker", false), "nonce", false));
}
#[test]
fn pred_foreign_dir() {
    let mut r = Repo::new("foreign", false);
    r.git = r.home.join("foreign.git");
    git(
        r._tmp.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            r.origin.to_str().unwrap(),
            r.git.to_str().unwrap(),
        ],
    );
    let m = std::fs::metadata(&r.git).unwrap();
    r.dev = m.dev().to_string();
    r.ino = m.ino().to_string();
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_bad_url() {
    let r = Repo::new("bad-url", false);
    set(&r, &["remote", "set-url", "origin", "::bad::"]);
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn pred_sha256_commit() {
    let r = Repo::with_format("commit", false, true);
    assert!(matches(&r, "adopted", false));
    assert_eq!(
        git(
            &r.home,
            &["--git-dir", r.git.to_str().unwrap(), "rev-parse", "HEAD"]
        )
        .len(),
        64
    );
}
#[test]
fn pred_toplevel_mismatch() {
    let mut r = Repo::new("top", true);
    r.home = r.home.join("sub");
    std::fs::create_dir(&r.home).unwrap();
    r.git = r.home.parent().unwrap().join(".git");
    assert!(!matches(&r, "adopted", false));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    None,
    Branch,
    Generation,
    RecordBacking,
    Move,
    RecordBacked,
    Stage,
    PublishGit,
    Worktree,
    RecordCheckout,
    Converge,
    RecordComplete,
    Completed,
}
struct Out {
    result: Result<()>,
    events: Vec<String>,
}
fn run(
    tag: &str,
    phase: &str,
    fail: Fail,
    setup: impl Fn(&Path, &Path),
) -> (TempDir, PathBuf, PathBuf, Out) {
    let repo = Repo::new(tag, false);
    let root = repo._tmp.path().to_path_buf();
    let tx = root.join("transaction");
    let backup = root.join("backup");
    std::fs::create_dir_all(&tx).unwrap();
    std::fs::create_dir_all(&backup).unwrap();
    for n in ["tree.tsv", "prior.tsv", "conflicts.tsv"] {
        std::fs::write(tx.join(n), b"journal\n").unwrap();
    }
    let record = tx.join("record");
    std::fs::write(&record, b"phase=prepared\n").unwrap();
    setup(&tx, &backup);
    let events = RefCell::new(Vec::new());
    let note = |s: &str| events.borrow_mut().push(s.into());
    let path = |p: &Path| dot::temp::path_identity(p).map(|(d, i)| format!("{d}:{i}"));
    let live_commit = git(
        &repo.home,
        &["--git-dir", repo.git.to_str().unwrap(), "rev-parse", "HEAD"],
    );
    let marker = |path: &Path| {
        dot::init_client_generation::generation_marker_matches(
            path,
            "nonce",
            &live_commit,
            &repo.identity,
        )
    };
    let identity = |u: &str| {
        dot::init_client_identity::repo_identity(u).ok_or(Error::Usage {
            message: "identity",
        })
    };
    let live = resume::LiveGitDeps {
        path_identity: &path,
        generation_matches: &marker,
        repo_identity: &identity,
    };
    let record_phase = |_: &Path, p: &str| {
        note(&format!("record:{p}"));
        let bad = matches!(
            (p, fail),
            ("backing-up", Fail::RecordBacking)
                | ("backed-up", Fail::RecordBacked)
                | ("checkout", Fail::RecordCheckout)
                | ("complete", Fail::RecordComplete)
        );
        if bad {
            Err(Error::Usage {
                message: "record failure",
            })
        } else {
            Ok(())
        }
    };
    let move_conflicts = |c: &Path, b: &Path| {
        note("move-conflicts");
        assert_eq!(c, tx.join("conflicts.tsv"));
        assert_eq!(b, backup);
        if fail == Fail::Move {
            Err(Error::Usage {
                message: "move failure",
            })
        } else {
            Ok(())
        }
    };
    let stage = |r: &Path| {
        note("stage-git");
        assert_eq!(r, record);
        if fail == Fail::Stage {
            Err(Error::Usage {
                message: "stage failure",
            })
        } else {
            Ok(())
        }
    };
    let publish_git = |_: &Path| {
        note("publish-git");
        if fail == Fail::PublishGit {
            Err(Error::Usage {
                message: "publish git failure",
            })
        } else {
            Ok(())
        }
    };
    let worktree = |t: &Path| {
        note("publish-worktree");
        assert_eq!(t, tx);
        if fail == Fail::Worktree {
            Err(Error::Usage {
                message: "worktree failure",
            })
        } else {
            Ok(())
        }
    };
    let converge = || {
        note("converge");
        if fail == Fail::Converge {
            Err(Error::Usage {
                message: "converge failure",
            })
        } else {
            Ok(())
        }
    };
    let completed = |r: &Path| {
        note("completed");
        if fail == Fail::Completed {
            Err(Error::Usage {
                message: "completion failure",
            })
        } else {
            std::fs::write(root.join("completed"), std::fs::read(r).unwrap()).map_err(|source| {
                Error::Io {
                    context: "complete",
                    source,
                }
            })
        }
    };
    let nonce = if fail == Fail::Generation {
        "nonce"
    } else {
        "adopted"
    };
    let base_git = repo.inputs(nonce);
    let git = resume::LiveGitInputs {
        branch: if fail == Fail::Branch {
            "other"
        } else {
            "main"
        },
        ..base_git
    };
    let input = resume::ResumeInputs {
        transaction: &tx,
        record: &record,
        phase,
        backup: &backup,
        nonce,
        git,
    };
    let result = resume::resume_transaction(
        &input,
        &resume::ResumeDeps {
            live: &live,
            record_phase: &record_phase,
            move_conflicts: &move_conflicts,
            stage_git: &stage,
            publish_git: &publish_git,
            publish_worktree: &worktree,
            forward_converge: &converge,
            publish_completed: &completed,
        },
    );
    (
        repo._tmp,
        tx,
        backup,
        Out {
            result,
            events: events.into_inner(),
        },
    )
}
fn all() -> Vec<String> {
    [
        "record:backing-up",
        "move-conflicts",
        "record:backed-up",
        "stage-git",
        "publish-git",
        "publish-worktree",
        "record:checkout",
        "record:converging",
        "converge",
        "record:complete",
        "completed",
    ]
    .map(str::to_string)
    .to_vec()
}

#[test]
fn resume_prepared_healthy() {
    let (d, t, _, o) = run("prepared", "prepared", Fail::None, |_, _| {});
    assert!(o.result.is_ok());
    assert_eq!(o.events, all());
    assert!(!t.exists());
    assert_eq!(
        std::fs::read(d.path().join("completed")).unwrap(),
        b"phase=prepared\n"
    );
}
#[test]
fn resume_checkout_healthy() {
    let (_dir, t, _, o) = run("checkout", "checkout", Fail::None, |_, _| {});
    assert!(o.result.is_ok());
    assert_eq!(
        o.events,
        [
            "record:converging",
            "converge",
            "record:complete",
            "completed"
        ]
    );
    assert!(!t.exists());
}
#[test]
fn resume_complete_healthy() {
    let (d, t, _, o) = run("complete", "complete", Fail::None, |_, _| {});
    assert!(o.result.is_ok());
    assert_eq!(o.events, ["completed"]);
    assert!(!t.exists());
    assert_eq!(
        std::fs::read(d.path().join("completed")).unwrap(),
        b"phase=prepared\n"
    );
}
#[test]
fn resume_bogus_phase() {
    let (_dir, t, _, o) = run("bogus", "bogus", Fail::None, |_, _| {});
    assert!(matches!(
        o.result,
        Err(Error::Usage {
            message: "unknown resume phase"
        })
    ));
    assert!(o.events.is_empty());
    assert!(t.exists());
}
#[test]
fn resume_missing_prior() {
    let (_dir, t, _, o) = run("prior", "prepared", Fail::None, |t, _| {
        std::fs::remove_file(t.join("prior.tsv")).unwrap()
    });
    assert!(matches!(
        o.result,
        Err(Error::Usage {
            message: "transaction journals are missing"
        })
    ));
    assert!(o.events.is_empty());
    assert!(t.exists());
}
#[test]
fn resume_checkout_branch_mismatch() {
    let (_dir, t, _, o) = run("checkout-fail", "checkout", Fail::Branch, |_, _| {});
    assert!(matches!(
        o.result,
        Err(Error::Usage {
            message: "live git does not match record"
        })
    ));
    assert!(o.events.is_empty());
    assert!(t.exists());
}
#[test]
fn resume_backedup_reuses_stage() {
    let (_dir, t, b, o) = run("reuse", "backed-up", Fail::None, |_, b| {
        let s = b.join("git-stage");
        std::fs::create_dir(&s).unwrap();
        std::fs::write(s.join("identity"), b"nonce=adopted\n").unwrap();
    });
    assert!(o.result.is_ok());
    assert_eq!(o.events, all());
    assert!(!b.join("git-stage").exists());
    assert!(!t.exists());
}
#[test]
fn resume_backedup_bad_stage_marker() {
    let (_dir, t, b, o) = run("bad-stage", "backed-up", Fail::None, |_, b| {
        let s = b.join("git-stage");
        std::fs::create_dir(&s).unwrap();
        std::fs::write(s.join("identity"), b"nonce=other\n").unwrap();
    });
    assert!(matches!(
        o.result,
        Err(Error::Usage {
            message: "staged git identity changed"
        })
    ));
    assert_eq!(o.events, &all()[..7]);
    assert!(b.join("git-stage").exists());
    assert!(t.exists());
}
#[test]
fn resume_checkout_adopted() {
    let (_dir, t, _, o) = run("adopted", "checkout", Fail::None, |_, _| {});
    assert!(o.result.is_ok());
    assert_eq!(
        o.events,
        [
            "record:converging",
            "converge",
            "record:complete",
            "completed"
        ]
    );
    assert!(!t.exists());
}
#[test]
fn resume_checkout_missing_marker() {
    let (_dir, t, _, o) = run("missing-marker", "checkout", Fail::Generation, |_, _| {});
    assert!(matches!(
        o.result,
        Err(Error::Usage {
            message: "live git does not match record"
        })
    ));
    assert!(o.events.is_empty());
    assert!(t.exists());
}
#[test]
fn resume_publishing_healthy() {
    let (_dir, t, _, o) = run("publishing", "publishing", Fail::None, |_, _| {});
    assert!(o.result.is_ok());
    assert_eq!(o.events, all());
    assert!(!t.exists());
}
#[test]
fn resume_backingup_missing_tree() {
    let (_dir, t, _, o) = run("tree", "backing-up", Fail::None, |t, _| {
        std::fs::remove_file(t.join("tree.tsv")).unwrap()
    });
    assert!(matches!(
        o.result,
        Err(Error::Usage {
            message: "transaction journals are missing"
        })
    ));
    assert!(o.events.is_empty());
    assert!(t.exists());
}
#[test]
fn resume_record_is_dir() {
    let (_dir, t, _, o) = run("record-dir", "prepared", Fail::RecordBacking, |t, _| {
        std::fs::remove_file(t.join("record")).unwrap();
        std::fs::create_dir(t.join("record")).unwrap();
    });
    assert!(o.result.is_err());
    assert_eq!(o.events, ["record:backing-up"]);
    assert!(t.exists());
}
#[test]
fn resume_bogus_origin() {
    let r = Repo::new("bogus-origin", false);
    set(&r, &["remote", "set-url", "origin", "::bad::"]);
    assert!(!matches(&r, "adopted", false));
}
#[test]
fn resume_symlinked_journals() {
    let (_dir, t, _, o) = run("links", "prepared", Fail::None, |t, _| {
        let p = t.join("real");
        std::fs::write(&p, b"prior\n").unwrap();
        std::fs::remove_file(t.join("prior.tsv")).unwrap();
        symlink(p, t.join("prior.tsv")).unwrap();
    });
    assert!(o.result.is_ok());
    assert_eq!(o.events, all());
    assert!(!t.exists());
}
#[test]
fn resume_converging_healthy() {
    let (_dir, t, _, o) = run("converging", "converging", Fail::None, |_, _| {});
    assert!(o.result.is_ok());
    assert_eq!(
        o.events,
        [
            "record:converging",
            "converge",
            "record:complete",
            "completed"
        ]
    );
    assert!(!t.exists());
}
#[test]
fn resume_prepared_prebuilt() {
    for (fail, n) in [
        (Fail::RecordBacking, 1),
        (Fail::Move, 2),
        (Fail::RecordBacked, 3),
        (Fail::Stage, 4),
        (Fail::PublishGit, 5),
        (Fail::Worktree, 6),
        (Fail::RecordCheckout, 7),
        (Fail::Converge, 9),
        (Fail::RecordComplete, 10),
        (Fail::Completed, 11),
    ] {
        let (_dir, t, _, o) = run("failure", "prepared", fail, |_, _| {});
        assert!(o.result.is_err());
        assert_eq!(o.events.len(), n);
        assert!(t.exists());
    }
}
