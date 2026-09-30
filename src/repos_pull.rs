//! `_pull_repo` and `_pull_base` (`lib/dot/repos/pull.sh`): the
//! logged pull with conflict-backup retry, and the base
//! orchestrator built on it.
//!
//! The implementation stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::ffi::OsString;
use std::io::Write;
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
            let visible: Vec<&str> = content
                .lines()
                .filter(|line| !is_up_to_date_noise(line))
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

/// A checkout state with no `@{u}` that is not a deliberate
/// no-upstream branch. A rebase in progress is broken local state
/// (possibly dot's own, with conflict markers in live files) and
/// fails; a bare detached HEAD is always a user action (dot never
/// leaves one without rebase state), such as a bisect or a pinned
/// commit, so it warns but keeps the rc-0 skip instead of freezing
/// linking, hooks, and the provider every cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrandedHead {
    /// A rebase is in progress (conflict markers may be live).
    Rebase,
    /// HEAD is detached with no rebase in progress.
    Detached,
}

impl StrandedHead {
    /// Warning tail naming the state and the manual way out.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            StrandedHead::Rebase => {
                "has a rebase in progress; resolve it or run `git rebase --abort`"
            }
            StrandedHead::Detached => {
                "has a detached HEAD; pulls are skipped until it is back on a branch"
            }
        }
    }

    /// Whether the state fails the pull (else it warns and skips).
    pub(crate) fn fails(self) -> bool {
        self == StrandedHead::Rebase
    }
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

/// Classify a checkout whose `@{u}` probe failed. Only runs on that
/// already-unusual path, so healthy pulls pay no extra subprocess.
/// Git failures fall back to `None` (the historical skip).
pub(crate) fn stranded_head(prefix: &[OsString]) -> Option<StrandedHead> {
    if absolute_git_dir(prefix).is_some_and(|dir| rebase_state_exists(&dir)) {
        return Some(StrandedHead::Rebase);
    }
    // `symbolic-ref -q` exits 1 exactly for a detached HEAD; any
    // other failure is not evidence either way.
    let output = crate::repos_base::run_git(prefix, &["symbolic-ref", "-q", "HEAD"])?;
    (output.status.code() == Some(1)).then_some(StrandedHead::Detached)
}

/// Warning tail for a pull skipped by [`RebaseGuard::repeats_conflict`].
pub(crate) const REPEATED_CONFLICT: &str =
    "still conflicts with its upstream; rebase it manually (or wait for upstream to move)";

/// Marker in the per-worktree git dir recording the `HEAD upstream`
/// pair whose rebase conflicted and was aborted. Git ignores unknown
/// files there; nothing but [`RebaseGuard`] reads it.
const CONFLICT_MARKER: &str = "dot-rebase-conflict";

/// Rebase bookkeeping around the `rebase --autostash` that
/// [`pull_repo`] runs: the state observed before it (so a failure
/// only aborts a rebase this run started) and the pair it replays.
pub(crate) struct RebaseGuard {
    /// Per-worktree git dir, or `None` when unresolvable (no abort,
    /// no conflict memory).
    git_dir: Option<PathBuf>,
    /// False when a rebase (or `git am`) was already in progress:
    /// that session belongs to the user and is never aborted.
    armed: bool,
    /// HEAD the rebase replays (its `orig-head`).
    head: String,
    /// Resolved upstream tip it replays onto (its `onto`).
    upstream: String,
}

impl RebaseGuard {
    /// Record the pre-pull state for replaying `head` onto the
    /// resolved `upstream` tip.
    pub(crate) fn arm(prefix: &[OsString], head: &str, upstream: &str) -> Self {
        let git_dir = absolute_git_dir(prefix);
        let armed = git_dir.as_deref().is_some_and(|dir| {
            dir.join("rebase-merge").symlink_metadata().is_err()
                && dir.join("rebase-apply").symlink_metadata().is_err()
        });
        Self {
            git_dir,
            armed,
            head: head.to_string(),
            upstream: upstream.to_string(),
        }
    }

    /// Marker body for this attempt.
    fn pair(&self) -> String {
        format!("{} {}\n", self.head, self.upstream)
    }

    /// Whether this exact `HEAD upstream` pair already conflicted on
    /// an earlier run. Retrying it cannot succeed, and every attempt
    /// swaps tracked files (in `$HOME`, for the base) under a running
    /// session and then hard-resets them on abort, so a save landing
    /// in that window would be lost. The caller reports failure
    /// without touching the work tree until the user rebases, or
    /// either side moves.
    pub(crate) fn repeats_conflict(&self) -> bool {
        self.git_dir.as_deref().is_some_and(|dir| {
            std::fs::read_to_string(dir.join(CONFLICT_MARKER)).is_ok_and(|text| text == self.pair())
        })
    }

