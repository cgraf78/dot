//! Differential parity tests for the init plan-review and
//! conflict-safekeeping family (`lib/dot/init-client.sh`) against the
//! live shell: the plan summary, the confirmation gate, the conflict
//! backup and restore pair, and the completion publication.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories, so
//! manifests, backups, previews, and completion markers never
//! collide. Cross-lane predicates (`_dot_init_path_state_matches`,
//! `_dot_init_private_directory`) cross the port as closures; rows
//! marked `live` feed a closure that runs the real shell predicate,
//! while rows marked `record` override the shell predicate with a
//! logging stub and compare the normalized call log plus the
//! verdict, pinning field assignment on adversarial rows (leading
//! tabs, doubled tabs, extra fields, blank lines, unterminated
//! tails) that journals never carry but the parser must survive.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_plan as plan;
use dot::temp::MoveCache;
use dot::test_support::TempDir;

/// Sources for the plan chapter: the resource runtime, the shared
/// temp helpers (hashing, exclusive moves), the XDG root, and the
/// init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the plan runtime sourced and report
/// the verdict the snippet printed alongside both byte streams.
/// Every probe ends with `printf 'code=%s\n' "$code"`, so the
/// returned code is that verdict — not the process status, which
/// only says the printer ran. A snippet that never reports (a
/// harness bug, never a pass) yields 99.
///
/// The locale stays pinned: git diagnostics must read English on
/// both engines, and the port pins `LC_ALL=C` around every git run.
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

