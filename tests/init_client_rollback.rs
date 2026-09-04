//! Differential parity tests for the init rollback family
//! (`lib/dot/init-client.sh` lines 1574-1710) against the live shell:
//! `_dot_init_rollback_entry`, `_dot_init_rollback_parents`,
//! `_dot_init_rollback_published`, and `_dot_init_rollback`.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories (plus
//! disjoint XDG state roots, since the transaction directory lives
//! under `dot_xdg_path state dot/init`), so intents, stages, parks,
//! backups, and transaction journals never collide.
//!
//! End-state inventories cover the home trees only. Transaction
//! journals embed side-specific bindings by construction (absolute
//! record paths, live device/inode numbers), so no byte comparison
//! across sides could ever hold for them; rows where the
//! transaction must vanish assert both sides explicitly instead.
//! Nothing under test writes inside the transaction — stages,
//! parks, containers, and the transaction directory itself are all
//! home-side or whole-directory effects — so the home inventory
//! plus the verdict pins every observable behavior.
//!
//! Every out-of-scope helper the four functions call crosses the
//! port as a closure. Rows marked `live` feed closures that run the
//! real shell helper in engine mode (`set -euo pipefail` inside a
//! subshell, exactly like the bare `_dot_init_rollback` call site),
//! so flag-sensitive paths (`$(<file)` reads, unguarded `rm -rf`)
//! behave identically on both engines. Rows marked `record` swap in
//! logging stubs to pin argument threading (identity strings, the
//! `${stage#"$HOME"/}/next` relative form, descending parent
//! order) and the three absorbed diagnostics.
//!
//! Error rows compare the exit verdict plus the full end-state
//! inventory: the shell prints `dot init: ...` diagnostics (and
//! occasional OS error text) that the port absorbs into `Err`, so
//! stderr bytes are asserted only where both engines are silent.

use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_rollback as rb;
use dot::test_support::{self, TempDir};

/// Sources for the rollback chapter: the resource runtime, the
/// shared temp helpers (identity, exclusive moves), the XDG root
/// (the transaction directory lives under it), and the init client
/// itself. Same set the plan lane sources.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell body with the rollback runtime sourced and report
/// the verdict the body left in `$?` alongside both byte streams.
/// The body always runs inside `( set -euo pipefail; ... )`, the
/// engine mode of the bare `_dot_init_rollback` call site, so a
/// failing statement stops the subshell exactly like the engine.
/// Every probe ends with `printf 'code=%s\n' "$?"`, so the returned
/// code is that verdict. A snippet that never reports (a harness
/// bug, never a pass) yields 99.
///
/// `LC_ALL=C` stays pinned (sort order, git diagnostics) and `HOME`
/// steers the worktree root; the `DOT_INIT_*` panel comes from the
/// row's record values so live helpers read the same globals on
/// both engines.
fn shell_run(home: &Path, env: &[(&str, &str)], body: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        "{SOURCES}( set -euo pipefail\n{body}\n ); printf 'code=%s\\n' \"$?\""
    ));
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
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    let verdict = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("code=")
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(99);
    (verdict, output.stdout, output.stderr)
}

/// Single-quote arbitrary bytes for snippet embedding. Fixture
/// words are UTF-8 in practice (hashes, numeric ids, ASCII paths),
/// so the lossy render here never fires on a real row.
fn sq(bytes: &[u8]) -> String {
    let mut quoted = String::from("'");
    quoted.push_str(&String::from_utf8_lossy(bytes).replace('\'', "'\\''"));
    quoted.push('\'');
    quoted
}

/// Twin engine roots: disjoint homes plus disjoint XDG state roots,
/// since the transaction directory lives under the state root, not
/// the home. Fixtures materialize under each side with per-side
/// absolute values (record journals, device/inode bindings).
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    shell_xdg: PathBuf,
    rust_xdg: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        let shell_xdg = dir.path().join("sh-xdg");
        let rust_xdg = dir.path().join("rs-xdg");
        for root in [&shell_home, &rust_home, &shell_xdg, &rust_xdg] {
            std::fs::create_dir_all(root).expect("engine root");
        }
        Self {
            _dir: dir,
            shell_home,
            rust_home,
            shell_xdg,
            rust_xdg,
        }
    }
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Write `bytes` to `dir/name`, creating parents.
fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Run git for fixtures; asserts success, silences output.
fn git(args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

/// Run git, feeding optional stdin under extra environment, and
/// return the chomped stdout. Asserts success, silences stderr.
fn git_output(args: &[&str], stdin_bytes: Option<&[u8]>, extra_env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "user.name=t", "-c", "user.email=t@t"]);
    cmd.args(args);
    cmd.env("LC_ALL", "C");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn git");
    if let Some(payload) = stdin_bytes {
        use std::io::Write as _;
        child
            .stdin
            .as_mut()
            .expect("git stdin")
            .write_all(payload)
            .expect("feed git");
    }
    let output = child.wait_with_output().expect("reap git");
    assert!(output.status.success(), "git {args:?}");
    let mut text = output.stdout;
    while text.last() == Some(&b'\n') {
        text.pop();
    }
    String::from_utf8(text).expect("git output text")
}

/// `git hash-object --stdin` over raw bytes, for intent names.
fn hash_bytes(payload: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git hash-object");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("hash stdin")
        .write_all(payload)
        .expect("feed hash");
    let output = child.wait_with_output().expect("reap hash");
    assert!(output.status.success(), "hash-object");
    let mut text = output.stdout;
    while text.last() == Some(&b'\n') {
        text.pop();
    }
    String::from_utf8(text).expect("hex hash")
}

/// One inventoried entry: relative path plus kind, mode, and
/// content identity, so end states compare as bytes.
fn inventory(roots: &[&Path]) -> Vec<u8> {
    fn walk(root: &Path, prefix: &str, out: &mut Vec<(Vec<u8>, Vec<u8>)>) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        let mut names: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        names.sort();
        for path in names {
            let name = path.file_name().expect("entry name");
            let mut rel = prefix.as_bytes().to_vec();
            rel.extend_from_slice(name.as_bytes());
            let meta = std::fs::symlink_metadata(&path).expect("fixture stat");
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            let mut desc = format!("{:04o} ", mode).into_bytes();
            if meta.is_dir() && !meta.is_symlink() {
                desc.extend_from_slice(b"dir");
                out.push((rel.clone(), desc));
                rel.push(b'/');
                walk(&path, &String::from_utf8_lossy(&rel), out);
            } else if meta.is_symlink() {
                desc.extend_from_slice(b"link->");
                desc.extend_from_slice(
                    std::fs::read_link(&path)
                        .expect("readlink")
                        .as_os_str()
                        .as_bytes(),
                );
                out.push((rel, desc));
            } else if meta.is_file() {
                desc.extend_from_slice(b"file:");
                match std::fs::read(&path) {
                    Ok(bytes) => desc.extend_from_slice(&bytes),
                    Err(_) => desc.extend_from_slice(b"<unreadable>"),
                }
                out.push((rel, desc));
            } else {
                desc.extend_from_slice(b"other");
                out.push((rel, desc));
            }
        }
    }
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        walk(root, &format!("root{index}/"), &mut rows);
    }
    rows.sort();
    let mut out = Vec::new();
    for (rel, desc) in rows {
        out.extend_from_slice(&rel);
        out.push(b'\t');
        out.extend_from_slice(&desc);
        out.push(b'\n');
    }
    out
}

/// Per-engine record values: absolute paths differ per side, so
/// each engine gets its own journal. `git_dev`/`git_ino` stay `-`
/// (the record allows the dash pair), keeping fixtures free of
/// device probes.
struct Rec {
    origin: String,
    identity: String,
    branch: String,
    commit: String,
    git_dir: PathBuf,
    backup: String,
    dot: String,
    dot_revision: String,
    nonce: String,
    git_dev: String,
    git_ino: String,
}

impl Rec {
    fn new(home: &Path, commit: &str, backup: &str) -> Self {
        Self {
            origin: "https://example.test/dotfiles".to_string(),
            identity: "example-test-identity".to_string(),
            branch: "main".to_string(),
            commit: commit.to_string(),
            git_dir: home.join(".dotfiles"),
            backup: backup.to_string(),
            dot: "/tmp/dot-rollback-src".to_string(),
            dot_revision: "ab".repeat(20),
            nonce: "t-nonce.1".to_string(),
            git_dev: "-".to_string(),
            git_ino: "-".to_string(),
        }
    }

