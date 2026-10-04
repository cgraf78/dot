//! Native `dot update` end-to-end execution coverage.
//!
//! The dispatcher arm ([`dot::cli::run`] with `update`/`pull`) runs the
//! update lifecycle for real and reports its exit code. These tests exercise
//! the wired arm on synthetic `file://` fixtures in both steady states:
//!
//! - clean: nothing changed since `init` (the cron steady state);
//! - dirty: one pushed overlay change waiting to converge.
//!
//! The historical Bash implementation is benchmarked separately from its
//! pre-cutover revision; this suite contains no hidden second engine.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Scratch helper shared with the other parity suites: pid plus a
/// monotonic counter, no wall-clock reads.
type Scratch = dot_test_support::TempDir;

/// The Rust binary under test.
fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dot"));
    // One `.env` per variable (never `.envs`): MSRV-clean and matches
    // the oracle convention in `tests/cli.rs`.
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd
}

#[test]
fn update_has_no_legacy_engine_selector_or_adapter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in std::fs::read_dir(root.join("src")).expect("source directory") {
        let relative = relative.expect("source entry").path();
        if relative
            .extension()
            .is_none_or(|extension| extension != "rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&relative).expect("read production source");
        let executable: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        assert!(
            !source.contains("DOT_UPDATE_NATIVE"),
            "{} still selects an optional update lane",
            relative.display()
        );
        if relative
            .file_name()
            .is_some_and(|name| name == "update_engine.rs" || name == "update_run.rs")
        {
            for legacy in [
                "ENGINE_SCRIPT",
                "run_update_or_engine",
                "should_go_native",
                "Fallback",
            ] {
                assert!(
                    !executable.contains(legacy),
                    "{} still contains legacy update engine dependency {legacy}",
                    relative.display()
                );
            }
        }
    }
    let cli = std::fs::read_to_string(root.join("src/cli.rs")).expect("CLI source");
    let update = cli
        .split_once("Command::Update =>")
        .and_then(|(_, tail)| tail.split_once("Command::Init =>").map(|(arm, _)| arm))
        .expect("bounded Update dispatch arm");
    assert!(update.contains("run_update(runtime"));
    assert!(!update.contains("run_engine_arm"));
}

/// Controlled client environment, mirroring `init_env` in
/// `tests/cli.rs`: a cleared environment plus a twin home/state pair,
/// so rows never touch the developer's own checkout.
fn client_env(cmd: &mut Command, home: &Path, state: &Path) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    cmd.env_clear();
    cmd.env("LC_ALL", "C");
    cmd.env("PATH", &path);
    cmd.env("TMPDIR", &tmpdir);
    cmd.env("HOME", home);
    // Bash may synthesize this when absent while the native process preserves
    // the cleared environment, so make the reload-hint input explicit.
    cmd.env("SHELL", "/bin/bash");
    cmd.env("XDG_STATE_HOME", state);
    cmd.env("XDG_CONFIG_HOME", "");
    cmd.env("DOT_SOURCE_ROOT", repo);
    cmd.env("GIT_AUTHOR_NAME", "fixture");
    cmd.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
    cmd.env("GIT_COMMITTER_NAME", "fixture");
    cmd.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    cmd.current_dir(home);
}

#[test]
fn command_pins_the_reload_shell() {
    let scratch = Scratch::new("update-shell-input").expect("scratch dir");
    let home = scratch.path().join("home");
    let state = scratch.path().join("state");
    let mut command = bin();
    client_env(&mut command, &home, &state);
    let value = command
        .get_envs()
        .find(|(key, _)| *key == "SHELL")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("/bin/bash")));
}

