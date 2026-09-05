//! Differential parity tests for `repos/dirty.sh` against the
//! live shell: dirty detection, upstream resolution, remote-match
//! repair, and mtime-noise normalization across base and overlay
//! repositories.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_base::{Base, Topology};
use dot::repos_dirty;
use dot::test_support::TempDir;

/// Sources plus the init stub `model.sh` needs at source time.
/// `model.sh` runs `_dot_client_select` on load, which prints a
/// diagnostic whenever `$HOME` already holds a repo; that loader
/// noise is suppressed (the functions under test run afterwards
/// with an explicit topology, and their own stderr stays asserted).
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/dirty.sh\"\n",
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

/// Bare origin with one commit, then a clone: the clone's HEAD
/// tracks the origin, so `@{u}` resolves. Returns the clone.
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
    work
}

/// Ordinary-topology base prefix for a fixture home.
fn ordinary_prefix(home: &Path) -> Vec<OsString> {
    Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home.to_string_lossy().into_owned(),
    }
    .git_prefix()
    .expect("ordinary prefix")
}

/// Separate-topology base prefix: bare git dir plus worktree home.
fn separate_prefix(home: &Path) -> Vec<OsString> {
    let git_dir = home.join(".dotfiles");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    git(&git_dir, &["init", "--bare", "-q"]);
    Base {
        topology: Topology::Separate,
        client_git_dir: git_dir.to_string_lossy().into_owned(),
        home: home.to_string_lossy().into_owned(),
    }
    .git_prefix()
    .expect("separate prefix")
}

