//! Shdeps update UI session frame and renderers, part 2 of
//! `lib/dot/providers/shdeps-ui.sh`.
//!
//! This family frames one update run and turns the group record into
//! terminal output: the session reset (`_shdeps_ui_reset`), the prompt
//! bracketing (`_shdeps_prompt_pause`, `_shdeps_prompt_resume`), and
//! every renderer (`_shdeps_print_verbose_group_rows`,
//! `_shdeps_print_verbose_items`,
//! `_shdeps_print_group_items_with_status`,
//! `_shdeps_print_group_summaries`). Part 1 (the group vocabulary and
//! record) lives on the unmerged `rust-port-slice-44` lane as
//! `shdeps_ui`; this module stacks beside it once both land, which is
//! why the renderers take the record as plain maps plus a display
//! fallback instead of borrowing that lane's `State`.
//!
//! Later lanes own the remainder of the file: the JSONL event layer
//! (`_shdeps_parse_event`, `_handle_shdeps_event`), the child
//! liveness probes (`_shdeps_proc_state`, `_shdeps_update_finished`),
//! and the FIFO update orchestration (`_run_shdeps_update_ui`,
//! `_run_shdeps_update_command`). A different lane family
//! (`shdeps_env_abi` on the unmerged `rust-port-slice-59` lane, the
//! lock reader and checkpoint record on `rust-port-slice-37` /
//! `rust-port-slice-40`) owns the sibling
//! `lib/dot/providers/shdeps.sh` provider; nothing here duplicates
//! any of them.
//!
//! Engine boundaries: text flows as bytes, like the sibling
//! [`crate::progress_ui`] helpers and the part-1 record, so item
//! names outside UTF-8 pass through verbatim on both sides. Every
//! `_ui_*` effect goes through the already ported
//! [`crate::progress_ui`] twins with the same marker palette the
//! progress tests pin, so only this family's control flow is new:
//! the known-group order plus discovery order with the shell's
//! dedup gate, the per-label section merge, the wanted-status
//! filter, and the threshold branch. The dedup gate quotes the
//! group (`*" $group "*`), so metacharacters match literally: a
//! group spelling `c*` never swallows `cargo`, and the port is a
//! plain substring search. Shell `_warn` diagnostics fold into the
//! data refusal like parts before them. Counts and elapsed values
//! arrive canonical from shell arithmetic upstream, matching the
//! precedent in [`crate::progress_ui`] and `merges::summary`, so
//! only `i64` is modeled; other arithmetic spellings stay
//! unreproduced and unrowed, like the decimal-only flags on the
//! `rust-port-slice-59` lane. Concretely: identifier-like elapsed
//! values read as unset shell variables (`0`) with no diagnostic,
//! so `abc` against a positive threshold agrees false; against a
//! zero threshold the shell would read true, which agrees with the
//! port only for statuses whose note is threshold-invariant
//! (`failed`, `warning`, and unknown — rows pin `failed`) and stays
//! unrowed for the rest; invalid-octal spellings like `08` print an
//! arithmetic diagnostic and read false on both sides (rows pin the
//! shared stdout while the diagnostic stays shell-side stderr); and
//! hex, valid octal, and whitespace-padded spellings keep their
//! shell numeric meaning unreproduced.

use std::collections::HashMap;

use crate::progress_ui::Palette;

/// Canonical display order of the dependency groups dot knows about,
/// like `_SHDEPS_KNOWN_GROUPS`. Groups a newer shdeps emits that are
/// not listed here still render, appended in discovery order after
/// these, exactly like the shell brace of the known list with the
/// order array.
pub const KNOWN_GROUPS: &[&[u8]] = &[
    b"packages",
    b"github-releases",
    b"github-repos",
    b"cargo",
    b"go",
    b"uv",
    b"npm",
    b"custom",
    b"other",
];

/// Session globals this family writes, mirroring the
/// `DOT_UI_SHDEPS_*` scalars `_shdeps_ui_reset` establishes. The
/// group maps the reset also clears belong to the part-1 record
/// (the unmerged `rust-port-slice-44` lane's `State::new()`); the
/// prompt acknowledgment descriptor belongs to the FIFO lane, which
/// passes its raw value into [`prompt_pause`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// `DOT_UI_SHDEPS_STATUS`, consumed by `update.sh` after shdeps
    /// exits.
    pub status: Vec<u8>,
    /// `DOT_UI_SHDEPS_SUMMARY`, consumed by `update.sh` after shdeps
    /// exits.
    pub summary: Vec<u8>,
    /// `DOT_UI_SHDEPS_HAS_JQ`, from the `command -v jq` probe (see
    /// [`have_jq`]).
    pub has_jq: bool,
    /// `DOT_UI_SHDEPS_PROMPT_ACTIVE`, set while shdeps waits for the
    /// prompt acknowledgment.
    pub prompt_active: bool,
}

