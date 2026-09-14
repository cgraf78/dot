//! Native contracts for overlay descriptors, discovery, paths, and security.
use dot::overlays::{self, Inputs, MatchInputs, State};
use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
fn file(root: &Path, name: &str, body: &[u8], mode: u32) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}
fn matches() -> MatchInputs {
    MatchInputs {
        platform: Some("linux".into()),
        termux: false,
        host: Some("nas".into()),
    }
}
fn inputs(home: &Path, profiles: bool, selected: &[&str]) -> Inputs {
    Inputs {
        home: home.to_string_lossy().into(),
        xdg_config: String::new(),
        discovery_silent: false,
        profiles_present: profiles,
        selected: selected.iter().map(|s| (*s).into()).collect(),
        platform: Some("linux".into()),
        termux: false,
        host: Some("nas".into()),
        euid: euid(),
    }
}

#[test]
fn names_and_safe_value_path_grammars_are_exact() {
    for (f, s, w) in [
        ("10-work.conf", "git", "work"),
        ("10-local.local.conf", "none", "local"),
        ("10-local.local.conf", "git", "local.local"),
        ("plain", "git", "plain"),
        ("10-", "git", "10-"),
    ] {
        assert_eq!(overlays::overlay_name(f, s), w)
    }
    assert_eq!(
        overlays::overlay_profile_name("10-local.local.conf"),
        "local"
    );
    for (v, ok) in [
        (b"safe".as_slice(), true),
        (b"a|b", false),
        (b"a\nb", false),
        (&[127], false),
    ] {
        assert_eq!(overlays::descriptor_value_safe(v), ok)
    }
    for (p, ok) in [
        (b"a".as_slice(), true),
        (b"a/b", true),
        (b"", false),
        (b"/a", false),
        (b"a//b", false),
        (b"a/./b", false),
        (b"a/../b", false),
    ] {
        assert_eq!(overlays::relative_path_safe(p), ok)
    }
}

#[test]
fn descriptor_file_and_parser_enforce_strict_format_filters_and_warnings() {
    let d = TempDir::new("overlay-parse").unwrap();
    let h = d.path().to_string_lossy();
    let good = file(
        d.path(),
        "10-work.conf",
        b"url=https://example/work.git\nplatforms=linux\nhosts=nas\noptional=true\n",
        0o600,
    );
    assert!(overlays::descriptor_file_safe(&good));
    let mut warnings = vec![];
    let record = overlays::parse_conf(
        &good,
        &good.to_string_lossy(),
        true,
        &h,
        &matches(),
        &mut warnings,
    )
    .unwrap()
    .unwrap();
    assert!(record.starts_with("work|"));
    assert!(record.ends_with("|true|git"));
    assert!(warnings.is_empty());
    let filtered = file(d.path(), "20-mac.conf", b"url=x\nplatforms=macos\n", 0o600);
    assert_eq!(
        overlays::parse_conf(
            &filtered,
            &filtered.to_string_lossy(),
            true,
            &h,
            &matches(),
            &mut vec![]
        )
        .unwrap(),
        None
    );
    for (name, body) in [
        ("bad-key.conf", b"unknown=x\n".as_slice()),
        ("duplicate.conf", b"url=a\nurl=b\n"),
        ("bad-sync.conf", b"sync=bad\n"),
        ("bad-optional.conf", b"optional=maybe\n"),
        ("control.conf", b"url=a\0b\n"),
    ] {
        let p = file(d.path(), name, body, 0o600);
        assert!(
            overlays::parse_conf(&p, &p.to_string_lossy(), true, &h, &matches(), &mut vec![])
                .is_err(),
            "{name}"
        )
    }
}

#[test]
fn local_sources_refuse_escape_dangling_unreadable_and_cross_source_destinations() {
    let d = TempDir::new("overlay-local").unwrap();
    let home = d.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let overlay = d.path().join("overlay");
    file(&overlay, "home/config/app", b"x", 0o600);
    let path = overlay.to_string_lossy();
    assert!(overlays::source_validate(&path, &[], &home.to_string_lossy()).is_ok());
    let src = overlay.join("home/config/app");
    let real = overlay.join("home").canonicalize().unwrap();
    assert!(
        overlays::source_entry_validate(
            &path,
            &src,
            "config/app",
            &real.to_string_lossy(),
            &[],
            &home.to_string_lossy()
        )
        .is_ok()
    );
    assert!(
        overlays::source_entry_validate(
            &path,
            &src,
            "../app",
            &real.to_string_lossy(),
            &[],
            &home.to_string_lossy()
        )
        .is_err()
    );
    std::os::unix::fs::symlink("missing", overlay.join("home/dangling")).unwrap();
    assert!(overlays::source_validate(&path, &[], &home.to_string_lossy()).is_err());
    let second = d.path().join("second");
    file(&second, "home/data", b"x", 0o600);
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::os::unix::fs::symlink(second.join("home"), home.join("config/inside")).unwrap();
    let records = vec![format!("second|{}|||false|none", second.to_string_lossy())];
    assert!(
        overlays::destination_outside_local_sources(
            "config/inside/data",
            &records,
            &home.to_string_lossy()
        )
        .is_err()
    );
}