/// Whether the ambient `/dev/tty` refuses a write from a scratch
/// shell: only then can the live refusal row run without hanging a
/// real terminal at the interactive read. Probes the prompt step
/// (the shell fails there before ever reading), so a refusal here
/// predicts a deterministic refusal there.
fn tty_refuses_write() -> bool {
    let probe = Command::new(dot::test_support::bash())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg("printf x >/dev/tty")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    !probe.is_ok_and(|output| output.status.success())
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so manifests and backups never
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

    fn root(&self) -> &Path {
        self._dir.path()
    }
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// File mode bits (`stat %a` spelling) for assertions.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("fixture stat")
        .permissions()
        .mode()
        & 0o777
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

/// Lexical existence plus bytes: the observable end-state of a row
/// on one side. Symlinks report their target text.
fn shape(path: &Path) -> (bool, Vec<u8>) {
    let exists = std::fs::symlink_metadata(path).is_ok();
    let content = std::fs::read_link(path)
        .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
        .or_else(|_| std::fs::read(path))
        .unwrap_or_default();
    (exists, content)
}

/// Snapshot one side-relative path the shell way, via the live
/// `_dot_init_snapshot_path`: returns the manifest row
/// (`rel\tkind\tdev\tino\tmode\tsize\tvalue`, no trailing
/// newline) as the behavior-neutral oracle for row construction.
/// Row bytes come from the shell on both sides, so only the
/// orchestration under test can diverge.
fn snapshot_row(home: &Path, rel: &str) -> String {
    let body = format!(
        "if row=$(_dot_init_snapshot_path {}); then code=0; else code=$?; row=; fi\nprintf 'row=%s\\ncode=%s\\n' \"$row\" \"$code\"\n",
        sq(home.join(rel).to_str().expect("fixture path"))
    );
    let (code, out, _) = shell_run(home, &[], &body);
    assert_eq!(code, 0, "snapshot {rel}");
    let text = String::from_utf8_lossy(&out);
    let row = text
        .lines()
        .find_map(|line| line.strip_prefix("row="))
        .unwrap_or_default();
    assert!(!row.is_empty(), "snapshot row {rel}");
    format!("{rel}\t{row}")
}

/// Live worktree-state matcher: runs the real shell predicate per
/// call, so `live` rows exercise true end-to-end parity.
fn live_matches(home: PathBuf) -> impl Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool {
    move |target, kind, dev, ino, mode, size, value| {
        let body = format!(
            "if _dot_init_path_state_matches {} {} {} {} {} {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(target.to_str().expect("match path")),
            sq(kind),
            sq(dev),
            sq(ino),
            sq(mode),
            sq(size),
            sq(value)
        );
        shell_run(&home, &[], &body).0 == 0
    }
}

/// Live private-directory provision: runs the real shell helper.
fn live_private_dir(home: PathBuf) -> impl Fn(&Path) -> dot::Result<()> {
    move |path| {
        let body = format!(
            "if _dot_init_private_directory {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(path.to_str().expect("private path"))
        );
        if shell_run(&home, &[], &body).0 == 0 {
            Ok(())
        } else {
            Err(dot::errors::Error::Usage {
                message: "private directory refused",
            })
        }
    }
}

/// Failing provisioner for refusal rows.
fn failing_private_dir() -> impl Fn(&Path) -> dot::Result<()> {
    |_| {
        Err(dot::errors::Error::Usage {
            message: "private directory refused",
        })
    }
}

/// Recording matcher for `record` rows: logs normalized calls and
/// answers from a script.
struct Recorder {
    calls: RefCell<Vec<String>>,
    answers: RefCell<Vec<bool>>,
}

impl Recorder {
    fn new(answers: Vec<bool>) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            answers: RefCell::new(answers),
        }
    }

    fn matcher(&self) -> impl Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool + '_ {
        |target, kind, dev, ino, mode, size, value| {
            self.calls.borrow_mut().push(format!(
                "{}|{kind}|{dev}|{ino}|{mode}|{size}|{value}",
                target.display()
            ));
            if self.answers.borrow().is_empty() {
                true
            } else {
                self.answers.borrow_mut().remove(0)
            }
        }
    }

    fn take(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

/// Shell-side recording stub for `record` rows: overrides the live
/// predicate with a logger that answers from `$STUB_ANSWERS`
/// (`1`/`0` per call) and prints `call=` lines for the harness.
const RECORD_STUB: &str = concat!(
    "_dot_init_path_state_matches() {\n",
    "  printf 'call=%s|%s|%s|%s|%s|%s|%s\\n' \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$6\" \"$7\"\n",
    "  case $STUB_ANSWERS in\n",
    "    1*) STUB_ANSWERS=${STUB_ANSWERS#1} ;;\n",
    "    0*) STUB_ANSWERS=${STUB_ANSWERS#0}; return 1 ;;\n",
    "    *) return 1 ;;\n",
    "  esac\n",
    "}\n",
);

/// Normalize a recorded target path for cross-side comparison: the
/// twin homes differ, so only the side-relative tail compares.
fn normalize_call(call: &str, home: &Path) -> String {
    let prefix = format!("{}/", home.to_str().expect("home text"));
    call.replacen(&prefix, "HOME/", 1)
}

#[test]
fn confirm_empty_manifest_is_silent() {
    for name in ["missing", "empty"] {
        let dir = TempDir::new("plan-confirm-empty").expect("temp dir");
        let manifest = dir.path().join("manifest");
        if name == "empty" {
            std::fs::write(&manifest, b"").expect("empty manifest");
        }
        // The listing goes to stderr; stdout carries only the verdict.
        let listing_body = format!(
            "if _dot_init_confirm {} true >/dev/null; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(manifest.to_str().expect("manifest path")),
        );
        let (shell_code, _, shell_err) = shell_run(dir.path(), &[], &listing_body);
        assert_eq!(shell_code, 0, "shell {name}");
        assert!(shell_err.is_empty(), "shell {name} silent");
        let rust = plan::confirm(&manifest, true, Path::new("/dev/tty")).expect("rust {name}");
        assert!(rust.is_empty(), "rust {name} silent");
    }
}

#[test]
fn confirm_listing_matches_cut_first() {
    // Adversarial manifest: a plain row, a path-less row, a blank
    // line, a leading-tab row (`cut` reports empty, no stripping),
    // a doubled-tab row, an extra-fields row, and an unterminated
    // tail. Both engines must print the identical listing.
    let dir = TempDir::new("plan-confirm-list").expect("temp dir");
    let manifest = dir.path().join("manifest");
    // BSD cut truncates fields at NUL bytes, so a NUL row has no
    // portable shell spelling on macOS; byte-exactness for that row
    // is probed on Linux CI instead (fleet non-UTF8 precedent).
    let content = if cfg!(target_os = "macos") {
        "file1\tregular\t1\t2\t644\t3\tabc\nNOTAB\n\n\tlead\tk\np\t\tk\td\ti\tm\ts\tv\na\tb\tc\td\te\tf\tg\te1\ntail-row"
    } else {
        "file1\tregular\t1\t2\t644\t3\tabc\nNOTAB\n\n\tlead\tk\np\t\tk\td\ti\tm\ts\tv\na\tb\tc\td\te\tf\tg\te1\nn\0ul\tk\ntail-row"
    };
    std::fs::write(&manifest, content).expect("manifest");
    let body = format!(
        "if _dot_init_confirm {} true >/dev/null; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(manifest.to_str().expect("manifest path")),
    );
    let (shell_code, _, shell_err) = shell_run(dir.path(), &[], &body);
    assert_eq!(shell_code, 0, "shell listing");
    let rust = plan::confirm(&manifest, true, Path::new("/dev/tty")).expect("rust listing");
    assert_eq!(rust, shell_err, "listing bytes");
    let text = String::from_utf8_lossy(&rust);
    assert!(text.starts_with("dot init: conflicting paths will be backed up:\n"));
    assert!(text.contains("\n  file1\n"));
    assert!(text.contains("\n  NOTAB\n"));
    // Blank line and leading-tab row both list an empty field.
    assert!(text.contains("\n  \n"));
    assert!(text.contains("\n  p\n"));
    assert!(text.contains("\n  a\n"));
    // GNU `cut` preserves NUL bytes in the listed field; BSD cut
    // truncates at NUL, so this row only exists off macOS.
    if !cfg!(target_os = "macos") {
        assert!(text.contains("\n  n\0ul\n"));
    }
    assert!(text.contains("\n  tail-row\n"));
}

#[test]
fn confirm_without_yes_refuses() {
    let dir = TempDir::new("plan-confirm-no").expect("temp dir");
    let manifest = dir.path().join("manifest");
    std::fs::write(&manifest, b"file1\tregular\t1\t2\t644\t3\tabc\n").expect("manifest");
    // Live shell comparison only when the prompt step deterministically
    // fails here: on a real terminal the shell would block reading the
    // answer, which no test may do.
    if tty_refuses_write() {
        let body = format!(
            "if _dot_init_confirm {} false >/dev/null; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            sq(manifest.to_str().expect("manifest path")),
        );
        let (shell_code, _, shell_err) = shell_run(dir.path(), &[], &body);
        assert_eq!(shell_code, 1, "shell refuses");
        let noise = String::from_utf8_lossy(&shell_err);
        assert!(
            shell_err.is_empty()
                || noise.contains("conflicts require --yes")
                || noise.contains("/dev/tty"),
            "refusal noise is the gate diagnostic or bash's own redirection error, got: {noise}"
        );
        assert!(
            plan::confirm(&manifest, false, Path::new("/dev/tty")).is_err(),
            "rust refuses on the live terminal"
        );
    } else {
        eprintln!("live terminal present: skipping live refusal comparison");
    }
    // Deterministic refusal rows with a pinned terminal path: a
    // missing node fails the open, and a regular file fails the
    // read (the prompt write lands in the file, so the answer never
    // matches). The shell hardcodes /dev/tty, so these pin the port
    // alone — the three branches above carry the live comparison.
    assert!(
        plan::confirm(&manifest, false, &dir.path().join("no-such-tty")).is_err(),
        "missing terminal refuses"
    );
    let regular = dir.path().join("regular-tty");
    std::fs::write(&regular, b"yes\n").expect("regular tty");
    assert!(
        plan::confirm(&manifest, false, &regular).is_err(),
        "regular file refuses"
    );
}

/// One plan case: twin candidate checkouts (identical content, so
/// previews compare) plus twin tree journals.
struct PlanCase {
    _dir: TempDir,
    shell_candidate: PathBuf,
    rust_candidate: PathBuf,
    shell_tree: PathBuf,
    rust_tree: PathBuf,
}

/// Build a candidate checkout holding `config` as
/// `.config/dot/config` (or no config blob when `None`), plus twin
/// tree journals with `tree_text`.
fn plan_case(tag: &str, config: Option<&str>, tree_text: &str) -> PlanCase {
    let dir = TempDir::new(tag).expect("temp dir");
    let root = dir.path().to_path_buf();
    let shell_candidate = root.join("sh-cand");
    let rust_candidate = root.join("rs-cand");
    for candidate in [&shell_candidate, &rust_candidate] {
        git(&[
            "init",
            "--quiet",
            "--initial-branch",
            "main",
            candidate.to_str().expect("candidate path"),
        ]);
        if let Some(body) = config {
            write(candidate, ".config/dot/config", body.as_bytes());
        } else {
            write(candidate, "other.txt", b"unrelated\n");
        }
        let text = candidate.to_str().expect("candidate path").to_string();
        git(&["-C", &text, "add", "-A"]);
        git(&["-C", &text, "commit", "--quiet", "-m", "candidate"]);
    }
    let shell_tree = root.join("sh-tree");
    let rust_tree = root.join("rs-tree");
    std::fs::write(&shell_tree, tree_text).expect("shell tree");
    std::fs::write(&rust_tree, tree_text).expect("rust tree");
    PlanCase {
        _dir: dir,
        shell_candidate,
        rust_candidate,
        shell_tree,
        rust_tree,
    }
}

/// Preview bytes on one side, `None` when the summary wrote none.
fn preview_of(candidate: &Path) -> Option<Vec<u8>> {
    let preview = candidate.join("dot-config.preview");
    std::fs::read(&preview).ok()
}

/// Run the shell summary over one candidate; returns the verdict
/// plus the raw stderr report. Probes run under `set -o pipefail`,
/// the engine flags from `lib/dot/main.sh`: without it the shell's
/// `count=$(wc -l | tr ...)` masks a failed `wc` and reports an
/// empty count where the engine (and the port) refuse. Every other
/// pipeline in this family succeeds-or-fails identically either way,
/// so the flag only affects the missing-tree row by design.
fn shell_summary(
    home: &Path,
    candidate: &Path,
    tree: &Path,
    backup: &str,
    identity: &str,
    skip: bool,
) -> (i32, Vec<u8>) {
    let body = format!(
        "set -o pipefail\nif _dot_init_plan_summary {} {} {} {} {} >/dev/null; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(candidate.to_str().expect("candidate path")),
        sq("main"),
        sq(tree.to_str().expect("tree path")),
        sq(backup),
        sq(identity),
    );
    let env = [("DOT_INIT_SKIP_PROVIDER", if skip { "1" } else { "0" })];
    let (code, _, err) = shell_run(home, &env, &body);
    (code, err)
}

#[test]
fn plan_summary_reports() {
    let backup = "/home/u/.local/state/dot/init/backup";
    let identity = "github.com/example/dot";
    // No config blob: compiled-in defaults, no preview.
    let case = plan_case("plan-defaults", None, "a\tb\nc\td\ne\tf\n");
    let (shell_code, shell_err) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        false,
    );
    assert_eq!(shell_code, 0, "shell defaults");
    let rust = plan::plan_summary(&plan::PlanInputs {
        candidate: &case.rust_candidate,
        branch: "main",
        tree: &case.rust_tree,
        backup,
        identity,
        home: case._dir.path(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: false,
    })
    .expect("rust defaults");
    assert_eq!(rust, shell_err, "defaults report");
    assert!(String::from_utf8_lossy(&rust).contains("tracked paths: 3\n"));
    assert_eq!(preview_of(&case.shell_candidate), None);
    assert_eq!(preview_of(&case.rust_candidate), None);

    // Minimal config: provider none, policy pinned, no extensions.
    let case = plan_case("plan-minimal", Some("version=1\n"), "only\n");
    let (shell_code, shell_err) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        false,
    );
    assert_eq!(shell_code, 0, "shell minimal");
    let rust = plan::plan_summary(&plan::PlanInputs {
        candidate: &case.rust_candidate,
        branch: "main",
        tree: &case.rust_tree,
        backup,
        identity,
        home: case._dir.path(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: false,
    })
    .expect("rust minimal");
    assert_eq!(rust, shell_err, "minimal report");
    let text = String::from_utf8_lossy(&rust);
    assert!(text.contains("tracked paths: 1\n"));
    assert!(text.contains("dependency provider: none\n"));
    assert!(text.contains("shdeps update policy: pinned\n"));
    assert!(text.contains("extensions: disabled\n"));
    assert_eq!(
        preview_of(&case.shell_candidate),
        preview_of(&case.rust_candidate),
        "preview bytes"
    );
    assert_eq!(
        preview_of(&case.rust_candidate),
        Some(b"version=1\n".to_vec()),
        "preview content"
    );

    // Full config: non-default triple, counted tree.
    let config =
        "version=1\nextension_api=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n";
    let case = plan_case("plan-full", Some(config), "a\nb\nc\nd\n");
    let (shell_code, shell_err) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        false,
    );
    assert_eq!(shell_code, 0, "shell full");
    let rust = plan::plan_summary(&plan::PlanInputs {
        candidate: &case.rust_candidate,
        branch: "main",
        tree: &case.rust_tree,
        backup,
        identity,
        home: case._dir.path(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: false,
    })
    .expect("rust full");
    assert_eq!(rust, shell_err, "full report");
    let text = String::from_utf8_lossy(&rust);
    assert!(text.contains("tracked paths: 4\n"));
    assert!(text.contains("dependency provider: shdeps\n"));
    assert!(text.contains("shdeps update policy: latest\n"));
    assert!(text.contains("extensions: enabled\n"));

    // Unterminated tree: `wc -l` counts newline bytes only.
    let case = plan_case("plan-unterminated", None, "a\nb");
    let (shell_code, shell_err) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        false,
    );
    assert_eq!(shell_code, 0, "shell unterminated");
    let rust = plan::plan_summary(&plan::PlanInputs {
        candidate: &case.rust_candidate,
        branch: "main",
        tree: &case.rust_tree,
        backup,
        identity,
        home: case._dir.path(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: false,
    })
    .expect("rust unterminated");
    assert_eq!(rust, shell_err, "unterminated report");
    assert!(
        String::from_utf8_lossy(&rust).contains("tracked paths: 1\n"),
        "newline count, not line count"
    );
}

#[test]
fn plan_summary_flags_and_failures() {
    let backup = "/home/u/.local/state/dot/init/backup";
    let identity = "github.com/example/dot";
    let config =
        "version=1\nextension_api=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\n";

    // Skip flag annotates a real provider.
    let case = plan_case("plan-skip", Some(config), "a\n");
    let (shell_code, shell_err) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        true,
    );
    assert_eq!(shell_code, 0, "shell skip");
    let rust = plan::plan_summary(&plan::PlanInputs {
        candidate: &case.rust_candidate,
        branch: "main",
        tree: &case.rust_tree,
        backup,
        identity,
        home: case._dir.path(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: true,
    })
    .expect("rust skip");
    assert_eq!(rust, shell_err, "skip report");
    assert!(
        String::from_utf8_lossy(&rust)
            .contains("dependency provider: shdeps (skipped for this invocation)\n")
    );

    // Skip flag leaves `none` alone.
    let case = plan_case("plan-skip-none", Some("version=1\n"), "a\n");
    let (shell_code, shell_err) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        true,
    );
    assert_eq!(shell_code, 0, "shell skip none");
    let rust = plan::plan_summary(&plan::PlanInputs {
        candidate: &case.rust_candidate,
        branch: "main",
        tree: &case.rust_tree,
        backup,
        identity,
        home: case._dir.path(),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider: true,
    })
    .expect("rust skip none");
    assert_eq!(rust, shell_err, "skip none report");
    assert!(
        String::from_utf8_lossy(&rust).contains("dependency provider: none\n"),
        "no annotation on none"
    );

    // Unreadable tree refuses on both engines.
    let case = plan_case("plan-no-tree", None, "a\n");
    std::fs::remove_file(&case.shell_tree).expect("remove shell tree");
    std::fs::remove_file(&case.rust_tree).expect("remove rust tree");
    let (shell_code, _) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        false,
    );
    assert_eq!(shell_code, 1, "shell refuses missing tree");
    assert!(
        plan::plan_summary(&plan::PlanInputs {
            candidate: &case.rust_candidate,
            branch: "main",
            tree: &case.rust_tree,
            backup,
            identity,
            home: case._dir.path(),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            skip_provider: false,
        })
        .is_err(),
        "rust refuses missing tree"
    );

    // Garbage config: the child fails, both refuse, and the preview
    // the `show` wrote stays behind with the blob bytes.
    let case = plan_case("plan-garbage", Some("bogus\n"), "a\n");
    let (shell_code, _) = shell_summary(
        case._dir.path(),
        &case.shell_candidate,
        &case.shell_tree,
        backup,
        identity,
        false,
    );
    assert_eq!(shell_code, 1, "shell refuses garbage config");
    assert!(
        plan::plan_summary(&plan::PlanInputs {
            candidate: &case.rust_candidate,
            branch: "main",
            tree: &case.rust_tree,
            backup,
            identity,
            home: case._dir.path(),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            skip_provider: false,
        })
        .is_err(),
        "rust refuses garbage config"
    );
    assert_eq!(
        preview_of(&case.shell_candidate),
        preview_of(&case.rust_candidate),
        "leftover preview"
    );
    assert_eq!(
        preview_of(&case.rust_candidate),
        Some(b"bogus\n".to_vec()),
        "preview holds the blob"
    );
}

