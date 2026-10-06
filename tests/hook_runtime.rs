//! Contracts for the public, versioned hook runtime ABI
//! (`lib/dot/public/hook-runtime-v1`): fragment families and their byte
//! globs, managed blocks, merge-hook helpers, and the `dot_hook_*` API.
//!
//! The shell runtime is the only implementation of these helpers, so every
//! case drives it through [`hook_runtime::Runtime`].

#[path = "support/hook_runtime.rs"]
mod hook_runtime;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use dot_test_support::TempDir;
use hook_runtime::Runtime;

/// Filter patterns and the family keys they select.
type FilterCase<'a> = (&'a [&'a [u8]], &'a [&'a [u8]]);
/// JSON layer label, destination body (absent when `None`), source body.
type JsonCase<'a> = (&'a str, Option<&'a [u8]>, &'a [u8]);

fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture parents");
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).expect("metadata").mode() & 0o7777
}

/// Split newline-terminated output into its lines (bytes, no terminators).
pub fn lines(stdout: &[u8]) -> Vec<Vec<u8>> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// A scratch HOME for one runtime invocation.
fn home(tag: &str) -> TempDir {
    TempDir::new(tag).expect("fixture home")
}

// --- Fragment families -----------------------------------------------------

/// One family containing ordinary aggregates, replacement groups, artifacts,
/// nested non-candidates, links, a tabbed name, and (off macOS, whose
/// filesystems reject it) a non-UTF-8 name.
struct Family {
    dir: TempDir,
}

