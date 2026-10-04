//! `_pull_repo` and `_pull_base` (`lib/dot/repos/pull.sh`): the
//! logged pull with conflict-backup retry, and the base
//! orchestrator built on it.
//!
//! The implementation stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::cleanup::Registry;
use crate::log::Log;
use crate::repos_base::Base;
use crate::repos_config::has_upstream;
use crate::repos_overlays::{DestinationInputs, QuarantineInputs};
use crate::repos_pull_backup::{BackupConflictsInputs, backup_pull_conflicts};
use crate::repos_pull_normalize::{normalize_updated_paths, snapshot_updated_path_parents};
use crate::repos_pull_queries::{
    CandidateEnv, accept_current_generation, repo_head, repo_head_is, validate_candidate_tree,
};
use crate::repos_pull_support::prepare_base_upstream;
use crate::run::logfile_create;
use crate::temp::{MoveCache, MoveTool, read_umask};

/// Inputs for [`pull_repo`], replacing the shell's backup-root plus
/// command argv with explicit values. The backup context mirrors
/// [`BackupConflictsInputs`] minus its log path, which the pull
/// allocates itself like `_logfile_create` does.
pub struct PullRepoInputs<'a> {
    /// Client `$HOME`: the backup parent.
    pub home: &'a str,
    /// Backup root holding the conflicting paths (`$1`).
    pub root: &'a str,
    /// Base checkout for the installed-link restore walk.
    pub base: &'a Base,
    /// Quarantine support (`None` backs everything as user data).
    pub quarantine: Option<QuarantineInputs>,
    /// Overlay records (`OVERLAYS`) for the restore walk.
    pub overlays: &'a [String],
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a DestinationInputs,
    /// Selected manifest (`$DOT_OVERLAY_MANIFEST`).
    pub manifest: &'a str,
    /// Legacy manifest (`$DOT_OVERLAY_LEGACY_MANIFEST`).
    pub legacy_manifest: &'a str,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root: &'a Path,
    /// Base for the legacy-hash throwaway repository.
    pub tmp: &'a Path,
    /// Probed move tool for the restore walk.
    pub tool: &'a MoveTool,
    /// Full pull command argv (git prefix plus pull arguments).
    pub command: &'a [OsString],
    /// Cron mode (`$DOT_QUIET`): append `--quiet`.
    pub quiet: bool,
    /// Verbose mode (`$DOT_VERBOSE`): show the log on success too.
    pub verbose: bool,
    /// Logger for the dim log dump and backup warnings.
    pub log: &'a Log,
}

/// Captured pull run into `log`, like `run_to_file` but with the
/// locale pinned per invocation: `_pull_cmd` sets `LC_ALL=C` around
/// every git run so the conflict detector and the quiet-output
/// filter match literal English, and the shared runner takes a bare
/// argv with inherited environment. Ticks stay with the worker
/// layer, which owns the progress stage; this leaf always runs
/// unticked, like the shell with no live UI.
fn run_pull_to_log(log: &Path, argv: &[OsString]) -> i32 {
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(log)
    {
        Ok(file) => file,
        Err(_) => return 127,
    };
    let stream = match file.try_clone() {
        Ok(stream) => stream,
        Err(_) => return 127,
    };
    let Some((program, args)) = argv.split_first() else {
        return 127;
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::from(stream));
    crate::cleanup::run_session_status(command, crate::cleanup::LingerPolicy::Detach)
}

/// Streaming pull without a log, like the `_logfile_create`
/// fallback running `_pull_cmd`: inherited stdio, pinned locale,
/// exit code (127 when spawning fails).
fn run_streaming(argv: &[OsString]) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        return 127;
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    crate::cleanup::run_foreground_status(command)
}

/// Whether a pull-log line is filtered from the visible dump, like
/// the `sed` deletions for `Already up to date.` and `Current
/// branch ... is up to date.`.
fn is_up_to_date_noise(line: &str) -> bool {
    line == "Already up to date."
        || (line.starts_with("Current branch ") && line.ends_with(" is up to date."))
}

/// `_pull_repo`: run the pull command into a log, back conflicting
/// untracked files up and retry once on failure, dim the visible
/// remainder when loud, and remove the log. Returns the pull exit
/// code. The dim dump goes to `out` (the shell's stdout) and backup
/// warnings to `warnings` (its stderr).
pub fn pull_repo(
    inputs: &PullRepoInputs<'_>,
    moves: &mut MoveCache,
    out: &mut dyn Write,
    warnings: &mut dyn Write,
) -> i32 {
    let mut argv: Vec<OsString> = inputs.command.to_vec();
    if inputs.quiet {
        argv.push(OsString::from("--quiet"));
    }
    let Some(log) = logfile_create() else {
        return run_streaming(&argv);
    };
    let mut rc = run_pull_to_log(&log, &argv);
    if crate::cleanup::received_signal().is_some() {
        let mut cleanup = Registry::new();
        let _ = cleanup.remove_path(&log);
        return rc;
    }
    if rc != 0 {
        let backup = BackupConflictsInputs {
            home: inputs.home,
            root: inputs.root,
            pull_log: &log,
            base: inputs.base,
            quarantine: inputs.quarantine.clone(),
            overlays: inputs.overlays,
            dest: inputs.dest,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            euid: inputs.euid,
            source_root: inputs.source_root,
            tmp: inputs.tmp,
            log: inputs.log,
            tool: inputs.tool,
        };
        if backup_pull_conflicts(&backup, moves, warnings).succeeded {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&log);
            rc = run_pull_to_log(&log, &argv);
        }
    }
    let loud = !inputs.quiet
        && std::fs::metadata(&log).is_ok_and(|meta| meta.len() > 0)
        && (inputs.verbose || rc != 0);
    if loud {
        if let Ok(content) = std::fs::read_to_string(&log) {
            // Git's own lines, indented under the Repos stage like any child
            // output (empty lines stay empty).
            let indent = String::from_utf8_lossy(crate::progress_ui::CHILD_OUTPUT_INDENT);
            let visible: Vec<String> = content
                .lines()
                .filter(|line| !is_up_to_date_noise(line))
                .map(|line| {
                    if line.is_empty() {
                        String::new()
                    } else {
                        format!("{indent}{line}")
                    }
                })
                .collect();
            if !visible.is_empty() {
                inputs.log.dim(out, &visible.join("\n"));
            }
        }
    }
    let mut cleanup = Registry::new();
    let _ = cleanup.remove_path(&log);
    rc
}

