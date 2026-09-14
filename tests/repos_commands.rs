//! Differential parity tests for `src/repos_commands.rs` against the
//! live shell (`lib/dot/repos/commands.sh`): the header table plus the
//! fetch/push/diff/status one/all wrappers over real fixtures.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_base::{Base, RepoKind, Topology};
use dot::repos_commands::{
    diff_all, diff_one, fetch_all, fetch_one, header_text, push_all, push_one, status_all,
    status_one,
};
use dot::test_support::TempDir;

/// Sources plus the init stub `model.sh` needs at source time (copied
/// from the `repos_dirty.rs` preamble convention): `model.sh` runs
/// `_dot_client_select` on load, whose loader diagnostic is suppressed;
/// each snippet then exports an explicit topology. `log.sh` adds
/// `_header`/`_warn`, `temp.sh` the FETCH_HEAD ceiling helper,
/// `git.sh` the iteration/invocation layer, and `commands.sh` the
/// functions under test.
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/git.sh\"\n",
    ". \"$1/lib/dot/repos/dirty.sh\"\n",
    ". \"$1/lib/dot/repos/commands.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
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
    // Redirect any machine-local launcher-shim cache out of the
    // fixture home: the shim resolves through XDG cache, whose
    // default (`$HOME/.cache`) would otherwise appear as an untracked
    // `?? .cache/` row in status assertions. Outside such machines
    // this only moves an unused directory.
    let shim_cache = Path::new(&tmpdir).join("dot-git-shim-cache");
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        // Hermetic git: some machines wrap `git` in a launcher shim
        // that rewrites HOME-rooted calls. `DOT_GIT_REAL=1` bypasses
        // that shim to the real git; where no shim exists the
        // variable is inert.
        .env("DOT_GIT_REAL", "1")
        .env("XDG_CACHE_HOME", &shim_cache)
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

/// Run `git -C dir args`, silenced, asserting success. `DOT_GIT_REAL`
/// keeps fixture git off any machine-local launcher shim (see
/// `shell_run`).
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

