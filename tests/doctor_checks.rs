//! Native behavioral tests for the doctor check family.

use std::path::Path;
use std::process::{Command, Stdio};

use dot::doctor_checks::{
    BaseRepoInputs, LifecycleInputs, MergeInputs, OverlayInputs, ProviderInputs, ProviderInstaller,
    Record, check_base_repo, check_merges, check_overlays, check_profile_lifecycle, check_provider,
    check_update_lock, completed_identity_matches_home, is_client_checkout, render, shdeps_binary,
};
use dot_test_support::TempDir;

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
            "commit.gpgsign=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn valid_local(_: &str) -> Result<(), String> {
    Ok(())
}

#[test]
fn renderer_has_stable_protocol() {
    let records = vec![
        Record::section("Heads"),
        Record::ok("fine", Some("detail".into())),
        Record::warn("careful", Some("detail".into())),
        Record::fail("broken", None),
        Record::skip("later", None),
    ];
    assert_eq!(
        render(&records),
        "\nHeads\n  ✓ fine (detail)\n  ⚠ careful\n    detail\n  ✗ broken\n  · later\n"
    );
}

#[test]
fn update_lock_resolution_clear_and_unsafe() {
    let scratch = TempDir::new("doctor-lock-native").expect("scratch");
    let missing = scratch.path().join("missing");
    assert!(render(&check_update_lock(None)).contains("path cannot be resolved"));
    assert!(render(&check_update_lock(Some(&missing))).contains("update lock is clear"));
    let file = scratch.path().join("file");
    std::fs::write(&file, b"unsafe").expect("write");
    assert!(render(&check_update_lock(Some(&file))).contains("path is unsafe"));
}

#[test]
fn update_lock_initializing_incomplete_live_and_stale() {
    let scratch = TempDir::new("doctor-lock-states").expect("scratch");
    let fresh = scratch.path().join("fresh");
    std::fs::create_dir_all(&fresh).expect("fresh lock");
    assert!(render(&check_update_lock(Some(&fresh))).contains("being initialized"));

    let aged = scratch.path().join("aged");
    std::fs::create_dir_all(&aged).expect("aged lock");
    let status = Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(&aged)
        .status()
        .expect("age lock");
    assert!(status.success());
    assert!(render(&check_update_lock(Some(&aged))).contains("record is incomplete"));

    let state = scratch.path().join("state");
    let log = dot::log::Log::new(false, false);
    let mut sink = Vec::new();
    let guard = dot::update_lock::acquire(&state, false, &log, None, &mut sink).expect("lock");
    let live = dot::update_lock::lock_path(&state);
    let output = render(&check_update_lock(Some(&live)));
    assert!(output.contains("update is currently running"));
    assert!(output.contains(&format!("pid {}", std::process::id())));
    drop(guard);

    let stale = scratch.path().join("stale");
    std::fs::create_dir_all(&stale).expect("stale lock");
    std::fs::write(
        stale.join("owner"),
        "pid\t42424242\nstart\tproc:1\ntoken\tstale\n",
    )
    .expect("owner");
    assert!(render(&check_update_lock(Some(&stale))).contains("owner is stale"));
}

#[test]
fn merge_inventory_branch_matrix() {
    let scratch = TempDir::new("doctor-merges-native").expect("scratch");
    let ext = scratch.path().join("extensions");
    let check = |enabled, count| {
        render(&check_merges(&MergeInputs {
            enabled,
            extensions_dir: ext.to_string_lossy().into_owned(),
            spec_count: count,
        }))
    };
    assert!(check(false, None).contains("no extension root configured"));
    assert!(check(true, Some(0)).contains("none configured"));
    std::fs::create_dir_all(ext.join("merge-hooks.d")).expect("hooks");
    assert!(check(true, None).contains("inventory is invalid"));
    assert!(check(true, Some(2)).contains("2 hook(s)"));
}

#[test]
fn lifecycle_branch_matrix() {
    let check = |present, load, eligible, records, extensions| {
        check_profile_lifecycle(&LifecycleInputs {
            profiles_present: present,
            load_ok: load,
            eligible,
            active: vec![],
            records,
            extensions_enabled: extensions,
            deactivation_ok: &|_| true,
        })
    };
    assert!(check(false, true, vec![], vec![], true).is_empty());
    assert!(render(&check(true, false, vec![], vec![], true)).contains("state unsafe"));
    assert!(render(&check(true, true, vec![], vec![], true)).contains("no pending deactivations"));
    let pending = render(&check(true, true, vec![], vec!["old|record".into()], false));
    assert!(pending.contains("extensions are disabled"));
    assert!(pending.contains("profile deactivation pending"));
}

