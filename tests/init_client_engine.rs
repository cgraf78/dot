//! Differential parity tests for the production `dot init` engine
//! (`src/init_client_engine.rs`) against the live shell: the three
//! `run_init` closures (`resume`, `rollback`, `fresh`) wired to the
//! already-ported `init_client_*` modules, driving
//! [`cmd::run`][dot::init_client_command::run] end to end on
//! `file://` fixtures.
//!
//! Twin homes and states keep the engines disjoint (like the
//! `init_client_command` tests); the shared bare origin lives under
//! the twin root so both engines derive the same repository
//! identity. Effect-free helpers run as the REAL ports on the Rust
//! side; only the network default-branch probe and the
//! update-engine convergence cross as closures. The probe runs the
//! PORTED [`identity::remote_default_branch`][dot::init_client_identity::remote_default_branch]
//! (a `file://` clone needs no network), while convergence runs a
//! RECORDING stub on both engines (see `STUBS`): the executors
//! (`_dot_update_sync_repos`, `_dot_update_finalize`) stay
//! shell-owned until their lanes land, and the stub keeps both
//! sides silent, so every row compares byte for byte — streams,
//! codes, and (redacted) journals alike.
//!
//! Nondeterministic text is normalized before comparing: the twin
//! root, the backup stamp (`<14 digits>-<pid>`), and the journal
//! nonce plus live device identity. Planted journals pin the nonce
//! (`n1`); fresh runs mint one per engine, so completion records
//! compare through [`normalize_journal`].

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::errors::Error;
use dot::init_client_command as cmd;
use dot::init_client_engine as engine;
use dot::init_client_record::TransactionRecord;
use dot::test_support::{TempDir, bash};

/// Sources for the command oracle: the resource runtime, the shared
/// temp helpers, the XDG root, and the init client itself. The
/// repository model stays out (like the `init_client_command`
/// tests): `dot_init_command` never consults it, and model.sh runs
/// client selection at source time, which would pollute the streams.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixture-scoped stand-ins for the callees outside the ported
/// range, defined after the sources so they win. Each preserves the
/// fixture behavior exactly (asserted by the rows that need them):
/// `_dot_init_forward_converge` records its call and returns
/// `$CONVERGE_RC` (the update-engine executors stay shell-owned
/// until their lanes land, so rows that converge compare codes
/// plus post-state, never converge bytes); `_base_repo_exists`
/// reports the missing-topology shape `bin/dot` starts from (the
/// model that would set it is unsourced here); and
/// `dot_candidate_path_is_reserved` reports the `.testrc`
/// fixtures unreserved (the only tree entries these tests plant).
const STUBS: &str = concat!(
    "_dot_init_forward_converge() { printf 'converged\\n' >>\"${CONVERGE_LOG:-/dev/null}\"; return \"${CONVERGE_RC:-0}\"; }\n",
    "_base_repo_exists() { return 1; }\n",
    "dot_candidate_path_is_reserved() { return 1; }\n",
);

