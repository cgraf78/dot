//! Native contracts for init plan review, conflict parking/restoration, and
//! durable completion publication.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_candidate as candidate;
use dot::init_client_plan as plan;
use dot::temp::MoveCache;
use dot_test_support::TempDir;

fn write(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parent");
    }
    std::fs::write(&path, bytes).expect("fixture write");
    path
}

fn shape(path: &Path) -> Option<Vec<u8>> {
    std::fs::symlink_metadata(path).ok()?;
    Some(
        std::fs::read_link(path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .or_else(|_| std::fs::read(path))
            .unwrap_or_default(),
    )
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("fixture metadata")
        .permissions()
        .mode()
        & 0o777
}

fn private_dir(path: &Path) -> dot::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(path).map_err(|source| dot::errors::Error::Io {
        context: "create test private directory",
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        dot::errors::Error::Io {
            context: "chmod test private directory",
            source,
        }
    })
}

fn present(
    target: &Path,
    kind: &str,
    _dev: &str,
    _ino: &str,
    _mode: &str,
    _size: &str,
    _value: &str,
) -> bool {
    let exists = std::fs::symlink_metadata(target).is_ok();
    if kind == "absent" { !exists } else { exists }
}

fn fail_private(_: &Path) -> dot::Result<()> {
    Err(dot::errors::Error::Usage {
        message: "private directory refused",
    })
}

fn row(path: &str) -> String {
    format!("{path}\tregular\t1\t2\t600\t3\tvalue\n")
}

