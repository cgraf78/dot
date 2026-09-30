//! Native contracts for base-repository pull orchestration.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull::{PullBaseInputs, PullBaseOutcome, PullStatus, pull_base};
use dot::repos_pull_queries::CandidateEnv;
use dot_test_support::TempDir;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stage(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ],
    );
}

struct Side {
    _scope: TempDir,
    home: PathBuf,
    origin: PathBuf,
    manifest: String,
    legacy: String,
}

impl Side {
    fn new(case: &str) -> Self {
        let scope = TempDir::new("pull-base").unwrap();
        let origin = scope.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        stage(&origin, "base.txt", b"v1\n");
        commit(&origin, "seed");
        let home = scope.path().join("home");
        let output = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "clone", "-q"])
            .arg(&origin)
            .arg(&home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // `pull_base` runs git with the host's config; a repo-local
        // identity keeps its autostash and rebase commits working on
        // hosts (CI) with none configured.
        git(&home, &["config", "user.name", "t"]);
        git(&home, &["config", "user.email", "t@t"]);
        match case {
            "skipped" => git(&home, &["branch", "--unset-upstream"]),
            "changed" => {
                stage(&origin, "newfile.txt", b"from origin\n");
                commit(&origin, "add newfile");
            }
            "conflict-backup" => {
                stage(&origin, "clash.txt", b"origin clash\n");
                commit(&origin, "add clash");
                stage(&home, "clash.txt", b"user clash\n");
            }
            "diverged" => {
                stage(&home, "base.txt", b"home change\n");
                commit(&home, "home change");
                stage(&origin, "base.txt", b"origin change\n");
                commit(&origin, "origin change");
            }
            "invalid-candidate" => {
                stage(&origin, ".dotfiles/evil", b"x\n");
                commit(&origin, "add evil");
            }
            "current" | "current-quiet" => {}
            _ => unreachable!(),
        }
        let home_text = home.to_string_lossy();
        let manifest = format!("{home_text}/manifest.tsv");
        let legacy = format!("{home_text}/legacy.tsv");
        Self {
            _scope: scope,
            home,
            origin,
            manifest,
            legacy,
        }
    }
}

/// Run [`pull_base`] against `side` with a hermetic ordinary
/// topology, returning the outcome plus both captured streams.
fn run(side: &Side, quiet: bool, verbose: bool) -> (PullBaseOutcome, Vec<u8>, Vec<u8>) {
    run_in(side, Topology::Ordinary, quiet, verbose)
}

/// [`run`] with an explicit topology; `Separate` points the client
/// git dir at the clone's `.git`, like the production `~/.dotfiles`.
fn run_in(
    side: &Side,
    topology: Topology,
    quiet: bool,
    verbose: bool,
) -> (PullBaseOutcome, Vec<u8>, Vec<u8>) {
    run_with(side, topology, quiet, verbose, &[])
}

