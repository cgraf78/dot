//! Differential parity tests for `src/repos_pull_support.rs` against
//! the live shell (`lib/dot/repos/pull.sh`): the conflict-log parser,
//! the timestamped backup directory maker, and the locale-pinned pull
//! runner.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::repos_pull_support::{backup_dir, conflicts_from_log};
use dot::test_support::TempDir;

/// Sources plus the init stub `model.sh` needs at source time.
/// `pull.sh` contributes only function definitions at load; the stub
/// keeps the `model.sh` selection on the no-record path like the
/// slice-11/12 harnesses.
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the repos libraries sourced.
fn shell_run(
    home: &Path,
    argv: &[&OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Write `content` to a fresh temp file, returning its path.
fn log_file(dir: &TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, content).expect("write log fixture");
    path.to_string_lossy().into_owned()
}

#[test]
fn conflicts_from_log_parses_awk_state_machine() {
    let dir = TempDir::new("pull-conflicts").expect("fixture dir");
    // (fixture bytes, shell-expectation note): the awk program starts
    // at the marker, strips one leading-whitespace run per indented
    // file line, and stops at the first non-indented line.
    for (name, content) in [
        ("plain", "Already up to date.\n"),
        ("nomarker", "error: some other failure\n  indented.txt\n"),
        (
            "two-files",
            "From example\nuntracked working tree files would be overwritten by merge:\n\ttracked-a.txt\n  tracked-b.txt\nPlease commit.\n",
        ),
        (
            "blank-ends",
            "untracked working tree files would be overwritten by checkout:\n\ttracked.txt\n\ntrailing.txt\n",
        ),
        (
            "ws-only-ends",
            "untracked working tree files would be overwritten by merge:\n\ttracked.txt\n   \nlate.txt\n",
        ),
        (
            "second-marker-ignored",
            "untracked working tree files would be overwritten by merge:\n\ttracked.txt\nDone.\nuntracked working tree files would be overwritten by merge:\n\tlate.txt\n",
        ),
    ] {
        let log = log_file(&dir, name, content);
        let snippet = format!("_pull_conflicts_from_log {log}\n");
        let (shell_status, shell_out, shell_err) = shell_run(dir.path(), &[], &[], &snippet);
        assert_eq!(shell_status, 0, "harness exit for {name}");
        assert!(
            shell_err.is_empty(),
            "shell stderr for {name}: {shell_err:?}"
        );
        let shell_text = String::from_utf8_lossy(&shell_out).into_owned();
        let rust_files = conflicts_from_log(content);
        let rust_text = if rust_files.is_empty() {
            String::new()
        } else {
            rust_files.join("\n") + "\n"
        };
        assert_eq!(shell_text, rust_text, "parser parity for {name}");
    }
    // Pinned absolute outcomes (not just differential): the marker
    // line itself never emits, and only whitespace-led file lines do.
    assert!(conflicts_from_log("no marker here\n").is_empty());
    assert_eq!(
        conflicts_from_log(
            "untracked working tree files would be overwritten by merge:\n\ttracked-a.txt\n  tracked-b.txt\n"
        ),
        vec!["tracked-a.txt".to_string(), "tracked-b.txt".to_string()]
    );
}

/// A 14-digit `%Y%m%d%H%M%S` stamp, like the shell `date` format.
fn is_stamp(name: &str) -> bool {
    name.len() == 14 && name.bytes().all(|byte| byte.is_ascii_digit())
}

#[test]
fn backup_dir_creates_timestamped_dir_and_reports_it() {
    // Separate homes: the stamp has one-second resolution, so two
    // runs against one home would collide by construction (the second
    // runner would take the mktemp fallback on both sides alike).
    let shell_dir = TempDir::new("pull-backup-shell").expect("fixture dir");
    let rust_dir = TempDir::new("pull-backup-rust").expect("fixture dir");
    let snippet = "_backup_dir; rc=$?\nprintf 'rc=%d reply=%s\\n' \"$rc\" \"$REPLY\"\n";
    let (shell_status, shell_out, shell_err) = shell_run(shell_dir.path(), &[], &[], snippet);
    assert_eq!(shell_status, 0, "harness exit");
    assert!(shell_err.is_empty(), "shell stderr: {shell_err:?}");
    let shell_text = String::from_utf8_lossy(&shell_out).into_owned();
    let shell_line = shell_text.lines().next().unwrap_or("");
    assert!(
        shell_line.starts_with("rc=0 reply="),
        "shell creates and reports: {shell_text:?}"
    );
    let shell_reply = shell_line.strip_prefix("rc=0 reply=").unwrap_or("");
    let shell_path = Path::new(shell_reply);
    assert!(shell_path.is_dir(), "shell backup dir exists");
    let home = rust_dir.path();
    let rust_reply = backup_dir(&home.to_string_lossy()).expect("rust backup dir");
    assert!(rust_reply.is_dir(), "rust backup dir exists");
    // Same shape on both sides (timestamps may straddle a second
    // boundary, so names compare by shape, not equality): a 14-digit
    // stamp directly under each `$HOME/.dot-backup/pull`.
    for (reply, base) in [(shell_path, shell_dir.path()), (rust_reply.as_path(), home)] {
        assert_eq!(
            reply.parent().and_then(|parent| parent.to_str()),
            Some(base.join(".dot-backup/pull").to_string_lossy()).as_deref(),
            "backup parent: {}",
            reply.display()
        );
        assert!(
            reply
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_stamp),
            "timestamped leaf: {}",
            reply.display()
        );
    }
}

