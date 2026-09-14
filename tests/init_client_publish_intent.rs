//! Native behavioral tests for init intent publication.
//!
//! The fixtures pin publication branches, bytes, modes, and failure behavior
//! directly against the Rust owner; the removed private Bash engine is not a
//! test dependency.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use dot::init_client_publish_intent::{
    EntryIntentCheck, EntryStage, PublishIntentHooks, WritePrivateLine, publish_intent,
};
use dot_test_support::TempDir;

// Boxed live hooks, aliased so clippy's complexity lint stays quiet.
type StageHook<'a> = Box<EntryStage<'a>>;
type IntentHook<'a> = Box<EntryIntentCheck<'a>>;
type WriteHook<'a> = Box<WritePrivateLine<'a>>;

/// Fixed nonce shared by every row.
const NONCE: &str = "n73";

/// Empty-blob oid every fresh row publishes.
const OID: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

/// Native entry-stage hook with `HOME` steered at `home`.
fn live_entry_stage(home: &Path) -> StageHook<'_> {
    Box::new(move |path| {
        let path = std::str::from_utf8(path).map_err(|_| dot::Error::Usage {
            message: "path is not UTF-8",
        })?;
        dot::init_client_entry::entry_stage(
            home,
            path,
            NONCE,
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
    })
}

/// Native entry-intent hook.
fn live_entry_intent(home: &Path) -> IntentHook<'_> {
    Box::new(move |file, mode, oid, path| {
        let path = std::str::from_utf8(path).map_err(|_| dot::Error::Usage {
            message: "path is not UTF-8",
        })?;
        dot::init_client_entry::entry_intent(
            file,
            mode,
            oid,
            path,
            home,
            NONCE,
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .map(|_| ())
    })
}

/// Native private-line writer for a fresh intent.
fn live_write_private_line(home: &Path) -> WriteHook<'_> {
    let _ = home;
    Box::new(move |file, line| {
        let line = std::str::from_utf8(line).map_err(|_| dot::Error::Usage {
            message: "line is not UTF-8",
        })?;
        let mut cache = dot::temp::MoveCache::default();
        dot::init_client_entry::write_private_line(file, line, false, &mut cache)
    })
}

/// Isolated home plus native publication hooks.
struct Twins {
    _dir: TempDir,
    rust_home: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        Self {
            _dir: dir,
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
    let rust_file = twins.rust_home.join("intent");
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, mode, oid, path, &twins.rust_home);
    assert!(
        rust_result.is_ok(),
        "port failed for {tag}: {rust_result:?}"
    );
    let rust_bytes = std::fs::read(&rust_file).expect("rust intent bytes");
    assert!(rust_bytes.starts_with(format!("pending\t{mode}\t{oid}\t").as_bytes()));
    assert!(rust_bytes.ends_with(b"\t-\t-\t-\t-\n"));
    assert_eq!(mode_of(&rust_file), 0o600, "port intent mode for {tag}");
}

#[test]
fn publish_fresh_top_level_has_expected_contract() {
    fresh_row("publish-fresh-top", "100644", OID, b"doc.txt");
}

#[test]
fn publish_fresh_nested_has_expected_contract() {
    fresh_row("publish-fresh-nested", "100644", OID, b"a/b/doc.txt");
}

#[test]
fn publish_fresh_executable_has_expected_contract() {
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
    let rust_file = twins.rust_home.join("intent");
    let hooks = twins.hooks();
    publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home).unwrap();
    let fixture = std::fs::read(&rust_file).expect("fixture bytes");
    let rust_result = publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home);
    assert!(rust_result.is_ok(), "port rejected: {rust_result:?}");
    assert_eq!(
        std::fs::read(&rust_file).expect("rust bytes"),
        fixture,
        "port rewrote a valid intent",
    );
}

/// A stale intent (oid no longer matches) refuses on both engines
/// and neither side touches the record.
#[test]
fn publish_existing_stale_refuses() {
    let twins = Twins::build("publish-stale");
    let path = b"doc.txt";
    let rust_file = twins.rust_home.join("intent");
    let hooks = twins.hooks();
    publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home).unwrap();
    let fixture = std::fs::read(&rust_file).expect("fixture bytes");
    let other = "0123456789abcdef0123456789abcdef0123456789";
    let rust_result = publish_intent(&hooks, &rust_file, "100644", other, path, &twins.rust_home);
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
    let rust_file = twins.rust_home.join("intent");
    std::fs::create_dir_all(&rust_file).expect("rust dir fixture");
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home);
    assert!(rust_result.is_err(), "port accepted a directory intent");
}

/// A dangling symlink is lexically present, so validation runs and
/// refuses on both engines.
#[test]
fn publish_dangling_symlink_refuses() {
    let twins = Twins::build("publish-dangling");
    let path = b"doc.txt";
    let rust_file = twins.rust_home.join("intent");
    std::os::unix::fs::symlink("nowhere", &rust_file).expect("rust link");
    let hooks = twins.hooks();
    let rust_result = publish_intent(&hooks, &rust_file, "100644", OID, path, &twins.rust_home);
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
