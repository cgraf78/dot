//! Native process ownership and scheduler regressions.
#[path = "support/test_fixture.rs"]
mod fixture;
use fixture::{Fixture, finish, poll, success};
use std::fs;
use std::process::{Command, Stdio};

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
        return Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()
            .is_some_and(|output| {
                output.status.success()
                    && !output.stdout.is_empty()
                    && !String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .starts_with('Z')
            });
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
    assert_eq!(finish(child).status.code(), Some(143));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "worker teardown serialized its grace periods"
    );
}

#[test]
fn native_cancellation_preserves_hup_and_int_status() {
    for (signal_number, code) in [(libc::SIGHUP, 129), (libc::SIGINT, 130)] {
        let f = Fixture::new();
        f.suite(
            "wait",
            "echo $$ >\"$HOME/ready\"; while :; do sleep 1; done",
        );
        let child = f.command(&[]).spawn().unwrap();
        poll(|| f.home.join("ready").exists());
        let pid = fs::read_to_string(f.home.join("ready")).unwrap();
        signal(child.id(), signal_number);
        assert_eq!(finish(child).status.code(), Some(code));
        poll(|| !live(pid.trim()));
    }
}
