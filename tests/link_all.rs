//! Native integration coverage for the complete overlay link phase.

use dot::repos_link_all;
use dot_test_support::TempDir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn stage(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}
fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let mut command = Command::new(dot_test_support::real_tool("git"));
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home);
    let out = dot_test_support::isolate_git(&mut command)
        .args(args)
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
        self.run_pass(entries, ui_total, verbose, live, None)
    }

    /// The fixture overlay's record (`ov`), as [`Self::run`] links it.
    fn entry(&self) -> String {
        let overlay = self.overlay.to_string_lossy().into_owned();
        let source = self.source.to_string_lossy().into_owned();
        format!("ov|{overlay}|{source}|||git")
    }

    /// The repair pass (`follow_up`) for `paths` over `entries` under
    /// counted UI.
    fn run_follow_up(
        &self,
        entries: &[String],
        paths: &[&str],
    ) -> (repos_link_all::LinkOutcome, Vec<u8>, Vec<u8>) {
        let paths: HashSet<String> = paths.iter().map(|path| path.to_string()).collect();
        self.run_pass(entries, Some("4"), false, false, Some(&paths))
    }

    fn recorded(&self) -> Vec<repos_link_all::RecordedLink> {
        repos_link_all::recorded_links(
            &self.home.to_string_lossy(),
            &self.manifest.to_string_lossy(),
        )
    }

    fn run_pass(
        &self,
        entries: &[String],
        ui_total: Option<&str>,
        verbose: bool,
        live: bool,
        follow_up: Option<&HashSet<String>>,
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
            follow_up,
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

fn link(rel: &str, intact: bool) -> repos_link_all::RecordedLink {
    owned(rel, "ov", intact)
}

fn owned(rel: &str, owner: &str, intact: bool) -> repos_link_all::RecordedLink {
    repos_link_all::RecordedLink {
        rel: rel.into(),
        owner: owner.into(),
        intact,
    }
}

#[test]
fn recorded_links_reads_each_record_back_against_its_home_link() {
    let f = Fixture::new(&[
        ("a.conf", "a\n"),
        ("b.conf", "b\n"),
        ("c.conf", "c\n"),
        ("d.conf", "d\n"),
    ]);
    assert_eq!(f.recorded(), Vec::new(), "no manifest records nothing");
    assert_eq!(f.run(None, false).0.rc, 0);
    assert_eq!(
        f.recorded(),
        vec![
            link("a.conf", true),
            link("b.conf", true),
            link("c.conf", true),
            link("d.conf", true),
        ]
    );
    // Retargeted, removed, and replaced with content: none reads back.
    std::fs::remove_file(f.home.join("a.conf")).unwrap();
    std::os::unix::fs::symlink(f.root.join("elsewhere"), f.home.join("a.conf")).unwrap();
    std::fs::remove_file(f.home.join("b.conf")).unwrap();
    std::fs::remove_file(f.home.join("c.conf")).unwrap();
    std::fs::write(f.home.join("c.conf"), "local\n").unwrap();
    assert_eq!(
        f.recorded(),
        vec![
            link("a.conf", false),
            link("b.conf", false),
            link("c.conf", false),
            link("d.conf", true),
        ]
    );
}

#[test]
fn recorded_links_reads_nothing_from_a_malformed_manifest() {
    let f = Fixture::new(&[("a.conf", "a\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    let mut manifest = std::fs::read(&f.manifest).unwrap();
    manifest.extend_from_slice(b"no tab here\n");
    std::fs::write(&f.manifest, manifest).unwrap();
    std::fs::remove_file(f.home.join("a.conf")).unwrap();
    assert_eq!(f.recorded(), Vec::new());
}

/// Overlays colliding on a path each record it; the last one linked owns
/// the live link, so only its record counts.
#[test]
fn recorded_links_reads_a_collided_path_against_its_last_owner_only() {
    let f = Fixture::new(&[]);
    std::fs::create_dir_all(f.manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &f.manifest,
        "shared.conf\ta\t.dotfiles-a/home/shared.conf\n\
         shared.conf\tb\t.dotfiles-b/home/shared.conf\n\
         only.conf\ta\t.dotfiles-a/home/only.conf\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(".dotfiles-b/home/shared.conf", f.home.join("shared.conf")).unwrap();
    assert_eq!(
        f.recorded(),
        vec![
            owned("shared.conf", "b", true),
            owned("only.conf", "a", false)
        ]
    );
}

#[test]
fn follow_up_pass_restores_a_replaced_link_without_rendering_a_stage() {
    let f = Fixture::new(&[("app.conf", "app\n"), ("keep.conf", "keep\n")]);
    assert_eq!(f.run(Some("4"), false).0.rc, 0);
    let target = std::fs::read_link(f.home.join("app.conf")).unwrap();
    std::fs::remove_file(f.home.join("app.conf")).unwrap();
    std::os::unix::fs::symlink(f.root.join("raw-binary"), f.home.join("app.conf")).unwrap();
    let (result, out, err) = f.run_follow_up(&[f.entry()], &["app.conf"]);
    assert_eq!(result.rc, 0);
    assert_eq!(result.changed_items, vec!["ov overlay linked 1"]);
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    assert_eq!(std::fs::read_link(f.home.join("app.conf")).unwrap(), target);
    assert!(f.recorded().iter().all(|link| link.intact));
}

#[test]
fn follow_up_pass_reports_conflicts_only_for_the_paths_it_repairs() {
    let f = Fixture::new(&[("mine.conf", "overlay\n"), ("app.conf", "app\n")]);
    std::fs::write(f.home.join("mine.conf"), "user\n").unwrap();
    let (result, _, err) = f.run(Some("4"), false);
    assert_eq!(result.rc, 0);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "  skip (would clobber untracked file): mine.conf\n  \
skip (stale overlay path has local content): mine.conf\n"
    );
    std::fs::remove_file(f.home.join("app.conf")).unwrap();
    std::fs::write(f.home.join("app.conf"), "tool\n").unwrap();
    let (result, out, err) = f.run_follow_up(&[f.entry()], &["app.conf"]);
    assert_eq!(result.rc, 0);
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "  skip (would clobber untracked file): app.conf\n  \
skip (stale overlay path has local content): app.conf\n"
    );
    assert_eq!(std::fs::read(f.home.join("app.conf")).unwrap(), b"tool\n");
}

#[test]
fn follow_up_pass_skips_an_unusable_overlay_without_repeating_its_warning() {
    let f = Fixture::new(&[("app.conf", "app\n")]);
    let plain = f.root.join("plain");
    stage(&plain, "home/plain.conf", b"plain\n");
    let entries = vec![
        f.entry(),
        format!("plain|{}|file:///plain.git|||git", plain.display()),
    ];
    let (result, _, err) = f.run_entries(&entries, Some("4"), false, false);
    assert_eq!(result.rc, 0);
    assert_eq!(result.skipped, ["plain".to_string()].into());
    assert!(
        String::from_utf8_lossy(&err)
            .contains("plain overlay path exists but is not a Git worktree"),
        "the Overlays stage warns: {}",
        String::from_utf8_lossy(&err)
    );
    let (result, out, err) = f.run_follow_up(&entries, &["plain.conf"]);
    assert_eq!(result.rc, 0);
    assert_eq!(result.skipped, ["plain".to_string()].into());
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    assert!(!f.home.join("plain.conf").exists());
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

/// Write `body` as the leftover pending manifest an interrupted run
/// leaves beside `manifest`, private like the publisher makes it.
fn leave_pending(manifest: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let pending = PathBuf::from(format!("{}.pending", manifest.display()));
    std::fs::write(&pending, body).unwrap();
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn skipped_overlay_does_not_carry_leftover_pending_candidates() {
    // A pending manifest left by an interrupted run holds unverified
    // candidates, not installed links. Only committed records carry
    // for a skipped overlay; a candidate it never linked must not be
    // promoted into the committed generation as if it were installed.
    let f = Fixture::new(&[("app.conf", "app\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    let app = link_at(&f.home, "app.conf");
    assert!(app.is_some());
    let before = std::fs::read_to_string(&f.manifest).unwrap();
    leave_pending(&f.manifest, &format!("{before}ghost.conf\tov\n"));
    let overlay = f.overlay.to_string_lossy().into_owned();
    let moved = vec![format!("ov|{overlay}|file:///elsewhere.git|||git")];
    let (result, _, err) = f.run_entries(&moved, None, false, false);
    assert_eq!(result.rc, 0, "{}", String::from_utf8_lossy(&err));
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("origin does not match")
    );
    assert_eq!(link_at(&f.home, "app.conf"), app);
    assert!(std::fs::symlink_metadata(f.home.join("ghost.conf")).is_err());
    assert_eq!(std::fs::read_to_string(&f.manifest).unwrap(), before);
    // The committed generation supersedes the leftover authority.
    assert!(!PathBuf::from(format!("{}.pending", f.manifest.display())).exists());
}

#[test]
fn skipped_overlay_does_not_carry_records_on_reserved_paths() {
    // A committed record can name a path that is overlay-control
    // reserved (written before the path was reserved, or by hand).
    // The authority load filters it out; carrying a skipped overlay's
    // records must not reintroduce it, and the file there stays put.
    let f = Fixture::new(&[("app.conf", "app\n")]);
    assert_eq!(f.run(None, false).0.rc, 0);
    let app = link_at(&f.home, "app.conf");
    assert!(app.is_some());
    let before = std::fs::read_to_string(&f.manifest).unwrap();
    let reserved = ".config/dot/profiles.d/work.conf";
    stage(&f.home, reserved, b"profile\n");
    std::fs::write(&f.manifest, format!("{before}{reserved}\tov\n")).unwrap();
    let overlay = f.overlay.to_string_lossy().into_owned();
    let moved = vec![format!("ov|{overlay}|file:///elsewhere.git|||git")];
    let (result, _, err) = f.run_entries(&moved, None, false, false);
    assert_eq!(result.rc, 0, "{}", String::from_utf8_lossy(&err));
    // Only a skipped overlay reaches the carry; an active one would
    // re-record `app.conf` and pass without exercising it.
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("origin does not match")
    );
    assert_eq!(link_at(&f.home, "app.conf"), app);
    assert_eq!(std::fs::read(f.home.join(reserved)).unwrap(), b"profile\n");
    assert_eq!(std::fs::read_to_string(&f.manifest).unwrap(), before);
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
