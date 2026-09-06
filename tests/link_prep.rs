//! Differential parity tests for the parallel inventory preparation
//! layer (`src/repos_link_prep.rs`, porting `_overlay_prepare_inventories`
//! from `lib/dot/repos/overlays.sh`).
//!
//! Every case runs the live shell function and its Rust twin on one
//! shared fixture (separate output roots, read-only overlay inputs)
//! and compares exit status, index numbering, file modes, inventory
//! entry sets, and frozen local-source identities. Entry sets compare
//! sorted: walk order is filesystem order on both sides, stable on
//! one host but not a cross-host contract.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_link_prep;
use dot::test_support::TempDir;

/// Run one shell snippet with the overlay runtime sourced (the
/// inventory builder shares the config helpers of the leaf layer).
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/reserved.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/repos/config.sh\"\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
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

/// Run `git` hermetically inside `cwd` (no user or system config).
fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut cmd = Command::new("git");
    cmd.args(args);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `name|path|url|conf|optional|sync` overlay record.
fn ov(name: &str, path: &Path, url: &str, sync: &str) -> String {
    format!(
        "{name}|{}|{url}|||{sync}",
        path.to_string_lossy().into_owned()
    )
}

/// Seed a Git worktree overlay: `work` holds the payload commit,
/// `dest` is the clone the record points at (origin = `work`).
fn seed_git_overlay(
    root: &Path,
    home: &Path,
    name: &str,
    files: &[(&str, &str)],
) -> (PathBuf, String) {
    let work = root.join(format!("{name}-work"));
    std::fs::create_dir_all(work.join("home")).expect("work home");
    for (rel, body) in files {
        stage(&work.join("home"), rel, body.as_bytes());
    }
    git(&work, home, &["init", "-b", "main"]);
    git(&work, home, &["add", "-A"]);
    // Empty payloads still need a commit so the clone carries an
    // origin (the empty `home/` dir itself never enters git).
    git(&work, home, &["commit", "-qm", "seed", "--allow-empty"]);
    let dest = root.join(name);
    let work_text = work.to_string_lossy().into_owned();
    let dest_text = dest.to_string_lossy().into_owned();
    git(root, home, &["clone", "-q", &work_text, &dest_text]);
    let url = work.to_string_lossy().into_owned();
    (dest, url)
}

/// Snippet dumping `_overlay_prepare_inventories` over `overlays`
/// into `root`: `rc`, then per-name index/mode rows, sorted entry
/// rows (relativized to `fix`), frozen identities, and the root
/// listing. Paths relativize so shell and Rust dumps compare.
fn prep_snippet(fix: &Path, root: &Path, overlays: &[String]) -> String {
    let mut out = format!("export HOME={}; ", sq(&fix.to_string_lossy()));
    out.push_str("OVERLAYS=(");
    for entry in overlays {
        out.push_str(&sq(entry));
        out.push(' ');
    }
    out.push_str("); ");
    out.push_str("declare -A _overlay_inventory_files=(); ");
    out.push_str("declare -A _overlay_inventory_source_roots=(); ");
    out.push_str("declare -A _overlay_inventory_source_identities=(); ");
    let fix_sq = sq(&fix.to_string_lossy());
    let root_sq = sq(&root.to_string_lossy());
    out.push_str(&format!(
        concat!(
            "_overlay_prepare_inventories {root}; code=$?; printf 'rc=%s\\n' \"$code\"; ",
            "while IFS= read -r k; do [[ -n $k ]] || continue; ",
            "p=${{_overlay_inventory_files[$k]}}; ",
            "printf 'inv\\t%s\\t%s\\t%s\\n' \"$k\" \"${{p##*/}}\" ",
            "\"$(stat -c '%a' \"$p\" 2>/dev/null || stat -f '%Lp' \"$p\" 2>/dev/null || echo NONE)\"; ",
            "tr '\\0' '\\n' <\"$p\" | LC_ALL=C sort | while IFS= read -r line; do ",
            "[[ -n $line ]] || continue; printf 'ent\\t%s\\t%s\\n' \"$k\" \"${{line#{fix}/}}\"; done; ",
            "done < <(printf '%s\\n' \"${{!_overlay_inventory_files[@]}}\" | LC_ALL=C sort); ",
            "while IFS= read -r k; do [[ -n $k ]] || continue; ",
            "printf 'root\\t%s\\t%s\\n' \"$k\" \"${{_overlay_inventory_source_roots[$k]#{fix}/}}\"; ",
            "done < <(printf '%s\\n' \"${{!_overlay_inventory_source_roots[@]}}\" | LC_ALL=C sort); ",
            "while IFS= read -r k; do [[ -n $k ]] || continue; ",
            "printf 'ident\\t%s\\t%s\\n' \"$k\" \"${{_overlay_inventory_source_identities[$k]}}\"; ",
            "done < <(printf '%s\\n' \"${{!_overlay_inventory_source_identities[@]}}\" | LC_ALL=C sort); ",
            "if [[ -d {root} ]]; then ls -A {root} | LC_ALL=C sort | ",
            "while IFS= read -r f; do printf 'file\\t%s\\n' \"$f\"; done; fi\n",
        ),
        root = root_sq,
        fix = fix_sq,
    ));
    out
}

