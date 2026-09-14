//! End-to-end update latency harness (slice 2 foundations).
//!
//! Measures wall-clock `dot update` on a synthetic client (base + three
//! overlays, local `file://` remotes) in two modes: clean (nothing
//! changed — the cron steady state) and dirty (a distinct pushed change to
//! converge for every sample). This is the priority benchmark from the port plan:
//! startup budgets in `tests/perf_budget.rs` cannot catch an update
//! regression, only this harness can.
//!
//! Each mode runs on an isolated native client. Historical Bash comparison is
//! performed separately against the pre-cutover revision, so this regression
//! gate cannot accidentally compare the Rust binary with itself. The overlays
//! are REAL: descriptors live in a scratch `XDG_CONFIG_HOME/dot/overlays.d`
//! (kept outside
//! `$HOME` so status stays clean, per the `tests/cli.rs` repos-client
//! convention), and the harness asserts the overlay payloads actually
//! landed in the converged tree. An earlier revision set
//! `XDG_CONFIG_HOME=""` with descriptors only in the base seed — which
//! discovery never reads — so every run reported `0 overlays current`
//! and the budgets gated a hollow base-only path.
//!
//! `#[ignore]`-gated like the hive-memory heavy suites: gate CI runs it
//! explicitly via the shared `test-command` override at multiplier 1.
//! Budgets below are ceilings calibrated on the reference host (nas,
//! 2026-09-05); the harness compares the converged trees byte for byte
//! so a "fast" run that converges differently still fails.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Fixture shape: big enough to exercise per-file/per-repo costs, small
/// enough to run in CI without dominating the suite.
const OVERLAYS: usize = 3;
const FILES_PER_OVERLAY: usize = 20;
const RUNS: usize = 20;
/// Payload files are namespaced per overlay (`{name}-file-NNN.txt`):
/// identical names across overlays would collide, and colliding stacks
/// never settle — every update re-steals each link in descriptor order
/// (each loser replaces the live link, the winner takes it back), so
/// the harness would time perpetual `changed` churn instead of the cron
/// steady state it exists to gate. Namespaced files keep the same
/// per-file/per-repo costs while letting links settle to `current`.
/// Ceilings calibrated on the reference host (nas, 2026-09-05) over
/// historical runs of the same fixture shape. Real overlay convergence costs ~6x the
/// old hollow base-only numbers (clean p95 328ms with zero overlays
/// discovered) — four `file://` fetches plus 60 link checks per update
/// — so the ceilings move with the fixture, not with any implementation
/// change. Budgets sit ~3-4x above measured (clean 6000ms = 3.2x,
/// dirty 10000ms) to absorb CI variance. Separate
/// pre-cutover measurements prove the speedup; these ceilings prevent it from
/// regressing afterward.
const CLEAN_UPDATE_BUDGET_MS: u128 = 6_000;
const DIRTY_UPDATE_BUDGET_MS: u128 = 10_000;
const PERF_BUDGET_MULTIPLIER_ENV: &str = "DOT_PERF_BUDGET_MULTIPLIER";

fn budget_ms(base: u128) -> u128 {
    let multiplier: f64 = std::env::var(PERF_BUDGET_MULTIPLIER_ENV)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .filter(|parsed: &f64| parsed.is_finite() && *parsed > 0.0)
        .unwrap_or(1.0);
    ((base as f64) * multiplier) as u128
}

/// Shared counter-based scratch (see `dot_test_support`): pid plus a
/// monotonic counter, no wall-clock reads.
type Scratch = dot_test_support::TempDir;

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

/// Seed a repo with `files` payload files under `prefix` (overlays
/// publish their `home/` tree; the base publishes its root), return its
/// bare remote path.
fn seed_remote(scratch: &Scratch, name: &str, branch: &str, prefix: &str, files: usize) -> PathBuf {
    let seed = scratch.path().join(format!("{name}-seed"));
    let root = seed.join(prefix);
    std::fs::create_dir_all(&root).expect("seed dir");
    git(&seed, &["init", "-q"]);
    git(&seed, &["config", "user.name", "fixture"]);
    git(&seed, &["config", "user.email", "fixture@example.invalid"]);
    for index in 0..files {
        let rel = format!("{prefix}{name}-file-{index:03}.txt");
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

/// The engine under test. CI uses the just-built native binary; maintainers
/// can supply an explicit historical executable to reproduce comparison data.
fn dot_cmd() -> Command {
    Command::new(
        std::env::var_os("DOT_PERF_EXECUTABLE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dot").into()),
    )
}

/// Controlled client environment, mirroring `client_env` in
/// `tests/update_run.rs`: a cleared environment plus a home/state/XDG
/// triple, so rows never touch the developer's own checkout. One `.env`
/// per variable (never `.envs`): MSRV-clean and matches the oracle
/// convention in `tests/cli.rs`.
fn client_env(cmd: &mut Command, home: &Path, state: &Path, xdg: &Path) {
    let repo = std::env::var_os("DOT_PERF_SOURCE_ROOT")
        .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into());
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
    cmd.env("XDG_CONFIG_HOME", xdg);
    cmd.env("DOT_SOURCE_ROOT", repo);
    // Client-side identity for rebase/autostash paths: scoped env,
    // never the operator's global gitconfig (which CI isolates).
    cmd.env("GIT_AUTHOR_NAME", "fixture");
    cmd.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
    cmd.env("GIT_COMMITTER_NAME", "fixture");
    cmd.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    cmd.env_remove("DOT_TEST_RESULT_FILE");
    cmd.env_remove("DOT_TEST_REPORTER");
    cmd.current_dir(home);
}

