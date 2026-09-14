//! `_backup_pull_conflicts` (`lib/dot/repos/pull.sh`): back up the
//! untracked files a failed pull names so the pull can retry over a
//! clean tree.
//!
//! Managed overlay generations are adopted into quarantine instead
//! of the backup when the pull root is `$HOME` and quarantine
//! inputs are provided (the shell's `root == $HOME` plus
//! `declare -F` gate); anything else moves under a stamped
//! `$HOME/.dot-backup/pull` directory after a device-and-inode
//! identity check, with committed adoptions and moved files
//! restored on failure exactly like the shell's recovery walk.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::log::Log;
use crate::repos_base::Base;
use crate::repos_overlays::{
    Adoption, QuarantineInputs, QuarantineOutcome, RestoreInstalledInputs, commit_quarantined_link,
    quarantine_rollback_link, restore_installed_links, restore_quarantined_link,
};
use crate::repos_pull_support::{backup_dir, conflicts_from_log};
use crate::temp::{MoveCache, MoveTool, move_noreplace_cached};

/// `_dot_path_identity` for the backup walk: `stat -c '%d:%i'`,
/// falling back to `stat -f '%d:%i'` exactly like the shell. The
/// fallback matters on Linux, where `-f` reports filesystem status:
/// it succeeds for dangling links (whose target cannot be stated)
/// with values that still match across the move, so dangling
/// conflicts back up instead of failing. Forking costs what the
/// shell pays per conflict file; the shared [`crate::temp`] helper
/// stays on its fast `stat(2)` path for the swap comparisons, which
/// never meet dangling links.
fn live_identity(path: &Path) -> Option<String> {
    for format in ["-c", "-f"] {
        let output = std::process::Command::new("stat")
            .arg(format)
            .arg("%d:%i")
            .arg(path)
            .output()
            .ok()?;
        if output.status.success() {
            return Some(
                String::from_utf8_lossy(&output.stdout)
                    .trim_end_matches('\n')
                    .to_string(),
            );
        }
    }
    None
}

/// Inputs for [`backup_pull_conflicts`], replacing the shell's
/// positional log/root parameters and its process-wide globals with
/// explicit values.
pub struct BackupConflictsInputs<'a> {
    /// Client `$HOME`: the backup parent and the adoption root.
    pub home: &'a str,
    /// Pull root holding the conflicting paths (`$2`, `$HOME` by
    /// default in the shell).
    pub root: &'a str,
    /// Failed-pull log file scanned for conflict names.
    pub pull_log: &'a Path,
    /// Base checkout for the installed-link restore walk.
    pub base: &'a Base,
    /// Quarantine support. `Some` engages adoption when
    /// `root == home`, mirroring the shell's `declare -F` gate;
    /// `None` backs every conflict up as user data. The snapshot
    /// inside doubles as the installed-link restore walk, and its
    /// `source_root` must equal [`BackupConflictsInputs::source_root`]
    /// — the shell shares one `$DOT_SOURCE_ROOT` for both.
    pub quarantine: Option<QuarantineInputs>,
    /// Overlay records (`OVERLAYS`) for the restore walk.
    pub overlays: &'a [String],
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a crate::repos_overlays::DestinationInputs,
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
    /// Logger for the backup warnings (`_warn`).
    pub log: &'a Log,
    /// Probed move tool for the restore walk.
    pub tool: &'a MoveTool,
}

/// `_backup_pull_conflicts` outcome: success (shell 0) or failure
/// (shell 1), mirroring `REPLY`.
pub struct BackupOutcome {
    /// Whether conflicts were backed up or adopted (shell 0).
    pub succeeded: bool,
    /// Stamped backup directory (`REPLY`) whenever `_backup_dir`
    /// created one — on success and on failure alike. `None` when no
    /// backup was created; the shell's `REPLY` then holds whatever
    /// helper output came last, which no caller consumes.
    pub backup: Option<PathBuf>,
}

