//! Differential parity tests for the init resume chapter
//! (`lib/dot/init-client.sh` lines 1711-1788) against the live shell:
//! the live-git verification (`_dot_init_live_git_matches_record`)
//! and the resume orchestrator (`_dot_init_resume_transaction`).
//!
//! Separate binary because each row drives real filesystem state: the
//! two engines work under disjoint home directories (twin homes under
//! one row temp dir), so git dirs, journals, backups, and completion
//! markers never collide. A shared origin repo per row is read-only to
//! both engines (clones and `realpath` reads), so parallel rows stay
//! isolated.
//!
//! Every row runs the live shell function as oracle and the port with
//! closures that delegate to the live cross-lane helpers
//! (`_dot_path_identity`, `_dot_init_generation_marker_matches`,
//! `_dot_init_repo_identity`, `_dot_init_record_phase`,
//! `_dot_init_move_conflicts`, `_dot_init_stage_git`,
//! `_dot_init_publish_git`, `_dot_init_publish_worktree`,
//! `_dot_init_forward_converge`, `_dot_init_publish_completed`), so the
//! verdict, both byte streams, and the cross-lane call log compare.
//! The closures log each invocation (home prefixes normalized to `~`)
//! and stash the child streams they capture; rows assert oracle code
//! equals port code, oracle residual streams equal stashed streams,
//! and the log equals the expected orchestration sequence, pinning
//! short-circuit order as well as outcomes. Children run with
//! `DOT_INIT_SKIP_PROVIDER=1` and `DOT_QUIET=1` so the converge tail
//! stays silent and deterministic; every row below was probed to emit
//! byte-stable streams before being pinned here.

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_resume as resume;
use dot::test_support::TempDir;

/// Full engine composition, the way production sources it: `init.sh`
/// loads config plus the runtime (init client, update/converge stack).
/// Every probe ends with `printf 'code=%s\n' "$code"`, so the returned
/// code is that verdict, not the process status. A snippet that never
/// reports (a harness bug, never a pass) yields 99.
const SOURCES: &str = ". \"$1/lib/dot/init.sh\" || exit 99\n";

/// Predicate-only composition: the temp helpers plus the init client.
/// No config load, so sourcing never runs client selection — its
/// legacy-shape warnings would otherwise multiply once per closure
/// child while the single-process oracle prints them once (probed).
/// The plan lane sources exactly this set for the same reason.
const SOURCES_MINIMAL: &str = concat!(
    ". \"$1/lib/dot/resources.sh\" || exit 99\n",
    ". \"$1/lib/dot/temp.sh\" || exit 99\n",
    ". \"$1/lib/dot/public/xdg.sh\" || exit 99\n",
    ". \"$1/lib/dot/init-client.sh\" || exit 99\n",
);

/// Fixed nonexistent origin for the clone-failure row: constant across
/// twins so both engines emit identical git diagnostics.
const BOGUS_ORIGIN: &str = "/tmp/dot-resume-no-such-origin";

/// Environment for one child: owned pairs so non-UTF8 paths cross as
/// `OsString` without lossy conversion (paths only ever travel via
/// env, never embedded in snippets).
type Env = Vec<(String, OsString)>;

/// Stash for one row's Rust side: child streams the port's closures
/// capture plus the normalized cross-lane call log.
struct Captured {
    out: RefCell<Vec<u8>>,
    err: RefCell<Vec<u8>>,
    log: RefCell<Vec<String>>,
    valdir: PathBuf,
    next_val: RefCell<u64>,
    phase: RefCell<String>,
    git_dev: RefCell<String>,
    git_ino: RefCell<String>,
}

impl Captured {
    fn build(valdir: PathBuf, phase: &str, git_dev: &str, git_ino: &str) -> Self {
        Self {
            out: RefCell::new(Vec::new()),
            err: RefCell::new(Vec::new()),
            log: RefCell::new(Vec::new()),
            valdir,
            next_val: RefCell::new(0),
            phase: RefCell::new(phase.to_string()),
            git_dev: RefCell::new(git_dev.to_string()),
            git_ino: RefCell::new(git_ino.to_string()),
        }
    }

    fn note(&self, entry: String) {
        self.log.borrow_mut().push(entry);
    }

    fn streams(&self, out: &[u8], err: &[u8]) {
        self.out.borrow_mut().extend_from_slice(out);
        self.err.borrow_mut().extend_from_slice(err);
    }

    fn valfile(&self) -> PathBuf {
        let mut next = self.next_val.borrow_mut();
        let path = self.valdir.join(format!("val-{}", *next));
        *next += 1;
        path
    }
}

/// Replace a leading `home` prefix with `~` so call logs compare
/// across twins.
fn disp(home: &Path, path: &Path) -> String {
    let raw = path.as_os_str().to_string_lossy();
    let prefix = home.as_os_str().to_string_lossy();
    match raw.strip_prefix(prefix.as_ref()) {
        Some(rest) => format!("~{rest}"),
        None => raw.into_owned(),
    }
}

/// Split a child stdout into the reported verdict (last `code=` line,
/// 99 when absent) and the residual bytes with that line removed.
fn extract_verdict(stdout: &[u8]) -> (i32, Vec<u8>) {
    let terminated = stdout.last() == Some(&b'\n');
    let mut parts: Vec<&[u8]> = stdout.split(|byte| *byte == b'\n').collect();
    if terminated {
        parts.pop();
    }
    let mut code = 99;
    let mut at = None;
    for (index, part) in parts.iter().enumerate().rev() {
        let Some(rest) = part.strip_prefix(b"code=") else {
            continue;
        };
        if let Ok(parsed) = std::str::from_utf8(rest).unwrap_or("").parse::<i32>() {
            code = parsed;
            at = Some(index);
            break;
        }
    }
    if let Some(index) = at {
        parts.remove(index);
    }
    let mut residual = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            residual.push(b'\n');
        }
        residual.extend_from_slice(part);
    }
    if terminated && !parts.is_empty() {
        residual.push(b'\n');
    }
    (code, residual)
}

/// Run one shell snippet with the engine sourced and report the
/// verdict alongside the residual streams (verdict line removed).
/// The locale stays pinned and the home steered, like the port pins
/// `LC_ALL=C` around every git run.
fn shell_run(home: &Path, env: &Env, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    shell_run_sources(home, env, SOURCES, snippet)
}

/// Predicate-oracle run with the minimal composition (see
/// `SOURCES_MINIMAL`).
fn shell_run_min(home: &Path, env: &Env, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    shell_run_sources(home, env, SOURCES_MINIMAL, snippet)
}

fn shell_run_sources(
    home: &Path,
    env: &Env,
    sources: &str,
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let output = shell_child_sources(home, env, &[], sources, snippet)
        .output()
        .expect("spawn bash");
    let (code, residual) = extract_verdict(&output.stdout);
    (code, residual, output.stderr)
}