/// Run one engine's `dot` with an isolated HOME/state/XDG triple;
/// returns wall time plus the captured output. Streams are captured
/// (not inherited) so progress spam neither floods the log nor perturbs
/// timing with terminal writes.
fn run_dot(
    home: &Path,
    state: &Path,
    xdg: &Path,
    args: &[&str],
) -> (Duration, std::process::Output) {
    let mut cmd = dot_cmd();
    client_env(&mut cmd, home, state, xdg);
    for arg in args {
        cmd.arg(arg);
    }
    let start = Instant::now();
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn dot");
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "native dot {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (elapsed, output)
}

fn p95_ms(samples: &mut [u128]) -> u128 {
    samples.sort_unstable();
    let index = ((samples.len() as f64) * 0.95).ceil() as usize;
    samples[index.saturating_sub(1).min(samples.len() - 1)]
}

/// One client: `init --yes` into a fresh home/state/XDG triple,
/// then register the overlay descriptors where discovery reads them
/// (`${XDG_CONFIG_HOME}/dot/overlays.d`, per `docs/overlays.md`).
fn twin_client(
    scratch: &Scratch,
    tag: &str,
    overlay_origins: &[PathBuf],
    base_origin: &Path,
) -> (PathBuf, PathBuf, PathBuf) {
    let home = scratch.path().join(format!("home-{tag}"));
    let state = scratch.path().join(format!("state-{tag}"));
    let xdg = scratch.path().join(format!("xdg-{tag}"));
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&state).expect("state");
    let conf_dir = xdg.join("dot/overlays.d");
    std::fs::create_dir_all(&conf_dir).expect("conf dir");
    for (index, origin) in overlay_origins.iter().enumerate() {
        // Descriptor grammar is `key=value`, no spaces (docs/overlays.md).
        let conf = format!("url=file://{}\n", origin.display());
        std::fs::write(conf_dir.join(format!("overlay-{index}.conf")), conf).expect("write conf");
    }
    run_dot(
        &home,
        &state,
        &xdg,
        &[
            "init",
            "--yes",
            &format!("file://{}", base_origin.display()),
        ],
    );
    (home, state, xdg)
}

