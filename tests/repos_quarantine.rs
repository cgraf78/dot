//! Native contracts for resolving and quarantining managed overlay links.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use dot::repos_overlays::{self, QuarantineOutcome};
use dot_test_support::TempDir;

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn context(root: &Path, configured: bool) -> repos_overlays::DestinationInputs {
    let home = root.to_string_lossy().into_owned();
    repos_overlays::DestinationInputs {
        pwd: home.clone(),
        home: home.clone(),
        xdg_state_home: Some(format!("{home}/xdg-state")),
        install_dir: Some(format!("{home}/install")),
        state_dir: Some(format!("{home}/shdeps")),
        overlay_paths: if configured {
            vec![format!("{home}/ov")]
        } else {
            Vec::new()
        },
        init_backup: configured.then(|| format!("{home}/backup")),
    }
}

fn state(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Err(_) => "absent".into(),
        Ok(meta) if meta.file_type().is_symlink() => {
            format!("link:{}", std::fs::read_link(path).unwrap().display())
        }
        Ok(meta) if meta.is_file() => "file".into(),
        Ok(_) => "other".into(),
    }
}

#[test]
fn destination_context_resolves_physical_paths_and_rejects_reserved_destinations() {
    for (relative, expected) in [
        ("sub/anchor", true),
        ("nodir/deep/x", false),
        (".dotfiles-evil/x", false),
    ] {
        let scope = TempDir::new("quarantine-context").unwrap();
        let root = scope.path();
        stage(root, "sub/anchor-target", b"target\n");
        std::os::unix::fs::symlink("anchor-target", root.join("sub/anchor")).unwrap();
        let result = repos_overlays::destination_context(relative, &context(root, false));
        assert_eq!(result.is_some(), expected, "{relative}");
        if let Some(resolved) = result {
            let physical = root.join(relative);
            assert_eq!(resolved.physical, physical, "physical {relative}");
            assert_eq!(
                resolved.parent,
                physical.parent().unwrap(),
                "parent {relative}"
            );
            let parent = std::fs::symlink_metadata(physical.parent().unwrap()).unwrap();
            assert_eq!(
                resolved.parent_identity,
                format!("{}:{}", parent.dev(), parent.ino())
            );
        }
    }
}

fn inputs(
    root: &Path,
    paths: Vec<String>,
    targets: Vec<String>,
    configured: bool,
    tool: &dot::temp::MoveTool,
) -> repos_overlays::QuarantineInputs {
    repos_overlays::QuarantineInputs {
        snapshot: repos_overlays::RollbackSnapshot { paths, targets },
        context: context(root, configured),
        tool: tool.clone(),
        source_root: root.to_path_buf(),
    }
}

#[test]
fn quarantine_rollback_link_preserves_the_named_outcome_and_filesystem_contract() {
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    for case in [
        "happy",
        "unknown-rel",
        "ragged-snapshot",
        "regular-file",
        "wrong-target",
        "reserved-rel",
        "configured-env",
    ] {
        let scope = TempDir::new("quarantine-link").unwrap();
        let root = scope.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let target = stage(root, "target.txt", b"managed\n")
            .to_string_lossy()
            .into_owned();
        let mut relative = "sub/anchor".to_string();
        std::os::unix::fs::symlink(&target, root.join(&relative)).unwrap();
        let mut paths = vec![relative.clone()];
        let mut targets = vec![target.clone()];
        match case {
            "unknown-rel" => paths = vec!["other/path".into()],
            "ragged-snapshot" => targets.clear(),
            "regular-file" => {
                std::fs::remove_file(root.join(&relative)).unwrap();
                stage(root, &relative, b"user file\n");
            }
            "wrong-target" => {
                std::fs::remove_file(root.join(&relative)).unwrap();
                std::os::unix::fs::symlink("elsewhere", root.join(&relative)).unwrap();
            }
            "reserved-rel" => {
                relative = ".dotfiles-evil/x".into();
                paths = vec![relative.clone()];
                std::fs::create_dir_all(root.join(".dotfiles-evil")).unwrap();
                std::os::unix::fs::symlink(&target, root.join(&relative)).unwrap();
            }
            _ => {}
        }
        let physical = root.join(&relative);
        let before = state(&physical);
        let result = repos_overlays::quarantine_rollback_link(
            &relative,
            &inputs(root, paths, targets, case == "configured-env", &tool),
        );
        match case {
            "happy" | "configured-env" => match result {
                QuarantineOutcome::Adopt(adoption) => {
                    assert_eq!(adoption.physical, physical);
                    assert_eq!(state(&physical), "absent");
                    assert_eq!(adoption.parked, adoption.stage.join("previous"));
                    assert_eq!(
                        std::fs::read_link(&adoption.parked).unwrap(),
                        PathBuf::from(&target)
                    );
                    assert_eq!(
                        repos_overlays::replacement_identity(root, &adoption.parked).unwrap(),
                        adoption.expected
                    );
                    assert_eq!(
                        std::fs::metadata(&adoption.stage)
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o700
                    );
                    assert!(
                        adoption
                            .stage
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".anchor.dot-overlay-adopt.")
                    );
                }
                other => panic!("{case}: expected adoption, got {other:?}"),
            },
            "reserved-rel" => {
                assert_eq!(result, QuarantineOutcome::Unsafe);
                assert_eq!(state(&physical), before);
            }
            _ => {
                assert_eq!(result, QuarantineOutcome::NotManaged, "{case}");
                assert_eq!(state(&physical), before, "{case}");
            }
        }
        let parent = physical.parent().unwrap();
        if !matches!(case, "happy" | "configured-env") {
            assert!(
                std::fs::read_dir(parent)
                    .unwrap()
                    .flatten()
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .contains(".dot-overlay-adopt.")),
                "stage leaked for {case}"
            );
        }
    }
}
