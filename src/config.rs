//! Strict client configuration parser (slice 2 foundations).
//!
//! Ports `lib/dot/config.sh` exactly: the file is data, never code —
//! only documented HOME spellings expand, everything else is rejected
//! before any extension or provider can execute. Error texts are
//! byte-identical to the shell (`dot: config: …`, exit 2 at the CLI).

use std::path::Path;

use crate::errors::{Error, Result};

/// Maximum config file size in bytes (shell: `-le 65536`).
const MAX_CONFIG_BYTES: u64 = 65536;

/// Dependency provider selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// No provider (`none`, the default).
    None,
    /// The shdeps provider.
    Shdeps,
}

/// shdeps update policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePolicy {
    /// Pinned releases (default).
    Pinned,
    /// Latest releases.
    Latest,
}

/// Parsed client configuration with shell defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Config format version (always 1; anything else is rejected).
    pub version: u32,
    /// Extension API enabled (`extension_api=1`).
    pub extension_api: bool,
    /// Expanded absolute extensions directory, if configured.
    pub extensions_dir: Option<String>,
    /// Selected dependency provider.
    pub provider: Provider,
    /// Default profile name.
    pub default_profile: String,
    /// Effective shdeps update policy (env override wins over file).
    pub shdeps_update_policy: UpdatePolicy,
    /// Whether the policy came from the environment (controls export).
    pub policy_from_env: bool,
}

/// Inputs the shell reads from its surroundings.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// Explicit config path (`$1`). `None` means "missing path argument"
    /// (XDG default resolution lives in the xdg module, next).
    pub config_path: Option<&'a Path>,
    /// `$HOME` for `~`/`$HOME`/`${HOME}` expansion.
    pub home: &'a str,
    /// Pre-captured `$DOT_SHDEPS_UPDATE_POLICY` (the shell captures it at
    /// source time so re-execs re-read a changed file instead of
    /// inheriting a stale export).
    pub env_policy: Option<&'a str>,
}

/// Whether extensions are enabled, mirroring `_dot_extensions_enabled`.
///
/// The shell tests the published environment (`DOT_EXTENSION_API == 1`
/// with a non-empty `DOT_EXTENSIONS_DIR`); by the time Rust runs, those
/// values have arrived resolved on [`Config`], so this takes the struct
/// instead of reading the environment.
pub fn extensions_enabled(config: &Config) -> bool {
    config.extension_api
        && config
            .extensions_dir
            .as_deref()
            .is_some_and(|dir| !dir.is_empty())
}

/// Build a config rejection exactly like `_dot_config_error`.
///
/// The shell prints `dot: config: %s` for `$*` to stderr and returns 1;
/// the engine carries the same payload in [`Error::Config`] (whose
/// `Display` adds the prefix) and lets the CLI render it to stderr with
/// the failing exit, so words arrive here already joined with spaces.
pub fn config_error(message: impl Into<String>) -> Error {
    Error::Config {
        message: message.into(),
    }
}

fn reject(message: impl Into<String>) -> Error {
    config_error(message)
}

/// Validate a HOME value for expansion (mirrors the shell `case`).
fn home_valid(home: &str) -> bool {
    if home.is_empty() {
        return false;
    }
    if home == "/" {
        return true;
    }
    if !home.starts_with('/') {
        return false;
    }
    // Reject trailing slash, doubled slashes, dot segments, CR/LF —
    // the same spellings the shell's glob list rejects.
    if home.ends_with('/')
        || home.contains("//")
        || home.contains("/./")
        || home.ends_with("/.")
        || home.contains("/../")
        || home.ends_with("/..")
        || home.contains('\n')
        || home.contains('\r')
    {
        return false;
    }
    true
}

/// Final absolute-path validation shared by expanded and plain paths.
fn path_valid(path: &str) -> bool {
    if path.is_empty() || path == "/" {
        return false;
    }
    if !path.starts_with('/') {
        return false;
    }
    if path.ends_with('/')
        || path.contains("//")
        || path.contains("/./")
        || path.ends_with("/.")
        || path.contains("/../")
        || path.ends_with("/..")
        || path.contains('\n')
        || path.contains('\r')
    {
        return false;
    }
    true
}

