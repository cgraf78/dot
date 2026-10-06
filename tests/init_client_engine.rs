//! End-to-end regression tests for the production `dot init` engine.
//!
//! Each case drives [`cmd::run`][dot::init_client_command::run] through the
//! production resume, rollback, and fresh-init wiring against an isolated
//! `file://` repository. The update convergence callback is recorded so tests
//! can assert exactly when initialization hands control to the update engine.
//! Nondeterministic backup stamps and transaction identities are normalized
//! only where a test needs to inspect their stable structure.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use dot::errors::Error;
use dot::init_client_command as cmd;
use dot::init_client_engine as engine;
use dot::init_client_record::{RecordFields, TransactionRecord};
use dot_test_support::TempDir;

/// `&Path` to `&str` for fixture inputs, which always live under `TMPDIR`.
fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path UTF-8")
}

/// Isolated home, state, and repository root for one native engine run.
struct Fixture {
    _dir: TempDir,
    home: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        for path in [&home, &state] {
            std::fs::create_dir_all(path).expect("fixture dir");
        }
        Self {
            _dir: dir,
            home,
            state,
        }
    }

    fn root(&self) -> &Path {
        self._dir.path()
    }
}

/// Run git for fixtures; asserts success, silences output.
fn git(args: &[&str]) {
    let status = dot_test_support::git()
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

/// Write `bytes` to `dir/name`, creating parents.
fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Build a shared bare origin with one commit on `main` under
/// `root/origin.git` (idempotent: an existing origin is reused, so
/// both homes plant records from one).
fn make_origin(root: &Path) -> PathBuf {
    let seed = root.join("seed");
    let path = root.join("origin.git");
    if path.exists() {
        return path;
    }
    git(&["init", "--quiet", path_str(&seed)]);
    write(&seed, ".testrc", b"hello\n");
    git(&["-C", path_str(&seed), "add", ".testrc"]);
    git(&[
        "-C",
        path_str(&seed),
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "--quiet",
        "-m",
        "seed",
    ]);
    git(&["-C", path_str(&seed), "branch", "-M", "main"]);
    git(&[
        "clone",
        "--quiet",
        "--bare",
        path_str(&seed),
        path_str(&path),
    ]);
    git(&[
        "-C",
        path_str(&path),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    path
}

/// Canonical identity used in fixture transaction records.
fn repo_identity(url: &str) -> String {
    dot::init_client_identity::repo_identity(url).expect("valid repository identity")
}

/// Real commit at `branch` in the origin fixture.
fn origin_commit(origin: &Path, branch: &str) -> String {
    let output = dot_test_support::git()
        .args(["-C", path_str(origin), "rev-parse", branch])
        .stdin(Stdio::null())
        .output()
        .expect("rev-parse origin");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("commit UTF-8")
        .trim_end()
        .to_string()
}

/// Write one journal record into `dest` using the production record format.
#[allow(clippy::too_many_arguments)]
fn write_record(
    home: &Path,
    dest: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    git_dir: &str,
) {
    write_record_full(
        home,
        dest,
        phase,
        origin,
        identity,
        branch,
        backup,
        git_dir,
        &"a".repeat(40),
        "n1",
        "7",
        "8",
    );
}

/// Run the production wiring with a recording convergence callback.
fn production_run(
    home: &Path,
    state: &Path,
    scratch: &Path,
    skip_provider: Option<&str>,
    argv: &[&str],
    converge_ok: bool,
) -> (cmd::InitReport, usize) {
    let (report, fired, _) =
        production_run_noticed(home, state, scratch, skip_provider, argv, converge_ok);
    (report, fired)
}

/// [`production_run`] plus the incomplete-initialization notice a failed
/// fresh run leaves, captured the way `dot init`'s dispatcher captures it.
fn production_run_noticed(
    home: &Path,
    state: &Path,
    scratch: &Path,
    skip_provider: Option<&str>,
    argv: &[&str],
    converge_ok: bool,
) -> (cmd::InitReport, usize, Option<Vec<u8>>) {
    let notice = RefCell::new(None);
    let fired = RefCell::new(0usize);
    let on_converge = || -> Result<(), Error> {
        *fired.borrow_mut() += 1;
        if converge_ok {
            Ok(())
        } else {
            Err(Error::Usage {
                message: "stub converge refused",
            })
        }
    };
    let wiring = engine::Production::new(
        engine::EngineCtx {
            home: path_str(home),
            xdg_state_home: path_str(state),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            skip_provider: skip_provider == Some("1"),
            shdeps_update_policy: None,
            cwd: home,
        },
        &on_converge,
    );
    let probe = |url: &str| dot::init_client_identity::remote_default_branch(url, scratch);
    let resume = |transaction: &Path, record: &Path, journal: &TransactionRecord| {
        wiring.resume(transaction, record, journal)
    };
    let rollback = |at: &Path| wiring.rollback(at);
    let fresh = |inputs: &cmd::FreshInputs| {
        let report = wiring.run_fresh(inputs);
        if report.code != 0 {
            *notice.borrow_mut() = wiring.incomplete_notice();
        }
        report
    };
    let eng = cmd::CommandEngine {
        remote_default_branch: &probe,
        resume: &resume,
        rollback: &rollback,
        fresh: &fresh,
    };
    let env = cmd::CommandEnv {
        home: path_str(home),
        xdg_state_home: path_str(state),
        skip_provider,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let bytes: Vec<Vec<u8>> = argv.iter().map(|word| word.as_bytes().to_vec()).collect();
    let report = cmd::run(&env, &eng, &bytes);
    let fired = *fired.borrow();
    (report, fired, notice.into_inner())
}

/// Run one command with successful convergence when the engine reaches it.
fn check(
    fixture: &Fixture,
    argv: &[&str],
    skip_provider: Option<&str>,
) -> (cmd::InitReport, usize) {
    production_run(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        skip_provider,
        argv,
        true,
    )
}

/// Run a command expected to reach a successful convergence callback.
fn check_converge(
    fixture: &Fixture,
    argv: &[&str],
    skip_provider: Option<&str>,
) -> (cmd::InitReport, usize) {
    production_run(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        skip_provider,
        argv,
        true,
    )
}

/// Whether `/dev/tty` opens for confirmation prompts: when the
/// harness has a controlling terminal, the interactive-confirm
/// rows would block, so they skip (CI has no terminal and runs
/// them fully).
fn tty_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok()
}

/// `<state>/dot/init/transaction` and friends: mirrors
/// `_dot_init_transaction_dir` / `_dot_init_completed_file` with
/// `XDG_STATE_HOME` set (the only shape these tests use).
fn transaction_dir(state: &Path) -> PathBuf {
    state.join("dot/init/transaction")
}

/// Plant one transaction journal, creating the transaction directory first.
#[allow(clippy::too_many_arguments)]
fn plant_transaction(
    home: &Path,
    state: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    commit: &str,
    nonce: &str,
    dev: &str,
    ino: &str,
) {
    let transaction = transaction_dir(state);
    std::fs::create_dir_all(&transaction).expect("transaction dir");
    write_record_full(
        home,
        &transaction.join("record"),
        phase,
        origin,
        identity,
        branch,
        backup,
        &format!("{}/.dotfiles", path_str(home)),
        commit,
        nonce,
        dev,
        ino,
    );
}

/// Device and inode identity of a fixture path.
fn path_identity(path: &Path) -> (String, String) {
    let (dev, ino) = dot::temp::path_identity(path).expect("path identity");
    (dev.to_string(), ino.to_string())
}

/// Build a live `$HOME/.dotfiles` checkout and plant its journal.
fn plant_live_checkout(
    fixture: &Fixture,
    origin: &str,
    identity: &str,
    branch: &str,
    phase: &str,
) -> String {
    let commit = origin_commit(&fixture.root().join("origin.git"), branch);
    let (home, state) = (&fixture.home, &fixture.state);
    let git_dir = plant_live_git(fixture, origin, branch);
    let (dev, ino) = path_identity(&git_dir);
    plant_transaction(
        home, state, phase, origin, identity, branch, "-", &commit, "adopted", &dev, &ino,
    );
    commit
}

/// Build a live bare `$HOME/.dotfiles` on `branch` of `origin`, with no
/// journal, and return it.
fn plant_live_git(fixture: &Fixture, origin: &str, branch: &str) -> PathBuf {
    let git_dir = fixture.home.join(".dotfiles");
    git(&["init", "--quiet", "--bare", path_str(&git_dir)]);
    git(&[
        "--git-dir",
        path_str(&git_dir),
        "fetch",
        "--quiet",
        origin,
        &format!("{branch}:refs/heads/{branch}"),
    ]);
    git(&[
        "--git-dir",
        path_str(&git_dir),
        "symbolic-ref",
        "HEAD",
        &format!("refs/heads/{branch}"),
    ]);
    git(&[
        "--git-dir",
        path_str(&git_dir),
        "config",
        "remote.origin.url",
        origin,
    ]);
    git_dir
}

/// Normalize the fixture root and nondeterministic backup stamp.
fn normalize(bytes: &[u8], root: &Path) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let out = text.replace(path_str(root), "<root>");
    let chars: Vec<char> = out.chars().collect();
    let mut normalized = String::with_capacity(out.len());
    let mut index = 0;
    while index < chars.len() {
        let stamp = index + 14 < chars.len()
            && chars[index..index + 14]
                .iter()
                .all(|cell| cell.is_ascii_digit())
            && chars[index + 14] == '-';
        if stamp {
            let mut end = index + 15;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 15 {
                normalized.push_str("<stamp>");
                index = end;
                continue;
            }
        }
        normalized.push(chars[index]);
        index += 1;
    }
    normalized.into_bytes()
}

/// Read the completion record.
fn read_completed(state: &Path) -> Vec<u8> {
    std::fs::read(state.join("dot/init/completed")).expect("completed record")
}

#[test]
fn status_reports_not_started() {
    let fixture = Fixture::build("engine-status-empty");
    let (rust, converged) = check(&fixture, &["--status"], None);
    assert_eq!(rust.code, 0);
    assert_eq!(rust.stdout, b"initialization: not started\n".to_vec());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 0, "status never converges");
}

#[test]
fn status_reports_incomplete_and_complete_transactions() {
    let fixture = Fixture::build("engine-status-reports");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    plant_transaction(
        &fixture.home,
        &fixture.state,
        "prepared",
        &url,
        &identity,
        "main",
        "-",
        &"a".repeat(40),
        "n1",
        "7",
        "8",
    );
    let (rust, converged) = check(&fixture, &["--status"], None);
    assert_eq!(rust.code, 0);
    assert_eq!(
        rust.stdout,
        format!(
            "initialization: incomplete\nphase: prepared\norigin: {url}\nbranch: main\nbackup: -\n"
        )
        .into_bytes()
    );
    assert_eq!(converged, 0, "status never converges");
    let transaction = transaction_dir(&fixture.state);
    let completed = fixture.state.join("dot/init/completed");
    std::fs::rename(transaction.join("record"), &completed).expect("promote journal");
    std::fs::remove_dir_all(&transaction).expect("drop transaction");
    let (rust, converged) = check(&fixture, &["--status"], None);
    assert_eq!(rust.code, 0);
    assert_eq!(
        rust.stdout,
        format!("initialization: complete\norigin: {url}\nbranch: main\n").into_bytes()
    );
    assert_eq!(converged, 0, "status never converges");
}

#[test]
fn rollback_without_transaction_is_rejected() {
    let fixture = Fixture::build("engine-rollback-empty");
    let (rust, converged) = check(&fixture, &["--rollback"], None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: no recoverable transaction\n".to_vec()
    );
    assert_eq!(converged, 0, "refused rollback never converges");
}

#[test]
fn rollback_committed_phase_is_rejected() {
    let fixture = Fixture::build("engine-rollback-committed");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    plant_transaction(
        &fixture.home,
        &fixture.state,
        "checkout",
        &url,
        &identity,
        "main",
        "-",
        &"a".repeat(40),
        "n1",
        "7",
        "8",
    );
    let (rust, converged) = check(&fixture, &["--rollback"], None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: checkout is committed; rerun the original init command to resume\n".to_vec()
    );
    assert_eq!(converged, 0, "refused rollback never converges");
}

#[test]
fn rollback_prepared_succeeds_end_to_end() {
    let fixture = Fixture::build("engine-rollback-prepared");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    let commit = origin_commit(&origin, "main");
    plant_transaction(
        &fixture.home,
        &fixture.state,
        "prepared",
        &url,
        &identity,
        "main",
        "-",
        &commit,
        "n1",
        "7",
        "8",
    );
    // One dangling tree row has no intent, so rollback safely skips it.
    write(
        &transaction_dir(&fixture.state),
        "tree.tsv",
        format!("100644\t{}\t.testrc\n", "b".repeat(40)).as_bytes(),
    );
    let argv = ["--rollback"];
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 0, "rollback never converges");
    assert!(!transaction_dir(&fixture.state).exists());
}

