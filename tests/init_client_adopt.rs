//! Native contracts for legacy-client adoption and initialization status.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_adopt as adopt;
use dot::repos_base::Topology;
use dot_test_support::TempDir;

fn git(args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

fn git_out(args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout)
        .expect("git UTF-8")
        .trim()
        .to_string()
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parent");
    }
    std::fs::write(path, bytes).expect("fixture write");
}

fn field<'a>(record: &'a [u8], key: &[u8]) -> &'a [u8] {
    record
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(key))
        .expect("record field")
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("record metadata")
        .permissions()
        .mode()
        & 0o777
}

struct Origin {
    path: PathBuf,
    commit: String,
}

fn origin(root: &Path) -> Origin {
    let seed = root.join("seed");
    let bare = root.join("origin.git");
    git(&[
        "init",
        "--quiet",
        "--initial-branch",
        "main",
        seed.to_str().expect("seed"),
    ]);
    write(&seed, ".testrc", b"hello\n");
    git(&["-C", seed.to_str().expect("seed"), "add", ".testrc"]);
    git(&[
        "-C",
        seed.to_str().expect("seed"),
        "commit",
        "--quiet",
        "-m",
        "seed",
    ]);
    git(&[
        "clone",
        "--quiet",
        "--bare",
        seed.to_str().expect("seed"),
        bare.to_str().expect("origin"),
    ]);
    git(&[
        "-C",
        bare.to_str().expect("origin"),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let commit = git_out(&["-C", bare.to_str().expect("origin"), "rev-parse", "HEAD"]);
    Origin { path: bare, commit }
}

fn url(origin: &Origin) -> String {
    format!("file://{}", origin.path.display())
}

fn plant_separate(home: &Path, origin: &Origin) {
    git(&[
        "clone",
        "--quiet",
        "--bare",
        origin.path.to_str().expect("origin"),
        home.join(".dotfiles").to_str().expect("git dir"),
    ]);
    write(home, ".testrc", b"hello\n");
}

fn plant_ordinary(home: &Path, origin: &Origin) {
    let home = home.to_str().expect("home");
    git(&["-C", home, "init", "--quiet", "--initial-branch", "main"]);
    git(&["-C", home, "remote", "add", "origin", &url(origin)]);
    git(&["-C", home, "fetch", "--quiet", "origin", "main"]);
    git(&["-C", home, "checkout", "--quiet", "main"]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    None,
    TransactionDir,
    Prepare,
    WriteConverging,
    PublishTransaction,
    Converge,
    WriteComplete,
    PublishCompleted,
}

struct Run {
    result: Result<adopt::Adopted, adopt::AdoptError>,
    phases: Vec<String>,
    converges: Vec<(Topology, PathBuf)>,
}

fn run_adopt(
    home: &Path,
    state: &Path,
    selected: Topology,
    requested_origin: &str,
    identity: &str,
    branch: &str,
    fail: Fail,
) -> Run {
    let git_dir = home.join(".dotfiles");
    let single_origin = |topology| {
        let scope = match topology {
            Topology::Separate => {
                dot::init_client_publish::OriginScope::Separate { git_dir: &git_dir }
            }
            _ => dot::init_client_publish::OriginScope::Ordinary,
        };
        dot::init_client_publish::single_origin(&scope, home)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|text| text.trim_end_matches('\n').to_string())
    };
    let repo_identity = |remote: &str| dot::init_client_identity::repo_identity(remote);
    let transaction = dot::init_client_transaction::transaction_dir(
        home.to_str().expect("home"),
        state.to_str().expect("state"),
    )
    .ok()
    .map(PathBuf::from);
    let transaction_dir = || {
        if fail == Fail::TransactionDir {
            None
        } else {
            transaction.clone()
        }
    };
    let prepare = |path: &Path| dot::init_client_transaction::prepare_transaction(path).ok();
    let phases = RefCell::new(Vec::new());
    let record_cache = RefCell::new(dot::temp::MoveCache::default());
    let write_record = |fields: &adopt::RecordFields<'_>| {
        phases.borrow_mut().push(fields.phase.to_string());
        if (fail == Fail::WriteConverging && fields.phase == "converging")
            || (fail == Fail::WriteComplete && fields.phase == "complete")
        {
            return false;
        }
        dot::init_client_record::write_record(
            fields.record,
            fields.phase,
            &dot::init_client_record::RecordFields {
                origin: fields.origin,
                identity: fields.identity,
                branch: fields.branch,
                backup: fields.backup,
                git_dir: Some(fields.git_dir),
                commit: Some(fields.commit),
                nonce: Some(fields.nonce),
                git_dev: Some(fields.git_dev),
                git_ino: Some(fields.git_ino),
                dot_bin: env!("CARGO_BIN_EXE_dot"),
                home,
                source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            },
            &mut record_cache.borrow_mut(),
        )
        .is_ok()
    };
    let transaction_cache = RefCell::new(dot::temp::MoveCache::default());
    let publish_transaction = |stage: &Path, transaction: &Path| {
        dot::init_client_transaction::publish_transaction(
            Path::new(env!("CARGO_MANIFEST_DIR")),
            stage,
            transaction,
            &mut transaction_cache.borrow_mut(),
        )
    };
    let converges = RefCell::new(Vec::new());
    let forward_converge = |topology, git_dir: &Path| {
        converges
            .borrow_mut()
            .push((topology, git_dir.to_path_buf()));
        fail != Fail::Converge
    };
    let completed = PathBuf::from(
        dot::init_client_transaction::completed_file(
            home.to_str().expect("home"),
            state.to_str().expect("state"),
        )
        .expect("completion path"),
    );
    let completion_cache = RefCell::new(dot::temp::MoveCache::default());
    let publish_completed = |record: &Path| {
        dot::init_client_plan::publish_completed(
            record,
            &completed,
            &|path| {
                if dot::init_client_transaction::private_directory(path) {
                    Ok(())
                } else {
                    Err(dot::errors::Error::Usage {
                        message: "private directory refused",
                    })
                }
            },
            &mut completion_cache.borrow_mut(),
        )
        .is_ok()
    };
    let engine = adopt::AdoptEngine {
        single_origin: &single_origin,
        repo_identity: &repo_identity,
        transaction_dir: &transaction_dir,
        prepare_transaction: &prepare,
        write_record: &write_record,
        publish_transaction: &publish_transaction,
        forward_converge: &forward_converge,
        publish_completed: &publish_completed,
    };
    let result = adopt::adopt_existing(home, selected, requested_origin, identity, branch, &engine);
    Run {
        result,
        phases: phases.into_inner(),
        converges: converges.into_inner(),
    }
}

fn status(home: &str, state: &str) -> adopt::StatusReport {
    let transaction = || {
        dot::init_client_transaction::transaction_dir(home, state)
            .ok()
            .map(PathBuf::from)
    };
    let completed = || {
        dot::init_client_transaction::completed_file(home, state)
            .ok()
            .map(PathBuf::from)
    };
    let home_path = Path::new(home);
    let read = |record: &Path| {
        dot::init_client_record::read_record(record, home_path)
            .ok()
            .map(|record| adopt::StatusRecord {
                phase: record.phase,
                origin: record.origin,
                branch: record.branch,
                backup: record.backup,
            })
    };
    adopt::status(&adopt::StatusEngine {
        transaction_dir: &transaction,
        completed_file: &completed,
        read_record: &read,
    })
}

fn seed_record(home: &Path, state: &Path, destination: &Path, phase: &str) {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).expect("record parent");
    }
    dot::init_client_record::write_record(
        destination,
        phase,
        &dot::init_client_record::RecordFields {
            origin: "file:///origin.git",
            identity: "file:///origin.git",
            branch: "main",
            backup: "-",
            git_dir: Some(&home.join(".dotfiles")),
            commit: Some("0123456789012345678901234567890123456789"),
            nonce: Some("adopted"),
            git_dev: Some("1"),
            git_ino: Some("2"),
            dot_bin: env!("CARGO_BIN_EXE_dot"),
            home,
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        },
        &mut dot::temp::MoveCache::default(),
    )
    .expect("record");
    assert!(state.is_absolute());
}