fn git(args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

struct PlanCase {
    _dir: TempDir,
    candidate: PathBuf,
    tree: PathBuf,
}

fn plan_case(tag: &str, config: Option<&str>, tree: &str) -> PlanCase {
    let dir = TempDir::new(tag).expect("fixture");
    let candidate = dir.path().join("candidate");
    git(&[
        "init",
        "--quiet",
        "--initial-branch",
        "main",
        candidate.to_str().expect("candidate path"),
    ]);
    match config {
        Some(content) => {
            write(&candidate, ".config/dot/config", content.as_bytes());
        }
        None => {
            write(&candidate, "other", b"unrelated\n");
        }
    }
    let candidate_text = candidate.to_str().expect("candidate path");
    git(&["-C", candidate_text, "add", "-A"]);
    git(&["-C", candidate_text, "commit", "--quiet", "-m", "candidate"]);
    let tree_path = write(dir.path(), "tree", tree.as_bytes());
    PlanCase {
        _dir: dir,
        candidate,
        tree: tree_path,
    }
}

fn summary(case: &PlanCase, backup: &str, skip_provider: bool) -> dot::Result<Vec<u8>> {
    summary_with_policy(case, backup, skip_provider, None)
}

fn summary_with_policy(
    case: &PlanCase,
    backup: &str,
    skip_provider: bool,
    env_policy: Option<&str>,
) -> dot::Result<Vec<u8>> {
    plan::plan_summary(&plan::PlanInputs {
        candidate: &case.candidate,
        branch: "main",
        tree: &case.tree,
        backup,
        identity: "github.com/example/dot",
        home: case._dir.path(),
        skip_provider,
        env_policy,
    })
}

#[test]
fn confirm_empty_manifest_is_silent() {
    let dir = TempDir::new("confirm-empty").expect("fixture");
    for manifest in [dir.path().join("missing"), write(dir.path(), "empty", b"")] {
        assert_eq!(
            plan::confirm(&manifest, true, Path::new("/unopened/tty")).expect("empty accepted"),
            b"",
        );
    }
}

#[test]
fn confirm_listing_matches_cut_first() {
    let dir = TempDir::new("confirm-list").expect("fixture");
    let content = if cfg!(target_os = "macos") {
        b"file1\tregular\nNOTAB\n\n\tlead\np\t\tk\na\tb\tc\ttail-row".as_slice()
    } else {
        b"file1\tregular\nNOTAB\n\n\tlead\np\t\tk\na\tb\tc\nn\0ul\tk\ntail-row".as_slice()
    };
    let manifest = write(dir.path(), "manifest", content);
    let output = plan::confirm(&manifest, true, Path::new("/unopened/tty")).expect("confirmed");
    let expected = if cfg!(target_os = "macos") {
        b"dot init: conflicting paths will be backed up:\n  file1\n  NOTAB\n  \n  \n  p\n  a\n  tail-row".as_slice()
    } else {
        b"dot init: conflicting paths will be backed up:\n  file1\n  NOTAB\n  \n  \n  p\n  a\n  n\0ul\n  tail-row\n".as_slice()
    };
    assert_eq!(output, expected, "exact first-field listing bytes");
}

#[test]
fn confirm_without_yes_refuses() {
    let dir = TempDir::new("confirm-refuse").expect("fixture");
    let manifest = write(dir.path(), "manifest", row("file1").as_bytes());
    let missing = dir.path().join("missing-tty");
    assert!(plan::confirm(&manifest, false, &missing).is_err());
    let regular = write(dir.path(), "regular-tty", b"yes\n");
    assert!(plan::confirm(&manifest, false, &regular).is_err());
    assert_eq!(
        std::fs::read(&regular).expect("prompt bytes"),
        b"Continue? [y/N] "
    );
}

#[test]
fn plan_summary_reports() {
    let backup = "/home/u/.local/state/dot/init/backup";
    for (tag, config, tree, provider, policy, extensions, count) in [
        (
            "defaults",
            None,
            "a\nc\ne\n",
            "none",
            "pinned",
            "disabled",
            3,
        ),
        (
            "minimal",
            Some("version=1\n"),
            "only\n",
            "none",
            "pinned",
            "disabled",
            1,
        ),
        (
            "full",
            Some(
                "version=1\nextension_api=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n",
            ),
            "a\nb\nc\nd\n",
            "shdeps",
            "latest",
            "enabled",
            4,
        ),
        (
            "unterminated",
            None,
            "a\nb",
            "none",
            "pinned",
            "disabled",
            1,
        ),
    ] {
        let case = plan_case(tag, config, tree);
        let expected = format!(
            "dot init plan:\n  repository: github.com/example/dot\n  branch: main\n  tracked paths: {count}\n  backup: {backup}\n  dependency provider: {provider}\n  shdeps update policy: {policy}\n  extensions: {extensions}\n"
        );
        assert_eq!(
            summary(&case, backup, false).expect("summary"),
            expected.as_bytes()
        );
        let preview = case.candidate.join("dot-config.preview");
        assert_eq!(shape(&preview), config.map(|text| text.as_bytes().to_vec()));
    }
}

#[test]
fn plan_summary_loads_candidate_config_without_shell_engine() {
    let case = plan_case(
        "native-config",
        Some(
            "version=1\nextension_api=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n",
        ),
        "one\n",
    );
    assert_eq!(summary(&case, "/tmp/backup", false).expect("summary"), b"dot init plan:\n  repository: github.com/example/dot\n  branch: main\n  tracked paths: 1\n  backup: /tmp/backup\n  dependency provider: shdeps\n  shdeps update policy: latest\n  extensions: enabled\n");
    assert_eq!(shape(&case.candidate.join("dot-config.preview")), Some(b"version=1\nextension_api=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n".to_vec()));
}

#[test]
fn plan_summary_applies_process_policy_override() {
    let case = plan_case(
        "env-policy",
        Some("version=1\ndependency_provider=shdeps\nshdeps_update_policy=pinned\n"),
        "only\n",
    );
    let report = summary_with_policy(&case, "/backup", false, Some("latest")).expect("summary");
    assert!(
        String::from_utf8_lossy(&report).contains("shdeps update policy: latest"),
        "{}",
        String::from_utf8_lossy(&report)
    );
}

#[test]
fn plan_summary_flags_and_failures() {
    let backup = "/backup";
    let shdeps = plan_case(
        "skip",
        Some(
            "version=1\nextension_api=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n",
        ),
        "a\n",
    );
    let output =
        String::from_utf8(summary(&shdeps, backup, true).expect("skip summary")).expect("UTF-8");
    assert_eq!(
        output,
        "dot init plan:\n  repository: github.com/example/dot\n  branch: main\n  tracked paths: 1\n  backup: /backup\n  dependency provider: shdeps (skipped for this invocation)\n  shdeps update policy: latest\n  extensions: enabled\n"
    );
    let none = plan_case("skip-none", Some("version=1\n"), "a\n");
    let output =
        String::from_utf8(summary(&none, backup, true).expect("none summary")).expect("UTF-8");
    assert_eq!(
        output,
        "dot init plan:\n  repository: github.com/example/dot\n  branch: main\n  tracked paths: 1\n  backup: /backup\n  dependency provider: none\n  shdeps update policy: pinned\n  extensions: disabled\n"
    );

    let missing = plan_case("missing-tree", None, "a\n");
    std::fs::remove_file(&missing.tree).expect("remove tree");
    assert!(summary(&missing, backup, false).is_err());

    let garbage = plan_case("garbage", Some("bogus\n"), "a\n");
    assert!(summary(&garbage, backup, false).is_err());
    assert_eq!(
        shape(&garbage.candidate.join("dot-config.preview")),
        Some(b"bogus\n".to_vec())
    );
}

fn seed_conflicts(home: &Path) {
    write(home, "file1", b"hello\n");
    write(home, "sub/file2", b"nested\n");
    write(home, "dir1/inner", b"inner\n");
    std::os::unix::fs::symlink("file1", home.join("link1")).expect("fixture symlink");
}

#[test]
fn move_conflicts_parks_live_tree() {
    let dir = TempDir::new("move-live").expect("fixture");
    let home = dir.path().join("home");
    seed_conflicts(&home);
    let manifest = write(
        dir.path(),
        "manifest",
        format!(
            "{}{}{}{}ghost\tabsent\t-\t-\t-\t-\t-\n",
            row("file1"),
            row("sub/file2"),
            row("dir1"),
            row("link1")
        )
        .as_bytes(),
    );
    let backup = dir.path().join("backup");
    let mut cache = MoveCache::default();
    plan::move_conflicts(
        &manifest,
        &backup,
        &home,
        dir.path(),
        &present,
        &private_dir,
        &mut cache,
    )
    .expect("park conflicts");
    assert_eq!(
        shape(&backup.join("manifest")),
        Some(std::fs::read(&manifest).expect("manifest bytes"))
    );
    assert_eq!(mode(&backup.join("manifest")), 0o600);
    for (relative, bytes) in [
        ("file1", b"hello\n".as_slice()),
        ("sub/file2", b"nested\n"),
        ("dir1/inner", b"inner\n"),
        ("link1", b"file1"),
    ] {
        assert_eq!(shape(&home.join(relative)), None, "live removed {relative}");
        assert_eq!(
            shape(&backup.join(relative)),
            Some(bytes.to_vec()),
            "parked {relative}"
        );
    }
    assert_eq!(shape(&backup.join("ghost")), None);
    plan::move_conflicts(
        &manifest,
        &backup,
        &home,
        dir.path(),
        &present,
        &private_dir,
        &mut cache,
    )
    .expect("idempotent reuse");
}

#[test]
fn move_conflicts_refuses() {
    // Changed stored manifest refuses before parking a second path.
    let dir = TempDir::new("move-refuse-manifest").expect("fixture");
    let home = dir.path().join("home");
    seed_conflicts(&home);
    let manifest = write(dir.path(), "manifest", row("file1").as_bytes());
    let backup = dir.path().join("backup");
    let mut cache = MoveCache::default();
    plan::move_conflicts(
        &manifest,
        &backup,
        &home,
        dir.path(),
        &present,
        &private_dir,
        &mut cache,
    )
    .expect("first park");
    std::fs::write(&manifest, row("sub/file2")).expect("rewrite manifest");
    assert!(
        plan::move_conflicts(
            &manifest,
            &backup,
            &home,
            dir.path(),
            &present,
            &private_dir,
            &mut cache
        )
        .is_err()
    );
    assert_eq!(shape(&home.join("sub/file2")), Some(b"nested\n".to_vec()));

    // Matcher refusal preserves a changed live path.
    let dir = TempDir::new("move-refuse-changed").expect("fixture");
    let home = dir.path().join("home");
    write(&home, "file1", b"tampered\n");
    let manifest = write(dir.path(), "manifest", row("file1").as_bytes());
    let backup = dir.path().join("backup");
    let never = |_: &Path, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str| false;
    let mut cache = MoveCache::default();
    assert!(
        plan::move_conflicts(
            &manifest,
            &backup,
            &home,
            dir.path(),
            &never,
            &private_dir,
            &mut cache
        )
        .is_err()
    );
    assert_eq!(shape(&home.join("file1")), Some(b"tampered\n".to_vec()));

    // Occupied backup stages the manifest, refuses, and preserves both paths.
    let dir = TempDir::new("move-refuse-occupied").expect("fixture");
    let home = dir.path().join("home");
    write(&home, "file1", b"live\n");
    let manifest = write(dir.path(), "manifest", row("file1").as_bytes());
    let backup = dir.path().join("backup");
    write(&backup, "file1", b"squatter\n");
    let mut cache = MoveCache::default();
    assert!(
        plan::move_conflicts(
            &manifest,
            &backup,
            &home,
            dir.path(),
            &present,
            &private_dir,
            &mut cache
        )
        .is_err()
    );
    assert_eq!(
        shape(&backup.join("manifest")),
        Some(row("file1").into_bytes())
    );
    assert_eq!(shape(&home.join("file1")), Some(b"live\n".to_vec()));
    assert_eq!(shape(&backup.join("file1")), Some(b"squatter\n".to_vec()));

    let dir = TempDir::new("move-refuse-private").expect("fixture");
    let home = dir.path().join("home");
    write(&home, "file1", b"live\n");
    let manifest = write(dir.path(), "manifest", row("file1").as_bytes());
    let backup = dir.path().join("backup");
    assert!(
        plan::move_conflicts(
            &manifest,
            &backup,
            &home,
            dir.path(),
            &present,
            &fail_private,
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(shape(&backup), None);
    assert_eq!(shape(&home.join("file1")), Some(b"live\n".to_vec()));
}

#[test]
fn move_conflicts_refuses_a_live_file_rewritten_after_snapshot() {
    let dir = TempDir::new("move-refuse-rewritten").expect("fixture");
    let home = dir.path().join("home");
    let live = write(&home, ".profile", b"original\n");
    let frozen = candidate::snapshot_path(&live).expect("freeze original generation");
    let manifest = write(
        dir.path(),
        "manifest",
        format!(".profile\t{frozen}\n").as_bytes(),
    );
    let backup = dir.path().join("backup");

    std::fs::write(&live, b"late\n").expect("replace live bytes after planning");
    let matches =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            candidate::path_state_matches(path, kind, dev, ino, mode, size, value)
        };
    let result = plan::move_conflicts(
        &manifest,
        &backup,
        &home,
        dir.path(),
        &matches,
        &private_dir,
        &mut MoveCache::default(),
    );

    assert!(result.is_err(), "rewritten live generation must refuse");
    assert_eq!(shape(&live), Some(b"late\n".to_vec()));
    assert_eq!(shape(&backup.join(".profile")), None);
}

struct Recorder {
    calls: RefCell<Vec<String>>,
    answers: RefCell<Vec<bool>>,
}

impl Recorder {
    fn new(answers: &[bool]) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            answers: RefCell::new(answers.to_vec()),
        }
    }
    fn matcher(&self) -> impl Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool + '_ {
        |target, kind, dev, ino, mode, size, value| {
            self.calls.borrow_mut().push(format!(
                "{}|{kind}|{dev}|{ino}|{mode}|{size}|{value}",
                target.display()
            ));
            self.answers.borrow_mut().remove(0)
        }
    }
}

