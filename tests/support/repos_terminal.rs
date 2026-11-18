//! Terminal presentation and job-control contracts for repository commands.

use super::*;

/// Run the real CLI on a controlling terminal, with bounded teardown of its
/// entire session. Python's PTY adapter avoids platform-specific test ABI code.
fn repos_terminal(
    client: &ReposClient,
    argv: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new(real_tool("python3"));
    repos_env(&mut command, client);
    command.env("TERM", "xterm-256color");
    for (key, value) in env {
        command.env(key, value);
    }
    command
        .args([
            "-c",
            r#"
import errno, fcntl, os, pty, re, select, signal, struct, subprocess, sys, termios, time
def same_terminal(left, right):
    left, right = list(left), list(right)
    if sys.platform == 'darwin':
        # XNU sets PENDIN when canonical mode is restored and preserves it
        # through tcsetattr. Compare settings, not that transient input state.
        left[3] &= ~termios.PENDIN
        right[3] &= ~termios.PENDIN
    return left == right
pid, master = pty.fork()
if pid == 0:
    # A persistent session owner reproduces an interactive shell: Dot's exit
    # must not trigger session-leader HUP and hide a surviving pager.
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    if os.environ.get('DOT_TEST_TOSTOP'):
        settings = termios.tcgetattr(0)
        settings[3] |= termios.TOSTOP
        termios.tcsetattr(0, termios.TCSANOW, settings)
    original = termios.tcgetattr(0)
    dot = os.fork()
    if dot == 0:
        os.setpgid(0, 0)
        signal.signal(signal.SIGTTOU, signal.SIG_IGN)
        os.tcsetpgrp(0, os.getpgrp())
        signal.signal(signal.SIGTTOU, signal.SIG_DFL)
        os.execv(sys.argv[1], sys.argv[1:])
    print(f'dot-pid={dot}', flush=True)
    stops = 0
    while True:
        _, status = os.waitpid(dot, os.WUNTRACED)
        if not os.WIFSTOPPED(status):
            break
        if os.environ.get('DOT_TEST_PTY_ACTION') == 'stop_background':
            stops += 1
            os.tcsetpgrp(0, os.getpgrp())
            if stops == 1:
                # A shell can change its own terminal while the pager's job
                # is stopped. A background resume must not overwrite it.
                shell = termios.tcgetattr(0)
                shell[3] ^= termios.ECHO
                termios.tcsetattr(0, termios.TCSANOW, shell)
                os.kill(dot, signal.SIGCONT)
            else:
                print('background-shell-preserved=' + str(same_terminal(termios.tcgetattr(0), shell)), flush=True)
                termios.tcsetattr(0, termios.TCSANOW, original)
                os.tcsetpgrp(0, dot)
                os.kill(dot, signal.SIGCONT)
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    os.tcsetpgrp(0, os.getpgrp())
    restored = termios.tcgetattr(0)
    if not same_terminal(restored, original):
        print('terminal-diff=' + repr([(i, before, after) for i, (before, after) in enumerate(zip(original, restored)) if before != after]), flush=True)
    print('terminal-restored=' + str(same_terminal(restored, original)), flush=True)
    code = os.waitstatus_to_exitcode(status)
    sys.exit(code if code >= 0 else 128 - code)
output = bytearray()
deadline = time.monotonic() + 15
action = os.environ.get('DOT_TEST_PTY_ACTION')
acted = False
continued = False
answered = False
groups = set()
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
try:
    while True:
        if time.monotonic() >= deadline:
            raise RuntimeError('PTY command timed out: ' + repr(bytes(output)))
        dot_match = re.search(rb'dot-pid=(\d+)', output)
        if action == 'stop' and acted and not continued and dot_match:
            dot = int(dot_match[1])
            state = subprocess.run(['ps', '-o', 'stat=', '-p', str(dot)], capture_output=True, text=True, check=False).stdout
            if 'T' in state:
                output.extend(b'dot-stopped\n')
                os.kill(dot, signal.SIGCONT)
                continued = True
        if not answered and ((action == 'interrupt_then_input' and b'pager-interrupted' in output) or (action == 'stop' and b'pager-resumed' in output) or (action == 'stop_background' and b'background-shell-preserved=' in output and b'pager-resumed' in output)):
            os.write(master, b'q')
            answered = True
        if not select.select([master], [], [], 0.05)[0]:
            continue
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not chunk:
            break
        output.extend(chunk)
        groups.update(int(m) for m in re.findall(rb'pager-pgid=(\d+)', output))
        if not acted and b'pager-ready' in output:
            acted = True
            if action == 'resize':
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 31, 101, 0, 0))
            elif action == 'input':
                os.write(master, b'q')
            elif action == 'interrupt':
                os.write(master, b'\x03')
            elif action == 'interrupt_then_input':
                os.write(master, b'\x03')
            elif action in ('terminate', 'terminate_stubborn'):
                os.kill(int(re.search(rb'dot-pid=(\d+)', output)[1]), signal.SIGTERM)
            elif action in ('stop', 'stop_background'):
                os.write(master, b'\x1a')
    while True:
        observed, status = os.waitpid(pid, os.WNOHANG)
        if observed:
            break
        if time.monotonic() >= deadline:
            raise RuntimeError('PTY command closed output without exiting')
        select.select([], [], [], 0.01)
    pid = None
    sys.stdout.buffer.write(output.replace(b'\r\n', b'\n'))
    code = os.waitstatus_to_exitcode(status)
    sys.exit(code if code >= 0 else 128 - code)
finally:
    if pid is not None:
        for group in groups | {pid}:
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        os.waitpid(pid, 0)
    os.close(master)
"#,
            env!("CARGO_BIN_EXE_dot"),
        ])
        .args(argv)
        .output()
        .expect("run repository command on PTY")
}

