//! Parallel inventory preparation for overlay linking (engine link-prep lane).
//!
//! Ports `_overlay_prepare_inventories` (`lib/dot/repos/overlays.sh`): one
//! NUL-delimited inventory per included overlay under a caller-owned root,
//! plus the frozen source-root identities for filesystem (non-`git`)
//! overlays. The link engine later publishes recovery authority and links
//! from these inventories; this layer only discovers and freezes the
//! candidate file sets.
//!
//! Inclusion mirrors the shell gate for gate: an entry needs a `home/`
//! directory, a matching Git worktree for `git`-synced overlays, or a
//! readable physical source root for local overlays. Anything else is
//! skipped silently, exactly like the shell's `continue` arms. Field
//! splitting reuses [`crate::repos_pull_fleet::parse_overlay`], so the
//! `OVERLAYS` record shape stays single-sourced.
//!
//! The per-overlay builds fan out in scoped threads bounded by
//! [`crate::merges::update_jobs`] (`DOT_UPDATE_JOBS`, minimum one) in
//! bound-sized chunks, the [`crate::repos_pull_fleet`] pattern: each
//! worker writes its own `$root/.build-<position>` file and the parent
//! renames the successes to the shell's sequential `$root/<index>`
//! numbering in declaration order. Nothing is wired yet: the update
//! engine still drives the shell `_link_overlays`, so this lane changes
//! no behavior (the integrator owns the wiring).
//!
//! Two boundaries are documented, not hidden:
//!
//! - Walk order is filesystem (`readdir`) order, like the shell
//!   `find ... -print0`: the byte order of one inventory is stable on
//!   one host but not a contract across hosts. Differential tests
//!   compare sorted entry sets.
//! - An empty overlay path reads as skipped here (fail closed). The
//!   shell would resolve it against `/home` and walk the live home
//!   tree; discovery never emits such descriptors, and no suite
//!   covers them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Shared inputs for [`prepare_inventories`]: every pull context the
/// shell reads from globals, plus the raw job spelling so the bound
/// reads exactly like the shell's.
pub struct Inputs<'a> {
    /// Overlay records (`OVERLAYS`).
    pub entries: &'a [String],
    /// Client `$HOME`: effective-URL base for checkout matching.
    pub home: &'a str,
    /// `DOT_UPDATE_JOBS`: numeric bound, else the CPU count.
    pub update_jobs: Option<&'a str>,
}

/// Outcome of [`prepare_inventories`]: the inventory index plus the
/// frozen local-source identities, keyed by overlay name like the
/// shell's `_overlay_inventory_files`, `_overlay_inventory_source_roots`,
/// and `_overlay_inventory_source_identities` maps.
#[derive(Debug, Default)]
pub struct Prepared {
    /// Overlay name to `$root/<index>` inventory path.
    pub inventories: HashMap<String, PathBuf>,
    /// Overlay name to physical `home/` root (local overlays only).
    pub source_roots: HashMap<String, String>,
    /// Overlay name to `dev:ino` of that root (local overlays only).
    pub source_identities: HashMap<String, String>,
}

/// One declaration-order build task: a `home/`-bearing entry whose
/// remaining gates (worktree, checkout, source identity, walk) run
/// in a worker.
struct Task<'a> {
    /// Declaration position: keys the worker's staging file.
    pos: usize,
    /// Overlay name for messages and map keys.
    name: &'a str,
    /// Checkout path.
    path: &'a str,
    /// Configured URL (before `~`/relative resolution).
    url: &'a str,
    /// Sync mode (`"git"`, or anything else for local sources).
    sync: &'a str,
}

