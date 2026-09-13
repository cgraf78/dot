//! Merge-hook shared mechanics.
//!
//! Owns the merge-hooks source
//! root under XDG config, family discovery (via [`crate::families`]),
//! marker-safe names, narrow home-placeholder expansion, and the
//! sibling-temp write paths including the `jq` JSON layer. File
//! effects reuse [`crate::temp`]; the `jq` probe and the XDG inputs
//! stay explicit so tests inject fixtures deterministically.
//!
//! Warnings (yellow stderr, always printed) arrive
//! as a caller-supplied `warn` callback carrying the same text, so
//! engine callers decide where diagnostics go.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::families;
use crate::temp::{self, MoveCache};
use crate::xdg;

/// Shared inputs for the writing half: the git source root for
/// content digests and the move-tool probe cache (see
/// [`crate::merge_block::Ctx`).
pub struct Ctx<'a> {
    /// Directory `git hash-object` runs under.
    pub source_root: &'a Path,
    /// Move-tool probe cache (one per engine run).
    pub cache: &'a mut MoveCache,
    /// Collected `_warn` texts, in order.
    pub warnings: &'a mut Vec<String>,
}

/// `_merge_hook_dir`: `$XDG_CONFIG_HOME/dot/merge-hooks.d` (or the
/// `$HOME/.config` fallback) via [`crate::xdg`].
pub fn hook_dir(xdg_config: &str, home: &str) -> Result<PathBuf, xdg::Error> {
    xdg::path(xdg::Kind::Config, "dot/merge-hooks.d", xdg_config, home).map(PathBuf::from)
}

/// `_merge_hook_source` / `_merge_hook_family`: join one segment to
/// the hooks root.
pub fn hook_source(hooks_root: &Path, name: &OsStr) -> PathBuf {
    hooks_root.join(name)
}

/// `_merge_hook_family`: resolve a merge-hook source family
/// directory by joining the family name to the hooks root. Same
/// join as [`hook_source`]; the shell keeps a separate name so
/// hook authors read family roots distinctly from single sources.
pub fn family(hooks_root: &Path, name: &OsStr) -> PathBuf {
    hook_source(hooks_root, name)
}

/// `_merge_hook_family_files`: ordered source stream for a family.
pub fn family_files(family_dir: &Path) -> Result<Vec<PathBuf>, families::Error> {
    families::family_files(Some(family_dir), &[])
}

/// `_merge_hook_family_files_matching`: family stream filtered by
/// shell patterns over the family-relative path.
pub fn family_files_matching(
    family_dir: &Path,
    patterns: &[&[u8]],
) -> Result<Vec<PathBuf>, families::Error> {
    families::family_files(Some(family_dir), patterns)
}

/// `_merge_hook_family_relpath`: strip the `family/` prefix, or
/// return the path unchanged when it is outside the family (the
/// shell `${file#"$dir/"}`).
pub fn family_relpath(family_dir: &Path, file: &Path) -> OsString {
    let dir = family_dir.as_os_str().as_bytes();
    let path = file.as_os_str().as_bytes();
    let mut prefix = dir.to_vec();
    prefix.push(b'/');
    match path.strip_prefix(&prefix[..]) {
        Some(rest) => OsString::from_vec(rest.to_vec()),
        None => file.as_os_str().to_os_string(),
    }
}

/// `_merge_hook_family_marker_name`: slashes become underscores
/// (basenames cannot contain `/`, so this is enough).
pub fn family_marker_name(relpath: &OsStr) -> OsString {
    let bytes: Vec<u8> = relpath
        .as_bytes()
        .iter()
        .map(|byte| if *byte == b'/' { b'_' } else { *byte })
        .collect();
    OsString::from_vec(bytes)
}

/// `_merge_hook_expand_home`: replace `${HOME}` then `$HOME`
/// (single pass, no rescan — like bash `//`), then a leading `~`
/// (`~` alone or `~/...`; `~otheruser/...` is untouched, never
/// resolved to another user's home).
pub fn expand_home(value: &str, home: &str) -> String {
    let replaced = value.replace("${HOME}", home).replace("$HOME", home);
    if replaced == "~" {
        return home.to_string();
    }
    if let Some(rest) = replaced.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    replaced
}

/// `_merge_hook_jq_available`: an executable `jq` on PATH.
pub fn jq_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| !dir.as_os_str().is_empty() && is_executable(&dir.join("jq")))
}