#[test]
fn update_rejects_ambient_topology_for_an_uninitialized_checkout() {
    let scratch = Scratch::new("update-topology-injection").expect("scratch dir");
    let home = scratch.path().join("home");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&home).expect("home");
    git(&home, &["init", "-q"]);
    git(&home, &["config", "user.name", "fixture"]);
    git(&home, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(home.join("tracked"), b"content\n").expect("tracked file");
    git(&home, &["add", "tracked"]);
    git(&home, &["commit", "-qm", "seed"]);

    let mut command = bin();
    client_env(&mut command, &home, &state);
    let output = command
        .arg("update")
        .env("DOT_BASE_TOPOLOGY", "ordinary")
        .env("DOT_CLIENT_GIT_DIR", home.join(".git"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run update");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stderr,
        b"dot: ordinary HOME checkout requires a completed dot init identity\n"
    );
}

fn assert_selector_failure(home: &Path, state: &Path, expected: &[u8]) {
    for argv in [&["status"][..], &["update", "--quiet"][..]] {
        let output = dot(argv, home, state);
        assert_eq!(
            output.status.code(),
            Some(1),
            "dot {argv:?} unexpectedly accepted the client"
        );
        assert_eq!(output.stderr, expected, "dot {argv:?} diagnostic");
    }
}

#[test]
fn repository_commands_and_update_share_client_identity_rejections() {
    let scratch = Scratch::new("client-selector-rejections").expect("scratch dir");

    let malformed_home = scratch.path().join("malformed-home");
    let malformed_state = scratch.path().join("malformed-state");
    std::fs::create_dir_all(&malformed_home).expect("malformed home");
    std::fs::create_dir_all(malformed_state.join("dot/init")).expect("malformed state");
    std::fs::write(
        malformed_state.join("dot/init/completed"),
        b"not a record\n",
    )
    .expect("malformed record");
    assert_selector_failure(
        &malformed_home,
        &malformed_state,
        b"dot: malformed initialization identity record\n",
    );

    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (tampered_home, tampered_state) =
        twin_client(&scratch, "tampered", &overlay_origin, &base_origin);
    let completed = tampered_state.join("dot/init/completed");
    let record = std::fs::read_to_string(&completed)
        .expect("completion record")
        .lines()
        .map(|line| {
            if line.starts_with("nonce=") {
                "nonce=tampered"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&completed, record).expect("tampered completion record");
    assert_selector_failure(
        &tampered_home,
        &tampered_state,
        b"dot: client Git directory no longer matches initialization identity\n",
    );

    let foreign_home = scratch.path().join("foreign-home");
    let foreign_state = scratch.path().join("foreign-state");
    std::fs::create_dir_all(foreign_home.join(".dotfiles")).expect("foreign client directory");
    assert_selector_failure(
        &foreign_home,
        &foreign_state,
        format!(
            "dot: unsupported or foreign client Git directory: {}\n",
            foreign_home.join(".dotfiles").display()
        )
        .as_bytes(),
    );

    let linked_home = scratch.path().join("linked-home");
    let linked_state = scratch.path().join("linked-state");
    let linked_target = scratch.path().join("linked-target");
    std::fs::create_dir_all(&linked_home).expect("linked home");
    std::fs::create_dir_all(&linked_target).expect("linked target");
    std::os::unix::fs::symlink(&linked_target, linked_home.join(".dotfiles"))
        .expect("linked legacy client");
    assert_selector_failure(
        &linked_home,
        &linked_state,
        format!(
            "dot: unsupported or foreign client Git directory: {}\n",
            linked_home.join(".dotfiles").display()
        )
        .as_bytes(),
    );

    let ordinary_home = scratch.path().join("ordinary-home");
    let ordinary_state = scratch.path().join("ordinary-state");
    std::fs::create_dir_all(&ordinary_home).expect("ordinary home");
    git(&ordinary_home, &["init", "-q"]);
    assert_selector_failure(
        &ordinary_home,
        &ordinary_state,
        b"dot: ordinary HOME checkout requires a completed dot init identity\n",
    );
}

#[test]
fn repository_commands_and_update_accept_an_in_progress_identity() {
    let scratch = Scratch::new("client-selector-transaction").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "transaction", &overlay_origin, &base_origin);
    let completed = state.join("dot/init/completed");
    let transaction = state.join("dot/init/transaction/record");
    std::fs::create_dir_all(transaction.parent().expect("transaction parent"))
        .expect("transaction directory");
    std::fs::set_permissions(
        transaction.parent().expect("transaction parent"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("private transaction directory");
    let record = std::fs::read_to_string(&completed)
        .expect("completion record")
        .replace("phase=complete\n", "phase=converging\n");
    std::fs::write(&transaction, record).expect("transaction record");
    std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o600))
        .expect("private transaction record");
    std::fs::remove_file(completed).expect("remove completion record");

    for argv in [&["status"][..], &["update", "--quiet"][..]] {
        let output = dot(argv, &home, &state);
        assert!(
            output.status.success(),
            "dot {argv:?} rejected the transaction identity: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Run the native binary with the same controlled client.
fn dot(argv: &[&str], home: &Path, state: &Path) -> std::process::Output {
    dot_env(argv, home, state, &[])
}

/// [`dot`] with extra environment (for example `DOT_QUIET`).
fn dot_env(
    argv: &[&str],
    home: &Path,
    state: &Path,
    extra: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = bin();
    client_env(&mut cmd, home, state);
    cmd.env("DOT_BASH", home.join("absent-old-update-engine"));
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.args(argv);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run native dot")
}

fn git(dir: &Path, args: &[&str]) {
    let status = dot_test_support::git()
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Seed a repo with payload files and return its bare remote path.
fn seed_remote(scratch: &Scratch, name: &str, branch: &str, prefix: &str, files: usize) -> PathBuf {
    let seed = scratch.path().join(format!("{name}-seed"));
    let root = seed.join(prefix);
    std::fs::create_dir_all(&root).expect("seed dir");
    git(&seed, &["init", "-q"]);
    git(&seed, &["config", "user.name", "fixture"]);
    git(&seed, &["config", "user.email", "fixture@example.invalid"]);
    for index in 0..files {
        let rel = format!("{prefix}file-{index:03}.txt");
        std::fs::write(seed.join(&rel), format!("{name} payload {index}\n")).expect("write");
        git(&seed, &["add", &rel]);
    }
    git(&seed, &["commit", "-qm", "seed"]);
    git(&seed, &["branch", "-M", branch]);
    let origin = scratch.path().join(format!("{name}.git"));
    let output = dot_test_support::git()
        .arg("clone")
        .arg("-q")
        .arg("--bare")
        .arg(&seed)
        .arg(&origin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("clone bare");
    assert!(
        output.status.success(),
        "clone bare {seed:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(
        &origin,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
    );
    origin
}

/// Build one twin client: `init --yes` into a fresh home/state pair,
/// then register the overlay descriptor where discovery reads it
/// (`${config_home}/dot/overlays.d`, per `docs/overlays.md`).
fn twin_client(
    scratch: &Scratch,
    tag: &str,
    overlay_origin: &Path,
    base_origin: &Path,
) -> (PathBuf, PathBuf) {
    let home = scratch.path().join(format!("home-{tag}"));
    let state = scratch.path().join(format!("state-{tag}"));
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&state).expect("state");
    let output = dot(
        &[
            "init",
            "--yes",
            &format!("file://{}", base_origin.display()),
        ],
        &home,
        &state,
    );
    assert!(
        output.status.success(),
        "twin {tag} init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conf_dir = home.join(".config/dot/overlays.d");
    std::fs::create_dir_all(&conf_dir).expect("conf dir");
    let conf = format!("url=file://{}\n", overlay_origin.display());
    std::fs::write(conf_dir.join("overlay-0.conf"), conf).expect("write conf");
    (home, state)
}

/// Build the shared remotes once: one overlay plus a base whose
/// `overlays.d` points at it.
fn shared_remotes(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let overlay_origin = seed_remote(scratch, "overlay-0", "main", "home/", 3);
    let base_seed = scratch.path().join("base-seed");
    std::fs::create_dir_all(base_seed.join("overlays.d")).expect("overlays.d");
    git(&base_seed, &["init", "-q"]);
    git(&base_seed, &["config", "user.name", "fixture"]);
    git(
        &base_seed,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(base_seed.join(".testrc"), "base\n").expect("write");
    let conf = format!("url=file://{}\n", overlay_origin.display());
    std::fs::write(base_seed.join("overlays.d").join("overlay-0.conf"), conf).expect("write conf");
    git(&base_seed, &["add", "-A"]);
    git(&base_seed, &["commit", "-qm", "seed"]);
    git(&base_seed, &["branch", "-M", "main"]);
    let base_origin = scratch.path().join("base.git");
    let output = dot_test_support::git()
        .arg("clone")
        .arg("-q")
        .arg("--bare")
        .arg(&base_seed)
        .arg(&base_origin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("clone bare");
    assert!(
        output.status.success(),
        "clone bare {base_seed:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(&base_origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    (overlay_origin, base_origin)
}

/// Blank the wall-clock stamps the UI rows carry (`0s`, `Done in
/// 12s`): the only bytes allowed to differ between two identical
/// updates. Counts (`1 repo current`, `[1/5]`, `1/1`) never match
/// `<digits>s` at a word boundary the way stamps do, except the
/// `s`-suffixed plural `repos` — which has no leading digit run of
/// its own (`1 repos` keeps its digit: only the stamp form
/// `<digits>s` with no intervening space is replaced).
fn normalize_timing(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let rest = &bytes[index..];
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits > 0 && rest.get(digits) == Some(&b's') {
            out.extend_from_slice(b"Ns");
            index += digits + 1;
        } else {
            out.push(rest[0]);
            index += 1;
        }
    }
    out
}

/// Snapshot the converged HOME tree (regular files only, sorted) for
/// stable comparisons across independent native clients. `.git` carries
/// checkout identity, `.dotfiles` carries the base checkout,
/// `.dot-backup` carries timestamped init-time safekeeping, and
/// `.scm.sqlite*` is SCM's async telemetry database with its SQLite
/// sidecars (a lingering SCM helper may create them after Dot returns):
/// none of them is converged content, so all stay out of the comparison,
/// exactly like the `tests/perf_update.rs` technique.
fn snapshot_tree(home: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).expect("read dir");
        for entry in read {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            let kind = entry.file_type().expect("file type");
            // Checkout identity, timestamped safekeeping, and SCM's async
            // telemetry database are never converged content, whatever
            // filesystem kind they take (a worktree `.git` may be a file,
            // not a directory).
            let skip = path.file_name().is_some_and(|n| {
                n == ".git"
                    || n == ".dotfiles"
                    || n == ".dot-backup"
                    || n == ".scm.sqlite"
                    // Same prefix rule as `tests/perf_update.rs`: SQLite
                    // sidecars (`-journal`, `-wal`, `-shm`, and whatever
                    // arrives next) are telemetry, not content.
                    || n.to_string_lossy().starts_with(".scm.sqlite-")
            });
            if kind.is_dir() {
                if !skip {
                    stack.push(path);
                }
            } else if (kind.is_file() || kind.is_symlink()) && !skip {
                let rel = path
                    .strip_prefix(home)
                    .expect("under home")
                    .to_string_lossy()
                    .into_owned();
                let bytes = std::fs::read(&path).unwrap_or_default();
                entries.push((rel, bytes));
            }
        }
    }
    entries.sort();
    entries
}

fn check_update(argv: &[&str], home: &Path, state: &Path) -> std::process::Output {
    let output = dot(argv, home, state);
    assert!(
        output.status.success(),
        "dot {argv:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn snapshot_tree_ignores_scm_telemetry_sidecars() {
    let scratch = Scratch::new("update-run-scm-skip").expect("scratch dir");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(home.join(".testrc"), "base\n").expect("content");
    for sidecar in [
        ".scm.sqlite",
        ".scm.sqlite-journal",
        ".scm.sqlite-wal",
        ".scm.sqlite-shm",
        // A hypothetical future sidecar: the exclusion is a prefix rule,
        // not an enumeration that must be extended per SQLite release.
        ".scm.sqlite-next",
    ] {
        std::fs::write(home.join(sidecar), "telemetry\n").expect("sidecar");
    }
    assert_eq!(
        snapshot_tree(&home)
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [".testrc"],
        "SCM async telemetry must never join converged content"
    );
}

#[test]
fn clean_update_converges_and_is_stable() {
    let scratch = Scratch::new("update-run-clean").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    let before = snapshot_tree(&home);
    let clean = check_update(&["update"], &home, &state);
    assert!(String::from_utf8_lossy(&clean.stdout).contains("current"));
    assert_eq!(snapshot_tree(&home), before);
}

#[test]
fn dirty_update_converges() {
    let scratch = Scratch::new("update-run-dirty").expect("scratch dir");
    let (_overlay_origin, base_origin) = shared_remotes(&scratch);
    let overlay_seed = scratch.path().join("overlay-0-seed");
    let overlay_origin = scratch.path().join("overlay-0.git");
    let (home, state) = twin_client(&scratch, "native", &overlay_origin, &base_origin);
    std::fs::write(
        overlay_seed.join("home/file-000.txt"),
        "overlay-0 payload CHANGED\n",
    )
    .expect("write");
    git(&overlay_seed, &["add", "home/file-000.txt"]);
    git(&overlay_seed, &["commit", "-qm", "change"]);
    git(
        &overlay_seed,
        &["push", "-q", &overlay_origin.to_string_lossy(), "HEAD:main"],
    );
    check_update(&["update"], &home, &state);
    let bytes = std::fs::read(home.join("file-000.txt")).expect("converged file");
    assert_eq!(bytes, b"overlay-0 payload CHANGED\n");
}

#[test]
fn pull_alias_matches_update() {
    let scratch = Scratch::new("update-run-pull").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home_update, state_update) =
        twin_client(&scratch, "update", &overlay_origin, &base_origin);
    let (home_pull, state_pull) = twin_client(&scratch, "pull", &overlay_origin, &base_origin);
    let update = check_update(&["update"], &home_update, &state_update);
    let pull = check_update(&["pull"], &home_pull, &state_pull);
    assert_eq!(
        normalize_timing(&pull.stdout),
        normalize_timing(&update.stdout)
    );
    assert_eq!(pull.stderr, update.stderr);
    assert_eq!(snapshot_tree(&home_pull), snapshot_tree(&home_update));
}

#[test]
fn lock_busy_reports_75() {
    // Hold the update lock from this process; the child must refuse and name
    // the live owner without mutating the client.
    use dot::log::Log;
    let scratch = Scratch::new("update-run-busy").expect("scratch dir");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let log = Log::new(false, false);
    let mut sink = Vec::new();
    let guard = dot::update_lock::acquire(&state, false, &log, None, &mut sink).expect("hold lock");
    // A fresh acquisition warns nothing; the busy diagnostic below
    // names this live owner.
    assert!(sink.is_empty());
    let pid = std::process::id();
    let expected = format!("  warning: dot update already running (pid {pid})\n");
    let output = dot(&["update"], &home, &state);
    assert_eq!(output.status.code(), Some(75));
    assert_eq!(output.stderr, expected.as_bytes());
    assert!(output.stdout.is_empty());
    let _ = guard;
}

/// A `Write` sink that keeps every `write` call's bytes as its own chunk, so
/// tests can observe emission granularity without changing what is emitted.
struct ChunkWriter {
    chunks: Vec<Vec<u8>>,
}

impl ChunkWriter {
    fn new() -> Self {
        ChunkWriter { chunks: Vec::new() }
    }

    fn concatenated(&self) -> Vec<u8> {
        self.chunks.concat()
    }

    /// Index of the first chunk holding `needle`, if any.
    fn chunk_holding(&self, needle: &[u8]) -> Option<usize> {
        self.chunks.iter().position(|chunk| {
            chunk
                .windows(needle.len().max(1))
                .any(|window| window == needle)
        })
    }
}

/// A `Write` sink that fails every write, proving delivery failure still
/// exits 1 after the engine runs to completion.
struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("closed stdout"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.chunks.push(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn update_streams_stage_rows_before_completion() {
    // `dot update` must emit each stage row as its phase files it: completed
    // stages reach stdout while later phases still run, instead of the whole
    // run rendering once at the end.
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    let scratch = Scratch::new("update-run-streaming").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home_ref, state_ref) = twin_client(&scratch, "reference", &overlay_origin, &base_origin);
    let (home_live, state_live) = twin_client(&scratch, "live", &overlay_origin, &base_origin);
    let reference = check_update(&["update"], &home_ref, &state_ref);

    let home_text = home_live.to_str().expect("UTF-8 home").to_string();
    let env = BTreeMap::<OsString, OsString>::from([
        ("HOME".into(), home_live.as_os_str().to_os_string()),
        (
            "XDG_STATE_HOME".into(),
            state_live.as_os_str().to_os_string(),
        ),
        ("XDG_CONFIG_HOME".into(), OsString::from("")),
        (
            "DOT_SOURCE_ROOT".into(),
            OsString::from(env!("CARGO_MANIFEST_DIR")),
        ),
        ("LC_ALL".into(), OsString::from("C")),
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        (
            "TMPDIR".into(),
            std::env::var_os("TMPDIR")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| OsString::from("/tmp")),
        ),
        ("SHELL".into(), OsString::from("/bin/bash")),
        ("GIT_AUTHOR_NAME".into(), OsString::from("fixture")),
        (
            "GIT_AUTHOR_EMAIL".into(),
            OsString::from("fixture@example.invalid"),
        ),
        ("GIT_COMMITTER_NAME".into(), OsString::from("fixture")),
        (
            "GIT_COMMITTER_EMAIL".into(),
            OsString::from("fixture@example.invalid"),
        ),
        (
            "DOT_BASH".into(),
            home_live
                .join("absent-old-update-engine")
                .as_os_str()
                .to_os_string(),
        ),
    ]);
    let runtime = dot::app::Runtime::from_env(&env, &home_live).expect("runtime");
    let config = dot::config::load(&dot::config::Request {
        config_path: None,
        home: &home_text,
        env_policy: None,
    })
    .expect("client config");
    let args: Vec<OsString> = Vec::new();
    let mut stdout = ChunkWriter::new();
    let mut stderr = Vec::new();
    let code = {
        let mut streams = dot::app::Streams::with_terminal(&mut stdout, &mut stderr, false);
        dot::update_engine::run_update(
            &runtime,
            &dot::update_engine::UpdateRequest {
                caller: dot::update_engine::Caller::Update,
                config: &config,
                env: &env,
                args: &args,
                state_home: &state_live,
            },
            &mut streams,
        )
    };
    assert_eq!(
        code,
        0,
        "in-process update failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    // The first stage opens long before the run closes: its row must reach
    // stdout in an earlier emission than the completion row.
    let first_stage = stdout.chunk_holding(b"[1/5]").expect("first stage row");
    let completion = stdout.chunk_holding(b"Done in").expect("completion row");
    assert!(
        first_stage < completion,
        "update buffered stage rows instead of streaming them"
    );
    assert_eq!(
        normalize_timing(&stdout.concatenated()),
        normalize_timing(&reference.stdout),
        "streamed update bytes differ from the process run"
    );
    assert_eq!(
        stderr, reference.stderr,
        "streamed update stderr differs from the process run"
    );
    // A delivery failure still exits 1 after the engine runs to completion:
    // this clean re-update would exit 0 with a working stdout.
    let mut failed_stdout = FailingWriter;
    let mut failed_stderr = Vec::new();
    let failed_code = {
        let mut streams =
            dot::app::Streams::with_terminal(&mut failed_stdout, &mut failed_stderr, false);
        dot::update_engine::run_update(
            &runtime,
            &dot::update_engine::UpdateRequest {
                caller: dot::update_engine::Caller::Update,
                config: &config,
                env: &env,
                args: &args,
                state_home: &state_live,
            },
            &mut streams,
        )
    };
    assert_eq!(failed_code, 1, "delivery failure must exit 1");
}

/// One converged client whose base remote then disappears, so every later
/// base pull fails the way an unreachable dotfiles remote does. The URL is
/// unchanged, so client identity still matches and only the pull fails.
fn unreachable_base_client(scratch: &Scratch, tag: &str) -> (PathBuf, PathBuf) {
    let (overlay_origin, base_origin) = shared_remotes(scratch);
    let (home, state) = twin_client(scratch, tag, &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    std::fs::rename(&base_origin, scratch.path().join("base.git.gone")).expect("hide base remote");
    // An empty directory keeps the recorded remote identity resolvable: macOS
    // resolves a file remote with BSD `realpath`, which refuses a missing
    // path, so a vanished remote trips the init-identity guard before the pull.
    std::fs::create_dir(&base_origin).expect("empty base remote");
    (home, state)
}

/// The `update.last-run` fields after its epoch.
fn last_run_fields(state: &Path) -> Vec<String> {
    let last = dot::update_status::read_last_run(state).expect("last-run stamp");
    let mut fields = vec![last.outcome, last.trigger];
    if !last.failing.is_empty() {
        fields.push(last.failing);
    }
    fields
}

#[test]
fn quiet_base_pull_failure_fails_like_a_loud_one() {
    // DOT-1: quiet mode used to warn about a failed base pull without
    // tallying it, so `DOT_QUIET=1` exited 0 and recorded `ok` while the
    // same run without it failed.
    let scratch = Scratch::new("update-run-quiet-base-fail").expect("scratch dir");
    let (home, state) = unreachable_base_client(&scratch, "quiet");
    // The quiet run goes first, so the stamp it leaves is its own (the
    // converging update before it recorded `ok`).
    let quiet = dot_env(&["update"], &home, &state, &[("DOT_QUIET", "1")]);
    assert_eq!(last_run_fields(&state), ["fail", "manual"]);
    // The restored generation keeps the installed overlay links.
    assert_eq!(
        std::fs::read(home.join("file-000.txt")).expect("overlay link kept"),
        b"overlay-0 payload 0\n"
    );
    let loud = dot_env(&["update"], &home, &state, &[]);
    assert_eq!(loud.status.code(), Some(1), "loud: {loud:?}");
    assert!(
        String::from_utf8_lossy(&loud.stdout).contains("1 repo failed"),
        "{loud:?}"
    );
    assert_eq!(quiet.status.code(), loud.status.code(), "quiet: {quiet:?}");
    // Quiet still hides every stage row; the stderr warning stands in for
    // the hidden Repos row.
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
    assert!(
        String::from_utf8_lossy(&quiet.stderr).contains("  warning: dotfiles pull failed\n"),
        "{quiet:?}"
    );
    let flag = dot_env(&["update", "--quiet"], &home, &state, &[]);
    assert_eq!(flag.status.code(), Some(1), "--quiet: {flag:?}");
    assert!(flag.stdout.is_empty(), "{flag:?}");
}

#[test]
fn cron_base_pull_failure_records_fail_and_keeps_success_stamps() {
    // DOT-1: a cron run whose base pull failed recorded `ok` and refreshed
    // the success stamps, so a host with an unreachable remote looked
    // healthy to `dot doctor` forever.
    let scratch = Scratch::new("update-run-cron-base-fail").expect("scratch dir");
    let (home, state) = unreachable_base_client(&scratch, "cron");
    // Old clean stamps: a refresh by this run would overwrite them.
    const OLD: i64 = 1_700_000_000;
    dot::update_status::record_success(&state, OLD);
    dot::update_status::record_converged(&state, OLD, dot::update_status::Degraded::default());
    let cron = dot_env(&["update", "--cron"], &home, &state, &[]);
    assert_eq!(cron.status.code(), Some(1), "cron: {cron:?}");
    assert!(cron.stdout.is_empty(), "{cron:?}");
    assert!(
        String::from_utf8_lossy(&cron.stderr).contains("  warning: dotfiles pull failed\n"),
        "{cron:?}"
    );
    let log = std::fs::read_to_string(dot::update_status::update_log_path(&state))
        .expect("cron outcome log");
    let last = log.lines().last().expect("one cron outcome");
    assert_eq!(
        last.split(' ').skip(1).collect::<Vec<_>>(),
        ["fail", "update"],
        "{log}"
    );
    assert_eq!(last_run_fields(&state), ["fail", "cron"]);
    // The repository that failed is the recorded cause of this very run.
    let failure = dot::update_status::read_last_failure(&state).expect("failure cause");
    let last = dot::update_status::read_last_run(&state).expect("last-run stamp");
    assert!(failure.describes(&last), "{failure:?} vs {last:?}");
    let items: Vec<(&str, &str)> = failure
        .items
        .iter()
        .map(|item| (item.stage.as_str(), item.name.as_str()))
        .collect();
    assert_eq!(items, [("repos", "dotfiles")]);
    assert_eq!(dot::update_status::read_last_success(&state), Some(OLD));
    let converged = dot::update_status::read_last_converged(&state).expect("convergence stamp");
    assert_eq!((converged.at, converged.failing.as_str()), (OLD, ""));
}

/// Run the native binary on a fresh pseudo-terminal as its controlling,
/// foreground terminal (stdin, stdout, and stderr all on the slave), the way
/// an interactive shell starts it. Returns the exit status and every byte the
/// terminal received, in arrival order across both output streams. The line
/// discipline's output processing stays on, so newlines arrive as `\r\n`.
fn dot_on_pty(argv: &[&str], home: &Path, state: &Path) -> (Option<i32>, Vec<u8>) {
    dot_on_pty_with(argv, home, state, &PtyRun::default())
}

/// What a [`dot_on_pty_with`] run sets up and does while dot runs.
#[derive(Default)]
struct PtyRun<'a> {
    /// Extra environment.
    env: Vec<(&'a str, std::ffi::OsString)>,
    /// Terminal width (0 leaves the size unset, as `openpty` defaults).
    columns: u16,
    /// Steps taken in order while dot runs: each waits for its file to
    /// exist, then for its delay, then types its bytes on the terminal.
    steps: Vec<(PathBuf, std::time::Duration, &'a [u8])>,
}

/// [`dot_on_pty`] with extra environment, a window size, and typed input.
fn dot_on_pty_with(
    argv: &[&str],
    home: &Path,
    state: &Path,
    run: &PtyRun<'_>,
) -> (Option<i32>, Vec<u8>) {
    use std::io::Write as _;
    use std::os::fd::FromRawFd as _;
    use std::os::unix::process::CommandExt as _;

    let mut master = -1;
    let mut slave = -1;
    // SAFETY: zeroed is a valid winsize; only the columns and rows are set.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    size.ws_col = run.columns;
    size.ws_row = if run.columns == 0 { 0 } else { 40 };
    // A raw pointer suits both signatures (`*mut` on macOS, `*const` on Linux).
    let size: *mut libc::winsize = &mut size;
    // SAFETY: openpty initializes both descriptors; the termios pointer is
    // null (platform defaults) and the window size is a live local.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                // macOS takes *mut termios/*mut winsize while Linux takes
                // *const; mutable pointers satisfy both through coercion.
                std::ptr::null_mut(),
                size,
            )
        },
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // Close-on-exec, so a child another test thread spawns meanwhile cannot
    // inherit either end and hold the terminal open past this run.
    for fd in [master, slave] {
        // SAFETY: F_SETFD on a descriptor openpty just returned.
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    // SAFETY: successful openpty returned two uniquely owned descriptors.
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let mut keyboard = master.try_clone().expect("PTY input end");
    let mut cmd = bin();
    client_env(&mut cmd, home, state);
    cmd.env("DOT_BASH", home.join("absent-old-update-engine"));
    for (key, value) in &run.env {
        cmd.env(key, value);
    }
    cmd.args(argv)
        .stdin(Stdio::from(slave.try_clone().expect("PTY stdin")))
        .stdout(Stdio::from(slave.try_clone().expect("PTY stdout")))
        .stderr(Stdio::from(slave));
    // SAFETY: the post-fork child is single threaded; these calls make fd 0's
    // PTY its controlling, foreground terminal before exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0
                || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0
                || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) < 0
            {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = cmd.spawn().expect("PTY-backed dot");
    // The command still owns its slave clones; drop them so the master sees
    // EOF once the child and its descendants close theirs.
    drop(cmd);
    // Drain the master for the child's whole lifetime: an unread master
    // stalls the child on a full terminal buffer, and BSD line disciplines
    // also hold exit teardown until pending output drains. The drainer ends
    // at EOF or EIO once every slave descriptor has closed.
    let (drained, collected) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut master = master;
        let mut output = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = drained.send(output);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut steps = run.steps.iter();
    let mut step = steps.next();
    let mut due: Option<std::time::Instant> = None;
    let status = loop {
        if let Some(status) = child.try_wait().expect("observe PTY dot") {
            break status;
        }
        if let Some((ready, delay, typed)) = step {
            if due.is_none() && ready.exists() {
                due = Some(std::time::Instant::now() + *delay);
            }
            if due.is_some_and(|due| std::time::Instant::now() >= due) {
                keyboard.write_all(typed).expect("type on the PTY");
                step = steps.next();
                due = None;
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("PTY-backed dot did not finish within its deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // The drainer gets its own grace: an exit seen right at the deadline
    // must not starve it of the time to hand over what it read.
    let grace = deadline
        .saturating_duration_since(std::time::Instant::now())
        .max(std::time::Duration::from_secs(5));
    let output = collected
        .recv_timeout(grace)
        .expect("PTY output never reached end of file after dot exited");
    assert!(
        step.is_none(),
        "dot finished ({status:?}) before every scripted step ran: {:?}",
        String::from_utf8_lossy(&output)
    );
    (status.code(), output)
}

/// A directory holding a `git` that runs the real one, except that a
/// `fetch` whose arguments contain `$DOT_TEST_FETCH_MATCH` first acts like
/// a fetch that talks to the user: it records `$DOT_TEST_FETCH_MARK.asked`,
/// asks on `/dev/tty` and reads the answer there (an SSH passphrase), or
/// with `DOT_TEST_FETCH_HANG=1` writes an unterminated line to stderr and
/// waits to be interrupted. It also reports whether its stderr is a
/// terminal. Put it first on `PATH`.
fn prompting_git(scratch: &Scratch) -> PathBuf {
    // The real Git, never a developer's launcher shim on PATH.
    let real = dot_test_support::real_tool("git");
    let dir = scratch.path().join("prompting-git-bin");
    std::fs::create_dir_all(&dir).expect("wrapper dir");
    let script = format!(
        r#"#!/bin/sh
case " $* " in
*" fetch "*)
  case "${{DOT_TEST_FETCH_MATCH:+$*}}" in
  "") ;;
  *"$DOT_TEST_FETCH_MATCH"*)
    if [ -t 2 ]; then echo 'stderr is a terminal' >&2; else echo 'stderr is not a terminal' >&2; fi
    : >"$DOT_TEST_FETCH_MARK.asked"
    if [ "${{DOT_TEST_FETCH_HANG:-0}}" = 1 ]; then
      trap 'echo "fetch: interrupted, cleaning up" >&2; exit 130' INT
      printf 'fetch: waiting' >&2
      while :; do sleep 1; done
    fi
    printf 'Enter passphrase for key: ' >/dev/tty
    IFS= read -r answer </dev/tty
    ;;
  esac
  ;;
esac
exec '{}' "$@"
"#,
        real.display()
    );
    let git = dir.join("git");
    std::fs::write(&git, script).expect("wrapper");
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).expect("wrapper mode");
    dir
}

/// A converged client initialized with `prompting_git` first on `PATH`
/// (the initialization identity records the Git it ran), plus that `PATH`
/// for later runs.
fn prompting_git_client(scratch: &Scratch, tag: &str) -> (PathBuf, PathBuf, std::ffi::OsString) {
    let (overlay_origin, base_origin) = shared_remotes(scratch);
    let path = path_with(&prompting_git(scratch));
    let home = scratch.path().join(format!("home-{tag}"));
    let state = scratch.path().join(format!("state-{tag}"));
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&state).expect("state");
    let with_path = |argv: &[&str]| {
        let mut cmd = bin();
        client_env(&mut cmd, &home, &state);
        cmd.env("PATH", &path)
            .env("DOT_BASH", home.join("absent-old-update-engine"))
            .args(argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().expect("run native dot");
        assert!(output.status.success(), "dot {argv:?}: {output:?}");
    };
    with_path(&[
        "init",
        "--yes",
        &format!("file://{}", base_origin.display()),
    ]);
    let conf_dir = home.join(".config/dot/overlays.d");
    std::fs::create_dir_all(&conf_dir).expect("conf dir");
    std::fs::write(
        conf_dir.join("overlay-0.conf"),
        format!("url=file://{}\n", overlay_origin.display()),
    )
    .expect("write conf");
    with_path(&["update"]);
    (home, state, path)
}

/// `PATH` with `dir` in front.
fn path_with(dir: &Path) -> std::ffi::OsString {
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(dirs).expect("PATH")
}

/// Push one change to overlay-0's remote so the next update must fetch it
/// (a probe would otherwise prove the fetch unnecessary and skip it).
fn push_overlay_change(scratch: &Scratch) {
    let seed = scratch.path().join("overlay-0-seed");
    let origin = scratch.path().join("overlay-0.git");
    std::fs::write(
        seed.join("home/file-000.txt"),
        "overlay-0 payload CHANGED\n",
    )
    .expect("write");
    git(&seed, &["add", "home/file-000.txt"]);
    git(&seed, &["commit", "-qm", "change"]);
    git(
        &seed,
        &["push", "-q", &origin.to_string_lossy(), "HEAD:main"],
    );
}

/// The bytes a terminal shows before `index` on the same visual line: back to
/// the last newline or the last carriage-return line erase (`\r\x1b[K`),
/// whichever is later. A message that starts on its own line has only its
/// indentation here; one glued to a progress row has the row's text.
fn visual_line_prefix(output: &[u8], index: usize) -> &[u8] {
    let head = &output[..index];
    let after_newline = head
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    const ERASE: &[u8] = b"\r\x1b[K";
    let after_erase = head
        .windows(ERASE.len())
        .rposition(|window| window == ERASE)
        .map_or(0, |at| at + ERASE.len());
    &head[after_newline.max(after_erase)..]
}

/// `bytes` without SGR colour sequences (`ESC [ <params> m`), keeping every
/// other control byte, so layout checks read the same with or without colour.
fn strip_colour(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\x1b[") {
            let params = bytes[index + 2..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
                .count();
            if bytes.get(index + 2 + params) == Some(&b'm') {
                index += params + 3;
                continue;
            }
        }
        plain.push(bytes[index]);
        index += 1;
    }
    plain
}

/// Every start offset of `needle` in `haystack`.
fn occurrences(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(at, _)| at)
        .collect()
}

#[test]
fn terminal_update_prints_child_errors_on_their_own_indented_lines() {
    // The base fetch fails while the Repos row is live on the terminal. Its
    // `fatal:` lines used to land at the end of that unfinished row and wrap;
    // each must start on a fresh line, indented under the stage.
    let scratch = Scratch::new("update-run-pty-fetch-failure").expect("scratch dir");
    let (home, state) = unreachable_base_client(&scratch, "pty");
    let (code, output) = dot_on_pty(&["update"], &home, &state);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(1), "{text}");
    let fatal = occurrences(&output, b"fatal:");
    assert!(
        !fatal.is_empty(),
        "no fetch diagnostic reached the terminal: {text}"
    );
    for at in fatal {
        assert_eq!(
            String::from_utf8_lossy(visual_line_prefix(&output, at)),
            "    ",
            "fetch diagnostic shares a line with other output: {text}"
        );
    }
    // The run still closes the stage table: the failed Repos row and the
    // completion line each sit on their own line after the diagnostic.
    for row in [&b"[1/5] Repos      failed"[..], b"Done with errors in "] {
        let at = *occurrences(&output, row)
            .first()
            .unwrap_or_else(|| panic!("missing {:?}: {text}", String::from_utf8_lossy(row)));
        assert!(
            visual_line_prefix(&output, at).is_empty(),
            "{:?} does not start its own line: {text}",
            String::from_utf8_lossy(row)
        );
    }
}

#[test]
fn piped_update_prints_child_errors_as_plain_indented_lines() {
    // Without a terminal (cron, pipes) the same failure prints one message
    // per line with no escape sequences or carriage returns, the fetch
    // diagnostic indented under the stage like hook output.
    let scratch = Scratch::new("update-run-piped-fetch-failure").expect("scratch dir");
    let (home, state) = unreachable_base_client(&scratch, "piped");
    let output = dot_env(&["update"], &home, &state, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    for (name, stream) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            !stream.iter().any(|byte| *byte == b'\r' || *byte == 0x1b),
            "{name} carries terminal control bytes: {output:?}"
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let fatal: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("fatal:"))
        .collect();
    assert!(!fatal.is_empty(), "{output:?}");
    for line in fatal {
        assert!(
            line.starts_with("    fatal: "),
            "unindented diagnostic {line:?}: {output:?}"
        );
    }
}

#[test]
fn invalid_overlay_descriptor_prints_its_warning_not_a_debug_dump() {
    // Overlay discovery failures reached stderr through `{:?}`, printing the
    // Rust enum (`Warning("invalid overlay descriptor ...")`) instead of the
    // warning line every other path prints.
    let scratch = Scratch::new("update-run-invalid-descriptor").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "invalid", &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    let descriptor = home.join(".config/dot/overlays.d/zz-bad.conf");
    std::fs::write(&descriptor, b"url=file:///nonexistent\nsync=bogus\n").expect("descriptor");
    let output = dot(&["update"], &home, &state);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "  warning: invalid overlay descriptor {}: unknown sync value: bogus\n",
            descriptor.display()
        ),
    );
    // The Repos row read `failed  1 repo current`; it now says what failed
    // and names the overlay.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[1/5] Repos      failed   overlay zz-bad: invalid descriptor "),
        "{stdout}"
    );
}

/// A client whose update runs a pre-sync extension that records
/// `$HOME/hook-ready` and then waits, holding the Repos stage open.
fn waiting_hook_client(scratch: &Scratch, tag: &str) -> (PathBuf, PathBuf) {
    let (overlay_origin, base_origin) = shared_remotes(scratch);
    let (home, state) = twin_client(scratch, tag, &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    let extensions = home.join("ext");
    let pre_sync = extensions.join("pre-sync.d");
    std::fs::create_dir_all(&pre_sync).expect("pre-sync dir");
    let hook = pre_sync.join("10-wait.sh");
    std::fs::write(&hook, b"prepare() { : >\"$HOME/hook-ready\"; sleep 30; }\n").expect("hook");
    for path in [&extensions, &pre_sync, &hook] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    std::fs::write(
        home.join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/ext\ndependency_provider=none\n",
    )
    .expect("config");
    (home, state)
}

#[test]
fn interrupted_update_ends_the_open_row_on_the_terminal() {
    // Ctrl-C mid-stage used to leave the progress row unterminated, so the
    // shell prompt landed on it (`...2s^Cuser@host:~$`): after a signal the
    // output relay refuses writes, so nothing ended the row.
    let scratch = Scratch::new("update-run-pty-interrupt").expect("scratch dir");
    let (home, state) = waiting_hook_client(&scratch, "interrupt");
    // Extensions run under a real Bash (the fixture default points at none).
    let bash = dot_test_support::real_tool("bash");
    let run = PtyRun {
        env: vec![("DOT_BASH", bash.into_os_string())],
        steps: vec![(
            home.join("hook-ready"),
            std::time::Duration::from_millis(300),
            b"\x03",
        )],
        ..PtyRun::default()
    };
    let (code, output) = dot_on_pty_with(&["update"], &home, &state, &run);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(130), "{text}");
    assert!(output.ends_with(b"\r\n"), "line left open: {text:?}");
    // The row ends its own line: whatever follows (the warning the
    // teardown printed) starts on the next one, and the last progress the
    // run reached stays visible.
    let row = *occurrences(&output, b"[1/5] Repos")
        .last()
        .expect("a Repos row");
    let rest = &output[row..];
    let end = rest
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("row ended");
    let row_line = String::from_utf8_lossy(&rest[..end]);
    assert!(!row_line.contains("warning"), "{row_line:?} in {text:?}");
    // The warning the teardown printed was refused by the relay (after the
    // signal) and still reaches the terminal, on its own line, worded as an
    // interruption rather than a failure.
    assert!(
        String::from_utf8_lossy(&rest[end..])
            .contains("\n  warning: pre-sync extension interrupted: 10-wait.sh\r\n"),
        "{text:?}"
    );
}

#[test]
fn interrupted_fetch_keeps_its_last_words_on_the_terminal() {
    // A child's diagnostic written while Ctrl-C tears the run down was
    // dropped (the relay refuses writes after a signal), and its unfinished
    // line was left open under the shell prompt.
    let scratch = Scratch::new("update-run-pty-fetch-interrupt").expect("scratch dir");
    let (home, state, path) = prompting_git_client(&scratch, "fetch-interrupt");
    let mark = scratch.path().join("fetch");
    let run = PtyRun {
        env: vec![
            ("PATH", path),
            ("DOT_TEST_FETCH_MATCH", "--work-tree".into()),
            ("DOT_TEST_FETCH_MARK", mark.clone().into_os_string()),
            ("DOT_TEST_FETCH_HANG", "1".into()),
        ],
        steps: vec![(
            PathBuf::from(format!("{}.asked", mark.display())),
            std::time::Duration::from_millis(500),
            b"\x03",
        )],
        ..PtyRun::default()
    };
    let (code, output) = dot_on_pty_with(&["update"], &home, &state, &run);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(130), "{text}");
    // The child's unfinished `fetch: waiting` line was shown and is ended,
    // so the shell prompt does not land on it. Whether the child's own
    // cleanup line is read before the teardown is a race; when it is, it
    // starts a line of its own.
    assert!(text.contains("    fetch: waiting"), "{text:?}");
    assert!(output.ends_with(b"\r\n"), "line left open: {text:?}");
    if let Some(at) = occurrences(&output, b"fetch: interrupted, cleaning up").first() {
        assert!(
            output[..*at].ends_with(b"\n"),
            "teardown diagnostic shares a line: {text:?}"
        );
    }
}

#[test]
fn base_fetch_prompt_starts_on_a_clear_line_and_keeps_a_terminal() {
    // A fetch that asks on the terminal (an SSH passphrase) used to print
    // its question at the end of the live Repos row. Its stderr must also
    // still be a terminal, so notices printed only to a terminal (an SSH
    // security-key touch request) still show.
    let scratch = Scratch::new("update-run-pty-base-prompt").expect("scratch dir");
    let (home, state, path) = prompting_git_client(&scratch, "base-prompt");
    let mark = scratch.path().join("fetch");
    let run = PtyRun {
        env: vec![
            ("PATH", path),
            ("DOT_TEST_FETCH_MATCH", "--work-tree".into()),
            ("DOT_TEST_FETCH_MARK", mark.clone().into_os_string()),
        ],
        steps: vec![(
            PathBuf::from(format!("{}.asked", mark.display())),
            std::time::Duration::from_millis(1500),
            b"secret\n",
        )],
        ..PtyRun::default()
    };
    let (code, output) = dot_on_pty_with(&["update"], &home, &state, &run);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(0), "{text}");
    let asked = *occurrences(&output, b"Enter passphrase for key: ")
        .first()
        .unwrap_or_else(|| panic!("no prompt: {text:?}"));
    assert!(
        visual_line_prefix(&output, asked).is_empty(),
        "prompt shares a line: {text:?}"
    );
    assert!(
        !occurrences(&output, b"    stderr is a terminal").is_empty(),
        "fetch stderr is not a terminal: {text:?}"
    );
    // The screen is never blank while the fetch may ask: a static line says
    // what runs, and the question starts below it.
    let fetching = *occurrences(&output, b"[1/5] Repos      running  fetching dotfiles")
        .first()
        .unwrap_or_else(|| panic!("no static fetch line: {text:?}"));
    assert!(fetching < asked, "{text:?}");
    let line_end = fetching + output[fetching..].iter().position(|b| *b == b'\n').unwrap();
    assert!(
        line_end < asked && output[line_end - 1] == b'\r',
        "{text:?}"
    );
}

#[test]
fn overlay_fetch_prompt_is_not_redrawn_over() {
    // The heartbeat redrew the Repos row every second while a required
    // overlay's fetch waited for an answer, wiping its question.
    let scratch = Scratch::new("update-run-pty-overlay-prompt").expect("scratch dir");
    let (home, state, path) = prompting_git_client(&scratch, "overlay-prompt");
    push_overlay_change(&scratch);
    let mark = scratch.path().join("fetch");
    let run = PtyRun {
        env: vec![
            ("PATH", path),
            ("DOT_TEST_FETCH_MATCH", "dotfiles-overlay-0".into()),
            ("DOT_TEST_FETCH_MARK", mark.clone().into_os_string()),
        ],
        steps: vec![(
            PathBuf::from(format!("{}.asked", mark.display())),
            std::time::Duration::from_millis(2500),
            b"secret\n",
        )],
        ..PtyRun::default()
    };
    let (code, output) = dot_on_pty_with(&["update"], &home, &state, &run);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(0), "{text}");
    let asked = *occurrences(&output, b"Enter passphrase for key: ")
        .first()
        .unwrap_or_else(|| panic!("no prompt: {text:?}"));
    assert!(
        visual_line_prefix(&output, asked).is_empty(),
        "prompt shares a line: {text:?}"
    );
    let answered = asked
        + occurrences(&output[asked..], b"secret")
            .first()
            .copied()
            .unwrap_or_else(|| panic!("answer never echoed: {text:?}"));
    assert!(
        occurrences(&output[asked..answered], b"\r\x1b[K").is_empty(),
        "redrawn over the prompt: {:?}",
        String::from_utf8_lossy(&output[asked..answered])
    );
    // A static line said what runs, so the screen was not left blank.
    let pulling = *occurrences(&output, b"[1/5] Repos      running  pulling 1 overlay")
        .first()
        .unwrap_or_else(|| panic!("no static pull line: {text:?}"));
    assert!(pulling < asked, "{text:?}");
}

#[test]
fn narrow_terminal_rows_never_wrap() {
    // Rows were a fixed 75 columns: on a narrower terminal every redraw
    // wrapped and its erase cleared only the last physical line, leaving
    // stale half-rows behind.
    let scratch = Scratch::new("update-run-pty-narrow").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "narrow", &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    let run = PtyRun {
        columns: 60,
        ..PtyRun::default()
    };
    let (code, output) = dot_on_pty_with(&["update"], &home, &state, &run);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(0), "{text}");
    for line in text.split("\r\n") {
        for drawn in line.split("\r\u{1b}[K") {
            assert!(
                drawn.chars().count() < 60,
                "{} columns: {drawn:?} in {text:?}",
                drawn.chars().count()
            );
        }
    }
    assert!(text.contains("[5/5] Cleanup"), "{text:?}");
}