#[test]
fn backup_dir_reports_none_when_nothing_is_creatable() {
    // `$HOME/.dot-backup` is a file: `mkdir -p` fails, the stamped
    // `mkdir` fails, and the `mktemp` fallback fails too, so both
    // sides report failure with an empty reply.
    let dir = TempDir::new("pull-backup-blocked").expect("fixture dir");
    let home = dir.path();
    std::fs::write(home.join(".dot-backup"), b"blocker\n").expect("blocker file");
    let snippet = "_backup_dir; rc=$?\nprintf 'rc=%d reply=%s\\n' \"$rc\" \"$REPLY\"\n";
    // The shell failure path is noisy on stderr (mkdir/mktemp
    // diagnostics); only the exit code and empty reply are pinned.
    let (shell_status, shell_out, _) = shell_run(home, &[], &[], snippet);
    assert_eq!(shell_status, 0, "harness exit");
    let shell_text = String::from_utf8_lossy(&shell_out).into_owned();
    assert_eq!(
        shell_text.lines().next().unwrap_or(""),
        "rc=1 reply=",
        "shell reports failure: {shell_text:?}"
    );
    assert_eq!(
        backup_dir(&home.to_string_lossy()),
        None,
        "rust reports failure"
    );
}

use dot::repos_pull_support::pull_cmd;

/// Probe recording its full argv plus `LC_ALL` into `$1`:
/// `probe OUTFILE args...` writes `argv:<args>\nlc:<LC_ALL>\n`.
const PROBE_SCRIPT: &str = "#!/bin/sh\nout=$1\nshift\n{\nprintf 'argv:%s\\n' \"$*\"\nprintf 'lc:%s\\n' \"$LC_ALL\"\n} >\"$out\"\n";

/// Write the probe script, returning its path.
fn probe_script(dir: &TempDir) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.path().join("probe.sh");
    std::fs::write(&path, PROBE_SCRIPT).expect("write probe");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod probe");
    path.to_string_lossy().into_owned()
}

/// Read a probe record with the per-side output path normalized away.
fn probe_record(record: &Path, own_out: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(record).expect("probe record")).replace(own_out, "OUT")
}

