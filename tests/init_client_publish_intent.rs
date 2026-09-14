//! Differential parity tests for the init intent publisher
//! (`lib/dot/init-client.sh` lines 927-938,
//! `_dot_init_publish_intent`) against the live shell.
//!
//! Twin homes keep the engines from colliding: the oracle publishes
//! under `shell_home`, the port under `rust_home`, with the same
//! nonce so the derived stage bytes agree. The port's three
//! out-of-scope calls (`_dot_init_entry_stage`,
//! `_dot_init_entry_intent`, `_dot_init_write_private_line`) cross as
//! closures that run the live shell functions, so every row compares
//! the orchestration — which branch ran, which bytes landed, which
//! status returned — byte for byte.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_publish_intent::{
    EntryIntentCheck, EntryStage, PublishIntentHooks, WritePrivateLine, publish_intent,
};
use dot::test_support::TempDir;

// Boxed live hooks, aliased so clippy's complexity lint stays quiet.
type StageHook<'a> = Box<EntryStage<'a>>;
type IntentHook<'a> = Box<EntryIntentCheck<'a>>;
type WriteHook<'a> = Box<WritePrivateLine<'a>>;

/// Shell prelude for every probe: the temp helpers (sibling temps,
/// exclusive moves) plus the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixed nonce shared by both engines in every row.
const NONCE: &str = "n73";

/// Empty-blob oid every fresh row publishes.
const OID: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

/// Run one shell snippet with `HOME` steered at `home` and report
/// the snippet's own verdict alongside both byte streams. The
/// snippet always ends with `printf 'code=%s\n' "$code" >&2`, so the
/// returned code is that verdict — stdout stays reserved for data
/// (`$REPLY` echoes), keeping reply bytes exact.
fn shell_run(
    home: &Path,
    nonce: &str,
    env: &[(&str, &OsStr)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "TMPDIR",
            std::env::var_os("TMPDIR")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| OsString::from("/tmp")),
        )
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .env("DOT_INIT_NONCE", nonce)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    let verdict = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| {
            line.strip_prefix("code=")
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(99);
    (verdict, output.stdout, output.stderr)
}

/// Oracle: the live `_dot_init_publish_intent`.
fn shell_publish(
    home: &Path,
    file: &Path,
    mode: &str,
    oid: &str,
    path: &[u8],
) -> (i32, Vec<u8>, Vec<u8>) {
    let file = file.as_os_str().to_os_string();
    let path = OsString::from_vec(path.to_vec());
    shell_run(
        home,
        NONCE,
        &[
            ("DOT_TEST_FILE", file.as_os_str()),
            ("DOT_TEST_MODE", OsStr::from_bytes(mode.as_bytes())),
            ("DOT_TEST_OID", OsStr::from_bytes(oid.as_bytes())),
            ("DOT_TEST_PATH", path.as_os_str()),
        ],
        "_dot_init_publish_intent \"$DOT_TEST_FILE\" \"$DOT_TEST_MODE\" \"$DOT_TEST_OID\" \"$DOT_TEST_PATH\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
    )
}

/// Live `_dot_init_entry_stage`: echoes `$REPLY` on stdout.
fn live_entry_stage(home: &Path) -> StageHook<'_> {
    Box::new(move |path| {
        let arg = OsString::from_vec(path.to_vec());
        let (code, stdout, _) = shell_run(
            home,
            NONCE,
            &[("DOT_TEST_PATH", arg.as_os_str())],
            "_dot_init_entry_stage \"$DOT_TEST_PATH\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\nif [ \"$code\" -eq 0 ]; then printf '%s' \"$REPLY\"; fi\n",
        );
        if code != 0 {
            return Err(dot::Error::Usage {
                message: "shell entry stage refused",
            });
        }
        Ok(PathBuf::from(OsString::from_vec(stdout)))
    })
}

