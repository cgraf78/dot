//! Lazy Bash capability selection for the retained shell boundaries.
//!
//! The native engine does not require Bash. User hooks and the reviewed Shdeps
//! bootstrap do, so those boundaries resolve one Bash 4+ interpreter from the
//! invocation snapshot and reuse it for the lifetime of that Runtime.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROBE_PREFIX: &[u8] = b"cgraf78-dot-bash-v1:";
const FIXED_CANDIDATES: [&str; 5] = [
    "/opt/homebrew/bin/bash",
    "/usr/local/bin/bash",
    "/opt/local/bin/bash",
    "/usr/bin/bash",
    "/bin/bash",
];

/// Shell startup controls removed before a trusted noninteractive boundary.
const STARTUP_CONTROLS: [&str; 9] = [
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
    "BASH_COMPAT",
    "POSIXLY_CORRECT",
    "BASH_XTRACEFD",
    "BASHOPTS",
    "SHELLOPTS",
];

/// One validated Bash interpreter and the version observed during selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    path: PathBuf,
    version: Vec<u8>,
    major: u64,
}

impl Resolved {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn version(&self) -> &[u8] {
        &self.version
    }

    pub(crate) fn major(&self) -> u64 {
        self.major
    }
}

/// Why an invocation could not acquire its Bash capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Error {
    /// A non-empty `DOT_BASH` is strict and therefore forbids fallback.
    InvalidExplicit(OsString),
    /// No soft candidate proved to be Bash 4 or newer.
    NotFound,
}

impl Error {
    /// Return the historical resolver diagnostic without a trailing newline.
    pub(crate) fn message(&self) -> Vec<u8> {
        match self {
            Self::InvalidExplicit(path) => {
                let mut message =
                    b"checkout Bash resolver: explicit interpreter is not Bash 4 or newer: "
                        .to_vec();
                message.extend_from_slice(path.as_bytes());
                message
            }
            Self::NotFound => {
                b"checkout Bash resolver: Bash 4 or newer was not found; install it and retry"
                    .to_vec()
            }
        }
    }

    /// Return the historical resolver diagnostic as one complete line.
    pub(crate) fn line(&self) -> Vec<u8> {
        let mut line = self.message();
        line.push(b'\n');
        line
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&String::from_utf8_lossy(&self.message()))
    }
}

/// Resolve the invocation's Bash boundary with the historical precedence.
pub(crate) fn resolve(env: &BTreeMap<OsString, OsString>, cwd: &Path) -> Result<Resolved, Error> {
    resolve_with_fixed(env, cwd, &FIXED_CANDIDATES.map(Path::new))
}

fn resolve_with_fixed(
    env: &BTreeMap<OsString, OsString>,
    cwd: &Path,
    fixed: &[&Path],
) -> Result<Resolved, Error> {
    if let Some(explicit) = value(env, "DOT_BASH") {
        return probe(Path::new(explicit), env, cwd)
            .ok_or_else(|| Error::InvalidExplicit(explicit.to_os_string()));
    }

    if let Some(candidate) = value(env, "BASH").and_then(|path| probe(Path::new(path), env, cwd)) {
        return Ok(candidate);
    }

    if let Some(path) = env.get(OsStr::new("PATH")) {
        for directory in std::env::split_paths(path) {
            if !directory.is_absolute() {
                continue;
            }
            // Preserve the exact spelling from PATH. The historical resolver
            // formed `$entry/bash` and rejected the resulting double slash
            // when an entry already ended in `/`.
            let mut bytes = directory.as_os_str().as_bytes().to_vec();
            bytes.extend_from_slice(b"/bash");
            if let Some(candidate) = probe(Path::new(&OsString::from_vec(bytes)), env, cwd) {
                return Ok(candidate);
            }
        }
    }

    if let Some(prefix) = value(env, "PREFIX") {
        let mut bytes = prefix.as_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/bash");
        if let Some(candidate) = probe(Path::new(&OsString::from_vec(bytes)), env, cwd) {
            return Ok(candidate);
        }
    }

    fixed
        .iter()
        .find_map(|candidate| probe(candidate, env, cwd))
        .ok_or(Error::NotFound)
}

