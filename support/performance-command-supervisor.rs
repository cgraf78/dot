//! Linux-only process-tree boundary for performance-gate build commands.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::ffi::{c_int, c_long, c_ulong};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
#[cfg(any(test, performance_supervisor_test))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

#[cfg(test)]
static PROCESS_TABLE_SCANS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FAIL_SCANS_AFTER_DISCOVERY: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static DISCOVERED_DESCENDANT: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static OMIT_RETAINED_ONCE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static OMITTED_RETAINED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static OMIT_UNRETAINED_ONCE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static OMITTED_UNRETAINED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static OMITTED_PID: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static OMITTED_WAS_RETAINED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static HIDE_DIRECT_AFTER_OMISSION: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static FAIL_SCANS_AFTER_OMISSION: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static FORCED_DIRECT_CHILD_PID: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FAIL_ALL_PROCESS_SCANS: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static FAIL_INTERNAL_FD_NORMALIZATION_AFTER: Cell<i32> = const { Cell::new(-1) };
}
#[cfg(performance_supervisor_test)]
static TEST_SCAN_PHASE: AtomicUsize = AtomicUsize::new(0);
#[cfg(performance_supervisor_test)]
static TEST_GUARDIAN_WAIT_FAULT_USED: AtomicBool = AtomicBool::new(false);
#[cfg(performance_supervisor_test)]
static TEST_GUARDIAN_SIGNAL_FAULT_USED: AtomicBool = AtomicBool::new(false);

static CANCELLATION_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CLEANUP_INCOMPLETE: AtomicBool = AtomicBool::new(false);
static DIAGNOSTICS_SAFE: AtomicBool = AtomicBool::new(true);
static TERMINAL_EXIT_STATUS: AtomicI32 = AtomicI32::new(70);
static START_BARRIER_STATE: AtomicI32 = AtomicI32::new(0);
static START_BARRIER_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static INHERITED_STDIN_FLAGS: AtomicI32 = AtomicI32::new(-1);
static INHERITED_STDOUT_FLAGS: AtomicI32 = AtomicI32::new(-1);
static INHERITED_STDERR_FLAGS: AtomicI32 = AtomicI32::new(-1);

const START_BARRIER_IDLE: c_int = 0;
const START_BARRIER_WAITING: c_int = 1;
const START_BARRIER_CANCELLED: c_int = 2;
const START_BARRIER_AUTHORIZED: c_int = 3;
const START_TOKEN: u8 = b'S';
const CANCEL_TOKEN: u8 = b'C';
const GUARDIAN_ARMED_TOKEN: u8 = b'A';
const GUARDIAN_SPAWN_TOKEN: u8 = b'S';
const GUARDIAN_TARGET_TOKEN: u8 = b'T';
const GUARDIAN_RUN_TOKEN: u8 = b'R';
const GUARDIAN_CANCEL_TOKEN: u8 = b'C';
const GUARDIAN_DONE_TOKEN: u8 = b'D';

const PR_SET_CHILD_SUBREAPER: c_int = 36;
const PR_SET_PDEATHSIG: c_int = 1;
const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGQUIT: c_int = 3;
const SIGCHLD: c_int = 17;
const SIGTERM: c_int = 15;
const SIGKILL: c_int = 9;
const SIGSTOP: c_int = 19;
const SIGCONT: c_int = 18;
const ESRCH: i32 = 3;
const ECHILD: i32 = 10;
const EINTR: i32 = 4;
const EBADF: i32 = 9;
const EAGAIN: i32 = 11;
const EPERM: i32 = 1;
const ENOSYS: i32 = 38;
const F_GETFD: c_int = 1;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const F_DUPFD_CLOEXEC: c_int = 1030;
const WUNTRACED: c_int = 2;
const WNOHANG: c_int = 1;
const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;
const SIG_BLOCK: c_int = 0;
const SIG_UNBLOCK: c_int = 1;
const SIG_SETMASK: c_int = 2;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SIGNAL_ERROR: usize = usize::MAX;
const POLL: Duration = Duration::from_millis(10);
const LEADER_POLL: c_int = 50;
const QUIESCENCE_GRACE: Duration = Duration::from_millis(500);
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const KILL_GRACE: Duration = Duration::from_secs(2);
const GUARDIAN_CLEANUP_GRACE: Duration = Duration::from_secs(8);
const SETUP_GRACE: Duration = Duration::from_secs(8);
const BOUNDED_DIAGNOSTIC_GRACE: Duration = Duration::from_millis(500);
const USAGE: &str =
    "usage: performance-command-supervisor [--parent-pid PID] [--cwd DIR] -- PROGRAM [ARG ...]";
#[cfg(performance_supervisor_test)]
const TEST_SCAN_FAULT_ENV: &str = "DOT_PERF_TEST_SUPERVISOR_SCAN_FAULT";
#[cfg(performance_supervisor_test)]
const TEST_DIRECT_PID_FILE_ENV: &str = "DOT_PERF_TEST_SUPERVISOR_DIRECT_PID_FILE";
#[cfg(performance_supervisor_test)]
const TEST_RELAY_PID_DIR_ENV: &str = "DOT_PERF_TEST_RELAY_PID_DIR";
#[cfg(performance_supervisor_test)]
const TEST_GUARDIAN_PID_FILE_ENV: &str = "DOT_PERF_TEST_GUARDIAN_PID_FILE";
#[cfg(performance_supervisor_test)]
const TEST_FAIL_RELAY_ENV: &str = "DOT_PERF_TEST_FAIL_RELAY";
#[cfg(performance_supervisor_test)]
const TEST_FAIL_OUTPUT_CAPTURE_ENV: &str = "DOT_PERF_TEST_FAIL_OUTPUT_CAPTURE";
#[cfg(performance_supervisor_test)]
const TEST_GUARDIAN_EXIT_PHASE_ENV: &str = "DOT_PERF_TEST_GUARDIAN_EXIT_PHASE";
#[cfg(performance_supervisor_test)]
const TEST_COORDINATOR_FAIL_PHASE_ENV: &str = "DOT_PERF_TEST_COORDINATOR_FAIL_PHASE";
#[cfg(performance_supervisor_test)]
const TEST_COORDINATOR_TARGET_PID_FILE_ENV: &str = "DOT_PERF_TEST_COORDINATOR_TARGET_PID_FILE";
#[cfg(performance_supervisor_test)]
const TEST_HANG_GUARDIAN_ON_CANCEL_ENV: &str = "DOT_PERF_TEST_HANG_GUARDIAN_ON_CANCEL";
#[cfg(performance_supervisor_test)]
const TEST_HANG_GUARDIAN_AFTER_DONE_ENV: &str = "DOT_PERF_TEST_HANG_GUARDIAN_AFTER_DONE";
#[cfg(performance_supervisor_test)]
const TEST_PHASE_MARKER_ENV: &str = "DOT_PERF_TEST_PHASE_MARKER";
#[cfg(performance_supervisor_test)]
const TEST_PHASE_RELEASE_ENV: &str = "DOT_PERF_TEST_PHASE_RELEASE";
#[cfg(performance_supervisor_test)]
const TEST_FAIL_GUARDIAN_WAIT_ENV: &str = "DOT_PERF_TEST_FAIL_GUARDIAN_WAIT";
#[cfg(performance_supervisor_test)]
const TEST_FAIL_GUARDIAN_SIGNAL_ENV: &str = "DOT_PERF_TEST_FAIL_GUARDIAN_SIGNAL";
#[cfg(performance_supervisor_test)]
const TEST_FAIL_DETACH_ENV: &str = "DOT_PERF_TEST_FAIL_DETACH";
#[cfg(performance_supervisor_test)]
const TEST_SETUP_GRACE_ENV: &str = "DOT_PERF_TEST_SETUP_GRACE_MS";
#[cfg(performance_supervisor_test)]
const TEST_STALL_GUARDIAN_PHASE_ENV: &str = "DOT_PERF_TEST_STALL_GUARDIAN_PHASE";
#[cfg(performance_supervisor_test)]
const TEST_STALL_COORDINATOR_PHASE_ENV: &str = "DOT_PERF_TEST_STALL_COORDINATOR_PHASE";
#[cfg(performance_supervisor_test)]
const TEST_GUARDIAN_TARGET_PID_FILE_ENV: &str = "DOT_PERF_TEST_GUARDIAN_TARGET_PID_FILE";

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const SYS_PIDFD_SEND_SIGNAL: c_long = 424;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const SYS_PIDFD_OPEN: c_long = 434;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_KCMP: c_long = 312;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const SYS_KCMP: c_long = 272;
const KCMP_FILE: c_int = 0;

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("the performance command supervisor supports Linux x86_64 and aarch64");

unsafe extern "C" {
    fn prctl(option: c_int, arg2: c_ulong, arg3: c_ulong, arg4: c_ulong, arg5: c_ulong) -> c_int;
    fn setsid() -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
    fn kill(pid: c_int, signal: c_int) -> c_int;
    fn getppid() -> c_int;
    fn close(descriptor: c_int) -> c_int;
    fn dup2(source: c_int, destination: c_int) -> c_int;
    fn fcntl(descriptor: c_int, command: c_int, ...) -> c_int;
    fn pipe(descriptors: *mut c_int) -> c_int;
    fn read(descriptor: c_int, buffer: *mut u8, count: usize) -> isize;
    fn write(descriptor: c_int, buffer: *const u8, count: usize) -> isize;
    fn poll(fds: *mut PollFd, count: c_ulong, timeout: c_int) -> c_int;
    fn signal(signal: c_int, handler: usize) -> usize;
    fn _exit(status: c_int) -> !;
    fn sigemptyset(set: *mut SignalSet) -> c_int;
    fn sigaddset(set: *mut SignalSet, signal: c_int) -> c_int;
    fn sigpending(set: *mut SignalSet) -> c_int;
    fn sigismember(set: *const SignalSet, signal: c_int) -> c_int;
    fn sigprocmask(how: c_int, set: *const SignalSet, old: *mut SignalSet) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
}

unsafe extern "C" fn capture_inherited_stdio() {
    // This constructor runs before Rust's runtime opens absent standard
    // descriptors for library safety. Preserve the executable-boundary state
    // so the eventual target sees the same descriptors as a direct exec.
    INHERITED_STDIN_FLAGS.store(unsafe { fcntl(0, F_GETFD) }, Ordering::Relaxed);
    INHERITED_STDOUT_FLAGS.store(unsafe { fcntl(1, F_GETFD) }, Ordering::Relaxed);
    INHERITED_STDERR_FLAGS.store(unsafe { fcntl(2, F_GETFD) }, Ordering::Relaxed);
}

#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
static CAPTURE_INHERITED_STDIO: unsafe extern "C" fn() = capture_inherited_stdio;

#[repr(C)]
#[derive(Clone, Copy)]
struct SignalSet {
    words: [c_ulong; 16],
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcessKey {
    pid: u32,
    start: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    parent: u32,
    session: u32,
    start: u64,
    live: bool,
}

struct ProcessMember {
    key: ProcessKey,
    pidfd: OwnedFd,
}

struct ParentBoundary {
    member: ProcessMember,
}

struct SignalMaskGuard {
    previous: SignalSet,
    active: bool,
}

struct SigchldGuard {
    ignored: bool,
    active: bool,
}

struct StartBarrier {
    read: Option<OwnedFd>,
    write: OwnedFd,
    armed: bool,
}

struct GuardianHandshake {
    ready: OwnedFd,
    start: OwnedFd,
}

struct GuardianOutputGuard {
    active: bool,
}

struct CoordinatorGuardian {
    child: Child,
    member: ProcessMember,
    ready: OwnedFd,
    start: Option<OwnedFd>,
    baseline: HashSet<ProcessKey>,
    target: Option<ProcessBoundary>,
    reaped_status: Option<ExitStatus>,
    guardian_reaped: bool,
    target_cleanup_done: bool,
}

struct ProcessBoundary {
    leader: ProcessMember,
    leader_reaped: bool,
    supervisor: u32,
    baseline_direct: HashSet<ProcessKey>,
    members: HashMap<ProcessKey, OwnedFd>,
}

struct OutputRelay {
    stdout: RelayChild,
    stderr: Option<RelayChild>,
    finished: bool,
}

struct OutputCaptures {
    stdout: File,
    stderr: Option<File>,
    stdout_destination_mask: u8,
}

struct RelayChild {
    child: Child,
    key: ProcessKey,
    destination_mask: u8,
    destination_detached: bool,
    stream: &'static str,
    status: Option<ExitStatus>,
}

enum DirectChildInventory {
    Available(HashSet<u32>),
    Unavailable,
}

fn process_vanished(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(ESRCH) | Some(2))
}

#[cfg(performance_supervisor_test)]
fn test_scan_fixture() -> Option<(&'static str, u32)> {
    let mode = env::var(TEST_SCAN_FAULT_ENV).ok()?;
    let mode = match mode.as_str() {
        "direct-before-fail" => "direct-before-fail",
        "unobserved-then-fail" => "unobserved-then-fail",
        _ => return None,
    };
    let marker = env::var_os(TEST_DIRECT_PID_FILE_ENV)?;
    let pid = fs::read_to_string(marker)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    Some((mode, pid))
}

fn parse_process_identity(pid: u32, stat: &[u8]) -> Result<ProcessIdentity, String> {
    let delimiter = stat
        .windows(2)
        .rposition(|window| window == b") ")
        .ok_or_else(|| format!("malformed process record for {pid}"))?;
    let fields = stat[delimiter + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() <= 19 {
        return Err(format!("short process record for {pid}"));
    }
    let parse = |field: &[u8], label: &str| {
        std::str::from_utf8(field)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| format!("invalid {label} for {pid}"))
    };
    let parent = parse(fields[1], "parent")?;
    let session = parse(fields[3], "session")?;
    let start = std::str::from_utf8(fields[19])
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| format!("invalid start time for {pid}"))?;
    Ok(ProcessIdentity {
        parent,
        session,
        start,
        live: !matches!(fields[0], b"Z" | b"X" | b"x"),
    })
}

fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>, String> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_vanished(&error) => return Ok(None),
        Err(error) => return Err(format!("read process identity for {pid}: {error}")),
    };
    parse_process_identity(pid, &stat).map(Some)
}

fn process_table() -> Result<HashMap<u32, ProcessIdentity>, String> {
    #[cfg(test)]
    {
        PROCESS_TABLE_SCANS.fetch_add(1, Ordering::Relaxed);
        if FAIL_ALL_PROCESS_SCANS.load(Ordering::Relaxed) {
            return Err("injected process-table failure".to_string());
        }
        if FAIL_SCANS_AFTER_DISCOVERY.load(Ordering::Relaxed)
            && DISCOVERED_DESCENDANT.load(Ordering::Relaxed)
        {
            return Err("injected process-table failure".to_string());
        }
        if FAIL_SCANS_AFTER_OMISSION.load(Ordering::Relaxed)
            && (OMITTED_RETAINED.load(Ordering::Relaxed)
                || OMITTED_UNRETAINED.load(Ordering::Relaxed))
        {
            return Err("injected process-table failure after omission".to_string());
        }
    }
    #[cfg(performance_supervisor_test)]
    let test_fixture = test_scan_fixture();
    #[cfg(performance_supervisor_test)]
    if let Some((mode, _)) = test_fixture {
        if mode == "direct-before-fail"
            || (mode == "unobserved-then-fail" && TEST_SCAN_PHASE.load(Ordering::Relaxed) >= 1)
        {
            return Err("injected process-table failure".to_string());
        }
    }
    let mut processes = HashMap::new();
    for entry in fs::read_dir("/proc").map_err(|error| format!("read process table: {error}"))? {
        let entry = entry.map_err(|error| format!("read process-table entry: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(identity) = process_identity(pid)? {
            processes.insert(pid, identity);
        }
    }
    #[cfg(performance_supervisor_test)]
    if let Some(("unobserved-then-fail", pid)) = test_fixture {
        processes.remove(&pid);
        TEST_SCAN_PHASE.store(1, Ordering::Relaxed);
    }
    Ok(processes)
}

fn direct_child_pids(parent: u32) -> Result<DirectChildInventory, String> {
    let task_root = format!("/proc/{parent}/task");
    let mut children = HashSet::new();
    let mut readable_records = 0_usize;
    for entry in
        fs::read_dir(&task_root).map_err(|error| format!("read process task table: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read process task entry: {error}"))?;
        let Some(task) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let record = match fs::read_to_string(format!("{task_root}/{task}/children")) {
            Ok(record) => {
                readable_records += 1;
                record
            }
            Err(error) if process_vanished(&error) => continue,
            Err(error) => return Err(format!("read direct child identities: {error}")),
        };
        for field in record.split_ascii_whitespace() {
            let child = field
                .parse::<u32>()
                .map_err(|_| "invalid direct child identity".to_string())?;
            if child > 0 {
                children.insert(child);
            }
        }
    }
    #[cfg(test)]
    {
        let forced = FORCED_DIRECT_CHILD_PID.load(Ordering::Relaxed) as u32;
        if forced != 0 {
            children.insert(forced);
            readable_records += 1;
        }
    }
    #[cfg(performance_supervisor_test)]
    if let Some((mode, pid)) = test_scan_fixture() {
        if mode == "unobserved-then-fail" {
            return Ok(DirectChildInventory::Unavailable);
        }
        children.insert(pid);
        readable_records += 1;
    }
    if readable_records == 0 {
        Ok(DirectChildInventory::Unavailable)
    } else {
        Ok(DirectChildInventory::Available(children))
    }
}

fn direct_children(parent: u32) -> Result<Option<HashSet<ProcessKey>>, String> {
    let mut children = HashSet::new();
    let DirectChildInventory::Available(pids) = direct_child_pids(parent)? else {
        return Ok(None);
    };
    for pid in pids {
        let Some(identity) = process_identity(pid)? else {
            continue;
        };
        if identity.parent == parent {
            children.insert(ProcessKey {
                pid,
                start: identity.start,
            });
        }
    }
    Ok(Some(children))
}

