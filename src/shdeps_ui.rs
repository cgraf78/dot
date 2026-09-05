//! Shdeps update group labels, summary text, and group record (slice 44).
//!
//! Ports the first coherent family from
//! `lib/dot/providers/shdeps-ui.sh`: the group display vocabulary
//! (`_shdeps_group_label`, `_shdeps_summary_text`) and the in-memory
//! group record the event adapter accumulates
//! (`_shdeps_remember_group`, `_shdeps_record_item`,
//! `_shdeps_record_group_summary`, `_shdeps_display_label`).
//!
//! Later lanes own the remainder of that file: the prompt
//! pause/resume pair and the UI reset (`_shdeps_prompt_pause`,
//! `_shdeps_prompt_resume`, `_shdeps_ui_reset`), the verbose and
//! summary renderers (`_shdeps_print_verbose_group_rows`,
//! `_shdeps_print_verbose_items`,
//! `_shdeps_print_group_items_with_status`,
//! `_shdeps_print_group_summaries`), the JSONL event layer
//! (`_shdeps_parse_event`, `_handle_shdeps_event`), the child
//! liveness probes (`_shdeps_proc_state`, `_shdeps_update_finished`),
//! and the FIFO update orchestration (`_run_shdeps_update_ui`,
//! `_run_shdeps_update_command`). A different lane family
//! (`src/shdeps.rs` on the unmerged `rust-port-slice-37`/`40`
//! lanes) owns the sibling `lib/dot/providers/shdeps.sh`
//! provider; nothing here duplicates it.
//!
//! Engine boundaries: text flows as bytes, like the sibling
//! [`crate::progress_ui`] helpers, so group keys outside the known
//! shdeps vocabulary pass through verbatim on both sides
//! (including non-UTF-8 bytes, which bash assoc keys accept and
//! `String` keys could not); counts arrive canonical from shell
//! arithmetic upstream (`$(( ))` never emits leading zeros or signs
//! on these paths), matching the precedent in
//! [`crate::progress_ui`] and `merges::summary`, so only `i64` is
//! modeled; and the `", "` join stays single-sourced behind
//! [`crate::progress_ui::join_comma`] rather than re-typed here.

use std::collections::{HashMap, HashSet};

/// `_shdeps_group_label`: the display name for a shdeps dependency
/// group. Known groups map to their fixed titles (`github-releases`
/// and `github-repos` share `GitHub`); `other` and the empty group
/// collapse to `Other`, while a group this dot does not yet know
/// passes through verbatim so a newer shdeps never hides distinct
/// work under `Other`, like the shell `*)` arm.
pub fn group_label(group: &[u8]) -> Vec<u8> {
    match group {
        b"packages" => b"Packages".to_vec(),
        b"github-releases" | b"github-repos" => b"GitHub".to_vec(),
        b"cargo" => b"Cargo".to_vec(),
        b"go" => b"Go".to_vec(),
        b"uv" => b"UV".to_vec(),
        b"npm" => b"NPM".to_vec(),
        b"custom" => b"Custom".to_vec(),
        b"other" | b"" => b"Other".to_vec(),
        _ => group.to_vec(),
    }
}

/// One `N unit` phrase, or empty when `count` is not positive, like
/// a single shell `[[ "$count" -gt 0 ]] && parts+=(...)` arm. The
/// warnings phrase keeps its shell singular (`2 warning`): the
/// shell twin hardcodes the word without plural handling, and the
/// slice pattern ports the quirk instead of fixing it.
fn count_part(count: i64, unit: &[u8]) -> Vec<u8> {
    if count > 0 {
        let mut out = count.to_string().into_bytes();
        out.push(b' ');
        out.extend_from_slice(unit);
        out
    } else {
        Vec::new()
    }
}

