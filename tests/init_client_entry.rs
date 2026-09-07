//! Native contract tests for per-entry staging and publication.

use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_entry as entry;
use dot::temp::MoveCache;
use dot_test_support::TempDir;

const NONCE: &str = "test-nonce-46";
const PUBLISH_NONCE: &str = "test-nonce-67";
const MODE: &str = "100644";
const OID: &str = "0123456789abcdef0123456789abcdef01234567";

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn home(tag: &str) -> TempDir {
    TempDir::new(tag).expect("temp dir")
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

fn tmp_residue(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("list dir")
        .filter_map(Result::ok)
        .filter(|item| item.file_name().to_string_lossy().contains(".tmp."))
        .count()
}

#[test]
fn private_line_creates_mode_600() {
    for (tag, line) in [("new", "pending\ta\tb"), ("tabs", "a\tb c")] {
        let dir = home(tag);
        let file = dir.path().join("intent");
        entry::write_private_line(&file, line, false, &mut MoveCache::default()).expect("write");
        assert_eq!(
            std::fs::read(&file).expect("bytes"),
            format!("{line}\n").as_bytes()
        );
        assert_eq!(mode_of(&file), 0o600);
    }
}

#[test]
fn private_line_noreplace_keeps_live_file() {
    let dir = home("line-lived");
    let file = dir.path().join("intent");
    std::fs::write(&file, b"live\n").expect("seed");
    assert!(entry::write_private_line(&file, "pending", false, &mut MoveCache::default()).is_err());
    assert_eq!(std::fs::read(&file).expect("bytes"), b"live\n");
    assert!(tmp_residue(dir.path()) >= 1);
}

#[test]
fn private_line_replace_swaps() {
    let dir = home("line-replace");
    let file = dir.path().join("intent");
    std::fs::write(&file, b"live\n").expect("seed");
    entry::write_private_line(&file, "staged", true, &mut MoveCache::default()).expect("replace");
    assert_eq!(std::fs::read(&file).expect("bytes"), b"staged\n");
    let fresh = dir.path().join("fresh");
    entry::write_private_line(&fresh, "x", true, &mut MoveCache::default()).expect("fresh replace");
    assert_eq!(std::fs::read(fresh).expect("bytes"), b"x\n");
}

#[test]
fn private_line_umask_077_stays_600() {
    let dir = home("line-mode");
    let file = dir.path().join("intent");
    entry::write_private_line(&file, "pending", false, &mut MoveCache::default()).expect("write");
    assert_eq!(mode_of(&file), 0o600);
}

fn git_digest(bytes: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(bytes)
        .expect("feed git");
    let output = child.wait_with_output().expect("wait git");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("oid")
        .trim()
        .to_string()
}

fn expected_stage(root: &Path, path: &str, nonce: &str) -> PathBuf {
    let hash = git_digest(path.as_bytes());
    match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => root
            .join(parent)
            .join(format!(".dot-init-entry.{nonce}.{hash}")),
        _ => root.join(format!(".dot-init-entry.{nonce}.{hash}")),
    }
}

#[test]
fn entry_stage_shapes() {
    let dir = home("stage-shapes");
    for path in ["dotfile", "a/b", "a/b/c"] {
        assert_eq!(
            entry::entry_stage(dir.path(), path, NONCE, &source_root()).expect("stage"),
            expected_stage(dir.path(), path, NONCE)
        );
    }
}

#[test]
fn entry_stage_trailing_slash_home() {
    let dir = home("stage-slash");
    let root = PathBuf::from(format!("{}/", dir.path().display()));
    let got = entry::entry_stage(&root, "top", NONCE, &source_root()).expect("stage");
    assert_eq!(
        got.as_os_str().as_bytes(),
        format!(
            "{}//.dot-init-entry.{NONCE}.{}",
            dir.path().display(),
            git_digest(b"top")
        )
        .as_bytes()
    );
}

fn stage_rel(root: &Path, path: &str) -> String {
    expected_stage(root, path, NONCE)
        .strip_prefix(root)
        .expect("relative")
        .to_string_lossy()
        .into_owned()
}

#[allow(clippy::too_many_arguments)]
fn intent_body(
    phase: &str,
    mode: &str,
    oid: &str,
    path: &str,
    stage: &str,
    dev: &str,
    ino: &str,
    next_dev: &str,
    next_ino: &str,
) -> Vec<u8> {
    format!("{phase}\t{mode}\t{oid}\t{path}\t{stage}\t{dev}\t{ino}\t{next_dev}\t{next_ino}\n")
        .into_bytes()
}