#[test]
fn resume_prepared_missing_journals_is_rejected() {
    let fixture = Fixture::build("engine-resume-journals");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    // A prepared journal with no tree/prior/conflicts files: the
    // resume refuses before any step runs, so no convergence.
    plant_transaction(
        &fixture.home,
        &fixture.state,
        "prepared",
        &url,
        &identity,
        "main",
        "-",
        &"a".repeat(40),
        "n1",
        "7",
        "8",
    );
    let argv = [url.as_str()];
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: initialization transaction could not be resumed safely\n".to_vec()
    );
    assert_eq!(converged, 0, "refused resume never converges");
}

#[test]
fn resume_complete_phase_succeeds_end_to_end() {
    let fixture = Fixture::build("engine-resume-complete");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    plant_live_checkout(&fixture, &url, &identity, "main", "complete");
    let argv = [url.as_str()];
    // No branch flag: the default-branch probe resolves `main` from the origin.
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 0, "complete resume converges nothing");
    assert!(!transaction_dir(&fixture.state).exists());
    assert!(!read_completed(&fixture.state).is_empty());
}

#[test]
fn resume_checkout_invokes_converge_structurally() {
    let fixture = Fixture::build("engine-resume-checkout");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    plant_live_checkout(&fixture, &url, &identity, "main", "checkout");
    let argv = [url.as_str()];
    let (rust, converged) = check_converge(&fixture, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 1, "checkout resume converges once");
    assert!(!transaction_dir(&fixture.state).exists());
    assert!(!read_completed(&fixture.state).is_empty());
}