/// Back up the conflicts a failed pull logged, adopting managed
/// overlay generations when `inputs.quarantine` allows. Warnings go
/// to `warnings` like the shell's `_warn` stderr lines.
pub fn backup_pull_conflicts(
    inputs: &BackupConflictsInputs<'_>,
    moves: &mut MoveCache,
    warnings: &mut dyn Write,
) -> BackupOutcome {
    let content = std::fs::read_to_string(inputs.pull_log).unwrap_or_default();
    let files = conflicts_from_log(&content);
    if files.is_empty() {
        return BackupOutcome {
            succeeded: false,
            backup: None,
        };
    }
    let root = Path::new(inputs.root);
    let mut backup: Option<PathBuf> = None;
    let mut backed: Vec<String> = Vec::new();
    let mut adoptions: Vec<Adoption> = Vec::new();
    let mut backed_up = 0;
    let mut adopted = 0;
    let mut failed = false;
    let mut recovery_failed = false;
    for file in &files {
        if file.is_empty() {
            continue;
        }
        let live = root.join(file);
        if std::fs::symlink_metadata(&live).is_err() {
            continue;
        }
        if inputs.root == inputs.home {
            if let Some(quarantine) = &inputs.quarantine {
                match quarantine_rollback_link(file, quarantine) {
                    QuarantineOutcome::Adopt(adoption) => {
                        adoptions.push(adoption);
                        adopted += 1;
                        continue;
                    }
                    QuarantineOutcome::NotManaged => {}
                    QuarantineOutcome::Unsafe => {
                        failed = true;
                        break;
                    }
                }
            }
        }
        if backup.is_none() {
            match backup_dir(inputs.home, warnings) {
                Some(dir) => backup = Some(dir),
                None => {
                    failed = true;
                    break;
                }
            }
        }
        let backup = backup.as_ref().expect("backup dir");
        // `${file%/*}` string semantics: a bare leaf stays at the
        // backup root, anything else nests.
        let parent = match file.rsplit_once('/') {
            Some((dir, _)) => backup.join(dir),
            None => backup.clone(),
        };
        if std::fs::create_dir_all(&parent).is_err() {
            failed = true;
            break;
        }
        let source = match live_identity(&live) {
            Some(identity) => identity,
            None => {
                failed = true;
                break;
            }
        };
        let target = backup.join(file);
        let moved = move_noreplace_cached(&live, &target, moves).is_ok();
        if live_identity(&target) == Some(source.clone()) {
            backed.push(file.clone());
            backed_up += 1;
            continue;
        }
        if live_identity(&live) != Some(source.clone()) {
            // The move raced or landed elsewhere: report where the
            // generation ended up before giving up.
            let leaf = file.rsplit('/').next().unwrap_or(file);
            let nested = target.join(leaf);
            if live_identity(&nested) == Some(source) {
                inputs.log.warn(
                    warnings,
                    &format!(
                        "  warning: user conflict stranded during backup: {}",
                        nested.display()
                    ),
                );
            } else {
                inputs.log.warn(
                    warnings,
                    &format!("  warning: user conflict move became ambiguous: {file}"),
                );
            }
            recovery_failed = true;
        }
        if moved {
            recovery_failed = true;
        }
        failed = true;
        break;
    }

    if !failed {
        for adoption in &adoptions {
            if commit_quarantined_link(
                inputs.source_root,
                &adoption.parked,
                &adoption.stage,
                &adoption.expected,
            )
            .is_err()
            {
                failed = true;
                break;
            }
        }
    }

    if failed {
        // The shell restores last-adopted-first; every step best
        // effort, sticky `recovery_failed` deciding the warning.
        let tool = moves.tool().ok();
        let mut committed = false;
        for adoption in adoptions.iter().rev() {
            if std::fs::symlink_metadata(&adoption.parked).is_err() {
                committed = true;
                if adoption.stage.is_dir() && std::fs::remove_dir(&adoption.stage).is_err() {
                    recovery_failed = true;
                    retained(inputs.log, warnings, &adoption.stage);
                }
                continue;
            }
            let restored = tool.as_ref().is_some_and(|tool| {
                restore_quarantined_link(
                    inputs.source_root,
                    &adoption.physical,
                    &adoption.parked,
                    &adoption.stage,
                    &adoption.expected,
                    tool,
                )
                .is_ok()
            });
            if !restored {
                recovery_failed = true;
                retained(inputs.log, warnings, &adoption.stage);
            }
        }
        if committed {
            if let Some(quarantine) = &inputs.quarantine {
                let restored = tool.as_ref().is_some_and(|tool| {
                    restore_installed_links(&RestoreInstalledInputs {
                        base: inputs.base,
                        home: inputs.home,
                        rels: &quarantine.snapshot.paths,
                        targets: &quarantine.snapshot.targets,
                        overlays: inputs.overlays,
                        dest: inputs.dest,
                        manifest: inputs.manifest,
                        legacy_manifest: inputs.legacy_manifest,
                        euid: inputs.euid,
                        source_root: inputs.source_root,
                        tmp: inputs.tmp,
                        tool,
                    })
                });
                if !restored {
                    recovery_failed = true;
                }
            }
        }
        if let Some(backup) = &backup {
            for file in backed.iter().rev() {
                let live = root.join(file);
                if std::fs::symlink_metadata(&live).is_err() {
                    if move_noreplace_cached(&backup.join(file), &live, moves).is_err() {
                        recovery_failed = true;
                    }
                } else {
                    recovery_failed = true;
                }
            }
            // `rmdir -p` over the nested parents, stopping at the
            // first non-empty directory like the shell's loop.
            for file in &backed {
                if !file.contains('/') {
                    continue;
                }
                let mut parent = backup.join(file.rsplit_once('/').expect("slash").0);
                while parent != *backup && parent.starts_with(backup) {
                    if std::fs::remove_dir(&parent).is_err() {
                        break;
                    }
                    // `${parent%/*}`: pop one level.
                    if !parent.pop() {
                        break;
                    }
                }
            }
            let _ = std::fs::remove_dir(backup);
        }
        if recovery_failed {
            inputs.log.warn(
                warnings,
                &format!(
                    "  warning: conflict recovery incomplete; preserved backup at {}",
                    backup
                        .as_deref()
                        .map(|dir: &Path| dir.display().to_string())
                        .unwrap_or_else(|| "see quarantine warning above".to_string()),
                ),
            );
        }
        return BackupOutcome {
            succeeded: false,
            backup,
        };
    }

    if backed_up == 0 && adopted == 0 {
        return BackupOutcome {
            succeeded: false,
            backup: None,
        };
    }

    // Adopted-only runs never create the backup directory: the shell
    // reports an empty `REPLY` and prints just the adoption lines.
    let backup = backup.as_ref();
    if backed_up > 0 {
        let backup = backup.expect("backup with backed files");
        inputs.log.warn(
            warnings,
            &format!(
                "  backed up {backed_up} conflicting untracked files to {}",
                backup.display()
            ),
        );
    }
    if adopted == 1 {
        inputs.log.warn(
            warnings,
            "  adopted 1 managed overlay path for the base repository",
        );
    } else if adopted > 1 {
        inputs.log.warn(
            warnings,
            &format!("  adopted {adopted} managed overlay paths for the base repository"),
        );
    }
    BackupOutcome {
        succeeded: true,
        backup: backup.cloned(),
    }
}

/// `managed-link quarantine retained at` warning shared by both
/// retained-stage paths.
fn retained(log: &Log, warnings: &mut dyn Write, stage: &Path) {
    log.warn(
        warnings,
        &format!(
            "  warning: managed-link quarantine retained at {}",
            stage.display()
        ),
    );
}
