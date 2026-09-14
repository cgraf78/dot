//! End-to-end parity for the native Shdeps provider coordinator.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::test_support::TempDir;

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

/// Locate a real host command for the deliberately closed fixture PATH.
///
/// The dotfiles-aware Git launcher in `~/.local/bin` uses `dot` itself, which
/// would recurse when the shell oracle runs under its synthetic environment.
/// Prefer the next ordinary command on PATH instead.
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
    home: PathBuf,
    state: PathBuf,
    provider: PathBuf,
    poison: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let scratch = TempDir::new(tag).expect("scratch");
        let root = scratch.path().join("dot-source");
        let home = scratch.path().join("home");
        let state = scratch.path().join("state");
        let provider = scratch.path().join("provider");
        std::fs::create_dir_all(root.join("support")).expect("support");
        std::fs::create_dir_all(home.join(".config/dot")).expect("config");
        std::fs::create_dir_all(&state).expect("state");
        std::fs::create_dir_all(&provider).expect("provider");
        let copied = Command::new("cp")
            .arg("-a")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("lib"))
            .arg(&root)
            .status()
            .expect("copy shell oracle");
        assert!(copied.success(), "copy shell oracle");

        let installer = provider.join("install.sh");
        write_exec(
            &installer,
            br#"#!/usr/bin/env bash
if [[ ${1:-} == --bootstrap ]]; then
  [[ ${DOT_TEST_PROVIDER_BOOTSTRAP_FAIL:-0} != 1 ]] || return 7
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
    [[ ${2:-} == version ]] || exit 2
    if [[ ${DOT_TEST_PROVIDER_ABI_LEAK_STDOUT:-0} == 1 ]]; then
      (sleep 30) &
      printf '%s\n' "$!" >"$DOT_TEST_PROVIDER_ABI_DESCENDANT_PID"
    fi
    if [[ ${DOT_TEST_PROVIDER_ABI_SLEEP:-0} == 1 ]]; then
      sleep "${DOT_TEST_PROVIDER_ABI_SLEEP_SECONDS:-5}"
    fi
    printf 'abi:1\n'
    ;;
  update)
    if [[ ${DOT_TEST_PROVIDER_LEAK_STDOUT:-0} == 1 ]]; then
      (sleep 30) &
      printf '%s\n' "$!" >"$DOT_TEST_PROVIDER_DESCENDANT_PID"
      exit 0
    fi
    if [[ ${DOT_TEST_PROVIDER_PROMPT:-0} == 1 ]]; then
      [[ -p ${SHDEPS_PROGRESS_PROMPT_ACK:-} ]] || exit 21
      printf '%s\n' '{"event":"prompt","status":"running","detail":"waiting"}'
      IFS= read -r -t 2 token <"$SHDEPS_PROGRESS_PROMPT_ACK" || exit 22
      [[ $token == ready ]] || exit 23
      printf '%s\n' "$token" >"$DOT_TEST_PROVIDER_PROMPT_RECORD"
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
    ;;
  *) exit 2 ;;
