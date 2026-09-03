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
/// developer builds shell out to git. The directory walk exists because
/// `cargo build` may run in a subdirectory of the checkout; probing only
/// `CARGO_MANIFEST_DIR` would miss the owning repo when the crate moves
/// into a workspace layout later.
fn resolve_commit() -> String {
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
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // Walk up so workspace checkouts resolve the owning repo, not a subdir.
    let mut dir = Some(manifest.as_path());
    while let Some(current) = dir {
        let output = Command::new("git")
            .arg("rev-parse")
            .arg("HEAD")
            .current_dir(current)
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !sha.is_empty() {
                    return sha;
                }
            }
        }
        dir = current.parent();
    }
    // Shell contract: `dot version` prints `unknown`, never fails here.
    "unknown".to_string()
}

/// A usable revision is at least 8 lowercase hex chars.
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
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let commit = resolve_commit();
    let short = short_revision(&commit);
    let version = resolve_version();
    println!("cargo:rustc-env={PREFIX}_COMMIT={commit}");
    println!("cargo:rustc-env={PREFIX}_SHORT_COMMIT={short}");
    println!("cargo:rustc-env={PREFIX}_VERSION={version}");
}
