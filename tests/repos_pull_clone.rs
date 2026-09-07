//! Native integration tests for staged overlay clones and mode validation.

use dot::log::Log;
use dot::repos_pull_clone::{
    CloneOverlayInputs, clone_overlay_staged, cloned_overlay_matches_commit,
    cloned_overlay_path_modes, normalize_cloned_overlay_modes,
};
use dot::repos_pull_queries::CandidateEnv;
use dot_test_support::TempDir;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

fn git_line(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

fn stage(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

fn mask() -> u32 {
    dot::temp::read_umask().unwrap()
}

struct Repo {
    _dir: TempDir,
    root: PathBuf,
    text: String,
}
impl Repo {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag).unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        stage(&root, "file.txt", b"data\n");
        stage(&root, "sub/nested.txt", b"nested\n");
        std::os::unix::fs::symlink("file.txt", root.join("link")).unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "seed"]);
        Self {
            _dir: dir,
            text: root.to_string_lossy().into_owned(),
            root,
        }
    }
    fn head(&self) -> String {
        git_line(&self.root, &["rev-parse", "HEAD"])
    }
    fn oid(&self, relative: &str) -> String {
        git_line(&self.root, &["hash-object", "--no-filters", "--", relative])
    }
}

#[test]
fn path_modes_validate_content_shape_and_safe_paths() {
    let repo = Repo::new("clone-path-modes");
    let file_oid = repo.oid("file.txt");
    let nested_oid = repo.oid("sub/nested.txt");
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"file.txt")
        .unwrap();
    let link_oid = String::from_utf8(child.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim_end()
        .to_string();
    assert!(cloned_overlay_path_modes(
        &repo.text,
        "file.txt",
        "100644",
        &file_oid,
        mask()
    ));
    assert!(cloned_overlay_path_modes(
        &repo.text,
        "sub/nested.txt",
        "100644",
        &nested_oid,
        mask()
    ));
    assert!(cloned_overlay_path_modes(
        &repo.text,
        "link",
        "120000",
        &link_oid,
        mask()
    ));
    assert!(!cloned_overlay_path_modes(
        &repo.text,
        "../file.txt",
        "100644",
        &file_oid,
        mask()
    ));
    assert!(!cloned_overlay_path_modes(
        &repo.text,
        "sub",
        "100644",
        &nested_oid,
        mask()
    ));
    stage(&repo.root, "file.txt", b"dirty\n");
    assert!(!cloned_overlay_path_modes(
        &repo.text,
        "file.txt",
        "100644",
        &file_oid,
        mask()
    ));
}

#[test]
fn matches_commit_rejects_index_worktree_and_untracked_changes() {
    for kind in ["clean", "index", "worktree", "untracked"] {
        let repo = Repo::new(kind);
        let head = repo.head();
        match kind {
            "index" => {
                stage(&repo.root, "file.txt", b"staged\n");
                git(&repo.root, &["add", "file.txt"]);
            }
            "worktree" => {
                stage(&repo.root, "file.txt", b"dirty\n");
            }
            "untracked" => {
                stage(&repo.root, "extra.txt", b"extra\n");
            }
            _ => {}
        }
        assert_eq!(
            cloned_overlay_matches_commit(&repo.text, &head),
            kind == "clean",
            "{kind}"
        );
    }
}

