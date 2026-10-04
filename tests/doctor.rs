//! End-to-end parity for the native `dot doctor` coordinator.
//!
//! Rust-side invocations select the retained hook boundary explicitly through
//! `DOT_BASH`; the native engine itself remains independent of Bash.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use dot_test_support::TempDir;

fn command(shell: bool, home: &TempDir, state: &TempDir, extra: &[(&str, &str)]) -> Command {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = if shell {
        let mut command = Command::new(dot_test_support::bash());
        command.arg(root.join("bin/dot"));
        command
    } else {
        Command::new(env!("CARGO_BIN_EXE_dot"))
    };
    command
        .arg("doctor")
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "TMPDIR",
            std::env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into()),
        )
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", state.path())
        .env("DOT_SOURCE_ROOT", root)
        .env("BASH", dot_test_support::bash())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !shell {
        command.env("DOT_BASH", dot_test_support::bash());
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    command
}

fn pair(home: &TempDir, state: &TempDir) -> (Output, Output) {
    pair_with(home, state, &[])
}

fn pair_with(home: &TempDir, state: &TempDir, extra: &[(&str, &str)]) -> (Output, Output) {
    let shell = command(true, home, state, extra)
        .output()
        .expect("shell doctor");
    let native = command(false, home, state, extra)
        .output()
        .expect("native doctor");
    (shell, native)
}

fn run_in_process(
    home: &TempDir,
    state: &TempDir,
    cwd: &Path,
    extra: &[(&str, &OsStr)],
) -> (i32, Vec<u8>, Vec<u8>) {
    run_in_process_with_terminal(home, state, cwd, extra, false)
}

