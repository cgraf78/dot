//! Differential parity tests for the git-stage family
//! (`lib/dot/init-client.sh`) against the live shell: the staged
//! clone ([`stage::stage_git`]) and its publication into the live
//! git directory ([`stage::publish_git`]).
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home directories sharing one
//! origin fixture, so stages, live directories, records, and
//! markers never collide. Cross-lane collaborators
//! (`_dot_init_private_directory`, `_dot_init_generation_matches`,
//! `_dot_init_configure_git_metadata_modes`,
//! `_dot_init_set_git_identity`,
//! `_dot_init_write_generation_marker`, `_dot_move_noreplace`,
//! `_dot_init_record_phase`) cross the port as closures; `live`
//! rows feed closures that run the real shell helpers, while
//! refusal rows swap individual slots for stubs and override the
//! same helper on the shell side, so both verdicts and end states
//! stay comparable. The shell's ambient identity state
//! (`DOT_INIT_GIT_DEV`/`DOT_INIT_GIT_INO`) lives in a shared cell
//! the identity closure fills and the record closure exports, the
//! way the shell threads it through globals; both sides start at
//! `-`/`-`, exactly like the shell's `write_record` defaults. The
//! adversarial marker row (newline and NUL bytes) only runs where
//! the oracle grep reports GNU, because the multi-pattern split it
//! pins is GNU-specific (the NUL-manifest gating precedent).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

use dot::init_client_git as stage;
use dot::test_support::TempDir;

/// Sources for the git-stage chapter: the resource runtime, the
/// shared temp helpers (sibling temps, identity, moves), the XDG
/// root, and the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the stage runtime sourced and report
/// the verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that
/// verdict — not the process status, which only says the printer
/// ran. A snippet that never reports (a harness bug, never a pass)
/// yields 99.
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Whether the oracle grep splits `-F` patterns the GNU way (the
/// adversarial marker row pins that split, so it must skip where
/// the oracle cannot do it).
fn grep_is_gnu() -> bool {
    Command::new("grep")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).starts_with("grep (GNU"))
}

/// Twin homes: disjoint directories so stages, live checkouts,
/// records, and markers never collide across engines.
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

/// Run git for fixtures; asserts success, silences output. Fixtures
/// run with `HOME` steered at the fixture directory so the
/// operator's own gitconfig (signing, templates) cannot leak in;
/// identity and signing come from `-c` flags instead.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("LC_ALL", "C")
        .env("HOME", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

/// Seed the shared origin fixture: one commit on `branch`.
/// Returns the origin path and the committed oid.
fn seed_origin(root: &Path, branch: &str) -> (PathBuf, String) {
    let origin = root.join("origin");
    git(root, &["init", "origin"]);
    std::fs::write(origin.join("file.txt"), b"seed\n").expect("seed file");
    git(&origin, &["add", "file.txt"]);
    git(&origin, &["commit", "-m", "seed"]);
    git(&origin, &["branch", "-M", branch]);
    let output = Command::new("git")
        .current_dir(&origin)
        .arg("rev-parse")
        .arg("HEAD")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("rev-parse HEAD");
    assert!(output.status.success(), "rev-parse HEAD");
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!commit.is_empty(), "origin commit");
    (origin, commit)
}

/// One side's run identity: side paths as owned strings plus the
/// run values shared across sides. Everything the shell reads from
/// `DOT_INIT_*`, `HOME`, and `DOT_BIN`, so probes build their
/// environment from exactly this.
#[derive(Clone)]
struct Engine {
    home: String,
    record: String,
    backup: String,
    git_dir: String,
    origin: String,
    branch: String,
    commit: String,
    identity: String,
    nonce: String,
    dot_bin: String,
}

impl Engine {
    fn new(
        home: &Path,
        record: &Path,
        origin: &Path,
        branch: &str,
        commit: &str,
        identity: &str,
        nonce: &str,
    ) -> Self {
        Self {
            home: home.to_str().expect("home text").to_string(),
            record: record.to_str().expect("record text").to_string(),
            backup: home
                .join(".dot-backup")
                .to_str()
                .expect("backup text")
                .to_string(),
            git_dir: home
                .join(".dotfiles")
                .to_str()
                .expect("git dir text")
                .to_string(),
            origin: origin.to_str().expect("origin text").to_string(),
            branch: branch.to_string(),
            commit: commit.to_string(),
            identity: identity.to_string(),
            nonce: nonce.to_string(),
            dot_bin: "dot".to_string(),
        }
    }

    /// Probe environment for this side: the row's `DOT_INIT_*`
    /// plus `DOT_BIN`, mirroring what the engine exports.
    fn env(&self) -> Vec<(&str, &str)> {
        vec![
            ("DOT_INIT_BACKUP", self.backup.as_str()),
            ("DOT_INIT_GIT_DIR", self.git_dir.as_str()),
            ("DOT_INIT_ORIGIN", self.origin.as_str()),
            ("DOT_INIT_BRANCH", self.branch.as_str()),
            ("DOT_INIT_COMMIT", self.commit.as_str()),
            ("DOT_INIT_IDENTITY", self.identity.as_str()),
            ("DOT_INIT_NONCE", self.nonce.as_str()),
            ("DOT_BIN", self.dot_bin.as_str()),
        ]
    }

    fn home(&self) -> &Path {
        Path::new(&self.home)
    }

