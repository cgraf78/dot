//! Native contracts for the Shdeps lock and installer trust boundary.

use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const INSTALL_SHA256: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ABI: &str = "12";

fn valid_lock() -> Vec<u8> {
    format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}\n").into_bytes()
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn build(tag: &str, lock: Option<&[u8]>) -> Self {
        let dir = TempDir::new(tag).expect("fixture directory");
        let root = dir.path().to_path_buf();
        if let Some(body) = lock {
            std::fs::create_dir_all(root.join("support")).expect("support directory");
            std::fs::write(root.join("support/shdeps.lock"), body).expect("lock fixture");
        }
        Self { _dir: dir, root }
    }
}

#[test]
fn lock_value_requires_the_literal_three_field_contract() {
    let valid = valid_lock();
    type LockRow<'a> = (&'a str, Option<Vec<u8>>, &'a str, Option<&'a str>);
    let rows: Vec<LockRow<'_>> = vec![
        ("revision", Some(valid.clone()), "revision", Some(REVISION)),
        (
            "digest",
            Some(valid.clone()),
            "install_sha256",
            Some(INSTALL_SHA256),
        ),
        ("abi", Some(valid.clone()), "abi", Some(ABI)),
        ("bogus-key", Some(valid.clone()), "bogus", None),
        ("missing", None, "revision", None),
        (
            "two-lines",
            Some(format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\n").into_bytes()),
            "revision",
            None,
        ),
        (
            "four-lines",
            Some(
                format!(
                    "revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}\nextra=1\n"
                )
                .into_bytes(),
            ),
            "revision",
            None,
        ),
        ("empty", Some(Vec::new()), "revision", None),
        (
            "uppercase-revision",
            Some(
                format!(
                    "revision={}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}\n",
                    "A".repeat(40)
                )
                .into_bytes(),
            ),
            "revision",
            None,
        ),
        (
            "short-digest",
            Some(format!("revision={REVISION}\ninstall_sha256=abc\nabi={ABI}\n").into_bytes()),
            "install_sha256",
            None,
        ),
        (
            "zero-abi",
            Some(
                format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi=0\n")
                    .into_bytes(),
            ),
            "abi",
            None,
        ),
        (
            "swapped-order",
            Some(
                format!("abi={ABI}\nrevision={REVISION}\ninstall_sha256={INSTALL_SHA256}\n")
                    .into_bytes(),
            ),
            "revision",
            None,
        ),
        (
            "no-trailing-newline",
            Some(
                format!("revision={REVISION}\ninstall_sha256={INSTALL_SHA256}\nabi={ABI}")
                    .into_bytes(),
            ),
            "abi",
            Some(ABI),
        ),
        (
            "crlf",
            Some(
                format!("revision={REVISION}\r\ninstall_sha256={INSTALL_SHA256}\r\nabi={ABI}\r\n")
                    .into_bytes(),
            ),
            "revision",
            None,
        ),
    ];
    for (tag, lock, key, expected) in rows {
        let fixture = Fixture::build(&format!("shdeps-lock-{tag}"), lock.as_deref());
        assert_eq!(
            dot::shdeps::lock_value(&fixture.root, key).as_deref(),
            expected,
            "{tag}"
        );
    }
}

#[test]
fn origin_allowlist_is_literal_and_case_sensitive() {
    for (origin, expected) in [
        ("https://github.com/cgraf78/shdeps", true),
        ("https://github.com/cgraf78/shdeps.git", true),
        ("git@github.com:cgraf78/shdeps", true),
        ("git@github.com:cgraf78/shdeps.git", true),
        ("ssh://git@github.com/cgraf78/shdeps", true),
        ("ssh://git@github.com/cgraf78/shdeps.git", true),
        ("http://github.com/cgraf78/shdeps", false),
        ("https://github.com/cgraf78/shdeps/", false),
        ("https://github.com/cgraf78/shdeps.git/extra", false),
        ("git@github.com:cgraf78/other.git", false),
        ("HTTPS://github.com/cgraf78/shdeps", false),
        ("", false),
    ] {
        assert_eq!(dot::shdeps::origin_allowed(origin), expected, "{origin:?}");
    }
}

