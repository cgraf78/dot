//! Public low-level bounded-command contracts, independent of Dot startup.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dot_test_support::TempDir;

fn boundary(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot"));
    command
        .arg("run-bounded-v1")
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin")
        // The native boundary must not inspect configuration or source roots.
        .env("DOT_SOURCE_ROOT", "/unavailable/dot-source")
        .env("DOT_REEXEC_EXPECTED_REVISION", "invalid")
        .current_dir(home);
    command
}

fn ready(path: &Path, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "boundary exited before ready"
        );
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn invalid_deadlines_never_launch_the_command() {
    let home = TempDir::new("bounded-invalid").unwrap();
    let marker = home.path().join("ran");
    for duration in [
        "",
        "-1",
        "NaN",
        "inf",
        ".5",
        "1.",
        "1e2",
        "1s",
        "1 2",
        "999999999999999999999999999999999999999999",
    ] {
        let output = boundary(home.path())
            .args([duration, "/bin/sh", "-c", ": >\"$1\"", "fixture"])
            .arg(&marker)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125), "{duration:?}: {output:?}");
        assert!(!marker.exists(), "invalid duration launched the target");
    }
    let output = boundary(home.path()).arg("1").output().unwrap();
    assert_eq!(output.status.code(), Some(125));
}

#[test]
fn zero_deadline_preserves_input_outputs_status_umask_and_extra_descriptors() {
    let home = TempDir::new("bounded-streams").unwrap();
    let extra_path = home.path().join("extra");
    let created = home.path().join("created");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "umask 027; exec 3>\"$1\"; exec 9>&3; shift; exec \"$@\"",
            "fixture",
        ])
        .arg(&extra_path)
        .arg(env!("CARGO_BIN_EXE_dot"))
        .args([
            "run-bounded-v1",
            "0.0",
            "/bin/sh",
            "-c",
            "cat; printf 3 >&3; printf 9 >&9; printf diagnostic >&2; : >\"$1\"; exit 2",
            "fixture",
        ])
        .arg(&created)
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let bytes = b"input\0bytes\nwith trailing newlines\n\n";
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(output.stdout, bytes);
    assert_eq!(output.stderr, b"diagnostic");
    assert_eq!(std::fs::read(extra_path).unwrap(), b"39");
    assert_eq!(
        std::fs::metadata(created).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn deadline_stops_term_ignoring_descendants_and_reports_124() {
    let home = TempDir::new("bounded-timeout").unwrap();
    let pid_file = home.path().join("descendant");
    let started = Instant::now();
    let output = boundary(home.path())
        .args(["1.0", "/bin/sh", "-c", "trap '' TERM; /bin/sh -c 'trap \"\" TERM; echo $$ >\"$1\"; exec sleep 30' descendant \"$1\" & wait", "leader"])
        .arg(&pid_file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(124), "{output:?}");
    assert!(started.elapsed() < Duration::from_secs(15));
    let pid = std::fs::read_to_string(pid_file).unwrap();
    let status = Command::new("/bin/ps")
        .args(["-p", pid.trim(), "-o", "stat="])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty() || status.stdout.starts_with(b"Z"),
        "descendant remains: {status:?}"
    );
}

#[test]
fn handled_signals_reach_the_owned_boundary_and_return_shell_status() {
    let home = TempDir::new("bounded-signals").unwrap();
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        let marker = home.path().join(format!("ready-{signal}"));
        let mut child = boundary(home.path())
            .args(["0", "/bin/sh", "-c", ": >\"$1\"; exec sleep 30", "fixture"])
            .arg(&marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ready(&marker, &mut child);
        // SAFETY: Child still owns this unreaped supervisor PID.
        assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(128 + signal), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn concurrent_fast_failures_keep_child_stderr_exact() {
    let home = TempDir::new("bounded-fast").unwrap();
    let barrier = std::sync::Barrier::new(6);
    std::thread::scope(|scope| {
        for _ in 0..6 {
            let barrier = &barrier;
            let home = home.path();
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..24 {
                    let output = boundary(home)
                        .args(["5", "/bin/sh", "-c", "printf child-error >&2; exit 6"])
                        .output()
                        .unwrap();
                    assert_eq!(output.status.code(), Some(6), "{output:?}");
                    assert!(output.stdout.is_empty());
                    assert_eq!(output.stderr, b"child-error");
                }
            });
        }
    });
}

