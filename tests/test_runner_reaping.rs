#![cfg(target_os = "linux")]

//! Linux regression for descendant adoption when the surrounding PID 1 does
//! not reap promptly, as in the minimal platform containers used by CI.
#[path = "support/test_fixture.rs"]
#[allow(dead_code)]
mod fixture;

use fixture::{Fixture, finish, poll, success};
use std::fs;

#[test]
#[cfg(target_os = "linux")]
fn native_supervisor_reaps_descendants_before_returning() {
    // Become the fallback adopter so a missing dot subreaper leaves a stable
    // zombie here instead of relying on the host init's reaping policy.
    // SAFETY: prctl has no pointer arguments for PR_SET_CHILD_SUBREAPER.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);

    let f = Fixture::new();
    f.suite(
        "descendant",
        "(trap '' TERM; echo $BASHPID >\"$HOME/descendant\"; while :; do sleep 1; done) &\n\
         until [[ -s $HOME/descendant ]]; do sleep 0.02; done\n\
         printf 'complete\\t1\\t0\\n' >\"$DOT_TEST_RESULT_FILE\"",
    );
    let child = f.command(&[]).spawn().unwrap();
    poll(|| f.home.join("descendant").is_file());
    let pid = fs::read_to_string(f.home.join("descendant")).unwrap();
    success(&finish(child));
    let process = std::path::Path::new("/proc").join(pid.trim());
    assert!(
        !process.exists(),
        "dot returned before its adopted descendant was reaped: {}",
        String::from_utf8_lossy(&fs::read(process.join("stat")).unwrap_or_default())
    );
}
