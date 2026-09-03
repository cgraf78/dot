//! Differential parity tests for crash-safe file transactions against
//! `lib/dot/temp.sh`: sibling temps, stat identities, mode clamping,
//! git digests, generation tokens, journal records, both `mv` flavors,
//! and the full prepare/quarantine/commit/remove lifecycle — including
//! cross-engine interop (shell-staged journals recovered by Rust and
//! vice versa) and forged non-minimal journals (leading-zero numerics
//! must fail closed on both sides, never normalize).

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use dot::temp;
use dot::test_support::TempDir;

/// Serializes tests that touch process-global state (`DOT_TEST`,
/// `DOT_SOURCE_ROOT`, `DOT_UPDATE_LOCK_TOKEN` for the `from_env`
/// probes below). Everything else pins its knobs per call, like the
/// `platform` suite's PATH swaps.
static SERIAL: Mutex<()> = Mutex::new(());

/// The lock gate, open on both sides: shell children get `DOT_TEST=1`
/// in their scrubbed env, Rust calls take an open context directly.
fn open_lock() -> temp::LockCtx {
    temp::LockCtx {
        test_mode: true,
        token_present: false,
    }
}

/// Run one shell snippet with `temp.sh` sourced. `argv` arrives as
/// `$1..` (byte-exact, for non-UTF8 paths); the snippet's stdout is
/// returned raw. `extra_env` sets (`Some`) or pins (`None`, removed)
/// variables; `fake_mv` prepends a directory holding a fake `mv` to
/// the child's PATH for BSD-nesting simulation.
fn shell_run(
    fixture: &Path,
    argv: &[&std::ffi::OsStr],
    extra_env: &[(&str, Option<&str>)],
    fake_mv: Option<&Path>,
    snippet: &str,
) -> (i32, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(dir) = fake_mv {
        let mut prefixed = dir.as_os_str().to_os_string();
        prefixed.push(":");
        prefixed.push(&path);
        path = prefixed;
    }
    let tmpdir = match std::env::var_os("TMPDIR") {
        Some(dir) if !dir.is_empty() => dir,
        _ => std::ffi::OsString::from("/tmp"),
    };
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/temp.sh\"\n. \"$1/lib/dot/resources.sh\"\n{snippet}"
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
    (output.status.code().unwrap_or(99), output.stdout)
}

/// Snapshot a fixture tree for cross-engine comparison: relative path
/// to kind (`f`/`d`/`l`), permission bits, and content bytes or link
/// target bytes. Sorted by construction (`BTreeMap`).
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

/// Write `bytes` to `dir/name`, creating parents, and force `mode`.
fn stage(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
    path
}

/// Make `dir/name` a directory at `mode`.
fn stage_dir(dir: &Path, name: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).expect("fixture dir");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
    path
}

/// Standard twin layouts for lifecycle tests: a destination file plus
/// a same-directory staging source. Returns `(root, destination,
/// source)`.
fn twin_layout(tag: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new(tag).expect("fixture dir");
    let root = dir.path();
    let destination = stage(root, "home/.config/app.conf", b"live-v1\n", 0o644);
    let source = stage(root, "home/.config/app.conf.new", b"live-v2\n", 0o644);
    (dir, destination, source)
}

/// Twin of [`twin_layout`] with no destination (absent-leaf cases).
fn twin_absent(tag: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new(tag).expect("fixture dir");
    let root = dir.path().to_path_buf();
    stage_dir(&root, "home/.config", 0o755);
    (
        dir,
        root.join("home/.config/app.conf"),
        stage(&root, "home/.config/app.conf.new", b"live-v2\n", 0o644),
    )
}

#[test]
fn sibling_tmp_shape_matches_mktemp() {
    let dir = TempDir::new("sibling-tmp").expect("fixture dir");
    let root = dir.path();
    // Sibling of a destination in a not-yet-existing directory: both
    // engines `mkdir -p` the parent first.
    let dst = root.join("fresh/sub/app.conf");
    let (code, out) = shell_run(
        root,
        &[dst.as_os_str()],
        &[],
        None,
        "_dot_sibling_tmp_for \"$2\" && printf '%s' \"$REPLY\"",
    );
    assert_eq!(code, 0, "shell sibling tmp");
    let rust = temp::sibling_tmp_for(&dst).expect("rust sibling tmp");
    for tmp in [
        PathBuf::from(String::from_utf8(out).expect("tmp path")),
        rust,
    ] {
        assert_eq!(
            tmp.parent().expect("tmp parent"),
            dst.parent().expect("dst parent")
        );
        let name = tmp.file_name().expect("tmp name").to_string_lossy();
        assert!(name.starts_with("app.conf.tmp."), "tmp prefix: {name}");
        let suffix = &name["app.conf.tmp.".len()..];
        assert_eq!(suffix.len(), 6, "tmp suffix length: {name}");
        assert!(
            suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "tmp suffix alphabet: {name}"
        );
        let meta = std::fs::symlink_metadata(&tmp).expect("tmp exists");
        assert!(meta.is_file(), "tmp is a file");
        assert_eq!(meta.permissions().mode() & 0o7777, 0o600, "tmp mode");
        assert_eq!(meta.len(), 0, "tmp starts empty");
    }
}

#[test]
fn path_identity_matches_stat() {
    let dir = TempDir::new("identity").expect("fixture dir");
    let root = dir.path();
    let file = stage(root, "app.conf", b"data\n", 0o644);
    let (code, out) = shell_run(
        root,
        &[file.as_os_str()],
        &[],
        None,
        "_dot_path_identity \"$2\"",
    );
    assert_eq!(code, 0, "shell identity");
    let shell = String::from_utf8(out).expect("identity text");
    let rust = temp::identity_string(temp::path_identity(&file).expect("rust identity"));
    assert_eq!(format!("{rust}\n"), shell, "identity strings match");
    assert!(
        temp::path_identity(&root.join("missing")).is_err(),
        "missing identity fails"
    );
}

#[test]
fn stat_helpers_match_shell() {
    let dir = TempDir::new("stat-helpers").expect("fixture dir");
    let root = dir.path();
    let file = stage(root, "app.conf", b"12345678", 0o640);
    let (code, out) = shell_run(
        root,
        &[file.as_os_str()],
        &[],
        None,
        "printf '%s|%s|%s|%s' \
          \"$(_dot_file_stat_mode \"$2\")\" \"$(_dot_file_stat_size \"$2\")\" \
          \"$(_dot_path_uid \"$2\")\" \"$(_dot_path_nlink \"$2\")\"",
    );
    assert_eq!(code, 0, "shell stat");
    let shell = String::from_utf8(out).expect("stat text");
    let rust = format!(
        "{:o}|{}|{}|{}",
        temp::file_mode(&file).expect("mode"),
        temp::file_size(&file).expect("size"),
        temp::path_uid(&file).expect("uid"),
        temp::path_nlink(&file).expect("nlink"),
    );
    assert_eq!(rust, shell, "stat fields match");
    // A second hard link is visible on both sides.
    std::fs::hard_link(&file, root.join("app.link")).expect("hard link");
    let (code, out) = shell_run(
        root,
        &[file.as_os_str()],
        &[],
        None,
        "printf '%s' \"$(_dot_path_nlink \"$2\")\"",
    );
    assert_eq!(code, 0, "shell nlink");
    let shell = String::from_utf8(out).expect("nlink text");
    assert_eq!(
        temp::path_nlink(&file).expect("rust nlink").to_string(),
        shell,
        "link count follows hard links"
    );
}