/// Render the Rust twin dump in the exact snippet shape above.
fn rust_dump(fix: &Path, root: &Path, result: &Option<repos_link_prep::Prepared>) -> String {
    let mut out = String::new();
    match result {
        None => out.push_str("rc=1\n"),
        Some(prepared) => {
            out.push_str("rc=0\n");
            let fix_prefix = format!("{}/", fix.to_string_lossy());
            let mut names: Vec<&String> = prepared.inventories.keys().collect();
            names.sort();
            for name in names {
                let path = &prepared.inventories[name];
                let index = path
                    .file_name()
                    .map(|base| base.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mode = std::fs::metadata(path)
                    .map(|meta| format!("{:o}", meta.permissions().mode() & 0o777))
                    .unwrap_or_else(|_| "NONE".to_string());
                out.push_str(&format!("inv\t{name}\t{index}\t{mode}\n"));
                let bytes = std::fs::read(path).unwrap_or_default();
                let mut entries: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
                // The trailing NUL leaves one empty tail slice.
                if entries.last() == Some(&b"".as_slice()) {
                    entries.pop();
                }
                entries.sort();
                for entry in entries {
                    let text = String::from_utf8_lossy(entry);
                    let rel = text.strip_prefix(&fix_prefix).unwrap_or(&text);
                    out.push_str(&format!("ent\t{name}\t{rel}\n"));
                }
            }
            let mut roots: Vec<&String> = prepared.source_roots.keys().collect();
            roots.sort();
            for name in roots {
                let value = &prepared.source_roots[name];
                let rel = value.strip_prefix(&fix_prefix).unwrap_or(value);
                out.push_str(&format!("root\t{name}\t{rel}\n"));
            }
            let mut idents: Vec<&String> = prepared.source_identities.keys().collect();
            idents.sort();
            for name in idents {
                out.push_str(&format!(
                    "ident\t{name}\t{}\n",
                    prepared.source_identities[name]
                ));
            }
            if let Ok(dir) = std::fs::read_dir(root) {
                let mut files: Vec<Vec<u8>> = Vec::new();
                for entry in dir.flatten() {
                    files.push(entry.file_name().as_bytes().to_vec());
                }
                files.sort();
                for file in files {
                    out.push_str(&format!("file\t{}\n", String::from_utf8_lossy(&file)));
                }
            }
        }
    }
    out
}

/// Run both twins over one shared fixture and require identical dumps.
fn assert_twins(fix: &Path, home: &Path, overlays: &[String], jobs: Option<&str>) {
    let shell_root = fix.join("inv-shell");
    std::fs::create_dir_all(&shell_root).expect("shell root");
    let (code, out, _serr) = shell_run(home, &prep_snippet(fix, &shell_root, overlays));
    assert_eq!(code, 0, "harness exit");
    let shell = String::from_utf8(out).expect("shell dump");
    let rust_root = fix.join("inv-rust");
    std::fs::create_dir_all(&rust_root).expect("rust root");
    let home_text = home.to_string_lossy().into_owned();
    let inputs = repos_link_prep::Inputs {
        entries: overlays,
        home: &home_text,
        update_jobs: jobs,
    };
    let result = repos_link_prep::prepare_inventories(&inputs, &rust_root);
    assert_eq!(
        shell,
        rust_dump(fix, &rust_root, &result),
        "shell/Rust dumps differ"
    );
    // No staging file survives success on either side: the shell
    // writes final names directly, Rust renames every staging file
    // into place during the ordered commit.
    for dir in [&shell_root, &rust_root] {
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let base = entry.file_name();
                assert!(
                    !base.as_bytes().starts_with(b".build-"),
                    "staging leftover: {}",
                    base.to_string_lossy()
                );
            }
        }
    }
}

