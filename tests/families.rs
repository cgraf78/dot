//! Differential parity tests for fragment-family discovery against
//! `lib/dot/families.sh`: aggregate files, `.replace` winner
//! selection, artifact filtering, pattern pre-filtering, and the
//! byte-ordered stream (including a non-UTF8 filename probe).

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use dot::families::family_files;

/// Raw bytes of an `OsStr` for byte-exact comparisons. Unix: lossless.
/// Elsewhere: lossy (the byte-level probes are `#[cfg(unix)]`-gated;
/// only UTF-8 fixtures reach this helper there).
#[cfg(unix)]
fn raw_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

/// Non-Unix fallback for [`raw_bytes`].
#[cfg(not(unix))]
fn raw_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn bash_bin() -> &'static str {
    for candidate in ["/usr/bin/bash", "/bin/bash"] {
        if std::path::Path::new(candidate).is_file() {
            return candidate;
        }
    }
    panic!("no bash interpreter found");
}

/// Run `dot_family_files` / `dot_family_files_matching` on `dir` with
/// `patterns`. Returns (exit code, raw stdout bytes).
fn shell_family(dir: &OsStr, matching: bool, patterns: &[&OsStr]) -> (i32, Vec<u8>) {
    // $0 dummy, $1 family dir, $2 tree root, $3 mode, $4+ patterns.
    // No `shift` juggling (`shift` never moves `$0`); `"${@:4}"`
    // keeps the callee argv exact in both modes.
    let mut cmd = Command::new(bash_bin());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(
        ". \"$2/lib/dot/families.sh\"\n\
         if [ \"$3\" = matching ]; then dot_family_files_matching \"$1\" \"${@:4}\";\n\
         else dot_family_files \"$1\"; fi\n",
    );
    cmd.arg("dot-test-sh");
    cmd.arg(dir);
    cmd.arg(env!("CARGO_MANIFEST_DIR"));
    cmd.arg(if matching { "matching" } else { "plain" });
    for pattern in patterns {
        cmd.arg(pattern);
    }
    let output = cmd
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn bash");
    (output.status.code().unwrap_or(99), output.stdout)
}

/// Fixture family: aggregates, two populated groups, empty and
/// ignored-only groups, a non-directory `*.replace` file, editor
/// artifacts, a symlink pair, and a non-UTF8 name.
struct Fixture {
    dir: dot::test_support::TempDir,
}

impl Fixture {
    fn build() -> Self {
        let dir = dot::test_support::TempDir::new("families").expect("temp dir");
        let root = dir.path();
        for name in [
            "10-a.sh",
            "20-b.sh",
            "notes.txt",
            ".hidden",
            "bak~",
            "x.tmp",
            "x.tmp.1",
            "y.bak",
            "z.swp",
            "w.swo",
            ".DS_Store",
            "strange.replace",
        ] {
            std::fs::write(root.join(name), b"payload").expect("write");
        }
        // Non-UTF8 filename: byte-level candidacy and ordering.
        // Unix-only: non-UTF8 names have no portable spelling.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            std::fs::write(root.join(OsStr::from_bytes(b"bad\xffname.sh")), b"payload")
                .expect("write");
        }
        let group = root.join("05-group.replace");
        std::fs::create_dir(&group).expect("mkdir");
        for name in ["01-low.sh", "02-high.sh", "skip~", ".hidden-in-group"] {
            std::fs::write(group.join(name), b"payload").expect("write");
        }
        let second = root.join("30-second.replace");
        std::fs::create_dir(&second).expect("mkdir");
        for name in ["a.sh", "b.sh"] {
            std::fs::write(second.join(name), b"payload").expect("write");
        }
        std::fs::create_dir(root.join("empty.replace")).expect("mkdir");
        let ignored = root.join("ignored-only.replace");
        std::fs::create_dir(&ignored).expect("mkdir");
        std::fs::write(ignored.join("x.tmp"), b"payload").expect("write");
        std::fs::create_dir(root.join("plain")).expect("mkdir");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("10-a.sh", root.join("link-ok.sh")).expect("symlink");
            std::os::unix::fs::symlink("no-such-target", root.join("dangling.sh"))
                .expect("symlink");
        }
        Self { dir }
    }

    fn check(&self, matching: bool, patterns: &[&OsStr]) {
        let dir = self.dir.path().as_os_str();
        let (shell_code, shell_out) = shell_family(dir, matching, patterns);
        let owned: Vec<Vec<u8>> = patterns.iter().map(|p| raw_bytes(p)).collect();
        let borrowed: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let rust = family_files(Some(self.dir.path()), &borrowed).expect("arity ok");
        let mut rust_out = Vec::new();
        for path in &rust {
            rust_out.extend_from_slice(&raw_bytes(path.as_os_str()));
            rust_out.push(b'\n');
        }
        assert_eq!(
            (0, rust_out),
            (shell_code, shell_out),
            "family divergence matching={matching} patterns={patterns:?}"
        );
    }
}