/// Run `git` with an explicit `--git-dir/--work-tree` prefix (separate
/// topology fixtures), silenced, asserting success.
fn git_prefix(prefix: &[OsString], args: &[&str]) {
    let status = Command::new("git")
        .args(prefix)
        .args(args)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn prefix git");
    assert!(status.success(), "prefix git {args:?}");
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

/// Bare origin with one commit, then a clone: the clone's HEAD tracks
/// the origin, so plain `fetch`/`push` are silent no-ops when
/// up-to-date. Returns the clone.
fn clone_with_upstream(scope: &Path, name: &str) -> PathBuf {
    let origin = scope.join(format!("{name}.git"));
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(&origin, &["init", "--bare", "-q"]);
    let seed = scope.join(format!("{name}-seed"));
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&seed)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone seed");
    assert!(status.success(), "clone seed {}", seed.display());
    std::fs::write(seed.join("tracked.txt"), b"v1\n").expect("seed file");
    git(
        seed.as_path(),
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        seed.as_path(),
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
    git(seed.as_path(), &["push", "-q", "origin", "HEAD"]);
    let work = scope.join(format!("{name}-work"));
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&work)
        .env("DOT_GIT_REAL", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone work");
    assert!(status.success(), "clone work {}", work.display());
    work
}

/// Shell export prelude selecting a base topology.
fn topology_env(topology: &str, git_dir: &str) -> String {
    format!("export DOT_BASE_TOPOLOGY={topology} DOT_CLIENT_GIT_DIR={git_dir}\n")
}

/// OVERLAY_RECORDS env from records (None unsets, matching an empty
/// Rust vec on both sides of the mapfile split).
fn records_env(records: &[String]) -> (String, Option<String>) {
    if records.is_empty() {
        ("OVERLAY_RECORDS".to_string(), None)
    } else {
        ("OVERLAY_RECORDS".to_string(), Some(records.join("\n")))
    }
}

/// Run one `_repo_<op>_one` shell function and split the harness
/// stdout into `(inner rc, fn stdout bytes, fn stderr bytes)`.
///
/// Layout: `rc=<n>\n<fn stdout>\n__ERR__\n<fn stderr>`; the marker is
/// safe because these outputs never contain that line.
#[allow(clippy::too_many_arguments)]
fn shell_one(
    home: &Path,
    topology: &str,
    git_dir: &str,
    records: &[String],
    op: &str,
    kind: &str,
    name: &str,
    path: &str,
    extra: &[&str],
    quiet: bool,
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut argv = String::new();
    for arg in extra {
        argv.push(' ');
        argv.push_str(&sh_quote(arg));
    }
    let (key, value) = records_env(records);
    let extra_env: &[(&str, Option<&str>)] = match &value {
        Some(text) => &[(key.as_str(), Some(text.as_str()))],
        None => &[(key.as_str(), None)],
    };
    let quiet_export = if quiet {
        "export DOT_QUIET=1\n".to_string()
    } else {
        String::new()
    };
    let snippet = format!(
        "{}{}_t=$(mktemp -d)\n_repo_{op}_one {kind} {} {} {}{} >\"$_t/out\" 2>\"$_t/err\"\nprintf 'rc=%s\\n' \"$?\"\ncat \"$_t/out\"\nprintf '\\n__ERR__\\n'\ncat \"$_t/err\"\nrm -rf \"$_t\"\n",
        topology_env(topology, git_dir),
        quiet_export,
        sh_quote(name),
        sh_quote(path),
        sh_quote(""),
        argv,
    );
    let (wrap_rc, out, err) = shell_run(home, &[], extra_env, &snippet);
    assert_eq!(wrap_rc, 0, "wrapper failed: {snippet}");
    assert!(err.is_empty(), "harness stderr must be silent: {err:?}");
    let marker = b"\n__ERR__\n";
    let pos = find_marker(&out, marker).expect("marker line in wrapper stdout");
    let (head, tail) = out.split_at(pos);
    let fn_err = tail[marker.len()..].to_vec();
    let rc = dump_rc(head);
    assert!(rc >= 0, "unparseable rc line: {head:?}");
    let nl = head
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("rc line");
    let fn_out = head[nl + 1..].to_vec();
    (rc, fn_out, fn_err)
}

#[test]
fn fetch_one_base_parity() {
    // Ordinary base whose HEAD tracks an origin: plain `fetch` is an
    // up-to-date silent no-op, so stdout carries exactly the header on
    // both sides with empty stderr.
    let dir = TempDir::new("commands-fetch-base").expect("fixture dir");
    let home = clone_with_upstream(dir.path(), "base");
    let home_text = home.to_string_lossy().into_owned();
    let (shell_rc, shell_out, shell_err) = shell_one(
        &home,
        "ordinary",
        "",
        &[],
        "fetch",
        "base",
        "dotfiles",
        &home_text,
        &[],
        false,
    );
    assert_eq!(shell_rc, 0, "shell fetch must succeed");
    let log = Log::new(false, false);
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    let mut out = Vec::new();
    // A realistic umask (not the 0600 ceiling itself): the clamp must
    // land FETCH_HEAD at exactly 0600 on both sides.
    let rc = fetch_one(
        &log,
        &mut out,
        &base,
        RepoKind::Base,
        "dotfiles",
        &home_text,
        &[],
        0o022,
    );
    assert_eq!(rc, shell_rc, "fetch exit parity");
    assert_eq!(out, shell_out, "fetch stdout bytes parity");
    assert_eq!(shell_out, b"==> Fetching dotfiles...\n");
    assert_eq!(shell_err, b"", "fetch stderr parity");
    assert_eq!(
        mode_of(&home.join(".git/FETCH_HEAD")),
        0o600,
        "fetch clamps FETCH_HEAD to 0600"
    );
}

/// Run one `_repo_<op>_all` shell function and split the harness
/// stdout into `(inner rc, fn stdout bytes, fn stderr bytes)`.
fn shell_all(
    home: &Path,
    topology: &str,
    git_dir: &str,
    records: &[String],
    op: &str,
    extra: &[&str],
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut argv = String::new();
    for arg in extra {
        argv.push(' ');
        argv.push_str(&sh_quote(arg));
    }
    let (key, value) = records_env(records);
    let extra_env: &[(&str, Option<&str>)] = match &value {
        Some(text) => &[(key.as_str(), Some(text.as_str()))],
        None => &[(key.as_str(), None)],
    };
    let snippet = format!(
        "{}_t=$(mktemp -d)\n_repo_{op}_all{} >\"$_t/out\" 2>\"$_t/err\"\nprintf 'rc=%s\\n' \"$?\"\ncat \"$_t/out\"\nprintf '\\n__ERR__\\n'\ncat \"$_t/err\"\nrm -rf \"$_t\"\n",
        topology_env(topology, git_dir),
        argv,
    );
    let (wrap_rc, out, err) = shell_run(home, &[], extra_env, &snippet);
    assert_eq!(wrap_rc, 0, "wrapper failed: {snippet}");
    assert!(err.is_empty(), "harness stderr must be silent: {err:?}");
    let marker = b"\n__ERR__\n";
    let pos = find_marker(&out, marker).expect("marker line in wrapper stdout");
    let (head, tail) = out.split_at(pos);
    let fn_err = tail[marker.len()..].to_vec();
    let rc = dump_rc(head);
    assert!(rc >= 0, "unparseable rc line: {head:?}");
    let nl = head
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("rc line");
    let fn_out = head[nl + 1..].to_vec();
    (rc, fn_out, fn_err)
}

/// One overlay record in the `name|path|url|...|sync` shape the shell
/// `read` idiom parses.
fn overlay_record(name: &str, path: &str, home: &str, sync: &str) -> String {
    format!("{name}|{path}|file:///repo/{name}.git|{home}/conf/10-{name}.conf|false|{sync}")
}

/// Ordinary-topology `Base` for a fixture home.
fn ordinary_base(home: &str) -> Base {
    Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home.to_string(),
    }
}

