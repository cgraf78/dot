//! Differential parity tests for the init adopt/status chapter
//! (`lib/dot/init-client.sh` lines 1418-1501) against the live
//! shell: the legacy-client adoption, the init usage text, and the
//! init status report.
//!
//! Separate binary because each row drives real filesystem state:
//! the two engines work under disjoint home and state directories,
//! so git stores, journals, and transactions never collide. Every
//! cross-lane call on the Rust side runs the LIVE shell function in
//! the Rust twin home — only `_dot_init_forward_converge` (lane 65,
//! the whole update engine) is stubbed on both sides, with its
//! topology/git-dir arguments logged for comparison.

use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dot::init_client_adopt as adopt;
use dot::repos_base::Topology;
use dot::test_support::TempDir;

/// Sources for the adopt/status chapter: the resource runtime, the
/// shared temp helpers (path identity, exclusive moves), the XDG
/// root, the init client itself, and the repository model (whose
/// source-time client selection sets the exact
/// `DOT_BASE_TOPOLOGY` / `DOT_CLIENT_GIT_DIR` production sees).
/// `DOT_ORIGINAL_ARGV` is preset before the model loads so the
/// selection takes the `init` path, exactly like `bin/dot init`.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    "DOT_ORIGINAL_ARGV=(init)\n",
    ". \"$1/lib/dot/repos/model.sh\"\n",
);

/// Sources for the status chapter: the status report never consults
/// the repository model, and model.sh runs client selection (with
/// its own diagnostics) at source time. Scoping the oracle to the
/// chapter keeps the report streams byte-pure.
const STATUS_SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the chapter runtime sourced. The
/// snippet must end with `exit "$code"` carrying the verdict under
/// test, so standard output stays byte-pure for comparison — the
/// process status only says the interpreter ran. `home` may be
/// empty (the unresolvable-state rows), in which case `cwd` must
/// still exist.
fn shell_eval_with(
    home: &str,
    cwd: &Path,
    state: &Path,
    extra: &[(&str, &str)],
    sources: &str,
    snippet: &str,
) -> Output {
    // Unresolvable-state rows pass an empty home; the child still
    // needs an existing working directory. `/` is inert here: every
    // snippet path is absolute.
    let cwd = if cwd.as_os_str().is_empty() {
        Path::new("/")
    } else {
        cwd
    };
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{sources}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("XDG_STATE_HOME", state)
        .env("XDG_CONFIG_HOME", "")
        .env("DOT_SOURCE_ROOT", repo)
        .env("DOT_TEST", "1")
        .env("DOT_BIN", format!("{repo}/bin/dot"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn bash")
}

/// Run one shell snippet with the full chapter runtime (including
/// the repository model and its source-time client selection).
fn shell_eval(
    home: &str,
    cwd: &Path,
    state: &Path,
    extra: &[(&str, &str)],
    snippet: &str,
) -> Output {
    shell_eval_with(home, cwd, state, extra, SOURCES, snippet)
}

/// Run one shell snippet with the model-less chapter runtime (no
/// source-time client selection).
fn shell_eval_bare(
    home: &str,
    cwd: &Path,
    state: &Path,
    extra: &[(&str, &str)],
    snippet: &str,
) -> Output {
    shell_eval_with(home, cwd, state, extra, STATUS_SOURCES, snippet)
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Strip every trailing newline, exactly like command substitution.
fn chomp(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

/// Twin homes and states: disjoint directories so git stores,
/// journals, and transactions never collide across engines.
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    shell_state: PathBuf,
    rust_state: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        let shell_state = dir.path().join("sh-state");
        let rust_state = dir.path().join("rs-state");
        for path in [&shell_home, &rust_home, &shell_state, &rust_state] {
            std::fs::create_dir_all(path).expect("twin dir");
        }
        Self {
            _dir: dir,
            shell_home,
            rust_home,
            shell_state,
            rust_state,
        }
    }

    fn root(&self) -> &Path {
        self._dir.path()
    }
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

/// Run git for fixtures and capture chomped stdout.
fn git_out(args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// Write `bytes` to `dir/name`, creating parents.
fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// A fixture origin: a bare repository with one commit on `main`.
struct Origin {
    path: PathBuf,
    commit: String,
}

/// Build the origin under `root/origin.git` from `root/seed`.
/// Idempotent: an existing origin is reused, so shared origins (the
/// foreign-identity row plants both homes from one) build once.
fn make_origin(root: &Path) -> Origin {
    let seed = root.join("seed");
    let path = root.join("origin.git");
    if path.exists() {
        let commit = git_out(&["-C", path_str(&path), "rev-parse", "HEAD"]);
        assert_eq!(commit.len(), 40, "fixture commit is SHA-1");
        return Origin { path, commit };
    }
    git(&["init", "--quiet", seed.to_str().expect("seed path")]);
    write(&seed, ".testrc", b"hello\n");
    git(&["-C", seed.to_str().expect("seed path"), "add", ".testrc"]);
    git(&[
        "-C",
        seed.to_str().expect("seed path"),
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "--quiet",
        "-m",
        "seed",
    ]);
    git(&[
        "-C",
        seed.to_str().expect("seed path"),
        "branch",
        "-M",
        "main",
    ]);
    git(&[
        "clone",
        "--quiet",
        "--bare",
        seed.to_str().expect("seed path"),
        path.to_str().expect("origin path"),
    ]);
    git(&[
        "-C",
        path.to_str().expect("origin path"),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let commit = git_out(&[
        "-C",
        path.to_str().expect("origin path"),
        "rev-parse",
        "HEAD",
    ]);
    assert_eq!(commit.len(), 40, "fixture commit is SHA-1");
    Origin { path, commit }
}

/// Canonical identity of `file://<origin>` through the live shell
/// (home-independent: only the origin path feeds it).
fn live_identity(origin: &Origin) -> String {
    let url = format!("file://{}", origin.path.to_str().expect("origin path"));
    let output = shell_eval_bare(
        "/",
        Path::new("/"),
        Path::new("/"),
        &[],
        &format!(
            "_dot_init_repo_identity {}\ncode=$?\nexit \"$code\"\n",
            sq(&url)
        ),
    );
    assert_eq!(output.status.code(), Some(0), "identity oracle");
    String::from_utf8_lossy(&chomp(output.stdout)).into_owned()
}

/// Plant a legacy separate client: a bare clone at
/// `$HOME/.dotfiles` plus one worktree file.
fn plant_separate(home: &Path, origin: &Origin) {
    git(&[
        "clone",
        "--quiet",
        "--bare",
        origin.path.to_str().expect("origin path"),
        home.join(".dotfiles").to_str().expect("git dir"),
    ]);
    write(home, ".testrc", b"hello\n");
}

/// Plant an ordinary client: `$HOME/.git` with `main` checked out
/// from the origin.
fn plant_ordinary(home: &Path, origin: &Origin) {
    let home_text = home.to_str().expect("home path");
    let url = format!("file://{}", origin.path.to_str().expect("origin path"));
    git(&[
        "-C",
        home_text,
        "init",
        "--quiet",
        "--initial-branch",
        "main",
    ]);
    git(&["-C", home_text, "remote", "add", "origin", &url]);
    git(&["-C", home_text, "fetch", "--quiet", "origin", "main"]);
    git(&["-C", home_text, "checkout", "--quiet", "main"]);
}

/// Fixture path as text (fixtures live under ASCII temp roots).
fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

/// Shell topology word for a detected topology.
fn topology_word(topology: Topology) -> &'static str {
    match topology {
        Topology::Separate => "separate",
        Topology::Ordinary => "ordinary",
        Topology::Missing => "missing",
    }
}

/// Parse the source-time selection the shell oracle observed.
fn parse_topology(word: &str) -> Topology {
    match word {
        "separate" => Topology::Separate,
        "ordinary" => Topology::Ordinary,
        _ => Topology::Missing,
    }
}

/// Live lane-65 single-origin reader in the Rust twin home.
fn live_single_origin<'x>(
    home: &'x Path,
    state: &'x Path,
    topology: &'x str,
    git_dir: &'x str,
) -> impl Fn(Topology) -> Option<String> + 'x {
    // The selection rides in the snippet body, not the process
    // environment: sourcing runs client selection first, which would
    // otherwise clobber globals derived from whatever the state
    // directory currently holds.
    move |detected| {
        let output = shell_eval(
            path_str(home),
            home,
            state,
            &[],
            &format!(
                "DOT_BASE_TOPOLOGY={}\nDOT_CLIENT_GIT_DIR={}\n_dot_init_single_origin {}\ncode=$?\nexit \"$code\"\n",
                sq(topology),
                sq(git_dir),
                topology_word(detected)
            ),
        );
        if output.status.code() != Some(0) {
            return None;
        }
        Some(String::from_utf8_lossy(&chomp(output.stdout)).into_owned())
    }
}

/// Live lane-41 repository-identity canonicalizer.
fn live_repo_identity<'x>(home: &'x Path, state: &'x Path) -> impl Fn(&str) -> Option<String> + 'x {
    move |url| {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            &format!(
                "_dot_init_repo_identity {}\ncode=$?\nexit \"$code\"\n",
                sq(url)
            ),
        );
        if output.status.code() != Some(0) {
            return None;
        }
        Some(String::from_utf8_lossy(&chomp(output.stdout)).into_owned())
    }
}

/// Live lane-35 transaction-directory derivation.
fn live_transaction_dir<'x>(home: &'x Path, state: &'x Path) -> impl Fn() -> Option<PathBuf> + 'x {
    move || {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            "_dot_init_transaction_dir\ncode=$?\nprintf '%s' \"$REPLY\"\nexit \"$code\"\n",
        );
        if output.status.code() != Some(0) {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsString::from_vec(output.stdout)))
    }
}

