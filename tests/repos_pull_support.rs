//! Direct behavioral tests for native repository-pull support primitives.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(unix)]
use std::os::fd::FromRawFd as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use dot::progress_ui::Palette;
use dot::repos_base::{Base, Topology};
use dot::repos_pull_support::{
    OriginMismatch, PullTally, backup_dir, conflicts_from_log, origin_mismatch, overlay_active,
    overlay_count, prepare_base_upstream, prepare_overlay_upstream, pull_cmd, record_status,
    result_prefix, shell_quote,
};
use dot_test_support::TempDir;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?} in {}", cwd.display());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn lonely_repo(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    git(dir.path(), &["init", "--quiet", "-b", "main", name]);
    git(&path, &["config", "user.email", "t@t"]);
    git(&path, &["config", "user.name", "t"]);
    std::fs::write(path.join("file"), "hi\n").expect("fixture file");
    git(&path, &["add", "file"]);
    git(&path, &["commit", "--quiet", "-m", "init"]);
    path
}

fn pushed_clone(dir: &TempDir, remote: &Path, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    git(
        dir.path(),
        &["clone", "--quiet", &remote.to_string_lossy(), name],
    );
    git(&path, &["config", "user.email", "t@t"]);
    git(&path, &["config", "user.name", "t"]);
    std::fs::write(path.join("file"), "hi\n").expect("fixture file");
    git(&path, &["add", "file"]);
    git(&path, &["commit", "--quiet", "-m", "init"]);
    git(&path, &["push", "--quiet", "-u", "origin", "HEAD"]);
    path
}

#[test]
fn conflict_parser_stops_at_the_first_non_file_line() {
    assert!(conflicts_from_log("Already up to date.\n").is_empty());
    assert!(conflicts_from_log("error\n  not-after-marker\n").is_empty());
    assert_eq!(
        conflicts_from_log(
            "untracked working tree files would be overwritten by merge:\n\ta.txt\n  b.txt\nstop\n\tc.txt\n"
        ),
        ["a.txt", "b.txt"]
    );
    assert_eq!(
        conflicts_from_log(
            "untracked working tree files would be overwritten by checkout:\n\ta.txt\n   \nlate.txt\n"
        ),
        ["a.txt"]
    );
}

#[test]
fn backup_dir_creates_a_timestamped_leaf_and_fails_closed() {
    let dir = TempDir::new("pull-backup").expect("fixture dir");
    let mut warnings = Vec::new();
    let backup = backup_dir(&dir.path().to_string_lossy(), &mut warnings).expect("backup dir");
    assert!(backup.is_dir());
    assert_eq!(
        backup.parent(),
        Some(dir.path().join(".dot-backup/pull").as_path())
    );
    let leaf = backup
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    assert_eq!(leaf.len(), 14);
    assert!(leaf.bytes().all(|byte| byte.is_ascii_digit()));
    assert!(warnings.is_empty());

    let blocked = TempDir::new("pull-backup-blocked").expect("fixture dir");
    std::fs::write(blocked.path().join(".dot-backup"), b"blocker\n").expect("blocker");
    warnings.clear();
    assert!(backup_dir(&blocked.path().to_string_lossy(), &mut warnings).is_none());
    assert!(!warnings.is_empty(), "mkdir diagnostic is forwarded");
}

#[test]
fn pull_cmd_propagates_status_and_missing_program() {
    assert_eq!(pull_cmd(false, "sh", &["-c", "exit 7"]), 7);
    assert_eq!(pull_cmd(false, "/definitely/missing/dot-command", &[]), 127);
}

#[cfg(unix)]
#[test]
fn pull_cmd_pins_locale_and_appends_quiet() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("pull-command-env").expect("fixture dir");
    let script = dir.path().join("probe.sh");
    let record = dir.path().join("record");
    std::fs::write(
        &script,
        "#!/bin/sh\nout=$1\nprintf 'lc=%s\\n' \"$LC_ALL\" >\"$out\"\nshift\nprintf 'args=%s\\n' \"$*\" >\"$out.unused\"\n",
    )
    .expect("probe");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let script = script.to_string_lossy();
    let record_text = record.to_string_lossy();
    assert_eq!(pull_cmd(true, "sh", &[&script, &record_text]), 0);
    assert_eq!(std::fs::read_to_string(&record).expect("record"), "lc=C\n");

    // The probe shifts the record path before writing the remaining
    // arguments, so its sibling captures the appended quiet flag.
    assert_eq!(
        std::fs::read_to_string(format!("{}.unused", record.display())).expect("argv"),
        "args=--quiet\n"
    );
}

