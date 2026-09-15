//! Native contracts for profile definitions, selection, and conflicts.
use dot::profiles::{self, MemberKind, SelectorClass, State};
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

#[test]
fn scalar_and_list_validators_cover_full_grammar() {
    for (v, ok) in [
        (b"base".as_slice(), true),
        (b"a1-b", true),
        (b"", false),
        (b"Base", false),
        (b"1a", false),
        (b"a_b", false),
    ] {
        assert_eq!(profiles::identifier_valid(v), ok)
    }
    for (v, ok) in [
        (b"safe".as_slice(), true),
        (b"", true),
        (b"a|b", false),
        (b"a\tb", false),
        (b"a\nb", false),
        (b"a\rb", false),
    ] {
        assert_eq!(profiles::value_safe(v), ok)
    }
    assert!(profiles::list_valid(b"base,work", MemberKind::Profile));
    assert!(!profiles::list_valid(b"base,,work", MemberKind::Profile));
    assert!(!profiles::list_valid(b"dotfiles", MemberKind::Overlay));
    assert!(profiles::list_valid(b"work,personal", MemberKind::Overlay));
    for (h, want) in [
        (b"Host.".as_slice(), Some("host")),
        (b"a-b.example", Some("a-b.example")),
        (b"", None),
        (b".bad", None),
        (b"bad_", None),
    ] {
        assert_eq!(profiles::host_normalize(h), want.map(str::to_string))
    }
    for (u, ok) in [
        (b"user".as_slice(), true),
        (b"_svc", true),
        (b"A.b-1", true),
        (b"", false),
        (b"1user", false),
        (b"bad/x", false),
    ] {
        assert_eq!(profiles::user_valid(u), ok)
    }
}

#[test]
fn profile_files_enforce_shape_size_controls_and_private_ownership() {
    let d = TempDir::new("profiles-safe").unwrap();
    let p = file(d.path(), "ok.conf", b"version=1\noverlays=work\n", 0o600);
    assert!(profiles::file_safe(&p).is_ok());
    assert!(profiles::private_path_safe(&p, euid()));
    for (name, body) in [
        ("nul", b"a\0b".as_slice()),
        ("tab", b"a\tb"),
        ("del", b"a\x7fb"),
        ("repeat", b"0123456789abcdef0123456789abcdef"),
    ] {
        let p = file(d.path(), name, body, 0o600);
        assert!(profiles::file_safe(&p).is_err())
    }
    let large = file(d.path(), "large", &vec![b'x'; 65537], 0o600);
    assert!(profiles::file_safe(&large).is_err());
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!profiles::private_path_safe(&p, euid()));
    std::os::unix::fs::symlink(&p, d.path().join("link")).unwrap();
    assert!(profiles::file_safe(&d.path().join("link")).is_err());
}

#[test]
fn definition_and_selector_errors_are_explicit() {
    let path = Path::new("profile.conf");
    let good = profiles::parse_definition(
        path,
        b"# comment\nversion=1\nprofiles=base\noverlays=work,personal\n",
    )
    .unwrap();
    assert_eq!(good.parents, "base");
    assert_eq!(good.overlays, "work,personal");
    for body in [
        b"overlays=work\n".as_slice(),
        b"version=2\n",
        b"version=1\nversion=1\n",
        b"version=1\nunknown=x\n",
        b"version=1\noverlays=dotfiles\n",
        b"version=1\nkey",
        b"version=1\noverlays=x\\\n",
    ] {
        assert!(profiles::parse_definition(path, body).is_err())
    }
    let d = TempDir::new("profile-selector").unwrap();
    file(d.path(), "base.conf", b"version=1\noverlays=base\n", 0o600);
    let mut state = State::default();
    state.load(Some(d.path()), "", "/home/test", None).unwrap();
    let selector = state
        .selector_parse(
            Path::new("selector.conf"),
            b"version=1\nuser=Chris\nhost=HOST.\nprofile=base\n",
            SelectorClass::Local,
        )
        .unwrap();
    assert_eq!(selector.user, "Chris");
    assert_eq!(selector.host, "host");
    for body in [
        b"profile=base\n".as_slice(),
        b"version=1\n",
        b"version=1\nprofile=missing\n",
        b"version=1\nprofile=base\n",
        b"version=1\nuser=bad/x\nprofile=base\n",
    ] {
        assert!(
            state
                .selector_parse(Path::new("bad.conf"), body, SelectorClass::Local)
                .is_err()
        )
    }
}