/// `-c` override for every rebase and `rebase --abort` dot runs.
/// Replaying already-committed local commits onto upstream is not a
/// new commit, but git still runs `prepare-commit-msg` (and
/// `pre-rebase`, `post-rewrite`, `post-checkout`) for it. A commit
/// gate selected through a global `core.hooksPath` then fails the
/// unattended pull, and every cycle checks upstream out in the work
/// tree only to hard-reset it. dot installs no hooks and its pull has
/// no hook dependency: none is needed to converge a checkout on
/// upstream (LFS content arrives through filters, which still run).
/// The override also skips repo-local hooks, such as a user's
/// `pre-rebase` guard, for dot's own rebase only.
const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// Full argv for the unattended `rebase --autostash` onto the
/// resolved `upstream` tip, with hooks disabled. `prefix` carries
/// the topology flags (`--git-dir=`/`--work-tree=` or `-C <path>`).
pub(crate) fn rebase_command(
    prefix: &[OsString],
    upstream: &str,
    extra_args: &[OsString],
) -> Vec<OsString> {
    let mut command = vec![crate::init_client_identity::host_git_program()];
    command.extend(NO_HOOKS.iter().map(OsString::from));
    command.extend(prefix.iter().cloned());
    command.push(OsString::from("rebase"));
    command.push(OsString::from("--autostash"));
    command.push(OsString::from(upstream));
    command.extend(extra_args.iter().cloned());
    command
}

/// `git rebase --abort` under `prefix` with hooks disabled, so the
/// same hooks that could fail the rebase cannot fail its cleanup.
fn abort_rebase(prefix: &[OsString]) {
    let mut hookless: Vec<OsString> = NO_HOOKS.iter().map(OsString::from).collect();
    hookless.extend(prefix.iter().cloned());
    let _ = crate::repos_base::run_git(&hookless, &["rebase", "--abort"]);
}

/// A copy-pasteable `git` command for this checkout, for warnings.
/// Agents and users otherwise run a bare `git rebase --abort` in the
/// wrong directory; the base needs its separate git dir and work
/// tree, which `prefix` already spells out.
pub(crate) fn git_hint(prefix: &[OsString], args: &str) -> String {
    let mut words = vec!["git".to_string()];
    words.extend(
        prefix
            .iter()
            .map(|word| crate::repos_pull_support::shell_quote(word.as_bytes())),
    );
    format!("`{} {args}`", words.join(" "))
}

/// Why a checkout has no `@{u}` although it is not a deliberate
/// no-upstream branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrandedHead {
    /// dot's own rebase, interrupted (killed or signalled) before it
    /// could finish or abort, was aborted now: the checkout is back
    /// on its branch and the pull continues.
    Healed,
    /// dot's own interrupted rebase was left in progress (`--abort`
    /// failed, or would discard changes): its mid-rebase files are
    /// live, so the pull fails.
    Unhealed,
    /// A merge, rebase, or `git am` session dot did not start (paused
    /// at a conflict, `edit`, or `break`): the user's work in
    /// progress, so the pull warns and skips with rc 0.
    UserSession,
    /// HEAD is detached with no session in progress (a bisect or a
    /// pinned commit): warns and skips with rc 0.
    Detached,
}

/// A classified stranded checkout plus what its cron warning
/// throttle needs.
pub(crate) struct Stranded {
    /// The classification.
    pub(crate) kind: StrandedHead,
    /// Per-worktree git dir (`None` when unresolvable).
    git_dir: Option<PathBuf>,
    /// State identity for the once-per-state cron warning.
    key: String,
}

impl Stranded {
    /// Classification plus its cron throttle key: the kind and the
    /// current HEAD, so a new commit or a different state warns again.
    fn new(kind: StrandedHead, git_dir: Option<PathBuf>, prefix: &[OsString]) -> Self {
        let key = format!(
            "{kind:?} {}\n",
            crate::repos_pull_queries::repo_head(prefix)
        );
        Self { kind, git_dir, key }
    }

    /// Warning tail naming the state, with a command for this
    /// checkout only where dot owns the fix.
    pub(crate) fn describe(&self, prefix: &[OsString]) -> String {
        match self.kind {
            StrandedHead::Healed => {
                "had an interrupted dot rebase in progress; it was aborted and the checkout restored"
                    .to_string()
            }
            StrandedHead::Unhealed => unaborted(prefix),
            StrandedHead::UserSession => format!(
                "has a merge, cherry-pick, revert, rebase, or `git am` in progress that dot did not start (see {}); pulls are skipped until it finishes",
                git_hint(prefix, "status")
            ),
            StrandedHead::Detached => {
                "has a detached HEAD; pulls are skipped until it is back on a branch".to_string()
            }
        }
    }

    /// Whether this state fails the pull. A user session and a
    /// detached HEAD skip with rc 0 (optional overlays included), so
    /// the user's own work never fails the update or freezes linking,
    /// hooks, and the provider every cycle.
    pub(crate) fn fails(&self) -> bool {
        self.kind == StrandedHead::Unhealed
    }

    /// Whether to print the warning now. dot-owned states always
    /// warn; the user's own states go through [`throttled`], keyed by
    /// kind and HEAD.
    pub(crate) fn warn_now(&self, quiet: bool) -> bool {
        if matches!(self.kind, StrandedHead::Healed | StrandedHead::Unhealed) {
            return true;
        }
        throttled(self.git_dir.as_deref(), &self.key, quiet)
    }
}

