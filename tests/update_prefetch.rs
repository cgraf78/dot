//! Overlay probe coverage for `dot update` ([`dot::repos_prefetch`]).
//!
//! Every update here runs through a logging `git` shim placed first on
//! `PATH`, so tests observe which overlay fetches ran instead of inferring it
//! from timing. Control files beside the log make the shim's `ls-remote`
//! fail, print to stderr, or hang, which drives the probe's fallback and
//! lifecycle paths deterministically. No assertion depends on wall-clock
//! duration: lifecycle tests poll observable process state with bounded
//! deadlines and check a marker the shim writes only if it was never stopped.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

type Scratch = dot_test_support::TempDir;

/// Bounded wait for lifecycle observations (probe started, process gone).
const OBSERVE_DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

/// One synthetic client: overlay remotes and seeds, base remote, client
/// HOME/state, and the shim control directory.
struct Fixture {
    scratch: Scratch,
    overlays: usize,
    home: PathBuf,
    state: PathBuf,
    shim_dir: PathBuf,
    ctl: PathBuf,
}

/// Fixture-side `git`, isolated from the developer's system and global
/// configuration (hooks, signing, URL rewriting) so fixtures build the same
/// everywhere. Dot's own runs use the fixture HOME instead.
fn fixture_git() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("DOT_GIT_REAL", "1")
        .stdin(Stdio::null());
    command
}