/// Expand one documented HOME spelling, else validate a plain path.
///
/// Accepted leading tokens: `~`, `~/…`, `$HOME`, `$HOME/…`, `${HOME}`,
/// `${HOME}/…`. A `$` or `~` anywhere else — including a second token
/// after expansion — is rejected so no later shell can interpret it.
fn expand_path(value: &str, home: &str) -> Option<String> {
    let suffix: Option<&str> = if value == "~" || value == "$HOME" || value == "${HOME}" {
        None
    } else if let Some(rest) = value.strip_prefix("~/") {
        Some(rest)
    } else if let Some(rest) = value.strip_prefix("$HOME/") {
        Some(rest)
    } else if let Some(rest) = value.strip_prefix("${HOME}/") {
        Some(rest)
    } else if value.contains('$') || value.contains('~') {
        // A token outside leading position: fail closed.
        return None;
    } else {
        // Plain path with no expansion involved.
        return if path_valid(value) {
            Some(value.to_string())
        } else {
            None
        };
    };
    if !home_valid(home) {
        return None;
    }
    let expanded = match suffix {
        None => home.to_string(),
        Some(rest) => {
            if home == "/" {
                format!("/{rest}")
            } else {
                format!("{home}/{rest}")
            }
        }
    };
    // A recognized leading token does not authorize a second expansion
    // later in the path (shell checks AFTER expanding).
    if expanded.contains('$') || expanded.contains('~') {
        return None;
    }
    if path_valid(&expanded) {
        Some(expanded)
    } else {
        None
    }
}

