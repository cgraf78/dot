//! Differential parity tests for `src/repos_git.rs` against the live
//! shell (`lib/dot/repos/git.sh`): repo-set iteration, streaming git
//! invocation, and fetch with `FETCH_HEAD` clamping.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};

use dot::repos_base::{Base, Topology};
use dot::repos_git;
use dot::test_support::TempDir;

/// Sources plus the `OVERLAYS` loader the shell iteration reads.
/// `model.sh` runs `_dot_client_select` on load, which prints a
/// diagnostic whenever `$HOME` already holds a repo; that loader
/// noise is suppressed (the functions under test run afterwards
/// with an explicit topology, and their own stderr stays asserted).
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/repos/git.sh\"\n",
    "if [[ -n \"${OVERLAY_RECORDS+x}\" && -n \"$OVERLAY_RECORDS\" ]]; then\n",
    "  mapfile -t OVERLAYS <<<\"$OVERLAY_RECORDS\"\n",
    "else\n",
    "  OVERLAYS=()\n",
    "fi\n",
);

/// Run one shell snippet with the repos libraries sourced.
fn shell_run(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Run `git -C dir args`, silenced, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

/// `git init` plus one tracked committed file.
fn seed_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("tracked.txt"), b"v1\n").expect("seed file");
    git(
        dir,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "seed",
        ],
    );
}

/// One `each_existing` record row, byte-identical to the shell `cb`
/// printf below (`kind=.. name=.. path=.. url=..[ arg=..]*`).
fn record_row(kind: &str, name: &str, path: &str, url: &str, args: &[OsString]) -> Vec<u8> {
    let mut row = format!("kind={kind} name={name} path={path} url={url}");
    for arg in args {
        row.push_str(&format!(" arg={}", arg.to_string_lossy()));
    }
    row.push('\n');
    row.into_bytes()
}

/// Shell callback printing the same row shape as [`record_row`].
const EACH_CALLBACK: &str = concat!(
    "cb() {\n",
    "  local kind=$1 name=$2 path=$3 url=$4; shift 4\n",
    "  printf 'kind=%s name=%s path=%s url=%s' \"$kind\" \"$name\" \"$path\" \"$url\"\n",
    "  local a; for a in \"$@\"; do printf ' arg=%s' \"$a\"; done\n",
    "  printf '\\n'\n",
    "}\n",
);

#[test]
fn each_existing_order_and_skips() {
    let dir = TempDir::new("repos-git-each").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    seed_repo(home);
    let ov1 = home.join("ov1");
    seed_repo(&ov1);
    let ov2 = home.join("ov2");
    seed_repo(&ov2);
    let ov3 = home.join("ov3");
    seed_repo(&ov3);
    std::fs::create_dir_all(home.join("empty")).expect("empty dir");
    std::fs::write(home.join("notadir"), b"not a dir\n").expect("plain file");
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let records = vec![
        format!(
            "web|{}|file:///repo/web.git|{home_text}/conf/10-web.conf|false|git",
            text(&ov1)
        ),
        format!("bare|{}", text(&ov2)),
        format!(
            "skipped|{}|file:///x|{home_text}/conf|false|none",
            text(&ov3)
        ),
        format!("remainder|{}|u|d|o|git|x", text(&ov1)),
        format!("missing|{home_text}/gone|file:///x|c|false|git"),
        format!("plainfile|{home_text}/notadir|file:///x|c|false|git"),
        format!("emptydir|{home_text}/empty|file:///x|c|false|git"),
        String::new(),
    ];
    let joined = records.join("\n");
    let args = [OsString::from("extra1"), OsString::from("extra2")];
    for (topology, shell_topology, want_base) in [
        (Topology::Ordinary, "ordinary", true),
        (Topology::Missing, "missing", false),
    ] {
        let (code, out, serr) = shell_run(
            home,
            &[],
            &[("OVERLAY_RECORDS", Some(&joined))],
            &format!(
                "{EACH_CALLBACK}export DOT_BASE_TOPOLOGY={shell_topology} DOT_CLIENT_GIT_DIR=\n\
                 _repo_each_existing cb extra1 extra2\n\
                 printf 'rc=%s\\n' \"$?\"\n"
            ),
        );
        assert_eq!(code, 0, "shell harness each {shell_topology}");
        assert_eq!(serr, b"", "each stderr {shell_topology}");
        let mut want: Vec<u8> = Vec::new();
        if want_base {
            want.extend(record_row("base", "dotfiles", &home_text, "", &args));
        }
        want.extend(record_row(
            "overlay",
            "web",
            &text(&ov1),
            "file:///repo/web.git",
            &args,
        ));
        want.extend(record_row("overlay", "bare", &text(&ov2), "", &args));
        want.extend(b"rc=0\n");
        assert_eq!(out, want, "each rows {shell_topology}");
        let base = Base {
            topology,
            client_git_dir: String::new(),
            home: home_text.clone(),
        };
        let mut got: Vec<u8> = Vec::new();
        let mut callback = |kind, name: &str, path: &str, url: &str, seen: &[OsString]| {
            got.extend(record_row(
                match kind {
                    dot::repos_base::RepoKind::Base => "base",
                    dot::repos_base::RepoKind::Overlay => "overlay",
                },
                name,
                path,
                url,
                seen,
            ));
            0
        };
        let rc = repos_git::each_existing(&base, &records, &home_text, &args, &mut callback);
        got.extend(format!("rc={rc}\n").bytes());
        assert_eq!(got, want, "rust rows {shell_topology:?}");
    }
}

