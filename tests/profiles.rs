//! Differential parity tests for profile loading and selection
//! against `lib/dot/profiles.sh` (+ `profile-format.sh`): validators,
//! definition parsing, include expansion, selector matching, and the
//! load/resolve/select entry points — including every error message.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::profiles;
use dot::test_support::TempDir;

/// Run one shell snippet with the profile libraries sourced. `argv`
/// arrives as `$2..`; `extra_env` sets (`Some`) or removes (`None`)
/// variables. Returns exit code, stdout, and stderr.
fn shell_run(
    fixture: &Path,
    argv: &[&std::ffi::OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/platform.sh\"\n. \"$1/lib/dot/profiles.sh\"\n{snippet}"
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

/// Current euid for ownership-gated checks (fixtures are self-owned).
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

/// Dump a loaded shell state in the canonical comparison shape.
const DUMP: &str = r#"printf 'present=%s\nselected=%s\nstate=%s\nincluded=%s\noverlays=%s\nuser=%s\nhost=%s\nmatches=%s\nrecords=%s\nconfig-error=%s\n' "$DOT_PROFILES_PRESENT" "$SELECTED_PROFILE" "$DOT_PROFILE_SELECTION_STATE" "$(IFS=,; printf '%s' "${INCLUDED_PROFILES[*]}")" "$(IFS=,; printf '%s' "${SELECTED_OVERLAY_NAMES[*]}")" "$DOT_PROFILE_CURRENT_USER" "$DOT_PROFILE_CURRENT_HOST" "$(IFS=,; printf '%s' "${DOT_PROFILE_SELECTOR_MATCHES[*]}")" "$(IFS='|'; printf '%s' "${DOT_PROFILE_SELECTOR_RECORDS[*]}")" "${DOT_PROFILE_CONFIGURATION_ERROR:-}""#;

/// Normalize the per-twin fixture roots both sides embed in error
/// messages (definition paths), so twin dumps compare.
fn normalize_roots(text: &str, shell_root: &Path, rust_root: &Path) -> String {
    text.replace(&shell_root.to_string_lossy().into_owned(), "<root>")
        .replace(&rust_root.to_string_lossy().into_owned(), "<root>")
}

/// Dump a Rust [`profiles::State`] in the same shape.
fn dump(state: &profiles::State) -> String {
    format!(
        "present={}\nselected={}\nstate={}\nincluded={}\noverlays={}\nuser={}\nhost={}\nmatches={}\nrecords={}\nconfig-error={}\n",
        if state.present { "1" } else { "0" },
        state.selected,
        state.selection_state,
        state.included.join(","),
        state.overlay_names.join(","),
        state.current_user,
        state.current_host,
        state.selector_matches.join(","),
        state.selector_records.join("|"),
        state.config_error.as_deref().unwrap_or(""),
    )
}

#[test]
fn scalar_validators_agree() {
    let dir = TempDir::new("prof-valid").expect("fixture dir");
    let root = dir.path();
    let identifiers = ["base", "a", "a-b", "a1", "A", "1a", "", "a_b", "a b", "-a"];
    for name in identifiers {
        let (code, _, _) = shell_run(
            root,
            &[name.as_ref()],
            &[],
            "_dot_profile_identifier_valid \"$2\"",
        );
        assert_eq!(
            profiles::identifier_valid(name.as_bytes()),
            code == 0,
            "identifier {name:?}"
        );
    }
    let values: &[&[u8]] = &[
        b"ok",
        b"a=b",
        b"",
        b"a|b",
        b"a\tb",
        b"a\rb",
        b"\xc3\xa9",
        b"a\x00b",
    ];
    for value in values {
        let expected = !value
            .iter()
            .any(|b| matches!(b, b'|' | b'\t' | b'\n' | b'\r'));
        assert_eq!(
            profiles::value_safe(value),
            expected,
            "rust value for {value:?}"
        );
        if value.contains(&0) {
            // NUL cannot survive argv or environment; the shell side
            // is untestable here, and the Rust rule is literal.
            continue;
        }
        let lossy = String::from_utf8_lossy(value);
        let (code, _, _) = shell_run(
            root,
            &[lossy.as_ref().as_ref()],
            &[],
            "_dot_profile_value_safe \"$2\"",
        );
        assert_eq!(code == 0, expected, "shell value for {value:?}");
    }
    let users = ["amy", "_x", "a.b-c_d9", "", "1a", "a b", "-a"];
    for user in users {
        let (code, _, _) = shell_run(
            root,
            &[user.as_ref()],
            &[],
            "_dot_profile_user_valid \"$2\"",
        );
        assert_eq!(
            profiles::user_valid(user.as_bytes()),
            code == 0,
            "user {user:?}"
        );
    }
    let hosts = [
        "web1",
        "Web1.Example.COM",
        "h.",
        "",
        "-x",
        "a_b",
        "a..b",
        "a-",
    ];
    for host in hosts {
        // The oracle reports through `REPLY`, not stdout.
        let (code, out, _) = shell_run(
            root,
            &[host.as_ref()],
            &[],
            "_dot_profile_host_normalize \"$2\" && printf '%s' \"$REPLY\"",
        );
        let shell = (code == 0).then(|| String::from_utf8(out).expect("host text"));
        let rust = profiles::host_normalize(host.as_bytes());
        assert_eq!(rust.as_deref(), shell.as_deref(), "host {host:?}");
    }
}

#[test]
fn file_safe_cases_agree() {
    let dir = TempDir::new("prof-file").expect("fixture dir");
    let root = dir.path();
    let big = vec![b'x'; 65537];
    let exact = vec![b'y'; 65536];
    let cases: &[(&str, Option<&[u8]>)] = &[
        ("ok.conf", Some(b"version=1\noverlays=x\n")),
        ("empty.conf", Some(b"")),
        ("exact.conf", Some(&exact)),
        ("big.conf", Some(&big)),
        ("nul.conf", Some(b"a\x00b\n")),
        ("tab.conf", Some(b"a\tb\n")),
        ("cr.conf", Some(b"a\rb\n")),
        ("del.conf", Some(b"a\x7fb\n")),
        ("high.conf", Some(b"\xc3\xa9\n")),
        ("missing.conf", None),
    ];
    stage(root, "adir/child", b"x");
    std::os::unix::fs::symlink("ok.conf", root.join("link.conf")).expect("symlink");
    for (name, body) in cases {
        let path = match body {
            Some(body) => stage(root, name, body),
            None => root.join(name),
        };
        let (code, _, serr) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_dot_profile_file_safe \"$2\"",
        );
        let shell_err = String::from_utf8_lossy(&serr).into_owned();
        let rust = profiles::file_safe(&path);
        assert_eq!(rust.is_ok(), code == 0, "file_safe code for {name}");
        let rust_err = rust
            .err()
            .map(|error| format!("dot: profile: {}\n", error.message));
        assert_eq!(
            rust_err.unwrap_or_default(),
            shell_err,
            "file_safe message for {name}"
        );
    }
    for (name, arg) in [("dir", "adir"), ("symlink", "link.conf")] {
        let path = root.join(arg);
        let (code, _, serr) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_dot_profile_file_safe \"$2\"",
        );
        assert_ne!(code, 0, "shell rejects {name}");
        assert!(profiles::file_safe(&path).is_err(), "rust rejects {name}");
        assert!(
            String::from_utf8_lossy(&serr).contains("not a regular file"),
            "message for {name}"
        );
    }
}

