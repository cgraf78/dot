//! Differential parity tests for marked-block merging against
//! `lib/dot/merge-block.sh`: block assembly, stripping (single and
//! family), atomic finalize, and both merge flavors — including
//! modeline corners, unterminated blocks, and idempotent re-merges.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::merge_block;
use dot::temp;
use dot::test_support::TempDir;

/// Run one shell snippet with `merge-block.sh` sourced. `argv`
/// arrives as `$2..` (`$1` is the repo root, byte-exact).
fn shell_run(fixture: &Path, argv: &[&std::ffi::OsStr], snippet: &str) -> (i32, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/temp.sh\"\n. \"$1/lib/dot/merge-block.sh\"\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", fixture)
        .current_dir(fixture)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = cmd.output().expect("spawn bash");
    (output.status.code().unwrap_or(99), output.stdout)
}

/// Fresh publishing context over `root` (git digests run under it).
fn ctx<'a>(root: &'a Path, cache: &'a mut temp::MoveCache) -> merge_block::Ctx<'a> {
    merge_block::Ctx {
        source_root: root,
        cache,
    }
}

/// Snapshot a fixture tree: relative path to kind, mode, and payload.
#[derive(Debug, PartialEq)]
struct Snap {
    kind: char,
    mode: u32,
    payload: Vec<u8>,
}

fn snapshot(root: &Path) -> BTreeMap<String, Snap> {
    let mut map = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .expect("list fixture")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let rel = path
                .strip_prefix(root)
                .expect("fixture prefix")
                .as_os_str()
                .as_bytes()
                .to_vec();
            let key = String::from_utf8_lossy(&rel).into_owned();
            let meta = std::fs::symlink_metadata(&path).expect("stat fixture");
            let mode = meta.permissions().mode() & 0o7777;
            if meta.file_type().is_symlink() {
                map.insert(
                    key,
                    Snap {
                        kind: 'l',
                        mode,
                        payload: std::fs::read_link(&path)
                            .expect("read link")
                            .as_os_str()
                            .as_bytes()
                            .to_vec(),
                    },
                );
            } else if meta.is_dir() {
                map.insert(
                    key,
                    Snap {
                        kind: 'd',
                        mode,
                        payload: Vec::new(),
                    },
                );
                stack.push(path);
            } else if meta.is_file() {
                map.insert(
                    key,
                    Snap {
                        kind: 'f',
                        mode,
                        payload: std::fs::read(&path).expect("read fixture"),
                    },
                );
            }
        }
    }
    map
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

#[test]
fn build_shapes_agree() {
    let dir = TempDir::new("mb-build").expect("fixture dir");
    let root = dir.path();
    let bodies = [
        ("plain", "Host example\n  ForwardAgent yes"),
        ("padded", "\n\nHost example\n\n"),
        ("empty", ""),
        (
            "modelines",
            "# vim: ft=sshconfig\nHost a\n#   vim: sw=2\n# -*- mode: conf -*-\nHost b",
        ),
        ("only-modelines", "# vim: x\n# -*- y -*-"),
        ("hash-kept", "# a comment\nHost c"),
        ("crlf", "Host d\r\n  Opt yes\r\n"),
        ("tabs", "\tHost e"),
    ];
    for (label, body) in bodies {
        let marker = format!("# dot-{label}");
        let (code, out) = shell_run(
            root,
            &[
                marker.as_str().as_ref(),
                "/src/frag".as_ref(),
                body.as_ref(),
            ],
            "_mb_build \"$2\" \"$3\" \"$4\"",
        );
        assert_eq!(code, 0, "shell build {label}");
        let shell = String::from_utf8(out).expect("build text");
        let rust = merge_block::build(&marker, "/src/frag", body);
        assert_eq!(rust, shell, "build parity for {label}");
        assert!(!rust.ends_with('\n'), "no trailing newline for {label}");
    }
}

