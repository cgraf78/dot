//! Doctor result rendering and counters (slice 39: doctor layer, part 1).
//!
//! Ports the five reporting helpers from `lib/dot/doctor/runtime.sh`
//! exactly: the `ok` / `warn` / `fail` / `skip` result lines, the
//! `section` titles, and the pass/warn/fail counters the section
//! modules report through. Later doctor slices (paths, repos, lock,
//! provider, overlays, merges, and the `_dot_doctor` coordinator)
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

/// The `_DR_*_COUNT` aggregate counters. Section modules report
/// through [`ok`], [`warn`], and [`fail`]; [`skip`] and [`section`]
/// leave the counts alone, exactly like the shell.
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
        Counts {
            pass: 0,
            warn: 0,
            fail: 0,
        }
    }
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