#[test]
fn lifecycle_active_retained_and_retiring_authority_matrix() {
    let bad = |record: &str| !record.contains("bad");
    let records = check_profile_lifecycle(&LifecycleInputs {
        profiles_present: true,
        load_ok: true,
        eligible: vec!["active".into(), "retained".into()],
        active: vec!["active|bad".into()],
        records: vec![
            "active|ledger".into(),
            "retained|bad".into(),
            "retiring|bad".into(),
        ],
        extensions_enabled: true,
        deactivation_ok: &bad,
    });
    let output = render(&records);
    assert!(output.contains("active profile deactivation authority unsafe"));
    assert!(output.contains("retained profile deactivation authority unavailable"));
    assert!(output.contains("retiring overlay authority unsafe"));
    assert!(output.contains("profile deactivation pending"));
}

fn overlays<'a>(
    manifest: String,
    config_error: Option<&'a str>,
    present: bool,
) -> OverlayInputs<'a> {
    OverlayInputs {
        home: "/home/test",
        profile_config_error: config_error,
        profiles_present: present,
        profile_user: None,
        profile_host: None,
        selected_profile: None,
        selection_state: None,
        included_profiles: vec![],
        phase_one: vec![],
        selectors: vec![],
        lifecycle: LifecycleInputs {
            profiles_present: present,
            load_ok: true,
            eligible: vec![],
            active: vec![],
            records: vec![],
            extensions_enabled: false,
            deactivation_ok: &|_| true,
        },
        configured_count: 0,
        manifest,
        discovery_error: None,
        active_records: vec![],
        overlay_lifecycle: vec![],
        local_validate: &|_| Ok(()),
    }
}

#[test]
fn overlays_report_config_legacy_and_empty_inventory() {
    let scratch = TempDir::new("doctor-overlays-native").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let invalid = render(&check_overlays(&overlays(
        manifest.clone(),
        Some("bad profile"),
        true,
    )));
    assert!(invalid.contains("profile configuration invalid"));
    assert!(invalid.contains("bad profile"));
    let legacy = render(&check_overlays(&overlays(manifest.clone(), None, false)));
    assert!(legacy.contains("profile selection disabled"));
    assert!(legacy.contains("no overlays to check"));
    let profiles = render(&check_overlays(&overlays(manifest, None, true)));
    assert!(profiles.contains("no pending deactivations"));
    assert!(profiles.contains("no overlays to check"));
}

#[test]
fn overlays_discovery_lifecycle_local_and_sync_matrix() {
    let scratch = TempDir::new("doctor-overlays-matrix").expect("scratch");
    let manifest = scratch
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let local = scratch.path().join("local").to_string_lossy().into_owned();
    let missing = scratch
        .path()
        .join("missing-git")
        .to_string_lossy()
        .into_owned();
    let mut input = overlays(manifest, None, true);
    input.configured_count = 7;
    input.discovery_error = Some("bad descriptor");
    input.overlay_lifecycle = vec![
        "off|not-selected|d".into(),
        "wrong-host|selected-ineligible|d".into(),
        "optional-gone|selected-optional-unavailable|d".into(),
        "required-gone|selected-unavailable|d".into(),
        "unknown|future-state|d".into(),
        "orphan|active|d".into(),
        "local|active|d".into(),
        "required|active|d".into(),
        "optional|active|d".into(),
    ];
    input.active_records = vec![
        format!("local|{local}|||false|none"),
        format!("required|{missing}|||false|git"),
        format!("optional|{missing}|||true|git"),
    ];
    input.local_validate = &|path| {
        if path.ends_with("local") {
            Err("local diagnostic".into())
        } else {
            Ok(())
        }
    };
    let output = render(&check_overlays(&input));
    for expected in [
        "overlay descriptor invalid",
        "off: not selected",
        "host/platform ineligible",
        "selected optional but unavailable",
        "selected but unavailable",
        "unknown overlay lifecycle state",
        "active lifecycle record missing",
        "local: local source unavailable",
        "required: not cloned",
        "optional overlay not cloned",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?} in {output}"
        );
    }
}