#[test]
fn converge_failure_reports_resume_error() {
    let fixture = Fixture::build("engine-converge-failure");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    plant_live_checkout(&fixture, &url, &identity, "main", "checkout");
    let argv = [url.as_str()];
    let (rust, converged) = production_run(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        None,
        &argv,
        false,
    );
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: initialization transaction could not be resumed safely\n".to_vec()
    );
    assert_eq!(converged, 1, "refused converge still fires once");
}

#[test]
fn early_argument_gates_do_not_converge() {
    let fixture = Fixture::build("engine-early-gates");
    for argv in [
        vec!["--frobnicate"],
        vec!["--branch"],
        vec!["a", "b"],
        vec!["--status", "some-origin"],
        vec!["--rollback", "--branch", "main"],
        vec!["--branch", "bad..name", "notaurl"],
        vec!["notaurl"],
    ] {
        let (_, converged) = check(&fixture, &argv, None);
        assert_eq!(converged, 0, "gated rows never converge: {argv:?}");
    }
    let (rust, _) = check(&fixture, &["--frobnicate"], None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: unknown option: --frobnicate\n".to_vec()
    );
}

#[test]
fn skip_provider_gate_validates_before_initialization() {
    let fixture = Fixture::build("engine-skip-gate");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    // Only `0` and `1` pass; anything else fails before identity.
    // The mode dispatch runs first, so `--status` never consults
    // the gate.
    let (rust, converged) = check(&fixture, &["--branch", "main", url.as_str()], Some("2"));
    assert_eq!(rust.code, 2);
    assert_eq!(
        rust.stderr,
        b"dot init: DOT_INIT_SKIP_PROVIDER must be 0 or 1\n".to_vec()
    );
    assert_eq!(converged, 0);
    let (rust, _) = check(&fixture, &["--status"], Some("2"));
    assert_eq!(rust.code, 0);
    // Empty counts as unset: the gate passes and the explicit
    // branch plus a missing origin then fail at the clone, past the gate.
    let missing = format!("file://{}/nope.git", path_str(fixture.root()));
    let argv = ["--branch", "main", missing.as_str()];
    let (rust, converged) = check(&fixture, &argv, Some(""));
    assert_eq!(rust.code, 1);
    assert_eq!(converged, 0, "failed clone never converges");
}