/// Profile identifier syntax (`profile-format.sh`): `^[a-z][a-z0-9-]*$`.
fn profile_identifier_valid(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => (),
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Scan a config file for control bytes, mirroring
/// `_dot_config_control_bytes` (`od -An -t u1` piped to an `awk` scan).
///
/// Returns `true` when the file is clean (shell exit 0): every byte is
/// LF, printable, or >= 128. Returns `false` (shell exit 1) when any
/// byte is < 32 except LF, or is DEL — so CR and TAB are rejected here.
/// A path that cannot be read also fails: production runs under
/// `set -o pipefail`, so a failed `od` fails the whole `od | awk`
/// pipeline (probed: missing files exit 1 everywhere; directories
/// diverge by platform — BSD `od` exits 0 empty, GNU `od` fails —
/// so directories are port-tested, not differential).
pub fn config_control_bytes(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    !bytes.iter().any(|b| (*b < 32 && *b != b'\n') || *b == 127)
}

/// Parse one config file with shell defaults for a missing path.
pub fn load(request: &Request<'_>) -> Result<Config> {
    // The env override is validated before anything else — even before
    // touching the file — because it decides the provider path.
    let env_policy = match request.env_policy {
        None | Some("") => None,
        Some("pinned") => Some(UpdatePolicy::Pinned),
        Some("latest") => Some(UpdatePolicy::Latest),
        Some(other) => {
            return Err(reject(format!(
                "DOT_SHDEPS_UPDATE_POLICY must be pinned or latest, found: {other}"
            )));
        }
    };

    let mut config = Config {
        version: 1,
        extension_api: false,
        extensions_dir: None,
        provider: Provider::None,
        default_profile: "base".to_string(),
        shdeps_update_policy: env_policy.unwrap_or(UpdatePolicy::Pinned),
        policy_from_env: env_policy.is_some(),
    };
    let mut configured_policy = UpdatePolicy::Pinned;

    let Some(path) = request.config_path else {
        // No path argument: XDG resolution happens in the xdg module.
        // Loading "nothing" yields pure defaults (used by tests).
        return Ok(config);
    };
    // Symlinks never qualify — not even to regular files — and neither
    // do directories or missing paths beyond the early return below.
    // (`-e || -L` passes, then `-f && ! -L` must hold.)
    let meta = std::fs::symlink_metadata(path).ok();
    match meta {
        None => return Ok(config),
        Some(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                return Err(reject(format!("not a regular file: {}", path.display())));
            }
        }
    }
    let bytes =
        std::fs::read(path).map_err(|_| reject(format!("cannot size file: {}", path.display())))?;
    // `wc -c` counts bytes; the limit compares the decimal byte count.
    if (bytes.len() as u64) > MAX_CONFIG_BYTES {
        return Err(reject(format!(
            "file exceeds 65536 bytes: {}",
            path.display()
        )));
    }
    // Control bytes: anything < 32 except LF, plus DEL. CR and TAB are
    // rejected here (the shell's `od|awk` scan sees raw bytes).
    if bytes.iter().any(|b| (*b < 32 && *b != b'\n') || *b == 127) {
        return Err(reject(format!(
            "contains control bytes: {}",
            path.display()
        )));
    }
    // Lossy decode is safe post-scan: remaining bytes are printable ASCII
    // range or >= 128, and the shell's byte-oriented `case` patterns only
    // distinguish ASCII tokens. (Non-UTF8 high bytes pass through into
    // values exactly like the shell keeps them.)
    let text = String::from_utf8_lossy(&bytes);

    let mut seen_version = false;
    let mut seen_extension_api = false;
    let mut seen_extensions_dir = false;
    let mut seen_provider = false;
    let mut seen_profile = false;
    let mut seen_policy = false;
    let mut saw_value = false;

    // `IFS= read -r || [[ -n $line ]]`: lines split on LF; a final line
    // without trailing newline is still processed.
    let mut lines = text.split('\n');
    // A trailing newline leaves a phantom empty final element that the
    // shell loop never sees (its last `read` fails with empty `$line`).
    let mut numbered: Vec<(usize, &str)> = Vec::new();
    let raw: Vec<&str> = lines.by_ref().collect();
    let count = if raw.last().is_some_and(|last| last.is_empty()) {
        raw.len() - 1
    } else {
        raw.len()
    };
    for (index, line) in raw.iter().take(count).enumerate() {
        numbered.push((index + 1, line));
    }

    for (line_number, line) in numbered {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.ends_with('\\') {
            return Err(reject(format!("line {line_number} uses a continuation")));
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(reject(format!("line {line_number} is not key=value")));
        };
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(reject(format!("line {line_number} has an invalid key")));
        }
        // `version=1` must be the first setting: any earlier key=value
        // line (comments/blank lines excepted) fails here.
        if !saw_value && key != "version" {
            return Err(reject("version=1 must be the first setting".to_string()));
        }
        saw_value = true;

        match key {
            "version" => {
                if seen_version {
                    return Err(reject("duplicate version".to_string()));
                }
                seen_version = true;
                if value != "1" {
                    return Err(reject(format!("unsupported version: {value}")));
                }
            }
            "extension_api" => {
                if seen_extension_api {
                    return Err(reject("duplicate extension_api".to_string()));
                }
                seen_extension_api = true;
                if value != "1" {
                    return Err(reject(format!("unsupported extension_api: {value}")));
                }
                config.extension_api = true;
            }
            "extensions_dir" => {
                if seen_extensions_dir {
                    return Err(reject("duplicate extensions_dir".to_string()));
                }
                seen_extensions_dir = true;
                match expand_path(value, request.home) {
                    Some(dir) => config.extensions_dir = Some(dir),
                    None => {
                        return Err(reject(format!("invalid extensions_dir: {value}")));
                    }
                }
            }
            "dependency_provider" => {
                if seen_provider {
                    return Err(reject("duplicate dependency_provider".to_string()));
                }
                seen_provider = true;
                match value {
                    "none" => config.provider = Provider::None,
                    "shdeps" => config.provider = Provider::Shdeps,
                    _ => {
                        return Err(reject(format!("unsupported dependency_provider: {value}")));
                    }
                }
            }
            "default_profile" => {
                if seen_profile {
                    return Err(reject("duplicate default_profile".to_string()));
                }
                seen_profile = true;
                if !profile_identifier_valid(value) {
                    return Err(reject(format!("invalid default_profile: {value}")));
                }
                config.default_profile = value.to_string();
            }
            "shdeps_update_policy" => {
                if seen_policy {
                    return Err(reject("duplicate shdeps_update_policy".to_string()));
                }
                seen_policy = true;
                match value {
                    "pinned" => configured_policy = UpdatePolicy::Pinned,
                    "latest" => configured_policy = UpdatePolicy::Latest,
                    _ => {
                        return Err(reject(format!(
                            "shdeps_update_policy must be pinned or latest, found: {value}"
                        )));
                    }
                }
            }
            _ => return Err(reject(format!("unknown key: {key}"))),
        }
    }

    if !seen_version {
        return Err(reject("missing version=1".to_string()));
    }
    if seen_extensions_dir && !config.extension_api {
        return Err(reject(
            "extensions_dir requires extension_api=1".to_string(),
        ));
    }
    // A config-derived value must not become an environment override
    // across re-execs: with no env override, re-read the file each load.
    if !config.policy_from_env {
        config.shdeps_update_policy = configured_policy;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    struct Fixture {
        _dir: TempDir,
        path: std::path::PathBuf,
    }

    /// Write a config body to an isolated file, keeping the temp dir
    /// alive for the whole test (dropping it deletes the file).
    fn fixture(body: &[u8]) -> Fixture {
        let dir = TempDir::new("config").expect("temp dir");
        let path = dir.path().join("config");
        std::fs::write(&path, body).expect("write fixture");
        Fixture { _dir: dir, path }
    }

    fn load_body(body: &str, home: &str, env_policy: Option<&str>) -> Result<Config> {
        let fx = fixture(body.as_bytes());
        load(&Request {
            config_path: Some(&fx.path),
            home,
            env_policy,
        })
    }

    fn err_text(result: Result<Config>) -> String {
        format!("{}", result.expect_err("must be rejected"))
    }

    #[test]
    fn missing_path_yields_defaults() {
        let config = load(&Request {
            config_path: Some(Path::new("/nonexistent/dot-config")),
            home: "/home/u",
            env_policy: None,
        })
        .expect("missing file loads");
        assert_eq!(config.provider, Provider::None);
        assert_eq!(config.extensions_dir, None);
        assert_eq!(config.default_profile, "base");
        assert_eq!(config.shdeps_update_policy, UpdatePolicy::Pinned);
        assert!(!config.policy_from_env);
    }

    #[test]
    fn full_valid_config_parses() {
        let config = load_body(
            "version=1\nextension_api=1\nextensions_dir=${HOME}/.local/lib/dotfiles\ndependency_provider=shdeps\ndefault_profile=dev\nshdeps_update_policy=latest\n",
            "/home/u",
            None,
        )
        .expect("valid config loads");
        assert!(config.extension_api);
        assert_eq!(
            config.extensions_dir.as_deref(),
            Some("/home/u/.local/lib/dotfiles")
        );
        assert_eq!(config.provider, Provider::Shdeps);
        assert_eq!(config.default_profile, "dev");
        assert_eq!(config.shdeps_update_policy, UpdatePolicy::Latest);
    }

    #[test]
    fn comments_blanks_and_missing_trailing_newline() {
        let config = load_body("# lead\n\nversion=1", "/home/u", None).expect("loads");
        assert_eq!(config.version, 1);
    }

    #[test]
    fn env_policy_wins_over_file_and_validates_first() {
        let config = load_body(
            "version=1\nshdeps_update_policy=pinned\n",
            "/home/u",
            Some("latest"),
        )
        .expect("loads");
        assert_eq!(config.shdeps_update_policy, UpdatePolicy::Latest);
        assert!(config.policy_from_env);
        let text = err_text(load_body("version=1\n", "/home/u", Some("bogus")));
        assert_eq!(
            text,
            "dot: config: DOT_SHDEPS_UPDATE_POLICY must be pinned or latest, found: bogus"
        );
    }

    #[test]
    fn rejection_table_matches_shell_texts() {
        let cases = [
            ("extension_api=1\n", "version=1 must be the first setting"),
            ("version=2\n", "unsupported version: 2"),
            ("version=1\nversion=1\n", "duplicate version"),
            ("version=1\nbogus=1\n", "unknown key: bogus"),
            ("version=1\nVERSION=1\n", "line 2 has an invalid key"),
            ("", "missing version=1"),
            ("version=1\\\n", "line 1 uses a continuation"),
            ("version\n", "line 1 is not key=value"),
            (
                "version=1\nextension_api=2\n",
                "unsupported extension_api: 2",
            ),
            (
                "version=1\nextensions_dir=/x\n",
                "extensions_dir requires extension_api=1",
            ),
            (
                "version=1\ndependency_provider=apt\n",
                "unsupported dependency_provider: apt",
            ),
            (
                "version=1\ndefault_profile=Dev\n",
                "invalid default_profile: Dev",
            ),
            (
                "version=1\nshdeps_update_policy=sometimes\n",
                "shdeps_update_policy must be pinned or latest, found: sometimes",
            ),
        ];
        for (body, expected) in cases {
            let text = err_text(load_body(body, "/home/u", None));
            assert_eq!(text, format!("dot: config: {expected}"), "body: {body:?}");
        }
    }

    #[test]
    fn home_spellings_expand_and_mixed_tokens_fail() {
        for (value, expected) in [
            ("~", "/home/u"),
            ("~/a", "/home/u/a"),
            ("$HOME", "/home/u"),
            ("$HOME/a", "/home/u/a"),
            ("${HOME}", "/home/u"),
            ("${HOME}/a", "/home/u/a"),
            ("/abs/path", "/abs/path"),
        ] {
            let body = format!("version=1\nextension_api=1\nextensions_dir={value}\n");
            let config = load_body(&body, "/home/u", None).expect("expands");
            assert_eq!(
                config.extensions_dir.as_deref(),
                Some(expected),
                "value: {value}"
            );
        }
        for value in ["~/$HOME/x", "$HOME/~", "/a$HOME", "a~/b", "~/", "$HOME/"] {
            let body = format!("version=1\nextension_api=1\nextensions_dir={value}\n");
            assert!(
                load_body(&body, "/home/u", None).is_err(),
                "must reject: {value}"
            );
        }
        // HOME=/ with a bare token expands to / which the final path
        // check rejects (mirrors the shell exactly).
        let text = err_text(load_body(
            "version=1\nextension_api=1\nextensions_dir=~\n",
            "/",
            None,
        ));
        assert_eq!(text, "dot: config: invalid extensions_dir: ~");
    }

    #[test]
    fn control_bytes_and_size_limits() {
        let fx = fixture(b"version=1\n\x01\n");
        let text = err_text(load(&Request {
            config_path: Some(&fx.path),
            home: "/home/u",
            env_policy: None,
        }));
        assert!(text.contains("contains control bytes"), "{text}");
        // TAB is also rejected (only LF survives the byte scan).
        let fx = fixture(b"version=1\n\t\n");
        assert!(
            load(&Request {
                config_path: Some(&fx.path),
                home: "/home/u",
                env_policy: None,
            })
            .is_err()
        );
        // One byte over the limit fails; exactly at the limit would need
        // a valid body, so assert the boundary message on overflow.
        let mut big = vec![b'#'; 65537];
        big.push(b'\n');
        let fx = fixture(&big);
        let text = err_text(load(&Request {
            config_path: Some(&fx.path),
            home: "/home/u",
            env_policy: None,
        }));
        assert!(text.contains("file exceeds 65536 bytes"), "{text}");
    }

    #[test]
    fn config_error_renders_shell_diagnostic() {
        // The shell prints `dot: config: %s` for `$*`; words arrive
        // already joined here, and the CLI renders the error to stderr.
        assert_eq!(
            format!("{}", config_error("duplicate version")),
            "dot: config: duplicate version"
        );
        assert_eq!(
            format!("{}", config_error(["a", "b", "c"].join(" "))),
            "dot: config: a b c"
        );
        assert_eq!(format!("{}", config_error("")), "dot: config: ");
    }

    #[test]
    fn control_bytes_scan_matches_shell_od_awk() {
        // (body, clean): TAB/CR/NUL/DEL/US fail; LF, space, tilde,
        // and high bytes pass — the shell probe agrees on each.
        let cases: &[(&[u8], bool)] = &[
            (b"", true),
            (b"abc\n", true),
            (b" ~\n", true),
            (b"\xff\xfe\x80\n", true),
            (b"a\tb\n", false),
            (b"a\rb\n", false),
            (b"a\x00b", false),
            (b"a\x7fb", false),
            (b"a\x1fb", false),
        ];
        for (index, (body, expected)) in cases.iter().enumerate() {
            let fx = fixture(body);
            assert_eq!(
                config_control_bytes(&fx.path),
                *expected,
                "case {index} body {body:?}"
            );
        }
    }

    #[test]
    fn control_bytes_unreadable_paths_fail_closed() {
        // Production runs under `set -o pipefail`, so a failed `od`
        // fails the whole `od | awk` pipeline: missing paths and
        // directories exit 1 (verified against the live oracle).
        let dir = TempDir::new("config-control").expect("temp dir");
        assert!(!config_control_bytes(&dir.path().join("does-not-exist")));
        assert!(!config_control_bytes(dir.path()));
    }

    #[test]
    fn control_bytes_follow_symlinks_like_od() {
        let dir = TempDir::new("config-controllink").expect("temp dir");
        let target = dir.path().join("real");
        std::fs::write(&target, b"version=1\n\x01\n").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(!config_control_bytes(&link));
    }

    #[test]
    fn extensions_gate_needs_api_and_dir() {
        fn gate(api: bool, dir: Option<&str>) -> bool {
            extensions_enabled(&Config {
                version: 1,
                extension_api: api,
                extensions_dir: dir.map(str::to_string),
                provider: Provider::None,
                default_profile: "base".to_string(),
                shdeps_update_policy: UpdatePolicy::Pinned,
                policy_from_env: false,
            })
        }
        assert!(!gate(false, None));
        assert!(!gate(true, None));
        assert!(!gate(true, Some("")));
        assert!(!gate(false, Some("/x")));
        assert!(gate(true, Some("/x")));
    }

    #[test]
    fn symlink_and_directory_are_not_regular_files() {
        let dir = TempDir::new("configlink").expect("temp dir");
        let target = dir.path().join("real");
        std::fs::write(&target, b"version=1\n").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        for path in [&link, dir.path()] {
            let text = err_text(load(&Request {
                config_path: Some(path),
                home: "/home/u",
                env_policy: None,
            }));
            assert!(text.contains("not a regular file"), "{text}");
        }
    }
}
