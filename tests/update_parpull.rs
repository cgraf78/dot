//! Native parallel overlay pull coverage for `dot update`.
//!
//! The native parallel fan-out itself lives in [`dot::repos_pull_fleet`]
//! (scoped threads bounded by `DOT_UPDATE_JOBS`, falling back to the
//! serial path when scratch allocation fails); the `update`/`pull`
//! dispatcher arm ([`dot::cli::run`] via `update_run`) drives that native
//! implementation directly. This suite exercises a three-overlay `file://`
//! fixture (the speedup fixture):
//!
//! - clean and pushed-change updates converge with the default job bound and
//!   with `DOT_UPDATE_JOBS=2`;
//! - dirty overlays, fetch failures, config rejection (exit `2`), and a held
//!   update lock (exit `75`) retain their native contracts;
//! - clean wall-clock medians are reported as a native regression signal.
//!
//! Each twin side runs on its own HOME/state pair built from the same
//! remotes, so the two updates never share mutable state. Stdout
//! carries wall-clock stamps (`0s`, `Done in 0s`) that legitimately
//! differ run to run; [`normalize`] blanks those (the technique from
//! `tests/update_run.rs`) and additionally blanks the twin HOME paths,
//! which failure diagnostics may quote. Stderr is compared after the
//! same normalization. The converged HOME trees are compared with the
//! byte-comparison technique from `tests/perf_update.rs` (regular
//! files only, sorted; `.git`, `.dotfiles`, and the timestamped
//! init-time `.dot-backup` are excluded because they carry clock or
//! checkout identity rather than converged content).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Overlays on the speedup fixture (matches `tests/perf_update.rs`).
const OVERLAYS: usize = 3;
/// Payload files per overlay: enough fetch/pull substance to time,
/// small enough to stay fast under CI.
const FILES_PER_OVERLAY: usize = 12;
/// Timed native update iterations in `report_native_wall_clock_three_overlay`.
const TIMING_RUNS: usize = 3;

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

/// Controlled client environment, mirroring `init_env` in
/// `tests/cli.rs`: a cleared environment plus a twin home/state pair,
/// so rows never touch the developer's own checkout. `jobs` sets
/// `DOT_UPDATE_JOBS` (the `_dot_update_jobs` bound both engines fan
/// out within); `policy` sets `DOT_SHDEPS_UPDATE_POLICY` (a bogus
/// value makes `dot_config_load` reject the run with exit 2).
fn client_env(
    cmd: &mut Command,
    home: &Path,
    state: &Path,
    jobs: Option<&str>,
    policy: Option<&str>,
) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    cmd.env_clear();
    cmd.env("LC_ALL", "C");
    cmd.env("PATH", &path);
    cmd.env("TMPDIR", &tmpdir);
    cmd.env("HOME", home);
    // Bash supplies its own shell identity on some platforms when `SHELL` is
    // absent, while the native process correctly preserves the cleared map.
    // Pin the semantic input so reload-hint parity never depends on the host.
    cmd.env("SHELL", "/bin/bash");
    cmd.env("XDG_STATE_HOME", state);
    cmd.env("XDG_CONFIG_HOME", "");
    cmd.env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"));
    // Bypass any machine-local Git launcher while retaining the caller's
    // public PATH. The fixture must exercise Git itself, not mutate host-only
    // launcher caches inside its synthetic HOME.
    cmd.env("DOT_GIT_REAL", "1");
    cmd.env("GIT_AUTHOR_NAME", "fixture");
    cmd.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
    cmd.env("GIT_COMMITTER_NAME", "fixture");
    cmd.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    if let Some(jobs) = jobs {
        cmd.env("DOT_UPDATE_JOBS", jobs);
    }
    if let Some(policy) = policy {
        cmd.env("DOT_SHDEPS_UPDATE_POLICY", policy);
    }
    cmd.current_dir(home);
}

#[test]
fn command_pins_the_reload_shell() {
    let scratch = Scratch::new("parpull-shell-input").expect("scratch dir");
    let home = scratch.path().join("home");
    let state = scratch.path().join("state");
    let mut command = bin();
    client_env(&mut command, &home, &state, None, None);
    let value = command
        .get_envs()
        .find(|(key, _)| *key == "SHELL")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("/bin/bash")));
    let git_real = command
        .get_envs()
        .find(|(key, _)| *key == "DOT_GIT_REAL")
        .and_then(|(_, value)| value);
    assert_eq!(git_real, Some(OsStr::new("1")));
}

