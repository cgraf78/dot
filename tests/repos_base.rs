//! Differential parity tests for `src/repos_base.rs` against the live
//! shell (`lib/dot/repos/model.sh`): `_base_repo_exists` / `_base_git`
//! dispatch, the overlay `path|sync` read idiom, and `run_git` stdio.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::repos_base::{Base, Topology, overlay_path_sync, run_git};
use dot::test_support::TempDir;

/// Run one shell snippet with `model.sh` sourced.
///
/// The preamble stubs `dot_xdg_path` (always fails) BEFORE sourcing:
/// `model.sh` runs `_dot_client_select` at source time and the stub
/// forces the no-record path. Sourcing happens under a neutral `$HOME`
/// (an empty dir, so selection lands on silent `missing`); each snippet
/// then `export`s `DOT_BASE_TOPOLOGY` / `DOT_CLIENT_GIT_DIR` / `HOME`
/// per case, overriding the source-time selection.
fn shell_run(neutral_home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        "dot_xdg_path() {{ return 1; }}\n. \"$1/lib/dot/repos/model.sh\"\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", neutral_home)
        .env("DOT_TEST", "1")
        .current_dir(neutral_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Helper status from a dump harness: the process exit is always 0
/// (the dump `printf` runs last).
fn dump_rc(dump: &[u8]) -> i32 {
    let line = dump.split(|byte| *byte == b'\n').next().unwrap_or(b"");
    let line = line.strip_prefix(b"rc=").unwrap_or(b"");
    std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(-1)
}

/// Single-quote a string for embedding in a bash snippet.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Byte offset of the first `needle` occurrence in `haystack`.
fn find_marker(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Run a `git` fixture command, silenced; panics with context on failure.
fn git_fixture(args: &[&str], cwd: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(
        status.success(),
        "fixture git {args:?} in {}",
        cwd.display()
    );
}

/// Separate-topology fixture: bare git dir plus a `$HOME` worktree with
/// one commit, made through the `--git-dir/--work-tree` prefix form.
fn fixture_separate(home_label: &str, git_label: &str) -> (TempDir, TempDir) {
    let home = TempDir::new(home_label).expect("temp home");
    let git_dir = TempDir::new(git_label).expect("temp git dir");
    let git = git_dir.path().to_string_lossy().into_owned();
    let work = home.path().to_string_lossy().into_owned();
    git_fixture(&["init", "--bare", "-q", &git], home.path());
    std::fs::write(home.path().join("file.txt"), b"hello\n").expect("write fixture");
    git_fixture(
        &[
            &format!("--git-dir={git}"),
            &format!("--work-tree={work}"),
            "add",
            "file.txt",
        ],
        home.path(),
    );
    git_fixture(
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            &format!("--git-dir={git}"),
            &format!("--work-tree={work}"),
            "commit",
            "-qm",
            "init",
        ],
        home.path(),
    );
    (home, git_dir)
}

/// Ordinary-topology fixture: `$HOME` itself is a checkout with one commit.
fn fixture_ordinary(label: &str) -> TempDir {
    let home = TempDir::new(label).expect("temp home");
    git_fixture(&["init", "-q"], home.path());
    std::fs::write(home.path().join("file.txt"), b"hello\n").expect("write fixture");
    git_fixture(&["add", "file.txt"], home.path());
    git_fixture(
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
        home.path(),
    );
    home
}

/// Run `_base_git <args>` under the shell for one topology and split the
/// harness stdout into `(inner rc, git stdout bytes, git stderr bytes)`.
///
/// Layout: `rc=<n>\n<git stdout>\n__ERR__\n<git stderr>`; the marker is
/// safe because these plumbing outputs never contain that line.
fn shell_base_git(
    neutral: &Path,
    topology: &str,
    git_dir: &Path,
    home: &Path,
    args: &[&str],
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut argv = String::new();
    for arg in args {
        argv.push(' ');
        argv.push_str(&sh_quote(arg));
    }
    let snippet = format!(
        "export DOT_BASE_TOPOLOGY={topology} DOT_CLIENT_GIT_DIR={} HOME={}\n\
         _t=$(mktemp -d)\n\
         _base_git{argv} >\"$_t/out\" 2>\"$_t/err\"\n\
         printf 'rc=%s\\n' \"$?\"\n\
         cat \"$_t/out\"\n\
         printf '\\n__ERR__\\n'\n\
         cat \"$_t/err\"\n\
         rm -rf \"$_t\"\n",
        sh_quote(&git_dir.to_string_lossy()),
        sh_quote(&home.to_string_lossy()),
    );
    let (wrap_rc, out, err) = shell_run(neutral, &snippet);
    assert_eq!(wrap_rc, 0, "wrapper failed: {snippet}");
    assert!(err.is_empty(), "harness stderr must be silent: {err:?}");
    let marker = b"\n__ERR__\n";
    let pos = find_marker(&out, marker).expect("marker line in wrapper stdout");
    let (head, tail) = out.split_at(pos);
    let git_err = tail[marker.len()..].to_vec();
    let rc = dump_rc(head);
    assert!(rc >= 0, "unparseable rc line: {head:?}");
    let nl = head
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("rc line");
    let git_out = head[nl + 1..].to_vec();
    (rc, git_out, git_err)
}

/// Run `args` through both the Rust prefix and the shell `_base_git` and
/// compare exit codes plus exact stdout bytes.
///
/// `run_git` nulls stderr by construction while the shell passes git's
/// stderr through, so the caller states the expected shell stderr bytes:
/// empty for silent plumbing, the exact `fatal:` bytes for the failing
/// `rev-parse --verify` case.
fn assert_dispatch_parity(
    topology: Topology,
    shell_topology: &str,
    git_dir: &Path,
    home: &Path,
    args: &[&str],
    want_shell_err: &[u8],
) -> (i32, Vec<u8>) {
    let neutral = TempDir::new("repos-base-neutral").expect("temp neutral");
    let (shell_rc, shell_out, shell_err) =
        shell_base_git(neutral.path(), shell_topology, git_dir, home, args);
    assert_eq!(shell_err, want_shell_err, "shell stderr for {args:?}");
    let base = Base {
        topology,
        client_git_dir: git_dir.to_string_lossy().into_owned(),
        home: home.to_string_lossy().into_owned(),
    };
    let prefix = base
        .git_prefix()
        .expect("non-missing topology has a prefix");
    let output = run_git(&prefix, args).expect("spawn git");
    assert_eq!(
        output.status.code(),
        Some(shell_rc),
        "exit code parity for {args:?}"
    );
    assert_eq!(output.stdout, shell_out, "stdout bytes parity for {args:?}");
    assert!(
        output.stderr.is_empty(),
        "run_git nulls stderr for {args:?}"
    );
    (shell_rc, shell_out)
}

/// `(entry, want_path, want_sync)` matrix shared by the pure and shell
/// overlay tests.
fn overlay_cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("n|p|u|d|o|git", "p", "git"),
        ("n|p", "p", "git"),
        ("n|p|u|d|o|", "p", "git"),
        ("n|p|u|d|o|none", "p", "none"),
        ("n|p|u|d|o|git|x", "p", "git|x"),
        ("", "", "git"),
        ("plain-no-pipes", "", "git"),
    ]
}

