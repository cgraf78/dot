//! Differential parity tests for the parent-directory publisher
//! (`lib/dot/init-client.sh` lines 1062-1130,
//! `_dot_init_parent_directories`) against the live shell.
//!
//! Twin homes and twin transaction directories keep the engines from
//! colliding: the oracle works under `shell_home`/`shell_tx`, the
//! port under `rust_home`/`rust_tx`, with the same nonce so every
//! derived name agrees. The port's eight out-of-scope calls (the
//! parent record, the private-line publisher, the four stage-claim
//! helpers, and the two private-directory gates) cross as closures
//! that run the live shell functions, so each row compares the
//! orchestration — which branch ran, which bytes landed, which
//! status returned.
//!
//! Identity fields (`dev`/`ino`) name live inodes, so intent records
//! compare field-wise with those columns checked for shape and
//! self-consistency (matching the directory the engine actually
//! built) rather than byte equality. Everything else — verdicts,
//! trees, modes, and the remaining columns — compares exactly.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_parent::{ParentHooks, parent_directories};
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Shell prelude for every probe: the temp helpers (sibling temps,
/// identity, exclusive moves) plus the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Fixed nonce shared by both engines in every row.
const NONCE: &str = "n73";

/// Claim-marker leaf the stage checks look for.
const CLAIM_NAME: &str = ".dot-init-stage-claim-v1";

/// Run one shell snippet with `HOME` steered at `home` and report
/// the snippet's own verdict alongside both byte streams. The
/// snippet always ends with `printf 'code=%s\n' "$code" >&2`, so the
/// returned code is that verdict — stdout stays reserved for data
/// (`$REPLY` echoes), keeping reply bytes exact.
fn shell_run(home: &Path, env: &[(&str, &OsStr)], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "TMPDIR",
            std::env::var_os("TMPDIR")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| OsString::from("/tmp")),
        )
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .env("DOT_INIT_NONCE", NONCE)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    let verdict = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| {
            line.strip_prefix("code=")
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(99);
    (verdict, output.stdout, output.stderr)
}

/// `git hash-object --stdin` over raw bytes, the shell's
/// `printf '%s' ... | git hash-object --stdin` for intent and stage
/// names. Used only to derive deterministic fixture names.
fn git_hash(payload: &[u8]) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("git stdin")
        .write_all(payload)
        .expect("feed git");
    let output = child.wait_with_output().expect("reap git");
    assert!(output.status.success(), "git hash-object failed");
    let mut hex = String::from_utf8_lossy(&output.stdout).into_owned();
    while hex.ends_with('\n') {
        hex.pop();
    }
    assert!(
        hex.len() == 40 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "unexpected hash output: {hex:?}",
    );
    hex
}

/// Stage-relative name for one parent level: the shell's
/// `${current%/*}/.dot-init-parent.$DOT_INIT_NONCE.$hash` with the
/// `$HOME/` prefix stripped.
fn stage_rel_for(parent_rel: &[u8]) -> Vec<u8> {
    let hash = git_hash(parent_rel);
    let dir = match parent_rel.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &parent_rel[..index],
        None => &parent_rel[..0],
    };
    let mut out = dir.to_vec();
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend_from_slice(format!(".dot-init-parent.{NONCE}.{hash}").as_bytes());
    out
}

/// Twin homes plus twin transaction directories.
struct Sides {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    shell_tx: PathBuf,
    rust_tx: PathBuf,
}