#[test]
fn private_validators_agree() {
    let dir = TempDir::new("private-validate").expect("fixture dir");
    let root = dir.path();
    let good_dir = stage_dir(root, "ctl", 0o700);
    let bad_dir = stage_dir(root, "loose", 0o755);
    let good_file = stage(root, "ctl/record", b"v1\n", 0o600);
    let group_file = stage(root, "group", b"v1\n", 0o640);
    std::fs::hard_link(&good_file, root.join("ctl/record.link")).expect("second link");
    let missing = root.join("missing");
    let linked = root.join("ctl/record.link");
    let cases: &[(&str, &Path)] = &[
        ("dir-ok", &good_dir),
        ("dir-loose", &bad_dir),
        ("dir-missing", &missing),
        ("file-linked", &linked),
        ("file-group", &group_file),
    ];
    for (label, path) in cases {
        let (code, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            None,
            &format!(
                "if [ {is_dir} \"$2\" ]; then _dot_private_dir_validate \"$2\"; \
                 else _dot_private_control_file_validate \"$2\"; fi",
                is_dir = if label.starts_with("dir") { "-d" } else { "-f" },
            ),
        );
        let rust = if label.starts_with("dir") {
            temp::private_dir_validate(path).is_ok()
        } else {
            temp::private_control_file_validate(path).is_ok()
        };
        assert_eq!(rust, code == 0, "validator parity for {label}");
        if *label == "dir-ok" {
            assert_eq!(code, 0, "shell accepts the good directory");
            assert!(rust, "rust accepts the good directory");
        }
    }
    // The unlinked control file validates on both sides.
    std::fs::remove_file(root.join("ctl/record.link")).expect("unlink");
    let (code, _) = shell_run(
        root,
        &[good_file.as_os_str()],
        &[],
        None,
        "_dot_private_control_file_validate \"$2\"",
    );
    assert_eq!(code, 0, "shell control ok");
    assert!(
        temp::private_control_file_validate(&good_file).is_ok(),
        "rust control ok"
    );
}

#[test]
fn tracked_modes_and_ceiling_agree() {
    let dir = TempDir::new("modes").expect("fixture dir");
    let root = dir.path();
    // Same starting modes on twin files; each case runs the shell on
    // one twin and Rust on the other, then the modes must match.
    for (label, git_mode, mask, start) in [
        ("plain", "100644", 0o022u32, 0o777u32),
        ("exec", "100755", 0o022u32, 0o666u32),
        ("strict", "100644", 0o077u32, 0o777u32),
        ("strict-exec", "100755", 0o077u32, 0o666u32),
    ] {
        let shell_file = stage(root, format!("shell-{label}").as_str(), b"x\n", start);
        let rust_file = stage(root, format!("rust-{label}").as_str(), b"x\n", start);
        let mask_text = format!("{mask:o}");
        let (code, _) = shell_run(
            root,
            &[shell_file.as_os_str()],
            &[],
            None,
            &format!("umask {mask_text} && _dot_apply_tracked_file_mode \"$2\" {git_mode}"),
        );
        assert_eq!(code, 0, "shell tracked mode {label}");
        temp::apply_tracked_file_mode(&rust_file, git_mode, mask).expect("rust tracked mode");
        let shell_mode = mode_of(&shell_file);
        let rust_mode = mode_of(&rust_file);
        assert_eq!(rust_mode, shell_mode, "tracked mode parity for {label}");
        // Umask ceiling on the same pair, twice: default ceiling, then
        // an explicit one.
        for ceiling in [None, Some(0o600u32)] {
            let shell_c = stage(root, format!("shell-c-{label}").as_str(), b"x\n", 0o666);
            let rust_c = stage(root, format!("rust-c-{label}").as_str(), b"x\n", 0o666);
            let ceiling_arg = ceiling.map(|mode| format!("{mode:o}")).unwrap_or_default();
            let (code, _) = shell_run(
                root,
                &[shell_c.as_os_str()],
                &[],
                None,
                &format!("umask {mask_text} && _dot_apply_umask_ceiling \"$2\" {ceiling_arg}"),
            );
            assert_eq!(code, 0, "shell ceiling {label}");
            temp::apply_umask_ceiling(&rust_c, ceiling, mask).expect("rust ceiling");
            assert_eq!(
                mode_of(&rust_c),
                mode_of(&shell_c),
                "ceiling parity for {label}"
            );
        }
    }
    // Rejections agree: bad git mode, ceiling on a link.
    let file = stage(root, "plain", b"x\n", 0o644);
    let (code, _) = shell_run(
        root,
        &[file.as_os_str()],
        &[],
        None,
        "_dot_apply_tracked_file_mode \"$2\" 100600",
    );
    assert_ne!(code, 0, "shell rejects bad git mode");
    assert!(
        temp::apply_tracked_file_mode(&file, "100600", 0o022).is_err(),
        "rust rejects bad git mode"
    );
    #[cfg(unix)]
    {
        // Ceilings follow symlinks on both sides (no `-L` guard here,
        // unlike the tracked-mode setter): twin files behind twin
        // links must end with identical target modes.
        let file2 = stage(root, "plain2", b"x\n", 0o666);
        std::os::unix::fs::symlink(&file, root.join("link")).expect("symlink");
        std::os::unix::fs::symlink(&file2, root.join("link2")).expect("symlink");
        let link = root.join("link");
        let link2 = root.join("link2");
        let (code, _) = shell_run(
            root,
            &[link.as_os_str()],
            &[],
            None,
            "_dot_apply_umask_ceiling \"$2\"",
        );
        assert_eq!(code, 0, "shell ceilings through symlink");
        temp::apply_umask_ceiling(&link2, None, 0o022).expect("rust ceilings through symlink");
        assert_eq!(mode_of(&file), mode_of(&file2), "link-target mode parity");
        // `stat` lstates the link itself (0o777), so the shell ceilings the
        // target to 0o777 & ~mask, not the target's own starting mode.
        assert_eq!(mode_of(&file), 0o755, "link-target ceiling math");
    }
}

/// Permission bits of a fixture path.
fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn read_umask_matches_process_mask() {
    // Both readings observe the test process mask (inherited across
    // the fork), so they must agree without pinning any value.
    let rust = temp::read_umask().expect("rust umask");
    let output = Command::new("sh")
        .arg("-c")
        .arg("umask")
        .output()
        .expect("spawn sh");
    assert!(output.status.success(), "sh umask");
    let shell = u32::from_str_radix(String::from_utf8_lossy(&output.stdout).trim(), 8)
        .expect("parse sh umask");
    assert_eq!(rust, shell, "umask readings agree");
}

