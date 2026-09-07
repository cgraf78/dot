//! Native contract tests for init publication orchestration.

use std::cell::RefCell;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_publish as publish;
use dot_test_support::TempDir;

const NONCE: &str = "7.native";

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

fn write(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn identity(path: &Path) -> String {
    let meta = std::fs::metadata(path).expect("stat fixture");
    format!("{}:{}", meta.dev(), meta.ino())
}

fn private_dir(path: &Path, expected: &str, mode: &str) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_dir()
        && !meta.file_type().is_symlink()
        && identity(path) == expected
        && format!("{:o}", meta.permissions().mode() & 0o777) == mode
}

fn only_next(stage: &Path) -> bool {
    let Ok(items) = std::fs::read_dir(stage) else {
        return false;
    };
    items.filter_map(Result::ok).all(|item| {
        matches!(
            item.file_name().to_str(),
            Some("next" | ".dot-init-stage-claim-v1")
        )
    })
}

fn claim_body(path: &str) -> Vec<u8> {
    format!("cgraf78 dot publication stage claim v1\nkind=entry\nnonce={NONCE}\npath={path}\n")
        .into_bytes()
}

fn claim_matches(stage: &Path, kind: &str, path: &str) -> bool {
    kind == "entry"
        && std::fs::read(stage.join(".dot-init-stage-claim-v1"))
            .is_ok_and(|body| body == claim_body(path))
}

fn empty_dir(path: &Path, expected: &str, mode: &str) -> bool {
    private_dir(path, expected, mode)
        && std::fs::read_dir(path).is_ok_and(|mut items| items.next().is_none())
}

fn remove_claim(stage: &Path, kind: &str, path: &str) -> dot::Result<()> {
    if !claim_matches(stage, kind, path) {
        return Err(dot::errors::Error::Usage {
            message: "claim mismatch",
        });
    }
    std::fs::remove_file(stage.join(".dot-init-stage-claim-v1")).map_err(|source| {
        dot::errors::Error::Io {
            context: "remove test claim",
            source,
        }
    })
}