fn definitions() -> TempDir {
    let d = TempDir::new("profile-defs").unwrap();
    file(d.path(), "base.conf", b"version=1\noverlays=base\n", 0o600);
    file(
        d.path(),
        "work.conf",
        b"version=1\nprofiles=base\noverlays=work,base\n",
        0o600,
    );
    file(
        d.path(),
        "personal.conf",
        b"version=1\nprofiles=work\noverlays=personal\n",
        0o600,
    );
    d
}

#[test]
fn load_default_flatten_and_phase_one_preserve_order() {
    let d = definitions();
    let mut s = State::default();
    s.load(Some(d.path()), "", "/home/test", Some("work"))
        .unwrap();
    assert!(s.present);
    s.flatten("personal").unwrap();
    assert_eq!(s.included, ["base", "work", "personal"]);
    assert_eq!(s.overlay_names, ["base", "work", "personal"]);
    s.select_base().unwrap();
    assert_eq!(s.selected, "base");
    assert_eq!(s.selection_state, "phase-one");
    assert_eq!(s.overlay_names, ["base"]);
    assert!(s.flatten("missing").is_err());
}

#[test]
fn selectors_choose_specific_match_default_and_conflict() {
    let d = definitions();
    let root = TempDir::new("selectors-root").unwrap();
    file(
        root.path(),
        "10-default.conf",
        b"version=1\nprofile=work\n",
        0o600,
    );
    file(
        root.path(),
        "20-specific.conf",
        b"version=1\nuser=chris\nhost=nas\nprofile=personal\n",
        0o600,
    );
    let local = TempDir::new("selectors-local").unwrap();
    std::fs::set_permissions(local.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut s = State::default();
    s.load(Some(d.path()), "", "/home/test", Some("base"))
        .unwrap();
    s.resolve_with(root.path(), local.path(), &[], "chris", "NAS.", euid())
        .unwrap();
    assert_eq!(s.selected, "personal");
    assert_eq!(s.selection_state, "agreed-match");
    assert_eq!(s.overlay_names, ["base", "work", "personal"]);
    let empty = TempDir::new("selectors-empty").unwrap();
    let mut s = State::default();
    s.load(Some(d.path()), "", "/home/test", Some("work"))
        .unwrap();
    s.resolve_with(empty.path(), local.path(), &[], "chris", "nas", euid())
        .unwrap();
    assert_eq!(s.selected, "work");
    assert_eq!(s.selection_state, "implicit-default");
    file(
        local.path(),
        "one.conf",
        b"version=1\nuser=chris\nprofile=work\n",
        0o600,
    );
    file(
        local.path(),
        "two.conf",
        b"version=1\nuser=chris\nprofile=personal\n",
        0o600,
    );
    let mut s = State::default();
    s.load(Some(d.path()), "", "/home/test", None).unwrap();
    assert!(
        s.resolve_with(empty.path(), local.path(), &[], "chris", "nas", euid())
            .is_err()
    );
    assert_eq!(s.selection_state, "conflict");
}

#[test]
fn load_rejects_missing_base_cycles_bad_defaults_and_unsafe_local_selectors() {
    let d = TempDir::new("profiles-invalid").unwrap();
    file(d.path(), "work.conf", b"version=1\noverlays=work\n", 0o600);
    assert!(
        State::default()
            .load(Some(d.path()), "", "/home/test", None)
            .is_err()
    );
    file(
        d.path(),
        "base.conf",
        b"version=1\nprofiles=work\noverlays=base\n",
        0o600,
    );
    file(
        d.path(),
        "work.conf",
        b"version=1\nprofiles=base\noverlays=work\n",
        0o600,
    );
    assert!(
        State::default()
            .load(Some(d.path()), "", "/home/test", None)
            .is_err()
    );
    let d = definitions();
    assert!(
        State::default()
            .load(Some(d.path()), "", "/home/test", Some("missing"))
            .is_err()
    );
}

#[test]
fn parent_only_profiles_expand_and_invalid_definitions_fail_closed() {
    let d = TempDir::new("profile-expansion-errors").unwrap();
    file(
        d.path(),
        "base.conf",
        b"version=1\noverlays=personal\n",
        0o600,
    );
    file(
        d.path(),
        "parent-only.conf",
        b"version=1\nprofiles=base\n",
        0o600,
    );
    let mut state = State::default();
    state.load(Some(d.path()), "", "/home/test", None).unwrap();
    state.flatten("parent-only").unwrap();
    assert_eq!(state.included, ["base", "parent-only"]);
    assert_eq!(state.overlay_names, ["personal"]);

    for (name, files) in [
        (
            "unknown-parent",
            vec![("base.conf", "version=1\nprofiles=missing\n")],
        ),
        (
            "direct-cycle",
            vec![("base.conf", "version=1\nprofiles=base\n")],
        ),
        ("empty", vec![("base.conf", "version=1\n")]),
        (
            "explicit-empty",
            vec![("base.conf", "version=1\noverlays=\n")],
        ),
        (
            "invalid-overlay",
            vec![("base.conf", "version=1\noverlays=../personal\n")],
        ),
    ] {
        let invalid = TempDir::new(name).unwrap();
        for (path, body) in files {
            file(invalid.path(), path, body.as_bytes(), 0o600);
        }
        assert!(
            State::default()
                .load(Some(invalid.path()), "", "/home/test", None)
                .is_err(),
            "accepted {name}"
        );
    }
}

#[test]
fn selector_sources_enforce_local_modes_names_and_missing_personal_fallback() {
    let definitions = definitions();
    let root = TempDir::new("selector-security-root").unwrap();
    let local = TempDir::new("selector-security-local").unwrap();
    std::fs::set_permissions(local.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    file(
        local.path(),
        "bad|name.conf",
        b"version=1\nuser=chris\nprofile=base\n",
        0o600,
    );
    let mut state = State::default();
    state
        .load(Some(definitions.path()), "", "/home/test", None)
        .unwrap();
    assert!(
        state
            .resolve_with(root.path(), local.path(), &[], "chris", "host", euid())
            .is_err()
    );

    std::fs::remove_file(local.path().join("bad|name.conf")).unwrap();
    file(
        local.path(),
        "match.conf",
        b"version=1\nuser=chris\nprofile=work\n",
        0o660,
    );
    let mut state = State::default();
    state
        .load(Some(definitions.path()), "", "/home/test", None)
        .unwrap();
    assert!(
        state
            .resolve_with(root.path(), local.path(), &[], "chris", "host", euid())
            .is_err()
    );

    std::fs::remove_file(local.path().join("match.conf")).unwrap();
    let missing = local.path().join("missing-personal");
    let mut state = State::default();
    state
        .load(Some(definitions.path()), "", "/home/test", None)
        .unwrap();
    state
        .resolve_with(
            root.path(),
            local.path(),
            &[&missing],
            "chris",
            "host",
            euid(),
        )
        .unwrap();
    assert_eq!(state.selected, "base");
    assert_eq!(state.selection_state, "implicit-default");
}

#[test]
fn personal_selector_ancestry_rejects_symlinked_components() {
    for component in ["dot", "selector-dir", "selector-file"] {
        let fixture = TempDir::new(component).unwrap();
        let config = fixture.path().join("config");
        let checkout = fixture.path().join("personal");
        let external = fixture.path().join("external");
        std::fs::create_dir_all(config.join("dot")).unwrap();
        std::fs::create_dir_all(external.join("profile-selectors.d")).unwrap();
        file(
            &config.join("dot/profiles.d"),
            "base.conf",
            b"version=1\noverlays=personal\n",
            0o600,
        );
        file(
            &external.join("profile-selectors.d"),
            "match.conf",
            b"version=1\nuser=chris\nprofile=base\n",
            0o600,
        );
        std::fs::create_dir_all(&checkout).unwrap();
        match component {
            "dot" => std::os::unix::fs::symlink(&external, checkout.join("dot")).unwrap(),
            "selector-dir" => {
                std::fs::create_dir(checkout.join("dot")).unwrap();
                std::os::unix::fs::symlink(
                    external.join("profile-selectors.d"),
                    checkout.join("dot/profile-selectors.d"),
                )
                .unwrap();
            }
            _ => {
                std::fs::create_dir_all(checkout.join("dot/profile-selectors.d")).unwrap();
                std::os::unix::fs::symlink(
                    external.join("profile-selectors.d/match.conf"),
                    checkout.join("dot/profile-selectors.d/match.conf"),
                )
                .unwrap();
            }
        }
        let mut state = State::default();
        state
            .load(
                Some(&config.join("dot/profiles.d")),
                config.to_str().unwrap(),
                fixture.path().to_str().unwrap(),
                None,
            )
            .unwrap();
        let overlay = format!(
            "personal|{}|https://example.invalid/personal.git|config|true|git",
            checkout.display()
        );
        assert!(
            state
                .resolve_default(
                    config.to_str().unwrap(),
                    fixture.path().to_str().unwrap(),
                    &[&overlay],
                    "chris",
                    "host",
                    euid()
                )
                .is_err(),
            "accepted symlinked {component}"
        );
    }
}

#[test]
fn checked_in_profile_example_resolves_combined_identity_natively() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/profile-dotfiles");
    let definitions = root.join("root/.config/dot/profiles.d");
    let selectors = root.join("root/.config/dot/profile-selectors.d");
    let local = TempDir::new("profile-example-local").unwrap();
    std::fs::set_permissions(local.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let combined = std::fs::read(root.join("local/combined.conf")).unwrap();
    file(local.path(), "combined.conf", &combined, 0o600);

    let mut state = State::default();
    state
        .load(Some(&definitions), "", "/home/example", None)
        .unwrap();
    state
        .resolve_with(
            &selectors,
            local.path(),
            &[],
            "example-user",
            "Example-Host.",
            euid(),
        )
        .unwrap();
    assert_eq!(state.selected, "dev");
    assert_eq!(state.included, ["base", "editor", "dev"]);
    assert_eq!(state.overlay_names, ["personal", "nvim", "dev", "work"]);

    for relative in [
        "README.md",
        "root/.config/dot/overlays.d/20-nvim.conf",
        "root/.config/dot/overlays.d/30-dev.conf",
        "root/.config/dot/overlays.d/80-personal.conf",
        "root/.config/dot/overlays.d/90-work.conf",
        "personal/dot/profile-selectors.d/dev.conf",
    ] {
        assert!(
            root.join(relative).is_file(),
            "missing profile example {relative}"
        );
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let configuration = std::fs::read_to_string(repo.join("docs/configuration.md")).unwrap();
    assert!(configuration.contains("profile-selectors.local.d"));
    let overlays = std::fs::read_to_string(repo.join("docs/overlays.md")).unwrap();
    for term in [
        "selected",
        "eligible",
        "active",
        "inspect validated existing state only",
    ] {
        assert!(overlays.contains(term), "overlay docs lost `{term}`");
    }
}