/// `_shdeps_summary_text`: the `failed, warning, changed, current,
/// skipped` rollup for one group or the whole run. `current` also
/// renders when every other count is zero, so an idle check still
/// reports `0 current`, like the shell `${#parts[@]} -eq 0` arm.
/// `warnings` mirrors the shell `${5:-0}` default at the call
/// boundary: callers with no warning count pass `0`.
pub fn summary_text(
    changed: i64,
    current: i64,
    skipped: i64,
    failed: i64,
    warnings: i64,
) -> Vec<u8> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    parts.push(count_part(failed, b"failed"));
    parts.push(count_part(warnings, b"warning"));
    parts.push(count_part(changed, b"changed"));
    if current > 0 || parts.iter().all(Vec::is_empty) {
        let mut out = current.to_string().into_bytes();
        out.extend_from_slice(b" current");
        parts.push(out);
    }
    parts.push(count_part(skipped, b"skipped"));
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    crate::progress_ui::join_comma(&refs)
}

/// In-memory `DOT_UI_SHDEPS_*` group globals for one update run:
/// the discovery-ordered group list (`DOT_UI_SHDEPS_GROUP_ORDER`
/// plus the `_SEEN` gate behind it), the per-group display labels
/// (`DOT_UI_SHDEPS_GROUP_LABELS`), the tab-separated item rows
/// (`DOT_UI_SHDEPS_GROUP_ITEMS`), and the tab-separated group
/// summary records (`DOT_UI_SHDEPS_GROUP_SUMMARIES`).
///
/// Bundled so the record stays single-sourced while the later
/// render and event lanes are still shell: those lanes read the
/// same associative state through the accessors below.
#[derive(Debug, Default)]
pub struct State {
    /// `DOT_UI_SHDEPS_GROUP_ORDER`, in first-seen order.
    order: Vec<Vec<u8>>,
    /// `DOT_UI_SHDEPS_GROUP_SEEN` gate behind the order list.
    seen: HashSet<Vec<u8>>,
    /// `DOT_UI_SHDEPS_GROUP_LABELS` display label per group.
    labels: HashMap<Vec<u8>, Vec<u8>>,
    /// `DOT_UI_SHDEPS_GROUP_ITEMS` tab-separated item rows per
    /// group, each `${status}\t${name}\t${detail}\n`.
    items: HashMap<Vec<u8>, Vec<u8>>,
    /// `DOT_UI_SHDEPS_GROUP_SUMMARIES` one tab-separated record per
    /// group, `${status}\t${detail}\t${elapsed_ms}`.
    summaries: HashMap<Vec<u8>, Vec<u8>>,
}

impl State {
    /// Empty record, like the shell right after `_shdeps_ui_reset`
    /// (which the reset lane owns): no group discovered, and no
    /// label, item, or summary stored. The maps are always declared
    /// here, matching the post-reset shell the record family runs
    /// under in production.
    pub fn new() -> Self {
        State::default()
    }

    /// `_shdeps_remember_group`: append `group` to the discovery
    /// order unless already seen, like the shell `_SEEN` gate. An
    /// empty `group` is a no-op: the shell's empty assoc subscript
    /// (`bad array subscript`) aborts the call storing nothing, so
    /// this returns without touching the order either. The stderr
    /// diagnostic and nonzero status are caller UI; the stored state
    /// is the contract.
    pub fn remember_group(&mut self, group: &[u8]) {
        if group.is_empty() {
            return;
        }
        if self.seen.insert(group.to_vec()) {
            self.order.push(group.to_vec());
        }
    }

    /// Discovery order of the groups recorded so far, like
    /// `DOT_UI_SHDEPS_GROUP_ORDER`. Later render lanes iterate this
    /// after the known-group list to append newly discovered groups.
    pub fn order(&self) -> &[Vec<u8>] {
        &self.order
    }

