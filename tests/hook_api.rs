//! Differential parity tests for the hook-API layer
//! (`lib/dot/hook-api.sh`) against the live shell: support-file
//! validation, family resolution, home expansion, platform/host
//! matchers (including Termux), and tool lookup.
//!
//! Separate binary because the rows drive the shell oracle: each
//! side shares one fixture directory (paths compare verbatim), the
//! environment stays scrubbed (`env_clear` plus `LC_ALL=C`), and the
//! only process-global mutation (the `PATH` swap for the live tool
//! probe) holds a serial lock.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::test_support::TempDir;

/// Sources for the hook-API chapter.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/families.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/merge-hooks.sh\"\n",
    ". \"$1/lib/dot/merge-block.sh\"\n",
    ". \"$1/lib/dot/platform.sh\"\n",
    ". \"$1/lib/dot/extension-trust.sh\"\n",
    ". \"$1/lib/dot/hook-api.sh\"\n",
);

/// Run one shell snippet with the hook-API runtime sourced. `argv`
/// arrives as `$2..` (byte-exact, for newline relatives);
/// `extra_env` sets (`Some`) or removes (`None`) variables.
/// Returns exit code, stdout, and stderr.
fn shell_run(
    fixture: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
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
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Trust inputs for an extensions root: no manifest involvement for
/// regular files, so empty manifest fields stay empty.
fn trust_inputs(home: &Path, extensions: &Path) -> dot::extension_trust::Inputs {
    dot::extension_trust::Inputs {
        euid: dot::temp::current_uid().expect("uid"),
        home: home.to_string_lossy().into_owned(),
        extensions_dir: extensions.to_string_lossy().into_owned(),
        manifest: String::new(),
        retiring_root: String::new(),
    }
}

#[test]
fn hook_relative_shape_matrix_agrees() {
    let dir = TempDir::new("hookapi-shape").expect("fixture dir");
    let root = dir.path();
    let home = root.join("home");
    let extensions = root.join("ext");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::create_dir_all(&extensions).expect("ext dir");
    let home_text = home.to_string_lossy().into_owned();
    let ext_text = extensions.to_string_lossy().into_owned();
    let env = [
        ("HOME", Some(home_text.as_str())),
        ("DOT_EXTENSIONS_DIR", Some(ext_text.as_str())),
    ];
    // (relative bytes, malformed shape)
    let rows: &[(&[u8], bool)] = &[
        (b"", true),
        (b"/", true),
        (b"/abs", true),
        (b".", true),
        (b"..", true),
        (b"./x", true),
        (b"../x", true),
        (b"a/./b", true),
        (b"a/../b", true),
        (b"a/.", true),
        (b"a/..", true),
        (b"a/", true),
        (b"a//b", true),
        (b"a\nb", true),
        (b"a\rb", true),
        (b"hook.sh", false),
        (b"a/b.sh", false),
        (b".hidden", false),
        (b"..a", false),
        (b"a..b", false),
        (b"a b", false),
        (b"$HOME-x", false),
    ];
    for (relative, malformed) in rows {
        let arg = OsStr::from_bytes(relative);
        let (code, out, _) = shell_run(
            root,
            &[arg],
            &env,
            "dot_hook_file \"$2\"; printf 'code=%s\\n' \"$?\"",
        );
        assert_eq!(code, 0, "harness exit for {relative:?}");
        let shell = String::from_utf8(out).expect("shape text");
        // Well-formed relatives miss their file here, so the shell
        // refuses (1); malformed ones are usage errors (2).
        let want = if *malformed { "code=2\n" } else { "code=1\n" };
        assert_eq!(shell, want, "shell shape for {relative:?}");
        assert_eq!(
            dot::hook_api::relative_valid(arg),
            !malformed,
            "rust shape for {relative:?}"
        );
    }
    // Arity is shell-only (the Rust signature cannot mis-fire), but
    // the exit-2 contract still pins here.
    for snippet in [
        "dot_hook_file; printf 'code=%s\\n' \"$?\"",
        "dot_hook_file a b; printf 'code=%s\\n' \"$?\"",
    ] {
        let (code, out, _) = shell_run(root, &[], &env, snippet);
        assert_eq!(code, 0, "arity harness for {snippet:?}");
        assert_eq!(
            String::from_utf8(out).expect("arity text"),
            "code=2\n",
            "arity contract for {snippet:?}"
        );
    }
}

#[test]
fn hook_file_trust_agrees() {
    let dir = TempDir::new("hookapi-file").expect("fixture dir");
    let root = dir.path();
    let home = root.join("home");
    let extensions = root.join("ext");
    std::fs::create_dir_all(home.join("x")).expect("home dir");
    std::fs::create_dir_all(extensions.join("lib")).expect("ext dirs");
    std::fs::set_permissions(&extensions, std::fs::Permissions::from_mode(0o755))
        .expect("ext mode");
    std::fs::set_permissions(
        extensions.join("lib"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("lib mode");
    let home_text = home.to_string_lossy().into_owned();
    let ext_text = extensions.to_string_lossy().into_owned();
    let env = [
        ("HOME", Some(home_text.as_str())),
        ("DOT_EXTENSIONS_DIR", Some(ext_text.as_str())),
    ];
    let trusted = stage(&extensions, "hook.sh", b"helper\n");
    let nested = stage(&extensions, "lib/helper.sh", b"nested\n");
    let refused = stage(&extensions, "loose.sh", b"loose\n");
    for path in [&trusted, &nested, &refused] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("file mode");
    }
    // Group-writable content fails the trust walk on both sides.
    std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o666)).expect("loose mode");
    let inputs = trust_inputs(&home, &extensions);
    let empty: &[String] = &[];
    // (relative, shell code, rust ok)
    let rows: &[(&str, i32, bool)] = &[
        ("hook.sh", 0, true),
        ("lib/helper.sh", 0, true),
        ("loose.sh", 1, false),
        ("gone.sh", 1, false),
    ];
    for (relative, want_code, want_ok) in rows {
        let (code, out, _) = shell_run(
            root,
            &[OsStr::new(relative)],
            &env,
            "dot_hook_file \"$2\"; c=$?; if [[ $c -eq 0 ]]; then printf 'code=0 reply=%s\\n' \"$REPLY\"; else printf 'code=%s\\n' \"$c\"; fi",
        );
        assert_eq!(code, 0, "harness exit for {relative:?}");
        let shell = String::from_utf8(out).expect("file text");
        let rust = dot::hook_api::hook_file(OsStr::new(relative), &inputs, empty);
        assert_eq!(rust.is_ok(), *want_ok, "rust trust for {relative:?}");
        if *want_ok {
            let path = rust.expect("trusted path");
            assert_eq!(shell, format!("code=0 reply={}\n", path.display()));
            // The source half resolves the same validated path; the
            // actual sourcing stays shell-side by design.
            let sourced = dot::hook_api::hook_source_path(OsStr::new(relative), &inputs, empty)
                .expect("source path");
            assert_eq!(sourced, path, "source path for {relative:?}");
        } else {
            assert_eq!(
                shell,
                format!("code={want_code}\n"),
                "shell trust for {relative:?}"
            );
            assert_eq!(
                rust.expect_err("refused"),
                dot::extension_trust::Error::Refused,
                "rust refusal for {relative:?}"
            );
        }
    }
}

