//! Differential parity tests for the init transaction-directory
//! lifecycle (`lib/dot/init-client.sh`, part 1) against the live
//! shell: state-root resolution, the transaction/completed paths,
//! private directory setup, stage preparation, the ownership gate,
//! orphan recovery, and publication.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so random
//! stage names normalize before comparing.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_transaction as init;
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the init transaction chapter: the shared temp
/// helpers (content hash, stat probes, move without replace), the
/// XDG resolver, and the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the init runtime sourced. The locale
/// stays pinned: `stat` and `mktemp` diagnostics must read English
/// on both engines, and the port pins `LC_ALL=C` around every probe
/// like the shell helpers do.
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
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
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Crate root: `DOT_SOURCE_ROOT` for the shell hash oracle and
/// `source_root` for the Rust content check.
fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Shell fragment printing the `stat` mode of one squeezed path,
/// with the BSD fallback the shell helpers use.
fn mode_of(path: &Path) -> String {
    let quoted = sq(&path.to_string_lossy());
    format!("stat -c '%a' {quoted} 2>/dev/null || stat -f '%Lp' {quoted} 2>/dev/null")
}

/// Twin homes: disjoint directories so random stage names never
/// collide across engines.
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&shell_home).expect("shell home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        Self {
            _dir: dir,
            shell_home,
            rust_home,
        }
    }
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

