//! Direct behavioral tests for native repository-pull support primitives.

use std::path::{Path, PathBuf};
use std::process::Stdio;

#[cfg(unix)]
#[cfg(unix)]
use dot::progress_ui::Palette;
use dot::repos_base::{Base, Topology};
use dot::repos_pull_support::{
    BackupDirError, OriginMismatch, PullTally, backup_dir, conflicts_from_log, origin_mismatch,
    overlay_active, prepare_base_upstream, prepare_overlay_upstream, record_status, result_prefix,
    shell_quote,
};
use dot_test_support::TempDir;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = dot_test_support::git()
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
    // Name the cause on failure. The typed reason separates a latched
    // signal from a failed leaf step and carries that step's errno; the
    // forwarded `mkdir` diagnostics and the root's presence explain a
    // `NotFound` leaf with `root_created == false`, which means the root
    // step failed first.
    let backup =
        backup_dir(&dir.path().to_string_lossy(), &mut warnings).unwrap_or_else(|failure| {
            panic!(
                "backup dir: {failure:?} (root created: {}); forwarded diagnostics: {:?}",
                dir.path().join(".dot-backup/pull").is_dir(),
                String::from_utf8_lossy(&warnings)
            )
        });
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
    match backup_dir(&blocked.path().to_string_lossy(), &mut warnings) {
        Err(BackupDirError::Leaf {
            error,
            root_created,
            ..
        }) => {
            assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory, "{error:?}");
            assert!(!root_created, "the blocked root step must report failure");
        }
        other => panic!("a blocked root must fail its leaf step: {other:?}"),
    }
    assert!(!warnings.is_empty(), "mkdir diagnostic is forwarded");
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
fn overlay_is_active_as_a_worktree_or_with_a_remote() {
    let dir = TempDir::new("pull-active").expect("fixture dir");
    let worktree = dir.path().join("worktree");
    let plain = dir.path().join("plain");
    git(dir.path(), &["init", "--quiet", "worktree"]);
    std::fs::create_dir(&plain).expect("plain dir");
    assert!(overlay_active(&worktree, ""));
    assert!(overlay_active(&plain, "https://example.invalid/repo"));
    assert!(!overlay_active(&plain, ""));
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
    let mut diagnostics = Vec::new();
    assert_eq!(
        prepare_base_upstream(&base, &mut diagnostics, false),
        Ok(expected.clone())
    );
    assert_eq!(
        prepare_overlay_upstream(&pushed, true, None, &mut diagnostics),
        Ok(expected)
    );
    assert_eq!(String::from_utf8_lossy(&diagnostics), "");

    let lonely = lonely_repo(&dir, "lonely");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: lonely.to_string_lossy().into_owned(),
    };
    assert_eq!(
        prepare_base_upstream(&base, &mut diagnostics, false),
        Err(1)
    );
    assert_eq!(
        prepare_overlay_upstream(&lonely, true, None, &mut diagnostics),
        Err(1)
    );
    git(
        &lonely,
        &["remote", "add", "origin", "/definitely/missing/dot.git"],
    );
    git(&lonely, &["config", "branch.main.remote", "origin"]);
    git(&lonely, &["config", "branch.main.merge", "refs/heads/main"]);
    let head = git(&lonely, &["rev-parse", "HEAD"]);
    git(&lonely, &["update-ref", "refs/remotes/origin/main", &head]);
    assert_eq!(
        prepare_base_upstream(&base, &mut diagnostics, false),
        Err(2)
    );
    // The failed fetch's own diagnostics reach the caller's stream, every
    // line indented under the stage, instead of the inherited descriptor.
    let fetch_output = String::from_utf8(std::mem::take(&mut diagnostics)).expect("UTF-8");
    assert!(fetch_output.contains("fatal:"), "{fetch_output:?}");
    assert!(
        fetch_output
            .lines()
            .all(|line| line.is_empty() || line.starts_with("    ")),
        "{fetch_output:?}"
    );
    // Optional overlays keep their failed fetch quiet.
    assert_eq!(
        prepare_overlay_upstream(&lonely, true, None, &mut diagnostics),
        Err(2)
    );
    assert_eq!(String::from_utf8_lossy(&diagnostics), "");
    assert_eq!(
        prepare_overlay_upstream(&lonely, false, None, &mut diagnostics),
        Err(2)
    );
    let overlay_output = String::from_utf8(diagnostics.clone()).expect("UTF-8");
    assert!(
        overlay_output.starts_with("    fatal:"),
        "{overlay_output:?}"
    );
    let missing = Base {
        topology: Topology::Missing,
        client_git_dir: String::new(),
        home: dir.path().join("missing").to_string_lossy().into_owned(),
    };
    assert_eq!(
        prepare_base_upstream(&missing, &mut diagnostics, false),
        Err(1)
    );
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