/// Shell `_repo_git` exit code for one call: git's own stdout streams
/// into the harness output, so the exit rides on the last line.
const GIT_SNIPPET: &str = concat!(
    // Drop only $1 (the repo path); the remaining argv is the git call.
    "shift 1\n",
    // Repaired (3 harness bugs vs the shell contract `_repo_git kind path
    // args...`): sourcing model.sh runs _dot_client_select, which assigns
    // DOT_BASE_TOPOLOGY (a plain fixture checkout selects `missing`) and
    // clobbers any env-passed value, so the topology arrives via $TOPO and
    // exports here, after sourcing (the repos_base harness pattern); and
    // the base call was missing its (ignored) path slot, so `rev-parse`
    // landed in $path and `--show-toplevel` reached bare `git` (129).
    "export DOT_BASE_TOPOLOGY=\"$TOPO\" DOT_CLIENT_GIT_DIR=\"$HOME/.dotfiles\"\n",
    "if [[ \"$KIND\" == base ]]; then\n",
    "  _repo_git base \"$HOME\" \"$@\" 2>/dev/null; rc=$?\n",
    "else\n",
    "  _repo_git overlay \"$OPATH\" \"$@\" 2>/dev/null; rc=$?\n",
    "fi\n",
    "printf 'rc=%d\\n' \"$rc\"\n",
);

#[test]
fn repo_git_dispatches_base_and_overlay_with_shell_exit_codes() {
    let dir = TempDir::new("repos-git-dispatch").expect("fixture dir");
    let home = dir.path();
    seed_repo(home);
    let ov = home.join("ov");
    seed_repo(&ov);
    let ov_text = ov.to_string_lossy().into_owned();
    let home_text = home.to_string_lossy().into_owned();
    let show = ["rev-parse", "--show-toplevel"];
    // Base + overlay success, plus a failing git invocation: the
    // exit codes must match the shell exactly (0, 0, 128).
    for (kind, opath, args, topology) in [
        ("base", "", show.as_slice(), "ordinary"),
        ("overlay", ov_text.as_str(), show.as_slice(), "ordinary"),
        ("base", "", &["rev-parse", "--no-such-flag"][..], "ordinary"),
        ("base", "", show.as_slice(), "missing"),
    ] {
        let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let (shell_status, shell_out, shell_err) = shell_run(
            home,
            &argv,
            &[
                ("KIND", Some(kind)),
                ("OPATH", Some(opath)),
                ("TOPO", Some(topology)),
            ],
            GIT_SNIPPET,
        );
        assert_eq!(shell_status, 0, "harness exit");
        assert!(shell_err.is_empty(), "shell stderr: {shell_err:?}");
        // Git's own stdout (e.g. the toplevel path) streams ahead of
        // the harness line, so the exit rides on the last line.
        let shell_rc: i32 = String::from_utf8_lossy(&shell_out)
            .lines()
            .last()
            .unwrap_or("")
            .strip_prefix("rc=")
            .and_then(|text| text.parse().ok())
            .unwrap_or(-1);
        let base = Base {
            topology: if topology == "missing" {
                Topology::Missing
            } else {
                Topology::Ordinary
            },
            client_git_dir: format!("{home_text}/.dotfiles"),
            home: home_text.clone(),
        };
        let rust_rc = if kind == "base" {
            repos_git::repo_git(&base, dot::repos_base::RepoKind::Base, &home_text, args)
        } else {
            repos_git::repo_git(&base, dot::repos_base::RepoKind::Overlay, &ov_text, args)
        };
        assert_eq!(shell_rc, rust_rc, "exit parity for {kind} {args:?}");
        if topology == "missing" {
            assert_eq!(rust_rc, 128, "missing topology refuses like the shell");
        }
    }
}