#[test]
fn three_git_overlays_match_shell() {
    let dir = TempDir::new("linkprep-three").expect("fixture dir");
    let fix = dir.path();
    let home = fix.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let (p0, u0) = seed_git_overlay(
        fix,
        &home,
        "ov0",
        &[
            ("a.conf", "a\n"),
            ("sub/b.conf", "b\n"),
            ("stale.~1~", "backup\n"),
            ("tilde~", "kept\n"),
            ("x.~a~", "kept\n"),
        ],
    );
    std::os::unix::fs::symlink("a.conf", p0.join("home").join("link.conf")).expect("symlink");
    git(&p0, &home, &["add", "-A"]);
    git(&p0, &home, &["commit", "-qm", "link"]);
    let (p1, u1) = seed_git_overlay(fix, &home, "ov1", &[("one.conf", "1\n")]);
    // An existing-but-empty `home/` stays included with an empty
    // inventory on both sides (empty dirs never enter git, so seed
    // one file and remove it post-clone; the prep path never checks
    // worktree cleanliness).
    let (p2, u2) = seed_git_overlay(fix, &home, "ov2", &[("gone.conf", "x\n")]);
    std::fs::remove_file(p2.join("home").join("gone.conf")).expect("empty the clone home");
    let overlays = vec![
        ov("ov0", &p0, &u0, "git"),
        ov("ov1", &p1, &u1, "git"),
        ov("ov2", &p2, &u2, "git"),
    ];
    assert_twins(fix, &home, &overlays, None);
    assert_twins(fix, &home, &overlays, Some("2"));
}

#[test]
fn skips_mirror_shell() {
    let dir = TempDir::new("linkprep-skips").expect("fixture dir");
    let fix = dir.path();
    let home = fix.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let (good, good_url) = seed_git_overlay(fix, &home, "good", &[("a.conf", "a\n")]);
    let (stale, _) = seed_git_overlay(fix, &home, "stale", &[("a.conf", "a\n")]);
    git(
        &stale,
        &home,
        &["remote", "set-url", "origin", "/nonexistent/other.git"],
    );
    let plain = fix.join("plain");
    stage(&plain.join("home"), "a.conf", b"a\n");
    let bogus = fix.join("bogus");
    stage(&bogus.join("home"), "b.conf", b"b\n");
    let local = fix.join("local");
    stage(&local.join("home"), "c.conf", b"c\n");
    let overlays = vec![
        ov("good", &good, &good_url, "git"),
        // Origin mismatch: skipped by both.
        ov("stale", &stale, "/nonexistent/elsewhere.git", "git"),
        // Not a worktree: skipped by both.
        ov("plain", &plain, "https://example.invalid/x", "git"),
        // No home/ directory: skipped by both.
        ov(
            "missing",
            &fix.join("absent"),
            "https://example.invalid/x",
            "git",
        ),
        // Empty path: skipped (fail closed; see module docs).
        "bad|||||git".to_string(),
        // Unknown sync reads as a local source on both sides.
        ov("bogus", &bogus, "https://example.invalid/x", "bogus"),
        ov("local", &local, "https://example.invalid/x", "none"),
    ];
    assert_twins(fix, &home, &overlays, Some("3"));
}