#[test]
fn move_conflicts_record_rows() {
    struct Case<'a> {
        name: &'a str,
        manifest: &'a [u8],
        answers: &'a [bool],
        ok: bool,
        calls: &'a [&'a str],
    }
    let cases = [
        Case {
            name: "plain-accept",
            manifest: b"p\tk\td\ti\tm\ts\tv\n",
            answers: &[true],
            ok: true,
            calls: &["BACKUP/p|k|d|i|m|s|v"],
        },
        Case {
            name: "plain-refuse",
            manifest: b"p\tk\td\ti\tm\ts\tv\n",
            answers: &[false, false],
            ok: false,
            calls: &["BACKUP/p|k|d|i|m|s|v", "HOME/p|k|d|i|m|s|v"],
        },
        Case {
            name: "leading-tab-strips",
            manifest: b"\tp\tk\td\ti\tm\ts\tv\n",
            answers: &[true],
            ok: true,
            calls: &["BACKUP/p|k|d|i|m|s|v"],
        },
        Case {
            name: "doubled-tab-collapses",
            manifest: b"p\t\tk\td\ti\tm\ts\tv\n",
            answers: &[true],
            ok: true,
            calls: &["BACKUP/p|k|d|i|m|s|v"],
        },
        Case {
            name: "extra-fields-fold-right",
            manifest: b"a\tb\tc\td\te\tf\tg\te1\n",
            answers: &[true],
            ok: true,
            calls: &["BACKUP/a|b|c|d|e|f|g\te1"],
        },
        Case {
            name: "blank-line-skips",
            manifest: b"\nq\tk2\td\ti\tm\ts\tv\n",
            answers: &[true],
            ok: true,
            calls: &["BACKUP/q|k2|d|i|m|s|v"],
        },
        Case {
            name: "empty-fields-skip",
            manifest: b"\t\n",
            answers: &[],
            ok: true,
            calls: &[],
        },
        Case {
            name: "unterminated-tail-skips",
            manifest: b"p\tk\td\ti\tm\ts\tv",
            answers: &[],
            ok: true,
            calls: &[],
        },
        Case {
            name: "nul-bytes-strip",
            manifest: b"n\0p\tk\td\ti\tm\ts\tv\n",
            answers: &[true],
            ok: true,
            calls: &["BACKUP/np|k|d|i|m|s|v"],
        },
    ];
    for case in cases {
        let dir = TempDir::new(case.name).expect("fixture");
        let home = dir.path().join("home");
        let backup = dir.path().join("backup");
        let manifest = write(dir.path(), "manifest", case.manifest);
        let recorder = Recorder::new(case.answers);
        let result = plan::move_conflicts(
            &manifest,
            &backup,
            &home,
            dir.path(),
            &recorder.matcher(),
            &private_dir,
            &mut MoveCache::default(),
        );
        assert_eq!(result.is_ok(), case.ok, "{} verdict", case.name);
        let calls = recorder
            .calls
            .borrow()
            .iter()
            .map(|call| {
                call.replacen(&backup.display().to_string(), "BACKUP", 1)
                    .replacen(&home.display().to_string(), "HOME", 1)
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, case.calls, "{} exact calls", case.name);
        assert_eq!(
            shape(&backup.join("manifest")),
            Some(case.manifest.to_vec()),
            "{} staged manifest",
            case.name
        );
    }
}

