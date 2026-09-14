//! Binary startup prelude for `dot` (slice 84).
//!
//! Ports the `lib/dot/main.sh` prelude plus the `bin/dot` entry
//! contract into the Rust binary startup path (`cli::run` calls
//! [`check_ambient`]; `main.rs` binds [`ambient_source_root`]).
//! Reuses the already-ported [`crate::config`], [`crate::xdg`], and
//! [`crate::version`] modules plus the existing `cli::HELP` text —
//! nothing here re-ports their internals.
//!
//! Shell line map (`bin/dot` is 59 lines, `lib/dot/main.sh` 94):
//!
//! | Shell | Rust |
//! |---|---|
//! | `CDPATH=` | No equivalent needed: the engine builds absolute paths only (`source_root` / `xdg` joins); no `cd`-relative lookup exists to perturb. |
//! | `set -euo pipefail` | No equivalent needed: fallibility is typed (`Result`, explicit `Option`) instead of dynamic. |
//! | `umask g-w,o-w` | [`ensure_umask_ceiling`]: the same `mask \| 0o022` as a pure function. The process mask itself is never mutated (no `std` binding exists — see `temp::read_umask` — and a global mutation would race every thread); creation sites already carry explicit modes (`temp::sibling_tmp_for` uses `0o600`, plus `temp::apply_umask_ceiling`). |
//! | `shopt -u nocasematch` | No equivalent needed: Rust `match` on argv bytes is always byte-exact and case-sensitive (pinned by test against both entry files). |
//! | Bash-4+ gate (`dot: Bash 4 or newer is required`, exit 1) | No equivalent needed: the compiled binary has no interpreter to version-gate. `test_support::bash` still requires Bash 4+ for the differential oracles only. |
//! | `DOT_SOURCE_ROOT=$(cd -P …/lib/dot/main.sh …/../..)` + export | [`resolve_source_root`] / [`ambient_source_root`]: an explicit `DOT_SOURCE_ROOT` wins verbatim (the hermetic-test and embedding hook the shell's unconditional bind cannot offer); otherwise the current executable's canonical path is walked up to the first ancestor holding `lib/dot/main.sh` (canonicalization resolves symlink chains the way `cd -P` plus the `bin/dot` 40-hop loop does — an unresolvable chain simply misses the probe instead of printing `launcher symlink chain is too deep`); otherwise the cwd applies (mirroring `${DOT_SOURCE_ROOT:-$PWD}` in `_dot_source_git`). |
//! | `. lib/dot/temp.sh` | No sourcing step: `temp` is linked statically and called directly ([`observed_revision`] uses `temp::sanitized_git`). |
//! | `DOT_ORIGINAL_ARGV=("$@")` | No global: argv is threaded explicitly (`cli::run` takes `args`). |
//! | `DOT_REEXEC_EXPECTED_REVISION` guard (exit 1) | [`check_reexec_revision`] + [`observed_revision`], same order (before config), same bytes including the `${var:-<missing>}` spelling. |
//! | `. public/api-version.sh` | [`crate::version::LIBRARY_API`] (already pinned to `DOT_LIBRARY_API=1` by `tests/constants.rs`). |
//! | `. public/xdg.sh` | [`crate::xdg`] (relative XDG values already fall back, exactly like the shell). |
//! | `. public/ui.sh` | [`crate::ui`] (already ported; startup performs no presentation). |
//! | `. lib/dot/config.sh` + `dot_config_load \|\| exit 2` | [`load_default_config`]: the XDG-default `dot/config` through `config::load`; any rejection becomes exit 2 with byte-identical diagnostics. Runs BEFORE dispatch for EVERY command per the forward contracts in `docs/rust-port-spec.md` ("an unloadable config exits 2 for ANY command") — the shell `case` currently exempts `help`/`version`, and that divergence is deliberate and pinned in `tests/startup.rs`. |
//! | `REPLY` global | No equivalent: every helper returns values (the `xdg` precedent). |
//! | `dot_version` / `dot_help` | `version::version_line` / `cli::HELP` (byte parity already pinned by `tests/cli.rs`; the startup suite re-pins `version` end to end so the prelude cannot perturb it). |
//!
//! A loaded config is otherwise invisible: [`preflight`] validates
//! and returns the [`Config`] for future slices, while
//! [`check_ambient`] (the `cli::run` entry) discards it
//! — no process environment is published yet, so already-wired
//! commands behave exactly as before whenever config loads.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::Config;

/// Bits `umask g-w,o-w` adds to the mask: group-write denied,
/// other-write denied, every stricter caller bit retained.
pub const UMASK_CEILING_BITS: u32 = 0o022;

/// Apply the startup umask ceiling to a mask without touching the
/// process: `umask g-w,o-w` is `mask | 0o022` (a stricter caller
/// policy such as `0077` passes through unchanged).
pub fn ensure_umask_ceiling(mask: u32) -> u32 {
    mask | UMASK_CEILING_BITS
}

