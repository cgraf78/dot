//! Doctor result rendering and counters.
//!
//! Owns the `ok` / `warn` / `fail` / `skip` result lines, the
//! `section` titles, and the pass/warn/fail counters the section
//! modules report through. Path, repository, lock, provider, overlay, merge,
//! and coordinator modules
//! call into this API instead of reimplementing the layout.
//!
//! Text flows as bytes: `printf '%s'` copies its arguments verbatim,
//! so messages and details travel as `&[u8]` and compare exactly,
//! including empty strings, tabs, and multibyte glyphs. Whether the
//! detail trailer renders at all depends on the call arity — the
//! shell tests `$#` — so callers pass `None` for one-argument calls
//! and `Some` (possibly empty) for two-argument calls, exactly like
//! `_dot_doctor_render_records` always passing `$detail` through.
//! Colors travel with the call in [`Palette`] so these helpers stay
//! pure; production resolves it with [`resolve_palette`], tests with
//! marker strings.

/// One doctor result row, mirroring a single `_dr_*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Which `_dr_*` helper filed this row.
    pub kind: Kind,
    /// The message verbatim (`$1`), as bytes.
    pub message: Vec<u8>,
    /// The detail verbatim (`$2`), or `None` for one-argument calls.
    pub detail: Option<Vec<u8>>,
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

    fn text(kind: Kind, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind,
            message: message.into().into_bytes(),
            detail: detail.map(String::into_bytes),
        }
    }
}

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

/// The six `_DR_*` color slots from `lib/dot/doctor/runtime.sh`,
/// resolved by the caller (empty under pipes, ANSI escapes on a
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
        };
        out.extend_from_slice(&row);
    }
    out
}

/// `_dr_ok`: `  ✓ message [ (detail)]`, then the pass count goes up
/// by one. `detail` is `None` for one-argument calls; `Some` —
/// even empty — renders the ` (detail)` trailer, like `$# -gt 1`.
pub fn ok(
    counts: &mut Counts,
    palette: &Palette,
    message: &[u8],
    detail: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"  ");
    out.extend_from_slice(palette.green.as_bytes());
    out.extend_from_slice("✓".as_bytes());
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(message);
    if let Some(detail) = detail {
        out.push(b' ');
        out.extend_from_slice(palette.dim.as_bytes());
        out.push(b'(');
        out.extend_from_slice(detail);
        out.push(b')');
        out.extend_from_slice(palette.reset.as_bytes());
    }
    out.push(b'\n');
    counts.pass += 1;
    out
}

/// `_dr_warn`: `  ⚠ message` plus, for two-argument calls, an
/// indented dim detail line; then the warn count goes up by one.
/// An empty `Some` still emits the bare indented line, like the
/// shell's unconditional `printf '\n    %s%s%s'` arm.
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
    if let Some(detail) = detail {
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

/// `_dr_fail`: `  ✗ message` plus, for two-argument calls, an
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
    if let Some(detail) = detail {
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
    let mut out = Vec::new();
    out.extend_from_slice(b"  ");
    out.extend_from_slice(palette.dim.as_bytes());
    out.extend_from_slice("·".as_bytes());
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(message);
    if let Some(detail) = detail {
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
