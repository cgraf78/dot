//! Native behavioral coverage for linking one prepared overlay.

use dot::repos_link_exec::{self, Outcome, OverlayState};
use dot_test_support::TempDir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn stage(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}
struct Fixture {
    _scope: TempDir,
    root: PathBuf,
    home: PathBuf,
    overlay: PathBuf,
    manifest: PathBuf,
    draft: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let scope = TempDir::new("link-exec").unwrap();
        let root = scope.path().to_path_buf();
        let home = root.join("home");
        let overlay = root.join("overlay");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir_all(overlay.join("home")).unwrap();
        let manifest = root.join("manifest.tsv");
        let draft = root.join("draft.tsv");
        std::fs::write(&draft, b"").unwrap();
        Self {
            _scope: scope,
            root,
            home,
            overlay,
            manifest,
            draft,
        }
    }
    fn run(
        &self,
        inventory: &[PathBuf],
        sync: &str,
        verbose: bool,
    ) -> (Outcome, OverlayState, Vec<u8>, Vec<u8>) {
        let home = self.home.to_string_lossy().into_owned();
        let overlay = self.overlay.to_string_lossy().into_owned();
        let overlay_home = self.overlay.join("home").to_string_lossy().into_owned();
        let manifest = self.manifest.to_string_lossy().into_owned();
        let legacy = self.root.join("legacy.tsv").to_string_lossy().into_owned();
        let entries = vec![format!("ov|{overlay}||||{sync}")];
        let dest = dot::repos_overlays::DestinationInputs {
            home: home.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![overlay.clone()],
            init_backup: None,
            pwd: home.clone(),
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let palette = dot::progress_ui::Palette::empty();
        let reserved = dot::repos_overlays::reserved_snapshot_vec(
            &home,
            &dest,
            std::slice::from_ref(&overlay),
        )
        .unwrap()
        .join("\n");
        let bytes: Vec<u8> = inventory
            .iter()
            .flat_map(|path| {
                use std::os::unix::ffi::OsStrExt as _;
                let mut record = path.as_os_str().as_bytes().to_vec();
                record.push(0);
                record
            })
            .collect();
        let inputs = repos_link_exec::Inputs {
            name: "ov",
            path: &overlay,
            sync,
            home: &home,
            overlay_home: &overlay_home,
            overlays: &entries,
            dest: &dest,
            reserved_roots: Some(&reserved),
            authority_targets: &[],
            base: None,
            base_tracked: &HashSet::new(),
            manifest: &manifest,
            legacy_manifest: &legacy,
            manifest_new: &self.draft,
            source_root: None,
            source_identity: None,
            euid: dot::temp::current_uid().unwrap(),
            source_root_git: &self.root,
            tmp: &self.root,
            tool: &tool,
            palette: &palette,
            multibyte: false,
            dot_quiet: None,
            dot_verbose: verbose.then_some("1"),
            ui_total: None,
        };
        let mut state = OverlayState::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome =
            repos_link_exec::link_overlay(&inputs, &mut state, &bytes, &mut out, &mut err);
        (outcome, state, out, err)
    }
}

#[test]
fn links_nested_files_records_manifest_and_converges() {
    let f = Fixture::new();
    let a = stage(&f.overlay, "home/a.conf", b"a\n");
    let nested = stage(&f.overlay, "home/sub/b.conf", b"b\n");
    let (first, state, out, err) = f.run(&[a.clone(), nested.clone()], "git", false);
    assert_eq!(first, Outcome::Changed("ov overlay linked 2".into()));
    assert_eq!(
        state.current,
        HashSet::from(["a.conf".into(), "sub/b.conf".into()])
    );
    assert!(err.is_empty());
    assert!(String::from_utf8(out).unwrap().contains("linked:"));
    assert_eq!(
        std::fs::read_link(f.home.join("a.conf")).unwrap(),
        PathBuf::from(".dotfiles-ov/home/a.conf")
    );
    assert_eq!(
        std::fs::read_link(f.home.join("sub/b.conf")).unwrap(),
        PathBuf::from("../.dotfiles-ov/home/sub/b.conf")
    );
    assert!(
        std::fs::read_to_string(&f.draft)
            .unwrap()
            .contains("a.conf\tov\t.dotfiles-ov/home/a.conf")
    );
    let (second, state, out, err) = f.run(&[a, nested], "git", false);
    assert_eq!(second, Outcome::Current("ov overlay current".into()));
    assert_eq!(state.current.len(), 2);
    assert!(out.is_empty());
    assert!(err.is_empty());
}

#[test]
fn refuses_reserved_paths() {
    let f = Fixture::new();
    let source = stage(
        &f.overlay,
        "home/.local/share/cgraf78/dot/owned",
        b"unsafe\n",
    );
    let (outcome, state, _, err) = f.run(&[source], "git", false);
    assert_eq!(outcome, Outcome::Failed);
    assert!(state.current.is_empty());
    assert!(!f.home.join(".local/share/cgraf78/dot/owned").exists());
    assert!(String::from_utf8(err).unwrap().contains("reserved path"));
}

#[test]
fn preserves_unmanaged_destination_objects() {
    for (kind, warning) in [
        ("file", "untracked file"),
        ("directory", "directory in the way"),
    ] {
        let f = Fixture::new();
        let source = stage(&f.overlay, "home/a.conf", b"overlay\n");
        match kind {
            "file" => {
                stage(&f.home, "a.conf", b"mine\n");
            }
            _ => std::fs::create_dir(f.home.join("a.conf")).unwrap(),
        }
        let (outcome, state, _, err) = f.run(&[source], "git", false);
        assert_eq!(
            outcome,
            Outcome::Current("ov overlay current".into()),
            "{kind}"
        );
        assert!(state.current.is_empty(), "{kind}");
        assert!(String::from_utf8(err).unwrap().contains(warning), "{kind}");
    }
}

#[test]
fn verbose_mode_emits_running_and_changed_status() {
    let f = Fixture::new();
    let source = stage(&f.overlay, "home/a.conf", b"a\n");
    let (outcome, _, out, err) = f.run(&[source], "git", true);
    assert!(matches!(outcome, Outcome::Changed(_)));
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("running"));
    assert!(text.contains("changed"));
    assert!(err.is_empty());
}