#[test]
fn entry_intent_accepts_phases() {
    let dir = home("intent-phases");
    let stage = stage_rel(dir.path(), "a/b");
    for (phase, ids) in [
        ("pending", ["-", "-", "-", "-"]),
        ("staged", ["11", "22", "-", "-"]),
        ("prepared", ["11", "22", "33", "44"]),
    ] {
        let body = intent_body(
            phase, MODE, OID, "a/b", &stage, ids[0], ids[1], ids[2], ids[3],
        );
        let file = dir.path().join(phase);
        std::fs::write(&file, body).expect("fixture");
        let got = entry::entry_intent(&file, MODE, OID, "a/b", dir.path(), NONCE, &source_root())
            .expect("valid");
        assert_eq!(got.phase, phase);
        assert_eq!(got.stage, stage);
    }
}

#[test]
fn entry_intent_rejects_mismatch() {
    let dir = home("intent-mismatch");
    let stage = stage_rel(dir.path(), "a/b");
    for (mode, oid, recorded_path, recorded_stage) in [
        ("100755", OID, "a/b", stage.as_str()),
        (MODE, "ffff", "a/b", stage.as_str()),
        (MODE, OID, "a/c", stage.as_str()),
        (MODE, OID, "a/b", "wrong-stage"),
    ] {
        let body = intent_body(
            "pending",
            mode,
            oid,
            recorded_path,
            recorded_stage,
            "-",
            "-",
            "-",
            "-",
        );
        let file = dir.path().join(git_digest(body.as_slice()));
        std::fs::write(&file, body).expect("fixture");
        assert!(
            entry::entry_intent(&file, MODE, OID, "a/b", dir.path(), NONCE, &source_root())
                .is_err()
        );
    }
}

#[test]
fn entry_intent_rejects_phases_and_shapes() {
    let dir = home("intent-shapes");
    let stage = stage_rel(dir.path(), "a/b");
    let mut bodies = vec![
        intent_body("unknown", MODE, OID, "a/b", &stage, "-", "-", "-", "-"),
        intent_body("prepared", MODE, OID, "a/b", &stage, "-", "-", "-", "-"),
        intent_body("staged", MODE, OID, "a/b", &stage, "11", "22", "33", "44"),
        intent_body("pending", MODE, OID, "a/b", &stage, "11", "22", "-", "-"),
    ];
    let mut short = intent_body("pending", MODE, OID, "a/b", &stage, "-", "-", "-", "-");
    short.truncate(short.len() - 2);
    bodies.push(short);
    let mut extra = intent_body("pending", MODE, OID, "a/b", &stage, "-", "-", "-", "-");
    extra.extend_from_slice(b"extra");
    bodies.push(extra);
    for (index, body) in bodies.iter().enumerate() {
        let file = dir.path().join(index.to_string());
        std::fs::write(&file, body).expect("fixture");
        assert!(
            entry::entry_intent(&file, MODE, OID, "a/b", dir.path(), NONCE, &source_root())
                .is_err(),
            "shape {index}"
        );
    }
}

#[test]
fn entry_intent_missing_and_directory() {
    let dir = home("intent-kinds");
    assert!(
        entry::entry_intent(
            &dir.path().join("missing"),
            MODE,
            OID,
            "a/b",
            dir.path(),
            NONCE,
            &source_root()
        )
        .is_err()
    );
    let folder = dir.path().join("folder");
    std::fs::create_dir(&folder).expect("folder");
    assert!(
        entry::entry_intent(&folder, MODE, OID, "a/b", dir.path(), NONCE, &source_root()).is_err()
    );
}

#[test]
fn entry_intent_trailing_bytes() {
    let dir = home("intent-trailing");
    let stage = stage_rel(dir.path(), "top");
    for suffix in [b"\t".as_slice(), b"\n\n"] {
        let mut body = intent_body("pending", MODE, OID, "top", &stage, "-", "-", "-", "-");
        body.pop();
        body.extend_from_slice(suffix);
        let file = dir.path().join(git_digest(&body));
        std::fs::write(&file, body).expect("fixture");
        assert!(
            entry::entry_intent(&file, MODE, OID, "top", dir.path(), NONCE, &source_root()).is_ok()
        );
    }
    let mut bad = intent_body("pending", MODE, OID, "top", &stage, "-", "-", "-", "-");
    bad.extend_from_slice(b"second\n");
    let file = dir.path().join("bad");
    std::fs::write(&file, bad).expect("fixture");
    assert!(
        entry::entry_intent(&file, MODE, OID, "top", dir.path(), NONCE, &source_root()).is_err()
    );
}

fn claim_body(kind: &str, nonce: &str, path: &str) -> Vec<u8> {
    format!(
        "{}\nkind={kind}\nnonce={nonce}\npath={path}\n",
        entry::STAGE_CLAIM_HEADER
    )
    .into_bytes()
}
fn seed_claim(stage: &Path, body: &[u8]) {
    std::fs::create_dir_all(stage).expect("stage");
    let marker = entry::stage_claim_file(stage);
    std::fs::write(&marker, body).expect("marker");
    chmod(&marker, 0o600);
}