impl Family {
    fn build() -> Self {
        let dir = TempDir::new("families").expect("temp dir");
        let root = dir.path();
        for name in [
            "10-core.json",
            "15-a.sh",
            "20-b.sh",
            "25-notes.txt",
            "80-strange.replace",
            "85-tab\tname.json",
            "90-extra.json",
            ".hidden.json",
            "99-temp.json~",
            "x.tmp",
            "x.tmp.1",
            "y.bak",
            "z.swp",
            "w.swo",
            ".DS_Store",
            "65-subdir/10-nested.json",
            "05-group.replace/01-low.sh",
            "05-group.replace/02-high.sh",
            "30-second.replace/a.sh",
            "30-second.replace/b.sh",
            "50-env.replace/50-alpha.json",
            "50-env.replace/80-beta.json",
            "60-ignored.replace/.hidden.json",
            "60-ignored.replace/x.tmp",
            "70-mode.replace/10-dark.json",
            "70-mode.replace/20-light.json",
            "75-nested.replace/90-dir/99-nested.json",
        ] {
            stage(root, name, format!("{name}\n").as_bytes());
        }
        std::fs::create_dir(root.join("55-empty.replace")).expect("empty replacement group");
        std::os::unix::fs::symlink("10-core.json", root.join("40-link.json"))
            .expect("live fragment link");
        std::os::unix::fs::symlink("missing", root.join("45-dangling.json"))
            .expect("dangling fragment link");
        #[cfg(not(target_os = "macos"))]
        std::fs::write(
            root.join(OsStr::from_bytes(b"87-bad\xff.json")),
            b"non-utf8\n",
        )
        .expect("non-UTF-8 fixture");
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn add_filtered_out_winner(&self) {
        stage(self.root(), "50-env.replace/90-not-json.txt", b"late\n");
    }

    /// Family-relative keys `dot_family_files_matching` streams for
    /// `patterns`, in stream order.
    fn keys(&self, patterns: &[&[u8]]) -> Vec<Vec<u8>> {
        family_keys(self.root(), patterns)
    }
}

/// Stream one family through the runtime and strip the `DIRECTORY/` prefix
/// each result carries.
fn family_keys(root: &Path, patterns: &[&[u8]]) -> Vec<Vec<u8>> {
    let scratch = home("families-home");
    let stdout = Runtime::new(scratch.path(), "dot_family_files_matching \"$@\"\n")
        .arg(root)
        .args(patterns.iter().map(|pattern| OsStr::from_bytes(pattern)))
        .stdout();
    let mut prefix = root.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    lines(&stdout)
        .into_iter()
        .map(|line| {
            line.strip_prefix(prefix.as_slice())
                .expect("family result stays under its root")
                .to_vec()
        })
        .collect()
}

fn sorted(mut keys: Vec<&[u8]>) -> Vec<Vec<u8>> {
    keys.sort();
    keys.into_iter().map(<[u8]>::to_vec).collect()
}

fn all_keys(filtered_out_winner: bool) -> Vec<Vec<u8>> {
    let mut keys = vec![
        b"05-group.replace/02-high.sh".as_slice(),
        b"10-core.json",
        b"15-a.sh",
        b"20-b.sh",
        b"25-notes.txt",
        b"30-second.replace/b.sh",
        b"40-link.json",
        if filtered_out_winner {
            b"50-env.replace/90-not-json.txt"
        } else {
            b"50-env.replace/80-beta.json"
        },
        b"70-mode.replace/20-light.json",
        b"80-strange.replace",
        b"85-tab\tname.json",
        b"90-extra.json",
    ];
    // `cfg!` rather than `#[cfg]`, so `keys` is mutated on every target
    // (APFS refuses the non-UTF-8 name, so macOS never creates it).
    if cfg!(not(target_os = "macos")) {
        keys.push(b"87-bad\xff.json");
    }
    sorted(keys)
}

#[test]
fn family_stream_orders_aggregates_and_replacement_winners() {
    let family = Family::build();
    assert_eq!(family.keys(&[]), all_keys(false));
    let scratch = home("families-unfiltered");
    let unfiltered = Runtime::new(scratch.path(), "dot_family_files \"$1\"\n")
        .arg(family.root())
        .stdout();
    let matching = Runtime::new(scratch.path(), "dot_family_files_matching \"$1\"\n")
        .arg(family.root())
        .stdout();
    assert_eq!(unfiltered, matching);
}

#[test]
fn filtering_precedes_replacement_selection_and_has_literal_results() {
    let family = Family::build();
    family.add_filtered_out_winner();

    let mut json = vec![
        b"10-core.json".as_slice(),
        b"40-link.json",
        b"50-env.replace/80-beta.json",
        b"70-mode.replace/20-light.json",
        b"85-tab\tname.json",
        b"90-extra.json",
    ];
    // `cfg!` rather than `#[cfg]`, so `json` is mutated on every target
    // (APFS refuses the non-UTF-8 name, so macOS never creates it).
    if cfg!(not(target_os = "macos")) {
        json.push(b"87-bad\xff.json");
    }
    assert_eq!(
        family.keys(&[b"*.json", b"*.replace/*.json"]),
        sorted(json),
        "the later non-JSON member must not displace a matching JSON winner"
    );

    let cases: &[FilterCase<'_>] = &[
        (
            &[b"*.sh"],
            &[
                b"05-group.replace/02-high.sh",
                b"15-a.sh",
                b"20-b.sh",
                b"30-second.replace/b.sh",
            ],
        ),
        (
            &[b"*.txt"],
            &[b"25-notes.txt", b"50-env.replace/90-not-json.txt"],
        ),
        (&[b"05-group.replace/*"], &[b"05-group.replace/02-high.sh"]),
        (&[b"nomatch*"], &[]),
        (&[b"10-*", b"*-b.sh"], &[b"10-core.json", b"20-b.sh"]),
        (&[b"02-high.sh"], &[]),
        (&[b"*.replace"], &[b"80-strange.replace"]),
        (&[b"a|b"], &[]),
        (&[b"[12]0-*"], &[b"10-core.json", b"20-b.sh"]),
    ];
    for (patterns, want) in cases {
        assert_eq!(
            family.keys(patterns),
            sorted(want.to_vec()),
            "patterns {patterns:?}"
        );
    }
    assert_eq!(family.keys(&[b"*"]), all_keys(true));
}

#[test]
fn artifact_names_nested_entries_and_dangling_links_are_excluded() {
    let cases: &[(&str, bool)] = &[
        ("hook.sh", true),
        ("10-first", true),
        ("tmp", true),
        ("a.b", true),
        ("DS_Store-x", true),
        ("~lead", true),
        (".hidden", false),
        (".replace", false),
        ("notes~", false),
        ("frag.tmp", false),
        ("frag.tmp.1", false),
        ("old.bak", false),
        ("x.swp", false),
        ("y.swo", false),
        (".DS_Store", false),
    ];
    for (name, want) in cases {
        let dir = TempDir::new("family-candidate").expect("temp dir");
        stage(dir.path(), name, b"fragment\n");
        let keys = family_keys(dir.path(), &[]);
        assert_eq!(!keys.is_empty(), *want, "name {name:?}");
    }

    let keys = Family::build().keys(&[]);
    for rejected in [
        b"65-subdir/10-nested.json".as_slice(),
        b"75-nested.replace/90-dir/99-nested.json",
        b"45-dangling.json",
        b".hidden.json",
        b"99-temp.json~",
    ] {
        assert!(!keys.iter().any(|key| key == rejected), "key {rejected:?}");
    }
}

#[test]
fn missing_and_non_directory_inputs_are_empty_and_missing_argument_is_usage() {
    let scratch = home("family-inputs");
    let file = scratch.write("not-a-directory", b"payload");
    let ignored = scratch.path().join("ignored-only");
    std::fs::create_dir_all(ignored.join("group.replace")).expect("ignored-only group");
    stage(&ignored, "group.replace/.hidden.json", b"hidden\n");
    let stdout = Runtime::new(
        scratch.path(),
        r#"
printf 'missing=%s\n' "$(dot_family_files /nonexistent-family-dir-xyz)"
printf 'file=%s\n' "$(dot_family_files "$1")"
printf 'ignored=%s\n' "$(dot_family_files "$2")"
printf 'files-usage=%s\n' "$(rc dot_family_files)"
printf 'files-extra=%s\n' "$(rc dot_family_files "$2" extra)"
printf 'matching-usage=%s\n' "$(rc dot_family_files_matching)"
"#,
    )
    .arg(&file)
    .arg(&ignored)
    .stdout();
    assert_eq!(
        String::from_utf8(stdout).expect("utf8"),
        "missing=\nfile=\nignored=\nfiles-usage=2\nfiles-extra=2\nmatching-usage=2\n"
    );
}

#[test]
fn family_globs_have_literal_byte_verdicts() {
    let pairs: &[(&[u8], &[u8], bool)] = &[
        (b"[c-a]", b"a", false),
        (b"[c-a]", b"c", false),
        (b"[c-a]", b"b", false),
        (b"[--0]", b"-", true),
        (b"[--0]", b".", true),
        (b"[--0]", b"0", true),
        (b"[--0]", b"1", false),
        (b"[\\]]", b"]", true),
        (b"[\\]]", b"\\", false),
        (b"[a\\]c]", b"]", true),
        (b"[a\\]c]", b"a]c", false),
        (b"[a\\bc]", b"b", true),
        (b"[\\\\]", b"\\", true),
        (b"[a\\\\c]", b"\\", true),
        (b"[a\\\\c]", b"a", true),
        (b"[a\\\\-c]", b"a", true),
        (b"[a\\\\-c]", b"\\", true),
        (b"[a\\\\-c]", b"b", true),
        (b"[a\\\\-c]", b"-", false),
        (b"[a\\\\-c]", b"c", true),
        (b"[\\--0]", b"-", true),
        (b"[\\--0]", b".", true),
        (b"[\\--0]", b"0", true),
        (b"[\\--0]", b"\\", false),
        (b"[a-c-e-g]", b"b", true),
        (b"[a-c-e-g]", b"-", true),
        (b"[a-c-e-g]", b"f", true),
        (b"[a-c-e-g]", b"d", false),
        (b"[a-c-]", b"-", true),
        (b"[a-c--d]", b"-", true),
        (b"[a-c--d]", b".", true),
        (b"[a-c--d]", b"d", true),
        (b"[\\\\--0]", b"-", false),
        (b"[\\\\--0]", b".", false),
        (b"[\\\\--0]", b"0", true),
        (b"[c-A--b]", b"-", true),
        (b"[c-A--b]", b".", true),
        (b"[c-A--b]", b"b", true),
        (b"[c-A---b]", b"-", true),
        (b"[c-A---b]", b".", false),
        (b"[c-A---b]", b"b", true),
        (b"?", "é".as_bytes(), false),
        (b"??", "é".as_bytes(), true),
        ("[é]".as_bytes(), "é".as_bytes(), false),
        ("*é*".as_bytes(), "café".as_bytes(), true),
        (b"", b"", true),
        (b"", b"a", false),
        (b"*a*b", b"aab", true),
        (b"a*b*c", b"abc", true),
        (b"a*b*c", b"axbyc", true),
        (b"a*b*c", b"ac", false),
        (b"[ab]*[cd]", b"axd", true),
        (b"[ab]*[cd]", b"axe", false),
        (b"*[*]*", b"a[b", false),
        (b"*[*]*", b"ab", false),
        (b"a\\", b"a\\", true),
        (b"a\\", b"ab", false),
        (b"[ab", b"[ab", true),
        (b"[ab", b"a", false),
        (b"*.*", b"x.tmp.1", true),
        (b"*.tmp.*", b"x.tmp", false),
        (b"?", b"", false),
        (b"[!a]", b"b", true),
        (b"[^a]", b"a", false),
        (b"[]a]", b"]", true),
        (b"[a-]", b"-", true),
        (b"[-a]", b".", false),
        (b"a|b", b"a", false),
        (b"**", b"anything", true),
        (b"\\*\\?\\[", b"*?[", true),
    ];
    // Patterns reach the filter exactly as hooks pass them to
    // `dot_family_files_matching`; the key matcher is the one place that
    // interprets them, so the matrix calls it directly.
    let scratch = home("family-globs");
    let mut runtime = Runtime::new(
        scratch.path(),
        r#"
while (($#)); do
  if _dot_family_key_matches "$2" "$1"; then printf '1\n'; else printf '0\n'; fi
  shift 2
done
"#,
    );
    for (pattern, key, _) in pairs {
        runtime = runtime
            .arg(OsStr::from_bytes(pattern))
            .arg(OsStr::from_bytes(key));
    }
    let verdicts = runtime.stdout();
    let verdicts: Vec<bool> = verdicts
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line == b"1")
        .collect();
    assert_eq!(verdicts.len(), pairs.len());
    for ((pattern, key, want), got) in pairs.iter().zip(verdicts) {
        assert_eq!(
            got,
            *want,
            "pattern={:?} key={:?}",
            String::from_utf8_lossy(pattern),
            String::from_utf8_lossy(key)
        );
    }
}

#[test]
#[cfg(not(target_os = "macos"))]
fn non_utf8_name_and_filter_are_byte_exact() {
    let family = Family::build();
    assert_eq!(family.keys(&[b"87-*"]), vec![b"87-bad\xff.json".to_vec()]);
}

// --- Managed blocks --------------------------------------------------------

/// The exact block `dot_managed_block_build` assembles for a body that needs
/// no modeline or whitespace trimming.
fn block(marker: &str, source: &str, body: &str) -> String {
    format!(
        "{marker} begin\n# DO NOT EDIT: changes will be overwritten by dot update\n# source: {source}\n{body}\n{marker} end"
    )
}

/// `dot_managed_block_build MARKER SOURCE BODY`, byte for byte.
///
/// Hooks capture blocks with `$(...)`, where errexit does not reach into
/// the helper; the `||` list gives it the same context while keeping the
/// exact output bytes (a body of only modelines makes the helper's internal
/// `grep -v` pipeline fail, which aborts a direct call under `set -e`).
fn build(marker: &str, source: &str, body: &str) -> String {
    let scratch = home("block-build");
    let stdout = Runtime::new(scratch.path(), "dot_managed_block_build \"$@\" || exit\n")
        .args([marker, source, body])
        .stdout();
    String::from_utf8(stdout).expect("utf8 block")
}

/// A strip helper's result as hooks consume it: through command
/// substitution, which drops the newline the helper prints.
fn strip(function: &str, marker: &str, input: &str) -> String {
    let scratch = home("block-strip");
    let stdout = Runtime::new(
        scratch.path(),
        &format!("printf '%s' \"$({function} \"$1\" \"$2\")\"\n"),
    )
    .args([marker, input])
    .stdout();
    String::from_utf8(stdout).expect("utf8 strip")
}

/// Run `dot_managed_block_merge DESTINATION BLOCK...` (or the family form
/// with `family_prefix`) and return its status.
fn merge(destination: &Path, family_prefix: Option<&str>, blocks: &[&str]) -> i32 {
    let scratch = home("block-merge");
    let runtime = match family_prefix {
        Some(prefix) => Runtime::new(scratch.path(), "rc dot_managed_block_merge_family \"$@\"\n")
            .arg(destination)
            .arg(prefix),
        None => {
            Runtime::new(scratch.path(), "rc dot_managed_block_merge \"$@\"\n").arg(destination)
        }
    };
    let stdout = runtime.args(blocks).stdout();
    String::from_utf8(stdout)
        .expect("utf8 status")
        .trim()
        .parse()
        .expect("status")
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
        let built = build(&marker, "/src/frag", body);
        assert!(built.starts_with(&format!("{marker} begin\n")), "{label}");
        assert!(built.ends_with(&format!("{marker} end")), "{label}");
        assert!(!built.ends_with('\n'), "no trailing newline for {label}");
    }
    assert_eq!(
        build(
            "# dot-managed:example",
            "/source/example",
            "  generated\n# vim: set ft=conf\n# -*- mode: conf -*-\n",
        ),
        block("# dot-managed:example", "/source/example", "generated")
    );
    assert_eq!(
        build("# dot-a", "/s/a", "# vim: x\nHost a\n  # -*- kept -*-"),
        block("# dot-a", "/s/a", "Host a\n  # -*- kept -*-"),
        "only modelines starting at column zero are dropped"
    );
}

#[test]
fn strip_removes_only_the_selected_marker_range() {
    let block_a = block("# dot-a", "/s/a", "Host a");
    let block_b = block("# dot-b", "/s/b", "Host b");
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
            let stripped = strip("dot_managed_block_strip", marker, input);
            assert!(
                !stripped.contains(&format!("{marker} begin")),
                "strip {label} {marker}"
            );
        }
    }
    assert_eq!(
        strip(
            "dot_managed_block_strip",
            "# dot-a",
            &format!("Host hand\n{block_a}\nHost tail\n")
        ),
        "Host hand\nHost tail"
    );
    // Stripping with the wrong marker leaves content alone.
    let input = format!("{block_a}\n");
    assert_eq!(
        strip("dot_managed_block_strip", "# dot-b", &input),
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
        let stripped = strip("dot_managed_block_strip_family", "# ssh", input);
        assert!(
            !stripped
                .lines()
                .any(|line| line.starts_with("# ssh ") && line.ends_with(" begin")),
            "family {label}"
        );
    }
    assert_eq!(
        strip(
            "dot_managed_block_strip_family",
            "# ssh",
            "Host hand\n# ssh frag begin\nHost a\n# ssh frag end\nHost tail\n"
        ),
        "Host hand\nHost tail"
    );
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
    let managed = block("# dot-app", "/src/app", "Host managed\n  Opt no");
    for (label, current) in setups {
        for family in [None, Some("# dot")] {
            let dir = TempDir::new("block-merge-contract").expect("fixture");
            let destination = dir.path().join("sub/ssh_config");
            if !current.is_empty() {
                stage(dir.path(), "sub/ssh_config", current.as_bytes());
            }
            assert_eq!(
                merge(&destination, family, &[&managed]),
                0,
                "{label} {family:?}"
            );
            let first = std::fs::read_to_string(&destination).expect("merged");
            assert!(first.contains("Host managed"), "{label} {family:?}");
            assert!(!first.contains("Host stale"), "{label} {family:?}");
            let inode = std::fs::metadata(&destination).expect("merged").ino();
            // Re-merging the same blocks skips the write entirely.
            assert_eq!(merge(&destination, family, &[&managed]), 0);
            assert_eq!(
                std::fs::metadata(&destination).expect("re-merged").ino(),
                inode
            );
            assert_eq!(
                std::fs::read_to_string(&destination).expect("re-merged"),
                first
            );
        }
    }
}

