//! Native integration tests for the single-overlay pull orchestrator.

use dot::log::Log;
use dot::progress_ui::Palette;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull_overlay::{
    PullOverlayInputs, PullOverlayOutcome, PullOverlayStatus, pull_overlay,
};
use dot::repos_pull_queries::CandidateEnv;
use dot_test_support::TempDir;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn git(repo: &Path, args: &[&str]) {
    let status = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn stage(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn commit(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

fn palette() -> Palette {
    Palette {
        reset: String::new(),
        bold: String::new(),
        dim: String::new(),
        green: String::new(),
        yellow: String::new(),
        red: String::new(),
        blue: String::new(),
        cyan: String::new(),
        white: String::new(),
    }
}

struct Fixture {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    origin: PathBuf,
    origin_text: String,
    overlay: PathBuf,
    overlay_text: String,
}
impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag).unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        stage(&origin, "home/overlay.txt", b"v1\n");
        commit(&origin, "seed");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let overlay = home.join("overlay");
        let status = dot_test_support::git()
            .arg("clone")
            .arg("-q")
            .arg(&origin)
            .arg(&overlay)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        // `pull_overlay` runs git with the host's config; a repo-local
        // identity keeps its autostash and rebase commits working on
        // hosts (CI) with none configured.
        git(&overlay, &["config", "user.name", "t"]);
        git(&overlay, &["config", "user.email", "t@t"]);
        Self {
            home_text: home.to_string_lossy().into_owned(),
            origin_text: origin.to_string_lossy().into_owned(),
            overlay_text: overlay.to_string_lossy().into_owned(),
            _dir: dir,
            home,
            origin,
            overlay,
        }
    }

    fn pull(
        &self,
        url: &str,
        optional: bool,
        ui_total: Option<&str>,
        verbose: bool,
    ) -> (PullOverlayOutcome, Vec<u8>, Vec<u8>) {
        self.pull_with(url, optional, ui_total, verbose, false, &[])
    }

    /// [`Self::pull`] plus cron quiet mode and extra rebase arguments
    /// (for example `--exec` to stop the rebase without a conflict).
    fn pull_with(
        &self,
        url: &str,
        optional: bool,
        ui_total: Option<&str>,
        verbose: bool,
        quiet: bool,
        extra: &[&str],
    ) -> (PullOverlayOutcome, Vec<u8>, Vec<u8>) {
        let dest = DestinationInputs {
            pwd: self.home_text.clone(),
            home: self.home_text.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![],
            init_backup: None,
        };
        let candidate = CandidateEnv {
            home: self.home_text.clone(),
            checkout: format!("{}/.local/share/cgraf78/dot", self.home_text),
            pwd: self.home_text.clone(),
            source_root: env!("CARGO_MANIFEST_DIR").to_string(),
            state_home: format!("{}/.local/state", self.home_text),
            install_root: format!("{}/.local/share", self.home_text),
            provider_state: format!("{}/.local/state/shdeps", self.home_text),
            overlay_paths: vec![],
            init_backup: None,
        };
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: self.home_text.clone(),
        };
        let logger = Log::new(false, false);
        let colors = palette();
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let mut out = vec![];
        let mut warnings = vec![];
        let extra: Vec<OsString> = extra.iter().map(OsString::from).collect();
        let outcome = pull_overlay(
            &PullOverlayInputs {
                name: "work",
                path: &self.overlay_text,
                url,
                optional,
                extra_args: &extra,
                home: &self.home_text,
                ui_total,
                dot_quiet: Some(if quiet { "1" } else { "0" }),
                dot_verbose: Some(if verbose { "1" } else { "0" }),
                palette: &colors,
                live_active: false,
                multibyte: false,
                candidate: &candidate,
                base: &base,
                quarantine: None,
                overlays: &[],
                dest: &dest,
                manifest: &format!("{}/manifest", self.home_text),
                legacy_manifest: &format!("{}/legacy", self.home_text),
                euid: dot::temp::current_uid().unwrap(),
                source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
                tmp: &self.home,
                tool: &tool,
                log: &logger,
                prefetch: None,
            },
            &mut moves,
            &mut out,
            &mut warnings,
        );
        (outcome, out, warnings)
    }
}

