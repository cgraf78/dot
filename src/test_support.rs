//! Test-only helpers: isolated temp directories for differential harnesses.
//!
//! Public so integration tests under `tests/` share one isolation
//! convention; never used by the shipped engine. Mirrors the `shdeps`
//! `test_support` pattern with std only (no `tempfile` dev-dependency
//! in slice 1): pid plus a process-wide atomic counter keeps parallel
//! tests collision-free without wall-clock reads (immune to NTP steps
//! and coarse clocks), paths are canonicalized, and the guard removes
//! the directory on drop.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static BASH: OnceLock<PathBuf> = OnceLock::new();

/// Absolute path to the engine-runtime `bash` for differential harnesses.
///
/// Resolved once from the parent test process PATH — never from a
/// child's scrubbed or swapped env — so hermetic children still spawn
/// the interpreter the shell suite runs under (`#!/usr/bin/env bash`)
/// and the `DOT_BASH` engine runtime resolves. A bare `bash` looked up
/// at spawn time instead follows the child's env: under `env_clear`
/// without PATH that falls back to the OS default lookup, which finds
/// the macOS 3.2 trampoline whose `case`-pattern corners (e.g. a
/// trailing lone backslash) differ from the pinned 5.x runtime — a
/// silent wrong oracle surfacing as a mystery divergence.
///
/// The first `bash` on PATH reporting major version 4+ wins; with no
/// such binary resolution panics instead of testing the wrong runtime.
pub fn bash() -> &'static std::path::Path {
    BASH.get_or_init(|| {
        let mut tried: Vec<PathBuf> = Vec::new();
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            // Empty entries mean "cwd" to the shell, but a relative
            // interpreter would resolve against whatever directory a
            // later child sets — skip them rather than bind the oracle
            // to a test's working directory.
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("bash");
            if candidate.is_file() {
                if bash_major(&candidate) >= 4 {
                    return candidate;
                }
                tried.push(candidate);
            }
        }
        panic!(
            "no bash 4+ on PATH for the differential oracle (saw: {tried:?}); \
             install bash 5.x, the pinned engine runtime",
        );
    })
}

/// Major version from `$BASH_VERSION` (`5.2.37(1)-release` -> 5);
/// 0 when the binary cannot be asked (treated as unusable above).
fn bash_major(candidate: &std::path::Path) -> u64 {
    let Ok(output) = std::process::Command::new(candidate)
        .arg("-c")
        .arg("echo $BASH_VERSION")
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Owned isolated temp directory, removed on drop.
///
/// Scaffolding for slice 2 (first consumer: config-parser tests); kept
/// rather than re-added so the naming/isolation convention is settled
/// before the slices that depend on it.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `dot-<name>-<pid>-<n>` under the system temp directory.
    ///
    /// `name` is a fixed test label, not user input, but it is validated
    /// anyway: a `..` or separator would break the isolation the type
    /// advertises, and tests copy-paste labels freely.
    pub fn new(name: &str) -> std::io::Result<Self> {
        Self::new_in(&std::env::temp_dir(), name)
    }

    /// Create the same layout for fixtures that must EXECUTE (fake
    /// binaries probed by `Command`). The system temp directory is not
    /// exec-capable everywhere — some CI images mount it `noexec` — so
    /// these live under the Cargo target directory instead, which is
    /// exec-capable by construction (test binaries run from it).
    /// `CARGO_TARGET_DIR` is honored when exported; otherwise this is
    /// `<crate>/target/dot-test-fixtures` (gitignored build output).
    pub fn new_exec(name: &str) -> std::io::Result<Self> {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("dot-test-fixtures");
        Self::new_in(&root, name)
    }

    fn new_in(root: &std::path::Path, name: &str) -> std::io::Result<Self> {
        if name.is_empty()
            || name.contains(['/', '\\', '\0'])
            || name.split(std::path::MAIN_SEPARATOR).count() != 1
            || name == "."
            || name == ".."
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsafe temp dir label: {name:?}"),
            ));
        }
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = root.join(format!("dot-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        Ok(Self { path })
    }

    /// Isolated directory path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Write fixture `bytes` to `name` inside the directory and return
    /// its path. `name` is a single path segment (same rules as labels);
    /// the post-write metadata check turns a vanished scratch dir into
    /// an explicit environmental panic instead of a misleading
    /// engine-output diff downstream.
    pub fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        if name.is_empty() || name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
            panic!("unsafe fixture name: {name:?}");
        }
        let path = self.path.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        // Post-write stat: the file must be visible before either engine
        // runs; if scratch storage is flaky the failure points here.
        let size = std::fs::metadata(&path).expect("fixture visible").len();
        assert_eq!(size, bytes.len() as u64, "fixture short write: {name:?}");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dirs_are_unique_and_cleaned_up() {
        let first = TempDir::new("probe").expect("create temp dir");
        let second = TempDir::new("probe").expect("create temp dir");
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists());
    }

    #[test]
    fn exec_dir_runs_fixture_binaries() {
        // Guards the `noexec`-tmp CI failure: `new_exec` dirs must run
        // fixture binaries (the target dir is exec-capable by
        // construction, since test binaries execute from it).
        let dir = TempDir::new_exec("probe").expect("exec dir");
        let script = dir.path().join("probe.sh");
        std::fs::write(&script, "#!/bin/sh\necho ok\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            // Under parallel load the kernel can report the
            // just-written script busy (ETXTBSY) on first exec;
            // retry that transient only, with a bounded deadline.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let output = loop {
                match std::process::Command::new(&script).output() {
                    Err(e)
                        if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::yield_now();
                        continue;
                    }
                    result => break result.expect("spawn fixture"),
                }
            };
            assert!(output.status.success());
            assert_eq!(output.stdout, b"ok\n");
        }
    }

    #[test]
    fn oracle_bash_is_absolute_and_modern() {
        // Regression pin for the macOS CI failure where a bare `bash`
        // under `env_clear` fell back to the 3.2 trampoline: the oracle
        // must be an absolute 4+ binary resolved from the parent PATH.
        let bash = super::bash();
        assert!(bash.is_absolute(), "oracle bash must be absolute");
        assert!(
            super::bash_major(bash) >= 4,
            "oracle bash must be 4+, got: {}",
            bash.display()
        );
    }

    #[test]
    fn unsafe_labels_are_rejected() {
        for label in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(
                TempDir::new(label).is_err(),
                "label must be rejected: {label:?}"
            );
        }
    }
}