impl Sides {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        let shell_tx = dir.path().join("sh-tx");
        let rust_tx = dir.path().join("rs-tx");
        for path in [&shell_home, &rust_home, &shell_tx, &rust_tx] {
            std::fs::create_dir_all(path).expect("fixture root");
        }
        Self {
            _dir: dir,
            shell_home,
            rust_home,
            shell_tx,
            rust_tx,
        }
    }

    /// Live-backed hooks for the Rust side: every closure runs the
    /// real shell helper with `HOME` steered at `rust_home`, so the
    /// nonce-derived names agree with the port's inputs.
    fn hooks(&self) -> ParentHooks<'_> {
        let home = &self.rust_home;
        ParentHooks {
            parent_record: Box::new(move |transaction, parent| {
                let transaction = transaction.as_os_str().to_os_string();
                let parent = OsString::from_vec(parent.to_vec());
                let (code, stdout, _) = shell_run(
                    home,
                    &[
                        ("DOT_TEST_TX", transaction.as_os_str()),
                        ("DOT_TEST_REL", parent.as_os_str()),
                    ],
                    "_dot_init_parent_record \"$DOT_TEST_TX\" \"$DOT_TEST_REL\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\nif [ \"$code\" -eq 0 ]; then printf '%s' \"$REPLY\"; fi\n",
                );
                if code != 0 {
                    return Err(dot::Error::Usage {
                        message: "shell parent record refused",
                    });
                }
                Ok(stdout)
            }),
            write_private_line: Box::new(move |file, line, replace| {
                let file = file.as_os_str().to_os_string();
                let line = OsString::from_vec(line.to_vec());
                let (code, _, _) = shell_run(
                    home,
                    &[
                        ("DOT_TEST_FILE", file.as_os_str()),
                        ("DOT_TEST_LINE", line.as_os_str()),
                        (
                            "DOT_TEST_REPLACE",
                            if replace {
                                OsStr::from_bytes(b"true")
                            } else {
                                OsStr::from_bytes(b"false")
                            },
                        ),
                    ],
                    "_dot_init_write_private_line \"$DOT_TEST_FILE\" \"$DOT_TEST_LINE\" \"$DOT_TEST_REPLACE\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
                );
                if code != 0 {
                    return Err(dot::Error::Usage {
                        message: "shell write private line refused",
                    });
                }
                Ok(())
            }),
            stage_claim_write: Box::new(move |stage, kind, path| {
                claim_call(home, "_dot_init_stage_claim_write", stage, kind, path)
            }),
            stage_claim_matches: Box::new(move |stage, kind, path| {
                claim_call(home, "_dot_init_stage_claim_matches", stage, kind, path)
            }),
            stage_claim_only: Box::new(move |stage| {
                let stage = stage.as_os_str().to_os_string();
                let (code, _, _) = shell_run(
                    home,
                    &[("DOT_TEST_STAGE", stage.as_os_str())],
                    "_dot_init_stage_claim_only \"$DOT_TEST_STAGE\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
                );
                if code != 0 {
                    return Err(dot::Error::Usage {
                        message: "shell stage claim only refused",
                    });
                }
                Ok(())
            }),
            stage_claim_remove: Box::new(move |stage, kind, path| {
                claim_call(home, "_dot_init_stage_claim_remove", stage, kind, path)
            }),
            private_directory_matches: Box::new(move |path, identity, mode| {
                private_call(
                    home,
                    "_dot_init_private_directory_matches",
                    path,
                    identity,
                    mode,
                )
            }),
            private_empty_directory_matches: Box::new(move |path, identity, mode| {
                private_call(
                    home,
                    "_dot_init_private_empty_directory_matches",
                    path,
                    identity,
                    mode,
                )
            }),
        }
    }
}

/// One live stage-claim call (`write`, `matches`, or `remove`) by
/// position (`stage kind path`).
fn claim_call(
    home: &Path,
    function: &str,
    stage: &Path,
    kind: &str,
    path: &[u8],
) -> dot::Result<()> {
    let stage = stage.as_os_str().to_os_string();
    let path = OsString::from_vec(path.to_vec());
    let (code, _, _) = shell_run(
        home,
        &[
            ("DOT_TEST_STAGE", stage.as_os_str()),
            ("DOT_TEST_KIND", OsStr::from_bytes(kind.as_bytes())),
            ("DOT_TEST_PATH", path.as_os_str()),
        ],
        &format!(
            "{function} \"$DOT_TEST_STAGE\" \"$DOT_TEST_KIND\" \"$DOT_TEST_PATH\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n"
        ),
    );
    if code != 0 {
        return Err(dot::Error::Usage {
            message: "shell stage claim call refused",
        });
    }
    Ok(())
}

