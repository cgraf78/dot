//! Platform and host predicates.
//!
//! Owns WSL detection (env vars or a
//! case-insensitive `microsoft` in the kernel osrelease), `uname -s`
//! platform names (`darwin` canonicalized to `macos`), short-hostname
//! detection (both read from the kernel through libc, not by spawning
//! `uname` and `hostname`), comma-spec matching with `!` exclusions, and
//! Termux's dual `linux`+`android` identity. Lowercasing is ASCII-only,
//! matching the shell's `${var,,}` under the C locale the engine pins.
//!
//! The shell reports wrong arity with exit 2; Rust surfaces the same
//! split as [`Error`] so callers map to identical exit codes.

/// Platform failure, mirroring the shell exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity or malformed value (shell exit 2).
    Usage,
    /// Detection failed: `uname(2)`/`gethostname(2)` failed (shell exit 1).
    Unavailable,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Unavailable => 1,
        }
    }
}

/// Whether the host is WSL: either WSL env marker is non-empty (an
/// empty value counts as unset, like the shell's `-n` test) or the
/// kernel osrelease mentions `microsoft` case-insensitively
/// (`grep -qi`, so any line counts).
pub fn is_wsl(distro: &str, interop: &str, osrelease: Option<&str>) -> bool {
    if !distro.is_empty() || !interop.is_empty() {
        return true;
    }
    osrelease.is_some_and(|content| content.to_ascii_lowercase().contains("microsoft"))
}

/// Canonical platform name from a `uname -s` value: WSL wins outright,
/// otherwise ASCII-lowercase with `darwin` folded to `macos`.
pub fn platform_name(uname_s: &str, wsl: bool) -> String {
    if wsl {
        return "wsl".to_string();
    }
    let lowered = uname_s.to_ascii_lowercase();
    if lowered == "darwin" {
        "macos".to_string()
    } else {
        lowered
    }
}

/// Memoized platform/hostname detection. `dot update` detects
/// each ~4x (CLI dispatch plus engine phases), and neither answer
/// can change mid-process: the kernel, hostname, and WSL markers
/// are immutable for the run's lifetime (production code never
/// mutates the environment). Only successful detections memoize;
/// a failed probe re-probes rather than pinning `Unavailable`.
static PLATFORM_MEMO: crate::memo::Memo<String> = crate::memo::Memo::new();
static HOST_MEMO: crate::memo::Memo<String> = crate::memo::Memo::new();

/// The kernel name `uname -s` prints: `uname(2)`'s `sysname`.
///
/// Read through libc instead of spawning `uname`: every supervised
/// child costs a fork plus, for a strict session, a host-wide
/// process-table walk, and `dot doctor` and `dot update` both ask on
/// every run. The system call is what the `uname` binary itself
/// reports, so the answer is unchanged, and it also works where no
/// `uname` is on `PATH`.
pub fn kernel_name() -> Option<String> {
    // SAFETY: `utsname` is plain old data; zeroed is a valid value
    // and `uname` only writes into the struct it is handed.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return None;
    }
    let value = c_field(&name.sysname);
    (!value.is_empty()).then_some(value)
}

/// The short host name `hostname -s` prints: `gethostname(2)` cut
/// at the first dot.
///
/// No resolver lookup is involved: `hostname -s` in the Debian
/// `hostname` package (the Linux distributions' `hostname`), BSD and
/// macOS `hostname`, and BusyBox all cut the kernel host name at the
/// first dot. Like [`kernel_name`], the system call replaces a child
/// process on every run and keeps working where no `hostname` binary
/// is installed (minimal container images).
pub fn short_hostname() -> Option<String> {
    // Host names are at most 255 bytes on Linux and macOS
    // (HOST_NAME_MAX); the spare byte guarantees a terminator.
    let mut buffer = [0 as libc::c_char; 257];
    // SAFETY: the pointer and length describe the live local buffer,
    // and the length leaves the final byte as a terminator.
    if unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) } != 0 {
        return None;
    }
    let full = c_field(&buffer);
    Some(match full.split_once('.') {
        Some((short, _)) => short.to_string(),
        None => full,
    })
}

