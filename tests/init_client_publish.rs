//! Differential parity tests for the init publish family
//! (`lib/dot/init-client.sh` lines 1279-1417) against the live shell:
//! the published-stage and prepared-intent validators, the stage
//! reaper, the worktree publisher, the convergence entry, and the
//! single-origin reader.
//!
//! Separate binary because most rows drive real filesystem state:
//! the two engines work under disjoint home directories, so stages,
//! intents, journals, backups, and git bindings never collide.
//! Cross-lane collaborators (`entry_intent`, `prior_record`,
//! `candidate_matches_git`, `path_state_matches`, `publish_intent`,
//! `publish_one`, and the stage gates) cross the port as closures;
//! every row feeds closures that run the real shell predicates, so
//! only the six ported functions can diverge. Pure predicates share
//! one fixture across engines; mutating rows build twin fixtures and
//! compare end-state shapes alongside the verdict codes.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_publish as publish;
use dot::test_support::TempDir;

/// Sources for the publish chapter: the resource runtime, the shared
/// temp helpers (identity, moves, modes), the XDG root, and the init
/// client itself — the same oracle stack the plan lane uses.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Pinned run nonce: stage names and claim bodies derive from it, so
/// both sides build identical layouts.
const NONCE: &str = "7.parity";

/// Run one shell snippet with the publish runtime sourced and report
/// the verdict the snippet printed alongside both byte streams.
/// Every probe ends with `printf 'code=%s\n' "$code"`, so the
/// returned code is that verdict — not the process status, which
/// only says the printer ran. A snippet that never reports (a
/// harness bug, never a pass) yields 99.
///
/// Extra bindings (run nonce, git binding, backup root) cross per
/// row; the locale stays pinned like the port pins it around every
/// git run.
fn shell_run(home: &Path, env: &[(&str, &str)], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so journals, stages, and git
/// bindings never collide across engines.
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

/// Assert two byte strings are identical, pinpointing the first
/// diverging offset instead of dumping full arrays into CI logs.
fn assert_bytes_eq(left: &[u8], right: &[u8], what: &str) {
    if left == right {
        return;
    }
    let offset = left
        .iter()
        .zip(right.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(left.len().min(right.len()));
    panic!(
        "{what}: byte divergence at offset {offset} (lengths {} vs {}): {left:?} vs {right:?}",
        left.len(),
        right.len()
    );
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

/// Run git for fixtures; asserts success, silences output. Fixed
/// identity and dates keep twin repositories content-identical.
fn git_run(args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

/// `stat -c '%d:%i'` identity string of one path, read in-process:
/// GNU `stat` has no macOS spelling and the BSD fallback differs, so
/// shelling out breaks the macOS gate (lane-67 `MetadataExt` pattern).
fn identity_of(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).expect("stat fixture");
    format!("{}:{}", meta.dev(), meta.ino())
}

/// Lexical existence: the observable end-state of a row on one side.
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Read one side-relative file the byte way, for shape comparison.
fn shape(home: &Path, rel: &str) -> (bool, Vec<u8>) {
    let path = home.join(rel);
    let present = exists(&path);
    let content = std::fs::read(&path).unwrap_or_default();
    (present, content)
}

/// Snapshot one home-relative path the shell way, via the live
/// `_dot_init_snapshot_path`: returns the manifest row
/// (`rel\tkind\tdev\tino\tmode\tsize\tvalue`) as the
/// behavior-neutral oracle for row construction. Row bytes come from
/// the shell on both sides, so only the orchestration under test can
/// diverge.
fn snapshot_row(home: &Path, rel: &str) -> String {
    let body = format!(
        "if row=$(_dot_init_snapshot_path {}); then code=0; else code=$?; row=; fi\nprintf 'row=%s\\ncode=%s\\n' \"$row\" \"$code\"\n",
        sq(home.join(rel).to_str().expect("fixture path"))
    );
    let (code, out, _) = shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &body);
    assert_eq!(code, 0, "snapshot {rel}");
    let text = String::from_utf8_lossy(&out);
    let row = text
        .lines()
        .find_map(|line| line.strip_prefix("row="))
        .unwrap_or_default();
    assert!(!row.is_empty(), "snapshot row {rel}");
    format!("{rel}\t{row}")
}

/// Entry stage path for `path` under `home`, via the live
/// `_dot_init_entry_stage`.
fn entry_stage(home: &Path, path: &str) -> PathBuf {
    let body = format!(
        "if _dot_init_entry_stage {}; then code=0; else code=$?; REPLY=; fi\nprintf 'stage=%s\\ncode=%s\\n' \"$REPLY\" \"$code\"\n",
        sq(path)
    );
    let (code, out, _) = shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &body);
    assert_eq!(code, 0, "entry stage {path}");
    let text = String::from_utf8_lossy(&out);
    let stage = text
        .lines()
        .find_map(|line| line.strip_prefix("stage="))
        .unwrap_or_default();
    assert!(!stage.is_empty(), "stage path {path}");
    PathBuf::from(stage)
}

/// Live private-directory gate: runs the real shell predicate per
/// call, so rows exercise true end-to-end parity.
fn live_private_directory_matches(home: PathBuf) -> impl Fn(&Path, &str, &str) -> bool {
    move |path, identity, mode| {
        let body = format!(
            "if _dot_init_private_directory_matches {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(path.to_str().expect("gate path")),
            sq(identity),
            sq(mode),
        );
        shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0
    }
}

/// Live stage-content gate: the real `_dot_init_entry_stage_only_next`.
fn live_stage_only_next(home: PathBuf) -> impl Fn(&Path) -> bool {
    move |stage| {
        let body = format!(
            "if _dot_init_entry_stage_only_next {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(stage.to_str().expect("gate path")),
        );
        shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0
    }
}

/// Live claim-content gate: the real `_dot_init_stage_claim_matches`.
fn live_stage_claim_matches(home: PathBuf) -> impl Fn(&Path, &str, &str) -> bool {
    move |stage, kind, path| {
        let body = format!(
            "if _dot_init_stage_claim_matches {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(stage.to_str().expect("gate path")),
            sq(kind),
            sq(path),
        );
        shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0
    }
}

/// Live empty-directory gate: the real
/// `_dot_init_private_empty_directory_matches`.
fn live_private_empty_directory_matches(home: PathBuf) -> impl Fn(&Path, &str, &str) -> bool {
    move |path, identity, mode| {
        let body = format!(
            "if _dot_init_private_empty_directory_matches {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(path.to_str().expect("gate path")),
            sq(identity),
            sq(mode),
        );
        shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0
    }
}

/// Live claim reaper: the real `_dot_init_stage_claim_remove`.
fn live_stage_claim_remove(home: PathBuf) -> impl Fn(&Path, &str, &str) -> dot::Result<()> {
    move |stage, kind, path| {
        let body = format!(
            "if _dot_init_stage_claim_remove {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(stage.to_str().expect("gate path")),
            sq(kind),
            sq(path),
        );
        if shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "stage claim refused",
            })
        }
    }
}

/// Boxed three-argument gate, so the bundle below stays a simple type.
type Gate3Bool = Box<dyn Fn(&Path, &str, &str) -> bool>;

/// Boxed one-argument gate.
type Gate1Bool = Box<dyn Fn(&Path) -> bool>;

/// Boxed claim reaper.
type Gate3Unit = Box<dyn Fn(&Path, &str, &str) -> dot::Result<()>>;