/// Extra argv as owned `OsString`s for the `_all` functions.
fn os_extra(extra: &[&str]) -> Vec<OsString> {
    extra.iter().map(|arg| OsString::from(*arg)).collect()
}

/// Permission bits of a fixture path, masked to the low 12.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn header_unknown_op_prints_nothing() {
    // The shell case table falls through for unknown ops: no header.
    for op in ["bogus", "", "FETCH", "fetch "] {
        assert_eq!(
            header_text(op, RepoKind::Base, "dotfiles"),
            None,
            "base header for {op:?}"
        );
        assert_eq!(
            header_text(op, RepoKind::Overlay, "web"),
            None,
            "overlay header for {op:?}"
        );
    }
    // The base rows ignore the name, like the shell ignoring `$3`.
    assert_eq!(
        header_text("fetch", RepoKind::Base, "web"),
        header_text("fetch", RepoKind::Base, "dotfiles")
    );
}

#[test]
fn fetch_one_overlay_parity() {
    // Overlay clone, up-to-date: silent fetch, overlay header.
    let dir = TempDir::new("commands-fetch-overlay").expect("fixture dir");
    let home = TempDir::new("commands-fetch-overlay-home").expect("neutral home");
    let overlay = clone_with_upstream(dir.path(), "web");
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record(
        "web",
        &overlay_text,
        &home.path().to_string_lossy(),
        "git",
    )];
    let (shell_rc, shell_out, shell_err) = shell_one(
        home.path(),
        "missing",
        "",
        &records,
        "fetch",
        "overlay",
        "web",
        &overlay_text,
        &[],
        false,
    );
    assert_eq!(shell_rc, 0, "shell fetch must succeed");
    let log = Log::new(false, false);
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: home.path().to_string_lossy().into_owned(),
    };
    let mut out = Vec::new();
    // Realistic umask so the 0600 clamp outcome is observable (see
    // the base test): the clamped mode is asserted below.
    let rc = fetch_one(
        &log,
        &mut out,
        &base,
        RepoKind::Overlay,
        "web",
        &overlay_text,
        &[],
        0o022,
    );
    assert_eq!(mode_of(&overlay.join(".git/FETCH_HEAD")), 0o600);
    assert_eq!(rc, shell_rc, "fetch exit parity");
    assert_eq!(out, shell_out, "fetch stdout bytes parity");
    assert_eq!(shell_out, b"==> Fetching web dotfiles...\n");
    assert_eq!(shell_err, b"", "fetch stderr parity");
}

