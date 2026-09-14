//! Native process ownership and scheduler regressions.
#[path = "support/test_fixture.rs"]
mod fixture;
use fixture::{Fixture, finish, poll, success};
use std::fs;
use std::io::{BufRead as _, Read as _};
use std::path::Path;
use std::process::{Command, Stdio};

fn pid_marker(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    value
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .map(|_| value.to_string())
}

fn live(pid: &str) -> bool {
    let Ok(pid) = pid.parse::<i32>() else {
        return false;
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Ok(stat) = fs::read(format!("/proc/{pid}/stat")) {
        let Some(end) = stat.windows(2).rposition(|part| part == b") ") else {
            return false;
        };
        return stat.get(end + 2) != Some(&b'Z');
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // `/bin/ps` is the OS process-table interface on macOS and the BSDs;
        // do not let a caller-controlled PATH turn a missing observer into a
        // false cleanup success.
        let output = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap_or_else(|error| panic!("could not inspect process {pid}: {error}"));
        if output.status.success() {
            return !output.stdout.is_empty()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z');
        }
        // SAFETY: a positive PID and signal zero only test existence.
        if unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return false;
        }
        panic!(
            "ps could not inspect live process {pid}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // SAFETY: a positive PID and signal zero only test process existence.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn signal(pid: u32, signal: i32) {
    // SAFETY: the fixture owns this positive child PID and uses valid signals.
    assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0);
}

fn await_output_start(
    started: &std::sync::mpsc::Receiver<()>,
    release: &std::sync::mpsc::Sender<()>,
    child: &mut std::process::Child,
    label: &str,
) {
    if started
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok()
    {
        return;
    }
    // Give the command's own signal path a chance to reap any suite session,
    // then bound the emergency fallback so a failing test cannot leak it.
    // SAFETY: the fixture owns this positive Dot child and SIGTERM is valid.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = release.send(());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    panic!("{label} did not start");
}

fn stop_signal_fixture(child: &mut std::process::Child, home: &Path) {
    // SAFETY: the fixture owns this positive Dot child and SIGTERM is valid.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    // The worker and member fixtures are self-bounded. If the supervision
    // path under test fails, wait for their own deadlines rather than sending
    // to process numbers observed before Dot exited.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let leader_live = pid_marker(&home.join("ready")).is_some_and(|pid| live(&pid));
        let member_live = pid_marker(&home.join("member")).is_some_and(|pid| live(&pid));
        if !leader_live && !member_live {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn native_parallel_is_bounded_and_replays_indexed_output() {
    let f = Fixture::new();
    for name in ["alpha", "beta", "gamma"] {
        f.suite(name, &format!("touch \"$HOME/{name}-ready\"; until [[ -f $HOME/release ]]; do sleep 0.02; done\necho OUTPUT-{name}\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\""));
    }
    let child = f.command(&["-j", "2", "-v"]).spawn().unwrap();
    poll(|| f.home.join("alpha-ready").exists() && f.home.join("beta-ready").exists());
    assert!(!f.home.join("gamma-ready").exists());
    fs::write(f.home.join("release"), "").unwrap();
    let output = finish(child);
    success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.find("OUTPUT-alpha") < text.find("OUTPUT-beta"));
    assert!(text.find("OUTPUT-beta") < text.find("OUTPUT-gamma"));
    assert!(text.contains("3 passed (3 total)"));
}

#[test]
fn native_timeout_kills_term_ignoring_descendant() {
    let f = Fixture::new();
    f.suite("hang", "trap '' TERM\n(trap '' TERM; echo $BASHPID >\"$HOME/descendant\"; while :; do sleep 1; done) &\nwait");
    let child = f
        .command(&[])
        .env("DOT_TEST_SUITE_TIMEOUT_SECONDS", "0.4")
        .spawn()
        .unwrap();
    poll(|| f.home.join("descendant").exists());
    let pid = fs::read_to_string(f.home.join("descendant")).unwrap();
    let output = finish(child);
    assert_eq!(output.status.code(), Some(1));
    poll(|| !live(pid.trim()));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Failed: hang-test"));
}

#[test]
fn native_cancellation_cleans_both_modes_and_preserves_concurrent_run() {
    for args in [vec![], vec!["-s"]] {
        let f = Fixture::new();
        f.suite(
            "wait",
            "trap '' TERM\necho $$ >\"$HOME/leader\"\nwhile :; do sleep 1; done",
        );
        let child = f.command(&args).spawn().unwrap();
        poll(|| f.home.join("leader").exists());
        let pid = fs::read_to_string(f.home.join("leader")).unwrap();
        signal(child.id(), libc::SIGTERM);
        let output = finish(child);
        assert_eq!(output.status.code(), Some(143));
        poll(|| !live(pid.trim()));
        let tmp = f.scope.path().join("tmp");
        for parent in fs::read_dir(tmp).unwrap() {
            assert_eq!(fs::read_dir(parent.unwrap().path()).unwrap().count(), 0);
        }
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_success_cleans_descendant_in_separate_process_group() {
    let f = Fixture::new();
    f.suite("descendant", "python3 -c 'import os,time; os.setpgid(0,0); open(os.environ[\"HOME\"]+\"/descendant\",\"w\").write(str(os.getpid())); time.sleep(30)' &\nuntil [[ -s $HOME/descendant ]]; do sleep 0.02; done\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    let child = f.command(&["-s"]).spawn().unwrap();
    poll(|| f.home.join("descendant").is_file());
    let pid = fs::read_to_string(f.home.join("descendant")).unwrap();
    success(&finish(child));
    poll(|| !live(pid.trim()));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_tracks_close_fds_setsid_descendant_on_success_and_cancellation() {
    for cancel in [false, true] {
        let f = Fixture::new();
        let wait = if cancel { "; time.sleep(4)" } else { "" };
        f.suite(
            "detached",
            &format!(
                "python3 -c 'import os,subprocess,time; p=subprocess.Popen([\"/bin/sleep\",\"10\"], start_new_session=True, close_fds=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); open(os.environ[\"HOME\"]+\"/descendant\",\"w\").write(str(p.pid)){wait}'\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\""
            ),
        );
        let child = f.command(&["-s"]).spawn().unwrap();
        poll(|| f.home.join("descendant").is_file());
        let pid = fs::read_to_string(f.home.join("descendant")).unwrap();
        let started = std::time::Instant::now();
        let output = if cancel {
            signal(child.id(), libc::SIGTERM);
            finish(child)
        } else {
            finish(child)
        };
        assert_eq!(
            output.status.code(),
            Some(if cancel { 143 } else { 0 }),
            "close-fds descendant cleanup failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "owned close-fds descendant was allowed to reach its self-bound"
        );
        poll(|| !live(pid.trim()));
    }
}

#[test]
#[cfg(target_os = "macos")]
fn native_changed_process_group_fails_closed_without_unpinned_signal() {
    let f = Fixture::new();
    // The escaped member is deliberately self-bounded. macOS cannot retain a
    // pidfd-like identity for safe direct delivery, so the expected contract
    // is an explicit incomplete-cleanup failure, never a raw PID signal.
    f.suite("descendant", "python3 -c 'import os,time; os.setpgid(0,0); open(os.environ[\"HOME\"]+\"/descendant\",\"w\").write(str(os.getpid())); time.sleep(2)' </dev/null >/dev/null 2>&1 &\nuntil [[ -s $HOME/descendant ]]; do sleep 0.02; done\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    let child = f.command(&["-s"]).spawn().unwrap();
    poll(|| f.home.join("descendant").is_file());
    let pid = fs::read_to_string(f.home.join("descendant")).unwrap();
    let started = std::time::Instant::now();
    let output = finish(child);

    assert_eq!(
        output.status.code(),
        Some(1),
        "unsafe cleanup was reported as success: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "fail-closed cleanup exceeded its bounded fixture lifetime"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("safe descendant delivery has no stable process authority"),
        "missing explicit incomplete-cleanup diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    poll(|| !live(pid.trim()));
}

#[test]
fn native_concurrent_invocations_have_disjoint_roots() {
    let f = Fixture::new();
    f.suite(
        "core",
        "printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let children: Vec<_> = (0..8)
        .map(|_| f.command(&[]).stdin(Stdio::null()).spawn().unwrap())
        .collect();
    for child in children {
        success(&finish(child));
    }
    for parent in fs::read_dir(f.scope.path().join("tmp")).unwrap() {
        assert_eq!(fs::read_dir(parent.unwrap().path()).unwrap().count(), 0);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn native_teardown_reaps_orphaned_descendants() {
    let f = Fixture::new();
    f.suite("descendant", "(trap '' TERM; echo $BASHPID >\"$HOME/descendant\"; while :; do sleep 1; done) &\nuntil [[ -s $HOME/descendant ]]; do sleep 0.02; done\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    let child = f.command(&[]).spawn().unwrap();
    poll(|| f.home.join("descendant").is_file());
    let pid = fs::read_to_string(f.home.join("descendant")).unwrap();
    success(&finish(child));
    assert!(
        !std::path::Path::new("/proc").join(pid.trim()).exists(),
        "descendant survived as a process or zombie"
    );
}

#[test]
fn native_prunes_only_old_dead_marked_owned_roots() {
    let f = Fixture::new();
    f.suite(
        "core",
        "printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let uid = Command::new("id").arg("-u").output().unwrap();
    let parent = f.scope.path().join("tmp").join(format!(
        "dot-suite-runs.{}",
        String::from_utf8_lossy(&uid.stdout).trim()
    ));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (name, body) in [
        ("old-dead", "9999999999\t1\n".into()),
        ("recent-dead", format!("9999999999\t{now}\n")),
        ("old-live", format!("{}\t1\n", std::process::id())),
        ("malformed", "oops\t1\n".into()),
    ] {
        let root = parent.join(format!("run.{name}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".dot-suite-owner-v3"), body).unwrap();
    }
    let target = f.scope.path().join("protected");
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, parent.join("run.link")).unwrap();
    success(&f.run(&[]));
    assert!(!parent.join("run.old-dead").exists());
    for name in ["recent-dead", "old-live", "malformed", "link"] {
        assert!(parent.join(format!("run.{name}")).exists());
    }
    assert!(target.is_dir());
}

#[test]
fn native_cancellation_does_not_stop_another_invocation() {
    let f = Fixture::new();
    f.suite(
        "first",
        "echo $$ >\"$HOME/first\"; while :; do sleep 1; done",
    );
    f.suite("second", "echo $$ >\"$HOME/second\"; until [[ -e $HOME/release ]]; do sleep 0.02; done; printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"");
    let first = f.command(&["first"]).spawn().unwrap();
    let second = f.command(&["second"]).spawn().unwrap();
    poll(|| f.home.join("first").exists() && f.home.join("second").exists());
    signal(first.id(), libc::SIGTERM);
    assert_eq!(finish(first).status.code(), Some(143));
    let pid = fs::read_to_string(f.home.join("second")).unwrap();
    assert!(live(pid.trim()));
    fs::write(f.home.join("release"), "").unwrap();
    success(&finish(second));
}

#[test]
fn native_parallel_cancellation_has_one_shared_grace_deadline() {
    let f = Fixture::new();
    for index in 0..8 {
        f.suite(
            &format!("wait-{index}"),
            &format!("trap '' TERM\ntouch \"$HOME/ready-{index}\"; while :; do sleep 1; done"),
        );
    }
    let child = f.command(&["-j", "8"]).spawn().unwrap();
    poll(|| (0..8).all(|index| f.home.join(format!("ready-{index}")).exists()));
    let started = std::time::Instant::now();
    signal(child.id(), libc::SIGTERM);
    let output = finish(child);
    assert_eq!(
        output.status.code(),
        Some(143),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "worker teardown serialized its grace periods"
    );
}

#[test]
fn native_cancellation_preserves_signal_status_and_reaps_worker() {
    for (signal_number, code) in [
        (libc::SIGHUP, 129),
        (libc::SIGINT, 130),
        (libc::SIGQUIT, 131),
        (libc::SIGTERM, 143),
    ] {
        let f = Fixture::new();
        f.suite(
            "wait",
            "trap '' HUP INT QUIT\ntrap 'printf \"%s\\n\" TERM >>\"$HOME/signal\"' TERM\n(\n  trap 'printf \"%s\\n\" TERM >>\"$HOME/member-signal\"' TERM\n  echo $BASHPID >\"$HOME/member\"\n  deadline=$((SECONDS + 8))\n  while ((SECONDS < deadline)); do sleep 0.05; done\n) &\nuntil [[ -s $HOME/member ]]; do sleep 0.02; done\necho $$ >\"$HOME/ready\"\ndeadline=$((SECONDS + 8))\nwhile ((SECONDS < deadline)); do sleep 0.05; done",
        );
        let mut child = f.command(&[]).spawn().unwrap();
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let (pid, member) = loop {
            let pids = pid_marker(&f.home.join("ready")).zip(pid_marker(&f.home.join("member")));
            if let Some(pids) = pids {
                break pids;
            }
            if std::time::Instant::now() >= ready_deadline {
                stop_signal_fixture(&mut child, &f.home);
                panic!("signal lifecycle fixture did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        signal(child.id(), signal_number);
        let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= exit_deadline {
                stop_signal_fixture(&mut child, &f.home);
                panic!("test runner did not finish after signal {signal_number}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let observed = child.wait_with_output().unwrap().status.code();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (live(pid.trim()) || live(member.trim())) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let worker_survived = live(pid.trim());
        let member_survived = live(member.trim());
        if worker_survived || member_survived {
            let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while (live(pid.trim()) || live(member.trim()))
                && std::time::Instant::now() < cleanup_deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert_eq!(observed, Some(code));
        assert!(!worker_survived, "test worker survived cancellation");
        assert!(!member_survived, "test worker member survived cancellation");
        for (path, label) in [
            (f.home.join("signal"), "test worker"),
            (f.home.join("member-signal"), "same-group member"),
        ] {
            let delivered = fs::read_to_string(path).expect("delivered signal marker");
            assert!(
                !delivered.is_empty() && delivered.lines().all(|line| line == "TERM"),
                "{label} received an unexpected cleanup signal sequence: {delivered:?}"
            );
        }
    }
}

#[test]
fn signal_during_parallel_replay_owns_final_status() {
    let f = Fixture::new();
    f.suite(
        "replay",
        "printf 'REPLAY-START\\n'\npython3 - <<'PY'\nimport sys\nsys.stdout.write('x' * (8 * 1024 * 1024))\nPY\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let mut child = f
        .command(&["-j", "1", "-v"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().expect("test stdout");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "missing replay marker"
            );
            if line == "REPLAY-START\n" {
                break;
            }
        }
        started_tx.send(()).unwrap();
        let mut remainder = Vec::new();
        reader.read_to_end(&mut remainder).unwrap();
    });
    if started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_err()
    {
        // SAFETY: the fixture owns this positive Dot child and SIGTERM is valid.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let _ = child.wait();
        panic!("parallel replay did not start");
    }
    signal(child.id(), libc::SIGHUP);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut observed = None;
    while observed.is_none() && std::time::Instant::now() < deadline {
        observed = child.try_wait().unwrap();
        if observed.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    if observed.is_none() {
        let _ = child.kill();
    }
    let status = observed.unwrap_or_else(|| child.wait().unwrap());
    reader.join().unwrap();
    assert_eq!(status.code(), Some(129));
}

#[test]
fn signal_interrupts_backpressured_parallel_replay() {
    let f = Fixture::new();
    f.suite(
        "backpressure",
        "printf 'REPLAY-BLOCKED\\n'\npython3 - <<'PY'\nimport sys\nsys.stdout.write('x' * (8 * 1024 * 1024))\nPY\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut child = f
        .command(&["-j", "1", "-v"])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "missing replay marker"
            );
            if line == "REPLAY-BLOCKED\n" {
                break;
            }
        }
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let mut remainder = Vec::new();
        reader.read_to_end(&mut remainder).unwrap();
    });
    await_output_start(&started_rx, &release_tx, &mut child, "parallel replay");
    signal(child.id(), libc::SIGQUIT);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut observed = None;
    while observed.is_none() && std::time::Instant::now() < deadline {
        observed = child.try_wait().unwrap();
        if observed.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let blocked = observed.is_none();
    release_tx.send(()).unwrap();
    let status = observed.unwrap_or_else(|| child.wait().unwrap());
    reader.join().unwrap();
    assert!(!blocked, "signal left replay blocked on an unread stdout");
    assert_eq!(status.code(), Some(131));
}

#[test]
fn signal_interrupts_backpressured_result_rendering() {
    let f = Fixture::new();
    f.suite(
        "skip-backpressure",
        "python3 - <<'PY'\nimport os\nwith open(os.environ['DOT_TEST_RESULT_FILE'], 'wb') as result:\n    result.write(b'skip\\t' + b'x' * (8 * 1024 * 1024) + b'\\t\\n')\nPY",
    );
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut child = f
        .command(&["-j", "1"])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut observed = Vec::new();
        loop {
            let mut byte = [0];
            assert_ne!(reader.read(&mut byte).unwrap(), 0, "missing result marker");
            observed.push(byte[0]);
            if observed.ends_with(b"skip-backpressure-test") {
                break;
            }
        }
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        reader.read_to_end(&mut observed).unwrap();
    });
    await_output_start(
        &started_rx,
        &release_tx,
        &mut child,
        "skip result rendering",
    );
    signal(child.id(), libc::SIGINT);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut observed = None;
    while observed.is_none() && std::time::Instant::now() < deadline {
        observed = child.try_wait().unwrap();
        if observed.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let blocked = observed.is_none();
    release_tx.send(()).unwrap();
    let status = observed.unwrap_or_else(|| child.wait().unwrap());
    reader.join().unwrap();
    assert!(
        !blocked,
        "signal left result rendering blocked on an unread stdout"
    );
    assert_eq!(status.code(), Some(130));
}

#[test]
fn signal_interrupts_backpressured_sequential_output() {
    let f = Fixture::new();
    f.suite(
        "sequential-backpressure",
        "printf 'SEQUENTIAL-BLOCKED\\n'\npython3 - <<'PY'\nimport sys\nsys.stdout.write('x' * (8 * 1024 * 1024))\nPY\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut child = f
        .command(&["-s"])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "missing output marker"
            );
            if line == "SEQUENTIAL-BLOCKED\n" {
                break;
            }
        }
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let mut remainder = Vec::new();
        reader.read_to_end(&mut remainder).unwrap();
    });
    await_output_start(&started_rx, &release_tx, &mut child, "sequential output");
    signal(child.id(), libc::SIGTERM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut observed = None;
    while observed.is_none() && std::time::Instant::now() < deadline {
        observed = child.try_wait().unwrap();
        if observed.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let blocked = observed.is_none();
    release_tx.send(()).unwrap();
    let status = observed.unwrap_or_else(|| child.wait().unwrap());
    reader.join().unwrap();
    assert!(
        !blocked,
        "signal left sequential output blocked on an unread stdout"
    );
    assert_eq!(status.code(), Some(143));
}
