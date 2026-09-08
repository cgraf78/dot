//! Binary startup prelude for `dot`.
//!
//! Owns source-root discovery and the process preflight before
//! command dispatch (`app::run` calls
//! [`check`] with its snapshotted runtime).
//! Reuses the [`crate::config`], [`crate::xdg`], and
//! [`crate::version`] modules plus the existing `cli::HELP` text —
//! nothing here re-ports their internals.
//!
//! [`preflight`] validates and returns the loaded [`Config`] to native command
//! dispatch. No process environment is published; consumers receive the
//! immutable configuration explicitly.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::Config;

/// Bits `umask g-w,o-w` adds to the mask: group-write denied,
/// other-write denied, every stricter caller bit retained.
pub const UMASK_CEILING_BITS: u32 = 0o022;

/// Termux's published replacement for `/proc/self/exe` while its exec shim
/// launches an application through Android's system linker.
pub(crate) const TERMUX_EXECUTABLE_ENV: &str = "TERMUX_EXEC__PROC_SELF_EXE";

/// Apply the startup umask ceiling to a mask without touching the
/// process: `umask g-w,o-w` is `mask | 0o022` (a stricter caller
/// policy such as `0077` passes through unchanged).
pub fn ensure_umask_ceiling(mask: u32) -> u32 {
    mask | UMASK_CEILING_BITS
}

/// Resolve the Dot checkout or release that owns `exe`.
///
/// The executable is canonicalized before its recognized development or
/// release layout is inspected, so an installed launcher symlink still binds
/// the runtime to the payload it actually executes. A missing or incomplete
/// root is an error: the binary must never fall back to caller-controlled
/// process state when selecting code that hooks and providers may execute.
pub fn executable_source_root(exe: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(exe).map_err(|_| source_root_error())?;
    source_root_from_canonical(
        &canonical,
        Path::new(env!("OUT_DIR")),
        Path::new(env!("CARGO_MANIFEST_DIR")),
        true,
    )
    .ok_or_else(source_root_error)
}

fn executable_owner_root(exe: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(exe).map_err(|_| source_root_error())?;
    source_root_from_canonical(
        &canonical,
        Path::new(env!("OUT_DIR")),
        Path::new(env!("CARGO_MANIFEST_DIR")),
        false,
    )
    .ok_or_else(source_root_error)
}

fn source_root_shape(root: &Path, require_public: bool) -> bool {
    (root.join("Cargo.toml").is_file() || root.join(".dot-install.json").is_file())
        && (!require_public || root.join("lib/dot/public").is_dir())
}

fn source_root_from_canonical(
    executable: &Path,
    out_dir: &Path,
    manifest_dir: &Path,
    require_public: bool,
) -> Option<PathBuf> {
    cargo_source_root_from(executable, out_dir, manifest_dir, require_public)
        .or_else(|| physical_source_root(executable, require_public))
}

fn physical_source_root(executable: &Path, require_public: bool) -> Option<PathBuf> {
    let parent = executable.parent()?;
    if source_root_shape(parent, require_public) {
        return Some(parent.to_path_buf());
    }
    if parent.file_name() == Some(OsStr::new("bin")) {
        let root = parent.parent()?;
        if source_root_shape(root, require_public) {
            return Some(root.to_path_buf());
        }
    }
    let target = if parent.file_name() == Some(OsStr::new("deps")) {
        parent.parent()?.parent()?
    } else {
        parent.parent()?
    };
    if target.file_name() == Some(OsStr::new("target")) {
        let root = target.parent()?;
        if source_root_shape(root, require_public) {
            return Some(root.to_path_buf());
        }
    }
    None
}

fn cargo_source_root_from(
    executable: &Path,
    out_dir: &Path,
    manifest_dir: &Path,
    require_public: bool,
) -> Option<PathBuf> {
    // Cargo may place target artifacts outside the checkout. `OUT_DIR` and
    // `CARGO_MANIFEST_DIR` are compile-time values, so unlike process
    // environment they cannot redirect a built binary. Restrict the fallback
    // to a direct binary or `deps/` artifact in the exact Cargo profile that
    // produced this crate; a copied or installed binary must carry release
    // metadata beside itself.
    let profile = out_dir.ancestors().nth(3)?;
    let profile = std::fs::canonicalize(profile).ok()?;
    let relative = executable.strip_prefix(&profile).ok()?;
    let mut components = relative.components();
    let cargo_artifact = match (components.next(), components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None, None) => true,
        (
            Some(std::path::Component::Normal(directory)),
            Some(std::path::Component::Normal(_)),
            None,
        ) => directory == OsStr::new("deps"),
        _ => false,
    };
    if !cargo_artifact {
        return None;
    }
    let manifest = std::fs::canonicalize(manifest_dir).ok()?;
    source_root_shape(&manifest, require_public).then_some(manifest)
}

