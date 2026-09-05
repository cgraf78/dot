//! Native overlay link hot loop (engine link-exec lane).
//!
//! Ports `_link_overlay` (`lib/dot/repos/overlays.sh`): one overlay's
//! `home/` inventory linked into `$HOME` with the shell's exact
//! validation order, skip messages, link rows, manifest records, and
//! failure points. The per-file cost that dominates converged updates
//! (~160ms under bash: ~16 `stat` spawns, `readlink`/`git` spawns,
//! and thousands of loop iterations) becomes native syscalls here;
//! the orchestration (inventories, authority, stale cleanup, manifest
//! commit) and the update wiring arrive in later slices.
//!
//! Composition notes:
//!
//! - Field splitting reuses the fleet's `parse_overlay` shape through
//!   the caller; this layer takes the normalized `sync` word.
//! - Inventory bytes arrive NUL-delimited, exactly as
//!   [`crate::repos_link_prep`] (or the shell `find ... -print0`)
//!   writes them; `rel` derivation is the shell's byte prefix strip,
//!   lossy past this boundary like the other ports.
//! - The authority cache stays disabled: it only memoizes pure
//!   verdicts, so rows and state agree with the shell's enabled
//!   cache while one knob fewer can diverge.
//! - stdout (`out`) carries `_log`/`_ui_status` rows, stderr (`err`)
//!   carries `_warn` rows, matching the shell streams. Quiet gating
//!   follows `_log` (arithmetic `DOT_QUIET == 1`); `_warn` never
//!   gates, like the shell.
//!
//! - On failure the shell leaks whatever `REPLY` the authority check
//!   left behind (the pending manifest path); no caller reads it on
//!   the failure path, so [`Outcome::Failed`] carries no reply text
//!   and differential tests normalize the failure reply away.

use std::collections::HashSet;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use crate::progress_ui::{Palette, arith_value};
use crate::repos_base::Base;
use crate::repos_overlays::AuthorityCache;

/// Shared inputs for [`link_overlay`]: every value the shell reads
/// from globals or the environment, plus the raw UI spellings so
/// row gating reads exactly like the shell's.
pub struct Inputs<'a> {
    /// Overlay name for messages and manifest ownership.
    pub name: &'a str,
    /// Checkout path.
    pub path: &'a str,
    /// Sync mode, normalized (`""` already reads `"git"`).
    pub sync: &'a str,
    /// Client `$HOME`.
    pub home: &'a str,
    /// `"$path/home"` prefix the inventory paths strip.
    pub overlay_home: &'a str,
    /// Overlay records (`OVERLAYS`) for the active/outside checks.
    pub overlays: &'a [String],
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a crate::repos_overlays::DestinationInputs,
    /// Reserved roots snapshot (`_dot_reserved_roots_snapshot`).
    pub reserved_roots: Option<&'a str>,
    /// Authority `(rel, target)` pairs for stale-link matching.
    pub authority_targets: &'a [(String, String)],
    /// Base client repository (`None` without one: nothing reads
    /// tracked and git never runs, like the empty `_base_tracked`).
    pub base: Option<&'a Base>,
    /// `git ls-files` set for skip-worktree decisions.
    pub base_tracked: &'a HashSet<String>,
    /// Selected manifest for recovery records.
    pub manifest: &'a str,
    /// Legacy manifest for authority comparisons.
    pub legacy_manifest: &'a str,
    /// Manifest under construction (`$_overlay_manifest_new`).
    pub manifest_new: &'a Path,
    /// Frozen source root (local overlays only).
    pub source_root: Option<&'a str>,
    /// Frozen `dev:ino` of that root (local overlays only).
    pub source_identity: Option<&'a str>,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root_git: &'a Path,
    /// Base for the legacy-hash throwaway repository.
    pub tmp: &'a Path,
    /// Probed move tool for the publish walk.
    pub tool: &'a crate::temp::MoveTool,
    /// Logger palette for rows and warnings.
    pub palette: &'a Palette,
    /// Whether to count UTF-8 characters for status cells.
    pub multibyte: bool,
    /// `DOT_QUIET`: `_log` rows stay silent at arithmetic 1.
    pub dot_quiet: Option<&'a str>,
    /// `DOT_VERBOSE`: running/changed/ok rows print at arithmetic 1.
    pub dot_verbose: Option<&'a str>,
    /// `DOT_UI_TOTAL`: link rows print at zero or under verbose.
    pub ui_total: Option<&'a str>,
}

