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
        self.run_with_live(ui_total, verbose, false)
    }

    fn run_with_live(
        &self,
        ui_total: Option<&str>,
        verbose: bool,
        live: bool,
    ) -> (repos_link_all::LinkOutcome, Vec<u8>, Vec<u8>) {
        let overlay = self.overlay.to_string_lossy().into_owned();
        let source = self.source.to_string_lossy().into_owned();
        let entries = vec![format!("ov|{overlay}|{source}|||git")];
        self.run_entries(&entries, ui_total, verbose, live)
    }

    fn run_entries(
        &self,
        entries: &[String],
        ui_total: Option<&str>,
        verbose: bool,
        live: bool,
    ) -> (repos_link_all::LinkOutcome, Vec<u8>, Vec<u8>) {
        let home = self.home.to_string_lossy().into_owned();
        let overlay = self.overlay.to_string_lossy().into_owned();
        let manifest = self.manifest.to_string_lossy().into_owned();
        let legacy = self.legacy.to_string_lossy().into_owned();
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
            live,
            false,
            true,
        );
        let inputs = repos_link_all::Inputs {
            entries,
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
        let result = repos_link_all::link_overlays(&inputs, &mut stage, &mut out, &mut err);
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

/// The symlink target at `rel` under `home`, or `None` when the path
/// is missing or not a link.
fn link_at(home: &Path, rel: &str) -> Option<PathBuf> {
    std::fs::read_link(home.join(rel)).ok()
}

#[test]
fn origin_mismatch_preserves_previously_installed_links() {
    // "Leaving it untouched" must hold for links installed by an
    // earlier run too: the skipped overlay's manifest records carry
    // into the new generation, so stale cleanup keeps its links. The
    // trigger here is a changed configured `url=` (a base commit
    // editing `overlays.d`); an in-process `remote set-url` would hit
    // the memoized origin probe instead.
    let f = Fixture::new(&[("app.conf", "app\n"), ("sub/nested.conf", "nested\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    let app = link_at(&f.home, "app.conf");
    let nested = link_at(&f.home, "sub/nested.conf");
    assert!(app.is_some() && nested.is_some());
    let before = std::fs::read_to_string(&f.manifest).unwrap();
    let overlay = f.overlay.to_string_lossy().into_owned();
    let moved = vec![format!("ov|{overlay}|file:///elsewhere.git|||git")];
    let (result, out, err) = f.run_entries(&moved, None, true, false);
    assert_eq!(result.rc, 0);
    assert_eq!(result.changed, 0);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("origin does not match")
    );
    assert!(
        !String::from_utf8(out).unwrap().contains("removed:"),
        "skipped overlay links were cleaned as stale"
    );
    assert_eq!(link_at(&f.home, "app.conf"), app);
    assert_eq!(link_at(&f.home, "sub/nested.conf"), nested);
    assert_eq!(std::fs::read_to_string(&f.manifest).unwrap(), before);
    // Restoring the URL converges without relinking.
    let (again, _, err) = f.run(None, false);
    assert_eq!(again.rc, 0);
    assert_eq!(again.current, 1);
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
}

#[test]
fn non_worktree_overlay_preserves_previously_installed_links() {
    let f = Fixture::new(&[("app.conf", "app\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    let app = link_at(&f.home, "app.conf");
    assert!(app.is_some());
    let before = std::fs::read_to_string(&f.manifest).unwrap();
    std::fs::remove_dir_all(f.overlay.join(".git")).unwrap();
    // The worktree probe memoizes per path spelling for the process
    // lifetime (each `dot update` is a fresh process); an equivalent
    // spelling observes the removed `.git` like the next run would.
    let overlay = f.overlay.to_string_lossy().into_owned();
    let source = f.source.to_string_lossy().into_owned();
    let respelled = vec![format!("ov|{overlay}/.|{source}|||git")];
    let (result, _, err) = f.run_entries(&respelled, None, false, false);
    assert_eq!(result.rc, 0);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("not a Git worktree")
    );
    assert_eq!(link_at(&f.home, "app.conf"), app);
    assert_eq!(std::fs::read_to_string(&f.manifest).unwrap(), before);
}

#[test]
fn active_overlay_wins_a_path_shared_with_a_skipped_overlay() {
    // Carrying happens after every active overlay recorded its paths:
    // a path an active overlay now provides keeps only that overlay's
    // record, while the skipped overlay's other links still carry.
    let f = Fixture::new(&[("app.conf", "app\n"), ("only.conf", "only\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    let only = link_at(&f.home, "only.conf");
    assert!(only.is_some());
    let source2 = f.root.join("source2");
    stage(&source2, "home/app.conf", b"second\n");
    git(&source2, &f.home, &["init", "-b", "main"]);
    git(&source2, &f.home, &["add", "-A"]);
    git(&source2, &f.home, &["commit", "-qm", "seed"]);
    let overlay2 = f.root.join("overlay2");
    git(
        &f.root,
        &f.home,
        &[
            "clone",
            "-q",
            source2.to_str().unwrap(),
            overlay2.to_str().unwrap(),
        ],
    );
    let overlay = f.overlay.to_string_lossy().into_owned();
    let entries = vec![
        format!("ov|{overlay}|file:///elsewhere.git|||git"),
        format!("ov2|{}|{}|||git", overlay2.display(), source2.display()),
    ];
    let (result, _, err) = f.run_entries(&entries, None, false, false);
    assert_eq!(result.rc, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(link_at(&f.home, "only.conf"), only);
    let manifest = std::fs::read_to_string(&f.manifest).unwrap();
    let app_records: Vec<&str> = manifest
        .lines()
        .filter(|line| line.starts_with("app.conf\t"))
        .collect();
    assert_eq!(
        link_at(&f.home, "app.conf"),
        Some(PathBuf::from(".dotfiles-ov2/home/app.conf"))
    );
    assert_eq!(app_records.len(), 1, "{manifest}");
    assert!(app_records[0].starts_with("app.conf\tov2\t"), "{manifest}");
    assert!(manifest.contains("only.conf\tov\t"), "{manifest}");
}

#[test]
fn deselected_overlay_links_are_still_cleaned() {
    // An overlay absent from the entries is genuinely gone: its links
    // stay stale and are removed, unlike a skipped overlay's.
    let f = Fixture::new(&[("app.conf", "app\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    assert!(link_at(&f.home, "app.conf").is_some());
    let (result, _, err) = f.run_entries(&[], None, false, false);
    assert_eq!(result.rc, 0, "{}", String::from_utf8_lossy(&err));
    assert!(std::fs::symlink_metadata(f.home.join("app.conf")).is_err());
    assert!(
        !std::fs::read_to_string(&f.manifest)
            .unwrap()
            .contains("app.conf")
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

/// Live-rendered rows start with the `\r\x1b[K[` redraw prefix (split off
/// the `\r` here); a `-` immediately followed by a digit inside one can
/// only come from the elapsed field, since labels, counters, bars, and
/// spinner frames never pair them.
fn live_rows(out: &[u8]) -> Vec<&[u8]> {
    out.split(|byte| *byte == b'\r')
        .filter(|row| row.starts_with(b"\x1b[K["))
        .collect()
}

fn live_rows_have_negative_elapsed(out: &[u8]) -> bool {
    live_rows(out).iter().any(|row| {
        row.windows(2)
            .any(|pair| pair[0] == b'-' && pair[1].is_ascii_digit())
    })
}

/// Progress rows are the only live rows carrying the `[#` progress bar.
fn live_progress_was_rendered(out: &[u8]) -> bool {
    live_rows(out)
        .iter()
        .any(|row| row.windows(2).any(|window| window == b"[#"))
}

#[test]
fn live_link_progress_reports_non_negative_elapsed() {
    let f = Fixture::new(&[("app.conf", "app\n")]);
    let (result, out, err) = f.run_with_live(Some("4"), false, true);
    assert_eq!(result.rc, 0);
    assert!(err.is_empty());
    assert!(
        live_progress_was_rendered(&out),
        "expected live progress rows: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        !live_rows_have_negative_elapsed(&out),
        "live progress carried a negative elapsed stamp: {}",
        String::from_utf8_lossy(&out)
    );
}