fn repos_config(client: &ReposClient, key: &str, value: &str) {
    repos_git_prefix(&client.base_git_dir, &client.home, &["config", key, value]);
    repos_git(&client.overlay, &["config", key, value]);
}

fn repos_dirty(client: &ReposClient) {
    for path in [&client.home, &client.overlay] {
        std::fs::write(path.join("tracked.txt"), b"v1\nv2\n").expect("dirty repository");
    }
}

#[test]
fn repos_terminal_status_uses_git_auto_color() {
    let client = stage_repos_client();
    repos_dirty(&client);
    let output = repos_terminal(&client, &["status", "--short"], &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output
            .stdout
            .windows(b"\x1b[31mM".len())
            .filter(|s| *s == b"\x1b[31mM")
            .count(),
        2,
        "both repositories retain Git's auto status color: {output:?}"
    );
}

#[test]
fn repos_terminal_diff_uses_configured_pager_after_each_header() {
    let client = stage_repos_client();
    repos_dirty(&client);
    repos_config(&client, "core.pager", "printf 'configured-pager\\n'; cat");
    let output = repos_terminal(&client, &["diff", "--color=never"], &[]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    let base = text.find("==> dotfiles").expect("base header");
    let first = text.find("configured-pager").expect("base pager");
    let overlay = text.find("==> alpha dotfiles").expect("overlay header");
    let second = text.rfind("configured-pager").expect("overlay pager");
    assert!(
        base < first && first < overlay && overlay < second,
        "{text}"
    );
    assert_eq!(
        text.matches("+v2").count(),
        2,
        "pager forwards both patches: {text}"
    );
}

#[test]
fn repos_terminal_all_git_commands_honor_pager_configuration() {
    let client = stage_repos_client();
    repos_config(&client, "core.pager", "printf 'configured-pager\\n'; cat");
    for operation in ["status", "fetch", "push"] {
        repos_config(&client, &format!("pager.{operation}"), "true");
        let output = repos_terminal(&client, &[operation], &[]);
        assert!(output.status.success(), "{operation}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .matches("configured-pager")
                .count(),
            2,
            "{operation}: {output:?}"
        );
    }
}

#[test]
fn repos_terminal_respects_disabled_pager_and_color() {
    let client = stage_repos_client();
    repos_dirty(&client);
    repos_config(&client, "core.pager", "printf 'unwanted-pager\\n'; cat");
    repos_config(&client, "pager.diff", "false");
    repos_config(&client, "color.ui", "false");
    let output = repos_terminal(&client, &["diff"], &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(!has_bytes(&output.stdout, b"unwanted-pager"));
    assert!(!has_bytes(&output.stdout, b"\x1b[31m"));
    assert!(has_bytes(&output.stdout, b"+v2\n"));
}

#[test]
fn repos_terminal_respects_git_pager_environment_override() {
    let client = stage_repos_client();
    repos_dirty(&client);
    repos_config(&client, "core.pager", "printf 'unwanted-pager\\n'; cat");
    let output = repos_terminal(
        &client,
        &["diff"],
        &[("GIT_PAGER", "printf 'override-pager\\n'; cat")],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!has_bytes(&output.stdout, b"unwanted-pager"));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("override-pager")
            .count(),
        2
    );
}

#[test]
fn repos_redirected_output_keeps_git_pager_and_auto_color_disabled() {
    let client = stage_repos_client();
    repos_dirty(&client);
    repos_config(&client, "core.pager", "printf 'unwanted-pager\\n'; cat");
    for operation in ["status", "diff"] {
        let output = repos_run(&client, &[operation]);
        assert!(output.status.success(), "{output:?}");
        assert!(!has_bytes(&output.stdout, b"unwanted-pager"));
        assert!(!has_bytes(&output.stdout, b"\x1b["));
        assert!(!output.stdout.contains(&b'\r'));
    }
}

#[test]
fn repos_terminal_preserves_git_diff_exit_code() {
    let client = stage_repos_client();
    repos_dirty(&client);
    let output = repos_terminal(&client, &["diff", "--exit-code"], &[("GIT_PAGER", "cat")]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!has_bytes(&output.stdout, b"==> alpha dotfiles"));
}

/// Configure only the base pager so one interactive test needs exactly one
/// keystroke or resize. The fixture is interpreted, never executed from /tmp.
fn repos_interactive_pager(client: &ReposClient, script: &str) {
    let path = client.scope.path().join("pager.py");
    std::fs::write(&path, script).expect("pager fixture");
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let pager = format!("{} {}", quote(&real_tool("python3")), quote(&path));
    repos_git_prefix(
        &client.base_git_dir,
        &client.home,
        &["config", "core.pager", &pager],
    );
    repos_git(&client.overlay, &["config", "pager.diff", "false"]);
    repos_dirty(client);
}

const RESIZE_PAGER: &str = r#"
import os, signal, struct, sys, termios, fcntl
def resized(signum, frame):
    rows, cols, _, _ = struct.unpack('HHHH', fcntl.ioctl(1, termios.TIOCGWINSZ, b'\0' * 8))
    # No print() in a signal handler: a signal landing mid-print re-enters
    # the buffered writer and raises RuntimeError. Write to the fd directly.
    os.write(1, f'pager-size={rows}x{cols}\n'.encode())
    if (rows, cols) == (31, 101):
        sys.exit(0)
signal.signal(signal.SIGWINCH, resized)
print(f'pager-ready pid={os.getpid()} pager-pgid={os.getpgrp()}', flush=True)
while True:
    signal.pause()
"#;

#[test]
fn repos_terminal_pager_receives_updated_window_size() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, RESIZE_PAGER);
    let output = repos_terminal(&client, &["diff"], &[("DOT_TEST_PTY_ACTION", "resize")]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"pager-size=31x101"),
        "{output:?}"
    );
}