esac
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
        git(&root, &["add", "support/shdeps.lock", "lib"]);
        git(&root, &["commit", "-qm", "fixture"]);

        let poison = scratch.path().join("poison-bash");
        write_exec(
            &poison,
            b"#!/bin/sh\nprintf 'legacy provider adapter executed\\n' >&2\nexit 97\n",
        );
        Self {
            _scratch: scratch,
            root,
            home,
            state,
            provider,
            poison,
        }
    }

    fn command(&self, rust: bool) -> Command {
        let mut command = if rust {
            Command::new(env!("CARGO_BIN_EXE_dot"))
        } else {
            let mut shell = Command::new(dot::test_support::bash());
            shell.arg(self.root.join("lib/dot/main.sh"));
            shell
        };
        let path = std::env::var_os("PATH").unwrap_or_default();
        command
            .arg("update")
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", "")
            .env("XDG_STATE_HOME", &self.state)
            .env("DOT_SOURCE_ROOT", &self.root)
            .env("DOT_DEPENDENCY_PROVIDER", "shdeps")
            .env("DOT_SHDEPS_UPDATE_POLICY", "pinned")
            .env("SHDEPS_LIB", self.provider.join("shdeps.sh"))
            .env("SHDEPS_DIR", &self.provider)
            .env("DOT_TEST_PROVIDER_DIR", &self.provider)
            .env(
                "DOT_TEST_PROVIDER_RECORD",
                self.home.join("provider-record"),
            )
            .env("BASH", dot::test_support::bash())
            // Bash fills an absent SHELL with its own path. Pin it explicitly
            // so both engines produce the same final reload hint.
            .env("SHELL", dot::test_support::bash())
            .env("GIT_AUTHOR_NAME", "fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if rust {
            command.env("DOT_BASH", &self.poison);
        }
        command
    }

    fn run(&self, rust: bool) -> Output {
        self.command(rust).output().expect("run dot update")
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
            b"#!/bin/sh\nprintf 'attempt\\n' >>\"$DOT_TEST_CURL_RECORD\"\nexit 22\n",
        );
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&current));
        std::env::join_paths(paths).expect("fixture PATH")
    }

    /// Rebuild the inherited command path without `jq`. The shell intentionally
    /// falls back to its bootstrap JSON parser in this mode, so this must not
    /// depend on whether a host image happens to install jq.
    fn without_jq_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("without-jq-bin");
        std::fs::create_dir_all(&bin).expect("without-jq bin");
        // Bash is the fixture's only execution prerequisite. Every other
        // candidate is copied only when available: keeping PATH closed still
        // proves jq is absent, while an optional platform helper such as ps
        // remains absent for the real shell/native oracle to handle.
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

    /// Supply the shell's jq branch directly rather than borrowing a package
    /// from the host image. The fixture emits only the documented progress
    /// records below, and this NUL frame is the shell parser ABI.
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
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    output.status.success()
        && !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z')
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
fn elapsed_normalization_changes_only_ui_elapsed_positions() {
    let input = b"[1/5] Tools ok 2 current 10s\nDone in 2s\nDone with errors in 3s\nDone in 4s.\nDone with errors in 5s. Reload your shell: source ~/.bashrc\nDone in 6s. Reload your shell: source ~/.zshrc\n[hook diagnostic] 10s\n[one/5] malformed 8s\nretry after 10s\ncount=9s\xff\n";
    assert_eq!(
        normalize_elapsed(input),
        b"[1/5] Tools ok 2 current Ns\nDone in Ns\nDone with errors in Ns\nDone in Ns.\nDone with errors in Ns. Reload your shell: source ~/.bashrc\nDone in Ns. Reload your shell: source ~/.zshrc\n[hook diagnostic] 10s\n[one/5] malformed 8s\nretry after 10s\ncount=9s\xff\n"
    );
}

#[test]
fn explicit_reviewed_provider_runs_natively() {
    let shell = Fixture::new("shdeps-provider-shell");
    let rust = Fixture::new("shdeps-provider-rust");
    let shell_output = shell.run(false);
    assert!(
        shell_output.status.success(),
        "shell stderr: {}",
        String::from_utf8_lossy(&shell_output.stderr)
    );
    let rust_output = rust.run(true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    assert_eq!(
        std::fs::read(rust.home.join("provider-record")).expect("native provider record"),
        std::fs::read(shell.home.join("provider-record")).expect("shell provider record")
    );
}

#[test]
fn bootstrap_selected_binary_is_the_only_executable_authority() {
    let shell = Fixture::new("shdeps-provider-selected-bin-shell");
    let rust = Fixture::new("shdeps-provider-selected-bin-rust");
    let run = |fixture: &Fixture, native: bool| {
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
            .command(native)
            .env("DOT_TEST_BOOTSTRAP_BIN", &alternate)
            .env("DOT_TEST_RECORD_BINARY_MARKER", "1")
            .output()
            .expect("selected provider binary")
    };
    let shell_output = run(&shell, false);
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        std::fs::read(rust.home.join("provider-record")).expect("native binary marker"),
        std::fs::read(shell.home.join("provider-record")).expect("shell binary marker")
    );

    let shell = Fixture::new("shdeps-provider-rejected-bin-shell");
    let rust = Fixture::new("shdeps-provider-rejected-bin-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .env("DOT_TEST_BOOTSTRAP_BIN", "relative/shdeps")
            .output()
            .expect("rejected provider binary")
    };
    assert_eq!(
        run(&rust, true).status.code(),
        run(&shell, false).status.code()
    );
}

