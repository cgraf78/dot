//! Process-wide `dot update` serialization (slice 3).
//!
//! Ports `lib/dot/update-lock.sh`. `mkdir` atomicity, the owner record
//! (`pid\t…/start\t…/token\t…`), stale-claim-by-rename, the empty-guard
//! protocol, and the 3-attempt acquire loop are transliterated. Two
//! deliberate replacements (same rationale as `cleanup`):
//!
//! - Process identity uses the same two backends (procfs field 22,
//!   `ps -o lstart=`), read through std/`Command` — no change needed.
//! - Liveness probing (`kill -0`) goes through the `kill` CLI (std has
//!   no `kill(pid, 0)`); rare path only, no new dependency.
//! - The lock TOKEN format differs (`pid.nanos.counter` instead of
//!   `$$.${SECONDS}.${RANDOM}`): tokens are opaque, and wall-clock
//!   seconds plus `$RANDOM` are weaker uniqueness than a monotonic
//!   counter plus nanoseconds.
//! - Signal-trap installation (`_dot_update_lock_install_traps`) is
//!   EXCLUDED: [`LockGuard`] releases on drop (RAII), which is strictly
//!   stronger than EXIT-trap release (it also covers early returns and
//!   panics). A signal-handler story is a later slice.
//! - `DOT_UPDATE_LOCK_TOKEN`/`DOT_UPDATE_LOCK_CRON_MODE` globals become
//!   explicit parameters and return values.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::errors::{Error, Result};
use crate::log::Log;

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Dot state directory name under the XDG state home: the shell
/// resolves the lock via `dot_xdg_path state "dot/update.lock.d"`,
/// so the `dot/` segment is part of the lock path contract, not a
/// caller choice.
pub const DOT_DIR_NAME: &str = "dot";
/// Lock directory name under the dot state dir.
pub const LOCK_DIR_NAME: &str = "update.lock.d";
/// Owner record file name inside the lock dir.
pub const OWNER_FILE_NAME: &str = "owner";
/// Reclaim-claim filename prefix (`owner.reclaim.<pid>.<nanos>.<n>`).
pub const CLAIM_PREFIX: &str = "owner.reclaim.";
/// Empty-lock guard directory name.
pub const GUARD_DIR_NAME: &str = "reclaim.d";
/// Seconds a lock may look half-initialized before reclaim.
pub const INITIALIZING_WINDOW_SECS: i64 = 5;
/// Acquire attempts before reporting busy (shell `while attempt < 3`).
pub const ACQUIRE_ATTEMPTS: u32 = 3;
/// Shell exit code when another valid owner holds the lock.
pub const EXIT_LOCK_BUSY: i32 = 75;

/// Parsed owner record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// Owner process id.
    pub pid: u32,
    /// Process-start identity (`proc:<tick>` or `ps lstart` text).
    pub start: String,
    /// Opaque claim token minted at acquisition.
    pub token: String,
}

/// Read and validate an owner file. `None` means missing, unreadable,
/// or malformed — the caller treats all three as "no valid owner" and
/// proceeds to reclaim, exactly like the shell's `|| return 1` flow
/// (a corrupt owner file must never wedge updates forever).
pub fn read_owner(lock_dir: &Path) -> Option<Owner> {
    let text = std::fs::read_to_string(lock_dir.join(OWNER_FILE_NAME)).ok()?;
    parse_owner(&text)
}

/// Parse owner text: tab-separated `pid`/`start`/`token` lines.
/// Unknown keys are ignored; pid must be a positive integer without a
/// leading zero (shell `^[0-9]+$` — note: the lock path is stricter
/// than needed, pid 0 can never own a lock); start and token must be
/// non-empty.
pub fn parse_owner(text: &str) -> Option<Owner> {
    let mut pid: Option<u32> = None;
    let mut start: Option<&str> = None;
    let mut token: Option<&str> = None;
    for line in text.lines() {
        let (key, value) = line.split_once('\t')?;
        match key {
            "pid" => {
                if !crate::cleanup::valid_pid(value) {
                    return None;
                }
                // valid_pid excludes 0 and leading zeros; parse cannot
                // fail on what it accepted, but stay total anyway.
                pid = Some(value.parse().ok()?);
            }
            "start" if !value.is_empty() => start = Some(value),
            "token" if !value.is_empty() => token = Some(value),
            _ => {}
        }
    }
    Some(Owner {
        pid: pid?,
        start: start?.to_string(),
        token: token?.to_string(),
    })
}

