//! `dot update` end-to-end execution parity (slice 80).
//!
//! The dispatcher arm ([`dot::cli::run`] with `update`/`pull`) runs the
//! update lifecycle for real and reports its exit code, instead of the
//! interim not-yet-implemented diagnostic. These tests pin the wired arm
//! against the production shell (`bin/dot`) on synthetic `file://`
//! fixtures, in both steady states:
//!
//! - clean: nothing changed since `init` (the cron steady state);
//! - dirty: one pushed overlay change waiting to converge.
//!
//! Each side runs on its own twin HOME/state pair built from the same
//! remotes, so the two updates never share mutable state. Stdout carries
//! wall-clock stamps (`0s`, `Done in 0s`) that legitimately differ run to
//! run; [`normalize_timing`] blanks those before the byte comparison while
//! leaving every other byte exact. Stderr is compared byte for byte with
//! no normalization. The converged HOME trees are compared with the
//! byte-comparison technique from `tests/perf_update.rs` (regular files
//! only, sorted; `.git`, `.dotfiles`, and the timestamped init-time
//! `.dot-backup` are excluded because they carry clock or checkout
//! identity rather than converged content).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Scratch helper shared with the other parity suites: pid plus a
/// monotonic counter, no wall-clock reads.
type Scratch = dot::test_support::TempDir;

/// The Rust binary under test.
fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dot"));
    // One `.env` per variable (never `.envs`): MSRV-clean and matches
    // the oracle convention in `tests/cli.rs`.
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd
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
    cmd.env("XDG_STATE_HOME", state);
    cmd.env("XDG_CONFIG_HOME", "");
    cmd.env("DOT_SOURCE_ROOT", repo);
    cmd.env("GIT_AUTHOR_NAME", "fixture");
    cmd.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
    cmd.env("GIT_COMMITTER_NAME", "fixture");
    cmd.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    cmd.current_dir(home);
}

/// The production shell oracle (`bin/dot` under `set -euo pipefail`)
/// with the same controlled client.
fn shell_dot(argv: &[&str], home: &Path, state: &Path) -> std::process::Output {
    let mut cmd = Command::new("bash");
    cmd.arg("bin/dot");
    for arg in argv {
        cmd.arg(arg);
    }
    client_env(&mut cmd, home, state);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"));
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run bin/dot")
}