/// Once-per-state gate for warnings about states the user must fix:
/// cron (`quiet`) runs every 30 minutes and mails stderr, so it
/// prints only when `key` differs from the last warned state in
/// [`WARNED_MARKER`]; interactive runs always print. Any printed
/// warning records the key, so an interactive one also covers cron.
fn throttled(git_dir: Option<&Path>, key: &str, quiet: bool) -> bool {
    let Some(dir) = git_dir else {
        return true;
    };
    let path = dir.join(WARNED_MARKER);
    let seen = std::fs::read_to_string(&path).is_ok_and(|text| text == key);
    if !seen {
        // Best effort: without the record, cron simply warns again.
        let _ = std::fs::write(&path, key);
    }
    !(quiet && seen)
}

/// Warning tail for dot's own rebase left in progress (`--abort`
/// failed, a signal stopped dot first, or aborting would discard
/// changes). Its in-flight record stays, so later runs retry the
/// abort while it discards nothing. Only here may dot name the abort
/// command: the rebase is its own.
pub(crate) fn unaborted(prefix: &[OsString]) -> String {
    format!(
        "has a dot rebase in progress that was not aborted; run {} (dot retries the abort itself only while the checkout has no uncommitted changes)",
        git_hint(prefix, "rebase --abort")
    )
}

/// Absolute per-worktree git dir under `prefix` (where rebase state
/// lives, including for linked worktrees), or `None` when git fails.
fn absolute_git_dir(prefix: &[OsString]) -> Option<PathBuf> {
    let output = crate::repos_base::run_git(prefix, &["rev-parse", "--absolute-git-dir"])?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let dir = text.trim_end_matches('\n');
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// Whether `git_dir` holds rebase state, like git's own status
/// probe: `rebase-merge`, or `rebase-apply` without the `applying`
/// marker that `git am` leaves (an am session is not ours to abort).
fn rebase_state_exists(git_dir: &Path) -> bool {
    let apply = git_dir.join("rebase-apply");
    git_dir.join("rebase-merge").symlink_metadata().is_ok()
        || (apply.symlink_metadata().is_ok() && apply.join("applying").symlink_metadata().is_err())
}

/// Whether `git_dir` holds any operation the user is in the middle
/// of: a rebase, `git am`, merge, cherry-pick, or revert. dot must not
/// rebase over one: a resolved but uncommitted merge has nothing
/// unmerged, yet `rebase --autostash` would drop `MERGE_HEAD` and with
/// it the merge's second parent.
fn session_exists(git_dir: &Path) -> bool {
    [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "sequencer",
    ]
    .iter()
    .any(|entry| git_dir.join(entry).symlink_metadata().is_ok())
}

/// The rebase state directory replaying exactly `head` onto
/// `upstream` (its `orig-head` and `onto`), if any.
fn state_replaying(dir: &Path, head: &str, upstream: &str) -> Option<PathBuf> {
    let read = |state: &Path, leaf: &str| {
        std::fs::read_to_string(state.join(leaf))
            .map(|text| text.trim().to_string())
            .unwrap_or_default()
    };
    ["rebase-merge", "rebase-apply"]
        .iter()
        .map(|state| dir.join(state))
        .find(|state| read(state, "orig-head") == head && read(state, "onto") == upstream)
}

/// Longest plausible gap between writing [`INFLIGHT_MARKER`] and git
/// writing the rebase's `orig-head` (the autostash comes first and
/// can take a while over a large `$HOME` work tree). The small
/// allowance before the record covers coarse filesystem timestamps.
const INFLIGHT_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
const INFLIGHT_SLACK: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the in-progress rebase is the one dot recorded in
/// [`INFLIGHT_MARKER`] before running it (and never cleared because
/// dot was killed or signalled). The pair must match exactly, and the
/// rebase must have started right after the record was written: a
/// stale record (dot killed outside the rebase, or its rebase aborted
/// by hand) then never claims a later rebase the user starts over the
/// same pair. `git am` state is never dot's.
fn interrupted_by_dot(dir: &Path) -> bool {
    let marker = dir.join(INFLIGHT_MARKER);
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return false;
    };
    let mut words = text.split_whitespace();
    let (Some(head), Some(upstream), None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    if !rebase_state_exists(dir) {
        return false;
    }
    let Some(state) = state_replaying(dir, head, upstream) else {
        return false;
    };
    let modified = |path: &Path| path.symlink_metadata().and_then(|meta| meta.modified());
    match (modified(&marker), modified(&state.join("orig-head"))) {
        (Ok(recorded), Ok(started)) => match started.duration_since(recorded) {
            Ok(gap) => gap <= INFLIGHT_WINDOW,
            Err(early) => early.duration() <= INFLIGHT_SLACK,
        },
        _ => false,
    }
}

/// Whether the work tree and index match HEAD. Healing runs
/// `rebase --abort`, which hard-resets: it only runs when that
/// discards nothing, so a user who took over dot's paused rebase and
/// started resolving it keeps that work. Failures read as dirty.
fn checkout_clean(prefix: &[OsString]) -> bool {
    crate::repos_base::run_git(prefix, &["diff", "--quiet", "HEAD", "--"])
        .is_some_and(|output| output.status.success())
}

/// Abort dot's own interrupted rebase when that discards nothing.
fn heal(dir: &Path, prefix: &[OsString]) -> StrandedHead {
    // A latched signal would fail the supervised spawn; leave the
    // state (and its record) for the next run.
    if crate::cleanup::received_signal().is_some() || !checkout_clean(prefix) {
        return StrandedHead::Unhealed;
    }
    abort_rebase(prefix);
    if rebase_state_exists(dir) {
        return StrandedHead::Unhealed;
    }
    let _ = std::fs::remove_file(dir.join(INFLIGHT_MARKER));
    StrandedHead::Healed
}

/// What a checkout without a branch to pull is in the middle of, as
/// [`stranded_head`] would classify it, read-only: no healing, no marker
/// cleanup, no git process (doctor runs it on every report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interruption {
    /// dot's own rebase, interrupted before it could finish or abort.
    /// Update aborts it when that discards nothing and fails otherwise.
    DotRebase,
    /// A merge, rebase, cherry-pick, revert, or `git am` dot did not
    /// start. Update warns and skips the checkout.
    UserSession,
}