/// True for a regular file with any execute bit (POSIX `command -v`
/// only reports executables).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

/// Non-Unix fallback: executability has no bit to test.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `_merge_hook_tmp_for`: sibling temp beside the destination so the
/// publish rename stays on one filesystem.
pub fn tmp_for(dst: &Path) -> Result<PathBuf, Error> {
    temp::sibling_tmp_for(dst)
}

/// `_merge_hook_commit_tmp`: publish the staged temp over the
/// destination.
pub fn commit_tmp(tmp: &Path, dst: &Path, ctx: &mut Ctx<'_>) -> Result<(), Error> {
    temp::publish_prepared_regular(tmp, dst, ctx.cache)
}

/// Remove a temp file, ignoring the outcome like `rm -f`.
fn remove_tmp(tmp: &Path) {
    let _ = std::fs::remove_file(tmp);
}

/// `_merge_hook_write_text_if_changed`: write `text` plus a newline
/// unless the destination already holds it.
pub fn write_text_if_changed(dst: &Path, text: &str, ctx: &mut Ctx<'_>) -> Result<(), Error> {
    let rendered = format!("{text}\n");
    if temp::stdin_matches_file(ctx.source_root, rendered.as_bytes(), dst).unwrap_or(false) {
        return Ok(());
    }
    let tmp = tmp_for(dst)?;
    if let Err(source) = std::fs::write(&tmp, rendered.as_bytes()) {
        remove_tmp(&tmp);
        return Err(Error::Io {
            context: "write hook text temp",
            source,
        });
    }
    if let Err(error) = commit_tmp(&tmp, dst, ctx) {
        remove_tmp(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Run `jq` with `args`, writing stdout to `tmp`. Missing binary and
/// nonzero exit both count as failure (the shell `jq ... >tmp`
/// branches on `$?`). `jq` diagnostics flow to the `warn` sink line
/// by line, exactly where the shell leaves them on stderr.
fn run_jq(args: &[&OsStr], tmp: &Path, warn: &mut dyn FnMut(&str)) -> bool {
    if crate::cancellation::check().is_err() {
        return false;
    }
    let mut command = std::process::Command::new("jq");
    command.args(args).stdin(std::process::Stdio::null());
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Strict,
    );
    let Ok(output) = output else {
        return false;
    };
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        warn(line);
    }
    if !output.status.success() {
        return false;
    }
    crate::cancellation::check().is_ok() && std::fs::write(tmp, &output.stdout).is_ok()
}

/// The `jq empty` corruption probe. Ordinary nonzero exit means invalid JSON;
/// cancellation or an unverified teardown remains a typed error so callers
/// never delete a valid destination merely because validation was interrupted.
fn jq_valid(dst: &Path) -> Result<bool, Error> {
    crate::cancellation::check_mutation()?;
    let mut command = std::process::Command::new("jq");
    command
        .arg("empty")
        .arg(dst)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match crate::cleanup::run_session_output_typed(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Strict,
    ) {
        Ok(output) => Ok(output.status.success()),
        Err(crate::cleanup::SessionOutputError::Interrupted(_)) => Err(Error::Io {
            context: "jq validation interrupted",
            source: std::io::ErrorKind::Interrupted.into(),
        }),
        Err(crate::cleanup::SessionOutputError::CleanupIncomplete) => Err(Error::Io {
            context: "jq validation cleanup incomplete",
            source: std::io::Error::other("could not verify jq subprocess cleanup"),
        }),
        Err(crate::cleanup::SessionOutputError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            // Shell parity: a missing `jq` binary fails the `jq empty`
            // probe like corrupt JSON, so the caller takes the
            // warn-and-rebuild path (which then degrades to warn-and-skip
            // in run_jq) instead of failing the whole layer.
            Ok(false)
        }
        Err(crate::cleanup::SessionOutputError::Io(error)) => Err(Error::Io {
            context: "run jq validation",
            source: error,
        }),
        Err(
            crate::cleanup::SessionOutputError::TimedOut
            | crate::cleanup::SessionOutputError::CaptureLimit,
        ) => Err(Error::Io {
            context: "run jq validation",
            source: std::io::Error::other("jq validation did not complete"),
        }),
    }
}

/// `_merge_hook_jq_layer`: install (`! -f dst`) or merge JSON through
/// `jq`, rebuilding corrupt destinations. Every skip warns and still
/// succeeds; only temp creation and the final publish can fail.
pub fn jq_layer(
    label: &str,
    src: &Path,
    dst: &Path,
    filter: &str,
    ctx: &mut Ctx<'_>,
) -> Result<(), Error> {
    let tmp = tmp_for(dst)?;
    if !dst.is_file() {
        let src_str = src.as_os_str();
        let copied = {
            let warnings = &mut *ctx.warnings;
            run_jq(
                &[
                    OsStr::new("--sort-keys"),
                    OsStr::new("--indent"),
                    OsStr::new("2"),
                    OsStr::new("."),
                    src_str,
                ],
                &tmp,
                &mut |line: &str| warnings.push(line.to_string()),
            )
        };
        if !copied {
            ctx.warnings.push(format!(
                "    warning: {label} copy failed \u{2014} skipping"
            ));
            remove_tmp(&tmp);
            return Ok(());
        }
        if let Err(error) = commit_tmp(&tmp, dst, ctx) {
            remove_tmp(&tmp);
            return Err(error);
        }
        return Ok(());
    }
    let empty = std::fs::metadata(dst).is_ok_and(|meta| meta.len() == 0);
    let valid = if empty { false } else { jq_valid(dst)? };
    if !valid {
        ctx.warnings.push(format!(
            "    warning: corrupt {} \u{2014} rebuilding",
            dst.display()
        ));
        crate::cancellation::check_mutation()?;
        let _ = std::fs::remove_file(dst);
        remove_tmp(&tmp);
        return jq_layer(label, src, dst, filter, ctx);
    }
    let (src_str, dst_str, filter_str) = (src.as_os_str(), dst.as_os_str(), OsStr::new(filter));
    let merged = {
        let warnings = &mut *ctx.warnings;
        run_jq(
            &[
                OsStr::new("-n"),
                OsStr::new("--sort-keys"),
                OsStr::new("--indent"),
                OsStr::new("2"),
                OsStr::new("--slurpfile"),
                OsStr::new("s"),
                src_str,
                OsStr::new("--slurpfile"),
                OsStr::new("d"),
                dst_str,
                filter_str,
            ],
            &tmp,
            &mut |line: &str| warnings.push(line.to_string()),
        )
    };
    if !merged {
        ctx.warnings.push(format!(
            "    warning: {label} merge failed \u{2014} skipping"
        ));
        remove_tmp(&tmp);
        return Ok(());
    }
    if let Err(error) = commit_tmp(&tmp, dst, ctx) {
        remove_tmp(&tmp);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn interrupted_jq_validation_never_removes_the_existing_destination() {
        const HELPER: &str = "DOT_MERGE_JQ_CANCEL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "merge_hooks::cancellation_tests::interrupted_jq_validation_never_removes_the_existing_destination",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "interrupted jq helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let scratch = dot_test_support::TempDir::new("merge-jq-cancel").unwrap();
        let bin = scratch.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ready = scratch.path().join("jq.ready");
        let jq = bin.join("jq");
        std::fs::write(
            &jq,
            format!(
                "#!/bin/sh\n: >'{}'\ntrap '' TERM\nwhile :; do /bin/sleep 1; done\n",
                ready.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&jq, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: this recursive helper is the only test in its process.
        unsafe { std::env::set_var("PATH", &bin) };
        let source = scratch.path().join("source.json");
        let destination = scratch.path().join("destination.json");
        std::fs::write(&source, b"{\"new\":true}\n").unwrap();
        std::fs::write(&destination, b"{\"preserve\":true}\n").unwrap();
        let signals = crate::cleanup::Signals::install().unwrap();
        let ready_for_signal = ready.clone();
        let sender = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !ready_for_signal.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(ready_for_signal.exists());
            // SAFETY: the helper owns an installed SIGTERM handler.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
        });
        let mut cache = MoveCache::default();
        let mut warnings = Vec::new();
        let result = jq_layer(
            "fixture",
            &source,
            &destination,
            "$s[0] * $d[0]",
            &mut Ctx {
                source_root: scratch.path(),
                cache: &mut cache,
                warnings: &mut warnings,
            },
        );
        sender.join().unwrap();
        let status = signals.finish(if result.is_ok() { 0 } else { 1 });

        assert_eq!(status, 128 + libc::SIGTERM);
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"{\"preserve\":true}\n"
        );
    }
}