#[test]
fn family_merge_has_exact_order_and_keeps_inode_when_unchanged() {
    let dir = TempDir::new("block-merge-family").expect("fixture");
    let destination = stage(
        dir.path(),
        "config/output",
        b"manual first\n\n# dot-managed:family:old begin\n# DO NOT EDIT: changes will be overwritten by dot update\n# source: /source/old\nold generated\n# dot-managed:family:old end\n\nmanual last\n",
    );
    let first = block("# dot-managed:family:new", "/source/new", "new generated");
    let second = block(
        "# dot-managed:family:second",
        "/source/second",
        "second generated",
    );
    let prefix = Some("# dot-managed:family:");
    assert_eq!(merge(&destination, prefix, &[&first, &second]), 0);
    let expected = format!("manual first\n\nmanual last\n\n{first}\n\n{second}\n");
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), expected);
    let inode = std::fs::metadata(&destination).unwrap().ino();
    assert_eq!(merge(&destination, prefix, &[&first, &second]), 0);
    assert_eq!(std::fs::metadata(&destination).unwrap().ino(), inode);
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), expected);
}

#[test]
fn merge_refuses_directory_destination_without_nesting_stage() {
    let dir = TempDir::new("block-merge-directory").expect("fixture");
    let destination = dir.path().join("foreign-directory");
    std::fs::create_dir(&destination).unwrap();
    let managed = block("# dot-app", "/source", "managed");
    assert_eq!(merge(&destination, None, &[&managed]), 1);
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "the staged sibling is removed"
    );
}