#[test]
fn discovery_is_sorted_tracks_lifecycle_and_strict_selection_errors() {
    let d = TempDir::new("overlay-discover").unwrap();
    let conf = d.path().join("conf");
    std::fs::create_dir(&conf).unwrap();
    file(
        &conf,
        "20-two.local.conf",
        b"sync=none\npath=~/two\n",
        0o600,
    );
    file(
        &conf,
        "10-one.local.conf",
        b"sync=none\npath=~/one\n",
        0o600,
    );
    let mut state = State::default();
    overlays::discover(
        &mut state,
        &conf,
        &conf.to_string_lossy(),
        &inputs(d.path(), false, &[]),
        &matches(),
    )
    .unwrap();
    assert_eq!(state.configured, ["one", "two"]);
    assert_eq!(state.selected, ["one", "two"]);
    assert_eq!(state.eligible_names, ["one", "two"]);
    assert_eq!(state.lifecycle.len(), 2);
    let mut strict = State::default();
    assert!(
        overlays::discover(
            &mut strict,
            &conf,
            &conf.to_string_lossy(),
            &inputs(d.path(), true, &["missing"]),
            &matches()
        )
        .is_err()
    );
    assert!(
        strict
            .discovery_error
            .as_deref()
            .unwrap()
            .contains("no descriptor")
    );
    let mut selected = State::default();
    overlays::discover(
        &mut selected,
        &conf,
        &conf.to_string_lossy(),
        &inputs(d.path(), true, &["two"]),
        &matches(),
    )
    .unwrap();
    assert_eq!(selected.selected, ["two"]);
    assert!(selected.lifecycle[0].contains("not-selected"));
}

#[test]
fn duplicate_and_invalid_descriptor_names_fail_strict_but_legacy_warns() {
    let d = TempDir::new("overlay-duplicates").unwrap();
    let conf = d.path().join("conf");
    std::fs::create_dir(&conf).unwrap();
    file(&conf, "10-work.conf", b"url=a\n", 0o600);
    file(&conf, "20-work.conf", b"url=a\n", 0o600);
    let mut legacy = State::default();
    overlays::discover(
        &mut legacy,
        &conf,
        &conf.to_string_lossy(),
        &inputs(d.path(), false, &[]),
        &matches(),
    )
    .unwrap();
    assert_eq!(legacy.eligible_names, ["work"]);
    assert_eq!(legacy.warnings.len(), 1);
    let mut strict = State::default();
    assert!(
        overlays::discover(
            &mut strict,
            &conf,
            &conf.to_string_lossy(),
            &inputs(d.path(), true, &["work"]),
            &matches()
        )
        .is_err()
    );
    assert!(strict.discovery_error.unwrap().contains("duplicate"));
}

#[test]
fn checkout_identity_requires_worktree_and_matching_effective_origin() {
    let d = TempDir::new("overlay-checkout").unwrap();
    let repo = d.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["remote", "add", "origin", "https://example/repo.git"])
            .status()
            .unwrap()
            .success()
    );
    assert!(overlays::is_worktree(&repo));
    assert!(overlays::origin_matches(&repo, "https://example/repo.git").is_ok());
    assert!(overlays::origin_matches(&repo, "https://example/other.git").is_err());
    assert!(
        overlays::checkout_matches(
            &repo,
            "https://example/repo.git",
            &d.path().to_string_lossy()
        )
        .is_ok()
    );
    assert!(!overlays::is_worktree(&d.path().join("missing")));
    assert_eq!(
        overlays::effective_url("~/repo.git", "/home/test"),
        "/home/test/repo.git"
    );
}

#[test]
fn preflight_and_use_set_publish_only_valid_requested_sets() {
    let d = TempDir::new("overlay-preflight").unwrap();
    let overlay = d.path().join("local");
    file(&overlay, "home/file", b"x", 0o600);
    let record = format!("local|{}|||false|none", overlay.to_string_lossy());
    let mut state = State {
        eligible: vec![record.clone()],
        active: vec![],
        overlays: vec![record],
        ..State::default()
    };
    assert!(overlays::preflight(&mut state, &d.path().to_string_lossy()).is_ok());
    overlays::use_set(&mut state, "eligible").unwrap();
    assert_eq!(state.overlays, state.eligible);
    overlays::use_set(&mut state, "active").unwrap();
    assert!(state.overlays.is_empty());
    assert!(overlays::use_set(&mut state, "unknown").is_err());
}

#[test]
fn conf_directory_resolution_has_xdg_and_home_defaults() {
    assert_eq!(
        overlays::conf_dir("/cfg", "/home/test").as_deref(),
        Some("/cfg/dot/overlays.d")
    );
    assert_eq!(
        overlays::conf_dir("", "/home/test").as_deref(),
        Some("/home/test/.config/dot/overlays.d")
    );
    assert_eq!(overlays::conf_dir("", ""), None);
}
