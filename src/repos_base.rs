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
            // The refusal stops every command, `dot doctor` included, so
            // this is the only place that can say what changed and how to
            // put it back. Classifying re-runs the probes, but only here.
            // A signal that interrupted the probes makes them read as a
            // mismatch; only a check that ran to the end has a reason.
            let classified = crate::cancellation::check()
                .is_ok()
                .then(|| client_mismatch(&record, Path::new(home)))
                .flatten()
                .filter(|_| crate::cancellation::check().is_ok());
            if let Some(why) = classified {
                let completed = completed_record.then_some(&completed);
                let lines = identity_recovery(runtime, &record, home, topology, completed, &why);
                // The reads that pick a step can be interrupted too; a step
                // chosen from an interrupted read is not printed.
                if crate::cancellation::check().is_ok() {
                    for line in lines {
                        let _ = writeln!(stderr, "dot: {line}");
                    }
                }
            }
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
/// phase; each validation is up to three supervised `git` probes
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
    // A renumbered device's verdict also depends on when the record was
    // written, so a record with the same fields but an older mtime does
    // not share it.
    let journaled = record
        .journaled
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos().to_string())
        .unwrap_or_default();
    key.extend_from_slice(journaled.as_bytes());
    key
}

/// Drop every memoized client-match verdict. Call after any engine
/// phase that may have moved the base checkout's branch or HEAD
/// (pull, staged clone into place) and after arbitrary user code
/// (hooks) or git passthrough. Link-only generation restores need
/// no call. Over-invalidation only costs a re-probe; a missed
/// invalidation would trust a replaced checkout.
pub(crate) fn invalidate_client_match_cache() {
    #[cfg(test)]
    let _gate = crate::memo::probe_cache_test_gate::shared();
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
    client_mismatch(record, home).is_none()
}

/// Why the live client no longer matches `record` (`None` when it does),
/// from the same check [`client_matches`] runs.
pub(crate) fn client_mismatch(
    record: &crate::init_client_record::TransactionRecord,
    home: &Path,
) -> Option<crate::init_client_resume::IdentityMismatch> {
    let git_dir = Path::new(&record.git_dir);
    let path_identity = |path: &Path| crate::persisted_identity::LiveIdentity::of(path);
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
        journaled: record.journaled,
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
    crate::init_client_resume::live_git_mismatch(&inputs, &deps)
}

/// Names Git itself keeps at the top of a Git directory: a branch named like
/// one of them must never be told to move that file away.
const GIT_DIR_FILES: [&str; 22] = [
    "HEAD",
    "ORIG_HEAD",
    "FETCH_HEAD",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_HEAD",
    "AUTO_MERGE",
    "config",
    "index",
    "description",
    "packed-refs",
    "shallow",
    "COMMIT_EDITMSG",
    "commondir",
    "gitdir",
    "objects",
    "refs",
    "logs",
    "hooks",
    "info",
    "worktrees",
];