    fn inputs(&self) -> stage::GitStageInputs<'_> {
        stage::GitStageInputs {
            record: Path::new(&self.record),
            backup: Path::new(&self.backup),
            git_dir: Path::new(&self.git_dir),
            origin: Path::new(&self.origin),
            branch: self.branch.as_str(),
            commit: self.commit.as_str(),
            identity: self.identity.as_str(),
            nonce: self.nonce.as_str(),
            home: Path::new(&self.home),
        }
    }
}

/// Cross-lane collaborators for the Rust side: every slot runs the
/// live shell helper unless a refusal row overrides it. The ambient
/// identity state (`DOT_INIT_GIT_DEV`/`DOT_INIT_GIT_INO`) lives in
/// `state`, filled by the identity closure and exported by the
/// record closure — the integrator threads the same state through
/// the same closures, so this harness is the wiring preview.
/// Owned live collaborators: boxed because the harness owns one
/// slot per cross-lane helper and refusal rows swap slots
/// individually. The aliases keep `type_complexity` quiet the way
/// the port's own closure aliases do.
type LiveProvision = Box<dyn Fn(&Path) -> dot::Result<()>>;
/// Generation check, owned (answers false for every refusal).
type LiveGeneration = Box<dyn Fn(&Path) -> bool>;
/// Exclusive move, owned.
type LiveMove = Box<dyn Fn(&Path, &Path) -> dot::Result<()>>;
/// Phase journal, owned.
type LiveRecord = Box<dyn Fn(&Path, &str) -> dot::Result<()>>;

struct Harness {
    ensure_private_dir: LiveProvision,
    generation_matches: LiveGeneration,
    configure_metadata_modes: LiveProvision,
    set_git_identity: LiveProvision,
    write_generation_marker: LiveProvision,
    move_noreplace: LiveMove,
    record_phase: LiveRecord,
}

/// Refusal diagnostic for stubbed collaborators.
fn refused(slot: &'static str) -> dot::errors::Error {
    dot::errors::Error::Usage { message: slot }
}

impl Harness {
    /// Every slot runs the live shell helper against the Rust
    /// side's home, so `live` rows exercise true end-to-end parity.
    fn live(rust: Engine) -> Self {
        let state = Rc::new(RefCell::new(("-".to_string(), "-".to_string())));
        let home = PathBuf::from(&rust.home);
        let private_home = home.clone();
        let matches_eng = rust.clone();
        let modes_eng = rust.clone();
        let identity_eng = rust.clone();
        let identity_state = state.clone();
        let marker_eng = rust.clone();
        let move_home = home.clone();
        let record_eng = rust.clone();
        let record_state = state.clone();
        Self {
            ensure_private_dir: Box::new(move |path| {
                let body = format!(
                    "if _dot_init_private_directory {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
                    sq(path.to_str().expect("private path"))
                );
                if shell_run(&private_home, &[], &body).0 == 0 {
                    Ok(())
                } else {
                    Err(refused("private directory refused"))
                }
            }),
            generation_matches: Box::new(move |path| {
                let body = format!(
                    "if _dot_init_generation_matches {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
                    sq(path.to_str().expect("generation path"))
                );
                shell_run(matches_eng.home(), &matches_eng.env(), &body).0 == 0
            }),
            configure_metadata_modes: Box::new(move |path| {
                let body = format!(
                    "if _dot_init_configure_git_metadata_modes {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
                    sq(path.to_str().expect("modes path"))
                );
                if shell_run(modes_eng.home(), &modes_eng.env(), &body).0 == 0 {
                    Ok(())
                } else {
                    Err(refused("metadata modes refused"))
                }
            }),
            set_git_identity: Box::new(move |path| {
                let body = format!(
                    "if _dot_init_set_git_identity {}; then printf 'dev=%s\\nino=%s\\ncode=0\\n' \"$DOT_INIT_GIT_DEV\" \"$DOT_INIT_GIT_INO\"; else printf 'code=%s\\n' \"$?\"; fi\n",
                    sq(path.to_str().expect("identity path"))
                );
                let (code, out, _) = shell_run(identity_eng.home(), &identity_eng.env(), &body);
                if code != 0 {
                    return Err(refused("git identity refused"));
                }
                let text = String::from_utf8_lossy(&out);
                let dev = text
                    .lines()
                    .find_map(|line| line.strip_prefix("dev="))
                    .expect("identity dev")
                    .to_string();
                let ino = text
                    .lines()
                    .find_map(|line| line.strip_prefix("ino="))
                    .expect("identity ino")
                    .to_string();
                identity_state.replace((dev, ino));
                Ok(())
            }),
            write_generation_marker: Box::new(move |path| {
                let body = format!(
                    "if _dot_init_write_generation_marker {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
                    sq(path.to_str().expect("marker path"))
                );
                if shell_run(marker_eng.home(), &marker_eng.env(), &body).0 == 0 {
                    Ok(())
                } else {
                    Err(refused("generation marker refused"))
                }
            }),
            move_noreplace: Box::new(move |source, target| {
                let body = format!(
                    "if _dot_move_noreplace {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
                    sq(source.to_str().expect("move source")),
                    sq(target.to_str().expect("move target"))
                );
                if shell_run(&move_home, &[], &body).0 == 0 {
                    Ok(())
                } else {
                    Err(refused("move refused"))
                }
            }),
            record_phase: Box::new(move |record, phase| {
                let (dev, ino) = record_state.borrow().clone();
                let mut env = record_eng.env();
                env.push(("DOT_INIT_GIT_DEV", dev.as_str()));
                env.push(("DOT_INIT_GIT_INO", ino.as_str()));
                let body = format!(
                    "if _dot_init_record_phase {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
                    sq(record.to_str().expect("record path")),
                    sq(phase)
                );
                if shell_run(record_eng.home(), &env, &body).0 == 0 {
                    Ok(())
                } else {
                    Err(refused("record phase refused"))
                }
            }),
        }
    }

