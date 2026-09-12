//! Native contracts for repository iteration, Git dispatch, and fetch cleanup.
use dot::repos_base::{Base, RepoKind, Topology};
use dot_test_support::TempDir;
use std::ffi::OsString;
use std::os::fd::FromRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

mod repos_git {
    pub use dot::repos_git::each_existing;

    use dot::repos_base::{Base, RepoKind};
    use std::ffi::OsString;

    fn quiet(op: &str, base: &Base, kind: RepoKind, path: &str, args: &[&str], mask: u32) -> i32 {
        let topology = match base.topology {
            dot::repos_base::Topology::Missing => "missing",
            dot::repos_base::Topology::Ordinary => "ordinary",
            dot::repos_base::Topology::Separate => "separate",
        };
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "repos_git_stream_child", "--nocapture"])
            .env("DOT_REPOS_GIT_CHILD", op)
            .env("DOT_REPOS_GIT_TOPOLOGY", topology)
            .env("DOT_REPOS_GIT_DIR", &base.client_git_dir)
            .env("DOT_REPOS_GIT_HOME", &base.home)
            .env(
                "DOT_REPOS_GIT_KIND",
                if kind == RepoKind::Base {
                    "base"
                } else {
                    "overlay"
                },
            )
            .env("DOT_REPOS_GIT_PATH", path)
            .env("DOT_REPOS_GIT_ARGS", args.join("\x1f"))
            .env("DOT_REPOS_GIT_MASK", mask.to_string())
            .output()
            .expect("run isolated streaming call");
        assert!(
            output.status.success(),
            "child stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("DOT_RC=")?.parse().ok())
            .expect("child result marker")
    }

    pub fn repo_git(base: &Base, kind: RepoKind, path: &str, args: &[&str]) -> i32 {
        quiet("git", base, kind, path, args, 0)
    }

    pub fn run_git_streaming(prefix: &[OsString], args: &[&str]) -> i32 {
        let base = Base {
            topology: dot::repos_base::Topology::Ordinary,
            client_git_dir: String::new(),
            home: prefix.get(1).expect("-C path").to_string_lossy().into(),
        };
        quiet("stream", &base, RepoKind::Overlay, &base.home, args, 0)
    }

    pub fn repo_git_fetch(
        base: &Base,
        kind: RepoKind,
        path: &str,
        extra: &[&str],
        mask: u32,
    ) -> i32 {
        quiet("fetch", base, kind, path, extra, mask)
    }
}

#[test]
fn repos_git_stream_child() {
    let Ok(op) = std::env::var("DOT_REPOS_GIT_CHILD") else {
        return;
    };
    let topology = match std::env::var("DOT_REPOS_GIT_TOPOLOGY").unwrap().as_str() {
        "missing" => Topology::Missing,
        "separate" => Topology::Separate,
        _ => Topology::Ordinary,
    };
    let base = Base {
        topology,
        client_git_dir: std::env::var("DOT_REPOS_GIT_DIR").unwrap(),
        home: std::env::var("DOT_REPOS_GIT_HOME").unwrap(),
    };
    let kind = if std::env::var("DOT_REPOS_GIT_KIND").unwrap() == "base" {
        RepoKind::Base
    } else {
        RepoKind::Overlay
    };
    let path = std::env::var("DOT_REPOS_GIT_PATH").unwrap();
    let packed = std::env::var("DOT_REPOS_GIT_ARGS").unwrap();
    let args: Vec<&str> = if packed.is_empty() {
        vec![]
    } else {
        packed.split('\x1f').collect()
    };
    let rc = match op.as_str() {
        "fetch" => dot::repos_git::repo_git_fetch(
            &base,
            kind,
            &path,
            &args,
            std::env::var("DOT_REPOS_GIT_MASK")
                .unwrap()
                .parse()
                .unwrap(),
        ),
        "stream" => dot::repos_git::run_git_streaming(&["-C".into(), path.into()], &args),
        _ => dot::repos_git::repo_git(&base, kind, &path, &args),
    };
    println!("DOT_RC={rc}");
}