    /// `_shdeps_record_item`: remember `group`, then append one
    /// `${status}\t${name}\t${detail}\n` row to its item blob,
    /// exactly like the shell string append (empty fields still
    /// emit their tabs, so a later `IFS=$'\t' read` splits the
    /// same columns on both sides). An empty `group` stores
    /// nothing: the shell's failed remember unwinds the whole call
    /// before the append, so this returns early like
    /// [`State::remember_group`].
    pub fn record_item(&mut self, group: &[u8], status: &[u8], name: &[u8], detail: &[u8]) {
        if group.is_empty() {
            return;
        }
        self.remember_group(group);
        let blob = self.items.entry(group.to_vec()).or_default();
        blob.extend_from_slice(status);
        blob.push(b'\t');
        blob.extend_from_slice(name);
        blob.push(b'\t');
        blob.extend_from_slice(detail);
        blob.push(b'\n');
    }

    /// Raw item blob for `group`, or `None` before its first item,
    /// like `${DOT_UI_SHDEPS_GROUP_ITEMS[$group]:-}` expanding
    /// empty. Later print lanes split this into status rows.
    pub fn items_blob(&self, group: &[u8]) -> Option<&[u8]> {
        self.items.get(group).map(Vec::as_slice)
    }

    /// `_shdeps_record_group_summary`: remember `group`, resolve an
    /// empty `label` through [`group_label`], then store the
    /// `${status}\t${detail}\t${elapsed_ms}` record where `detail`
    /// is `${label}: ${summary}`. An empty `elapsed_ms` stores `0`,
    /// like the shell `${elapsed_ms:-0}`; any other value stores
    /// literally. `warnings` mirrors the shell `${9:-0}` default at
    /// the call boundary: callers with no warning count pass `0`.
    /// An empty `group` stores nothing: the shell's failed remember
    /// unwinds the whole call before any map write, so this returns
    /// early like [`State::remember_group`].
    #[allow(clippy::too_many_arguments)] // positional parity with the ported shell function
    pub fn record_group_summary(
        &mut self,
        group: &[u8],
        label: &[u8],
        status: &[u8],
        changed: i64,
        current: i64,
        skipped: i64,
        failed: i64,
        elapsed_ms: &[u8],
        warnings: i64,
    ) {
        if group.is_empty() {
            return;
        }
        self.remember_group(group);
        let resolved = if label.is_empty() {
            group_label(group)
        } else {
            label.to_vec()
        };
        self.labels.insert(group.to_vec(), resolved.clone());
        let mut detail = resolved;
        detail.extend_from_slice(b": ");
        detail.extend_from_slice(&summary_text(changed, current, skipped, failed, warnings));
        let mut record = status.to_vec();
        record.push(b'\t');
        record.extend_from_slice(&detail);
        record.push(b'\t');
        if elapsed_ms.is_empty() {
            record.extend_from_slice(b"0");
        } else {
            record.extend_from_slice(elapsed_ms);
        }
        self.summaries.insert(group.to_vec(), record);
    }

    /// Raw summary record for `group`, or `None` before its first
    /// summary, like `${DOT_UI_SHDEPS_GROUP_SUMMARIES[$group]:-}`
    /// expanding empty. Later summary lanes split this into the
    /// status, detail, and elapsed columns.
    pub fn summary_blob(&self, group: &[u8]) -> Option<&[u8]> {
        self.summaries.get(group).map(Vec::as_slice)
    }

    /// `_shdeps_display_label`: the recorded label for `group`, or
    /// [`group_label`] when the group has no recorded (non-empty)
    /// label, like the shell `${...:-...}` fallback. A stored empty
    /// label is unreachable through [`State::record_group_summary`]
    /// (empty resolves at record time) but still falls back here,
    /// exactly like the shell. The empty group resolves through
    /// [`group_label`] to `Other`: the shell's `:-` fallback still
    /// expands there with exit 0, so the stdout contract matches.
    pub fn display_label(&self, group: &[u8]) -> Vec<u8> {
        match self.labels.get(group) {
            Some(label) if !label.is_empty() => label.clone(),
            _ => group_label(group),
        }
    }
}
