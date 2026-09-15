//! Native tests for marked-block merging: block assembly, stripping (single and
//! family), atomic finalize, and both merge flavors — including
//! modeline corners, unterminated blocks, and idempotent re-merges.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use dot::merge_block;
use dot::temp;
use dot_test_support::TempDir;

/// Fresh publishing context over `root` (git digests run under it).
fn ctx<'a>(root: &'a Path, cache: &'a mut temp::MoveCache) -> merge_block::Ctx<'a> {
    merge_block::Ctx {
        source_root: root,
        cache,
    }
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
fn build_shapes_have_stable_boundaries() {
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
        let rust = merge_block::build(&marker, "/src/frag", body);
        assert!(rust.starts_with(&format!("{marker} begin\n")));
        assert!(rust.ends_with(&format!("{marker} end")));
        assert!(!rust.ends_with('\n'), "no trailing newline for {label}");
    }
    assert_eq!(
        merge_block::build(
            "# dot-managed:example",
            "/source/example",
            "  generated\n# vim: set ft=conf\n# -*- mode: conf -*-\n",
        ),
        "# dot-managed:example begin\n# DO NOT EDIT: changes will be overwritten by dot update\n# source: /source/example\ngenerated\n# dot-managed:example end"
    );
}

#[test]
fn strip_removes_only_the_selected_marker_range() {
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
            let rust = merge_block::strip(marker, input);
            assert!(!rust.contains(&format!("{marker} begin")), "strip {label}");
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
fn strip_family_removes_every_matching_marker_range() {
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
        let rust = merge_block::strip_family("# ssh", input);
        assert!(
            !rust
                .lines()
                .any(|line| line.starts_with("# ssh ") && line.ends_with(" begin")),
            "family {label}"
        );
    }
}

#[test]
fn merge_contracts_preserve_manual_content_and_replace_managed_blocks() {
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
            let rdir = TempDir::new(&format!("merge-{label}-rust")).expect("rust dir");
            let block_r = merge_block::build("# dot-app", "/src/app", "Host managed\n  Opt no");
            let dst_r = rdir.path().join("sub/ssh_config");
            if !current.is_empty() {
                stage(rdir.path(), "sub/ssh_config", current.as_bytes());
            }
            let verb = if family { "family" } else { "exact" };
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
            assert!(rcode.is_ok(), "merge {verb} code for {label}");
            // Re-merging is a no-op on both sides (same bytes and mtime).
            let before = std::fs::metadata(&dst_r)
                .expect("merged file")
                .modified()
                .expect("mtime");
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
            assert!(rcode2.is_ok(), "rust re-merge {label}");
            let after = std::fs::metadata(&dst_r)
                .expect("merged file")
                .modified()
                .expect("mtime");
            assert_eq!(before, after, "re-merge skips the write for {label}");
            assert!(
                std::fs::read_to_string(&dst_r)
                    .expect("merged")
                    .contains("Host managed")
            );
        }
    }
}

#[test]
fn family_merge_has_exact_order_and_keeps_inode_when_unchanged() {
    let dir = TempDir::new("merge-family-exact").expect("fixture");
    let destination = stage(
        dir.path(),
        "config/output",
        b"manual first\n\n# dot-managed:family:old begin\n# DO NOT EDIT: changes will be overwritten by dot update\n# source: /source/old\nold generated\n# dot-managed:family:old end\n\nmanual last\n",
    );
    let first = merge_block::build("# dot-managed:family:new", "/source/new", "new generated");
    let second = merge_block::build(
        "# dot-managed:family:second",
        "/source/second",
        "second generated",
    );
    let mut cache = temp::MoveCache::default();
    merge_block::merge_family(
        &destination,
        "# dot-managed:family:",
        &[first.as_str(), second.as_str()],
        &mut ctx(dir.path(), &mut cache),
    )
    .expect("family merge");
    let expected = format!("manual first\n\nmanual last\n\n{first}\n\n{second}\n");
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), expected);
    let inode = std::fs::metadata(&destination).unwrap().ino();
    let mut cache = temp::MoveCache::default();
    merge_block::merge_family(
        &destination,
        "# dot-managed:family:",
        &[first.as_str(), second.as_str()],
        &mut ctx(dir.path(), &mut cache),
    )
    .expect("idempotent family merge");
    assert_eq!(std::fs::metadata(&destination).unwrap().ino(), inode);
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), expected);
}

#[test]
fn merge_refuses_directory_destination_without_nesting_stage() {
    let dir = TempDir::new("merge-directory-refusal").expect("fixture");
    let destination = dir.path().join("foreign-directory");
    std::fs::create_dir(&destination).unwrap();
    let block = merge_block::build("# dot-app", "/source", "managed");
    let mut cache = temp::MoveCache::default();
    assert!(
        merge_block::merge(
            &destination,
            &[block.as_str()],
            &mut ctx(dir.path(), &mut cache),
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
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