#[test]
fn each_existing_short_circuits() {
    let dir = TempDir::new("repos-git-short").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    seed_repo(home);
    let ov1 = home.join("ov1");
    seed_repo(&ov1);
    let ov2 = home.join("ov2");
    seed_repo(&ov2);
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let records = vec![
        format!("web|{}|file:///repo/web.git|c|false|git", text(&ov1)),
        format!("late|{}|file:///repo/late.git|c|false|git", text(&ov2)),
    ];
    let joined = records.join("\n");
    // Fail on the `fail_at`-th invocation: 1 stops at the base record,
    // 2 stops after the first overlay; later records never run.
    for fail_at in [1, 2] {
        let (code, out, serr) = shell_run(
            home,
            &[],
            &[("OVERLAY_RECORDS", Some(&joined))],
            &format!(
                "export DOT_BASE_TOPOLOGY=ordinary DOT_CLIENT_GIT_DIR=\n\
                 n=0\n\
                 cb() {{\n\
                   n=$((n + 1))\n\
                   printf 'call=%s kind=%s name=%s\\n' \"$n\" \"$1\" \"$2\"\n\
                   [[ $n -eq {fail_at} ]] && return 7\n\
                   return 0\n\
                 }}\n\
                 _repo_each_existing cb\n\
                 printf 'rc=%s\\n' \"$?\"\n"
            ),
        );
        assert_eq!(code, 0, "shell harness short {fail_at}");
        assert_eq!(serr, b"", "short stderr {fail_at}");
        let mut want: Vec<u8> = Vec::new();
        want.extend("call=1 kind=base name=dotfiles\n".bytes());
        if fail_at > 1 {
            want.extend("call=2 kind=overlay name=web\n".bytes());
        }
        want.extend(b"rc=7\n");
        assert_eq!(out, want, "shell short rows {fail_at}");
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home_text.clone(),
        };
        let mut got: Vec<u8> = Vec::new();
        let mut n = 0;
        let mut callback = |kind, name: &str, _: &str, _: &str, _: &[OsString]| {
            n += 1;
            got.extend(
                format!(
                    "call={n} kind={} name={name}\n",
                    match kind {
                        dot::repos_base::RepoKind::Base => "base",
                        dot::repos_base::RepoKind::Overlay => "overlay",
                    }
                )
                .bytes(),
            );
            if n == fail_at { 7 } else { 0 }
        };
        let rc = repos_git::each_existing(&base, &records, &home_text, &[], &mut callback);
        got.extend(format!("rc={rc}\n").bytes());
        assert_eq!(got, want, "rust short rows {fail_at}");
    }
}

