//! Differential parity tests for overlay discovery and
//! local-source validation against `lib/dot/overlays.sh` (plus the
//! `repos/config.sh` checkout-match block and `overlay-context.sh`
//! field gate): filename identities, safety gates, descriptor
//! parsing in both strictness modes, legacy and profile-aware
//! discovery, inventory validation, and preflight — including
//! every message.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the overlay libraries sourced.
/// `extra_env` sets (`Some`) or removes (`None`) variables.
/// Returns exit code, stdout, and stderr.
fn shell_run(
    home: &Path,
    argv: &[&std::ffi::OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let prefix = std::env::var_os("PREFIX").unwrap_or_default();
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/platform.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/temp.sh\"\n. \"$1/lib/dot/resources.sh\"\n. \"$1/lib/dot/overlay-context.sh\"\n. \"$1/lib/dot/overlays.sh\"\n. \"$1/lib/dot/repos/config.sh\"\n. \"$1/lib/dot/profiles.sh\"\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("PREFIX", &prefix)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(home)
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

/// Helper status from a dump harness: the process exit is always
/// 0 (the dump `printf` runs last), so the status is the printed
/// leading `rc=N` line (`-1` when absent or malformed).
fn dump_rc(dump: &[u8]) -> i32 {
    let line = dump.split(|byte| *byte == b'\n').next().unwrap_or(b"");
    let line = line.strip_prefix(b"rc=").unwrap_or(b"");
    std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(-1)
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

/// Current euid for ownership-gated checks.
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

/// Live matching inputs, detected exactly like the shell does.
fn live_matches() -> overlays::MatchInputs {
    let prefix = std::env::var_os("PREFIX").unwrap_or_default();
    let prefix = prefix.to_string_lossy();
    overlays::MatchInputs {
        platform: dot::platform::detect_platform().ok(),
        termux: !prefix.is_empty() && prefix.contains("/com.termux/"),
        host: dot::platform::detect_host().ok(),
    }
}

/// Base [`overlays::Inputs`] for a fixture home.
fn base_inputs(home: &Path) -> overlays::Inputs {
    overlays::Inputs {
        home: home.to_string_lossy().into_owned(),
        xdg_config: String::new(),
        discovery_silent: false,
        profiles_present: false,
        selected: Vec::new(),
        platform: None,
        termux: false,
        host: None,
        euid: euid(),
    }
}

/// `git init` plus one origin (output silenced).
fn git_repo(path: &Path, origin: Option<&str>) {
    std::fs::create_dir_all(path).expect("repo dir");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git init");
    assert!(status.success(), "git init {}", path.display());
    if let Some(url) = origin {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git remote add");
        assert!(status.success(), "git remote add {}", path.display());
    }
}

/// Render an overlay [`overlays::State`] in the canonical
/// comparison shape.
fn dump(state: &overlays::State) -> String {
    let mut out = String::new();
    for (tag, values) in [
        ("O", &state.overlays),
        ("C", &state.configured),
        ("EN", &state.eligible_names),
        ("E", &state.eligible),
        ("AN", &state.active_names),
        ("A", &state.active),
        ("L", &state.lifecycle),
        ("S", &state.selected),
    ] {
        for value in values {
            out.push_str(&format!("{tag}|{value}\n"));
        }
    }
    out.push_str(&format!(
        "error={}\n",
        state.discovery_error.as_deref().unwrap_or(""),
    ));
    out
}

/// Render collected warnings plus a returned error exactly like
/// the shell's stderr: one line each, in order.
fn render_stderr(warnings: &[String], error: Option<&overlays::Error>) -> String {
    let mut out = String::new();
    for warning in warnings {
        out.push_str(warning);
        out.push('\n');
    }
    if let Some(error) = error {
        let rendered = error.to_string();
        if !rendered.is_empty() {
            out.push_str(&rendered);
            out.push('\n');
        }
    }
    out
}

/// Shell twin of [`dump`]. Announcement (`dot: overlay: ...`
/// unless silent) is compared through stderr, not state.
const SHELL_DUMP: &str = r#"for e in ${OVERLAYS[@]+"${OVERLAYS[@]}"}; do printf 'O|%s\n' "$e"; done; for e in ${CONFIGURED_OVERLAY_NAMES[@]+"${CONFIGURED_OVERLAY_NAMES[@]}"}; do printf 'C|%s\n' "$e"; done; for e in ${ELIGIBLE_OVERLAY_NAMES[@]+"${ELIGIBLE_OVERLAY_NAMES[@]}"}; do printf 'EN|%s\n' "$e"; done; for e in ${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}; do printf 'E|%s\n' "$e"; done; for e in ${ACTIVE_OVERLAY_NAMES[@]+"${ACTIVE_OVERLAY_NAMES[@]}"}; do printf 'AN|%s\n' "$e"; done; for e in ${ACTIVE_OVERLAYS[@]+"${ACTIVE_OVERLAYS[@]}"}; do printf 'A|%s\n' "$e"; done; for e in ${DOT_OVERLAY_LIFECYCLE[@]+"${DOT_OVERLAY_LIFECYCLE[@]}"}; do printf 'L|%s\n' "$e"; done; for e in ${SELECTED_OVERLAY_NAMES[@]+"${SELECTED_OVERLAY_NAMES[@]}"}; do printf 'S|%s\n' "$e"; done; printf 'error=%s\n' "${DOT_OVERLAY_DISCOVERY_ERROR:-}""#;

#[test]
fn names_agree() {
    let dir = TempDir::new("ov-names").expect("fixture dir");
    let home = dir.path();
    // (file, sync) identities, including the newline-capture
    // quirk: command substitution strips trailing newlines after
    // the (non-)strip, which the port mirrors.
    let cases = [
        ("10-work.conf", "git"),
        ("10-work.conf", "none"),
        ("work.conf", "git"),
        ("10-x.local.conf", "git"),
        ("10-x.local.conf", "none"),
        ("x.local.conf", "none"),
        ("10-a-b.conf", "git"),
        ("10-.conf", "git"),
        ("10-.conf", "none"),
        ("nodigits.conf", "git"),
        ("/x/y/10-deep.conf", "git"),
        ("Makefile", "git"),
        ("10-w.conf\n", "git"),
        ("10-w\n.conf", "git"),
    ];
    for (file, sync) in cases {
        let (code, out, _) = shell_run(
            home,
            &[file.as_ref(), sync.as_ref()],
            &[],
            "name=$(_overlay_name \"$2\" \"$3\"); profile=$(_overlay_profile_name \"$2\"); printf '%s\\n%s\\n' \"$name\" \"$profile\"",
        );
        assert_eq!(code, 0, "shell harness names {file:?}");
        let text = String::from_utf8(out).expect("names text");
        let mut lines = text.split('\n');
        let shell_name = lines.next().unwrap_or("");
        let shell_profile = lines.next().unwrap_or("");
        assert_eq!(
            overlays::overlay_name(file, sync),
            shell_name,
            "overlay name for {file:?}/{sync}"
        );
        assert_eq!(
            overlays::overlay_profile_name(file),
            shell_profile,
            "profile name for {file:?}"
        );
    }
}

#[test]
fn safeties_agree() {
    let dir = TempDir::new("ov-safe").expect("fixture dir");
    let home = dir.path();
    // Field values: the shared gate plus its `od` repeat-marker
    // fail-closed quirk (two identical 16-byte chunks reject,
    // even when every byte is otherwise innocent).
    let sixteen = "A".repeat(16);
    let thirty_two = "A".repeat(32);
    let dashes = "-".repeat(48);
    let values: &[&[u8]] = &[
        b"ok",
        b"a|b",
        b"a\tb",
        b"a\nb",
        b"a\rb",
        b"a\x7fb",
        b"\xc3\xa9",
        sixteen.as_bytes(),
        thirty_two.as_bytes(),
        dashes.as_bytes(),
        b"0123456789abcdef0123456789abcde",
    ];
    for value in values {
        let lossy = String::from_utf8_lossy(value);
        let (code, _, _) = shell_run(
            home,
            &[lossy.as_ref().as_ref()],
            &[],
            "_dot_overlay_field_safe \"$2\"",
        );
        assert_eq!(
            overlays::descriptor_value_safe(value),
            code == 0,
            "field for {value:?}"
        );
    }
    // Relative paths: every rejected shape in the case list.
    let rels = [
        "a",
        "a/b",
        ".hidden/x",
        "a/.../b",
        "...",
        "",
        "/a",
        ".",
        "..",
        "./a",
        "../a",
        "a/",
        "a//b",
        "a/./b",
        "a/../b",
        "a/.",
        "a/..",
        "a|b",
    ];
    for rel in rels {
        let (code, _, _) = shell_run(
            home,
            &[rel.as_ref()],
            &[],
            "_overlay_relative_path_safe \"$2\"",
        );
        assert_eq!(
            overlays::relative_path_safe(rel.as_bytes()),
            code == 0,
            "relative path for {rel:?}"
        );
    }
}

#[test]
fn descriptor_file_safe_agrees() {
    let dir = TempDir::new("ov-file").expect("fixture dir");
    let root = dir.path();
    let big = vec![b'x'; 65537];
    let exact = vec![b'y'; 65536];
    let repeat = {
        let mut bytes = vec![b'A'; 16];
        bytes.extend_from_slice(&[b'A'; 16]);
        bytes
    };
    let cases: &[(&str, Option<&[u8]>)] = &[
        ("ok.conf", Some(b"url=x\n")),
        ("empty.conf", Some(b"")),
        ("exact.conf", Some(&exact)),
        ("big.conf", Some(&big)),
        ("nul.conf", Some(b"a\x00b\n")),
        ("del.conf", Some(b"a\x7fb\n")),
        ("high.conf", Some(b"\xc3\xa9\n")),
        ("repeat.conf", Some(&repeat)),
        ("missing.conf", None),
    ];
    for &(name, body) in cases {
        let path = match body {
            Some(body) => stage(root, name, body),
            None => root.join(name),
        };
        let (code, _, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_overlay_descriptor_file_safe \"$2\"",
        );
        assert_eq!(
            overlays::descriptor_file_safe(&path),
            code == 0,
            "descriptor file safety for {name}"
        );
    }
    stage(root, "adir/child", b"x");
    std::os::unix::fs::symlink("ok.conf", root.join("link.conf")).expect("symlink");
    for (name, arg) in [("dir", "adir"), ("symlink", "link.conf")] {
        let path = root.join(arg);
        let (code, _, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            "_overlay_descriptor_file_safe \"$2\"",
        );
        assert_ne!(code, 0, "shell rejects {name}");
        assert!(
            !overlays::descriptor_file_safe(&path),
            "rust rejects {name}"
        );
    }
}

#[test]
fn parse_twins_agree() {
    let dir = TempDir::new("ov-parse").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let matches = live_matches();
    let live_platform = matches.platform.clone().unwrap_or_default();
    let live_host = matches.host.clone().unwrap_or_default();
    let live_host_upper = live_host.to_uppercase();
    // (filename, body); `None` stages nothing for the missing-file
    // pair. The same file feeds both engines, so `conf` strings
    // compare exactly with no root normalization.
    let cases: &[(&str, Option<&[u8]>)] = &[
        ("10-git-min.conf", Some(b"url=https://example.com/x.git\n")),
        // A star looks like a glob but specs compare literally
        // (`[[ $item == "$current" ]]`), so this filters on both
        // sides — the parity pin matters more than eligibility.
        (
            "10-git-star.conf",
            Some(b"url=https://example.com/x.git\nplatforms=*\nhosts=*\noptional=true\n"),
        ),
        (
            "10-git-unknown.conf",
            Some(b"url=https://example.com/x.git\nfrobnicate=1\n"),
        ),
        ("10-git-opt.conf", Some(b"url=x\noptional=true\n")),
        ("10-dup-url.conf", Some(b"url=a\nurl=b\n")),
        ("10-dup-sync.conf", Some(b"url=x\nsync=git\nsync=none\n")),
        ("10-bad-sync.conf", Some(b"url=x\nsync=hg\n")),
        ("10-git-path.conf", Some(b"url=x\npath=/y\n")),
        ("10-git-missing-url.conf", Some(b"optional=true\n")),
        ("10-bad-optional.conf", Some(b"url=x\noptional=maybe\n")),
        (
            "10-plat-miss.conf",
            Some(b"url=x\nplatforms=no-such-os-zzz\n"),
        ),
        (
            "10-host-miss.conf",
            Some(b"url=x\nhosts=no-such-host-zzz\n"),
        ),
        ("10-none-min.conf", Some(b"sync=none\npath=~/trees/w\n")),
        ("10-w.local.conf", Some(b"sync=none\npath=~/trees/w\n")),
        ("10-none-abs.conf", Some(b"sync=none\npath=/srv/trees/w\n")),
        ("10-none-rel.conf", Some(b"sync=none\npath=trees/w\n")),
        ("10-none-missing.conf", Some(b"sync=none\n")),
        ("10-none-empty-path.conf", Some(b"sync=none\npath=\n")),
        ("10-none-url.conf", Some(b"sync=none\npath=/x\nurl=y\n")),
        (
            "10-none-optional.conf",
            Some(b"sync=none\npath=/x\noptional=true\n"),
        ),
        ("10-none-slash.conf", Some(b"sync=none\npath=~/a//b\n")),
        ("10-none-root.conf", Some(b"sync=none\npath=/\n")),
        (
            "10-none-dup-path.conf",
            Some(b"sync=none\npath=/a\npath=/b\n"),
        ),
        (
            "10-none-unknown.conf",
            Some(b"sync=none\npath=/a\nbogus=1\n"),
        ),
        ("10-comments.conf", Some(b"# top\n\nurl=x\n# mid\n")),
        ("10-no-trailing-nl.conf", Some(b"url=x")),
        ("10-crlf.conf", Some(b"url=x\r\n")),
        ("10-empty.conf", Some(b"")),
        ("10-url-pipe.conf", Some(b"url=a|b\n")),
        ("10-control.conf", Some(b"url=a\x01b\n")),
        ("10-gone.conf", None),
    ];
    for strict in [false, true] {
        for &(name, body) in cases {
            let path = match body {
                Some(body) => stage(home, name, body),
                None => home.join(name),
            };
            let text = path.to_string_lossy().into_owned();
            let flag = if strict { "1" } else { "0" };
            let (_, sout, serr) = shell_run(
                home,
                &[path.as_os_str(), flag.as_ref()],
                &[],
                "DOT_OVERLAY_STRICT_SELECTED=\"$3\"; _parse_overlay_conf \"$2\"; rc=$?; printf 'rc=%s\\nrecord=%s\\n' \"$rc\" \"$REPLY\"",
            );
            let mut warnings = Vec::new();
            let rout =
                overlays::parse_conf(&path, &text, strict, &home_text, &matches, &mut warnings);
            let (rcode, record) = match &rout {
                Ok(Some(record)) => (0, record.clone()),
                Ok(None) => (1, String::new()),
                Err(error) => match error {
                    overlays::Error::Warning(message) => (2, message.clone()),
                    _ => (error.code(), String::new()),
                },
            };
            let rust = format!("rc={rcode}\nrecord={record}\n");
            let scode = dump_rc(&sout);
            let shell = String::from_utf8(sout).expect("parse dump");
            assert_eq!(scode, rcode, "parse code for {name}/strict={strict}");
            assert_eq!(rust, shell, "parse dump for {name}/strict={strict}");
            let rust_err = render_stderr(&warnings, rout.as_ref().err());
            let shell_err = String::from_utf8(serr).expect("parse stderr");
            // A missing descriptor makes bash itself report the
            // failed redirect (`{script}: line {n}: {path}: No such
            // file or directory`); the engine prints nothing there,
            // so that runtime noise stays out of the comparison. The
            // filter only applies when the fixture is known absent,
            // so it cannot mask engine output.
            let shell_err: String = if body.is_none() {
                let suffix = format!("{text}: No such file or directory");
                shell_err
                    .lines()
                    .filter(|line| !line.ends_with(suffix.as_str()))
                    .map(|line| format!("{line}\n"))
                    .collect()
            } else {
                shell_err
            };
            assert_eq!(
                shell_err, rust_err,
                "parse stderr for {name}/strict={strict}"
            );
        }
        // Live-identity selectors, built from detection both sides
        // agree on: a hit stays eligible, a miss filters.
        for (label, key, value, hit) in [
            ("plat", "platforms", live_platform.as_str(), true),
            ("plat-miss", "platforms", "no-such-os-zzz", false),
            // A hit stays eligible; an empty live identity means an
            // empty spec, which also matches (no filter).
            ("host", "hosts", live_host_upper.as_str(), true),
            ("host-miss", "hosts", "no-such-host-zzz", false),
        ] {
            let name = format!("10-live-{label}.conf");
            let body = format!("url=x\n{key}={value}\n");
            let path = stage(home, &name, body.as_bytes());
            let text = path.to_string_lossy().into_owned();
            let flag = if strict { "1" } else { "0" };
            let (_, sout, serr) = shell_run(
                home,
                &[path.as_os_str(), flag.as_ref()],
                &[],
                "DOT_OVERLAY_STRICT_SELECTED=\"$3\"; _parse_overlay_conf \"$2\"; rc=$?; printf 'rc=%s\\nrecord=%s\\n' \"$rc\" \"$REPLY\"",
            );
            let mut warnings = Vec::new();
            let rout =
                overlays::parse_conf(&path, &text, strict, &home_text, &matches, &mut warnings);
            let (rcode, record) = match &rout {
                Ok(Some(record)) => (0, record.clone()),
                Ok(None) => (1, String::new()),
                Err(error) => match error {
                    overlays::Error::Warning(message) => (2, message.clone()),
                    _ => (error.code(), String::new()),
                },
            };
            let scode = dump_rc(&sout);
            assert_eq!(
                (scode, rcode),
                (if hit { 0 } else { 1 }, if hit { 0 } else { 1 }),
                "live {label}/strict={strict}"
            );
            assert_eq!(
                format!("rc={rcode}\nrecord={record}\n"),
                String::from_utf8(sout).expect("live dump"),
                "live dump for {label}/strict={strict}"
            );
            assert_eq!(
                String::from_utf8(serr).expect("live stderr"),
                render_stderr(&warnings, rout.as_ref().err()),
                "live stderr for {label}/strict={strict}"
            );
        }
    }
}

/// Stage the shared legacy-discovery fixture: one `sync=none`
/// tree, git checkouts with matching/missing origins, a filtered
/// descriptor, a duplicate-name pair, and an unknown-key warning.
/// Returns the `overlays.d` directory.
fn stage_legacy_fixture(home: &Path) -> PathBuf {
    let confd = home.join("config/dot/overlays.d");
    // Filesystem overlay with a real tree.
    stage(home, "trees/beta/home/app.conf", b"version=1\noverlays=x\n");
    stage(&confd, "20-beta.conf", b"sync=none\npath=~/trees/beta\n");
    // Git overlay whose checkout matches.
    git_repo(
        &home.join(".dotfiles-alpha"),
        Some("file:///repo/alpha.git"),
    );
    stage(&confd, "10-alpha.conf", b"url=file:///repo/alpha.git\n");
    // Git overlays with no usable source.
    stage(
        &confd,
        "30-gamma.conf",
        b"url=https://example.com/missing.git\n",
    );
    stage(
        &confd,
        "40-delta.conf",
        b"url=https://example.com/opt.git\noptional=true\n",
    );
    // Filtered from this host, silently.
    stage(
        &confd,
        "50-eps.conf",
        b"url=https://example.com/eps.git\nplatforms=no-such-os-zzz\n",
    );
    // Duplicate parsed name: the second warns and skips.
    stage(&confd, "60-dup.conf", b"url=https://example.com/dup.git\n");
    stage(
        &confd,
        "70-dup.conf",
        b"url=https://example.com/dup.git\noptional=true\n",
    );
    // Unknown keys warn but stay eligible.
    stage(
        &confd,
        "80-warn.conf",
        b"url=https://example.com/warn.git\nfrobnicate=1\n",
    );
    confd
}

#[test]
fn discover_legacy_agrees() {
    let dir = TempDir::new("ov-disc-leg").expect("fixture dir");
    let home = dir.path();
    let confd = stage_legacy_fixture(home);
    let config = home.join("config");
    // XDG_CONFIG_HOME as an owned string outlives the call.
    let xdg = config.to_string_lossy().into_owned();
    let env = [("XDG_CONFIG_HOME", Some(xdg.as_str()))];
    let (_, sout, serr) = shell_run(
        home,
        &[],
        &env,
        &format!("_discover_overlays; rc=$?; printf 'rc=%s\\n' \"$rc\"; {SHELL_DUMP}"),
    );
    let inputs = base_inputs(home);
    let matches = live_matches();
    let mut state = overlays::State::default();
    let rcode = overlays::discover(
        &mut state,
        &confd,
        &confd.to_string_lossy(),
        &inputs,
        &matches,
    );
    let rcode = match rcode {
        Ok(()) => 0,
        Err(error) => error.code(),
    };
    let scode = dump_rc(&sout);
    assert_eq!(scode, rcode, "legacy discovery code");
    assert_eq!(
        format!("rc={rcode}\n{}", dump(&state)),
        String::from_utf8(sout).expect("legacy dump"),
        "legacy discovery dump"
    );
    assert_eq!(
        String::from_utf8(serr).expect("legacy stderr"),
        render_stderr(&state.warnings, None),
        "legacy discovery stderr"
    );
}

#[test]
fn discover_legacy_invalid_agrees() {
    let dir = TempDir::new("ov-disc-bad").expect("fixture dir");
    let home = dir.path();
    let confd = home.join("config/dot/overlays.d");
    stage(home, "trees/good/home/app.conf", b"version=1\noverlays=x\n");
    stage(&confd, "10-good.conf", b"sync=none\npath=~/trees/good\n");
    stage(&confd, "20-bad.conf", b"url=x\nsync=hg\n");
    let config = home.join("config");
    let xdg = config.to_string_lossy().into_owned();
    let env = [("XDG_CONFIG_HOME", Some(xdg.as_str()))];
    let (_, sout, serr) = shell_run(
        home,
        &[],
        &env,
        &format!("_discover_overlays; rc=$?; printf 'rc=%s\\n' \"$rc\"; {SHELL_DUMP}"),
    );
    let inputs = base_inputs(home);
    let matches = live_matches();
    let mut state = overlays::State::default();
    let rcode = overlays::discover(
        &mut state,
        &confd,
        &confd.to_string_lossy(),
        &inputs,
        &matches,
    );
    let rerror = rcode.as_ref().err().cloned();
    let rcode = match rcode {
        Ok(()) => 0,
        Err(error) => error.code(),
    };
    let scode = dump_rc(&sout);
    assert_eq!(scode, rcode, "invalid legacy code");
    assert_eq!(
        format!("rc={rcode}\n{}", dump(&state)),
        String::from_utf8(sout).expect("invalid legacy dump"),
        "invalid legacy dump"
    );
    assert_eq!(
        String::from_utf8(serr).expect("invalid legacy stderr"),
        render_stderr(&state.warnings, rerror.as_ref()),
        "invalid legacy stderr"
    );
}

/// Run one strict-discovery twin: `selected` names flow through
/// `SELECTED_OVERLAY_NAMES` on the shell side and
/// [`overlays::Inputs`] on the Rust side.
fn run_strict(
    home: &Path,
    selected: &[&str],
    silent: bool,
) -> (i32, String, String, i32, String, String) {
    let config = home.join("config");
    let confd = config.join("dot/overlays.d");
    let xdg = config.to_string_lossy().into_owned();
    let flag = if silent { "1" } else { "0" };
    let mut argv: Vec<&std::ffi::OsStr> = Vec::new();
    for name in selected {
        argv.push(name.as_ref());
    }
    let env = [
        ("XDG_CONFIG_HOME", Some(xdg.as_str())),
        ("DOT_OVERLAY_DISCOVERY_SILENT", Some(flag)),
    ];
    let (_, sout, serr) = shell_run(
        home,
        &argv,
        &env,
        &format!(
            "DOT_PROFILES_PRESENT=1; SELECTED_OVERLAY_NAMES=(\"${{@:2}}\"); _discover_overlays; rc=$?; printf 'rc=%s\\n' \"$rc\"; {SHELL_DUMP}"
        ),
    );
    let mut inputs = base_inputs(home);
    inputs.profiles_present = true;
    inputs.discovery_silent = silent;
    inputs.selected = selected.iter().map(|name| name.to_string()).collect();
    let matches = live_matches();
    let mut state = overlays::State::default();
    let rcode = overlays::discover(
        &mut state,
        &confd,
        &confd.to_string_lossy(),
        &inputs,
        &matches,
    );
    let rerror = rcode.as_ref().err().cloned();
    let rcode = match rcode {
        Ok(()) => 0,
        Err(error) => error.code(),
    };
    // Silence gates only the `dot: overlay:` announcement; fatal
    // `  warning:` lines still print.
    let show_error = match &rerror {
        Some(overlays::Error::Announced(_)) => !silent,
        _ => true,
    };
    let rust_err = render_stderr(
        &state.warnings,
        if show_error { rerror.as_ref() } else { None },
    );
    let scode = dump_rc(&sout);
    (
        scode,
        String::from_utf8(sout).expect("strict dump"),
        String::from_utf8(serr).expect("strict stderr"),
        rcode,
        format!("rc={rcode}\n{}", dump(&state)),
        rust_err,
    )
}

#[test]
fn discover_strict_agrees() {
    let dir = TempDir::new("ov-disc-strict").expect("fixture dir");
    let home = dir.path();
    let confd = home.join("config/dot/overlays.d");
    git_repo(&home.join(".dotfiles-web"), Some("file:///repo/web.git"));
    stage(&confd, "10-web.conf", b"url=file:///repo/web.git\n");
    stage(
        &confd,
        "20-base.conf",
        b"url=https://example.com/base.git\n",
    );
    stage(
        &confd,
        "30-skip.conf",
        b"url=https://example.com/skip.git\n",
    );
    stage(
        &confd,
        "40-inelig.conf",
        b"url=https://example.com/inelig.git\nplatforms=no-such-os-zzz\n",
    );
    for silent in [false, true] {
        let (scode, shell, serr, rcode, rust, rust_err) =
            run_strict(home, &["web", "base", "inelig"], silent);
        assert_eq!(scode, rcode, "strict code silent={silent}");
        assert_eq!(rust, shell, "strict dump silent={silent}");
        assert_eq!(serr, rust_err, "strict stderr silent={silent}");
    }
}

#[test]
fn discover_strict_errors_agree() {
    // (label, files, selected): each aborts discovery.
    type StrictError = (
        &'static str,
        &'static [(&'static str, &'static [u8])],
        &'static [&'static str],
    );
    let errors: &[StrictError] = &[
        (
            "missing-selected",
            &[("10-web.conf", b"url=file:///repo/web.git\n")],
            &["web", "ghost"],
        ),
        (
            "bad-filename",
            &[
                ("10-web.conf", b"url=file:///repo/web.git\n"),
                ("BAD.conf", b"url=https://example.com/bad.git\n"),
            ],
            &["web"],
        ),
        (
            "dup-names",
            &[
                ("10-dup.conf", b"url=https://example.com/a.git\n"),
                ("20-dup.conf", b"url=https://example.com/b.git\n"),
            ],
            &["dup"],
        ),
        (
            "invalid-content",
            &[("10-web.conf", b"url=file:///repo/web.git\nfrobnicate=1\n")],
            &["web"],
        ),
    ];
    for &(label, files, selected) in errors {
        let dir = TempDir::new(&format!("ov-disc-err-{label}")).expect("fixture dir");
        let home = dir.path();
        let confd = home.join("config/dot/overlays.d");
        git_repo(&home.join(".dotfiles-web"), Some("file:///repo/web.git"));
        for &(name, body) in files {
            stage(&confd, name, body);
        }
        for silent in [false, true] {
            let (scode, shell, serr, rcode, rust, rust_err) = run_strict(home, selected, silent);
            assert_eq!(scode, rcode, "{label} code silent={silent}");
            assert_eq!(scode, 2, "{label} fails silent={silent}");
            assert_eq!(rust, shell, "{label} dump silent={silent}");
            assert_eq!(serr, rust_err, "{label} stderr silent={silent}");
        }
    }
}

/// Run one inventory-validation twin with an explicit `OVERLAYS`
/// record list for the cross-source check.
fn run_validate(home: &Path, path: &str, records: &[&str]) -> (i32, String, String) {
    let mut argv: Vec<&std::ffi::OsStr> = vec![path.as_ref()];
    for record in records {
        argv.push(record.as_ref());
    }
    let (_, sout, serr) = shell_run(
        home,
        &argv,
        &[],
        "OVERLAYS=(\"${@:3}\"); _overlay_local_source_validate \"$2\"; rc=$?; printf 'rc=%s\\nreply=%s\\n' \"$rc\" \"$REPLY\"",
    );
    let scode = dump_rc(&sout);
    (
        scode,
        String::from_utf8(sout).expect("validate dump"),
        String::from_utf8(serr).expect("validate stderr"),
    )
}

#[test]
fn local_validate_agrees() {
    // Healthy tree: files, a subdir, links, and a skipped editor
    // backup. Unreadable modes resolve identically on both sides
    // whether the runner is root or not.
    let dir = TempDir::new("ov-val-ok").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let tree = home.join("tree");
    stage(&tree, "home/app.conf", b"ok\n");
    stage(&tree, "home/sub/deep.conf", b"ok\n");
    stage(&tree, "home/backup.~1~", b"stale\n");
    stage(&tree, "home/locked.conf", b"nope\n");
    std::fs::set_permissions(
        tree.join("home/locked.conf"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod");
    std::os::unix::fs::symlink("app.conf", tree.join("home/link.conf")).expect("symlink");
    std::os::unix::fs::symlink("sub", tree.join("home/dirlink")).expect("symlink");
    let tree_text = tree.to_string_lossy().into_owned();
    let (scode, shell, serr) = run_validate(home, &tree_text, &[]);
    let rust = overlays::source_validate(&tree_text, &[], &home_text);
    let (rcode, reply) = match &rust {
        Ok(()) => (0, String::new()),
        Err(diagnostic) => (1, diagnostic.clone()),
    };
    assert_eq!(scode, rcode, "healthy tree code");
    assert_eq!(
        format!("rc={rcode}\nreply={reply}\n"),
        shell,
        "healthy tree dump"
    );
    assert_eq!(serr, "", "healthy tree stderr");

    // Missing `home` directory.
    let dir = TempDir::new("ov-val-missing").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let path = home.join("tree");
    std::fs::create_dir(&path).expect("mkdir");
    let path_text = path.to_string_lossy().into_owned();
    let (scode, shell, serr) = run_validate(home, &path_text, &[]);
    let rust = overlays::source_validate(&path_text, &[], &home_text);
    let (rcode, reply) = match &rust {
        Ok(()) => (0, String::new()),
        Err(diagnostic) => (1, diagnostic.clone()),
    };
    assert_eq!(scode, rcode, "missing home code");
    assert_eq!(
        format!("rc={rcode}\nreply={reply}\n"),
        shell,
        "missing home dump"
    );
    assert_eq!(serr, "", "missing home stderr");

    // Dangling symlink and unrepresentable name.
    for (label, setup) in [
        ("dangling", "home/gone.conf"),
        ("pipe-name", "home/a|b.conf"),
    ] {
        let dir = TempDir::new(&format!("ov-val-{label}")).expect("fixture dir");
        let home = dir.path();
        let home_text = home.to_string_lossy().into_owned();
        let tree = home.join("tree");
        if label == "dangling" {
            std::fs::create_dir_all(tree.join("home")).expect("mkdir");
            std::os::unix::fs::symlink("absent-target", tree.join(setup)).expect("symlink");
        } else {
            stage(&tree, setup, b"x\n");
        }
        let tree_text = tree.to_string_lossy().into_owned();
        let (scode, shell, serr) = run_validate(home, &tree_text, &[]);
        let rust = overlays::source_validate(&tree_text, &[], &home_text);
        let (rcode, reply) = match &rust {
            Ok(()) => (0, String::new()),
            Err(diagnostic) => (1, diagnostic.clone()),
        };
        assert_eq!(scode, rcode, "{label} code");
        assert_eq!(scode, 1, "{label} fails");
        assert_eq!(
            format!("rc={rcode}\nreply={reply}\n"),
            shell,
            "{label} dump"
        );
        assert_eq!(serr, "", "{label} stderr");
    }
    // A fifo is skipped by the inventory walk on both sides.
    let dir = TempDir::new("ov-val-fifo").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let tree = home.join("tree");
    stage(&tree, "home/app.conf", b"ok\n");
    let status = Command::new("mkfifo")
        .arg(tree.join("home/pipe"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo pipe");
    let tree_text = tree.to_string_lossy().into_owned();
    let (scode, shell, _) = run_validate(home, &tree_text, &[]);
    let rust = overlays::source_validate(&tree_text, &[], &home_text);
    assert_eq!(scode, 0, "fifo shell code");
    assert!(rust.is_ok(), "fifo rust ok");
    assert!(shell.contains("rc=0"), "fifo shell dump");

    // Destination resolving inside the writer's own source: HOME
    // nested under the source tree.
    let dir = TempDir::new("ov-val-inside").expect("fixture dir");
    let base = dir.path();
    let home = base.join("w/home/nested");
    let home_text = home.to_string_lossy().into_owned();
    stage(base, "w/home/marker.conf", b"ok\n");
    std::fs::create_dir_all(&home).expect("nested home");
    let path = base.join("w");
    let path_text = path.to_string_lossy().into_owned();
    let (scode, shell, serr) = run_validate(&home, &path_text, &[]);
    let rust = overlays::source_validate(&path_text, &[], &home_text);
    let (rcode, reply) = match &rust {
        Ok(()) => (0, String::new()),
        Err(diagnostic) => (1, diagnostic.clone()),
    };
    assert_eq!(scode, rcode, "inside-source code");
    assert_eq!(scode, 1, "inside-source fails");
    assert_eq!(
        format!("rc={rcode}\nreply={reply}\n"),
        shell,
        "inside-source dump"
    );
    assert_eq!(serr, "", "inside-source stderr");

    // Destination reaching another active filesystem overlay's
    // source through a symlinked HOME parent.
    let dir = TempDir::new("ov-val-cross").expect("fixture dir");
    let base = dir.path();
    let home = base.join("home");
    let home_text = home.to_string_lossy().into_owned();
    let other = base.join("other");
    stage(&other, "home/target/planted.conf", b"ok\n");
    let writer = base.join("writer");
    std::fs::create_dir_all(writer.join("home/link")).expect("writer tree");
    std::fs::create_dir_all(&home).expect("home dir");
    std::os::unix::fs::symlink(other.join("home/target"), home.join("link")).expect("symlink");
    stage(&writer, "home/link/deep.conf", b"ok\n");
    let writer_text = writer.to_string_lossy().into_owned();
    let other_text = other.to_string_lossy().into_owned();
    let record = format!("other|{other_text}|||false|none");
    let (scode, shell, serr) = run_validate(&home, &writer_text, &[&record]);
    let rust = overlays::source_validate(&writer_text, &[record], &home_text);
    let (rcode, reply) = match &rust {
        Ok(()) => (0, String::new()),
        Err(diagnostic) => (1, diagnostic.clone()),
    };
    assert_eq!(scode, rcode, "cross-source code");
    assert_eq!(scode, 1, "cross-source fails");
    assert_eq!(
        format!("rc={rcode}\nreply={reply}\n"),
        shell,
        "cross-source dump"
    );
    assert_eq!(serr, "", "cross-source stderr");
}

#[test]
fn checkout_matches_agrees() {
    let dir = TempDir::new("ov-checkout").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    git_repo(&home.join("match"), Some("file:///repo/match.git"));
    git_repo(&home.join("mismatch"), Some("file:///repo/other.git"));
    git_repo(&home.join("norremote"), None);
    let multi = home.join("multi");
    git_repo(&multi, Some("file:///repo/a.git"));
    let status = Command::new("git")
        .arg("-C")
        .arg(&multi)
        .arg("remote")
        .arg("set-url")
        .arg("--add")
        .arg("origin")
        .arg("file:///repo/b.git")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("second origin");
    assert!(status.success(), "second origin");
    std::fs::create_dir(home.join("plain")).expect("plain dir");
    std::fs::create_dir_all(home.join("fakegit/.git")).expect("fake git");
    std::fs::create_dir(home.join("filegit")).expect("filegit dir");
    std::fs::write(home.join("filegit/.git"), b"not a gitdir\n").expect("gitfile");
    let cases = [
        ("match", "file:///repo/match.git", "file:///repo/match.git"),
        (
            "mismatch",
            "file:///repo/other.git",
            "file:///repo/other-than-recorded.git",
        ),
        (
            "norremote",
            "file:///repo/norremote.git",
            "file:///repo/norremote.git",
        ),
        ("multi", "file:///repo/a.git", "file:///repo/a.git"),
        ("plain", "file:///repo/plain.git", "file:///repo/plain.git"),
        ("gone", "file:///repo/gone.git", "file:///repo/gone.git"),
        ("fakegit", "file:///repo/fake.git", "file:///repo/fake.git"),
        (
            "filegit",
            "file:///repo/filegit.git",
            "file:///repo/filegit.git",
        ),
    ];
    for (leaf, _origin, url) in cases {
        let path = home.join(leaf);
        let (_, sout, serr) = shell_run(
            home,
            &[path.as_os_str(), url.as_ref()],
            &[],
            "_overlay_checkout_matches \"$2\" \"$3\"; rc=$?; printf 'rc=%s\\nreply=%s\\n' \"$rc\" \"$REPLY\"",
        );
        let scode = dump_rc(&sout);
        let rust = overlays::checkout_matches(&path, url, &home_text);
        let (rcode, reply) = match &rust {
            Ok(recorded) => (0, recorded.clone()),
            Err(diagnostic) => (1, diagnostic.clone()),
        };
        assert_eq!(scode, rcode, "checkout code for {leaf}");
        assert_eq!(
            format!("rc={rcode}\nreply={reply}\n"),
            String::from_utf8(sout).expect("checkout dump"),
            "checkout dump for {leaf}"
        );
        assert_eq!(serr, b"", "checkout stderr for {leaf}");
    }
    // Relative-URL spellings resolve from HOME on both sides.
    git_repo(&home.join("tilde"), Some(&format!("{home_text}/u")));
    git_repo(&home.join("rel"), Some(&format!("{home_text}/rel/u")));
    git_repo(&home.join("scp"), Some("host:path/u.git"));
    for (leaf, url) in [
        ("tilde", "~/u"),
        ("rel", "rel/u"),
        ("scp", "host:path/u.git"),
    ] {
        let path = home.join(leaf);
        let (_, sout, _) = shell_run(
            home,
            &[path.as_os_str(), url.as_ref()],
            &[],
            "_overlay_checkout_matches \"$2\" \"$3\"; rc=$?; printf 'rc=%s\\nreply=%s\\n' \"$rc\" \"$REPLY\"",
        );
        let scode = dump_rc(&sout);
        let rust = overlays::checkout_matches(&path, url, &home_text);
        assert!(rust.is_ok(), "rust matches {leaf}");
        assert_eq!(scode, 0, "shell matches {leaf}");
        assert!(
            String::from_utf8(sout)
                .expect("url dump")
                .starts_with("rc=0\nreply="),
            "url dump for {leaf}"
        );
    }
}

#[test]
fn preflight_agrees() {
    let dir = TempDir::new("ov-preflight").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    stage(&home.join("good"), "home/app.conf", b"ok\n");
    let good = home.join("good");
    let good_text = good.to_string_lossy().into_owned();
    let bad = home.join("bad");
    let bad_text = bad.to_string_lossy().into_owned();
    let records = vec![
        format!("good|{good_text}|||false|none"),
        format!("bad|{bad_text}|||false|none"),
        "git|/nowhere|||false|git".to_string(),
        "weird|/nowhere|||false|hg".to_string(),
    ];
    let mut argv: Vec<&std::ffi::OsStr> = Vec::new();
    for record in &records {
        argv.push(record.as_ref());
    }
    let (_, sout, serr) = shell_run(
        home,
        &argv,
        &[],
        "OVERLAYS=(\"${@:2}\"); _preflight_local_overlays; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    let scode = dump_rc(&sout);
    let mut state = overlays::State {
        overlays: records.clone(),
        ..Default::default()
    };
    let rust = overlays::preflight(&mut state, &home_text);
    let shell = String::from_utf8(sout).expect("preflight dump");
    match rust {
        Ok(()) => {
            assert_eq!(scode, 0, "preflight code");
            assert_eq!(shell, "rc=0\n", "preflight dump");
        }
        Err(warning) => {
            assert_eq!(scode, 1, "preflight code");
            assert_eq!(shell, "rc=1\n", "preflight dump");
            assert_eq!(
                String::from_utf8(serr).expect("preflight stderr"),
                format!("{warning}\n"),
                "preflight stderr"
            );
            assert_eq!(state.warnings, vec![warning], "preflight warnings");
        }
    }
    // Healthy-only preflight passes quietly on both sides.
    let healthy = vec![format!("good|{good_text}|||false|none")];
    let mut argv: Vec<&std::ffi::OsStr> = Vec::new();
    for record in &healthy {
        argv.push(record.as_ref());
    }
    let (_, sout, serr) = shell_run(
        home,
        &argv,
        &[],
        "OVERLAYS=(\"${@:2}\"); _preflight_local_overlays; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    let scode = dump_rc(&sout);
    let mut state = overlays::State::default();
    state.overlays.clone_from(&healthy);
    assert!(overlays::preflight(&mut state, &home_text).is_ok());
    assert_eq!(scode, 0, "healthy preflight code");
    assert_eq!(
        String::from_utf8(sout).expect("dump"),
        "rc=0\n",
        "healthy dump"
    );
    assert_eq!(serr, b"", "healthy stderr");
}

#[test]
fn use_set_agrees() {
    let dir = TempDir::new("ov-useset").expect("fixture dir");
    let home = dir.path();
    for kind in ["eligible", "active", "bogus"] {
        let (_, sout, _) = shell_run(
            home,
            &[kind.as_ref()],
            &[],
            "ELIGIBLE_OVERLAYS=(e1 e2); ACTIVE_OVERLAYS=(a1); _dot_overlay_use_set \"$2\"; rc=$?; printf 'rc=%s\\n' \"$rc\"; printf 'overlays<<<%s>>>\\n' \"${OVERLAYS[*]}\"",
        );
        let scode = dump_rc(&sout);
        let mut state = overlays::State {
            eligible: vec!["e1".to_string(), "e2".to_string()],
            active: vec!["a1".to_string()],
            ..Default::default()
        };
        let rcode = overlays::use_set(&mut state, kind);
        let (rcode, joined) = match rcode {
            Ok(()) => (0, state.overlays.join(" ")),
            Err(error) => (error.code(), String::new()),
        };
        assert_eq!(scode, rcode, "use_set code for {kind}");
        if scode == 0 {
            let shell_joined = String::from_utf8(sout)
                .expect("use_set dump")
                .lines()
                .nth(1)
                .unwrap_or("")
                .to_string();
            assert_eq!(
                format!("overlays<<<{joined}>>>\n"),
                format!("{shell_joined}\n"),
                "use_set dump for {kind}"
            );
        }
    }
}

#[test]
fn conf_dir_agrees() {
    let dir = TempDir::new("ov-confdir").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    // Empty XDG falls back to the home default; custom roots
    // resolve verbatim. `None` scrubs the variable outright.
    for xdg in ["", "/custom/config"] {
        let env: Vec<(&str, Option<&str>)> = if xdg.is_empty() {
            vec![("XDG_CONFIG_HOME", None)]
        } else {
            vec![("XDG_CONFIG_HOME", Some(xdg))]
        };
        let (scode, sout, _) = shell_run(
            home,
            &[],
            &env,
            "_overlay_conf_dir; rc=$?; printf 'rc=%s\\nreply=%s\\n' \"$rc\" \"$REPLY\"",
        );
        let rust = overlays::conf_dir(if xdg.is_empty() { "" } else { xdg }, &home_text);
        let shell = String::from_utf8(sout).expect("conf_dir dump");
        match rust {
            // The shell helper prints the directory itself before
            // the dump `printf`.
            Some(dir) => assert_eq!(
                shell,
                format!("{dir}\nrc=0\nreply={dir}\n"),
                "conf_dir {xdg:?}"
            ),
            None => assert_eq!(scode, 0, "conf_dir shell code {xdg:?}"),
        }
    }
}