/// Threaded `_ui_*` environment shared by every renderer below: the
/// palette slots, the quiet gate, and the locale counting mode.
/// Production resolves `multibyte` with
/// [`crate::progress_ui::utf8_locale`]; rows pin `false` under the
/// `LC_ALL=C` harness, where the shell counts bytes too.
#[derive(Debug, Clone, Copy)]
pub struct Ui<'a> {
    /// The nine `_C_*` palette slots.
    pub palette: &'a Palette,
    /// The `DOT_QUIET` gate every `_ui_*` primitive honors.
    pub quiet: bool,
    /// Byte (`false`) or character (`true`) cell counting.
    pub multibyte: bool,
}

/// Whether `jq` resolves on `path_dirs`, like the `command -v jq`
/// probe inside `_shdeps_ui_reset`. Each colon-separated entry is
/// tried in order; an entry hits when `jq` under it exists and is
/// not a directory, matching the probed default-mode shell rows: a
/// non-executable or unreadable file still counts, as does a fifo
/// or a symlink to a file, while directories, symlinks to
/// directories, and broken symlinks never do. POSIX mode would
/// additionally require executability, but the engine never enables
/// it, so only the default read is modeled. Empty entries, which
/// the shell reads as the working directory, stay shell-side and
/// never hit here; function and alias shadows do not exist in the
/// engine, so only `PATH` entries are modeled.
pub fn have_jq(path_dirs: &str) -> bool {
    path_dirs.split(':').any(|dir| {
        if dir.is_empty() {
            return false;
        }
        match std::fs::metadata(std::path::Path::new(dir).join("jq")) {
            Ok(meta) => !meta.file_type().is_dir(),
            Err(_) => false,
        }
    })
}

/// `_shdeps_ui_reset`: fresh session globals for one update run:
/// status `ok`, the `dependencies checked` summary, the jq probe
/// result, and an inactive prompt (the shell's `_shdeps_prompt_resume`
/// folded in). The caller supplies the probe via [`have_jq`] so the
/// reset stays pure; the group maps restart empty beside this on the
/// part-1 record lane.
pub fn reset(has_jq: bool) -> Session {
    Session {
        status: b"ok".to_vec(),
        summary: b"dependencies checked".to_vec(),
        has_jq,
        prompt_active: false,
    }
}

/// `_shdeps_prompt_pause`: mark the prompt active, clear any live
/// row, and offer the cross-project `ready` token. Returns the
/// stdout bytes with the new live flag, plus the acknowledgment
/// bytes when `ack_fd` — the raw `DOT_UI_SHDEPS_PROMPT_ACK_FD`
/// value — is all digits like the shell `^[0-9]+$` gate; the caller
/// writes those bytes to the descriptor, so a closed descriptor
/// surfaces as caller IO while the flag set stays the contract.
pub fn prompt_pause(
    session: &mut Session,
    live_active: bool,
    ack_fd: &str,
) -> (Vec<u8>, bool, Option<Vec<u8>>) {
    session.prompt_active = true;
    let (out, live_active) = crate::progress_ui::clear_live(live_active);
    let ack = if !ack_fd.is_empty() && ack_fd.bytes().all(|b| b.is_ascii_digit()) {
        Some(b"ready\n".to_vec())
    } else {
        None
    };
    (out, live_active, ack)
}

/// `_shdeps_prompt_resume`: mark the prompt inactive again, like the
/// shell assignment.
pub fn prompt_resume(session: &mut Session) {
    session.prompt_active = false;
}

/// The shell `[[ "$seen" == *" $group "* ]]` dedup gate: whether the
/// space-padded `seen` list already holds `group`. The group travels
/// inside the quotes, so its bytes match literally even when they
/// spell glob metacharacters: the port is a plain substring search
/// for the space-framed group.
fn seen_contains(seen: &[u8], group: &[u8]) -> bool {
    let mut needle = Vec::with_capacity(group.len() + 2);
    needle.push(b' ');
    needle.extend_from_slice(group);
    needle.push(b' ');
    seen.windows(needle.len())
        .any(|window| window == needle.as_slice())
}

