//! Base client repository identity and command selection.
//!
//! The shared selector checks initialized-client records and legacy separate
//! Git directories before publishing a Base. Native doctor and test use this
//! same authority boundary; repository commands consume its topology model.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};

/// Failures that must not be mistaken for an empty Git query result before a
/// caller performs durable repository mutations.
#[derive(Debug)]
pub(crate) enum GitOutputError {
    Interrupted(i32),
    CaptureLimit,
    CleanupIncomplete,
    Other,
}

/// `_base_repo_exists` shape: which `git` command form addresses
/// the base client repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// `missing`: no base repository selected.
    Missing,
    /// `separate`: detached git dir with `$HOME` as the work tree.
    Separate,
    /// `ordinary`: plain checkout rooted at `$HOME`.
    Ordinary,
}

/// `_normalize_repo` dispatch: base repository or one overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    /// The base client repository.
    Base,
    /// One overlay checkout.
    Overlay,
}

/// Explicit caller state for base dispatch: the selected topology,
/// the client git directory, and home.
#[derive(Debug, Clone)]
pub struct Base {
    /// Selected topology (`DOT_BASE_TOPOLOGY`).
    pub topology: Topology,
    /// Client git directory (`DOT_CLIENT_GIT_DIR`).
    pub client_git_dir: String,
    /// Home directory (`HOME`), the work tree for both topologies.
    pub home: String,
}

impl Base {
    /// `_base_repo_exists`: any topology but `missing`.
    pub fn exists(&self) -> bool {
        !matches!(self.topology, Topology::Missing)
    }

    /// `_base_git` argv prefix for `git`, or `None` when the
    /// topology is missing (shell exit 128). The `--opt=value`
    /// spelling equals the shell's `--opt value` spelling.
    pub fn git_prefix(&self) -> Option<Vec<OsString>> {
        match self.topology {
            Topology::Missing => None,
            Topology::Separate => Some(vec![
                OsString::from(format!("--git-dir={}", self.client_git_dir)),
                OsString::from(format!("--work-tree={}", self.home)),
            ]),
            Topology::Ordinary => Some(vec![OsString::from("-C"), OsString::from(&self.home)]),
        }
    }
}

/// `(path, sync)` from an overlay record (`name|path|url|...|sync`).
/// Missing fields read empty like shell `read`; `sync` defaults to
/// `git` like `${sync:-git}`. A seventh field stays glued to `sync`
/// (`read` parks the remainder in the last variable), so a record
/// like `n|p|u|d|o|git|x` does not match `git`.
pub fn overlay_path_sync(entry: &str) -> (String, String) {
    let fields: Vec<&str> = entry.split('|').collect();
    let path = fields.get(1).copied().unwrap_or("").to_string();
    let rest = if fields.len() > 5 {
        fields[5..].join("|")
    } else {
        String::new()
    };
    let sync = if rest.is_empty() {
        "git".to_string()
    } else {
        rest
    };
    (path, sync)
}

/// Run `git` with `prefix` plus `args`: stdout piped, stderr
/// nulled, stdin null. `None` on spawn failure (callers treat that
/// like any other git failure).
pub fn run_git(prefix: &[OsString], args: &[&str]) -> Option<Output> {
    run_git_typed(prefix, args).ok()
}

pub(crate) fn run_git_typed(prefix: &[OsString], args: &[&str]) -> Result<Output, GitOutputError> {
    let mut cmd = crate::init_client_identity::host_git_command();
    cmd.args(prefix)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::cleanup::run_session_output_typed(
        cmd,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    )
    .map(|mut output| {
        // `run_git` has always nulled inspection stderr. The bounded
        // supervisor captures it only so a writer cannot hold teardown
        // open; do not change the returned public contract.
        output.stderr.clear();
        output
    })
    .map_err(|error| match error {
        crate::cleanup::SessionOutputError::Interrupted(signal) => {
            GitOutputError::Interrupted(signal)
        }
        crate::cleanup::SessionOutputError::CaptureLimit => GitOutputError::CaptureLimit,
        crate::cleanup::SessionOutputError::CleanupIncomplete => GitOutputError::CleanupIncomplete,
        crate::cleanup::SessionOutputError::Io(_)
        | crate::cleanup::SessionOutputError::TimedOut => GitOutputError::Other,
    })
}