fn run_in_process_with_terminal(
    home: &TempDir,
    state: &TempDir,
    cwd: &Path,
    extra: &[(&str, &OsStr)],
    stdout_terminal: bool,
) -> (i32, Vec<u8>, Vec<u8>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut env = BTreeMap::<OsString, OsString>::from([
        ("HOME".into(), home.path().as_os_str().to_os_string()),
        (
            "XDG_STATE_HOME".into(),
            state.path().as_os_str().to_os_string(),
        ),
        ("DOT_SOURCE_ROOT".into(), root.as_os_str().to_os_string()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("BASH".into(), dot_test_support::bash().into()),
        ("LC_ALL".into(), "C".into()),
    ]);
    for (key, value) in extra {
        env.insert((*key).into(), (*value).to_os_string());
    }
    let runtime = dot::app::Runtime::from_env(&env, cwd).expect("runtime");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = {
        let mut streams =
            dot::app::Streams::with_terminal(&mut stdout, &mut stderr, stdout_terminal);
        dot::doctor::run(&runtime, &mut streams)
    };
    (code, stdout, stderr)
}

#[test]
fn in_process_terminal_doctor_colors_result_records() {
    let home = TempDir::new("doctor-native-terminal-home").expect("home");
    let state = TempDir::new("doctor-native-terminal-state").expect("state");
    let (_, stdout, _) = run_in_process_with_terminal(&home, &state, home.path(), &[], true);
    assert!(
        stdout.windows(5).any(|part| part == b"\x1b[32m"),
        "terminal doctor did not color pass records: {}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn in_process_piped_doctor_keeps_result_records_plain() {
    let home = TempDir::new("doctor-native-pipe-home").expect("home");
    let state = TempDir::new("doctor-native-pipe-state").expect("state");
    let (_, stdout, _) = run_in_process(&home, &state, home.path(), &[]);
    assert!(
        !stdout.contains(&b'\x1b'),
        "piped doctor emitted ANSI: {stdout:?}"
    );
}

#[test]
fn in_process_no_color_disables_terminal_result_colors() {
    let home = TempDir::new("doctor-native-no-color-home").expect("home");
    let state = TempDir::new("doctor-native-no-color-state").expect("state");
    let (_, stdout, _) = run_in_process_with_terminal(
        &home,
        &state,
        home.path(),
        &[("NO_COLOR", OsStr::new("1"))],
        true,
    );
    assert!(
        !stdout.contains(&b'\x1b'),
        "NO_COLOR doctor emitted ANSI: {stdout:?}"
    );
}

/// Blank stamp-age renderings (`last success 497206h5m ago`, `ran by init
/// 4s ago`): shell and native run sequentially, so a second, minute, or hour
/// boundary between the two renders a different age for the same stamp.
/// Only the age span before each ` ago` is blanked, every other byte still
/// compares exactly, and the cron test below pins the age value itself with
/// a ±1-minute tolerance instead of exact bytes.
fn normalize_stamp_age(bytes: &[u8]) -> Vec<u8> {
    const SUFFIX: &[u8] = b" ago";
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(at) = rest
        .windows(SUFFIX.len())
        .position(|window| window == SUFFIX)
    {
        let head = &rest[..at];
        let age_len = head
            .iter()
            .rev()
            .take_while(|byte| byte.is_ascii_digit() || matches!(byte, b'h' | b'm' | b's'))
            .count();
        let age = &head[head.len() - age_len..];
        out.extend_from_slice(&head[..head.len() - age_len]);
        if age.first().is_some_and(u8::is_ascii_digit) {
            out.extend_from_slice(b"AGE");
        } else {
            out.extend_from_slice(age);
        }
        out.extend_from_slice(SUFFIX);
        rest = &rest[at + SUFFIX.len()..];
    }
    out.extend_from_slice(rest);
    out
}

fn assert_pair(shell: &Output, native: &Output) {
    assert_eq!(
        native.status.code(),
        shell.status.code(),
        "native stderr: {}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(
        normalize_stamp_age(&native.stdout),
        normalize_stamp_age(&shell.stdout),
        "doctor stdout"
    );
    assert_eq!(native.stderr, shell.stderr, "doctor stderr");
}

#[cfg(unix)]
fn seal(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("fixture mode");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_live(pid: i32) -> bool {
    match std::fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let end = stat
                .windows(2)
                .rposition(|part| part == b") ")
                .expect("well-formed proc stat");
            stat.get(end + 2) != Some(&b'Z')
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("could not inspect process {pid}: {error}"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn process_live(pid: i32) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("could not inspect process {pid}: {error}"));
    if output.status.success() {
        portable_process_live(&output.stdout)
    // SAFETY: a positive PID and signal zero only test existence.
    } else if unsafe { libc::kill(pid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        false
    } else {
        panic!(
            "ps could not inspect live process {pid}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

fn portable_process_live(status: &[u8]) -> bool {
    !matches!(
        status
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace()),
        None | Some(b'Z')
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DoctorProcessIdentity {
    pid: i32,
    group: i32,
    session: i32,
    generation: Vec<u8>,
}

struct PinnedDoctorProcess {
    identity: DoctorProcessIdentity,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pidfd: std::os::fd::OwnedFd,
}

impl PinnedDoctorProcess {
    fn claim(identity: DoctorProcessIdentity) -> Option<Self> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::fd::FromRawFd as _;

            // SAFETY: pidfd_open only observes the positive fixture PID.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
            if fd < 0 {
                return None;
            }
            // SAFETY: a successful pidfd_open returns one newly owned fd.
            let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
            if !same_doctor_process(&identity) {
                return None;
            }
            Some(Self { identity, pidfd })
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            same_doctor_process(&identity).then_some(Self { identity })
        }
    }

    fn signal_for_cleanup(&self, signal: i32) -> bool {
        if !same_doctor_process(&self.identity) {
            return false;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::fd::AsRawFd as _;

            if self.identity.pid == self.identity.group
                && self.identity.pid == self.identity.session
                && self.identity.group != unsafe { libc::getpgrp() }
            {
                // The retained pidfd anchors this private process-group number
                // across validation and delivery.
                // SAFETY: this group is owned by the pinned fixture leader.
                if unsafe { libc::kill(-self.identity.group, signal) } == 0 {
                    return true;
                }
            }
            // SAFETY: pidfd_send_signal targets the retained kernel identity.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) == 0
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = signal;
            false
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn doctor_process_generation(pid: i32) -> Option<Vec<u8>> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.windows(2).rposition(|part| part == b") ")?;
    stat[end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .nth(19)
        .map(<[u8]>::to_vec)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn doctor_process_generation(pid: i32) -> Option<Vec<u8>> {
    let output = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let generation = output.status.success().then_some(output.stdout)?;
    (!generation.iter().all(u8::is_ascii_whitespace)).then_some(generation)
}

fn doctor_process_identity(pid: i32) -> Option<DoctorProcessIdentity> {
    let generation = doctor_process_generation(pid)?;
    // SAFETY: `pid` is positive and these calls only inspect process topology.
    let (group, session) = unsafe { (libc::getpgid(pid), libc::getsid(pid)) };
    let identity = DoctorProcessIdentity {
        pid,
        group,
        session,
        generation,
    };
    (group > 0 && session > 0 && doctor_process_generation(pid)? == identity.generation)
        .then_some(identity)
}

fn same_doctor_process_identity(
    expected: &DoctorProcessIdentity,
    observed: &DoctorProcessIdentity,
) -> bool {
    expected == observed
}

fn same_doctor_process(identity: &DoctorProcessIdentity) -> bool {
    doctor_process_identity(identity.pid)
        .as_ref()
        .is_some_and(|observed| same_doctor_process_identity(identity, observed))
}

#[test]
fn doctor_fixture_rejects_a_stale_process_generation() {
    let current = DoctorProcessIdentity {
        pid: 101,
        group: 202,
        session: 303,
        generation: b"current".to_vec(),
    };
    let stale = DoctorProcessIdentity {
        generation: b"stale".to_vec(),
        ..current.clone()
    };

    assert!(!same_doctor_process_identity(&stale, &current));
    assert!(same_doctor_process_identity(&current, &current));
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn doctor_cleanup_handle_suppresses_reused_generation_delivery() {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new("sleep");
    command.arg("30").stdin(Stdio::null());
    // SAFETY: setsid creates a private fixture session before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().expect("spawn cleanup fixture");
    let identity = doctor_process_identity(child.id() as i32).expect("fixture identity");
    let mut process = PinnedDoctorProcess::claim(identity).expect("pin fixture identity");
    process.identity.generation.push(b'x');

    assert!(!process.signal_for_cleanup(libc::SIGKILL));
    assert_eq!(child.try_wait().expect("observe fixture"), None);
    child.kill().expect("stop retained fixture child");
    child.wait().expect("reap retained fixture child");
}

#[test]
fn empty_portable_process_status_is_not_live() {
    assert!(!portable_process_live(b""));
    assert!(!portable_process_live(b" \n"));
    assert!(!portable_process_live(b"Z+\n"));
    assert!(portable_process_live(b"S+\n"));
}

struct GuardedDoctorChild {
    child: Option<Child>,
    identity: DoctorProcessIdentity,
    reaped: bool,
}

impl GuardedDoctorChild {
    fn new(child: Child) -> Self {
        let identity = doctor_process_identity(child.id() as i32)
            .expect("observe retained doctor process identity");
        Self {
            child: Some(child),
            identity,
            reaped: false,
        }
    }

    fn exited_wnowait(&self) -> std::io::Result<bool> {
        let child = self.child.as_ref().expect("retained doctor child");
        // SAFETY: waitid writes only the initialized local siginfo and WNOWAIT
        // preserves this owned child's identity while descendants are checked.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(info.si_pid() != 0)
        }
    }

    fn signal(&self, signal: i32) -> std::io::Result<()> {
        // The unreaped `Child` retains authority over this exact process even
        // after it exits, so its positive PID cannot be recycled underneath us.
        if unsafe { libc::kill(self.identity.pid, signal) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.as_mut().expect("retained doctor child").wait()?;
        self.reaped = true;
        Ok(status)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reap_with_output(&mut self) -> std::io::Result<Output> {
        let output = self
            .child
            .take()
            .expect("retained doctor child")
            .wait_with_output()?;
        self.reaped = true;
        Ok(output)
    }
}

impl Drop for GuardedDoctorChild {
    fn drop(&mut self) {
        if self.reaped || self.child.is_none() {
            return;
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !self.exited_wnowait().unwrap_or(false) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if self.exited_wnowait().unwrap_or(false) {
            let _ = self.child.as_mut().expect("retained doctor child").wait();
            self.reaped = true;
        }
    }
}

struct GuardedProbeSession {
    process: PinnedDoctorProcess,
    active: bool,
}

impl GuardedProbeSession {
    fn new(leader: i32) -> Self {
        Self::from_identity(
            doctor_process_identity(leader).expect("observe fixture process identity"),
        )
    }

    fn from_identity(identity: DoctorProcessIdentity) -> Self {
        Self {
            process: PinnedDoctorProcess::claim(identity).expect("pin fixture process identity"),
            active: true,
        }
    }

    fn observe_stopped(&mut self) -> bool {
        let stopped = !same_doctor_process(&self.process.identity)
            || !process_live(self.process.identity.pid);
        if stopped {
            self.active = false;
        }
        stopped
    }

    fn force_stop(&mut self) {
        if !self.active || !same_doctor_process(&self.process.identity) {
            self.active = false;
            return;
        }
        let _ = self.process.signal_for_cleanup(libc::SIGKILL);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while same_doctor_process(&self.process.identity)
            && process_live(self.process.identity.pid)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !same_doctor_process(&self.process.identity) || !process_live(self.process.identity.pid)
        {
            self.active = false;
        }
    }
}

impl Drop for GuardedProbeSession {
    fn drop(&mut self) {
        self.force_stop();
    }
}

/// Hanging doctor extension `NN-hang<suffix>.sh`: traps every forwarded
/// signal into a marker, records its PID, and starts an escaped-group
/// descendant that ignores everything but TERM.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn hanging_extension(suffix: &str) -> Vec<u8> {
    format!(
        "doctor() {{\n  trap 'printf \"%s\\n\" HUP >>\"$HOME/doctor-worker{suffix}-signal\"' HUP\n  trap 'printf \"%s\\n\" INT >>\"$HOME/doctor-worker{suffix}-signal\"' INT\n  trap 'printf \"%s\\n\" QUIT >>\"$HOME/doctor-worker{suffix}-signal\"' QUIT\n  trap 'printf \"%s\\n\" TERM >>\"$HOME/doctor-worker{suffix}-signal\"' TERM\n  set -m\n  (\n    trap '' HUP INT QUIT\n    trap 'printf \"%s\\n\" TERM >>\"$HOME/doctor-worker{suffix}-descendant-signal\"' TERM\n    printf '%s\\n' \"$BASHPID\" >\"$HOME/doctor-worker{suffix}-descendant\"\n    while :; do sleep 1; done\n  ) </dev/null >/dev/null 2>&1 &\n  printf '%s\\n' \"$BASHPID\" >\"$HOME/doctor-worker{suffix}\"\n  while :; do wait || true; done\n}}\n"
    )
    .into_bytes()
}

/// Cancel `dot doctor` while `hangs` extensions fill its whole worker window
/// (`DOT_DOCTOR_JOBS=hangs`). Every running worker and escaped descendant must
/// receive exactly one cleanup TERM and be reaped, the extension queued behind
/// the full window must never start, and no scratch state may remain.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn assert_doctor_signal_with_window(signal: i32, expected: i32, hangs: usize) {
    let home = TempDir::new("doctor-native-signal-home").expect("home");
    let state = TempDir::new("doctor-native-signal-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    let later = home.path().join("later-extension");
    let temporary = home.path().join("tmp");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::create_dir(&temporary).expect("temporary directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    let suffixes: Vec<String> = (0..hangs)
        .map(|index| {
            if index == 0 {
                String::new()
            } else {
                (index + 1).to_string()
            }
        })
        .collect();
    for (index, suffix) in suffixes.iter().enumerate() {
        let name = format!("{:02}-hang{suffix}.sh", 10 + index);
        std::fs::write(directory.join(&name), hanging_extension(suffix)).expect("extension");
        seal(&directory.join(&name), 0o644);
    }
    std::fs::write(
        directory.join("90-later.sh"),
        b"doctor() { printf ran >\"$HOME/later-extension\"; }\n",
    )
    .expect("later extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("90-later.sh"), 0o644);

    let jobs = hangs.to_string();
    let child = command(
        false,
        &home,
        &state,
        &[
            ("TMPDIR", temporary.to_str().expect("temporary path")),
            ("DOT_DOCTOR_JOBS", jobs.as_str()),
        ],
    )
    .spawn()
    .expect("doctor");
    let mut child = GuardedDoctorChild::new(child);
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut identities: Vec<[Option<DoctorProcessIdentity>; 2]> =
        suffixes.iter().map(|_| [None, None]).collect();
    let mut sessions = loop {
        for (suffix, pair) in suffixes.iter().zip(identities.iter_mut()) {
            for (slot, marker) in pair.iter_mut().zip([
                format!("doctor-worker{suffix}"),
                format!("doctor-worker{suffix}-descendant"),
            ]) {
                if slot.is_none() {
                    *slot = std::fs::read_to_string(home.path().join(marker))
                        .ok()
                        .and_then(|value| value.trim().parse::<i32>().ok())
                        .and_then(doctor_process_identity);
                }
            }
        }
        if identities.iter().flatten().all(Option::is_some) {
            break identities
                .iter_mut()
                .flatten()
                .map(|slot| GuardedProbeSession::from_identity(slot.take().expect("identity")))
                .collect::<Vec<_>>();
        }
        assert!(
            !child.exited_wnowait().expect("doctor status"),
            "doctor exited before starting its cancellation fixture"
        );
        if std::time::Instant::now() >= ready_deadline {
            let _started = identities
                .iter_mut()
                .flatten()
                .filter_map(Option::take)
                .map(GuardedProbeSession::from_identity)
                .collect::<Vec<_>>();
            panic!("doctor cancellation fixture did not start");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    child.signal(signal).expect("signal retained doctor child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !child.exited_wnowait().expect("doctor status") {
        if std::time::Instant::now() >= deadline {
            panic!("doctor did not finish after signal {signal}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sessions
        .iter_mut()
        .any(|session| !session.observe_stopped())
        && std::time::Instant::now() < cleanup_deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let survivors = sessions
        .iter_mut()
        .map(|session| !session.observe_stopped())
        .filter(|survived| *survived)
        .count();
    for session in &mut sessions {
        session.force_stop();
    }
    let output = child.reap_with_output().expect("doctor output");
    let observed = output.status.code();
    assert_eq!(
        observed,
        Some(expected),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        survivors, 0,
        "doctor extension workers or descendants survived"
    );
    for suffix in &suffixes {
        for (marker, label) in [
            (format!("doctor-worker{suffix}-signal"), "worker"),
            (
                format!("doctor-worker{suffix}-descendant-signal"),
                "escaped-group descendant",
            ),
        ] {
            let delivered = std::fs::read_to_string(home.path().join(&marker))
                .expect("delivered signal marker");
            let signals = delivered.lines().collect::<Vec<_>>();
            assert_eq!(
                signals,
                ["TERM"],
                "{label} {suffix:?} did not receive exactly one cleanup TERM"
            );
        }
    }
    assert!(
        !later.exists(),
        "doctor started an extension after cancellation"
    );
    assert_eq!(
        std::fs::read_dir(&temporary)
            .expect("temporary directory")
            .count(),
        0,
        "doctor left extension scratch state"
    );
}

/// The serial window: one hanging extension, and the next extension must
/// not start after cancellation.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn assert_doctor_signal(signal: i32, expected: i32) {
    assert_doctor_signal_with_window(signal, expected, 1);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_doctor_int_reaps_every_parallel_extension() {
    assert_doctor_signal_with_window(libc::SIGINT, 130, 3);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_doctor_term_reaps_every_parallel_extension() {
    assert_doctor_signal_with_window(libc::SIGTERM, 143, 2);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_doctor_hup_reaps_extension() {
    assert_doctor_signal(libc::SIGHUP, 129);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_doctor_int_reaps_extension() {
    assert_doctor_signal(libc::SIGINT, 130);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_doctor_quit_reaps_extension() {
    assert_doctor_signal(libc::SIGQUIT, 131);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_doctor_term_reaps_extension() {
    assert_doctor_signal(libc::SIGTERM, 143);
}

#[test]
#[cfg(target_os = "macos")]
fn native_doctor_unpinned_descendant_fails_closed_boundedly() {
    // Portable supervision has no stable signal authority for an unpinned
    // live member (no pidfds), so it refuses delivery and fails closed
    // with 125 instead of Linux's exact-delivery 1. The descendant must
    // outlive the stop sequence deterministically: a self-exiting sleeper
    // races the authority check (exit 1 when already dead, 125 when live).
    // The test therefore owns cleanup of the surviving descendant, like
    // the provider escaped-stderr fixtures do. The refusal does not
    // short-circuit the TERM/KILL grace phases, and each host snapshot
    // spawns ps plus per-PID getsid queries, so the bound stays generous
    // enough for a loaded runner while remaining well under the
    // descendant lifetime (which proves no awaiting happened).
    let home = TempDir::new("doctor-unpinned-home").expect("home");
    let state = TempDir::new("doctor-unpinned-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    let marker = home.path().join("doctor-unpinned-descendant");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-escape.sh"),
        b"doctor() {\n  set -m\n  (trap '' TERM; printf '%s\\n' \"$BASHPID\" >\"$HOME/doctor-unpinned-descendant\"; sleep 60) </dev/null >/dev/null 2>&1 &\n  until [[ -s $HOME/doctor-unpinned-descendant ]]; do sleep 0.02; done\n}\n",
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-escape.sh"), 0o644);

    let started = std::time::Instant::now();
    let output = command(false, &home, &state, &[])
        .output()
        .expect("doctor output");
    let pid = std::fs::read_to_string(&marker)
        .expect("descendant marker")
        .trim()
        .parse::<i32>()
        .expect("descendant pid");
    assert_eq!(output.status.code(), Some(125));
    kill_process(pid);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_live(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "doctor incomplete cleanup was not bounded"
    );
    assert!(
        !process_live(pid),
        "refused descendant survived test cleanup"
    );
}

#[cfg(target_os = "macos")]
fn kill_process(pid: i32) {
    // SAFETY: positive test-owned PID read from the descendant marker.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

#[test]
fn signal_interrupts_backpressured_doctor_rendering() {
    let home = TempDir::new("doctor-backpressure-home").expect("home");
    let state = TempDir::new("doctor-backpressure-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-output.sh"),
        b"doctor() {\n  python3 - \"$DOT_DOCTOR_RESULT_FILE\" <<'PY'\nimport sys\nwith open(sys.argv[1], 'ab') as result:\n    result.write(b'warn\\tDOCTOR-BLOCKED\\t' + b'x' * (8 * 1024 * 1024) + b'\\n')\nPY\n}\n",
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-output.sh"), 0o644);

    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("stdout pair");
    let child = command(false, &home, &state, &[])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .expect("doctor");
    let mut child = GuardedDoctorChild::new(child);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut reader = std::io::BufReader::new(reader);
        let mut observed = Vec::new();
        loop {
            let mut byte = [0];
            assert_ne!(reader.read(&mut byte).unwrap(), 0, "missing doctor marker");
            observed.push(byte[0]);
            if observed.ends_with(b"DOCTOR-BLOCKED") {
                break;
            }
        }
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        reader.read_to_end(&mut observed).unwrap();
    });
    if started_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .is_err()
    {
        // Let the command's own handler clean any extension session before
        // the bounded guard terminates the retained coordinator.
        let _ = child.signal(libc::SIGTERM);
        let _ = release_tx.send(());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !child.exited_wnowait().unwrap_or(false) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if child.exited_wnowait().unwrap_or(false) {
            let _ = child.reap();
        }
        panic!("doctor rendering did not start");
    }
    child
        .signal(libc::SIGINT)
        .expect("signal retained doctor child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut completed = false;
    while !completed && std::time::Instant::now() < deadline {
        completed = child.exited_wnowait().expect("doctor status");
        if !completed {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let blocked = !completed;
    release_tx.send(()).unwrap();
    if !completed {
        let release_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !completed && std::time::Instant::now() < release_deadline {
            completed = child.exited_wnowait().expect("doctor status after release");
            if !completed {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    assert!(
        completed,
        "doctor did not exit after releasing its output sink"
    );
    let status = child.reap().expect("doctor exit");
    reader.join().unwrap();
    assert!(!blocked, "signal left doctor blocked on an unread stdout");
    assert_eq!(status.code(), Some(130));
}

fn git(cwd: &Path, args: &[&str]) {
    let output = dot_test_support::git()
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git -C {} {args:?}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn origin(scope: &Path) -> std::path::PathBuf {
    let seed = scope.join("seed");
    let origin = scope.join("origin.git");
    std::fs::create_dir_all(&seed).expect("seed directory");
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["config", "user.name", "fixture"]);
    git(&seed, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(seed.join("README.md"), b"fixture\n").expect("seed file");
    git(&seed, &["add", "README.md"]);
    git(
        &seed,
        &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture"],
    );
    let status = dot_test_support::git()
        .args(["init", "--bare", "-q"])
        .arg(&origin)
        .status()
        .expect("bare origin");
    assert!(status.success());
    git(
        &seed,
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("origin text"),
        ],
    );
    git(&seed, &["push", "-q", "origin", "main"]);
    git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    origin
}

fn init_client(home: &TempDir, state: &TempDir, origin: &Path) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(dot_test_support::bash())
        .arg(root.join("bin/dot"))
        .args(["init", "--yes"])
        .arg(format!("file://{}", origin.display()))
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "TMPDIR",
            std::env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into()),
        )
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", state.path())
        .env("DOT_SOURCE_ROOT", root)
        .env("BASH", dot_test_support::bash())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("initialize client");
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A logging `git` wrapper doctor will actually select as its host Git.
///
/// Doctor resolves Git from `PATH` but skips any candidate under `HOME` or
/// the Dot checkout (`init_client_identity::select_command_git`), and the
/// exec-capable fixture root (`TempDir::new_exec`) lives in the checkout's
/// target directory unless `CARGO_TARGET_DIR` points elsewhere. So the
/// wrapper goes in the first fixture root outside the checkout whose files
/// can execute; no such root is a test-environment error, not a pass. The
/// wrapper execs the real Git directly, never a developer launcher.
fn host_git_wrapper(label: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    host_git_wrapper_running(label, "")
}

/// [`host_git_wrapper`] that runs the shell `snippet` (after logging, before
/// the real Git) on every call. The wrapper must live outside the fixture
/// `HOME` and the Dot checkout: doctor's host-Git selection skips any `git`
/// under either, as client-provided launchers.
fn host_git_wrapper_running(
    label: &str,
    snippet: &str,
) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let checkout = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("checkout");
    let real = dot_test_support::real_tool("git");
    for exec_root in [false, true] {
        let dir = if exec_root {
            TempDir::new_exec(label)
        } else {
            TempDir::new(label)
        }
        .expect("wrapper directory");
        if dir.path().starts_with(&checkout) {
            continue;
        }
        let log = dir.path().join("git.log");
        let wrapper = dir.path().join("git");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\n{snippet}exec '{}' \"$@\"\n",
                log.display(),
                real.display()
            ),
        )
        .expect("git wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("wrapper mode");
        // A `noexec` mount refuses to run it: try the next root. The probe
        // asks for `--exec-path`, which no snippet reacts to. A sibling test
        // thread that forks while this one still has the file open for
        // writing makes exec fail with ETXTBSY until that child execs, and a
        // saturated host can refuse the fork (EAGAIN); both transient
        // refusals are retried, never read as `noexec`.
        let mut attempts = 0;
        let probe = loop {
            let probe = Command::new(&wrapper)
                .arg("--exec-path")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match probe {
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::ETXTBSY | libc::EAGAIN))
                        && attempts < 100 =>
                {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                probe => break probe,
            }
        };
        if probe.is_ok_and(|status| status.success()) {
            let _ = std::fs::remove_file(&log);
            return (dir, wrapper, log);
        }
    }
    panic!(
        "no exec-capable fixture directory outside the checkout {} for the host Git wrapper",
        checkout.display()
    );
}

#[test]
fn base_repository_state_costs_one_status_call() {
    // P3: branch, upstream distance, and tracked changes used to cost four
    // Git processes (and relied on repo config for `-uno`); one
    // porcelain-v2 status answers all of them.
    let scope = TempDir::new("doctor-status-calls-origin").expect("origin scope");
    let home = TempDir::new("doctor-status-calls-home").expect("home");
    let state = TempDir::new("doctor-status-calls-state").expect("state");
    let origin = origin(scope.path());
    init_client(&home, &state, &origin);
    let (wrappers, wrapper, log) = host_git_wrapper("doctor-status-calls-git");
    let path = format!(
        "{}:{}",
        wrappers.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let native = command(false, &home, &state, &[("PATH", &path)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    // The healthy client folds into one row naming its branch.
    assert!(
        stdout.contains("✓ client repository (")
            && stdout.contains("main, current with origin/main)"),
        "{stdout}"
    );
    // A bypassed wrapper must fail here, not as a missing file.
    let calls = std::fs::read_to_string(&log).unwrap_or_else(|_| {
        panic!(
            "doctor never ran the logging Git wrapper {}: it resolved Git elsewhere",
            wrapper.display()
        )
    });
    let base: Vec<&str> = calls
        .lines()
        .filter(|line| line.contains("--work-tree="))
        .collect();
    assert_eq!(
        base.iter()
            .filter(|line| line.ends_with(" status --porcelain=v2 --branch --untracked-files=no"))
            .count(),
        1,
        "{calls}"
    );
    for retired in ["symbolic-ref", "rev-list", "@{u}", "status --porcelain\n"] {
        assert!(
            !base
                .iter()
                .any(|line| format!("{line}\n").contains(retired)),
            "{retired} still runs: {calls}"
        );
    }
}

#[test]
fn missing_client_and_clear_lock_match_without_the_old_engine() {
    // Catches routing doctor back through the removed whole-engine adapter and
    // covers failing source/base plus the healthy lock/no-provider/no-overlay
    // core rows.
    let home = TempDir::new("doctor-native-empty-home").expect("home");
    let state = TempDir::new("doctor-native-empty-state").expect("state");
    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stdout).contains("client repository is missing"));
    assert_pair(&shell, &native);
}

#[test]
fn source_checkout_ignores_caller_git_selection() {
    // Catches bypassing the shared source-Git isolation: caller Git selectors
    // and host-owned container mounts must not hide the already-selected Dot
    // checkout from the runtime health check.
    let home = TempDir::new("doctor-native-source-git-env-home").expect("home");
    let state = TempDir::new("doctor-native-source-git-env-state").expect("state");
    let (shell, native) = pair_with(
        &home,
        &state,
        &[
            ("GIT_DIR", "/definitely/missing.git"),
            ("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1"),
        ],
    );
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains(", checkout)\n"),
        "shell source row: {}",
        String::from_utf8_lossy(&shell.stdout)
    );
    assert_pair(&shell, &native);
}

#[test]
fn extensions_disabled_doctor_does_not_execute_bash_startup_code() {
    let home = TempDir::new_exec("doctor-native-no-bash-code-home").expect("home");
    let state = TempDir::new("doctor-native-no-bash-code-state").expect("state");
    let marker = home.path().join("startup-ran");
    let bash = home.path().join("bash-probe");
    std::fs::write(
        &bash,
        format!(
            "#!/bin/sh\nprintf ran >'{}'\nexec '{}' \"$@\"\n",
            marker.display(),
            dot_test_support::bash().display()
        ),
    )
    .expect("Bash probe");
    std::fs::set_permissions(&bash, std::fs::Permissions::from_mode(0o755))
        .expect("Bash probe mode");

    let native = command(
        false,
        &home,
        &state,
        &[
            ("DOT_BASH", bash.to_str().expect("Bash path")),
            // Keep this regression about Dot's Bash probe. A developer PATH
            // may contain script wrappers for otherwise native tools, which
            // would be caller-supplied execution outside the fixture's scope.
            ("PATH", "/usr/bin:/bin"),
        ],
    )
    .output()
    .expect("native doctor");
    assert!(!native.status.success(), "missing client remains unhealthy");
    assert!(
        !marker.exists(),
        "doctor probed Bash while it was not required"
    );
}

#[test]
fn foreign_legacy_client_refuses_without_the_old_engine() {
    let home = TempDir::new("doctor-native-foreign-legacy-home").expect("home");
    let state = TempDir::new("doctor-native-foreign-legacy-state").expect("state");
    std::fs::create_dir_all(home.path().join(".dotfiles")).expect("foreign client directory");

    let (shell, native) = pair(&home, &state);
    assert!(shell.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&shell.stderr)
            .contains("unsupported or foreign client Git directory")
    );
    assert_pair(&shell, &native);
}

#[test]
fn healthy_legacy_client_matches_without_the_old_engine() {
    let scope = TempDir::new("doctor-native-legacy-scope").expect("scope");
    let home = TempDir::new("doctor-native-legacy-home").expect("home");
    let state = TempDir::new("doctor-native-legacy-state").expect("state");
    let origin = origin(scope.path());
    let status = dot_test_support::git()
        .args(["clone", "--bare", "-q"])
        .arg(&origin)
        .arg(home.path().join(".dotfiles"))
        .status()
        .expect("legacy bare client");
    assert!(status.success());

    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stdout).contains("legacy bare client layout"));
    assert_pair(&shell, &native);
}

#[test]
fn healthy_source_and_client_match_without_the_old_engine() {
    let scope = TempDir::new("doctor-native-client-scope").expect("scope");
    let home = TempDir::new("doctor-native-client-home").expect("home");
    let state = TempDir::new("doctor-native-client-state").expect("state");
    let origin = origin(scope.path());
    init_client(&home, &state, &origin);

    let (shell, native) = pair(&home, &state);
    let output = String::from_utf8_lossy(&shell.stdout);
    assert!(output.contains(", checkout)\n"), "{output}");
    assert!(
        output.contains(
            "✓ client repository (~/.dotfiles, worktree ~, main, current with origin/main)"
        ),
        "{output}"
    );
    assert_pair(&shell, &native);
}

#[test]
fn malformed_client_identity_refuses_before_doctor_without_the_old_engine() {
    let home = TempDir::new("doctor-native-malformed-client-home").expect("home");
    let state = TempDir::new("doctor-native-malformed-client-state").expect("state");
    std::fs::create_dir_all(state.path().join("dot/init")).expect("init state");
    std::fs::write(
        state.path().join("dot/init/completed"),
        b"not an identity\n",
    )
    .expect("malformed identity");
    seal(&state.path().join("dot/init/completed"), 0o600);

    let (shell, native) = pair(&home, &state);
    assert!(shell.stdout.is_empty());
    assert!(String::from_utf8_lossy(&shell.stderr).contains("malformed initialization identity"));
    assert_pair(&shell, &native);
}

#[test]
fn recorded_client_identity_mismatch_refuses_without_the_old_engine() {
    let scope = TempDir::new("doctor-native-mismatch-scope").expect("scope");
    let home = TempDir::new("doctor-native-mismatch-home").expect("home");
    let state = TempDir::new("doctor-native-mismatch-state").expect("state");
    let origin = origin(scope.path());
    init_client(&home, &state, &origin);
    std::fs::rename(
        home.path().join(".dotfiles"),
        home.path().join(".dotfiles-moved"),
    )
    .expect("move client git directory");
    let status = dot_test_support::git()
        .args(["init", "--bare", "-q"])
        .arg(home.path().join(".dotfiles"))
        .status()
        .expect("foreign client git directory");
    assert!(status.success());

    let (shell, native) = pair(&home, &state);
    assert!(shell.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&shell.stderr)
            .contains("no longer matches initialization identity")
    );
    assert_pair(&shell, &native);
}

#[test]
fn ignored_overlay_resolution_failure_still_runs_natively() {
    // Catches treating the doctor's tolerated inspect failure as a dispatcher
    // failure: the resolution diagnostic remains on stderr and the health
    // coordinator still renders all sections.
    let home = TempDir::new("doctor-native-bad-overlay-home").expect("home");
    let state = TempDir::new("doctor-native-bad-overlay-state").expect("state");
    let directory = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&directory).expect("overlay directory");
    std::fs::write(home.path().join(".config/dot/config"), b"version=1\n").expect("config");
    std::fs::write(directory.join("90-bad.conf"), b"url=x\nsync=hg\n").expect("descriptor");

    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stderr).contains("unknown sync value: hg"));
    assert!(String::from_utf8_lossy(&shell.stdout).contains("dot runtime"));
    assert_pair(&shell, &native);
}

#[test]
fn unsafe_lock_and_unavailable_provider_match_without_the_old_engine() {
    let home = TempDir::new("doctor-native-lock-home").expect("home");
    let state = TempDir::new("doctor-native-lock-state").expect("state");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\ndependency_provider=shdeps\n",
    )
    .expect("config");
    std::fs::create_dir_all(state.path().join("dot")).expect("state directory");
    std::os::unix::fs::symlink(
        state.path().join("foreign-lock"),
        state.path().join("dot/update.lock.d"),
    )
    .expect("unsafe lock");

    let (shell, native) = pair(&home, &state);
    let output = String::from_utf8_lossy(&shell.stdout);
    assert!(output.contains("update lock path is unsafe"));
    assert!(output.contains("Shdeps provider is unavailable"));
    assert_pair(&shell, &native);
}

/// Build a sealed doctor extension tree under a fresh fixture home.
fn doctor_extension_fixture(tag: &str, extensions: &[(String, Vec<u8>)]) -> (TempDir, TempDir) {
    let home = TempDir::new(&format!("doctor-{tag}-home")).expect("home");
    let state = TempDir::new(&format!("doctor-{tag}-state")).expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::create_dir(home.path().join("tmp")).expect("temporary directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    for (name, body) in extensions {
        std::fs::write(directory.join(name), body).expect("extension");
        seal(&directory.join(name), 0o644);
    }
    seal(&root, 0o700);
    seal(&directory, 0o700);
    (home, state)
}

/// [`doctor_extension_fixture`] around an initialized, healthy client, so
/// doctor's exit status reflects the extensions alone (the bare fixture has
/// no client and always exits 1). Returns the origin scope too, which must
/// outlive the run.
fn doctor_extension_client_fixture(
    tag: &str,
    extensions: &[(String, Vec<u8>)],
) -> (TempDir, TempDir, TempDir) {
    let scope = TempDir::new(&format!("doctor-{tag}-origin")).expect("origin scope");
    let home = TempDir::new(&format!("doctor-{tag}-home")).expect("home");
    let state = TempDir::new(&format!("doctor-{tag}-state")).expect("state");
    let origin = origin(scope.path());
    init_client(&home, &state, &origin);
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::create_dir(home.path().join("tmp")).expect("temporary directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    for (name, body) in extensions {
        std::fs::write(directory.join(name), body).expect("extension");
        seal(&directory.join(name), 0o644);
    }
    seal(&root, 0o700);
    seal(&directory, 0o700);
    (scope, home, state)
}

/// The good extension the client-fixture tests share.
fn good_extension() -> (String, Vec<u8>) {
    (
        "10-good.sh".to_string(),
        b"doctor() {\n  dot_doctor_section 'Good'\n  dot_doctor_ok 'good extension ran'\n}\n"
            .to_vec(),
    )
}

#[test]
fn healthy_client_with_a_good_extension_exits_zero() {
    // The control for the refused/timed-out exit-status tests below.
    let (_scope, home, state) = doctor_extension_client_fixture("healthy", &[good_extension()]);
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ good extension ran\n"), "{stdout}");
    assert!(stdout.contains(" 0 failed"), "{stdout}");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
}

/// Run native doctor with a fixed worker window and the fixture's private
/// `TMPDIR`, returning the output and whether that `TMPDIR` is empty again.
fn doctor_with_jobs(home: &TempDir, state: &TempDir, jobs: &str) -> (Output, bool) {
    doctor_with_env(home, state, &[("DOT_DOCTOR_JOBS", jobs)])
}

/// [`doctor_with_jobs`] with arbitrary job-policy variables.
fn doctor_with_env(home: &TempDir, state: &TempDir, extra: &[(&str, &str)]) -> (Output, bool) {
    let temporary = home.path().join("tmp");
    let mut env = vec![("TMPDIR", temporary.to_str().expect("temporary path"))];
    env.extend_from_slice(extra);
    let output = command(false, home, state, &env)
        .output()
        .expect("native doctor");
    let clean = std::fs::read_dir(&temporary)
        .expect("temporary directory")
        .next()
        .is_none();
    (output, clean)
}

/// Extensions covering every record and tail shape: ok/warn/fail/skip rows,
/// sections, a nonzero worker status, and stray stdout/stderr output. Earlier
/// extensions sleep longer so parallel completion order is the reverse of
/// discovery order; the sleep only perturbs scheduling and never
/// synchronizes anything.
fn mixed_extensions(count: usize) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|index| {
            let delay = (count - index) as f64 * 0.02;
            let body = match index % 5 {
                0 => format!(
                    "doctor() {{\n  sleep {delay}\n  dot_doctor_section 'Section {index}'\n  dot_doctor_ok 'check {index}' 'detail {index}'\n}}\n"
                ),
                1 => format!(
                    "doctor() {{\n  sleep {delay}\n  dot_doctor_warn 'warning {index}' 'hint {index}'\n  dot_doctor_skip 'skipped {index}'\n}}\n"
                ),
                2 => format!(
                    "doctor() {{\n  sleep {delay}\n  dot_doctor_fail 'failure {index}' 'fix {index}'\n  return 3\n}}\n"
                ),
                3 => format!(
                    "doctor() {{\n  sleep {delay}\n  printf 'stray stdout {index}\\n'\n  printf 'stray stderr {index}\\n' >&2\n  dot_doctor_ok 'noisy {index}'\n}}\n"
                ),
                _ => format!("doctor() {{\n  sleep {delay}\n  return 0\n}}\n"),
            };
            (format!("{:03}-mixed{index}.sh", 100 + index), body.into_bytes())
        })
        .collect()
}

fn assert_same_doctor(serial: &Output, parallel: &Output, context: &str) {
    assert_eq!(
        parallel.status.code(),
        serial.status.code(),
        "{context}: exit status; stderr={}",
        String::from_utf8_lossy(&parallel.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&parallel.stdout),
        String::from_utf8_lossy(&serial.stdout),
        "{context}: stdout"
    );
    assert_eq!(
        String::from_utf8_lossy(&parallel.stderr),
        String::from_utf8_lossy(&serial.stderr),
        "{context}: stderr"
    );
}

#[test]
fn parallel_doctor_extensions_run_concurrently() {
    // The earlier extension can only report success if the later one runs
    // while it is still waiting, which a serial loop can never satisfy.
    let (home, state) = overlap_fixture("parallel-overlap");
    let (output, clean) = doctor_with_jobs(&home, &state, "2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The bare fixture home fails unrelated client checks, so judge the
    // extension records rather than the aggregate exit status.
    assert!(
        !stdout.contains("did not overlap") && !stdout.contains("doctor extension failed"),
        "stdout={stdout}"
    );
    let overlapped = stdout.find("extensions overlapped").expect("waiter record");
    let started = stdout
        .find("later extension started")
        .expect("signaler record");
    assert!(
        overlapped < started,
        "records must render in discovery order: {stdout}"
    );
    assert!(clean, "doctor left extension scratch state");
}

#[test]
fn parallel_doctor_output_matches_serial_output() {
    let (home, state) = doctor_extension_fixture("parallel-parity", &mixed_extensions(10));
    let (serial, serial_clean) = doctor_with_jobs(&home, &state, "1");
    let serial_stdout = String::from_utf8_lossy(&serial.stdout);
    for expected in [
        "Section 0",
        "warning 1",
        "failure 2",
        "mixed2 doctor extension failed",
        "mixed3 doctor extension wrote outside the result API",
        "stray stderr 3",
    ] {
        assert!(
            serial_stdout.contains(expected),
            "serial fixture lacks {expected:?}: {serial_stdout}"
        );
    }
    assert_ne!(
        serial.status.code(),
        Some(0),
        "fixture failures must fail doctor"
    );
    let (parallel, parallel_clean) = doctor_with_jobs(&home, &state, "8");
    assert_same_doctor(&serial, &parallel, "jobs=8");
    assert!(
        serial_clean && parallel_clean,
        "doctor left extension scratch state"
    );
}

#[test]
fn zero_doctor_jobs_runs_serially() {
    // `DOT_DOCTOR_JOBS=0` normalizes to one worker like the other job knobs.
    let (home, state) = serial_probe_fixture("zero-jobs");
    let (output, clean) = doctor_with_jobs(&home, &state, "0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("later extension started"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("extensions ran serially"),
        "stdout={stdout}"
    );
    assert!(clean, "doctor left extension scratch state");
}

/// Marks that the later extension started, for the waiter fixtures below.
const SIGNALER: &[u8] =
    b"doctor() {\n  : >\"$HOME/later-started\"\n  dot_doctor_ok 'later extension started'\n}\n";

fn overlap_fixture(tag: &str) -> (TempDir, TempDir) {
    let waiter = b"doctor() {\n  local deadline=$((SECONDS + 30))\n  until [[ -e $HOME/later-started ]]; do\n    ((SECONDS < deadline)) || { dot_doctor_fail 'extensions did not overlap'; return 1; }\n    sleep 0.02\n  done\n  dot_doctor_ok 'extensions overlapped'\n}\n";
    doctor_extension_fixture(
        tag,
        &[
            ("10-waiter.sh".to_string(), waiter.to_vec()),
            ("20-signaler.sh".to_string(), SIGNALER.to_vec()),
        ],
    )
}

/// A waiter that polls long enough for any concurrent sibling to start, so
/// only a strictly serial run reports that it never saw one.
fn serial_probe_fixture(tag: &str) -> (TempDir, TempDir) {
    let waiter = b"doctor() {\n  local tries\n  for ((tries = 0; tries < 50; tries++)); do\n    [[ -e $HOME/later-started ]] && { dot_doctor_fail 'extensions overlapped'; return 0; }\n    sleep 0.02\n  done\n  dot_doctor_ok 'extensions ran serially'\n}\n";
    doctor_extension_fixture(
        tag,
        &[
            ("10-waiter.sh".to_string(), waiter.to_vec()),
            ("20-signaler.sh".to_string(), SIGNALER.to_vec()),
        ],
    )
}

#[test]
fn doctor_jobs_fall_back_to_update_jobs() {
    // Without DOT_DOCTOR_JOBS the window follows DOT_UPDATE_JOBS: two
    // workers let the waiter observe its sibling.
    let (home, state) = overlap_fixture("update-jobs-parallel");
    let (output, clean) = doctor_with_env(&home, &state, &[("DOT_UPDATE_JOBS", "2")]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("extensions overlapped"), "stdout={stdout}");
    assert!(clean, "doctor left extension scratch state");
}

#[test]
fn doctor_jobs_override_update_jobs() {
    // DOT_DOCTOR_JOBS wins over DOT_UPDATE_JOBS: one worker means the
    // later extension only starts after the earlier one finished.
    let (home, state) = serial_probe_fixture("doctor-jobs-override");
    let (output, clean) = doctor_with_env(
        &home,
        &state,
        &[("DOT_DOCTOR_JOBS", "1"), ("DOT_UPDATE_JOBS", "8")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("extensions ran serially"),
        "stdout={stdout}"
    );
    assert!(clean, "doctor left extension scratch state");
}

#[test]
fn parallel_doctor_stress_preserves_serial_output() {
    // Many short extensions through a window narrower than the extension
    // count exercise refill, ordered drain, and scratch cleanup repeatedly.
    let (home, state) = doctor_extension_fixture("parallel-stress", &mixed_extensions(40));
    let (serial, serial_clean) = doctor_with_jobs(&home, &state, "1");
    assert!(serial_clean, "serial doctor left extension scratch state");
    for (round, jobs) in ["2", "7", "16", "64", "7"].into_iter().enumerate() {
        let (parallel, clean) = doctor_with_jobs(&home, &state, jobs);
        assert_same_doctor(&serial, &parallel, &format!("round {round} jobs={jobs}"));
        assert!(
            clean,
            "round {round} jobs={jobs} left extension scratch state"
        );
    }
}

#[test]
fn trusted_failing_extension_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-extension-home").expect("home");
    let state = TempDir::new("doctor-native-extension-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("20-failing.sh"),
        b"doctor() {\n  dot_doctor_fail 'expected extension failure' 'fixture failure'\n  return 1\n}\n",
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("20-failing.sh"), 0o644);

    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stdout).contains("expected extension failure"));
    assert_pair(&shell, &native);
}

#[test]
fn invalid_explicit_bash_prevents_doctor_extension_execution() {
    let home = TempDir::new("doctor-native-invalid-bash-home").expect("home");
    let state = TempDir::new("doctor-native-invalid-bash-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    let marker = home.path().join("extension-ran");
    let missing = home.path().join("missing/bash");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-marker.sh"),
        format!(
            "doctor() {{ printf ran >'{}'; dot_doctor_ok marker; }}\n",
            marker.display()
        ),
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-marker.sh"), 0o644);

    let (code, stdout, stderr) = run_in_process(
        &home,
        &state,
        home.path(),
        &[("DOT_BASH", missing.as_os_str())],
    );
    let expected = format!(
        "checkout Bash resolver: explicit interpreter is not Bash 4 or newer: {}\n",
        missing.display()
    );

    assert_eq!(code, 1);
    assert_eq!(stderr, expected.as_bytes());
    assert!(
        String::from_utf8_lossy(&stdout).contains("Bash runtime is too old"),
        "doctor output: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(!marker.exists(), "doctor extension ran without Bash 4+");
    // K7: the extensions that did not run get a row of their own.
    assert!(
        String::from_utf8_lossy(&stdout).contains(
            "  · doctor extensions did not run (they need Bash 4 or newer; see the Bash runtime row)\n"
        ),
        "doctor output: {}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn shdeps_provider_requires_bash_without_extensions() {
    let home = TempDir::new("doctor-native-provider-bash-home").expect("home");
    let state = TempDir::new("doctor-native-provider-bash-state").expect("state");
    let missing = home.path().join("missing/bash");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\ndependency_provider=shdeps\n",
    )
    .expect("config");

    let (code, stdout, stderr) = run_in_process(
        &home,
        &state,
        home.path(),
        &[("DOT_BASH", missing.as_os_str())],
    );
    let expected = format!(
        "checkout Bash resolver: explicit interpreter is not Bash 4 or newer: {}\n",
        missing.display()
    );

    assert_eq!(code, 1);
    assert_eq!(stderr, expected.as_bytes());
    assert!(
        String::from_utf8_lossy(&stdout).contains("Bash runtime is too old"),
        "doctor output: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        !String::from_utf8_lossy(&stdout).contains("Bash runtime is not required"),
        "doctor output: {}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn in_process_doctor_uses_runtime_identity_probe() {
    let home = TempDir::new("doctor-native-runtime-user-home").expect("home");
    let state = TempDir::new("doctor-native-runtime-user-state").expect("state");
    let bin = home.path().join("bin");
    let profiles = home.path().join(".config/dot/profiles.d");
    let selectors = home.path().join(".config/dot/profile-selectors.local.d");
    std::fs::create_dir_all(&bin).expect("bin directory");
    std::fs::create_dir_all(&profiles).expect("profiles directory");
    std::fs::create_dir_all(&selectors).expect("selectors directory");
    let id = bin.join("id");
    std::fs::write(
        &id,
        b"#!/bin/sh\ncase $1 in\n  -u) exec /usr/bin/id -u ;;\n  -un) printf 'runtime-user\\n' ;;\nesac\n",
    )
    .expect("id probe");
    std::fs::write(profiles.join("base.conf"), b"version=1\noverlays=core\n")
        .expect("base profile");
    std::fs::write(
        profiles.join("web.conf"),
        b"version=1\nprofiles=base\noverlays=web\n",
    )
    .expect("web profile");
    let selector = selectors.join("10-runtime.conf");
    std::fs::write(&selector, b"version=1\nuser=runtime-user\nprofile=web\n").expect("selector");
    seal(&id, 0o755);
    seal(&selectors, 0o700);
    seal(&selector, 0o600);
    let (_, stdout, stderr) = run_in_process(
        &home,
        &state,
        home.path(),
        &[("PATH", OsStr::new("bin:/usr/bin:/bin"))],
    );
    assert!(stderr.is_empty(), "doctor stderr: {stderr:?}");
    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains("; for runtime-user@"),
        "doctor output: {output}"
    );
    assert!(
        output.contains("› profile web (agreed-match"),
        "doctor output: {output}"
    );
}

#[test]
fn in_process_doctor_uses_runtime_temporary_root() {
    let home = TempDir::new("doctor-native-runtime-tmp-home").expect("home");
    let state = TempDir::new("doctor-native-runtime-tmp-state").expect("state");
    let temporary_root = home.path().join("runtime-tmp");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&temporary_root).expect("temporary root");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-runtime.sh"),
        b"doctor() { dot_doctor_ok 'runtime temporary root' \"$TMPDIR\"; }\n",
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-runtime.sh"), 0o644);

    let (_, stdout, stderr) = run_in_process(
        &home,
        &state,
        home.path(),
        &[("TMPDIR", temporary_root.as_os_str())],
    );
    assert!(stderr.is_empty(), "doctor stderr: {stderr:?}");
    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains(temporary_root.to_string_lossy().as_ref()),
        "doctor output: {output}"
    );
}

#[test]
fn unsafe_and_malformed_extensions_match_without_the_old_engine() {
    let home = TempDir::new("doctor-native-unsafe-extension-home").expect("home");
    let state = TempDir::new("doctor-native-unsafe-extension-state").expect("state");
    let root = home.path().join("extensions");
    let real = root.join("doctor-real");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&real).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(real.join("Bad.sh"), b"not_doctor() { :; }\n").expect("extension");
    seal(&root, 0o700);
    seal(&real, 0o700);
    seal(&real.join("Bad.sh"), 0o644);
    std::os::unix::fs::symlink("doctor-real", root.join("doctor.d")).expect("unsafe collection");

    let (shell, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&native.stdout).contains(
            "  ✗ doctor extension discovery failed\n    ~/extensions/doctor.d fails the extension trust checks; check its owner and mode\n"
        ),
        "{}",
        String::from_utf8_lossy(&native.stdout)
    );
    assert_pair(&shell, &native);
}

#[test]
fn hung_extension_times_out_and_later_extensions_still_run() {
    // C2: one hung extension used to block every later section until it
    // ended on its own.
    let (_scope, home, state) = doctor_extension_client_fixture(
        "timeout",
        &[
            (
                "10-hangs.sh".to_string(),
                b"doctor() {\n  dot_doctor_section 'Hangs'\n  dot_doctor_ok 'before the hang'\n  sleep 30 &\n  printf '%s\\n' \"$!\" >\"$HOME/hung-pid\"\n  wait\n}\n".to_vec(),
            ),
            (
                "20-after.sh".to_string(),
                b"doctor() {\n  dot_doctor_section 'After'\n  dot_doctor_ok 'later extension ran'\n}\n".to_vec(),
            ),
        ],
    );
    let started = std::time::Instant::now();
    let (output, clean) = doctor_with_env(&home, &state, &[("DOT_DOCTOR_TIMEOUT", "1")]);
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ before the hang\n"), "{stdout}");
    assert!(
        stdout.contains("  ✗ 10-hangs doctor extension timed out\n    stopped after 1s\n    → set DOT_DOCTOR_TIMEOUT to raise the limit\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  ✓ later extension ran\n"), "{stdout}");
    // The timeout is the only failure on this healthy client.
    assert!(stdout.contains(" 1 failed"), "{stdout}");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "doctor waited for the hung extension: {elapsed:?}"
    );
    assert!(clean, "the stopped extension left scratch state");
    // The whole session was stopped, not just abandoned.
    let pid: i32 = std::fs::read_to_string(home.path().join("hung-pid"))
        .expect("hung pid")
        .trim()
        .parse()
        .expect("numeric pid");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while process_live(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!process_live(pid), "hung extension process {pid} survived");
}

#[test]
fn dangling_extension_link_is_refused_alone() {
    // A pull that renames an overlay extension leaves a dangling link until
    // the link phase runs; that used to fail discovery and run nothing.
    let (_scope, home, state) = doctor_extension_client_fixture("dangling", &[good_extension()]);
    let directory = home.path().join("extensions/doctor.d");
    std::os::unix::fs::symlink(
        home.path().join("renamed-away.sh"),
        directory.join("20-gone.sh"),
    )
    .expect("dangling link");
    let (output, clean) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ good extension ran\n"), "{stdout}");
    assert!(
        stdout.contains("  ✗ 20-gone doctor extension refused\n    ~/extensions/doctor.d/20-gone.sh is not linked from an active overlay (a dangling or retired link); run dot update to relink overlay extensions\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("discovery failed"), "{stdout}");
    // The refusal alone fails this otherwise healthy client.
    assert!(stdout.contains(" 1 failed"), "{stdout}");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert!(clean);
}

#[test]
fn untrusted_extension_is_refused_without_running() {
    // Trust still gates every script, just one at a time.
    let (_scope, home, state) = doctor_extension_client_fixture(
        "untrusted",
        &[
            good_extension(),
            (
                "20-writable.sh".to_string(),
                b"doctor() {\n  printf ran >\"$HOME/untrusted-ran\"\n}\n".to_vec(),
            ),
        ],
    );
    seal(
        &home.path().join("extensions/doctor.d/20-writable.sh"),
        0o666,
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ good extension ran\n"), "{stdout}");
    assert!(
        stdout.contains("  ✗ 20-writable doctor extension refused\n    ~/extensions/doctor.d/20-writable.sh fails the extension trust checks; check its owner and mode\n"),
        "{stdout}"
    );
    assert!(!home.path().join("untrusted-ran").exists());
    assert!(stdout.contains(" 1 failed"), "{stdout}");
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn unlinked_overlay_extensions_share_one_refusal_row() {
    // An overlay descriptor typo leaves every overlay-owned extension link
    // untrusted; that used to print one "check its owner and mode" row per
    // link, all for the same cause.
    let (_scope, home, state) =
        doctor_extension_client_fixture("unlinked-overlay", &[good_extension()]);
    let checkout = home.path().join("overlay-checkout");
    std::fs::create_dir_all(&checkout).expect("overlay checkout");
    let directory = home.path().join("extensions/doctor.d");
    for index in 0..7 {
        let target = checkout.join(format!("2{index}-overlay.sh"));
        std::fs::write(&target, b"doctor() { :; }\n").expect("overlay extension");
        std::os::unix::fs::symlink(&target, directory.join(format!("2{index}-overlay.sh")))
            .expect("overlay link");
    }
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ good extension ran\n"), "{stdout}");
    assert_eq!(stdout.matches("doctor extension").count(), 1, "{stdout}");
    assert!(
        stdout.contains("  ✗ 7 doctor extensions refused\n    20-overlay, 21-overlay, 22-overlay, 23-overlay, 24-overlay, 25-overlay, 26-overlay are not linked from an active overlay (dangling or retired links); run dot update to relink overlay extensions\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("owner and mode"), "{stdout}");
    // When the overlays themselves failed to resolve, the row says so.
    let descriptors = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&descriptors).expect("descriptors");
    std::fs::write(
        descriptors.join("typo.conf"),
        b"url=file:///nowhere.git\nsync=never\n",
    )
    .expect("bad descriptor");
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("overlay descriptor invalid"), "{stdout}");
    assert!(
        stdout.contains(
            "the overlays did not resolve; fix the overlay error above, then run dot update"
        ),
        "{stdout}"
    );
}

#[test]
fn authorized_overlay_link_to_an_untrusted_file_keeps_its_own_row() {
    // A link the overlay manifest authorizes, whose target fails trust (a
    // group-writable checkout file), is not fixed by relinking: it must not
    // join the "run dot update" group.
    let (scope, home, state) =
        doctor_extension_client_fixture("authorized-untrusted", &[good_extension()]);
    // The client fixture's origin doubles as the overlay's.
    let origin = scope.path().join("origin.git");
    let overlay = home.path().join(".dotfiles-ov");
    let status = dot_test_support::git()
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&overlay)
        .status()
        .expect("overlay clone");
    assert!(status.success());
    let descriptors = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&descriptors).expect("descriptors");
    std::fs::write(
        descriptors.join("20-ov.conf"),
        format!("url={}\n", origin.display()),
    )
    .expect("descriptor");
    let rel = "extensions/doctor.d/30-ov.sh";
    let source = overlay.join("home").join(rel);
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("source parent");
    std::fs::write(&source, b"doctor() { :; }\n").expect("overlay extension");
    seal(&source, 0o664);
    let overlay_text = overlay.to_str().expect("utf8 overlay");
    let target = dot::repos_overlays::record_link_target(rel, "ov", overlay_text, Some("git"))
        .expect("link target");
    std::os::unix::fs::symlink(&target, home.path().join(rel)).expect("overlay link");
    let manifest = state.path().join("dot/overlay-links");
    std::fs::create_dir_all(manifest.parent().expect("manifest parent")).expect("manifest dir");
    std::fs::write(&manifest, format!("{rel}\tov\t{target}\n")).expect("manifest");
    seal(&manifest, 0o600);
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("✓ ov ("), "{stdout}");
    assert!(
        stdout.contains("  ✗ 30-ov doctor extension refused\n    ~/extensions/doctor.d/30-ov.sh links to a file that fails the extension trust checks; check its owner and mode\n"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("not linked from an active overlay"),
        "{stdout}"
    );
}

#[test]
fn timeout_counts_from_each_extension_launch() {
    // Serial extensions that each finish well inside the limit must never
    // time out, however long they waited for a slot.
    let slow = |index: usize| {
        (
            format!("{index}0-slow{index}.sh"),
            b"doctor() {\n  sleep 2\n  dot_doctor_ok 'slow finished'\n}\n".to_vec(),
        )
    };
    let (home, state) = doctor_extension_fixture("timeout-launch", &[slow(1), slow(2), slow(3)]);
    let (output, _) = doctor_with_env(
        &home,
        &state,
        &[("DOT_DOCTOR_JOBS", "1"), ("DOT_DOCTOR_TIMEOUT", "5")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("slow finished").count(), 3, "{stdout}");
    assert!(!stdout.contains("timed out"), "{stdout}");
}

#[test]
fn info_rows_and_empty_details_render_without_counting() {
    // C3/C4: an empty detail renders like an omitted one, and informational
    // rows render but never count. The guarded form is how an extension that
    // must also run under an older coordinator calls the newer helper.
    let body = |info: bool| {
        let mut body = String::from(
            "doctor() {\n  dot_doctor_section 'Facts'\n  dot_doctor_ok 'no detail' ''\n  dot_doctor_warn 'empty warning' ''\n",
        );
        if info {
            body.push_str("  dot_doctor_info 'selected thing' 'value'\n  if declare -F dot_doctor_info >/dev/null; then dot_doctor_info 'guarded'; else dot_doctor_ok 'guarded'; fi\n");
        }
        body.push_str("}\n");
        vec![("10-facts.sh".to_string(), body.into_bytes())]
    };
    let (home, state) = doctor_extension_fixture("info", &body(true));
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ no detail\n"), "{stdout}");
    assert!(stdout.contains("  ⚠ empty warning\n  ›"), "{stdout}");
    assert!(!stdout.contains("()"), "{stdout}");
    assert!(stdout.contains("  › selected thing (value)\n"), "{stdout}");
    assert!(stdout.contains("  › guarded\n"), "{stdout}");
    let (home_plain, state_plain) = doctor_extension_fixture("info-plain", &body(false));
    let (plain, _) = doctor_with_env(&home_plain, &state_plain, &[]);
    let summary = |out: &str| {
        out.lines()
            .find(|line| line.contains(" passed · "))
            .map(str::to_string)
            .expect("summary line")
    };
    assert_eq!(
        summary(&stdout),
        summary(&String::from_utf8_lossy(&plain.stdout)),
        "informational rows must not change the counts"
    );
}

#[test]
fn unsafe_extension_is_refused_beside_a_malformed_identity() {
    let home = TempDir::new("doctor-native-extension-order-home").expect("home");
    let state = TempDir::new("doctor-native-extension-order-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(directory.join("10-unsafe.sh"), b"doctor() { :; }\n").expect("unsafe");
    std::fs::write(directory.join("Bad.sh"), b"doctor() { :; }\n").expect("malformed");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-unsafe.sh"), 0o666);
    seal(&directory.join("Bad.sh"), 0o644);

    // The unsafe script and the malformed name are each refused on their
    // own; neither fails discovery as a whole (K1).
    let (shell, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("10-unsafe doctor extension refused"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  ✗ Bad doctor extension refused\n"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("doctor extension discovery failed"),
        "{stdout}"
    );
    assert!(
        native.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_pair(&shell, &native);
}

#[test]
fn identity_errors_refuse_only_their_own_extension() {
    // K1: one malformed or duplicate name used to file a detail-less
    // "discovery failed" row and hide every extension section.
    let ok = |label: &str| {
        format!("doctor() {{\n  dot_doctor_section '{label}'\n  dot_doctor_ok '{label} ran'\n}}\n")
            .into_bytes()
    };
    let (_scope, home, state) = doctor_extension_client_fixture(
        "identity",
        &[
            ("10-first.sh".to_string(), ok("first")),
            ("15-Upper.sh".to_string(), ok("upper")),
            ("20-tools.sh".to_string(), ok("tools")),
            ("21-tools.sh".to_string(), ok("renumbered")),
            ("30-last.sh".to_string(), ok("last")),
        ],
    );
    let (output, clean) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let directory = "~/extensions/doctor.d";
    assert!(
        stdout.contains(&format!(
            "  ✗ 15-Upper doctor extension refused\n    {directory}/15-Upper.sh has an invalid name; rename it to NN-name using lowercase letters, digits, and hyphens\n"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "  ✗ 21-tools doctor extension refused\n    {directory}/21-tools.sh repeats identity tools of 20-tools.sh; remove or rename one of them\n"
        )),
        "{stdout}"
    );
    for ran in ["first ran", "tools ran", "last ran"] {
        assert!(stdout.contains(&format!("  ✓ {ran}\n")), "{stdout}");
    }
    assert!(!stdout.contains("upper ran"), "{stdout}");
    assert!(!stdout.contains("renumbered ran"), "{stdout}");
    assert!(!stdout.contains("discovery failed"), "{stdout}");
    // The row carries the reason; stderr no longer repeats it.
    assert!(!stderr.contains("doctor extension identity"), "{stderr}");
    assert!(stdout.contains(" 2 failed"), "{stdout}");
    assert_eq!(output.status.code(), Some(1));
    assert!(clean);
}

#[test]
fn unsafe_extension_directory_names_the_path_and_next_step() {
    // K7: discovery failures carried no detail at all.
    let (home, state) = doctor_extension_fixture("unsafe-dir", &[good_extension()]);
    seal(&home.path().join("extensions/doctor.d"), 0o777);
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ doctor extension discovery failed\n    ~/extensions/doctor.d fails the extension trust checks; check its owner and mode\n"
        ),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(1));
}

/// A client fixture running one extension body as `10-crash.sh`.
fn crash_fixture(tag: &str, body: &str) -> (TempDir, TempDir, TempDir) {
    doctor_extension_client_fixture(
        tag,
        &[("10-crash.sh".to_string(), body.as_bytes().to_vec())],
    )
}

#[test]
fn crashed_extension_reports_status_and_failing_line() {
    // K3: an extension killed by `set -e` used to file an empty failure row.
    let (_scope, home, state) = crash_fixture(
        "crash-line",
        "doctor() {\n  dot_doctor_section 'Crash'\n  grep -q missing /dev/null\n  dot_doctor_ok 'unreachable'\n}\n",
    );
    let (output, clean) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ 10-crash doctor extension failed\n    exited with status 1 at doctor.d/10-crash.sh:3: grep -q missing /dev/null\n"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("unreachable"), "{stdout}");
    assert_eq!(output.status.code(), Some(1));
    assert!(clean, "the failure note leaked scratch state");
}

#[test]
fn crashed_pipeline_says_an_earlier_element_failed() {
    // Under `pipefail` the failing element is not the last one, but Bash
    // only exposes the last element's command and line: the row used to
    // blame `sort`, which succeeded. It now marks the pipeline and lists
    // each element's status.
    let (_scope, home, state) = crash_fixture(
        "crash-pipeline",
        "doctor() {\n  dot_doctor_section 'Crash'\n  grep -q missing /dev/null | sort\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "    exited with status 1 at doctor.d/10-crash.sh:3: pipeline with statuses 1 0, ending in: sort\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_test_after_a_pipeline_is_not_reported_as_the_pipeline() {
    // `[[` and `((` leave the previous pipeline's statuses in place, so they
    // must not be read as a pipeline failure.
    let (_scope, home, state) = crash_fixture(
        "crash-after-pipeline",
        "doctor() {\n  true | true\n  [[ -e /nonexistent/dot-doctor ]]\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "    exited with status 1 at doctor.d/10-crash.sh:3: [[ -e /nonexistent/dot-doctor ]]\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_failed_redirection_after_a_pipeline_is_not_reported_as_the_pipeline() {
    // The group's redirection fails after its pipeline succeeded: the
    // leftover all-zero statuses are not that failure.
    let (_scope, home, state) = crash_fixture(
        "crash-redirect-after-pipeline",
        "doctor() {\n  { true | true; } > /nonexistent/dot-doctor/out\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("10-crash doctor extension failed"),
        "{stdout}"
    );
    assert!(!stdout.contains("pipeline with statuses"), "{stdout}");
}

#[test]
fn rejected_helper_record_reports_the_calling_line() {
    // A newline in a detail makes the helper return 2, which `set -e` turns
    // into a silent crash at the call site.
    let (_scope, home, state) = crash_fixture(
        "crash-helper",
        "doctor() {\n  dot_doctor_section 'Crash'\n  dot_doctor_warn 'bad' $'two\\nlines'\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("    exited with status 2 at doctor.d/10-crash.sh:3: dot_doctor_warn\n"),
        "{stdout}"
    );
}

#[test]
fn failing_last_command_reports_the_command() {
    // `doctor` ending on a false `[[ ... ]] && ...` returns 1 from the
    // function itself, so only the command is known, not its line.
    let (_scope, home, state) = crash_fixture(
        "crash-last",
        "doctor() {\n  dot_doctor_ok 'ran'\n  [[ -e /nonexistent/dot-doctor ]] && dot_doctor_warn 'never'\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ 10-crash doctor extension failed\n    exited with status 1; last command: [[ -e /nonexistent/dot-doctor ]]\n"
        ),
        "{stdout}"
    );
}

#[test]
fn explicit_return_reports_the_status_and_command() {
    let (_scope, home, state) = crash_fixture(
        "crash-return",
        "doctor() {\n  dot_doctor_ok 'ran'\n  return 4\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ 10-crash doctor extension failed\n    exited with status 4; last command: return 4\n"
        ),
        "{stdout}"
    );
}

#[test]
fn handled_failures_inside_substitutions_stay_quiet() {
    // The failure trap is inherited into command substitutions, where a
    // failing command does not stop the extension. It must not surface.
    let (_scope, home, state) = crash_fixture(
        "crash-quiet",
        "doctor() {\n  local value\n  value=$(false; printf ok)\n  grep -q missing /dev/null || true\n  dot_doctor_ok \"quiet $value\"\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  ✓ quiet ok\n"), "{stdout}");
    assert!(!stdout.contains("10-crash doctor extension"), "{stdout}");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
}

#[test]
fn extension_without_entry_point_says_so() {
    let (_scope, home, state) = crash_fixture("crash-entry", "not_doctor() { :; }\n");
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ 10-crash doctor extension failed\n    exited with status 1\n    - dot: 10-crash.sh defines no doctor function\n"
        ),
        "{stdout}"
    );
}

#[test]
fn items_and_hints_render_under_their_row() {
    // K2: lists and next steps used to be glued into one long detail line.
    let (_scope, home, state) = doctor_extension_client_fixture(
        "items",
        &[(
            "10-items.sh".to_string(),
            b"doctor() {\n  dot_doctor_section 'Items'\n  if declare -F dot_doctor_item >/dev/null && declare -F dot_doctor_hint >/dev/null; then\n    dot_doctor_warn '7 stale things' 'merged upstream'\n    for n in 1 2 3 4 5 6 7; do dot_doctor_item \"thing $n\"; done\n    dot_doctor_hint 'run cleanup'\n    dot_doctor_ok 'two things'\n    dot_doctor_item 'a'\n    dot_doctor_item ''\n    dot_doctor_item 'b'\n  else\n    dot_doctor_warn 'no item helpers'\n  fi\n}\n"
                .to_vec(),
        )],
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ⚠ 7 stale things\n    merged upstream\n    - thing 1\n    - thing 2\n    - thing 3\n    - thing 4\n    - thing 5\n    +2 more\n    → run cleanup\n  ✓ two things\n    - a\n    - b\n"
        ),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(0), "{stdout}");
}

#[test]
fn orphan_item_is_an_invalid_result() {
    let (_scope, home, state) = doctor_extension_client_fixture(
        "orphan",
        &[(
            "10-orphan.sh".to_string(),
            b"doctor() {\n  dot_doctor_section 'Orphan'\n  dot_doctor_item 'lost'\n}\n".to_vec(),
        )],
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ doctor extension emitted an invalid result\n    item has no check row before it: lost\n"
        ),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn malformed_extension_entrypoint_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-malformed-extension-home").expect("home");
    let state = TempDir::new("doctor-native-malformed-extension-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(directory.join("10-malformed.sh"), b"not_doctor() { :; }\n").expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-malformed.sh"), 0o644);

    let (shell, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&shell.stdout).contains("10-malformed doctor extension failed")
    );
    assert_pair(&shell, &native);
}

#[test]
fn malformed_extension_result_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-malformed-result-home").expect("home");
    let state = TempDir::new("doctor-native-malformed-result-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-malformed.sh"),
        b"doctor() { printf '\\n' >>\"$DOT_DOCTOR_RESULT_FILE\"; }\n",
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-malformed.sh"), 0o644);

    let (shell, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&shell.stdout)
            .contains("doctor extension emitted an invalid result")
    );
    assert_pair(&shell, &native);
}

#[test]
fn trusted_merge_inventory_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-merge-home").expect("home");
    let state = TempDir::new("doctor-native-merge-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("merge-hooks.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("merge directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(directory.join("10-fixture.sh"), b"merge() { :; }\n").expect("hook");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-fixture.sh"), 0o644);

    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stdout).contains("1 hook(s)"));
    assert_pair(&shell, &native);
}

fn merge_fixture(tag: &str) -> (TempDir, TempDir, std::path::PathBuf) {
    let home = TempDir::new(&format!("doctor-native-{tag}-home")).expect("home");
    let state = TempDir::new(&format!("doctor-native-{tag}-state")).expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("merge-hooks.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("merge directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(directory.join("10-fixture.sh"), b"merge() { :; }\n").expect("hook");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-fixture.sh"), 0o644);
    (home, state, directory)
}

fn backdate_mtime(path: &Path, secs_ago: u64) {
    let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for backdating")
        .set_modified(mtime)
        .expect("backdate mtime");
}

#[test]
fn declared_merge_outputs_verify_end_to_end() {
    // Handoff finding #8: the `.outputs` sidecar declares live
    // outputs; doctor verifies each exists. Hooks without a sidecar skip
    // verification.
    let (home, state, directory) = merge_fixture("merge-outputs");
    let output = home.path().join("live.conf");
    std::fs::write(
        directory.join("10-fixture.outputs"),
        format!("{}\n# comment line\n\n", output.display()),
    )
    .expect("sidecar");
    seal(&directory.join("10-fixture.outputs"), 0o644);
    backdate_mtime(&directory.join("10-fixture.sh"), 100);
    backdate_mtime(&directory.join("10-fixture.outputs"), 100);
    std::fs::write(&output, b"live\n").expect("output");

    let (_, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("1 hook(s)"),
        "discovery aggregate kept: {stdout}"
    );
    assert!(
        stdout.contains("merge-hook outputs exist"),
        "existing output passes: {stdout}"
    );

    // A missing output fails the merge section.
    std::fs::remove_file(&output).expect("remove output");
    let (_, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("merge-hook output is missing"),
        "missing output fails: {stdout}"
    );

    // N2: an output older than the hook script is still current; the
    // write-if-changed helpers leave an unchanged output untouched.
    std::fs::write(&output, b"unchanged\n").expect("older output");
    backdate_mtime(&output, 200);
    let (_, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("merge-hook outputs exist") && !stdout.contains("stale"),
        "an older output is not stale: {stdout}"
    );
}

#[test]
fn bad_sidecars_degrade_per_spec_end_to_end() {
    // Fresh-review-B B2: one unreadable/oversized/untrusted sidecar
    // fails its own hook, never the whole inventory; the 1 MiB read
    // cap bounds a corrupt sidecar. Native-only (sidecars are new
    // observability the shell never read).
    let (home, state, directory) = merge_fixture("merge-sidecar-degrade");
    std::fs::write(directory.join("20-second.sh"), b"merge() { :; }\n").expect("hook");
    seal(&directory.join("20-second.sh"), 0o644);
    // Healthy hook: fresh declared output still verifies.
    let output = home.path().join("live.conf");
    std::fs::write(
        directory.join("10-fixture.outputs"),
        format!("{}\n", output.display()),
    )
    .expect("good sidecar");
    seal(&directory.join("10-fixture.outputs"), 0o644);
    backdate_mtime(&directory.join("10-fixture.sh"), 100);
    backdate_mtime(&directory.join("10-fixture.outputs"), 100);
    std::fs::write(&output, b"live\n").expect("output");
    // Sick hook: an oversized sidecar (1 MiB + 1 byte).
    std::fs::write(
        directory.join("20-second.outputs"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .expect("big sidecar");
    seal(&directory.join("20-second.outputs"), 0o644);

    let (_, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("merge-hook outputs exist"),
        "healthy hook still verifies: {stdout}"
    );
    assert!(
        stdout.contains("merge-hook output declaration is invalid"),
        "sick hook fails its spec: {stdout}"
    );
    assert!(
        !stdout.contains("inventory is invalid"),
        "one bad sidecar must not blind the inventory: {stdout}"
    );

    // An untrusted (group-writable) sidecar degrades the same way.
    std::fs::write(directory.join("20-second.outputs"), b"/tmp/x\n").expect("sidecar");
    seal(&directory.join("20-second.outputs"), 0o666);
    let (_, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("merge-hook outputs exist"),
        "healthy hook still verifies: {stdout}"
    );
    assert!(
        stdout.contains("merge-hook output declaration is invalid"),
        "untrusted sidecar fails its spec: {stdout}"
    );
    assert!(
        !stdout.contains("inventory is invalid"),
        "one bad sidecar must not blind the inventory: {stdout}"
    );
}

#[test]
fn undeclared_merge_outputs_skip_verification_end_to_end() {
    let (home, state, _) = merge_fixture("merge-undeclared");
    let (shell, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(stdout.contains("1 hook(s)"));
    assert!(
        !stdout.contains("merge-hook outputs"),
        "no sidecar files no output row: {stdout}"
    );
    assert_pair(&shell, &native);
}

#[test]
fn cron_freshness_reports_stamp_age_end_to_end() {
    // Handoff finding #1: doctor reads the cron last-success stamp
    // a successful `update --cron` writes.
    let (home, state, _) = merge_fixture("cron-freshness");
    let (_, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&native.stdout).contains("cron update success is unknown"),
        "missing stamp skips"
    );

    let stamp = state.path().join("dot/update.last-success");
    std::fs::create_dir_all(stamp.parent().expect("stamp parent")).expect("stamp dir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    std::fs::write(&stamp, format!("{now}\n")).expect("fresh stamp");
    let (_, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&native.stdout).contains("cron update succeeded recently"),
        "fresh stamp passes"
    );

    std::fs::write(&stamp, b"1\n").expect("aged stamp");
    let (shell, native) = pair(&home, &state);
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("cron update has not succeeded recently"),
        "stale stamp warns: {stdout}"
    );
    assert_pair(&shell, &native);
    // The age value itself must agree up to the minute boundary that may
    // fall between the two runs — never by exact bytes (loaded-host flake).
    let shell_age = stamp_age_minutes(&String::from_utf8_lossy(&shell.stdout));
    let native_age = stamp_age_minutes(&stdout);
    assert!(
        shell_age.abs_diff(native_age) <= 1,
        "stamp age diverged: shell {shell_age}m vs native {native_age}m"
    );
}

#[test]
fn cron_freshness_reports_degraded_convergence_end_to_end() {
    // Moe E1: a host that keeps converging while Tools or Prune fails must
    // read as degraded, not as a frozen host that stopped updating.
    let (home, state, _) = merge_fixture("cron-degraded");
    let dir = state.path().join("dot");
    std::fs::create_dir_all(&dir).expect("stamp dir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    std::fs::write(dir.join("update.last-success"), b"1\n").expect("aged stamp");
    std::fs::write(
        dir.join("update.last-converged"),
        format!("{now} tools,prune\n"),
    )
    .expect("fresh degraded convergence");
    let native = command(false, &home, &state, &[]).output().expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("cron update degraded: tools,prune failing"),
        "degraded convergence: {stdout}"
    );
    assert!(stdout.contains("since last success "), "{stdout}");
    assert!(
        !stdout.contains("cron update has not succeeded recently"),
        "degraded host must not read as frozen: {stdout}"
    );

    // Convergence that also stopped is the frozen warning again.
    std::fs::write(dir.join("update.last-converged"), b"2 prune\n").expect("aged convergence");
    let native = command(false, &home, &state, &[]).output().expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("cron update has not succeeded recently"),
        "stopped convergence warns as frozen: {stdout}"
    );
}

#[test]
fn cron_failure_cause_reaches_doctor_end_to_end() {
    // U1/U3: a cron run that failed after a recent clean one is reported
    // with the cause the update recorded, read from the state files that
    // `dot update` writes.
    let (home, state, _) = merge_fixture("cron-failure-cause");
    let dir = state.path().join("dot");
    std::fs::create_dir_all(&dir).expect("stamp dir");
    let now = now_epoch();
    let failed = now - 60;
    std::fs::write(dir.join("update.last-success"), format!("{}\n", now - 1800))
        .expect("recent clean run");
    std::fs::write(
        dir.join("update.last-run"),
        format!("{failed} degraded cron tools\n"),
    )
    .expect("newer degraded run");
    std::fs::write(
        dir.join("update.last-failure"),
        format!(
            "{failed} degraded cron\nitem\ttools\twatchexec/watchexec\terror: blocked transition\n"
        ),
    )
    .expect("cause");
    let native = command(false, &home, &state, &[]).output().expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("⚠ last cron run degraded: tools failing"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "failing: tools: watchexec/watchexec (blocked transition)\n    → run shdeps health"
        ),
        "{stdout}"
    );
    assert!(
        !stdout.contains("cron update succeeded recently"),
        "{stdout}"
    );
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs()
}

#[test]
fn hand_updated_host_reports_its_last_run_end_to_end() {
    // M8: a host updated only by hand used to read "cron update success is
    // unknown" forever; the any-trigger last-run stamp replaces that.
    let home = TempDir::new("doctor-last-run-home").expect("home");
    let state = TempDir::new("doctor-last-run-state").expect("state");
    let dir = state.path().join("dot");
    std::fs::create_dir_all(&dir).expect("stamp dir");
    std::fs::write(
        dir.join("update.last-run"),
        format!("{} ok manual\n", now_epoch() - 3 * 3600),
    )
    .expect("last run");
    // A PATH holding only the probes doctor runs, so whether a `crontab`
    // exists is the test's choice, not the host's.
    let tools = TempDir::new_exec("doctor-last-run-tools").expect("tools");
    std::os::unix::fs::symlink(dot_test_support::real_tool("git"), tools.path().join("git"))
        .expect("git link");
    // `id -un` names the user; host and platform come from the kernel.
    let id = ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"]
        .into_iter()
        .map(|dir| Path::new(dir).join("id"))
        .find(|candidate| candidate.is_file());
    if let Some(id) = id {
        std::os::unix::fs::symlink(id, tools.path().join("id")).expect("id link");
    }
    let cron = TempDir::new_exec("doctor-last-run-crontab").expect("cron tools");
    let crontab = cron.path().join("crontab");
    std::fs::write(&crontab, b"#!/bin/sh\nexit 0\n").expect("crontab");
    seal(&crontab, 0o755);
    let without_cron = tools.path().display().to_string();
    let with_cron = format!("{}:{without_cron}", cron.path().display());
    // With a `crontab` on PATH the host could schedule cron, so a cron
    // that never ran warns.
    let native = command(false, &home, &state, &[("PATH", &with_cron)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(stdout.contains("⚠ cron update has never run"), "{stdout}");
    assert!(
        stdout.contains("✓ last update succeeded (manual run 3h0m ago)"),
        "{stdout}"
    );
    assert!(!stdout.contains("success is unknown"), "{stdout}");
    // Without one (Termux, containers) the same state only skips.
    let native = command(false, &home, &state, &[("PATH", &without_cron)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains(
            "· cron update has never run (no crontab on PATH; last update: manual run 3h0m ago)"
        ),
        "{stdout}"
    );
}

#[test]
fn init_convergence_records_the_init_trigger() {
    let scope = TempDir::new("doctor-init-trigger-origin").expect("origin scope");
    let home = TempDir::new("doctor-init-trigger-home").expect("home");
    let state = TempDir::new("doctor-init-trigger-state").expect("state");
    let origin = origin(scope.path());
    init_client(&home, &state, &origin);
    let stamp = std::fs::read_to_string(state.path().join("dot/update.last-run"))
        .expect("init last-run stamp");
    assert!(stamp.ends_with(" ok init\n"), "{stamp:?}");
}

#[test]
fn release_root_checkpoint_pins_its_install_metadata_end_to_end() {
    // A packaged release has no `.git`: the checkpoint must compare against
    // its install metadata, never Git's answer for some enclosing tree.
    let home = TempDir::new("doctor-release-checkpoint-home").expect("home");
    let state = TempDir::new("doctor-release-checkpoint-state").expect("state");
    let scope = TempDir::new("doctor-release-checkpoint-root").expect("release scope");
    let release = scope.path().join("dot");
    std::fs::create_dir_all(release.join("lib/dot/public")).expect("release");
    let pinned = "c".repeat(40);
    std::fs::write(
        release.join(".dot-install.json"),
        format!("{{\n  \"schema\": 1,\n  \"commit\": \"{pinned}\"\n}}\n"),
    )
    .expect("metadata");
    let record = state.path().join("dot/provider-reexec-failed");
    std::fs::create_dir_all(record.parent().expect("parent")).expect("state dir");
    std::fs::write(
        &record,
        format!(
            "cgraf78 dot provider reexec checkpoint v1\nbefore={}\nafter={pinned}\n",
            "a".repeat(40)
        ),
    )
    .expect("checkpoint");
    seal(&record, 0o600);
    // The shipped binary derives its own source root, so run in process
    // with this release as the embedded source root.
    let (_, stdout, _) = run_in_process(
        &home,
        &state,
        home.path(),
        &[("DOT_SOURCE_ROOT", release.as_os_str())],
    );
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(stdout.contains(", release)\n"), "{stdout}");
    assert!(
        stdout.contains("⚠ provider re-exec checkpoint pending"),
        "{stdout}"
    );
    assert!(!stdout.contains("blocks dot update"), "{stdout}");
}

#[test]
fn misspelled_config_key_fails_end_to_end() {
    // I5: a likely misspelled key makes every `dot update` exit 1, so
    // doctor fails on it; a key from a newer Dot stays a warning.
    let home = TempDir::new("doctor-misspelled-key-home").expect("home");
    let state = TempDir::new("doctor-misspelled-key-state").expect("state");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\ndefualt_profile=base\nfuture_key=1\n",
    )
    .expect("config");
    let native = command(false, &home, &state, &[]).output().expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("  ✗ unknown configuration key ignored\n    defualt_profile on line 2"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  ⚠ unknown configuration key ignored\n    future_key on line 3"),
        "{stdout}"
    );
}

#[test]
fn blocking_reexec_checkpoint_fails_end_to_end() {
    // M8: a provider re-exec checkpoint that does not pin the active Dot
    // makes every `dot update` exit 1; doctor used to say nothing.
    let home = TempDir::new("doctor-checkpoint-home").expect("home");
    let state = TempDir::new("doctor-checkpoint-state").expect("state");
    let record = state.path().join("dot/provider-reexec-failed");
    std::fs::create_dir_all(record.parent().expect("parent")).expect("state dir");
    std::fs::write(
        &record,
        format!(
            "cgraf78 dot provider reexec checkpoint v1\nbefore={}\nafter={}\n",
            "a".repeat(40),
            "b".repeat(40)
        ),
    )
    .expect("checkpoint");
    seal(&record, 0o600);
    let native = command(false, &home, &state, &[]).output().expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("✗ provider re-exec checkpoint blocks dot update"),
        "{stdout}"
    );
    assert!(
        stdout.contains("pins bbbbbbbbbbbb but dot is at "),
        "{stdout}"
    );
    assert!(record.exists(), "doctor must not consume the checkpoint");
}

#[test]
fn standalone_release_under_shdeps_is_reported_end_to_end() {
    // M1: a standalone install (`cgraf78/dot -> .dot-standalone/current`)
    // read green under the Shdeps provider. Shdeps adopts it on its next
    // update of Dot, unless the installer's lock blocks that.
    let home = TempDir::new("doctor-standalone-home").expect("home");
    let state = TempDir::new("doctor-standalone-state").expect("state");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\ndependency_provider=shdeps\n",
    )
    .expect("config");
    let cgraf = home.path().join(".local/share/cgraf78");
    let release = cgraf.join(".dot-standalone/releases/v1-linux");
    std::fs::create_dir_all(release.join("lib/dot/public")).expect("release");
    std::fs::write(release.join(".dot-install.json"), b"{}\n").expect("metadata");
    std::os::unix::fs::symlink("releases/v1-linux", cgraf.join(".dot-standalone/current"))
        .expect("current");
    std::os::unix::fs::symlink(".dot-standalone/current", cgraf.join("dot")).expect("root");
    let release = std::fs::canonicalize(&release).expect("release path");
    let native = command(
        false,
        &home,
        &state,
        &[("DOT_SOURCE_ROOT", release.to_str().expect("utf8 release"))],
    )
    .output()
    .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(stdout.contains("⚠ dot is standalone-installed"), "{stdout}");
    assert!(
        stdout.contains("~/.local/share/cgraf78/dot: Shdeps adopts it on its next update of dot"),
        "{stdout}"
    );
    std::fs::create_dir(cgraf.join(".dot-standalone/lock")).expect("installer lock");
    let native = command(
        false,
        &home,
        &state,
        &[("DOT_SOURCE_ROOT", release.to_str().expect("utf8 release"))],
    )
    .output()
    .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("✗ standalone installer lock blocks Shdeps adoption"),
        "{stdout}"
    );
}

/// Total minutes rendered in `last success 497206h5m ago`. Small ages
/// without an hour part (`90m`, `45s`) parse to their minute count.
fn stamp_age_minutes(stdout: &str) -> u64 {
    const PREFIX: &str = "last success ";
    const SUFFIX: &str = " ago";
    let start = stdout.find(PREFIX).expect("rendered stamp age") + PREFIX.len();
    let end = stdout[start..].find(SUFFIX).expect("age suffix") + start;
    let age = &stdout[start..end];
    let (hours, rest) = match age.find('h') {
        Some(at) => (age[..at].parse::<u64>().expect("age hours"), &age[at + 1..]),
        None => (0, age),
    };
    let minutes = match rest.find('m') {
        Some(at) => rest[..at].parse::<u64>().expect("age minutes"),
        None => 0,
    };
    hours * 60 + minutes
}

#[test]
fn unsafe_merge_inventory_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-unsafe-merge-home").expect("home");
    let state = TempDir::new("doctor-native-unsafe-merge-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("merge-hooks.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("merge directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(directory.join("10-unsafe.sh"), b"merge() { :; }\n").expect("hook");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-unsafe.sh"), 0o666);

    let (shell, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&native.stdout).contains(
            "  ✗ merge-hook extension inventory is invalid\n    ~/extensions/merge-hooks.d/10-unsafe.sh fails the extension trust checks; check its owner and mode\n"
        ),
        "{}",
        String::from_utf8_lossy(&native.stdout)
    );
    assert_pair(&shell, &native);
}

#[test]
fn earlier_malformed_merge_identity_precedes_later_unsafe_hook() {
    let home = TempDir::new("doctor-native-merge-order-home").expect("home");
    let state = TempDir::new("doctor-native-merge-order-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("merge-hooks.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("merge directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(directory.join("10-Bad.sh"), b"merge() { :; }\n").expect("malformed");
    std::fs::write(directory.join("20-unsafe.sh"), b"merge() { :; }\n").expect("unsafe");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-Bad.sh"), 0o644);
    seal(&directory.join("20-unsafe.sh"), 0o666);

    // K7: the reason lands in the row, not only on stderr.
    let (shell, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&native.stdout).contains(
            "  ✗ merge-hook extension inventory is invalid\n    ~/extensions/merge-hooks.d/10-Bad.sh has an invalid name; rename it to NN-name using lowercase letters, digits, and hyphens\n"
        ),
        "{}",
        String::from_utf8_lossy(&native.stdout)
    );
    assert!(
        native.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_pair(&shell, &native);
}

#[test]
fn profile_context_and_unsafe_lifecycle_match_without_the_old_engine() {
    let home = TempDir::new("doctor-native-profile-home").expect("home");
    let state = TempDir::new("doctor-native-profile-state").expect("state");
    let profiles = home.path().join(".config/dot/profiles.d");
    let overlays = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&profiles).expect("profiles directory");
    std::fs::create_dir_all(&overlays).expect("overlays directory");
    std::fs::write(
        profiles.join("base.conf"),
        b"version=1\noverlays=required\n",
    )
    .expect("profile");
    std::fs::write(
        overlays.join("10-required.conf"),
        b"url=https://example.invalid/required.git\n",
    )
    .expect("overlay");
    std::fs::create_dir_all(state.path().join("dot")).expect("state directory");
    std::os::unix::fs::symlink(
        state.path().join("foreign-lifecycle"),
        state.path().join("dot/profile-overlay-lifecycle-v1"),
    )
    .expect("unsafe lifecycle");

    let (shell, native) = pair(&home, &state);
    let output = String::from_utf8_lossy(&shell.stdout);
    assert!(output.contains("› profile "), "{output}");
    assert!(output.contains("; for "), "{output}");
    assert!(output.contains("profile lifecycle state unsafe"));
    assert!(output.contains("required: selected but unavailable"));
    assert_pair(&shell, &native);
}

#[test]
fn linked_worktree_overlay_matches_without_the_old_engine() {
    let scope = TempDir::new("doctor-native-linked-scope").expect("scope");
    let home = TempDir::new("doctor-native-linked-home").expect("home");
    let state = TempDir::new("doctor-native-linked-state").expect("state");
    let origin = origin(scope.path());
    let main = scope.path().join("main");
    let status = dot_test_support::git()
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&main)
        .status()
        .expect("main clone");
    assert!(status.success());
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            home.path()
                .join(".dotfiles-linked")
                .to_str()
                .expect("worktree text"),
            "origin/main",
        ],
    );
    let overlays = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&overlays).expect("overlays directory");
    std::fs::write(
        overlays.join("20-linked.conf"),
        format!("url={}\n", origin.display()),
    )
    .expect("descriptor");

    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stdout).contains("linked: cloned"));
    assert_pair(&shell, &native);
}