#[test]
fn merge_sets_modes() {
    let dir = TempDir::new("block-merge-modes").expect("fixture dir");
    let root = dir.path();
    let managed = block("# dot-app", "/src/app", "Host m");
    let destination = root.join("new/dir/ssh_config");
    assert_eq!(merge(&destination, None, &[&managed]), 0);
    assert_eq!(mode(&destination), 0o600, "destination mode");
    assert_eq!(mode(&root.join("new/dir")), 0o700, "created parent mode");
    // Pre-existing parents keep their mode.
    std::fs::set_permissions(root.join("new/dir"), std::fs::Permissions::from_mode(0o750))
        .expect("loosen parent");
    assert_eq!(merge(&root.join("new/dir/second"), None, &[&managed]), 0);
    assert_eq!(
        mode(&root.join("new/dir")),
        0o750,
        "existing parent untouched"
    );
}

// --- Merge-hook helpers and the hook API -----------------------------------

#[test]
fn hook_relative_shape_matrix_is_byte_exact() {
    let rows: &[(&[u8], bool)] = &[
        (b"", false),
        (b"/abs", false),
        (b".", false),
        (b"..", false),
        (b"./x", false),
        (b"../x", false),
        (b"a/./b", false),
        (b"a/../b", false),
        (b"a/", false),
        (b"a//b", false),
        (b"a\nb", false),
        (b"a\rb", false),
        (b"hook.sh", true),
        (b"a/b.sh", true),
        (b".hidden", true),
        (b"..a", true),
        (b"a..b", true),
        (b"a b", true),
        (b"$HOME-x", true),
    ];
    let scratch = home("hook-relative");
    let extensions = scratch.path().join("extensions");
    std::fs::create_dir(&extensions).expect("extensions");
    let mut runtime = Runtime::new(
        scratch.path(),
        "for relative in \"$@\"; do rc dot_hook_file \"$relative\"; done\n",
    )
    .env("DOT_EXTENSIONS_DIR", &extensions);
    for (relative, _) in rows {
        runtime = runtime.arg(OsStr::from_bytes(relative));
    }
    let statuses = String::from_utf8(runtime.stdout()).expect("utf8");
    let statuses: Vec<&str> = statuses.lines().collect();
    assert_eq!(statuses.len(), rows.len());
    for ((relative, valid), status) in rows.iter().zip(statuses) {
        // Well-shaped names reach trust validation and fail there (the
        // files do not exist); malformed ones are usage errors.
        let want = if *valid { "1" } else { "2" };
        assert_eq!(status, want, "{:?}", String::from_utf8_lossy(relative));
    }
}