/// Canonical shell-arithmetic integer: an optional `-` with digits,
/// no leading zeros unless the value is zero itself, fitting `i64`.
/// Anything else (identifiers, hex, octal-looking, whitespace, `+`,
/// overflow) reads `None` and renders or compares through the
/// documented fallbacks instead of the shell arithmetic the harness
/// never feeds these paths.
fn parse_count(raw: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(raw).ok()?;
    let negative = text.starts_with('-');
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    // A signed zero still renders raw on the shell (`-0ms`), so it
    // never takes the canonical branch here.
    if negative && digits.bytes().all(|b| b == b'0') {
        return None;
    }
    text.parse().ok()
}

/// `_ui_duration_ms` over the raw elapsed bytes the summary record
/// stores. Canonical values delegate to
/// [`crate::progress_ui::duration_ms`]; anything else falls to the
/// shell's trailing `printf '%sms'` arm verbatim, and the empty
/// default (`${1:-0}`) renders `0ms` through the same path.
fn render_elapsed(raw: &[u8]) -> Vec<u8> {
    match parse_count(raw) {
        Some(ms) => crate::progress_ui::duration_ms(ms),
        None => {
            let mut out = raw.to_vec();
            out.extend_from_slice(b"ms");
            out
        }
    }
}

/// One `IFS=$'\t' read -r` over a blob line: status, name, and
/// detail. The tab is IFS whitespace here, not a plain delimiter:
/// leading and trailing tab runs never start or end a field, runs
/// between assigned fields collapse, and only the last field keeps
/// its interior tabs verbatim. Missing columns read empty, like
/// short shell reads.
fn split_row(line: &[u8]) -> (&[u8], &[u8], &[u8]) {
    let mut cursor = line;
    while cursor.first() == Some(&b'\t') {
        cursor = &cursor[1..];
    }
    fn take<'a>(cursor: &mut &'a [u8]) -> &'a [u8] {
        match cursor.iter().position(|b| *b == b'\t') {
            None => {
                let field = *cursor;
                *cursor = b"";
                field
            }
            Some(ix) => {
                let field = &cursor[..ix];
                let mut rest = &cursor[ix..];
                while rest.first() == Some(&b'\t') {
                    rest = &rest[1..];
                }
                *cursor = rest;
                field
            }
        }
    }
    let status = take(&mut cursor);
    let name = take(&mut cursor);
    let mut detail = cursor;
    while detail.last() == Some(&b'\t') {
        detail = &detail[..detail.len() - 1];
    }
    (status, name, detail)
}

/// First blob line only, like the herestring-fed `read` the summary
/// printer parses its record with.
fn first_line(blob: &[u8]) -> &[u8] {
    match blob.iter().position(|b| *b == b'\n') {
        Some(ix) => &blob[..ix],
        None => blob,
    }
}

/// The recorded display label for `group`, or `fallback` when the
/// group has no recorded non-empty label, like the shell
/// `${...:-...}` fallback in `_shdeps_display_label`. The fallback
/// vocabulary (the part-1 `_shdeps_group_label`) arrives injected so
/// this module never re-types the unmerged record lane; once the
/// lanes stack, the caller passes that lane's display resolver here.
fn display_label(
    labels: &HashMap<Vec<u8>, Vec<u8>>,
    fallback: &dyn Fn(&[u8]) -> Vec<u8>,
    group: &[u8],
) -> Vec<u8> {
    match labels.get(group) {
        Some(label) if !label.is_empty() => label.clone(),
        _ => fallback(group),
    }
}

/// Known groups first, then discovery order, skipping repeats with
/// the shell dedup gate. The callback sees each surviving group
/// once, in shell order.
fn each_group(order: &[Vec<u8>], mut visit: impl FnMut(&[u8])) {
    let mut seen = vec![b' '];
    let mut one = |group: &[u8]| {
        if seen_contains(&seen, group) {
            return;
        }
        seen.extend_from_slice(group);
        seen.push(b' ');
        visit(group);
    };
    for known in KNOWN_GROUPS {
        one(known);
    }
    for group in order {
        one(group);
    }
}

/// Whether the threshold branch fires: both sides parse canonical
/// and elapsed reaches the threshold, like the shell
/// `[[ -n "$threshold" && "$elapsed_ms" -ge "$threshold" ]]`. Any
/// unparsable side reads false, matching the shell except for
/// identifier-like spellings against a non-positive threshold, where
/// the shell's unset-variable read fires the branch (the module
/// header draws that rowed boundary). A shell arithmetic diagnostic,
/// if any, is not part of the contract.
fn threshold_hit(elapsed_ms: &[u8], threshold: &[u8]) -> bool {
    match (parse_count(elapsed_ms), parse_count(threshold)) {
        (Some(elapsed), Some(limit)) => elapsed >= limit,
        _ => false,
    }
}

