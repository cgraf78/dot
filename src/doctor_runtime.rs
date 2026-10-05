//! Doctor result rendering and counters.
//!
//! Owns the `ok` / `warn` / `fail` / `skip` / `info` result lines, the
//! `section` titles, and the pass/warn/fail counters the section
//! modules report through. Path, repository, lock, provider, overlay, merge,
//! and coordinator modules
//! call into this API instead of reimplementing the layout.
//!
//! Text flows as bytes: `printf '%s'` copies its arguments verbatim,
//! so messages and details travel as `&[u8]` and compare exactly,
//! including empty strings, tabs, and multibyte glyphs. A detail renders
//! only when it is present and non-empty: extensions routinely pass an
//! empty second argument (`dot_doctor_ok LABEL "$detail"` with nothing in
//! `$detail`), and the shell-era renderer turned that into a bare `()`
//! trailer or an empty indented line. Colors travel with the call in
//! [`Palette`] so these helpers stay pure; production resolves it with
//! [`resolve_palette`], tests with marker strings.
//!
//! A verdict row may carry attachments that render on their own lines below
//! it: list items (`    - item`, at most [`ITEM_LIMIT`] before a `+N more`
//! line) and next-step hints (`    → hint`). They keep long lists and the
//! "what to do" text out of the one-line detail, and they never count.

/// One doctor result row, mirroring a single `_dr_*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Which `_dr_*` helper filed this row.
    pub kind: Kind,
    /// The message verbatim (`$1`), as bytes.
    pub message: Vec<u8>,
    /// The detail verbatim (`$2`), or `None` for one-argument calls.
    pub detail: Option<Vec<u8>>,
    /// List items attached to this row (`dot_doctor_item`), in filing
    /// order. Rendered one per line below the row; never counted.
    pub items: Vec<Vec<u8>>,
    /// Further items that exist but are not attached (a record that kept
    /// only the first few), folded into the `+N more` line. Core rows only;
    /// the extension API has no way to set it.
    pub omitted: usize,
    /// Next steps attached to this row (`dot_doctor_hint`), in filing
    /// order. Rendered after the items; never counted.
    pub hints: Vec<Vec<u8>>,
}

impl Record {
    /// Build a section record from the text-oriented check modules.
    pub fn section(message: impl Into<String>) -> Self {
        Self::text(Kind::Section, message, None)
    }

    /// Build a passing record from the text-oriented check modules.
    pub fn ok(message: impl Into<String>, detail: Option<String>) -> Self {
        Self::text(Kind::Ok, message, detail)
    }

    /// Build a warning record from the text-oriented check modules.
    pub fn warn(message: impl Into<String>, detail: Option<String>) -> Self {
        Self::text(Kind::Warn, message, detail)
    }

    /// Build a failing record from the text-oriented check modules.
    pub fn fail(message: impl Into<String>, detail: Option<String>) -> Self {
        Self::text(Kind::Fail, message, detail)
    }

    /// Build a skipped record from the text-oriented check modules.
    pub fn skip(message: impl Into<String>, detail: Option<String>) -> Self {
        Self::text(Kind::Skip, message, detail)
    }

    /// Build an informational record from the text-oriented check modules.
    pub fn info(message: impl Into<String>, detail: Option<String>) -> Self {
        Self::text(Kind::Info, message, detail)
    }

    /// Attach one list item, rendered on its own line below the row.
    pub fn with_item(mut self, item: impl Into<String>) -> Self {
        self.items.push(item.into().into_bytes());
        self
    }

    /// Attach list items in order, each rendered on its own line below the
    /// row; past [`ITEM_LIMIT`] the rest fold into one `+N more` line.
    pub fn with_items<I>(mut self, items: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.items
            .extend(items.into_iter().map(|item| item.into().into_bytes()));
        self
    }

    /// Count `omitted` further items that are not attached, so the `+N
    /// more` line covers them too.
    pub fn with_omitted(mut self, omitted: usize) -> Self {
        self.omitted += omitted;
        self
    }

    /// Attach a next step, rendered as a `→` line after the items.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into().into_bytes());
        self
    }

    /// A bare row of `kind` carrying `message` and `detail` as bytes.
    pub fn bytes(kind: Kind, message: &[u8], detail: Option<&[u8]>) -> Self {
        Self {
            kind,
            message: message.to_vec(),
            detail: detail.map(<[u8]>::to_vec),
            items: Vec::new(),
            omitted: 0,
            hints: Vec::new(),
        }
    }

    fn text(kind: Kind, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind,
            message: message.into().into_bytes(),
            detail: detail.map(String::into_bytes),
            items: Vec::new(),
            omitted: 0,
            hints: Vec::new(),
        }
    }
}

/// Items shown below one row before the rest fold into `+N more`: enough to
/// name the first few offenders, short enough that one row with hundreds of
/// entries cannot bury the rest of the report.
pub const ITEM_LIMIT: usize = 5;