#[test]
fn hook_file_requires_a_trusted_regular_extension() {
    let scratch = home("hook-file");
    let extensions = scratch.path().join("extensions");
    let good = stage(&extensions, "lib/hook.sh", b"hook_loaded=yes\n");
    let loose = stage(&extensions, "loose.sh", b"hook_loaded=loose\n");
    std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o700)).expect("mode");
    std::fs::set_permissions(
        extensions.join("lib"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("mode");
    std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o644)).expect("mode");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o666)).expect("mode");
    let stdout = Runtime::new(
        scratch.path(),
        r#"
dot_hook_file lib/hook.sh
printf 'file=%s\n' "$REPLY"
hook_loaded=no
dot_hook_source lib/hook.sh
printf 'sourced=%s\n' "$hook_loaded"
printf 'loose=%s\n' "$(rc dot_hook_file loose.sh)"
printf 'loose-source=%s\n' "$(rc dot_hook_source loose.sh)"
printf 'gone=%s\n' "$(rc dot_hook_file gone.sh)"
printf 'usage=%s\n' "$(rc dot_hook_file lib/hook.sh extra)"
"#,
    )
    .env("DOT_EXTENSIONS_DIR", &extensions)
    .stdout();
    assert_eq!(
        String::from_utf8(stdout).expect("utf8"),
        format!(
            "file={}\nsourced=yes\nloose=1\nloose-source=1\ngone=1\nusage=2\n",
            good.display()
        )
    );
}