#[test]
fn strip_cases_agree() {
    let dir = TempDir::new("mb-strip").expect("fixture dir");
    let root = dir.path();
    let block_a = merge_block::build("# dot-a", "/s/a", "Host a");
    let block_b = merge_block::build("# dot-b", "/s/b", "Host b");
    let cases = [
        ("absent", "Host hand\n".to_string()),
        ("trailing-blanks", "Host hand\n\n\n".to_string()),
        ("single", format!("Host hand\n{block_a}\nHost tail\n")),
        (
            "unterminated",
            "Host hand\n# dot-a begin\nHost a\n".to_string(),
        ),
        ("two-ranges", format!("{block_a}\nmid\n{block_a}\n")),
        (
            "same-line",
            "top\n# dot-a begin stuff # dot-a end\nbottom\n".to_string(),
        ),
        ("other-marker-kept", format!("{block_b}\n{block_a}\n")),
    ];
    for (label, input) in &cases {
        for marker in ["# dot-a", "# dot-b", "# dot-missing"] {
            // Callers consume the capture, which strips the output
            // newline the function itself prints.
            let (code, out) = shell_run(
                root,
                &[marker.as_ref(), input.as_ref()],
                "stripped=$(_mb_strip \"$2\" \"$3\"); printf '%s' \"$stripped\"",
            );
            assert_eq!(code, 0, "shell strip {label}");
            let shell = String::from_utf8(out).expect("strip text");
            let rust = merge_block::strip(marker, input);
            assert_eq!(rust, shell, "strip parity for {label} with {marker}");
        }
    }
    // Stripping with the wrong marker leaves content alone.
    let input = format!("{block_a}\n");
    assert_eq!(
        merge_block::strip("# dot-b", &input),
        input.trim_end_matches('\n')
    );
}

#[test]
fn strip_family_cases_agree() {
    let dir = TempDir::new("mb-family").expect("fixture dir");
    let root = dir.path();
    let cases = [
        ("empty", ""),
        ("no-family", "Host hand\n# other begin\nx\n# other end\n"),
        (
            "one-block",
            "Host hand\n# ssh frag begin\nHost a\n# ssh frag end\nHost tail\n",
        ),
        (
            "stale-name",
            "Host hand\n# ssh old-frag begin\nHost old\n# ssh old-frag end\n",
        ),
        ("unterminated", "Host hand\n# ssh frag begin\nHost a\n"),
        (
            "nested-other",
            "# ssh frag begin\n# other begin\nx\n# ssh frag end\n",
        ),
        ("begin-only-line", "prefix # ssh frag begin\nHost a\n"),
    ];
    for (label, input) in cases {
        // Callers consume the capture, which strips the output
        // newline the function itself prints.
        let (code, out) = shell_run(
            root,
            &[input.as_ref()],
            "stripped=$(_mb_strip_family \"# ssh\" \"$2\"); printf '%s' \"$stripped\"",
        );
        assert_eq!(code, 0, "shell strip family {label}");
        let shell = String::from_utf8(out).expect("family text");
        let rust = merge_block::strip_family("# ssh", input);
        assert_eq!(rust, shell, "family parity for {label}");
    }
}

