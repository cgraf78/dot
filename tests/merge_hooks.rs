//! Native contracts for merge-hook mechanics.

use dot::{merge_hooks, temp};
use dot_test_support::TempDir;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parents");
    std::fs::write(&path, bytes).expect("write");
    path
}

#[test]
fn hook_paths_and_family_stream_are_literal() {
    let dir = TempDir::new("mh-paths-native").expect("fixture");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home = home.to_str().expect("home utf8");
    assert_eq!(
        merge_hooks::hook_dir("", home).expect("default"),
        Path::new(home).join(".config/dot/merge-hooks.d")
    );
    assert_eq!(
        merge_hooks::hook_dir("/custom/xdg", home).expect("xdg"),
        Path::new("/custom/xdg/dot/merge-hooks.d")
    );
    let root = merge_hooks::hook_dir("", home).expect("root");
    let family = merge_hooks::family(&root, OsStr::new("ssh"));
    let base = stage(&family, "10-base", b"a");
    let extra = stage(&family, "20-extra", b"b");
    let loser = stage(&family, "30-group.replace/10-low", b"low");
    let winner = stage(&family, "30-group.replace/20-winner", b"winner");
    let files = merge_hooks::family_files(&family).expect("files");
    assert_eq!(files, vec![base.clone(), extra, winner.clone()]);
    assert!(
        !files.contains(&loser),
        "replace group must publish only its last winner"
    );
    assert_eq!(
        merge_hooks::family_files_matching(&family, &[b"1*", b"3*"]).expect("matching"),
        vec![base, winner.clone()]
    );
    let rel = merge_hooks::family_relpath(&family, &winner);
    assert_eq!(rel, "30-group.replace/20-winner");
    assert_eq!(
        merge_hooks::family_marker_name(&rel),
        "30-group.replace_20-winner"
    );
    assert_eq!(
        merge_hooks::hook_source(&root, OsStr::new("ssh")),
        root.join("ssh")
    );
}

#[test]
fn expand_home_replaces_only_supported_tokens() {
    let home = "/home/tester";
    for (input, expected) in [
        ("$HOME/.ssh", "/home/tester/.ssh"),
        ("${HOME}/.ssh", "/home/tester/.ssh"),
        ("~", "/home/tester"),
        ("~/doc", "/home/tester/doc"),
        ("~other", "~other"),
        ("/abs", "/abs"),
        ("rel", "rel"),
        ("", ""),
        ("$HOME", "/home/tester"),
        ("${HOME}", "/home/tester"),
        ("~/$HOME", "/home/tester//home/tester"),
        ("$HOME~", "/home/tester~"),
        ("$$HOME", "$/home/tester"),
    ] {
        assert_eq!(merge_hooks::expand_home(input, home), expected, "{input}");
    }
}

#[test]
fn write_text_is_exact_and_skips_an_unchanged_destination() {
    for (label, initial) in [
        ("absent", None),
        ("same", Some("line\n")),
        ("different", Some("old\n")),
    ] {
        let dir = TempDir::new(&format!("mh-write-{label}")).expect("fixture");
        let dst = dir.path().join("out/conf");
        std::fs::create_dir_all(dst.parent().expect("parent")).expect("parent");
        if let Some(body) = initial {
            std::fs::write(&dst, body).expect("initial");
        }
        let before = std::fs::metadata(&dst).ok().and_then(|m| m.modified().ok());
        let mut cache = temp::MoveCache::default();
        let mut warnings = vec![];
        merge_hooks::write_text_if_changed(
            &dst,
            "line",
            &mut merge_hooks::Ctx {
                source_root: dir.path(),
                cache: &mut cache,
                warnings: &mut warnings,
            },
        )
        .expect("write");
        assert_eq!(std::fs::read(&dst).expect("dst"), b"line\n");
        assert!(warnings.is_empty());
        if label == "same" {
            assert_eq!(
                before,
                std::fs::metadata(&dst).ok().and_then(|m| m.modified().ok())
            );
        }
    }
}

#[test]
fn write_text_refuses_a_directory_without_nesting_staged_output() {
    let dir = TempDir::new("mh-write-directory").expect("fixture");
    let dst = dir.path().join("directory");
    std::fs::create_dir(&dst).expect("destination directory");
    let mut cache = temp::MoveCache::default();
    let mut warnings = vec![];
    assert!(
        merge_hooks::write_text_if_changed(
            &dst,
            "unsafe",
            &mut merge_hooks::Ctx {
                source_root: dir.path(),
                cache: &mut cache,
                warnings: &mut warnings,
            },
        )
        .is_err()
    );
    let entries: Vec<_> = std::fs::read_dir(&dst)
        .expect("read destination")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect();
    assert_eq!(entries, Vec::<std::ffi::OsString>::new());
}

