//! Native contracts for profile lifecycle ledger, rollback, and retirement.
use dot::log::Log;
use dot::profile_lifecycle::{self as lifecycle, WorkerOutcome, WorkerRun};
use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn file(root: &Path, name: &str, body: &[u8], mode: u32) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}
fn checkout(home: &Path, name: &str, script: bool) -> String {
    let path = home.join(format!(".dotfiles-{name}"));
    std::fs::create_dir_all(&path).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let origin = format!("file:///repo/{name}.git");
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["remote", "add", "origin", &origin])
            .status()
            .unwrap()
            .success()
    );
    if script {
        file(&path, "dot/profile-deactivate", b"#!/bin/sh\n", 0o600);
    }
    let h = home.to_string_lossy();
    format!("{name}|{h}/.dotfiles-{name}|{origin}|{h}/conf/10-{name}.conf|false|git")
}
fn log() -> Log {
    Log::new(false, false)
}

#[test]
fn ledger_safety_rejects_links_modes_hardlinks_and_size() {
    let d = TempDir::new("ledger-safe").unwrap();
    let p = file(d.path(), "ledger", b"version=1\n", 0o600);
    assert!(lifecycle::file_safe(&p, euid()));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(!lifecycle::file_safe(&p, euid()));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&p, d.path().join("hard")).unwrap();
    assert!(!lifecycle::file_safe(&p, euid()));
    let big = file(
        d.path(),
        "big",
        &vec![b'x'; lifecycle::MAX_LEDGER_BYTES as usize + 1],
        0o600,
    );
    assert!(!lifecycle::file_safe(&big, euid()));
    std::os::unix::fs::symlink(&big, d.path().join("link")).unwrap();
    assert!(!lifecycle::file_safe(&d.path().join("link"), euid()));
}

#[test]
fn load_and_write_roundtrip_order_and_reject_malformed_duplicate_unsafe() {
    let d = TempDir::new("ledger-roundtrip").unwrap();
    let home = d.path().to_string_lossy();
    let a = checkout(d.path(), "a", true);
    let b = checkout(d.path(), "b", true);
    let ledger = d.path().join("state/deep/ledger");
    assert!(lifecycle::write(&ledger, &[a.clone(), b.clone()], euid()));
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        format!("version=1\n{a}\n{b}\n")
    );
    let mut records = vec![];
    let mut warnings = vec![];
    assert!(lifecycle::load(
        Some(&ledger),
        &home,
        euid(),
        &log(),
        &mut warnings,
        &mut records
    ));
    assert_eq!(records, [a.clone(), b.clone()]);
    assert!(warnings.is_empty());
    for body in [
        b"".as_slice(),
        b"version=2\n",
        format!("version=1\n{a}\n{a}\n").as_bytes(),
        b"version=1\nbad\n",
        b"version=1\n\n",
    ] {
        std::fs::write(&ledger, body).unwrap();
        records.clear();
        warnings.clear();
        assert!(!lifecycle::load(
            Some(&ledger),
            &home,
            euid(),
            &log(),
            &mut warnings,
            &mut records
        ));
        assert!(!warnings.is_empty());
    }
}

#[test]
fn deactivation_script_requires_saved_checkout_identity_and_fixed_regular_path() {
    let d = TempDir::new("ledger-script").unwrap();
    let home = d.path().to_string_lossy();
    let good = checkout(d.path(), "web", true);
    let script = lifecycle::deactivation_script(&good, &home, euid()).unwrap();
    assert!(script.ends_with("/.dotfiles-web/dot/profile-deactivate"));
    let missing = checkout(d.path(), "missing", false);
    assert_eq!(
        lifecycle::deactivation_script(&missing, &home, euid()),
        Err(lifecycle::ScriptError::Missing)
    );
    assert_eq!(
        lifecycle::deactivation_script("bad", &home, euid()),
        Err(lifecycle::ScriptError::Refused)
    );
    assert_eq!(
        lifecycle::deactivation_script(&good.replace("web.git", "other.git"), &home, euid()),
        Err(lifecycle::ScriptError::Refused)
    );
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o620)).unwrap();
    assert_eq!(
        lifecycle::deactivation_script(&good, &home, euid()),
        Err(lifecycle::ScriptError::Refused)
    );
}