#[test]
fn state_root_resolution() {
    let twins = Twins::build("init-txn-state-root");
    // Explicit XDG state home row.
    let xdg_sh = twins.shell_home.join("xstate");
    let xdg_rs = twins.rust_home.join("xstate");
    let snippet = format!(
        "export XDG_STATE_HOME={}\n_dot_init_state_root; code=$?; printf 'code=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n",
        sq(&xdg_sh.to_string_lossy())
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    let want_sh = format!("{}/dot/init", xdg_sh.display());
    assert_eq!(
        (code, String::from_utf8_lossy(&out).into_owned()),
        (0, format!("code=0\nreply={want_sh}\n"))
    );
    let want_rs = format!("{}/dot/init", xdg_rs.display());
    assert_eq!(
        init::state_root(
            &twins.rust_home.to_string_lossy(),
            &xdg_rs.to_string_lossy()
        ),
        Ok(want_rs)
    );
    // Fallback row: XDG unset, HOME owns the base on both sides.
    let snippet =
        "_dot_init_state_root; code=$?; printf 'code=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n";
    let (code, out, _) = shell_run(&twins.shell_home, snippet);
    let want_sh = format!("{}/.local/state/dot/init", twins.shell_home.display());
    assert_eq!(
        (code, String::from_utf8_lossy(&out).into_owned()),
        (0, format!("code=0\nreply={want_sh}\n"))
    );
    let want_rs = format!("{}/.local/state/dot/init", twins.rust_home.display());
    assert_eq!(
        init::state_root(&twins.rust_home.to_string_lossy(), ""),
        Ok(want_rs)
    );
}

#[test]
fn transaction_and_completed_paths() {
    let twins = Twins::build("init-txn-paths");
    let snippet = "_dot_init_transaction_dir; printf 'txn=%s\\n' \"$REPLY\"; _dot_init_completed_file; printf 'completed=%s\\n' \"$REPLY\"\n";
    let (code, out, _) = shell_run(&twins.shell_home, snippet);
    let home = twins.shell_home.display().to_string();
    assert_eq!(
        (code, String::from_utf8_lossy(&out).into_owned()),
        (
            0,
            format!(
                "txn={home}/.local/state/dot/init/transaction\ncompleted={home}/.local/state/dot/init/completed\n"
            )
        )
    );
    let home = twins.rust_home.to_string_lossy().into_owned();
    assert_eq!(
        init::transaction_dir(&home, ""),
        Ok(format!("{home}/.local/state/dot/init/transaction"))
    );
    assert_eq!(
        init::completed_file(&home, ""),
        Ok(format!("{home}/.local/state/dot/init/completed"))
    );
}

#[test]
fn private_directory_lifecycle() {
    let twins = Twins::build("init-txn-privdir");
    // Fresh nested path, then a repair row over a lax mode.
    let fresh_sh = twins.shell_home.join("a/b");
    let snippet = format!(
        "p={fresh}\n_dot_init_private_directory \"$p\"; echo \"fresh=$?\"; {mode_fresh}\nchmod 755 \"$p\"\n_dot_init_private_directory \"$p\"; echo \"repair=$?\"; {mode_repair}\n",
        fresh = sq(&fresh_sh.to_string_lossy()),
        mode_fresh = mode_of(&fresh_sh),
        mode_repair = mode_of(&fresh_sh),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    let fresh_rs = twins.rust_home.join("a/b");
    let mut rust_out = String::new();
    let fresh_ok = init::private_directory(&fresh_rs);
    rust_out.push_str(&format!(
        "fresh={}\n{:o}\n",
        i32::from(!fresh_ok),
        dot::temp::file_mode(&fresh_rs).expect("stage mode")
    ));
    chmod(&fresh_rs, 0o755);
    let repair_ok = init::private_directory(&fresh_rs);
    rust_out.push_str(&format!(
        "repair={}\n{:o}\n",
        i32::from(!repair_ok),
        dot::temp::file_mode(&fresh_rs).expect("repaired mode")
    ));
    assert_eq!(code, 0);
    assert_eq!(shell_out, rust_out);
    assert_eq!(rust_out, "fresh=0\n700\nrepair=0\n700\n");
}

#[test]
fn private_directory_rejects_non_directories() {
    let twins = Twins::build("init-txn-privdir-reject");
    let file_sh = twins.shell_home.join("file");
    std::fs::write(&file_sh, b"x").expect("blocker file");
    let real_sh = twins.shell_home.join("real");
    std::fs::create_dir(&real_sh).expect("link target");
    std::os::unix::fs::symlink(&real_sh, twins.shell_home.join("link")).expect("dir symlink");
    let snippet = format!(
        "_dot_init_private_directory {} 2>/dev/null; echo \"file=$?\"\n_dot_init_private_directory {} 2>/dev/null; echo \"link=$?\"\n",
        sq(&file_sh.to_string_lossy()),
        sq(&twins.shell_home.join("link").to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    let file_rs = twins.rust_home.join("file");
    std::fs::write(&file_rs, b"x").expect("blocker file");
    let real_rs = twins.rust_home.join("real");
    std::fs::create_dir(&real_rs).expect("link target");
    let link_rs = twins.rust_home.join("link");
    std::os::unix::fs::symlink(&real_rs, &link_rs).expect("dir symlink");
    let rust_out = format!(
        "file={}\nlink={}\n",
        i32::from(!init::private_directory(&file_rs)),
        i32::from(!init::private_directory(&link_rs)),
    );
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8_lossy(&out).into_owned(), rust_out);
    assert_eq!(rust_out, "file=1\nlink=1\n");
}

#[test]
fn prepare_creates_owned_stage() {
    let twins = Twins::build("init-txn-prepare");
    let txn_sh = twins.shell_home.join("transaction");
    let snippet = format!(
        "txn={txn}\n_dot_init_prepare_transaction \"$txn\"; echo \"code=$?\"\nstage=$REPLY; base=${{stage##*/}}; suffix=${{base#transaction.prepare.}}\necho \"prefix=${{base%.$suffix}}\"\necho \"suffix-len=${{#suffix}}\"\n_dot_init_transaction_stage_owned \"$stage\" 2>/dev/null; echo \"owned=$?\"\nstat -c '%a' \"$stage\" 2>/dev/null || stat -f '%Lp' \"$stage\" 2>/dev/null\nstat -c '%a' \"$stage/.dot-transaction-stage-v1\" 2>/dev/null || stat -f '%Lp' \"$stage/.dot-transaction-stage-v1\" 2>/dev/null\ncmp -s <(printf 'cgraf78 dot initialization preparation v1\\n') \"$stage/.dot-transaction-stage-v1\"; echo \"marker=$?\"\n",
        txn = sq(&txn_sh.to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    let txn_rs = twins.rust_home.join("transaction");
    let stage = init::prepare_transaction(&txn_rs).expect("rust prepare");
    let base = stage
        .file_name()
        .expect("stage name")
        .to_string_lossy()
        .into_owned();
    let suffix = base
        .strip_prefix("transaction.prepare.")
        .expect("stage prefix");
    let marker = stage.join(init::PREPARATION_MARKER_NAME);
    let rust_out = format!(
        "code=0\nprefix=transaction.prepare\nsuffix-len={}\nowned=0\n{:o}\n{:o}\nmarker=0\n",
        suffix.len(),
        dot::temp::file_mode(&stage).expect("stage mode"),
        dot::temp::file_mode(&marker).expect("marker mode"),
    );
    assert_eq!(code, 0);
    assert_eq!(shell_out, rust_out);
    assert!(init::transaction_stage_owned(&repo(), &stage));
    assert_eq!(
        std::fs::read(&marker).expect("marker bytes"),
        init::PREPARATION_MARKER
    );
}

#[test]
fn prepare_fails_when_parent_unmakable() {
    let twins = Twins::build("init-txn-prepare-fail");
    let blocker_sh = twins.shell_home.join("blocker");
    std::fs::write(&blocker_sh, b"x").expect("blocker file");
    let txn_sh = blocker_sh.join("transaction");
    let snippet = format!(
        "_dot_init_prepare_transaction {} 2>/dev/null; echo $?\n",
        sq(&txn_sh.to_string_lossy())
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    let blocker_rs = twins.rust_home.join("blocker");
    std::fs::write(&blocker_rs, b"x").expect("blocker file");
    let rust_code = i32::from(init::prepare_transaction(&blocker_rs.join("transaction")).is_err());
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8_lossy(&out).into_owned(),
        format!("{rust_code}\n")
    );
    assert_eq!(rust_code, 1);
}

/// Evaluate the ownership gate with both engines on one shared
/// absolute path; returns the shell verdict.
fn shell_owned(home: &Path, stage: &Path) -> bool {
    let snippet = format!(
        "if _dot_init_transaction_stage_owned {} 2>/dev/null; then echo 0; else echo 1; fi\n",
        sq(&stage.to_string_lossy())
    );
    let (code, out, _) = shell_run(home, &snippet);
    assert_eq!(code, 0);
    String::from_utf8_lossy(&out).trim() == "0"
}

#[test]
fn owned_gate_cross_engine() {
    let twins = Twins::build("init-txn-owned-xeng");
    // Shell-prepared stage validated by Rust.
    let txn_sh = twins.shell_home.join("transaction");
    let snippet = format!(
        "_dot_init_prepare_transaction {} 2>/dev/null || exit 1; printf '%s' \"$REPLY\"\n",
        sq(&txn_sh.to_string_lossy())
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    assert_eq!(code, 0);
    let shell_stage = PathBuf::from(std::ffi::OsStr::from_bytes(&out));
    assert!(init::transaction_stage_owned(&repo(), &shell_stage));
    // Rust-prepared stage validated by the shell.
    let txn_rs = twins.rust_home.join("transaction");
    let rust_stage = init::prepare_transaction(&txn_rs).expect("rust prepare");
    assert!(shell_owned(&twins.shell_home, &rust_stage));
}

#[test]
fn owned_gate_rejects_forgery() {
    let twins = Twins::build("init-txn-owned-forge");
    let root = twins._dir.path().join("cases");
    std::fs::create_dir_all(&root).expect("cases dir");
    // Each subcase builds a fixture, then both engines judge the
    // same absolute path: (shell verdict, rust verdict, expected).
    let mut row = 0;
    let mut check = |build: &dyn Fn(&Path) -> PathBuf, expected: bool| {
        row += 1;
        let case = root.join(format!("case-{row}"));
        std::fs::create_dir_all(&case).expect("case dir");
        let stage = build(&case);
        let shell_ok = shell_owned(&twins.shell_home, &stage);
        let rust_ok = init::transaction_stage_owned(&repo(), &stage);
        assert_eq!((shell_ok, rust_ok), (expected, expected), "row {row}");
    };
    // Valid control row.
    check(
        &|case| init::prepare_transaction(&case.join("transaction")).expect("prepare control"),
        true,
    );
    // Missing marker.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            std::fs::remove_file(stage.join(init::PREPARATION_MARKER_NAME)).expect("strip marker");
            stage
        },
        false,
    );
    // Wrong marker bytes.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            std::fs::write(stage.join(init::PREPARATION_MARKER_NAME), b"forged\n")
                .expect("forge marker");
            stage
        },
        false,
    );
    // Lax marker mode.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            chmod(&stage.join(init::PREPARATION_MARKER_NAME), 0o644);
            stage
        },
        false,
    );
    // Group-writable stage directory.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            chmod(&stage, 0o770);
            stage
        },
        false,
    );
    // Stage reached through a symlink.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            let alias = case.join("alias");
            std::os::unix::fs::symlink(&stage, &alias).expect("stage alias");
            alias
        },
        false,
    );
    // Marker reached through a symlink.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            let marker = stage.join(init::PREPARATION_MARKER_NAME);
            let elsewhere = case.join("marker-bytes");
            std::fs::write(&elsewhere, init::PREPARATION_MARKER).expect("marker bytes");
            std::fs::remove_file(&marker).expect("strip marker");
            std::os::unix::fs::symlink(&elsewhere, &marker).expect("marker alias");
            stage
        },
        false,
    );
    // Marker with a second hard link.
    check(
        &|case| {
            let stage = init::prepare_transaction(&case.join("transaction")).expect("prepare");
            let marker = stage.join(init::PREPARATION_MARKER_NAME);
            std::fs::hard_link(&marker, case.join("marker-twin")).expect("twin marker");
            stage
        },
        false,
    );
}

