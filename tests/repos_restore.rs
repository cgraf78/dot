//! Native contracts for the installed-link recovery walk.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_base::{Base, Topology};
use dot::repos_overlays::{self, DestinationInputs, RestoreInstalledInputs};
use dot_test_support::TempDir;

/// Run `git -C dir args`, silenced, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Destination shape probe shared by both sides.
fn dst_state(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Err(_) => "absent".to_string(),
        Ok(meta) if meta.file_type().is_symlink() => format!(
            "link:{}",
            std::fs::read_link(path)
                .map(|link| link.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        Ok(meta) if meta.is_dir() => "dir".to_string(),
        Ok(meta) if meta.is_file() => format!(
            "file:{}",
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim_end_matches('\n')
        ),
        Ok(_) => "other".to_string(),
    }
}

/// Skip-worktree flag for `rel` in the base repo at `home`.
fn skip_flag(home: &Path, rel: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(home)
        .args(["ls-files", "-v", "--", rel])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    let text = String::from_utf8_lossy(&output.stdout);
    if text.starts_with("S ") {
        "skip".to_string()
    } else if text.starts_with("H ") {
        "keep".to_string()
    } else {
        "none".to_string()
    }
}

/// One twin side: an ordinary base checkout at `$HOME` with the
/// rollback arrays, overlay records, and manifest paths.
struct Side {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    manifest: String,
    legacy: String,
}

impl Side {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("fixture home");
        git(&home, &["init", "-q"]);
        let home_text = home.to_string_lossy().into_owned();
        Side {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }

    fn home_text(&self) -> &str {
        &self.home_text
    }

    /// Commit `rel` with `body` in the base repo.
    fn track(&self, rel: &str, body: &[u8]) {
        stage(&self.home, rel, body);
        git(&self.home, &["add", "--", rel]);
        git(
            &self.home,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "track",
            ],
        );
    }

    /// Rust inputs mirroring the shell preamble.
    fn inputs<'a>(
        &'a self,
        base: &'a Base,
        dest: &'a DestinationInputs,
        rels: &'a [String],
        targets: &'a [String],
        overlays: &'a [String],
        tool: &'a dot::temp::MoveTool,
    ) -> RestoreInstalledInputs<'a> {
        RestoreInstalledInputs {
            base,
            home: self.home_text(),
            rels,
            targets,
            overlays,
            dest,
            manifest: &self.manifest,
            legacy_manifest: &self.legacy,
            euid: dot::temp::current_uid().expect("uid"),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            tmp: &self.home,
            tool,
        }
    }
}

/// Base checkout plus destination inputs for one side, with the
/// overlay link paths extracted from the active records.
fn base_and_dest(side: &Side, overlays: &[String]) -> (Base, DestinationInputs) {
    let home = side.home_text().to_owned();
    let overlay_paths: Vec<String> = overlays
        .iter()
        .map(|entry| dot::repos_base::overlay_path_sync(entry).0)
        .collect();
    (
        Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home.clone(),
        },
        DestinationInputs {
            pwd: home.clone(),
            home,
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths,
            init_backup: None,
        },
    )
}

/// Run one restore row and pin its result plus durable filesystem state.
#[allow(clippy::too_many_arguments)]
fn check_row(
    tag: &str,
    rels: &[&str],
    targets: &dyn Fn(&Side) -> Vec<String>,
    overlays: &dyn Fn(&Side) -> Vec<String>,
    setup: &dyn Fn(&Side),
    want_ok: bool,
) {
    let side = Side::build(tag);
    setup(&side);
    let targets_owned = targets(&side);
    let overlays_owned = overlays(&side);
    let (base, dest) = base_and_dest(&side, &overlays_owned);
    let rels_owned: Vec<String> = rels.iter().map(|rel| rel.to_string()).collect();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let inputs = side.inputs(
        &base,
        &dest,
        &rels_owned,
        &targets_owned,
        &overlays_owned,
        &tool,
    );
    let ok = repos_overlays::restore_installed_links(&inputs);
    assert_eq!(ok, want_ok, "{tag}");
    let states: Vec<String> = rels
        .iter()
        .map(|rel| dst_state(&side.home.join(rel)))
        .collect();
    match tag {
        "length" => assert_eq!(states, ["absent"]),
        "tracked-link" | "untracked-link" => assert_eq!(states, ["link:real.txt"]),
        "missing-tracked" => assert_eq!(states, [format!("link:{}/real.txt", side.home_text())]),
        "dir-blocks" => assert_eq!(states, ["dir"]),
        "fallback" | "lost-link" => assert_eq!(states, ["link:.dotfiles-o/home/owned.txt"]),
        "clean-file" => assert_eq!(states, ["file:keep"]),
        "wrong-link" => assert_eq!(states, ["link:/elsewhere"]),
        "fallback-fast" => assert_eq!(
            states,
            [format!("link:{}/overlay/home/owned.txt", side.home_text())]
        ),
        "sticky" => assert_eq!(
            states,
            [format!("link:{}/real.txt", side.home_text()), "dir".into()]
        ),
        _ => unreachable!(),
    }
}

/// Correct link at an available relative target, tracked: rc 0
/// with the skip-worktree bit set.
fn tracked_link_setup(side: &Side) {
    stage(&side.home, "real.txt", b"real\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("real.txt", side.home.join("owned.txt")).expect("link");
    git(&side.home, &["add", "--", "real.txt", "owned.txt"]);
    git(
        &side.home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "track",
        ],
    );
}

#[test]
fn restore_rejects_length_mismatch() {
    check_row(
        "length",
        &["a"],
        &|_| vec!["x".to_string(), "y".to_string()],
        &|_| vec![],
        &|_| {},
        false,
    );
}

