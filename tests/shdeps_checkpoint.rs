//! Native filesystem and Git contracts for the Shdeps re-exec checkpoint.

use dot_test_support::TempDir;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const R40A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const R40B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const R64C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn git_command() -> Command {
    let mut command = Command::new(dot_test_support::real_tool("git"));
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    dot_test_support::isolate_git(&mut command).args([
        "-c",
        "user.name=Dot Test",
        "-c",
        "user.email=dot-test@example.invalid",
    ]);
    command
}

fn git(repo: &Path, args: &[&str]) {
    let status = git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run Git fixture command");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn init_repo(root: &Path) -> String {
    git(root, &["init", "-q", "--template="]);
    std::fs::write(root.join("file.txt"), b"checkpoint fixture\n").expect("seed file");
    git(root, &["add", "--", "file.txt"]);
    git(root, &["commit", "-qm", "seed"]);
    let output = git_command()
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("read HEAD");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("ASCII revision")
        .trim_end()
        .to_owned()
}

fn record(before: &str, after: &str) -> Vec<u8> {
    format!("cgraf78 dot provider reexec checkpoint v1\nbefore={before}\nafter={after}\n")
        .into_bytes()
}

fn stage_mode(root: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture parents");
    // An executable fixture is created by a child so this process never holds
    // a writer to it (see `install_fixture_executable`).
    if mode & 0o111 != 0 {
        dot_test_support::install_fixture_executable(&path, bytes, mode).expect("write fixture");
    } else {
        std::fs::write(&path, bytes).expect("write fixture");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
    path
}

#[test]
fn revision_gate_accepts_only_forty_to_sixty_four_hex_digits() {
    let mixed40 = "aAbB00112233445566778899ccDDeeFF00112233";
    assert_eq!(mixed40.len(), 40);
    for (revision, expected) in [
        (R40A.to_owned(), true),
        (R40A.to_ascii_uppercase(), true),
        (mixed40.to_owned(), true),
        (R64C.to_owned(), true),
        (R64C.to_ascii_uppercase(), true),
        ("a".repeat(41), true),
        ("b".repeat(63), true),
        ("a".repeat(39), false),
        ("a".repeat(65), false),
        (String::new(), false),
        (format!("{}g", "a".repeat(39)), false),
        ("G".repeat(40), false),
        (format!("0x{}", "a".repeat(38)), false),
        (" ".repeat(40), false),
        (format!("{R40A} "), false),
    ] {
        assert_eq!(
            dot::shdeps::revision_valid(&revision),
            expected,
            "{revision:?}"
        );
    }
}

#[test]
fn checkpoint_path_uses_only_absolute_xdg_or_home_roots() {
    let home = TempDir::new("shdeps-checkpoint-path").expect("fixture directory");
    let home_text = home.path().to_string_lossy();
    assert_eq!(
        dot::shdeps::checkpoint_path("", &home_text),
        Some(home.path().join(".local/state/dot/provider-reexec-failed"))
    );
    assert_eq!(
        dot::shdeps::checkpoint_path("/srv/state", &home_text),
        Some(PathBuf::from("/srv/state/dot/provider-reexec-failed"))
    );
    assert_eq!(
        dot::shdeps::checkpoint_path("/", &home_text),
        Some(PathBuf::from("/dot/provider-reexec-failed"))
    );
    assert_eq!(
        dot::shdeps::checkpoint_path("relative", &home_text),
        Some(home.path().join(".local/state/dot/provider-reexec-failed"))
    );
    assert_eq!(
        dot::shdeps::checkpoint_path("", "/"),
        Some(PathBuf::from("/.local/state/dot/provider-reexec-failed"))
    );
    assert_eq!(dot::shdeps::checkpoint_path("", "relative"), None);
}

#[test]
fn active_revision_is_exact_for_a_checkout_and_empty_elsewhere() {
    let repo = TempDir::new("shdeps-active-repo").expect("fixture directory");
    let head = init_repo(repo.path());
    assert_eq!(dot::shdeps::active_revision(repo.path()), head);
    let plain = TempDir::new("shdeps-active-plain").expect("fixture directory");
    assert_eq!(dot::shdeps::active_revision(plain.path()), "");
    assert_eq!(
        dot::shdeps::active_revision(&plain.path().join("missing")),
        ""
    );
    let file = stage_mode(plain.path(), "file.txt", b"not a repo\n", 0o644);
    assert_eq!(dot::shdeps::active_revision(&file), "");
}

fn write_metadata(root: &Path, body: &str) {
    std::fs::write(root.join(".dot-install.json"), body).expect("release metadata");
}

/// Release metadata in the packaged one-key-per-line layout.
fn metadata(commit: &str) -> String {
    format!("{{\n  \"schema\": 1,\n  \"commit\": \"{commit}\",\n  \"repo\": \"cgraf78/dot\"\n}}\n")
}

#[test]
fn installed_revision_reads_a_release_root_from_its_metadata() {
    let release = TempDir::new("shdeps-installed-release").expect("fixture directory");
    write_metadata(release.path(), &metadata(R40A));
    // Unlike the startup identity, this is not bound to the running build:
    // after an upgrade the metadata names the newly installed release.
    assert_ne!(R40A, dot::version::COMMIT);
    assert_eq!(dot::shdeps::installed_revision(release.path()), R40A);
}

#[test]
fn installed_revision_never_asks_git_inside_a_release_root() {
    // A release root under some other repository (a `$HOME` checkout, say)
    // must not report that repository's HEAD: Git walks up from a root that
    // has no `.git` of its own.
    let repo = TempDir::new("shdeps-installed-enclosing").expect("fixture directory");
    let head = init_repo(repo.path());
    let release = repo.path().join("share/cgraf78/dot");
    std::fs::create_dir_all(&release).expect("nested release root");
    assert_eq!(dot::shdeps::active_revision(&release), head);
    write_metadata(&release, &metadata(R40B));
    assert_eq!(dot::shdeps::installed_revision(&release), R40B);
    // Ambiguous metadata is unreadable, not a reason to fall back to Git.
    write_metadata(
        &release,
        &format!("\"commit\": \"{R40A}\",\n\"commit\": \"{R40B}\"\n"),
    );
    assert_eq!(dot::shdeps::installed_revision(&release), "");
    write_metadata(&release, "{\"schema\": 1}\n");
    assert_eq!(dot::shdeps::installed_revision(&release), "");
}

#[test]
fn installed_revision_keeps_git_for_a_checkout() {
    let repo = TempDir::new("shdeps-installed-checkout").expect("fixture directory");
    let head = init_repo(repo.path());
    assert_eq!(dot::shdeps::installed_revision(repo.path()), head);
    // A symlinked metadata file does not make a checkout a release root.
    let elsewhere = TempDir::new("shdeps-installed-link").expect("fixture directory");
    write_metadata(elsewhere.path(), &metadata(R40A));
    std::os::unix::fs::symlink(
        elsewhere.path().join(".dot-install.json"),
        repo.path().join(".dot-install.json"),
    )
    .expect("metadata link");
    assert_eq!(dot::shdeps::installed_revision(repo.path()), head);
}

enum ReadStage {
    File(Vec<u8>, u32),
    Link,
    Hardlink,
    Dangling,
    Directory,
    Missing,
}

fn read_case(label: &str, stage: ReadStage, expected: Option<&str>) {
    let home = TempDir::new(&format!("shdeps-checkpoint-read-{label}")).expect("fixture directory");
    let target = home.path().join("checkpoint");
    match stage {
        ReadStage::File(bytes, mode) => {
            stage_mode(home.path(), "checkpoint", &bytes, mode);
        }
        ReadStage::Link => {
            let valid = stage_mode(home.path(), "valid", &record(R40A, R40B), 0o600);
            std::os::unix::fs::symlink(valid, &target).expect("symlink");
        }
        ReadStage::Hardlink => {
            let valid = stage_mode(home.path(), "valid", &record(R40A, R40B), 0o600);
            std::fs::hard_link(valid, &target).expect("hardlink");
        }
        ReadStage::Dangling => {
            std::os::unix::fs::symlink(home.path().join("missing"), &target).expect("symlink");
        }
        ReadStage::Directory => {
            std::fs::create_dir(&target).expect("directory fixture");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }
        ReadStage::Missing => {}
    }
    assert_eq!(
        dot::shdeps::read_checkpoint(&target).as_deref(),
        expected,
        "{label}"
    );
}

#[test]
fn checkpoint_reader_enforces_framing_identity_mode_size_and_links() {
    let upper_a = R40A.to_ascii_uppercase();
    let upper_b = R40B.to_ascii_uppercase();
    let mixed64 = "aAbB00112233445566778899ccDDeeFF00112233445566778899aAbB00112233";
    let valid = record(R40A, R40B);
    let mut oversized = valid.clone();
    oversized.extend(std::iter::repeat_n(b'x', 600));
    let rows = vec![
        ("valid", ReadStage::File(valid.clone(), 0o600), Some(R40B)),
        ("uppercase", ReadStage::File(record(&upper_a, &upper_b), 0o600), Some(R40B)),
        ("mixed64", ReadStage::File(record(mixed64, R64C), 0o600), Some(R64C)),
        ("no-newline", ReadStage::File(valid[..valid.len() - 1].to_vec(), 0o600), Some(R40B)),
        ("crlf", ReadStage::File(format!("cgraf78 dot provider reexec checkpoint v1\r\nbefore={R40A}\r\nafter={R40B}\r\n").into_bytes(), 0o600), None),
        ("bad-magic", ReadStage::File(format!("wrong\nbefore={R40A}\nafter={R40B}\n").into_bytes(), 0o600), None),
        ("swapped", ReadStage::File(format!("cgraf78 dot provider reexec checkpoint v1\nafter={R40B}\nbefore={R40A}\n").into_bytes(), 0o600), None),
        ("two-lines", ReadStage::File(format!("cgraf78 dot provider reexec checkpoint v1\nbefore={R40A}\n").into_bytes(), 0o600), None),
        ("duplicate", ReadStage::File(format!("cgraf78 dot provider reexec checkpoint v1\nbefore={R40A}\nbefore={R40A}\nafter={R40B}\n").into_bytes(), 0o600), None),
        ("empty", ReadStage::File(Vec::new(), 0o600), None),
        ("same", ReadStage::File(record(R40A, R40A), 0o600), None),
        ("same-different-case", ReadStage::File(record(&upper_a, R40A), 0o600), Some(R40A)),
        ("bad-after", ReadStage::File(record(R40A, "xyz"), 0o600), None),
        ("bad-before", ReadStage::File(record("0", R40B), 0o600), None),
        ("empty-before", ReadStage::File(record("", R40B), 0o600), None),
        ("equals-in-value", ReadStage::File(record("ab=ab", R40B), 0o600), None),
        ("mode-644", ReadStage::File(valid.clone(), 0o644), None),
        ("mode-400", ReadStage::File(valid.clone(), 0o400), None),
        ("mode-660", ReadStage::File(valid.clone(), 0o660), None),
        ("oversized", ReadStage::File(oversized, 0o600), None),
        ("directory", ReadStage::Directory, None),
        ("link", ReadStage::Link, None),
        ("hardlink", ReadStage::Hardlink, None),
        ("dangling", ReadStage::Dangling, None),
        ("missing", ReadStage::Missing, None),
    ];
    for (label, stage, expected) in rows {
        read_case(label, stage, expected);
    }
}

#[derive(Clone)]
enum WritePre {
    File(Vec<u8>, u32),
    Link,
    Directory,
}

fn stray_temps(parent: &Path) -> usize {
    std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count()
}

fn write_case(label: &str, before: &str, after: &str, pre: Option<WritePre>, expected: bool) {
    let home =
        TempDir::new(&format!("shdeps-checkpoint-write-{label}")).expect("fixture directory");
    let target = home.path().join(".local/state/dot/provider-reexec-failed");
    match pre.clone() {
        Some(WritePre::File(bytes, mode)) => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                &bytes,
                mode,
            );
        }
        Some(WritePre::Link) => {
            let valid = stage_mode(home.path(), "valid", &record(R40A, R40B), 0o600);
            std::fs::create_dir_all(target.parent().unwrap()).expect("checkpoint parent");
            std::os::unix::fs::symlink(valid, &target).expect("symlink");
        }
        Some(WritePre::Directory) => {
            std::fs::create_dir_all(&target).expect("directory fixture");
        }
        None => {}
    }
    let mut moves = dot::temp::MoveCache::default();
    assert_eq!(
        dot::shdeps::write_checkpoint(before, after, &target, &mut moves),
        expected,
        "{label}"
    );
    if expected {
        assert_eq!(
            std::fs::read(&target).expect("checkpoint bytes"),
            record(&before.to_ascii_lowercase(), &after.to_ascii_lowercase())
        );
        assert_eq!(
            std::fs::symlink_metadata(&target).unwrap().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            std::fs::symlink_metadata(target.parent().unwrap())
                .unwrap()
                .mode()
                & 0o7777,
            0o700
        );
    } else {
        match pre {
            Some(WritePre::File(bytes, mode)) => {
                assert_eq!(std::fs::read(&target).unwrap(), bytes);
                assert_eq!(
                    std::fs::symlink_metadata(&target).unwrap().mode() & 0o7777,
                    mode
                );
            }
            Some(WritePre::Link) => {
                assert!(target.symlink_metadata().unwrap().file_type().is_symlink())
            }
            Some(WritePre::Directory) => assert!(target.is_dir()),
            None => assert!(target.symlink_metadata().is_err()),
        }
    }
    assert_eq!(target.parent().map(stray_temps).unwrap_or(0), 0, "{label}");
}