/// Resolve the selected Dot checkout.
///
/// `env_root` (explicit `DOT_SOURCE_ROOT`) wins verbatim when
/// non-empty; otherwise the canonicalized `exe` path is walked up to
/// the first ancestor holding `lib/dot/main.sh` (a binary installed
/// as `<root>/bin/dot` answers exactly like the shell's
/// `dirname …/../..` derivation); otherwise `cwd` applies.
pub fn resolve_source_root(exe: &Path, env_root: Option<&OsStr>, cwd: &Path) -> PathBuf {
    if let Some(root) = env_root {
        if !root.is_empty() {
            return PathBuf::from(root);
        }
    }
    if let Ok(canonical) = std::fs::canonicalize(exe) {
        for ancestor in canonical.ancestors() {
            if ancestor.join("lib/dot/main.sh").is_file() {
                return ancestor.to_path_buf();
            }
        }
    }
    cwd.to_path_buf()
}

/// Resolve the checkout from ambient process state: `DOT_SOURCE_ROOT`
///, the current executable, and the working directory, in the
/// [`resolve_source_root`] precedence. Unresolvable pieces degrade to
/// inert placeholders (never an error: the re-exec probe then reports
/// `<missing>` and config resolution falls back exactly like the
/// shell's `${DOT_SOURCE_ROOT:-$PWD}`).
pub fn ambient_source_root() -> PathBuf {
    let env_root = std::env::var_os("DOT_SOURCE_ROOT");
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/nonexistent-dot-exe"));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    resolve_source_root(&exe, env_root.as_deref(), &cwd)
}

/// Read the observed checkout revision: `git rev-parse HEAD` bound to
/// `source_root` through the same sanitized `-c`/`-C` isolation the
/// shell's `_dot_source_git` applies (`2>/dev/null || true` there is
/// `None` here — spawn failure, non-zero exit, and empty output all
/// mean "missing", which the guard spells `<missing>`).
pub fn observed_revision(source_root: &Path) -> Option<String> {
    let mut cmd = crate::temp::sanitized_git(source_root, &["rev-parse", "HEAD"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Decide the re-exec guard over explicit revisions.
///
/// `None` (or empty) expectation skips the guard, like the shell's
/// `[[ -n … ]]`; otherwise the observed revision must equal the
/// expectation. `None` (or empty) observed spells `<missing>`, like
/// `${_dot_reexec_observed:-<missing>}`. Returns the exact stderr
/// line (without trailing newline) on mismatch.
pub fn check_reexec_revision(expected: Option<&str>, observed: Option<&str>) -> Result<(), String> {
    let expected = match expected {
        Some(value) if !value.is_empty() => value,
        _ => return Ok(()),
    };
    let observed = match observed {
        Some(value) if !value.is_empty() => value,
        _ => "<missing>",
    };
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "dot: re-exec revision mismatch: expected {expected}, found {observed}"
        ))
    }
}

/// Load the default client configuration: the XDG-default
/// `dot/config` through [`crate::config::load`], exactly as
/// `dot_config_load` with no `$1` (an unresolvable HOME becomes the
/// shell's `HOME does not provide an absolute config root`
/// rejection). A missing file yields shell defaults. Returns the
/// exact stderr line (without trailing newline) on rejection.
pub fn load_default_config(
    home: &str,
    xdg_config_home: &str,
    env_policy: Option<&str>,
) -> Result<Config, String> {
    let path = match crate::xdg::path(
        crate::xdg::Kind::Config,
        "dot/config",
        xdg_config_home,
        home,
    ) {
        Ok(path) => path,
        Err(_) => {
            return Err("dot: config: HOME does not provide an absolute config root".to_string());
        }
    };
    let request = crate::config::Request {
        config_path: Some(Path::new(&path)),
        home,
        env_policy,
    };
    match crate::config::load(&request) {
        Ok(config) => Ok(config),
        Err(error) => Err(error.to_string()),
    }
}

/// Explicit startup inputs: raw process spellings, where `None` reads
/// as unset (empty `DOT_SOURCE_ROOT`-style values are handled per
/// field, like the shell's `:-` defaults).
pub struct Inputs<'a> {
    /// Raw `$HOME`.
    pub home: &'a str,
    /// Raw `$XDG_CONFIG_HOME` (empty counts as unset, like the shell).
    pub xdg_config_home: &'a str,
    /// Pre-captured `$DOT_SHDEPS_UPDATE_POLICY` (empty counts as
    /// unset; validated before the file is touched).
    pub env_policy: Option<&'a str>,
    /// Raw `$DOT_REEXEC_EXPECTED_REVISION` (empty skips the guard).
    pub reexec_expected: Option<&'a str>,
    /// Already-resolved checkout for the revision probe.
    pub source_root: &'a Path,
}

/// Startup failure, carrying the shell's exit code and exact stderr
/// line (without trailing newline; the caller terminates the line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Re-exec revision mismatch (shell exit 1).
    Reexec {
        /// The `dot: re-exec revision mismatch: …` line.
        line: String,
    },
    /// Config rejection (shell `dot_config_load || exit 2`).
    Config {
        /// The `dot: config: …` line.
        line: String,
    },
}

