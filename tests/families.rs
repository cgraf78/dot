//! Native contracts for ordered fragment-family discovery and byte globbing.

use std::ffi::OsStr;
use std::path::Path;

use dot::families::{self, family_files};
use dot_test_support::TempDir;

type FilterCase<'a> = (&'a [&'a [u8]], &'a [&'a [u8]]);

#[cfg(unix)]
fn raw_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn raw_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn write(root: &Path, name: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(path, format!("{name}\n")).expect("fixture file");
}

/// One family containing ordinary aggregates, replacement groups, artifacts,
/// nested non-candidates, links, a tabbed name, and a non-UTF-8 name.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn build() -> Self {
        let dir = TempDir::new("families-native").expect("temp dir");
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
            write(root, name);
        }
        std::fs::create_dir(root.join("55-empty.replace")).expect("empty replacement group");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("10-core.json", root.join("40-link.json"))
                .expect("live fragment link");
            std::os::unix::fs::symlink("missing", root.join("45-dangling.json"))
                .expect("dangling fragment link");
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::os::unix::ffi::OsStrExt as _;
            std::fs::write(
                root.join(OsStr::from_bytes(b"87-bad\xff.json")),
                b"non-utf8\n",
            )
            .expect("non-UTF-8 fixture");
        }
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn add_filtered_out_winner(&self) {
        write(self.root(), "50-env.replace/90-not-json.txt");
    }

    fn keys(&self, patterns: &[&[u8]]) -> Vec<Vec<u8>> {
        family_files(Some(self.root()), patterns)
            .expect("family directory argument")
            .into_iter()
            .map(|path| relative_bytes(self.root(), &path))
            .collect()
    }
}

fn relative_bytes(root: &Path, path: &Path) -> Vec<u8> {
    raw_bytes(
        path.strip_prefix(root)
            .expect("family result stays under its root")
            .as_os_str(),
    )
}

fn expected(mut keys: Vec<&[u8]>) -> Vec<Vec<u8>> {
    keys.sort();
    keys.into_iter().map(<[u8]>::to_vec).collect()
}

fn all_expected(filtered_out_winner: bool) -> Vec<Vec<u8>> {
    let mut keys = vec![
        b"05-group.replace/02-high.sh".as_slice(),
        b"10-core.json",
        b"15-a.sh",
        b"20-b.sh",
        b"25-notes.txt",
        b"30-second.replace/b.sh",
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
    #[cfg(unix)]
    keys.push(b"40-link.json");
    #[cfg(all(unix, not(target_os = "macos")))]
    keys.push(b"87-bad\xff.json");
    expected(keys)
}

#[test]
fn family_stream_orders_aggregates_and_replacement_winners() {
    let fixture = Fixture::build();
    assert_eq!(fixture.keys(&[]), all_expected(false));
}

#[test]
fn filtering_precedes_replacement_selection_and_has_literal_results() {
    let fixture = Fixture::build();
    fixture.add_filtered_out_winner();

    let mut json = vec![
        b"10-core.json".as_slice(),
        b"50-env.replace/80-beta.json",
        b"70-mode.replace/20-light.json",
        b"85-tab\tname.json",
        b"90-extra.json",
    ];
    #[cfg(unix)]
    json.push(b"40-link.json");
    #[cfg(all(unix, not(target_os = "macos")))]
    json.push(b"87-bad\xff.json");
    assert_eq!(
        fixture.keys(&[b"*.json", b"*.replace/*.json"]),
        expected(json),
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
            fixture.keys(patterns),
            expected(want.to_vec()),
            "patterns {patterns:?}"
        );
    }
    assert_eq!(fixture.keys(&[b"*"]), all_expected(true));
}

#[test]
fn artifact_names_nested_entries_and_dangling_links_are_excluded() {
    let cases: &[(&[u8], bool)] = &[
        (b"hook.sh", true),
        (b"a.b", true),
        (b"DS_Store-x", true),
        (b"~lead", true),
        (b"", false),
        (b".hidden", false),
        (b".replace", false),
        (b"notes~", false),
        (b"frag.tmp", false),
        (b"frag.tmp.1", false),
        (b"old.bak", false),
        (b"x.swp", false),
        (b"y.swo", false),
        (b".DS_Store", false),
    ];
    for (name, want) in cases {
        assert_eq!(families::is_candidate_name(name), *want, "name {name:?}");
    }

    let fixture = Fixture::build();
    let keys = fixture.keys(&[]);
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
    assert_eq!(
        family_files(Some(Path::new("/nonexistent-family-dir-xyz")), &[]),
        Ok(Vec::new())
    );
    let scratch = TempDir::new("family-file-input").expect("temp dir");
    let file = scratch.write("not-a-directory", b"payload");
    assert_eq!(family_files(Some(&file), &[]), Ok(Vec::new()));
    assert_eq!(family_files(None, &[]), Err(families::Error::Usage));
    assert_eq!(families::Error::Usage.code(), 2);
    std::fs::remove_file(file).expect("remove non-directory fixture");

    let ignored = scratch.path().join("ignored-only.replace");
    std::fs::create_dir(&ignored).expect("ignored-only group");
    write(&ignored, ".hidden.json");
    assert_eq!(family_files(Some(scratch.path()), &[]), Ok(Vec::new()));
}

#[test]
#[cfg(unix)]
fn glob_byte_contract_has_literal_expected_verdicts() {
    use dot::glob::matches;
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
    for (pattern, key, want) in pairs {
        assert_eq!(
            matches(pattern, key),
            *want,
            "pattern={pattern:?} key={key:?}"
        );
    }
}

#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn non_utf8_name_and_filter_are_byte_exact() {
    let fixture = Fixture::build();
    assert_eq!(fixture.keys(&[b"87-*"]), vec![b"87-bad\xff.json".to_vec()]);
}