#[test]
fn usage_bytes_are_exact() {
    assert_eq!(adopt::usage(), b"usage: dot init [--branch BRANCH] [--yes] REPOSITORY_URL\n       dot init --status\n       dot init --rollback\n");
}

#[test]
fn status_reports_not_started() {
    let dir = TempDir::new("status-none").expect("fixture");
    assert_eq!(
        status(
            dir.path().to_str().expect("home"),
            dir.path().join("state").to_str().expect("state")
        ),
        adopt::StatusReport {
            stdout: b"initialization: not started\n".to_vec(),
            stderr: vec![],
            code: 0
        }
    );
}

#[test]
fn status_reports_incomplete_transaction() {
    let dir = TempDir::new("status-incomplete").expect("fixture");
    let state = dir.path().join("state");
    let transaction = state.join("dot/init/transaction");
    seed_record(
        dir.path(),
        &state,
        &transaction.join("record"),
        "converging",
    );
    assert_eq!(status(dir.path().to_str().expect("home"), state.to_str().expect("state")), adopt::StatusReport { stdout: b"initialization: incomplete\nphase: converging\norigin: file:///origin.git\nbranch: main\nbackup: -\n".to_vec(), stderr: vec![], code: 0 });
}

#[test]
fn status_reports_completed_transaction() {
    let dir = TempDir::new("status-complete").expect("fixture");
    let state = dir.path().join("state");
    seed_record(
        dir.path(),
        &state,
        &state.join("dot/init/completed"),
        "complete",
    );
    assert_eq!(
        status(
            dir.path().to_str().expect("home"),
            state.to_str().expect("state")
        ),
        adopt::StatusReport {
            stdout: b"initialization: complete\norigin: file:///origin.git\nbranch: main\n"
                .to_vec(),
            stderr: vec![],
            code: 0
        }
    );
}