fn open_member(pid: u32, expected: &ProcessIdentity) -> Result<Option<ProcessMember>, String> {
    // SAFETY: pidfd_open has no pointer arguments. The returned descriptor
    // binds later signaling to this process instance rather than a reused PID.
    let descriptor = unsafe { syscall(SYS_PIDFD_OPEN, pid, 0) };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ESRCH) {
            return Ok(None);
        }
        return Err(format!("open process identity handle for {pid}: {error}"));
    }
    // SAFETY: pidfd_open returned a new descriptor owned by this process.
    let pidfd = unsafe { OwnedFd::from_raw_fd(descriptor as c_int) };
    if process_identity(pid)?.as_ref() != Some(expected) {
        return Ok(None);
    }
    Ok(Some(ProcessMember {
        key: ProcessKey {
            pid,
            start: expected.start,
        },
        pidfd,
    }))
}

fn cancellation_signal_set() -> Result<SignalSet, String> {
    let mut set = SignalSet { words: [0; 16] };
    // SAFETY: set points to writable storage with the Linux sigset_t layout.
    if unsafe { sigemptyset(&mut set) } != 0 {
        return Err(format!(
            "initialize cancellation signal set: {}",
            std::io::Error::last_os_error()
        ));
    }
    for requested_signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        // SAFETY: set remains initialized and the signal numbers are valid.
        if unsafe { sigaddset(&mut set, requested_signal) } != 0 {
            return Err(format!(
                "add cancellation signal {requested_signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(set)
}

impl SigchldGuard {
    fn normalize() -> Result<Self, String> {
        // Exec has already reset caught actions and SA_NOCLDWAIT, while an
        // ignored action intentionally survives. Remember only that one
        // exec-visible bit and make supervisor-owned children waitable.
        // SAFETY: SIG_DFL is a valid disposition for SIGCHLD.
        let previous = unsafe { signal(SIGCHLD, SIG_DFL) };
        if previous == SIGNAL_ERROR {
            return Err(format!(
                "normalize inherited SIGCHLD action: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            ignored: previous == SIG_IGN,
            active: true,
        })
    }

    fn child_ignored(&self) -> bool {
        self.ignored
    }
}

impl Drop for SigchldGuard {
    fn drop(&mut self) {
        if self.active {
            let handler = if self.ignored { SIG_IGN } else { SIG_DFL };
            // SAFETY: both possible handlers are valid SIGCHLD dispositions.
            let _ = unsafe { signal(SIGCHLD, handler) };
            self.active = false;
        }
    }
}

impl SignalMaskGuard {
    fn block() -> Result<Self, String> {
        let set = cancellation_signal_set()?;
        let mut previous = SignalSet { words: [0; 16] };
        // SAFETY: both pointers reference initialized Linux sigset_t storage.
        if unsafe { sigprocmask(SIG_BLOCK, &set, &mut previous) } != 0 {
            return Err(format!(
                "block cancellation signals: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            previous,
            active: true,
        })
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        // SAFETY: previous is the mask returned by the successful block call.
        if unsafe { sigprocmask(SIG_SETMASK, &self.previous, std::ptr::null_mut()) } != 0 {
            return Err(format!(
                "restore cancellation signal mask: {}",
                std::io::Error::last_os_error()
            ));
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        if self.active {
            // Best effort during error unwinding; the explicit success path
            // reports restoration failures.
            let _ = unsafe { sigprocmask(SIG_SETMASK, &self.previous, std::ptr::null_mut()) };
        }
    }
}

fn inherited_stdio_mask() -> u8 {
    let mut mask = 0_u8;
    for (descriptor, flags) in [
        INHERITED_STDIN_FLAGS.load(Ordering::Relaxed),
        INHERITED_STDOUT_FLAGS.load(Ordering::Relaxed),
        INHERITED_STDERR_FLAGS.load(Ordering::Relaxed),
    ]
    .into_iter()
    .enumerate()
    {
        if flags >= 0 {
            mask |= 1 << descriptor;
        }
    }
    mask
}

fn set_descriptor_cloexec(descriptor: c_int, enabled: bool) -> std::io::Result<()> {
    // SAFETY: F_GETFD only inspects descriptor-local flags.
    let flags = unsafe { fcntl(descriptor, F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let updated = if enabled {
        flags | FD_CLOEXEC
    } else {
        flags & !FD_CLOEXEC
    };
    if updated == flags {
        return Ok(());
    }
    // SAFETY: F_SETFD updates only descriptor-local flags.
    if unsafe { fcntl(descriptor, F_SETFD, updated) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn normalize_internal_fd(descriptor: OwnedFd, label: &str) -> Result<OwnedFd, String> {
    #[cfg(test)]
    let injected_failure = FAIL_INTERNAL_FD_NORMALIZATION_AFTER.with(|remaining| {
        let current = remaining.get();
        if current < 0 {
            return false;
        }
        if current == 0 {
            remaining.set(-1);
            return true;
        }
        remaining.set(current - 1);
        false
    });
    #[cfg(test)]
    if injected_failure {
        return Err(format!("injected {label} descriptor failure"));
    }
    if descriptor.as_raw_fd() >= 3 {
        set_descriptor_cloexec(descriptor.as_raw_fd(), true)
            .map_err(|error| format!("secure {label} descriptor: {error}"))?;
        return Ok(descriptor);
    }
    // SAFETY: F_DUPFD_CLOEXEC duplicates this owned descriptor at or above 3
    // and atomically sets close-on-exec. On failure the input remains owned by
    // descriptor and is closed during unwinding.
    let duplicate = unsafe { fcntl(descriptor.as_raw_fd(), F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(format!(
            "normalize {label} descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this scope.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn duplicate_internal_fd(descriptor: &OwnedFd, label: &str) -> Result<OwnedFd, String> {
    // SAFETY: F_DUPFD_CLOEXEC duplicates this live descriptor at or above 3
    // and atomically applies the internal close-on-exec invariant.
    let duplicate = unsafe { fcntl(descriptor.as_raw_fd(), F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(format!(
            "duplicate {label} descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this scope.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn internal_pipe(read_label: &str, write_label: &str) -> Result<(OwnedFd, OwnedFd), String> {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors points to storage for both pipe descriptors.
    if unsafe { pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(format!(
            "create {read_label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Take ownership of both raw results before fallible normalization so
    // every partial-setup path closes both pipe ends exactly once.
    // SAFETY: pipe returned two new descriptors owned by this process.
    let read = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    // SAFETY: pipe returned two new descriptors owned by this process.
    let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    let read = normalize_internal_fd(read, read_label)?;
    let write = normalize_internal_fd(write, write_label)?;
    Ok((read, write))
}

fn same_open_file(left: c_int, right: c_int) -> Result<bool, String> {
    let process = std::process::id();
    // SAFETY: kcmp with KCMP_FILE compares two descriptors in this process
    // without modifying either open-file description.
    let result = unsafe { syscall(SYS_KCMP, process, process, KCMP_FILE, left, right) };
    if result >= 0 {
        return Ok(result == 0);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // kcmp needs CAP_SYS_PTRACE, which container runtimes withhold, and
        // kernels older than 3.5 lack the call entirely. Fall back to the
        // portable procfs comparison there; any other failure (a closed
        // descriptor, a bad call) still fails closed below.
        Some(EPERM) | Some(ENOSYS) => same_open_file_by_procfs(left, right),
        _ => Err(format!(
            "compare command-supervisor output identities: {error}"
        )),
    }
}

// Compares open-file identity through /proc/self/fd links when kcmp is
// unavailable. Anonymous objects (pipes, sockets) compare by kernel inode,
// so duplicates match and distinct objects do not. Path-backed links can
// over-match separately opened descriptions of one path (notably /dev/null
// and same-path deleted files); merging those captures is benign because
// the relay still delivers identical bytes to both inherited sinks.
fn same_open_file_by_procfs(left: c_int, right: c_int) -> Result<bool, String> {
    let left_target = fs::read_link(format!("/proc/self/fd/{left}"))
        .map_err(|error| format!("read command-supervisor output identity {left}: {error}"))?;
    let right_target = fs::read_link(format!("/proc/self/fd/{right}"))
        .map_err(|error| format!("read command-supervisor output identity {right}: {error}"))?;
    Ok(left_target == right_target)
}

fn outputs_aliased(stdio_mask: u8) -> Result<bool, String> {
    if stdio_mask & 0b110 != 0b110 {
        return Ok(false);
    }
    same_open_file(1, 2)
}

fn close_absent_stdio(mask: u8) -> std::io::Result<()> {
    for descriptor in 0..=2 {
        if mask & (1 << descriptor) != 0 {
            continue;
        }
        // SAFETY: the descriptor is intentionally absent at the caller's exec
        // boundary. Internal descriptors are normalized above this range.
        if unsafe { close(descriptor) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(EBADF)
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn write_start_barrier(descriptor: c_int, token: u8) -> Result<(), String> {
    // SAFETY: token points to one readable byte and descriptor is the retained
    // write end of the start-barrier pipe.
    let result = unsafe { write(descriptor, &token, 1) };
    if result == 1 {
        Ok(())
    } else {
        Err(format!(
            "write command start barrier: {}",
            std::io::Error::last_os_error()
        ))
    }
}

impl StartBarrier {
    fn new() -> Result<Self, String> {
        if START_BARRIER_STATE
            .compare_exchange(
                START_BARRIER_IDLE,
                START_BARRIER_WAITING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return Err("another command start barrier is already active".to_string());
        }
        let (read, write) =
            match internal_pipe("command start-barrier read", "command start-barrier write") {
                Ok(descriptors) => descriptors,
                Err(error) => {
                    START_BARRIER_STATE.store(START_BARRIER_IDLE, Ordering::SeqCst);
                    return Err(error);
                }
            };
        START_BARRIER_WRITE_FD.store(write.as_raw_fd(), Ordering::SeqCst);
        Ok(Self {
            read: Some(read),
            write,
            armed: true,
        })
    }

    fn child_descriptors(&self) -> (c_int, c_int) {
        (
            self.read
                .as_ref()
                .expect("start barrier read end")
                .as_raw_fd(),
            self.write.as_raw_fd(),
        )
    }

    fn spawned(&mut self) {
        self.read.take();
    }

    fn cancel(&mut self) -> Result<(), String> {
        let result = if START_BARRIER_STATE
            .compare_exchange(
                START_BARRIER_WAITING,
                START_BARRIER_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            write_start_barrier(self.write.as_raw_fd(), CANCEL_TOKEN)
        } else {
            Ok(())
        };
        self.disarm();
        result
    }

    fn authorize(&mut self) -> Result<bool, String> {
        if START_BARRIER_STATE
            .compare_exchange(
                START_BARRIER_WAITING,
                START_BARRIER_AUTHORIZED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            self.disarm();
            return Ok(false);
        }
        let result = write_start_barrier(self.write.as_raw_fd(), START_TOKEN);
        self.disarm();
        result.map(|()| true)
    }

    fn disarm(&mut self) {
        if self.armed {
            START_BARRIER_WRITE_FD.store(-1, Ordering::SeqCst);
            START_BARRIER_STATE.store(START_BARRIER_IDLE, Ordering::SeqCst);
            self.armed = false;
        }
    }
}

impl Drop for StartBarrier {
    fn drop(&mut self) {
        self.disarm();
    }
}

impl GuardianHandshake {
    fn exchange_token(
        &self,
        parent: &ParentBoundary,
        published: u8,
        expected: u8,
        phase: &str,
        deadline: Instant,
    ) -> Result<(), String> {
        // The coordinator cannot authorize arbitrary target code until this
        // process is a subreaper, has bound its exact parent, and has armed
        // parent-death notification.
        let result = unsafe { write(self.ready.as_raw_fd(), &published, 1) };
        if result != 1 {
            return Err(format!(
                "publish cleanup-guardian {phase}: {}",
                std::io::Error::last_os_error()
            ));
        }
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "cleanup-guardian authorization timed out during {phase}"
                ));
            }
            if let Some(requested_signal) = pending_cancellation()? {
                return Err(format!(
                    "cleanup guardian cancelled before authorization by signal {requested_signal}"
                ));
            }
            if pidfd_ready(&parent.member.pidfd, 0)? {
                record_cancellation(SIGTERM);
                return Err("cleanup-guardian parent exited before authorization".to_string());
            }
            let mut descriptor = PollFd {
                fd: self.start.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor points to one initialized pollfd.
            let ready = unsafe { poll(&mut descriptor, 1, POLL.as_millis() as c_int) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(EINTR) {
                    continue;
                }
                return Err(format!("wait for cleanup-guardian authorization: {error}"));
            }
            if ready == 0 {
                continue;
            }
            if descriptor.revents & POLLNVAL != 0 {
                return Err("cleanup-guardian authorization descriptor became invalid".to_string());
            }
            let mut token = 0_u8;
            // SAFETY: token points to one writable byte and start remains live.
            let count = unsafe { read(self.start.as_raw_fd(), &mut token, 1) };
            if count == 1 && token == expected {
                return Ok(());
            }
            if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!(
                "cleanup guardian was not authorized during {phase}"
            ));
        }
    }

    fn arm(&self, parent: &ParentBoundary) -> Result<(), String> {
        // The reciprocal of the coordinator's arming deadline: a coordinator
        // that never authorizes the spawn must not retain this guardian.
        let deadline = Instant::now() + setup_grace();
        self.exchange_token(
            parent,
            GUARDIAN_ARMED_TOKEN,
            GUARDIAN_SPAWN_TOKEN,
            "arming",
            deadline,
        )
    }

    fn publish_target(&self, parent: &ParentBoundary, target: ProcessKey) -> Result<(), String> {
        let mut frame = [0_u8; 13];
        frame[0] = GUARDIAN_TARGET_TOKEN;
        frame[1..5].copy_from_slice(&target.pid.to_ne_bytes());
        frame[5..13].copy_from_slice(&target.start.to_ne_bytes());
        let mut written = 0;
        while written < frame.len() {
            // SAFETY: frame contains initialized bytes and ready remains live.
            let count = unsafe {
                write(
                    self.ready.as_raw_fd(),
                    frame[written..].as_ptr(),
                    frame.len() - written,
                )
            };
            if count > 0 {
                written += count as usize;
                continue;
            }
            if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!(
                "publish cleanup-guardian target: {}",
                std::io::Error::last_os_error()
            ));
        }
        exit_at_test_guardian_phase("target");
        // The reciprocal of the coordinator's target-publication deadline.
        // On expiry the caller cancels the start barrier and tears down the
        // still-stopped target, so its body never executes unauthorized.
        let deadline = Instant::now() + setup_grace();
        self.wait_for_token(parent, GUARDIAN_RUN_TOKEN, "target authorization", deadline)
    }

    fn wait_for_token(
        &self,
        parent: &ParentBoundary,
        expected: u8,
        phase: &str,
        deadline: Instant,
    ) -> Result<(), String> {
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "cleanup-guardian authorization timed out during {phase}"
                ));
            }
            if let Some(requested_signal) = pending_cancellation()? {
                return Err(format!(
                    "cleanup guardian cancelled before {phase} by signal {requested_signal}"
                ));
            }
            if pidfd_ready(&parent.member.pidfd, 0)? {
                record_cancellation(SIGTERM);
                return Err(format!("cleanup-guardian parent exited before {phase}"));
            }
            let mut descriptor = PollFd {
                fd: self.start.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor points to one initialized pollfd.
            let ready = unsafe { poll(&mut descriptor, 1, POLL.as_millis() as c_int) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(EINTR) {
                    continue;
                }
                return Err(format!("wait for cleanup-guardian {phase}: {error}"));
            }
            if ready == 0 {
                continue;
            }
            if descriptor.revents & POLLNVAL != 0 {
                return Err(format!(
                    "cleanup-guardian {phase} descriptor became invalid"
                ));
            }
            let mut token = 0_u8;
            // SAFETY: token points to one writable byte and start remains live.
            let count = unsafe { read(self.start.as_raw_fd(), &mut token, 1) };
            if count == 1 && token == expected {
                return Ok(());
            }
            if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!(
                "cleanup guardian was not authorized during {phase}"
            ));
        }
    }

    fn complete(&self) -> Result<(), String> {
        let frame = [
            GUARDIAN_DONE_TOKEN,
            u8::from(CLEANUP_INCOMPLETE.load(Ordering::Acquire)),
        ];
        let mut written = 0;
        while written < frame.len() {
            // SAFETY: frame contains initialized bytes and ready remains live.
            let count = unsafe {
                write(
                    self.ready.as_raw_fd(),
                    frame[written..].as_ptr(),
                    frame.len() - written,
                )
            };
            if count > 0 {
                written += count as usize;
                continue;
            }
            if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!(
                "publish cleanup-guardian completion: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn acknowledge_cancellation(&self, requested_signal: c_int) -> Result<(), String> {
        let frame = [GUARDIAN_CANCEL_TOKEN, requested_signal as u8];
        // SAFETY: frame contains two initialized bytes and ready remains live.
        if unsafe { write(self.ready.as_raw_fd(), frame.as_ptr(), frame.len()) }
            == frame.len() as isize
        {
            Ok(())
        } else {
            Err(format!(
                "publish cleanup-guardian cancellation: {}",
                std::io::Error::last_os_error()
            ))
        }
    }
}

fn unblock_cancellation_signals() -> Result<(), String> {
    let set = cancellation_signal_set()?;
    // SAFETY: set references an initialized Linux sigset_t.
    if unsafe { sigprocmask(SIG_UNBLOCK, &set, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "unblock cancellation signals: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn pending_cancellation() -> Result<Option<c_int>, String> {
    let recorded = CANCELLATION_SIGNAL.load(Ordering::Relaxed);
    if recorded != 0 {
        return Ok(Some(recorded));
    }
    let mut pending = SignalSet { words: [0; 16] };
    // SAFETY: pending points to writable Linux sigset_t storage.
    if unsafe { sigpending(&mut pending) } != 0 {
        return Err(format!(
            "inspect pending cancellation signals: {}",
            std::io::Error::last_os_error()
        ));
    }
    for requested_signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        // SAFETY: pending was initialized by sigpending.
        let present = unsafe { sigismember(&pending, requested_signal) };
        if present < 0 {
            return Err(format!(
                "inspect pending signal {requested_signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if present == 1 {
            record_cancellation(requested_signal);
            return Ok(Some(requested_signal));
        }
    }
    Ok(None)
}

fn current_parent_pid() -> Result<u32, String> {
    // SAFETY: getppid has no arguments and cannot modify memory.
    let parent = unsafe { getppid() };
    u32::try_from(parent)
        .ok()
        .filter(|parent| *parent > 0)
        .ok_or_else(|| "cannot identify command-supervisor parent".to_string())
}

fn bind_parent(expected_pid: u32) -> Result<ParentBoundary, String> {
    if current_parent_pid()? != expected_pid {
        return Err("command-supervisor parent does not match the expected process".to_string());
    }
    let identity = process_identity(expected_pid)?
        .filter(|identity| identity.live)
        .ok_or_else(|| "expected command-supervisor parent is not live".to_string())?;
    let member = open_member(expected_pid, &identity)?
        .ok_or_else(|| "expected command-supervisor parent vanished".to_string())?;
    // SAFETY: PR_SET_PDEATHSIG takes integer arguments only. The blocked TERM
    // signal is recorded after the start-boundary decision and cannot run an
    // asynchronous cleanup handler.
    if unsafe { prctl(PR_SET_PDEATHSIG, SIGTERM as c_ulong, 0, 0, 0) } != 0 {
        return Err(format!(
            "bind command-supervisor parent death: {}",
            std::io::Error::last_os_error()
        ));
    }
    let post_identity = process_identity(expected_pid)?;
    if current_parent_pid()? != expected_pid
        || post_identity.as_ref() != Some(&identity)
        || pidfd_ready(&member.pidfd, 0)?
    {
        return Err("command-supervisor parent changed during identity binding".to_string());
    }
    Ok(ParentBoundary { member })
}

fn enable_subreaper() -> Result<(), String> {
    // SAFETY: PR_SET_CHILD_SUBREAPER takes integer arguments only.
    if unsafe { prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "enable child subreaper: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn boundary(leader: u32, baseline_direct: HashSet<ProcessKey>) -> Result<ProcessBoundary, String> {
    let identity = process_identity(leader)?
        .ok_or_else(|| "command leader vanished before identity validation".to_string())?;
    if identity.session != leader {
        return Err("command leader did not establish its process session".to_string());
    }
    let leader = open_member(leader, &identity)?
        .ok_or_else(|| "command leader vanished before pidfd validation".to_string())?;
    Ok(ProcessBoundary {
        leader,
        leader_reaped: false,
        supervisor: std::process::id(),
        baseline_direct,
        members: HashMap::new(),
    })
}

fn identity_matches(key: ProcessKey, identity: &ProcessIdentity) -> bool {
    key.start == identity.start
}

fn group_identity_is_pinned(
    leader_reaped: bool,
    leader: ProcessKey,
    identity: Option<&ProcessIdentity>,
) -> bool {
    !leader_reaped
        && identity.is_some_and(|identity| {
            identity_matches(leader, identity) && identity.session == leader.pid
        })
}

fn retain_adopted_children(boundary: &mut ProcessBoundary) -> Result<bool, String> {
    let DirectChildInventory::Available(pids) = direct_child_pids(boundary.supervisor)? else {
        return Ok(false);
    };
    for pid in pids {
        if pid == boundary.leader.key.pid {
            continue;
        }
        let Some(identity) = process_identity(pid)? else {
            continue;
        };
        let key = ProcessKey {
            pid,
            start: identity.start,
        };
        if identity.parent != boundary.supervisor
            || !identity.live
            || identity.start < boundary.leader.key.start
            || boundary.baseline_direct.contains(&key)
            || boundary.members.contains_key(&key)
        {
            continue;
        }
        if let Some(member) = open_member(pid, &identity)? {
            #[cfg(test)]
            DISCOVERED_DESCENDANT.store(true, Ordering::Relaxed);
            boundary.members.insert(member.key, member.pidfd);
        }
    }
    Ok(true)
}

fn discover_members(boundary: &mut ProcessBoundary) -> Result<usize, String> {
    prune_exited_members(boundary)?;
    // Pin newly adopted children from the kernel's direct inventory before a
    // fallible broad scan. If both independent observations are unavailable,
    // report that loss explicitly instead of treating it as proven empty.
    #[cfg(test)]
    let hide_direct = HIDE_DIRECT_AFTER_OMISSION.load(Ordering::Relaxed)
        && (OMIT_UNRETAINED_ONCE.load(Ordering::Relaxed)
            || OMITTED_UNRETAINED.load(Ordering::Relaxed));
    #[cfg(not(test))]
    let hide_direct = false;
    let direct_inventory = if hide_direct {
        Ok(false)
    } else {
        retain_adopted_children(boundary)
    };
    let processes = match process_table() {
        Ok(processes) => processes,
        Err(error) => {
            return Err(match &direct_inventory {
                Ok(true) => error,
                Ok(false) => format!("{error}; direct child process inventory is unavailable"),
                Err(direct) => format!("{error}; direct child process inventory failed: {direct}"),
            });
        }
    };
    #[cfg(test)]
    let mut processes = processes;
    #[cfg(test)]
    if OMIT_RETAINED_ONCE.load(Ordering::Relaxed) && DISCOVERED_DESCENDANT.load(Ordering::Relaxed) {
        for key in boundary.members.keys() {
            processes.remove(&key.pid);
        }
        OMIT_RETAINED_ONCE.store(false, Ordering::Relaxed);
        OMITTED_RETAINED.store(true, Ordering::Relaxed);
    }
    #[cfg(test)]
    if OMIT_UNRETAINED_ONCE.load(Ordering::Relaxed) {
        let omitted = processes.iter().find_map(|(&pid, identity)| {
            let key = ProcessKey {
                pid,
                start: identity.start,
            };
            (pid != boundary.leader.key.pid
                && identity.live
                && identity.parent == boundary.supervisor
                && identity.start >= boundary.leader.key.start
                && !boundary.baseline_direct.contains(&key)
                && !boundary.members.contains_key(&key))
            .then_some(pid)
        });
        if let Some(pid) = omitted {
            OMITTED_WAS_RETAINED.store(
                boundary.members.keys().any(|member| member.pid == pid),
                Ordering::Relaxed,
            );
            processes.remove(&pid);
            OMIT_UNRETAINED_ONCE.store(false, Ordering::Relaxed);
            OMITTED_UNRETAINED.store(true, Ordering::Relaxed);
            OMITTED_PID.store(pid as usize, Ordering::Relaxed);
        }
    }
    let mut observed = HashSet::new();
    if processes
        .get(&boundary.leader.key.pid)
        .is_some_and(|identity| identity_matches(boundary.leader.key, identity))
    {
        observed.insert(boundary.leader.key);
    }
    for key in boundary.members.keys().copied() {
        if processes
            .get(&key.pid)
            .is_some_and(|identity| identity_matches(key, identity))
        {
            observed.insert(key);
        }
    }
    loop {
        let before = observed.len();
        for (&pid, identity) in &processes {
            let key = ProcessKey {
                pid,
                start: identity.start,
            };
            let parent_owned = processes.get(&identity.parent).is_some_and(|parent| {
                observed.contains(&ProcessKey {
                    pid: identity.parent,
                    start: parent.start,
                })
            });
            let adopted_after_spawn = identity.parent == boundary.supervisor
                && identity.start >= boundary.leader.key.start
                && !boundary.baseline_direct.contains(&key);
            let original_session =
                !boundary.leader_reaped && identity.session == boundary.leader.key.pid;
            if original_session || parent_owned || adopted_after_spawn {
                observed.insert(key);
            }
        }
        if observed.len() == before {
            break;
        }
    }
    for key in observed {
        if key == boundary.leader.key || boundary.members.contains_key(&key) {
            continue;
        }
        let Some(identity) = processes.get(&key.pid) else {
            continue;
        };
        if !identity_matches(key, identity) || !identity.live {
            continue;
        }
        if let Some(member) = open_member(key.pid, identity)? {
            #[cfg(test)]
            DISCOVERED_DESCENDANT.store(true, Ordering::Relaxed);
            boundary.members.insert(member.key, member.pidfd);
        }
    }
    direct_inventory?;
    Ok(boundary.members.len())
}

fn signal_pidfd(key: ProcessKey, pidfd: &OwnedFd, requested_signal: c_int) -> Result<(), String> {
    // SAFETY: pidfd_send_signal targets the exact process represented by the
    // descriptor and cannot signal a recycled numeric PID.
    let result = unsafe {
        syscall(
            SYS_PIDFD_SEND_SIGNAL,
            pidfd.as_raw_fd(),
            requested_signal,
            std::ptr::null::<u8>(),
            0,
        )
    };
    if result >= 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        Ok(())
    } else {
        Err(format!("signal process {}: {error}", key.pid))
    }
}

fn signal_known_members(boundary: &ProcessBoundary, requested_signal: c_int) -> Result<(), String> {
    let mut first_error = None;
    if !boundary.leader_reaped {
        if let Err(error) = signal_pidfd(
            boundary.leader.key,
            &boundary.leader.pidfd,
            requested_signal,
        ) {
            first_error = Some(error);
        }
    }
    for (&key, pidfd) in &boundary.members {
        if let Err(error) = signal_pidfd(key, pidfd, requested_signal) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn signal_leader_group(boundary: &ProcessBoundary, requested_signal: c_int) -> Result<(), String> {
    let identity = process_identity(boundary.leader.key.pid)?;
    if !group_identity_is_pinned(
        boundary.leader_reaped,
        boundary.leader.key,
        identity.as_ref(),
    ) {
        return Err("refusing to signal an unpinned command process group".to_string());
    }
    // SAFETY: the unreaped session leader still pins this process-group ID.
    if unsafe { kill(-(boundary.leader.key.pid as c_int), requested_signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        Ok(())
    } else {
        Err(format!("signal command process group: {error}"))
    }
}

fn pidfd_ready(pidfd: &OwnedFd, timeout: c_int) -> Result<bool, String> {
    let mut descriptor = PollFd {
        fd: pidfd.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor points to one initialized pollfd for this call.
    let result = unsafe { poll(&mut descriptor, 1, timeout) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(EINTR) {
            return Ok(false);
        }
        return Err(format!("poll command leader identity: {error}"));
    }
    if descriptor.revents & POLLNVAL != 0 {
        return Err("command leader identity handle became invalid".to_string());
    }
    Ok(result > 0 && descriptor.revents & (POLLIN | POLLERR | POLLHUP) != 0)
}

fn relay_output(mut reader: File, destination: c_int, stream: &str) -> Result<(), String> {
    let mut buffer = [0_u8; 8192];
    loop {
        let mut descriptor = PollFd {
            fd: reader.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for this call.
        let result = unsafe { poll(&mut descriptor, 1, POLL.as_millis() as c_int) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!("poll supervised command {stream}: {error}"));
        }
        if result == 0 {
            continue;
        }
        if descriptor.revents & POLLNVAL != 0 {
            return Err(format!("supervised command {stream} pipe became invalid"));
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                #[cfg(performance_supervisor_test)]
                wait_at_test_relay_drain(stream)?;
                return Ok(());
            }
            Ok(count) => {
                let mut written = 0;
                while written < count {
                    // This relay runs in its own exact child process. A
                    // blocked destination can therefore be cancelled with
                    // SIGKILL without trapping the supervisor in a thread
                    // join or changing shared descriptor flags.
                    // SAFETY: buffer contains count initialized bytes and the
                    // destination is the inherited standard stream.
                    let result = unsafe {
                        write(
                            destination,
                            buffer[written..count].as_ptr(),
                            count - written,
                        )
                    };
                    if result > 0 {
                        written += result as usize;
                        continue;
                    }
                    if result == 0 {
                        return Err(format!(
                            "forward supervised command {stream}: write returned zero"
                        ));
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(format!("forward supervised command {stream}: {error}"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read supervised command {stream}: {error}")),
        }
    }
}

fn detach_destination(destination: c_int, stream: &str) -> Result<(), String> {
    #[cfg(performance_supervisor_test)]
    if env::var(TEST_FAIL_DETACH_ENV).as_deref() == Ok(stream) {
        return Err(format!("injected {stream} destination detach failure"));
    }
    let sink = OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .map_err(|error| format!("open {stream} relay terminal sink: {error}"))?;
    if sink.as_raw_fd() == destination {
        let descriptor = sink.into_raw_fd();
        set_descriptor_cloexec(descriptor, true)
            .map_err(|error| format!("secure {stream} relay terminal sink: {error}"))?;
        return Ok(());
    }
    // SAFETY: sink and destination are live descriptors in this process;
    // dup2 atomically releases the downstream writer while keeping the
    // standard descriptor occupied so later opens cannot reuse it.
    if unsafe { dup2(sink.as_raw_fd(), destination) } < 0 {
        return Err(format!(
            "detach supervised command {stream} destination: {}",
            std::io::Error::last_os_error()
        ));
    }
    set_descriptor_cloexec(destination, true)
        .map_err(|error| format!("secure {stream} relay terminal sink: {error}"))
}

fn spawn_relay(
    source: File,
    destination: c_int,
    destination_mask: u8,
    stream: &'static str,
) -> Result<RelayChild, String> {
    #[cfg(performance_supervisor_test)]
    if env::var(TEST_FAIL_RELAY_ENV).as_deref() == Ok(stream) {
        return Err(format!(
            "injected supervised command {stream} relay failure"
        ));
    }
    let source_descriptor = source.as_raw_fd();
    let expected_parent = std::process::id();
    let relay_stdio_mask = if destination == 1 { 2 } else { 4 };
    if destination_mask & relay_stdio_mask == 0 {
        return Err(format!(
            "supervised command {stream} relay does not own its destination"
        ));
    }
    #[cfg(not(test))]
    let mut command = {
        let executable = env::current_exe()
            .map_err(|error| format!("resolve command supervisor executable: {error}"))?;
        let mut command = Command::new(executable);
        command.args([
            OsString::from("--output-relay"),
            OsString::from(source_descriptor.to_string()),
            OsString::from(destination.to_string()),
            OsString::from(stream),
        ]);
        command
    };
    #[cfg(test)]
    let mut command = {
        let mut command = Command::new("/bin/bash");
        command.args([
            "--noprofile",
            "--norc",
            "-c",
            "source_fd=$1; destination=$2; \
             if [[ $destination == 1 ]]; then \
               exec /bin/cat <&${source_fd}; \
             else \
               exec /bin/cat <&${source_fd} >&2; \
             fi",
            "test-output-relay",
        ]);
        command
            .arg(source_descriptor.to_string())
            .arg(destination.to_string());
        command
    };
    // SAFETY: the closure uses only async-signal-safe integer system calls.
    // The source descriptor is otherwise CLOEXEC, and parent-death signaling
    // prevents a blocked relay from outliving an abruptly lost supervisor.
    unsafe {
        command.pre_exec(move || {
            set_descriptor_cloexec(source_descriptor, false)?;
            close_absent_stdio(relay_stdio_mask)?;
            if prctl(PR_SET_PDEATHSIG, SIGKILL as c_ulong, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if getppid() as u32 != expected_parent {
                return Err(std::io::Error::from_raw_os_error(ESRCH));
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn supervised command {stream} relay: {error}"))?;
    let identity = match process_identity(child.id()) {
        Ok(Some(identity)) if identity.live => identity,
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "supervised command {stream} relay vanished during identity binding"
            ));
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    #[cfg(performance_supervisor_test)]
    if let Some(directory) = env::var_os(TEST_RELAY_PID_DIR_ENV) {
        if let Err(error) = fs::write(
            PathBuf::from(directory).join(stream),
            format!("{}\n", child.id()),
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "publish supervised command {stream} relay identity: {error}"
            ));
        }
    }
    Ok(RelayChild {
        key: ProcessKey {
            pid: child.id(),
            start: identity.start,
        },
        child,
        destination_mask,
        destination_detached: false,
        stream,
        status: None,
    })
}

impl RelayChild {
    fn refresh(&mut self) -> Result<bool, String> {
        if self.status.is_some() {
            return Ok(true);
        }
        self.status = self
            .child
            .try_wait()
            .map_err(|error| format!("reap supervised command {} relay: {error}", self.stream))?;
        Ok(self.status.is_some())
    }

    fn request_stop(&mut self) -> Result<(), String> {
        if self.refresh()? {
            return Ok(());
        }
        match self.child.kill() {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.refresh()? {
                    Ok(())
                } else {
                    Err(format!(
                        "stop supervised command {} relay: {error}",
                        self.stream
                    ))
                }
            }
        }
    }

    fn successful(&self) -> bool {
        self.status.as_ref().is_some_and(ExitStatus::success)
    }

    #[cfg(test)]
    fn detach_mask(&mut self, _mask: u8) -> Result<(), String> {
        if self.destination_detached {
            return Ok(());
        }
        // The unit-test binary calls supervise in-process and must keep the
        // harness streams live for later cases. Executable-level acceptance
        // tests exercise the real one-shot ownership handoff.
        self.destination_detached = true;
        Ok(())
    }

    #[cfg(not(test))]
    fn detach_mask(&mut self, mask: u8) -> Result<(), String> {
        let destinations = self.destination_mask & mask;
        if destinations == 0 || self.destination_detached {
            return Ok(());
        }
        for destination in 1..=2 {
            if destinations & (1 << destination) != 0 {
                if let Err(error) = detach_destination(destination, self.stream) {
                    DIAGNOSTICS_SAFE.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if destinations == self.destination_mask {
            self.destination_detached = true;
        }
        Ok(())
    }

    fn detach(&mut self) -> Result<(), String> {
        self.detach_mask(self.destination_mask)
    }
}

impl OutputRelay {
    fn start(captures: OutputCaptures) -> Result<Self, String> {
        let OutputCaptures {
            stdout,
            stderr,
            stdout_destination_mask,
        } = captures;
        let mut stdout = spawn_relay(stdout, 1, stdout_destination_mask, "stdout")?;
        let stderr = match stderr {
            Some(stderr) => match spawn_relay(stderr, 2, 0b100, "stderr") {
                Ok(stderr) => Some(stderr),
                Err(error) => {
                    let cleanup = stdout.request_stop().and_then(|()| {
                        let deadline = Instant::now() + KILL_GRACE;
                        while !stdout.refresh()? {
                            if Instant::now() >= deadline {
                                return Err(
                                    "stdout relay survived partial-startup cleanup".to_string()
                                );
                            }
                            std::thread::sleep(POLL);
                        }
                        Ok(())
                    });
                    if let Err(cleanup) = cleanup {
                        CLEANUP_INCOMPLETE.store(true, Ordering::Release);
                        return Err(format!("{error}; cleanup failed: {cleanup}"));
                    }
                    return Err(error);
                }
            },
            None => None,
        };
        Ok(Self {
            stdout,
            stderr,
            finished: false,
        })
    }

    fn keys(&self) -> Vec<ProcessKey> {
        let mut keys = vec![self.stdout.key];
        if let Some(stderr) = &self.stderr {
            keys.push(stderr.key);
        }
        keys
    }

    fn stop(&mut self) -> Result<(), String> {
        let mut first_error = None;
        if let Err(error) = self.stdout.request_stop() {
            remember_error(&mut first_error, error);
        }
        if let Some(stderr) = &mut self.stderr {
            if let Err(error) = stderr.request_stop() {
                remember_error(&mut first_error, error);
            }
        }
        let deadline = Instant::now() + KILL_GRACE;
        loop {
            let stdout_done = match self.stdout.refresh() {
                Ok(done) => done,
                Err(error) => {
                    remember_error(&mut first_error, error);
                    false
                }
            };
            let stderr_done = match &mut self.stderr {
                Some(stderr) => match stderr.refresh() {
                    Ok(done) => done,
                    Err(error) => {
                        remember_error(&mut first_error, error);
                        false
                    }
                },
                None => true,
            };
            if stdout_done && stderr_done {
                break;
            }
            if Instant::now() >= deadline {
                remember_error(
                    &mut first_error,
                    "command output relays survived forced cleanup".to_string(),
                );
                break;
            }
            std::thread::sleep(POLL);
        }
        if let Err(error) = self.stdout.detach() {
            remember_error(&mut first_error, error);
        }
        if let Some(stderr) = &mut self.stderr {
            if let Err(error) = stderr.detach() {
                remember_error(&mut first_error, error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn finish(mut self) -> Result<(), String> {
        #[cfg(performance_supervisor_test)]
        mark_test_relay_finish()?;
        loop {
            let stdout_done = self.stdout.refresh()?;
            let stderr_done = match &mut self.stderr {
                Some(stderr) => stderr.refresh()?,
                None => true,
            };
            let stderr_failed = self
                .stderr
                .as_ref()
                .is_some_and(|stderr| stderr_done && !stderr.successful());
            if (stdout_done && !self.stdout.successful()) || stderr_failed {
                self.stop()?;
                let stream = if stdout_done && !self.stdout.successful() {
                    self.stdout.stream
                } else {
                    self.stderr.as_ref().expect("failed stderr relay").stream
                };
                return Err(format!("supervised command {stream} relay failed"));
            }
            if stdout_done {
                self.stdout.detach_mask(0b010)?;
            }
            // stdout may close independently while stderr is still draining.
            // Once every relay succeeds, release every destination before any
            // later cleanup or terminal diagnostic can block on backpressure.
            if stdout_done && stderr_done {
                self.stdout.detach()?;
                if let Some(stderr) = &mut self.stderr {
                    stderr.detach()?;
                }
                self.finished = true;
                return Ok(());
            }
            if CANCELLATION_SIGNAL.load(Ordering::Relaxed) != 0 {
                return self.stop();
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(performance_supervisor_test)]
fn mark_test_relay_finish() -> Result<(), String> {
    let Some(marker) = env::var_os("DOT_PERF_TEST_RELAY_FINISH_MARKER") else {
        return Ok(());
    };
    fs::write(marker, format!("{}\n", std::process::id()))
        .map_err(|error| format!("write relay-finish marker: {error}"))
}

#[cfg(performance_supervisor_test)]
fn wait_at_test_relay_drain(stream: &str) -> Result<(), String> {
    if env::var("DOT_PERF_TEST_HOLD_RELAY").as_deref() == Ok(stream) {
        let marker = env::var_os("DOT_PERF_TEST_HOLD_RELAY_MARKER")
            .ok_or_else(|| "missing held-relay marker".to_string())?;
        let release = env::var_os("DOT_PERF_TEST_HOLD_RELAY_RELEASE")
            .ok_or_else(|| "missing held-relay release marker".to_string())?;
        fs::write(marker, format!("{}\n", std::process::id()))
            .map_err(|error| format!("write held-relay marker: {error}"))?;
        while !std::path::Path::new(&release).exists() {
            std::thread::sleep(POLL);
        }
    }
    if stream != "stdout" || env::var_os("DOT_PERF_TEST_RELAY_FINISH_MARKER").is_none() {
        return Ok(());
    }
    let release = env::var_os("DOT_PERF_TEST_RELAY_FINISH_RELEASE")
        .ok_or_else(|| "missing relay-finish release marker".to_string())?;
    while !std::path::Path::new(&release).exists() {
        std::thread::sleep(POLL);
    }
    Ok(())
}

impl Drop for OutputRelay {
    fn drop(&mut self) {
        if !self.finished && self.stop().is_err() {
            CLEANUP_INCOMPLETE.store(true, Ordering::Release);
        }
    }
}

enum LeaderEvent {
    Exited,
    Cancelled(c_int),
}

extern "C" fn record_cancellation(requested_signal: c_int) {
    let _ = CANCELLATION_SIGNAL.compare_exchange(
        0,
        requested_signal,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    if START_BARRIER_STATE
        .compare_exchange(
            START_BARRIER_WAITING,
            START_BARRIER_CANCELLED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
    {
        let descriptor = START_BARRIER_WRITE_FD.load(Ordering::SeqCst);
        if descriptor >= 0 {
            // SAFETY: write is async-signal-safe. The descriptor is retained
            // until the supervisor observes the cancelled barrier state.
            let _ = unsafe { write(descriptor, &CANCEL_TOKEN, 1) };
        }
    }
}

extern "C" fn terminal_signal_exit(requested_signal: c_int) {
    let status = if CLEANUP_INCOMPLETE.load(Ordering::Acquire) {
        70
    } else {
        128 + requested_signal
    };
    TERMINAL_EXIT_STATUS.store(status, Ordering::Release);
    // SAFETY: _exit is async-signal-safe and terminates without running code
    // that could reopen the completed supervision lifecycle.
    unsafe { _exit(status) }
}

fn install_cancellation_handlers() -> Result<(), String> {
    CANCELLATION_SIGNAL.store(0, Ordering::Relaxed);
    for requested_signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        // SAFETY: record_cancellation only performs a lock-free atomic store.
        let previous =
            unsafe { signal(requested_signal, record_cancellation as *const () as usize) };
        if previous == usize::MAX {
            return Err(format!(
                "install cancellation handler for signal {requested_signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn install_terminal_handlers() -> Result<(), String> {
    for requested_signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        // SAFETY: terminal_signal_exit performs only lock-free atomic accesses
        // followed by the async-signal-safe _exit system interface.
        let previous =
            unsafe { signal(requested_signal, terminal_signal_exit as *const () as usize) };
        if previous == usize::MAX {
            return Err(format!(
                "install terminal handler for signal {requested_signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn emit_bounded_bytes(descriptor: c_int, bytes: &[u8]) {
    // Every supervisor diagnostic travels this path, including usage errors
    // emitted before the first fallible operation. A blocking write to a full
    // downstream pipe would retain the reporting process past any watchdog
    // while holding its inherited lock descriptors, and killing the driver
    // cannot recover a process stuck in write. Poll-gated emission preserves
    // the full diagnostic on writable sinks (regular files and live pipes
    // report POLLOUT immediately) while giving up within a fixed grace on a
    // blocked or vanished sink. This is best effort by design: it never fails.
    if bytes.is_empty() {
        return;
    }
    let deadline = Instant::now() + BOUNDED_DIAGNOSTIC_GRACE;
    let mut written = 0;
    while written < bytes.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let mut descriptor_poll = PollFd {
            fd: descriptor,
            events: POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor_poll points to one initialized pollfd.
        let ready = unsafe {
            poll(
                &mut descriptor_poll,
                1,
                remaining.as_millis().min(c_int::MAX as u128) as c_int,
            )
        };
        if ready < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
            return;
        }
        if ready == 0 {
            return;
        }
        if descriptor_poll.revents & POLLNVAL != 0 {
            return;
        }
        if descriptor_poll.revents & (POLLOUT | POLLERR | POLLHUP) == 0 {
            continue;
        }
        // SAFETY: bytes holds initialized storage for the unwritten suffix.
        let count = unsafe { write(descriptor, bytes[written..].as_ptr(), bytes.len() - written) };
        if count > 0 {
            written += count as usize;
            continue;
        }
        if count < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(EINTR | EAGAIN) => continue,
                _ => return,
            }
        }
        std::thread::sleep(POLL);
    }
}

fn emit_bounded_line(descriptor: c_int, message: &str) {
    emit_bounded_bytes(descriptor, format!("{message}\n").as_bytes());
}

fn emit_bounded_diagnostic(descriptor: c_int, message: &str) {
    emit_bounded_bytes(descriptor, format!("error: {message}\n").as_bytes());
}

fn report_error(error: &str) {
    if DIAGNOSTICS_SAFE.load(Ordering::Acquire) {
        emit_bounded_diagnostic(2, error);
    }
}

fn terminal_handoff(base_status: c_int) -> ! {
    let signal_mask = match SignalMaskGuard::block() {
        Ok(signal_mask) => signal_mask,
        Err(error) => {
            report_error(&error);
            // SAFETY: no supervised descendants or relay threads remain and
            // _exit avoids reopening teardown work after this fatal failure.
            unsafe { _exit(70) }
        }
    };
    let (pending, pending_failed) = match pending_cancellation() {
        Ok(pending) => (pending, false),
        Err(error) => {
            report_error(&error);
            (None, true)
        }
    };
    let status = if CLEANUP_INCOMPLETE.load(Ordering::Acquire) || pending_failed {
        70
    } else if let Some(requested_signal) = pending {
        128 + requested_signal
    } else {
        base_status
    };
    TERMINAL_EXIT_STATUS.store(status, Ordering::Release);
    if let Err(error) = install_terminal_handlers() {
        report_error(&error);
        // SAFETY: cancellation signals remain blocked and no supervised
        // process or relay thread remains alive.
        unsafe { _exit(70) }
    }
    // The terminal handler, rather than Rust cleanup, owns any signal that
    // became pending after the status snapshot. It cannot report success for
    // that signal and cannot lose cleanup-incomplete status 70.
    std::mem::forget(signal_mask);
    if let Err(error) = unblock_cancellation_signals() {
        report_error(&error);
        TERMINAL_EXIT_STATUS.store(70, Ordering::Release);
        // SAFETY: no supervised process or relay thread remains alive.
        unsafe { _exit(70) }
    }
    // SAFETY: all descendants and relay threads have completed, and the
    // terminal signal handler is active for the final instruction window.
    unsafe { _exit(status) }
}

fn wait_for_leader(
    boundary: &ProcessBoundary,
    parent: &ParentBoundary,
) -> Result<LeaderEvent, String> {
    loop {
        let requested_signal = CANCELLATION_SIGNAL.load(Ordering::Relaxed);
        if requested_signal != 0 {
            return Ok(LeaderEvent::Cancelled(requested_signal));
        }
        let mut descriptors = [
            PollFd {
                fd: boundary.leader.pidfd.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: parent.member.pidfd.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: descriptors contains two initialized pollfd records.
        let result = unsafe { poll(descriptors.as_mut_ptr(), 2, LEADER_POLL) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!("poll supervised identities: {error}"));
        }
        if descriptors
            .iter()
            .any(|descriptor| descriptor.revents & POLLNVAL != 0)
        {
            return Err("supervised identity handle became invalid".to_string());
        }
        if descriptors[1].revents & (POLLIN | POLLERR | POLLHUP) != 0 {
            record_cancellation(SIGTERM);
            return Ok(LeaderEvent::Cancelled(SIGTERM));
        }
        if descriptors[0].revents & (POLLIN | POLLERR | POLLHUP) != 0 {
            let requested_signal = CANCELLATION_SIGNAL.load(Ordering::Relaxed);
            return if requested_signal == 0 {
                Ok(LeaderEvent::Exited)
            } else {
                Ok(LeaderEvent::Cancelled(requested_signal))
            };
        }
    }
}

enum Quiescence {
    Complete,
    TimedOut,
    Cancelled(c_int),
}

fn quiescent(boundary: &mut ProcessBoundary, deadline: Instant) -> Result<Quiescence, String> {
    let mut empty = 0;
    loop {
        let requested_signal = CANCELLATION_SIGNAL.load(Ordering::Relaxed);
        if requested_signal != 0 {
            return Ok(Quiescence::Cancelled(requested_signal));
        }
        if discover_members(boundary)? == 0 {
            empty += 1;
            if empty == 2 {
                return Ok(Quiescence::Complete);
            }
        } else {
            empty = 0;
        }
        if Instant::now() >= deadline {
            return Ok(Quiescence::TimedOut);
        }
        std::thread::sleep(POLL);
    }
}

fn reap_adopted_children(deadline: Instant) -> Result<(), String> {
    loop {
        let mut status = 0;
        // SAFETY: waitpid(-1, WNOHANG) reaps only children of this supervisor.
        let result = unsafe { waitpid(-1, &mut status, WNOHANG) };
        if result > 0 {
            continue;
        }
        if result == 0 {
            if Instant::now() >= deadline {
                return Err("timed out reaping command descendants".to_string());
            }
            std::thread::sleep(POLL);
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ECHILD) {
            return Ok(());
        }
        return Err(format!("reap command descendant: {error}"));
    }
}

fn remember_error(first_error: &mut Option<String>, error: String) {
    if first_error.is_none() {
        *first_error = Some(error);
    }
}

fn signal_authority(
    boundary: &ProcessBoundary,
    requested_signal: c_int,
    first_error: &mut Option<String>,
) {
    if let Err(error) = signal_leader_group(boundary, requested_signal) {
        remember_error(first_error, error);
    }
    if let Err(error) = signal_known_members(boundary, requested_signal) {
        remember_error(first_error, error);
    }
}

fn reap_leader(
    child: &mut Child,
    boundary: &mut ProcessBoundary,
    deadline: Instant,
) -> Result<ExitStatus, String> {
    loop {
        if pidfd_ready(&boundary.leader.pidfd, 0)? {
            let status = child
                .wait()
                .map_err(|error| format!("reap command leader: {error}"))?;
            boundary.leader_reaped = true;
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("timed out reaping command leader".to_string());
        }
        std::thread::sleep(POLL);
    }
}

fn leader_ready_for_cleanup(boundary: &ProcessBoundary, first_error: &mut Option<String>) -> bool {
    match pidfd_ready(&boundary.leader.pidfd, 0) {
        Ok(ready) => ready,
        Err(error) => {
            remember_error(first_error, error);
            false
        }
    }
}

fn prune_exited_members(boundary: &mut ProcessBoundary) -> Result<usize, String> {
    let mut exited = Vec::new();
    for (&key, pidfd) in &boundary.members {
        if pidfd_ready(pidfd, 0)? {
            exited.push(key);
        }
    }
    for key in exited {
        boundary.members.remove(&key);
    }
    Ok(boundary.members.len())
}

fn cleanup_boundary(
    child: &mut Child,
    boundary: &mut ProcessBoundary,
    relay: &mut OutputRelay,
) -> Result<(), String> {
    let mut first_error = None;
    if let Err(error) = discover_members(boundary) {
        remember_error(&mut first_error, error);
    }
    signal_authority(boundary, SIGTERM, &mut first_error);
    let term_deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < term_deadline {
        match discover_members(boundary) {
            Ok(_) => {
                if let Err(error) = signal_known_members(boundary, SIGTERM) {
                    remember_error(&mut first_error, error);
                }
            }
            Err(error) => remember_error(&mut first_error, error),
        }
        let live_members = match prune_exited_members(boundary) {
            Ok(count) => count,
            Err(error) => {
                remember_error(&mut first_error, error);
                boundary.members.len()
            }
        };
        if leader_ready_for_cleanup(boundary, &mut first_error) && live_members == 0 {
            break;
        }
        std::thread::sleep(POLL);
    }

    signal_authority(boundary, SIGKILL, &mut first_error);
    let kill_deadline = Instant::now() + KILL_GRACE;
    let mut empty = 0;
    loop {
        match discover_members(boundary) {
            Ok(_) => {
                if let Err(error) = signal_known_members(boundary, SIGKILL) {
                    remember_error(&mut first_error, error);
                }
                let live_members = match prune_exited_members(boundary) {
                    Ok(count) => count,
                    Err(error) => {
                        remember_error(&mut first_error, error);
                        boundary.members.len()
                    }
                };
                if live_members == 0 && leader_ready_for_cleanup(boundary, &mut first_error) {
                    empty += 1;
                    if empty == 2 {
                        break;
                    }
                } else {
                    empty = 0;
                }
            }
            Err(error) => {
                remember_error(&mut first_error, error);
                if let Err(error) = signal_known_members(boundary, SIGKILL) {
                    remember_error(&mut first_error, error);
                }
                if let Err(error) = prune_exited_members(boundary) {
                    remember_error(&mut first_error, error);
                }
            }
        }
        if Instant::now() >= kill_deadline {
            remember_error(
                &mut first_error,
                "command retained descendants after forced cleanup".to_string(),
            );
            break;
        }
        std::thread::sleep(POLL);
    }
    let reap_deadline = Instant::now() + KILL_GRACE;
    if let Err(error) = reap_leader(child, boundary, reap_deadline) {
        remember_error(&mut first_error, error);
    }
    let relays_stopped = match relay.stop() {
        Ok(()) => true,
        Err(error) => {
            remember_error(&mut first_error, error);
            false
        }
    };
    if boundary.leader_reaped && relays_stopped {
        if let Err(error) = reap_adopted_children(reap_deadline) {
            remember_error(&mut first_error, error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn reject_after_spawn(
    child: &mut Child,
    boundary: &mut ProcessBoundary,
    relay: &mut OutputRelay,
    cause: String,
) -> Result<ExitStatus, String> {
    match cleanup_boundary(child, boundary, relay) {
        Ok(()) => Err(cause),
        Err(cleanup) => {
            CLEANUP_INCOMPLETE.store(true, Ordering::Release);
            Err(format!("{cause}; cleanup failed: {cleanup}"))
        }
    }
}

fn configure_output_captures(
    command: &mut Command,
    aliased: bool,
) -> Result<OutputCaptures, String> {
    let (stdout_read, stdout_write) =
        internal_pipe("supervised stdout capture", "supervised stdout writer")?;
    #[cfg(performance_supervisor_test)]
    if env::var(TEST_FAIL_OUTPUT_CAPTURE_ENV).as_deref() == Ok("stdout") {
        return Err("injected supervised stdout capture failure".to_string());
    }
    if aliased {
        let stderr_write = duplicate_internal_fd(&stdout_write, "aliased stderr writer")?;
        command.stdout(Stdio::from(stdout_write));
        command.stderr(Stdio::from(stderr_write));
        return Ok(OutputCaptures {
            stdout: File::from(stdout_read),
            stderr: None,
            stdout_destination_mask: 0b110,
        });
    }
    let (stderr_read, stderr_write) =
        internal_pipe("supervised stderr capture", "supervised stderr writer")?;
    #[cfg(performance_supervisor_test)]
    if env::var(TEST_FAIL_OUTPUT_CAPTURE_ENV).as_deref() == Ok("stderr") {
        return Err("injected supervised stderr capture failure".to_string());
    }
    command.stdout(Stdio::from(stdout_write));
    command.stderr(Stdio::from(stderr_write));
    Ok(OutputCaptures {
        stdout: File::from(stdout_read),
        stderr: Some(File::from(stderr_read)),
        stdout_destination_mask: 0b010,
    })
}

fn stopped_command(
    program: OsString,
    arguments: Vec<OsString>,
    cwd: Option<PathBuf>,
    start_read: c_int,
    start_write: c_int,
    stdio_mask: u8,
) -> Result<Command, String> {
    #[cfg(not(test))]
    let mut command = {
        let supervisor = env::current_exe()
            .map_err(|error| format!("resolve command supervisor executable: {error}"))?;
        let mut command = Command::new(supervisor);
        command.args([
            OsString::from("--command-child"),
            OsString::from(start_read.to_string()),
            OsString::from(start_write.to_string()),
            OsString::from(stdio_mask.to_string()),
            OsString::from("--"),
        ]);
        command
    };
    #[cfg(test)]
    let mut command = {
        let mut command = Command::new("/bin/bash");
        command.args([
            "--noprofile",
            "--norc",
            "-c",
            "read_fd=$1; write_fd=$2; stdio_mask=$3; shift 3; eval \"exec ${write_fd}>&-\"; \
             kill -STOP $$; IFS= read -r -N 1 token <&${read_fd}; \
             eval \"exec ${read_fd}<&-\"; [[ $token == S ]] || exit 125; \
             ((stdio_mask & 1)) || exec 0<&-; ((stdio_mask & 2)) || exec 1>&-; \
             ((stdio_mask & 4)) || exec 2>&-; exec \"$@\"",
            "test-supervisor",
        ]);
        command
            .arg(start_read.to_string())
            .arg(start_write.to_string())
            .arg(stdio_mask.to_string());
        command
    };
    command.arg(program).args(arguments);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    Ok(command)
}

#[cfg(performance_supervisor_test)]
fn wait_at_test_start_barrier(child: u32) -> Result<(), String> {
    let Some(marker) = env::var_os("DOT_PERF_TEST_START_BARRIER") else {
        return Ok(());
    };
    fs::write(&marker, format!("{child}\n"))
        .map_err(|error| format!("write start-barrier test marker: {error}"))?;
    while pending_cancellation()?.is_none() {
        std::thread::sleep(POLL);
    }
    Ok(())
}

#[cfg(performance_supervisor_test)]
fn exit_at_test_guardian_phase(phase: &str) {
    if env::var(TEST_GUARDIAN_EXIT_PHASE_ENV).as_deref() == Ok(phase) {
        wait_at_test_phase(phase);
        // SAFETY: this deterministic crash fixture deliberately bypasses all
        // guardian cleanup so the coordinator's fallback owns it.
        unsafe { _exit(91) }
    }
}

#[cfg(not(performance_supervisor_test))]
fn exit_at_test_guardian_phase(_phase: &str) {}

#[cfg(performance_supervisor_test)]
fn fail_at_test_coordinator_phase(phase: &str) -> Result<(), String> {
    if env::var(TEST_COORDINATOR_FAIL_PHASE_ENV).as_deref() == Ok(phase) {
        wait_at_test_phase(phase);
        Err(format!("injected coordinator failure during {phase}"))
    } else {
        Ok(())
    }
}

#[cfg(not(performance_supervisor_test))]
fn fail_at_test_coordinator_phase(_phase: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(performance_supervisor_test)]
fn setup_grace() -> Duration {
    if let Ok(value) = env::var(TEST_SETUP_GRACE_ENV) {
        if let Ok(milliseconds) = value.parse::<u64>() {
            return Duration::from_millis(milliseconds.max(1));
        }
    }
    SETUP_GRACE
}

#[cfg(not(performance_supervisor_test))]
fn setup_grace() -> Duration {
    SETUP_GRACE
}

#[cfg(performance_supervisor_test)]
fn stall_at_test_guardian_phase(phase: &str, target: Option<ProcessKey>) {
    // A true setup hang: the guardian stops responding to the coordinator
    // without exiting, so the coordinator-side setup deadline owns teardown.
    // A stalled pre-publication guardian also publishes the adopted target
    // identity it holds, since the coordinator never learns it.
    if env::var(TEST_STALL_GUARDIAN_PHASE_ENV).as_deref() != Ok(phase) {
        return;
    }
    if let (Some(target), Some(path)) = (target, env::var_os(TEST_GUARDIAN_TARGET_PID_FILE_ENV)) {
        let _ = fs::write(path, format!("{}\n", target.pid));
    }
    loop {
        std::thread::sleep(POLL);
    }
}

#[cfg(not(performance_supervisor_test))]
fn stall_at_test_guardian_phase(_phase: &str, _target: Option<ProcessKey>) {}

#[cfg(performance_supervisor_test)]
fn stall_at_test_coordinator_phase(phase: &str) {
    // The reciprocal hang: the coordinator withholds authorization so the
    // guardian-side authorization deadline owns teardown. When the test
    // provides the shared phase gate, the stall holds only until release so
    // the test can observe the guardian timeout first and then verify the
    // coordinator still fails closed; without a gate it hangs outright.
    if env::var(TEST_STALL_COORDINATOR_PHASE_ENV).as_deref() != Ok(phase) {
        return;
    }
    if env::var_os(TEST_PHASE_MARKER_ENV).is_some() && env::var_os(TEST_PHASE_RELEASE_ENV).is_some()
    {
        wait_at_test_phase(phase);
    } else {
        loop {
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(not(performance_supervisor_test))]
fn stall_at_test_coordinator_phase(_phase: &str) {}

#[cfg(performance_supervisor_test)]
fn hang_at_test_guardian_cancellation() {
    if env::var_os(TEST_HANG_GUARDIAN_ON_CANCEL_ENV).is_some() {
        loop {
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(not(performance_supervisor_test))]
fn hang_at_test_guardian_cancellation() {}

#[cfg(performance_supervisor_test)]
fn take_test_fault(variable: &str, used: &AtomicBool) -> bool {
    env::var_os(variable).is_some() && !used.swap(true, Ordering::SeqCst)
}

#[cfg(performance_supervisor_test)]
fn wait_at_test_phase(phase: &str) {
    let (Some(marker), Some(release)) = (
        env::var_os(TEST_PHASE_MARKER_ENV),
        env::var_os(TEST_PHASE_RELEASE_ENV),
    ) else {
        return;
    };
    if fs::write(marker, format!("{phase}\n")).is_err() {
        return;
    }
    while !std::path::Path::new(&release).exists() {
        std::thread::sleep(POLL);
    }
}

fn supervise_for_parent(
    expected_parent: u32,
    cwd: Option<PathBuf>,
    program: OsString,
    arguments: Vec<OsString>,
    guardian_handshake: Option<GuardianHandshake>,
) -> Result<ExitStatus, String> {
    CLEANUP_INCOMPLETE.store(false, Ordering::Release);
    #[cfg(performance_supervisor_test)]
    TEST_SCAN_PHASE.store(0, Ordering::Relaxed);
    let stdio_mask = inherited_stdio_mask();
    let output_is_aliased = outputs_aliased(stdio_mask)?;
    let sigchld = SigchldGuard::normalize()?;
    let mut signal_mask = SignalMaskGuard::block()?;
    enable_subreaper()?;
    install_cancellation_handlers()?;
    let parent = bind_parent(expected_parent)?;
    exit_at_test_guardian_phase("arming");
    if let Some(handshake) = &guardian_handshake {
        stall_at_test_guardian_phase("pre-arm", None);
        handshake.arm(&parent)?;
    }
    let mut output_guard = GuardianOutputGuard::new(guardian_handshake.is_some());
    // Every fallible step before the relay handoff runs inside the output
    // guard so a setup failure emits its actionable diagnostic while the
    // inherited sink is still attached, before the detach that keeps blocked
    // sinks bounded.
    let (mut child, mut relay, mut baseline_direct, mut start_barrier) = match output_guard
        .run_guarded_setup(|| {
            let supervisor = std::process::id();
            let baseline_direct = match direct_children(supervisor)? {
                Some(children) => children,
                None => process_table()?
                    .into_iter()
                    .filter_map(|(pid, identity)| {
                        (identity.parent == supervisor).then_some(ProcessKey {
                            pid,
                            start: identity.start,
                        })
                    })
                    .collect(),
            };
            let start_barrier = StartBarrier::new()?;
            let (start_read, start_write) = start_barrier.child_descriptors();
            let mut command =
                stopped_command(program, arguments, cwd, start_read, start_write, stdio_mask)?;
            let output_captures = configure_output_captures(&mut command, output_is_aliased)?;
            let child_ignores_sigchld = sigchld.child_ignored();
            // SAFETY: setsid has no memory arguments and is async-signal-safe between
            // fork and exec. The re-executed supervisor child stops only after
            // Command::spawn observes a successful exec, avoiding a pre-exec
            // synchronization deadlock.
            unsafe {
                command.pre_exec(move || {
                    set_descriptor_cloexec(start_read, false)?;
                    set_descriptor_cloexec(start_write, false)?;
                    close_absent_stdio(stdio_mask)?;
                    if setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // SIGCHLD ignore is the only disposition bit that survives the
                    // caller's exec into this supervisor. Restore it immediately
                    // before the command-child exec; default needs no extra window.
                    if child_ignores_sigchld && signal(SIGCHLD, SIG_IGN) == SIGNAL_ERROR {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command
                .spawn()
                .map_err(|error| format!("spawn supervised command: {error}"))?;
            // Command retains caller-supplied Stdio handles so it can be spawned more
            // than once. This supervisor launches exactly once; release those writer
            // copies now so relay EOF depends only on the target process tree.
            drop(command);
            let relay = match OutputRelay::start(output_captures) {
                Ok(relay) => relay,
                Err(error) => {
                    let _ = unsafe { kill(child.id() as c_int, SIGKILL) };
                    let _ = child.wait();
                    return Err(error);
                }
            };
            Ok((child, relay, baseline_direct, start_barrier))
        }) {
        Ok(setup) => setup,
        Err(error) => return Err(error),
    };
    output_guard.transfer();
    baseline_direct.extend(relay.keys());
    start_barrier.spawned();
    let mut stopped_status = 0;
    // SAFETY: this waits only for the exact direct child and retains it for
    // the later std::process::Child wait.
    let stopped = unsafe { waitpid(child.id() as c_int, &mut stopped_status, WUNTRACED) };
    if stopped != child.id() as c_int || stopped_status & 0xff != 0x7f {
        // No arbitrary code has run before the start barrier. The child is the
        // still-live session leader, so this exact positive-PID signal is safe.
        let _ = unsafe { kill(child.id() as c_int, SIGKILL) };
        let _ = child.wait();
        return Err("command leader did not enter the start barrier".to_string());
    }
    let mut boundary = match boundary(child.id(), baseline_direct) {
        Ok(boundary) => boundary,
        Err(error) => {
            let _ = unsafe { kill(child.id() as c_int, SIGKILL) };
            let _ = child.wait();
            return Err(error);
        }
    };
    if let Some(handshake) = &guardian_handshake {
        stall_at_test_guardian_phase("pre-publish", Some(boundary.leader.key));
        if let Err(error) = handshake.publish_target(&parent, boundary.leader.key) {
            let _ = start_barrier.cancel();
            let result = reject_after_spawn(&mut child, &mut boundary, &mut relay, error);
            return finish_guardian(&guardian_handshake, result);
        }
        exit_at_test_guardian_phase("completion");
    }
    let mut cancellation = match pending_cancellation() {
        Ok(cancellation) => cancellation,
        Err(error) => {
            let _ = start_barrier.cancel();
            let rejection = reject_after_spawn(&mut child, &mut boundary, &mut relay, error);
            signal_mask.restore()?;
            return finish_guardian(&guardian_handshake, rejection);
        }
    };
    if cancellation.is_none() {
        match pidfd_ready(&parent.member.pidfd, 0) {
            Ok(true) => cancellation = Some(SIGTERM),
            Ok(false) => {}
            Err(error) => {
                let _ = start_barrier.cancel();
                let rejection = reject_after_spawn(&mut child, &mut boundary, &mut relay, error);
                signal_mask.restore()?;
                return finish_guardian(&guardian_handshake, rejection);
            }
        }
    }
    if let Some(requested_signal) = cancellation {
        record_cancellation(requested_signal);
        let _ = start_barrier.cancel();
        let rejection = reject_after_spawn(
            &mut child,
            &mut boundary,
            &mut relay,
            format!("supervised command cancelled by signal {requested_signal}"),
        );
        signal_mask.restore()?;
        return finish_guardian(&guardian_handshake, rejection);
    }
    // This test-only stop is deliberately after the initial pending-signal
    // inspection. It exercises the otherwise microscopic arrival window
    // immediately before SIGCONT.
    #[cfg(performance_supervisor_test)]
    if let Err(error) = wait_at_test_start_barrier(child.id()) {
        let _ = start_barrier.cancel();
        let rejection = reject_after_spawn(&mut child, &mut boundary, &mut relay, error);
        signal_mask.restore()?;
        return finish_guardian(&guardian_handshake, rejection);
    }
    if let Err(error) = signal_pidfd(boundary.leader.key, &boundary.leader.pidfd, SIGCONT) {
        let _ = start_barrier.cancel();
        return finish_guardian(
            &guardian_handshake,
            reject_after_spawn(&mut child, &mut boundary, &mut relay, error),
        );
    }
    if let Err(error) = signal_mask.restore() {
        let _ = start_barrier.cancel();
        return finish_guardian(
            &guardian_handshake,
            reject_after_spawn(&mut child, &mut boundary, &mut relay, error),
        );
    }
    match start_barrier.authorize() {
        Ok(true) => {}
        Ok(false) => {
            let requested_signal = CANCELLATION_SIGNAL.load(Ordering::Relaxed);
            let requested_signal = if requested_signal == 0 {
                SIGTERM
            } else {
                requested_signal
            };
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(
                    &mut child,
                    &mut boundary,
                    &mut relay,
                    format!("supervised command cancelled by signal {requested_signal}"),
                ),
            );
        }
        Err(error) => {
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(&mut child, &mut boundary, &mut relay, error),
            );
        }
    }

    match wait_for_leader(&boundary, &parent) {
        Err(error) => {
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(&mut child, &mut boundary, &mut relay, error),
            );
        }
        Ok(LeaderEvent::Exited) => {}
        Ok(LeaderEvent::Cancelled(requested_signal)) => {
            hang_at_test_guardian_cancellation();
            let mut cause = format!("supervised command cancelled by signal {requested_signal}");
            if let Some(handshake) = &guardian_handshake {
                if let Err(error) = handshake.acknowledge_cancellation(requested_signal) {
                    cause = format!("{cause}; {error}");
                }
            }
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(&mut child, &mut boundary, &mut relay, cause),
            );
        }
    }
    match quiescent(&mut boundary, Instant::now() + QUIESCENCE_GRACE) {
        Ok(Quiescence::Complete) => {}
        Ok(Quiescence::TimedOut) => {
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(
                    &mut child,
                    &mut boundary,
                    &mut relay,
                    "command leader exited while a descendant remained live".to_string(),
                ),
            );
        }
        Ok(Quiescence::Cancelled(requested_signal)) => {
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(
                    &mut child,
                    &mut boundary,
                    &mut relay,
                    format!("supervised command cancelled by signal {requested_signal}"),
                ),
            );
        }
        Err(error) => {
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(&mut child, &mut boundary, &mut relay, error),
            );
        }
    }
    let status = match reap_leader(&mut child, &mut boundary, Instant::now() + KILL_GRACE) {
        Ok(status) => status,
        Err(error) => {
            return finish_guardian(
                &guardian_handshake,
                reject_after_spawn(&mut child, &mut boundary, &mut relay, error),
            );
        }
    };
    // Relay children are excluded from target ownership and must be reaped
    // before the generic adopted-child sweep can consume their exact status.
    let relay_result = relay.finish();
    let adopted_result = reap_adopted_children(Instant::now() + KILL_GRACE);
    let requested_signal = CANCELLATION_SIGNAL.load(Ordering::Relaxed);
    if requested_signal != 0 {
        return finish_guardian(
            &guardian_handshake,
            Err(format!(
                "supervised command cancelled by signal {requested_signal}"
            )),
        );
    }
    let result = relay_result.and(adopted_result).map(|()| status);
    finish_guardian(&guardian_handshake, result)
}

fn finish_guardian<T>(
    handshake: &Option<GuardianHandshake>,
    result: Result<T, String>,
) -> Result<T, String> {
    let Some(handshake) = handshake else {
        return result;
    };
    let completion = handshake.complete();
    #[cfg(performance_supervisor_test)]
    if completion.is_ok() && env::var_os(TEST_HANG_GUARDIAN_AFTER_DONE_ENV).is_some() {
        loop {
            std::thread::sleep(POLL);
        }
    }
    match (result, completion) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(completion)) => Err(format!("{error}; {completion}")),
    }
}

enum GuardianFrame {
    Bytes,
    Cancelled(c_int),
    Exited(ExitStatus),
    TimedOut,
}

fn read_guardian_frame(
    child: &mut Child,
    ready: &OwnedFd,
    parent: &ParentBoundary,
    frame: &mut [u8],
    observe_cancellation: bool,
    deadline: Option<Instant>,
) -> Result<GuardianFrame, String> {
    let mut received = 0;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(GuardianFrame::TimedOut);
        }
        if observe_cancellation {
            if let Some(requested_signal) = pending_cancellation()? {
                return Ok(GuardianFrame::Cancelled(requested_signal));
            }
            if pidfd_ready(&parent.member.pidfd, 0)? {
                record_cancellation(SIGTERM);
                return Ok(GuardianFrame::Cancelled(SIGTERM));
            }
        }
        let mut descriptor = PollFd {
            fd: ready.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd.
        let polled = unsafe { poll(&mut descriptor, 1, POLL.as_millis() as c_int) };
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(format!("poll cleanup-guardian handshake: {error}"));
        }
        if descriptor.revents & POLLNVAL != 0 {
            return Err("cleanup-guardian handshake descriptor became invalid".to_string());
        }
        if polled > 0 && descriptor.revents & (POLLIN | POLLERR | POLLHUP) != 0 {
            // SAFETY: frame has writable storage for the remaining bytes.
            let count = unsafe {
                read(
                    ready.as_raw_fd(),
                    frame[received..].as_mut_ptr(),
                    frame.len() - received,
                )
            };
            if count > 0 {
                received += count as usize;
                if received == frame.len() {
                    return Ok(GuardianFrame::Bytes);
                }
                continue;
            }
            if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
                continue;
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect cleanup guardian: {error}"))?
        {
            return Ok(GuardianFrame::Exited(status));
        }
        if polled > 0 {
            return Err("cleanup guardian closed an incomplete handshake frame".to_string());
        }
    }
}

fn write_guardian_token(descriptor: &OwnedFd, token: u8, phase: &str) -> Result<(), String> {
    loop {
        // SAFETY: token points to one readable byte and descriptor is live.
        let count = unsafe { write(descriptor.as_raw_fd(), &token, 1) };
        if count == 1 {
            return Ok(());
        }
        if count < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
            continue;
        }
        return Err(format!(
            "authorize cleanup guardian during {phase}: {}",
            std::io::Error::last_os_error()
        ));
    }
}

fn exit_status_code(status: ExitStatus) -> c_int {
    if status.success() {
        0
    } else {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
            .clamp(1, 255)
    }
}

fn stop_unarmed_guardian(child: &mut Child, requested_signal: c_int) -> Result<ExitStatus, String> {
    // The authorization writer is closed by the caller first. Therefore the
    // guardian cannot release arbitrary target code even if this signal is
    // delayed across its exec boundary.
    // SAFETY: an unreaped Child reserves its numeric PID against reuse.
    let result = unsafe { kill(child.id() as c_int, requested_signal) };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(ESRCH) {
        return Err(format!(
            "stop unarmed cleanup guardian: {}",
            std::io::Error::last_os_error()
        ));
    }
    let deadline = Instant::now() + KILL_GRACE;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("reap unarmed cleanup guardian: {error}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .map_err(|error| format!("kill unarmed cleanup guardian: {error}"))?;
            let kill_deadline = Instant::now() + KILL_GRACE;
            loop {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|error| format!("reap killed cleanup guardian: {error}"))?
                {
                    return Ok(status);
                }
                if Instant::now() >= kill_deadline {
                    return Err("killed cleanup guardian could not be proven reaped".to_string());
                }
                std::thread::sleep(POLL);
            }
        }
        std::thread::sleep(POLL);
    }
}

fn detach_coordinator_outputs() -> Result<(), String> {
    let mut first_error = None;
    for (destination, stream) in [(1, "coordinator stdout"), (2, "coordinator stderr")] {
        if let Err(error) = detach_destination(destination, stream) {
            DIAGNOSTICS_SAFE.store(false, Ordering::Release);
            remember_error(&mut first_error, error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

impl GuardianOutputGuard {
    fn new(active: bool) -> Self {
        Self { active }
    }

    fn run_guarded_setup<T>(
        &mut self,
        setup: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        match setup() {
            Ok(value) => Ok(value),
            Err(error) => {
                if self.active {
                    // Emit the actionable diagnostic while the inherited sink
                    // is still attached, then detach so a blocked sink cannot
                    // retain this process. The emission is bounded: writable
                    // sinks receive the full message and blocked sinks cost at
                    // most the diagnostic grace. The terminal report after the
                    // detach reaches only the relay terminal sink.
                    emit_bounded_diagnostic(2, &error);
                    let _ = detach_coordinator_outputs();
                    self.active = false;
                }
                Err(error)
            }
        }
    }

    fn transfer(&mut self) {
        self.active = false;
    }
}

impl Drop for GuardianOutputGuard {
    fn drop(&mut self) {
        if self.active && detach_coordinator_outputs().is_err() {
            DIAGNOSTICS_SAFE.store(false, Ordering::Release);
        }
    }
}

fn cleanup_adopted_after(baseline: &HashSet<ProcessKey>, minimum_start: u64) -> Result<(), String> {
    let supervisor = std::process::id();
    let deadline = Instant::now() + KILL_GRACE;
    let mut empty = 0;
    loop {
        let direct = match direct_children(supervisor)? {
            Some(children) => children,
            None => process_table()?
                .into_iter()
                .filter_map(|(pid, identity)| {
                    (identity.parent == supervisor).then_some(ProcessKey {
                        pid,
                        start: identity.start,
                    })
                })
                .collect(),
        };
        let mut live = 0;
        for key in direct {
            if key.start < minimum_start || baseline.contains(&key) {
                continue;
            }
            let Some(identity) = process_identity(key.pid)? else {
                continue;
            };
            if !identity.live || !identity_matches(key, &identity) {
                continue;
            }
            live += 1;
            if let Some(member) = open_member(key.pid, &identity)? {
                let _ = signal_pidfd(member.key, &member.pidfd, SIGCONT);
                signal_pidfd(member.key, &member.pidfd, SIGKILL)?;
            }
        }
        let mut status = 0;
        loop {
            // SAFETY: the coordinator is a dedicated subreaper and owns every
            // non-baseline child created after the guardian was spawned.
            let reaped = unsafe { waitpid(-1, &mut status, WNOHANG) };
            if reaped > 0 {
                continue;
            }
            if reaped < 0 && std::io::Error::last_os_error().raw_os_error() != Some(ECHILD) {
                return Err(format!(
                    "reap cleanup-guardian descendants: {}",
                    std::io::Error::last_os_error()
                ));
            }
            break;
        }
        if live == 0 {
            empty += 1;
            if empty == 2 {
                return Ok(());
            }
        } else {
            empty = 0;
        }
        if Instant::now() >= deadline {
            return Err("cleanup guardian left adopted descendants alive".to_string());
        }
        std::thread::sleep(POLL);
    }
}

fn cleanup_after_guardian_loss(
    boundary: &mut Option<ProcessBoundary>,
    baseline: &HashSet<ProcessKey>,
    minimum_start: u64,
) -> Result<(), String> {
    let mut first_error = None;
    if let Some(boundary) = boundary {
        if let Err(error) = discover_members(boundary) {
            remember_error(&mut first_error, error);
        }
        signal_authority(boundary, SIGTERM, &mut first_error);
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline && !leader_ready_for_cleanup(boundary, &mut first_error) {
            if let Err(error) = discover_members(boundary) {
                remember_error(&mut first_error, error);
            }
            if let Err(error) = signal_known_members(boundary, SIGTERM) {
                remember_error(&mut first_error, error);
            }
            std::thread::sleep(POLL);
        }
        signal_authority(boundary, SIGKILL, &mut first_error);
    }
    if let Err(error) = cleanup_adopted_after(baseline, minimum_start) {
        remember_error(&mut first_error, error);
    }
    first_error.map_or(Ok(()), Err)
}

impl CoordinatorGuardian {
    fn read_frame(
        &mut self,
        parent: &ParentBoundary,
        frame: &mut [u8],
        observe_cancellation: bool,
        deadline: Option<Instant>,
    ) -> Result<GuardianFrame, String> {
        let result = read_guardian_frame(
            &mut self.child,
            &self.ready,
            parent,
            frame,
            observe_cancellation,
            deadline,
        );
        if let Ok(GuardianFrame::Exited(status)) = &result {
            self.reaped_status = Some(*status);
            self.guardian_reaped = true;
        }
        result
    }

    fn authorize(&self, token: u8, phase: &str) -> Result<(), String> {
        let start = self
            .start
            .as_ref()
            .ok_or_else(|| "cleanup-guardian authorization is already closed".to_string())?;
        write_guardian_token(start, token, phase)
    }

    fn pin_target(&mut self, key: ProcessKey) -> Result<(), String> {
        let boundary = boundary(key.pid, self.baseline.clone())?;
        if boundary.leader.key != key {
            return Err("cleanup guardian published a mismatched target identity".to_string());
        }
        self.target = Some(boundary);
        Ok(())
    }

    fn try_reap(&mut self, first_error: &mut Option<String>) -> bool {
        if self.guardian_reaped {
            return true;
        }
        #[cfg(performance_supervisor_test)]
        let injected_wait_failure =
            take_test_fault(TEST_FAIL_GUARDIAN_WAIT_ENV, &TEST_GUARDIAN_WAIT_FAULT_USED);
        #[cfg(not(performance_supervisor_test))]
        let injected_wait_failure = false;
        let wait_result = if injected_wait_failure {
            Err(std::io::Error::other(
                "injected cleanup-guardian wait failure",
            ))
        } else {
            self.child.try_wait()
        };
        match wait_result {
            Ok(Some(status)) => {
                self.reaped_status = Some(status);
                self.guardian_reaped = true;
                return true;
            }
            Ok(None) => {}
            Err(error) => remember_error(
                first_error,
                format!("inspect cleanup guardian during teardown: {error}"),
            ),
        }
        match pidfd_ready(&self.member.pidfd, 0) {
            Ok(false) => false,
            Err(error) => {
                remember_error(first_error, error);
                false
            }
            Ok(true) => {
                let mut raw_status = 0;
                // SAFETY: member.key names the exact direct child represented
                // by member.pidfd. WNOHANG keeps this recovery path bounded.
                let reaped =
                    unsafe { waitpid(self.member.key.pid as c_int, &mut raw_status, WNOHANG) };
                if reaped == self.member.key.pid as c_int {
                    self.reaped_status = Some(ExitStatus::from_raw(raw_status));
                    self.guardian_reaped = true;
                    true
                } else if reaped < 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(ECHILD)
                {
                    self.guardian_reaped = true;
                    true
                } else if reaped < 0 {
                    remember_error(
                        first_error,
                        format!(
                            "reap cleanup guardian by exact identity: {}",
                            std::io::Error::last_os_error()
                        ),
                    );
                    false
                } else {
                    false
                }
            }
        }
    }

    fn wait_until_reaped(&mut self, deadline: Instant, first_error: &mut Option<String>) -> bool {
        loop {
            if self.try_reap(first_error) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    fn stop(&mut self, requested_signal: c_int) -> Result<ExitStatus, String> {
        self.start.take();
        let mut first_error = None;
        if !self.guardian_reaped {
            #[cfg(performance_supervisor_test)]
            let injected_signal_failure = take_test_fault(
                TEST_FAIL_GUARDIAN_SIGNAL_ENV,
                &TEST_GUARDIAN_SIGNAL_FAULT_USED,
            );
            #[cfg(not(performance_supervisor_test))]
            let injected_signal_failure = false;
            let signal_result = if injected_signal_failure {
                Err("injected cleanup-guardian signal failure".to_string())
            } else {
                signal_pidfd(self.member.key, &self.member.pidfd, requested_signal)
            };
            if let Err(error) = signal_result {
                remember_error(&mut first_error, error);
            }
            if !self.wait_until_reaped(Instant::now() + KILL_GRACE, &mut first_error) {
                if let Err(error) = signal_pidfd(self.member.key, &self.member.pidfd, SIGKILL) {
                    remember_error(&mut first_error, error);
                }
                if !self.wait_until_reaped(Instant::now() + KILL_GRACE, &mut first_error) {
                    remember_error(
                        &mut first_error,
                        "cleanup guardian survived forced teardown".to_string(),
                    );
                }
            }
        }
        if !self.target_cleanup_done {
            if let Err(error) =
                cleanup_after_guardian_loss(&mut self.target, &self.baseline, self.member.key.start)
            {
                remember_error(&mut first_error, error);
            } else {
                self.target_cleanup_done = true;
            }
        }
        match (self.reaped_status, self.guardian_reaped, first_error) {
            (Some(status), true, None) => Ok(status),
            (_, _, Some(error)) => Err(error),
            _ => Err("cleanup guardian could not be proven reaped".to_string()),
        }
    }

    fn finish(&mut self) -> Result<ExitStatus, String> {
        self.start.take();
        // A valid DONE frame proves target and relay cleanup. Disarm fallback
        // ownership before the bounded guardian reap so an internal terminal
        // hang cannot retain the coordinator indefinitely.
        self.target_cleanup_done = true;
        let mut first_error = None;
        if !self.wait_until_reaped(Instant::now() + KILL_GRACE, &mut first_error) {
            if let Err(error) = signal_pidfd(self.member.key, &self.member.pidfd, SIGKILL) {
                remember_error(&mut first_error, error);
            }
            if !self.wait_until_reaped(Instant::now() + KILL_GRACE, &mut first_error) {
                remember_error(
                    &mut first_error,
                    "completed cleanup guardian survived forced teardown".to_string(),
                );
            } else {
                remember_error(
                    &mut first_error,
                    "cleanup guardian did not exit after completion".to_string(),
                );
            }
        }
        match (self.reaped_status, self.guardian_reaped, first_error) {
            (Some(status), true, None) => Ok(status),
            (_, _, Some(error)) => Err(error),
            _ => Err("cleanup guardian exited without an available status".to_string()),
        }
    }
}

impl Drop for CoordinatorGuardian {
    fn drop(&mut self) {
        if (!self.guardian_reaped || !self.target_cleanup_done) && self.stop(SIGTERM).is_err() {
            CLEANUP_INCOMPLETE.store(true, Ordering::Release);
            DIAGNOSTICS_SAFE.store(false, Ordering::Release);
        }
    }
}

fn setup_timeout(guardian: &mut CoordinatorGuardian, phase: &str) -> Result<c_int, String> {
    // A guardian that misses its setup deadline is exact-killed without a
    // graceful round: it holds no irreplaceable state, and any target it
    // spawned is still stopped behind the start barrier. Killing first keeps
    // teardown bounded even when the guardian ignores signals, and the
    // adopted-child sweep reaps the reparented target and relays by exact
    // identity. The timeout diagnostic itself is reported by the caller
    // through the bounded terminal path.
    let timeout = format!("cleanup guardian stalled before {phase}");
    match guardian.stop(SIGKILL) {
        Ok(_) => Err(timeout),
        Err(cleanup) => {
            CLEANUP_INCOMPLETE.store(true, Ordering::Release);
            Err(format!("{timeout}; cleanup failed: {cleanup}"))
        }
    }
}

fn coordinate_for_parent(
    expected_parent: u32,
    cwd: Option<PathBuf>,
    program: OsString,
    arguments: Vec<OsString>,
) -> Result<c_int, String> {
    CANCELLATION_SIGNAL.store(0, Ordering::Relaxed);
    CLEANUP_INCOMPLETE.store(false, Ordering::Release);
    DIAGNOSTICS_SAFE.store(true, Ordering::Release);
    #[cfg(performance_supervisor_test)]
    {
        TEST_GUARDIAN_WAIT_FAULT_USED.store(false, Ordering::SeqCst);
        TEST_GUARDIAN_SIGNAL_FAULT_USED.store(false, Ordering::SeqCst);
    }
    let stdio_mask = inherited_stdio_mask();
    let sigchld = SigchldGuard::normalize()?;
    let mut signal_mask = SignalMaskGuard::block()?;
    install_cancellation_handlers()?;
    enable_subreaper()?;
    let parent = bind_parent(expected_parent)?;
    let coordinator = std::process::id();
    let mut baseline_direct = match direct_children(coordinator)? {
        Some(children) => children,
        None => process_table()?
            .into_iter()
            .filter_map(|(pid, identity)| {
                (identity.parent == coordinator).then_some(ProcessKey {
                    pid,
                    start: identity.start,
                })
            })
            .collect(),
    };
    let (ready_read, ready_write) = internal_pipe(
        "cleanup-guardian ready read",
        "cleanup-guardian ready write",
    )?;
    let (start_read, start_write) = internal_pipe(
        "cleanup-guardian start read",
        "cleanup-guardian start write",
    )?;
    let ready_write_descriptor = ready_write.as_raw_fd();
    let start_read_descriptor = start_read.as_raw_fd();
    let child_ignores_sigchld = sigchld.child_ignored();
    let executable = env::current_exe()
        .map_err(|error| format!("resolve command supervisor executable: {error}"))?;
    let mut command = Command::new(executable);
    command.args([
        OsString::from("--cleanup-guardian"),
        OsString::from(ready_write_descriptor.to_string()),
        OsString::from(start_read_descriptor.to_string()),
        OsString::from("--parent-pid"),
        OsString::from(coordinator.to_string()),
    ]);
    if let Some(cwd) = cwd {
        command.arg("--cwd").arg(cwd);
    }
    command.arg("--").arg(program).args(arguments);
    // SAFETY: the closure uses only async-signal-safe descriptor and signal
    // operations. Cancellation signals are already blocked in this process,
    // so they remain pending rather than disappearing across the child exec.
    unsafe {
        command.pre_exec(move || {
            set_descriptor_cloexec(ready_write_descriptor, false)?;
            set_descriptor_cloexec(start_read_descriptor, false)?;
            close_absent_stdio(stdio_mask)?;
            if child_ignores_sigchld && signal(SIGCHLD, SIG_IGN) == SIGNAL_ERROR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn cleanup guardian: {error}"))?;
    drop(command);
    drop(ready_write);
    drop(start_read);
    // Publish the test-only guardian identity before binding: a guardian that
    // exits during arming may already be gone when binding runs, and the
    // post-mortem assertions still need its exact PID.
    #[cfg(performance_supervisor_test)]
    if let Some(path) = env::var_os(TEST_GUARDIAN_PID_FILE_ENV) {
        fs::write(path, format!("{}\n", child.id()))
            .map_err(|error| format!("publish cleanup-guardian identity: {error}"))?;
    }
    let guardian_identity = match process_identity(child.id()) {
        Ok(Some(identity)) if identity.live => identity,
        result => {
            drop(start_write);
            let cleanup = stop_unarmed_guardian(&mut child, SIGTERM);
            return Err(match (result, cleanup) {
                (Err(error), Ok(_)) => error,
                (Ok(_), Ok(_)) => "cleanup guardian vanished during identity binding".to_string(),
                (Err(error), Err(cleanup)) => format!("{error}; cleanup failed: {cleanup}"),
                (Ok(_), Err(cleanup)) => format!(
                    "cleanup guardian vanished during identity binding; cleanup failed: {cleanup}"
                ),
            });
        }
    };
    let guardian_member = match open_member(child.id(), &guardian_identity) {
        Ok(Some(member)) => member,
        result => {
            drop(start_write);
            let cleanup = stop_unarmed_guardian(&mut child, SIGTERM);
            return Err(match (result, cleanup) {
                (Err(error), Ok(_)) => error,
                (Ok(_), Ok(_)) => "cleanup guardian vanished before pidfd binding".to_string(),
                (Err(error), Err(cleanup)) => format!("{error}; cleanup failed: {cleanup}"),
                (Ok(_), Err(cleanup)) => format!(
                    "cleanup guardian vanished before pidfd binding; cleanup failed: {cleanup}"
                ),
            });
        }
    };
    let guardian_key = guardian_member.key;
    baseline_direct.insert(guardian_key);
    let mut guardian = CoordinatorGuardian {
        child,
        member: guardian_member,
        ready: ready_read,
        start: Some(start_write),
        baseline: baseline_direct,
        target: None,
        reaped_status: None,
        guardian_reaped: false,
        target_cleanup_done: false,
    };
    let mut armed = [0_u8; 1];
    let arming_deadline = Instant::now() + setup_grace();
    match guardian.read_frame(&parent, &mut armed, true, Some(arming_deadline))? {
        GuardianFrame::Bytes if armed[0] == GUARDIAN_ARMED_TOKEN => {}
        GuardianFrame::Bytes => {
            return Err("cleanup guardian published an invalid arming frame".to_string());
        }
        GuardianFrame::Cancelled(requested_signal) => {
            guardian.stop(requested_signal)?;
            signal_mask.restore()?;
            return Ok(128 + requested_signal);
        }
        GuardianFrame::Exited(status) => {
            return Err(format!(
                "cleanup guardian exited during arming with status {}",
                exit_status_code(status)
            ));
        }
        GuardianFrame::TimedOut => {
            return setup_timeout(&mut guardian, "arming");
        }
    }

    if let Err(error) = detach_coordinator_outputs() {
        return Err(error);
    }
    if let Some(requested_signal) = pending_cancellation()? {
        guardian.stop(requested_signal)?;
        signal_mask.restore()?;
        return Ok(128 + requested_signal);
    }
    stall_at_test_coordinator_phase("spawn-authorize");
    guardian.authorize(GUARDIAN_SPAWN_TOKEN, "target spawn")?;

    let mut target_frame = [0_u8; 13];
    let publication_deadline = Instant::now() + setup_grace();
    let target_key =
        match guardian.read_frame(&parent, &mut target_frame, true, Some(publication_deadline))? {
            GuardianFrame::Bytes if target_frame[0] == GUARDIAN_TARGET_TOKEN => ProcessKey {
                pid: u32::from_ne_bytes(target_frame[1..5].try_into().expect("target PID frame")),
                start: u64::from_ne_bytes(
                    target_frame[5..13].try_into().expect("target start frame"),
                ),
            },
            GuardianFrame::Bytes => {
                return Err("cleanup guardian published an invalid target frame".to_string());
            }
            GuardianFrame::Cancelled(requested_signal) => {
                guardian.stop(requested_signal)?;
                signal_mask.restore()?;
                return Ok(128 + requested_signal);
            }
            GuardianFrame::Exited(status) => {
                return Err(format!(
                    "cleanup guardian exited before target publication with status {}",
                    exit_status_code(status)
                ));
            }
            GuardianFrame::TimedOut => {
                return setup_timeout(&mut guardian, "target publication");
            }
        };
    #[cfg(performance_supervisor_test)]
    if let Some(path) = env::var_os(TEST_COORDINATOR_TARGET_PID_FILE_ENV) {
        fs::write(path, format!("{}\n", target_key.pid))
            .map_err(|error| format!("publish coordinator target identity: {error}"))?;
    }
    guardian.pin_target(target_key)?;
    fail_at_test_coordinator_phase("run-write")?;
    stall_at_test_coordinator_phase("run-authorize");
    guardian.authorize(GUARDIAN_RUN_TOKEN, "target execution")?;
    guardian.start.take();
    fail_at_test_coordinator_phase("post-run")?;
    signal_mask.restore()?;

    let mut completion = [0_u8; 2];
    let mut forwarded_signal = 0;
    let mut completion_deadline = None;
    fail_at_test_coordinator_phase("completion-read")?;
    loop {
        match guardian.read_frame(
            &parent,
            &mut completion,
            forwarded_signal == 0,
            completion_deadline,
        )? {
            GuardianFrame::Bytes if completion[0] == GUARDIAN_DONE_TOKEN && completion[1] <= 1 => {
                if completion[1] == 1 {
                    CLEANUP_INCOMPLETE.store(true, Ordering::Release);
                }
                let status = guardian.finish()?;
                return Ok(exit_status_code(status));
            }
            GuardianFrame::Bytes
                if completion[0] == GUARDIAN_CANCEL_TOKEN
                    && matches!(completion[1] as c_int, SIGHUP | SIGINT | SIGQUIT | SIGTERM) =>
            {
                if forwarded_signal == 0 {
                    forwarded_signal = completion[1] as c_int;
                }
                completion_deadline = Some(Instant::now() + GUARDIAN_CLEANUP_GRACE);
            }
            GuardianFrame::Bytes => {
                return Err("cleanup guardian published an invalid completion frame".to_string());
            }
            GuardianFrame::Cancelled(requested_signal) => {
                if signal_pidfd(
                    guardian.member.key,
                    &guardian.member.pidfd,
                    requested_signal,
                )
                .is_err()
                {
                    CLEANUP_INCOMPLETE.store(true, Ordering::Release);
                }
                forwarded_signal = requested_signal;
                completion_deadline = Some(Instant::now() + KILL_GRACE);
            }
            GuardianFrame::Exited(status) => {
                return Err(format!(
                    "cleanup guardian exited before completion with status {}",
                    exit_status_code(status)
                ));
            }
            GuardianFrame::TimedOut => {
                let requested_signal = if forwarded_signal == 0 {
                    SIGTERM
                } else {
                    forwarded_signal
                };
                guardian.stop(SIGKILL)?;
                return Ok(128 + requested_signal);
            }
        }
    }
}

#[cfg(test)]
fn supervise(program: OsString, arguments: Vec<OsString>) -> Result<ExitStatus, String> {
    supervise_for_parent(current_parent_pid()?, None, program, arguments, None)
}

fn output_relay(mut arguments: impl Iterator<Item = OsString>) -> ExitCode {
    let Some(source_descriptor) = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<c_int>().ok()))
        .filter(|value| *value >= 3)
    else {
        return ExitCode::from(70);
    };
    let Some(destination) = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<c_int>().ok()))
        .filter(|value| matches!(*value, 1 | 2))
    else {
        return ExitCode::from(70);
    };
    let Some(stream) = arguments.next().and_then(|value| value.into_string().ok()) else {
        return ExitCode::from(70);
    };
    let stream = match stream.as_str() {
        "stdout" if destination == 1 => "stdout",
        "stderr" if destination == 2 => "stderr",
        _ => return ExitCode::from(70),
    };
    if arguments.next().is_some() {
        return ExitCode::from(70);
    }
    if set_descriptor_cloexec(source_descriptor, true).is_err() {
        return ExitCode::from(70);
    }
    let relay_stdio_mask = if destination == 1 { 2 } else { 4 };
    if close_absent_stdio(relay_stdio_mask).is_err() {
        return ExitCode::from(70);
    }
    // SAFETY: this internal mode receives sole ownership of the inherited
    // capture descriptor from spawn_relay.
    let source = unsafe { OwnedFd::from_raw_fd(source_descriptor) };
    match relay_output(File::from(source), destination, stream) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

fn command_child(mut arguments: impl Iterator<Item = OsString>) -> ExitCode {
    let Some(read_descriptor) = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<c_int>().ok()))
        .filter(|value| *value >= 0)
    else {
        emit_bounded_line(2, "invalid internal command-supervisor invocation");
        return ExitCode::from(2);
    };
    let Some(write_descriptor) = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<c_int>().ok()))
        .filter(|value| *value >= 0 && *value != read_descriptor)
    else {
        emit_bounded_line(2, "invalid internal command-supervisor invocation");
        return ExitCode::from(2);
    };
    let Some(stdio_mask) = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u8>().ok()))
        .filter(|value| *value <= 7)
    else {
        emit_bounded_line(2, "invalid internal command-supervisor invocation");
        return ExitCode::from(2);
    };
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        emit_bounded_line(2, "invalid internal command-supervisor invocation");
        return ExitCode::from(2);
    }
    let Some(program) = arguments.next() else {
        emit_bounded_line(2, "invalid internal command-supervisor invocation");
        return ExitCode::from(2);
    };
    // SAFETY: both descriptors were inherited from the supervisor's pipe and
    // are exclusively owned by this process after fork and exec.
    let start_read = unsafe { OwnedFd::from_raw_fd(read_descriptor) };
    // SAFETY: see start_read; this end is closed before the child waits.
    let start_write = unsafe { OwnedFd::from_raw_fd(write_descriptor) };
    if set_descriptor_cloexec(start_read.as_raw_fd(), true).is_err()
        || set_descriptor_cloexec(start_write.as_raw_fd(), true).is_err()
    {
        emit_bounded_line(2, "cannot secure command-supervisor start barrier");
        return ExitCode::from(70);
    }
    drop(start_write);
    // SAFETY: the child signals only its own process identity. Its parent has
    // already returned from spawn and can now establish the pidfd boundary.
    if unsafe { kill(std::process::id() as c_int, SIGSTOP) } != 0 {
        emit_bounded_line(
            2,
            &format!(
                "cannot enter command-supervisor start barrier: {}",
                std::io::Error::last_os_error()
            ),
        );
        return ExitCode::from(70);
    }
    let mut token = 0_u8;
    loop {
        // SAFETY: token points to one writable byte and start_read is live.
        let result = unsafe { read(start_read.as_raw_fd(), &mut token, 1) };
        if result == 1 {
            break;
        }
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) {
            continue;
        }
        emit_bounded_line(2, "cannot read command-supervisor start barrier");
        return ExitCode::from(70);
    }
    drop(start_read);
    if token != START_TOKEN {
        return ExitCode::from(125);
    }
    if let Err(error) = unblock_cancellation_signals() {
        emit_bounded_line(
            2,
            &format!("cannot leave command-supervisor start barrier: {error}"),
        );
        return ExitCode::from(70);
    }
    let mut command = Command::new(program);
    command.args(arguments);
    // SAFETY: this closure only closes standard descriptors that were absent
    // when the supervisor executable began. All internal descriptors have
    // already been dropped or normalized above the standard range.
    unsafe {
        command.pre_exec(move || close_absent_stdio(stdio_mask));
    }
    let error = command.exec();
    emit_bounded_line(2, &format!("cannot execute supervised command: {error}"));
    ExitCode::from(126)
}

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    let Some(mut mode) = arguments.next() else {
        emit_bounded_line(2, USAGE);
        return ExitCode::from(2);
    };
    if mode == std::ffi::OsStr::new("--command-child") {
        return command_child(arguments);
    }
    if mode == std::ffi::OsStr::new("--output-relay") {
        return output_relay(arguments);
    }
    let guardian_handshake = if mode == std::ffi::OsStr::new("--cleanup-guardian") {
        let Some(ready_descriptor) = arguments
            .next()
            .and_then(|value| value.to_str().and_then(|value| value.parse::<c_int>().ok()))
            .filter(|value| *value >= 3)
        else {
            return ExitCode::from(70);
        };
        let Some(start_descriptor) = arguments
            .next()
            .and_then(|value| value.to_str().and_then(|value| value.parse::<c_int>().ok()))
            .filter(|value| *value >= 3 && *value != ready_descriptor)
        else {
            return ExitCode::from(70);
        };
        if set_descriptor_cloexec(ready_descriptor, true).is_err()
            || set_descriptor_cloexec(start_descriptor, true).is_err()
        {
            return ExitCode::from(70);
        }
        let Some(next) = arguments.next() else {
            return ExitCode::from(70);
        };
        mode = next;
        // SAFETY: both descriptors are unique pipe ends deliberately inherited
        // by the cleanup-guardian exec and owned by this process now.
        Some(GuardianHandshake {
            ready: unsafe { OwnedFd::from_raw_fd(ready_descriptor) },
            start: unsafe { OwnedFd::from_raw_fd(start_descriptor) },
        })
    } else {
        None
    };
    let mut expected_parent = None;
    let mut cwd = None;
    while mode != std::ffi::OsStr::new("--") {
        match mode.to_str() {
            Some("--parent-pid") => {
                let Some(value) = arguments.next() else {
                    emit_bounded_line(2, "missing command-supervisor parent PID");
                    return ExitCode::from(2);
                };
                expected_parent = value
                    .to_str()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|value| *value > 0);
                if expected_parent.is_none() {
                    emit_bounded_line(2, "invalid command-supervisor parent PID");
                    return ExitCode::from(2);
                }
            }
            Some("--cwd") => {
                let Some(value) = arguments.next() else {
                    emit_bounded_line(2, "missing command-supervisor working directory");
                    return ExitCode::from(2);
                };
                let value = PathBuf::from(value);
                if !value.is_absolute() {
                    emit_bounded_line(2, "command-supervisor working directory must be absolute");
                    return ExitCode::from(2);
                }
                cwd = Some(value);
            }
            _ => {
                emit_bounded_line(2, USAGE);
                return ExitCode::from(2);
            }
        }
        let Some(next) = arguments.next() else {
            emit_bounded_line(2, USAGE);
            return ExitCode::from(2);
        };
        mode = next;
    }
    let Some(program) = arguments.next() else {
        emit_bounded_line(2, USAGE);
        return ExitCode::from(2);
    };
    let expected_parent = match expected_parent {
        Some(parent) => parent,
        None => match current_parent_pid() {
            Ok(parent) => parent,
            Err(error) => {
                report_error(&error);
                return ExitCode::from(70);
            }
        },
    };
    let arguments = arguments.collect();
    let result = if let Some(handshake) = guardian_handshake {
        if let Err(error) = unblock_cancellation_signals() {
            report_error(&error);
            return ExitCode::from(70);
        }
        supervise_for_parent(expected_parent, cwd, program, arguments, Some(handshake))
            .map(exit_status_code)
    } else {
        coordinate_for_parent(expected_parent, cwd, program, arguments)
    };
    let base_status = match result {
        Ok(status) => status,
        Err(error) => {
            report_error(&error);
            70
        }
    };
    terminal_handoff(base_status)
}

#[cfg(test)]
mod tests {
    use super::*;

    static FAULT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ExactProcessGuard {
        member: Option<ProcessMember>,
    }

    impl ExactProcessGuard {
        fn acquire(pid: u32) -> Result<Self, String> {
            let member = match process_identity(pid)? {
                Some(identity) => open_member(pid, &identity)?,
                None => None,
            };
            Ok(Self { member })
        }

        fn is_live(&self) -> bool {
            self.member
                .as_ref()
                .is_some_and(|member| !pidfd_ready(&member.pidfd, 0).unwrap_or(true))
        }

        fn terminate(&mut self) -> Result<(), String> {
            let Some(member) = self.member.take() else {
                return Ok(());
            };
            signal_pidfd(member.key, &member.pidfd, SIGKILL)?;
            let deadline = Instant::now() + KILL_GRACE;
            while !pidfd_ready(&member.pidfd, 0)? {
                if Instant::now() >= deadline {
                    self.member = Some(member);
                    return Err("timed out terminating exact fixture process".to_string());
                }
                std::thread::sleep(POLL);
            }
            drop(member);
            let _ = reap_adopted_children(Instant::now() + KILL_GRACE);
            Ok(())
        }
    }

    impl Drop for ExactProcessGuard {
        fn drop(&mut self) {
            let _ = self.terminate();
        }
    }

    fn pid_is_live(pid: u32) -> bool {
        process_identity(pid)
            .ok()
            .flatten()
            .is_some_and(|identity| identity.live)
    }

    fn wait_for_pid_file(path: &std::path::Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(value) = fs::read_to_string(path) {
                if let Ok(pid) = value.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(Instant::now() < deadline, "process marker was not written");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn direct_child_authority_is_pinned_before_a_failing_broad_scan() {
        let _serial = FAULT_TEST_LOCK.lock().expect("lock scan-fault tests");
        let directory = env::temp_dir().join(format!(
            "dot-performance-supervisor-direct-first-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let pid_path = directory.join("escaped-pid");
        let release_path = directory.join("release");
        let lock_path = directory.join("inherited-lock");
        FAIL_ALL_PROCESS_SCANS.store(false, Ordering::Relaxed);
        FORCED_DIRECT_CHILD_PID.store(0, Ordering::Relaxed);
        let thread_pid_path = pid_path.clone();
        let thread_release_path = release_path.clone();
        let thread_lock_path = lock_path.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = supervise(
                OsString::from("/bin/bash"),
                vec![
                    OsString::from("--noprofile"),
                    OsString::from("--norc"),
                    OsString::from("-c"),
                    OsString::from(
                        "exec 9>\"$3\"; /usr/bin/flock --exclusive --nonblock 9 || exit 91; \
                         /usr/bin/setsid /bin/bash --noprofile --norc -c \
                         'trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; \
                         kill -STOP $$; while :; do :; done' direct-child \"$1\" & \
                         while [[ ! -e $2 ]]; do :; done",
                    ),
                    OsString::from("direct-fixture"),
                    thread_pid_path.into_os_string(),
                    thread_release_path.into_os_string(),
                    thread_lock_path.into_os_string(),
                ],
            );
            let _ = sender.send(result);
        });
        let escaped = wait_for_pid_file(&pid_path);
        let mut escaped_guard =
            ExactProcessGuard::acquire(escaped).expect("bind exact escaped fixture identity");
        FORCED_DIRECT_CHILD_PID.store(escaped as usize, Ordering::Relaxed);
        FAIL_ALL_PROCESS_SCANS.store(true, Ordering::Relaxed);
        fs::write(&release_path, b"release\n").expect("release fixture leader");
        let result = receiver.recv_timeout(Duration::from_secs(6));
        FAIL_ALL_PROCESS_SCANS.store(false, Ordering::Relaxed);
        FORCED_DIRECT_CHILD_PID.store(0, Ordering::Relaxed);
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                let cleanup = escaped_guard.terminate();
                let _ = worker.join();
                panic!("direct-first cleanup did not return: {error}; cleanup={cleanup:?}");
            }
        };
        worker.join().expect("join supervisor worker");
        let error = result.expect_err("broad process-scan failure must reject");
        assert!(error.contains("process-table failure"), "{error}");
        assert!(
            !escaped_guard.is_live(),
            "directly pinned escaped child survived broad-scan failure"
        );
        assert!(
            Command::new("/usr/bin/flock")
                .args(["--exclusive", "--nonblock"])
                .arg(&lock_path)
                .arg("/bin/true")
                .status()
                .expect("probe released fixture lock")
                .success(),
            "directly pinned escaped child retained its inherited lock"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn command_churn_does_not_trigger_process_table_polling() {
        FAIL_SCANS_AFTER_DISCOVERY.store(false, Ordering::Relaxed);
        DISCOVERED_DESCENDANT.store(false, Ordering::Relaxed);
        PROCESS_TABLE_SCANS.store(0, Ordering::Relaxed);
        let status = supervise(
            OsString::from("/bin/bash"),
            vec![
                OsString::from("--noprofile"),
                OsString::from("--norc"),
                OsString::from("-c"),
                OsString::from("for _ in {1..80}; do /bin/true; /bin/sleep 0.005; done"),
            ],
        )
        .expect("high-churn command must remain supervisable");
        assert!(status.success());
        let scans = PROCESS_TABLE_SCANS.load(Ordering::Relaxed);
        assert!(
            scans <= 6,
            "process table was scanned {scans} times while the leader was running"
        );
    }

    #[test]
    fn retained_pidfd_cleans_escaped_child_after_process_scan_failure() {
        let _serial = FAULT_TEST_LOCK.lock().expect("lock scan-fault tests");
        let directory = env::temp_dir().join(format!(
            "dot-performance-supervisor-scan-failure-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let pid_path = directory.join("escaped-pid");
        DISCOVERED_DESCENDANT.store(false, Ordering::Relaxed);
        FAIL_SCANS_AFTER_DISCOVERY.store(true, Ordering::Relaxed);
        let result = supervise(
            OsString::from("/bin/bash"),
            vec![
                OsString::from("--noprofile"),
                OsString::from("--norc"),
                OsString::from("-c"),
                OsString::from(
                    "/usr/bin/setsid /bin/bash --noprofile --norc -c \
                     'trap \"\" TERM; exec </dev/null >/dev/null 2>&1; \
                     printf \"%s\\n\" \"$$\" >\"$1\"; \
                     while :; do /bin/sleep 1; done' scan-child \"$1\" & \
                     while [[ ! -s \"$1\" ]]; do /bin/sleep 0.01; done",
                ),
                OsString::from("scan-fixture"),
                pid_path.as_os_str().to_owned(),
            ],
        );
        FAIL_SCANS_AFTER_DISCOVERY.store(false, Ordering::Relaxed);
        let escaped = wait_for_pid_file(&pid_path);
        let escaped_guard =
            ExactProcessGuard::acquire(escaped).expect("bind exact escaped fixture identity");
        let survived = escaped_guard.is_live();
        let _ = fs::remove_dir_all(&directory);
        assert!(result.is_err(), "process scan failure was accepted");
        assert!(!survived, "known escaped child survived scan failure");
    }

    #[test]
    fn retained_pidfd_survives_an_omitted_then_failed_process_snapshot() {
        let _serial = FAULT_TEST_LOCK.lock().expect("lock scan-fault tests");
        let directory = env::temp_dir().join(format!(
            "dot-performance-supervisor-omitted-scan-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let pid_path = directory.join("escaped-pid");
        let lock_path = directory.join("inherited-lock");
        let before = fs::read_dir("/proc/self/fd")
            .expect("read initial descriptor inventory")
            .count();
        DISCOVERED_DESCENDANT.store(false, Ordering::Relaxed);
        OMITTED_UNRETAINED.store(false, Ordering::Relaxed);
        OMITTED_RETAINED.store(false, Ordering::Relaxed);
        OMIT_RETAINED_ONCE.store(true, Ordering::Relaxed);
        FAIL_SCANS_AFTER_OMISSION.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let result = supervise(
            OsString::from("/bin/bash"),
            vec![
                OsString::from("--noprofile"),
                OsString::from("--norc"),
                OsString::from("-c"),
                OsString::from(
                    "exec 9>\"$2\"; /usr/bin/flock --exclusive --nonblock 9 || exit 91; \
                     /usr/bin/setsid /bin/bash --noprofile --norc -c \
                     'trap \"\" TERM; \
                     printf \"%s\\n\" \"$$\" >\"$1\"; \
                     kill -STOP $$; while :; do :; done' scan-child \"$1\" & \
                     while [[ ! -s \"$1\" ]]; do /bin/sleep 0.01; done",
                ),
                OsString::from("scan-fixture"),
                pid_path.as_os_str().to_owned(),
                lock_path.as_os_str().to_owned(),
            ],
        );
        OMIT_RETAINED_ONCE.store(false, Ordering::Relaxed);
        FAIL_SCANS_AFTER_OMISSION.store(false, Ordering::Relaxed);
        let escaped = wait_for_pid_file(&pid_path);
        let escaped_guard =
            ExactProcessGuard::acquire(escaped).expect("bind exact escaped fixture identity");
        let survived = escaped_guard.is_live();
        let lock_available = Command::new("/usr/bin/flock")
            .args(["--exclusive", "--nonblock"])
            .arg(&lock_path)
            .arg("/bin/true")
            .status()
            .expect("probe inherited fixture lock")
            .success();
        let after = fs::read_dir("/proc/self/fd")
            .expect("read final descriptor inventory")
            .count();
        let _ = fs::remove_dir_all(&directory);
        assert!(
            OMITTED_RETAINED.load(Ordering::Relaxed),
            "test did not omit the retained child from a later snapshot"
        );
        assert!(result.is_err(), "omitted then failed scan was accepted");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "omitted then failed scan cleanup was not bounded"
        );
        assert!(
            !survived,
            "retained escaped child survived scan failure: {result:?}; omitted={} discovered={}",
            OMITTED_RETAINED.load(Ordering::Relaxed),
            DISCOVERED_DESCENDANT.load(Ordering::Relaxed)
        );
        assert!(lock_available, "escaped child retained its inherited lock");
        assert!(
            after <= before + 1,
            "process identity handles leaked after cleanup: {before} -> {after}"
        );
    }

    #[test]
    fn omission_before_first_pidfd_fails_bounded_and_retains_lock_authority() {
        let _serial = FAULT_TEST_LOCK.lock().expect("lock scan-fault tests");
        let directory = env::temp_dir().join(format!(
            "dot-performance-supervisor-unobserved-scan-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create test directory");
        let pid_path = directory.join("escaped-pid");
        let lock_path = directory.join("inherited-lock");
        DISCOVERED_DESCENDANT.store(false, Ordering::Relaxed);
        OMITTED_RETAINED.store(false, Ordering::Relaxed);
        OMITTED_UNRETAINED.store(false, Ordering::Relaxed);
        OMITTED_PID.store(0, Ordering::Relaxed);
        OMITTED_WAS_RETAINED.store(true, Ordering::Relaxed);
        OMIT_UNRETAINED_ONCE.store(true, Ordering::Relaxed);
        HIDE_DIRECT_AFTER_OMISSION.store(true, Ordering::Relaxed);
        FAIL_SCANS_AFTER_OMISSION.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let thread_pid_path = pid_path.clone();
        let thread_lock_path = lock_path.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = supervise(
                OsString::from("/bin/bash"),
                vec![
                    OsString::from("--noprofile"),
                    OsString::from("--norc"),
                    OsString::from("-c"),
                    OsString::from(
                        "exec 9>\"$2\"; /usr/bin/flock --exclusive --nonblock 9 || exit 91; \
                         /usr/bin/setsid /bin/bash --noprofile --norc -c \
                         'trap \"\" TERM; \
                         printf \"%s\\n\" \"$$\" >\"$1\"; \
                         kill -STOP $$; while :; do :; done' scan-child \"$1\" & \
                         while [[ ! -s \"$1\" ]]; do /bin/sleep 0.01; done",
                    ),
                    OsString::from("scan-fixture"),
                    thread_pid_path.into_os_string(),
                    thread_lock_path.into_os_string(),
                ],
            );
            let _ = sender.send(result);
        });
        let escaped = wait_for_pid_file(&pid_path);
        let mut escaped_guard =
            ExactProcessGuard::acquire(escaped).expect("bind exact escaped fixture identity");
        let result = match receiver.recv_timeout(Duration::from_secs(6)) {
            Ok(result) => result,
            Err(error) => {
                let cleanup = escaped_guard.terminate();
                let _ = worker.join();
                panic!(
                    "total observation loss exceeded its watchdog: {error}; cleanup={cleanup:?}"
                );
            }
        };
        worker.join().expect("join observation-loss supervisor");
        OMIT_UNRETAINED_ONCE.store(false, Ordering::Relaxed);
        HIDE_DIRECT_AFTER_OMISSION.store(false, Ordering::Relaxed);
        FAIL_SCANS_AFTER_OMISSION.store(false, Ordering::Relaxed);
        assert!(
            OMITTED_UNRETAINED.load(Ordering::Relaxed),
            "test did not omit the new child before its first pidfd"
        );
        assert_eq!(
            OMITTED_PID.load(Ordering::Relaxed),
            escaped as usize,
            "the omitted identity was not the escaped fixture"
        );
        assert!(
            !OMITTED_WAS_RETAINED.load(Ordering::Relaxed),
            "escaped fixture already had a pidfd when it was omitted"
        );
        let error = result.expect_err("total observation loss was accepted");
        assert!(error.contains("process-table failure"), "{error}");
        assert!(
            error.contains("direct child process inventory is unavailable"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "total observation loss did not return within its cleanup bound"
        );
        assert!(
            !Command::new("/usr/bin/flock")
                .args(["--exclusive", "--nonblock"])
                .arg(&lock_path)
                .arg("/bin/true")
                .status()
                .expect("probe retained fixture lock")
                .success(),
            "unobserved escaped fixture lost lifecycle-lock authority"
        );
        escaped_guard
            .terminate()
            .expect("kill exact escaped fixture identity");
        assert!(!pid_is_live(escaped), "escaped fixture did not terminate");
        let lock_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let available = Command::new("/usr/bin/flock")
                .args(["--exclusive", "--nonblock"])
                .arg(&lock_path)
                .arg("/bin/true")
                .status()
                .expect("probe released fixture lock")
                .success();
            if available {
                break;
            }
            assert!(
                Instant::now() < lock_deadline,
                "escaped fixture retained its lock after exact teardown"
            );
            std::thread::sleep(POLL);
        }
        let _ = fs::remove_dir_all(&directory);
        OMITTED_UNRETAINED.store(false, Ordering::Relaxed);
        OMITTED_PID.store(0, Ordering::Relaxed);
        OMITTED_WAS_RETAINED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn process_group_requires_the_unreaped_leader_identity() {
        let key = ProcessKey { pid: 42, start: 7 };
        let matching_zombie = ProcessIdentity {
            parent: 1,
            session: 42,
            start: 7,
            live: false,
        };
        let reused = ProcessIdentity {
            start: 8,
            ..matching_zombie.clone()
        };
        let reassigned_group = ProcessIdentity {
            session: 43,
            ..matching_zombie.clone()
        };
        assert!(group_identity_is_pinned(false, key, Some(&matching_zombie)));
        assert!(!group_identity_is_pinned(true, key, Some(&matching_zombie)));
        assert!(!group_identity_is_pinned(false, key, Some(&reused)));
        assert!(!group_identity_is_pinned(
            false,
            key,
            Some(&reassigned_group)
        ));
        assert!(!group_identity_is_pinned(false, key, None));
    }

    #[test]
    fn terminal_proc_states_are_not_live() {
        for state in ["Z", "X", "x"] {
            let fields = [
                state, "1", "2", "42", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0",
                "0", "1", "0", "777",
            ];
            let stat = format!("9 (fixture) {}\n", fields.join(" "));
            let identity = parse_process_identity(9, stat.as_bytes()).expect("valid proc record");
            assert!(!identity.live, "state {state} must be terminal");
        }
    }

    #[test]
    fn internal_pipes_are_above_stdio_and_close_on_exec() {
        let (read, write) =
            internal_pipe("fixture read", "fixture write").expect("create normalized fixture pipe");
        for descriptor in [&read, &write] {
            assert!(descriptor.as_raw_fd() >= 3);
            // SAFETY: descriptor is live and F_GETFD has no side effects.
            let flags = unsafe { fcntl(descriptor.as_raw_fd(), F_GETFD) };
            assert!(flags >= 0, "inspect normalized fixture descriptor");
            assert_ne!(flags & FD_CLOEXEC, 0, "internal descriptor must be CLOEXEC");
        }
    }

    #[test]
    fn start_barrier_rolls_back_partial_descriptor_normalization() {
        let _serial = FAULT_TEST_LOCK.lock().expect("lock descriptor-fault tests");
        START_BARRIER_STATE.store(START_BARRIER_IDLE, Ordering::SeqCst);
        START_BARRIER_WRITE_FD.store(-1, Ordering::SeqCst);
        let before = fs::read_dir("/proc/self/fd")
            .expect("read initial descriptor inventory")
            .count();
        FAIL_INTERNAL_FD_NORMALIZATION_AFTER.with(|remaining| remaining.set(1));
        let error = match StartBarrier::new() {
            Ok(_) => panic!("injected descriptor normalization unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.contains("injected command start-barrier write"));
        assert_eq!(
            START_BARRIER_STATE.load(Ordering::SeqCst),
            START_BARRIER_IDLE
        );
        assert_eq!(START_BARRIER_WRITE_FD.load(Ordering::SeqCst), -1);
        let after = fs::read_dir("/proc/self/fd")
            .expect("read final descriptor inventory")
            .count();
        assert_eq!(before, after, "partial barrier setup leaked a descriptor");
        drop(StartBarrier::new().expect("create barrier after rollback"));
        assert_eq!(
            START_BARRIER_STATE.load(Ordering::SeqCst),
            START_BARRIER_IDLE
        );
    }

    #[test]
    fn open_file_identity_distinguishes_aliases() {
        let (_read, write) =
            internal_pipe("fixture read", "fixture write").expect("create aliased fixture pipe");
        let alias = duplicate_internal_fd(&write, "fixture alias").expect("duplicate fixture pipe");
        let aliased = same_open_file(write.as_raw_fd(), alias.as_raw_fd())
            .expect("compare aliased descriptors");
        assert!(aliased);
        let (_other_read, other_write) =
            internal_pipe("other read", "other write").expect("create distinct fixture pipe");
        let distinct = same_open_file(write.as_raw_fd(), other_write.as_raw_fd())
            .expect("compare distinct descriptors");
        assert!(!distinct);
    }

    #[test]
    fn procfs_identity_comparison_distinguishes_pipe_aliases() {
        // Containers without CAP_SYS_PTRACE deny kcmp; the procfs fallback
        // must still tell a duplicated pipe from a distinct one.
        let (_read, write) =
            internal_pipe("fixture read", "fixture write").expect("create aliased fixture pipe");
        let alias = duplicate_internal_fd(&write, "fixture alias").expect("duplicate fixture pipe");
        assert!(
            same_open_file_by_procfs(write.as_raw_fd(), alias.as_raw_fd())
                .expect("compare aliased descriptors without kcmp")
        );
        let (_other_read, other_write) =
            internal_pipe("other read", "other write").expect("create distinct fixture pipe");
        assert!(
            !same_open_file_by_procfs(write.as_raw_fd(), other_write.as_raw_fd())
                .expect("compare distinct descriptors without kcmp")
        );
    }

    #[test]
    fn repeated_commands_do_not_retain_process_identity_handles() {
        FAIL_SCANS_AFTER_DISCOVERY.store(false, Ordering::Relaxed);
        DISCOVERED_DESCENDANT.store(false, Ordering::Relaxed);
        let before = fs::read_dir("/proc/self/fd")
            .expect("read initial descriptor inventory")
            .count();
        for _ in 0..16 {
            let status = supervise(OsString::from("/bin/true"), Vec::new())
                .expect("short command must remain supervisable");
            assert!(status.success());
        }
        let after = fs::read_dir("/proc/self/fd")
            .expect("read final descriptor inventory")
            .count();
        assert!(
            after <= before + 1,
            "process identity handles grew across commands: {before} -> {after}"
        );
    }
}