/// Mutable per-overlay link state: the installed-path set (the
/// shell's `_overlay_current_paths`) plus the UI live flag the
/// status rows thread through.
pub struct OverlayState {
    /// Home-relative paths installed by this run so far.
    pub current: HashSet<String>,
    /// Live-line flag for [`crate::progress_ui::status`].
    pub live_active: bool,
}

impl OverlayState {
    /// Fresh link state: nothing installed, no live line (isolated
    /// runs and the fleet convention both start cleared).
    pub fn new() -> Self {
        Self {
            current: HashSet::new(),
            live_active: false,
        }
    }
}

impl Default for OverlayState {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of [`link_overlay`]: the shell `REPLY`/`REPLY_STATUS`
/// pair (`Failed` is the shell `return 1` with no reply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// At least one link published (`"$name overlay linked $n"`).
    Changed(String),
    /// Every entry already current (`"$name overlay current"`).
    Current(String),
    /// Any validation, publish, or record step failed.
    Failed,
}

/// Whether `DOT_QUIET` silences `_log` rows.
fn is_quiet(dot_quiet: Option<&str>) -> bool {
    crate::log::is_quiet(dot_quiet)
}

/// Whether `DOT_VERBOSE` enables running/changed/ok rows
/// (arithmetic 1, like the shell).
fn is_verbose(dot_verbose: Option<&str>) -> bool {
    arith_value(dot_verbose.unwrap_or("0")) == Some(1)
}

/// Whether link rows print: `DOT_UI_TOTAL == 0` or verbose, like
/// the shell's `[[ "${DOT_UI_TOTAL:-0}" -eq 0 || ... == 1 ]]`.
fn link_rows_visible(ui_total: Option<&str>, verbose: bool) -> bool {
    ui_total.and_then(arith_value).unwrap_or(0) == 0 || verbose
}

/// Append one `_warn` row to the stderr stream.
fn warn_row(err: &mut Vec<u8>, palette: &Palette, message: String) {
    err.extend_from_slice(&crate::progress_ui::warn_line(palette, message.as_bytes()));
}

/// Append one `_ui_status` row to the stdout stream, threading the
/// live flag exactly like the shell global.
fn status_row(
    state: &mut OverlayState,
    inputs: &Inputs<'_>,
    status: &[u8],
    detail: &str,
    out: &mut Vec<u8>,
) {
    let (bytes, live) = crate::progress_ui::status(
        inputs.palette,
        is_quiet(inputs.dot_quiet),
        state.live_active,
        status,
        detail.as_bytes(),
        inputs.multibyte,
    );
    out.extend_from_slice(&bytes);
    state.live_active = live;
}

/// Append one `_log` row unless quiet hides it.
fn log_row(out: &mut Vec<u8>, inputs: &Inputs<'_>, text: &str) {
    if is_quiet(inputs.dot_quiet) {
        return;
    }
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
}

/// `_base_git update-index --skip-worktree`: mark one shadowed path
/// so Git stops seeing the overlay symlink. `None` without a base
/// repository (the caller never asks then, like the empty shell
/// map); failure is the shell `|| return 1`.
fn skip_worktree(base: Option<&Base>, rel: &str) -> bool {
    let Some(base) = base else {
        return false;
    };
    let Some(prefix) = base.git_prefix() else {
        return false;
    };
    crate::repos_base::run_git(&prefix, &["update-index", "--skip-worktree", rel])
        .is_some_and(|output| output.status.success())
}