#[test]
fn overlays_git_origin_and_manifest_health_matrix() {
    let scratch = TempDir::new("doctor-overlays-git").expect("scratch");
    let home = scratch.path().join("home");
    let repo = home.join(".dotfiles-git");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&repo).expect("overlay");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &["remote", "add", "origin", "https://actual.invalid/repo.git"],
    );
    let rel = ".config/demo";
    let source = repo.join("home").join(rel);
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("source parent");
    std::fs::write(&source, b"demo").expect("source");
    let destination = home.join(rel);
    std::fs::create_dir_all(destination.parent().expect("destination parent"))
        .expect("destination parent");
    let link_target =
        dot::repos_overlays::record_link_target(rel, "git", &repo.to_string_lossy(), Some("git"))
            .expect("git link target");
    std::os::unix::fs::symlink(&link_target, &destination).expect("link");
    let manifest = scratch.path().join("manifest");
    std::fs::write(&manifest, format!("{rel}\tgit\t{link_target}\n")).expect("manifest");
    let input = OverlayInputs {
        home: home.to_str().expect("home utf8"),
        profile_config_error: None,
        profiles_present: false,
        profile_user: None,
        profile_host: None,
        selected_profile: None,
        selection_state: None,
        included_profiles: vec![],
        phase_one: vec![],
        selectors: vec![],
        lifecycle: LifecycleInputs {
            profiles_present: false,
            load_ok: true,
            eligible: vec![],
            active: vec![],
            records: vec![],
            extensions_enabled: false,
            deactivation_ok: &|_| true,
        },
        configured_count: 1,
        manifest: manifest.to_string_lossy().into_owned(),
        discovery_error: None,
        active_records: vec![format!(
            "git|{}|https://expected.invalid/repo.git||false|git",
            repo.display()
        )],
        overlay_lifecycle: vec!["git|active|d".into()],
        local_validate: &valid_local,
    };
    let drift = render(&check_overlays(&input));
    assert!(drift.contains("git: cloned"));
    assert!(drift.contains("remote URL drift"));
    assert!(drift.contains("overlay symlinks healthy"));

    std::fs::write(&manifest, b"malformed\n").expect("bad manifest");
    assert!(render(&check_overlays(&input)).contains("1 overlay symlink issue(s)"));
}

#[test]
fn shdeps_selection_precedence_and_fallback() {
    let scratch = TempDir::new_exec("doctor-shdeps-native").expect("scratch");
    let explicit = scratch.path().join("chosen");
    executable(&explicit);
    let installer = scratch.path().join("install.sh");
    assert_eq!(shdeps_binary(Some(&explicit), &installer), Some(explicit));
    let sibling = scratch.path().join("shdeps");
    executable(&sibling);
    assert_eq!(shdeps_binary(None, &installer), Some(sibling));
    assert_eq!(shdeps_binary(None, Path::new("missing/install.sh")), None);
}

fn provider<'a>(
    kind: Option<&'a str>,
    installer: Option<ProviderInstaller<'a>>,
    binary: Option<&'a str>,
    expected: Option<&'a str>,
    actual: Option<&'a str>,
) -> ProviderInputs<'a> {
    ProviderInputs {
        home: "/home/test",
        dependency_provider: kind,
        policy: "pinned",
        configure_ok: true,
        dev_dir: "/dev",
        development_exists: false,
        development_valid: false,
        installer,
        locked_revision: None,
        development_revision: None,
        binary,
        expected_abi: expected,
        actual_abi: actual,
    }
}

#[test]
fn provider_disabled_unsupported_and_healthy() {
    assert!(
        render(&check_provider(&provider(None, None, None, None, None))).contains("no dependency")
    );
    assert!(
        render(&check_provider(&provider(
            Some("other"),
            None,
            None,
            None,
            None
        )))
        .contains("unsupported")
    );
    let installer = ProviderInstaller {
        path: "/managed/install.sh",
        source: "managed",
    };
    let output = render(&check_provider(&provider(
        Some("shdeps"),
        Some(installer),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    )));
    assert!(output.contains("Shdeps provider ABI (abi:1)"));
}