    /// `DOT_INIT_*` panel plus `XDG_STATE_HOME`, so live helpers
    /// read the same globals the engine would hold post-record.
    fn panel<'a>(&'a self, _home: &'a Path, xdg: &'a Path) -> Vec<(&'static str, &'a str)> {
        vec![
            ("DOT_INIT_NONCE", self.nonce.as_str()),
            ("DOT_INIT_COMMIT", self.commit.as_str()),
            ("DOT_INIT_IDENTITY", self.identity.as_str()),
            ("DOT_INIT_BRANCH", self.branch.as_str()),
            (
                "DOT_INIT_GIT_DIR",
                self.git_dir.to_str().expect("utf8 git dir"),
            ),
            ("DOT_INIT_GIT_DEV", self.git_dev.as_str()),
            ("DOT_INIT_GIT_INO", self.git_ino.as_str()),
            ("DOT_INIT_BACKUP", self.backup.as_str()),
            ("XDG_STATE_HOME", xdg.to_str().expect("utf8 xdg")),
        ]
    }
}

/// Ask the live shell for the transaction directory under this
/// engine's state root, then create it. Both engines derive the
/// same relative layout from different absolute roots.
fn transaction_dir(home: &Path, xdg: &Path) -> PathBuf {
    let body = "_dot_init_transaction_dir\nprintf 'reply=%s\\n' \"$REPLY\"".to_string();
    let env = [("XDG_STATE_HOME", xdg.to_str().expect("utf8 xdg"))];
    let (code, stdout, _) = shell_run(home, &env, &body);
    assert_eq!(code, 0, "transaction dir");
    let text = String::from_utf8_lossy(&stdout);
    let reply = text
        .lines()
        .find_map(|line| line.strip_prefix("reply="))
        .expect("reply line");
    let dir = PathBuf::from(reply);
    std::fs::create_dir_all(&dir).expect("make transaction");
    dir
}

/// Write a 14-line transaction journal the live `read_record`
/// accepts.
fn write_record(home: &Path, transaction: &Path, rec: &Rec, phase: &str) {
    let home_str = home.to_str().expect("utf8 home");
    let body = format!(
        "cgraf78 dot initialization transaction v1\n\
         phase={phase}\n\
         origin={}\n\
         identity={}\n\
         branch={}\n\
         commit={}\n\
         git_dir={}\n\
         worktree={home_str}\n\
         backup={}\n\
         dot={}\n\
         dot_revision={}\n\
         nonce={}\n\
         git_dev={}\n\
         git_ino={}\n",
        rec.origin,
        rec.identity,
        rec.branch,
        rec.commit,
        rec.git_dir.to_str().expect("utf8 git dir"),
        rec.backup,
        rec.dot,
        rec.dot_revision,
        rec.nonce,
        rec.git_dev,
        rec.git_ino,
    );
    let path = transaction.join("record");
    std::fs::write(&path, body).expect("write record");
    chmod(&path, 0o600);
}

/// Stage relative path for an entry, plus its intent hash:
/// `_dot_init_entry_stage` over `path`.
fn entry_stage(nonce: &str, path: &str) -> (String, String) {
    let hash = hash_bytes(path.as_bytes());
    let stage = match path.rfind('/') {
        Some(cut) => format!("{}/.dot-init-entry.{nonce}.{hash}", &path[..cut]),
        None => format!(".dot-init-entry.{nonce}.{hash}"),
    };
    (hash, stage)
}

/// Write a publish-intent journal (`pending`, `staged`, or
/// `prepared`): nine tab fields, trailing newline, private mode.
#[allow(clippy::too_many_arguments)] // positional parity with the journal layout
fn write_intent(
    transaction: &Path,
    path: &str,
    mode: &str,
    oid: &str,
    stage: &str,
    phase: &str,
    dev: &str,
    ino: &str,
    next_dev: &str,
    next_ino: &str,
) {
    let hash = hash_bytes(path.as_bytes());
    let line =
        format!("{phase}\t{mode}\t{oid}\t{path}\t{stage}\t{dev}\t{ino}\t{next_dev}\t{next_ino}\n");
    let file = transaction.join(format!("publish-intent.{hash}"));
    std::fs::write(&file, line).expect("write intent");
    chmod(&file, 0o600);
}

/// Exact claim-file bytes the stage gates compare.
fn claim_bytes(kind: &str, nonce: &str, path: &str) -> Vec<u8> {
    format!("cgraf78 dot publication stage claim v1\nkind={kind}\nnonce={nonce}\npath={path}\n")
        .into_bytes()
}

/// Provision a stage directory: private mode, optional claim, plus
/// optional extra leaves (`next` with content, stray files).
fn make_stage(
    home: &Path,
    stage_rel: &str,
    claim: Option<(&str, &str, &str)>,
    extras: &[(&str, &[u8])],
) -> PathBuf {
    let stage = home.join(stage_rel);
    std::fs::create_dir_all(&stage).expect("make stage");
    chmod(&stage, 0o700);
    if let Some((kind, nonce, path)) = claim {
        let marker = stage.join(".dot-init-stage-claim-v1");
        std::fs::write(&marker, claim_bytes(kind, nonce, path)).expect("write claim");
        chmod(&marker, 0o600);
    }
    for (name, content) in extras {
        std::fs::write(stage.join(name), content).expect("write extra");
    }
    stage
}

/// `dev:ino` identity text for prepared journals.
fn identity_of(path: &Path) -> String {
    let identity = dot::temp::path_identity(path).expect("stat identity");
    dot::temp::identity_string(identity)
}

/// Write a parent-intent journal: six tab fields, trailing
/// newline, private mode.
fn write_parent_intent(
    transaction: &Path,
    parent: &str,
    phase: &str,
    stage_rel: &str,
    dev: &str,
    ino: &str,
    mode: &str,
) {
    let hash = hash_bytes(parent.as_bytes());
    let line = format!("{phase}\t{parent}\t{stage_rel}\t{dev}\t{ino}\t{mode}\n");
    let file = transaction.join(format!("parent-intent.{hash}"));
    std::fs::write(&file, line).expect("write parent intent");
    chmod(&file, 0o600);
}

/// Stage relative path for a parent, mirroring
/// `_dot_init_parent_directories`: the stage sits beside the parent
/// directory's own parent.
fn parent_stage(nonce: &str, parent: &str) -> (String, String) {
    let hash = hash_bytes(parent.as_bytes());
    let stage = match parent.rfind('/') {
        Some(cut) => format!("{}/.dot-init-parent.{nonce}.{hash}", &parent[..cut]),
        None => format!(".dot-init-parent.{nonce}.{hash}"),
    };
    (hash, stage)
}

/// Record context for entry probes: the entry chapter reads only
/// the git directory and commit out of the run identity.
fn entry_ctx(rec: &Rec) -> rb::RecordCtx {
    rb::RecordCtx {
        phase: String::new(),
        backup: PathBuf::from("-"),
        nonce: rec.nonce.clone(),
        git_dir: rec.git_dir.clone(),
        commit: rec.commit.clone(),
        git_identity: "-:-".to_string(),
    }
}

/// One engine side of an entry row: record values, transaction
/// directory, and intent path for `path`.
fn entry_side(home: &Path, xdg: &Path, commit: &str) -> (Rec, PathBuf, ()) {
    let rec = Rec::new(home, commit, "-");
    let transaction = transaction_dir(home, xdg);
    (rec, transaction, ())
}

