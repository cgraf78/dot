//! Owned resources and POSIX process lifecycle.
//!
//! Registry owns explicitly registered children, files and temporary paths.
//! Callers invoke its idempotent cleanup; scoped owners provide Drop where
//! required. Native suite supervision additionally reserves each session
//! leader until descendant teardown completes, then reaps through Child.
//!
//! std owns spawn, file I/O and reaping. This module centralizes the small
//! libc boundary for signal handlers, session identity, non-reaping waitid,
//! descendant adoption, and TERM/group delivery, which std does not expose. Handler callbacks
//! only publish an atomic cancellation flag; resource cleanup runs normally.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::errors::{Error, Result};

/// TERM grace: 20 attempts × 50ms, mirroring
/// `DOT_CLEANUP_GRACE_ATTEMPTS=20` and `sleep 0.05`.
pub const GRACE_ATTEMPTS: u32 = 20;
/// Milliseconds between grace polls.
pub const GRACE_INTERVAL_MS: u64 = 50;
/// Exit status used when owned subprocess cleanup could not be proven.
pub(crate) const CLEANUP_INCOMPLETE_STATUS: i32 = 125;
/// Maximum combined stdout/stderr retained from an inspection subprocess.
/// Repository and provider-preparation queries are data producers, not
/// unbounded streaming commands; exceeding this boundary fails the query and
/// tears down its owned session.
pub(crate) const COMMAND_CAPTURE_LIMIT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const COMMAND_CAPTURE_LIMIT_ERROR: &str = "subprocess output exceeded its safety limit";
const COMMAND_CAPTURE_TICK_BYTES: usize = 64 * 1024;
const COMMAND_CAPTURE_FINAL_BYTES: usize = 1024 * 1024;
const SESSION_BOUNDARY_ENV: &str = "DOT_OWNED_SESSION_BOUNDARY_V1";
const SESSION_LEASE_FDS_ENV: &str = "DOT_OWNED_SESSION_LEASE_FDS_V1";
const SESSION_CONTROL_FDS_ENV: &str = "DOT_OWNED_SESSION_CONTROL_FDS_V1";
const SESSION_BOUNDARY_BYTES: usize = 32;
const MAX_ANCESTOR_BOUNDARIES: usize = 16;
const MAX_ACTIVE_NESTED_SESSIONS: usize = 1024;
const CONTROL_FRAME_MAGIC: &[u8; 4] = b"DTS2";
const CONTROL_FRAME_BYTES: usize = 4 + std::mem::size_of::<u32>() + SESSION_BOUNDARY_BYTES * 2;
static OUTWARD_WRITE_ABORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static TARGET_SIGCHLD_IGNORED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static TARGET_SIGCHLD_FLAGS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static ENTRY_STDIO_MASK: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
const ENTRY_STDIO_INITIALIZED: u8 = 1 << 7;

extern "C" fn capture_entry_stdio() {
    let mut mask = ENTRY_STDIO_INITIALIZED;
    for descriptor in 0..=2 {
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0 {
            mask |= 1 << descriptor;
        }
    }
    ENTRY_STDIO_MASK.store(mask, std::sync::atomic::Ordering::Relaxed);
}

// Rust's runtime deliberately reserves closed standard descriptors before
// `main`. ELF pre-initialization captures the kernel exec-boundary state before
// that normalization; Darwin does not synthesize the descriptors and its
// earliest image initializer provides the same stable snapshot.
// Plain `used` lets the macOS linker drop this initializer from test
// binaries (stable has no `used(linker)`); the functions below that
// consume the mask also take its address so the object is retained.
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    unsafe(link_section = ".preinit_array")
)]
static CAPTURE_ENTRY_STDIO: extern "C" fn() = capture_entry_stdio;

/// Whether a standard descriptor was open at exec. The binary entry point
/// uses this for informational commands that bypass the output relay, so a
/// descriptor closed at exec fails identically on both paths.
pub fn entry_stdio_open(descriptor: i32) -> bool {
    // Take the initializer's address so the linker keeps its object
    // (see above).
    std::hint::black_box(&CAPTURE_ENTRY_STDIO);
    let mask = ENTRY_STDIO_MASK.load(std::sync::atomic::Ordering::Relaxed);
    if mask & ENTRY_STDIO_INITIALIZED == 0 {
        // No exec-boundary capture ran (in-process embedding): probe live.
        return descriptor >= 0 && unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0;
    }
    (0..=2).contains(&descriptor) && mask & (1 << descriptor) != 0
}

/// Create an internal socket pair that cannot alias stdin, stdout, or stderr.
///
/// `socketpair(2)` returns the lowest available descriptors. A caller that
/// intentionally closed stdio can therefore receive fd 0, 1, or 2; passing
/// such an endpoint through `Command` would silently change target stdio.
/// Normalize each owned endpoint with close-on-exec and let RAII close every
/// partial result on failure.
pub(crate) fn internal_stream_pair() -> std::io::Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    let (left, right) = std::os::unix::net::UnixStream::pair()?;
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    let left = normalize_internal_fd(left.into_raw_fd())?;
    let right = normalize_internal_fd(right.into_raw_fd())?;
    // SAFETY: each descriptor is uniquely owned and has been normalized.
    let left = unsafe { std::os::unix::net::UnixStream::from_raw_fd(left.into_raw_fd()) };
    // SAFETY: each descriptor is uniquely owned and has been normalized.
    let right = unsafe { std::os::unix::net::UnixStream::from_raw_fd(right.into_raw_fd()) };
    Ok((left, right))
}

fn internal_datagram_pair() -> std::io::Result<(
    std::os::unix::net::UnixDatagram,
    std::os::unix::net::UnixDatagram,
)> {
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};
    let (left, right) = std::os::unix::net::UnixDatagram::pair()?;
    // Keep default buffer sizes: a large buffer queues bulk stdout flood
    // ahead of stderr diagnostics and the finish record, starving them
    // behind a blocked sink (head-of-line blocking). Small buffers keep
    // backpressure at the writer, where the retry quantum below paces it.
    let left = normalize_internal_fd(left.into_raw_fd())?;
    let right = normalize_internal_fd(right.into_raw_fd())?;
    // SAFETY: each descriptor is uniquely owned and has been normalized.
    let left = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(left.into_raw_fd()) };
    // SAFETY: each descriptor is uniquely owned and has been normalized.
    let right = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(right.into_raw_fd()) };
    Ok((left, right))
}

fn normalize_internal_fd(fd: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    // SAFETY: ownership is transferred exactly once into OwnedFd; every error
    // path drops the currently owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(fd) };
    if descriptor.as_raw_fd() >= 3 {
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe {
                libc::fcntl(
                    descriptor.as_raw_fd(),
                    libc::F_SETFD,
                    flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        return Ok(descriptor);
    }
    let duplicate = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    Ok(duplicate)
}

#[derive(Clone)]
struct ActiveSessionBoundary {
    boundary: String,
    control: std::sync::Arc<std::sync::Mutex<NestedControlState>>,
}

struct NestedControlState {
    complete: bool,
    supervisors: std::collections::BTreeMap<String, u32>,
}

impl NestedControlState {
    fn new() -> Self {
        Self {
            complete: true,
            supervisors: std::collections::BTreeMap::new(),
        }
    }
}

fn active_session_boundaries() -> &'static Mutex<
    std::collections::BTreeMap<u32, std::collections::BTreeMap<u64, ActiveSessionBoundary>>,
> {
    static BOUNDARIES: OnceLock<
        Mutex<
            std::collections::BTreeMap<u32, std::collections::BTreeMap<u64, ActiveSessionBoundary>>,
        >,
    > = OnceLock::new();
    BOUNDARIES.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

static NEXT_PROCESS_REGISTRATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn next_process_registration() -> u64 {
    NEXT_PROCESS_REGISTRATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

fn register_session_boundary(
    pid: u32,
    token: &str,
    control: std::sync::Arc<std::sync::Mutex<NestedControlState>>,
) -> u64 {
    let _state = PROCESS_OWNERSHIP_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let registration = next_process_registration();
    active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(pid)
        .or_default()
        .insert(
            registration,
            ActiveSessionBoundary {
                boundary: token.to_string(),
                control,
            },
        );
    PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    registration
}

fn unregister_session_boundary(pid: u32, registration: u64) {
    let _state = PROCESS_OWNERSHIP_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut boundaries = active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let removed = boundaries
        .get_mut(&pid)
        .is_some_and(|registrations| registrations.remove(&registration).is_some());
    if boundaries
        .get(&pid)
        .is_some_and(std::collections::BTreeMap::is_empty)
    {
        boundaries.remove(&pid);
    }
    if removed {
        PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn session_boundary(pid: u32) -> Option<String> {
    active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&pid)
        .and_then(|registrations| registrations.last_key_value())
        .map(|(_registration, active)| active.boundary.clone())
}

fn session_control(pid: u32) -> Option<std::sync::Arc<std::sync::Mutex<NestedControlState>>> {
    active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&pid)
        .and_then(|registrations| registrations.last_key_value())
        .map(|(_registration, active)| active.control.clone())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_registered_session_leader(pid: u32) -> bool {
    active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&pid)
}

fn active_status_children()
-> &'static Mutex<std::collections::BTreeMap<u32, std::collections::BTreeSet<u64>>> {
    static CHILDREN: OnceLock<
        Mutex<std::collections::BTreeMap<u32, std::collections::BTreeSet<u64>>>,
    > = OnceLock::new();
    CHILDREN.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

static STATUS_CHILD_LAUNCHES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static PROCESS_OWNERSHIP_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static PROCESS_OWNERSHIP_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

struct StatusChildLaunch {
    track_generation: bool,
}

impl StatusChildLaunch {
    fn begin() -> Self {
        Self::begin_with_generation(true)
    }

    fn begin_with_generation(track_generation: bool) -> Self {
        let _state = PROCESS_OWNERSHIP_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        STATUS_CHILD_LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if track_generation {
            PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Self { track_generation }
    }
}

impl Drop for StatusChildLaunch {
    fn drop(&mut self) {
        let _state = PROCESS_OWNERSHIP_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        STATUS_CHILD_LAUNCHES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if self.track_generation {
            PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[derive(Debug)]
struct StatusChildRegistration {
    pid: u32,
    registration: u64,
    track_generation: bool,
}

impl StatusChildRegistration {
    fn new(pid: u32) -> Self {
        Self::new_with_generation(pid, true)
    }

    fn new_with_generation(pid: u32, track_generation: bool) -> Self {
        let _state = PROCESS_OWNERSHIP_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let registration = next_process_registration();
        active_status_children()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(pid)
            .or_default()
            .insert(registration);
        if track_generation {
            PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Self {
            pid,
            registration,
            track_generation,
        }
    }
}

impl Drop for StatusChildRegistration {
    fn drop(&mut self) {
        let _state = PROCESS_OWNERSHIP_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut children = active_status_children()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = children
            .get_mut(&self.pid)
            .is_some_and(|registrations| registrations.remove(&self.registration));
        if children
            .get(&self.pid)
            .is_some_and(|entries| entries.is_empty())
        {
            children.remove(&self.pid);
        }
        if removed && self.track_generation {
            PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

// Needed on macOS test builds (registry bookkeeping assertions) in addition
// to the Linux/Android reconciliation passes that use it in production.
#[cfg(any(test, target_os = "linux", target_os = "android"))]
fn is_registered_status_child(pid: u32) -> bool {
    active_status_children()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&pid)
}

/// Live owned-session lease writers by session leader. A direct child that
/// holds this session's lease socket inherited it through this session's
/// spawn tree, which is the only positive parentage proof available once a
/// reparented orphan has scrubbed its environment marker. Foreign children
/// of an embedded host process never hold it.
fn active_session_leases() -> &'static Mutex<std::collections::BTreeMap<u32, u64>> {
    static LEASES: OnceLock<Mutex<std::collections::BTreeMap<u32, u64>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

fn register_session_lease(leader: u32, inode: u64) {
    let _state = PROCESS_OWNERSHIP_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    active_session_leases()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(leader, inode);
    PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn unregister_session_lease(leader: u32) {
    let _state = PROCESS_OWNERSHIP_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let removed = active_session_leases()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&leader)
        .is_some();
    if removed {
        PROCESS_OWNERSHIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Identities a session actually observed as its members. The
/// adopted-zombie reaper may only consume wait status for these: an
/// unregistered direct zombie outside this set is either foreign (an
/// embedded host's child) or retained through a live handle, and stealing
/// its status would hand its owner a spurious `ECHILD`. Entries carry the
/// kernel start generation, so PID reuse can never match a stale record.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn known_session_descendants() -> &'static Mutex<std::collections::BTreeSet<ProcessIdentity>> {
    static KNOWN: OnceLock<Mutex<std::collections::BTreeSet<ProcessIdentity>>> = OnceLock::new();
    KNOWN.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn record_session_descendant(identity: &ProcessIdentity) {
    known_session_descendants()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(identity.clone());
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_recorded_session_descendant(identity: &ProcessIdentity) -> bool {
    known_session_descendants()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(identity)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn forget_session_descendant(identity: &ProcessIdentity) {
    known_session_descendants()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(identity);
}

/// Reap recorded adoptees that `waitid(P_ALL)` cannot reach behind a
/// protected waitable child, without a host-wide snapshot. Every member the
/// session observed is already recorded with its kernel start generation,
/// so targeted `P_PID` waits prove the same attribution enumeration would.
/// A snapshot here would serialize every teardown on `/proc` and miss its
/// deadline under parallel load, failing sessions that are otherwise
/// clean. Unrecorded same-group stragglers are swept next by
/// [`reap_session_group_stragglers`]; only members that also left their
/// leader's group wait for a pass whose `P_ALL` drain is unblocked.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_recorded_descendants() -> std::io::Result<()> {
    let recorded = known_session_descendants()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if recorded.is_empty() {
        return Ok(());
    }
    let _fork_registration = STATUS_CHILD_FORK_REGISTRATION
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for identity in recorded {
        let Some(current) = linux_process_info(identity.pid) else {
            continue;
        };
        if current.identity != identity
            || current.live
            || current.parent != std::process::id()
            || is_registered_session_leader(current.pid)
            || is_registered_status_child(current.pid)
            || !reaper_may_consume(&current)
        {
            continue;
        }
        reap_observed_zombie(&current)?;
    }
    Ok(())
}

/// Whether the adopted-zombie reaper may consume this zombie's status. A
/// recorded adoptee was attributed while live; a zombie whose session is a
/// live registered leader is a session member the fast path never needed to
/// observe. With no registered owners at all, this process is exclusively
/// owned (a helper or a real binary between sessions), so an unregistered
/// direct zombie has no other plausible owner: the only exception is a child
/// whose pre-exec hook just failed, which the spawner still reports as an
/// error either way. Anything else is foreign or retained and keeps its
/// status. All three checks read live state, so overlapping launches can
/// delay discovery but never authorize consuming foreign status.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reaper_may_consume(process: &ProcessInfo) -> bool {
    is_recorded_session_descendant(&process.identity)
        || is_registered_session_leader(process.session)
        || no_registered_owners()
}

/// Whether no session or status child currently owns wait status.
/// Production reaper passes always run under a live session registration;
/// only an exclusively owned process observes a fully idle registry.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn no_registered_owners() -> bool {
    active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_empty()
        && active_status_children()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn session_lease_inode(leader: u32) -> Option<u64> {
    active_session_leases()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&leader)
        .copied()
}

fn new_session_boundary() -> std::io::Result<String> {
    let mut random = [0u8; SESSION_BOUNDARY_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut token = String::with_capacity(SESSION_BOUNDARY_BYTES * 2);
    use std::fmt::Write as _;
    for byte in random {
        write!(&mut token, "{byte:02x}").expect("write to String");
    }
    Ok(token)
}

fn command_environment(command: &Command, name: &str) -> Option<std::ffi::OsString> {
    if let Some((_key, value)) = command.get_envs().find(|(key, _value)| *key == name) {
        return value.map(std::ffi::OsStr::to_os_string);
    }
    std::env::var_os(name)
}

fn valid_boundary_token(token: &str) -> bool {
    token.len() == SESSION_BOUNDARY_BYTES * 2 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn session_boundary_chain(command: &Command, current: &str) -> std::io::Result<Vec<String>> {
    if !valid_boundary_token(current) {
        return Err(std::io::Error::other("invalid owned-session boundary"));
    }
    let mut tokens = match command_environment(command, SESSION_BOUNDARY_ENV) {
        None => Vec::new(),
        Some(value) => {
            let value = value
                .into_string()
                .map_err(|_| std::io::Error::other("invalid inherited owned-session boundary"))?;
            let tokens = value.split(':').map(str::to_owned).collect::<Vec<_>>();
            if tokens.is_empty()
                || tokens.len() >= MAX_ANCESTOR_BOUNDARIES
                || tokens.iter().any(|token| !valid_boundary_token(token))
            {
                return Err(std::io::Error::other(
                    "invalid or overlong inherited owned-session boundary",
                ));
            }
            tokens
        }
    };
    tokens.push(current.to_owned());
    Ok(tokens)
}

fn inherited_session_lease_fds(command: &Command) -> std::io::Result<Vec<i32>> {
    inherited_socket_fds(
        command_environment(command, SESSION_LEASE_FDS_ENV),
        "session lease",
    )
}

fn inherited_session_control_fds(command: &Command) -> std::io::Result<Vec<i32>> {
    inherited_socket_fds(
        command_environment(command, SESSION_CONTROL_FDS_ENV),
        "supervisor control",
    )
}

fn inherited_socket_fds(
    value: Option<std::ffi::OsString>,
    description: &str,
) -> std::io::Result<Vec<i32>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let value = value
        .to_str()
        .ok_or_else(|| std::io::Error::other(format!("invalid inherited {description} list")))?;
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut fds = Vec::new();
    for field in value.split(':') {
        if fds.len() >= MAX_ANCESTOR_BOUNDARIES {
            return Err(std::io::Error::other(format!(
                "too many inherited {description} descriptors"
            )));
        }
        let fd = field
            .parse::<i32>()
            .ok()
            .filter(|fd| *fd >= 3)
            .ok_or_else(|| {
                std::io::Error::other(format!("invalid inherited {description} descriptor"))
            })?;
        // SAFETY: fstat only writes initialized local storage and does not
        // consume the caller-owned descriptor.
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut metadata) } != 0
            || metadata.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || fds.contains(&fd)
        {
            return Err(std::io::Error::other(format!(
                "invalid inherited {description} descriptor"
            )));
        }
        fds.push(fd);
    }
    Ok(fds)
}

fn send_nested_registration(
    control_fd: i32,
    boundary: &str,
    registration_fd: i32,
) -> std::io::Result<()> {
    send_nested_registration_with_pid(control_fd, boundary, registration_fd, std::process::id())
}

fn send_nested_registration_with_pid(
    control_fd: i32,
    boundary: &str,
    registration_fd: i32,
    pid: u32,
) -> std::io::Result<()> {
    let mut frame = [0u8; CONTROL_FRAME_BYTES];
    frame[..4].copy_from_slice(CONTROL_FRAME_MAGIC);
    frame[4..8].copy_from_slice(&pid.to_be_bytes());
    frame[8..].copy_from_slice(boundary.as_bytes());
    let mut iovec = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let control_bytes =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as usize;
    let mut control = vec![0usize; control_bytes.div_ceil(std::mem::size_of::<usize>())];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // msg_controllen is size_t on Linux but socklen_t (u32) on macOS.
    message.msg_controllen = control_bytes as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(std::io::Error::other(
                "could not construct nested-supervisor registration",
            ));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        // cmsg_len is size_t on Linux but socklen_t (u32) on macOS.
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
        std::ptr::write(
            libc::CMSG_DATA(header).cast::<libc::c_int>(),
            registration_fd,
        );
    }
    // One datagram carries one fixed frame and one private per-session stream.
    // The immediate parent validates the stream peer before acknowledging it.
    let written = unsafe {
        libc::sendmsg(
            control_fd,
            &message,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if written == frame.len() as isize {
        Ok(())
    } else if written < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::other("short supervisor registration"))
    }
}

fn publish_nested_supervisor(
    control_fds: &[i32],
    boundary: &str,
) -> std::io::Result<Option<std::os::unix::net::UnixStream>> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    let Some(&control_fd) = control_fds.last() else {
        return Ok(None);
    };
    if control_fds.len() != 1 {
        return Err(std::io::Error::other(
            "nested supervision requires one immediate-parent control",
        ));
    }
    // This is the immediate supervisor's rendezvous, never a capability for
    // arbitrary target code.  The current wrapper uses it once, while its own
    // freshly created control endpoint is the only one passed across exec.
    let flags = unsafe { libc::fcntl(control_fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(control_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let (mut local, parent) = internal_stream_pair()?;
    local.set_nonblocking(true)?;
    send_nested_registration(control_fd, boundary, parent.as_raw_fd())?;
    // Hold our copy of the registered end until the ACK below, not just
    // until the send. The datagram (with its in-flight descriptor) can
    // sit unread while the worker ticks, and closing our copy during
    // that window corrupts the pending macOS install: the worker has
    // received the same fd number twice (two map entries, one socket)
    // and links with phantom queued bytes plus HUP despite open peers.
    // Keeping the sender copy open until acknowledged removes the
    // close-during-queue race; the binding drops at scope end on every
    // return path.
    let _hold_parent_until_ack = parent;
    let deadline = cleanup_deadline();
    let mut reply = [0u8; 1];
    loop {
        match local.read(&mut reply) {
            Ok(1) if reply[0] == 1 => return Ok(Some(local)),
            Ok(0) | Ok(_) => {
                return Err(std::io::Error::other(
                    "nested-supervisor registration was rejected",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        if received_signal().is_some() {
            return Err(signal_io_error());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nested-supervisor registration timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct NestedRegistration {
    pid: u32,
    boundary: String,
    link: std::os::unix::net::UnixStream,
}

fn nested_peer_pid(fd: i32) -> std::io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        ) != 0
            || length as usize != std::mem::size_of::<libc::ucred>()
            || credentials.pid <= 0
        {
            return Err(std::io::Error::last_os_error());
        }
        u32::try_from(credentials.pid)
            .map_err(|_| std::io::Error::other("invalid nested-supervisor peer PID"))
    }

    #[cfg(target_os = "macos")]
    unsafe {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        ) != 0
            || length as usize != std::mem::size_of::<libc::pid_t>()
            || pid <= 0
        {
            return Err(std::io::Error::last_os_error());
        }
        return u32::try_from(pid)
            .map_err(|_| std::io::Error::other("invalid nested-supervisor peer PID"));
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = fd;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "nested-supervisor peer identity is unavailable",
        ))
    }
}

fn receive_nested_registration(
    control: &std::os::unix::net::UnixDatagram,
) -> std::io::Result<Option<NestedRegistration>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let mut frame = [0u8; CONTROL_FRAME_BYTES + 1];
    let mut iovec = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let control_bytes =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as usize;
    let mut ancillary = vec![0usize; control_bytes.div_ceil(std::mem::size_of::<usize>())];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    // msg_controllen is size_t on Linux but socklen_t (u32) on macOS.
    message.msg_controllen = control_bytes as _;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;
    // The control socket is already nonblocking, so per-call flags are
    // redundant here. macOS CI invalidates the nested-supervisor channel on
    // every session while Linux never does; pass no flags so a platform
    // quirk in flag handling cannot fail the receive.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = 0;
    let received = unsafe { libc::recvmsg(control.as_raw_fd(), &mut message, flags) };
    if received < 0 {
        let error = std::io::Error::last_os_error();
        // macOS reports ECONNRESET (not EAGAIN) when the datagram queue is
        // empty: the child closes the CLOEXEC control writer at exec, so a
        // session with no nested supervisor deterministically resets while
        // Linux reports EAGAIN for the same state. Tolerate the reset as
        // "no data this tick": queued frames are still returned normally,
        // the worker keeps polling, and a reset carries no bytes that
        // could forge a delegation, so fail-closed behavior is unchanged.
        // TEMP-DIAG-180: remove once macOS control-channel failures are
        // root-caused. Logs the raw errno behind a channel invalidation.
        if !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionReset
        ) {
            eprintln!(
                "TEMP-DIAG-180: nested-control recvmsg failed: {error:?} \
                 (raw_os_error={:?})",
                error.raw_os_error()
            );
        }
        // Darwin reports a momentarily exhausted socket buffer as ENOBUFS
        // (raw 55, surfaced as `Uncategorized`), not `WouldBlock`; treat
        // it as transient like the other retryable reads.
        return if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionReset
        ) || error.raw_os_error() == Some(libc::ENOBUFS)
        {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut descriptors = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let header_bytes = libc::CMSG_LEN(0) as usize;
                // cmsg_len is size_t on Linux but socklen_t (u32) on macOS.
                let payload_bytes = ((*header).cmsg_len as usize).saturating_sub(header_bytes);
                let count = payload_bytes / std::mem::size_of::<libc::c_int>();
                let values = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for index in 0..count {
                    descriptors.push(*values.add(index));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    let invalid = received as usize != CONTROL_FRAME_BYTES
        || &frame[..4] != CONTROL_FRAME_MAGIC
        || message.msg_flags & libc::MSG_CTRUNC != 0
        || descriptors.len() != 1;
    if invalid {
        for descriptor in descriptors {
            unsafe { libc::close(descriptor) };
        }
        return Err(std::io::Error::other(
            "invalid nested-supervisor registration",
        ));
    }
    let descriptor = normalize_internal_fd(descriptors.pop().expect("one received descriptor"))?;
    // SAFETY: the normalized descriptor is one uniquely owned stream endpoint.
    let link = unsafe {
        std::os::unix::net::UnixStream::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(descriptor))
    };
    link.set_nonblocking(true)?;
    let pid = u32::from_be_bytes(frame[4..8].try_into().expect("registration PID"));
    let boundary = std::str::from_utf8(&frame[8..CONTROL_FRAME_BYTES])
        .ok()
        .filter(|token| valid_boundary_token(token))
        .ok_or_else(|| std::io::Error::other("invalid nested-supervisor boundary"))?
        .to_owned();
    if nested_peer_pid(link.as_raw_fd())? != pid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nested-supervisor peer identity did not match",
        ));
    }
    Ok(Some(NestedRegistration {
        pid,
        boundary,
        link,
    }))
}

fn cleanup_deadline() -> Instant {
    Instant::now() + Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS)
}

fn poll_until<T>(
    deadline: Instant,
    mut poll: impl FnMut() -> std::io::Result<Option<T>>,
) -> std::io::Result<T> {
    loop {
        if let Some(value) = poll()? {
            return Ok(value);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "subprocess did not become observable before cleanup deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(10).min(deadline.saturating_duration_since(now)));
    }
}

fn wait_child_until(
    child: &mut Child,
    deadline: Instant,
) -> std::io::Result<std::process::ExitStatus> {
    poll_until(deadline, || child.try_wait())
}

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
    child_registrations: std::collections::BTreeMap<u32, StatusChildRegistration>,
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
        self.child_registrations
            .insert(child.id(), StatusChildRegistration::new(child.id()));
        self.children.push(child);
    }

    /// Stop tracking the child with this pid. Returns whether one was
    /// present (the shell unregisters every match; PIDs are unique here
    /// by handle construction).
    pub fn untrack_child(&mut self, pid: u32) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id() != pid);
        let removed = self.children.len() != before;
        if removed {
            self.child_registrations.remove(&pid);
        }
        removed
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
        self.child_registrations.clear();
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

/// Send TERM to a registered child before bounded grace and KILL escalation.
fn terminate(pid: u32) {
    signal_pid(pid, libc::SIGTERM);
}

/// Deliver a signal to a positive process identity held by the caller.
pub(crate) fn signal_pid(pid: u32, signal: i32) {
    if let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) {
        // SAFETY: positive PID, no pointer arguments. Callers retain ownership.
        unsafe { libc::kill(pid, signal) };
    }
}

/// Deliver a signal to the group of an unreaped owned leader.
#[cfg(test)]
pub(crate) fn signal_group(pid: u32, signal: i32) {
    let _ = signal_group_result(pid, signal);
}

fn signal_group_result(pid: u32, signal: i32) -> std::io::Result<bool> {
    if let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 0) {
        // SAFETY: only the explicitly owned group is addressed.
        if unsafe { libc::kill(-pid, signal) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        // EPERM means no group member accepted the signal. Any live owned
        // member is same-uid signalable (stopped members queue the signal,
        // zombies accept it), so a fully unsignalable group holds nothing
        // owned: the pgid is stale or reused by a foreign group. Report
        // undelivered without failing teardown; the shell reference ignores
        // group-kill errors the same way (`|| true`) and verifies absence
        // through snapshots instead of signal delivery.
        if error.raw_os_error() == Some(libc::EPERM) {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(false)
}

/// Effective process identity, never an environment-provided value.
pub(crate) fn euid() -> u32 {
    // SAFETY: get effective uid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Conservative foreign-process liveness for stale-directory pruning.
pub(crate) fn alive(pid: i32) -> bool {
    // SAFETY: positive PID and signal zero only probe existence.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

const SIGNAL_CLOSED: i32 = -1;
static INTERRUPTED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static CLEANUP_INCOMPLETE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
// TEMP-DIAG-180: first-wins attribution for the sticky atomic. Remove with
// the 125 fix.
static CLEANUP_INCOMPLETE_SOURCE: std::sync::Mutex<Option<&'static str>> =
    std::sync::Mutex::new(None);

/// TEMP-DIAG-180: record which production path set the sticky atomic.
/// First setter wins: it names the original poison when a later
/// supervision succeeds but the process still exits 125.
fn set_cleanup_incomplete(source: &'static str) {
    CLEANUP_INCOMPLETE.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut guard = CLEANUP_INCOMPLETE_SOURCE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if guard.is_none() {
        *guard = Some(source);
    }
}

/// TEMP-DIAG-180: read the first-wins setter attribution.
fn cleanup_incomplete_source() -> Option<&'static str> {
    *CLEANUP_INCOMPLETE_SOURCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}
static ACTIVE_HANDLERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SIGNAL_OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(any(target_os = "linux", target_os = "android"))]
static ADOPTED_ZOMBIE_REAPER: std::sync::Mutex<()> = std::sync::Mutex::new(());
static STATUS_CHILD_FORK_REGISTRATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
const HANDLED_SIGNALS: [i32; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

#[cfg(test)]
thread_local! {
    /// Deterministic performance seam: a normal owned child must not require
    /// a host-wide `/proc` walk for each stable-empty observation.
    static GLOBAL_PROCESS_SNAPSHOT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
static FORCE_PIDFD_UNAVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
static FORCE_PROC_SNAPSHOT_UNAVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
static FORCE_FALLBACK_PROCESS_INFO_UNAVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn reset_global_process_snapshot_calls() {
    GLOBAL_PROCESS_SNAPSHOT_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn global_process_snapshot_calls() -> usize {
    GLOBAL_PROCESS_SNAPSHOT_CALLS.with(std::cell::Cell::get)
}

extern "C" fn interrupted(signal: i32) {
    // A signal handler must neither allocate nor lock nor touch owned resources.
    ACTIVE_HANDLERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _ = INTERRUPTED.compare_exchange(
        0,
        signal,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    );
    ACTIVE_HANDLERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
}

/// Process-local handler guard. Process-wide dispositions admit only one owner
/// even when public command entry points are invoked by concurrent threads.
pub(crate) struct Signals {
    previous: Vec<(i32, libc::sigaction)>,
    previous_sigchld: Option<libc::sigaction>,
    _owner: Option<std::sync::MutexGuard<'static, ()>>,
    restore: bool,
    active: bool,
}

impl Signals {
    #[cfg(test)]
    pub(crate) fn install() -> std::io::Result<Self> {
        Self::install_with_restore(true)
    }

    /// Own signals only for a real process entry. Embedded invocations are
    /// signal-neutral and isolate through [`crate::app::run`] instead.
    pub(crate) fn for_runtime(runtime: &crate::app::Runtime) -> std::io::Result<Self> {
        if runtime.is_process_entry() {
            Self::install_with_restore(false)
        } else {
            Ok(Self {
                previous: Vec::new(),
                previous_sigchld: None,
                _owner: None,
                restore: false,
                active: false,
            })
        }
    }

    fn install_with_restore(restore: bool) -> std::io::Result<Self> {
        let owner = SIGNAL_OWNER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        INTERRUPTED.store(0, std::sync::atomic::Ordering::SeqCst);
        CLEANUP_INCOMPLETE.store(false, std::sync::atomic::Ordering::SeqCst);
        // TEMP-DIAG-180: fresh signal ownership clears the attribution too.
        *CLEANUP_INCOMPLETE_SOURCE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        OUTWARD_WRITE_ABORTED.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut guard = Self {
            previous: Vec::new(),
            previous_sigchld: None,
            _owner: Some(owner),
            // A partial install must restore what it changed on error. Switch
            // to process-lifetime ownership only after all actions succeed.
            restore: true,
            active: true,
        };
        for signal in HANDLED_SIGNALS {
            // SAFETY: zero initialization is valid for sigaction, the mask is
            // initialized explicitly, and saved actions outlive each call.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = interrupted as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut previous) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                guard.previous.push((signal, previous));
            }
        }
        // The supervisor must retain child statuses even if its caller used
        // SIG_IGN or SA_NOCLDWAIT. Preserve those exec-visible semantics for
        // authorized targets, then install a plain default action internally.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let mut previous: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGCHLD, &action, &mut previous) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            TARGET_SIGCHLD_IGNORED.store(
                previous.sa_sigaction == libc::SIG_IGN,
                std::sync::atomic::Ordering::SeqCst,
            );
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let inherited_flags = previous.sa_flags & libc::SA_NOCLDWAIT;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let inherited_flags = 0;
            TARGET_SIGCHLD_FLAGS.store(inherited_flags, std::sync::atomic::Ordering::SeqCst);
            guard.previous_sigchld = Some(previous);
        }
        guard.restore = restore;
        Ok(guard)
    }

    pub(crate) fn received(&self) -> Option<i32> {
        self.active.then(received_signal).flatten()
    }

    /// Wrap an output sink so a handled signal can break a blocked write.
    pub(crate) fn writer<'a>(&'a self, inner: &'a mut dyn std::io::Write) -> SignalWriter<'a> {
        SignalWriter {
            inner,
            signals: self,
        }
    }

    /// Restore the caller's dispositions and return the final command status
    /// without losing a signal in the load-versus-restore window.
    pub(crate) fn finish(self, code: i32) -> i32 {
        let owns_process_signals = self.active;
        let signal = self.finish_signal();
        if owns_process_signals && CLEANUP_INCOMPLETE.load(std::sync::atomic::Ordering::SeqCst) {
            // TEMP-DIAG-180: remove with the 125 fix.
            eprintln!(
                "TEMP-DIAG-180: finish: CLEANUP_INCOMPLETE atomic overrode code={code} signal={signal:?} source={:?}",
                cleanup_incomplete_source()
            );
            CLEANUP_INCOMPLETE_STATUS
        } else {
            signal.map_or(code, |signal| 128 + signal)
        }
    }

    fn finish_signal(mut self) -> Option<i32> {
        self.close()
    }

    fn close(&mut self) -> Option<i32> {
        if !self.active {
            return None;
        }
        let mut blocked = BlockedLaunchSignals::install().ok();
        // Blocking on the owner thread turns a delivery during disposition
        // handoff into a pending signal instead of discarding it. Callbacks on
        // another thread remain visible through the atomic latch.
        while ACTIVE_HANDLERS.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
        record_pending_signal();
        if self.restore {
            self.restore();
            while ACTIVE_HANDLERS.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                std::thread::yield_now();
            }
            // A signal delivered while its old action was still installed is
            // either latched by that callback or pending on this blocked
            // thread. Consume both before returning ownership to the caller.
            record_pending_signal();
            let received =
                match INTERRUPTED.swap(SIGNAL_CLOSED, std::sync::atomic::Ordering::SeqCst) {
                    signal if signal > 0 => Some(signal),
                    _ => None,
                };
            if let Some(mask) = blocked.as_mut() {
                let _ = mask.restore();
            }
            self.active = false;
            return received;
        } else {
            // Process entry keeps the capture actions and blocked mask until
            // `exit_process` installs terminal handlers and calls `_exit`.
            // A signal in that handoff is either latched on another thread or
            // remains pending on this one; neither can disappear as SIG_IGN.
            self.previous.clear();
            self.previous_sigchld.take();
            if let Some(mask) = blocked.take() {
                mask.keep_blocked();
            }
        }
        self.active = false;
        match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
            signal if signal > 0 => Some(signal),
            _ => None,
        }
    }

    fn restore(&mut self) {
        for (signal, action) in self.previous.drain(..) {
            // SAFETY: restore the exact initialized action saved at install.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        }
        if let Some(action) = self.previous_sigchld.take() {
            // SAFETY: restore the exact action captured before supervision.
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) };
        }
    }
}

fn record_pending_signal() {
    // The caller blocks every handled signal before entering this loop.
    // sigtimedwait consumes all currently pending instances so restoring a
    // caller's disposition cannot deliver a signal that this owner already
    // incorporated into its result.
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in HANDLED_SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let signal = libc::sigtimedwait(&set, &mut info, &timeout);
            if signal > 0 {
                let _ = INTERRUPTED.compare_exchange(
                    0,
                    signal,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                );
                continue;
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                _ => break,
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    drain_pending_signals_without_sigtimedwait();
}

/// Pending-signal drain for platforms without `sigtimedwait` (macOS, and
/// Android below the API level that provides it): `sigpending` observes the
/// blocked set, the first pending handled signal is latched, and a brief
/// SIG_IGN round-trip consumes every pending instance before the saved
/// dispositions are restored. A signal arriving inside the round-trip window
/// is discarded instead of staying pending; teardown still converges because
/// the owner either already latched an instance or re-observes on its next
/// drain. Compiled on Linux for direct test coverage only.
#[cfg(any(test, not(target_os = "linux")))]
fn drain_pending_signals_without_sigtimedwait() {
    // SAFETY: all sets and actions name initialized local storage; the
    // caller blocks every handled signal, so observation here cannot race
    // delivery to this thread.
    unsafe {
        let mut pending: libc::sigset_t = std::mem::zeroed();
        if libc::sigpending(&mut pending) != 0 {
            return;
        }
        for signal in HANDLED_SIGNALS {
            if libc::sigismember(&pending, signal) > 0 {
                let _ = INTERRUPTED.compare_exchange(
                    0,
                    signal,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                );
                break;
            }
        }
        for signal in HANDLED_SIGNALS {
            let mut saved: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut saved) != 0 {
                continue;
            }
            let mut ignore: libc::sigaction = std::mem::zeroed();
            ignore.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignore.sa_mask);
            if libc::sigaction(signal, &ignore, std::ptr::null_mut()) != 0 {
                continue;
            }
            // Discarding is complete once SIG_IGN is installed; restore the
            // exact disposition saved above (normally the owner's latch).
            libc::sigaction(signal, &saved, std::ptr::null_mut());
        }
    }
}

extern "C" fn terminal_signal(signal: i32) {
    let code = if CLEANUP_INCOMPLETE.load(std::sync::atomic::Ordering::SeqCst) {
        CLEANUP_INCOMPLETE_STATUS
    } else {
        match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
            first if first > 0 => 128 + first,
            _ => 128 + signal,
        }
    };
    // SAFETY: `_exit` is async-signal-safe and terminates without running
    // destructors from a signal context.
    unsafe { libc::_exit(code) }
}

/// Complete the real process-entry signal handoff without a disposition gap.
/// All output must already be flushed before this function is called.
#[doc(hidden)]
pub fn exit_process(code: i32) -> ! {
    exit_process_with_hook(code, |_| {})
}

fn exit_process_with_hook(code: i32, mut after_install: impl FnMut(i32)) -> ! {
    let blocked = BlockedLaunchSignals::install().ok();
    while ACTIVE_HANDLERS.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        std::thread::yield_now();
    }
    record_pending_signal();
    for signal in HANDLED_SIGNALS {
        // SAFETY: zero initialization is valid for sigaction; every installed
        // terminal action calls only atomics and `_exit`.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = terminal_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
        after_install(signal);
    }
    record_pending_signal();
    let final_code = if CLEANUP_INCOMPLETE.load(std::sync::atomic::Ordering::SeqCst) {
        // TEMP-DIAG-180: remove with the 125 fix.
        eprintln!(
            "TEMP-DIAG-180: exit_process: CLEANUP_INCOMPLETE atomic overrode code={code} source={:?}",
            cleanup_incomplete_source()
        );
        CLEANUP_INCOMPLETE_STATUS
    } else {
        match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
            signal if signal > 0 => 128 + signal,
            _ => code,
        }
    };
    if blocked.is_some() {
        // Process entry deliberately unblocks the handled set rather than
        // restoring a caller mask: a signal arriving after the final pending
        // check must run `terminal_signal` before the fallback `_exit`.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in HANDLED_SIGNALS {
                libc::sigaddset(&mut set, signal);
            }
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
    }
    // SAFETY: this is the terminal process-entry handoff; output has already
    // been flushed and no Rust destructor is part of the public contract.
    unsafe { libc::_exit(final_code) }
}

