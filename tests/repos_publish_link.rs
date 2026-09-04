//! Differential parity tests for the link-publication layer of
//! `lib/dot/repos/overlays.sh`: destination-parent repair, final
//! record appending, replacement recovery, and the transactional
//! link publisher.
//!
//! Every case runs the live shell function and its Rust twin on
//! identical fixtures and compares exit status and file effects.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the publication runtime sourced.
fn shell_run(home: &Path, argv: &[&std::ffi::OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(
            ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/reserved.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/init-client.sh\"\n{snippet}"
        ));
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

/// chmod a fixture to an exact mode.
fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Minimal reserved-roots inputs over one fixture side.
fn inputs(side: &Path) -> repos_overlays::DestinationInputs {
    let home = side.to_string_lossy().into_owned();
    repos_overlays::DestinationInputs {
        pwd: home.clone(),
        home,
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: Vec::new(),
        init_backup: None,
    }
}

/// Octal mode of a path, or NONE.
fn mode_of(path: &Path) -> String {
    std::fs::symlink_metadata(path)
        .map(|meta| format!("{:o}", meta.permissions().mode() & 0o777))
        .unwrap_or_else(|_| "NONE".to_string())
}

#[test]
fn ensure_destination_parent_agrees() {
    let dir = TempDir::new("ovlink-parent").expect("fixture dir");
    let home = dir.path();
    // (name, parent rel, setup): parents resolve under the side HOME.
    for (name, rel, setup) in [
        ("at-home", "", ""),
        ("nested", "a/b/c", ""),
        ("existing", "a", "dir"),
        ("blocked-file", "a", "file"),
        ("blocked-link", "a", "link"),
        ("outside", "/elsewhere/x", ""),
        ("dotdot", "a/../b", ""),
        ("git-dir", "a/.Git/b", ""),
    ] {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            match setup {
                "dir" => {
                    std::fs::create_dir_all(root.join("a")).expect("dir");
                }
                "file" => {
                    stage(&root, "a", b"x\n");
                }
                "link" => {
                    std::os::unix::fs::symlink("elsewhere", root.join("a")).expect("link");
                }
                _ => {}
            }
            let parent = if rel.starts_with('/') {
                rel.to_string()
            } else if rel.is_empty() {
                root.to_string_lossy().into_owned()
            } else {
                root.join(rel).to_string_lossy().into_owned()
            };
            let snippet = format!(
                "export HOME={}; _overlay_ensure_destination_parent {}; code=$?; printf 'rc=%s\\n' \"$code\"\n",
                sq(&root.to_string_lossy()),
                sq(&parent),
            );
            if side == "shell" {
                let (code, out, serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                assert!(serr.is_empty(), "parent stderr for {name:?}: {serr:?}");
                let shell = String::from_utf8(out).expect("parent dump");
                std::fs::write(home.join(format!("{name}.shell.out")), shell).expect("stash");
                // Stage the mode tree for the comparison below.
                let mut tree = String::new();
                let mut dirs: Vec<PathBuf> = vec![root.clone()];
                if !rel.is_empty() && !rel.starts_with('/') && !rel.contains("..") {
                    let mut current = root.clone();
                    for component in rel.split('/') {
                        current = current.join(component);
                        dirs.push(current.clone());
                    }
                }
                for dir in dirs {
                    tree.push_str(&format!(
                        "{}:{}\n",
                        dir.strip_prefix(&root).unwrap_or(&dir).to_string_lossy(),
                        mode_of(&dir)
                    ));
                }
                std::fs::write(home.join(format!("{name}.shell.tree")), tree).expect("stash");
            } else {
                let ok =
                    repos_overlays::ensure_destination_parent(&root.to_string_lossy(), &parent);
                let shell =
                    std::fs::read_to_string(home.join(format!("{name}.shell.out"))).expect("stash");
                assert_eq!(
                    format!("rc={}\n", if ok { 0 } else { 1 }),
                    shell,
                    "ensure parent rc for {name:?}"
                );
                let mut tree = String::new();
                let mut dirs: Vec<PathBuf> = vec![root.clone()];
                if !rel.is_empty() && !rel.starts_with('/') && !rel.contains("..") {
                    let mut current = root.clone();
                    for component in rel.split('/') {
                        current = current.join(component);
                        dirs.push(current.clone());
                    }
                }
                for dir in dirs {
                    tree.push_str(&format!(
                        "{}:{}\n",
                        dir.strip_prefix(&root).unwrap_or(&dir).to_string_lossy(),
                        mode_of(&dir)
                    ));
                }
                let shell_tree = std::fs::read_to_string(home.join(format!("{name}.shell.tree")))
                    .expect("stash");
                assert_eq!(tree, shell_tree, "ensure parent tree for {name:?}");
            }
        }
    }
}

#[test]
fn record_final_agrees() {
    let dir = TempDir::new("ovlink-final").expect("fixture dir");
    let home = dir.path();
    for side in ["shell", "rust"] {
        let root = home.join(side);
        std::fs::create_dir_all(&root).expect("side dir");
        let manifest_new = root.join("new.tsv");
        std::fs::write(&manifest_new, b"old\tbase\tt\n").expect("manifest");
        if side == "shell" {
            // `_overlay_manifest_new` is a plain global, not an
            // array: assign it before the call in one line.
            let snippet = format!(
                "_overlay_manifest_new={}; declare -A _overlay_current_paths=(); _overlay_record_final app.conf web .config/app.conf; code=$?; printf 'rc=%s\\n' \"$code\"; printf 'body='; cat {} 2>/dev/null || true; printf 'keys='; printf '%s\\n' \"${{!_overlay_current_paths[@]}}\" | LC_ALL=C sort\n",
                sq(&manifest_new.to_string_lossy()),
                sq(&manifest_new.to_string_lossy()),
            );
            let (code, out, serr) = shell_run(&root, &[], &snippet);
            assert_eq!(code, 0, "harness exit");
            assert!(serr.is_empty(), "final stderr: {serr:?}");
            let shell = String::from_utf8(out).expect("final dump");
            std::fs::write(home.join("final.shell.out"), shell).expect("stash");
        } else {
            let mut current = HashSet::new();
            let ok = repos_overlays::record_final(
                "app.conf",
                "web",
                ".config/app.conf",
                &manifest_new,
                &mut current,
            );
            let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
            rust.push_str("body=");
            rust.push_str(&std::fs::read_to_string(&manifest_new).unwrap_or_default());
            rust.push_str("keys=");
            let mut keys: Vec<&String> = current.iter().collect();
            keys.sort();
            for key in keys {
                rust.push_str(key);
                rust.push('\n');
            }
            let shell = std::fs::read_to_string(home.join("final.shell.out")).expect("stash");
            assert_eq!(rust, shell, "record final");
        }
    }
    // An unwritable manifest fails on both sides.
    let root = home.join("unwritable");
    std::fs::create_dir_all(root.join("new.tsv")).expect("dir blocked");
    let manifest_new = root.join("new.tsv");
    let snippet = format!(
        "_overlay_manifest_new={}; declare -A _overlay_current_paths=(); _overlay_record_final app.conf web .config/app.conf; printf 'rc=%s\\n' \"$?\"\n",
        sq(&manifest_new.to_string_lossy()),
    );
    let (code, out, _serr) = shell_run(&root, &[], &snippet);
    assert_eq!(code, 0, "harness exit");
    assert_eq!(
        String::from_utf8(out).expect("dump"),
        "rc=1\n",
        "shell unwritable"
    );
    let mut current = HashSet::new();
    assert!(
        !repos_overlays::record_final(
            "app.conf",
            "web",
            ".config/app.conf",
            &manifest_new,
            &mut current
        ),
        "rust unwritable"
    );
    assert!(current.is_empty(), "failed append records nothing");
}

/// One crash-state fixture: destination, physical leaf, record,
/// and transaction paths plus the identities that bind them.
struct CrashFixture {
    destination: String,
    physical: PathBuf,
    record: PathBuf,
    transaction: PathBuf,
    parent_identity: String,
}

/// Stage the shared crash layout under `root`: `work/app.conf` is
/// the physical file with `old` bytes, `rel` names the
/// destination, and the manifest lives beside them. Returns the
/// fixture plus the content identity of the physical file.
fn crash_fixture(root: &Path, rel: &str) -> (CrashFixture, String) {
    let destination = root.join(rel).to_string_lossy().into_owned();
    let physical = stage(root, "work/app.conf", b"old");
    let parent = physical.parent().expect("parent").to_path_buf();
    let transaction = parent.join(".app.conf.dot-overlay-replace-v1");
    let expected = repos_overlays::replacement_identity(root, &physical).expect("content identity");
    let parent_identity =
        dot::temp::identity_string(dot::temp::path_identity(&parent).expect("parent identity"));
    (
        CrashFixture {
            destination,
            physical,
            record: PathBuf::new(),
            transaction,
            parent_identity,
        },
        expected,
    )
}

/// The six-field record line for a crash fixture.
fn crash_line(
    fixture: &CrashFixture,
    target: &str,
    expected: &str,
    manifest: &str,
) -> (PathBuf, Vec<u8>) {
    let record = PathBuf::from(format!(
        "{manifest}.replace.{}",
        hash_of(&fixture.destination)
    ));
    let line = format!(
        "{}\t{}\t{target}\t{expected}\t{}\t{}\n",
        fixture.destination,
        fixture.physical.to_string_lossy(),
        fixture.transaction.to_string_lossy(),
        fixture.parent_identity,
    );
    (record, line.into_bytes())
}

/// Independent oracle for record-name hashing (setup only).
fn hash_of(value: &str) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(value.as_bytes())
        .expect("feed git");
    let output = child.wait_with_output().expect("wait git");
    assert!(output.status.success(), "setup hash");
    String::from_utf8(output.stdout)
        .expect("hash")
        .trim()
        .to_string()
}