fn stage_mode(root: &Path, name: &str, mode: u32) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, b"owned\n").expect("mode fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

#[test]
fn path_ownership_matches_host_link_modes_and_rejects_unsafe_entries() {
    let home = TempDir::new("shdeps-owned").expect("fixture directory");
    let euid = dot::temp::current_uid().expect("uid");
    let clean = stage_mode(home.path(), "clean", 0o644);
    let locked = stage_mode(home.path(), "locked", 0o600);
    let group = stage_mode(home.path(), "group", 0o664);
    let other = stage_mode(home.path(), "other", 0o602);
    let directory = home.path().join("directory");
    std::fs::create_dir(&directory).expect("directory fixture");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let link_clean = home.path().join("link-clean");
    std::os::unix::fs::symlink(&clean, &link_clean).expect("symlink");
    let link_group = home.path().join("link-group");
    std::os::unix::fs::symlink(&group, &link_group).expect("symlink");
    let dangling = home.path().join("dangling");
    std::os::unix::fs::symlink(home.path().join("nowhere"), &dangling).expect("symlink");
    // BSD `stat -f %Lp` reports owner-only link modes while GNU
    // `stat -c %a` reports 0777. The native predicate deliberately
    // follows that host metadata, like the retired shell engine; the
    // enclosing checkout trust gate rejects links independently.
    let link_owned = cfg!(target_os = "macos");
    for (label, path, expected) in [
        ("clean", clean.clone(), true),
        ("locked", locked, true),
        ("group-writable", group, false),
        ("other-writable", other, false),
        ("missing", home.path().join("missing"), false),
        ("directory", directory, true),
        ("clean symlink", link_clean, link_owned),
        ("writable symlink", link_group, link_owned),
        ("dangling symlink", dangling, link_owned),
    ] {
        assert_eq!(dot::shdeps::path_owned(&path, euid), expected, "{label}");
    }
}

#[test]
fn path_ownership_rejects_a_foreign_uid() {
    let home = TempDir::new("shdeps-owned-foreign").expect("fixture directory");
    let euid = dot::temp::current_uid().expect("uid");
    let clean = stage_mode(home.path(), "clean", 0o644);
    assert!(!dot::shdeps::path_owned(&clean, euid.wrapping_add(1)));
}

#[test]
fn sha256_uses_known_literal_vectors_and_refuses_missing_files() {
    let home = TempDir::new("shdeps-digest").expect("fixture directory");
    for (name, bytes, digest) in [
        (
            "text",
            b"hello shdeps\n".as_slice(),
            "f990fdf034b96b1ce03e80928a7a7bd50889fe3a22c3c8da6011b61396e7ea80",
        ),
        (
            "empty",
            b"".as_slice(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            "binary",
            &[0, 1, 2, 250, 255, 10, 13],
            "111a85dee2c0029ca7b6bd7b61857e7b0936deed0d219ffc1993097ed8d371b6",
        ),
    ] {
        let path = home.path().join(name);
        std::fs::write(&path, bytes).expect("digest fixture");
        assert_eq!(dot::shdeps::sha256_file(&path).as_deref(), Some(digest));
    }
}

#[test]
fn sha256_refuses_a_missing_file() {
    let home = TempDir::new("shdeps-digest-missing").expect("fixture directory");
    assert_eq!(dot::shdeps::sha256_file(&home.path().join("missing")), None);
}

#[test]
fn installer_hash_requires_a_valid_lock_present_file_and_exact_digest() {
    let payload = b"#!/usr/bin/env bash\nprintf 'fixture installer\\n'\n";
    const PAYLOAD_DIGEST: &str = "358129fa0b56b96840dbaad26878bfea8bfc985e23cd2fb782e6effd3722df13";
    let matching = format!("revision={REVISION}\ninstall_sha256={PAYLOAD_DIGEST}\nabi={ABI}\n");
    let mismatch = valid_lock();
    let corrupt = format!("revision={REVISION}\ninstall_sha256={PAYLOAD_DIGEST}\nabi=0\n");
    for (label, lock, staged, expected) in [
        ("match", matching.as_bytes(), true, true),
        ("mismatch", mismatch.as_slice(), true, false),
        ("missing-file", matching.as_bytes(), false, false),
        ("corrupt-lock", corrupt.as_bytes(), true, false),
    ] {
        let fixture = Fixture::build(&format!("shdeps-installer-{label}"), Some(lock));
        let installer = fixture.root.join("install.sh");
        if staged {
            std::fs::write(&installer, payload).expect("installer fixture");
        }
        assert_eq!(
            dot::shdeps::installer_hash_matches(&fixture.root, &installer),
            expected,
            "{label}"
        );
    }
}
