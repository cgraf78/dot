//! Parallel inventory preparation for overlay linking (engine link-prep lane).
//!
//! Owns overlay inventory preparation: one
//! NUL-delimited inventory per included overlay under a caller-owned root,
//! plus the frozen source-root identities for filesystem (non-`git`)
//! overlays. The link engine later publishes recovery authority and links
//! from these inventories; this layer only discovers and freezes the
//! candidate file sets.
//!
//! A Git overlay's candidates are the files its index tracks under
//! `home/` (`tracked_inventory`); a local (`sync=none`) overlay has no
//! index, so its whole `home/` tree is walked (`walk_inventory`).
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
//! - Inventory order is index order for Git overlays and filesystem
//!   (`readdir`) order for local ones: the byte order of one inventory
//!   is stable on one host but not a contract across hosts. Tests
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

/// Collect the NUL-delimited inventory bytes for a Git overlay at
/// `checkout`: every path its index tracks under `home/` (submodule
/// contents included) that is a regular file or symlink on disk, in
/// index order, with the same backup-name filter as
/// [`walk_inventory`].
///
/// The index, not the worktree, defines what an overlay owns. Anything
/// else in the checkout (a bytecode cache, an editor swap file, a
/// scratch note) was never published by the overlay, and linking it
/// would publish it into `$HOME` on this host only. Staged files count,
/// so `git add` is enough to try a new file before committing it. One
/// `ls-files` reads the index without walking the worktree, so the
/// inventory costs one Git spawn per overlay (in its prep worker)
/// instead of a directory walk.
///
/// A tracked path whose worktree copy is missing, became a directory,
/// or sits below a symlinked directory is skipped: the walk never
/// descended symlinked directories either, so nothing outside the
/// checkout can enter the inventory. A path listed twice for one file
/// (each stage of a merge conflict, or two index spellings of one name
/// on a case- or normalization-insensitive volume) is linked once: both
/// copies would otherwise replace each other's link on every run.
///
/// `None` when Git fails, and when the index file itself is missing
/// while the inventory came out empty over a non-empty `home/` (a
/// deleted `.git/index` lists nothing and exits zero): the caller fails
/// the whole preparation rather than guess at ownership, which would
/// otherwise remove every link of the overlay. An intact index that
/// tracks nothing under `home/` is an empty overlay, not an error.
fn tracked_inventory(checkout: &Path) -> Option<Vec<u8>> {
    use std::collections::{HashMap, HashSet};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    let output = crate::overlays::retry_once(|| {
        let mut command = crate::init_client_identity::host_git_command();
        // An inherited `GIT_INDEX_FILE` (a `dot update` run from a Git
        // hook) would otherwise list some other index as this overlay's.
        crate::temp::scrub_repository_selectors(&mut command);
        command.arg("-C").arg(checkout).args([
            "ls-files",
            "-z",
            "--cached",
            "--recurse-submodules",
            "--",
            "home",
        ]);
        crate::cleanup::run_session_output(
            command,
            None,
            crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
            crate::cleanup::LingerPolicy::Detach,
        )
    })
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut out = Vec::new();
    // Parent directories already proven real (not symlinks) below the
    // checkout, so sibling files share one `lstat` per directory.
    let mut real_dirs: HashSet<PathBuf> = HashSet::new();
    // `dev:ino` of each emitted entry, with the index spellings it took.
    let mut emitted: HashMap<(u64, u64), Vec<Vec<u8>>> = HashMap::new();
    for rel in output.stdout.split(|byte| *byte == 0) {
        // Only `home/...` records: a pathspec setting such as
        // `GIT_ICASE_PATHSPECS` could match `Home/` too, and its paths
        // would not strip to a home-relative destination.
        if !rel.starts_with(b"home/") {
            continue;
        }
        let path = checkout.join(std::ffi::OsStr::from_bytes(rel));
        let Some(base) = path.file_name() else {
            continue;
        };
        if is_backup_name(base.as_bytes()) || !real_parents(checkout, &path, &mut real_dirs) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() && !meta.file_type().is_symlink() {
            continue;
        }
        // A file with one link has one name, so any repeat of its inode
        // is another spelling of it (whatever the volume folds: case,
        // Unicode normalization). Distinct names of a hardlinked file
        // still link separately; only ASCII-case spellings collapse.
        let spellings = emitted.entry((meta.dev(), meta.ino())).or_default();
        if !spellings.is_empty()
            && (meta.nlink() == 1
                || spellings
                    .iter()
                    .any(|spelling| spelling.eq_ignore_ascii_case(rel)))
        {
            continue;
        }
        spellings.push(rel.to_vec());
        out.extend_from_slice(path.as_os_str().as_bytes());
        out.push(0);
    }
    if out.is_empty()
        && std::fs::read_dir(checkout.join("home"))
            .is_ok_and(|mut entries| entries.next().is_some())
        && !index_exists(checkout)?
    {
        return None;
    }
    Some(out)
}

