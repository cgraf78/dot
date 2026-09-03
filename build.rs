//! Build script: resolve the source revision and release version.
//!
//! Unlike the sibling `shdeps` port, a missing revision is NOT fatal here:
//! the shell contract defines `dot version` as printing `unknown` when git
//! is unavailable, so the build falls back to `unknown` instead of
//! panicking. New code must preserve that fallback.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Environment prefix for build overrides, mirroring the sibling repos.
const PREFIX: &str = "DOT_BUILD";

/// Resolve the full commit SHA: explicit env, then GitHub, then git.
///
/// Ordering is precedence, highest first: a release build stamps an exact
/// commit via `DOT_BUILD_COMMIT` (reproducible even from an exported
/// tarball with no `.git`), CI falls back to `GITHUB_SHA`, and only local
/// developer builds shell out to git. git itself resolves the owning repo
/// from the manifest dir (worktrees, submodules, ceilings included) —
///
/// the script must NOT walk up manually: climbing past the owning repo
/// would silently bake an unrelated parent checkout's SHA into the
/// binary (e.g. a crate copy nested inside another repo).
fn resolve_commit(manifest_dir: &std::path::Path) -> String {
    if let Ok(commit) = env::var(format!("{PREFIX}_COMMIT")) {
        if !commit.trim().is_empty() {
            return commit.trim().to_string();
        }
    }
    if let Ok(sha) = env::var("GITHUB_SHA") {
        if !sha.trim().is_empty() {
            return sha.trim().to_string();
        }
    }
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(manifest_dir)
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !sha.is_empty() {
                return sha;
            }
        }
    }
    // Shell contract: `dot version` prints `unknown`, never fails here.
    "unknown".to_string()
}

/// Resolve the git dir owning the manifest (worktree gitfiles included)
/// so rebuild tracking watches the real HEAD, not a dangling path.
fn resolve_git_dir(manifest_dir: &std::path::Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--git-dir")
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if git_dir.is_empty() {
        return None;
    }
    let path = PathBuf::from(git_dir);
    Some(if path.is_absolute() {
        path
    } else {
        manifest_dir.join(path)
    })
}

/// A usable revision is the lowercased first 12 hex chars of the SHA.
fn short_revision(commit: &str) -> String {
    let short: String = commit.chars().take(12).collect();
    if short.len() == 12 && short.chars().all(|c| c.is_ascii_hexdigit()) {
        short.to_ascii_lowercase()
    } else {
        "unknown".to_string()
    }
}

/// Resolve the release version: explicit env, else `unknown`.
/// Slice 1 has no `scripts/release-version.sh` yet; later slices introduce
/// the shared `YYYYMMDD-HHMMSS-8hex` scheme and validate it here.
fn resolve_version() -> String {
    if let Ok(version) = env::var(format!("{PREFIX}_VERSION")) {
        let version = version.trim().to_string();
        if !version.is_empty() {
            return version;
        }
    }
    "unknown".to_string()
}

fn main() {
    println!("cargo:rerun-if-env-changed={PREFIX}_COMMIT");
    println!("cargo:rerun-if-env-changed={PREFIX}_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    // In a linked worktree `.git` is a file, not a dir, so watching the
    // literal `.git/HEAD` would never fire and the binary would report a
    // stale SHA. Watch the resolved git dir instead. (Thin checkouts whose
    // HEAD moves without touching these paths still need an explicit
    // `DOT_BUILD_COMMIT`; that limitation is inherent to mtime tracking.)
    match resolve_git_dir(&manifest_dir) {
        Some(git_dir) => {
            println!("cargo:rerun-if-changed={}/HEAD", git_dir.display());
            println!("cargo:rerun-if-changed={}/packed-refs", git_dir.display());
        }
        None => {
            println!("cargo:rerun-if-changed=.git/HEAD");
            println!("cargo:rerun-if-changed=.git/packed-refs");
        }
    }

    let commit = resolve_commit(&manifest_dir);
    let short = short_revision(&commit);
    let version = resolve_version();
    println!("cargo:rustc-env={PREFIX}_COMMIT={commit}");
    println!("cargo:rustc-env={PREFIX}_SHORT_COMMIT={short}");
    println!("cargo:rustc-env={PREFIX}_VERSION={version}");
}