/// Run `_repo_git kind path args` under the shell, capturing the git
/// exit code plus its exact stdout bytes (stderr to a file).
fn shell_repo_git(
    home: &Path,
    topology: &str,
    git_dir: &Path,
    kind: &str,
    path: &str,
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
         _repo_git {kind} {} {argv} >\"$_t/out\" 2>\"$_t/err\"\n\
         printf 'rc=%s\\n' \"$?\"\n\
         cat \"$_t/out\"\n\
         printf '\\n__ERR__\\n'\n\
         cat \"$_t/err\"\n\
         rm -rf \"$_t\"\n",
        sh_quote(&git_dir.to_string_lossy()),
        sh_quote(&home.to_string_lossy()),
        sh_quote(path),
    );
    let (wrap_rc, out, err) = shell_run(home, &[], &[], &snippet);
    assert_eq!(wrap_rc, 0, "wrapper failed: {snippet}");
    assert!(err.is_empty(), "harness stderr must be silent: {err:?}");
    let marker = b"\n__ERR__\n";
    let pos = find_marker(&out, marker).expect("marker line in wrapper stdout");
    let (head, tail) = out.split_at(pos);
    let git_err = tail[marker.len()..].to_vec();
    let rc_line = head.split(|byte| *byte == b'\n').next().unwrap_or(b"");
    let rc = std::str::from_utf8(rc_line.strip_prefix(b"rc=").unwrap_or(b""))
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(-1);
    assert!(rc >= 0, "unparseable rc line: {head:?}");
    let nl = head
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("rc line");
    (rc, head[nl + 1..].to_vec(), git_err)
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

/// Ordinary-topology fixture: `$HOME` itself is a checkout with one commit.
fn fixture_ordinary(label: &str) -> TempDir {
    let home = TempDir::new(label).expect("temp home");
    seed_repo(home.path());
    home
}

/// Separate-topology fixture: bare git dir plus a `$HOME` worktree with
/// one commit, made through the `--git-dir/--work-tree` prefix form.
fn fixture_separate(home_label: &str, git_label: &str) -> (TempDir, TempDir) {
    let home = TempDir::new(home_label).expect("temp home");
    let git_dir = TempDir::new(git_label).expect("temp git dir");
    let git_dir_text = git_dir.path().to_string_lossy().into_owned();
    let work_text = home.path().to_string_lossy().into_owned();
    git(git_dir.path(), &["init", "--bare", "-q"]);
    std::fs::write(home.path().join("file.txt"), b"hello\n").expect("write fixture");
    prefixed_git(&git_dir_text, &work_text, &["add", "file.txt"]);
    prefixed_git(&git_dir_text, &work_text, &["commit", "-qm", "init"]);
    (home, git_dir)
}

/// Run `git --git-dir=.. --work-tree=.. -c identity... args`, silenced.
fn prefixed_git(git: &str, work: &str, args: &[&str]) {
    let status = Command::new("git")
        .arg(format!("--git-dir={git}"))
        .arg(format!("--work-tree={work}"))
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn prefixed git");
    assert!(status.success(), "prefixed git {args:?}");
}