#[test]
fn ownership_gates_agree() {
    let dir = TempDir::new("prof-owned").expect("fixture dir");
    let root = dir.path();
    let uid = euid();
    let file = stage(root, "f", b"x");
    let sub = root.join("sub");
    std::fs::create_dir(&sub).expect("mkdir");
    std::os::unix::fs::symlink("f", root.join("l")).expect("symlink");
    for (label, path, mode) in [
        ("file-600", file.clone(), 0o600),
        ("file-644", file.clone(), 0o644),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        let (code, _, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_dot_profile_private_path_safe \"$2\"",
        );
        assert_eq!(
            profiles::private_path_safe(&path, uid),
            code == 0,
            "private {label}"
        );
    }
    // (private expectation, owned expectation) per path: links,
    // files, and missing paths fail both; the owned directory
    // passes the directory gate only.
    for (label, name, private, owned) in [
        ("link", "l", false, false),
        ("dir", "sub", false, true),
        ("missing", "nope", false, false),
    ] {
        let path = root.join(name);
        let (code, _, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_dot_profile_private_path_safe \"$2\"",
        );
        assert_eq!(code == 0, private, "shell private {label}");
        assert_eq!(
            profiles::private_path_safe(&path, uid),
            private,
            "rust private {label}"
        );
        let (code, _, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_dot_profile_owned_directory_safe \"$2\"",
        );
        assert_eq!(code == 0, owned, "shell owned {label}");
        assert_eq!(
            profiles::owned_directory_safe(&path, uid),
            owned,
            "rust owned {label}"
        );
    }
}