#[test]
fn overlay_state_ignores_inherited_git_selectors() {
    // Doctor also runs from hook contexts that export Git selectors; the
    // overlay status probe must still inspect the overlay itself.
    let scope = TempDir::new("doctor-overlay-git-env-scope").expect("scope");
    let home = TempDir::new("doctor-overlay-git-env-home").expect("home");
    let state = TempDir::new("doctor-overlay-git-env-state").expect("state");
    let origin = origin(scope.path());
    let overlay = home.path().join(".dotfiles-plain");
    let status = dot_test_support::git()
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&overlay)
        .status()
        .expect("overlay clone");
    assert!(status.success());
    let overlays = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&overlays).expect("overlays directory");
    std::fs::write(
        overlays.join("20-plain.conf"),
        format!("url={}\n", origin.display()),
    )
    .expect("descriptor");
    // A Git pre-commit hook exports `GIT_INDEX_FILE`; read against that
    // foreign index, a clean overlay looked like every file was deleted.
    let foreign = scope.path().join("foreign-index");
    let foreign = foreign.to_str().expect("utf8");
    let native = command(false, &home, &state, &[("GIT_INDEX_FILE", foreign)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    // Clean and current: the overlay folds into one row.
    assert!(
        stdout.contains("✓ plain (") && stdout.contains("main, current with origin/main)"),
        "{stdout}"
    );
    assert!(!stdout.contains("plain: 1 tracked change"), "{stdout}");
}

#[test]
fn client_state_ignores_an_inherited_git_index() {
    // The client probes must not read a Git hook's `GIT_INDEX_FILE` either.
    let scope = TempDir::new("doctor-client-git-env-origin").expect("origin scope");
    let home = TempDir::new("doctor-client-git-env-home").expect("home");
    let state = TempDir::new("doctor-client-git-env-state").expect("state");
    let origin = origin(scope.path());
    init_client(&home, &state, &origin);
    let foreign = scope.path().join("foreign-index");
    let foreign = foreign.to_str().expect("utf8");
    let native = command(false, &home, &state, &[("GIT_INDEX_FILE", foreign)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&native.stdout);
    assert!(
        stdout.contains("✓ client repository (") && !stdout.contains("tracked client change"),
        "{stdout}"
    );
}

fn latest_provider(home: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = home.path().join("provider-dev");
    let provider = root.join("shdeps");
    let managed = home.path().join("provider-managed");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&provider).expect("provider directory");
    std::fs::create_dir_all(&managed).expect("managed directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n",
    )
    .expect("config");
    std::fs::write(
        provider.join("install.sh"),
        b"case ${1:-} in --bootstrap) ;; esac\n",
    )
    .expect("installer");
    std::fs::write(provider.join("shdeps.sh"), b"# fixture library\n").expect("library");
    std::fs::write(
        provider.join("shdeps"),
        br#"#!/usr/bin/env bash
doctor_hang() {
  [[ ${DOT_TEST_PROVIDER_DOCTOR_HANG_API:-} == "$1" ]] || return 0
  printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_DOCTOR_PID"
  trap 'printf TERM >"$DOT_TEST_PROVIDER_DOCTOR_SIGNAL"; exit 0' TERM
  while :; do sleep 1; done
}
if [[ ${1:-} == __api && ${2:-} == version ]]; then
  [[ -z ${DOT_TEST_PROVIDER_API_RECORD:-} ]] || printf 'version\n' >>"$DOT_TEST_PROVIDER_API_RECORD"
  doctor_hang version
  printf '%s\n' "${DOT_TEST_PROVIDER_ABI_OUTPUT:-abi:1}"
  exit 0
fi
if [[ ${1:-} == __api && ${2:-} == capability ]]; then
  [[ -z ${DOT_TEST_PROVIDER_API_RECORD:-} ]] || printf 'capability:%s\n' "${3:-}" >>"$DOT_TEST_PROVIDER_API_RECORD"
  doctor_hang capability
  case ${3:-} in
    owned-subprocess-cancellation-v1)
      [[ ${DOT_TEST_PROVIDER_REJECT_CAPABILITY:-0} != 1 ]]
      ;;
    prompt-fifo-reader-before-event-v1)
      [[ ${DOT_TEST_PROVIDER_REJECT_PROMPT_CAPABILITY:-0} != 1 ]]
      ;;
    *) exit 2 ;;
  esac
  exit