#[test]
fn missing_checkout_is_cloned_and_invalid_clone_is_cleaned_up() {
    let valid = Fixture::new("overlay-clone");
    std::fs::remove_dir_all(&valid.overlay).unwrap();
    let (outcome, _, warnings) = valid.pull(&valid.origin_text, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Cloned);
    assert_eq!(outcome.rc, 0);
    assert!(warnings.is_empty());
    assert_eq!(
        std::fs::read(valid.overlay.join("home/overlay.txt")).unwrap(),
        b"v1\n"
    );

    let bad = Fixture::new("overlay-bad-clone");
    std::fs::remove_dir_all(&bad.overlay).unwrap();
    let missing = bad
        ._dir
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let (outcome, _, warnings) = bad.pull(&missing, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    assert!(!bad.overlay.exists());
    assert!(
        String::from_utf8(warnings)
            .unwrap()
            .contains("clone failed")
    );

    let optional = Fixture::new("overlay-optional-clone");
    std::fs::remove_dir_all(&optional.overlay).unwrap();
    assert_eq!(
        optional.pull(&missing, true, None, false).0.status,
        PullOverlayStatus::Empty
    );
}

#[test]
fn existing_non_worktree_and_wrong_origin_are_left_untouched() {
    let plain = Fixture::new("overlay-plain");
    std::fs::remove_dir_all(&plain.overlay).unwrap();
    stage(&plain.overlay, "user.txt", b"user\n");
    let (outcome, _, warnings) = plain.pull(&plain.origin_text, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    assert_eq!(
        std::fs::read(plain.overlay.join("user.txt")).unwrap(),
        b"user\n"
    );
    assert!(
        String::from_utf8(warnings)
            .unwrap()
            .contains("not a Git worktree")
    );

    let mismatch = Fixture::new("overlay-origin");
    git(
        &mismatch.overlay,
        &["remote", "set-url", "origin", "/elsewhere"],
    );
    let (outcome, _, _) = mismatch.pull(&mismatch.origin_text, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    assert_eq!(
        std::fs::read(mismatch.overlay.join("home/overlay.txt")).unwrap(),
        b"v1\n"
    );
}

#[test]
fn upstream_states_are_classified_and_updates_are_installed() {
    let skipped = Fixture::new("overlay-skipped");
    git(&skipped.overlay, &["branch", "--unset-upstream"]);
    assert_eq!(
        skipped
            .pull(&skipped.origin_text, false, None, false)
            .0
            .status,
        PullOverlayStatus::Skipped
    );

    let current = Fixture::new("overlay-current");
    assert_eq!(
        current
            .pull(&current.origin_text, false, None, false)
            .0
            .status,
        PullOverlayStatus::Current
    );

    let changed = Fixture::new("overlay-changed");
    stage(&changed.origin, "home/new.txt", b"new\n");
    commit(&changed.origin, "update");
    let (outcome, _, warnings) = changed.pull(&changed.origin_text, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Changed);
    assert!(warnings.is_empty());
    assert_eq!(
        std::fs::read(changed.overlay.join("home/new.txt")).unwrap(),
        b"new\n"
    );
}

#[test]
fn invalid_candidates_and_diverged_updates_fail_without_installing_them() {
    let invalid = Fixture::new("overlay-invalid");
    stage(&invalid.origin, "home/.dotfiles/evil", b"bad\n");
    commit(&invalid.origin, "invalid");
    let (outcome, _, warnings) = invalid.pull(&invalid.origin_text, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    assert!(!invalid.overlay.join("home/.dotfiles/evil").exists());
    assert!(
        String::from_utf8(warnings)
            .unwrap()
            .contains("reserved-path validation")
    );

    let diverged = Fixture::new("overlay-diverged");
    stage(&diverged.overlay, "home/overlay.txt", b"local\n");
    commit(&diverged.overlay, "local");
    stage(&diverged.overlay, "home/other.txt", b"seed\n");
    commit(&diverged.overlay, "other");
    stage(&diverged.overlay, "home/other.txt", b"user edit\n");
    stage(&diverged.origin, "home/overlay.txt", b"remote\n");
    commit(&diverged.origin, "remote");
    let outcome = diverged.pull(&diverged.origin_text, false, None, false).0;
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    // The failed rebase is aborted: no conflict markers in the live
    // file, HEAD back on its branch, and the autostashed edit back.
    assert_eq!(
        std::fs::read(diverged.overlay.join("home/overlay.txt")).unwrap(),
        b"local\n"
    );
    assert_eq!(
        std::fs::read(diverged.overlay.join("home/other.txt")).unwrap(),
        b"user edit\n"
    );
    assert!(!rebase_in_progress(&diverged.overlay));
    assert_eq!(
        git_status(&diverged.overlay, &["symbolic-ref", "-q", "HEAD"]),
        Some(0)
    );
    assert_rebase_aborted(&diverged.overlay);
}

/// Exit status of a raw git query in `repo` (hermetic config).
fn git_status(repo: &Path, args: &[&str]) -> Option<i32> {
    dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .code()
}

/// Proof that a rebase really started and was aborted, with the
/// autostash consumed (identical bytes alone could also mean the
/// rebase never started).
fn assert_rebase_aborted(repo: &Path) {
    let output = |args: &[&str]| {
        let output = dot_test_support::git()
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let reflog = output(&["reflog", "-n", "5", "--format=%gs"]);
    assert!(reflog.contains("rebase (abort)"), "{reflog}");
    assert_eq!(output(&["stash", "list"]), "");
}

/// Whether `repo` has a rebase in progress (either backend).
fn rebase_in_progress(repo: &Path) -> bool {
    repo.join(".git/rebase-merge").exists() || repo.join(".git/rebase-apply").exists()
}

#[test]
fn a_recorded_conflict_is_not_replayed_until_a_side_moves() {
    let diverged = Fixture::new("overlay-repeat-conflict");
    stage(&diverged.overlay, "home/overlay.txt", b"local\n");
    commit(&diverged.overlay, "local");
    stage(&diverged.origin, "home/overlay.txt", b"remote\n");
    commit(&diverged.origin, "remote");
    assert_eq!(
        diverged
            .pull(&diverged.origin_text, false, None, false)
            .0
            .status,
        PullOverlayStatus::Failed
    );
    let aborts = || {
        let output = dot_test_support::git()
            .arg("-C")
            .arg(&diverged.overlay)
            .args(["reflog", "--format=%gs"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("rebase (abort)"))
            .count()
    };
    assert_eq!(aborts(), 1);
    // Loud and optional repeats both leave the work tree alone.
    let (outcome, _, warnings) = diverged.pull(&diverged.origin_text, false, None, false);
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    assert!(
        String::from_utf8_lossy(&warnings).contains("failed to rebase onto its upstream"),
        "{}",
        String::from_utf8_lossy(&warnings)
    );
    assert_eq!(
        diverged
            .pull(&diverged.origin_text, true, None, false)
            .0
            .status,
        PullOverlayStatus::Empty
    );
    assert_eq!(aborts(), 1);
    stage(&diverged.origin, "home/later.txt", b"later\n");
    commit(&diverged.origin, "later");
    assert_eq!(
        diverged
            .pull(&diverged.origin_text, false, None, false)
            .0
            .status,
        PullOverlayStatus::Failed
    );
    assert_eq!(aborts(), 2);
}

#[test]
fn optional_overlay_failed_rebase_is_aborted() {
    // The optional pull stays quiet and statusless on failure, but it
    // must still not strand the checkout mid-rebase.
    let diverged = Fixture::new("overlay-optional-diverged");
    stage(&diverged.overlay, "home/overlay.txt", b"local\n");
    commit(&diverged.overlay, "local");
    stage(&diverged.origin, "home/overlay.txt", b"remote\n");
    commit(&diverged.origin, "remote");
    let outcome = diverged.pull(&diverged.origin_text, true, None, false).0;
    assert_eq!(outcome.status, PullOverlayStatus::Empty);
    assert!(!rebase_in_progress(&diverged.overlay));
    assert_rebase_aborted(&diverged.overlay);
    assert_eq!(
        std::fs::read(diverged.overlay.join("home/overlay.txt")).unwrap(),
        b"local\n"
    );
}

#[test]
fn user_rebase_and_detached_head_warn_and_skip_without_failing() {
    // A rebase dot did not start is the user's work in progress: it
    // warns and skips, optional or not, never fails the update, and
    // never suggests aborting it. A bare detached HEAD does the same.
    for optional in [false, true] {
        let stranded = Fixture::new("overlay-stranded");
        stage(&stranded.overlay, "home/overlay.txt", b"local\n");
        commit(&stranded.overlay, "local");
        stage(&stranded.origin, "home/overlay.txt", b"remote\n");
        commit(&stranded.origin, "remote");
        git(&stranded.overlay, &["fetch", "-q", "origin"]);
        assert_eq!(
            git_status(&stranded.overlay, &["rebase", "origin/HEAD"]),
            Some(1)
        );
        let (outcome, _, warnings) = stranded.pull(&stranded.origin_text, optional, None, false);
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Skipped,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains("dot did not start"), "{warnings}");
        assert!(!warnings.contains("--abort"), "{warnings}");
        assert!(rebase_in_progress(&stranded.overlay));

        let detached = Fixture::new("overlay-detached");
        git(&detached.overlay, &["checkout", "-q", "--detach"]);
        let (outcome, _, warnings) = detached.pull(&detached.origin_text, optional, None, false);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Skipped,
            "optional={optional}"
        );
        assert!(
            String::from_utf8_lossy(&warnings).contains("detached"),
            "{}",
            String::from_utf8_lossy(&warnings)
        );
    }
}

/// Seed `fixture` with a local commit and an upstream commit that
/// touch different files, so a rebase replays cleanly unless
/// something other than a conflict stops it.
fn diverge_cleanly(fixture: &Fixture) {
    stage(&fixture.overlay, "home/local.txt", b"local\n");
    commit(&fixture.overlay, "local");
    stage(&fixture.origin, "home/new.txt", b"new\n");
    commit(&fixture.origin, "update");
}

/// Number of `rebase (abort)` entries in the overlay's HEAD reflog.
fn abort_count(repo: &Path) -> usize {
    let output = dot_test_support::git()
        .arg("-C")
        .arg(repo)
        .args(["reflog", "--format=%gs"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("rebase (abort)"))
        .count()
}

#[test]
fn commit_hooks_do_not_stop_the_replay_of_local_commits() {
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-hooks");
        diverge_cleanly(&fixture);
        let hooks = fixture.overlay.join(".git/gate-hooks");
        std::fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("prepare-commit-msg");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &fixture.overlay,
            &["config", "core.hooksPath", &hooks.to_string_lossy()],
        );
        let (outcome, _, warnings) = fixture.pull(&fixture.origin_text, optional, None, false);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Changed,
            "optional={optional}: {}",
            String::from_utf8_lossy(&warnings)
        );
    }
}

#[test]
fn a_rebase_that_failed_without_conflicts_is_retried_once_then_not_replayed() {
    let fixture = Fixture::new("overlay-exec-failure");
    diverge_cleanly(&fixture);
    let exec = ["--exec", "false"];
    let run = |optional| {
        fixture
            .pull_with(&fixture.origin_text, optional, None, false, false, &exec)
            .clone_parts()
    };
    assert_eq!(run(false).0, PullOverlayStatus::Failed);
    assert_eq!(abort_count(&fixture.overlay), 1);
    // The optional pull takes the one retry quietly.
    assert_eq!(run(true).0, PullOverlayStatus::Empty);
    assert_eq!(abort_count(&fixture.overlay), 2);
    let (status, warnings) = run(false);
    assert_eq!(status, PullOverlayStatus::Failed);
    assert!(
        warnings.contains(&format!("git -C {} pull --rebase", fixture.overlay_text)),
        "{warnings}"
    );
    assert_eq!(run(true).0, PullOverlayStatus::Empty);
    assert_eq!(abort_count(&fixture.overlay), 2);
}

/// Start a rebase in the overlay that stops mid-way (an `--exec`
/// that fails after the first pick) and record it the way dot
/// records its own rebase before running it.
fn strand_dot_rebase(fixture: &Fixture, exec: &str) {
    git(&fixture.overlay, &["fetch", "-q", "origin"]);
    let rev = |spec: &str| {
        let output = dot_test_support::git()
            .arg("-C")
            .arg(&fixture.overlay)
            .args(["rev-parse", spec])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    std::fs::write(
        fixture.overlay.join(".git/dot-rebase-inflight"),
        format!("{} {}\n", rev("HEAD"), rev("origin/HEAD^{commit}")),
    )
    .unwrap();
    assert_eq!(
        git_status(&fixture.overlay, &["rebase", "--exec", exec, "origin/HEAD"]),
        Some(1)
    );
    assert!(rebase_in_progress(&fixture.overlay));
}

#[test]
fn its_own_interrupted_rebase_is_healed_and_pulled() {
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-heal");
        diverge_cleanly(&fixture);
        strand_dot_rebase(&fixture, "false");
        let (outcome, _, warnings) = fixture.pull(&fixture.origin_text, optional, None, false);
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Changed,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains("interrupted"), "{warnings}");
        assert_eq!(abort_count(&fixture.overlay), 1);
        assert!(!rebase_in_progress(&fixture.overlay));
        assert_eq!(
            std::fs::read(fixture.overlay.join("home/new.txt")).unwrap(),
            b"new\n"
        );
    }
}

#[test]
fn a_failed_abort_is_surfaced_even_for_optional_overlays() {
    // The optional pull stays quiet about ordinary failures, but a
    // rebase of its own that `--abort` cannot undo (here a stale
    // index lock) leaves mid-rebase files live: it warns with the
    // exact command for this checkout and fails, both on the run
    // itself and when a later run finds its interrupted rebase.
    // Each scenario is a fresh fixture: the in-process upstream probe
    // cache would otherwise serve the first run's answer.
    let lock = "touch .git/index.lock; false";
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-abort-fails");
        diverge_cleanly(&fixture);
        let hint = format!("git -C {} rebase --abort", fixture.overlay_text);
        let (outcome, _, warnings) = fixture.pull_with(
            &fixture.origin_text,
            optional,
            None,
            false,
            false,
            &["--exec", lock],
        );
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Failed,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains(&hint), "{warnings}");
        assert!(rebase_in_progress(&fixture.overlay));
        // The record the pull wrote before rebasing names exactly the
        // state git left, so the next run can claim it.
        let state = |leaf: &str| {
            std::fs::read_to_string(fixture.overlay.join(".git/rebase-merge").join(leaf))
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(
            std::fs::read_to_string(fixture.overlay.join(".git/dot-rebase-inflight")).unwrap(),
            format!("{} {}\n", state("orig-head"), state("onto"))
        );

        let stranded = Fixture::new("overlay-heal-fails");
        diverge_cleanly(&stranded);
        strand_dot_rebase(&stranded, lock);
        let hint = format!("git -C {} rebase --abort", stranded.overlay_text);
        let (outcome, _, warnings) = stranded.pull(&stranded.origin_text, optional, None, false);
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Failed,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains(&hint), "{warnings}");
        assert!(rebase_in_progress(&stranded.overlay));
    }
}