#[test]
fn push_one_base_success_parity() {
    // Up-to-date clone base: `push` succeeds; the shell passes git's
    // stderr ("Everything up-to-date") through, so only the exit code
    // and stdout header bytes are compared.
    let dir = TempDir::new("commands-push-base").expect("fixture dir");
    let home = clone_with_upstream(dir.path(), "base");
    let home_text = home.to_string_lossy().into_owned();
    let (shell_rc, shell_out, _) = shell_one(
        &home,
        "ordinary",
        "",
        &[],
        "push",
        "base",
        "dotfiles",
        &home_text,
        &[],
        false,
    );
    assert_eq!(shell_rc, 0, "shell push must succeed");
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let rc = push_one(
        &log,
        &mut out,
        &mut err,
        &base,
        RepoKind::Base,
        "dotfiles",
        &home_text,
        &[],
    );
    assert_eq!(rc, shell_rc, "push exit parity");
    assert_eq!(out, shell_out, "push stdout bytes parity");
    assert_eq!(shell_out, b"==> Pushing dotfiles...\n");
    assert!(err.is_empty(), "successful push warns nowhere: {err:?}");
}

#[test]
fn push_one_base_failure_is_hard_fail() {
    // Base with no remote: `git push` fails, and the shell keeps its
    // hard-fail with exactly exit 1 and no warning.
    let dir = TempDir::new("commands-push-base-fail").expect("fixture dir");
    seed_repo(dir.path());
    let home_text = dir.path().to_string_lossy().into_owned();
    let (shell_rc, shell_out, _) = shell_one(
        dir.path(),
        "ordinary",
        "",
        &[],
        "push",
        "base",
        "dotfiles",
        &home_text,
        &[],
        false,
    );
    assert_ne!(shell_rc, 0, "shell push without a remote must fail");
    assert_eq!(shell_rc, 1, "shell base push fails with exactly 1");
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let rc = push_one(
        &log,
        &mut out,
        &mut err,
        &base,
        RepoKind::Base,
        "dotfiles",
        &home_text,
        &[],
    );
    assert_eq!(rc, 1, "rust base push fails with exactly 1");
    assert_eq!(out, shell_out, "push stdout bytes parity");
    assert!(err.is_empty(), "base failure warns nowhere: {err:?}");
}

#[test]
fn push_one_overlay_failure_warns_and_continues() {
    // Overlay with no remote: `git push` fails, but both engines
    // report success with a stderr warning naming the overlay.
    let dir = TempDir::new("commands-push-overlay-fail").expect("fixture dir");
    let home = TempDir::new("commands-push-overlay-home").expect("neutral home");
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record(
        "web",
        &overlay_text,
        &home.path().to_string_lossy(),
        "git",
    )];
    let (shell_rc, shell_out, shell_err) = shell_one(
        home.path(),
        "missing",
        "",
        &records,
        "push",
        "overlay",
        "web",
        &overlay_text,
        &[],
        false,
    );
    assert_eq!(
        shell_rc, 0,
        "shell overlay push failure warns and continues"
    );
    let log = Log::new(false, false);
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: home.path().to_string_lossy().into_owned(),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let rc = push_one(
        &log,
        &mut out,
        &mut err,
        &base,
        RepoKind::Overlay,
        "web",
        &overlay_text,
        &[],
    );
    assert_eq!(rc, 0, "rust overlay push failure warns and continues");
    assert_eq!(out, shell_out, "push stdout bytes parity");
    assert_eq!(shell_out, b"==> Pushing web dotfiles...\n");
    assert_eq!(err, b"  warning: web dotfiles push failed\n");
    assert!(
        shell_err
            .windows(b"  warning: web dotfiles push failed\n".len())
            .any(|window| window == b"  warning: web dotfiles push failed\n"),
        "shell warning names the overlay: {shell_err:?}"
    );
}