#[test]
fn rust_matches_shell_on_family_stream() {
    let fixture = Fixture::build();
    // Plain stream, no patterns.
    fixture.check(false, &[]);
    // Pattern pre-filtering (applied before winner selection).
    for patterns in [
        vec![],
        vec![OsStr::new("*.sh")],
        vec![OsStr::new("*.txt")],
        vec![OsStr::new("05-group.replace/*")],
        vec![OsStr::new("*")],
        vec![OsStr::new("nomatch*")],
        vec![OsStr::new("10-*"), OsStr::new("*-b.sh")],
        vec![OsStr::new("02-high.sh")],
        vec![OsStr::new("*.replace")],
        vec![OsStr::new("a|b")],
        vec![OsStr::new("[12]0-*")],
    ] {
        fixture.check(true, &patterns);
    }
}

#[test]
fn missing_and_file_directories_are_empty() {
    let missing = std::ffi::OsStr::new("/nonexistent-family-dir-xyz");
    assert_eq!(shell_family(missing, false, &[]), (0, Vec::new()));
    assert_eq!(
        family_files(Some(std::path::Path::new(missing)), &[]),
        Ok(Vec::new())
    );
    let file = std::env::temp_dir().join("dot-family-file-probe");
    std::fs::write(&file, b"x").expect("write");
    let (code, out) = shell_family(file.as_os_str(), false, &[]);
    assert_eq!((code, out), (0, Vec::new()));
    assert_eq!(family_files(Some(&file), &[]), Ok(Vec::new()));
    std::fs::remove_file(&file).expect("cleanup");
}

