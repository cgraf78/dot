//! Parallel overlay pull parity for `dot update` (engine-parallel-pull lane).
//!
//! The native parallel fan-out itself lives in [`dot::repos_pull_fleet`]
//! (scoped threads bounded by `DOT_UPDATE_JOBS`, falling back to the
//! serial path when scratch allocation fails); the `update`/`pull`
//! dispatcher arm ([`dot::cli::run`] via `update_run`) still drives the
//! shell's `_dot_update`, which already fans overlay pulls out within
//! the same `_dot_update_jobs` bound. This suite pins the differential
//! contract the final native wiring must preserve, on a three-overlay
//! `file://` fixture (the speedup fixture):
//!
//! - clean and pushed-change updates agree with the shell (`bin/dot`)
//!   on exit codes, streams, and converged HOME trees, both with the
//!   default job bound and with `DOT_UPDATE_JOBS=2`;
//! - the same fixture converges byte-identically when the shell and
//!   the Rust binary update it in turn (shell-then-Rust, then shell
//!   again for stability), not just on twin fixtures;
//! - dirty overlays (an uncommitted local edit), fetch failures (a
//!   missing overlay remote), config rejection (exit `2`), and a held
//!   update lock (exit `75`) agree on codes and trees;
//! - wall-clock medians for shell vs Rust updates on the fixture are
//!   measured and reported (see `report_wall_clock`), so the lane's
//!   speedup claim — or its absence — carries numbers.
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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Overlays on the speedup fixture (matches `tests/perf_update.rs`).
const OVERLAYS: usize = 3;
/// Payload files per overlay: enough fetch/pull substance to time,
/// small enough to stay fast under CI.
const FILES_PER_OVERLAY: usize = 12;
/// Timed update iterations per engine in
/// `report_wall_clock_three_overlay_shell_vs_rust`.
const TIMING_RUNS: usize = 3;

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
    cmd.env("XDG_STATE_HOME", state);
    cmd.env("XDG_CONFIG_HOME", "");
    cmd.env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"));
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

/// The production shell oracle (`bin/dot`) with the same controlled
/// client.
fn shell_dot(
    argv: &[&str],
    home: &Path,
    state: &Path,
    jobs: Option<&str>,
    policy: Option<&str>,
) -> std::process::Output {
    let mut cmd = Command::new("bash");
    cmd.arg("bin/dot");
    for arg in argv {
        cmd.arg(arg);
    }
    client_env(&mut cmd, home, state, jobs, policy);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"));
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run bin/dot")
}

