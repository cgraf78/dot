//! Build identity for `dot version`.
//!
//! The shell `dot_version()` prints `dot commit <short12|unknown>
//! (config 1; extensions 1; library 1)`. The revision comes from
//! `build.rs` (`DOT_BUILD_COMMIT`); `unknown` is a first-class value,
//! never a build failure.

/// Full commit SHA from the build, or `unknown`.
pub const COMMIT: &str = env!("DOT_BUILD_COMMIT");
/// 12-char lowercase short revision, or `unknown`.
pub const SHORT_COMMIT: &str = env!("DOT_BUILD_SHORT_COMMIT");
/// Release version stamp, or `unknown` until the shared
/// `YYYYMMDD-HHMMSS-8hex` scheme lands in a later slice.
pub const VERSION: &str = env!("DOT_BUILD_VERSION");
/// Public standalone-dot library ABI (`DOT_LIBRARY_API` in
/// `lib/dot/public/api-version.sh`). Consumers check this before
/// relying on any exported function.
pub const LIBRARY_API: u32 = 1;

/// The exact `dot version` output line (without trailing newline).
pub fn version_line() -> String {
    format!("dot commit {SHORT_COMMIT} (config 1; extensions 1; library 1)")
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
    fn revision_is_hex_or_unknown() {
        assert!(
            SHORT_COMMIT == "unknown"
                || (SHORT_COMMIT.len() == 12
                    && SHORT_COMMIT.chars().all(|c| c.is_ascii_hexdigit())),
            "unexpected SHORT_COMMIT: {SHORT_COMMIT}"
        );
    }

    #[test]
    fn version_line_shape_matches_shell_contract() {
        let line = version_line();
        assert!(line.starts_with("dot commit "), "line: {line}");
        assert!(
            line.ends_with(" (config 1; extensions 1; library 1)"),
            "line: {line}"
        );
        let rev = line
            .strip_prefix("dot commit ")
            .and_then(|rest| rest.strip_suffix(" (config 1; extensions 1; library 1)"))
            .expect("line has revision field");
        assert!(
            rev == "unknown" || (rev.len() == 12 && rev.chars().all(|c| c.is_ascii_hexdigit())),
            "revision field: {rev}"
        );
    }
}