#[test]
fn missing_provider_uses_reviewed_download_natively() {
    let shell = Fixture::new("shdeps-provider-download-shell");
    let rust = Fixture::new("shdeps-provider-download-rust");
    let run = |fixture: &Fixture, native: bool| {
        let mut command = fixture.command(native);
        command
            .env_remove("SHDEPS_LIB")
            .env("SHDEPS_DIR", fixture.home.join("managed-shdeps"))
            .env("SHDEPS_GIT_DEV_DIR", fixture.home.join("missing-dev"))
            .env("PATH", fixture.curl_path())
            .env("DOT_TEST_CURL_RECORD", fixture.home.join("curl-record"));
        command.output().expect("download update")
    };
    let shell_output = run(&shell, false);
    assert!(
        shell_output.status.success(),
        "shell stderr: {}",
        String::from_utf8_lossy(&shell_output.stderr)
    );
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    assert_eq!(
        std::fs::read(rust.home.join("curl-record")).expect("native download record"),
        std::fs::read(shell.home.join("curl-record")).expect("shell download record")
    );
}

#[test]
fn provider_source_change_reexecs_once_natively() {
    let shell = Fixture::new("shdeps-provider-reexec-shell");
    let rust = Fixture::new("shdeps-provider-reexec-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .env("DOT_TEST_PROVIDER_ADVANCE_SOURCE", "1")
            .env(
                "DOT_TEST_PROVIDER_ADVANCED",
                fixture.home.join("provider-advanced"),
            )
            .output()
            .expect("reexec update")
    };
    let shell_output = run(&shell, false);
    assert!(
        shell_output.status.success(),
        "shell stderr: {}",
        String::from_utf8_lossy(&shell_output.stderr)
    );
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    assert!(rust.home.join("provider-advanced").exists());
    assert!(!rust.state.join("dot/provider-reexec-failed").exists());
}

#[test]
fn provider_abi_probe_obeys_its_deadline_natively() {
    let shell = Fixture::new("shdeps-provider-abi-timeout-shell");
    let rust = Fixture::new("shdeps-provider-abi-timeout-rust");
    let run = |fixture: &Fixture, native: bool| {
        let started = std::time::Instant::now();
        let output = fixture
            .command(native)
            .env("DOT_TEST_PROVIDER_ABI_SLEEP", "1")
            // Keep the fixture's blocked child much longer than the generous
            // CI deadline, so this remains a no-hang assertion under load.
            .env("DOT_TEST_PROVIDER_ABI_SLEEP_SECONDS", "30")
            .env("_DOT_SHDEPS_ABI_TIMEOUT_SECONDS", "1")
            .output()
            .expect("timed ABI update");
        (output, started.elapsed())
    };
    let (shell_output, shell_elapsed) = run(&shell, false);
    assert_eq!(shell_output.status.code(), Some(1));
    assert!(shell_elapsed < std::time::Duration::from_secs(10));
    let timeout_warning = b"warning: Shdeps provider ABI probe timed out after 1s";
    assert!(
        shell_output
            .stderr
            .windows(timeout_warning.len())
            .any(|row| row == timeout_warning),
        "shell stderr: {}",
        String::from_utf8_lossy(&shell_output.stderr),
    );
    let (rust_output, rust_elapsed) = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert!(
        rust_elapsed < std::time::Duration::from_secs(10),
        "native ABI probe ran for {rust_elapsed:?}"
    );
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert!(
        rust_output
            .stderr
            .windows(timeout_warning.len())
            .any(|row| row == timeout_warning),
        "native stderr: {}",
        String::from_utf8_lossy(&rust_output.stderr),
    );
}