fi
exit 2
"#,
    )
    .expect("binary");
    seal(&provider.join("shdeps"), 0o755);
    git(&provider, &["init", "-q", "-b", "main"]);
    git(&provider, &["config", "user.name", "fixture"]);
    git(
        &provider,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(
        &provider,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/cgraf78/shdeps.git",
        ],
    );
    git(&provider, &["add", "install.sh", "shdeps.sh", "shdeps"]);
    git(
        &provider,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "provider",
        ],
    );
    seal(&provider, 0o755);
    seal(&provider.join(".git"), 0o755);
    seal(&provider.join("install.sh"), 0o644);
    seal(&provider.join("shdeps.sh"), 0o644);
    (root, managed)
}

#[test]
fn healthy_latest_provider_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-provider-home").expect("home");
    let state = TempDir::new("doctor-native-provider-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let root = root.to_str().expect("provider root");
    let managed = managed.to_str().expect("managed root");
    let (shell, native) = pair_with(
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root),
            ("SHDEPS_DIR", managed),
        ],
    );
    let output = String::from_utf8_lossy(&shell.stdout);
    assert!(
        output.contains("› Shdeps provider (latest policy; trusted development checkout "),
        "{output}"
    );
    assert!(output.contains("Shdeps provider ABI (abi:1)"));
    assert_pair(&shell, &native);
}