#[test]
fn pull_cmd_pins_quiet_and_locale_exactly() {
    // `_pull_cmd` appends `--quiet` only in quiet mode and always
    // runs under `LC_ALL=C`; the probe records both, byte for byte.
    let dir = TempDir::new("pull-cmd").expect("fixture dir");
    let probe = probe_script(&dir);
    for quiet in [false, true] {
        let flag = if quiet { "1" } else { "0" };
        let shell_out = dir.path().join(format!("shell-{flag}.out"));
        let shell_out_text = shell_out.to_string_lossy().into_owned();
        let snippet = format!(
            "DOT_QUIET={flag} _pull_cmd {probe} {shell_out_text} alpha beta\necho \"rc=$?\"\n"
        );
        let (shell_status, harness_out, harness_err) = shell_run(dir.path(), &[], &[], &snippet);
        assert_eq!(shell_status, 0, "harness exit quiet={quiet}");
        assert!(
            harness_err.is_empty(),
            "shell stderr quiet={quiet}: {harness_err:?}"
        );
        let shell_rc: i32 = String::from_utf8_lossy(&harness_out)
            .lines()
            .last()
            .unwrap_or("")
            .strip_prefix("rc=")
            .and_then(|text| text.parse().ok())
            .unwrap_or(-1);
        assert_eq!(shell_rc, 0, "probe must succeed quiet={quiet}");
        let rust_out = dir.path().join(format!("rust-{flag}.out"));
        let rust_out_text = rust_out.to_string_lossy().into_owned();
        let rc = pull_cmd(quiet, &probe, &[&rust_out_text, "alpha", "beta"]);
        assert_eq!(rc, 0, "rust probe must succeed quiet={quiet}");
        assert_eq!(
            probe_record(&rust_out, &rust_out_text),
            probe_record(&shell_out, &shell_out_text),
            "probe records match quiet={quiet}"
        );
        let record = probe_record(&shell_out, &shell_out_text);
        assert!(record.contains("lc:C\n"), "locale pinned: {record:?}");
        if quiet {
            assert!(
                record.contains("argv:alpha beta --quiet\n"),
                "quiet appended: {record:?}"
            );
        } else {
            assert!(
                record.contains("argv:alpha beta\n") && !record.contains("--quiet"),
                "argv passthrough: {record:?}"
            );
        }
    }
}

#[test]
fn pull_cmd_propagates_child_exit_code() {
    // A failing child surfaces its exact exit on both sides.
    let dir = TempDir::new("pull-cmd-rc").expect("fixture dir");
    let snippet = "_pull_cmd sh -c 'exit 7'\necho \"rc=$?\"\n";
    let (shell_status, shell_out, _) = shell_run(dir.path(), &[], &[], snippet);
    assert_eq!(shell_status, 0, "harness exit");
    let shell_rc: i32 = String::from_utf8_lossy(&shell_out)
        .lines()
        .last()
        .unwrap_or("")
        .strip_prefix("rc=")
        .and_then(|text| text.parse().ok())
        .unwrap_or(-1);
    assert_eq!(shell_rc, 7, "shell propagates 7");
    assert_eq!(pull_cmd(false, "sh", &["-c", "exit 7"]), 7);
}

use dot::repos_pull_support::{
    PullTally, overlay_active, overlay_count, record_status, result_prefix,
};

/// Make `path` a real Git worktree root for `_overlay_is_worktree`.
fn git_init(path: &Path) {
    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init fixture");
}

#[test]
fn result_prefix_zero_pads_index() {
    let dir = TempDir::new("pull-prefix").expect("fixture dir");
    for (idx, expected) in [
        (0, "out/000"),
        (7, "out/007"),
        (42, "out/042"),
        (1234, "out/1234"),
    ] {
        let (code, out, _) = shell_run(
            dir.path(),
            &[],
            &[],
            &format!("_pull_overlay_result_prefix out {idx}"),
        );
        assert_eq!(code, 0, "shell result prefix {idx}");
        assert_eq!(out, expected.as_bytes(), "shell prefix bytes {idx}");
        assert_eq!(
            result_prefix("out", idx),
            expected,
            "prefix parity for {idx}"
        );
    }
}

#[test]
fn record_status_tallies_and_summarizes() {
    // (name, status): counters start nonzero to prove increments.
    for (name, status) in [
        ("ovl", ""),
        ("ovl", "failed"),
        ("ovl", "changed"),
        ("ovl", "cloned"),
        ("ovl", "skipped"),
        ("ovl", "current"),
        ("ovl", "bogus"),
        ("my ovl", "changed"),
    ] {
        let dir = TempDir::new("pull-tally").expect("fixture dir");
        let (code, out, _) = shell_run(
            dir.path(),
            &[],
            &[],
            &format!(
                "DOT_PULL_OVERLAY_FAILED=1; DOT_PULL_OVERLAY_CHANGED=2; \
                 DOT_PULL_OVERLAY_SKIPPED=3; DOT_PULL_OVERLAY_CURRENT=4; \
                 DOT_PULL_OVERLAY_CHANGED_ITEMS=$'prior\\n'; _summaries=(); \
                 _pull_overlay_record_status \"{name}\" \"{status}\"; \
                 printf 'F=%s C=%s S=%s U=%s N=%s\\n' \"$DOT_PULL_OVERLAY_FAILED\" \
                 \"$DOT_PULL_OVERLAY_CHANGED\" \"$DOT_PULL_OVERLAY_SKIPPED\" \
                 \"$DOT_PULL_OVERLAY_CURRENT\" \"${{#_summaries[@]}}\"; \
                 printf 'SUM=[%s]\\n' \"${{_summaries[*]}}\"; \
                 printf 'ITEMS<<<%s>>>' \"$DOT_PULL_OVERLAY_CHANGED_ITEMS\""
            ),
        );
        assert_eq!(code, 0, "shell record status {name:?} {status:?}");
        let mut tally = PullTally {
            failed: 1,
            changed: 2,
            skipped: 3,
            current: 4,
            changed_items: "prior\n".to_string(),
        };
        let summary = record_status(name, status, &mut tally);
        let summaries = summary.map_or_else(Vec::new, |line| vec![line]);
        let expected = format!(
            "F={} C={} S={} U={} N={}\nSUM=[{}]\nITEMS<<<{}>>>",
            tally.failed,
            tally.changed,
            tally.skipped,
            tally.current,
            summaries.len(),
            summaries.join(" "),
            tally.changed_items,
        );
        assert_eq!(
            String::from_utf8(out).expect("tally utf8"),
            expected,
            "tally parity for {name:?} {status:?}"
        );
    }
}