    fn deps(&self) -> stage::GitStageDeps<'_> {
        stage::GitStageDeps {
            ensure_private_dir: &self.ensure_private_dir,
            generation_matches: &self.generation_matches,
            configure_metadata_modes: &self.configure_metadata_modes,
            set_git_identity: &self.set_git_identity,
            write_generation_marker: &self.write_generation_marker,
            move_noreplace: &self.move_noreplace,
            record_phase: &self.record_phase,
        }
    }
}

/// Run the shell stage over one side, optionally after overriding
/// helpers in `prelude` (refusal rows). Returns the verdict and the
/// probe stderr for failure diagnosis. A verdict outside 0/1 is a
/// harness bug, never a pass.
fn shell_stage(eng: &Engine, prelude: &str) -> (i32, Vec<u8>) {
    let body = format!(
        "{prelude}if _dot_init_stage_git {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&eng.record)
    );
    let (code, _, err) = shell_run(eng.home(), &eng.env(), &body);
    assert!(
        code == 0 || code == 1,
        "oracle stage verdict {code}: {}",
        String::from_utf8_lossy(&err)
    );
    (code, err)
}

/// Run the shell publication over one side, with the same prelude
/// convention as [`shell_stage`].
fn shell_publish(eng: &Engine, prelude: &str) -> (i32, Vec<u8>) {
    let body = format!(
        "{prelude}if _dot_init_publish_git {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(&eng.record)
    );
    let (code, _, err) = shell_run(eng.home(), &eng.env(), &body);
    assert!(
        code == 0 || code == 1,
        "oracle publish verdict {code}: {}",
        String::from_utf8_lossy(&err)
    );
    (code, err)
}

/// Poll the observable end-state verdict: both engines must agree
/// on success versus refusal, and a shell verdict is always 0/1.
fn check_verdict(test: &str, shell_code: i32, shell_err: &[u8], rust: &dot::Result<()>) {
    match rust {
        Ok(()) => assert_eq!(
            shell_code,
            0,
            "{test}: shell refused but rust accepted: {}",
            String::from_utf8_lossy(shell_err)
        ),
        Err(err) => assert_eq!(
            shell_code, 1,
            "{test}: rust refused ({err}) but shell accepted"
        ),
    }
}

