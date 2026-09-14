//! Contracts for shared integration-test isolation helpers.

use std::sync::atomic::AtomicU64;

use dot_test_support::TempDir;

#[test]
fn temp_dir_skips_a_preexisting_process_counter_path() {
    let root = TempDir::new("collision-root").expect("scratch root");
    let counter = AtomicU64::new(0);
    let stale = root.path().join("dot-collision-42-0");
    std::fs::create_dir(&stale).expect("stale temp directory");
    std::fs::write(stale.join("sentinel"), b"owned by an earlier process\n")
        .expect("stale sentinel");

    let allocated = TempDir::new_in_with_identity(root.path(), "collision", &counter, 42)
        .expect("skip stale directory");

    assert_eq!(allocated.path(), root.path().join("dot-collision-42-1"));
    assert_eq!(
        std::fs::read(stale.join("sentinel")).expect("retained stale sentinel"),
        b"owned by an earlier process\n"
    );
}