/// One inventory entry: the shell `while read -d ''` body. Returns
/// the link tally step (`Linked`, `Current`, `Skipped`) or `None`
/// for the shell `return 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStep {
    /// A symlink was published.
    Linked,
    /// The live link already matched.
    Current,
    /// A skip message was noted; neither counter moves.
    Skipped,
}

/// Split inventory bytes the way `read -d ''` consumes `find
/// -print0` output: every NUL-terminated record runs the body,
/// including a trailing unterminated tail; only the terminator's
/// own empty tail is skipped.
fn inventory_records(inventory: &[u8]) -> Vec<&[u8]> {
    let mut records: Vec<&[u8]> = Vec::new();
    let mut parts = inventory.split(|byte| *byte == 0).peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() && parts.peek().is_none() {
            break;
        }
        records.push(part);
    }
    records
}

/// Link one `home/` source path into `$HOME`, replicating the
/// `_link_overlay` loop body line for line.
#[allow(clippy::too_many_arguments)]
fn link_one(
    inputs: &Inputs<'_>,
    state: &mut OverlayState,
    cache: &mut AuthorityCache,
    src: &[u8],
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> Option<FileStep> {
    let verbose = is_verbose(inputs.dot_verbose);
    let prefix = format!("{}/", inputs.overlay_home);
    let rel_bytes = src.strip_prefix(prefix.as_bytes()).unwrap_or(src);
    let rel = String::from_utf8_lossy(rel_bytes);
    let rel = rel.as_ref();
    if crate::repos_overlays::path_is_authority(
        inputs.home,
        rel,
        inputs.manifest,
        inputs.legacy_manifest,
        inputs.dest,
        inputs.reserved_roots,
        cache,
    ) {
        warn_row(
            err,
            inputs.palette,
            format!(
                "  warning: {} overlay contains a reserved path: {rel}",
                inputs.name
            ),
        );
        return None;
    }
    if inputs.sync == "none"
        && crate::repos_overlays::local_inventory_entry_current(
            inputs.path,
            &PathBuf::from(std::ffi::OsString::from_vec(src.to_vec())),
            rel,
            inputs.source_root.unwrap_or(""),
            inputs.source_identity.unwrap_or(""),
            inputs.overlays,
            inputs.home,
        )
        .is_err()
    {
        // The shell quotes the diagnostic or falls back to the
        // source path; the port always carries one, so the
        // fallback never fires differentially.
        warn_row(
            err,
            inputs.palette,
            format!(
                "  warning: {} overlay source changed after inventory: {src}",
                inputs.name,
                src = String::from_utf8_lossy(src)
            ),
        );
        return None;
    }
    if let Err(diag) =
        crate::overlays::destination_outside_local_sources(rel, inputs.overlays, inputs.home)
    {
        warn_row(
            err,
            inputs.palette,
            format!(
                "  warning: {} overlay destination is unsafe: {diag}",
                inputs.name
            ),
        );
        return None;
    }
    let dst = format!("{}/{rel}", inputs.home);
    let dst_parent = Path::new(&dst)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent_is_dir = std::fs::metadata(&dst_parent).is_ok_and(|meta| meta.is_dir());
    if !parent_is_dir && !crate::repos_overlays::ensure_destination_parent(inputs.home, &dst_parent)
    {
        return None;
    }
    let target = crate::repos_overlays::record_link_target(
        rel,
        inputs.name,
        inputs.path,
        Some(inputs.sync),
    )?;
    let tracked = inputs.base_tracked.contains(rel);
    if let Ok(live) = std::fs::read_link(&dst) {
        if live.as_os_str().as_bytes() == target.as_bytes() {
            if inputs.sync == "none"
                && crate::repos_overlays::local_inventory_entry_current(
                    inputs.path,
                    &PathBuf::from(std::ffi::OsString::from_vec(src.to_vec())),
                    rel,
                    inputs.source_root.unwrap_or(""),
                    inputs.source_identity.unwrap_or(""),
                    inputs.overlays,
                    inputs.home,
                )
                .is_err()
            {
                warn_row(
                    err,
                    inputs.palette,
                    format!(
                        "  warning: {} overlay source changed before link acceptance: {src}",
                        inputs.name,
                        src = String::from_utf8_lossy(src)
                    ),
                );
                return None;
            }
            if tracked && !skip_worktree(inputs.base, rel) {
                return None;
            }
            if !crate::repos_overlays::record_final(
                rel,
                inputs.name,
                &target,
                inputs.manifest_new,
                &mut state.current,
            ) {
                return None;
            }
            return Some(FileStep::Current);
        }
    }
    let mut replace: Option<String> = None;
    match std::fs::symlink_metadata(&dst) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
        Ok(meta) => {
            let ftype = meta.file_type();
            if ftype.is_symlink() {
                // The shell gates on `sync == none || tracked`
                // before consulting either match: a guarded foreign
                // symlink stays, while an unguarded one (or a
                // matched one) falls through to replacement below.
                let guarded = inputs.sync == "none" || tracked;
                if guarded
                    && !crate::repos_overlays::active_link_matches(
                        inputs.home,
                        inputs.overlays,
                        rel,
                    )
                    && !crate::repos_overlays::authority_link_matches(
                        inputs.home,
                        inputs.authority_targets,
                        rel,
                    )
                {
                    warn_row(
                        err,
                        inputs.palette,
                        format!("  skip (would replace unmanaged symlink): {rel}"),
                    );
                    return Some(FileStep::Skipped);
                }
                match crate::repos_overlays::replacement_identity(
                    inputs.source_root_git,
                    Path::new(&dst),
                ) {
                    Ok(identity) => replace = Some(identity),
                    Err(_) => return None,
                }
            } else if ftype.is_dir() {
                warn_row(
                    err,
                    inputs.palette,
                    format!("  skip (directory in the way): {rel}"),
                );
                return Some(FileStep::Skipped);
            } else if !tracked {
                warn_row(
                    err,
                    inputs.palette,
                    format!("  skip (would clobber untracked file): {rel}"),
                );
                return Some(FileStep::Skipped);
            } else {
                let base = inputs.base?;
                if !crate::repos_overlays::tracked_path_clean(base, rel) {
                    warn_row(
                        err,
                        inputs.palette,
                        format!("  skip (would clobber modified tracked file): {rel}"),
                    );
                    return Some(FileStep::Skipped);
                }
                match crate::repos_overlays::replacement_identity(
                    inputs.source_root_git,
                    Path::new(&dst),
                ) {
                    Ok(identity) => replace = Some(identity),
                    Err(_) => return None,
                }
            }
        }
    }
    if let Err(diag) =
        crate::overlays::destination_outside_local_sources(rel, inputs.overlays, inputs.home)
    {
        warn_row(
            err,
            inputs.palette,
            format!(
                "  warning: {} overlay destination became unsafe: {diag}",
                inputs.name
            ),
        );
        return None;
    }
    if inputs.sync == "none"
        && crate::repos_overlays::local_inventory_entry_current(
            inputs.path,
            &PathBuf::from(std::ffi::OsString::from_vec(src.to_vec())),
            rel,
            inputs.source_root.unwrap_or(""),
            inputs.source_identity.unwrap_or(""),
            inputs.overlays,
            inputs.home,
        )
        .is_err()
    {
        warn_row(
            err,
            inputs.palette,
            format!(
                "  warning: {} overlay source changed before link creation: {src}",
                inputs.name,
                src = String::from_utf8_lossy(src)
            ),
        );
        return None;
    }
    let publish = crate::repos_overlays::PublishLinkInputs {
        target: &target,
        destination: &dst,
        expected: replace.as_deref(),
        inputs: inputs.dest,
        manifest: inputs.manifest,
        euid: inputs.euid,
        source_root: inputs.source_root_git,
        tmp: inputs.tmp,
        tool: inputs.tool,
    };
    if !crate::repos_overlays::publish_link(&publish) {
        return None;
    }
    if tracked && !skip_worktree(inputs.base, rel) {
        return None;
    }
    if link_rows_visible(inputs.ui_total, verbose) {
        if tracked {
            log_row(out, inputs, &format!("  linked (override): {rel}"));
        } else {
            log_row(out, inputs, &format!("  linked: {rel}"));
        }
    }
    if !crate::repos_overlays::record_final(
        rel,
        inputs.name,
        &target,
        inputs.manifest_new,
        &mut state.current,
    ) {
        return None;
    }
    Some(FileStep::Linked)
}

