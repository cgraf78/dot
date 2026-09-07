//! Native contracts for `dot init` parsing and dispatch.
use dot::init_client_command::{
    self as c, CommandEngine, CommandEnv, FreshInputs, InitMode, InitReport, ParseOutcome,
};
use dot::init_client_record::{RecordFields, write_record};
use dot::temp::MoveCache;
use dot_test_support::TempDir;
use std::cell::Cell;
use std::path::Path;
fn words(v: &[&str]) -> Vec<Vec<u8>> {
    v.iter().map(|s| s.as_bytes().to_vec()).collect()
}
fn parsed(v: &[&str]) -> c::ParsedInit {
    match c::parse(&words(v)) {
        ParseOutcome::Args(p) => p,
        x => panic!("expected args: {x:?}"),
    }
}
fn report(code: i32) -> InitReport {
    InitReport {
        stdout: vec![],
        stderr: vec![],
        code,
    }
}
fn paths(tag: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let d = TempDir::new(tag).unwrap();
    let home = d.path().join("home");
    let state = d.path().join("state");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&state).unwrap();
    (d, home, state)
}
fn plant(state: &Path, home: &Path, name: &str, phase: &str, origin: &str) {
    let destination = state.join("dot/init").join(name);
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let identity = dot::init_client_identity::repo_identity(origin).unwrap();
    let fields = RecordFields {
        origin,
        identity: &identity,
        branch: "main",
        backup: "-",
        git_dir: None,
        commit: None,
        nonce: None,
        git_dev: None,
        git_ino: None,
        dot_bin: "/usr/bin/dot",
        home,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    write_record(&destination, phase, &fields, &mut MoveCache::default()).unwrap();
}
fn run(
    v: &[&str],
    skip: Option<&str>,
    branch: Option<&str>,
    resume_ok: bool,
    rollback_ok: bool,
    fresh_code: i32,
) -> (InitReport, usize, usize, usize) {
    let d = TempDir::new("command").unwrap();
    let home = d.path().join("home");
    let state = d.path().join("state");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&state).unwrap();
    let resumes = Cell::new(0);
    let rollbacks = Cell::new(0);
    let fresh = Cell::new(0);
    let probe = |_: &str| branch.map(str::to_owned);
    let resume = |_: &Path, _: &Path, _: &dot::init_client_record::TransactionRecord| {
        resumes.set(resumes.get() + 1);
        if resume_ok {
            Ok(())
        } else {
            Err(dot::Error::Usage {
                message: "resume refused",
            })
        }
    };
    let rollback = |_: &Path| {
        rollbacks.set(rollbacks.get() + 1);
        if rollback_ok {
            Err(dot::Error::Usage {
                message: "no recoverable transaction",
            })
        } else {
            Err(dot::Error::Usage {
                message: "checkout is committed; rerun the original init command to resume",
            })
        }
    };
    let tail = |_: &FreshInputs| {
        fresh.set(fresh.get() + 1);
        report(fresh_code)
    };
    let engine = CommandEngine {
        remote_default_branch: &probe,
        resume: &resume,
        rollback: &rollback,
        fresh: &tail,
    };
    let env = CommandEnv {
        home: home.to_str().unwrap(),
        xdg_state_home: state.to_str().unwrap(),
        skip_provider: skip,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let out = c::run(&env, &engine, &words(v));
    (out, resumes.get(), rollbacks.get(), fresh.get())
}
#[test]
fn help_flags_print_usage_and_ignore_the_rest() {
    for a in [["--help", "--bad"], ["-h", "origin"]] {
        assert_eq!(c::parse(&words(&a)), ParseOutcome::Help);
        let (out, _, _, _) = run(&a, Some("invalid"), None, true, true, 0);
        assert_eq!(out.code, 0);
        assert_eq!(out.stdout, dot::init_client_adopt::usage());
        assert!(out.stderr.is_empty());
    }
}
#[test]
fn unknown_options_fail_with_the_spelling() {
    let ParseOutcome::Failure(r) = c::parse(&words(&["--wat"])) else {
        panic!()
    };
    assert_eq!(r.code, 1);
    assert_eq!(r.stderr, b"dot init: unknown option: --wat\n");
}
#[test]
fn branch_without_a_value_and_double_origins_are_silent_usage_errors() {
    for a in [vec!["--branch"], vec!["one", "two"]] {
        let ParseOutcome::Failure(r) = c::parse(&words(&a)) else {
            panic!()
        };
        assert_eq!(r.code, 2);
        assert!(r.stderr.is_empty())
    }
}
#[test]
fn status_and_rollback_reject_origins_and_branches() {
    for a in [
        vec!["--status", "origin"],
        vec!["--rollback", "--branch", "main"],
    ] {
        let (r, _, _, _) = run(&a, None, None, true, true, 0);
        assert_eq!(r.code, 2)
    }
}
#[test]
fn missing_origin_prints_usage_to_stderr() {
    let (out, _, _, _) = run(&[], None, None, true, true, 0);
    assert_eq!(out.code, 2);
    assert!(!out.stderr.is_empty());
}
#[test]
fn skip_provider_gate_runs_before_identity_but_after_modes() {
    let (out, _, _, f) = run(
        &["https://github.com/a/b.git"],
        Some("2"),
        Some("main"),
        true,
        true,
        0,
    );
    assert_eq!(out.code, 2);
    assert_eq!(f, 0);
    let (out, _, _, _) = run(&["--status"], Some("2"), None, true, true, 0);
    assert_eq!(out.code, 0);
}
#[test]
fn unsupported_origins_fail_with_the_spelling() {
    let (out, _, _, f) = run(&["not-a-url"], None, Some("main"), true, true, 0);
    assert_eq!(out.code, 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not-a-url"));
    assert_eq!(f, 0);
}
#[test]
fn invalid_branches_fail_with_the_spelling() {
    let (out, _, _, f) = run(
        &["--branch", "bad..name", "https://github.com/a/b.git"],
        None,
        None,
        true,
        true,
        0,
    );
    assert_eq!(out.code, 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad..name"));
    assert_eq!(f, 0);
}
#[test]
fn branch_takes_the_next_word_verbatim() {
    let p = parsed(&["--branch", "--status", "https://github.com/a/b.git"]);
    assert_eq!(p.mode, InitMode::Run);
    assert_eq!(p.branch, b"--status");
}
#[test]
fn status_reports_both_journals() {
    let (_d, home, state) = paths("status-live");
    let origin = "https://github.com/a/b.git";
    plant(&state, &home, "transaction/record", "prepared", origin);
    let env = CommandEnv {
        home: home.to_str().unwrap(),
        xdg_state_home: state.to_str().unwrap(),
        skip_provider: None,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let never = |_: &Path| unreachable!();
    let engine = CommandEngine {
        remote_default_branch: &|_| unreachable!(),
        resume: &|_, _, _| unreachable!(),
        rollback: &never,
        fresh: &|_| unreachable!(),
    };
    let out = c::run(&env, &engine, &words(&["--status"]));
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, b"initialization: incomplete\nphase: prepared\norigin: https://github.com/a/b.git\nbranch: main\nbackup: -\n");
    assert!(out.stderr.is_empty());

    let (_d, home, state) = paths("status-complete");
    plant(&state, &home, "completed", "complete", origin);
    let env = CommandEnv {
        home: home.to_str().unwrap(),
        xdg_state_home: state.to_str().unwrap(),
        skip_provider: None,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let out = c::run(&env, &engine, &words(&["--status"]));
    assert_eq!(
        out.stdout,
        b"initialization: complete\norigin: https://github.com/a/b.git\nbranch: main\n"
    );
}
#[test]
fn status_names_malformed_journals_per_home() {
    for (name, label) in [
        ("transaction/record", "malformed initialization transaction"),
        ("completed", "malformed completion record"),
    ] {
        let (_d, home, state) = paths("status-bad");
        let file = state.join("dot/init").join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"garbage\n").unwrap();
        let env = CommandEnv {
            home: home.to_str().unwrap(),
            xdg_state_home: state.to_str().unwrap(),
            skip_provider: None,
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        };
        let engine = CommandEngine {
            remote_default_branch: &|_| unreachable!(),
            resume: &|_, _, _| unreachable!(),
            rollback: &|_| unreachable!(),
            fresh: &|_| unreachable!(),
        };
        let out = c::run(&env, &engine, &words(&["--status"]));
        assert_eq!(out.code, 1);
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            format!(
                "dot init: {label}: {}\n",
                if name == "completed" {
                    file.display().to_string()
                } else {
                    file.parent().unwrap().display().to_string()
                }
            )
        );
    }
}
#[test]
fn rollback_without_a_transaction_matches() {
    let (out, _, roll, _) = run(&["--rollback"], None, None, true, true, 0);
    assert_eq!(out.code, 1);
    assert_eq!(roll, 1);
    assert_eq!(out.stderr, b"dot init: no recoverable transaction\n");
}
#[test]
fn rollback_of_a_committed_transaction_refuses() {
    let (out, _, roll, _) = run(&["--rollback"], None, None, true, false, 0);
    assert_eq!(out.code, 1);
    assert_eq!(roll, 1);
    assert_eq!(
        out.stderr,
        b"dot init: checkout is committed; rerun the original init command to resume\n"
    );
}
#[test]
fn resume_runs_only_for_matching_live_transactions() {
    let (_d, home, state) = paths("resume");
    let origin = "https://github.com/a/b.git";
    plant(&state, &home, "transaction/record", "prepared", origin);
    let resumes = Cell::new(0);
    let fresh = Cell::new(0);
    let engine = CommandEngine {
        remote_default_branch: &|_| unreachable!(),
        resume: &|_, _, _| {
            resumes.set(resumes.get() + 1);
            Ok(())
        },
        rollback: &|_| unreachable!(),
        fresh: &|_| {
            fresh.set(fresh.get() + 1);
            report(0)
        },
    };
    let env = CommandEnv {
        home: home.to_str().unwrap(),
        xdg_state_home: state.to_str().unwrap(),
        skip_provider: None,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
    };
    let out = c::run(&env, &engine, &words(&["--branch", "main", origin]));
    assert_eq!(out.code, 0);
    assert_eq!(resumes.get(), 1);
    assert_eq!(fresh.get(), 0);

    let out = c::run(&env, &engine, &words(&["--branch", "other", origin]));
    assert_eq!(
        out.stderr,
        b"dot init: existing transaction belongs to a different repository or branch\n"
    );
    assert_eq!(resumes.get(), 1);
}
#[test]
fn default_branch_plumbing_and_fresh_continuation() {
    let (out, resume, _, fresh) = run(
        &["https://github.com/a/b.git"],
        None,
        Some("main"),
        true,
        true,
        7,
    );
    assert_eq!(out.code, 7);
    assert_eq!(resume, 0);
    assert_eq!(fresh, 1);
}
#[test]
fn empty_positionals_are_absent_not_origins() {
    let p = parsed(&[""]);
    assert!(p.origin.is_empty());
    let (out, _, _, f) = run(&[""], None, None, true, true, 0);
    assert_eq!(out.code, 2);
    assert_eq!(f, 0);
}