fn plain_git(dir: &Path, args: &[&str]) {
    let output = fixture_git()
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("DOT_GIT_REAL", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {dir:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// First `git` on the test process PATH outside the caller's HOME; the shim
/// execs it by absolute path. Dot's own host-Git selection skips HOME entries
/// (user launchers live there), and the shim mirrors that rule.
fn real_git() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .filter(|dir| home.as_ref().is_none_or(|home| !dir.starts_with(home)))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// Publish `seed` as a bare remote with `main` as HEAD (pack transport, like
/// `tests/update_parpull.rs`, so a concurrently written seed cannot tear the
/// copy).
fn clone_bare(seed: &Path, origin: &Path) {
    let output = fixture_git()
        .args(["clone", "-q", "--bare", "--no-local"])
        .arg(seed)
        .arg(origin)
        .env("DOT_GIT_REAL", "1")
        .stdin(Stdio::null())
        .output()
        .expect("clone bare");
    assert!(output.status.success(), "clone bare {seed:?}");
    plain_git(origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

fn seed_repo(dir: &Path, files: &[(&str, String)]) {
    std::fs::create_dir_all(dir).expect("seed dir");
    plain_git(dir, &["init", "-q"]);
    for (rel, body) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
        std::fs::write(&path, body).expect("write seed file");
        plain_git(dir, &["add", rel]);
    }
    plain_git(dir, &["commit", "-qm", "seed"]);
    plain_git(dir, &["branch", "-M", "main"]);
}

impl Fixture {
    /// Build `overlays` overlay remotes, a base remote, and an initialized
    /// client with every overlay registered, then converge once so the
    /// overlays are cloned before any test observes a probe.
    fn new(label: &str, overlays: usize) -> Self {
        let scratch = Scratch::new(label).expect("scratch");
        for index in 0..overlays {
            let seed = scratch.path().join(format!("overlay-{index}-seed"));
            // Distinct payload names per overlay keep the link pass
            // collision-free.
            let rel = format!("home/only-{index}.txt");
            seed_repo(&seed, &[(rel.as_str(), format!("overlay-{index}\n"))]);
            clone_bare(&seed, &scratch.path().join(format!("overlay-{index}.git")));
        }
        let base_seed = scratch.path().join("base-seed");
        seed_repo(&base_seed, &[(".testrc", "base\n".to_string())]);
        let base_origin = scratch.path().join("base.git");
        clone_bare(&base_seed, &base_origin);

        // Dot selects host Git only outside both the client HOME and the Dot
        // checkout, so the shim lives beside the fixture HOME in scratch
        // space (as `tests/cli.rs` does), never under the Cargo target dir.
        let shim_dir = scratch.path().join("bin");
        let ctl = scratch.path().join("shim-ctl");
        std::fs::create_dir_all(&shim_dir).expect("shim dir");
        std::fs::create_dir_all(&ctl).expect("ctl dir");
        let shim = shim_dir.join("git");
        let script = format!(
            r#"#!{bash}
ctl=${{PREFETCH_SHIM_CTL:?}}
printf '%s\n' "$*" >>"$ctl/log"
if [[ -e $ctl/config-stderr && $* == *--show-scope* ]]; then
  printf 'warning: configuration notice\n' >&2
fi
op=
for arg; do
  case $arg in
    ls-remote | fetch)
      op=$arg
      break
      ;;
  esac
done
if [[ $op == ls-remote ]]; then
  [[ -e $ctl/ls-remote-fail ]] && exit 2
  [[ -e $ctl/ls-remote-stderr ]] && printf 'notice from transport\n' >&2
  if [[ -e $ctl/ls-remote-hang ]]; then
    : >"$ctl/hang.$$"
    # End after the round's consume grace (10s) but before the probe
    # deadline (60s): the marker shows whether Dot waited past its grace.
    for ((i = 0; i < 450; i++)); do sleep 0.1; done
    : >"$ctl/finished.$$"
    exit 2
  fi
fi
# Hold the base fetch until the expected number of probes is in flight, so
# lifecycle tests stop probes that provably started.
if [[ $op == fetch && $* == *--git-dir=* && -s $ctl/base-gate ]]; then
  want=$(<"$ctl/base-gate")
  # Wall-clock bound: slow runners stretch an iteration count arbitrarily,
  # and supervised probe setup is much slower on some platforms.
  gate_end=$((SECONDS + 120))
  while ((SECONDS < gate_end)); do
    have=("$ctl"/hang.*)
    [[ -e ${{have[0]}} && ${{#have[@]}} -ge $want ]] && break
    sleep 0.05
  done
  # Fail the base fetch itself; hiding the remote instead trips client
  # identity checks on some platforms before synchronization starts.
  [[ -e $ctl/base-fetch-fail ]] && exit 128
fi
if [[ -z $op ]]; then
  exec {git} "$@"
fi
# Bracket network operations so tests can check their ordering.
printf '@begin %s\n' "$*" >>"$ctl/events"
{git} "$@"
rc=$?
printf '@end %s\n' "$*" >>"$ctl/events"
exit "$rc"
"#,
            bash = dot_test_support::bash().display(),
            git = real_git().display(),
        );
        std::fs::write(&shim, script).expect("write shim");
        std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("shim mode");
        dot_test_support::wait_until_executable(&shim, &["--version"]).expect("shim ready");

        let home = scratch.path().join("home");
        let state = scratch.path().join("state");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&state).expect("state");
        let fixture = Self {
            scratch,
            overlays,
            home,
            state,
            shim_dir,
            ctl,
        };
        let init = fixture.run_plain(&[
            "init",
            "--yes",
            &format!("file://{}", base_origin.display()),
        ]);
        assert!(
            init.status.success(),
            "init: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        let conf_dir = fixture.home.join(".config/dot/overlays.d");
        std::fs::create_dir_all(&conf_dir).expect("conf dir");
        for index in 0..overlays {
            std::fs::write(
                conf_dir.join(format!("overlay-{index}.conf")),
                format!("url=file://{}\n", fixture.remote(index).display()),
            )
            .expect("write conf");
        }
        let warm = fixture.update(None);
        assert!(
            warm.status.success(),
            "warm update: {}",
            String::from_utf8_lossy(&warm.stderr)
        );
        fixture
    }

    fn remote(&self, index: usize) -> PathBuf {
        self.scratch.path().join(format!("overlay-{index}.git"))
    }

    fn checkout(&self, index: usize) -> PathBuf {
        self.home.join(format!(".dotfiles-overlay-{index}"))
    }

    fn command(&self, with_shim: bool, jobs: Option<&str>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dot"));
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = if with_shim {
            let mut dirs = vec![self.shim_dir.clone()];
            dirs.extend(std::env::split_paths(&inherited));
            std::env::join_paths(dirs).expect("join PATH")
        } else {
            inherited
        };
        let tmpdir = std::env::var_os("TMPDIR")
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
        cmd.env_clear();
        cmd.env("LC_ALL", "C");
        cmd.env("PATH", path);
        cmd.env("TMPDIR", tmpdir);
        cmd.env("HOME", &self.home);
        cmd.env("SHELL", "/bin/bash");
        cmd.env("XDG_STATE_HOME", &self.state);
        cmd.env("XDG_CONFIG_HOME", "");
        cmd.env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"));
        cmd.env("DOT_BASH", dot_test_support::bash());
        cmd.env("DOT_GIT_REAL", "1");
        // Host system configuration (URL rewriting, fetch policy) must not
        // decide whether proofs hold; the fixture HOME owns global config.
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("PREFETCH_SHIM_CTL", &self.ctl);
        cmd.env("GIT_AUTHOR_NAME", "fixture");
        cmd.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
        cmd.env("GIT_COMMITTER_NAME", "fixture");
        cmd.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
        // Pin the bound: the default follows the host CPU count, which
        // would change how many probes start on small CI runners.
        cmd.env("DOT_UPDATE_JOBS", jobs.unwrap_or("8"));
        cmd.current_dir(&self.home);
        cmd.stdin(Stdio::null());
        cmd
    }

    /// Run without the shim (init never needs observation, and init's
    /// strict host-Git selection is not under test here).
    fn run_plain(&self, argv: &[&str]) -> Output {
        self.command(false, None)
            .args(argv)
            .output()
            .expect("run dot")
    }

    /// Run `dot update` through the logging shim with a fresh log.
    fn update(&self, jobs: Option<&str>) -> Output {
        let _ = std::fs::remove_file(self.ctl.join("log"));
        let _ = std::fs::remove_file(self.ctl.join("events"));
        self.command(true, jobs)
            .arg("update")
            .output()
            .expect("run dot update")
    }

    fn set(&self, control: &str, on: bool) {
        let path = self.ctl.join(control);
        if on {
            std::fs::write(path, b"").expect("set control");
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.ctl.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Overlay indexes whose checkout ran `git fetch` in the last update,
    /// once per invocation (so a duplicate fetch shows up twice).
    fn fetched(&self) -> Vec<usize> {
        self.invocations("fetch")
    }

    /// Overlay indexes probed in the last update, once per invocation.
    fn probed(&self) -> Vec<usize> {
        self.invocations("ls-remote")
    }

    fn invocations(&self, subcommand: &str) -> Vec<usize> {
        let log = self.log();
        let mut found = Vec::new();
        for index in 0..self.overlays {
            let needle = format!("-C {} {subcommand} ", self.checkout(index).display());
            let count = log.iter().filter(|line| line.contains(&needle)).count();
            found.extend(std::iter::repeat_n(index, count));
        }
        found
    }

    /// Commit `rel` with `body` to overlay `index`'s remote through its seed.
    fn push(&self, index: usize, rel: &str, body: &str) {
        let seed = self.scratch.path().join(format!("overlay-{index}-seed"));
        std::fs::write(seed.join(rel), body).expect("write change");
        plain_git(&seed, &["add", rel]);
        plain_git(&seed, &["commit", "-qm", "change"]);
        plain_git(
            &seed,
            &[
                "push",
                "-q",
                &self.remote(index).to_string_lossy(),
                "HEAD:main",
            ],
        );
    }

    fn head(&self, repo: &Path, rev: &str) -> String {
        let output = fixture_git()
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", rev])
            .env("DOT_GIT_REAL", "1")
            .output()
            .expect("rev-parse");
        assert!(output.status.success(), "rev-parse {rev} in {repo:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Install one trusted pre-sync hook body (see `docs/extensions.md`).
    fn with_pre_sync(&self, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let extensions = self.home.join("extensions");
        let hooks = extensions.join("pre-sync.d");
        std::fs::create_dir_all(&hooks).expect("pre-sync dir");
        let hook = hooks.join("10-transport.sh");
        std::fs::write(&hook, body).expect("pre-sync hook");
        std::fs::write(
            self.home.join(".config/dot/config"),
            b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\n",
        )
        .expect("extension config");
        for dir in [&extensions, &hooks] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .expect("private extension dir");
        }
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))
            .expect("private hook");
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "update failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Blank elapsed-time fields (`0s`, `Done in 1s`), which differ run to run.
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

/// PIDs of every probe the shim held, and whether any ran to completion.
fn held_probes(ctl: &Path) -> (Vec<String>, bool) {
    let mut pids = Vec::new();
    let mut finished = false;
    for entry in std::fs::read_dir(ctl).expect("read ctl") {
        let name = entry.expect("ctl entry").file_name();
        let name = name.to_string_lossy();
        if let Some(pid) = name.strip_prefix("hang.") {
            pids.push(pid.to_string());
        } else if name.starts_with("finished.") {
            finished = true;
        }
    }
    pids.sort();
    (pids, finished)
}

/// Whether any process remains in the process group led by `pid`. Dot starts
/// every Git command in its own session, so the shim leads a group that also
/// holds its `sleep` and `git` children; the whole group must go.
fn alive(pid: &str) -> bool {
    let output = Command::new("ps")
        .args(["-A", "-o", "pgid="])
        .stderr(Stdio::null())
        .output()
        .expect("run ps");
    assert!(output.status.success(), "ps failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == pid)
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + OBSERVE_DEADLINE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(POLL);
    }
}

#[test]
fn clean_update_skips_fetches_the_probes_prove_current() {
    let fixture = Fixture::new("prefetch-clean", 3);
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.probed(), vec![0, 1, 2]);
    assert_eq!(fixture.fetched(), Vec::<usize>::new());
    // The base fetch itself is never replaced.
    assert!(
        fixture
            .log()
            .iter()
            .any(|line| line.contains("--git-dir=") && line.contains(" fetch ")),
        "base fetch missing: {:?}",
        fixture.log()
    );
}

#[test]
fn pushed_overlay_is_fetched_and_the_others_are_skipped() {
    let fixture = Fixture::new("prefetch-pushed", 3);
    fixture.push(1, "home/new-1.txt", "fresh\n");
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.fetched(), vec![1]);
    assert_eq!(
        std::fs::read(fixture.home.join("new-1.txt")).expect("converged"),
        b"fresh\n"
    );
}

#[test]
fn proof_and_fallback_paths_print_identical_output() {
    let fixture = Fixture::new("prefetch-parity", 3);
    let proved = fixture.update(None);
    assert_ok(&proved);
    assert_eq!(fixture.fetched(), Vec::<usize>::new());
    fixture.set("ls-remote-fail", true);
    let fallback = fixture.update(None);
    assert_ok(&fallback);
    assert_eq!(fixture.fetched(), vec![0, 1, 2]);
    assert_eq!(
        String::from_utf8_lossy(&normalize_timing(&proved.stdout)),
        String::from_utf8_lossy(&normalize_timing(&fallback.stdout))
    );
    assert_eq!(
        String::from_utf8_lossy(&normalize_timing(&proved.stderr)),
        String::from_utf8_lossy(&normalize_timing(&fallback.stderr))
    );
}

#[test]
fn failed_probe_falls_back_and_still_converges() {
    let fixture = Fixture::new("prefetch-probe-fail", 3);
    fixture.push(2, "home/new-2.txt", "late\n");
    fixture.set("ls-remote-fail", true);
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.fetched(), vec![0, 1, 2]);
    assert_eq!(
        std::fs::read(fixture.home.join("new-2.txt")).expect("converged"),
        b"late\n"
    );
}

#[test]
fn probe_stderr_keeps_the_foreground_fetch() {
    let fixture = Fixture::new("prefetch-probe-stderr", 2);
    fixture.set("ls-remote-stderr", true);
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.probed(), vec![0, 1]);
    assert_eq!(fixture.fetched(), vec![0, 1]);
}

#[test]
fn unreachable_remote_fails_exactly_like_a_fetch_failure() {
    let fixture = Fixture::new("prefetch-unreachable", 3);
    let gone = fixture.remote(2);
    let kept = fixture.scratch.path().join("overlay-2.git.kept");
    std::fs::rename(&gone, &kept).expect("hide remote");
    let probed = fixture.update(None);
    // The probe failed, so the round ran its own fetch and reported it.
    assert_eq!(fixture.fetched(), vec![2]);
    // Refusing every probe reproduces the unprefetched engine; the failure
    // must look identical.
    fixture.set("ls-remote-fail", true);
    let unprefetched = fixture.update(None);
    assert_eq!(fixture.fetched(), vec![0, 1, 2]);
    assert!(!probed.status.success());
    assert_eq!(probed.status.code(), unprefetched.status.code());
    assert_eq!(
        String::from_utf8_lossy(&normalize_timing(&probed.stdout)),
        String::from_utf8_lossy(&normalize_timing(&unprefetched.stdout))
    );
    assert_eq!(
        String::from_utf8_lossy(&normalize_timing(&probed.stderr)),
        String::from_utf8_lossy(&normalize_timing(&unprefetched.stderr))
    );
    std::fs::rename(&kept, &gone).expect("restore remote");
}

#[test]
fn transport_change_by_pre_sync_forces_a_fresh_fetch() {
    let fixture = Fixture::new("prefetch-presync-url", 2);
    // An alternate remote that is one commit ahead. Pre-sync redirects the
    // unchanged origin URL to it after the probe observed the original, so
    // only a fetch issued after pre-sync can see the new commit.
    let alternate = fixture.scratch.path().join("overlay-0-alt.git");
    clone_bare(&fixture.remote(0), &alternate);
    let alt_seed = fixture.scratch.path().join("overlay-0-alt-seed");
    let output = fixture_git()
        .arg("clone")
        .arg("-q")
        .arg(&alternate)
        .arg(&alt_seed)
        .env("DOT_GIT_REAL", "1")
        .output()
        .expect("clone alternate");
    assert!(output.status.success());
    std::fs::write(alt_seed.join("home/redirected.txt"), "alternate\n").expect("write");
    plain_git(&alt_seed, &["add", "home/redirected.txt"]);
    plain_git(&alt_seed, &["commit", "-qm", "alternate only"]);
    plain_git(&alt_seed, &["push", "-q", "origin", "HEAD:main"]);
    fixture.with_pre_sync(&format!(
        "prepare() {{\n  printf '[url \"file://{alt}\"]\\n\\tinsteadOf = file://{orig}\\n' >\"$HOME/.gitconfig\"\n}}\n",
        alt = alternate.display(),
        orig = fixture.remote(0).display(),
    ));
    let output = fixture.update(None);
    assert_ok(&output);
    assert!(fixture.fetched().contains(&0), "log: {:?}", fixture.log());
    assert_eq!(
        std::fs::read(fixture.home.join("redirected.txt")).expect("converged from alternate"),
        b"alternate\n"
    );
}

#[test]
fn ssh_config_change_by_pre_sync_voids_the_probe() {
    let fixture = Fixture::new("prefetch-presync-ssh", 2);
    fixture.with_pre_sync(
        "prepare() {\n  mkdir -p \"$HOME/.ssh\"\n  printf 'Host changed-%s\\n' \"$RANDOM$RANDOM\" >\"$HOME/.ssh/config\"\n}\n",
    );
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.probed(), vec![0, 1]);
    assert_eq!(fixture.fetched(), vec![0, 1]);
}

#[test]
fn job_bound_caps_the_number_of_probes() {
    // The base fetch holds one slot of the bound while the probes run.
    let fixture = Fixture::new("prefetch-cap", 3);
    let output = fixture.update(Some("2"));
    assert_ok(&output);
    assert_eq!(fixture.probed(), vec![0]);
    assert_eq!(fixture.fetched(), vec![1, 2]);
}

#[test]
fn job_bound_of_one_keeps_remote_work_serial() {
    let fixture = Fixture::new("prefetch-serial", 2);
    let output = fixture.update(Some("1"));
    assert_ok(&output);
    assert_eq!(fixture.probed(), Vec::<usize>::new());
    assert_eq!(fixture.fetched(), vec![0, 1]);
}

#[test]
fn failed_base_pull_abandons_in_flight_probes() {
    let fixture = Fixture::new("prefetch-abandon", 2);
    fixture.set("base-fetch-fail", true);
    fixture.set("ls-remote-hang", true);
    std::fs::write(fixture.ctl.join("base-gate"), b"2").expect("gate base fetch");
    let output = fixture.update(None);
    assert!(!output.status.success());
    // Both probes were in flight when the base pull failed, and the guard
    // stopped them instead of waiting for either to finish.
    let (pids, finished) = held_probes(&fixture.ctl);
    assert_eq!(
        pids.len(),
        2,
        "probes held: {pids:?}\nlog: {:?}\nstderr: {}",
        fixture.log(),
        String::from_utf8_lossy(&output.stderr)
    );
    for pid in &pids {
        wait_until("abandoned probe exit", || !alive(pid));
    }
    assert!(!finished, "a probe ran to completion");
    assert!(
        !held_probes(&fixture.ctl).1,
        "a probe finished after abandonment"
    );
}

#[test]
fn termination_stops_every_in_flight_probe() {
    let fixture = Fixture::new("prefetch-term", 2);
    fixture.set("ls-remote-hang", true);
    let _ = std::fs::remove_file(fixture.ctl.join("log"));
    let mut child = fixture
        .command(true, None)
        .arg("update")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn update");
    wait_until("both probes in flight", || {
        held_probes(&fixture.ctl).0.len() == 2
    });
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send TERM");
    assert!(status.success());
    let mut exit = None;
    wait_until("update exit", || {
        exit = child.try_wait().expect("try_wait");
        exit.is_some()
    });
    assert_eq!(exit.expect("exit status").code(), Some(143));
    let (pids, _) = held_probes(&fixture.ctl);
    for pid in &pids {
        wait_until("probe exit", || !alive(pid));
    }
    assert!(!held_probes(&fixture.ctl).1, "a probe ran to completion");
}

#[test]
fn stderr_from_any_probe_step_keeps_the_foreground_fetch() {
    // A warning from reading configuration is text a real fetch would print,
    // so neither the probe nor the round may swallow it.
    let fixture = Fixture::new("prefetch-config-stderr", 2);
    fixture.set("config-stderr", true);
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.fetched(), vec![0, 1]);
}

#[test]
fn stalled_probes_are_abandoned_and_the_rounds_fetch() {
    // The base update succeeds while every probe hangs: the round waits out
    // the bounded grace, stops the probes, and fetches as before.
    let fixture = Fixture::new("prefetch-stalled", 2);
    fixture.set("ls-remote-hang", true);
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.fetched(), vec![0, 1]);
    let (pids, _) = held_probes(&fixture.ctl);
    assert_eq!(pids.len(), 2, "probes held: {pids:?}");
    for pid in &pids {
        wait_until("stalled probe exit", || !alive(pid));
    }
    assert!(!held_probes(&fixture.ctl).1, "a probe ran to completion");
}

#[test]
fn gitmodules_keeps_the_foreground_fetch() {
    let fixture = Fixture::new("prefetch-gitmodules", 2);
    // Per-submodule recursion lives in `.gitmodules`, outside the modeled
    // configuration, so its presence alone must keep the real fetch.
    fixture.push(0, ".gitmodules", "");
    assert_ok(&fixture.update(None));
    assert!(fixture.checkout(0).join(".gitmodules").exists());
    let output = fixture.update(None);
    assert_ok(&output);
    assert_eq!(fixture.probed(), vec![1]);
    assert_eq!(fixture.fetched(), vec![0]);
}

#[test]
fn probes_never_overlap_the_overlay_fetch_lanes() {
    // Three overlays with a bound of two: one probe runs beside the base
    // fetch. The pushed overlay needs a real fetch, which must not start
    // until every probe has ended.
    let fixture = Fixture::new("prefetch-overlap", 3);
    fixture.push(0, "home/overlap.txt", "x\n");
    let output = fixture.update(Some("2"));
    assert_ok(&output);
    assert_eq!(fixture.probed(), vec![0]);
    assert_eq!(fixture.fetched(), vec![0, 1, 2]);
    let events = std::fs::read_to_string(fixture.ctl.join("events")).expect("events");
    let overlay_fetch = |line: &str| line.starts_with("@begin -C ") && line.contains(" fetch ");
    let lines: Vec<&str> = events.lines().collect();
    let first_fetch = lines
        .iter()
        .position(|line| overlay_fetch(line))
        .expect("overlay fetch event");
    let last_probe_end = lines
        .iter()
        .rposition(|line| line.starts_with("@end ") && line.contains(" ls-remote "))
        .expect("probe end event");
    assert!(
        last_probe_end < first_fetch,
        "a probe overlapped an overlay fetch:\n{events}"
    );
}

/// Parallel probes under a small job bound across repeated updates with a
/// deterministic mix of pushed and untouched overlays. Every run must
/// converge each checkout to its remote, fetch exactly the probed overlays
/// that changed, and fetch every overlay the bound left unprobed.
#[test]
fn stress_repeated_updates_with_mixed_changes_under_a_job_bound() {
    const OVERLAYS: usize = 8;
    const BOUND: usize = 4;
    // One slot of the bound belongs to the base fetch.
    const PROBES: usize = BOUND - 1;
    const ROUNDS: usize = 8;
    let fixture = Fixture::new("prefetch-stress", OVERLAYS);
    let bound = BOUND.to_string();
    for round in 0..ROUNDS {
        // A fixed pseudo-random subset per round (never all, never fixed).
        let changed: Vec<usize> = (0..OVERLAYS)
            .filter(|index| (index * 7 + round * 3) % 5 < 2)
            .collect();
        for index in &changed {
            fixture.push(
                *index,
                &format!("home/round-{index}.txt"),
                &format!("{round}\n"),
            );
        }
        let output = fixture.update(Some(&bound));
        assert_ok(&output);
        let probed: Vec<usize> = (0..PROBES).collect();
        assert_eq!(fixture.probed(), probed, "round {round}");
        let expected: Vec<usize> = (0..OVERLAYS)
            .filter(|index| *index >= PROBES || changed.contains(index))
            .collect();
        assert_eq!(fixture.fetched(), expected, "round {round}");
        for index in 0..OVERLAYS {
            assert_eq!(
                fixture.head(&fixture.checkout(index), "HEAD"),
                fixture.head(&fixture.remote(index), "main"),
                "round {round} overlay {index}"
            );
        }
        for index in &changed {
            assert_eq!(
                std::fs::read_to_string(fixture.home.join(format!("round-{index}.txt")))
                    .expect("converged payload"),
                format!("{round}\n")
            );
        }
    }
}