#[test]
fn status_rejects_malformed_transaction() {
    let dir = TempDir::new("status-bad-tx").expect("fixture");
    let state = dir.path().join("state");
    write(&state, "dot/init/transaction/record", b"bad\n");
    let transaction = state.join("dot/init/transaction");
    assert_eq!(
        status(
            dir.path().to_str().expect("home"),
            state.to_str().expect("state")
        ),
        adopt::StatusReport {
            stdout: vec![],
            stderr: format!(
                "dot init: malformed initialization transaction: {}\n",
                transaction.display()
            )
            .into_bytes(),
            code: 1
        }
    );
}

#[test]
fn status_rejects_malformed_completion() {
    let dir = TempDir::new("status-bad-completed").expect("fixture");
    let state = dir.path().join("state");
    write(&state, "dot/init/completed", b"bad\n");
    let completed = state.join("dot/init/completed");
    assert_eq!(
        status(
            dir.path().to_str().expect("home"),
            state.to_str().expect("state")
        ),
        adopt::StatusReport {
            stdout: vec![],
            stderr: format!(
                "dot init: malformed completion record: {}\n",
                completed.display()
            )
            .into_bytes(),
            code: 1
        }
    );
}

#[test]
fn status_refuses_unresolvable_state_root() {
    assert_eq!(
        status("", ""),
        adopt::StatusReport {
            stdout: vec![],
            stderr: vec![],
            code: 1
        }
    );
}