#[test]
fn restore_keeps_correct_tracked_link() {
    check_row(
        "tracked-link",
        &["owned.txt"],
        &|_| vec!["real.txt".to_string()],
        &|_| vec![],
        &tracked_link_setup,
        true,
    );
}

#[test]
fn restore_keeps_correct_untracked_link() {
    check_row(
        "untracked-link",
        &["owned.txt"],
        &|_| vec!["real.txt".to_string()],
        &|_| vec![],
        &|side| {
            stage(&side.home, "real.txt", b"real\n");
            #[cfg(unix)]
            std::os::unix::fs::symlink("real.txt", side.home.join("owned.txt")).expect("link");
        },
        true,
    );
}

#[test]
fn restore_publishes_missing_tracked_link() {
    check_row(
        "missing-tracked",
        &["owned.txt"],
        &|side| vec![side.home.join("real.txt").to_string_lossy().into_owned()],
        &|_| vec![],
        &|side| {
            let body = b"real\n";
            stage(&side.home, "real.txt", body);
            side.track("owned.txt", body);
            std::fs::remove_file(side.home.join("owned.txt")).expect("remove dst");
        },
        true,
    );
}

#[test]
fn restore_rejects_directory_destination() {
    check_row(
        "dir-blocks",
        &["owned.txt"],
        &|side| vec![side.home.join("real.txt").to_string_lossy().into_owned()],
        &|_| vec![],
        &|side| {
            stage(&side.home, "real.txt", b"real\n");
            std::fs::create_dir_all(side.home.join("owned.txt")).expect("dir dst");
        },
        false,
    );
}

/// Overlay checkout shipping `home/owned.txt` for fallback rows.
fn fallback_overlays(side: &Side) -> Vec<String> {
    let checkout = side.home.join("overlay");
    stage(&checkout, "home/owned.txt", b"shipped\n");
    vec![format!(
        "o|{}|https://example.invalid/x|git||git",
        checkout.to_string_lossy()
    )]
}

#[test]
fn restore_publishes_fallback_link() {
    check_row(
        "fallback",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &fallback_overlays,
        &|_| {},
        true,
    );
}

#[test]
fn restore_keeps_clean_tracked_file_without_fallback() {
    check_row(
        "clean-file",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &|_| vec![],
        &|side| {
            side.track("owned.txt", b"keep\n");
        },
        true,
    );
}

#[test]
fn restore_reclaims_the_exact_lost_overlay_link_for_the_tracked_base() {
    let side = Side::build("lost-overlay-to-base");
    side.track("owned.txt", b"base destination\n");
    git(
        &side.home,
        &["update-index", "--skip-worktree", "owned.txt"],
    );
    std::fs::remove_file(side.home.join("owned.txt")).unwrap();
    std::os::unix::fs::symlink("missing-overlay.txt", side.home.join("owned.txt")).unwrap();

    let rels = vec!["owned.txt".to_string()];
    let targets = vec!["missing-overlay.txt".to_string()];
    let overlays = Vec::new();
    let (base, dest) = base_and_dest(&side, &overlays);
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    let inputs = side.inputs(&base, &dest, &rels, &targets, &overlays, &tool);

    assert!(repos_overlays::restore_installed_links(&inputs));
    let restored = side.home.join("owned.txt");
    assert!(
        !std::fs::symlink_metadata(&restored)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&restored).unwrap(), b"base destination\n");
    assert_eq!(skip_flag(&side.home, "owned.txt"), "keep");
}

#[test]
fn restore_rejects_wrong_link_without_fallback() {
    check_row(
        "wrong-link",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &|_| vec![],
        &|side| {
            #[cfg(unix)]
            std::os::unix::fs::symlink("/elsewhere", side.home.join("owned.txt")).expect("link");
        },
        false,
    );
}

#[test]
fn restore_republishes_dangling_lost_link_with_fallback() {
    // The live link still names the lost target while a fallback
    // ships: the link's own fingerprint pins the replacement, so
    // the fallback publishes on both sides.
    check_row(
        "lost-link",
        &["owned.txt"],
        &|_| vec!["elsewhere.txt".to_string()],
        &fallback_overlays,
        &|side| {
            #[cfg(unix)]
            std::os::unix::fs::symlink("elsewhere.txt", side.home.join("owned.txt")).expect("link");
        },
        true,
    );
}

#[test]
fn restore_takes_available_fallback_fast_path() {
    // A none-synced overlay's absolute target is available, so a
    // live link to it confirms in place.
    check_row(
        "fallback-fast",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &|side| {
            let checkout = side.home.join("overlay");
            stage(&checkout, "home/owned.txt", b"shipped\n");
            vec![format!(
                "o|{}|https://example.invalid/x|git||none",
                checkout.to_string_lossy()
            )]
        },
        &|side| {
            let shipped = side.home.join("overlay/home/owned.txt");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&shipped, side.home.join("owned.txt")).expect("link");
        },
        true,
    );
}

#[test]
fn restore_applies_good_records_despite_bad_ones() {
    // The walk is sticky: an early publication stands even when a
    // later record fails.
    check_row(
        "sticky",
        &["fresh.txt", "blocked.txt"],
        &|side| {
            vec![
                side.home.join("real.txt").to_string_lossy().into_owned(),
                side.home.join("real.txt").to_string_lossy().into_owned(),
            ]
        },
        &|_| vec![],
        &|side| {
            stage(&side.home, "real.txt", b"real\n");
            std::fs::create_dir_all(side.home.join("blocked.txt")).expect("dir dst");
        },
        false,
    );
}