/// Live lane-35 completion-record derivation.
fn live_completed_file<'x>(home: &'x Path, state: &'x Path) -> impl Fn() -> Option<PathBuf> + 'x {
    move || {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            "_dot_init_completed_file\ncode=$?\nprintf '%s' \"$REPLY\"\nexit \"$code\"\n",
        );
        if output.status.code() != Some(0) {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsString::from_vec(output.stdout)))
    }
}

/// Live lane-35 transaction stager.
fn live_prepare<'x>(home: &'x Path, state: &'x Path) -> impl Fn(&Path) -> Option<PathBuf> + 'x {
    move |transaction| {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            &format!(
                "_dot_init_prepare_transaction {}\ncode=$?\nprintf '%s' \"$REPLY\"\nexit \"$code\"\n",
                sq(path_str(transaction))
            ),
        );
        if output.status.code() != Some(0) {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsString::from_vec(output.stdout)))
    }
}

/// Live lanes-51/54 record journal writer. The commit, nonce, and
/// git identity cross as `DOT_INIT_*` process entries exactly the
/// way the shell adopt sets its globals before calling.
fn live_write_record<'x>(
    home: &'x Path,
    state: &'x Path,
    phases: &'x std::cell::RefCell<Vec<String>>,
) -> impl Fn(&adopt::RecordFields<'_>) -> bool + 'x {
    move |fields| {
        phases.borrow_mut().push(fields.phase.to_string());
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[
                ("DOT_INIT_COMMIT", fields.commit),
                ("DOT_INIT_NONCE", fields.nonce),
                ("DOT_INIT_GIT_DEV", fields.git_dev),
                ("DOT_INIT_GIT_INO", fields.git_ino),
            ],
            &format!(
                "_dot_init_write_record {} {} {} {} {} {} {}\ncode=$?\nexit \"$code\"\n",
                sq(path_str(fields.record)),
                sq(fields.phase),
                sq(fields.origin),
                sq(fields.identity),
                sq(fields.branch),
                sq(fields.backup),
                sq(path_str(fields.git_dir)),
            ),
        );
        output.status.code() == Some(0)
    }
}

/// Live lane-35 transaction publisher.
fn live_publish_transaction<'x>(
    home: &'x Path,
    state: &'x Path,
) -> impl Fn(&Path, &Path) -> bool + 'x {
    move |stage, transaction| {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            &format!(
                "_dot_init_publish_transaction {} {}\ncode=$?\nexit \"$code\"\n",
                sq(path_str(stage)),
                sq(path_str(transaction))
            ),
        );
        output.status.code() == Some(0)
    }
}

/// Live lane-62 completion publisher.
fn live_publish_completed<'x>(home: &'x Path, state: &'x Path) -> impl Fn(&Path) -> bool + 'x {
    move |record| {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            &format!(
                "_dot_init_publish_completed {}\ncode=$?\nexit \"$code\"\n",
                sq(path_str(record))
            ),
        );
        output.status.code() == Some(0)
    }
}