#[test]
fn adopt_error_reports_engine_diagnostics() {
    assert_eq!(
        adopt::AdoptError::NoRepository.to_string(),
        "no adoptable client repository"
    );
    assert_eq!(
        adopt::AdoptError::Mismatch.to_string(),
        "existing client repository is untrusted"
    );
    assert_eq!(
        adopt::AdoptError::Failed.to_string(),
        "existing client repository failed adoption"
    );
}

fn error_case(
    tag: &str,
    plant: impl Fn(&Path, &Origin),
    selected: Topology,
    branch: &str,
    identity_override: Option<&str>,
) -> Result<adopt::Adopted, adopt::AdoptError> {
    let dir = TempDir::new(tag).expect("fixture");
    let home = dir.path().join("home");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&home).expect("home");
    let origin = origin(dir.path());
    plant(&home, &origin);
    let url = url(&origin);
    let identity = identity_override
        .map(str::to_string)
        .unwrap_or_else(|| dot::init_client_identity::repo_identity(&url).expect("identity"));
    run_adopt(&home, &state, selected, &url, &identity, branch, Fail::None).result
}

#[test]
fn adopt_reports_no_repository() {
    assert_eq!(
        error_case("no-repo", |_, _| {}, Topology::Missing, "main", None),
        Err(adopt::AdoptError::NoRepository)
    );
}

#[test]
fn adopt_rejects_home_git_file() {
    assert_eq!(
        error_case(
            "git-file",
            |home, _| write(home, ".git", b"gitdir: /elsewhere\n"),
            Topology::Missing,
            "main",
            None
        ),
        Err(adopt::AdoptError::NoRepository)
    );
}

#[test]
fn adopt_rejects_stray_git_directory() {
    assert_eq!(
        error_case(
            "stray-git",
            |home, _| {
                std::fs::create_dir_all(home.join(".git")).expect("stray");
            },
            Topology::Missing,
            "main",
            None
        ),
        Err(adopt::AdoptError::NoRepository)
    );
}

#[test]
fn adopt_ignores_unselected_bare_directory() {
    assert_eq!(
        error_case(
            "unselected-bare",
            plant_separate,
            Topology::Missing,
            "main",
            None
        ),
        Err(adopt::AdoptError::NoRepository)
    );
}

#[test]
fn adopt_rejects_remote_less_selected_repository() {
    assert_eq!(
        error_case(
            "remote-less",
            |home, _| git(&[
                "init",
                "--quiet",
                "--bare",
                home.join(".dotfiles").to_str().expect("git dir")
            ]),
            Topology::Separate,
            "main",
            None
        ),
        Err(adopt::AdoptError::Mismatch)
    );
}

#[test]
fn adopt_rejects_identity_mismatch() {
    assert_eq!(
        error_case(
            "identity",
            plant_separate,
            Topology::Separate,
            "main",
            Some("github.com/other/repo")
        ),
        Err(adopt::AdoptError::Mismatch)
    );
}

#[test]
fn adopt_rejects_branch_mismatch_for_both_topologies() {
    assert_eq!(
        error_case(
            "branch-separate",
            plant_separate,
            Topology::Separate,
            "trunk",
            None
        ),
        Err(adopt::AdoptError::Mismatch)
    );
    assert_eq!(
        error_case(
            "branch-ordinary",
            plant_ordinary,
            Topology::Missing,
            "trunk",
            None
        ),
        Err(adopt::AdoptError::Mismatch)
    );
}

#[test]
fn adopt_rejects_ordinary_repository_without_remote() {
    assert_eq!(
        error_case(
            "ordinary-no-remote",
            |home, _| git(&[
                "-C",
                home.to_str().expect("home"),
                "init",
                "--quiet",
                "--initial-branch",
                "main"
            ]),
            Topology::Missing,
            "main",
            None
        ),
        Err(adopt::AdoptError::Mismatch)
    );
}