#[test]
fn hook_family_helpers_agree() {
    let dir = TempDir::new("hookapi-family").expect("fixture dir");
    let root = dir.path();
    let home = root.join("home");
    let xdg = root.join("xdg");
    let hooks = xdg.join("dot/merge-hooks.d/demo");
    stage(&hooks, "10-a", b"a\n");
    stage(&hooks, "20-b", b"b\n");
    stage(&hooks, "grp.replace/05-w", b"loser\n");
    stage(&hooks, "grp.replace/09-winner", b"winner\n");
    stage(&hooks, "skip~", b"artifact\n");
    let home_text = home.to_string_lossy().into_owned();
    let xdg_text = xdg.to_string_lossy().into_owned();
    let env = [
        ("HOME", Some(home_text.as_str())),
        ("XDG_CONFIG_HOME", Some(xdg_text.as_str())),
    ];
    let fam_text = hooks.to_string_lossy().into_owned();
    let snippet = format!(
        "printf 'fam=%s\\n' \"$(dot_hook_family demo)\"\n\
         dot_hook_family_files demo\n\
         printf '###\\n'\n\
         dot_hook_family_files_matching demo '1*' 'g*'\n\
         printf '###\\n'\n\
         dot_hook_family_relpath demo {}\n\
         dot_hook_family_marker_name demo {}\n\
         dot_family_relpath demo {}\n\
         dot_family_relpath demo '/elsewhere/file'\n",
        sq(&format!("{fam_text}/grp.replace/09-winner")),
        sq(&format!("{fam_text}/grp.replace/09-winner")),
        sq(&format!("{fam_text}/grp.replace/09-winner")),
    );
    let (code, out, _) = shell_run(root, &[], &env, &snippet);
    assert_eq!(code, 0, "shell family helpers");
    let shell = String::from_utf8(out).expect("family text");
    let hooks_root = dot::merge_hooks::hook_dir(&xdg_text, &home_text).expect("rust hooks root");
    let family = OsStr::new("demo");
    let mut rust = format!("fam={}\n", hooks_root.join("demo").display());
    for path in dot::hook_api::hook_family_files(&hooks_root, family).expect("rust files") {
        rust.push_str(&format!("{}\n", path.display()));
    }
    rust.push_str("###\n");
    for path in dot::hook_api::hook_family_files_matching(&hooks_root, family, &[b"1*", b"g*"])
        .expect("rust matching")
    {
        rust.push_str(&format!("{}\n", path.display()));
    }
    rust.push_str("###\n");
    let winner = hooks.join("grp.replace/09-winner");
    rust.push_str(&format!(
        "{}\n{}\n{}\n{}\n",
        dot::hook_api::hook_family_relpath(&hooks_root, family, &winner).to_string_lossy(),
        dot::hook_api::hook_family_marker_name(&hooks_root, family, &winner).to_string_lossy(),
        dot::hook_api::family_relpath(&hooks, &winner).to_string_lossy(),
        dot::hook_api::family_relpath(&hooks, Path::new("/elsewhere/file")).to_string_lossy(),
    ));
    assert_eq!(rust, shell, "family helper parity");
    assert_eq!(
        dot::hook_api::hook_family_dir(&hooks_root, family),
        hooks,
        "family dir join"
    );
}

