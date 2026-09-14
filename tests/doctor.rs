//! End-to-end parity for the native `dot doctor` coordinator.
//!
//! Rust-side invocations select the retained hook boundary explicitly through
//! `DOT_BASH`; the native engine itself remains independent of Bash.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

fn assert_pair(shell: &Output, native: &Output) {
    assert_eq!(
        native.status.code(),
        shell.status.code(),
        "native stderr: {}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(native.stdout, shell.stdout, "doctor stdout");
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

#[test]
fn empty_portable_process_status_is_not_live() {
    assert!(!portable_process_live(b""));
    assert!(!portable_process_live(b" \n"));
    assert!(!portable_process_live(b"Z+\n"));
    assert!(portable_process_live(b"S+\n"));
}

fn poll_until(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "observable condition timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn assert_doctor_signal(signal: i32, expected: i32) {
    let home = TempDir::new("doctor-native-signal-home").expect("home");
    let state = TempDir::new("doctor-native-signal-state").expect("state");
    let root = home.path().join("extensions");
    let directory = root.join("doctor.d");
    let marker = home.path().join("doctor-worker");
    let descendant_marker = home.path().join("doctor-worker-descendant");
    let delivered = home.path().join("doctor-worker-signal");
    let descendant_delivered = home.path().join("doctor-worker-descendant-signal");
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
    std::fs::write(
        directory.join("10-hang.sh"),
        b"doctor() {\n  trap 'printf \"%s\\n\" HUP >>\"$HOME/doctor-worker-signal\"' HUP\n  trap 'printf \"%s\\n\" INT >>\"$HOME/doctor-worker-signal\"' INT\n  trap 'printf \"%s\\n\" QUIT >>\"$HOME/doctor-worker-signal\"' QUIT\n  trap 'printf \"%s\\n\" TERM >>\"$HOME/doctor-worker-signal\"' TERM\n  set -m\n  (\n    trap '' HUP INT QUIT\n    trap 'printf \"%s\\n\" TERM >>\"$HOME/doctor-worker-descendant-signal\"' TERM\n    printf '%s\\n' \"$BASHPID\" >\"$HOME/doctor-worker-descendant\"\n    while :; do sleep 1; done\n  ) </dev/null >/dev/null 2>&1 &\n  printf '%s\\n' \"$BASHPID\" >\"$HOME/doctor-worker\"\n  while :; do wait || true; done\n}\n",
    )
    .expect("extension");
    std::fs::write(
        directory.join("20-later.sh"),
        b"doctor() { printf ran >\"$HOME/later-extension\"; }\n",
    )
    .expect("later extension");
    seal(&root, 0o700);
    seal(&directory, 0o700);
    seal(&directory.join("10-hang.sh"), 0o644);
    seal(&directory.join("20-later.sh"), 0o644);

    let mut child = command(
        false,
        &home,
        &state,
        &[("TMPDIR", temporary.to_str().expect("temporary path"))],
    )
    .spawn()
    .expect("doctor");
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let (worker, descendant) = loop {
        let worker = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok());
        let descendant = std::fs::read_to_string(&descendant_marker)
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok());
        if let (Some(worker), Some(descendant)) = (worker, descendant) {
            break (worker, descendant);
        }
        if std::time::Instant::now() >= ready_deadline {
            for path in [&marker, &descendant_marker] {
                if let Ok(pid) = std::fs::read_to_string(path) {
                    if let Ok(pid) = pid.trim().parse::<i32>() {
                        // SAFETY: fixture markers contain only process-group
                        // leaders created by this doctor invocation.
                        unsafe { libc::kill(-pid, libc::SIGKILL) };
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("doctor cancellation fixture did not start");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // SAFETY: the fixture owns this positive Dot child and uses a valid signal.
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while child.try_wait().expect("doctor status").is_none() {
        if std::time::Instant::now() >= deadline {
            // SAFETY: these are the two fixture-owned process identities.
            unsafe {
                libc::kill(-worker, libc::SIGKILL);
                libc::kill(-descendant, libc::SIGKILL);
                libc::kill(child.id() as i32, libc::SIGKILL);
            }
            let _ = child.wait();
            panic!("doctor did not finish after signal {signal}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("doctor output");
    let observed = output.status.code();
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while (process_live(worker) || process_live(descendant))
        && std::time::Instant::now() < cleanup_deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let worker_survived = process_live(worker);
    let descendant_survived = process_live(descendant);
    if worker_survived {
        // Keep the intentionally failing RED run from leaking the worker.
        // SAFETY: the hook worker is the leader of its owned session.
        unsafe { libc::kill(-worker, libc::SIGKILL) };
        poll_until(|| !process_live(worker));
    }
    if descendant_survived {
        // SAFETY: set -m made this fixture descendant its process-group leader.
        unsafe { libc::kill(-descendant, libc::SIGKILL) };
        poll_until(|| !process_live(descendant));
    }
    assert_eq!(
        observed,
        Some(expected),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!worker_survived, "doctor extension worker survived");
    assert!(!descendant_survived, "doctor extension descendant survived");
    assert_eq!(
        std::fs::read(&delivered).expect("delivered signal marker"),
        b"TERM\n",
        "worker received the parent signal instead of cleanup TERM"
    );
    assert_eq!(
        std::fs::read(&descendant_delivered).expect("descendant signal marker"),
        b"TERM\n",
        "escaped-group descendant did not receive exactly one cleanup TERM"
    );
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

#[test]
fn native_doctor_hup_reaps_extension() {
    assert_doctor_signal(libc::SIGHUP, 129);
}

#[test]
fn native_doctor_int_reaps_extension() {
    assert_doctor_signal(libc::SIGINT, 130);
}

#[test]
fn native_doctor_quit_reaps_extension() {
    assert_doctor_signal(libc::SIGQUIT, 131);
}

#[test]
fn native_doctor_term_reaps_extension() {
    assert_doctor_signal(libc::SIGTERM, 143);
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
    let mut child = command(false, &home, &state, &[])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .expect("doctor");
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
        // the bounded emergency fallback terminates the coordinator.
        // SAFETY: the fixture owns this positive Dot child and SIGTERM is valid.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let _ = release_tx.send(());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        panic!("doctor rendering did not start");
    }
    // SAFETY: the fixture owns this positive Dot child and SIGINT is valid.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut observed = None;
    while observed.is_none() && std::time::Instant::now() < deadline {
        observed = child.try_wait().expect("doctor status");
        if observed.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let blocked = observed.is_none();
    release_tx.send(()).unwrap();
    let status = observed.unwrap_or_else(|| child.wait().expect("doctor exit"));
    reader.join().unwrap();
    assert!(!blocked, "signal left doctor blocked on an unread stdout");
    assert_eq!(status.code(), Some(130));
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
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
    let status = Command::new("git")
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
        String::from_utf8_lossy(&shell.stdout).contains("dot checkout exists"),
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
    let status = Command::new("git")
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
    assert!(output.contains("dot checkout exists"));
    assert!(output.contains("client Git directory exists"));
    assert!(output.contains("client upstream"));
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
    let status = Command::new("git")
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
        output.contains("profile identity (runtime-user@"),
        "doctor output: {output}"
    );
    assert!(
        output.contains("web (agreed-match)"),
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
    assert!(String::from_utf8_lossy(&shell.stdout).contains("doctor extension discovery failed"));
    assert_pair(&shell, &native);
}

#[test]
fn first_unsafe_extension_precedes_later_malformed_identity() {
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

    let (shell, native) = pair(&home, &state);
    assert!(String::from_utf8_lossy(&shell.stderr).contains("unsafe doctor extension"));
    assert_pair(&shell, &native);
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
    assert!(String::from_utf8_lossy(&shell.stdout).contains("extension inventory is invalid"));
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

    let (shell, native) = pair(&home, &state);
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("invalid merge-hook identity: 10-Bad.sh")
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
    assert!(output.contains("profile identity"));
    assert!(output.contains("selected profile"));
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
    let status = Command::new("git")
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
        b"#!/usr/bin/env bash\nif [[ ${1:-} == __api && ${2:-} == version ]]; then printf 'abi:1\\n'; exit 0; fi\nexit 2\n",
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
    assert!(output.contains("Shdeps provider source (trusted development checkout:"));
    assert!(output.contains("Shdeps provider ABI (abi:1)"));
    assert_pair(&shell, &native);
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