#[test]
fn provider_failure_source_development_and_abi_matrix() {
    let mut configure = provider(Some("shdeps"), None, None, None, None);
    configure.configure_ok = false;
    assert!(render(&check_provider(&configure)).contains("provider is unavailable"));

    let no_installer = provider(Some("shdeps"), None, None, None, None);
    assert!(render(&check_provider(&no_installer)).contains("provider is unavailable"));

    let unknown = ProviderInstaller {
        path: "/install",
        source: "unknown",
    };
    assert!(
        render(&check_provider(&provider(
            Some("shdeps"),
            Some(unknown),
            None,
            None,
            None
        )))
        .contains("source is unavailable")
    );

    let explicit = ProviderInstaller {
        path: "/install",
        source: "explicit",
    };
    let explicit_output = render(&check_provider(&provider(
        Some("shdeps"),
        Some(explicit),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:2"),
    )));
    assert!(explicit_output.contains("caller-selected reviewed installer"));
    assert!(explicit_output.contains("ABI mismatch"));

    let mut dev = provider(
        Some("shdeps"),
        Some(ProviderInstaller {
            path: "/dev/shdeps/install.sh",
            source: "pinned-dev",
        }),
        Some("/bin/shdeps"),
        Some("1"),
        Some("abi:1"),
    );
    dev.policy = "latest";
    dev.development_exists = true;
    dev.development_valid = true;
    dev.locked_revision = Some("1234567890abcdef");
    dev.development_revision = Some("1234567890abcdef");
    let dev_output = render(&check_provider(&dev));
    assert!(dev_output.contains("development checkout"));
    assert!(dev_output.contains("matches Dot lock: 1234567890ab"));

    let mut invalid_dev = dev;
    invalid_dev.development_valid = false;
    invalid_dev.installer = Some(ProviderInstaller {
        path: "/managed/install.sh",
        source: "managed",
    });
    assert!(render(&check_provider(&invalid_dev)).contains("development checkout ignored"));

    let managed = ProviderInstaller {
        path: "/managed/install.sh",
        source: "managed",
    };
    let missing_binary = provider(Some("shdeps"), Some(managed), None, Some("1"), None);
    assert!(render(&check_provider(&missing_binary)).contains("binary is unavailable"));
}

#[test]
fn completed_identity_is_plain_and_last_wins() {
    let scratch = TempDir::new("doctor-identity-native").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let marker = scratch.path().join("completed");
    std::fs::write(
        &marker,
        format!(
            "git_dir=/bad\nworktree=/bad\ngit_dir={}/.git\nworktree={}\n",
            home.display(),
            home.display()
        ),
    )
    .expect("marker");
    assert!(completed_identity_matches_home(
        Some(&marker),
        &home.to_string_lossy()
    ));
    assert!(!completed_identity_matches_home(
        None,
        &home.to_string_lossy()
    ));
}

#[test]
fn completed_identity_rejects_malformed_mismatched_and_symlink_markers() {
    let scratch = TempDir::new("doctor-identity-invalid").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let marker = scratch.path().join("completed");
    for body in [
        "",
        "git_dir=/bad\nworktree=/bad\n",
        "git_dir=/home/test/.git\n",
    ] {
        std::fs::write(&marker, body).expect("marker");
        assert!(!completed_identity_matches_home(
            Some(&marker),
            &home.to_string_lossy()
        ));
    }
    let target = scratch.path().join("target");
    std::fs::write(
        &target,
        format!(
            "git_dir={}/.git\nworktree={}\n",
            home.display(),
            home.display()
        ),
    )
    .expect("target");
    std::fs::remove_file(&marker).expect("remove marker");
    std::os::unix::fs::symlink(&target, &marker).expect("symlink marker");
    assert!(!completed_identity_matches_home(
        Some(&marker),
        &home.to_string_lossy()
    ));
}

#[test]
fn client_checkout_needs_root_and_identity_or_flag() {
    let scratch = TempDir::new("doctor-client-native").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    assert!(!is_client_checkout(&home, None));
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["config", "dot.clientRepository", "true"]);
    assert!(is_client_checkout(&home, None));
}

#[test]
fn nested_directory_is_not_the_client_checkout_root() {
    let scratch = TempDir::new("doctor-client-nested").expect("scratch");
    let home = scratch.path().join("home");
    let nested = home.join("nested");
    std::fs::create_dir_all(&nested).expect("nested");
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["config", "dot.clientRepository", "true"]);
    assert!(!is_client_checkout(&nested, None));
}

#[test]
fn base_repo_missing_recognized_and_unrecognized() {
    let missing = BaseRepoInputs {
        topology: "missing",
        client_git_dir: "/home/test/.git",
        home: "/home/test",
        is_client_checkout: false,
    };
    assert!(render(&check_base_repo(&missing)).contains("client repository is missing"));
    let recognized = BaseRepoInputs {
        is_client_checkout: true,
        ..missing
    };
    assert!(render(&check_base_repo(&recognized)).contains("ordinary checkout rooted at $HOME"));
}