/// Bundle the five entry-family gates for one side.
fn live_stage_hooks(home: &Path) -> (Gate3Bool, Gate1Bool, Gate3Bool, Gate3Bool, Gate3Unit) {
    (
        Box::new(live_private_directory_matches(home.to_path_buf())),
        Box::new(live_stage_only_next(home.to_path_buf())),
        Box::new(live_stage_claim_matches(home.to_path_buf())),
        Box::new(live_private_empty_directory_matches(home.to_path_buf())),
        Box::new(live_stage_claim_remove(home.to_path_buf())),
    )
}

/// Live intent reader: the real `_dot_init_entry_intent`, with its
/// `REPLY` parsed into the port's record twin.
fn live_entry_intent(
    home: PathBuf,
) -> impl Fn(&Path, &str, &str, &str) -> dot::Result<publish::IntentRecord> {
    move |file, mode, oid, path| {
        let body = format!(
            "if _dot_init_entry_intent {} {} {} {}; then code=0; else code=$?; REPLY=; fi\nprintf 'reply=%s\\ncode=%s\\n' \"$REPLY\" \"$code\"\n",
            sq(file.to_str().expect("intent path")),
            sq(mode),
            sq(oid),
            sq(path),
        );
        let (code, out, _) = shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body);
        if code != 0 {
            return Err(dot::errors::Error::Usage {
                message: "entry intent refused",
            });
        }
        let text = String::from_utf8_lossy(&out);
        let reply = text
            .lines()
            .find_map(|line| line.strip_prefix("reply="))
            .unwrap_or_default();
        let fields: Vec<&str> = reply.split('\t').collect();
        if fields.len() != 6 {
            return Err(dot::errors::Error::Usage {
                message: "entry intent misparsed",
            });
        }
        Ok(publish::IntentRecord {
            phase: fields[0].to_string(),
            stage: fields[1].to_string(),
            dev: fields[2].to_string(),
            ino: fields[3].to_string(),
            next_dev: fields[4].to_string(),
            next_ino: fields[5].to_string(),
        })
    }
}

/// Live prior reader: the real `_dot_init_prior_record`, with its
/// `REPLY` parsed into the port's record twin.
fn live_prior_record(home: PathBuf) -> impl Fn(&Path, &str) -> dot::Result<publish::PriorRecord> {
    move |prior, path| {
        let body = format!(
            "if _dot_init_prior_record {} {}; then code=0; else code=$?; REPLY=; fi\nprintf 'reply=%s\\ncode=%s\\n' \"$REPLY\" \"$code\"\n",
            sq(prior.to_str().expect("prior path")),
            sq(path),
        );
        let (code, out, _) = shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body);
        if code != 0 {
            return Err(dot::errors::Error::Usage {
                message: "prior record refused",
            });
        }
        let text = String::from_utf8_lossy(&out);
        let reply = text
            .lines()
            .find_map(|line| line.strip_prefix("reply="))
            .unwrap_or_default();
        let fields: Vec<&str> = reply.split('\t').collect();
        if fields.len() != 6 {
            return Err(dot::errors::Error::Usage {
                message: "prior record misparsed",
            });
        }
        Ok(publish::PriorRecord {
            kind: fields[0].to_string(),
            dev: fields[1].to_string(),
            ino: fields[2].to_string(),
            mode: fields[3].to_string(),
            size: fields[4].to_string(),
            value: fields[5].to_string(),
        })
    }
}

/// Live worktree-blob matcher: the real
/// `_dot_init_candidate_matches_git` with the side's git binding
/// curried in.
fn live_candidate_matches(
    home: PathBuf,
    git_dir: PathBuf,
    commit: String,
) -> impl Fn(&str, &str, &str) -> bool {
    move |mode, oid, path| {
        let body = format!(
            "if _dot_init_candidate_matches_git {} {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(git_dir.to_str().expect("git dir")),
            sq(&commit),
            sq(mode),
            sq(oid),
            sq(path),
        );
        shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0
    }
}

/// Live worktree-state matcher: the real
/// `_dot_init_path_state_matches` per call.
fn live_path_state_matches(
    home: PathBuf,
) -> impl Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool {
    move |target, kind, dev, ino, mode, size, value| {
        let body = format!(
            "if _dot_init_path_state_matches {} {} {} {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(target.to_str().expect("match path")),
            sq(kind),
            sq(dev),
            sq(ino),
            sq(mode),
            sq(size),
            sq(value),
        );
        shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0
    }
}

/// Live intent publisher: the real `_dot_init_publish_intent`.
fn live_publish_intent(home: PathBuf) -> impl Fn(&Path, &str, &str, &str) -> dot::Result<()> {
    move |file, mode, oid, path| {
        let body = format!(
            "if _dot_init_publish_intent {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(file.to_str().expect("intent path")),
            sq(mode),
            sq(oid),
            sq(path),
        );
        if shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "publish intent refused",
            })
        }
    }
}

/// Live single-entry publisher: the real `_dot_init_publish_one`
/// with the side's git binding curried in.
fn live_publish_one(
    home: PathBuf,
    git_dir: PathBuf,
    commit: String,
) -> impl Fn(&Path, &Path, &str, &str, &str) -> dot::Result<()> {
    move |transaction, intent, mode, oid, path| {
        let body = format!(
            "if _dot_init_publish_one {} {} {} {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(transaction.to_str().expect("transaction")),
            sq(intent.to_str().expect("intent path")),
            sq(git_dir.to_str().expect("git dir")),
            sq(&commit),
            sq(mode),
            sq(oid),
            sq(path),
        );
        if shell_run(&home, &[("DOT_INIT_NONCE", NONCE)], &body).0 == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "publish one refused",
            })
        }
    }
}

/// Run the live `_dot_init_published_stage_matches` oracle on one
/// side and report its verdict.
fn shell_stage_matches(home: &Path, stage: &Path, identity: &str, path: &str) -> i32 {
    let body = format!(
        "if _dot_init_published_stage_matches {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(stage.to_str().expect("stage path")),
        sq(identity),
        sq(path),
    );
    shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &body).0
}

/// Run the live `_dot_init_published_intent_matches` oracle and
/// report its verdict plus `REPLY` bytes.
fn shell_intent_matches(
    home: &Path,
    intent: &Path,
    mode: &str,
    oid: &str,
    path: &str,
) -> (i32, Vec<u8>) {
    let body = format!(
        "if _dot_init_published_intent_matches {} {} {} {}; then code=0; else code=$?; REPLY=; fi\nprintf 'reply=%s\\ncode=%s\\n' \"$REPLY\" \"$code\"\n",
        sq(intent.to_str().expect("intent path")),
        sq(mode),
        sq(oid),
        sq(path),
    );
    let (code, out, _) = shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &body);
    let text = String::from_utf8_lossy(&out);
    let reply = text
        .lines()
        .find_map(|line| line.strip_prefix("reply="))
        .unwrap_or_default()
        .as_bytes()
        .to_vec();
    (code, reply)
}

/// Run the live `_dot_init_cleanup_published_stage` oracle and
/// report its verdict.
fn shell_cleanup(home: &Path, stage: &Path, identity: &str, path: &str) -> i32 {
    let body = format!(
        "if _dot_init_cleanup_published_stage {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(stage.to_str().expect("stage path")),
        sq(identity),
        sq(path),
    );
    shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &body).0
}

