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

/// The libc identity readers must answer what the binaries they replaced
/// print on this host, so profile and platform selection do not move.
#[test]
fn libc_identity_matches_the_binaries_it_replaced() {
    let printed = |program: &str, args: &[&str]| {
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        Some(
            String::from_utf8_lossy(&output.stdout)
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        )
    };
    let kernel = platform::kernel_name().expect("kernel name");
    assert_eq!(Some(kernel), printed("uname", &["-s"]));
    // `uname -n` prints the kernel host name; `hostname -s` cuts that same
    // name at its first dot (some `hostname` builds consult the resolver
    // instead, so they are not a reliable oracle).
    let short = platform::short_hostname().expect("short host name");
    assert!(!short.contains('.'), "{short}");
    let full = printed("uname", &["-n"]).expect("uname -n");
    assert_eq!(full.split('.').next(), Some(short.as_str()));
    assert_eq!(
        platform::detect_host().expect("host"),
        short.to_ascii_lowercase()
    );
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

/// The shell hook API reads the host the way the engine does, so a hook's
/// `dot_hook_host_match` agrees with profile selection.
#[test]
fn hook_api_host_matches_the_engine_host() {
    let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("lib/dot/public/hook-runtime-v1/hook-api.sh");
    let output = std::process::Command::new(dot_test_support::bash())
        .args(["--noprofile", "--norc", "-c"])
        .arg(format!(". '{}' && _dot_hook_host", runtime.display()))
        .env("LC_ALL", "C")
        .output()
        .expect("bash");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        platform::detect_host().expect("host")
    );
}