/// Live lanes-51/54 record reader projected onto the four status
/// fields. Values cannot hold tabs (the journal gate), so one tab
/// join carries them back.
fn live_read_record<'x>(
    home: &'x Path,
    state: &'x Path,
) -> impl Fn(&Path) -> Option<adopt::StatusRecord> + 'x {
    move |record| {
        let output = shell_eval_bare(
            path_str(home),
            home,
            state,
            &[],
            &format!(
                concat!(
                    "_dot_init_read_record {}\n",
                    "code=$?\n",
                    "printf '%s\\t%s\\t%s\\t%s\\n' ",
                    "\"$DOT_INIT_PHASE\" \"$DOT_INIT_ORIGIN\" ",
                    "\"$DOT_INIT_BRANCH\" \"$DOT_INIT_BACKUP\"\n",
                    "exit \"$code\"\n",
                ),
                sq(path_str(record))
            ),
        );
        if output.status.code() != Some(0) {
            return None;
        }
        let chomped = chomp(output.stdout);
        let mut fields = chomped.split(|byte| *byte == b'\t');
        let take = |next: Option<&[u8]>| {
            String::from_utf8(next.unwrap_or_default().to_vec()).expect("record is UTF-8")
        };
        Some(adopt::StatusRecord {
            phase: take(fields.next()),
            origin: take(fields.next()),
            branch: take(fields.next()),
            backup: take(fields.next()),
        })
    }
}

/// What the shell oracle did for one adopt call: the exit code, the
/// (always empty) standard output, and the source-time selection the
/// runtime observed before the call.
struct ShellAdopt {
    rc: i32,
    stdout: Vec<u8>,
    topology: String,
    git_dir: String,
}

/// Run the live `_dot_init_adopt_existing` in the shell twin home.
/// The lane-65 convergence stays stubbed (its arguments land in the
/// converge log); everything else runs live. `force` overrides the
/// source-time selection afterwards, for rows unreachable through
/// selection alone. `pre` runs after sourcing but before the call,
/// for sabotage that must postdate selection. Standard error is NOT
/// compared: source-time selection diagnostics belong to the
/// repository model, not this chapter.
#[allow(clippy::too_many_arguments)]
fn shell_adopt(
    home: &Path,
    state: &Path,
    converge_log: &Path,
    converge_rc: i32,
    origin: &str,
    identity: &str,
    branch: &str,
    force: Option<(&str, &str)>,
    pre: &str,
) -> ShellAdopt {
    let mut snippet = String::new();
    if let Some((topology, git_dir)) = force {
        snippet.push_str(&format!(
            "DOT_BASE_TOPOLOGY={topology}\nDOT_CLIENT_GIT_DIR={}\n",
            sq(git_dir)
        ));
    }
    snippet.push_str(pre);
    snippet.push_str(&format!(
        concat!(
            "printf 'select=%s\\t%s\\n' \"$DOT_BASE_TOPOLOGY\" \"$DOT_CLIENT_GIT_DIR\" >&2\n",
            "_dot_init_forward_converge() {{\n",
            "  printf '%s\\t%s\\n' \"$DOT_BASE_TOPOLOGY\" \"$DOT_CLIENT_GIT_DIR\" >>{}\n",
            "  return {converge_rc}\n",
            "}}\n",
            "_dot_init_adopt_existing {} {} {}\n",
            "code=$?\n",
            "exit \"$code\"\n",
        ),
        sq(path_str(converge_log)),
        sq(origin),
        sq(identity),
        sq(branch),
        converge_rc = converge_rc,
    ));
    let output = shell_eval(
        path_str(home),
        home,
        state,
        &[("CONVERGE_LOG", path_str(converge_log))],
        &snippet,
    );
    let mut topology = String::new();
    let mut git_dir = String::new();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if let Some(rest) = line.strip_prefix("select=") {
            let mut parts = rest.split('\t');
            topology = parts.next().unwrap_or_default().to_string();
            git_dir = parts.next().unwrap_or_default().to_string();
        }
    }
    ShellAdopt {
        rc: output.status.code().unwrap_or(99),
        stdout: output.stdout,
        topology,
        git_dir,
    }
}

/// What the Rust engine did for one adopt call: the outcome, the
/// stubbed-convergence log, and the journal phases the
/// record-writer closure saw, in order.
struct RustAdopt {
    result: Result<adopt::Adopted, adopt::AdoptError>,
    converge_log: Vec<Vec<u8>>,
    record_phases: Vec<String>,
}

/// Run [`adopt::adopt_existing`] in the Rust twin home with live
/// closures and one stubbed convergence. `observed_topology` and
/// `observed_git_dir` are the selection the shell oracle reported
/// for the mirrored fixture, so both engines start from the same
/// named inputs.
#[allow(clippy::too_many_arguments)]
fn rust_adopt(
    home: &Path,
    state: &Path,
    observed_topology: &str,
    observed_git_dir: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    converge_ok: bool,
) -> RustAdopt {
    let single_origin = live_single_origin(home, state, observed_topology, observed_git_dir);
    let repo_identity = live_repo_identity(home, state);
    let transaction_dir = live_transaction_dir(home, state);
    let prepare = live_prepare(home, state);
    let phases = std::cell::RefCell::new(Vec::new());
    let write_record = live_write_record(home, state, &phases);
    let publish = live_publish_transaction(home, state);
    let publish_completed = live_publish_completed(home, state);
    let converge_entries = std::cell::RefCell::new(Vec::new());
    let converge = |topology: Topology, git_dir: &Path| {
        let mut entry = topology_word(topology).as_bytes().to_vec();
        entry.push(b'\t');
        entry.extend_from_slice(path_str(git_dir).as_bytes());
        entry.push(b'\n');
        converge_entries.borrow_mut().push(entry);
        converge_ok
    };
    let engine = adopt::AdoptEngine {
        single_origin: &single_origin,
        repo_identity: &repo_identity,
        transaction_dir: &transaction_dir,
        prepare_transaction: &prepare,
        write_record: &write_record,
        publish_transaction: &publish,
        forward_converge: &converge,
        publish_completed: &publish_completed,
    };
    let result = adopt::adopt_existing(
        home,
        parse_topology(observed_topology),
        origin,
        identity,
        branch,
        &engine,
    );
    RustAdopt {
        result,
        converge_log: converge_entries.borrow().clone(),
        record_phases: phases.borrow().clone(),
    }
}

