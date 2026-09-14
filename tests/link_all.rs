//! Native integration coverage for the complete overlay link phase.

use dot::repos_link_all;
use dot_test_support::TempDir;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn stage(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}
fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}
struct Fixture {
    _scope: TempDir,
    root: PathBuf,
    home: PathBuf,
    overlay: PathBuf,
    source: PathBuf,
    manifest: PathBuf,
    legacy: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let scope = TempDir::new("link-all").unwrap();
        let root = scope.path().to_path_buf();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        for (rel, body) in files {
            stage(&source, &format!("home/{rel}"), body.as_bytes());
        }
        git(&source, &home, &["init", "-b", "main"]);
        git(&source, &home, &["add", "-A"]);
        git(&source, &home, &["commit", "-qm", "seed", "--allow-empty"]);
        let overlay = root.join("overlay");
        git(
            &root,
            &home,
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                overlay.to_str().unwrap(),
            ],
        );
        let manifest = root.join("state/manifest.tsv");
        let legacy = root.join("state/legacy.tsv");
        Self {
            _scope: scope,
            root,
            home,
            overlay,
            source,
            manifest,
            legacy,
        }
    }
    fn run(
        &self,
        ui_total: Option<&str>,
        verbose: bool,
    ) -> (repos_link_all::LinkOutcome, Vec<u8>, Vec<u8>) {
        let home = self.home.to_string_lossy().into_owned();
        let overlay = self.overlay.to_string_lossy().into_owned();
        let source = self.source.to_string_lossy().into_owned();
        let manifest = self.manifest.to_string_lossy().into_owned();
        let legacy = self.legacy.to_string_lossy().into_owned();
        let entries = vec![format!("ov|{overlay}|{source}|||git")];
        let dest = dot::repos_overlays::DestinationInputs {
            home: home.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![overlay],
            init_backup: None,
            pwd: home.clone(),
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let palette = dot::progress_ui::Palette::empty();
        let log = dot::log::Log::new(false, false);
        let mut stage = dot::progress_ui::Stage::begin(
            palette.clone(),
            ui_total.unwrap_or("0"),
            false,
            false,
            false,
            true,
        );
        let inputs = repos_link_all::Inputs {
            entries: &entries,
            home: &home,
            manifest: &manifest,
            legacy_manifest: &legacy,
            update_jobs: Some("2"),
            ui_total,
            dot_verbose: verbose.then_some("1"),
            dot_quiet: None,
            dest: &dest,
            base: None,
            euid: dot::temp::current_uid().unwrap(),
            source_root_git: &self.root,
            tmp: &self.root,
            tool: &tool,
            palette: &palette,
            multibyte: false,
            bar_width: "8",
            log: &log,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = repos_link_all::link_overlays(&inputs, &mut stage, &mut out, &mut err, 10);
        (result, out, err)
    }
}

#[test]
fn fresh_phase_links_files_commits_manifest_and_converges() {
    let f = Fixture::new(&[("app.conf", "app\n"), ("sub/nested.conf", "nested\n")]);
    let (first, out, err) = f.run(None, false);
    assert_eq!(first.rc, 0);
    assert_eq!(first.changed, 1);
    assert_eq!(first.current, 0);
    assert_eq!(first.changed_items, vec!["ov overlay linked 2"]);
    assert!(err.is_empty());
    assert!(String::from_utf8(out).unwrap().starts_with("Overlays\n"));
    assert!(std::fs::read_link(f.home.join("app.conf")).is_ok());
    assert!(std::fs::read_link(f.home.join("sub/nested.conf")).is_ok());
    let manifest = std::fs::read_to_string(&f.manifest).unwrap();
    assert!(manifest.contains("app.conf\tov\t"));
    assert!(manifest.contains("sub/nested.conf\tov\t"));
    let (second, _, err) = f.run(None, false);
    assert_eq!(second.rc, 0);
    assert_eq!(second.changed, 0);
    assert_eq!(second.current, 1);
    assert!(err.is_empty());
}

#[test]
fn counted_ui_reports_changed_and_current_summaries() {
    let f = Fixture::new(&[("app.conf", "app\n")]);
    let (first, out, err) = f.run(Some("4"), true);
    assert_eq!(first.rc, 0);
    assert!(err.is_empty());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("checking overlay links"));
    assert!(text.contains("1 overlay changed"));
    let (second, out, err) = f.run(Some("4"), true);
    assert_eq!(second.rc, 0);
    assert!(err.is_empty());
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("1 overlay current")
    );
}

#[test]
fn origin_mismatch_warns_without_touching_home() {
    let f = Fixture::new(&[("app.conf", "app\n")]);
    git(
        &f.overlay,
        &f.home,
        &["remote", "set-url", "origin", "file:///elsewhere.git"],
    );
    let (result, _, err) = f.run(None, false);
    assert_eq!(result.rc, 0);
    assert_eq!(result.changed, 0);
    assert!(!f.home.join("app.conf").exists());
    let warning = String::from_utf8(err).unwrap();
    assert!(warning.contains("origin does not match"));
    assert!(warning.contains("remote set-url origin"));
}

#[test]
fn non_worktree_overlay_warns_and_is_skipped() {
    let f = Fixture::new(&[("app.conf", "app\n")]);
    std::fs::remove_dir_all(f.overlay.join(".git")).unwrap();
    let (result, _, err) = f.run(None, false);
    assert_eq!(result.rc, 0);
    assert_eq!(result.changed, 0);
    assert!(!f.home.join("app.conf").exists());
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("not a Git worktree")
    );
}

#[test]
fn empty_counted_phase_succeeds_without_a_manifest() {
    let f = Fixture::new(&[]);
    std::fs::remove_dir_all(f.overlay.join("home")).ok();
    let (result, out, err) = f.run(Some("4"), false);
    assert_eq!(result.rc, 0);
    assert!(err.is_empty());
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("0 overlays current")
    );
    assert!(!f.manifest.exists());
}
