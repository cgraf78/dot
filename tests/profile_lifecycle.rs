//! Differential parity tests for the profile lifecycle ledger
//! (`lib/dot/profile-lifecycle.sh`) against the live shell: ledger
//! gating, load diagnostics, atomic writes, entry-point
//! resolution, prepare/commit orchestration, and worker-backed
//! retire/run-one execution — including every warning line.
//!
//! Separate binary because the execution rows drive real worker
//! spawns: each side builds twin fixture homes (absolute checkout
//! paths embed the home, so homes normalize before comparing), and
//! the Rust [`profile_lifecycle::WorkerRun`] seam shells out to the
//! live `_dot_extension_worker_run`, so the comparison covers the
//! ported plumbing while the leaf worker stays identical.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::profile_lifecycle::{self, WorkerOutcome, WorkerRun};
use dot::test_support::TempDir;

/// Sources for the lifecycle chapter: the trust/launch stack the
/// entry points validate and execute through.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/platform.sh\"\n",
    ". \"$1/lib/dot/config.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/overlay-context.sh\"\n",
    ". \"$1/lib/dot/overlays.sh\"\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/extension-trust.sh\"\n",
    ". \"$1/lib/dot/extension-worker-launch.sh\"\n",
    ". \"$1/lib/dot/profile-lifecycle.sh\"\n",
);

/// Run one shell snippet with the lifecycle libraries sourced. The
/// locale stays pinned like the other harnesses, and
/// `DOT_SOURCE_ROOT` points at the tree so the worker can find
/// `extension-worker.sh`.
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
        .arg(format!("{SOURCES}{snippet}"));
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
        .env("DOT_SOURCE_ROOT", repo)
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    stage_mode(dir, name, bytes, 0o644)
}

