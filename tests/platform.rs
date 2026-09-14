//! Native contracts for platform, host, specification, tool, and sudo policy.

use dot::platform;

#[test]
fn live_platform_and_host_are_canonical() {
    let platform = platform::detect_platform().expect("platform");
    assert!(!platform.is_empty());
    assert_eq!(platform, platform.to_ascii_lowercase());
    let host = platform::detect_host().expect("host");
    assert!(!host.is_empty());
    assert_eq!(host, host.to_ascii_lowercase());
}

#[test]
fn wsl_markers_override_kernel_platform() {
    assert!(platform::is_wsl("Ubuntu", "", None));
    assert!(platform::is_wsl("", "x", None));
    assert!(platform::is_wsl("", "", Some("microsoft-standard-WSL2")));
    assert!(!platform::is_wsl("", "", Some("6.1.0-amd64")));
    assert_eq!(platform::platform_name("Linux", true), "wsl");
    assert_eq!(platform::platform_name("Darwin", false), "macos");
}

#[test]
fn platform_specs_include_exclude_and_add_termux_identity() {
    use platform::Error;
    for (spec, expected) in [
        ("", true),
        (",", true),
        ("linux", true),
        ("macos,linux", true),
        ("nomatch", false),
        ("!nomatch", true),
        ("!linux", false),
        ("linux,!linux", false),
        ("!", true),
        ("*,!*", false),
        ("LINUX", false),
        ("*", false),
    ] {
        assert_eq!(
            platform::platform_matches(Some(spec), "linux", false),
            Ok(expected),
            "{spec:?}"
        );
    }
    assert_eq!(
        platform::platform_matches(Some("android"), "linux", true),
        Ok(true)
    );
    assert_eq!(
        platform::platform_matches(Some("android"), "linux", false),
        Ok(false)
    );
    assert_eq!(
        platform::platform_matches(None, "linux", false),
        Err(Error::Usage)
    );
}

#[test]
fn raw_specs_treat_metacharacters_and_newlines_literally() {
    for (spec, lowercase, currents, expected) in [
        ("!anything", false, &["*"][..], true),
        ("!*", false, &["*"][..], false),
        ("!linux", false, &["lin*"][..], true),
        ("!lin*", false, &["lin*"][..], false),
        ("linux", false, &["lin*"][..], false),
        ("lin*", false, &["linux"][..], false),
        ("[!a]", false, &["b"][..], false),
        ("[!a]", false, &["[", "a", "]"][..], false),
        ("?", false, &["?"][..], true),
        ("?", false, &["x"][..], false),
        ("?", false, &["??"][..], false),
        ("LINUX,!other", true, &["linux"][..], true),
        ("!LINUX", true, &["linux"][..], false),
        ("a,b", false, &["b", "c"][..], true),
        ("a,!b", false, &["a", "b"][..], false),
        ("", false, &["x"][..], true),
        ("*,!*", false, &["anything"][..], false),
        ("linux\nevil", false, &["linux"][..], true),
        ("nomatch\nlinux", false, &["linux"][..], false),
    ] {
        assert_eq!(
            platform::match_specs(spec, lowercase, currents),
            expected,
            "{spec:?}"
        );
    }
}

#[test]
fn host_specs_are_ascii_case_insensitive() {
    assert_eq!(platform::host_matches(Some(""), "host-a"), Ok(true));
    assert_eq!(platform::host_matches(Some("HOST-A"), "host-a"), Ok(true));
    assert_eq!(
        platform::host_matches(Some("other,HOST-A"), "host-a"),
        Ok(true)
    );
    assert_eq!(platform::host_matches(Some("!HOST-A"), "host-a"), Ok(false));
    assert_eq!(platform::host_matches(Some("!other"), "host-a"), Ok(true));
    assert_eq!(
        platform::host_matches(None, "host-a"),
        Err(platform::Error::Usage)
    );
}

#[test]
fn tool_lookup_distinguishes_path_names_and_direct_paths() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dot_test_support::TempDir::new("tool-contract").expect("temp");
    let executable = dir.write("tool", b"#!/bin/sh\n");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir.write("plain", b"x");
    std::fs::create_dir(dir.path().join("directory")).unwrap();
    let path = dir.path().to_string_lossy();
    assert_eq!(platform::tool_present(Some("tool"), &path), Ok(true));
    assert_eq!(platform::tool_present(Some("plain"), &path), Ok(true));
    assert_eq!(platform::tool_present(Some("directory"), &path), Ok(false));
    assert_eq!(platform::tool_present(Some("missing"), &path), Ok(false));
    assert_eq!(
        platform::tool_present(Some(executable.to_str().unwrap()), ""),
        Ok(true)
    );
    assert_eq!(
        platform::tool_present(Some(dir.path().join("missing").to_str().unwrap()), ""),
        Ok(false)
    );
    assert_eq!(
        platform::tool_present(Some(dir.path().join("directory").to_str().unwrap()), ""),
        Ok(true)
    );
    assert_eq!(
        platform::tool_present(None, &path),
        Err(platform::Error::Usage)
    );
}

#[test]
fn sudo_ladder_has_fixed_precedence() {
    let yes = || true;
    let no = || false;
    assert!(platform::decide_sudo(true, false, true, &no));
    assert!(platform::decide_sudo(false, true, true, &no));
    assert!(!platform::decide_sudo(false, false, true, &yes));
    assert!(platform::decide_sudo(false, false, false, &yes));
    assert!(!platform::decide_sudo(false, false, false, &no));
}
