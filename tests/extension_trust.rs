//! Differential parity tests for `extension-trust.sh` against the
//! live shell: stat gates, root/component validation, symlink
//! authorization, entry-point validation, and the retiring
//! resolver.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::extension_trust::{self, Inputs};
use dot::test_support::TempDir;

/// Run one shell snippet with the trust libraries sourced.
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
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(
            ". \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/platform.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/temp.sh\"\n. \"$1/lib/dot/resources.sh\"\n. \"$1/lib/dot/overlay-context.sh\"\n. \"$1/lib/dot/overlays.sh\"\n. \"$1/lib/dot/repos/config.sh\"\n. \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/extension-trust.sh\"\n{snippet}"
        ));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
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

/// Helper status from a dump harness: the process exit is always 0
/// (the dump `printf` runs last).
fn dump_rc(dump: &[u8]) -> i32 {
    let line = dump.split(|byte| *byte == b'\n').next().unwrap_or(b"");
    let line = line.strip_prefix(b"rc=").unwrap_or(b"");
    std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(-1)
}

/// Write `bytes` to `dir/name` with `mode`, creating parents.
fn stage_mode(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Make a directory (and parents) with `mode`.
fn mkdir_mode(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).expect("mkdir");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Current euid for ownership-gated checks.
fn euid() -> u32 {
    dot::temp::current_uid().expect("current uid")
}

/// Base [`Inputs`] for a fixture home with extensions at `ext`.
fn inputs(home: &Path) -> Inputs {
    let home_text = home.to_string_lossy().into_owned();
    Inputs {
        euid: euid(),
        home: home_text.clone(),
        extensions_dir: format!("{home_text}/ext"),
        manifest: String::new(),
        retiring_root: String::new(),
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

#[test]
fn stat_shapes_agree() {
    let dir = TempDir::new("ext-stat").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    // The group/other *write* bit is the only mode that matters;
    // read/exec bits, zero modes, and links decide the rest.
    let f600 = stage_mode(home, "f600", b"x", 0o600);
    let f644 = stage_mode(home, "f644", b"x", 0o644);
    let f640 = stage_mode(home, "f640", b"x", 0o640);
    let f622 = stage_mode(home, "f622", b"x", 0o622);
    let f602 = stage_mode(home, "f602", b"x", 0o602);
    let f000 = stage_mode(home, "f000", b"x", 0o000);
    let d700 = mkdir_mode(home, "d700", 0o700);
    let d755 = mkdir_mode(home, "d755", 0o755);
    std::os::unix::fs::symlink("f600", home.join("link")).expect("symlink");
    std::os::unix::fs::symlink("absent", home.join("dangling")).expect("symlink");
    std::fs::hard_link(&f600, home.join("hard")).expect("hard link");
    // `f600` carries the hard link below, so its row would
    // duplicate `hard`; the remaining modes pin both outcomes.
    for (label, path) in [
        ("f644", f644.clone()),
        ("f640", f640.clone()),
        ("f622", f622.clone()),
        ("f602", f602.clone()),
        ("f000", f000.clone()),
        ("d700", d700.clone()),
        ("d755", d755.clone()),
        ("link", home.join("link")),
        ("dangling", home.join("dangling")),
        ("hard", home.join("f600")),
        ("dir", d700.clone()),
        ("missing", home.join("gone")),
    ] {
        for (gate, snippet, rust) in [
            (
                "file",
                "_dot_extension_file_stat \"$2\"",
                extension_trust::file_stat(&path, uid),
            ),
            (
                "dir",
                "_dot_extension_directory_stat \"$2\"",
                extension_trust::directory_stat(&path, uid),
            ),
        ] {
            let (code, out, serr) = shell_run(
                home,
                &[path.as_os_str()],
                &[],
                &format!("if {snippet}; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi"),
            );
            assert_eq!(code, 0, "shell harness {gate} {label}");
            assert_eq!(
                format!("rc={}\n", i32::from(!rust)),
                String::from_utf8(out).expect("stat dump"),
                "{gate} code for {label}"
            );
            assert_eq!(serr, b"", "{gate} stderr for {label}");
        }
    }
}

#[test]
fn root_shapes_agree() {
    let dir = TempDir::new("ext-root").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    let home_text = home.to_string_lossy().into_owned();
    let good = mkdir_mode(home, "good", 0o700);
    let open = mkdir_mode(home, "open", 0o755);
    let afile = stage_mode(home, "afile", b"x", 0o600);
    std::os::unix::fs::symlink("good", home.join("linkdir")).expect("symlink");
    // Absolute normalized owned directories pass; everything else
    // in the case list fails identically on both sides.
    let candidates = [
        good.to_string_lossy().into_owned(),
        open.to_string_lossy().into_owned(),
        afile.to_string_lossy().into_owned(),
        home.join("linkdir").to_string_lossy().into_owned(),
        home.join("gone").to_string_lossy().into_owned(),
        String::new(),
        "/".to_string(),
        "relative".to_string(),
        format!("{home_text}/"),
        format!("{home_text}//x"),
        format!("{home_text}/./x"),
        format!("{home_text}/x/."),
        format!("{home_text}/../x"),
        format!("{home_text}/x/.."),
        format!("{home_text}/a\nb"),
        format!("{home_text}/a\rb"),
    ];
    for candidate in &candidates {
        let (code, out, serr) = shell_run(
            home,
            &[candidate.as_ref()],
            &[],
            "DOT_EXTENSIONS_DIR=\"$2\"; if _dot_extension_root_validate; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
        );
        assert_eq!(code, 0, "shell harness root {candidate:?}");
        assert_eq!(
            format!(
                "rc={}\n",
                i32::from(!extension_trust::root_validate(candidate, uid))
            ),
            String::from_utf8(out).expect("root dump"),
            "root code for {candidate:?}"
        );
        assert_eq!(serr, b"", "root stderr for {candidate:?}");
    }
}

#[test]
fn components_walk_agrees() {
    let dir = TempDir::new("ext-walk").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    let home_text = home.to_string_lossy().into_owned();
    let ext = mkdir_mode(home, "ext", 0o700);
    let ext_text = ext.to_string_lossy().into_owned();
    mkdir_mode(&ext, "ok/sub", 0o700);
    mkdir_mode(&ext, "open", 0o755);
    stage_mode(&ext, "afile", b"x", 0o600);
    std::os::unix::fs::symlink("ok", ext.join("link")).expect("symlink");
    // Leaf existence is never checked: only the shape and the
    // parent-component walk decide.
    let leaves = [
        "tool",
        "ok/tool",
        "ok/sub/deep",
        "open/tool",
        "afile/tool",
        "link/tool",
        "gone/tool",
        "ok",
        ".",
        "..",
        "a/../b",
        "a/./b",
        "a//b",
        "a/",
        "a/.",
        "a/..",
        "trailing/",
    ];
    for leaf in leaves {
        let path = format!("{ext_text}/{leaf}");
        let (code, out, serr) = shell_run(
            home,
            &[path.as_ref()],
            &[],
            "DOT_EXTENSIONS_DIR=\"$HOME/ext\"; if _dot_extension_parent_components_validate \"$2\"; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
        );
        assert_eq!(code, 0, "shell harness walk {leaf:?}");
        assert_eq!(
            format!(
                "rc={}\n",
                i32::from(!extension_trust::parent_components_validate(
                    Path::new(&path),
                    &ext_text,
                    uid
                ))
            ),
            String::from_utf8(out).expect("walk dump"),
            "walk code for {leaf:?}"
        );
        assert_eq!(serr, b"", "walk stderr for {leaf:?}");
    }
    // Paths outside the root, and the owned-root variant with an
    // explicit root, behave the same on both sides.
    let outside = format!("{home_text}/elsewhere/tool");
    let (code, out, _) = shell_run(
        home,
        &[outside.as_ref()],
        &[],
        "DOT_EXTENSIONS_DIR=\"$HOME/ext\"; _dot_extension_parent_components_validate \"$2\"; printf 'rc=%s\\n' \"$?\"",
    );
    assert_eq!(code, 0, "shell harness outside");
    assert_eq!(
        format!(
            "rc={}\n",
            i32::from(!extension_trust::parent_components_validate(
                Path::new(&outside),
                &ext_text,
                uid
            ))
        ),
        String::from_utf8(out).expect("outside dump"),
        "outside code"
    );
    let custom = format!("{home_text}/custom/sub/leaf");
    mkdir_mode(home, "custom/sub", 0o700);
    let (code, out, serr) = shell_run(
        home,
        &[custom.as_ref(), home_text.as_ref()],
        &[],
        "if _dot_extension_owned_parent_components_validate \"$3\" \"$2\"; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
    );
    assert_eq!(code, 0, "shell harness owned walk");
    assert_eq!(
        format!(
            "rc={}\n",
            i32::from(!extension_trust::owned_parent_components_validate(
                &home_text, &custom, uid
            ))
        ),
        String::from_utf8(out).expect("owned dump"),
        "owned walk code"
    );
    assert_eq!(serr, b"", "owned walk stderr");
}

/// Stage the shared symlink-authorization fixture: a matching git
/// checkout with a source tree, a manifest, and an `OVERLAYS`
/// record. Returns `(manifest, record)`.
fn stage_authorized(home: &Path, name: &str, origin: &str) -> (PathBuf, String) {
    let home_text = home.to_string_lossy().into_owned();
    let checkout = home.join(format!(".dotfiles-{name}"));
    git_repo(&checkout, Some(origin));
    stage_mode(&checkout, "home/app.conf", b"ok\n", 0o644);
    let manifest = stage_mode(home, "manifest", b"app.conf\tweb\n", 0o644);
    let record =
        format!("web|{home_text}/.dotfiles-web|{origin}|{home_text}/conf/10-web.conf|false|git");
    (manifest, record)
}

#[test]
fn symlink_authorized_agrees() {
    let dir = TempDir::new("ext-auth").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let uid = euid();
    let (manifest, record) = stage_authorized(home, "web", "file:///repo/web.git");
    let manifest_text = manifest.to_string_lossy().into_owned();
    // The HOME link uses the generated relative target.
    std::os::unix::fs::symlink(".dotfiles-web/home/app.conf", home.join("app.conf"))
        .expect("symlink");
    std::os::unix::fs::symlink(".dotfiles-web/home/app.conf", home.join("wrong.conf"))
        .expect("symlink");
    // A dangling link still reads (but never matches), like
    // `readlink` on an unresolved link.
    std::os::unix::fs::symlink("absent-target", home.join("dangling")).expect("symlink");
    // (label, link, manifest body, record override): every row
    // agrees on the code with silent stderr.
    type AuthCase = (
        &'static str,
        &'static str,
        Option<&'static [u8]>,
        Option<&'static str>,
    );
    let cases: &[AuthCase] = &[
        ("match", "app.conf", None, None),
        ("rel-miss", "wrong.conf", None, None),
        ("dangling-match", "dangling", Some(b"dangling\tweb\n"), None),
        ("target-miss", "app.conf", Some(b"other.conf\tweb\n"), None),
        ("unparsed", "app.conf", Some(b"no-tabs\n"), None),
        (
            "origin-miss",
            "app.conf",
            None,
            Some("web|{h}/.dotfiles-web|file:///repo/other.git|{h}/conf/10-web.conf|false|git"),
        ),
        (
            "sync-none",
            "app.conf",
            None,
            Some("web|{h}/.dotfiles-web||{h}/conf/10-web.conf|false|none"),
        ),
        (
            "owner-miss",
            "app.conf",
            None,
            Some("ghost|{h}/.dotfiles-web|file:///repo/web.git|{h}/conf/10-web.conf|false|git"),
        ),
    ];
    for (label, link, body, record_override) in cases {
        if let Some(body) = body {
            std::fs::write(&manifest, body).expect("rewrite manifest");
        } else {
            std::fs::write(&manifest, b"app.conf\tweb\n").expect("reset manifest");
        }
        let record = match record_override {
            Some(template) => template.replace("{h}", &home_text),
            None => record.clone(),
        };
        let link_path = home.join(link);
        let (_, sout, serr) = shell_run(
            home,
            &[link_path.as_os_str(), manifest.as_os_str(), record.as_ref()],
            &[],
            "DOT_OVERLAY_MANIFEST=\"$3\"; OVERLAYS=(\"$4\"); _dot_extension_symlink_authorized \"$2\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
        );
        let scode = dump_rc(&sout);
        let rust = extension_trust::symlink_authorized(
            &link_path,
            &home_text,
            &manifest_text,
            std::slice::from_ref(&record),
            uid,
        );
        assert_eq!(scode, i32::from(!rust), "authorized code for {label}");
        assert_eq!(
            format!("rc={}\n", i32::from(!rust)),
            String::from_utf8(sout).expect("auth dump"),
            "authorized dump for {label}"
        );
        assert_eq!(serr, b"", "authorized stderr for {label}");
    }
    // A non-link and a missing manifest refuse on both sides.
    stage_mode(home, "plain", b"x", 0o644);
    for (label, link, manifest_arg) in [
        ("plain", "plain", manifest_text.clone()),
        ("no-manifest", "app.conf", String::new()),
    ] {
        let link_path = home.join(link);
        let (_, sout, serr) = shell_run(
            home,
            &[link_path.as_os_str(), manifest_arg.as_ref()],
            &[],
            "DOT_OVERLAY_MANIFEST=\"$3\"; OVERLAYS=(); _dot_extension_symlink_authorized \"$2\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
        );
        let scode = dump_rc(&sout);
        let rust =
            extension_trust::symlink_authorized(&link_path, &home_text, &manifest_arg, &[], uid);
        assert_eq!(scode, i32::from(!rust), "refused code for {label}");
        assert_eq!(serr, b"", "refused stderr for {label}");
    }
}

/// Stage the shared entry-point fixture: extensions root, an
/// authorized HOME link, and the retiring checkout with a
/// deactivation script. Returns the inputs with manifest set.
fn stage_entrypoint(home: &Path) -> (Inputs, String) {
    let ext = mkdir_mode(home, "ext", 0o700);
    let ext_text = ext.to_string_lossy().into_owned();
    let (manifest, record) = stage_authorized(home, "web", "file:///repo/web.git");
    let manifest_text = manifest.to_string_lossy().into_owned();
    // Entry point inside the extensions root, authorized through
    // the manifest like the HOME link.
    std::fs::write(&manifest, b"ext/tool\tweb\n").expect("manifest");
    let checkout = home.join(".dotfiles-web");
    stage_mode(&checkout, "home/ext/tool", b"tool\n", 0o644);
    std::os::unix::fs::symlink("../.dotfiles-web/home/ext/tool", ext.join("tool"))
        .expect("symlink");
    // Retiring checkout with its fixed entry point.
    let retiring = home.join(".dotfiles-web");
    stage_mode(&retiring, "dot/profile-deactivate", b"#!/bin/sh\n", 0o600);
    let mut inputs = inputs(home);
    inputs.extensions_dir = ext_text;
    inputs.manifest = manifest_text;
    inputs.retiring_root = retiring.to_string_lossy().into_owned();
    (inputs, record)
}

#[test]
fn file_validate_agrees() {
    let dir = TempDir::new("ext-fileval").expect("fixture dir");
    let home = dir.path();
    let (inputs, record) = stage_entrypoint(home);
    let ext = PathBuf::from(&inputs.extensions_dir);
    let overlays = vec![record];
    stage_mode(&ext, "plain", b"x", 0o644);
    stage_mode(&ext, "locked", b"x", 0o000);
    mkdir_mode(&ext, "adir", 0o700);
    // (label, leaf): links resolve through the manifest while
    // plain files only need clean stats.
    for (label, leaf) in [
        ("plain", "plain"),
        ("link", "tool"),
        ("locked", "locked"),
        ("dir", "adir"),
        ("missing", "gone"),
        ("outside", "../plain"),
    ] {
        let path = ext.join(leaf);
        let (code, out, serr) = shell_run(
            home,
            &[path.as_os_str()],
            &[],
            "DOT_EXTENSIONS_DIR=\"$HOME/ext\"; OVERLAYS=(); _dot_extension_file_validate \"$2\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
        );
        assert_eq!(code, 0, "shell harness file {label}");
        // The authorized link needs the record; plain rows pass
        // (or refuse) with an empty set on both sides.
        let with_record = extension_trust::file_validate(&path, &inputs, &overlays);
        let without_record = extension_trust::file_validate(&path, &inputs, &[]);
        let (scode, shell_empty) = (dump_rc(&out), String::from_utf8(out).expect("file dump"));
        if label == "link" {
            let (_, sout, _) = shell_run(
                home,
                &[path.as_os_str(), overlays[0].as_ref()],
                &[],
                "DOT_EXTENSIONS_DIR=\"$HOME/ext\"; DOT_OVERLAY_MANIFEST=\"$HOME/manifest\"; OVERLAYS=(\"$3\"); _dot_extension_file_validate \"$2\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
            );
            assert_eq!(
                dump_rc(&sout),
                i32::from(!with_record),
                "file code for {label}"
            );
            assert_eq!(
                format!("rc={}\n", i32::from(!with_record)),
                String::from_utf8(sout).expect("link dump"),
                "file dump for {label}"
            );
        } else {
            assert_eq!(scode, i32::from(!without_record), "file code for {label}");
            assert_eq!(
                format!("rc={}\n", i32::from(!without_record)),
                shell_empty,
                "file dump for {label}"
            );
        }
        assert_eq!(serr, b"", "file stderr for {label}");
    }
}

#[test]
fn deactivation_validate_agrees() {
    let dir = TempDir::new("ext-deact").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let uid = euid();
    let (inputs, record) = stage_entrypoint(home);
    let retiring = PathBuf::from(&inputs.retiring_root);
    let script = retiring.join("dot/profile-deactivate");
    let script_text = script.to_string_lossy().into_owned();
    // (label, record, script): the fixed entry-point spelling, git
    // identity, and clean stats all have to hold.
    let bad_record = "G|/x|u|/d/10-g.conf|false|git".to_string();
    let cases: &[(&str, String, String)] = &[
        ("good", record.clone(), script_text.clone()),
        ("bad-record", bad_record, script_text.clone()),
        (
            "sync-none",
            record.replace("|false|git", "|false|none"),
            script_text.clone(),
        ),
        (
            "path-miss",
            record.replace(".dotfiles-web|", ".dotfiles-ghost|"),
            script_text.clone(),
        ),
        (
            "script-miss",
            record.clone(),
            format!("{home_text}/elsewhere"),
        ),
        (
            "origin-miss",
            record.replace("file:///repo/web.git", "file:///repo/other.git"),
            script_text.clone(),
        ),
    ];
    for (label, record, script) in cases {
        let (code, out, serr) = shell_run(
            home,
            &[record.as_ref(), script.as_ref()],
            &[],
            "_dot_profile_deactivation_validate \"$2\" \"$3\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
        );
        assert_eq!(code, 0, "shell harness deactivation {label}");
        let rust = extension_trust::deactivation_validate(record, script, &home_text, uid);
        let rcode = match rust {
            Ok(()) => 0,
            Err(error) => error.code(),
        };
        assert_eq!(dump_rc(&out), rcode, "deactivation code for {label}");
        assert_eq!(
            format!("rc={rcode}\n"),
            String::from_utf8(out).expect("deactivation dump"),
            "deactivation dump for {label}"
        );
        assert_eq!(serr, b"", "deactivation stderr for {label}");
    }
    // A wrong script spelling refuses on both sides.
    std::os::unix::fs::symlink("profile-deactivate", retiring.join("dot/alias")).expect("symlink");
    let alias = retiring.join("dot/alias").to_string_lossy().into_owned();
    let (code, out, _) = shell_run(
        home,
        &[record.as_ref(), alias.as_ref()],
        &[],
        "_dot_profile_deactivation_validate \"$2\" \"$3\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    assert_eq!(code, 0, "shell harness alias");
    assert_eq!(
        dump_rc(&out),
        extension_trust::deactivation_validate(&record, &alias, &home_text, uid)
            .err()
            .map(|error| error.code())
            .unwrap_or(0),
        "alias code"
    );
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o620)).expect("chmod");
    let (code, out, serr) = shell_run(
        home,
        &[record.as_ref(), script_text.as_ref()],
        &[],
        "_dot_profile_deactivation_validate \"$2\" \"$3\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    assert_eq!(code, 0, "shell harness open script");
    assert_eq!(
        dump_rc(&out),
        extension_trust::deactivation_validate(&record, &script_text, &home_text, uid)
            .err()
            .map(|error| error.code())
            .unwrap_or(0),
        "open script code"
    );
    assert_eq!(serr, b"", "open script stderr");
    // A symlink at the fixed spelling refuses on both sides.
    std::fs::remove_file(&script).expect("remove script");
    std::os::unix::fs::symlink("alias", &script).expect("symlink");
    let (code, out, serr) = shell_run(
        home,
        &[record.as_ref(), script_text.as_ref()],
        &[],
        "_dot_profile_deactivation_validate \"$2\" \"$3\"; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    assert_eq!(code, 0, "shell harness link script");
    assert_eq!(
        dump_rc(&out),
        extension_trust::deactivation_validate(&record, &script_text, &home_text, uid)
            .err()
            .map(|error| error.code())
            .unwrap_or(0),
        "link script code"
    );
    assert_eq!(serr, b"", "link script stderr");
}

#[test]
fn retiring_overlay_file_agrees() {
    let dir = TempDir::new("ext-retire").expect("fixture dir");
    let home = dir.path();
    let (inputs, _) = stage_entrypoint(home);
    stage_mode(
        Path::new(&inputs.retiring_root),
        "support/deep.conf",
        b"ok\n",
        0o644,
    );
    mkdir_mode(Path::new(&inputs.retiring_root), "open", 0o755);
    // (label, relative): malformed shapes are usage errors (2),
    // failed checks refuse (1), all silent.
    for (label, relative) in [
        ("file", "support/deep.conf"),
        ("dotfile", "dot/profile-deactivate"),
        ("missing", "gone.conf"),
        ("dir", "support"),
        ("empty", ""),
        ("absolute", "/abs"),
        ("dot", "."),
        ("dotdot", ".."),
        ("dot-slash", "./x"),
        ("dotdot-slash", "../x"),
        ("mid-dot", "a/./b"),
        ("mid-dotdot", "a/../b"),
        ("trailing-dot", "a/."),
        ("trailing-dotdot", "a/.."),
        ("trailing-slash", "a/"),
        ("doubleslash", "a//b"),
        ("open-dir", "open/tool"),
    ] {
        let (code, out, serr) = shell_run(
            home,
            &[relative.as_ref()],
            &[],
            "DOT_RETIRING_OVERLAY_ROOT=\"$HOME/.dotfiles-web\"; if dot_retiring_overlay_file \"$2\"; then printf 'rc=0\\nreply=%s\\n' \"$REPLY\"; else printf 'rc=%s\\n' \"$?\"; fi",
        );
        assert_eq!(code, 0, "shell harness retiring {label}");
        let shell = String::from_utf8(out).expect("retiring dump");
        let rust = extension_trust::retiring_overlay_file(relative, &inputs);
        match rust {
            Ok(reply) => assert_eq!(
                format!("rc=0\nreply={reply}\n"),
                shell,
                "retiring dump for {label}"
            ),
            Err(error) => assert_eq!(
                format!("rc={}\n", error.code()),
                shell,
                "retiring code for {label}"
            ),
        }
        assert_eq!(serr, b"", "retiring stderr for {label}");
    }
    // An unreadable file refuses on both sides.
    stage_mode(Path::new(&inputs.retiring_root), "locked.conf", b"x", 0o000);
    let (code, out, serr) = shell_run(
        home,
        &[std::ffi::OsStr::new("locked.conf")],
        &[],
        "DOT_RETIRING_OVERLAY_ROOT=\"$HOME/.dotfiles-web\"; dot_retiring_overlay_file \"$2\" >/dev/null; rc=$?; printf 'rc=%s\\n' \"$rc\"",
    );
    assert_eq!(code, 0, "shell harness locked");
    assert_eq!(
        dump_rc(&out),
        extension_trust::retiring_overlay_file("locked.conf", &inputs)
            .err()
            .map(|error| error.code())
            .unwrap_or(0),
        "locked code"
    );
    assert_eq!(serr, b"", "locked stderr");
}
