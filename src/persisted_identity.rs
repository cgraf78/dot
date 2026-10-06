//! `dev:ino` identities read back by a later process, possibly after a
//! reboot.
//!
//! Dot journals a filesystem object's `st_dev:st_ino` (the `stat -c '%d:%i'`
//! text [`crate::temp::identity_string`] renders) and later proves that the
//! object at a path is still the one it recorded, not one recreated, copied,
//! or restored since, before trusting or deleting it. The journals are the
//! init record and its `completed` copy, the init transaction's snapshots and
//! intents, and overlay replacement records.
//!
//! The inode number belongs to the object, but the device number belongs to
//! the mount: macOS assigns APFS volumes their `st_dev` at mount time, Linux
//! gives btrfs (and every other anonymous-device filesystem) one at mount,
//! and device-mapper minors follow activation order. A reboot can renumber
//! the device under an unchanged object, so comparing the recorded text
//! exactly refuses a healthy client after a plain reboot.
//!
//! A recorded identity still names a live object when the inode matches and
//! either the device matches too (the exact rule, unchanged) or the object's
//! birth time is known and no later than the time its journal was written.
//! The identity was read from the object before the journal holding it was
//! written, so the recorded object was born before the journal, while a
//! directory recreated after that was born later. A file-level copy or
//! restore gets a new inode, which is what refuses it: on macOS a
//! time-preserving copy (`cp -p`, `ditto`, Time Machine) can carry the birth
//! time back with it, but APFS never reuses an inode number on a volume.
//! Birth time survives remounts because filesystems store it with the inode
//! (APFS, btrfs `otime`, ext4 `crtime`, XFS v5).
//!
//! Where no birth time is available, the device must still match exactly, so
//! the check is never weaker than the exact rule. That covers Android (std
//! reports no birth time there, and `statx` is not tried because older app
//! seccomp filters kill the process on it), filesystems that store none, and
//! kernels older than `statx`.
//!
//! Trade-offs. An object that reuses the recorded inode number on the same
//! device passes, exactly as before. A changed device widens that to an
//! object that also has the recorded inode number and already existed when
//! the journal was written:
//! - a block-level restore or snapshot rollback that keeps inode numbers and
//!   birth times (a btrfs subvolume swap, an APFS snapshot revert) now reads
//!   as the same object; the content checks behind each identity (Git
//!   origin, branch, and generation marker; blob digests; claim markers)
//!   still apply;
//! - filesystem roots have fixed inode numbers (2 on ext4 and APFS, 256 for
//!   a btrfs subvolume), so an older filesystem mounted where a recorded
//!   root was also matches; excluding mount roots would refuse the common
//!   btrfs home that is a subvolume root after every reboot;
//! - callers capture the bound before this process rewrites a journal, but
//!   a resume or rollback that failed after rewriting one leaves a later
//!   bound for the next attempt.
//!
//! None of these is weaker than the exact rule on a device that was never
//! renumbered, where inode reuse alone already passed.
//!
//! Recording the birth time instead would have changed the journal formats,
//! and older dot releases parse them strictly (the init record refuses
//! unknown keys), so a downgrade would refuse every record a newer dot
//! wrote. Bounding by the journal's own modification time needs no format
//! change and also covers records written before this rule existed.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::SystemTime;

use crate::errors::{Error, Result};

/// The facts about one live object that decide whether a recorded
/// `dev:ino` still names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveIdentity {
    /// `st_dev`, which a remount may renumber.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// Birth time, when the platform and filesystem report one. Never the
    /// epoch itself: some filesystems report zero for "unknown".
    pub birth: Option<SystemTime>,
}

impl LiveIdentity {
    /// Stat `path` following symlinks, like `stat -c '%d:%i'` and
    /// [`crate::temp::path_identity`].
    pub fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).map_err(|source| Error::Io {
            context: "stat path identity",
            source,
        })?;
        Ok(Self::from_metadata(path, &meta, true))
    }

    /// Stat `path` itself, never a symlink's target (`lstat`).
    pub fn of_leaf(path: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
            context: "stat path identity",
            source,
        })?;
        Ok(Self::from_metadata(path, &meta, false))
    }

    /// The identity of an object already stat'ed into `meta` (`follow` says
    /// whether `meta` came from `stat` or `lstat`, so the birth time is read
    /// from the same object).
    pub fn from_metadata(path: &Path, meta: &std::fs::Metadata, follow: bool) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            birth: known(birth_time(path, meta, follow)),
        }
    }
}

/// A reported birth time, or `None` for the epoch itself: some filesystems
/// report zero for "unknown", which would predate every journal.
fn known(birth: Option<SystemTime>) -> Option<SystemTime> {
    birth.filter(|birth| *birth > SystemTime::UNIX_EPOCH)
}

/// When `journal` was last written: the bound a renumbered identity read from
/// it is held to. `None` (the exact rule) when it cannot be read.
pub fn journal_time(journal: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(journal)
        .and_then(|meta| meta.modified())
        .ok()
}