#[test]
fn a_conflicting_autostash_is_reported_as_failed() {
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-autostash");
        stage(&fixture.origin, "home/overlay.txt", b"remote\n");
        commit(&fixture.origin, "remote");
        stage(&fixture.overlay, "home/overlay.txt", b"user edit\n");
        let (outcome, _, warnings) = fixture.pull(&fixture.origin_text, optional, None, false);
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Failed,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains("stash"), "{warnings}");
        assert!(warnings.contains("home/overlay.txt"), "{warnings}");
    }
}

#[test]
fn an_unmerged_index_fails_every_pull_without_rebasing_over_it() {
    // Optional or not: conflict markers in live overlay files must not
    // read as current, and no rebase may run on top of them.
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-unmerged");
        stage(&fixture.origin, "home/overlay.txt", b"remote\n");
        commit(&fixture.origin, "remote");
        stage(&fixture.overlay, "home/overlay.txt", b"user edit\n");
        assert_eq!(
            fixture
                .pull(&fixture.origin_text, optional, None, false)
                .0
                .status,
            PullOverlayStatus::Failed
        );
        stage(&fixture.origin, "home/later.txt", b"later\n");
        commit(&fixture.origin, "later");
        let (outcome, _, warnings) = fixture.pull(&fixture.origin_text, optional, None, false);
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Failed,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains("home/overlay.txt"), "{warnings}");
        assert!(
            warnings.contains(&format!("git -C {} stash list", fixture.overlay_text)),
            "{warnings}"
        );
        assert!(!fixture.overlay.join("home/later.txt").exists());
        // Cron keeps failing but does not repeat the warning.
        let (outcome, _, warnings) =
            fixture.pull_with(&fixture.origin_text, optional, None, false, true, &[]);
        assert_eq!(outcome.status, PullOverlayStatus::Failed);
        assert!(
            !String::from_utf8_lossy(&warnings).contains("home/overlay.txt"),
            "{}",
            String::from_utf8_lossy(&warnings)
        );
    }
}