pub(crate) fn select(
    runtime: &crate::app::Runtime,
    home: &str,
    state: &str,
    stderr: &mut dyn std::io::Write,
) -> Result<crate::repos_base::Base, ()> {
    select_with(runtime, home, state, stderr, false)
}

/// Validate client identity for `dot init`, which may adopt an ordinary
/// checkout that does not have a completed identity yet.
pub(crate) fn select_for_init(
    runtime: &crate::app::Runtime,
    home: &str,
    state: &str,
    stderr: &mut dyn std::io::Write,
) -> Result<crate::repos_base::Base, ()> {
    select_with(runtime, home, state, stderr, true)
}

fn select_with(
    runtime: &crate::app::Runtime,
    home: &str,
    state: &str,
    stderr: &mut dyn std::io::Write,
    allow_uninitialized_ordinary: bool,
) -> Result<crate::repos_base::Base, ()> {
    let completed = Path::new(state).join("dot/init/completed");
    let transaction = Path::new(state).join("dot/init/transaction/record");
    let selected = if std::fs::symlink_metadata(&completed).is_ok() {
        Some((&completed, true))
    } else if std::fs::symlink_metadata(&transaction).is_ok() {
        Some((&transaction, false))
    } else {
        None
    };
    if let Some((record_path, completed_record)) = selected {
        let record = match crate::init_client_record::read_record(record_path, Path::new(home)) {
            Ok(record) if !completed_record || record.phase == "complete" => record,
            _ => {
                let _ = stderr.write_all(b"dot: malformed initialization identity record\n");
                return Err(());
            }
        };
        let topology = if record.git_dir == format!("{home}/.dotfiles") {
            "separate"
        } else if record.git_dir == format!("{home}/.git") {
            "ordinary"
        } else {
            let _ = stderr
                .write_all(b"dot: initialization identity names an unsupported Git directory\n");
            return Err(());
        };
        let live_exists = std::fs::symlink_metadata(&record.git_dir).is_ok();
        if topology == "separate" && !live_exists {
            return Ok(crate::cli::base_from_values(home, Some("missing"), None));
        }
        if !client_matches(&record, Path::new(home)) {
            let line = if topology == "ordinary" {
                b"dot: ordinary HOME checkout no longer matches initialization identity\n"
                    .as_slice()
            } else {
                b"dot: client Git directory no longer matches initialization identity\n".as_slice()
            };
            let _ = stderr.write_all(line);
            return Err(());
        }
        return Ok(crate::cli::base_from_values(
            home,
            Some(topology),
            Some(&record.git_dir),
        ));
    }
    let legacy = Path::new(home).join(".dotfiles");
    if std::fs::symlink_metadata(&legacy).is_ok() {
        if !legacy_client_valid(runtime, &legacy, home) {
            let _ = writeln!(
                stderr,
                "dot: unsupported or foreign client Git directory: {}",
                legacy.display()
            );
            return Err(());
        }
        return Ok(crate::cli::base_from_values(
            home,
            Some("separate"),
            legacy.to_str(),
        ));
    }
    if std::fs::symlink_metadata(Path::new(home).join(".git")).is_ok() {
        if allow_uninitialized_ordinary {
            return Ok(crate::cli::base_from_values(
                home,
                Some("ordinary"),
                Some(&format!("{home}/.git")),
            ));
        }
        let _ = stderr
            .write_all(b"dot: ordinary HOME checkout requires a completed dot init identity\n");
        return Err(());
    }
    Ok(crate::cli::base_from_values(home, Some("missing"), None))
}

