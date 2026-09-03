//! Differential parity tests: the Rust config parser must agree with
//! the shell `dot_config_load` on a shared corpus, byte-for-byte.
//!
//! Each case runs the real shell implementation in a clean `bash`
//! subprocess (isolated HOME, controlled env) and compares the published
//! variables — or the exact stderr diagnostic and exit code — against
//! `dot::config::load`. Any divergence fails here, not in production.
//!
//! Requires `bash` and the shell tree at `CARGO_MANIFEST_DIR`; that is
//! always true in this repo (the shell is the behavior owner).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::config::{Provider, Request, UpdatePolicy, load};
use dot::test_support::TempDir;

/// Fail loudly (not as a byte-diff) when scratch storage loses a
/// fixture between setup and engine invocation: a missing file parses
/// as defaults on both sides, which would otherwise masquerade as a
/// parser divergence.
fn require_fixture(path: &Path, context: &str) {
    assert!(
        path.is_file(),
        "fixture vanished before {context}: {}",
        path.display()
    );
}

/// Outcome of one parse, normalized so shell and Rust compare directly:
/// `Ok` carries the six published variables in shell order;
/// `Err` carries (exit code, stderr text).
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Ok(String),
    Err(i32, String),
}

fn shell_parse(
    manifest_dir: &Path,
    config_path: &Path,
    home: &Path,
    env_policy: Option<&str>,
) -> Outcome {
    // A fresh interpreter per case: the shell parser keeps globals, and
    // `config.sh` captures the env policy at source time, so sharing a
    // process across cases would leak state between them.
    let script = r#"
set -uo pipefail
. "$1/lib/dot/public/xdg.sh"
. "$1/lib/dot/config.sh"
if dot_config_load "$2"; then
  printf '%s|%s|%s|%s|%s|%s\n' \
    "$DOT_CONFIG_VERSION" "$DOT_EXTENSION_API" "$DOT_EXTENSIONS_DIR" \
    "$DOT_DEPENDENCY_PROVIDER" "$DOT_DEFAULT_PROFILE" "$DOT_SHDEPS_UPDATE_POLICY"
else
  # Propagate the failure code with no extra output: stderr must contain
  # only the shell's own diagnostic for the byte comparison to hold.
  exit $?
fi
"#;
    let mut cmd = Command::new("bash");
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(script)
        .arg("dot-test-sh")
        .arg(manifest_dir)
        .arg(config_path)
        .env("HOME", home)
        .env_remove("DOT_SHDEPS_UPDATE_POLICY")
        .env_remove("DOT_CONFIG_VERSION")
        .env_remove("DOT_EXTENSION_API")
        .env_remove("DOT_EXTENSIONS_DIR")
        .env_remove("DOT_DEPENDENCY_PROVIDER")
        .env_remove("DOT_DEFAULT_PROFILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(policy) = env_policy {
        cmd.env("DOT_SHDEPS_UPDATE_POLICY", policy);
    }
    let output = cmd.output().expect("spawn bash");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match output.status.code() {
        Some(0) => Outcome::Ok(stdout),
        Some(code) => Outcome::Err(code, stderr),
        None => panic!("shell killed by signal: {stderr}"),
    }
}

fn rust_parse(config_path: &Path, home: &str, env_policy: Option<&str>) -> Outcome {
    match load(&Request {
        config_path: Some(config_path),
        home,
        env_policy,
    }) {
        Ok(config) => Outcome::Ok(format!(
            "{}|{}|{}|{}|{}|{}\n",
            config.version,
            if config.extension_api { "1" } else { "" },
            config.extensions_dir.as_deref().unwrap_or(""),
            match config.provider {
                Provider::None => "none",
                Provider::Shdeps => "shdeps",
            },
            config.default_profile,
            match config.shdeps_update_policy {
                UpdatePolicy::Pinned => "pinned",
                UpdatePolicy::Latest => "latest",
            },
        )),
        Err(err) => Outcome::Err(1, format!("{err}\n")),
    }
}

