//! Differential parity tests for the cleanup registry against
//! `lib/dot/resources.sh`: registration validation codes, removal
//! semantics, and the observable teardown contract (dead children,
//! removed paths, idempotent cleanup).
//!
//! Mechanism differs by construction (Child handles vs PID+start-tick
//! identities, no job tables), so these compare EXIT CODES and
//! filesystem/process OUTCOMES, never internals.

use std::process::{Command, Stdio};

use dot::cleanup::{Registry, valid_group, valid_pid};

/// Absolute bash (child env may be scrubbed; see `tests/ui.rs`).
fn bash_bin() -> &'static str {
    for candidate in ["/usr/bin/bash", "/bin/bash"] {
        if std::path::Path::new(candidate).is_file() {
            return candidate;
        }
    }
    panic!("no bash interpreter found");
}

/// Run a shell cleanup snippet; returns (exit code, stdout, stderr).
/// The snippet runs with `set -u` (NOT `-e`: assertions inspect codes).
fn shell_cleanup(script_body: &str) -> (i32, String, String) {
    let script = format!("set -u\n. \"$1/lib/dot/resources.sh\"\n{script_body}\n",);
    let output = Command::new(bash_bin())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(script)
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn registration_codes_match_shell() {
    // (shell snippet, expected code). Rust outcomes computed inline.
    let cases: &[(&str, i32)] = &[
        ("_dot_cleanup_register_pid 123", 0),
        ("_dot_cleanup_register_pid 0", 2),
        ("_dot_cleanup_register_pid abc", 2),
        ("_dot_cleanup_register_pid 01", 2),
        ("_dot_cleanup_register_pid 123 123", 0),
        ("_dot_cleanup_register_pid 123 456", 2),
        ("_dot_cleanup_register_pid 123 ''", 0),
        ("_dot_cleanup_register_path /tmp/x", 0),
        ("_dot_cleanup_register_path ''", 2),
        ("_dot_cleanup_register_fd 3", 0),
        ("_dot_cleanup_register_fd x", 2),
    ];
    for (snippet, code) in cases {
        let (shell_code, _, _) = shell_cleanup(snippet);
        assert_eq!(shell_code, *code, "shell changed for {snippet:?}");
    }
    // Rust validators enforce the same rules.
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
fn both_reap_a_registered_sleep() {
    // Shell: background sleep, register its job PID, run full cleanup,
    // report whether anything with that PID survives.
    let body = "sleep 30 & child=$!\n_dot_cleanup_register_pid \"$child\"\n_dot_cleanup_all\nif kill -0 \"$child\" 2>/dev/null; then echo SURVIVED; else echo REAPED; fi\n";
    let (code, stdout, _) = shell_cleanup(body);
    assert_eq!(code, 0, "shell cleanup failed");
    assert!(
        stdout.contains("REAPED"),
        "shell left child alive: {stdout:?}"
    );
    // No `kill -0` recheck on the recorded pid here: pid-only
    // rechecks race with pid reuse (a recycled pid reads "alive"),
    // and the in-shell REAPED above already asserts the contract in
    // the tight kill-then-check window with no fork between.

    // Rust: same observable contract.
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
fn both_remove_registered_paths_and_repeat_cleanly() {
    let dir = dot::test_support::TempDir::new("cleanup-diff").expect("scratch");
    let target = dir.path().join("victim");
    std::fs::create_dir_all(&target).expect("setup");
    let body = format!(
        "path=\"{path}\"\n_dot_cleanup_register_path \"$path\"\n_dot_cleanup_remove_path \"$path\"; echo \"remove=$?\"\n_dot_cleanup_all; echo \"cleanup=$?\"\ntest -e \"$path\" && echo PRESENT || echo GONE\n",
        path = target.display(),
    );
    let (code, stdout, _) = shell_cleanup(&body);
    assert_eq!(code, 0);
    assert!(stdout.contains("remove=0"), "{stdout:?}");
    assert!(stdout.contains("cleanup=0"), "{stdout:?}");
    assert!(stdout.contains("GONE"), "{stdout:?}");

    let target = dir.path().join("victim-rs");
    std::fs::create_dir_all(&target).expect("setup");
    let mut registry = Registry::new();
    registry.register_path(&target).expect("register");
    registry.remove_path(&target).expect("remove");
    registry.cleanup();
    assert!(!target.exists());
    assert_eq!(registry.path_count(), 0);
}
