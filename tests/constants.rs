//! Native behavioral tests for runtime constant resolution.

use dot::constants::resolve;
use dot::version::LIBRARY_API;

#[test]
fn constants_follow_xdg_and_environment_policy() {
    let home = "/home/fixture-user";
    let source_root = "/opt/dot-checkout";
    // (xdg_state, quiet, verbose); `None` means unset.
    struct Case {
        xdg_state: Option<&'static str>,
        quiet: Option<&'static str>,
        verbose: Option<&'static str>,
    }
    let cases = [
        Case {
            xdg_state: Some("/x/state"),
            quiet: Some("1"),
            verbose: Some("2"),
        },
        Case {
            xdg_state: Some("/x/state"),
            quiet: None,
            verbose: None,
        },
        Case {
            xdg_state: None,
            quiet: None,
            verbose: None,
        },
        Case {
            xdg_state: None,
            quiet: Some(""),
            verbose: Some("0"),
        },
        // Relative XDG values fall back to HOME on both sides.
        Case {
            xdg_state: Some("relative/state"),
            quiet: None,
            verbose: None,
        },
        Case {
            xdg_state: Some(""),
            quiet: None,
            verbose: None,
        },
    ];
    for case in &cases {
        let (xdg_state, quiet, verbose) = (case.xdg_state, case.quiet, case.verbose);
        let rust = resolve(home, xdg_state.unwrap_or(""), source_root, quiet, verbose)
            .expect("resolvable");
        let state = xdg_state
            .filter(|value| value.starts_with('/'))
            .unwrap_or("/home/fixture-user/.local/state");
        assert_eq!(rust.overlay_manifest, format!("{state}/dot/overlay-links"));
        assert_eq!(
            rust.overlay_legacy_manifest,
            "/home/fixture-user/.local/state/dot/overlay-links"
        );
        assert_eq!(
            rust.profile_lifecycle_ledger,
            format!("{state}/dot/profile-overlay-lifecycle-v1")
        );
        assert_eq!(rust.bin, "/opt/dot-checkout/bin/dot");
        assert_eq!(
            rust.quiet,
            quiet.filter(|value| !value.is_empty()).unwrap_or("0")
        );
        assert_eq!(
            rust.verbose,
            verbose.filter(|value| !value.is_empty()).unwrap_or("0")
        );
        assert_eq!(LIBRARY_API, 1);
    }
}

#[test]
fn unresolvable_home_is_rejected() {
    assert!(resolve("relative", "", "/src", None, None).is_err());
}