#[test]
fn provider_without_owned_subprocess_cancellation_is_unhealthy_natively() {
    let home = TempDir::new("doctor-native-provider-capability-home").expect("home");
    let state = TempDir::new("doctor-native-provider-capability-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let native = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            ("DOT_TEST_PROVIDER_REJECT_CAPABILITY", "1"),
        ],
    )
    .output()
    .expect("native doctor");

    assert_eq!(native.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&native.stdout)
            .contains("Shdeps provider cancellation capability is unavailable"),
        "doctor stdout: {}",
        String::from_utf8_lossy(&native.stdout)
    );
}

#[test]
fn provider_without_prompt_reader_handshake_is_unhealthy_natively() {
    let home = TempDir::new("doctor-native-provider-prompt-home").expect("home");
    let state = TempDir::new("doctor-native-provider-prompt-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let native = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            ("DOT_TEST_PROVIDER_REJECT_PROMPT_CAPABILITY", "1"),
        ],
    )
    .output()
    .expect("native doctor");

    assert_eq!(native.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&native.stdout)
            .contains("Shdeps provider prompt handshake capability is unavailable"),
        "doctor stdout: {}",
        String::from_utf8_lossy(&native.stdout)
    );
}

#[test]
fn disabled_provider_does_not_execute_provider_api_natively() {
    let home = TempDir::new("doctor-native-provider-disabled-home").expect("home");
    let state = TempDir::new("doctor-native-provider-disabled-state").expect("state");
    let (root, managed) = latest_provider(&home);
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\ndependency_provider=none\nshdeps_update_policy=latest\n",
    )
    .expect("disabled provider config");
    let api_record = home.path().join("provider-api-record");
    let native = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            (
                "DOT_TEST_PROVIDER_API_RECORD",
                api_record.to_str().expect("API record path"),
            ),
        ],
    )
    .output()
    .expect("native doctor");

    assert!(String::from_utf8_lossy(&native.stdout).contains("no dependency provider configured"));
    assert!(
        !api_record.exists(),
        "disabled provider unexpectedly received an API probe"
    );
}