#[test]
fn exists_matrix() {
    for (topology, want) in [
        (Topology::Missing, false),
        (Topology::Separate, true),
        (Topology::Ordinary, true),
    ] {
        let base = Base {
            topology,
            client_git_dir: String::from("/git"),
            home: String::from("/home/u"),
        };
        assert_eq!(base.exists(), want, "{topology:?}");
    }
}

#[test]
fn git_prefix_argv() {
    let separate = Base {
        topology: Topology::Separate,
        client_git_dir: String::from("/g/dir"),
        home: String::from("/home/u"),
    };
    assert_eq!(
        separate.git_prefix(),
        Some(vec![
            OsString::from("--git-dir=/g/dir"),
            OsString::from("--work-tree=/home/u"),
        ])
    );
    let ordinary = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::from("/g/dir"),
        home: String::from("/home/u"),
    };
    assert_eq!(
        ordinary.git_prefix(),
        Some(vec![OsString::from("-C"), OsString::from("/home/u")])
    );
    let missing = Base {
        topology: Topology::Missing,
        client_git_dir: String::from("/g/dir"),
        home: String::from("/home/u"),
    };
    assert_eq!(missing.git_prefix(), None);
}

#[test]
fn separate_show_toplevel_parity() {
    let (home, git_dir) = fixture_separate("repos-base-topo-sep-home", "repos-base-topo-sep-git");
    let (rc, out) = assert_dispatch_parity(
        Topology::Separate,
        "separate",
        git_dir.path(),
        home.path(),
        &["rev-parse", "--show-toplevel"],
        b"",
    );
    assert_eq!(rc, 0);
    assert!(!out.is_empty(), "toplevel must print the work tree");
    assert!(out.ends_with(b"\n"), "toplevel ends with newline: {out:?}");
}