#[test]
fn tiny_positive_deadlines_never_become_unbounded() {
    let home = TempDir::new("bounded-tiny").unwrap();
    for duration in ["0.0000000001", "000.0000000001"] {
        let output = boundary(home.path())
            .args([duration, "/bin/sleep", "1"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(124), "{duration}: {output:?}");
    }
    for duration in ["0", "0.0", "000.000"] {
        let output = boundary(home.path())
            .args([duration, "/bin/sh", "-c", "printf real-child; exit 125"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125), "child status: {output:?}");
        assert_eq!(output.stdout, b"real-child");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn normal_completion_cleans_a_descendant_with_detached_stdio() {
    let home = TempDir::new("bounded-normal-tree").unwrap();
    let pid_file = home.path().join("descendant");
    let output = boundary(home.path())
        .args(["0", "/bin/sh", "-c", "/bin/sh -c 'trap \"\" TERM; echo $$ >\"$1\"; exec sleep 30' descendant \"$1\" </dev/null >/dev/null 2>&1 & while [ ! -s \"$1\" ]; do :; done; exit 2", "leader"])
        .arg(&pid_file).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let pid = std::fs::read_to_string(pid_file).unwrap();
    let status = Command::new("/bin/ps")
        .args(["-p", pid.trim(), "-o", "stat="])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty() || status.stdout.starts_with(b"Z"),
        "descendant remains: {status:?}"
    );
}

#[test]
fn public_executable_and_doctor_helper_preserve_the_shell_boundary() {
    let home = TempDir::new("bounded-public").unwrap();
    let root = home.path().join("provider");
    let public = root.join("lib/dot/public/run-bounded-v1");
    std::fs::create_dir_all(public.parent().unwrap()).unwrap();
    dot_test_support::copy_dot_binary(Path::new(env!("CARGO_BIN_EXE_dot")), &root.join("dot"))
        .unwrap();
    dot_test_support::install_fixture_executable(
        &public,
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/dot/public/run-bounded-v1"))
            .unwrap(),
        0o755,
    )
    .unwrap();
    let extra = home.path().join("extra");
    let api =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/dot/public/hook-runtime-v1/doctor-api.sh");
    let output = Command::new("/bin/bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#"
          source "$1"
          declare -F dot_doctor_run_bounded >/dev/null || exit 91
          trap ':' TERM
          before=$(trap -p TERM)
          options=$-
          exec 9>"$2"
          status=0
          dot_doctor_run_bounded 0 /bin/sh -c 'printf child >&9; exit 2' || status=$?
          [ "$status" = 2 ] || exit 92
          [ "$before" = "$(trap -p TERM)" ] || exit 93
          [ "$options" = "$-" ] || exit 94
          printf caller >&9
        "#,
            "fixture",
        ])
        .arg(api)
        .arg(&extra)
        .env_clear()
        .env("HOME", home.path())
        .env("DOT_RUN_BOUNDED", &public)
        // The executable must not depend on dirname, readlink or discovery tools.
        .env("PATH", "/unavailable")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(std::fs::read(extra).unwrap(), b"childcaller");
}

#[test]
fn missing_target_keeps_the_conventional_shell_status() {
    let home = TempDir::new("bounded-missing").unwrap();
    let output = boundary(home.path())
        .arg("1")
        .arg(home.path().join("missing"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127), "{output:?}");
}

#[test]
fn nonexecutable_target_keeps_the_conventional_shell_status() {
    let home = TempDir::new("bounded-nonexec").unwrap();
    let target = home.path().join("not-executable");
    std::fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = boundary(home.path()).arg("1").arg(target).output().unwrap();
    assert_eq!(output.status.code(), Some(126), "{output:?}");
}

#[test]
fn nested_native_boundaries_keep_their_ancestor_lifetime_lease() {
    let home = TempDir::new("bounded-nested").unwrap();
    let output = boundary(home.path())
        .arg("5")
        .arg(env!("CARGO_BIN_EXE_dot"))
        .args([
            "run-bounded-v1",
            "2",
            "/bin/sh",
            "-c",
            "printf nested; exit 2",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(output.stdout, b"nested");
    assert!(output.stderr.is_empty(), "{output:?}");
}