/// A writer that turns a latched signal into a non-retriable I/O error.
///
/// `write_all` retries `Interrupted`, so returning that error kind would leave
/// a command stuck when its consumer stops reading. `Other` escapes the retry
/// loop; the owning [`Signals`] guard then converts the command result to the
/// conventional signal status.
pub(crate) struct SignalWriter<'a> {
    inner: &'a mut dyn std::io::Write,
    signals: &'a Signals,
}

impl std::io::Write for SignalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.signals.received().is_some() {
            return Err(signal_io_error());
        }
        match self.inner.write(bytes) {
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted
                    && self.signals.received().is_some() =>
            {
                Err(signal_io_error())
            }
            result => result,
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.signals.received().is_some() {
            return Err(signal_io_error());
        }
        match self.inner.flush() {
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted
                    && self.signals.received().is_some() =>
            {
                Err(signal_io_error())
            }
            result => result,
        }
    }
}

fn signal_io_error() -> std::io::Error {
    std::io::Error::other("interrupted by signal")
}

/// Return the signal captured by the invocation owner, if any.
///
/// Worker threads cannot borrow the guard that owns the process handler, but
/// they must still stop their isolated child sessions before the top-level
/// command returns the conventional `128 + signal` status.
pub(crate) fn received_signal() -> Option<i32> {
    match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        signal if signal > 0 => Some(signal),
        _ => None,
    }
}

pub(crate) fn cleanup_incomplete() -> bool {
    CLEANUP_INCOMPLETE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether the process-descriptor adapter observed a handled signal. This is
/// public only for the package binary's unbuffered writer.
#[doc(hidden)]
pub fn outward_write_interrupted() -> bool {
    received_signal().is_some()
}

/// Whether a subprocess deadline has cancelled waiting on a backpressured
/// process descriptor. Regular-file output remains safe to finish.
#[doc(hidden)]
pub fn outward_write_aborted() -> bool {
    OUTWARD_WRITE_ABORTED.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn abort_outward_writes() {
    OUTWARD_WRITE_ABORTED.store(true, std::sync::atomic::Ordering::Release);
}

pub(crate) fn resume_outward_writes() {
    OUTWARD_WRITE_ABORTED.store(false, std::sync::atomic::Ordering::Release);
}

const OUTPUT_RELAY_PAYLOAD_BYTES: usize = 512;
const OUTPUT_RELAY_FINISH: u8 = 0;

struct ProcessOutputChannel {
    sender: std::os::unix::net::UnixDatagram,
    failed: std::sync::atomic::AtomicBool,
    used: std::sync::atomic::AtomicBool,
}

/// One process-entry output stream backed by an independently killable relay.
///
/// The caller-facing write never touches the external descriptor. Small
/// datagrams are sent nonblocking to a private socket instead, so another
/// writer consuming readiness on the caller's pipe, a slow device/FUSE file,
/// or an indefinitely blocked sink cannot trap the supervising process in a
/// write syscall after cancellation or a provider deadline.
pub struct ProcessRelayWriter {
    channel: Option<std::sync::Arc<Mutex<ProcessOutputChannel>>>,
    fallback: Option<std::sync::Arc<Mutex<ProcessOutputChannel>>>,
    target: u8,
    present_at_entry: bool,
    failed: bool,
}

impl std::io::Write for ProcessRelayWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.failed || !self.present_at_entry {
            self.failed = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "process output descriptor was closed at entry",
            ));
        }
        if outward_write_interrupted() || outward_write_aborted() {
            if let Some(channel) = &self.channel {
                channel
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .failed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            self.failed = self.fallback.is_none();
            return Err(std::io::Error::other("outward write cancelled"));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let primary_failed = self.channel.as_ref().is_none_or(|channel| {
            channel
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .failed
                .load(std::sync::atomic::Ordering::Acquire)
        });
        let count = bytes.len().min(OUTPUT_RELAY_PAYLOAD_BYTES);
        let mut packet = [0u8; OUTPUT_RELAY_PAYLOAD_BYTES + 1];
        packet[0] = self.target;
        packet[1..count + 1].copy_from_slice(&bytes[..count]);
        if !primary_failed {
            if let Some(channel) = &self.channel {
                match send_process_output_record(channel, &packet[..count + 1]) {
                    Ok(()) => return Ok(count),
                    Err(ProcessOutputRecordError::Cancelled(error)) => {
                        channel
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .failed
                            .store(true, std::sync::atomic::Ordering::Release);
                        self.failed = self.fallback.is_none();
                        return Err(error);
                    }
                    Err(ProcessOutputRecordError::Failed(error)) if self.fallback.is_none() => {
                        self.failed = true;
                        return Err(error);
                    }
                    Err(ProcessOutputRecordError::Failed(_)) => {}
                }
            }
        }
        let Some(fallback) = &self.fallback else {
            self.failed = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "process output relay is unavailable",
            ));
        };
        match send_process_output_record(fallback, &packet[..count + 1]) {
            Ok(()) => Ok(count),
            Err(ProcessOutputRecordError::Cancelled(error)) => Err(error),
            Err(ProcessOutputRecordError::Failed(error)) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let channel_failed = self.channel.as_ref().is_none_or(|channel| {
            channel
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .failed
                .load(std::sync::atomic::Ordering::Acquire)
        });
        let fallback_failed = self.fallback.as_ref().is_none_or(|channel| {
            channel
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .failed
                .load(std::sync::atomic::Ordering::Acquire)
        });
        if self.failed
            || (channel_failed && fallback_failed)
            || outward_write_aborted()
            || outward_write_interrupted()
        {
            Err(std::io::Error::other("process output relay failed"))
        } else {
            Ok(())
        }
    }
}

enum ProcessOutputRecordError {
    Cancelled(std::io::Error),
    Failed(std::io::Error),
}

fn send_process_output_record(
    channel: &std::sync::Arc<Mutex<ProcessOutputChannel>>,
    packet: &[u8],
) -> std::result::Result<(), ProcessOutputRecordError> {
    loop {
        if outward_write_interrupted() || outward_write_aborted() {
            return Err(ProcessOutputRecordError::Cancelled(std::io::Error::other(
                "outward write cancelled",
            )));
        }
        let channel = channel.lock().unwrap_or_else(|error| error.into_inner());
        if channel.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ProcessOutputRecordError::Failed(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "process output relay failed",
            )));
        }
        match channel.sender.send(packet) {
            Ok(written) if written == packet.len() => {
                channel
                    .used
                    .store(true, std::sync::atomic::Ordering::Release);
                return Ok(());
            }
            Ok(_) => {
                channel
                    .failed
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(ProcessOutputRecordError::Failed(std::io::Error::other(
                    "process output relay accepted a partial record",
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // Darwin reports a momentarily full datagram buffer as ENOBUFS
            // (raw 55, surfaced as `Uncategorized`), not `WouldBlock`.
            // Retry it like any other transient backpressure signal instead
            // of poisoning the channel; cancellation still wins at the top
            // of the loop.
            Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {}
            // A sustained provider flood can briefly exhaust Darwin mbuf
            // clusters, surfacing as ENOMEM on an otherwise healthy socket.
            // Treat it as transient backpressure like ENOBUFS: the sleeping
            // retry lets the relay child drain while cancellation still
            // wins at the top of the loop.
            Err(error) if error.raw_os_error() == Some(libc::ENOMEM) => {}
            Err(error) => {
                eprintln!(
                    "TEMP-DIAG-180 RELAY-POISON errno={:?} kind={:?}",
                    error.raw_os_error(),
                    error.kind()
                );
                channel
                    .failed
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(ProcessOutputRecordError::Failed(error));
            }
        }
        drop(channel);
        // One-millisecond backpressure quantum, matching the supervisor
        // tick: macOS default datagram buffers hold only a couple of
        // payloads, so a coarser sleep throttles floods to tens of KB/s
        // and megabyte capture limits never trip before timeouts.
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Retained process-entry stdout/stderr relay owner.
pub struct ProcessOutputRelay {
    primary: Option<ProcessOutputEndpoint>,
    stderr_fallback: Option<ProcessOutputEndpoint>,
    stdout_channel: Option<std::sync::Arc<Mutex<ProcessOutputChannel>>>,
    stderr_channel: Option<std::sync::Arc<Mutex<ProcessOutputChannel>>>,
    stderr_fallback_channel: Option<std::sync::Arc<Mutex<ProcessOutputChannel>>>,
    stdout_present: bool,
    stderr_present: bool,
}

/// Terminal state of the process-entry output boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOutputFinish {
    /// Every requested record drained and every helper was reaped.
    Complete,
    /// A sink/relay rejected output, but helper cleanup was proven complete.
    OutputFailed,
    /// A relay or watchdog could not be proven stopped and reaped.
    CleanupIncomplete,
}

/// Apply process-cleanup precedence without allowing a pre-existing command
/// failure or cancellation code to hide incomplete relay teardown.
pub fn process_output_status(code: i32, finish: ProcessOutputFinish) -> i32 {
    match finish {
        ProcessOutputFinish::CleanupIncomplete => {
            // TEMP-DIAG-180: remove with the 125 fix.
            eprintln!("TEMP-DIAG-180: process_output_status: relay finish overrode code={code}");
            CLEANUP_INCOMPLETE_STATUS
        }
        ProcessOutputFinish::OutputFailed if code == 0 => 1,
        ProcessOutputFinish::Complete | ProcessOutputFinish::OutputFailed => code,
    }
}

struct ProcessOutputEndpoint {
    channel: Option<std::sync::Arc<Mutex<ProcessOutputChannel>>>,
    lifetime_writer: Option<std::os::unix::net::UnixStream>,
    child: Option<libc::pid_t>,
    registration: Option<StatusChildRegistration>,
}

fn open_process_descriptors() -> std::io::Result<Vec<i32>> {
    let directory = if Path::new("/proc/self/fd").is_dir() {
        Path::new("/proc/self/fd")
    } else {
        Path::new("/dev/fd")
    };
    let mut descriptors = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(descriptor) = name.parse::<i32>() {
            descriptors.push(descriptor);
        }
    }
    Ok(descriptors)
}

fn close_process_descriptors_except(descriptors: &[i32], retained: &[i32]) {
    for descriptor in descriptors {
        if *descriptor >= 3 && !retained.contains(descriptor) {
            unsafe { libc::close(*descriptor) };
        }
    }
}

fn snapshot_process_output(
    fd: i32,
    present_at_entry: bool,
) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd as _;

    if !present_at_entry {
        return Ok(None);
    }
    // F_DUPFD_CLOEXEC both snapshots the exact open file description and
    // guarantees the retained descriptor cannot alias 0, 1, or 2. EBADF is
    // the exec-boundary "closed" state and remains permanent for this writer
    // even if unrelated runtime opens later reuse the numeric descriptor.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate >= 0 {
        // SAFETY: fcntl returned a new uniquely owned descriptor.
        return Ok(Some(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(duplicate)
        }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBADF) {
        Ok(None)
    } else {
        Err(error)
    }
}

fn output_relay_child(
    receiver: std::os::unix::net::UnixDatagram,
    _expected_parent: libc::pid_t,
    stdout: Option<std::os::fd::OwnedFd>,
    stderr: Option<std::os::fd::OwnedFd>,
    relay_ready_writer: std::os::unix::net::UnixStream,
) -> ! {
    use std::os::fd::AsRawFd as _;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
            || libc::getppid() != _expected_parent
        {
            libc::_exit(125);
        }
    }
    unsafe {
        libc::close(libc::STDIN_FILENO);
        for signal in HANDLED_SIGNALS {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                libc::_exit(125);
            }
        }
        libc::close(libc::STDOUT_FILENO);
        libc::close(libc::STDERR_FILENO);
    }
    // Readiness is the authorization boundary for the parent. Publish it only
    // after every fallible child-side lifecycle operation has succeeded; an
    // early marker could otherwise expose an endpoint whose relay dies before
    // it can receive even one record.
    let ready = [1u8; 1];
    if unsafe {
        libc::write(
            relay_ready_writer.as_raw_fd(),
            ready.as_ptr().cast(),
            ready.len(),
        )
    } != 1
    {
        unsafe { libc::_exit(125) };
    }
    drop(relay_ready_writer);
    let mut packet = [0u8; OUTPUT_RELAY_PAYLOAD_BYTES + 1];
    loop {
        let received = unsafe {
            libc::recv(
                receiver.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                0,
            )
        };
        if received < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            unsafe { libc::_exit(1) };
        }
        if received == 1 && packet[0] == OUTPUT_RELAY_FINISH {
            unsafe { libc::_exit(0) };
        }
        if received <= 1 || !matches!(packet[0], 1 | 2) {
            unsafe { libc::_exit(1) };
        }
        let target = match packet[0] {
            1 => stdout.as_ref(),
            2 => stderr.as_ref(),
            _ => None,
        };
        let Some(target) = target else {
            unsafe { libc::_exit(1) };
        };
        let target = target.as_raw_fd();
        let mut offset = 1usize;
        let end = received as usize;
        while offset < end {
            let written =
                unsafe { libc::write(target, packet[offset..end].as_ptr().cast(), end - offset) };
            if written > 0 {
                offset += written as usize;
                continue;
            }
            if written < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            unsafe { libc::_exit(1) };
        }
    }
}

fn output_relay_watchdog(
    relay: libc::pid_t,
    lifetime_reader: std::os::unix::net::UnixStream,
    relay_ready_reader: std::os::unix::net::UnixStream,
) -> ! {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    unsafe {
        libc::close(libc::STDIN_FILENO);
        libc::close(libc::STDOUT_FILENO);
        libc::close(libc::STDERR_FILENO);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            libc::_exit(125);
        }
        for handled in HANDLED_SIGNALS {
            action.sa_sigaction = libc::SIG_IGN;
            if libc::sigaction(handled, &action, std::ptr::null_mut()) != 0 {
                libc::_exit(125);
            }
        }
    }
    if lifetime_reader.set_nonblocking(true).is_err()
        || relay_ready_reader.set_nonblocking(true).is_err()
    {
        unsafe { libc::_exit(125) };
    }
    let mut lifetime_reader = lifetime_reader;
    let mut relay_ready_reader = relay_ready_reader;
    let startup_deadline = cleanup_deadline();
    loop {
        let mut ready = [0u8; 1];
        match relay_ready_reader.read(&mut ready) {
            Ok(1) if ready[0] == 1 => break,
            Ok(0) | Ok(_) => {
                unsafe { libc::kill(relay, libc::SIGKILL) };
                unsafe { libc::_exit(125) };
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                unsafe { libc::kill(relay, libc::SIGKILL) };
                unsafe { libc::_exit(125) };
            }
        }
        let mut parent = [0u8; 1];
        match lifetime_reader.read(&mut parent) {
            Ok(0) | Ok(_) => {
                unsafe { libc::kill(relay, libc::SIGKILL) };
                unsafe { libc::_exit(125) };
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                unsafe { libc::kill(relay, libc::SIGKILL) };
                unsafe { libc::_exit(125) };
            }
        }
        let mut status = 0;
        let waited = unsafe { libc::waitpid(relay, &mut status, libc::WNOHANG) };
        if waited != 0 || Instant::now() >= startup_deadline {
            unsafe { libc::kill(relay, libc::SIGKILL) };
            unsafe { libc::_exit(125) };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(relay_ready_reader);
    let ready = [1u8; 1];
    if unsafe {
        libc::write(
            lifetime_reader.as_raw_fd(),
            ready.as_ptr().cast(),
            ready.len(),
        )
    } != 1
    {
        unsafe { libc::kill(relay, libc::SIGKILL) };
        unsafe { libc::_exit(125) };
    }
    let mut parent_gone_at = None;
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(relay, &mut status, libc::WNOHANG) };
        if waited == relay {
            let success = parent_gone_at.is_none()
                && libc::WIFEXITED(status)
                && libc::WEXITSTATUS(status) == 0;
            unsafe { libc::_exit(if success { 0 } else { 1 }) };
        }
        if waited < 0 {
            // EINTR is not a relay failure: a signal between the
            // waitpid and its result must retry, not abandon a
            // recv-blocked relay holding stdio fds.
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe { libc::_exit(1) };
        }
        let mut byte = [0u8; 1];
        match lifetime_reader.read(&mut byte) {
            Ok(0) | Ok(_) => {
                parent_gone_at.get_or_insert_with(Instant::now);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                parent_gone_at.get_or_insert_with(Instant::now);
            }
        }
        if let Some(started) = parent_gone_at {
            unsafe { libc::kill(relay, libc::SIGKILL) };
            if started.elapsed() >= Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS)
            {
                unsafe { libc::_exit(125) };
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_output_relay_watchdog(pid: libc::pid_t, deadline: Instant) -> std::io::Result<Option<i32>> {
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return Ok(Some(status));
        }
        if waited < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10).min(deadline.duration_since(now)));
    }
}

/// Stop one retained relay-watchdog session without ever signalling its raw
/// process-group ID after the leader has been reaped. The first wait is
/// optional so normal Drop can let the watchdog observe lifetime EOF, while
/// startup failures can revoke the whole group immediately.
fn stop_output_relay_watchdog(pid: libc::pid_t, allow_grace: bool) -> bool {
    let first_deadline = if allow_grace {
        cleanup_deadline()
    } else {
        Instant::now()
    };
    match wait_output_relay_watchdog(pid, first_deadline) {
        Ok(Some(_)) => return true,
        Ok(None) => {}
        // ECHILD means an inherited auto-reap policy already released this
        // PID. It is no longer safe to signal the numeric process group.
        Err(_) => return false,
    }
    // The preceding nonblocking wait proved the retained child was not reaped,
    // so its session/process-group ID cannot yet have been reused.
    let _ = signal_group_result(pid as u32, libc::SIGKILL);
    match wait_output_relay_watchdog(pid, cleanup_deadline()) {
        Ok(Some(_)) => true,
        Ok(None) | Err(_) => false,
    }
}

impl ProcessOutputEndpoint {
    fn start(
        stdout: Option<std::os::fd::OwnedFd>,
        stderr: Option<std::os::fd::OwnedFd>,
    ) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;

