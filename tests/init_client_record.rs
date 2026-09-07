//! Native contracts for initialization transaction records.
use dot::init_client_record::{self as record, RecordFields};
use dot::temp::MoveCache;
use dot_test_support::TempDir;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
fn source() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}
struct Fix {
    _root: TempDir,
    home: PathBuf,
    file: PathBuf,
}
impl Fix {
    fn new(tag: &str) -> Self {
        let root = TempDir::new(tag).unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let file = root.path().join("record");
        Self {
            _root: root,
            home,
            file,
        }
    }
    fn fields(&self) -> RecordFields<'_> {
        RecordFields {
            origin: "https://example.invalid/repo.git",
            identity: "example.invalid/repo",
            branch: "main",
            backup: "-",
            git_dir: None,
            commit: Some(COMMIT),
            nonce: Some("n1"),
            git_dev: Some("-"),
            git_ino: Some("-"),
            dot_bin: "/usr/bin/dot",
            home: &self.home,
            source_root: source(),
        }
    }
    fn write(&self, phase: &str) {
        let mut c = MoveCache::default();
        record::write_record(&self.file, phase, &self.fields(), &mut c).unwrap();
    }
}
fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}
fn replace(file: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(file).unwrap();
    std::fs::write(file, text.replacen(from, to, 1)).unwrap();
}
fn parent_file(tx: &Path, rel: &str) -> PathBuf {
    let h = dot::temp::file_text_digest(source(), rel.as_bytes()).unwrap();
    tx.join(format!("parent-intent.{h}"))
}
fn parent_stage(_home: &Path, rel: &str, nonce: &str) -> String {
    let h = dot::temp::file_text_digest(source(), rel.as_bytes()).unwrap();
    let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    if dir.is_empty() {
        format!(".dot-init-parent.{nonce}.{h}")
    } else {
        format!("{dir}/.dot-init-parent.{nonce}.{h}")
    }
}
#[test]
fn write_record_creates_journal() {
    let f = Fix::new("record-create");
    f.write("prepared");
    assert_eq!(mode(&f.file), 0o600);
    assert_eq!(
        record::read_record(&f.file, &f.home).unwrap().phase,
        "prepared"
    );
}
#[test]
fn write_record_defaults() {
    let f = Fix::new("record-defaults");
    let mut fields = f.fields();
    fields.commit = None;
    fields.nonce = None;
    fields.git_dev = None;
    fields.git_ino = None;
    let mut c = MoveCache::default();
    record::write_record(&f.file, "prepared", &fields, &mut c).unwrap();
    let r = record::read_record(&f.file, &f.home).unwrap();
    assert_eq!(r.commit, "0".repeat(40));
    assert_eq!(r.nonce, "legacy");
    assert_eq!((r.git_dev.as_str(), r.git_ino.as_str()), ("-", "-"));
}
#[test]
fn write_record_explicit_git_dir() {
    let f = Fix::new("record-git-dir");
    let git = f.home.join(".git");
    let mut fields = f.fields();
    fields.git_dir = Some(&git);
    let mut c = MoveCache::default();
    record::write_record(&f.file, "prepared", &fields, &mut c).unwrap();
    assert_eq!(
        record::read_record(&f.file, &f.home).unwrap().git_dir,
        git.to_string_lossy()
    );
}
#[test]
fn write_record_replaces_live_file() {
    let f = Fix::new("record-replace");
    std::fs::write(&f.file, b"old\n").unwrap();
    f.write("prepared");
    assert!(!std::fs::read(&f.file).unwrap().starts_with(b"old"));
}
#[test]
fn write_record_directory_destination_fails() {
    let f = Fix::new("record-dir");
    std::fs::create_dir(&f.file).unwrap();
    let mut c = MoveCache::default();
    assert!(record::write_record(&f.file, "prepared", &f.fields(), &mut c).is_err());
    assert!(f.file.is_dir());
}
#[test]
fn write_record_missing_source_revision_fails() {
    let f = Fix::new("record-source");
    let missing = f.home.join("missing");
    let mut fields = f.fields();
    fields.source_root = &missing;
    let mut c = MoveCache::default();
    assert!(record::write_record(&f.file, "prepared", &fields, &mut c).is_err());
    assert!(!f.file.exists());
}
#[test]
fn write_record_umask_077_stays_600() {
    if std::env::var_os("DOT_RECORD_UMASK_CHILD").is_none() {
        let status = Command::new("sh")
            .args([
                "-c",
                "umask 077; exec \"$1\" --exact write_record_umask_077_stays_600",
                "record-umask",
            ])
            .arg(std::env::current_exe().unwrap())
            .env("DOT_RECORD_UMASK_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let f = Fix::new("record-umask");
    f.write("prepared");
    assert_eq!(mode(&f.file), 0o600);
}
#[test]
fn read_record_round_trip() {
    let f = Fix::new("record-roundtrip");
    f.write("prepared");
    let r = record::read_record(&f.file, &f.home).unwrap();
    assert_eq!(
        (r.origin.as_str(), r.branch.as_str(), r.commit.as_str()),
        ("https://example.invalid/repo.git", "main", COMMIT)
    );
}
#[test]
fn read_record_all_phases() {
    for phase in [
        "prepared",
        "backing-up",
        "publishing",
        "checkout",
        "complete",
    ] {
        let f = Fix::new(phase);
        f.write(phase);
        assert_eq!(record::read_record(&f.file, &f.home).unwrap().phase, phase);
    }
}
#[test]
fn read_record_rejects_malformed_shapes() {
    for change in [
        ("cgraf78 dot initialization transaction v1", "bad header"),
        ("origin=", "unknown=x\norigin="),
    ] {
        let f = Fix::new("record-shape");
        f.write("prepared");
        replace(&f.file, change.0, change.1);
        assert!(record::read_record(&f.file, &f.home).is_err());
    }
}
#[test]
fn read_record_rejects_bad_files() {
    let f = Fix::new("record-files");
    f.write("prepared");
    std::fs::set_permissions(&f.file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(record::read_record(&f.file, &f.home).is_err());
    let link = f.home.join("link");
    symlink(&f.file, &link).unwrap();
    assert!(record::read_record(&link, &f.home).is_err());
    assert!(record::read_record(&f.home.join("missing"), &f.home).is_err());
}
#[test]
fn read_record_rejects_bad_semantics() {
    for (from, to) in [
        ("phase=prepared", "phase=unknown"),
        ("branch=main", "branch=bad..name"),
        ("nonce=n1", "nonce=bad nonce"),
        ("git_dev=-", "git_dev=1"),
    ] {
        let f = Fix::new("record-semantics");
        f.write("prepared");
        replace(&f.file, from, to);
        assert!(record::read_record(&f.file, &f.home).is_err(), "{to}");
    }
}
#[test]
fn read_record_long_commit_and_git_dir() {
    let f = Fix::new("record-long");
    let git = f.home.join(".git");
    let long = "a".repeat(64);
    let mut fields = f.fields();
    fields.commit = Some(&long);
    fields.git_dir = Some(&git);
    let mut c = MoveCache::default();
    record::write_record(&f.file, "prepared", &fields, &mut c).unwrap();
    let r = record::read_record(&f.file, &f.home).unwrap();
    assert_eq!(r.commit, long);
    assert_eq!(r.git_dir, git.to_string_lossy());
}
#[test]
fn read_record_size_gate() {
    let f = Fix::new("record-size");
    f.write("prepared");
    let mut bytes = std::fs::read(&f.file).unwrap();
    bytes.resize(16385, b'x');
    std::fs::write(&f.file, bytes).unwrap();
    assert!(record::read_record(&f.file, &f.home).is_err());
}
#[test]
fn read_record_newline_edges() {
    let f = Fix::new("record-newline");
    f.write("prepared");
    let mut bytes = std::fs::read(&f.file).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    std::fs::write(&f.file, bytes).unwrap();
    assert!(record::read_record(&f.file, &f.home).is_ok());
}
#[test]
fn record_phase_advances() {
    let f = Fix::new("record-phase");
    f.write("prepared");
    let mut c = MoveCache::default();
    record::record_phase(&f.file, "backing-up", &f.fields(), &mut c).unwrap();
    assert_eq!(
        record::read_record(&f.file, &f.home).unwrap().phase,
        "backing-up"
    );
}
#[test]
fn record_phase_missing_parent_fails() {
    let f = Fix::new("record-phase-parent");
    std::fs::write(f.home.join("blocked"), b"x").unwrap();
    let file = f.home.join("blocked/record");
    let mut c = MoveCache::default();
    assert!(record::record_phase(&file, "backing-up", &f.fields(), &mut c).is_err());
}
#[test]
fn parent_record_pending_and_prepared() {
    for (phase, dev, ino, mode) in [("pending", "-", "-", "-"), ("prepared", "1", "2", "700")] {
        let f = Fix::new(phase);
        let tx = f.home.join("tx");
        std::fs::create_dir(&tx).unwrap();
        let rel = "a/b";
        let stage = parent_stage(&f.home, rel, "n1");
        std::fs::write(
            parent_file(&tx, rel),
            format!("{phase}\t{rel}\t{stage}\t{dev}\t{ino}\t{mode}\n"),
        )
        .unwrap();
        let got = record::parent_record(&tx, rel, &f.home, "n1", source()).unwrap();
        assert_eq!((got.phase.as_str(), got.dev.as_str()), (phase, dev));
    }
}
#[test]
fn parent_record_rejects() {
    for line in [
        "other\ta\tx\t-\t-\t-\n",
        "pending\tb\tx\t-\t-\t-\n",
        "pending\ta\tx\t1\t-\t-\n",
    ] {
        let f = Fix::new("parent-reject");
        let tx = f.home.join("tx");
        std::fs::create_dir(&tx).unwrap();
        std::fs::write(parent_file(&tx, "a"), line).unwrap();
        assert!(record::parent_record(&tx, "a", &f.home, "n1", source()).is_err());
    }
}
#[test]
fn parent_record_missing_intent_fails() {
    let f = Fix::new("parent-missing");
    assert!(record::parent_record(&f.home, "a", &f.home, "n1", source()).is_err());
}
#[test]
fn prior_record_first_match_wins() {
    let f = Fix::new("prior-first");
    std::fs::write(
        &f.file,
        b"dot\tregular\t1\t2\t644\t3\tone\ndot\tregular\t4\t5\t600\t6\ttwo\n",
    )
    .unwrap();
    let got = record::prior_record(&f.file, "dot").unwrap();
    assert_eq!(got.value, "one");
}
#[test]
fn prior_record_miss_and_edges() {
    let f = Fix::new("prior-edges");
    std::fs::write(&f.file, b"other\tabsent\t-\t-\t-\t-\t-\n").unwrap();
    assert!(record::prior_record(&f.file, "dot").is_err());
    std::fs::write(&f.file, b"dot\tregular\t1\t2\t644\t3\tvalue\textra\n").unwrap();
    assert_eq!(
        record::prior_record(&f.file, "dot").unwrap().value,
        "value\textra"
    );
}