/// The Rust binary with the same controlled client.
fn rust_dot(
    argv: &[&str],
    home: &Path,
    state: &Path,
    jobs: Option<&str>,
    policy: Option<&str>,
) -> std::process::Output {
    let mut cmd = bin();
    client_env(&mut cmd, home, state, jobs, policy);
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
    let init = shell_dot(
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

/// Blank the wall-clock stamps the UI rows carry (`0s`, `Done in
/// 12s`): the only bytes allowed to differ between two identical
/// updates. Counts (`1 repo current`, `[1/5]`, `1/1`) never match
/// `<digits>s` at a word boundary the way stamps do, except the
/// `s`-suffixed plural `repos` — which has no leading digit run of
/// its own (`1 repos` keeps its digit: only the stamp form
/// `<digits>s` with no intervening space is replaced).
fn blank_timing(bytes: &[u8]) -> Vec<u8> {
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

/// Blank every occurrence of `needle` (a twin HOME path) so failure
/// diagnostics that quote checkout paths compare across twins.
fn blank_home(bytes: &[u8], needle: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(found) = rest.windows(needle.len()).position(|w| w == needle) {
        out.extend_from_slice(&rest[..found]);
        out.extend_from_slice(b"HOME");
        rest = &rest[found + needle.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Normalize a captured stream for cross-twin comparison: timing
/// stamps first, then both twin HOME paths.
fn normalize(bytes: &[u8], home_a: &Path, home_b: &Path) -> Vec<u8> {
    let timed = blank_timing(bytes);
    let no_a = blank_home(&timed, home_a.to_string_lossy().as_bytes());
    blank_home(&no_a, home_b.to_string_lossy().as_bytes())
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

/// One parallel-pull row on twin clients: exit codes match, streams
/// match after normalization, and the converged trees match byte for
/// byte.
fn check_update(
    argv: &[&str],
    home_shell: &Path,
    state_shell: &Path,
    home_rust: &Path,
    state_rust: &Path,
    jobs: Option<&str>,
    policy: Option<&str>,
) {
    let shell = shell_dot(argv, home_shell, state_shell, jobs, policy);
    let rust = rust_dot(argv, home_rust, state_rust, jobs, policy);
    assert_eq!(rust.status.code(), shell.status.code(), "argv: {argv:?}");
    assert_eq!(
        normalize(&rust.stdout, home_shell, home_rust),
        normalize(&shell.stdout, home_shell, home_rust),
        "argv: {argv:?} stdout:\nrust:\n{}\nshell:\n{}",
        String::from_utf8_lossy(&rust.stdout),
        String::from_utf8_lossy(&shell.stdout),
    );
    assert_eq!(
        normalize(&rust.stderr, home_shell, home_rust),
        normalize(&shell.stderr, home_shell, home_rust),
        "argv: {argv:?} stderr:\nrust:\n{}\nshell:\n{}",
        String::from_utf8_lossy(&rust.stderr),
        String::from_utf8_lossy(&shell.stderr),
    );
    assert_eq!(
        snapshot_tree(home_rust),
        snapshot_tree(home_shell),
        "argv: {argv:?} converged trees differ"
    );
}

/// Warm both twins into the clean steady state, like cron does, so
/// the measured run converges nothing.
fn warm_twins(
    home_shell: &Path,
    state_shell: &Path,
    home_rust: &Path,
    state_rust: &Path,
    jobs: Option<&str>,
) {
    for (home, state) in [(home_shell, state_shell), (home_rust, state_rust)] {
        let warm = shell_dot(&["update"], home, state, jobs, None);
        assert!(
            warm.status.success(),
            "warm-up failed: {}",
            String::from_utf8_lossy(&warm.stderr)
        );
    }
}

fn median_ms(samples: &mut [u128]) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Time one `update` run; the caller asserts success.
fn time_update(home: &Path, state: &Path, shell: bool, jobs: Option<&str>) -> Duration {
    let start = Instant::now();
    let output = if shell {
        shell_dot(&["update"], home, state, jobs, None)
    } else {
        rust_dot(&["update"], home, state, jobs, None)
    };
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "timed update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    elapsed
}

#[test]
fn clean_three_overlay_update_matches_shell_and_converges() {
    let scratch = Scratch::new("parpull-clean").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlays, &base_origin, None);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, None);
    warm_twins(&home_shell, &state_shell, &home_rust, &state_rust, None);
    check_update(
        &["update"],
        &home_shell,
        &state_shell,
        &home_rust,
        &state_rust,
        None,
        None,
    );
    let rust = rust_dot(&["update"], &home_rust, &state_rust, None, None);
    assert_eq!(rust.status.code(), Some(0));
}

#[test]
fn bounded_jobs_three_overlay_update_matches_shell() {
    // The bounded fan-out (`DOT_UPDATE_JOBS=2`, like
    // `_dot_update_jobs`) runs end to end on both engines: same
    // bound, same codes, same converged trees.
    let scratch = Scratch::new("parpull-jobs").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) =
        twin_client(&scratch, "shell", &overlays, &base_origin, Some("2"));
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, Some("2"));
    warm_twins(
        &home_shell,
        &state_shell,
        &home_rust,
        &state_rust,
        Some("2"),
    );
    check_update(
        &["update"],
        &home_shell,
        &state_shell,
        &home_rust,
        &state_rust,
        Some("2"),
        None,
    );
}

#[test]
fn pushed_change_converges_on_three_overlays() {
    let scratch = Scratch::new("parpull-pushed").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let overlay_seed = scratch.path().join("overlay-1-seed");
    let overlay_origin = scratch.path().join("overlay-1.git");
    assert!(
        overlays.iter().any(|origin| origin == &overlay_origin),
        "fixture keeps overlay sharing its seed checkout"
    );
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlays, &base_origin, None);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, None);
    // Push one new file to the shared overlay-1 remote: both twins
    // converge the same change through the parallel fan-out. A new
    // `only-1` file (not an edit of a shared `file-NNN` name) keeps
    // later overlays from winning the same-path collision over it.
    std::fs::write(overlay_seed.join("home/only-1.txt"), "overlay-1 unique\n").expect("write");
    git(&overlay_seed, &["add", "home/only-1.txt"]);
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
        None,
        None,
    );
    for home in [&home_shell, &home_rust] {
        let bytes = std::fs::read(home.join("only-1.txt")).expect("converged file");
        assert_eq!(bytes, b"overlay-1 unique\n", "home: {home:?}");
    }
}

