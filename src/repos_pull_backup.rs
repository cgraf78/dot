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
use std::os::unix::ffi::OsStrExt as _;
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
        let mut command = std::process::Command::new("stat");
        command.arg(format).arg("%d:%i").arg(path);
        let output = crate::cleanup::run_session_output(
            command,
            None,
            crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
            crate::cleanup::LingerPolicy::Strict,
        )
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

/// Restore one already-recorded backup without consulting the cancellation
/// latch. Recovery is not a new publication: once the source has moved, an
/// interrupt must not strand it merely because the ordinary move helper quite
/// correctly refuses to start more operational subprocesses. The kernel
/// no-replace operation also prevents a concurrent replacement at `target`
/// from being overwritten during rollback.
fn restore_noreplace(source: &Path, target: &Path) -> std::io::Result<()> {
    let source = std::ffi::CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: both C strings remain alive for the syscall, AT_FDCWD selects
    // their absolute/relative path interpretation, and RENAME_NOREPLACE has no
    // pointer output.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            1u32,
        ) as i32
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both C strings remain alive for the call and RENAME_EXCL asks
    // the kernel to reject an existing destination atomically.
    let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    let result = {
        if std::fs::symlink_metadata(std::ffi::OsStr::from_bytes(target.as_bytes())).is_ok() {
            -1
        } else {
            return std::fs::rename(
                std::ffi::OsStr::from_bytes(source.as_bytes()),
                std::ffi::OsStr::from_bytes(target.as_bytes()),
            );
        }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
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
        if move_noreplace_cached(&live, &target, moves).is_ok() {
            // The move helper verified this exact source inode at `target`.
            // Record rollback ownership before any further cancellable probe.
            backed.push(file.clone());
            backed_up += 1;
            if crate::cancellation::check().is_err() {
                failed = true;
                break;
            }
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
                    if restore_noreplace(&backup.join(file), &live).is_err() {
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::repos_base::Topology;
    use crate::repos_overlays::DestinationInputs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    #[test]
    fn cancellation_after_the_move_restores_the_exact_conflict_bytes() {
        const HELPER: &str = "DOT_BACKUP_POST_MOVE_CANCEL_HELPER";
        if std::env::var_os(HELPER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "repos_pull_backup::cancellation_tests::cancellation_after_the_move_restores_the_exact_conflict_bytes",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "post-move cancellation helper failed with {:?}:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let scope = dot_test_support::TempDir::new("backup-post-move-cancel").unwrap();
        let home = scope.path().join("home");
        let root = scope.path().join("root");
        let bin = scope.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let original = b"byte-identical user data\n\0tail";
        std::fs::write(root.join("note"), original).unwrap();
        let pull_log = scope.path().join("pull.log");
        std::fs::write(
            &pull_log,
            b"untracked working tree files would be overwritten by\n\tnote\n",
        )
        .unwrap();

        let ready = scope.path().join("move.ready");
        let fake_mv = bin.join("mv");
        std::fs::write(
            &fake_mv,
            "#!/bin/sh\n\"$DOT_TEST_REAL_MV\" \"$@\"\nstatus=$?\ncase \" $* \" in\n  *'.dot-backup/pull/'*)\n    : >\"$DOT_TEST_MOVE_READY\"\n    trap '' TERM\n    while :; do /bin/sleep 1; done\n    ;;\nesac\nexit \"$status\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_mv, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/usr/bin/stat", bin.join("stat")).unwrap();
        std::os::unix::fs::symlink("/bin/mkdir", bin.join("mkdir")).unwrap();
        // SAFETY: this recursive helper is the only test in its process.
        unsafe {
            std::env::set_var("PATH", &bin);
            std::env::set_var("DOT_TEST_REAL_MV", "/bin/mv");
            std::env::set_var("DOT_TEST_MOVE_READY", &ready);
        }

        let home_text = home.to_string_lossy().into_owned();
        let root_text = root.to_string_lossy().into_owned();
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home_text.clone(),
        };
        let destination = DestinationInputs {
            pwd: home_text.clone(),
            home: home_text.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![],
            init_backup: None,
        };
        let mut moves = MoveCache::default();
        let tool = moves.tool().unwrap();
        let log = Log::new(false, false);
        let signals = crate::cleanup::Signals::install().unwrap();
        let signal_ready = ready.clone();
        let sender = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !signal_ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                signal_ready.exists(),
                "the move never reached its post-publication hold"
            );
            // SAFETY: the recursive helper owns an installed SIGTERM handler.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
        });
        let outcome = backup_pull_conflicts(
            &BackupConflictsInputs {
                home: &home_text,
                root: &root_text,
                pull_log: &pull_log,
                base: &base,
                quarantine: None,
                overlays: &[],
                dest: &destination,
                manifest: "",
                legacy_manifest: "",
                euid: crate::temp::current_uid().unwrap(),
                source_root: scope.path(),
                tmp: scope.path(),
                log: &log,
                tool: &tool,
            },
            &mut moves,
            &mut Vec::new(),
        );
        sender.join().unwrap();
        let status = signals.finish(i32::from(!outcome.succeeded));

        assert_eq!(status, 128 + libc::SIGTERM);
        assert_eq!(std::fs::read(root.join("note")).unwrap(), original);
    }
}