fn seed_stash(backup: &Path) {
    write(backup, "file1", b"hello\n");
    write(backup, "sub/file2", b"nested\n");
    write(backup, "dir1/inner", b"inner\n");
    std::os::unix::fs::symlink("file1", backup.join("link1")).expect("stash symlink");
}

#[test]
fn restore_backups_restores_live_stash() {
    let dir = TempDir::new("restore-live").expect("fixture");
    let home = dir.path().join("home");
    let backup = dir.path().join("backup");
    seed_stash(&backup);
    write(
        &backup,
        "manifest",
        format!(
            "{}{}{}{}",
            row("file1"),
            row("sub/file2"),
            row("dir1"),
            row("link1")
        )
        .as_bytes(),
    );
    plan::restore_backups(&backup, &home, &present, &mut MoveCache::default()).expect("restore");
    for (relative, bytes) in [
        ("file1", b"hello\n".as_slice()),
        ("sub/file2", b"nested\n"),
        ("dir1/inner", b"inner\n"),
        ("link1", b"file1"),
    ] {
        assert_eq!(
            shape(&home.join(relative)),
            Some(bytes.to_vec()),
            "restored {relative}"
        );
        assert_eq!(
            shape(&backup.join(relative)),
            None,
            "stash consumed {relative}"
        );
    }
}