#[test]
fn claim_file_shapes() {
    for raw in ["stage", "stage/"] {
        let stage = PathBuf::from(raw);
        let got = entry::stage_claim_file(&stage);
        assert_eq!(
            got.as_os_str().as_bytes(),
            format!("{raw}/{}", entry::STAGE_CLAIM_NAME).as_bytes()
        );
    }
}

#[test]
fn claim_matches_accepts() {
    let dir = home("claim-ok");
    for kind in ["entry", "parent"] {
        let stage = dir.path().join(kind);
        seed_claim(&stage, &claim_body(kind, NONCE, "a/b"));
        assert!(entry::stage_claim_matches(
            &stage,
            kind,
            "a/b",
            NONCE,
            &source_root()
        ));
    }
}

#[test]
fn claim_matches_rejects_shape() {
    let dir = home("claim-shape");
    for (kind, path) in [
        ("bad", "a/b"),
        ("entry", "/abs"),
        ("entry", "a/../b"),
        ("entry", "a\tb"),
    ] {
        let stage = dir
            .path()
            .join(git_digest(format!("{kind}{path}").as_bytes()));
        seed_claim(&stage, &claim_body(kind, NONCE, path));
        assert!(!entry::stage_claim_matches(
            &stage,
            kind,
            path,
            NONCE,
            &source_root()
        ));
    }
}

#[test]
fn claim_matches_rejects_marker() {
    let dir = home("claim-marker");
    let stage = dir.path().join("stage");
    std::fs::create_dir(&stage).expect("stage");
    assert!(!entry::stage_claim_matches(
        &stage,
        "entry",
        "a/b",
        NONCE,
        &source_root()
    ));
    seed_claim(&stage, &claim_body("entry", "wrong", "a/b"));
    assert!(!entry::stage_claim_matches(
        &stage,
        "entry",
        "a/b",
        NONCE,
        &source_root()
    ));
    chmod(&entry::stage_claim_file(&stage), 0o640);
    assert!(!entry::stage_claim_matches(
        &stage,
        "entry",
        "a/b",
        NONCE,
        &source_root()
    ));

    let linked = dir.path().join("linked");
    seed_claim(&linked, &claim_body("entry", NONCE, "a/b"));
    std::fs::hard_link(entry::stage_claim_file(&linked), linked.join("alias"))
        .expect("hard-link marker");
    assert!(!entry::stage_claim_matches(
        &linked,
        "entry",
        "a/b",
        NONCE,
        &source_root()
    ));

    let symlinked = dir.path().join("symlinked");
    std::fs::create_dir(&symlinked).expect("stage");
    let target = symlinked.join("target");
    std::fs::write(&target, claim_body("entry", NONCE, "a/b")).expect("target marker");
    chmod(&target, 0o600);
    std::os::unix::fs::symlink(&target, entry::stage_claim_file(&symlinked))
        .expect("symlink marker");
    assert!(!entry::stage_claim_matches(
        &symlinked,
        "entry",
        "a/b",
        NONCE,
        &source_root()
    ));
}

#[test]
fn claim_write_publishes() {
    let dir = home("claim-write");
    let stage = dir.path().join("stage");
    std::fs::create_dir(&stage).expect("stage");
    entry::stage_claim_write(
        &stage,
        "entry",
        "a/b",
        NONCE,
        &source_root(),
        &mut MoveCache::default(),
    )
    .expect("write");
    let marker = entry::stage_claim_file(&stage);
    assert_eq!(
        std::fs::read(&marker).expect("bytes"),
        claim_body("entry", NONCE, "a/b")
    );
    assert_eq!(mode_of(&marker), 0o600);
}