/// Probe aftermath: record presence, physical shape, and
/// transaction presence. Dumps carry no side paths.
fn aftermath_snippet(record: &str, physical: &str, transaction: &str) -> String {
    format!(
        "r=missing; [[ -e {} || -L {} ]] && r=present; p=absent; if [[ -L {} ]]; then p=\"link:$(readlink {})\"; elif [[ -f {} ]]; then p=\"file:$(cat {})\"; elif [[ -e {} ]]; then p=other; fi; t=missing; [[ -e {} || -L {} ]] && t=present; printf 'record=%s\\nphysical=%s\\ntransaction=%s\\n' \"$r\" \"$p\" \"$t\"\n",
        sq(record),
        sq(record),
        sq(physical),
        sq(physical),
        sq(physical),
        sq(physical),
        sq(physical),
        sq(transaction),
        sq(transaction),
    )
}

/// Read back the same aftermath from Rust.
fn aftermath(record: &Path, physical: &Path, transaction: &Path) -> String {
    let record_state = if record.symlink_metadata().is_ok() {
        "present"
    } else {
        "missing"
    };
    let physical_state = match std::fs::symlink_metadata(physical) {
        Err(_) => "absent".to_string(),
        Ok(meta) if meta.file_type().is_symlink() => format!(
            "link:{}",
            std::fs::read_link(physical)
                .map(|target| target.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        Ok(meta) if meta.is_file() => {
            // `$(cat ...)` strips every trailing newline.
            let body = std::fs::read_to_string(physical).unwrap_or_default();
            format!("file:{}", body.trim_end_matches('\n'))
        }
        Ok(_) => "other".to_string(),
    };
    let transaction_state = if transaction.symlink_metadata().is_ok() {
        "present"
    } else {
        "missing"
    };
    format!("record={record_state}\nphysical={physical_state}\ntransaction={transaction_state}\n")
}

#[test]
fn recover_replacement_agrees() {
    let dir = TempDir::new("ovlink-recover").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    for name in [
        "settled-no-transaction",
        "settled-mismatch",
        "restore-previous",
        "converged-link",
        "diverted-link",
        "previous-mismatch",
        "parent-changed",
        "transaction-unsafe",
        "next-wrong",
        "bad-record",
        "stale-transaction-cleanup",
        "link-cleanup",
    ] {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
            // The diverted case resolves the destination through a
            // symlinked parent, so the lexical parent is stale. The
            // converged case needs the destination to resolve at the
            // physical leaf itself.
            let rel = if name == "diverted-link" {
                "alias/app.conf"
            } else if name == "converged-link" {
                "work/app.conf"
            } else {
                "dest/app.conf"
            };
            let (mut fixture, expected) = crash_fixture(&root, rel);
            if name == "diverted-link" {
                std::os::unix::fs::symlink("elsewhere", root.join("alias")).expect("alias");
                std::fs::create_dir_all(root.join("elsewhere")).expect("elsewhere");
                // The physical leaf is the desired link itself.
                std::fs::remove_file(&fixture.physical).expect("remove file");
                std::os::unix::fs::symlink("want-target", &fixture.physical).expect("link");
            }
            let target = "want-target";
            let (record, line) = crash_line(&fixture, target, &expected, &manifest);
            fixture.record = record;
            match name {
                "settled-mismatch" => {
                    std::fs::write(&fixture.physical, b"new").expect("rewrite");
                }
                "restore-previous" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    std::fs::remove_file(&fixture.physical).expect("remove physical");
                    stage(&fixture.transaction, "previous", b"old");
                }
                "converged-link" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    std::fs::remove_file(&fixture.physical).expect("remove file");
                    std::os::unix::fs::symlink(target, &fixture.physical).expect("link");
                    stage(&fixture.transaction, "previous", b"old");
                }
                "diverted-link" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    stage(&fixture.transaction, "previous", b"old");
                }
                "previous-mismatch" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    std::fs::remove_file(&fixture.physical).expect("remove physical");
                    stage(&fixture.transaction, "previous", b"changed");
                }
                "parent-changed" => {
                    let parent = fixture.physical.parent().expect("parent").to_path_buf();
                    std::fs::remove_file(&fixture.physical).expect("remove file");
                    std::fs::remove_dir(&parent).expect("remove parent");
                    std::fs::create_dir_all(&parent).expect("new parent");
                    std::fs::write(&fixture.physical, b"old").expect("rewrite");
                }
                "transaction-unsafe" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    stage(&fixture.transaction, "stray", b"x");
                }
                "next-wrong" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    std::os::unix::fs::symlink("other", fixture.transaction.join("next"))
                        .expect("next");
                }
                "stale-transaction-cleanup" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                }
                "link-cleanup" => {
                    std::fs::create_dir_all(&fixture.transaction).expect("txn");
                    std::fs::remove_file(&fixture.physical).expect("remove file");
                    std::os::unix::fs::symlink(target, &fixture.physical).expect("link");
                }
                _ => {}
            }
            let (record, line) = if name == "bad-record" {
                (fixture.record.clone(), b"garbage\n".to_vec())
            } else {
                (fixture.record.clone(), line)
            };
            fixture.record = record;
            std::fs::write(&fixture.record, &line).expect("record");
            chmod(&fixture.record, 0o600);
            let snippet = format!(
                "export HOME={}; DOT_OVERLAY_MANIFEST={} _overlay_recover_replacement {}; code=$?; printf 'rc=%s\\n' \"$code\"; {}\n",
                sq(&root.to_string_lossy()),
                sq(&manifest),
                sq(&fixture.record.to_string_lossy()),
                aftermath_snippet(
                    &fixture.record.to_string_lossy(),
                    &fixture.physical.to_string_lossy(),
                    &fixture.transaction.to_string_lossy(),
                ),
            );
            if side == "shell" {
                let (code, out, serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                assert!(serr.is_empty(), "recover stderr for {name:?}: {serr:?}");
                let shell = String::from_utf8(out).expect("recover dump");
                std::fs::write(home.join(format!("{name}.shell.out")), shell).expect("stash");
            } else {
                let ok = repos_overlays::recover_replacement(
                    &fixture.record,
                    &manifest,
                    euid,
                    &root,
                    &root,
                    &root.to_string_lossy(),
                    &tool,
                );
                let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
                rust.push_str(&aftermath(
                    &fixture.record,
                    &fixture.physical,
                    &fixture.transaction,
                ));
                let shell =
                    std::fs::read_to_string(home.join(format!("{name}.shell.out"))).expect("stash");
                assert_eq!(rust, shell, "recover replacement for {name:?}");
            }
        }
    }
}