#[test]
fn restore_backups_gates_and_refusals() {
    let dir = TempDir::new("restore-noop").expect("fixture");
    plan::restore_backups(
        &dir.path().join("missing"),
        dir.path(),
        &present,
        &mut MoveCache::default(),
    )
    .expect("missing no-op");
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).expect("empty backup");
    plan::restore_backups(&empty, dir.path(), &present, &mut MoveCache::default())
        .expect("no-manifest no-op");

    let absent = dir.path().join("absent");
    write(&absent, "manifest", row("ghost").as_bytes());
    plan::restore_backups(&absent, dir.path(), &present, &mut MoveCache::default())
        .expect("absent stash skips");

    let changed = dir.path().join("changed");
    write(&changed, "file1", b"tampered\n");
    write(&changed, "manifest", row("file1").as_bytes());
    let never = |_: &Path, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str| false;
    assert!(
        plan::restore_backups(&changed, dir.path(), &never, &mut MoveCache::default()).is_err()
    );
    assert_eq!(shape(&changed.join("file1")), Some(b"tampered\n".to_vec()));

    let occupied = dir.path().join("occupied");
    write(&occupied, "file1", b"stash\n");
    write(&occupied, "manifest", row("file1").as_bytes());
    write(dir.path(), "file1", b"squatter\n");
    assert!(
        plan::restore_backups(&occupied, dir.path(), &present, &mut MoveCache::default()).is_err()
    );
    assert_eq!(shape(&occupied.join("file1")), Some(b"stash\n".to_vec()));
    assert_eq!(
        shape(&dir.path().join("file1")),
        Some(b"squatter\n".to_vec())
    );
}