/// One live private-directory gate by position
/// (`path identity? mode?`); absent arguments cross as empty, which
/// the shell's `${2:-}` reads exactly like an omission.
fn private_call(
    home: &Path,
    function: &str,
    path: &Path,
    identity: Option<&str>,
    mode: Option<&str>,
) -> dot::Result<()> {
    let path = path.as_os_str().to_os_string();
    let (code, _, _) = shell_run(
        home,
        &[
            ("DOT_TEST_P", path.as_os_str()),
            (
                "DOT_TEST_IDENT",
                identity.map_or(OsStr::from_bytes(b""), |value| {
                    OsStr::from_bytes(value.as_bytes())
                }),
            ),
            (
                "DOT_TEST_MODE",
                mode.map_or(OsStr::from_bytes(b""), |value| {
                    OsStr::from_bytes(value.as_bytes())
                }),
            ),
        ],
        &format!(
            "{function} \"$DOT_TEST_P\" \"$DOT_TEST_IDENT\" \"$DOT_TEST_MODE\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n"
        ),
    );
    if code != 0 {
        return Err(dot::Error::Usage {
            message: "shell private directory gate refused",
        });
    }
    Ok(())
}

/// Sorted structural snapshot of `root`: one `rel:kind:mode` line
/// per entry, modes in `stat %a` spelling read in-process. The
/// `.cache` subtree is skipped: the git launcher shim on PATH
/// creates it on first use, symmetrically on both engines and
/// unrelated to the port.
fn snapshot(root: &Path) -> Vec<String> {
    fn lossy(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read fixture dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        entries.sort();
        for path in entries {
            let rel = path
                .strip_prefix(root)
                .expect("fixture prefix")
                .as_os_str()
                .as_bytes();
            if rel == b".cache" || rel.starts_with(b".cache/") {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).expect("stat fixture");
            let mode = meta.permissions().mode() & 0o7777;
            if meta.is_symlink() {
                let target = std::fs::read_link(&path).expect("read link");
                out.push(format!(
                    "{}:link->{}:{mode:o}",
                    lossy(rel),
                    lossy(target.as_os_str().as_bytes()),
                ));
            } else if meta.is_dir() {
                out.push(format!("{}:dir:{mode:o}", lossy(rel)));
                stack.push(path);
            } else {
                out.push(format!("{}:file:{mode:o}", lossy(rel)));
            }
        }
    }
    out.sort();
    out
}

/// One parsed parent-intent record: the shell's five `$REPLY` fields
/// (`phase`, parent, stage, `dev`, `ino`, `mode` — six tab columns).
struct Intent {
    phase: Vec<u8>,
    parent: Vec<u8>,
    stage: Vec<u8>,
    dev: Vec<u8>,
    ino: Vec<u8>,
    mode: Vec<u8>,
}

/// Read and split one intent record. Records are single lines (the
/// publisher always appends the newline); anything else fails the
/// row loudly instead of comparing garbage.
fn read_intent(path: &Path) -> Intent {
    let mut bytes = std::fs::read(path).expect("read intent");
    assert!(
        bytes.ends_with(b"\n"),
        "unterminated intent: {}",
        path.display(),
    );
    bytes.pop();
    assert!(
        !bytes.contains(&b'\n'),
        "multiline intent: {}",
        path.display(),
    );
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == b'\t').collect();
    assert_eq!(
        fields.len(),
        6,
        "intent field count in {}: {:?}",
        path.display(),
        String::from_utf8_lossy(&bytes),
    );
    Intent {
        phase: fields[0].to_vec(),
        parent: fields[1].to_vec(),
        stage: fields[2].to_vec(),
        dev: fields[3].to_vec(),
        ino: fields[4].to_vec(),
        mode: fields[5].to_vec(),
    }
}

