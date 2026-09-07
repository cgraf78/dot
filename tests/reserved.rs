//! Native integration contracts for reserved control-plane paths.

use std::path::PathBuf;

use dot::reserved::{
    Error, RootsInput, candidate_path_is_reserved_from_roots, path_is_reserved_from_roots,
    physical_directory_candidate, physical_leaf_candidate, reserved_roots,
};
use dot_test_support::TempDir;

struct Fixture {
    _scope: TempDir,
    root: PathBuf,
    home: String,
    input: RootsInput,
}

impl Fixture {
    fn new() -> Self {
        let scope = TempDir::new("reserved-native").expect("temp dir");
        let root = scope.path().to_path_buf();
        let home_path = root.join("home");
        let home = home_path.to_string_lossy().into_owned();
        for path in [
            home_path.join("real-dotfiles"),
            home_path.join(".local/state/dot"),
            home_path.join(".local/state/shdeps"),
            home_path.join(".local/share/cgraf78"),
            home_path.join("overlays/one"),
            home_path.join("backup"),
        ] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        std::os::unix::fs::symlink(home_path.join("real-dotfiles"), home_path.join(".dotfiles"))
            .expect("client alias");
        std::os::unix::fs::symlink(
            home_path.join("overlays/missing"),
            home_path.join("overlays/dangling"),
        )
        .expect("dangling overlay");
        let input = RootsInput {
            home: home.clone(),
            state_home: format!("{home}/.local/state"),
            install_root: format!("{home}/.local/share"),
            provider_state: format!("{home}/.local/state/shdeps"),
            overlay_paths: vec![
                format!("{home}/overlays/one"),
                format!("{home}/overlays/dangling"),
            ],
            init_backup: Some(format!("{home}/backup")),
        };
        Self {
            _scope: scope,
            root,
            home,
            input,
        }
    }

    fn checkout(&self) -> String {
        format!("{}/.local/share/cgraf78/dot", self.home)
    }
}

#[test]
fn roots_inventory_preserves_order_and_physical_aliases() {
    let fixture = Fixture::new();
    let roots = reserved_roots(&fixture.input, &fixture.home).expect("roots");
    let expected_prefix = [
        format!("{}/.local/state/dot", fixture.home),
        format!("{}/.local/state/shdeps", fixture.home),
        format!("{}/.dotfiles", fixture.home),
        format!("{}/real-dotfiles", fixture.home),
        format!("{}/.dot-backup", fixture.home),
        format!("{}/.local/bin/.dot.dot-install-stage-v1", fixture.home),
        format!("{}/.local/lib/.dot.dot-install-stage-v1", fixture.home),
        format!("{}/.local/bin/dot", fixture.home),
        format!("{}/.local/lib/dot", fixture.home),
        format!("{}/.config/dot/profile-selectors.local.d", fixture.home),
        fixture.checkout(),
    ];
    assert_eq!(&roots[..expected_prefix.len()], expected_prefix.as_slice());
    assert_eq!(
        roots.last(),
        Some(&format!("{}/backup", fixture.home)),
        "configured backup is the final authority root"
    );
    assert!(roots.contains(&format!("{}/overlays/one", fixture.home)));
    assert!(roots.contains(&format!("{}/overlays/dangling", fixture.home)));
    assert!(roots.contains(&format!("{}/overlays/missing", fixture.home)));
}

#[test]
fn leaf_and_candidate_matrix_covers_roots_ancestors_transients_and_aliases() {
    let fixture = Fixture::new();
    let roots = reserved_roots(&fixture.input, &fixture.home).expect("roots");
    let checkout = fixture.checkout();
    let h = &fixture.home;
    let rows = [
        (format!("{h}/.local/state/dot/overlay-links"), true, true),
        (format!("{h}/.local/state/shdeps/cache"), true, true),
        (format!("{h}/.dotfiles/config"), true, true),
        (format!("{h}/real-dotfiles/config"), true, true),
        (format!("{h}/overlays/one/payload"), true, true),
        (format!("{h}/overlays/dangling/payload"), true, true),
        (format!("{h}/overlays/missing/payload"), true, true),
        (format!("{h}/backup/snapshot"), true, true),
        (format!("{h}/.local/bin/dot"), true, true),
        (format!("{h}/.local/share/cgraf78/.dot.clone.1"), true, true),
        (format!("{h}/.local/share/cgraf78/dot.tmp.2"), true, true),
        (format!("{h}/.dot-init-entry.1/file"), true, true),
        (format!("{h}/sub/.dot-init-parent.2"), true, true),
        (h.clone(), false, true),
        (format!("{h}/.local"), false, true),
        (format!("{h}/other/file"), false, false),
        (format!("{h}/.dotfiles-backup"), false, true),
    ];
    for (path, leaf, candidate) in rows {
        assert_eq!(
            path_is_reserved_from_roots(&path, &roots, h, &checkout),
            leaf,
            "leaf {path}"
        );
        assert_eq!(
            candidate_path_is_reserved_from_roots(&path, &roots, h, &checkout, &fixture.home),
            candidate,
            "candidate {path}"
        );
    }
}

#[test]
fn physical_resolution_preserves_missing_suffix_and_parent_identity() {
    let fixture = Fixture::new();
    let real = fixture.root.join("real-parent");
    std::fs::create_dir(&real).expect("real parent");
    let alias = fixture.root.join("alias-parent");
    std::os::unix::fs::symlink(&real, &alias).expect("parent alias");
    let candidate = alias.join("missing/deep/file");
    assert_eq!(
        physical_directory_candidate(&candidate.to_string_lossy(), &fixture.home),
        Ok(format!("{}/missing/deep/file", real.display()))
    );
    let leaf =
        physical_leaf_candidate(&alias.join("missing-leaf").to_string_lossy(), &fixture.home)
            .expect("leaf candidate");
    assert_eq!(leaf.path, format!("{}/missing-leaf", real.display()));
    assert_eq!(leaf.physical_parent, real.to_string_lossy());
    let mut identity = leaf.parent_identity.split(':');
    assert!(
        identity
            .next()
            .is_some_and(|part| part.parse::<u64>().is_ok())
    );
    assert!(
        identity
            .next()
            .is_some_and(|part| part.parse::<u64>().is_ok())
    );
    assert!(identity.next().is_none());
    assert_eq!(
        physical_directory_candidate("", &fixture.home),
        Err(Error::Usage)
    );
}
