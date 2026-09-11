//! End-to-end native contracts for the Shdeps provider coordinator.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

/// Keep provider-boundary tests independent of developer command wrappers.
fn isolated_tool_path() -> std::ffi::OsString {
    let mut directories = Vec::new();
    for name in ["bash", "git"] {
        let command = fixture_command(name).unwrap_or_else(|| panic!("fixture requires {name}"));
        let directory = command
            .parent()
            .expect("fixture command parent")
            .to_path_buf();
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    // The closed PATH must still resolve the OS tools the engine shells
    // out to (notably PATH-resolved `ps` for update-lock identity and
    // `mv` for atomic moves): on macOS those live in /bin, which the
    // bash/git homes do not cover. Developer wrappers stay excluded by
    // construction; only genuine system directories are appended.
    for directory in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let directory = PathBuf::from(directory);
        if directory.is_dir() && !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    std::env::join_paths(directories).expect("isolated tool PATH")
}

#[test]
fn isolated_tool_path_resolves_engine_os_tools() {
    // The engine shells out to PATH-resolved `ps` (update-lock identity)
    // and `mv` (atomic moves). If the closed PATH stops resolving them,
    // provider-boundary tests fail with a bare exit status instead of a
    // clear message — pin the resolution directly. (On macOS both live
    // in /bin, outside every bash/git home.)
    let path = isolated_tool_path();
    for name in ["bash", "git", "ps", "mv"] {
        let found = std::env::split_paths(&path).any(|directory| {
            let candidate = directory.join(name);
            candidate.is_file()
                && candidate
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        });
        assert!(
            found,
            "{name} must resolve under the isolated tool PATH ({path:?})"
        );
    }
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
        let mut command = Command::new(&self.binary);
        let path = std::env::var_os("PATH").unwrap_or_default();
        command
            .arg("update")
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

    /// Rebuild the inherited command path without `jq`. The provider intentionally
    /// falls back to its bootstrap JSON parser in this mode, so this must not
    /// depend on whether a host image happens to install jq.
    fn without_jq_path(&self) -> std::ffi::OsString {
        let bin = self.home.join("without-jq-bin");
        std::fs::create_dir_all(&bin).expect("without-jq bin");
        // Bash is the fixture's only execution prerequisite. Every other
        // candidate is copied only when available: keeping PATH closed still
        // proves jq is absent, while an optional platform helper such as ps
        // remains absent so the native fallback owns the result.
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
    assert_eq!(output.status.code(), Some(status));
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
fn provider_processes_do_not_evaluate_bash_env() {
    let rust = Fixture::new("shdeps-provider-bash-env");
    let poison = rust.home.join("bash-env");
    let marker = rust.home.join("bash-env-ran");
    std::fs::write(&poison, format!("printf poison >>'{}'\n", marker.display()))
        .expect("BASH_ENV poison");

    let output = rust
        .command()
        .env("BASH_ENV", &poison)
        .env("PATH", isolated_tool_path())
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
        "() {{ command touch '{}'; builtin printf \"$@\"; }}",
        marker.display()
    );

    let output = rust
        .command()
        .env("BASH_FUNC_printf%%", function)
        .env("PATH", isolated_tool_path())
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
                .replace("printf 'abi:1", "printf 'abi:2");
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
fn provider_prompt_rendezvous_acknowledges_natively() {
    let rust = Fixture::new("shdeps-provider-prompt");
    let mut command = rust.command();
    command.env("DOT_TEST_PROVIDER_PROMPT", "1").env(
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
                fixture.without_jq_path()
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
            .env("PATH", fixture.without_jq_path())
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