#[test]
fn provider_capability_is_not_probed_after_abi_mismatch() {
    let home = TempDir::new("doctor-native-provider-abi-mismatch-home").expect("home");
    let state = TempDir::new("doctor-native-provider-abi-mismatch-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let api_record = home.path().join("provider-api-record");
    let native = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            ("DOT_TEST_PROVIDER_ABI_OUTPUT", "abi:999"),
            (
                "DOT_TEST_PROVIDER_API_RECORD",
                api_record.to_str().expect("API record path"),
            ),
        ],
    )
    .output()
    .expect("native doctor");

    assert_eq!(native.status.code(), Some(1));
    assert_eq!(
        std::fs::read(&api_record).expect("API probe record"),
        b"version\n"
    );
}

#[test]
fn provider_capability_is_not_probed_after_abi_timeout() {
    let home = TempDir::new("doctor-native-provider-abi-timeout-home").expect("home");
    let state = TempDir::new("doctor-native-provider-abi-timeout-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let api_record = home.path().join("provider-api-record");
    let native = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            ("DOT_TEST_PROVIDER_DOCTOR_HANG_API", "version"),
            ("_DOT_SHDEPS_ABI_TIMEOUT_SECONDS", "1"),
            (
                "DOT_TEST_PROVIDER_API_RECORD",
                api_record.to_str().expect("API record path"),
            ),
        ],
    )
    .output()
    .expect("native doctor");

    assert_eq!(native.status.code(), Some(1));
    assert_eq!(
        std::fs::read(&api_record).expect("API probe record"),
        b"version\n"
    );
}