/// Render an owner record byte-identical to the shell's `printf`s.
pub fn format_owner(owner: &Owner) -> String {
    format!(
        "pid\t{}\nstart\t{}\ntoken\t{}\n",
        owner.pid, owner.start, owner.token
    )
}

/// Mint a unique claim token. Opaque to readers; uniqueness comes from
/// pid plus nanosecond time plus a process-wide monotonic counter.
pub fn mint_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let n = TOKEN_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}.{nanos}.{n}", std::process::id())
}

/// Process-start identity for liveness comparison.
///
/// Selection mirrors `_dot_update_lock_process_start` exactly: a
/// `proc:`-prefixed expectation pins the procfs backend (a later PATH
/// change must not make a live process look stale); otherwise a full
/// `ps` answer wins (preserving locks from older generations); procfs
/// is the fallback only when no expectation constrains the backend.
pub fn process_start(pid: u32, expected: Option<&str>) -> Option<String> {
    if expected.is_some_and(|text| text.starts_with("proc:")) {
        return process_start_proc(pid);
    }
    if let Some(start) = process_start_ps(pid) {
        return Some(start);
    }
    if expected.is_none() {
        return process_start_proc(pid);
    }
    None
}

/// Procfs backend: kernel start tick (field 22) of `/proc/<pid>/stat`.
/// Parsing mirrors the shell: split after the FINAL `)` (comm may
/// contain spaces or parens), then the 20th token of the remainder.
pub fn process_start_proc(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(") ")?.1;
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    if fields.len() < 20 || !fields[19].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("proc:{}", fields[19]))
}