/// Read-only twin of [`stranded_head`]'s classification for the
/// per-worktree git dir `git_dir`; `None` when nothing is in progress
/// (a plain detached HEAD, if HEAD has no branch).
pub(crate) fn interruption(git_dir: &Path) -> Option<Interruption> {
    if interrupted_by_dot(git_dir) {
        Some(Interruption::DotRebase)
    } else if session_exists(git_dir) {
        Some(Interruption::UserSession)
    } else {
        None
    }
}

/// Classify a checkout whose `@{u}` probe failed, aborting dot's own
/// interrupted rebase on the way when that discards nothing. Only
/// runs on that already-unusual path, so healthy pulls pay no extra
/// subprocess. Git failures fall back to `None` (the historical
/// silent skip).
pub(crate) fn stranded_head(prefix: &[OsString]) -> Option<Stranded> {
    let git_dir = absolute_git_dir(prefix);
    let found = git_dir.as_deref().map(|dir| (dir, interruption(dir)));
    let kind = match found {
        Some((dir, Some(Interruption::DotRebase))) => heal(dir, prefix),
        other => {
            // Whatever is in progress is not dot's: an in-flight
            // record here is stale and must never claim a later one.
            if let Some((dir, _)) = other {
                let _ = std::fs::remove_file(dir.join(INFLIGHT_MARKER));
            }
            if matches!(other, Some((_, Some(Interruption::UserSession)))) {
                StrandedHead::UserSession
            } else {
                // `symbolic-ref -q` exits 1 exactly for a detached
                // HEAD; any other failure is not evidence either way.
                let output = crate::repos_base::run_git(prefix, &["symbolic-ref", "-q", "HEAD"])?;
                if output.status.code() != Some(1) {
                    return None;
                }
                StrandedHead::Detached
            }
        }
    };
    Some(Stranded::new(kind, git_dir, prefix))
}

/// Warning tail for a pull skipped by [`RebaseGuard::repeats_failure`].
pub(crate) fn repeated_failure(prefix: &[OsString]) -> String {
    format!(
        "failed to rebase onto its upstream on an earlier run and the rebase was aborted; it retries once either side moves, or rebase it yourself with {}",
        git_hint(prefix, "pull --rebase")
    )
}

/// Unmerged index entries: conflict markers in live files. Either the
/// autostash of the rebase that just succeeded did not reapply
/// cleanly (the edit stays in the stash; git still exits 0), or they
/// were already there before this run (that same leftover, or a merge
/// the user is resolving). A pull on top of them would fail at best
/// and read the markers as `current` at worst, so it fails instead.
pub(crate) struct Unmerged {
    /// Unmerged paths, relative to the work tree.
    paths: Vec<String>,
    /// Per-worktree git dir for the cron throttle.
    git_dir: Option<PathBuf>,
    /// State identity for the once-per-state cron warning.
    key: String,
    /// The autostash of this run's rebase produced them.
    after_autostash: bool,
}

impl Unmerged {
    fn new(paths: Vec<String>, prefix: &[OsString], after_autostash: bool) -> Self {
        let key = format!(
            "Unmerged {} {}\n",
            crate::repos_pull_queries::repo_head(prefix),
            paths.join("\0")
        );
        Self {
            paths,
            git_dir: absolute_git_dir(prefix),
            key,
            after_autostash,
        }
    }

    /// Warning tail listing the files and where the edit went.
    pub(crate) fn describe(&self, prefix: &[OsString]) -> String {
        let stash = git_hint(prefix, "stash list");
        let files = self.paths.join(", ");
        if self.after_autostash {
            format!(
                "was updated, but uncommitted changes conflicted with the update and are kept in the stash (see {stash}); conflict markers are in: {files}"
            )
        } else {
            format!(
                "has unmerged paths with conflict markers in live files: {files}; pulls stay stopped until they are resolved (after an autostash conflict the uncommitted changes are kept in the stash, see {stash})"
            )
        }
    }

    /// Whether to print the warning now (once per state under cron;
    /// the status stays failed either way).
    pub(crate) fn warn_now(&self, quiet: bool) -> bool {
        throttled(self.git_dir.as_deref(), &self.key, quiet)
    }
}

/// What the index allows before any pull work starts.
pub(crate) enum IndexCheck {
    /// No unmerged entries.
    Clean,
    /// Unmerged entries inside a merge, rebase, or `git am` session
    /// the user started while HEAD kept its upstream: warn and skip
    /// like any other user session.
    Session(Stranded),
    /// Unmerged entries outside any session: fail without pulling.
    Unmerged(Unmerged),
}

/// Check the index before fetching or rebasing. One index-only git
/// call on every pull (`ls-files -u` reads no work tree); the git dir
/// is resolved only when something is unmerged.
pub(crate) fn check_index(prefix: &[OsString]) -> IndexCheck {
    let paths = unmerged_paths(prefix);
    if paths.is_empty() {
        return IndexCheck::Clean;
    }
    let git_dir = absolute_git_dir(prefix);
    if git_dir.as_deref().is_some_and(session_exists) {
        return IndexCheck::Session(Stranded::new(StrandedHead::UserSession, git_dir, prefix));
    }
    IndexCheck::Unmerged(Unmerged::new(paths, prefix, false))
}

/// Unmerged entries left by a rebase git reported as successful:
/// `rebase --autostash` exits 0 when reapplying the autostash
/// conflicts, which must not read as a clean update.
pub(crate) fn unmerged_after_pull(prefix: &[OsString]) -> Option<Unmerged> {
    let paths = unmerged_paths(prefix);
    (!paths.is_empty()).then(|| Unmerged::new(paths, prefix, true))
}