/// Worker result: skipped entries vanish (the shell `continue`),
/// ready entries carry their inventory bytes plus the frozen local
/// identity (`None` pair for `git` overlays).
enum TaskOutcome {
    /// Gate failed: the entry takes no index, like `continue`.
    Skip,
    /// Gate passed: the staging file is written; the commit phase
    /// renames it into place plus records the frozen local identity
    /// (`None` pair for `git` overlays).
    Ready {
        /// Declaration position for the staging-file rename.
        pos: usize,
        /// Overlay name for the map keys.
        name: String,
        /// Physical `home/` root (local overlays only).
        source_root: Option<String>,
        /// `dev:ino` of that root (local overlays only).
        source_identity: Option<String>,
    },
}

/// Job bound from `DOT_UPDATE_JOBS` (numeric, else the CPU count,
/// minimum one), the [`crate::repos_pull_fleet`] spelling.
fn jobs_bound(raw: Option<&str>) -> usize {
    let text = crate::merges::update_jobs(raw.unwrap_or(""));
    text.parse::<usize>().unwrap_or(1).max(1)
}

/// Whether `find -name '*.~[0-9]*~'` drops `base`: ends with `~`
/// with a `.~<ASCII digit>` span somewhere before it. Byte-level,
/// like the shell glob (only the stream split carries meaning
/// elsewhere, so non-UTF8 names compare exact here too).
fn is_backup_name(base: &[u8]) -> bool {
    if base.len() < 4 || !base.ends_with(b"~") {
        return false;
    }
    let stem = &base[..base.len() - 1];
    stem.windows(3)
        .any(|w| w[0] == b'.' && w[1] == b'~' && w[2].is_ascii_digit())
}

/// Collect the NUL-delimited inventory bytes for `home`: every
/// regular file and symlink under it, depth-first in `readdir`
/// order (the shell `find` traversal with its default `-P`, which
/// never descends an overlay-shipped symlinked dir). A symlinked
/// root emits itself alone, like the shell `find` printing its
/// command-line argument.
fn walk_inventory(home: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![home.to_path_buf()];
    while let Some(path) = stack.pop() {
        let ftype = std::fs::symlink_metadata(&path)?.file_type();
        if !ftype.is_dir() {
            if ftype.is_file() || ftype.is_symlink() {
                if let Some(base) = path.file_name() {
                    if !is_backup_name(base.as_bytes()) {
                        out.extend_from_slice(path.as_os_str().as_bytes());
                        out.push(0);
                    }
                }
            }
            continue;
        }
        let mut children: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            children.push(entry?.path());
        }
        // Reverse-push so pops visit children in `readdir` order
        // with depth-first descent, exactly like `find`.
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    Ok(out)
}

/// Run one task: gates, walk, and staging-file write. `None` is the
/// shell `return 1` (unwritable staging, lost source root, failed
/// walk); the caller discards the whole root, like `_link_overlays`
/// removing `inventory_root` on failure.
fn run_task(task: &Task<'_>, home: &str, root: &Path) -> Option<TaskOutcome> {
    let home_dir = Path::new(task.path).join("home");
    let (source_root, source_identity) = if task.sync == "git" {
        if !crate::overlays::is_worktree(Path::new(task.path)) {
            return Some(TaskOutcome::Skip);
        }
        if crate::overlays::checkout_matches(Path::new(task.path), task.url, home).is_err() {
            return Some(TaskOutcome::Skip);
        }
        (None, None)
    } else {
        // `cd -P` plus `pwd -P`: the physical root or failure when
        // the directory is gone.
        let real = std::fs::canonicalize(&home_dir).ok()?;
        let identity = crate::repos_overlays::file_identity(&real)?;
        (Some(real.to_string_lossy().into_owned()), Some(identity))
    };
    let bytes = walk_inventory(&home_dir).ok()?;
    let staging = root.join(format!(".build-{}", task.pos));
    std::fs::write(&staging, &bytes).ok()?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600)).ok()?;
    }
    Some(TaskOutcome::Ready {
        pos: task.pos,
        name: task.name.to_string(),
        source_root,
        source_identity,
    })
}