/// A NUL-terminated C character field as text, lossily like the
/// subprocess output it replaces.
fn c_field(field: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Detect the live platform: WSL markers from the environment plus
/// `/proc/sys/kernel/osrelease` when readable, then the kernel name.
pub fn detect_platform() -> Result<String, Error> {
    detect_inner(&PLATFORM_MEMO, detect_platform_uncached).ok_or(Error::Unavailable)
}

fn detect_inner(
    memo: &crate::memo::Memo<String>,
    probe: impl FnOnce() -> Result<String, Error>,
) -> Option<String> {
    memo.get_or_probe(|| probe().ok())
}

fn detect_platform_uncached() -> Result<String, Error> {
    let distro = std::env::var("WSL_DISTRO_NAME").unwrap_or_default();
    let interop = std::env::var("WSL_INTEROP").unwrap_or_default();
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let kernel = kernel_name().ok_or(Error::Unavailable)?;
    Ok(platform_name(
        &kernel,
        is_wsl(&distro, &interop, osrelease.as_deref()),
    ))
}

/// Short hostname canonicalization: ASCII-lowercase, like the shell's
/// `${value,,}` under the C locale.
pub fn host_name(raw: &str) -> String {
    raw.to_ascii_lowercase()
}

/// Detect the live short hostname (what `hostname -s` prints); see
/// [`short_hostname`].
pub fn detect_host() -> Result<String, Error> {
    detect_inner(&HOST_MEMO, detect_host_uncached).ok_or(Error::Unavailable)
}

fn detect_host_uncached() -> Result<String, Error> {
    short_hostname()
        .map(|short| host_name(&short))
        .ok_or(Error::Unavailable)
}

/// Match a comma spec against current values (`_dot_match_specs`).
///
/// An empty spec matches everything. Only the first line participates:
/// the shell splits with `read -a`, which stops at the first newline,
/// so everything from `\n` on is invisible. Empty items are skipped
/// (so a trailing comma changes nothing). Items starting with `!` are
/// exclusions checked FIRST — and, like inclusion, compared LITERALLY:
/// both right-hand sides sit inside double quotes
/// (`[[ $normalized == "!$current" ]]`), so an expansion there never
/// acts as a pattern even when a current value carries glob
/// metacharacters (only a bare `$var` would). A spec with no inclusion
/// items matches unless excluded; otherwise at least one inclusion
/// must equal a current value. `lowercase` lowercases each item
/// (ASCII, C-locale `${item,,}`) before comparing.
pub fn match_specs(spec: &str, lowercase: bool, currents: &[&str]) -> bool {
    // `read -a` consumes one line; later lines never become items.
    let spec = spec.split('\n').next().unwrap_or("");
    if spec.is_empty() {
        return true;
    }
    let fold = |item: &str| {
        if lowercase {
            item.to_ascii_lowercase()
        } else {
            item.to_string()
        }
    };
    let mut has_include = false;
    for item in spec.split(',') {
        if item.is_empty() {
            continue;
        }
        let normalized = fold(item);
        if !normalized.starts_with('!') {
            has_include = true;
        }
        // Exclusion: literal `!`-prefixed equality (the quotes make
        // even metachar values inert).
        for current in currents {
            if normalized
                .strip_prefix('!')
                .is_some_and(|tail| tail == *current)
            {
                return false;
            }
        }
    }
    if !has_include {
        return true;
    }
    for item in spec.split(',') {
        if item.is_empty() {
            continue;
        }
        let normalized = fold(item);
        // Inclusion: `[[ $item == "$current" ]]`, quoted, so literal.
        if currents.iter().any(|current| normalized == *current) {
            return true;
        }
    }
    false
}

/// Match a platform spec: exactly one spec string (anything else is a
/// usage error, shell exit 2). Termux keeps both durable identities —
/// the kernel platform plus `android` — matching the provider ABI.
pub fn platform_matches(spec: Option<&str>, platform: &str, termux: bool) -> Result<bool, Error> {
    let spec = spec.ok_or(Error::Usage)?;
    if termux {
        Ok(match_specs(spec, false, &[platform, "android"]))
    } else {
        Ok(match_specs(spec, false, &[platform]))
    }
}

/// Match a host spec: exactly one spec string, compared lowercased.
pub fn host_matches(spec: Option<&str>, host: &str) -> Result<bool, Error> {
    let spec = spec.ok_or(Error::Usage)?;
    Ok(match_specs(spec, true, &[host]))
}

/// Whether `PREFIX` selects the Termux dual identity: non-empty and
/// containing `/com.termux/`, exactly the hook runtime's
/// `[[ -n ${PREFIX:-} && $PREFIX == */com.termux/* ]]` (`*` matches
/// `/` in bash globs, so containment is the whole test).
pub fn is_termux(prefix: &str) -> bool {
    !prefix.is_empty() && prefix.contains("/com.termux/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_markers_and_osrelease() {
        assert!(is_wsl("Ubuntu", "", None));
        assert!(is_wsl("", "x", None));
        // Empty counts as unset.
        assert!(!is_wsl("", "", None));
        assert!(is_wsl("", "", Some("5.15.90.1-microsoft-standard-WSL2\n")));
        assert!(is_wsl("", "", Some("MICROSOFT x86_64")));
        assert!(!is_wsl("", "", Some("6.1.0-18-amd64\n")));
        assert!(!is_wsl("", "", None));
    }

    #[test]
    fn platform_names_fold_darwin() {
        assert_eq!(platform_name("Linux", false), "linux");
        assert_eq!(platform_name("Darwin", false), "macos");
        assert_eq!(platform_name("DARWIN", false), "macos");
        assert_eq!(platform_name("FreeBSD", false), "freebsd");
        assert_eq!(platform_name("Linux", true), "wsl");
        assert_eq!(platform_name("anything", true), "wsl");
    }

    #[test]
    fn spec_matrix() {
        // Empty spec matches everything, even with no currents.
        assert!(match_specs("", false, &[]));
        assert!(match_specs("", false, &["linux"]));
        // Inclusion.
        assert!(match_specs("linux", false, &["linux"]));
        assert!(!match_specs("linux", false, &["macos"]));
        assert!(match_specs("macos,linux", false, &["linux"]));
        // Trailing/empty items change nothing.
        assert!(match_specs("linux,", false, &["linux"]));
        assert!(match_specs(",linux", false, &["linux"]));
        // Only separators: every item is empty, so (like the shell's
        // `read -a` split) the spec behaves as if it had no items.
        assert!(match_specs(",", false, &["linux"]));
        // Exclusion-only specs match unless excluded.
        assert!(match_specs("!macos", false, &["linux"]));
        assert!(!match_specs("!linux", false, &["linux"]));
        // Exclusions win over inclusions.
        assert!(!match_specs("linux,!linux", false, &["linux"]));
        assert!(match_specs("linux,!macos", false, &["linux"]));
        // `!` alone excludes nothing and includes nothing.
        assert!(match_specs("!", false, &["!"]));
        // Only the first line is read; the rest is invisible.
        assert!(match_specs("linux\nevil", false, &["linux"]));
        assert!(!match_specs("nomatch\nlinux", false, &["linux"]));
        assert!(match_specs("\nlinux", false, &["linux"]));
        // Case modes.
        assert!(!match_specs("LINUX", false, &["linux"]));
        assert!(match_specs("LINUX", true, &["linux"]));
        assert!(!match_specs("!LINUX", true, &["linux"]));
        // No currents: inclusions fail, pure exclusions pass.
        assert!(!match_specs("linux", false, &[]));
        assert!(match_specs("!linux", false, &[]));
    }

    #[test]
    fn exclusion_values_are_literal() {
        // Both right-hand sides sit inside double quotes, so a current
        // value carrying glob metacharacters never acts as a pattern
        // (adversarial review caught this inverted).
        assert!(match_specs("!anything", false, &["*"]));
        assert!(!match_specs("!*", false, &["*"]));
        assert!(match_specs("!linux", false, &["lin*"]));
        assert!(!match_specs("!lin*", false, &["lin*"]));
        assert!(!match_specs("other", false, &["*"]));
    }

    #[test]
    fn inclusion_is_literal() {
        // The inclusion side is quoted in the shell: `*` never globs.
        assert!(!match_specs("*", false, &["linux"]));
        assert!(match_specs("*", false, &["*"]));
    }

    #[test]
    fn arity_errors() {
        assert_eq!(platform_matches(None, "linux", false), Err(Error::Usage));
        assert_eq!(host_matches(None, "h"), Err(Error::Usage));
        assert_eq!(Error::Usage.code(), 2);
        assert_eq!(Error::Unavailable.code(), 1);
    }

    #[test]
    fn termux_adds_android_identity() {
        assert_eq!(platform_matches(Some("android"), "linux", true), Ok(true));
        assert_eq!(platform_matches(Some("android"), "linux", false), Ok(false));
        assert_eq!(platform_matches(Some("linux"), "linux", true), Ok(true));
        assert!(is_termux("/data/data/com.termux/files/usr"));
        assert!(!is_termux("/usr"));
        assert!(!is_termux(""));
    }

    #[test]
    fn detection_memoizes_successes_and_reprobes_failures() {
        use std::cell::Cell;
        let probes = Cell::new(0);
        let memo = crate::memo::Memo::new();
        let probe = || {
            probes.set(probes.get() + 1);
            Ok("linux".to_string())
        };
        assert_eq!(detect_inner(&memo, probe), Some("linux".to_string()));
        let probe = || {
            probes.set(probes.get() + 1);
            Ok("changed".to_string())
        };
        assert_eq!(detect_inner(&memo, probe), Some("linux".to_string()));
        assert_eq!(probes.get(), 1);
        let failing = crate::memo::Memo::new();
        for _ in 0..2 {
            assert_eq!(detect_inner(&failing, || Err(Error::Unavailable)), None);
        }
    }
}