#[test]
fn same_fixture_shell_then_rust_converges_identically() {
    // Same-fixture sequencing (not twins): the shell converges the
    // fixture, a pushed change lands, the Rust binary converges the
    // same fixture, and a final shell run proves the tree is stable
    // across engines — byte-identical, not just equally successful.
    let scratch = Scratch::new("parpull-same").expect("scratch dir");
    let (_overlays, base_origin) = shared_remotes(&scratch);
    let overlay_seed = scratch.path().join("overlay-2-seed");
    let overlay_origin = scratch.path().join("overlay-2.git");
    let home = scratch.path().join("home-same");
    let state = scratch.path().join("state-same");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&state).expect("state");
    let init = shell_dot(
        &[
            "init",
            "--yes",
            &format!("file://{}", base_origin.display()),
        ],
        &home,
        &state,
        None,
        None,
    );
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let conf_dir = home.join(".config/dot/overlays.d");
    std::fs::create_dir_all(&conf_dir).expect("conf dir");
    for index in 0..OVERLAYS {
        let origin = scratch.path().join(format!("overlay-{index}.git"));
        let conf = format!("url=file://{}\n", origin.display());
        std::fs::write(conf_dir.join(format!("overlay-{index}.conf")), conf).expect("write conf");
    }
    let shell = shell_dot(&["update"], &home, &state, None, None);
    assert!(
        shell.status.success(),
        "shell update failed: {}",
        String::from_utf8_lossy(&shell.stderr)
    );
    let before = snapshot_tree(&home);
    // Overlay-2 is the last overlay in descriptor order, so it wins
    // the same-path collision on `file-001.txt` at `$HOME`.
    std::fs::write(
        overlay_seed.join("home/file-001.txt"),
        "overlay-2 payload CHANGED\n",
    )
    .expect("write");
    git(&overlay_seed, &["add", "home/file-001.txt"]);
    git(&overlay_seed, &["commit", "-qm", "change"]);
    git(
        &overlay_seed,
        &["push", "-q", &overlay_origin.to_string_lossy(), "HEAD:main"],
    );
    let rust = rust_dot(&["update"], &home, &state, None, None);
    assert_eq!(rust.status.code(), shell.status.code());
    assert!(
        rust.status.success(),
        "rust update failed: {}",
        String::from_utf8_lossy(&rust.stderr)
    );
    let converged = snapshot_tree(&home);
    assert_ne!(converged, before, "the pushed change must converge");
    let bytes = std::fs::read(home.join("file-001.txt")).expect("converged file");
    assert_eq!(bytes, b"overlay-2 payload CHANGED\n");
    let again = shell_dot(&["update"], &home, &state, None, None);
    assert!(again.status.success());
    assert_eq!(
        snapshot_tree(&home),
        converged,
        "shell re-update after the Rust update must be a byte-identical no-op"
    );
}

#[test]
fn dirty_overlay_matches_shell() {
    // An uncommitted local edit inside one overlay checkout: both
    // engines see the same dirty worktree and must agree on the
    // outcome (whatever the rebase/autostash path decides).
    let scratch = Scratch::new("parpull-dirty").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlays, &base_origin, None);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, None);
    warm_twins(&home_shell, &state_shell, &home_rust, &state_rust, None);
    for home in [&home_shell, &home_rust] {
        let dirty = home.join(".dotfiles-overlay-1/home/file-002.txt");
        std::fs::write(&dirty, "local dirty edit\n").expect("write dirty");
    }
    check_update(
        &["update"],
        &home_shell,
        &state_shell,
        &home_rust,
        &state_rust,
        None,
        None,
    );
}