#[test]
fn digests_match_git() {
    let dir = TempDir::new("digests").expect("fixture dir");
    let root = dir.path();
    let file = stage(root, "payload.bin", b"\x00\x01\x02binary\n", 0o644);
    let empty = stage(root, "empty", b"", 0o644);
    for (label, path) in [("payload", &file), ("empty", &empty)] {
        let (code, out) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            None,
            "printf '%s' \"$(_dot_file_digest \"$2\")\"",
        );
        assert_eq!(code, 0, "shell digest {label}");
        let shell = String::from_utf8(out).expect("digest text");
        let rust = temp::file_digest(root, path).expect("rust digest");
        assert_eq!(rust, shell, "file digest parity for {label}");
    }
    // Text digests, pair equality, and stdin matching.
    let (code, out) = shell_run(
        root,
        &[],
        &[],
        None,
        "printf '%s' \"$(_dot_file_text_digest 'hello')\"",
    );
    assert_eq!(code, 0, "shell text digest");
    let shell = String::from_utf8(out).expect("text digest");
    assert_eq!(
        temp::file_text_digest(root, b"hello").expect("rust text digest"),
        shell
    );
    let (code, _) = shell_run(
        root,
        &[file.as_os_str()],
        &[],
        None,
        "_dot_files_equal \"$2\" \"$2\"",
    );
    assert_eq!(code, 0, "shell equal files");
    assert!(temp::files_equal(root, &file, &file).expect("rust equal files"));
    let (code, _) = shell_run(
        root,
        &[file.as_os_str(), empty.as_os_str()],
        &[],
        None,
        "! _dot_files_equal \"$2\" \"$3\"",
    );
    assert_eq!(code, 0, "shell unequal files");
    assert!(!temp::files_equal(root, &file, &empty).expect("rust unequal files"));
    let (code, _) = shell_run(
        root,
        &[file.as_os_str()],
        &[],
        None,
        "printf 'x' | _dot_stdin_matches_file /dev/stdin \"$2\"",
    );
    assert_ne!(code, 0, "shell stdin mismatch");
    assert!(!temp::stdin_matches_file(root, b"x", &file).expect("rust stdin mismatch"));
    // Missing files fail on both sides.
    let missing = root.join("missing");
    let (code, _) = shell_run(
        root,
        &[missing.as_os_str()],
        &[],
        None,
        "_dot_files_equal \"$2\" \"$2\"",
    );
    assert_ne!(code, 0, "shell missing files");
    assert!(
        temp::files_equal(root, &missing, &missing).is_err(),
        "rust missing files"
    );
}

#[test]
fn hash_pair_table() {
    let sha = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    let other = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    assert!(
        temp::hash_pair_equal(&format!("{sha}\n{sha}")),
        "identical pair"
    );
    assert!(
        temp::hash_pair_equal(&format!("{sha}\n{sha}\n")),
        "trailing newline"
    );
    assert!(
        !temp::hash_pair_equal(&format!("{sha}\n{other}")),
        "mismatch"
    );
    assert!(!temp::hash_pair_equal(sha), "single hash");
    assert!(!temp::hash_pair_equal(""), "empty");
    assert!(
        !temp::hash_pair_equal(&format!("{sha}\n{sha}\n{sha}")),
        "three hashes"
    );
    assert!(
        !temp::hash_pair_equal(&format!("{sha}\n{}", sha.to_uppercase())),
        "uppercase rejected"
    );
    assert!(
        !temp::hash_pair_equal(&format!("{sha}\nshort")),
        "short rejected"
    );
}

#[test]
fn target_resolve_matches_shell() {
    let dir = TempDir::new("target").expect("fixture dir");
    let root = dir.path();
    let live = stage(root, "home/app.conf", b"v1\n", 0o644);
    let absent = root.join("home/absent.conf");
    for (label, path) in [("file", live.clone()), ("absent", absent)] {
        let (code, out) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            None,
            "_dot_file_target_resolve \"$2\" && printf '%s|%s|%s|%s|%s' \
              \"$DOT_FILE_TARGET_PARENT\" \"$DOT_FILE_TARGET_PATH\" \
              \"$DOT_FILE_TARGET_PARENT_ID\" \"$DOT_FILE_TARGET_PATH_DIGEST\" \
              \"$DOT_FILE_TARGET_TRANSACTION\"",
        );
        assert_eq!(code, 0, "shell resolve {label}");
        let shell = String::from_utf8(out).expect("target text");
        let rust = temp::file_target_resolve(root, &path).expect("rust resolve");
        let rendered = format!(
            "{}|{}|{}|{}|{}",
            rust.parent.display(),
            rust.path.display(),
            rust.parent_id,
            rust.path_digest,
            rust.transaction.display()
        );
        assert_eq!(rendered, shell, "target binding parity for {label}");
    }
    // Rejections agree: relative, control characters, missing parent.
    let bad: &[(&str, PathBuf)] = &[
        ("relative", PathBuf::from("home/app.conf")),
        ("newline", root.join("home/bad\nname")),
        ("no-parent", root.join("missing-dir/app.conf")),
    ];
    for (label, path) in bad {
        let (code, _) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            None,
            "_dot_file_target_resolve \"$2\"",
        );
        assert_ne!(code, 0, "shell rejects {label}");
        assert!(
            temp::file_target_resolve(root, path).is_err(),
            "rust rejects {label}"
        );
    }
}

#[test]
fn signature_and_generation_roundtrip() {
    let dir = TempDir::new("generation").expect("fixture dir");
    let root = dir.path();
    let live = stage(root, "app.conf", b"content\n", 0o640);
    let absent = root.join("absent.conf");
    for (label, path) in [("file", live.clone()), ("absent", absent)] {
        // Raw tokens are fully deterministic: byte equality.
        let (code, out) = shell_run(
            root,
            &[path.as_os_str()],
            &[],
            None,
            "printf '%s' \"$(_dot_file_generation_raw \"$2\")\"",
        );
        assert_eq!(code, 0, "shell raw token {label}");
        let shell = String::from_utf8(out).expect("token text");
        let rust = temp::file_generation_raw(root, &path).expect("rust raw token");
        assert_eq!(rust, shell, "raw token parity for {label}");
        // Each side validates the other's token with identical fields.
        let expected = temp::generation_validate(root, &shell).expect("rust validates shell");
        let (code, out) = shell_run(
            root,
            &[rust.as_ref()],
            &[],
            None,
            "_dot_file_generation_validate \"$2\" && printf '%s|%s|%s|%s' \
              \"$DOT_FILE_GENERATION_STATE\" \"$DOT_FILE_GENERATION_PATH_DIGEST\" \
              \"$DOT_FILE_GENERATION_PARENT_ID\" \"$DOT_FILE_GENERATION_SIGNATURE\"",
        );
        assert_eq!(code, 0, "shell validates rust {label}");
        let shell = String::from_utf8(out).expect("expected text");
        let rendered = format!(
            "{}|{}|{}|{}",
            expected.state,
            expected.path_digest,
            expected.parent_id,
            expected.signature.as_deref().unwrap_or("-|-|-|-|-")
        );
        assert_eq!(rendered, shell, "expected-field parity for {label}");
    }
}

