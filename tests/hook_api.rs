//! Native hook API contracts plus one hermetic shell boundary for the public,
//! versioned hook runtime ABI.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot_test_support::TempDir;

fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parents");
    std::fs::write(&path, bytes).expect("write");
    path
}

fn trust(home: &Path, extensions: &Path) -> dot::extension_trust::Inputs {
    dot::extension_trust::Inputs {
        euid: dot::temp::current_uid().expect("uid"),
        home: home.to_string_lossy().into_owned(),
        extensions_dir: extensions.to_string_lossy().into_owned(),
        manifest: String::new(),
        retiring_root: String::new(),
    }
}

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
    for (relative, valid) in rows {
        use std::os::unix::ffi::OsStrExt as _;
        assert_eq!(
            dot::hook_api::relative_valid(OsStr::from_bytes(relative)),
            *valid,
            "{relative:?}"
        );
    }
}

#[test]
fn hook_file_requires_a_trusted_regular_extension() {
    let dir = TempDir::new("hook-file-native").expect("fixture");
    let home = dir.path().join("home");
    let ext = dir.path().join("ext");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(ext.join("lib")).expect("ext");
    let good = stage(&ext, "hook.sh", b"good");
    let bad = stage(&ext, "loose.sh", b"bad");
    std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o644)).expect("mode");
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o666)).expect("mode");
    let inputs = trust(&home, &ext);
    let retiring: &[String] = &[];
    assert_eq!(
        dot::hook_api::hook_file(OsStr::new("hook.sh"), &inputs, retiring).expect("trusted"),
        good
    );
    assert!(dot::hook_api::hook_source_path(OsStr::new("hook.sh"), &inputs, retiring).is_ok());
    assert!(dot::hook_api::hook_file(OsStr::new("loose.sh"), &inputs, retiring).is_err());
    assert!(dot::hook_api::hook_file(OsStr::new("gone.sh"), &inputs, retiring).is_err());
}

#[test]
fn hook_family_helpers_preserve_order_filters_and_marker_identity() {
    let dir = TempDir::new("hook-family-native").expect("fixture");
    let root = dir.path().join("merge-hooks.d");
    let family = OsStr::new("demo");
    let family_dir = root.join("demo");
    let first = stage(&family_dir, "10-a", b"a");
    let loser = stage(&family_dir, "grp.replace/05-low", b"x");
    let winner = stage(&family_dir, "grp.replace/09-winner", b"w");
    assert_eq!(dot::hook_api::hook_family_dir(&root, family), family_dir);
    let files = dot::hook_api::hook_family_files(&root, family).expect("files");
    assert_eq!(files, vec![first.clone(), winner.clone()]);
    assert!(!files.contains(&loser));
    assert_eq!(
        dot::hook_api::hook_family_files_matching(&root, family, &[b"1*"]).expect("matching"),
        vec![first]
    );
    assert_eq!(
        dot::hook_api::hook_family_relpath(&root, family, &winner),
        "grp.replace/09-winner"
    );
    assert_eq!(
        dot::hook_api::hook_family_marker_name(&root, family, &winner),
        "grp.replace_09-winner"
    );
}

#[test]
fn hook_expand_home_expands_only_documented_placeholders() {
    let home = "/home/tester";
    for (value, expected) in [
        ("$HOME/.ssh", "/home/tester/.ssh"),
        ("${HOME}/.ssh", "/home/tester/.ssh"),
        ("~", "/home/tester"),
        ("~/doc", "/home/tester/doc"),
        ("~other", "~other"),
        ("plain", "plain"),
    ] {
        assert_eq!(dot::hook_api::expand_home(value, home), expected);
    }
}