#[test]
fn repo_git_prefix_shapes() {
    // `rev-parse --show-toplevel` succeeds only through the right prefix
    // and prints the work tree: the shell bytes pin the shape, the Rust
    // exit code must match (streaming output goes to the terminal on
    // both sides, so bytes are asserted shell-side only).
    let home = fixture_ordinary("repos-git-shape-ord");
    let home_text = home.path().to_string_lossy().into_owned();
    let (shell_rc, shell_out, shell_err) = shell_repo_git(
        home.path(),
        "ordinary",
        home.path(),
        "overlay",
        &home_text,
        &["rev-parse", "--show-toplevel"],
    );
    assert_eq!(shell_rc, 0);
    assert_eq!(shell_out, format!("{home_text}\n").into_bytes());
    assert_eq!(shell_err, b"");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    assert_eq!(
        repos_git::repo_git(
            &base,
            dot::repos_base::RepoKind::Overlay,
            &home_text,
            &["rev-parse", "--show-toplevel"]
        ),
        shell_rc,
        "overlay toplevel rc"
    );
    assert_eq!(
        repos_git::repo_git(
            &base,
            dot::repos_base::RepoKind::Base,
            &home_text,
            &["rev-parse", "--show-toplevel"]
        ),
        0,
        "ordinary base toplevel rc"
    );
    let (sep_home, sep_git) =
        fixture_separate("repos-git-shape-sep-home", "repos-git-shape-sep-git");
    let sep_home_text = sep_home.path().to_string_lossy().into_owned();
    let sep_git_text = sep_git.path().to_string_lossy().into_owned();
    let (shell_rc, shell_out, shell_err) = shell_repo_git(
        sep_home.path(),
        "separate",
        sep_git.path(),
        "base",
        &sep_home_text,
        &["rev-parse", "--show-toplevel"],
    );
    assert_eq!(shell_rc, 0);
    assert_eq!(shell_out, format!("{sep_home_text}\n").into_bytes());
    assert_eq!(shell_err, b"");
    let separate = Base {
        topology: Topology::Separate,
        client_git_dir: sep_git_text,
        home: sep_home_text.clone(),
    };
    assert_eq!(
        repos_git::repo_git(
            &separate,
            dot::repos_base::RepoKind::Base,
            &sep_home_text,
            &["rev-parse", "--show-toplevel"]
        ),
        shell_rc,
        "separate base toplevel rc"
    );
}

#[test]
fn repo_git_missing_topology_refuses() {
    let dir = TempDir::new("repos-git-missing").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let (shell_rc, shell_out, shell_err) = shell_repo_git(
        home,
        "missing",
        home,
        "base",
        &home_text,
        &["rev-parse", "--show-toplevel"],
    );
    assert_eq!(shell_rc, 128, "shell _repo_git refuses missing with 128");
    assert!(
        shell_out.is_empty(),
        "missing prints nothing: {shell_out:?}"
    );
    assert!(shell_err.is_empty(), "missing is silent: {shell_err:?}");
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    assert_eq!(
        repos_git::repo_git(
            &base,
            dot::repos_base::RepoKind::Base,
            &home_text,
            &["rev-parse", "--show-toplevel"]
        ),
        128,
        "rust refuses missing with 128"
    );
}

#[test]
fn repo_git_quiet_failure_propagates() {
    // `diff --quiet` on a dirty tree exits 1 with no output on either
    // stream: exit-code parity without terminal noise.
    let home = fixture_ordinary("repos-git-quiet");
    let home_text = home.path().to_string_lossy().into_owned();
    std::fs::write(home.path().join("tracked.txt"), b"local edit\n").expect("dirty tree");
    for (kind, shell_kind) in [
        (dot::repos_base::RepoKind::Base, "base"),
        (dot::repos_base::RepoKind::Overlay, "overlay"),
    ] {
        let (shell_rc, shell_out, shell_err) = shell_repo_git(
            home.path(),
            "ordinary",
            home.path(),
            shell_kind,
            &home_text,
            &["diff", "--quiet"],
        );
        assert_eq!(shell_rc, 1, "dirty diff exits 1 ({shell_kind})");
        assert!(shell_out.is_empty(), "quiet diff prints nothing");
        assert!(shell_err.is_empty(), "quiet diff is silent");
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home_text.clone(),
        };
        assert_eq!(
            repos_git::repo_git(&base, kind, &home_text, &["diff", "--quiet"]),
            shell_rc,
            "quiet diff rc ({shell_kind})"
        );
    }
}

/// Commit all current changes with the fixture identity.
fn commit_all(dir: &Path, message: &str) {
    git(
        dir,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ],
    );
}

/// Bare origin with one commit, then a clone: `fetch origin` succeeds
/// and `.git/FETCH_HEAD` exists. Returns `(origin, work)`.
fn clone_with_upstream(scope: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let origin = scope.join(format!("{name}.git"));
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(&origin, &["init", "--bare", "-q"]);
    let seed = scope.join(format!("{name}-seed"));
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&seed)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone seed");
    assert!(status.success(), "clone seed {}", seed.display());
    std::fs::write(seed.join("tracked.txt"), b"v1\n").expect("seed file");
    commit_all(&seed, "seed");
    git(&seed, &["push", "-q", "origin", "HEAD"]);
    let work = scope.join(format!("{name}-work"));
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&work)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone work");
    assert!(status.success(), "clone work {}", work.display());
    // `clone` leaves no FETCH_HEAD behind (verified against the
    // pinned git): one explicit fetch creates it, like production.
    git(&work, &["fetch", "origin"]);
    assert!(
        work.join(".git/FETCH_HEAD").is_file(),
        "fetch writes FETCH_HEAD"
    );
    (origin, work)
}

