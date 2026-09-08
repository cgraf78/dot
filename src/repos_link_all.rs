//! Native `_link_overlays` link phase (engine link-all lane).
//!
//! Ports `_link_overlays` (`lib/dot/repos/overlays.sh`): manifest
//! directory setup, replacement recovery, the reserved-roots
//! snapshot, local preflight, authority load (plus the legacy
//! adopt scan), the inventory build, pending-authority publication,
//! the per-overlay link loop over [`crate::repos_link_exec`], stale
//! cleanup, the reserved-roots recheck, inode-verified manifest
//! publication, and the counted UI close. Nothing is wired yet:
//! the update engine still drives the shell `_link_overlays`, so
//! this lane changes no behavior (the integrator owns the wiring).
//!
//! Composition, in shell order: [`crate::repos_overlays`] owns
//! recovery (`recover_replacement`), authority (`load_authority`,
//! `publish_pending`), identity, restore, and manifest safety;
//! [`crate::repos_link_prep`] owns the parallel inventory build;
//! [`crate::repos_link_exec`] owns the per-overlay hot loop. This
//! module only sequences them with the shell's exact warnings,
//! cleanup, and UI.
//!
//! Three boundaries are documented, not hidden:
//!
//! - The shell iterates `_overlay_authority_paths` in hash order
//!   for stale cleanup, so multi-stale runs order warnings
//!   nondeterministically there too. This port sorts stale rels so
//!   one run is reproducible; differential tests keep at most one
//!   stale path where byte order matters.
//! - Temporary names use `pid.counter` suffixes instead of
//!   `mktemp`'s random alphabet. The names never escape into
//!   manifests or replies, so only uniqueness (per process, via
//!   an atomic counter) is contractual.
//! - `_overlay_manifest_new` and the inventory root are created
//!   with `0600`/`0700` at creation instead of `mktemp` plus a
//!   separate `chmod`: observably identical and never briefly
//!   broader.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::log::Log;
use crate::overlays;
use crate::progress_ui::{Palette, Stage};
use crate::repos_base::Base;
use crate::repos_link_exec::{self, OverlayState};
use crate::repos_link_prep;
use crate::repos_overlays::{self, AuthorityCache, DestinationInputs};
use crate::reserved;
use crate::temp::{self, MoveTool};

/// Temporary-name counter: `mktemp` uniqueness without the random
/// alphabet (see module docs). Process-private, so the pid prefix
/// plus this counter never repeats within one run.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Shared inputs for [`link_overlays`]: every global the shell
/// `_link_overlays` reads, plus the UI/logger handles the pull
/// lane threads through the same way.
pub struct Inputs<'a> {
    /// Overlay records (`OVERLAYS`).
    pub entries: &'a [String],
    /// Client `$HOME`.
    pub home: &'a str,
    /// Selected manifest (`$DOT_OVERLAY_MANIFEST`).
    pub manifest: &'a str,
    /// Legacy manifest (`$DOT_OVERLAY_LEGACY_MANIFEST`).
    pub legacy_manifest: &'a str,
    /// `DOT_UPDATE_JOBS`: numeric bound, else the CPU count.
    pub update_jobs: Option<&'a str>,
    /// `DOT_UI_TOTAL`: counted UI takes the stage path.
    pub ui_total: Option<&'a str>,
    /// `DOT_VERBOSE`: running/changed/ok rows print at arithmetic 1.
    pub dot_verbose: Option<&'a str>,
    /// `DOT_QUIET`: `_log` rows stay silent at arithmetic 1.
    pub dot_quiet: Option<&'a str>,
    /// Reserved-roots environment for destination resolution.
    pub dest: &'a DestinationInputs,
    /// Base client repository (`None` without one: the tracked set
    /// stays empty and git never runs, like the shell guard).
    pub base: Option<&'a Base>,
    /// Caller uid for the private record writer.
    pub euid: u32,
    /// Sanitized Git source root for fingerprints.
    pub source_root_git: &'a Path,
    /// Base for the legacy-hash throwaway repository.
    pub tmp: &'a Path,
    /// Probed move tool for the manifest publication.
    pub tool: &'a MoveTool,
    /// Logger palette for rows and warnings.
    pub palette: &'a Palette,
    /// Whether to count UTF-8 characters for status cells.
    pub multibyte: bool,
    /// `DOT_UI_PROGRESS_WIDTH`: bar width, default `"8"`.
    pub bar_width: &'a str,
    /// Logger for headers and `_log` rows.
    pub log: &'a Log,
}