#[test]
fn completed_abi_probe_cleans_inherited_stdout_descendants() {
    let rust = Fixture::new("shdeps-provider-abi-stdout-descendant");
    let pid_file = rust.home.join("abi-descendant-pid");
    let mut command = rust.command(true);
    command
        .env("DOT_TEST_PROVIDER_ABI_LEAK_STDOUT", "1")
        .env("DOT_TEST_PROVIDER_ABI_DESCENDANT_PID", &pid_file);
    let output = bounded_output(command, 10);
    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
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
    let shell = Fixture::new("shdeps-provider-flags-shell");
    let rust = Fixture::new("shdeps-provider-flags-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .arg("--force")
            .arg("--quiet")
            .env("DOT_UPDATE_JOBS", "3")
            .env("DOT_TEST_PROVIDER_RECORD_PATH", "1")
            .output()
            .expect("quiet forced update")
    };
    let shell_output = run(&shell, false);
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    assert_eq!(rust_output.stdout, b"");
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
    let shell = Fixture::new("shdeps-provider-failure-shell");
    let rust = Fixture::new("shdeps-provider-failure-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .env("DOT_TEST_PROVIDER_FAIL", "1")
            .output()
            .expect("failed provider update")
    };
    let shell_output = run(&shell, false);
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    assert!(String::from_utf8_lossy(&rust_output.stdout).contains("network unavailable"));
}

#[test]
fn second_provider_source_change_publishes_checkpoint_natively() {
    let shell = Fixture::new("shdeps-provider-checkpoint-shell");
    let rust = Fixture::new("shdeps-provider-checkpoint-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .env("DOT_TEST_PROVIDER_ADVANCE_TWICE", "1")
            .env(
                "DOT_TEST_PROVIDER_ADVANCED",
                fixture.home.join("provider-advanced"),
            )
            .output()
            .expect("double-change update")
    };
    let shell_output = run(&shell, false);
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(rust_output.status.code(), Some(1));
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    let shell_checkpoint = shell.state.join("dot/provider-reexec-failed");
    let rust_checkpoint = rust.state.join("dot/provider-reexec-failed");
    for checkpoint in [&rust_checkpoint, &shell_checkpoint] {
        let body = std::fs::read_to_string(checkpoint).expect("checkpoint");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "cgraf78 dot provider reexec checkpoint v1");
        let before = lines[1].strip_prefix("before=").expect("before field");
        let after = lines[2].strip_prefix("after=").expect("after field");
        assert!(dot::shdeps::revision_valid(before));
        assert!(dot::shdeps::revision_valid(after));
        assert_ne!(before, after);
    }
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
        let shell = Fixture::new(&format!("shdeps-provider-{name}-shell"));
        let rust = Fixture::new(&format!("shdeps-provider-{name}-rust"));
        let shell_dev = (source > 0).then(|| shell.development(source == 1));
        let rust_dev = (source > 0).then(|| rust.development(source == 1));
        let run = |fixture: &Fixture, native: bool, dev: Option<&PathBuf>| {
            let mut command = fixture.command(native);
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
        let shell_output = run(&shell, false, shell_dev.as_ref());
        assert!(
            shell_output.status.success(),
            "{name} shell stderr: {}",
            String::from_utf8_lossy(&shell_output.stderr)
        );
        let rust_output = run(&rust, true, rust_dev.as_ref());
        assert_eq!(
            rust_output.status.code(),
            shell_output.status.code(),
            "{name}"
        );
        assert_eq!(
            normalize_elapsed(&rust_output.stdout),
            normalize_elapsed(&shell_output.stdout),
            "{name}"
        );
        assert_eq!(rust_output.stderr, shell_output.stderr, "{name}");
    }
}