        let (sender, receiver) = internal_datagram_pair()?;
        let (lifetime_reader, lifetime_writer) = internal_stream_pair()?;
        let (relay_ready_reader, relay_ready_writer) = internal_stream_pair()?;
        sender.set_nonblocking(true)?;
        lifetime_writer.set_nonblocking(true)?;
        let open_descriptors = open_process_descriptors()?;
        let launch = StatusChildLaunch::begin();
        let mut prior_sigchld: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut default_sigchld: libc::sigaction = unsafe { std::mem::zeroed() };
        default_sigchld.sa_sigaction = libc::SIG_DFL;
        unsafe { libc::sigemptyset(&mut default_sigchld.sa_mask) };
        if unsafe { libc::sigaction(libc::SIGCHLD, &default_sigchld, &mut prior_sigchld) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pid = unsafe { libc::fork() };
        let fork_error = (pid < 0).then(std::io::Error::last_os_error);
        if pid == 0 {
            drop(sender);
            drop(lifetime_writer);
            // Keep both helper processes outside the caller's foreground
            // process group.  The retained watchdog PID then safely anchors
            // a final group KILL on every supported Unix target.
            if unsafe { libc::setsid() } < 0 {
                unsafe { libc::_exit(125) };
            }
            let relay_parent = unsafe { libc::getpid() };
            let relay = unsafe { libc::fork() };
            if relay == 0 {
                drop(lifetime_reader);
                drop(relay_ready_reader);
                let retained = [
                    receiver.as_raw_fd(),
                    stdout.as_ref().map_or(-1, std::os::fd::AsRawFd::as_raw_fd),
                    stderr.as_ref().map_or(-1, std::os::fd::AsRawFd::as_raw_fd),
                    relay_ready_writer.as_raw_fd(),
                ];
                close_process_descriptors_except(&open_descriptors, &retained);
                output_relay_child(receiver, relay_parent, stdout, stderr, relay_ready_writer);
            }
            if relay < 0 {
                unsafe { libc::_exit(125) };
            }
            drop(receiver);
            drop(stdout);
            drop(stderr);
            drop(relay_ready_writer);
            let retained = [lifetime_reader.as_raw_fd(), relay_ready_reader.as_raw_fd()];
            close_process_descriptors_except(&open_descriptors, &retained);
            output_relay_watchdog(relay, lifetime_reader, relay_ready_reader);
        }
        let registration = (pid > 0).then(|| StatusChildRegistration::new(pid as u32));
        drop(stdout);
        drop(stderr);
        let restore_result =
            unsafe { libc::sigaction(libc::SIGCHLD, &prior_sigchld, std::ptr::null_mut()) };
        let restore_error = (restore_result != 0).then(std::io::Error::last_os_error);
        drop(receiver);
        drop(lifetime_reader);
        drop(relay_ready_reader);
        drop(relay_ready_writer);
        drop(launch);
        if let Some(error) = fork_error {
            return Err(error);
        }
        if let Some(error) = restore_error {
            drop(lifetime_writer);
            let _ = stop_output_relay_watchdog(pid, false);
            return Err(error);
        }
        let startup_deadline = cleanup_deadline();
        let mut lifetime_writer = lifetime_writer;
        let mut ready = [0u8; 1];
        loop {
            match std::io::Read::read(&mut lifetime_writer, &mut ready) {
                Ok(1) if ready[0] == 1 => break,
                Ok(0) | Ok(_) => {
                    drop(lifetime_writer);
                    let _ = stop_output_relay_watchdog(pid, false);
                    return Err(std::io::Error::other(
                        "process output relay failed before authorization",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    drop(lifetime_writer);
                    let _ = stop_output_relay_watchdog(pid, false);
                    return Err(error);
                }
            }
            let waited = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if waited == pid {
                drop(lifetime_writer);
                return Err(std::io::Error::other(
                    "process output relay exited before authorization",
                ));
            }
            if waited < 0 {
                drop(lifetime_writer);
                return Err(std::io::Error::last_os_error());
            }
            if Instant::now() >= startup_deadline {
                drop(lifetime_writer);
                let _ = stop_output_relay_watchdog(pid, false);
                return Err(std::io::Error::other(
                    "process output relay authorization timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let channel = std::sync::Arc::new(Mutex::new(ProcessOutputChannel {
            sender,
            failed: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
        }));
        Ok(Self {
            channel: Some(channel),
            lifetime_writer: Some(lifetime_writer),
            child: Some(pid),
            registration,
        })
    }

    fn finish(&mut self, drain: bool) -> ProcessOutputFinish {
        let deadline = cleanup_deadline();
        let channel_failed = self.channel.as_ref().is_some_and(|channel| {
            channel
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .failed
                .load(std::sync::atomic::Ordering::Acquire)
        });
        let mut finish_sent = false;
        if drain && !channel_failed {
            if let Some(channel) = &self.channel {
                loop {
                    match channel
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .sender
                        .send(&[OUTPUT_RELAY_FINISH])
                    {
                        Ok(1) => {
                            finish_sent = true;
                            break;
                        }
                        Ok(_) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            }
        }
        self.channel.take();
        let Some(pid) = self.child.take() else {
            self.registration.take();
            return if channel_failed {
                ProcessOutputFinish::OutputFailed
            } else {
                ProcessOutputFinish::Complete
            };
        };
        let mut status = 0;
        if !drain || channel_failed || !finish_sent {
            // EOF is the portable parent-lifetime signal. The watchdog, not
            // this process, owns the possibly blocked relay and reaps it.
            self.lifetime_writer.take();
        }
        while Instant::now() < deadline {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if waited == pid {
                self.registration.take();
                return if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) == 125 {
                    // TEMP-DIAG-180: remove with the 125 fix.
                    eprintln!(
                        "TEMP-DIAG-180: relay finish: child status={status} drain={drain} channel_failed={channel_failed} finish_sent={finish_sent}"
                    );
                    ProcessOutputFinish::CleanupIncomplete
                } else if drain
                    && (!channel_failed && finish_sent && libc::WEXITSTATUS(status) == 0)
                {
                    ProcessOutputFinish::Complete
                } else if !drain && libc::WEXITSTATUS(status) == 1 {
                    // Exit 1 is the watchdog's successful forced-teardown
                    // report: it observed owner EOF, killed the blocked relay,
                    // and reaped it. Queued output was intentionally discarded.
                    ProcessOutputFinish::Complete
                } else {
                    ProcessOutputFinish::OutputFailed
                };
            }
            if waited < 0 {
                self.registration.take();
                // TEMP-DIAG-180: remove with the 125 fix.
                eprintln!("TEMP-DIAG-180: relay finish: waitpid error, drain={drain}");
                return ProcessOutputFinish::CleanupIncomplete;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.lifetime_writer.take();
        let reaped = stop_output_relay_watchdog(pid, true);
        self.registration.take();
        // TEMP-DIAG-180: remove with the 125 fix.
        eprintln!(
            "TEMP-DIAG-180: relay finish: deadline expired with relay child unreaped, drain={drain} reaped={reaped}"
        );
        // A relay child that never drains is usually blocked on an
        // undrained sink, not a leaked descendant: the watchdog kill above
        // reaps it, so only genuinely unreaped helpers fail closed. Lost
        // output preserves the command's own status instead of raising 125.
        if reaped {
            ProcessOutputFinish::OutputFailed
        } else {
            ProcessOutputFinish::CleanupIncomplete
        }
    }
}

impl Drop for ProcessOutputEndpoint {
    fn drop(&mut self) {
        self.channel.take();
        self.lifetime_writer.take();
        if let Some(pid) = self.child.take() {
            let _ = stop_output_relay_watchdog(pid, true);
        }
        self.registration.take();
    }
}

impl ProcessOutputRelay {
    /// Capture the exec-boundary descriptor bitmap and start a serialized
    /// primary stdout/stderr relay. Stderr also has an independent dormant
    /// fallback so a blocked stdout cannot suppress the bounded timeout or
    /// cancellation diagnostic that explains the terminal status.
    pub fn start() -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;

        // Take the initializer's address so the linker keeps its object
        // in every binary that starts a relay (see above).
        std::hint::black_box(&CAPTURE_ENTRY_STDIO);
        let entry_mask = ENTRY_STDIO_MASK.load(std::sync::atomic::Ordering::Relaxed);
        if entry_mask & ENTRY_STDIO_INITIALIZED == 0 {
            return Err(std::io::Error::other(
                "process stdio was not captured before runtime initialization",
            ));
        }
        let stdout = snapshot_process_output(
            libc::STDOUT_FILENO,
            entry_mask & (1 << libc::STDOUT_FILENO) != 0,
        )?;
        let stderr = snapshot_process_output(
            libc::STDERR_FILENO,
            entry_mask & (1 << libc::STDERR_FILENO) != 0,
        )?;
        let stdout_present = stdout.is_some();
        let stderr_present = stderr.is_some();
        let fallback_stderr = match stderr.as_ref() {
            Some(stderr) => snapshot_process_output(stderr.as_raw_fd(), true)?,
            None => None,
        };
        let primary = if stdout_present || stderr_present {
            Some(ProcessOutputEndpoint::start(stdout, stderr)?)
        } else {
            None
        };
        let stderr_fallback = match fallback_stderr {
            Some(stderr) => match ProcessOutputEndpoint::start(None, Some(stderr)) {
                Ok(endpoint) => Some(endpoint),
                Err(error) => {
                    drop(primary);
                    return Err(error);
                }
            },
            None => None,
        };
        let stdout_channel = primary
            .as_ref()
            .and_then(|endpoint| endpoint.channel.clone());
        let stderr_channel = stdout_channel.clone();
        let stderr_fallback_channel = stderr_fallback
            .as_ref()
            .and_then(|endpoint| endpoint.channel.clone());
        Ok(Self {
            primary,
            stderr_fallback,
            stdout_channel,
            stderr_channel,
            stderr_fallback_channel,
            stdout_present,
            stderr_present,
        })
    }

    /// Return the writer bound to stdout's entry-time open file description.
    pub fn stdout(&self) -> ProcessRelayWriter {
        ProcessRelayWriter {
            channel: self.stdout_channel.clone(),
            fallback: None,
            target: libc::STDOUT_FILENO as u8,
            present_at_entry: self.stdout_present,
            failed: false,
        }
    }

    /// Return the writer bound to stderr's entry-time open file description.
    pub fn stderr(&self) -> ProcessRelayWriter {
        ProcessRelayWriter {
            channel: self.stderr_channel.clone(),
            fallback: self.stderr_fallback_channel.clone(),
            target: libc::STDERR_FILENO as u8,
            present_at_entry: self.stderr_present,
            failed: false,
        }
    }

    /// Finish queued output without allowing an external sink to hold process
    /// termination beyond the fixed cleanup grace.
    pub fn finish(mut self, drain: bool) -> ProcessOutputFinish {
        let fallback_used = self
            .stderr_fallback_channel
            .as_ref()
            .is_some_and(|channel| {
                channel
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .used
                    .load(std::sync::atomic::Ordering::Acquire)
            });
        self.stdout_channel.take();
        self.stderr_channel.take();
        self.stderr_fallback_channel.take();
        let primary_complete = self
            .primary
            .as_mut()
            .map_or(ProcessOutputFinish::Complete, |endpoint| {
                endpoint.finish(drain)
            });
        let fallback_complete = self
            .stderr_fallback
            .as_mut()
            .map_or(ProcessOutputFinish::Complete, |endpoint| {
                endpoint.finish(fallback_used)
            });
        match (primary_complete, fallback_complete) {
            (ProcessOutputFinish::CleanupIncomplete, _)
            | (_, ProcessOutputFinish::CleanupIncomplete) => ProcessOutputFinish::CleanupIncomplete,
            (ProcessOutputFinish::OutputFailed, _) | (_, ProcessOutputFinish::OutputFailed) => {
                ProcessOutputFinish::OutputFailed
            }
            (ProcessOutputFinish::Complete, ProcessOutputFinish::Complete) => {
                ProcessOutputFinish::Complete
            }
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Start a child session before exec; all work in this post-fork closure is
/// async-signal-safe. No allocation or Rust runtime operation runs there.
pub(crate) fn isolate(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: setsid has no memory arguments and is safe between fork/exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Per-thread launch barrier. A handled signal delivered to the spawning
/// thread remains pending until the child session has both exec'd and been
/// registered as owned. No process-global mutex is held, so parallel suite
/// launches remain concurrent.
struct BlockedLaunchSignals {
    previous: libc::sigset_t,
    active: bool,
}

impl BlockedLaunchSignals {
    fn install() -> std::io::Result<Self> {
        // SAFETY: both sets are initialized before use and pthread_sigmask
        // writes only the supplied previous-mask object.
        unsafe {
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            for signal in HANDLED_SIGNALS {
                libc::sigaddset(&mut blocked, signal);
            }
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            Ok(Self {
                previous,
                active: true,
            })
        }
    }

    fn previous(&self) -> libc::sigset_t {
        // SAFETY: sigset_t is plain initialized C storage.
        unsafe { std::ptr::read(&self.previous) }
    }

    fn restore(&mut self) -> std::io::Result<()> {
        // SAFETY: `previous` was initialized by pthread_sigmask above.
        unsafe {
            if !self.active {
                return Ok(());
            }
            let error =
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            self.active = false;
            Ok(())
        }
    }

    fn keep_blocked(mut self) {
        self.active = false;
    }
}

impl Drop for BlockedLaunchSignals {
    fn drop(&mut self) {
        // SAFETY: restoring a saved mask is always valid. Drop cannot surface
        // an error, but explicit successful launches use `restore` above.
        unsafe {
            if self.active {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
                self.active = false;
            }
        }
    }
}

/// Restore default cancellation dispositions in a forked child while those
/// signals are still blocked. This prevents a pending child-directed signal
/// from running Dot's inherited capture handler and then disappearing at exec.
unsafe fn reset_child_signal_dispositions() -> std::io::Result<()> {
    for signal in HANDLED_SIGNALS {
        // SAFETY: zero initialization is valid for sigaction and every pointer
        // passed to libc names initialized local storage.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // Preserve the caller's exec-visible SIGCHLD policy for the actual target
    // while internal supervisor/helper children always run with SIG_DFL.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = if TARGET_SIGCHLD_IGNORED.load(std::sync::atomic::Ordering::SeqCst) {
        libc::SIG_IGN
    } else {
        libc::SIG_DFL
    };
    action.sa_flags = TARGET_SIGCHLD_FLAGS.load(std::sync::atomic::Ordering::SeqCst);
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Adopt orphaned suite descendants so their lifecycle never depends on an
/// outer init process reaping promptly. Other Unix platforms reap through
/// their init process because Linux's subreaper facility is not available.
pub(crate) fn adopt_descendants() -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: prctl has no pointer arguments for PR_SET_CHILD_SUBREAPER.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Observe a child's exit without releasing its PID/session identity. A
/// retained zombie leader prevents reuse throughout descendant teardown.
pub(crate) fn exited(child: &Child) -> std::io::Result<bool> {
    // SAFETY: siginfo is initialized; waitid writes only this local value.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(info.si_pid() != 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessInfo {
    pid: u32,
    parent: u32,
    group: u32,
    session: u32,
    live: bool,
    identity: ProcessIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ProcessIdentity {
    pid: u32,
    /// Linux and Android expose the kernel start tick in procfs. Portable
    /// snapshots intentionally leave this absent because they never authorize
    /// a process-directed signal or wait from a numeric PID.
    start: Option<u64>,
}

/// Snapshot all live processes once per polling pass, so a busy first session
/// cannot consume the discovery budget allocated to later workers.
fn process_snapshot(deadline: Instant) -> Option<Vec<ProcessInfo>> {
    #[cfg(test)]
    GLOBAL_PROCESS_SNAPSHOT_CALLS.with(|calls| calls.set(calls.get() + 1));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        #[cfg(test)]
        let force_portable =
            FORCE_PROC_SNAPSHOT_UNAVAILABLE.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(test))]
        let force_portable = false;
        if !force_portable {
            if let Some(processes) = proc_process_snapshot(Path::new("/proc"), deadline) {
                return Some(processes);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(processes) = macos_native_snapshot(deadline) {
            return Some(processes);
        }
    }
    // This is an OS process-table interface, not a caller-selected tool.
    #[cfg(target_os = "android")]
    let mut command = Command::new("/system/bin/ps");
    #[cfg(not(target_os = "android"))]
    let mut command = Command::new("/bin/ps");
    // macOS ps rejects the Linux group/session keywords: it prints the
    // columns it knows and exits nonzero, so request only the portable
    // columns there and resolve group/session per PID below.
    #[cfg(target_os = "macos")]
    command.args(["-A", "-o", "pid=,ppid=,stat="]);
    #[cfg(not(target_os = "macos"))]
    command.args(["-A", "-o", "pid=,ppid=,pgid=,sid=,stat="]);
    let bytes = snapshot(command, deadline)?;
    #[cfg(target_os = "macos")]
    let processes = match parse_macos_ps_snapshot(&bytes) {
        Some(processes) => processes,
        // TEMP-DIAG-180: remove with the recvmsg diag.
        None => {
            eprintln!(
                "TEMP-DIAG-180: snapshot: macOS ps parse failed ({} bytes)",
                bytes.len()
            );
            return None;
        }
    };
    #[cfg(not(target_os = "macos"))]
    let processes = parse_ps_snapshot(&bytes)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // `ps` supplies an atomic topology row but no start generation. Read
        // each still-present row back through procfs before granting any
        // process-directed authority; the internal `ps` helper has already
        // been reaped and is therefore skipped rather than misattributed.
        let mut validated = Vec::with_capacity(processes.len());
        for observed in processes {
            #[cfg(test)]
            if FORCE_FALLBACK_PROCESS_INFO_UNAVAILABLE.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            match linux_process_info_result(observed.pid) {
                Ok(Some(current)) => validated.push(current),
                // A process that vanished after its complete ps row is
                // unrelated churn, not a partial snapshot.
                Ok(None) => {}
                // Permission, parse, and other query failures leave a listed
                // identity unaccounted for and invalidate the whole pass.
                Err(_) => return None,
            }
        }
        Some(validated)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Some(processes)
}

/// Resolve the group/session pair for one macOS `ps` row.
///
/// The supervisor deliberately keeps the leader unreaped until the empty
/// proof completes, so every macOS observation races a zombie leader:
/// Darwin answers ESRCH for getpgid/getsid once the process has exited
/// even though `ps` still lists it. A zombie (`Z...` state) therefore
/// keeps its row with a self-referential pair: the row is definitively
/// dead, so the pair preserves the leader-observed proof without
/// authorizing delivery to any live group (delivery, survivor checks,
/// and direct-authority escalation only consult live rows). A
/// non-zombie row that vanished between the listing and the query is
/// unrelated churn (`Ok(None)`); any other query failure fails the
/// whole snapshot closed (`Err`).
#[cfg(any(test, target_os = "macos"))]
fn macos_row_membership(pid: u32, zombie: bool) -> std::io::Result<Option<(u32, u32)>> {
    // SAFETY: getpgid/getsid take a PID and no pointers.
    let group = unsafe { libc::getpgid(pid as libc::pid_t) };
    let group_errno = if group < 0 {
        std::io::Error::last_os_error().raw_os_error()
    } else {
        None
    };
    let session = unsafe { libc::getsid(pid as libc::pid_t) };
    let session_errno = if session < 0 {
        std::io::Error::last_os_error().raw_os_error()
    } else {
        None
    };
    match (group_errno, session_errno) {
        (None, None) => Ok(Some((group as u32, session as u32))),
        _ if group_errno.is_none_or(|errno| errno == libc::ESRCH)
            && session_errno.is_none_or(|errno| errno == libc::ESRCH) =>
        {
            if zombie {
                Ok(Some((pid, pid)))
            } else {
                Ok(None)
            }
        }
        _ => Err(std::io::Error::other("macOS session query failed")),
    }
}

/// Snapshot the macOS process table without spawning `ps`.
///
/// A fork+exec costs ~200ms on loaded macOS runners and teardown takes
/// several snapshots per stop, so spawn latency burns the verification
/// budget and reports "could not verify". `proc_listpids` enumerates the
/// table in one syscall and each row resolves through `proc_pidinfo` plus
/// `getsid`, matching the `ps` fallback row-for-row. Any failure returns
/// None and the caller falls back to `ps`.
#[cfg(target_os = "macos")]
fn macos_native_snapshot(deadline: Instant) -> Option<Vec<ProcessInfo>> {
    // `PROC_ALL_PIDS` is stable libproc ABI (1) but absent from libc 0.2.
    const PROC_ALL_PIDS: u32 = 1;
    let mut capacity: usize = 4096;
    let mut pids: Vec<i32> = Vec::new();
    let mut complete = false;
    // A full buffer may mean truncation (processes fork concurrently), so
    // grow boundedly until a fetch leaves room, then fall back to `ps`.
    for _ in 0..4 {
        if Instant::now() >= deadline {
            return None;
        }
        pids.resize(capacity, 0);
        // SAFETY: pids owns capacity pid_t slots; PROC_ALL_PIDS lists all.
        let written = unsafe {
            libc::proc_listpids(
                PROC_ALL_PIDS,
                0,
                pids.as_mut_ptr().cast(),
                (capacity * std::mem::size_of::<i32>()) as libc::c_int,
            )
        };
        if written < 0 {
            return None;
        }
        let count = (written as usize) / std::mem::size_of::<i32>();
        if count < capacity {
            pids.truncate(count);
            complete = true;
            break;
        }
        capacity = capacity.saturating_mul(2);
    }
    if !complete {
        return None;
    }
    let mut processes = Vec::with_capacity(pids.len());
    for pid in pids {
        if Instant::now() >= deadline {
            return None;
        }
        let Ok(pid) = u32::try_from(pid) else {
            continue;
        };
        // PID 0 (the kernel scheduler) is skipped: querying it would
        // alias the caller.
        if pid == 0 {
            continue;
        }
        match macos_native_process_info(pid) {
            Ok(Some(process)) => processes.push(process),
            Ok(None) => {}
            Err(()) => return None,
        }
    }
    Some(processes)
}

/// Resolve one macOS snapshot row through `proc_pidinfo`.
///
/// Returns `Ok(None)` for a row that exited mid-query (unrelated churn),
/// `Err` for any other query failure (fails the whole snapshot closed,
/// matching the `ps` fallback), and the row otherwise. The `getsid`
/// lookup is bracketed with two matching kernel records so PID reuse or
/// a concurrent setsid cannot splice topology from different generations
/// into one row. Zombies keep the self-referential pair the `ps`
/// fallback uses since they are definitively dead; an unreadable zombie
/// reports parent zero (unknown) since only its presence is observable.
#[cfg(target_os = "macos")]
fn macos_native_process_info(pid: u32) -> std::result::Result<Option<ProcessInfo>, ()> {
    let before = match macos_bsd_info(pid) {
        Ok(BsdOutcome::Record(info)) => info,
        Ok(BsdOutcome::Gone) => return Ok(None),
        Ok(BsdOutcome::Zombie) => {
            return Ok(Some(ProcessInfo {
                pid,
                parent: 0,
                group: pid,
                session: pid,
                live: false,
                identity: ProcessIdentity { pid, start: None },
            }));
        }
        Err(()) => return Err(()),
    };
    if before.pbi_status == libc::SZOMB {
        return Ok(Some(ProcessInfo {
            pid,
            parent: before.pbi_ppid,
            group: pid,
            session: pid,
            live: false,
            identity: ProcessIdentity { pid, start: None },
        }));
    }
    // SAFETY: getsid takes a PID and no pointers.
    let session = unsafe { libc::getsid(pid as libc::pid_t) };
    if session < 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(());
    }
    let after = match macos_bsd_info(pid) {
        Ok(BsdOutcome::Record(info)) => info,
        // The row died between the two records: report the zombie with
        // the parent observed while it was still readable.
        Ok(BsdOutcome::Gone) | Ok(BsdOutcome::Zombie) => {
            return Ok(Some(ProcessInfo {
                pid,
                parent: before.pbi_ppid,
                group: pid,
                session: pid,
                live: false,
                identity: ProcessIdentity { pid, start: None },
            }));
        }
        Err(()) => return Err(()),
    };
    // A generation change between the two records means the PID was
    // reused mid-query; skip the row rather than splicing generations.
    // Liveness (live versus zombie) is compared but the scheduling
    // status is not: a busy writer flaps between running and sleeping
    // across two back-to-back reads, which would drop live rows
    // (notably the provider during teardown), while a row that died
    // mid-query must never report as live.
    let before_live = before.pbi_status != libc::SZOMB;
    let after_live = after.pbi_status != libc::SZOMB;
    if before.pbi_pid != after.pbi_pid
        || before_live != after_live
        || before.pbi_ppid != after.pbi_ppid
        || before.pbi_pgid != after.pbi_pgid
        || before.pbi_start_tvsec != after.pbi_start_tvsec
        || before.pbi_start_tvusec != after.pbi_start_tvusec
    {
        return Ok(None);
    }
    Ok(Some(ProcessInfo {
        pid,
        parent: before.pbi_ppid,
        group: before.pbi_pgid,
        session: session as u32,
        live: true,
        identity: ProcessIdentity { pid, start: None },
    }))
}

/// Outcome of one macOS process-record query.
#[cfg(target_os = "macos")]
enum BsdOutcome {
    /// A complete record for the requested PID.
    Record(libc::proc_bsdinfo),
    /// The PID is gone, reused, or unobservable: unrelated churn.
    Gone,
    /// The PID exists but `proc_pidinfo` cannot read it: an unreaped
    /// zombie, which the teardown proof must observe as present-but-dead.
    Zombie,
}

/// Read one macOS process record through `proc_pidinfo`.
///
/// `proc_pidinfo` cannot read zombies, so an `ESRCH` row is probed with
/// `kill(pid, 0)`: a present-but-unreadable PID is reported as a zombie
/// (matching the `ps` fallback, which lists zombies) while a fully gone
/// PID is churn. The probe re-reads once so an exit/refork race resolves
/// to the fresh live record instead of a zombie row for a live process.
/// Permission-denied rows stay skipped without probing: session members
/// are always same-user observable, so skipping cannot hide a member.
/// Any other query failure fails closed like the `ps` fallback.
#[cfg(target_os = "macos")]
fn macos_bsd_info(pid: u32) -> std::result::Result<BsdOutcome, ()> {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return Err(());
    };
    let Ok(size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return Err(());
    };
    match macos_bsd_read(pid, pid_i32, size) {
        Ok(info) => Ok(BsdOutcome::Record(info)),
        Err(error) if error == libc::ESRCH => {
            // SAFETY: positive PID and signal zero only probe existence.
            if unsafe { libc::kill(pid_i32, 0) } != 0 {
                return Ok(BsdOutcome::Gone);
            }
            match macos_bsd_read(pid, pid_i32, size) {
                Ok(info) => Ok(BsdOutcome::Record(info)),
                Err(error) if error == libc::ESRCH => Ok(BsdOutcome::Zombie),
                Err(error) if error == libc::EPERM || error == libc::EACCES => Ok(BsdOutcome::Gone),
                Err(_) => Err(()),
            }
        }
        Err(error) if error == libc::EPERM || error == libc::EACCES => Ok(BsdOutcome::Gone),
        Err(_) => Err(()),
    }
}

/// One `proc_pidinfo` attempt: a complete matching record, or the errno.
///
/// A complete record for another PID reports `ESRCH`: the slot was
/// reused mid-query, which the caller treats like an exited row.
#[cfg(target_os = "macos")]
fn macos_bsd_read(
    pid: u32,
    pid_i32: i32,
    size: i32,
) -> std::result::Result<libc::proc_bsdinfo, i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: info owns size writable bytes and this flavor has no
    // auxiliary argument.
    let written = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written == size && info.pbi_pid == pid {
        return Ok(info);
    }
    if written == size {
        return Err(libc::ESRCH);
    }
    Err(std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL))
}

/// Parse the macOS `ps` columns and resolve group/session per PID.
///
/// macOS `ps` has no numeric session/group keywords, so the snapshot
/// requests only PID/PPID/state and reads the rest through getpgid/getsid.
/// A PID that exits between the listing and the query is unrelated churn;
/// any other query failure fails the whole snapshot closed. PID 0 (the
/// kernel scheduler) is skipped: querying it would alias the caller.
#[cfg(target_os = "macos")]
fn parse_macos_ps_snapshot(bytes: &[u8]) -> Option<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for bytes in bytes.split(|byte| *byte == b'\n') {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line = std::str::from_utf8(bytes).ok()?;
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<u32>().ok()?;
        let parent = fields.next()?.parse::<u32>().ok()?;
        let state = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        if pid == 0 {
            continue;
        }
        let (group, session) = match macos_row_membership(pid, state.starts_with('Z')) {
            Ok(Some(pair)) => pair,
            Ok(None) => continue,
            Err(_) => return None,
        };
        processes.push(ProcessInfo {
            pid,
            parent,
            group,
            session,
            live: !state.starts_with('Z'),
            identity: ProcessIdentity { pid, start: None },
        });
    }
    Some(processes)
}

// The macOS snapshot path uses `parse_macos_ps_snapshot`; keep this parser
// for unit tests on every platform so malformed-row coverage still builds.
#[cfg(any(test, not(target_os = "macos")))]
fn parse_ps_snapshot(bytes: &[u8]) -> Option<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for bytes in bytes.split(|byte| *byte == b'\n') {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line = std::str::from_utf8(bytes).ok()?;
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<u32>().ok()?;
        let parent = fields.next()?.parse::<u32>().ok()?;
        let group = fields.next()?.parse::<u32>().ok()?;
        let session = fields.next()?.parse::<u32>().ok()?;
        let state = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        processes.push(ProcessInfo {
            pid,
            parent,
            group,
            session,
            live: !state.starts_with('Z'),
            identity: ProcessIdentity { pid, start: None },
        });
    }
    Some(processes)
}

/// Whether a procfs query failed because the process is already gone. A
/// numeric entry can exit between directory listing and stat read: ENOENT
/// means it was gone before open, ESRCH means it exited (and was reaped)
/// between open and read. Both are unrelated churn, not a partial view;
/// anything else stays fail-closed.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_process_vanished(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// Whether an unreadable procfs entry belongs to another app.
///
/// Android denies an app's reads of other apps' stat files, and app
/// processes cannot change UID (no setuid), so a permission-denied entry
/// is provably foreign to every owned session. Linux keeps denials
/// fail-closed instead: a setuid descendant's stat stays world-readable
/// there, so an unreadable stat indicates a genuinely partial view.
#[cfg(any(test, target_os = "android"))]
fn proc_process_foreign(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
static FORCE_FAKE_PROC_STAT_ESRCH_ROOT: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Exact stat path that must fail with permission denied. Mode-bit tricks
/// cannot simulate EACCES for root, so the seam injects the errno directly;
/// registration is path-exact so parallel tests never observe it.
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
static FORCE_FAKE_PROC_STAT_DENIED: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Read one procfs stat file. The test seams inject an exit race (ESRCH)
/// or a foreign-app denial (EACCES) only for registered fake paths, so
/// parallel tests using their own roots or the live process table never
/// observe them.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_proc_stat(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(test)]
    if !path.starts_with("/proc")
        && FORCE_FAKE_PROC_STAT_DENIED
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|denied| path == denied)
    {
        return Err(std::io::Error::from_raw_os_error(libc::EACCES));
    }
    #[cfg(test)]
    if !path.starts_with("/proc")
        && FORCE_FAKE_PROC_STAT_ESRCH_ROOT
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
    {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    std::fs::read(path)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_process_snapshot(root: &Path, deadline: Instant) -> Option<Vec<ProcessInfo>> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut processes = Vec::new();
    for entry in entries {
        if Instant::now() >= deadline {
            return None;
        }
        // An iterator error means the directory walk was incomplete. Falling
        // back to the fixed OS process-table command is safer than certifying
        // an empty session from a partial view.
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match read_proc_stat(&entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if proc_process_vanished(&error) => continue,
            // Android hides other apps' stat files behind permission errors;
            // those entries cannot be owned descendants, so skip them rather
            // than failing a snapshot that is complete for everything owned.
            #[cfg(target_os = "android")]
            Err(error) if proc_process_foreign(&error) => continue,
            Err(_) => return None,
        };
        processes.push(parse_proc_process(pid, &stat)?);
    }
    Some(processes)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn parse_proc_process(pid: u32, stat: &[u8]) -> Option<ProcessInfo> {
    let end = stat.windows(2).rposition(|part| part == b") ")?;
    let fields: Vec<_> = stat[end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect();
    let parse = |index: usize| {
        fields
            .get(index)
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<u32>().ok())
    };
    Some(ProcessInfo {
        pid,
        parent: parse(1)?,
        group: parse(2)?,
        session: parse(3)?,
        live: fields.first() != Some(&b"Z".as_slice()),
        identity: ProcessIdentity {
            pid,
            start: Some(
                fields
                    .get(19)
                    .and_then(|field| std::str::from_utf8(field).ok())
                    .and_then(|field| field.parse::<u64>().ok())?,
            ),
        },
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
enum PidFd {
    Open(std::os::fd::OwnedFd),
    Unsupported,
    Gone,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn stable_pidfd(state: PidFd) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    match state {
        PidFd::Open(pidfd) => Ok(Some(pidfd)),
        PidFd::Gone => Ok(None),
        PidFd::Unsupported => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe descendant delivery requires pidfd support",
        )),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_pidfd(pid: u32) -> std::io::Result<PidFd> {
    use std::os::fd::FromRawFd as _;

    #[cfg(test)]
    if FORCE_PIDFD_UNAVAILABLE.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(PidFd::Unsupported);
    }

    let Ok(pid) = i32::try_from(pid) else {
        return Ok(PidFd::Gone);
    };
    // SAFETY: pidfd_open takes a positive PID and flags=0. The returned
    // descriptor pins that exact identity across validation and delivery.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd >= 0 {
        // SAFETY: a successful syscall returns one newly owned descriptor.
        return Ok(PidFd::Open(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        }));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(PidFd::Gone),
        // Older kernels and Android sandboxes may not expose pidfds. Never
        // reopen the PID-reuse race with a raw process-directed fallback.
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EPERM) => Ok(PidFd::Unsupported),
        _ => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_info(pid: u32) -> Option<ProcessInfo> {
    linux_process_info_result(pid).ok().flatten()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_info_result(pid: u32) -> std::io::Result<Option<ProcessInfo>> {
    let path = format!("/proc/{pid}/stat");
    let stat = match read_proc_stat(Path::new(&path)) {
        Ok(stat) => stat,
        Err(error) if proc_process_vanished(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    parse_proc_process(pid, &stat)
        .map(Some)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid proc stat"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug)]
enum BoundaryMatch {
    Matches,
    Delegated,
    ForeignMarker,
    Absent,
    Unknown,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_boundary(pid: u32, expected: &str) -> BoundaryMatch {
    const MAX_ENVIRON_BYTES: u64 = 1024 * 1024;
    let Ok(file) = File::open(format!("/proc/{pid}/environ")) else {
        return BoundaryMatch::Unknown;
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_ENVIRON_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_ENVIRON_BYTES
    {
        return BoundaryMatch::Unknown;
    }
    let Some(tokens) = boundary_tokens(&bytes) else {
        return BoundaryMatch::Absent;
    };
    if let Some(index) = tokens
        .iter()
        .rposition(|token| token.as_slice() == expected.as_bytes())
    {
        if index + 1 == tokens.len() {
            BoundaryMatch::Matches
        } else {
            BoundaryMatch::Delegated
        }
    } else {
        BoundaryMatch::ForeignMarker
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn boundary_tokens(environ: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut prefix = SESSION_BOUNDARY_ENV.as_bytes().to_vec();
    prefix.push(b'=');
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_slice()))
        .map(|value| {
            value
                .split(|byte| *byte == b':')
                .map(<[u8]>::to_vec)
                .collect()
        })
}

/// Whether a process currently holds our session lease socket open. Only
/// file descriptors inherited through this session's spawn tree can match:
/// an unrelated process cannot forge the lease inode, and a descriptor the
/// candidate closed before the scan reads as absent. Any inspection failure
/// (exited candidate, unreadable fd table) fails closed toward no claim;
/// the post-teardown lease-closure check remains the backstop for genuine
/// orphans, and claiming still revalidates identity through a pidfd.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_holds_session_lease(pid: u32, lease: u64) -> bool {
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    let needle = format!("socket:[{lease}]");
    let needle = std::ffi::OsStr::new(&needle);
    for entry in dir.flatten() {
        if std::fs::read_link(entry.path()).is_ok_and(|target| target.as_os_str() == needle) {
            return true;
        }
    }
    false
}

#[cfg(any(target_os = "linux", target_os = "android"))]
struct OwnedMember {
    process: ProcessInfo,
    pidfd: std::os::fd::OwnedFd,
    signaled: bool,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl OwnedMember {
    fn claim(observed: &ProcessInfo) -> std::io::Result<Option<Self>> {
        let Some(pidfd) = stable_pidfd(open_pidfd(observed.pid)?)? else {
            return Ok(None);
        };
        // Read the identity again only after the pidfd exists. If the numeric
        // PID changed owners around open, either the start token or session no
        // longer matches and the pinned process is rejected without action.
        let Some(current) = linux_process_info(observed.pid) else {
            return Ok(None);
        };
        if current.identity != observed.identity || current.identity.start.is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            process: current,
            pidfd,
            signaled: false,
        }))
    }

    fn refresh(&mut self, observed: &ProcessInfo) {
        if self.process.identity == observed.identity {
            self.process = observed.clone();
        }
    }

    fn refresh_from_kernel(&mut self) {
        match linux_process_info(self.process.pid) {
            Some(current) if current.identity == self.process.identity => {
                self.process = current;
            }
            Some(_) => self.process.live = false,
            None => {
                // A failed procfs read is not proof of exit. The retained
                // pidfd can distinguish a gone identity from an unreadable
                // live (or zombie) one without reopening PID reuse.
                self.process.live = !matches!(self.signal(0), Ok(false));
            }
        }
    }

    fn signal(&self, signal: i32) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd as _;

        // SAFETY: the owned pidfd remains live for the syscall, siginfo is
        // null for ordinary delivery, and flags must be zero.
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } == 0
        {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

/// Capture a portable process snapshot without a blocking pipe reader thread.
fn snapshot(mut command: Command, deadline: Instant) -> Option<Vec<u8>> {
    // TEMP-DIAG-180: remove with the recvmsg diag. Identifies which
    // snapshot stage fails on macOS (every probe session currently burns
    // its full teardown budget and reports "could not verify").
    let entered = Instant::now();
    if entered >= deadline {
        eprintln!("TEMP-DIAG-180: snapshot: deadline already passed at entry");
        return None;
    }
    // No per-iteration "entry with..." print: the sudo-PTY test's helper
    // writes to a small macOS PTY buffer nobody drains, so hot-path
    // diagnostics flow-control the helper into a 15s timeout (Heisenbug).
    let (reader, writer) = match internal_stream_pair() {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("TEMP-DIAG-180: snapshot: stream pair failed: {error:?}");
            return None;
        }
    };
    if reader.set_nonblocking(true).is_err() {
        eprintln!("TEMP-DIAG-180: snapshot: nonblocking failed");
        return None;
    }
    // The fallback helper participates in the same launch generation as
    // every other status-owning child.  A concurrent adopted-zombie reaper
    // must never mistake the just-forked `ps` process for an unregistered
    // descendant and steal its wait status.
    let launch = StatusChildLaunch::begin();
    let fork_registration = STATUS_CHILD_FORK_REGISTRATION
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("TEMP-DIAG-180: snapshot: spawn failed: {error:?}");
            return None;
        }
    };
    let _registration = StatusChildRegistration::new(child.id());
    drop(fork_registration);
    drop(launch);
    // Command retains configured descriptors after spawn; release our writer
    // so child EOF is observable instead of timing out every valid snapshot.
    drop(command);
    let mut bytes = Vec::new();
    let mut reader = reader;
    use std::io::Read as _;
    loop {
        if Instant::now() >= deadline {
            eprintln!(
                "TEMP-DIAG-180: snapshot: read deadline passed with {} bytes after {}ms",
                bytes.len(),
                entered.elapsed().as_millis()
            );
            let _ = child.kill();
            let _ = wait_child_until(&mut child, cleanup_deadline());
            return None;
        }
        let mut chunk = [0; 8192];
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                // No per-iteration "first byte" print: see the PTY
                // flow-control note at this function's entry.
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                eprintln!("TEMP-DIAG-180: snapshot: read failed: {error:?}");
                let _ = child.kill();
                let _ = wait_child_until(&mut child, cleanup_deadline());
                return None;
            }
        }
    }
    // EOF is independent of process exit: a helper may close stdout early.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    eprintln!("TEMP-DIAG-180: snapshot: helper status {status:?}");
                }
                return status.success().then_some(bytes);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                eprintln!("TEMP-DIAG-180: snapshot: helper did not exit in time");
                let _ = child.kill();
                let _ = wait_child_until(&mut child, cleanup_deadline());
                return None;
            }
        }
    }
}

/// Stop all still-owned session members, including descendants that created
/// another process group. Keep the leader unreaped until the final signal.
pub(crate) fn stop_session(
    child: &mut Child,
    first_signal: i32,
) -> std::io::Result<std::process::ExitStatus> {
    let mut tick = || Ok(());
    stop_session_with_tick(child, first_signal, &mut tick)
}

fn stop_session_with_tick(
    child: &mut Child,
    first_signal: i32,
    tick: &mut dyn FnMut() -> std::io::Result<()>,
) -> std::io::Result<std::process::ExitStatus> {
    stop_session_outcome(child, first_signal, tick).into_result()
}

fn stop_session_outcome(
    child: &mut Child,
    first_signal: i32,
    tick: &mut dyn FnMut() -> std::io::Result<()>,
) -> StopOutcome {
    let (mut statuses, tick_result) =
        stop_sessions_with_tick(std::slice::from_mut(child), first_signal, tick);
    StopOutcome {
        status: statuses.pop().expect("one owned child"),
        tick_error: tick_result.err(),
    }
}

/// How normal completion treats descendants that outlive the leader.
///
/// `Strict` verifies every descendant stopped and was reaped, escalating
/// through termination and full discovery; it is the default for arbitrary
/// commands (hooks, provider shims, test workers) whose survivors must be
/// stopped synchronously. `Detach` instead returns the leader's status as
/// soon as the leader exits and output is complete, leaving live
/// fire-and-forget holders running detached (they self-exit; later
/// sessions opportunistically reap them). Detach may therefore return
/// while grandchildren are still running — callers must not assume a
/// quiescent process table afterwards. Detach applies only to pinned
/// deterministic leaf tools (Git builtins) whose lingering children are
/// short telemetry and probe helpers by construction; cancellation,
/// timeouts, and unproven completion always take the strict path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LingerPolicy {
    Strict,
    Detach,
}

/// Terminal state for one supervised child session.
#[derive(Debug)]
pub(crate) enum SessionEnd {
    Exited(std::process::ExitStatus),
    Interrupted(i32),
    TimedOut,
    /// The child outcome is intentionally suppressed because teardown could
    /// not prove that every owned process stopped and was reaped.
    CleanupIncomplete,
}

fn end_after_cleanup(
    cleanup: std::io::Result<std::process::ExitStatus>,
    completed: SessionEnd,
) -> SessionEnd {
    if cleanup.is_ok() {
        completed
    } else {
        // TEMP-DIAG-180: end_after_cleanup drops the stop error silently while
        // decide_after_cleanup prints DOT_TEARDOWN_FAIL. The macOS 125-vs-143
        // failures take this path with no other diagnostic; print the error
        // so CI names which stop failure macOS hits. Remove with the fix.
        eprintln!(
            "TEMP-DIAG-180: end_after_cleanup err: {:?}",
            cleanup.as_ref().err()
        );
        set_cleanup_incomplete("end_after_cleanup");
        SessionEnd::CleanupIncomplete
    }
}

fn decide_after_cleanup(
    cleanup: std::io::Result<std::process::ExitStatus>,
    deadline: Option<Instant>,
    deferred_error: Option<std::io::Error>,
    completed: impl FnOnce(std::process::ExitStatus) -> SessionEnd,
) -> std::io::Result<SessionEnd> {
    let status = match cleanup {
        Ok(status) => status,
        Err(error) => {
            eprintln!("DOT_TEARDOWN_FAIL: {error:?}");
            // TEMP-DIAG-180: remove with the recvmsg diag.
            eprintln!(
                "TEMP-DIAG-180: decide: cleanup err preempts (deferred={})",
                deferred_error.is_some()
            );
            set_cleanup_incomplete("decide_after_cleanup");
            return Ok(SessionEnd::CleanupIncomplete);
        }
    };
    if let Some(signal) = received_signal() {
        return Ok(SessionEnd::Interrupted(signal));
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(SessionEnd::TimedOut);
    }
    if let Some(error) = deferred_error {
        return Err(error);
    }
    Ok(completed(status))
}

struct StopOutcome {
    status: std::io::Result<std::process::ExitStatus>,
    tick_error: Option<std::io::Error>,
}

impl StopOutcome {
    fn into_result(self) -> std::io::Result<std::process::ExitStatus> {
        match (self.status, self.tick_error) {
            (_, Some(error)) => Err(error),
            (status, None) => status,
        }
    }
}

/// Typed failure from an owned command whose output is captured. Callers that
/// guard durable mutations must distinguish cancellation and bounded-capture
/// rejection from an ordinary command failure instead of collapsing both to
/// an absent output value.
#[derive(Debug)]
pub(crate) enum SessionOutputError {
    Io(std::io::Error),
    Interrupted(i32),
    TimedOut,
    CaptureLimit,
    CleanupIncomplete,
}

/// Run one command in an owned session, polling cancellation and an optional
/// absolute deadline while the caller drains bounded output in `tick`.
///
/// The retained leader remains the session identity through teardown. Every
/// return path stops and reaps the child session, and a signal arriving during
/// normal teardown wins over the child's ordinary status.
pub(crate) fn supervise_session(
    command: Command,
    deadline: Option<Instant>,
    tick: impl FnMut(bool) -> std::io::Result<()>,
) -> std::io::Result<SessionEnd> {
    supervise_session_with_completion(command, deadline, tick, || true, LingerPolicy::Strict)
}

/// Run one command like [`supervise_session`], but detach lingering
/// descendants on normal completion instead of stopping them. See
/// [`LingerPolicy::Detach`]; only pinned deterministic leaf tools qualify.
pub(crate) fn supervise_session_detached(
    command: Command,
    deadline: Option<Instant>,
    tick: impl FnMut(bool) -> std::io::Result<()>,
) -> std::io::Result<SessionEnd> {
    supervise_session_with_completion(command, deadline, tick, || true, LingerPolicy::Detach)
}