/// A converged client whose overlay-0 remote then disappears, so the next
/// update's overlay fetch fails.
fn unreachable_overlay_client(scratch: &Scratch, tag: &str) -> (PathBuf, PathBuf) {
    let (overlay_origin, base_origin) = shared_remotes(scratch);
    let (home, state) = twin_client(scratch, tag, &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    std::fs::rename(&overlay_origin, scratch.path().join("overlay-0.git.gone"))
        .expect("hide overlay remote");
    std::fs::create_dir(&overlay_origin).expect("empty overlay remote");
    (home, state)
}

#[test]
fn overlay_fetch_errors_print_under_the_line_naming_the_overlay() {
    // The fetch's own lines printed ahead of `overlay-0 dotfiles pull
    // failed` (so in `-v` they sat under the base's row), and with that
    // row hidden by `--quiet`/`--cron` nothing named the overlay at all.
    let scratch = Scratch::new("update-run-overlay-fetch-order").expect("scratch dir");
    let (home, state) = unreachable_overlay_client(&scratch, "order");
    for verbose in [false, true] {
        let argv: &[&str] = if verbose {
            &["update", "-v"]
        } else {
            &["update"]
        };
        let output = dot(argv, &home, &state);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let named = stdout
            .find("overlay-0 dotfiles pull failed")
            .unwrap_or_else(|| panic!("no failure row: {output:?}"));
        let fatal = stdout
            .find("    fatal:")
            .unwrap_or_else(|| panic!("no fetch diagnostic: {output:?}"));
        assert!(named < fatal, "diagnostic before its overlay: {stdout}");
    }
    for argv in [&["update", "--quiet"][..], &["update", "--cron"][..]] {
        let output = dot(argv, &home, &state);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("  overlay-0 fetch output:\n    fatal:"),
            "{argv:?}: {output:?}"
        );
        for stream in [&output.stdout, &output.stderr] {
            assert!(
                !stream.iter().any(|byte| *byte == b'\r' || *byte == 0x1b),
                "{argv:?}: {output:?}"
            );
        }
    }
}

