//! Native security contracts for extension files and retiring overlays.

use dot::extension_trust::{self as trust, Inputs};
use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
fn dir(root: &Path, name: &str, mode: u32) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    chmod(&path, mode);
    path
}
fn file(root: &Path, name: &str, bytes: &[u8], mode: u32) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    chmod(&path, mode);
    path
}
fn isolated_git() -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ]);
    command
}
fn git_repo(path: &Path, origin: &str) {
    dir(
        path.parent().unwrap(),
        path.file_name().unwrap().to_str().unwrap(),
        0o700,
    );
    assert!(
        isolated_git()
            .arg("init")
            .arg("-q")
            .arg("--template=")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        isolated_git()
            .arg("-C")
            .arg(path)
            .args(["remote", "add", "origin", origin])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}
fn inputs(home: &Path) -> Inputs {
    let h = home.to_string_lossy().into_owned();
    Inputs {
        euid: euid(),
        home: h.clone(),
        extensions_dir: format!("{h}/ext"),
        manifest: String::new(),
        retiring_root: String::new(),
    }
}

#[test]
fn stat_root_and_component_walk_reject_writable_links_and_bad_shapes() {
    let fixture = TempDir::new("trust-stat").unwrap();
    let root = dir(fixture.path(), "ext", 0o700);
    let root_s = root.to_string_lossy();
    let good = file(&root, "good/tool", b"x", 0o644);
    assert!(trust::file_stat(&good, euid()));
    assert!(trust::directory_stat(&root, euid()));
    assert!(trust::root_validate(&root_s, euid()));
    assert!(trust::parent_components_validate(&good, &root_s, euid()));
    chmod(&good, 0o666);
    assert!(!trust::file_stat(&good, euid()));
    chmod(&good, 0o644);
    std::fs::hard_link(&good, root.join("hard")).unwrap();
    assert!(!trust::file_stat(&good, euid()));
    for bad in [
        "",
        "/",
        "relative",
        "/tmp/",
        "/tmp//x",
        "/tmp/./x",
        "/tmp/../x",
        "/tmp\nx",
    ] {
        assert!(!trust::root_validate(bad, euid()));
    }
    chmod(&root.join("good"), 0o722);
    assert!(!trust::parent_components_validate(&good, &root_s, euid()));
    std::os::unix::fs::symlink(&root, fixture.path().join("ext-link")).unwrap();
    assert!(!trust::root_validate(
        &fixture.path().join("ext-link").to_string_lossy(),
        euid()
    ));
}

fn authorized(home: &Path) -> (Inputs, String, std::path::PathBuf) {
    let mut input = inputs(home);
    let ext = dir(home, "ext", 0o700);
    input.extensions_dir = ext.to_string_lossy().into();
    let checkout = home.join(".dotfiles-web");
    git_repo(&checkout, "file:///repo/web.git");
    file(&checkout, "home/ext/tool", b"tool\n", 0o644);
    let manifest = file(home, "manifest", b"ext/tool\tweb\n", 0o644);
    input.manifest = manifest.to_string_lossy().into();
    let link = ext.join("tool");
    std::os::unix::fs::symlink("../.dotfiles-web/home/ext/tool", &link).unwrap();
    let h = home.to_string_lossy();
    let record =
        format!("web|{h}/.dotfiles-web|file:///repo/web.git|{h}/conf/10-web.conf|false|git");
    (input, record, link)
}

#[test]
fn symlink_authority_binds_manifest_owner_target_checkout_and_origin() {
    let fixture = TempDir::new("trust-link").unwrap();
    let home = fixture.path();
    let (input, record, link) = authorized(home);
    assert!(trust::symlink_authorized(
        &link,
        &input.home,
        &input.manifest,
        std::slice::from_ref(&record),
        euid()
    ));
    for bad in [
        record.replace("web|", "ghost|"),
        record.replace("web.git", "other.git"),
        record.replace("|false|git", "|false|none"),
    ] {
        assert!(!trust::symlink_authorized(
            &link,
            &input.home,
            &input.manifest,
            &[bad],
            euid()
        ));
    }
    std::fs::write(&input.manifest, b"other\tweb\n").unwrap();
    assert!(!trust::symlink_authorized(
        &link,
        &input.home,
        &input.manifest,
        &[record],
        euid()
    ));
}

#[test]
fn symlink_authority_rejects_inactive_writable_and_outside_implementations() {
    let fixture = TempDir::new("trust-link-boundaries").unwrap();
    let home = fixture.path();
    let (input, record, link) = authorized(home);
    assert!(!trust::symlink_authorized(
        &link,
        &input.home,
        &input.manifest,
        &[],
        euid()
    ));

    let implementation_parent = home.join(".dotfiles-web/home/ext");
    chmod(&implementation_parent, 0o775);
    assert!(!trust::symlink_authorized(
        &link,
        &input.home,
        &input.manifest,
        std::slice::from_ref(&record),
        euid()
    ));
    chmod(&implementation_parent, 0o700);

    let outside = file(home, "outside.sh", b"merge() { :; }\n", 0o644);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    std::fs::write(
        &input.manifest,
        format!("ext/tool\tweb\t{}\n", outside.display()),
    )
    .unwrap();
    assert!(!trust::symlink_authorized(
        &link,
        &input.home,
        &input.manifest,
        &[record],
        euid()
    ));
}

#[test]
fn file_and_directory_validation_revalidate_every_entry() {
    let fixture = TempDir::new("trust-entry").unwrap();
    let home = fixture.path();
    let (input, record, link) = authorized(home);
    assert!(trust::file_validate(
        &link,
        &input,
        std::slice::from_ref(&record)
    ));
    let plain = file(Path::new(&input.extensions_dir), "plain", b"x", 0o644);
    assert!(trust::file_validate(&plain, &input, &[]));
    chmod(&plain, 0o666);
    assert!(!trust::file_validate(&plain, &input, &[]));
    let clean = dir(Path::new(&input.extensions_dir), "clean/sub", 0o700);
    assert!(trust::directory_validate(
        &clean,
        &input.extensions_dir,
        euid()
    ));
    let open = dir(Path::new(&input.extensions_dir), "open", 0o755);
    assert!(trust::directory_validate(
        &open,
        &input.extensions_dir,
        euid()
    ));
    chmod(&open, 0o777);
    assert!(!trust::directory_validate(
        &open,
        &input.extensions_dir,
        euid()
    ));
    std::os::unix::fs::symlink("clean", Path::new(&input.extensions_dir).join("dir-link")).unwrap();
    assert!(!trust::directory_validate(
        &Path::new(&input.extensions_dir).join("dir-link"),
        &input.extensions_dir,
        euid()
    ));
}

#[test]
fn deactivation_requires_fixed_regular_script_and_saved_git_identity() {
    let fixture = TempDir::new("trust-deactivate").unwrap();
    let home = fixture.path();
    let checkout = home.join(".dotfiles-web");
    git_repo(&checkout, "file:///repo/web.git");
    let script = file(&checkout, "dot/profile-deactivate", b"#!/bin/sh\n", 0o600);
    let h = home.to_string_lossy();
    let record =
        format!("web|{h}/.dotfiles-web|file:///repo/web.git|{h}/conf/10-web.conf|false|git");
    let script_s = script.to_string_lossy();
    assert!(trust::deactivation_validate(&record, &script_s, &h, euid()).is_ok());
    for bad in [
        record.replace("|false|git", "|false|none"),
        record.replace("web.git", "other.git"),
        record.replace(".dotfiles-web|", ".dotfiles-ghost|"),
    ] {
        assert!(trust::deactivation_validate(&bad, &script_s, &h, euid()).is_err());
    }
    assert!(trust::deactivation_validate(&record, &format!("{h}/elsewhere"), &h, euid()).is_err());

    let real = checkout.join("dot/profile-deactivate.real");
    std::fs::rename(&script, &real).unwrap();
    std::os::unix::fs::symlink("profile-deactivate.real", &script).unwrap();
    assert!(trust::deactivation_validate(&record, &script_s, &h, euid()).is_err());
    std::fs::remove_file(&script).unwrap();
    std::fs::rename(&real, &script).unwrap();

    let hardlink = checkout.join("dot/profile-deactivate.hardlink");
    if std::fs::hard_link(&script, &hardlink).is_ok() {
        assert!(trust::deactivation_validate(&record, &script_s, &h, euid()).is_err());
        std::fs::remove_file(hardlink).unwrap();
    }
    chmod(&script, 0o620);
    assert!(trust::deactivation_validate(&record, &script_s, &h, euid()).is_err());
    chmod(&script, 0o600);
    chmod(&checkout.join("dot"), 0o777);
    assert!(trust::deactivation_validate(&record, &script_s, &h, euid()).is_err());
}

#[test]
fn retiring_file_distinguishes_usage_from_refusal() {
    let fixture = TempDir::new("trust-retiring").unwrap();
    let root = dir(fixture.path(), "retiring", 0o700);
    let good = file(&root, "support/deep.conf", b"ok", 0o644);
    let mut input = inputs(fixture.path());
    input.retiring_root = root.to_string_lossy().into();
    assert_eq!(
        trust::retiring_overlay_file("support/deep.conf", &input).unwrap(),
        good.to_string_lossy()
    );
    for malformed in [
        "", "/abs", ".", "..", "./x", "../x", "a/./b", "a/../b", "a/", "a//b",
    ] {
        assert_eq!(
            trust::retiring_overlay_file(malformed, &input),
            Err(trust::Error::Usage)
        );
    }
    for refused in ["missing", "support"] {
        assert_eq!(
            trust::retiring_overlay_file(refused, &input),
            Err(trust::Error::Refused)
        );
    }

    let real = root.join("support/deep.real");
    std::fs::rename(&good, &real).unwrap();
    std::os::unix::fs::symlink("deep.real", &good).unwrap();
    assert_eq!(
        trust::retiring_overlay_file("support/deep.conf", &input),
        Err(trust::Error::Refused)
    );
    std::fs::remove_file(&good).unwrap();
    std::fs::rename(&real, &good).unwrap();
    let hardlink = root.join("support/deep.hardlink");
    if std::fs::hard_link(&good, &hardlink).is_ok() {
        assert_eq!(
            trust::retiring_overlay_file("support/deep.conf", &input),
            Err(trust::Error::Refused)
        );
        std::fs::remove_file(hardlink).unwrap();
    }
    chmod(&root.join("support"), 0o777);
    assert_eq!(
        trust::retiring_overlay_file("support/deep.conf", &input),
        Err(trust::Error::Refused)
    );
}