/// Memoized [`legacy_client_valid_uncached`] with the same TRUE-only,
/// cancellation-bypassing policy as [`client_matches`]. Dispatch
/// selects the base before every command and `status`/`diff`/`fetch`/
/// `push`, `doctor`, and `test` select it again; without the memo each
/// selection re-ran the five-probe legacy validation. The key binds the
/// Git directory's device and inode, so a replaced directory re-probes,
/// and [`invalidate_client_match_cache`] clears these verdicts too.
fn legacy_client_valid(runtime: &crate::app::Runtime, git_dir: &Path, home: &str) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match std::fs::symlink_metadata(git_dir) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    if crate::cancellation::check().is_err() {
        return legacy_client_valid_uncached(runtime, git_dir, home);
    }
    let mut key = b"legacy\0".to_vec();
    for part in [
        git_dir.as_os_str().as_bytes(),
        meta.dev().to_string().as_bytes(),
        meta.ino().to_string().as_bytes(),
        home.as_bytes(),
    ] {
        key.extend_from_slice(part);
        key.push(0);
    }
    if let Ok(cache) = client_match_cache().lock() {
        if cache.contains_key(&key) {
            return true;
        }
    }
    let valid = legacy_client_valid_uncached(runtime, git_dir, home);
    if valid {
        if let Ok(mut cache) = client_match_cache().lock() {
            cache.insert(key, ());
        }
    }
    valid
}

fn legacy_client_valid_uncached(runtime: &crate::app::Runtime, git_dir: &Path, home: &str) -> bool {
    let Some(absolute) = git_dir_output(runtime, git_dir, &["rev-parse", "--absolute-git-dir"])
    else {
        return false;
    };
    let absolute = PathBuf::from(OsString::from_vec(chomp_newlines(absolute)));
    if std::fs::canonicalize(absolute).ok() != std::fs::canonicalize(git_dir).ok() {
        return false;
    }
    let Some(urls) = git_dir_output(
        runtime,
        git_dir,
        &["config", "--get-all", "remote.origin.url"],
    ) else {
        return false;
    };
    let urls = output_lines(&urls);
    if urls.len() != 1
        || std::str::from_utf8(urls[0])
            .ok()
            .and_then(crate::init_client_identity::repo_identity)
            .is_none()
    {
        return false;
    }
    let Some(branch) = git_dir_output(runtime, git_dir, &["symbolic-ref", "--short", "HEAD"])
    else {
        return false;
    };
    if !std::str::from_utf8(&chomp_newlines(branch))
        .is_ok_and(crate::init_client_identity::branch_valid)
    {
        return false;
    }
    let Some(bare) = git_dir_output(runtime, git_dir, &["config", "--bool", "core.bare"]) else {
        return false;
    };
    match chomp_newlines(bare).as_slice() {
        b"true" => true,
        b"false" => git_dir_output(runtime, git_dir, &["config", "core.worktree"])
            .is_some_and(|worktree| chomp_newlines(worktree) == home.as_bytes()),
        _ => false,
    }
}

fn output_lines(output: &[u8]) -> Vec<&[u8]> {
    let output = output.strip_suffix(b"\n").unwrap_or(output);
    if output.is_empty() {
        Vec::new()
    } else {
        output.split(|byte| *byte == b'\n').collect()
    }
}

fn chomp_newlines(mut output: Vec<u8>) -> Vec<u8> {
    while output.last() == Some(&b'\n') {
        output.pop();
    }
    output
}

/// Memoized `client_matches` TRUE verdicts by record identity plus
/// home (and, under a `legacy` key prefix, [`legacy_client_valid`]
/// verdicts by Git directory identity plus home). Dispatch validates the base client before running the
/// command, and `update` gather validates it again before the pull
/// phase; each validation is up to five supervised `git` probes
/// against unchanging state, so the second call shares the first
/// answer. Only TRUE pins: a mismatch re-probes, so a checkout
/// converging mid-run (staged clone landing between phases) is
/// observed.
static CLIENT_MATCH_CACHE: OnceLock<Mutex<HashMap<Vec<u8>, ()>>> = OnceLock::new();