fn with_stage_hooks<T>(run: impl FnOnce(&publish::StageHooks<'_>) -> T) -> T {
    let a = |path: &Path, expected: &str, mode: &str| private_dir(path, expected, mode);
    let b = |stage: &Path| only_next(stage);
    let c = |stage: &Path, kind: &str, path: &str| claim_matches(stage, kind, path);
    let d = |path: &Path, expected: &str, mode: &str| empty_dir(path, expected, mode);
    let e = |stage: &Path, kind: &str, path: &str| remove_claim(stage, kind, path);
    run(&publish::StageHooks {
        private_directory_matches: &a,
        stage_only_next: &b,
        stage_claim_matches: &c,
        private_empty_directory_matches: &d,
        stage_claim_remove: &e,
    })
}

fn stage(root: &Path, name: &str, claim: Option<&str>) -> (PathBuf, String) {
    let path = root.join(name);
    std::fs::create_dir(&path).expect("stage");
    chmod(&path, 0o700);
    if let Some(entry) = claim {
        std::fs::write(path.join(".dot-init-stage-claim-v1"), claim_body(entry)).expect("claim");
    }
    let id = identity(&path);
    (path, id)
}

#[test]
fn stage_matches_claim_and_empty_states() {
    let dir = TempDir::new("publish-stage-ok").expect("temp");
    let (claimed, claimed_id) = stage(dir.path(), "claimed", Some("a/b"));
    let (empty, empty_id) = stage(dir.path(), "empty", None);
    with_stage_hooks(|hooks| {
        assert!(publish::published_stage_matches(
            &claimed,
            &claimed_id,
            "a/b",
            hooks
        ));
        assert!(publish::published_stage_matches(
            &empty, &empty_id, "a/b", hooks
        ));
    });
}

#[test]
fn stage_matches_refusal_states() {
    let dir = TempDir::new("publish-stage-no").expect("temp");
    let (wrong_id, id) = stage(dir.path(), "wrong-id", None);
    let (bad_mode, bad_mode_id) = stage(dir.path(), "bad-mode", None);
    chmod(&bad_mode, 0o755);
    let (next, next_id) = stage(dir.path(), "next", None);
    write(&next, "next", b"candidate");
    let (extra, extra_id) = stage(dir.path(), "extra", None);
    write(&extra, "stray", b"x");
    let (bad_claim, bad_claim_id) = stage(dir.path(), "claim", Some("other"));
    with_stage_hooks(|hooks| {
        assert!(!publish::published_stage_matches(
            &dir.path().join("missing"),
            "0:0",
            "a/b",
            hooks
        ));
        assert!(!publish::published_stage_matches(
            &wrong_id, "0:0", "a/b", hooks
        ));
        assert_ne!(id, "0:0");
        assert!(!publish::published_stage_matches(
            &bad_mode,
            &bad_mode_id,
            "a/b",
            hooks
        ));
        assert!(!publish::published_stage_matches(
            &next, &next_id, "a/b", hooks
        ));
        assert!(!publish::published_stage_matches(
            &extra, &extra_id, "a/b", hooks
        ));
        assert!(!publish::published_stage_matches(
            &bad_claim,
            &bad_claim_id,
            "a/b",
            hooks
        ));
    });
}

fn intent(
    phase: &str,
    stage: &Path,
    stage_id: &str,
    target_id: &str,
    home: &Path,
) -> publish::IntentRecord {
    let rel = stage
        .strip_prefix(home)
        .expect("relative")
        .to_string_lossy()
        .into_owned();
    let (dev, ino) = stage_id.split_once(':').unwrap_or(("0", "0"));
    let (next_dev, next_ino) = target_id.split_once(':').unwrap_or(("0", "0"));
    publish::IntentRecord {
        phase: phase.into(),
        stage: rel,
        dev: dev.into(),
        ino: ino.into(),
        next_dev: next_dev.into(),
        next_ino: next_ino.into(),
    }
}

#[test]
fn intent_matches_prepared_states() {
    let dir = TempDir::new("publish-intent-ok").expect("temp");
    let home = dir.path();
    let target = write(home, "a/b", b"published");
    let target_id = identity(&target);
    let (live_stage, stage_id) = stage(home, "stage-live", Some("a/b"));
    let record = intent("prepared", &live_stage, &stage_id, &target_id, home);
    let read = |_: &Path, _: &str, _: &str, _: &str| {
        Ok(publish::IntentRecord {
            phase: record.phase.clone(),
            stage: record.stage.clone(),
            dev: record.dev.clone(),
            ino: record.ino.clone(),
            next_dev: record.next_dev.clone(),
            next_ino: record.next_ino.clone(),
        })
    };
    with_stage_hooks(|hooks| {
        let reply = publish::published_intent_matches(
            &home.join("intent"),
            "100644",
            "oid",
            "a/b",
            home,
            &read,
            hooks,
        )
        .expect("prepared");
        assert_eq!(
            reply,
            format!("{}\t{stage_id}", live_stage.display()).as_bytes()
        );
    });
    let consumed = home.join("consumed");
    let consumed_record = intent("prepared", &consumed, "1:2", &target_id, home);
    let read = |_: &Path, _: &str, _: &str, _: &str| {
        Ok(publish::IntentRecord {
            phase: consumed_record.phase.clone(),
            stage: consumed_record.stage.clone(),
            dev: consumed_record.dev.clone(),
            ino: consumed_record.ino.clone(),
            next_dev: consumed_record.next_dev.clone(),
            next_ino: consumed_record.next_ino.clone(),
        })
    };
    with_stage_hooks(|hooks| {
        assert!(
            publish::published_intent_matches(
                &home.join("intent"),
                "100644",
                "oid",
                "a/b",
                home,
                &read,
                hooks
            )
            .is_ok()
        )
    });
}

#[test]
fn intent_matches_refusal_states() {
    let dir = TempDir::new("publish-intent-no").expect("temp");
    let home = dir.path();
    let target = write(home, "a/b", b"published");
    let target_id = identity(&target);
    let (stage_path, stage_id) = stage(home, "stage", Some("a/b"));
    for (phase, next_id) in [
        ("pending", target_id.as_str()),
        ("staged", target_id.as_str()),
        ("prepared", "0:0"),
    ] {
        let record = intent(phase, &stage_path, &stage_id, next_id, home);
        let read = |_: &Path, _: &str, _: &str, _: &str| {
            Ok(publish::IntentRecord {
                phase: record.phase.clone(),
                stage: record.stage.clone(),
                dev: record.dev.clone(),
                ino: record.ino.clone(),
                next_dev: record.next_dev.clone(),
                next_ino: record.next_ino.clone(),
            })
        };
        with_stage_hooks(|hooks| {
            assert!(
                publish::published_intent_matches(
                    &home.join("intent"),
                    "100644",
                    "oid",
                    "a/b",
                    home,
                    &read,
                    hooks
                )
                .is_err(),
                "{phase}"
            )
        });
    }
    write(&stage_path, "next", b"stray");
    let record = intent("prepared", &stage_path, &stage_id, &target_id, home);
    let read = |_: &Path, _: &str, _: &str, _: &str| {
        Ok(publish::IntentRecord {
            phase: record.phase.clone(),
            stage: record.stage.clone(),
            dev: record.dev.clone(),
            ino: record.ino.clone(),
            next_dev: record.next_dev.clone(),
            next_ino: record.next_ino.clone(),
        })
    };
    with_stage_hooks(|hooks| {
        assert!(
            publish::published_intent_matches(
                &home.join("intent"),
                "100644",
                "oid",
                "a/b",
                home,
                &read,
                hooks
            )
            .is_err()
        )
    });
}

#[test]
fn cleanup_published_stage_rows() {
    let dir = TempDir::new("publish-cleanup").expect("temp");
    with_stage_hooks(|hooks| {
        assert!(
            publish::cleanup_published_stage(&dir.path().join("missing"), "0:0", "a/b", hooks)
                .is_ok()
        )
    });
    let (claimed, id) = stage(dir.path(), "claimed", Some("a/b"));
    with_stage_hooks(|hooks| {
        publish::cleanup_published_stage(&claimed, &id, "a/b", hooks).expect("cleanup")
    });
    assert!(!claimed.exists());
    let (empty, id) = stage(dir.path(), "empty", None);
    with_stage_hooks(|hooks| {
        publish::cleanup_published_stage(&empty, &id, "a/b", hooks).expect("cleanup")
    });
    assert!(!empty.exists());
    let (dirty, id) = stage(dir.path(), "dirty", None);
    write(&dirty, "next", b"x");
    with_stage_hooks(|hooks| {
        assert!(publish::cleanup_published_stage(&dirty, &id, "a/b", hooks).is_err())
    });
    assert!(dirty.exists());
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct Worktree {
    _dir: TempDir,
    home: PathBuf,
    tx: PathBuf,
    backup: PathBuf,
    repo: PathBuf,
    git_dir: PathBuf,
    commit: String,
}
fn worktree(tag: &str) -> Worktree {
    let dir = TempDir::new(tag).expect("temp");
    let repo = dir.path().join("repo");
    let home = dir.path().join("home");
    let tx = dir.path().join("tx");
    let backup = dir.path().join("backup");
    for path in [&repo, &home, &tx, &backup] {
        std::fs::create_dir(path).expect("dir");
    }
    git(&repo, &["init", "-q"]);
    write(&repo, "a.txt", b"A\n");
    write(&repo, "nested/b.txt", b"B\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let git_dir = repo.join(".git");
    Worktree {
        _dir: dir,
        home,
        tx,
        backup,
        repo,
        git_dir,
        commit,
    }
}

fn absent() -> publish::PriorRecord {
    publish::PriorRecord {
        kind: "absent".into(),
        dev: "-".into(),
        ino: "-".into(),
        mode: "-".into(),
        size: "-".into(),
        value: "-".into(),
    }
}

fn run_publish(
    world: &Worktree,
    prior: &publish::PriorRecord,
    candidate_ok: bool,
    state_ok: bool,
    prepared: Option<&publish::IntentRecord>,
    calls: &RefCell<Vec<String>>,
) -> dot::Result<()> {
    let prior_fn = |_: &Path, _: &str| {
        Ok(publish::PriorRecord {
            kind: prior.kind.clone(),
            dev: prior.dev.clone(),
            ino: prior.ino.clone(),
            mode: prior.mode.clone(),
            size: prior.size.clone(),
            value: prior.value.clone(),
        })
    };
    let candidate = |_: &str, _: &str, _: &str| candidate_ok;
    let state = |_: &Path, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str| state_ok;
    let publish_intent = |_: &Path, mode: &str, oid: &str, path: &str| {
        calls
            .borrow_mut()
            .push(format!("intent {mode} {oid} {path}"));
        Ok(())
    };
    let publish_one = |_: &Path, _: &Path, mode: &str, oid: &str, path: &str| {
        calls.borrow_mut().push(format!("one {mode} {oid} {path}"));
        let spec = format!("{}:{path}", world.commit);
        let bytes = Command::new("git")
            .arg("--git-dir")
            .arg(&world.git_dir)
            .args(["show", &spec])
            .output()
            .expect("git show");
        assert!(bytes.status.success());
        write(&world.home, path, &bytes.stdout);
        Ok(())
    };
    let entry_intent = |_: &Path, _: &str, _: &str, _: &str| match prepared {
        Some(record) => Ok(publish::IntentRecord {
            phase: record.phase.clone(),
            stage: record.stage.clone(),
            dev: record.dev.clone(),
            ino: record.ino.clone(),
            next_dev: record.next_dev.clone(),
            next_ino: record.next_ino.clone(),
        }),
        None => Err(dot::errors::Error::Usage {
            message: "no prepared intent",
        }),
    };
    let a = |_: &Path, _: &str, _: &str| false;
    let b = |_: &Path| false;
    let c = |_: &Path, _: &str, _: &str| false;
    let d = |_: &Path, _: &str, _: &str| false;
    let e = |_: &Path, _: &str, _: &str| {
        Err(dot::errors::Error::Usage {
            message: "no stage",
        })
    };
    let hooks = publish::PublishHooks {
        prior_record: &prior_fn,
        candidate_matches_git: &candidate,
        path_state_matches: &state,
        publish_intent: &publish_intent,
        publish_one: &publish_one,
        entry_intent: &entry_intent,
        stages: publish::StageHooks {
            private_directory_matches: &a,
            stage_only_next: &b,
            stage_claim_matches: &c,
            private_empty_directory_matches: &d,
            stage_claim_remove: &e,
        },
    };
    let binding = publish::PublishGit {
        git_dir: &world.git_dir,
        commit: &world.commit,
        branch: "publish-test",
        work_dir: &world.home,
    };
    publish::publish_worktree(&world.tx, &world.home, &world.backup, &binding, &hooks)
}

#[test]
fn publish_worktree_end_to_end() {
    let world = worktree("publish-worktree");
    let a_oid = git(&world.repo, &["rev-parse", "HEAD:a.txt"]);
    let b_oid = git(&world.repo, &["rev-parse", "HEAD:nested/b.txt"]);
    std::fs::write(
        world.tx.join("tree.tsv"),
        format!("100644\t{a_oid}\ta.txt\n100644\t{b_oid}\tnested/b.txt\n"),
    )
    .expect("tree");
    std::fs::write(world.tx.join("prior.tsv"), b"fixture\n").expect("prior");
    let calls = RefCell::new(Vec::new());
    run_publish(&world, &absent(), false, false, None, &calls).expect("publish");
    assert_eq!(std::fs::read(world.home.join("a.txt")).expect("a"), b"A\n");
    assert_eq!(
        std::fs::read(world.home.join("nested/b.txt")).expect("b"),
        b"B\n"
    );
    assert_eq!(calls.borrow().len(), 4);
    assert_eq!(
        git(&world.repo, &["rev-parse", "refs/heads/publish-test"]),
        world.commit
    );
    assert_eq!(
        git(&world.repo, &["symbolic-ref", "HEAD"]),
        "refs/heads/publish-test"
    );

    let unchanged = worktree("publish-unchanged");
    let oid = git(&unchanged.repo, &["rev-parse", "HEAD:a.txt"]);
    std::fs::write(
        unchanged.tx.join("tree.tsv"),
        format!("100644\t{oid}\ta.txt\n"),
    )
    .expect("tree");
    std::fs::write(unchanged.tx.join("prior.tsv"), b"fixture\n").expect("prior");
    write(&unchanged.home, "a.txt", b"A\n");
    let calls = RefCell::new(Vec::new());
    run_publish(&unchanged, &absent(), true, true, None, &calls).expect("unchanged skip");
    assert!(calls.borrow().is_empty());

    let recovered = worktree("publish-recovered");
    let oid = git(&recovered.repo, &["rev-parse", "HEAD:a.txt"]);
    std::fs::write(
        recovered.tx.join("tree.tsv"),
        format!("100644\t{oid}\ta.txt\n"),
    )
    .expect("tree");
    std::fs::write(recovered.tx.join("prior.tsv"), b"fixture\n").expect("prior");
    let target = write(&recovered.home, "a.txt", b"A\n");
    let target_id = identity(&target);
    let consumed = recovered.home.join("consumed-stage");
    let record = intent("prepared", &consumed, "1:2", &target_id, &recovered.home);
    let calls = RefCell::new(Vec::new());
    run_publish(&recovered, &absent(), true, false, Some(&record), &calls)
        .expect("prepared recovery");
    assert!(calls.borrow().is_empty());

    let lineage = worktree("publish-lineage-ok");
    let oid = git(&lineage.repo, &["rev-parse", "HEAD:a.txt"]);
    std::fs::write(
        lineage.tx.join("tree.tsv"),
        format!("100644\t{oid}\ta.txt\n"),
    )
    .expect("tree");
    std::fs::write(lineage.tx.join("prior.tsv"), b"fixture\n").expect("prior");
    std::fs::write(
        lineage.tx.join("conflicts.tsv"),
        b"a.txt\tregular\t1\t2\t644\t2\toid\n",
    )
    .expect("conflicts");
    write(&lineage.backup, "a.txt", b"old");
    let regular = publish::PriorRecord {
        kind: "regular".into(),
        dev: "1".into(),
        ino: "2".into(),
        mode: "644".into(),
        size: "2".into(),
        value: "oid".into(),
    };
    let calls = RefCell::new(Vec::new());
    run_publish(&lineage, &regular, false, true, None, &calls).expect("lineage publish");
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn publish_worktree_refusals() {
    let missing = worktree("publish-missing");
    let calls = RefCell::new(Vec::new());
    assert!(run_publish(&missing, &absent(), false, false, None, &calls).is_err());
    let occupied = worktree("publish-occupied");
    std::fs::write(occupied.tx.join("tree.tsv"), b"100644\toid\ta.txt\n").expect("tree");
    std::fs::write(occupied.tx.join("prior.tsv"), b"x\n").expect("prior");
    write(&occupied.home, "a.txt", b"occupied");
    assert!(
        run_publish(
            &occupied,
            &absent(),
            false,
            false,
            None,
            &RefCell::new(Vec::new())
        )
        .is_err()
    );
    let lineage = worktree("publish-lineage");
    std::fs::write(lineage.tx.join("tree.tsv"), b"100644\toid\ta.txt\n").expect("tree");
    std::fs::write(lineage.tx.join("prior.tsv"), b"x\n").expect("prior");
    let regular = publish::PriorRecord {
        kind: "regular".into(),
        dev: "1".into(),
        ino: "2".into(),
        mode: "644".into(),
        size: "1".into(),
        value: "oid".into(),
    };
    assert!(
        run_publish(
            &lineage,
            &regular,
            false,
            false,
            None,
            &RefCell::new(Vec::new())
        )
        .is_err()
    );
    let drift = worktree("publish-drift");
    std::fs::write(drift.tx.join("tree.tsv"), b"100644\toid\ta.txt\n").expect("tree");
    std::fs::write(drift.tx.join("prior.tsv"), b"x\n").expect("prior");
    write(&drift.home, "a.txt", b"candidate");
    assert!(
        run_publish(
            &drift,
            &absent(),
            true,
            false,
            None,
            &RefCell::new(Vec::new())
        )
        .is_err()
    );
}

#[test]
fn forward_converge_rows() {
    for (skip, select_ok, config_ok, sync_ok, finalize_ok, want_ok, want_calls) in [
        (false, true, true, true, true, true, 5),
        (true, true, true, true, true, true, 5),
        (false, true, true, false, true, true, 5),
        (true, true, true, false, false, false, 5),
        (false, false, true, true, true, true, 5),
        (false, true, false, true, true, false, 2),
    ] {
        let log = RefCell::new(Vec::new());
        let ok = |pass| {
            if pass {
                Ok(())
            } else {
                Err(dot::errors::Error::Usage {
                    message: "scripted",
                })
            }
        };
        let hooks = publish::ConvergeHooks {
            select_client: &|| {
                log.borrow_mut().push("select".into());
                ok(select_ok)
            },
            load_config: &|| {
                log.borrow_mut().push("config".into());
                ok(config_ok)
            },
            begin_ui: &|total| log.borrow_mut().push(format!("begin:{total}")),
            sync_repos: &|seen| {
                log.borrow_mut().push(format!("sync:{seen}"));
                ok(sync_ok)
            },
            finalize: &|status, seen| {
                log.borrow_mut().push(format!("finalize:{status}:{seen}"));
                ok(finalize_ok)
            },
        };
        assert_eq!(publish::forward_converge(skip, &hooks).is_ok(), want_ok);
        assert_eq!(log.borrow().len(), want_calls);
        if config_ok {
            assert_eq!(log.borrow()[2], "begin:5");
            assert_eq!(log.borrow()[3], format!("sync:{skip}"));
            assert_eq!(
                log.borrow()[4],
                format!("finalize:{}:{skip}", i32::from(!sync_ok))
            );
        }
    }
}

fn origin_repo(root: &Path, tag: &str, urls: &[&str]) -> PathBuf {
    let repo = root.join(tag);
    std::fs::create_dir(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    for url in urls {
        git(&repo, &["config", "--add", "remote.origin.url", url]);
    }
    repo
}

#[test]
fn single_origin_ordinary_rows() {
    let dir = TempDir::new("origin-ordinary").expect("temp");
    let one = origin_repo(dir.path(), "one", &["https://example.test/dot.git"]);
    assert_eq!(
        publish::single_origin(&publish::OriginScope::Ordinary, &one).expect("one"),
        b"https://example.test/dot.git\n"
    );
    let none = origin_repo(dir.path(), "none", &[]);
    assert!(publish::single_origin(&publish::OriginScope::Ordinary, &none).is_err());
    let plain = dir.path().join("plain");
    std::fs::create_dir(&plain).expect("plain");
    assert!(publish::single_origin(&publish::OriginScope::Ordinary, &plain).is_err());
    let many = origin_repo(dir.path(), "many", &["a", "b"]);
    assert!(publish::single_origin(&publish::OriginScope::Ordinary, &many).is_err());
}

#[test]
fn single_origin_separate_rows() {
    let dir = TempDir::new("origin-separate").expect("temp");
    let one = origin_repo(dir.path(), "one", &["ssh://example/dot"]);
    let git_dir = one.join(".git");
    assert_eq!(
        publish::single_origin(&publish::OriginScope::Separate { git_dir: &git_dir }, &one)
            .expect("one"),
        b"ssh://example/dot\n"
    );
    let many = origin_repo(dir.path(), "many", &["a", "b"]);
    let many_git = many.join(".git");
    assert!(
        publish::single_origin(
            &publish::OriginScope::Separate { git_dir: &many_git },
            &many
        )
        .is_err()
    );
}

#[test]
fn single_origin_unsafe_url_refuses() {
    let dir = TempDir::new("origin-unsafe").expect("temp");
    let repo = origin_repo(dir.path(), "repo", &["safe"]);
    git(&repo, &["config", "remote.origin.url", "a\tb"]);
    assert!(publish::single_origin(&publish::OriginScope::Ordinary, &repo).is_err());
}