const INPUT_PAGER: &str = r#"
import os, signal, termios, tty
fd = os.open('/dev/tty', os.O_RDWR)
original = termios.tcgetattr(fd)
try:
    tty.setcbreak(fd)
    if os.environ.get('DOT_TEST_PTY_ACTION') == 'interrupt_then_input':
        # Signal handlers must not print(): a signal landing while the main
        # thread is mid-print re-enters the buffered writer and raises
        # RuntimeError, killing the pager. Write to the fd directly.
        signal.signal(signal.SIGINT, lambda *_: os.write(1, b'pager-interrupted\n'))
    if os.environ.get('DOT_TEST_PTY_ACTION') in ('stop', 'stop_background'):
        signal.signal(signal.SIGCONT, lambda *_: os.write(1, b'pager-resumed\n'))
    if os.environ.get('DOT_TEST_PTY_ACTION') == 'terminate_stubborn':
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    print(f'pager-ready pid={os.getpid()} pager-pgid={os.getpgrp()}', flush=True)
    value = os.read(fd, 1)
    print('pager-input=' + value.decode(), flush=True)
finally:
    termios.tcsetattr(fd, termios.TCSANOW, original)
    os.close(fd)
"#;

/// A signal handler that calls `print()` can land while the main thread is
/// mid-print and re-enter the buffered writer, raising `RuntimeError` and
/// killing the pager (scheduled run 44268510873). Handlers must write to the
/// fd directly. This static guard fails if a handler ever prints again: the
/// race itself is microseconds wide and no behavioral test can hit it.
#[test]
fn repos_terminal_pager_signal_handlers_never_print() {
    for (name, fixture) in [("INPUT_PAGER", INPUT_PAGER), ("RESIZE_PAGER", RESIZE_PAGER)] {
        let lines: Vec<&str> = fixture.lines().collect();
        for line in &lines {
            let Some(signal) = line.find("signal.signal(") else {
                continue;
            };
            if line.contains("lambda") {
                assert!(
                    !line.contains("print("),
                    "{name}: signal handler must not print(): {line}"
                );
                continue;
            }
            // Named handler: `signal.signal(<sig>, <name>)` — the body of
            // `def <name>(...):` is the handler.
            let rest = &line[signal + "signal.signal(".len()..];
            let Some(handler) = rest
                .split(',')
                .nth(1)
                .map(str::trim)
                .map(|h| h.trim_end_matches(')').trim())
            else {
                continue;
            };
            let def = format!("def {handler}(");
            let Some(start) = lines
                .iter()
                .position(|candidate| candidate.trim_start().starts_with(&def))
            else {
                continue;
            };
            for body in &lines[start + 1..] {
                if body.trim().is_empty() || body.trim_start().starts_with('#') {
                    continue;
                }
                if !body.starts_with([' ', '\t']) {
                    break;
                }
                assert!(
                    !body.contains("print("),
                    "{name}: signal handler `{handler}` must not print(): {body}"
                );
            }
        }
    }
}