#[test]
fn merge_twins_agree() {
    let setups: &[(&str, &str)] = &[
        ("fresh", ""),
        ("hand", "Host hand-managed\n  Opt yes\n\n\n"),
        (
            "stale",
            "Host hand\n# dot-app begin\n# DO NOT EDIT: changes will be overwritten by dot update\n# source: /old\nHost stale\n# dot-app end\nHost tail\n",
        ),
        ("foreign", "Host hand\n# foreign begin\nx\n# foreign end\n"),
    ];
    for (label, current) in setups {
        for family in [false, true] {
            let sdir = TempDir::new(&format!("merge-{label}-shell")).expect("shell dir");
            let rdir = TempDir::new(&format!("merge-{label}-rust")).expect("rust dir");
            let block_s = merge_block::build("# dot-app", "/src/app", "Host managed\n  Opt no");
            let block_r = block_s.clone();
            let dst_s = sdir.path().join("sub/ssh_config");
            let dst_r = rdir.path().join("sub/ssh_config");
            if !current.is_empty() {
                stage(sdir.path(), "sub/ssh_config", current.as_bytes());
                stage(rdir.path(), "sub/ssh_config", current.as_bytes());
            }
            let verb = if family { "family" } else { "exact" };
            let snippet = if family {
                "_mb_merge_family \"$2\" \"# dot\" \"$3\""
            } else {
                "_mb_merge \"$2\" \"$3\""
            };
            let (scode, _) = shell_run(
                sdir.path(),
                &[dst_s.as_os_str(), block_s.as_str().as_ref()],
                snippet,
            );
            let mut cache = temp::MoveCache::default();
            let rcode = if family {
                merge_block::merge_family(
                    &dst_r,
                    "# dot",
                    &[block_r.as_str()],
                    &mut ctx(rdir.path(), &mut cache),
                )
            } else {
                merge_block::merge(
                    &dst_r,
                    &[block_r.as_str()],
                    &mut ctx(rdir.path(), &mut cache),
                )
            };
            assert_eq!(rcode.is_ok(), scode == 0, "merge {verb} code for {label}");
            assert_eq!(
                snapshot(rdir.path()),
                snapshot(sdir.path()),
                "merge {verb} tree for {label}"
            );
            // Re-merging is a no-op on both sides (same bytes and mtime).
            let before = std::fs::metadata(&dst_r)
                .expect("merged file")
                .modified()
                .expect("mtime");
            let (scode2, _) = shell_run(
                sdir.path(),
                &[dst_s.as_os_str(), block_s.as_str().as_ref()],
                snippet,
            );
            let mut cache2 = temp::MoveCache::default();
            let rcode2 = if family {
                merge_block::merge_family(
                    &dst_r,
                    "# dot",
                    &[block_r.as_str()],
                    &mut ctx(rdir.path(), &mut cache2),
                )
            } else {
                merge_block::merge(
                    &dst_r,
                    &[block_r.as_str()],
                    &mut ctx(rdir.path(), &mut cache2),
                )
            };
            assert_eq!(scode2, 0, "shell re-merge {label}");
            assert!(rcode2.is_ok(), "rust re-merge {label}");
            let after = std::fs::metadata(&dst_r)
                .expect("merged file")
                .modified()
                .expect("mtime");
            assert_eq!(before, after, "re-merge skips the write for {label}");
            assert_eq!(
                snapshot(rdir.path()),
                snapshot(sdir.path()),
                "re-merge tree for {label}"
            );
        }
    }
}

#[test]
fn merge_sets_modes() {
    let dir = TempDir::new("mb-modes").expect("fixture dir");
    let root = dir.path();
    let block = merge_block::build("# dot-app", "/src/app", "Host m");
    let dst = root.join("new/dir/ssh_config");
    let mut cache = temp::MoveCache::default();
    merge_block::merge(&dst, &[block.as_str()], &mut ctx(root, &mut cache)).expect("merge");
    let file_mode = std::fs::symlink_metadata(&dst)
        .expect("dst")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(file_mode, 0o600, "destination mode");
    let dir_mode = std::fs::symlink_metadata(root.join("new/dir"))
        .expect("parent")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(dir_mode, 0o700, "created parent mode");
    // Pre-existing parents keep their mode.
    let dst2 = root.join("new/dir/second");
    merge_block::merge(&dst2, &[block.as_str()], &mut ctx(root, &mut cache)).expect("merge 2");
    let dir_mode2 = std::fs::symlink_metadata(root.join("new/dir"))
        .expect("parent")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(dir_mode2, 0o700, "existing parent untouched");
}