/// Replace every occurrence of `needle` with `replacement` (byte
/// level; the needle is never empty at these call sites).
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(position) = rest
        .windows(needle.len())
        .position(|window| window == needle)
    {
        out.extend_from_slice(&rest[..position]);
        out.extend_from_slice(replacement);
        rest = &rest[position + needle.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// True when `needle` occurs in `haystack` (byte level).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Normalize record bytes for cross-side comparison: the twin homes
/// differ, so home-rooted paths collapse to `HOME/`, and the
/// device/inode lines collapse to `#`, because the two sides stat
/// distinct directories. Every other line (origin, identity,
/// branch, commit, nonce, dot, revision, phase) compares exactly.
fn normalize_record(bytes: &[u8], home: &str) -> Vec<u8> {
    let merged = replace_bytes(bytes, home.as_bytes(), b"HOME");
    let mut out = Vec::with_capacity(merged.len());
    for line in merged.split_inclusive(|byte| *byte == b'\n') {
        let masked = line
            .strip_prefix(b"git_dev=")
            .map(|_| b"git_dev=#\n".as_slice())
            .or_else(|| {
                line.strip_prefix(b"git_ino=")
                    .map(|_| b"git_ino=#\n".as_slice())
            })
            .unwrap_or(line);
        out.extend_from_slice(masked);
    }
    out
}

/// One side's transaction record, normalized for comparison.
/// `None` when the run wrote no record.
fn record_shape(eng: &Engine) -> Option<Vec<u8>> {
    std::fs::read(Path::new(&eng.record))
        .ok()
        .map(|bytes| normalize_record(&bytes, &eng.home))
}

/// One side's stage marker bytes, if the run left one.
fn marker_shape(eng: &Engine) -> Option<Vec<u8>> {
    let marker = Path::new(&eng.backup).join("git-stage/identity");
    std::fs::read(&marker).ok()
}

/// Branch tip of the checkout at `git_dir`, if resolvable.
fn tip_of(git_dir: &Path, branch: &str) -> Option<String> {
    let reference = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().expect("git dir text"),
            "rev-parse",
            reference.as_str(),
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("rev-parse tip");
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git config --list` of the checkout, home-normalized for
/// cross-side comparison.
fn config_shape(git_dir: &Path, home: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().expect("git dir text"),
            "config",
            "--list",
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("config list");
    if !output.status.success() {
        return None;
    }
    Some(replace_bytes(&output.stdout, home.as_bytes(), b"HOME"))
}

/// One row's twin engines plus the shared origin fixture.
struct Case {
    twins: Twins,
    commit: String,
    branch: String,
    identity: String,
    nonce: String,
    shell: Engine,
    rust: Engine,
}

impl Case {
    fn setup(tag: &str, identity: &str, nonce: &str) -> Self {
        let twins = Twins::build(tag);
        let branch = "main";
        let (origin, commit) = seed_origin(twins.root(), branch);
        let shell = Engine::new(
            &twins.shell_home,
            &twins.root().join("sh-record"),
            &origin,
            branch,
            &commit,
            identity,
            nonce,
        );
        let rust = Engine::new(
            &twins.rust_home,
            &twins.root().join("rs-record"),
            &origin,
            branch,
            &commit,
            identity,
            nonce,
        );
        Self {
            twins,
            commit,
            branch: branch.to_string(),
            identity: identity.to_string(),
            nonce: nonce.to_string(),
            shell,
            rust,
        }
    }

    fn staged_of(eng: &Engine) -> PathBuf {
        Path::new(&eng.backup).join("git-stage/repo")
    }

    fn live_of(eng: &Engine) -> PathBuf {
        PathBuf::from(&eng.git_dir)
    }

    fn marker_bytes(&self) -> Vec<u8> {
        format!(
            "cgraf78 dot Git stage v1\nnonce={}\ncommit={}\nidentity={}\n",
            self.nonce, self.commit, self.identity
        )
        .into_bytes()
    }
}

/// Move one path aside with the live shell helper (fixture setup,
/// identical operation on both sides).
fn shell_move(home: &Path, source: &Path, target: &Path) {
    let body = format!(
        "if _dot_move_noreplace {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(source.to_str().expect("move source")),
        sq(target.to_str().expect("move target"))
    );
    assert_eq!(shell_run(home, &[], &body).0, 0, "setup move");
}

/// Copy one directory with `cp -R` (fixture setup only, never the
/// oracle: publication itself must move, never copy).
fn shell_copy_dir(home: &Path, source: &Path, target: &Path) {
    let body = format!(
        "if cp -R -- {} {}; then code=0; else code=$?; fi\nprintf 'code=%s\\n' \"$code\"\n",
        sq(source.to_str().expect("copy source")),
        sq(target.to_str().expect("copy target"))
    );
    assert_eq!(shell_run(home, &[], &body).0, 0, "setup copy");
}

/// Delete the branch tip of the checkout at `git_dir` (fixture
/// setup: turns a matching generation foreign without touching
/// the marker the row's run values still satisfy).
fn delete_tip(git_dir: &Path, branch: &str) {
    let status = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().expect("git dir text"),
            "update-ref",
            "-d",
            format!("refs/heads/{branch}").as_str(),
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("delete tip");
    assert!(status.success(), "delete tip");
}

/// Assert the fresh-clone end state on one side: marker, record,
/// tip, config, generation marker, and explicit modes.
fn assert_staged(case: &Case, eng: &Engine, phase: &str) {
    let staged = Case::staged_of(eng);
    assert_eq!(
        marker_shape(eng),
        Some(case.marker_bytes()),
        "stage marker bytes"
    );
    let record = record_shape(eng).expect("stage record");
    let want = format!("phase={phase}{}", '\n');
    assert!(contains(&record, want.as_bytes()), "record phase {phase}");
    assert_eq!(
        tip_of(&staged, &case.branch),
        Some(case.commit.clone()),
        "staged tip"
    );
    let config = config_shape(&staged, &eng.home).expect("staged config");
    assert!(
        config
            .windows(18)
            .any(|window| window == b"core.worktree=HOME"),
        "worktree points home: {}",
        String::from_utf8_lossy(&config)
    );
    assert_eq!(mode_of(Path::new(&eng.backup)), 0o700, "backup mode");
    assert_eq!(
        mode_of(&Path::new(&eng.backup).join("git-stage")),
        0o700,
        "container mode"
    );
    assert_eq!(
        mode_of(&Path::new(&eng.backup).join("git-stage/identity")),
        0o600,
        "marker mode"
    );
    assert_eq!(mode_of(Path::new(&eng.record)), 0o600, "record mode");
    let marker = std::fs::read(staged.join("dot-init-generation-v1")).expect("generation marker");
    let expected = format!(
        "cgraf78 dot client generation v1\nnonce={}\ncommit={}\nidentity={}\n",
        case.nonce, case.commit, case.identity
    );
    assert_eq!(marker, expected.as_bytes(), "generation marker bytes");
}

#[test]
fn stage_git_clones_fresh_stage() {
    let case = Case::setup("git-stage-fresh", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("fresh clone", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust stages: {rust:?}");
    assert_staged(&case, &case.shell, "git-staged");
    assert_staged(&case, &case.rust, "git-staged");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "record bytes agree"
    );
    assert_eq!(
        config_shape(&Case::staged_of(&case.shell), &case.shell.home),
        config_shape(&Case::staged_of(&case.rust), &case.rust.home),
        "staged config agrees"
    );

    // Restage: the generation still matches, so the staged
    // checkout is adopted without cloning again.
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("restage reuse", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust restages: {rust:?}");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "restage record bytes agree"
    );
    assert_eq!(
        tip_of(&Case::staged_of(&case.shell), &case.branch),
        Some(case.commit.clone()),
        "shell tip intact"
    );
    assert_eq!(
        tip_of(&Case::staged_of(&case.rust), &case.branch),
        Some(case.commit.clone()),
        "rust tip intact"
    );
}

#[test]
fn stage_git_reuses_live_git_dir() {
    let case = Case::setup("git-stage-live", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("setup stage", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust stages: {rust:?}");

    // Publish the stage out of band on both sides, so the next
    // run meets an already-live git directory and never touches
    // the (now absent) stage.
    for (home, eng) in [
        (case.twins.shell_home.clone(), &case.shell),
        (case.twins.rust_home.clone(), &case.rust),
    ] {
        shell_move(&home, &Case::staged_of(eng), &Case::live_of(eng));
    }
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("live reuse", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust reuses live: {rust:?}");
    for eng in [&case.shell, &case.rust] {
        assert!(
            std::fs::symlink_metadata(Case::staged_of(eng)).is_err(),
            "stage never rebuilt"
        );
        assert_eq!(
            tip_of(&Case::live_of(eng), &case.branch),
            Some(case.commit.clone()),
            "live tip"
        );
    }
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "live-reuse record bytes agree"
    );
    assert_eq!(
        config_shape(&Case::live_of(&case.shell), &case.shell.home),
        config_shape(&Case::live_of(&case.rust), &case.rust.home),
        "live config agrees"
    );
}

#[test]
fn stage_git_reclones_stale_repo() {
    let case = Case::setup("git-stage-stale", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("setup stage", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust stages: {rust:?}");

    // A stage whose tip no longer resolves is foreign: the run
    // removes it and clones again under the same marker.
    for eng in [&case.shell, &case.rust] {
        delete_tip(&Case::staged_of(eng), &case.branch);
    }
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("stale replace", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust replaces stale: {rust:?}");
    assert_staged(&case, &case.shell, "git-staged");
    assert_staged(&case, &case.rust, "git-staged");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "replace record bytes agree"
    );

    // A vanished stage clones again without touching the marker.
    for eng in [&case.shell, &case.rust] {
        std::fs::remove_dir_all(Case::staged_of(eng)).expect("drop staged repo");
    }
    let harness = Harness::live(case.rust.clone());
    let (shell_code, shell_err) = shell_stage(&case.shell, "");
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    check_verdict("absent reclone", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust reclones: {rust:?}");
    assert_eq!(
        marker_shape(&case.shell),
        Some(case.marker_bytes()),
        "shell marker intact"
    );
    assert_eq!(
        marker_shape(&case.rust),
        Some(case.marker_bytes()),
        "rust marker intact"
    );
    assert_eq!(
        tip_of(&Case::staged_of(&case.rust), &case.branch),
        Some(case.commit.clone()),
        "rust tip restored"
    );
}

/// Shell-helper overrides for refusal rows: each replaces one
/// collaborator with a refusal, mirroring the stubbed slot on the
/// Rust side.
const FAIL_PRIVATE: &str = "_dot_init_private_directory() { return 1; }\n";
const FAIL_CONFIGURE: &str = "_dot_init_configure_git_metadata_modes() { return 1; }\n";
const FAIL_IDENTITY: &str = "_dot_init_set_git_identity() { return 1; }\n";
const FAIL_RECORD: &str = "_dot_init_record_phase() { return 1; }\n";
const FAIL_MARKER: &str = "_dot_init_write_generation_marker() { return 1; }\n";
const FAIL_MOVE: &str = "_dot_move_noreplace() { return 1; }\n";

/// Run one stage on both engines with a shell prelude; the caller
/// supplies a fresh harness per call, mirroring one probe process
/// per oracle call.
fn run_stage(case: &Case, harness: &Harness, prelude: &str) -> ((i32, Vec<u8>), dot::Result<()>) {
    let shell = shell_stage(&case.shell, prelude);
    let rust = stage::stage_git(&case.rust.inputs(), &harness.deps());
    (shell, rust)
}

/// Run one publication on both engines, same convention as
/// [`run_stage`].
fn run_publish(case: &Case, harness: &Harness, prelude: &str) -> ((i32, Vec<u8>), dot::Result<()>) {
    let shell = shell_publish(&case.shell, prelude);
    let rust = stage::publish_git(&case.rust.inputs(), &harness.deps());
    (shell, rust)
}

/// Stub one path-taking collaborator with a refusal.
fn stub_path(slot: &'static str) -> LiveProvision {
    Box::new(move |_| Err(refused(slot)))
}

/// Write `bytes` at the side-relative `rel` under `base`,
/// creating parents.
fn place(base: &Path, rel: &str, bytes: &[u8]) {
    let path = base.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Build one row's bad stage: `setup` crafts the adversarial state
/// identically on both sides, then both engines must refuse with
/// no record left behind.
fn refuse_case(tag: &str, setup: impl Fn(&Case)) -> Case {
    let case = Case::setup(tag, "test-identity", "test-nonce-68");
    setup(&case);
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict(tag, shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "{tag}: rust must refuse");
    assert_eq!(
        record_shape(&case.shell),
        None,
        "{tag}: shell record absent"
    );
    assert_eq!(record_shape(&case.rust), None, "{tag}: rust record absent");
    case
}

#[test]
fn stage_git_refuses_bad_stage() {
    // A marker bound to another run refuses at the identity gate.
    let wrong = b"cgraf78 dot Git stage v1\nnonce=test-nonce-68\ncommit=deadbeef\nidentity=WRONG\n";
    let case = refuse_case("git-stage-wrong-marker", |case| {
        for eng in [&case.shell, &case.rust] {
            let container = Path::new(&eng.backup).join("git-stage");
            std::fs::create_dir_all(&container).expect("container");
            chmod(&container, 0o700);
            let marker = container.join("identity");
            std::fs::write(&marker, wrong).expect("wrong marker");
            chmod(&marker, 0o600);
        }
    });
    for eng in [&case.shell, &case.rust] {
        assert_eq!(marker_shape(eng), Some(wrong.to_vec()), "marker untouched");
    }

    // A regular file where the container belongs refuses.
    refuse_case("git-stage-container-file", |case| {
        for eng in [&case.shell, &case.rust] {
            place(Path::new(&eng.backup), "git-stage", b"blocker\n");
        }
    });

    // A dangling symlink container exists lexically, so creation
    // is skipped and the real-directory gate refuses.
    refuse_case("git-stage-container-link", |case| {
        for eng in [&case.shell, &case.rust] {
            std::fs::create_dir_all(Path::new(&eng.backup)).expect("backup");
            std::os::unix::fs::symlink("nowhere", Path::new(&eng.backup).join("git-stage"))
                .expect("dangling container");
        }
    });

    // A symlinked marker fails the real-file gate.
    refuse_case("git-stage-marker-link", |case| {
        for eng in [&case.shell, &case.rust] {
            let home = Path::new(&eng.home);
            place(home, "decoy", b"decoy\n");
            let container = Path::new(&eng.backup).join("git-stage");
            std::fs::create_dir_all(&container).expect("container");
            std::os::unix::fs::symlink(home.join("decoy"), container.join("identity"))
                .expect("marker link");
        }
    });

    // An unprovisionable backup refuses before touching anything.
    let case = refuse_case("git-stage-backup-file", |case| {
        for eng in [&case.shell, &case.rust] {
            std::fs::write(Path::new(&eng.backup), b"blocker\n").expect("backup blocker");
        }
    });
    for eng in [&case.shell, &case.rust] {
        assert!(
            Path::new(&eng.backup)
                .join("git-stage")
                .symlink_metadata()
                .is_err(),
            "no container built"
        );
    }

    // An unprovisionable collaborator refuses the same way.
    let case = Case::setup("git-stage-no-private", "test-identity", "test-nonce-68");
    let mut harness = Harness::live(case.rust.clone());
    harness.ensure_private_dir = stub_path("private directory refused");
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, FAIL_PRIVATE);
    check_verdict("no private dir", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses unprovisionable backup");
    assert_eq!(record_shape(&case.shell), None, "shell record absent");
    assert_eq!(record_shape(&case.rust), None, "rust record absent");

    // A stale non-directory stage refuses after journaling
    // `git-staging`: the record pins the failure point.
    let case = Case::setup("git-stage-stale-file", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("stale setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");
    for eng in [&case.shell, &case.rust] {
        std::fs::remove_dir_all(Case::staged_of(eng)).expect("drop staged repo");
        std::fs::write(Case::staged_of(eng), b"stale file\n").expect("stale file");
    }
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("stale non-dir", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses stale file");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "staging record agrees"
    );
    let record = record_shape(&case.shell).expect("staging record");
    let want = format!("phase=git-staging{}", '\n');
    assert!(
        contains(&record, want.as_bytes()),
        "failure point journaled"
    );
    for eng in [&case.shell, &case.rust] {
        assert_eq!(
            std::fs::read(Case::staged_of(eng)).expect("stale file intact"),
            b"stale file\n",
            "stale file untouched"
        );
    }
}

#[test]
fn stage_git_records_staging_on_late_failure() {
    // A live checkout from a foreign generation refuses after the
    // `git-staging` journal entry, never reaching `git-staged`.
    let case = Case::setup("git-stage-foreign-live", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("foreign setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");
    for (home, eng) in [
        (case.twins.shell_home.clone(), &case.shell),
        (case.twins.rust_home.clone(), &case.rust),
    ] {
        shell_move(&home, &Case::staged_of(eng), &Case::live_of(eng));
        delete_tip(&Case::live_of(eng), &case.branch);
    }
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("foreign live", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses foreign live");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "staging record agrees"
    );

    // A failing modes walk refuses at the same point.
    let case = Case::setup("git-stage-no-modes", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("modes setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");
    for (home, eng) in [
        (case.twins.shell_home.clone(), &case.shell),
        (case.twins.rust_home.clone(), &case.rust),
    ] {
        shell_move(&home, &Case::staged_of(eng), &Case::live_of(eng));
    }
    let mut harness = Harness::live(case.rust.clone());
    harness.configure_metadata_modes = stub_path("metadata modes refused");
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, FAIL_CONFIGURE);
    check_verdict("modes failure", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses failed modes");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "modes-failure record agrees"
    );

    // A failing identity capture refuses one step later, with the
    // modes already applied on both sides.
    let mut harness = Harness::live(case.rust.clone());
    harness.set_git_identity = stub_path("git identity refused");
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, FAIL_IDENTITY);
    check_verdict("identity failure", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses failed identity");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "identity-failure record agrees"
    );

    // A failing journal refuses before cloning, leaving the fresh
    // marker but no record.
    let case = Case::setup("git-stage-no-record", "test-identity", "test-nonce-68");
    let mut harness = Harness::live(case.rust.clone());
    harness.record_phase = Box::new(|_, _| Err(refused("record phase refused")));
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, FAIL_RECORD);
    check_verdict("record failure", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses failed journal");
    assert_eq!(record_shape(&case.shell), None, "shell record absent");
    assert_eq!(record_shape(&case.rust), None, "rust record absent");
    assert_eq!(
        marker_shape(&case.shell),
        marker_shape(&case.rust),
        "marker left behind on both"
    );

    // A failing generation-marker write refuses after the clone,
    // leaving the unmarked checkout and the `git-staging` record.
    let case = Case::setup("git-stage-no-marker", "test-identity", "test-nonce-68");
    let mut harness = Harness::live(case.rust.clone());
    harness.write_generation_marker = stub_path("generation marker refused");
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, FAIL_MARKER);
    check_verdict("marker failure", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses failed marker");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "marker-failure record agrees"
    );
    for eng in [&case.shell, &case.rust] {
        let staged = Case::staged_of(eng);
        assert_eq!(
            tip_of(&staged, &case.branch),
            Some(case.commit.clone()),
            "clone left behind"
        );
        assert!(
            std::fs::symlink_metadata(staged.join("dot-init-generation-v1")).is_err(),
            "no generation marker"
        );
    }
}

#[test]
fn stage_git_marks_special_values() {
    // Spaces, equals signs, percent signs, and quotes round-trip
    // through the marker printf and the fixed-string gate.
    let case = Case::setup(
        "git-stage-special",
        "id with spaces = and % percent 'quote'",
        "n c=1%2'3",
    );
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("special values", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust stages: {rust:?}");
    assert_eq!(
        marker_shape(&case.shell),
        Some(case.marker_bytes()),
        "shell marker exact"
    );
    assert_eq!(
        marker_shape(&case.rust),
        Some(case.marker_bytes()),
        "rust marker exact"
    );
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "special record bytes agree"
    );
    assert_eq!(
        tip_of(&Case::staged_of(&case.rust), &case.branch),
        Some(case.commit.clone()),
        "rust tip"
    );
}

#[test]
fn stage_git_matches_adversarial_marker() {
    // The newline/NUL split below is GNU-grep behavior; elsewhere
    // this row passes vacuously rather than pinning the wrong
    // oracle (the NUL-manifest gating precedent).
    if !grep_is_gnu() {
        return;
    }
    // A nonce carrying a newline still stages: `grep -F` splits
    // the pattern, so the first piece carries the match.
    let case = Case::setup("git-stage-newline", "test-identity", "a\nb");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("newline nonce", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust stages newline nonce: {rust:?}");
    assert_eq!(
        marker_shape(&case.shell),
        marker_shape(&case.rust),
        "newline marker agrees"
    );
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "newline record agrees"
    );
    assert_eq!(
        tip_of(&Case::staged_of(&case.rust), &case.branch),
        Some(case.commit.clone()),
        "newline tip"
    );

    // A NUL inside the marker separates framed lines for the gate
    // without disturbing the values either engine reads.
    let case = Case::setup("git-stage-nul", "test-identity", "test-nonce-68");
    let mut marker = b"cgraf78 dot Git stage v1\nnonce=test-nonce-68\x00junk\n".to_vec();
    marker
        .extend_from_slice(format!("commit={}\nidentity=test-identity\n", case.commit).as_bytes());
    for eng in [&case.shell, &case.rust] {
        let container = Path::new(&eng.backup).join("git-stage");
        std::fs::create_dir_all(&container).expect("container");
        chmod(&container, 0o700);
        let path = container.join("identity");
        std::fs::write(&path, &marker).expect("nul marker");
        chmod(&path, 0o600);
    }
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("nul marker", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust stages past nul: {rust:?}");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "nul record agrees"
    );
    assert_eq!(
        tip_of(&Case::staged_of(&case.rust), &case.branch),
        Some(case.commit.clone()),
        "nul tip"
    );
}

/// Assert the publication end state on one side: the stage moved
/// away, the live checkout resolving at the locked commit with a
/// matching config, and the `publishing` record.
fn assert_published(case: &Case, eng: &Engine) {
    let live = Case::live_of(eng);
    assert!(
        std::fs::symlink_metadata(Case::staged_of(eng)).is_err(),
        "stage moved away"
    );
    assert_eq!(
        tip_of(&live, &case.branch),
        Some(case.commit.clone()),
        "live tip"
    );
    let record = record_shape(eng).expect("publish record");
    let want = format!("phase=publishing{}", '\n');
    assert!(
        contains(&record, want.as_bytes()),
        "record phase publishing"
    );
    let marker =
        std::fs::read(live.join("dot-init-generation-v1")).expect("live generation marker");
    let expected = format!(
        "cgraf78 dot client generation v1\nnonce={}\ncommit={}\nidentity={}\n",
        case.nonce, case.commit, case.identity
    );
    assert_eq!(marker, expected.as_bytes(), "live generation marker bytes");
}

#[test]
fn publish_git_moves_staged_live() {
    let case = Case::setup("git-publish-move", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("publish setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");

    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, "");
    check_verdict("publish move", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust publishes: {rust:?}");
    assert_published(&case, &case.shell);
    assert_published(&case, &case.rust);
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "publish record bytes agree"
    );
    assert_eq!(
        config_shape(&Case::live_of(&case.shell), &case.shell.home),
        config_shape(&Case::live_of(&case.rust), &case.rust.home),
        "live config agrees"
    );

    // A second publication meets the live checkout and revalidates
    // it without moving anything.
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, "");
    check_verdict("republish", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust republishes: {rust:?}");
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "republish record bytes agree"
    );
    assert_eq!(
        tip_of(&Case::live_of(&case.rust), &case.branch),
        Some(case.commit.clone()),
        "live tip intact"
    );
}

#[test]
fn publish_git_uses_existing_live() {
    let case = Case::setup("git-publish-live", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("live setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");

    // A live checkout alongside the stage (a copy keeps the
    // generation marker the clone path would not): publication
    // adopts the live side and leaves the stage untouched.
    for (home, eng) in [
        (case.twins.shell_home.clone(), &case.shell),
        (case.twins.rust_home.clone(), &case.rust),
    ] {
        shell_copy_dir(&home, &Case::staged_of(eng), &Case::live_of(eng));
    }
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, "");
    check_verdict("publish live", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "rust adopts live: {rust:?}");
    for eng in [&case.shell, &case.rust] {
        assert_eq!(
            tip_of(&Case::staged_of(eng), &case.branch),
            Some(case.commit.clone()),
            "stage untouched"
        );
        assert_eq!(
            tip_of(&Case::live_of(eng), &case.branch),
            Some(case.commit.clone()),
            "live tip"
        );
    }
    assert_eq!(
        record_shape(&case.shell),
        record_shape(&case.rust),
        "adopt-live record bytes agree"
    );
}

#[test]
fn publish_git_refuses() {
    // Nothing staged and no live checkout: the generation check
    // fails on the absent stage before anything moves.
    let case = Case::setup("git-publish-empty", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, "");
    check_verdict("empty publish", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses empty publish");
    assert_eq!(record_shape(&case.shell), None, "shell record absent");
    assert_eq!(record_shape(&case.rust), None, "rust record absent");
    for eng in [&case.shell, &case.rust] {
        assert!(
            std::fs::symlink_metadata(Case::live_of(eng)).is_err(),
            "no live checkout built"
        );
    }

    // A live checkout from a foreign generation refuses without
    // journaling: the record never advances.
    let case = Case::setup("git-publish-foreign", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("foreign setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");
    let staged_shell = record_shape(&case.shell).expect("setup record");
    let staged_rust = record_shape(&case.rust).expect("setup record");
    assert_eq!(staged_shell, staged_rust, "setup records agree");
    for (home, eng) in [
        (case.twins.shell_home.clone(), &case.shell),
        (case.twins.rust_home.clone(), &case.rust),
    ] {
        shell_move(&home, &Case::staged_of(eng), &Case::live_of(eng));
        delete_tip(&Case::live_of(eng), &case.branch);
    }
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, "");
    check_verdict("foreign live", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses foreign live");
    assert_eq!(
        record_shape(&case.shell),
        Some(staged_shell),
        "shell record untouched"
    );
    assert_eq!(
        record_shape(&case.rust),
        Some(staged_rust),
        "rust record untouched"
    );

    // A failing move refuses with the stage intact and nothing
    // journaled.
    let case = Case::setup("git-publish-no-move", "test-identity", "test-nonce-68");
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_stage(&case, &harness, "");
    check_verdict("move setup", shell_code, &shell_err, &rust);
    assert!(rust.is_ok(), "setup stages: {rust:?}");
    let staged_shell = record_shape(&case.shell).expect("setup record");
    let staged_rust = record_shape(&case.rust).expect("setup record");
    assert_eq!(staged_shell, staged_rust, "setup records agree");
    let mut harness = Harness::live(case.rust.clone());
    harness.move_noreplace = Box::new(|_, _| Err(refused("move refused")));
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, FAIL_MOVE);
    check_verdict("move failure", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses failed move");
    assert_eq!(
        record_shape(&case.shell),
        Some(staged_shell.clone()),
        "shell record untouched"
    );
    assert_eq!(
        record_shape(&case.rust),
        Some(staged_rust.clone()),
        "rust record untouched"
    );
    for eng in [&case.shell, &case.rust] {
        assert_eq!(
            tip_of(&Case::staged_of(eng), &case.branch),
            Some(case.commit.clone()),
            "stage intact"
        );
        assert!(
            std::fs::symlink_metadata(Case::live_of(eng)).is_err(),
            "no live checkout built"
        );
    }

    // A failing journal refuses after the move: the live checkout
    // stands, but no record advances on either side.
    let mut harness = Harness::live(case.rust.clone());
    harness.record_phase = Box::new(|_, _| Err(refused("record phase refused")));
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, FAIL_RECORD);
    check_verdict("journal failure", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses failed journal");
    assert_eq!(
        record_shape(&case.shell),
        Some(staged_shell),
        "shell record untouched"
    );
    assert_eq!(
        record_shape(&case.rust),
        Some(staged_rust),
        "rust record untouched"
    );
    for eng in [&case.shell, &case.rust] {
        assert_eq!(
            tip_of(&Case::live_of(eng), &case.branch),
            Some(case.commit.clone()),
            "live checkout stands"
        );
        assert!(
            std::fs::symlink_metadata(Case::staged_of(eng)).is_err(),
            "stage moved away"
        );
    }

    // A live path that is not a directory refuses the same way.
    let case = Case::setup("git-publish-live-file", "test-identity", "test-nonce-68");
    for eng in [&case.shell, &case.rust] {
        std::fs::write(Path::new(&eng.git_dir), b"blocker\n").expect("live blocker");
    }
    let harness = Harness::live(case.rust.clone());
    let ((shell_code, shell_err), rust) = run_publish(&case, &harness, "");
    check_verdict("live file", shell_code, &shell_err, &rust);
    assert!(rust.is_err(), "rust refuses live file");
    assert_eq!(record_shape(&case.shell), None, "shell record absent");
    assert_eq!(record_shape(&case.rust), None, "rust record absent");
}