fn assert_doctor_provider_probe_signal(api: &str) {
    let home = TempDir::new("doctor-native-provider-probe-signal-home").expect("home");
    let state = TempDir::new("doctor-native-provider-probe-signal-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let pid_file = home.path().join("doctor-provider-pid");
    let signal_file = home.path().join("doctor-provider-signal");
    let child = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            ("DOT_TEST_PROVIDER_DOCTOR_HANG_API", api),
            (
                "DOT_TEST_PROVIDER_DOCTOR_PID",
                pid_file.to_str().expect("provider pid path"),
            ),
            (
                "DOT_TEST_PROVIDER_DOCTOR_SIGNAL",
                signal_file.to_str().expect("provider signal path"),
            ),
        ],
    )
    .spawn()
    .expect("native doctor");
    let mut child = GuardedDoctorChild::new(child);
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !pid_file.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        assert!(
            !child.exited_wnowait().expect("doctor status"),
            "doctor exited before starting its {api} probe"
        );
        assert!(
            std::time::Instant::now() < ready_deadline,
            "doctor did not start its {api} probe"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let provider_pid = std::fs::read_to_string(&pid_file)
        .expect("provider pid")
        .trim()
        .parse::<i32>()
        .expect("numeric provider pid");
    let mut provider = GuardedProbeSession::new(provider_pid);

    let delivered = child.signal(libc::SIGINT).is_ok();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.exited_wnowait().expect("doctor status") {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("doctor did not finish after interrupting {api} probe");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !provider.observe_stopped() && std::time::Instant::now() < cleanup_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let provider_survived = !provider.observe_stopped();
    if provider_survived {
        provider.force_stop();
    }
    let observed = child.reap().expect("doctor exit status").code();

    assert!(delivered, "SIGINT was not delivered to doctor");
    assert_eq!(observed, Some(130), "doctor did not preserve SIGINT status");
    assert!(!provider_survived, "doctor left the {api} probe alive");
    assert_eq!(
        std::fs::read(&signal_file).expect("provider cleanup signal"),
        b"TERM",
        "doctor did not cooperatively terminate the {api} probe"
    );
}

#[test]
fn signal_interrupts_and_reaps_doctor_provider_abi_probe() {
    assert_doctor_provider_probe_signal("version");
}

#[test]
fn signal_interrupts_and_reaps_doctor_provider_capability_probe() {
    assert_doctor_provider_probe_signal("capability");
}

#[test]
fn provider_abi_probe_does_not_evaluate_bash_env() {
    let home = TempDir::new("doctor-native-provider-bash-env-home").expect("home");
    let state = TempDir::new("doctor-native-provider-bash-env-state").expect("state");
    let (root, managed) = latest_provider(&home);
    let poison = home.path().join("bash-env");
    let marker = home.path().join("bash-env-ran");
    std::fs::write(&poison, format!("printf poison >'{}'\n", marker.display()))
        .expect("BASH_ENV poison");

    let native = command(
        false,
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root.to_str().expect("provider root")),
            ("SHDEPS_DIR", managed.to_str().expect("managed root")),
            ("BASH_ENV", poison.to_str().expect("BASH_ENV path")),
            // This test isolates the provider ABI boundary. A developer PATH
            // may contain Bash-script wrappers for Git, which are separate
            // caller-selected processes and would also evaluate BASH_ENV.
            ("PATH", "/usr/bin:/bin"),
        ],
    )
    .output()
    .expect("native doctor");

    assert!(
        String::from_utf8_lossy(&native.stdout).contains("Shdeps provider ABI (abi:1)"),
        "doctor stdout: {}",
        String::from_utf8_lossy(&native.stdout)
    );
    assert!(
        !marker.exists(),
        "doctor provider ABI probe evaluated BASH_ENV"
    );
}

#[test]
fn wrong_origin_latest_provider_matches_without_the_old_engine() {
    let home = TempDir::new("doctor-native-provider-wrong-home").expect("home");
    let state = TempDir::new("doctor-native-provider-wrong-state").expect("state");
    let (root, managed) = latest_provider(&home);
    git(
        &root.join("shdeps"),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/example/shdeps.git",
        ],
    );
    let root = root.to_str().expect("provider root");
    let managed = managed.to_str().expect("managed root");
    let (shell, native) = pair_with(
        &home,
        &state,
        &[
            ("SHDEPS_LIB", ""),
            ("SHDEPS_GIT_DEV_DIR", root),
            ("SHDEPS_DIR", managed),
        ],
    );
    assert!(String::from_utf8_lossy(&shell.stdout).contains("Shdeps development checkout ignored"));
    assert_pair(&shell, &native);
}

/// A `Write` sink that keeps every `write` call's bytes as its own chunk, so
/// tests can observe emission granularity without changing what is emitted.
struct ChunkWriter {
    chunks: Vec<Vec<u8>>,
}

impl ChunkWriter {
    fn new() -> Self {
        ChunkWriter { chunks: Vec::new() }
    }

    fn concatenated(&self) -> Vec<u8> {
        self.chunks.concat()
    }

    /// Index of the first chunk holding `needle`, if any.
    fn chunk_holding(&self, needle: &[u8]) -> Option<usize> {
        self.chunks.iter().position(|chunk| {
            chunk
                .windows(needle.len().max(1))
                .any(|window| window == needle)
        })
    }
}

impl std::io::Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.chunks.push(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn doctor_streams_result_records_before_extensions_complete() {
    // `dot doctor` must emit each result record as its check files it: the
    // core runtime rows reach stdout before any extension runs, instead of
    // the whole report rendering once at the end.
    let home = TempDir::new("doctor-streaming-home").expect("home");
    let state = TempDir::new("doctor-streaming-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-marker.sh"),
        b"doctor() {\n  printf 'ok\\tSTREAM-MARKER\\t\\n' >>\"$DOT_DOCTOR_RESULT_FILE\"\n}\n",
    )
    .expect("extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-marker.sh"), 0o644);

    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let env = BTreeMap::<OsString, OsString>::from([
        ("HOME".into(), home.path().as_os_str().to_os_string()),
        (
            "XDG_STATE_HOME".into(),
            state.path().as_os_str().to_os_string(),
        ),
        ("DOT_SOURCE_ROOT".into(), repo.as_os_str().to_os_string()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("BASH".into(), dot_test_support::bash().into()),
        ("DOT_BASH".into(), dot_test_support::bash().into()),
        ("LC_ALL".into(), "C".into()),
    ]);
    let runtime = dot::app::Runtime::from_env(&env, home.path()).expect("runtime");
    let mut stdout = ChunkWriter::new();
    let mut stderr = Vec::new();
    let code = {
        let mut streams = dot::app::Streams::with_terminal(&mut stdout, &mut stderr, false);
        dot::doctor::run(&runtime, &mut streams)
    };
    // The fixture home has no client checkout, so the completed run reports
    // its missing base repository and exits 1.
    assert_eq!(code, 1, "streamed doctor run did not complete");
    let output = stdout.concatenated();
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("dot runtime"),
        "streamed doctor omitted the runtime section: {text}"
    );
    assert!(
        text.contains("STREAM-MARKER"),
        "streamed doctor omitted the extension marker: {text}"
    );
    assert!(
        text.contains("passed \u{b7}"),
        "streamed doctor omitted the summary: {text}"
    );
    let runtime_chunk = stdout
        .chunk_holding(b"dot runtime")
        .expect("runtime section chunk");
    let marker_chunk = stdout
        .chunk_holding(b"STREAM-MARKER")
        .expect("marker chunk");
    assert_ne!(
        runtime_chunk, marker_chunk,
        "doctor buffered the report into one emission instead of streaming records"
    );
}