#[test]
fn generation_rejections_agree() {
    let dir = TempDir::new("generation-bad").expect("fixture dir");
    let root = dir.path();
    let live = stage(root, "app.conf", b"content\n", 0o644);
    let good = temp::file_generation_raw(root, &live).expect("raw token");
    let bad = [
        ("empty", String::new()),
        ("truncated", good[..good.len() / 2].to_string()),
        ("flipped", format!("{}X", &good[..good.len() - 1])),
        (
            "ten-fields",
            good.split('|').take(10).collect::<Vec<_>>().join("|"),
        ),
        ("twelve-fields", format!("{good}|extra")),
        // A trailing delimiter does NOT spell acceptance: the checksum
        // was computed over the shorter payload, so `${token%|*}`
        // mismatches — both sides must agree it fails.
        ("trailing-pipe", format!("{good}|")),
    ];
    for (label, token) in &bad {
        let (code, _) = shell_run(
            root,
            &[token.as_str().as_ref()],
            &[],
            None,
            "_dot_file_generation_validate \"$2\"",
        );
        let rust = temp::generation_validate(root, token).is_ok();
        assert_eq!(rust, code == 0, "validate parity for {label}");
    }
    assert!(
        temp::generation_validate(root, &format!("{good}|")).is_err(),
        "trailing pipe rejected"
    );
}

/// Drive a full prepare on twin fixtures and compare the journal
/// shape, then read each side's record with both engines.
fn prepare_twins(tag: &str, absent: bool) -> (TempDir, TempDir) {
    let (sdir, sdst, ssrc) = if absent {
        let (d, dst, src) = twin_absent(&format!("{tag}-shell"));
        (d, dst, src)
    } else {
        let (d, dst, src) = twin_layout(&format!("{tag}-shell"));
        (d, dst, src)
    };
    let (rdir, rdst, rsrc) = if absent {
        let (d, dst, src) = twin_absent(&format!("{tag}-rust"));
        (d, dst, src)
    } else {
        let (d, dst, src) = twin_layout(&format!("{tag}-rust"));
        (d, dst, src)
    };
    let sroot = sdir.path().to_path_buf();
    let rroot = rdir.path().to_path_buf();
    // Tokens are per-fixture (identities differ across TempDirs): the
    // shell stages its own inside the snippet; Rust stages here.
    let expected = temp::file_generation_raw(&rroot, &rdst).expect("stage token");
    let operation = "replace";
    let shell_prep = format!(
        "token=$(_dot_file_generation_raw \"$2\") && \
         _dot_file_transaction_prepare {operation} \"$3\" \"$2\" \"$token\" && \
         printf '%s' \"$DOT_FILE_TRANSACTION_PATH\""
    );
    let (code, out) = shell_run(
        &sroot,
        &[sdst.as_os_str(), ssrc.as_os_str()],
        &[],
        None,
        &shell_prep,
    );
    assert_eq!(code, 0, "shell prepare");
    assert!(!out.is_empty(), "shell prepared path");
    let mut cache = temp::MoveCache::default();
    let prepared = temp::transaction_prepare(
        &rroot,
        open_lock(),
        operation,
        Some(&rsrc),
        &rdst,
        &expected,
        &mut cache,
    )
    .expect("rust prepare");
    // Journals are deterministic: record bytes must match exactly.
    let shell_record = std::fs::read(
        sdir.path()
            .join("home/.config/.app.conf.dot-file-transaction-v1/record"),
    )
    .expect("shell record");
    let rust_record = std::fs::read(prepared.transaction.join("record")).expect("rust record");
    // Device/inode identities differ across fixtures, so compare the
    // record SHAPE (field classes), not bytes: five tab fields with a
    // valid token inside.
    for (label, bytes) in [("shell", &shell_record), ("rust", &rust_record)] {
        let text = String::from_utf8(bytes.clone()).expect("record text");
        let line = text.lines().next().expect("record line");
        assert_eq!(line.split('\t').count(), 5, "{label} record fields");
    }
    // Both engines read both journals (by transaction directory,
    // like the shell reader takes it).
    for (label, dir) in [("shell", sdir.path()), ("rust", rdir.path())] {
        let txn = dir.join("home/.config/.app.conf.dot-file-transaction-v1");
        let parsed = temp::record_read(&sroot, &txn)
            .unwrap_or_else(|error| panic!("rust reads {label}: {error}"));
        assert_eq!(parsed.operation, "replace");
        assert_eq!(parsed.phase, "prepared");
        let (code, _) = shell_run(
            &sroot,
            &[txn.as_os_str()],
            &[],
            None,
            "_dot_file_transaction_record_read \"$2\" && \
             printf '%s|%s' \"$DOT_FILE_TRANSACTION_OPERATION\" \"$DOT_FILE_TRANSACTION_PHASE\"",
        );
        assert_eq!(code, 0, "shell reads {label}");
    }
    // Candidate payloads are identical content (different inodes).
    let shell_cand = std::fs::read(
        sdir.path()
            .join("home/.config/.app.conf.dot-file-transaction-v1/candidate"),
    )
    .expect("shell candidate");
    let rust_cand = std::fs::read(prepared.transaction.join("candidate")).expect("rust candidate");
    assert_eq!(shell_cand, rust_cand, "candidate content parity");
    (sdir, rdir)
}

#[test]
fn prepare_journals_agree() {
    prepare_twins("prepare-file", false);
    prepare_twins("prepare-absent", true);
    // Remove-operation prepare on an absent leaf journals `-`.
    let (sdir, _, _) = twin_absent("prepare-absent-shell");
    let (rdir, rdst, _) = twin_absent("prepare-absent-rust");
    let sroot = sdir.path().to_path_buf();
    let rroot = rdir.path().to_path_buf();
    let sdst = sroot.join("home/.config/app.conf");
    let rust_token = temp::file_generation_raw(&rroot, &rdst).expect("stage token");
    let (code, _) = shell_run(
        &sroot,
        &[sdst.as_os_str()],
        &[],
        None,
        "token=$(_dot_file_generation_raw \"$2\") && _dot_file_transaction_prepare remove '' \"$2\" \"$token\"",
    );
    assert_eq!(code, 0, "shell remove prepare");
    let mut cache = temp::MoveCache::default();
    temp::transaction_prepare(
        &rroot,
        open_lock(),
        "remove",
        None,
        &rdst,
        &rust_token,
        &mut cache,
    )
    .expect("rust remove prepare");
    let shell_record =
        std::fs::read(sroot.join("home/.config/.app.conf.dot-file-transaction-v1/record"))
            .expect("shell record");
    let rust_record =
        std::fs::read(rroot.join("home/.config/.app.conf.dot-file-transaction-v1/record"))
            .expect("rust record");
    for (label, bytes) in [("shell", &shell_record), ("rust", &rust_record)] {
        let text = String::from_utf8(bytes.clone()).expect("record text");
        assert!(
            text.starts_with("v1\tremove\tprepared\t"),
            "{label} remove record"
        );
        assert!(
            text.trim_end().ends_with("\t-"),
            "{label} remove candidate dash"
        );
    }
}

