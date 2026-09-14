//! Native worker boundary contracts plus public hook-runtime compatibility.

use dot::extension_worker::{self as worker, Error, Mode};
use dot_test_support::TempDir;
use std::process::Command;

fn inventory_names(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("read public API inventory");
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            line.split('\t')
                .next()
                .expect("inventory function column")
                .to_owned()
        })
        .collect()
}

#[test]
fn protocol_whitelist_and_survivors_are_exact() {
    for name in worker::OVERLAY_PROTOCOL_KEEP {
        assert!(worker::overlay_protocol_keep(name));
    }
    for name in [
        "merge",
        "doctor",
        "_overlay_private_helper",
        "",
        "_overlay_link_target_extra",
    ] {
        assert!(!worker::overlay_protocol_keep(name));
    }
    let before = vec!["existing".into()];
    let after = vec![
        "new".into(),
        "existing".into(),
        "_overlay_link_target".into(),
    ];
    assert_eq!(
        worker::protocol_survivors(&before, &after),
        vec!["existing", "_overlay_link_target"]
    );
}

#[test]
fn public_hook_runtime_remains_sourceable_without_private_engine_files() {
    let root = env!("CARGO_MANIFEST_DIR");
    for path in [
        "lib/dot/public/xdg.sh",
        "lib/dot/public/ui.sh",
        "lib/dot/public/api-version.sh",
        "lib/dot/public/test-reporter-v1",
        "lib/dot/public/test-timeout-v1",
    ] {
        assert!(std::path::Path::new(root).join(path).is_file(), "{path}");
    }
    let runtime = std::path::Path::new(root).join("lib/dot/public/hook-runtime-v1");
    assert!(runtime.is_dir());
    for path in [
        "hook-api.sh",
        "doctor-api.sh",
        "extension-trust.sh",
        "temp.sh",
    ] {
        assert!(runtime.join(path).is_file(), "{path}");
    }
    let status = Command::new("sh")
        .arg("-c")
        .arg(". \"$1/lib/dot/public/xdg.sh\"; . \"$1/lib/dot/public/ui.sh\"")
        .arg("dot-public-test")
        .arg(root)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn public_extension_inventories_and_documentation_pin_the_literal_contract() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(
        inventory_names(&root.join("lib/dot/public/hook-api-v1.tsv")),
        [
            "dot_hook_file",
            "dot_hook_source",
            "dot_hook_family",
            "dot_hook_family_files",
            "dot_hook_family_files_matching",
            "dot_hook_family_relpath",
            "dot_hook_family_marker_name",
            "dot_family_files",
            "dot_family_files_matching",
            "dot_family_relpath",
            "dot_expand_home",
            "dot_sibling_tmp_for",
            "dot_write_text_if_changed",
            "dot_commit_tmp",
            "dot_file_generation",
            "dot_commit_tmp_if_generation",
            "dot_remove_if_generation",
            "dot_json_available",
            "dot_json_layer",
            "dot_managed_block_build",
            "dot_managed_block_strip",
            "dot_managed_block_strip_family",
            "dot_managed_block_merge",
            "dot_managed_block_merge_family",
            "dot_xdg_path",
            "dot_tool_present",
            "dot_hook_platform_match",
            "dot_hook_host_match",
            "dot_hook_log",
            "dot_hook_warn",
        ]
    );
    assert_eq!(
        inventory_names(&root.join("lib/dot/public/doctor-api-v1.tsv")),
        [
            "dot_doctor_section",
            "dot_doctor_ok",
            "dot_doctor_warn",
            "dot_doctor_fail",
            "dot_doctor_skip",
            "dot_doctor_display_path",
            "dot_doctor_source",
        ]
    );

    let docs = std::fs::read_to_string(root.join("docs/extensions.md"))
        .expect("read extension documentation");
    for link in [
        "(../lib/dot/public/hook-api-v1.tsv)",
        "(../lib/dot/public/doctor-api-v1.tsv)",
        "(../lib/dot/public/test-api-v1.tsv)",
    ] {
        assert!(docs.contains(link), "missing normative API link: {link}");
    }
}

#[test]
fn modes_map_to_fixed_entry_points() {
    for (text, mode, entry) in [
        ("merge", Mode::Merge, "merge"),
        ("pre-sync", Mode::PreSync, "prepare"),
        ("deactivate", Mode::Deactivate, "deactivate"),
        ("doctor", Mode::Doctor, "doctor"),
    ] {
        assert_eq!(Mode::parse(text), Some(mode));
        assert_eq!(mode.as_str(), text);
        assert_eq!(mode.entry_point(), entry);
    }
    for bad in ["", "prepare", "pre_sync", "Doctor", "unknown"] {
        assert_eq!(Mode::parse(bad), None);
    }
}

#[test]
fn source_root_and_result_validation_reject_links_relative_and_empty() {
    let dir = TempDir::new("worker-root").unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("lib/dot")).unwrap();
    let root_s = root.to_string_lossy();
    assert!(worker::source_root_shape_ok(&root_s));
    assert!(worker::lib_dot_dir_ok(&root_s));
    assert!(worker::source_root_valid(&root_s));
    assert!(!worker::source_root_shape_ok("relative"));
    assert!(!worker::source_root_valid(""));
    assert!(!worker::source_root_valid("/missing"));
    std::fs::remove_dir(root.join("lib/dot")).unwrap();
    std::os::unix::fs::symlink("elsewhere", root.join("lib/dot")).unwrap();
    assert!(!worker::source_root_valid(&root_s));
    assert!(worker::result_path_valid("/tmp/result"));
    assert!(!worker::result_path_valid(""));
}

#[test]
fn precheck_preserves_arity_mode_and_refusal_precedence() {
    let dir = TempDir::new("worker-precheck").unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("lib/dot")).unwrap();
    let root_s = root.to_string_lossy();
    for argc in [0, 4, 6] {
        assert_eq!(
            worker::main_precheck(argc, "merge", &root_s, "result"),
            Err(Error::Usage)
        );
    }
    assert_eq!(
        worker::main_precheck(5, "bad", &root_s, "result"),
        Err(Error::Usage)
    );
    assert_eq!(
        worker::main_precheck(5, "merge", "relative", "result"),
        Err(Error::Refused)
    );
    assert_eq!(
        worker::main_precheck(5, "merge", &root_s, ""),
        Err(Error::Refused)
    );
    for mode in ["merge", "pre-sync", "deactivate", "doctor"] {
        assert_eq!(
            worker::main_precheck(5, mode, &root_s, "result"),
            Ok(Mode::parse(mode).unwrap())
        );
    }
    assert_eq!(Error::Usage.code(), 2);
    assert_eq!(Error::Refused.code(), 1);
    assert_eq!(Error::Usage.to_string(), "");
}

#[test]
fn deactivate_set_is_exactly_one_retiring_overlay() {
    for (kind, count, expected) in [
        ("retiring", 1, true),
        ("retiring", 0, false),
        ("retiring", 2, false),
        ("active", 1, false),
        ("eligible", 1, false),
        ("", 1, false),
    ] {
        assert_eq!(worker::deactivate_set_valid(kind, count), expected);
    }
}