/// Spawn one snippet child; `extra` env augments `env`.
/// Minimal-composition spawn (see `SOURCES_MINIMAL`).
fn shell_child_min(home: &Path, env: &Env, extra: &[(String, OsString)], snippet: &str) -> Command {
    shell_child_sources(home, env, extra, SOURCES_MINIMAL, snippet)
}

fn shell_child_sources(
    home: &Path,
    env: &Env,
    extra: &[(String, OsString)],
    sources: &str,
    snippet: &str,
) -> Command {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{sources}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env.iter().chain(extra.iter()) {
        cmd.env(key, value);
    }
    cmd
}

/// Run a fixture `git` with hooks disabled and a canned identity so
/// the global user config and hook runtime cannot leak into rows.
fn fixture_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args)
        .env("LC_ALL", "C")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn fixture git");
    assert!(
        output.status.success(),
        "fixture git failed: {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Evaluate a snippet for its raw stdout (fixture scaffolding only,
/// never compared): `$()` semantics strip trailing newlines.
fn shell_text(home: &Path, env: &Env, snippet: &str) -> String {
    let output = shell_child_min(home, env, &[], snippet)
        .output()
        .expect("spawn bash");
    assert!(
        output.status.success(),
        "fixture snippet failed: {snippet:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.trim_end_matches('\n').to_string()
}

/// One row's world: twin homes plus the shared read-only origin and
/// the identity the live shell derives from it.
struct World {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    origin: PathBuf,
    commit: String,
    identity: String,
}

impl World {
    fn build(tag: &str) -> Self {
        Self::build_format(tag, false)
    }

    fn build_sha256(tag: &str) -> Self {
        Self::build_format(tag, true)
    }

    fn build_format(tag: &str, sha256: bool) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&shell_home).expect("shell home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        let origin = dir.path().join("origin-src");
        std::fs::create_dir_all(&origin).expect("origin dir");
        let mut init_args = vec!["init", "-q", "-b", "main"];
        if sha256 {
            init_args.push("--object-format=sha256");
        }
        init_args.push(".");
        fixture_git(&origin, &init_args);
        std::fs::write(origin.join("file.txt"), "hello\n").expect("origin file");
        fixture_git(&origin, &["add", "file.txt"]);
        fixture_git(
            &origin,
            &[
                "-c",
                "user.name=dot-test",
                "-c",
                "user.email=dot-test@example",
                "commit",
                "-qm",
                "init",
            ],
        );
        // The origin path crosses via env (paths never embed in
        // snippets).
        let env = vec![("ORIGIN".to_string(), OsString::from(origin.as_os_str()))];
        let commit = shell_text(&shell_home, &env, "git -C \"$ORIGIN\" rev-parse HEAD");
        let identity = shell_text(
            &shell_home,
            &env,
            "identity=$(_dot_init_repo_identity \"$ORIGIN\"); printf '%s' \"$identity\"",
        );
        assert!(!commit.is_empty() && !identity.is_empty());
        Self {
            _dir: dir,
            shell_home,
            rust_home,
            origin,
            commit,
            identity,
        }
    }
}

/// Per-home run context: every value the shell reads from `DOT_INIT_*`
/// plus the paths the resume snippet takes. `dev`/`ino` are `-` when
/// the git dir is absent (the shell's own `${VAR:--}` default); the
/// stage/publish closures refresh them from the child, mirroring the
/// shell's in-process `_dot_init_set_git_identity`.
struct Ctx {
    git_dir: PathBuf,
    dev: String,
    ino: String,
    nonce: String,
    identity: String,
    branch: String,
    origin: PathBuf,
    backup: PathBuf,
    txn: PathBuf,
    record: PathBuf,
}

/// Read `dev:ino` through the live helper (empty when absent).
fn live_identity(home: &Path, git_dir: &Path) -> (String, String) {
    let env = vec![("G".to_string(), OsString::from(git_dir.as_os_str()))];
    let output = shell_child_min(
        home,
        &env,
        &[],
        "identity=$(_dot_path_identity \"$G\"); printf '%s' \"$identity\"",
    )
    .output()
    .expect("spawn bash");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    match text.split_once(':') {
        Some((dev, ino)) if !dev.is_empty() && !ino.is_empty() => {
            (dev.to_string(), ino.to_string())
        }
        _ => ("-".to_string(), "-".to_string()),
    }
}

/// Clone the origin the way `_dot_init_stage_git` does, apply its
/// post-clone config, stamp the generation marker with the run values,
/// and move the repo to `dest`. Returns `dest`.
fn build_staged_git(
    home: &Path,
    origin: &Path,
    commit: &str,
    nonce: &str,
    identity: &str,
    branch: &str,
    dest: &Path,
) -> PathBuf {
    let staged = home.join("staged-tmp");
    let origin_str = origin.to_str().expect("utf8 fixture path");
    fixture_git(
        home,
        &[
            "clone",
            "-q",
            "--bare",
            "--no-hardlinks",
            "--branch",
            branch,
            "--single-branch",
            "--",
            origin_str,
            "staged-tmp",
        ],
    );
    let worktree = home.to_str().expect("utf8 fixture home");
    for (key, value) in [
        ("core.bare", "false"),
        ("core.worktree", worktree),
        ("status.showUntrackedFiles", "no"),
        ("core.fsmonitor", "false"),
        ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
    ] {
        fixture_git(&staged, &["config", key, value]);
    }
    let refspec = format!("refs/heads/{branch}");
    fixture_git(
        &staged,
        &[
            "update-ref",
            &format!("refs/remotes/origin/{branch}"),
            commit,
        ],
    );
    fixture_git(
        &staged,
        &["config", &format!("branch.{branch}.remote"), "origin"],
    );
    fixture_git(
        &staged,
        &["config", &format!("branch.{branch}.merge"), &refspec],
    );
    let env = vec![
        ("G".to_string(), OsString::from(staged.as_os_str())),
        ("DOT_INIT_NONCE".to_string(), OsString::from(nonce)),
        ("DOT_INIT_COMMIT".to_string(), OsString::from(commit)),
        ("DOT_INIT_IDENTITY".to_string(), OsString::from(identity)),
    ];
    let marker_out = shell_child_min(
        home,
        &env,
        &[],
        "_dot_init_write_generation_marker \"$G\"; printf 'code=%s\\n' \"$?\"",
    )
    .output()
    .expect("spawn bash");
    assert!(
        String::from_utf8_lossy(&marker_out.stdout).contains("code=0"),
        "marker write failed: {}",
        String::from_utf8_lossy(&marker_out.stderr)
    );
    std::fs::rename(&staged, dest).expect("move staged git dir");
    dest.to_path_buf()
}

/// Standard `.dotfiles` git dir for `home`.
fn build_dotfiles_git(home: &Path, world: &World, nonce: &str) -> PathBuf {
    build_staged_git(
        home,
        &world.origin,
        &world.commit,
        nonce,
        &world.identity,
        "main",
        &home.join(".dotfiles"),
    )
}

/// Ordinary `$HOME/.git` checkout: init in place, fetch the origin,
/// hard-reset to it (any valid HEAD would do for the predicate, but a
/// true checkout keeps converge quiet too).
fn build_home_git(home: &Path, world: &World, nonce: &str) -> PathBuf {
    let origin_str = world.origin.to_str().expect("utf8 fixture path");
    fixture_git(home, &["init", "-q", "-b", "main", "."]);
    fixture_git(home, &["remote", "add", "origin", origin_str]);
    fixture_git(home, &["fetch", "-q", "origin"]);
    fixture_git(home, &["reset", "-q", "--hard", "origin/main"]);
    let env = vec![
        (
            "G".to_string(),
            OsString::from(home.join(".git").as_os_str()),
        ),
        ("DOT_INIT_NONCE".to_string(), OsString::from(nonce)),
        (
            "DOT_INIT_COMMIT".to_string(),
            OsString::from(world.commit.clone()),
        ),
        (
            "DOT_INIT_IDENTITY".to_string(),
            OsString::from(world.identity.clone()),
        ),
    ];
    let marker_out = shell_child_min(
        home,
        &env,
        &[],
        "_dot_init_write_generation_marker \"$G\"; printf 'code=%s\\n' \"$?\"",
    )
    .output()
    .expect("spawn bash");
    assert!(
        String::from_utf8_lossy(&marker_out.stdout).contains("code=0"),
        "marker write failed"
    );
    home.join(".git")
}

/// Empty transaction journals plus an empty record.
fn build_journals(home: &Path) -> (PathBuf, PathBuf) {
    let txn = home.join("txn");
    std::fs::create_dir_all(&txn).expect("txn dir");
    for name in ["tree.tsv", "prior.tsv", "conflicts.tsv", "record"] {
        std::fs::write(txn.join(name), b"").expect("journal file");
    }
    let record = txn.join("record");
    (txn, record)
}

fn ctx_for(home: &Path, world: &World, git_dir: PathBuf, nonce: &str) -> Ctx {
    let (dev, ino) = live_identity(home, &git_dir);
    let (txn, record) = (home.join("txn"), home.join("txn").join("record"));
    Ctx {
        git_dir,
        dev,
        ino,
        nonce: nonce.to_string(),
        identity: world.identity.clone(),
        branch: "main".to_string(),
        origin: world.origin.clone(),
        backup: home.join(".dot-backup").join("t1"),
        txn,
        record,
    }
}

/// Static env for a home: everything except the threaded triple
/// (`DOT_INIT_PHASE`, `DOT_INIT_GIT_DEV`, `DOT_INIT_GIT_INO`), which
/// the scope appends at spawn time.
fn base_env(ctx: &Ctx, world: &World) -> Env {
    let origin = ctx.origin.clone();
    vec![
        (
            "DOT_INIT_GIT_DIR".to_string(),
            OsString::from(ctx.git_dir.as_os_str()),
        ),
        (
            "DOT_INIT_NONCE".to_string(),
            OsString::from(ctx.nonce.clone()),
        ),
        (
            "DOT_INIT_IDENTITY".to_string(),
            OsString::from(ctx.identity.clone()),
        ),
        (
            "DOT_INIT_BRANCH".to_string(),
            OsString::from(ctx.branch.clone()),
        ),
        (
            "DOT_INIT_COMMIT".to_string(),
            OsString::from(world.commit.clone()),
        ),
        (
            "DOT_INIT_ORIGIN".to_string(),
            OsString::from(origin.as_os_str()),
        ),
        (
            "DOT_INIT_BACKUP".to_string(),
            OsString::from(ctx.backup.as_os_str()),
        ),
        ("DOT_INIT_SKIP_PROVIDER".to_string(), OsString::from("1")),
        ("DOT_QUIET".to_string(), OsString::from("1")),
        ("TXN".to_string(), OsString::from(ctx.txn.as_os_str())),
        ("RECORD".to_string(), OsString::from(ctx.record.as_os_str())),
    ]
}

/// Rust-side row scope: the home, its static env, and the threaded
/// run state. Children always spawn with the CURRENT phase and git
/// identity, mirroring the shell oracle's in-process globals.
struct Scope {
    home: PathBuf,
    base: Env,
    captured: Captured,
}

impl Scope {
    fn build(
        home: &Path,
        valdir: &Path,
        base: Env,
        phase: &str,
        git_dev: &str,
        git_ino: &str,
    ) -> Self {
        // Valfiles live beside the homes, never inside them: stray
        // files under HOME could perturb the engines under test.
        std::fs::create_dir_all(valdir).expect("vals dir");
        Self {
            home: home.to_path_buf(),
            base,
            captured: Captured::build(valdir.to_path_buf(), phase, git_dev, git_ino),
        }
    }

    fn spawn(&self, extra: &[(String, OsString)], snippet: &str) -> std::process::Output {
        self.spawn_sources(extra, SOURCES, snippet)
    }

    /// Minimal-composition child for the predicate closures (see
    /// `SOURCES_MINIMAL`).
    fn spawn_min(&self, extra: &[(String, OsString)], snippet: &str) -> std::process::Output {
        self.spawn_sources(extra, SOURCES_MINIMAL, snippet)
    }

    fn spawn_sources(
        &self,
        extra: &[(String, OsString)],
        sources: &str,
        snippet: &str,
    ) -> std::process::Output {
        let mut full = self.base.clone();
        full.push((
            "DOT_INIT_PHASE".to_string(),
            OsString::from(self.captured.phase.borrow().clone()),
        ));
        full.push((
            "DOT_INIT_GIT_DEV".to_string(),
            OsString::from(self.captured.git_dev.borrow().clone()),
        ));
        full.push((
            "DOT_INIT_GIT_INO".to_string(),
            OsString::from(self.captured.git_ino.borrow().clone()),
        ));
        shell_child_sources(&self.home, &full, extra, sources, snippet)
            .output()
            .expect("spawn bash")
    }

    /// Run a silent step; stash residual streams; map the verdict.
    fn step(
        &self,
        label: String,
        extra: &[(String, OsString)],
        snippet: &str,
    ) -> Result<(), dot::errors::Error> {
        self.captured.note(label);
        let output = self.spawn(extra, snippet);
        let (code, residual) = extract_verdict(&output.stdout);
        self.captured.streams(&residual, &output.stderr);
        if code == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "live step failed",
            })
        }
    }

    /// Minimal-composition value step for the predicate closures.
    fn value_step_min(
        &self,
        label: String,
        extra: &[(String, OsString)],
        snippet: &str,
    ) -> TestResult<String> {
        self.captured.note(label);
        let valfile = self.captured.valfile();
        let mut full_extra = extra.to_vec();
        full_extra.push(("VALFILE".to_string(), OsString::from(valfile.as_os_str())));
        let output = self.spawn_min(&full_extra, snippet);
        let (code, residual) = extract_verdict(&output.stdout);
        self.captured.streams(&residual, &output.stderr);
        if code != 0 {
            return Err(dot::errors::Error::Usage {
                message: "live step failed",
            });
        }
        let value = std::fs::read(&valfile).map_err(|_| dot::errors::Error::Usage {
            message: "live step failed",
        })?;
        String::from_utf8(value).map_err(|_| dot::errors::Error::Usage {
            message: "live step failed",
        })
    }

    /// Run a value step; the snippet stores its value in `VALFILE`.
    fn value_step(
        &self,
        label: String,
        extra: &[(String, OsString)],
        snippet: &str,
    ) -> Result<String, dot::errors::Error> {
        self.captured.note(label);
        let valfile = self.captured.valfile();
        let mut full_extra = extra.to_vec();
        full_extra.push(("VALFILE".to_string(), OsString::from(valfile.as_os_str())));
        let output = self.spawn(&full_extra, snippet);
        let (code, residual) = extract_verdict(&output.stdout);
        self.captured.streams(&residual, &output.stderr);
        if code != 0 {
            return Err(dot::errors::Error::Usage {
                message: "live step failed",
            });
        }
        let value = std::fs::read(&valfile).map_err(|_| dot::errors::Error::Usage {
            message: "live step failed",
        })?;
        String::from_utf8(value).map_err(|_| dot::errors::Error::Usage {
            message: "live step failed",
        })
    }

    /// Refresh the threaded git identity from the child (stage and
    /// publish provenance `_dot_init_set_git_identity` on success).
    fn refresh_identity(&self, value: &str) {
        if let Some((dev, ino)) = value.split_once(':') {
            if !dev.is_empty() && !ino.is_empty() {
                *self.captured.git_dev.borrow_mut() = dev.to_string();
                *self.captured.git_ino.borrow_mut() = ino.to_string();
            }
        }
    }
}