/// Write `bytes` to `dir/name` with `mode`, creating parents.
/// Directories along the way arrive with `0o755` (no group/other
/// write bit, so the extension stat gates hold regardless of the
/// ambient umask).
fn stage_mode(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        // Pin `0o755` on the levels this call creates (the ambient
        // umask may be wider than the stat gates allow); levels
        // that already exist keep the mode the test staged, and
        // nothing above the fixture root is ever touched.
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut current = parent;
        loop {
            if std::fs::symlink_metadata(current).is_ok() {
                break;
            }
            missing.push(current.to_path_buf());
            match current.parent() {
                Some(next) if next.starts_with(dir) => current = next,
                _ => break,
            }
        }
        std::fs::create_dir_all(parent).expect("fixture parents");
        for fresh in &missing {
            let _ = std::fs::set_permissions(fresh, std::fs::Permissions::from_mode(0o755));
        }
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

/// Current epoch seconds for context freshness.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

/// Parent TMPDIR the harnesses run under.
fn parent_tmpdir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Replace both twin homes so dumps compare.
fn normalize(text: &str, shell_home: &str, rust_home: &str) -> String {
    text.replace(shell_home, "@HOME@")
        .replace(rust_home, "@HOME@")
}

/// Uncolored logger matching piped shell stderr/stdout.
fn logger() -> Log {
    Log::new(false, false)
}

/// `git init` plus one origin (output silenced). The root lands
/// `0o755` regardless of umask so the directory stat gates hold.
fn git_repo(path: &Path, origin: &str) {
    std::fs::create_dir_all(path).expect("repo dir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git init");
    assert!(status.success(), "git init {}", path.display());
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(origin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git remote add");
    assert!(status.success(), "git remote add {}", path.display());
}

/// Canonical valid record for `name` in `home`.
fn record(home: &str, name: &str, origin: &str) -> String {
    record_descriptor(home, name, origin, &format!("{home}/conf/10-{name}.conf"))
}

/// Valid record with an explicit descriptor path (for the refresh
/// rows where the retained and current descriptors differ).
fn record_descriptor(home: &str, name: &str, origin: &str, descriptor: &str) -> String {
    format!("{name}|{home}/.dotfiles-{name}|{origin}|{descriptor}|false|git")
}

/// Stage `$HOME/.dotfiles-<name>` (git repo with `origin`) plus an
/// optional `dot/profile-deactivate` script body; returns the valid
/// record for the checkout.
fn checkout(home: &Path, name: &str, origin: &str, script: Option<(&[u8], u32)>) -> String {
    let home_text = home.to_string_lossy().into_owned();
    let repo = home.join(format!(".dotfiles-{name}"));
    git_repo(&repo, origin);
    mkdir_mode(&repo, "dot", 0o755);
    if let Some((body, mode)) = script {
        stage_mode(&repo, "dot/profile-deactivate", body, mode);
    }
    record(&home_text, name, origin)
}

#[test]
fn file_safe_cases_agree() {
    let dir = TempDir::new("lc-file").expect("fixture dir");
    let home = dir.path();
    let uid = euid();
    let big = vec![b'x'; 1048577];
    let exact = vec![b'y'; 1048576];
    // (label, body, mode, want): `None` is a missing path.
    let cases: &[(&str, Option<&[u8]>, u32, bool)] = &[
        ("good", Some(b"version=1\n"), 0o600, true),
        ("empty", Some(b""), 0o600, true),
        ("exact", Some(&exact), 0o600, true),
        ("big", Some(&big), 0o600, false),
        ("open", Some(b"version=1\n"), 0o644, false),
        ("group-write", Some(b"version=1\n"), 0o620, false),
        ("missing", None, 0o600, false),
    ];
    for (label, body, mode, want) in cases {
        let path = match body {
            Some(body) => stage_mode(home, label, body, *mode),
            None => home.join(label),
        };
        // Both sides are silent, so the harness stdout carries
        // the shell verdict next to the Rust gate.
        let (code, sout, serr) = shell_run(
            home,
            &[path.as_os_str()],
            &[],
            "_dot_profile_lifecycle_file_safe \"$2\"; printf 'rc=%s\\n' \"$?\"",
        );
        assert_eq!(code, 0, "shell harness file {label}");
        assert_eq!(serr, b"", "shell silent for {label}");
        assert_eq!(
            String::from_utf8(sout).expect("dump"),
            format!("rc={}\n", i32::from(!want)),
            "shell code for {label}"
        );
        assert_eq!(
            profile_lifecycle::file_safe(&path, uid),
            *want,
            "rust gate for {label}"
        );
    }
    // Directories, symlinks, and multi-link files all refuse.
    mkdir_mode(home, "adir", 0o700);
    std::os::unix::fs::symlink("good", home.join("link")).expect("symlink");
    let hard_src = stage_mode(home, "hard-src", b"version=1\n", 0o600);
    std::fs::hard_link(&hard_src, home.join("hard")).expect("hard link");
    for (label, name) in [("dir", "adir"), ("symlink", "link"), ("hardlink", "hard")] {
        let path = home.join(name);
        let (_, sout, serr) = shell_run(
            home,
            &[path.as_os_str()],
            &[],
            "_dot_profile_lifecycle_file_safe \"$2\"; printf 'rc=%s\\n' \"$?\"",
        );
        assert_eq!(serr, b"", "shell silent for {label}");
        assert_eq!(
            String::from_utf8(sout).expect("dump"),
            "rc=1\n",
            "shell code for {label}"
        );
        assert!(
            !profile_lifecycle::file_safe(&path, uid),
            "rust refuses {label}"
        );
    }
}

/// Dump a load/prepare outcome the way the shell snippet does:
/// rc, record count, then one `rec=` line each. Failures still
/// carry the records the shell global holds (cleared, then
/// whatever validated before the fault).
fn state_dump(ok: bool, records: &[String]) -> String {
    let mut dump = format!("rc={}\nn={}\n", i32::from(!ok), records.len());
    for record in records {
        dump.push_str(&format!("rec={record}\n"));
    }
    dump
}

#[test]
fn load_rows_agree() {
    // (label, body builder, mode, ledger setup): bodies take the
    // side home so records validate on both sides.
    type Body = fn(&str) -> Vec<u8>;
    fn good(home: &str) -> Vec<u8> {
        format!(
            "version=1\n{}\n{}\n",
            record(home, "web", "file:///repo/web.git"),
            record(home, "api", "file:///repo/api.git"),
        )
        .into_bytes()
    }
    fn bad_version(_home: &str) -> Vec<u8> {
        b"version=2\n".to_vec()
    }
    fn record_first(home: &str) -> Vec<u8> {
        format!("{}\n", record(home, "web", "file:///repo/web.git")).into_bytes()
    }
    fn blank_line(home: &str) -> Vec<u8> {
        format!(
            "version=1\n\n{}\n",
            record(home, "web", "file:///repo/web.git")
        )
        .into_bytes()
    }
    fn bad_record(_home: &str) -> Vec<u8> {
        b"version=1\nGARBAGE\n".to_vec()
    }
    fn duplicate(home: &str) -> Vec<u8> {
        let web = record(home, "web", "file:///repo/web.git");
        format!("version=1\n{web}\n{web}\n").into_bytes()
    }
    fn only_newline(_home: &str) -> Vec<u8> {
        b"\n".to_vec()
    }
    fn no_trailing(_home: &str) -> Vec<u8> {
        b"version=1".to_vec()
    }
    fn cr_version(home: &str) -> Vec<u8> {
        let mut body = b"version=1\r\n".to_vec();
        body.extend_from_slice(record(home, "web", "file:///repo/web.git").as_bytes());
        body.push(b'\n');
        body
    }
    let rows: &[(&str, Option<Body>, u32, bool)] = &[
        ("good", Some(good), 0o600, true),
        ("unsafe-mode", Some(good), 0o644, true),
        ("bad-version", Some(bad_version), 0o600, true),
        ("record-first", Some(record_first), 0o600, true),
        ("blank-line", Some(blank_line), 0o600, true),
        ("bad-record", Some(bad_record), 0o600, true),
        ("duplicate", Some(duplicate), 0o600, true),
        ("empty-file", Some(|_: &str| Vec::new()), 0o600, true),
        ("only-newline", Some(only_newline), 0o600, true),
        ("no-trailing", Some(no_trailing), 0o600, true),
        ("cr-version", Some(cr_version), 0o600, true),
        ("missing", None, 0o600, true),
        ("unset", Some(good), 0o600, false),
        ("dir", None, 0o700, true),
        ("link", None, 0o600, true),
    ];
    for (label, body, mode, with_var) in rows {
        let sdir = TempDir::new(&format!("lc-load-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("lc-load-{label}-rust")).expect("rust dir");
        let shell_home = sdir.path().to_string_lossy().into_owned();
        let rust_home = rdir.path().to_string_lossy().into_owned();
        let sled = sdir.path().join("ledger");
        let rled = rdir.path().join("ledger");
        if *label == "dir" {
            mkdir_mode(sdir.path(), "ledger", 0o700);
            mkdir_mode(rdir.path(), "ledger", 0o700);
        } else if *label == "link" {
            let target_s = stage_mode(sdir.path(), "real", &good(&shell_home), 0o600);
            let target_r = stage_mode(rdir.path(), "real", &good(&rust_home), 0o600);
            std::os::unix::fs::symlink(&target_s, &sled).expect("symlink");
            std::os::unix::fs::symlink(&target_r, &rled).expect("symlink");
        } else if let Some(build) = body {
            stage_mode(sdir.path(), "ledger", &build(&shell_home), *mode);
            stage_mode(rdir.path(), "ledger", &build(&rust_home), *mode);
        }
        let setup = if *with_var {
            "DOT_PROFILE_LIFECYCLE_LEDGER=\"$2\"; "
        } else {
            "unset DOT_PROFILE_LIFECYCLE_LEDGER; "
        };
        let (scode, sout, serr) = shell_run(
            sdir.path(),
            &[sled.as_os_str()],
            &[],
            &format!(
                "{setup}_dot_profile_lifecycle_load; printf 'rc=%s\\n' \"$?\"; \
                 printf 'n=%s\\n' \"${{#DOT_PROFILE_LIFECYCLE_RECORDS[@]}}\"; \
                 for r in \"${{DOT_PROFILE_LIFECYCLE_RECORDS[@]}}\"; do printf 'rec=%s\\n' \"$r\"; done"
            ),
        );
        assert_eq!(scode, 0, "shell harness load {label}");
        let uid = euid();
        let log = logger();
        let mut warnings = Vec::new();
        let ledger_opt = with_var.then_some(rled.as_path());
        let mut records = Vec::new();
        let ok = profile_lifecycle::load(
            ledger_opt,
            &rust_home,
            uid,
            &log,
            &mut warnings,
            &mut records,
        );
        let rust = state_dump(ok, &records);
        let shell = String::from_utf8(sout).expect("dump text");
        assert_eq!(
            normalize(&rust, &shell_home, &rust_home),
            normalize(&shell, &shell_home, &rust_home),
            "load dump for {label}"
        );
        assert_eq!(
            normalize(
                &String::from_utf8(warnings).expect("warnings text"),
                &shell_home,
                &rust_home
            ),
            normalize(
                &String::from_utf8(serr).expect("stderr text"),
                &shell_home,
                &rust_home
            ),
            "load warnings for {label}"
        );
    }
}

/// Read a ledger file the way the dumps do: `ABSENT` when no
/// regular file is there, else the raw bytes.
fn ledger_aftermath(path: &Path) -> Vec<u8> {
    match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => b"ABSENT\n".to_vec(),
    }
}

/// Permission bits (`0o777`) of `path`, or `0` when stat fails.
fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0)
}

#[test]
fn write_rows_agree() {
    // (label, parent setup, with records, with ledger variable).
    #[derive(Clone, Copy)]
    enum Parent {
        Missing,
        Nested,
        Ready,
        Open,
        File,
    }
    let rows: &[(&str, Parent, bool, bool)] = &[
        ("fresh", Parent::Missing, true, true),
        ("nested", Parent::Nested, true, true),
        ("ready", Parent::Ready, true, true),
        ("empty-records", Parent::Ready, false, true),
        ("overwrite", Parent::Ready, true, true),
        ("unset", Parent::Ready, true, false),
        ("parent-file", Parent::File, true, true),
        ("parent-open", Parent::Open, true, true),
    ];
    for (label, parent, with_records, with_var) in rows {
        let sdir = TempDir::new(&format!("lc-write-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("lc-write-{label}-rust")).expect("rust dir");
        let shell_home = sdir.path().to_string_lossy().into_owned();
        let rust_home = rdir.path().to_string_lossy().into_owned();
        for base in [sdir.path(), rdir.path()] {
            match parent {
                Parent::Missing | Parent::File => (),
                Parent::Nested => {
                    std::fs::create_dir_all(base.join("a")).expect("nested grandparent");
                }
                Parent::Ready => {
                    mkdir_mode(base, "led", 0o700);
                }
                Parent::Open => {
                    mkdir_mode(base, "led", 0o755);
                }
            }
        }
        // A file where the parent directory should be.
        if matches!(parent, Parent::File) {
            stage(sdir.path(), "led", b"in the way\n");
            stage(rdir.path(), "led", b"in the way\n");
        }
        let sled = match parent {
            Parent::Nested => sdir.path().join("a/b/ledger"),
            _ => sdir.path().join("led/ledger"),
        };
        let rled = match parent {
            Parent::Nested => rdir.path().join("a/b/ledger"),
            _ => rdir.path().join("led/ledger"),
        };
        if *label == "overwrite" {
            stage_mode(sdir.path(), "led/ledger", b"version=1\nSTALE\n", 0o600);
            stage_mode(rdir.path(), "led/ledger", b"version=1\nSTALE\n", 0o600);
        }
        let shell_recs: Vec<String> = if *with_records {
            vec![
                record(&shell_home, "web", "file:///repo/web.git"),
                record(&shell_home, "api", "file:///repo/api.git"),
            ]
        } else {
            Vec::new()
        };
        let rust_recs: Vec<String> = if *with_records {
            vec![
                record(&rust_home, "web", "file:///repo/web.git"),
                record(&rust_home, "api", "file:///repo/api.git"),
            ]
        } else {
            Vec::new()
        };
        let setup = if *with_var {
            "DOT_PROFILE_LIFECYCLE_LEDGER=\"$2\"; "
        } else {
            "unset DOT_PROFILE_LIFECYCLE_LEDGER; "
        };
        let mut argv: Vec<&std::ffi::OsStr> = vec![sled.as_os_str()];
        for rec in &shell_recs {
            argv.push(rec.as_ref());
        }
        let (scode, sout, serr) = shell_run(
            sdir.path(),
            &argv,
            &[],
            &format!(
                "{setup}_dot_profile_lifecycle_write \"${{@:3}}\"; printf 'rc=%s\\n' \"$?\"; \
                 if [[ -f \"$2\" ]]; then cat \"$2\"; else printf 'ABSENT\\n'; fi"
            ),
        );
        assert_eq!(scode, 0, "shell harness write {label}");
        if *label == "parent-file" {
            // The shell surfaces the `mkdir -p` tool error for a
            // file in the directory slot; the port stays silent
            // (rc plus aftermath carry the verdict on both sides).
            let noise = String::from_utf8(serr).expect("stderr text");
            // Only the `strerror` tail is asserted: GNU and BSD
            // `mkdir` wordings differ around it.
            assert!(noise.contains("File exists"), "mkdir cause for {label}");
        } else {
            assert_eq!(serr, b"", "shell silent for {label}");
        }
        let uid = euid();
        let ok = match with_var.then_some(rled.as_path()) {
            // The shell write reads the variable, so `None`
            // models the unset row.
            Some(path) => profile_lifecycle::write(path, &rust_recs, uid),
            None => false,
        };
        let shell = String::from_utf8(sout).expect("dump text");
        let mut expected = format!("rc={}\n", i32::from(!ok));
        expected.push_str(&String::from_utf8_lossy(&ledger_aftermath(&rled)));
        assert_eq!(
            normalize(&shell, &shell_home, &rust_home),
            normalize(&expected, &shell_home, &rust_home),
            "write dump for {label}"
        );
        // Modes agree too: ledgers land `0o600`, created parents
        // `0o700`, on both sides.
        assert_eq!(mode_of(&sled), mode_of(&rled), "ledger mode for {label}");
        if matches!(parent, Parent::Missing | Parent::Nested) && ok {
            assert_eq!(
                mode_of(sled.parent().expect("parent")),
                0o700,
                "shell parent {label}"
            );
            assert_eq!(
                mode_of(rled.parent().expect("parent")),
                0o700,
                "rust parent {label}"
            );
        }
    }
}

#[test]
fn deactivation_script_rows_agree() {
    let dir = TempDir::new("lc-script").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let uid = euid();
    let origin = "file:///repo/web.git";
    // The shared good checkout; destructive rows run last.
    let web = checkout(home, "web", origin, Some((b"#!/bin/sh\n", 0o600)));
    let script = format!("{home_text}/.dotfiles-web/dot/profile-deactivate");
    let ghost = record(&home_text, "ghost", "file:///repo/ghost.git");
    let bad_record = "G|/x|u|/d/10-g.conf|false|git".to_string();
    let sync_none = web.replace("|false|git", "|false|none");
    let origin_miss = web.replace("file:///repo/web.git", "file:///repo/other.git");
    // (label, record, script path): the script mutations apply to
    // every later row, so breaking rows come last.
    let rows: &[(&str, String, String)] = &[
        ("good", web.clone(), script.clone()),
        ("bad-record", bad_record, script.clone()),
        ("sync-none", sync_none, script.clone()),
        (
            "path-miss",
            web.replace(".dotfiles-web|", ".dotfiles-ghost|"),
            script.clone(),
        ),
        ("origin-miss", origin_miss, script.clone()),
        ("missing-script", ghost, format!("{home_text}/elsewhere")),
        ("open-script", web.clone(), script.clone()),
        ("symlink-script", web.clone(), script.clone()),
        ("dir-script", web.clone(), script.clone()),
        ("dangling-script", web.clone(), script.clone()),
    ];
    for (label, rec, script_path) in rows {
        // Per-row script state: the open mode only for its row,
        // then the spelling mutations in order.
        if *label == "open-script" {
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o664))
                .expect("chmod");
        }
        if *label == "symlink-script" {
            std::fs::remove_file(&script).expect("remove script");
            std::os::unix::fs::symlink("/dev/null", &script).expect("symlink");
        }
        if *label == "dir-script" {
            std::fs::remove_file(&script).expect("remove link");
            std::fs::create_dir(&script).expect("script dir");
        }
        if *label == "dangling-script" {
            std::fs::remove_dir(&script).expect("remove script dir");
            std::os::unix::fs::symlink("no-such-target", &script).expect("dangling");
        }
        let (code, out, serr) = shell_run(
            home,
            &[rec.as_ref(), script_path.as_ref()],
            &[],
            "REPLY=; _dot_profile_deactivation_script \"$2\"; rc=$?; printf 'rc=%s\\n' \"$rc\"; if [[ $rc -eq 0 ]]; then printf 'reply=%s\\n' \"$REPLY\"; fi",
        );
        assert_eq!(code, 0, "shell harness script {label}");
        assert_eq!(serr, b"", "shell silent for {label}");
        let shell = String::from_utf8(out).expect("dump");
        let rust = match profile_lifecycle::deactivation_script(rec, &home_text, uid) {
            Ok(reply) => format!("rc=0\nreply={reply}\n"),
            Err(error) => format!("rc={}\n", error.code()),
        };
        assert_eq!(
            normalize(&rust, &home_text, &home_text),
            normalize(&shell, &home_text, &home_text),
            "script dump for {label}"
        );
    }
}

/// Script-state mutations a row needs beyond the good fixtures.
#[derive(Clone, Copy)]
enum Tweak {
    /// Everything stays valid.
    None,
    /// The `web` entry point disappears (removed-entrypoint rows).
    DeleteWebScript,
    /// The `web` entry point turns group-writable (unsafe rows).
    OpenWebScript,
    /// The `bad` entry point turns group-writable (retiring rows).
    OpenBadScript,
}

/// Apply one [`Tweak`] to a staged home.
fn apply_tweak(home: &Path, tweak: Tweak) {
    let web = home.join(".dotfiles-web/dot/profile-deactivate");
    let bad = home.join(".dotfiles-bad/dot/profile-deactivate");
    match tweak {
        Tweak::None => (),
        Tweak::DeleteWebScript => {
            std::fs::remove_file(&web).expect("remove web script");
        }
        Tweak::OpenWebScript => {
            std::fs::set_permissions(&web, std::fs::Permissions::from_mode(0o664)).expect("chmod");
        }
        Tweak::OpenBadScript => {
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o664)).expect("chmod");
        }
    }
}

