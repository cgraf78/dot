//! Differential parity tests for `dot_init_command`
//! (`lib/dot/init-client.sh` lines 1789-1967, first half) against the
//! live shell: argument parsing, `--help`, unknown options, the
//! `--status` / `--rollback` mode gates, the `DOT_INIT_SKIP_PROVIDER`
//! gate, identity/branch resolution, and the transaction
//! recover/resume sequencing.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home and state directories,
//! so journals and transactions never collide. Effect-free helpers
//! (`usage`, `repo_identity`, `branch_valid`, the transaction-dir
//! derivations, stage recovery, record reads) run as the REAL Rust
//! ports on the Rust side — comparing against the shell oracle on
//! identical fixtures is the parity check, no shim needed. Only the
//! network step (`remote_default_branch`), the deep resume/rollback
//! trees, and the not-yet-ported fresh tail cross as closures: the
//! first two run the LIVE shell functions in the Rust twin home, the
//! tail is a recording stub (the shell has no comparable stopping
//! point there). Rows that would enter the fresh tail on the shell
//! side are stub-only by design.
//!
//! Scope boundary: this lane ports `dot_init_command` through the
//! live-transaction resume (`return 0` at line 1870). Everything
//! from the completed-file branch on (lines 1872+) arrives through
//! the `fresh` continuation, owned by a later slice.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::errors::Error;
use dot::init_client_command as cmd;
use dot::init_client_record::TransactionRecord;
use dot::test_support::{TempDir, bash};

/// Sources for the command oracle: the resource runtime, the shared
/// temp helpers, the XDG root, and the init client itself. The
/// repository model is deliberately absent: `dot_init_command` never
/// consults it, and model.sh runs client selection (with its own
/// diagnostics) at source time, which would pollute the streams.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the command runtime sourced, in engine
/// mode: the snippet body runs inside `( set -euo pipefail; ... )`
/// because production (`bin/dot`, `lib/dot/main.sh`) always does.
/// Without it the harness would observe continuations production never
/// reaches (a second rollback diagnostic, a dead `return 2`). The
/// snippet's trailing `exit` carries the subshell verdict out, so the
/// process status still reports the command while streams stay pure.
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
        .arg(format!("{SOURCES}( set -euo pipefail\n{snippet}\n)"));
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
    let output = shell_eval(
        path_str(home),
        home,
        state,
        &[
            ("DOT_INIT_COMMIT", &"a".repeat(40)),
            ("DOT_INIT_NONCE", "n1"),
            ("DOT_INIT_GIT_DEV", "7"),
            ("DOT_INIT_GIT_INO", "8"),
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

/// Calls observed through the test engine: deep-step invocations the
/// rows assert on, plus the fresh-tail inputs.
#[derive(Default)]
struct Calls {
    remote_default_branch: usize,
    resume: usize,
    rollback: usize,
    fresh: Vec<cmd::FreshInputs>,
}

/// Map a shell step's `(code, stderr)` to the engine's
/// `Result<(), Error>`: the command module renders `Usage` payloads
/// as `dot init: {message}` diagnostics, so the shim recovers the
/// closed diagnostic set from the oracle bytes. Anything outside the
/// set fails LOUD, never silently.
fn usage_static(text: &str) -> &'static str {
    match text {
        "no recoverable transaction" => "no recoverable transaction",
        "checkout is committed; rerun the original init command to resume" => {
            "checkout is committed; rerun the original init command to resume"
        }
        "transaction-owned paths changed; refusing rollback" => {
            "transaction-owned paths changed; refusing rollback"
        }
        "initialization transaction could not be resumed safely" => {
            "initialization transaction could not be resumed safely"
        }
        _ => panic!("unexpected shell diagnostic: {text:?}"),
    }
}

/// Export every journal field of `record` as `DOT_INIT_*`, the way
/// the shell carries a just-read record in globals for the resume
/// and rollback steps.
fn record_env(record: &TransactionRecord) -> Vec<(String, String)> {
    vec![
        ("DOT_INIT_PHASE".to_string(), record.phase.clone()),
        ("DOT_INIT_ORIGIN".to_string(), record.origin.clone()),
        ("DOT_INIT_IDENTITY".to_string(), record.identity.clone()),
        ("DOT_INIT_BRANCH".to_string(), record.branch.clone()),
        ("DOT_INIT_COMMIT".to_string(), record.commit.clone()),
        ("DOT_INIT_GIT_DIR".to_string(), record.git_dir.clone()),
        ("DOT_INIT_WORKTREE".to_string(), record.worktree.clone()),
        ("DOT_INIT_BACKUP".to_string(), record.backup.clone()),
        ("DOT_INIT_DOT".to_string(), record.dot.clone()),
        (
            "DOT_INIT_DOT_REVISION".to_string(),
            record.dot_revision.clone(),
        ),
        ("DOT_INIT_NONCE".to_string(), record.nonce.clone()),
        ("DOT_INIT_GIT_DEV".to_string(), record.git_dev.clone()),
        ("DOT_INIT_GIT_INO".to_string(), record.git_ino.clone()),
    ]
}

/// Test engine: effect-free helpers run as the real Rust ports
/// (inside the module under test); only the network step, the deep
/// resume/rollback trees, and the fresh tail cross as closures. The
/// first three run the LIVE shell functions in the Rust twin home;
/// the tail records its inputs and returns a marker report.
/// Live-shell closure types for the test engine: one per deep step
/// the engine injects (each runs the real shell function in the Rust
/// twin home, except the recording fresh tail).
type RemoteDefaultBranch<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;
/// Live `_dot_init_resume_transaction` in the Rust twin home.
type ResumeStep<'a> = Box<dyn Fn(&Path, &Path, &TransactionRecord) -> Result<(), Error> + 'a>;
/// Live `_dot_init_rollback` in the Rust twin home.
type RollbackStep<'a> = Box<dyn Fn(&Path) -> Result<(), Error> + 'a>;
/// Recording stub for the not-yet-ported fresh tail.
type FreshTail<'a> = Box<dyn Fn(&cmd::FreshInputs) -> cmd::InitReport + 'a>;

struct Engine<'a> {
    home: &'a Path,
    state: &'a Path,
    remote_default_branch: RemoteDefaultBranch<'a>,
    resume: ResumeStep<'a>,
    rollback: RollbackStep<'a>,
    fresh: FreshTail<'a>,
}