/// Pin [`dot::glob::matches`] against the true oracle — bash `case`
/// with the pattern arriving via a variable, exactly like
/// `_dot_family_key_matches`. Whatever bash says here rules; the
/// unit tests in `src/glob.rs` must agree with this matrix.
/// Unix-only: byte-exact argv have no portable spelling.
#[test]
#[cfg(unix)]
fn glob_exotics_match_shell_case() {
    use dot::glob::matches;
    // (pattern, key) pairs; the verdict comes from bash at runtime.
    // Rust byte literals are exact here (no shell quoting layer), so
    // `\\` below is one real backslash and `\\\\` is two.
    let pairs: &[(&[u8], &[u8])] = &[
        // Descending ranges are void, endpoints included.
        (b"[c-a]", b"a"),
        (b"[c-a]", b"c"),
        (b"[c-a]", b"b"),
        // Leading dashes stage: `[--0]` spans.
        (b"[--0]", b"-"),
        (b"[--0]", b"."),
        (b"[--0]", b"0"),
        (b"[--0]", b"1"),
        // Escapes contribute the escaped char as a member.
        (b"[\\]]", b"]"),
        (b"[\\]]", b"\\"),
        (b"[a\\]c]", b"]"),
        (b"[a\\]c]", b"a]c"),
        (b"[a\\bc]", b"b"),
        (b"[\\\\]", b"\\"),
        (b"[a\\\\c]", b"\\"),
        (b"[a\\\\c]", b"a"),
        (b"[a\\\\-c]", b"a"),
        (b"[a\\\\-c]", b"\\"),
        (b"[a\\\\-c]", b"b"),
        (b"[a\\\\-c]", b"-"),
        (b"[a\\\\-c]", b"c"),
        (b"[\\--0]", b"-"),
        (b"[\\--0]", b"."),
        (b"[\\--0]", b"0"),
        (b"[\\--0]", b"\\"),
        // Post-range dashes: literal after good ranges ...
        (b"[a-c-e-g]", b"b"),
        (b"[a-c-e-g]", b"-"),
        (b"[a-c-e-g]", b"f"),
        (b"[a-c-e-g]", b"d"),
        (b"[a-c-]", b"-"),
        (b"[a-c--d]", b"-"),
        (b"[a-c--d]", b"."),
        (b"[a-c--d]", b"d"),
        // ... shadowed after void ones ...
        (b"[\\\\--0]", b"-"),
        (b"[\\\\--0]", b"."),
        (b"[\\\\--0]", b"0"),
        (b"[c-A--b]", b"-"),
        (b"[c-A--b]", b"."),
        (b"[c-A--b]", b"b"),
        (b"[c-A---b]", b"-"),
        (b"[c-A---b]", b"."),
        (b"[c-A---b]", b"b"),
        // Byte semantics under LC_ALL=C: `?` and class members are
        // single bytes, so a two-byte UTF-8 char needs two of them.
        (b"?", "é".as_bytes()),
        (b"??", "é".as_bytes()),
        ("[é]".as_bytes(), "é".as_bytes()),
        ("*é*".as_bytes(), "café".as_bytes()),
        // Empty pattern matches only the empty text.
        (b"", b""),
        (b"", b"a"),
        // Backtracking across classes and stars.
        (b"*a*b", b"aab"),
        (b"a*b*c", b"abc"),
        (b"a*b*c", b"axbyc"),
        (b"a*b*c", b"ac"),
        (b"[ab]*[cd]", b"axd"),
        (b"[ab]*[cd]", b"axe"),
        (b"*[*]*", b"a[b"),
        (b"*[*]*", b"ab"),
        // Everyday shapes.
        (b"a\\", b"a\\"),
        (b"a\\", b"ab"),
        (b"[ab", b"[ab"),
        (b"[ab", b"a"),
        (b"*.*", b"x.tmp.1"),
        (b"*.tmp.*", b"x.tmp"),
        (b"?", b""),
        (b"[!a]", b"b"),
        (b"[^a]", b"a"),
        (b"[]a]", b"]"),
        (b"[a-]", b"-"),
        (b"[-a]", b"."),
        (b"a|b", b"a"),
        (b"**", b"anything"),
        (b"\\*\\?\\[", b"*?["),
    ];
    for (pattern, key) in pairs {
        let output = Command::new(bash_bin())
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg("key=$1; pat=$2; case $key in $pat) exit 0;; *) exit 1;; esac")
            .arg("dot-test-sh")
            .arg(os_arg(key))
            .arg(os_arg(pattern))
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("spawn bash");
        let shell = output.status.code() == Some(0);
        assert_eq!(
            matches(pattern, key),
            shell,
            "glob divergence pattern={pattern:?} key={key:?}",
        );
    }
}

/// Build an argv element from raw bytes (Unix-only: byte-exact argv
/// have no portable spelling).
#[cfg(unix)]
fn os_arg(bytes: &[u8]) -> &OsStr {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes)
}

#[test]
#[cfg(unix)]
fn non_utf8_name_is_byte_exact() {
    let fixture = Fixture::build();
    let dir = fixture.dir.path().as_os_str();
    let (_, shell_out) = shell_family(dir, true, &[OsStr::new("bad*")]);
    let mut expected = raw_bytes(dir);
    expected.extend_from_slice(b"/bad\xffname.sh\n");
    assert_eq!(shell_out, expected, "shell fixture sanity");
    let patterns: Vec<&[u8]> = vec![b"bad*"];
    let rust = family_files(Some(fixture.dir.path()), &patterns).expect("ok");
    assert_eq!(rust.len(), 1);
    assert_eq!(
        raw_bytes(rust[0].as_os_str()),
        expected[..expected.len() - 1]
    );
}