/// Stage the shared orchestration fixtures: `web` and `api` with
/// valid entry points, `ghost` without one, and `bad` with one the
/// row may loosen. Returns the per-home records.
fn stage_orchestration(home: &Path) -> (String, String, String, String) {
    let home_text = home.to_string_lossy().into_owned();
    let web = checkout(
        home,
        "web",
        "file:///repo/web.git",
        Some((b"#!/bin/sh\n", 0o600)),
    );
    let api = checkout(
        home,
        "api",
        "file:///repo/api.git",
        Some((b"#!/bin/sh\n", 0o600)),
    );
    checkout(home, "ghost", "file:///repo/ghost.git", None);
    checkout(
        home,
        "bad",
        "file:///repo/bad.git",
        Some((b"#!/bin/sh\n", 0o600)),
    );
    let ghost = record(&home_text, "ghost", "file:///repo/ghost.git");
    let bad = record(&home_text, "bad", "file:///repo/bad.git");
    (web, api, ghost, bad)
}

/// Join array items into a shell array literal.
fn sh_array(items: &[String]) -> String {
    items
        .iter()
        .map(|item| sq(item))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn prepare_rows_agree() {
    // Ledger bodies per row, built from the side home.
    fn body_for(label: &str, home: &str, web: &str, api: &str, bad: &str) -> Option<Vec<u8>> {
        let mut out = b"version=1\n".to_vec();
        match label {
            "absent" => return None,
            "disabled-clean" => out.extend_from_slice(format!("{web}\n").as_bytes()),
            "disabled-pending" => {
                out.extend_from_slice(format!("{web}\n{api}\n").as_bytes());
            }
            "steady" => out.extend_from_slice(format!("{api}\n{web}\n").as_bytes()),
            "refresh" => {
                let web10 = record_descriptor(
                    home,
                    "web",
                    "file:///repo/web.git",
                    &format!("{home}/conf/10-web.conf"),
                );
                out.extend_from_slice(format!("{web10}\n").as_bytes());
            }
            "add" => out.extend_from_slice(format!("{web}\n").as_bytes()),
            "scriptless" => out.extend_from_slice(format!("{web}\n").as_bytes()),
            "removed" => out.extend_from_slice(format!("{web}\n").as_bytes()),
            "unsafe-retiring" => out.extend_from_slice(format!("{bad}\n").as_bytes()),
            "unsafe-current" => (),
            "load-fails" => return Some(b"version=2\n".to_vec()),
            "unset" => out.extend_from_slice(format!("{web}\n").as_bytes()),
            _ => unreachable!("unknown row {label}"),
        }
        Some(out)
    }
    // (label, present, enabled, eligible, phase-one, active,
    // prior, tweak, with ledger variable).
    struct Row {
        label: &'static str,
        present: bool,
        enabled: bool,
        eligible: &'static [&'static str],
        phase_one: &'static [&'static str],
        active: &'static [&'static str],
        prior: bool,
        tweak: Tweak,
        with_var: bool,
    }
    let rows: &[Row] = &[
        Row {
            label: "absent",
            present: false,
            enabled: true,
            eligible: &[],
            phase_one: &[],
            active: &[],
            prior: true,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "disabled-clean",
            present: true,
            enabled: false,
            eligible: &["web"],
            phase_one: &[],
            active: &[],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "disabled-pending",
            present: true,
            enabled: false,
            eligible: &["web"],
            phase_one: &[],
            active: &[],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "steady",
            present: true,
            enabled: true,
            eligible: &["web", "api"],
            phase_one: &[],
            active: &["web", "api"],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "refresh",
            present: true,
            enabled: true,
            eligible: &["web"],
            phase_one: &[],
            active: &["web20"],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "add",
            present: true,
            enabled: true,
            eligible: &["web", "api"],
            phase_one: &[],
            active: &["web", "api"],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "scriptless",
            present: true,
            enabled: true,
            eligible: &["web", "ghost"],
            phase_one: &[],
            active: &["web", "ghost"],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "removed",
            present: true,
            enabled: true,
            eligible: &[],
            phase_one: &[],
            active: &["web"],
            prior: false,
            tweak: Tweak::DeleteWebScript,
            with_var: true,
        },
        Row {
            label: "unsafe-retiring",
            present: true,
            enabled: true,
            eligible: &[],
            phase_one: &[],
            active: &[],
            prior: false,
            tweak: Tweak::OpenBadScript,
            with_var: true,
        },
        Row {
            label: "unsafe-current",
            present: true,
            enabled: true,
            eligible: &["web"],
            phase_one: &[],
            active: &["web"],
            prior: false,
            tweak: Tweak::OpenWebScript,
            with_var: true,
        },
        Row {
            label: "load-fails",
            present: true,
            enabled: true,
            eligible: &["web"],
            phase_one: &[],
            active: &[],
            prior: false,
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "unset",
            present: true,
            enabled: true,
            eligible: &["web"],
            phase_one: &[],
            active: &[],
            prior: false,
            tweak: Tweak::None,
            with_var: false,
        },
    ];
    for row in rows {
        let sdir = TempDir::new(&format!("lc-prep-{}-shell", row.label)).expect("shell dir");
        let rdir = TempDir::new(&format!("lc-prep-{}-rust", row.label)).expect("rust dir");
        let shell_home = sdir.path().to_string_lossy().into_owned();
        let rust_home = rdir.path().to_string_lossy().into_owned();
        let (sweb, sapi, sghost, sbad) = stage_orchestration(sdir.path());
        let (rweb, rapi, rghost, rbad) = stage_orchestration(rdir.path());
        apply_tweak(sdir.path(), row.tweak);
        apply_tweak(rdir.path(), row.tweak);
        // Name-keyed records per side (`web20` is the refreshed
        // descriptor spelling).
        let by_name =
            |home: &str, web: &str, api: &str, ghost: &str, bad: &str, key: &str| -> String {
                match key {
                    "web" => web.to_string(),
                    "api" => api.to_string(),
                    "ghost" => ghost.to_string(),
                    "bad" => bad.to_string(),
                    "web20" => record_descriptor(
                        home,
                        "web",
                        "file:///repo/web.git",
                        &format!("{home}/conf/20-web.conf"),
                    ),
                    _ => unreachable!("unknown overlay {key}"),
                }
            };
        let resolve = |home: &str,
                       web: &str,
                       api: &str,
                       ghost: &str,
                       bad: &str,
                       keys: &[&str]|
         -> Vec<String> {
            keys.iter()
                .map(|key| by_name(home, web, api, ghost, bad, key))
                .collect()
        };
        let sled = sdir.path().join("ledger");
        let rled = rdir.path().join("ledger");
        if let Some(body) = body_for(row.label, &shell_home, &sweb, &sapi, &sbad) {
            stage_mode(sdir.path(), "ledger", &body, 0o600);
        }
        if let Some(body) = body_for(row.label, &rust_home, &rweb, &rapi, &rbad) {
            stage_mode(rdir.path(), "ledger", &body, 0o600);
        }
        // Eligibility arrays hold bare names on both sides;
        // the record sets resolve per side.
        let shell_elig: Vec<String> = row.eligible.iter().map(|key| key.to_string()).collect();
        let shell_po = resolve(&shell_home, &sweb, &sapi, &sghost, &sbad, row.phase_one);
        let shell_act = resolve(&shell_home, &sweb, &sapi, &sghost, &sbad, row.active);
        let rust_elig: Vec<String> = row.eligible.iter().map(|key| key.to_string()).collect();
        let rust_po = resolve(&rust_home, &rweb, &rapi, &rghost, &rbad, row.phase_one);
        let rust_act = resolve(&rust_home, &rweb, &rapi, &rghost, &rbad, row.active);
        let shell_prior = if row.prior {
            vec![sweb.clone()]
        } else {
            Vec::new()
        };
        let rust_prior = if row.prior {
            vec![rweb.clone()]
        } else {
            Vec::new()
        };
        let setup = if row.with_var {
            "DOT_PROFILE_LIFECYCLE_LEDGER=\"$2\"; "
        } else {
            "unset DOT_PROFILE_LIFECYCLE_LEDGER; "
        };
        let api_flag = if row.enabled { "1" } else { "" };
        let (scode, sout, serr) = shell_run(
            sdir.path(),
            &[sled.as_os_str()],
            &[],
            &format!(
                "ELIGIBLE_OVERLAY_NAMES=({elig}); PHASE_ONE_ACTIVE_OVERLAYS=({po}); \
                 ACTIVE_OVERLAYS=({act}); DOT_PROFILE_LIFECYCLE_RECORDS=({prior}); \
                 DOT_PROFILES_PRESENT={present}; DOT_EXTENSION_API={api}; DOT_EXTENSIONS_DIR=\"$HOME/ext\"; \
                 {setup}_dot_profile_lifecycle_prepare; printf 'rc=%s\\n' \"$?\"; \
                 printf 'n=%s\\n' \"${{#DOT_PROFILE_LIFECYCLE_RECORDS[@]}}\"; \
                 for r in \"${{DOT_PROFILE_LIFECYCLE_RECORDS[@]}}\"; do printf 'rec=%s\\n' \"$r\"; done",
                elig = sh_array(&shell_elig),
                po = sh_array(&shell_po),
                act = sh_array(&shell_act),
                prior = sh_array(&shell_prior),
                present = i32::from(row.present),
                api = api_flag,
            ),
        );
        assert_eq!(scode, 0, "shell harness prepare {}", row.label);
        let uid = euid();
        let log = logger();
        let mut warnings = Vec::new();
        let inputs = profile_lifecycle::PrepareInputs {
            present: row.present,
            extensions_enabled: row.enabled,
            eligible: &rust_elig,
            phase_one: &rust_po,
            active: &rust_act,
            prior: &rust_prior,
            ledger: row.with_var.then_some(rled.as_path()),
            home: &rust_home,
            euid: uid,
            log: &log,
        };
        let outcome = profile_lifecycle::prepare(&inputs, &mut warnings);
        let rust = state_dump(outcome.succeeded, &outcome.records);
        let shell = String::from_utf8(sout).expect("dump text");
        assert_eq!(
            normalize(&rust, &shell_home, &rust_home),
            normalize(&shell, &shell_home, &rust_home),
            "prepare dump for {}",
            row.label
        );
        assert_eq!(
            normalize(
                &String::from_utf8(warnings).expect("warnings text"),
                &shell_home,
                &rust_home
            ),
            normalize(
                &String::from_utf8(serr).expect("stderr text"),
                &shell_home,
                &rust_home
            ),
            "prepare warnings for {}",
            row.label
        );
        // The ledger file itself agrees too.
        assert_eq!(
            normalize(
                &String::from_utf8_lossy(&ledger_aftermath(&sled)),
                &shell_home,
                &rust_home
            ),
            normalize(
                &String::from_utf8_lossy(&ledger_aftermath(&rled)),
                &shell_home,
                &rust_home
            ),
            "prepare ledger for {}",
            row.label
        );
    }
}