#[test]
fn fresh_clone_failure_cleans_candidate() {
    let fixture = Fixture::build("engine-fresh-clone");
    // A well-formed `file://` URL with no repository behind it:
    // identity resolves, the explicit branch skips the probe, and
    // the candidate clone fails and carries git's fatal text in the report.
    let missing = format!("file://{}/nope.git", path_str(fixture.root()));
    let argv = ["--branch", "main", missing.as_str()];
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(converged, 0, "failed clone never converges");
    let init = fixture.state.join("dot/init");
    if init.exists() {
        let leftovers: Vec<_> = std::fs::read_dir(&init)
            .expect("read init dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".candidate.")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "candidate cleaned at {}",
            init.display()
        );
    }
}

#[test]
fn fresh_conflicts_require_yes_noninteractive() {
    if tty_available() {
        eprintln!("SKIP: /dev/tty is present; the confirm prompt would block");
        return;
    }
    let fixture = Fixture::build("engine-fresh-conflicts");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    // A live path colliding with the candidate tree prints the plan, conflict
    // listing, and noninteractive diagnostic.
    write(&fixture.home, ".testrc", b"local work\n");
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = production_run(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        None,
        &argv,
        true,
    );
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    let rust_err = normalize(&rust.stderr, fixture.root());
    let listing = b"dot init: conflicting paths will be backed up:\n  .testrc\n";
    let end = rust_err
        .windows(listing.len())
        .position(|window| window == listing)
        .map(|at| at + listing.len())
        .expect("conflict listing printed");
    assert!(end <= rust_err.len());
    assert!(
        rust_err.ends_with(b"dot init: conflicts require --yes in a noninteractive session\n"),
        "noninteractive diagnostic present"
    );
    assert_eq!(converged, 0, "refused confirm never converges");
}

