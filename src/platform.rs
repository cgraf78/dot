//! Platform, host, and executable predicates (slice 2 foundations).
//!
//! Ports `lib/dot/platform.sh` exactly: WSL detection (env vars or a
//! case-insensitive `microsoft` in the kernel osrelease), `uname -s`
//! platform names (`darwin` canonicalized to `macos`), short-hostname
//! detection, comma-spec matching with `!` exclusions, Termux's dual
//! `linux`+`android` identity, slash-vs-PATH tool lookup, and the
//! sudo escalation ladder. Lowercasing is ASCII-only, matching the
//! shell's `${var,,}` under the C locale the engine pins.
//!
//! The shell reports wrong arity with exit 2; Rust surfaces the same
//! split as [`Error`] so callers map to identical exit codes.

use std::path::Path;

/// Platform failure, mirroring the shell exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity or malformed value (shell exit 2).
    Usage,
    /// Detection failed: `uname`/`hostname` unusable (shell exit 1).
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

/// Detect the live platform: WSL markers from the environment plus
/// `/proc/sys/kernel/osrelease` when readable, then `uname -s`.
pub fn detect_platform() -> Result<String, Error> {
    let distro = std::env::var("WSL_DISTRO_NAME").unwrap_or_default();
    let interop = std::env::var("WSL_INTEROP").unwrap_or_default();
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let output = std::process::Command::new("uname")
        .arg("-s")
        .output()
        .map_err(|_| Error::Unavailable)?;
    if !output.status.success() {
        return Err(Error::Unavailable);
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(platform_name(
        raw.trim_end_matches(['\r', '\n']),
        is_wsl(&distro, &interop, osrelease.as_deref()),
    ))
}

/// Short hostname canonicalization: ASCII-lowercase, like the shell's
/// `${value,,}` under the C locale.
pub fn host_name(raw: &str) -> String {
    raw.to_ascii_lowercase()
}

/// Detect the live short hostname: `hostname -s`, falling back to
/// plain `hostname` exactly like the shell's `||` chain.
pub fn detect_host() -> Result<String, Error> {
    for args in [&["-s"][..], &[][..]] {
        // No let-chains: the crate MSRV is 1.85 and let-chains need
        // 1.88. Same for the other two sites like this one.
        let output = match std::process::Command::new("hostname").args(args).output() {
            Ok(output) => output,
            Err(_) => continue,
        };
        if !output.status.success() {
            continue;
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        return Ok(host_name(raw.trim_end_matches(['\r', '\n'])));
    }
    Err(Error::Unavailable)
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

/// Whether a command name resolves (`_dot_tool_present`).
///
/// Exactly one non-empty name (anything else is exit 2). A name
/// containing `/` is an existence probe (`[[ -e ]]`, following
/// symlinks); otherwise each colon-separated `path_dirs` entry is
/// searched the way the shell's `command -v` searches for external
/// commands: the first stat-able non-directory wins — executability
/// is NOT required (a `644` file on PATH satisfies `command -v`;
/// pinned live against bash). Shell builtins and functions also
/// satisfy `command -v`; that lookup is intentionally out of contract
/// — engine callers pass external tool names. Empty PATH entries are
/// skipped, matching `find_gum` convention.
pub fn tool_present(name: Option<&str>, path_dirs: &str) -> Result<bool, Error> {
    let name = match name {
        Some(name) if !name.is_empty() => name,
        _ => return Err(Error::Usage),
    };
    if name.contains('/') {
        return Ok(Path::new(name).exists());
    }
    Ok(path_dirs
        .split(':')
        .any(|dir| !dir.is_empty() && is_path_command(&Path::new(dir).join(name))))
}

/// `command -v` candidacy: stat-able (symlinks followed) and not a
/// directory. Permissions, fifos, and sockets pass exactly as in bash.
fn is_path_command(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| !meta.is_dir())
}

/// Sudo escalation decision table (`_require_sudo`): root passes, then
/// passwordless `sudo -n true`, then quiet mode fails closed, and only
/// then the interactive `sudo true` prompt (whose outcome the caller
/// supplies — the library never blocks on a tty in tests).
pub fn decide_sudo(
    euid_is_root: bool,
    nopass_ok: bool,
    quiet: bool,
    prompt_ok: &impl Fn() -> bool,
) -> bool {
    if euid_is_root {
        return true;
    }
    if nopass_ok {
        return true;
    }
    if quiet {
        return false;
    }
    prompt_ok()
}

/// Live `_require_sudo`: `id -u` for root (the shell forks `id`, so no
/// libc binding is needed for parity), then the [`decide_sudo`]
/// ladder with real `sudo` probes. `quiet` is the verbatim
/// `DOT_QUIET` value; only exactly `1` suppresses the prompt, like the
/// shell's `-eq 1`.
pub fn require_sudo(quiet: &str) -> bool {
    let euid_output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    // Faithful to `[[ $(id -u) -eq 0 ]]`: bash arithmetic coerces empty
    // or non-numeric output to 0, so only an explicit nonzero uid
    // denies the root fast path (notably when PATH lacks `id`).
    let euid_is_root = euid_output
        .as_deref()
        .is_none_or(|text| !matches!(text.parse::<i64>(), Ok(uid) if uid != 0));
    let probe = |extra: &[&str]| {
        std::process::Command::new("sudo")
            .args(extra)
            .arg("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    decide_sudo(euid_is_root, probe(&["-n"]), quiet == "1", &|| probe(&[]))
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
        assert_eq!(tool_present(None, "/bin"), Err(Error::Usage));
        assert_eq!(tool_present(Some(""), "/bin"), Err(Error::Usage));
        assert_eq!(Error::Usage.code(), 2);
        assert_eq!(Error::Unavailable.code(), 1);
    }

    #[test]
    fn termux_adds_android_identity() {
        assert_eq!(platform_matches(Some("android"), "linux", true), Ok(true));
        assert_eq!(platform_matches(Some("android"), "linux", false), Ok(false));
        assert_eq!(platform_matches(Some("linux"), "linux", true), Ok(true));
    }

    #[test]
    fn sudo_ladder() {
        let yes = || true;
        let no = || false;
        assert!(decide_sudo(true, false, true, &no));
        assert!(decide_sudo(false, true, true, &no));
        assert!(!decide_sudo(false, false, true, &yes));
        assert!(decide_sudo(false, false, false, &yes));
        assert!(!decide_sudo(false, false, false, &no));
    }
}
