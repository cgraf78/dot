//! Native behavioral tests for cleanup validation and observable teardown.

use dot::cleanup::{Registry, valid_pid};

#[test]
fn registration_validation_rejects_ambiguous_identities_and_empty_paths() {
    for good in ["123", "9773"] {
        assert!(valid_pid(good));
    }
    for bad in ["0", "01", "abc", ""] {
        assert!(!valid_pid(bad));
    }
    // Rust registry mirrors the path rule (empty rejected).
    let mut registry = Registry::new();
    assert!(
        registry
            .register_path(std::path::Path::new("/tmp/x"))
            .is_ok()
    );
    assert!(registry.register_path(std::path::Path::new("")).is_err());
}

#[test]
fn remove_registered_paths_and_repeat_cleanly() {
    let dir = dot_test_support::TempDir::new("cleanup-diff").expect("scratch");
    let target = dir.path().join("victim");
    std::fs::create_dir_all(&target).expect("setup");
    let mut registry = Registry::new();
    registry.register_path(&target).expect("register");
    registry.remove_path(&target).expect("remove");
    std::fs::create_dir(&target).expect("replacement directory");
    std::fs::write(target.join("sentinel"), b"replacement\n").expect("replacement marker");
    registry.cleanup();
    assert_eq!(
        std::fs::read(target.join("sentinel")).unwrap(),
        b"replacement\n"
    );
    assert_eq!(registry.path_count(), 0);
}