/// `stat` permission bits of a fixture path.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

/// `chmod` a fixture path to an exact mode.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Current euid / umask for ownership-gated checks.
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

/// Run `_repo_git_fetch kind path extra...` under the shell: the git
/// exit code, git stdout bytes, git stderr bytes, and the `stat` mode
/// line of `fetch_head` (`nomode` when it cannot be stated).
fn shell_repo_git_fetch(
    home: &Path,
    topology: &str,
    git_dir: &Path,
    kind: &str,
    path: &str,
    fetch_head: &str,
    extra: &[&str],
) -> (i32, Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut argv = String::new();
    for arg in extra {
        argv.push(' ');
        argv.push_str(&sh_quote(arg));
    }
    let snippet = format!(
        "export DOT_BASE_TOPOLOGY={topology} DOT_CLIENT_GIT_DIR={} HOME={}\n\
         _t=$(mktemp -d)\n\
         _repo_git_fetch {kind} {} {argv} >\"$_t/out\" 2>\"$_t/err\"\n\
         printf 'rc=%s\\n' \"$?\"\n\
         cat \"$_t/out\"\n\
         printf '\\n__ERR__\\n'\n\
         cat \"$_t/err\"\n\
         printf '\\n__MODE__\\n'\n\
         stat -c '%a' {} 2>/dev/null || stat -f '%Lp' {} 2>/dev/null || printf 'nomode\\n'\n\
         rm -rf \"$_t\"\n",
        sh_quote(&git_dir.to_string_lossy()),
        sh_quote(&home.to_string_lossy()),
        sh_quote(path),
        sh_quote(fetch_head),
        sh_quote(fetch_head),
    );
    let (wrap_rc, out, err) = shell_run(home, &[], &[], &snippet);
    assert_eq!(wrap_rc, 0, "wrapper failed: {snippet}");
    assert!(err.is_empty(), "harness stderr must be silent: {err:?}");
    let mode_marker = b"\n__MODE__\n";
    let mpos = find_marker(&out, mode_marker).expect("mode marker in wrapper stdout");
    let (head, tail) = out.split_at(mpos);
    let mode = tail[mode_marker.len()..].to_vec();
    let err_marker = b"\n__ERR__\n";
    let epos = find_marker(head, err_marker).expect("err marker in wrapper stdout");
    let (head, tail) = head.split_at(epos);
    let git_err = tail[err_marker.len()..].to_vec();
    let rc_line = head.split(|byte| *byte == b'\n').next().unwrap_or(b"");
    let rc = std::str::from_utf8(rc_line.strip_prefix(b"rc=").unwrap_or(b""))
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(-1);
    assert!(rc >= 0, "unparseable rc line: {head:?}");
    let nl = head
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("rc line");
    (rc, head[nl + 1..].to_vec(), git_err, mode)
}

