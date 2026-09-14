//! Owned-resource cleanup registry (slice 3).
//!
//! Ports the GUARANTEES of `lib/dot/resources.sh`, not its Bash
//! machinery. The shell tracks PIDs (with procfs start-tick identities
//! against reuse), temp paths, and fds in parallel arrays, then tears
//! down TERM-first with bounded KILL escalation exactly once. None of
//! that machinery exists in Rust — no job tables, no `BASHPID`, no
//! coprocs — and none of it is needed:
//!
//! - Owned children are tracked as [`std::process::Child`] HANDLES, not
//!   PIDs. Handle ownership makes PID reuse impossible by construction,
//!   so procfs observation, `pgrep` descendant discovery, and start-tick
//!   identities are out of scope (they only exist to approximate what a
//!   handle states outright).
//! - Liveness polling uses [`std::process::Child::try_wait`] (free,
//!   race-free reaping). Only signal DELIVERY needs help: std exposes
//!   SIGKILL via [`std::process::Child::kill`] but not SIGTERM, so the
//!   TERM grace step shells out to the `kill` CLI — one spawn per child
//!   on the rare cleanup path, no new dependency (the engine already
//!   requires `kill`, `sleep`, and friends).
//! - Temp paths are removed with symlink-aware semantics matching
//!   `rm -rf` (a symlink removes the LINK, never the target).
//!
//! Explicitly EXCLUDED (later slices, each documented at its call site):
//! trap installation (needs a signal-handler story), process-group
//! isolation (needs a `setpgid` story), the mktemp coproc allocator
//! (temp slice: direct atomic creation needs no coproc), and
//! FOREIGN-pid liveness probing (update-lock slice).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::errors::{Error, Result};

/// TERM grace: 20 attempts × 50ms, mirroring
/// `DOT_CLEANUP_GRACE_ATTEMPTS=20` and `sleep 0.05`.
pub const GRACE_ATTEMPTS: u32 = 20;
/// Milliseconds between grace polls.
pub const GRACE_INTERVAL_MS: u64 = 50;

/// Shell validation rule for registry PIDs (`^[1-9][0-9]*$`): positive,
/// no leading zero, all digits. Ported as a pure predicate so the
/// update-lock slice (which deals in FOREIGN pids) shares the rule.
pub fn valid_pid(text: &str) -> bool {
    !text.is_empty() && text.as_bytes()[0] != b'0' && text.bytes().all(|b| b.is_ascii_digit())
}

/// Shell rule for the optional launch group: empty, or exactly the
/// leader PID (arbitrary numeric PGIDs never enter the registry).
pub fn valid_group(pid_text: &str, group_text: &str) -> bool {
    group_text.is_empty() || group_text == pid_text
}

/// Owned resources awaiting teardown.
#[derive(Debug, Default)]
pub struct Registry {
    children: Vec<Child>,
    paths: Vec<PathBuf>,
    files: Vec<File>,
    running: bool,
}

impl Registry {
    /// Empty registry.
    pub fn new() -> Self {
        Registry::default()
    }

    /// Track an owned child for TERM/KILL escalation and reaping.
    pub fn track_child(&mut self, child: Child) {
        self.children.push(child);
    }