#[test]
fn entry_pending_without_stage_is_vacuous() {
    let twins = Twins::build("e1-vacuous");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_parity(
        "e1",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_pending_with_clean_stage_removes_it() {
    let twins = Twins::build("e2-clean-stage");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        make_stage(home, &stage, Some(("entry", &rec.nonce, path)), &[]);
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert!(rust.is_ok(), "e2: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "e2: rust stage removed"
    );
    assert_parity(
        "e2",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_pending_with_next_present_refuses() {
    let twins = Twins::build("e3-next-present");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        make_stage(
            home,
            &stage,
            Some(("entry", &rec.nonce, path)),
            &[("next", b"staged bytes")],
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e3",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_staged_discards_next_and_removes_stage() {
    let twins = Twins::build("e4-staged");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        let stage_abs = make_stage(
            home,
            &stage,
            Some(("entry", &rec.nonce, path)),
            &[("next", b"staged bytes")],
        );
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "staged",
            dev,
            ino,
            "-",
            "-",
        );
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert!(rust.is_ok(), "e4: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "e4: rust stage removed"
    );
    assert_parity(
        "e4",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_staged_with_foreign_stage_refuses() {
    let twins = Twins::build("e5-foreign-stage");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        make_stage(
            home,
            &stage,
            Some(("entry", &rec.nonce, path)),
            &[("next", b"staged bytes")],
        );
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "staged",
            "7",
            "8",
            "-",
            "-",
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e5",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_prepared_with_matching_next_removes_both() {
    let twins = Twins::build("e6-prepared-match");
    let content = b"tracked content";
    let oid = hash_bytes(content);
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        git(&["init", rec.git_dir.to_str().expect("utf8 git dir")]);
        let (_, stage) = entry_stage(&rec.nonce, path);
        let stage_abs = make_stage(
            home,
            &stage,
            Some(("entry", &rec.nonce, path)),
            &[("next", content)],
        );
        chmod(&stage_abs.join("next"), 0o644);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        let next_identity = identity_of(&stage_abs.join("next"));
        let (next_dev, next_ino) = next_identity.split_once(':').expect("dev:ino");
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "prepared",
            dev,
            ino,
            next_dev,
            next_ino,
        );
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert!(rust.is_ok(), "e6: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "e6: rust stage removed"
    );
    assert_parity(
        "e6",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_prepared_with_swapped_next_refuses() {
    let twins = Twins::build("e7-swapped-next");
    let content = b"tracked content";
    let oid = hash_bytes(content);
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        git(&["init", rec.git_dir.to_str().expect("utf8 git dir")]);
        let (_, stage) = entry_stage(&rec.nonce, path);
        let stage_abs = make_stage(
            home,
            &stage,
            Some(("entry", &rec.nonce, path)),
            &[("next", content)],
        );
        chmod(&stage_abs.join("next"), 0o644);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "prepared",
            dev,
            ino,
            "9",
            "10",
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e7",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_prepared_with_changed_next_refuses() {
    let twins = Twins::build("e8-changed-next");
    let oid = hash_bytes(b"original content");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        git(&["init", rec.git_dir.to_str().expect("utf8 git dir")]);
        let (_, stage) = entry_stage(&rec.nonce, path);
        let stage_abs = make_stage(
            home,
            &stage,
            Some(("entry", &rec.nonce, path)),
            &[("next", b"changed content")],
        );
        chmod(&stage_abs.join("next"), 0o644);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        let next_identity = identity_of(&stage_abs.join("next"));
        let (next_dev, next_ino) = next_identity.split_once(':').expect("dev:ino");
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "prepared",
            dev,
            ino,
            next_dev,
            next_ino,
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e8",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_pending_with_live_target_refuses() {
    let twins = Twins::build("e9-live-target");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        write(home, path, b"foreign content");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e9",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_prepared_removes_tracked_target_and_stage() {
    let twins = Twins::build("e10-tracked-target");
    let content = b"tracked content";
    let oid = hash_bytes(content);
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        git(&["init", rec.git_dir.to_str().expect("utf8 git dir")]);
        let (_, stage) = entry_stage(&rec.nonce, path);
        let stage_abs = make_stage(home, &stage, Some(("entry", &rec.nonce, path)), &[]);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write(home, path, content);
        chmod(&home.join(path), 0o644);
        let target_identity = identity_of(&home.join(path));
        let (next_dev, next_ino) = target_identity.split_once(':').expect("dev:ino");
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "prepared",
            dev,
            ino,
            next_dev,
            next_ino,
        );
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert!(rust.is_ok(), "e10: port must accept");
    assert!(
        !twins.rust_home.join(path).exists(),
        "e10: rust target removed"
    );
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "e10: rust stage removed"
    );
    assert_parity(
        "e10",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_missing_intent_refuses() {
    let twins = Twins::build("e12-missing-intent");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e12",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_without_intents_is_vacuous() {
    let twins = Twins::build("p1-vacuous");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_parity(
        "p1",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_pending_without_stage_is_vacuous() {
    let twins = Twins::build("p2-pending-vacuous");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_parity(
        "p2",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_pending_with_clean_stage_removes_it() {
    let twins = Twins::build("p3-clean-stage");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
        make_stage(home, &stage_rel, Some(("parent", &rec.nonce, parent)), &[]);
        sides.push((rec, transaction, stage_rel));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert!(rust.is_ok(), "p3: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "p3: rust stage removed"
    );
    assert_parity(
        "p3",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_prepared_with_claimed_stage_cleans_up() {
    let twins = Twins::build("p4-prepared-stage");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        let stage_abs = make_stage(home, &stage_rel, Some(("parent", &rec.nonce, parent)), &[]);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write_parent_intent(
            &transaction,
            parent,
            "prepared",
            &stage_rel,
            dev,
            ino,
            "700",
        );
        sides.push((rec, transaction, stage_rel));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert!(rust.is_ok(), "p4: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "p4: rust stage removed"
    );
    assert_parity(
        "p4",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_prepared_with_live_target_removes_it() {
    let twins = Twins::build("p5-live-target");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        let target = home.join(parent);
        std::fs::create_dir_all(&target).expect("make target");
        chmod(&target, 0o700);
        let identity = identity_of(&target);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write_parent_intent(
            &transaction,
            parent,
            "prepared",
            &stage_rel,
            dev,
            ino,
            "700",
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert!(rust.is_ok(), "p5: port must accept");
    assert!(
        !twins.rust_home.join(parent).exists(),
        "p5: rust target removed"
    );
    assert_parity(
        "p5",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

/// Pull the `reply=<bytes>` line out of a helper run.
fn reply_of(stdout: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(stdout);
    let line = text
        .lines()
        .find(|line| line.strip_prefix("reply=").is_some())
        .expect("reply line");
    line.as_bytes()["reply=".len()..].to_vec()
}

/// Refusal error for a live helper closure, naming the helper so a
/// row failure pinpoints the gate.
fn refused(helper: &'static str) -> dot::Error {
    let message = match helper {
        "entry_intent" => "live entry_intent refused",
        "delete_park_path" => "live delete_park_path refused",
        "remove_parked_leaf" => "live remove_parked_leaf refused",
        "entry_stage_valid" => "live entry_stage_valid refused",
        "stage_claim_matches" => "live stage_claim_matches refused",
        "entry_stage_only_next" => "live entry_stage_only_next refused",
        "discard_staged_next" => "live discard_staged_next refused",
        "candidate_matches_git" => "live candidate_matches_git refused",
        "stage_claim_remove" => "live stage_claim_remove refused",
        "parent_record" => "live parent_record refused",
        "safe_relative_path" => "live safe_relative_path refused",
        "remove_parked_parent" => "live remove_parked_parent refused",
        "private_directory_matches" => "live private_directory_matches refused",
        "stage_claim_only" => "live stage_claim_only refused",
        "private_empty_directory_matches" => "live private_empty_directory_matches refused",
        "remove_parked_tree" => "live remove_parked_tree refused",
        "transaction_dir" => "live transaction_dir refused",
        "read_record" => "live read_record refused",
        "restore_backups" => "live restore_backups refused",
        _ => "live shell helper refused",
    };
    dot::Error::Usage { message }
}

/// Twenty live closures running the real shell helpers in engine
/// mode: the differential oracle for the port's orchestration. Each
/// closure mirrors one out-of-scope call site argument for
/// argument; `$REPLY`-carried outputs surface as return values.
fn live_deps<'a>(home: &'a Path, xdg: &'a Path, rec: &'a Rec) -> rb::RollbackDeps<'a> {
    rb::RollbackDeps {
        entry_intent: Box::new(|intent, mode, oid, path| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_entry_intent {} {} {} {}\nprintf 'reply=%s\\n' \"$REPLY\"",
                sq(intent.as_os_str().as_bytes()),
                sq(mode.as_bytes()),
                sq(oid.as_bytes()),
                sq(path.as_os_str().as_bytes()),
            );
            let (code, stdout, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(reply_of(&stdout))
            } else {
                Err(refused("entry_intent"))
            }
        }),
        delete_park_path: Box::new(|target, kind, key| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_delete_park_path {} {} {}\nprintf 'reply=%s\\n' \"$REPLY\"",
                sq(target.as_os_str().as_bytes()),
                sq(kind.as_bytes()),
                sq(key),
            );
            let (code, stdout, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(PathBuf::from(std::ffi::OsString::from_vec(reply_of(
                    &stdout,
                ))))
            } else {
                Err(refused("delete_park_path"))
            }
        }),
        remove_parked_leaf: Box::new(|target, park, identity, git_dir, commit, mode, oid| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_delete_parked_generation {} {} leaf _dot_init_leaf_delete_matches {} {} {} {} {}",
                sq(target.as_os_str().as_bytes()),
                sq(park.as_os_str().as_bytes()),
                sq(identity.as_bytes()),
                sq(git_dir.as_os_str().as_bytes()),
                sq(commit.as_bytes()),
                sq(mode.as_bytes()),
                sq(oid.as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("remove_parked_leaf"))
            }
        }),
        entry_stage_valid: Box::new(|stage, identity| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_entry_stage_valid {} {}",
                sq(stage.as_os_str().as_bytes()),
                sq(identity.unwrap_or_default().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("entry_stage_valid"))
            }
        }),
        stage_claim_matches: Box::new(|stage, kind, path| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_stage_claim_matches {} {} {}",
                sq(stage.as_os_str().as_bytes()),
                sq(kind.as_bytes()),
                sq(path.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("stage_claim_matches"))
            }
        }),
        entry_stage_only_next: Box::new(|stage| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_entry_stage_only_next {}",
                sq(stage.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("entry_stage_only_next"))
            }
        }),
        discard_staged_next: Box::new(|stage| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_discard_staged_next {}",
                sq(stage.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("discard_staged_next"))
            }
        }),
        path_identity: Box::new(|path| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "value=$(_dot_path_identity {} 2>/dev/null || true)\nprintf 'reply=%s\\n' \"$value\"",
                sq(path.as_os_str().as_bytes()),
            );
            let (_, stdout, _) = shell_run(home, &env, &body);
            String::from_utf8(reply_of(&stdout)).expect("identity text")
        }),
        candidate_matches_git: Box::new(|git_dir, commit, mode, oid, rel| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_candidate_matches_git {} {} {} {} {}",
                sq(git_dir.as_os_str().as_bytes()),
                sq(commit.as_bytes()),
                sq(mode.as_bytes()),
                sq(oid.as_bytes()),
                sq(rel.as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("candidate_matches_git"))
            }
        }),
        stage_claim_remove: Box::new(|stage, kind, path| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_stage_claim_remove {} {} {}",
                sq(stage.as_os_str().as_bytes()),
                sq(kind.as_bytes()),
                sq(path.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("stage_claim_remove"))
            }
        }),
        parent_record: Box::new(|transaction, parent| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_parent_record {} {}\nprintf 'reply=%s\\n' \"$REPLY\"",
                sq(transaction.as_os_str().as_bytes()),
                sq(parent.as_os_str().as_bytes()),
            );
            let (code, stdout, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(reply_of(&stdout))
            } else {
                Err(refused("parent_record"))
            }
        }),
        safe_relative_path: Box::new(|parent| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_safe_relative_path {}",
                sq(parent.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("safe_relative_path"))
            }
        }),
        remove_parked_parent: Box::new(|target, park, identity, mode| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_delete_parked_generation {} {} parent _dot_init_parent_delete_matches {} {}",
                sq(target.as_os_str().as_bytes()),
                sq(park.as_os_str().as_bytes()),
                sq(identity.as_bytes()),
                sq(mode.as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("remove_parked_parent"))
            }
        }),
        private_directory_matches: Box::new(|stage, identity, mode| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_private_directory_matches {} {} {}",
                sq(stage.as_os_str().as_bytes()),
                sq(identity.unwrap_or_default().as_bytes()),
                sq(mode.unwrap_or_default().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("private_directory_matches"))
            }
        }),
        stage_claim_only: Box::new(|stage| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_stage_claim_only {}",
                sq(stage.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("stage_claim_only"))
            }
        }),
        private_empty_directory_matches: Box::new(|stage, identity, mode| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_private_empty_directory_matches {} {} {}",
                sq(stage.as_os_str().as_bytes()),
                sq(identity.unwrap_or_default().as_bytes()),
                sq(mode.unwrap_or_default().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("private_empty_directory_matches"))
            }
        }),
        remove_parked_tree: Box::new(|git_dir, park, identity| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_delete_parked_generation {} {} tree _dot_init_git_delete_matches {}",
                sq(git_dir.as_os_str().as_bytes()),
                sq(park.as_os_str().as_bytes()),
                sq(identity.as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("remove_parked_tree"))
            }
        }),
        transaction_dir: Box::new(|| {
            let env = rec.panel(home, xdg);
            let body = "_dot_init_transaction_dir\nprintf 'reply=%s\\n' \"$REPLY\"";
            let (code, stdout, _) = shell_run(home, &env, body);
            if code == 0 {
                Ok(PathBuf::from(std::ffi::OsString::from_vec(reply_of(
                    &stdout,
                ))))
            } else {
                Err(refused("transaction_dir"))
            }
        }),
        read_record: Box::new(|record| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_read_record {}\nprintf 'rec=%s\\t%s\\t%s\\t%s\\t%s\\t%s\\n' \
                 \"$DOT_INIT_PHASE\" \"$DOT_INIT_BACKUP\" \"$DOT_INIT_NONCE\" \
                 \"$DOT_INIT_GIT_DIR\" \"$DOT_INIT_COMMIT\" \"$DOT_INIT_GIT_DEV:$DOT_INIT_GIT_INO\"",
                sq(record.as_os_str().as_bytes()),
            );
            let (code, stdout, _) = shell_run(home, &env, &body);
            if code != 0 {
                return Err(refused("read_record"));
            }
            let text = String::from_utf8_lossy(&stdout);
            let line = text
                .lines()
                .find_map(|line| line.strip_prefix("rec="))
                .expect("rec line");
            let mut fields = line.splitn(6, '\t');
            let take = |fields: &mut std::str::SplitN<'_, char>| {
                fields.next().unwrap_or_default().to_string()
            };
            let phase = take(&mut fields);
            let backup = take(&mut fields);
            let nonce = take(&mut fields);
            let git_dir = take(&mut fields);
            let commit = take(&mut fields);
            let git_identity = take(&mut fields);
            Ok(rb::RecordCtx {
                phase,
                backup: PathBuf::from(backup),
                nonce,
                git_dir: PathBuf::from(git_dir),
                commit,
                git_identity,
            })
        }),
        restore_backups: Box::new(|backup| {
            let env = rec.panel(home, xdg);
            let body = format!(
                "_dot_init_restore_backups {}",
                sq(backup.as_os_str().as_bytes()),
            );
            let (code, _, _) = shell_run(home, &env, &body);
            if code == 0 {
                Ok(())
            } else {
                Err(refused("restore_backups"))
            }
        }),
    }
}