#[test]
fn a_conflicted_merge_in_progress_is_skipped_not_failed() {
    // Unmerged entries from the user's own merge are their work in
    // progress: skip with rc 0 (optional overlays included) instead
    // of failing the update or pointing at the stash.
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-merge");
        git(&fixture.overlay, &["checkout", "-q", "-b", "side"]);
        stage(&fixture.overlay, "home/overlay.txt", b"side\n");
        commit(&fixture.overlay, "side");
        git(&fixture.overlay, &["checkout", "-q", "-"]);
        stage(&fixture.overlay, "home/overlay.txt", b"main\n");
        commit(&fixture.overlay, "main");
        assert_eq!(
            git_status(&fixture.overlay, &["merge", "-q", "side"]),
            Some(1)
        );
        let (outcome, _, warnings) = fixture.pull(&fixture.origin_text, optional, None, false);
        let warnings = String::from_utf8_lossy(&warnings);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Skipped,
            "optional={optional}: {warnings}"
        );
        assert!(warnings.contains("merge"), "{warnings}");
        assert!(!warnings.contains("stash"), "{warnings}");
        assert!(fixture.overlay.join(".git/MERGE_HEAD").exists());
    }
}

#[test]
fn stranded_warnings_repeat_under_cron_only_when_the_state_changes() {
    let detached = Fixture::new("overlay-detached-cron");
    git(&detached.overlay, &["checkout", "-q", "--detach"]);
    let warned = |quiet| {
        let (outcome, _, warnings) =
            detached.pull_with(&detached.origin_text, false, None, false, quiet, &[]);
        assert_eq!(outcome.status, PullOverlayStatus::Skipped);
        String::from_utf8_lossy(&warnings).contains("detached")
    };
    assert!(warned(true));
    assert!(!warned(true));
    assert!(warned(false));
    stage(&detached.overlay, "home/pinned.txt", b"pinned\n");
    commit(&detached.overlay, "pinned");
    assert!(warned(true));
    assert!(!warned(true));
}