    /// Whether the in-progress rebase replays exactly this attempt's
    /// `orig-head` onto its `onto`, so a rebase the user started in
    /// the window after [`Self::arm`] is never mistaken for ours.
    fn owns_state(&self, dir: &Path) -> bool {
        let read = |state: &str, leaf: &str| {
            std::fs::read_to_string(dir.join(state).join(leaf))
                .map(|text| text.trim().to_string())
                .unwrap_or_default()
        };
        ["rebase-merge", "rebase-apply"].iter().any(|state| {
            read(state, "orig-head") == self.head && read(state, "onto") == self.upstream
        })
    }

    /// After a failed pull, abort the rebase it left behind. A
    /// conflicting `rebase --autostash` otherwise strands the
    /// checkout with conflict markers in live files and a detached
    /// HEAD, and every later run would read the missing `@{u}` as a
    /// no-upstream skip. `--abort` restores the pre-rebase HEAD and
    /// reapplies the autostash; if that reapply conflicts, git keeps
    /// the changes in the stash list, so no user edit is lost. An
    /// abort that follows real conflicts records the pair for
    /// [`Self::repeats_conflict`]; other mid-rebase failures (a lock,
    /// a full disk) stay retryable. Returns false only when a rebase
    /// remains in progress (the next run then reports it through
    /// [`stranded_head`]); an unresolvable git dir or a failure that
    /// never started a rebase returns true.
    pub(crate) fn abort_failed(&self, prefix: &[OsString]) -> bool {
        let Some(dir) = self.git_dir.as_deref().filter(|_| self.armed) else {
            return true;
        };
        if !rebase_state_exists(dir) {
            return true;
        }
        // Not ours, or a latched signal (the supervised spawn would
        // fail anyway): leave the state for the next run to report.
        if !self.owns_state(dir) || crate::cleanup::received_signal().is_some() {
            return false;
        }
        let conflicted =
            crate::repos_base::run_git(prefix, &["diff", "--name-only", "--diff-filter=U"])
                .is_some_and(|output| output.status.success() && !output.stdout.is_empty());
        let _ = crate::repos_base::run_git(prefix, &["rebase", "--abort"]);
        if rebase_state_exists(dir) {
            return false;
        }
        if conflicted {
            // Best effort: without the marker the next run retries.
            let _ = std::fs::write(dir.join(CONFLICT_MARKER), self.pair());
        }
        true
    }

    /// After a successful pull, forget any recorded conflict.
    pub(crate) fn succeeded(&self) {
        if let Some(dir) = self.git_dir.as_deref() {
            let _ = std::fs::remove_file(dir.join(CONFLICT_MARKER));
        }
    }
}

/// `_pull_base` outcome status (`REPLY_STATUS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullStatus {
    /// No upstream tracks the checkout (a branch without tracking,
    /// or a warned detached HEAD; a stranded rebase reads as
    /// `Failed`).
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
    };
    let done = |status| PullBaseOutcome { status, rc: 0 };
    // A missing topology has no git function to probe, like the
    // shell's failing `_base_git`.
    let Some(prefix) = inputs.base.git_prefix() else {
        return done(PullStatus::Skipped);
    };
    if !has_upstream(&prefix) {
        if let Some(stranded) = stranded_head(&prefix) {
            inputs.log.warn(
                warnings,
                &format!("  warning: dotfiles checkout {}", stranded.describe()),
            );
            if stranded.fails() {
                return failed();
            }
        }
        return done(PullStatus::Skipped);
    }
    let upstream = match prepare_base_upstream(inputs.base) {
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
    let mut command: Vec<OsString> = vec![crate::init_client_identity::host_git_program()];
    command.extend(prefix.iter().cloned());
    command.push(OsString::from("rebase"));
    command.push(OsString::from("--autostash"));
    command.push(OsString::from(&upstream));
    command.extend(inputs.extra_args.iter().cloned());
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
    if rebase.repeats_conflict() {
        inputs.log.warn(
            warnings,
            &format!("  warning: dotfiles checkout {REPEATED_CONFLICT}"),
        );
        return failed();
    }
    if pull_repo(&repo_inputs, moves, out, warnings) != 0 {
        if !rebase.abort_failed(&prefix) {
            inputs.log.warn(
                warnings,
                &format!(
                    "  warning: dotfiles checkout {}",
                    StrandedHead::Rebase.describe()
                ),
            );
        }
        return failed();
    }
    rebase.succeeded();
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
        if !normalized {
            return failed();
        }
        status = PullStatus::Changed;
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
        let guard = RebaseGuard {
            git_dir: Some(dir.to_path_buf()),
            armed: true,
            head: "a".repeat(40),
            upstream: "b".repeat(40),
        };
        let state = dir.join("rebase-merge");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("orig-head"), format!("{}\n", "a".repeat(40))).unwrap();
        std::fs::write(state.join("onto"), format!("{}\n", "c".repeat(40))).unwrap();
        assert!(!guard.owns_state(dir));
        std::fs::write(state.join("onto"), format!("{}\n", "b".repeat(40))).unwrap();
        assert!(guard.owns_state(dir));
        // Not ours means no abort: the state stays for the next run.
        std::fs::write(state.join("orig-head"), "user\n").unwrap();
        assert!(!guard.abort_failed(&[]));
        assert!(state.exists());
    }
}