/// The Rust binary with the same controlled client.
fn rust_dot(argv: &[&str], home: &Path, state: &Path) -> std::process::Output {
    let mut cmd = bin();
    client_env(&mut cmd, home, state);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run Rust dot")
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
    let shell = shell_dot(
        &[
            "init",
            "--yes",
            &format!("file://{}", base_origin.display()),
        ],
        &home,
        &state,
    );
    assert!(
        shell.status.success(),
        "twin {tag} init failed: {}",
        String::from_utf8_lossy(&shell.stderr)
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
/// byte comparison between shell and Rust runs. `.git` carries
/// checkout identity, `.dotfiles` carries the base checkout, and
/// `.dot-backup` carries timestamped init-time safekeeping: none of
/// them is converged content, so all three stay out of the
/// comparison, exactly like the `tests/perf_update.rs` technique.
fn snapshot_tree(home: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).expect("read dir");
        for entry in read {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            let kind = entry.file_type().expect("file type");
            // Checkout identity and timestamped safekeeping are never
            // converged content, whatever filesystem kind they take
            // (a worktree `.git` may be a file, not a directory).
            let skip = path
                .file_name()
                .is_some_and(|n| n == ".git" || n == ".dotfiles" || n == ".dot-backup");
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

/// One wired-arm row on twin clients: exit codes match, stdout
/// matches after timing normalization, stderr matches byte for byte,
/// and the converged trees match byte for byte.
fn check_update(
    argv: &[&str],
    home_shell: &Path,
    state_shell: &Path,
    home_rust: &Path,
    state_rust: &Path,
) {
    let shell = shell_dot(argv, home_shell, state_shell);
    let rust = rust_dot(argv, home_rust, state_rust);
    assert_eq!(rust.status.code(), shell.status.code(), "argv: {argv:?}");
    assert_eq!(
        normalize_timing(&rust.stdout),
        normalize_timing(&shell.stdout),
        "argv: {argv:?}\nrust:\n{}\nshell:\n{}",
        String::from_utf8_lossy(&rust.stdout),
        String::from_utf8_lossy(&shell.stdout),
    );
    assert_eq!(rust.stderr, shell.stderr, "argv: {argv:?}");
    assert_eq!(
        snapshot_tree(home_rust),
        snapshot_tree(home_shell),
        "argv: {argv:?} converged trees differ"
    );
}

#[test]
fn clean_update_matches_shell_and_converges() {
    let scratch = Scratch::new("update-run-clean").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlay_origin, &base_origin);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlay_origin, &base_origin);
    // Warm-up converges both twins like cron does, so the measured
    // run is the clean steady state.
    for (home, state) in [(&home_shell, &state_shell), (&home_rust, &state_rust)] {
        let warm = shell_dot(&["update"], home, state);
        assert!(
            warm.status.success(),
            "warm-up failed: {}",
            String::from_utf8_lossy(&warm.stderr)
        );
    }
    check_update(
        &["update"],
        &home_shell,
        &state_shell,
        &home_rust,
        &state_rust,
    );
    let rust = rust_dot(&["update"], &home_rust, &state_rust);
    assert_eq!(rust.status.code(), Some(0));
}

#[test]
fn dirty_update_matches_shell_and_converges() {
    let scratch = Scratch::new("update-run-dirty").expect("scratch dir");
    let (_overlay_origin, base_origin) = shared_remotes(&scratch);
    let overlay_seed = scratch.path().join("overlay-0-seed");
    let overlay_origin = scratch.path().join("overlay-0.git");
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlay_origin, &base_origin);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlay_origin, &base_origin);
    // Push one changed file to the shared overlay remote: both twins
    // converge the same change.
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
    check_update(
        &["update"],
        &home_shell,
        &state_shell,
        &home_rust,
        &state_rust,
    );
    for home in [&home_shell, &home_rust] {
        let bytes = std::fs::read(home.join("file-000.txt")).expect("converged file");
        assert_eq!(bytes, b"overlay-0 payload CHANGED\n", "home: {home:?}");
    }
}

#[test]
fn pull_alias_matches_shell_update() {
    let scratch = Scratch::new("update-run-pull").expect("scratch dir");
    let (overlay_origin, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlay_origin, &base_origin);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlay_origin, &base_origin);
    // Warm-up converges both twins, so `pull` meets the clean steady
    // state exactly like `update` does.
    for (home, state) in [(&home_shell, &state_shell), (&home_rust, &state_rust)] {
        let warm = shell_dot(&["update"], home, state);
        assert!(
            warm.status.success(),
            "warm-up failed: {}",
            String::from_utf8_lossy(&warm.stderr)
        );
    }
    let shell = shell_dot(&["update"], &home_shell, &state_shell);
    assert!(shell.status.success());
    let rust = rust_dot(&["pull"], &home_rust, &state_rust);
    assert_eq!(rust.status.code(), shell.status.code());
    assert_eq!(
        normalize_timing(&rust.stdout),
        normalize_timing(&shell.stdout),
        "pull vs update:\nrust:\n{}\nshell:\n{}",
        String::from_utf8_lossy(&rust.stdout),
        String::from_utf8_lossy(&shell.stdout),
    );
    assert_eq!(rust.stderr, shell.stderr);
}

#[test]
fn lock_busy_reports_75_like_shell() {
    // Hold the update lock from this process (a live owner), then run
    // both binaries against the same state: each must refuse with
    // exit 75 and the shell's exact busy diagnostic, naming our pid.
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
    let shell = shell_dot(&["update"], &home, &state);
    assert_eq!(shell.status.code(), Some(75));
    assert_eq!(shell.stderr, expected.as_bytes());
    let rust = rust_dot(&["update"], &home, &state);
    assert_eq!(rust.status.code(), Some(75));
    assert_eq!(rust.stderr, expected.as_bytes());
    assert!(rust.stdout.is_empty());
    assert!(shell.stdout.is_empty());
    let _ = guard;
}