/// Live `_dot_init_entry_intent`, output discarded like the shell's
/// `>/dev/null`.
fn live_entry_intent(home: &Path) -> IntentHook<'_> {
    Box::new(move |file, mode, oid, path| {
        let file = file.as_os_str().to_os_string();
        let arg = OsString::from_vec(path.to_vec());
        let (code, _, _) = shell_run(
            home,
            NONCE,
            &[
                ("DOT_TEST_FILE", file.as_os_str()),
                ("DOT_TEST_MODE", OsStr::from_bytes(mode.as_bytes())),
                ("DOT_TEST_OID", OsStr::from_bytes(oid.as_bytes())),
                ("DOT_TEST_PATH", arg.as_os_str()),
            ],
            "_dot_init_entry_intent \"$DOT_TEST_FILE\" \"$DOT_TEST_MODE\" \"$DOT_TEST_OID\" \"$DOT_TEST_PATH\" >/dev/null\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
        );
        if code != 0 {
            return Err(dot::Error::Usage {
                message: "shell entry intent refused",
            });
        }
        Ok(())
    })
}

/// Live `_dot_init_write_private_line` for a fresh intent
/// (`replace` is always false here, like the shell call site).
fn live_write_private_line(home: &Path) -> WriteHook<'_> {
    Box::new(move |file, line| {
        let file = file.as_os_str().to_os_string();
        let line = OsString::from_vec(line.to_vec());
        let (code, _, _) = shell_run(
            home,
            NONCE,
            &[
                ("DOT_TEST_FILE", file.as_os_str()),
                ("DOT_TEST_LINE", line.as_os_str()),
            ],
            "_dot_init_write_private_line \"$DOT_TEST_FILE\" \"$DOT_TEST_LINE\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
        );
        if code != 0 {
            return Err(dot::Error::Usage {
                message: "shell write private line refused",
            });
        }
        Ok(())
    })
}

/// Twin homes plus the live-backed hooks for the Rust side.
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&shell_home).expect("shell home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        Self {
            _dir: dir,
            shell_home,
            rust_home,
        }
    }

    fn hooks(&self) -> PublishIntentHooks<'_> {
        PublishIntentHooks {
            entry_stage: live_entry_stage(&self.rust_home),
            entry_intent: live_entry_intent(&self.rust_home),
            write_private_line: live_write_private_line(&self.rust_home),
        }
    }
}

/// File mode bits in `stat %a` spelling, read in-process (never a
/// bare GNU `stat -c`).
fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("fixture stat")
        .permissions()
        .mode()
        & 0o777
}

/// One fresh-publish row: both engines write the identical pending
/// intent at mode 0600.
fn fresh_row(tag: &str, mode: &str, oid: &str, path: &[u8]) {
    let twins = Twins::build(tag);
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    let (shell_code, _, _) = shell_publish(&twins.shell_home, &shell_file, mode, oid, path);
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, mode, oid, path, &twins.rust_home);
    assert_eq!(shell_code, 0, "oracle failed for {tag}");
    assert!(
        rust_result.is_ok(),
        "port failed for {tag}: {rust_result:?}"
    );
    let shell_bytes = std::fs::read(&shell_file).expect("shell intent bytes");
    let rust_bytes = std::fs::read(&rust_file).expect("rust intent bytes");
    assert_eq!(
        rust_bytes,
        shell_bytes,
        "intent bytes diverge for {tag}:\nport : {:?}\nshell: {:?}",
        String::from_utf8_lossy(&rust_bytes),
        String::from_utf8_lossy(&shell_bytes),
    );
    assert_eq!(mode_of(&rust_file), 0o600, "port intent mode for {tag}");
    assert_eq!(mode_of(&shell_file), 0o600, "oracle intent mode for {tag}");
}

#[test]
fn publish_fresh_top_level_matches_shell() {
    fresh_row("publish-fresh-top", "100644", OID, b"doc.txt");
}

#[test]
fn publish_fresh_nested_matches_shell() {
    fresh_row("publish-fresh-nested", "100644", OID, b"a/b/doc.txt");
}

#[test]
fn publish_fresh_executable_matches_shell() {
    fresh_row("publish-fresh-exec", "100755", OID, b"bin/run");
}