#[test]
fn definition_errors_agree() {
    let dir = TempDir::new("prof-parse").expect("fixture dir");
    let root = dir.path();
    let cases = [
        ("good", "version=1\nprofiles=base\noverlays=x\n"),
        ("comments", "# top\n\nversion=1\n# mid\noverlays=x\n"),
        ("no-trailing-nl", "version=1\noverlays=x"),
        ("continuation", "version=1\\\noverlays=x\n"),
        ("bare", "version=1\nhello\n"),
        ("bad-key", "version=1\nOverlays=x\n"),
        ("empty-key", "version=1\n=x\n"),
        ("unsafe-value", "version=1\noverlays=a|b\n"),
        ("late-version", "overlays=x\nversion=1\n"),
        ("dup-version", "version=1\nversion=1\noverlays=x\n"),
        ("bad-version", "version=2\noverlays=x\n"),
        ("dup-profiles", "version=1\nprofiles=a\nprofiles=b\n"),
        ("bad-profiles", "version=1\nprofiles=Base\n"),
        ("dotfiles-overlay", "version=1\noverlays=dotfiles\n"),
        ("unknown-key", "version=1\nfrobnicate=1\n"),
        ("missing-version", "overlays=x\n"),
        ("no-members", "version=1\n"),
        ("empty", ""),
        ("equals-value", "version=1\noverlays=x\nextra=a=b\n"),
    ];
    for (label, body) in cases {
        let path = stage(root, &format!("{label}.conf"), body.as_bytes());
        // Production arity is (file, name) with `$3` unset, so
        // `missing version` / `no members` report the path: pass
        // the file as the (otherwise unused-here) name too.
        let (code, _, serr) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_dot_profile_parse_definition \"$2\" \"$2\"",
        );
        let shell_err = String::from_utf8_lossy(&serr).into_owned();
        let rust = profiles::parse_definition(&path, body.as_bytes());
        assert_eq!(rust.is_ok(), code == 0, "parse code for {label}");
        let rust_err = rust
            .err()
            .map(|error| format!("dot: profile: {}\n", error.message));
        assert_eq!(
            rust_err.unwrap_or_default(),
            shell_err,
            "parse message for {label}"
        );
    }
}