/// `ps` backend: `ps -o lstart=` under C locale and UTC, trimmed of the
/// trailing newline exactly like command substitution (leading spaces,
/// if any, are preserved — both sides compare opaque strings).
/// Empty output means no such process (or a broken `ps`).
pub fn process_start_ps(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .arg("-o")
        .arg("lstart=")
        .arg("-p")
        .arg(pid.to_string())
        .env("LC_ALL", "C")
        .env("TZ", "UTC0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Foreign-pid liveness via the shell's `kill -0` builtin (std cannot
/// send signal 0, and the external `kill` binary is absent from
/// minimal images that the shell still supports: there `kill` is a
/// builtin, so invoke it through `sh`, exactly like the shell's own
/// `kill -0 "$pid" 2>/dev/null`. The pid is a u32, so no quoting is
/// needed. A missing `sh` degrades to inactive, like every other
/// unreadable-identity path.
fn pid_alive(pid: u32) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether a recorded owner still holds the lock: process alive AND
/// start identity unchanged (PID-reuse protection).
pub fn owner_is_active(owner: &Owner) -> bool {
    if !pid_alive(owner.pid) {
        return false;
    }
    process_start(owner.pid, Some(&owner.start)).as_deref() == Some(owner.start.as_str())
}

/// File mtime as whole seconds since the epoch.
fn mtime_secs(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

/// Now as whole seconds since the epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a lock dir looks half-initialized (created within the last
/// 5 seconds): a crash between `mkdir` and the owner write. Signed
/// arithmetic mirrors `$((now - mtime < 5))` (future mtimes count).
pub fn is_initializing(lock_dir: &Path) -> bool {
    match mtime_secs(lock_dir) {
        Some(mtime) => now_secs() - mtime < INITIALIZING_WINDOW_SECS,
        None => false,
    }
}

/// Remove the owner file then the lock dir (best-effort rmdir, like the
/// shell: a recreated dir must not fail the releaser).
fn remove_dir(lock_dir: &Path) {
    let _ = std::fs::remove_file(lock_dir.join(OWNER_FILE_NAME));
    let _ = std::fs::remove_dir(lock_dir);
}

/// Atomic stale claim: rename the owner file aside under a unique name.
/// Only this claimant owns the moved file, so a second contender cannot
/// remove the directory out from under the first.
fn claim_file(source: &Path) -> Option<PathBuf> {
    if !is_plain_file(source) {
        return None;
    }
    // Build from the file name explicitly: claim names are
    // `owner.reclaim.<pid>.<nanos>.<n>` like the shell's.
    let mut name = source.file_name()?.to_os_string();
    name.push(format!(
        ".reclaim.{}.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0),
        TOKEN_COUNTER.fetch_add(1, Ordering::SeqCst),
    ));
    let claim = source.with_file_name(name);
    std::fs::rename(source, &claim).ok()?;
    Some(claim)
}

/// Regular file, not a symlink (shell `[[ -f … && ! -L … ]]`).
fn is_plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// First reclaim-candidate claim file in a lock dir, if any.
fn find_claim(lock_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(lock_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(CLAIM_PREFIX))
            && is_plain_file(&path)
        {
            return Some(path);
        }
    }
    None
}

/// Reclaim an empty (ownerless) lock: exactly one cleaner wins via the
/// inner guard dir; a dead cleaner's guard is itself reclaimable.
/// Mirrors `_dot_update_lock_reclaim_empty` including its return
/// contract (true = reclaimed).
fn reclaim_empty(lock_dir: &Path) -> bool {
    let guard = lock_dir.join(GUARD_DIR_NAME);
    match std::fs::symlink_metadata(&guard) {
        Ok(meta) if meta.is_dir() => {
            if is_initializing(&guard) {
                return false;
            }
            if std::fs::remove_dir(&guard).is_err() {
                return false;
            }
        }
        Ok(_) | Err(_) if guard.exists() || guard.is_symlink() => {
            // Non-directory occupant (or inaccessible): not ours.
            return false;
        }
        _ => {
            if std::fs::create_dir(&guard).is_err() {
                return false;
            }
            let _ = std::fs::remove_dir(&guard);
        }
    }
    std::fs::remove_dir(lock_dir).is_ok()
}

/// Reclaim a stale lock (dead owner, or none). Mirrors
/// `_dot_update_lock_reclaim_stale`.
fn reclaim_stale(lock_dir: &Path) -> bool {
    let owner_file = lock_dir.join(OWNER_FILE_NAME);
    if is_plain_file(&owner_file) {
        let Some(claim) = claim_file(&owner_file) else {
            return false;
        };
        if !is_plain_file(&claim) || std::fs::remove_file(&claim).is_err() {
            return false;
        }
        let _ = std::fs::remove_dir(lock_dir);
        return true;
    }
    if let Some(stale) = find_claim(lock_dir) {
        let Some(claim) = claim_file(&stale) else {
            return false;
        };
        if !is_plain_file(&claim) || std::fs::remove_file(&claim).is_err() {
            return false;
        }
        let _ = std::fs::remove_dir(lock_dir);
        return true;
    }
    reclaim_empty(lock_dir)
}

/// Write the owner record with `0600` permissions (shell `umask 077` +
/// `chmod 600`). Returns the minted token.
fn write_owner(lock_dir: &Path) -> Result<String> {
    let Some(start) = process_start(std::process::id(), None) else {
        return Err(Error::Io {
            context: "lock could not identify its own process",
            source: std::io::Error::other("no process start identity"),
        });
    };
    let token = mint_token();
    let owner = Owner {
        pid: std::process::id(),
        start,
        token: token.clone(),
    };
    std::fs::write(lock_dir.join(OWNER_FILE_NAME), format_owner(&owner)).map_err(|source| {
        Error::Io {
            context: "lock could not write its owner record",
            source,
        }
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            lock_dir.join(OWNER_FILE_NAME),
            std::fs::Permissions::from_mode(0o600),
        );
    }
    Ok(token)
}

/// An acquired update lock. Releases (verified: pid + token must still
/// match, so a stolen lock is never removed) on [`LockGuard::release`]
/// or best-effort on drop.
#[derive(Debug)]
pub struct LockGuard {
    lock_dir: PathBuf,
    token: String,
    pid: u32,
    released: bool,
}

impl LockGuard {
    /// Lock directory this guard owns.
    pub fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }

    /// Claim token minted at acquisition.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Verified release: re-read the owner and only remove when pid and
    /// token still match this guard (shell `_dot_update_lock_release`).
    /// Removal failures warn through `log` into `warn_sink`.
    /// Returns true when the lock was removed.
    pub fn release(mut self, log: &Log, warn_sink: &mut dyn std::io::Write) -> bool {
        let removed = match read_owner(&self.lock_dir) {
            Some(owner)
                if owner.pid == self.pid
                    && owner.token == self.token
                    && std::fs::remove_file(self.lock_dir.join(OWNER_FILE_NAME)).is_ok() =>
            {
                std::fs::remove_dir(&self.lock_dir).is_ok()
            }
            _ => false,
        };
        if !removed {
            log.warn(
                warn_sink,
                &format!(
                    "warning: unable to remove dot update lock state: {}",
                    self.lock_dir.display()
                ),
            );
        }
        self.released = true;
        removed
    }

    /// Whether the lock file still names this guard's token.
    pub fn is_current(&self) -> bool {
        read_owner(&self.lock_dir)
            .is_some_and(|owner| owner.pid == self.pid && owner.token == self.token)
    }
}

impl Drop for LockGuard {
    /// Best-effort silent release for early returns and panics (the
    /// RAII replacement for EXIT-trap release). Verification still
    /// applies: never remove a lock that no longer names us.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Some(owner) = read_owner(&self.lock_dir) {
            if owner.pid == self.pid && owner.token == self.token {
                remove_dir(&self.lock_dir);
            }
        }
    }
}