#[test]
fn bootstrap_and_abi_failures_match_shell_natively() {
    for (name, bootstrap, abi) in [("bootstrap", true, false), ("abi", false, true)] {
        let shell = Fixture::new(&format!("shdeps-provider-{name}-shell"));
        let rust = Fixture::new(&format!("shdeps-provider-{name}-rust"));
        if abi {
            for fixture in [&shell, &rust] {
                let binary = fixture.provider.join("shdeps");
                let body = std::fs::read_to_string(&binary)
                    .expect("provider binary")
                    .replace("printf 'abi:1", "printf 'abi:2");
                write_exec(&binary, body.as_bytes());
            }
        }
        let run = |fixture: &Fixture, native: bool| {
            fixture
                .command(native)
                .env(
                    "DOT_TEST_PROVIDER_BOOTSTRAP_FAIL",
                    if bootstrap { "1" } else { "0" },
                )
                .output()
                .expect("provider refusal")
        };
        let shell_output = run(&shell, false);
        let rust_output = run(&rust, true);
        assert_eq!(
            rust_output.status.code(),
            shell_output.status.code(),
            "{name}"
        );
        assert_eq!(rust_output.status.code(), Some(1), "{name}");
        assert_eq!(
            normalize_elapsed(&rust_output.stdout),
            normalize_elapsed(&shell_output.stdout),
            "{name}"
        );
        assert_eq!(rust_output.stderr, shell_output.stderr, "{name}");
    }
}

#[test]
fn verbose_provider_events_render_in_order_natively() {
    let shell = Fixture::new("shdeps-provider-verbose-shell");
    let rust = Fixture::new("shdeps-provider-verbose-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .arg("--verbose")
            .env("DOT_TEST_PROVIDER_VERBOSE_EVENTS", "1")
            .output()
            .expect("verbose provider update")
    };
    let shell_output = run(&shell, false);
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
}

const PROMPT_CHANGED: &[u8] =
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

#[test]
fn provider_prompt_rendezvous_acknowledges_natively() {
    let rust = Fixture::new("shdeps-provider-prompt-rust");
    let mut command = rust.command(true);
    command.env("DOT_TEST_PROVIDER_PROMPT", "1").env(
        "DOT_TEST_PROVIDER_PROMPT_RECORD",
        rust.home.join("prompt-record"),
    );
    let output = bounded_output(command, 10);
    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert_eq!(normalize_elapsed(&output.stdout), PROMPT_CHANGED);
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read(rust.home.join("prompt-record")).expect("native prompt acknowledgment"),
        b"ready\n"
    );
}