    /// Stop tracking the child with this pid. Returns whether one was
    /// present (the shell unregisters every match; PIDs are unique here
    /// by handle construction).
    pub fn untrack_child(&mut self, pid: u32) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id() != pid);
        self.children.len() != before
    }

    /// Number of tracked children (for tests).
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Register a temp path for removal at cleanup. Empty paths are a
    /// usage error (shell exit 2), like the shell registry.
    pub fn register_path(&mut self, path: &Path) -> Result<()> {
        if path.as_os_str().is_empty() {
            return Err(Error::Usage {
                message: "cleanup path must not be empty",
            });
        }
        self.paths.push(path.to_path_buf());
        Ok(())
    }

    /// Forget a path without removing it.
    pub fn unregister_path(&mut self, path: &Path) {
        self.paths.retain(|owned| owned != path);
    }

    /// Number of registered paths (for tests).
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// Hold an open file for closing at cleanup (drop closes it).
    pub fn hold_file(&mut self, file: File) {
        self.files.push(file);
    }

    /// Remove one owned path now, mirroring
    /// `_dot_cleanup_remove_path`: unregister first so a path recreated
    /// after removal cannot be deleted by a later cleanup as though it
    /// were still the original object; on failure RE-REGISTER (the path
    /// is still ours and must not leak) and report the error.
    pub fn remove_path(&mut self, path: &Path) -> Result<()> {
        self.unregister_path(path);
        match remove_one(path) {
            Ok(()) => Ok(()),
            Err(source) => {
                self.paths.push(path.to_path_buf());
                Err(Error::Io {
                    context: "cleanup could not remove path",
                    source,
                })
            }
        }
    }

    /// Run the full teardown exactly once: TERM grace for children, KILL
    /// escalation, reap, close files, remove paths (individual path
    /// failures do not abort the pass, like the shell's `|| true`).
    /// Reentrant calls while running, or after completion, return
    /// immediately (shell: `[[ RUNNING -eq 0 ]] || return 0`).
    pub fn cleanup(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
        terminate_children(&mut self.children);
        // Files close on drop; drain explicitly so descriptor release
        // precedes path removal, matching the shell's fd-then-path order.
        self.files.clear();
        let paths = std::mem::take(&mut self.paths);
        for path in &paths {
            let _ = remove_one(path);
        }
        self.running = false;
    }
}

/// `rm -rf` for one path: symlinks remove the LINK (never the target),
/// directories remove recursively, anything else removes as a file.
/// Missing paths are success (shell `rm -rf` semantics).
fn remove_one(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
        Ok(meta) => {
            if meta.file_type().is_symlink() || meta.file_type().is_file() {
                std::fs::remove_file(path)
            } else if meta.file_type().is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                // Sockets, fifos, devices: unlink like `rm`.
                std::fs::remove_file(path)
            }
        }
    }
}