/// Seed one conflict tree under `home`: two regular files (one
/// nested), a directory with content, and a symlink.
fn seed_conflict_tree(home: &Path) {
    write(home, "file1", b"hello\n");
    write(home, "sub/file2", b"nested\n");
    write(home, "dir1/inner", b"inner\n");
    std::os::unix::fs::symlink("file1", home.join("link1")).expect("fixture link");
}

/// Manifest text snapshotting `rels` under `home` the shell way,
/// plus any `extra` rows appended verbatim.
fn manifest_for(home: &Path, rels: &[&str], extra: &[&str]) -> String {
    let mut text = String::new();
    for rel in rels {
        text.push_str(&snapshot_row(home, rel));
        text.push('\n');
    }
    for row in extra {
        text.push_str(row);
        text.push('\n');
    }
    text
}

/// Run the shell move over one side; returns the verdict.
fn shell_move(home: &Path, manifest: &Path, backup: &Path, prelude: &str) -> i32 {
    let body = format!(
        "{prelude}if _dot_init_move_conflicts {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(manifest.to_str().expect("manifest path")),
        sq(backup.to_str().expect("backup path")),
    );
    shell_run(home, &[], &body).0
}

/// Stored-manifest bytes on one side, `None` when absent.
fn stored_manifest(backup: &Path) -> Option<Vec<u8>> {
    std::fs::read(backup.join("manifest")).ok()
}

