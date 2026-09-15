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
    let mut cmd = bin();
    client_env(&mut cmd, home, state);
    cmd.env("DOT_BASH", home.join("absent-old-update-engine"));
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run native dot")
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
    let output = Command::new("git")
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
    let output = Command::new("git")
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
/// `.scm.sqlite` is SCM's async telemetry database (a lingering SCM
/// helper may create it after Dot returns): none of them is converged
/// content, so all four stay out of the comparison, exactly like the
/// `tests/perf_update.rs` technique.
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
                n == ".git" || n == ".dotfiles" || n == ".dot-backup" || n == ".scm.sqlite"
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