/// Write a stage claim marker the shell way, via the live
/// `_dot_init_stage_claim_write`.
fn write_claim(home: &Path, stage: &Path, kind: &str, path: &str) {
    let body = format!(
        "if _dot_init_stage_claim_write {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(stage.to_str().expect("stage path")),
        sq(kind),
        sq(path),
    );
    let (code, _, err) = shell_run(home, &[("DOT_INIT_NONCE", NONCE)], &body);
    assert_eq!(
        code,
        0,
        "claim write {}: {}",
        stage.display(),
        String::from_utf8_lossy(&err)
    );
}

/// Make a mode-700 stage directory for `path` under `home` and
/// return its absolute path plus identity.
fn make_stage(home: &Path, path: &str) -> (PathBuf, String) {
    let stage = entry_stage(home, path);
    std::fs::create_dir_all(&stage).expect("make stage");
    chmod(&stage, 0o700);
    let identity = identity_of(&stage);
    (stage, identity)
}

/// Check one stage row on a shared fixture: the shell verdict and
/// the port verdict (through the live gates) must agree.
fn check_stage_row(home: &Path, stage: &Path, identity: &str, path: &str, what: &str) {
    let expected = shell_stage_matches(home, stage, identity, path);
    let (a, b, c, d, _) = live_stage_hooks(home);
    let hooks = publish::StageHooks {
        private_directory_matches: a.as_ref(),
        stage_only_next: b.as_ref(),
        stage_claim_matches: c.as_ref(),
        private_empty_directory_matches: d.as_ref(),
        stage_claim_remove: &live_stage_claim_remove(home.to_path_buf()),
    };
    let actual = i32::from(!publish::published_stage_matches(
        stage, identity, path, &hooks,
    ));
    assert_eq!(actual, expected, "stage row {what}");
}

#[test]
fn stage_matches_claim_and_empty_states() {
    let dir = TempDir::new("publish-stage").expect("temp dir");
    let home = dir.path();

    // Missing stage refuses on both engines.
    let missing = home.join("no-such-stage");
    check_stage_row(home, &missing, "0:0", "doc", "missing stage");

    // Valid stage with a live claim and no `next` verifies.
    let (claimed, claimed_identity) = make_stage(home, "doc");
    write_claim(home, &claimed, "entry", "doc");
    check_stage_row(home, &claimed, &claimed_identity, "doc", "live claim");

    // Valid stage with no claim and nothing inside verifies.
    let (bare, bare_identity) = make_stage(home, "bare");
    check_stage_row(home, &bare, &bare_identity, "bare", "empty no claim");

    // A present `next` file always refuses, claim or not.
    write(&claimed, "next", b"in progress");
    check_stage_row(home, &claimed, &claimed_identity, "doc", "next with claim");
    write(&bare, "next", b"in progress");
    check_stage_row(home, &bare, &bare_identity, "bare", "next without claim");
    std::fs::remove_file(claimed.join("next")).expect("drop next");
    std::fs::remove_file(bare.join("next")).expect("drop next");
    check_stage_row(
        home,
        &claimed,
        &claimed_identity,
        "doc",
        "claim after next drain",
    );
}

#[test]
fn stage_matches_refusal_states() {
    let dir = TempDir::new("publish-stage-no").expect("temp dir");
    let home = dir.path();

    // A claim for another path does not verify this one.
    let (foreign, foreign_identity) = make_stage(home, "doc");
    write_claim(home, &foreign, "entry", "other");
    check_stage_row(home, &foreign, &foreign_identity, "doc", "foreign claim");

    // A sibling junk file breaks the content gate, claim or not.
    write(&foreign, "junk", b"x");
    check_stage_row(
        home,
        &foreign,
        &foreign_identity,
        "doc",
        "junk beside claim",
    );
    std::fs::remove_file(foreign.join("junk")).expect("drop junk");
    std::fs::remove_file(foreign.join(".dot-init-stage-claim-v1")).expect("drop claim");
    write(&foreign, "junk", b"x");
    check_stage_row(
        home,
        &foreign,
        &foreign_identity,
        "doc",
        "junk without claim",
    );

    // Wrong mode and wrong identity refuse.
    let (loose, loose_identity) = make_stage(home, "loose");
    write_claim(home, &loose, "entry", "loose");
    chmod(&loose, 0o755);
    check_stage_row(home, &loose, &loose_identity, "loose", "loose mode");
    chmod(&loose, 0o700);
    check_stage_row(home, &loose, "0:0", "loose", "wrong identity");

    // A regular file or a symlink is never a stage.
    let file = home.join("flat");
    write(dir.path(), "flat", b"x");
    check_stage_row(home, &file, "0:0", "flat", "file stage");
    std::os::unix::fs::symlink(&loose, home.join("link")).expect("link stage");
    let link = home.join("link");
    check_stage_row(home, &link, &loose_identity, "loose", "symlink stage");
}

/// Craft an intent record line the shell way:
/// `phase\tmode\toid\tpath\tstage_rel\tdev\tino\tnext_dev\tnext_ino`.
fn intent_line(fields: [&str; 9]) -> Vec<u8> {
    let [
        phase,
        mode,
        oid,
        path,
        stage_rel,
        dev,
        ino,
        next_dev,
        next_ino,
    ] = fields;
    format!("{phase}\t{mode}\t{oid}\t{path}\t{stage_rel}\t{dev}\t{ino}\t{next_dev}\t{next_ino}\n")
        .into_bytes()
}

/// Home-relative spelling of an absolute stage path.
fn stage_rel(home: &Path, stage: &Path) -> String {
    stage
        .to_str()
        .expect("stage text")
        .strip_prefix(&format!("{}/", home.to_str().expect("home text")))
        .expect("stage under home")
        .to_string()
}

/// Check one intent row on a shared fixture: verdicts must agree,
/// and on success the `REPLY` bytes must match byte for byte.
fn check_intent_row(home: &Path, intent: &Path, mode: &str, oid: &str, path: &str, what: &str) {
    let (expected_code, expected_reply) = shell_intent_matches(home, intent, mode, oid, path);
    let entry = live_entry_intent(home.to_path_buf());
    let (a, b, c, d, _) = live_stage_hooks(home);
    let hooks = publish::StageHooks {
        private_directory_matches: a.as_ref(),
        stage_only_next: b.as_ref(),
        stage_claim_matches: c.as_ref(),
        private_empty_directory_matches: d.as_ref(),
        stage_claim_remove: &live_stage_claim_remove(home.to_path_buf()),
    };
    match publish::published_intent_matches(intent, mode, oid, path, home, &entry, &hooks) {
        Ok(reply) => {
            assert_eq!(expected_code, 0, "intent row {what}");
            assert_bytes_eq(&reply, &expected_reply, &format!("intent reply {what}"));
        }
        Err(_) => assert_ne!(expected_code, 0, "intent row {what}"),
    }
}

const INTENT_MODE: &str = "100644";
const INTENT_OID: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

/// Build a prepared-intent fixture: live target with a known
/// identity, a live stage (optionally claimed), and an intent file
/// binding them. Returns the intent path.
fn prepared_fixture(home: &Path, path: &str, claim: bool) -> PathBuf {
    let (stage, stage_identity) = make_stage(home, path);
    if claim {
        write_claim(home, &stage, "entry", path);
    }
    write(home, path, b"published body");
    let target_identity = identity_of(&home.join(path));
    let (stage_dev, stage_ino) = stage_identity.split_once(':').expect("stage identity");
    let (next_dev, next_ino) = target_identity.split_once(':').expect("target identity");
    let intent = home.join(format!("intent-{path}"));
    std::fs::write(
        &intent,
        intent_line([
            "prepared",
            INTENT_MODE,
            INTENT_OID,
            path,
            &stage_rel(home, &stage),
            stage_dev,
            stage_ino,
            next_dev,
            next_ino,
        ]),
    )
    .expect("write intent");
    intent
}