#[test]
fn commit_rows_agree() {
    struct Row {
        label: &'static str,
        present: bool,
        enabled: bool,
        retained: &'static [&'static str],
        eligible: &'static [&'static str],
        active: &'static [&'static str],
        tweak: Tweak,
        with_var: bool,
    }
    let rows: &[Row] = &[
        Row {
            label: "absent",
            present: false,
            enabled: true,
            retained: &["web"],
            eligible: &["web"],
            active: &["web"],
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "disabled",
            present: true,
            enabled: false,
            retained: &["web"],
            eligible: &["web"],
            active: &["web"],
            tweak: Tweak::None,
            with_var: true,
        },
        // Retained eligible-but-inactive records keep ledger
        // order; refreshed actives append in sorted name order.
        Row {
            label: "reorder",
            present: true,
            enabled: true,
            retained: &["web10", "api"],
            eligible: &["web", "api"],
            active: &["web20"],
            tweak: Tweak::None,
            with_var: true,
        },
        Row {
            label: "drop-ineligible",
            present: true,
            enabled: true,
            retained: &["web", "api"],
            eligible: &["web"],
            active: &[],
            tweak: Tweak::None,
            with_var: true,
        },
        // An active entry point that went missing drops silently.
        Row {
            label: "drop-scriptless",
            present: true,
            enabled: true,
            retained: &["web"],
            eligible: &["web", "ghost"],
            active: &["ghost"],
            tweak: Tweak::None,
            with_var: true,
        },
        // An unsafe active entry point fails with no warning.
        Row {
            label: "unsafe-active",
            present: true,
            enabled: true,
            retained: &["web"],
            eligible: &["web"],
            active: &["web"],
            tweak: Tweak::OpenWebScript,
            with_var: true,
        },
        Row {
            label: "unset",
            present: true,
            enabled: true,
            retained: &["web"],
            eligible: &["web"],
            active: &["web"],
            tweak: Tweak::None,
            with_var: false,
        },
    ];
    for row in rows {
        let sdir = TempDir::new(&format!("lc-commit-{}-shell", row.label)).expect("shell dir");
        let rdir = TempDir::new(&format!("lc-commit-{}-rust", row.label)).expect("rust dir");
        let shell_home = sdir.path().to_string_lossy().into_owned();
        let rust_home = rdir.path().to_string_lossy().into_owned();
        let (sweb, sapi, sghost, _) = stage_orchestration(sdir.path());
        let (rweb, rapi, rghost, _) = stage_orchestration(rdir.path());
        apply_tweak(sdir.path(), row.tweak);
        apply_tweak(rdir.path(), row.tweak);
        let by_name = |home: &str, web: &str, api: &str, ghost: &str, key: &str| -> String {
            match key {
                "web" => web.to_string(),
                "api" => api.to_string(),
                "ghost" => ghost.to_string(),
                "web10" => record_descriptor(
                    home,
                    "web",
                    "file:///repo/web.git",
                    &format!("{home}/conf/10-web.conf"),
                ),
                "web20" => record_descriptor(
                    home,
                    "web",
                    "file:///repo/web.git",
                    &format!("{home}/conf/20-web.conf"),
                ),
                _ => unreachable!("unknown overlay {key}"),
            }
        };
        let resolve =
            |home: &str, web: &str, api: &str, ghost: &str, keys: &[&str]| -> Vec<String> {
                keys.iter()
                    .map(|key| by_name(home, web, api, ghost, key))
                    .collect()
            };
        let sled = sdir.path().join("ledger");
        let rled = rdir.path().join("ledger");
        // A known ledger proves the no-op rows leave it alone and
        // the live rows rewrite it.
        let staged = format!(
            "version=1\n{}\n",
            by_name(&shell_home, &sweb, &sapi, &sghost, "web")
        );
        stage_mode(sdir.path(), "ledger", staged.as_bytes(), 0o600);
        let staged = format!(
            "version=1\n{}\n",
            by_name(&rust_home, &rweb, &rapi, &rghost, "web")
        );
        stage_mode(rdir.path(), "ledger", staged.as_bytes(), 0o600);
        let shell_ret = resolve(&shell_home, &sweb, &sapi, &sghost, row.retained);
        let shell_elig: Vec<String> = row.eligible.iter().map(|key| key.to_string()).collect();
        let shell_act = resolve(&shell_home, &sweb, &sapi, &sghost, row.active);
        let rust_ret = resolve(&rust_home, &rweb, &rapi, &rghost, row.retained);
        let rust_elig: Vec<String> = row.eligible.iter().map(|key| key.to_string()).collect();
        let rust_act = resolve(&rust_home, &rweb, &rapi, &rghost, row.active);
        let setup = if row.with_var {
            "DOT_PROFILE_LIFECYCLE_LEDGER=\"$2\"; "
        } else {
            "unset DOT_PROFILE_LIFECYCLE_LEDGER; "
        };
        let api_flag = if row.enabled { "1" } else { "" };
        let (scode, sout, serr) = shell_run(
            sdir.path(),
            &[sled.as_os_str()],
            &[],
            &format!(
                "DOT_PROFILE_LIFECYCLE_RECORDS=({retained}); ELIGIBLE_OVERLAY_NAMES=({elig}); \
                 ACTIVE_OVERLAYS=({act}); DOT_PROFILES_PRESENT={present}; \
                 DOT_EXTENSION_API={api}; DOT_EXTENSIONS_DIR=\"$HOME/ext\"; \
                 {setup}_dot_profile_lifecycle_commit; printf 'rc=%s\\n' \"$?\"",
                retained = sh_array(&shell_ret),
                elig = sh_array(&shell_elig),
                act = sh_array(&shell_act),
                present = i32::from(row.present),
                api = api_flag,
            ),
        );
        assert_eq!(scode, 0, "shell harness commit {}", row.label);
        // Commit never warns on either side.
        assert_eq!(serr, b"", "shell silent for {}", row.label);
        let uid = euid();
        let inputs = profile_lifecycle::CommitInputs {
            present: row.present,
            extensions_enabled: row.enabled,
            retained: &rust_ret,
            eligible: &rust_elig,
            active: &rust_act,
            ledger: row.with_var.then_some(rled.as_path()),
            home: &rust_home,
            euid: uid,
        };
        let ok = profile_lifecycle::commit(&inputs);
        let shell = String::from_utf8(sout).expect("dump text");
        assert_eq!(
            shell,
            format!("rc={}\n", i32::from(!ok)),
            "commit code for {}",
            row.label
        );
        assert_eq!(
            normalize(
                &String::from_utf8_lossy(&ledger_aftermath(&sled)),
                &shell_home,
                &rust_home
            ),
            normalize(
                &String::from_utf8_lossy(&ledger_aftermath(&rled)),
                &shell_home,
                &rust_home
            ),
            "commit ledger for {}",
            row.label
        );
    }
}