#[test]
fn fresh_yes_reaches_converge_structurally() {
    let fixture = Fixture::build("engine-fresh-yes");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    write(&fixture.home, ".testrc", b"local work\n");
    let argv = ["--yes", "--branch", "main", url.as_str()];
    let (rust, converged) = check_converge(&fixture, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    let report = normalize(&rust.stderr, fixture.root());
    assert!(report.starts_with(b"dot init plan:\n"));
    assert!(report.ends_with(b"dot init: conflicting paths will be backed up:\n  .testrc\n"));
    assert_eq!(converged, 1, "fresh success converges once");
    assert!(!transaction_dir(&fixture.state).exists());
    assert!(!read_completed(&fixture.state).is_empty());
}

/// Older Git (seen with 2.47) runs `guess_remote_head` on every clone,
/// including `--branch` clones, and when the remote HEAD is detached it
/// looks up `init.defaultBranch` without the quiet flag. A host with no
/// configured default then prints the multi-line "Using 'master' as the
/// name for the initial branch" advice on the clone's stderr, which the
/// engine merges into the init report. This shim reproduces that on any
/// Git: it prints the advice for a clone unless the command line itself
/// configures a default. Ambient config sources are ignored so a
/// developer's own setting cannot make the test pass vacuously.
const OLD_GIT_ADVICE_SHIM: &str = r#"#!/bin/sh
real='@REAL@'
prefix=
for arg do
  if [ "$arg" = clone ]; then
    # shellcheck disable=SC2086 # The global options are plain words.
    default=$(unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
      GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
        "$real" -C / $prefix config --get init.defaultBranch 2>/dev/null)
    [ -n "$default" ] || printf '%s\n' \
      "hint: Using 'master' as the name for the initial branch. This default branch name" \
      'hint: is subject to change.' >&2
    break
  fi
  prefix="$prefix $arg"
done
exec "$real" "$@"
"#;

#[test]
fn fresh_clone_of_detached_head_relays_no_branch_name_advice() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::build("engine-detached-advice");
    let origin = make_origin(fixture.root());
    let commit = origin_commit(&origin, "main");
    git(&[
        "--git-dir",
        path_str(&origin),
        "update-ref",
        "--no-deref",
        "HEAD",
        &commit,
    ]);
    let real = dot_test_support::real_tool("git");
    let shim = fixture.root().join("old-git");
    std::fs::write(
        &shim,
        OLD_GIT_ADVICE_SHIM.replace("@REAL@", path_str(&real)),
    )
    .expect("write git shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("git shim mode");
    let url = format!("file://{}", path_str(&origin));
    let argv = ["--yes", "--branch", "main", url.as_str()];
    let (rust, converged) =
        dot::init_client_identity::with_host_git(&shim, || check_converge(&fixture, &argv, None));
    let stderr = String::from_utf8_lossy(&rust.stderr);
    assert_eq!(rust.code, 0, "init succeeds: {stderr}");
    assert!(
        !stderr.contains("hint:"),
        "a successful clone relays no Git advice: {stderr}"
    );
    assert!(
        stderr.starts_with("dot init plan:\n"),
        "plan printed: {stderr}"
    );
    assert_eq!(converged, 1, "fresh success converges once");
}

#[test]
fn adopt_mismatch_is_rejected() {
    let fixture = Fixture::build("engine-adopt-mismatch");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let other = fixture.root().join("other.git");
    git(&[
        "clone",
        "--quiet",
        "--bare",
        path_str(&origin),
        path_str(&other),
    ]);
    let other_url = format!("file://{}", path_str(&other));
    // An ordinary `$HOME/.git` checkout tracking another origin:
    // adoption refuses before convergence.
    git(&["init", "--quiet", path_str(&fixture.home)]);
    git(&[
        "-C",
        path_str(&fixture.home),
        "remote",
        "add",
        "origin",
        &other_url,
    ]);
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: existing client repository does not match the requested origin and branch\n"
            .to_vec()
    );
    assert_eq!(converged, 0, "refused adoption never converges");
}

