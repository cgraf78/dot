//! Build identity for `dot version`.
//!
//! The version contract follows the shared Rust-repo policy: report the
//! same generated version string used by release tags, archive names, and
//! installer metadata. (The historical shell `dot_version()` printed
//! `dot commit <short12|unknown> ...`; the native implementation and its
//! tests now own this behavior.) The config/extensions/library payload
//! advertises the CLI surface the binary implements.

/// Full commit SHA from the build. Always concrete: the build fails
/// without a resolvable commit instead of baking in `unknown`.
pub const COMMIT: &str = env!("DOT_BUILD_COMMIT");
/// Public `YYYYMMDD-HHMMSS-<8hex>` version stamp; the hash suffix matches
/// the commit. Always concrete: the build fails without one.
pub const VERSION: &str = env!("DOT_BUILD_VERSION");
/// Public standalone-dot library ABI (`DOT_LIBRARY_API` in
/// `lib/dot/public/api-version.sh`). Consumers check this before
/// relying on any exported function.
pub const LIBRARY_API: u32 = 1;

/// The exact `dot version` output line (without trailing newline).
pub fn version_line() -> String {
    format!("dot {VERSION} (config 1; extensions 1; library 1)")
}

/// Crate-level description for logs and diagnostics.
pub fn description() -> String {
    format!("dot {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_api_is_one() {
        // `lib/dot/public/api-version.sh` exports `DOT_LIBRARY_API=1`;
        // the differential test in `tests/constants.rs` pins the two
        // together so the ABI can never drift silently.
        assert_eq!(LIBRARY_API, 1);
    }

    #[test]
    fn embedded_commit_is_concrete() {
        assert!(COMMIT.len() >= 8);
        assert!(COMMIT.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(COMMIT, "unknown");
    }

    #[test]
    fn public_version_is_readable_and_traceable() {
        let parts = VERSION.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 6);
        assert_eq!(parts[2].len(), 8);
        assert!(parts[0].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(parts[1].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(parts[2].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(parts[2], &COMMIT[..8]);
        assert_ne!(VERSION, "unknown");
    }

    #[test]
    fn version_line_uses_public_version() {
        assert_eq!(
            version_line(),
            format!("dot {VERSION} (config 1; extensions 1; library 1)")
        );
    }
}