/// `_link_overlay`: link one overlay's inventory into `$HOME`,
/// returning the shell `REPLY`/`REPLY_STATUS` pair. Rows land in
/// `out`/`err`; `state.current` collects the installed paths for
/// the run-level stale cleanup the orchestration owns.
pub fn link_overlay(
    inputs: &Inputs<'_>,
    state: &mut OverlayState,
    inventory: &[u8],
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> Outcome {
    let verbose = is_verbose(inputs.dot_verbose);
    if verbose {
        status_row(
            state,
            inputs,
            b"running",
            &format!("{} overlay: linking", inputs.name),
            out,
        );
    }
    let mut cache = AuthorityCache::disabled();
    let mut linked: u64 = 0;
    for src in inventory_records(inventory) {
        let step = match link_one(inputs, state, &mut cache, src, out, err) {
            // The shell `|| return 1` after `_link_overlay` fails
            // the whole overlay on the first failing entry.
            None => return Outcome::Failed,
            Some(step) => step,
        };
        // `Current` and `Skipped` move no tally, exactly like the
        // shell: only `linked` drives the closing status.
        if step == FileStep::Linked {
            linked += 1;
        }
    }
    if linked > 0 {
        let reply = format!("{} overlay linked {linked}", inputs.name);
        if verbose {
            status_row(state, inputs, b"changed", &reply, out);
        }
        Outcome::Changed(reply)
    } else {
        let reply = format!("{} overlay current", inputs.name);
        if verbose {
            status_row(state, inputs, b"ok", &reply, out);
        }
        Outcome::Current(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_split_matches_read_d() {
        // `find -print0` terminates every record; only the
        // terminator's empty tail is skipped, while a trailing
        // unterminated tail still runs the body once.
        assert_eq!(inventory_records(b""), Vec::<&[u8]>::new());
        assert_eq!(inventory_records(b"a\0"), vec![b"a".as_slice()]);
        assert_eq!(
            inventory_records(b"a\0b\0"),
            vec![b"a".as_slice(), b"b".as_slice()]
        );
        assert_eq!(
            inventory_records(b"a\0b"),
            vec![b"a".as_slice(), b"b".as_slice()]
        );
        assert_eq!(
            inventory_records(b"a\0\0b\0"),
            vec![b"a".as_slice(), b"".as_slice(), b"b".as_slice()]
        );
    }

    #[test]
    fn row_gates_match_shell_arithmetic() {
        assert!(!is_quiet(None));
        assert!(!is_quiet(Some("0")));
        assert!(is_quiet(Some("1")));
        assert!(!is_quiet(Some("bogus")));
        assert!(!is_verbose(None));
        assert!(is_verbose(Some("1")));
        assert!(!is_verbose(Some("2")));
        assert!(link_rows_visible(None, false));
        assert!(link_rows_visible(Some("0"), false));
        assert!(!link_rows_visible(Some("3"), false));
        assert!(link_rows_visible(Some("3"), true));
    }
}