#[test]
fn hook_family_helpers_preserve_order_filters_and_marker_identity() {
    let scratch = home("hook-family");
    let config = scratch.path().join("xdg-config");
    let family = config.join("dot/merge-hooks.d/demo");
    let first = stage(&family, "10-a", b"a");
    stage(&family, "grp.replace/05-low", b"x");
    let winner = stage(&family, "grp.replace/09-winner", b"w");
    let script = r#"
printf 'family=%s\n' "$(dot_hook_family demo)"
dot_hook_family_files demo | sed 's/^/files=/'
dot_hook_family_files_matching demo '1*' | sed 's/^/matching=/'
printf 'relative=%s\n' "$(dot_hook_family_relpath demo "$1")"
printf 'generic=%s\n' "$(dot_family_relpath demo "$1")"
printf 'marker=%s\n' "$(dot_hook_family_marker_name demo "$1")"
"#;
    let stdout = Runtime::new(scratch.path(), script)
        .arg(&winner)
        .env("XDG_CONFIG_HOME", &config)
        .stdout();
    assert_eq!(
        String::from_utf8(stdout).expect("utf8"),
        format!(
            "family={family}\nfiles={first}\nfiles={winner}\nmatching={first}\n\
             relative=grp.replace/09-winner\ngeneric=grp.replace/09-winner\n\
             marker=grp.replace_09-winner\n",
            family = family.display(),
            first = first.display(),
            winner = winner.display(),
        )
    );
    // Without XDG_CONFIG_HOME the family root falls back under HOME.
    let stdout = Runtime::new(scratch.path(), "dot_hook_family ssh\n").stdout();
    assert_eq!(
        stdout,
        format!(
            "{}\n",
            scratch
                .path()
                .join(".config/dot/merge-hooks.d/ssh")
                .display()
        )
        .into_bytes()
    );
}

#[test]
fn hook_expand_home_expands_only_documented_placeholders() {
    let scratch = home("hook-expand-home");
    let home = scratch.path().to_str().expect("utf8 home");
    let cases = [
        ("$HOME/.ssh", format!("{home}/.ssh")),
        ("${HOME}/.ssh", format!("{home}/.ssh")),
        ("~", home.to_string()),
        ("~/doc", format!("{home}/doc")),
        ("~other", "~other".to_string()),
        ("/abs", "/abs".to_string()),
        ("rel", "rel".to_string()),
        ("", String::new()),
        ("$HOME", home.to_string()),
        ("${HOME}", home.to_string()),
        ("~/$HOME", format!("{home}/{home}")),
        ("$HOME~", format!("{home}~")),
        ("$$HOME", format!("${home}")),
    ];
    let stdout = Runtime::new(
        scratch.path(),
        "for value in \"$@\"; do dot_expand_home \"$value\"; done\n",
    )
    .args(cases.iter().map(|(input, _)| *input))
    .stdout();
    let expected: String = cases.iter().map(|(_, want)| format!("{want}\n")).collect();
    assert_eq!(String::from_utf8(stdout).expect("utf8"), expected);
}

#[test]
fn hook_platform_and_host_filters_have_literal_inclusion_precedence() {
    let termux = "/data/data/com.termux/files/usr";
    let scratch = home("hook-platform");
    for (spec, prefix, expected) in [
        ("", "", "0"),
        ("linux", "", "0"),
        ("macos", "", "1"),
        ("wsl", "", "1"),
        ("android", "", "1"),
        ("!linux", "", "1"),
        ("linux,!macos", "", "0"),
        ("android", termux, "0"),
        ("!android", termux, "1"),
        ("linux", termux, "0"),
        ("linux,macos", termux, "0"),
        ("!macos", termux, "0"),
        ("!linux", termux, "1"),
        ("!android,!macos", termux, "1"),
        ("linux,!android", termux, "1"),
        ("android,!linux", termux, "1"),
        ("macos,!freebsd", termux, "1"),
    ] {
        let stdout = Runtime::new(
            scratch.path(),
            "_dot_hook_platform() { printf 'linux\\n'; }\nrc dot_hook_platform_match \"$1\"\n",
        )
        .arg(spec)
        .env("PREFIX", prefix)
        .stdout();
        assert_eq!(
            String::from_utf8(stdout).expect("utf8").trim(),
            expected,
            "spec {spec:?} prefix {prefix:?}"
        );
    }
    let stdout = Runtime::new(
        scratch.path(),
        r#"
_dot_hook_host() { printf 'fixture-host\n'; }
rc dot_hook_host_match FIXTURE-HOST
rc dot_hook_host_match 'fixture-host,!other'
rc dot_hook_host_match '!fixture-host'
rc dot_hook_platform_match
rc dot_hook_host_match
"#,
    )
    .stdout();
    assert_eq!(stdout, b"0\n0\n1\n2\n2\n");
}

#[test]
fn hook_tool_presence_distinguishes_path_lookup_and_explicit_paths() {
    let scratch = TempDir::new_exec("hook-tool").expect("fixture");
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).expect("bin");
    let tool = stage(&bin, "tool", b"#!/bin/sh\n");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("mode");
    stage(&bin, "plain", b"x");
    std::fs::create_dir(bin.join("directory")).expect("directory");
    let stdout = Runtime::new(
        scratch.path(),
        r#"
PATH=$1
for name in tool plain directory missing "$1/tool" "$1/missing" "$1/directory" ''; do
  rc dot_tool_present "$name"
done
rc dot_tool_present
rc dot_tool_present tool extra
"#,
    )
    .arg(&bin)
    .stdout();
    // PATH lookup follows `command -v`: a non-executable regular file
    // still resolves, a directory never does. Paths with a slash are
    // existence probes, so a directory path is present.
    assert_eq!(stdout, b"0\n0\n1\n1\n0\n1\n0\n2\n2\n2\n");
}

#[test]
fn public_hook_runtime_exports_literal_abi_results_and_usage_codes() {
    let scratch = TempDir::new_exec("hook-public-runtime").expect("fixture");
    let home = scratch.path().join("home");
    let config = scratch.path().join("config");
    let extensions = scratch.path().join("extensions");
    let family = config.join("dot/merge-hooks.d/fixture");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&extensions).expect("extensions");
    let member = stage(&family, "10-one.txt", b"one\n");
    let script = r#"