/// Whether `path` (`$GIT_DIR/<branch>`) is a stray file Git reads as a ref,
/// shadowing the branch: not one of Git's own files, and holding exactly
/// what a loose ref holds, an object id or `ref: <name>`.
fn stray_ref_file(path: &Path, branch: &str) -> bool {
    if GIT_DIR_FILES.contains(&branch) || branch.ends_with("_HEAD") {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 512 {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let value = text.trim_end_matches('\n');
    let oid = matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    oid || value
        .strip_prefix("ref: ")
        .is_some_and(|name| name.starts_with("refs/"))
}

/// What changed and how to put it back, for a client the identity guard
/// refuses: one line naming the mismatch, then the recovery. Every step was
/// checked to restore a working client (or, for a replaced Git directory, to
/// re-establish its identity) without touching the files in `$HOME`.
/// `completed` is the record to move aside to re-adopt a replaced
/// directory, when that is the way back. The few extra Git reads that pick
/// between steps run only here, on a refusal.
fn identity_recovery(
    runtime: &crate::app::Runtime,
    record: &crate::init_client_record::TransactionRecord,
    home: &str,
    topology: &str,
    completed: Option<&PathBuf>,
    why: &crate::init_client_resume::IdentityMismatch,
) -> Vec<String> {
    use crate::init_client_resume::IdentityMismatch as Why;
    use crate::repos_pull_support::{quote_command, shell_quote};

    let quote = |text: &str| shell_quote(text.as_bytes());
    let git_dir = Path::new(&record.git_dir);
    let git = format!("git --git-dir={}", quote(&record.git_dir));
    // The recorded origin can carry credentials; no step prints them, and a
    // final line says where they go back.
    let origin = crate::redact::credentials(&record.origin);
    let run = |command: String| format!("to fix it, run {}", quote_command(&command));
    // The repository's own config: a global `remote.origin.*` key does not
    // make a remote exist for `git remote`.
    let has_key = |key: &str| {
        git_dir_output(runtime, git_dir, &["config", "--local", "--get", key]).is_some()
    };
    let has_ref = |name: &str| {
        git_dir_output(runtime, git_dir, &["show-ref", "--verify", "-q", name]).is_some()
    };
    let branch = quote(&record.branch);
    // `--` keeps a branch name that also names a tracked file from being
    // read as a path to check out (which would overwrite that file).
    let checkout = || {
        run(format!(
            "{git} --work-tree={} checkout {branch} --",
            quote(home)
        ))
    };
    let shown = &record.git_dir;
    // The recorded branch, so a client initialized with `--branch` comes
    // back on that branch rather than the remote's default.
    let init = crate::init_client_command::rerun_command(&record.origin, &record.branch);
    let mut lines = match why {
        Why::Branch(found) => {
            // Still on the branch when Git spells it `heads/<branch>`: another
            // ref with that name (a tag, or a stray file in the Git directory)
            // shadows it, and checking the branch out changes nothing. Name
            // and move the shadowing refs; without one, it is another branch.
            let mut shadows = Vec::new();
            if *found == format!("heads/{}", record.branch) {
                for kind in ["refs/tags/", "refs/"] {
                    let name = format!("{kind}{}", record.branch);
                    if !has_ref(&name) {
                        continue;
                    }
                    let target = (1..)
                        .map(|n| match n {
                            1 => format!("{name}-renamed"),
                            n => format!("{name}-renamed-{n}"),
                        })
                        .find(|candidate| !has_ref(candidate))
                        .unwrap_or_default();
                    shadows.push(format!(
                        "rename {name}: run {}",
                        quote_command(&format!(
                            "{git} update-ref {} {} && {git} update-ref --no-deref -d {}",
                            quote(&target),
                            quote(&name),
                            quote(&name)
                        ))
                    ));
                    if kind == "refs/tags/" {
                        // `dot update` fetches tags, so a tag origin also has
                        // comes back on the next update.
                        shadows.push(format!(
                            "if origin has that tag too, stop fetching tags: run {}",
                            quote_command(&format!("{git} config remote.origin.tagOpt --no-tags"))
                        ));
                    }
                }
                let stray = git_dir.join(&record.branch);
                if stray_ref_file(&stray, &record.branch) {
                    shadows.push(format!(
                        "move the stray file {} out of the Git directory",
                        stray.display()
                    ));
                }
            }
            if shadows.is_empty() {
                vec![
                    format!(
                        "{shown} is on branch '{found}', not '{}', the branch dot init recorded",
                        record.branch
                    ),
                    checkout(),
                ]
            } else {
                let mut lines = vec![format!(
                    "{shown} is on branch '{}', but another ref with that name shadows it",
                    record.branch
                )];
                lines.extend(shadows);
                lines
            }
        }
        Why::Head => vec![
            format!(
                "{shown} has a detached or unborn HEAD, not branch '{}'",
                record.branch
            ),
            format!(
                "if you made commits there, keep them first: run {}",
                quote_command(&format!("{git} branch NAME"))
            ),
            checkout(),
        ],
        Why::Worktree(found) => vec![
            match found {
                Some(found) => format!("{shown} has core.worktree {found}, not {home}"),
                None => format!("{shown} has no core.worktree; it must name {home}"),
            },
            run(format!("{git} config core.worktree {}", quote(home))),
        ],
        Why::Bare => vec![
            format!("{shown} has no valid core.bare setting"),
            run(format!("{git} config core.bare false")),
        ],
        Why::Config => vec![
            format!("{shown} has a Git config that cannot be read"),
            format!(
                "run {} to see the error, fix it, then rerun the command",
                quote_command(&format!("{git} config --list"))
            ),
        ],
        Why::OriginCount(0) => vec![
            format!("{shown} has no origin URL"),
            // A remote with any key left still exists: `remote add` refuses
            // it, and `set-url` needs it. A removed remote took its
            // tracking refs and the branch's upstream along, without which
            // `dot update` skips the client, so they come back too.
            // A remote left with only a stray key (`prune`) exists too, but
            // has no fetch refspec for `set-url` to use; removing it first
            // lets `remote add` write a whole one.
            if has_key("remote.origin.fetch") {
                run(format!("{git} remote set-url origin {}", quote(&origin)))
            } else {
                let stray = git_dir_output(
                    runtime,
                    git_dir,
                    &["config", "--local", "--get-regexp", r"^remote\.origin\."],
                )
                .is_some();
                run(format!(
                    "{}{git} remote add origin {} && {git} fetch origin && {git} branch --set-upstream-to=origin/{branch} {branch}",
                    if stray {
                        format!("{git} remote remove origin && ")
                    } else {
                        String::new()
                    },
                    quote(&origin)
                ))
            },
        ],
        Why::OriginCount(1) => vec![
            format!("{shown} has an origin URL that is not valid UTF-8"),
            run(format!("{git} remote set-url origin {}", quote(&origin))),
        ],
        Why::OriginCount(count) => vec![
            format!("{shown} has {count} origin URLs"),
            run(format!(
                "{git} config --replace-all remote.origin.url {}",
                quote(&origin)
            )),
        ],
        Why::OriginRepository(found) => vec![
            if found.is_empty() {
                format!("{shown} has an empty origin URL, not {origin}")
            } else {
                format!(
                    "{shown} has origin {}, not {origin}",
                    crate::redact::credentials(found)
                )
            },
            run(format!("{git} remote set-url origin {}", quote(&origin))),
        ],
        Why::TopLevel => vec![
            format!("{shown} no longer resolves its work tree to {home}"),
            if has_key("core.worktree") {
                run(format!("{git} config --unset core.worktree"))
            } else {
                // A bare `$HOME/.git` resolves no work tree at all.
                run(format!("{git} config core.bare false"))
            },
        ],
        Why::NotDirectory | Why::Replaced | Why::Generation => {
            let what = match why {
                Why::NotDirectory => format!("{shown} is missing or not a real directory"),
                Why::Replaced => format!("{shown} was replaced or recreated since dot init"),
                _ => format!("{shown} carries another dot init's generation marker"),
            };
            let mut lines = vec![what];
            if topology == "ordinary" && *why == Why::NotDirectory {
                // Without a real `$HOME/.git` there is nothing to adopt, and
                // `dot init` would clone a separate `~/.dotfiles` beside it.
                lines.push(format!(
                    "put the Git directory for {home} back at {shown}, then rerun the command"
                ));
            } else {
                // Re-adopting needs a real Git directory there, which a link
                // or file standing in for the separate layout is not.
                let adoptable = *why != Why::NotDirectory;
                let adopt = completed.filter(|_| adoptable).map(|completed| {
                    format!(
                        "if you replaced it yourself, move {} aside and run {init} to adopt it",
                        completed.display()
                    )
                });
                let adopting = adopt.is_some();
                lines.extend(adopt);
                // A fresh clone needs the separate layout: an ordinary
                // `$HOME/.git` record still demands the directory it names.
                if topology == "separate" {
                    lines.push(format!(
                        "{}move {shown} aside (it keeps any local commits) and run {init} for a fresh clone",
                        if adopting { "otherwise " } else { "" }
                    ));
                }
            }
            lines
        }
        Why::Commit | Why::Topology => vec![
            format!("{shown} failed an internal identity check"),
            "rerun the command; if it persists, report it as a dot bug".to_string(),
        ],
    };
    // Shell quoting escapes each `*`, so match the origin as the steps spell it.
    let quoted = quote(&origin);
    if origin != record.origin && lines.iter().any(|line| line.contains(&quoted)) {
        lines.push(
            "the origin's credentials are hidden (shown as \\*\\*\\*); put them back when you run it".to_string(),
        );
    }
    lines
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
    use std::os::unix::fs::MetadataExt;

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
            // The subcommand is matched anywhere: global options such as
            // `-c key=value` may sit between `--git-dir DIR` and it. The
            // readiness probe exits before logging, so counts stay exact; see
            // `dot_test_support::publish_fixture_script`.
            dot_test_support::publish_fixture_script(
                &shim,
                &format!(
                    "printf '%s\\n' \"$*\" >> {log}\ncase \" $* \" in\n  *\" config \"*) printf 'remote.origin.url\\n%s\\000core.bare\\nfalse\\000core.worktree\\n%s\\000' \"{CANNED_URL}\" \"{worktree}\";;\n  *\" rev-parse \"*) printf '%s\\n%s\\n' \"{CANNED_HEAD}\" \"{branch}\";;\nesac\n",
                    log = log.display(),
                ),
            )
            .expect("identity git shim");
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
                // Journaled now, after the fixture directory was created.
                journaled: Some(std::time::SystemTime::now()),
            }
        }
    }

    #[test]
    fn recovery_steps_never_print_origin_credentials_and_say_where_they_go() {
        let scope = dot_test_support::TempDir::new("identity-recovery-creds").expect("scope");
        let env = std::collections::BTreeMap::from([(
            std::ffi::OsString::from("HOME"),
            scope.path().as_os_str().to_owned(),
        )]);
        let runtime = crate::app::Runtime::from_env(&env, scope.path()).expect("runtime");
        let home = scope.path().to_str().expect("utf8");
        let record = crate::init_client_record::TransactionRecord {
            phase: "complete".to_string(),
            origin: "https://bot:s3cret@git.example/o/r.git".to_string(),
            identity: "git.example/o/r".to_string(),
            branch: "main".to_string(),
            commit: CANNED_HEAD.to_string(),
            git_dir: format!("{home}/.dotfiles"),
            worktree: home.to_string(),
            backup: "-".to_string(),
            dot: "/nonexistent".to_string(),
            dot_revision: CANNED_HEAD.to_string(),
            nonce: "adopted".to_string(),
            git_dev: "-".to_string(),
            git_ino: "-".to_string(),
            journaled: None,
        };
        let completed = scope.path().join("completed");
        for why in [
            crate::init_client_resume::IdentityMismatch::OriginRepository(
                "https://u:p@other.example/x.git".to_string(),
            ),
            crate::init_client_resume::IdentityMismatch::Replaced,
        ] {
            let lines = super::identity_recovery(
                &runtime,
                &record,
                home,
                "separate",
                Some(&completed),
                &why,
            );
            let text = lines.join("\n");
            assert!(!text.contains("s3cret") && !text.contains(":p@"), "{text}");
            // The note spells the redaction the way the quoted steps print it.
            assert!(
                text.contains("https://\\*\\*\\*@git.example/o/r.git"),
                "{text}"
            );
            assert!(
                text.ends_with(
                    "the origin's credentials are hidden (shown as \\*\\*\\*); put them back when you run it"
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn only_a_file_holding_a_ref_and_not_named_like_gits_own_is_stray() {
        let scope = dot_test_support::TempDir::new("stray-ref-file").expect("scope");
        let file = |name: &str, body: &str| {
            let path = scope.path().join(name);
            std::fs::write(&path, body).expect("write");
            path
        };
        let oid = "a".repeat(40);
        assert!(super::stray_ref_file(
            &file("main", &format!("{oid}\n")),
            "main"
        ));
        assert!(super::stray_ref_file(
            &file("work", "ref: refs/heads/main\n"),
            "work"
        ));
        assert!(!super::stray_ref_file(
            &file("notes", "not a ref\n"),
            "notes"
        ));
        // Git's own files never count, whatever they hold.
        assert!(!super::stray_ref_file(
            &file("config", &format!("{oid}\n")),
            "config"
        ));
        assert!(!super::stray_ref_file(
            &file("ORIG_HEAD", &format!("{oid}\n")),
            "ORIG_HEAD"
        ));
        assert!(!super::stray_ref_file(
            &scope.path().join("absent"),
            "absent"
        ));
    }

    #[test]
    fn an_empty_origin_url_is_named_as_empty() {
        let scope = dot_test_support::TempDir::new("identity-recovery-empty").expect("scope");
        let env = std::collections::BTreeMap::from([(
            std::ffi::OsString::from("HOME"),
            scope.path().as_os_str().to_owned(),
        )]);
        let runtime = crate::app::Runtime::from_env(&env, scope.path()).expect("runtime");
        let home = scope.path().to_str().expect("utf8");
        let record = crate::init_client_record::TransactionRecord {
            phase: "complete".to_string(),
            origin: "https://git.example/o/r.git".to_string(),
            identity: "git.example/o/r".to_string(),
            branch: "main".to_string(),
            commit: CANNED_HEAD.to_string(),
            git_dir: format!("{home}/.dotfiles"),
            worktree: home.to_string(),
            backup: "-".to_string(),
            dot: "/nonexistent".to_string(),
            dot_revision: CANNED_HEAD.to_string(),
            nonce: "adopted".to_string(),
            git_dev: "-".to_string(),
            git_ino: "-".to_string(),
            journaled: None,
        };
        let lines = super::identity_recovery(
            &runtime,
            &record,
            home,
            "separate",
            None,
            &crate::init_client_resume::IdentityMismatch::OriginRepository(String::new()),
        );
        assert!(
            lines[0].ends_with("has an empty origin URL, not https://git.example/o/r.git"),
            "{lines:?}"
        );
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
            dot_test_support::publish_fixture_script(
                &shim,
                &format!(
                    "printf '%s\\n' \"$*\" >> '{log}'\ncase \"$3\" in\n  rev-parse) printf '%s\\n' \"$2\";;\n  symbolic-ref) printf 'main\\n';;\n  config)\n    case \"$4\" in\n      --get-all) printf '%s\\n' '{CANNED_URL}';;\n      --bool) printf '{bare}\\n';;\n      *) printf '/elsewhere\\n';;\n    esac;;\nesac\n",
                    log = log.display(),
                ),
            )
            .expect("legacy git shim");
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
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
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
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
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
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
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
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
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
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
        let git = IdentityGit::matching("dedup");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(client_matches(&record, &git.home));
            assert!(client_matches(&record, &git.home));
        });
        // One config read plus one HEAD read.
        assert_eq!(git.invocations(), 2);
    }

    #[test]
    fn concurrent_global_clears_cannot_double_counted_client_probes() {
        // Regression: hook-worker tests on other threads clear the
        // process-global client-match cache through the production hook
        // boundary. A clearer racing the two matches must wait for the
        // counting test instead of forcing a re-probe.
        let _serial = TEST_SERIAL.lock();
        let cache_still = crate::memo::probe_cache_test_gate::exclusive();
        let git = IdentityGit::matching("concurrent-clear");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(client_matches(&record, &git.home));
        });
        // Launch the clear only once the cache is populated, and confirm it
        // reached the gate. Give an unguarded clear time to land before the
        // second match: a gated one cannot finish while the cache is held,
        // so this wait never affects the passing path.
        let (done, cleared) = std::sync::mpsc::channel();
        let clearer = std::thread::spawn(move || {
            invalidate_client_match_cache();
            let _ = done.send(());
        });
        assert!(
            crate::memo::probe_cache_test_gate::wait_for_arrival(
                clearer.thread().id(),
                std::time::Duration::from_secs(30)
            ),
            "client-match clear bypassed the probe cache gate"
        );
        let _ = cleared.recv_timeout(std::time::Duration::from_millis(100));
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(client_matches(&record, &git.home));
        });
        assert_eq!(git.invocations(), 2);
        drop(cache_still);
        cleared
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("client-match clear did not resume after release");
        clearer.join().expect("clearer thread");
    }

    #[test]
    fn client_matches_keys_homes_separately() {
        let _serial = TEST_SERIAL.lock();
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
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
        assert_eq!(first.invocations(), 2);
        assert_eq!(second.invocations(), 2);
    }

    #[test]
    fn client_match_invalidation_reprobes() {
        let _serial = TEST_SERIAL.lock();
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
        let git = IdentityGit::matching("invalidate");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(client_matches(&record, &git.home));
            assert_eq!(git.invocations(), 2);
            invalidate_client_match_cache();
            assert!(client_matches(&record, &git.home));
        });
        assert_eq!(git.invocations(), 4);
    }

    /// Whether this host reports birth times for the fixture's Git
    /// directory. Without one the exact device rule still applies, so the
    /// renumbering tests below have nothing to prove (Android, or a
    /// filesystem that stores no birth time).
    fn reports_birth_time(git: &IdentityGit) -> bool {
        let known = crate::persisted_identity::LiveIdentity::of(&git.home.join(".dotfiles"))
            .is_ok_and(|live| live.birth.is_some());
        if !known {
            eprintln!("skipping: no birth time on this host");
        }
        known
    }

    /// `record` as a process before a reboot journaled it: same inode, but
    /// the device number the mount had then.
    fn renumber(record: &mut crate::init_client_record::TransactionRecord) {
        let dev: u64 = record.git_dev.parse().expect("recorded device");
        record.git_dev = (dev + 1).to_string();
    }

    #[test]
    fn a_reboot_that_renumbers_the_device_keeps_the_client() {
        let _serial = TEST_SERIAL.lock();
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
        let git = IdentityGit::matching("renumbered");
        if !reports_birth_time(&git) {
            return;
        }
        let mut record = git.record("main");
        renumber(&mut record);
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert_eq!(client_mismatch(&record, &git.home), None);
            assert!(client_matches(&record, &git.home));
        });
    }

    #[test]
    fn a_git_dir_born_after_its_record_is_refused_on_a_renumbered_device() {
        let _serial = TEST_SERIAL.lock();
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
        let git = IdentityGit::matching("born-after-record");
        if !reports_birth_time(&git) {
            return;
        }
        let mut record = git.record("main");
        renumber(&mut record);
        let birth = crate::persisted_identity::LiveIdentity::of(&git.home.join(".dotfiles"))
            .expect("stat")
            .birth
            .expect("birth time");
        record.journaled = birth.checked_sub(std::time::Duration::from_secs(1));
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert_eq!(
                client_mismatch(&record, &git.home),
                Some(crate::init_client_resume::IdentityMismatch::Replaced)
            );
            assert!(!client_matches(&record, &git.home));
        });
    }

    #[test]
    fn client_matches_mismatches_stay_uncached() {
        let _serial = TEST_SERIAL.lock();
        let _cache_still = crate::memo::probe_cache_test_gate::exclusive();
        let git = IdentityGit::answering("mismatch", "other", "/elsewhere");
        let record = git.record("main");
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            assert!(!client_matches(&record, &git.home));
            assert!(!client_matches(&record, &git.home));
        });
        assert_eq!(git.invocations(), 4);
    }
}