#[test]
fn diff_one_clean_parity() {
    // Clean overlay: `git diff` is silent, so stdout is exactly the
    // blank line plus header on both sides.
    let dir = TempDir::new("commands-diff-clean").expect("fixture dir");
    let home = TempDir::new("commands-diff-clean-home").expect("neutral home");
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record(
        "web",
        &overlay_text,
        &home.path().to_string_lossy(),
        "git",
    )];
    let (shell_rc, shell_out, shell_err) = shell_one(
        home.path(),
        "missing",
        "",
        &records,
        "diff",
        "overlay",
        "web",
        &overlay_text,
        &[],
        false,
    );
    assert_eq!(shell_rc, 0, "shell diff must succeed");
    let log = Log::new(false, false);
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: home.path().to_string_lossy().into_owned(),
    };
    let mut out = Vec::new();
    let rc = diff_one(
        &log,
        &mut out,
        &base,
        RepoKind::Overlay,
        "web",
        &overlay_text,
        &[],
    );
    assert_eq!(rc, shell_rc, "diff exit parity");
    assert_eq!(out, shell_out, "diff stdout bytes parity");
    assert_eq!(shell_out, b"\n==> web dotfiles\n");
    assert_eq!(shell_err, b"", "diff stderr parity");
}

#[test]
fn diff_one_dirty_propagates_exit() {
    // Dirty overlay: `git diff` still exits 0 while printing the
    // patch through (past the Rust header buffer, which only owns the
    // header), so compare the exit plus the header prefix.
    let dir = TempDir::new("commands-diff-dirty").expect("fixture dir");
    let home = TempDir::new("commands-diff-dirty-home").expect("neutral home");
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    std::fs::write(overlay.join("tracked.txt"), b"v2\n").expect("dirty file");
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record(
        "web",
        &overlay_text,
        &home.path().to_string_lossy(),
        "git",
    )];
    let (shell_rc, shell_out, shell_err) = shell_one(
        home.path(),
        "missing",
        "",
        &records,
        "diff",
        "overlay",
        "web",
        &overlay_text,
        &[],
        false,
    );
    assert_eq!(shell_rc, 0, "shell diff with changes exits 0");
    let log = Log::new(false, false);
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: home.path().to_string_lossy().into_owned(),
    };
    let mut out = Vec::new();
    let rc = diff_one(
        &log,
        &mut out,
        &base,
        RepoKind::Overlay,
        "web",
        &overlay_text,
        &[],
    );
    assert_eq!(rc, shell_rc, "diff exit parity");
    assert_eq!(out, b"\n==> web dotfiles\n");
    assert!(
        shell_out.starts_with(&out),
        "shell diff starts with the header: {shell_out:?}"
    );
    assert!(
        shell_out.len() > out.len(),
        "shell diff carries the patch: {shell_out:?}"
    );
    assert_eq!(shell_err, b"", "diff stderr parity");
}