/// `_overlay_prepare_inventories`: build one `$root/<index>` inventory
/// per included overlay in declaration order, fanning the gate and
/// walk work out within the job bound. `root` must exist (the shell
/// `mktemp -d` caller owns it); a missing or unwritable root fails,
/// like the shell's `: >file`. Returns `None` exactly where the
/// shell returns 1. Staging files never survive success: every
/// `.build-<position>` is renamed into place during the ordered
/// commit.
pub fn prepare_inventories(inputs: &Inputs<'_>, root: &Path) -> Option<Prepared> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut tasks: Vec<Task<'_>> = Vec::new();
    for (pos, entry) in inputs.entries.iter().enumerate() {
        let parsed = crate::repos_pull_fleet::parse_overlay(entry);
        // Empty paths read as skipped (fail closed; see module docs).
        if parsed.path.is_empty() {
            continue;
        }
        if !Path::new(parsed.path).join("home").is_dir() {
            continue;
        }
        tasks.push(Task {
            pos,
            name: parsed.name,
            path: parsed.path,
            url: parsed.url,
            sync: parsed.sync,
        });
    }
    let bound = jobs_bound(inputs.update_jobs);
    let mut outcomes: Vec<Option<TaskOutcome>> = (0..tasks.len()).map(|_| None).collect();
    for (task_chunk, out_chunk) in tasks.chunks(bound).zip(outcomes.chunks_mut(bound)) {
        std::thread::scope(|scope| {
            for (task, slot) in task_chunk.iter().zip(out_chunk.iter_mut()) {
                scope.spawn(move || {
                    *slot = run_task(task, inputs.home, root);
                });
            }
        });
    }
    let mut prepared = Prepared::default();
    let mut index: u64 = 0;
    for slot in &outcomes {
        match slot {
            // A panicking worker never fills its slot (workers hold
            // no locks and index nothing, so panics cannot happen by
            // construction); read it as a plumbing failure, the
            // fleet's missing-rc contract.
            None => return None,
            Some(TaskOutcome::Skip) => {}
            Some(TaskOutcome::Ready {
                pos,
                name,
                source_root,
                source_identity,
            }) => {
                index += 1;
                let staged = root.join(format!(".build-{pos}"));
                let placed = root.join(index.to_string());
                if std::fs::rename(&staged, &placed).is_err() {
                    return None;
                }
                // Clear any ambient mode bits the staging write may
                // have inherited before the rename (the shell
                // `chmod 600`s the final name explicitly).
                if std::fs::set_permissions(&placed, std::fs::Permissions::from_mode(0o600))
                    .is_err()
                {
                    return None;
                }
                prepared.inventories.insert(name.clone(), placed);
                if let Some(value) = source_root {
                    prepared.source_roots.insert(name.clone(), value.clone());
                }
                if let Some(value) = source_identity {
                    prepared
                        .source_identities
                        .insert(name.clone(), value.clone());
                }
            }
        }
    }
    Some(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_match_shell_glob() {
        // `*.~[0-9]*~`: any head, literal `.~`, one digit,
        // any tail, trailing `~`.
        assert!(is_backup_name(b".~0~"));
        assert!(is_backup_name(b"file.~1~"));
        assert!(is_backup_name(b"a.b.~12~"));
        assert!(is_backup_name(b"..~1~"));
        assert!(!is_backup_name(b"a~"));
        assert!(!is_backup_name(b".~~"));
        assert!(!is_backup_name(b".~a~"));
        assert!(!is_backup_name(b".~a0~"));
        assert!(!is_backup_name(b"file.~1"));
        assert!(!is_backup_name(b"file~"));
        assert!(!is_backup_name(b""));
    }

    #[test]
    fn job_bound_matches_fleet_spelling() {
        assert_eq!(jobs_bound(Some("3")), 3);
        assert_eq!(jobs_bound(Some("0")), 1);
        assert_eq!(jobs_bound(Some("")), jobs_bound(None));
        assert_eq!(jobs_bound(Some("abc")), jobs_bound(None));
        assert!(jobs_bound(None) >= 1);
    }
}