/// Intent record names (hashes of the parent spelling) in one
/// transaction directory: real files only. Non-file occupants (the
/// intent-is-a-directory refusal row) compare through the tree
/// snapshots instead of record parsing.
fn intent_names(tx: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(tx)
        .expect("read tx")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.as_bytes().starts_with(b"parent-intent."))
                && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
        })
        .map(|path| {
            path.file_name()
                .expect("intent name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn is_digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|byte| byte.is_ascii_digit())
}

/// Compare the intent journals of both sides: same records, same
/// phase/parent/stage/mode bytes. The `dev`/`ino` columns name live
/// inodes (one per side), so they compare by shape — digits on both
/// sides — while [`assert_consistent`] pins each side's values
/// against its own directories.
fn compare_intents(sides: &Sides, tag: &str) {
    let shell_names = intent_names(&sides.shell_tx);
    let rust_names = intent_names(&sides.rust_tx);
    assert_eq!(rust_names, shell_names, "intent sets diverge for {tag}");
    for name in &shell_names {
        let shell = read_intent(&sides.shell_tx.join(name));
        let rust = read_intent(&sides.rust_tx.join(name));
        assert_eq!(rust.phase, shell.phase, "phase in {name} for {tag}");
        assert_eq!(rust.parent, shell.parent, "parent in {name} for {tag}");
        assert_eq!(rust.stage, shell.stage, "stage in {name} for {tag}");
        assert_eq!(rust.mode, shell.mode, "mode in {name} for {tag}");
        for (side, intent) in [("shell", &shell), ("rust", &rust)] {
            if intent.phase == b"prepared" {
                assert!(
                    is_digits(&intent.dev) && is_digits(&intent.ino),
                    "{side} prepared identity is not numeric in {name} for {tag}",
                );
            } else {
                assert_eq!(intent.phase, b"pending", "unknown phase for {tag}");
                assert_eq!(intent.dev, b"-", "pending dev for {tag}");
                assert_eq!(intent.ino, b"-", "pending ino for {tag}");
                assert_eq!(intent.mode, b"-", "pending mode for {tag}");
            }
        }
    }
}

/// Every prepared record must describe the directory its own engine
/// actually built: the recorded `dev:ino` is the live identity and
/// the recorded mode is the live mode, read in-process.
fn assert_consistent(home: &Path, tx: &Path, tag: &str) {
    for name in intent_names(tx) {
        let intent = read_intent(&tx.join(name));
        if intent.phase != b"prepared" {
            continue;
        }
        let mut current = home.as_os_str().as_bytes().to_vec();
        current.push(b'/');
        current.extend_from_slice(&intent.parent);
        let current = PathBuf::from(OsString::from_vec(current));
        let meta = std::fs::metadata(&current)
            .unwrap_or_else(|_| panic!("prepared current is gone for {tag}"));
        let live = format!("{}:{}", meta.dev(), meta.ino());
        let recorded = format!(
            "{}:{}",
            String::from_utf8_lossy(&intent.dev),
            String::from_utf8_lossy(&intent.ino),
        );
        assert_eq!(live, recorded, "recorded identity is stale for {tag}");
        let live_mode = format!("{:o}", meta.permissions().mode() & 0o7777);
        assert_eq!(
            live_mode,
            String::from_utf8_lossy(&intent.mode),
            "recorded mode is stale for {tag}",
        );
    }
}

/// Oracle: the live `_dot_init_parent_directories`.
fn shell_parents(home: &Path, tx: &Path, relative: &[u8]) -> (i32, Vec<u8>) {
    let tx = tx.as_os_str().to_os_string();
    let relative = OsString::from_vec(relative.to_vec());
    let (code, _, stderr) = shell_run(
        home,
        &[
            ("DOT_TEST_TX", tx.as_os_str()),
            ("DOT_TEST_REL", relative.as_os_str()),
        ],
        "_dot_init_parent_directories \"$DOT_TEST_TX\" \"$DOT_TEST_REL\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
    );
    (code, stderr)
}

/// Drive both engines from identical starts and compare verdicts,
/// trees, journals, and record consistency.
fn run_row(tag: &str, relative: &[u8], setup: impl Fn(&Sides)) {
    let sides = Sides::build(tag);
    setup(&sides);
    let (shell_code, shell_stderr) = shell_parents(&sides.shell_home, &sides.shell_tx, relative);
    let hooks = sides.hooks();
    let mut cache = MoveCache::default();
    let rust_result = parent_directories(
        &hooks,
        &sides.rust_tx,
        relative,
        &sides.rust_home,
        NONCE,
        &mut cache,
    );
    assert_eq!(
        shell_code == 0,
        rust_result.is_ok(),
        "verdicts diverge for {tag}: shell={shell_code} port={rust_result:?} shell-stderr={:?}",
        String::from_utf8_lossy(&shell_stderr),
    );
    assert_eq!(
        snapshot(&sides.rust_home),
        snapshot(&sides.shell_home),
        "home trees diverge for {tag}",
    );
    assert_eq!(
        snapshot(&sides.rust_tx),
        snapshot(&sides.shell_tx),
        "transaction trees diverge for {tag}",
    );
    compare_intents(&sides, tag);
    assert_consistent(&sides.shell_home, &sides.shell_tx, tag);
    assert_consistent(&sides.rust_home, &sides.rust_tx, tag);
}

/// Fixture helpers: the same bytes under either home.
fn make_dir(path: &Path, mode: u32) {
    std::fs::create_dir_all(path).expect("make dir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod dir");
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parent");
    }
    std::fs::write(path, bytes).expect("write fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod file");
}

/// Write one parent-intent record under `tx` for `parent_rel`.
fn write_intent(tx: &Path, parent_rel: &[u8], line: &[u8]) -> PathBuf {
    let hash = git_hash(parent_rel);
    let path = tx.join(format!("parent-intent.{hash}"));
    write_file(&path, line, 0o600);
    path
}

/// Pending line for `parent_rel`, exactly the publisher's spelling.
fn pending_line(parent_rel: &[u8]) -> Vec<u8> {
    let stage = stage_rel_for(parent_rel);
    let mut line = b"pending\t".to_vec();
    line.extend_from_slice(parent_rel);
    line.push(b'\t');
    line.extend_from_slice(&stage);
    line.extend_from_slice(b"\t-\t-\t-\n");
    line
}

fn no_setup(_: &Sides) {}

#[test]
fn parents_top_level_is_a_noop() {
    run_row("parents-top-level", b"f.txt", no_setup);
}

#[test]
fn parents_empty_relative_is_a_noop() {
    run_row("parents-empty", b"", no_setup);
}

#[test]
fn parents_root_relative_is_a_noop() {
    // `/` strips to an empty parent whose split reads no fields,
    // so the loop never runs: a successful no-op on both engines.
    run_row("parents-root", b"/", no_setup);
}

#[test]
fn parents_single_missing_level() {
    run_row("parents-single", b"a/f.txt", no_setup);
}

#[test]
fn parents_nested_missing_levels() {
    run_row("parents-nested", b"a/b/c/f.txt", no_setup);
}

#[test]
fn parents_trailing_slash_builds_the_named_dir() {
    // `a/` strips to parent `a`, which the loop then publishes.
    run_row("parents-trailing-slash", b"a/", no_setup);
}

/// A pre-existing real directory is kept as-is: no intent is
/// published for its level, while deeper missing levels proceed.
#[test]
fn parents_existing_directory_continues() {
    run_row("parents-existing-dir", b"a/b/f.txt", |sides| {
        for home in [&sides.shell_home, &sides.rust_home] {
            make_dir(&home.join("a"), 0o755);
        }
    });
}

/// A regular file blocking a parent level refuses on both engines.
#[test]
fn parents_file_block_refuses() {
    run_row("parents-file-block", b"a/f.txt", |sides| {
        for home in [&sides.shell_home, &sides.rust_home] {
            write_file(&home.join("a"), b"block", 0o644);
        }
    });
}

/// A dangling symlink blocking a parent level refuses: it exists
/// lexically but is no real directory.
#[test]
fn parents_dangling_link_refuses() {
    run_row("parents-dangling", b"a/f.txt", |sides| {
        for home in [&sides.shell_home, &sides.rust_home] {
            std::os::unix::fs::symlink("nowhere", home.join("a")).expect("link");
        }
    });
}

/// A symlink to a live directory still refuses: the gate demands a
/// real directory, never a link.
#[test]
fn parents_dir_link_refuses() {
    run_row("parents-dir-link", b"a/f.txt", |sides| {
        for home in [&sides.shell_home, &sides.rust_home] {
            make_dir(&home.join("target"), 0o755);
            std::os::unix::fs::symlink("target", home.join("a")).expect("link");
        }
    });
}

/// A correct pending record with no stage yet resumes through
/// mkdir, claim, preparation, and the move.
#[test]
fn parents_pending_record_resumes() {
    run_row("parents-pending", b"a/f.txt", |sides| {
        let line = pending_line(b"a");
        for tx in [&sides.shell_tx, &sides.rust_tx] {
            write_intent(tx, b"a", &line);
        }
    });
}

/// A foreign directory squatting on the derived stage path refuses:
/// the pending arm only proceeds while the stage is absent.
#[test]
fn parents_foreign_stage_refuses() {
    run_row("parents-foreign-stage", b"a/f.txt", |sides| {
        let stage = stage_rel_for(b"a");
        for home in [&sides.shell_home, &sides.rust_home] {
            let path = PathBuf::from(OsString::from_vec(
                [home.as_os_str().as_bytes(), b"/", &stage].concat(),
            ));
            make_dir(&path, 0o755);
        }
    });
}

/// A record naming another run's stage refuses in the reader: the
/// recorded path must equal the nonce-derived stage.
#[test]
fn parents_stale_record_refuses() {
    run_row("parents-stale", b"a/f.txt", |sides| {
        let hash = git_hash(b"a");
        let line = format!("pending\ta\ta/.dot-init-parent.other.{hash}\t-\t-\t-\n");
        for tx in [&sides.shell_tx, &sides.rust_tx] {
            write_intent(tx, b"a", line.as_bytes());
        }
    });
}

/// A directory at the intent path refuses in the reader: records
/// must be real files.
#[test]
fn parents_intent_is_dir_refuses() {
    run_row("parents-intent-dir", b"a/f.txt", |sides| {
        let hash = git_hash(b"a");
        for tx in [&sides.shell_tx, &sides.rust_tx] {
            make_dir(&tx.join(format!("parent-intent.{hash}")), 0o755);
        }
    });
}

/// A second run over published parents succeeds without touching
/// anything: prepared records plus live directories continue.
#[test]
fn parents_rerun_is_stable() {
    let sides = Sides::build("parents-rerun");
    let relative = b"a/b/f.txt";
    let rel = OsString::from_vec(relative.to_vec());
    let shell_tx = sides.shell_tx.as_os_str().to_os_string();
    let (first, _, _) = shell_run(
        &sides.shell_home,
        &[
            ("DOT_TEST_TX", shell_tx.as_os_str()),
            ("DOT_TEST_REL", rel.as_os_str()),
        ],
        "_dot_init_parent_directories \"$DOT_TEST_TX\" \"$DOT_TEST_REL\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
    );
    assert_eq!(first, 0, "oracle first run failed");
    let hooks = sides.hooks();
    let mut cache = MoveCache::default();
    parent_directories(
        &hooks,
        &sides.rust_tx,
        relative,
        &sides.rust_home,
        NONCE,
        &mut cache,
    )
    .expect("port first run failed");
    let before_home = snapshot(&sides.rust_home);
    let before_tx = snapshot(&sides.rust_tx);
    run_row_on(&sides, "parents-rerun", relative);
    assert_eq!(snapshot(&sides.rust_home), before_home, "rerun moved home");
    assert_eq!(snapshot(&sides.rust_tx), before_tx, "rerun moved tx");
}

/// Drive both engines on already-built sides (the first pass ran
/// above), comparing exactly like [`run_row`].
fn run_row_on(sides: &Sides, tag: &str, relative: &[u8]) {
    let (shell_code, shell_stderr) = shell_parents(&sides.shell_home, &sides.shell_tx, relative);
    let hooks = sides.hooks();
    let mut cache = MoveCache::default();
    let rust_result = parent_directories(
        &hooks,
        &sides.rust_tx,
        relative,
        &sides.rust_home,
        NONCE,
        &mut cache,
    );
    assert_eq!(
        shell_code == 0,
        rust_result.is_ok(),
        "verdicts diverge for {tag}: shell={shell_code} port={rust_result:?} {:?}",
        String::from_utf8_lossy(&shell_stderr),
    );
    assert_eq!(
        snapshot(&sides.rust_home),
        snapshot(&sides.shell_home),
        "home trees diverge for {tag}",
    );
    compare_intents(sides, tag);
    assert_consistent(&sides.shell_home, &sides.shell_tx, tag);
    assert_consistent(&sides.rust_home, &sides.rust_tx, tag);
}

/// A prepared record with its live stage still parked resumes
/// through claim removal and the move: the stage carries the exact
/// claim of this run, so the engine adopts and renames it.
#[test]
fn parents_prepared_stage_resumes() {
    let sides = Sides::build("parents-prepared");
    let parent_rel = b"a";
    let stage_rel = stage_rel_for(parent_rel);
    for (home, tx) in [
        (&sides.shell_home, &sides.shell_tx),
        (&sides.rust_home, &sides.rust_tx),
    ] {
        let stage = PathBuf::from(OsString::from_vec(
            [home.as_os_str().as_bytes(), b"/", &stage_rel].concat(),
        ));
        make_dir(&stage, 0o700);
        let stage_arg = stage.as_os_str().to_os_string();
        let (code, _, _) = shell_run(
            home,
            &[
                ("DOT_TEST_STAGE", stage_arg.as_os_str()),
                ("DOT_TEST_PATH", OsStr::from_bytes(parent_rel)),
            ],
            "_dot_init_stage_claim_write \"$DOT_TEST_STAGE\" parent \"$DOT_TEST_PATH\"\ncode=$?\nprintf 'code=%s\\n' \"$code\" >&2\n",
        );
        assert_eq!(code, 0, "claim setup failed");
        let meta = std::fs::metadata(&stage).expect("stage stat");
        let line = format!(
            "prepared\ta\t{}\t{}\t{}\t{:o}\n",
            String::from_utf8_lossy(&stage_rel),
            meta.dev(),
            meta.ino(),
            meta.permissions().mode() & 0o7777,
        );
        write_intent(tx, parent_rel, line.as_bytes());
    }
    run_row_on(&sides, "parents-prepared", b"a/f.txt");
    for home in [&sides.shell_home, &sides.rust_home] {
        let current = home.join("a");
        let meta = std::fs::symlink_metadata(&current).expect("published dir stat");
        assert!(meta.is_dir(), "published parent is no directory");
        assert!(
            std::fs::symlink_metadata(current.join(CLAIM_NAME)).is_err(),
            "claim marker leaked into the published dir",
        );
    }
}

/// A mid-tree file blocks only the deeper level: the existing real
/// directory above is kept, then the file refuses.
#[test]
fn parents_mid_file_block_refuses() {
    run_row("parents-mid-block", b"a/b/f.txt", |sides| {
        for home in [&sides.shell_home, &sides.rust_home] {
            make_dir(&home.join("a"), 0o755);
            write_file(&home.join("a/b"), b"block", 0o644);
        }
    });
}