fn client_match_cache() -> &'static Mutex<HashMap<Vec<u8>, ()>> {
    CLIENT_MATCH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn client_match_key(record: &crate::init_client_record::TransactionRecord, home: &Path) -> Vec<u8> {
    let mut key = Vec::new();
    for part in [
        record.git_dir.as_bytes(),
        record.git_dev.as_bytes(),
        record.git_ino.as_bytes(),
        record.nonce.as_bytes(),
        record.identity.as_bytes(),
        record.branch.as_bytes(),
        home.as_os_str().as_bytes(),
    ] {
        key.extend_from_slice(part);
        key.push(0);
    }
    key
}

/// Drop every memoized client-match verdict. Call after any engine
/// phase that may have moved the base checkout's branch or HEAD
/// (pull, staged clone into place) and after arbitrary user code
/// (hooks) or git passthrough. Link-only generation restores need
/// no call. Over-invalidation only costs a re-probe; a missed
/// invalidation would trust a replaced checkout.
pub(crate) fn invalidate_client_match_cache() {
    if let Ok(mut cache) = client_match_cache().lock() {
        cache.clear();
    }
}

fn client_matches(record: &crate::init_client_record::TransactionRecord, home: &Path) -> bool {
    // Match the uncached path under cancellation: the supervised
    // probes observe the latched signal and fail, which reads as a
    // mismatch. Serving a stale TRUE here would let teardown
    // proceed as if uninterrupted.
    if crate::cancellation::check().is_err() {
        return client_matches_uncached(record, home);
    }
    let key = client_match_key(record, home);
    if let Ok(cache) = client_match_cache().lock() {
        if cache.contains_key(&key) {
            return true;
        }
    }
    let matched = client_matches_uncached(record, home);
    if matched {
        if let Ok(mut cache) = client_match_cache().lock() {
            cache.insert(key, ());
        }
    }
    matched
}

fn client_matches_uncached(
    record: &crate::init_client_record::TransactionRecord,
    home: &Path,
) -> bool {
    let git_dir = Path::new(&record.git_dir);
    let path_identity =
        |path: &Path| crate::temp::path_identity(path).map(crate::temp::identity_string);
    let generation_matches = |path: &Path| {
        crate::init_client_generation::generation_marker_matches(
            path,
            &record.nonce,
            &record.commit,
            &record.identity,
        )
    };
    let repo_identity = |origin: &str| {
        crate::init_client_identity::repo_identity(origin).ok_or(crate::errors::Error::Usage {
            message: "unsupported repository URL",
        })
    };
    let inputs = crate::init_client_resume::LiveGitInputs {
        git_dir,
        git_dev: &record.git_dev,
        git_ino: &record.git_ino,
        nonce: &record.nonce,
        identity: &record.identity,
        branch: &record.branch,
        home,
    };
    let deps = crate::init_client_resume::LiveGitDeps {
        path_identity: &path_identity,
        generation_matches: &generation_matches,
        repo_identity: &repo_identity,
    };
    crate::init_client_resume::live_git_matches_record(&inputs, &deps)
}

