//! Native behavioral tests for cleanup validation and observable teardown.

use std::process::{Command, Stdio};

use dot::cleanup::{Registry, valid_group, valid_pid};

#[test]
fn registration_validation_rejects_ambiguous_identities_and_empty_paths() {
    for good in ["123", "9773"] {
        assert!(valid_pid(good));
    }
    for bad in ["0", "01", "abc", ""] {
        assert!(!valid_pid(bad));
    }
    assert!(valid_group("123", ""));
    assert!(valid_group("123", "123"));
    assert!(!valid_group("123", "456"));
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
fn cleanup_reaps_a_registered_sleep() {
    let child = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let mut registry = Registry::new();
    registry.track_child(child);
    registry.cleanup();
    // Handle-based proof, not pid-based: cleanup drains and reaps
    // every tracked child, and `wait` cannot mistake a recycled pid
    // for ours (a pid is only recyclable after we reap it). A
    // `kill -0` recheck here would reintroduce the race above.
    assert_eq!(registry.child_count(), 0, "cleanup leaked a child");
}

#[test]
fn unregistering_a_child_drops_signal_authority_atomically() {
    let mut child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let mut registry = Registry::new();
    let tracked = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tracked sleep");
    let tracked_pid = tracked.id();
    registry.track_child(tracked);
    assert!(registry.untrack_child(tracked_pid));
    assert!(!registry.untrack_child(tracked_pid));
    assert_eq!(registry.child_count(), 0);
    registry.cleanup();
    assert!(
        child.try_wait().unwrap().is_none(),
        "unowned control child exited"
    );
    let _ = child.kill();
    child.wait().unwrap();
    // The untracked child is deliberately no longer registry-owned. Reap it
    // explicitly without giving cleanup stale PID authority.
    unsafe {
        libc::kill(tracked_pid as i32, libc::SIGKILL);
    }
    let mut status = 0;
    unsafe {
        libc::waitpid(tracked_pid as i32, &mut status, 0);
    }
    assert_ne!(pid, tracked_pid);
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