#[test]
fn move_conflicts_parks_live_tree() {
    let twins = Twins::build("plan-move-live");
    let rels = ["file1", "sub/file2", "dir1", "link1"];
    let extra = ["ghost.txt\tabsent\t-\t-\t-\t-\t-"];
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflict_tree(home);
    }
    // Per-side manifests: device and inode bytes differ by home.
    let shell_manifest = twins.root().join("sh-manifest");
    let rust_manifest = twins.root().join("rs-manifest");
    std::fs::write(
        &shell_manifest,
        manifest_for(&twins.shell_home, &rels, &extra),
    )
    .expect("shell manifest");
    std::fs::write(
        &rust_manifest,
        manifest_for(&twins.rust_home, &rels, &extra),
    )
    .expect("rust manifest");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    // Original bytes for the end-state comparison.
    let mut originals = Vec::new();
    for rel in rels {
        originals.push((rel, shape(&twins.shell_home.join(rel))));
    }

    let shell_code = shell_move(&twins.shell_home, &shell_manifest, &shell_backup, "");
    assert_eq!(shell_code, 0, "shell parks");
    let mut cache = MoveCache::default();
    plan::move_conflicts(
        &rust_manifest,
        &rust_backup,
        &twins.rust_home,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &live_matches(twins.rust_home.clone()),
        &live_private_dir(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust parks");

    // Stored manifests echo the input at mode 600 on both sides.
    for (manifest, backup) in [
        (&shell_manifest, &shell_backup),
        (&rust_manifest, &rust_backup),
    ] {
        assert_eq!(
            stored_manifest(backup),
            Some(std::fs::read(manifest).expect("input manifest")),
            "stored manifest"
        );
        assert_eq!(mode_of(&backup.join("manifest")), 0o600, "manifest mode");
    }
    // Every live path moved with its bytes; the absent row no-ops.
    for (rel, (_, bytes)) in &originals {
        assert_eq!(shape(&twins.shell_home.join(rel)), (false, Vec::new()));
        assert_eq!(shape(&twins.rust_home.join(rel)), (false, Vec::new()));
        assert_eq!(shape(&shell_backup.join(rel)), (true, bytes.clone()));
        assert_eq!(shape(&rust_backup.join(rel)), (true, bytes.clone()));
    }
    for backup in [&shell_backup, &rust_backup] {
        assert_eq!(
            shape(&backup.join("dir1/inner")),
            (true, b"inner\n".to_vec()),
            "directory content follows"
        );
        assert_eq!(
            shape(&backup.join("link1")),
            (true, b"file1".to_vec()),
            "link target text follows"
        );
    }

    // A second run reuses the stored manifest and skips parked rows.
    let shell_code = shell_move(&twins.shell_home, &shell_manifest, &shell_backup, "");
    assert_eq!(shell_code, 0, "shell reuses");
    let mut cache = MoveCache::default();
    plan::move_conflicts(
        &rust_manifest,
        &rust_backup,
        &twins.rust_home,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &live_matches(twins.rust_home.clone()),
        &live_private_dir(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust reuses");
}

#[test]
fn move_conflicts_refuses() {
    // Changed input after a stored manifest: the equality gate fires.
    let twins = Twins::build("plan-move-changed");
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflict_tree(home);
    }
    let shell_manifest = twins.root().join("sh-manifest");
    let rust_manifest = twins.root().join("rs-manifest");
    std::fs::write(
        &shell_manifest,
        manifest_for(&twins.shell_home, &["file1"], &[]),
    )
    .expect("manifest");
    std::fs::write(
        &rust_manifest,
        manifest_for(&twins.rust_home, &["file1"], &[]),
    )
    .expect("manifest");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    assert_eq!(
        shell_move(&twins.shell_home, &shell_manifest, &shell_backup, ""),
        0,
        "shell first parks"
    );
    let mut cache = MoveCache::default();
    plan::move_conflicts(
        &rust_manifest,
        &rust_backup,
        &twins.rust_home,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &live_matches(twins.rust_home.clone()),
        &live_private_dir(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust first parks");
    // Rewrite the inputs with an extra row: stored copies disagree.
    std::fs::write(
        &shell_manifest,
        manifest_for(&twins.shell_home, &["sub/file2"], &[]),
    )
    .expect("rewrite");
    std::fs::write(
        &rust_manifest,
        manifest_for(&twins.rust_home, &["sub/file2"], &[]),
    )
    .expect("rewrite");
    assert_eq!(
        shell_move(&twins.shell_home, &shell_manifest, &shell_backup, ""),
        1,
        "shell refuses changed manifest"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::move_conflicts(
            &rust_manifest,
            &rust_backup,
            &twins.rust_home,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &live_matches(twins.rust_home.clone()),
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses changed manifest"
    );
    // Nothing new parked on either side.
    assert!(!exists_side(&shell_backup.join("sub/file2")));
    assert!(!exists_side(&rust_backup.join("sub/file2")));

    // Changed home content: the home-state gate fires.
    let twins = Twins::build("plan-move-mismatch");
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflict_tree(home);
    }
    let shell_manifest = twins.root().join("sh-manifest");
    let rust_manifest = twins.root().join("rs-manifest");
    std::fs::write(
        &shell_manifest,
        manifest_for(&twins.shell_home, &["file1"], &[]),
    )
    .expect("manifest");
    std::fs::write(
        &rust_manifest,
        manifest_for(&twins.rust_home, &["file1"], &[]),
    )
    .expect("manifest");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    write(&twins.shell_home, "file1", b"tampered\n");
    write(&twins.rust_home, "file1", b"tampered\n");
    assert_eq!(
        shell_move(&twins.shell_home, &shell_manifest, &shell_backup, ""),
        1,
        "shell refuses mismatch"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::move_conflicts(
            &rust_manifest,
            &rust_backup,
            &twins.rust_home,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &live_matches(twins.rust_home.clone()),
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses mismatch"
    );
    assert!(!exists_side(&shell_backup.join("file1")));
    assert!(!exists_side(&rust_backup.join("file1")));

    // Occupied destination with live home: parked elsewhere first.
    let twins = Twins::build("plan-move-occupied");
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflict_tree(home);
    }
    let shell_manifest = twins.root().join("sh-manifest");
    let rust_manifest = twins.root().join("rs-manifest");
    std::fs::write(
        &shell_manifest,
        manifest_for(&twins.shell_home, &["file1"], &[]),
    )
    .expect("manifest");
    std::fs::write(
        &rust_manifest,
        manifest_for(&twins.rust_home, &["file1"], &[]),
    )
    .expect("manifest");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    write(&shell_backup, "file1", b"squatter\n");
    write(&rust_backup, "file1", b"squatter\n");
    assert_eq!(
        shell_move(&twins.shell_home, &shell_manifest, &shell_backup, ""),
        1,
        "shell refuses occupied destination"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::move_conflicts(
            &rust_manifest,
            &rust_backup,
            &twins.rust_home,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &live_matches(twins.rust_home.clone()),
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses occupied destination"
    );
    // The manifest still stages first, on both engines.
    assert_eq!(
        stored_manifest(&shell_backup),
        Some(std::fs::read(&shell_manifest).expect("input")),
        "shell stages manifest before refusing"
    );
    assert_eq!(
        stored_manifest(&rust_backup),
        Some(std::fs::read(&rust_manifest).expect("input")),
        "rust stages manifest before refusing"
    );

    // Unprovisionable backup root refuses before touching anything.
    let twins = Twins::build("plan-move-noprivate");
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflict_tree(home);
    }
    let shell_manifest = twins.root().join("sh-manifest");
    let rust_manifest = twins.root().join("rs-manifest");
    std::fs::write(
        &shell_manifest,
        manifest_for(&twins.shell_home, &["file1"], &[]),
    )
    .expect("manifest");
    std::fs::write(
        &rust_manifest,
        manifest_for(&twins.rust_home, &["file1"], &[]),
    )
    .expect("manifest");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    assert_eq!(
        shell_move(
            &twins.shell_home,
            &shell_manifest,
            &shell_backup,
            "_dot_init_private_directory() { return 1; }\n",
        ),
        1,
        "shell refuses unprovisionable root"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::move_conflicts(
            &rust_manifest,
            &rust_backup,
            &twins.rust_home,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &live_matches(twins.rust_home.clone()),
            &failing_private_dir(),
            &mut cache,
        )
        .is_err(),
        "rust refuses unprovisionable root"
    );
    assert!(!exists_side(&shell_backup));
    assert!(!exists_side(&rust_backup));
}

/// Lexical existence for assertions.
fn exists_side(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// One recorded move row: manifest text, stub answers (`1` accept,
/// `0` refuse, consumed left to right), expected verdict, and the
/// expected normalized matcher calls.
struct RecordRow {
    name: &'static str,
    manifest: &'static str,
    answers: &'static str,
    ok: bool,
    calls: &'static [&'static str],
}

#[test]
fn move_conflicts_record_rows() {
    let rows = [
        RecordRow {
            name: "plain-accept",
            manifest: "p\tk\td\ti\tm\ts\tv\n",
            answers: "1",
            ok: true,
            calls: &["HOME/backup/p|k|d|i|m|s|v"],
        },
        RecordRow {
            name: "plain-refuse",
            manifest: "p\tk\td\ti\tm\ts\tv\n",
            answers: "00",
            ok: false,
            calls: &["HOME/backup/p|k|d|i|m|s|v", "HOME/p|k|d|i|m|s|v"],
        },
        RecordRow {
            name: "leading-tab-strips",
            manifest: "\tp\tk\td\ti\tm\ts\tv\n",
            answers: "1",
            ok: true,
            calls: &["HOME/backup/p|k|d|i|m|s|v"],
        },
        RecordRow {
            name: "doubled-tab-collapses",
            manifest: "p\t\tk\td\ti\tm\ts\tv\n",
            answers: "1",
            ok: true,
            calls: &["HOME/backup/p|k|d|i|m|s|v"],
        },
        RecordRow {
            name: "extra-fields-fold-right",
            manifest: "a\tb\tc\td\te\tf\tg\te1\n",
            answers: "1",
            ok: true,
            calls: &["HOME/backup/a|b|c|d|e|f|g\te1"],
        },
        RecordRow {
            name: "blank-line-skips",
            manifest: "\nq\tk2\td\ti\tm\ts\tv\n",
            answers: "1",
            ok: true,
            calls: &["HOME/backup/q|k2|d|i|m|s|v"],
        },
        RecordRow {
            name: "empty-fields-skip",
            manifest: "\t\n",
            answers: "",
            ok: true,
            calls: &[],
        },
        RecordRow {
            name: "unterminated-tail-skips",
            manifest: "p\tk\td\ti\tm\ts\tv",
            answers: "",
            ok: true,
            calls: &[],
        },
        RecordRow {
            name: "nul-bytes-strip",
            manifest: "n\0p\tk\td\ti\tm\ts\tv\n",
            answers: "1",
            ok: true,
            calls: &["HOME/backup/np|k|d|i|m|s|v"],
        },
    ];
    for row in rows {
        let twins = Twins::build("plan-move-record");
        let manifest = twins.root().join("manifest");
        std::fs::write(&manifest, row.manifest).expect("manifest");
        let shell_backup = twins.shell_home.join("backup");
        let rust_backup = twins.rust_home.join("backup");
        let shell_body = format!(
            "{RECORD_STUB}STUB_ANSWERS={}\nif _dot_init_move_conflicts {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
            row.answers,
            sq(manifest.to_str().expect("manifest path")),
            sq(shell_backup.to_str().expect("backup path")),
        );
        let (shell_code, shell_out, _) = shell_run(&twins.shell_home, &[], &shell_body);
        let shell_calls: Vec<String> = String::from_utf8_lossy(&shell_out)
            .lines()
            .filter_map(|line| line.strip_prefix("call="))
            .map(|call| normalize_call(call, &twins.shell_home))
            .collect();
        let recorder = Recorder::new(row.answers.chars().map(|answer| answer == '1').collect());
        let mut cache = MoveCache::default();
        let rust = plan::move_conflicts(
            &manifest,
            &rust_backup,
            &twins.rust_home,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &recorder.matcher(),
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        );
        let expected: Vec<String> = row.calls.iter().map(|call| call.to_string()).collect();
        let rust_calls: Vec<String> = recorder
            .take()
            .iter()
            .map(|call| normalize_call(call, &twins.rust_home))
            .collect();
        assert_eq!(shell_code == 0, rust.is_ok(), "{} verdict", row.name);
        assert_eq!(shell_code == 0, row.ok, "{} shell oracle", row.name);
        assert_eq!(rust_calls, expected, "{} rust calls", row.name);
        assert_eq!(shell_calls, expected, "{} shell calls", row.name);
    }
}

/// Run the shell restore over one side; returns the verdict.
fn shell_restore(home: &Path, backup: &Path, prelude: &str) -> i32 {
    let body = format!(
        "{prelude}if _dot_init_restore_backups {}; then code=0; else code=$?; fi\nprintf 'code=%s\n' \"$code\"\n",
        sq(backup.to_str().expect("backup path")),
    );
    shell_run(home, &[], &body).0
}

/// Seed one stash tree under `backup`: the mirror of
/// [`seed_conflict_tree`], snapshotted for restore manifests.
fn seed_stash(backup: &Path) {
    write(backup, "file1", b"hello\n");
    write(backup, "sub/file2", b"nested\n");
    write(backup, "dir1/inner", b"inner\n");
    std::os::unix::fs::symlink("file1", backup.join("link1")).expect("stash link");
}

#[test]
fn restore_backups_restores_live_stash() {
    let twins = Twins::build("plan-restore-live");
    let rels = ["file1", "sub/file2", "dir1", "link1"];
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    seed_stash(&shell_backup);
    seed_stash(&rust_backup);
    // Manifests snapshot the stash sides, then land as the stored
    // manifest the gate requires.
    for backup in [&shell_backup, &rust_backup] {
        let mut text = String::new();
        for rel in rels {
            text.push_str(&snapshot_row(backup, rel));
            text.push('\n');
        }
        std::fs::write(backup.join("manifest"), &text).expect("stored manifest");
    }
    let mut originals = Vec::new();
    for rel in rels {
        originals.push((rel, shape(&shell_backup.join(rel))));
    }

    assert_eq!(
        shell_restore(&twins.shell_home, &shell_backup, ""),
        0,
        "shell restores"
    );
    let mut cache = MoveCache::default();
    plan::restore_backups(
        &rust_backup,
        &twins.rust_home,
        &live_matches(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust restores");

    for (rel, (_, bytes)) in &originals {
        assert_eq!(shape(&shell_backup.join(rel)), (false, Vec::new()));
        assert_eq!(shape(&rust_backup.join(rel)), (false, Vec::new()));
        assert_eq!(shape(&twins.shell_home.join(rel)), (true, bytes.clone()));
        assert_eq!(shape(&twins.rust_home.join(rel)), (true, bytes.clone()));
    }
    for home in [&twins.shell_home, &twins.rust_home] {
        assert_eq!(
            shape(&home.join("dir1/inner")),
            (true, b"inner\n".to_vec()),
            "directory content follows"
        );
    }
}

#[test]
fn restore_backups_gates_and_refusals() {
    // Missing backup root is a successful no-op.
    let dir = TempDir::new("plan-restore-missing").expect("temp dir");
    let missing = dir.path().join("no-backup");
    let body = format!(
        "if _dot_init_restore_backups {}; then code=0; else code=$?; fi\nprintf 'code=%s\n' \"$code\"\n",
        sq(missing.to_str().expect("backup path")),
    );
    assert_eq!(shell_run(dir.path(), &[], &body).0, 0, "shell no-op");
    let mut cache = MoveCache::default();
    plan::restore_backups(
        &missing,
        dir.path(),
        &live_matches(dir.path().to_path_buf()),
        &mut cache,
    )
    .expect("rust no-op");

    // A backup directory without a manifest is a no-op too.
    let dir = TempDir::new("plan-restore-nomanifest").expect("temp dir");
    let backup = dir.path().join("backup");
    std::fs::create_dir_all(&backup).expect("backup dir");
    let body = format!(
        "if _dot_init_restore_backups {}; then code=0; else code=$?; fi\nprintf 'code=%s\n' \"$code\"\n",
        sq(backup.to_str().expect("backup path")),
    );
    assert_eq!(shell_run(dir.path(), &[], &body).0, 0, "shell no-op");
    let mut cache = MoveCache::default();
    plan::restore_backups(
        &backup,
        dir.path(),
        &live_matches(dir.path().to_path_buf()),
        &mut cache,
    )
    .expect("rust no-op");

    // Absent stashes skip without calling the matcher.
    let twins = Twins::build("plan-restore-absent");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    for backup in [&shell_backup, &rust_backup] {
        std::fs::create_dir_all(backup).expect("backup dir");
        std::fs::write(
            backup.join("manifest"),
            b"ghost\tregular\t1\t2\t644\t3\tabc\n",
        )
        .expect("manifest");
    }
    assert_eq!(
        shell_restore(&twins.shell_home, &shell_backup, ""),
        0,
        "shell skips absent stash"
    );
    let mut cache = MoveCache::default();
    plan::restore_backups(
        &rust_backup,
        &twins.rust_home,
        &live_matches(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust skips absent stash");

    // Changed stash content refuses; the stash stays put.
    let twins = Twins::build("plan-restore-mismatch");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    seed_stash(&shell_backup);
    seed_stash(&rust_backup);
    for backup in [&shell_backup, &rust_backup] {
        let row = snapshot_row(backup, "file1");
        std::fs::write(backup.join("manifest"), format!("{row}\n")).expect("manifest");
    }
    write(&shell_backup, "file1", b"tampered\n");
    write(&rust_backup, "file1", b"tampered\n");
    assert_eq!(
        shell_restore(&twins.shell_home, &shell_backup, ""),
        1,
        "shell refuses mismatch"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::restore_backups(
            &rust_backup,
            &twins.rust_home,
            &live_matches(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses mismatch"
    );
    assert!(exists_side(&shell_backup.join("file1")));
    assert!(exists_side(&rust_backup.join("file1")));

    // An occupied home path refuses before moving.
    let twins = Twins::build("plan-restore-occupied");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    seed_stash(&shell_backup);
    seed_stash(&rust_backup);
    for (backup, home) in [
        (&shell_backup, &twins.shell_home),
        (&rust_backup, &twins.rust_home),
    ] {
        let row = snapshot_row(backup, "file1");
        std::fs::write(backup.join("manifest"), format!("{row}\n")).expect("manifest");
        write(home, "file1", b"squatter\n");
    }
    assert_eq!(
        shell_restore(&twins.shell_home, &shell_backup, ""),
        1,
        "shell refuses occupied home"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::restore_backups(
            &rust_backup,
            &twins.rust_home,
            &live_matches(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses occupied home"
    );
}

#[test]
fn restore_backups_sorts_descending_and_stops() {
    // Rows process newest-first; the unsafe row sorts last, so the
    // first row still moves before the run refuses. The call log
    // pins the order on both engines.
    let twins = Twins::build("plan-restore-order");
    let shell_backup = twins.shell_home.join("backup");
    let rust_backup = twins.rust_home.join("backup");
    for backup in [&shell_backup, &rust_backup] {
        std::fs::create_dir_all(backup.join("b")).expect("stash parent");
        write(backup, "b/keep", b"kept\n");
        let row = snapshot_row(backup, "b/keep");
        let manifest =
            format!("{row}\na/gone\tregular\t1\t2\t644\t1\tx\n../evil\tregular\t1\t2\t644\t1\tx");
        std::fs::write(backup.join("manifest"), manifest).expect("manifest");
    }
    let shell_body = format!(
        "{RECORD_STUB}STUB_ANSWERS=1\nif _dot_init_restore_backups {}; then code=0; else code=$?; fi\nprintf 'code=%s\n' \"$code\"\n",
        sq(shell_backup.to_str().expect("backup path")),
    );
    let (shell_code, shell_out, _) = shell_run(&twins.shell_home, &[], &shell_body);
    let shell_calls: Vec<String> = String::from_utf8_lossy(&shell_out)
        .lines()
        .filter_map(|line| line.strip_prefix("call="))
        .map(|call| normalize_call(call, &twins.shell_home))
        .collect();
    let recorder = Recorder::new(vec![true]);
    let mut cache = MoveCache::default();
    let rust = plan::restore_backups(
        &rust_backup,
        &twins.rust_home,
        &recorder.matcher(),
        &mut cache,
    );
    assert_eq!(shell_code, 1, "shell stops at unsafe row");
    assert!(rust.is_err(), "rust stops at unsafe row");
    // Descending byte order puts b/keep first; the absent a/gone
    // skips silently; the unsafe row refuses with no matcher call.
    assert_eq!(shell_calls.len(), 1, "one shell matcher call");
    let rust_calls: Vec<String> = recorder
        .take()
        .iter()
        .map(|call| normalize_call(call, &twins.rust_home))
        .collect();
    assert_eq!(rust_calls.len(), 1, "one rust matcher call");
    assert!(
        shell_calls[0].starts_with("HOME/backup/b/keep|"),
        "shell processes b/keep first"
    );
    assert!(
        rust_calls[0].starts_with("HOME/backup/b/keep|"),
        "rust processes b/keep first"
    );
    // The first row already moved home before the refusal.
    assert_eq!(
        shape(&twins.shell_home.join("b/keep")),
        (true, b"kept\n".to_vec())
    );
    assert_eq!(
        shape(&twins.rust_home.join("b/keep")),
        (true, b"kept\n".to_vec())
    );
}

/// Completed path for one side, via the live
/// `_dot_init_completed_file` (owned by the transaction lane).
fn completed_for(home: &Path) -> PathBuf {
    let body = "if _dot_init_completed_file; then code=0; else code=$?; fi\nprintf 'completed=%s\ncode=%s\n' \"$REPLY\" \"$code\"\n";
    let (code, out, _) = shell_run(home, &[], body);
    assert_eq!(code, 0, "completed path");
    let text = String::from_utf8_lossy(&out);
    let completed = text
        .lines()
        .find_map(|line| line.strip_prefix("completed="))
        .unwrap_or_default();
    assert!(!completed.is_empty(), "completed value");
    PathBuf::from(completed)
}

/// Run the shell publication over one side; returns the verdict.
/// The shell derives the destination from `HOME` itself.
fn shell_publish(home: &Path, record: &Path) -> i32 {
    let body = format!(
        "if _dot_init_publish_completed {}; then code=0; else code=$?; fi\nprintf 'code=%s\n' \"$code\"\n",
        sq(record.to_str().expect("record path")),
    );
    shell_run(home, &[], &body).0
}

/// Leftover sibling temporaries (`.completed.*`) in `dir`.
fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).expect("list dir");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with(".completed.") {
            found.push(entry.path());
        }
    }
    found.sort();
    found
}

#[test]
fn publish_completed_publishes_live() {
    // Fresh publication: parent provisioned, record stamped at 0600.
    let twins = Twins::build("plan-publish-fresh");
    let shell_record = twins.root().join("sh-record");
    let rust_record = twins.root().join("rs-record");
    std::fs::write(&shell_record, b"record-body\n").expect("shell record");
    std::fs::write(&rust_record, b"record-body\n").expect("rust record");
    let shell_completed = completed_for(&twins.shell_home);
    let rust_completed = completed_for(&twins.rust_home);
    assert_eq!(
        shell_publish(&twins.shell_home, &shell_record),
        0,
        "shell publishes"
    );
    let mut cache = MoveCache::default();
    plan::publish_completed(
        &rust_record,
        &rust_completed,
        &live_private_dir(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust publishes");
    for completed in [&shell_completed, &rust_completed] {
        assert_eq!(
            std::fs::read(completed).expect("completed bytes"),
            b"record-body\n",
            "completed content"
        );
        assert_eq!(mode_of(completed), 0o600, "completed mode");
        let root = completed.parent().expect("completed parent");
        assert_eq!(mode_of(root), 0o700, "completed root mode");
        assert!(leftovers(root).is_empty(), "no leftovers");
    }

    // Replacement: an owned regular completion is swapped in place.
    let twins = Twins::build("plan-publish-replace");
    let shell_record = twins.root().join("sh-record");
    let rust_record = twins.root().join("rs-record");
    std::fs::write(&shell_record, b"next-body\n").expect("shell record");
    std::fs::write(&rust_record, b"next-body\n").expect("rust record");
    let shell_completed = completed_for(&twins.shell_home);
    let rust_completed = completed_for(&twins.rust_home);
    for completed in [&shell_completed.clone(), &rust_completed.clone()] {
        let root = completed.parent().expect("root").to_path_buf();
        std::fs::create_dir_all(&root).expect("root");
        chmod(&root, 0o700);
        std::fs::write(completed, b"stale-body\n").expect("stale completed");
        chmod(completed, 0o600);
    }
    assert_eq!(
        shell_publish(&twins.shell_home, &shell_record),
        0,
        "shell replaces"
    );
    let mut cache = MoveCache::default();
    plan::publish_completed(
        &rust_record,
        &rust_completed,
        &live_private_dir(twins.rust_home.clone()),
        &mut cache,
    )
    .expect("rust replaces");
    for completed in [&shell_completed, &rust_completed] {
        assert_eq!(
            std::fs::read(completed).expect("completed bytes"),
            b"next-body\n",
            "replaced content"
        );
        assert_eq!(mode_of(completed), 0o600, "replaced mode");
        let root = completed.parent().expect("completed parent");
        assert!(leftovers(root).is_empty(), "no leftovers");
    }
}

#[test]
fn publish_completed_refuses() {
    // A directory at the destination refuses and leaves the sibling.
    let twins = Twins::build("plan-publish-isdir");
    let shell_record = twins.root().join("sh-record");
    let rust_record = twins.root().join("rs-record");
    std::fs::write(&shell_record, b"record-body\n").expect("shell record");
    std::fs::write(&rust_record, b"record-body\n").expect("rust record");
    let shell_completed = completed_for(&twins.shell_home);
    let rust_completed = completed_for(&twins.rust_home);
    std::fs::create_dir_all(&shell_completed).expect("shell completed dir");
    std::fs::create_dir_all(&rust_completed).expect("rust completed dir");
    assert_eq!(
        shell_publish(&twins.shell_home, &shell_record),
        1,
        "shell refuses directory"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::publish_completed(
            &rust_record,
            &rust_completed,
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses directory"
    );
    for completed in [&shell_completed, &rust_completed] {
        assert!(completed.is_dir(), "destination untouched");
        let root = completed.parent().expect("completed parent");
        assert_eq!(leftovers(root).len(), 1, "one leftover sibling");
    }

    // A symlink at the destination refuses the same way.
    let twins = Twins::build("plan-publish-islink");
    let shell_record = twins.root().join("sh-record");
    let rust_record = twins.root().join("rs-record");
    std::fs::write(&shell_record, b"record-body\n").expect("shell record");
    std::fs::write(&rust_record, b"record-body\n").expect("rust record");
    let shell_completed = completed_for(&twins.shell_home);
    let rust_completed = completed_for(&twins.rust_home);
    for (completed, home) in [
        (&shell_completed, &twins.shell_home),
        (&rust_completed, &twins.rust_home),
    ] {
        let root = completed.parent().expect("root").to_path_buf();
        std::fs::create_dir_all(&root).expect("root");
        write(home, "decoy", b"decoy\n");
        std::os::unix::fs::symlink(home.join("decoy"), completed).expect("completed link");
    }
    assert_eq!(
        shell_publish(&twins.shell_home, &shell_record),
        1,
        "shell refuses symlink"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::publish_completed(
            &rust_record,
            &rust_completed,
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses symlink"
    );
    for (completed, home) in [
        (&shell_completed, &twins.shell_home),
        (&rust_completed, &twins.rust_home),
    ] {
        let root = completed.parent().expect("completed parent");
        assert_eq!(leftovers(root).len(), 1, "one leftover sibling");
        assert_eq!(
            std::fs::read_link(completed).expect("link intact"),
            home.join("decoy").as_path(),
            "link untouched"
        );
    }

    // A missing record refuses after staging the sibling.
    let twins = Twins::build("plan-publish-norecord");
    let shell_record = twins.root().join("sh-record");
    let rust_record = twins.root().join("rs-record");
    let shell_completed = completed_for(&twins.shell_home);
    let rust_completed = completed_for(&twins.rust_home);
    assert_eq!(
        shell_publish(&twins.shell_home, &shell_record),
        1,
        "shell refuses missing record"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::publish_completed(
            &rust_record,
            &rust_completed,
            &live_private_dir(twins.rust_home.clone()),
            &mut cache,
        )
        .is_err(),
        "rust refuses missing record"
    );
    for completed in [&shell_completed, &rust_completed] {
        assert!(!exists_side(completed), "no destination");
        let root = completed.parent().expect("completed parent");
        let found = leftovers(root);
        assert_eq!(found.len(), 1, "one leftover sibling");
        assert_eq!(
            std::fs::read(&found[0]).expect("leftover bytes").len(),
            0,
            "leftover never filled"
        );
    }

    // An unprovisionable root refuses before staging anything.
    let twins = Twins::build("plan-publish-noprivate");
    let shell_record = twins.root().join("sh-record");
    let rust_record = twins.root().join("rs-record");
    std::fs::write(&shell_record, b"record-body\n").expect("shell record");
    std::fs::write(&rust_record, b"record-body\n").expect("rust record");
    let shell_completed = completed_for(&twins.shell_home);
    let rust_completed = completed_for(&twins.rust_home);
    let shell_body = format!(
        "_dot_init_private_directory() {{ return 1; }}\nif _dot_init_publish_completed {}; then code=0; else code=$?; fi\nprintf 'code=%s\n' \"$code\"\n",
        sq(shell_record.to_str().expect("record path")),
    );
    assert_eq!(
        shell_run(&twins.shell_home, &[], &shell_body).0,
        1,
        "shell refuses unprovisionable root"
    );
    let mut cache = MoveCache::default();
    assert!(
        plan::publish_completed(
            &rust_record,
            &rust_completed,
            &failing_private_dir(),
            &mut cache,
        )
        .is_err(),
        "rust refuses unprovisionable root"
    );
    assert!(!exists_side(&shell_completed));
    assert!(!exists_side(&rust_completed));
}