#[test]
fn claim_write_rejects_existing_and_bad_kind() {
    let dir = home("claim-write-bad");
    let lived = dir.path().join("lived");
    seed_claim(&lived, &claim_body("entry", NONCE, "a/b"));
    assert!(
        entry::stage_claim_write(
            &lived,
            "entry",
            "a/b",
            NONCE,
            &source_root(),
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(tmp_residue(&lived), 0);
    let bad = dir.path().join("bad");
    std::fs::create_dir(&bad).expect("stage");
    assert!(
        entry::stage_claim_write(
            &bad,
            "bogus",
            "a/b",
            NONCE,
            &source_root(),
            &mut MoveCache::default()
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(entry::stage_claim_file(&bad)).expect("residue"),
        claim_body("bogus", NONCE, "a/b")
    );
}

#[test]
fn claim_only_matrix() {
    let dir = home("claim-only");
    let only = dir.path().join("only");
    seed_claim(&only, &claim_body("entry", NONCE, "a/b"));
    assert!(entry::stage_claim_only(&only));
    for name in ["empty", "extra", "missing"] {
        let stage = dir.path().join(name);
        if name != "missing" {
            std::fs::create_dir(&stage).expect("stage");
        }
        if name == "extra" {
            seed_claim(&stage, &claim_body("entry", NONCE, "a/b"));
            std::fs::write(stage.join("extra"), b"x").expect("extra");
        }
        assert!(!entry::stage_claim_only(&stage));
    }
}

#[test]
fn claim_remove_lifecycle() {
    let dir = home("claim-remove");
    let stage = dir.path().join("stage");
    seed_claim(&stage, &claim_body("entry", NONCE, "a/b"));
    assert!(entry::stage_claim_remove(&stage, "entry", "wrong", NONCE, &source_root()).is_err());
    assert!(entry::stage_claim_file(&stage).exists());
    entry::stage_claim_remove(&stage, "entry", "a/b", NONCE, &source_root()).expect("remove");
    assert!(!entry::stage_claim_file(&stage).exists());
}

#[test]
fn claim_interop() {
    let dir = home("claim-roundtrip");
    let stage = dir.path().join("stage");
    std::fs::create_dir(&stage).expect("stage");
    entry::stage_claim_write(
        &stage,
        "entry",
        "shared",
        NONCE,
        &source_root(),
        &mut MoveCache::default(),
    )
    .expect("write");
    assert!(entry::stage_claim_matches(
        &stage,
        "entry",
        "shared",
        NONCE,
        &source_root()
    ));
    entry::stage_claim_remove(&stage, "entry", "shared", NONCE, &source_root()).expect("remove");
    assert!(!entry::stage_claim_file(&stage).exists());
}

fn write_rel(home: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = home.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// lstat mode bits (never follows symlinks): the publication
/// chapter's `mode_of`, kept under a distinct name because the
/// staging chapter's `mode_of` follows symlinks
/// (`std::fs::metadata`) while this one reads the link itself
/// (`std::fs::symlink_metadata`). Both stay so neither suite
/// changes behavior.
fn mode_of_nofollow(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("stat fixture")
        .permissions()
        .mode()
        & 0o7777
}

/// `stat -c '%d:%i'` identity string of one path.
fn identity_of(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).expect("stat fixture");
    format!("{}:{}", meta.dev(), meta.ino())
}

/// One `entry_stage_valid` native contract fixture.
fn valid_row(tag: &str, build: &dyn Fn(&Path), identity: Option<&str>, want: bool) {
    let dir = TempDir::new(tag).expect("temp dir");
    build(dir.path());
    let expected = match identity {
        Some("live") => Some(identity_of(&dir.path().join("stage"))),
        Some(fixed) => Some(fixed.to_string()),
        None => None,
    };
    let rust = entry::entry_stage_valid(&dir.path().join("stage"), expected.as_deref());
    assert_eq!(rust, want, "entry_stage_valid contract on {tag}");
}

#[test]
fn stage_valid_missing_is_false() {
    valid_row("valid-missing", &|_| {}, None, false);
}

#[test]
fn stage_valid_regular_file_is_false() {
    valid_row(
        "valid-file",
        &|home| {
            write_rel(home, "stage", b"not a dir");
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_symlink_to_dir_is_false() {
    valid_row(
        "valid-link",
        &|home| {
            let target = home.join("target");
            std::fs::create_dir_all(&target).expect("fixture dir");
            chmod(&target, 0o700);
            std::os::unix::fs::symlink(&target, home.join("stage")).expect("fixture link");
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_dangling_symlink_is_false() {
    valid_row(
        "valid-dangling",
        &|home| {
            std::os::unix::fs::symlink("nowhere", home.join("stage")).expect("fixture link");
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_owned_700_is_true() {
    valid_row(
        "valid-700",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        None,
        true,
    );
}

#[test]
fn stage_valid_755_is_false() {
    valid_row(
        "valid-755",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o755);
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_777_is_false() {
    valid_row(
        "valid-777",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o777);
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_750_is_false() {
    valid_row(
        "valid-750",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o750);
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_600_passes_the_mask() {
    // `0600 & 077 == 0`: the gate masks bits, it does not compare
    // against `0700`.
    valid_row(
        "valid-600",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o600);
        },
        None,
        true,
    );
}

#[test]
fn stage_valid_setuid_only_passes_the_mask() {
    // `04700 & 077 == 0`: setuid alone does not fail the gate.
    valid_row(
        "valid-setuid",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o4700);
        },
        None,
        true,
    );
}

#[test]
fn stage_valid_setuid_with_group_fails_the_mask() {
    valid_row(
        "valid-setuid-group",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o4750);
        },
        None,
        false,
    );
}

#[test]
fn stage_valid_matching_identity_is_true() {
    valid_row(
        "valid-id-match",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        Some("live"),
        true,
    );
}

#[test]
fn stage_valid_wrong_identity_is_false() {
    valid_row(
        "valid-id-wrong",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        Some("0:0"),
        false,
    );
}

#[test]
fn stage_valid_empty_identity_skips_the_check() {
    valid_row(
        "valid-id-empty",
        &|home| {
            let stage = home.join("stage");
            std::fs::create_dir_all(&stage).expect("fixture dir");
            chmod(&stage, 0o700);
        },
        Some(""),
        true,
    );
}

#[test]
fn stage_valid_fifo_is_false() {
    valid_row(
        "valid-fifo",
        &|home| {
            let status = Command::new("mkfifo")
                .arg(home.join("stage"))
                .status()
                .expect("spawn mkfifo");
            assert!(status.success(), "mkfifo fixture");
        },
        None,
        false,
    );
}

/// One `entry_stage_only_next` native contract fixture.
fn only_next_row(tag: &str, want: bool, build: &dyn Fn(&Path)) {
    let dir = TempDir::new(tag).expect("temp dir");
    build(dir.path());
    let rust = entry::entry_stage_only_next(&dir.path().join("stage"));
    assert_eq!(rust, want, "entry_stage_only_next contract on {tag}");
}

#[test]
fn only_next_missing_stage_passes_vacuously() {
    // `nullglob`: a missing stage expands to nothing.
    only_next_row("only-missing", true, &|_| {});
}

#[test]
fn only_next_file_stage_passes_vacuously() {
    only_next_row("only-file", true, &|home| {
        write_rel(home, "stage", b"not a dir");
    });
}

#[test]
fn only_next_empty_dir_passes() {
    only_next_row("only-empty", true, &|home| {
        std::fs::create_dir_all(home.join("stage")).expect("fixture dir");
    });
}

#[test]
fn only_next_next_file_passes() {
    only_next_row("only-next", true, &|home| {
        write_rel(home, "stage/next", b"candidate");
    });
}

#[test]
fn only_next_claim_passes() {
    only_next_row("only-claim", true, &|home| {
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn only_next_next_plus_claim_passes() {
    only_next_row("only-both", true, &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn only_next_symlink_next_passes() {
    // The gate matches basenames, never types.
    only_next_row("only-link", true, &|home| {
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        std::os::unix::fs::symlink("anywhere", stage.join("next")).expect("fixture link");
    });
}

#[test]
fn only_next_claim_dir_passes() {
    only_next_row("only-claim-dir", true, &|home| {
        std::fs::create_dir_all(home.join("stage/.dot-init-stage-claim-v1")).expect("fixture dir");
    });
}

#[test]
fn only_next_extra_file_fails() {
    only_next_row("only-extra", false, &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/stray", b"intruder");
    });
}

#[test]
fn only_next_hidden_extra_fails() {
    // `dotglob`: hidden entries count too.
    only_next_row("only-hidden", false, &|home| {
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
        write_rel(home, "stage/.stray", b"intruder");
    });
}

#[test]
fn only_next_extra_dir_fails() {
    only_next_row("only-dir", false, &|home| {
        std::fs::create_dir_all(home.join("stage/next")).expect("fixture dir");
        std::fs::create_dir_all(home.join("stage/other")).expect("fixture dir");
    });
}

/// Raw tab fields of the intent record at `home/tx/intent`.
fn intent_fields(home: &Path) -> Vec<Vec<u8>> {
    let mut bytes = std::fs::read(home.join("tx/intent")).expect("read intent");
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
        .split(|byte| *byte == b'\t')
        .map(<[u8]>::to_vec)
        .collect()
}

/// One `discard_staged_next` native contract fixture.
fn discard_row(tag: &str, want: bool, build: &dyn Fn(&Path)) {
    let dir = TempDir::new(tag).expect("temp dir");
    build(dir.path());
    let stage = dir.path().join("stage");
    let rust = entry::discard_staged_next(&stage).is_ok();
    assert_eq!(rust, want, "discard_staged_next contract on {tag}");
    if rust {
        assert!(
            std::fs::symlink_metadata(stage.join("next")).is_err(),
            "successful discard removes next on {tag}"
        );
    }
}

#[test]
fn discard_claim_only_without_next_passes() {
    discard_row("discard-claim", true, &|home| {
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn discard_next_file_is_removed() {
    discard_row("discard-file", true, &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/.dot-init-stage-claim-v1", b"claim");
    });
}

#[test]
fn discard_dangling_symlink_is_removed() {
    discard_row("discard-dangling", true, &|home| {
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        std::os::unix::fs::symlink("nowhere", stage.join("next")).expect("fixture link");
    });
}

#[test]
fn discard_live_symlink_is_removed() {
    discard_row("discard-link", true, &|home| {
        let pointed = write_rel(home, "pointed", b"pointed-to");
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        std::os::unix::fs::symlink(&pointed, stage.join("next")).expect("fixture link");
    });
}

#[test]
fn discard_extra_file_refuses() {
    discard_row("discard-extra", false, &|home| {
        write_rel(home, "stage/next", b"candidate");
        write_rel(home, "stage/stray", b"intruder");
    });
}

#[test]
fn discard_next_dir_refuses() {
    discard_row("discard-dir", false, &|home| {
        std::fs::create_dir_all(home.join("stage/next")).expect("fixture dir");
    });
}

#[test]
fn discard_next_fifo_refuses() {
    discard_row("discard-fifo", false, &|home| {
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).expect("fixture dir");
        let status = Command::new("mkfifo")
            .arg(stage.join("next"))
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo fixture");
    });
}

#[test]
fn discard_missing_stage_passes_vacuously() {
    // The content gate passes on a missing stage and the missing
    // candidate is already discarded.
    discard_row("discard-missing", true, &|_| {});
}

/// Run `git` for fixtures with a pinned identity and no commit
/// hooks: the ambient user config (`core.hooksPath` pointing at
/// the dotfiles hook entry) must not slow down or reject
/// fixture commits. Only the shared fixture repo commits this
/// way; fixture setup and native contracts run with hooks disabled.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// Capture one git stdout line for fixtures.
fn git_line(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// One publish fixture: a git repo outside both homes plus twin
/// homes with empty transaction directories.
struct PublishWorld {
    _dir: TempDir,
    home: PathBuf,
    git_dir: String,
    commit: String,
    app_oid: String,
    run_oid: String,
    link_oid: String,
    newline_oid: String,
    empty_oid: String,
}

fn publish_world(tag: &str) -> PublishWorld {
    let dir = TempDir::new(tag).expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q"]);
    let app = write_rel(&repo, "cfg/app.conf", b"app-config-bytes\n");
    chmod(&app, 0o644);
    let run = write_rel(&repo, "run.sh", b"#!/bin/sh\necho hi\n");
    chmod(&run, 0o755);
    std::os::unix::fs::symlink("app-target", repo.join("cfg/link")).expect("link");
    std::os::unix::fs::symlink("bad\ntarget", repo.join("cfg/nl-link")).expect("link");
    let empty_oid = git_line(&repo, &["hash-object", "-w", "--stdin"]);
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{empty_oid},cfg/empty-link"),
        ],
    );
    git(&repo, &["commit", "-qm", "fixture"]);
    let commit = git_line(&repo, &["rev-parse", "HEAD"]);
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("tx")).expect("home");
    PublishWorld {
        app_oid: git_line(&repo, &["rev-parse", "HEAD:cfg/app.conf"]),
        run_oid: git_line(&repo, &["rev-parse", "HEAD:run.sh"]),
        link_oid: git_line(&repo, &["rev-parse", "HEAD:cfg/link"]),
        newline_oid: git_line(&repo, &["rev-parse", "HEAD:cfg/nl-link"]),
        git_dir: repo.join(".git").to_string_lossy().into_owned(),
        _dir: dir,
        home,
        commit,
        empty_oid,
    }
}

fn seed_pending(home: &Path, mode: &str, oid: &str, path: &str) {
    let stage = entry::entry_stage(home, path, PUBLISH_NONCE, &source_root()).expect("stage");
    let rel = stage
        .strip_prefix(home)
        .expect("relative")
        .to_string_lossy();
    let line = format!("pending\t{mode}\t{oid}\t{path}\t{rel}\t-\t-\t-\t-");
    entry::write_private_line(
        &home.join("tx/intent"),
        &line,
        false,
        &mut MoveCache::default(),
    )
    .expect("intent");
}

fn seed_staged(home: &Path, mode: &str, oid: &str, path: &str) {
    let target = home.join(path);
    std::fs::create_dir_all(target.parent().expect("parent")).expect("parents");
    let stage = entry::entry_stage(home, path, PUBLISH_NONCE, &source_root()).expect("stage");
    std::fs::create_dir(&stage).expect("stage");
    chmod(&stage, 0o700);
    entry::stage_claim_write(
        &stage,
        "entry",
        path,
        PUBLISH_NONCE,
        &source_root(),
        &mut MoveCache::default(),
    )
    .expect("claim");
    let id = dot::temp::path_identity(&stage).expect("identity");
    let rel = stage
        .strip_prefix(home)
        .expect("relative")
        .to_string_lossy();
    let line = format!(
        "staged\t{mode}\t{oid}\t{path}\t{rel}\t{}\t{}\t-\t-",
        id.0, id.1
    );
    entry::write_private_line(
        &home.join("tx/intent"),
        &line,
        true,
        &mut MoveCache::default(),
    )
    .expect("intent");
}

fn publish_at(world: &PublishWorld, commit: &str, mode: &str, oid: &str, path: &str) -> bool {
    let home = &world.home;
    let tx = home.join("tx");
    let intent = tx.join("intent");
    let ensure = |_: &Path, entry_path: &str| {
        if let Some(parent) = home.join(entry_path).parent() {
            std::fs::create_dir_all(parent).map_err(|source| dot::errors::Error::Io {
                context: "create test parents",
                source,
            })?;
        }
        Ok(())
    };
    let read = |file: &Path, m: &str, o: &str, p: &str| {
        entry::entry_intent(file, m, o, p, home, PUBLISH_NONCE, &source_root())
    };
    let matches = |stage: &Path, p: &str| {
        entry::stage_claim_matches(stage, "entry", p, PUBLISH_NONCE, &source_root())
    };
    let claim_write = |stage: &Path, p: &str| {
        entry::stage_claim_write(
            stage,
            "entry",
            p,
            PUBLISH_NONCE,
            &source_root(),
            &mut MoveCache::default(),
        )
    };
    let claim_remove = |stage: &Path, p: &str| {
        entry::stage_claim_remove(stage, "entry", p, PUBLISH_NONCE, &source_root())
    };
    let write_line = |file: &Path, line: &str, replace: bool| {
        entry::write_private_line(file, line, replace, &mut MoveCache::default())
    };
    let candidate = |git_dir: &str, rev: &str, m: &str, expected_oid: &str, p: &str| {
        dot::init_client_delete::candidate_matches_git(
            Path::new(git_dir),
            rev,
            m,
            expected_oid,
            p,
            home,
        )
    };
    let inputs = entry::PublishOneInputs {
        home,
        transaction: &tx,
        intent: &intent,
        git_dir: &world.git_dir,
        commit,
        mode,
        oid,
        path,
        mask: dot::temp::read_umask().expect("umask"),
        ensure_parents: &ensure,
        read_intent: &read,
        claim_matches: &matches,
        claim_write: &claim_write,
        claim_remove: &claim_remove,
        write_line: &write_line,
        candidate_matches: &candidate,
    };
    entry::publish_one(&inputs, &mut MoveCache::default()).is_ok()
}

fn publish(world: &PublishWorld, mode: &str, oid: &str, path: &str) -> bool {
    publish_at(world, &world.commit, mode, oid, path)
}

fn stage_leftovers(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for item in std::fs::read_dir(dir).expect("read") {
            let item = item.expect("entry");
            let path = item.path();
            if item
                .file_name()
                .as_os_str()
                .as_bytes()
                .starts_with(b".dot-init-entry.")
            {
                out.push(path.clone());
            }
            if path.symlink_metadata().expect("stat").is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

fn assert_target_bound(home: &Path, path: &str) {
    let fields = intent_fields(home);
    assert_eq!(fields[0], b"prepared");
    let id = dot::temp::path_identity(&home.join(path)).expect("target id");
    assert_eq!(
        format!(
            "{}:{}",
            String::from_utf8_lossy(&fields[7]),
            String::from_utf8_lossy(&fields[8])
        ),
        dot::temp::identity_string(id)
    );
}
fn assert_stage_bound(home: &Path) {
    let fields = intent_fields(home);
    assert_eq!(fields[0], b"staged");
    assert_eq!((&fields[7], &fields[8]), (&b"-".to_vec(), &b"-".to_vec()));
    let stages = stage_leftovers(home);
    assert_eq!(stages.len(), 1);
    assert_eq!(
        format!(
            "{}:{}",
            String::from_utf8_lossy(&fields[5]),
            String::from_utf8_lossy(&fields[6])
        ),
        identity_of(&stages[0])
    );
}

#[test]
fn publish_regular_file_nested() {
    let world = publish_world("publish-regular");
    seed_pending(&world.home, "100644", &world.app_oid, "cfg/app.conf");
    assert!(publish(&world, "100644", &world.app_oid, "cfg/app.conf"));
    assert_eq!(
        std::fs::read(world.home.join("cfg/app.conf")).expect("target"),
        b"app-config-bytes\n"
    );
    assert_eq!(
        mode_of_nofollow(&world.home.join("cfg/app.conf")) & 0o111,
        0
    );
    assert!(stage_leftovers(&world.home).is_empty());
    assert_target_bound(&world.home, "cfg/app.conf");
}

#[test]
fn publish_executable_top_level() {
    let world = publish_world("publish-exec");
    seed_pending(&world.home, "100755", &world.run_oid, "run.sh");
    assert!(publish(&world, "100755", &world.run_oid, "run.sh"));
    assert_eq!(
        std::fs::read(world.home.join("run.sh")).expect("target"),
        b"#!/bin/sh\necho hi\n"
    );
    assert_ne!(mode_of_nofollow(&world.home.join("run.sh")) & 0o111, 0);
    assert_target_bound(&world.home, "run.sh");
}

#[test]
fn publish_symlink_candidate_check_refuses() {
    let world = publish_world("publish-link");
    seed_pending(&world.home, "120000", &world.link_oid, "cfg/link");
    assert!(!publish(&world, "120000", &world.link_oid, "cfg/link"));
    assert_stage_bound(&world.home);
    assert_eq!(
        std::fs::read_link(stage_leftovers(&world.home)[0].join("next")).expect("link"),
        PathBuf::from("app-target")
    );
}

#[test]
fn publish_from_staged_intent() {
    let world = publish_world("publish-staged");
    seed_staged(&world.home, "100644", &world.app_oid, "cfg/app.conf");
    assert!(publish(&world, "100644", &world.app_oid, "cfg/app.conf"));
    assert_eq!(
        std::fs::read(world.home.join("cfg/app.conf")).expect("target"),
        b"app-config-bytes\n"
    );
    assert_target_bound(&world.home, "cfg/app.conf");
}

#[test]
fn publish_rejects_intent_mismatch() {
    let world = publish_world("publish-intent-mismatch");
    seed_pending(&world.home, "100644", &world.app_oid, "cfg/app.conf");
    let bogus = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    assert!(!publish(&world, "100644", bogus, "cfg/app.conf"));
    assert_eq!(intent_fields(&world.home)[0], b"pending");
    assert!(stage_leftovers(&world.home).is_empty());
}

#[test]
fn publish_rejects_blob_mismatch() {
    let world = publish_world("publish-blob-mismatch");
    let bogus = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    seed_pending(&world.home, "100644", bogus, "cfg/app.conf");
    assert!(!publish(&world, "100644", bogus, "cfg/app.conf"));
    assert_stage_bound(&world.home);
    assert_eq!(
        std::fs::read(stage_leftovers(&world.home)[0].join("next")).expect("next"),
        b"app-config-bytes\n"
    );
}

#[test]
fn publish_failed_blob_leaves_partial_next() {
    let world = publish_world("publish-missing-commit");
    seed_pending(&world.home, "100644", &world.app_oid, "cfg/app.conf");
    assert!(!publish_at(
        &world,
        &"0".repeat(40),
        "100644",
        &world.app_oid,
        "cfg/app.conf"
    ));
    assert_stage_bound(&world.home);
    assert_eq!(
        std::fs::read(stage_leftovers(&world.home)[0].join("next")).expect("next"),
        b""
    );
}

#[test]
fn publish_rejects_unsupported_mode() {
    let world = publish_world("publish-mode");
    seed_pending(&world.home, "100666", &world.app_oid, "cfg/app.conf");
    assert!(!publish(&world, "100666", &world.app_oid, "cfg/app.conf"));
    assert_stage_bound(&world.home);
    assert!(!stage_leftovers(&world.home)[0].join("next").exists());
}

#[test]
fn publish_rejects_newline_link_target() {
    let world = publish_world("publish-newline");
    seed_pending(&world.home, "120000", &world.newline_oid, "cfg/nl-link");
    assert!(!publish(
        &world,
        "120000",
        &world.newline_oid,
        "cfg/nl-link"
    ));
    assert_stage_bound(&world.home);
    assert!(!stage_leftovers(&world.home)[0].join("next").exists());
}

#[test]
fn publish_rejects_empty_link_target() {
    let world = publish_world("publish-empty");
    seed_pending(&world.home, "120000", &world.empty_oid, "cfg/empty-link");
    assert!(!publish(
        &world,
        "120000",
        &world.empty_oid,
        "cfg/empty-link"
    ));
    assert_stage_bound(&world.home);
    assert!(!stage_leftovers(&world.home)[0].join("next").exists());
}

#[test]
fn publish_prepared_rerun_refuses() {
    let world = publish_world("publish-rerun");
    seed_pending(&world.home, "100644", &world.app_oid, "cfg/app.conf");
    assert!(publish(&world, "100644", &world.app_oid, "cfg/app.conf"));
    let before = std::fs::read(world.home.join("cfg/app.conf")).expect("target");
    assert!(!publish(&world, "100644", &world.app_oid, "cfg/app.conf"));
    assert_eq!(
        std::fs::read(world.home.join("cfg/app.conf")).expect("target"),
        before
    );
    assert_target_bound(&world.home, "cfg/app.conf");
}