/// When anything in the journal directory `dir` was last written: the newest
/// modification time of `dir` itself (moved when a journal is created,
/// renamed into place, or removed) and of its direct entries (moved when one
/// is rewritten in place). An init transaction writes every identity it
/// records into a journal in its directory right after reading it, so this
/// bounds all of them at once. `None` when `dir` cannot be read.
pub fn newest_journal_time(dir: &Path) -> Option<SystemTime> {
    let mut newest = journal_time(dir)?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if let Some(written) = journal_time(&entry.path()) {
            newest = newest.max(written);
        }
    }
    Some(newest)
}

/// Whether `recorded` (`dev:ino`, as journaled) still names `live`.
/// `journaled` is when the journal holding `recorded` was written (see the
/// module docs); `None` keeps the exact rule.
pub fn matches(recorded: &str, live: &LiveIdentity, journaled: Option<SystemTime>) -> bool {
    let Some((dev, ino)) = recorded.split_once(':') else {
        return false;
    };
    // Text comparison, like the exact rule always was: a spelling such as
    // `0377` is not the `377` a stat renders.
    if ino != live.ino.to_string() {
        return false;
    }
    if dev == live.dev.to_string() {
        return true;
    }
    // Only a device the exact rule could have matched may be renumbered: a
    // malformed or respelled one (`-`, `01`) stays a mismatch.
    if dev
        .parse::<u64>()
        .map(|parsed| parsed.to_string())
        .as_deref()
        != Ok(dev)
    {
        return false;
    }
    match (live.birth, journaled) {
        (Some(birth), Some(journaled)) => birth <= journaled,
        _ => false,
    }
}

/// [`matches()`] for the object at `path`, following symlinks. A failed stat
/// never matches.
pub fn path_matches(path: &Path, recorded: &str, journaled: Option<SystemTime>) -> bool {
    LiveIdentity::of(path).is_ok_and(|live| matches(recorded, &live, journaled))
}