impl<'a> Engine<'a> {
    fn build(calls: &'a RefCell<Calls>, home: &'a Path, state: &'a Path) -> Self {
        let remote_default_branch = {
            Box::new(move |url: &str| -> Option<String> {
                calls.borrow_mut().remote_default_branch += 1;
                let output = shell_eval(
                    path_str(home),
                    home,
                    state,
                    &[],
                    &format!(
                        "out=$(_dot_init_remote_default_branch {})\ncode=$?\nprintf '%s' \"$out\"\nexit \"$code\"\n",
                        sq(url),
                    ),
                );
                if output.status.code() == Some(0) {
                    String::from_utf8(output.stdout).ok()
                } else {
                    None
                }
            }) as RemoteDefaultBranch<'a>
        };
        let resume = {
            Box::new(
                move |transaction: &Path,
                      record: &Path,
                      journal: &TransactionRecord|
                      -> Result<(), Error> {
                    calls.borrow_mut().resume += 1;
                    let env = record_env(journal);
                    let refs: Vec<(&str, &str)> = env
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str()))
                        .collect();
                    let output = shell_eval(
                        path_str(home),
                        home,
                        state,
                        &refs,
                        &format!(
                            "_dot_init_resume_transaction {} {}\ncode=$?\nexit \"$code\"\n",
                            sq(path_str(transaction)),
                            sq(path_str(record)),
                        ),
                    );
                    if output.status.code() == Some(0) {
                        Ok(())
                    } else {
                        Err(Error::Usage {
                            message: "initialization transaction could not be resumed safely",
                        })
                    }
                },
            ) as ResumeStep<'a>
        };
        let rollback = {
            Box::new(move |at: &Path| -> Result<(), Error> {
                calls.borrow_mut().rollback += 1;
                assert_eq!(at, home, "rollback runs at the client home");
                let output = shell_eval(
                    path_str(home),
                    home,
                    state,
                    &[],
                    "_dot_init_rollback\ncode=$?\nexit \"$code\"\n",
                );
                if output.status.code() == Some(0) {
                    assert!(
                        output.stderr.is_empty(),
                        "silent rollback success: {output:?}"
                    );
                    Ok(())
                } else {
                    let text =
                        String::from_utf8(output.stderr.clone()).expect("rollback stderr UTF-8");
                    match text
                        .strip_prefix("dot init: ")
                        .and_then(|rest| rest.strip_suffix('\n'))
                    {
                        Some(message) => Err(Error::Usage {
                            message: usage_static(message),
                        }),
                        None => panic!("rollback stderr is not a dot-init diagnostic: {text:?}"),
                    }
                }
            }) as RollbackStep<'a>
        };
        let fresh = {
            Box::new(move |inputs: &cmd::FreshInputs| -> cmd::InitReport {
                calls.borrow_mut().fresh.push(inputs.clone());
                cmd::InitReport {
                    stdout: b"fresh\n".to_vec(),
                    stderr: Vec::new(),
                    code: 0,
                }
            }) as FreshTail<'a>
        };
        Self {
            home,
            state,
            remote_default_branch,
            resume,
            rollback,
            fresh,
        }
    }

    fn command(&self) -> cmd::CommandEngine<'_> {
        cmd::CommandEngine {
            remote_default_branch: &self.remote_default_branch,
            resume: &self.resume,
            rollback: &self.rollback,
            fresh: &self.fresh,
        }
    }

    /// [`cmd::CommandEnv`] for one twin side. `skip_provider`
    /// `None` is an unset variable (the shell's `:-0` default);
    /// `source_root` is the checkout, like production.
    fn env<'b>(&self, skip_provider: Option<&'b str>) -> cmd::CommandEnv<'b>
    where
        'a: 'b,
    {
        cmd::CommandEnv {
            home: path_str(self.home),
            xdg_state_home: path_str(self.state),
            skip_provider,
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        }
    }

    /// Run the Rust command on `argv` (plain words; byte-exactness
    /// for non-UTF8 spellings is pinned in the module unit tests).
    fn rust(&self, argv: &[&str], skip_provider: Option<&str>) -> cmd::InitReport {
        let bytes: Vec<Vec<u8>> = argv.iter().map(|word| word.as_bytes().to_vec()).collect();
        cmd::run(&self.env(skip_provider), &self.command(), &bytes)
    }
}