printf 'home=%s\n' "$(dot_expand_home '$HOME/example')"
printf 'tilde=%s\n' "$(dot_expand_home '~/example')"
printf 'family=%s\n' "$(dot_hook_family fixture)"
printf 'files=%s\n' "$(dot_hook_family_files fixture)"
printf 'matching=%s\n' "$(dot_hook_family_files_matching fixture '*.txt')"
printf 'relative=%s\n' "$(dot_hook_family_relpath fixture "$1")"
printf 'marker=%s\n' "$(dot_hook_family_marker_name fixture "$1")"
_dot_hook_platform() { printf 'linux\n'; }
_dot_hook_host() { printf 'fixture-host\n'; }
printf 'platform-linux=%s\n' "$(rc dot_hook_platform_match linux)"
printf 'platform-macos=%s\n' "$(rc dot_hook_platform_match macos)"
printf 'host-include=%s\n' "$(rc dot_hook_host_match FIXTURE-HOST)"
printf 'host-exclude=%s\n' "$(rc dot_hook_host_match '!fixture-host')"
for call in \
  'dot_expand_home' \
  'dot_expand_home one two' \
  'dot_hook_family_files_matching' \
  'dot_json_available extra' \
  'dot_managed_block_merge' \
  'dot_managed_block_merge_family only-destination' \
  'dot_tool_present ""'
do
  if eval "$call" >/dev/null 2>&1; then code=0; else code=$?; fi
  printf 'usage=%s\n' "$code"
done
"#;
    let stdout = Runtime::new(&home, script)
        .arg(&member)
        .env("XDG_CONFIG_HOME", &config)
        .env("DOT_EXTENSIONS_DIR", &extensions)
        .stdout();
    let expected = format!(
        "home={}/example\n\
         tilde={}/example\n\
         family={}\n\
         files={}\n\
         matching={}\n\
         relative=10-one.txt\n\
         marker=10-one.txt\n\
         platform-linux=0\n\
         platform-macos=1\n\
         host-include=0\n\
         host-exclude=1\n\
         usage=2\nusage=2\nusage=2\nusage=2\nusage=2\nusage=2\nusage=2\n",
        home.display(),
        home.display(),
        family.display(),
        member.display(),
        member.display(),
    );
    assert_eq!(String::from_utf8(stdout).expect("utf8"), expected);
}

/// `dot_write_text_if_changed DESTINATION TEXT`, returning its status.
fn write_text(destination: &Path, text: &str) -> Vec<u8> {
    let scratch = home("hook-write-text");
    Runtime::new(
        scratch.path(),
        "rc dot_write_text_if_changed \"$1\" \"$2\"\n",
    )
    .arg(destination)
    .arg(text)
    .stdout()
}

#[test]
fn write_text_is_exact_and_skips_an_unchanged_destination() {
    for (label, initial) in [
        ("absent", None),
        ("same", Some("line\n")),
        ("different", Some("old\n")),
    ] {
        let dir = TempDir::new("hook-write").expect("fixture");
        let destination = dir.path().join("out/conf");
        std::fs::create_dir_all(destination.parent().expect("parent")).expect("parent");
        if let Some(body) = initial {
            std::fs::write(&destination, body).expect("initial");
        }
        let before = std::fs::metadata(&destination).ok().map(|meta| meta.ino());
        assert_eq!(write_text(&destination, "line"), b"0\n", "{label}");
        assert_eq!(
            std::fs::read(&destination).expect("written"),
            b"line\n",
            "{label}"
        );
        if label == "same" {
            assert_eq!(
                before,
                std::fs::metadata(&destination).ok().map(|meta| meta.ino()),
                "an unchanged destination keeps its inode"
            );
        }
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            1,
            "{label}: no staged sibling survives"
        );
    }
}

#[test]
fn write_text_refuses_a_directory_without_nesting_staged_output() {
    let dir = TempDir::new("hook-write-directory").expect("fixture");
    let destination = dir.path().join("directory");
    std::fs::create_dir(&destination).expect("destination directory");
    assert_eq!(write_text(&destination, "unsafe"), b"1\n");
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
}

#[test]
fn repeated_multiline_text_write_preserves_bytes_and_inode() {
    let dir = TempDir::new("hook-write-multiline").expect("fixture");
    let destination = dir.path().join("generated/config");
    assert_eq!(write_text(&destination, "alpha\nbeta"), b"0\n");
    assert_eq!(std::fs::read(&destination).unwrap(), b"alpha\nbeta\n");
    let inode = std::fs::metadata(&destination).unwrap().ino();
    assert_eq!(write_text(&destination, "alpha\nbeta"), b"0\n");
    assert_eq!(std::fs::metadata(&destination).unwrap().ino(), inode);
}

/// A PATH directory of links to the tools the JSON-layer helpers need,
/// with or without `jq`, so availability is chosen by the test instead of
/// the host.
fn tool_path(dir: &Path, with_jq: bool) -> PathBuf {
    let bin = dir.join(if with_jq { "bin-jq" } else { "bin-no-jq" });
    std::fs::create_dir(&bin).expect("tool dir");
    let mut tools = vec![
        "basename", "chmod", "dirname", "git", "mkdir", "mktemp", "mv", "rm", "rmdir", "stat",
    ];
    if with_jq {
        tools.push("jq");
    }
    for tool in tools {
        std::os::unix::fs::symlink(dot_test_support::real_tool(tool), bin.join(tool))
            .expect("tool link");
    }
    bin
}