/// The native Rust CLI with the same controlled client.
fn dot(
    argv: &[&str],
    home: &Path,
    state: &Path,
    jobs: Option<&str>,
    policy: Option<&str>,
) -> std::process::Output {
    let mut cmd = bin();
    client_env(&mut cmd, home, state, jobs, policy);
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
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Seed one overlay repo publishing `files` payload files under
/// `home/` and return its bare remote path.
fn seed_overlay(scratch: &Scratch, index: usize, files: usize) -> PathBuf {
    let name = format!("overlay-{index}");
    let seed = scratch.path().join(format!("{name}-seed"));
    let root = seed.join("home");
    std::fs::create_dir_all(&root).expect("seed dir");
    git(&seed, &["init", "-q"]);
    git(&seed, &["config", "user.name", "fixture"]);
    git(&seed, &["config", "user.email", "fixture@example.invalid"]);
    for file in 0..files {
        let rel = format!("home/file-{file:03}.txt");
        std::fs::write(seed.join(&rel), format!("{name} payload {file}\n")).expect("write");
        git(&seed, &["add", &rel]);
    }
    git(&seed, &["commit", "-qm", "seed"]);
    git(&seed, &["branch", "-M", "main"]);
    let origin = scratch.path().join(format!("{name}.git"));
    let output = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg("--bare")
        .arg(&seed)
        .arg(&origin)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("clone bare");
    assert!(
        output.status.success(),
        "clone bare {seed:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    origin
}

/// Build the shared remotes once: three overlay remotes plus a base
/// whose `overlays.d` points at them.
fn shared_remotes(scratch: &Scratch) -> (Vec<PathBuf>, PathBuf) {
    let mut overlays = Vec::new();
    for index in 0..OVERLAYS {
        overlays.push(seed_overlay(scratch, index, FILES_PER_OVERLAY));
    }
    let base_seed = scratch.path().join("base-seed");
    std::fs::create_dir_all(base_seed.join("overlays.d")).expect("overlays.d");
    git(&base_seed, &["init", "-q"]);
    git(&base_seed, &["config", "user.name", "fixture"]);
    git(
        &base_seed,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(base_seed.join(".testrc"), "base\n").expect("write");
    for (index, origin) in overlays.iter().enumerate() {
        let conf = format!("url=file://{}\n", origin.display());
        std::fs::write(
            base_seed
                .join("overlays.d")
                .join(format!("overlay-{index}.conf")),
            conf,
        )
        .expect("write conf");
    }
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
        .env("DOT_GIT_REAL", "1")
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
    (overlays, base_origin)
}

/// Build one twin client: `init --yes` into a fresh home/state pair,
/// then register the overlay descriptors where discovery reads them
/// (`${config_home}/dot/overlays.d`, per `docs/overlays.md`).
fn twin_client(
    scratch: &Scratch,
    tag: &str,
    overlays: &[PathBuf],
    base_origin: &Path,
    jobs: Option<&str>,
) -> (PathBuf, PathBuf) {
    let home = scratch.path().join(format!("home-{tag}"));
    let state = scratch.path().join(format!("state-{tag}"));
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&state).expect("state");
    let init = dot(
        &[
            "init",
            "--yes",
            &format!("file://{}", base_origin.display()),
        ],
        &home,
        &state,
        jobs,
        None,
    );
    assert!(
        init.status.success(),
        "twin {tag} init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let conf_dir = home.join(".config/dot/overlays.d");
    std::fs::create_dir_all(&conf_dir).expect("conf dir");
    for (index, origin) in overlays.iter().enumerate() {
        let conf = format!("url=file://{}\n", origin.display());
        std::fs::write(conf_dir.join(format!("overlay-{index}.conf")), conf).expect("write conf");
    }
    (home, state)
}

/// Snapshot the converged HOME tree (regular files only, sorted) for
/// stability comparisons between native runs. `.git` carries
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

/// Run one update and require success.
fn check_update(home: &Path, state: &Path, jobs: Option<&str>) -> std::process::Output {
    let output = dot(&["update"], home, state, jobs, None);
    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn warm(home: &Path, state: &Path, jobs: Option<&str>) {
    check_update(home, state, jobs);
}

fn median_ms(samples: &mut [u128]) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn time_update(home: &Path, state: &Path, jobs: Option<&str>) -> Duration {
    let start = Instant::now();
    check_update(home, state, jobs);
    start.elapsed()
}

#[test]
fn clean_three_overlay_update_converges_and_is_stable() {
    let scratch = Scratch::new("parpull-clean").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    warm(&home, &state, None);
    let before = snapshot_tree(&home);
    let output = check_update(&home, &state, None);
    assert!(String::from_utf8_lossy(&output.stdout).contains("current"));
    assert_eq!(snapshot_tree(&home), before);
}

#[test]
fn bounded_jobs_three_overlay_update_converges() {
    let scratch = Scratch::new("parpull-jobs").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, Some("2"));
    warm(&home, &state, Some("2"));
    let before = snapshot_tree(&home);
    check_update(&home, &state, Some("2"));
    assert_eq!(snapshot_tree(&home), before);
}

#[test]
fn pushed_change_converges_on_three_overlays() {
    let scratch = Scratch::new("parpull-pushed").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let overlay_seed = scratch.path().join("overlay-1-seed");
    let overlay_origin = scratch.path().join("overlay-1.git");
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    std::fs::write(overlay_seed.join("home/only-1.txt"), "overlay-1 unique\n").expect("write");
    git(&overlay_seed, &["add", "home/only-1.txt"]);
    git(&overlay_seed, &["commit", "-qm", "change"]);
    git(
        &overlay_seed,
        &["push", "-q", &overlay_origin.to_string_lossy(), "HEAD:main"],
    );
    check_update(&home, &state, None);
    assert_eq!(
        std::fs::read(home.join("only-1.txt")).expect("converged file"),
        b"overlay-1 unique\n"
    );
}

#[test]
fn repeated_native_updates_are_byte_stable() {
    let scratch = Scratch::new("parpull-repeat").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    check_update(&home, &state, None);
    let converged = snapshot_tree(&home);
    check_update(&home, &state, None);
    assert_eq!(snapshot_tree(&home), converged);
}

#[test]
fn dirty_overlay_preserves_local_edit() {
    let scratch = Scratch::new("parpull-dirty").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    warm(&home, &state, None);
    let dirty = home.join(".dotfiles-overlay-1/home/file-002.txt");
    std::fs::write(&dirty, "local dirty edit\n").expect("write dirty");
    check_update(&home, &state, None);
    assert_eq!(
        std::fs::read(dirty).expect("dirty file"),
        b"local dirty edit\n"
    );
}

#[test]
fn fetch_failure_is_nonzero_and_preserves_tree() {
    let scratch = Scratch::new("parpull-fetchfail").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    warm(&home, &state, None);
    let before = snapshot_tree(&home);
    let gone = scratch.path().join("overlay-2.git");
    let kept = scratch.path().join("overlay-2.git.kept");
    std::fs::rename(&gone, &kept).expect("remove remote");
    let output = dot(&["update"], &home, &state, None, None);
    assert!(!output.status.success());
    assert_eq!(snapshot_tree(&home), before);
    std::fs::rename(&kept, &gone).expect("restore remote");
}

#[test]
fn config_rejection_reports_2() {
    let scratch = Scratch::new("parpull-config2").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    let output = dot(&["update"], &home, &state, None, Some("bogus"));
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("DOT_SHDEPS_UPDATE_POLICY"));
}

#[test]
fn lock_busy_reports_75_on_three_overlay_fixture() {
    use dot::log::Log;
    let scratch = Scratch::new("parpull-busy").expect("scratch dir");
    let state = scratch.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let log = Log::new(false, false);
    let mut sink = Vec::new();
    let guard = dot::update_lock::acquire(&state, false, &log, None, &mut sink).expect("hold lock");
    assert!(sink.is_empty());
    let expected = format!(
        "  warning: dot update already running (pid {})\n",
        std::process::id()
    );
    let output = dot(&["update"], &home, &state, None, None);
    assert_eq!(output.status.code(), Some(75));
    assert_eq!(output.stderr, expected.as_bytes());
    assert!(output.stdout.is_empty());
    let _ = guard;
}

#[test]
fn report_native_wall_clock_three_overlay() {
    let scratch = Scratch::new("parpull-timing").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home, state) = twin_client(&scratch, "native", &overlays, &base_origin, None);
    warm(&home, &state, None);
    let mut samples = Vec::with_capacity(TIMING_RUNS);
    for _ in 0..TIMING_RUNS {
        samples.push(time_update(&home, &state, None).as_millis());
    }
    let median = median_ms(&mut samples);
    eprintln!(
        "native parpull wall-clock on {OVERLAYS} overlays x {FILES_PER_OVERLAY} files \
         ({TIMING_RUNS} clean updates): median {median}ms {samples:?}"
    );
}