/// Run the live `dot_init_command` oracle on `argv` with
/// `DOT_INIT_SKIP_PROVIDER` optionally set. Returns
/// (exit code, stdout, stderr); the snippet carries the verdict in
/// the process status so the streams stay byte-pure.
fn oracle(
    argv: &[&str],
    home: &Path,
    state: &Path,
    skip_provider: Option<&str>,
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut snippet = String::from("dot_init_command");
    for word in argv {
        snippet.push_str(&format!(" {}", sq(word)));
    }
    snippet.push_str("\ncode=$?\nexit \"$code\"\n");
    let extra: Vec<(&str, &str)> = skip_provider
        .map(|value| vec![("DOT_INIT_SKIP_PROVIDER", value)])
        .unwrap_or_default();
    let output = shell_eval(path_str(home), home, state, &extra, &snippet);
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// One differential row: both engines see identical argv, homes
/// (per-side fixtures), and `DOT_INIT_SKIP_PROVIDER`. Streams and
/// codes must agree byte for byte; the deep steps must fire the
/// same number of times on the Rust side.
fn check(
    twins: &Twins,
    rust_engine: &Engine<'_>,
    argv: &[&str],
    skip_provider: Option<&str>,
) -> cmd::InitReport {
    let rust = rust_engine.rust(argv, skip_provider);
    let (code, stdout, stderr) = oracle(argv, &twins.shell_home, &twins.shell_state, skip_provider);
    assert_eq!(rust.code, code, "argv: {argv:?}");
    assert_eq!(rust.stdout, stdout, "argv: {argv:?}");
    assert_eq!(rust.stderr, stderr, "argv: {argv:?}");
    rust
}

/// Usage bytes shared by `--help` (stdout, code 0) and the missing
/// origin (stderr, code 2).
fn usage() -> Vec<u8> {
    dot::init_client_adopt::usage()
}

#[test]
fn help_flags_print_usage_and_ignore_the_rest() {
    let twins = Twins::build("init-command-help");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    for argv in [
        vec!["--help"],
        vec!["-h"],
        vec!["--help", "--yes", "foo", "--status"],
        vec!["origin-url", "--help"],
    ] {
        let rust = engine.rust(&argv, None);
        let (code, stdout, stderr) = oracle(&argv, &twins.shell_home, &twins.shell_state, None);
        assert_eq!((rust.code, code), (0, 0), "argv: {argv:?}");
        assert_eq!(rust.stdout, usage(), "argv: {argv:?}");
        assert_eq!(stdout, usage(), "argv: {argv:?}");
        assert!(rust.stderr.is_empty(), "argv: {argv:?}");
        assert!(stderr.is_empty(), "argv: {argv:?}");
    }
    let seen = calls.borrow();
    assert_eq!(seen.remote_default_branch, 0);
    assert_eq!(seen.resume, 0);
    assert_eq!(seen.rollback, 0);
    assert!(seen.fresh.is_empty());
}

#[test]
fn unknown_options_fail_with_the_spelling() {
    let twins = Twins::build("init-command-unknown");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    // `--branch=x` takes no special case: the shell only matches the
    // bare `--branch` word, so the joined form is unknown too. A bare
    // `--` is an unknown option as well, never an end-of-flags.
    // Production runs under `set -euo pipefail`, so the shell exits
    // inside `_dot_init_error` with `1` before its trailing
    // `return 2` ever runs (pinned against `bin/dot`).
    for option in ["--frobnicate", "--branch=x", "--", "--YES"] {
        let argv = [option];
        let rust = check(&twins, &engine, &argv, None);
        assert_eq!(rust.code, 1, "option: {option}");
        assert!(rust.stdout.is_empty(), "option: {option}");
        assert_eq!(
            rust.stderr,
            format!("dot init: unknown option: {option}\n").into_bytes(),
            "option: {option}"
        );
    }
}

#[test]
fn branch_without_a_value_and_double_origins_are_silent_usage_errors() {
    let twins = Twins::build("init-command-arity");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    for argv in [
        vec!["--branch"],
        vec!["--yes", "--branch"],
        vec!["a", "b"],
        vec!["--yes", "a", "b"],
        vec!["a", ""],
    ] {
        let rust = engine.rust(&argv, None);
        let (code, stdout, stderr) = oracle(&argv, &twins.shell_home, &twins.shell_state, None);
        assert_eq!((rust.code, code), (2, 2), "argv: {argv:?}");
        assert!(rust.stdout.is_empty(), "argv: {argv:?}");
        assert!(stdout.is_empty(), "argv: {argv:?}");
        assert!(rust.stderr.is_empty(), "argv: {argv:?}");
        assert!(stderr.is_empty(), "argv: {argv:?}");
    }
}

#[test]
fn status_and_rollback_reject_origins_and_branches() {
    let twins = Twins::build("init-command-mode-gate");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    for argv in [
        vec!["--status", "some-origin"],
        vec!["--rollback", "some-origin"],
        vec!["--status", "--branch", "main"],
        vec!["--rollback", "--branch", "main"],
    ] {
        let rust = check(&twins, &engine, &argv, None);
        assert_eq!(rust.code, 2, "argv: {argv:?}");
        assert!(rust.stdout.is_empty(), "argv: {argv:?}");
        assert!(rust.stderr.is_empty(), "argv: {argv:?}");
    }
    // Repeated mode flags are harmless: no origin or branch binds.
    let rust = check(&twins, &engine, &["--status", "--status"], None);
    assert_eq!(
        rust.stdout,
        b"initialization: not started\n".to_vec(),
        "repeated --status"
    );
    let rust = check(&twins, &engine, &["--rollback", "--rollback"], None);
    assert_eq!(rust.code, 1, "repeated --rollback");
    assert_eq!(
        rust.stderr,
        b"dot init: no recoverable transaction\n".to_vec(),
        "repeated --rollback"
    );
    let seen = calls.borrow();
    assert_eq!(seen.rollback, 1, "repeated --rollback runs once");
}

#[test]
fn missing_origin_prints_usage_to_stderr() {
    let twins = Twins::build("init-command-no-origin");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    for argv in [vec![], vec!["--yes"], vec!["--branch", "main"]] {
        let rust = engine.rust(&argv, None);
        let (code, stdout, stderr) = oracle(&argv, &twins.shell_home, &twins.shell_state, None);
        assert_eq!((rust.code, code), (2, 2), "argv: {argv:?}");
        assert!(rust.stdout.is_empty(), "argv: {argv:?}");
        assert!(stdout.is_empty(), "argv: {argv:?}");
        assert_eq!(rust.stderr, usage(), "argv: {argv:?}");
        assert_eq!(stderr, usage(), "argv: {argv:?}");
    }
}

#[test]
fn skip_provider_gate_runs_before_identity_but_after_modes() {
    let twins = Twins::build("init-command-skip");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let origin = format!("file://{}", path_str(&twins.root().join("origin.git")));
    // Only `0` and `1` pass: anything else fails before identity,
    // even though the origin and branch are otherwise fine.
    for skip in ["2", "01", " 1"] {
        let argv = ["--branch", "main", origin.as_str()];
        let rust = check(&twins, &engine, &argv, Some(skip));
        assert_eq!(rust.code, 2, "skip: {skip:?}");
        assert!(rust.stdout.is_empty(), "skip: {skip:?}");
        assert_eq!(
            rust.stderr,
            b"dot init: DOT_INIT_SKIP_PROVIDER must be 0 or 1\n".to_vec(),
            "skip: {skip:?}"
        );
    }
    // Empty counts as unset (the shell's `:-0` null-default): the
    // gate passes and an unresolvable origin then fails at identity,
    // never reaching the fresh tail on either side.
    let argv = ["notaurl"];
    let rust = check(&twins, &engine, &argv, Some(""));
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: unsupported repository URL: notaurl\n".to_vec()
    );
    // The mode dispatch runs first: `--status` and `--rollback`
    // never consult the gate.
    let rust = engine.rust(&["--status"], Some("2"));
    let (code, stdout, stderr) = oracle(
        &["--status"],
        &twins.shell_home,
        &twins.shell_state,
        Some("2"),
    );
    assert_eq!((rust.code, code), (0, 0));
    assert_eq!(rust.stdout, stdout);
    assert_eq!(rust.stdout, b"initialization: not started\n".to_vec());
    assert!(rust.stderr.is_empty());
    assert!(stderr.is_empty());
    // `1` passes the gate: an unresolvable origin then fails at
    // identity, proving the gate let it through.
    let argv = ["notaurl"];
    let rust = check(&twins, &engine, &argv, Some("1"));
    assert_eq!(rust.code, 1);
    assert_eq!(
        rust.stderr,
        b"dot init: unsupported repository URL: notaurl\n".to_vec()
    );
}