#[test]
fn completed_record_for_another_identity_is_rejected() {
    let fixture = Fixture::build("engine-completed-gates");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    // A completed record for another identity: the rerun refuses
    // before touching the live checkout.
    let completed = fixture.state.join("dot/init/completed");
    std::fs::create_dir_all(completed.parent().expect("completed parent")).expect("completed dir");
    write_record(
        &fixture.home,
        &completed,
        "complete",
        "file:///elsewhere.git",
        "elsewhere-identity",
        "main",
        "-",
        &format!("{}/.dotfiles", path_str(&fixture.home)),
    );
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: initialized client belongs to a different repository or branch\n".to_vec()
    );
    assert_eq!(converged, 0);
}

#[test]
fn transaction_identity_mismatch_is_rejected() {
    let fixture = Fixture::build("engine-transaction-mismatch");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    plant_transaction(
        &fixture.home,
        &fixture.state,
        "prepared",
        "file:///elsewhere.git",
        "elsewhere-identity",
        "main",
        "-",
        &"a".repeat(40),
        "n1",
        "7",
        "8",
    );
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = check(&fixture, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: existing transaction belongs to a different repository or branch\n".to_vec()
    );
    assert_eq!(converged, 0, "rejected transaction never resumes");
}

#[test]
fn completed_rerun_converges_structurally() {
    let fixture = Fixture::build("engine-rerun");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = repo_identity(&url);
    plant_live_checkout(&fixture, &url, &identity, "main", "complete");
    let transaction = transaction_dir(&fixture.state);
    let completed = fixture.state.join("dot/init/completed");
    std::fs::rename(transaction.join("record"), &completed).expect("promote");
    std::fs::remove_dir_all(&transaction).expect("drop transaction");
    let before = read_completed(&fixture.state);
    // A rerun with no transaction takes the completed-file branch:
    // the live checkout still matches, so the engine converges.
    let argv = [url.as_str()];
    let (rust, converged) = check_converge(&fixture, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 1, "completed rerun converges once");
    assert_eq!(
        read_completed(&fixture.state),
        before,
        "completion record is untouched"
    );
}