/// Birth time of the object `meta` describes. Linux reads it through `statx`
/// directly: std does that only on glibc, and release binaries are static
/// musl builds.
#[cfg(target_os = "linux")]
fn birth_time(path: &Path, meta: &std::fs::Metadata, follow: bool) -> Option<SystemTime> {
    use std::os::unix::ffi::OsStrExt as _;

    // The kernel's `struct statx` (include/uapi/linux/stat.h), the same on
    // every architecture. libc declares it only for glibc and Android (musl
    // needs an opt-in cfg), so the two fields read here are laid out
    // locally; the assertions below pin their offsets to the UAPI layout.
    #[repr(C)]
    struct Statx {
        mask: u32,
        _head: [u8; 28],
        ino: u64,
        _middle: [u8; 40],
        btime_sec: i64,
        btime_nsec: u32,
        _tail: [u8; 164],
    }
    const _: () = assert!(std::mem::size_of::<Statx>() == 256);
    const _: () = assert!(std::mem::offset_of!(Statx, ino) == 32);
    const _: () = assert!(std::mem::offset_of!(Statx, btime_sec) == 80);
    const STATX_INO: libc::c_uint = 0x100;
    const STATX_BTIME: libc::c_uint = 0x800;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // `stat` and `lstat` never trigger an automount; `statx` does unless
    // told not to.
    let nofollow = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    let flags = libc::AT_NO_AUTOMOUNT | nofollow;
    let mut buffer = std::mem::MaybeUninit::<Statx>::zeroed();
    // SAFETY: `path` is NUL-terminated and outlives the call, and `buffer`
    // is a writable 256-byte `struct statx`. A kernel without `statx`
    // (ENOSYS) or a seccomp filter that refuses it (EPERM) fails the call,
    // which reads as "no birth time".
    let status = unsafe {
        // Every argument widened to `long`, the way std passes them: the
        // variadic `syscall` reads each one as a `long`.
        libc::syscall(
            libc::SYS_statx,
            libc::c_long::from(libc::AT_FDCWD),
            path.as_ptr(),
            libc::c_long::from(flags),
            // A two-bit mask: lossless as a `long` on every target.
            (STATX_INO | STATX_BTIME) as libc::c_long,
            buffer.as_mut_ptr(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: the zeroed buffer is a valid `Statx` (plain integers), now
    // filled by a successful call.
    let statx = unsafe { buffer.assume_init() };
    // The path may have been replaced between the two stats; a birth time
    // from another object proves nothing.
    if statx.mask & STATX_BTIME == 0 || statx.mask & STATX_INO == 0 || statx.ino != meta.ino() {
        return None;
    }
    let since_epoch =
        std::time::Duration::new(u64::try_from(statx.btime_sec).ok()?, statx.btime_nsec);
    SystemTime::UNIX_EPOCH.checked_add(since_epoch)
}

/// Birth time of the object `meta` describes, where std reports one
/// (`st_birthtime` on macOS; none on Android).
#[cfg(not(target_os = "linux"))]
fn birth_time(_path: &Path, meta: &std::fs::Metadata, _follow: bool) -> Option<SystemTime> {
    meta.created().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(seconds: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
    }

    fn live(dev: u64, ino: u64, birth: Option<SystemTime>) -> LiveIdentity {
        LiveIdentity { dev, ino, birth }
    }

    #[test]
    fn an_unchanged_identity_matches_without_a_birth_time() {
        assert!(matches(
            "16777229:377670759",
            &live(16777229, 377670759, None),
            None
        ));
    }

    #[test]
    fn a_renumbered_device_matches_an_object_born_before_its_journal() {
        // A Mac after a reboot: same inode, APFS device renumbered.
        let renumbered = live(16777231, 377670759, at(1_000));
        assert!(matches("16777229:377670759", &renumbered, at(2_000)));
        assert!(matches("16777229:377670759", &renumbered, at(1_000)));
    }

    #[test]
    fn a_renumbered_device_refuses_an_object_born_after_its_journal() {
        // Recreated (reusing the inode number) after the journal was written.
        let recreated = live(16777231, 377670759, at(3_000));
        assert!(!matches("16777229:377670759", &recreated, at(2_000)));
    }

    #[test]
    fn a_renumbered_device_needs_both_a_birth_time_and_a_journal_time() {
        assert!(!matches("1:7", &live(2, 7, None), at(2_000)));
        assert!(!matches("1:7", &live(2, 7, at(1_000)), None));
    }

    #[test]
    fn another_inode_never_matches() {
        // A copy or restore: new inode, whatever the device or birth.
        assert!(!matches("1:7", &live(1, 8, at(1_000)), at(2_000)));
        assert!(!matches("1:7", &live(2, 8, at(1_000)), at(2_000)));
    }

    #[test]
    fn malformed_or_respelled_records_never_match() {
        let object = live(1, 7, at(1_000));
        assert!(!matches("", &object, at(2_000)));
        assert!(!matches("17", &object, at(2_000)));
        assert!(!matches("1:07", &object, at(2_000)));
        assert!(!matches("01:7", &object, None));
    }

    #[test]
    fn only_a_well_formed_device_may_be_renumbered() {
        // Each of these would pass on inode and birth time alone.
        let object = live(1, 7, at(1_000));
        for recorded in ["01:7", "-:7", ":7", "x:7", "+2:7", "2 :7"] {
            assert!(!matches(recorded, &object, at(2_000)), "{recorded}");
        }
        assert!(matches("2:7", &object, at(2_000)));
    }

    #[test]
    fn a_zero_birth_time_counts_as_unknown() {
        assert_eq!(known(Some(SystemTime::UNIX_EPOCH)), None);
        assert_eq!(known(at(1)), at(1));
        assert_eq!(known(None), None);
    }

    #[test]
    fn birth_time_belongs_to_the_object_it_names() {
        // Where the platform reports a birth time, a fresh directory's is
        // recent and the leaf stat of a symlink reports the link, not its
        // target. Elsewhere both read as unknown.
        let dir = dot_test_support::TempDir::new("persisted-identity-birth").expect("scope");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("target");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let followed = LiveIdentity::of(&link).expect("stat");
        let leaf = LiveIdentity::of_leaf(&link).expect("lstat");
        assert_eq!(followed.ino, LiveIdentity::of(&target).expect("stat").ino);
        assert_ne!(leaf.ino, followed.ino);
        if let Some(birth) = followed.birth {
            let age = SystemTime::now().duration_since(birth).unwrap_or_default();
            assert!(age < Duration::from_secs(600), "{age:?}");
            assert!(leaf.birth.is_some());
        }
    }

    #[test]
    fn birth_time_agrees_with_std_where_std_reports_one() {
        // The renumbering tests skip where no birth time is reported, so a
        // broken `statx` (or `created()`) path would quietly fall back to
        // the exact rule with every test green. std reads the same birth
        // time on glibc Linux (through `statx`) and on macOS, where APFS
        // always stores one.
        let dir = dot_test_support::TempDir::new("persisted-identity-std").expect("scope");
        let ours = LiveIdentity::of(dir.path()).expect("stat").birth;
        if let Ok(created) = std::fs::metadata(dir.path()).and_then(|meta| meta.created()) {
            assert_eq!(ours, known(Some(created)));
        }
        if cfg!(target_os = "macos") {
            assert!(ours.is_some());
        }
    }

    #[test]
    fn newest_journal_time_sees_entries_rewritten_in_place() {
        let dir = dot_test_support::TempDir::new("persisted-identity-journal").expect("scope");
        let journal = dir.path().join("intent");
        std::fs::write(&journal, "x").expect("journal");
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let new = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let set = |path: &Path, time: SystemTime| {
            std::fs::File::open(path)
                .and_then(|file| file.set_modified(time))
                .expect("set mtime");
        };
        set(dir.path(), old);
        set(&journal, new);
        assert_eq!(journal_time(&journal), Some(new));
        assert_eq!(newest_journal_time(dir.path()), Some(new));
        set(&journal, old);
        assert_eq!(newest_journal_time(dir.path()), Some(old));
        assert_eq!(newest_journal_time(&dir.path().join("missing")), None);
    }
}
