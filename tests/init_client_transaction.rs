//! Native contracts for initialization transaction-directory lifecycle.
use dot::init_client_transaction as txn;
use dot::temp::MoveCache;
use dot_test_support::TempDir;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
fn root(tag: &str) -> TempDir {
    TempDir::new(tag).expect("fixture root")
}
fn source() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}
fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}
fn prepared(root: &TempDir) -> (PathBuf, PathBuf) {
    let transaction = root.path().join("state/dot/init/transaction");
    let stage = txn::prepare_transaction(&transaction).expect("prepare");
    (transaction, stage)
}
#[test]
fn state_root_resolution() {
    assert_eq!(
        txn::state_root("/home/u", "").unwrap(),
        "/home/u/.local/state/dot/init"
    );
    assert_eq!(
        txn::state_root("/home/u", "/state").unwrap(),
        "/state/dot/init"
    );
    assert!(txn::state_root("relative", "").is_err());
}
#[test]
fn transaction_and_completed_paths() {
    assert_eq!(
        txn::transaction_dir("/home/u", "/state").unwrap(),
        "/state/dot/init/transaction"
    );
    assert_eq!(
        txn::completed_file("/home/u", "/state").unwrap(),
        "/state/dot/init/completed"
    );
}
#[test]
fn private_directory_lifecycle() {
    let r = root("txn-private");
    let p = r.path().join("a/b");
    assert!(txn::private_directory(&p));
    assert_eq!(mode(&p), 0o700);
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(txn::private_directory(&p));
    assert_eq!(mode(&p), 0o700);
}
#[test]
fn private_directory_rejects_non_directories() {
    let r = root("txn-private-refuse");
    let f = r.path().join("file");
    std::fs::write(&f, b"x").unwrap();
    assert!(!txn::private_directory(&f));
    let target = r.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = r.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(!txn::private_directory(&link));
}
#[test]
fn prepare_creates_owned_stage() {
    let r = root("txn-prepare");
    let (t, s) = prepared(&r);
    assert!(
        s.to_string_lossy()
            .starts_with(&format!("{}.prepare.", t.display()))
    );
    assert_eq!(mode(&s), 0o700);
    assert_eq!(mode(&s.join(txn::PREPARATION_MARKER_NAME)), 0o600);
    assert!(txn::transaction_stage_owned(source(), &s));
}
#[test]
fn prepare_fails_when_parent_unmakable() {
    let r = root("txn-prepare-refuse");
    let p = r.path().join("parent");
    std::fs::write(&p, b"x").unwrap();
    assert!(txn::prepare_transaction(&p.join("transaction")).is_err());
}
#[test]
fn owned_gate_accepts_prepared_stage() {
    let r = root("txn-owned");
    let (_, s) = prepared(&r);
    assert!(txn::transaction_stage_owned(source(), &s));
}
#[test]
fn owned_gate_rejects_forgery() {
    let r = root("txn-forgery");
    let (_, s) = prepared(&r);
    std::fs::write(s.join(txn::PREPARATION_MARKER_NAME), b"forged\n").unwrap();
    assert!(!txn::transaction_stage_owned(source(), &s));
}
#[test]
fn recover_keeps_only_unowned() {
    let r = root("txn-recover");
    let (t, owned) = prepared(&r);
    let foreign = PathBuf::from(format!("{}.prepare.foreign", t.display()));
    std::fs::create_dir(&foreign).unwrap();
    assert!(txn::recover_transaction_stages(source(), &t));
    assert!(!owned.exists());
    assert!(foreign.exists());
}
#[test]
fn recover_missing_parent_succeeds() {
    let r = root("txn-recover-missing");
    assert!(txn::recover_transaction_stages(
        source(),
        &r.path().join("missing/transaction")
    ));
}
#[test]
fn publish_moves_stage() {
    let r = root("txn-publish");
    let (t, s) = prepared(&r);
    std::fs::write(s.join("record"), b"record\n").unwrap();
    let mut c = MoveCache::default();
    assert!(txn::publish_transaction(source(), &s, &t, &mut c));
    assert!(!s.exists());
    assert_eq!(std::fs::read(t.join("record")).unwrap(), b"record\n");
}
#[test]
fn publish_rejects_missing_record() {
    let r = root("txn-publish-missing");
    let (t, s) = prepared(&r);
    let mut c = MoveCache::default();
    assert!(!txn::publish_transaction(source(), &s, &t, &mut c));
    assert!(s.exists());
}
#[test]
fn publish_rejects_late_transaction() {
    let r = root("txn-publish-race");
    let (t, s) = prepared(&r);
    std::fs::write(s.join("record"), b"record\n").unwrap();
    std::fs::create_dir(&t).unwrap();
    let mut c = MoveCache::default();
    assert!(!txn::publish_transaction(source(), &s, &t, &mut c));
    assert!(s.exists());
}
#[test]
fn publish_rejects_forged_stage() {
    let r = root("txn-publish-forged");
    let (t, s) = prepared(&r);
    std::fs::write(s.join("record"), b"record\n").unwrap();
    std::fs::write(s.join(txn::PREPARATION_MARKER_NAME), b"forged\n").unwrap();
    let mut c = MoveCache::default();
    assert!(!txn::publish_transaction(source(), &s, &t, &mut c));
    assert!(s.exists());
}