#[test]
fn repos_terminal_pager_can_read_the_real_controlling_terminal() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(&client, &["diff"], &[("DOT_TEST_PTY_ACTION", "input")]);
    assert!(output.status.success(), "{output:?}");
    assert!(has_bytes(&output.stdout, b"pager-input=q"), "{output:?}");
}

#[test]
fn repos_terminal_interrupt_reaps_the_interactive_pager() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(&client, &["diff"], &[("DOT_TEST_PTY_ACTION", "interrupt")]);
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    let pid = text
        .split("pager-ready pid=")
        .nth(1)
        .expect("pager ran")
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    // SAFETY: signal zero only probes whether this invocation's pager exists.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "pager {pid} survived: {output:?}"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn repos_terminal_targeted_termination_reaps_pager_and_restores_terminal() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(&client, &["diff"], &[("DOT_TEST_PTY_ACTION", "terminate")]);
    assert_eq!(output.status.code(), Some(143), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"terminal-restored=True"),
        "{output:?}"
    );
}

#[test]
fn repos_terminal_forced_termination_restores_terminal() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(
        &client,
        &["diff"],
        &[("DOT_TEST_PTY_ACTION", "terminate_stubborn")],
    );
    assert_eq!(output.status.code(), Some(143), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"terminal-restored=True"),
        "{output:?}"
    );
}

#[test]
fn repos_terminal_tostop_does_not_stop_the_output_relay() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(
        &client,
        &["diff"],
        &[("DOT_TEST_PTY_ACTION", "input"), ("DOT_TEST_TOSTOP", "1")],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(has_bytes(&output.stdout, b"pager-input=q"), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"terminal-restored=True"),
        "{output:?}"
    );
}

#[test]
fn repos_terminal_pager_can_handle_interrupt_and_remain_interactive() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(
        &client,
        &["diff"],
        &[("DOT_TEST_PTY_ACTION", "interrupt_then_input")],
    );
    // Git defers SIGINT until its pager exits; less can remain interactive
    // after Ctrl-C, then Git reports shell status 130 once the user quits.
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"pager-interrupted"),
        "{output:?}"
    );
    assert!(has_bytes(&output.stdout, b"pager-input=q"), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"terminal-restored=True"),
        "{output:?}"
    );
}

#[test]
fn repos_terminal_pager_supports_job_stop_and_resume() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(&client, &["diff"], &[("DOT_TEST_PTY_ACTION", "stop")]);
    assert!(output.status.success(), "{output:?}");
    assert!(has_bytes(&output.stdout, b"dot-stopped"), "{output:?}");
    assert!(has_bytes(&output.stdout, b"pager-input=q"), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"terminal-restored=True"),
        "{output:?}"
    );
}

#[test]
fn repos_terminal_background_resume_preserves_the_foreground_shell() {
    let client = stage_repos_client();
    repos_interactive_pager(&client, INPUT_PAGER);
    let output = repos_terminal(
        &client,
        &["diff"],
        &[("DOT_TEST_PTY_ACTION", "stop_background")],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"background-shell-preserved=True"),
        "{output:?}"
    );
    assert!(has_bytes(&output.stdout, b"pager-input=q"), "{output:?}");
    assert!(
        has_bytes(&output.stdout, b"terminal-restored=True"),
        "{output:?}"
    );
}