#[test]
fn repeated_multiline_text_write_preserves_bytes_and_inode() {
    let dir = TempDir::new("mh-write-multiline").expect("fixture");
    let dst = dir.path().join("generated/config");
    let mut cache = temp::MoveCache::default();
    let mut warnings = vec![];
    let mut context = merge_hooks::Ctx {
        source_root: dir.path(),
        cache: &mut cache,
        warnings: &mut warnings,
    };
    merge_hooks::write_text_if_changed(&dst, "alpha\nbeta", &mut context).expect("first write");
    assert_eq!(
        std::fs::read(&dst).expect("written bytes"),
        b"alpha\nbeta\n"
    );
    let inode = std::fs::metadata(&dst).expect("first metadata").ino();
    merge_hooks::write_text_if_changed(&dst, "alpha\nbeta", &mut context).expect("repeat write");
    assert_eq!(
        std::fs::metadata(&dst).expect("repeat metadata").ino(),
        inode
    );
    assert!(warnings.is_empty());
}

#[test]
fn jq_layer_install_merge_and_failure_contracts_are_literal() {
    let filter = "$s[0] * $d[0]";
    for (label, dst_body, src_body) in [
        ("install", None, b"{\"a\":1}\n".as_slice()),
        ("merge", Some(b"{\"a\":1}\n".as_slice()), b"{\"b\":2}\n"),
        ("corrupt", Some(b"not json\n".as_slice()), b"{\"b\":2}\n"),
        ("empty", Some(b"".as_slice()), b"{\"b\":2}\n"),
        ("bad-src", Some(b"{\"a\":1}\n".as_slice()), b"not json\n"),
    ] {
        let dir = TempDir::new(&format!("mh-jq-{label}")).expect("fixture");
        let src = stage(dir.path(), "src.json", src_body);
        let dst = dir.path().join("dst.json");
        if let Some(body) = dst_body {
            std::fs::write(&dst, body).expect("dst");
        }
        let original = std::fs::read(&dst).ok();
        let mut cache = temp::MoveCache::default();
        let mut warnings = vec![];
        merge_hooks::jq_layer(
            label,
            &src,
            &dst,
            filter,
            &mut merge_hooks::Ctx {
                source_root: dir.path(),
                cache: &mut cache,
                warnings: &mut warnings,
            },
        )
        .expect("layer outcome");
        let actual = std::fs::read(&dst).ok();
        if merge_hooks::jq_available() {
            match label {
                "install" => {
                    assert_eq!(actual, Some(b"{\n  \"a\": 1\n}\n".to_vec()));
                    assert!(warnings.is_empty());
                }
                "merge" => {
                    assert_eq!(actual, Some(b"{\n  \"a\": 1,\n  \"b\": 2\n}\n".to_vec()));
                    assert!(warnings.is_empty());
                }
                "corrupt" | "empty" => {
                    assert_eq!(actual, Some(b"{\n  \"b\": 2\n}\n".to_vec()));
                    assert_eq!(warnings.len(), 1);
                    assert!(warnings[0].contains("corrupt"));
                    assert!(warnings[0].contains("rebuilding"));
                }
                "bad-src" => {
                    assert_eq!(actual, original, "failed merge preserves destination");
                    assert!(
                        warnings
                            .iter()
                            .any(|line| line.contains("bad-src merge failed — skipping"))
                    );
                }
                _ => unreachable!(),
            }
        } else {
            assert_eq!(actual, None, "jq-free copy or rebuild installs nothing");
            assert!(
                warnings
                    .iter()
                    .any(|line| line.contains(&format!("{label} copy failed — skipping")))
            );
            if dst_body.is_some() {
                assert!(
                    warnings
                        .iter()
                        .any(|line| line.contains("corrupt") && line.contains("rebuilding"))
                );
            }
        }
    }
    let dir = TempDir::new("mh-jq-filter").expect("fixture");
    let src = stage(dir.path(), "src", b"{}\n");
    let dst = stage(dir.path(), "dst", b"{}\n");
    let mut cache = temp::MoveCache::default();
    let mut warnings = vec![];
    merge_hooks::jq_layer(
        "bad-filter",
        &src,
        &dst,
        "?!",
        &mut merge_hooks::Ctx {
            source_root: dir.path(),
            cache: &mut cache,
            warnings: &mut warnings,
        },
    )
    .expect("skip failure");
    if merge_hooks::jq_available() {
        assert_eq!(std::fs::read(&dst).expect("preserved destination"), b"{}\n");
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("bad-filter merge failed — skipping"))
        );
    } else {
        assert!(!dst.exists(), "jq-free failed rebuild removes destination");
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("bad-filter copy failed — skipping"))
        );
    }
}

#[test]
fn jq_availability_reflects_the_executable_path() {
    let available = merge_hooks::jq_available();
    let from_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let jq = dir.join("jq");
            std::fs::metadata(jq)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    });
    assert_eq!(available, from_path);
}
