//! `_pull_repo` and `_pull_base` (`lib/dot/repos/pull.sh`): the
//! logged pull with conflict-backup retry, and the base
//! orchestrator built on it.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
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
    let child = std::process::Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::from(stream))
        .spawn();
    match child {
        Ok(mut child) => match child.wait() {
            Ok(status) => status.code().unwrap_or(127),
            Err(_) => 127,
        },
        Err(_) => 127,
    }
}

/// Streaming pull without a log, like the `_logfile_create`
/// fallback running `_pull_cmd`: inherited stdio, pinned locale,
/// exit code (127 when spawning fails).
fn run_streaming(argv: &[OsString]) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        return 127;
    };
    match std::process::Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
    {
        Ok(status) => status.code().unwrap_or(127),
        Err(_) => 127,
    }
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

/// `_pull_base` outcome status (`REPLY_STATUS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullStatus {
    /// No upstream tracks the checkout.
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
        return done(PullStatus::Skipped);
    }
    let upstream = match prepare_base_upstream(inputs.base) {
        Ok(upstream) => upstream,
        Err(_) => return failed(),
    };
    let head_before = repo_head(&prefix);
    match accept_current_generation(
        &prefix,
        "base",
        &head_before,
        &upstream,
        inputs.candidate,
        inputs.log,
        warnings,
    ) {
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
    let snapshot =
        match snapshot_updated_path_parents(&prefix, &inputs.base.home, &head_before, &upstream) {
            Some(snapshot) => PathBuf::from(snapshot),
            None => return failed(),
        };
    if !repo_head_is(&prefix, &head_before) {
        let mut cleanup = Registry::new();
        let _ = cleanup.remove_path(&snapshot);
        return failed();
    }
    // The prefix carries only the topology flags; `_base_git`
    // supplies the `git` binary itself.
    let mut command: Vec<OsString> = vec![OsString::from("git")];
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
    if pull_repo(&repo_inputs, moves, out, warnings) != 0 {
        let mut cleanup = Registry::new();
        let _ = cleanup.remove_path(&snapshot);
        return failed();
    }
    let head_after = repo_head(&prefix);
    let mut status = PullStatus::Current;
    if !head_before.is_empty() && !head_after.is_empty() && head_before != head_after {
        let snapshot_text = snapshot.to_string_lossy().into_owned();
        let normalized = read_umask().is_ok_and(|mask| {
            normalize_updated_paths(
                &prefix,
                &inputs.base.home,
                "base",
                &head_before,
                &head_after,
                &snapshot_text,
                &inputs.base.home,
                inputs.overlays,
                mask,
            )
        });
        if !normalized {
            let mut cleanup = Registry::new();
            let _ = cleanup.remove_path(&snapshot);
            return failed();
        }
        status = PullStatus::Changed;
    }
    let mut cleanup = Registry::new();
    if cleanup.remove_path(&snapshot).is_err() {
        return failed();
    }
    done(status)
}