#[test]
fn load_twins_agree() {
    // Valid tree shared by the happy paths.
    let tree: &[(&str, &[u8])] = &[
        ("base.conf", b"version=1\noverlays=core\n"),
        ("web.conf", b"version=1\nprofiles=base\noverlays=websvc\n"),
    ];
    // Happy path plus single-fault variants (one fault each keeps
    // shell hash-order nondeterminism out of the messages).
    type Fault<'a> = (&'a str, &'a [(&'a str, &'a [u8])]);
    let faults: &[Fault<'_>] = &[
        ("ok", &[]),
        ("bad-file", &[("broken.conf", b"version=1\nbogus\n")]),
        ("bad-name", &[("BAD.conf", b"version=1\noverlays=x\n")]),
        (
            "cycle",
            &[("loop.conf", b"version=1\nprofiles=loop\noverlays=x\n")],
        ),
        (
            "unknown-parent",
            &[("orphan.conf", b"version=1\nprofiles=ghost\noverlays=x\n")],
        ),
    ];
    for (label, extra) in faults {
        for default in ["base", "web"] {
            let sdir = TempDir::new(&format!("prof-load-{label}-shell")).expect("shell dir");
            let rdir = TempDir::new(&format!("prof-load-{label}-rust")).expect("rust dir");
            for (name, body) in tree.iter().chain(extra.iter()) {
                stage(sdir.path(), &format!("profiles.d/{name}"), body);
                stage(rdir.path(), &format!("profiles.d/{name}"), body);
            }
            let pd_s = sdir.path().join("profiles.d");
            let pd_r = rdir.path().join("profiles.d");
            let env = [("DOT_DEFAULT_PROFILE", Some(default))];
            let (scode, sout, _) = shell_run(
                sdir.path(),
                &[pd_s.as_os_str()],
                &env,
                &format!("_dot_profiles_load \"$2\"; printf 'rc=%s\\n' \"$?\"; {DUMP}"),
            );
            let mut state = profiles::State::default();
            let rcode = state.load(Some(&pd_r), "", "", Some(default));
            let rust = format!(
                "rc={}\n{}",
                if rcode.is_ok() { "0" } else { "1" },
                dump(&state)
            );
            let shell = String::from_utf8(sout).expect("dump text");
            assert_eq!(scode, 0, "shell harness runs for {label}/{default}");
            assert_eq!(
                normalize_roots(&rust, sdir.path(), rdir.path()),
                normalize_roots(&shell, sdir.path(), rdir.path()),
                "load parity for {label}/{default}"
            );
        }
    }
    // Missing directory is a clean empty state; a file is an error.
    for label in ["absent", "file"] {
        let sdir = TempDir::new(&format!("prof-load-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("prof-load-{label}-rust")).expect("rust dir");
        let pd_s = sdir.path().join("profiles.d");
        let pd_r = rdir.path().join("profiles.d");
        if label == "file" {
            stage(sdir.path(), "profiles.d", b"x");
            stage(rdir.path(), "profiles.d", b"x");
        }
        let (scode, sout, _) = shell_run(
            sdir.path(),
            &[pd_s.as_os_str()],
            &[],
            &format!("_dot_profiles_load \"$2\"; printf 'rc=%s\\n' \"$?\"; {DUMP}"),
        );
        let mut state = profiles::State::default();
        let rcode = state.load(Some(&pd_r), "", "", Some("base"));
        assert_eq!(scode, 0, "shell harness for {label}");
        let rust = format!(
            "rc={}\n{}",
            if rcode.is_ok() { "0" } else { "1" },
            dump(&state)
        );
        let shell = String::from_utf8(sout).expect("dump");
        assert_eq!(
            normalize_roots(&rust, sdir.path(), rdir.path()),
            normalize_roots(&shell, sdir.path(), rdir.path()),
            "load parity for {label}"
        );
    }
    // No base definition fails even when everything else is valid.
    let sdir = TempDir::new("prof-load-nobase-shell").expect("shell dir");
    let rdir = TempDir::new("prof-load-nobase-rust").expect("rust dir");
    stage(
        sdir.path(),
        "profiles.d/solo.conf",
        b"version=1\noverlays=x\n",
    );
    stage(
        rdir.path(),
        "profiles.d/solo.conf",
        b"version=1\noverlays=x\n",
    );
    let (scode, sout, _) = shell_run(
        sdir.path(),
        &[sdir.path().join("profiles.d").as_os_str()],
        &[],
        &format!("_dot_profiles_load \"$2\"; printf 'rc=%s\\n' \"$?\"; {DUMP}"),
    );
    let mut state = profiles::State::default();
    let rcode = state.load(Some(&rdir.path().join("profiles.d")), "", "", Some("base"));
    assert_eq!(scode, 0, "shell harness nobase");
    let rust = format!(
        "rc={}\n{}",
        if rcode.is_ok() { "0" } else { "1" },
        dump(&state)
    );
    let shell = String::from_utf8(sout).expect("dump");
    assert_eq!(
        normalize_roots(&rust, sdir.path(), rdir.path()),
        normalize_roots(&shell, sdir.path(), rdir.path()),
        "nobase parity"
    );
}

#[test]
fn flatten_and_select_agree() {
    let dir = TempDir::new("prof-flatten").expect("fixture dir");
    let root = dir.path();
    stage(root, "profiles.d/base.conf", b"version=1\noverlays=core\n");
    stage(
        root,
        "profiles.d/web.conf",
        b"version=1\nprofiles=base\noverlays=websvc,metrics\n",
    );
    for name in ["web", "base", "ghost"] {
        let (scode, sout, _) = shell_run(
            root,
            &[root.join("profiles.d").as_os_str(), name.as_ref()],
            &[],
            &format!(
                "_dot_profiles_load \"$2\" && _dot_profile_flatten \"$3\"; printf 'rc=%s\\n' \"$?\"; {DUMP}"
            ),
        );
        let mut state = profiles::State::default();
        let rcode = state
            .load(Some(&root.join("profiles.d")), "", "", Some("base"))
            .and_then(|_| state.flatten(name));
        assert_eq!(scode, 0, "shell harness flatten {name}");
        assert_eq!(
            format!(
                "rc={}\n{}",
                if rcode.is_ok() { "0" } else { "1" },
                dump(&state)
            ),
            String::from_utf8(sout).expect("dump"),
            "flatten parity for {name}"
        );
    }
    // Phase-one base selection on a loaded state.
    let (scode, sout, _) = shell_run(
        root,
        &[root.join("profiles.d").as_os_str()],
        &[],
        &format!(
            "_dot_profiles_load \"$2\" && _dot_profile_select_base; printf 'rc=%s\\n' \"$?\"; {DUMP}"
        ),
    );
    let mut state = profiles::State::default();
    let rcode = state
        .load(Some(&root.join("profiles.d")), "", "", Some("base"))
        .and_then(|_| state.select_base());
    assert_eq!(scode, 0, "shell harness select_base");
    assert_eq!(
        format!(
            "rc={}\n{}",
            if rcode.is_ok() { "0" } else { "1" },
            dump(&state)
        ),
        String::from_utf8(sout).expect("dump"),
        "select_base parity"
    );
}

#[test]
fn resolve_twins_agree() {
    let user = profiles::current_user().expect("login name");
    let host = dot::platform::detect_host().expect("hostname");
    // Selector bodies per label, built from the live identity.
    let bodies = |kind: &str| -> Vec<(&'static str, String)> {
        match kind {
            "user" => vec![(
                "10-u.conf",
                format!("version=1\nuser={user}\nprofile=web\n"),
            )],
            "host" => vec![(
                "10-h.conf",
                format!("version=1\nhost={}\nprofile=web\n", host.to_uppercase()),
            )],
            "both" => vec![(
                "10-b.conf",
                format!("version=1\nuser={user}\nhost={host}\nprofile=web\n"),
            )],
            "tie" => vec![
                (
                    "10-a.conf",
                    format!("version=1\nuser={user}\nprofile=web\n"),
                ),
                (
                    "20-b.conf",
                    format!("version=1\nuser={user}\nprofile=base\n"),
                ),
            ],
            "specific" => vec![
                (
                    "10-u.conf",
                    format!("version=1\nuser={user}\nprofile=base\n"),
                ),
                (
                    "20-uh.conf",
                    format!("version=1\nuser={user}\nhost={host}\nprofile=web\n"),
                ),
            ],
            _ => vec![],
        }
    };
    for label in ["none", "user", "host", "both", "tie", "specific"] {
        let sdir = TempDir::new(&format!("prof-res-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("prof-res-{label}-rust")).expect("rust dir");
        for base in [&sdir, &rdir] {
            stage(
                base.path(),
                "profiles.d/base.conf",
                b"version=1\noverlays=core\n",
            );
            stage(
                base.path(),
                "profiles.d/web.conf",
                b"version=1\nprofiles=base\noverlays=websvc\n",
            );
            for (name, body) in bodies(label) {
                stage(
                    base.path(),
                    &format!("selectors/root/{name}"),
                    body.as_bytes(),
                );
            }
        }
        let snippet = format!(
            "_dot_profiles_load \"$2\" && _dot_profile_resolve \"$3\" \"$4\"; printf 'rc=%s\\n' \"$?\"; {DUMP}"
        );
        let (scode, sout, _) = shell_run(
            sdir.path(),
            &[
                sdir.path().join("profiles.d").as_os_str(),
                sdir.path().join("selectors/root").as_os_str(),
                sdir.path().join("selectors/local").as_os_str(),
            ],
            &[],
            &snippet,
        );
        let mut state = profiles::State::default();
        let rcode = state
            .load(Some(&rdir.path().join("profiles.d")), "", "", Some("base"))
            .and_then(|_| {
                state.resolve_with(
                    &rdir.path().join("selectors/root"),
                    &rdir.path().join("selectors/local"),
                    &[],
                    &user,
                    &host,
                    euid(),
                )
            });
        assert_eq!(scode, 0, "shell harness resolve {label}");
        let rust = format!(
            "rc={}\n{}",
            if rcode.is_ok() { "0" } else { "1" },
            dump(&state)
        );
        let shell = String::from_utf8(sout).expect("dump");
        assert_eq!(
            normalize_roots(&rust, sdir.path(), rdir.path()),
            normalize_roots(&shell, sdir.path(), rdir.path()),
            "resolve parity for {label}"
        );
    }
    // Machine-local selectors: valid file wins; bad mode fails.
    for (label, mode) in [("local-ok", 0o600), ("local-bad", 0o644)] {
        let sdir = TempDir::new(&format!("prof-local-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("prof-local-{label}-rust")).expect("rust dir");
        for base in [&sdir, &rdir] {
            stage(
                base.path(),
                "profiles.d/base.conf",
                b"version=1\noverlays=core\n",
            );
            stage(
                base.path(),
                "profiles.d/web.conf",
                b"version=1\nprofiles=base\noverlays=websvc\n",
            );
            let path = stage(
                base.path(),
                "selectors/local/10-l.conf",
                format!("version=1\nuser={user}\nprofile=web\n").as_bytes(),
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        }
        let snippet = format!(
            "_dot_profiles_load \"$2\" && _dot_profile_resolve \"$3\" \"$4\"; printf 'rc=%s\\n' \"$?\"; {DUMP}"
        );
        let (scode, sout, _) = shell_run(
            sdir.path(),
            &[
                sdir.path().join("profiles.d").as_os_str(),
                sdir.path().join("selectors/root").as_os_str(),
                sdir.path().join("selectors/local").as_os_str(),
            ],
            &[],
            &snippet,
        );
        let mut state = profiles::State::default();
        let rcode = state
            .load(Some(&rdir.path().join("profiles.d")), "", "", Some("base"))
            .and_then(|_| {
                state.resolve_with(
                    &rdir.path().join("selectors/root"),
                    &rdir.path().join("selectors/local"),
                    &[],
                    &user,
                    &host,
                    euid(),
                )
            });
        assert_eq!(scode, 0, "shell harness local {label}");
        let rust = format!(
            "rc={}\n{}",
            if rcode.is_ok() { "0" } else { "1" },
            dump(&state)
        );
        let shell = String::from_utf8(sout).expect("dump");
        assert_eq!(
            normalize_roots(&rust, sdir.path(), rdir.path()),
            normalize_roots(&shell, sdir.path(), rdir.path()),
            "local parity for {label}"
        );
    }
}