fn source_root_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot resolve source root from executable",
    )
}

#[cfg(any(target_os = "android", test))]
fn process_executable_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot resolve process executable",
    )
}

#[cfg(any(target_os = "android", test))]
fn process_executable_from(
    current_exe: &Path,
    argv0: Option<&OsStr>,
    termux_executable: Option<&OsStr>,
    system_linkers: &[&Path],
) -> std::io::Result<PathBuf> {
    let current_identity = match std::fs::canonicalize(current_exe) {
        Ok(path) => path,
        Err(_) => return Ok(current_exe.to_path_buf()),
    };
    let uses_system_linker = system_linkers.iter().any(|linker| {
        std::fs::canonicalize(linker).is_ok_and(|identity| identity == current_identity)
    });
    if !uses_system_linker {
        return Ok(current_exe.to_path_buf());
    }

    // In Termux's system-linker mode `/proc/self/exe` identifies Bionic's
    // linker. The preload shim records the actual ELF in this variable, while
    // Bionic independently presents that same loaded path as application
    // `argv[0]`. Require both signals to agree so arbitrary ambient state can
    // never select the checkout whose hook code Dot will execute.
    let candidate = termux_executable
        .filter(|path| !path.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(process_executable_error)?;
    let argv0 = argv0
        .filter(|path| !path.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(process_executable_error)?;
    let candidate = std::fs::canonicalize(candidate).map_err(|_| process_executable_error())?;
    let argv0 = std::fs::canonicalize(argv0).map_err(|_| process_executable_error())?;
    if candidate != argv0 {
        return Err(process_executable_error());
    }
    Ok(candidate)
}

#[cfg(any(target_os = "android", test))]
fn android_system_linkers() -> [&'static Path; 4] {
    [
        Path::new("/system/bin/linker"),
        Path::new("/system/bin/linker64"),
        Path::new("/apex/com.android.runtime/bin/linker"),
        Path::new("/apex/com.android.runtime/bin/linker64"),
    ]
}

pub(crate) fn process_executable(
    argv0: Option<&OsStr>,
    termux_executable: Option<&OsStr>,
) -> std::io::Result<PathBuf> {
    let executable = std::env::current_exe().map_err(|_| source_root_error())?;

    #[cfg(target_os = "android")]
    {
        process_executable_from(
            &executable,
            argv0,
            termux_executable,
            &android_system_linkers(),
        )
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = (argv0, termux_executable);
        Ok(executable)
    }
}

pub(crate) fn process_source_root(executable: &Path) -> std::io::Result<PathBuf> {
    executable_source_root(executable)
}

pub(crate) fn process_owner_root(executable: &Path) -> std::io::Result<PathBuf> {
    executable_owner_root(executable)
}

pub(crate) fn informational_command(command: &[u8]) -> bool {
    matches!(
        command,
        b"" | b"help" | b"-h" | b"--help" | b"version" | b"--version"
    )
}

/// Resolve the selected Dot checkout for a trusted embedded runtime.
///
/// `env_root` (explicit `DOT_SOURCE_ROOT`) wins verbatim when
/// non-empty; otherwise the canonicalized `exe` path is walked up to
/// the first ancestor holding either native checkout metadata or native
/// release metadata plus its public API directory; otherwise `cwd` applies.
pub fn resolve_source_root(exe: &Path, env_root: Option<&OsStr>, cwd: &Path) -> PathBuf {
    if let Some(root) = env_root {
        if !root.is_empty() {
            return PathBuf::from(root);
        }
    }
    if let Ok(root) = executable_source_root(exe) {
        return root;
    }
    cwd.to_path_buf()
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

/// Resolve the immutable revision associated with a checkout or packaged
/// release. Development checkouts retain Git as their authority. A packaged
/// runtime has no `.git`, so its signed archive metadata must agree with the
/// commit compiled into the executable before that identity is accepted.
pub(crate) fn source_revision(source_root: &Path) -> Option<String> {
    observed_revision(source_root).or_else(|| release_revision(source_root))
}

fn release_revision(source_root: &Path) -> Option<String> {
    let commit = crate::version::COMMIT;
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let metadata = source_root.join(".dot-install.json");
    let kind = std::fs::symlink_metadata(&metadata).ok()?.file_type();
    if !kind.is_file() || kind.is_symlink() {
        return None;
    }
    let body = std::fs::read_to_string(metadata).ok()?;
    let mut found = None;
    for line in body.lines() {
        let Some(rest) = line.trim().strip_prefix("\"commit\"") else {
            continue;
        };
        let value = rest.trim_start().strip_prefix(':')?.trim();
        let value = value.strip_suffix(',').unwrap_or(value).trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        if found.replace(value).is_some() {
            return None;
        }
    }
    (found == Some(commit)).then(|| commit.to_string())
}

/// Return the packaged native entry only when the release metadata matches this
/// build and the payload is a real executable file.
pub(crate) fn release_binary(source_root: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    release_revision(source_root)?;
    let binary = source_root.join("dot");
    let metadata = std::fs::symlink_metadata(&binary).ok()?;
    (metadata.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.permissions().mode() & 0o111 != 0)
        .then_some(binary)
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

/// Run the startup prelude in shell order: re-exec guard first (exit 1), then
/// default config load (exit 2). Native dispatch consumes the returned config.
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

/// Run [`preflight`] against an immutable invocation runtime.
pub fn check(runtime: &crate::app::Runtime) -> Result<Config, Failure> {
    check_reexec(runtime)?;
    let home = runtime
        .value("HOME")
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let xdg_config_home = runtime
        .value("XDG_CONFIG_HOME")
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let env_policy = runtime
        .value("DOT_SHDEPS_UPDATE_POLICY")
        .and_then(OsStr::to_str);
    load_default_config(home, xdg_config_home, env_policy).map_err(|line| Failure::Config { line })
}

/// Validate the provider re-exec generation before any command dispatch.
/// Help and version deliberately stop after this guard, matching the shell
/// entry point which does not load user configuration for informational output.
pub(crate) fn check_reexec(runtime: &crate::app::Runtime) -> Result<(), Failure> {
    let expected = runtime
        .value("DOT_REEXEC_EXPECTED_REVISION")
        .and_then(OsStr::to_str);
    let observed = observed_revision(runtime.source_root());
    check_reexec_revision(expected, observed.as_deref()).map_err(|line| Failure::Reexec { line })
}

/// Run [`check`] against a one-time snapshot of the ambient process state.
///
/// Compatibility-only native callers use this adapter; command dispatch enters
/// through [`crate::app`] with an explicit snapshot.
pub fn check_ambient() -> Result<Config, Failure> {
    let env = std::env::vars_os().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let runtime = crate::app::Runtime::from_env(&env, &cwd)
        .expect("the current directory fallback is absolute");
    check(&runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_test_support::TempDir;

    fn executable_fixture(root: &Path, relative: &str) -> PathBuf {
        let executable = root.join(relative);
        std::fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("executable parent");
        std::fs::write(&executable, b"fixture executable").expect("executable fixture");
        executable
    }

    #[test]
    fn ordinary_process_ignores_termux_executable_override() {
        let scratch = TempDir::new("ordinary-process-executable").expect("scratch");
        let current = executable_fixture(scratch.path(), "bin/dot");
        let hostile = executable_fixture(scratch.path(), "hostile/dot");

        assert_eq!(
            process_executable_from(
                &current,
                Some(hostile.as_os_str()),
                Some(hostile.as_os_str()),
                &[],
            )
            .expect("ordinary executable"),
            current
        );
    }

    #[test]
    fn android_system_linker_inventory_covers_legacy_and_apex_paths() {
        assert_eq!(
            android_system_linkers(),
            [
                Path::new("/system/bin/linker"),
                Path::new("/system/bin/linker64"),
                Path::new("/apex/com.android.runtime/bin/linker"),
                Path::new("/apex/com.android.runtime/bin/linker64"),
            ]
        );
    }

    #[test]
    fn linker_name_outside_android_system_paths_is_not_trusted() {
        let scratch = TempDir::new("untrusted-linker-name").expect("scratch");
        let current = executable_fixture(scratch.path(), "attacker/linker64");
        let candidate = executable_fixture(scratch.path(), "release/dot");
        let trusted_linker = executable_fixture(scratch.path(), "system/bin/linker64");

        assert_eq!(
            process_executable_from(
                &current,
                Some(candidate.as_os_str()),
                Some(candidate.as_os_str()),
                &[trusted_linker.as_path()],
            )
            .expect("ordinary executable"),
            current
        );
    }

    #[test]
    fn android_linker_accepts_matching_absolute_termux_executable() {
        let scratch = TempDir::new("termux-process-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");
        let candidate = executable_fixture(scratch.path(), "release/dot");

        assert_eq!(
            process_executable_from(
                &linker,
                Some(candidate.as_os_str()),
                Some(candidate.as_os_str()),
                &[linker.as_path()],
            )
            .expect("Termux executable"),
            candidate.canonicalize().expect("canonical candidate")
        );
    }

    #[test]
    fn android_linker_rejects_missing_termux_executable() {
        let scratch = TempDir::new("termux-missing-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");

        let error = process_executable_from(
            &linker,
            Some(OsStr::new("/actual/dot")),
            None,
            &[linker.as_path()],
        )
        .expect_err("missing Termux identity must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_rejects_missing_argv0_identity() {
        let scratch = TempDir::new("termux-missing-argv0").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");
        let candidate = executable_fixture(scratch.path(), "release/dot");

        let error = process_executable_from(
            &linker,
            None,
            Some(candidate.as_os_str()),
            &[linker.as_path()],
        )
        .expect_err("missing argv0 identity must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_rejects_empty_termux_executable() {
        let scratch = TempDir::new("termux-empty-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");

        let error = process_executable_from(
            &linker,
            Some(OsStr::new("/actual/dot")),
            Some(OsStr::new("")),
            &[linker.as_path()],
        )
        .expect_err("empty Termux identity must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_rejects_relative_termux_executable() {
        let scratch = TempDir::new("termux-relative-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");
        let candidate = executable_fixture(scratch.path(), "release/dot");

        let error = process_executable_from(
            &linker,
            Some(candidate.as_os_str()),
            Some(OsStr::new("relative/dot")),
            &[linker.as_path()],
        )
        .expect_err("relative Termux identity must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_rejects_relative_argv0_identity() {
        let scratch = TempDir::new("termux-relative-argv0").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");
        let candidate = executable_fixture(scratch.path(), "release/dot");

        let error = process_executable_from(
            &linker,
            Some(OsStr::new("relative/dot")),
            Some(candidate.as_os_str()),
            &[linker.as_path()],
        )
        .expect_err("relative argv0 identity must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_rejects_nonexistent_termux_executable() {
        let scratch = TempDir::new("termux-nonexistent-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");
        let missing = scratch.path().join("missing/dot");

        let error = process_executable_from(
            &linker,
            Some(missing.as_os_str()),
            Some(missing.as_os_str()),
            &[linker.as_path()],
        )
        .expect_err("nonexistent Termux identity must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_rejects_mismatched_argv0() {
        let scratch = TempDir::new("termux-mismatched-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "system/bin/linker64");
        let candidate = executable_fixture(scratch.path(), "release/dot");
        let other = executable_fixture(scratch.path(), "other/dot");

        let error = process_executable_from(
            &linker,
            Some(other.as_os_str()),
            Some(candidate.as_os_str()),
            &[linker.as_path()],
        )
        .expect_err("mismatched executable identities must fail closed");
        assert_eq!(error.to_string(), "cannot resolve process executable");
    }

    #[test]
    fn android_linker_accepts_canonically_matching_symlink_spellings() {
        use std::os::unix::fs::symlink;

        let scratch = TempDir::new("termux-symlink-executable").expect("scratch");
        let linker = executable_fixture(scratch.path(), "apex/linker64");
        let linker_alias = scratch.path().join("system/bin/linker64");
        std::fs::create_dir_all(linker_alias.parent().expect("linker alias parent"))
            .expect("linker alias parent");
        symlink(&linker, &linker_alias).expect("linker alias");
        let candidate = executable_fixture(scratch.path(), "release/dot");
        let candidate_alias = scratch.path().join("release/dot-link");
        symlink(&candidate, &candidate_alias).expect("candidate alias");

        assert_eq!(
            process_executable_from(
                &linker_alias,
                Some(candidate_alias.as_os_str()),
                Some(candidate.as_os_str()),
                &[linker.as_path()],
            )
            .expect("matching canonical identities"),
            candidate.canonicalize().expect("canonical candidate")
        );
    }

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
    fn cargo_source_root_is_limited_to_its_compiled_profile() {
        let scratch = TempDir::new("cargo-source-root").expect("scratch");
        let manifest = scratch.path().join("checkout");
        let decoy = scratch.path().join("decoy-checkout");
        let profile = decoy.join("external-target/debug");
        let out_dir = profile.join("build/dot-fixture/out");
        let executable = profile.join("deps/dot-fixture");
        let copied = scratch.path().join("copied-dot");
        std::fs::create_dir_all(manifest.join("lib/dot/public")).expect("public API");
        std::fs::write(manifest.join("Cargo.toml"), b"[package]\n").expect("manifest");
        std::fs::create_dir_all(decoy.join("lib/dot/public")).expect("decoy public API");
        std::fs::write(decoy.join("Cargo.toml"), b"[package]\n").expect("decoy manifest");
        std::fs::create_dir_all(&out_dir).expect("Cargo OUT_DIR");
        std::fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("profile deps");
        std::fs::write(&executable, b"binary").expect("profile executable");
        std::fs::write(&copied, b"binary").expect("copied executable");

        let executable = executable.canonicalize().expect("canonical executable");
        assert_eq!(
            cargo_source_root_from(&executable, &out_dir, &manifest, true),
            Some(manifest.canonicalize().expect("canonical manifest"))
        );
        assert_eq!(
            source_root_from_canonical(&executable, &out_dir, &manifest, true),
            Some(manifest.canonicalize().expect("canonical manifest")),
            "compiled ownership must win over a surrounding checkout"
        );
        assert_eq!(
            cargo_source_root_from(&copied, &out_dir, &manifest, true),
            None
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