#[test]
fn hook_platform_and_host_filters_have_literal_inclusion_precedence() {
    let termux = "/data/data/com.termux/files/usr";
    assert!(dot::hook_api::is_termux(termux));
    assert!(!dot::hook_api::is_termux("/usr"));
    for (spec, prefix, expected) in [
        ("", "", true),
        ("linux", "", true),
        ("macos", "", false),
        ("wsl", "", false),
        ("android", "", false),
        ("!linux", "", false),
        ("linux,!macos", "", true),
        ("android", termux, true),
        ("!android", termux, false),
        ("linux", termux, true),
        ("linux,macos", termux, true),
        ("!macos", termux, true),
        ("!linux", termux, false),
        ("!android,!macos", termux, false),
        ("linux,!android", termux, false),
        ("android,!linux", termux, false),
        ("macos,!freebsd", termux, false),
    ] {
        assert_eq!(
            dot::hook_api::hook_platform_match(Some(spec), "linux", prefix).expect("match"),
            expected
        );
    }
    assert!(dot::hook_api::hook_host_match(Some("FIXTURE-HOST"), "fixture-host").expect("host"));
    assert!(
        dot::hook_api::hook_host_match(Some("fixture-host,!other"), "fixture-host").expect("host")
    );
    assert!(!dot::hook_api::hook_host_match(Some("!fixture-host"), "fixture-host").expect("host"));
    assert!(dot::hook_api::hook_platform_match(None, "linux", "").is_err());
    assert!(dot::hook_api::hook_host_match(None, "host").is_err());
}

#[test]
fn hook_tool_presence_distinguishes_path_lookup_and_explicit_paths() {
    let dir = TempDir::new_exec("hook-tool-native").expect("fixture");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let tool = stage(&bin, "have", b"#!/bin/sh\n");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("mode");
    stage(&bin, "plain", b"x");
    let path = bin.to_string_lossy();
    assert!(dot::platform::tool_present(Some("have"), &path).expect("tool"));
    assert!(dot::platform::tool_present(Some("plain"), &path).expect("plain"));
    assert!(!dot::platform::tool_present(Some("missing"), &path).expect("missing"));
    assert!(
        dot::hook_api::tool_present_live(Some(tool.to_str().expect("tool utf8"))).expect("slash")
    );
    assert!(dot::hook_api::tool_present_live(None).is_err());
}

#[test]
fn public_hook_runtime_exports_literal_abi_results_and_usage_codes() {
    let dir = TempDir::new_exec("hook-public-runtime").expect("fixture");
    let home = dir.path().join("home");
    let config = dir.path().join("config");
    let extensions = dir.path().join("extensions");
    let family = config.join("dot/merge-hooks.d/fixture");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&extensions).expect("extensions");
    let member = stage(&family, "10-one.txt", b"one\n");
    let root = env!("CARGO_MANIFEST_DIR");
    let script = r#"
set -euo pipefail
. "$1/lib/dot/public/xdg.sh"
. "$1/lib/dot/public/hook-runtime-v1/merge-hooks.sh"
. "$1/lib/dot/public/hook-runtime-v1/merge-block.sh"
. "$1/lib/dot/public/hook-runtime-v1/hook-api.sh"
printf 'home=%s\n' "$(dot_expand_home '$HOME/example')"
printf 'tilde=%s\n' "$(dot_expand_home '~/example')"
printf 'family=%s\n' "$(dot_hook_family fixture)"
printf 'files=%s\n' "$(dot_hook_family_files fixture)"
printf 'matching=%s\n' "$(dot_hook_family_files_matching fixture '*.txt')"
printf 'relative=%s\n' "$(dot_hook_family_relpath fixture "$2")"
printf 'marker=%s\n' "$(dot_hook_family_marker_name fixture "$2")"
_dot_hook_platform() { printf 'linux\n'; }
_dot_hook_host() { printf 'fixture-host\n'; }
dot_hook_platform_match linux
printf 'platform-linux=%s\n' "$?"
if dot_hook_platform_match macos; then code=0; else code=$?; fi
printf 'platform-macos=%s\n' "$code"
dot_hook_host_match FIXTURE-HOST
printf 'host-include=%s\n' "$?"
if dot_hook_host_match '!fixture-host'; then code=0; else code=$?; fi
printf 'host-exclude=%s\n' "$code"
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
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            script,
            "dot-hook-runtime",
            root,
        ])
        .arg(&member)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("DOT_EXTENSIONS_DIR", &extensions)
        .env("DOT_SOURCE_ROOT", root)
        .env("DOT_TEST", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run public hook runtime");
    assert!(
        output.status.success(),
        "public runtime failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(output.stderr.is_empty());
}
