//! Shared test helpers: isolated temp directories.
//!
//! Mirrors the `shdeps` `test_support` pattern with std only (no
//! `tempfile` dev-dependency in slice 1): pid plus an atomic counter
//! keeps parallel tests collision-free, paths are canonicalized, and the
//! guard removes the directory on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let path = std::env::temp_dir().join(format!("dot-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        Ok(Self { path })
    }

    /// Isolated directory path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
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
    fn unsafe_labels_are_rejected() {
        for label in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(
                TempDir::new(label).is_err(),
                "label must be rejected: {label:?}"
            );
        }
    }
}