/// Commit through a base prefix (separate topology needs the
/// work-tree form; ordinary commits like any repo dir).
fn commit_via(prefix: &[OsString], dir: &Path, message: &str) {
    let status = Command::new("git")
        .args(prefix)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("prefix add");
    assert!(status.success(), "prefix add in {}", dir.display());
    let status = Command::new("git")
        .args(prefix)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("prefix commit");
    assert!(status.success(), "prefix commit in {}", dir.display());
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

#[test]
fn dirty_matrix_agrees() {
    let dir = TempDir::new("dirty-matrix").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    // Ordinary base plus one overlay; the sync=none overlay stays
    // dirty forever and must never count.
    seed_repo(home);
    let ordinary = ordinary_prefix(home);
    let overlay = home.join("overlay");
    seed_repo(&overlay);
    let overlay_text = overlay.to_string_lossy().into_owned();
    let skipped = home.join("skipped");
    seed_repo(&skipped);
    let skipped_text = skipped.to_string_lossy().into_owned();
    std::fs::write(skipped.join("tracked.txt"), b"local edit\n").expect("dirty skipped");
    let record = |path: &str, sync: &str| {
        format!("web|{path}|file:///repo/web.git|{home_text}/conf/10-web.conf|false|{sync}")
    };
    // (label, make-dirty, records): every row agrees on the code
    // with silent stderr.
    let cases: &[(&str, bool, bool, Vec<String>)] = &[
        ("clean", false, false, vec![]),
        ("dirty-base", true, false, vec![]),
        (
            "dirty-overlay",
            false,
            true,
            vec![record(&overlay_text, "git")],
        ),
        (
            "skipped-sync",
            false,
            false,
            vec![record(&skipped_text, "none")],
        ),
        (
            "missing-path",
            false,
            false,
            vec![record(&format!("{home_text}/gone"), "git")],
        ),
        // A dirty overlay whose sync remainder is not exactly
        // `git` still does not count.
        (
            "remainder-sync",
            false,
            true,
            vec![format!("web|{overlay_text}|u|d|o|git|x")],
        ),
    ];
    for (label, dirty_base, dirty_overlay, records) in cases {
        if *dirty_base {
            std::fs::write(home.join("tracked.txt"), b"local edit\n").expect("dirty base");
        }
        if *dirty_overlay {
            std::fs::write(overlay.join("tracked.txt"), b"local edit\n").expect("dirty overlay");
        }
        let (key, value) = records_env(records);
        let extra: &[(&str, Option<&str>)] = match &value {
            Some(text) => &[(key.as_str(), Some(text.as_str()))],
            None => &[(key.as_str(), None)],
        };
        let (code, out, serr) = shell_run(
            home,
            &[],
            extra,
            &format!(
                "{}if _is_worktree_dirty; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
                topology_env("ordinary", "")
            ),
        );
        assert_eq!(code, 0, "shell harness dirty {label}");
        assert_eq!(
            format!(
                "rc={}\n",
                i32::from(!repos_dirty::is_worktree_dirty(
                    Some(ordinary.as_slice()),
                    records
                ))
            ),
            String::from_utf8(out).expect("dirty dump"),
            "dirty code for {label}"
        );
        assert_eq!(serr, b"", "dirty stderr for {label}");
        // Restore clean state for the next row.
        git(home, &["checkout", "--", "tracked.txt"]);
        git(&overlay, &["checkout", "--", "tracked.txt"]);
    }
}

#[test]
fn configured_upstream_agrees() {
    let dir = TempDir::new("dirty-upstream").expect("fixture dir");
    let scope = dir.path();
    let linked = clone_with_upstream(scope, "linked");
    let plain = scope.join("plain");
    seed_repo(&plain);
    let linked_prefix = vec![OsString::from("-C"), OsString::from(linked.as_os_str())];
    let plain_prefix = vec![OsString::from("-C"), OsString::from(plain.as_os_str())];
    let gone_prefix = vec![
        OsString::from("-C"),
        OsString::from(scope.join("gone").as_os_str()),
    ];
    // (label, prefix): the clone reports its tracking branch on
    // both sides; the rest report none, silently.
    for (label, prefix) in [
        ("linked", &linked_prefix),
        ("plain", &plain_prefix),
        ("missing", &gone_prefix),
    ] {
        let path = PathBuf::from(prefix[1].clone());
        let (code, out, serr) = shell_run(
            scope,
            &[path.as_os_str()],
            &[],
            "if u=$(_repo_configured_upstream git -C \"$2\"); then printf 'up=%s\\n' \"$u\"; else printf 'up=none\\n'; fi",
        );
        assert_eq!(code, 0, "shell harness upstream {label}");
        let rust = repos_dirty::configured_upstream(prefix);
        assert_eq!(
            format!("up={}\n", rust.as_deref().unwrap_or("none")),
            String::from_utf8(out).expect("upstream dump"),
            "upstream value for {label}"
        );
        assert_eq!(serr, b"", "upstream stderr for {label}");
    }
    // The separate-topology prefix shape resolves the same way.
    let separate_home = scope.join("sephome");
    std::fs::create_dir_all(&separate_home).expect("separate home");
    let separate = separate_prefix(&separate_home);
    commit_via(&separate, &separate_home, "seed");
    std::fs::write(separate_home.join("tracked.txt"), b"v1\n").expect("seed file");
    let (code, out, serr) = shell_run(
        &separate_home,
        &[],
        &[],
        &format!(
            "{}if u=$(_repo_configured_upstream _base_git); then printf 'up=%s\\n' \"$u\"; else printf 'up=none\\n'; fi",
            topology_env(
                "separate",
                &separate_home.join(".dotfiles").to_string_lossy()
            )
        ),
    );
    assert_eq!(code, 0, "shell harness separate upstream");
    assert_eq!(
        format!(
            "up={}\n",
            repos_dirty::configured_upstream(&separate)
                .as_deref()
                .unwrap_or("none")
        ),
        String::from_utf8(out).expect("separate dump"),
        "separate upstream value"
    );
    assert_eq!(serr, b"", "separate upstream stderr");
}

#[test]
fn dirty_files_match_ref_agrees() {
    let dir = TempDir::new("dirty-matchref").expect("fixture dir");
    let scope = dir.path();
    let work = clone_with_upstream(scope, "match");
    let prefix = vec![OsString::from("-C"), OsString::from(work.as_os_str())];
    let work_text = work.to_string_lossy().into_owned();
    let upstream = repos_dirty::configured_upstream(&prefix).expect("fixture upstream");
    // A clean tree REFUSES: the empty listing still runs one
    // empty herestring iteration, and hashing `$worktree/` fails.
    // (Unreachable in practice: both callers guard on dirty first.)
    let (code, out, serr) = shell_run(
        scope,
        &[work.as_os_str()],
        &[],
        "if _dirty_files_match_ref \"$2\" HEAD git -C \"$2\"; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
    );
    assert_eq!(code, 0, "shell harness clean match");
    assert_eq!(
        format!(
            "rc={}\n",
            i32::from(!repos_dirty::dirty_files_match_ref(
                &work_text, "HEAD", &prefix
            ))
        ),
        String::from_utf8_lossy(&out).into_owned(),
        "clean match code"
    );
    assert_eq!(dump_rc(&out), 1, "clean tree refuses");
    assert_eq!(serr, b"", "clean match stderr");
    // Advance HEAD, then restore the upstream bytes: `diff-index`
    // lists the file while its content matches the remote. A bare
    // `touch` cannot stage this state (`diff-index` hashes content,
    // so mtime-only noise never lists).
    let upstream_bytes = stage_listed_matching(&work);
    for (label, setup, reference) in [
        ("listed-match", "listed", upstream.as_str()),
        ("edited", "edit", upstream.as_str()),
        ("unverifiable", "listed", "refs/heads/no-such-branch"),
    ] {
        match setup {
            "listed" => {
                std::fs::write(work.join("tracked.txt"), &upstream_bytes).expect("restore");
            }
            "edit" => {
                std::fs::write(work.join("tracked.txt"), b"local edit\n").expect("edit");
            }
            _ => {}
        }
        let (code, out, serr) = shell_run(
            scope,
            &[work.as_os_str(), reference.as_ref()],
            &[],
            "if _dirty_files_match_ref \"$2\" \"$3\" git -C \"$2\"; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
        );
        assert_eq!(code, 0, "shell harness match {label}");
        assert_eq!(
            format!(
                "rc={}\n",
                i32::from(!repos_dirty::dirty_files_match_ref(
                    &work_text, reference, &prefix
                ))
            ),
            String::from_utf8(out).expect("match dump"),
            "match code for {label}"
        );
        assert_eq!(serr, b"", "match stderr for {label}");
        // Restore the listed-matching state for the next row.
        std::fs::write(work.join("tracked.txt"), &upstream_bytes).expect("restore");
    }
}

/// `git diff-index --quiet HEAD` status without output.
fn git_quiet(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git")
        .success()
}

/// Advance HEAD one commit, then restore the worktree file to the
/// upstream content: `diff-index` lists the file (it differs from
/// HEAD) while the content matches the remote. Returns the upstream
/// content bytes.
fn stage_listed_matching(work: &Path) -> Vec<u8> {
    let prefix = vec![OsString::from("-C"), OsString::from(work.as_os_str())];
    let upstream = repos_dirty::configured_upstream(&prefix).expect("fixture upstream");
    let output = Command::new("git")
        .arg("-C")
        .arg(work)
        .args(["show", format!("{upstream}:tracked.txt").as_str()])
        .output()
        .expect("git show");
    assert!(output.status.success(), "git show upstream file");
    std::fs::write(work.join("tracked.txt"), b"v3\n").expect("advance file");
    commit_all(work, "v3");
    std::fs::write(work.join("tracked.txt"), &output.stdout).expect("restore upstream bytes");
    assert!(
        !git_quiet(work, &["diff-index", "--quiet", "HEAD"]),
        "fixture file must be listed as dirty"
    );
    output.stdout
}

/// Bump a file's mtime without changing content (stat-dirty but
/// content-clean) via `set_modified`: no subprocess, portable
/// across the suite platforms, with a +2s step for coarse
/// filesystem granularity.
fn filetime_touch(path: &Path, mtime: std::time::SystemTime) {
    let later = mtime + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for mtime")
        .set_modified(later)
        .expect("set mtime");
}

#[test]
fn dirty_files_match_remote_agrees() {
    let dir = TempDir::new("dirty-matchremote").expect("fixture dir");
    let scope = dir.path();
    let linked = clone_with_upstream(scope, "remote");
    let linked_prefix = vec![OsString::from("-C"), OsString::from(linked.as_os_str())];
    let linked_text = linked.to_string_lossy().into_owned();
    let plain = scope.join("plain");
    seed_repo(&plain);
    let plain_prefix = vec![OsString::from("-C"), OsString::from(plain.as_os_str())];
    let plain_text = plain.to_string_lossy().into_owned();
    // A listed file matching the remote agrees true; a repo without
    // upstream and a missing base refuse.
    let _upstream_bytes = stage_listed_matching(&linked);
    for (label, workdir, kind) in [
        ("listed-match", linked_text.as_str(), "ordinary"),
        ("no-upstream", plain_text.as_str(), "ordinary"),
        ("missing-base", linked_text.as_str(), "missing"),
    ] {
        // The shell function ignores its arguments and always uses
        // `_base_git` plus `$HOME`: export both per row. The Rust
        // `["-C", workdir]` prefixes below spell the same commands.
        let snippet = match kind {
            "missing" => {
                "HOME=\"$2\"; export HOME DOT_BASE_TOPOLOGY=missing; if _dirty_files_match_remote; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi"
            }
            _ => {
                "HOME=\"$2\"; export HOME DOT_BASE_TOPOLOGY=ordinary; if _dirty_files_match_remote; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi"
            }
        };
        let (code, out, serr) = shell_run(scope, &[workdir.as_ref()], &[], snippet);
        assert_eq!(code, 0, "shell harness remote {label}");
        let prefix: Option<&[OsString]> = match kind {
            "missing" => None,
            _ if workdir == linked_text => Some(linked_prefix.as_slice()),
            _ => Some(plain_prefix.as_slice()),
        };
        assert_eq!(
            format!(
                "rc={}\n",
                i32::from(!repos_dirty::dirty_files_match_remote(workdir, prefix))
            ),
            String::from_utf8(out).expect("remote dump"),
            "remote code for {label}"
        );
        assert_eq!(serr, b"", "remote stderr for {label}");
    }
    // An edited tree refuses on both sides.
    std::fs::write(linked.join("tracked.txt"), b"local edit\n").expect("edit");
    let (code, out, _) = shell_run(
        scope,
        &[linked.as_os_str()],
        &[],
        "HOME=\"$2\"; export HOME DOT_BASE_TOPOLOGY=ordinary; if _dirty_files_match_remote; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
    );
    assert_eq!(code, 0, "shell harness edited remote");
    assert_eq!(
        dump_rc(&out),
        i32::from(!repos_dirty::dirty_files_match_remote(
            &linked_text,
            Some(linked_prefix.as_slice())
        )),
        "edited remote code"
    );
}

/// Twin-fixture repair check: stage the same listed-matching state
/// in two clones, run the shell on one and Rust on the other, and
/// require the same code plus the same post-state bytes.
fn repair_twins(scope: &Path, name: &str) -> (PathBuf, PathBuf, Vec<u8>) {
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
    assert!(status.success(), "clone seed");
    std::fs::write(seed.join("tracked.txt"), b"v1\n").expect("seed file");
    commit_all(&seed, "seed");
    git(&seed, &["push", "-q", "origin", "HEAD"]);
    let mut twins = Vec::new();
    let mut upstream_bytes = Vec::new();
    for side in ["a", "b"] {
        let work = scope.join(format!("{name}-{side}"));
        let status = Command::new("git")
            .arg("clone")
            .arg("-q")
            .arg(&origin)
            .arg(&work)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("clone twin");
        assert!(status.success(), "clone twin");
        upstream_bytes = stage_listed_matching(&work);
        twins.push(work);
    }
    (twins.remove(0), twins.remove(0), upstream_bytes)
}

#[test]
fn try_resolve_dirty_agrees() {
    let dir = TempDir::new("dirty-resolve").expect("fixture dir");
    let scope = dir.path();
    // Listed-but-matching repairs to clean; an edit, a repo without
    // upstream, and an already-clean tree exercise the other arms.
    let (shell_work, rust_work, _) = repair_twins(scope, "resolve");
    let shell_text = shell_work.to_string_lossy().into_owned();
    let rust_text = rust_work.to_string_lossy().into_owned();
    let rust_base = ordinary_prefix(&rust_work);
    let (code, out, serr) = shell_run(
        &shell_work,
        &[],
        &[],
        &format!(
            "{}if _try_resolve_dirty; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            topology_env("ordinary", "")
        ),
    );
    assert_eq!(code, 0, "shell harness repaired");
    // `rc=0` is success: negate the Rust bool like every other code
    // assert in this file.
    let rust_clean = repos_dirty::try_resolve_dirty(&rust_text, Some(rust_base.as_slice()), &[]);
    assert_eq!(dump_rc(&out), i32::from(!rust_clean), "repaired code");
    assert!(rust_clean, "rust must repair the listed-matching base");
    assert_eq!(serr, b"", "repaired stderr");
    for (label, work) in [("shell", &shell_work), ("rust", &rust_work)] {
        assert_eq!(
            read_bytes(&work.join("tracked.txt")),
            b"v3\n",
            "{label} post-repair bytes"
        );
        assert!(
            git_quiet(work, &["diff-index", "--quiet", "HEAD"]),
            "{label} post-repair clean"
        );
    }
    for work in [&shell_work, &rust_work] {
        std::fs::write(work.join("tracked.txt"), b"local edit\n").expect("edit");
    }
    let (code, out, _) = shell_run(
        &shell_work,
        &[],
        &[],
        &format!(
            "{}if _try_resolve_dirty; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            topology_env("ordinary", "")
        ),
    );
    assert_eq!(code, 0, "shell harness edited resolve");
    let rust_clean = repos_dirty::try_resolve_dirty(&rust_text, Some(rust_base.as_slice()), &[]);
    assert_eq!(dump_rc(&out), i32::from(!rust_clean), "edited resolve code");
    assert!(!rust_clean, "rust must refuse the edited base");
    for (label, work) in [("shell", &shell_work), ("rust", &rust_work)] {
        assert_eq!(
            read_bytes(&work.join("tracked.txt")),
            b"local edit\n",
            "{label} edited bytes kept"
        );
        assert!(
            !git_quiet(work, &["diff-index", "--quiet", "HEAD"]),
            "{label} edited stays dirty"
        );
    }
    // No upstream refuses without mutating (single fixture is safe:
    // neither side fetches nor checks out on this arm).
    let plain = scope.join("resolve-plain");
    seed_repo(&plain);
    std::fs::write(plain.join("tracked.txt"), b"local edit\n").expect("edit");
    let plain_prefix = vec![OsString::from("-C"), OsString::from(plain.as_os_str())];
    let plain_text = plain.to_string_lossy().into_owned();
    let (code, out, serr) = shell_run(
        &plain,
        &[],
        &[],
        &format!(
            "{}if _try_resolve_dirty; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            topology_env("ordinary", "")
        ),
    );
    assert_eq!(code, 0, "shell harness plain resolve");
    assert_eq!(
        dump_rc(&out),
        i32::from(!repos_dirty::try_resolve_dirty(
            &plain_text,
            Some(plain_prefix.as_slice()),
            &[]
        )),
        "plain resolve code"
    );
    assert_eq!(dump_rc(&out), 1, "plain resolve refuses");
    assert_eq!(serr, b"", "plain resolve stderr");
    // An already-clean tree resolves true on both sides.
    let fresh = clone_with_upstream(scope, "resolve-clean");
    let fresh_prefix = vec![OsString::from("-C"), OsString::from(fresh.as_os_str())];
    let fresh_text = fresh.to_string_lossy().into_owned();
    let (code, out, serr) = shell_run(
        &fresh,
        &[],
        &[],
        &format!(
            "{}if _try_resolve_dirty; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            topology_env("ordinary", "")
        ),
    );
    assert_eq!(code, 0, "shell harness clean resolve");
    assert_eq!(
        dump_rc(&out),
        i32::from(!repos_dirty::try_resolve_dirty(
            &fresh_text,
            Some(fresh_prefix.as_slice()),
            &[]
        )),
        "clean resolve code"
    );
    assert_eq!(dump_rc(&out), 0, "clean resolve repairs");
    assert_eq!(serr, b"", "clean resolve stderr");
    // A listed-matching overlay repairs with no base selected.
    let (shell_over, rust_over, _) = repair_twins(scope, "resolve-over");
    let shell_record = format!(
        "web|{}|file:///repo/web.git|{}/conf|false|git",
        shell_over.to_string_lossy(),
        shell_text
    );
    let rust_record = format!(
        "web|{}|file:///repo/web.git|{}/conf|false|git",
        rust_over.to_string_lossy(),
        rust_text
    );
    let (code, out, serr) = shell_run(
        scope,
        &[],
        &[("OVERLAY_RECORDS", Some(shell_record.as_str()))],
        &format!(
            "{}if _try_resolve_dirty; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            topology_env("missing", "")
        ),
    );
    assert_eq!(code, 0, "shell harness overlay resolve");
    let rust_clean = repos_dirty::try_resolve_dirty("", None, &[rust_record]);
    assert_eq!(
        dump_rc(&out),
        i32::from(!rust_clean),
        "overlay resolve code"
    );
    assert!(rust_clean, "rust must repair the overlay");
    assert_eq!(serr, b"", "overlay resolve stderr");
    for (label, work) in [("shell", &shell_over), ("rust", &rust_over)] {
        assert_eq!(
            read_bytes(&work.join("tracked.txt")),
            b"v3\n",
            "{label} overlay post-repair bytes"
        );
    }
}

/// Read a file's bytes for post-state comparison.
fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read fixture")
}