/// Valid-config corpus: every combination the shell suite exercises,
/// plus path-shape edges. Compared field-by-field via the pipe format.
const VALID_CORPUS: &[&str] = &[
    "version=1\n",
    "version=1",
    "# only a comment\n\nversion=1\n",
    "version=1\nextension_api=1\nextensions_dir=${HOME}/.local/lib/dotfiles\ndependency_provider=shdeps\ndefault_profile=dev\nshdeps_update_policy=latest\n",
    "version=1\nextension_api=1\nextensions_dir=~/x\n",
    "version=1\nextension_api=1\nextensions_dir=$HOME/x\n",
    "version=1\nextension_api=1\nextensions_dir=/srv/dotfiles\n",
    "version=1\ndefault_profile=a-b-c-9\n",
    "version=1\nshdeps_update_policy=pinned\n",
];

/// Invalid-config corpus: exact stderr text must match.
const INVALID_CORPUS: &[&str] = &[
    "extension_api=1\n",
    "version=2\n",
    "version=1\nversion=1\n",
    "version=1\nbogus=1\n",
    "version=1\nBad_Key=1\n",
    "version=1\n=1\n",
    "version\n",
    "not a setting\n",
    "version=1\\\n",
    "version=1\nextension_api=1\nextension_api=1\n",
    "version=1\nextension_api=yes\n",
    "version=1\nextensions_dir=/x\n",
    "version=1\nextensions_dir=~/$HOME/x\n",
    "version=1\nextensions_dir=relative/path\n",
    "version=1\ndependency_provider=apt\n",
    "version=1\ndependency_provider=Shdeps\n",
    "version=1\ndefault_profile=\n",
    "version=1\ndefault_profile=9abc\n",
    "version=1\nshdeps_update_policy=sometimes\n",
    "version=1\nshdeps_update_policy=PINNED\n",
];

/// Env-policy sweep applied to one fixed valid file.
const ENV_POLICIES: &[Option<&str>] = &[None, Some("pinned"), Some("latest"), Some("bogus")];

#[test]
fn rust_matches_shell_on_valid_corpus() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let home_str = home.to_str().expect("home utf8");
    for (index, body) in VALID_CORPUS.iter().enumerate() {
        let path = scratch.write(&format!("valid-{index}"), body.as_bytes());
        for env_policy in ENV_POLICIES {
            require_fixture(&path, "shell parse");
            let shell = shell_parse(&manifest, &path, &home, *env_policy);
            require_fixture(&path, "rust parse");
            let rust = rust_parse(&path, home_str, *env_policy);
            assert_eq!(
                rust, shell,
                "divergence on valid case {index} env {env_policy:?} body {body:?}"
            );
        }
    }
}

#[test]
fn rust_matches_shell_on_invalid_corpus() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let home_str = home.to_str().expect("home utf8");
    for (index, body) in INVALID_CORPUS.iter().enumerate() {
        let path = scratch.write(&format!("invalid-{index}"), body.as_bytes());
        require_fixture(&path, "shell parse");
        let shell = shell_parse(&manifest, &path, &home, None);
        require_fixture(&path, "rust parse");
        let rust = rust_parse(&path, home_str, None);
        assert_eq!(
            rust, shell,
            "divergence on invalid case {index} body {body:?}"
        );
    }
}

#[test]
fn rust_matches_shell_on_missing_file() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let home_str = home.to_str().expect("home utf8");
    let missing = scratch.path().join("does-not-exist");
    let shell = shell_parse(&manifest, &missing, &home, None);
    let rust = rust_parse(&missing, home_str, None);
    assert_eq!(rust, shell);
}

/// The shell prints nothing on stdout for rejections and returns 1;
/// assert the shape explicitly so a stderr/stdout swap cannot hide.
#[test]
fn rejection_shape_is_stderr_and_exit_1() {
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    let path = scratch.write("bad", b"version=2\n");
    match rust_parse(&path, "/home/u", None) {
        Outcome::Err(code, stderr) => {
            assert_eq!(code, 1);
            assert!(stderr.starts_with("dot: config: "));
        }
        Outcome::Ok(_) => panic!("must reject"),
    }
}