/// Emit one `_ui_item` row, threading the live flag, with the empty
/// detail taking the short shell form.
fn emit_item(
    ui: &Ui<'_>,
    live_active: bool,
    out: &mut Vec<u8>,
    status: &[u8],
    name: &[u8],
    detail: &[u8],
) -> bool {
    let detail = if detail.is_empty() {
        None
    } else {
        Some(detail)
    };
    let (bytes, live_active) = crate::progress_ui::item(
        ui.palette,
        ui.quiet,
        live_active,
        status,
        name,
        detail,
        ui.multibyte,
    );
    out.extend_from_slice(&bytes);
    live_active
}

/// `_shdeps_print_verbose_group_rows`: the item rows of every group
/// whose display label is `label`, in known-plus-discovery order.
/// Groups without rows stay silent, and lines with an empty status
/// (the herestring's trailing read) are skipped, like the shell.
pub fn print_verbose_group_rows(
    ui: &Ui<'_>,
    live_active: bool,
    order: &[Vec<u8>],
    items: &HashMap<Vec<u8>, Vec<u8>>,
    labels: &HashMap<Vec<u8>, Vec<u8>>,
    fallback: &dyn Fn(&[u8]) -> Vec<u8>,
    label: &[u8],
) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut live_active = live_active;
    each_group(order, |group| {
        if display_label(labels, fallback, group) != label {
            return;
        }
        let blob = match items.get(group) {
            Some(blob) if !blob.is_empty() => blob,
            _ => return,
        };
        for line in blob.split(|b| *b == b'\n') {
            let (status, name, detail) = split_row(line);
            if status.is_empty() {
                continue;
            }
            live_active = emit_item(ui, live_active, &mut out, status, name, detail);
        }
    });
    (out, live_active)
}

/// `_shdeps_print_verbose_items`: one `_ui_section` per display
/// label with rows, merging groups that share a label (the two
/// GitHub groups render one section) in first-seen order. Silent
/// unless `verbose` (the `DOT_VERBOSE` `-eq 1` read), returning the
/// live flag untouched then, like the shell early return.
pub fn print_verbose_items(
    ui: &Ui<'_>,
    live_active: bool,
    verbose: bool,
    order: &[Vec<u8>],
    items: &HashMap<Vec<u8>, Vec<u8>>,
    labels: &HashMap<Vec<u8>, Vec<u8>>,
    fallback: &dyn Fn(&[u8]) -> Vec<u8>,
) -> (Vec<u8>, bool) {
    if !verbose {
        return (Vec::new(), live_active);
    }
    let mut out = Vec::new();
    let mut live_active = live_active;
    // Seen labels framed in newlines, starting from one newline like
    // the shell `$'\n'` seed, so the containment read stays a
    // literal substring search (the label is quoted in the shell).
    let mut seen_labels = vec![b'\n'];
    each_group(order, |group| {
        if items.get(group).is_none_or(Vec::is_empty) {
            return;
        }
        let label = display_label(labels, fallback, group);
        let mut needle = Vec::with_capacity(label.len() + 2);
        needle.push(b'\n');
        needle.extend_from_slice(&label);
        needle.push(b'\n');
        if seen_labels
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
        {
            return;
        }
        seen_labels.extend_from_slice(&label);
        seen_labels.push(b'\n');
        let (bytes, live) =
            crate::progress_ui::section(ui.palette, ui.quiet, live_active, &label, ui.multibyte);
        out.extend_from_slice(&bytes);
        live_active = live;
        let (rows, live) =
            print_verbose_group_rows(ui, live, order, items, labels, fallback, &label);
        out.extend_from_slice(&rows);
        live_active = live;
    });
    (out, live_active)
}

/// `_shdeps_print_group_items_with_status`: the item rows of `group`
/// whose status is `wanted`, in blob order. A missing or empty blob
/// stays silent, like the shell `[[ -n "$rows" ]]` gate. Unlike the
/// verbose row printer there is no empty-status skip: the shell
/// compares every line, so an empty `wanted` would match the
/// trailing read too.
pub fn print_group_items_with_status(
    ui: &Ui<'_>,
    live_active: bool,
    items: &HashMap<Vec<u8>, Vec<u8>>,
    group: &[u8],
    wanted: &[u8],
) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut live_active = live_active;
    let blob = match items.get(group) {
        Some(blob) if !blob.is_empty() => blob,
        _ => return (out, live_active),
    };
    for line in blob.split(|b| *b == b'\n') {
        let (status, name, detail) = split_row(line);
        if status != wanted {
            continue;
        }
        live_active = emit_item(ui, live_active, &mut out, status, name, detail);
    }
    (out, live_active)
}

