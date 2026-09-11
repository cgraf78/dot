//! Base client repository identity and command selection.
//!
//! The shared selector checks initialized-client records and legacy separate
//! Git directories before publishing a Base. Native doctor and test use this
//! same authority boundary; repository commands consume its topology model.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

fn legacy_client_valid(runtime: &crate::app::Runtime, git_dir: &Path, home: &str) -> bool {
    let meta = match std::fs::symlink_metadata(git_dir) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
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

fn client_matches(record: &crate::init_client_record::TransactionRecord, home: &Path) -> bool {
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
    let program = runtime.find_on_path("git")?;
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