#[test]
fn status_one_separate_base_parity() {
    // Separate-topology base: exercises the `--git-dir/--work-tree`
    // dispatch with `--porcelain`, silent on a clean tree.
    let dir = TempDir::new("commands-status-sep").expect("fixture dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let git_dir = home.join(".dotfiles");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    git(&git_dir, &["init", "--bare", "-q"]);
    let prefix = Base {
        topology: Topology::Separate,
        client_git_dir: git_dir.to_string_lossy().into_owned(),
        home: home.to_string_lossy().into_owned(),
    }
    .git_prefix()
    .expect("separate prefix");
    // Seed exactly one file by pathspec: a bare `add -A` would commit
    // the in-worktree git dir into itself, and the git dir itself
    // always reads as untracked noise (hence `--untracked-files=no`
    // below, which also exercises multi-word extra argv).
    std::fs::write(home.join("tracked.txt"), b"v1\n").expect("seed file");
    git_prefix(
        &prefix,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "add",
            "tracked.txt",
        ],
    );
    git_prefix(
        &prefix,
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
    let home_text = home.to_string_lossy().into_owned();
    let git_text = git_dir.to_string_lossy().into_owned();
    let (shell_rc, shell_out, shell_err) = shell_one(
        &home,
        "separate",
        &git_text,
        &[],
        "status",
        "base",
        "dotfiles",
        &home_text,
        &["--porcelain", "--untracked-files=no"],
        false,
    );
    assert_eq!(shell_rc, 0, "shell status must succeed");
    let log = Log::new(false, false);
    let base = Base {
        topology: Topology::Separate,
        client_git_dir: git_text,
        home: home_text.clone(),
    };
    let mut out = Vec::new();
    let rc = status_one(
        &log,
        &mut out,
        &base,
        RepoKind::Base,
        "dotfiles",
        &home_text,
        &["--porcelain", "--untracked-files=no"],
    );
    assert_eq!(rc, shell_rc, "status exit parity");
    assert_eq!(out, shell_out, "status stdout bytes parity");
    assert_eq!(shell_out, b"==> dotfiles\n");
    assert_eq!(shell_err, b"", "status stderr parity");
}

#[test]
fn status_one_quiet_still_prints_header() {
    // `_header` (and the overlay blank `echo`) ignore `DOT_QUIET`:
    // quiet mode prints the same header bytes on both sides.
    let dir = TempDir::new("commands-status-quiet").expect("fixture dir");
    let home = TempDir::new("commands-status-quiet-home").expect("neutral home");
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record(
        "web",
        &overlay_text,
        &home.path().to_string_lossy(),
        "git",
    )];
    let (shell_rc, shell_out, shell_err) = shell_one(
        home.path(),
        "missing",
        "",
        &records,
        "status",
        "overlay",
        "web",
        &overlay_text,
        &["--porcelain"],
        true,
    );
    assert_eq!(shell_rc, 0, "shell status must succeed");
    let log = Log::new(false, true);
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: home.path().to_string_lossy().into_owned(),
    };
    let mut out = Vec::new();
    let rc = status_one(
        &log,
        &mut out,
        &base,
        RepoKind::Overlay,
        "web",
        &overlay_text,
        &["--porcelain"],
    );
    assert_eq!(rc, shell_rc, "status exit parity");
    assert_eq!(out, shell_out, "quiet header bytes parity");
    assert_eq!(shell_out, b"\n==> web dotfiles\n");
    assert_eq!(shell_err, b"", "status stderr parity");
}

#[test]
fn fetch_all_parity() {
    // Base plus one overlay, both up-to-date clones: two headers, no
    // normalization on either side (the shell `_repo_fetch_all` has
    // none), silent git.
    let dir = TempDir::new("commands-fetch-all").expect("fixture dir");
    let home = clone_with_upstream(dir.path(), "base");
    let home_text = home.to_string_lossy().into_owned();
    let overlay = clone_with_upstream(dir.path(), "web");
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record("web", &overlay_text, &home_text, "git")];
    let (shell_rc, shell_out, shell_err) = shell_all(&home, "ordinary", "", &records, "fetch", &[]);
    assert_eq!(shell_rc, 0, "shell fetch-all must succeed");
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let extra = os_extra(&[]);
    // Realistic umask (see the one-fetch tests): both FETCH_HEADs
    // must land at exactly 0600.
    let rc = fetch_all(&log, &mut out, &base, &records, &home_text, &extra, 0o022);
    assert_eq!(mode_of(&home.join(".git/FETCH_HEAD")), 0o600);
    assert_eq!(mode_of(&overlay.join(".git/FETCH_HEAD")), 0o600);
    assert_eq!(rc, shell_rc, "fetch-all exit parity");
    assert_eq!(out, shell_out, "fetch-all stdout bytes parity");
    assert_eq!(
        shell_out,
        "==> Fetching dotfiles...\n==> Fetching web dotfiles...\n".as_bytes()
    );
    assert_eq!(shell_err, b"", "fetch-all stderr parity");
}