#[test]
fn unsupported_origins_fail_with_the_spelling() {
    let twins = Twins::build("init-command-identity");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    for origin in ["notaurl", "http://", "ssh://host-only"] {
        let argv = [origin];
        let rust = check(&twins, &engine, &argv, None);
        assert_eq!(rust.code, 1, "origin: {origin}");
        assert!(rust.stdout.is_empty(), "origin: {origin}");
        assert_eq!(
            rust.stderr,
            format!("dot init: unsupported repository URL: {origin}\n").into_bytes(),
            "origin: {origin}"
        );
    }
    let seen = calls.borrow();
    assert_eq!(seen.remote_default_branch, 0);
}

#[test]
fn invalid_branches_fail_with_the_spelling() {
    let twins = Twins::build("init-command-branch");
    let origin = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin));
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    // An empty `--branch` value is absence, not a name: the shell
    // resolves the remote default instead. That path needs the
    // network step, so it stays stub-covered (see
    // `default_branch_plumbing_and_fresh_continuation`), never
    // differential.
    let branch = "bad..name";
    let argv = ["--branch", branch, origin.as_str()];
    let rust = check(&twins, &engine, &argv, None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        format!("dot init: invalid branch: {branch}\n").into_bytes()
    );
}

#[test]
fn branch_takes_the_next_word_verbatim() {
    // `--branch` consumes `$2` unconditionally: a flag-looking word
    // becomes the branch name, and `git check-ref-format` rejects it
    // on both engines alike.
    let twins = Twins::build("init-command-branch-trap");
    let origin = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin));
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let argv = ["--branch", "--yes", origin.as_str()];
    let rust = check(&twins, &engine, &argv, None);
    assert_eq!(rust.code, 1);
    assert_eq!(rust.stderr, b"dot init: invalid branch: --yes\n".to_vec());
}