#[test]
fn failed_pre_sync_keeps_the_repos_counts_the_shell_engine_printed() {
    // Only an overlay descriptor fault replaces the Repos counts: every other
    // convergence failure keeps the row the shell engine printed, which the
    // release performance gate compares byte for byte (its pre-sync-failure
    // workload is exactly this client).
    let scratch = Scratch::new("update-run-pre-sync-row").expect("scratch dir");
    let (home, state) = waiting_hook_client(&scratch, "pre-sync-row");
    std::fs::write(
        home.join("ext/pre-sync.d/10-wait.sh"),
        b"prepare() { return 7; }\n",
    )
    .expect("failing hook");
    let output = dot_env(
        &["update"],
        &home,
        &state,
        &[(
            "DOT_BASH",
            dot_test_support::real_tool("bash")
                .to_str()
                .expect("bash path"),
        )],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[1/5] Repos      failed   1 repo current "),
        "{stdout}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("  warning: pre-sync extension failed: 10-wait.sh\n"),
        "{output:?}"
    );
}

#[test]
fn interrupted_merge_hook_reports_interrupted_not_failed() {
    // A merge hook stopped by Ctrl-C was reported as `merge failed`.
    let scratch = Scratch::new("update-run-pty-merge-interrupt").expect("scratch dir");
    let (home, state) = waiting_hook_client(&scratch, "merge-interrupt");
    let hooks = home.join("ext/merge-hooks.d");
    std::fs::create_dir_all(&hooks).expect("merge hooks");
    std::fs::remove_file(home.join("ext/pre-sync.d/10-wait.sh")).expect("drop pre-sync hook");
    let hook = hooks.join("10-slow.sh");
    std::fs::write(&hook, b"merge() { : >\"$HOME/hook-ready\"; sleep 30; }\n").expect("hook");
    for path in [&hooks, &hook] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    let run = PtyRun {
        env: vec![(
            "DOT_BASH",
            dot_test_support::real_tool("bash").into_os_string(),
        )],
        steps: vec![(
            home.join("hook-ready"),
            std::time::Duration::from_millis(300),
            b"\x03",
        )],
        ..PtyRun::default()
    };
    let (code, output) = dot_on_pty_with(&["update"], &home, &state, &run);
    let output = strip_colour(&output);
    let text = String::from_utf8_lossy(&output);
    assert_eq!(code, Some(130), "{text}");
    assert!(text.contains("warning: merge interrupted"), "{text:?}");
    assert!(!text.contains("merge failed"), "{text:?}");
}