#[test]
fn checkpoint_writer_is_no_replace_lowercase_atomic_and_temp_clean() {
    let upper_a = R40A.to_ascii_uppercase();
    let upper_b = R40B.to_ascii_uppercase();
    let valid = record(R40A, R40B);
    let rows = vec![
        ("fresh", R40A.to_owned(), R40B.to_owned(), None, true),
        ("uppercase", upper_a.clone(), upper_b, None, true),
        (
            "mixed64",
            "aAbB00112233445566778899ccDDeeFF00112233".to_owned(),
            R64C.to_owned(),
            None,
            true,
        ),
        ("same", R40A.to_owned(), R40A.to_owned(), None, false),
        ("same-case-folded", upper_a, R40A.to_owned(), None, false),
        ("bad-before", "xyz".to_owned(), R40B.to_owned(), None, false),
        ("bad-after", R40A.to_owned(), String::new(), None, false),
        (
            "occupied-file",
            R40A.to_owned(),
            R40B.to_owned(),
            Some(WritePre::File(valid, 0o600)),
            false,
        ),
        (
            "occupied-empty",
            R40A.to_owned(),
            R40B.to_owned(),
            Some(WritePre::File(Vec::new(), 0o644)),
            false,
        ),
        (
            "occupied-link",
            R40A.to_owned(),
            R40B.to_owned(),
            Some(WritePre::Link),
            false,
        ),
        (
            "occupied-directory",
            R40A.to_owned(),
            R40B.to_owned(),
            Some(WritePre::Directory),
            false,
        ),
    ];
    for (label, before, after, pre, expected) in rows {
        write_case(label, &before, &after, pre, expected);
    }
}