#[test]
fn intent_matches_prepared_states() {
    let dir = TempDir::new("publish-intent").expect("temp dir");
    let home = dir.path();

    // Prepared with a consumed stage (removed after publication).
    let intent = prepared_fixture(home, "gone", false);
    let stage = entry_stage(home, "gone");
    std::fs::remove_dir(&stage).expect("consume stage");
    check_intent_row(
        home,
        &intent,
        INTENT_MODE,
        INTENT_OID,
        "gone",
        "consumed stage",
    );

    // Prepared with a live empty stage and with a live claim.
    let bare = prepared_fixture(home, "bare", false);
    check_intent_row(home, &bare, INTENT_MODE, INTENT_OID, "bare", "empty stage");
    let claimed = prepared_fixture(home, "kept", true);
    check_intent_row(
        home,
        &claimed,
        INTENT_MODE,
        INTENT_OID,
        "kept",
        "claimed stage",
    );

    // Non-prepared phases refuse even when everything else lines up.
    for phase in ["pending", "staged"] {
        let row = prepared_fixture(home, phase, false);
        let raw = std::fs::read(&row).expect("read intent");
        let mut text = String::from_utf8(raw).expect("intent text");
        text = text.replacen("prepared", phase, 1);
        std::fs::write(&row, text).expect("rewrite intent");
        check_intent_row(
            home,
            &row,
            INTENT_MODE,
            INTENT_OID,
            phase,
            &format!("{phase} phase"),
        );
    }
}

#[test]
fn intent_matches_refusal_states() {
    let dir = TempDir::new("publish-intent-no").expect("temp dir");
    let home = dir.path();

    // A replaced target (identity drift) refuses.
    let drifted = prepared_fixture(home, "drifted", false);
    write(home, "drifted", b"different body, new inode");
    check_intent_row(
        home,
        &drifted,
        INTENT_MODE,
        INTENT_OID,
        "drifted",
        "identity drift",
    );

    // A missing target refuses.
    let missing = prepared_fixture(home, "missing", false);
    std::fs::remove_file(home.join("missing")).expect("drop target");
    check_intent_row(
        home,
        &missing,
        INTENT_MODE,
        INTENT_OID,
        "missing",
        "missing target",
    );

    // A corrupted stage refuses.
    let dirty = prepared_fixture(home, "dirty", false);
    let stage = entry_stage(home, "dirty");
    write(&stage, "junk", b"x");
    check_intent_row(
        home,
        &dirty,
        INTENT_MODE,
        INTENT_OID,
        "dirty",
        "dirty stage",
    );

    // Mode and path mismatches fail the intent gate itself.
    let strict = prepared_fixture(home, "strict", false);
    check_intent_row(
        home,
        &strict,
        "100755",
        INTENT_OID,
        "strict",
        "mode mismatch",
    );
    check_intent_row(
        home,
        &strict,
        INTENT_MODE,
        INTENT_OID,
        "other",
        "path mismatch",
    );

    // A missing intent file refuses.
    let absent = home.join("no-intent");
    check_intent_row(
        home,
        &absent,
        INTENT_MODE,
        INTENT_OID,
        "absent",
        "missing intent",
    );
}

/// Check one cleanup row on twin fixtures: verdicts must agree and
/// the stage/marker shapes must match afterwards. Each side verifies
/// against its own stage identity (twin directories carry distinct
/// inodes by construction).
fn check_cleanup_row(
    home_shell: &Path,
    home_rust: &Path,
    stage_rel_name: &str,
    path: &str,
    what: &str,
) {
    for home in [home_shell, home_rust] {
        let stage = home.join(stage_rel_name);
        assert!(exists(&stage), "fixture stage {what}");
    }
    let shell_stage = home_shell.join(stage_rel_name);
    let rust_stage = home_rust.join(stage_rel_name);
    let identity = identity_of(&shell_stage);
    let rust_identity = identity_of(&rust_stage);
    let expected = shell_cleanup(home_shell, &shell_stage, &identity, path);
    let (a, b, c, d, e) = live_stage_hooks(home_rust);
    let hooks = publish::StageHooks {
        private_directory_matches: a.as_ref(),
        stage_only_next: b.as_ref(),
        stage_claim_matches: c.as_ref(),
        private_empty_directory_matches: d.as_ref(),
        stage_claim_remove: e.as_ref(),
    };
    let actual = match publish::cleanup_published_stage(&rust_stage, &rust_identity, path, &hooks) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    assert_eq!(actual, expected, "cleanup row {what}");
    assert_eq!(
        exists(&shell_stage),
        exists(&rust_stage),
        "cleanup stage shape {what}"
    );
    assert_eq!(
        exists(&shell_stage.join(".dot-init-stage-claim-v1")),
        exists(&rust_stage.join(".dot-init-stage-claim-v1")),
        "cleanup marker shape {what}"
    );
}

#[test]
fn cleanup_published_stage_rows() {
    let twins = Twins::build("publish-cleanup");

    // Missing stage is a successful no-op on both engines.
    for home in [&twins.shell_home, &twins.rust_home] {
        assert!(!exists(&home.join("never")));
    }
    let missing_shell = twins.shell_home.join("never");
    let missing_rust = twins.rust_home.join("never");
    assert_eq!(
        shell_cleanup(&twins.shell_home, &missing_shell, "0:0", "never"),
        0
    );
    let (a, b, c, d, e) = live_stage_hooks(&twins.rust_home);
    let hooks = publish::StageHooks {
        private_directory_matches: a.as_ref(),
        stage_only_next: b.as_ref(),
        stage_claim_matches: c.as_ref(),
        private_empty_directory_matches: d.as_ref(),
        stage_claim_remove: e.as_ref(),
    };
    assert!(
        publish::cleanup_published_stage(&missing_rust, "0:0", "never", &hooks).is_ok(),
        "missing stage is a no-op"
    );

    // Build twin stages: one claimed, one bare-empty, one dirty.
    for home in [&twins.shell_home, &twins.rust_home] {
        let (claimed, _) = make_stage(home, "doc");
        write_claim(home, &claimed, "entry", "doc");
        make_stage(home, "bare");
        let (dirty, _) = make_stage(home, "dirty");
        write(&dirty, "junk", b"x");
    }
    // The nonce-derived leaf names match on both sides because the
    // nonce is pinned.
    for path in ["doc", "bare", "dirty"] {
        let rel = entry_stage(&twins.shell_home, path);
        let name = rel
            .file_name()
            .expect("leaf")
            .to_str()
            .expect("leaf text")
            .to_string();
        check_cleanup_row(&twins.shell_home, &twins.rust_home, &name, path, path);
    }
}

/// One publication side: a git repo with committed candidates, a
/// home worktree, a backup root, and a transaction journal set.
struct Side {
    home: PathBuf,
    transaction: PathBuf,
    backup: PathBuf,
    git_dir: PathBuf,
    commit: String,
}