#[test]
fn restore_backups_sorts_descending_and_stops() {
    let dir = TempDir::new("restore-order").expect("fixture");
    let home = dir.path().join("home");
    let backup = dir.path().join("backup");
    write(&backup, "b/keep", b"kept\n");
    write(
        &backup,
        "manifest",
        format!(
            "{}a/gone\tregular\t1\t2\t600\t3\tv\n../evil\tregular\t1\t2\t600\t3\tv",
            row("b/keep")
        )
        .as_bytes(),
    );
    let recorder = Recorder::new(&[true]);
    assert!(
        plan::restore_backups(
            &backup,
            &home,
            &recorder.matcher(),
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(
        recorder.calls.borrow().as_slice(),
        &[format!(
            "{}|regular|1|2|600|3|value",
            backup.join("b/keep").display()
        )]
    );
    assert_eq!(shape(&home.join("b/keep")), Some(b"kept\n".to_vec()));
    assert_eq!(shape(&backup.join("b/keep")), None);
    assert_eq!(shape(&home.join("evil")), None);
}

fn leftovers(root: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(root)
        .expect("completion root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".completed."))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn publish_completed_publishes_live() {
    for (tag, prior, expected) in [
        ("fresh", None, b"record-body\n".as_slice()),
        (
            "replace",
            Some(b"stale-body\n".as_slice()),
            b"next-body\n".as_slice(),
        ),
    ] {
        let dir = TempDir::new(tag).expect("fixture");
        let record = write(dir.path(), "record", expected);
        let completed = dir.path().join("state/completed");
        if let Some(bytes) = prior {
            write(dir.path(), "state/completed", bytes);
        }
        plan::publish_completed(&record, &completed, &private_dir, &mut MoveCache::default())
            .expect("publish");
        assert_eq!(shape(&completed), Some(expected.to_vec()));
        assert_eq!(mode(&completed), 0o600);
        assert_eq!(mode(completed.parent().expect("root")), 0o700);
        assert_eq!(
            leftovers(completed.parent().expect("root")),
            Vec::<PathBuf>::new()
        );
    }
}

#[test]
fn publish_completed_refuses() {
    for kind in ["directory", "symlink"] {
        let dir = TempDir::new(kind).expect("fixture");
        let record = write(dir.path(), "record", b"record-body\n");
        let completed = dir.path().join("state/completed");
        std::fs::create_dir_all(completed.parent().expect("root")).expect("root");
        if kind == "directory" {
            std::fs::create_dir(&completed).expect("completed dir");
        } else {
            write(dir.path(), "decoy", b"decoy\n");
            std::os::unix::fs::symlink(dir.path().join("decoy"), &completed)
                .expect("completed link");
        }
        assert!(
            plan::publish_completed(&record, &completed, &private_dir, &mut MoveCache::default())
                .is_err()
        );
        let staged = leftovers(completed.parent().expect("root"));
        assert_eq!(staged.len(), 1);
        assert_eq!(shape(&staged[0]), Some(b"record-body\n".to_vec()));
        assert_eq!(mode(&staged[0]), 0o600);
        if kind == "directory" {
            assert!(completed.is_dir());
        } else {
            assert_eq!(
                std::fs::read_link(&completed).expect("link"),
                dir.path().join("decoy")
            );
        }
    }

    let dir = TempDir::new("missing-record").expect("fixture");
    let completed = dir.path().join("state/completed");
    assert!(
        plan::publish_completed(
            &dir.path().join("missing"),
            &completed,
            &private_dir,
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(shape(&completed), None);
    let staged = leftovers(completed.parent().expect("root"));
    assert_eq!(staged.len(), 1);
    assert_eq!(shape(&staged[0]), Some(Vec::new()));

    let dir = TempDir::new("private-refusal").expect("fixture");
    let record = write(dir.path(), "record", b"record\n");
    let completed = dir.path().join("state/completed");
    assert!(
        plan::publish_completed(
            &record,
            &completed,
            &fail_private,
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(shape(completed.parent().expect("root")), None);
}