/// Whether the index file of the repository at `checkout` exists, asked
/// only for an empty inventory (one extra Git spawn on that rare path).
/// `None` when Git fails.
fn index_exists(checkout: &Path) -> Option<bool> {
    let output = crate::overlays::retry_once(|| {
        let mut command = crate::init_client_identity::host_git_command();
        crate::temp::scrub_repository_selectors(&mut command);
        command
            .arg("-C")
            .arg(checkout)
            .args(["rev-parse", "--git-path", "index"]);
        crate::cleanup::run_session_output(
            command,
            None,
            crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
            crate::cleanup::LingerPolicy::Detach,
        )
    })
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let index = text.strip_suffix('\n').unwrap_or(&text);
    // A relative answer is relative to the `-C` directory.
    Some(checkout.join(index).exists())
}

/// Whether every directory between `checkout` and `path` is a real
/// directory, never a symlink, memoizing proven ones in `real_dirs`.
fn real_parents(
    checkout: &Path,
    path: &Path,
    real_dirs: &mut std::collections::HashSet<PathBuf>,
) -> bool {
    let mut pending = Vec::new();
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == checkout || real_dirs.contains(current) {
            break;
        }
        pending.push(current.to_path_buf());
        dir = current.parent();
    }
    for current in pending.into_iter().rev() {
        if !std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_dir()) {
            return false;
        }
        real_dirs.insert(current);
    }
    true
}

/// Run one task: gates, inventory, and staging-file write. `None` is
/// the shell `return 1` (unwritable staging, lost source root, failed
/// walk or index read); the caller discards the whole root, like
/// `_link_overlays` removing `inventory_root` on failure.
fn run_task(task: &Task<'_>, home: &str, root: &Path) -> Option<TaskOutcome> {
    let home_dir = Path::new(task.path).join("home");
    let (source_root, source_identity, bytes) = if task.sync == "git" {
        if !crate::overlays::is_worktree(Path::new(task.path)) {
            return Some(TaskOutcome::Skip);
        }
        if crate::overlays::checkout_matches(Path::new(task.path), task.url, home).is_err() {
            return Some(TaskOutcome::Skip);
        }
        (None, None, tracked_inventory(Path::new(task.path))?)
    } else {
        // `cd -P` plus `pwd -P`: the physical root or failure when
        // the directory is gone.
        let real = std::fs::canonicalize(&home_dir).ok()?;
        let identity = crate::repos_overlays::file_identity(&real)?;
        // A local source has no index: its whole tree is the overlay.
        let bytes = walk_inventory(&home_dir).ok()?;
        (
            Some(real.to_string_lossy().into_owned()),
            Some(identity),
            bytes,
        )
    };
    let staging = root.join(format!(".build-{}", task.pos));
    crate::cancellation::check().ok()?;
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
    // Workers probe overlay checkouts with Git; the host-Git binding is
    // thread-local, so carry the dispatcher's selection into each one.
    let host_git = crate::init_client_identity::carry_host_git();
    for (task_chunk, out_chunk) in tasks.chunks(bound).zip(outcomes.chunks_mut(bound)) {
        std::thread::scope(|scope| {
            for (task, slot) in task_chunk.iter().zip(out_chunk.iter_mut()) {
                let host_git = host_git.clone();
                scope.spawn(move || {
                    let _host_git = host_git.bind();
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
                if crate::cancellation::check().is_err() {
                    return None;
                }
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