fn git_dir_output(runtime: &crate::app::Runtime, git_dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let program = runtime.git_program()?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(runtime.env())
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .stderr(Stdio::null());
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    )
    .ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt as _};

    const CANNED_URL: &str = "https://example.invalid/dotfiles.git";
    const CANNED_HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    struct IdentityGit {
        _scope: dot_test_support::TempDir,
        home: PathBuf,
        log: PathBuf,
        shim: PathBuf,
    }

    impl IdentityGit {
        fn answering(tag: &str, branch: &str, worktree: &str) -> Self {
            let scope = dot_test_support::TempDir::new_exec(&format!("identity-git-{tag}"))
                .expect("identity git scope");
            let home = scope.path().join("home");
            let git_dir = home.join(".dotfiles");
            std::fs::create_dir_all(&git_dir).expect("fixture git dir");
            let log = scope.path().join("invocations.log");
            let shim = scope.path().join("git");
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\ncase \"$3\" in\n  config)\n    case \"$4\" in\n      --get-all) printf '%s\\n' \"{CANNED_URL}\";;\n      --bool) printf 'false\\n';;\n      *) printf '%s\\n' \"{worktree}\";;\n    esac;;\n  symbolic-ref) printf '%s\\n' \"{branch}\";;\n  rev-parse) printf '%s\\n' \"{CANNED_HEAD}\";;\nesac\n",
                    log = log.display(),
                ),
            )
            .expect("identity git shim");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
                .expect("identity git mode");
            Self {
                _scope: scope,
                home,
                log,
                shim,
            }
        }

        fn matching(tag: &str) -> Self {
            // The identity probes export the fixture home as `HOME`,
            // so the shim answers the worktree query from the runtime
            // environment instead of a baked-in path.
            Self::answering(tag, "main", "$HOME")
        }

        fn invocations(&self) -> usize {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .count()
        }

        fn record(&self, branch: &str) -> crate::init_client_record::TransactionRecord {
            let git_dir = self.home.join(".dotfiles");
            let meta = std::fs::metadata(&git_dir).expect("git dir stat");
            let identity = crate::init_client_identity::repo_identity(CANNED_URL)
                .expect("canned URL identity");
            crate::init_client_record::TransactionRecord {
                phase: "complete".to_string(),
                origin: CANNED_URL.to_string(),
                identity,
                branch: branch.to_string(),
                commit: CANNED_HEAD.to_string(),
                git_dir: git_dir.to_string_lossy().into_owned(),
                worktree: self.home.to_string_lossy().into_owned(),
                backup: "-".to_string(),
                dot: "/nonexistent".to_string(),
                dot_revision: CANNED_HEAD.to_string(),
                nonce: "adopted".to_string(),
                git_dev: meta.dev().to_string(),
                git_ino: meta.ino().to_string(),
            }
        }
    }

    /// A legacy `~/.dotfiles` client plus a Git shim that answers the
    /// five legacy validation probes (`--git-dir <dir> <command> ...`) and
    /// logs every call. `bare` selects the `core.bare` answer; `false`
    /// sends validation to the `core.worktree` probe, which the shim
    /// answers with a foreign worktree so validation fails.
    struct LegacyGit {
        scope: dot_test_support::TempDir,
        home: PathBuf,
        git_dir: PathBuf,
        log: PathBuf,
        shim: PathBuf,
    }

    impl LegacyGit {
        fn new(tag: &str, bare: bool) -> Self {
            let scope = dot_test_support::TempDir::new_exec(&format!("legacy-git-{tag}"))
                .expect("legacy git scope");
            let home = scope.path().join("home");
            let git_dir = home.join(".dotfiles");
            std::fs::create_dir_all(&git_dir).expect("legacy git dir");
            let log = scope.path().join("invocations.log");
            let shim = scope.path().join("git");
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$3\" in\n  rev-parse) printf '%s\\n' \"$2\";;\n  symbolic-ref) printf 'main\\n';;\n  config)\n    case \"$4\" in\n      --get-all) printf '%s\\n' '{CANNED_URL}';;\n      --bool) printf '{bare}\\n';;\n      *) printf '/elsewhere\\n';;\n    esac;;\nesac\n",
                    log = log.display(),
                ),
            )
            .expect("legacy git shim");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
                .expect("legacy git mode");
            Self {
                scope,
                home,
                git_dir,
                log,
                shim,
            }
        }

        fn runtime(&self) -> crate::app::Runtime {
            let env = std::collections::BTreeMap::from([
                (OsString::from("HOME"), self.home.as_os_str().to_owned()),
                (
                    OsString::from("PATH"),
                    self.scope.path().as_os_str().to_owned(),
                ),
                (
                    OsString::from("DOT_SOURCE_ROOT"),
                    OsString::from(env!("CARGO_MANIFEST_DIR")),
                ),
            ]);
            crate::app::Runtime::from_env(&env, &self.home).expect("legacy runtime")
        }

        fn valid(&self, runtime: &crate::app::Runtime) -> bool {
            crate::init_client_identity::with_host_git(&self.shim, || {
                legacy_client_valid(runtime, &self.git_dir, &self.home.to_string_lossy())
            })
        }

        fn invocations(&self) -> usize {
            std::fs::read_to_string(&self.log)
                .map(|log| log.lines().count())
                .unwrap_or(0)
        }
    }

    #[test]
    fn repeated_legacy_validation_probes_git_once() {
        let _serial = TEST_SERIAL.lock();
        let git = LegacyGit::new("dedup", true);
        let runtime = git.runtime();
        assert!(git.valid(&runtime));
        let probes = git.invocations();
        assert_eq!(probes, 5);
        assert!(git.valid(&runtime));
        assert_eq!(git.invocations(), probes);
    }

    #[test]
    fn legacy_validation_invalidation_reprobes() {
        let _serial = TEST_SERIAL.lock();
        let git = LegacyGit::new("invalidate", true);
        let runtime = git.runtime();
        assert!(git.valid(&runtime));
        invalidate_client_match_cache();
        assert!(git.valid(&runtime));
        assert_eq!(git.invocations(), 10);
    }

    #[test]
    fn legacy_validation_reprobes_a_replaced_git_dir() {
        // The verdict is bound to the directory's device and inode, so a
        // directory swapped in under the same path is validated afresh.
        let _serial = TEST_SERIAL.lock();
        let git = LegacyGit::new("replaced", true);
        let runtime = git.runtime();
        assert!(git.valid(&runtime));
        let parked = git.home.join(".dotfiles-parked");
        std::fs::rename(&git.git_dir, &parked).expect("park git dir");
        std::fs::create_dir(&git.git_dir).expect("replacement git dir");
        assert!(git.valid(&runtime));
        assert_eq!(git.invocations(), 10);
    }

    #[test]
    fn legacy_validation_failures_stay_uncached() {
        let _serial = TEST_SERIAL.lock();
        let git = LegacyGit::new("foreign", false);
        let runtime = git.runtime();
        assert!(!git.valid(&runtime));
        let probes = git.invocations();
        assert_eq!(probes, 6);
        assert!(!git.valid(&runtime));
        assert_eq!(git.invocations(), probes * 2);
    }

    #[test]
    fn repeated_client_matches_probes_git_once() {
        let _serial = TEST_SERIAL.lock();
        let git = IdentityGit::matching("dedup");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(client_matches(&record, &git.home));
            assert!(client_matches(&record, &git.home));
        });
        assert_eq!(git.invocations(), 5);
    }

    #[test]
    fn client_matches_keys_homes_separately() {
        let _serial = TEST_SERIAL.lock();
        let first = IdentityGit::matching("first");
        let second = IdentityGit::matching("second");
        let first_record = first.record("main");
        let second_record = second.record("main");
        crate::init_client_identity::with_host_git(first.shim.as_path(), || {
            assert!(client_matches(&first_record, &first.home));
            assert!(client_matches(&first_record, &first.home));
        });
        crate::init_client_identity::with_host_git(second.shim.as_path(), || {
            assert!(client_matches(&second_record, &second.home));
            assert!(client_matches(&second_record, &second.home));
        });
        assert_eq!(first.invocations(), 5);
        assert_eq!(second.invocations(), 5);
    }

    #[test]
    fn client_match_invalidation_reprobes() {
        let _serial = TEST_SERIAL.lock();
        let git = IdentityGit::matching("invalidate");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(client_matches(&record, &git.home));
            assert_eq!(git.invocations(), 5);
            invalidate_client_match_cache();
            assert!(client_matches(&record, &git.home));
        });
        assert_eq!(git.invocations(), 10);
    }

    #[test]
    fn client_matches_mismatches_stay_uncached() {
        let _serial = TEST_SERIAL.lock();
        let git = IdentityGit::answering("mismatch", "other", "/elsewhere");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(!client_matches(&record, &git.home));
            assert!(!client_matches(&record, &git.home));
        });
        assert_eq!(git.invocations(), 4);
    }
}