/// Blob oid of `content` through the side's git.
fn blob_oid(git_dir: &Path, content: &[u8]) -> String {
    let dir = git_dir.parent().expect("repo root");
    let probe = dir.join("oid-probe.tmp");
    std::fs::write(&probe, content).expect("oid probe");
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("hash-object")
        .arg("--no-filters")
        .arg(&probe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    std::fs::remove_file(&probe).expect("drop oid probe");
    assert!(output.status.success(), "hash-object");
    String::from_utf8(output.stdout)
        .expect("oid text")
        .trim()
        .to_string()
}

/// Build one publication side under `root`: the repo commits the
/// candidate generation; home holds an unchanged entry, a
/// republished entry (stale prior row plus a prepared intent with
/// its stage consumed), a fresh entry backed up aside, a brand-new
/// entry, and a nested entry under an existing directory with its
/// lineage recorded at the directory root.
fn build_side(root: &Path, tag: &str) -> Side {
    let base = root.join(tag);
    let repo = base.join("repo");
    let home = base.join("home");
    let backup = base.join("backup");
    let transaction = base.join("transaction");
    for dir in [&repo, &home, &backup, &transaction] {
        std::fs::create_dir_all(dir).expect("side dirs");
    }
    git_run(&["init", "-q", repo.to_str().expect("repo path")]);
    git_run(&[
        "-C",
        repo.to_str().expect("repo path"),
        "config",
        "user.name",
        "t",
    ]);
    git_run(&[
        "-C",
        repo.to_str().expect("repo path"),
        "config",
        "user.email",
        "t@t",
    ]);
    let repo_str = repo.to_str().expect("repo path");
    let candidates: &[(&str, &[u8])] = &[
        ("same.txt", b"same body\n"),
        ("repub.txt", b"new published body\n"),
        ("fresh.txt", b"fresh body\n"),
        ("new.txt", b"brand new\n"),
        ("sub/nested.txt", b"nested body\n"),
    ];
    for (rel, body) in candidates {
        write(&repo, rel, body);
    }
    git_run(&["-C", repo_str, "add", "-A"]);
    git_run(&["-C", repo_str, "commit", "-qm", "candidate"]);
    let git_dir = repo.join(".git");
    let commit = {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .arg("rev-parse")
            .arg("HEAD")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("rev-parse");
        assert!(output.status.success(), "rev-parse HEAD");
        String::from_utf8(output.stdout)
            .expect("sha text")
            .trim()
            .to_string()
    };

    // Unchanged entry: home matches the candidate and the prior row.
    write(&home, "same.txt", b"same body\n");
    // Republished entry: snapshot the old state, then advance home
    // to the candidate and bind a prepared intent to it.
    write(&home, "repub.txt", b"old body\n");
    let repub_prior = snapshot_row(&home, "repub.txt");
    write(&home, "repub.txt", b"new published body\n");
    let repub_identity = identity_of(&home.join("repub.txt"));
    let (repub_next_dev, repub_next_ino) = repub_identity.split_once(':').expect("identity");
    let repub_stage = entry_stage(&home, "repub.txt");
    let repub_rel = stage_rel(&home, &repub_stage);
    // Fresh entry: snapshot while present, then move aside to the
    // backup root and record the backup lineage.
    write(&home, "fresh.txt", b"fresh body\n");
    let fresh_prior = snapshot_row(&home, "fresh.txt");
    std::fs::create_dir_all(backup.as_path()).expect("backup root");
    std::fs::rename(home.join("fresh.txt"), backup.join("fresh.txt")).expect("stash fresh");
    let fresh_conflict = snapshot_row(&backup, "fresh.txt");
    // Brand-new entry: absent everywhere, absent prior row.
    let new_prior = "new.txt\tabsent\t-\t-\t-\t-\t-".to_string();
    // Nested entry: the parent directory already exists at home (so
    // no parent intent is needed), the file waits in the backup
    // tree, and the lineage is recorded at the directory root.
    std::fs::create_dir_all(home.join("sub")).expect("home parent");
    std::fs::create_dir_all(backup.join("sub")).expect("backup parent");
    write(&home, "sub/nested.txt", b"nested body\n");
    let nested_prior = snapshot_row(&home, "sub/nested.txt");
    std::fs::rename(home.join("sub/nested.txt"), backup.join("sub/nested.txt"))
        .expect("stash nested");
    let nested_conflict = snapshot_row(&backup, "sub");

    let same_oid = blob_oid(&git_dir, b"same body\n");
    let repub_oid = blob_oid(&git_dir, b"new published body\n");
    let fresh_oid = blob_oid(&git_dir, b"fresh body\n");
    let new_oid = blob_oid(&git_dir, b"brand new\n");
    let nested_oid = blob_oid(&git_dir, b"nested body\n");
    let tree = format!(
        "100644\t{same_oid}\tsame.txt\n100644\t{repub_oid}\trepub.txt\n100644\t{fresh_oid}\tfresh.txt\n100644\t{new_oid}\tnew.txt\n100644\t{nested_oid}\tsub/nested.txt\n"
    );
    std::fs::write(transaction.join("tree.tsv"), tree).expect("tree journal");
    let same_prior = snapshot_row(&home, "same.txt");
    let prior =
        format!("{same_prior}\n{repub_prior}\n{fresh_prior}\n{new_prior}\n{nested_prior}\n");
    std::fs::write(transaction.join("prior.tsv"), prior).expect("prior journal");
    let conflicts = format!("{fresh_conflict}\n{nested_conflict}\n");
    std::fs::write(transaction.join("conflicts.tsv"), conflicts).expect("conflicts journal");

    // Prepared intent for the republished entry, with its stage
    // consumed (removed after publication) like a recovered run.
    // The intent file name carries `git hash-object --stdin` over
    // the path, exactly like the publisher derives it.
    let path_hash = {
        let mut child = Command::new("git")
            .arg("hash-object")
            .arg("--stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn hash-object");
        {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("hash stdin")
                .write_all(b"repub.txt")
                .expect("hash write");
        }
        let output = child.wait_with_output().expect("hash output");
        assert!(output.status.success(), "hash path");
        String::from_utf8(output.stdout)
            .expect("hash text")
            .trim()
            .to_string()
    };
    std::fs::write(
        transaction.join(format!("publish-intent.{path_hash}")),
        intent_line([
            "prepared",
            "100644",
            &repub_oid,
            "repub.txt",
            &repub_rel,
            "0",
            "0",
            repub_next_dev,
            repub_next_ino,
        ]),
    )
    .expect("repub intent");

    Side {
        home,
        transaction,
        backup,
        git_dir,
        commit,
    }
}

/// Run the live `_dot_init_publish_worktree` oracle on one side.
fn shell_publish(side: &Side) -> i32 {
    let body = format!(
        "if _dot_init_publish_worktree {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(side.transaction.to_str().expect("transaction")),
    );
    shell_run(
        &side.home,
        &[
            ("DOT_INIT_NONCE", NONCE),
            ("DOT_INIT_GIT_DIR", side.git_dir.to_str().expect("git dir")),
            ("DOT_INIT_COMMIT", &side.commit),
            ("DOT_INIT_BRANCH", "publish-test"),
            ("DOT_INIT_BACKUP", side.backup.to_str().expect("backup")),
        ],
        &body,
    )
    .0
}

/// Read the branch and HEAD bindings out of one side's git dir.
fn git_bindings(side: &Side) -> (String, String) {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&side.git_dir)
        .arg("rev-parse")
        .arg("refs/heads/publish-test")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("rev-parse branch");
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&side.git_dir)
        .arg("symbolic-ref")
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("symbolic-ref HEAD");
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (branch, head)
}