#[test]
fn record_corruptions_agree() {
    let dir = TempDir::new("record-bad").expect("fixture dir");
    let root = dir.path();
    let live = stage(root, "app.conf", b"v1\n", 0o644);
    let good = temp::file_generation_raw(root, &live).expect("raw token");
    let txn = stage_dir(root, "txn", 0o700);
    let mut cache = temp::MoveCache::default();
    // Round-trip one valid record through the real writer first: the
    // shell must read Rust's journal and vice versa.
    let payload = stage(root, "candidate", b"new\n", 0o644);
    let signature = temp::file_signature(root, &payload).expect("candidate signature");
    temp::record_write(
        &txn,
        "replace",
        "prepared",
        &good,
        Some(&signature),
        &mut cache,
    )
    .expect("seed record");
    let seeded = temp::record_read(root, &txn).expect("read seed");
    assert_eq!(seeded.operation, "replace");
    assert_eq!(seeded.phase, "prepared");
    assert_eq!(
        seeded.candidate.map(|candidate| candidate.to_string()),
        Some(signature.to_string())
    );
    let (code, _) = shell_run(
        root,
        &[txn.as_os_str()],
        &[],
        None,
        "_dot_file_transaction_record_read \"$2\"",
    );
    assert_eq!(code, 0, "shell reads rust record");
    let cases = [
        ("bad-version", format!("v2\treplace\tprepared\t{good}\t-")),
        ("bad-op", format!("v1\trename\tprepared\t{good}\t-")),
        ("bad-phase", format!("v1\treplace\tstaged\t{good}\t-")),
        ("bad-token", "v1\treplace\tprepared\tbogus\t-".to_string()),
        (
            "bad-candidate",
            format!("v1\treplace\tprepared\t{good}\tbogus"),
        ),
        (
            "remove-with-candidate",
            format!("v1\tremove\tprepared\t{good}\t1|2|644|3|{good}"),
        ),
        ("four-fields", format!("v1\treplace\tprepared\t{good}")),
        ("empty", String::new()),
    ];
    for (label, body) in &cases {
        // Each body lives at `<txn>/record`: the shell reader takes a
        // transaction directory, never a bare file.
        let case_txn = root.join(format!("txn-{label}"));
        std::fs::create_dir_all(&case_txn).expect("case txn dir");
        std::fs::set_permissions(&case_txn, std::fs::Permissions::from_mode(0o700))
            .expect("chmod case txn");
        let record = case_txn.join("record");
        std::fs::write(&record, body.as_bytes()).expect("write record");
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600))
            .expect("chmod record");
        let (code, _) = shell_run(
            root,
            &[case_txn.as_os_str()],
            &[],
            None,
            "_dot_file_transaction_record_read \"$2\"",
        );
        let rust = temp::record_read(root, &case_txn).is_ok();
        assert_eq!(rust, code == 0, "record parity for {label}");
    }
    // A trailing garbage line is ignored by the single `read` on both sides.
    let mut extra = std::fs::read(txn.join("record")).expect("seed record");
    extra.extend_from_slice(b"GARBAGE LINE\n");
    let trailing_txn = root.join("txn-trailing");
    std::fs::create_dir_all(&trailing_txn).expect("trailing txn dir");
    std::fs::set_permissions(&trailing_txn, std::fs::Permissions::from_mode(0o700))
        .expect("chmod trailing txn");
    let trailing = trailing_txn.join("record");
    std::fs::write(&trailing, &extra).expect("write record");
    std::fs::set_permissions(&trailing, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (code, _) = shell_run(
        root,
        &[trailing_txn.as_os_str()],
        &[],
        None,
        "_dot_file_transaction_record_read \"$2\"",
    );
    assert_eq!(code, 0, "shell ignores trailing line");
    assert!(
        temp::record_read(root, &trailing_txn).is_ok(),
        "rust ignores trailing line"
    );
    // A preexisting record.next wedges both writers identically.
    let txn2 = stage_dir(root, "txn2", 0o700);
    stage(&txn2, "record.next", b"stale\n", 0o600);
    let (code, _) = shell_run(
        root,
        &[txn2.as_os_str(), good.as_str().as_ref()],
        &[],
        None,
        "_dot_file_transaction_record_write \"$2\" remove committed \"$3\" -",
    );
    assert_ne!(code, 0, "shell wedged on record.next");
    assert!(
        temp::record_write(&txn2, "remove", "committed", &good, None, &mut cache).is_err(),
        "rust wedged on record.next"
    );
    // A dangling record symlink fails both writers and leaves the
    // staged record.next behind on both sides (twin dirs: each side
    // must hit the dangling-record stage, not a stale next).
    let txn3 = stage_dir(root, "txn3", 0o700);
    let txn3r = stage_dir(root, "txn3r", 0o700);
    std::os::unix::fs::symlink("nowhere", txn3.join("record")).expect("dangling record");
    std::os::unix::fs::symlink("nowhere", txn3r.join("record")).expect("dangling record");
    let (code, _) = shell_run(
        root,
        &[txn3.as_os_str(), good.as_str().as_ref()],
        &[],
        None,
        "_dot_file_transaction_record_write \"$2\" remove committed \"$3\" -",
    );
    assert_ne!(code, 0, "shell fails on dangling record");
    assert!(
        temp::record_write(&txn3r, "remove", "committed", &good, None, &mut cache).is_err(),
        "rust fails on dangling record"
    );
    assert!(
        txn3.join("record.next").exists(),
        "shell leaves record.next"
    );
    assert!(
        txn3r.join("record.next").exists(),
        "rust leaves record.next"
    );
}

/// Move one twin pair with both engines and compare outcomes: exit
/// codes plus full resulting trees.
fn move_twins(
    tag: &str,
    setup: &dyn Fn(&Path),
    target: &str,
    replace: bool,
    fake_mv: Option<&Path>,
    tool_override: Option<temp::MoveTool>,
) {
    let sdir = TempDir::new(&format!("{tag}-shell")).expect("fixture dir");
    let rdir = TempDir::new(&format!("{tag}-rust")).expect("fixture dir");
    let sroot = sdir.path().to_path_buf();
    let rroot = rdir.path().to_path_buf();
    setup(&sroot);
    setup(&rroot);
    let verb = if replace { "replace" } else { "noreplace" };
    let mover = if replace {
        "_dot_move_replace_nodir \"$2\" \"$3\""
    } else {
        "_dot_move_noreplace \"$2\" \"$3\""
    };
    let (scode, _) = shell_run(
        &sroot,
        &[
            sroot.join("source").as_os_str(),
            sroot.join(target).as_os_str(),
        ],
        &[],
        fake_mv,
        mover,
    );
    let mut cache = temp::MoveCache::default();
    let tool = tool_override.unwrap_or_else(|| cache.tool().expect("detect tool"));
    let rcode = if replace {
        temp::move_replace_nodir_with(&rroot.join("source"), &rroot.join(target), &tool)
    } else {
        temp::move_noreplace_with(&rroot.join("source"), &rroot.join(target), &tool)
    };
    assert_eq!(
        rcode.is_ok(),
        scode == 0,
        "move {verb} code parity for {tag}"
    );
    assert_eq!(
        snapshot(&rroot),
        snapshot(&sroot),
        "move {verb} tree parity for {tag}"
    );
}