#[derive(Clone, Copy)]
enum ConsumeKind {
    Absent,
    Match,
    UpperMatch,
    Mismatch,
    Malformed,
    Same,
    Mode644,
    Link,
    Dangling,
    Detached,
}

fn consume_case(label: &str, kind: ConsumeKind, expected: bool, kept: bool) {
    let home =
        TempDir::new(&format!("shdeps-checkpoint-consume-{label}")).expect("fixture directory");
    let plain =
        TempDir::new(&format!("shdeps-checkpoint-plain-{label}")).expect("fixture directory");
    let head = init_repo(home.path());
    let target = home.path().join(".local/state/dot/provider-reexec-failed");
    let source = if matches!(kind, ConsumeKind::Detached) {
        plain.path()
    } else {
        home.path()
    };
    match kind {
        ConsumeKind::Absent => {}
        ConsumeKind::Match | ConsumeKind::Detached => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                &record(R40A, &head),
                0o600,
            );
        }
        ConsumeKind::UpperMatch => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                &record(R40A, &head.to_ascii_uppercase()),
                0o600,
            );
        }
        ConsumeKind::Mismatch => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                &record(R40A, R40B),
                0o600,
            );
        }
        ConsumeKind::Malformed => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                b"not a checkpoint\n",
                0o600,
            );
        }
        ConsumeKind::Same => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                &record(&head, &head),
                0o600,
            );
        }
        ConsumeKind::Mode644 => {
            stage_mode(
                home.path(),
                ".local/state/dot/provider-reexec-failed",
                &record(R40A, &head),
                0o644,
            );
        }
        ConsumeKind::Link => {
            let valid = stage_mode(home.path(), "valid", &record(R40A, &head), 0o600);
            std::fs::create_dir_all(target.parent().unwrap()).expect("checkpoint parent");
            std::os::unix::fs::symlink(valid, &target).expect("symlink");
        }
        ConsumeKind::Dangling => {
            std::fs::create_dir_all(target.parent().unwrap()).expect("checkpoint parent");
            std::os::unix::fs::symlink(home.path().join("missing"), &target).expect("symlink");
        }
    }
    // `dot doctor` predicts the consumer without touching the record: the
    // states it reports as harmless are exactly the ones `dot update`
    // proceeds past, and every refusal is reported as blocking.
    let present = target.symlink_metadata().is_ok();
    let predicted = dot::shdeps::checkpoint_state(&target, source);
    assert_eq!(
        target.symlink_metadata().is_ok(),
        present,
        "{label} prediction must not consume"
    );
    assert_eq!(
        matches!(
            predicted,
            dot::shdeps::CheckpointState::Absent | dot::shdeps::CheckpointState::Pending
        ),
        expected,
        "{label} prediction: {predicted:?}"
    );
    assert_eq!(
        predicted == dot::shdeps::CheckpointState::Absent,
        !present,
        "{label} absence: {predicted:?}"
    );
    assert_eq!(
        dot::shdeps::consume_checkpoint(&target, source),
        expected,
        "{label}"
    );
    assert_eq!(target.symlink_metadata().is_ok(), kept, "{label} survival");
}