#[test]
fn fetch_failure_matches_shell() {
    // A missing overlay remote fails the fetch on both engines: same
    // exit code (nonzero), same converged trees.
    let scratch = Scratch::new("parpull-fetchfail").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlays, &base_origin, None);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, None);
    warm_twins(&home_shell, &state_shell, &home_rust, &state_rust, None);
    let gone = scratch.path().join("overlay-2.git");
    let kept = scratch.path().join("overlay-2.git.kept");
    assert!(
        overlays.iter().any(|origin| origin == &gone),
        "the removed remote backs overlay-2"
    );
    std::fs::rename(&gone, &kept).expect("remove remote");
    let shell = shell_dot(&["update"], &home_shell, &state_shell, None, None);
    let rust = rust_dot(&["update"], &home_rust, &state_rust, None, None);
    assert_eq!(rust.status.code(), shell.status.code());
    assert_ne!(rust.status.code(), Some(0), "a missing remote must fail");
    assert_eq!(
        snapshot_tree(&home_rust),
        snapshot_tree(&home_shell),
        "failed-fetch trees differ"
    );
    std::fs::rename(&kept, &gone).expect("restore remote");
}

#[test]
fn config_rejection_reports_2_like_shell() {
    // `dot_config_load || exit 2`: a bogus shdeps policy rejects the
    // run before any repository moves on both engines.
    let scratch = Scratch::new("parpull-config2").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlays, &base_origin, None);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, None);
    let shell = shell_dot(&["update"], &home_shell, &state_shell, None, Some("bogus"));
    let rust = rust_dot(&["update"], &home_rust, &state_rust, None, Some("bogus"));
    assert_eq!(shell.status.code(), Some(2));
    assert_eq!(rust.status.code(), Some(2));
    assert_eq!(
        normalize(&rust.stderr, &home_shell, &home_rust),
        normalize(&shell.stderr, &home_shell, &home_rust),
    );
}

#[test]
fn lock_busy_reports_75_on_three_overlay_fixture() {
    // Hold the update lock from this process (a live owner), then run
    // both binaries against the same state: each must refuse with
    // exit 75 and the shell's exact busy diagnostic, naming our pid.
    use dot::log::Log;
    let scratch = Scratch::new("parpull-busy").expect("scratch dir");
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
    let shell = shell_dot(&["update"], &home, &state, None, None);
    assert_eq!(shell.status.code(), Some(75));
    assert_eq!(shell.stderr, expected.as_bytes());
    let rust = rust_dot(&["update"], &home, &state, None, None);
    assert_eq!(rust.status.code(), Some(75));
    assert_eq!(rust.stderr, expected.as_bytes());
    assert!(rust.stdout.is_empty());
    assert!(shell.stdout.is_empty());
    let _ = guard;
}

#[test]
fn report_wall_clock_three_overlay_shell_vs_rust() {
    // Measured before/after medians on the 3-overlay fixture. The
    // Rust arm still drives the shell's `_dot_update` (with its own
    // `_dot_update_jobs` fan-out) through the `update_run` adapter,
    // so parity — not a speedup — is the expected reading here; the
    // numbers below are the baseline the native wiring must beat.
    // They print on `--nocapture` and land in the lane PR body.
    let scratch = Scratch::new("parpull-timing").expect("scratch dir");
    let (overlays, base_origin) = shared_remotes(&scratch);
    let (home_shell, state_shell) = twin_client(&scratch, "shell", &overlays, &base_origin, None);
    let (home_rust, state_rust) = twin_client(&scratch, "rust", &overlays, &base_origin, None);
    warm_twins(&home_shell, &state_shell, &home_rust, &state_rust, None);
    let mut shell_ms = Vec::new();
    let mut rust_ms = Vec::new();
    for _ in 0..TIMING_RUNS {
        shell_ms.push(time_update(&home_shell, &state_shell, true, None).as_millis());
        rust_ms.push(time_update(&home_rust, &state_rust, false, None).as_millis());
    }
    let shell_median = median_ms(&mut shell_ms);
    let rust_median = median_ms(&mut rust_ms);
    eprintln!(
        "parpull wall-clock on {OVERLAYS} overlays x {FILES_PER_OVERLAY} files \
         ({TIMING_RUNS} clean updates each): shell median {shell_median}ms {shell_ms:?}, \
         rust median {rust_median}ms {rust_ms:?}"
    );
    assert_eq!(
        snapshot_tree(&home_rust),
        snapshot_tree(&home_shell),
        "timed runs must converge identically"
    );
}