/// Run `dot_json_layer LABEL SOURCE DESTINATION FILTER` with `path` as
/// PATH; returns (status, stderr).
fn json_layer(
    path: &Path,
    label: &str,
    source: &Path,
    destination: &Path,
    filter: &str,
) -> (i32, String) {
    let scratch = home("hook-json-layer");
    let output = Runtime::new(scratch.path(), "dot_json_layer \"$@\"\n")
        .args([
            OsStr::new(label),
            source.as_os_str(),
            destination.as_os_str(),
            OsStr::new(filter),
        ])
        .env("PATH", path)
        .output();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

fn jq_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join("jq"))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

#[test]
fn json_layer_install_merge_and_failure_contracts_are_literal() {
    let filter = "$s[0] * $d[0]";
    let cases: &[JsonCase<'_>] = &[
        ("install", None, b"{\"a\":1}\n"),
        ("merge", Some(b"{\"a\":1}\n"), b"{\"b\":2}\n"),
        ("corrupt", Some(b"not json\n"), b"{\"b\":2}\n"),
        ("empty", Some(b""), b"{\"b\":2}\n"),
        ("bad-src", Some(b"{\"a\":1}\n"), b"not json\n"),
    ];
    // CI installs jq on every leg; a lost install must not silently skip
    // the jq branches there.
    assert!(
        jq_on_path() || std::env::var_os("CI").is_none(),
        "CI runs need jq on PATH"
    );
    let mut variants = vec![false];
    if jq_on_path() {
        variants.push(true);
    }
    for with_jq in variants {
        for (label, destination_body, source_body) in cases {
            let dir = TempDir::new("hook-json").expect("fixture");
            let path = tool_path(dir.path(), with_jq);
            let source = stage(dir.path(), "src.json", source_body);
            let destination = dir.path().join("dst.json");
            if let Some(body) = destination_body {
                std::fs::write(&destination, body).expect("destination");
            }
            let original = std::fs::read(&destination).ok();
            let (status, warnings) = json_layer(&path, label, &source, &destination, filter);
            // Layer failures are warnings, never hook failures.
            assert_eq!(status, 0, "{label} jq={with_jq}: {warnings}");
            let actual = std::fs::read(&destination).ok();
            if with_jq {
                match *label {
                    "install" => {
                        assert_eq!(actual, Some(b"{\n  \"a\": 1\n}\n".to_vec()));
                        assert_eq!(warnings, "");
                    }
                    "merge" => {
                        assert_eq!(actual, Some(b"{\n  \"a\": 1,\n  \"b\": 2\n}\n".to_vec()));
                        assert_eq!(warnings, "");
                    }
                    "corrupt" | "empty" => {
                        assert_eq!(actual, Some(b"{\n  \"b\": 2\n}\n".to_vec()));
                        assert_eq!(
                            warnings,
                            format!(
                                "    warning: corrupt {} — rebuilding\n",
                                destination.display()
                            )
                        );
                    }
                    "bad-src" => {
                        assert_eq!(actual, original, "failed merge preserves destination");
                        assert!(
                            warnings.ends_with("    warning: bad-src merge failed — skipping\n"),
                            "{warnings}"
                        );
                    }
                    _ => unreachable!(),
                }
            } else {
                assert_eq!(actual, None, "jq-free copy or rebuild installs nothing");
                assert!(
                    warnings.ends_with(&format!("    warning: {label} copy failed — skipping\n")),
                    "{label}: {warnings}"
                );
                assert_eq!(
                    warnings.contains(&format!("corrupt {} — rebuilding", destination.display())),
                    destination_body.is_some(),
                    "{label}: {warnings}"
                );
            }
            assert_eq!(
                std::fs::read_dir(dir.path())
                    .unwrap()
                    .filter(|entry| {
                        entry
                            .as_ref()
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .contains(".tmp.")
                    })
                    .count(),
                0,
                "{label} jq={with_jq}: no staged sibling survives"
            );
        }
    }
    if jq_on_path() {
        let dir = TempDir::new("hook-json-filter").expect("fixture");
        let path = tool_path(dir.path(), true);
        let source = stage(dir.path(), "src", b"{}\n");
        let destination = stage(dir.path(), "dst", b"{}\n");
        let (status, warnings) = json_layer(&path, "bad-filter", &source, &destination, "?!");
        assert_eq!(status, 0);
        assert_eq!(std::fs::read(&destination).unwrap(), b"{}\n");
        assert!(
            warnings.ends_with("    warning: bad-filter merge failed — skipping\n"),
            "{warnings}"
        );
    }
}

#[test]
fn json_availability_reflects_the_executable_path() {
    let scratch = TempDir::new_exec("hook-json-available").expect("fixture");
    let with_jq = scratch.path().join("with-jq");
    let without = scratch.path().join("without");
    std::fs::create_dir(&with_jq).expect("dir");
    std::fs::create_dir(&without).expect("dir");
    let jq = stage(&with_jq, "jq", b"#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(&jq, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let stdout = Runtime::new(
        scratch.path(),
        "PATH=$1; rc dot_json_available\nPATH=$2; rc dot_json_available\n",
    )
    .arg(&with_jq)
    .arg(&without)
    .stdout();
    assert_eq!(stdout, b"0\n1\n");
}