#[test]
fn hook_expand_home_agrees() {
    let dir = TempDir::new("hookapi-expand").expect("fixture dir");
    let root = dir.path();
    let home = "/home/tester";
    for value in [
        "$HOME/.ssh",
        "${HOME}/.ssh",
        "~",
        "~/doc",
        "~other",
        "plain",
    ] {
        let (code, out, _) = shell_run(
            root,
            &[OsStr::new(value)],
            &[("HOME", Some(home))],
            "dot_expand_home \"$2\"",
        );
        assert_eq!(code, 0, "shell expand {value:?}");
        let shell = String::from_utf8(out).expect("expand text");
        assert_eq!(
            format!("{}\n", dot::hook_api::expand_home(value, home)),
            shell,
            "expand parity for {value:?}"
        );
    }
}

#[test]
fn hook_platform_host_match_agrees() {
    let dir = TempDir::new("hookapi-match").expect("fixture dir");
    let root = dir.path();
    let termux = "/data/data/com.termux/files/usr";
    assert!(dot::hook_api::is_termux(termux));
    assert!(!dot::hook_api::is_termux(""));
    assert!(!dot::hook_api::is_termux("/usr"));
    assert!(!dot::hook_api::is_termux("com.termux"));
    // (matcher, filter, prefix, want code)
    let rows: &[(&str, &str, &str, i32)] = &[
        ("platform", "", "", 0),
        ("platform", "linux", "", 0),
        ("platform", "macos", "", 1),
        ("platform", "!linux", "", 1),
        ("platform", "linux,!macos", "", 0),
        ("platform", "LINUX", "", 1),
        ("platform", "android", termux, 0),
        ("platform", "android", "", 1),
        ("platform", "!android", termux, 1),
        ("platform", "!android", "", 0),
        ("platform", "linux,!android", termux, 1),
        ("host", "", "", 0),
        ("host", "fixture-host", "", 0),
        ("host", "FIXTURE-HOST", "", 0),
        ("host", "!fixture-host", "", 1),
        ("host", "other", "", 1),
    ];
    for (matcher, filter, prefix, want) in rows {
        // Stubs mirror the real detectors' canonicalization
        // (`_dot_hook_host` lowercases; the spec side folds in
        // `_dot_hook_match_specs`, which the `FIXTURE-HOST` row
        // exercises).
        let stub = if *matcher == "platform" {
            "_dot_hook_platform() { printf 'linux\\n'; }; dot_hook_platform_match \"$2\""
        } else {
            "_dot_hook_host() { printf 'fixture-host\\n'; }; dot_hook_host_match \"$2\""
        };
        let prefix_env: Vec<(&str, Option<&str>)> = if prefix.is_empty() {
            vec![("PREFIX", None)]
        } else {
            vec![("PREFIX", Some(prefix))]
        };
        // Run the stubbed matcher once, reporting through its exit
        // code: 0 prints match, 1 prints miss.
        let (code, out, _) = shell_run(
            root,
            &[OsStr::new(filter)],
            &prefix_env,
            &format!("if {stub}; then printf '0'; else printf '1'; fi"),
        );
        assert_eq!(code, 0, "harness exit for {matcher} [{filter}]");
        let shell_digit = String::from_utf8(out).expect("digit text");
        let rust_ok = if *matcher == "platform" {
            dot::hook_api::hook_platform_match(Some(filter), "linux", prefix)
                .expect("rust platform match")
        } else {
            dot::hook_api::hook_host_match(Some(filter), "fixture-host").expect("rust host match")
        };
        assert_eq!(
            shell_digit,
            if rust_ok { "0" } else { "1" },
            "{matcher} parity for [{filter}] prefix=[{prefix}]"
        );
        assert_eq!(
            (if rust_ok { 0 } else { 1 }),
            *want,
            "{matcher} expectation for [{filter}] prefix=[{prefix}]"
        );
    }
    // Missing specs are usage errors on both sides.
    let (code, out, _) = shell_run(
        root,
        &[],
        &[],
        "dot_hook_platform_match; printf '%s' \"$?\"",
    );
    assert_eq!(code, 0, "platform arity harness");
    assert_eq!(String::from_utf8(out).expect("arity text"), "2");
    assert_eq!(
        dot::hook_api::hook_platform_match(None, "linux", ""),
        Err(dot::platform::Error::Usage)
    );
    let (code, out, _) = shell_run(root, &[], &[], "dot_hook_host_match; printf '%s' \"$?\"");
    assert_eq!(code, 0, "host arity harness");
    assert_eq!(String::from_utf8(out).expect("arity text"), "2");
    assert_eq!(
        dot::hook_api::hook_host_match(None, "h"),
        Err(dot::platform::Error::Usage)
    );
}