/// Outcome of [`link_overlays`]: the shell return code plus the
/// tallies behind the counted-UI summary.
pub struct LinkOutcome {
    /// Shell return code (0 unless a warned step failed).
    pub rc: i32,
    /// Overlays that published links.
    pub changed: i64,
    /// Overlays already current.
    pub current: i64,
    /// `"$name overlay linked $n"` replies, in link order.
    pub changed_items: Vec<String>,
}

/// `[[ ${text:-0} -gt 0 ]]`: unset and empty read zero, malformed
/// arithmetic reads falsy (the shell errors).
fn gt_zero(raw: Option<&str>) -> bool {
    raw.and_then(crate::progress_ui::arith_value)
        .is_some_and(|value| value > 0)
}

/// `DOT_VERBOSE` at arithmetic 1 (the pull and link lanes spell
/// this the same way).
fn is_verbose(dot_verbose: Option<&str>) -> bool {
    dot_verbose.and_then(crate::progress_ui::arith_value) == Some(1)
}

/// `IFS='|' read -r name path url _ _ sync`: six fields, the
/// remainder collapsing into the last like the shell builtin.
fn split_entry(entry: &str) -> (String, String, String, String) {
    let mut parts = entry.splitn(6, '|');
    let name = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let url = parts.next().unwrap_or("").to_string();
    let _ = parts.next();
    let _ = parts.next();
    let sync = parts.next().unwrap_or("").to_string();
    (name, path, url, sync)
}

/// Append one `_warn` row to the stderr stream.
fn warn_row(err: &mut Vec<u8>, palette: &Palette, message: &str) {
    err.extend_from_slice(&crate::progress_ui::warn_line(palette, message.as_bytes()));
}

/// The shell `${manifest%/*}`: everything before the last slash,
/// or the whole string when there is none.
fn manifest_dir(manifest: &str) -> String {
    match manifest.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => manifest.to_string(),
    }
}