/// Re-enter a lock already owned by this process under `token`
/// (shell `_dot_update_lock_reenter`, minus trap installation which
/// [`LockGuard`] drop makes redundant). Returns a guard on success.
pub fn try_reenter(lock_dir: &Path, token: &str) -> Option<LockGuard> {
    if token.is_empty() {
        return None;
    }
    let owner = read_owner(lock_dir)?;
    if owner.pid != std::process::id() || owner.token != token {
        return None;
    }
    let start = process_start(std::process::id(), Some(&owner.start))?;
    if start != owner.start {
        return None;
    }
    Some(LockGuard {
        lock_dir: lock_dir.to_path_buf(),
        token: token.to_string(),
        pid: std::process::id(),
        released: false,
    })
}

/// Acquire the process-wide update lock under `state_dir`
/// (`<state_dir>/dot/update.lock.d`), creating parents as needed.
///
/// `state_dir` is the already-resolved XDG state home (the `$XDG_STATE_HOME`
/// the shell's `_dot_update_lock_path` resolves via
/// `dot_xdg_path state "dot/update.lock.d"`): env lookup is a caller
/// concern, path SUFFIX is this module's contract. Full
/// fallback resolution (`xdg::path`) belongs to the CLI slice that
/// wires real argv/env; tests pass an isolated XDG root directly.
///
/// - `cron` selects cron mode: busy warnings are suppressed (stderr
///   stays silent) exactly like `--cron`.
/// - `prior_token` re-enters a lock this process already holds.
/// - `log` renders the busy/initializing warnings (never quiet-gated —
///   `_warn` semantics) into `warn`, which callers bind to real stderr.
///
/// Returns [`LockGuard`] on success, [`Error::LockBusy`] (already
/// warned, unless cron) when another valid owner is active, or an
/// [`Error::Io`] when the lock path itself is unusable.
pub fn acquire(
    state_dir: &Path,
    cron: bool,
    log: &Log,
    prior_token: Option<&str>,
    warn_sink: &mut dyn std::io::Write,
) -> Result<LockGuard> {
    let lock_dir = state_dir.join(DOT_DIR_NAME).join(LOCK_DIR_NAME);
    std::fs::create_dir_all(lock_dir.parent().unwrap_or(state_dir)).map_err(|source| {
        Error::Io {
            context: "lock could not create its state directory",
            source,
        }
    })?;
    if let Some(token) = prior_token {
        if let Some(guard) = try_reenter(&lock_dir, token) {
            return Ok(guard);
        }
    }

    for _ in 0..ACQUIRE_ATTEMPTS {
        match std::fs::create_dir(&lock_dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o700));
                }
                return match write_owner(&lock_dir) {
                    Ok(token) => Ok(LockGuard {
                        lock_dir,
                        token,
                        pid: std::process::id(),
                        released: false,
                    }),
                    Err(err) => {
                        remove_dir(&lock_dir);
                        Err(err)
                    }
                };
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(Error::Io {
                    context: "lock could not create its directory",
                    source,
                });
            }
        }

        if !lock_dir.is_dir() || lock_dir.is_symlink() {
            log.warn(
                warn_sink,
                &format!(
                    "warning: dot update lock path is not a directory: {}",
                    lock_dir.display()
                ),
            );
            return Err(Error::Io {
                context: "dot update lock path is not a directory",
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    lock_dir.display().to_string(),
                ),
            });
        }

        if let Some(owner) = read_owner(&lock_dir) {
            if owner_is_active(&owner) {
                let message = format!("warning: dot update already running (pid {})", owner.pid);
                if !cron {
                    log.warn(warn_sink, &message);
                }
                return Err(Error::LockBusy { message });
            }
        } else if is_initializing(&lock_dir) {
            let message = "warning: dot update lock is initializing".to_string();
            if !cron {
                log.warn(warn_sink, &message);
            }
            return Err(Error::LockBusy { message });
        }

        if !reclaim_stale(&lock_dir) {
            return Err(Error::LockBusy {
                message: "warning: dot update already running".to_string(),
            });
        }
    }
    Err(Error::LockBusy {
        message: "warning: dot update already running".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_log() -> Log {
        Log::new(false, false)
    }

    #[test]
    fn owner_round_trip() {
        let owner = Owner {
            pid: 123,
            start: "proc:456".to_string(),
            token: "123.456.0".to_string(),
        };
        assert_eq!(
            format_owner(&owner),
            "pid\t123\nstart\tproc:456\ntoken\t123.456.0\n"
        );
        assert_eq!(parse_owner(&format_owner(&owner)), Some(owner));
    }

    #[test]
    fn owner_rejects_malformed() {
        for bad in [
            "",
            "pid\t0\nstart\tx\ntoken\ty\n",
            "pid\t01\nstart\tx\ntoken\ty\n",
            "pid\tabc\nstart\tx\ntoken\ty\n",
            "pid\t1\nstart\t\ntoken\ty\n",
            "pid\t1\nstart\tx\n",
            "start\tx\ntoken\ty\n",
            "pid\t1\nstart\tx\ntoken\ty\nextra",
        ] {
            assert_eq!(parse_owner(bad), None, "input: {bad:?}");
        }
        // Unknown keys are ignored (forward compatibility).
        assert_eq!(
            parse_owner("pid\t1\nstart\tx\ntoken\ty\nfuture\tz\n"),
            Some(Owner {
                pid: 1,
                start: "x".to_string(),
                token: "y".to_string(),
            })
        );
    }

    #[test]
    fn self_start_identity_is_stable() {
        let pid = std::process::id();
        let start = process_start(pid, None).expect("self identity");
        assert!(!start.is_empty());
        // Pinned re-probe agrees (PID-reuse protection round trip).
        assert_eq!(process_start(pid, Some(&start)), Some(start.clone()));
        // No such process: both backends agree on absence.
        assert_eq!(process_start(1 << 30, None), None);
        assert_eq!(process_start_proc(1 << 30), None);
        assert_eq!(process_start_ps(1 << 30), None);
    }

    #[test]
    fn tokens_are_unique() {
        let first = mint_token();
        let second = mint_token();
        assert_ne!(first, second);
        assert!(first.starts_with(&std::process::id().to_string()));
    }

    #[test]
    fn acquire_release_cycle() {
        let scratch = crate::test_support::TempDir::new("lock-cycle").expect("scratch");
        let log = test_log();
        let mut warnings = Vec::new();
        let guard = acquire(scratch.path(), false, &log, None, &mut warnings).expect("acquire");
        assert!(guard.is_current());
        assert!(warnings.is_empty(), "fresh acquire warned");
        // Re-entry under the same token succeeds without blocking.
        let token = guard.token().to_string();
        let dir = guard.lock_dir().to_path_buf();
        let again =
            acquire(scratch.path(), false, &log, Some(&token), &mut warnings).expect("reenter");
        assert_eq!(again.token(), token);
        // Explicit release removes the lock; dropping the other guard
        // for the same (now ownerless) lock must remove nothing and
        // must not fail.
        assert!(again.release(&log, &mut Vec::new()));
        assert!(!dir.exists());
        drop(guard);
        assert!(!dir.exists());
    }

    #[test]
    fn drop_releases_unreleased_guard() {
        let scratch = crate::test_support::TempDir::new("lock-drop").expect("scratch");
        let log = test_log();
        let dir = scratch.path().join(DOT_DIR_NAME).join(LOCK_DIR_NAME);
        {
            let _guard =
                acquire(scratch.path(), false, &log, None, &mut Vec::new()).expect("acquire");
        }
        assert!(!dir.exists());
    }

    #[test]
    fn initializing_empty_lock_reports_busy() {
        let scratch = crate::test_support::TempDir::new("lock-init").expect("scratch");
        let dir = scratch.path().join(DOT_DIR_NAME).join(LOCK_DIR_NAME);
        std::fs::create_dir_all(&dir).expect("empty lock");
        assert!(is_initializing(&dir));
        let log = test_log();
        let mut warnings = Vec::new();
        match acquire(scratch.path(), false, &log, None, &mut warnings) {
            Err(Error::LockBusy { message }) => {
                assert!(message.contains("initializing"), "{message:?}");
            }
            other => panic!("expected busy, got {other:?}"),
        }
    }
}