/// Oracle probe for `_dot_init_rollback_entry`, in engine mode.
fn oracle_entry(
    home: &Path,
    env: &[(&str, &str)],
    intent: &Path,
    mode: &str,
    oid: &str,
    path: &Path,
) -> (i32, Vec<u8>, Vec<u8>) {
    let body = format!(
        "_dot_init_rollback_entry {} {} {} {}",
        sq(intent.as_os_str().as_bytes()),
        sq(mode.as_bytes()),
        sq(oid.as_bytes()),
        sq(path.as_os_str().as_bytes()),
    );
    shell_run(home, env, &body)
}

/// Oracle probe for `_dot_init_rollback_parents`, in engine mode.
fn oracle_parents(
    home: &Path,
    env: &[(&str, &str)],
    transaction: &Path,
) -> (i32, Vec<u8>, Vec<u8>) {
    let body = format!(
        "_dot_init_rollback_parents {}",
        sq(transaction.as_os_str().as_bytes()),
    );
    shell_run(home, env, &body)
}

/// Oracle probe for `_dot_init_rollback_published`, in engine mode.
fn oracle_published(
    home: &Path,
    env: &[(&str, &str)],
    transaction: &Path,
) -> (i32, Vec<u8>, Vec<u8>) {
    let body = format!(
        "_dot_init_rollback_published {}",
        sq(transaction.as_os_str().as_bytes()),
    );
    shell_run(home, env, &body)
}

/// Oracle probe for `_dot_init_rollback`, in engine mode.
fn oracle_rollback(home: &Path, env: &[(&str, &str)]) -> (i32, Vec<u8>, Vec<u8>) {
    shell_run(home, env, "_dot_init_rollback")
}

/// The port's verdict as a shell-style code.
fn rust_code(result: &Result<(), dot::Error>) -> i32 {
    if result.is_ok() { 0 } else { 1 }
}