#[test]
fn repo_git_fetch_success_clamps_fetch_head() {
    let dir = TempDir::new("repos-git-fetch-ok").expect("fixture dir");
    let scope = dir.path();
    let (_origin, work) = clone_with_upstream(scope, "ok");
    let work_text = work.to_string_lossy().into_owned();
    let fetch_head = work.join(".git/FETCH_HEAD");
    assert!(fetch_head.is_file(), "clone writes FETCH_HEAD");
    let mask = dot::temp::read_umask().expect("read umask");
    let dummy = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: work_text.clone(),
    };
    // Loosen first so the clamp is observable, on both sides in turn.
    chmod(&fetch_head, 0o644);
    let (shell_rc, shell_out, shell_err, shell_mode) = shell_repo_git_fetch(
        scope,
        "ordinary",
        scope,
        "overlay",
        &work_text,
        &fetch_head.to_string_lossy(),
        &["origin"],
    );
    assert_eq!(shell_rc, 0, "shell fetch succeeds");
    assert!(shell_out.is_empty(), "quiet fetch prints nothing");
    assert_eq!(shell_err, b"", "quiet fetch is silent");
    assert_eq!(shell_mode, b"600\n", "shell clamps FETCH_HEAD to 0600");
    chmod(&fetch_head, 0o644);
    assert_eq!(mode_of(&fetch_head), 0o644, "loosen FETCH_HEAD first");
    let rc = repos_git::repo_git_fetch(
        &dummy,
        dot::repos_base::RepoKind::Overlay,
        &work_text,
        &["origin"],
        mask,
    );
    assert_eq!(rc, shell_rc, "fetch rc parity");
    assert_eq!(
        mode_of(&fetch_head),
        0o600,
        "rust clamps FETCH_HEAD to 0600"
    );
}

#[test]
fn repo_git_fetch_rc_preserved_on_failure() {
    let dir = TempDir::new("repos-git-fetch-rc").expect("fixture dir");
    let scope = dir.path();
    let dummy = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: scope.to_string_lossy().into_owned(),
    };
    let mask = dot::temp::read_umask().expect("read umask");
    // Unknown flag: fetch itself fails, so the return must be fetch's
    // own code — never the gate's 1 — on both sides.
    let (_origin, work) = clone_with_upstream(scope, "badflag");
    let work_text = work.to_string_lossy().into_owned();
    let fetch_head = work.join(".git/FETCH_HEAD");
    let (shell_rc, _, _, shell_mode) = shell_repo_git_fetch(
        scope,
        "ordinary",
        scope,
        "overlay",
        &work_text,
        &fetch_head.to_string_lossy(),
        &["--no-such-flag"],
    );
    assert_ne!(shell_rc, 0, "bad flag must fail");
    assert_ne!(shell_rc, 1, "bad flag rc is not the gate code");
    let rc = repos_git::repo_git_fetch(
        &dummy,
        dot::repos_base::RepoKind::Overlay,
        &work_text,
        &["--no-such-flag"],
        mask,
    );
    assert_eq!(rc, shell_rc, "failing fetch rc parity");
    assert_eq!(shell_mode, b"600\n", "gate still clamps on failure");
    assert_eq!(mode_of(&fetch_head), 0o600, "rust still clamps on failure");
    // No FETCH_HEAD at all: a remote-less repo's failing fetch returns
    // fetch's own code with the gate skipped.
    let plain = scope.join("plain");
    seed_repo(&plain);
    let plain_text = plain.to_string_lossy().into_owned();
    assert!(!plain.join(".git/FETCH_HEAD").exists());
    let (shell_rc, _, _, _) = shell_repo_git_fetch(
        scope,
        "ordinary",
        scope,
        "overlay",
        &plain_text,
        &plain.join(".git/FETCH_HEAD").to_string_lossy(),
        &["origin"],
    );
    assert_eq!(shell_rc, 128, "fetch with no remote exits 128");
    let rc = repos_git::repo_git_fetch(
        &dummy,
        dot::repos_base::RepoKind::Overlay,
        &plain_text,
        &["origin"],
        mask,
    );
    assert_eq!(rc, shell_rc, "missing FETCH_HEAD rc parity");
}