#[test]
fn status_all_mtime_noise_parity() {
    // `touch`ed-but-content-clean overlay file: both sides normalize
    // first (silent), then report clean `--porcelain` under two
    // headers.
    let dir = TempDir::new("commands-status-all").expect("fixture dir");
    let home = dir.path().join("home");
    seed_repo(&home);
    let home_text = home.to_string_lossy().into_owned();
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    // Stat-dirty, content-clean: refresh the mtime without editing.
    let touched = overlay.join("tracked.txt");
    let content = std::fs::read(&touched).expect("read fixture");
    std::fs::write(&touched, &content).expect("rewrite fixture");
    filetime_touch(&touched);
    let records = vec![overlay_record("web", &overlay_text, &home_text, "git")];
    let (shell_rc, shell_out, shell_err) =
        shell_all(&home, "ordinary", "", &records, "status", &["--porcelain"]);
    assert_eq!(shell_rc, 0, "shell status-all must succeed");
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let extra = os_extra(&["--porcelain"]);
    let rc = status_all(&log, &mut out, &base, &records, &home_text, &extra);
    assert_eq!(rc, shell_rc, "status-all exit parity");
    assert_eq!(out, shell_out, "status-all stdout bytes parity");
    assert_eq!(shell_out, b"==> dotfiles\n\n==> web dotfiles\n");
    assert_eq!(shell_err, b"", "status-all stderr parity");
}

/// Bump a file's mtime without changing its content, portably (a
/// rewrite followed by an explicit later timestamp would also work;
/// the rewrite alone already refreshes stat on coarse clocks).
fn filetime_touch(path: &Path) {
    let content = std::fs::read(path).expect("read for touch");
    std::fs::write(path, &content).expect("rewrite for touch");
}

#[test]
fn diff_all_clean_parity() {
    // Clean base plus overlay: `git diff` silent everywhere, two
    // headers back to back.
    let dir = TempDir::new("commands-diff-all").expect("fixture dir");
    let home = dir.path().join("home");
    seed_repo(&home);
    let home_text = home.to_string_lossy().into_owned();
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record("web", &overlay_text, &home_text, "git")];
    let (shell_rc, shell_out, shell_err) = shell_all(&home, "ordinary", "", &records, "diff", &[]);
    assert_eq!(shell_rc, 0, "shell diff-all must succeed");
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let extra = os_extra(&[]);
    let rc = diff_all(&log, &mut out, &base, &records, &home_text, &extra);
    assert_eq!(rc, shell_rc, "diff-all exit parity");
    assert_eq!(out, shell_out, "diff-all stdout bytes parity");
    assert_eq!(shell_out, b"==> dotfiles\n\n==> web dotfiles\n");
    assert_eq!(shell_err, b"", "diff-all stderr parity");
}

#[test]
fn push_all_parity() {
    // Base plus overlay, both up-to-date clones: `push` succeeds on
    // each; git's stderr chatter stays uncompared, headers and exits
    // must match.
    let dir = TempDir::new("commands-push-all").expect("fixture dir");
    let home = clone_with_upstream(dir.path(), "base");
    let home_text = home.to_string_lossy().into_owned();
    let overlay = clone_with_upstream(dir.path(), "web");
    let overlay_text = overlay.to_string_lossy().into_owned();
    let records = vec![overlay_record("web", &overlay_text, &home_text, "git")];
    let (shell_rc, shell_out, _) = shell_all(&home, "ordinary", "", &records, "push", &[]);
    assert_eq!(shell_rc, 0, "shell push-all must succeed");
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let extra = os_extra(&[]);
    let rc = push_all(
        &log, &mut out, &mut err, &base, &records, &home_text, &extra,
    );
    assert_eq!(rc, shell_rc, "push-all exit parity");
    assert_eq!(out, shell_out, "push-all stdout bytes parity");
    assert_eq!(
        shell_out,
        b"==> Pushing dotfiles...\n==> Pushing web dotfiles...\n"
    );
    assert!(err.is_empty(), "successful push-all warns nowhere: {err:?}");
}

/// Capture one `git` stdout line, asserting success.
fn git_line(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?} in {}", dir.display());
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

/// True when `object` exists in the bare `git_dir` repository.
fn object_in(git_dir: &Path, object: &str) -> bool {
    Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("cat-file")
        .arg("-t")
        .arg(object)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git")
        .success()
}