/// The `_dr_*` helper family a [`Record`] was filed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `_dr_section`: a section title, never counted.
    Section,
    /// `_dr_ok`: a passing check, bumps the pass count.
    Ok,
    /// `_dr_warn`: a warning, bumps the warn count.
    Warn,
    /// `_dr_fail`: a failure, bumps the fail count.
    Fail,
    /// `_dr_skip`: a skipped check, never counted.
    Skip,
    /// An informational row (a configuration fact such as the selected
    /// profile), never counted: it is neither a passed check nor a skipped
    /// one.
    Info,
    /// An extension result kind that the public API does not define.
    Unknown,
}

/// The `_DR_*_COUNT` aggregate counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// `_DR_PASS_COUNT`, incremented by [`ok`].
    pub pass: u64,
    /// `_DR_WARN_COUNT`, incremented by [`warn`].
    pub warn: u64,
    /// `_DR_FAIL_COUNT`, incremented by [`fail`].
    pub fail: u64,
}

impl Counts {
    /// Zeroed counters, like a freshly sourced `runtime.sh`.
    pub fn new() -> Self {
        Counts::default()
    }
}

/// The six `_DR_*` color slots of the former shell renderer
/// (`lib/dot/doctor/runtime.sh`), resolved by the caller (empty under pipes, ANSI escapes on a
/// color terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// `$_DR_GREEN`: the `ok` glyph slot.
    pub green: String,
    /// `$_DR_YELLOW`: the `warn` glyph slot.
    pub yellow: String,
    /// `$_DR_RED`: the `fail` glyph slot.
    pub red: String,
    /// `$_DR_DIM`: detail trailers and the `skip` glyph slot.
    pub dim: String,
    /// `$_DR_BOLD`: section titles.
    pub bold: String,
    /// `$_DR_RESET`: closes every colored span.
    pub reset: String,
}

impl Palette {
    /// Every slot empty, like a sourced `runtime.sh` under a pipe.
    pub fn empty() -> Self {
        Palette {
            green: String::new(),
            yellow: String::new(),
            red: String::new(),
            dim: String::new(),
            bold: String::new(),
            reset: String::new(),
        }
    }

    /// The ANSI slots `runtime.sh` installs on a color terminal.
    pub fn ansi() -> Self {
        Palette {
            green: "\x1b[32m".to_string(),
            yellow: "\x1b[33m".to_string(),
            red: "\x1b[31m".to_string(),
            dim: "\x1b[2m".to_string(),
            bold: "\x1b[1m".to_string(),
            reset: "\x1b[0m".to_string(),
        }
    }
}

/// Resolve the [`Palette`] the way `runtime.sh` does at source time:
/// colors exactly when stdout is a terminal and `NO_COLOR` is unset
/// or empty (`[[ -t 1 && -z "${NO_COLOR:-}" ]]`). `no_color` mirrors
/// the variable (`None` when unset); production passes the live
/// terminal probe, tests pass literals.
pub fn resolve_palette(stdout_is_tty: bool, no_color: Option<&str>) -> Palette {
    let colored = stdout_is_tty && no_color.is_none_or(|value| value.is_empty());
    if colored {
        Palette::ansi()
    } else {
        Palette::empty()
    }
}

/// Render canonical records with one palette and one counts model.
pub fn render(records: &[Record], palette: &Palette) -> Vec<u8> {
    let mut counts = Counts::new();
    let mut out = Vec::new();
    for record in records {
        let row = match record.kind {
            Kind::Section => section(palette, &record.message),
            Kind::Ok => ok(
                &mut counts,
                palette,
                &record.message,
                record.detail.as_deref(),
            ),
            Kind::Warn => warn(
                &mut counts,
                palette,
                &record.message,
                record.detail.as_deref(),
            ),
            Kind::Fail | Kind::Unknown => fail(
                &mut counts,
                palette,
                &record.message,
                record.detail.as_deref(),
            ),
            Kind::Skip => skip(palette, &record.message, record.detail.as_deref()),
            Kind::Info => info(palette, &record.message, record.detail.as_deref()),
        };
        out.extend_from_slice(&row);
        out.extend_from_slice(&attachments(
            palette,
            &record.items,
            record.omitted,
            &record.hints,
        ));
    }
    out
}