/// Bump mtime without changing content: `diff-files` lists the
/// file while `diff --quiet` still passes (stat-dirty noise).
fn stage_noise(path: &Path) {
    let mtime = std::fs::metadata(path)
        .expect("stat")
        .modified()
        .expect("mtime");
    filetime_touch(path, mtime);
}

#[test]
fn checkout_dirty_files_agrees() {
    let dir = TempDir::new("dirty-checkout").expect("fixture dir");
    let scope = dir.path();
    let shell_work = scope.join("shell");
    let rust_work = scope.join("rust");
    for work in [&shell_work, &rust_work] {
        seed_repo(work);
        std::fs::write(work.join("tracked.txt"), b"local edit\n").expect("edit");
    }
    let rust_prefix = vec![OsString::from("-C"), OsString::from(rust_work.as_os_str())];
    let (code, out, serr) = shell_run(
        scope,
        &[shell_work.as_os_str()],
        &[],
        "_checkout_dirty_files git -C \"$2\"; printf 'rc=%s\\n' \"$?\"",
    );
    assert_eq!(code, 0, "shell harness checkout");
    repos_dirty::checkout_dirty_files(&rust_prefix);
    assert_eq!(dump_rc(&out), 0, "checkout code");
    assert_eq!(serr, b"", "checkout stderr");
    for (label, work) in [("shell", &shell_work), ("rust", &rust_work)] {
        assert_eq!(
            read_bytes(&work.join("tracked.txt")),
            b"v1\n",
            "{label} post-checkout bytes"
        );
        assert!(
            git_quiet(work, &["diff-index", "--quiet", "HEAD"]),
            "{label} post-checkout clean"
        );
    }
    // A missing repository is a silent no-op on both sides.
    let gone_prefix = vec![
        OsString::from("-C"),
        OsString::from(scope.join("gone").as_os_str()),
    ];
    let (code, out, serr) = shell_run(
        scope,
        &[scope.join("gone").as_os_str()],
        &[],
        "_checkout_dirty_files git -C \"$2\"; printf 'rc=%s\\n' \"$?\"",
    );
    assert_eq!(code, 0, "shell harness missing checkout");
    repos_dirty::checkout_dirty_files(&gone_prefix);
    assert_eq!(dump_rc(&out), 0, "missing checkout code");
    assert_eq!(serr, b"", "missing checkout stderr");
}