#[test]
fn repo_git_fetch_gate_rejections() {
    let dir = TempDir::new("repos-git-fetch-gate").expect("fixture dir");
    let scope = dir.path();
    let dummy = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: scope.to_string_lossy().into_owned(),
    };
    let mask = dot::temp::read_umask().expect("read umask");
    // Symlink and dangling-symlink FETCH_HEAD: `! -L` fails shell-side,
    // the symlink gate fails Rust-side; both return 1.
    for label in ["link", "dangling"] {
        let (_origin, work) = clone_with_upstream(scope, label);
        let fetch_head = work.join(".git/FETCH_HEAD");
        std::fs::remove_file(&fetch_head).expect("remove FETCH_HEAD");
        // A live target exercises the `! -L` conjunct specifically
        // (`-f` alone would pass); a dead target is dangling.
        let target = if label == "link" {
            work.join("tracked.txt")
        } else {
            scope.join("no-such-target")
        };
        std::os::unix::fs::symlink(target, &fetch_head).expect("symlink FETCH_HEAD");
        let work_text = work.to_string_lossy().into_owned();
        let (shell_rc, _, _, _) = shell_repo_git_fetch(
            scope,
            "ordinary",
            scope,
            "overlay",
            &work_text,
            &fetch_head.to_string_lossy(),
            &["origin"],
        );
        assert_eq!(shell_rc, 1, "shell rejects {label} FETCH_HEAD");
        let rc = repos_git::repo_git_fetch(
            &dummy,
            dot::repos_base::RepoKind::Overlay,
            &work_text,
            &["origin"],
            mask,
        );
        assert_eq!(rc, 1, "rust rejects {label} FETCH_HEAD");
    }
    // Non-owned FETCH_HEAD: `-O` fails shell-side, the uid gate
    // fails Rust-side. Needs privilege; skip without it.
    {
        let (_origin, work) = clone_with_upstream(scope, "owned");
        let fetch_head = work.join(".git/FETCH_HEAD");
        let mine = euid();
        let foreign = if mine == 60000 { 60001 } else { 60000 };
        let chowned = Command::new("chown")
            .arg(foreign.to_string())
            .arg(&fetch_head)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !chowned {
            eprintln!("SKIP non-owned FETCH_HEAD: cannot chown (uid {mine})");
        } else {
            let work_text = work.to_string_lossy().into_owned();
            let (shell_rc, _, _, _) = shell_repo_git_fetch(
                scope,
                "ordinary",
                scope,
                "overlay",
                &work_text,
                &fetch_head.to_string_lossy(),
                &["origin"],
            );
            assert_eq!(shell_rc, 1, "shell rejects non-owned FETCH_HEAD");
            let rc = repos_git::repo_git_fetch(
                &dummy,
                dot::repos_base::RepoKind::Overlay,
                &work_text,
                &["origin"],
                mask,
            );
            assert_eq!(rc, 1, "rust rejects non-owned FETCH_HEAD");
        }
    }
    // Unresolvable git dir: rev-parse fails, so both return 1 even
    // though fetch itself also failed (rc 128 is discarded).
    let gone = scope.join("gone");
    let gone_text = gone.to_string_lossy().into_owned();
    let (shell_rc, _, _, shell_mode) = shell_repo_git_fetch(
        scope,
        "ordinary",
        scope,
        "overlay",
        &gone_text,
        &gone.join(".git/FETCH_HEAD").to_string_lossy(),
        &["origin"],
    );
    assert_eq!(shell_rc, 1, "shell returns 1 for missing git dir");
    assert_eq!(shell_mode, b"nomode\n");
    let rc = repos_git::repo_git_fetch(
        &dummy,
        dot::repos_base::RepoKind::Overlay,
        &gone_text,
        &["origin"],
        mask,
    );
    assert_eq!(rc, 1, "rust returns 1 for missing git dir");
    // Missing base topology: fetch refuses (128, recorded) and the
    // rev-parse refuses too, so both return 1.
    let missing = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: scope.to_string_lossy().into_owned(),
    };
    let (shell_rc, _, _, _) = shell_repo_git_fetch(
        scope,
        "missing",
        scope,
        "base",
        &scope.to_string_lossy(),
        &scope.join(".git/FETCH_HEAD").to_string_lossy(),
        &["origin"],
    );
    assert_eq!(shell_rc, 1, "shell returns 1 for missing topology");
    let rc = repos_git::repo_git_fetch(
        &missing,
        dot::repos_base::RepoKind::Base,
        &scope.to_string_lossy(),
        &["origin"],
        mask,
    );
    assert_eq!(rc, 1, "rust returns 1 for missing topology");
}