/// Replace every occurrence of `home` with `@HOME@`, so twin-home
/// paths compare directly.
fn normalize(home: &[u8], bytes: &[u8]) -> Vec<u8> {
    if home.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(position) = rest.windows(home.len()).position(|w| w == home) {
        out.extend_from_slice(&rest[..position]);
        out.extend_from_slice(b"@HOME@");
        rest = &rest[position + home.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Split a journal record into its body (with the machine-specific
/// `git_dev=` / `git_ino=` lines dropped) plus the device and inode
/// values, so twin records compare on content while the identity
/// lines are verified against live stat instead.
fn record_parts(bytes: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut body = Vec::new();
    let mut dev = Vec::new();
    let mut ino = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"git_dev=") {
            dev = value.to_vec();
        } else if let Some(value) = line.strip_prefix(b"git_ino=") {
            ino = value.to_vec();
        } else {
            body.extend_from_slice(line);
            body.push(b'\n');
        }
    }
    (body, dev, ino)
}

/// `dev:ino` identity text for one path, via the port's own stat
/// helper (both engines must agree on the value).
fn identity_of(path: &Path) -> String {
    dot::temp::identity_string(dot::temp::path_identity(path).expect("fixture identity"))
}

/// Assert two journal records match: identical content modulo the
/// twin home, with device/inode lines verified against the live
/// stat of each side's git directory.
fn assert_records_match(
    shell_home: &Path,
    rust_home: &Path,
    shell_git_dir: &Path,
    rust_git_dir: &Path,
    shell_record: &[u8],
    rust_record: &[u8],
) {
    let (shell_body, shell_dev, shell_ino) = record_parts(shell_record);
    let (rust_body, rust_dev, rust_ino) = record_parts(rust_record);
    assert_eq!(
        String::from_utf8_lossy(&shell_dev).into_owned()
            + ":"
            + &String::from_utf8_lossy(&shell_ino),
        identity_of(shell_git_dir),
        "shell record carries the shell git identity"
    );
    assert_eq!(
        String::from_utf8_lossy(&rust_dev).into_owned() + ":" + &String::from_utf8_lossy(&rust_ino),
        identity_of(rust_git_dir),
        "rust record carries the rust git identity"
    );
    assert_eq!(
        normalize(path_str(shell_home).as_bytes(), &shell_body),
        normalize(path_str(rust_home).as_bytes(), &rust_body),
        "journal bodies match modulo home"
    );
}

/// Run the live `_dot_init_status`: pure standard output plus the
/// exit code (no code trailer: the snippet carries the verdict in
/// the process status so the streams stay byte-pure).
fn shell_status(home: &str, cwd: &Path, state: &Path) -> (Vec<u8>, Vec<u8>, i32) {
    let output = shell_eval_bare(
        home,
        cwd,
        state,
        &[],
        "_dot_init_status\ncode=$?\nexit \"$code\"\n",
    );
    (
        output.stdout,
        output.stderr,
        output.status.code().unwrap_or(99),
    )
}

/// Run [`adopt::status`] with live closures.
fn rust_status(home: &Path, state: &Path) -> adopt::StatusReport {
    let transaction_dir = live_transaction_dir(home, state);
    let completed_file = live_completed_file(home, state);
    let read_record = live_read_record(home, state);
    let engine = adopt::StatusEngine {
        transaction_dir: &transaction_dir,
        completed_file: &completed_file,
        read_record: &read_record,
    };
    adopt::status(&engine)
}

/// Write one journal record through the live shell into `dest`.
#[allow(clippy::too_many_arguments)]
fn shell_write_record(
    home: &Path,
    state: &Path,
    dest: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    git_dir: &str,
) {
    let output = shell_eval_bare(
        path_str(home),
        home,
        state,
        &[
            ("DOT_INIT_COMMIT", &"a".repeat(40)),
            ("DOT_INIT_NONCE", "n1"),
            ("DOT_INIT_GIT_DEV", "7"),
            ("DOT_INIT_GIT_INO", "8"),
        ],
        &format!(
            "_dot_init_write_record {} {} {} {} {} {} {}\ncode=$?\nexit \"$code\"\n",
            sq(path_str(dest)),
            sq(phase),
            sq(origin),
            sq(identity),
            sq(branch),
            sq(backup),
            sq(git_dir),
        ),
    );
    assert_eq!(output.status.code(), Some(0), "write fixture record");
}

/// Stage a transaction directory through the live shell; returns
/// the stage path.
fn shell_prepare(home: &Path, state: &Path, transaction: &Path) -> PathBuf {
    let output = shell_eval_bare(
        path_str(home),
        home,
        state,
        &[],
        &format!(
            "_dot_init_prepare_transaction {}\ncode=$?\nprintf '%s' \"$REPLY\"\nexit \"$code\"\n",
            sq(path_str(transaction))
        ),
    );
    assert_eq!(output.status.code(), Some(0), "prepare fixture stage");
    PathBuf::from(std::ffi::OsString::from_vec(output.stdout))
}

/// Publish a prepared stage through the live shell.
fn shell_publish(home: &Path, state: &Path, stage: &Path, transaction: &Path) {
    let output = shell_eval_bare(
        path_str(home),
        home,
        state,
        &[],
        &format!(
            "_dot_init_publish_transaction {} {}\ncode=$?\nexit \"$code\"\n",
            sq(path_str(stage)),
            sq(path_str(transaction))
        ),
    );
    assert_eq!(output.status.code(), Some(0), "publish fixture stage");
}

/// Publish a completion record through the live shell.
fn shell_publish_completed(home: &Path, state: &Path, record: &Path) {
    let output = shell_eval_bare(
        path_str(home),
        home,
        state,
        &[],
        &format!(
            "_dot_init_publish_completed {}\ncode=$?\nexit \"$code\"\n",
            sq(path_str(record))
        ),
    );
    assert_eq!(output.status.code(), Some(0), "publish fixture completion");
}

/// Assert two status reports match modulo the twin home and
/// state. Journal paths derive from the state directory, so both
/// roots normalize (they never nest, so the replacements commute).
fn assert_status_match(
    twins: &Twins,
    shell: &(Vec<u8>, Vec<u8>, i32),
    rust: &adopt::StatusReport,
    expect_paths: bool,
) {
    fn normalized(home: &Path, state: &Path, bytes: &[u8]) -> Vec<u8> {
        normalize(
            path_str(state).as_bytes(),
            &normalize(path_str(home).as_bytes(), bytes),
        )
    }
    let (shell_out, shell_err, shell_code) = shell;
    assert_eq!(*shell_code, i32::from(rust.code), "status code");
    assert_eq!(
        normalized(&twins.shell_home, &twins.shell_state, shell_out),
        normalized(&twins.rust_home, &twins.rust_state, &rust.stdout),
        "status stdout"
    );
    assert_eq!(
        normalized(&twins.shell_home, &twins.shell_state, shell_err),
        normalized(&twins.rust_home, &twins.rust_state, &rust.stderr),
        "status stderr"
    );
    if expect_paths {
        let joined = [
            normalized(&twins.shell_home, &twins.shell_state, shell_out),
            normalized(&twins.shell_home, &twins.shell_state, shell_err),
        ]
        .concat();
        assert!(
            joined
                .windows(7)
                .any(|w| w == b"@HOME@/" || w == b"@STATE@"),
            "paths appear in the status report"
        );
    }
}

#[test]
fn usage_bytes_match_shell() {
    let output = shell_eval_bare(
        "/",
        Path::new("/"),
        Path::new("/"),
        &[],
        "_dot_init_usage\ncode=$?\nexit \"$code\"\n",
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(adopt::usage(), output.stdout, "usage bytes");
}

#[test]
fn status_not_started_matches_shell() {
    let twins = Twins::build("status-not-started");
    let shell = shell_status(
        path_str(&twins.shell_home),
        &twins.shell_home,
        &twins.shell_state,
    );
    let rust = rust_status(&twins.rust_home, &twins.rust_state);
    assert_eq!(shell.0, b"initialization: not started\n");
    assert_status_match(&twins, &shell, &rust, false);
    assert_eq!(rust.code, 0);
}

#[test]
fn status_incomplete_matches_shell() {
    let twins = Twins::build("status-incomplete");
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        let transaction = state.join("dot/init/transaction");
        let backup = format!("{}/.dot-backup/x", path_str(home));
        let git_dir = format!("{}/.dotfiles", path_str(home));
        let stage = shell_prepare(home, state, &transaction);
        shell_write_record(
            home,
            state,
            &stage.join("record"),
            "converging",
            "file:///o",
            "file:///o",
            "main",
            &backup,
            &git_dir,
        );
        shell_publish(home, state, &stage, &transaction);
    }
    let shell = shell_status(
        path_str(&twins.shell_home),
        &twins.shell_home,
        &twins.shell_state,
    );
    let rust = rust_status(&twins.rust_home, &twins.rust_state);
    assert_status_match(&twins, &shell, &rust, true);
    assert_eq!(rust.code, 0);
}

#[test]
fn status_complete_matches_shell() {
    let twins = Twins::build("status-complete");
    for (home, state) in [
        (&twins.shell_home, &twins.shell_state),
        (&twins.rust_home, &twins.rust_state),
    ] {
        let transaction = state.join("dot/init/transaction");
        let git_dir = format!("{}/.git", path_str(home));
        let stage = shell_prepare(home, state, &transaction);
        shell_write_record(
            home,
            state,
            &stage.join("record"),
            "complete",
            "file:///o",
            "file:///o",
            "trunk",
            "-",
            &git_dir,
        );
        shell_publish_completed(home, state, &stage.join("record"));
    }
    let shell = shell_status(
        path_str(&twins.shell_home),
        &twins.shell_home,
        &twins.shell_state,
    );
    let rust = rust_status(&twins.rust_home, &twins.rust_state);
    assert_status_match(&twins, &shell, &rust, false);
    assert_eq!(rust.code, 0);
}

#[test]
fn status_malformed_transaction_matches_shell() {
    let twins = Twins::build("status-malformed-tx");
    for state in [&twins.shell_state, &twins.rust_state] {
        let transaction = state.join("dot/init/transaction");
        std::fs::create_dir_all(&transaction).expect("fixture transaction");
        std::fs::write(transaction.join("record"), b"garbage\n").expect("fixture record");
    }
    let shell = shell_status(
        path_str(&twins.shell_home),
        &twins.shell_home,
        &twins.shell_state,
    );
    let rust = rust_status(&twins.rust_home, &twins.rust_state);
    assert_status_match(&twins, &shell, &rust, true);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
}

#[test]
fn status_malformed_completed_matches_shell() {
    let twins = Twins::build("status-malformed-done");
    for state in [&twins.shell_state, &twins.rust_state] {
        let completed = state.join("dot/init/completed");
        std::fs::create_dir_all(completed.parent().expect("state parent")).expect("fixture state");
        std::fs::write(&completed, b"garbage\n").expect("fixture record");
    }
    let shell = shell_status(
        path_str(&twins.shell_home),
        &twins.shell_home,
        &twins.shell_state,
    );
    let rust = rust_status(&twins.rust_home, &twins.rust_state);
    assert_status_match(&twins, &shell, &rust, true);
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty());
}

#[test]
fn status_state_root_unresolvable_matches_shell() {
    let twins = Twins::build("status-no-state");
    let shell = shell_status("", twins.root(), Path::new(""));
    let rust = rust_status(Path::new(""), Path::new(""));
    assert_eq!(shell.2, 1, "shell refuses without a state root");
    assert!(shell.0.is_empty() && shell.1.is_empty());
    assert_eq!(rust.code, 1);
    assert!(rust.stdout.is_empty() && rust.stderr.is_empty());
}

#[test]
fn adopt_error_reports_engine_diagnostics() {
    assert_eq!(
        format!("{}", adopt::AdoptError::NoRepository),
        "no adoptable client repository"
    );
    assert_eq!(
        format!("{}", adopt::AdoptError::Mismatch),
        "existing client repository is untrusted"
    );
    assert_eq!(
        format!("{}", adopt::AdoptError::Failed),
        "existing client repository failed adoption"
    );
    fn is_error<E: std::error::Error>() {}
    is_error::<adopt::AdoptError>();
}

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`.
fn lexical_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Read a log file, empty when absent.
fn read_log(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

/// Mirror an observed shell-home path into the Rust twin home.
fn mirror(shell_home: &Path, rust_home: &Path, observed: &str) -> String {
    observed.replacen(path_str(shell_home), path_str(rust_home), 1)
}

/// Value of one `key=` journal line.
fn record_field<'b>(bytes: &'b [u8], key: &[u8]) -> &'b [u8] {
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(key) {
            return value;
        }
    }
    panic!("record lacks {}", String::from_utf8_lossy(key));
}

/// One no-output adopt row: build the fixture in both homes, run
/// both engines, and compare the verdict plus the absence of side
/// effects. `force_separate` overrides the source-time selection
/// with `separate` (plus the per-side git directory) for rows
/// unreachable through selection alone.
#[allow(clippy::too_many_arguments)]
fn adopt_rc_row(
    tag: &str,
    plant: &dyn Fn(&Path, &Origin, &Path),
    branch: &str,
    force_separate: bool,
    expected_rc: i32,
    expected: adopt::AdoptError,
) {
    let twins = Twins::build(tag);
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant(&twins.shell_home, &origin, twins.root());
    plant(&twins.rust_home, &origin, twins.root());
    let shell_log = twins.root().join(format!("{tag}-sh-converge.log"));
    let shell_git_dir = format!("{}/.dotfiles", path_str(&twins.shell_home));
    let force;
    let force_ref = if force_separate {
        force = ("separate".to_string(), shell_git_dir.clone());
        Some((force.0.as_str(), force.1.as_str()))
    } else {
        None
    };
    let shell = shell_adopt(
        &twins.shell_home,
        &twins.shell_state,
        &shell_log,
        0,
        &url,
        &identity,
        branch,
        force_ref,
        "",
    );
    assert_eq!(shell.rc, expected_rc, "shell verdict for {tag}");
    assert!(shell.stdout.is_empty(), "adopt is silent");
    let (topology, git_dir) = if force_separate {
        (
            "separate".to_string(),
            mirror(&twins.shell_home, &twins.rust_home, &shell_git_dir),
        )
    } else {
        (
            shell.topology.clone(),
            mirror(&twins.shell_home, &twins.rust_home, &shell.git_dir),
        )
    };
    let rust = rust_adopt(
        &twins.rust_home,
        &twins.rust_state,
        &topology,
        &git_dir,
        &url,
        &identity,
        branch,
        true,
    );
    assert_eq!(rust.result, Err(expected), "rust verdict for {tag}");
    assert!(rust.converge_log.is_empty(), "converge never ran");
    assert!(rust.record_phases.is_empty(), "no journal writes");
    assert!(read_log(&shell_log).is_empty(), "shell converge never ran");
}

#[test]
fn adopt_no_repository_matches_shell() {
    adopt_rc_row(
        "adopt-no-repo",
        &|_, _, _| {},
        "main",
        false,
        1,
        adopt::AdoptError::NoRepository,
    );
}

#[test]
fn adopt_home_git_file_matches_shell() {
    adopt_rc_row(
        "adopt-git-file",
        &|home, _, _| std::fs::write(home.join(".git"), b"gitdir: /x\n").expect("fixture"),
        "main",
        false,
        1,
        adopt::AdoptError::NoRepository,
    );
}

#[test]
fn adopt_stray_git_dir_matches_shell() {
    // `$HOME/.git` is a real directory serving another worktree, so
    // the top-level probe rejects it. (A `--separate-git-dir` layout
    // does not stray: git still reports the parent as its top
    // level.)
    adopt_rc_row(
        "adopt-stray-git",
        &|home, _, _| {
            let elsewhere = home.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).expect("fixture worktree");
            git(&[
                "-C",
                path_str(home),
                "init",
                "--quiet",
                "--initial-branch",
                "main",
            ]);
            git(&[
                "-C",
                path_str(home),
                "config",
                "core.worktree",
                path_str(&elsewhere),
            ]);
        },
        "main",
        false,
        1,
        adopt::AdoptError::NoRepository,
    );
}

#[test]
fn adopt_unselected_bare_dir_matches_shell() {
    // A remote-less bare `$HOME/.dotfiles` fails source-time
    // selection, so adoption sees `missing` and reports no
    // repository — the select-then-adopt interplay, not a trust
    // verdict.
    adopt_rc_row(
        "adopt-unselected",
        &|home, _, _| {
            git(&[
                "init",
                "--quiet",
                "--bare",
                path_str(&home.join(".dotfiles")),
            ]);
        },
        "main",
        false,
        1,
        adopt::AdoptError::NoRepository,
    );
}

#[test]
fn adopt_forced_remote_less_dir_matches_shell() {
    // Same shape with the selection forced to `separate`: the
    // single-origin read fails, which is a trust verdict.
    adopt_rc_row(
        "adopt-forced-noremote",
        &|home, _, _| {
            git(&[
                "init",
                "--quiet",
                "--bare",
                path_str(&home.join(".dotfiles")),
            ]);
        },
        "main",
        true,
        2,
        adopt::AdoptError::Mismatch,
    );
}

#[test]
fn adopt_identity_mismatch_matches_shell() {
    // Both homes clone a shared foreign origin while the requested
    // identity names the main origin: recorded and requested differ
    // on both engines.
    adopt_rc_row(
        "adopt-foreign",
        &|home, _origin, root| {
            let foreign = make_origin(&root.join("foreign"));
            git(&[
                "clone",
                "--quiet",
                "--bare",
                path_str(&foreign.path),
                path_str(&home.join(".dotfiles")),
            ]);
            write(home, ".testrc", b"hello\n");
        },
        "main",
        false,
        2,
        adopt::AdoptError::Mismatch,
    );
}

#[test]
fn adopt_branch_mismatch_matches_shell() {
    adopt_rc_row(
        "adopt-branch-separate",
        &|home, origin, _| plant_separate(home, origin),
        "trunk",
        false,
        2,
        adopt::AdoptError::Mismatch,
    );
    adopt_rc_row(
        "adopt-branch-ordinary",
        &|home, origin, _| plant_ordinary(home, origin),
        "trunk",
        false,
        2,
        adopt::AdoptError::Mismatch,
    );
}

#[test]
fn adopt_ordinary_without_remote_matches_shell() {
    adopt_rc_row(
        "adopt-ord-noremote",
        &|home, _, _| {
            git(&[
                "-C",
                path_str(home),
                "init",
                "--quiet",
                "--initial-branch",
                "main",
            ]);
        },
        "main",
        false,
        2,
        adopt::AdoptError::Mismatch,
    );
}

#[test]
fn adopt_unborn_head_matches_shell() {
    let plant_unborn = |home: &Path, origin: &Origin, _root: &Path| {
        let git_dir = home.join(".dotfiles");
        let url = format!("file://{}", path_str(&origin.path));
        git(&["init", "--quiet", "--bare", path_str(&git_dir)]);
        git(&[
            "--git-dir",
            path_str(&git_dir),
            "remote",
            "add",
            "origin",
            &url,
        ]);
    };
    // The unborn head passes source-time selection (a symref needs
    // no commit), then fails the branch/commit trust gates: whether
    // the default branch reads `main` or not, the verdict is a
    // mismatch either way.
    adopt_rc_row(
        "adopt-unborn",
        &plant_unborn,
        "main",
        false,
        2,
        adopt::AdoptError::Mismatch,
    );
}

/// One successful adopt row for `plant`: both engines converge
/// (stubbed), publish live completions, and must agree on the
/// verdict, the journal bytes, the convergence arguments, and the
/// filesystem effects.
fn adopt_happy_row(tag: &str, plant: &dyn Fn(&Path, &Origin), git_name: &str) {
    let twins = Twins::build(tag);
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant(&twins.shell_home, &origin);
    plant(&twins.rust_home, &origin);
    let shell_log = twins.root().join(format!("{tag}-sh-converge.log"));
    let shell = shell_adopt(
        &twins.shell_home,
        &twins.shell_state,
        &shell_log,
        0,
        &url,
        &identity,
        "main",
        None,
        "",
    );
    assert_eq!(shell.rc, 0, "shell adopts");
    assert!(shell.stdout.is_empty(), "adopt is silent");
    let rust_git_dir = mirror(&twins.shell_home, &twins.rust_home, &shell.git_dir);
    let rust = rust_adopt(
        &twins.rust_home,
        &twins.rust_state,
        &shell.topology,
        &rust_git_dir,
        &url,
        &identity,
        "main",
        true,
    );
    let expected_git_dir = twins.rust_home.join(git_name);
    let expected_topology = if git_name == ".dotfiles" {
        Topology::Separate
    } else {
        Topology::Ordinary
    };
    // The ordinary shape defers selection: the runtime still
    // reports `missing` while adoption detects `ordinary`.
    let observed = if git_name == ".dotfiles" {
        "separate"
    } else {
        "missing"
    };
    assert_eq!(shell.topology, observed);
    assert_eq!(
        rust.result,
        Ok(adopt::Adopted {
            topology: expected_topology,
            git_dir: expected_git_dir.clone(),
        }),
        "rust adopts with the detected shape"
    );
    assert_eq!(rust.record_phases, ["converging", "complete"]);
    assert_eq!(
        normalize(
            path_str(&twins.shell_home).as_bytes(),
            &read_log(&shell_log)
        ),
        normalize(
            path_str(&twins.rust_home).as_bytes(),
            &rust.converge_log.concat()
        ),
        "convergence arguments match"
    );
    assert_eq!(rust.converge_log.len(), 1, "one convergence");
    let shell_completed =
        std::fs::read(twins.shell_state.join("dot/init/completed")).expect("shell completed");
    let rust_completed =
        std::fs::read(twins.rust_state.join("dot/init/completed")).expect("rust completed");
    assert_records_match(
        &twins.shell_home,
        &twins.rust_home,
        &twins.shell_home.join(git_name),
        &expected_git_dir,
        &shell_completed,
        &rust_completed,
    );
    for (side, record) in [("shell", &shell_completed), ("rust", &rust_completed)] {
        assert_eq!(record_field(record, b"phase="), b"complete", "{side} phase");
        assert_eq!(
            record_field(record, b"origin="),
            url.as_bytes(),
            "{side} origin"
        );
        assert_eq!(
            record_field(record, b"identity="),
            identity.as_bytes(),
            "{side} identity"
        );
        assert_eq!(record_field(record, b"branch="), b"main", "{side} branch");
        assert_eq!(record_field(record, b"backup="), b"-", "{side} backup");
        assert_eq!(record_field(record, b"nonce="), b"adopted", "{side} nonce");
        assert_eq!(
            record_field(record, b"commit="),
            origin.commit.as_bytes(),
            "{side} commit"
        );
    }
    for (side, state) in [("shell", &twins.shell_state), ("rust", &twins.rust_state)] {
        assert!(
            !lexical_exists(&state.join("dot/init/transaction")),
            "{side} transaction is removed"
        );
    }
}

#[test]
fn adopt_separate_success_matches_shell() {
    adopt_happy_row("adopt-happy-separate", &plant_separate, ".dotfiles");
}

#[test]
fn adopt_ordinary_success_matches_shell() {
    adopt_happy_row("adopt-happy-ordinary", &plant_ordinary, ".git");
}

#[test]
fn adopt_converge_failure_matches_shell() {
    // A failing convergence aborts after publication: the
    // converging journal lingers in the transaction on both
    // engines.
    let twins = Twins::build("adopt-converge-fail");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant_separate(&twins.shell_home, &origin);
    plant_separate(&twins.rust_home, &origin);
    let shell_log = twins.root().join("sh-converge.log");
    let shell = shell_adopt(
        &twins.shell_home,
        &twins.shell_state,
        &shell_log,
        1,
        &url,
        &identity,
        "main",
        None,
        "",
    );
    assert_eq!(shell.rc, 3);
    let rust_git_dir = mirror(&twins.shell_home, &twins.rust_home, &shell.git_dir);
    let rust = rust_adopt(
        &twins.rust_home,
        &twins.rust_state,
        &shell.topology,
        &rust_git_dir,
        &url,
        &identity,
        "main",
        false,
    );
    assert_eq!(rust.result, Err(adopt::AdoptError::Failed));
    assert_eq!(rust.record_phases, ["converging"]);
    assert_eq!(rust.converge_log.len(), 1);
    assert_eq!(
        normalize(
            path_str(&twins.shell_home).as_bytes(),
            &read_log(&shell_log)
        ),
        normalize(
            path_str(&twins.rust_home).as_bytes(),
            &rust.converge_log.concat()
        ),
        "convergence arguments match"
    );
    for (side, state) in [("shell", &twins.shell_state), ("rust", &twins.rust_state)] {
        assert!(
            lexical_exists(&state.join("dot/init/transaction/record")),
            "{side} transaction lingers"
        );
    }
    let shell_record = std::fs::read(twins.shell_state.join("dot/init/transaction/record"))
        .expect("shell converging record");
    let rust_record = std::fs::read(twins.rust_state.join("dot/init/transaction/record"))
        .expect("rust converging record");
    assert_records_match(
        &twins.shell_home,
        &twins.rust_home,
        &twins.shell_home.join(".dotfiles"),
        &twins.rust_home.join(".dotfiles"),
        &shell_record,
        &rust_record,
    );
    assert_eq!(record_field(&shell_record, b"phase="), b"converging");
    assert_eq!(record_field(&rust_record, b"phase="), b"converging");
}

#[test]
fn adopt_completed_publication_failure_matches_shell() {
    // A completed path blocked by a directory fails the final
    // publication: the complete journal lingers in the transaction
    // on both engines.
    let twins = Twins::build("adopt-publish-fail");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant_separate(&twins.shell_home, &origin);
    plant_separate(&twins.rust_home, &origin);
    // The block must postdate source-time selection (which reads
    // the completion record): the shell sabotages itself after
    // sourcing, while the Rust side blocks up front.
    let blocked = twins.rust_state.join("dot/init/completed");
    std::fs::create_dir_all(&blocked).expect("block completion");
    let shell_blocked = twins.shell_state.join("dot/init/completed");
    let shell_log = twins.root().join("sh-converge.log");
    let shell = shell_adopt(
        &twins.shell_home,
        &twins.shell_state,
        &shell_log,
        0,
        &url,
        &identity,
        "main",
        None,
        &format!("mkdir -p {}\n", sq(path_str(&shell_blocked))),
    );
    assert_eq!(shell.rc, 3);
    let rust_git_dir = mirror(&twins.shell_home, &twins.rust_home, &shell.git_dir);
    let rust = rust_adopt(
        &twins.rust_home,
        &twins.rust_state,
        &shell.topology,
        &rust_git_dir,
        &url,
        &identity,
        "main",
        true,
    );
    assert_eq!(rust.result, Err(adopt::AdoptError::Failed));
    assert_eq!(rust.record_phases, ["converging", "complete"]);
    assert_eq!(rust.converge_log.len(), 1);
    let shell_record = std::fs::read(twins.shell_state.join("dot/init/transaction/record"))
        .expect("shell complete record");
    let rust_record = std::fs::read(twins.rust_state.join("dot/init/transaction/record"))
        .expect("rust complete record");
    assert_records_match(
        &twins.shell_home,
        &twins.rust_home,
        &twins.shell_home.join(".dotfiles"),
        &twins.rust_home.join(".dotfiles"),
        &shell_record,
        &rust_record,
    );
    assert_eq!(record_field(&shell_record, b"phase="), b"complete");
    assert_eq!(record_field(&rust_record, b"phase="), b"complete");
}

/// Count leftover `<transaction>.prepare.*` stage orphans.
fn prepare_orphans(state: &Path) -> usize {
    let dir = state.join("dot/init");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("transaction.prepare."))
        })
        .count()
}

#[test]
fn adopt_transaction_publication_failure_matches_shell() {
    // A pre-existing transaction directory defeats the exclusive
    // publication move: the prepared stage orphans on both engines.
    let twins = Twins::build("adopt-tx-publish-fail");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant_separate(&twins.shell_home, &origin);
    plant_separate(&twins.rust_home, &origin);
    for state in [&twins.shell_state, &twins.rust_state] {
        std::fs::create_dir_all(state.join("dot/init/transaction")).expect("block transaction");
    }
    let shell_log = twins.root().join("sh-converge.log");
    let shell = shell_adopt(
        &twins.shell_home,
        &twins.shell_state,
        &shell_log,
        0,
        &url,
        &identity,
        "main",
        None,
        "",
    );
    assert_eq!(shell.rc, 3);
    let rust_git_dir = mirror(&twins.shell_home, &twins.rust_home, &shell.git_dir);
    let rust = rust_adopt(
        &twins.rust_home,
        &twins.rust_state,
        &shell.topology,
        &rust_git_dir,
        &url,
        &identity,
        "main",
        true,
    );
    assert_eq!(rust.result, Err(adopt::AdoptError::Failed));
    assert_eq!(rust.record_phases, ["converging"]);
    assert!(rust.converge_log.is_empty());
    assert_eq!(
        prepare_orphans(&twins.shell_state),
        1,
        "shell stage orphans"
    );
    assert_eq!(prepare_orphans(&twins.rust_state), 1, "rust stage orphans");
    for (side, state) in [("shell", &twins.shell_state), ("rust", &twins.rust_state)] {
        assert!(
            !lexical_exists(&state.join("dot/init/transaction/record")),
            "{side} journal never publishes"
        );
    }
}

#[test]
fn adopt_prepare_failure_matches_shell() {
    // A state root blocked by a file fails staging before any
    // journal write on both engines.
    let twins = Twins::build("adopt-prepare-fail");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant_separate(&twins.shell_home, &origin);
    plant_separate(&twins.rust_home, &origin);
    for state in [&twins.shell_state, &twins.rust_state] {
        let parent = state.join("dot");
        std::fs::create_dir_all(&parent).expect("fixture state");
        std::fs::write(parent.join("init"), b"blocked\n").expect("block staging");
    }
    let shell_log = twins.root().join("sh-converge.log");
    let shell = shell_adopt(
        &twins.shell_home,
        &twins.shell_state,
        &shell_log,
        0,
        &url,
        &identity,
        "main",
        None,
        "",
    );
    assert_eq!(shell.rc, 3);
    let rust_git_dir = mirror(&twins.shell_home, &twins.rust_home, &shell.git_dir);
    let rust = rust_adopt(
        &twins.rust_home,
        &twins.rust_state,
        &shell.topology,
        &rust_git_dir,
        &url,
        &identity,
        "main",
        true,
    );
    assert_eq!(rust.result, Err(adopt::AdoptError::Failed));
    assert!(rust.record_phases.is_empty());
    assert!(rust.converge_log.is_empty());
    for (side, state) in [("shell", &twins.shell_state), ("rust", &twins.rust_state)] {
        assert!(
            !lexical_exists(&state.join("dot/init/transaction")),
            "{side} transaction never stages"
        );
    }
}

#[test]
fn adopt_unresolvable_transaction_dir_fails() {
    // Not differential: an underivable transaction directory is
    // unreachable through selection (detection fails first), so no
    // live row produces it. The stubbed derivation pins the
    // defensive mapping to the workflow-failure code on a fixture
    // that passes every earlier gate live.
    let twins = Twins::build("adopt-no-tx-dir");
    let origin = make_origin(twins.root());
    let url = format!("file://{}", path_str(&origin.path));
    let identity = live_identity(&origin);
    plant_separate(&twins.rust_home, &origin);
    let home = &twins.rust_home;
    let state = &twins.rust_state;
    let git_dir = format!("{}/.dotfiles", path_str(home));
    let single_origin = live_single_origin(home, state, "separate", &git_dir);
    let repo_identity = live_repo_identity(home, state);
    let transaction_dir = || None::<PathBuf>;
    let prepare = live_prepare(home, state);
    let phases = std::cell::RefCell::new(Vec::new());
    let write_record = live_write_record(home, state, &phases);
    let publish = live_publish_transaction(home, state);
    let publish_completed = live_publish_completed(home, state);
    let converge = |_: Topology, _: &Path| true;
    let engine = adopt::AdoptEngine {
        single_origin: &single_origin,
        repo_identity: &repo_identity,
        transaction_dir: &transaction_dir,
        prepare_transaction: &prepare,
        write_record: &write_record,
        publish_transaction: &publish,
        forward_converge: &converge,
        publish_completed: &publish_completed,
    };
    assert_eq!(
        adopt::adopt_existing(home, Topology::Separate, &url, &identity, "main", &engine),
        Err(adopt::AdoptError::Failed)
    );
    assert!(phases.borrow().is_empty());
}