#[test]
fn checkpoint_state_names_the_pinned_and_active_revisions() {
    let home = TempDir::new("shdeps-checkpoint-state").expect("fixture directory");
    let head = init_repo(home.path());
    let target = stage_mode(
        home.path(),
        ".local/state/dot/provider-reexec-failed",
        &record(R40A, &R40B.to_ascii_uppercase()),
        0o600,
    );
    assert_eq!(
        dot::shdeps::checkpoint_state(&target, home.path()),
        dot::shdeps::CheckpointState::Mismatch {
            pinned: R40B.to_owned(),
            active: head,
        }
    );
    assert_eq!(
        dot::shdeps::checkpoint_in(Path::new("/srv/state")),
        PathBuf::from("/srv/state/dot/provider-reexec-failed")
    );
}

#[test]
fn checkpoint_consumer_removes_only_a_valid_active_generation() {
    for (label, kind, expected, kept) in [
        ("absent", ConsumeKind::Absent, true, false),
        ("match", ConsumeKind::Match, true, false),
        ("uppercase-match", ConsumeKind::UpperMatch, true, false),
        ("mismatch", ConsumeKind::Mismatch, false, true),
        ("malformed", ConsumeKind::Malformed, false, true),
        ("same", ConsumeKind::Same, false, true),
        ("mode-644", ConsumeKind::Mode644, false, true),
        ("link", ConsumeKind::Link, false, true),
        ("dangling", ConsumeKind::Dangling, false, true),
        ("detached", ConsumeKind::Detached, false, true),
    ] {
        consume_case(label, kind, expected, kept);
    }
}