/// Build the fixture remotes once: a base publishing its root plus one
/// remote per overlay publishing a `home/` tree. Descriptors are NOT
/// committed into the base seed — discovery reads them from
/// `XDG_CONFIG_HOME/dot/overlays.d` (see [`twin_client`]), and a copy
/// in the base would leak into the converged tree as plain content.
fn shared_remotes(scratch: &Scratch) -> (Vec<PathBuf>, PathBuf) {
    let base_seed = scratch.path().join("base-seed");
    std::fs::create_dir_all(&base_seed).expect("base seed");
    git(&base_seed, &["init", "-q"]);
    git(&base_seed, &["config", "user.name", "fixture"]);
    git(
        &base_seed,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(base_seed.join(".testrc"), "base\n").expect("write");
    let mut overlay_origins = Vec::with_capacity(OVERLAYS);
    for index in 0..OVERLAYS {
        let name = format!("overlay-{index}");
        overlay_origins.push(seed_remote(
            scratch,
            &name,
            "main",
            "home/",
            FILES_PER_OVERLAY,
        ));
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
    (overlay_origins, base_origin)
}

/// Snapshot the converged HOME tree (sorted rel-path/bytes pairs) for
/// byte comparison between shell and Rust runs. `.git` carries checkout
/// identity, `.dotfiles` carries the base checkout, and `.dot-backup`
/// carries timestamped init-time safekeeping: none of them is converged
/// content, so all three stay out of the comparison whatever filesystem
/// kind they take (a worktree `.git` may be a file, not a directory) —
/// the same exclusions as `tests/update_run.rs`.
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
            // converged content, whatever filesystem kind they take.
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

/// Assert the overlays really converged into `home`: every overlay's
/// payload files are present with their seeded bytes. A hollow
/// base-only run — zero discovered overlays — fails here, not just on
/// the timing ceiling.
fn assert_overlays_converged(home: &Path) {
    for overlay in 0..OVERLAYS {
        for index in 0..FILES_PER_OVERLAY {
            let rel = format!("overlay-{overlay}-file-{index:03}.txt");
            let expected = format!("overlay-{overlay} payload {index}\n").into_bytes();
            let actual = std::fs::read(home.join(&rel)).unwrap_or_else(|_| {
                panic!(
                    "{}: {rel} missing (overlays did not converge)",
                    home.display()
                )
            });
            assert_eq!(actual, expected, "{rel} carries the wrong overlay bytes");
        }
    }
}

/// Assert one engine's warm-up update actually discovered the fixture
/// overlays. The stdout count row is stamp-free, so it pins discovery
/// without normalizing timing output.
fn assert_overlays_discovered(stdout: &[u8]) {
    let text = String::from_utf8_lossy(stdout);
    assert!(
        text.contains(&format!("{OVERLAYS} overlays current")),
        "native update discovered no overlays (hollow base-only path): {text}"
    );
}

/// Time `RUNS` updates of one engine on its twin; returns the p95.
fn timed_block(home: &Path, state: &Path, xdg: &Path) -> u128 {
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        samples.push(run_dot(home, state, xdg, &["update"]).0.as_millis());
    }
    p95_ms(&mut samples)
}

/// Time updates that each have one distinct upstream commit to converge.
/// Preparing and pushing the commit is deliberately outside the timed region;
/// the measured work starts when `dot update` observes the changed remote.
fn timed_dirty_block(
    home: &Path,
    state: &Path,
    xdg: &Path,
    seed: &Path,
    origin: &Path,
) -> (u128, Vec<u8>) {
    let payload = seed.join("home/overlay-0-file-000.txt");
    let mut samples = Vec::with_capacity(RUNS);
    let mut expected = Vec::new();
    for index in 0..RUNS {
        expected = format!("overlay-0 payload change-{index:02}\n").into_bytes();
        std::fs::write(&payload, &expected).expect("write dirty payload");
        git(seed, &["add", "home/overlay-0-file-000.txt"]);
        git(seed, &["commit", "-qm", &format!("change-{index:02}")]);
        git(
            seed,
            &["push", "-q", &origin.to_string_lossy(), "HEAD:main"],
        );
        samples.push(run_dot(home, state, xdg, &["update"]).0.as_millis());
    }
    (p95_ms(&mut samples), expected)
}

#[test]
#[ignore = "CI runs this explicitly: it builds a fixture client and times native updates"]
fn clean_and_dirty_update_within_budget() {
    let scratch = Scratch::new("perf-update").expect("scratch dir");
    let (overlay_origins, base_origin) = shared_remotes(&scratch);
    let (home, state, xdg) = twin_client(&scratch, "native", &overlay_origins, &base_origin);

    // Warm-up: the first update converges the fresh clones (init only
    // fetches) and populates caches exactly like cron does; the second
    // reaches the clean steady state the timed block measures. Only the
    // steady-state wording is pinned — the first run reports `changed`.
    run_dot(&home, &state, &xdg, &["update"]);
    let (_, steady) = run_dot(&home, &state, &xdg, &["update"]);
    assert_overlays_discovered(&steady.stdout);
    assert_overlays_converged(&home);
    let before = snapshot_tree(&home);

    let clean_p95 = timed_block(&home, &state, &xdg);
    eprintln!("engine clean update p95: {clean_p95}ms over {RUNS} runs");

    // Dirty: every timed sample receives a fresh change to overlay-0's remote.
    // Namespaced payloads mean no overlay shadows another, so each change is
    // observable in the converged tree.
    let overlay_seed = scratch.path().join("overlay-0-seed");
    let overlay_origin = overlay_origins[0].clone();
    let (dirty_p95, changed) =
        timed_dirty_block(&home, &state, &xdg, &overlay_seed, &overlay_origin);
    eprintln!("engine dirty update p95: {dirty_p95}ms over {RUNS} runs");

    let actual = std::fs::read(home.join("overlay-0-file-000.txt")).expect("changed file");
    assert_eq!(actual, changed, "native update did not converge the change");

    let after = snapshot_tree(&home);
    assert_eq!(
        after.iter().map(|(p, _)| p).collect::<Vec<_>>(),
        before.iter().map(|(p, _)| p).collect::<Vec<_>>(),
        "update changed the converged file set"
    );

    assert!(
        clean_p95 <= budget_ms(CLEAN_UPDATE_BUDGET_MS),
        "native clean p95 {clean_p95}ms exceeds budget"
    );
    assert!(
        dirty_p95 <= budget_ms(DIRTY_UPDATE_BUDGET_MS),
        "native dirty p95 {dirty_p95}ms exceeds budget"
    );
}