#[test]
fn quiet_base_fetch_output_is_named() {
    // With the Repos row hidden, the base fetch's lines are named like an
    // overlay's instead of standing alone.
    let scratch = Scratch::new("update-run-quiet-base-header").expect("scratch dir");
    let (home, state) = unreachable_base_client(&scratch, "quiet-header");
    for argv in [&["update", "--quiet"][..], &["update", "--cron"][..]] {
        let output = dot(argv, &home, &state);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("  dotfiles fetch output:\n    fatal:"),
            "{argv:?}: {output:?}"
        );
    }
    // The visible row names it otherwise: no header.
    let output = dot(&["update"], &home, &state);
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("    fatal:"),
        "{output:?}"
    );
}

#[test]
fn descriptor_names_reach_the_terminal_without_control_bytes() {
    // The overlay name in the Repos row and the warning come from a file name
    // that failed validation, so it may hold escapes.
    let scratch = Scratch::new("update-run-descriptor-escape").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "escape", &overlay_origin, &base_origin);
    check_update(&["update"], &home, &state);
    std::fs::write(
        home.join(".config/dot/overlays.d/zz\x1b[31mbad.conf"),
        b"url=file:///nonexistent\nsync=bogus\n",
    )
    .expect("descriptor");
    let output = dot(&["update"], &home, &state);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    for stream in [&output.stdout, &output.stderr] {
        assert!(!stream.contains(&0x1b), "{output:?}");
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("overlay zz [31mbad: invalid descriptor"),
        "{output:?}"
    );
}