fn ev(key: &str, value: &Path) -> (String, OsString) {
    (key.to_string(), OsString::from(value.as_os_str()))
}

/// Wire the port's cross-lane dependencies to live delegating
/// closures over `scope`.
type TestResult<T> = Result<T, dot::errors::Error>;

/// Wire the port's cross-lane dependencies to live delegating
/// closures. A macro (not a helper fn): the closures borrow the
/// caller's scope, so they must be locals of the runner. Each closure
/// logs its normalized invocation, delegates to the live shell
/// helper, stashes residual streams, and threads run-state mutations
/// (`record_phase` advances the phase; stage/publish refresh the git
/// identity), mirroring the oracle's in-process globals.
macro_rules! wire_deps {
    ($scope:expr, $run:expr) => {{
        let scope_ref: &Scope = $scope;
        let path_identity = |path: &Path| -> TestResult<String> {
            scope_ref.value_step_min(
                format!("path_identity({})", disp(&scope_ref.home, path)),
                &[ev("P", path)],
                "identity=$(_dot_path_identity \"$P\"); code=$?; printf '%s' \"$identity\" >\"$VALFILE\"; printf 'code=%s\\n' \"$code\"",
            )
        };
        let generation_matches = |path: &Path| -> bool {
            let label = format!("generation_matches({})", disp(&scope_ref.home, path));
            scope_ref.captured.note(label);
            let output = scope_ref.spawn_min(
                &[ev("P", path)],
                "_dot_init_generation_marker_matches \"$P\"; printf 'code=%s\\n' \"$?\"",
            );
            let (code, residual) = extract_verdict(&output.stdout);
            scope_ref.captured.streams(&residual, &output.stderr);
            code == 0
        };
        let repo_identity = |url: &str| -> TestResult<String> {
            let extra = vec![("U".to_string(), OsString::from(url))];
            scope_ref.value_step_min(
                format!("repo_identity({url})"),
                &extra,
                "identity=$(_dot_init_repo_identity \"$U\"); code=$?; printf '%s' \"$identity\" >\"$VALFILE\"; printf 'code=%s\\n' \"$code\"",
            )
        };
        let record_phase = |record: &Path, phase: &str| -> TestResult<()> {
            let extra = vec![ev("R", record), ("PH".to_string(), OsString::from(phase))];
            let outcome = scope_ref.step(
                format!("record_phase({},{phase})", disp(&scope_ref.home, record)),
                &extra,
                "_dot_init_record_phase \"$R\" \"$PH\"; printf 'code=%s\\n' \"$?\"",
            );
            if outcome.is_ok() {
                scope_ref.captured.phase.replace(phase.to_string());
            }
            outcome
        };
        let move_conflicts = |manifest: &Path, backup: &Path| -> TestResult<()> {
            scope_ref.step(
                format!(
                    "move_conflicts({},{})",
                    disp(&scope_ref.home, manifest),
                    disp(&scope_ref.home, backup)
                ),
                &[ev("M", manifest), ev("B", backup)],
                "_dot_init_move_conflicts \"$M\" \"$B\"; printf 'code=%s\\n' \"$?\"",
            )
        };
        let stage_git = |record: &Path| -> TestResult<()> {
            let value = scope_ref.value_step(
                format!("stage_git({})", disp(&scope_ref.home, record)),
                &[ev("R", record)],
                "_dot_init_stage_git \"$R\"; code=$?; printf '%s' \"$(_dot_path_identity \"$DOT_INIT_GIT_DIR\")\" >\"$VALFILE\"; printf 'code=%s\\n' \"$code\"",
            )?;
            scope_ref.refresh_identity(&value);
            Ok(())
        };
        let publish_git = |record: &Path| -> TestResult<()> {
            let value = scope_ref.value_step(
                format!("publish_git({})", disp(&scope_ref.home, record)),
                &[ev("R", record)],
                "_dot_init_publish_git \"$R\"; code=$?; printf '%s' \"$(_dot_path_identity \"$DOT_INIT_GIT_DIR\")\" >\"$VALFILE\"; printf 'code=%s\\n' \"$code\"",
            )?;
            scope_ref.refresh_identity(&value);
            Ok(())
        };
        let publish_worktree = |transaction: &Path| -> TestResult<()> {
            scope_ref.step(
                format!("publish_worktree({})", disp(&scope_ref.home, transaction)),
                &[ev("T", transaction)],
                "_dot_init_publish_worktree \"$T\"; printf 'code=%s\\n' \"$?\"",
            )
        };
        let forward_converge = || -> TestResult<()> {
            scope_ref.step(
                "forward_converge()".to_string(),
                &[],
                "_dot_init_forward_converge; printf 'code=%s\\n' \"$?\"",
            )
        };
        let publish_completed = |record: &Path| -> TestResult<()> {
            scope_ref.step(
                format!("publish_completed({})", disp(&scope_ref.home, record)),
                &[ev("R", record)],
                "_dot_init_publish_completed \"$R\"; printf 'code=%s\\n' \"$?\"",
            )
        };
        let live = resume::LiveGitDeps {
            path_identity: &path_identity,
            generation_matches: &generation_matches,
            repo_identity: &repo_identity,
        };
        let deps = resume::ResumeDeps {
            live: &live,
            record_phase: &record_phase,
            move_conflicts: &move_conflicts,
            stage_git: &stage_git,
            publish_git: &publish_git,
            publish_worktree: &publish_worktree,
            forward_converge: &forward_converge,
            publish_completed: &publish_completed,
        };
        $run(&live, &deps)
    }};
}