/// `_shdeps_print_group_summaries`: the non-verbose per-group stage
/// notes with their actionable item rows. `verbose` is the same
/// `DOT_VERBOSE` read as the verbose printer, inverted: a verbose
/// run returns silent. `threshold` is the raw
/// `DOT_UPDATE_SUBPHASE_THRESHOLD_MS` value (`None` when unset or
/// empty); a group reaching it always reports its note with the
/// elapsed time, even `ok` ones. Otherwise `changed` reports plainly
/// with its rows, `ok` and `skipped` stay silent, and anything else
/// reports with elapsed plus the `failed` or `warning` rows. Only
/// the record's first line parses, like the herestring-fed shell
/// read, and a missing elapsed defaults to `0`.
pub fn print_group_summaries(
    ui: &Ui<'_>,
    live_active: bool,
    verbose: bool,
    threshold: Option<&[u8]>,
    order: &[Vec<u8>],
    summaries: &HashMap<Vec<u8>, Vec<u8>>,
    items: &HashMap<Vec<u8>, Vec<u8>>,
) -> (Vec<u8>, bool) {
    if verbose {
        return (Vec::new(), live_active);
    }
    let threshold = threshold.filter(|limit| !limit.is_empty());
    let mut out = Vec::new();
    let mut live_active = live_active;
    each_group(order, |group| {
        let record = match summaries.get(group) {
            Some(record) if !record.is_empty() => record,
            _ => return,
        };
        let (status, detail, elapsed_raw) = split_row(first_line(record));
        let elapsed_ms = if elapsed_raw.is_empty() {
            b"0".as_slice()
        } else {
            elapsed_raw
        };
        let over = match threshold {
            Some(limit) => threshold_hit(elapsed_ms, limit),
            None => false,
        };
        if over {
            live_active = emit_timed(ui, live_active, &mut out, status, detail, elapsed_ms);
            live_active = emit_wanted(ui, live_active, &mut out, items, group, status);
            return;
        }
        match status {
            b"changed" => {
                let (bytes, live) = crate::progress_ui::status(
                    ui.palette,
                    ui.quiet,
                    live_active,
                    status,
                    detail,
                    ui.multibyte,
                );
                out.extend_from_slice(&bytes);
                live_active = live;
                let (rows, live) =
                    print_group_items_with_status(ui, live_active, items, group, b"changed");
                out.extend_from_slice(&rows);
                live_active = live;
            }
            b"ok" | b"skipped" => {}
            _ => {
                live_active = emit_timed(ui, live_active, &mut out, status, detail, elapsed_ms);
                live_active = emit_wanted(ui, live_active, &mut out, items, group, status);
            }
        }
    });
    (out, live_active)
}

/// One `_ui_stage_note` with the `detail, elapsed` shape the summary
/// printer renders, threading the live flag.
fn emit_timed(
    ui: &Ui<'_>,
    live_active: bool,
    out: &mut Vec<u8>,
    status: &[u8],
    detail: &[u8],
    elapsed_ms: &[u8],
) -> bool {
    let mut full = detail.to_vec();
    full.extend_from_slice(b", ");
    full.extend_from_slice(&render_elapsed(elapsed_ms));
    let (bytes, live_active) = crate::progress_ui::status(
        ui.palette,
        ui.quiet,
        live_active,
        status,
        &full,
        ui.multibyte,
    );
    out.extend_from_slice(&bytes);
    live_active
}

/// The actionable item rows a summary status unfolds: `changed`
/// replays changed rows, `failed` failed rows, `warning` warning
/// rows, and anything else replays nothing, like the shell guards.
fn emit_wanted(
    ui: &Ui<'_>,
    live_active: bool,
    out: &mut Vec<u8>,
    items: &HashMap<Vec<u8>, Vec<u8>>,
    group: &[u8],
    status: &[u8],
) -> bool {
    let wanted = match status {
        b"changed" => Some(b"changed".as_slice()),
        b"failed" => Some(b"failed".as_slice()),
        b"warning" => Some(b"warning".as_slice()),
        _ => None,
    };
    match wanted {
        Some(wanted) => {
            let (rows, live_active) =
                print_group_items_with_status(ui, live_active, items, group, wanted);
            out.extend_from_slice(&rows);
            live_active
        }
        None => live_active,
    }
}