#[test]
fn fresh_convergence_failure_names_the_unfinished_initialization() {
    let fixture = Fixture::build("engine-fresh-incomplete");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    let argv = ["--yes", "--branch", "main", url.as_str()];
    let (rust, converged, notice) = production_run_noticed(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        None,
        &argv,
        false,
    );
    assert_eq!(rust.code, 1);
    assert_eq!(converged, 1);
    let record = std::fs::read_to_string(transaction_dir(&fixture.state).join("record"))
        .expect("the transaction stays for a rerun");
    assert!(record.contains("\nphase=converging\n"), "{record}");
    assert_eq!(
        String::from_utf8(notice.expect("notice")).expect("UTF-8"),
        format!(
            "dot init: initialization is incomplete (stopped at phase converging); \
             rerun 'dot init --branch main {url}' to finish it\n"
        )
    );
}

#[test]
fn adoption_convergence_failure_names_the_unfinished_initialization() {
    let fixture = Fixture::build("engine-adopt-incomplete");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    plant_live_git(&fixture, &url, "main");
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged, notice) = production_run_noticed(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        None,
        &argv,
        false,
    );
    assert_eq!(rust.code, 1);
    assert_eq!(converged, 1, "adoption converges once");
    assert_eq!(
        String::from_utf8(notice.expect("notice")).expect("UTF-8"),
        format!(
            "dot init: initialization is incomplete (stopped at phase converging); \
             rerun 'dot init --branch main {url}' to finish it\n"
        )
    );
}

#[test]
fn fresh_failure_before_the_transaction_leaves_no_notice() {
    let fixture = Fixture::build("engine-fresh-no-notice");
    let missing = format!("file://{}/nope.git", path_str(fixture.root()));
    let argv = ["--branch", "main", missing.as_str()];
    let (rust, _, notice) = production_run_noticed(
        &fixture.home,
        &fixture.state,
        fixture.root(),
        None,
        &argv,
        true,
    );
    assert_eq!(rust.code, 1);
    assert_eq!(notice, None);
}

/// The engine inputs `dot update` finishes an initialization with.
fn finish(fixture: &Fixture) -> dot::errors::Result<engine::Finish> {
    engine::finish_converged(engine::EngineCtx {
        home: path_str(&fixture.home),
        xdg_state_home: path_str(&fixture.state),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: false,
        shdeps_update_policy: None,
        cwd: &fixture.home,
    })
}

/// The phase a journal at `path` records.
fn recorded_phase(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("journal")
        .lines()
        .find_map(|line| line.strip_prefix("phase="))
        .expect("phase line")
        .to_string()
}

#[test]
fn finish_converged_completes_every_committed_phase() {
    for phase in ["checkout", "converging", "complete"] {
        let fixture = Fixture::build("engine-finish-committed");
        let origin = make_origin(fixture.root());
        let url = format!("file://{}", path_str(&origin));
        plant_live_checkout(&fixture, &url, &repo_identity(&url), "main", phase);
        assert_eq!(finish(&fixture).expect(phase), engine::Finish::Completed);
        assert!(!transaction_dir(&fixture.state).exists(), "{phase}");
        let completed = fixture.state.join("dot/init/completed");
        assert_eq!(recorded_phase(&completed), "complete", "{phase}");
    }
}

#[test]
fn finish_converged_retires_a_transaction_left_after_its_completion_record() {
    // A crash between publishing the completion record and removing the
    // transaction leaves both.
    let fixture = Fixture::build("engine-finish-both");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    plant_live_checkout(&fixture, &url, &repo_identity(&url), "main", "complete");
    let completed = fixture.state.join("dot/init/completed");
    std::fs::copy(transaction_dir(&fixture.state).join("record"), &completed)
        .expect("completion record");
    assert_eq!(finish(&fixture).expect("finish"), engine::Finish::Completed);
    assert!(!transaction_dir(&fixture.state).exists());
    assert_eq!(recorded_phase(&completed), "complete");
}