/// Plant the same journal in both homes: `origin`/`identity` shared,
/// `git_dir` pinned per home (the journal binds its own client
/// root), writing through the live shell writer.
fn plant_record(twins: &Twins, dest: &str, phase: &str, origin: &str, identity: &str) {
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        let state_root = state.join("dot/init");
        let dest = state_root.join(dest);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).expect("fixture parents");
        }
        shell_write_record(
            home,
            state,
            &dest,
            phase,
            origin,
            identity,
            "main",
            "-",
            &format!("{}/.dotfiles", path_str(home)),
        );
    }
}

#[test]
fn status_reports_both_journals() {
    let twins = Twins::build("init-command-status");
    let origin_path = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin_path));
    let identity = shell_identity(&origin);
    plant_record(&twins, "transaction/record", "prepared", &origin, &identity);
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let argv = ["--status"];
    let rust = check(&twins, &engine, &argv, None);
    assert_eq!(rust.code, 0);
    assert_eq!(
        rust.stdout,
        format!(
            "initialization: incomplete\nphase: prepared\norigin: {origin}\nbranch: main\nbackup: -\n"
        )
        .into_bytes()
    );
    assert!(rust.stderr.is_empty());

    // A completion record reports `complete` with origin and branch.
    let twins = Twins::build("init-command-status-done");
    plant_record(&twins, "completed", "complete", &origin, &identity);
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let rust = check(&twins, &engine, &argv, None);
    assert_eq!(rust.code, 0);
    assert_eq!(
        rust.stdout,
        format!("initialization: complete\norigin: {origin}\nbranch: main\n").into_bytes()
    );
    assert!(rust.stderr.is_empty());
}

