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
        &mut vec![],
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
            &mut vec![],
            &mut vec![]
        )
        .unwrap(),
        None
    );
    for (name, body) in [
        ("bad-key.conf", b"url=a\nUnknown=x\n".as_slice()),
        ("duplicate.conf", b"url=a\nurl=b\n"),
        ("bad-sync.conf", b"sync=bad\n"),
        ("bad-optional.conf", b"optional=maybe\n"),
        ("control.conf", b"url=a\0b\n"),
    ] {
        let p = file(d.path(), name, body, 0o600);
        assert!(
            overlays::parse_conf(
                &p,
                &p.to_string_lossy(),
                true,
                &h,
                &matches(),
                &mut vec![],
                &mut vec![]
            )
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
        dot_test_support::git()
            .arg("init")
            .arg("-q")
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        dot_test_support::git()
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

/// Parse one descriptor body: the outcome plus the permissive warnings
/// and the newer-Dot keys it recorded.
fn parse(
    root: &Path,
    name: &str,
    body: &[u8],
    strict: bool,
) -> (
    overlays::ParseOutcome,
    Vec<String>,
    Vec<dot::unknown_keys::DataKey>,
) {
    let p = file(root, name, body, 0o600);
    let mut warnings = vec![];
    let mut unknown = vec![];
    let outcome = overlays::parse_conf(
        &p,
        &p.to_string_lossy(),
        strict,
        &root.to_string_lossy(),
        &matches(),
        &mut warnings,
        &mut unknown,
    );
    (outcome, warnings, unknown)
}

#[test]
fn strict_descriptors_with_a_newer_key_are_skipped_not_activated() {
    // A descriptor decides what is cloned and linked, so a key this
    // release cannot read must never let it activate on a partial
    // reading: the descriptor is skipped (filtered), and the key is
    // recorded with the overlay it kept off, once per key.
    let d = TempDir::new("overlay-newer-key").unwrap();
    for (name, body) in [
        (
            "10-work.conf",
            b"url=https://example/work.git\nbranch=main\nbranch=next\n".as_slice(),
        ),
        (
            "20-local.local.conf",
            b"sync=none\npath=~/local\nbranch=main\n",
        ),
    ] {
        let (outcome, warnings, unknown) = parse(d.path(), name, body, true);
        assert_eq!(outcome, Ok(None), "{name}");
        assert!(warnings.is_empty(), "{name}: {warnings:?}");
        assert_eq!(unknown.len(), 1, "{name}");
        assert_eq!(unknown[0].key, "branch");
        assert_eq!(unknown[0].line, if name == "10-work.conf" { 2 } else { 3 });
        let overlay = if name == "10-work.conf" {
            "work"
        } else {
            "local"
        };
        assert_eq!(
            unknown[0].effect,
            dot::unknown_keys::Effect::OverlaySkipped(overlay.to_string())
        );
    }
    // A `sync=none` descriptor parses strictly even in legacy discovery.
    let (outcome, _, unknown) = parse(
        d.path(),
        "30-legacy.local.conf",
        b"sync=none\npath=~/legacy\nbranch=main\n",
        false,
    );
    assert_eq!(outcome, Ok(None));
    assert_eq!(unknown.len(), 1);
}

#[test]
fn strict_descriptors_keep_every_known_rule_beside_newer_keys() {
    // The key is skipped only after everything this release understands
    // has been validated: typos, malformed lines, known duplicates and
    // values, and the structural rules all still fail.
    let d = TempDir::new("overlay-newer-strict").unwrap();
    for (body, detail) in [
        (
            "url=a\nplatfroms=linux\n",
            "unknown key: platfroms (did you mean 'platforms'?)",
        ),
        ("url=a\nURL=b\n", "unknown key: URL"),
        ("url=a\nbranch\n", "unknown key: branch"),
        ("url=a\nbranch=x\nurl=b\n", "duplicate url"),
        ("url=a\nbranch=x\nsync=hg\n", "unknown sync value: hg"),
        (
            "url=a\nbranch=x\noptional=maybe\n",
            "unknown optional value: maybe",
        ),
        ("optional=true\n", "missing url"),
        (
            "sync=none\npath=rel\nbranch=x\n",
            "path must be absolute or begin with ~/",
        ),
        (
            "sync=none\npath=~/x\nurl=a\nbranch=x\n",
            "url is not valid with sync=none",
        ),
    ] {
        let (outcome, _, unknown) = parse(d.path(), "10-bad.conf", body.as_bytes(), true);
        let message = match outcome {
            Err(overlays::Error::Warning(message)) => message,
            other => panic!("{body:?}: {other:?}"),
        };
        assert!(
            message.ends_with(&format!(": {detail}")),
            "{body:?}: {message}"
        );
        assert!(unknown.is_empty(), "{body:?}");
    }
}

#[test]
fn a_newer_key_may_stand_in_for_the_source_of_a_skipped_descriptor() {
    // A newer Dot may name the source another way; the descriptor is
    // skipped either way, so a missing `url`/`path` does not fail the run.
    let d = TempDir::new("overlay-newer-source").unwrap();
    for (name, body) in [
        ("10-work.conf", b"github=owner/repo\n".as_slice()),
        ("20-local.local.conf", b"sync=none\nroot=~/x\n"),
    ] {
        let (outcome, _, unknown) = parse(d.path(), name, body, true);
        assert_eq!(outcome, Ok(None), "{name}");
        assert_eq!(unknown.len(), 1, "{name}");
    }
}

#[test]
fn legacy_skipped_local_descriptor_still_claims_its_name() {
    // Without the claim, a later same-named descriptor would take over the
    // name on this release only (the newer Dot activates the first).
    let d = TempDir::new("overlay-newer-claim").unwrap();
    let conf = d.path().join("conf");
    file(
        &conf,
        "10-work.local.conf",
        b"sync=none\npath=~/work\nbranch=main\n",
        0o600,
    );
    file(
        &conf,
        "20-work.conf",
        b"url=https://example/work.git\n",
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
    assert!(
        state.eligible_names.is_empty(),
        "{:?}",
        state.eligible_names
    );
    assert!(
        state
            .warnings
            .iter()
            .any(|w| w.contains("duplicate overlay name 'work'")),
        "{:?}",
        state.warnings
    );
}

#[test]
fn newer_keys_on_a_filtered_descriptor_stay_quiet() {
    // The host or platform filter already keeps the overlay off here, so
    // the unknown key changes nothing on this host and is not reported.
    let d = TempDir::new("overlay-newer-filtered").unwrap();
    let (outcome, _, unknown) = parse(
        d.path(),
        "10-mac.conf",
        b"url=a\nplatforms=macos\nbranch=main\n",
        true,
    );
    assert_eq!(outcome, Ok(None));
    assert!(unknown.is_empty());
}

#[test]
fn legacy_git_descriptors_still_warn_and_ignore_unknown_lines() {
    // Permissive parsing is unchanged: it always warned about and ignored
    // unknown lines, typos included, and activates the overlay.
    let d = TempDir::new("overlay-newer-legacy").unwrap();
    let (outcome, warnings, unknown) = parse(
        d.path(),
        "10-work.conf",
        b"url=a\nbranch=main\nplatfroms=linux\n",
        false,
    );
    assert!(outcome.unwrap().unwrap().starts_with("work|"));
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].ends_with(": branch=main"));
    assert!(unknown.is_empty());
}

#[test]
fn descriptor_known_keys_stay_more_than_two_edits_apart() {
    assert_eq!(
        dot::unknown_keys::crowded_pair(&overlays::DESCRIPTOR_KEYS),
        None
    );
}

#[test]
fn discovery_marks_a_skipped_descriptor_unsupported() {
    // Profile-aware discovery keeps the overlay selected but never
    // eligible, under its own lifecycle state (not the host/platform
    // filter's), and keeps the key for the caller to report.
    let d = TempDir::new("overlay-unsupported").unwrap();
    let conf = d.path().join("conf");
    file(
        &conf,
        "10-one.local.conf",
        b"sync=none\npath=~/one\n",
        0o600,
    );
    file(
        &conf,
        "20-two.local.conf",
        b"sync=none\npath=~/two\nbranch=main\n",
        0o600,
    );
    for profiles in [true, false] {
        let mut state = State::default();
        overlays::discover(
            &mut state,
            &conf,
            &conf.to_string_lossy(),
            &inputs(d.path(), profiles, &["one", "two"]),
            &matches(),
        )
        .unwrap();
        assert_eq!(state.eligible_names, ["one"], "profiles={profiles}");
        assert!(
            state
                .lifecycle
                .iter()
                .any(|record| record.starts_with("two|selected-unsupported|")),
            "profiles={profiles}: {:?}",
            state.lifecycle
        );
        assert_eq!(state.unknown_keys.len(), 1, "profiles={profiles}");
    }
}

#[test]
fn a_waived_source_keeps_the_other_rules_and_filters() {
    // Waiving `url`/`path` waives nothing else, and a host the descriptor
    // never applies to stays quiet.
    let d = TempDir::new("overlay-newer-waiver").unwrap();
    let (outcome, _, unknown) = parse(
        d.path(),
        "10-local.local.conf",
        b"sync=none\nroot=~/x\nurl=a\n",
        true,
    );
    assert!(
        matches!(outcome, Err(overlays::Error::Warning(ref m)) if m.ends_with("url is not valid with sync=none")),
        "{outcome:?}"
    );
    assert!(unknown.is_empty());
    let (outcome, _, unknown) = parse(
        d.path(),
        "20-mac.conf",
        b"github=owner/repo\nplatforms=macos\n",
        true,
    );
    assert_eq!(outcome, Ok(None));
    assert!(unknown.is_empty());
}

#[test]
fn profile_discovery_names_a_skipped_overlay_like_its_lifecycle() {
    // `20-beta.local.conf` is `beta` to profile-aware discovery even with
    // `sync=git`; the warning must use the same name.
    let d = TempDir::new("overlay-newer-name").unwrap();
    let conf = d.path().join("conf");
    file(
        &conf,
        "20-beta.local.conf",
        b"url=https://example/beta.git\nbranch=main\n",
        0o600,
    );
    let mut state = State::default();
    overlays::discover(
        &mut state,
        &conf,
        &conf.to_string_lossy(),
        &inputs(d.path(), true, &["beta"]),
        &matches(),
    )
    .unwrap();
    assert_eq!(
        state.unknown_keys[0].effect,
        dot::unknown_keys::Effect::OverlaySkipped("beta".to_string())
    );
    assert!(state.lifecycle[0].starts_with("beta|selected-unsupported|"));
}

#[test]
fn a_skipped_base_overlay_makes_selection_fall_back_to_base() {
    // `base` selects `one`, whose descriptor has a newer key. Its personal
    // selectors (which the newer Dot reads) cannot be read here, and they
    // could outrank the root selector choosing `full`, so selection falls
    // back to `base` and says why.
    let d = TempDir::new("overlay-newer-phase-one").unwrap();
    let home = d.path().join("home");
    let xdg = d.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    file(
        &xdg,
        "dot/profiles.d/base.conf",
        b"version=1\noverlays=one\n",
        0o600,
    );
    file(
        &xdg,
        "dot/profiles.d/full.conf",
        b"version=1\nprofiles=base\noverlays=two\n",
        0o600,
    );
    file(
        &xdg,
        "dot/profile-selectors.d/00-default.conf",
        b"version=1\nprofile=full\n",
        0o600,
    );
    file(
        &xdg,
        "dot/overlays.d/10-one.local.conf",
        b"sync=none\npath=~/one\nbranch=main\n",
        0o600,
    );
    file(
        &xdg,
        "dot/overlays.d/20-two.local.conf",
        b"sync=none\npath=~/two\n",
        0o600,
    );
    let inputs = overlays::ResolveInputs {
        home: home.to_string_lossy().into_owned(),
        xdg_config: xdg.to_string_lossy().into_owned(),
        discovery_silent: true,
        default_profile: None,
        user: Some("chris".into()),
        host: Some("nas".into()),
        platform: Some("linux".into()),
        termux: false,
        euid: euid(),
    };
    let mut state = State::default();
    let mut profiles = dot::profiles::State::default();
    overlays::resolve(&mut state, &mut profiles, "inspect", &inputs).unwrap();
    assert_eq!(profiles.selected, "base");
    assert_eq!(
        profiles.selection_state,
        dot::profiles::SELECTOR_FALLBACK_STATE
    );
    let effects: Vec<_> = overlays::unknown_keys(&state, &profiles)
        .map(|key| key.effect.clone())
        .collect();
    assert_eq!(
        effects,
        [
            dot::unknown_keys::Effect::SelectorsUnread("one".to_string()),
            dot::unknown_keys::Effect::OverlaySkipped("one".to_string()),
        ]
    );
}