/// Run one shell snippet with the command runtime sourced, in engine
/// mode: the snippet body runs inside `( set -euo pipefail; ... )`
/// because production (`bin/dot`, `lib/dot/main.sh`) always does.
/// `home` may be empty (the unresolvable-state rows), in which case
/// `cwd` must still exist, so an empty home runs at `/`.
fn shell_eval(
    home: &str,
    cwd: &Path,
    state: &Path,
    extra: &[(&str, &str)],
    snippet: &str,
) -> Output {
    let cwd = if cwd.as_os_str().is_empty() {
        Path::new("/")
    } else {
        cwd
    };
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut child = Command::new(bash());
    child
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{STUBS}( set -euo pipefail\n{snippet}\n)"));
    child.arg("dot-test-sh").arg(repo);
    child
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("XDG_STATE_HOME", state)
        .env("XDG_CONFIG_HOME", "")
        .env("DOT_SOURCE_ROOT", repo)
        .env("DOT_TEST", "1")
        .env("DOT_BIN", format!("{repo}/bin/dot"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra {
        child.env(key, value);
    }
    child.output().expect("spawn bash")
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// `&Path` to `&str` for engine and snippet inputs: twin paths are
/// always UTF-8 (they live under `TMPDIR`).
fn path_str(path: &Path) -> &str {
    path.to_str().expect("twin path UTF-8")
}

/// Twin homes and states: disjoint directories so journals and
/// transactions never collide across engines.
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    shell_state: PathBuf,
    rust_state: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        let shell_state = dir.path().join("sh-state");
        let rust_state = dir.path().join("rs-state");
        for path in [&shell_home, &rust_home, &shell_state, &rust_state] {
            std::fs::create_dir_all(path).expect("twin dir");
        }
        Self {
            _dir: dir,
            shell_home,
            rust_home,
            shell_state,
            rust_state,
        }
    }

    fn root(&self) -> &Path {
        self._dir.path()
    }
}

/// Run git for fixtures; asserts success, silences output.
fn git(args: &[&str]) {
    let status = Command::new("git")
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

/// Canonical identity of `url` through the LIVE shell normalizer, so
/// fixtures match what the oracle itself derives (a Rust-derived
/// identity here would cancel out a real drift).
fn shell_identity(url: &str) -> String {
    let output = shell_eval(
        "/",
        Path::new("/"),
        Path::new("/"),
        &[],
        &format!(
            "identity=$(_dot_init_repo_identity {})\ncode=$?\nprintf '%s' \"$identity\"\nexit \"$code\"\n",
            sq(url),
        ),
    );
    assert_eq!(output.status.code(), Some(0), "shell identity of {url}");
    String::from_utf8(output.stdout).expect("identity UTF-8")
}

/// Real commit at `branch` in the origin fixture.
fn origin_commit(origin: &Path, branch: &str) -> String {
    let output = Command::new("git")
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

/// Write one journal record through the live shell into `dest`.
/// `git_dir` is the per-home `$HOME/.dotfiles` spelling the journal
/// pins to its own home.
#[allow(clippy::too_many_arguments)]
fn shell_write_record(
    home: &Path,
    state: &Path,
    dest: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    git_dir: &str,
) {
    shell_write_record_full(
        home,
        state,
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

/// Run the REAL production wiring on `argv`: the ported
/// default-branch probe plus a recording converge stub.
/// `converge_ok` decides the stub verdict (mirroring `$CONVERGE_RC`
/// on the shell side). Returns the report plus the converge
/// invocation count; only owned values escape, so the closures can
/// borrow the wiring freely.
fn production_run(
    home: &Path,
    state: &Path,
    scratch: &Path,
    skip_provider: Option<&str>,
    argv: &[&str],
    converge_ok: bool,
) -> (cmd::InitReport, usize) {
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
            // The shell oracle runs at the twin home, so the
            // reserved probe sees the same working directory.
            cwd: home,
        },
        &on_converge,
    );
    let probe = |url: &str| dot::init_client_identity::remote_default_branch(url, scratch);
    let resume = |transaction: &Path, record: &Path, journal: &TransactionRecord| {
        wiring.resume(transaction, record, journal)
    };
    let rollback = |at: &Path| wiring.rollback(at);
    let fresh = |inputs: &cmd::FreshInputs| wiring.run_fresh(inputs);
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
    (report, *fired.borrow())
}

/// Converge-stub log inside the twin root: the shell stub appends
/// one line per call, so rows assert the boundary was (or was not)
/// reached on both engines.
fn converge_log(root: &Path) -> PathBuf {
    root.join("converge.log")
}

/// Read the converge log, or empty when the boundary never fired.
fn converge_lines(root: &Path) -> Vec<u8> {
    std::fs::read(converge_log(root)).unwrap_or_default()
}

/// Run the live `dot_init_command` oracle on `argv` with
/// `DOT_INIT_SKIP_PROVIDER` optionally set and the converge stub
/// returning `converge_rc`. Returns (exit code, stdout, stderr).
fn oracle(
    argv: &[&str],
    home: &Path,
    state: &Path,
    skip_provider: Option<&str>,
    root: &Path,
    converge_rc: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut snippet = String::from("dot_init_command");
    for word in argv {
        snippet.push_str(&format!(" {}", sq(word)));
    }
    snippet.push_str("\ncode=$?\nexit \"$code\"\n");
    let log = converge_log(root);
    let mut extra: Vec<(&str, &str)> = vec![
        ("CONVERGE_LOG", path_str(&log)),
        ("CONVERGE_RC", converge_rc),
    ];
    if let Some(value) = skip_provider {
        extra.push(("DOT_INIT_SKIP_PROVIDER", value));
    }
    let output = shell_eval(path_str(home), home, state, &extra, &snippet);
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// One byte-exact differential row: both engines see identical argv
/// and `DOT_INIT_SKIP_PROVIDER`, with the converge stub succeeding.
/// Streams and codes must agree byte for byte, and the converge
/// log must stay empty (these rows never reach the boundary).
fn check(twins: &Twins, argv: &[&str], skip_provider: Option<&str>) -> (cmd::InitReport, usize) {
    let (rust, converged) = production_run(
        &twins.rust_home,
        &twins.rust_state,
        twins.root(),
        skip_provider,
        argv,
        true,
    );
    let (code, stdout, stderr) = oracle(
        argv,
        &twins.shell_home,
        &twins.shell_state,
        skip_provider,
        twins.root(),
        "0",
    );
    assert_eq!(rust.code, code, "argv: {argv:?}");
    assert_eq!(rust.stdout, stdout, "argv: {argv:?}");
    assert_eq!(rust.stderr, stderr, "argv: {argv:?}");
    assert!(
        converge_lines(twins.root()).is_empty(),
        "no convergence on {argv:?}"
    );
    (rust, converged)
}

/// One structural converge row: both engines run with succeeding
/// converge stubs, which stay silent on both sides (unlike the
/// real update engine). Returns the Rust report, its stub count,
/// and the shell (code, stdout, stderr, log) tuple.
/// Shell side of a [`check_converge`] row: exit code, stdout,
/// stderr, and converge-log bytes.
type OracleConverge = (i32, Vec<u8>, Vec<u8>, Vec<u8>);

fn check_converge(
    twins: &Twins,
    argv: &[&str],
    skip_provider: Option<&str>,
) -> (cmd::InitReport, usize, OracleConverge) {
    let (rust, converged) = production_run(
        &twins.rust_home,
        &twins.rust_state,
        twins.root(),
        skip_provider,
        argv,
        true,
    );
    let (code, stdout, stderr) = oracle(
        argv,
        &twins.shell_home,
        &twins.shell_state,
        skip_provider,
        twins.root(),
        "0",
    );
    assert_eq!(rust.code, code, "argv: {argv:?}");
    assert_eq!(rust.stdout, stdout, "argv: {argv:?} stdout");
    (
        rust,
        converged,
        (code, stdout, stderr, converge_lines(twins.root())),
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

/// Plant one transaction journal through the live shell, creating
/// the transaction directory first.
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
    shell_write_record_full(
        home,
        state,
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

/// Device and inode identity of `path` through the live shell
/// (`stat` spelling differs per OS; the shell helper hides that).
fn shell_path_identity(home: &Path, state: &Path, path: &Path) -> (String, String) {
    let output = shell_eval(
        path_str(home),
        home,
        state,
        &[],
        &format!(
            "identity=$(_dot_path_identity {})\ncode=$?\nprintf '%s' \"$identity\"\nexit \"$code\"\n",
            sq(path_str(path)),
        ),
    );
    assert_eq!(output.status.code(), Some(0), "path identity");
    let text = String::from_utf8(output.stdout).expect("identity UTF-8");
    let (dev, ino) = text.split_once(':').expect("dev:ino shape");
    (dev.to_string(), ino.to_string())
}

/// Build a live `$HOME/.dotfiles` bare checkout matching `origin`
/// at `branch`, then plant a journal for it in both twins. With
/// nonce `adopted` the generation-marker gate is skipped, exactly
/// like the shell's adopted runs. Returns the locked commit.
fn plant_live_checkout(
    twins: &Twins,
    origin: &str,
    identity: &str,
    branch: &str,
    phase: &str,
) -> String {
    let commit = origin_commit(&twins.root().join("origin.git"), branch);
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        let git_dir = home.join(".dotfiles");
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
        let (dev, ino) = shell_path_identity(home, state, &git_dir);
        plant_transaction(
            home, state, phase, origin, identity, branch, "-", &commit, "adopted", &dev, &ino,
        );
    }
    commit
}

/// Normalize nondeterministic run text: the twin root (per-engine
/// temp dirs) becomes `<root>`, the per-twin home and state leaves
/// become `<home>` and `<state>`, and the backup stamp
/// (`<14 digits>-<pid>`, one per engine pid and second) becomes
/// `<stamp>`.
fn normalize(bytes: &[u8], root: &Path) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let out = text
        .replace(path_str(root), "<root>")
        .replace("<root>/sh-home", "<home>")
        .replace("<root>/rs-home", "<home>")
        .replace("<root>/sh-state", "<state>")
        .replace("<root>/rs-state", "<state>");
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

/// Redact the per-engine journal lines (run nonce and live device
/// identity) on top of [`normalize`], so completion records compare
/// byte for byte across twins. Structural fields (phase, origin,
/// identity, branch, commit, backup, paths) still compare exactly.
fn normalize_journal(bytes: &[u8], root: &Path) -> Vec<u8> {
    let normalized = normalize(bytes, root);
    let text = String::from_utf8_lossy(&normalized);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        let redacted = if trimmed.starts_with("nonce=") {
            "nonce=<nonce>\n"
        } else if trimmed.starts_with("git_dev=") {
            "git_dev=<id>\n"
        } else if trimmed.starts_with("git_ino=") {
            "git_ino=<id>\n"
        } else {
            line
        };
        out.push_str(redacted);
    }
    out.into_bytes()
}

/// Read one twin's completion record.
fn read_completed(state: &Path) -> Vec<u8> {
    std::fs::read(state.join("dot/init/completed")).expect("completed record")
}

#[test]
fn status_not_started_matches_shell() {
    let twins = Twins::build("engine-status-empty");
    let (rust, converged) = check(&twins, &["--status"], None);
    assert_eq!(rust.code, 0);
    assert_eq!(rust.stdout, b"initialization: not started\n".to_vec());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 0, "status never converges");
}

#[test]
fn status_reports_match_shell() {
    let twins = Twins::build("engine-status-reports");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    // Incomplete: backup `-` keeps the report free of twin paths,
    // so both engines print byte-identical text.
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        plant_transaction(
            home,
            state,
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
    }
    let (rust, converged) = check(&twins, &["--status"], None);
    assert_eq!(rust.code, 0);
    assert_eq!(
        rust.stdout,
        format!(
            "initialization: incomplete\nphase: prepared\norigin: {url}\nbranch: main\nbackup: -\n"
        )
        .into_bytes()
    );
    assert_eq!(converged, 0, "status never converges");
    // Complete: move both journals to the completed file and drop
    // the transactions, then compare again.
    for state in [&twins.shell_state, &twins.rust_state] {
        let transaction = transaction_dir(state);
        let completed = state.join("dot/init/completed");
        std::fs::rename(transaction.join("record"), &completed).expect("promote journal");
        std::fs::remove_dir_all(&transaction).expect("drop transaction");
    }
    let (rust, converged) = check(&twins, &["--status"], None);
    assert_eq!(rust.code, 0);
    assert_eq!(
        rust.stdout,
        format!("initialization: complete\norigin: {url}\nbranch: main\n").into_bytes()
    );
    assert_eq!(converged, 0, "status never converges");
}

#[test]
fn rollback_without_transaction_matches_shell() {
    let twins = Twins::build("engine-rollback-empty");
    let (rust, converged) = check(&twins, &["--rollback"], None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: no recoverable transaction\n".to_vec()
    );
    assert_eq!(converged, 0, "refused rollback never converges");
}

#[test]
fn rollback_committed_phase_matches_shell() {
    let twins = Twins::build("engine-rollback-committed");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        plant_transaction(
            home,
            state,
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
    }
    let (rust, converged) = check(&twins, &["--rollback"], None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: checkout is committed; rerun the original init command to resume\n".to_vec()
    );
    assert_eq!(converged, 0, "refused rollback never converges");
}

#[test]
fn rollback_prepared_succeeds_end_to_end() {
    let twins = Twins::build("engine-rollback-prepared");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    let commit = origin_commit(&origin, "main");
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        plant_transaction(
            home, state, "prepared", &url, &identity, "main", "-", &commit, "n1", "7", "8",
        );
        // One dangling tree row: its intent is missing, so both
        // engines hash, miss, and skip it (the sort/hash/skip loop
        // runs without any stage dance).
        write(
            &transaction_dir(state),
            "tree.tsv",
            format!("100644\t{}\t.testrc\n", "b".repeat(40)).as_bytes(),
        );
    }
    let argv = ["--rollback"];
    let (rust, converged) = check(&twins, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 0, "rollback never converges");
    for state in [&twins.shell_state, &twins.rust_state] {
        assert!(
            !transaction_dir(state).exists(),
            "transaction removed at {}",
            state.display()
        );
    }
}

#[test]
fn resume_prepared_missing_journals_matches_shell() {
    let twins = Twins::build("engine-resume-journals");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    // A prepared journal with no tree/prior/conflicts files: the
    // resume refuses before any step runs, so no convergence.
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        plant_transaction(
            home,
            state,
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
    }
    let argv = [url.as_str()];
    let (rust, converged) = check(&twins, &argv, None);
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
    let twins = Twins::build("engine-resume-complete");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    plant_live_checkout(&twins, &url, &identity, "main", "complete");
    let argv = [url.as_str()];
    // No branch flag: the ported default-branch probe resolves
    // `main` from the shared origin, exactly like the shell.
    let (rust, converged) = check(&twins, &argv, None);
    assert_eq!(rust.code, 0);
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 0, "complete resume converges nothing");
    // The completion record is a copy of the planted journal, so
    // both engines land identical post-state up to the per-twin
    // device identity.
    for state in [&twins.shell_state, &twins.rust_state] {
        assert!(
            !transaction_dir(state).exists(),
            "transaction removed at {}",
            state.display()
        );
    }
    assert_eq!(
        normalize_journal(&read_completed(&twins.shell_state), twins.root()),
        normalize_journal(&read_completed(&twins.rust_state), twins.root()),
        "completion records agree across engines"
    );
}

#[test]
fn resume_checkout_invokes_converge_structurally() {
    let twins = Twins::build("engine-resume-checkout");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    plant_live_checkout(&twins, &url, &identity, "main", "checkout");
    let argv = [url.as_str()];
    // Both converge stubs succeed silently, so streams compare
    // exactly too; only the real update engine (outside this
    // slice) would print progress here.
    let (rust, converged, (code, _, _, log)) = check_converge(&twins, &argv, None);
    assert_eq!((rust.code, code), (0, 0));
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 1, "checkout resume converges once");
    assert_eq!(log, b"converged\n".to_vec(), "shell converges once");
    for state in [&twins.shell_state, &twins.rust_state] {
        assert!(
            !transaction_dir(state).exists(),
            "transaction removed at {}",
            state.display()
        );
    }
    assert_eq!(
        normalize_journal(&read_completed(&twins.shell_state), twins.root()),
        normalize_journal(&read_completed(&twins.rust_state), twins.root()),
        "completion records agree across engines"
    );
}

#[test]
fn converge_failure_matches_shell() {
    let twins = Twins::build("engine-converge-failure");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    plant_live_checkout(&twins, &url, &identity, "main", "checkout");
    // Both converge stubs refuse: the resume maps the failure onto
    // the shell's fixed resume text, byte for byte.
    let argv = [url.as_str()];
    let (rust, converged) = production_run(
        &twins.rust_home,
        &twins.rust_state,
        twins.root(),
        None,
        &argv,
        false,
    );
    let (code, stdout, stderr) = oracle(
        &argv,
        &twins.shell_home,
        &twins.shell_state,
        None,
        twins.root(),
        "1",
    );
    assert_eq!(rust.code, code, "refused converge code");
    assert_eq!(rust.code, 1);
    assert_eq!(rust.stdout, stdout);
    assert_eq!(
        rust.stderr,
        b"dot init: initialization transaction could not be resumed safely\n".to_vec()
    );
    assert_eq!(rust.stderr, stderr);
    assert_eq!(converged, 1, "refused converge still fires once");
    assert_eq!(
        converge_lines(twins.root()),
        b"converged\n".to_vec(),
        "shell converge fires once"
    );
}

#[test]
fn early_gates_match_shell() {
    let twins = Twins::build("engine-early-gates");
    // Unknown options diagnose with code 1 (production runs under
    // errexit, so the trailing `return 2` is dead); arity failures
    // stay silent with code 2. None reach the engine closures.
    for argv in [
        vec!["--frobnicate"],
        vec!["--branch"],
        vec!["a", "b"],
        vec!["--status", "some-origin"],
        vec!["--rollback", "--branch", "main"],
        vec!["--branch", "bad..name", "notaurl"],
        vec!["notaurl"],
    ] {
        let (_, converged) = check(&twins, &argv, None);
        assert_eq!(converged, 0, "gated rows never converge: {argv:?}");
    }
    let (rust, _) = check(&twins, &["--frobnicate"], None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: unknown option: --frobnicate\n".to_vec()
    );
}

#[test]
fn skip_provider_gate_matches_shell() {
    let twins = Twins::build("engine-skip-gate");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    // Only `0` and `1` pass; anything else fails before identity.
    // The mode dispatch runs first, so `--status` never consults
    // the gate.
    let (rust, converged) = check(&twins, &["--branch", "main", url.as_str()], Some("2"));
    assert_eq!(rust.code, 2);
    assert_eq!(
        rust.stderr,
        b"dot init: DOT_INIT_SKIP_PROVIDER must be 0 or 1\n".to_vec()
    );
    assert_eq!(converged, 0);
    let (rust, _) = check(&twins, &["--status"], Some("2"));
    assert_eq!(rust.code, 0);
    // Empty counts as unset: the gate passes and the explicit
    // branch plus a missing origin then fail at the clone, far
    // past the gate, on both engines.
    let missing = format!("file://{}/nope.git", path_str(twins.root()));
    let argv = ["--branch", "main", missing.as_str()];
    // `check` already proved byte parity (the clone's own fatal
    // text travels in both reports); only the code and the
    // boundary remain to pin here.
    let (rust, converged) = check(&twins, &argv, Some(""));
    assert_eq!(rust.code, 1);
    assert_eq!(converged, 0, "failed clone never converges");
}

#[test]
fn fresh_clone_failure_matches_shell() {
    let twins = Twins::build("engine-fresh-clone");
    // A well-formed `file://` URL with no repository behind it:
    // identity resolves, the explicit branch skips the probe, and
    // the candidate clone fails on both engines, carrying git's
    // own fatal text in both reports.
    let missing = format!("file://{}/nope.git", path_str(twins.root()));
    let argv = ["--branch", "main", missing.as_str()];
    let (rust, converged) = check(&twins, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(converged, 0, "failed clone never converges");
    for state in [&twins.shell_state, &twins.rust_state] {
        let init = state.join("dot/init");
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
}

#[test]
fn fresh_conflicts_require_yes_noninteractive() {
    if tty_available() {
        eprintln!("SKIP: /dev/tty is present; the confirm prompt would block");
        return;
    }
    let twins = Twins::build("engine-fresh-conflicts");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    // A live path colliding with the candidate tree: both engines
    // print the plan, then the conflict listing, then the
    // noninteractive diagnostic. Backup stamps and twin roots are
    // normalized before comparing.
    for home in [&twins.shell_home, &twins.rust_home] {
        write(home, ".testrc", b"local work\n");
    }
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = production_run(
        &twins.rust_home,
        &twins.rust_state,
        twins.root(),
        None,
        &argv,
        true,
    );
    let (code, shell_out, shell_err) = oracle(
        &argv,
        &twins.shell_home,
        &twins.shell_state,
        None,
        twins.root(),
        "0",
    );
    assert_eq!((rust.code, code), (1, 1));
    assert!(rust.stdout.is_empty() && shell_out.is_empty());
    // The common prefix (plan plus conflict listing) compares
    // exactly after normalization. The tails diverge by terminal
    // shape, not logic: without a controlling terminal the shell's
    // access-based tty probe passes and its prompt redirects fail
    // with bash's own redirect errors, while the port's open-based
    // probe refuses with the noninteractive diagnostic (both exit
    // 1). With a controlling terminal both sides would prompt, so
    // that shape stays skipped above.
    let rust_err = normalize(&rust.stderr, twins.root());
    let shell_err = normalize(&shell_err, twins.root());
    let listing = b"dot init: conflicting paths will be backed up:\n  .testrc\n";
    let prefix = &shell_err[..shell_err.len().min(rust_err.len())];
    let end = prefix
        .windows(listing.len())
        .position(|window| window == listing)
        .map(|at| at + listing.len())
        .expect("conflict listing printed on both engines");
    let common = &prefix[..end];
    assert_eq!(&rust_err[..common.len()], common, "plan plus listing agree");
    assert!(
        rust_err.ends_with(b"dot init: conflicts require --yes in a noninteractive session\n"),
        "port diagnostic present"
    );
    let tail = &shell_err[common.len()..];
    let needle = b"/dev/tty";
    assert!(
        tail.windows(needle.len()).any(|window| window == needle),
        "shell tail is tty redirect noise, not logic drift: {tail:?}"
    );
    assert_eq!(converged, 0, "refused confirm never converges");
}

#[test]
fn fresh_yes_reaches_converge_structurally() {
    let twins = Twins::build("engine-fresh-yes");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    for home in [&twins.shell_home, &twins.rust_home] {
        write(home, ".testrc", b"local work\n");
    }
    let argv = ["--yes", "--branch", "main", url.as_str()];
    // Both converge stubs succeed silently, so the plan on stderr
    // matches after normalization and stdout stays empty on both.
    let (rust, converged, (code, _, shell_err, log)) = check_converge(&twins, &argv, None);
    assert_eq!((rust.code, code), (0, 0));
    assert!(rust.stdout.is_empty());
    assert_eq!(
        normalize(&rust.stderr, twins.root()),
        normalize(&shell_err, twins.root()),
        "plan report"
    );
    assert_eq!(converged, 1, "fresh success converges once");
    assert_eq!(log, b"converged\n".to_vec(), "shell converges once");
    for state in [&twins.shell_state, &twins.rust_state] {
        assert!(
            !transaction_dir(state).exists(),
            "transaction removed at {}",
            state.display()
        );
    }
    assert_eq!(
        normalize_journal(&read_completed(&twins.shell_state), twins.root()),
        normalize_journal(&read_completed(&twins.rust_state), twins.root()),
        "completion records agree across engines"
    );
}

#[test]
fn adopt_mismatch_matches_shell() {
    let twins = Twins::build("engine-adopt-mismatch");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let other = twins.root().join("other.git");
    git(&[
        "clone",
        "--quiet",
        "--bare",
        path_str(&origin),
        path_str(&other),
    ]);
    let other_url = format!("file://{}", path_str(&other));
    // An ordinary `$HOME/.git` checkout tracking another origin:
    // adoption refuses with code 2 mapped to the mismatch
    // diagnostic on both engines, before any convergence.
    for home in [&twins.shell_home, &twins.rust_home] {
        git(&["init", "--quiet", path_str(home)]);
        git(&["-C", path_str(home), "remote", "add", "origin", &other_url]);
    }
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = check(&twins, &argv, None);
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
fn completed_record_gates_match_shell() {
    let twins = Twins::build("engine-completed-gates");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    // A completed record for another identity: the rerun refuses
    // before touching the live checkout.
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        let completed = state.join("dot/init/completed");
        std::fs::create_dir_all(completed.parent().expect("completed parent"))
            .expect("completed dir");
        shell_write_record(
            home,
            state,
            &completed,
            "complete",
            "file:///elsewhere.git",
            "elsewhere-identity",
            "main",
            "-",
            &format!("{}/.dotfiles", path_str(home)),
        );
    }
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = check(&twins, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: initialized client belongs to a different repository or branch\n".to_vec()
    );
    assert_eq!(converged, 0);
    let _ = identity;
}

#[test]
fn transaction_identity_mismatch_matches_shell() {
    let twins = Twins::build("engine-transaction-mismatch");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        plant_transaction(
            home,
            state,
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
    }
    let argv = ["--branch", "main", url.as_str()];
    let (rust, converged) = check(&twins, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: existing transaction belongs to a different repository or branch\n".to_vec()
    );
    assert_eq!(converged, 0, "rejected transaction never resumes");
}

#[test]
fn completed_rerun_converges_structurally() {
    let twins = Twins::build("engine-rerun");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin));
    let identity = shell_identity(&url);
    plant_live_checkout(&twins, &url, &identity, "main", "complete");
    // Promote the planted transaction journals to completion
    // records, as a first successful run would leave them.
    for state in [&twins.shell_state, &twins.rust_state] {
        let transaction = transaction_dir(state);
        let completed = state.join("dot/init/completed");
        std::fs::rename(transaction.join("record"), &completed).expect("promote");
        std::fs::remove_dir_all(&transaction).expect("drop transaction");
    }
    // A rerun with no transaction takes the completed-file branch:
    // the live checkout still matches, so both engines converge.
    // Both stubs stay silent, so streams compare exactly too.
    let argv = [url.as_str()];
    let (rust, converged, (code, _, _, log)) = check_converge(&twins, &argv, None);
    assert_eq!((rust.code, code), (0, 0));
    assert!(rust.stdout.is_empty());
    assert!(rust.stderr.is_empty());
    assert_eq!(converged, 1, "completed rerun converges once");
    assert_eq!(log, b"converged\n".to_vec(), "shell converges once");
    assert_eq!(
        normalize_journal(&read_completed(&twins.shell_state), twins.root()),
        normalize_journal(&read_completed(&twins.rust_state), twins.root()),
        "completion records untouched on both engines"
    );
}

/// Write one journal record with explicit commit, nonce, and device
/// identity (the live-git fixtures need the real values).
#[allow(clippy::too_many_arguments)]
fn shell_write_record_full(
    home: &Path,
    state: &Path,
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
    let output = shell_eval(
        path_str(home),
        home,
        state,
        &[
            ("DOT_INIT_COMMIT", commit),
            ("DOT_INIT_NONCE", nonce),
            ("DOT_INIT_GIT_DEV", dev),
            ("DOT_INIT_GIT_INO", ino),
        ],
        &format!(
            "_dot_init_write_record {} {} {} {} {} {} {}\ncode=$?\nexit \"$code\"\n",
            sq(path_str(dest)),
            sq(phase),
            sq(origin),
            sq(identity),
            sq(branch),
            sq(backup),
            sq(git_dir),
        ),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "write fixture record at {}",
        dest.display()
    );
}
