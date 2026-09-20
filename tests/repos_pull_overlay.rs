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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
        let status = Command::new("git")
            .arg("clone")
            .arg("-q")
            .arg(&origin)
            .arg(&overlay)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
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

    #[allow(clippy::too_many_arguments)]
    fn pull(
        &self,
        url: &str,
        optional: bool,
        ui_total: Option<&str>,
        verbose: bool,
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
        let extra: &[OsString] = &[];
        let outcome = pull_overlay(
            &PullOverlayInputs {
                name: "work",
                path: &self.overlay_text,
                url,
                optional,
                extra_args: extra,
                home: &self.home_text,
                ui_total,
                dot_quiet: Some("0"),
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
    stage(&diverged.origin, "home/overlay.txt", b"remote\n");
    commit(&diverged.origin, "remote");
    let outcome = diverged.pull(&diverged.origin_text, false, None, false).0;
    assert_eq!(outcome.status, PullOverlayStatus::Failed);
    let conflicted = std::fs::read(diverged.overlay.join("home/overlay.txt")).unwrap();
    assert!(
        conflicted
            .windows(b"<<<<<<< HEAD".len())
            .any(|part| part == b"<<<<<<< HEAD")
    );
    assert!(
        conflicted
            .windows(b"local".len())
            .any(|part| part == b"local")
    );
    assert!(
        conflicted
            .windows(b"remote".len())
            .any(|part| part == b"remote")
    );
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