#[test]
fn ordinary_show_toplevel_parity() {
    let home = fixture_ordinary("repos-base-topo-ord-home");
    let (rc, out) = assert_dispatch_parity(
        Topology::Ordinary,
        "ordinary",
        home.path(),
        home.path(),
        &["rev-parse", "--show-toplevel"],
        b"",
    );
    assert_eq!(rc, 0);
    assert!(!out.is_empty(), "toplevel must print the work tree");
    assert!(out.ends_with(b"\n"), "toplevel ends with newline: {out:?}");
}

#[test]
fn verify_failure_parity() {
    let args = ["rev-parse", "--verify", "no-such-ref"];
    // The shell passes git's `fatal:` line through on stderr while
    // `run_git` nulls it, so the expected shell stderr is pinned here
    // (exact bytes; re-pin if the git version rewords the message) and
    // the Rust side still asserts empty stderr inside the helper.
    let want_err = b"fatal: Needed a single revision\n";
    let (sep_home, sep_git) = fixture_separate("repos-base-vfy-sep-home", "repos-base-vfy-sep-git");
    let (rc, out) = assert_dispatch_parity(
        Topology::Separate,
        "separate",
        sep_git.path(),
        sep_home.path(),
        &args,
        want_err,
    );
    assert_ne!(rc, 0, "bad ref must fail");
    assert!(out.is_empty(), "failed verify prints nothing: {out:?}");
    let ord_home = fixture_ordinary("repos-base-vfy-ord-home");
    let (rc, out) = assert_dispatch_parity(
        Topology::Ordinary,
        "ordinary",
        ord_home.path(),
        ord_home.path(),
        &args,
        want_err,
    );
    assert_ne!(rc, 0, "bad ref must fail");
    assert!(out.is_empty(), "failed verify prints nothing: {out:?}");
}

#[test]
fn status_porcelain_parity() {
    // One untracked file: both engines must report exactly one `??`
    // row with identical bytes on every platform (an empty
    // directory would print nothing anywhere, so seed a file).
    let args = ["status", "--porcelain"];
    let (sep_home, sep_git) = fixture_separate("repos-base-por-sep-home", "repos-base-por-sep-git");
    std::fs::write(sep_home.path().join("scratch.txt"), b"scratch\n").expect("seed untracked");
    let (rc, out) = assert_dispatch_parity(
        Topology::Separate,
        "separate",
        sep_git.path(),
        sep_home.path(),
        &args,
        b"",
    );
    assert_eq!(rc, 0);
    // Exact bytes already match across engines inside the helper;
    // here pin the scratch row's presence (other environment rows,
    // like a launcher cache dir, may legitimately appear).
    let rows: Vec<&[u8]> = out
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert!(
        rows.contains(&b"?? scratch.txt".as_slice()),
        "untracked row present: {out:?}"
    );
    let ord_home = fixture_ordinary("repos-base-por-ord-home");
    std::fs::write(ord_home.path().join("scratch.txt"), b"scratch\n").expect("seed untracked");
    let (rc, out) = assert_dispatch_parity(
        Topology::Ordinary,
        "ordinary",
        ord_home.path(),
        ord_home.path(),
        &args,
        b"",
    );
    assert_eq!(rc, 0);
    let rows: Vec<&[u8]> = out
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert!(
        rows.contains(&b"?? scratch.txt".as_slice()),
        "untracked row present: {out:?}"
    );
}

#[test]
fn missing_refuses() {
    let home = TempDir::new("repos-base-missing-home").expect("temp home");
    let git_dir = TempDir::new("repos-base-missing-git").expect("temp git dir");
    let neutral = TempDir::new("repos-base-neutral").expect("temp neutral");
    let (rc, out, err) = shell_base_git(
        neutral.path(),
        "missing",
        git_dir.path(),
        home.path(),
        &["rev-parse", "--show-toplevel"],
    );
    assert_eq!(rc, 128, "shell _base_git refuses missing with 128");
    assert!(out.is_empty(), "missing prints nothing: {out:?}");
    assert!(err.is_empty(), "missing is silent: {err:?}");
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: git_dir.path().to_string_lossy().into_owned(),
        home: home.path().to_string_lossy().into_owned(),
    };
    assert!(!base.exists());
    assert_eq!(base.git_prefix(), None);
}