/// The lines below one row: up to [`ITEM_LIMIT`] dim `- item` lines, a dim
/// `+N more` line for the rest and the `omitted` ones, then one `→ hint`
/// line per hint. Empty entries are dropped, like an empty detail.
/// Rendering stays per record, so streaming filed prefixes still
/// concatenates to the whole report.
pub fn attachments(
    palette: &Palette,
    items: &[Vec<u8>],
    omitted: usize,
    hints: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::new();
    let items: Vec<&Vec<u8>> = items.iter().filter(|item| !item.is_empty()).collect();
    for item in items.iter().take(ITEM_LIMIT) {
        out.extend_from_slice(b"    ");
        out.extend_from_slice(palette.dim.as_bytes());
        out.extend_from_slice(b"- ");
        out.extend_from_slice(item);
        out.extend_from_slice(palette.reset.as_bytes());
        out.push(b'\n');
    }
    let more = items.len().saturating_sub(ITEM_LIMIT) + omitted;
    if more > 0 {
        out.extend_from_slice(b"    ");
        out.extend_from_slice(palette.dim.as_bytes());
        out.extend_from_slice(format!("+{more} more").as_bytes());
        out.extend_from_slice(palette.reset.as_bytes());
        out.push(b'\n');
    }
    for hint in hints.iter().filter(|hint| !hint.is_empty()) {
        out.extend_from_slice("    → ".as_bytes());
        out.extend_from_slice(hint);
        out.push(b'\n');
    }
    out
}

/// A detail worth rendering: present and non-empty.
fn shown(detail: Option<&[u8]>) -> Option<&[u8]> {
    detail.filter(|detail| !detail.is_empty())
}

/// One `  <glyph> message[ (detail)]` row, the inline-detail layout shared
/// by `ok`, `skip`, and `info`.
fn inline_row(
    palette: &Palette,
    color: &str,
    glyph: &str,
    message: &[u8],
    detail: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"  ");
    out.extend_from_slice(color.as_bytes());
    out.extend_from_slice(glyph.as_bytes());
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(message);
    if let Some(detail) = shown(detail) {
        out.push(b' ');
        out.extend_from_slice(palette.dim.as_bytes());
        out.push(b'(');
        out.extend_from_slice(detail);
        out.push(b')');
        out.extend_from_slice(palette.reset.as_bytes());
    }
    out.push(b'\n');
    out
}

/// `_dr_ok`: `  ✓ message [ (detail)]`, then the pass count goes up
/// by one. The ` (detail)` trailer renders only for a non-empty detail.
pub fn ok(
    counts: &mut Counts,
    palette: &Palette,
    message: &[u8],
    detail: Option<&[u8]>,
) -> Vec<u8> {
    counts.pass += 1;
    inline_row(palette, &palette.green, "✓", message, detail)
}

/// `_dr_warn`: `  ⚠ message` plus, for a non-empty detail, an
/// indented dim detail line; then the warn count goes up by one.
pub fn warn(
    counts: &mut Counts,
    palette: &Palette,
    message: &[u8],
    detail: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"  ");
    out.extend_from_slice(palette.yellow.as_bytes());
    out.extend_from_slice("⚠".as_bytes());
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(message);
    if let Some(detail) = shown(detail) {
        out.push(b'\n');
        out.extend_from_slice(b"    ");
        out.extend_from_slice(palette.dim.as_bytes());
        out.extend_from_slice(detail);
        out.extend_from_slice(palette.reset.as_bytes());
    }
    out.push(b'\n');
    counts.warn += 1;
    out
}

/// `_dr_fail`: `  ✗ message` plus, for a non-empty detail, an
/// indented dim detail line; then the fail count goes up by one.
/// Layout mirrors [`warn`]; only the glyph slot and the counter
/// differ.
pub fn fail(
    counts: &mut Counts,
    palette: &Palette,
    message: &[u8],
    detail: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"  ");
    out.extend_from_slice(palette.red.as_bytes());
    out.extend_from_slice("✗".as_bytes());
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(message);
    if let Some(detail) = shown(detail) {
        out.push(b'\n');
        out.extend_from_slice(b"    ");
        out.extend_from_slice(palette.dim.as_bytes());
        out.extend_from_slice(detail);
        out.extend_from_slice(palette.reset.as_bytes());
    }
    out.push(b'\n');
    counts.fail += 1;
    out
}

/// `_dr_skip`: `  · message [ (detail)]`, with the same trailer rule
/// as [`ok`]. Skips never touch [`Counts`].
pub fn skip(palette: &Palette, message: &[u8], detail: Option<&[u8]>) -> Vec<u8> {
    inline_row(palette, &palette.dim, "·", message, detail)
}

/// An informational row: `  › message [ (detail)]`, with the same trailer
/// rule as [`ok`]. Informational rows never touch [`Counts`]. The marker is
/// one column wide in every font (an information sign renders as a wide
/// emoji in some, misaligning the rows) and unlike the skip dot at a glance.
pub fn info(palette: &Palette, message: &[u8], detail: Option<&[u8]>) -> Vec<u8> {
    inline_row(palette, &palette.dim, "›", message, detail)
}

/// `_dr_section`: a blank line, then the bold title. Sections never
/// touch [`Counts`].
pub fn section(palette: &Palette, title: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(b'\n');
    out.extend_from_slice(palette.bold.as_bytes());
    out.extend_from_slice(title);
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b'\n');
    out
}