/// Full oracle env: static base plus the threaded triple snapshot.
fn full_env(ctx: &Ctx, world: &World, phase: &str) -> Env {
    let mut env = base_env(ctx, world);
    env.push(("DOT_INIT_PHASE".to_string(), OsString::from(phase)));
    env.push((
        "DOT_INIT_GIT_DEV".to_string(),
        OsString::from(ctx.dev.clone()),
    ));
    env.push((
        "DOT_INIT_GIT_INO".to_string(),
        OsString::from(ctx.ino.clone()),
    ));
    env
}

const PRED_SNIPPET: &str = "_dot_init_live_git_matches_record; printf 'code=%s\n' \"$?\"";
const RESUME_SNIPPET: &str =
    "_dot_init_resume_transaction \"$TXN\" \"$RECORD\"; printf 'code=%s\n' \"$?\"";

/// `phase=` line of a home's completion marker, if present.
fn completed_phase(home: &Path) -> Option<String> {
    let completed = home.join(".local/state/dot/init/completed");
    let content = std::fs::read_to_string(&completed).ok()?;
    content
        .lines()
        .find_map(|line| line.strip_prefix("phase=").map(str::to_string))
}

/// Git mutation inside a fixture git dir (hooks stay disabled).
fn git_dir_cmd(git_dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn fixture git");
    assert!(
        output.status.success(),
        "fixture git-dir command failed: {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn no_mutate(_home: &Path, _ctx: &mut Ctx, _world: &World) {}

/// Foreign git dir: a valid repo that is neither `.dotfiles` nor
/// `.git`, pinning the topology `else` refusal.
fn build_foreign_git(home: &Path, world: &World, nonce: &str) -> PathBuf {
    build_staged_git(
        home,
        &world.origin,
        &world.commit,
        nonce,
        &world.identity,
        "main",
        &home.join("other.git"),
    )
}

fn run_predicate(
    tag: &str,
    sha256: bool,
    nonce: &str,
    build: fn(&Path, &World, &str) -> PathBuf,
    mutate: fn(&Path, &mut Ctx, &World),
    expect: bool,
    expect_log: &[&str],
) {
    let world = if sha256 {
        World::build_sha256(tag)
    } else {
        World::build(tag)
    };
    let sh_git = build(&world.shell_home, &world, nonce);
    let rs_git = build(&world.rust_home, &world, nonce);
    let mut sh_ctx = ctx_for(&world.shell_home, &world, sh_git, nonce);
    let mut rs_ctx = ctx_for(&world.rust_home, &world, rs_git, nonce);
    mutate(&world.shell_home, &mut sh_ctx, &world);
    mutate(&world.rust_home, &mut rs_ctx, &world);
    let oracle = shell_run_min(
        &world.shell_home,
        &full_env(&sh_ctx, &world, "checkout"),
        PRED_SNIPPET,
    );
    let valdir = world._dir.path().join("rs-vals");
    let scope = Scope::build(
        &world.rust_home,
        &valdir,
        base_env(&rs_ctx, &world),
        "checkout",
        &rs_ctx.dev,
        &rs_ctx.ino,
    );
    wire_deps!(
        &scope,
        |live: &resume::LiveGitDeps<'_>, _deps: &resume::ResumeDeps<'_>| {
            let inputs = resume::LiveGitInputs {
                git_dir: &rs_ctx.git_dir,
                git_dev: &rs_ctx.dev,
                git_ino: &rs_ctx.ino,
                nonce: &rs_ctx.nonce,
                identity: &rs_ctx.identity,
                branch: &rs_ctx.branch,
                home: &world.rust_home,
            };
            let matched = resume::live_git_matches_record(&inputs, live);
            let got = if matched { 0 } else { 1 };
            assert_eq!(if expect { 0 } else { 1 }, oracle.0, "{tag}: oracle");
            assert_eq!(oracle.0, got, "{tag}: verdict");
            assert_eq!(
                scope.captured.out.borrow().as_slice(),
                oracle.1.as_slice(),
                "{tag}: stdout"
            );
            assert_eq!(
                scope.captured.err.borrow().as_slice(),
                oracle.2.as_slice(),
                "{tag}: stderr"
            );
            let origin_str = world.origin.to_string_lossy();
            let log: Vec<String> = scope.captured.log.borrow().clone();
            let expected: Vec<String> = expect_log
                .iter()
                .map(|line| line.replace("{O}", origin_str.as_ref()))
                .collect();
            assert_eq!(log, expected, "{tag}: call log");
        }
    );
}

#[allow(clippy::too_many_arguments)]
fn run_resume(
    tag: &str,
    phase: &str,
    nonce: &str,
    prebuilt: Option<fn(&Path, &World, &str) -> PathBuf>,
    mutate: fn(&Path, &mut Ctx, &World),
    expect_code: i32,
    expect_log: &[&str],
    expect_txn: bool,
    expect_stage: bool,
    expect_completed: bool,
) -> World {
    let world = World::build(tag);
    let sh_git = match prebuilt {
        Some(build) => build(&world.shell_home, &world, nonce),
        None => world.shell_home.join(".dotfiles"),
    };
    let rs_git = match prebuilt {
        Some(build) => build(&world.rust_home, &world, nonce),
        None => world.rust_home.join(".dotfiles"),
    };
    build_journals(&world.shell_home);
    build_journals(&world.rust_home);
    let mut sh_ctx = ctx_for(&world.shell_home, &world, sh_git, nonce);
    let mut rs_ctx = ctx_for(&world.rust_home, &world, rs_git, nonce);
    mutate(&world.shell_home, &mut sh_ctx, &world);
    mutate(&world.rust_home, &mut rs_ctx, &world);
    let oracle = shell_run(
        &world.shell_home,
        &full_env(&sh_ctx, &world, phase),
        RESUME_SNIPPET,
    );
    let valdir = world._dir.path().join("rs-vals");
    let scope = Scope::build(
        &world.rust_home,
        &valdir,
        base_env(&rs_ctx, &world),
        phase,
        &rs_ctx.dev,
        &rs_ctx.ino,
    );
    wire_deps!(
        &scope,
        |_live: &resume::LiveGitDeps<'_>, deps: &resume::ResumeDeps<'_>| {
            let git = resume::LiveGitInputs {
                git_dir: &rs_ctx.git_dir,
                git_dev: &rs_ctx.dev,
                git_ino: &rs_ctx.ino,
                nonce: &rs_ctx.nonce,
                identity: &rs_ctx.identity,
                branch: &rs_ctx.branch,
                home: &world.rust_home,
            };
            let inputs = resume::ResumeInputs {
                transaction: &rs_ctx.txn,
                record: &rs_ctx.record,
                phase,
                backup: &rs_ctx.backup,
                nonce: &rs_ctx.nonce,
                git,
            };
            let outcome = resume::resume_transaction(&inputs, deps);
            let got = if outcome.is_ok() { 0 } else { 1 };
            assert_eq!(expect_code, oracle.0, "{tag}: oracle");
            assert_eq!(oracle.0, got, "{tag}: verdict");
            assert_eq!(
                scope.captured.out.borrow().as_slice(),
                oracle.1.as_slice(),
                "{tag}: stdout"
            );
            assert_eq!(
                scope.captured.err.borrow().as_slice(),
                oracle.2.as_slice(),
                "{tag}: stderr"
            );
            let origin_str = world.origin.to_string_lossy();
            let log: Vec<String> = scope.captured.log.borrow().clone();
            let expected: Vec<String> = expect_log
                .iter()
                .map(|line| line.replace("{O}", origin_str.as_ref()))
                .collect();
            assert_eq!(log, expected, "{tag}: call log");
            assert_eq!(
                sh_ctx.txn.exists(),
                rs_ctx.txn.exists(),
                "{tag}: transaction parity"
            );
            assert_eq!(rs_ctx.txn.exists(), expect_txn, "{tag}: transaction");
            let sh_stage = sh_ctx.backup.join("git-stage").exists();
            let rs_stage = rs_ctx.backup.join("git-stage").exists();
            assert_eq!(sh_stage, rs_stage, "{tag}: git-stage parity");
            assert_eq!(rs_stage, expect_stage, "{tag}: git-stage");
            let sh_completed = completed_phase(&world.shell_home);
            let rs_completed = completed_phase(&world.rust_home);
            assert_eq!(sh_completed, rs_completed, "{tag}: completed parity");
            assert_eq!(rs_completed.is_some(), expect_completed, "{tag}: completed");
            if let Some(line) = rs_completed {
                assert_eq!(line, "complete", "{tag}: completed phase");
            }
            world
        }
    )
}

const NONCE: &str = "resume-nonce-1";
const FULL_DOTFILES: &[&str] = &[
    "path_identity(~/.dotfiles)",
    "generation_matches(~/.dotfiles)",
    "repo_identity({O})",
];

#[test]
fn pred_healthy_dotfiles() {
    run_predicate(
        "p01",
        false,
        NONCE,
        build_dotfiles_git,
        no_mutate,
        true,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_bare_true() {
    run_predicate(
        "p02",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| git_dir_cmd(&ctx.git_dir, &["config", "core.bare", "true"]),
        true,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_worktree_wrong() {
    run_predicate(
        "p03",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            git_dir_cmd(&ctx.git_dir, &["config", "core.worktree", "/elsewhere"]);
        },
        false,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_bare_unset() {
    run_predicate(
        "p04",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            git_dir_cmd(&ctx.git_dir, &["config", "--unset", "core.bare"]);
        },
        false,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_git_topology() {
    run_predicate(
        "p05",
        false,
        NONCE,
        build_home_git,
        no_mutate,
        true,
        &[
            "path_identity(~/.git)",
            "generation_matches(~/.git)",
            "repo_identity({O})",
        ],
    );
}

#[test]
fn pred_two_origin_urls() {
    run_predicate(
        "p06",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            git_dir_cmd(
                &ctx.git_dir,
                &[
                    "remote",
                    "set-url",
                    "--add",
                    "origin",
                    "/tmp/dot-resume-second-url",
                ],
            );
        },
        false,
        &[
            "path_identity(~/.dotfiles)",
            "generation_matches(~/.dotfiles)",
        ],
    );
}

#[test]
fn pred_zero_urls() {
    run_predicate(
        "p07",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            git_dir_cmd(&ctx.git_dir, &["remote", "remove", "origin"]);
        },
        false,
        &[
            "path_identity(~/.dotfiles)",
            "generation_matches(~/.dotfiles)",
        ],
    );
}

#[test]
fn pred_wrong_identity() {
    // The marker binds the identity too: re-stamp it under the wrong
    // identity so the generation gate passes and the verdict rests on
    // the normalized-identity comparison.
    run_predicate(
        "p08",
        false,
        NONCE,
        build_dotfiles_git,
        |home, ctx, world| {
            ctx.identity = "file:///nonexistent-identity".to_string();
            std::fs::remove_file(ctx.git_dir.join("dot-init-generation-v1")).expect("rm marker");
            let env = vec![
                ("G".to_string(), OsString::from(ctx.git_dir.as_os_str())),
                (
                    "DOT_INIT_NONCE".to_string(),
                    OsString::from(ctx.nonce.clone()),
                ),
                (
                    "DOT_INIT_COMMIT".to_string(),
                    OsString::from(world.commit.clone()),
                ),
                (
                    "DOT_INIT_IDENTITY".to_string(),
                    OsString::from(ctx.identity.clone()),
                ),
            ];
            let output = shell_child_sources(
                home,
                &env,
                &[],
                SOURCES,
                "_dot_init_write_generation_marker \"$G\"",
            )
            .output()
            .expect("spawn bash");
            assert!(output.status.success(), "re-stamp marker");
        },
        false,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_wrong_branch() {
    run_predicate(
        "p09",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            ctx.branch = "otherbranch".to_string();
        },
        false,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_detached_head() {
    run_predicate(
        "p10",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            git_dir_cmd(
                &ctx.git_dir,
                &["symbolic-ref", "HEAD", "refs/heads/detached-never"],
            );
        },
        false,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_ino_mismatch() {
    run_predicate(
        "p11",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            ctx.ino = "999999999999".to_string();
        },
        false,
        &["path_identity(~/.dotfiles)"],
    );
}

#[test]
fn pred_missing_dir() {
    run_predicate(
        "p12",
        false,
        NONCE,
        build_dotfiles_git,
        |home, ctx, _world| {
            ctx.git_dir = home.join("no-such-git-dir");
        },
        false,
        &[],
    );
}

#[test]
fn pred_symlinked_dir() {
    run_predicate(
        "p13",
        false,
        NONCE,
        build_dotfiles_git,
        |home, ctx, _world| {
            let link = home.join("gitlink");
            std::os::unix::fs::symlink(&ctx.git_dir, &link).expect("symlink git dir");
            ctx.git_dir = link;
        },
        false,
        &[],
    );
}

#[test]
fn pred_adopted_garbage_marker() {
    // Adopted runs skip the generation gate entirely: only the
    // identity and branch probes run.
    run_predicate(
        "p14",
        false,
        "adopted",
        build_dotfiles_git,
        |_home, ctx, _world| {
            std::fs::write(ctx.git_dir.join("dot-init-generation-v1"), b"garbage\n")
                .expect("tamper marker");
        },
        true,
        &["path_identity(~/.dotfiles)", "repo_identity({O})"],
    );
}

#[test]
fn pred_tampered_marker() {
    run_predicate(
        "p15",
        false,
        NONCE,
        build_dotfiles_git,
        |_home, ctx, _world| {
            std::fs::write(ctx.git_dir.join("dot-init-generation-v1"), b"garbage\n")
                .expect("tamper marker");
        },
        false,
        &[
            "path_identity(~/.dotfiles)",
            "generation_matches(~/.dotfiles)",
        ],
    );
}

#[test]
fn pred_foreign_dir() {
    run_predicate(
        "p16",
        false,
        NONCE,
        build_foreign_git,
        no_mutate,
        false,
        &[
            "path_identity(~/other.git)",
            "generation_matches(~/other.git)",
            "repo_identity({O})",
        ],
    );
}

#[test]
fn pred_bad_url() {
    run_predicate(
        "p17",
        false,
        NONCE,
        build_foreign_git,
        |_home, ctx, _world| {
            git_dir_cmd(&ctx.git_dir, &["remote", "set-url", "origin", "::bad::"]);
        },
        false,
        &[
            "path_identity(~/other.git)",
            "generation_matches(~/other.git)",
            "repo_identity(::bad::)",
        ],
    );
}

#[test]
fn pred_sha256_commit() {
    // 64-hex object ids take the regex's second arm.
    run_predicate(
        "p18",
        true,
        NONCE,
        build_dotfiles_git,
        no_mutate,
        true,
        FULL_DOTFILES,
    );
}

#[test]
fn pred_toplevel_mismatch() {
    // A redirected worktree is not an ordinary HOME checkout: the
    // toplevel's physical path no longer equals HOME's, pinning the
    // canonicalization arm. (A symlinked HOME alone cannot trip this:
    // git and `cd -P` resolve it identically, probed.)
    run_predicate(
        "p19",
        false,
        NONCE,
        build_home_git,
        |_home, ctx, _world| {
            git_dir_cmd(
                &ctx.git_dir,
                &["config", "core.worktree", "/tmp/dot-resume-elsewhere"],
            );
        },
        false,
        &[
            "path_identity(~/.git)",
            "generation_matches(~/.git)",
            "repo_identity({O})",
        ],
    );
}

const FULL_PREPARED: &[&str] = &[
    "record_phase(~/txn/record,backing-up)",
    "move_conflicts(~/txn/conflicts.tsv,~/.dot-backup/t1)",
    "record_phase(~/txn/record,backed-up)",
    "stage_git(~/txn/record)",
    "publish_git(~/txn/record)",
    "publish_worktree(~/txn)",
    "record_phase(~/txn/record,checkout)",
    "record_phase(~/txn/record,converging)",
    "forward_converge()",
    "record_phase(~/txn/record,complete)",
    "publish_completed(~/txn/record)",
];
const CHECKOUT_TAIL: &[&str] = &[
    "path_identity(~/.dotfiles)",
    "generation_matches(~/.dotfiles)",
    "repo_identity({O})",
    "record_phase(~/txn/record,converging)",
    "forward_converge()",
    "record_phase(~/txn/record,complete)",
    "publish_completed(~/txn/record)",
];

#[test]
fn resume_prepared_healthy() {
    let world = run_resume(
        "r01",
        "prepared",
        NONCE,
        None,
        no_mutate,
        0,
        FULL_PREPARED,
        false,
        false,
        true,
    );
    // The resumed world is coherent: the live check passes on the
    // result from both engines.
    let sh_ctx = ctx_for(
        &world.shell_home,
        &world,
        world.shell_home.join(".dotfiles"),
        NONCE,
    );
    let oracle = shell_run_min(
        &world.shell_home,
        &full_env(&sh_ctx, &world, "checkout"),
        PRED_SNIPPET,
    );
    assert_eq!(oracle.0, 0, "r01: post-check oracle");
    let rs_ctx = ctx_for(
        &world.rust_home,
        &world,
        world.rust_home.join(".dotfiles"),
        NONCE,
    );
    let valdir = world._dir.path().join("rs-post");
    let scope = Scope::build(
        &world.rust_home,
        &valdir,
        base_env(&rs_ctx, &world),
        "checkout",
        &rs_ctx.dev,
        &rs_ctx.ino,
    );
    wire_deps!(
        &scope,
        |live: &resume::LiveGitDeps<'_>, _deps: &resume::ResumeDeps<'_>| {
            let inputs = resume::LiveGitInputs {
                git_dir: &rs_ctx.git_dir,
                git_dev: &rs_ctx.dev,
                git_ino: &rs_ctx.ino,
                nonce: &rs_ctx.nonce,
                identity: &rs_ctx.identity,
                branch: &rs_ctx.branch,
                home: &world.rust_home,
            };
            assert!(
                resume::live_git_matches_record(&inputs, live),
                "r01: post-check port"
            );
        }
    );
}

#[test]
fn resume_checkout_healthy() {
    run_resume(
        "r02",
        "checkout",
        NONCE,
        Some(build_dotfiles_git),
        no_mutate,
        0,
        CHECKOUT_TAIL,
        false,
        false,
        true,
    );
}

#[test]
fn resume_complete_healthy() {
    run_resume(
        "r03",
        "complete",
        NONCE,
        Some(build_dotfiles_git),
        |home, ctx, world| {
            let env = full_env(ctx, world, "complete");
            let output = shell_child_sources(
                home,
                &env,
                &[],
                SOURCES,
                "_dot_init_record_phase \"$RECORD\" complete",
            )
            .output()
            .expect("spawn bash");
            assert!(output.status.success(), "seed record");
        },
        0,
        &[
            "path_identity(~/.dotfiles)",
            "generation_matches(~/.dotfiles)",
            "repo_identity({O})",
            "publish_completed(~/txn/record)",
        ],
        false,
        false,
        true,
    );
}

#[test]
fn resume_bogus_phase() {
    run_resume(
        "r04",
        "bogus",
        NONCE,
        None,
        no_mutate,
        1,
        &[],
        true,
        false,
        false,
    );
}

#[test]
fn resume_missing_prior() {
    run_resume(
        "r05",
        "prepared",
        NONCE,
        None,
        |_home, ctx, _world| {
            std::fs::remove_file(ctx.txn.join("prior.tsv")).expect("rm prior");
        },
        1,
        &[],
        true,
        false,
        false,
    );
}

#[test]
fn resume_checkout_branch_mismatch() {
    run_resume(
        "r06",
        "checkout",
        NONCE,
        Some(build_dotfiles_git),
        |_home, ctx, _world| {
            git_dir_cmd(
                &ctx.git_dir,
                &["symbolic-ref", "HEAD", "refs/heads/otherside"],
            );
        },
        1,
        &[
            "path_identity(~/.dotfiles)",
            "generation_matches(~/.dotfiles)",
            "repo_identity({O})",
        ],
        true,
        false,
        false,
    );
}

#[test]
fn resume_backedup_reuses_stage() {
    // A pre-existing stage with a valid marker flows through clone
    // and is cleaned up, pinning the container-reuse path.
    run_resume(
        "r07a",
        "backed-up",
        NONCE,
        None,
        |_home, ctx, world| {
            let stage = ctx.backup.join("git-stage");
            std::fs::create_dir_all(&stage).expect("stage dir");
            let marker = format!(
                "cgraf78 dot Git stage v1\nnonce={}\ncommit={}\nidentity={}\n",
                ctx.nonce, world.commit, world.identity
            );
            let identity = stage.join("identity");
            std::fs::write(&identity, marker).expect("stage marker");
            std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600))
                .expect("chmod marker");
        },
        0,
        FULL_PREPARED,
        false,
        false,
        true,
    );
}

#[test]
fn resume_backedup_bad_stage_marker() {
    // The stage gate fails before any checkout work: the log stops
    // after the stage attempt.
    run_resume(
        "r07b",
        "backed-up",
        NONCE,
        None,
        |_home, ctx, _world| {
            let stage = ctx.backup.join("git-stage");
            std::fs::create_dir_all(&stage).expect("stage dir");
            std::fs::write(stage.join("identity"), b"nonce=WRONG\n").expect("stage marker");
        },
        1,
        &[
            "record_phase(~/txn/record,backing-up)",
            "move_conflicts(~/txn/conflicts.tsv,~/.dot-backup/t1)",
            "record_phase(~/txn/record,backed-up)",
            "stage_git(~/txn/record)",
        ],
        true,
        true,
        false,
    );
}

#[test]
fn resume_checkout_adopted() {
    // Adopted runs never consult the marker: absent is fine.
    run_resume(
        "r08",
        "checkout",
        "adopted",
        Some(build_dotfiles_git),
        |_home, ctx, _world| {
            std::fs::remove_file(ctx.git_dir.join("dot-init-generation-v1")).expect("rm marker");
        },
        0,
        &[
            "path_identity(~/.dotfiles)",
            "repo_identity({O})",
            "record_phase(~/txn/record,converging)",
            "forward_converge()",
            "record_phase(~/txn/record,complete)",
            "publish_completed(~/txn/record)",
        ],
        false,
        false,
        true,
    );
}

#[test]
fn resume_checkout_missing_marker() {
    run_resume(
        "r09",
        "checkout",
        NONCE,
        Some(build_dotfiles_git),
        |_home, ctx, _world| {
            std::fs::remove_file(ctx.git_dir.join("dot-init-generation-v1")).expect("rm marker");
        },
        1,
        &[
            "path_identity(~/.dotfiles)",
            "generation_matches(~/.dotfiles)",
        ],
        true,
        false,
        false,
    );
}

#[test]
fn resume_publishing_healthy() {
    // Every early-phase alias takes the same arm: pin one more.
    run_resume(
        "r10",
        "publishing",
        NONCE,
        None,
        no_mutate,
        0,
        FULL_PREPARED,
        false,
        false,
        true,
    );
}

#[test]
fn resume_backingup_missing_tree() {
    run_resume(
        "r11",
        "backing-up",
        NONCE,
        None,
        |_home, ctx, _world| {
            std::fs::remove_file(ctx.txn.join("tree.tsv")).expect("rm tree");
        },
        1,
        &[],
        true,
        false,
        false,
    );
}

#[test]
fn resume_record_is_dir() {
    // The first journal write fails; nothing else runs.
    run_resume(
        "r12",
        "prepared",
        NONCE,
        None,
        |_home, ctx, _world| {
            std::fs::remove_file(&ctx.record).expect("rm record");
            std::fs::create_dir(&ctx.record).expect("mkdir record");
        },
        1,
        &["record_phase(~/txn/record,backing-up)"],
        true,
        false,
        false,
    );
}

#[test]
fn resume_bogus_origin() {
    // The clone fails identically on both engines (fixed path, so
    // the git diagnostic bytes match); the log stops after stage.
    run_resume(
        "r13",
        "prepared",
        NONCE,
        None,
        |_home, ctx, _world| {
            ctx.origin = PathBuf::from(BOGUS_ORIGIN);
        },
        1,
        &[
            "record_phase(~/txn/record,backing-up)",
            "move_conflicts(~/txn/conflicts.tsv,~/.dot-backup/t1)",
            "record_phase(~/txn/record,backed-up)",
            "stage_git(~/txn/record)",
        ],
        true,
        true,
        false,
    );
}

#[test]
fn resume_symlinked_journals() {
    // The journal gate follows symlinks (`-f`, not `-l`): linked
    // journals resume exactly like regular ones.
    run_resume(
        "r14",
        "prepared",
        NONCE,
        None,
        |home, ctx, _world| {
            let real = home.join("real-journals");
            std::fs::create_dir_all(&real).expect("real journals");
            for name in ["tree.tsv", "prior.tsv", "conflicts.tsv"] {
                std::fs::write(real.join(name), b"").expect("real journal");
                std::fs::remove_file(ctx.txn.join(name)).expect("rm journal");
                std::os::unix::fs::symlink(real.join(name), ctx.txn.join(name))
                    .expect("link journal");
            }
        },
        0,
        FULL_PREPARED,
        false,
        false,
        true,
    );
}

#[test]
fn resume_converging_healthy() {
    run_resume(
        "r15",
        "converging",
        NONCE,
        Some(build_dotfiles_git),
        no_mutate,
        0,
        CHECKOUT_TAIL,
        false,
        false,
        true,
    );
}

#[test]
fn resume_prepared_prebuilt() {
    // With the git dir already staged, no container is ever created:
    // the cleanup gate stays false and the run still converges.
    run_resume(
        "r16",
        "prepared",
        NONCE,
        Some(build_dotfiles_git),
        no_mutate,
        0,
        FULL_PREPARED,
        false,
        false,
        true,
    );
}