/// [`profile_lifecycle::WorkerRun`] over the live shell worker: the
/// seam spawns the same `_dot_extension_worker_run` the shell side
/// runs in-process, with the same scrubbed environment, so the
/// comparison covers the ported plumbing while the leaf worker is
/// identical. Standard error joins standard output in the child
/// (the shell's `2>&1`), which is the combined stream `run_one`
/// relays.
struct ShellWorker {
    home: PathBuf,
    verbose: bool,
    calls: usize,
}

impl ShellWorker {
    fn new(home: &Path, verbose: bool) -> Self {
        ShellWorker {
            home: home.to_path_buf(),
            verbose,
            calls: 0,
        }
    }
}

impl WorkerRun for ShellWorker {
    fn run(
        &mut self,
        script: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome {
        self.calls += 1;
        let repo = env!("CARGO_MANIFEST_DIR");
        let path = std::env::var_os("PATH").unwrap_or_default();
        let tmpdir = parent_tmpdir();
        let mut cmd = Command::new(dot::test_support::bash());
        cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
            "exec 2>&1\n{SOURCES}_dot_extension_worker_run deactivate \"$2\" \"$3\" \"$4\" \"$5\" \"$6\""
        ));
        cmd.arg("dot-test-worker")
            .arg(repo)
            .arg(script)
            .arg(result_dir)
            .arg(result_file)
            .arg(context)
            .arg(token);
        cmd.env_clear()
            .env("LC_ALL", "C")
            .env("PATH", &path)
            .env("TMPDIR", &tmpdir)
            .env("HOME", &self.home)
            .env("DOT_TEST", "1")
            .env("DOT_SOURCE_ROOT", repo)
            .env("DOT_VERBOSE", if self.verbose { "1" } else { "0" })
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let output = cmd.output().expect("spawn worker");
        WorkerOutcome {
            rc: output.status.code().unwrap_or(99),
            output: output.stdout,
        }
    }
}