#[test]
fn normalize_dirty_files_agrees() {
    let dir = TempDir::new("dirty-normalize").expect("fixture dir");
    let scope = dir.path();
    let shell_work = scope.join("shell");
    let rust_work = scope.join("rust");
    for work in [&shell_work, &rust_work] {
        seed_repo(work);
        stage_noise(&work.join("tracked.txt"));
        assert!(
            !git_quiet(work, &["diff-files", "--quiet"]),
            "fixture must be stat-dirty"
        );
    }
    let rust_prefix = vec![OsString::from("-C"), OsString::from(rust_work.as_os_str())];
    let (code, out, serr) = shell_run(
        scope,
        &[shell_work.as_os_str()],
        &[],
        "_normalize_dirty_files git -C \"$2\"; printf 'rc=%s\\n' \"$?\"",
    );
    assert_eq!(code, 0, "shell harness normalize");
    repos_dirty::normalize_dirty_files(&rust_prefix);
    assert_eq!(dump_rc(&out), 0, "normalize code");
    assert_eq!(serr, b"", "normalize stderr");
    for (label, work) in [("shell", &shell_work), ("rust", &rust_work)] {
        let output = Command::new("git")
            .arg("-C")
            .arg(work)
            .args(["diff-files", "--name-only"])
            .output()
            .expect("diff-files");
        assert!(output.status.success(), "{label} diff-files runs");
        assert_eq!(output.stdout, b"", "{label} noise normalized away");
    }
    // Real edits survive on both sides.
    for work in [&shell_work, &rust_work] {
        std::fs::write(work.join("tracked.txt"), b"local edit\n").expect("edit");
    }
    let (code, out, _) = shell_run(
        scope,
        &[shell_work.as_os_str()],
        &[],
        "_normalize_dirty_files git -C \"$2\"; printf 'rc=%s\\n' \"$?\"",
    );
    assert_eq!(code, 0, "shell harness edited normalize");
    repos_dirty::normalize_dirty_files(&rust_prefix);
    assert_eq!(dump_rc(&out), 0, "edited normalize code");
    for (label, work) in [("shell", &shell_work), ("rust", &rust_work)] {
        assert_eq!(
            read_bytes(&work.join("tracked.txt")),
            b"local edit\n",
            "{label} edited bytes kept"
        );
    }
}