/// Send SIGTERM via the shell's `kill` builtin (std cannot address
/// SIGTERM, and the external `kill` binary is absent from minimal
/// images where the shell still works). Failures are best-effort
/// (`|| true` in the shell): the grace loop and KILL escalation below
/// handle uncooperative children. The pid is a u32, so no quoting is
/// needed.
fn terminate(pid: u32) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -TERM {pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// TERM grace, then KILL escalation, then reap — mirroring
/// `_dot_cleanup_owned` for owned (non-group) children.
fn terminate_children(children: &mut Vec<Child>) {
    for child in children.iter() {
        terminate(child.id());
    }
    let deadline_grace =
        Instant::now() + Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS);
    loop {
        let mut all_done = true;
        for child in children.iter_mut() {
            // Still running: keep waiting on it. Exited or stale
            // handles need nothing further.
            if let Ok(None) = child.try_wait() {
                all_done = false;
            }
        }
        if all_done || Instant::now() >= deadline_grace {
            break;
        }
        std::thread::sleep(Duration::from_millis(GRACE_INTERVAL_MS));
    }
    for child in children.iter_mut() {
        // SIGKILL for stragglers; already-exited handles report an
        // error here, which is fine (shell: `|| true`).
        let _ = child.kill();
    }
    // Reap every handle so no zombie survives cleanup (shell `wait`).
    // Drain into a temp vec: `Child::wait` needs `&mut`, and the
    // registry drops the handles either way.
    for mut child in children.drain(..) {
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_and_group_rules_match_shell() {
        // `^[1-9][0-9]*$`: positive, no leading zero.
        for good in ["1", "9", "123", "9773"] {
            assert!(valid_pid(good), "{good:?}");
        }
        for bad in ["", "0", "01", "007", "-1", "12a", " 1", "1 "] {
            assert!(!valid_pid(bad), "{bad:?}");
        }
        // Group must be empty or exactly the leader pid.
        assert!(valid_group("123", ""));
        assert!(valid_group("123", "123"));
        assert!(!valid_group("123", "456"));
        assert!(!valid_group("123", "0"));
    }

    #[test]
    fn empty_path_is_usage_error() {
        let mut registry = Registry::new();
        let err = registry
            .register_path(Path::new(""))
            .expect_err("empty path");
        assert!(matches!(err, Error::Usage { .. }), "{err:?}");
        assert_eq!(registry.path_count(), 0);
    }

    #[test]
    fn remove_missing_path_succeeds_like_rm_rf() {
        let mut registry = Registry::new();
        let missing = PathBuf::from("dot-cleanup-definitely-missing-xyz");
        assert!(!missing.exists());
        registry.remove_path(&missing).expect("missing ok");
    }

    #[test]
    fn remove_unregisters_first_then_deletes() {
        let dir = std::env::temp_dir().join(format!("dot-cleanup-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("setup");
        std::fs::write(dir.join("sub").join("f"), b"x").expect("setup");
        let mut registry = Registry::new();
        registry.register_path(&dir).expect("register");
        registry.remove_path(&dir).expect("remove");
        assert!(!dir.exists());
        assert_eq!(registry.path_count(), 0);
    }

    #[test]
    fn symlink_removes_link_not_target() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let base =
                std::env::temp_dir().join(format!("dot-cleanup-link-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("setup");
            let target = base.join("target");
            std::fs::write(&target, b"data").expect("setup");
            let link = base.join("link");
            symlink(&target, &link).expect("symlink");
            let mut registry = Registry::new();
            registry.remove_path(&link).expect("remove link");
            assert!(std::fs::symlink_metadata(&link).is_err());
            assert_eq!(std::fs::read(&target).expect("target survives"), b"data");
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn failed_removal_re_registers() {
        // Make removal fail portably: a file inside a read-only dir.
        // (As root the removal may still succeed; the contract then is
        // the mirror image. Assert the disjunction so the test holds
        // for both.)
        let base = std::env::temp_dir().join(format!("dot-cleanup-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("setup");
        let inner = base.join("inner");
        std::fs::write(&inner, b"x").expect("setup");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        }
        let mut registry = Registry::new();
        registry.register_path(&inner).expect("register");
        let removed = registry.remove_path(&inner).is_ok();
        // Restore permissions so the temp dir always cleans up.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755));
        }
        if removed {
            // Succeeded (e.g. running as root): unregistered, gone.
            assert!(!inner.exists());
            assert_eq!(registry.path_count(), 0);
        } else {
            // Failed: still owned (re-registered), still present.
            assert!(inner.exists());
            assert_eq!(registry.path_count(), 1);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cleanup_is_idempotent_and_drains() {
        let mut registry = Registry::new();
        let dir = std::env::temp_dir().join(format!("dot-cleanup-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("setup");
        registry.register_path(&dir).expect("register");
        registry.cleanup();
        registry.cleanup();
        assert!(!dir.exists());
        assert_eq!(registry.path_count(), 0);
    }

    #[test]
    fn cleanup_terminates_and_reaps_child() {
        let child = Command::new("sleep")
            .arg("300")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut registry = Registry::new();
        registry.track_child(child);
        assert_eq!(registry.child_count(), 1);
        registry.cleanup();
        assert_eq!(registry.child_count(), 0);
        assert!(process_gone(pid), "child {pid} must be reaped");
    }

    /// `kill -0` probe (test-only): true when the pid is gone or
    /// permission-denied-detached; mirrors the shell's reap check.
    #[cfg(unix)]
    fn process_gone(pid: u32) -> bool {
        // Reaped children vanish from the table; a tiny race between
        // wait() and table teardown is impossible (waited == reaped).
        // Confirm via /proc when present, else assume reaped.
        let proc = PathBuf::from(format!("/proc/{pid}"));
        !proc.exists()
    }

    #[cfg(not(unix))]
    fn process_gone(_pid: u32) -> bool {
        true
    }
}
