//! Shdeps update group labels, summary text, and group records.
//!
//! Owns the group display vocabulary
//! (`_shdeps_group_label`, `_shdeps_summary_text`) and the in-memory
//! group record the event adapter accumulates
//! (`_shdeps_remember_group`, `_shdeps_record_item`,
//! `_shdeps_record_group_summary`, `_shdeps_display_label`).
//!
//! [`crate::shdeps_ui_render`] owns prompt
//! pause/resume pair and the UI reset (`_shdeps_prompt_pause`,
//! `_shdeps_prompt_resume`, `_shdeps_ui_reset`), the verbose and
//! summary renderers (`_shdeps_print_verbose_group_rows`,
//! `_shdeps_print_verbose_items`,
//! `_shdeps_print_group_items_with_status`,
//! `_shdeps_print_group_summaries`), the JSONL event layer
//! (`_shdeps_parse_event`, `_handle_shdeps_event`), the child
//! liveness probes (`_shdeps_proc_state`, `_shdeps_update_finished`),
//! and the FIFO update orchestration (`_run_shdeps_update_ui`,
//! `_run_shdeps_update_command`). [`crate::shdeps`] owns provider policy;
//! nothing here duplicates it.
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

const RETAINED_STATE_LIMIT_BYTES: usize = 1024 * 1024;

/// A provider attempted to retain more UI state than one update run permits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateLimit;

impl std::fmt::Display for StateLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Shdeps provider state exceeded its safety limit")
    }
}

impl std::error::Error for StateLimit {}

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
#[derive(Debug)]
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
    /// Total retained payload bytes across the vectors and map entries above.
    /// Container allocator overhead is bounded separately by the event limit
    /// at the provider boundary.
    retained_bytes: usize,
    retained_limit: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            seen: HashSet::new(),
            labels: HashMap::new(),
            items: HashMap::new(),
            summaries: HashMap::new(),
            retained_bytes: 0,
            retained_limit: RETAINED_STATE_LIMIT_BYTES,
        }
    }
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
    pub fn remember_group(&mut self, group: &[u8]) -> Result<(), StateLimit> {
        if group.is_empty() {
            return Ok(());
        }
        if self.seen.contains(group) {
            return Ok(());
        }
        self.retain(group.len().checked_mul(2).ok_or(StateLimit)?)?;
        self.seen.insert(group.to_vec());
        self.order.push(group.to_vec());
        Ok(())
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
    pub fn record_item(
        &mut self,
        group: &[u8],
        status: &[u8],
        name: &[u8],
        detail: &[u8],
    ) -> Result<(), StateLimit> {
        if group.is_empty() {
            return Ok(());
        }
        self.remember_group(group)?;
        let row_bytes = status
            .len()
            .checked_add(name.len())
            .and_then(|bytes| bytes.checked_add(detail.len()))
            .and_then(|bytes| bytes.checked_add(3))
            .ok_or(StateLimit)?;
        let key_bytes = if self.items.contains_key(group) {
            0
        } else {
            group.len()
        };
        self.retain(key_bytes.checked_add(row_bytes).ok_or(StateLimit)?)?;
        let blob = self.items.entry(group.to_vec()).or_default();
        blob.extend_from_slice(status);
        blob.push(b'\t');
        blob.extend_from_slice(name);
        blob.push(b'\t');
        blob.extend_from_slice(detail);
        blob.push(b'\n');
        Ok(())
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
    ) -> Result<(), StateLimit> {
        if group.is_empty() {
            return Ok(());
        }
        self.remember_group(group)?;
        let resolved = if label.is_empty() {
            group_label(group)
        } else {
            label.to_vec()
        };
        let mut detail = resolved.clone();
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

        let old_label = self.labels.get(group).map_or(0, Vec::len);
        let old_summary = self.summaries.get(group).map_or(0, Vec::len);
        let label_key = if self.labels.contains_key(group) {
            0
        } else {
            group.len()
        };
        let summary_key = if self.summaries.contains_key(group) {
            0
        } else {
            group.len()
        };
        let new_keys = label_key.checked_add(summary_key).ok_or(StateLimit)?;
        let replaced = self
            .retained_bytes
            .checked_sub(old_label)
            .and_then(|bytes| bytes.checked_sub(old_summary))
            .ok_or(StateLimit)?;
        let retained_bytes = replaced
            .checked_add(new_keys)
            .and_then(|bytes| bytes.checked_add(resolved.len()))
            .and_then(|bytes| bytes.checked_add(record.len()))
            .ok_or(StateLimit)?;
        if retained_bytes > self.retained_limit {
            return Err(StateLimit);
        }
        self.retained_bytes = retained_bytes;
        self.labels.insert(group.to_vec(), resolved);
        self.summaries.insert(group.to_vec(), record);
        Ok(())
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

    /// Borrow the recorded item map for the native provider renderer.
    pub(crate) fn items(&self) -> &HashMap<Vec<u8>, Vec<u8>> {
        &self.items
    }

    /// Borrow the recorded label map for the native provider renderer.
    pub(crate) fn labels(&self) -> &HashMap<Vec<u8>, Vec<u8>> {
        &self.labels
    }

    /// Borrow the recorded summary map for the native provider renderer.
    pub(crate) fn summaries(&self) -> &HashMap<Vec<u8>, Vec<u8>> {
        &self.summaries
    }

    fn retain(&mut self, bytes: usize) -> Result<(), StateLimit> {
        let retained = self.retained_bytes.checked_add(bytes).ok_or(StateLimit)?;
        if retained > self.retained_limit {
            return Err(StateLimit);
        }
        self.retained_bytes = retained;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{State, StateLimit};

    #[test]
    fn retained_state_limit_rejects_growth_without_exceeding_the_bound() {
        let mut state = State {
            retained_limit: 32,
            ..State::default()
        };
        state.record_item(b"g", b"ok", b"one", b"two").unwrap();
        let retained_before = state.retained_bytes;

        assert_eq!(
            state.record_item(b"g", b"changed", b"three", b"four"),
            Err(StateLimit)
        );
        assert_eq!(state.retained_bytes, retained_before);
        assert_eq!(state.items_blob(b"g"), Some(b"ok\tone\ttwo\n".as_slice()));
    }

    #[test]
    fn replacing_summaries_accounts_for_only_current_retained_payload() {
        let mut state = State {
            retained_limit: 80,
            ..State::default()
        };
        state
            .record_group_summary(b"g", b"label", b"ok", 0, 1, 0, 0, b"1", 0)
            .unwrap();
        state
            .record_group_summary(b"g", b"x", b"ok", 0, 1, 0, 0, b"2", 0)
            .unwrap();
        assert!(state.retained_bytes <= state.retained_limit);
        assert_eq!(state.display_label(b"g"), b"x");
    }
}