/// Stage a mixed base+overlay tree: mtime noise everywhere plus one
/// real edit in the overlay, and a sync=none overlay whose noise
/// must survive (it is never normalized).
fn stage_mixed(home: &Path) -> Vec<String> {
    let home_text = home.to_string_lossy().into_owned();
    git(home, &["init", "-q"]);
    std::fs::write(home.join("tracked.txt"), b"v1\n").expect("seed file");
    std::fs::write(home.join("other.txt"), b"v1\n").expect("seed file");
    commit_all(home, "seed");
    let overlay = home.join("overlay");
    std::fs::create_dir_all(&overlay).expect("overlay dir");
    std::fs::write(overlay.join("tracked.txt"), b"v1\n").expect("seed file");
    std::fs::write(overlay.join("other.txt"), b"v1\n").expect("seed file");
    commit_all_overlay(&overlay);
    let skipped = home.join("skipped");
    std::fs::create_dir_all(&skipped).expect("skipped dir");
    std::fs::write(skipped.join("tracked.txt"), b"v1\n").expect("seed file");
    commit_all_overlay(&skipped);
    stage_noise(&home.join("tracked.txt"));
    stage_noise(&overlay.join("tracked.txt"));
    stage_noise(&skipped.join("tracked.txt"));
    std::fs::write(overlay.join("other.txt"), b"local edit\n").expect("edit");
    let overlay_text = overlay.to_string_lossy().into_owned();
    let skipped_text = skipped.to_string_lossy().into_owned();
    vec![
        format!("web|{overlay_text}|file:///repo/web.git|{home_text}/c|false|git"),
        format!("web|{skipped_text}|file:///repo/web.git|{home_text}/c|false|none"),
        format!("web|{home_text}/gone|file:///repo/web.git|{home_text}/c|false|git"),
    ]
}