#[test]
fn status_names_malformed_journals_per_home() {
    let twins = Twins::build("init-command-status-bad");
    for state in [&twins.shell_state, &twins.rust_state] {
        let dir = state.join("dot/init/transaction");
        std::fs::create_dir_all(&dir).expect("fixture parents");
        std::fs::write(dir.join("record"), b"garbage\n").expect("garbage record");
    }
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let rust = engine.rust(&["--status"], None);
    let (code, stdout, stderr) = oracle(&["--status"], &twins.shell_home, &twins.shell_state, None);
    assert_eq!((rust.code, code), (1, 1));
    assert!(rust.stdout.is_empty());
    assert!(stdout.is_empty());
    for (report, state) in [
        (&rust.stderr, &twins.rust_state),
        (&stderr, &twins.shell_state),
    ] {
        let expected = format!(
            "dot init: malformed initialization transaction: {}/dot/init/transaction\n",
            path_str(state),
        );
        assert_eq!(*report, expected.into_bytes());
    }

    let twins = Twins::build("init-command-status-bad-done");
    for state in [&twins.shell_state, &twins.rust_state] {
        let dir = state.join("dot/init");
        std::fs::create_dir_all(&dir).expect("fixture parents");
        std::fs::write(dir.join("completed"), b"garbage\n").expect("garbage record");
    }
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let rust = engine.rust(&["--status"], None);
    let (code, stdout, stderr) = oracle(&["--status"], &twins.shell_home, &twins.shell_state, None);
    assert_eq!((rust.code, code), (1, 1));
    assert!(rust.stdout.is_empty());
    assert!(stdout.is_empty());
    for (report, state) in [
        (&rust.stderr, &twins.rust_state),
        (&stderr, &twins.shell_state),
    ] {
        let expected = format!(
            "dot init: malformed completion record: {}/dot/init/completed\n",
            path_str(state),
        );
        assert_eq!(*report, expected.into_bytes());
    }
}

#[test]
fn rollback_without_a_transaction_matches() {
    let twins = Twins::build("init-command-rollback-empty");
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let rust = check(&twins, &engine, &["--rollback"], None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: no recoverable transaction\n".to_vec()
    );
    assert_eq!(calls.borrow().rollback, 1);
}