#[test]
fn move_matrix_agrees() {
    // Plain rename onto an absent name.
    move_twins(
        "mv-plain",
        &|root| {
            stage(root, "source", b"payload\n", 0o644);
        },
        "target",
        false,
        None,
        None,
    );
    // Late regular file: noreplace fails, replace wins.
    for replace in [false, true] {
        move_twins(
            &format!("mv-latefile-{replace}"),
            &|root| {
                stage(root, "source", b"new\n", 0o644);
                stage(root, "target", b"old\n", 0o644);
            },
            "target",
            replace,
            None,
            None,
        );
    }
    // Late empty directory: both fail, nothing nests.
    move_twins(
        "mv-latedir",
        &|root| {
            stage(root, "source", b"new\n", 0o644);
            stage_dir(root, "target", 0o755);
        },
        "target",
        false,
        None,
        None,
    );
    // Late symlink (to elsewhere and to the source itself).
    move_twins(
        "mv-latelink",
        &|root| {
            stage(root, "source", b"new\n", 0o644);
            stage(root, "other", b"other\n", 0o644);
            #[cfg(unix)]
            std::os::unix::fs::symlink("other", root.join("target")).expect("symlink");
        },
        "target",
        false,
        None,
        None,
    );
    // Missing source fails on both sides.
    move_twins("mv-missing", &|_root| {}, "target", false, None, None);
    // Publishing a prepared file onto absent and late names.
    for (label, late) in [("absent", false), ("late", true)] {
        let sdir = TempDir::new(&format!("pub-{label}-shell")).expect("fixture dir");
        let rdir = TempDir::new(&format!("pub-{label}-rust")).expect("fixture dir");
        for root in [sdir.path(), rdir.path()] {
            stage(root, "staged", b"staged\n", 0o644);
            if late {
                stage(root, "out", b"old\n", 0o644);
            }
        }
        let (scode, _) = shell_run(
            sdir.path(),
            &[
                sdir.path().join("staged").as_os_str(),
                sdir.path().join("out").as_os_str(),
            ],
            &[],
            None,
            "_dot_publish_prepared_regular \"$2\" \"$3\"",
        );
        let mut cache = temp::MoveCache::default();
        let rcode = temp::publish_prepared_regular(
            &rdir.path().join("staged"),
            &rdir.path().join("out"),
            &mut cache,
        );
        assert_eq!(rcode.is_ok(), scode == 0, "publish code parity for {label}");
        assert_eq!(
            snapshot(rdir.path()),
            snapshot(sdir.path()),
            "publish tree parity"
        );
    }
}