fn base<'a>(topology: &'a str, git_dir: &'a Path, home: &'a Path) -> BaseRepoInputs<'a> {
    BaseRepoInputs {
        topology,
        client_git_dir: git_dir.to_str().expect("git dir utf8"),
        home: home.to_str().expect("home utf8"),
        is_client_checkout: false,
    }
}

#[test]
fn base_repo_ordinary_dirty_detached_upstream_and_mismatch_matrix() {
    let scratch = TempDir::new("doctor-base-ordinary").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    let clean = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &home,
    )));
    assert!(clean.contains("ordinary client layout"));
    assert!(clean.contains("no tracked client changes"));
    assert!(clean.contains("client HEAD on branch (main)"));
    assert!(clean.contains("upstream is not configured"));

    std::fs::write(home.join("tracked"), b"one\n").expect("tracked");
    git(&home, &["add", "tracked"]);
    git(&home, &["commit", "-q", "-m", "tracked"]);
    std::fs::write(home.join("tracked"), b"two\n").expect("dirty");
    git(&home, &["checkout", "-q", "--detach"]);
    let dirty = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &home,
    )));
    assert!(dirty.contains("1 tracked client change(s)"));
    assert!(dirty.contains("client HEAD is detached"));

    let nested = home.join("nested");
    std::fs::create_dir_all(&nested).expect("nested");
    let mismatch = render(&check_base_repo(&base(
        "ordinary",
        &home.join(".git"),
        &nested,
    )));
    assert!(mismatch.contains("client worktree mismatch"));
}

#[test]
fn base_repo_separate_and_unrecognized_topology_matrix() {
    let scratch = TempDir::new("doctor-base-separate").expect("scratch");
    let home = scratch.path().join("home");
    let bare = scratch.path().join("bare.git");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&bare).expect("bare");
    git(&bare, &["init", "-q", "--bare"]);
    let bare_output = render(&check_base_repo(&base("separate", &bare, &home)));
    assert!(bare_output.contains("legacy bare client layout"));

    git(&bare, &["config", "core.bare", "false"]);
    git(
        &bare,
        &["config", "core.worktree", home.to_str().expect("home utf8")],
    );
    let explicit = render(&check_base_repo(&base("separate", &bare, &home)));
    assert!(explicit.contains("explicit-worktree client layout"));

    git(&bare, &["config", "--unset", "core.worktree"]);
    let unidentified = render(&check_base_repo(&base("separate", &bare, &home)));
    assert!(unidentified.contains("no worktree identity"));

    let unknown = render(&check_base_repo(&base("liminal", &bare, &home)));
    assert!(unknown.contains("client worktree mismatch"));
    assert!(unknown.contains("client upstream is not configured"));
}

#[test]
fn base_repo_upstream_current_ahead_behind_and_diverged() {
    let scratch = TempDir::new("doctor-base-upstream").expect("scratch");
    let remote = scratch.path().join("remote.git");
    std::fs::create_dir_all(&remote).expect("remote");
    git(&remote, &["init", "-q", "--bare"]);
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(
        &home,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("remote utf8"),
        ],
    );
    git(&home, &["push", "-q", "-u", "origin", "main"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let git_dir = home.join(".git");
    let inputs = || base("ordinary", &git_dir, &home);
    assert!(render(&check_base_repo(&inputs())).contains("origin/main (current)"));

    git(&home, &["commit", "-q", "--allow-empty", "-m", "ahead"]);
    assert!(render(&check_base_repo(&inputs())).contains("1 commit(s) ahead"));

    git(&home, &["reset", "-q", "--hard", "origin/main"]);
    let peer = scratch.path().join("peer");
    let status = Command::new("git")
        .args(["clone", "-q"])
        .arg(&remote)
        .arg(&peer)
        .status()
        .expect("clone peer");
    assert!(status.success());
    git(&peer, &["commit", "-q", "--allow-empty", "-m", "behind"]);
    git(&peer, &["push", "-q", "origin", "main"]);
    git(&home, &["fetch", "-q", "origin"]);
    assert!(render(&check_base_repo(&inputs())).contains("1 commit(s) behind"));

    git(&home, &["commit", "-q", "--allow-empty", "-m", "local"]);
    assert!(render(&check_base_repo(&inputs())).contains("1 ahead, 1 behind"));
}
