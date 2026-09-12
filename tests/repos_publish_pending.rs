//! Native contracts for authority manifests and pending publication.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use dot::repos_overlays;
use dot_test_support::TempDir;

fn stage(root: &Path, relative: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn record(relative: &str, owner: &str, target: &str) -> Vec<u8> {
    format!("{relative}\t{owner}\t{target}\n").into_bytes()
}

fn overlay(name: &str, path: &Path, sync: &str) -> String {
    format!("{name}|{}|url|conf|false|{sync}", path.display())
}

struct Context {
    home: String,
    manifest: String,
    legacy: String,
    inputs: repos_overlays::DestinationInputs,
}

impl Context {
    fn new(root: &Path) -> Self {
        let home = root.to_string_lossy().into_owned();
        Self {
            manifest: format!("{home}/manifest.tsv"),
            legacy: format!("{home}/legacy.tsv"),
            inputs: repos_overlays::DestinationInputs {
                pwd: home.clone(),
                home: home.clone(),
                xdg_state_home: None,
                install_dir: None,
                state_dir: None,
                overlay_paths: Vec::new(),
                init_backup: None,
            },
            home,
        }
    }

    fn with<T>(&self, run: impl FnOnce(&mut repos_overlays::AuthorityCtx<'_>) -> T) -> T {
        let mut cache = repos_overlays::AuthorityCache::disabled();
        let mut context = repos_overlays::AuthorityCtx {
            home: &self.home,
            manifest: &self.manifest,
            legacy_manifest: &self.legacy,
            inputs: &self.inputs,
            roots: None,
            cache: &mut cache,
            euid: dot::temp::current_uid().unwrap(),
        };
        run(&mut context)
    }
}

#[test]
fn authority_file_selection_pins_order_deduplication_and_unsafe_precedence() {
    for case in [
        "none",
        "selected",
        "legacy",
        "pending",
        "unsafe-selected",
        "unsafe-pending",
        "dedupe",
    ] {
        let scope = TempDir::new("pending-authority-files").unwrap();
        let root = scope.path();
        let context = Context::new(root);
        match case {
            "selected" => {
                stage(
                    root,
                    "manifest.tsv",
                    &record("app", "base", "target"),
                    0o600,
                );
            }
            "legacy" => {
                stage(root, "legacy.tsv", b"old\tbase\n", 0o600);
            }
            "pending" => {
                stage(
                    root,
                    "manifest.tsv.pending",
                    &record("app", "base", "target"),
                    0o600,
                );
            }
            "unsafe-selected" => {
                stage(
                    root,
                    "manifest.tsv",
                    &record("app", "base", "target"),
                    0o644,
                );
            }
            "unsafe-pending" => {
                stage(
                    root,
                    "manifest.tsv",
                    &record("app", "base", "target"),
                    0o600,
                );
                stage(root, "manifest.tsv.pending", b"junk\n", 0o600);
            }
            "dedupe" => {
                context_manifest_alias(root);
            }
            _ => {}
        }
        let result = repos_overlays::authority_files(
            &context.manifest,
            if case == "dedupe" {
                &context.manifest
            } else {
                &context.legacy
            },
            dot::temp::current_uid().unwrap(),
        );
        match case {
            "none" => assert_eq!(result.unwrap().manifests, Vec::<String>::new()),
            "selected" => assert_eq!(result.unwrap().manifests, vec![context.manifest]),
            "legacy" => assert_eq!(result.unwrap().manifests, vec![context.legacy]),
            "pending" => assert_eq!(
                result.unwrap().manifests,
                vec![format!("{}.pending", context.manifest)]
            ),
            "unsafe-selected" => assert_eq!(result.unwrap_err(), context.manifest),
            "unsafe-pending" => {
                assert_eq!(result.unwrap_err(), format!("{}.pending", context.manifest))
            }
            "dedupe" => assert_eq!(result.unwrap().manifests, vec![context.manifest]),
            _ => unreachable!(),
        }
    }
}

fn context_manifest_alias(root: &Path) {
    stage(
        root,
        "manifest.tsv",
        &record("app", "base", "target"),
        0o600,
    );
}

#[test]
fn load_authority_unions_manifests_and_skips_control_plane_paths() {
    let scope = TempDir::new("pending-load-authority").unwrap();
    let context = Context::new(scope.path());
    let body = [
        record("app.conf", "base", "target-a"),
        record("manifest.tsv", "base", "target-control"),
    ]
    .concat();
    stage(scope.path(), "manifest.tsv", &body, 0o600);
    stage(
        scope.path(),
        "legacy.tsv",
        &record("other.conf", "old", "target-b"),
        0o600,
    );
    let data = context.with(repos_overlays::load_authority).unwrap();
    assert_eq!(data.paths.len(), 2);
    assert!(data.paths.contains("app.conf"));
    assert!(data.paths.contains("other.conf"));
    assert!(!data.paths.contains("manifest.tsv"));
    assert!(
        data.targets
            .contains(&("app.conf".into(), "target-a".into()))
    );
    assert!(
        data.targets
            .contains(&("other.conf".into(), "target-b".into()))
    );
}

#[test]
fn append_manifest_records_derives_targets_skips_authority_and_stops_after_bad_rows() {
    for case in ["normal", "authority", "bad-tail", "missing", "empty"] {
        let scope = TempDir::new("pending-append-records").unwrap();
        let root = scope.path();
        let context = Context::new(root);
        let source = root.join("source");
        match case {
            "normal" => {
                stage(root, "source", b"a\tweb\nb\tweb\ttarget-b\n", 0o600);
            }
            "authority" => {
                stage(root, "source", b"manifest.tsv\tweb\na\tweb\n", 0o600);
            }
            "bad-tail" => {
                stage(root, "source", b"a\tweb\njunk\n", 0o600);
            }
            "empty" => {
                stage(root, "source", b"", 0o600);
            }
            _ => {}
        }
        let destination = root.join("destination");
        let ok =
            context.with(|ctx| repos_overlays::append_manifest_records(&source, &destination, ctx));
        let expected = match case {
            "normal" => b"a\tweb\t.dotfiles-web/home/a\nb\tweb\ttarget-b\n".as_slice(),
            "authority" | "bad-tail" => b"a\tweb\t.dotfiles-web/home/a\n",
            _ => b"",
        };
        assert_eq!(ok, !matches!(case, "bad-tail" | "missing"));
        assert_eq!(std::fs::read(&destination).unwrap_or_default(), expected);
        assert_eq!(destination.exists(), !matches!(case, "missing" | "empty"));
    }
}

#[test]
fn append_candidates_pins_nul_boundaries_sync_targets_and_inventory_guards() {
    for case in [
        "git",
        "local",
        "empty-record",
        "unterminated",
        "authority",
        "bad-sync",
        "missing",
        "symlink",
    ] {
        let scope = TempDir::new("pending-append-candidates").unwrap();
        let root = scope.path();
        let context = Context::new(root);
        let overlay_root = root.join("overlay");
        stage(&overlay_root, "home/a", b"a", 0o600);
        let inventory = root.join("inventory");
        match case {
            "git" | "local" | "bad-sync" => {
                stage(
                    root,
                    "inventory",
                    format!("{}/home/a\0", overlay_root.display()).as_bytes(),
                    0o600,
                );
            }
            "empty-record" => {
                stage(root, "inventory", b"\0", 0o600);
            }
            "unterminated" => {
                stage(
                    root,
                    "inventory",
                    format!("{}/home/a", overlay_root.display()).as_bytes(),
                    0o600,
                );
            }
            "authority" => {
                stage(
                    root,
                    "inventory",
                    format!("{}/home/manifest.tsv\0", overlay_root.display()).as_bytes(),
                    0o600,
                );
            }
            "symlink" => {
                stage(root, "real-inventory", b"x\0", 0o600);
                std::os::unix::fs::symlink("real-inventory", &inventory).unwrap();
            }
            _ => {}
        }
        let destination = root.join("destination");
        let sync = match case {
            "local" => "none",
            "bad-sync" => "bogus",
            _ => "git",
        };
        let ok = context.with(|ctx| {
            repos_overlays::append_candidates(
                &destination,
                "web",
                &overlay_root.to_string_lossy(),
                &inventory,
                Some(sync),
                ctx,
            )
        });
        assert_eq!(
            ok,
            !matches!(
                case,
                "empty-record" | "authority" | "bad-sync" | "missing" | "symlink"
            )
        );
        let expected = match case {
            "git" => b"a\tweb\t.dotfiles-web/home/a\n".to_vec(),
            "local" => format!("a\tweb\t{}/home/a\n", overlay_root.display()).into_bytes(),
            _ => Vec::new(),
        };
        assert_eq!(
            std::fs::read(&destination).unwrap_or_default(),
            expected,
            "{case}"
        );
    }
}

#[test]
fn publish_pending_is_private_atomic_and_preserves_or_rejects_existing_authority() {
    for case in ["fresh", "replace", "unsafe", "untracked", "candidate-fails"] {
        let scope = TempDir::new("pending-publish").unwrap();
        let root = scope.path();
        let context = Context::new(root);
        let selected = if case == "replace" {
            record("old", "base", "old-target")
        } else {
            record("app", "base", "base-target")
        };
        stage(
            root,
            "manifest.tsv",
            &selected,
            if case == "unsafe" { 0o644 } else { 0o600 },
        );
        if case == "replace" {
            stage(
                root,
                "manifest.tsv.pending",
                &record("stale", "base", "stale-target"),
                0o600,
            );
        }
        let overlay_root = root.join("overlay");
        stage(&overlay_root, "home/new", b"new", 0o600);
        let inventory = stage(
            root,
            "inventory",
            format!("{}/home/new\0", overlay_root.display()).as_bytes(),
            0o600,
        );
        let sync = if case == "candidate-fails" {
            "bogus"
        } else {
            "none"
        };
        let overlays = vec![overlay("web", &overlay_root, sync)];
        let inventories = if case == "untracked" {
            HashMap::new()
        } else {
            HashMap::from([("web".into(), inventory)])
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().unwrap();
        let result = context.with(|ctx| {
            repos_overlays::publish_pending(
                ctx,
                dot::temp::current_uid().unwrap(),
                &overlays,
                &inventories,
                &tool,
            )
        });
        let success = matches!(case, "fresh" | "replace" | "untracked");
        assert_eq!(result.is_some(), success, "{case}");
        let pending = PathBuf::from(format!("{}.pending", context.manifest));
        if success {
            assert_eq!(result.unwrap(), pending.to_string_lossy());
            assert_eq!(
                std::fs::metadata(&pending).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let body = std::fs::read_to_string(&pending).unwrap();
            assert!(body.contains("\tbase\t"));
            assert_eq!(body.contains("\tweb\t"), case != "untracked");
            assert_eq!(
                body.contains("stale\tbase\tstale-target"),
                case == "replace"
            );
        } else {
            assert!(!pending.exists());
        }
        assert!(
            std::fs::read_dir(root)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp."))
        );
    }
}

#[test]
fn fallback_publication_is_idempotent_and_active_selection_is_last_publishable_nonexcluded() {
    let scope = TempDir::new("pending-fallback").unwrap();
    let root = scope.path();
    let context = Context::new(root);
    stage(root, "manifest.tsv", &record("app", "web", "target"), 0o600);
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().unwrap();
    assert!(
        context.with(|ctx| repos_overlays::publish_fallback_authority(
            "app",
            "web",
            "target",
            ctx,
            dot::temp::current_uid().unwrap(),
            &tool
        ))
    );
    assert!(
        !Path::new(&format!("{}.pending", context.manifest)).exists(),
        "exact hit writes nothing"
    );
    assert!(
        context.with(|ctx| repos_overlays::publish_fallback_authority(
            "other",
            "web",
            "other-target",
            ctx,
            dot::temp::current_uid().unwrap(),
            &tool
        ))
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}.pending", context.manifest)).unwrap(),
        "app\tweb\ttarget\nother\tweb\tother-target\n"
    );

    let alpha = root.join("alpha");
    let beta = root.join("beta");
    let bad = root.join("bad");
    stage(&alpha, "home/app", b"a", 0o600);
    stage(&beta, "home/app", b"b", 0o600);
    stage(&bad, "home/app", b"x", 0o600);
    let overlays = vec![
        overlay("alpha", &alpha, "none"),
        overlay("beta", &beta, "none"),
        overlay("bad", &bad, "bogus"),
    ];
    let alpha_target = format!("{}/home/app", alpha.display());
    let beta_target = format!("{}/home/app", beta.display());
    assert_eq!(
        repos_overlays::active_fallback_target("app", "", &overlays),
        Some((beta_target.clone(), "beta".into()))
    );
    assert_eq!(
        repos_overlays::active_fallback_target("app", &beta_target, &overlays),
        Some((alpha_target, "alpha".into()))
    );
    assert_eq!(
        repos_overlays::active_fallback_target("missing", "", &overlays),
        None
    );
    assert_eq!(
        repos_overlays::active_fallback_target("app", "", &[overlay("bad", &bad, "bogus")]),
        None
    );
}