/// `git init` + commit for an overlay fixture (identity flags).
fn commit_all_overlay(dir: &Path) {
    git(dir, &["init", "-q"]);
    commit_all(dir, "seed");
}

#[test]
fn normalize_filtered_agrees() {
    let dir = TempDir::new("dirty-filtered").expect("fixture dir");
    let scope = dir.path();
    // Twin homes: the shell normalizes one with the real parallel
    // machinery (owner traps installed like the suite does), Rust
    // the other sequentially. Same silence, same tree state.
    let shell_home = scope.join("shell-home");
    let rust_home = scope.join("rust-home");
    std::fs::create_dir_all(&shell_home).expect("shell home");
    std::fs::create_dir_all(&rust_home).expect("rust home");
    let shell_records = stage_mixed(&shell_home);
    let rust_records = stage_mixed(&rust_home);
    let shell_joined = shell_records.join("\n");
    let rust_base = ordinary_prefix(&rust_home);
    let (code, out, serr) = shell_run(
        &shell_home,
        &[],
        &[("OVERLAY_RECORDS", Some(shell_joined.as_str()))],
        &format!(
            "{}_dot_cleanup_install_owner_traps; _normalize_filtered; printf 'rc=%s\\n' \"$?\"",
            topology_env("ordinary", "")
        ),
    );
    assert_eq!(code, 0, "shell harness filtered");
    repos_dirty::normalize_filtered(Some(rust_base.as_slice()), &rust_records);
    assert_eq!(dump_rc(&out), 0, "filtered code");
    assert_eq!(serr, b"", "filtered stderr");
    for (label, home) in [("shell", &shell_home), ("rust", &rust_home)] {
        // (repo, expected): noise is gone everywhere, but the real
        // edit keeps the overlay listed.
        for (repo, expected) in [
            (home.clone(), b"".as_slice()),
            (home.join("overlay"), b"other.txt\n".as_slice()),
        ] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["diff-files", "--name-only"])
                .output()
                .expect("diff-files");
            assert!(output.status.success(), "{label} diff-files runs");
            assert_eq!(
                output.stdout.as_slice(),
                expected,
                "{label} post-normalize listing"
            );
        }
        assert_eq!(
            read_bytes(&home.join("overlay/other.txt")),
            b"local edit\n",
            "{label} real edit kept"
        );
        assert!(
            !git_quiet(&home.join("skipped"), &["diff-files", "--quiet"]),
            "{label} sync=none noise survives"
        );
    }
}