#[test]
fn unwritable_root_fails_both() {
    let dir = TempDir::new("linkprep-noroot").expect("fixture dir");
    let fix = dir.path();
    let home = fix.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let (p0, u0) = seed_git_overlay(fix, &home, "ov0", &[("a.conf", "a\n")]);
    let overlays = vec![ov("ov0", &p0, &u0, "git")];
    // A regular file where the inventory root belongs: the shell's
    // `: >file` fails and the Rust staging write fails too.
    let root = stage(fix, "file-root", b"x");
    let (code, out, _serr) = shell_run(&home, &prep_snippet(fix, &root, &overlays));
    assert_eq!(code, 0, "harness exit");
    let shell = String::from_utf8(out).expect("shell dump");
    assert!(shell.starts_with("rc=1\n"), "shell rc: {shell:?}");
    let home_text = home.to_string_lossy().into_owned();
    let inputs = repos_link_prep::Inputs {
        entries: &overlays,
        home: &home_text,
        update_jobs: None,
    };
    assert!(
        repos_link_prep::prepare_inventories(&inputs, &root).is_none(),
        "Rust fails closed too"
    );
}

#[test]
fn empty_entries_match_shell() {
    let dir = TempDir::new("linkprep-empty").expect("fixture dir");
    let fix = dir.path();
    let home = fix.join("home");
    std::fs::create_dir_all(&home).expect("home");
    assert_twins(fix, &home, &[], None);
}

/// Stress the fan-out (new parallelism needs a stress test, not
/// just parity): one shared fixture under many job bounds plus
/// concurrent runs from several threads must all converge on the
/// same dumps with no staging leftovers.
#[test]
fn stress_parallel_repeated() {
    let dir = TempDir::new("linkprep-stress").expect("fixture dir");
    let fix = dir.path().to_path_buf();
    let home = fix.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let mut overlays = Vec::new();
    for index in 0..6 {
        let name = format!("ov{index}");
        let (path, url) = seed_git_overlay(
            &fix,
            &home,
            &name,
            &[("a.conf", "a\n"), ("sub/b.conf", "b\n")],
        );
        overlays.push(ov(&name, &path, &url, "git"));
    }
    let local = fix.join("local");
    stage(&local.join("home"), "c.conf", b"c\n");
    overlays.push(ov("local", &local, "https://example.invalid/x", "none"));
    let home_text = home.to_string_lossy().into_owned();
    let dump_for = |tag: &str, jobs: Option<&str>| -> String {
        let root = fix.join(format!("inv-{tag}"));
        std::fs::create_dir_all(&root).expect("stress root");
        let inputs = repos_link_prep::Inputs {
            entries: &overlays,
            home: &home_text,
            update_jobs: jobs,
        };
        let result = repos_link_prep::prepare_inventories(&inputs, &root);
        let dump = rust_dump(&fix, &root, &result);
        assert!(result.is_some(), "stress run {tag} succeeds");
        assert!(dump.starts_with("rc=0\n"), "stress rc {tag}");
        dump
    };
    let baseline = dump_for("base", None);
    for jobs in ["1", "2", "8", "", "abc", "0"] {
        assert_eq!(dump_for(&format!("jobs-{jobs}"), Some(jobs)), baseline);
    }
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for worker in 0..4 {
            let run = scope.spawn(move || {
                let mut seen = Vec::new();
                for round in 0..3 {
                    seen.push(dump_for(&format!("t{worker}-r{round}"), Some("2")));
                }
                seen
            });
            handles.push(run);
        }
        for run in handles {
            for dump in run.join().expect("stress worker") {
                assert_eq!(dump, baseline);
            }
        }
    });
}