#[test]
fn publish_worktree_end_to_end() {
    let dir = TempDir::new("publish-worktree").expect("temp dir");
    let shell_side = build_side(dir.path(), "sh");
    let rust_side = build_side(dir.path(), "rs");

    let expected = shell_publish(&shell_side);
    assert_eq!(expected, 0, "oracle publishes cleanly");

    let git = publish::PublishGit {
        git_dir: &rust_side.git_dir,
        commit: &rust_side.commit,
        branch: "publish-test",
        work_dir: &rust_side.home,
    };
    let prior = live_prior_record(rust_side.home.clone());
    let candidate = live_candidate_matches(
        rust_side.home.clone(),
        rust_side.git_dir.clone(),
        rust_side.commit.clone(),
    );
    let state = live_path_state_matches(rust_side.home.clone());
    let intent = live_publish_intent(rust_side.home.clone());
    let one = live_publish_one(
        rust_side.home.clone(),
        rust_side.git_dir.clone(),
        rust_side.commit.clone(),
    );
    let entry = live_entry_intent(rust_side.home.clone());
    let (a, b, c, d, e) = live_stage_hooks(&rust_side.home);
    let stages = publish::StageHooks {
        private_directory_matches: a.as_ref(),
        stage_only_next: b.as_ref(),
        stage_claim_matches: c.as_ref(),
        private_empty_directory_matches: d.as_ref(),
        stage_claim_remove: e.as_ref(),
    };
    let hooks = publish::PublishHooks {
        prior_record: &prior,
        candidate_matches_git: &candidate,
        path_state_matches: &state,
        publish_intent: &intent,
        publish_one: &one,
        entry_intent: &entry,
        stages,
    };
    let actual = match publish::publish_worktree(
        &rust_side.transaction,
        &rust_side.home,
        &rust_side.backup,
        &git,
        &hooks,
    ) {
        Ok(()) => 0,
        Err(error) => panic!("port refused publication: {error}"),
    };
    assert_eq!(actual, expected, "publish verdict");

    // End-state shapes must match file for file.
    for rel in [
        "same.txt",
        "repub.txt",
        "fresh.txt",
        "new.txt",
        "sub/nested.txt",
    ] {
        assert_eq!(
            shape(&shell_side.home, rel),
            shape(&rust_side.home, rel),
            "published shape {rel}"
        );
    }
    // No stage directory survives on either side.
    for rel in [
        "same.txt",
        "repub.txt",
        "fresh.txt",
        "new.txt",
        "sub/nested.txt",
    ] {
        let leaf = entry_stage(&shell_side.home, rel);
        let name = leaf.file_name().expect("leaf");
        assert_eq!(
            exists(&shell_side.home.join(name)),
            exists(&rust_side.home.join(name)),
            "stage shape {rel}"
        );
    }
    assert_eq!(
        git_bindings(&shell_side),
        git_bindings(&rust_side),
        "git bindings"
    );
    assert_eq!(
        git_bindings(&rust_side).0,
        rust_side.commit,
        "branch advanced"
    );
    assert_eq!(
        git_bindings(&rust_side).1,
        "refs/heads/publish-test",
        "HEAD bound"
    );
}

/// Run one refusal side through both engines and compare verdicts.
/// The closure set is live, so only the orchestration can diverge.
fn check_publish_refusal(build: &dyn Fn(&Path, &str) -> Side, root: &Path, tag: &str, what: &str) {
    let shell_side = build(root, &format!("sh-{tag}"));
    let rust_side = build(root, &format!("rs-{tag}"));
    let expected = shell_publish(&shell_side);
    assert_ne!(expected, 0, "oracle refuses {what}");
    let git = publish::PublishGit {
        git_dir: &rust_side.git_dir,
        commit: &rust_side.commit,
        branch: "publish-test",
        work_dir: &rust_side.home,
    };
    let prior = live_prior_record(rust_side.home.clone());
    let candidate = live_candidate_matches(
        rust_side.home.clone(),
        rust_side.git_dir.clone(),
        rust_side.commit.clone(),
    );
    let state = live_path_state_matches(rust_side.home.clone());
    let intent = live_publish_intent(rust_side.home.clone());
    let one = live_publish_one(
        rust_side.home.clone(),
        rust_side.git_dir.clone(),
        rust_side.commit.clone(),
    );
    let entry = live_entry_intent(rust_side.home.clone());
    let (a, b, c, d, e) = live_stage_hooks(&rust_side.home);
    let stages = publish::StageHooks {
        private_directory_matches: a.as_ref(),
        stage_only_next: b.as_ref(),
        stage_claim_matches: c.as_ref(),
        private_empty_directory_matches: d.as_ref(),
        stage_claim_remove: e.as_ref(),
    };
    let hooks = publish::PublishHooks {
        prior_record: &prior,
        candidate_matches_git: &candidate,
        path_state_matches: &state,
        publish_intent: &intent,
        publish_one: &one,
        entry_intent: &entry,
        stages,
    };
    let actual = match publish::publish_worktree(
        &rust_side.transaction,
        &rust_side.home,
        &rust_side.backup,
        &git,
        &hooks,
    ) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    assert_eq!(actual, expected, "publish refusal {what}");
}

/// Minimal side: one repo, one home file, caller-supplied journals.
fn bare_side(root: &Path, tag: &str, content: &[u8]) -> (Side, String) {
    let base = root.join(tag);
    let repo = base.join("repo");
    let home = base.join("home");
    let backup = base.join("backup");
    let transaction = base.join("transaction");
    for dir in [&repo, &home, &backup, &transaction] {
        std::fs::create_dir_all(dir).expect("side dirs");
    }
    git_run(&["init", "-q", repo.to_str().expect("repo path")]);
    let repo_str = repo.to_str().expect("repo path").to_string();
    git_run(&["-C", &repo_str, "config", "user.name", "t"]);
    git_run(&["-C", &repo_str, "config", "user.email", "t@t"]);
    write(&repo, "doc.txt", content);
    git_run(&["-C", &repo_str, "add", "-A"]);
    git_run(&["-C", &repo_str, "commit", "-qm", "candidate"]);
    let git_dir = repo.join(".git");
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&git_dir)
        .arg("rev-parse")
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("rev-parse");
    assert!(output.status.success(), "rev-parse HEAD");
    let commit = String::from_utf8(output.stdout)
        .expect("sha text")
        .trim()
        .to_string();
    let oid = blob_oid(&git_dir, content);
    (
        Side {
            home,
            transaction,
            backup,
            git_dir,
            commit,
        },
        oid,
    )
}

