//! Differential parity tests for `_backup_pull_conflicts`
//! (`lib/dot/repos/pull.sh`) against the live shell: conflict-log
//! triage, managed-overlay adoption, backup moves with identity
//! checks, commit, and the failure-recovery walk.
//!
//! Separate binary because this chapter needs the full pull
//! runtime: `pull.sh` plus the resources/temp/log/model/overlays
//! libraries it draws on.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::{DestinationInputs, QuarantineInputs, RollbackSnapshot};
use dot::repos_pull_backup::{BackupConflictsInputs, backup_pull_conflicts};
use dot::test_support::TempDir;

/// Sources for the backup chapter.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the pull runtime sourced.
fn shell_run(home: &Path, argv: &[&OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
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
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `.envs` needs Rust 1.89; the explicit loop keeps the 1.85
    // floor (like the port, which avoids let-chains). This must
    // run after `env_clear`, which wipes everything set before it.
    for (key, value) in locale_passthrough() {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

// Ambient locale variables passed to the shell so both engines'
// forked tools diagnose alike. The conflict fixtures are ASCII, so
// the detector stays deterministic under any locale.
fn locale_passthrough() -> Vec<(String, String)> {
    ["LANG", "LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// One twin side: `$HOME` plus the pull root (the same directory
/// when the row exercises adoption, a sibling otherwise).
struct Side {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    root: PathBuf,
    root_text: String,
    manifest: String,
    legacy: String,
}

impl Side {
    fn build(tag: &str, root_is_home: bool) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("fixture home");
        let root = if root_is_home {
            home.clone()
        } else {
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).expect("fixture root");
            root
        };
        let home_text = home.to_string_lossy().into_owned();
        let root_text = root.to_string_lossy().into_owned();
        Side {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            root,
            root_text,
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }
}

/// Pull-log body listing `rels` after the marker the detector scans
/// for, mirroring `git pull` output.
fn pull_log(rels: &[&str]) -> String {
    let mut out = String::from(
        "error: Your local changes to the following files would be overwritten by merge:\n",
    );
    out.push_str("error: untracked working tree files would be overwritten by merge:\n");
    for rel in rels {
        out.push('\t');
        out.push_str(rel);
        out.push('\n');
    }
    out
}

/// Snapshot builder: per-side absolute targets for the row's rels.
fn snapshot_for(side: &Side, tag: &str) -> (Vec<String>, Vec<String>) {
    match tag {
        "adopts" => {
            let target = side.root.join("target.txt").to_string_lossy().into_owned();
            (
                vec!["sub/anchor".to_string(), "sub/anchor2".to_string()],
                vec![target.clone(), target],
            )
        }
        "reserved" => (
            vec![".dotfiles-evil/x".to_string()],
            vec!["whatever".to_string()],
        ),
        "adopt-then-fail" => {
            let target = side.root.join("target.txt").to_string_lossy().into_owned();
            (vec!["sub/anchor".to_string()], vec![target])
        }
        _ => (vec![], vec![]),
    }
}

/// Fixture per row under the side's pull root (and home).
fn setup_side(side: &Side, tag: &str) {
    match tag {
        "no-marker" | "backs-up" | "backup-blocked" => {
            stage(&side.root, "note.txt", b"user data\n");
            if tag == "backup-blocked" {
                stage(&side.home, ".dot-backup/pull", b"in the way\n");
            }
        }
        "absent" => {}
        "dir-pair" => {
            stage(&side.root, "sub/file", b"nested\n");
        }
        "adopts" | "adopt-then-fail" => {
            let target = stage(&side.root, "target.txt", b"managed\n");
            std::fs::create_dir_all(side.root.join("sub")).expect("subdir");
            std::os::unix::fs::symlink(&target, side.root.join("sub/anchor"))
                .expect("managed link");
            if tag == "adopts" {
                std::os::unix::fs::symlink(&target, side.root.join("sub/anchor2"))
                    .expect("second managed link");
            }
            if tag == "adopt-then-fail" {
                stage(&side.root, "sub/file", b"nested\n");
            }
        }
        "reserved" => {
            stage(&side.root, ".dotfiles-evil/x", b"user file\n");
        }
        "dangling" => {
            std::os::unix::fs::symlink("gone-nowhere", side.root.join("link"))
                .expect("dangling link");
        }
        _ => unreachable!("unknown row {tag}"),
    }
}

/// Rels the aftermath dump probes under the pull root.
fn probe_rels(tag: &str) -> &'static [&'static str] {
    match tag {
        "no-marker" | "backs-up" | "backup-blocked" => &["note.txt"],
        "absent" => &["note.txt"],
        "dangling" => &["link"],
        "dir-pair" => &["sub/file", "sub"],
        "adopts" => &["sub/anchor", "sub/anchor2"],
        "adopt-then-fail" => &["sub/anchor", "sub/file", "sub"],
        "reserved" => &[".dotfiles-evil/x"],
        _ => unreachable!("unknown row {tag}"),
    }
}

/// Rows whose quarantine gate can engage (pull root at `$HOME`)
/// also probe for leaked stage directories.
fn leak_probe(tag: &str) -> bool {
    matches!(tag, "adopts" | "adopt-then-fail" | "reserved")
}

/// Rows where `_backup_dir` runs, so `REPLY` is the backup path by
/// contract rather than stale helper output.
fn backup_compared(tag: &str) -> bool {
    matches!(tag, "backs-up" | "dir-pair" | "adopt-then-fail")
}

/// Shell aftermath probe: one `st=` line per rel plus the surviving
/// backup-pull entries.
fn shell_probe(side: &Side, tag: &str) -> String {
    let mut out = String::new();
    for rel in probe_rels(tag) {
        out.push_str(&format!(
            "p={}; if [[ -L \"$p\" ]]; then printf 'st={rel}:link:%s\\n' \"$(readlink \"$p\")\"; \
             elif [[ -d \"$p\" ]]; then printf 'st={rel}:dir\\n'; \
             elif [[ -f \"$p\" ]]; then printf 'st={rel}:file:%s\\n' \"$(cat \"$p\")\"; \
             else printf 'st={rel}:absent\\n'; fi; ",
            sq(&format!("{}/{}", side.root_text, rel)),
        ));
    }
    out.push_str(&format!(
        "shopt -s nullglob; if [[ -d {} ]]; then for e in {}/*; do [[ -e \"$e\" ]] && printf 'kept=%s\\n' \"${{e##*/}}\"; done; printf 'pulldir=present\\n'; else printf 'pulldir=absent\\n'; fi\n",
        sq(&format!("{}/.dot-backup/pull", side.home_text)),
        sq(&format!("{}/.dot-backup/pull", side.home_text)),
    ));
    if leak_probe(tag) {
        out.push_str(&format!(
            "leaked=no; for e in {}/sub/.*.dot-overlay-adopt.*; do [[ -e \"$e\" ]] && leaked=yes; done; printf 'leaked=%s\\n' \"$leaked\"\n",
            sq(&side.root_text),
        ));
    }
    out
}

/// Shell preamble: home, rollback snapshot, empty overlay records,
/// manifests, and the source root; the topology pins after sourcing
/// because model.sh detection runs at load.
fn shell_preamble(side: &Side, snapshot: &(Vec<String>, Vec<String>)) -> String {
    let repo = env!("CARGO_MANIFEST_DIR");
    let mut out = format!("export HOME={} ", sq(&side.home_text));
    out.push_str("DOT_OVERLAY_ROLLBACK_PATHS=(");
    for rel in &snapshot.0 {
        out.push_str(&sq(rel));
        out.push(' ');
    }
    out.push_str("); DOT_OVERLAY_ROLLBACK_TARGETS=(");
    for target in &snapshot.1 {
        out.push_str(&sq(target));
        out.push(' ');
    }
    out.push_str(&format!(
        "); OVERLAYS=(); ACTIVE_OVERLAYS=(); DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={} DOT_SOURCE_ROOT={}; ",
        sq(&side.manifest),
        sq(&side.legacy),
        sq(repo),
    ));
    out.push_str("DOT_BASE_TOPOLOGY=ordinary; ");
    out
}

/// Replace side-local paths and date stamps so twin dumps compare.
fn normalize(text: &str, home: &str, root: &str) -> String {
    let text = text.replace(home, "@HOME@").replace(root, "@ROOT@");
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let run = bytes[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if run >= 14 {
            out.push_str("@STAMP@");
            index += run;
            while index < bytes.len() && bytes[index] == b'.' {
                let suffix: Vec<u8> = bytes[index + 1..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric())
                    .copied()
                    .collect();
                if suffix.len() == 6 {
                    out.push_str(".@RAND@");
                    index += 7;
                } else {
                    break;
                }
            }
        } else {
            out.push(bytes[index] as char);
            index += 1;
        }
    }
    out
}

/// Rust aftermath dump mirroring [`shell_probe`].
fn rust_probe(side: &Side, tag: &str) -> String {
    let mut out = String::new();
    for rel in probe_rels(tag) {
        let path = side.root.join(rel);
        let state = match std::fs::symlink_metadata(&path) {
            Err(_) => "absent".to_string(),
            Ok(meta) if meta.file_type().is_symlink() => format!(
                "link:{}",
                std::fs::read_link(&path)
                    .map(|target| target.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            Ok(meta) if meta.is_dir() => "dir".to_string(),
            Ok(_) => format!(
                "file:{}",
                String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default())
                    .trim_end_matches('\n')
            ),
        };
        out.push_str(&format!("st={rel}:{state}\n"));
    }
    let pull = side.home.join(".dot-backup/pull");
    match std::fs::read_dir(&pull) {
        Err(_) => out.push_str("pulldir=absent\n"),
        Ok(entries) => {
            let mut kept: Vec<String> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            kept.sort();
            for name in kept {
                out.push_str(&format!("kept={name}\n"));
            }
            out.push_str("pulldir=present\n");
        }
    }
    if leak_probe(tag) {
        let leaked = std::fs::read_dir(side.root.join("sub")).is_ok_and(|entries| {
            entries.filter_map(|entry| entry.ok()).any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".dot-overlay-adopt.")
            })
        });
        out.push_str(&format!("leaked={}\n", if leaked { "yes" } else { "no" }));
    }
    out
}

/// Run one row on twin sides and compare rc, backup, warnings, and
/// aftermath.
fn check_row(tag: &str, root_is_home: bool, log: &str, want_ok: bool) {
    let shell_side = Side::build(&format!("{tag}-shell"), root_is_home);
    let rust_side = Side::build(&format!("{tag}-rust"), root_is_home);
    setup_side(&shell_side, tag);
    setup_side(&rust_side, tag);
    let shell_snapshot = snapshot_for(&shell_side, tag);
    let rust_snapshot = snapshot_for(&rust_side, tag);
    let log_path = shell_side._dir.path().join("pull.log");
    std::fs::write(&log_path, log.as_bytes()).expect("log fixture");
    let snippet = format!(
        "{}{} code=$?; printf 'rc=%s\\nbackup=%s\\n' \"$code\" \"${{REPLY:-}}\"; ",
        shell_preamble(&shell_side, &shell_snapshot),
        format_args!(
            "_backup_pull_conflicts {} {};",
            sq(&log_path.to_string_lossy()),
            sq(&shell_side.root_text),
        ),
    );
    let snippet = format!("{snippet}{}", shell_probe(&shell_side, tag));
    let (code, out, err) = shell_run(shell_side._dir.path(), &[], &snippet);
    assert_eq!(
        code,
        0,
        "harness exit for {tag}: {}",
        String::from_utf8_lossy(&err)
    );
    let shell = format!(
        "{}{}",
        String::from_utf8(out).expect("shell dump"),
        String::from_utf8(err).expect("shell warnings"),
    );
    let shell = normalize(&shell, &shell_side.home_text, &shell_side.root_text);

    let dest = DestinationInputs {
        pwd: rust_side.home_text.clone(),
        home: rust_side.home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let quarantine = root_is_home.then(|| QuarantineInputs {
        snapshot: RollbackSnapshot {
            paths: rust_snapshot.0.clone(),
            targets: rust_snapshot.1.clone(),
        },
        context: dest.clone(),
        tool: tool.clone(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
    });
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: rust_side.home_text.clone(),
    };
    let logger = Log::new(false, false);
    let inputs = BackupConflictsInputs {
        home: &rust_side.home_text,
        root: &rust_side.root_text,
        pull_log: &log_path,
        base: &base,
        quarantine: quarantine.clone(),
        overlays: &[],
        dest: &dest,
        manifest: &rust_side.manifest,
        legacy_manifest: &rust_side.legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &rust_side.home,
        log: &logger,
        tool: &tool,
    };
    // The log fixture lives on the shell side; each engine reads its
    // own copy so mutation (none here) cannot leak across.
    let rust_log = rust_side._dir.path().join("pull.log");
    std::fs::write(&rust_log, log.as_bytes()).expect("rust log");
    let inputs = BackupConflictsInputs {
        pull_log: &rust_log,
        ..inputs
    };
    let mut warnings = Vec::new();
    let outcome = backup_pull_conflicts(&inputs, &mut moves, &mut warnings);
    let mut rust = format!(
        "rc={}\nbackup={}\n",
        if outcome.succeeded { 0 } else { 1 },
        outcome
            .backup
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    rust.push_str(&rust_probe(&rust_side, tag));
    rust.push_str(&String::from_utf8(warnings).expect("rust warnings"));
    let rust = normalize(&rust, &rust_side.home_text, &rust_side.root_text);
    // Without a created backup `REPLY` is stale helper output, not
    // contract: drop the line on both sides instead of comparing
    // noise.
    let (rust, shell) = if backup_compared(tag) {
        (rust, shell)
    } else {
        (strip_backup(&rust), strip_backup(&shell))
    };
    assert_eq!(rust, shell, "backup aftermath for {tag}");
    assert_eq!(outcome.succeeded, want_ok, "backup rc for {tag}");
}

/// Drop the `backup=` line from a dump.
fn strip_backup(dump: &str) -> String {
    dump.lines()
        .filter(|line| !line.starts_with("backup="))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Non-adopting rows use a pull root beside `$HOME`.
#[test]
fn backup_rows_agree() {
    for ((tag, rels), want_ok) in [
        (("no-marker", &[] as &[&str]), false),
        (("absent", &["note.txt"]), false),
        (("backs-up", &["note.txt"]), true),
        (("dir-pair", &["sub/file", "sub"]), false),
        (("backup-blocked", &["note.txt"]), false),
        (("dangling", &["link"]), true),
    ] {
        let log = if tag == "no-marker" {
            "error: something else failed\n".to_string()
        } else {
            pull_log(rels)
        };
        check_row(tag, false, &log, want_ok);
    }
}

/// Adoption rows run with the pull root at `$HOME` so the quarantine
/// gate engages.
#[test]
fn adoption_rows_agree() {
    for (tag, rels, want_ok) in [
        ("adopts", &["sub/anchor", "sub/anchor2"] as &[&str], true),
        ("reserved", &[".dotfiles-evil/x"], false),
        (
            "adopt-then-fail",
            &["sub/anchor", "sub/file", "sub"] as &[&str],
            false,
        ),
    ] {
        check_row(tag, true, &pull_log(rels), want_ok);
    }
}