/// Byte-compare one success row: same verdict, silent streams on
/// both sides, same end-state inventory over both engine roots.
/// The oracle's stdout must hold exactly its own `code=` report
/// line (the functions print nothing themselves).
fn assert_parity(
    name: &str,
    shell: (i32, Vec<u8>, Vec<u8>),
    rust: &Result<(), dot::Error>,
    shell_stderr: &[u8],
    shell_inv: &[u8],
    rust_inv: &[u8],
) {
    let (code, stdout, _) = shell;
    assert_eq!(code, rust_code(rust), "{name}: verdict");
    assert_eq!(
        stdout,
        format!("code={code}\n").into_bytes(),
        "{name}: oracle stdout"
    );
    assert_eq!(shell_stderr, &[][..], "{name}: oracle stderr");
    assert_eq!(shell_inv, rust_inv, "{name}: end state");
}

/// Failure-row comparison with an exact expected oracle stderr
/// (empty, or one absorbed `dot init: ...` diagnostic line). The
/// port absorbs diagnostics into `Err`, so only the verdict and
/// the end state cross over.
fn assert_failure_parity(
    name: &str,
    shell: (i32, Vec<u8>, Vec<u8>),
    rust: &Result<(), dot::Error>,
    expected_stderr: &[u8],
    shell_inv: &[u8],
    rust_inv: &[u8],
) {
    let (code, stdout, stderr) = shell;
    assert_ne!(code, 0, "{name}: oracle must refuse");
    assert!(rust.is_err(), "{name}: port must refuse");
    assert_eq!(
        stdout,
        format!("code={code}\n").into_bytes(),
        "{name}: oracle stdout"
    );
    assert_eq!(stderr, expected_stderr, "{name}: oracle stderr");
    assert_eq!(shell_inv, rust_inv, "{name}: end state");
}