#[test]
fn rollback_of_a_committed_transaction_refuses() {
    let twins = Twins::build("init-command-rollback-done");
    let origin_path = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin_path));
    let identity = shell_identity(&origin);
    plant_record(&twins, "transaction/record", "complete", &origin, &identity);
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let rust = check(&twins, &engine, &["--rollback"], None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: checkout is committed; rerun the original init command to resume\n".to_vec()
    );
}

#[test]
fn resume_runs_only_for_matching_live_transactions() {
    let twins = Twins::build("init-command-resume");
    let origin_path = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin_path));
    let identity = shell_identity(&origin);
    // A bare `prepared` record has no journals, so the live resume
    // fails on both engines with the fixed diagnostic — while still
    // proving the sequencing (recover, read, match, resume) fires.
    plant_record(&twins, "transaction/record", "prepared", &origin, &identity);
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let argv = [origin.as_str(), "--branch", "main"];
    let rust = check(&twins, &engine, &argv, None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: initialization transaction could not be resumed safely\n".to_vec()
    );
    assert_eq!(calls.borrow().resume, 1);
    assert_eq!(calls.borrow().remote_default_branch, 0);

    // A foreign origin mismatches the planted identity: both engines
    // refuse before resuming.
    let calls = RefCell::new(Calls::default());
    let engine = Engine::build(&calls, &twins.rust_home, &twins.rust_state);
    let foreign = format!("file://{}", path_str(&twins.root().join("seed")));
    let argv = [foreign.as_str(), "--branch", "main"];
    let rust = check(&twins, &engine, &argv, None);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
    assert_eq!(
        rust.stderr,
        b"dot init: existing transaction belongs to a different repository or branch\n".to_vec()
    );
    assert_eq!(calls.borrow().resume, 0);
    // A foreign branch mismatches the same way.
    let argv = [origin.as_str(), "--branch", "other"];
    check(&twins, &engine, &argv, None);
    assert_eq!(calls.borrow().resume, 0);
}