#[test]
fn prepare_and_commit_preserve_prior_on_failure_and_sort_survivors() {
    let d = TempDir::new("ledger-prepare").unwrap();
    let home = d.path().to_string_lossy();
    let a = checkout(d.path(), "a", true);
    let b = checkout(d.path(), "b", true);
    let state_dir = d.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ledger = state_dir.join("ledger");
    assert!(lifecycle::write(&ledger, std::slice::from_ref(&b), euid()));
    let logger = log();
    let mut warnings = vec![];
    let prepared = lifecycle::prepare(
        &lifecycle::PrepareInputs {
            present: true,
            extensions_enabled: true,
            eligible: &["a".into(), "b".into()],
            phase_one: std::slice::from_ref(&a),
            active: &[],
            prior: &[],
            ledger: Some(&ledger),
            home: &home,
            euid: euid(),
            log: &logger,
        },
        &mut warnings,
    );
    assert!(prepared.succeeded, "{}", String::from_utf8_lossy(&warnings));
    assert_eq!(prepared.prior.as_slice(), std::slice::from_ref(&b));
    assert_eq!(prepared.records, [a.clone(), b.clone()]);
    let failed = lifecycle::prepare(
        &lifecycle::PrepareInputs {
            present: true,
            extensions_enabled: false,
            eligible: &["a".into()],
            phase_one: &[],
            active: &[],
            prior: &prepared.records,
            ledger: Some(&ledger),
            home: &home,
            euid: euid(),
            log: &logger,
        },
        &mut warnings,
    );
    assert!(!failed.succeeded);
    assert_eq!(failed.records, prepared.records);
    assert!(lifecycle::commit(&lifecycle::CommitInputs {
        present: true,
        extensions_enabled: true,
        retained: &prepared.records,
        eligible: &["a".into()],
        active: std::slice::from_ref(&a),
        ledger: Some(&ledger),
        home: &home,
        euid: euid()
    }));
    let mut final_records = vec![];
    assert!(lifecycle::load(
        Some(&ledger),
        &home,
        euid(),
        &logger,
        &mut vec![],
        &mut final_records
    ));
    assert_eq!(final_records, [a]);
}

struct Fake {
    outcomes: Vec<WorkerOutcome>,
    calls: usize,
    contexts: Vec<bool>,
}
impl WorkerRun for Fake {
    fn run(
        &mut self,
        _: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome {
        self.calls += 1;
        self.contexts.push(
            result_dir.is_dir()
                && result_file.starts_with(result_dir)
                && context.is_file()
                && token.len() == 64,
        );
        self.outcomes.remove(0)
    }
}

#[test]
fn run_one_mints_one_use_context_relays_status_output_and_cleans_scratch() {
    let d = TempDir::new("ledger-run").unwrap();
    let home = d.path().to_string_lossy();
    let record = checkout(d.path(), "web", true);
    let logger = log();
    for (outcome, verbose, want_rc, want_out, want_warn) in [
        (
            WorkerOutcome {
                rc: 0,
                output: b"ok\n\n".to_vec(),
            },
            true,
            0,
            true,
            false,
        ),
        (
            WorkerOutcome {
                rc: 0,
                output: b"quiet\n".to_vec(),
            },
            false,
            0,
            false,
            false,
        ),
        (
            WorkerOutcome {
                rc: 7,
                output: b"failed\n".to_vec(),
            },
            true,
            7,
            false,
            true,
        ),
    ] {
        let mut worker = Fake {
            outcomes: vec![outcome],
            calls: 0,
            contexts: vec![],
        };
        let mut out = vec![];
        let mut warnings = vec![];
        let rc = lifecycle::run_one(
            &lifecycle::RunInputs {
                record: &record,
                home: &home,
                euid: euid(),
                tmpdir: d.path(),
                now_secs: now(),
                verbose,
                log: &logger,
            },
            &mut worker,
            &mut out,
            &mut warnings,
        );
        assert_eq!(rc, want_rc);
        assert_eq!(!out.is_empty(), want_out);
        assert_eq!(!warnings.is_empty(), want_warn);
        assert_eq!(worker.calls, 1);
        assert_eq!(worker.contexts, [true]);
        assert_eq!(
            std::fs::read_dir(d.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("dot."))
                .count(),
            0
        );
    }
}

#[test]
fn retire_skips_eligible_runs_all_retiring_and_latches_failures() {
    let d = TempDir::new("ledger-retire").unwrap();
    let home = d.path().to_string_lossy();
    let keep = checkout(d.path(), "keep", true);
    let old = checkout(d.path(), "old", true);
    let bad = checkout(d.path(), "bad", true);
    let logger = log();
    let mut worker = Fake {
        outcomes: vec![
            WorkerOutcome {
                rc: 0,
                output: vec![],
            },
            WorkerOutcome {
                rc: 2,
                output: b"bad output".to_vec(),
            },
        ],
        calls: 0,
        contexts: vec![],
    };
    let mut out = vec![];
    let mut warnings = vec![];
    let rc = lifecycle::retire(
        &lifecycle::RetireInputs {
            present: true,
            extensions_enabled: true,
            retained: &[keep.clone(), old, bad],
            eligible: &["keep".into()],
            home: &home,
            euid: euid(),
            tmpdir: d.path(),
            now_secs: now(),
            verbose: false,
            log: &logger,
        },
        &mut worker,
        &mut out,
        &mut warnings,
    );
    assert_eq!(rc, 1);
    assert_eq!(worker.calls, 2);
    let text = String::from_utf8(warnings).unwrap();
    assert!(text.contains("bad output"));
    assert!(text.contains("profile deactivation failed: bad"));
    let mut unused = Fake {
        outcomes: vec![],
        calls: 0,
        contexts: vec![],
    };
    assert_eq!(
        lifecycle::retire(
            &lifecycle::RetireInputs {
                present: false,
                extensions_enabled: true,
                retained: &[keep],
                eligible: &[],
                home: &home,
                euid: euid(),
                tmpdir: d.path(),
                now_secs: now(),
                verbose: false,
                log: &logger
            },
            &mut unused,
            &mut vec![],
            &mut vec![]
        ),
        0
    );
    assert_eq!(unused.calls, 0);
}