#[test]
fn adopt_rejects_unborn_head() {
    assert_eq!(
        error_case(
            "unborn",
            |home, origin| {
                let git_dir = home.join(".dotfiles");
                git(&[
                    "init",
                    "--quiet",
                    "--bare",
                    git_dir.to_str().expect("git dir"),
                ]);
                git(&[
                    "--git-dir",
                    git_dir.to_str().expect("git dir"),
                    "remote",
                    "add",
                    "origin",
                    &url(origin),
                ]);
            },
            Topology::Separate,
            "main",
            None
        ),
        Err(adopt::AdoptError::Mismatch)
    );
}

fn success_case(tag: &str, topology: Topology, plant: fn(&Path, &Origin), git_name: &str) {
    let dir = TempDir::new(tag).expect("fixture");
    let home = dir.path().join("home");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&home).expect("home");
    let origin = origin(dir.path());
    plant(&home, &origin);
    let url = url(&origin);
    let identity = dot::init_client_identity::repo_identity(&url).expect("identity");
    let run = run_adopt(&home, &state, topology, &url, &identity, "main", Fail::None);
    assert_eq!(
        run.result,
        Ok(adopt::Adopted {
            topology: if git_name == ".git" {
                Topology::Ordinary
            } else {
                Topology::Separate
            },
            git_dir: home.join(git_name)
        })
    );
    assert_eq!(run.phases, ["converging", "complete"]);
    assert_eq!(
        run.converges,
        [(
            if git_name == ".git" {
                Topology::Ordinary
            } else {
                Topology::Separate
            },
            home.join(git_name)
        )]
    );
    let completed = state.join("dot/init/completed");
    let raw = std::fs::read(&completed).expect("completed bytes");
    assert_eq!(raw.split(|byte| *byte == b'\n').count(), 15);
    assert_eq!(
        raw.split(|byte| *byte == b'\n').next(),
        Some(b"cgraf78 dot initialization transaction v1".as_slice())
    );
    assert_eq!(field(&raw, b"phase="), b"complete");
    assert_eq!(field(&raw, b"origin="), url.as_bytes());
    assert_eq!(field(&raw, b"identity="), identity.as_bytes());
    assert_eq!(field(&raw, b"branch="), b"main");
    assert_eq!(field(&raw, b"backup="), b"-");
    assert_eq!(field(&raw, b"nonce="), b"adopted");
    assert_eq!(field(&raw, b"commit="), origin.commit.as_bytes());
    assert_eq!(
        field(&raw, b"git_dir="),
        home.join(git_name).as_os_str().as_encoded_bytes()
    );
    assert_eq!(
        field(&raw, b"worktree="),
        home.as_os_str().as_encoded_bytes()
    );
    assert_eq!(mode(&completed), 0o600);
    let record = dot::init_client_record::read_record(&completed, &home).expect("completed record");
    assert_eq!(record.phase, "complete");
    assert_eq!(record.origin, url);
    assert_eq!(record.identity, identity);
    assert_eq!(record.branch, "main");
    assert_eq!(record.backup, "-");
    assert_eq!(record.nonce, "adopted");
    assert_eq!(record.commit, origin.commit);
    assert!(!state.join("dot/init/transaction").exists());
}

#[test]
fn adopt_separate_repository_succeeds() {
    success_case(
        "separate-success",
        Topology::Separate,
        plant_separate,
        ".dotfiles",
    );
}

#[test]
fn adopt_ordinary_repository_succeeds() {
    success_case(
        "ordinary-success",
        Topology::Missing,
        plant_ordinary,
        ".git",
    );
}

fn failure_case(tag: &str, fail: Fail) -> (TempDir, PathBuf, PathBuf, Run) {
    let dir = TempDir::new(tag).expect("fixture");
    let home = dir.path().join("home");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&home).expect("home");
    let origin = origin(dir.path());
    plant_separate(&home, &origin);
    let url = url(&origin);
    let identity = dot::init_client_identity::repo_identity(&url).expect("identity");
    match fail {
        Fail::Prepare => write(&state, "dot/init", b"blocked\n"),
        Fail::PublishTransaction => {
            std::fs::create_dir_all(state.join("dot/init/transaction"))
                .expect("blocking transaction");
        }
        Fail::PublishCompleted => {
            std::fs::create_dir_all(state.join("dot/init/completed")).expect("blocking completion");
        }
        _ => {}
    }
    let run = run_adopt(
        &home,
        &state,
        Topology::Separate,
        &url,
        &identity,
        "main",
        fail,
    );
    (dir, home, state, run)
}