/// Remove the scratch artifacts a failing step leaves behind (the
/// shell's repeated `rm -f manifest_new` / `rm -rf inventory_root`
/// before every `return 1`).
fn cleanup(manifest_new: Option<&Path>, inventory_root: Option<&Path>) {
    if let Some(path) = manifest_new {
        let _ = std::fs::remove_file(path);
    }
    if let Some(root) = inventory_root {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// `_dot_reserved_roots_snapshot`: the newline-joined inventory (no
/// trailing newline — command substitution strips it), or `None`
/// like the bare `return 1`. Overlay link paths come from the
/// `OVERLAYS` records exactly like the shell loop, skipping empty
/// paths.
fn reserved_snapshot(inputs: &Inputs<'_>) -> Option<Vec<String>> {
    use crate::xdg;
    let state_home = xdg::base(
        xdg::Kind::State,
        inputs.dest.xdg_state_home.as_deref().unwrap_or(""),
        inputs.home,
    )
    .ok()?;
    let install_root = inputs
        .dest
        .install_dir
        .clone()
        .unwrap_or_else(|| format!("{}/.local/share", inputs.home));
    let provider_state = inputs
        .dest
        .state_dir
        .clone()
        .unwrap_or_else(|| format!("{state_home}/shdeps"));
    let mut overlay_paths = Vec::new();
    for entry in inputs.entries {
        let (_, path, _, _) = split_entry(entry);
        if !path.is_empty() {
            overlay_paths.push(path);
        }
    }
    let mut init_backup = inputs.dest.init_backup.clone();
    if init_backup.as_deref() == Some("-") {
        init_backup = None;
    }
    reserved::reserved_roots(
        &reserved::RootsInput {
            home: inputs.home.to_string(),
            state_home,
            install_root,
            provider_state,
            overlay_paths,
            init_backup,
        },
        &inputs.dest.pwd,
    )
    .ok()
}

/// Snapshot string form: lines joined with `\n`, like the
/// substitution-stripped shell `REPLY`.
fn snapshot_string(roots: &[String]) -> String {
    roots.join("\n")
}

/// The `_base_tracked` capture: one `git ls-files` whose newline
/// split mirrors `while IFS= read -r` (only the terminator's empty
/// tail is dropped). Empty without a base repository.
fn base_tracked(inputs: &Inputs<'_>) -> HashSet<String> {
    let mut tracked = HashSet::new();
    let Some(base) = inputs.base else {
        return tracked;
    };
    let Some(prefix) = base.git_prefix() else {
        return tracked;
    };
    let Some(output) = crate::repos_base::run_git(&prefix, &["ls-files"]) else {
        return tracked;
    };
    if !output.status.success() {
        return tracked;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    tracked.extend(lines.into_iter().map(str::to_string));
    tracked
}

/// Next temporary suffix (see module docs).
fn temp_suffix() -> String {
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}.{}", std::process::id(), count)
}

/// `mktemp "${manifest}.tmp.XXXXXX"` plus `chmod 600`: an
/// exclusively created empty `0600` file. The name is only unique,
/// never contractual.
fn create_manifest_new(manifest: &str) -> Option<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt as _;
    for _ in 0..16 {
        let path = PathBuf::from(format!("{manifest}.tmp.{}", temp_suffix()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => return Some(path),
            Err(_) => continue,
        }
    }
    None
}

/// `mktemp -d "${manifest}.inventory.XXXXXX"` plus `chmod 700`.
fn create_inventory_root(manifest: &str) -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    for _ in 0..16 {
        let root = PathBuf::from(format!("{manifest}.inventory.{}", temp_suffix()));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => return Some(root),
            Err(_) => continue,
        }
    }
    None
}