#[test]
fn recover_replacements_agrees() {
    let dir = TempDir::new("ovlink-recovers").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    // (name, bad sorted position or neither): the sweep follows
    // record-name sort, so badness is assigned after sorting. The
    // bad pair pins stop-at-first (the reply names the first
    // failure, the later record stays).
    for (name, bad_at) in [
        ("all-settled", None),
        ("first-bad", Some(0)),
        ("second-bad", Some(1)),
    ] {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
            // Stage both physicals first; record names (hashes)
            // decide the sweep order.
            let mut staged = Vec::new();
            for stem in ["aaa", "zzz"] {
                let destination = root
                    .join(format!("{stem}.conf"))
                    .to_string_lossy()
                    .into_owned();
                let physical = stage(&root, &format!("work-{stem}/app.conf"), b"old");
                let record = PathBuf::from(format!("{manifest}.replace.{}", hash_of(&destination)));
                staged.push((stem, destination, physical, record));
            }
            staged.sort_by(|a, b| a.3.cmp(&b.3));
            let mut paths = Vec::new();
            for (index, (stem, destination, physical, record)) in staged.iter().enumerate() {
                let good = bad_at != Some(index);
                if good {
                    let parent = physical.parent().expect("parent").to_path_buf();
                    let parent_identity = dot::temp::identity_string(
                        dot::temp::path_identity(&parent).expect("parent identity"),
                    );
                    let expected =
                        repos_overlays::replacement_identity(&root, physical).expect("identity");
                    let transaction = parent.join(".app.conf.dot-overlay-replace-v1");
                    let line = format!(
                        "{destination}\t{}\t{stem}-target\t{expected}\t{}\t{parent_identity}\n",
                        physical.to_string_lossy(),
                        transaction.to_string_lossy(),
                    );
                    std::fs::write(record, line.into_bytes()).expect("record");
                    chmod(record, 0o600);
                } else {
                    std::fs::write(record, b"garbage\n").expect("record");
                    chmod(record, 0o600);
                }
                paths.push(record.clone());
            }
            let snippet = format!(
                "export HOME={}; DOT_OVERLAY_MANIFEST={} _overlay_recover_replacements; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n",
                sq(&root.to_string_lossy()),
                sq(&manifest),
            );
            if side == "shell" {
                let (code, out, serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                assert!(serr.is_empty(), "recovers stderr for {name:?}: {serr:?}");
                let shell = String::from_utf8(out).expect("recovers dump");
                std::fs::write(home.join(format!("{name}.shell.out")), shell).expect("stash");
                // The shell side's own sweep order (hashes embed
                // side paths, so orders differ per side).
                let mut ordered: Vec<PathBuf> = paths.clone();
                ordered.sort();
                let order: Vec<String> = ordered
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                std::fs::write(home.join(format!("{name}.shell.order")), order.join("\n"))
                    .expect("stash");
            } else {
                let result = repos_overlays::recover_replacements(
                    &manifest,
                    euid,
                    &root,
                    &root,
                    &root.to_string_lossy(),
                    &tool,
                );
                // All three in-engine callers read `$REPLY` only on
                // failure, so a success reply (whatever helper ran
                // last) compares as rc alone.
                let shell =
                    std::fs::read_to_string(home.join(format!("{name}.shell.out"))).expect("stash");
                let shell_rc: i32 = shell
                    .lines()
                    .next()
                    .expect("rc line")
                    .strip_prefix("rc=")
                    .expect("rc")
                    .parse()
                    .expect("rc number");
                // Record names hash side-specific destinations, so
                // each side names its own failure; both must name
                // their sorted `bad_at` record with rc 1.
                match (result, bad_at) {
                    (Ok(()), None) => {
                        assert_eq!(shell_rc, 0, "shell rc for {name:?}");
                    }
                    (Err(failed), Some(bad)) => {
                        assert_eq!(shell_rc, 1, "shell rc for {name:?}");
                        assert_eq!(
                            failed,
                            paths[bad].to_string_lossy(),
                            "rust names its bad record for {name:?}"
                        );
                        let shell_order =
                            std::fs::read_to_string(home.join(format!("{name}.shell.order")))
                                .expect("stash");
                        let shell_bad: Vec<&str> = shell_order.lines().collect();
                        assert_eq!(
                            shell,
                            format!("rc=1\nreply={}\n", shell_bad[bad]),
                            "shell names its bad record for {name:?}"
                        );
                    }
                    (result, bad_at) => {
                        panic!("unexpected outcome for {name:?}: {result:?} vs {bad_at:?}");
                    }
                }
                // Settled records vanish; the bad record and
                // anything past it stay.
                for (index, path) in paths.iter().enumerate() {
                    let settled = bad_at.is_none_or(|bad| index < bad);
                    assert_eq!(
                        path.symlink_metadata().is_ok(),
                        !settled,
                        "record fate for {name:?} {}",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn publish_link_agrees() {
    let dir = TempDir::new("ovlink-publish").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    for name in [
        "fresh-absent",
        "fresh-parent-missing",
        "fresh-occupied",
        "replace-ok",
        "replace-stale",
        "replace-transaction-blocked",
        "replace-recover-leftover",
        "replace-record-unrecoverable",
    ] {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
            let parent = root.join("sub/dir");
            let destination = parent.join("app.conf").to_string_lossy().into_owned();
            let target = "want-target";
            // Stage per case; the expected identity derives from
            // the live destination file where one exists.
            let mut with_expected = false;
            match name {
                "fresh-absent" => {
                    std::fs::create_dir_all(&parent).expect("parent");
                }
                "fresh-occupied" => {
                    std::fs::create_dir_all(&parent).expect("parent");
                    std::fs::write(parent.join("app.conf"), b"user\n").expect("file");
                }
                "replace-ok"
                | "replace-stale"
                | "replace-transaction-blocked"
                | "replace-recover-leftover"
                | "replace-record-unrecoverable" => {
                    std::fs::create_dir_all(&parent).expect("parent");
                    std::fs::write(parent.join("app.conf"), b"old").expect("file");
                    with_expected = true;
                }
                _ => {}
            }
            let expected = if with_expected {
                Some(
                    repos_overlays::replacement_identity(&root, &parent.join("app.conf"))
                        .expect("identity"),
                )
            } else {
                None
            };
            let expected = if name == "replace-stale" {
                std::fs::write(parent.join("app.conf"), b"new").expect("rewrite");
                expected
            } else {
                expected
            };
            if name == "replace-transaction-blocked" {
                std::fs::create_dir_all(parent.join(".app.conf.dot-overlay-replace-v1"))
                    .expect("txn");
            }
            if name == "replace-recover-leftover" || name == "replace-record-unrecoverable" {
                let record = PathBuf::from(format!("{manifest}.replace.{}", hash_of(&destination)));
                if name == "replace-recover-leftover" {
                    // A settled leftover: physical matches, no
                    // transaction, so recovery clears it.
                    let physical = parent.join("app.conf");
                    let physical_parent = physical.parent().expect("parent").to_path_buf();
                    let parent_identity = dot::temp::identity_string(
                        dot::temp::path_identity(&physical_parent).expect("parent identity"),
                    );
                    let settled =
                        repos_overlays::replacement_identity(&root, &physical).expect("identity");
                    let transaction = physical_parent.join(".app.conf.dot-overlay-replace-v1");
                    let line = format!(
                        "{destination}\t{}\t{target}\t{settled}\t{}\t{parent_identity}\n",
                        physical.to_string_lossy(),
                        transaction.to_string_lossy(),
                    );
                    std::fs::write(&record, line.into_bytes()).expect("record");
                } else {
                    std::fs::write(&record, b"garbage\n").expect("record");
                }
                chmod(&record, 0o600);
            }
            let expected_arg = expected.clone().unwrap_or_default();
            let snippet = if with_expected {
                format!(
                    "export HOME={}; DOT_OVERLAY_MANIFEST={} _overlay_publish_link {} {} {}; code=$?; printf 'rc=%s\\n' \"$code\"\n",
                    sq(&root.to_string_lossy()),
                    sq(&manifest),
                    sq(target),
                    sq(&destination),
                    sq(&expected_arg),
                )
            } else {
                format!(
                    "export HOME={}; DOT_OVERLAY_MANIFEST={} _overlay_publish_link {} {}; code=$?; printf 'rc=%s\\n' \"$code\"\n",
                    sq(&root.to_string_lossy()),
                    sq(&manifest),
                    sq(target),
                    sq(&destination),
                )
            };
            // Aftermath probe shared by both sides: destination
            // shape, record presence, transaction presence, and
            // stage leftovers under the parent (which may not
            // exist in the missing-parent case).
            let probe = format!(
                "d=absent; if [[ -L {} ]]; then d=\"link:$(readlink {})\"; elif [[ -f {} ]]; then d=\"file:$(cat {})\"; elif [[ -e {} ]]; then d=other; fi; rec=missing; [[ -e {} || -L {} ]] && rec=present; txn=missing; [[ -e {} || -L {} ]] && txn=present; stages=0; if [[ -d {} ]]; then for s in {}/.app.conf.overlay-link.*; do [[ -e $s || -L $s ]] && stages=$((stages+1)); done; fi; printf 'dest=%s\\nrecord=%s\\ntransaction=%s\\nstages=%s\\n' \"$d\" \"$rec\" \"$txn\" \"$stages\"\n",
                sq(&destination),
                sq(&destination),
                sq(&destination),
                sq(&destination),
                sq(&destination),
                sq(&format!("{manifest}.replace.{}", hash_of(&destination))),
                sq(&format!("{manifest}.replace.{}", hash_of(&destination))),
                sq(&parent
                    .join(".app.conf.dot-overlay-replace-v1")
                    .to_string_lossy()),
                sq(&parent
                    .join(".app.conf.dot-overlay-replace-v1")
                    .to_string_lossy()),
                sq(&parent.to_string_lossy()),
                // The directory stays quoted while the appended
                // glob expands.
                sq(&parent.to_string_lossy()),
            );
            if side == "shell" {
                let (code, out, serr) = shell_run(&root, &[], &format!("{snippet}{probe}"));
                assert_eq!(code, 0, "harness exit for {name:?}");
                assert!(serr.is_empty(), "publish stderr for {name:?}: {serr:?}");
                let shell = String::from_utf8(out).expect("publish dump");
                std::fs::write(home.join(format!("{name}.shell.out")), shell).expect("stash");
            } else {
                let twin_inputs = inputs(&root);
                let link = repos_overlays::PublishLinkInputs {
                    target,
                    destination: &destination,
                    expected: expected.as_deref(),
                    inputs: &twin_inputs,
                    manifest: &manifest,
                    euid,
                    source_root: &root,
                    tmp: &root,
                    tool: &tool,
                };
                let ok = repos_overlays::publish_link(&link);
                let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
                let destination_path = Path::new(&destination);
                let dest_state = match std::fs::symlink_metadata(destination_path) {
                    Err(_) => "absent".to_string(),
                    Ok(meta) if meta.file_type().is_symlink() => format!(
                        "link:{}",
                        std::fs::read_link(destination_path)
                            .map(|link| link.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    Ok(meta) if meta.is_file() => {
                        let body = std::fs::read_to_string(destination_path).unwrap_or_default();
                        format!("file:{}", body.trim_end_matches('\n'))
                    }
                    Ok(_) => "other".to_string(),
                };
                let record_path =
                    PathBuf::from(format!("{manifest}.replace.{}", hash_of(&destination)));
                let transaction_path = parent.join(".app.conf.dot-overlay-replace-v1");
                let mut stages = 0;
                if parent.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&parent) {
                        for entry in entries.filter_map(|entry| entry.ok()) {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if name.starts_with(".app.conf.overlay-link.") {
                                stages += 1;
                            }
                        }
                    }
                }
                rust.push_str(&format!(
                    "dest={dest_state}\nrecord={}\ntransaction={}\nstages={stages}\n",
                    if record_path.symlink_metadata().is_ok() {
                        "present"
                    } else {
                        "missing"
                    },
                    if transaction_path.symlink_metadata().is_ok() {
                        "present"
                    } else {
                        "missing"
                    },
                ));
                let shell =
                    std::fs::read_to_string(home.join(format!("{name}.shell.out"))).expect("stash");
                assert_eq!(rust, shell, "publish link for {name:?}");
            }
        }
    }
}