/// Unmerged index paths (each once, in index order). Index-only, so
/// cheap even for the `$HOME` work tree. A failed probe reads as none
/// (the historical result).
pub(crate) fn unmerged_paths(prefix: &[OsString]) -> Vec<String> {
    let Some(output) = crate::repos_base::run_git(prefix, &["ls-files", "--unmerged", "-z"])
        .filter(|output| output.status.success())
    else {
        return Vec::new();
    };
    let mut paths: Vec<String> = Vec::new();
    // Records are `mode object stage<TAB>path`, one per stage.
    for record in output.stdout.split(|byte| *byte == 0) {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
        if paths.last() != Some(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Marker in the per-worktree git dir recording the `HEAD upstream`
/// pair whose rebase failed and was aborted: the bare pair freezes
/// it, the pair plus [`RETRY_STRIKE`] allows one more attempt. Git
/// ignores unknown files there; nothing but [`RebaseGuard`] reads it.
const FAILED_MARKER: &str = "dot-rebase-failed";

/// Third word of a [`FAILED_MARKER`] recording a first non-conflict
/// failure.
const RETRY_STRIKE: &str = "retry";

/// The HEAD whose rebase is frozen in the per-worktree `git_dir`: the
/// [`FAILED_MARKER`] holds a bare `HEAD upstream` pair (no
/// [`RETRY_STRIKE`]), which [`RebaseGuard::repeats_failure`] refuses to
/// retry while that HEAD and upstream tip stay put. `None` without a
/// marker or for a one-strike record. One read; `dot doctor` reports it.
pub(crate) fn frozen_rebase_head(git_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir.join(FAILED_MARKER)).ok()?;
    let line = text.strip_suffix('\n')?;
    let mut words = line.split(' ');
    let (head, _upstream) = (words.next()?, words.next()?);
    words.next().is_none().then(|| head.to_string())
}

/// Marker recording the `HEAD upstream` pair of the rebase dot is
/// about to run, removed once the pull and any abort are done. A
/// rebase left in progress that matches it is dot's own, interrupted
/// by a kill or signal, and is safe for dot to abort.
const INFLIGHT_MARKER: &str = "dot-rebase-inflight";

/// Marker recording the last stranded state warned about, so cron
/// warns once per state (see [`Stranded::warn_now`]).
const WARNED_MARKER: &str = "dot-stranded-warned";

/// Rebase bookkeeping around the `rebase --autostash` that
/// [`pull_repo`] runs: the state observed before it (so a failure
/// only aborts a rebase this run started) and the pair it replays.
pub(crate) struct RebaseGuard {
    /// Per-worktree git dir, or `None` when unresolvable (no abort,
    /// no failure memory).
    git_dir: Option<PathBuf>,
    /// False when a merge, rebase, or `git am` was already in
    /// progress: that session belongs to the user and is never
    /// aborted or rebased over.
    armed: bool,
    /// HEAD the rebase replays (its `orig-head`).
    head: String,
    /// Resolved upstream tip it replays onto (its `onto`).
    upstream: String,
}

impl RebaseGuard {
    /// Record the pre-pull state for replaying `head` onto the
    /// resolved `upstream` tip. With no session in progress, a stale
    /// in-flight or warned record is meaningless and is dropped.
    pub(crate) fn arm(prefix: &[OsString], head: &str, upstream: &str) -> Self {
        let git_dir = absolute_git_dir(prefix);
        let armed = git_dir.as_deref().is_some_and(|dir| !session_exists(dir));
        if armed {
            if let Some(dir) = git_dir.as_deref() {
                let _ = std::fs::remove_file(dir.join(INFLIGHT_MARKER));
                let _ = std::fs::remove_file(dir.join(WARNED_MARKER));
            }
        }
        Self {
            git_dir,
            armed,
            head: head.to_string(),
            upstream: upstream.to_string(),
        }
    }

    /// A merge, rebase, or `git am` session the user started while
    /// HEAD kept its upstream (a merge or am stays on the branch).
    /// Rebasing over it would fail every cycle, or silently drop an
    /// uncommitted merge, so the caller warns and skips like any
    /// other user session.
    pub(crate) fn user_session(&self, prefix: &[OsString]) -> Option<Stranded> {
        let dir = self.git_dir.as_deref().filter(|_| !self.armed)?;
        Some(Stranded::new(
            StrandedHead::UserSession,
            Some(dir.to_path_buf()),
            prefix,
        ))
    }

    /// Marker body for this attempt.
    fn pair(&self) -> String {
        format!("{} {}\n", self.head, self.upstream)
    }

    /// Whether this exact `HEAD upstream` pair is frozen: it
    /// conflicted, or failed twice, and was aborted on earlier runs.
    /// Retrying it swaps tracked files (in `$HOME`, for the base)
    /// under a running session and then hard-resets them on abort, so
    /// a save landing in that window would be lost, and a failing
    /// environment (a conflict, a broken signing setup, a stale lock)
    /// would do so every cycle. The caller reports failure without
    /// touching the work tree until the user rebases, or either side
    /// moves.
    pub(crate) fn repeats_failure(&self) -> bool {
        self.git_dir.as_deref().is_some_and(|dir| {
            std::fs::read_to_string(dir.join(FAILED_MARKER)).is_ok_and(|text| text == self.pair())
        })
    }

    /// Record this attempt as in flight just before the rebase runs,
    /// so a later run can recognize and heal it if dot dies mid-way.
    /// Best effort: without the record, an interrupted rebase reads
    /// as the user's and is only warned about, never aborted.
    pub(crate) fn begin(&self) {
        if let Some(dir) = self.git_dir.as_deref().filter(|_| self.armed) {
            let _ = std::fs::write(dir.join(INFLIGHT_MARKER), self.pair());
        }
    }

    /// Drop the in-flight record once no rebase of this attempt can
    /// remain in progress.
    fn finish(&self, dir: &Path) {
        let _ = std::fs::remove_file(dir.join(INFLIGHT_MARKER));
    }

    /// After a failed pull, abort the rebase it left behind. A
    /// failed `rebase --autostash` otherwise strands the checkout
    /// with conflict markers or mid-rebase files live and a detached
    /// HEAD. `--abort` restores the pre-rebase HEAD and reapplies the
    /// autostash; if that reapply conflicts, git keeps the changes in
    /// the stash list, so no user edit is lost. Every abort records
    /// the pair for [`Self::repeats_failure`], whatever stopped the
    /// rebase. Returns false only when dot's own rebase remains in
    /// progress (its in-flight record stays, so the next run retries
    /// the abort through [`stranded_head`]); an unresolvable git dir,
    /// a failure that never started a rebase, or a session that is
    /// not this attempt's returns true.
    pub(crate) fn abort_failed(&self, prefix: &[OsString]) -> bool {
        let Some(dir) = self.git_dir.as_deref().filter(|_| self.armed) else {
            return true;
        };
        if !rebase_state_exists(dir) {
            self.finish(dir);
            return true;
        }
        // A rebase the user started in the window after `arm` replays
        // a different pair: not ours to abort or to report.
        if state_replaying(dir, &self.head, &self.upstream).is_none() {
            self.finish(dir);
            return true;
        }
        // A latched signal would fail the supervised spawn anyway:
        // leave the state and its record for the next run to heal.
        if crate::cleanup::received_signal().is_some() {
            return false;
        }
        let conflicted = !unmerged_paths(prefix).is_empty();
        abort_rebase(prefix);
        if rebase_state_exists(dir) {
            return false;
        }
        // A conflict is deterministic: freeze the pair at once. Any
        // other failure (a signing setup, a lock held by a prompt's
        // `git status`) may be transient, so it earns one retry on the
        // next run and freezes only if the same pair fails again.
        // Either way a failing environment never checks out and
        // hard-resets the work tree every cycle. Best effort: without
        // the marker the next run retries.
        let strike = format!("{} {} {RETRY_STRIKE}\n", self.head, self.upstream);
        let second = std::fs::read_to_string(dir.join(FAILED_MARKER)).is_ok_and(|t| t == strike);
        let record = if conflicted || second {
            self.pair()
        } else {
            strike
        };
        let _ = std::fs::write(dir.join(FAILED_MARKER), record);
        self.finish(dir);
        true
    }

    /// After a successful pull, forget any recorded failure and the
    /// in-flight record.
    pub(crate) fn succeeded(&self) {
        if let Some(dir) = self.git_dir.as_deref() {
            let _ = std::fs::remove_file(dir.join(FAILED_MARKER));
            self.finish(dir);
        }
    }
}

/// `_pull_base` outcome status (`REPLY_STATUS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullStatus {
    /// No upstream tracks the checkout (a branch without tracking),
    /// or a warned user session or detached HEAD (dot's own rebase
    /// that cannot be aborted reads as `Failed`).
    Skipped,
    /// The checkout already matches upstream.
    Current,
    /// The pull moved the checkout.
    Changed,
    /// Anything else went wrong.
    Failed,
}

impl PullStatus {
    /// The `REPLY_STATUS` word.
    pub fn as_str(&self) -> &'static str {
        match self {
            PullStatus::Skipped => "skipped",
            PullStatus::Current => "current",
            PullStatus::Changed => "changed",
            PullStatus::Failed => "failed",
        }
    }
}