/// `_overlay_recover_replacements`: every
/// `$manifest.replace.*` record in glob (sorted) order. `Err`
/// carries the failing record (the shell `REPLY`).
fn recover_replacements(inputs: &Inputs<'_>) -> Result<(), String> {
    let manifest_path = Path::new(inputs.manifest);
    let parent = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(inputs.manifest);
    let prefix = format!("{file_name}.replace.");
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(&prefix) {
                    records.push(entry.path());
                }
            }
        }
    }
    records.sort();
    for record in records {
        if !repos_overlays::recover_replacement(
            &record,
            inputs.manifest,
            inputs.euid,
            inputs.source_root_git,
            inputs.tmp,
            &inputs.dest.pwd,
            inputs.tool,
        ) {
            return Err(record.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// `_dot_candidate_path_is_reserved`: absolute paths only (anything
/// else reads reserved, like `return 2`), a failed snapshot reads
/// reserved (fail closed, like `|| return 0`), else the
/// from-roots verdict over a live inventory. The inventory
/// re-resolves like the shell call: linking may have mutated
/// destinations since the phase snapshot.
fn candidate_reserved(inputs: &Inputs<'_>, path: &str) -> bool {
    if !path.starts_with('/') {
        return true;
    }
    let live = reserved_snapshot(inputs);
    match live {
        None => true,
        Some(live) => {
            let install_root = inputs
                .dest
                .install_dir
                .clone()
                .unwrap_or_else(|| format!("{}/.local/share", inputs.home));
            let checkout = format!("{install_root}/cgraf78/dot");
            reserved::candidate_path_is_reserved_from_roots(
                path,
                &live,
                inputs.home,
                &checkout,
                &inputs.dest.pwd,
            )
        }
    }
}

/// `_overlay_origin_mismatch` adopt command, one `printf %q` word
/// per path (the C-locale byte quoting lives in
/// [`crate::repos_pull_support::shell_quote`]).
fn adopt_command(path: &str, expected: &str, actual: &str) -> String {
    match actual {
        "<missing>" => format!(
            "git -C {} remote add origin {}",
            crate::repos_pull_support::shell_quote(path.as_bytes()),
            crate::repos_pull_support::shell_quote(expected.as_bytes())
        ),
        "<multiple origin URLs>" => format!(
            "git -C {} config --replace-all remote.origin.url {}",
            crate::repos_pull_support::shell_quote(path.as_bytes()),
            crate::repos_pull_support::shell_quote(expected.as_bytes())
        ),
        _ => format!(
            "git -C {} remote set-url origin {}",
            crate::repos_pull_support::shell_quote(path.as_bytes()),
            crate::repos_pull_support::shell_quote(expected.as_bytes())
        ),
    }
}

/// `_link_overlays`: run the whole link phase natively. Rows land
/// in `out`/`err` exactly like the shell streams; `stage` renders
/// the counted-UI open/close the pull lane threads the same way.
/// `now_secs` stamps the stage rows (tests pin matching clocks).
pub fn link_overlays(
    inputs: &Inputs<'_>,
    stage: &mut Stage,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    now_secs: i64,
) -> LinkOutcome {
    let mut outcome = LinkOutcome {
        rc: 1,
        changed: 0,
        current: 0,
        changed_items: Vec::new(),
    };
    // `mkdir -p "${manifest%/*}"`.
    let dir = manifest_dir(inputs.manifest);
    if std::fs::create_dir_all(&dir).is_err() {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: could not create overlay manifest directory: {dir}"),
        );
        return outcome;
    }
    // Recovery first: a stranded generation must converge before
    // anything else reads the filesystem.
    if let Err(record) = recover_replacements(inputs) {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: unsafe overlay replacement recovery record: {record}"),
        );
        return outcome;
    }
    // The reserved-roots snapshot and the local preflight both fail
    // silently (bare `return 1`), except the preflight's own
    // warning, which it emits itself.
    let Some(snapshot) = reserved_snapshot(inputs) else {
        return outcome;
    };
    let snapshot_text = snapshot_string(&snapshot);
    let mut preflight_state = overlays::State {
        overlays: inputs.entries.to_vec(),
        ..Default::default()
    };
    if let Err(warning) = overlays::preflight(&mut preflight_state, inputs.home) {
        err.extend_from_slice(warning.as_bytes());
        err.push(b'\n');
        return outcome;
    }
    // The manifest may be absent, but never a directory or link.
    let manifest_path = Path::new(inputs.manifest);
    if std::fs::symlink_metadata(manifest_path)
        .is_ok_and(|meta| !meta.file_type().is_file() || meta.file_type().is_symlink())
    {
        warn_row(
            err,
            inputs.palette,
            &format!(
                "  warning: overlay manifest path is not a regular file: {}",
                inputs.manifest
            ),
        );
        return outcome;
    }
    // Authority load with the verdict cache enabled: everything
    // above inspects one immutable pre-mutation generation.
    let mut cache = AuthorityCache::enabled();
    let mut ctx = repos_overlays::AuthorityCtx {
        home: inputs.home,
        manifest: inputs.manifest,
        legacy_manifest: inputs.legacy_manifest,
        inputs: inputs.dest,
        roots: Some(&snapshot_text),
        cache: &mut cache,
        euid: inputs.euid,
    };
    // The first load only validates: the post-publication reload
    // below replaces its maps (the shell reassigns the same
    // globals).
    if let Err(pending) = repos_overlays::load_authority(&mut ctx) {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: unsafe overlay recovery manifest; refusing to link: {pending}"),
        );
        return outcome;
    }
    let manifests =
        repos_overlays::authority_files(inputs.manifest, inputs.legacy_manifest, inputs.euid)
            .map(|found| found.manifests)
            .unwrap_or_default();
    let mut adopted_legacy = false;
    if inputs.legacy_manifest != inputs.manifest {
        for authority_manifest in &manifests {
            if authority_manifest == inputs.legacy_manifest {
                adopted_legacy = true;
                break;
            }
        }
    }
    // Declaration scan: overlays with a linkable `home/` tree.
    let mut has_overlay_home = false;
    let mut overlay_total: i64 = 0;
    for entry in inputs.entries {
        let (_, path, url, sync) = split_entry(entry);
        let sync = if sync.is_empty() { "git" } else { &sync };
        if !Path::new(&path).join("home").is_dir() {
            continue;
        }
        if sync == "none" || overlays::checkout_matches(Path::new(&path), &url, inputs.home).is_ok()
        {
            has_overlay_home = true;
            overlay_total += 1;
        }
    }
    // The header and the empty-phase early return only run when
    // something can link (or counted UI forces the stage open).
    // The quiet gate lives inside the stage rendering.
    if has_overlay_home || gt_zero(inputs.ui_total) {
        if gt_zero(inputs.ui_total) {
            let open = stage.start(
                b"Overlays",
                Some(b"checking overlay links"),
                now_secs,
                inputs.dot_verbose,
            );
            let _ = out.write_all(&open);
        } else {
            let open = stage.header_text(b"Overlays");
            let _ = out.write_all(&open);
        }
        if !has_overlay_home && manifests.is_empty() {
            let close = stage.finish(b"ok", b"0 overlays current", now_secs);
            let _ = out.write_all(&close);
            outcome.rc = 0;
            return outcome;
        }
    }
    let tracked = base_tracked(inputs);
    // Manifest draft plus inventory root, then the pending
    // publication that makes every later link recoverable.
    let Some(manifest_new) = create_manifest_new(inputs.manifest) else {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: could not create overlay manifest temp file: {dir}"),
        );
        return outcome;
    };
    let Some(inventory_root) = create_inventory_root(inputs.manifest) else {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: could not inventory overlay recovery candidates: {dir}"),
        );
        cleanup(Some(&manifest_new), None);
        return outcome;
    };
    let prep_inputs = repos_link_prep::Inputs {
        entries: inputs.entries,
        home: inputs.home,
        update_jobs: inputs.update_jobs,
    };
    let Some(prepared) = repos_link_prep::prepare_inventories(&prep_inputs, &inventory_root) else {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: could not inventory overlay recovery candidates: {dir}"),
        );
        cleanup(Some(&manifest_new), Some(&inventory_root));
        return outcome;
    };
    let pending = match repos_overlays::publish_pending(
        &mut ctx,
        inputs.euid,
        inputs.entries,
        &prepared.inventories,
        inputs.tool,
    ) {
        Some(pending) => pending,
        None => {
            cleanup(Some(&manifest_new), Some(&inventory_root));
            return outcome;
        }
    };
    // Reload after publication so stale cleanup accepts both prior
    // owners and every candidate target an interrupted mutation
    // may have left behind. The cache drops here: linking mutates
    // destinations, so every live destination validates again.
    let authority = match repos_overlays::load_authority(&mut ctx) {
        Ok(authority) => authority,
        Err(pending) => {
            warn_row(
                err,
                inputs.palette,
                &format!(
                    "  warning: could not load published overlay recovery authority: {pending}"
                ),
            );
            cleanup(Some(&manifest_new), Some(&inventory_root));
            return outcome;
        }
    };
    // `ctx` (and its cache borrow) ends here: linking validates
    // every live destination again.
    let mut targets: Vec<(String, String)> = authority.targets.into_iter().collect();
    targets.sort();
    let mut overlay_state = OverlayState::new();
    let mut done: i64 = 0;
    let verbose = is_verbose(inputs.dot_verbose);
    for entry in inputs.entries {
        let (name, path, url, sync) = split_entry(entry);
        let sync = if sync.is_empty() {
            "git".to_string()
        } else {
            sync
        };
        if !Path::new(&path).join("home").is_dir() {
            continue;
        }
        if sync == "git" {
            if !overlays::is_worktree(Path::new(&path)) {
                warn_row(
                    err,
                    inputs.palette,
                    &format!(
                        "  warning: {name} overlay path exists but is not a Git worktree; leaving it untouched: {path}"
                    ),
                );
                continue;
            }
            if let Err(actual) = overlays::checkout_matches(Path::new(&path), &url, inputs.home) {
                let expected = overlays::effective_url(&url, inputs.home);
                let command = adopt_command(&path, &expected, &actual);
                if gt_zero(inputs.ui_total) {
                    for detail in [
                        format!(
                            "{name} overlay origin mismatch: expected {expected}, found {actual}"
                        ),
                        format!("verify the checkout, then adopt it with: {command}"),
                    ] {
                        let (bytes, live) = crate::progress_ui::status(
                            inputs.palette,
                            crate::log::is_quiet(inputs.dot_quiet),
                            overlay_state.live_active,
                            b"warning",
                            detail.as_bytes(),
                            inputs.multibyte,
                        );
                        let _ = out.write_all(&bytes);
                        overlay_state.live_active = live;
                    }
                } else {
                    for line in [
                        format!(
                            "  warning: {name} overlay origin does not match its configured URL"
                        ),
                        format!("    expected: {expected}"),
                        format!("    found:    {actual}"),
                        format!("    verify the checkout, then adopt it with: {command}"),
                    ] {
                        warn_row(err, inputs.palette, &line);
                    }
                }
                continue;
            }
        }
        done += 1;
        let progress = stage.maybe_progress(
            name.as_bytes(),
            done,
            overlay_total,
            now_secs,
            inputs.dot_verbose,
            inputs.bar_width,
        );
        let _ = out.write_all(&progress);
        let Some(inventory_path) = prepared.inventories.get(&name) else {
            warn_row(
                err,
                inputs.palette,
                &format!(
                    "  warning: could not link {name} overlay; recovery authority retained: {pending}"
                ),
            );
            cleanup(Some(&manifest_new), Some(&inventory_root));
            return outcome;
        };
        let inventory = match std::fs::read(inventory_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                warn_row(
                    err,
                    inputs.palette,
                    &format!(
                        "  warning: could not link {name} overlay; recovery authority retained: {pending}"
                    ),
                );
                cleanup(Some(&manifest_new), Some(&inventory_root));
                return outcome;
            }
        };
        let overlay_home = format!("{path}/home");
        let link_inputs = repos_link_exec::Inputs {
            name: &name,
            path: &path,
            sync: &sync,
            home: inputs.home,
            overlay_home: &overlay_home,
            overlays: inputs.entries,
            dest: inputs.dest,
            reserved_roots: Some(&snapshot_text),
            authority_targets: &targets,
            base: inputs.base,
            base_tracked: &tracked,
            manifest: inputs.manifest,
            legacy_manifest: inputs.legacy_manifest,
            manifest_new: &manifest_new,
            source_root: prepared.source_roots.get(&name).map(String::as_str),
            source_identity: prepared.source_identities.get(&name).map(String::as_str),
            euid: inputs.euid,
            source_root_git: inputs.source_root_git,
            tmp: inputs.tmp,
            tool: inputs.tool,
            palette: inputs.palette,
            multibyte: inputs.multibyte,
            dot_quiet: inputs.dot_quiet,
            dot_verbose: inputs.dot_verbose,
            ui_total: inputs.ui_total,
        };
        match repos_link_exec::link_overlay(&link_inputs, &mut overlay_state, &inventory, out, err)
        {
            repos_link_exec::Outcome::Changed(reply) => {
                outcome.changed += 1;
                outcome.changed_items.push(reply);
            }
            repos_link_exec::Outcome::Current(_) => {
                outcome.current += 1;
            }
            repos_link_exec::Outcome::Failed => {
                warn_row(
                    err,
                    inputs.palette,
                    &format!(
                        "  warning: could not link {name} overlay; recovery authority retained: {pending}"
                    ),
                );
                cleanup(Some(&manifest_new), Some(&inventory_root));
                return outcome;
            }
        }
    }
    // Clean up every previously or provisionally authoritative
    // path omitted from the final manifest. Stale rels sort: the
    // shell hash order is not contractual (see module docs).
    let mut stale_rels: Vec<&String> = authority.paths.iter().collect();
    stale_rels.sort();
    let mut stale_header = false;
    // `DOT_VERBOSE=1` (and the uncounted path) shows the cleaning
    // rows; the counted path stays quiet unless verbose.
    let verbose_rows = !gt_zero(inputs.ui_total) || verbose;
    for rel in stale_rels {
        if overlay_state.current.contains(rel) {
            continue;
        }
        let dst = format!("{}/{}", inputs.home, rel);
        match std::fs::symlink_metadata(&dst) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&dst)
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !targets.contains(&(rel.clone(), target)) {
                    warn_row(
                        err,
                        inputs.palette,
                        &format!("  skip (stale overlay link was replaced): {rel}"),
                    );
                    continue;
                }
                if candidate_reserved(inputs, &dst) {
                    warn_row(
                        err,
                        inputs.palette,
                        &format!("  warning: refusing to clean a reserved overlay path: {rel}"),
                    );
                    cleanup(Some(&manifest_new), Some(&inventory_root));
                    return outcome;
                }
                if let Err(diagnostic) =
                    overlays::destination_outside_local_sources(rel, inputs.entries, inputs.home)
                {
                    let whose = if diagnostic.is_empty() {
                        rel
                    } else {
                        &diagnostic
                    };
                    warn_row(
                        err,
                        inputs.palette,
                        &format!(
                            "  warning: refusing to clean a link inside a local overlay source: {whose}"
                        ),
                    );
                    cleanup(Some(&manifest_new), Some(&inventory_root));
                    return outcome;
                }
                if !stale_header && verbose_rows {
                    inputs
                        .log
                        .header(out, "==> Cleaning stale overlay symlinks...");
                    stale_header = true;
                }
                if std::fs::remove_file(&dst).is_err() {
                    warn_row(
                        err,
                        inputs.palette,
                        &format!("  warning: could not remove stale overlay link: {rel}"),
                    );
                    cleanup(Some(&manifest_new), Some(&inventory_root));
                    return outcome;
                }
                if verbose_rows {
                    inputs.log.log(out, &format!("  removed: {rel}"));
                }
                // A removed stale link still falls through to the
                // base restore below, like the shell branch.
            }
            Ok(_) => {
                if !tracked.contains(rel)
                    || !inputs
                        .base
                        .is_some_and(|base| repos_overlays::tracked_path_clean(base, rel))
                {
                    warn_row(
                        err,
                        inputs.palette,
                        &format!("  skip (stale overlay path has local content): {rel}"),
                    );
                }
                continue;
            }
            Err(_) => {}
        }
        if tracked.contains(rel) {
            let (restored, warnings) = match inputs.base {
                Some(base) => repos_overlays::restore_tracked_path(
                    inputs.palette,
                    base,
                    inputs.entries,
                    inputs.home,
                    rel,
                ),
                None => (false, Vec::new()),
            };
            err.extend_from_slice(&warnings);
            if !restored {
                warn_row(
                    err,
                    inputs.palette,
                    &format!("  warning: could not restore stale base path: {rel}"),
                );
                cleanup(Some(&manifest_new), Some(&inventory_root));
                return outcome;
            }
        }
    }
    // Linking may have replaced a reserved ancestor: a changed
    // inventory refuses to publish.
    if reserved_snapshot(inputs).as_ref() != Some(&snapshot) {
        warn_row(
            err,
            inputs.palette,
            &format!(
                "  warning: reserved paths changed while linking overlays; recovery authority retained: {pending}"
            ),
        );
        cleanup(Some(&manifest_new), Some(&inventory_root));
        return outcome;
    }
    // Commit the prepared manifest with the inode check that
    // proves the prepared file (not an intervening replacement)
    // landed at the selected path.
    let Some(final_identity) = repos_overlays::file_identity(&manifest_new) else {
        warn_row(
            err,
            inputs.palette,
            &format!(
                "  warning: could not identify prepared overlay manifest: {}",
                manifest_new.display()
            ),
        );
        cleanup(Some(&manifest_new), Some(&inventory_root));
        return outcome;
    };
    let manifest_exists = std::fs::symlink_metadata(manifest_path).is_ok();
    let moved = if manifest_exists {
        temp::move_replace_nodir_with(&manifest_new, manifest_path, inputs.tool).is_ok()
    } else {
        temp::move_noreplace_with(&manifest_new, manifest_path, inputs.tool).is_ok()
    };
    if !moved {
        warn_row(
            err,
            inputs.palette,
            &format!(
                "  warning: could not write overlay manifest: {}",
                inputs.manifest
            ),
        );
        cleanup(Some(&manifest_new), Some(&inventory_root));
        return outcome;
    }
    let verified = repos_overlays::private_regular_file(manifest_path, inputs.euid)
        && repos_overlays::file_identity(manifest_path).as_deref() == Some(final_identity.as_str());
    if !verified {
        warn_row(
            err,
            inputs.palette,
            &format!(
                "  warning: overlay manifest publication could not be verified: {}",
                inputs.manifest
            ),
        );
        cleanup(Some(&manifest_new), Some(&inventory_root));
        return outcome;
    }
    let _ = std::fs::remove_dir_all(&inventory_root);
    if std::fs::remove_file(&pending).is_err() {
        warn_row(
            err,
            inputs.palette,
            &format!("  warning: could not remove overlay recovery manifest: {pending}"),
        );
    }
    if adopted_legacy {
        let legacy_path = Path::new(inputs.legacy_manifest);
        let legacy_link =
            std::fs::symlink_metadata(legacy_path).is_ok_and(|meta| meta.file_type().is_symlink());
        let legacy_regular = std::fs::metadata(legacy_path).is_ok_and(|meta| meta.is_file());
        if !legacy_link && legacy_regular {
            if std::fs::remove_file(legacy_path).is_err() {
                warn_row(
                    err,
                    inputs.palette,
                    &format!(
                        "  warning: could not remove adopted overlay manifest: {}",
                        inputs.legacy_manifest
                    ),
                );
            }
        } else {
            warn_row(
                err,
                inputs.palette,
                &format!(
                    "  warning: adopted overlay manifest changed type; leaving it untouched: {}",
                    inputs.legacy_manifest
                ),
            );
        }
    }
    // Counted close: `ok` with a current phrase when nothing
    // changed, `changed` with both phrases on a mixed run. Notes
    // print unless verbose already showed the rows.
    if gt_zero(inputs.ui_total) {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        if outcome.changed > 0 {
            let mut phrase =
                crate::progress_ui::count_phrase(outcome.changed, b"overlay", Some(b"overlays"));
            phrase.extend_from_slice(b" changed");
            parts.push(phrase);
        }
        if outcome.current > 0 || outcome.changed == 0 {
            let mut phrase =
                crate::progress_ui::count_phrase(outcome.current, b"overlay", Some(b"overlays"));
            phrase.extend_from_slice(b" current");
            parts.push(phrase);
        }
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        let summary = crate::progress_ui::join_comma(&refs);
        let status = if outcome.changed > 0 { "changed" } else { "ok" };
        let close = stage.finish(status.as_bytes(), &summary, now_secs);
        let _ = out.write_all(&close);
        if !verbose {
            for item in &outcome.changed_items {
                let note = stage.note(b"changed", item.as_bytes());
                let _ = out.write_all(&note);
            }
        }
    }
    outcome.rc = 0;
    outcome
}