/// `pid:ppid:stat:comm` for `pid` plus every process parented to it, as a
/// single-line wedge snapshot. Best-effort: `ps` failure yields a marker.
fn ps_snapshot(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat=,comm="])
        .output();
    let Ok(output) = output else {
        return "ps-failed".to_owned();
    };
    let wanted = pid.to_string();
    let rows: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let row_pid = fields.next()?;
            let row_ppid = fields.next()?;
            let stat = fields.next()?;
            let comm = fields.next()?;
            (row_pid == wanted || row_ppid == wanted)
                .then(|| format!("{row_pid}:{row_ppid}:{stat}:{comm}"))
        })
        .collect();
    if rows.is_empty() {
        "no-rows".to_owned()
    } else {
        rows.join(" ")
    }
}

#[test]
fn streaming_git_keeps_the_callers_foreground_controlling_tty() {
    const HELPER: &str = "DOT_REPOS_GIT_PTY_HELPER";
    if std::env::var_os(HELPER).is_some() {
        assert_eq!(dot::repos_git::run_git_streaming(&[], &["push"]), 0);
        return;
    }

    let scope = TempDir::new("repos-git-pty").unwrap();
    let bin = scope.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let observed = scope.path().join("foreground-tty");
    let git = bin.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\n/usr/bin/python3 -c 'import os,sys; sys.exit(0 if all(os.isatty(fd) and os.tcgetpgrp(fd) == os.getpgrp() for fd in (0,1,2)) else 9)' || exit $?\n: >\"$DOT_TEST_GIT_PTY_OBSERVED\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty initializes both descriptors and null optional pointers
    // request the platform defaults.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                // macOS takes *mut termios/*mut winsize while Linux takes
                // *const; null_mut() satisfies both through coercion.
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: successful openpty returned uniquely owned descriptors.
    let _master = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "streaming_git_keeps_the_callers_foreground_controlling_tty",
            "--nocapture",
        ])
        .env(HELPER, "1")
        .env("PATH", &bin)
        .env("DOT_TEST_GIT_PTY_OBSERVED", &observed)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    // SAFETY: the post-fork child is single threaded and the calls establish
    // fd 0's PTY as its controlling, foreground terminal before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0
                || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0
                || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) < 0
            {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    // The helper exits in milliseconds when the runner is quiet, but it
    // spawns a Python check plus supervised PTY teardown, so a saturated
    // macOS runner needs headroom. The bound still catches true hangs.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            // Report whether the fake git ran (marker) and whether the
            // helper is even killable, so a timeout names the wedge
            // instead of just the bound. Never block here: poll the
            // reap briefly, then fail; master close HUPs strays.
            // Snapshot the helper's kernel state plus its live children
            // BEFORE signaling: a SIGKILL-proof wedge is a kernel wait
            // (uninterruptible/exiting), and the state plus whom it waits
            // on distinguishes driver, exit-teardown, and userspace spins.
            let marker_exists = observed.exists();
            let helper_pid = child.id();
            let ps_before = ps_snapshot(helper_pid);
            let _ = child.kill();
            let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut reaped = None;
            while std::time::Instant::now() < reap_deadline {
                if let Some(status) = child.try_wait().unwrap() {
                    reaped = Some(status);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let ps_after = ps_snapshot(helper_pid);
            panic!(
                "PTY Git helper did not stop (marker_exists={marker_exists}, reaped={reaped:?}, before=[{ps_before}], after=[{ps_after}])"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success(), "PTY Git helper failed with {status:?}");
    assert!(
        observed.exists(),
        "streaming Git did not retain foreground TTY access"
    );
}
fn git(path: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .arg("-C")
            .arg(path)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    )
}
fn repo(tag: &str) -> TempDir {
    let d = TempDir::new(tag).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["init", "-q"])
            .arg(d.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    d
}
fn base(t: Topology, p: &Path) -> Base {
    Base {
        topology: t,
        client_git_dir: p.to_string_lossy().into(),
        home: p.to_string_lossy().into(),
    }
}
#[test]
fn each_existing_order_and_skips() {
    let b = repo("git-each-base");
    let a = repo("git-each-a");
    let missing = b.path().join("missing");
    let empty = b.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let plain = b.path().join("plain");
    std::fs::write(&plain, b"not a directory").unwrap();
    let overlays = vec![
        format!("a|{}|url|||git", a.path().display()),
        format!("bare|{}", a.path().display()),
        format!("local|{}||||none", a.path().display()),
        format!("remainder|{}|url|||git|extra", a.path().display()),
        format!("missing|{}|url|||git", missing.display()),
        format!("empty|{}|url|||git", empty.display()),
        format!("plain|{}|url|||git", plain.display()),
        String::new(),
    ];
    let mut rows = vec![];
    let rc = repos_git::each_existing(
        &base(Topology::Ordinary, b.path()),
        &overlays,
        &b.path().to_string_lossy(),
        &["status".into()],
        &mut |kind, name, path, url, args| {
            rows.push((
                kind,
                name.to_string(),
                path.to_string(),
                url.to_string(),
                args.to_vec(),
            ));
            0
        },
    );
    assert_eq!(rc, 0);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0, RepoKind::Base);
    assert_eq!(rows[0].1, "dotfiles");
    assert_eq!(rows[1].0, RepoKind::Overlay);
    assert_eq!(rows[1].1, "a");
    assert_eq!(rows[1].3, "url");
    assert_eq!(rows[1].4, [OsString::from("status")]);
    assert_eq!(rows[2].1, "bare");
    let mut missing_rows = vec![];
    let rc = repos_git::each_existing(
        &base(Topology::Missing, b.path()),
        &overlays,
        &b.path().to_string_lossy(),
        &[],
        &mut |_, name, _, _, _| {
            missing_rows.push(name.to_string());
            0
        },
    );
    assert_eq!(rc, 0);
    assert_eq!(missing_rows, ["a", "bare"]);
}
#[test]
fn repo_git_dispatches_base_and_overlay_with_status_codes() {
    let b = repo("git-dispatch-base");
    let o = repo("git-dispatch-overlay");
    assert_eq!(
        repos_git::repo_git(
            &base(Topology::Ordinary, b.path()),
            RepoKind::Base,
            "",
            &["rev-parse", "--is-inside-work-tree"]
        ),
        0
    );
    assert_eq!(
        repos_git::repo_git(
            &base(Topology::Ordinary, b.path()),
            RepoKind::Overlay,
            &o.path().to_string_lossy(),
            &["rev-parse", "--is-inside-work-tree"]
        ),
        0
    );
    assert_eq!(
        repos_git::repo_git(
            &base(Topology::Ordinary, b.path()),
            RepoKind::Overlay,
            &o.path().to_string_lossy(),
            &["rev-parse", "--verify", "missing"]
        ),
        128
    );
}
#[test]
fn each_existing_short_circuits_exact_status() {
    let b = repo("git-short-base");
    let o = repo("git-short-overlay");
    let rows = vec![format!("o|{}|u|||git", o.path().display())];
    for fail_at in [1, 2] {
        let mut calls = 0;
        let rc = repos_git::each_existing(
            &base(Topology::Ordinary, b.path()),
            &rows,
            "/home",
            &[],
            &mut |_, _, _, _, _| {
                calls += 1;
                if calls == fail_at { 23 } else { 0 }
            },
        );
        assert_eq!(rc, 23);
        assert_eq!(calls, fail_at);
    }
}
#[test]
fn repo_git_prefix_shapes_cover_separate_and_ordinary() {
    let ord = repo("git-prefix-ordinary");
    assert_eq!(
        repos_git::repo_git(
            &base(Topology::Ordinary, ord.path()),
            RepoKind::Base,
            "",
            &["status", "--porcelain"]
        ),
        0
    );
    let home = TempDir::new("git-prefix-home").unwrap();
    let gd = TempDir::new("git-prefix-dir").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["init", "--bare", "-q"])
            .arg(gd.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let sep = Base {
        topology: Topology::Separate,
        client_git_dir: gd.path().to_string_lossy().into(),
        home: home.path().to_string_lossy().into(),
    };
    assert_eq!(
        repos_git::repo_git(&sep, RepoKind::Base, "", &["rev-parse", "--show-toplevel"]),
        0
    );
}
#[test]
fn repo_git_missing_topology_refuses_128() {
    let d = TempDir::new("git-missing").unwrap();
    assert_eq!(
        repos_git::repo_git(
            &base(Topology::Missing, d.path()),
            RepoKind::Base,
            "",
            &["status"]
        ),
        128
    );
}
#[test]
fn repo_git_quiet_failure_propagates() {
    let d = repo("git-failure");
    std::fs::write(d.path().join("dirty"), b"x").unwrap();
    git(d.path(), &["add", "dirty"]);
    for kind in [RepoKind::Base, RepoKind::Overlay] {
        assert_eq!(
            repos_git::repo_git(
                &base(Topology::Ordinary, d.path()),
                kind,
                &d.path().to_string_lossy(),
                &["diff", "--cached", "--quiet"]
            ),
            1
        );
    }
    assert_eq!(
        repos_git::run_git_streaming(&["-C".into(), d.path().join("missing").into()], &["status"]),
        128
    );
}
fn remote_pair(tag: &str) -> (TempDir, TempDir) {
    let remote = TempDir::new(&format!("{tag}-remote")).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["init", "--bare", "-q"])
            .arg(remote.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let work = repo(&format!("{tag}-work"));
    git(
        work.path(),
        &["remote", "add", "origin", &remote.path().to_string_lossy()],
    );
    std::fs::write(work.path().join("f"), b"x").unwrap();
    git(work.path(), &["add", "f"]);
    git(
        work.path(),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "seed",
        ],
    );
    git(work.path(), &["push", "-qu", "origin", "HEAD"]);
    (remote, work)
}
#[test]
fn repo_git_fetch_success_clamps_fetch_head() {
    let (_r, w) = remote_pair("git-fetch-ok");
    let head = w.path().join(".git/FETCH_HEAD");
    std::fs::write(&head, b"stale\n").unwrap();
    std::fs::set_permissions(&head, std::fs::Permissions::from_mode(0o644)).unwrap();
    let rc = repos_git::repo_git_fetch(
        &base(Topology::Ordinary, w.path()),
        RepoKind::Base,
        "",
        &[],
        0o022,
    );
    assert_eq!(rc, 0);
    assert!(head.is_file());
    assert_eq!(
        std::fs::metadata(head).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn repo_git_fetch_preserves_fetch_failure_status() {
    let (_r, d) = remote_pair("git-fetch-fail");
    let head = d.path().join(".git/FETCH_HEAD");
    std::fs::write(&head, b"stale\n").unwrap();
    std::fs::set_permissions(&head, std::fs::Permissions::from_mode(0o644)).unwrap();
    let rc = repos_git::repo_git_fetch(
        &base(Topology::Ordinary, d.path()),
        RepoKind::Base,
        "",
        &["--no-such-flag"],
        0o022,
    );
    assert_eq!(rc, 129, "git's bad-option status survives cleanup");
    assert_eq!(
        std::fs::metadata(head).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let plain = repo("git-fetch-no-remote");
    assert_eq!(
        repos_git::repo_git_fetch(
            &base(Topology::Ordinary, plain.path()),
            RepoKind::Overlay,
            &plain.path().to_string_lossy(),
            &["origin"],
            0o022,
        ),
        128
    );
}
#[test]
fn repo_git_fetch_rejects_missing_base_and_unsafe_fetch_head() {
    let d = repo("git-fetch-gates");
    assert_eq!(
        repos_git::repo_git_fetch(
            &base(Topology::Missing, d.path()),
            RepoKind::Base,
            "",
            &[],
            0o022
        ),
        1
    );
    for target in [d.path().join("target"), d.path().join("missing-target")] {
        let head = d.path().join(".git/FETCH_HEAD");
        if head.exists() || head.is_symlink() {
            std::fs::remove_file(&head).unwrap();
        }
        if target.file_name().unwrap() == "target" {
            std::fs::write(&target, b"x").unwrap();
        }
        std::os::unix::fs::symlink(&target, &head).unwrap();
        assert_eq!(
            repos_git::repo_git_fetch(
                &base(Topology::Ordinary, d.path()),
                RepoKind::Base,
                "",
                &["no-such-remote"],
                0o022
            ),
            1
        );
    }
    let gone = d.path().join("gone");
    assert_eq!(
        repos_git::repo_git_fetch(
            &base(Topology::Ordinary, d.path()),
            RepoKind::Overlay,
            &gone.to_string_lossy(),
            &["origin"],
            0o022,
        ),
        1
    );
}