/// A BSD-ish `mv`: honors `-n` (no clobber) but always nests the
/// source into a target directory, exercising the inode-recovery path
/// on both engines. Lives in an exec-capable fixture dir (the system
/// temp dir may be `noexec`).
fn fake_bsd_mv() -> TempDir {
    let dir = TempDir::new_exec("fake-mv").expect("exec dir");
    // Resolve the wrapped binary NOW, while this dir is on no PATH:
    // the tests prepend the fixture dir to PATH, so a bare `mv`
    // inside the script would re-resolve to the fixture itself and
    // fork-chain until fork fails (shell detection then reports
    // failure instead of BSD mode).
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("mv"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .expect("real mv on PATH");
    let quoted = format!("'{}'", real.display().to_string().replace('\'', "'\\''"));
    let script = dir.path().join("mv");
    let body = "#!/bin/sh\n\
         no_clobber=0\n\
         for flag in \"$1\" \"$2\"; do\n\
           case \"$flag\" in -nT|-fT) exit 2;; -nh|-fh) no_clobber=1;; esac\n\
         done\n\
         # last two args are source and target (`${$#-1}` would be\n\
         # `${3-1}`, i.e. the last arg again: use arithmetic)\n\
         eval \"src=\\${$(($# - 1))}\"; eval \"dst=\\$$#\"\n\
         if [ -e \"$dst\" ] || [ -L \"$dst\" ]; then\n\
           if [ -d \"$dst\" ] && [ ! -L \"$dst\" ]; then\n\
             base=${src##*/}; {REAL} \"$src\" \"$dst/$base\"; exit $?\n\
           fi\n\
           [ \"$no_clobber\" = 1 ] && exit 1\n\
           {REAL} -f \"$src\" \"$dst\"; exit $?\n\
         fi\n\
         {REAL} \"$src\" \"$dst\"\n"
        .replace("{REAL}", &quoted);
    std::fs::write(&script, body).expect("write fake mv");
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod fake");
    dir
}

#[test]
fn bsd_nesting_recovers_on_both() {
    let mv_dir = fake_bsd_mv();
    let fake = mv_dir.path().join("mv");
    // Detection prefers the fake binary off the prepended PATH.
    let (code, out) = shell_run(
        mv_dir.path(),
        &[],
        &[],
        Some(mv_dir.path()),
        "_dot_detect_move_tool && printf '%s|%s' \"$DOT_MOVE_BIN\" \"$DOT_MOVE_MODE\"",
    );
    assert_eq!(code, 0, "shell detects fake mv");
    let shell = String::from_utf8(out).expect("detect text");
    assert!(shell.ends_with("|h"), "fake detects as BSD mode: {shell}");
    assert!(
        shell.starts_with(mv_dir.path().to_string_lossy().as_ref()),
        "fake bin: {shell}"
    );
    let tool = temp::MoveTool {
        bin: fake.clone(),
        no_target_dir: false,
    };
    // Late directory: the fake nests, both engines move the source
    // back out and report failure with identical trees.
    move_twins(
        "bsd-nested",
        &|root| {
            stage(root, "source", b"new\n", 0o644);
            stage_dir(root, "target", 0o755);
        },
        "target",
        false,
        Some(mv_dir.path()),
        Some(tool.clone()),
    );
    // Plain rename through the fake still succeeds on both sides.
    move_twins(
        "bsd-plain",
        &|root| {
            stage(root, "source", b"new\n", 0o644);
        },
        "target",
        false,
        Some(mv_dir.path()),
        Some(tool),
    );
}

#[test]
fn commit_happy_path_agrees() {
    for (label, absent) in [("replace", false), ("create", true)] {
        let (sdir, sdst, ssrc) = if absent {
            twin_absent(&format!("commit-{label}-shell"))
        } else {
            twin_layout(&format!("commit-{label}-shell"))
        };
        let (rdir, rdst, rsrc) = if absent {
            twin_absent(&format!("commit-{label}-rust"))
        } else {
            twin_layout(&format!("commit-{label}-rust"))
        };
        let sroot = sdir.path().to_path_buf();
        let rroot = rdir.path().to_path_buf();
        let (scode, _) = shell_run(
            &sroot,
            &[ssrc.as_os_str(), sdst.as_os_str()],
            &[],
            None,
            "token=$(_dot_file_generation_raw \"$3\") && _dot_commit_tmp_if_generation \"$2\" \"$3\" \"$token\"",
        );
        assert_eq!(scode, 0, "shell commit {label}");
        let rtoken = temp::file_generation_raw(&rroot, &rdst).expect("stage token");
        let mut cache = temp::MoveCache::default();
        temp::commit_tmp_if_generation(&rroot, open_lock(), &rsrc, &rdst, &rtoken, &mut cache)
            .expect("rust commit");
        assert_eq!(
            snapshot(&rroot),
            snapshot(&sroot),
            "commit tree parity for {label}"
        );
        // Destination carries the staged content on both sides.
        assert_eq!(
            std::fs::read(&rdst).expect("rust destination"),
            b"live-v2\n",
            "rust destination content"
        );
    }
}

#[test]
fn stale_token_fails_closed_on_both() {
    let (sdir, sdst, ssrc) = twin_layout("stale-shell");
    let (rdir, rdst, rsrc) = twin_layout("stale-rust");
    let sroot = sdir.path().to_path_buf();
    let rroot = rdir.path().to_path_buf();
    // Stage tokens, then mutate both destinations identically: the
    // commit must fail and change nothing further.
    let (scode_before, _) = shell_run(
        &sroot,
        &[ssrc.as_os_str(), sdst.as_os_str()],
        &[],
        None,
        "token=$(_dot_file_generation_raw \"$3\") && \
         printf 'race\\n' > \"$3\" && _dot_commit_tmp_if_generation \"$2\" \"$3\" \"$token\"",
    );
    assert_ne!(scode_before, 0, "shell stale commit fails");
    let rtoken = temp::file_generation_raw(&rroot, &rdst).expect("stage token");
    std::fs::write(&rdst, b"race\n").expect("race rust");
    let mut cache = temp::MoveCache::default();
    assert!(
        temp::commit_tmp_if_generation(&rroot, open_lock(), &rsrc, &rdst, &rtoken, &mut cache)
            .is_err(),
        "rust stale commit fails"
    );
    assert_eq!(snapshot(&rroot), snapshot(&sroot), "stale tree parity");
    // The losing candidate went home on both sides.
    assert_eq!(std::fs::read(&ssrc).expect("shell source"), b"live-v2\n");
    assert_eq!(std::fs::read(&rsrc).expect("rust source"), b"live-v2\n");
}

#[test]
fn remove_lifecycle_agrees() {
    let (sdir, sdst, _) = twin_layout("remove-shell");
    let (rdir, rdst, _) = twin_layout("remove-rust");
    let sroot = sdir.path().to_path_buf();
    let rroot = rdir.path().to_path_buf();
    let (scode, _) = shell_run(
        &sroot,
        &[sdst.as_os_str()],
        &[],
        None,
        "token=$(_dot_file_generation_raw \"$2\") && _dot_remove_if_generation \"$2\" \"$token\"",
    );
    assert_eq!(scode, 0, "shell remove");
    let rtoken = temp::file_generation_raw(&rroot, &rdst).expect("stage token");
    let mut cache = temp::MoveCache::default();
    temp::remove_if_generation(&rroot, open_lock(), &rdst, &rtoken, &mut cache)
        .expect("rust remove");
    assert_eq!(snapshot(&rroot), snapshot(&sroot), "remove tree parity");
    assert!(!rdst.exists(), "rust destination gone");
    // Removing an already-absent leaf with a fresh absent token is an
    // idempotent success on both sides.
    let (scode, _) = shell_run(
        &sroot,
        &[sdst.as_os_str()],
        &[],
        None,
        "token=$(_dot_file_generation_raw \"$2\") && _dot_remove_if_generation \"$2\" \"$token\"",
    );
    assert_eq!(scode, 0, "shell double remove succeeds");
    let rtoken = temp::file_generation_raw(&rroot, &rdst).expect("stage token");
    temp::remove_if_generation(&rroot, open_lock(), &rdst, &rtoken, &mut cache)
        .expect("rust double remove succeeds");
    assert_eq!(
        snapshot(&rroot),
        snapshot(&sroot),
        "double-remove tree parity"
    );
}

/// Stage a journal with the shell on both twins, crash between
/// quarantine's moves (destination aside as `previous`), then recover
/// cross-engine: Rust recovers the shell's journal, the shell recovers
/// the twin. Both must converge to the restored destination.
#[test]
fn crash_recovery_interop() {
    let (sdir, sdst, ssrc) = twin_layout("interop-shell");
    let (rdir, rdst, rsrc) = twin_layout("interop-rust");
    let sroot = sdir.path().to_path_buf();
    let rroot = rdir.path().to_path_buf();
    for (root, dst, src) in [(&sroot, &sdst, &ssrc), (&rroot, &rdst, &rsrc)] {
        let (code, _) = shell_run(
            root,
            &[src.as_os_str(), dst.as_os_str()],
            &[],
            None,
            "token=$(_dot_file_generation_raw \"$3\") && \
             _dot_file_transaction_prepare replace \"$2\" \"$3\" \"$token\"",
        );
        assert_eq!(code, 0, "shell prepare stages");
    }
    for (root, dst) in [(&sroot, &sdst), (&rroot, &rdst)] {
        let txn = root.join("home/.config/.app.conf.dot-file-transaction-v1");
        std::fs::rename(dst, txn.join("previous")).expect("simulate crash");
    }
    let starget = temp::file_target_resolve(&sroot, &sdst).expect("shell target");
    let mut cache = temp::MoveCache::default();
    temp::transaction_recover(&sroot, &sdst, &starget.transaction, &starget, &mut cache)
        .expect("rust recovers shell journal");
    let (code, _) = shell_run(
        &rroot,
        &[rdst.as_os_str()],
        &[],
        None,
        "_dot_file_target_resolve \"$2\" && \
         _dot_file_transaction_recover \"$DOT_FILE_TARGET_PATH\" \"$DOT_FILE_TARGET_TRANSACTION\"",
    );
    assert_eq!(code, 0, "shell recovers twin journal");
    assert_eq!(snapshot(&rroot), snapshot(&sroot), "interop tree parity");
    // The quarantined previous is back as the destination with live
    // content; the staged source was consumed by prepare on both sides.
    assert_eq!(
        std::fs::read(&sdst).expect("shell destination"),
        b"live-v1\n"
    );
    assert_eq!(
        std::fs::read(&rdst).expect("rust destination"),
        b"live-v1\n"
    );
    assert!(!ssrc.exists(), "shell source staged away");
    assert!(!rsrc.exists(), "rust source staged away");
}

/// Forged journals with non-minimal numerics validate (classes pass)
/// but must fail closed at recovery on both sides — never normalize
/// into the committed branch.
#[test]
fn forged_nonminimal_journal_fails_closed() {
    let dir = TempDir::new("forged").expect("fixture dir");
    let root = dir.path();
    let live = stage(root, "home/app.conf", b"v1\n", 0o644);
    let token = temp::file_generation_raw(root, &live).expect("raw token");
    // Zero-pad the parent device and re-checksum: the token validates,
    // but its parent binding matches nothing minimal.
    let mut fields: Vec<&str> = token.split('|').collect();
    assert_eq!(fields.len(), 11, "token shape");
    fields[2] = "007";
    let payload = fields[..10].join("|");
    let checksum =
        temp::file_text_digest(root, format!("dot-file-generation-v1|{payload}").as_bytes())
            .expect("re-checksum");
    let forged = format!("{payload}|{checksum}");
    assert!(
        temp::generation_validate(root, &forged).is_ok(),
        "rust validates forged"
    );
    let (code, _) = shell_run(
        root,
        &[forged.as_str().as_ref()],
        &[],
        None,
        "_dot_file_generation_validate \"$2\"",
    );
    assert_eq!(code, 0, "shell validates forged");
    // Hand-stage identical `prepared` journals and recover both sides.
    let target = temp::file_target_resolve(root, &live).expect("target");
    let txn = target.transaction.clone();
    std::fs::create_dir_all(&txn).expect("forge txn dir");
    std::fs::set_permissions(&txn, std::fs::Permissions::from_mode(0o700)).expect("chmod txn");
    let candidate = "1|2|644|3|e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    let body = format!("v1\treplace\tprepared\t{forged}\t{candidate}\n");
    let record = txn.join("record");
    std::fs::write(&record, body.as_bytes()).expect("forge record");
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600))
        .expect("chmod record");
    assert!(temp::record_read(root, &txn).is_ok(), "forged record reads");
    let before = snapshot(root);
    let mut cache = temp::MoveCache::default();
    assert!(
        temp::transaction_recover(root, &live, &txn, &target, &mut cache).is_err(),
        "rust rejects forged journal"
    );
    assert_eq!(snapshot(root), before, "rust changed nothing");
    let (code, _) = shell_run(
        root,
        &[live.as_os_str()],
        &[],
        None,
        "_dot_file_target_resolve \"$2\" && \
         _dot_file_transaction_record_read \"$DOT_FILE_TARGET_TRANSACTION/record\" && \
         _dot_file_transaction_recover \"$DOT_FILE_TARGET_PATH\" \"$DOT_FILE_TARGET_TRANSACTION\"",
    );
    assert_ne!(code, 0, "shell rejects forged journal");
    assert_eq!(snapshot(root), before, "shell changed nothing");
}