#[test]
fn run_one_rows_agree() {
    // (label, script body, record key, verbose, want worker
    // calls, TMPDIR-is-a-file): scripts define the `deactivate`
    // entry point the worker sources. `None` is a missing
    // checkout (missing entry point).
    struct Row {
        label: &'static str,
        script: Option<&'static [u8]>,
        record_key: &'static str,
        verbose: bool,
        calls: usize,
        tmp_file: bool,
    }
    let rows: &[Row] = &[
        Row {
            label: "silent",
            script: Some(b"deactivate() { return 0; }\n"),
            record_key: "web",
            verbose: false,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "loud-quiet",
            script: Some(b"deactivate() { echo hello-deactivate; }\n"),
            record_key: "web",
            verbose: false,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "loud-verbose",
            script: Some(b"deactivate() { echo hello-deactivate; }\n"),
            record_key: "web",
            verbose: true,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "fail",
            script: Some(b"deactivate() { echo boom-deactivate; return 3; }\n"),
            record_key: "web",
            verbose: false,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "stderr-fail",
            script: Some(b"deactivate() { echo oops-deactivate >&2; return 2; }\n"),
            record_key: "web",
            verbose: false,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "stripped",
            script: Some(b"deactivate() { printf 'a\n\n\n'; }\n"),
            record_key: "web",
            verbose: true,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "no-entry",
            script: Some(b"# no entry point here\n"),
            record_key: "web",
            verbose: false,
            calls: 1,
            tmp_file: false,
        },
        Row {
            label: "invalid-record",
            script: Some(b"deactivate() { return 0; }\n"),
            record_key: "garbage",
            verbose: false,
            calls: 0,
            tmp_file: false,
        },
        Row {
            label: "missing-script",
            script: None,
            record_key: "ghost",
            verbose: false,
            calls: 0,
            tmp_file: false,
        },
        Row {
            label: "tmpdir-file",
            script: Some(b"deactivate() { return 0; }\n"),
            record_key: "web",
            verbose: false,
            calls: 0,
            tmp_file: true,
        },
    ];
    for row in rows {
        let sdir = TempDir::new(&format!("lc-run-{}-shell", row.label)).expect("shell dir");
        let rdir = TempDir::new(&format!("lc-run-{}-rust", row.label)).expect("rust dir");
        let shell_home = sdir.path().to_string_lossy().into_owned();
        let rust_home = rdir.path().to_string_lossy().into_owned();
        let sweb = checkout(
            sdir.path(),
            "web",
            "file:///repo/web.git",
            row.script.map(|body| (body, 0o600)),
        );
        let rweb = checkout(
            rdir.path(),
            "web",
            "file:///repo/web.git",
            row.script.map(|body| (body, 0o600)),
        );
        let srec = match row.record_key {
            "garbage" => "GARBAGE".to_string(),
            "ghost" => record(&shell_home, "ghost", "file:///repo/ghost.git"),
            _ => sweb.clone(),
        };
        let rrec = match row.record_key {
            "garbage" => "GARBAGE".to_string(),
            "ghost" => record(&rust_home, "ghost", "file:///repo/ghost.git"),
            _ => rweb.clone(),
        };
        // A file for TMPDIR breaks scratch allocation on both
        // sides before the worker is reached.
        let stmp = sdir.path().join("not-a-dir");
        let rtmp = rdir.path().join("not-a-dir");
        if row.tmp_file {
            stage(sdir.path(), "not-a-dir", b"in the way\n");
            stage(rdir.path(), "not-a-dir", b"in the way\n");
        }
        let tmp_override: Vec<(&str, Option<&str>)> = if row.tmp_file {
            vec![("TMPDIR", Some(stmp.to_str().expect("tmpdir text")))]
        } else {
            Vec::new()
        };
        let verbose_flag = if row.verbose { "1" } else { "0" };
        let (scode, sout, serr) = shell_run(
            sdir.path(),
            &[srec.as_ref()],
            &tmp_override,
            &format!(
                "DOT_VERBOSE={verbose_flag}; _dot_profile_lifecycle_run_one \"$2\"; printf 'rc=%s\\n' \"$?\""
            ),
        );
        assert_eq!(scode, 0, "shell harness run {}", row.label);
        let uid = euid();
        let log = logger();
        let mut worker = ShellWorker::new(rdir.path(), row.verbose);
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        let tmpdir = if row.tmp_file { rtmp } else { parent_tmpdir() };
        let inputs = profile_lifecycle::RunInputs {
            record: &rrec,
            home: &rust_home,
            euid: uid,
            tmpdir: &tmpdir,
            now_secs: now_secs(),
            verbose: row.verbose,
            log: &log,
        };
        let rc = profile_lifecycle::run_one(&inputs, &mut worker, &mut out, &mut warnings);
        assert_eq!(worker.calls, row.calls, "worker calls for {}", row.label);
        // The shell dump is relayed stdout plus a trailing `rc=`
        // line; split them for a shape-exact comparison.
        let shell_text = String::from_utf8(sout).expect("dump text");
        let lines: Vec<&str> = shell_text.lines().collect();
        let rc_line = lines.last().expect("rc line");
        let shell_rc: i32 = rc_line
            .strip_prefix("rc=")
            .expect("rc prefix")
            .parse()
            .expect("rc number");
        assert_eq!(shell_rc, rc, "run rc for {}", row.label);
        let mut expected = format!("rc={shell_rc}\n");
        for line in &lines[..lines.len() - 1] {
            expected.push_str(line);
            expected.push('\n');
        }
        let mut rust = format!("rc={rc}\n");
        rust.push_str(&String::from_utf8(out).expect("stdout text"));
        assert_eq!(
            normalize(&rust, &shell_home, &rust_home),
            normalize(&expected, &shell_home, &rust_home),
            "run stdout for {}",
            row.label
        );
        if row.label == "tmpdir-file" {
            // The shell surfaces the `mktemp -d` tool error for a
            // file TMPDIR; the port stays silent (rc carries the
            // verdict on both sides). Only the `strerror` tail is
            // asserted: GNU and BSD `mktemp` wordings differ.
            assert!(warnings.is_empty(), "rust silent for {}", row.label);
            let noise = String::from_utf8(serr).expect("stderr text");
            assert!(
                noise.contains("Not a directory"),
                "mktemp cause for {}",
                row.label
            );
        } else {
            assert_eq!(
                normalize(
                    &String::from_utf8(warnings).expect("warnings text"),
                    &shell_home,
                    &rust_home
                ),
                normalize(
                    &String::from_utf8(serr).expect("stderr text"),
                    &shell_home,
                    &rust_home
                ),
                "run warnings for {}",
                row.label
            );
        }
    }
}