/// An existing pending intent validates and stays byte-identical on
/// both engines. The fixture crosses sides verbatim: the stage is
/// home-independent (parent plus nonce plus path hash), so the same
/// bytes are live under either home.
#[test]
fn publish_existing_pending_validates() {
    let twins = Twins::build("publish-existing");
    let path = b"a/doc.txt";
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    let (setup_code, _, _) = shell_publish(&twins.shell_home, &shell_file, "100644", OID, path);
    assert_eq!(setup_code, 0, "fixture setup failed");
    let fixture = std::fs::read(&shell_file).expect("fixture bytes");
    std::fs::write(&rust_file, &fixture).expect("stage fixture");
    std::fs::set_permissions(&rust_file, std::fs::Permissions::from_mode(0o600))
        .expect("fixture chmod");
    let (shell_code, _, _) = shell_publish(&twins.shell_home, &shell_file, "100644", OID, path);
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home);
    assert_eq!(shell_code, 0, "oracle rejected its own intent");
    assert!(rust_result.is_ok(), "port rejected: {rust_result:?}");
    assert_eq!(
        std::fs::read(&rust_file).expect("rust bytes"),
        fixture,
        "port rewrote a valid intent",
    );
    assert_eq!(
        std::fs::read(&shell_file).expect("shell bytes"),
        fixture,
        "oracle rewrote a valid intent",
    );
}

/// A stale intent (oid no longer matches) refuses on both engines
/// and neither side touches the record.
#[test]
fn publish_existing_stale_refuses() {
    let twins = Twins::build("publish-stale");
    let path = b"doc.txt";
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    let (setup_code, _, _) = shell_publish(&twins.shell_home, &shell_file, "100644", OID, path);
    assert_eq!(setup_code, 0, "fixture setup failed");
    let fixture = std::fs::read(&shell_file).expect("fixture bytes");
    std::fs::write(&rust_file, &fixture).expect("stage fixture");
    std::fs::set_permissions(&rust_file, std::fs::Permissions::from_mode(0o600))
        .expect("fixture chmod");
    let other = "0123456789abcdef0123456789abcdef0123456789";
    let (shell_code, _, _) = shell_publish(&twins.shell_home, &shell_file, "100644", other, path);
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, "100644", other, path, &twins.rust_home);
    assert_ne!(shell_code, 0, "oracle accepted a stale intent");
    assert!(rust_result.is_err(), "port accepted a stale intent");
    assert_eq!(
        std::fs::read(&rust_file).expect("rust bytes"),
        fixture,
        "port touched a stale intent",
    );
}

/// A directory at the intent path fails the record gate on both
/// engines: `[[ -e || -L ]]` is true, so validation (not the fresh
/// write) runs and refuses.
#[test]
fn publish_existing_directory_refuses() {
    let twins = Twins::build("publish-isdir");
    let path = b"doc.txt";
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    std::fs::create_dir_all(&shell_file).expect("shell dir fixture");
    std::fs::create_dir_all(&rust_file).expect("rust dir fixture");
    let (shell_code, _, _) = shell_publish(&twins.shell_home, &shell_file, "100644", OID, path);
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home);
    assert_ne!(shell_code, 0, "oracle accepted a directory intent");
    assert!(rust_result.is_err(), "port accepted a directory intent");
}

/// A dangling symlink is lexically present, so validation runs and
/// refuses on both engines.
#[test]
fn publish_dangling_symlink_refuses() {
    let twins = Twins::build("publish-dangling");
    let path = b"doc.txt";
    let shell_file = twins.shell_home.join("intent");
    let rust_file = twins.rust_home.join("intent");
    std::os::unix::fs::symlink("nowhere", &shell_file).expect("shell link");
    std::os::unix::fs::symlink("nowhere", &rust_file).expect("rust link");
    let (shell_code, _, _) = shell_publish(&twins.shell_home, &shell_file, "100644", OID, path);
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home);
    assert_ne!(shell_code, 0, "oracle accepted a dangling intent");
    assert!(rust_result.is_err(), "port accepted a dangling intent");
}

/// A failing stage derivation refuses before touching the
/// filesystem: the intent file must not appear.
#[test]
fn publish_stage_failure_writes_nothing() {
    let twins = Twins::build("publish-stage-fail");
    let rust_file = twins.rust_home.join("intent");
    let hooks = PublishIntentHooks {
        entry_stage: Box::new(|_| {
            Err(dot::Error::Usage {
                message: "no stage",
            })
        }),
        entry_intent: live_entry_intent(&twins.rust_home),
        write_private_line: live_write_private_line(&twins.rust_home),
    };
    let result = publish_intent(
        &hooks,
        &rust_file,
        "100644",
        OID,
        b"doc.txt",
        &twins.rust_home,
    );
    assert!(result.is_err(), "port ignored a stage failure");
    assert!(
        std::fs::symlink_metadata(&rust_file).is_err(),
        "port wrote despite a stage failure",
    );
}