#[test]
fn normalization_repairs_modes_without_hiding_content_changes() {
    let clean = Repo::new("normalize-mode");
    std::fs::set_permissions(
        clean.root.join("file.txt"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(normalize_cloned_overlay_modes(
        &clean.text,
        &clean.head(),
        mask()
    ));
    assert_eq!(
        std::fs::metadata(clean.root.join("file.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    let dirty = Repo::new("normalize-dirty");
    stage(&dirty.root, "file.txt", b"dirty\n");
    assert!(!normalize_cloned_overlay_modes(
        &dirty.text,
        &dirty.head(),
        mask()
    ));
}

struct CloneFixture {
    _dir: TempDir,
    home: PathBuf,
    origin: PathBuf,
    parent: PathBuf,
    dest: PathBuf,
}
impl CloneFixture {
    fn new(tag: &str, invalid: bool) -> Self {
        let dir = TempDir::new(tag).unwrap();
        let home = dir.path().join("home");
        let origin = dir.path().join("origin");
        let parent = dir.path().join("parent");
        let dest = parent.join("checkout");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        stage(&origin, "base.txt", b"origin\n");
        if invalid {
            stage(&origin, "home/.dotfiles/evil", b"reserved\n");
        }
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-qm", "seed"]);
        Self {
            _dir: dir,
            home,
            origin,
            parent,
            dest,
        }
    }
    fn candidate(&self) -> CandidateEnv {
        let home = self.home.to_string_lossy().into_owned();
        CandidateEnv {
            home: home.clone(),
            checkout: format!("{home}/.local/share/cgraf78/dot"),
            pwd: home.clone(),
            source_root: env!("CARGO_MANIFEST_DIR").to_string(),
            state_home: format!("{home}/.local/state"),
            install_root: format!("{home}/.local/share"),
            provider_state: format!("{home}/.local/state/shdeps"),
            overlay_paths: vec![],
            init_backup: None,
        }
    }
    fn clone(&self, url: &str, mask: u32) -> (bool, Vec<u8>) {
        let candidate = self.candidate();
        let logger = Log::new(false, false);
        let mut moves = dot::temp::MoveCache::default();
        let mut warnings = vec![];
        let ok = clone_overlay_staged(
            &CloneOverlayInputs {
                url,
                path: &self.dest.to_string_lossy(),
                candidate: &candidate,
                mask,
                log: &logger,
            },
            &mut moves,
            &mut warnings,
        );
        (ok, warnings)
    }
    fn leaked_stage(&self) -> bool {
        std::fs::read_dir(&self.parent).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".checkout.clone.")
            })
        })
    }
}

#[test]
fn staged_clone_installs_only_a_valid_clean_candidate() {
    let valid = CloneFixture::new("clone-valid", false);
    let (ok, warnings) = valid.clone(&valid.origin.to_string_lossy(), mask());
    assert!(ok);
    assert!(warnings.is_empty());
    assert_eq!(
        std::fs::read(valid.dest.join("base.txt")).unwrap(),
        b"origin\n"
    );
    assert!(!valid.leaked_stage());
    for (tag, invalid) in [("invalid-tree", true), ("bad-url", false)] {
        let fixture = CloneFixture::new(tag, invalid);
        let url = if invalid {
            fixture.origin.clone()
        } else {
            fixture._dir.path().join("missing")
        };
        assert!(!fixture.clone(&url.to_string_lossy(), mask()).0, "{tag}");
        assert!(!fixture.dest.exists());
        assert!(!fixture.leaked_stage());
    }
}

#[test]
fn staged_clone_does_not_replace_an_existing_destination() {
    let fixture = CloneFixture::new("clone-existing", false);
    stage(&fixture.parent, "checkout", b"user data\n");
    assert!(!fixture.clone(&fixture.origin.to_string_lossy(), mask()).0);
    assert_eq!(std::fs::read(&fixture.dest).unwrap(), b"user data\n");
    assert!(!fixture.leaked_stage());
}

#[cfg(target_os = "linux")]
#[test]
fn staged_clone_canonicalizes_acl_inherited_extension_and_git_modes() {
    let fixture = CloneFixture::new("acl-modes", false);
    stage(
        &fixture.origin,
        "home/.local/lib/dotfiles/merge-hooks.d/overlay.serial.sh",
        b"merge() { :; }\n",
    );
    git(&fixture.origin, &["add", "-A"]);
    git(&fixture.origin, &["commit", "-qm", "extension"]);
    std::fs::create_dir_all(&fixture.parent).unwrap();
    let acl = Command::new("setfacl")
        .args(["-m", "d:u::rwx,d:g::rwx,d:o::rx"])
        .arg(&fixture.parent)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(acl.success(), "test requires default ACL support");
    let (ok, warnings) = fixture.clone(
        &fixture.origin.to_string_lossy(),
        dot::startup::ensure_umask_ceiling(0o002),
    );
    assert!(ok);
    assert!(warnings.is_empty());
    for relative in [
        "home/.local/lib/dotfiles",
        "home/.local/lib/dotfiles/merge-hooks.d",
    ] {
        assert_eq!(
            std::fs::metadata(fixture.dest.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "{relative}"
        );
    }
    for relative in [".git", ".git/objects"] {
        assert_eq!(
            std::fs::metadata(fixture.dest.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "{relative}"
        );
    }
}