/// Presence probe: `kept` when the path exists (links count, like
/// the shell's `[[ -e || -L ]]`), else `gone`.
fn presence(path: &Path) -> &'static str {
    if path.symlink_metadata().is_ok() {
        "kept"
    } else {
        "gone"
    }
}

/// Static recovery witnesses: a forged stage (wrong marker bytes
/// but right modes), a junk file matching the prepare glob, and an
/// unrelated directory. All three must survive recovery on both
/// engines.
fn build_static_witnesses(dir: &Path) {
    let forged = dir.join("txn.prepare.forged");
    std::fs::create_dir(&forged).expect("forged stage");
    chmod(&forged, 0o700);
    let forged_marker = forged.join(init::PREPARATION_MARKER_NAME);
    std::fs::write(&forged_marker, b"forged\n").expect("forged marker");
    chmod(&forged_marker, 0o600);
    std::fs::write(dir.join("txn.prepare.junk"), b"junk\n").expect("junk file");
    let other = dir.join("txn.other");
    std::fs::create_dir(&other).expect("unrelated dir");
    std::fs::write(other.join("file"), b"data\n").expect("unrelated file");
}

/// Witness paths in report order: owned, forged, junk, other.
fn recover_witnesses(dir: &Path, owned: &Path) -> [PathBuf; 4] {
    [
        owned.to_path_buf(),
        dir.join("txn.prepare.forged"),
        dir.join("txn.prepare.junk"),
        dir.join("txn.other"),
    ]
}