#[test]
fn finish_converged_leaves_a_transaction_before_its_checkout() {
    for phase in ["prepared", "publishing"] {
        let fixture = Fixture::build("engine-finish-uncommitted");
        let origin = make_origin(fixture.root());
        let url = format!("file://{}", path_str(&origin));
        plant_live_checkout(&fixture, &url, &repo_identity(&url), "main", phase);
        let record = transaction_dir(&fixture.state).join("record");
        let before = std::fs::read(&record).expect("record");
        assert_eq!(finish(&fixture).expect(phase), engine::Finish::Uncommitted);
        assert_eq!(std::fs::read(&record).expect("record"), before, "{phase}");
        assert!(!fixture.state.join("dot/init/completed").exists());
    }
}

#[test]
fn finish_converged_reports_a_transaction_it_could_not_remove() {
    // Resuming a `complete` record ignores a failed removal, like `dot init`.
    if dot::temp::current_uid() == Some(0) {
        eprintln!("SKIP: root removes entries from a read-only directory");
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::build("engine-finish-stuck");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    plant_live_checkout(&fixture, &url, &repo_identity(&url), "main", "complete");
    let transaction = transaction_dir(&fixture.state);
    let mode = |bits| std::fs::Permissions::from_mode(bits);
    std::fs::set_permissions(&transaction, mode(0o500)).expect("read-only transaction");
    let finished = finish(&fixture);
    std::fs::set_permissions(&transaction, mode(0o700)).expect("restore transaction");
    assert!(finished.is_err());
    assert!(transaction.join("record").exists());
}

#[test]
fn finish_converged_without_a_transaction_does_nothing() {
    let fixture = Fixture::build("engine-finish-absent");
    assert_eq!(finish(&fixture).expect("finish"), engine::Finish::Absent);
    assert!(!fixture.state.join("dot/init").exists());
}

#[test]
fn finish_converged_refuses_a_client_that_no_longer_matches() {
    let fixture = Fixture::build("engine-finish-mismatch");
    let origin = make_origin(fixture.root());
    let url = format!("file://{}", path_str(&origin));
    plant_live_checkout(&fixture, &url, &repo_identity(&url), "main", "converging");
    // Another branch checked out since: the record no longer names it.
    let git_dir = fixture.home.join(".dotfiles");
    git(&["--git-dir", path_str(&git_dir), "branch", "other", "main"]);
    git(&[
        "--git-dir",
        path_str(&git_dir),
        "symbolic-ref",
        "HEAD",
        "refs/heads/other",
    ]);
    let record = transaction_dir(&fixture.state).join("record");
    let before = std::fs::read(&record).expect("record");
    assert!(finish(&fixture).is_err());
    assert_eq!(std::fs::read(&record).expect("record"), before);
    assert!(!fixture.state.join("dot/init/completed").exists());
}

#[test]
fn finish_converged_refuses_an_unreadable_record() {
    let fixture = Fixture::build("engine-finish-unreadable");
    let transaction = transaction_dir(&fixture.state);
    std::fs::create_dir_all(&transaction).expect("transaction");
    std::fs::write(transaction.join("record"), b"garbage\n").expect("record");
    assert!(finish(&fixture).is_err());
    assert!(transaction.join("record").exists());
}

/// Write one journal record with explicit commit, nonce, and device
/// identity (the live-git fixtures need the real values).
#[allow(clippy::too_many_arguments)]
fn write_record_full(
    home: &Path,
    dest: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    git_dir: &str,
    commit: &str,
    nonce: &str,
    dev: &str,
    ino: &str,
) {
    let mut cache = dot::temp::MoveCache::default();
    dot::init_client_record::write_record(
        dest,
        phase,
        &RecordFields {
            origin,
            identity,
            branch,
            backup,
            git_dir: Some(Path::new(git_dir)),
            commit: Some(commit),
            nonce: Some(nonce),
            git_dev: Some(dev),
            git_ino: Some(ino),
            dot_bin: concat!(env!("CARGO_MANIFEST_DIR"), "/bin/dot"),
            home,
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        },
        &mut cache,
    )
    .expect("write fixture record");
}