/// [`run_in`] with extra rebase arguments (for example `--exec` to
/// stop the rebase without a conflict).
fn run_with(
    side: &Side,
    topology: Topology,
    quiet: bool,
    verbose: bool,
    extra: &[&str],
) -> (PullBaseOutcome, Vec<u8>, Vec<u8>) {
    let extra: Vec<OsString> = extra.iter().map(OsString::from).collect();
    let home = side.home.to_string_lossy().into_owned();
    let client_git_dir = match topology {
        Topology::Separate => format!("{home}/.git"),
        _ => String::new(),
    };
    let base = Base {
        topology,
        client_git_dir,
        home: home.clone(),
    };
    let candidate = CandidateEnv {
        home: home.clone(),
        checkout: format!("{home}/.local/share/cgraf78/dot"),
        pwd: home.clone(),
        source_root: env!("CARGO_MANIFEST_DIR").into(),
        state_home: format!("{home}/.local/state"),
        install_root: format!("{home}/.local/share"),
        provider_state: format!("{home}/.local/state/shdeps"),
        overlay_paths: Vec::new(),
        init_backup: None,
    };
    let dest = DestinationInputs {
        pwd: home.clone(),
        home: home.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: Vec::new(),
        init_backup: None,
    };
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    let log = Log::new(false, false);
    let inputs = PullBaseInputs {
        base: &base,
        candidate: &candidate,
        quarantine: None,
        overlays: &[],
        dest: &dest,
        manifest: &side.manifest,
        legacy_manifest: &side.legacy,
        euid: dot::temp::current_uid().unwrap(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &side.home,
        tool: &tool,
        extra_args: &extra,
        quiet,
        verbose,
        log: &log,
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let outcome = pull_base(&inputs, &mut moves, &mut stdout, &mut stderr);
    (outcome, stdout, stderr)
}

/// Exit status of a raw git query in `root` (hermetic config).
fn git_status(root: &Path, args: &[&str]) -> Option<i32> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .code()
}

/// Stdout of a raw git query in `root` (hermetic config).
fn git_stdout(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Proof that a rebase really started and was aborted, with the
/// autostash consumed: without it, a rebase that never started
/// (for example `git stash create` failing for want of an
/// identity) would leave the same bytes behind.
fn assert_rebase_aborted(root: &Path) {
    let reflog = git_stdout(root, &["reflog", "-n", "5", "--format=%gs"]);
    assert!(reflog.contains("rebase (abort)"), "{reflog}");
    assert_eq!(git_stdout(root, &["stash", "list"]), "");
}

/// Whether `root` has a rebase in progress (either backend).
fn rebase_in_progress(root: &Path) -> bool {
    root.join(".git/rebase-merge").exists() || root.join(".git/rebase-apply").exists()
}

#[test]
fn pull_base_preserves_status_failure_backup_and_candidate_safety_rows() {
    for (case, quiet, verbose, expected_status, expected_rc) in [
        ("skipped", false, false, PullStatus::Skipped, 0),
        ("current", false, true, PullStatus::Current, 0),
        ("current-quiet", true, false, PullStatus::Current, 0),
        ("changed", false, false, PullStatus::Changed, 0),
        ("conflict-backup", false, false, PullStatus::Changed, 0),
        ("diverged", false, false, PullStatus::Failed, 1),
        ("invalid-candidate", false, false, PullStatus::Failed, 1),
    ] {
        let side = Side::new(case);
        let (outcome, _stdout, _stderr) = run(&side, quiet, verbose);
        assert_eq!(outcome.status, expected_status, "{case}");
        assert_eq!(outcome.rc, expected_rc, "{case}");
        match case {
            "changed" => assert_eq!(
                std::fs::read(side.home.join("newfile.txt")).unwrap(),
                b"from origin\n"
            ),
            "conflict-backup" => {
                assert_eq!(
                    std::fs::read(side.home.join("clash.txt")).unwrap(),
                    b"origin clash\n"
                );
                let backups = std::fs::read_dir(side.home.join(".dot-backup/pull"))
                    .unwrap()
                    .flatten()
                    .collect::<Vec<_>>();
                assert_eq!(backups.len(), 1);
            }
            "invalid-candidate" => assert!(!side.home.join(".dotfiles/evil").exists()),
            "diverged" => {
                // The failed rebase is aborted: the live file keeps the
                // local commit's bytes instead of conflict markers, and
                // HEAD is back on its branch.
                assert_eq!(
                    std::fs::read(side.home.join("base.txt")).unwrap(),
                    b"home change\n"
                );
                assert!(!rebase_in_progress(&side.home));
                assert_eq!(
                    git_status(&side.home, &["symbolic-ref", "-q", "HEAD"]),
                    Some(0)
                );
                assert_rebase_aborted(&side.home);
            }
            _ => {}
        }
        assert!(!side.manifest.ends_with(".pending"));
        assert!(side.origin.is_dir());
    }
}

#[test]
fn pull_base_changed_with_many_sibling_updates_succeeds() {
    // The parent snapshot repeats one `identity\t.config` record per
    // updated file, so 40 siblings make its text far longer than
    // NAME_MAX. The snapshot is in-memory text, never a path: a
    // successful rebase must still report Changed with rc 0.
    let side = Side::new("current");
    stage(&side.origin, ".config/seed", b"seed\n");
    commit(&side.origin, "seed config");
    assert_eq!(run(&side, false, false).0.status, PullStatus::Changed);
    for index in 0..40 {
        stage(
            &side.origin,
            &format!(".config/sibling-{index:02}"),
            b"sibling\n",
        );
    }
    commit(&side.origin, "many siblings");
    let (outcome, _stdout, stderr) = run(&side, false, false);
    assert_eq!(
        outcome.status,
        PullStatus::Changed,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(outcome.rc, 0);
    assert_eq!(
        std::fs::read(side.home.join(".config/sibling-39")).unwrap(),
        b"sibling\n"
    );
}

#[test]
fn pull_base_failed_rebase_restores_autostashed_changes() {
    // A conflicting rebase must not strand HEAD mid-rebase, and the
    // user's uncommitted edit to another tracked file (autostashed
    // before the rebase started) must come back on abort.
    let side = Side::new("diverged");
    stage(&side.home, "other.txt", b"seed\n");
    commit(&side.home, "home other");
    stage(&side.home, "other.txt", b"user edit\n");
    let (outcome, _stdout, _stderr) = run(&side, false, false);
    assert_eq!(outcome.status, PullStatus::Failed);
    assert_eq!(outcome.rc, 1);
    assert!(!rebase_in_progress(&side.home));
    assert_rebase_aborted(&side.home);
    assert_eq!(
        std::fs::read(side.home.join("other.txt")).unwrap(),
        b"user edit\n"
    );
    assert_eq!(
        std::fs::read(side.home.join("base.txt")).unwrap(),
        b"home change\n"
    );
    // The next run reports the same conflict as failed (never a
    // skip) without replaying it over the live files again.
    let (again, _stdout, stderr) = run(&side, false, false);
    assert_eq!(again.status, PullStatus::Failed);
    assert_eq!(again.rc, 1);
    assert!(
        String::from_utf8_lossy(&stderr).contains("failed to rebase onto its upstream"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(abort_count(&side.home), 1);
    assert_eq!(
        std::fs::read(side.home.join("other.txt")).unwrap(),
        b"user edit\n"
    );
}

/// Number of `rebase (abort)` entries in the HEAD reflog.
fn abort_count(root: &Path) -> usize {
    git_stdout(root, &["reflog", "--format=%gs"])
        .lines()
        .filter(|line| line.starts_with("rebase (abort)"))
        .count()
}

#[test]
fn pull_base_retries_a_conflict_once_either_side_moves() {
    let side = Side::new("diverged");
    assert_eq!(run(&side, false, false).0.status, PullStatus::Failed);
    assert_eq!(abort_count(&side.home), 1);
    // A new upstream commit is a new pair: the rebase runs again.
    stage(&side.origin, "later.txt", b"later\n");
    commit(&side.origin, "later");
    assert_eq!(run(&side, false, false).0.status, PullStatus::Failed);
    assert_eq!(abort_count(&side.home), 2);
    // Dropping the conflicting local commit lets the pull succeed,
    // which also clears the recorded conflict.
    git(&side.home, &["reset", "-q", "--hard", "HEAD~1"]);
    assert_eq!(run(&side, false, false).0.status, PullStatus::Changed);
    assert!(!side.home.join(".git/dot-rebase-failed").exists());
    assert_eq!(
        std::fs::read(side.home.join("later.txt")).unwrap(),
        b"later\n"
    );
}

#[test]
fn pull_base_failed_rebase_is_aborted_in_separate_topology() {
    // Production bases use `--git-dir=~/.dotfiles --work-tree=$HOME`;
    // the abort must resolve rebase state through that prefix too.
    let side = Side::new("diverged");
    let (outcome, _stdout, _stderr) = run_in(&side, Topology::Separate, false, false);
    assert_eq!(outcome.status, PullStatus::Failed);
    assert!(!rebase_in_progress(&side.home));
    assert_rebase_aborted(&side.home);
    assert_eq!(
        std::fs::read(side.home.join("base.txt")).unwrap(),
        b"home change\n"
    );
    assert_eq!(
        git_status(&side.home, &["symbolic-ref", "-q", "HEAD"]),
        Some(0)
    );
}

#[test]
fn pull_base_skips_a_rebase_the_user_started() {
    // A rebase dot did not start (the user's own, paused at a conflict,
    // `edit`, or `break`) has a detached HEAD and no `@{u}`. It must
    // warn and skip with rc 0: failing would skip every later stage
    // each cycle, and dot must never tell the user to abort it.
    let side = Side::new("diverged");
    git(&side.home, &["fetch", "-q", "origin"]);
    assert_eq!(
        git_status(
            &side.home,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "rebase",
                "origin/HEAD"
            ]
        ),
        Some(1)
    );
    assert!(rebase_in_progress(&side.home));
    let (outcome, _stdout, stderr) = run(&side, false, false);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(outcome.status, PullStatus::Skipped, "{stderr}");
    assert_eq!(outcome.rc, 0);
    assert!(stderr.contains("dot did not start"), "{stderr}");
    assert!(!stderr.contains("--abort"), "{stderr}");
    assert!(rebase_in_progress(&side.home));
    assert_eq!(abort_count(&side.home), 0);
}

#[test]
fn pull_base_warns_and_skips_a_detached_head() {
    // A bare detached HEAD is a user action (bisect, pinned commit):
    // it must be visible, but not fail and freeze every cycle.
    let side = Side::new("changed");
    git(&side.home, &["checkout", "-q", "--detach"]);
    let (outcome, _stdout, stderr) = run(&side, false, false);
    assert_eq!(outcome.status, PullStatus::Skipped);
    assert_eq!(outcome.rc, 0);
    assert!(
        String::from_utf8_lossy(&stderr).contains("detached"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(!side.home.join("newfile.txt").exists());
}

/// Seed `side` with a local commit and an upstream commit that touch
/// different files, so a rebase replays cleanly unless something
/// other than a conflict stops it.
fn diverge_cleanly(side: &Side) {
    stage(&side.home, "local.txt", b"local\n");
    commit(&side.home, "local");
    stage(&side.origin, "newfile.txt", b"from origin\n");
    commit(&side.origin, "add newfile");
}

#[test]
fn pull_base_does_not_run_commit_hooks_while_replaying_local_commits() {
    // A hooks directory whose `prepare-commit-msg` fails (like a
    // commit gate configured through a global `core.hooksPath`) must
    // not stop dot's replay of already-committed local commits.
    let side = Side::new("current");
    diverge_cleanly(&side);
    let hooks = side.home.join(".hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("prepare-commit-msg");
    std::fs::write(&hook, "#!/bin/sh\necho gate failed >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &side.home,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
    let (outcome, _stdout, stderr) = run(&side, false, false);
    assert_eq!(
        outcome.status,
        PullStatus::Changed,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(
        std::fs::read(side.home.join("newfile.txt")).unwrap(),
        b"from origin\n"
    );
    assert_eq!(
        std::fs::read(side.home.join("local.txt")).unwrap(),
        b"local\n"
    );
}

#[test]
fn pull_base_does_not_replay_a_pair_whose_rebase_failed_without_conflicts() {
    // Any rebase dot had to abort (here an `--exec` that fails, the
    // stand-in for a broken environment) is remembered. A failure
    // without conflicts may be transient, so the same pair is retried
    // once; after a second failure the next cycle no longer checks out
    // upstream in the work tree and hard-resets it. The warning names
    // the checkout.
    let side = Side::new("current");
    diverge_cleanly(&side);
    let exec = ["--exec", "false"];
    for attempt in 1..=2 {
        let (outcome, _stdout, _stderr) = run_with(&side, Topology::Ordinary, false, false, &exec);
        assert_eq!(outcome.status, PullStatus::Failed);
        assert!(!rebase_in_progress(&side.home));
        assert_eq!(abort_count(&side.home), attempt);
    }
    let (again, _stdout, stderr) = run_with(&side, Topology::Ordinary, false, false, &exec);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(again.status, PullStatus::Failed);
    assert_eq!(abort_count(&side.home), 2, "{stderr}");
    assert!(
        stderr.contains("failed to rebase onto its upstream"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("git -C {} pull --rebase", side.home.display())),
        "{stderr}"
    );
}

#[test]
fn pull_base_repeated_failure_hint_uses_the_separate_git_dir() {
    // A `$HOME` work tree with a separate git dir needs both flags;
    // `git -C $HOME` would not find the repository.
    let side = Side::new("current");
    diverge_cleanly(&side);
    let exec = ["--exec", "false"];
    for _ in 0..2 {
        assert_eq!(
            run_with(&side, Topology::Separate, false, false, &exec)
                .0
                .status,
            PullStatus::Failed
        );
    }
    let (again, _stdout, stderr) = run_with(&side, Topology::Separate, false, false, &exec);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(again.status, PullStatus::Failed);
    assert_eq!(abort_count(&side.home), 2, "{stderr}");
    let home = side.home.display();
    assert!(
        stderr.contains(&format!(
            "git --git-dir={home}/.git --work-tree={home} pull --rebase"
        )),
        "{stderr}"
    );
}

/// Start a rebase in `side.home` that stops mid-way (an `--exec`
/// that fails after the first pick) and record it the way dot
/// records its own rebase before running it.
fn strand_dot_rebase(side: &Side, exec: &str) {
    git(&side.home, &["fetch", "-q", "origin"]);
    let head = git_stdout(&side.home, &["rev-parse", "HEAD"]);
    let onto = git_stdout(&side.home, &["rev-parse", "origin/HEAD^{commit}"]);
    std::fs::write(
        side.home.join(".git/dot-rebase-inflight"),
        format!("{} {}\n", head.trim(), onto.trim()),
    )
    .unwrap();
    assert_eq!(
        git_status(
            &side.home,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "rebase",
                "--exec",
                exec,
                "origin/HEAD"
            ]
        ),
        Some(1)
    );
    assert!(rebase_in_progress(&side.home));
}

#[test]
fn pull_base_heals_its_own_interrupted_rebase_and_pulls() {
    // dot killed mid-rebase leaves its recorded pair in progress. The
    // next run proves it is dot's own, aborts it, and pulls normally
    // instead of failing (or skipping) forever.
    let side = Side::new("current");
    diverge_cleanly(&side);
    strand_dot_rebase(&side, "false");
    let (outcome, _stdout, stderr) = run(&side, true, false);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(outcome.status, PullStatus::Changed, "{stderr}");
    assert_eq!(outcome.rc, 0);
    assert!(stderr.contains("interrupted"), "{stderr}");
    assert_eq!(abort_count(&side.home), 1);
    assert!(!rebase_in_progress(&side.home));
    assert!(!side.home.join(".git/dot-rebase-inflight").exists());
    assert_eq!(
        std::fs::read(side.home.join("newfile.txt")).unwrap(),
        b"from origin\n"
    );
    assert_eq!(
        std::fs::read(side.home.join("local.txt")).unwrap(),
        b"local\n"
    );
}

#[test]
fn pull_base_reports_its_own_rebase_that_cannot_be_aborted() {
    // When the abort itself fails (here a stale index lock), the
    // rebase is dot's own, so the warning may name the exact abort
    // command for this checkout, and the run fails.
    let side = Side::new("current");
    diverge_cleanly(&side);
    strand_dot_rebase(&side, "touch .git/index.lock; false");
    let (outcome, _stdout, stderr) = run_in(&side, Topology::Separate, true, false);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(outcome.status, PullStatus::Failed, "{stderr}");
    assert_eq!(outcome.rc, 1);
    let home = side.home.display();
    assert!(
        stderr.contains(&format!(
            "git --git-dir={home}/.git --work-tree={home} rebase --abort"
        )),
        "{stderr}"
    );
    assert!(rebase_in_progress(&side.home));
}

#[test]
fn pull_base_does_not_heal_its_own_rebase_over_uncommitted_changes() {
    // The heal is a hard reset. If the interrupted rebase's work tree
    // holds changes (for example the user took it over and started
    // resolving), dot leaves it alone, names the abort, and fails.
    let side = Side::new("current");
    diverge_cleanly(&side);
    strand_dot_rebase(&side, "false");
    stage(&side.home, "local.txt", b"resolving\n");
    let (outcome, _stdout, stderr) = run(&side, false, false);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(outcome.status, PullStatus::Failed, "{stderr}");
    assert!(stderr.contains("rebase --abort"), "{stderr}");
    assert!(rebase_in_progress(&side.home));
    assert_eq!(abort_count(&side.home), 0);
    assert_eq!(
        std::fs::read(side.home.join("local.txt")).unwrap(),
        b"resolving\n"
    );
}

#[test]
fn pull_base_reports_a_conflicting_autostash_as_failed() {
    // `rebase --autostash` exits 0 when reapplying the stash
    // conflicts: the edit stays in the stash and the live file holds
    // conflict markers. That must not read as a clean `changed`.
    let side = Side::new("current");
    stage(&side.origin, "base.txt", b"origin change\n");
    commit(&side.origin, "origin change");
    stage(&side.home, "base.txt", b"user edit\n");
    let (outcome, _stdout, stderr) = run(&side, false, false);
    let stderr = String::from_utf8_lossy(&stderr);
    assert_eq!(outcome.status, PullStatus::Failed, "{stderr}");
    assert_eq!(outcome.rc, 1);
    assert!(stderr.contains("stash"), "{stderr}");
    assert!(stderr.contains("base.txt"), "{stderr}");
    assert!(git_stdout(&side.home, &["stash", "list"]).contains("autostash"));
}

#[test]
fn pull_base_warns_once_per_stranded_state_under_cron() {
    // Cron runs every 30 minutes and mails stderr: a detached HEAD or
    // a user rebase warns once per state there, while interactive
    // runs keep warning every time.
    let side = Side::new("changed");
    git(&side.home, &["checkout", "-q", "--detach"]);
    let warned = |quiet| {
        let (outcome, _stdout, stderr) = run(&side, quiet, false);
        assert_eq!(outcome.status, PullStatus::Skipped);
        assert_eq!(outcome.rc, 0);
        String::from_utf8_lossy(&stderr).contains("detached")
    };
    assert!(warned(true));
    assert!(!warned(true));
    assert!(warned(false));
    assert!(!warned(true));
    // A different detached commit is a new state.
    stage(&side.home, "pinned.txt", b"pinned\n");
    commit(&side.home, "pinned");
    assert!(warned(true));
    assert!(!warned(true));
}