#[test]
fn adopt_preserves_transaction_on_converge_failure() {
    let (_dir, home, state, run) = failure_case("converge-fail", Fail::Converge);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert_eq!(run.phases, ["converging"]);
    assert_eq!(
        run.converges,
        [(Topology::Separate, home.join(".dotfiles"))]
    );
    let record =
        dot::init_client_record::read_record(&state.join("dot/init/transaction/record"), &home)
            .expect("converging record");
    assert_eq!(record.phase, "converging");
}

#[test]
fn adopt_preserves_complete_record_on_completion_failure() {
    let (_dir, home, state, run) = failure_case("completed-fail", Fail::PublishCompleted);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert_eq!(run.phases, ["converging", "complete"]);
    let record =
        dot::init_client_record::read_record(&state.join("dot/init/transaction/record"), &home)
            .expect("complete record");
    assert_eq!(record.phase, "complete");
    assert!(state.join("dot/init/completed").is_dir());
}

#[test]
fn adopt_preserves_stage_on_transaction_publication_failure() {
    let (_dir, _home, state, run) = failure_case("transaction-fail", Fail::PublishTransaction);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert_eq!(run.phases, ["converging"]);
    assert!(run.converges.is_empty());
    assert!(!state.join("dot/init/transaction/record").exists());
    let stages = std::fs::read_dir(state.join("dot/init"))
        .expect("stages")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("transaction.prepare.")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(stages.len(), 1, "exactly one unpublished stage remains");
    let stage = &stages[0];
    assert!(stage.is_dir());
    assert_eq!(
        std::fs::read(stage.join(".dot-transaction-stage-v1")).expect("stage marker"),
        b"cgraf78 dot initialization preparation v1\n"
    );
    assert_eq!(mode(stage), 0o700);
    assert_eq!(mode(&stage.join(".dot-transaction-stage-v1")), 0o600);
    let raw = std::fs::read(stage.join("record")).expect("staged record");
    assert_eq!(field(&raw, b"phase="), b"converging");
}

#[test]
fn adopt_stops_at_prepare_and_record_write_failures() {
    let (_dir, _home, state, run) = failure_case("prepare-fail", Fail::Prepare);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert!(run.phases.is_empty());
    assert!(run.converges.is_empty());
    assert!(!state.join("dot/init/transaction").exists());

    let (_dir, _home, state, run) = failure_case("converging-write-fail", Fail::WriteConverging);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert_eq!(run.phases, ["converging"]);
    assert!(run.converges.is_empty());
    assert!(!state.join("dot/init/transaction").exists());

    let (_dir, home, state, run) = failure_case("complete-write-fail", Fail::WriteComplete);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert_eq!(run.phases, ["converging", "complete"]);
    assert_eq!(
        run.converges,
        [(Topology::Separate, home.join(".dotfiles"))]
    );
    let record =
        dot::init_client_record::read_record(&state.join("dot/init/transaction/record"), &home)
            .expect("converging record remains");
    assert_eq!(record.phase, "converging");
}

#[test]
fn adopt_unresolvable_transaction_dir_fails() {
    let (_dir, _home, state, run) = failure_case("transaction-dir-fail", Fail::TransactionDir);
    assert_eq!(run.result, Err(adopt::AdoptError::Failed));
    assert!(run.phases.is_empty());
    assert!(run.converges.is_empty());
    assert!(!state.join("dot/init/transaction").exists());
}