impl Failure {
    /// Shell exit code for this failure.
    pub fn code(&self) -> i32 {
        match self {
            Failure::Reexec { .. } => 1,
            Failure::Config { .. } => 2,
        }
    }

    /// Exact stderr line for this failure (no trailing newline).
    pub fn line(&self) -> &str {
        match self {
            Failure::Reexec { line } | Failure::Config { line } => line,
        }
    }
}

/// Run the startup prelude in shell order: re-exec guard first (exit
/// 1), then default config load (exit 2). The returned config is for
/// future slices; `cli::run` discards it so wired commands cannot
/// observe a difference whenever config loads.
pub fn preflight(inputs: &Inputs<'_>) -> Result<Config, Failure> {
    let observed = observed_revision(inputs.source_root);
    if let Err(line) = check_reexec_revision(inputs.reexec_expected, observed.as_deref()) {
        return Err(Failure::Reexec { line });
    }
    match load_default_config(inputs.home, inputs.xdg_config_home, inputs.env_policy) {
        Ok(config) => Ok(config),
        Err(line) => Err(Failure::Config { line }),
    }
}

/// Run [`preflight`] against ambient process state (`HOME`,
/// `XDG_CONFIG_HOME`, `DOT_SHDEPS_UPDATE_POLICY`,
/// `DOT_REEXEC_EXPECTED_REVISION`, and the [`ambient_source_root`]
/// checkout). The single-flight command entry path, like the shell's
/// own exports.
pub fn check_ambient() -> Result<Config, Failure> {
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_default();
    let env_policy = std::env::var("DOT_SHDEPS_UPDATE_POLICY").ok();
    let reexec_expected = std::env::var("DOT_REEXEC_EXPECTED_REVISION").ok();
    let root = ambient_source_root();
    let inputs = Inputs {
        home: &home,
        xdg_config_home: &xdg_config_home,
        env_policy: env_policy.as_deref(),
        reexec_expected: reexec_expected.as_deref(),
        source_root: &root,
    };
    preflight(&inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umask_ceiling_ors_group_and_other_write() {
        for (start, expected) in [
            (0o022, 0o022),
            (0o002, 0o022),
            (0o027, 0o027),
            (0o077, 0o077),
            (0o000, 0o022),
            (0o007, 0o027),
            (0o026, 0o026),
        ] {
            assert_eq!(ensure_umask_ceiling(start), expected, "mask: {start:o}");
        }
    }

    #[test]
    fn reexec_guard_skips_without_expectation() {
        assert_eq!(check_reexec_revision(None, None), Ok(()));
        assert_eq!(check_reexec_revision(Some(""), Some("abc")), Ok(()));
        assert_eq!(check_reexec_revision(None, Some("abc")), Ok(()));
    }

    #[test]
    fn reexec_guard_matches_and_mismatches() {
        assert_eq!(check_reexec_revision(Some("abc"), Some("abc")), Ok(()));
        assert_eq!(
            check_reexec_revision(Some("abc"), Some("def")),
            Err("dot: re-exec revision mismatch: expected abc, found def".to_string())
        );
    }

    #[test]
    fn reexec_guard_spells_missing_for_absent_or_empty() {
        let line = "dot: re-exec revision mismatch: expected abc, found <missing>".to_string();
        assert_eq!(check_reexec_revision(Some("abc"), None), Err(line.clone()));
        assert_eq!(check_reexec_revision(Some("abc"), Some("")), Err(line));
    }

    #[test]
    fn failure_codes_and_lines() {
        let reexec = Failure::Reexec {
            line: "r".to_string(),
        };
        assert_eq!(reexec.code(), 1);
        assert_eq!(reexec.line(), "r");
        let config = Failure::Config {
            line: "c".to_string(),
        };
        assert_eq!(config.code(), 2);
        assert_eq!(config.line(), "c");
    }

    #[test]
    fn source_root_prefers_explicit_env() {
        assert_eq!(
            resolve_source_root(
                Path::new("/nonexistent/exe"),
                Some(OsStr::new("/custom/root")),
                Path::new("/fallback"),
            ),
            PathBuf::from("/custom/root")
        );
        assert_eq!(
            resolve_source_root(
                Path::new("/nonexistent/exe"),
                Some(OsStr::new("")),
                Path::new("/fallback"),
            ),
            PathBuf::from("/fallback")
        );
        assert_eq!(
            resolve_source_root(Path::new("/nonexistent/exe"), None, Path::new("/fallback")),
            PathBuf::from("/fallback")
        );
    }

    #[test]
    fn default_config_rejects_unresolvable_home() {
        let err = load_default_config("relative", "", None).expect_err("must reject");
        assert_eq!(
            err,
            "dot: config: HOME does not provide an absolute config root"
        );
    }
}