fn supervise_session_with_completion(
    command: Command,
    deadline: Option<Instant>,
    mut tick: impl FnMut(bool) -> std::io::Result<()>,
    mut completion_proven: impl FnMut() -> bool,
    linger: LingerPolicy,
) -> std::io::Result<SessionEnd> {
    let Some(mut child) = spawn_owned_session(command)? else {
        return Ok(SessionEnd::Interrupted(
            received_signal().expect("cancelled launch has a signal"),
        ));
    };
    child.linger = linger;
    loop {
        // Cancellation owns the boundary before callers are allowed to
        // interpret another unit of child output. A tick may do more than
        // copy bytes (for example, acknowledging a provider prompt), so
        // invoking it first could release new work after the user interrupted
        // the command.
        if let Some(signal) = received_signal() {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
            let _ = tick(true);
            return Ok(end_after_cleanup(
                stopped.status,
                SessionEnd::Interrupted(signal),
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
            let deferred_error = stopped.tick_error.or_else(|| tick(true).err());
            return decide_after_cleanup(stopped.status, deadline, deferred_error, |_| {
                SessionEnd::TimedOut
            });
        }
        if let Err(error) = tick(false) {
            // Preserve graceful process cleanup even when output delivery
            // fails; later ticks can still drain child-facing capture pipes.
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
            let _ = tick(true);
            return decide_after_cleanup(stopped.status, deadline, Some(error), |_| {
                SessionEnd::CleanupIncomplete
            });
        }
        if let Some(signal) = received_signal() {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
            let _ = tick(true);
            return Ok(end_after_cleanup(
                stopped.status,
                SessionEnd::Interrupted(signal),
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
            let deferred_error = stopped.tick_error.or_else(|| tick(true).err());
            return decide_after_cleanup(stopped.status, deadline, deferred_error, |_| {
                SessionEnd::TimedOut
            });
        }
        match exited(child.child()) {
            Ok(true) => {
                // Drain the already-exited child's bounded capture before
                // deciding whether normal completion is fully observed. A
                // short command commonly exits just before its socket EOF is
                // consumed; treating that harmless race as abnormal teardown
                // would run a host-wide descendant scan for every query.
                let drain_deadline = cleanup_deadline();
                let mut final_tick = tick(true);
                if final_tick.is_ok() && !completion_proven() {
                    // The leader is gone but output is undrained, so a
                    // lingering descendant still holds the pipes. Those
                    // holders are usually fire-and-forget helpers that
                    // would loiter for seconds; terminate the retained
                    // group once so they exit promptly instead of being
                    // waited out. Bytes already written stay buffered in
                    // the sockets (termination cannot retract them) and
                    // are collected by the bounded drain below, while
                    // anything still open afterwards takes the full path
                    // as before.
                    let _ = signal_group_result(child.child().id(), libc::SIGTERM);
                    let _ = signal_group_result(child.child().id(), libc::SIGCONT);
                }
                while final_tick.is_ok()
                    && !completion_proven()
                    && received_signal().is_none()
                    && deadline.is_none_or(|deadline| Instant::now() < deadline)
                    && Instant::now() < drain_deadline
                {
                    std::thread::sleep(Duration::from_millis(1));
                    final_tick = tick(true);
                }
                let completion_proven = final_tick.is_ok() && completion_proven();
                let stopped =
                    child.stop_with_tick_outcome(libc::SIGTERM, completion_proven, &mut || {
                        tick(false)
                    });
                let trailing_tick = if completion_proven {
                    Ok(())
                } else {
                    tick(true)
                };
                let deferred_error = stopped
                    .tick_error
                    .or_else(|| final_tick.err())
                    .or_else(|| trailing_tick.err());
                return decide_after_cleanup(stopped.status, deadline, deferred_error, |status| {
                    SessionEnd::Exited(status)
                });
            }
            Ok(false) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                let stopped =
                    child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
                let final_tick = tick(true);
                let deferred_error = stopped.tick_error.or_else(|| final_tick.err());
                return decide_after_cleanup(stopped.status, deadline, deferred_error, |_| {
                    SessionEnd::TimedOut
                });
            }
            // One-millisecond exit polling: short commands dominate
            // supervised sessions, and a coarser quantum here overshoots
            // every reap by half its period on average.
            Ok(false) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                let stopped =
                    child.stop_with_tick_outcome(libc::SIGTERM, false, &mut || tick(false));
                let _ = tick(true);
                return decide_after_cleanup(stopped.status, deadline, Some(error), |_| {
                    SessionEnd::CleanupIncomplete
                });
            }
        }
    }
}

/// Private lifetime lease inherited across exec and by ordinary descendants.
/// The parent retains only the nonblocking reader: EOF after the retained
/// leader exits is kernel proof that no descendant still owns the write end,
/// so the common short-command path needs no host-wide process-table scan.
struct NestedControlWorker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn nested_supervisor_belongs_to_session(pid: u32, leader: u32) -> bool {
    if pid == leader {
        return true;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // A shell or other cooperative launcher may retain the owned session and
    // wait for a nested Dot process instead of exec-replacing itself. The
    // private stream's peer credentials authenticate the claimed PID; its SID
    // then proves that peer is still inside the retained leader's boundary.
    unsafe { libc::getsid(pid) == leader as libc::pid_t }
}

impl NestedControlWorker {
    fn start(
        receiver: std::os::unix::net::UnixDatagram,
        expected_leader: u32,
        state: std::sync::Arc<std::sync::Mutex<NestedControlState>>,
    ) -> Self {
        use std::io::Read as _;
        use std::io::Write as _;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            // Third tuple element is the insert instant: links younger
            // than the settle window skip liveness reads (see below).
            let mut links = std::collections::BTreeMap::<
                String,
                (u32, std::os::unix::net::UnixStream, Instant),
            >::new();
            // Boundaries that read EOF on the previous tick but have not
            // confirmed it yet (see the liveness scan below).
            let mut suspect = std::collections::HashSet::<String>::new();
            while !worker_stop.load(std::sync::atomic::Ordering::Acquire) {
                let mut received = 0usize;
                loop {
                    if received >= 128 {
                        // This is a per-tick work budget, not the protocol's
                        // capacity limit. Registered supervisors have already
                        // been recorded before their ACK, and an unprocessed
                        // publisher remains gated, so yielding here is safe.
                        break;
                    }
                    match receive_nested_registration(&receiver) {
                        Ok(Some(mut registration)) => {
                            received += 1;
                            let valid = nested_supervisor_belongs_to_session(
                                registration.pid,
                                expected_leader,
                            ) && !links.contains_key(&registration.boundary)
                                && links.len() < MAX_ACTIVE_NESTED_SESSIONS;
                            if !valid {
                                // TEMP-DIAG-180: remove with the recvmsg diag.
                                eprintln!("TEMP-DIAG-180: nested-control cleared: invalid frame");
                                let mut state =
                                    state.lock().unwrap_or_else(|error| error.into_inner());
                                state.complete = false;
                                state.supervisors.clear();
                                links.clear();
                                suspect.clear();
                                break;
                            }
                            // Fail closed on a duplicate install: macOS has
                            // handed the same fd number out twice while the
                            // first entry still mapped it, so both entries
                            // would read one socket. Never acknowledge it.
                            let incoming_fd = std::os::fd::AsRawFd::as_raw_fd(&registration.link);
                            if links.values().any(|(_, link, _)| {
                                std::os::fd::AsRawFd::as_raw_fd(link) == incoming_fd
                            }) {
                                // TEMP-DIAG-180: remove with the recvmsg diag.
                                eprintln!(
                                    "TEMP-DIAG-180: nested-control cleared: duplicate install fd={incoming_fd}"
                                );
                                let mut state =
                                    state.lock().unwrap_or_else(|error| error.into_inner());
                                state.complete = false;
                                state.supervisors.clear();
                                links.clear();
                                suspect.clear();
                                break;
                            }
                            // Record before acknowledging: the ACK tells the
                            // publisher its registration is already visible,
                            // so the shared insert must land first. ACK-first
                            // let a burst's final insert land after every ACK
                            // and flaked convergence polls on loaded runners.
                            state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .supervisors
                                .insert(registration.boundary.clone(), registration.pid);
                            if registration.link.write_all(&[1]).is_err() {
                                // TEMP-DIAG-180: remove with the recvmsg diag.
                                eprintln!("TEMP-DIAG-180: nested-control cleared: ack failed");
                                let mut state =
                                    state.lock().unwrap_or_else(|error| error.into_inner());
                                state.complete = false;
                                state.supervisors.clear();
                                links.clear();
                                suspect.clear();
                                break;
                            }
                            // TEMP-DIAG-180: remove with the recvmsg diag.
                            eprintln!(
                                "TEMP-DIAG-180: nested-control link insert: {} fd={}",
                                registration.boundary,
                                std::os::fd::AsRawFd::as_raw_fd(&registration.link),
                            );
                            links.insert(
                                registration.boundary,
                                (registration.pid, registration.link, Instant::now()),
                            );
                        }
                        Ok(None) => break,
                        Err(error) => {
                            // TEMP-DIAG-180: remove with the recvmsg diag.
                            eprintln!(
                                "TEMP-DIAG-180: nested-control cleared: receive error {error:?}"
                            );
                            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                            state.complete = false;
                            state.supervisors.clear();
                            links.clear();
                            suspect.clear();
                            break;
                        }
                    }
                }
                let mut closed = Vec::new();
                let scan = Instant::now();
                for (boundary, (_pid, link, born)) in &mut links {
                    // Settle window: on macOS a freshly received link can
                    // read a sticky-but-spurious 0 for its first several
                    // milliseconds (16/64 died at insert age with no
                    // window; 4/64 still died at 5-7ms under a 5ms window,
                    // with back-to-back confirmatory reads also 0, peer
                    // held open throughout). 50ms is far past the observed
                    // window under any monotone-decay fit, and the
                    // tick-confirmation below absorbs any tail. Real peer
                    // closes persist, so the window only delays withdrawal
                    // detection for closes that land inside it.
                    if scan.saturating_duration_since(*born) < Duration::from_millis(50) {
                        continue;
                    }
                    let mut byte = [0u8; 1];
                    match link.read(&mut byte) {
                        Ok(0) => {
                            // Tick-confirmed EOF: a single 0 with no
                            // history merely flags the link suspect; only
                            // the second consecutive EOF removes it. A
                            // real peer close reads 0 on every tick (EOF
                            // is sticky), so confirmation costs one tick
                            // (1ms) of withdrawal latency. Inside the
                            // settle window above, spurious macOS 0s can
                            // repeat back-to-back, which is why fresh
                            // links skip reads entirely; out here any
                            // lone 0 is absorbed instead of removing a
                            // live link.
                            if suspect.contains(boundary.as_str()) {
                                // TEMP-DIAG-180: remove with the recvmsg diag.
                                // Root-cause the sticky macOS EOF: is the
                                // socket identity intact (peerpid still the
                                // publisher?) and does poll agree it is
                                // at EOF (HUP/ERR) or readable-empty?
                                let peer = nested_peer_pid(std::os::fd::AsRawFd::as_raw_fd(link));
                                let mut pollfd = libc::pollfd {
                                    fd: std::os::fd::AsRawFd::as_raw_fd(link),
                                    events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
                                    revents: 0,
                                };
                                let poll_result = unsafe { libc::poll(&mut pollfd, 1, 0) };
                                eprintln!(
                                    "TEMP-DIAG-180: nested-control link EOF confirmed: {boundary} fd={} peer={peer:?} poll={poll_result} revents={} claimed={}",
                                    std::os::fd::AsRawFd::as_raw_fd(link),
                                    pollfd.revents,
                                    _pid,
                                );
                                suspect.remove(boundary.as_str());
                                closed.push(boundary.clone())
                            } else {
                                // TEMP-DIAG-180: remove with the recvmsg diag.
                                eprintln!(
                                    "TEMP-DIAG-180: nested-control link EOF suspect: {boundary} fd={}",
                                    std::os::fd::AsRawFd::as_raw_fd(link),
                                );
                                suspect.insert(boundary.clone());
                            }
                        }
                        Ok(_) => {
                            // TEMP-DIAG-180: remove with the recvmsg diag.
                            eprintln!(
                                "TEMP-DIAG-180: nested-control cleared: unexpected link byte"
                            );
                            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                            state.complete = false;
                            state.supervisors.clear();
                            suspect.clear();
                            closed.extend(links.keys().cloned());
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            suspect.remove(boundary.as_str());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                            suspect.remove(boundary.as_str());
                        }
                        Err(error) => {
                            // TEMP-DIAG-180: remove with the recvmsg diag.
                            eprintln!(
                                "TEMP-DIAG-180: nested-control link read error: {boundary} {error:?}"
                            );
                            suspect.remove(boundary.as_str());
                            closed.push(boundary.clone())
                        }
                    }
                }
                if !closed.is_empty() {
                    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                    for boundary in closed {
                        links.remove(&boundary);
                        suspect.remove(&boundary);
                        state.supervisors.remove(&boundary);
                    }
                }
                // One-millisecond control polling: session teardown joins
                // this worker, so a coarser quantum delays every normal
                // completion by half its period on average.
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for NestedControlWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct SessionLease {
    reader: std::os::unix::net::UnixStream,
    parent_writer: Option<std::os::unix::net::UnixStream>,
    control_reader: Option<std::os::unix::net::UnixDatagram>,
    parent_control_writer: Option<std::os::unix::net::UnixDatagram>,
    control_state: std::sync::Arc<std::sync::Mutex<NestedControlState>>,
    control_worker: Option<NestedControlWorker>,
    ancestor_registration: Option<std::os::unix::net::UnixStream>,
    boundary: String,
    lease_inode: u64,
    leader: Option<u32>,
    registration: Option<u64>,
}

impl SessionLease {
    fn install(command: &mut Command) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        adopt_descendants()?;
        let boundary = new_session_boundary()?;
        let mut inherited_lease_fds = inherited_session_lease_fds(command)?;
        let inherited_control_fds = inherited_session_control_fds(command)?;
        let boundary_chain = session_boundary_chain(command, &boundary)?;
        let expected_parent_controls = usize::from(boundary_chain.len() > 1);
        if inherited_lease_fds.len() + 1 != boundary_chain.len()
            || inherited_control_fds.len() != expected_parent_controls
        {
            return Err(std::io::Error::other(
                "owned-session boundary, lease, and control chains differ",
            ));
        }
        let (reader, writer) = internal_stream_pair()?;
        let (control_reader, control_writer) = internal_datagram_pair()?;
        let lease_inode = {
            // SAFETY: the writer is a live socket owned here; fstat writes
            // only the local stat. Capture now: the parent drops its writer
            // at spawn, after which the inode is no longer observable here.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(writer.as_raw_fd(), &mut stat) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            stat.st_ino as u64
        };
        reader.set_nonblocking(true)?;
        control_reader.set_nonblocking(true)?;
        let writer_fd = writer.as_raw_fd();
        let control_writer_fd = control_writer.as_raw_fd();
        if inherited_lease_fds.len() >= MAX_ANCESTOR_BOUNDARIES {
            return Err(std::io::Error::other(
                "too many inherited session lease descriptors",
            ));
        }
        inherited_lease_fds.push(writer_fd);
        command.env(SESSION_BOUNDARY_ENV, boundary_chain.join(":"));
        command.env(
            SESSION_LEASE_FDS_ENV,
            inherited_lease_fds
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(":"),
        );
        command.env(SESSION_CONTROL_FDS_ENV, control_writer_fd.to_string());
        // SAFETY: `writer_fd` remains owned by `Self` through spawn. The
        // callback performs only async-signal-safe fcntl calls after fork and
        // before exec, clearing CLOEXEC in the child process alone.
        unsafe {
            command.pre_exec(move || {
                for fd in inherited_lease_fds
                    .iter()
                    .chain(std::iter::once(&control_writer_fd))
                {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        // Publish only to the immediate parent and retain a private stream
        // that arbitrary target code never inherits. The parent must
        // authenticate and acknowledge this exact per-session registration
        // before `install` can return and the child can be authorized.
        let ancestor_registration = publish_nested_supervisor(&inherited_control_fds, &boundary)?;
        let control_state = std::sync::Arc::new(std::sync::Mutex::new(NestedControlState::new()));
        Ok(Self {
            reader,
            parent_writer: Some(writer),
            control_reader: Some(control_reader),
            parent_control_writer: Some(control_writer),
            control_state,
            control_worker: None,
            ancestor_registration,
            boundary,
            lease_inode,
            leader: None,
            registration: None,
        })
    }

    fn parent_spawned(&mut self, leader: u32, registration: u64) {
        self.parent_spawned_inner(leader);
        register_session_lease(leader, self.lease_inode);
        self.registration = Some(registration);
    }

    fn parent_spawned_foreground(&mut self, leader: u32) {
        self.parent_spawned_inner(leader);
    }

    fn parent_spawned_inner(&mut self, leader: u32) {
        self.parent_writer.take();
        self.parent_control_writer.take();
        let control_reader = self
            .control_reader
            .take()
            .expect("unstarted nested-supervisor control reader");
        self.control_worker = Some(NestedControlWorker::start(
            control_reader,
            leader,
            self.control_state.clone(),
        ));
        self.leader = Some(leader);
    }

    fn closed(&mut self) -> std::io::Result<bool> {
        loop {
            let mut byte = [0u8; 1];
            match self.reader.read(&mut byte) {
                Ok(0) => return Ok(true),
                // The lease protocol never writes payload bytes. Treat an
                // unexpected byte as evidence that a writer is still open;
                // bounded cleanup must not spin draining a hostile writer.
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // Darwin reports a momentarily exhausted socket buffer as
                // ENOBUFS (raw 55, surfaced as `Uncategorized`), not
                // `WouldBlock`; retry it like an interrupt.
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Block until the lease closes or `timeout` elapses, waking the
    /// instant the last writer goes away instead of polling on a sleep
    /// quantum. True survivors outlast the timeout and report still-open
    /// so the caller takes the full discovery path, exactly as before.
    /// A received cancellation signal likewise ends the wait early as
    /// still-open so the caller tears the session down promptly.
    fn wait_closed(&mut self, timeout: Duration) -> bool {
        use std::os::fd::AsRawFd as _;

        match self.closed() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(_) => return false,
        }
        let deadline = Instant::now() + timeout;
        loop {
            if received_signal().is_some() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let mut observed = libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll observes one live socket and writes only the
            // local revents field before the bounded timeout expires.
            let ready = unsafe {
                libc::poll(
                    &mut observed,
                    1,
                    i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX),
                )
            };
            if ready > 0 {
                match self.closed() {
                    // A data byte (a protocol violation) or a spurious
                    // wakeup with an open lease re-polls the remainder
                    // rather than spinning or declaring victory.
                    Ok(false) => {}
                    Ok(true) => return true,
                    Err(_) => return false,
                }
            } else if ready == 0 {
                return false;
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return false;
            }
        }
    }
}

/// Normal-completion kill verify window: after an optimistic group
/// kill for an open lease, the fast path waits this long for the lease
/// to close before falling back to full discovery. Lingering holders at
/// this point have already had the drain (cooperative termination with
/// output collection) and the capture pipes are EOF, so there is no
/// output left to be courteous about; SIGKILL also catches holders with
/// TERM blocked or ignored in startup. Anything still open afterwards
/// (moved-group survivors) takes the full path, exactly as before.
const NORMAL_COMPLETION_KILL_VERIFY: Duration = Duration::from_millis(50);

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.ancestor_registration.take();
        self.control_worker.take();
        if let (Some(leader), Some(registration)) = (self.leader.take(), self.registration.take()) {
            unregister_session_boundary(leader, registration);
            unregister_session_lease(leader);
        }
    }
}

/// Run a command as an owned session and translate its terminal state to a
/// shell-compatible status. This is the blocking-command boundary used by
/// repository operations that otherwise inherit their caller's streams.
pub(crate) fn run_session_status(command: Command, linger: LingerPolicy) -> i32 {
    match linger {
        LingerPolicy::Strict => session_end_status(supervise_session(command, None, |_| Ok(()))),
        LingerPolicy::Detach => {
            session_end_status(supervise_session_detached(command, None, |_| Ok(())))
        }
    }
}

/// Run a cooperative child without creating a new session, preserving the
/// caller's controlling terminal and foreground process group for prompts.
/// The child must own and stop any subprocesses it creates (Git/SSH and sudo
/// provide that contract); Dot retains the direct child handle for bounded
/// TERM/KILL/reap on cancellation. Normal completion trusts the reaped
/// leader status even when a benign daemon outlives it with the lease
/// open (shell parity); only cancellation and timeout stay strict.
pub(crate) fn run_foreground_status(command: Command) -> i32 {
    session_end_status(supervise_child_with_policy(
        command,
        None,
        |_| Ok(()),
        ForegroundLeasePolicy::TrustLeader,
    ))
}

fn session_end_status(end: std::io::Result<SessionEnd>) -> i32 {
    match end {
        Ok(SessionEnd::Exited(status)) => status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal().map_or(127, |signal| 128 + signal)
        }),
        Ok(SessionEnd::Interrupted(signal)) => 128 + signal,
        Ok(SessionEnd::CleanupIncomplete) => CLEANUP_INCOMPLETE_STATUS,
        Ok(SessionEnd::TimedOut) | Err(_) => 127,
    }
}

struct SessionCapture {
    reader: std::os::unix::net::UnixStream,
    closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SessionCapture {
    fn new() -> std::io::Result<(Self, Stdio)> {
        let (reader, writer) = internal_stream_pair()?;
        reader.set_nonblocking(true)?;
        Ok((
            Self {
                reader,
                closed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            Stdio::from(std::os::fd::OwnedFd::from(writer)),
        ))
    }

    fn closed_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.closed.clone()
    }

    fn drain(
        &mut self,
        output: &mut Vec<u8>,
        remaining: &mut usize,
        budget: usize,
        discard: bool,
    ) -> std::io::Result<bool> {
        let mut drained = 0;
        let mut overflow = false;
        while drained < budget {
            let mut chunk = [0u8; 8192];
            let available = (budget - drained).min(chunk.len());
            match self.reader.read(&mut chunk[..available]) {
                Ok(0) => {
                    self.closed
                        .store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                Ok(count) => {
                    drained += count;
                    if discard || count > *remaining {
                        overflow |= !discard;
                        *remaining = 0;
                    } else {
                        output.extend_from_slice(&chunk[..count]);
                        *remaining -= count;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(overflow)
    }
}

/// Run an inspection command in an owned session while draining both output
/// streams into one cumulative bound. Cancellation stops the whole session;
/// overflow changes subsequent ticks to drain-only teardown and returns a
/// stable error instead of retaining more data.
pub(crate) fn run_session_output(
    command: Command,
    deadline: Option<Instant>,
    limit: usize,
    linger: LingerPolicy,
) -> std::io::Result<Output> {
    run_session_output_typed(command, deadline, limit, linger).map_err(|error| match error {
        SessionOutputError::Io(error) => error,
        SessionOutputError::Interrupted(_) => std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "subprocess interrupted by signal",
        ),
        SessionOutputError::TimedOut => std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "subprocess exceeded its deadline",
        ),
        SessionOutputError::CaptureLimit => std::io::Error::other(COMMAND_CAPTURE_LIMIT_ERROR),
        SessionOutputError::CleanupIncomplete => {
            std::io::Error::other("owned subprocess cleanup could not be verified")
        }
    })
}

/// Run a captured owned session with a finite byte slice as stdin. The feeder
/// owns one end of a socket pair, so terminating the child closes the peer and
/// unblocks the writer before it is joined. Input is bounded by the same limit
/// as retained output; callers use this for small hash and JSON payloads.
///
/// Precondition: every caller feeds a small payload to a command that
/// drains stdin (`git hash-object`, `jq`, init-client git plumbing).
/// A feeder error fails the call even when the child exits 0, so a
/// caller whose child might exit without draining stdin would invert
/// a success into a failure here (the shell reports success there).
pub(crate) fn run_session_output_with_input(
    mut command: Command,
    input: &[u8],
    deadline: Option<Instant>,
    limit: usize,
    linger: LingerPolicy,
) -> std::io::Result<Output> {
    if input.len() > limit {
        return Err(std::io::Error::other(COMMAND_CAPTURE_LIMIT_ERROR));
    }
    let (reader, mut writer) = internal_stream_pair()?;
    writer.set_nonblocking(true)?;
    command.stdin(Stdio::from(std::os::fd::OwnedFd::from(reader)));
    let input = input.to_vec();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeder_stop = stop.clone();
    let feeder = std::thread::spawn(move || {
        use std::io::Write as _;
        let mut written = 0;
        while written < input.len() {
            if feeder_stop.load(std::sync::atomic::Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "subprocess input cancelled",
                ));
            }
            match writer.write(&input[written..]) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "subprocess stopped consuming input",
                    ));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    });
    let output = run_session_output(command, deadline, limit, linger);
    stop.store(true, std::sync::atomic::Ordering::Release);
    let fed = feeder
        .join()
        .map_err(|_| std::io::Error::other("subprocess input feeder panicked"))?;
    match (output, fed) {
        (Ok(output), Ok(())) => Ok(output),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

pub(crate) fn run_session_output_typed(
    mut command: Command,
    deadline: Option<Instant>,
    limit: usize,
    linger: LingerPolicy,
) -> std::result::Result<Output, SessionOutputError> {
    let (mut stdout_reader, stdout_writer) =
        SessionCapture::new().map_err(SessionOutputError::Io)?;
    let (mut stderr_reader, stderr_writer) =
        SessionCapture::new().map_err(SessionOutputError::Io)?;
    let stdout_closed = stdout_reader.closed_flag();
    let stderr_closed = stderr_reader.closed_flag();
    command.stdout(stdout_writer).stderr(stderr_writer);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut remaining = limit;
    let mut overflowed = false;
    let end = supervise_session_with_completion(
        command,
        deadline,
        |final_pass| {
            let budget = if final_pass {
                COMMAND_CAPTURE_FINAL_BYTES
            } else {
                COMMAND_CAPTURE_TICK_BYTES
            };
            let stdout_overflow =
                stdout_reader.drain(&mut stdout, &mut remaining, budget, overflowed)?;
            let stderr_overflow = stderr_reader.drain(
                &mut stderr,
                &mut remaining,
                budget,
                overflowed || stdout_overflow,
            )?;
            overflowed |= stdout_overflow || stderr_overflow;
            if overflowed {
                Err(std::io::Error::other(COMMAND_CAPTURE_LIMIT_ERROR))
            } else {
                Ok(())
            }
        },
        || {
            stdout_closed.load(std::sync::atomic::Ordering::Acquire)
                && stderr_closed.load(std::sync::atomic::Ordering::Acquire)
        },
        linger,
    );
    let end = match end {
        Ok(end) => end,
        Err(_) if overflowed => return Err(SessionOutputError::CaptureLimit),
        Err(error) => return Err(SessionOutputError::Io(error)),
    };
    match end {
        SessionEnd::Exited(status) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        SessionEnd::Interrupted(signal) => Err(SessionOutputError::Interrupted(signal)),
        SessionEnd::TimedOut => Err(SessionOutputError::TimedOut),
        SessionEnd::CleanupIncomplete => Err(SessionOutputError::CleanupIncomplete),
    }
}

/// Launch a cooperative foreground child behind the same signal/registration
/// authorization barrier as an isolated session, without changing its process
/// group or controlling terminal.
fn spawn_owned_child(mut command: Command) -> std::io::Result<Option<OwnedChild>> {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    let mut blocked = BlockedLaunchSignals::install()?;
    let launch = StatusChildLaunch::begin();
    if received_signal().is_some() {
        return Ok(None);
    }
    let (mut ready_parent, ready_child) = internal_stream_pair()?;
    let (mut authorize_parent, authorize_child) = internal_stream_pair()?;
    // A foreground child stays in Dot's controlling terminal group, but it
    // still needs a private lifetime/control boundary.  In a nested Dot this
    // registration tells the immediate ancestor to delegate the subtree to
    // this process while it owns the child's retained status.
    let mut lease = SessionLease::install(&mut command)?;
    let ready_fd = ready_child.as_raw_fd();
    let authorize_fd = authorize_child.as_raw_fd();
    let ready_parent_fd = ready_parent.as_raw_fd();
    let authorize_parent_fd = authorize_parent.as_raw_fd();
    let previous = blocked.previous();
    // SAFETY: the callback performs only async-signal-safe libc operations.
    // The child cannot exec until the registrar has retained its exact status
    // identity and has rechecked cancellation.
    unsafe {
        command.pre_exec(move || {
            libc::close(ready_parent_fd);
            libc::close(authorize_parent_fd);
            let pid = libc::getpid() as u32;
            let bytes = pid.to_ne_bytes();
            let mut written = 0usize;
            while written < bytes.len() {
                let count = libc::write(
                    ready_fd,
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                );
                if count < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                written += count as usize;
            }
            libc::close(ready_fd);
            let mut decision = 0u8;
            loop {
                let count = libc::read(authorize_fd, (&mut decision as *mut u8).cast(), 1);
                if count == 1 {
                    break;
                }
                if count < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            libc::close(authorize_fd);
            if decision != 1 {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            reset_child_signal_dispositions()?;
            let error = libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
            if error == 0 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(error))
            }
        });
    }
    let registrar_mask = blocked.previous();
    let registrar = std::thread::spawn(
        move || -> std::io::Result<(u32, bool, Option<StatusChildRegistration>)> {
            let error = unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &registrar_mask, std::ptr::null_mut())
            };
            if error != 0 {
                let _ = authorize_parent.write_all(&[0]);
                return Err(std::io::Error::from_raw_os_error(error));
            }
            let mut bytes = [0u8; std::mem::size_of::<u32>()];
            if let Err(error) = ready_parent.read_exact(&mut bytes) {
                let _ = authorize_parent.write_all(&[0]);
                return Err(error);
            }
            let pid = u32::from_ne_bytes(bytes);
            let mut registration = None;
            let mut authorized = received_signal().is_none();
            if authorized {
                registration = Some(StatusChildRegistration::new(pid));
                if received_signal().is_some() {
                    registration.take();
                    authorized = false;
                }
            }
            authorize_parent.write_all(&[u8::from(authorized)])?;
            Ok((pid, authorized, registration))
        },
    );
    let spawned = command.spawn();
    drop(command);
    drop(ready_child);
    drop(authorize_child);
    let decision = match registrar.join() {
        Ok(Ok(decision)) => decision,
        Ok(Err(error)) => {
            if let Ok(mut child) = spawned {
                let _ = child.kill();
                let _ = wait_child_until(&mut child, cleanup_deadline());
            }
            blocked.restore()?;
            return Err(error);
        }
        Err(_) => {
            if let Ok(mut child) = spawned {
                let _ = child.kill();
                let _ = wait_child_until(&mut child, cleanup_deadline());
            }
            blocked.restore()?;
            return Err(std::io::Error::other("child launch registrar panicked"));
        }
    };
    let child = match spawned {
        Ok(child) if decision.1 && child.id() == decision.0 => child,
        Ok(mut child) => {
            drop(decision.2);
            let _ = child.kill();
            let _ = wait_child_until(&mut child, cleanup_deadline());
            blocked.restore()?;
            if !decision.1 && received_signal().is_some() {
                return Ok(None);
            }
            return Err(std::io::Error::other(
                "child launch authorization did not match its retained identity",
            ));
        }
        Err(error) => {
            drop(decision.2);
            blocked.restore()?;
            if !decision.1 && received_signal().is_some() {
                return Ok(None);
            }
            return Err(error);
        }
    };
    let registration = decision
        .2
        .expect("authorized foreground child has status registration");
    lease.parent_spawned_foreground(child.id());
    let owned = OwnedChild::from_registered_with_lease(child, registration, lease);
    if let Err(error) = blocked.restore() {
        drop(owned);
        return Err(error);
    }
    drop(launch);
    Ok(Some(owned))
}

/// How `supervise_child` treats a session lease left open past the
/// leader's normal completion (a descendant outlived the leader).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ForegroundLeasePolicy {
    /// Provider contract: the provider owns its dependency-manager
    /// descendants explicitly (`owned-subprocess-cancellation-v1`),
    /// so an open lease past leader exit is `CleanupIncomplete`
    /// (125), never the leader status.
    Strict,
    /// Passthrough (git fetch/push, the pull streaming fallback,
    /// interactive sudo): no ownership contract covers the child's
    /// daemons (an ssh mux master legitimately inherits the lease),
    /// so normal completion reports the reaped leader status like
    /// the shell instead of 125. Cancellation and timeout keep the
    /// strict outcome.
    TrustLeader,
}

/// Run a cooperative child in the caller's process group.
///
/// This boundary is for a child that must retain the caller's controlling
/// terminal and promises to stop every process tree it creates. Dot therefore
/// signals and reaps only the exact retained child PID; group delivery would
/// also target Dot and the invoking shell.
pub(crate) fn supervise_child(
    command: Command,
    deadline: Option<Instant>,
    tick: impl FnMut(bool) -> std::io::Result<()>,
) -> std::io::Result<SessionEnd> {
    supervise_child_with_policy(command, deadline, tick, ForegroundLeasePolicy::Strict)
}

/// [`supervise_child`] with an explicit open-lease policy: the
/// provider path always runs [`ForegroundLeasePolicy::Strict`];
/// shell-parity passthrough runs
/// [`ForegroundLeasePolicy::TrustLeader`].
pub(crate) fn supervise_child_with_policy(
    command: Command,
    deadline: Option<Instant>,
    mut tick: impl FnMut(bool) -> std::io::Result<()>,
    policy: ForegroundLeasePolicy,
) -> std::io::Result<SessionEnd> {
    let Some(mut child) = spawn_owned_child(command)? else {
        return Ok(SessionEnd::Interrupted(
            received_signal().expect("cancelled launch has a signal"),
        ));
    };
    loop {
        if let Some(signal) = received_signal() {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
            let _ = tick(true);
            return Ok(end_after_cleanup(
                stopped.status,
                SessionEnd::Interrupted(signal),
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
            let deferred_error = stopped.tick_error.or_else(|| tick(true).err());
            return decide_after_cleanup(stopped.status, deadline, deferred_error, |_| {
                SessionEnd::TimedOut
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                child.disarm();
                return decide_after_cleanup(Ok(status), deadline, tick(true).err(), |status| {
                    SessionEnd::Exited(status)
                });
            }
            Ok(None) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
                let deferred_error = stopped.tick_error.or_else(|| tick(true).err());
                return decide_after_cleanup(stopped.status, deadline, deferred_error, |_| {
                    SessionEnd::TimedOut
                });
            }
            Ok(None) => {}
            Err(error) => {
                let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
                let _ = tick(true);
                // Passthrough trust: the leader already exited with a
                // known status and only the lease proof failed — on
                // normal completion (no signal, no expired deadline)
                // report the leader like the shell instead of 125.
                // Cancellation, timeout, and the Strict provider
                // policy keep the fail-closed outcome below.
                if matches!(policy, ForegroundLeasePolicy::TrustLeader)
                    && received_signal().is_none()
                    && !deadline.is_some_and(|deadline| Instant::now() >= deadline)
                {
                    if let Some(status) = child.reaped_status() {
                        return Ok(SessionEnd::Exited(status));
                    }
                }
                return decide_after_cleanup(stopped.status, deadline, Some(error), |_| {
                    SessionEnd::CleanupIncomplete
                });
            }
        }
        // Observe an already-published exit before interpreting another live
        // event. In particular, a capable provider can leave a complete JSONL
        // prompt queued and then exit with 128+signal; consuming that prompt
        // first could acknowledge and release work after cancellation.
        if let Err(error) = tick(false) {
            // The provider owns descendants that Dot cannot safely
            // group-signal in the caller's foreground session. Give it TERM
            // even when output delivery failed, and keep draining during the
            // grace period so its cleanup cannot deadlock behind a full pipe.
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
            let _ = tick(true);
            return decide_after_cleanup(stopped.status, deadline, Some(error), |_| {
                SessionEnd::CleanupIncomplete
            });
        }
        if let Some(signal) = received_signal() {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
            let _ = tick(true);
            return Ok(end_after_cleanup(
                stopped.status,
                SessionEnd::Interrupted(signal),
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let stopped = child.stop_with_tick_outcome(libc::SIGTERM, &mut || tick(false));
            let deferred_error = stopped.tick_error.or_else(|| tick(true).err());
            return decide_after_cleanup(stopped.status, deadline, deferred_error, |_| {
                SessionEnd::TimedOut
            });
        }
        // One-millisecond exit polling, matching the session supervisor:
        // short foreground commands otherwise overshoot every reap.
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct OwnedChild {
    child: Option<Child>,
    registration: Option<StatusChildRegistration>,
    lease: Option<SessionLease>,
    /// Exit status once `try_wait` has reaped the child. The numeric
    /// PID is free from that point (std caches the status for later
    /// `try_wait` calls), so every later path must send NO signals
    /// and issue NO kill — any of them could address an unrelated
    /// process that recycled the PID.
    reaped: Option<std::process::ExitStatus>,
}

impl OwnedChild {
    #[cfg(test)]
    fn new(child: Child) -> Self {
        let registration = StatusChildRegistration::new(child.id());
        Self::from_registered(child, registration)
    }

    #[cfg(test)]
    fn from_registered(child: Child, registration: StatusChildRegistration) -> Self {
        Self {
            child: Some(child),
            registration: Some(registration),
            lease: None,
            reaped: None,
        }
    }

    fn from_registered_with_lease(
        child: Child,
        registration: StatusChildRegistration,
        lease: SessionLease,
    ) -> Self {
        Self {
            child: Some(child),
            registration: Some(registration),
            lease: Some(lease),
            reaped: None,
        }
    }

    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("owned child")
    }

    fn reaped_status(&self) -> Option<std::process::ExitStatus> {
        self.reaped
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        if let Some(status) = self.reaped {
            // Already reaped: the PID has been free since the reap,
            // so only re-prove the lease here — never signal (the
            // stop path's reaped fast path owns that invariant).
            self.prove_lease_closed()?;
            return Ok(Some(status));
        }
        let status = self.child().try_wait()?;
        if status.is_some() {
            // std `try_wait` reaps on success: record it before the
            // lease poll, during which the PID stays free.
            self.reaped = status;
            self.prove_lease_closed()?;
        }
        Ok(status)
    }

    fn prove_lease_closed(&mut self) -> std::io::Result<()> {
        let Some(lease) = self.lease.as_mut() else {
            return Ok(());
        };
        poll_until(cleanup_deadline(), || {
            lease.closed().map(|closed| closed.then_some(()))
        })
        .map_err(|_| {
            std::io::Error::other("foreground subprocess descendants retained their lifetime lease")
        })
    }

    fn disarm(&mut self) {
        self.child.take();
        self.registration.take();
        self.lease.take();
    }

    #[cfg(test)]
    fn stop_with_tick(
        &mut self,
        signal: i32,
        tick: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> std::io::Result<std::process::ExitStatus> {
        self.stop_with_tick_outcome(signal, tick).into_result()
    }

    fn stop_with_tick_outcome(
        &mut self,
        signal: i32,
        tick: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> StopOutcome {
        if let Some(status) = self.reaped {
            // Already reaped: the numeric PID has been free since
            // `try_wait` observed the exit, so this path sends NO
            // signals and issues NO kill — any of them could strike
            // an unrelated recycled PID (proven by strace: the old
            // code signaled the freed PID twice, both ESRCH). The
            // caller just ran a full-deadline lease poll, so one
            // non-blocking recheck catches a just-closed race; the
            // outcome reports the lease proof directly.
            let lease_closed = match self.lease.as_mut() {
                None => true,
                Some(lease) => lease.closed().unwrap_or(false),
            };
            let outcome = if lease_closed {
                Ok(status)
            } else {
                Err(std::io::Error::other(
                    "foreground subprocess descendants retained their lifetime lease",
                ))
            };
            self.disarm();
            return StopOutcome {
                status: outcome,
                tick_error: None,
            };
        }
        let pid = self.child().id();
        // Drain before signaling: a provider blocked writing to a full
        // capture socket cannot run its TERM handler until the socket
        // moves, so the signal would pend behind output backpressure
        // (notably on macOS, where small socket buffers fill fast).
        // Best-effort unblock; errors are observed on the grace ticks.
        for _ in 0..4 {
            let _ = tick();
        }
        signal_pid(pid, signal);
        if signal != libc::SIGKILL {
            // A stopped cooperative provider cannot run its TERM handler until
            // resumed. The child is unreaped here (the reaped fast path
            // above returned already), so its zombie-or-live PID cannot
            // be reused and the follow-up signal addresses the same
            // identity.
            signal_pid(pid, libc::SIGCONT);
        }
        let deadline =
            Instant::now() + Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS);
        let mut tick_error = None;
        loop {
            match self.try_wait() {
                Err(error) => {
                    return StopOutcome {
                        status: Err(error),
                        tick_error,
                    };
                }
                Ok(Some(status)) => {
                    self.disarm();
                    return StopOutcome {
                        status: Ok(status),
                        tick_error,
                    };
                }
                Ok(None) if signal == libc::SIGKILL || Instant::now() >= deadline => break,
                Ok(None) => {
                    // Drain several bounded chunks per grace quantum, like
                    // the session-stop loop: one small drain per sleep can
                    // make a cooperative TERM handler hit the grace
                    // deadline merely because it is flushing output
                    // (notably on macOS, where small socket buffers and
                    // loaded-timer oversleep compound).
                    for _ in 0..4 {
                        if let Err(error) = tick() {
                            // Retain the first delivery failure but continue
                            // teardown. Later ticks may still drain bytes from the
                            // child-facing socket even when its external sink is
                            // permanently closed.
                            if tick_error.is_none() {
                                tick_error = Some(error);
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(GRACE_INTERVAL_MS));
                }
            }
        }
        let _ = self.child().kill();
        let mut status = wait_child_until(self.child(), cleanup_deadline());
        if status.is_ok() {
            if let Err(error) = self.prove_lease_closed() {
                status = Err(error);
            }
        }
        self.disarm();
        StopOutcome { status, tick_error }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // A reaped child is never signaled: its PID is free, and its
        // cached status needs no reaping. (All reaping paths disarm
        // first, so this guard is defense-in-depth for future ones;
        // current std also short-circuits kill-after-reap, but that
        // is a toolchain behavior, not a contract.)
        if self.reaped.is_none() {
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = wait_child_until(child, cleanup_deadline());
            }
        }
        self.registration.take();
        self.lease.take();
    }
}

pub(crate) struct OwnedSession {
    child: Option<Child>,
    lease: SessionLease,
    linger: LingerPolicy,
}

/// Drain already-exited members of a group-killed session without
/// registry validation or host-wide discovery. Runs after an optimistic
/// group kill whose lease already verified closed, so every victim is
/// a known-dead zombie; zombies keep their process group alive, which
/// pins the pgid against reuse while any victim still waits, and every
/// waitable member is ours by construction.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn drain_retained_group_zombies(pgid: u32) {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: P_PGID names the retained group; WNOWAIT keeps each
        // status until waitpid below consumes that exact PID.
        let waited = unsafe {
            libc::waitid(
                libc::P_PGID,
                pgid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if waited != 0 {
            return;
        }
        // SAFETY: a successful waitid initialized the siginfo union, and
        // si_pid is the documented discriminator (the same pattern
        // `wait_member` uses).
        let pid = unsafe { info.si_pid() };
        if pid <= 0 {
            return;
        }
        let mut status = 0;
        // SAFETY: WNOWAIT retained this exact group member and WNOHANG
        // consumes only its currently-waitable status, never blocking.
        unsafe {
            libc::waitpid(pid, &mut status, libc::WNOHANG);
        }
    }
}

/// Determine whether an exited session leader may have left live members
/// behind that only full descendant discovery can classify, never with a
/// signal.
///
/// A group-scoped `waitid` cannot prove this: under `WSTOPPED`-only options
/// `ECHILD` matches neither the retained zombie leader nor any live
/// unstopped child (verified by strace: `setsid` runs at spawn, yet the
/// query on the leader's own pgid still returns `ECHILD`), and members
/// that moved to another group escape group-scoped queries by design.
/// Treating that `ECHILD` as proof of absence skipped discovery for real
/// survivors, so the probe verifies the session directly instead.
///
/// Soundness: every spawned session owns its session ID, so a live process
/// in the leader's session is a genuine survivor however it moved between
/// groups. Sibling sessions own theirs, foreign direct children live in
/// ours, and nested owned sessions isolate theirs, so none of them can
/// trigger. The retained zombie leader never matches `live`.
///
/// Cost: one bounded host snapshot per strict stop. Detached leaf tools
/// (the thousands of git invocations per update) bypass this probe
/// entirely, so only the rare strict stops pay for it.
///
/// Known limit (risk-accepted, pinned by
/// `four_step_session_escape_is_a_documented_known_limit`): a
/// descendant that double-forks past an exited parent, setsids out
/// of the leader's session, closes every inherited fd, and execs
/// with a scrubbed environment is invisible to this probe (no
/// session member, no lease holder, no boundary marker) — the fast
/// path below then reports success with a live survivor. That
/// escapee is indistinguishable from a legitimate isolated process:
/// killing unprovable adoptees would risk foreign kills in embedded
/// mode, and any direct-child census false-positives on test
/// fixtures and breaks suite isolation (fresh-review-B P2-3 dilemma
/// analysis). Impact stays bounded: a same-uid stray holding no
/// fds, locks, or pipes, from an attacker that already had code
/// execution.
/// Error-path-only probe explaining why a process snapshot is unavailable.
///
/// Samples a bounded slice of the process table plus the fallback helper's
/// presence so a failed verification names its cause instead of reporting a
/// bare failure. No spawns, no sleeps, no retries: the diagnostic itself
/// must never stall teardown.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn snapshot_failure_hint() -> String {
    let mut readable = 0u32;
    let mut denied = 0u32;
    let mut missing = 0u32;
    let mut unparsable = 0u32;
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => return format!("cannot list /proc: {error}"),
    };
    for entry in entries.flatten().take(128) {
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            continue;
        };
        let Ok(pid) = text.parse::<u32>() else {
            continue;
        };
        match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => {
                if parse_proc_process(pid, &stat).is_some() {
                    readable += 1;
                } else {
                    unparsable += 1;
                }
            }
            Err(error) if proc_process_vanished(&error) => missing += 1,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                denied += 1;
            }
            Err(_) => missing += 1,
        }
    }
    #[cfg(target_os = "android")]
    let ps = "/system/bin/ps";
    #[cfg(not(target_os = "android"))]
    let ps = "/bin/ps";
    format!(
        "procfs sample: {readable} readable, {denied} denied, {missing} vanished, {unparsable} unparsable; {ps}: {}",
        if Path::new(ps).exists() {
            "present"
        } else {
            "missing"
        }
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn normal_completion_needs_discovery(leader: u32, deadline: Instant) -> std::io::Result<bool> {
    let raw = libc::pid_t::try_from(leader)
        .map_err(|_| std::io::Error::other("session leader PID does not fit pid_t"))?;
    // SAFETY: getsid takes a PID and no pointers; the retained zombie
    // leader still has a session to report.
    let session = unsafe { libc::getsid(raw) };
    if session <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let session = session as u32;
    let processes = process_snapshot(deadline).ok_or_else(|| {
        std::io::Error::other(format!(
            "could not verify the completed session ({})",
            snapshot_failure_hint()
        ))
    })?;
    Ok(processes
        .iter()
        .any(|process| process.live && process.session == session))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn normal_completion_needs_discovery(_leader: u32, _deadline: Instant) -> std::io::Result<bool> {
    // Portable Unix lacks the session-snapshot proof. Take the full
    // discovery path rather than certifying absence from a closed
    // cooperative lease alone.
    Ok(true)
}

impl OwnedSession {
    pub(crate) fn child(&self) -> &Child {
        self.child.as_ref().expect("owned session child")
    }

    pub(crate) fn exited(&self) -> std::io::Result<bool> {
        exited(self.child())
    }

    pub(crate) fn stop(&mut self, signal: i32) -> std::io::Result<std::process::ExitStatus> {
        let outcome = self.stop_with_tick_outcome(signal, true, &mut || Ok(()));
        if outcome.status.is_err() {
            set_cleanup_incomplete("OwnedSession::stop");
        }
        outcome.into_result()
    }

    fn stop_with_tick_outcome(
        &mut self,
        signal: i32,
        completion_proven: bool,
        tick: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> StopOutcome {
        let child = self.child.as_mut().expect("owned session child");
        let leader_exited = match exited(child) {
            Ok(exited) => exited,
            Err(error) => {
                return StopOutcome {
                    status: Err(error),
                    tick_error: None,
                };
            }
        };
        // Detach policy: a proven, exited leader with no cancellation
        // returns immediately even with an open lease, leaving live
        // fire-and-forget holders running detached (already-dead adoptees
        // are still collected below, and later sessions opportunistically
        // reap the rest). Anything else stays strict.
        let detach_completion = completion_proven
            && leader_exited
            && matches!(self.linger, LingerPolicy::Detach)
            && received_signal().is_none();
        // Only the Linux/Android fast path drains group-kill victims
        // directly; portable builds take the same tuple through the full
        // path below without reading the flag.
        #[cfg_attr(
            not(any(target_os = "linux", target_os = "android")),
            allow(unused_variables)
        )]
        let (lease_closed, group_killed) = if completion_proven && leader_exited {
            // Check once first: a closed lease takes the fast path with no
            // signals at all. An open lease means a live descendant past
            // the drain, usually a fire-and-forget helper lingering with
            // TERM blocked in startup; kill the retained group so it exits
            // promptly instead of being waited out, then verify with a
            // bounded blocking wait. Anything still open after that is a
            // moved-group survivor and takes the full discovery path
            // below, exactly as before. Detach skips all of that.
            match self.lease.closed() {
                Ok(true) => (true, false),
                _ if detach_completion => (false, false),
                _ => {
                    let _ = signal_group_result(child.id(), libc::SIGKILL);
                    (self.lease.wait_closed(NORMAL_COMPLETION_KILL_VERIFY), true)
                }
            }
        } else {
            match self.lease.closed() {
                Ok(closed) => (closed, false),
                Err(error) => {
                    return StopOutcome {
                        status: Err(error),
                        tick_error: None,
                    };
                }
            }
        };
        // A closed lease is not proof that no descendant survives: a
        // descendant can intentionally close every inherited descriptor.
        // On Linux the session-snapshot probe below verifies directly;
        // portable platforms always take the full fail-closed discovery
        // path.
        let same_group_member =
            if completion_proven && leader_exited && lease_closed && !detach_completion {
                match normal_completion_needs_discovery(child.id(), cleanup_deadline()) {
                    Ok(present) => present,
                    Err(error) => {
                        return StopOutcome {
                            status: Err(error),
                            tick_error: None,
                        };
                    }
                }
            } else {
                false
            };
        if completion_proven
            && leader_exited
            && (lease_closed || detach_completion)
            && !same_group_member
        {
            // Blocking reap: the leader is an already-observed retained
            // zombie, so this releases it immediately with no polling
            // quantum. A concurrent cancellation is observed by the caller,
            // which owns the teardown decision. A fully escaped
            // descendant (double-fork + setsid + close-all +
            // env-scrubbed exec) survives this path by design — see the
            // known-limit note on `normal_completion_needs_discovery`.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let leader = child.id();
            // The reassignment below is Linux/Android-only; macOS only moves
            // the initial value into the outcome.
            #[allow(unused_mut)]
            let mut status = child.wait();
            // A group kill leaves known-dead members whose status the
            // registry-validated sweep below cannot consume once the
            // leader's registration is gone, so drain them directly. The
            // victims verified dead above, and a zombie keeps its process
            // group alive, so the pgid cannot be reused while any victim
            // still waits; the drain only ever observes our own dead.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if group_killed {
                drain_retained_group_zombies(leader);
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if status.is_ok() {
                if let Err(error) = reap_ready_adopted_zombies(cleanup_deadline()) {
                    status = Err(error);
                }
            }
            self.child.take();
            return StopOutcome {
                status,
                tick_error: None,
            };
        }
        // The full session cleanup path owns cooperative delivery. Keeping the
        // initial signal there avoids signaling an unchanged handler once here
        // and again after descendant discovery.
        let mut result = stop_session_outcome(child, signal, tick);
        if result.status.is_err() {
            // `stop_session` can fail while discovering descendants or
            // reaping the leader. Do not let taking the guard's child turn
            // that observation failure into an unowned live process.
            let _ = child.kill();
            let _ = wait_child_until(child, cleanup_deadline());
        }
        self.child.take();
        let lease = poll_until(cleanup_deadline(), || {
            self.lease.closed().map(|closed| closed.then_some(()))
        });
        if result.status.is_ok() && lease.is_err() {
            result.status = Err(std::io::Error::other(
                "owned subprocess lease remained open after bounded cleanup",
            ));
        }
        result
    }
}

/// Atomically authorize, launch, and register an isolated child session.
/// A pre-latched signal returns `None`; a signal pending on the spawning thread
/// is released only after the child and its transitive ownership marker are
/// retained. The barrier is thread-local and never serializes parallel spawns.
pub(crate) fn spawn_owned_session(mut command: Command) -> std::io::Result<Option<OwnedSession>> {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    let mut blocked = BlockedLaunchSignals::install()?;
    let launch = StatusChildLaunch::begin();
    if received_signal().is_some() {
        return Ok(None);
    }
    isolate(&mut command);
    let mut lease = SessionLease::install(&mut command)?;
    let (mut ready_parent, ready_child) = internal_stream_pair()?;
    let (mut authorize_parent, authorize_child) = internal_stream_pair()?;
    let ready_fd = ready_child.as_raw_fd();
    let authorize_fd = authorize_child.as_raw_fd();
    let ready_parent_fd = ready_parent.as_raw_fd();
    let authorize_parent_fd = authorize_parent.as_raw_fd();
    let previous = blocked.previous();
    // SAFETY: the callback uses only async-signal-safe libc operations. The
    // parent-side registrar supplies one decision byte while Command::spawn is
    // waiting for exec, avoiding a post-fork ownership gap.
    unsafe {
        command.pre_exec(move || {
            // The child must not retain copies of the registrar's endpoints:
            // if that thread fails, EOF on the authorization reader is the
            // fail-closed release from this pre-exec barrier.
            libc::close(ready_parent_fd);
            libc::close(authorize_parent_fd);
            let pid = libc::getpid() as u32;
            let bytes = pid.to_ne_bytes();
            let mut written = 0usize;
            while written < bytes.len() {
                let count = libc::write(
                    ready_fd,
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                );
                if count < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                written += count as usize;
            }
            libc::close(ready_fd);
            let mut decision = 0u8;
            loop {
                let count = libc::read(authorize_fd, (&mut decision as *mut u8).cast(), 1);
                if count == 1 {
                    break;
                }
                if count < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            libc::close(authorize_fd);
            if decision != 1 {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            reset_child_signal_dispositions()?;
            let error = libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
            if error == 0 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(error))
            }
        });
    }
    let boundary = lease.boundary.clone();
    let boundary_control = lease.control_state.clone();
    let registrar_mask = blocked.previous();
    let registrar = std::thread::spawn(move || -> std::io::Result<(u32, bool, Option<u64>)> {
        let mut registered = None;
        let result = (|| {
            // The registrar must be eligible to run the installed handler
            // while the spawning thread and pre-exec child are blocked.
            // SAFETY: `registrar_mask` is the initialized pre-launch mask.
            let error = unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &registrar_mask, std::ptr::null_mut())
            };
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            let mut bytes = [0u8; std::mem::size_of::<u32>()];
            ready_parent.read_exact(&mut bytes)?;
            let pid = u32::from_ne_bytes(bytes);
            let mut authorized = received_signal().is_none();
            if authorized {
                registered = Some((
                    pid,
                    register_session_boundary(pid, &boundary, boundary_control.clone()),
                ));
                if received_signal().is_some() {
                    unregister_session_boundary(pid, registered.expect("registration").1);
                    registered = None;
                    authorized = false;
                }
            }
            authorize_parent.write_all(&[u8::from(authorized)])?;
            Ok((
                pid,
                authorized,
                registered.map(|(_pid, registration)| registration),
            ))
        })();
        if result.is_err() {
            if let Some((pid, registration)) = registered {
                unregister_session_boundary(pid, registration);
            }
            let _ = authorize_parent.write_all(&[0]);
        }
        result
    });
    let spawned = command.spawn();
    drop(command);
    drop(ready_child);
    drop(authorize_child);
    let decision = match registrar.join() {
        Ok(Ok(decision)) => decision,
        Ok(Err(error)) => {
            if let Ok(mut child) = spawned {
                let _ = child.kill();
                let _ = wait_child_until(&mut child, cleanup_deadline());
            }
            blocked.restore()?;
            return Err(error);
        }
        Err(_) => {
            if let Ok(mut child) = spawned {
                let _ = child.kill();
                let _ = wait_child_until(&mut child, cleanup_deadline());
            }
            blocked.restore()?;
            return Err(std::io::Error::other("session launch registrar panicked"));
        }
    };
    let child = match spawned {
        Ok(child) if decision.1 && child.id() == decision.0 => child,
        Ok(mut child) => {
            if let Some(registration) = decision.2 {
                unregister_session_boundary(decision.0, registration);
            }
            let _ = child.kill();
            let _ = wait_child_until(&mut child, cleanup_deadline());
            blocked.restore()?;
            if !decision.1 && received_signal().is_some() {
                return Ok(None);
            }
            return Err(std::io::Error::other(
                "session launch authorization did not match its child",
            ));
        }
        Err(error) => {
            if let Some(registration) = decision.2 {
                unregister_session_boundary(decision.0, registration);
            }
            blocked.restore()?;
            if !decision.1 && received_signal().is_some() {
                return Ok(None);
            }
            return Err(error);
        }
    };
    lease.parent_spawned(
        child.id(),
        decision
            .2
            .expect("authorized session has a boundary registration"),
    );
    // Command retains configured descriptors after spawn. Release them before
    // cancellation can enter descendant teardown.
    let mut owned = OwnedSession {
        child: Some(child),
        lease,
        linger: LingerPolicy::Strict,
    };
    if let Err(error) = blocked.restore() {
        let _ = owned.stop(libc::SIGKILL);
        return Err(error);
    }
    drop(launch);
    Ok(Some(owned))
}

/// What a `waitid` drain does next after handling one WNOWAIT candidate.
#[cfg(any(target_os = "linux", target_os = "android"))]
enum ReapCandidateOutcome {
    /// Re-observe: the candidate was reaped, vanished, skipped, or lost a race.
    Reobserve,
    /// Re-observe after yielding: the candidate stopped being waitable.
    YieldAndReobserve,
    /// The same unconsumable child was reported twice; the pass is done.
    PassComplete,
}

/// Validate and reap one WNOWAIT-observed candidate. The caller holds the
/// fork-registration lock across the observation and this call, and drops
/// it before acting on the outcome so launches can interleave passes.
/// Identity, parenthood, and live registrations are rechecked here; only
/// consumable direct zombies lose their status.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_observed_candidate(
    pid: u32,
    skipped: &mut std::collections::BTreeSet<ProcessIdentity>,
) -> std::io::Result<ReapCandidateOutcome> {
    use ReapCandidateOutcome::{PassComplete, Reobserve, YieldAndReobserve};
    let Some(process) = linux_process_info(pid) else {
        // A registered owner may have reaped the WNOWAIT candidate after
        // it was selected. Re-observe instead of treating disappearance
        // as authority to act on the numeric PID.
        return Ok(Reobserve);
    };
    if process.parent != std::process::id() {
        return Ok(Reobserve);
    }
    if !reaper_may_consume(&process) {
        // Not ours: a foreign direct zombie, or a retained handle's
        // child another owner will reap. Skipping it here needs no
        // host-wide snapshot: a repeated observation means waitid keeps
        // reporting the same unconsumable child, so the pass is done and
        // any later zombie waits for the next pass.
        if !skipped.insert(process.identity.clone()) {
            return Ok(PassComplete);
        }
        return Ok(Reobserve);
    }
    let mut status = 0;
    // SAFETY: WNOWAIT retained this direct child identity and waitpid with
    // WNOHANG can only reap that exact currently-waitable PID.
    let reaped = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    if reaped == pid as i32 {
        forget_session_descendant(&process.identity);
        return Ok(Reobserve);
    }
    if reaped == 0 {
        return Ok(YieldAndReobserve);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EINTR) {
        return Ok(Reobserve);
    }
    if error.raw_os_error() == Some(libc::ECHILD) {
        // Another retained pidfd owner may have completed the exact wait
        // after this WNOWAIT observation. Re-observe the child set;
        // losing the race is not evidence of incomplete cleanup.
        return Ok(Reobserve);
    }
    Err(error)
}

/// Reap already-exited direct adoptees after a proven normal completion.
///
/// Reaping the retained leader first lets `waitid(P_ALL)` distinguish
/// `ECHILD` (nothing else to reap) without walking the host process table.
/// Every candidate is revalidated against live registrations before its
/// status is consumed: recorded adoptees, members of live sessions, and (in
/// an exclusively owned process) unregistered direct zombies. Foreign
/// children and retained handles keep their status; overlapping launches
/// only delay discovery.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_ready_adopted_zombies(deadline: Instant) -> std::io::Result<()> {
    // This lock covers only nonblocking post-exit observation/reaping. It does
    // not serialize launches or waits, but prevents parallel completion paths
    // from both claiming the same WNOWAIT zombie.
    let _reaper = ADOPTED_ZOMBIE_REAPER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // A deadline may expire immediately after KILL verification while a
    // finite set of already-waitable adoptees remains. Draining those with
    // WNOHANG cannot block, so permit a bounded number of terminal
    // observations after the wall-clock boundary. The cap prevents a child
    // churner from turning this best-effort drain into an unbounded loop.
    let mut expired_attempts = 0usize;
    let mut skipped = std::collections::BTreeSet::new();
    loop {
        // No launch-quiescence wait: every candidate below is revalidated
        // against live registrations before its status is consumed, and a
        // zombie retains its PID until then, so overlapping launches can only
        // delay discovery. Waiting for global quiescence starves under
        // sustained parallel spawning instead.
        if Instant::now() >= deadline {
            if expired_attempts >= 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "adopted subprocess reaping exceeded its deadline",
                ));
            }
            expired_attempts += 1;
        }
        let fork_registration = STATUS_CHILD_FORK_REGISTRATION
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // SAFETY: P_ALL ignores the id argument, siginfo is initialized, and
        // WNOWAIT retains the exact child until its boundary is checked.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if waited != 0 {
            drop(fork_registration);
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ECHILD) => Ok(()),
                Some(libc::EINTR) => continue,
                _ => Err(error),
            };
        }
        // SAFETY: successful waitid initialized the siginfo union for a child
        // state result; si_pid is the documented discriminator for WEXITED.
        let pid = unsafe { info.si_pid() };
        if pid == 0 {
            drop(fork_registration);
            return Ok(());
        }
        let pid = u32::try_from(pid).map_err(|_| {
            std::io::Error::other("waitid returned an invalid adopted subprocess PID")
        })?;
        if is_registered_session_leader(pid) || is_registered_status_child(pid) {
            // waitid(P_ALL) cannot advance past a protected waitable child
            // without reaping it. Drain recorded adoptees behind it by PID,
            // then sweep each live leader's group for unrecorded stragglers;
            // both preserve that owner's retained status handle without a
            // host-wide snapshot.
            drop(fork_registration);
            reap_recorded_descendants()?;
            return reap_session_group_stragglers(deadline);
        }
        let outcome = reap_observed_candidate(pid, &mut skipped);
        drop(fork_registration);
        match outcome {
            Ok(ReapCandidateOutcome::PassComplete) => return Ok(()),
            Ok(ReapCandidateOutcome::YieldAndReobserve) => {
                std::thread::yield_now();
                continue;
            }
            Ok(ReapCandidateOutcome::Reobserve) => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Reap unrecorded same-group stragglers of live sessions after the `P_ALL`
/// drain met a protected child. An isolated session leader owns its process
/// group, so `waitid(P_PGID)` reaches members the global drain cannot name
/// without enumerating the host table; members that left their leader's
/// group wait for a pass whose `P_ALL` drain is unblocked. A protected
/// child inside a swept group pins that group the way it pins `P_ALL`: its
/// owner releases it, and a later pass continues.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_session_group_stragglers(deadline: Instant) -> std::io::Result<()> {
    let leaders = active_session_boundaries()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for leader in leaders {
        reap_group_stragglers(leader, deadline)?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_group_stragglers(pgid: u32, deadline: Instant) -> std::io::Result<()> {
    let mut expired_attempts = 0usize;
    let mut skipped = std::collections::BTreeSet::new();
    loop {
        if Instant::now() >= deadline {
            if expired_attempts >= 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "adopted subprocess reaping exceeded its deadline",
                ));
            }
            expired_attempts += 1;
        }
        let fork_registration = STATUS_CHILD_FORK_REGISTRATION
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // SAFETY: siginfo is initialized and P_PGID names one process group.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PGID,
                pgid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if waited != 0 {
            drop(fork_registration);
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ECHILD) => Ok(()),
                Some(libc::EINTR) => continue,
                _ => Err(error),
            };
        }
        // SAFETY: successful waitid initialized the siginfo union for a child
        // state result; si_pid is the documented discriminator for WEXITED.
        let pid = unsafe { info.si_pid() };
        if pid == 0 {
            drop(fork_registration);
            return Ok(());
        }
        let pid = u32::try_from(pid).map_err(|_| {
            std::io::Error::other("waitid returned an invalid adopted subprocess PID")
        })?;
        if is_registered_session_leader(pid) || is_registered_status_child(pid) {
            drop(fork_registration);
            return Ok(());
        }
        let outcome = reap_observed_candidate(pid, &mut skipped);
        drop(fork_registration);
        match outcome {
            Ok(ReapCandidateOutcome::PassComplete) => return Ok(()),
            Ok(ReapCandidateOutcome::YieldAndReobserve) => {
                std::thread::yield_now();
                continue;
            }
            Ok(ReapCandidateOutcome::Reobserve) => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Snapshot-based adopted-zombie drain, retained for direct test coverage
/// of the portable-fallback and overlapping-launch topologies. Production
/// teardown reaches adoptees behind a protected child through
/// [`reap_recorded_descendants`] plus [`reap_session_group_stragglers`]
/// instead, so no production pass pays a host-wide snapshot.
#[cfg(all(test, target_os = "linux"))]
fn scan_and_reap_unregistered_zombies(deadline: Instant) -> std::io::Result<()> {
    // Single pass with no launch quiescence: each candidate is revalidated
    // against live registrations inside reap_observed_zombie, and a zombie
    // retains its PID until consumed, so overlapping activity can only delay
    // discovery of a later zombie to the next pass.
    let processes = process_snapshot(deadline)
        .ok_or_else(|| std::io::Error::other("could not inspect adopted subprocesses"))?;
    let _fork_registration = STATUS_CHILD_FORK_REGISTRATION
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for process in processes.into_iter().filter(|process| {
        process.parent == std::process::id()
            && !process.live
            && !is_registered_session_leader(process.pid)
            && !is_registered_status_child(process.pid)
            && reaper_may_consume(process)
    }) {
        reap_observed_zombie(&process)?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_observed_zombie(process: &ProcessInfo) -> std::io::Result<()> {
    // A zombie retains its numeric PID until wait consumes it. WNOWAIT plus a
    // second start-generation read makes the no-pidfd fallback identity-safe.
    // SAFETY: siginfo is initialized and P_PID names the observed positive PID.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let waited = unsafe {
        libc::waitid(
            libc::P_PID,
            process.pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if waited != 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ECHILD) {
            Ok(())
        } else {
            Err(error)
        };
    }
    // SAFETY: successful waitid initialized the WEXITED siginfo fields.
    if unsafe { info.si_pid() } != process.pid as i32 {
        return Ok(());
    }
    let Some(current) = linux_process_info(process.pid) else {
        return Ok(());
    };
    if current.identity != process.identity
        || current.live
        || current.parent != std::process::id()
        || is_registered_session_leader(process.pid)
        || is_registered_status_child(process.pid)
        || !reaper_may_consume(process)
    {
        return Ok(());
    }
    let mut status = 0;
    // SAFETY: WNOWAIT retained this exact revalidated zombie child.
    let reaped = unsafe { libc::waitpid(process.pid as i32, &mut status, libc::WNOHANG) };
    if reaped == process.pid as i32 {
        forget_session_descendant(&process.identity);
        Ok(())
    } else if reaped < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
        // Another recorded owner consumed the status first; the record stays
        // until its start generation proves reuse, which a stale entry can
        // never match.
        Ok(())
    } else if reaped < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::other("adopted subprocess was not reaped"))
    }
}