#[test]
fn push_all_stops_when_base_push_fails() {
    // Base with no remote fails hard (exactly 1): the walk stops, so
    // the overlay header never prints and the overlay's unpushed
    // commit never reaches its origin on either side.
    let dir = TempDir::new("commands-push-all-stop").expect("fixture dir");
    let home = dir.path().join("home");
    seed_repo(&home);
    let home_text = home.to_string_lossy().into_owned();
    let overlay = clone_with_upstream(dir.path(), "web");
    let overlay_text = overlay.to_string_lossy().into_owned();
    std::fs::write(overlay.join("unpushed.txt"), b"local only\n").expect("stageable file");
    git(
        &overlay,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    git(
        &overlay,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "unpushed",
        ],
    );
    let pending = git_line(&overlay, &["rev-parse", "HEAD"]);
    let origin = dir.path().join("web.git");
    let records = vec![overlay_record("web", &overlay_text, &home_text, "git")];
    let (shell_rc, shell_out, _) = shell_all(&home, "ordinary", "", &records, "push", &[]);
    assert_eq!(shell_rc, 1, "shell push-all stops with exactly 1");
    assert_eq!(
        shell_out, b"==> Pushing dotfiles...\n",
        "shell never reaches the overlay"
    );
    assert!(
        !object_in(&origin, &pending),
        "shell left the overlay commit unpushed"
    );
    let log = Log::new(false, false);
    let base = ordinary_base(&home_text);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let extra = os_extra(&[]);
    let rc = push_all(
        &log, &mut out, &mut err, &base, &records, &home_text, &extra,
    );
    assert_eq!(rc, 1, "rust push-all stops with exactly 1");
    assert_eq!(out, shell_out, "push-all stop stdout bytes parity");
    assert!(err.is_empty(), "base failure warns nowhere: {err:?}");
    assert!(
        !object_in(&origin, &pending),
        "rust left the overlay commit unpushed"
    );
}

#[test]
fn header_table_matches_shell() {
    for (op, kind, name, want) in [
        ("fetch", RepoKind::Base, "", "==> Fetching dotfiles...\n"),
        (
            "fetch",
            RepoKind::Overlay,
            "web",
            "==> Fetching web dotfiles...\n",
        ),
        ("push", RepoKind::Base, "", "==> Pushing dotfiles...\n"),
        (
            "push",
            RepoKind::Overlay,
            "web",
            "==> Pushing web dotfiles...\n",
        ),
        ("diff", RepoKind::Base, "", "==> dotfiles\n"),
        ("diff", RepoKind::Overlay, "web", "\n==> web dotfiles\n"),
        ("status", RepoKind::Base, "", "==> dotfiles\n"),
        ("status", RepoKind::Overlay, "web", "\n==> web dotfiles\n"),
    ] {
        assert_eq!(
            header_text(op, kind, name),
            Some(want.to_string()),
            "header for {op}:{kind:?}"
        );
    }
}

#[test]
fn colored_overlay_header_keeps_blank_line_outside_paint() {
    // The shell runs a bare `echo ""` BEFORE `_header`, so the blank
    // line sits outside the BOLD+WHITE paint. Baking it into the
    // painted text would drag it inside the color codes: same glyphs,
    // different bytes on a terminal.
    let dir = TempDir::new("commands-colored-header").expect("fixture dir");
    let overlay = dir.path().join("web");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    let snippet = concat!(
        "_C_BOLD=$'\\033[1m'\n",
        "_C_WHITE=$'\\033[38;2;255;255;255m'\n",
        "_C_RESET=$'\\033[0m'\n",
        "_repo_simple_header status overlay web\n",
    );
    let (shell_status, shell_out, shell_err) = shell_run(dir.path(), &[], &[], snippet);
    assert_eq!(shell_status, 0, "harness exit");
    assert!(shell_err.is_empty(), "shell stderr: {shell_err:?}");
    // Clean overlay: `status --porcelain` is silent, so `out` carries
    // exactly the painted header on both sides.
    let log = Log::new(true, false);
    let base = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: dir.path().to_string_lossy().into_owned(),
    };
    let mut out = Vec::new();
    let rc = status_one(
        &log,
        &mut out,
        &base,
        RepoKind::Overlay,
        "web",
        &overlay_text,
        &["--porcelain"],
    );
    assert_eq!(rc, 0, "status must succeed");
    assert_eq!(out, shell_out, "colored header bytes parity");
    assert_eq!(
        shell_out,
        b"\n\x1b[1m\x1b[38;2;255;255;255m==> web dotfiles\x1b[0m\n"
    );
}
