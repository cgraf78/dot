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

/// Error context when a signal interrupts the `jq empty` probe.
const JQ_VALIDATION_INTERRUPTED: &str = "jq validation interrupted";

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
            context: JQ_VALIDATION_INTERRUPTED,
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    // Waiting for the fake `jq` to start and for the helper as a whole are
    // liveness waits: their failure mode is "never", so the bounds only need to
    // clear process startup plus bounded teardown on a loaded host. The fake
    // outlives the helper bound, so a teardown that waited it out instead of
    // killing it still fails, and a helper killed at the bound cannot leave
    // the fake looping forever.
    const FIXTURE_READY_TIMEOUT: Duration = Duration::from_secs(30);
    const HELPER_TIMEOUT: Duration = Duration::from_secs(90);
    const FIXTURE_LIFETIME_SECS: u32 = 180;
    const _: () = assert!(
        FIXTURE_LIFETIME_SECS as u64 > HELPER_TIMEOUT.as_secs(),
        "the fake jq must outlive the helper bound"
    );
    /// Scratch directory the parent creates and the helper uses, so the
    /// parent can remove it even when it has to kill the helper.
    const SCRATCH_ENV: &str = "DOT_MERGE_JQ_CANCEL_SCRATCH";
    /// File the fake `jq` publishes its PID to once it is running.
    const READY_FILE: &str = "jq.ready";

    /// How the signal sender thread ended.
    #[derive(Debug, PartialEq)]
    enum Delivery {
        /// The fake `jq` started and SIGTERM was delivered.
        Delivered,
        /// `jq_layer` returned before the fake `jq` ever started.
        LayerFinishedFirst,
        /// The fake never started; SIGTERM was still delivered so an
        /// unexpected spawn cannot run forever.
        NeverReady,
    }

    /// Drains `pipe` on its own thread so a chatty helper cannot block on a
    /// full pipe while the parent polls for its exit.
    fn drain(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = pipe.read_to_end(&mut text);
            String::from_utf8_lossy(&text).into_owned()
        })
    }

    /// SIGKILLs the fake `jq` and its process group if it is still running.
    ///
    /// The helper normally has the supervisor kill it, but a helper that is
    /// killed at its bound, or fails, leaves the setsid'd fake behind with
    /// TERM ignored. Its PID comes from the ready file and is pinned (a
    /// pidfd on Linux) and confirmed to still be running this scratch's
    /// fake before any signal, so a recycled PID is never touched. While
    /// that leader is alive no other group can carry its PID as a group ID,
    /// so the group kill reaches only the fake and its `sleep` children.
    fn kill_leftover_fixture(scratch: &std::path::Path) {
        let jq = scratch.join("bin").join("jq");
        let Some(pid) = std::fs::read_to_string(scratch.join(READY_FILE))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            return;
        };
        let runs_fixture = || {
            let command = std::process::Command::new("ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output();
            command.is_ok_and(|output| {
                String::from_utf8_lossy(&output.stdout).contains(&*jq.to_string_lossy())
            })
        };
        #[cfg(target_os = "linux")]
        {
            // SAFETY: pidfd_open takes a positive PID and flags=0; the
            // returned descriptor is owned and closed below.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                return;
            }
            let fd = fd as libc::c_int;
            if runs_fixture() {
                // SAFETY: the pinned leader is alive, so this group ID is
                // the fake's own group (or absent, which is harmless).
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                // SAFETY: fd is a live pidfd owned by this function.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd,
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
            // SAFETY: closes the pidfd opened above exactly once.
            unsafe { libc::close(fd) };
        }
        #[cfg(not(target_os = "linux"))]
        if runs_fixture() {
            // SAFETY: signals to a PID just confirmed to run the fake and to
            // the group it leads.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    /// Parent-owned scratch: on every exit path, including an unwind, it
    /// kills a leftover fake `jq` before the directory itself is removed.
    struct Scratch(dot_test_support::TempDir);

    impl Drop for Scratch {
        fn drop(&mut self) {
            kill_leftover_fixture(self.0.path());
        }
    }

    /// Kills and reaps the helper if `run_helper` unwinds before reaping it.
    struct HelperGuard(Option<std::process::Child>);

    impl Drop for HelperGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Re-runs this test in a child process (it owns process signal state)
    /// and bounds the child, so a lost cancellation fails instead of hanging.
    fn run_helper(name: &str, helper: &str) {
        // Declared before the helper guard so it drops after it: the helper
        // is killed first, then the fake, then the directory.
        let scratch = Scratch(dot_test_support::TempDir::new("merge-jq-cancel").unwrap());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(helper, "1")
            .env(SCRATCH_ENV, scratch.0.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let mut helper = HelperGuard(Some(child));
        let deadline = Instant::now() + HELPER_TIMEOUT;
        let status = loop {
            let child = helper.0.as_mut().unwrap();
            if let Some(status) = child.try_wait().unwrap() {
                helper.0 = None;
                break Some(status);
            }
            if Instant::now() >= deadline {
                drop(helper);
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Clean up before the assertions so their messages are not delayed;
        // `Scratch` repeats the (idempotent) kill on unwind paths.
        kill_leftover_fixture(scratch.0.path());
        let stdout = stdout.join().unwrap();
        let stderr = stderr.join().unwrap();
        match status {
            Some(status) => assert!(
                status.success(),
                "interrupted jq helper failed with {status:?}:\n{stdout}\n{stderr}"
            ),
            None => panic!(
                "interrupted jq helper did not finish within {HELPER_TIMEOUT:?}; \
                 the cancellation was lost or teardown waited out the fake jq:\n{stdout}\n{stderr}"
            ),
        }
        // A filter typo or a renamed test would otherwise "pass" by running
        // nothing in the helper.
        assert!(
            stdout.contains("test result: ok. 1 passed;"),
            "interrupted jq helper did not run exactly one test:\n{stdout}\n{stderr}"
        );
    }

    #[test]
    fn interrupted_jq_validation_never_removes_the_existing_destination() {
        const HELPER: &str = "DOT_MERGE_JQ_CANCEL_HELPER";
        const NAME: &str = "merge_hooks::cancellation_tests::interrupted_jq_validation_never_removes_the_existing_destination";
        if std::env::var_os(HELPER).is_none() {
            run_helper(NAME, HELPER);
            return;
        }

        let scratch = std::path::PathBuf::from(std::env::var_os(SCRATCH_ENV).unwrap());
        let scratch = scratch.as_path();
        let bin = scratch.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ready = scratch.join(READY_FILE);
        let jq = bin.join("jq");
        // Publish the PID atomically (write then rename) so the parent can
        // always parse it once the ready file exists. PATH holds only `bin`,
        // so the rename uses an absolute `/bin/mv`.
        std::fs::write(
            &jq,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" >'{ready}.tmp'\n/bin/mv '{ready}.tmp' '{ready}'\ntrap '' TERM\ni=0\nwhile [ \"$i\" -lt {FIXTURE_LIFETIME_SECS} ]; do /bin/sleep 1; i=$((i + 1)); done\n",
                ready = ready.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&jq, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: this recursive helper is the only test in its process.
        unsafe { std::env::set_var("PATH", &bin) };
        let source = scratch.join("source.json");
        let destination = scratch.join("destination.json");
        std::fs::write(&source, b"{\"new\":true}\n").unwrap();
        std::fs::write(&destination, b"{\"preserve\":true}\n").unwrap();
        let signals = crate::cleanup::Signals::install().unwrap();
        let ready_for_signal = ready.clone();
        let layer_done = Arc::new(AtomicBool::new(false));
        let layer_done_for_signal = Arc::clone(&layer_done);
        // The sender must never return without signalling while `jq_layer`
        // may still be waiting on the fake: that fake ignores TERM and only a
        // delivered cancellation makes the supervisor kill it.
        let sender = std::thread::spawn(move || {
            let deadline = Instant::now() + FIXTURE_READY_TIMEOUT;
            loop {
                if ready_for_signal.exists() {
                    break;
                }
                if layer_done_for_signal.load(Ordering::SeqCst) {
                    return Delivery::LayerFinishedFirst;
                }
                if Instant::now() >= deadline {
                    // SAFETY: the helper owns an installed SIGTERM handler.
                    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
                    return Delivery::NeverReady;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: the helper owns an installed SIGTERM handler.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
            Delivery::Delivered
        });
        let mut cache = MoveCache::default();
        let mut warnings = Vec::new();
        let result = jq_layer(
            "fixture",
            &source,
            &destination,
            "$s[0] * $d[0]",
            &mut Ctx {
                source_root: scratch,
                cache: &mut cache,
                warnings: &mut warnings,
            },
        );
        layer_done.store(true, Ordering::SeqCst);
        let delivery = sender.join().unwrap();
        let status = signals.finish(if result.is_ok() { 0 } else { 1 });

        assert_eq!(
            delivery,
            Delivery::Delivered,
            "the fake jq must start before cancellation; result={result:?} warnings={warnings:?}"
        );
        let context = match &result {
            Err(Error::Io { context, .. }) => Some(*context),
            _ => None,
        };
        assert_eq!(
            (status, context),
            (128 + libc::SIGTERM, Some(JQ_VALIDATION_INTERRUPTED)),
            "interrupted jq validation must report the signal and its typed error: \
             result={result:?} warnings={warnings:?}"
        );
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"{\"preserve\":true}\n"
        );
    }
}