impl Drop for OwnedSession {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = stop_session(child, libc::SIGKILL);
        }
    }
}

/// Stop a worker wave under one shared grace deadline. Each retained child
/// reserves its own SID until every final signal has been delivered.
pub(crate) fn stop_sessions(
    children: &mut [Child],
    first_signal: i32,
) -> Vec<std::io::Result<std::process::ExitStatus>> {
    let mut tick = || Ok(());
    stop_sessions_with_tick(children, first_signal, &mut tick).0
}

/// Stop a parallel wave of fully owned sessions under one shared deadline.
/// Their leases and boundary registrations remain live until the shared
/// process-table pass has completed, so one worker cannot lose attribution
/// while an adjacent worker is still being torn down.
pub(crate) fn stop_owned_sessions(
    sessions: &mut [OwnedSession],
    first_signal: i32,
) -> Vec<std::io::Result<std::process::ExitStatus>> {
    if sessions.iter().any(|session| session.child.is_none()) {
        set_cleanup_incomplete("stop_owned_sessions:child-lost");
        return (0..sessions.len())
            .map(|_| Err(std::io::Error::other("owned session lost its child handle")))
            .collect();
    }
    let mut children = Vec::with_capacity(sessions.len());
    for session in sessions.iter_mut() {
        children.push(session.child.take().expect("prevalidated owned child"));
    }
    let mut results = stop_sessions(&mut children, first_signal);
    let lease_deadline = cleanup_deadline();
    for (session, result) in sessions.iter_mut().zip(&mut results) {
        if result.is_ok()
            && poll_until(lease_deadline, || {
                session.lease.closed().map(|closed| closed.then_some(()))
            })
            .is_err()
        {
            *result = Err(std::io::Error::other(
                "owned subprocess lease remained open after bounded cleanup",
            ));
        }
    }
    if results.iter().any(std::result::Result::is_err) {
        set_cleanup_incomplete("stop_owned_sessions:results-err");
    }
    results
}