#[test]
fn recover_keeps_only_unowned() {
    let twins = Twins::build("init-txn-recover");
    // Shell side: static witnesses from the parent, owned stage
    // prepared inside the child, then recover plus a presence
    // report in witness order.
    let dir_sh = twins.shell_home.join("work");
    std::fs::create_dir_all(&dir_sh).expect("shell work dir");
    let txn_sh = dir_sh.join("txn");
    build_static_witnesses(&dir_sh);
    let snippet = format!(
        "txn={txn}\n_dot_init_prepare_transaction \"$txn\" 2>/dev/null || exit 1\nowned=$REPLY\n_dot_init_recover_transaction_stages \"$txn\"; echo \"code=$?\"\nfor s in \"$owned\" \"$txn.prepare.forged\" \"$txn.prepare.junk\" \"$txn.other\"; do if [[ -e $s || -L $s ]]; then echo kept; else echo gone; fi; done\n",
        txn = sq(&txn_sh.to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    assert_eq!(code, 0);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    // Rust side mirrors it.
    let dir_rs = twins.rust_home.join("work");
    std::fs::create_dir_all(&dir_rs).expect("rust work dir");
    let txn_rs = dir_rs.join("txn");
    let owned_rs = init::prepare_transaction(&txn_rs).expect("rust prepare");
    build_static_witnesses(&dir_rs);
    let ok = init::recover_transaction_stages(&repo(), &txn_rs);
    let rust_out = format!(
        "code={}\n{}\n",
        i32::from(!ok),
        recover_witnesses(&dir_rs, &owned_rs)
            .iter()
            .map(|path| presence(path))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    assert_eq!(shell_out, rust_out);
    assert_eq!(rust_out, "code=0\ngone\nkept\nkept\nkept\n");
}

#[test]
fn recover_missing_parent_succeeds() {
    let twins = Twins::build("init-txn-recover-missing");
    let txn_sh = twins.shell_home.join("nope/txn");
    let snippet = format!(
        "_dot_init_recover_transaction_stages {} 2>/dev/null; echo $?\n",
        sq(&txn_sh.to_string_lossy())
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    let txn_rs = twins.rust_home.join("nope/txn");
    let rust_code = i32::from(!init::recover_transaction_stages(&repo(), &txn_rs));
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8_lossy(&out).into_owned(),
        format!("{rust_code}\n")
    );
    assert_eq!(rust_code, 0);
}

/// Publish report: code plus transaction/record/marker/stage
/// presence in a fixed order.
fn publish_report(code: bool, txn: &Path, stage: &Path) -> String {
    format!(
        "code={}\n{}\n",
        i32::from(!code),
        [
            txn.join("record"),
            txn.join(init::PREPARATION_MARKER_NAME),
            stage.to_path_buf(),
        ]
        .iter()
        .map(|path| presence(path))
        .collect::<Vec<_>>()
        .join("\n"),
    )
}

#[test]
fn publish_moves_stage() {
    let twins = Twins::build("init-txn-publish");
    // Shell side prepares, records, and publishes inside the child.
    let dir_sh = twins.shell_home.join("work");
    std::fs::create_dir_all(&dir_sh).expect("shell work dir");
    let txn_sh = dir_sh.join("txn");
    let snippet = format!(
        "txn={txn}\n_dot_init_prepare_transaction \"$txn\" 2>/dev/null || exit 1\nstage=$REPLY\nprintf 'record-v1\\n' >\"$stage/record\"\n_dot_init_publish_transaction \"$stage\" \"$txn\"; echo \"code=$?\"\nfor s in \"$txn/record\" \"$txn/.dot-transaction-stage-v1\" \"$stage\"; do if [[ -e $s || -L $s ]]; then echo kept; else echo gone; fi; done\n",
        txn = sq(&txn_sh.to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    assert_eq!(code, 0);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    // Rust side mirrors it.
    let dir_rs = twins.rust_home.join("work");
    std::fs::create_dir_all(&dir_rs).expect("rust work dir");
    let txn_rs = dir_rs.join("txn");
    let stage_rs = init::prepare_transaction(&txn_rs).expect("rust prepare");
    std::fs::write(stage_rs.join("record"), b"record-v1\n").expect("record");
    let mut cache = MoveCache::default();
    let ok = init::publish_transaction(&repo(), &stage_rs, &txn_rs, &mut cache);
    let rust_out = publish_report(ok, &txn_rs, &stage_rs);
    assert_eq!(shell_out, rust_out);
    assert_eq!(rust_out, "code=0\nkept\nkept\ngone\n");
    assert_eq!(
        std::fs::read(txn_rs.join("record")).expect("published record"),
        b"record-v1\n"
    );
}

#[test]
fn publish_rejects_missing_record() {
    let twins = Twins::build("init-txn-publish-norecord");
    let dir_sh = twins.shell_home.join("work");
    std::fs::create_dir_all(&dir_sh).expect("shell work dir");
    let txn_sh = dir_sh.join("txn");
    let snippet = format!(
        "txn={txn}\n_dot_init_prepare_transaction \"$txn\" 2>/dev/null || exit 1\nstage=$REPLY\n_dot_init_publish_transaction \"$stage\" \"$txn\" 2>/dev/null; echo \"code=$?\"\nif [[ -e $stage || -L $stage ]]; then echo kept; else echo gone; fi\n",
        txn = sq(&txn_sh.to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    assert_eq!(code, 0);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    let dir_rs = twins.rust_home.join("work");
    std::fs::create_dir_all(&dir_rs).expect("rust work dir");
    let txn_rs = dir_rs.join("txn");
    let stage_rs = init::prepare_transaction(&txn_rs).expect("rust prepare");
    let mut cache = MoveCache::default();
    let ok = init::publish_transaction(&repo(), &stage_rs, &txn_rs, &mut cache);
    let rust_out = format!("code={}\n{}\n", i32::from(!ok), presence(&stage_rs));
    assert_eq!(shell_out, rust_out);
    assert_eq!(rust_out, "code=1\nkept\n");
}

#[test]
fn publish_rejects_late_transaction() {
    let twins = Twins::build("init-txn-publish-late");
    // A foreign transaction directory already occupies the target.
    let dir_sh = twins.shell_home.join("work");
    std::fs::create_dir_all(&dir_sh).expect("shell work dir");
    let txn_sh = dir_sh.join("txn");
    std::fs::create_dir(&txn_sh).expect("late transaction");
    std::fs::write(txn_sh.join("sentinel"), b"foreign\n").expect("sentinel");
    let snippet = format!(
        "txn={txn}\n_dot_init_prepare_transaction \"$txn.prepare-holder\" 2>/dev/null || exit 1\nstage=$REPLY\nprintf 'record-v1\\n' >\"$stage/record\"\n_dot_init_publish_transaction \"$stage\" \"$txn\" 2>/dev/null; echo \"code=$?\"\nfor s in \"$stage\" \"$txn/sentinel\" \"$txn/record\"; do if [[ -e $s || -L $s ]]; then echo kept; else echo gone; fi; done\n",
        txn = sq(&txn_sh.to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    assert_eq!(code, 0);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    let dir_rs = twins.rust_home.join("work");
    std::fs::create_dir_all(&dir_rs).expect("rust work dir");
    let txn_rs = dir_rs.join("txn");
    std::fs::create_dir(&txn_rs).expect("late transaction");
    std::fs::write(txn_rs.join("sentinel"), b"foreign\n").expect("sentinel");
    let stage_rs =
        init::prepare_transaction(&dir_rs.join("txn.prepare-holder")).expect("rust prepare");
    std::fs::write(stage_rs.join("record"), b"record-v1\n").expect("record");
    let mut cache = MoveCache::default();
    let ok = init::publish_transaction(&repo(), &stage_rs, &txn_rs, &mut cache);
    let rust_out = format!(
        "code={}\n{}\n",
        i32::from(!ok),
        [
            stage_rs.clone(),
            txn_rs.join("sentinel"),
            txn_rs.join("record"),
        ]
        .iter()
        .map(|path| presence(path))
        .collect::<Vec<_>>()
        .join("\n"),
    );
    assert_eq!(shell_out, rust_out);
    assert_eq!(rust_out, "code=1\nkept\nkept\ngone\n");
}

#[test]
fn publish_rejects_forged_stage() {
    let twins = Twins::build("init-txn-publish-forged");
    let dir_sh = twins.shell_home.join("work");
    std::fs::create_dir_all(&dir_sh).expect("shell work dir");
    let txn_sh = dir_sh.join("txn");
    let forged_sh = dir_sh.join("txn.prepare.forged");
    std::fs::create_dir(&forged_sh).expect("forged stage");
    chmod(&forged_sh, 0o700);
    let forged_marker_sh = forged_sh.join(init::PREPARATION_MARKER_NAME);
    std::fs::write(&forged_marker_sh, b"forged\n").expect("forged marker");
    chmod(&forged_marker_sh, 0o600);
    std::fs::write(forged_sh.join("record"), b"record-v1\n").expect("record");
    let snippet = format!(
        "stage={stage} txn={txn}\n_dot_init_publish_transaction \"$stage\" \"$txn\" 2>/dev/null; echo \"code=$?\"\nif [[ -e $stage || -L $stage ]]; then echo kept; else echo gone; fi\n",
        stage = sq(&forged_sh.to_string_lossy()),
        txn = sq(&txn_sh.to_string_lossy()),
    );
    let (code, out, _) = shell_run(&twins.shell_home, &snippet);
    assert_eq!(code, 0);
    let shell_out = String::from_utf8_lossy(&out).into_owned();
    let dir_rs = twins.rust_home.join("work");
    std::fs::create_dir_all(&dir_rs).expect("rust work dir");
    let txn_rs = dir_rs.join("txn");
    let forged_rs = dir_rs.join("txn.prepare.forged");
    std::fs::create_dir(&forged_rs).expect("forged stage");
    chmod(&forged_rs, 0o700);
    let forged_marker_rs = forged_rs.join(init::PREPARATION_MARKER_NAME);
    std::fs::write(&forged_marker_rs, b"forged\n").expect("forged marker");
    chmod(&forged_marker_rs, 0o600);
    std::fs::write(forged_rs.join("record"), b"record-v1\n").expect("record");
    let mut cache = MoveCache::default();
    let ok = init::publish_transaction(&repo(), &forged_rs, &txn_rs, &mut cache);
    let rust_out = format!("code={}\n{}\n", i32::from(!ok), presence(&forged_rs));
    assert_eq!(shell_out, rust_out);
    assert_eq!(rust_out, "code=1\nkept\n");
}