#[test]
fn hook_tool_present_agrees() {
    let dir = TempDir::new_exec("hookapi-tool").expect("fixture dir");
    let root = dir.path();
    let bindir = root.join("bin");
    std::fs::create_dir_all(&bindir).expect("bin dir");
    let have = bindir.join("dot-hook-test-have-tool");
    std::fs::write(&have, "#!/bin/sh\necho have\n").expect("have tool");
    std::fs::set_permissions(&have, std::fs::Permissions::from_mode(0o755)).expect("have mode");
    let plain = bindir.join("dot-hook-test-plain-file");
    std::fs::write(&plain, "plain\n").expect("plain file");
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("plain mode");
    let subdir = bindir.join("dot-hook-test-subdir");
    std::fs::create_dir_all(&subdir).expect("sub dir");
    let bin_text = bindir.to_string_lossy().into_owned();
    let path_env = vec![("PATH", Some(bin_text.as_str()))];
    let missing = bindir.join("dot-hook-test-no-such-tool");
    assert!(!missing.exists(), "absence row really absent");
    // Isolated `PATH`: the pure entry point against the fixture bin
    // versus the shell oracle with the same `PATH`.
    for name in [
        "dot-hook-test-have-tool",
        "dot-hook-test-plain-file",
        "dot-hook-test-no-such-tool",
    ] {
        let (code, out, _) = shell_run(
            root,
            &[OsStr::new(name)],
            &path_env,
            "if dot_tool_present \"$2\"; then printf '0'; else printf '1'; fi",
        );
        assert_eq!(code, 0, "harness exit for {name}");
        let shell_digit = String::from_utf8(out).expect("tool text");
        let rust = dot::platform::tool_present(Some(name), &bin_text).expect("pure probe");
        assert_eq!(
            shell_digit,
            if rust { "0" } else { "1" },
            "tool parity for {name}"
        );
    }
    // Live wrapper under the inherited `PATH`: the harness passes
    // the parent `PATH` through by default, exactly what the live
    // probe reads, so agreement is structural (no claim about which
    // tools exist, only that both engines agree).
    for name in ["sh", "dot-hook-test-no-such-tool"] {
        let (code, out, _) = shell_run(
            root,
            &[OsStr::new(name)],
            &[],
            "if dot_tool_present \"$2\"; then printf '0'; else printf '1'; fi",
        );
        assert_eq!(code, 0, "live harness exit for {name}");
        let shell_digit = String::from_utf8(out).expect("live tool text");
        let rust = dot::hook_api::tool_present_live(Some(name)).expect("live probe");
        assert_eq!(
            shell_digit,
            if rust { "0" } else { "1" },
            "live tool parity for {name}"
        );
    }
    // Slash spellings are existence probes, PATH-independent.
    for (path, want) in [
        (have.to_string_lossy().into_owned(), true),
        (missing.to_string_lossy().into_owned(), false),
        (subdir.to_string_lossy().into_owned(), true),
    ] {
        let (code, out, _) = shell_run(
            root,
            &[OsStr::new(&path)],
            &[],
            "if dot_tool_present \"$2\"; then printf '0'; else printf '1'; fi",
        );
        assert_eq!(code, 0, "slash harness for {path}");
        let shell_digit = String::from_utf8(out).expect("slash text");
        let rust = dot::hook_api::tool_present_live(Some(&path)).expect("rust slash probe");
        assert_eq!(rust, want, "rust slash expectation for {path}");
        assert_eq!(shell_digit, if want { "0" } else { "1" });
    }
    // Empty and missing names are usage errors on both sides.
    let (code, out, _) = shell_run(
        root,
        &[],
        &[],
        "dot_tool_present ''; printf '%s' \"$?\"; dot_tool_present; printf '%s' \"$?\"",
    );
    assert_eq!(code, 0, "tool arity harness");
    assert_eq!(String::from_utf8(out).expect("arity text"), "22");
    assert_eq!(
        dot::hook_api::tool_present_live(Some("")),
        Err(dot::platform::Error::Usage)
    );
    assert_eq!(
        dot::hook_api::tool_present_live(None),
        Err(dot::platform::Error::Usage)
    );
}