#[test]
fn publish_worktree_refusals() {
    let dir = TempDir::new("publish-no").expect("temp dir");

    // Changed entry with no recoverable intent refuses: the
    // candidate no longer matches home, and no prepared intent
    // explains the drift.
    let changed = |root: &Path, tag: &str| -> Side {
        let (side, oid) = bare_side(root, tag, b"candidate body\n");
        write(&side.home, "doc.txt", b"old body\n");
        let prior = snapshot_row(&side.home, "doc.txt");
        write(&side.home, "doc.txt", b"drifted body\n");
        std::fs::write(
            side.transaction.join("tree.tsv"),
            format!("100644\t{oid}\tdoc.txt\n"),
        )
        .expect("tree journal");
        std::fs::write(side.transaction.join("prior.tsv"), format!("{prior}\n"))
            .expect("prior journal");
        side
    };
    check_publish_refusal(&changed, dir.path(), "drift", "unrecoverable drift");

    // Missing journals refuse before any row runs.
    let missing = |root: &Path, tag: &str| -> Side {
        let (side, _) = bare_side(root, tag, b"candidate body\n");
        side
    };
    check_publish_refusal(&missing, dir.path(), "journals", "missing journals");

    // A fresh entry with no backup lineage refuses.
    let lineage = |root: &Path, tag: &str| -> Side {
        let (side, oid) = bare_side(root, tag, b"candidate body\n");
        write(&side.home, "doc.txt", b"old body\n");
        let prior = snapshot_row(&side.home, "doc.txt");
        std::fs::remove_file(side.home.join("doc.txt")).expect("stash");
        std::fs::write(
            side.transaction.join("tree.tsv"),
            format!("100644\t{oid}\tdoc.txt\n"),
        )
        .expect("tree journal");
        std::fs::write(side.transaction.join("prior.tsv"), format!("{prior}\n"))
            .expect("prior journal");
        std::fs::write(side.transaction.join("conflicts.tsv"), "").expect("conflicts journal");
        side
    };
    check_publish_refusal(&lineage, dir.path(), "lineage", "missing lineage");

    // An occupied target with a foreign blob refuses: the candidate
    // gate fails and home is present.
    let occupied = |root: &Path, tag: &str| -> Side {
        let (side, _) = bare_side(root, tag, b"candidate body\n");
        write(&side.home, "doc.txt", b"squatter body\n");
        let prior = snapshot_row(&side.home, "doc.txt");
        let foreign = blob_oid(&side.git_dir, b"unrelated\n");
        std::fs::write(
            side.transaction.join("tree.tsv"),
            format!("100644\t{foreign}\tdoc.txt\n"),
        )
        .expect("tree journal");
        std::fs::write(side.transaction.join("prior.tsv"), format!("{prior}\n"))
            .expect("prior journal");
        side
    };
    check_publish_refusal(&occupied, dir.path(), "occupied", "occupied target");
}

/// Scripted convergence collaborator: records the call sequence
/// with the provider each call observed, answering from a script.
struct ConvergeScript {
    log: RefCell<Vec<String>>,
    select_ok: bool,
    config_ok: bool,
    sync_ok: bool,
    finalize_ok: bool,
}

/// Provider observed outside any override: the test process must
/// not set it, so both engines start from `<unset>`.
fn outer_provider() -> String {
    assert!(
        std::env::var_os("DOT_DEPENDENCY_PROVIDER").is_none(),
        "test process must not set DOT_DEPENDENCY_PROVIDER"
    );
    "<unset>".to_string()
}

/// Run the live `_dot_init_forward_converge` oracle with every
/// callee stubbed to log its observation, and report the verdict,
/// the log, and the provider observed afterwards. Later definitions
/// win in bash, so the stubs replace the real collaborators after
/// sourcing; the real function under test still owns the
/// sequencing, the provider scoping, and the status threading.
fn shell_converge(
    home: &Path,
    skip: bool,
    select_ok: bool,
    config_ok: bool,
    sync_ok: bool,
    finalize_ok: bool,
) -> (i32, Vec<String>, String) {
    let bit = |ok: bool| if ok { 0 } else { 1 };
    let snippet = format!(
        "_dot_client_select() {{ echo \"select provider=${{DOT_DEPENDENCY_PROVIDER-<unset>}}\" >>\"$log\"; return {}; }}\n\
         dot_config_load() {{ echo \"config provider=${{DOT_DEPENDENCY_PROVIDER-<unset>}}\" >>\"$log\"; return {}; }}\n\
         _ui_begin() {{ echo \"begin total=$1 provider=${{DOT_DEPENDENCY_PROVIDER-<unset>}}\" >>\"$log\"; }}\n\
         _dot_update_sync_repos() {{ echo \"sync skip=${{DOT_INIT_SKIP_PROVIDER:-0}} provider=${{DOT_DEPENDENCY_PROVIDER-<unset>}}\" >>\"$log\"; return {}; }}\n\
         _dot_update_finalize() {{ echo \"finalize status=$1 provider=${{DOT_DEPENDENCY_PROVIDER-<unset>}}\" >>\"$log\"; return {}; }}\n\
         : >\"$log\"\n\
         if _dot_init_forward_converge; then code=0; else code=$?; fi\n\
         printf 'outer=%s\\n' \"${{DOT_DEPENDENCY_PROVIDER-<unset>}}\"\n\
         cat \"$log\"\n\
         printf 'code=%s\\n' \"$code\"\n",
        bit(select_ok),
        bit(config_ok),
        bit(sync_ok),
        bit(finalize_ok),
    );
    let skip_env: &[(&str, &str)] = if skip {
        &[("DOT_INIT_SKIP_PROVIDER", "1")]
    } else {
        &[]
    };
    let log = home.join("converge.log");
    let env: Vec<(&str, &str)> = std::iter::once(("DOT_INIT_NONCE", NONCE))
        .chain(std::iter::once(("log", log.to_str().expect("log path"))))
        .chain(skip_env.iter().copied())
        .collect();
    let (code, out, _) = shell_run(home, &env, &snippet);
    let text = String::from_utf8_lossy(&out);
    let mut log_lines = Vec::new();
    let mut outer = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("outer=") {
            outer = value.to_string();
        } else if line.starts_with("code=") {
            // Verdict line, reported separately.
        } else {
            log_lines.push(line.to_string());
        }
    }
    (code, log_lines, outer)
}

/// Check one convergence row: stub-log sequences, verdicts, and the
/// restored provider must agree between the shell (with stubbed
/// collaborators) and the port (with logging closures). The Rust
/// stubs render the provider from the skip flag the port threaded
/// to them — `none` when skipping, the outer value otherwise — so
/// the logs only match when the port threads the flag exactly where
/// the shell scopes its override.
fn check_converge_row(
    skip: bool,
    select_ok: bool,
    config_ok: bool,
    sync_ok: bool,
    finalize_ok: bool,
    what: &str,
) {
    let dir = TempDir::new("publish-converge").expect("temp dir");
    let home = dir.path();
    let (expected_code, expected_log, expected_outer) =
        shell_converge(home, skip, select_ok, config_ok, sync_ok, finalize_ok);
    let outer = outer_provider();
    let script = ConvergeScript {
        log: RefCell::new(Vec::new()),
        select_ok,
        config_ok,
        sync_ok,
        finalize_ok,
    };
    let seen = |skipped: bool| {
        if skipped {
            "none".to_string()
        } else {
            outer.clone()
        }
    };
    let ok = |flag: bool| {
        if flag {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "converge collaborator refused",
            })
        }
    };
    let hooks = publish::ConvergeHooks {
        select_client: &|| {
            script
                .log
                .borrow_mut()
                .push(format!("select provider={}", outer_provider()));
            ok(script.select_ok)
        },
        load_config: &|| {
            script
                .log
                .borrow_mut()
                .push(format!("config provider={}", outer_provider()));
            ok(script.config_ok)
        },
        begin_ui: &|total| {
            // The announcement provably ignores the provider (it
            // only assigns progress counters), so the stub renders
            // what a provider-reading callee at this point would
            // observe — exactly what the shell stub logs.
            script
                .log
                .borrow_mut()
                .push(format!("begin total={total} provider={}", seen(skip)));
        },
        sync_repos: &|skipped| {
            script.log.borrow_mut().push(format!(
                "sync skip={} provider={}",
                i32::from(skipped),
                seen(skipped)
            ));
            ok(script.sync_ok)
        },
        finalize: &|status, skipped| {
            script.log.borrow_mut().push(format!(
                "finalize status={status} provider={}",
                seen(skipped)
            ));
            ok(script.finalize_ok)
        },
    };
    let actual_code = match publish::forward_converge(skip, &hooks) {
        Ok(()) => 0,
        Err(_) => 1,
    };
    assert_eq!(actual_code, expected_code, "converge verdict {what}");
    assert_eq!(*script.log.borrow(), expected_log, "converge log {what}");
    assert_eq!("<unset>", expected_outer, "converge scoping {what}");
}