fn probe(candidate: &Path, env: &BTreeMap<OsString, OsString>, cwd: &Path) -> Option<Resolved> {
    if !normalized_absolute(candidate) || !executable(candidate) {
        return None;
    }
    let mut command = Command::new(candidate);
    command
        .args([
            "--noprofile",
            "--norc",
            "-c",
            "printf 'cgraf78-dot-bash-v1:%s:%s\\n' \"${BASH_VERSINFO[0]-}\" \"${BASH_VERSION-}\"",
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    sanitized_env(&mut command, env);
    // Match the former resolver's explicit empty values as an additional
    // defense if a future Bash startup path distinguishes unset from empty.
    command.env("BASH_ENV", "").env("ENV", "");
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let framed = output
        .stdout
        .strip_suffix(b"\n")?
        .strip_prefix(PROBE_PREFIX)?;
    let split = framed.iter().position(|byte| *byte == b':')?;
    let major = std::str::from_utf8(&framed[..split]).ok()?.parse().ok()?;
    let version = framed.get(split + 1..)?.to_vec();
    if major < 4
        || version.is_empty()
        || version
            .iter()
            .any(|byte| matches!(*byte, b'\n' | b'\r' | 0))
    {
        return None;
    }
    Some(Resolved {
        path: candidate.to_path_buf(),
        version,
        major,
    })
}

fn normalized_absolute(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    path.is_absolute()
        && bytes != b"/"
        && !bytes.ends_with(b"/")
        && !bytes.windows(2).any(|pair| pair == b"//")
        && !bytes.contains(&b'\n')
        && !bytes.contains(&b'\r')
        && !bytes
            .split(|byte| *byte == b'/')
            .any(|component| matches!(component, b"." | b".."))
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn value<'a>(env: &'a BTreeMap<OsString, OsString>, key: &str) -> Option<&'a OsStr> {
    env.get(OsStr::new(key))
        .filter(|value| !value.is_empty())
        .map(OsString::as_os_str)
}

/// Apply the snapshotted environment minus controls Bash evaluates at startup.
pub(crate) fn sanitized_env(command: &mut Command, env: &BTreeMap<OsString, OsString>) {
    command.env_clear().envs(env);
    for key in STARTUP_CONTROLS {
        command.env_remove(key);
    }
    for (key, value) in env {
        // Bash has used several exported-function encodings across the 4.x
        // line. Reserve its entire prefixed namespace and also remove the
        // legacy value form so the child sees no representation that a
        // supported interpreter could import before running trusted code.
        if exported_function(key) || legacy_exported_function(value) {
            command.env_remove(key);
        }
    }
}

fn exported_function(key: &OsStr) -> bool {
    key.as_bytes().starts_with(b"BASH_FUNC_")
}

fn legacy_exported_function(value: &OsStr) -> bool {
    value.as_bytes().starts_with(b"() {")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    use dot_test_support::TempDir;

    fn environment(entries: &[(&str, &OsStr)]) -> BTreeMap<OsString, OsString> {
        entries
            .iter()
            .map(|(key, value)| (OsString::from(key), (*value).to_os_string()))
            .collect()
    }

    fn bash_link(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("Bash parent")).expect("Bash parent");
        symlink(dot_test_support::bash(), &path).expect("Bash link");
        path
    }

    fn old_bash(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("Bash parent")).expect("Bash parent");
        std::fs::write(
            &path,
            b"#!/bin/sh\nprintf 'cgraf78-dot-bash-v1:3:3.2.57(1)-release\\n'\n",
        )
        .expect("old Bash fixture");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("old Bash mode");
        path
    }

    #[test]
    fn normalized_paths_match_the_checkout_resolver_contract() {
        for valid in ["/bin/bash", "/a b/bash", "/a/.hidden/bash"] {
            assert!(normalized_absolute(Path::new(valid)), "valid: {valid}");
        }
        for invalid in [
            "",
            "/",
            "bash",
            "/bin/bash/",
            "//bin/bash",
            "/opt//bin/bash",
            "/opt/./bin/bash",
            "/opt/bin/../bash",
            "/bin/bash\n",
            "/bin/bash\r",
        ] {
            assert!(
                !normalized_absolute(Path::new(invalid)),
                "invalid: {invalid:?}"
            );
        }
    }

    #[test]
    fn exported_function_prefix_is_reserved_for_all_suffixes() {
        assert!(exported_function(OsStr::new("BASH_FUNC_name%%")));
        assert!(exported_function(OsStr::new("BASH_FUNC_name()")));
        assert!(exported_function(OsStr::new("BASH_FUNC_name")));
        assert!(!exported_function(OsStr::new("NOT_BASH_FUNC_name%%")));
    }

    #[test]
    fn legacy_exported_function_values_are_recognized() {
        assert!(legacy_exported_function(OsStr::new(
            "() { printf poison; }"
        )));
        assert!(legacy_exported_function(OsStr::new("() {\n  :\n}")));
        assert!(!legacy_exported_function(OsStr::new("ordinary value")));
        assert!(!legacy_exported_function(OsStr::new(" () { :; }")));
    }

    #[test]
    fn sanitized_environment_removes_every_function_record_encoding() {
        let env_binary = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("env"))
            .find(|candidate| {
                std::fs::metadata(candidate).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .expect("env utility on the test PATH");
        let mut env = environment(&[
            (
                "PATH",
                std::env::var_os("PATH").unwrap_or_default().as_os_str(),
            ),
            ("BASH_FUNC_modern%%", OsStr::new("() { :; }")),
            ("BASH_FUNC_vendor()", OsStr::new("() { :; }")),
            ("legacy_function", OsStr::new("() { :; }")),
            ("ordinary", OsStr::new("prefix () { :; }")),
        ]);
        env.insert(OsString::from("kept"), OsString::from("yes"));
        let mut command = Command::new(env_binary);
        sanitized_env(&mut command, &env);

        let output = command.output().expect("sanitized environment probe");
        assert!(output.status.success());
        let records: Vec<&[u8]> = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|record| !record.is_empty())
            .collect();
        assert!(records.contains(&b"kept=yes".as_slice()));
        assert!(records.contains(&b"ordinary=prefix () { :; }".as_slice()));
        assert!(!records.iter().any(|record| {
            record.starts_with(b"BASH_FUNC_") || record.starts_with(b"legacy_function=")
        }));
    }

    #[test]
    fn explicit_error_preserves_non_utf8_path_bytes() {
        let path = OsString::from_vec(b"/tmp/bash-\xff".to_vec());
        let error = Error::InvalidExplicit(path);
        assert_eq!(
            error.message(),
            b"checkout Bash resolver: explicit interpreter is not Bash 4 or newer: /tmp/bash-\xff"
        );
    }

    #[test]
    fn not_found_error_matches_the_checkout_resolver() {
        assert_eq!(
            Error::NotFound.message(),
            b"checkout Bash resolver: Bash 4 or newer was not found; install it and retry"
        );
    }

    #[test]
    fn empty_dot_bash_allows_the_soft_bash_hint() {
        let scope = TempDir::new_exec("bash-empty-override").expect("scope");
        let candidate = bash_link(scope.path(), "hint/bash");
        let env = environment(&[
            ("DOT_BASH", OsStr::new("")),
            ("BASH", candidate.as_os_str()),
        ]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[])
                .expect("soft Bash hint")
                .path(),
            candidate
        );
    }

    #[test]
    fn valid_bash_hint_precedes_path() {
        let scope = TempDir::new_exec("bash-hint-before-path").expect("scope");
        let hint = bash_link(scope.path(), "hint/bash");
        let path_bash = bash_link(scope.path(), "path/bash");
        let env = environment(&[
            ("BASH", hint.as_os_str()),
            (
                "PATH",
                path_bash.parent().expect("PATH directory").as_os_str(),
            ),
        ]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[])
                .expect("BASH hint")
                .path(),
            hint
        );
    }

    #[test]
    fn relative_dot_bash_is_a_strict_error() {
        let scope = TempDir::new_exec("bash-relative-override").expect("scope");
        let fallback = bash_link(scope.path(), "fallback/bash");
        let env = environment(&[
            ("DOT_BASH", OsStr::new("relative/bash")),
            ("BASH", fallback.as_os_str()),
        ]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[]),
            Err(Error::InvalidExplicit(OsString::from("relative/bash")))
        );
    }

    #[test]
    fn pre_v4_dot_bash_is_a_strict_error() {
        let scope = TempDir::new_exec("bash-old-override").expect("scope");
        let old = old_bash(scope.path(), "old/bash");
        let fallback = bash_link(scope.path(), "fallback/bash");
        let env = environment(&[
            ("DOT_BASH", old.as_os_str()),
            ("BASH", fallback.as_os_str()),
        ]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[]),
            Err(Error::InvalidExplicit(old.into_os_string()))
        );
    }

    #[test]
    fn explicit_probe_rejects_trailing_output() {
        let scope = TempDir::new_exec("bash-noisy-override").expect("scope");
        let candidate = scope.path().join("noisy/bash");
        std::fs::create_dir_all(candidate.parent().expect("Bash parent")).expect("Bash parent");
        std::fs::write(
            &candidate,
            b"#!/bin/sh\nprintf 'cgraf78-dot-bash-v1:5:5.2.15(1)-release\\nunexpected\\n'\n",
        )
        .expect("noisy Bash fixture");
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))
            .expect("noisy Bash mode");
        let env = environment(&[("DOT_BASH", candidate.as_os_str())]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[]),
            Err(Error::InvalidExplicit(candidate.into_os_string()))
        );
    }

    #[test]
    fn path_skips_pre_v4_candidates() {
        let scope = TempDir::new_exec("bash-old-path").expect("scope");
        let old = old_bash(scope.path(), "old/bash");
        let current = bash_link(scope.path(), "current/bash");
        let path = std::env::join_paths([
            old.parent().expect("old PATH directory"),
            current.parent().expect("current PATH directory"),
        ])
        .expect("PATH");
        let env = environment(&[("PATH", path.as_os_str())]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[])
                .expect("current Bash")
                .path(),
            current
        );
    }

    #[test]
    fn path_uses_the_first_valid_candidate() {
        let scope = TempDir::new_exec("bash-path-order").expect("scope");
        let first = bash_link(scope.path(), "first/bash");
        let second = bash_link(scope.path(), "second/bash");
        let path = std::env::join_paths([
            first.parent().expect("first PATH directory"),
            second.parent().expect("second PATH directory"),
        ])
        .expect("PATH");
        let env = environment(&[("PATH", path.as_os_str())]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[])
                .expect("first PATH Bash")
                .path(),
            first
        );
    }

    #[test]
    fn path_skips_entries_with_a_trailing_slash() {
        let scope = TempDir::new_exec("bash-trailing-path").expect("scope");
        let path_bash = bash_link(scope.path(), "path/bash");
        let fixed = bash_link(scope.path(), "fixed/bash");
        let mut path = path_bash
            .parent()
            .expect("PATH directory")
            .as_os_str()
            .as_bytes()
            .to_vec();
        path.push(b'/');
        let env = environment(&[("PATH", OsStr::from_bytes(&path))]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[fixed.as_path()])
                .expect("fixed Bash")
                .path(),
            fixed
        );
    }

    #[test]
    fn path_precedes_prefix() {
        let scope = TempDir::new_exec("bash-path-before-prefix").expect("scope");
        let path_bash = bash_link(scope.path(), "path/bash");
        let prefix = scope.path().join("prefix");
        let _prefix_bash = bash_link(&prefix, "bin/bash");
        let env = environment(&[
            (
                "PATH",
                path_bash.parent().expect("PATH directory").as_os_str(),
            ),
            ("PREFIX", prefix.as_os_str()),
        ]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[])
                .expect("PATH Bash")
                .path(),
            path_bash
        );
    }

    #[test]
    fn prefix_precedes_fixed_candidates() {
        let scope = TempDir::new_exec("bash-prefix-before-fixed").expect("scope");
        let prefix = scope.path().join("prefix");
        let prefix_bash = bash_link(&prefix, "bin/bash");
        let fixed = bash_link(scope.path(), "fixed/bash");
        let env = environment(&[
            ("PATH", OsStr::new("/definitely/missing")),
            ("PREFIX", prefix.as_os_str()),
        ]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[fixed.as_path()])
                .expect("PREFIX Bash")
                .path(),
            prefix_bash
        );
    }

    #[test]
    fn fixed_candidate_is_the_last_fallback() {
        let scope = TempDir::new_exec("bash-fixed-fallback").expect("scope");
        let fixed = bash_link(scope.path(), "fixed/bash");
        let env = environment(&[("PATH", OsStr::new("/definitely/missing"))]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[fixed.as_path()])
                .expect("fixed Bash")
                .path(),
            fixed
        );
    }

    #[test]
    fn fixed_candidates_keep_their_declared_order() {
        let scope = TempDir::new_exec("bash-fixed-order").expect("scope");
        let first = bash_link(scope.path(), "first/bash");
        let second = bash_link(scope.path(), "second/bash");
        let env = environment(&[("PATH", OsStr::new("/definitely/missing"))]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[first.as_path(), second.as_path()])
                .expect("first fixed Bash")
                .path(),
            first
        );
    }

    #[test]
    fn no_usable_candidate_is_a_typed_error() {
        let scope = TempDir::new_exec("bash-no-candidate").expect("scope");
        let env = environment(&[("PATH", OsStr::new("/definitely/missing"))]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[]),
            Err(Error::NotFound)
        );
    }

    #[test]
    // macOS rejects this filename at the filesystem boundary with EILSEQ;
    // byte-preserving path behavior remains exercised on Unix filesystems
    // that can create the fixture.
    #[cfg(not(target_os = "macos"))]
    fn explicit_non_utf8_path_is_preserved() {
        let scope = TempDir::new_exec("bash-non-utf8").expect("scope");
        let name = OsString::from_vec(b"bash-\xff".to_vec());
        let candidate = scope.path().join(name);
        symlink(dot_test_support::bash(), &candidate).expect("Bash link");
        let env = environment(&[("DOT_BASH", candidate.as_os_str())]);

        assert_eq!(
            resolve_with_fixed(&env, scope.path(), &[])
                .expect("non-UTF-8 Bash")
                .path(),
            candidate
        );
    }
}
