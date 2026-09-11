//! End-to-end native contracts for the Shdeps provider coordinator.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{Read as _, Write as _};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
use std::os::fd::AsRawFd as _;
// FromRawFd transfers only happen on the PTY-capable platforms; Android's
// pidfd path brings its own scoped import.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::FromRawFd as _;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::OpenOptionsExt as _;

use dot_test_support::TempDir;

fn write_exec(path: &Path, body: &[u8]) {
    std::fs::write(path, body).expect("write executable fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod executable fixture");
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run fixture git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_head(dir: &Path) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("read fixture revision");
    assert!(
        output.status.success(),
        "git rev-parse: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Locate a real host command for the deliberately closed fixture PATH.
fn fixture_command(name: &str) -> Option<PathBuf> {
    let local_bin = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin"));
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let command = directory.join(name);
        if local_bin
            .as_ref()
            .is_some_and(|directory| command == directory.join(name))
        {
            continue;
        }
        let Ok(metadata) = command.metadata() else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Some(command);
        }
    }
    None
}

struct Fixture {
    _scratch: TempDir,
    root: PathBuf,
    binary: PathBuf,
    home: PathBuf,
    state: PathBuf,
    provider: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let scratch = TempDir::new_exec(tag).expect("scratch");
        let root = scratch.path().join("dot-source");
        let home = scratch.path().join("home");
        let state = scratch.path().join("state");
        let provider = scratch.path().join("provider");
        std::fs::create_dir_all(root.join("support")).expect("support");
        let binary =
            dot_test_support::owned_dot_binary(&root, Path::new(env!("CARGO_BIN_EXE_dot")))
                .expect("fixture-owned binary");
        std::fs::create_dir_all(home.join(".config/dot")).expect("config");
        std::fs::create_dir_all(&state).expect("state");
        std::fs::create_dir_all(&provider).expect("provider");
        let installer = provider.join("install.sh");
        write_exec(
            &installer,
            br#"#!/usr/bin/env bash
if [[ ${1:-} == --bootstrap ]]; then
  [[ -z ${DOT_TEST_PROVIDER_BOOTSTRAP_DIAGNOSTIC:-} ]] || printf '%s\n' 'bootstrap diagnostic' >&2
  if [[ ${DOT_TEST_PROVIDER_BOOTSTRAP_OVERFLOW:-0} == 1 ]]; then
    printf -v flood '%8192s' ''
    flood=${flood// /x}
    for ((chunk = 0; chunk < 512; chunk++)); do
      printf '%s' "$flood" >&2
    done
  fi
  [[ ${DOT_TEST_PROVIDER_BOOTSTRAP_FAIL:-0} != 1 ]] || return 7
  if [[ -n ${DOT_TEST_PROVIDER_BOOTSTRAP_RECORD:-} ]]; then
    printf '%s\n' "${SHDEPS_BOOTSTRAP_FORCE:-0}" >>"$DOT_TEST_PROVIDER_BOOTSTRAP_RECORD"
  fi
  if [[ ${SHDEPS_BOOTSTRAP_FORCE:-0} == 1 && -n ${DOT_TEST_PROVIDER_REFRESHED:-} ]]; then
    : >"$DOT_TEST_PROVIDER_REFRESHED"
  fi
  if [[ ${DOT_TEST_PROVIDER_SIGNAL_BOOTSTRAP:-0} == 1 ]]; then
    printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_SIGNAL_LEADER_PID"
    "$DOT_TEST_PROVIDER_SIGNAL_HELPER" "$PPID" \
      "$DOT_TEST_PROVIDER_SIGNAL_PID" "$DOT_TEST_PROVIDER_SIGNAL_RECORD"
    return $?
  fi
  if [[ ${DOT_TEST_PROVIDER_BOOTSTRAP_ESCAPE_STDERR:-0} == 1 ]]; then
    "$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_ESCAPE_HELPER" >/dev/null &
    ready_deadline=$((SECONDS + 10))
    until [[ -s $DOT_TEST_PROVIDER_ESCAPE_PID ]]; do
      ((SECONDS < ready_deadline)) || return 27
      sleep 0.01
    done
  fi
  source_dir=${BASH_SOURCE[0]%/*}
  if [[ $source_dir != "$SHDEPS_DIR" ]]; then
    mkdir -p "$SHDEPS_DIR"
    cp "$DOT_TEST_PROVIDER_DIR/shdeps" "$SHDEPS_DIR/shdeps"
    cp "$DOT_TEST_PROVIDER_DIR/shdeps.sh" "$SHDEPS_DIR/shdeps.sh"
    cp "$DOT_TEST_PROVIDER_DIR/install.sh" "$SHDEPS_DIR/install.sh"
  fi
  _SHDEPSW_BIN=${DOT_TEST_BOOTSTRAP_BIN:-$SHDEPS_DIR/shdeps}
  shdeps_update() {
    PATH="$SHDEPS_BIN_DIR:$PATH" SHDEPS_DIR="$DOT_TEST_PROVIDER_DIR" "$_SHDEPSW_BIN" update
  }
fi
"#,
        );
        std::fs::write(provider.join("shdeps.sh"), b"# fixture wrapper\n")
            .expect("provider wrapper");
        write_exec(
            &provider.join("shdeps"),
            br#"#!/usr/bin/env bash
case ${1:-} in
  __api)
    case ${2:-} in
      version)
        if [[ ${DOT_TEST_PROVIDER_SIGNAL_ABI:-0} == 1 ]]; then
          exec "$DOT_TEST_PROVIDER_SIGNAL_HELPER" "$PPID" \
            "$DOT_TEST_PROVIDER_SIGNAL_PID" "$DOT_TEST_PROVIDER_SIGNAL_RECORD"
        fi
        if [[ ${DOT_TEST_PROVIDER_ABI_LEAK_STDOUT:-0} == 1 ]]; then
          (sleep 30) &
          printf '%s\n' "$!" >"$DOT_TEST_PROVIDER_ABI_DESCENDANT_PID"
        fi
        if [[ ${DOT_TEST_PROVIDER_ABI_SLEEP:-0} == 1 ]]; then
          sleep "${DOT_TEST_PROVIDER_ABI_SLEEP_SECONDS:-5}"
        fi
        if [[ ${DOT_TEST_PROVIDER_ABI_OVERFLOW:-0} == 1 ]]; then
          printf -v flood '%8192s' ''
          flood=${flood// /x}
          for ((chunk = 0; chunk < 512; chunk++)); do
            printf '%s' "$flood"
          done
        fi
        printf 'abi:1\n'
        ;;
      capability)
        case ${3:-} in
          owned-subprocess-cancellation-v1|prompt-fifo-reader-before-event-v1) ;;
          *) exit 2 ;;
        esac
        [[ -z ${DOT_TEST_PROVIDER_CAPABILITY_RECORD:-} ]] ||
          printf '%s\n' "$3" >>"$DOT_TEST_PROVIDER_CAPABILITY_RECORD"
        if [[ ${DOT_TEST_PROVIDER_CAPABILITY_SLEEP:-0} == 1 ]]; then
          sleep "${DOT_TEST_PROVIDER_CAPABILITY_SLEEP_SECONDS:-5}"
        fi
        if [[ ${DOT_TEST_PROVIDER_CAPABILITY_OVERFLOW:-0} == 1 ]]; then
          printf -v flood '%8192s' ''
          flood=${flood// /x}
          for ((chunk = 0; chunk < 512; chunk++)); do
            printf '%s' "$flood"
          done
        fi
        if [[ ${DOT_TEST_PROVIDER_REQUIRE_REFRESH:-0} == 1 ]]; then
          [[ -e $DOT_TEST_PROVIDER_REFRESHED ]] || exit 1
        fi
        if [[ ${3:-} == prompt-fifo-reader-before-event-v1 && -n ${DOT_TEST_PROVIDER_SWAP_AFTER_CAPABILITY:-} ]]; then
          mv "$DOT_TEST_PROVIDER_SWAP_AFTER_CAPABILITY" "$DOT_TEST_PROVIDER_DIR/shdeps"
        fi
        if [[ ${3:-} == owned-subprocess-cancellation-v1 ]]; then
          [[ ${DOT_TEST_PROVIDER_REJECT_CAPABILITY:-0} != 1 ]]
        else
          [[ ${DOT_TEST_PROVIDER_REJECT_PROMPT_CAPABILITY:-0} != 1 ]]
        fi
        ;;
      *) exit 2 ;;
    esac
    ;;
  update)
    if [[ -n ${DOT_TEST_PROVIDER_EXIT_CODE:-} ]]; then
      exit "$DOT_TEST_PROVIDER_EXIT_CODE"
    fi
    if [[ ${DOT_TEST_PROVIDER_FOREGROUND_INTERRUPT:-0} == 1 ]]; then
      "$DOT_TEST_PROVIDER_FOREGROUND_HELPER" \
        --exact provider_foreground_interrupt_helper --nocapture &
      foreground=$!
      trap 'kill -KILL -- "-$foreground" 2>/dev/null || true; wait "$foreground" 2>/dev/null || true' EXIT
      wait "$foreground"
      status=$?
      trap - EXIT
      exit "$status"
    fi
    if [[ -n ${DOT_TEST_PROVIDER_FINAL_PROMPT_EXIT_CODE:-} ]]; then
      [[ -p ${SHDEPS_PROGRESS_PROMPT_ACK:-} ]] || exit 21
      exec "$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_PROMPT_HELPER" final \
        "$SHDEPS_PROGRESS_PROMPT_ACK" \
        "$DOT_TEST_PROVIDER_PROMPT_PATH.reader-ready" \
        "$DOT_TEST_PROVIDER_PROMPT_PATH" \
        "$DOT_TEST_PROVIDER_PROMPT_RELEASE" \
        "${DOT_TEST_PROVIDER_FINAL_PID:-/dev/null}" \
        "$DOT_TEST_PROVIDER_FINAL_PROMPT_EXIT_CODE" \
        "${DOT_TEST_PROVIDER_FINAL_PROMPT_NEWLINE:-0}"
    fi
    if [[ ${DOT_TEST_PROVIDER_PARENT_TOPOLOGY:-0} == 1 ]]; then
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_TOPOLOGY_PID"
      ready_deadline=$((SECONDS + 10))
      until [[ -e $DOT_TEST_PROVIDER_TOPOLOGY_RELEASE ]]; do
        ((SECONDS < ready_deadline)) || exit 29
        sleep 0.01
      done
    fi
    if [[ ${DOT_TEST_PROVIDER_HANG:-0} == 1 ]]; then
      trap 'printf "%s\n" HUP >>"$DOT_TEST_PROVIDER_SIGNAL_RECORD"' HUP
      trap 'printf "%s\n" INT >>"$DOT_TEST_PROVIDER_SIGNAL_RECORD"' INT
      trap 'printf "%s\n" QUIT >>"$DOT_TEST_PROVIDER_SIGNAL_RECORD"' QUIT
      set -m
      (
        trap '' HUP INT QUIT TERM
        printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_HANG_DESCENDANT_PID"
        while :; do sleep 1; done
      ) </dev/null >/dev/null 2>&1 &
      descendant=$!
      trap 'printf "%s\n" TERM >>"$DOT_TEST_PROVIDER_SIGNAL_RECORD"; kill -KILL -- "-$descendant" 2>/dev/null || true; wait "$descendant" 2>/dev/null || true; exit 143' TERM
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_HANG_PID"
      while :; do wait || true; done
    fi
    if [[ ${DOT_TEST_PROVIDER_LIVE_OUTPUT:-0} == 1 ]]; then
      trap '' TERM
      printf '%s\n' \
        '{"event":"warning","status":"warning","detail":"provider live stdout"}'
      printf '%s\n' 'provider live stderr' >&2
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_LIVE_PID"
      while :; do sleep 1; done
    fi
    if [[ -n ${DOT_TEST_PROVIDER_OVERFLOW_STREAM:-} ]]; then
      trap ': >"$DOT_TEST_PROVIDER_OVERFLOW_STOPPED"; exit 143' TERM
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_OVERFLOW_PID"
      printf -v flood '%8192s' ''
      flood=${flood// /x}
      for ((chunk = 0; chunk < 512; chunk++)); do
        if [[ $DOT_TEST_PROVIDER_OVERFLOW_STREAM == stdout ]]; then
          printf '%s' "$flood"
        else
          printf '%s' "$flood" >&2
        fi
      done
      while :; do sleep 1; done
    fi
    if [[ ${DOT_TEST_PROVIDER_MANY_EVENTS:-0} == 1 ]]; then
      trap ': >"$DOT_TEST_PROVIDER_MANY_EVENTS_STOPPED"; exit 143' TERM
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_MANY_EVENTS_PID"
      for ((event = 0; event < 5000; event++)); do
        printf '%s\n' \
          '{"event":"item","group":"cargo","status":"changed","name":"small","detail":"event"}'
      done
      printf '%s\n' \
        '{"event":"summary","status":"changed","changed":1,"warnings":0,"current":0,"skipped":0,"failed":0}'
      while :; do sleep 1; done
    fi
    if [[ ${DOT_TEST_PROVIDER_BACKPRESSURE:-0} == 1 ]]; then
      trap 'printf "%s\n" INT >"$DOT_TEST_PROVIDER_BACKPRESSURE_SIGNAL"; exit 130' INT
      trap 'printf "%s\n" TERM >"$DOT_TEST_PROVIDER_BACKPRESSURE_SIGNAL"; exit 143' TERM
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_BACKPRESSURE_PID"
      printf -v flood '%8192s' ''
      flood=${flood// /x}
      printf '%s' '{"event":"warning","status":"warning","detail":"PROVIDER-BLOCKED '
      # Stay below Dot's per-frame safety limit while exceeding ordinary pipe
      # capacity, so this exercises a blocked outward sink rather than the
      # oversized-provider-frame rejection path.
      for ((chunk = 0; chunk < 64; chunk++)); do
        printf '%s' "$flood"
      done
      printf '%s\n' '"}'
      while :; do sleep 1; done
    fi
    if [[ ${DOT_TEST_PROVIDER_BACKPRESSURE_EXIT130:-0} == 1 ]]; then
      printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_BACKPRESSURE_PID"
      printf -v flood '%8192s' ''
      flood=${flood// /x}
      printf '%s' '{"event":"warning","status":"warning","detail":"PROVIDER-EXIT130 '
      for ((chunk = 0; chunk < 64; chunk++)); do
        printf '%s' "$flood"
      done
      printf '%s\n' '"}'
      exit 130
    fi
    if [[ ${DOT_TEST_PROVIDER_SIGNAL_DURING_TEARDOWN:-0} == 1 ]]; then
      dot_pid=$PPID
      set -m
      (
        trap 'kill -STOP "$dot_pid"; printf "%s\n" TERM >"$DOT_TEST_PROVIDER_TEARDOWN_SIGNAL"; kill -INT "$dot_pid"; kill -CONT "$dot_pid"; trap "" TERM; while :; do sleep 1; done' TERM
        printf '%s\n' "$BASHPID" >"$DOT_TEST_PROVIDER_TEARDOWN_PID"
        while :; do sleep 1; done
      ) </dev/null >/dev/null 2>&1 &
      teardown_descendant=$!
      trap 'kill -KILL -- "-$teardown_descendant" 2>/dev/null || true; wait "$teardown_descendant" 2>/dev/null || true; exit 143' TERM
      ready_deadline=$((SECONDS + 10))
      until [[ -s $DOT_TEST_PROVIDER_TEARDOWN_PID ]]; do
        ((SECONDS < ready_deadline)) || exit 26
        sleep 0.01
      done
    fi
    if [[ ${DOT_TEST_PROVIDER_LEAK_STDOUT:-0} == 1 ]]; then
      set -m
      (trap '' HUP INT QUIT TERM; sleep 30) </dev/null &
      descendant=$!
      trap 'kill -KILL -- "-$descendant" 2>/dev/null || true; wait "$descendant" 2>/dev/null || true' EXIT
      printf '%s\n' "$descendant" >"$DOT_TEST_PROVIDER_DESCENDANT_PID"
      exit 0
    fi
    if [[ ${DOT_TEST_PROVIDER_ESCAPE_OUTPUT:-0} == 1 ]]; then
      case ${DOT_TEST_PROVIDER_ESCAPE_STREAM:-} in
        stdout)
          "$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_ESCAPE_HELPER" \
            --exact provider_exit_does_not_wait_for_escaped_session_output_holders \
            --nocapture 2>/dev/null &
          ;;
        stderr)
          "$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_ESCAPE_HELPER" \
            --exact provider_exit_does_not_wait_for_escaped_session_output_holders \
            --nocapture >/dev/null &
          ;;
        *) exit 24 ;;
      esac
      ready_deadline=$((SECONDS + 10))
      until [[ -s $DOT_TEST_PROVIDER_ESCAPE_PID ]]; do
        ((SECONDS < ready_deadline)) || exit 25
        sleep 0.01
      done
      if [[ -n ${DOT_TEST_PROVIDER_ESCAPE_RELEASE:-} ]]; then
        until [[ -e $DOT_TEST_PROVIDER_ESCAPE_RELEASE ]]; do
          ((SECONDS < ready_deadline)) || exit 26
          sleep 0.01
        done
        # Keep the provider alive while its escaped writer fills the capture.
        # A correct bounded relay stops this provider before the fixture's own
        # deadline; a regression remains bounded and fails `wait_bounded`.
        while ((SECONDS < ready_deadline)); do sleep 0.01; done
        exit 27
      fi
      exit 0
    fi
    if [[ ${DOT_TEST_PROVIDER_PROMPT:-0} == 1 ]]; then
      [[ -p ${SHDEPS_PROGRESS_PROMPT_ACK:-} ]] || exit 21
      "$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_PROMPT_HELPER" normal \
        "$SHDEPS_PROGRESS_PROMPT_ACK" \
        "$DOT_TEST_PROVIDER_PROMPT_RECORD.reader-ready" \
        "$DOT_TEST_PROVIDER_PROMPT_RECORD" || exit $?
    fi
    if [[ ${DOT_TEST_PROVIDER_PROMPT_AFTER_SIGNAL:-0} == 1 ]]; then
      [[ -p ${SHDEPS_PROGRESS_PROMPT_ACK:-} ]] || exit 21
      exec "$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_PROMPT_HELPER" after-signal \
        "$SHDEPS_PROGRESS_PROMPT_ACK" \
        "$DOT_TEST_PROVIDER_PROMPT_RECORD.reader-ready" \
        "$DOT_TEST_PROVIDER_PROMPT_RECORD"
    fi
    printf 'force=%s quiet=%s nested=%s jobs=%s\n' \
      "${SHDEPS_FORCE:-0}" "${SHDEPS_QUIET:-0}" "${SHDEPS_NESTED:-0}" "${SHDEPS_JOBS:-unset}" \
      >"$DOT_TEST_PROVIDER_RECORD"
    if [[ ${DOT_TEST_PROVIDER_RECORD_PATH:-0} == 1 ]]; then
      printf 'path=%s\n' "${PATH%%:*}" >>"$DOT_TEST_PROVIDER_RECORD"
    fi
    if [[ ${DOT_TEST_RECORD_BINARY_MARKER:-0} == 1 ]]; then
      printf 'binary=%s\n' "${DOT_TEST_BINARY_MARKER:-default}" >>"$DOT_TEST_PROVIDER_RECORD"
    fi
    advance=0
    generation=1
    if [[ ${DOT_TEST_PROVIDER_ADVANCE_TWICE:-0} == 1 ]]; then
      generation=$(($(cat "$DOT_TEST_PROVIDER_ADVANCED" 2>/dev/null || printf 0) + 1))
      [[ $generation -le 2 ]] && advance=1
    elif [[ ${DOT_TEST_PROVIDER_ADVANCE_SOURCE:-0} == 1 && ! -e $DOT_TEST_PROVIDER_ADVANCED ]]; then
      advance=1
    fi
    if [[ $advance == 1 ]]; then
      printf '%s\n' "$generation" >"$DOT_TEST_PROVIDER_ADVANCED"
      printf 'advanced %s\n' "$generation" >"$DOT_SOURCE_ROOT/provider-generation"
      git -C "$DOT_SOURCE_ROOT" add provider-generation
      git -C "$DOT_SOURCE_ROOT" -c core.hooksPath=/dev/null commit -qm provider-generation
    fi
    if [[ ${DOT_TEST_PROVIDER_FAIL:-0} == 1 ]]; then
      if [[ ${SHDEPS_PROGRESS:-} == jsonl ]]; then
        printf '%s\n' \
          '{"event":"item","group":"cargo","status":"failed","name":"ripgrep","detail":"network unavailable"}' \
          '{"event":"group_summary","group":"cargo","label":"Cargo","status":"failed","changed":0,"warnings":0,"current":0,"skipped":0,"failed":1,"elapsed_ms":12}' \
          '{"event":"summary","status":"failed","changed":0,"warnings":0,"current":0,"skipped":0,"failed":1}'
      fi
      exit 9
    elif [[ ${SHDEPS_PROGRESS:-} == jsonl ]]; then
      if [[ ${DOT_TEST_PROVIDER_LARGE_OUTPUT:-0} == 1 ]]; then
        printf '%300000s\n' x
        printf '%300000s\n%s\n' y 'provider stderr tail' >&2
      fi
      if [[ ${DOT_TEST_PROVIDER_VERBOSE_EVENTS:-0} == 1 ]]; then
        printf '%s\n' \
          '{"event":"phase","label":"Resolving","done":1,"total":2}' \
          '{"event":"warning","status":"warning","detail":"provider warning"}'
      fi
      item_name='ripgrep'
      item_detail='installed'
      [[ ${DOT_TEST_PROVIDER_ESCAPED_EVENT:-0} != 1 ]] || item_name='caf\u00e9'
      [[ ${DOT_TEST_PROVIDER_ESCAPED_QUOTE_DETAIL:-0} != 1 ]] || item_detail='said \"hello\" tail'
      printf '%s\n' \
        "{\"event\":\"item\",\"group\":\"cargo\",\"status\":\"changed\",\"name\":\"$item_name\",\"detail\":\"$item_detail\"}" \
        '{"event":"group_summary","group":"cargo","label":"Cargo","status":"changed","changed":1,"warnings":0,"current":0,"skipped":0,"failed":0,"elapsed_ms":12}' \
        '{"event":"summary","status":"changed","changed":1,"warnings":0,"current":0,"skipped":0,"failed":0}'
    fi
    if [[ ${DOT_TEST_PROVIDER_SIGNAL_DURING_TEARDOWN:-0} == 1 ]]; then
      kill -TERM -- "-$teardown_descendant"
      wait "$teardown_descendant" 2>/dev/null || true
    fi
    ;;
  *) exit 2 ;;
esac
"#,
        );
        write_exec(
            &provider.join("prompt-provider.py"),
            br#"#!/usr/bin/env python3
import os
import select
import signal
import sys
import time
from pathlib import Path

mode, fifo, ready, record = sys.argv[1:5]
fd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
Path(ready).write_text("ready", encoding="ascii")

def prompt(newline=True):
    payload = b'{"event":"prompt","status":"running","detail":"waiting"}'
    os.write(1, payload + (b"\n" if newline else b""))

def acknowledgement(timeout=2.0):
    poller = select.poll()
    poller.register(fd, select.POLLIN)
    deadline = time.monotonic() + timeout
    data = b""
    while time.monotonic() < deadline:
        events = poller.poll(max(1, int(min(0.05, deadline - time.monotonic()) * 1000)))
        if not events:
            continue
        chunk = os.read(fd, 4096)
        if not chunk:
            continue
        data += chunk
        if b"\n" in data:
            return data.split(b"\n", 1)[0]
    return None

if mode == "normal":
    prompt()
    token = acknowledgement()
    if token != b"ready":
        raise SystemExit(22)
    Path(record).write_bytes(token + b"\n")
    raise SystemExit(0)
if mode == "final":
    release, pid_file, exit_code, newline = sys.argv[5:9]
    Path(record).write_text(fifo + "\n", encoding="utf-8")
    Path(pid_file).write_text(f"{os.getpid()}\n", encoding="ascii")
    deadline = time.monotonic() + 10
    while not Path(release).exists():
        if time.monotonic() >= deadline:
            raise SystemExit(22)
        time.sleep(0.01)
    prompt(newline == "1")
    raise SystemExit(int(exit_code))
if mode == "after-signal":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    dot_pid = os.getppid()
    os.kill(dot_pid, signal.SIGSTOP)
    prompt()
    os.kill(dot_pid, signal.SIGINT)
    os.kill(dot_pid, signal.SIGCONT)
    token = acknowledgement()
    if token is not None:
        Path(record).write_bytes(token + b"\n")
    while True:
        time.sleep(1)
raise SystemExit(2)
"#,
        );
        let digest = dot::shdeps::sha256_file(&installer).expect("installer digest");
        std::fs::write(
            root.join("support/shdeps.lock"),
            format!(
                "revision=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ninstall_sha256={digest}\nabi=1\n"
            ),
        )
        .expect("provider lock");
        std::fs::write(
            home.join(".config/dot/config"),
            b"version=1\ndependency_provider=shdeps\nshdeps_update_policy=pinned\n",
        )
        .expect("dot config");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "fixture"]);
        git(&root, &["config", "user.email", "fixture@example.invalid"]);
        git(&root, &["add", "support/shdeps.lock"]);
        git(&root, &["commit", "-qm", "fixture"]);
        Self {
            _scratch: scratch,
            root,
            binary,
            home,
            state,
            provider,
        }
    }

    fn command(&self) -> Command {
        self.command_for("update")
    }

    fn command_for(&self, subcommand: &str) -> Command {
        let mut command = Command::new(&self.binary);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let fixture_python = if Path::new("/usr/bin/python3").is_file() {
            PathBuf::from("/usr/bin/python3")
        } else {
            fixture_command("python3").expect("fixture Python")
        };
        command
            .arg(subcommand)
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", "")
            .env("XDG_STATE_HOME", &self.state)
            .env("DOT_SOURCE_ROOT", self.home.join("untrusted-source-root"))
            .env("DOT_DEPENDENCY_PROVIDER", "shdeps")
            .env("DOT_SHDEPS_UPDATE_POLICY", "pinned")
            .env("DOT_UPDATE_JOBS", "2")
            .env("SHDEPS_LIB", self.provider.join("shdeps.sh"))
            .env("SHDEPS_DIR", &self.provider)
            .env("DOT_TEST_PROVIDER_DIR", &self.provider)
            .env(
                "DOT_TEST_PROVIDER_PROMPT_HELPER",
                self.provider.join("prompt-provider.py"),
            )
            .env("DOT_TEST_PROVIDER_PYTHON", fixture_python)
            .env(
                "DOT_TEST_PROVIDER_RECORD",
                self.home.join("provider-record"),
            )
            .env("DOT_BASH", dot_test_support::bash())
            // Bash fills an absent SHELL with its own path. Pin it explicitly
            // so both engines produce the same final reload hint.
            .env("SHELL", dot_test_support::bash())
            .env("GIT_AUTHOR_NAME", "fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", "/dev/null")
            .env("GIT_CONFIG_KEY_1", "commit.gpgSign")
            .env("GIT_CONFIG_VALUE_1", "false")
            .env("GIT_CONFIG_KEY_2", "tag.gpgSign")
            .env("GIT_CONFIG_VALUE_2", "false")
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn development_git_path(&self) -> std::ffi::OsString {
        let bin = self._scratch.path().join("development-git-bin");
        std::fs::create_dir_all(&bin).expect("development Git bin");
        write_exec(
            &bin.join("git"),
            br#"#!/bin/sh
[ -z "${DOT_TEST_DEVELOPMENT_GIT_TRACE:-}" ] || printf '%s\n' "$*" >>"$DOT_TEST_DEVELOPMENT_GIT_TRACE"
case " $* " in
  *" -C $DOT_TEST_DEVELOPMENT_CHECKOUT "*) ;;
  *) exec "$DOT_TEST_REAL_GIT" "$@" ;;
esac
case " $* " in
  *" $DOT_TEST_DEVELOPMENT_GIT_QUERY "*) ;;
  *) exec "$DOT_TEST_REAL_GIT" "$@" ;;
esac
case ${DOT_TEST_DEVELOPMENT_GIT_MODE:-} in
  block)
    trap '' TERM
    printf '%s\n' "$$" >"$DOT_TEST_DEVELOPMENT_GIT_READY"
    while :; do sleep 1; done
    ;;
  overflow)
    trap ': >"$DOT_TEST_DEVELOPMENT_GIT_STOPPED"; exit 143' TERM
    printf '%s\n' "$$" >"$DOT_TEST_DEVELOPMENT_GIT_READY"
    chunk='xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'
    while :; do
      printf '%s' "$chunk"
    done
    ;;
esac
exec "$DOT_TEST_REAL_GIT" "$@"
"#,
        );
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths).expect("development Git PATH")
    }

    fn curl_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("fixture-bin");
        std::fs::create_dir_all(&bin).expect("curl bin");
        write_exec(
            &bin.join("curl"),
            br#"#!/usr/bin/env bash
out=''
url=''
while [[ $# -gt 0 ]]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    http*) url=$1; shift ;;
    *) shift ;;
  esac
done
printf '%s\n' "$url" >"$DOT_TEST_CURL_RECORD"
cp "$DOT_TEST_PROVIDER_DIR/install.sh" "$out"
"#,
        );
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&current));
        std::env::join_paths(paths).expect("fixture PATH")
    }

    fn failing_curl_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("failing-bin");
        std::fs::create_dir_all(&bin).expect("failing curl bin");
        write_exec(
            &bin.join("curl"),
            b"#!/bin/sh\n[ -z \"${DOT_TEST_CURL_PID:-}\" ] || printf '%s\\n' \"$$\" >\"$DOT_TEST_CURL_PID\"\n[ -z \"${DOT_TEST_CURL_DIAGNOSTIC:-}\" ] || printf 'curl diagnostic\\n' >&2\nprintf 'attempt\\n' >>\"$DOT_TEST_CURL_RECORD\"\nexit 22\n",
        );
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&current));
        std::env::join_paths(paths).expect("fixture PATH")
    }

    fn adversarial_curl_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("adversarial-curl-bin");
        std::fs::create_dir_all(&bin).expect("adversarial curl bin");
        write_exec(
            &bin.join("curl"),
            br#"#!/bin/sh
trap '' TERM
printf '%s\n' "$$" >"$DOT_TEST_CURL_PID"
case ${DOT_TEST_CURL_MODE:-flood} in
  flood)
    head -c 629145 /dev/zero
    head -c 629145 /dev/zero >&2
    ;;
  backpressure-timeout)
    head -c 262144 /dev/zero
    printf '%s\n' ready >"$DOT_TEST_CURL_FLOOD_READY"
    ;;
  stall) ;;
  *) exit 2 ;;
esac
while :; do sleep 1; done
"#,
        );
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&current));
        std::env::join_paths(paths).expect("adversarial curl PATH")
    }

    fn signal_helper(&self) -> PathBuf {
        let helper = self.home.join("signal-helper.py");
        write_exec(
            &helper,
            br#"#!/usr/bin/env python3
import os
import signal
import time
import sys

if sys.argv[1:] == ["--dot-fixture-ready"]:
    raise SystemExit(0)

dot_pid = int(sys.argv[1])
pid_file = sys.argv[2]
signal_file = sys.argv[3]
owned = {signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM}
signal.pthread_sigmask(signal.SIG_BLOCK, owned)
with open(pid_file, "w", encoding="ascii") as marker:
    marker.write(f"{os.getpid()}\n")
release = os.environ.get("DOT_TEST_PROVIDER_SIGNAL_RELEASE")
if release:
    deadline = time.monotonic() + 10
    while not os.path.exists(release):
        if time.monotonic() >= deadline:
            raise SystemExit(27)
        time.sleep(0.01)
os.kill(dot_pid, signal.SIGINT)
received = signal.sigwait(owned)
with open(signal_file, "w", encoding="ascii") as marker:
    marker.write(f"{int(received)}\n")
"#,
        );
        dot_test_support::wait_until_executable(&helper, &["--dot-fixture-ready"])
            .expect("signal helper executable");
        helper
    }

    fn signal_curl_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("signal-curl-bin");
        std::fs::create_dir_all(&bin).expect("signal curl bin");
        write_exec(
            &bin.join("curl"),
            br#"#!/bin/sh
printf 'attempt\n' >>"$DOT_TEST_CURL_RECORD"
exec "$DOT_TEST_PROVIDER_SIGNAL_HELPER" "$PPID" \
  "$DOT_TEST_PROVIDER_SIGNAL_PID" "$DOT_TEST_PROVIDER_SIGNAL_RECORD"
"#,
        );
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&current));
        std::env::join_paths(paths).expect("signal curl PATH")
    }

    fn escaped_curl_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("escaped-curl-bin");
        std::fs::create_dir_all(&bin).expect("escaped curl bin");
        write_exec(
            &bin.join("curl"),
            br#"#!/bin/sh
printf 'attempt\n' >>"$DOT_TEST_CURL_RECORD"
"$DOT_TEST_PROVIDER_PYTHON" "$DOT_TEST_PROVIDER_ESCAPE_HELPER" >/dev/null &
ready=0
while [ "$ready" -lt 1000 ]; do
  [ -s "$DOT_TEST_PROVIDER_ESCAPE_PID" ] && break
  ready=$((ready + 1))
  sleep 0.01
done
[ -s "$DOT_TEST_PROVIDER_ESCAPE_PID" ] || exit 28
out=''
while [ "$#" -gt 0 ]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    *) shift ;;
  esac
done
cp "$DOT_TEST_PROVIDER_DIR/install.sh" "$out"
"#,
        );
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&current));
        std::env::join_paths(paths).expect("escaped curl PATH")
    }

    /// Long-lived escaped-output holder. Callers must spawn it via
    /// `DOT_TEST_PROVIDER_PYTHON`, never through its `env python3` shebang: a
    /// PATH `python3` may be a launcher that forks the real interpreter and
    /// waits, keeping inherited descriptors (like the session lease) open in
    /// the waiting parent for the holder's whole lifetime.
    fn escaped_output_helper(&self) -> PathBuf {
        let helper = self.home.join("escaped-output-helper.py");
        write_exec(
            &helper,
            br#"#!/usr/bin/env python3
import os
import signal
import time

for handled in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
    signal.signal(handled, signal.SIG_IGN)
os.setsid()
if os.environ.get("DOT_TEST_PROVIDER_ESCAPE_OUTPUT") == "1":
    # A foreground-provider escapee is outside Dot's owned session, so Dot
    # must neither signal it nor wait for it. Model the well-behaved daemon
    # pattern (close_fds): keep only stdio, cooperatively releasing the
    # owned-session lease so provider teardown can prove descendant exit.
    # Preparation-path escapees keep the lease so owned-session teardown can
    # attribute and reap them instead.
    try:
        ceiling = os.sysconf("SC_OPEN_MAX")
    except (AttributeError, ValueError, OSError):
        ceiling = 256
    os.closerange(3, ceiling)
with open(os.environ["DOT_TEST_PROVIDER_ESCAPE_PID"], "w", encoding="ascii") as marker:
    marker.write(f"{os.getpid()}\n")
stream = os.environ.get("DOT_TEST_PROVIDER_ESCAPE_FLOOD")
deadline = time.monotonic() + float(os.environ.get("DOT_TEST_PROVIDER_ESCAPE_LIFETIME", "30"))
release = os.environ.get("DOT_TEST_PROVIDER_ESCAPE_RELEASE")
while release and not os.path.exists(release) and time.monotonic() < deadline:
    time.sleep(0.01)
if stream:
    descriptor = 1 if stream == "stdout" else 2
    try:
        while time.monotonic() < deadline:
            os.write(descriptor, b"x" * 8192)
    except BrokenPipeError:
        raise SystemExit(0)
time.sleep(max(0.0, deadline - time.monotonic()))
"#,
        );
        helper
    }

    /// Rebuild the inherited command path without `jq`. The provider intentionally
    /// falls back to its bootstrap JSON parser in this mode, so this must not
    /// depend on whether a host image happens to install jq.
    fn closed_tool_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("without-jq-bin");
        std::fs::create_dir_all(&bin).expect("without-jq bin");
        // Build a fixture-owned allowlist instead of borrowing whole host
        // directories. Homebrew macOS splits Bash/Git from system tools such
        // as mv, id, and shasum, while Linux commonly co-locates them. Listing
        // each dependency keeps this path portable and excludes developer
        // wrappers and jq; unavailable optional helpers remain absent so the
        // native fallback owns the result.
        let bash = fixture_command("bash").expect("no-jq fixture requires `bash` in the host PATH");
        write_exec(
            &bin.join("bash"),
            format!("#!/bin/sh\nexec {} \"$@\"\n", bash.display()).as_bytes(),
        );
        for name in [
            "awk",
            "cat",
            "chmod",
            "dirname",
            "getconf",
            "git",
            "id",
            "mkdir",
            "mkfifo",
            "mktemp",
            "mv",
            "od",
            "ps",
            "python3",
            "rm",
            "rmdir",
            "sed",
            "sleep",
            "tr",
            "wc",
            "sha256sum",
            "shasum",
        ] {
            let Some(source) = fixture_command(name) else {
                continue;
            };
            // Git discovers its helper path from argv[0], so a small exec
            // wrapper is safer than a symlink into this synthetic directory.
            write_exec(
                &bin.join(name),
                format!("#!/bin/sh\nexec {} \"$@\"\n", source.display()).as_bytes(),
            );
        }
        bin.into_os_string()
    }

    /// Supply the documented jq progress-frame ABI rather than borrowing a package
    /// from the host image. The fixture emits only the documented progress
    /// records below, and this NUL frame is the documented parser ABI.
    fn jq_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("jq-bin");
        std::fs::create_dir_all(&bin).expect("jq bin");
        write_exec(
            &bin.join("jq"),
            br##"#!/usr/bin/env bash
line=$(cat)
case $line in
  *'"event":"item"'*)
    printf '%s\0' item cargo '' changed installed '' '' $'caf\xc3\xa9' '' '' '' '' '' ''
    ;;
  *'"event":"group_summary"'*)
    printf '%s\0' group_summary cargo Cargo changed '' '' '' '' 1 0 0 0 0 12
    ;;
  *'"event":"summary"'*)
    printf '%s\0' summary '' '' changed '' '' '' '' 1 0 0 0 0 ''
    ;;
esac
"##,
        );
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&inherited));
        std::env::join_paths(paths).expect("jq fixture PATH")
    }

    fn development(&self, pinned: bool) -> PathBuf {
        let root = self.home.join("dev-root");
        let checkout = root.join("shdeps");
        std::fs::create_dir_all(&checkout).expect("development checkout");
        for name in ["install.sh", "shdeps.sh", "shdeps"] {
            std::fs::copy(self.provider.join(name), checkout.join(name))
                .expect("copy development file");
        }
        git(&checkout, &["init", "-q"]);
        git(&checkout, &["config", "user.name", "fixture"]);
        git(
            &checkout,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/cgraf78/shdeps.git",
            ],
        );
        git(
            &checkout,
            &["add", "-f", "install.sh", "shdeps.sh", "shdeps"],
        );
        git(&checkout, &["commit", "-qm", "provider"]);
        if pinned {
            let output = Command::new("git")
                .arg("-C")
                .arg(&checkout)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("development revision");
            let revision = String::from_utf8(output.stdout).expect("revision UTF-8");
            let digest = dot::shdeps::sha256_file(&checkout.join("install.sh"))
                .expect("development installer digest");
            std::fs::write(
                self.root.join("support/shdeps.lock"),
                format!(
                    "revision={}\ninstall_sha256={digest}\nabi=1\n",
                    revision.trim()
                ),
            )
            .expect("pinned development lock");
            git(&self.root, &["add", "support/shdeps.lock"]);
            git(&self.root, &["commit", "-qm", "pin-provider"]);
        } else {
            std::fs::write(checkout.join("latest-note"), b"ahead\n").expect("latest note");
            git(&checkout, &["add", "latest-note"]);
            git(&checkout, &["commit", "-qm", "ahead"]);
            std::fs::write(
                self.home.join(".config/dot/config"),
                b"version=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n",
            )
            .expect("latest config");
        }
        std::fs::set_permissions(&checkout, std::fs::Permissions::from_mode(0o755))
            .expect("checkout mode");
        std::fs::set_permissions(
            checkout.join(".git"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("git dir mode");
        root
    }
}

fn normalize_elapsed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let elapsed = elapsed_range(body);
        if let Some(range) = elapsed {
            out.extend_from_slice(&body[..range.start]);
            out.push(b'N');
            out.extend_from_slice(&body[range.end..]);
        } else {
            out.extend_from_slice(body);
        }
        if line.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out
}

fn elapsed_range(line: &[u8]) -> Option<std::ops::Range<usize>> {
    for prefix in [b"Done in ".as_slice(), b"Done with errors in ".as_slice()] {
        if let Some(tail) = line.strip_prefix(prefix) {
            let digits = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
            let suffix = &tail[digits..];
            let valid = suffix == b"s"
                || suffix == b"s."
                || suffix == b"s. Reload your shell: source ~/.bashrc"
                || suffix == b"s. Reload your shell: source ~/.zshrc";
            return (digits > 0 && valid).then_some(prefix.len()..prefix.len() + digits);
        }
    }
    let close = line.iter().position(|byte| *byte == b']')?;
    let mut counts = line.get(1..close)?.split(|byte| *byte == b'/');
    let done = counts.next()?;
    let total = counts.next()?;
    let valid_prefix = line.first() == Some(&b'[')
        && !done.is_empty()
        && done.iter().all(u8::is_ascii_digit)
        && !total.is_empty()
        && total.iter().all(u8::is_ascii_digit)
        && counts.next().is_none()
        && line.get(close + 1) == Some(&b' ');
    let start = line.iter().rposition(|byte| *byte == b' ')? + 1;
    let digits = line.get(start..)?.strip_suffix(b"s")?;
    (valid_prefix && !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
        .then_some(start..start + digits.len())
}

/// Run a closed provider fixture under the same session-aware supervisor used
/// by the public test runner.
///
/// `Command` is not cloneable, so rebuild its observable program, arguments,
/// environment, and working directory around the supervisor. This keeps the
/// fixture hermetic while ensuring a timeout terminates and reaps descendants
/// instead of abandoning a thread blocked in `Command::output`.
fn bounded_output(command: Command, seconds: u64) -> Output {
    let program = command.get_program().to_os_string();
    let args: Vec<_> = command.get_args().map(ToOwned::to_owned).collect();
    let env: Vec<_> = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(ToOwned::to_owned)))
        .collect();
    let current_dir = command.get_current_dir().map(Path::to_path_buf);
    let mut bounded =
        Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("lib/dot/public/test-timeout-v1"));
    bounded
        .arg(format!("{seconds}s"))
        .arg(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        match value {
            Some(value) => bounded.env(key, value),
            None => bounded.env_remove(key),
        };
    }
    if let Some(current_dir) = current_dir {
        bounded.current_dir(current_dir);
    }
    bounded.output().expect("run bounded provider update")
}

/// `kill -0` reports a zombie until an unrelated container PID 1 reaps it.
/// The provider contract is that descendants have exited, not that a runner's
/// init has already collected their status record.
fn process_running(pid: &str) -> bool {
    let numeric = pid.parse::<i32>().expect("numeric process pid");
    process_running_pid(numeric)
}

fn wait_for_pid(path: &Path, seconds: u64) -> Option<i32> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        if let Ok(pid) = std::fs::read_to_string(path) {
            if let Some(pid) = pid.trim().parse::<i32>().ok().filter(|pid| *pid > 0) {
                return Some(pid);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Capture a fixture's identity, or confirm it already exited. A fast
/// provider can exit between writing its pidfile and our first identity
/// query; treating that gap as failure is a test race, not a product
/// regression. Returns `None` only when signal 0 reports `ESRCH` (no
/// process holds the pid, so ours definitively exited — pid reuse after
/// that cannot resurrect it). Zombies keep their identity queryable and
/// resolve to `Some`, like any live process.
fn wait_for_identity_or_exit(pid: i32, seconds: u64) -> Option<TestProcessIdentity> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        if let Some(identity) = process_identity(pid) {
            return Some(identity);
        }
        // SAFETY: signal 0 only probes the fixture-owned PID.
        let probe = unsafe { libc::kill(pid, 0) };
        if probe != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return None;
        }
        if std::time::Instant::now() >= deadline {
            panic!("provider identity never observable for pid {pid}");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn wait_for_nonempty(path: &Path, seconds: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_running_pid(pid: i32) -> bool {
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

fn process_session(pid: i32) -> Option<i32> {
    // SAFETY: getsid observes only the positive fixture-owned PID.
    let session = unsafe { libc::getsid(pid) };
    (session > 0).then_some(session)
}

fn process_group(pid: i32) -> Option<i32> {
    // SAFETY: getpgid observes only the positive fixture-owned PID.
    let group = unsafe { libc::getpgid(pid) };
    (group > 0).then_some(group)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestProcessIdentity {
    pid: i32,
    group: i32,
    session: i32,
    generation: Vec<u8>,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn process_identity(pid: i32) -> Option<TestProcessIdentity> {
    let generation = process_generation(pid)?;
    let identity = TestProcessIdentity {
        pid,
        group: process_group(pid)?,
        session: process_session(pid)?,
        generation,
    };
    (process_generation(pid)? == identity.generation).then_some(identity)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_generation(pid: i32) -> Option<Vec<u8>> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.windows(2).rposition(|part| part == b") ")?;
    stat[end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .nth(19)
        .map(<[u8]>::to_vec)
}

#[cfg(target_os = "macos")]
fn process_generation(pid: i32) -> Option<Vec<u8>> {
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

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn same_process(identity: &TestProcessIdentity) -> bool {
    process_identity(identity.pid).as_ref() == Some(identity)
}

struct PinnedTestProcess {
    identity: TestProcessIdentity,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pidfd: std::os::fd::OwnedFd,
}

impl PinnedTestProcess {
    fn claim(identity: TestProcessIdentity) -> Option<Self> {
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
            if !same_process(&identity) {
                return None;
            }
            Some(Self { identity, pidfd })
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            same_process(&identity).then_some(Self { identity })
        }
    }

    fn signal_for_cleanup(&self, signal: i32) -> bool {
        if !same_process(&self.identity) || self.identity.group == unsafe { libc::getpgrp() } {
            return false;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::fd::AsRawFd as _;

            if self.identity.pid == self.identity.group {
                // The retained pidfd anchors this private group number across
                // validation and delivery, even if the leader exits.
                // SAFETY: the pinned fixture leader owns this process group.
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

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn session_members(session: i32) -> Vec<TestProcessIdentity> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let pids = std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok()?.parse::<i32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    #[cfg(target_os = "macos")]
    let pids = {
        Command::new("/bin/ps")
            .args(["-A", "-o", "pid="])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<i32>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    pids.into_iter()
        .filter_map(process_identity)
        .filter(|identity| identity.session == session)
        .collect()
}

/// Own a child placed in a private session so every failure path can stop
/// fixture processes without touching the test runner or an unrelated PID.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
struct TestSessionChild {
    child: Option<std::process::Child>,
    leader: TestProcessIdentity,
    foreground: Option<PinnedTestProcess>,
    reaped: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
impl TestSessionChild {
    fn new(child: std::process::Child) -> Self {
        let pid = child.id() as i32;
        // `spawn_test_session` and `spawn_on_pty` return only after their
        // pre-exec `setsid` succeeds, so these are stable fixture identities
        // even if the process exits before the first observation.
        Self {
            child: Some(child),
            leader: process_identity(pid).expect("observe private session leader"),
            foreground: None,
            reaped: false,
        }
    }

    fn id(&self) -> i32 {
        self.leader.pid
    }

    fn take_output_readers(&mut self) -> (std::process::ChildStdout, std::process::ChildStderr) {
        let child = self.child.as_mut().expect("retained test child");
        (
            child.stdout.take().expect("captured test stdout"),
            child.stderr.take().expect("captured test stderr"),
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn observe_foreground(&mut self, identity: TestProcessIdentity) {
        self.foreground =
            Some(PinnedTestProcess::claim(identity).expect("pin foreground fixture identity"));
    }

    fn signal_leader_group(&self, signal: i32) -> std::io::Result<()> {
        let runner_group = unsafe { libc::getpgrp() };
        let identity = same_process(&self.leader)
            .then_some(&self.leader)
            .filter(|identity| identity.session == self.leader.session)
            .filter(|identity| identity.group != runner_group)
            .ok_or_else(|| std::io::Error::other("fixture process-group identity changed"))?;
        // SAFETY: the positive group was revalidated through its marked
        // member, belongs to this test-owned session, and is not the runner.
        if unsafe { libc::kill(-identity.group, signal) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn wait_bounded(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if child_exited_wnowait(self.child.as_ref().expect("retained test child"))
                .expect("inspect Dot exit status")
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.as_mut().expect("retained test child").wait()?;
        self.reaped = true;
        Ok(status)
    }

    fn reap_with_output(&mut self) -> std::io::Result<std::process::Output> {
        let output = self
            .child
            .take()
            .expect("retained test child")
            .wait_with_output()?;
        self.reaped = true;
        Ok(output)
    }

    fn signal_session_members(&self, signal: i32) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            for identity in session_members(self.leader.session) {
                if identity.pid != std::process::id() as i32 {
                    if let Some(process) = PinnedTestProcess::claim(identity) {
                        let _ = process.signal_for_cleanup(signal);
                    }
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            // There is no portable stable handle for arbitrary descendants.
            // The retained leader group is handled separately; do not signal
            // a numeric member observed only through `ps`.
            let _ = signal;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn child_exited_wnowait(child: &std::process::Child) -> std::io::Result<bool> {
    // SAFETY: waitid writes only the initialized local siginfo and WNOWAIT
    // preserves the child identity until fixture descendants are gone.
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

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
impl Drop for TestSessionChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(foreground) = &self.foreground {
            let _ = foreground.signal_for_cleanup(libc::SIGTERM);
        }
        let _ = self.signal_leader_group(libc::SIGTERM);
        self.signal_session_members(libc::SIGTERM);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            let child_done =
                child_exited_wnowait(self.child.as_ref().expect("retained test child"))
                    .unwrap_or(false);
            let members_done = session_members(self.leader.session)
                .into_iter()
                .filter(|identity| identity.pid != self.leader.pid)
                .all(|identity| !process_running_pid(identity.pid));
            if child_done && members_done {
                let _ = self.reap();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        if let Some(foreground) = &self.foreground {
            let _ = foreground.signal_for_cleanup(libc::SIGKILL);
        }
        let _ = self.signal_leader_group(libc::SIGKILL);
        self.signal_session_members(libc::SIGKILL);
        let _ = self.child.as_mut().expect("retained test child").kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            let members_done = session_members(self.leader.session)
                .into_iter()
                .filter(|identity| identity.pid != self.leader.pid)
                .all(|identity| !process_running_pid(identity.pid));
            if child_exited_wnowait(self.child.as_ref().expect("retained test child"))
                .unwrap_or(false)
                && members_done
            {
                break;
            }
            self.signal_session_members(libc::SIGKILL);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if child_exited_wnowait(self.child.as_ref().expect("retained test child")).unwrap_or(false)
        {
            let _ = self.reap();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn spawn_test_session(mut command: Command) -> TestSessionChild {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: setsid is async-signal-safe and runs after fork before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    TestSessionChild::new(command.spawn().expect("guarded test command should start"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_on_pty(mut command: Command) -> (TestSessionChild, File) {
    use std::os::unix::process::CommandExt as _;

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors; the default terminal
    // attributes and window size are sufficient for this job-control test.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                // macOS takes *mut termios/*mut winsize while Linux takes
                // *const; null_mut() satisfies both through coercion.
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: openpty returned two newly owned descriptors above.
    let master = unsafe { File::from_raw_fd(master_fd) };
    // SAFETY: same ownership transfer for the slave descriptor.
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    command
        .stdin(Stdio::from(
            slave.try_clone().expect("clone PTY slave stdin"),
        ))
        .stdout(Stdio::from(
            slave.try_clone().expect("clone PTY slave stdout"),
        ))
        .stderr(Stdio::from(
            slave.try_clone().expect("clone PTY slave stderr"),
        ));
    // SAFETY: after fork and before exec, create a private session and attach
    // descriptor zero's PTY as its controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = TestSessionChild::new(command.spawn().expect("PTY-backed Dot should start"));
    drop(slave);
    // SAFETY: F_GETFL/F_SETFL operate on this valid master descriptor.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "read PTY flags failed");
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "set PTY nonblocking failed"
    );
    (child, master)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_foreground(
    terminal: &File,
    pid: i32,
    session: i32,
    parent_group: i32,
    timeout: std::time::Duration,
) -> Option<TestProcessIdentity> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let identity = process_identity(pid);
        // SAFETY: tcgetpgrp only observes the valid PTY master descriptor.
        let terminal_group = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
        if let Some(identity) = identity.filter(|identity| {
            identity.session == session
                && identity.group != parent_group
                && identity.group == terminal_group
        }) {
            return Some(identity);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_identity_exit(identity: &TestProcessIdentity, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while same_process(identity) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    !same_process(identity)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_nonblocking_to_end(file: &mut File, timeout: std::time::Duration) -> Vec<u8> {
    let deadline = std::time::Instant::now() + timeout;
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return output,
            Ok(count) => output.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    panic!("timed out waiting for fixture stream EOF; bytes={output:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // Linux PTY masters report EIO once the final slave closes.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return output,
            Err(error) => panic!("read fixture stream: {error}"),
        }
    }
}

fn ps_state_running(stdout: &[u8]) -> bool {
    match stdout
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
    {
        None | Some(b'Z') => false,
        Some(_) => true,
    }
}

struct EscapedProcess {
    process: PinnedTestProcess,
    active: bool,
}

impl EscapedProcess {
    fn new(pid: i32) -> Self {
        let identity = process_identity(pid).expect("observe escaped fixture identity");
        assert_eq!(identity.session, identity.pid, "escaped process session");
        assert_eq!(identity.group, identity.pid, "escaped process group");
        Self::from_identity(identity)
    }

    fn in_session(pid: i32, session: i32) -> Self {
        let identity = process_identity(pid).expect("observe fixture process identity");
        assert_eq!(identity.session, session, "fixture process session");
        Self::from_identity(identity)
    }

    fn from_identity(identity: TestProcessIdentity) -> Self {
        Self {
            process: PinnedTestProcess::claim(identity).expect("pin fixture process identity"),
            active: true,
        }
    }

    fn live_in_owned_session(&self) -> bool {
        same_process(&self.process.identity)
            && process_running_pid(self.process.identity.pid)
            && self.process.identity.group != unsafe { libc::getpgrp() }
    }

    fn observe_stopped(&mut self) -> bool {
        let stopped = !self.live_in_owned_session();
        if stopped {
            self.active = false;
        }
        stopped
    }

    fn stop(&mut self) -> bool {
        if self.live_in_owned_session() {
            let _ = self.process.signal_for_cleanup(libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while same_process(&self.process.identity)
            && process_running_pid(self.process.identity.pid)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.observe_stopped()
    }
}

impl Drop for EscapedProcess {
    fn drop(&mut self) {
        if self.active {
            let _ = self.stop();
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn process_running_pid(pid: i32) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("could not inspect process {pid}: {error}"));
    if output.status.success() {
        ps_state_running(&output.stdout)
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

fn assert_provider_signal(
    signal: i32,
    expected: i32,
) -> (TestProcessIdentity, TestProcessIdentity) {
    let fixture = Fixture::new("shdeps-provider-signal");
    let pid_file = fixture.home.join("provider-pid");
    let descendant_file = fixture.home.join("provider-descendant-pid");
    let signal_file = fixture.home.join("provider-signal");
    let merge_marker = fixture.home.join("merge-after-cancellation");
    let tmp = fixture.home.join("tmp");
    std::fs::create_dir(&tmp).expect("temporary directory");
    let extensions = fixture.home.join("extensions");
    let merge_hooks = extensions.join("merge-hooks.d");
    std::fs::create_dir_all(&merge_hooks).expect("merge hooks");
    std::fs::write(
        fixture.home.join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=shdeps\nshdeps_update_policy=pinned\n",
    )
    .expect("signal config");
    std::fs::write(
        merge_hooks.join("10-later.sh"),
        b"merge() { printf ran >\"$HOME/merge-after-cancellation\"; }\n",
    )
    .expect("merge hook");
    std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o700))
        .expect("extensions mode");
    std::fs::set_permissions(&merge_hooks, std::fs::Permissions::from_mode(0o700))
        .expect("merge hooks mode");
    std::fs::set_permissions(
        merge_hooks.join("10-later.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("merge hook mode");
    let mut command = fixture.command();
    command
        .env("DOT_TEST_PROVIDER_HANG", "1")
        .env("DOT_TEST_PROVIDER_HANG_PID", &pid_file)
        .env("DOT_TEST_PROVIDER_HANG_DESCENDANT_PID", &descendant_file)
        .env("DOT_TEST_PROVIDER_SIGNAL_RECORD", &signal_file)
        .env("TMPDIR", &tmp);
    let mut child = spawn_test_session(command);
    let Some(provider_pid) = wait_for_pid(&pid_file, 15) else {
        panic!("provider did not publish its pid");
    };
    let provider_identity = process_identity(provider_pid).expect("observe provider identity");
    let provider_session = provider_identity.session;
    let mut provider = EscapedProcess::from_identity(provider_identity);
    let Some(descendant_pid) = wait_for_pid(&descendant_file, 15) else {
        panic!("provider did not publish its descendant pid");
    };
    let mut descendant = EscapedProcess::in_session(descendant_pid, provider_session);
    // The retained, unreaped child keeps this positive PID from being reused.
    assert_eq!(unsafe { libc::kill(child.id(), signal) }, 0);
    assert!(
        child.wait_bounded(std::time::Duration::from_secs(5)),
        "dot update did not finish after signal {signal}"
    );
    let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while (!provider.observe_stopped() || !descendant.observe_stopped())
        && std::time::Instant::now() < cleanup_deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let provider_survived = !provider.observe_stopped();
    let descendant_survived = !descendant.observe_stopped();
    if provider_survived {
        provider.stop();
    }
    if descendant_survived {
        descendant.stop();
    }
    let output = child.reap_with_output().expect("dot output");
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !provider_survived,
        "provider {provider_pid} survived cancellation"
    );
    assert!(
        !descendant_survived,
        "provider descendant {descendant_pid} survived cancellation"
    );
    assert_eq!(
        std::fs::read(&signal_file).expect("delivered signal marker"),
        b"TERM\n",
        "provider received the parent signal instead of cleanup TERM"
    );
    assert!(
        !merge_marker.exists(),
        "update started a merge hook after provider cancellation"
    );
    assert_eq!(
        std::fs::read_dir(&tmp)
            .expect("temporary directory")
            .count(),
        0,
        "provider cancellation left prompt or capture state"
    );
    assert!(
        !output
            .stdout
            .windows(b"[3/5] Configs".len())
            .any(|window| window == b"[3/5] Configs"),
        "update advanced to the merge stage after provider cancellation: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    (
        provider.process.identity.clone(),
        descendant.process.identity.clone(),
    )
}

const CHANGED: &[u8] =
    b"[1/5] Overlays   running  checking overlay links                         Ns\n\
[1/5] Overlays   ok       0 overlays current                             Ns\n\
[2/5] Tools      running  checking configured dependencies               Ns\n\
[2/5] Tools      changed  1 changed                                      Ns\n\
\x20\x20changed  Cargo: 1 changed\n\
\x20\x20changed  ripgrep                      installed\n\
[3/5] Configs    running  checking config hooks                          Ns\n\
[3/5] Configs    ok       no config hooks                                Ns\n\
[4/5] Cleanup    running  normalizing worktree                           Ns\n\
[4/5] Cleanup    ok       no base repo                                   Ns\n\
Done in Ns. Reload your shell: source ~/.bashrc\n";

const UNAVAILABLE: &[u8] =
    b"[1/5] Overlays   running  checking overlay links                         Ns\n\
[1/5] Overlays   ok       0 overlays current                             Ns\n\
[2/5] Tools      running  checking configured dependencies               Ns\n\
[2/5] Tools      failed   shdeps unavailable; dependency install ski     Ns\n\
[3/5] Configs    running  checking config hooks                          Ns\n\
[3/5] Configs    ok       no config hooks                                Ns\n\
[4/5] Cleanup    running  normalizing worktree                           Ns\n\
[4/5] Cleanup    ok       no base repo                                   Ns\n\
Done with errors in Ns. Reload your shell: source ~/.bashrc\n";

const FAILED_UPDATE: &[u8] =
    b"[1/5] Overlays   running  checking overlay links                         Ns\n\
[1/5] Overlays   ok       0 overlays current                             Ns\n\
[2/5] Tools      running  checking configured dependencies               Ns\n\
[2/5] Tools      failed   1 failed                                       Ns\n\
\x20\x20failed   Cargo: 1 failed, 12ms\n\
\x20\x20failed   ripgrep                      network unavailable\n\
[3/5] Configs    running  checking config hooks                          Ns\n\
[3/5] Configs    ok       no config hooks                                Ns\n\
[4/5] Cleanup    running  normalizing worktree                           Ns\n\
[4/5] Cleanup    ok       no base repo                                   Ns\n\
Done with errors in Ns. Reload your shell: source ~/.bashrc\n";

const CHECKED: &[u8] =
    b"[1/5] Overlays   running  checking overlay links                         Ns\n\
[1/5] Overlays   ok       0 overlays current                             Ns\n\
[2/5] Tools      running  checking configured dependencies               Ns\n\
[2/5] Tools      ok       dependencies checked                           Ns\n\
[3/5] Configs    running  checking config hooks                          Ns\n\
[3/5] Configs    ok       no config hooks                                Ns\n\
[4/5] Cleanup    running  normalizing worktree                           Ns\n\
[4/5] Cleanup    ok       no base repo                                   Ns\n\
Done in Ns. Reload your shell: source ~/.bashrc\n";

fn assert_cli(output: &Output, status: i32, stdout: &[u8], stderr: &[u8]) {
    assert_eq!(
        output.status.code(),
        Some(status),
        "stdout={:?}\nstderr={:?}",
        output.stdout,
        output.stderr
    );
    assert_eq!(normalize_elapsed(&output.stdout), stdout);
    assert_eq!(output.stderr, stderr);
}

#[test]
fn bounded_output_reaps_a_timed_out_process_session() {
    let scratch = TempDir::new("shdeps-provider-timeout").expect("scratch");
    let script = scratch.path().join("hang");
    let leader = scratch.path().join("leader-pid");
    let descendant = scratch.path().join("descendant-pid");
    write_exec(
        &script,
        br#"#!/usr/bin/env bash
printf '%s\n' "$$" >"$LEADER_PID"
(sleep 30) &
printf '%s\n' "$!" >"$DESCENDANT_PID"
wait
"#,
    );
    let mut command = Command::new(&script);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LEADER_PID", &leader)
        .env("DESCENDANT_PID", &descendant)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = bounded_output(command, 1);
    assert_eq!(
        output.status.code(),
        Some(124),
        "stderr={:?}",
        output.stderr
    );
    for pid_file in [&leader, &descendant] {
        let pid = std::fs::read_to_string(pid_file)
            .expect("timeout fixture pid")
            .trim()
            .to_string();
        assert!(!process_running(&pid), "timed-out process {pid} survived");
    }
}

#[test]
fn provider_conventional_signal_statuses_stop_the_update() {
    for (signal, code) in [
        (libc::SIGHUP, 129),
        (libc::SIGINT, 130),
        (libc::SIGQUIT, 131),
        (libc::SIGTERM, 143),
    ] {
        let fixture = Fixture::new(&format!("shdeps-provider-exit-{signal}"));
        let output = fixture
            .command()
            .env("DOT_TEST_PROVIDER_EXIT_CODE", code.to_string())
            .output()
            .expect("provider conventional signal exit");

        assert_eq!(
            output.status.code(),
            Some(code),
            "provider status {code} was not propagated: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output
                .stdout
                .windows(b"[3/5] Configs".len())
                .any(|window| window == b"[3/5] Configs"),
            "update continued after provider interruption {code}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !output
                .stdout
                .windows(b"[4/5] Cleanup".len())
                .any(|window| window == b"[4/5] Cleanup"),
            "cleanup ran after provider interruption {code}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
extern "C" fn foreground_interrupt_exit(_signal: libc::c_int) {
    // SAFETY: _exit is async-signal-safe and preserves the conventional
    // provider cancellation status without running test-harness destructors.
    unsafe { libc::_exit(130) }
}

/// Helper mode for the composed PTY test below. The separate process takes a
/// real foreground-terminal lease while Dot remains in its original group.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn provider_foreground_interrupt_helper() {
    if std::env::var_os("DOT_TEST_PROVIDER_FOREGROUND_HELPER_MODE").is_none() {
        return;
    }

    // SAFETY: this helper is a dedicated process. It owns its process-group
    // transition and ignores SIGTTOU before changing the foreground lease.
    assert_eq!(
        unsafe { libc::setpgid(0, 0) },
        0,
        "set helper process group"
    );
    assert_ne!(
        unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) },
        libc::SIG_ERR,
        "ignore SIGTTOU"
    );
    let terminal = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    assert!(
        terminal >= 0,
        "open controlling terminal: {}",
        std::io::Error::last_os_error()
    );
    let group = unsafe { libc::getpgrp() };
    assert_eq!(
        unsafe { libc::tcsetpgrp(terminal, group) },
        0,
        "give helper the foreground terminal: {}",
        std::io::Error::last_os_error()
    );
    assert_ne!(
        unsafe {
            libc::signal(
                libc::SIGINT,
                foreground_interrupt_exit as *const () as libc::sighandler_t,
            )
        },
        libc::SIG_ERR,
        "install SIGINT handler"
    );
    std::fs::write(
        std::env::var_os("DOT_TEST_PROVIDER_FOREGROUND_PID").expect("foreground helper pid marker"),
        format!("{}\n", std::process::id()),
    )
    .expect("publish foreground helper pid");

    loop {
        // SAFETY: pause has no pointer arguments; the SIGINT handler exits.
        unsafe { libc::pause() };
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn provider_foreground_interrupt_propagates_without_signaling_dot() {
    let fixture = Fixture::new("shdeps-provider-foreground-interrupt");
    let foreground_pid_file = fixture.home.join("provider-foreground-pid");
    let foreground_helper = std::env::current_exe().expect("provider foreground helper binary");
    let mut command = fixture.command();
    command
        .env("DOT_TEST_PROVIDER_FOREGROUND_INTERRUPT", "1")
        .env("DOT_TEST_PROVIDER_FOREGROUND_HELPER", foreground_helper)
        .env("DOT_TEST_PROVIDER_FOREGROUND_HELPER_MODE", "1")
        .env("DOT_TEST_PROVIDER_FOREGROUND_PID", &foreground_pid_file);
    let (mut dot, mut terminal) = spawn_on_pty(command);
    let dot_identity = process_identity(dot.id()).expect("observe PTY-backed Dot identity");
    assert_eq!(
        dot_identity, dot.leader,
        "Dot was not the leader of its private test session"
    );
    let foreground_pid = wait_for_pid(&foreground_pid_file, 10).expect("foreground provider pid");
    let foreground = wait_for_foreground(
        &terminal,
        foreground_pid,
        dot.leader.session,
        dot.leader.group,
        std::time::Duration::from_secs(10),
    )
    .expect("provider child did not own the terminal foreground group");
    dot.observe_foreground(foreground.clone());

    terminal
        .write_all(b"\x03")
        .expect("send the terminal VINTR character");
    assert!(
        dot.wait_bounded(std::time::Duration::from_secs(10)),
        "Dot did not exit after the provider foreground interrupt"
    );
    let output = read_nonblocking_to_end(&mut terminal, std::time::Duration::from_secs(5));
    let foreground_gone = wait_for_identity_exit(&foreground, std::time::Duration::from_secs(5));
    let status = dot.reap().expect("reap PTY-backed Dot");

    assert_eq!(
        status.code(),
        Some(130),
        "foreground SIGINT was not propagated: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        foreground_gone,
        "provider foreground child survived Dot's exit: {foreground:?}"
    );
    for phase in [b"[3/5] Configs".as_slice(), b"[4/5] Cleanup"] {
        assert!(
            !output.windows(phase.len()).any(|window| window == phase),
            "update advanced to {} after provider interruption: {}",
            String::from_utf8_lossy(phase),
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn provider_hup_cancellation_reaps_session() {
    let _ = assert_provider_signal(libc::SIGHUP, 129);
}

#[test]
fn provider_int_cancellation_reaps_session() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        const HELPER: &str = "DOT_TEST_PROVIDER_SUBREAPER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            #[cfg(target_os = "linux")]
            let executable = PathBuf::from("/proc/self/exe");
            #[cfg(target_os = "android")]
            let executable =
                PathBuf::from(std::env::args_os().next().expect("test executable argv[0]"));
            let output = Command::new(executable)
                .args([
                    "--exact",
                    "provider_int_cancellation_reaps_session",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .expect("subreaper test helper");
            assert!(
                output.status.success(),
                "subreaper helper failed with {:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Keep any descendant Dot fails to reap parented to this helper so a
        // zombie cannot disappear into PID 1 and make the assertion vacuous.
        // SAFETY: PR_SET_CHILD_SUBREAPER accepts the integer enable flag.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    }
    // Underscored: only the Linux/Android identity comparison below reads
    // the pair; every platform still runs the cancellation helper itself.
    let (_provider, _descendant) = assert_provider_signal(libc::SIGINT, 130);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let provider_exists = same_process(&_provider);
        let descendant_exists = same_process(&_descendant);
        for identity in [&_provider, &_descendant] {
            if same_process(identity) {
                // SAFETY: a matching generation is still an unreaped child of
                // this subreaper, so its positive PID cannot be recycled.
                unsafe { libc::waitpid(identity.pid, std::ptr::null_mut(), libc::WNOHANG) };
            }
        }
        assert!(!provider_exists, "provider was not reaped");
        assert!(!descendant_exists, "provider descendant was not reaped");
    }
}

#[test]
fn provider_quit_cancellation_reaps_session() {
    let _ = assert_provider_signal(libc::SIGQUIT, 131);
}

#[test]
fn provider_term_cancellation_reaps_session() {
    let _ = assert_provider_signal(libc::SIGTERM, 143);
}

#[test]
fn ps_state_requires_a_non_zombie_row() {
    for absent in [b"".as_slice(), b" \n".as_slice(), b"Z+\n".as_slice()] {
        assert!(!ps_state_running(absent), "state={absent:?}");
    }
    assert!(ps_state_running(b"S+\n"));
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
#[test]
fn provider_fixture_rejects_a_stale_process_generation() {
    let current = process_identity(std::process::id() as i32).expect("current process identity");
    let mut stale = current.clone();
    stale.generation.push(b'x');

    assert!(same_process(&current));
    assert!(!same_process(&stale));
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn provider_cleanup_handle_suppresses_reused_generation_delivery() {
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
    let identity = process_identity(child.id() as i32).expect("fixture identity");
    let mut process = PinnedTestProcess::claim(identity).expect("pin fixture identity");
    process.identity.generation.push(b'x');

    assert!(!process.signal_for_cleanup(libc::SIGKILL));
    assert_eq!(child.try_wait().expect("observe fixture"), None);
    child.kill().expect("stop retained fixture child");
    child.wait().expect("reap retained fixture child");
}

fn assert_preparation_signal(stage: &str) {
    let rust = Fixture::new(&format!("shdeps-provider-signal-{stage}"));
    let helper = rust.signal_helper();
    let pid_file = rust.home.join("preparation-signal-pid");
    let leader_file = rust.home.join("preparation-signal-leader-pid");
    let signal_file = rust.home.join("preparation-signal-record");
    let signal_release = rust.home.join("preparation-signal-release");
    let curl_record = rust.home.join("preparation-curl-record");
    let merge_marker = rust.home.join("merge-after-preparation-cancellation");
    let tmp = rust.home.join("tmp");
    std::fs::create_dir(&tmp).expect("temporary directory");
    let extensions = rust.home.join("extensions");
    let merge_hooks = extensions.join("merge-hooks.d");
    std::fs::create_dir_all(&merge_hooks).expect("merge hooks");
    std::fs::write(
        rust.home.join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=shdeps\nshdeps_update_policy=pinned\n",
    )
    .expect("signal config");
    std::fs::write(
        merge_hooks.join("10-later.sh"),
        b"merge() { printf ran >\"$HOME/merge-after-preparation-cancellation\"; }\n",
    )
    .expect("merge hook");
    std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o700))
        .expect("extensions mode");
    std::fs::set_permissions(&merge_hooks, std::fs::Permissions::from_mode(0o700))
        .expect("merge hooks mode");
    std::fs::set_permissions(
        merge_hooks.join("10-later.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("merge hook mode");
    let mut command = rust.command();
    command
        .env("TMPDIR", &tmp)
        .env("DOT_TEST_PROVIDER_SIGNAL_HELPER", &helper)
        .env("DOT_TEST_PROVIDER_SIGNAL_PID", &pid_file)
        .env("DOT_TEST_PROVIDER_SIGNAL_RECORD", &signal_file)
        .env("DOT_TEST_PROVIDER_SIGNAL_RELEASE", &signal_release)
        .env("DOT_TEST_PROVIDER_SIGNAL_LEADER_PID", &leader_file);
    match stage {
        "download" => {
            command
                .env_remove("SHDEPS_LIB")
                .env("SHDEPS_DIR", rust.home.join("missing-provider"))
                .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-development"))
                .env("PATH", rust.signal_curl_path())
                .env("DOT_TEST_CURL_RECORD", &curl_record)
                .env("_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS", "30");
        }
        "bootstrap" => {
            command
                .env("DOT_TEST_PROVIDER_SIGNAL_BOOTSTRAP", "1")
                .env("DOT_TEST_PROVIDER_BOOTSTRAP_DIAGNOSTIC", "1");
        }
        "abi" => {
            command.env("DOT_TEST_PROVIDER_SIGNAL_ABI", "1");
        }
        _ => panic!("unknown preparation stage"),
    }
    let mut child = spawn_test_session(command);
    let Some(pid) = wait_for_pid(&pid_file, 10) else {
        panic!("{stage} helper did not publish its pid");
    };
    let helper_identity = process_identity(pid).expect("observe preparation helper identity");
    let mut helper = EscapedProcess::from_identity(helper_identity);
    let leader_pid = wait_for_pid(&leader_file, if stage == "bootstrap" { 10 } else { 0 });
    if stage == "bootstrap" {
        assert!(
            leader_pid.is_some(),
            "bootstrap did not publish its supervised session leader"
        );
    }
    let mut leader = leader_pid
        .and_then(process_identity)
        .map(EscapedProcess::from_identity);
    std::fs::write(&signal_release, b"release\n").expect("release signal helper");
    let completed = child.wait_bounded(std::time::Duration::from_secs(5));
    let helper_survived = !helper.observe_stopped();
    let leader_survived = leader
        .as_mut()
        .is_some_and(|leader| !leader.observe_stopped());
    if helper_survived {
        helper.stop();
    }
    if leader_survived {
        leader.as_mut().expect("live preparation leader").stop();
    }
    assert!(completed, "Dot did not promptly cancel Shdeps {stage}");
    let output = child
        .reap_with_output()
        .expect("provider preparation output");

    assert!(
        !helper_survived,
        "Shdeps {stage} helper survived cancellation"
    );
    assert!(
        !leader_survived,
        "Shdeps {stage} session leader survived cancellation"
    );
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .windows(b"checking overlay links".len())
            .any(|window| { window == b"checking overlay links" }),
        "completed pre-preparation output was lost on {stage} cancellation: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !output
            .stdout
            .windows(b"checking configured dependencies".len())
            .any(|window| window == b"checking configured dependencies"),
        "Tools stage began before {stage} preparation completed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    if stage == "bootstrap" {
        assert!(
            output
                .stderr
                .windows(b"bootstrap diagnostic".len())
                .any(|window| window == b"bootstrap diagnostic"),
            "pre-signal bootstrap diagnostic was lost: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        std::fs::read(&signal_file).expect("preparation TERM marker"),
        format!("{}\n", libc::SIGTERM).as_bytes()
    );
    assert!(
        !rust.home.join("provider-record").exists(),
        "provider update ran after {stage} cancellation"
    );
    assert!(
        !merge_marker.exists(),
        "merge ran after {stage} cancellation"
    );
    for row in [b"[3/5] Configs".as_slice(), b"[4/5] Cleanup", b"Done "] {
        assert!(
            !output.stdout.windows(row.len()).any(|window| window == row),
            "update emitted a later row after {stage} cancellation: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    assert!(
        !rust.state.join("dot/update.lock.d").exists(),
        "update lock survived {stage} cancellation"
    );
    if stage == "abi" {
        assert!(
            !output
                .stderr
                .windows(b"timed out".len())
                .any(|window| window == b"timed out"),
            "ABI cancellation was misreported as timeout"
        );
    }
    if stage == "download" {
        assert_eq!(
            std::fs::read(&curl_record).expect("curl attempt record"),
            b"attempt\n",
            "download retried after cancellation"
        );
    }
    assert_eq!(
        std::fs::read_dir(&tmp)
            .expect("temporary directory")
            .count(),
        0,
        "Shdeps {stage} left temporary captures"
    );
}

#[test]
fn signal_cancels_provider_download() {
    assert_preparation_signal("download");
}

#[test]
fn signal_cancels_provider_bootstrap() {
    assert_preparation_signal("bootstrap");
}

#[test]
fn signal_cancels_provider_abi_probe() {
    assert_preparation_signal("abi");
}

#[test]
fn signal_cancels_provider_download_retry_delay() {
    let rust = Fixture::new("shdeps-provider-signal-download-delay");
    let curl_record = rust.home.join("download-delay-curl-record");
    let curl_pid_file = rust.home.join("download-delay-curl-pid");
    let tmp = rust.home.join("tmp");
    std::fs::create_dir(&tmp).expect("temporary directory");
    let mut command = rust.command();
    command
        .env("TMPDIR", &tmp)
        .env_remove("SHDEPS_LIB")
        .env("SHDEPS_DIR", rust.home.join("missing-provider"))
        .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-development"))
        .env("PATH", rust.failing_curl_path())
        .env("DOT_TEST_CURL_RECORD", &curl_record)
        .env("DOT_TEST_CURL_PID", &curl_pid_file)
        .env("_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS", "30");
    let mut child = spawn_test_session(command);
    let Some(curl_pid) = wait_for_pid(&curl_pid_file, 10) else {
        panic!("curl did not publish its pid");
    };
    let curl_identity = process_identity(curl_pid);
    let curl_exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while curl_identity.as_ref().is_some_and(same_process)
        && std::time::Instant::now() < curl_exit_deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !curl_identity.as_ref().is_some_and(same_process),
        "curl was still running before retry-delay cancellation"
    );
    assert!(
        wait_for_nonempty(&curl_record, 0),
        "curl did not record the first failed attempt"
    );
    // The retained, unreaped child keeps this positive PID from being reused.
    assert_eq!(unsafe { libc::kill(child.id(), libc::SIGINT) }, 0);
    let completed = child.wait_bounded(std::time::Duration::from_secs(5));

    assert!(
        completed,
        "Dot did not promptly cancel the download retry delay"
    );
    let output = child.reap_with_output().expect("download delay output");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&curl_record).expect("curl attempt record"),
        b"attempt\n",
        "download retried after retry-delay cancellation"
    );
    assert_eq!(
        std::fs::read_dir(&tmp)
            .expect("temporary directory")
            .count(),
        0,
        "download retry cancellation left its temporary installer"
    );
    assert!(
        !rust.state.join("dot/update.lock.d").exists(),
        "update lock survived download retry cancellation"
    );
}

#[test]
fn provider_download_combined_output_limit_is_bounded_and_aborts_update() {
    let rust = Fixture::new("shdeps-provider-download-output-budget");
    let curl_pid_file = rust.home.join("download-output-curl-pid");
    let stdout_path = rust.home.join("download-output-stdout");
    let output_path = rust.home.join("download-output-stderr");
    let stdout_file = std::fs::File::create(&stdout_path).expect("download stdout file");
    let output_file = std::fs::File::create(&output_path).expect("download stderr file");
    let mut command = rust.command();
    command
        .env_remove("SHDEPS_LIB")
        .env("SHDEPS_DIR", rust.home.join("missing-provider"))
        .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-development"))
        .env("PATH", rust.adversarial_curl_path())
        .env("DOT_TEST_CURL_PID", &curl_pid_file)
        .env("DOT_TEST_CURL_MODE", "flood")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(output_file));
    let mut child = spawn_test_session(command);
    let Some(curl_pid) = wait_for_pid(&curl_pid_file, 10) else {
        panic!("flooding curl did not publish its pid");
    };
    let mut curl = EscapedProcess::new(curl_pid);
    let started = std::time::Instant::now();
    let completed = child.wait_bounded(std::time::Duration::from_secs(8));
    if !completed {
        let _ = child.signal_leader_group(libc::SIGKILL);
        let _ = curl.stop();
    }
    let status = child.reap().expect("download overflow status");
    let stdout = std::fs::read(&stdout_path).expect("download stdout");
    let stderr = std::fs::read(&output_path).expect("download stderr");

    assert!(completed, "download output overflow was not bounded");
    assert_eq!(
        status.code(),
        Some(1),
        "unexpected download timeout status; stderr={}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert_eq!(
        stderr
            .windows(b"Shdeps provider output exceeded its safety limit".len())
            .filter(|window| *window == b"Shdeps provider output exceeded its safety limit")
            .count(),
        1,
        "download limit diagnostic was not stable: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(curl.observe_stopped(), "flooding curl survived teardown");
    assert!(
        !rust.home.join("provider-record").exists(),
        "provider ran after download overflow"
    );
    assert!(
        !stdout
            .windows(b"[3/5] Configs".len())
            .any(|window| window == b"[3/5] Configs"),
        "update advanced after download overflow"
    );
}

#[test]
fn provider_download_has_an_absolute_supervision_deadline() {
    let rust = Fixture::new("shdeps-provider-download-deadline");
    let curl_pid_file = rust.home.join("download-deadline-curl-pid");
    let stdout_path = rust.home.join("download-deadline-stdout");
    let output_path = rust.home.join("download-deadline-stderr");
    let stdout_file = std::fs::File::create(&stdout_path).expect("download stdout file");
    let output_file = std::fs::File::create(&output_path).expect("download stderr file");
    let mut command = rust.command();
    command
        .env_remove("SHDEPS_LIB")
        .env("SHDEPS_DIR", rust.home.join("missing-provider"))
        .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-development"))
        .env("PATH", rust.adversarial_curl_path())
        .env("DOT_TEST_CURL_PID", &curl_pid_file)
        .env("DOT_TEST_CURL_MODE", "stall")
        .env("_DOT_SHDEPS_DOWNLOAD_TIMEOUT_SECONDS", "1")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(output_file));
    let mut child = spawn_test_session(command);
    let Some(curl_pid) = wait_for_pid(&curl_pid_file, 10) else {
        panic!("stalled curl did not publish its pid");
    };
    let mut curl = EscapedProcess::new(curl_pid);
    let started = std::time::Instant::now();
    let completed = child.wait_bounded(std::time::Duration::from_secs(8));
    if !completed {
        let _ = child.signal_leader_group(libc::SIGKILL);
        let _ = curl.stop();
    }
    let status = child.reap().expect("download deadline status");
    let stdout = std::fs::read(&stdout_path).expect("download stdout");
    let stderr = std::fs::read(&output_path).expect("download stderr");

    assert!(completed, "download ignored its absolute deadline");
    assert_eq!(
        status.code(),
        Some(1),
        "unexpected backpressured download status; stderr={}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert!(curl.observe_stopped(), "stalled curl survived teardown");
    assert!(
        String::from_utf8_lossy(&stderr).contains("provider download timed out after 1s"),
        "missing stable timeout diagnostic: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        !rust.home.join("provider-record").exists(),
        "provider ran after download timeout"
    );
    assert!(
        !stdout
            .windows(b"[3/5] Configs".len())
            .any(|window| window == b"[3/5] Configs"),
        "update advanced after download timeout"
    );
}

#[test]
fn provider_download_deadline_interrupts_an_undrained_cli_pipe() {
    let rust = Fixture::new("shdeps-provider-download-pipe-deadline");
    let curl_pid_file = rust.home.join("download-pipe-curl-pid");
    let flood_ready = rust.home.join("download-pipe-flood-ready");
    let output_path = rust.home.join("download-pipe-stderr");
    let output_file = std::fs::File::create(&output_path).expect("download stderr file");
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("blocked stdout pair");
    let mut command = rust.command();
    command
        .env_remove("SHDEPS_LIB")
        .env("SHDEPS_DIR", rust.home.join("missing-provider"))
        .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-development"))
        .env("PATH", rust.adversarial_curl_path())
        .env("DOT_TEST_CURL_PID", &curl_pid_file)
        .env("DOT_TEST_CURL_MODE", "backpressure-timeout")
        .env("DOT_TEST_CURL_FLOOD_READY", &flood_ready)
        .env("_DOT_SHDEPS_DOWNLOAD_TIMEOUT_SECONDS", "3")
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::from(output_file));
    let mut child = spawn_test_session(command);
    let Some(curl_pid) = wait_for_pid(&curl_pid_file, 10) else {
        panic!("backpressured curl did not publish its pid");
    };
    let mut curl = EscapedProcess::new(curl_pid);
    assert!(
        wait_for_nonempty(&flood_ready, 10),
        "curl did not finish publishing its bounded flood"
    );
    let queued_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut queued = 0;
    while std::time::Instant::now() < queued_deadline {
        // SAFETY: `reader` owns a connected Unix stream and FIONREAD writes
        // one initialized integer with its currently queued byte count.
        let result = unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &mut queued) };
        assert_eq!(result, 0, "inspect blocked output pipe");
        if queued >= 4 * 1024 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        queued >= 4 * 1024,
        "Dot never filled the undrained output pipe (queued {queued} bytes)"
    );
    assert!(
        !child.wait_bounded(std::time::Duration::ZERO),
        "Dot exited before its undrained sink could exercise the deadline"
    );

    let started = std::time::Instant::now();
    let completed = child.wait_bounded(std::time::Duration::from_secs(6));
    if !completed {
        let _ = child.signal_leader_group(libc::SIGKILL);
        let _ = curl.stop();
    }
    let status = child.reap().expect("download pipe deadline status");
    drop(reader);
    let stderr = std::fs::read(&output_path).expect("download timeout stderr");

    assert!(
        completed,
        "download deadline was trapped behind an outward write for {:?}",
        started.elapsed()
    );
    assert_eq!(
        status.code(),
        Some(1),
        "unexpected backpressured download status; stderr={}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(
        curl.observe_stopped(),
        "backpressured curl survived teardown"
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("provider download timed out after 3s"),
        "missing stable timeout result: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        !rust.home.join("provider-record").exists(),
        "provider ran after the download timeout"
    );
}

#[test]
fn signal_during_completed_provider_teardown_stops_later_update_work() {
    let rust = Fixture::new("shdeps-provider-teardown-signal");
    let pid_file = rust.home.join("teardown-descendant-pid");
    let signal_file = rust.home.join("teardown-descendant-signal");
    let merge_marker = rust.home.join("merge-after-cancellation");
    let bootstrap_record = rust.home.join("bootstrap-record");
    let extensions = rust.home.join("extensions");
    let merge_hooks = extensions.join("merge-hooks.d");
    std::fs::create_dir_all(&merge_hooks).expect("merge hooks");
    std::fs::write(
        rust.home.join(".config/dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\ndependency_provider=shdeps\nshdeps_update_policy=pinned\n",
    )
    .expect("signal config");
    std::fs::write(
        merge_hooks.join("10-later.sh"),
        b"merge() { printf ran >\"$HOME/merge-after-cancellation\"; }\n",
    )
    .expect("merge hook");
    std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o700))
        .expect("extensions mode");
    std::fs::set_permissions(&merge_hooks, std::fs::Permissions::from_mode(0o700))
        .expect("merge hooks mode");
    std::fs::set_permissions(
        merge_hooks.join("10-later.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("merge hook mode");
    let before = git_head(&rust.root);
    let output = bounded_output(
        {
            let mut command = rust.command();
            command
                .env("DOT_TEST_PROVIDER_SIGNAL_DURING_TEARDOWN", "1")
                .env("DOT_TEST_PROVIDER_TEARDOWN_PID", &pid_file)
                .env("DOT_TEST_PROVIDER_TEARDOWN_SIGNAL", &signal_file)
                .env("DOT_TEST_PROVIDER_ADVANCE_SOURCE", "1")
                .env(
                    "DOT_TEST_PROVIDER_ADVANCED",
                    rust.home.join("provider-advanced"),
                )
                .env("DOT_TEST_PROVIDER_BOOTSTRAP_RECORD", &bootstrap_record);
            command
        },
        10,
    );
    let pid = wait_for_pid(&pid_file, 1).expect("teardown descendant pid");

    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!process_running_pid(pid), "teardown descendant survived");
    assert_eq!(
        std::fs::read(&signal_file).expect("teardown TERM marker"),
        b"TERM\n"
    );
    assert_eq!(
        std::fs::read(&bootstrap_record).expect("bootstrap invocation record"),
        b"0\n",
        "provider re-executed after teardown latched SIGINT"
    );
    assert!(
        rust.home.join("provider-advanced").exists(),
        "provider did not reach its source-change boundary"
    );
    assert_ne!(
        before,
        git_head(&rust.root),
        "provider did not advance source"
    );
    assert!(
        !rust.state.join("dot/provider-reexec-failed").exists(),
        "provider cancellation created a re-exec checkpoint"
    );
    assert!(!merge_marker.exists(), "merge ran after teardown signal");
    for row in [b"[3/5] Configs".as_slice(), b"[4/5] Cleanup", b"Done "] {
        assert!(
            !output.stdout.windows(row.len()).any(|window| window == row),
            "update emitted a later row after teardown cancellation: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn elapsed_normalization_changes_only_ui_elapsed_positions() {
    let input = b"[1/5] Tools ok 2 current 10s\nDone in 2s\nDone with errors in 3s\nDone in 4s.\nDone with errors in 5s. Reload your shell: source ~/.bashrc\nDone in 6s. Reload your shell: source ~/.zshrc\n[hook diagnostic] 10s\n[one/5] malformed 8s\nretry after 10s\ncount=9s\xff\n";
    assert_eq!(
        normalize_elapsed(input),
        b"[1/5] Tools ok 2 current Ns\nDone in Ns\nDone with errors in Ns\nDone in Ns.\nDone with errors in Ns. Reload your shell: source ~/.bashrc\nDone in Ns. Reload your shell: source ~/.zshrc\n[hook diagnostic] 10s\n[one/5] malformed 8s\nretry after 10s\ncount=9s\xff\n"
    );
}

#[test]
fn explicit_reviewed_provider_runs_natively() {
    let rust = Fixture::new("shdeps-provider-explicit");
    // A process launched directly from the native executable does not inherit
    // Bash's shell-local BASH variable. With no strict DOT_BASH override, the
    // provider must resolve its retained bootstrap interpreter from PATH.
    let rust_output = rust
        .command()
        .env_remove("DOT_BASH")
        .env_remove("BASH")
        .output()
        .expect("run native provider without BASH");
    assert_cli(&rust_output, 0, CHANGED, b"");
    assert_eq!(
        std::fs::read(rust.home.join("provider-record")).expect("native provider record"),
        b"force=0 quiet=0 nested=1 jobs=2\n"
    );
}

#[test]
fn invalid_explicit_bash_reports_the_resolver_failure() {
    let rust = Fixture::new("shdeps-provider-invalid-bash");
    let missing = rust.home.join("missing/bash");
    let output = rust
        .command()
        .env("DOT_BASH", &missing)
        .output()
        .expect("run provider with invalid DOT_BASH");
    let expected = format!(
        "checkout Bash resolver: explicit interpreter is not Bash 4 or newer: {}\n",
        missing.display()
    );

    assert_cli(&output, 1, UNAVAILABLE, expected.as_bytes());
}

#[test]
fn invalid_explicit_bash_does_not_download_an_installer() {
    let rust = Fixture::new("shdeps-provider-invalid-bash-no-download");
    let missing_bash = rust.home.join("missing/bash");
    let missing_provider = rust.home.join("missing/provider");
    let curl_record = rust.home.join("curl-record");
    let output = rust
        .command()
        .env("DOT_BASH", &missing_bash)
        .env_remove("SHDEPS_LIB")
        .env("SHDEPS_DIR", &missing_provider)
        .env("PATH", rust.curl_path())
        .env("DOT_TEST_CURL_RECORD", &curl_record)
        .output()
        .expect("run provider with invalid DOT_BASH");
    let expected = format!(
        "checkout Bash resolver: explicit interpreter is not Bash 4 or newer: {}\n",
        missing_bash.display()
    );

    assert_cli(&output, 1, UNAVAILABLE, expected.as_bytes());
    assert!(
        !curl_record.exists(),
        "invalid Bash must fail before provider download"
    );
}

#[test]
fn closed_provider_path_contains_native_prerequisites_without_jq() {
    let rust = Fixture::new("shdeps-provider-closed-path");
    let path = PathBuf::from(rust.closed_tool_path());

    for name in ["bash", "git", "id", "mv"] {
        assert!(
            path.join(name).is_file(),
            "closed provider PATH omitted required {name}"
        );
    }
    assert!(
        path.join("sha256sum").is_file() || path.join("shasum").is_file(),
        "closed provider PATH omitted a supported hash command"
    );
    assert!(!path.join("jq").exists(), "closed provider PATH leaked jq");
}

#[test]
fn provider_processes_do_not_evaluate_bash_env() {
    let rust = Fixture::new("shdeps-provider-bash-env");
    let poison = rust.home.join("bash-env");
    let marker = rust.home.join("bash-env-ran");
    std::fs::write(&poison, format!("printf poison >>'{}'\n", marker.display()))
        .expect("BASH_ENV poison");

    let output = rust
        .command()
        .env("BASH_ENV", &poison)
        .env("PATH", rust.closed_tool_path())
        .output()
        .expect("run provider with BASH_ENV");

    assert_cli(&output, 0, CHANGED, b"");
    assert!(!marker.exists(), "provider process evaluated BASH_ENV");
}

#[test]
fn provider_processes_do_not_import_exported_functions() {
    let rust = Fixture::new("shdeps-provider-exported-function");
    let marker = rust.home.join("exported-function-ran");
    let function = format!(
        "() {{ builtin printf poison >>'{}'; builtin printf \"$@\"; }}",
        marker.display()
    );

    let output = rust
        .command()
        .env("BASH_FUNC_printf%%", function)
        .env("PATH", rust.closed_tool_path())
        .output()
        .expect("run provider with exported function");

    assert_cli(&output, 0, CHANGED, b"");
    assert!(!marker.exists(), "provider imported an exported function");
}

#[test]
fn bootstrap_selected_binary_is_the_only_executable_authority() {
    let rust = Fixture::new("shdeps-provider-selected-bin");
    let run = |fixture: &Fixture| {
        let alternate = fixture.home.join("provider-bin/shdeps");
        std::fs::create_dir_all(alternate.parent().expect("binary parent"))
            .expect("binary directory");
        write_exec(
            &alternate,
            format!(
                "#!/bin/sh\nDOT_TEST_BINARY_MARKER=selected exec {} \"$@\"\n",
                fixture.provider.join("shdeps").display()
            )
            .as_bytes(),
        );
        fixture
            .command()
            .env("DOT_TEST_BOOTSTRAP_BIN", &alternate)
            .env("DOT_TEST_RECORD_BINARY_MARKER", "1")
            .output()
            .expect("selected provider binary")
    };
    let rust_output = run(&rust);
    assert_cli(&rust_output, 0, CHANGED, b"");
    assert_eq!(
        std::fs::read(rust.home.join("provider-record")).expect("native binary marker"),
        b"force=0 quiet=0 nested=1 jobs=2\nbinary=selected\n"
    );

    let rust = Fixture::new("shdeps-provider-rejected-bin");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .env("DOT_TEST_BOOTSTRAP_BIN", "relative/shdeps")
            .output()
            .expect("rejected provider binary")
    };
    assert_cli(&run(&rust), 1, UNAVAILABLE, b"");
}

#[test]
fn missing_provider_uses_reviewed_download_natively() {
    let rust = Fixture::new("shdeps-provider-download");
    let run = |fixture: &Fixture| {
        let mut command = fixture.command();
        command
            .env_remove("SHDEPS_LIB")
            .env("SHDEPS_DIR", fixture.home.join("managed-shdeps"))
            .env("SHDEPS_GIT_DEV_DIR", fixture.home.join("missing-dev"))
            .env("PATH", fixture.curl_path())
            .env("DOT_TEST_CURL_RECORD", fixture.home.join("curl-record"));
        command.output().expect("download update")
    };
    let rust_output = run(&rust);
    assert_cli(&rust_output, 0, CHANGED, b"");
    assert_eq!(
        std::fs::read(rust.home.join("curl-record")).expect("native download record"),
        b"https://raw.githubusercontent.com/cgraf78/shdeps/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/install.sh\n"
    );
}

#[test]
fn provider_source_change_reexecs_once_natively() {
    let rust = Fixture::new("shdeps-provider-reexec");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .env("DOT_TEST_PROVIDER_ADVANCE_SOURCE", "1")
            .env(
                "DOT_TEST_PROVIDER_ADVANCED",
                fixture.home.join("provider-advanced"),
            )
            .output()
            .expect("reexec update")
    };
    let rust_output = run(&rust);
    let mut expected = b"[1/5] Overlays   running  checking overlay links                         Ns\n[1/5] Overlays   ok       0 overlays current                             Ns\n[2/5] Tools      running  checking configured dependencies               Ns\n[2/5] Tools      changed  1 changed                                      Ns\n  changed  Cargo: 1 changed\n  changed  ripgrep                      installed\n".to_vec();
    expected.extend_from_slice(CHANGED);
    assert_cli(&rust_output, 0, &expected, b"");
    assert!(rust.home.join("provider-advanced").exists());
    assert!(!rust.state.join("dot/provider-reexec-failed").exists());
}

#[test]
fn provider_abi_probe_obeys_its_deadline_natively() {
    let rust = Fixture::new("shdeps-provider-abi-timeout");
    let run = |fixture: &Fixture| {
        let started = std::time::Instant::now();
        let output = fixture
            .command()
            .env("DOT_TEST_PROVIDER_ABI_SLEEP", "1")
            // Keep the fixture's blocked child much longer than the generous
            // CI deadline, so this remains a no-hang assertion under load.
            .env("DOT_TEST_PROVIDER_ABI_SLEEP_SECONDS", "30")
            .env("_DOT_SHDEPS_ABI_TIMEOUT_SECONDS", "1")
            .output()
            .expect("timed ABI update");
        (output, started.elapsed())
    };
    let (rust_output, rust_elapsed) = run(&rust);
    assert!(
        rust_elapsed < std::time::Duration::from_secs(10),
        "native ABI probe ran for {rust_elapsed:?}"
    );
    assert_cli(
        &rust_output,
        1,
        UNAVAILABLE,
        b"  warning: Shdeps provider ABI probe timed out after 1s\n",
    );
}

#[test]
fn provider_capability_probe_obeys_its_deadline_natively() {
    let rust = Fixture::new("shdeps-provider-capability-timeout");
    let started = std::time::Instant::now();
    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_CAPABILITY_SLEEP", "1")
        .env("DOT_TEST_PROVIDER_CAPABILITY_SLEEP_SECONDS", "30")
        .env("_DOT_SHDEPS_ABI_TIMEOUT_SECONDS", "1")
        .output()
        .expect("timed capability update");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "native capability probe exceeded its bounded deadline"
    );
    assert_cli(
        &output,
        1,
        UNAVAILABLE,
        b"  warning: Shdeps provider capability probe timed out after 1s\n",
    );
}

#[test]
fn completed_abi_probe_cleans_inherited_stdout_descendants() {
    let rust = Fixture::new("shdeps-provider-abi-stdout-descendant");
    let pid_file = rust.home.join("abi-descendant-pid");
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_ABI_LEAK_STDOUT", "1")
        .env("DOT_TEST_PROVIDER_ABI_DESCENDANT_PID", &pid_file);
    let output = bounded_output(command, 10);
    assert_cli(&output, 0, CHANGED, b"");
    let pid = std::fs::read_to_string(pid_file)
        .expect("ABI descendant pid")
        .trim()
        .to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_running(&pid) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        !process_running(&pid),
        "ABI descendant {pid} remained running"
    );
}

#[test]
fn force_and_quiet_reach_the_provider_natively() {
    let rust = Fixture::new("shdeps-provider-flags");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .arg("--force")
            .arg("--quiet")
            .env("DOT_UPDATE_JOBS", "3")
            .env("DOT_TEST_PROVIDER_RECORD_PATH", "1")
            .output()
            .expect("quiet forced update")
    };
    let rust_output = run(&rust);
    assert_cli(&rust_output, 0, b"", b"");
    assert_eq!(
        std::fs::read(rust.home.join("provider-record")).expect("native flags"),
        format!(
            "force=1 quiet=1 nested=1 jobs=3\npath={}\n",
            rust.home.join(".local/bin").display()
        )
        .as_bytes()
    );
}

#[test]
fn failed_provider_update_keeps_actionable_output_natively() {
    let rust = Fixture::new("shdeps-provider-failure");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .env("DOT_TEST_PROVIDER_FAIL", "1")
            .output()
            .expect("failed provider update")
    };
    let rust_output = run(&rust);
    assert_cli(&rust_output, 1, FAILED_UPDATE, b"");
}

#[test]
fn second_provider_source_change_publishes_checkpoint_natively() {
    let rust = Fixture::new("shdeps-provider-checkpoint");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .env("DOT_TEST_PROVIDER_ADVANCE_TWICE", "1")
            .env(
                "DOT_TEST_PROVIDER_ADVANCED",
                fixture.home.join("provider-advanced"),
            )
            .output()
            .expect("double-change update")
    };
    let rust_output = run(&rust);
    let expected = b"[1/5] Overlays   running  checking overlay links                         Ns\n[1/5] Overlays   ok       0 overlays current                             Ns\n[2/5] Tools      running  checking configured dependencies               Ns\n[2/5] Tools      changed  1 changed                                      Ns\n  changed  Cargo: 1 changed\n  changed  ripgrep                      installed\n[1/5] Overlays   running  checking overlay links                         Ns\n[1/5] Overlays   ok       0 overlays current                             Ns\n[2/5] Tools      running  checking configured dependencies               Ns\n[2/5] Tools      changed  1 changed                                      Ns\n  changed  Cargo: 1 changed\n  changed  ripgrep                      installed\nDone with errors in Ns. Reload your shell: source ~/.bashrc\n";
    assert_cli(
        &rust_output,
        1,
        expected,
        b"  warning: dot changed twice during one update; rerun to validate the provider checkpoint\n",
    );
    let rust_checkpoint = rust.state.join("dot/provider-reexec-failed");
    let body = std::fs::read_to_string(&rust_checkpoint).expect("checkpoint");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "cgraf78 dot provider reexec checkpoint v1");
    let before = lines[1].strip_prefix("before=").expect("before field");
    let after = lines[2].strip_prefix("after=").expect("after field");
    assert!(dot::shdeps::revision_valid(before));
    assert!(dot::shdeps::revision_valid(after));
    assert_ne!(before, after);
    assert_eq!(
        std::fs::metadata(rust_checkpoint)
            .expect("checkpoint metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn managed_and_development_provider_sources_run_natively() {
    for (name, source) in [("managed", 0), ("pinned-dev", 1), ("latest-dev", 2)] {
        let rust = Fixture::new(&format!("shdeps-provider-{name}"));
        let rust_dev = (source > 0).then(|| rust.development(source == 1));
        let run = |fixture: &Fixture, dev: Option<&PathBuf>| {
            let mut command = fixture.command();
            command.env_remove("SHDEPS_LIB");
            if let Some(dev) = dev {
                command
                    .env("SHDEPS_GIT_DEV_DIR", dev)
                    .env("SHDEPS_DIR", fixture.home.join("missing-managed"));
                if source == 2 {
                    command.env("DOT_SHDEPS_UPDATE_POLICY", "latest");
                }
            } else {
                command
                    .env("SHDEPS_GIT_DEV_DIR", fixture.home.join("missing-dev"))
                    .env("SHDEPS_DIR", &fixture.provider);
            }
            command.output().expect("provider source update")
        };
        let rust_output = run(&rust, rust_dev.as_ref());
        assert_cli(&rust_output, 0, CHANGED, b"");
    }
}

#[test]
fn bootstrap_and_abi_failures_are_actionable_natively() {
    for (name, bootstrap, abi) in [("bootstrap", true, false), ("abi", false, true)] {
        let rust = Fixture::new(&format!("shdeps-provider-{name}"));
        if abi {
            let binary = rust.provider.join("shdeps");
            let body = std::fs::read_to_string(&binary)
                .expect("provider binary")
                .replace("printf 'abi:1", "printf 'abi:999");
            write_exec(&binary, body.as_bytes());
        }
        let run = |fixture: &Fixture| {
            fixture
                .command()
                .env(
                    "DOT_TEST_PROVIDER_BOOTSTRAP_FAIL",
                    if bootstrap { "1" } else { "0" },
                )
                .output()
                .expect("provider refusal")
        };
        assert_cli(&run(&rust), 1, UNAVAILABLE, b"");
    }
}

#[test]
fn provider_requires_owned_subprocess_cancellation_capability() {
    let rust = Fixture::new("shdeps-provider-capability-missing");
    let capability = rust.home.join("provider-capability");
    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_CAPABILITY_RECORD", &capability)
        .env("DOT_TEST_PROVIDER_REJECT_CAPABILITY", "1")
        .output()
        .expect("provider capability refusal");

    assert_cli(&output, 1, UNAVAILABLE, b"");
    assert_eq!(
        std::fs::read(&capability).expect("capability probe record"),
        b"owned-subprocess-cancellation-v1\nprompt-fifo-reader-before-event-v1\n\
          owned-subprocess-cancellation-v1\nprompt-fifo-reader-before-event-v1\n"
    );
    assert!(
        !rust.home.join("provider-record").exists(),
        "provider update ran without the required ownership capability"
    );
}

#[test]
fn provider_requires_reader_before_prompt_capability() {
    let rust = Fixture::new("shdeps-provider-prompt-capability-missing");
    let capability = rust.home.join("provider-capability");
    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_CAPABILITY_RECORD", &capability)
        .env("DOT_TEST_PROVIDER_REJECT_PROMPT_CAPABILITY", "1")
        .output()
        .expect("provider prompt capability refusal");

    assert_cli(&output, 1, UNAVAILABLE, b"");
    assert_eq!(
        std::fs::read(&capability).expect("capability probe record"),
        b"owned-subprocess-cancellation-v1\nprompt-fifo-reader-before-event-v1\n\
          owned-subprocess-cancellation-v1\nprompt-fifo-reader-before-event-v1\n"
    );
    assert!(
        !rust.home.join("provider-record").exists(),
        "provider update ran without the required prompt handshake capability"
    );
}

#[test]
fn missing_capability_at_matching_abi_forces_one_safe_refresh() {
    let rust = Fixture::new("shdeps-provider-capability-refresh");
    let refreshed = rust.home.join("provider-refreshed");
    let bootstraps = rust.home.join("provider-bootstraps");
    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_REQUIRE_REFRESH", "1")
        .env("DOT_TEST_PROVIDER_REFRESHED", &refreshed)
        .env("DOT_TEST_PROVIDER_BOOTSTRAP_RECORD", &bootstraps)
        .output()
        .expect("provider capability refresh");

    assert_cli(&output, 0, CHANGED, b"");
    assert!(
        refreshed.exists(),
        "capability mismatch did not refresh provider"
    );
    assert!(
        rust.home.join("provider-record").exists(),
        "refreshed provider was not invoked"
    );
    assert_eq!(
        std::fs::read(&bootstraps).expect("bootstrap record"),
        b"0\n1\n",
        "capability recovery must retry exactly once with forced bootstrap"
    );
}

#[test]
fn provider_accepts_owned_subprocess_cancellation_capability() {
    let rust = Fixture::new("shdeps-provider-capability-present");
    let capability = rust.home.join("provider-capability");
    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_CAPABILITY_RECORD", &capability)
        .output()
        .expect("provider capability acceptance");

    assert_cli(&output, 0, CHANGED, b"");
    assert_eq!(
        std::fs::read(&capability).expect("capability probe record"),
        b"owned-subprocess-cancellation-v1\nprompt-fifo-reader-before-event-v1\n"
    );
}

#[test]
fn provider_executes_the_exact_bytes_that_passed_both_capability_probes() {
    let rust = Fixture::new("shdeps-provider-immutable-selection");
    let replacement = rust.home.join("replacement-provider");
    let replacement_ran = rust.home.join("replacement-ran");
    write_exec(
        &replacement,
        format!(
            "#!/bin/sh\nprintf replaced >{}\nexit 0\n",
            replacement_ran.display()
        )
        .as_bytes(),
    );

    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_SWAP_AFTER_CAPABILITY", &replacement)
        .output()
        .expect("provider immutable-selection update");

    assert_cli(&output, 0, CHANGED, b"");
    assert!(
        !replacement_ran.exists(),
        "replacement bytes executed after the original provider passed validation"
    );
    assert!(
        rust.home.join("provider-record").exists(),
        "validated provider snapshot did not execute the update"
    );
    assert!(
        std::fs::read_dir(&rust.state)
            .expect("provider state directory")
            .all(|entry| !entry
                .expect("provider state entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".dot-provider-snapshot-")),
        "private provider snapshot was not removed after update"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
fn assert_development_git_query_cancellation(subcommand: &str, query: &str) {
    let rust = Fixture::new(&format!("shdeps-development-git-{subcommand}"));
    let development_root = rust.development(false);
    let checkout = development_root.join("shdeps");
    let marker = rust.home.join("development-git-query-ready");
    let trace = rust.home.join("development-git-query-trace");
    let mut command = rust.command_for(subcommand);
    command
        .env("PATH", rust.development_git_path())
        .env(
            "DOT_TEST_REAL_GIT",
            fixture_command("git").expect("host Git"),
        )
        .env("DOT_TEST_DEVELOPMENT_CHECKOUT", &checkout)
        .env("DOT_TEST_DEVELOPMENT_GIT_QUERY", query)
        .env("DOT_TEST_DEVELOPMENT_GIT_MODE", "block")
        .env("DOT_TEST_DEVELOPMENT_GIT_READY", &marker)
        .env("DOT_TEST_DEVELOPMENT_GIT_TRACE", &trace)
        .env("SHDEPS_GIT_DEV_DIR", &development_root)
        .env("SHDEPS_DIR", &rust.provider)
        .env("DOT_SHDEPS_UPDATE_POLICY", "latest")
        .env_remove("SHDEPS_LIB");
    let mut dot = spawn_test_session(command);
    let query_pid = wait_for_pid(&marker, 10).unwrap_or_else(|| {
        panic!(
            "development Git query did not start; trace={}",
            String::from_utf8_lossy(&std::fs::read(&trace).unwrap_or_default())
        )
    });
    let mut query_process = EscapedProcess::from_identity(
        process_identity(query_pid).expect("development Git query identity"),
    );

    // SAFETY: the retained test child owns this exact positive Dot PID.
    let delivered = unsafe { libc::kill(dot.id(), libc::SIGTERM) } == 0;
    let completed = dot.wait_bounded(std::time::Duration::from_secs(5));
    let query_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !query_process.observe_stopped() && std::time::Instant::now() < query_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let query_stopped = query_process.observe_stopped();
    if !query_stopped {
        query_process.stop();
    }
    let output = dot.reap_with_output().expect("reap cancelled Dot query");

    assert!(delivered, "deliver signal during {subcommand} {query}");
    assert!(completed, "{subcommand} {query} did not stop boundedly");
    assert!(query_stopped, "{subcommand} {query} subprocess survived");
    assert_eq!(
        output.status.code(),
        Some(143),
        "{subcommand} {query}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
#[test]
fn signal_cancels_development_git_query_during_update() {
    assert_development_git_query_cancellation("update", "rev-parse --show-toplevel");
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
#[test]
fn signal_cancels_development_git_query_during_doctor() {
    assert_development_git_query_cancellation(
        "doctor",
        "config --local --get-all remote.origin.url",
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
#[test]
fn development_git_queries_reject_unbounded_output_and_fall_back_safely() {
    for (index, query) in [
        "rev-parse --show-toplevel",
        "rev-parse --absolute-git-dir",
        "rev-parse --path-format=absolute --git-common-dir",
        "config --local --get-all remote.origin.url",
        "remote get-url --all origin",
    ]
    .into_iter()
    .enumerate()
    {
        let rust = Fixture::new(&format!("shdeps-development-git-overflow-{index}"));
        let development_root = rust.development(false);
        let checkout = development_root.join("shdeps");
        let ready = rust.home.join("development-git-overflow-ready");
        let stopped = rust.home.join("development-git-overflow-stopped");
        let trace = rust.home.join("development-git-overflow-trace");
        let mut command = rust.command();
        command
            .env("PATH", rust.development_git_path())
            .env(
                "DOT_TEST_REAL_GIT",
                fixture_command("git").expect("host Git"),
            )
            .env("DOT_TEST_DEVELOPMENT_CHECKOUT", &checkout)
            .env("DOT_TEST_DEVELOPMENT_GIT_QUERY", query)
            .env("DOT_TEST_DEVELOPMENT_GIT_MODE", "overflow")
            .env("DOT_TEST_DEVELOPMENT_GIT_READY", &ready)
            .env("DOT_TEST_DEVELOPMENT_GIT_STOPPED", &stopped)
            .env("DOT_TEST_DEVELOPMENT_GIT_TRACE", &trace)
            .env("SHDEPS_GIT_DEV_DIR", &development_root)
            .env("SHDEPS_DIR", &rust.provider)
            .env("DOT_SHDEPS_UPDATE_POLICY", "latest")
            .env_remove("SHDEPS_LIB");

        // Overflow teardown runs the full bounded session stop (TERM grace
        // plus verification) before the fallback update completes; budget it
        // like the sibling provider-overflow tests, not like a fast query.
        let output = bounded_output(command, 10);
        assert_cli(&output, 0, CHANGED, b"");
        assert!(ready.exists(), "query was not exercised: {query}");
        assert!(
            stopped.exists(),
            "overflowing query skipped cooperative teardown: {query}; trace={}",
            String::from_utf8_lossy(&std::fs::read(&trace).unwrap_or_default())
        );
    }
}

#[test]
fn bootstrap_stderr_is_preserved_natively() {
    let rust = Fixture::new("shdeps-provider-bootstrap-stderr");
    let combined_path = rust.home.join("combined-output");
    let combined = std::fs::File::create(&combined_path).expect("combined output");
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_BOOTSTRAP_DIAGNOSTIC", "1")
        .stdout(Stdio::from(
            combined.try_clone().expect("clone combined stdout"),
        ))
        .stderr(Stdio::from(combined));
    let status = command.status().expect("provider update");
    let output = std::fs::read(combined_path).expect("read combined output");

    assert_eq!(
        status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output)
    );
    let tools_open = output
        .windows(b"checking configured dependencies".len())
        .position(|window| window == b"checking configured dependencies")
        .expect("Tools stage start");
    let diagnostic = output
        .windows(b"bootstrap diagnostic".len())
        .position(|window| window == b"bootstrap diagnostic")
        .expect("bootstrap diagnostic");
    let tools_close = output
        .windows(b"1 changed".len())
        .position(|window| window == b"1 changed")
        .expect("Tools stage close");
    assert!(
        diagnostic < tools_open && tools_open < tools_close,
        "bootstrap diagnostic was not emitted at its execution point: {}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn verbose_provider_events_render_in_order_natively() {
    let rust = Fixture::new("shdeps-provider-verbose");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .arg("--verbose")
            .env("DOT_TEST_PROVIDER_VERBOSE_EVENTS", "1")
            .output()
            .expect("verbose provider update")
    };
    let expected = b"[1/5] Overlays   running  checking overlay links                         Ns\n[1/5] Overlays   ok       0 overlays current                             Ns\n[2/5] Tools      running  checking configured dependencies               Ns\n[2/5] Tools      running  Resolving          [####----] 1/2              Ns\n  warning  provider warning\n  Cargo\n  changed  ripgrep                      installed\n[2/5] Tools      changed  1 changed                                      Ns\n[3/5] Configs    running  checking config hooks                          Ns\n[3/5] Configs    ok       no config hooks                                Ns\n[4/5] Cleanup    running  normalizing worktree                           Ns\n[4/5] Cleanup    ok       no base repo                                   Ns\nDone in Ns. Reload your shell: source ~/.bashrc\n";
    assert_cli(&run(&rust), 0, expected, b"");
}

#[test]
fn provider_capture_preserves_large_partial_lines_and_trailing_output() {
    let rust = Fixture::new("shdeps-provider-large-output");
    let output = rust
        .command()
        .env("DOT_TEST_PROVIDER_LARGE_OUTPUT", "1")
        .output()
        .expect("large provider output");
    let mut stderr = vec![b' '; 299_999];
    stderr.extend_from_slice(b"y\nprovider stderr tail\n");

    assert_cli(&output, 0, CHANGED, &stderr);
}

#[test]
fn active_newline_free_provider_streams_fail_boundedly() {
    for stream in ["stdout", "stderr"] {
        let rust = Fixture::new(&format!("shdeps-provider-{stream}-overflow"));
        let stopped = rust.home.join("provider-overflow-stopped");
        let mut command = rust.command();
        command
            .env("DOT_TEST_PROVIDER_OVERFLOW_STREAM", stream)
            .env(
                "DOT_TEST_PROVIDER_OVERFLOW_PID",
                rust.home.join("provider-overflow-pid"),
            )
            .env("DOT_TEST_PROVIDER_OVERFLOW_STOPPED", &stopped);
        let output = bounded_output(command, 10);

        assert_eq!(
            output.status.code(),
            Some(1),
            "{stream} overflow was accepted: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stopped.exists(), "{stream} overflow skipped TERM teardown");
        assert!(
            output.stdout.len() <= 2 * 1024 * 1024 && output.stderr.len() <= 2 * 1024 * 1024,
            "{stream} overflow escaped the aggregate capture bound"
        );
    }
}

#[test]
fn many_small_provider_events_exhaust_one_run_budget() {
    let rust = Fixture::new("shdeps-provider-many-events");
    let stopped = rust.home.join("provider-many-events-stopped");
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_MANY_EVENTS", "1")
        .env(
            "DOT_TEST_PROVIDER_MANY_EVENTS_PID",
            rust.home.join("provider-many-events-pid"),
        )
        .env("DOT_TEST_PROVIDER_MANY_EVENTS_STOPPED", &stopped);
    let output = bounded_output(command, 5);

    assert_eq!(
        output.status.code(),
        Some(1),
        "many small frames were accepted: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stopped.exists(), "event overflow skipped TERM teardown");
    assert_eq!(
        output
            .stderr
            .windows(b"Shdeps provider output exceeded its safety limit".len())
            .filter(|window| *window == b"Shdeps provider output exceeded its safety limit")
            .count(),
        1,
        "provider output limit did not have one stable diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output
            .stdout
            .windows(b"checking config hooks".len())
            .any(|window| window == b"checking config hooks"),
        "a later update stage ran after provider overflow"
    );
    assert!(
        output.stdout.len() <= 2 * 1024 * 1024 && output.stderr.len() <= 2 * 1024 * 1024,
        "provider output escaped the cumulative bound"
    );
}

#[test]
fn bootstrap_and_api_probe_output_are_capped() {
    for variable in [
        "DOT_TEST_PROVIDER_BOOTSTRAP_OVERFLOW",
        "DOT_TEST_PROVIDER_ABI_OVERFLOW",
        "DOT_TEST_PROVIDER_CAPABILITY_OVERFLOW",
    ] {
        let rust = Fixture::new(&format!(
            "shdeps-provider-{}",
            variable.to_ascii_lowercase()
        ));
        let mut command = rust.command();
        command.env(variable, "1");
        let output = bounded_output(command, 10);

        assert_eq!(
            output.status.code(),
            Some(1),
            "{variable} was accepted: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.len() <= 2 * 1024 * 1024 && output.stderr.len() <= 2 * 1024 * 1024,
            "{variable} escaped the aggregate capture bound"
        );
    }
}

#[test]
fn provider_prompt_rendezvous_acknowledges_natively() {
    let rust = Fixture::new("shdeps-provider-prompt");
    let path = PathBuf::from(rust.closed_tool_path());
    let _ = std::fs::remove_file(path.join("mkfifo"));
    let mut command = rust.command();
    command
        .env("PATH", &path)
        .env("DOT_TEST_PROVIDER_PROMPT", "1")
        .env(
            "DOT_TEST_PROVIDER_PROMPT_RECORD",
            rust.home.join("prompt-record"),
        );
    let output = bounded_output(command, 10);
    assert_cli(&output, 0, CHANGED, b"");
    assert_eq!(
        std::fs::read(rust.home.join("prompt-record")).expect("native prompt acknowledgment"),
        b"ready\n"
    );
}

#[test]
fn provider_prompt_is_not_acknowledged_after_cancellation() {
    let rust = Fixture::new("shdeps-provider-cancelled-prompt");
    let prompt_record = rust.home.join("prompt-record");
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_PROMPT_AFTER_SIGNAL", "1")
        .env("DOT_TEST_PROVIDER_PROMPT_RECORD", &prompt_record);
    let output = bounded_output(command, 10);

    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !prompt_record.exists(),
        "provider prompt was acknowledged after SIGINT"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn provider_final_prompt_is_not_acknowledged_after_conventional_interrupt() {
    let fixture = Fixture::new("shdeps-provider-final-cancelled-prompt");
    let tmp = fixture.home.join("tmp");
    let prompt_path_file = fixture.home.join("prompt-path");
    let prompt_release = fixture.home.join("prompt-release");
    std::fs::create_dir(&tmp).expect("provider prompt temporary directory");
    let mut command = fixture.command();
    command
        .env("TMPDIR", &tmp)
        .env("DOT_TEST_PROVIDER_FINAL_PROMPT_EXIT_CODE", "130")
        .env("DOT_TEST_PROVIDER_PROMPT_PATH", &prompt_path_file)
        .env("DOT_TEST_PROVIDER_PROMPT_RELEASE", &prompt_release)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut dot = spawn_test_session(command);
    assert!(
        wait_for_nonempty(&prompt_path_file, 10),
        "provider did not publish its prompt FIFO"
    );
    let prompt_path = PathBuf::from(
        std::fs::read_to_string(&prompt_path_file)
            .expect("read provider prompt FIFO path")
            .trim(),
    );
    assert!(
        prompt_path.starts_with(&tmp),
        "provider published a prompt FIFO outside the fixture: {}",
        prompt_path.display()
    );
    let mut prompt_reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&prompt_path)
        .expect("open provider prompt FIFO observer");
    std::fs::write(&prompt_release, b"ready\n").expect("release final prompt provider");

    assert!(
        dot.wait_bounded(std::time::Duration::from_secs(10)),
        "Dot did not exit after provider status 130"
    );
    let acknowledgment =
        read_nonblocking_to_end(&mut prompt_reader, std::time::Duration::from_secs(5));
    let status = dot.reap().expect("reap Dot after final prompt");

    assert_eq!(status.code(), Some(130));
    assert!(
        acknowledgment.is_empty(),
        "Dot acknowledged a final prompt after provider interruption: {acknowledgment:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn provider_complete_final_prompt_is_not_acknowledged_after_conventional_interrupt() {
    let fixture = Fixture::new("shdeps-provider-complete-final-cancelled-prompt");
    let tmp = fixture.home.join("tmp");
    let prompt_path_file = fixture.home.join("prompt-path");
    let prompt_release = fixture.home.join("prompt-release");
    let provider_pid_file = fixture.home.join("provider-pid");
    std::fs::create_dir(&tmp).expect("provider prompt temporary directory");
    let mut command = fixture.command();
    command
        .env("TMPDIR", &tmp)
        .env("DOT_TEST_PROVIDER_FINAL_PROMPT_EXIT_CODE", "130")
        .env("DOT_TEST_PROVIDER_FINAL_PROMPT_NEWLINE", "1")
        .env("DOT_TEST_PROVIDER_FINAL_PID", &provider_pid_file)
        .env("DOT_TEST_PROVIDER_PROMPT_PATH", &prompt_path_file)
        .env("DOT_TEST_PROVIDER_PROMPT_RELEASE", &prompt_release)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut dot = spawn_test_session(command);
    assert!(
        wait_for_nonempty(&prompt_path_file, 10),
        "provider did not publish its prompt FIFO"
    );
    let prompt_path = PathBuf::from(
        std::fs::read_to_string(&prompt_path_file)
            .expect("read provider prompt FIFO path")
            .trim(),
    );
    assert!(
        prompt_path.starts_with(&tmp),
        "provider published a prompt FIFO outside the fixture: {}",
        prompt_path.display()
    );
    let mut prompt_reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&prompt_path)
        .expect("open provider prompt FIFO observer");
    let provider_pid = wait_for_pid(&provider_pid_file, 10).expect("final prompt provider pid");
    // Freeze Dot before releasing the provider so the complete JSONL record
    // and exit status are both published before the supervisor can drain.
    // SAFETY: the fixture owns this positive Dot child.
    assert_eq!(unsafe { libc::kill(dot.id(), libc::SIGSTOP) }, 0);
    std::fs::write(&prompt_release, b"ready\n").expect("release final prompt provider");
    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_running_pid(provider_pid) && std::time::Instant::now() < exit_deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !process_running_pid(provider_pid),
        "final prompt provider did not publish its exit before Dot resumed"
    );
    // SAFETY: the same fixture-owned Dot child is deliberately stopped above.
    assert_eq!(unsafe { libc::kill(dot.id(), libc::SIGCONT) }, 0);

    assert!(
        dot.wait_bounded(std::time::Duration::from_secs(10)),
        "Dot did not exit after provider status 130"
    );
    let acknowledgment =
        read_nonblocking_to_end(&mut prompt_reader, std::time::Duration::from_secs(5));
    let status = dot.reap().expect("reap Dot after complete final prompt");

    assert_eq!(status.code(), Some(130));
    assert!(
        acknowledgment.is_empty(),
        "Dot acknowledged a complete final prompt after provider interruption: {acknowledgment:?}"
    );
}

#[test]
fn provider_inherits_parent_session_for_interactive_prompts() {
    let rust = Fixture::new("shdeps-provider-parent-topology");
    let provider_pid_file = rust.home.join("provider-topology-pid");
    let release = rust.home.join("provider-topology-release");
    let child = rust
        .command()
        .env("DOT_TEST_PROVIDER_PARENT_TOPOLOGY", "1")
        .env("DOT_TEST_PROVIDER_TOPOLOGY_PID", &provider_pid_file)
        .env("DOT_TEST_PROVIDER_TOPOLOGY_RELEASE", &release)
        .spawn()
        .expect("provider update");
    let provider_pid = wait_for_pid(&provider_pid_file, 10).expect("provider pid");
    let dot_pid = child.id() as i32;
    let same_session = process_session(provider_pid) == process_session(dot_pid);
    let same_group = process_group(provider_pid) == process_group(dot_pid);
    std::fs::write(&release, b"ready\n").expect("release provider");
    let output = child.wait_with_output().expect("provider output");

    assert_cli(&output, 0, CHANGED, b"");
    assert!(
        same_session && same_group,
        "provider did not inherit Dot's terminal topology"
    );
}

#[test]
fn provider_output_relayed_before_signal_is_not_discarded() {
    let rust = Fixture::new("shdeps-provider-live-output");
    let provider_pid_file = rust.home.join("provider-live-pid");
    let stdout_path = rust.home.join("provider-live-stdout");
    let stderr_path = rust.home.join("provider-live-stderr");
    let stdout = std::fs::File::create(&stdout_path).expect("provider stdout");
    let stderr = std::fs::File::create(&stderr_path).expect("provider stderr");
    let mut command = rust.command();
    command
        .arg("--verbose")
        .env("DOT_TEST_PROVIDER_LIVE_OUTPUT", "1")
        .env("DOT_TEST_PROVIDER_LIVE_PID", &provider_pid_file)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = spawn_test_session(command);
    let provider_pid = wait_for_pid(&provider_pid_file, 10).expect("provider pid");
    let provider_identity = process_identity(provider_pid).expect("observe live provider identity");
    let mut provider = EscapedProcess::from_identity(provider_identity);
    let live_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut stdout_live = false;
    let mut stderr_live = false;
    while std::time::Instant::now() < live_deadline {
        stdout_live = std::fs::read(&stdout_path).is_ok_and(|bytes| {
            bytes
                .windows(b"provider live stdout".len())
                .any(|window| window == b"provider live stdout")
        });
        stderr_live = std::fs::read(&stderr_path).is_ok_and(|bytes| {
            bytes
                .windows(b"provider live stderr".len())
                .any(|window| window == b"provider live stderr")
        });
        if stdout_live && stderr_live {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The retained, unreaped child keeps this positive PID from being reused.
    assert_eq!(unsafe { libc::kill(child.id(), libc::SIGINT) }, 0);
    let completed = child.wait_bounded(std::time::Duration::from_secs(5));
    let provider_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !provider.observe_stopped() && std::time::Instant::now() < provider_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let provider_survived = !provider.observe_stopped();
    if provider_survived {
        provider.stop();
    }
    assert!(
        completed,
        "Dot did not finish after interrupting live output"
    );
    assert!(!provider_survived, "provider survived output cancellation");
    let status = child.reap().expect("provider output status");
    let stdout = std::fs::read(&stdout_path).expect("read provider stdout");
    let stderr = std::fs::read(&stderr_path).expect("read provider stderr");

    assert_eq!(status.code(), Some(130));
    assert!(
        stdout_live,
        "provider stdout was not delivered before SIGINT"
    );
    assert!(
        stderr_live,
        "provider stderr was not delivered before SIGINT"
    );
    assert_eq!(
        stdout
            .windows(b"provider live stdout".len())
            .filter(|window| *window == b"provider live stdout")
            .count(),
        1,
        "provider stdout was lost or replayed: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert_eq!(
        stderr
            .windows(b"provider live stderr".len())
            .filter(|window| *window == b"provider live stderr")
            .count(),
        1,
        "provider stderr was lost or replayed: {}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn signal_interrupts_backpressured_provider_relay() {
    use std::io::Read as _;

    let rust = Fixture::new("shdeps-provider-backpressure");
    let provider_pid_file = rust.home.join("provider-backpressure-pid");
    let signal_file = rust.home.join("provider-backpressure-signal");
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("stdout pair");
    let mut command = rust.command();
    command
        .env("PATH", rust.closed_tool_path())
        .env("DOT_TEST_PROVIDER_BACKPRESSURE", "1")
        .env("DOT_TEST_PROVIDER_BACKPRESSURE_PID", &provider_pid_file)
        .env("DOT_TEST_PROVIDER_BACKPRESSURE_SIGNAL", &signal_file)
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null());
    let mut child = spawn_test_session(command);
    let dot_pid = child.id();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut observed = Vec::new();
        let mut byte = [0];
        loop {
            match reader.read(&mut byte) {
                Ok(0) | Err(_) => return false,
                Ok(_) => observed.push(byte[0]),
            }
            if observed.ends_with(b"PROVIDER-BLOCKED") {
                break;
            }
        }
        let _ = started_tx.send(());
        let _ = release_rx.recv();
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        true
    });
    let mut provider = wait_for_pid(&provider_pid_file, 10)
        .and_then(process_identity)
        .map(EscapedProcess::from_identity);
    let rendering_started = provider.is_some()
        && started_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .is_ok();
    let delivered = if rendering_started {
        // SAFETY: the fixture owns this positive Dot child and SIGINT is valid.
        (unsafe { libc::kill(dot_pid, libc::SIGINT) }) == 0
    } else {
        false
    };
    let mut completed = child.wait_bounded(std::time::Duration::from_secs(3));
    let blocked = !completed;
    let _ = release_tx.send(());
    if !completed {
        completed = child.wait_bounded(std::time::Duration::from_secs(2));
    }
    let provider_stopped = provider.as_mut().is_some_and(|provider| {
        let death_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !provider.observe_stopped() && std::time::Instant::now() < death_deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stopped = provider.observe_stopped();
        if !stopped {
            provider.stop();
        }
        stopped
    });
    if !completed {
        panic!("provider did not exit after releasing its output sink");
    }
    let status = child.reap().expect("provider exit");
    let marker_observed = reader.join().expect("provider output reader");

    assert!(
        rendering_started,
        "provider relay never reached its output sink"
    );
    assert!(marker_observed, "provider output marker was not observed");
    assert!(delivered, "SIGINT was not delivered to Dot");
    assert!(!blocked, "signal left Dot blocked on provider output");
    assert_eq!(status.code(), Some(130));
    assert_eq!(
        std::fs::read(&signal_file).expect("provider signal marker"),
        b"TERM\n"
    );
    assert!(
        provider_stopped,
        "provider survived backpressure cancellation"
    );
}

#[test]
fn trusted_provider_exit_interrupts_an_undrained_cli_pipe_without_a_signal() {
    let rust = Fixture::new("shdeps-provider-exit-backpressure");
    let provider_pid_file = rust.home.join("provider-exit-backpressure-pid");
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("stdout pair");
    let mut command = rust.command();
    command
        .env("PATH", rust.closed_tool_path())
        .env("DOT_TEST_PROVIDER_BACKPRESSURE_EXIT130", "1")
        .env("DOT_TEST_PROVIDER_BACKPRESSURE_PID", &provider_pid_file)
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null());
    let mut child = spawn_test_session(command);
    let provider_pid = wait_for_pid(&provider_pid_file, 10).expect("provider pid");
    // A fast provider can exit between its pidfile write and our identity
    // query; ESRCH there proves the stopped end-state this test asserts.
    let mut provider =
        wait_for_identity_or_exit(provider_pid, 10).map(EscapedProcess::from_identity);
    let exited_before_identity = provider.is_none();

    let completed = child.wait_bounded(std::time::Duration::from_secs(5));
    if !completed {
        let _ = child.signal_leader_group(libc::SIGKILL);
        if let Some(provider) = provider.as_mut() {
            provider.stop();
        }
    }
    let status = child.reap().expect("provider backpressure status");
    drop(reader);

    assert!(
        completed,
        "trusted provider exit 130 remained blocked on an unread output pipe"
    );
    assert_eq!(status.code(), Some(130));
    assert!(
        exited_before_identity
            || provider
                .as_mut()
                .expect("provider handle")
                .observe_stopped(),
        "trusted provider survived its terminal cancellation status"
    );
}

#[test]
fn provider_exit_does_not_wait_for_or_leak_stdout_descendants() {
    let rust = Fixture::new("shdeps-provider-stdout-descendant");
    let pid_file = rust.home.join("descendant-pid");
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_LEAK_STDOUT", "1")
        .env("DOT_TEST_PROVIDER_DESCENDANT_PID", &pid_file);
    let output = bounded_output(command, 10);
    assert_cli(&output, 0, CHECKED, b"");
    let pid = std::fs::read_to_string(pid_file)
        .expect("descendant pid")
        .trim()
        .to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_running(&pid) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        !process_running(&pid),
        "provider descendant {pid} remained running"
    );
}

#[test]
fn provider_exit_does_not_wait_for_escaped_session_output_holders() {
    for stream in ["stdout", "stderr"] {
        let rust = Fixture::new(&format!("shdeps-provider-escaped-{stream}"));
        let pid_file = rust.home.join("escaped-output-pid");
        let helper = rust.escaped_output_helper();
        let mut command = rust.command();
        command
            .env("DOT_TEST_PROVIDER_ESCAPE_OUTPUT", "1")
            .env("DOT_TEST_PROVIDER_ESCAPE_STREAM", stream)
            .env("DOT_TEST_PROVIDER_ESCAPE_HELPER", helper)
            .env("DOT_TEST_PROVIDER_ESCAPE_PID", &pid_file);
        #[cfg(target_os = "macos")]
        command.env("DOT_TEST_PROVIDER_ESCAPE_LIFETIME", "3");
        let mut child = spawn_test_session(command);
        let Some(pid) = wait_for_pid(&pid_file, 10) else {
            panic!("escaped {stream} holder did not publish its pid");
        };
        let mut escaped = EscapedProcess::new(pid);
        assert_eq!(
            process_session(pid),
            Some(pid),
            "fixture did not escape SID"
        );
        assert!(
            process_running_pid(pid),
            "escaped {stream} holder exited early"
        );
        let completed = child.wait_bounded(std::time::Duration::from_secs(5));
        // Revalidate the fixture-owned session identity before negative-PID
        // delivery; a stale PID must never authorize signaling another group.
        let escaped_live = escaped.live_in_owned_session();
        let stopped = escaped.stop();
        assert!(completed, "Dot waited for escaped {stream} EOF");
        let output = child.reap_with_output().expect("provider output");

        assert!(
            escaped_live,
            "Dot terminated the escaped {stream} holder outside its owned session"
        );
        assert!(stopped, "escaped {stream} holder survived test cleanup");
        assert_cli(&output, 0, CHECKED, b"");
    }
}

#[test]
fn provider_exit_bounds_an_escaped_continuous_writer() {
    let rust = Fixture::new("shdeps-provider-escaped-writer");
    let pid_file = rust.home.join("escaped-writer-pid");
    let release = rust.home.join("escaped-writer-release");
    let helper = rust.escaped_output_helper();
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_ESCAPE_OUTPUT", "1")
        .env("DOT_TEST_PROVIDER_ESCAPE_STREAM", "stdout")
        .env("DOT_TEST_PROVIDER_ESCAPE_FLOOD", "stdout")
        .env("DOT_TEST_PROVIDER_ESCAPE_HELPER", helper)
        .env("DOT_TEST_PROVIDER_ESCAPE_PID", &pid_file)
        .env("DOT_TEST_PROVIDER_ESCAPE_RELEASE", &release);
    #[cfg(target_os = "macos")]
    command.env("DOT_TEST_PROVIDER_ESCAPE_LIFETIME", "3");
    let mut child = spawn_test_session(command);
    let Some(pid) = wait_for_pid(&pid_file, 10) else {
        panic!("escaped writer did not publish its pid");
    };
    let mut escaped = EscapedProcess::new(pid);
    std::fs::write(&release, b"ready\n").expect("release escaped writer");
    assert!(
        escaped.live_in_owned_session(),
        "escaped writer exited before Dot observed it"
    );
    let completed = child.wait_bounded(std::time::Duration::from_secs(5));
    let writer_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !escaped.observe_stopped() && std::time::Instant::now() < writer_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let writer_exited = escaped.observe_stopped();
    let helper_stopped = escaped.stop();

    assert!(completed, "escaped continuous writer kept Dot running");
    let output = child.reap_with_output().expect("provider output");
    assert!(
        writer_exited,
        "escaped writer did not observe capture closure"
    );
    assert!(
        helper_stopped,
        "escaped continuous writer survived pipe closure"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "escaped continuous provider output was accepted"
    );
    assert!(
        output.stdout.len() <= 2 * 1024 * 1024 && output.stderr.len() <= 2 * 1024 * 1024,
        "escaped writer exceeded the aggregate capture bound"
    );
    assert!(
        !output
            .stdout
            .windows(b"[3/5] Configs".len())
            .any(|window| window == b"[3/5] Configs"),
        "update advanced after rejecting escaped provider output"
    );
}

fn assert_preparation_handles_escaped_stderr(stage: &str) {
    let rust = Fixture::new(&format!("shdeps-{stage}-escaped-stderr"));
    let pid_file = rust.home.join("escaped-preparation-stderr-pid");
    let helper = rust.escaped_output_helper();
    let mut command = rust.command();
    command
        .env("DOT_TEST_PROVIDER_ESCAPE_HELPER", helper)
        .env("DOT_TEST_PROVIDER_ESCAPE_PID", &pid_file)
        .env("DOT_TEST_PROVIDER_ESCAPE_LIFETIME", "5");
    match stage {
        "bootstrap" => {
            command.env("DOT_TEST_PROVIDER_BOOTSTRAP_ESCAPE_STDERR", "1");
        }
        "download" => {
            command
                .env_remove("SHDEPS_LIB")
                .env("SHDEPS_DIR", rust.home.join("missing-provider"))
                .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-development"))
                .env("PATH", rust.escaped_curl_path())
                .env("DOT_TEST_CURL_RECORD", rust.home.join("curl-record"));
        }
        _ => panic!("unknown preparation stage"),
    }
    let mut child = spawn_test_session(command);
    let (mut stdout_reader, mut stderr_reader) = child.take_output_readers();
    let (output_sender, output_receiver) = std::sync::mpsc::channel();
    let stdout_sender = output_sender.clone();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        let result = stdout_reader.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stdout_sender.send((true, result));
    });
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        let result = stderr_reader.read_to_end(&mut bytes).map(|_| bytes);
        let _ = output_sender.send((false, result));
    });
    let Some(pid) = wait_for_pid(&pid_file, 10) else {
        panic!("escaped {stage} stderr holder did not publish its pid");
    };
    let mut escaped = EscapedProcess::new(pid);
    assert_eq!(
        process_session(pid),
        Some(pid),
        "fixture did not escape SID"
    );
    let process_completed = child.wait_bounded(std::time::Duration::from_secs(5));
    let mut stdout = None;
    let mut stderr = None;
    let output_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while (stdout.is_none() || stderr.is_none()) && std::time::Instant::now() < output_deadline {
        let remaining = output_deadline.saturating_duration_since(std::time::Instant::now());
        let Ok((is_stdout, bytes)) = output_receiver.recv_timeout(remaining) else {
            break;
        };
        if is_stdout {
            stdout = Some(bytes.expect("read provider stdout"));
        } else {
            stderr = Some(bytes.expect("read provider stderr"));
        }
    }
    let output_completed = stdout.is_some() && stderr.is_some();
    assert!(
        process_completed,
        "Dot did not finish after escaped {stage} stderr holder"
    );
    let escaped_live = escaped.live_in_owned_session();
    let stopped = escaped.stop();
    while stdout.is_none() || stderr.is_none() {
        let (is_stdout, bytes) = output_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("provider output remained blocked after cleanup");
        if is_stdout {
            stdout = Some(bytes.expect("read provider stdout"));
        } else {
            stderr = Some(bytes.expect("read provider stderr"));
        }
    }
    let output = Output {
        status: child.reap().expect("provider exit"),
        stdout: stdout.expect("provider stdout"),
        stderr: stderr.expect("provider stderr"),
    };

    assert!(
        output_completed,
        "Dot left its external output pipe open after bounded {stage} cleanup"
    );
    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert!(
        !escaped_live,
        "Dot left its attributed escaped {stage} stderr holder running"
    );
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    assert!(
        escaped_live,
        "portable cleanup signaled an escaped {stage} stderr holder without stable authority"
    );
    assert!(
        stopped,
        "escaped {stage} stderr holder survived test cleanup"
    );
    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert_eq!(
        output.status.code(),
        Some(0),
        "owned escaped {stage} cleanup failed: stdout={:?} stderr={:?}",
        output.stdout,
        output.stderr
    );
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    assert_eq!(
        output.status.code(),
        Some(125),
        "portable escaped {stage} cleanup did not fail closed: stdout={:?} stderr={:?}",
        output.stdout,
        output.stderr
    );
}

#[test]
fn provider_bootstrap_fails_closed_for_escaped_stderr() {
    assert_preparation_handles_escaped_stderr("bootstrap");
}

#[test]
fn provider_download_fails_closed_for_escaped_stderr() {
    assert_preparation_handles_escaped_stderr("download");
}

#[test]
fn escaped_unicode_provider_event_is_literal_with_and_without_jq() {
    for (name, jq, expected) in [
        ("with-jq", true, b"caf\xc3\xa9".as_slice()),
        ("without-jq", false, b"caf\\u00e9".as_slice()),
    ] {
        let rust = Fixture::new(&format!("shdeps-provider-unicode-{name}"));
        let run = |fixture: &Fixture| {
            let path = if jq {
                fixture.jq_path()
            } else {
                fixture.closed_tool_path()
            };
            fixture
                .command()
                .env("PATH", path)
                .env("DOT_TEST_PROVIDER_ESCAPED_EVENT", "1")
                .output()
                .expect("escaped provider event")
        };
        let rust_output = run(&rust);
        let mut stdout = b"[1/5] Overlays   running  checking overlay links                         Ns\n[1/5] Overlays   ok       0 overlays current                             Ns\n[2/5] Tools      running  checking configured dependencies               Ns\n[2/5] Tools      changed  1 changed                                      Ns\n  changed  Cargo: 1 changed\n  changed  ".to_vec();
        stdout.extend_from_slice(expected);
        stdout.extend_from_slice(if jq {
            b"                        installed\n[3/5] Configs    running  checking config hooks                          Ns\n[3/5] Configs    ok       no config hooks                                Ns\n[4/5] Cleanup    running  normalizing worktree                           Ns\n[4/5] Cleanup    ok       no base repo                                   Ns\nDone in Ns. Reload your shell: source ~/.bashrc\n"
        } else {
            b"                    installed\n[3/5] Configs    running  checking config hooks                          Ns\n[3/5] Configs    ok       no config hooks                                Ns\n[4/5] Cleanup    running  normalizing worktree                           Ns\n[4/5] Cleanup    ok       no base repo                                   Ns\nDone in Ns. Reload your shell: source ~/.bashrc\n"
        });
        assert_cli(&rust_output, 0, &stdout, b"");
    }
}

#[test]
fn escaped_quote_provider_detail_pins_fallback_parser_limit() {
    let rust = Fixture::new("shdeps-provider-escaped-quote");
    let run = |fixture: &Fixture| {
        fixture
            .command()
            .env("PATH", fixture.closed_tool_path())
            .env("DOT_TEST_PROVIDER_ESCAPED_QUOTE_DETAIL", "1")
            .output()
            .expect("escaped quote provider event")
    };
    let rust_output = run(&rust);
    // The bootstrap sed parser captures through the slash before the quote,
    // then its `[^\"]*` expression stops. Pin that observable limitation.
    let expected = b"[1/5] Overlays   running  checking overlay links                         Ns\n[1/5] Overlays   ok       0 overlays current                             Ns\n[2/5] Tools      running  checking configured dependencies               Ns\n[2/5] Tools      changed  1 changed                                      Ns\n  changed  Cargo: 1 changed\n  changed  ripgrep                      said \\\n[3/5] Configs    running  checking config hooks                          Ns\n[3/5] Configs    ok       no config hooks                                Ns\n[4/5] Cleanup    running  normalizing worktree                           Ns\n[4/5] Cleanup    ok       no base repo                                   Ns\nDone in Ns. Reload your shell: source ~/.bashrc\n";
    assert_cli(&rust_output, 0, expected, b"");
}

#[test]
fn failed_reviewed_download_reports_and_stops_after_three_attempts() {
    let rust = Fixture::new("shdeps-provider-download-fail");
    let run = |fixture: &Fixture| {
        let mut command = fixture.command();
        command
            .env_remove("SHDEPS_LIB")
            .env("SHDEPS_DIR", fixture.home.join("missing-managed"))
            .env("SHDEPS_GIT_DEV_DIR", fixture.home.join("missing-dev"))
            .env("PATH", fixture.failing_curl_path())
            .env("DOT_TEST_CURL_RECORD", fixture.home.join("curl-record"))
            .env("_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS", "0");
        command.output().expect("failed download update")
    };
    let rust_output = run(&rust);
    assert_cli(
        &rust_output,
        1,
        UNAVAILABLE,
        b"  warning: Shdeps bootstrap download failed\n  warning: failed to fetch the reviewed Shdeps bootstrap\n",
    );
    assert_eq!(
        std::fs::read(rust.home.join("curl-record")).expect("native attempts"),
        b"attempt\nattempt\nattempt\n"
    );
}

#[test]
fn failed_reviewed_download_preserves_each_attempt_diagnostic() {
    let rust = Fixture::new("shdeps-provider-download-diagnostics");
    let output = rust
        .command()
        .env_remove("SHDEPS_LIB")
        .env("SHDEPS_DIR", rust.home.join("missing-managed"))
        .env("SHDEPS_GIT_DEV_DIR", rust.home.join("missing-dev"))
        .env("PATH", rust.failing_curl_path())
        .env("DOT_TEST_CURL_RECORD", rust.home.join("curl-record"))
        .env("DOT_TEST_CURL_DIAGNOSTIC", "1")
        .env("_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS", "0")
        .output()
        .expect("failed download update");

    assert_cli(
        &output,
        1,
        UNAVAILABLE,
        b"curl diagnostic\ncurl diagnostic\ncurl diagnostic\n  warning: Shdeps bootstrap download failed\n  warning: failed to fetch the reviewed Shdeps bootstrap\n",
    );
}