/// Outcome of [`pull_base`]: the status plus the shell exit code (0
/// except for `Failed`).
pub struct PullBaseOutcome {
    /// The `REPLY_STATUS` decision.
    pub status: PullStatus,
    /// The shell return code.
    pub rc: i32,
    /// Why a `Skipped` pull skipped (`None` otherwise).
    pub skip: Option<SkipReason>,
}

/// Why [`pull_base`] skipped, for the verbose status row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A branch without tracking (or no base checkout).
    NoUpstream,
    /// A merge, cherry-pick, revert, rebase, or `git am` the user
    /// started is in progress.
    Session,
    /// HEAD is detached.
    Detached,
}

impl SkipReason {
    /// Parenthesized detail for `dotfiles pull skipped (...)`.
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::NoUpstream => "no upstream",
            SkipReason::Session => "merge, cherry-pick, revert, rebase, or git am in progress",
            SkipReason::Detached => "detached HEAD",
        }
    }
}

impl StrandedHead {
    /// The skip reason for a non-failing stranded state that does not
    /// continue into a pull.
    fn skip_reason(self) -> SkipReason {
        match self {
            StrandedHead::UserSession => SkipReason::Session,
            StrandedHead::Detached => SkipReason::Detached,
            StrandedHead::Healed | StrandedHead::Unhealed => SkipReason::NoUpstream,
        }
    }
}

/// Inputs for [`pull_base`]: the base checkout, the candidate
/// environment for validation, the backup context, and the pull
/// flags. Extra git pull arguments ride `extra_args`.
pub struct PullBaseInputs<'a> {
    /// Base checkout (home is its work tree).
    pub base: &'a Base,
    /// Candidate validation environment.
    pub candidate: &'a CandidateEnv,
    /// Quarantine support (`None` backs everything as user data).
    pub quarantine: Option<QuarantineInputs>,
    /// Overlay records (`OVERLAYS`) for the restore walk.
    pub overlays: &'a [String],
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a DestinationInputs,
    /// Selected manifest (`$DOT_OVERLAY_MANIFEST`).
    pub manifest: &'a str,
    /// Legacy manifest (`$DOT_OVERLAY_LEGACY_MANIFEST`).
    pub legacy_manifest: &'a str,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root: &'a Path,
    /// Base for the legacy-hash throwaway repository.
    pub tmp: &'a Path,
    /// Probed move tool for the restore walk.
    pub tool: &'a MoveTool,
    /// Extra git pull arguments after the upstream.
    pub extra_args: &'a [OsString],
    /// Cron mode (`$DOT_QUIET`).
    pub quiet: bool,
    /// Verbose mode (`$DOT_VERBOSE`).
    pub verbose: bool,
    /// Logger for the dim log dump and backup warnings.
    pub log: &'a Log,
    /// Live rows reach a real terminal: the fetch may prompt the user, so
    /// its stderr is a pseudo-terminal (see `prepare_base_upstream`).
    pub terminal: bool,
}