/// Convergence sequencing, provider scoping, and status threading
/// match the shell row for row.
#[test]
fn forward_converge_rows() {
    // Clean run, provider untouched throughout.
    check_converge_row(false, true, true, true, true, "clean");
    // Skipped provider: sync and finalize observe `none`, the
    // committed config still parses first, and the override does
    // not leak out.
    check_converge_row(true, true, true, true, true, "skipped provider");
    // Failed sync threads status 1 into a succeeding finalize.
    check_converge_row(false, true, true, false, true, "sync failure");
    // Failed sync under a skip threads status 1 past a `none`
    // finalize that itself refuses.
    check_converge_row(true, true, true, false, false, "sync and finalize failure");
    // Selection failure flows through: the shell calls it bare, so
    // the sequencing continues and the verdict is finalize's.
    check_converge_row(false, false, true, true, true, "selection failure");
    // Config failure short-circuits after selection.
    check_converge_row(false, true, false, true, true, "config failure");
}

/// Verbatim `_base_git` dispatch from `lib/dot/repos/model.sh` (the
/// model lane owns it): the separate-topology arm the origin reader
/// calls through. Only the reader itself is under test here.
const BASE_GIT_STUB: &str = "_base_git() {\n  case $DOT_BASE_TOPOLOGY in\n    separate)\n      command git --git-dir=\"$DOT_CLIENT_GIT_DIR\" --work-tree=\"$HOME\" \"$@\"\n      ;;\n    ordinary)\n      command git -C \"$HOME\" \"$@\"\n      ;;\n    *) return 128 ;;\n  esac\n}\n";

/// Run the live `_dot_init_single_origin` oracle and report its
/// verdict plus the printed URL line (without its newline, like
/// command substitution leaves it).
fn shell_origin(home: &Path, kind: &str, extra: &[(&str, &str)]) -> (i32, String) {
    let snippet = format!(
        "{BASE_GIT_STUB}if url=$(_dot_init_single_origin {} 2>/dev/null); then code=0; else code=$?; url=; fi\nprintf 'url=%s\\ncode=%s\\n' \"$url\" \"$code\"\n",
        sq(kind)
    );
    let mut env: Vec<(&str, &str)> = vec![("DOT_INIT_NONCE", NONCE)];
    env.extend_from_slice(extra);
    let (code, out, _) = shell_run(home, &env, &snippet);
    let text = String::from_utf8_lossy(&out);
    let url = text
        .lines()
        .find_map(|line| line.strip_prefix("url="))
        .unwrap_or_default()
        .to_string();
    (code, url)
}

/// Check one origin row: verdicts must agree, and on success the
/// emitted `url\n` bytes must match byte for byte.
fn check_origin_row(
    home: &Path,
    kind: &str,
    extra: &[(&str, &str)],
    scope: &publish::OriginScope<'_>,
    what: &str,
) {
    let (expected_code, expected_url) = shell_origin(home, kind, extra);
    match publish::single_origin(scope, home) {
        Ok(bytes) => {
            assert_eq!(expected_code, 0, "origin row {what}");
            assert_bytes_eq(
                &bytes,
                format!("{expected_url}\n").as_bytes(),
                &format!("origin bytes {what}"),
            );
        }
        Err(_) => assert_ne!(expected_code, 0, "origin row {what}"),
    }
}

/// Make a home git repo; returns home. With `urls`, sets that many
/// `remote.origin.url` values.
fn origin_home(root: &Path, tag: &str, urls: &[&str]) -> PathBuf {
    let home = root.join(tag);
    std::fs::create_dir_all(&home).expect("origin home");
    git_run(&["init", "-q", home.to_str().expect("home path")]);
    let home_str = home.to_str().expect("home path").to_string();
    git_run(&["-C", &home_str, "config", "user.name", "t"]);
    git_run(&["-C", &home_str, "config", "user.email", "t@t"]);
    for url in urls {
        git_run(&["-C", &home_str, "config", "--add", "remote.origin.url", url]);
    }
    home
}

#[test]
fn single_origin_ordinary_rows() {
    let dir = TempDir::new("publish-origin").expect("temp dir");
    let ordinary = publish::OriginScope::Ordinary;

    // One URL reads back with its newline.
    let one = origin_home(dir.path(), "one", &["https://example.test/dot.git"]);
    check_origin_row(&one, "ordinary", &[], &ordinary, "lone url");

    // No remote, no repository, and several URLs all refuse.
    let none = origin_home(dir.path(), "none", &[]);
    check_origin_row(&none, "ordinary", &[], &ordinary, "no url");
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).expect("plain home");
    check_origin_row(&plain, "ordinary", &[], &ordinary, "no repository");
    let many = origin_home(
        dir.path(),
        "many",
        &["https://example.test/a.git", "https://example.test/b.git"],
    );
    check_origin_row(&many, "ordinary", &[], &ordinary, "several urls");

    // Any other command-kind spelling reads through `git -C` too.
    check_origin_row(&one, "weird", &[], &ordinary, "other kind spelling");
}

#[test]
fn single_origin_separate_rows() {
    let dir = TempDir::new("publish-origin-sep").expect("temp dir");

    // Separate topology reads through the base git binding.
    let home = origin_home(dir.path(), "sep", &["https://example.test/dot.git"]);
    let git_dir = home.join(".git");
    let scope = publish::OriginScope::Separate { git_dir: &git_dir };
    check_origin_row(
        &home,
        "separate",
        &[
            ("DOT_BASE_TOPOLOGY", "separate"),
            ("DOT_CLIENT_GIT_DIR", git_dir.to_str().expect("git dir")),
        ],
        &scope,
        "separate lone url",
    );

    // Several URLs refuse through the separate binding as well.
    let home_many = origin_home(
        dir.path(),
        "sep-many",
        &["https://example.test/a.git", "https://example.test/b.git"],
    );
    let git_dir_many = home_many.join(".git");
    let scope_many = publish::OriginScope::Separate {
        git_dir: &git_dir_many,
    };
    check_origin_row(
        &home_many,
        "separate",
        &[
            ("DOT_BASE_TOPOLOGY", "separate"),
            (
                "DOT_CLIENT_GIT_DIR",
                git_dir_many.to_str().expect("git dir"),
            ),
        ],
        &scope_many,
        "separate several urls",
    );
}

#[test]
fn single_origin_unsafe_url_refuses() {
    let dir = TempDir::new("publish-origin-unsafe").expect("temp dir");
    let ordinary = publish::OriginScope::Ordinary;

    // A tab-carrying URL stores in git but fails the safe-value
    // gate on both engines.
    let home = origin_home(dir.path(), "tabbed", &["https://example.test/dot.git"]);
    let home_str = home.to_str().expect("home path").to_string();
    git_run(&["-C", &home_str, "config", "remote.origin.url", "a\tb"]);
    let stored = Command::new("git")
        .args(["-C", &home_str, "config", "--get", "remote.origin.url"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("read back url");
    assert!(
        stored.status.success() && stored.stdout.contains(&b'\t'),
        "git stores the tabbed url"
    );
    check_origin_row(&home, "ordinary", &[], &ordinary, "tabbed url");
}
