//! Process-state probes for the standalone performance command supervisor.

use std::env;
use std::ffi::{c_int, c_ulong, OsString};
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicI32, Ordering};

const EBADF: c_int = 9;
const FD_CLOEXEC: c_int = 1;
const F_GETFD: c_int = 1;
const SA_NOCLDWAIT: c_int = 2;
const SIGCHLD: c_int = 17;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

static INHERITED_STDIN_FLAGS: AtomicI32 = AtomicI32::new(-1);
static INHERITED_STDOUT_FLAGS: AtomicI32 = AtomicI32::new(-1);
static INHERITED_STDERR_FLAGS: AtomicI32 = AtomicI32::new(-1);

#[repr(C)]
#[derive(Clone, Copy)]
struct SignalSet {
    words: [c_ulong; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SignalAction {
    handler: usize,
    mask: SignalSet,
    flags: c_int,
    restorer: usize,
}

unsafe extern "C" {
    fn close(descriptor: c_int) -> c_int;
    fn fcntl(descriptor: c_int, command: c_int, ...) -> c_int;
    #[link_name = "write"]
    fn libc_write(descriptor: c_int, buffer: *const u8, count: usize) -> isize;
    fn sigaction(signal: c_int, action: *const SignalAction, previous: *mut SignalAction) -> c_int;
    fn sigemptyset(set: *mut SignalSet) -> c_int;
}

unsafe extern "C" fn capture_inherited_stdio() {
    // This constructor runs before Rust's runtime can make absent standard
    // descriptors safe for library code. It records the descriptor state at
    // the executable boundary that the supervisor must preserve.
    INHERITED_STDIN_FLAGS.store(unsafe { fcntl(0, F_GETFD) }, Ordering::Relaxed);
    INHERITED_STDOUT_FLAGS.store(unsafe { fcntl(1, F_GETFD) }, Ordering::Relaxed);
    INHERITED_STDERR_FLAGS.store(unsafe { fcntl(2, F_GETFD) }, Ordering::Relaxed);
}

#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
static CAPTURE_INHERITED_STDIO: unsafe extern "C" fn() = capture_inherited_stdio;

extern "C" fn caught_sigchld(_: c_int) {}

fn action(handler: usize, flags: c_int) -> Result<SignalAction, String> {
    let mut action = SignalAction {
        handler,
        mask: SignalSet { words: [0; 16] },
        flags,
        restorer: 0,
    };
    // SAFETY: action.mask points to writable storage with the Linux sigset_t
    // layout used by the supervisor under test.
    if unsafe { sigemptyset(&mut action.mask) } != 0 {
        return Err(format!(
            "initialize SIGCHLD action: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(action)
}

fn set_sigchld(mode: &str) -> Result<(), String> {
    let action = match mode {
        "ignore" => action(SIG_IGN, 0)?,
        "default-no-cld-wait" => action(SIG_DFL, SA_NOCLDWAIT)?,
        "caught-no-cld-wait" => action(caught_sigchld as *const () as usize, SA_NOCLDWAIT)?,
        _ => return Err(format!("unknown SIGCHLD fixture mode: {mode}")),
    };
    // SAFETY: action has the Linux sigaction layout and remains live for the
    // duration of this call.
    if unsafe { sigaction(SIGCHLD, &action, std::ptr::null_mut()) } != 0 {
        return Err(format!(
            "install SIGCHLD fixture action: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn query_sigchld() -> Result<SignalAction, String> {
    let mut action = action(SIG_DFL, 0)?;
    // SAFETY: action points to writable Linux sigaction storage.
    if unsafe { sigaction(SIGCHLD, std::ptr::null(), &mut action) } != 0 {
        return Err(format!(
            "inspect SIGCHLD fixture action: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(action)
}

fn exec_command(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err("missing fixture command delimiter".to_string());
    }
    let program = arguments
        .next()
        .ok_or_else(|| "missing fixture command".to_string())?;
    let error = Command::new(program).args(arguments).exec();
    Err(format!("execute fixture command: {error}"))
}

fn launch_sigchld(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    let mode = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| "missing SIGCHLD fixture mode".to_string())?;
    set_sigchld(&mode)?;
    exec_command(arguments)
}

fn probe_sigchld(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    let report = arguments
        .next()
        .ok_or_else(|| "missing SIGCHLD report path".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected SIGCHLD probe argument".to_string());
    }
    let action = query_sigchld()?;
    let handler = match action.handler {
        SIG_DFL => "default",
        SIG_IGN => "ignore",
        _ => "caught",
    };
    fs::write(
        report,
        format!(
            "handler={handler}\nno_cld_wait={}\n",
            action.flags & SA_NOCLDWAIT != 0
        ),
    )
    .map_err(|error| format!("write SIGCHLD report: {error}"))?;
    Ok(ExitCode::from(42))
}

fn launch_closed_stdio(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    let mask = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u8>().ok()))
        .filter(|value| (1..=7).contains(value))
        .ok_or_else(|| "invalid closed-stdio fixture mask".to_string())?;
    for descriptor in 0..=2 {
        if mask & (1 << descriptor) != 0 {
            // SAFETY: closing an inherited standard descriptor affects only
            // this fixture process immediately before exec.
            let result = unsafe { close(descriptor) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(EBADF) {
                return Err(format!(
                    "close fixture descriptor {descriptor}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    exec_command(arguments)
}

fn probe_stdio(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    let report = arguments
        .next()
        .ok_or_else(|| "missing stdio report path".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected stdio probe argument".to_string());
    }
    let mut rows = Vec::new();
    for (descriptor, flags) in [
        INHERITED_STDIN_FLAGS.load(Ordering::Relaxed),
        INHERITED_STDOUT_FLAGS.load(Ordering::Relaxed),
        INHERITED_STDERR_FLAGS.load(Ordering::Relaxed),
    ]
    .into_iter()
    .enumerate()
    {
        if flags >= 0 {
            rows.push(format!(
                "{descriptor}=open,cloexec={}",
                flags & FD_CLOEXEC != 0
            ));
        } else {
            rows.push(format!("{descriptor}=closed"));
        }
    }
    fs::write(report, format!("{}\n", rows.join("\n")))
        .map_err(|error| format!("write stdio report: {error}"))?;
    Ok(ExitCode::SUCCESS)
}

fn flood_stdout(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    let marker = arguments
        .next()
        .ok_or_else(|| "missing blocked-output marker path".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected blocked-output fixture argument".to_string());
    }
    fs::write(marker, format!("{}\n", std::process::id()))
        .map_err(|error| format!("write blocked-output marker: {error}"))?;
    let bytes = [b'x'; 8192];
    loop {
        std::io::Write::write_all(&mut std::io::stdout(), &bytes)
            .map_err(|error| format!("write blocked fixture output: {error}"))?;
    }
}

fn flood_stderr(mut arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    let marker = arguments
        .next()
        .ok_or_else(|| "missing blocked-output marker path".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected blocked-output fixture argument".to_string());
    }
    fs::write(marker, format!("{}\n", std::process::id()))
        .map_err(|error| format!("write blocked-output marker: {error}"))?;
    let bytes = [b'x'; 8192];
    loop {
        std::io::Write::write_all(&mut std::io::stderr(), &bytes)
            .map_err(|error| format!("write blocked fixture output: {error}"))?;
    }
}

fn write_all(descriptor: c_int, bytes: &[u8]) -> Result<(), String> {
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: bytes contains initialized storage for the requested range.
        let result =
            unsafe { libc_write(descriptor, bytes[written..].as_ptr(), bytes.len() - written) };
        if result > 0 {
            written += result as usize;
            continue;
        }
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!(
            "write alternating fixture stream: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn alternate_output(arguments: impl Iterator<Item = OsString>) -> Result<ExitCode, String> {
    if arguments.count() != 0 {
        return Err("unexpected alternating-output fixture argument".to_string());
    }
    for index in 0..1024 {
        write_all(1, format!("stdout-{index:04}\n").as_bytes())?;
        write_all(2, format!("stderr-{index:04}\n").as_bytes())?;
    }
    Ok(ExitCode::from(23))
}

fn run() -> Result<ExitCode, String> {
    let mut arguments = env::args_os().skip(1);
    match arguments.next().as_deref() {
        Some(mode) if mode == std::ffi::OsStr::new("launch-sigchld") => launch_sigchld(arguments),
        Some(mode) if mode == std::ffi::OsStr::new("probe-sigchld") => probe_sigchld(arguments),
        Some(mode) if mode == std::ffi::OsStr::new("launch-closed-stdio") => {
            launch_closed_stdio(arguments)
        }
        Some(mode) if mode == std::ffi::OsStr::new("probe-stdio") => probe_stdio(arguments),
        Some(mode) if mode == std::ffi::OsStr::new("flood-stdout") => flood_stdout(arguments),
        Some(mode) if mode == std::ffi::OsStr::new("flood-stderr") => flood_stderr(arguments),
        Some(mode) if mode == std::ffi::OsStr::new("alternate-output") => {
            alternate_output(arguments)
        }
        _ => Err("invalid performance-supervisor fixture invocation".to_string()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("fixture error: {error}");
            ExitCode::from(70)
        }
    }
}