#[test]
fn provider_exit_does_not_wait_for_or_leak_stdout_descendants() {
    let rust = Fixture::new("shdeps-provider-stdout-descendant");
    let pid_file = rust.home.join("descendant-pid");
    let mut command = rust.command(true);
    command
        .env("DOT_TEST_PROVIDER_LEAK_STDOUT", "1")
        .env("DOT_TEST_PROVIDER_DESCENDANT_PID", &pid_file);
    let output = bounded_output(command, 10);
    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
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
fn escaped_unicode_provider_event_matches_shell_with_and_without_jq() {
    for (name, jq, expected) in [
        ("with-jq", true, b"caf\xc3\xa9".as_slice()),
        ("without-jq", false, b"caf\\u00e9".as_slice()),
    ] {
        let shell = Fixture::new(&format!("shdeps-provider-unicode-{name}-shell"));
        let rust = Fixture::new(&format!("shdeps-provider-unicode-{name}-rust"));
        let run = |fixture: &Fixture, native: bool| {
            let path = if jq {
                fixture.jq_path()
            } else {
                fixture.without_jq_path()
            };
            fixture
                .command(native)
                .env("PATH", path)
                .env("DOT_TEST_PROVIDER_ESCAPED_EVENT", "1")
                .output()
                .expect("escaped provider event")
        };
        let shell_output = run(&shell, false);
        assert!(
            shell_output
                .stdout
                .windows(expected.len())
                .any(|row| row == expected),
            "{name} shell stdout: {}; stderr: {}",
            String::from_utf8_lossy(&shell_output.stdout),
            String::from_utf8_lossy(&shell_output.stderr),
        );
        let rust_output = run(&rust, true);
        assert_eq!(
            rust_output.status.code(),
            shell_output.status.code(),
            "{name}; shell stdout={} stderr={}; native stdout={} stderr={}",
            String::from_utf8_lossy(&shell_output.stdout),
            String::from_utf8_lossy(&shell_output.stderr),
            String::from_utf8_lossy(&rust_output.stdout),
            String::from_utf8_lossy(&rust_output.stderr),
        );
        assert_eq!(
            normalize_elapsed(&rust_output.stdout),
            normalize_elapsed(&shell_output.stdout),
            "{name}"
        );
        assert_eq!(rust_output.stderr, shell_output.stderr, "{name}");
    }
}

#[test]
fn escaped_quote_provider_detail_matches_shell_without_jq() {
    let shell = Fixture::new("shdeps-provider-escaped-quote-shell");
    let rust = Fixture::new("shdeps-provider-escaped-quote-rust");
    let run = |fixture: &Fixture, native: bool| {
        fixture
            .command(native)
            .env("PATH", fixture.without_jq_path())
            .env("DOT_TEST_PROVIDER_ESCAPED_QUOTE_DETAIL", "1")
            .output()
            .expect("escaped quote provider event")
    };
    let shell_output = run(&shell, false);
    assert!(
        shell_output.status.success(),
        "shell stderr: {:?}",
        shell_output.stderr
    );
    // The bootstrap sed parser captures through the slash before the quote,
    // then its `[^\"]*` expression stops. Pin that observable limitation.
    assert!(
        shell_output
            .stdout
            .windows(b"said \\".len())
            .any(|row| row == b"said \\"),
        "shell stdout: {}",
        String::from_utf8_lossy(&shell_output.stdout),
    );
    assert!(
        !shell_output
            .stdout
            .windows(b"hello".len())
            .any(|row| row == b"hello"),
        "shell stdout retained text after escaped quote: {}",
        String::from_utf8_lossy(&shell_output.stdout),
    );
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
}

#[test]
fn failed_reviewed_download_reports_and_stops_after_three_attempts() {
    let shell = Fixture::new("shdeps-provider-download-fail-shell");
    let rust = Fixture::new("shdeps-provider-download-fail-rust");
    let run = |fixture: &Fixture, native: bool| {
        let mut command = fixture.command(native);
        command
            .env_remove("SHDEPS_LIB")
            .env("SHDEPS_DIR", fixture.home.join("missing-managed"))
            .env("SHDEPS_GIT_DEV_DIR", fixture.home.join("missing-dev"))
            .env("PATH", fixture.failing_curl_path())
            .env("DOT_TEST_CURL_RECORD", fixture.home.join("curl-record"))
            .env("_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS", "0");
        command.output().expect("failed download update")
    };
    let shell_output = run(&shell, false);
    let rust_output = run(&rust, true);
    assert_eq!(rust_output.status.code(), shell_output.status.code());
    assert_eq!(
        normalize_elapsed(&rust_output.stdout),
        normalize_elapsed(&shell_output.stdout)
    );
    assert_eq!(rust_output.stderr, shell_output.stderr);
    assert_eq!(
        std::fs::read(rust.home.join("curl-record")).expect("native attempts"),
        b"attempt\nattempt\nattempt\n"
    );
}