#[test]
fn retire_rows_agree() {
    struct Row {
        label: &'static str,
        present: bool,
        enabled: bool,
        retained: &'static [&'static str],
        eligible: &'static [&'static str],
        verbose: bool,
    }
    let rows: &[Row] = &[
        Row {
            label: "absent",
            present: false,
            enabled: true,
            retained: &["web", "api", "fail"],
            eligible: &[],
            verbose: false,
        },
        Row {
            label: "disabled",
            present: true,
            enabled: false,
            retained: &["web", "api", "fail"],
            eligible: &[],
            verbose: false,
        },
        Row {
            label: "mixed-quiet",
            present: true,
            enabled: true,
            retained: &["web", "api", "fail"],
            eligible: &["web"],
            verbose: false,
        },
        Row {
            label: "mixed-verbose",
            present: true,
            enabled: true,
            retained: &["web", "api", "fail"],
            eligible: &["web"],
            verbose: true,
        },
        Row {
            label: "all-eligible",
            present: true,
            enabled: true,
            retained: &["web", "api"],
            eligible: &["web", "api"],
            verbose: false,
        },
        Row {
            label: "empty",
            present: true,
            enabled: true,
            retained: &[],
            eligible: &[],
            verbose: false,
        },
        Row {
            label: "garbage",
            present: true,
            enabled: true,
            retained: &["garbage"],
            eligible: &[],
            verbose: false,
        },
    ];
    for row in rows {
        let sdir = TempDir::new(&format!("lc-retire-{}-shell", row.label)).expect("shell dir");
        let rdir = TempDir::new(&format!("lc-retire-{}-rust", row.label)).expect("rust dir");
        let shell_home = sdir.path().to_string_lossy().into_owned();
        let rust_home = rdir.path().to_string_lossy().into_owned();
        // `web` stays eligible and silent, `api` retires loud,
        // `fail` retires failing.
        for base in [sdir.path(), rdir.path()] {
            checkout(
                base,
                "web",
                "file:///repo/web.git",
                Some((b"deactivate() { return 0; }\n", 0o600)),
            );
            checkout(
                base,
                "api",
                "file:///repo/api.git",
                Some((b"deactivate() { echo api-gone; }\n", 0o600)),
            );
            checkout(
                base,
                "fail",
                "file:///repo/fail.git",
                Some((b"deactivate() { echo boom-retire; return 3; }\n", 0o600)),
            );
        }
        let by_name = |home: &str, key: &str| -> String {
            match key {
                "garbage" => "GARBAGE".to_string(),
                name => record(home, name, &format!("file:///repo/{name}.git")),
            }
        };
        let shell_ret: Vec<String> = row
            .retained
            .iter()
            .map(|key| by_name(&shell_home, key))
            .collect();
        let rust_ret: Vec<String> = row
            .retained
            .iter()
            .map(|key| by_name(&rust_home, key))
            .collect();
        let shell_elig: Vec<String> = row
            .eligible
            .iter()
            .map(|key| by_name(&shell_home, key))
            .collect();
        // Eligibility compares names only, so one spelling serves
        // both sides.
        let rust_elig: Vec<String> = row.eligible.iter().map(|key| key.to_string()).collect();
        let verbose_flag = if row.verbose { "1" } else { "0" };
        let api_flag = if row.enabled { "1" } else { "" };
        let (scode, sout, serr) = shell_run(
            sdir.path(),
            &[],
            &[],
            &format!(
                "DOT_PROFILE_LIFECYCLE_RECORDS=({retained}); ELIGIBLE_OVERLAY_NAMES=({elig}); \
                 DOT_PROFILES_PRESENT={present}; DOT_EXTENSION_API={api}; DOT_EXTENSIONS_DIR=\"$HOME/ext\"; \
                 DOT_VERBOSE={verbose_flag}; _dot_profile_lifecycle_retire; printf 'rc=%s\\n' \"$?\"",
                retained = sh_array(&shell_ret),
                elig = sh_array(&shell_elig),
                present = i32::from(row.present),
                api = api_flag,
            ),
        );
        assert_eq!(scode, 0, "shell harness retire {}", row.label);
        let uid = euid();
        let log = logger();
        let mut worker = ShellWorker::new(rdir.path(), row.verbose);
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        let tmpdir = parent_tmpdir();
        let inputs = profile_lifecycle::RetireInputs {
            present: row.present,
            extensions_enabled: row.enabled,
            retained: &rust_ret,
            eligible: &rust_elig,
            home: &rust_home,
            euid: uid,
            tmpdir: &tmpdir,
            now_secs: now_secs(),
            verbose: row.verbose,
            log: &log,
        };
        let rc = profile_lifecycle::retire(&inputs, &mut worker, &mut out, &mut warnings);
        if row.label == "all-eligible" || row.label == "empty" {
            assert_eq!(worker.calls, 0, "no worker runs for {}", row.label);
        }
        let shell_text = String::from_utf8(sout).expect("dump text");
        let lines: Vec<&str> = shell_text.lines().collect();
        let shell_rc: i32 = lines
            .last()
            .expect("rc line")
            .strip_prefix("rc=")
            .expect("rc prefix")
            .parse()
            .expect("rc number");
        assert_eq!(shell_rc, rc, "retire rc for {}", row.label);
        let mut expected = format!("rc={shell_rc}\n");
        for line in &lines[..lines.len() - 1] {
            expected.push_str(line);
            expected.push('\n');
        }
        let mut rust = format!("rc={rc}\n");
        rust.push_str(&String::from_utf8(out).expect("stdout text"));
        assert_eq!(
            normalize(&rust, &shell_home, &rust_home),
            normalize(&expected, &shell_home, &rust_home),
            "retire stdout for {}",
            row.label
        );
        assert_eq!(
            normalize(
                &String::from_utf8(warnings).expect("warnings text"),
                &shell_home,
                &rust_home
            ),
            normalize(
                &String::from_utf8(serr).expect("stderr text"),
                &shell_home,
                &rust_home
            ),
            "retire warnings for {}",
            row.label
        );
    }
}
