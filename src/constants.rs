//! Private runtime constants (slice 2 foundations).
//!
//! Ports `lib/dot/constants.sh` exactly: the XDG-derived overlay
//! manifest and profile lifecycle ledger, the legacy manifest pinned
//! to `$HOME/.local/state` (it deliberately ignores XDG), the engine
//! binary resolved through the checked-out source root (never PATH,
//! where a client launcher may own the same name), and the verbatim
//! `DOT_QUIET`/`DOT_VERBOSE` passthroughs (the shell applies `:-0`
//! defaults with no validation, so these stay strings here too).
//!
//! Pure resolution over explicit inputs; a failed XDG state lookup
//! aborts the whole set, like the shell's `|| return`.

use crate::xdg;

/// Resolved runtime constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constants {
    /// `<xdg-state>/dot/overlay-links`.
    pub overlay_manifest: String,
    /// `$HOME/.local/state/dot/overlay-links` (legacy, XDG-independent).
    pub overlay_legacy_manifest: String,
    /// `<xdg-state>/dot/profile-overlay-lifecycle-v1`.
    pub profile_lifecycle_ledger: String,
    /// `$DOT_SOURCE_ROOT/bin/dot`.
    pub bin: String,
    /// Verbatim `${DOT_QUIET:-0}`.
    pub quiet: String,
    /// Verbatim `${DOT_VERBOSE:-0}`.
    pub verbose: String,
}

/// Resolve every constant. `xdg_state_home` is the raw
/// `$XDG_STATE_HOME` value (empty counts as unset, like the shell);
/// `home` must be absolute for the fallback, exactly as in
/// [`xdg::base`].
pub fn resolve(
    home: &str,
    xdg_state_home: &str,
    source_root: &str,
    quiet: Option<&str>,
    verbose: Option<&str>,
) -> Result<Constants, xdg::Error> {
    let overlay_manifest = xdg::path(xdg::Kind::State, "dot/overlay-links", xdg_state_home, home)?;
    let profile_lifecycle_ledger = xdg::path(
        xdg::Kind::State,
        "dot/profile-overlay-lifecycle-v1",
        xdg_state_home,
        home,
    )?;
    Ok(Constants {
        overlay_manifest,
        overlay_legacy_manifest: format!("{home}/.local/state/dot/overlay-links"),
        profile_lifecycle_ledger,
        bin: format!("{source_root}/bin/dot"),
        // `${VAR:-0}` substitutes when unset OR empty.
        quiet: quiet
            .filter(|value| !value.is_empty())
            .unwrap_or("0")
            .to_string(),
        verbose: verbose
            .filter(|value| !value.is_empty())
            .unwrap_or("0")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_against_xdg_state() {
        let constants = resolve("/home/u", "/x/state", "/src", None, None).expect("resolve");
        assert_eq!(constants.overlay_manifest, "/x/state/dot/overlay-links");
        assert_eq!(
            constants.profile_lifecycle_ledger,
            "/x/state/dot/profile-overlay-lifecycle-v1"
        );
        assert_eq!(
            constants.overlay_legacy_manifest,
            "/home/u/.local/state/dot/overlay-links"
        );
        assert_eq!(constants.bin, "/src/bin/dot");
        assert_eq!(constants.quiet, "0");
        assert_eq!(constants.verbose, "0");
    }

    #[test]
    fn home_fallback_and_verbatim_flags() {
        let constants = resolve("/home/u", "", "/src", Some("1"), Some("2")).expect("resolve");
        assert_eq!(
            constants.overlay_manifest,
            "/home/u/.local/state/dot/overlay-links"
        );
        assert_eq!(constants.quiet, "1");
        assert_eq!(constants.verbose, "2");
    }

    #[test]
    fn xdg_failure_aborts_everything() {
        // No absolute HOME to fall back to: the shell's `|| return`
        // yields no constants at all.
        assert!(resolve("relative", "", "/src", None, None).is_err());
    }
}