#[test]
fn parents_settled_intent_refuses() {
    let twins = Twins::build("p6-settled");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        write_parent_intent(&transaction, parent, "staged", &stage_rel, "-", "-", "-");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_failure_parity(
        "p6",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_escaping_intent_refuses() {
    let twins = Twins::build("p7-escape");
    let parent = "../evil";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let hash = hash_bytes(parent.as_bytes());
        let stage_rel = format!("../.dot-init-parent.{}.{hash}", rec.nonce);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_failure_parity(
        "p7",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_temporary_intent_is_skipped() {
    let twins = Twins::build("p8-tmp-skip");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
        let hash = hash_bytes(parent.as_bytes());
        std::fs::write(
            transaction.join(format!("parent-intent.{hash}.tmp.9")),
            b"garbage that must never parse\n",
        )
        .expect("write tmp");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_parity(
        "p8",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_stage_with_live_target_refuses() {
    let twins = Twins::build("p9-target-wins");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
        make_stage(home, &stage_rel, Some(("parent", &rec.nonce, parent)), &[]);
        let target = home.join(parent);
        std::fs::create_dir_all(&target).expect("make target");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_failure_parity(
        "p9",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_process_in_reverse_order() {
    let twins = Twins::build("p10-reverse-order");
    let candidates = ["ord-alpha", "ord-beta", "ord-gamma", "ord-delta"];
    let mut hashes: Vec<(&str, String)> = candidates
        .iter()
        .map(|parent| (*parent, hash_bytes(parent.as_bytes())))
        .collect();
    hashes.sort_by(|a, b| b.1.cmp(&a.1));
    let (hi, lo) = (hashes[0].0, hashes[1].0);
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        for parent in [hi, lo] {
            let (_, stage_rel) = parent_stage(&rec.nonce, parent);
            write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
            make_stage(home, &stage_rel, Some(("parent", &rec.nonce, parent)), &[]);
        }
        let target = home.join(lo);
        std::fs::create_dir_all(&target).expect("make target");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert!(rust.is_err(), "p10: port must refuse on {lo}");
    assert_failure_parity(
        "p10",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

/// Full run identity for published probes: every field the chapter
/// reads, with the git identity bound as `dev:ino` text.
fn full_ctx(rec: &Rec) -> rb::RecordCtx {
    rb::RecordCtx {
        phase: String::new(),
        backup: PathBuf::from(rec.backup.clone()),
        nonce: rec.nonce.clone(),
        git_dir: rec.git_dir.clone(),
        commit: rec.commit.clone(),
        git_identity: format!("{}:{}", rec.git_dev, rec.git_ino),
    }
}

/// Write `tree.tsv`: one `mode\toid\tpath` row per line.
fn write_tree(transaction: &Path, rows: &[(&str, &str, &str)]) {
    let mut body = Vec::new();
    for (mode, oid, path) in rows {
        body.extend_from_slice(format!("{mode}\t{oid}\t{path}\n").as_bytes());
    }
    std::fs::write(transaction.join("tree.tsv"), body).expect("write tree");
}

/// Provision a deterministic git generation AT `git_dir` itself:
/// the engine stages bare generations (then flips `core.bare`
/// off), so the marker and branch tip live directly under the git
/// directory. Loose plumbing objects plus fixed dates keep every
/// byte identical across engines so inventories still compare.
/// (`core.worktree` stays unset: nothing in this chapter reads it.)
fn make_generation(rec: &Rec) -> String {
    let git_dir = rec.git_dir.to_str().expect("utf8 git dir");
    git(&["init", "--quiet", "--bare", git_dir]);
    git(&[
        "--git-dir",
        git_dir,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    git(&[
        "--git-dir",
        git_dir,
        "config",
        "core.logAllRefUpdates",
        "false",
    ]);
    git(&["--git-dir", git_dir, "config", "core.bare", "false"]);
    let blob = git_output(
        &["--git-dir", git_dir, "hash-object", "-w", "--stdin"],
        Some(b"generation seed".as_slice()),
        &[],
    );
    let tree = git_output(
        &["--git-dir", git_dir, "mktree"],
        Some(format!("100644 blob {blob}\tseed.txt\n").as_bytes()),
        &[],
    );
    let commit = git_output(
        &[
            "--git-dir",
            git_dir,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit-tree",
            tree.as_str(),
            "-m",
            "seed",
        ],
        None,
        &[
            ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
        ],
    );
    git(&[
        "--git-dir",
        git_dir,
        "update-ref",
        "refs/heads/main",
        commit.as_str(),
    ]);
    commit
}

#[test]
fn published_without_tree_is_vacuous() {
    let twins = Twins::build("q1-vacuous");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert_parity(
        "q1",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn published_rolls_back_tree_entries() {
    let twins = Twins::build("q2-tree-entry");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        make_stage(home, &stage, Some(("entry", &rec.nonce, path)), &[]);
        write_tree(&transaction, &[("100644", &oid, path)]);
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert!(rust.is_ok(), "q2: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "q2: rust stage removed"
    );
    assert_parity(
        "q2",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn published_skips_entries_without_intent() {
    let twins = Twins::build("q3-skip-missing");
    let oid_a = hash_bytes(b"a body");
    let oid_b = hash_bytes(b"b body");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_a) = entry_stage(&rec.nonce, "a.txt");
        write_intent(
            &transaction,
            "a.txt",
            "100644",
            &oid_a,
            &stage_a,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        make_stage(home, &stage_a, Some(("entry", &rec.nonce, "a.txt")), &[]);
        write_tree(
            &transaction,
            &[("100644", &oid_a, "a.txt"), ("100644", &oid_b, "b.txt")],
        );
        sides.push((rec, transaction, stage_a));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert!(rust.is_ok(), "q3: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "q3: rust stage removed"
    );
    assert_parity(
        "q3",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn published_foreign_container_refuses() {
    let twins = Twins::build("q4-foreign-container");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let backup = home.join(".dot-backup").to_str().expect("utf8").to_string();
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let mut rec = rec;
        rec.backup = backup;
        let container = PathBuf::from(&rec.backup).join("git-stage");
        std::fs::create_dir_all(&container).expect("make container");
        write(
            &container,
            "identity",
            b"cgraf78 dot Git stage v1\nnonce=WRONG\n",
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert_failure_parity(
        "q4",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn published_owned_container_is_removed() {
    let twins = Twins::build("q5-owned-container");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let backup = home.join(".dot-backup").to_str().expect("utf8").to_string();
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let mut rec = rec;
        rec.backup = backup;
        let container = PathBuf::from(&rec.backup).join("git-stage");
        std::fs::create_dir_all(&container).expect("make container");
        let marker = format!("cgraf78 dot Git stage v1\nnonce={}\n", rec.nonce);
        write(&container, "identity", marker.as_bytes());
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert!(rust.is_ok(), "q5: port must accept");
    assert_parity(
        "q5",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn published_matching_git_generation_is_removed() {
    let twins = Twins::build("q6-git-generation");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (mut rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let tip = make_generation(&rec);
        rec.commit = tip.clone();
        let identity = identity_of(&rec.git_dir);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        rec.git_dev = dev.to_string();
        rec.git_ino = ino.to_string();
        let marker = format!(
            "cgraf78 dot client generation v1\nnonce={}\ncommit={tip}\nidentity={}\n",
            rec.nonce, rec.identity,
        );
        write(&rec.git_dir, "dot-init-generation-v1", marker.as_bytes());
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert!(rust.is_ok(), "q6: port must accept");
    assert_parity(
        "q6",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn published_unreadable_tree_rolls_on() {
    let twins = Twins::build("q7-unreadable-tree");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        write_tree(&transaction, &[("100644", &"ab".repeat(20), "a.txt")]);
        chmod(&transaction.join("tree.tsv"), 0o000);
        sides.push((rec, transaction, ()));
    }
    if std::fs::read(sides[0].1.join("tree.tsv")).is_ok() {
        eprintln!("q7 skipped: tree.tsv still readable (root?)");
        return;
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_published(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_published(&deps, &twins.rust_home, &full_ctx(rs_rec), rs_tx);
    assert_parity(
        "q7",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn rollback_without_record_refuses() {
    let twins = Twins::build("r1-no-record");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, _transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        sides.push(rec);
    }
    let sh_rec = &sides[0];
    let rs_rec = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_err(), "r1: port must refuse");
    assert_eq!(
        rust.as_ref().unwrap_err().to_string(),
        "no recoverable transaction",
        "r1: absorbed diagnostic"
    );
    assert_failure_parity(
        "r1",
        shell,
        &rust,
        b"dot init: no recoverable transaction\n",
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn rollback_committed_phase_refuses() {
    let twins = Twins::build("r2-committed");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        write_record(home, &transaction, &rec, "checkout");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, _, _) = &sides[0];
    let (rs_rec, _, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_err(), "r2: port must refuse");
    assert_eq!(
        rust.as_ref().unwrap_err().to_string(),
        "checkout is committed; rerun the original init command to resume",
        "r2: absorbed diagnostic"
    );
    assert_failure_parity(
        "r2",
        shell,
        &rust,
        b"dot init: checkout is committed; rerun the original init command to resume\n",
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn rollback_empty_publishing_removes_transaction() {
    let twins = Twins::build("r3-empty-ok");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        write_record(home, &transaction, &rec, "publishing");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_ok(), "r3: port must accept");
    assert!(!rs_tx.exists(), "r3: rust transaction removed");
    assert!(!sh_tx.exists(), "r3: shell transaction removed");
    assert_parity(
        "r3",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn rollback_full_publishing_cleans_everything() {
    let twins = Twins::build("r4-full");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        write_record(home, &transaction, &rec, "publishing");
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        make_stage(home, &stage, Some(("entry", &rec.nonce, path)), &[]);
        write_tree(&transaction, &[("100644", &oid, path)]);
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_ok(), "r4: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "r4: rust stage removed"
    );
    assert!(!rs_tx.exists(), "r4: rust transaction removed");
    assert!(!sh_tx.exists(), "r4: shell transaction removed");
    assert_parity(
        "r4",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn rollback_changed_tree_refuses_and_keeps_transaction() {
    let twins = Twins::build("r5-changed-tree");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let backup = home
            .join(".dot-backup/r5")
            .to_str()
            .expect("utf8")
            .to_string();
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let mut rec = rec;
        rec.backup = backup;
        write_record(home, &transaction, &rec, "publishing");
        let container = PathBuf::from(&rec.backup).join("git-stage");
        std::fs::create_dir_all(&container).expect("make container");
        write(
            &container,
            "identity",
            b"cgraf78 dot Git stage v1\nnonce=WRONG\n",
        );
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_err(), "r5: port must refuse");
    assert_eq!(
        rust.as_ref().unwrap_err().to_string(),
        "transaction-owned paths changed; refusing rollback",
        "r5: absorbed diagnostic"
    );
    assert!(
        rs_tx.exists(),
        "r5: rust transaction kept after published failure"
    );
    assert_failure_parity(
        "r5",
        shell,
        &rust,
        b"dot init: transaction-owned paths changed; refusing rollback\n",
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
    assert!(sh_tx.exists(), "r5: shell transaction kept");
}

#[test]
fn rollback_backed_up_with_manifestless_backup_succeeds() {
    let twins = Twins::build("r6-backed-up");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let backup = home
            .join(".dot-backup/r6")
            .to_str()
            .expect("utf8")
            .to_string();
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let mut rec = rec;
        rec.backup = backup;
        std::fs::create_dir_all(&rec.backup).expect("make backup");
        write_record(home, &transaction, &rec, "backed-up");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_ok(), "r6: port must accept");
    assert!(!rs_tx.exists(), "r6: rust transaction removed");
    assert!(!sh_tx.exists(), "r6: shell transaction removed");
    assert_parity(
        "r6",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn rollback_complete_phase_refuses() {
    let twins = Twins::build("r7-complete");
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        write_record(home, &transaction, &rec, "complete");
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, _, _) = &sides[0];
    let (rs_rec, _, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_rollback(&twins.shell_home, &sh_env);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback(&deps, &twins.rust_home);
    assert!(rust.is_err(), "r7: port must refuse");
    assert_failure_parity(
        "r7",
        shell,
        &rust,
        b"dot init: checkout is committed; rerun the original init command to resume\n",
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

/// Canned outcomes for the recording stubs: fail-closed (refuse
/// unless armed), so each row pins exactly the calls it needs.
#[derive(Default)]
struct Behavior {
    entry_reply: Option<Vec<u8>>,
    park: Option<PathBuf>,
    leaf_ok: bool,
    stage_valid_ok: bool,
    claim_ok: bool,
    only_next_ok: bool,
    discard_ok: bool,
    identity: String,
    candidate_ok: bool,
    claim_remove_ok: bool,
    parent_reply: Option<Vec<u8>>,
    relative_ok: bool,
    parent_ok: bool,
    dir_ok: bool,
    claim_only_ok: bool,
    empty_ok: bool,
    tree_ok: bool,
    transaction: Option<PathBuf>,
    record: Option<rb::RecordCtx>,
    restore_ok: bool,
}

/// Ordered call log for the recording stubs.
struct Log {
    calls: std::cell::RefCell<Vec<String>>,
}

impl Log {
    fn new() -> Self {
        Self {
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn push(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }

    fn take(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

fn stub_error() -> dot::Error {
    dot::Error::Usage {
        message: "stub refused",
    }
}

/// Twenty recording stubs: every out-of-scope call appends its
/// exact argument vector and returns the canned outcome.
fn stub_deps<'a>(log: &'a Log, behavior: &'a Behavior) -> rb::RollbackDeps<'a> {
    rb::RollbackDeps {
        entry_intent: Box::new(|intent, mode, oid, path| {
            log.push(format!(
                "entry_intent {} {mode} {oid} {}",
                intent.display(),
                path.display()
            ));
            behavior.entry_reply.clone().ok_or_else(stub_error)
        }),
        delete_park_path: Box::new(|target, kind, key| {
            log.push(format!(
                "delete_park_path {} {kind} {}",
                target.display(),
                String::from_utf8_lossy(key)
            ));
            behavior.park.clone().ok_or_else(stub_error)
        }),
        remove_parked_leaf: Box::new(|target, park, identity, git_dir, commit, mode, oid| {
            log.push(format!(
                "remove_parked_leaf {} {} {identity} {} {commit} {mode} {oid}",
                target.display(),
                park.display(),
                git_dir.display()
            ));
            if behavior.leaf_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        entry_stage_valid: Box::new(|stage, identity| {
            log.push(format!(
                "entry_stage_valid {} {}",
                stage.display(),
                identity.unwrap_or_default()
            ));
            if behavior.stage_valid_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        stage_claim_matches: Box::new(|stage, kind, path| {
            log.push(format!(
                "stage_claim_matches {} {kind} {}",
                stage.display(),
                path.display()
            ));
            if behavior.claim_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        entry_stage_only_next: Box::new(|stage| {
            log.push(format!("entry_stage_only_next {}", stage.display()));
            if behavior.only_next_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        discard_staged_next: Box::new(|stage| {
            log.push(format!("discard_staged_next {}", stage.display()));
            if behavior.discard_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        path_identity: Box::new(|path| {
            log.push(format!("path_identity {}", path.display()));
            behavior.identity.clone()
        }),
        candidate_matches_git: Box::new(|git_dir, commit, mode, oid, rel| {
            log.push(format!(
                "candidate_matches_git {} {commit} {mode} {oid} {rel}",
                git_dir.display()
            ));
            if behavior.candidate_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        stage_claim_remove: Box::new(|stage, kind, path| {
            log.push(format!(
                "stage_claim_remove {} {kind} {}",
                stage.display(),
                path.display()
            ));
            if behavior.claim_remove_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        parent_record: Box::new(|transaction, parent| {
            log.push(format!(
                "parent_record {} {}",
                transaction.display(),
                parent.display()
            ));
            behavior.parent_reply.clone().ok_or_else(stub_error)
        }),
        safe_relative_path: Box::new(|parent| {
            log.push(format!("safe_relative_path {}", parent.display()));
            if behavior.relative_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        remove_parked_parent: Box::new(|target, park, identity, mode| {
            log.push(format!(
                "remove_parked_parent {} {} {identity} {mode}",
                target.display(),
                park.display()
            ));
            if behavior.parent_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        private_directory_matches: Box::new(|stage, identity, mode| {
            log.push(format!(
                "private_directory_matches {} {} {}",
                stage.display(),
                identity.unwrap_or_default(),
                mode.unwrap_or_default()
            ));
            if behavior.dir_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        stage_claim_only: Box::new(|stage| {
            log.push(format!("stage_claim_only {}", stage.display()));
            if behavior.claim_only_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        private_empty_directory_matches: Box::new(|stage, identity, mode| {
            log.push(format!(
                "private_empty_directory_matches {} {} {}",
                stage.display(),
                identity.unwrap_or_default(),
                mode.unwrap_or_default()
            ));
            if behavior.empty_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        remove_parked_tree: Box::new(|git_dir, park, identity| {
            log.push(format!(
                "remove_parked_tree {} {} {identity}",
                git_dir.display(),
                park.display()
            ));
            if behavior.tree_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
        transaction_dir: Box::new(|| {
            log.push("transaction_dir".to_string());
            behavior.transaction.clone().ok_or_else(stub_error)
        }),
        read_record: Box::new(|record| {
            log.push(format!("read_record {}", record.display()));
            behavior
                .record
                .as_ref()
                .map(stub_record)
                .ok_or_else(stub_error)
        }),
        restore_backups: Box::new(|backup| {
            log.push(format!("restore_backups {}", backup.display()));
            if behavior.restore_ok {
                Ok(())
            } else {
                Err(stub_error())
            }
        }),
    }
}

fn stub_record(record: &rb::RecordCtx) -> rb::RecordCtx {
    rb::RecordCtx {
        phase: record.phase.clone(),
        backup: record.backup.clone(),
        nonce: record.nonce.clone(),
        git_dir: record.git_dir.clone(),
        commit: record.commit.clone(),
        git_identity: record.git_identity.clone(),
    }
}

fn stub_ctx() -> rb::RecordCtx {
    rb::RecordCtx {
        phase: String::new(),
        backup: PathBuf::from("/stub/backup"),
        nonce: "stub-nonce".to_string(),
        git_dir: PathBuf::from("/stub/git"),
        commit: "cd".repeat(20),
        git_identity: "-:-".to_string(),
    }
}

#[test]
fn stub_entry_prepared_without_stage_calls_two_helpers() {
    let dir = TempDir::new("s1-prepared-args").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("make home");
    let log = Log::new();
    let fake_park = home.join(".park");
    let behavior = Behavior {
        entry_reply: Some(b"prepared\tw/stage\t11\t22\t33\t44".to_vec()),
        park: Some(fake_park),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let intent = home.join("intent");
    let result = rb::rollback_entry(
        &deps,
        &home,
        &stub_ctx(),
        &intent,
        "100644",
        &"ab".repeat(20),
        Path::new("w/file"),
    );
    assert!(result.is_ok(), "s1: port must accept");
    assert_eq!(
        log.take(),
        vec![
            format!(
                "entry_intent {} 100644 {} w/file",
                intent.display(),
                "ab".repeat(20)
            ),
            format!("delete_park_path {}/w/file leaf w/file", home.display()),
        ],
        "s1: exact call sequence"
    );
}

#[test]
fn stub_entry_prepared_next_threads_identity_and_relative() {
    let dir = TempDir::new("s1b-prepared-next").expect("temp dir");
    let home = dir.path().join("home");
    let stage = home.join("s1b/w/stage");
    std::fs::create_dir_all(&stage).expect("make stage");
    std::fs::write(stage.join("next"), b"bytes").expect("write next");
    let log = Log::new();
    let fake_park = home.join(".park");
    let behavior = Behavior {
        entry_reply: Some(b"prepared\ts1b/w/stage\t11\t22\t33\t44".to_vec()),
        park: Some(fake_park),
        stage_valid_ok: true,
        claim_ok: true,
        only_next_ok: true,
        identity: "33:44".to_string(),
        candidate_ok: true,
        claim_remove_ok: true,
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let intent = home.join("intent");
    let result = rb::rollback_entry(
        &deps,
        &home,
        &stub_ctx(),
        &intent,
        "100644",
        &"ab".repeat(20),
        Path::new("s1b/file"),
    );
    assert!(result.is_ok(), "s1b: port must accept");
    assert!(!stage.exists(), "s1b: stage removed");
    let oid = "ab".repeat(20);
    assert_eq!(
        log.take(),
        vec![
            format!("entry_intent {} 100644 {oid} s1b/file", intent.display()),
            format!("delete_park_path {}/s1b/file leaf s1b/file", home.display()),
            format!("entry_stage_valid {}/s1b/w/stage 11:22", home.display()),
            format!(
                "stage_claim_matches {}/s1b/w/stage entry s1b/file",
                home.display()
            ),
            format!("entry_stage_only_next {}/s1b/w/stage", home.display()),
            format!("path_identity {}/s1b/w/stage/next", home.display()),
            format!(
                "candidate_matches_git /stub/git {} 100644 {oid} s1b/w/stage/next",
                "cd".repeat(20)
            ),
            format!(
                "stage_claim_remove {}/s1b/w/stage entry s1b/file",
                home.display()
            ),
        ],
        "s1b: exact call sequence"
    );
}

#[test]
fn stub_parents_visit_records_in_reverse() {
    let dir = TempDir::new("s2-parents-order").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("make home");
    let transaction = dir.path().join("tx");
    std::fs::create_dir_all(&transaction).expect("make tx");
    let parents = ["ord-one", "ord-two", "ord-three"];
    for parent in parents {
        let (_, stage_rel) = parent_stage("n", parent);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
    }
    let log = Log::new();
    let fake_park = home.join(".park");
    let behavior = Behavior {
        parent_reply: None,
        relative_ok: true,
        park: Some(fake_park),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let result = rb::rollback_parents(&deps, &home, &transaction);
    assert!(result.is_err(), "s2: parent_record stub refuses");
    let mut lines: Vec<String> = parents
        .iter()
        .map(|parent| {
            let hash = hash_bytes(parent.as_bytes());
            let file = transaction.join(format!("parent-intent.{hash}"));
            format!("{parent}\t{}", file.display())
        })
        .collect();
    lines.sort_by(|a, b| b.cmp(a));
    let expected: Vec<String> = lines
        .iter()
        .map(|line| {
            let parent = line.split('\t').next().expect("parent field");
            format!("parent_record {} {parent}", transaction.display())
        })
        .collect();
    let calls = log.take();
    let records: Vec<String> = calls
        .iter()
        .filter(|call| call.starts_with("parent_record"))
        .cloned()
        .collect();
    assert_eq!(records.len(), 1, "s2: stops at first refusal");
    assert_eq!(records, vec![expected[0].clone()], "s2: reverse order");
}

#[test]
fn stub_rollback_maps_diagnostics() {
    let dir = TempDir::new("s3-diagnostics").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("make home");
    let transaction = dir.path().join("tx");
    std::fs::create_dir_all(&transaction).expect("make tx");

    let log = Log::new();
    let behavior = Behavior::default();
    let deps = stub_deps(&log, &behavior);
    let result = rb::rollback(&deps, &home);
    assert!(result.is_err(), "s3a: transaction_dir refusal propagates");
    assert_eq!(log.take(), vec!["transaction_dir".to_string()]);

    let log = Log::new();
    let behavior = Behavior {
        transaction: Some(transaction.clone()),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let result = rb::rollback(&deps, &home);
    assert_eq!(
        result.unwrap_err().to_string(),
        "no recoverable transaction",
        "s3b: unreadable record maps"
    );

    let log = Log::new();
    let mut ctx = stub_ctx();
    ctx.phase = "checkout".to_string();
    let behavior = Behavior {
        transaction: Some(transaction.clone()),
        record: Some(stub_record(&ctx)),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let result = rb::rollback(&deps, &home);
    assert_eq!(
        result.unwrap_err().to_string(),
        "checkout is committed; rerun the original init command to resume",
        "s3c: committed phase maps"
    );

    let log = Log::new();
    let mut ctx = stub_ctx();
    ctx.phase = "publishing".to_string();
    let row = format!("100644\t{}\ta.txt\n", "ab".repeat(20));
    std::fs::write(transaction.join("tree.tsv"), row).expect("write tree");
    let hash = hash_bytes(b"a.txt");
    std::fs::write(transaction.join(format!("publish-intent.{hash}")), b"").expect("intent");
    let behavior = Behavior {
        transaction: Some(transaction.clone()),
        record: Some(stub_record(&ctx)),
        park: Some(home.join(".park")),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let result = rb::rollback(&deps, &home);
    assert_eq!(
        result.unwrap_err().to_string(),
        "transaction-owned paths changed; refusing rollback",
        "s3d: published failure maps"
    );
    let calls = log.take();
    assert!(
        calls.iter().any(|call| call.starts_with("entry_intent")),
        "s3d: entry attempted, got {calls:?}"
    );
}

#[test]
fn stub_published_walks_tree_in_reverse() {
    let dir = TempDir::new("s4-tree-order").expect("temp dir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("make home");
    let transaction = dir.path().join("tx");
    std::fs::create_dir_all(&transaction).expect("make tx");
    let rows = [
        ("100644", "ab".repeat(20), "a.txt"),
        ("100644", "cd".repeat(20), "b.txt"),
    ];
    let mut body = Vec::new();
    for (mode, oid, path) in &rows {
        body.extend_from_slice(format!("{mode}\t{oid}\t{path}\n").as_bytes());
        let hash = hash_bytes(path.as_bytes());
        std::fs::write(transaction.join(format!("publish-intent.{hash}")), b"").expect("intent");
    }
    std::fs::write(transaction.join("tree.tsv"), body).expect("write tree");
    let log = Log::new();
    let behavior = Behavior {
        entry_reply: Some(b"pending\tgone/stage\t-\t-\t-\t-".to_vec()),
        park: Some(home.join(".park")),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let result = rb::rollback_published(&deps, &home, &stub_ctx(), &transaction);
    assert!(result.is_ok(), "s4: port must accept");
    let calls = log.take();
    let entries: Vec<String> = calls
        .iter()
        .filter(|call| call.starts_with("entry_intent"))
        .cloned()
        .collect();
    let hash_a = hash_bytes(b"a.txt");
    let hash_b = hash_bytes(b"b.txt");
    assert_eq!(
        entries,
        vec![
            format!(
                "entry_intent {}/publish-intent.{hash_b} 100644 {} b.txt",
                transaction.display(),
                "cd".repeat(20)
            ),
            format!(
                "entry_intent {}/publish-intent.{hash_a} 100644 {} a.txt",
                transaction.display(),
                "ab".repeat(20)
            ),
        ],
        "s4: tree walks in reverse"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("parent_record")),
        "s4: no parents without intents, got {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("delete_park_path") && call.contains(" git ")),
        "s4: git park computed, got {calls:?}"
    );
}

#[test]
fn stub_entry_unknown_phase_refuses_after_park() {
    let dir = TempDir::new("s1c-unknown-phase").expect("temp dir");
    let home = dir.path().join("home");
    let stage = home.join("w/stage");
    std::fs::create_dir_all(&stage).expect("make stage");
    let log = Log::new();
    let behavior = Behavior {
        entry_reply: Some(b"bogus\tw/stage\t11\t22\t33\t44".to_vec()),
        park: Some(home.join(".park")),
        ..Default::default()
    };
    let deps = stub_deps(&log, &behavior);
    let intent = home.join("intent");
    let result = rb::rollback_entry(
        &deps,
        &home,
        &stub_ctx(),
        &intent,
        "100644",
        &"ab".repeat(20),
        Path::new("w/file"),
    );
    assert!(result.is_err(), "s1c: unknown phase must refuse");
    assert!(stage.exists(), "s1c: stage kept");
    assert_eq!(
        log.take(),
        vec![
            format!(
                "entry_intent {} 100644 {} w/file",
                intent.display(),
                "ab".repeat(20)
            ),
            format!("delete_park_path {}/w/file leaf w/file", home.display()),
        ],
        "s1c: park precedes the phase dispatch"
    );
}

#[test]
fn entry_pending_claimless_stage_refuses() {
    let twins = Twins::build("e13-claimless");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "pending",
            "-",
            "-",
            "-",
            "-",
        );
        make_stage(home, &stage, None, &[]);
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert_failure_parity(
        "e13",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn entry_staged_without_next_removes_stage() {
    let twins = Twins::build("e14-staged-no-next");
    let oid = hash_bytes(b"note body");
    let path = "docs/note.txt";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage) = entry_stage(&rec.nonce, path);
        let stage_abs = make_stage(home, &stage, Some(("entry", &rec.nonce, path)), &[]);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write_intent(
            &transaction,
            path,
            "100644",
            &oid,
            &stage,
            "staged",
            dev,
            ino,
            "-",
            "-",
        );
        sides.push((rec, transaction, stage));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_intent = sh_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let rs_intent = rs_tx.join(format!("publish-intent.{}", hash_bytes(path.as_bytes())));
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_entry(
        &twins.shell_home,
        &sh_env,
        &sh_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_entry(
        &deps,
        &twins.rust_home,
        &entry_ctx(rs_rec),
        &rs_intent,
        "100644",
        &oid,
        Path::new(path),
    );
    assert!(rust.is_ok(), "e14: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "e14: rust stage removed"
    );
    assert_parity(
        "e14",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_prepared_claimless_stage_cleans_up() {
    let twins = Twins::build("p11-prepared-no-claim");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        let stage_abs = make_stage(home, &stage_rel, None, &[]);
        let identity = identity_of(&stage_abs);
        let (dev, ino) = identity.split_once(':').expect("dev:ino");
        write_parent_intent(
            &transaction,
            parent,
            "prepared",
            &stage_rel,
            dev,
            ino,
            "700",
        );
        sides.push((rec, transaction, stage_rel));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, rs_stage) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert!(rust.is_ok(), "p11: port must accept");
    assert!(
        !twins.rust_home.join(rs_stage).exists(),
        "p11: rust stage removed"
    );
    assert_parity(
        "p11",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}

#[test]
fn parents_pending_claimless_stage_refuses() {
    let twins = Twins::build("p12-pending-no-claim");
    let parent = "docs";
    let mut sides = Vec::new();
    for (home, xdg) in [
        (&twins.shell_home, &twins.shell_xdg),
        (&twins.rust_home, &twins.rust_xdg),
    ] {
        let (rec, transaction, _) = entry_side(home, xdg, &"cd".repeat(20));
        let (_, stage_rel) = parent_stage(&rec.nonce, parent);
        write_parent_intent(&transaction, parent, "pending", &stage_rel, "-", "-", "-");
        make_stage(home, &stage_rel, None, &[]);
        sides.push((rec, transaction, ()));
    }
    let (sh_rec, sh_tx, _) = &sides[0];
    let (rs_rec, rs_tx, _) = &sides[1];
    let sh_panel = sh_rec.panel(&twins.shell_home, &twins.shell_xdg);
    let sh_env: Vec<(&str, &str)> = sh_panel.iter().map(|(k, v)| (*k, *v)).collect();
    let shell = oracle_parents(&twins.shell_home, &sh_env, sh_tx);
    let deps = live_deps(&twins.rust_home, &twins.rust_xdg, rs_rec);
    let rust = rb::rollback_parents(&deps, &twins.rust_home, rs_tx);
    assert_failure_parity(
        "p12",
        shell,
        &rust,
        &[],
        &inventory(&[&twins.shell_home]),
        &inventory(&[&twins.rust_home]),
    );
}