#[cfg(unix)]
#[test]
fn pull_cmd_keeps_the_callers_foreground_controlling_tty() {
    use std::os::unix::fs::PermissionsExt as _;

    const HELPER: &str = "DOT_PULL_CMD_PTY_HELPER";
    if std::env::var_os(HELPER).is_some() {
        let program = std::env::var("DOT_TEST_PULL_PROGRAM").expect("pull fixture program");
        assert_eq!(pull_cmd(false, &program, &["pull"]), 0);
        return;
    }

    let scope = TempDir::new("pull-command-pty").expect("fixture dir");
    let observed = scope.path().join("foreground-tty");
    let program = scope.path().join("pull-program");
    std::fs::write(
        &program,
        "#!/bin/sh\n/usr/bin/python3 -c 'import os,sys; sys.exit(0 if all(os.isatty(fd) and os.tcgetpgrp(fd) == os.getpgrp() for fd in (0,1,2)) else 9)' || exit $?\n: >\"$DOT_TEST_PULL_PTY_OBSERVED\"\n",
    )
    .expect("pull fixture");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("pull fixture mode");

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
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args([
            "--exact",
            "pull_cmd_keeps_the_callers_foreground_controlling_tty",
            "--nocapture",
        ])
        .env(HELPER, "1")
        .env("DOT_TEST_PULL_PROGRAM", &program)
        .env("DOT_TEST_PULL_PTY_OBSERVED", &observed)
        .stdin(Stdio::from(slave.try_clone().expect("PTY stdin")))
        .stdout(Stdio::from(slave.try_clone().expect("PTY stdout")))
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
    let mut child = command.spawn().expect("PTY pull helper");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let status = loop {
        if let Some(status) = child.try_wait().expect("observe PTY pull helper") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "PTY pull helper did not stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success(), "PTY pull helper failed with {status:?}");
    assert!(
        observed.exists(),
        "streaming pull did not retain foreground TTY access"
    );
}

#[test]
fn prefixes_and_status_tallies_cover_every_status() {
    assert_eq!(result_prefix("out", 0), "out/000");
    assert_eq!(result_prefix("out", 7), "out/007");
    assert_eq!(result_prefix("out", 1234), "out/1234");
    let mut tally = PullTally::default();
    assert_eq!(record_status("empty", "", &mut tally), None);
    for (name, status) in [
        ("bad", "failed"),
        ("moved", "changed"),
        ("new", "cloned"),
        ("off", "skipped"),
        ("same", "current"),
        ("odd", "unknown"),
    ] {
        assert_eq!(
            record_status(name, status, &mut tally),
            Some(format!("{name} {status}"))
        );
    }
    assert_eq!(
        (tally.failed, tally.changed, tally.skipped, tally.current),
        (1, 2, 1, 1)
    );
    assert_eq!(
        tally.changed_items,
        "moved dotfiles updated\nnew dotfiles cloned\n"
    );
}

#[test]
fn overlay_filter_requires_git_sync_and_an_active_checkout() {
    let dir = TempDir::new("pull-active").expect("fixture dir");
    let worktree = dir.path().join("worktree");
    let plain = dir.path().join("plain");
    git(dir.path(), &["init", "--quiet", "worktree"]);
    std::fs::create_dir(&plain).expect("plain dir");
    assert!(overlay_active(&worktree, ""));
    assert!(overlay_active(&plain, "https://example.invalid/repo"));
    assert!(!overlay_active(&plain, ""));
    let entries = [
        format!("worktree|{}||||git", worktree.display()),
        format!(
            "configured|{}|https://example.invalid/repo|||git",
            plain.display()
        ),
        format!("inactive|{}||||git", plain.display()),
        format!(
            "disabled|{}|https://example.invalid/repo|||none",
            plain.display()
        ),
        format!(
            "default|{}|https://example.invalid/repo|||",
            plain.display()
        ),
        format!(
            "surplus|{}|https://example.invalid/repo|||git|extra",
            plain.display()
        ),
    ];
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    assert_eq!(overlay_count(&refs), 3);
}

#[test]
fn upstream_preparation_covers_success_and_failure_classes() {
    let dir = TempDir::new("pull-upstream").expect("fixture dir");
    git(dir.path(), &["init", "--quiet", "--bare", "remote.git"]);
    let remote = dir.path().join("remote.git");
    let pushed = pushed_clone(&dir, &remote, "pushed");
    let expected = git(&pushed, &["rev-parse", "HEAD"]);
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: pushed.to_string_lossy().into_owned(),
    };
    assert_eq!(prepare_base_upstream(&base), Ok(expected.clone()));
    assert_eq!(prepare_overlay_upstream(&pushed, true), Ok(expected));

    let lonely = lonely_repo(&dir, "lonely");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: lonely.to_string_lossy().into_owned(),
    };
    assert_eq!(prepare_base_upstream(&base), Err(1));
    assert_eq!(prepare_overlay_upstream(&lonely, true), Err(1));
    git(
        &lonely,
        &["remote", "add", "origin", "/definitely/missing/dot.git"],
    );
    git(&lonely, &["config", "branch.main.remote", "origin"]);
    git(&lonely, &["config", "branch.main.merge", "refs/heads/main"]);
    let head = git(&lonely, &["rev-parse", "HEAD"]);
    git(&lonely, &["update-ref", "refs/remotes/origin/main", &head]);
    assert_eq!(prepare_base_upstream(&base), Err(2));
    assert_eq!(prepare_overlay_upstream(&lonely, true), Err(2));
    let missing = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: dir.path().join("missing").to_string_lossy().into_owned(),
    };
    assert_eq!(prepare_base_upstream(&missing), Err(1));
}