#[test]
fn changed_with_many_sibling_updates_succeeds() {
    // 40 updated siblings under one parent make the in-memory parent
    // snapshot far longer than NAME_MAX; it is text, never a path,
    // so a successful rebase still reports Changed on both the loud
    // and the optional pull paths.
    for optional in [false, true] {
        let fixture = Fixture::new("overlay-many-siblings");
        for index in 0..40 {
            stage(
                &fixture.origin,
                &format!("home/sibling-{index:02}"),
                b"sibling\n",
            );
        }
        commit(&fixture.origin, "many siblings");
        let (outcome, _, warnings) = fixture.pull(&fixture.origin_text, optional, None, false);
        assert_eq!(
            outcome.status,
            PullOverlayStatus::Changed,
            "optional={optional}: {}",
            String::from_utf8_lossy(&warnings)
        );
        assert_eq!(
            std::fs::read(fixture.overlay.join("home/sibling-39")).unwrap(),
            b"sibling\n"
        );
    }
}

#[test]
fn counted_verbose_rows_report_clone_and_current_without_changing_status() {
    let clone = Fixture::new("overlay-clone-ui");
    std::fs::remove_dir_all(&clone.overlay).unwrap();
    let (outcome, out, _) = clone.pull(&clone.origin_text, false, Some("1"), true);
    assert_eq!(outcome.status, PullOverlayStatus::Cloned);
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("cloning"));
    assert!(out.contains("cloned"));

    let current = Fixture::new("overlay-current-ui");
    let (outcome, out, _) = current.pull(&current.origin_text, false, Some("1"), true);
    assert_eq!(outcome.status, PullOverlayStatus::Current);
    assert!(
        out.is_empty(),
        "the generation fast path returns before UI output"
    );
}

/// Status plus warnings text, for tests that only need those parts.
trait CloneParts {
    fn clone_parts(self) -> (PullOverlayStatus, String);
}

impl CloneParts for (PullOverlayOutcome, Vec<u8>, Vec<u8>) {
    fn clone_parts(self) -> (PullOverlayStatus, String) {
        (self.0.status, String::from_utf8_lossy(&self.2).into_owned())
    }
}