#[test]
fn overlay_path_sync_matrix() {
    for (entry, want_path, want_sync) in overlay_cases() {
        let (path, sync) = overlay_path_sync(entry);
        assert_eq!(path, want_path, "path for {entry:?}");
        assert_eq!(sync, want_sync, "sync for {entry:?}");
    }
    // The seven-field remainder stays glued and must NOT equal "git".
    let (_, sync) = overlay_path_sync("n|p|u|d|o|git|x");
    assert_ne!(sync, "git");
}

#[test]
fn overlay_path_sync_shell_parity() {
    let neutral = TempDir::new("repos-base-neutral").expect("temp neutral");
    let cases = overlay_cases();
    let mut snippet = String::new();
    for (entry, _, _) in &cases {
        snippet.push_str(&format!(
            "IFS='|' read -r _ path _ _ _ sync <<<{}; sync=\"${{sync:-git}}\"; \
             printf '%s|%s\\n' \"$path\" \"$sync\"\n",
            sh_quote(entry)
        ));
    }
    let (wrap_rc, out, err) = shell_run(neutral.path(), &snippet);
    assert_eq!(wrap_rc, 0);
    assert!(err.is_empty(), "read idiom is silent: {err:?}");
    let mut lines: Vec<&[u8]> = out.split(|byte| *byte == b'\n').collect();
    assert_eq!(
        lines.pop(),
        Some(b"".as_slice()),
        "snippet output ends with newline"
    );
    assert_eq!(lines.len(), cases.len(), "one row per case: {out:?}");
    for (index, ((entry, want_path, want_sync), line)) in cases.iter().zip(lines.iter()).enumerate()
    {
        let want = format!("{want_path}|{want_sync}");
        assert_eq!(
            *line,
            want.as_bytes(),
            "shell idiom case {index} ({entry:?})"
        );
        let (path, sync) = overlay_path_sync(entry);
        assert_eq!(
            format!("{path}|{sync}").as_bytes(),
            *line,
            "rust parity case {index} ({entry:?})"
        );
    }
}

#[test]
fn run_git_probe() {
    let home = fixture_ordinary("repos-base-probe-home");
    let neutral = TempDir::new("repos-base-probe-neutral").expect("temp neutral");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: home.path().to_string_lossy().into_owned(),
        home: home.path().to_string_lossy().into_owned(),
    };
    let prefix = base.git_prefix().expect("ordinary has a prefix");

    // Failing command: Some output, non-success status, nulled stderr.
    let fail = run_git(&prefix, &["rev-parse", "--verify", "no-such-ref"]).expect("spawn git");
    assert!(!fail.status.success(), "bad ref must fail");
    assert!(fail.stderr.is_empty(), "run_git nulls stderr");
    assert!(fail.stdout.is_empty(), "failed verify prints nothing");

    // Success: stdout bytes match the shell command substitution.
    let ok = run_git(&prefix, &["rev-parse", "--show-toplevel"]).expect("spawn git");
    assert!(ok.status.success(), "toplevel must succeed");
    assert!(ok.stderr.is_empty(), "run_git nulls stderr");
    let snippet = format!(
        "export DOT_BASE_TOPOLOGY=ordinary DOT_CLIENT_GIT_DIR={} HOME={}\n\
         probe=$(_base_git rev-parse --show-toplevel)\n\
         printf '%s' \"$probe\"\n",
        sh_quote(&home.path().to_string_lossy()),
        sh_quote(&home.path().to_string_lossy()),
    );
    let (wrap_rc, shell_out, shell_err) = shell_run(neutral.path(), &snippet);
    assert_eq!(wrap_rc, 0);
    assert!(
        shell_err.is_empty(),
        "probe harness is silent: {shell_err:?}"
    );
    // $(...) strips trailing newlines: the shell bytes are the Rust
    // bytes minus exactly one trailing newline.
    assert!(ok.stdout.ends_with(b"\n"), "toplevel ends with newline");
    assert_eq!(&ok.stdout[..ok.stdout.len() - 1], shell_out.as_slice());
}