#[test]
fn overlay_active_needs_worktree_or_url() {
    let dir = TempDir::new("pull-active").expect("fixture dir");
    let worktree = dir.path().join("wt");
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&worktree).expect("worktree dir");
    std::fs::create_dir_all(&plain).expect("plain dir");
    git_init(&worktree);
    // (path, url, optional): name/optional never decide.
    let cases = [
        (
            worktree.to_string_lossy().into_owned(),
            "https://x/y",
            "false",
            true,
        ),
        (worktree.to_string_lossy().into_owned(), "", "true", true),
        (
            plain.to_string_lossy().into_owned(),
            "https://x/y",
            "false",
            true,
        ),
        (plain.to_string_lossy().into_owned(), "", "true", false),
    ];
    for (path, url, optional, expected) in cases {
        let (code, out, _) = shell_run(
            dir.path(),
            &[],
            &[],
            &format!(
                ". \"$1/lib/dot/repos/config.sh\"; _pull_overlay_active name \"{path}\" \"{url}\" \"{optional}\" && printf yes || printf no"
            ),
        );
        assert_eq!(code, 0, "shell overlay active {path:?}");
        assert_eq!(
            out,
            if expected {
                b"yes".as_slice()
            } else {
                b"no".as_slice()
            },
            "shell active for {path:?}"
        );
        assert_eq!(
            overlay_active(Path::new(&path), url),
            expected,
            "active parity for {path:?} {url:?} {optional:?}"
        );
    }
}

#[test]
fn overlay_count_skips_inactive_and_nongit() {
    let dir = TempDir::new("pull-count").expect("fixture dir");
    let worktree = dir.path().join("wt");
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&worktree).expect("worktree dir");
    std::fs::create_dir_all(&plain).expect("plain dir");
    git_init(&worktree);
    let wt = worktree.to_string_lossy();
    let pl = plain.to_string_lossy();
    // (sync, path, url, counted): empty sync defaults to git.
    let rows = [
        ("git", wt.as_ref(), "https://x/y", true),
        ("git", pl.as_ref(), "https://x/y", true),
        ("git", pl.as_ref(), "", false),
        ("none", wt.as_ref(), "https://x/y", false),
        ("", wt.as_ref(), "https://x/y", true),
        ("hg", wt.as_ref(), "https://x/y", false),
    ];
    let entries: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, (sync, path, url, _))| format!("ovl{i}|{path}|{url}|x||{sync}"))
        .collect();
    let quoted: Vec<String> = entries.iter().map(|e| format!("'{e}'")).collect();
    let (code, out, _) = shell_run(
        dir.path(),
        &[],
        &[],
        &format!(
            ". \"$1/lib/dot/repos/config.sh\"; OVERLAYS=({}); _pull_overlay_count",
            quoted.join(" ")
        ),
    );
    assert_eq!(code, 0, "shell overlay count");
    let wanted = rows.iter().filter(|(_, _, _, c)| *c).count();
    assert_eq!(
        String::from_utf8(out).expect("count utf8"),
        wanted.to_string(),
        "shell count counts actives"
    );
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    assert_eq!(overlay_count(&refs), wanted, "count parity");
}