fn stop_sessions_with_tick(
    children: &mut [Child],
    first_signal: i32,
    tick: &mut dyn FnMut() -> std::io::Result<()>,
) -> (
    Vec<std::io::Result<std::process::ExitStatus>>,
    std::io::Result<()>,
) {
    let mut sessions: Vec<_> = children
        .iter()
        .map(|child| Session::new(child.id()))
        .collect();
    let graceful_deadline = cleanup_deadline();
    // TERM receives the historical one-second grace. KILL verification and
    // final reaping share one additional bounded window rather than renewing
    // a full deadline at each stage; retain half a grace of scan margin for a
    // busy host without allowing teardown latency to accumulate unboundedly.
    let hard_deadline = graceful_deadline
        + Duration::from_millis(GRACE_ATTEMPTS as u64 * GRACE_INTERVAL_MS * 3 / 2);
    // Deliver to the retained leader before a potentially expensive global
    // snapshot. On pidfd-capable systems every other member is then delivered
    // exactly once after discovery; portable/old-kernel systems use one
    // anchored group delivery and reserve later catchable delivery for newly
    // pinned identities.
    let initial_deliveries = sessions
        .iter_mut()
        .map(|session| session.signal_before_observation(first_signal))
        .collect::<Vec<_>>();
    let _ = observe_sessions(&mut sessions, graceful_deadline);
    for (session, delivery) in sessions.iter_mut().zip(initial_deliveries) {
        session.finish_initial_delivery(first_signal, delivery);
    }
    let mut tick_result = tick();
    let mut consecutive_empty = 0;
    while Instant::now() < graceful_deadline {
        let observed = observe_sessions(&mut sessions, graceful_deadline);
        if sessions_stably_empty(observed, &mut consecutive_empty) {
            break;
        }
        for session in &mut sessions {
            // New members receive one exact signal through retained authority.
            // A group-wide retry is reserved for a newly observed same-group
            // cohort that the platform could not pin safely.
            session.signal_new(first_signal);
        }
        // Drain several bounded chunks per process-table pass. Snapshotting a
        // busy host can dominate the loop; one small drain per scan can make
        // a cooperative TERM handler hit the grace deadline merely because
        // it is flushing its final diagnostics.
        for _ in 0..4 {
            let next_tick = tick();
            if tick_result.is_ok() {
                tick_result = next_tick;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if consecutive_empty < 2 {
        // Escalation is a verification phase, not a fire-and-forget signal.
        // Keep the leader unreaped so its original process-group ID remains
        // reserved until two complete snapshots prove the session empty.
        consecutive_empty = 0;
        for session in &mut sessions {
            // Do not spend the hard-phase budget on a pre-KILL host snapshot.
            // The last graceful observation already established the owned
            // cohort; KILL first, then use complete snapshots to discover and
            // deliver to any late members before certifying stable absence.
            session.signal_all(libc::SIGKILL);
        }
        while Instant::now() < hard_deadline {
            let observed = observe_sessions(&mut sessions, hard_deadline);
            if sessions_stably_empty(observed, &mut consecutive_empty) {
                break;
            }
            for session in &mut sessions {
                session.signal_all(libc::SIGKILL);
            }
            for _ in 0..4 {
                let next_tick = tick();
                if tick_result.is_ok() {
                    tick_result = next_tick;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if consecutive_empty < 2 {
            for session in &mut sessions {
                session.note_survivors();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let pending = sessions
        .iter_mut()
        .map(Session::take_members)
        .collect::<Vec<_>>();

    let statuses = children
        .iter_mut()
        .map(|child| wait_child_until(child, hard_deadline))
        .collect::<Vec<_>>();

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let reaped = reap_owned(pending, hard_deadline, wait_member);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    for (session, reaped) in sessions.iter_mut().zip(reaped) {
        if !reaped {
            session.record_error(std::io::Error::other(
                "could not reap every owned subprocess identity",
            ));
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Err(error) = reap_ready_adopted_zombies(hard_deadline) {
        for session in &mut sessions {
            session.record_error(std::io::Error::new(error.kind(), error.to_string()));
        }
    }

    let results = statuses
        .into_iter()
        .zip(&mut sessions)
        .map(|(status, session)| match session.error.take() {
            Some(error) => Err(error),
            None => status,
        })
        .collect();
    (results, merge_authority_errors(&mut sessions, tick_result))
}

/// Surface portable fail-closed refusals as the supervision call's error.
///
/// An authority refusal is not a cleanup verification failure: the reap
/// outcome stays intact and the refusal travels the deferred-error channel
/// so `decide_after_cleanup` returns it instead of suppressing the outcome
/// into `CleanupIncomplete`. First refusal wins; an existing tick error
/// takes precedence.
fn merge_authority_errors(
    sessions: &mut [Session],
    tick_result: std::io::Result<()>,
) -> std::io::Result<()> {
    tick_result?;
    for session in sessions {
        if let Some(error) = session.authority_error.take() {
            // TEMP-DIAG-180: remove with the recvmsg diag.
            eprintln!("TEMP-DIAG-180: authority refusal merged");
            return Err(error);
        }
    }
    Ok(())
}

/// Require two complete snapshots without a live session member. Process-table
/// enumeration is not atomic: a member can fork a replacement and exit between
/// entries, making one pass appear empty even though the owned session remains
/// active. An unavailable snapshot is likewise not evidence of quiescence.
fn sessions_stably_empty(observed: Option<bool>, consecutive_empty: &mut u8) -> bool {
    if observed == Some(true) {
        *consecutive_empty = consecutive_empty.saturating_add(1);
    } else {
        *consecutive_empty = 0;
    }
    *consecutive_empty >= 2
}

fn observe_sessions(sessions: &mut [Session], deadline: Instant) -> Option<bool> {
    for session in &mut *sessions {
        session.drain_nested_supervisors(deadline);
    }
    let mut processes = process_snapshot(deadline)?;
    // A nested owner publishes before authorizing its first child. If that
    // registration raced the first snapshot, incorporate it and take one
    // post-publication snapshot before any direct delivery decision.
    let mut registration_arrived = false;
    for session in &mut *sessions {
        let before = session.nested_supervisors.len();
        session.drain_nested_supervisors(deadline);
        registration_arrived |= session.nested_supervisors.len() != before;
    }
    if registration_arrived {
        processes = process_snapshot(deadline)?;
        for session in &mut *sessions {
            session.drain_nested_supervisors(deadline);
        }
    }
    // Snapshot helpers have now unregistered, so their short-lived
    // registrations cannot confuse direct-adoptee attribution below.
    let mut finished = true;
    for session in sessions {
        finished &= session.observe_snapshot(&processes);
    }
    Some(finished)
}

/// A retained leader reserves the original session and process-group IDs.
/// Linux and Android members additionally retain pidfds so direct delivery and
/// reaping never act on a reused numeric PID.
struct Session {
    leader: u32,
    current: std::collections::BTreeMap<u32, ProcessInfo>,
    group_delivered: std::collections::BTreeSet<ProcessIdentity>,
    delegated: std::collections::BTreeSet<ProcessIdentity>,
    nested_supervisors: std::collections::BTreeMap<u32, String>,
    control: Option<std::sync::Arc<std::sync::Mutex<NestedControlState>>>,
    control_complete: bool,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    members: std::collections::BTreeMap<u32, OwnedMember>,
    error: Option<std::io::Error>,
    // Portable fail-closed: an unpinned live member refuses unsafe delivery.
    // Unlike `error` (which suppresses the outcome into CleanupIncomplete),
    // this surfaces as the supervision call's error so callers must handle
    // the refusal explicitly.
    authority_error: Option<std::io::Error>,
}

#[derive(Clone, Copy)]
enum InitialDelivery {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    ExactLeader,
    Group(bool),
}

fn requires_direct_signal_authority(process: &ProcessInfo, leader: u32) -> bool {
    process.pid != leader && process.live && process.group != leader
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn member_claim_failure_is_fatal(process: &ProcessInfo, leader: u32) -> bool {
    // The retained leader keeps its original group ID stable, so group
    // delivery remains safe even on old kernels without pidfds. Only a live
    // member that escaped that anchored group requires process-directed
    // authority and therefore fails closed when it cannot be pinned.
    requires_direct_signal_authority(process, leader)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
enum WaitState {
    Reaped,
    Running,
    Interrupted,
    NotChild,
    Terminal,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn wait_member(member: &OwnedMember) -> WaitState {
    use std::os::fd::AsRawFd as _;

    // SAFETY: P_PIDFD binds the wait to the retained descriptor identity;
    // WNOHANG prevents an unadopted grandchild from blocking this pass.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let waited = libc::waitid(
            libc::P_PIDFD,
            member.pidfd.as_raw_fd() as u32,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
        );
        if waited == 0 && info.si_pid() != 0 {
            WaitState::Reaped
        } else if waited == 0 {
            WaitState::Running
        } else {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => WaitState::Interrupted,
                Some(libc::ECHILD) => {
                    let path = format!("/proc/{}/stat", member.process.pid);
                    match read_proc_stat(Path::new(&path)) {
                        Err(error) if proc_process_vanished(&error) => {
                            // The pidfd still names the old process, but no procfs
                            // identity exists to reap: its original parent already
                            // completed the wait before adoption was necessary.
                            // ESRCH is the same churn observed between open and read.
                            WaitState::Reaped
                        }
                        Ok(stat) => match parse_proc_process(member.process.pid, &stat) {
                            Some(current) if current.identity == member.process.identity => {
                                WaitState::NotChild
                            }
                            Some(_) => WaitState::Reaped,
                            None => WaitState::Terminal,
                        },
                        Err(_) => WaitState::Terminal,
                    }
                }
                _ => WaitState::Terminal,
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_owned<T>(
    sessions: Vec<Vec<T>>,
    deadline: Instant,
    mut wait: impl FnMut(&T) -> WaitState,
) -> Vec<bool> {
    let count = sessions.len();
    let mut failed = vec![false; count];
    let mut sessions = sessions.into_iter().enumerate().collect::<Vec<_>>();
    while !sessions.is_empty() {
        for (index, pending) in &mut sessions {
            let mut next = Vec::new();
            let mut not_children = Vec::new();
            let mut progress = false;
            let mut child_pending = false;
            let mut interrupted = false;
            for member in pending.drain(..) {
                match wait(&member) {
                    WaitState::Reaped => progress = true,
                    WaitState::Running => {
                        child_pending = true;
                        next.push(member);
                    }
                    WaitState::Interrupted => {
                        interrupted = true;
                        next.push(member);
                    }
                    // A grandchild can become ours only after its intermediate
                    // parent is reaped later in this pass.
                    WaitState::NotChild => not_children.push(member),
                    WaitState::Terminal => failed[*index] = true,
                }
            }
            if progress || child_pending || interrupted {
                next.extend(not_children);
            } else if !not_children.is_empty() {
                // No retained process in this owned session made adoption
                // progress and none remains a child we can wait for. Dropping
                // these stable handles would falsely report complete cleanup.
                failed[*index] = true;
            }
            *pending = next;
        }
        sessions.retain(|(_, pending)| !pending.is_empty());
        if !sessions.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10).min(deadline.duration_since(now)));
        }
    }
    for (index, pending) in sessions {
        if !pending.is_empty() {
            failed[index] = true;
        }
    }
    failed.into_iter().map(|failed| !failed).collect()
}

impl Session {
    fn new(leader: u32) -> Self {
        Self {
            leader,
            current: std::collections::BTreeMap::new(),
            group_delivered: std::collections::BTreeSet::new(),
            delegated: std::collections::BTreeSet::new(),
            nested_supervisors: std::collections::BTreeMap::new(),
            control: session_control(leader),
            control_complete: true,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            members: std::collections::BTreeMap::new(),
            error: None,
            authority_error: None,
        }
    }

    fn drain_nested_supervisors(&mut self, deadline: Instant) -> bool {
        let Some(control) = self.control.as_ref().cloned() else {
            return self.control_complete;
        };
        if Instant::now() >= deadline {
            // TEMP-DIAG-180: remove with the recvmsg diag.
            eprintln!("TEMP-DIAG-180: nested-control cleared: drain past deadline");
            self.invalidate_nested_control();
            return false;
        }
        let control = control.lock().unwrap_or_else(|error| error.into_inner());
        if !control.complete {
            drop(control);
            self.invalidate_nested_control();
            return false;
        }
        self.nested_supervisors = control
            .supervisors
            .iter()
            .map(|(boundary, pid)| (*pid, boundary.clone()))
            .collect();
        true
    }

    fn invalidate_nested_control(&mut self) {
        self.control_complete = false;
        // Registration frames are only delegation hints. Once the channel is
        // not complete, none of them can suppress direct cleanup; otherwise a
        // valid-looking frame followed by malformed input could manufacture a
        // surviving delegated branch. The eventual error preserves truthful
        // cleanup-incomplete status on platforms without direct authority.
        self.nested_supervisors.clear();
        self.delegated.clear();
    }

    #[cfg(test)]
    fn observe(&mut self, processes: &[ProcessInfo]) -> bool {
        self.observe_snapshot(processes)
    }

    fn observe_snapshot(&mut self, processes: &[ProcessInfo]) -> bool {
        if !self.control_complete {
            self.record_error(std::io::Error::other(
                "nested-supervisor control protocol became invalid",
            ));
        }
        self.current.clear();
        self.delegated.clear();
        let leader = self.leader;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let mut owned = self
            .members
            .values()
            .map(|member| member.process.identity.clone())
            .collect::<std::collections::BTreeSet<_>>();
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let mut owned = std::collections::BTreeSet::new();
        owned.insert(ProcessIdentity {
            pid: leader,
            start: processes
                .iter()
                .find(|process| process.pid == leader)
                .and_then(|process| process.identity.start),
        });
        // Underscored (not cfg-gated): the lookup keeps the session
        // registry helpers live on every platform; only the Linux/Android
        // reconciliation pass below consumes the result.
        let _boundary = session_boundary(leader);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let session_lease = session_lease_inode(leader);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let parent = std::process::id();
        let mut changed = true;
        while changed {
            changed = false;
            for process in processes {
                if owned.contains(&process.identity) {
                    continue;
                }
                let parent_process = processes
                    .iter()
                    .find(|candidate| candidate.pid == process.parent);
                let inherited =
                    parent_process.is_some_and(|candidate| owned.contains(&candidate.identity));
                #[cfg(any(target_os = "linux", target_os = "android"))]
                let inherited_delegation = parent_process
                    .is_some_and(|candidate| self.delegated.contains(&candidate.identity));
                #[cfg(any(target_os = "linux", target_os = "android"))]
                let direct_adoptee = process.parent == parent;
                // Skip every syscall below (status registry, environ read,
                // fd-table walk) for processes no ownership rule can match.
                // Adoption requires a direct child of this process; the other
                // rules use only snapshot fields. Delegation marking lives
                // inside the ownership insert, so a skipped process cannot
                // gain delegation state either.
                #[cfg(any(target_os = "linux", target_os = "android"))]
                if process.session != leader && !inherited && !direct_adoptee {
                    continue;
                }
                #[cfg(any(target_os = "linux", target_os = "android"))]
                if is_registered_status_child(process.pid) {
                    // Another caller retains this exact wait status. It must
                    // never become a member merely because a dead process no
                    // longer exposes its environment marker.
                    continue;
                }
                #[cfg(any(target_os = "linux", target_os = "android"))]
                let adopted = if direct_adoptee
                    && !is_registered_session_leader(process.pid)
                    && !is_registered_status_child(process.pid)
                {
                    // The pre-filter above already excluded every process
                    // this rule cannot adopt; probing this candidate's
                    // environment marker lazily keeps a full-table environ
                    // scan per fixpoint pass from burning the verification
                    // budget on a busy host.
                    let boundary_match = _boundary
                        .as_deref()
                        .map(|boundary| process_boundary(process.pid, boundary));
                    match boundary_match.as_ref() {
                        None => false,
                        Some(observed) => match observed {
                            BoundaryMatch::Matches | BoundaryMatch::Delegated => true,
                            // An unregistered direct child without our
                            // boundary marker is either a reparented session
                            // orphan that scrubbed its marker or a foreign
                            // child of an embedded host process. No
                            // registration census can distinguish the two, so
                            // claim only with positive file-descriptor proof
                            // that the candidate inherited this session's
                            // lease. Unproven candidates stay unowned: an
                            // orphan that also closed every descriptor is
                            // indistinguishable from foreign and escapes
                            // rather than risk killing foreign processes.
                            BoundaryMatch::Absent
                            | BoundaryMatch::Unknown
                            | BoundaryMatch::ForeignMarker => session_lease.is_some_and(|lease| {
                                process_holds_session_lease(process.pid, lease)
                            }),
                        },
                    }
                } else {
                    false
                };
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                let adopted = false;
                #[cfg(any(target_os = "linux", target_os = "android"))]
                let delegated = !direct_adoptee
                    && (inherited_delegation
                        || parent_process.is_some_and(|parent| {
                            parent.live && self.nested_supervisors.contains_key(&parent.pid)
                        }));
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                let delegated = false;
                if process.session == leader || inherited || adopted {
                    owned.insert(process.identity.clone());
                    if delegated {
                        self.delegated.insert(process.identity.clone());
                    }
                    changed = true;
                }
            }
        }
        for process in processes
            .iter()
            .filter(|process| owned.contains(&process.identity))
        {
            self.current.insert(process.pid, process.clone());
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if process.pid != self.leader {
                // Leaders stay out: their retained handles own their status.
                // Members recorded here authorize the adopted-zombie reaper
                // to consume their status once they exit unreaped.
                record_session_descendant(&process.identity);
                let same_identity = self
                    .members
                    .get(&process.pid)
                    .is_some_and(|member| member.process.identity == process.identity);
                if same_identity {
                    self.members
                        .get_mut(&process.pid)
                        .expect("observed member")
                        .refresh(process);
                } else {
                    match OwnedMember::claim(process) {
                        Ok(Some(member)) => {
                            self.members.insert(process.pid, member);
                        }
                        Ok(None) => {}
                        Err(error) if member_claim_failure_is_fatal(process, self.leader) => {
                            self.record_error(error);
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        // No per-observation print: the sudo-PTY test's helper writes to
        // a small macOS PTY buffer nobody drains, so hot-path diagnostics
        // flow-control the helper into a 15s timeout (Heisenbug).
        let leader_observed = self.current.contains_key(&self.leader);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        for member in self.members.values_mut() {
            if !self.current.contains_key(&member.process.pid) {
                // A claimed member may change session or disappear. Its pidfd
                // keeps authority stable while procfs supplies only current
                // liveness and topology for the pinned numeric identity.
                member.refresh_from_kernel();
            }
        }
        let current_empty = self.current.values().all(|process| !process.live);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return leader_observed
            && current_empty
            && self.members.values().all(|member| !member.process.live);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        (leader_observed && current_empty)
    }

    fn record_error(&mut self, error: std::io::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn signal_group(&mut self, signal: i32) -> bool {
        match signal_group_result(self.leader, signal) {
            Ok(delivered) => delivered,
            Err(error) => {
                self.record_error(error);
                false
            }
        }
    }

    fn signal_before_observation(&mut self, signal: i32) -> InitialDelivery {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        match open_pidfd(self.leader) {
            Ok(PidFd::Open(_)) => {
                // The retained Child prevents leader PID reuse. Delivering to
                // it before the potentially expensive process-table pass
                // keeps cancellation latency bounded; each surviving member
                // discovered afterward receives one exact pidfd delivery.
                signal_pid(self.leader, signal);
                return InitialDelivery::ExactLeader;
            }
            Ok(PidFd::Gone) | Ok(PidFd::Unsupported) => {}
            Err(error) => self.record_error(error),
        }

        InitialDelivery::Group(self.signal_group(signal))
    }

    fn finish_initial_delivery(&mut self, signal: i32, delivery: InitialDelivery) {
        match delivery {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            InitialDelivery::ExactLeader => self.signal_new(signal),
            InitialDelivery::Group(true) => {
                self.note_group_delivery();
                self.signal_new(signal);
            }
            InitialDelivery::Group(false) => {
                signal_pid(self.leader, signal);
                self.signal_new(signal);
            }
        }
    }

    fn note_group_delivery(&mut self) {
        for process in self
            .current
            .values()
            .filter(|process| process.live && process.group == self.leader)
        {
            self.group_delivered.insert(process.identity.clone());
        }
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "android")))]
    fn signal_initial(&mut self, signal: i32) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let complete_exact_cohort = self.current.contains_key(&self.leader)
                && self.current.values().all(|process| {
                    process.pid == self.leader
                        || !process.live
                        || self.delegated.contains(&process.identity)
                        || self
                            .members
                            .get(&process.pid)
                            .is_some_and(|member| member.process.identity == process.identity)
                });
            if complete_exact_cohort {
                // The retained Child pins the leader PID; pidfds pin every
                // pre-snapshot member. Exact delivery leaves a child forked in
                // the observe-to-signal gap untouched until the next snapshot,
                // where it receives one exact TERM instead of group TERM plus
                // a second pidfd TERM.
                signal_pid(self.leader, signal);
                self.signal_new(signal);
                return;
            }
        }

        // Portable/no-pidfd fallback: the retained leader anchors its group,
        // but the kernel exposes no exact identity for every member. One group
        // delivery is safe; a process created in the observation gap may be
        // delivered again when first discovered because exact-once delivery is
        // impossible without stable member authority.
        if self.signal_group(signal) {
            self.note_group_delivery();
        } else {
            signal_pid(self.leader, signal);
        }
        self.signal_new(signal);
    }

    fn signal_new(&mut self, _signal: i32) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let pending = self
                .members
                .iter()
                .filter(|(_, member)| {
                    member.process.live
                        && !member.signaled
                        && !self.delegated.contains(&member.process.identity)
                        && (member.process.group != self.leader
                            || !self.group_delivered.contains(&member.process.identity))
                })
                .map(|(&pid, _)| pid)
                .collect::<Vec<_>>();
            for pid in pending {
                let member = self.members.get_mut(&pid).expect("retained member");
                if !member.signaled {
                    match member.signal(_signal) {
                        Ok(true) => member.signaled = true,
                        Ok(false) => {}
                        Err(error) => {
                            if self.error.is_none() {
                                self.error = Some(error);
                            }
                        }
                    }
                }
            }
        }

        // Never redeliver a catchable group signal merely because a late
        // member lacks exact process authority. That would re-enter every
        // unchanged TERM handler in the group. Pidfd-capable platforms give
        // late members one exact delivery above; portable/old-kernel cleanup
        // leaves an unpinned late member for the anchored group KILL phase.

        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        if self.authority_error.is_none()
            && self
                .current
                .values()
                .any(|process| requires_direct_signal_authority(process, self.leader))
        {
            // Fail closed as the call's error (not a suppressed incomplete
            // outcome): an unpinned member must never be delivered unsafely,
            // and the caller must handle the refusal explicitly.
            set_cleanup_incomplete("signal_new:authority-refusal");
            // TEMP-DIAG-180: remove with the recvmsg diag.
            eprintln!("TEMP-DIAG-180: authority refusal set");
            self.authority_error = Some(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "safe descendant delivery has no stable process authority",
            ));
        }
    }

    fn signal_all(&mut self, signal: i32) {
        let group_delivered = self.signal_group(signal);
        if !group_delivered {
            signal_pid(self.leader, signal);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let errors = self
                .members
                .values()
                .filter(|member| !self.delegated.contains(&member.process.identity))
                .filter(|member| {
                    !group_delivered
                        || requires_direct_signal_authority(&member.process, self.leader)
                })
                .filter_map(|member| member.signal(signal).err())
                .collect::<Vec<_>>();
            for error in errors {
                self.record_error(error);
            }
        }
    }

    fn note_survivors(&mut self) {
        if self.authority_error.is_some() {
            // The refusal already explains the outcome: delivery was never
            // attempted, so no bounded SIGKILL ran and claiming survivors
            // would both misdescribe the run and suppress the refusal into
            // CleanupIncomplete instead of surfacing it as the call's error.
            return;
        }
        let current_live = self.current.values().any(|process| process.live);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let retained_live = self.members.values().any(|member| member.process.live);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let retained_live = false;
        if current_live || retained_live {
            self.record_error(std::io::Error::other(
                "owned subprocesses survived bounded SIGKILL cleanup",
            ));
        } else if self.error.is_none() {
            self.record_error(std::io::Error::other(
                "could not verify owned subprocess cleanup",
            ));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn take_members(&mut self) -> Vec<OwnedMember> {
        std::mem::take(&mut self.members).into_values().collect()
    }
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
        // SIGKILL for stragglers only: a `try_wait` first keeps an
        // already-reaped handle from issuing a kill syscall against
        // a possibly recycled PID (current std short-circuits this,
        // but that is a toolchain behavior, not a contract).
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
    // Reap every handle so no zombie survives cleanup (shell `wait`).
    // Drain into a temp vec: `Child::wait` needs `&mut`, and the
    // registry drops the handles either way.
    let deadline_reap = cleanup_deadline();
    for mut child in children.drain(..) {
        let _ = wait_child_until(&mut child, deadline_reap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_writer_falls_back_when_its_first_primary_send_discovers_failure() {
        use std::io::Write as _;

        resume_outward_writes();
        // An unbound sender fails deterministically. A dropped peer's send
        // can still succeed while another thread's forked-not-yet-exec'd
        // child holds the descriptor table open.
        let primary_sender = std::os::unix::net::UnixDatagram::unbound().unwrap();
        primary_sender.set_nonblocking(true).unwrap();
        let (fallback_sender, fallback_receiver) = internal_datagram_pair().unwrap();
        fallback_sender.set_nonblocking(true).unwrap();
        fallback_receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let primary = std::sync::Arc::new(Mutex::new(ProcessOutputChannel {
            sender: primary_sender,
            failed: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
        }));
        let fallback = std::sync::Arc::new(Mutex::new(ProcessOutputChannel {
            sender: fallback_sender,
            failed: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
        }));
        let mut writer = ProcessRelayWriter {
            channel: Some(primary.clone()),
            fallback: Some(fallback.clone()),
            target: libc::STDERR_FILENO as u8,
            present_at_entry: true,
            failed: false,
        };

        assert_eq!(writer.write(b"diagnostic").unwrap(), 10);
        let mut packet = [0u8; OUTPUT_RELAY_PAYLOAD_BYTES + 1];
        let received = fallback_receiver.recv(&mut packet).unwrap();
        assert_eq!(&packet[..received], b"\x02diagnostic");
        assert!(
            primary
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .failed
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(
            fallback
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .used
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn cancelled_primary_send_does_not_poison_the_stderr_fallback() {
        use std::io::Write as _;

        resume_outward_writes();
        let (primary_sender, _primary_receiver) = internal_datagram_pair().unwrap();
        primary_sender.set_nonblocking(true).unwrap();
        let (fallback_sender, fallback_receiver) = internal_datagram_pair().unwrap();
        fallback_sender.set_nonblocking(true).unwrap();
        fallback_receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let primary = std::sync::Arc::new(Mutex::new(ProcessOutputChannel {
            sender: primary_sender,
            failed: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
        }));
        let fallback = std::sync::Arc::new(Mutex::new(ProcessOutputChannel {
            sender: fallback_sender,
            failed: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
        }));
        let mut writer = ProcessRelayWriter {
            channel: Some(primary.clone()),
            fallback: Some(fallback.clone()),
            target: libc::STDERR_FILENO as u8,
            present_at_entry: true,
            failed: false,
        };

        abort_outward_writes();
        assert!(writer.write_all(b"discarded").is_err());
        resume_outward_writes();
        writer.write_all(b"diagnostic").unwrap();
        let mut packet = [0u8; OUTPUT_RELAY_PAYLOAD_BYTES + 1];
        let received = fallback_receiver.recv(&mut packet).unwrap();
        assert_eq!(&packet[..received], b"\x02diagnostic");
        assert!(
            fallback
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .used
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn nested_registration_is_session_scoped_and_withdraws_on_private_eof() {
        let (receiver, writer) = internal_datagram_pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let state = std::sync::Arc::new(std::sync::Mutex::new(NestedControlState::new()));
        let worker = NestedControlWorker::start(receiver, std::process::id(), state.clone());
        let tokens = (0..64)
            .map(|index| format!("{index:064x}"))
            .collect::<Vec<_>>();
        let mut registrations = Vec::new();
        // TEMP-DIAG-180: remove with the recvmsg diag. Prove whether the
        // test still holds every peer when convergence fails.
        let mut fd_proof = Vec::new();
        for token in &tokens {
            let local =
                publish_nested_supervisor(&[std::os::fd::AsRawFd::as_raw_fd(&writer)], token)
                    .unwrap()
                    .expect("private registration");
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let fd = std::os::fd::AsRawFd::as_raw_fd(&local);
            let ino = if unsafe { libc::fstat(fd, &mut stat) } == 0 {
                stat.st_ino as u64
            } else {
                u64::MAX
            };
            fd_proof.push((fd, ino));
            registrations.push(local);
        }
        let mut iterations = 0u32;
        let converged = poll_until(Instant::now() + Duration::from_secs(1), || {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            iterations += 1;
            // TEMP-DIAG-180: remove with the recvmsg diag.
            if iterations % 10 == 1 {
                eprintln!(
                    "TEMP-DIAG-180: nested-control poll: len={} complete={}",
                    state.supervisors.len(),
                    state.complete,
                );
            }
            Ok((state.supervisors.len() == tokens.len()).then_some(()))
        });
        // TEMP-DIAG-180: remove with the recvmsg diag.
        if let Err(error) = &converged {
            for (index, registration) in registrations.iter().enumerate() {
                let fd = std::os::fd::AsRawFd::as_raw_fd(registration);
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                let (open, ino) = if unsafe { libc::fstat(fd, &mut stat) } == 0 {
                    (true, stat.st_ino as u64)
                } else {
                    (false, u64::MAX)
                };
                let (_, first_ino) = fd_proof[index];
                eprintln!(
                    "TEMP-DIAG-180: test local {index} fd={fd} open={open} same_ino={}",
                    ino == first_ino,
                );
            }
            panic!("nested-control convergence failed: {error:?}");
        }

        let first_token = tokens[0].clone();
        let second_token = tokens[1].clone();
        drop(registrations.remove(0));
        poll_until(Instant::now() + Duration::from_secs(1), || {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            Ok((!state.supervisors.contains_key(&first_token)
                && state.supervisors.contains_key(&second_token))
            .then_some(()))
        })
        .unwrap();
        drop(registrations);
        poll_until(Instant::now() + Duration::from_secs(1), || {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            Ok(state.supervisors.is_empty().then_some(()))
        })
        .unwrap();
        drop(worker);
    }

    #[test]
    fn nested_registration_tick_budget_yields_without_revoking_valid_sessions() {
        use std::os::fd::AsRawFd as _;

        let (receiver, writer) = internal_datagram_pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        // The burst below queues 128 datagrams before the worker starts.
        // Linux buffers absorb that; macOS defaults return ENOBUFS once
        // the small socket buffer fills. Size both ends for the burst so
        // the test exercises the tick budget, not buffer exhaustion.
        // (Kernels silently clamp; the burst needs well under a megabyte.)
        for socket in [&receiver, &writer] {
            let size: libc::c_int = 512 * 1024;
            let size_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            for option in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
                let result = unsafe {
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::SOL_SOCKET,
                        option,
                        (&size as *const libc::c_int).cast(),
                        size_len,
                    )
                };
                assert_eq!(result, 0, "test socket buffer sizing");
            }
        }
        let mut registrations = Vec::new();
        // Hold every sent end until the burst below converges (like
        // publish_nested_supervisor holds until ACK): closing a copy
        // while its datagram still queues corrupts the pending macOS
        // install (duplicate fd numbers, phantom bytes plus HUP).
        let mut sent_ends = Vec::new();
        for index in 0..128 {
            let (local, parent) = internal_stream_pair().unwrap();
            send_nested_registration_with_pid(
                writer.as_raw_fd(),
                &format!("{index:064x}"),
                parent.as_raw_fd(),
                std::process::id(),
            )
            .unwrap();
            sent_ends.push(parent);
            registrations.push(local);
        }

        let state = std::sync::Arc::new(std::sync::Mutex::new(NestedControlState::new()));
        let worker = NestedControlWorker::start(receiver, std::process::id(), state.clone());
        poll_until(Instant::now() + Duration::from_secs(2), || {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.complete {
                return Err(std::io::Error::other(
                    "valid registration burst revoked nested supervision",
                ));
            }
            Ok((state.supervisors.len() == registrations.len()).then_some(()))
        })
        .unwrap();
        drop(registrations);
        drop(worker);
    }

    #[test]
    fn invalid_nested_control_revokes_every_delegation_hint() {
        let mut session = Session::new(u32::MAX - 31);
        session
            .nested_supervisors
            .insert(u32::MAX - 32, "a".repeat(64));
        session.delegated.insert(ProcessIdentity {
            pid: u32::MAX - 33,
            start: Some(1),
        });
        session.invalidate_nested_control();
        assert!(!session.control_complete);
        assert!(session.nested_supervisors.is_empty());
        assert!(session.delegated.is_empty());
    }

    #[test]
    fn nested_registration_rejects_a_spoofed_supervisor_pid() {
        use std::os::fd::AsRawFd as _;

        let (receiver, writer) = internal_datagram_pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let state = std::sync::Arc::new(std::sync::Mutex::new(NestedControlState::new()));
        let worker = NestedControlWorker::start(receiver, std::process::id(), state.clone());
        let (local, parent) = internal_stream_pair().unwrap();
        send_nested_registration_with_pid(
            writer.as_raw_fd(),
            &"c".repeat(64),
            parent.as_raw_fd(),
            std::process::id().saturating_add(1),
        )
        .unwrap();
        // Hold the sent end past the receive below: closing it while its
        // datagram still queues corrupts the pending macOS install.
        let _hold_parent_until_received = parent;

        poll_until(Instant::now() + Duration::from_secs(1), || {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            Ok((!state.complete).then_some(()))
        })
        .unwrap();
        assert!(
            state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .supervisors
                .is_empty()
        );
        drop(local);
        drop(worker);
    }

    #[test]
    fn nested_foreground_child_has_one_nearest_supervisor() {
        const MODE: &str = "DOT_NESTED_FOREGROUND_SUPERVISOR";
        const READY: &str = "DOT_NESTED_FOREGROUND_READY";
        const TERMS: &str = "DOT_NESTED_FOREGROUND_TERMS";
        const TARGET: &str = "DOT_NESTED_FOREGROUND_TARGET";

        if std::env::var_os(MODE).is_some() {
            let signals = Signals::install().unwrap();
            let ready = std::env::var_os(READY).unwrap();
            let terms = std::env::var_os(TERMS).unwrap();
            let target = std::env::var_os(TARGET).unwrap();
            let script = r#"
trap 'printf "15\n" >>"$TERMS"; exit 0' TERM
printf '%s\n' "$$" >"$TARGET"
: >"$READY"
while :; do sleep 0.02; done
"#;
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(script)
                .env("READY", ready)
                .env("TERMS", terms)
                .env("TARGET", target)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let end = supervise_child(command, None, |_| Ok(())).unwrap();
            let success = matches!(end, SessionEnd::Interrupted(libc::SIGTERM));
            let code = signals.finish(i32::from(!success));
            std::process::exit(code);
        }

        let scope = dot_test_support::TempDir::new("nested-foreground-owner").unwrap();
        let ready = scope.path().join("ready");
        let terms = scope.path().join("terms");
        let target = scope.path().join("target");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "cleanup::tests::nested_foreground_child_has_one_nearest_supervisor",
                "--nocapture",
            ])
            .env(MODE, "1")
            .env(READY, &ready)
            .env(TERMS, &terms)
            .env(TARGET, &target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut outer = spawn_owned_session(command).unwrap().unwrap();
        poll_until(Instant::now() + Duration::from_secs(3), || {
            Ok(ready.exists().then_some(()))
        })
        .unwrap();
        let status = outer.stop(libc::SIGTERM).unwrap();
        assert_eq!(status.code(), Some(128 + libc::SIGTERM));
        assert_eq!(std::fs::read_to_string(&terms).unwrap(), "15\n");
        let target_pid = std::fs::read_to_string(&target)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(target_pid as i32, 0) }, -1);

        let shell_scope = dot_test_support::TempDir::new("nested-shell-foreground-owner").unwrap();
        let shell_ready = shell_scope.path().join("ready");
        let shell_terms = shell_scope.path().join("terms");
        let shell_target = shell_scope.path().join("target");
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "\"$1\" --exact cleanup::tests::nested_foreground_child_has_one_nearest_supervisor --nocapture; status=$?; exit \"$status\"",
                "nested-shell",
            ])
            .arg(executable)
            .env(MODE, "1")
            .env(READY, &shell_ready)
            .env(TERMS, &shell_terms)
            .env(TARGET, &shell_target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut outer = spawn_owned_session(command).unwrap().unwrap();
        poll_until(Instant::now() + Duration::from_secs(3), || {
            Ok(shell_ready.exists().then_some(()))
        })
        .unwrap();
        let status = outer.stop(libc::SIGTERM).unwrap();
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert_eq!(std::fs::read_to_string(&shell_terms).unwrap(), "15\n");
        let target_pid = std::fs::read_to_string(&shell_target)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(target_pid as i32, 0) }, -1);
    }

    #[test]
    fn process_registrations_do_not_remove_a_newer_same_pid_owner() {
        let pid = u32::MAX - 17;
        let control = std::sync::Arc::new(std::sync::Mutex::new(NestedControlState::new()));
        let first = register_session_boundary(pid, &"a".repeat(64), control.clone());
        let second = register_session_boundary(pid, &"b".repeat(64), control);
        assert_eq!(
            active_session_boundaries()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&pid)
                .map(std::collections::BTreeMap::len),
            Some(2)
        );
        unregister_session_boundary(pid, first);
        assert_eq!(session_boundary(pid), Some("b".repeat(64)));
        unregister_session_boundary(pid, second);
        assert_eq!(session_boundary(pid), None);

        let first = StatusChildRegistration::new(pid);
        let second = StatusChildRegistration::new(pid);
        drop(first);
        assert!(is_registered_status_child(pid));
        drop(second);
        assert!(!is_registered_status_child(pid));
    }

    #[test]
    fn supervisor_owns_sigchld_but_restores_exec_visible_target_policy() {
        const MODE: &str = "DOT_SIGCHLD_SUPERVISOR_MODE";
        const PROBE: &str = "DOT_SIGCHLD_TARGET_PROBE";
        if let Some(expected) = std::env::var_os(PROBE) {
            let expected = expected.to_string_lossy();
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: query-only sigaction writes initialized local storage.
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) },
                0
            );
            if expected == "ignore" {
                assert_eq!(action.sa_sigaction, libc::SIG_IGN);
            } else {
                assert_eq!(action.sa_sigaction, libc::SIG_DFL);
                #[cfg(any(target_os = "linux", target_os = "android"))]
                assert_ne!(action.sa_flags & libc::SA_NOCLDWAIT, 0);
            }
            return;
        }
        if let Some(mode) = std::env::var_os(MODE) {
            let mode = mode.to_string_lossy();
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = if mode == "ignore" {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            };
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if mode == "nocldwait" {
                action.sa_flags = libc::SA_NOCLDWAIT;
            }
            // SAFETY: install a valid test-local disposition before ownership.
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
                0
            );
            let signals = Signals::install().unwrap();
            let mut exit = Command::new("sh");
            exit.args(["-c", "exit 42"]);
            assert_eq!(run_foreground_status(exit), 42);
            if mode == "ignore" {
                // SIG_IGN is explicitly retained across exec(2), so verify the
                // authorized target observes the caller's policy.
                let mut probe = Command::new(std::env::current_exe().unwrap());
                probe
                    .args([
                        "--exact",
                        "cleanup::tests::supervisor_owns_sigchld_but_restores_exec_visible_target_policy",
                        "--nocapture",
                    ])
                    .env(PROBE, mode.as_ref());
                assert_eq!(run_foreground_status(probe), 0);
            } else {
                // Linux clears SA_NOCLDWAIT on exec even when SIGCHLD is
                // SIG_DFL. Verify the final child-side handoff restores it;
                // exec itself then applies the kernel's documented policy.
                let child = unsafe { libc::fork() };
                assert!(child >= 0);
                if child == 0 {
                    let restored = unsafe { reset_child_signal_dispositions() }.is_ok();
                    let mut observed: libc::sigaction = unsafe { std::mem::zeroed() };
                    let queried =
                        unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut observed) }
                            == 0;
                    let matches = queried
                        && observed.sa_sigaction == libc::SIG_DFL
                        && observed.sa_flags & libc::SA_NOCLDWAIT != 0;
                    unsafe { libc::_exit(i32::from(!(restored && matches))) };
                }
                let mut status = 0;
                assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
            }
            assert_eq!(signals.finish(0), 0);
            return;
        }
        for mode in ["ignore", "nocldwait"] {
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            if mode == "nocldwait" {
                continue;
            }
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::supervisor_owns_sigchld_but_restores_exec_visible_target_policy",
                    "--nocapture",
                ])
                .env(MODE, mode)
                .status()
                .unwrap();
            assert!(status.success(), "SIGCHLD mode {mode} failed: {status:?}");
        }
    }

    #[cfg(target_os = "linux")]
    fn adopted_zombie_command(marker: &Path) -> Command {
        let script = r#"
import os
import sys

child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        descriptor = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        os.write(descriptor, f"{os.getpid()}\n".encode("ascii"))
        os.close(descriptor)
        os._exit(0)
    os._exit(0)
os.waitpid(child, 0)
while True:
    try:
        pid = open(sys.argv[1], encoding="ascii").read().strip()
        stat = open(f"/proc/{pid}/stat", encoding="ascii").read()
    except (FileNotFoundError, ValueError):
        continue
    if ") Z " in stat:
        break
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn pending_signal_fallback_records_and_consumes_without_sigtimedwait() {
        // Platforms without sigtimedwait (macOS, Android) drain through
        // sigpending plus a SIG_IGN round-trip. Block every handled signal
        // on this thread and target it directly so no other thread can
        // consume or observe the pending instance.
        let _signals = Signals::install().unwrap();
        let _blocked = BlockedLaunchSignals::install().unwrap();
        // SAFETY: the mask above blocks SIGTERM on this thread, so the
        // thread-directed signal stays pending for the drain below.
        assert_eq!(
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) },
            0
        );
        drain_pending_signals_without_sigtimedwait();
        assert_eq!(
            INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst),
            libc::SIGTERM
        );
        // SAFETY: sigpending inspects this thread's mask without blocking.
        unsafe {
            let mut pending: libc::sigset_t = std::mem::zeroed();
            assert_eq!(libc::sigpending(&mut pending), 0);
            assert!(
                libc::sigismember(&pending, libc::SIGTERM) <= 0,
                "fallback drain left SIGTERM pending"
            );
        }
    }

    #[test]
    fn signal_finish_keeps_first_signal_during_a_late_burst() {
        const HELPER: &str = "DOT_SIGNAL_FINISH_BURST_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::signal_finish_keeps_first_signal_during_a_late_burst",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "signal-finalization helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // A sender can observe the old latch immediately before close and
        // issue one final TERM after restoration. Make that post-boundary
        // delivery harmless; the test is about signals owned by the guard.
        // SAFETY: SIG_IGN is a valid process disposition in this helper.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        let signals = Signals::install().unwrap();
        // SAFETY: this process installed a handler for this valid signal.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        assert_eq!(signals.received(), Some(libc::SIGHUP));

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let sender_barrier = barrier.clone();
        let sender = std::thread::spawn(move || {
            sender_barrier.wait();
            while INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) != SIGNAL_CLOSED {
                // SAFETY: getpid returns this live process and SIGTERM is valid.
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            }
        });
        barrier.wait();
        let code = signals.finish(0);
        sender.join().unwrap();
        assert_eq!(code, 128 + libc::SIGHUP);
    }

    #[test]
    fn signal_guards_serialize_process_wide_ownership() {
        const HELPER: &str = "DOT_SIGNAL_OWNER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::signal_guards_serialize_process_wide_ownership",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "signal-owner helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let first = Signals::install().unwrap();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let (installed_tx, installed_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let guard = Signals::install().unwrap();
            installed_tx.send(()).unwrap();
            drop(guard);
        });
        attempted_rx.recv().unwrap();
        let installed_while_owned = installed_rx
            .recv_timeout(Duration::from_millis(250))
            .is_ok();
        drop(first);
        if !installed_while_owned {
            installed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        second.join().unwrap();
        assert!(
            !installed_while_owned,
            "overlapping guards replaced the process-global signal owner"
        );
    }

    #[test]
    fn process_signal_guard_keeps_handlers_closed_until_exit() {
        const HELPER: &str = "DOT_PROCESS_SIGNAL_OWNER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::process_signal_guard_keeps_handlers_closed_until_exit",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "process-signal helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let signals = Signals::install_with_restore(false).unwrap();
        // SAFETY: the guard owns both valid handled signals.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        assert_eq!(signals.finish(0), 128 + libc::SIGHUP);
        // A second signal in the small return-to-terminal-handoff window stays
        // pending instead of invoking a restored default disposition.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
    }

    #[test]
    fn terminal_handoff_preserves_every_post_install_signal_and_cleanup_failure() {
        const HELPER: &str = "DOT_TERMINAL_SIGNAL_HANDOFF_HELPER";
        if let Some(value) = std::env::var_os(HELPER) {
            let value = value.to_string_lossy();
            let (signal, incomplete) = value
                .split_once(':')
                .map(|(signal, incomplete)| {
                    (signal.parse::<i32>().unwrap(), incomplete == "incomplete")
                })
                .unwrap();
            let signals = Signals::install_with_restore(false).unwrap();
            if incomplete {
                CLEANUP_INCOMPLETE.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let code = signals.finish(0);
            exit_process_with_hook(code, |installed| {
                if installed == signal {
                    // SAFETY: the terminal handoff has installed this exact
                    // signal and blocked it on this exact thread. A
                    // thread-directed pending signal proves the final
                    // recheck/unblock path rather than delivery to an
                    // unrelated unblocked test-harness thread.
                    assert_eq!(
                        unsafe { libc::pthread_kill(libc::pthread_self(), signal) },
                        0
                    );
                }
            });
        }

        for signal in HANDLED_SIGNALS {
            for incomplete in [false, true] {
                let value = format!(
                    "{signal}:{}",
                    if incomplete { "incomplete" } else { "complete" }
                );
                let status = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "cleanup::tests::terminal_handoff_preserves_every_post_install_signal_and_cleanup_failure",
                        "--nocapture",
                    ])
                    .env(HELPER, value)
                    .status()
                    .unwrap();
                assert_eq!(
                    status.code(),
                    Some(if incomplete {
                        CLEANUP_INCOMPLETE_STATUS
                    } else {
                        128 + signal
                    }),
                    "terminal handoff lost signal {signal} with incomplete={incomplete}"
                );
            }
        }
    }

    #[test]
    fn snapshot_observes_every_session_when_first_remains_active() {
        let mut children = Vec::new();
        for _ in 0..2 {
            let mut command = Command::new("sleep");
            command.arg("30");
            isolate(&mut command);
            children.push(command.spawn().unwrap());
        }
        let mut sessions: Vec<_> = children
            .iter()
            .map(|child| Session::new(child.id()))
            .collect();
        assert_eq!(
            observe_sessions(&mut sessions, Instant::now() + Duration::from_secs(1)),
            Some(false)
        );
        let observed: Vec<_> = sessions
            .iter()
            .map(|session| session.current.contains_key(&session.leader))
            .collect();
        for child in &mut children {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(observed, [true, true]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn normal_exit_bounds_attribution_to_one_session_snapshot() {
        const HELPER: &str = "DOT_SESSION_LEASE_FAST_PATH_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::normal_exit_bounds_attribution_to_one_session_snapshot",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "session-lease helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // The pin is exactly one bounded session snapshot per strict stop,
        // not zero: the earlier zero-snapshot pin encoded the P_PGID-ECHILD
        // probe, which strace proved vacuous (ECHILD matches neither the
        // retained zombie nor any live unstopped child under WSTOPPED-only
        // options, so the snapshot line was dead code and real survivors
        // skipped discovery). Soundness requires verifying the session;
        // detached leaf tools bypass the probe with zero snapshots. Speed
        // is enforced by the timing gate, not by this count.
        reset_global_process_snapshot_calls();
        let result = supervise_session(Command::new("true"), None, |_| Ok(())).unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert_eq!(
            global_process_snapshot_calls(),
            1,
            "normal exit must take exactly one session snapshot"
        );

        if Path::new("/usr/bin/git").is_file() {
            reset_global_process_snapshot_calls();
            let mut git = Command::new("/usr/bin/git");
            git.arg("--version");
            let output =
                run_session_output(git, None, COMMAND_CAPTURE_LIMIT_BYTES, LingerPolicy::Strict)
                    .unwrap();
            assert!(output.status.success());
            assert_eq!(
                global_process_snapshot_calls(),
                1,
                "captured Git exit must take exactly one session snapshot"
            );
        }
    }

    #[test]
    fn lease_survives_env_clear_until_a_pipe_holding_descendant_exits() {
        let mut command = Command::new(dot_test_support::bash());
        command
            .args(["-c", "(/bin/sleep 30) & exit 0", "lease-env-clear"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate(&mut command);
        let mut lease = SessionLease::install(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let registration =
            register_session_boundary(child.id(), &lease.boundary, lease.control_state.clone());
        lease.parent_spawned(child.id(), registration);
        poll_until(Instant::now() + Duration::from_secs(2), || {
            exited(&child).map(|done| done.then_some(()))
        })
        .expect("session leader exited");

        assert!(
            !lease.closed().unwrap(),
            "env_clear discarded the descendant lifetime lease"
        );
        signal_group(child.id(), libc::SIGKILL);
        wait_child_until(&mut child, cleanup_deadline()).unwrap();
        poll_until(cleanup_deadline(), || {
            lease.closed().map(|closed| closed.then_some(()))
        })
        .expect("lease closed after anchored-group teardown");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn escaped_lease_holder_uses_safe_snapshot_cleanup() {
        const HELPER: &str = "DOT_ESCAPED_SESSION_LEASE_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::escaped_lease_holder_uses_safe_snapshot_cleanup",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "escaped-lease helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let scope = dot_test_support::TempDir::new("escaped-session-lease").unwrap();
        let marker = scope.path().join("worker.pid");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "set -m; (trap '' TERM; printf '%s\\n' \"$BASHPID\" >\"$1\"; exec /bin/sleep 30) & while [[ ! -s $1 ]]; do :; done; exit 0",
                "escaped-session-lease",
            ])
            .arg(&marker)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        reset_global_process_snapshot_calls();

        let result = supervise_session(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert!(
            global_process_snapshot_calls() >= 2,
            "an open descendant lease bypassed safe process discovery"
        );
        // SAFETY: the fixture wrote its positive PID; signal zero only probes.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn detach_policy_leaves_short_lived_in_group_holder_running() {
        const HELPER: &str = "DOT_DETACH_IN_GROUP_LEAK_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::detach_policy_leaves_short_lived_in_group_holder_running",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "detach in-group-leak helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let scope = dot_test_support::TempDir::new("detach-in-group-leak").unwrap();
        let marker = scope.path().join("holder.pid");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "(printf '%s\\n' \"$BASHPID\" >\"$1\"; exec /bin/sleep 5) & while [[ ! -s $1 ]]; do :; done; exit 0",
                "detach-in-group-leak",
            ])
            .arg(&marker)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        reset_global_process_snapshot_calls();

        let result = supervise_session_detached(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert_eq!(
            global_process_snapshot_calls(),
            0,
            "a detached open-lease holder must skip host-wide discovery entirely"
        );
        let process = linux_process_info(pid).expect("leaked holder identity");
        assert!(
            process.live,
            "detach policy unexpectedly stopped the in-group lease holder"
        );
        // SAFETY: the fixture wrote its positive PID; the holder is a
        // test-owned sleep this helper reaps so no stray process survives.
        let holder = pid as i32;
        assert_eq!(unsafe { libc::kill(holder, libc::SIGKILL) }, 0);
        let member = OwnedMember::claim(&process)
            .unwrap()
            .expect("stable leaked holder identity");
        assert!(
            reap_owned(
                vec![vec![member]],
                Instant::now() + Duration::from_secs(3),
                wait_member,
            )[0],
            "leaked test holder was not reaped"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn strict_policy_kills_short_lived_in_group_holder_without_discovery() {
        const HELPER: &str = "DOT_STRICT_IN_GROUP_KILL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::strict_policy_kills_short_lived_in_group_holder_without_discovery",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "strict in-group-kill helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let scope = dot_test_support::TempDir::new("strict-in-group-kill").unwrap();
        let marker = scope.path().join("holder.pid");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "(printf '%s\\n' \"$BASHPID\" >\"$1\"; exec /bin/sleep 5) & while [[ ! -s $1 ]]; do :; done; exit 0",
                "strict-in-group-kill",
            ])
            .arg(&marker)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        reset_global_process_snapshot_calls();

        let result = supervise_session(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        // The kill plus bounded verify absorbs the holder without discovery
        // on the fast path; under load the bounded verify may legitimately
        // fall back to one discovery snapshot. The pin is that the holder
        // dies (ESRCH below), not the snapshot count.
        assert!(
            global_process_snapshot_calls() <= 1,
            "the retained-group kill plus bounded verify must absorb an in-group holder without repeated host-wide discovery"
        );
        // SAFETY: the fixture wrote its positive PID; signal zero only probes.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn normal_exit_does_not_claim_a_detached_closed_lease_descendant() {
        const HELPER: &str = "DOT_NORMAL_DETACHED_SUCCESS_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::normal_exit_does_not_claim_a_detached_closed_lease_descendant",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "normal detached-success helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let scope = dot_test_support::TempDir::new("closed-lease-normal").unwrap();
        let marker = scope.path().join("descendant.pid");
        let script = r#"
import os
import subprocess
import sys

child = subprocess.Popen(
    ["/bin/sleep", "1"],
    start_new_session=True,
    close_fds=True,
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
with open(sys.argv[1], "w", encoding="ascii") as output:
    output.write(f"{child.pid}\n")
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        reset_global_process_snapshot_calls();
        let result = supervise_session(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let process = linux_process_info(pid).expect("detached descendant identity");
        let member = OwnedMember::claim(&process)
            .unwrap()
            .expect("stable detached descendant identity");

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert!(
            process.live,
            "normal success unexpectedly claimed the detached descendant"
        );
        // Exactly one bounded session snapshot: the detached descendant
        // owns its session, so the probe verifies and correctly ignores
        // it. The earlier zero pin cited the P_PGID filter, which strace
        // proved vacuous (see normal_exit_bounds_attribution_to_one_session_snapshot).
        assert_eq!(
            global_process_snapshot_calls(),
            1,
            "normal exit must take exactly one session snapshot"
        );
        assert!(
            reap_owned(
                vec![vec![member]],
                Instant::now() + Duration::from_secs(3),
                wait_member,
            )[0],
            "self-bounded detached fixture was not reaped"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn closed_lease_escaped_descendant_is_still_owned_on_cancellation() {
        const HELPER: &str = "DOT_CLOSED_LEASE_CANCEL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::closed_lease_escaped_descendant_is_still_owned_on_cancellation",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "closed-lease cancellation helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let signals = Signals::install().unwrap();
        let scope = dot_test_support::TempDir::new("closed-lease-cancel").unwrap();
        let marker = scope.path().join("descendant.pid");
        let script = r#"
import subprocess
import sys
import time

child = subprocess.Popen(
    ["/bin/sleep", "5"],
    start_new_session=True,
    close_fds=True,
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
with open(sys.argv[1], "w", encoding="ascii") as output:
    output.write(f"{child.pid}\n")
while True:
    time.sleep(1)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker_for_signal = marker.clone();
        let sender = std::thread::spawn(move || {
            // The fixture creates the marker before writing the child PID,
            // so existence alone races the write: signal only once the
            // marker carries a parseable PID.
            poll_until(Instant::now() + Duration::from_secs(2), || {
                let ready = std::fs::read_to_string(&marker_for_signal)
                    .ok()
                    .is_some_and(|text| valid_pid(text.trim()));
                Ok(ready.then_some(()))
            })
            .unwrap();
            // SAFETY: this helper process owns an installed SIGTERM handler.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
        });

        let result = supervise_session(command, None, |_| Ok(())).unwrap();
        sender.join().unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let survived = alive(pid);
        let deadline = Instant::now() + Duration::from_secs(6);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(matches!(result, SessionEnd::Interrupted(libc::SIGTERM)));
        assert!(
            !survived,
            "closed-FD escaped descendant survived cancellation"
        );
        drop(signals);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cancellation_owns_cleared_environment_live_direct_adoptees() {
        const HELPER: &str = "DOT_EMPTY_ENV_ADOPTEE_CANCEL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            for environment in ["empty", "nonempty"] {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "cleanup::tests::cancellation_owns_cleared_environment_live_direct_adoptees",
                        "--nocapture",
                    ])
                    .env(HELPER, environment)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{environment} cleared-environment adoptee helper failed with {:?}:\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }

        adopt_descendants().unwrap();
        let signals = Signals::install().unwrap();
        let scope = dot_test_support::TempDir::new("empty-env-live-adoptee").unwrap();
        let marker = scope.path().join("pids");
        let release = scope.path().join("release");
        let script = r#"
import os
import subprocess
import sys
import time

# The orphan clears its environment but keeps inherited descriptors, so
# the session lease remains the one positive parentage proof. Clearing the
# environment AND closing every descriptor would sever every observable tie:
# no sound supervisor can distinguish such a fully-severed orphan from a
# foreign child of an embedded host, so that corner stays unowned rather
# than risk killing foreign processes.
child_environment = {} if sys.argv[3] == "empty" else {"PATH": "/usr/bin:/bin"}
child = subprocess.Popen(
    ["/bin/sleep", "30"],
    env=child_environment,
    start_new_session=True,
    close_fds=False,
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
with open(sys.argv[1], "w", encoding="ascii") as output:
    output.write(f"{os.getpid()} {child.pid}\n")
while not os.path.exists(sys.argv[2]):
    time.sleep(0.001)
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .arg(&release)
            .arg(std::env::var(HELPER).unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut retained = None;
        let mut signal_sent = false;
        let result = supervise_session(command, None, |_| {
            if signal_sent || !marker.exists() {
                return Ok(());
            }
            let pids = std::fs::read_to_string(&marker)?;
            let mut pids = pids.split_whitespace();
            let (Some(leader), Some(child)) = (
                pids.next().and_then(|pid| pid.parse::<u32>().ok()),
                pids.next().and_then(|pid| pid.parse::<u32>().ok()),
            ) else {
                // The fixture creates the marker before writing both PIDs;
                // a tick landing in that gap retries instead of failing.
                return Ok(());
            };
            std::fs::write(&release, b"release\n")?;
            poll_until(Instant::now() + Duration::from_secs(2), || {
                let leader_zombie = linux_process_info(leader).is_some_and(|process| !process.live);
                let adopted = linux_process_info(child)
                    .is_some_and(|process| process.live && process.parent == std::process::id());
                Ok((leader_zombie && adopted).then_some(()))
            })?;
            let process = linux_process_info(child)
                .ok_or_else(|| std::io::Error::other("fixture adoptee disappeared"))?;
            retained = OwnedMember::claim(&process)?;
            assert!(retained.is_some(), "fixture adoptee could not be pinned");
            signal_sent = true;
            // SAFETY: this recursive helper owns the installed SIGTERM
            // handler. Thread-directed delivery makes the latch visible
            // before the callback returns to the supervisor.
            let delivered = unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) };
            if delivered != 0 {
                return Err(std::io::Error::from_raw_os_error(delivered));
            }
            Ok(())
        })
        .unwrap();

        let retained = retained.expect("stable fixture adoptee identity");
        assert!(matches!(result, SessionEnd::Interrupted(libc::SIGTERM)));
        let after = linux_process_info(retained.process.pid);
        assert!(
            !after.as_ref().is_some_and(|process| {
                process.identity == retained.process.identity && process.live
            }),
            "empty-environment direct adoptee remained live when the wrapper returned: {after:?}; pidfd_live={:?}",
            retained.signal(0)
        );
        drop(signals);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn lease_eof_does_not_leave_an_adopted_double_fork_zombie() {
        const HELPER: &str = "DOT_ADOPTED_ZOMBIE_REAP_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::lease_eof_does_not_leave_an_adopted_double_fork_zombie",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "adopted-zombie helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        adopt_descendants().unwrap();
        let scope = dot_test_support::TempDir::new("lease-adopted-zombie").unwrap();
        let marker = scope.path().join("grandchild.pid");
        let script = r#"
import os
import sys

child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        descriptor = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        os.write(descriptor, f"{os.getpid()}\n".encode("ascii"))
        os.close(descriptor)
        os._exit(0)
    while not os.path.exists(sys.argv[1]) or os.path.getsize(sys.argv[1]) == 0:
        pass
    os._exit(0)
os.waitpid(child, 0)
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let result = supervise_session(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let still_present = Path::new(&format!("/proc/{pid}")).exists();
        if still_present {
            // Keep a failing regression from polluting the test process.
            let mut status = 0;
            // SAFETY: a present adopted zombie is our waitable child.
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        }

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert!(!still_present, "lease EOF bypassed adopted-zombie reaping");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn adopted_zombie_reaping_does_not_wait_for_an_unrelated_status_child() {
        const HELPER: &str = "DOT_ZOMBIE_UNRELATED_STATUS_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::adopted_zombie_reaping_does_not_wait_for_an_unrelated_status_child",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "unrelated-status zombie helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
        let registration = StatusChildRegistration::new(unrelated.id());
        let scope = dot_test_support::TempDir::new("zombie-unrelated-status").unwrap();
        let marker = scope.path().join("zombie.pid");

        let result = supervise_session(adopted_zombie_command(&marker), None, |_| Ok(())).unwrap();
        let zombie = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert!(
            linux_process_info(zombie).is_none(),
            "unrelated live status child blocked adopted-zombie reaping"
        );
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "adopted-zombie reaping disturbed the registered status child"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        drop(registration);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reaper_behind_a_protected_child_reaps_recorded_zombies_without_a_snapshot() {
        const HELPER: &str = "DOT_ZOMBIE_PROTECTED_DRAIN_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::reaper_behind_a_protected_child_reaps_recorded_zombies_without_a_snapshot",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "protected-drain zombie helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let scope = dot_test_support::TempDir::new("zombie-protected-drain").unwrap();
        let marker = scope.path().join("zombie.pid");
        let script = r#"
import os
import sys

child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        with open(sys.argv[1], "w", encoding="ascii") as output:
            output.write(f"{os.getpid()}\n")
        os._exit(0)
    os._exit(0)
os.waitpid(child, 0)
while True:
    try:
        pid = open(sys.argv[1], encoding="ascii").read().strip()
        stat = open(f"/proc/{pid}/stat", encoding="ascii").read()
    except (FileNotFoundError, ValueError):
        continue
    if ") Z " in stat:
        break
os._exit(0)
"#;
        let mut producer = Command::new("/usr/bin/python3")
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .spawn()
            .unwrap();
        assert!(producer.wait().unwrap().success());
        let zombie = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let identity = linux_process_info(zombie)
            .map(|process| process.identity)
            .unwrap();
        record_session_descendant(&identity);

        // A registered waitable child pins waitid(P_ALL) on itself: the
        // reaper cannot advance past it without consuming its status.
        let mut protected = Command::new("true").spawn().unwrap();
        let registration = StatusChildRegistration::new(protected.id());
        let start = Instant::now();
        while !exited(&protected).unwrap() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "protected child never became waitable"
            );
            std::thread::yield_now();
        }

        reset_global_process_snapshot_calls();
        reap_ready_adopted_zombies(Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(
            linux_process_info(zombie).is_none(),
            "recorded zombie behind the protected child was not reaped"
        );
        assert_eq!(
            global_process_snapshot_calls(),
            0,
            "reaper behind a protected child performed a host-wide snapshot"
        );

        protected.wait().unwrap();
        drop(registration);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn portable_snapshot_fallback_reaps_an_adopted_zombie() {
        const HELPER: &str = "DOT_ZOMBIE_PS_FALLBACK_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::portable_snapshot_fallback_reaps_an_adopted_zombie",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "portable-snapshot zombie helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        struct ResetPortableSnapshot;
        impl Drop for ResetPortableSnapshot {
            fn drop(&mut self) {
                FORCE_PROC_SNAPSHOT_UNAVAILABLE.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        adopt_descendants().unwrap();
        FORCE_PROC_SNAPSHOT_UNAVAILABLE.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetPortableSnapshot;
        let scope = dot_test_support::TempDir::new("zombie-ps-fallback").unwrap();
        let marker = scope.path().join("zombie.pid");

        let result = supervise_session(adopted_zombie_command(&marker), None, |_| Ok(())).unwrap();
        let zombie = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert!(
            linux_process_info(zombie).is_none(),
            "the internal ps helper invalidated adopted-zombie ownership"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn portable_snapshot_fails_closed_when_listed_rows_cannot_be_verified() {
        const HELPER: &str = "DOT_PS_ROW_UNAVAILABLE_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::portable_snapshot_fails_closed_when_listed_rows_cannot_be_verified",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "unverifiable-ps-row helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        struct ResetSnapshotFailures;
        impl Drop for ResetSnapshotFailures {
            fn drop(&mut self) {
                FORCE_PROC_SNAPSHOT_UNAVAILABLE.store(false, std::sync::atomic::Ordering::SeqCst);
                FORCE_FALLBACK_PROCESS_INFO_UNAVAILABLE
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        FORCE_PROC_SNAPSHOT_UNAVAILABLE.store(true, std::sync::atomic::Ordering::SeqCst);
        FORCE_FALLBACK_PROCESS_INFO_UNAVAILABLE.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetSnapshotFailures;
        let mut command = Command::new("sleep");
        command.arg("30");
        isolate(&mut command);
        let mut child = command.spawn().unwrap();

        let result = stop_session(&mut child, libc::SIGTERM);

        assert!(
            result.is_err(),
            "an unverifiable ps snapshot certified subprocess cleanup"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn adopted_zombie_reaper_does_not_steal_another_registered_leader() {
        const HELPER: &str = "DOT_CROSS_BOUNDARY_ZOMBIE_REAP_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::adopted_zombie_reaper_does_not_steal_another_registered_leader",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "cross-boundary zombie helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let mut earlier = spawn_owned_session(Command::new("true")).unwrap().unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            earlier.exited().map(|done| done.then_some(()))
        })
        .unwrap();

        let scope = dot_test_support::TempDir::new("cross-boundary-zombie").unwrap();
        let marker = scope.path().join("zombie.pid");
        let script = r#"
import os
import sys

child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        with open(sys.argv[1], "w", encoding="ascii") as output:
            output.write(f"{os.getpid()}\n")
        os._exit(0)
    os._exit(0)
os.waitpid(child, 0)
while True:
    try:
        pid = open(sys.argv[1], encoding="ascii").read().strip()
        stat = open(f"/proc/{pid}/stat", encoding="ascii").read()
    except (FileNotFoundError, ValueError):
        continue
    if ") Z " in stat:
        break
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut later = spawn_owned_session(command).unwrap().unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            later.exited().map(|done| done.then_some(()))
        })
        .unwrap();
        let zombie = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        assert!(later.stop(libc::SIGTERM).unwrap().success());
        assert!(
            linux_process_info(zombie).is_none(),
            "later boundary left its adopted zombie behind a registered leader"
        );
        assert!(
            earlier.exited().unwrap(),
            "another boundary's registered leader status was stolen"
        );
        assert!(earlier.stop(libc::SIGTERM).unwrap().success());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn adopted_zombie_reaper_proceeds_despite_an_unsettled_launch() {
        const HELPER: &str = "DOT_ZOMBIE_LAUNCH_BARRIER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::adopted_zombie_reaper_proceeds_despite_an_unsettled_launch",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "zombie launch-barrier helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let scope = dot_test_support::TempDir::new("zombie-launch-barrier").unwrap();
        let marker = scope.path().join("zombie.pid");
        let script = r#"
import os
import sys

child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        with open(sys.argv[1], "w", encoding="ascii") as output:
            output.write(f"{os.getpid()}\n")
        os._exit(0)
    os._exit(0)
os.waitpid(child, 0)
while True:
    try:
        pid = open(sys.argv[1], encoding="ascii").read().strip()
        stat = open(f"/proc/{pid}/stat", encoding="ascii").read()
    except (FileNotFoundError, ValueError):
        continue
    if ") Z " in stat:
        break
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let session = spawn_owned_session(command).unwrap().unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            session.exited().map(|done| done.then_some(()))
        })
        .unwrap();
        let zombie = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        // An unrelated unsettled launch must neither authorize stealing nor
        // stall the drain: the zombie below is already attributed to this
        // session, so stopping proceeds while the launch is still held.
        let _launch = StatusChildLaunch::begin();
        let mut session = session;
        assert!(session.stop(libc::SIGTERM).unwrap().success());
        drop(_launch);
        assert!(
            linux_process_info(zombie).is_none(),
            "deferred adopted-zombie drain did not finish"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn session_cleanup_does_not_steal_a_registered_status_child() {
        use std::os::unix::process::ExitStatusExt as _;

        const HELPER: &str = "DOT_STATUS_CHILD_OWNERSHIP_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::session_cleanup_does_not_steal_a_registered_status_child",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "status-child ownership helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let mut status_child = Command::new("true").spawn().unwrap();
        let registration = StatusChildRegistration::new(status_child.id());
        poll_until(Instant::now() + Duration::from_secs(2), || {
            exited(&status_child).map(|done| done.then_some(()))
        })
        .unwrap();

        let mut command = Command::new("sleep");
        command.arg("30");
        let mut session = spawn_owned_session(command).unwrap().unwrap();
        let stopped = session.stop(libc::SIGTERM).unwrap();

        assert_eq!(stopped.signal(), Some(libc::SIGTERM));
        assert!(
            status_child.wait().unwrap().success(),
            "isolated-session cleanup consumed another caller's retained status"
        );
        drop(registration);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn in_flight_status_launch_excludes_unknown_adoptee_without_false_failure() {
        const HELPER: &str = "DOT_STATUS_LAUNCH_OWNERSHIP_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::in_flight_status_launch_excludes_unknown_adoptee_without_false_failure",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "status-launch ownership helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let mut unknown = Command::new("true").spawn().unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            exited(&unknown).map(|done| done.then_some(()))
        })
        .unwrap();
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut owned = spawn_owned_session(command).unwrap().unwrap();

        let launch = StatusChildLaunch::begin();
        let processes = process_snapshot(Instant::now() + Duration::from_secs(2)).unwrap();
        let mut observed = Session::new(owned.child().id());
        assert!(!observed.observe_snapshot(&processes));
        assert!(
            observed.error.is_none(),
            "an overlapping registered launch was misclassified as this session"
        );
        assert!(
            !observed.members.contains_key(&unknown.id()),
            "an in-flight status launch lost its potential child to another session"
        );
        drop(launch);

        assert!(unknown.wait().unwrap().success());
        let _ = owned.stop(libc::SIGKILL);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn adopted_zombie_scan_reaps_under_an_overlapping_launch() {
        const HELPER: &str = "DOT_ZOMBIE_LAUNCH_ABA_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::adopted_zombie_scan_reaps_under_an_overlapping_launch",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "zombie launch-epoch helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        adopt_descendants().unwrap();
        let scope = dot_test_support::TempDir::new("zombie-launch-aba").unwrap();
        let marker = scope.path().join("zombie.pid");
        let script = r#"
import os
import sys

child = os.fork()
if child == 0:
    grandchild = os.fork()
    if grandchild == 0:
        with open(sys.argv[1], "w", encoding="ascii") as output:
            output.write(f"{os.getpid()}\n")
        os._exit(0)
    os._exit(0)
os.waitpid(child, 0)
while True:
    try:
        pid = open(sys.argv[1], encoding="ascii").read().strip()
        stat = open(f"/proc/{pid}/stat", encoding="ascii").read()
    except (FileNotFoundError, ValueError):
        continue
    if ") Z " in stat:
        break
os._exit(0)
"#;
        let mut producer = Command::new("/usr/bin/python3")
            .arg("-c")
            .arg(script)
            .arg(&marker)
            .spawn()
            .unwrap();
        assert!(producer.wait().unwrap().success());
        let zombie = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        // A launch overlapping the whole scan must neither authorize stealing
        // nor stall the pass: every candidate is revalidated against live
        // registrations at reap time, which replaces epoch restarts.
        let _launch = StatusChildLaunch::begin();
        scan_and_reap_unregistered_zombies(Instant::now() + Duration::from_secs(2)).unwrap();
        drop(_launch);
        assert!(
            linux_process_info(zombie).is_none(),
            "an overlapping launch stalled the adopted-zombie drain"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn graceful_cleanup_signals_the_leader_once_and_a_late_same_group_child() {
        let scope = dot_test_support::TempDir::new("late-term-child").unwrap();
        let ready = scope.path().join("ready");
        let spawned = scope.path().join("spawned");
        let child_ready = scope.path().join("child-ready");
        let child_term = scope.path().join("child-term");
        let leader_term = scope.path().join("leader-term");
        // Subsecond sleeps bound trap deferral: Bash runs a TERM trap only
        // after its foreground sleep finishes, and a loaded host can deliver
        // TERM deep into a one-second sleep.
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "trap 'printf \"TERM\\n\" >>\"$5\"; if [[ ! -e $2 ]]; then : >\"$2\"; (trap '\"'\"': >\"$4\"; exit 0'\"'\"' TERM; : >\"$3\"; while :; do sleep 0.1; done) & fi' TERM; : >\"$1\"; while :; do sleep 0.1; done",
                "late-term-child",
            ])
            .arg(&ready)
            .arg(&spawned)
            .arg(&child_ready)
            .arg(&child_term)
            .arg(&leader_term)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate(&mut command);
        let mut child = command.spawn().unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            Ok(ready.exists().then_some(()))
        })
        .unwrap();

        let _ = stop_session(&mut child, libc::SIGTERM);

        assert!(
            child_ready.exists(),
            "TERM handler did not spawn its late child"
        );
        assert!(
            child_term.exists(),
            "late same-group child did not receive TERM"
        );
        assert_eq!(
            std::fs::read_to_string(&leader_term)
                .unwrap()
                .lines()
                .count(),
            1,
            "unchanged group leader received TERM more than once"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn no_pidfd_fallback_does_not_reenter_term_for_a_late_group_member() {
        const HELPER: &str = "DOT_NO_PIDFD_LATE_TERM_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::no_pidfd_fallback_does_not_reenter_term_for_a_late_group_member",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "no-pidfd late-TERM helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        struct ResetPidfd;
        impl Drop for ResetPidfd {
            fn drop(&mut self) {
                FORCE_PIDFD_UNAVAILABLE.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        FORCE_PIDFD_UNAVAILABLE.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetPidfd;
        let scope = dot_test_support::TempDir::new("no-pidfd-late-term").unwrap();
        let ready = scope.path().join("ready");
        let spawned = scope.path().join("spawned");
        let child_ready = scope.path().join("child-ready");
        let child_term = scope.path().join("child-term");
        let leader_term = scope.path().join("leader-term");
        // Subsecond sleeps bound trap deferral: Bash runs a TERM trap only
        // after its foreground sleep finishes, and a loaded host can deliver
        // TERM deep into a one-second sleep.
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "trap 'printf \"TERM\\n\" >>\"$5\"; if [[ ! -e $2 ]]; then : >\"$2\"; (trap '\"'\"': >\"$4\"; exit 0'\"'\"' TERM; : >\"$3\"; while :; do sleep 0.1; done) & fi' TERM; : >\"$1\"; while :; do sleep 0.1; done",
                "no-pidfd-late-term",
            ])
            .arg(&ready)
            .arg(&spawned)
            .arg(&child_ready)
            .arg(&child_term)
            .arg(&leader_term)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate(&mut command);
        let mut child = command.spawn().unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            Ok(ready.exists().then_some(()))
        })
        .unwrap();

        let _ = stop_session(&mut child, libc::SIGTERM);

        assert!(
            child_ready.exists(),
            "TERM handler did not spawn its late child"
        );
        assert!(
            !child_term.exists(),
            "portable fallback redelivered TERM to the whole unchanged group"
        );
        assert_eq!(
            std::fs::read_to_string(&leader_term)
                .unwrap()
                .lines()
                .count(),
            1,
            "portable fallback re-entered the leader TERM handler"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn initial_exact_delivery_does_not_double_signal_a_gap_child() {
        use std::io::Write as _;

        let scope = dot_test_support::TempDir::new("term-gap-child").unwrap();
        let child_ready = scope.path().join("child-ready");
        let child_term = scope.path().join("child-term");
        let leader_term = scope.path().join("leader-term");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "trap 'printf \"TERM\\n\" >>\"$3\"' TERM; while :; do if IFS= read -r line; then (trap 'printf \"TERM\\n\" >>\"$2\"' TERM; : >\"$1\"; while :; do sleep 0.05; done) & else sleep 0.05; fi; done",
                "term-gap-child",
            ])
            .arg(&child_ready)
            .arg(&child_term)
            .arg(&leader_term)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate(&mut command);
        let mut child = command.spawn().unwrap();
        let mut session = Session::new(child.id());
        let initial = process_snapshot(Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(!session.observe(&initial));

        child.stdin.as_mut().unwrap().write_all(b"spawn\n").unwrap();
        poll_until(Instant::now() + Duration::from_secs(2), || {
            Ok(child_ready.exists().then_some(()))
        })
        .unwrap();
        session.signal_initial(libc::SIGTERM);
        poll_until(Instant::now() + Duration::from_secs(2), || {
            Ok(leader_term.exists().then_some(()))
        })
        .unwrap();

        let after = process_snapshot(Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(!session.observe(&after));
        session.signal_new(libc::SIGTERM);
        poll_until(Instant::now() + Duration::from_secs(2), || {
            Ok(child_term.exists().then_some(()))
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(
            std::fs::read_to_string(&leader_term)
                .unwrap()
                .lines()
                .count(),
            1,
            "initial leader received more than one TERM"
        );
        assert_eq!(
            std::fs::read_to_string(&child_term)
                .unwrap()
                .lines()
                .count(),
            1,
            "observe-to-delivery gap child received more than one TERM"
        );

        session.signal_all(libc::SIGKILL);
        let pending = session.take_members();
        wait_child_until(&mut child, cleanup_deadline()).unwrap();
        assert!(reap_owned(vec![pending], cleanup_deadline(), wait_member)[0]);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn open_lease_from_a_direct_adoptee_is_cleaned_before_success() {
        let scope = dot_test_support::TempDir::new("unattributed-session-lease").unwrap();
        let marker = scope.path().join("worker.pid");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "setsid bash -c 'unset DOT_OWNED_SESSION_BOUNDARY_V1; trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; exec /bin/sleep 30' lease-worker \"$1\" & while [[ ! -s $1 ]]; do :; done; exit 0",
                "unattributed-session-lease",
            ])
            .arg(&marker)
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let result = supervise_session(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let identity = linux_process_info(pid).map(|process| process.identity);

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while linux_process_info(pid)
            .zip(identity.as_ref())
            .is_some_and(|(process, identity)| process.identity == *identity && process.live)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !linux_process_info(pid)
                .zip(identity.as_ref())
                .is_some_and(|(process, identity)| process.identity == *identity && process.live),
            "self-bounded unattributed lease holder survived its fixture"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn foreign_direct_child_survives_session_teardown() {
        const HELPER: &str = "DOT_FOREIGN_ADOPTEE_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::foreign_direct_child_survives_session_teardown",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "foreign-adoptee helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        struct ForeignChildGuard {
            child: std::process::Child,
        }

        impl Drop for ForeignChildGuard {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        // The helper exclusively owns this process: it is the sole session
        // owner by construction, so epoch-only fallback adoption would claim
        // any unregistered direct child without positive parentage proof. An
        // embedded host (or a test runner) routinely has such foreign
        // children; tearing down one session must never signal them.
        let scope = dot_test_support::TempDir::new("foreign-adoptee").unwrap();
        let terms = scope.path().join("terms");
        let ready = scope.path().join("ready");
        let mut foreign_command = Command::new(dot_test_support::bash());
        foreign_command
            .args([
                "-c",
                "trap 'printf TERM >>\"$1\"' TERM; printf started >\"$2\"; sleep 60",
                "foreign-adoptee",
            ])
            .arg(&terms)
            .arg(&ready)
            .env_remove(SESSION_BOUNDARY_ENV)
            .env_remove(SESSION_LEASE_FDS_ENV)
            .env_remove(SESSION_CONTROL_FDS_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut foreign = ForeignChildGuard {
            child: foreign_command.spawn().unwrap(),
        };
        poll_until(Instant::now() + Duration::from_secs(5), || {
            Ok(ready.exists().then_some(()))
        })
        .unwrap();

        let mut command = Command::new(dot_test_support::bash());
        command
            .args(["-c", "exec /bin/sleep 60", "owned-session"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut owned = spawn_owned_session(command).unwrap().unwrap();
        let status = owned.stop(libc::SIGTERM).unwrap();
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(status.signal(), Some(libc::SIGTERM));

        assert!(
            !terms.exists(),
            "session teardown signaled a foreign direct child"
        );
        assert!(
            matches!(foreign.child.try_wait(), Ok(None)),
            "session teardown killed a foreign direct child"
        );
    }

    /// Snapshot one process's fd table for lease-proof failure messages.
    /// CI-only mismatches without a local reproduction need the observed
    /// link targets, not just the boolean verdict.
    #[cfg(target_os = "linux")]
    fn lease_debug_fd_targets(pid: u32) -> Vec<String> {
        let mut targets = Vec::new();
        if let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let target = std::fs::read_link(entry.path())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|error| format!("<unreadable: {error}>"));
                targets.push(format!("{name}->{target}"));
            }
        }
        targets.sort();
        targets
    }

    #[test]
    #[cfg(unix)]
    fn internal_pairs_set_close_on_exec() {
        use std::os::fd::AsRawFd as _;
        // Pin the CLOEXEC property directly and deterministically (no
        // spawn involved): internal endpoints must never leak into an
        // exec'd child. The lease-proof test below relies on this for
        // its inheriting/plain distinction.
        let (stream_left, stream_right) = internal_stream_pair().unwrap();
        let (dgram_left, dgram_right) = internal_datagram_pair().unwrap();
        for fd in [
            stream_left.as_raw_fd(),
            stream_right.as_raw_fd(),
            dgram_left.as_raw_fd(),
            dgram_right.as_raw_fd(),
        ] {
            assert!(fd >= 3, "internal endpoint aliased stdio: fd {fd}");
            // SAFETY: F_GETFD only reads the descriptor flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "internal endpoint fd {fd} lacks close-on-exec (flags {flags})"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn session_lease_proof_distinguishes_an_inheriting_child() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        struct ReapGuard {
            child: std::process::Child,
        }

        impl Drop for ReapGuard {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        // Spawn the plain child before the lease socket exists: fork
        // copies the parent's descriptor table, so a child forked before
        // the socket exists cannot inherit it through any descriptor-table
        // race with the parallel tests sharing this process. (The writer
        // is additionally CLOEXEC by construction; that property is pinned
        // directly by `internal_pairs_set_close_on_exec` above.)
        let mut plain = ReapGuard {
            child: Command::new("/bin/sleep")
                .arg("30")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        };
        let (_reader, writer) = internal_stream_pair().unwrap();
        let inode = {
            // SAFETY: the writer is a live socket; fstat writes only `stat`.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::fstat(writer.as_raw_fd(), &mut stat) },
                0,
                "fstat the lease writer"
            );
            stat.st_ino as u64
        };
        let writer_fd = writer.as_raw_fd();
        let mut inheriting = Command::new("/bin/sleep");
        inheriting.arg("30").stdin(Stdio::null());
        // SAFETY: only async-signal-safe fcntl calls run after fork; the
        // cleared descriptor stays owned here through spawn.
        unsafe {
            inheriting.pre_exec(move || {
                let flags = libc::fcntl(writer_fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(writer_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut inheriting = ReapGuard {
            child: inheriting.spawn().unwrap(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        poll_until(deadline, || {
            Ok(process_holds_session_lease(inheriting.child.id(), inode).then_some(()))
        })
        .unwrap();
        assert!(
            !process_holds_session_lease(plain.child.id(), inode),
            "a child without the lease descriptor matched the lease proof: pid {} fds {:?} lease socket:[{}] writer fd {}",
            plain.child.id(),
            lease_debug_fd_targets(plain.child.id()),
            inode,
            writer_fd,
        );
        assert!(
            !process_holds_session_lease(u32::MAX - 7, inode),
            "a missing PID matched the lease proof"
        );
        let _ = inheriting.child.kill();
        let _ = inheriting.child.wait();
        let _ = plain.child.kill();
        let _ = plain.child.wait();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn adopted_zombie_reaper_preserves_unrecorded_direct_zombies() {
        // An unregistered direct zombie that no session ever attributed is
        // either foreign (an embedded host's child) or already owned through
        // a retained handle. The reaper must leave its wait status alone;
        // only identities a session actually observed as members may be
        // reaped here.
        let mut foreign = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        poll_until(Instant::now() + Duration::from_secs(5), || {
            exited(&foreign).map(|done| done.then_some(()))
        })
        .unwrap();
        let control = std::sync::Arc::new(std::sync::Mutex::new(NestedControlState::new()));
        let dummy = u32::MAX - 41;
        let registration = register_session_boundary(dummy, &"f".repeat(64), control);
        reap_ready_adopted_zombies(Instant::now() + Duration::from_secs(5)).unwrap();
        unregister_session_boundary(dummy, registration);
        assert!(
            matches!(foreign.try_wait(), Ok(Some(status)) if status.success()),
            "the adopted-zombie reaper stole an unrecorded direct child"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn same_group_probe_does_not_misattribute_a_foreign_stopped_child() {
        struct StopGuard {
            child: std::process::Child,
        }

        impl Drop for StopGuard {
            fn drop(&mut self) {
                // SAFETY: positive PID, no pointer arguments.
                unsafe {
                    libc::kill(self.child.id() as i32, libc::SIGCONT);
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        // A stopped child outside the probed group must not trigger a
        // host-wide scan: parallel sessions STOP-probe at once, and
        // cross-triggered snapshots exhaust the cleanup budget.
        let foreign = StopGuard {
            child: Command::new("/bin/sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        };
        // SAFETY: positive PID, no pointer arguments.
        unsafe {
            libc::kill(foreign.child.id() as i32, libc::SIGSTOP);
        }
        poll_until(Instant::now() + Duration::from_secs(5), || {
            let stat = std::fs::read(format!("/proc/{}/stat", foreign.child.id()))?;
            Ok(stat.windows(4).any(|word| word == b") T ").then_some(()))
        })
        .unwrap();

        let mut command = Command::new("/bin/true");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut session = spawn_owned_session(command).unwrap().unwrap();
        poll_until(Instant::now() + Duration::from_secs(5), || {
            session.exited().map(|done| done.then_some(()))
        })
        .unwrap();
        reset_global_process_snapshot_calls();
        let found = normal_completion_needs_discovery(
            session.child().id(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(!found, "foreign stopped child misread as a group member");
        assert!(
            global_process_snapshot_calls() <= 1,
            "a foreign stopped child triggered repeated host-wide scans"
        );
        assert!(session.stop(libc::SIGTERM).unwrap().success());
    }

    #[test]
    fn session_shutdown_requires_stable_empty_snapshots() {
        let mut consecutive_empty = 0;

        assert!(!sessions_stably_empty(Some(true), &mut consecutive_empty));
        assert_eq!(consecutive_empty, 1);
        assert!(!sessions_stably_empty(Some(false), &mut consecutive_empty));
        assert_eq!(consecutive_empty, 0);
        assert!(!sessions_stably_empty(Some(true), &mut consecutive_empty));
        assert!(!sessions_stably_empty(None, &mut consecutive_empty));
        assert_eq!(consecutive_empty, 0);
        assert!(!sessions_stably_empty(Some(true), &mut consecutive_empty));
        assert!(sessions_stably_empty(Some(true), &mut consecutive_empty));
    }

    #[test]
    fn cleanup_failure_overrides_interrupted_and_timed_out_results() {
        let failed = || Err(std::io::Error::other("injected incomplete cleanup"));
        assert!(matches!(
            end_after_cleanup(failed(), SessionEnd::Interrupted(libc::SIGTERM)),
            SessionEnd::CleanupIncomplete
        ));
        assert!(matches!(
            end_after_cleanup(failed(), SessionEnd::TimedOut),
            SessionEnd::CleanupIncomplete
        ));
    }

    #[test]
    fn cleanup_signal_deadline_and_tick_error_have_one_terminal_precedence() {
        use std::os::unix::process::ExitStatusExt as _;

        const HELPER: &str = "DOT_TERMINAL_PRECEDENCE_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::cleanup_signal_deadline_and_tick_error_have_one_terminal_precedence",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "terminal-precedence helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let signals = Signals::install().unwrap();
        // SAFETY: this helper process exclusively owns the installed handler.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);
        poll_until(Instant::now() + Duration::from_secs(1), || {
            Ok(received_signal().map(|_| ()))
        })
        .unwrap();
        let deadline = Some(Instant::now() - Duration::from_millis(1));
        let tick_error = || Some(std::io::Error::other("injected tick failure"));
        let status = std::process::ExitStatus::from_raw(0);

        let interrupted = decide_after_cleanup(Ok(status), deadline, tick_error(), |status| {
            SessionEnd::Exited(status)
        })
        .unwrap();
        assert!(matches!(interrupted, SessionEnd::Interrupted(libc::SIGINT)));
        let incomplete = decide_after_cleanup(
            Err(std::io::Error::other("injected incomplete cleanup")),
            deadline,
            tick_error(),
            SessionEnd::Exited,
        )
        .unwrap();
        assert!(matches!(incomplete, SessionEnd::CleanupIncomplete));
        assert_eq!(signals.finish(0), CLEANUP_INCOMPLETE_STATUS);
    }

    #[test]
    fn deadline_wins_when_observation_tick_crosses_it_with_an_exited_child() {
        for foreground in [false, true] {
            let scope = dot_test_support::TempDir::new(if foreground {
                "foreground-post-tick-deadline"
            } else {
                "session-post-tick-deadline"
            })
            .unwrap();
            let ready = scope.path().join("ready");
            let release = scope.path().join("release");
            let exited = scope.path().join("exited");
            let mut command = Command::new(dot_test_support::bash());
            command
                .args([
                    "-c",
                    ": >\"$1\"; while [[ ! -e $2 ]]; do sleep 0.001; done; : >\"$3\"; exit 0",
                    "post-tick-deadline",
                ])
                .arg(&ready)
                .arg(&release)
                .arg(&exited)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // The deadline must exceed fixture startup: a tick can only
            // release the child after its readiness marker appears, and a
            // loaded host can start Bash slower than a subsecond bound.
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut released = false;
            let mut tick = |_final_pass: bool| {
                if ready.exists() && !released {
                    std::fs::write(&release, b"go")?;
                    released = true;
                }
                if released {
                    while (!exited.exists() || Instant::now() < deadline)
                        && Instant::now() < deadline + Duration::from_secs(1)
                    {
                        std::thread::yield_now();
                    }
                }
                Ok(())
            };
            let end = if foreground {
                supervise_child(command, Some(deadline), &mut tick).unwrap()
            } else {
                supervise_session(command, Some(deadline), &mut tick).unwrap()
            };

            assert!(
                exited.exists(),
                "fixture child did not exit inside the tick"
            );
            assert!(
                matches!(end, SessionEnd::TimedOut),
                "an exit observed after the absolute deadline won for foreground={foreground}"
            );
        }
    }

    #[test]
    fn prelatched_cancellation_does_not_authorize_a_session_launch() {
        const HELPER: &str = "DOT_PRELATCHED_LAUNCH_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::prelatched_cancellation_does_not_authorize_a_session_launch",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "prelatched launch helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let scope = dot_test_support::TempDir::new("cancelled-session-launch").unwrap();
        let side_effect = scope.path().join("launched");
        let signals = Signals::install().unwrap();
        // SAFETY: this test owns the temporary signal handler guard.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
        poll_until(Instant::now() + Duration::from_secs(1), || {
            Ok(received_signal().map(|_| ()))
        })
        .unwrap();

        let mut command = Command::new(dot_test_support::bash());
        command
            .arg("-c")
            .arg(": >\"$1\"")
            .arg("launch")
            .arg(&side_effect);
        let session = spawn_owned_session(command).unwrap();

        assert!(session.is_none());
        assert!(
            !side_effect.exists(),
            "cancelled suite crossed the launch barrier"
        );
        assert_eq!(signals.finish(0), 128 + libc::SIGTERM);
    }

    #[test]
    fn signal_after_fork_denies_child_exec_authorization() {
        const HELPER: &str = "DOT_POST_FORK_LAUNCH_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::signal_after_fork_denies_child_exec_authorization",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "post-fork launch helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use std::os::unix::process::CommandExt as _;
        let scope = dot_test_support::TempDir::new("post-fork-session-launch").unwrap();
        let side_effect = scope.path().join("launched");
        let signals = Signals::install().unwrap();
        let mut command = Command::new(dot_test_support::bash());
        command
            .arg("-c")
            .arg(": >\"$1\"")
            .arg("launch")
            .arg(&side_effect);
        // SAFETY: kill/getppid are async-signal-safe. The callback runs before
        // the ownership callback, deterministically placing cancellation in
        // the fork-to-authorization window under test.
        unsafe {
            command.pre_exec(|| {
                if libc::kill(libc::getppid(), libc::SIGTERM) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }

        let session = spawn_owned_session(command).unwrap();

        assert!(session.is_none());
        assert!(
            !side_effect.exists(),
            "child exec crossed denied authorization"
        );
        assert_eq!(signals.finish(0), 128 + libc::SIGTERM);
    }

    #[test]
    fn foreground_signal_after_fork_denies_every_target_exec() {
        const HELPER: &str = "DOT_FOREGROUND_POST_FORK_HELPER";
        if let Some(signal) = std::env::var_os(HELPER) {
            use std::os::unix::process::CommandExt as _;

            let signal = signal.to_string_lossy().parse::<i32>().unwrap();
            let scope = dot_test_support::TempDir::new("foreground-post-fork").unwrap();
            let marker = scope.path().join("launched");
            let signals = Signals::install().unwrap();
            let mut command = Command::new(dot_test_support::bash());
            command
                .arg("-c")
                .arg(": >\"$1\"")
                .arg("foreground-launch")
                .arg(&marker);
            // SAFETY: kill/getppid are async-signal-safe. This callback runs
            // before the supervisor's authorization callback and places the
            // signal in the exact fork-to-exec window under test.
            unsafe {
                command.pre_exec(move || {
                    if libc::kill(libc::getppid(), signal) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }

            let result = supervise_child(command, None, |_| Ok(())).unwrap();

            assert!(matches!(result, SessionEnd::Interrupted(observed) if observed == signal));
            assert!(
                !marker.exists(),
                "foreground target crossed denied exec barrier"
            );
            assert_eq!(signals.finish(0), 128 + signal);
            return;
        }

        for signal in HANDLED_SIGNALS {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::foreground_signal_after_fork_denies_every_target_exec",
                    "--nocapture",
                ])
                .env(HELPER, signal.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "foreground post-fork signal {signal} helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn child_pending_signals_use_default_disposition_before_exec() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

        const HELPER: &str = "DOT_CHILD_PENDING_SIGNAL_HELPER";
        if let Some(value) = std::env::var_os(HELPER) {
            let value = value.to_string_lossy();
            let (kind, signal) = value.split_once(':').unwrap();
            let signal = signal.parse::<i32>().unwrap();
            let scope = dot_test_support::TempDir::new("child-pending-signal").unwrap();
            let marker = scope.path().join("execed");
            let signals = Signals::install().unwrap();
            let mut command = Command::new(dot_test_support::bash());
            command
                .arg("-c")
                .arg(": >\"$1\"")
                .arg("child-pending")
                .arg(&marker);
            // SAFETY: pthread_self/pthread_kill are async-signal-safe. The
            // spawning thread blocked every handled signal before fork, so
            // this signal remains pending until the authorization callback
            // restores default dispositions and the original mask.
            unsafe {
                command.pre_exec(move || {
                    let error = libc::pthread_kill(libc::pthread_self(), signal);
                    if error == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::from_raw_os_error(error))
                    }
                });
            }
            let end = if kind == "foreground" {
                supervise_child(command, None, |_| Ok(())).unwrap()
            } else {
                supervise_session(command, None, |_| Ok(())).unwrap()
            };

            assert!(
                matches!(end, SessionEnd::Exited(status) if status.signal() == Some(signal)),
                "pending child signal did not retain its default disposition"
            );
            assert!(
                !marker.exists(),
                "pending child signal was erased before exec"
            );
            assert_eq!(signals.finish(0), 0, "child signal polluted parent latch");
            return;
        }

        for kind in ["foreground", "session"] {
            for signal in HANDLED_SIGNALS {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "cleanup::tests::child_pending_signals_use_default_disposition_before_exec",
                        "--nocapture",
                    ])
                    .env(HELPER, format!("{kind}:{signal}"))
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{kind} pending child signal {signal} failed with {:?}:\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn old_kernel_without_pidfds_cleans_anchored_group_and_rejects_escape() {
        const HELPER: &str = "DOT_NO_PIDFD_RUNTIME_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cleanup::tests::old_kernel_without_pidfds_cleans_anchored_group_and_rejects_escape",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "no-pidfd runtime helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        FORCE_PIDFD_UNAVAILABLE.store(true, std::sync::atomic::Ordering::SeqCst);
        let scope = dot_test_support::TempDir::new("no-pidfd-runtime").unwrap();
        let grouped_pid = scope.path().join("grouped.pid");
        let mut grouped = Command::new(dot_test_support::bash());
        grouped
            .args([
                "-c",
                "(trap '' TERM; printf '%s\\n' \"$BASHPID\" >\"$1\"; while :; do sleep 0.05; done) & while [[ ! -s $1 ]]; do sleep 0.01; done; exit 0",
                "no-pidfd-grouped",
            ])
            .arg(&grouped_pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let grouped_end = supervise_session(grouped, None, |_| Ok(())).unwrap();
        assert!(matches!(grouped_end, SessionEnd::Exited(status) if status.success()));
        let grouped_pid = std::fs::read_to_string(grouped_pid)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(
            linux_process_info(grouped_pid).is_none(),
            "same-group no-pidfd descendant remained as an adopted zombie"
        );

        let escaped_pid = scope.path().join("escaped.pid");
        let mut escaped = Command::new(dot_test_support::bash());
        escaped
            .args([
                "-c",
                "setsid bash -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; sleep 2' no-pidfd-escaped \"$1\" & while [[ ! -s $1 ]]; do sleep 0.01; done; exit 0",
                "no-pidfd-parent",
            ])
            .arg(&escaped_pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let escaped_end = supervise_session(escaped, None, |_| Ok(())).unwrap();
        assert!(matches!(escaped_end, SessionEnd::CleanupIncomplete));
        let escaped_pid = std::fs::read_to_string(escaped_pid)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        poll_until(Instant::now() + Duration::from_secs(4), || {
            Ok(
                (!linux_process_info(escaped_pid).is_some_and(|process| process.live))
                    .then_some(()),
            )
        })
        .unwrap();
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn four_step_session_escape_is_a_documented_known_limit() {
        // Fresh-review-B P2-3 RISK ACCEPTANCE (not a fix): a descendant
        // that (1) double-forks past an exited intermediate parent,
        // (2) setsids out of the leader's session and group, (3)
        // closes every inherited fd including the lease and capture
        // pipes, and (4) execs with a scrubbed environment is
        // indistinguishable from a legitimate isolated process — so
        // normal completion reports SUCCESS with a live survivor.
        // Killing unprovable adoptees would risk foreign kills in
        // embedded mode, and any census false-positives on test
        // fixtures; the impact stays bounded (a same-uid stray
        // holding no fds, locks, or pipes). This test pins that
        // documented behavior, including the fast path (exactly one
        // session-probe snapshot, no discovery).
        let scope = dot_test_support::TempDir::new("four-step-escape").unwrap();
        let pid_path = scope.path().join("escapee.pid");
        let program = r#"
import os
import sys

pid_path = sys.argv[1]
first = os.fork()
if first == 0:
    second = os.fork()
    if second == 0:
        os.setsid()
        maximum = os.sysconf("SC_OPEN_MAX")
        if not isinstance(maximum, int) or maximum < 3:
            maximum = 65536
        os.closerange(3, maximum)
        with open(pid_path, "w", encoding="utf-8") as output:
            output.write(str(os.getpid()))
        os.execvpe("sleep", ["sleep", "30"], {})
    os._exit(0)
os.waitpid(first, 0)
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-I", "-S", "-c", program])
            .arg(&pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        reset_global_process_snapshot_calls();
        let started = Instant::now();
        let end = supervise_session(command, None, |_| Ok(())).unwrap();
        let snapshots = global_process_snapshot_calls();
        let pid = read_pidfile(&pid_path) as u32;
        let survivor = linux_process_info(pid).is_some_and(|process| process.live);

        assert!(
            matches!(end, SessionEnd::Exited(status) if status.success()),
            "four-step escape must report the documented success, got {end:?}"
        );
        assert_eq!(
            snapshots, 1,
            "four-step escape must take the fast path (one session probe, no discovery)"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "four-step escape completion stalled"
        );
        assert!(
            survivor,
            "four-step escapee did not survive (documented known limit)"
        );
        reap_fixture_daemon(&pid_path);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn normal_success_stops_same_group_descendant_that_closed_every_lease() {
        let scope = dot_test_support::TempDir::new("closed-lease-same-group").unwrap();
        let pid_path = scope.path().join("child.pid");
        let term_path = scope.path().join("child.term");
        let program = r#"
import os
import signal
import sys
import time

pid_path, term_path = sys.argv[1:]
child = os.fork()
if child == 0:
    def stop(_signum, _frame):
        with open(term_path, "a", encoding="utf-8") as output:
            output.write("TERM\n")
        os._exit(0)
    signal.signal(signal.SIGTERM, stop)
    with open(pid_path, "w", encoding="utf-8") as output:
        output.write(str(os.getpid()))
    maximum = os.sysconf("SC_OPEN_MAX")
    if not isinstance(maximum, int) or maximum < 3:
        maximum = 65536
    os.closerange(3, maximum)
    time.sleep(4)
    os._exit(0)
while not os.path.exists(pid_path):
    time.sleep(0.005)
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-I", "-S", "-c", program])
            .arg(&pid_path)
            .arg(&term_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let started = Instant::now();
        let end = supervise_session(command, None, |_| Ok(())).unwrap();
        let pid = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let survived_return = linux_process_info(pid).is_some_and(|process| process.live);
        if survived_return {
            poll_until(Instant::now() + Duration::from_secs(5), || {
                Ok((!linux_process_info(pid).is_some_and(|process| process.live)).then_some(()))
            })
            .unwrap();
        }

        assert!(
            matches!(end, SessionEnd::Exited(status) if status.success()),
            "unexpected same-group completion: {end:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "same-group descendant reached its self-bound"
        );
        assert!(
            !survived_return,
            "same-group descendant survived completion"
        );
        assert_eq!(std::fs::read_to_string(term_path).unwrap(), "TERM\n");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn normal_success_detects_same_group_grandchild_behind_a_moved_direct_child() {
        let scope = dot_test_support::TempDir::new("split-group-closed-lease").unwrap();
        let parent_pid = scope.path().join("parent.pid");
        let child_pid = scope.path().join("child.pid");
        let parent_term = scope.path().join("parent.term");
        let child_term = scope.path().join("child.term");
        let program = r#"
import os
import signal
import sys
import time

parent_pid, child_pid, parent_term, child_term = sys.argv[1:]
branch = os.fork()
if branch == 0:
    member = os.fork()
    if member == 0:
        def stop_member(_signum, _frame):
            with open(child_term, "a", encoding="utf-8") as output:
                output.write("TERM\n")
            os._exit(0)
        signal.signal(signal.SIGTERM, stop_member)
        with open(child_pid, "w", encoding="utf-8") as output:
            output.write(str(os.getpid()))
        maximum = os.sysconf("SC_OPEN_MAX")
        if not isinstance(maximum, int) or maximum < 3:
            maximum = 65536
        os.closerange(3, maximum)
        time.sleep(4)
        os._exit(0)
    os.setpgid(0, 0)
    def stop_parent(_signum, _frame):
        with open(parent_term, "a", encoding="utf-8") as output:
            output.write("TERM\n")
        os._exit(0)
    signal.signal(signal.SIGTERM, stop_parent)
    with open(parent_pid, "w", encoding="utf-8") as output:
        output.write(str(os.getpid()))
    maximum = os.sysconf("SC_OPEN_MAX")
    if not isinstance(maximum, int) or maximum < 3:
        maximum = 65536
    os.closerange(3, maximum)
    time.sleep(4)
    os._exit(0)
while not (os.path.exists(parent_pid) and os.path.exists(child_pid)):
    time.sleep(0.005)
os._exit(0)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-I", "-S", "-c", program])
            .arg(&parent_pid)
            .arg(&child_pid)
            .arg(&parent_term)
            .arg(&child_term)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let started = Instant::now();
        let end = supervise_session(command, None, |_| Ok(())).unwrap();
        let pids = [&parent_pid, &child_pid].map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap()
        });
        let survived_return = pids
            .iter()
            .any(|pid| linux_process_info(*pid).is_some_and(|process| process.live));
        if survived_return {
            poll_until(Instant::now() + Duration::from_secs(5), || {
                Ok((!pids
                    .iter()
                    .any(|pid| linux_process_info(*pid).is_some_and(|process| process.live)))
                .then_some(()))
            })
            .unwrap();
        }

        assert!(
            matches!(end, SessionEnd::Exited(status) if status.success()),
            "unexpected split-group completion: {end:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "split-group descendants reached their self-bound"
        );
        assert!(
            !survived_return,
            "split-group descendants survived completion"
        );
        assert_eq!(std::fs::read_to_string(parent_term).unwrap(), "TERM\n");
        assert_eq!(std::fs::read_to_string(child_term).unwrap(), "TERM\n");
    }

    #[test]
    fn session_snapshot_without_its_retained_leader_is_incomplete() {
        let mut session = Session::new(123);

        assert!(!session.observe(&[]));
    }

    #[test]
    fn snapshot_wave_consumes_one_absolute_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(250);
        for _ in 0..3 {
            let mut command = Command::new("sleep");
            command.arg("30");
            assert!(snapshot(command, deadline).is_none());
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "each snapshot renewed the cancellation budget"
        );
    }

    #[test]
    fn final_wait_poll_stops_at_its_absolute_deadline() {
        let started = Instant::now();
        let result = poll_until(started + Duration::from_millis(40), || {
            Ok::<_, std::io::Error>(None::<()>)
        });

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a final observation renewed or ignored its deadline"
        );
    }

    #[test]
    fn snapshot_collects_successful_process_output() {
        let mut command = Command::new(dot_test_support::bash());
        command.args(["-c", "printf 'snapshot\\n'"]);
        assert_eq!(
            snapshot(command, Instant::now() + Duration::from_secs(1)),
            Some(b"snapshot\n".to_vec())
        );
    }

    #[test]
    fn portable_snapshot_rejects_every_malformed_nonblank_row() {
        assert_eq!(
            parse_ps_snapshot(b"\n123 1 122 121 S\n124 1 122 121 Z\n"),
            Some(vec![
                ProcessInfo {
                    pid: 123,
                    parent: 1,
                    group: 122,
                    session: 121,
                    live: true,
                    identity: ProcessIdentity {
                        pid: 123,
                        start: None,
                    },
                },
                ProcessInfo {
                    pid: 124,
                    parent: 1,
                    group: 122,
                    session: 121,
                    live: false,
                    identity: ProcessIdentity {
                        pid: 124,
                        start: None,
                    },
                },
            ])
        );
        assert_eq!(parse_ps_snapshot(b"123 S\n"), None);
        assert_eq!(parse_ps_snapshot(b"123 1 123 123 Z\nmalformed\n"), None);
        assert_eq!(parse_ps_snapshot(b"123 1 123 123 S extra\n"), None);
    }

    #[test]
    fn macos_membership_reads_live_session_identity() {
        let pid = std::process::id();
        let (group, session) = macos_row_membership(pid, false)
            .expect("live lookup")
            .expect("live present");
        assert!(group > 0, "live group must be resolved, got {group}");
        assert!(session > 0, "live session must be resolved, got {session}");
    }

    #[test]
    fn macos_membership_keeps_zombie_rows_without_session_identity() {
        // A reaped child is the portable stand-in for ESRCH lookups:
        // Darwin answers ESRCH for getpgid/getsid on zombies too, and
        // dropping those rows starves the leader-observed proof on macOS.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        assert_eq!(
            macos_row_membership(pid, true).expect("zombie lookup"),
            Some((pid, pid)),
            "zombie rows keep a self-referential pair"
        );
        assert_eq!(
            macos_row_membership(pid, false).expect("gone lookup"),
            None,
            "vanished non-zombie rows stay unrelated churn"
        );
    }

    #[test]
    fn supervisor_reaps_the_session_when_a_tick_fails() {
        let scope = dot_test_support::TempDir::new("supervisor-tick-error").unwrap();
        let ready = scope.path().join("ready");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "trap '' TERM; printf '%s\\n' \"$$\" >\"$1\"; while :; do sleep 0.05; done",
                "supervisor-tick-error",
            ])
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut failed_ticks = 0;
        let result = supervise_session(command, None, |_| {
            if ready.exists() {
                failed_ticks += 1;
                Err(std::io::Error::other("injected tick failure"))
            } else {
                Ok(())
            }
        });
        let pid = std::fs::read_to_string(&ready)
            .expect("supervisor child pid")
            .trim()
            .parse::<i32>()
            .expect("numeric supervisor child pid");

        assert_eq!(result.unwrap_err().to_string(), "injected tick failure");
        assert!(
            failed_ticks >= 2,
            "session teardown stopped draining after the first output error"
        );
        // SAFETY: a positive PID and signal zero only test existence.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn cooperative_supervisor_allows_child_cleanup_when_a_tick_fails() {
        let scope = dot_test_support::TempDir::new("cooperative-tick-error").unwrap();
        let descendant = scope.path().join("descendant.pid");
        let ready = scope.path().join("ready");
        let cleaned = scope.path().join("cleaned");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "(trap '' TERM; exec sleep 4) & child=$!; printf '%s\\n' \"$child\" >\"$1\"; trap 'kill -KILL \"$child\" 2>/dev/null; wait \"$child\" 2>/dev/null; : >\"$3\"; exit 0' TERM; printf ready >\"$2\"; wait \"$child\"",
                "cooperative-tick-error",
            ])
            .arg(&descendant)
            .arg(&ready)
            .arg(&cleaned)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut failed_ticks = 0;
        let result = supervise_child(command, None, |_| {
            if std::fs::metadata(&ready).is_ok_and(|metadata| metadata.len() > 0) {
                failed_ticks += 1;
                Err(std::io::Error::other("injected output failure"))
            } else {
                Ok(())
            }
        });
        let pid = std::fs::read_to_string(&descendant)
            .expect("cooperative descendant pid")
            .trim()
            .parse::<i32>()
            .expect("numeric cooperative descendant pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survived = alive(pid);

        assert_eq!(result.unwrap_err().to_string(), "injected output failure");
        assert!(
            failed_ticks >= 2,
            "teardown stopped draining after the first output error"
        );
        assert!(cleaned.exists(), "cooperative TERM cleanup did not run");
        assert!(!survived, "tick failure leaked a provider descendant");
    }

    #[test]
    fn stopped_cooperative_child_runs_term_cleanup_before_escalation() {
        let scope = dot_test_support::TempDir::new("stopped-cooperative-child").unwrap();
        let ready = scope.path().join("ready");
        let cleaned = scope.path().join("cleaned");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "(trap '' TERM; sleep 4) & worker=$!; trap 'kill -KILL \"$worker\" 2>/dev/null; : >\"$2\"; exit 0' TERM; : >\"$1\"; kill -STOP \"$$\"; wait \"$worker\"",
                "stopped-cooperative-child",
            ])
            .arg(&ready)
            .arg(&cleaned)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = OwnedChild::new(command.spawn().unwrap());
        poll_until(Instant::now() + Duration::from_secs(2), || {
            Ok(ready.exists().then_some(()))
        })
        .expect("stopped provider fixture became ready");

        let status = child.stop_with_tick(libc::SIGTERM, &mut || Ok(())).unwrap();

        assert!(
            status.success(),
            "provider TERM handler did not exit cleanly"
        );
        assert!(
            cleaned.exists(),
            "stopped provider never ran its TERM handler"
        );
    }

    /// Build a benign-daemon fixture: the leader records its PID,
    /// spawns a lease-inheriting daemon (double-fork equivalent via
    /// backgrounding, no closefrom — the ssh-mux-master shape), and
    /// exits with `exit_code` immediately. The daemon self-bounds
    /// with `sleep 8` so a supervision hang can never outlive the
    /// test by more than seconds.
    fn daemonizing_fixture(pidfile: &Path, daemon_pidfile: &Path, exit_code: i32) -> Command {
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "printf '%s\\n' \"$$\" >\"$1\"; bash -c 'printf \"%s\\n\" \"$$\" >\"$1\"; exec sleep 8' daemon \"$2\" & exit \"$3\"",
                "benign-daemon",
            ])
            .arg(pidfile)
            .arg(daemon_pidfile)
            .arg(exit_code.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn read_pidfile(pidfile: &Path) -> i32 {
        poll_until(Instant::now() + Duration::from_secs(5), || {
            Ok(std::fs::read_to_string(pidfile)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok().filter(|pid| *pid > 0)))
        })
        .expect("fixture pidfile")
    }

    /// SIGKILL a fixture daemon and wait until it is gone (bounded).
    /// Orphaned daemons reparent to the subreaper test process on
    /// Linux, so a bare kill-zero poll would observe the adopted
    /// zombie forever: the Linux path reaps it with an
    /// identity-validated `waitpid` (a recycled PID is never
    /// touched). Other platforms reparent to init, which reaps.
    fn reap_fixture_daemon(daemon_pidfile: &Path) {
        let pid = read_pidfile(daemon_pidfile);
        // SAFETY: the pidfile names the test's own daemon.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let identity = linux_process_info(pid as u32).map(|process| process.identity);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match linux_process_info(pid as u32) {
                    Some(process) if Some(process.identity.clone()) == identity && process.live => {
                        assert!(
                            Instant::now() < deadline,
                            "daemonizing fixture leaked its daemon"
                        );
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Some(process) if Some(process.identity.clone()) == identity => {
                        // Our daemon, now a zombie: collect it. The
                        // identity match proves this PID is still
                        // ours, so no foreign status is stolen.
                        let mut status = 0;
                        // SAFETY: WNOHANG never blocks; the PID names
                        // our identity-verified adopted child.
                        unsafe {
                            libc::waitpid(pid, &mut status, libc::WNOHANG);
                        }
                        break;
                    }
                    // Gone, or recycled by a foreign process (our
                    // daemon is dead either way; never touch it).
                    _ => break,
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!alive(pid), "daemonizing fixture leaked its daemon");
        }
    }

    #[test]
    fn foreground_passthrough_trusts_leader_with_lease_holding_daemon() {
        // Fresh-review-A P2-2 RED pin: a successful foreground leader
        // whose benign daemon (ssh-mux-master shape) holds the lease
        // past the deadline reports the LEADER status, not 125 —
        // shell parity for cooperative foreground children. A failed
        // leader is trusted the same way.
        for exit_code in [0, 3] {
            let scope = dot_test_support::TempDir::new("foreground-benign-daemon").unwrap();
            let pidfile = scope.path().join("leader.pid");
            let daemon_pidfile = scope.path().join("daemon.pid");
            let command = daemonizing_fixture(&pidfile, &daemon_pidfile, exit_code);
            let status = run_foreground_status(command);
            // The lease-hold premise: the daemon must still be alive
            // (proving the lease was open past the deadline).
            let daemon = read_pidfile(&daemon_pidfile);
            assert!(
                alive(daemon),
                "daemon exited before the supervision window closed"
            );
            assert_eq!(status, exit_code);
            reap_fixture_daemon(&daemon_pidfile);
        }
    }

    #[test]
    fn foreground_strict_provider_contract_keeps_125_on_open_lease() {
        // Fresh-review-A P2-2 companion pin: the PROVIDER foreground
        // path (`supervise_child`, explicit ownership contract) stays
        // strict — an open lease past leader exit is CleanupIncomplete
        // (125), never the leader status.
        let scope = dot_test_support::TempDir::new("foreground-strict-lease").unwrap();
        let pidfile = scope.path().join("leader.pid");
        let daemon_pidfile = scope.path().join("daemon.pid");
        let command = daemonizing_fixture(&pidfile, &daemon_pidfile, 0);
        let end = supervise_child(command, None, |_| Ok(())).unwrap();
        let daemon = read_pidfile(&daemon_pidfile);
        assert!(
            alive(daemon),
            "daemon exited before the supervision window closed"
        );
        assert!(
            matches!(end, SessionEnd::CleanupIncomplete),
            "provider path must stay strict on an open lease, got {end:?}"
        );
        reap_fixture_daemon(&daemon_pidfile);
    }

    /// LD_PRELOAD shim sources: interposed `kill(2)` logs every
    /// `(pid, signal)` to `$KILL_LOG`, then calls through. Proves
    /// the no-signal-after-reap invariant with zero syscalls.
    #[cfg(target_os = "linux")]
    const KILL_COUNTER_SHIM: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <unistd.h>
int kill(pid_t pid, int sig) {
    static int (*real_kill)(pid_t, int) = 0;
    if (!real_kill) {
        real_kill = dlsym(RTLD_NEXT, "kill");
    }
    const char *log = getenv("KILL_LOG");
    if (log) {
        char comm[64] = {0};
        FILE *selfstat = fopen("/proc/self/comm", "r");
        if (selfstat) {
            size_t n = fread(comm, 1, sizeof(comm) - 1, selfstat);
            comm[n] = 0;
            fclose(selfstat);
            char *nl = comm;
            while (*nl && *nl != '\n') {
                nl++;
            }
            *nl = 0;
        }
        FILE *file = fopen(log, "a");
        if (file) {
            fprintf(file, "%s[%d] %d %d\n", comm, (int)getpid(), (int)pid, sig);
            fclose(file);
        }
    }
    return real_kill(pid, sig);
}
"#;

    #[test]
    #[cfg(target_os = "linux")]
    fn foreground_reaped_child_receives_no_signals() {
        const HELPER: &str = "DOT_FOREGROUND_NO_SIGNAL_AFTER_REAP";
        const PIDFILE: &str = "DOT_FOREGROUND_NO_SIGNAL_PIDFILE";
        const DAEMON_PIDFILE: &str = "DOT_FOREGROUND_NO_SIGNAL_DAEMON";
        const RESULT: &str = "DOT_FOREGROUND_NO_SIGNAL_RESULT";
        if std::env::var_os(HELPER).is_some() {
            let pidfile = PathBuf::from(std::env::var_os(PIDFILE).unwrap());
            let daemon_pidfile = PathBuf::from(std::env::var_os(DAEMON_PIDFILE).unwrap());
            let result = PathBuf::from(std::env::var_os(RESULT).unwrap());
            let command = daemonizing_fixture(&pidfile, &daemon_pidfile, 0);
            let started = Instant::now();
            let end = supervise_child(command, None, |_| Ok(())).unwrap();
            let elapsed_ms = started.elapsed().as_millis();
            assert!(
                matches!(end, SessionEnd::CleanupIncomplete),
                "strict helper must observe the open lease, got {end:?}"
            );
            std::fs::write(&result, elapsed_ms.to_string()).unwrap();
            // The helper is a subreaper (every supervision adopts),
            // so the orphaned daemon is its own child: collect it
            // here so `strace -f` terminates with the helper.
            reap_fixture_daemon(&daemon_pidfile);
            return;
        }

        // Fresh-review-A P2-1 RED pin: after `try_wait` reaps the
        // leader, the PID is free through the ~1s lease poll — the
        // stop path must send NO signals and NO kill to it (any of
        // them could strike an unrelated recycled PID). Two
        // independent counters prove zero kills to the reaped
        // leader: the LD_PRELOAD shim interposes libc `kill`
        // (always available — `cc` builds this crate's build
        // script), and `strace` counts true `kill(2)` syscalls
        // including std's direct-syscall `Child::kill`, which
        // bypasses libc PLT interposition (observed: pre-fix
        // LD_PRELOAD sees 2 kills, strace sees 3). The elapsed
        // bound pins the no-grace-stall behavior (one lease poll,
        // no second poll, no grace loop).
        let scope = dot_test_support::TempDir::new("foreground-no-signal").unwrap();
        let shim_source = scope.path().join("killcount.c");
        let shim_object = scope.path().join("killcount.so");
        std::fs::write(&shim_source, KILL_COUNTER_SHIM).unwrap();
        let built = Command::new("cc")
            .args([
                "-shared",
                "-fPIC",
                "-o",
                shim_object.to_str().unwrap(),
                shim_source.to_str().unwrap(),
                "-ldl",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("cc for the kill-counter shim");
        assert!(built.success(), "kill-counter shim did not compile");
        let pidfile = scope.path().join("leader.pid");
        let daemon_pidfile = scope.path().join("daemon.pid");
        let kill_log = scope.path().join("kills.log");
        let strace_log = scope.path().join("strace.log");
        let result = scope.path().join("result");
        let helper = std::env::current_exe().unwrap();
        let helper_args = [
            "--exact".to_string(),
            "cleanup::tests::foreground_reaped_child_receives_no_signals".to_string(),
            "--nocapture".to_string(),
        ];
        let mut strace_command = Command::new("strace");
        // `-f` is required: libtest runs the test body on a worker
        // thread, which stays untraced without follow-forks. The
        // helper reaps its daemon before exiting, so tracing still
        // terminates with the helper.
        strace_command
            .args([
                "-f",
                "-e",
                "trace=kill,tkill,tgkill",
                "-o",
                strace_log.to_str().unwrap(),
            ])
            .arg(&helper)
            .args(&helper_args);
        let uses_strace = std::process::Command::new("sh")
            .args(["-c", "command -v strace"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let mut command = if uses_strace {
            strace_command
        } else {
            eprintln!("strace missing: kill proof rests on LD_PRELOAD alone");
            Command::new(&helper)
        };
        if !uses_strace {
            command.args(&helper_args);
        }
        let output = command
            .env(HELPER, "1")
            .env(PIDFILE, &pidfile)
            .env(DAEMON_PIDFILE, &daemon_pidfile)
            .env(RESULT, &result)
            .env("LD_PRELOAD", &shim_object)
            .env("KILL_LOG", &kill_log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "no-signal helper failed with {:?}",
            output.status
        );
        let leader = read_pidfile(&pidfile);
        // The helper reaped its daemon before exiting, so the
        // lease-hold premise pins through the daemon pidfile (the
        // daemon started) plus the elapsed lower bound below (the
        // full ~1s lease poll ran — an early lease close would
        // return in milliseconds).
        assert!(daemon_pidfile.exists(), "daemon never started");
        let elapsed_ms: u128 = std::fs::read_to_string(&result)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            elapsed_ms >= 900,
            "lease poll returned in {elapsed_ms}ms without proving an open lease"
        );
        let kills = std::fs::read_to_string(&kill_log).unwrap_or_default();
        let leader_kills: Vec<&str> = kills
            .lines()
            .filter(|line| {
                let mut fields = line.split(' ');
                let _caller = fields.next();
                fields
                    .next()
                    .is_some_and(|pid| pid.parse::<i32>().is_ok_and(|pid| pid == leader))
            })
            .collect();
        if uses_strace {
            // True-syscall proof: every `kill(2)`/`tkill`/`tgkill`
            // in the helper tree, whatever issued it.
            let trace = std::fs::read_to_string(&strace_log).unwrap_or_default();
            let syscall_kills: Vec<&str> = trace
                .lines()
                .filter(|line| {
                    line.find("kill(").is_some_and(|start| {
                        line[start + "kill(".len()..]
                            .split([',', ')'])
                            .next()
                            .is_some_and(|pid| {
                                pid.trim().parse::<i32>().is_ok_and(|pid| pid == leader)
                            })
                    })
                })
                .collect();
            assert!(
                syscall_kills.is_empty(),
                "reaped leader PID {leader} received kill syscalls after reap: {syscall_kills:?} (full trace: {trace:?})"
            );
        }
        assert!(
            leader_kills.is_empty(),
            "reaped leader PID {leader} received libc kills after reap: {leader_kills:?} (full log: {kills:?})"
        );
        assert!(
            elapsed_ms < 1900,
            "reaped-child stop stalled {elapsed_ms}ms (expected one ~1s lease poll, no grace loop)"
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn supervisor_drains_output_while_stopping_descendants() {
        use std::io::Read as _;
        use std::os::fd::OwnedFd;

        let scope = dot_test_support::TempDir::new("supervisor-term-output").unwrap();
        let ready = scope.path().join("ready");
        let completed = scope.path().join("completed");
        let (mut reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                // 320KB exceeds one socket buffer (so the trap blocks until the
                // supervisor drains mid-stop) but needs a single drain round
                // trip; 512KB needs two, which a loaded host cannot always
                // fit inside the one-second TERM grace.
                "set -m; (trap 'printf \"%327680s\" x; : >\"$2\"; exit 0' TERM; printf ready >\"$1\"; while :; do sleep 0.05; done) & until [[ -s $1 ]]; do sleep 0.01; done; exit 0",
                "supervisor-term-output",
            ])
            .arg(&ready)
            .arg(&completed)
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::null());
        let mut output = Vec::new();
        let result = supervise_session(command, None, |_| {
            let mut drained = 0;
            while drained < 64 * 1024 {
                let mut chunk = [0u8; 8192];
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        output.extend_from_slice(&chunk[..count]);
                        drained += count;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })
        .unwrap();

        assert!(matches!(result, SessionEnd::Exited(status) if status.success()));
        assert!(
            completed.exists(),
            "TERM trap was killed while writing output"
        );
        assert!(
            output.len() >= 256 * 1024,
            "TERM output never exceeded ordinary socket capacity"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn supervisor_fails_closed_for_an_unpinned_escaped_member() {
        let scope = dot_test_support::TempDir::new("supervisor-unpinned-member").unwrap();
        let marker = scope.path().join("descendant");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "set -m; (trap '' TERM; echo $BASHPID >\"$1\"; sleep 3) </dev/null >/dev/null 2>&1 & until [[ -s $1 ]]; do sleep 0.01; done",
                "supervisor-unpinned-member",
            ])
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        let error = supervise_session(command, None, |_| Ok(())).unwrap_err();
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(
            error
                .to_string()
                .contains("safe descendant delivery has no stable process authority")
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(!alive(pid), "self-bounded descendant survived");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_native_snapshot_lists_current_process() {
        let snapshot = macos_native_snapshot(Instant::now() + Duration::from_secs(5))
            .expect("native snapshot succeeds");
        let me = std::process::id();
        let row = snapshot
            .iter()
            .find(|process| process.pid == me)
            .expect("current process row present");
        assert!(row.live, "current process must be live: {row:?}");
        // The native row must match the per-PID resolver the snapshot is
        // built from, so rows stay consistent across the walk.
        let resolved = macos_native_process_info(me)
            .expect("resolver succeeds")
            .expect("current process row resolves");
        assert_eq!(row.parent, resolved.parent);
        assert_eq!(row.group, resolved.group);
        assert_eq!(row.session, resolved.session);
        // The native row must also match the `ps` fallback's membership
        // query for the same PID, pinning cross-source interchangeability.
        let (group, session) = macos_row_membership(me, false)
            .expect("fallback membership succeeds")
            .expect("current process membership present");
        assert_eq!(row.group, group);
        assert_eq!(row.session, session);
        // SAFETY: getppid takes no arguments and always succeeds.
        assert_eq!(row.parent, unsafe { libc::getppid() } as u32);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_native_snapshot_lists_an_unreaped_zombie() {
        // SAFETY: fork in a threaded test is safe when the child calls
        // only async-signal-safe functions before exiting immediately.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork the zombie fixture");
        if child == 0 {
            unsafe { libc::_exit(0) };
        }
        let zombie = child as u32;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = macos_native_snapshot(deadline).expect("snapshot succeeds");
            match snapshot.iter().find(|process| process.pid == zombie) {
                // The teardown proof requires the unreaped leader to be
                // listed as present-but-dead, like the `ps` fallback.
                Some(row) if !row.live => break,
                _ if Instant::now() >= deadline => {
                    panic!("unreaped zombie was never listed as not-live")
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        // SAFETY: the fixture child is our direct child; reap it.
        unsafe {
            libc::waitpid(child, std::ptr::null_mut(), 0);
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_deadline_does_not_wait_for_a_still_running_member() {
        let started = Instant::now();
        let reaped = reap_owned(
            vec![vec![10_u32]],
            started + Duration::from_millis(50),
            |_| WaitState::Running,
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(reaped, [false]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_retries_not_child_after_adoption_progress() {
        let mut calls = Vec::new();
        let mut grandchild_calls = 0;
        let reaped = reap_owned(
            vec![vec![10_u32, 20]],
            Instant::now() + Duration::from_secs(1),
            |pid| {
                calls.push(*pid);
                if *pid == 10 {
                    grandchild_calls += 1;
                    if grandchild_calls == 1 {
                        WaitState::NotChild
                    } else {
                        WaitState::Reaped
                    }
                } else {
                    WaitState::Reaped
                }
            },
        );
        assert_eq!(calls, [10, 20, 10]);
        assert_eq!(reaped, [true]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_reports_an_unadopted_member_without_progress() {
        let reaped = reap_owned(
            vec![vec![10_u32]],
            Instant::now() + Duration::from_secs(1),
            |_| WaitState::NotChild,
        );

        assert_eq!(reaped, [false]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_retries_interrupted_wait() {
        let mut calls = 0;
        let reaped = reap_owned(
            vec![vec![10_u32]],
            Instant::now() + Duration::from_secs(1),
            |_| {
                calls += 1;
                if calls == 1 {
                    WaitState::Interrupted
                } else {
                    WaitState::Reaped
                }
            },
        );
        assert_eq!(calls, 2);
        assert_eq!(reaped, [true]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_pass_visits_later_sessions_when_an_earlier_member_runs() {
        let mut calls = Vec::new();
        let reaped = reap_owned(
            vec![vec![10_u32], vec![20]],
            Instant::now() + Duration::from_millis(30),
            |pid| {
                calls.push(*pid);
                if *pid == 10 {
                    WaitState::Running
                } else {
                    WaitState::Reaped
                }
            },
        );
        assert!(calls.starts_with(&[10, 20]));
        assert_eq!(reaped, [false, true]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_reports_a_stable_handle_wait_failure() {
        let reaped = reap_owned(
            vec![vec![10_u32]],
            Instant::now() + Duration::from_secs(1),
            |_| WaitState::Terminal,
        );

        assert_eq!(reaped, [false]);
    }

    #[test]
    fn session_cannot_report_success_with_a_live_observed_member() {
        let mut session = Session::new(123);
        session.current.insert(
            124,
            ProcessInfo {
                pid: 124,
                parent: 123,
                group: 123,
                session: 123,
                live: true,
                identity: ProcessIdentity {
                    pid: 124,
                    start: None,
                },
            },
        );

        session.note_survivors();

        assert_eq!(
            session.error.expect("live member must fail cleanup").kind(),
            std::io::ErrorKind::Other
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn unsupported_pidfd_fails_closed() {
        let error = stable_pidfd(PidFd::Unsupported).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn authority_refusal_travels_the_deferred_error_channel() {
        let refusal = || {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "safe descendant delivery has no stable process authority",
            )
        };
        // A refusal with a clean tick surfaces as the call's error.
        let mut sessions = vec![Session::new(11), Session::new(12)];
        sessions[1].authority_error = Some(refusal());
        let error = merge_authority_errors(&mut sessions, Ok(())).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("no stable process authority"));
        // No refusal leaves the tick result untouched.
        let mut sessions = vec![Session::new(11)];
        assert!(merge_authority_errors(&mut sessions, Ok(())).is_ok());
        // An existing tick error takes precedence over a later refusal.
        let mut sessions = vec![Session::new(11)];
        sessions[0].authority_error = Some(refusal());
        let tick_error = std::io::Error::other("drain failed");
        let error = merge_authority_errors(&mut sessions, Err(tick_error)).unwrap_err();
        assert_eq!(error.to_string(), "drain failed");
    }

    #[test]
    fn authority_refusal_suppresses_survivor_error() {
        // A refusal means delivery was never attempted, so survivor
        // accounting must not record a second error that would suppress
        // the refusal into CleanupIncomplete.
        let mut session = Session::new(11);
        session.current.insert(
            12,
            ProcessInfo {
                pid: 12,
                parent: 11,
                group: 99,
                session: 11,
                live: true,
                identity: ProcessIdentity {
                    pid: 12,
                    start: None,
                },
            },
        );
        session.authority_error = Some(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe descendant delivery has no stable process authority",
        ));
        session.note_survivors();
        assert!(session.error.is_none());
        assert!(session.authority_error.is_some());
        // Without a refusal, live survivors still record.
        let mut plain = Session::new(11);
        plain.current.insert(
            12,
            ProcessInfo {
                pid: 12,
                parent: 11,
                group: 99,
                session: 11,
                live: true,
                identity: ProcessIdentity {
                    pid: 12,
                    start: None,
                },
            },
        );
        plain.note_survivors();
        assert!(plain.error.is_some());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn member_claim_rejects_a_stale_start_generation() {
        let mut command = Command::new("sleep");
        command.arg("4").stdin(Stdio::null());
        isolate(&mut command);
        let mut child = command.spawn().unwrap();
        let current = linux_process_info(child.id()).expect("fixture process identity");
        let mut stale = current.clone();
        stale.identity.start = stale.identity.start.map(|start| start.saturating_add(1));

        assert!(OwnedMember::claim(&stale).unwrap().is_none());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn live_same_group_member_requires_stable_reap_authority() {
        let process = ProcessInfo {
            pid: 124,
            parent: 123,
            group: 123,
            session: 123,
            live: true,
            identity: ProcessIdentity {
                pid: 124,
                start: Some(88),
            },
        };

        assert!(!member_claim_failure_is_fatal(&process, 123));
        let escaped = ProcessInfo {
            group: 124,
            session: 124,
            ..process
        };
        assert!(member_claim_failure_is_fatal(&escaped, 123));
    }

    #[test]
    fn only_an_escaped_member_requires_direct_signal_authority() {
        let anchored = ProcessInfo {
            pid: 124,
            parent: 123,
            group: 123,
            session: 123,
            live: true,
            identity: ProcessIdentity {
                pid: 124,
                start: None,
            },
        };
        let escaped = ProcessInfo {
            group: 124,
            ..anchored.clone()
        };

        assert!(!requires_direct_signal_authority(&anchored, 123));
        assert!(requires_direct_signal_authority(&escaped, 123));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn proc_identity_includes_the_kernel_start_tick() {
        let process = parse_proc_process(
            123,
            b"123 (nested ) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 77\n",
        )
        .expect("valid proc stat fixture");

        assert_eq!(process.pid, 123);
        assert_eq!(process.group, 2);
        assert_eq!(process.session, 3);
        assert_eq!(process.identity.start, Some(77));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn incomplete_proc_snapshot_is_not_authoritative() {
        let root = dot_test_support::TempDir::new("incomplete-proc-snapshot").unwrap();
        let process = root.path().join("123");
        std::fs::create_dir(&process).unwrap();
        std::fs::create_dir(process.join("stat")).unwrap();

        assert!(
            proc_process_snapshot(root.path(), Instant::now() + Duration::from_secs(1)).is_none()
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn proc_vanished_classifies_exit_races() {
        // ESRCH: the process exited between open and read.
        assert!(proc_process_vanished(&std::io::Error::from_raw_os_error(
            libc::ESRCH
        )));
        // ENOENT: the process was already gone at open.
        assert!(proc_process_vanished(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        // Anything else is a partial view, not churn.
        assert!(!proc_process_vanished(&std::io::Error::from_raw_os_error(
            libc::EACCES
        )));
        assert!(!proc_process_vanished(&std::io::Error::from_raw_os_error(
            libc::EISDIR
        )));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn proc_snapshot_skips_exit_races() {
        struct ResetEsrchRoot;
        impl Drop for ResetEsrchRoot {
            fn drop(&mut self) {
                *FORCE_FAKE_PROC_STAT_ESRCH_ROOT
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = None;
            }
        }
        let root = dot_test_support::TempDir::new("esrch-proc-snapshot").unwrap();
        let process = root.path().join("123");
        std::fs::create_dir(&process).unwrap();
        // A well-formed row whose read races its exit: the seam reports ESRCH.
        std::fs::write(
            process.join("stat"),
            b"123 (stat) R 1 123 123 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 100 0 0",
        )
        .unwrap();
        *FORCE_FAKE_PROC_STAT_ESRCH_ROOT
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(root.path().to_path_buf());
        let _reset = ResetEsrchRoot;
        let snapshot = proc_process_snapshot(root.path(), Instant::now() + Duration::from_secs(5));
        assert!(snapshot.is_some(), "an exit race failed the whole snapshot");
        assert!(snapshot.unwrap().is_empty(), "an exit race was not skipped");
    }

    #[test]
    fn proc_foreign_classifies_permission_denied() {
        assert!(proc_process_foreign(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(proc_process_foreign(&std::io::Error::from_raw_os_error(
            libc::EACCES
        )));
        assert!(!proc_process_foreign(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn proc_snapshot_denied_entry_is_platform_scoped() {
        struct ResetDenied;
        impl Drop for ResetDenied {
            fn drop(&mut self) {
                *FORCE_FAKE_PROC_STAT_DENIED
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = None;
            }
        }
        let root = dot_test_support::TempDir::new("denied-proc-snapshot").unwrap();
        let readable = root.path().join("123");
        std::fs::create_dir(&readable).unwrap();
        std::fs::write(
            readable.join("stat"),
            b"123 (stat) R 1 123 123 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 100 0 0",
        )
        .unwrap();
        let denied = root.path().join("456");
        std::fs::create_dir(&denied).unwrap();
        let denied_stat = denied.join("stat");
        std::fs::write(
            &denied_stat,
            b"456 (stat) R 1 456 456 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 200 0 0",
        )
        .unwrap();
        *FORCE_FAKE_PROC_STAT_DENIED
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(denied_stat);
        let _reset = ResetDenied;
        let snapshot = proc_process_snapshot(root.path(), Instant::now() + Duration::from_secs(5));
        #[cfg(target_os = "android")]
        assert_eq!(
            snapshot,
            Some(vec![ProcessInfo {
                pid: 123,
                parent: 1,
                group: 123,
                session: 123,
                live: true,
                identity: ProcessIdentity {
                    pid: 123,
                    start: Some(100),
                },
            }]),
            "a foreign-app denial must skip one row, not fail the snapshot"
        );
        // Linux has setuid transitions, so an unreadable stat stays a
        // fail-closed partial view there.
        #[cfg(not(target_os = "android"))]
        assert!(
            snapshot.is_none(),
            "a denied stat must fail the snapshot closed on Linux"
        );
    }

    #[test]
    fn snapshot_deadline_covers_child_that_closes_stdout_early() {
        let root = dot_test_support::TempDir::new("snapshot-deadline").unwrap();
        let marker = root.path().join("pid");
        let mut command = Command::new(dot_test_support::bash());
        command
            .args([
                "-c",
                "echo $$ >\"$1\"; exec >/dev/null; exec sleep 4",
                "snapshot",
            ])
            .arg(&marker);
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(snapshot(command, Instant::now() + Duration::from_secs(1)))
                .unwrap();
        });
        let observed = receive.recv_timeout(Duration::from_secs(3));
        worker.join().unwrap();
        assert!(
            observed.is_ok(),
            "snapshot EOF bypassed the bounded child wait"
        );
        assert!(observed.unwrap().is_none());
    }

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