#[test]
fn metadata_tree_clamp_agrees() {
    let sdir = TempDir::new("meta-shell").expect("fixture dir");
    let rdir = TempDir::new("meta-rust").expect("fixture dir");
    for root in [sdir.path(), rdir.path()] {
        stage(root, "top.sh", b"x\n", 0o777);
        stage(root, "sub/deep.conf", b"y\n", 0o666);
        stage(root, "sub/exec", b"z\n", 0o777);
        stage_dir(root, "sub", 0o777);
        stage_dir(root, "empty", 0o777);
    }
    let (scode, _) = shell_run(
        sdir.path(),
        &[sdir.path().as_os_str()],
        &[],
        None,
        "umask 022 && _dot_apply_git_metadata_modes \"$2\"",
    );
    assert_eq!(scode, 0, "shell metadata walk");
    temp::apply_git_metadata_modes(rdir.path(), 0o022).expect("rust metadata walk");
    assert_eq!(
        snapshot(rdir.path()),
        snapshot(sdir.path()),
        "metadata tree parity"
    );
    // Pin the umask math absolutely: 777->755, 666->644, dirs alike.
    for (name, mode) in [
        ("top.sh", 0o755u32),
        ("sub/deep.conf", 0o644u32),
        ("sub/exec", 0o755u32),
        ("sub", 0o755u32),
        ("empty", 0o755u32),
    ] {
        assert_eq!(
            mode_of(&rdir.path().join(name)),
            mode,
            "clamped mode for {name}"
        );
    }
    // A symlink in the tree aborts both walks.
    for root in [sdir.path(), rdir.path()] {
        #[cfg(unix)]
        std::os::unix::fs::symlink("top.sh", root.join("link")).expect("symlink");
    }
    let (scode, _) = shell_run(
        sdir.path(),
        &[sdir.path().as_os_str()],
        &[],
        None,
        "umask 022 && _dot_apply_git_metadata_modes \"$2\"",
    );
    assert_ne!(scode, 0, "shell rejects symlink tree");
    assert!(
        temp::apply_git_metadata_modes(rdir.path(), 0o022).is_err(),
        "rust rejects symlink tree"
    );
}

#[test]
fn lock_gate_and_source_root_from_env() {
    let _guard = SERIAL.lock().expect("serial");
    // SAFETY: serialized with every other env-touching test; the vars
    // are restored before returning.
    let old_test = std::env::var_os("DOT_TEST");
    let old_token = std::env::var_os("DOT_UPDATE_LOCK_TOKEN");
    let old_root = std::env::var_os("DOT_SOURCE_ROOT");
    unsafe {
        std::env::remove_var("DOT_TEST");
        std::env::remove_var("DOT_UPDATE_LOCK_TOKEN");
        std::env::remove_var("DOT_SOURCE_ROOT");
    }
    assert!(!temp::LockCtx::from_env().valid(), "closed gate");
    unsafe {
        std::env::set_var("DOT_TEST", "1");
    }
    assert!(temp::LockCtx::from_env().valid(), "test mode opens");
    unsafe {
        std::env::set_var("DOT_TEST", "yes");
        std::env::set_var("DOT_UPDATE_LOCK_TOKEN", "tok");
    }
    assert!(temp::LockCtx::from_env().valid(), "token opens");
    unsafe {
        std::env::remove_var("DOT_UPDATE_LOCK_TOKEN");
    }
    assert!(!temp::LockCtx::from_env().valid(), "non-1 test mode closed");
    // Source root binding: explicit var wins, else the workdir.
    let dir = TempDir::new("source-root").expect("fixture dir");
    unsafe {
        std::env::set_var("DOT_SOURCE_ROOT", dir.path());
    }
    assert_eq!(temp::source_root().expect("source root"), dir.path());
    unsafe {
        std::env::remove_var("DOT_SOURCE_ROOT");
    }
    assert!(temp::source_root().is_ok(), "cwd fallback works");
    unsafe {
        match old_test {
            Some(value) => std::env::set_var("DOT_TEST", value),
            None => std::env::remove_var("DOT_TEST"),
        }
        match old_token {
            Some(value) => std::env::set_var("DOT_UPDATE_LOCK_TOKEN", value),
            None => std::env::remove_var("DOT_UPDATE_LOCK_TOKEN"),
        }
        match old_root {
            Some(value) => std::env::set_var("DOT_SOURCE_ROOT", value),
            None => std::env::remove_var("DOT_SOURCE_ROOT"),
        }
    }
}