/// `_pull_base`: fetch the upstream, fast-path the current
/// generation, validate the candidate, snapshot the parents, pull
/// with backup retry, and normalize the updated modes.
pub fn pull_base(
    inputs: &PullBaseInputs<'_>,
    moves: &mut MoveCache,
    out: &mut dyn Write,
    warnings: &mut dyn Write,
) -> PullBaseOutcome {
    let failed = || PullBaseOutcome {
        status: PullStatus::Failed,
        rc: 1,
        skip: None,
    };
    let done = |status| PullBaseOutcome {
        status,
        rc: 0,
        skip: None,
    };
    let skipped = |reason| PullBaseOutcome {
        status: PullStatus::Skipped,
        rc: 0,
        skip: Some(reason),
    };
    // A missing topology has no git function to probe, like the
    // shell's failing `_base_git`.
    let Some(prefix) = inputs.base.git_prefix() else {
        return skipped(SkipReason::NoUpstream);
    };
    if !has_upstream(&prefix) {
        let Some(stranded) = stranded_head(&prefix) else {
            return skipped(SkipReason::NoUpstream);
        };
        if stranded.warn_now(inputs.quiet) {
            inputs.log.warn(
                warnings,
                &format!(
                    "  warning: dotfiles checkout {}",
                    stranded.describe(&prefix)
                ),
            );
        }
        if stranded.fails() {
            return failed();
        }
        // A healed checkout is back on its branch: pull it now rather
        // than waiting a cycle. Anything else keeps the skip.
        if stranded.kind != StrandedHead::Healed || !has_upstream(&prefix) {
            return skipped(stranded.kind.skip_reason());
        }
    }
    // Before any fetch or rebase: conflict markers in live files must
    // never read as `current`, and nothing may rebase over them.
    match check_index(&prefix) {
        IndexCheck::Clean => {}
        IndexCheck::Session(session) => {
            if session.warn_now(inputs.quiet) {
                inputs.log.warn(
                    warnings,
                    &format!("  warning: dotfiles checkout {}", session.describe(&prefix)),
                );
            }
            return skipped(SkipReason::Session);
        }
        IndexCheck::Unmerged(unmerged) => {
            if unmerged.warn_now(inputs.quiet) {
                inputs.log.warn(
                    warnings,
                    &format!(
                        "  warning: dotfiles checkout {}",
                        unmerged.describe(&prefix)
                    ),
                );
            }
            return failed();
        }
    }
    // With the Repos row hidden (quiet, cron), name the repository above its
    // fetch output, as an overlay's is named.
    let mut header = Vec::new();
    if inputs.quiet {
        inputs.log.warn(&mut header, "  dotfiles fetch output:");
    }
    let mut fetch_output = crate::repos_pull_support::HeaderFirst::new(warnings, header);
    let fetched = prepare_base_upstream(inputs.base, &mut fetch_output, inputs.terminal);
    let upstream = match fetched {
        Ok(upstream) => upstream,
        Err(_) => return failed(),
    };
    let (accept_status, head_before) = accept_current_generation(
        &prefix,
        "base",
        &upstream,
        inputs.candidate,
        inputs.log,
        warnings,
    );
    match accept_status {
        0 => return done(PullStatus::Current),
        1 => {}
        _ => return failed(),
    }
    if !validate_candidate_tree(
        &prefix,
        "base",
        &upstream,
        inputs.candidate,
        inputs.log,
        warnings,
    ) {
        return failed();
    }
    // The snapshot is in-memory `identity\trelative` text, not a temp
    // file: nothing to release, and it must never reach a path API
    // (long snapshots exceed NAME_MAX and failed a successful pull).
    let Some(snapshot) =
        snapshot_updated_path_parents(&prefix, &inputs.base.home, &head_before, &upstream)
    else {
        return failed();
    };
    if !repo_head_is(&prefix, &head_before) {
        return failed();
    }
    // The prefix carries only the topology flags; `_base_git`
    // supplies the `git` binary itself.
    let command = rebase_command(&prefix, &upstream, inputs.extra_args);
    let repo_inputs = PullRepoInputs {
        home: &inputs.base.home,
        root: &inputs.base.home,
        base: inputs.base,
        quarantine: inputs.quarantine.clone(),
        overlays: inputs.overlays,
        dest: inputs.dest,
        manifest: inputs.manifest,
        legacy_manifest: inputs.legacy_manifest,
        euid: inputs.euid,
        source_root: inputs.source_root,
        tmp: inputs.tmp,
        tool: inputs.tool,
        command: &command,
        quiet: inputs.quiet,
        verbose: inputs.verbose,
        log: inputs.log,
    };
    let rebase = RebaseGuard::arm(&prefix, &head_before, &upstream);
    if let Some(session) = rebase.user_session(&prefix) {
        if session.warn_now(inputs.quiet) {
            inputs.log.warn(
                warnings,
                &format!("  warning: dotfiles checkout {}", session.describe(&prefix)),
            );
        }
        return skipped(SkipReason::Session);
    }
    if rebase.repeats_failure() {
        inputs.log.warn(
            warnings,
            &format!("  warning: dotfiles checkout {}", repeated_failure(&prefix)),
        );
        return failed();
    }
    rebase.begin();
    if pull_repo(&repo_inputs, moves, out, warnings) != 0 {
        if !rebase.abort_failed(&prefix) {
            inputs.log.warn(
                warnings,
                &format!("  warning: dotfiles checkout {}", unaborted(&prefix)),
            );
        }
        return failed();
    }
    rebase.succeeded();
    // Probed first: normalization itself can fail over the conflict
    // markers, and the stash warning is the one the user must see.
    let unmerged = unmerged_after_pull(&prefix);
    let head_after = repo_head(&prefix);
    let mut status = PullStatus::Current;
    if !head_before.is_empty() && !head_after.is_empty() && head_before != head_after {
        let normalized = read_umask()
            .map(crate::startup::ensure_umask_ceiling)
            .is_ok_and(|mask| {
                normalize_updated_paths(
                    &prefix,
                    &inputs.base.home,
                    "base",
                    &head_before,
                    &head_after,
                    &snapshot,
                    &inputs.base.home,
                    inputs.overlays,
                    mask,
                )
            });
        if !normalized && unmerged.is_none() {
            return failed();
        }
        status = PullStatus::Changed;
    }
    if let Some(unmerged) = unmerged {
        // A new event: always shown, and recorded so cron does not
        // repeat it while the markers stay.
        if unmerged.warn_now(inputs.quiet) {
            inputs.log.warn(
                warnings,
                &format!(
                    "  warning: dotfiles checkout {}",
                    unmerged.describe(&prefix)
                ),
            );
        }
        return failed();
    }
    done(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_state_ignores_an_am_session() {
        // `git am` shares `rebase-apply` but marks it `applying`; that
        // session is the user's and must neither read as a stranded
        // rebase nor arm an abort.
        let scope = dot_test_support::TempDir::new("rebase-state").unwrap();
        let dir = scope.path();
        assert!(!rebase_state_exists(dir));
        std::fs::create_dir(dir.join("rebase-apply")).unwrap();
        assert!(rebase_state_exists(dir));
        std::fs::write(dir.join("rebase-apply/applying"), b"").unwrap();
        assert!(!rebase_state_exists(dir));
        std::fs::remove_dir_all(dir.join("rebase-apply")).unwrap();
        std::fs::create_dir(dir.join("rebase-merge")).unwrap();
        assert!(rebase_state_exists(dir));
    }

    #[test]
    fn abort_only_claims_a_rebase_of_this_attempts_pair() {
        // A rebase the user started between arming and the pull
        // replays a different pair and must not be aborted as ours.
        let scope = dot_test_support::TempDir::new("rebase-owner").unwrap();
        let dir = scope.path();
        let (head, upstream) = ("a".repeat(40), "b".repeat(40));
        let guard = RebaseGuard {
            git_dir: Some(dir.to_path_buf()),
            armed: true,
            head: head.clone(),
            upstream: upstream.clone(),
        };
        let state = dir.join("rebase-merge");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("orig-head"), format!("{head}\n")).unwrap();
        std::fs::write(state.join("onto"), format!("{}\n", "c".repeat(40))).unwrap();
        assert!(state_replaying(dir, &head, &upstream).is_none());
        std::fs::write(state.join("onto"), format!("{upstream}\n")).unwrap();
        assert!(state_replaying(dir, &head, &upstream).is_some());
        // Not ours means no abort and no failure record; the session
        // stays for the user.
        std::fs::write(state.join("orig-head"), "user\n").unwrap();
        assert!(guard.abort_failed(&[]));
        assert!(state.exists());
        assert!(!dir.join(FAILED_MARKER).exists());
    }

    #[test]
    fn only_the_recorded_pair_reads_as_dots_interrupted_rebase() {
        let scope = dot_test_support::TempDir::new("rebase-inflight").unwrap();
        let dir = scope.path();
        let state = dir.join("rebase-merge");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("orig-head"), "h\n").unwrap();
        std::fs::write(state.join("onto"), "u\n").unwrap();
        // No record: the user's.
        assert!(!interrupted_by_dot(dir));
        std::fs::write(dir.join(INFLIGHT_MARKER), "h other\n").unwrap();
        assert!(!interrupted_by_dot(dir));
        // A torn or malformed record never claims a session.
        std::fs::write(dir.join(INFLIGHT_MARKER), "h").unwrap();
        assert!(!interrupted_by_dot(dir));
        std::fs::write(dir.join(INFLIGHT_MARKER), "h u\n").unwrap();
        assert!(interrupted_by_dot(dir));
        // A record written long before this rebase started is stale
        // (dot died outside the rebase, or it was aborted by hand): a
        // later rebase over the same pair is the user's.
        let marker = std::fs::File::options()
            .write(true)
            .open(dir.join(INFLIGHT_MARKER))
            .unwrap();
        let old = std::time::SystemTime::now() - INFLIGHT_WINDOW * 2;
        marker.set_modified(old).unwrap();
        assert!(!interrupted_by_dot(dir));
        marker
            .set_modified(std::time::SystemTime::now() + INFLIGHT_WINDOW)
            .unwrap();
        assert!(!interrupted_by_dot(dir));
        marker.set_modified(std::time::SystemTime::now()).unwrap();
        assert!(interrupted_by_dot(dir));
        // `git am` shares `rebase-apply`; it is never dot's.
        std::fs::remove_dir_all(&state).unwrap();
        let apply = dir.join("rebase-apply");
        std::fs::create_dir(&apply).unwrap();
        std::fs::write(apply.join("orig-head"), "h\n").unwrap();
        std::fs::write(apply.join("onto"), "u\n").unwrap();
        std::fs::write(apply.join("applying"), b"").unwrap();
        assert!(!interrupted_by_dot(dir));
    }

    #[test]
    fn hints_quote_the_checkout_prefix() {
        let prefix = [
            OsString::from("--git-dir=/h/a b/.dotfiles"),
            OsString::from("--work-tree=/h/a b"),
        ];
        assert_eq!(
            git_hint(&prefix, "rebase --abort"),
            "`git --git-dir=/h/a\\ b/.dotfiles --work-tree=/h/a\\ b rebase --abort`"
        );
    }
}