#[test]
fn default_branch_plumbing_and_fresh_continuation() {
    // Stub-only: the shell has no stopping point before the fresh
    // tail, so these rows pin the engine contract instead — the
    // default-branch closure fires exactly when `--branch` is
    // absent, its failure carries the shell diagnostic, and a live
    // transaction-free run reaches `fresh` with the resolved inputs.
    let twins = Twins::build("init-command-fresh");
    let origin_path = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin_path));
    let identity = shell_identity(&origin);
    let calls = RefCell::new(Calls::default());
    let remote_default_branch = |url: &str| -> Option<String> {
        calls.borrow_mut().remote_default_branch += 1;
        assert_eq!(url, origin);
        Some("main".to_string())
    };
    let fresh = |inputs: &cmd::FreshInputs| -> cmd::InitReport {
        calls.borrow_mut().fresh.push(inputs.clone());
        cmd::InitReport {
            stdout: Vec::new(),
            stderr: Vec::new(),
            code: 0,
        }
    };
    let engine = cmd::CommandEngine {
        remote_default_branch: &remote_default_branch,
        resume: &|_, _, _| {
            panic!("no live transaction: resume must not fire");
        },
        rollback: &|_| {
            panic!("run mode: rollback must not fire");
        },
        fresh: &fresh,
    };
    let env = cmd::CommandEnv {
        home: path_str(&twins.rust_home),
        xdg_state_home: path_str(&twins.rust_state),
        skip_provider: None,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let report = cmd::run(
        &env,
        &engine,
        &[b"--yes".to_vec(), origin.as_bytes().to_vec()],
    );
    assert_eq!(report.code, 0);
    let seen = calls.borrow();
    assert_eq!(seen.remote_default_branch, 1);
    assert_eq!(seen.fresh.len(), 1);
    assert_eq!(seen.fresh[0].origin, origin);
    assert_eq!(seen.fresh[0].identity, identity);
    assert_eq!(seen.fresh[0].branch, "main");
    assert!(seen.fresh[0].yes);

    // An explicit `--branch` skips the default lookup; an empty one
    // still counts as absent.
    for argv in [
        vec!["--branch", "main", origin.as_str()],
        vec![origin.as_str()],
    ] {
        let calls = RefCell::new(Calls::default());
        let remote_default_branch = |url: &str| -> Option<String> {
            calls.borrow_mut().remote_default_branch += 1;
            assert_eq!(url, origin);
            Some("main".to_string())
        };
        let fresh = |inputs: &cmd::FreshInputs| -> cmd::InitReport {
            calls.borrow_mut().fresh.push(inputs.clone());
            cmd::InitReport {
                stdout: Vec::new(),
                stderr: Vec::new(),
                code: 0,
            }
        };
        let engine = cmd::CommandEngine {
            remote_default_branch: &remote_default_branch,
            resume: &|_, _, _| {
                panic!("no live transaction: resume must not fire");
            },
            rollback: &|_| {
                panic!("run mode: rollback must not fire");
            },
            fresh: &fresh,
        };
        let bytes: Vec<Vec<u8>> = argv.iter().map(|word| word.as_bytes().to_vec()).collect();
        let report = cmd::run(&env, &engine, &bytes);
        assert_eq!(report.code, 0, "argv: {argv:?}");
        let seen = calls.borrow();
        assert_eq!(seen.fresh.len(), 1, "argv: {argv:?}");
        assert_eq!(seen.fresh[0].branch, "main", "argv: {argv:?}");
        assert!(!seen.fresh[0].yes, "argv: {argv:?}");
        if argv.len() == 1 {
            assert_eq!(seen.remote_default_branch, 1, "argv: {argv:?}");
        } else {
            assert_eq!(seen.remote_default_branch, 0, "argv: {argv:?}");
        }
    }

    // An unresolvable default branch carries the shell diagnostic.
    let calls = RefCell::new(Calls::default());
    let remote_default_branch = |_: &str| -> Option<String> {
        calls.borrow_mut().remote_default_branch += 1;
        None
    };
    let fresh = |_: &cmd::FreshInputs| -> cmd::InitReport {
        panic!("unresolvable branch: fresh must not fire");
    };
    let engine = cmd::CommandEngine {
        remote_default_branch: &remote_default_branch,
        resume: &|_, _, _| {
            panic!("unresolvable branch: resume must not fire");
        },
        rollback: &|_| {
            panic!("run mode: rollback must not fire");
        },
        fresh: &fresh,
    };
    let report = cmd::run(&env, &engine, &[origin.as_bytes().to_vec()]);
    assert_eq!(report.code, 1);
    assert!(report.stdout.is_empty());
    assert_eq!(
        report.stderr,
        b"dot init: could not resolve a non-empty remote default branch\n".to_vec()
    );
    assert!(calls.borrow().fresh.is_empty());
}

#[test]
fn empty_positionals_are_absent_not_origins() {
    // The shell's `[[ -z $origin ]]` absorb-then-reject rule: leading
    // empties vanish, but an empty after an origin is a second
    // positional and fails.
    let twins = Twins::build("init-command-empty");
    let origin_path = make_origin(twins.root());
    let origin = format!("file://{}", path_str(&origin_path));
    let calls = RefCell::new(Calls::default());
    let fresh = |inputs: &cmd::FreshInputs| -> cmd::InitReport {
        calls.borrow_mut().fresh.push(inputs.clone());
        cmd::InitReport {
            stdout: Vec::new(),
            stderr: Vec::new(),
            code: 0,
        }
    };
    let engine = cmd::CommandEngine {
        remote_default_branch: &|_: &str| Some("main".to_string()),
        resume: &|_, _, _| {
            panic!("no live transaction: resume must not fire");
        },
        rollback: &|_| {
            panic!("run mode: rollback must not fire");
        },
        fresh: &fresh,
    };
    let env = cmd::CommandEnv {
        home: path_str(&twins.rust_home),
        xdg_state_home: path_str(&twins.rust_state),
        skip_provider: None,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let report = cmd::run(
        &env,
        &engine,
        &[
            b"".to_vec(),
            b"".to_vec(),
            origin.as_bytes().to_vec(),
            b"--branch".to_vec(),
            b"main".to_vec(),
        ],
    );
    assert_eq!(report.code, 0);
    assert_eq!(calls.borrow().fresh.len(), 1);
    assert_eq!(calls.borrow().fresh[0].origin, origin);
}
