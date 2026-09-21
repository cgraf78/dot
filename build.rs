//! Build script: resolve the source revision and public version.
//!
//! This follows the shared Rust-repo policy: the build stamps the exact
//! commit plus the generated `YYYYMMDD-HHMMSS-8hex` version used by release
//! tags, archive names, and installer metadata. Resolution is strict — a
//! missing commit or an invalid version fails the build with an actionable
//! message instead of baking in an ambiguous `unknown`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Environment prefix for build overrides, mirroring the sibling repos.
const PREFIX: &str = "DOT_BUILD";

fn main() {
    println!("cargo:rerun-if-env-changed={PREFIX}_COMMIT");
    println!("cargo:rerun-if-env-changed={PREFIX}_VERSION");
    println!("cargo:rerun-if-env-changed={PREFIX}_TIMESTAMP");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_REF");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_TYPE");
    println!("cargo:rerun-if-changed=scripts/release-version.sh");
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    // In a linked worktree `.git` is a file, not a dir, so watching the
    // literal `.git/HEAD` would never fire and the binary would report a
    // stale version. Watch the resolved git dir instead.
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

    let commit = env_commit(&format!("{PREFIX}_COMMIT"))
        .or_else(|| env_commit("GITHUB_SHA"))
        .or_else(|| git_commit(&manifest_dir))
        .unwrap_or_else(|| {
            panic!(
                "failed to resolve dot build commit; set {PREFIX}_COMMIT \
                 to a concrete git hash when building outside a git checkout"
            );
        });
    let version = build_version(&commit);

    println!("cargo:rustc-env={PREFIX}_COMMIT={commit}");
    println!("cargo:rustc-env={PREFIX}_VERSION={version}");
}

fn env_commit(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    let trimmed = value.trim();
    if valid_commit(trimmed) {
        Some(trimmed.to_owned())
    } else if trimmed.is_empty() {
        None
    } else {
        panic!("{name} must be a concrete git hash, got {trimmed:?}");
    }
}

fn git_commit(manifest_dir: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    valid_commit(commit).then(|| commit.to_owned())
}

fn valid_commit(value: &str) -> bool {
    value.len() >= 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

fn build_version(commit: &str) -> String {
    // Keep the public version formatter in one shell helper because release
    // tags, archive names, and installer smoke tests need the exact same logic
    // without reimplementing Rust build-script details. Passing the already
    // resolved commit also lets containerized builds avoid extra Git metadata
    // reads when a safe.directory mismatch would otherwise block them.
    let output = Command::new("bash")
        .arg("scripts/release-version.sh")
        .env(format!("{PREFIX}_COMMIT"), commit)
        .output()
        .unwrap_or_else(|error| panic!("failed to run scripts/release-version.sh: {error}"));

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("failed to compute dot build version: {stderr}");
    }

    let version = String::from_utf8(output.stdout)
        .expect("release-version.sh output should be utf-8")
        .trim()
        .to_owned();
    if !valid_version(&version) {
        panic!("release-version.sh produced invalid version {version:?}");
    }
    version
}

fn valid_version(value: &str) -> bool {
    let mut parts = value.split('-');
    let date = parts.next().unwrap_or_default();
    let time = parts.next().unwrap_or_default();
    let commit = parts.next().unwrap_or_default();

    parts.next().is_none()
        && date.len() == 8
        && time.len() == 6
        && date.bytes().all(|byte| byte.is_ascii_digit())
        && time.bytes().all(|byte| byte.is_ascii_digit())
        && commit.len() == 8
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}