/// A `Write` sink that forwards the first `limit` bytes, then fails every
/// further write: the title fits, but record emission fails partway, proving
/// the run continues through a stdout delivery failure.
struct FailAfterBytes {
    forwarded: Vec<u8>,
    limit: usize,
}

impl FailAfterBytes {
    fn new(limit: usize) -> Self {
        FailAfterBytes {
            forwarded: Vec::new(),
            limit,
        }
    }
}

impl std::io::Write for FailAfterBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.forwarded.len() + bytes.len() > self.limit {
            return Err(std::io::Error::other("closed stdout"));
        }
        self.forwarded.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn doctor_runs_extensions_through_stdout_delivery_failure() {
    // A broken stdout must not abort the run: extensions still execute, later
    // stderr diagnostics are still delivered, and the exit status still
    // reports the delivery failure.
    let home = TempDir::new("doctor-failing-stdout-home").expect("home");
    let state = TempDir::new("doctor-failing-stdout-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    let hooks = root.join("merge-hooks.d");
    std::fs::create_dir_all(home.path().join(".config/dot")).expect("config directory");
    std::fs::create_dir_all(&directory).expect("doctor directory");
    std::fs::create_dir_all(&hooks).expect("hook directory");
    std::fs::write(
        home.path().join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=none\n",
    )
    .expect("config");
    std::fs::write(
        directory.join("10-marker.sh"),
        b"doctor() {\n  printf ran >\"$HOME/extension-ran\"\n  printf 'ok\\tSTREAM-MARKER\\t\\n' >>\"$DOT_DOCTOR_RESULT_FILE\"\n}\n",
    )
    .expect("extension");
    // An unsafe outputs sidecar is still reported on stderr, after the
    // first record emission has already failed.
    let hook = hooks.join("10-hook.sh");
    let unsafe_sidecar = hooks.join("10-hook.outputs");
    std::fs::write(&hook, b"merge() { :; }\n").expect("hook");
    std::fs::write(&unsafe_sidecar, b"~/out\n").expect("sidecar");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-marker.sh"), 0o644);
    seal(&hooks, 0o700);
    seal(&hook, 0o644);
    seal(&unsafe_sidecar, 0o666);

    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let env = BTreeMap::<OsString, OsString>::from([
        ("HOME".into(), home.path().as_os_str().to_os_string()),
        (
            "XDG_STATE_HOME".into(),
            state.path().as_os_str().to_os_string(),
        ),
        ("DOT_SOURCE_ROOT".into(), repo.as_os_str().to_os_string()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("BASH".into(), dot_test_support::bash().into()),
        ("DOT_BASH".into(), dot_test_support::bash().into()),
        ("LC_ALL".into(), "C".into()),
    ]);
    let runtime = dot::app::Runtime::from_env(&env, home.path()).expect("runtime");
    // The piped title is 12 bytes; the first record emission is hundreds, so
    // 64 delivers the title, then fails every record write.
    let mut stdout = FailAfterBytes::new(64);
    let mut stderr = Vec::new();
    let code = {
        let mut streams = dot::app::Streams::with_terminal(&mut stdout, &mut stderr, false);
        dot::doctor::run(&runtime, &mut streams)
    };
    assert_eq!(code, 1, "delivery failure must exit 1");
    assert!(
        home.path().join("extension-ran").is_file(),
        "doctor skipped extensions after a stdout failure"
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("dot: unsafe merge-hook outputs"),
        "doctor dropped later stderr diagnostics after a stdout failure: {}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn crash_inside_a_sourced_library_names_the_library_line() {
    let (_scope, home, state) = crash_fixture(
        "crash-lib",
        "doctor() {\n  dot_doctor_source doctor.d/lib/checks.sh\n  dot_doctor_section 'Crash'\n  check_things\n}\n",
    );
    let lib = home.path().join("extensions/doctor.d/lib");
    std::fs::create_dir(&lib).expect("library directory");
    std::fs::write(
        lib.join("checks.sh"),
        b"check_things() {\n  local probe\n  probe=$(printf ok)\n  test \"$probe\" = missing\n}\n",
    )
    .expect("library");
    seal(&lib.join("checks.sh"), 0o644);
    seal(&lib, 0o700);
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ 10-crash doctor extension failed\n    exited with status 1 at doctor.d/lib/checks.sh:4: test \"$probe\" = missing\n"
        ),
        "{stdout}"
    );
}

#[test]
fn extension_result_variable_cannot_redirect_the_failure_note() {
    // A top-level `result=` in an extension used to steer where the worker
    // wrote its note (relative to HOME); the note path is readonly now.
    let (_scope, home, state) =
        crash_fixture("crash-result", "result=stray\ndoctor() {\n  false\n}\n");
    let (output, clean) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("    exited with status 1 at doctor.d/10-crash.sh:3: false\n"),
        "{stdout}"
    );
    assert!(!home.path().join("stray.failure").exists());
    assert!(clean);
}

#[test]
fn helper_argument_count_misuse_names_the_helper() {
    let (_scope, home, state) = crash_fixture(
        "crash-arity",
        "doctor() {\n  dot_doctor_section 'Crash'\n  dot_doctor_ok one two three\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("    exited with status 2 at doctor.d/10-crash.sh:3: dot_doctor_ok\n"),
        "{stdout}"
    );
}

#[test]
fn tolerated_failure_does_not_take_the_blame_for_a_later_exit() {
    // Under `set +e` the failure trap still notes `grep`, but the extension
    // exits on line 6 with the same status; the note must not blame line 3.
    let (_scope, home, state) = crash_fixture(
        "crash-stale",
        "doctor() {\n  set +e\n  grep -q missing /dev/null\n  dot_doctor_ok 'ran'\n  set -e\n  exit 1\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("  ✗ 10-crash doctor extension failed\n    exited with status 1\n"),
        "{stdout}"
    );
}

#[test]
fn load_failure_says_the_file_did_not_load() {
    // The shipped extensions guard their support modules at the top level:
    // `dot_doctor_source ... || return`.
    let (_scope, home, state) = crash_fixture(
        "crash-load",
        "dot_doctor_source doctor.d/lib/missing.sh || return\ndoctor() { dot_doctor_ok 'unreachable'; }\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ 10-crash doctor extension failed\n    exited with status 1 while loading the extension file\n"
        ),
        "{stdout}"
    );
}

#[test]
fn extension_owned_exit_trap_degrades_to_the_status() {
    let (_scope, home, state) =
        crash_fixture("crash-trap", "doctor() {\n  trap ':' EXIT\n  false\n}\n");
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("  ✗ 10-crash doctor extension failed\n    exited with status 1\n"),
        "{stdout}"
    );
}

#[test]
fn duplicate_merge_identity_names_the_earlier_hook() {
    let (home, state, directory) = merge_fixture("merge-duplicate");
    for name in ["10-same.sh", "20-same.serial.sh"] {
        std::fs::write(directory.join(name), b"merge() { :; }\n").expect("hook");
        seal(&directory.join(name), 0o644);
    }
    let output = command(false, &home, &state, &[])
        .output()
        .expect("native doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  ✗ merge-hook extension inventory is invalid\n    ~/extensions/merge-hooks.d/20-same.serial.sh repeats identity same of 10-same.sh; remove or rename one of them\n"
        ),
        "{stdout}"
    );
}

#[test]
fn helper_argument_errors_still_return_two_inside_conditions() {
    // Where errexit is off, a misused helper must return its documented
    // status 2 without recording anything, including with no arguments.
    let (_scope, home, state) = crash_fixture(
        "arity-condition",
        "doctor() {\n  if dot_doctor_ok a b c; then dot_doctor_warn 'accepted three'; else dot_doctor_ok \"three $?\"; fi\n  dot_doctor_ok || dot_doctor_ok \"none $?\"\n  dot_doctor_item || dot_doctor_ok \"item $?\"\n}\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for row in ["  ✓ three 2\n", "  ✓ none 2\n", "  ✓ item 2\n"] {
        assert!(stdout.contains(row), "{stdout}");
    }
    assert!(!stdout.contains("accepted"), "{stdout}");
    assert!(!stdout.contains("  ✓ a (b)"), "{stdout}");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
}

#[test]
fn top_level_failure_in_the_extension_file_names_its_line() {
    let (_scope, home, state) = crash_fixture(
        "crash-top",
        "false\ndoctor() { dot_doctor_ok 'unreachable'; }\n",
    );
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("    exited with status 1 at doctor.d/10-crash.sh:1: false\n"),
        "{stdout}"
    );
}

#[test]
fn top_level_failure_in_a_support_module_names_its_line() {
    let (_scope, home, state) = crash_fixture(
        "crash-module",
        "doctor() {\n  dot_doctor_source doctor.d/lib/broken.sh\n  dot_doctor_ok 'unreachable'\n}\n",
    );
    let lib = home.path().join("extensions/doctor.d/lib");
    std::fs::create_dir(&lib).expect("library directory");
    std::fs::write(lib.join("broken.sh"), b"helper() { :; }\nfalse\n").expect("library");
    seal(&lib.join("broken.sh"), 0o644);
    seal(&lib, 0o700);
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("    exited with status 1 at doctor.d/10-crash.sh:2: dot_doctor_source\n"),
        "{stdout}"
    );
}

#[test]
fn directly_sourced_module_failure_names_the_module_line() {
    let (_scope, home, state) = crash_fixture(
        "crash-dot-module",
        "doctor() {
  . \"$DOT_EXTENSIONS_DIR/doctor.d/lib/broken.sh\"\n  dot_doctor_ok 'unreachable'\n}\n",
    );
    let lib = home.path().join("extensions/doctor.d/lib");
    std::fs::create_dir(&lib).expect("library directory");
    std::fs::write(lib.join("broken.sh"), b"helper() { :; }\nfalse\n").expect("library");
    seal(&lib.join("broken.sh"), 0o644);
    seal(&lib, 0o700);
    let (output, _) = doctor_with_env(&home, &state, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("    exited with status 1 at doctor.d/lib/broken.sh:2: false\n"),
        "{stdout}"
    );
}

/// The shell snippet for [`host_git_wrapper_running`] that holds the
/// `--version` probe (one of the first core checks) for up to 20s, until
/// `marker` exists, then records in `observed` whether it appeared.
fn version_probe_waiting_for(marker: &Path, observed: &Path) -> String {
    format!(
        "if [ \"$1\" = --version ]; then\n\
           i=0\n\
           while [ ! -e '{marker}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i + 1)); done\n\
           if [ -e '{marker}' ]; then echo concurrent; else echo serial; fi > '{observed}'\n\
         fi\n",
        marker = marker.display(),
        observed = observed.display(),
    )
}

/// P2: extensions start alongside the core checks, not after them, and their
/// rows still render after every core row.
#[test]
fn extensions_run_while_the_core_checks_run() {
    let extension = (
        "10-early.sh".to_string(),
        b"doctor() {\n  : >\"$HOME/extension-started\"\n  dot_doctor_section 'Early'\n  dot_doctor_ok 'early extension ran'\n}\n"
            .to_vec(),
    );
    let (home, state) = doctor_extension_fixture("concurrent", &[extension]);
    let (wrappers, _wrapper, _log) = host_git_wrapper_running(
        "doctor-concurrent-git",
        &version_probe_waiting_for(
            &home.path().join("extension-started"),
            &home.path().join("core-observed"),
        ),
    );
    let path = format!(
        "{}:{}",
        wrappers.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = command(false, &home, &state, &[("PATH", &path)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let observed = std::fs::read_to_string(home.path().join("core-observed")).unwrap_or_default();
    assert_eq!(observed.trim(), "concurrent", "{stdout}");
    let core = stdout.find("\ndot runtime\n").expect("core section");
    let early = stdout.find("\nEarly\n").expect("extension section");
    assert!(
        core < early,
        "extension rows render after the core: {stdout}"
    );
    assert!(stdout.contains("  ✓ early extension ran\n"), "{stdout}");
}

/// Extension output must not depend on whether the extensions finish before
/// or after the core: run a window of fast and slow extensions repeatedly and
/// compare every report with the serial one (`DOT_DOCTOR_JOBS=1`).
#[test]
fn concurrent_extension_rows_render_identically_to_serial() {
    let extensions: Vec<(String, Vec<u8>)> = (0..12)
        .map(|index| {
            // Every third extension outlasts the core checks.
            let pause = if index % 3 == 0 { "sleep 0.3\n  " } else { "" };
            (
                format!("{:02}-ext{index}.sh", 10 + index),
                format!(
                    "doctor() {{\n  {pause}dot_doctor_section 'Extension {index}'\n  dot_doctor_ok 'row a {index}'\n  dot_doctor_warn 'row b {index}' 'detail {index}'\n}}\n"
                )
                .into_bytes(),
            )
        })
        .collect();
    let (home, state) = doctor_extension_fixture("stress", &extensions);
    let run = |jobs: &str| {
        let output = command(false, &home, &state, &[("DOT_DOCTOR_JOBS", jobs)])
            .output()
            .expect("doctor");
        (
            output.status.code(),
            normalize_stamp_age(&output.stdout),
            output.stderr,
        )
    };
    let serial = run("1");
    let serial_text = String::from_utf8_lossy(&serial.1).into_owned();
    assert!(serial_text.contains("Extension 11"), "{serial_text}");
    assert_eq!(
        serial_text.matches("  ⚠ row b ").count(),
        12,
        "{serial_text}"
    );
    for round in 0..5 {
        let parallel = run("4");
        assert_eq!(
            parallel,
            serial,
            "round {round}:\n{}",
            String::from_utf8_lossy(&parallel.1)
        );
    }
}

/// Extensions now finish while the core checks still run. A finished doctor
/// extension must not clear the overlay probe answers the core reads, or the
/// core would probe every overlay again.
#[test]
fn finished_extensions_keep_the_cores_overlay_probe_answers() {
    let scope = TempDir::new("doctor-probe-cache-origin").expect("origin scope");
    let origin = origin(scope.path());
    let extension = (
        "10-quick.sh".to_string(),
        b"doctor() {\n  dot_doctor_section 'Quick'\n  dot_doctor_ok 'quick extension ran'\n  : >\"$HOME/extension-done\"\n}\n"
            .to_vec(),
    );
    let (home, state) = doctor_extension_fixture("probe-cache", &[extension]);
    let checkout = home.path().join(".dotfiles-probe");
    let status = dot_test_support::git()
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&checkout)
        .status()
        .expect("overlay clone");
    assert!(status.success());
    let descriptors = home.path().join(".config/dot/overlays.d");
    std::fs::create_dir_all(&descriptors).expect("overlays directory");
    std::fs::write(
        descriptors.join("20-probe.conf"),
        format!("url={}\n", origin.display()),
    )
    .expect("descriptor");
    // A logging Git whose `--version` probe (an early core check, before
    // the overlay rows) holds the core until the extension has finished, so
    // the extension's worker always completes while the core still runs.
    let done = home.path().join("extension-done");
    let (wrappers, _wrapper, log) = host_git_wrapper_running(
        "doctor-probe-cache-git",
        &format!(
            "if [ \"$1\" = --version ]; then\n\
               i=0\n\
               while [ ! -e '{done}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i + 1)); done\n\
               sleep 1\n\
             fi\n",
            done = done.display(),
        ),
    );
    let path = format!(
        "{}:{}",
        wrappers.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = command(false, &home, &state, &[("PATH", &path)])
        .output()
        .expect("doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The healthy overlay folds into one row (cloned, origin, current).
    assert!(
        stdout.contains("  ✓ probe (~/.dotfiles-probe, "),
        "{stdout}"
    );
    assert!(stdout.contains("quick extension ran"), "{stdout}");
    let calls = std::fs::read_to_string(&log).expect("git log");
    let probes = |needle: &str| {
        calls
            .lines()
            .filter(|line| line.contains(".dotfiles-probe") && line.contains(needle))
            .count()
    };
    assert_eq!(probes("rev-parse --show-toplevel"), 1, "{calls}");
    assert_eq!(probes("remote.origin.url"), 1, "{calls}");
}