#[test]
fn shell_quote_handles_safe_printable_control_and_non_utf8_bytes() {
    for (input, expected) in [
        (b"".as_slice(), "''"),
        (b"abc/def".as_slice(), "abc/def"),
        (b"a b".as_slice(), "a\\ b"),
        (b"a'b".as_slice(), "a\\'b"),
        (b"a\nb".as_slice(), "$'a\\nb'"),
        (b"\xff".as_slice(), "$'\\377'"),
    ] {
        assert_eq!(shell_quote(input), expected);
    }
}

fn palette() -> Palette {
    Palette {
        reset: "<R>".into(),
        bold: String::new(),
        dim: String::new(),
        green: String::new(),
        yellow: "<Y>".into(),
        red: String::new(),
        blue: String::new(),
        cyan: String::new(),
        white: String::new(),
    }
}

#[test]
fn origin_mismatch_chooses_warning_channel_and_adoption_command() {
    let details = OriginMismatch {
        name: "my overlay",
        path: "/tmp/my overlay",
        expected: "weird \"url\"",
        actual: "<missing>",
        ui_total: None,
        quiet: None,
    };
    let (out, err, live) = origin_mismatch(&palette(), true, false, &details);
    assert!(out.is_empty());
    let err = String::from_utf8(err).expect("utf8 warning");
    assert!(err.contains("my overlay overlay origin does not match"));
    assert!(err.contains("git -C /tmp/my\\ overlay remote add origin weird\\ \\\"url\\\""));
    assert!(live);
    let counted = OriginMismatch {
        ui_total: Some("1"),
        quiet: None,
        actual: "<multiple origin URLs>",
        ..details
    };
    let (out, err, live) = origin_mismatch(&palette(), true, false, &counted);
    let out = String::from_utf8(out).expect("utf8 status");
    assert!(err.is_empty());
    assert!(out.contains("overlay origin mismatch"));
    assert!(out.contains("config --replace-all remote.origin.url"));
    assert!(!live);
}
