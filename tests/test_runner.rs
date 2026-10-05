//! Native process ownership and scheduler regressions.
#[path = "support/test_fixture.rs"]
mod fixture;
use fixture::{BOUNDED_HANG, Fixture, finish, pid_file, poll, success};
use std::fs;
use std::io::{BufRead as _, Read as _};
use std::path::Path;
use std::process::{Command, Stdio};

/// How long a test waits for Dot to exit after a cancelling signal.
///
/// Interrupt teardown is bounded by the product, not instantaneous: a one
/// second TERM grace (up to two when slow process-table walks are refunded
/// to it, with the KILL window shifted to match), a 1.5 s KILL window, up to
/// three late verification walks when process-table walks overran that
/// window, then a one second lease poll. Native walks take well under a
/// second each, so a loaded host typically finishes in a few seconds. Only
/// the pathological `ps` fallback (up to 5 s to read plus 1 s to reap per
/// walk) can approach or pass this bound. These checks prove that a signal
/// is honoured at all (a writer blocked on an unread pipe, or a supervisor
/// that never finishes, does not exit), so the bound sits above the
/// product's ceiling instead of at its typical latency, and below the signal
/// fixture's 30 s self-bound so a waited-out worker still fails.
const SIGNAL_EXIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long a test waits for a suite to publish its readiness marker.
/// Matches the fixture `poll` bound: suites start interpreters and write
/// megabytes of output before their marker, which a loaded host can delay
/// by seconds.
const READY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Wait until `child` exits or `deadline` elapses, returning the status if
/// it exited. Callers decide how to fail so each test keeps its own
/// teardown and diagnostic.
fn exit_within(
    child: &mut std::process::Child,
    deadline: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let end = std::time::Instant::now() + deadline;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if std::time::Instant::now() >= end {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

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

#[cfg(target_os = "linux")]
fn proc_start_time(pid: &str) -> Option<String> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.windows(2).rposition(|part| part == b") ")?;
    let after = std::str::from_utf8(&stat[end + 2..]).ok()?;
    // state, ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt,
    // majflt, cmajflt, utime, stime, cutime, cstime, priority, nice,
    // num_threads, itrealvalue, then starttime.
    after.split_whitespace().nth(19).map(str::to_string)
}

/// Whether `/proc/<pid>` still refers to the captured process. A zombie
/// counts as present (unreaped is the failure under test), while a changed
/// start token proves PID reuse, which is disappearance of our descendant.
#[cfg(target_os = "linux")]
fn same_proc_entry(pid: &str, start: &Option<String>) -> bool {
    match (proc_start_time(pid), start) {
        (Some(now), Some(was)) => now == *was,
        (Some(_), None) => true,
        (None, _) => false,
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
    if started.recv_timeout(READY_DEADLINE).is_ok() {
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

/// Seconds the signal-lifecycle worker and member keep running on their
/// own. Their exit must stay attributable to Dot's teardown, so this
/// exceeds the post-signal exit deadline plus the post-exit liveness check.
const SIGNAL_FIXTURE_SELF_BOUND_SECS: u64 = 30;

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
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(SIGNAL_FIXTURE_SELF_BOUND_SECS + 5);
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
    f.suite(
        "hang",
        &format!("trap '' TERM\n(trap '' TERM; echo $BASHPID >\"$HOME/descendant\"; {BOUNDED_HANG}) &\nwait"),
    );
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
            &format!("trap '' TERM\necho $$ >\"$HOME/leader\"\n{BOUNDED_HANG}"),
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
    let pid = pid_file(&f.home.join("descendant")).to_string();
    success(&finish(child));
    poll(|| !live(pid.trim()));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn native_close_fds_setsid_descendant_detaches_on_success_and_stops_on_cancellation() {
    for cancel in [false, true] {
        let f = Fixture::new();
        // On cancellation the parent stays until Dot stops it, so a starved
        // test thread cannot signal only after the suite already completed.
        let wait = if cancel {
            "; [time.sleep(1) for _ in range(300)]"
        } else {
            ""
        };
        // The descendant setsids and closes every fd. Dot must stop it on
        // cancellation, while its Python parent still ties it to the suite,
        // and must never wait for it. On success Dot deliberately leaves such
        // a detached process running (pinned by
        // `normal_exit_does_not_claim_a_detached_closed_lease_descendant` in
        // `src/cleanup.rs`). Its 60s self-bound is far past `finish`'s 25s
        // deadline, so a Dot that waited for its natural exit fails there,
        // and the post-cancellation liveness poll (15s) cannot pass by that
        // exit. A wall-clock bound on the run would also count Dot's
        // process-table walks, which a loaded host stretches by seconds.
        f.suite(
            "detached",
            &format!(
                "python3 -c 'import os,subprocess,time; p=subprocess.Popen([\"/bin/sleep\",\"60\"], start_new_session=True, close_fds=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); open(os.environ[\"HOME\"]+\"/descendant\",\"w\").write(str(p.pid)){wait}'\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\""
            ),
        );
        let child = f.command(&["-s"]).spawn().unwrap();
        let pid = pid_file(&f.home.join("descendant")).to_string();
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
        if cancel {
            poll(|| !live(pid.trim()));
        } else if live(pid.trim()) {
            // Detached on success by design; stop it so it cannot outlive
            // the test.
            let pid: i32 = pid.trim().parse().expect("numeric descendant pid");
            // SAFETY: a positive PID and a valid signal. The descendant was
            // just seen live and cannot exit on its own before its 60s
            // self-bound, so the PID still names it.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        }
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
    let pid = pid_file(&f.home.join("descendant")).to_string();
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
    f.suite("descendant", &format!("(trap '' TERM; echo $BASHPID >\"$HOME/descendant\"; {BOUNDED_HANG}) &\nuntil [[ -s $HOME/descendant ]]; do sleep 0.02; done\nprintf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\""));
    let child = f.command(&[]).spawn().unwrap();
    let pid = pid_file(&f.home.join("descendant")).to_string();
    let start = proc_start_time(pid.trim());
    success(&finish(child));
    // The orphan is init's child by the time teardown KILLs it, so only
    // init can reap the zombie; teardown certifies death, not init's reap
    // latency. Poll for /proc disappearance instead of asserting it
    // immediately (loaded hosts flake). A missing entry or a reused PID
    // both prove our descendant is gone; a lingering live process or a
    // zombie still fails after the deadline.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while same_proc_entry(pid.trim(), &start) {
        assert!(
            std::time::Instant::now() < end,
            "descendant survived as a process or zombie"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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
        &format!("echo $$ >\"$HOME/first\"; {BOUNDED_HANG}"),
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
    const WORKERS: usize = 8;
    let f = Fixture::new();
    for index in 0..WORKERS {
        f.suite(
            &format!("wait-{index}"),
            &format!("trap '' TERM\necho $$ >\"$HOME/ready-{index}\"; {BOUNDED_HANG}"),
        );
    }
    let mut child = f.command(&["-j", &WORKERS.to_string()]).spawn().unwrap();
    let mut alive: Vec<String> = (0..WORKERS)
        .map(|index| pid_file(&f.home.join(format!("ready-{index}"))).to_string())
        .collect();
    signal(child.id(), libc::SIGTERM);
    // Every worker ignores TERM, so each dies only at its KILL. Under one
    // shared grace deadline those KILLs land together; serialized teardown
    // would give each worker its own full grace first, spacing the deaths
    // at least one grace apart. Measure that spacing directly instead of the
    // total teardown latency, which also includes process-table walks a
    // loaded host can stretch by seconds without serializing anything.
    let mut first_death = None;
    let mut last_death = None;
    let end = std::time::Instant::now() + SIGNAL_EXIT_DEADLINE;
    while !alive.is_empty() && std::time::Instant::now() < end {
        let before = alive.len();
        alive.retain(|pid| live(pid));
        if alive.len() < before {
            let now = std::time::Instant::now();
            first_death.get_or_insert(now);
            last_death = Some(now);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let exited = exit_within(&mut child, SIGNAL_EXIT_DEADLINE).is_some();
    let output = finish(child);
    assert!(exited, "dot test did not finish after SIGTERM");
    assert_eq!(
        output.status.code(),
        Some(143),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(alive.is_empty(), "workers survived cancellation: {alive:?}");
    let grace = std::time::Duration::from_millis(
        u64::from(dot::cleanup::GRACE_ATTEMPTS) * dot::cleanup::GRACE_INTERVAL_MS,
    );
    // Serialized grace periods would spread the deaths over at least
    // (WORKERS - 1) graces; half of that separates the two designs with
    // margin on both sides.
    let serialized = grace * (WORKERS as u32 - 1);
    let spread = last_death.unwrap() - first_death.unwrap();
    assert!(
        spread < serialized / 2,
        "worker teardown serialized its grace periods: deaths spread over {spread:?}"
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
            &format!("trap '' HUP INT QUIT\ntrap 'printf \"%s\\n\" TERM >>\"$HOME/signal\"' TERM\n(\n  trap 'printf \"%s\\n\" TERM >>\"$HOME/member-signal\"' TERM\n  echo $BASHPID >\"$HOME/member\"\n  deadline=$((SECONDS + {SIGNAL_FIXTURE_SELF_BOUND_SECS}))\n  while ((SECONDS < deadline)); do sleep 0.05; done\n) &\nuntil [[ -s $HOME/member ]]; do sleep 0.02; done\necho $$ >\"$HOME/ready\"\ndeadline=$((SECONDS + {SIGNAL_FIXTURE_SELF_BOUND_SECS}))\nwhile ((SECONDS < deadline)); do sleep 0.05; done"),
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
        if exit_within(&mut child, SIGNAL_EXIT_DEADLINE).is_none() {
            stop_signal_fixture(&mut child, &f.home);
            panic!("test runner did not finish after signal {signal_number}");
        }
        let output = child.wait_with_output().unwrap();
        let observed = output.status.code();
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
        assert_eq!(
            observed,
            Some(code),
            "signal {signal_number} status: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
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
    if started_rx.recv_timeout(READY_DEADLINE).is_err() {
        // SAFETY: the fixture owns this positive Dot child and SIGTERM is valid.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let _ = child.wait();
        panic!("parallel replay did not start");
    }
    signal(child.id(), libc::SIGHUP);
    let observed = exit_within(&mut child, SIGNAL_EXIT_DEADLINE);
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
    let observed = exit_within(&mut child, SIGNAL_EXIT_DEADLINE);
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
    let observed = exit_within(&mut child, SIGNAL_EXIT_DEADLINE);
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
    let observed = exit_within(&mut child, SIGNAL_EXIT_DEADLINE);
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
