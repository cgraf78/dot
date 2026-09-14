//! Live-progress formatting leaves (slice 20).
//!
//! Ports the pure helpers from `lib/dot/progress-ui.sh` exactly:
//! status colors, ASCII detection, cell fitting, and the summary
//! phrases the update stages report through. Text flows as bytes:
//! bash counts string length in characters under a working UTF-8
//! locale and in bytes otherwise, so callers pass the counting mode
//! explicitly (production resolves it with [`utf8_locale`], tests
//! with the shell's own `${#glyph}` probe). Numeric inputs arrive
//! canonical from shell arithmetic upstream (`$(( ))` never emits
//! leading zeros or signs on these paths), matching the precedent in
//! `merges::summary`.

/// The nine `_C_*` palette slots from `lib/dot/log.sh`, resolved by
/// the caller (empty under pipes, ANSI escapes on a terminal).
/// Colors travel with the call so these helpers stay pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// `$_C_RESET`.
    pub reset: String,
    /// `$_C_BOLD`.
    pub bold: String,
    /// `$_C_DIM`.
    pub dim: String,
    /// `$_C_GREEN`.
    pub green: String,
    /// `$_C_YELLOW`.
    pub yellow: String,
    /// `$_C_RED`.
    pub red: String,
    /// `$_C_BLUE`.
    pub blue: String,
    /// `$_C_CYAN`.
    pub cyan: String,
    /// `$_C_WHITE`.
    pub white: String,
}

impl Palette {
    /// Every slot empty, like a sourced `log.sh` under a pipe.
    pub fn empty() -> Self {
        Palette {
            reset: String::new(),
            bold: String::new(),
            dim: String::new(),
            green: String::new(),
            yellow: String::new(),
            red: String::new(),
            blue: String::new(),
            cyan: String::new(),
            white: String::new(),
        }
    }
}

/// `_ui_color`: the palette slot for a stage status, empty for
/// anything else.
pub fn color<'a>(status: &[u8], palette: &'a Palette) -> &'a str {
    match status {
        b"ok" => &palette.green,
        b"changed" => &palette.blue,
        b"running" => &palette.cyan,
        b"warning" => &palette.yellow,
        b"failed" => &palette.red,
        b"detail" | b"hint" => &palette.dim,
        _ => "",
    }
}

/// Whether a locale name selects a usable UTF-8 charmap, resolving
/// the `multibyte` flag for production. The shell twin lives inside
/// bash (`${#glyph}` under the live locale), so this trusts the
/// name: empty never counts characters, and any other name counts
/// them exactly when it carries a UTF-8 designator (dash optional).
/// A name that merely claims UTF-8 without being installed is a
/// documented corner — bash falls back to byte counting there while
/// this rule still reports multibyte.
pub fn utf8_locale(name: &str) -> bool {
    !name.is_empty() && has_utf8_designator(name)
}

/// Case-insensitive `utf8` designator with the dash optional, so both
/// `C.UTF-8` (macOS) and `C.utf8` (glibc `locale -a`) match.
fn has_utf8_designator(name: &str) -> bool {
    let flat: String = name
        .chars()
        .filter(|c| *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    flat.contains("utf8")
}

/// `_ui_ascii_mode`: stable ASCII glyphs when forced, under a `C` or
/// `POSIX` locale, or whenever the locale cannot count the probe
/// glyph as one character. `dot_ui_ascii` mirrors `DOT_UI_ASCII`
/// with its `:-0` default (absent means off); `multibyte` is the live
/// counting mode from [`utf8_locale`] (production) or the shell
/// probe (tests). The flag and locale-prefix arms short-circuit
/// before the probe, exactly like the shell `&&` chain.
pub fn ascii_mode(dot_ui_ascii: Option<&str>, locale: &str, multibyte: bool) -> bool {
    crate::log::is_quiet(dot_ui_ascii) || locale.starts_with('C') || locale == "POSIX" || !multibyte
}

/// Split off the next shell-counted character: a strictly valid
/// UTF-8 sequence, or — like glibc `mbrtowc` failing with `EILSEQ` —
/// one raw byte. Strictness matters: overlongs, surrogates, and
/// codepoints past U+10FFFF count as bytes under a UTF-8 locale,
/// not as characters.
fn next_char(text: &[u8]) -> &[u8] {
    if text.is_empty() {
        return &[];
    }
    let first = text[0];
    if first < 0x80 {
        return &text[..1];
    }
    let width = match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return &text[..1],
    };
    if text.len() < width || !text[1..width].iter().all(|b| (0x80..=0xBF).contains(b)) {
        return &text[..1];
    }
    match (first, text.get(1)) {
        (0xE0, Some(second)) if *second < 0xA0 => return &text[..1],
        (0xED, Some(second)) if *second > 0x9F => return &text[..1],
        (0xF0, Some(second)) if *second < 0x90 => return &text[..1],
        (0xF4, Some(second)) if *second > 0x8F => return &text[..1],
        _ => {}
    }
    &text[..width]
}

/// String length the way bash `${#text}` measures it: characters when
/// `multibyte`, bytes otherwise.
fn measured_len(text: &[u8], multibyte: bool) -> usize {
    if !multibyte {
        return text.len();
    }
    let mut count = 0;
    let mut rest = text;
    while !rest.is_empty() {
        rest = &rest[next_char(rest).len()..];
        count += 1;
    }
    count
}

/// The first `width` characters (`multibyte`) or bytes, like
/// `${text:0:width}`. Byte truncation may split a UTF-8 sequence,
/// exactly like bash under a byte-counting locale.
fn take_prefix(text: &[u8], width: usize, multibyte: bool) -> Vec<u8> {
    if !multibyte {
        return text[..width.min(text.len())].to_vec();
    }
    let mut out = Vec::new();
    let mut rest = text;
    for _ in 0..width {
        if rest.is_empty() {
            break;
        }
        let taken = next_char(rest);
        out.extend_from_slice(taken);
        rest = &rest[taken.len()..];
    }
    out
}

/// `_ui_fit`: pad `text` with spaces to `width`, or — with
/// `truncate` — cut it to `width`. Over-width text without
/// truncation passes through intact; zero width truncates any
/// non-empty text to empty. Widths arrive as positive constants
/// from the callers, so only `usize` is modeled.
pub fn fit(text: &[u8], width: usize, truncate: bool, multibyte: bool) -> Vec<u8> {
    let len = measured_len(text, multibyte);
    if len >= width {
        if truncate && len > width {
            take_prefix(text, width, multibyte)
        } else {
            text.to_vec()
        }
    } else {
        let mut out = text.to_vec();
        out.extend(std::iter::repeat_n(b' ', width - len));
        out
    }
}

/// `_ui_cell`: fixed-width cell, pad or truncate.
pub fn cell(text: &[u8], width: usize, multibyte: bool) -> Vec<u8> {
    fit(text, width, true, multibyte)
}

/// `_ui_pad`: minimum-width pad, never truncate.
pub fn pad(text: &[u8], width: usize, multibyte: bool) -> Vec<u8> {
    fit(text, width, false, multibyte)
}

/// `_join_comma`: join non-empty items with `", "`.
pub fn join_comma(items: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        if item.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(item);
    }
    out
}

/// `_ui_count_phrase`: `1 repo` / `N repos`, with an explicit plural
/// form when provided (`None` appends `s`, like `${3:-${singular}s}`
/// with an unset `$3`).
pub fn count_phrase(count: i64, singular: &[u8], plural: Option<&[u8]>) -> Vec<u8> {
    if count == 1 {
        let mut out = b"1 ".to_vec();
        out.extend_from_slice(singular);
        out
    } else {
        let mut out = count.to_string().into_bytes();
        out.push(b' ');
        match plural {
            Some(form) => out.extend_from_slice(form),
            None => {
                out.extend_from_slice(singular);
                out.push(b's');
            }
        }
        out
    }
}

/// `_ui_duration_ms`: `999ms`, `1.5s`, `12s`, rounding half up at ten
/// seconds and over. Non-negative inputs take the literal arithmetic
/// branches; anything below a second prints raw.
pub fn duration_ms(ms: i64) -> Vec<u8> {
    if ms >= 10_000 {
        format!("{}s", (ms + 500) / 1000).into_bytes()
    } else if ms >= 1000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100).into_bytes()
    } else {
        format!("{ms}ms").into_bytes()
    }
}

/// `_ui_elapsed`: the `SECONDS`-style difference rendered as `12s`.
/// The clock read stays with the caller (tests cannot pin `$SECONDS`,
/// production passes its own now/started pair).
pub fn elapsed(seconds_now: i64, started_secs: i64) -> Vec<u8> {
    format!("{}s", seconds_now - started_secs).into_bytes()
}

/// `_ui_live_enabled`: live redraws unless quiet, on a terminal or
/// forced. `force_live` mirrors `DOT_UI_FORCE_LIVE` with its `:-0`
/// default through the shared quiet normalization.
pub fn live_enabled(quiet: bool, stdout_is_tty: bool, force_live: Option<&str>) -> bool {
    !quiet && (stdout_is_tty || crate::log::is_quiet(force_live))
}

/// `_ui_clear_live`: emit the carriage-return clear exactly once,
/// returning the output with the new flag (always cleared).
pub fn clear_live(live_active: bool) -> (Vec<u8>, bool) {
    if live_active {
        (b"\r\x1b[K".to_vec(), false)
    } else {
        (Vec::new(), false)
    }
}

/// Right-align ASCII text in a six-wide column, like `printf %6s`.
/// Elapsed stamps and counts are digits plus a unit suffix, so byte
/// padding matches on every locale.
fn column6(stamp: &[u8]) -> Vec<u8> {
    let pad = 6usize.saturating_sub(stamp.len());
    let mut out = Vec::with_capacity(6 + stamp.len());
    out.extend(std::iter::repeat_n(b' ', pad));
    out.extend_from_slice(stamp);
    out
}

/// The `[index/total]` head shared by both line renderers.
fn line_head(palette: &Palette, index: i64, total: &str) -> Vec<u8> {
    let mut out = palette.cyan.as_bytes().to_vec();
    out.extend_from_slice(format!("[{index}/{total}]").as_bytes());
    out.extend_from_slice(palette.reset.as_bytes());
    out
}

/// The label/status/detail cells shared by both line renderers.
/// The color comes from the stage `status` while the cell shows
/// `cell_text`: for a running live line those differ (colored
/// `running` slot around the current spinner frame).
fn line_cells(
    palette: &Palette,
    label: &[u8],
    color_status: &[u8],
    cell_text: &[u8],
    detail: &[u8],
    multibyte: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&cell(label, 10, multibyte));
    out.push(b' ');
    out.extend_from_slice(color(color_status, palette).as_bytes());
    out.extend_from_slice(&cell(cell_text, 8, multibyte));
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(&cell(detail, 42, multibyte));
    out
}

/// `_ui_line`: one newline-terminated progress line. `total` and
/// `elapsed` interpolate literally (`%s`), so stage counters pass
/// through untouched.
#[allow(clippy::too_many_arguments)] // positional parity with the ported shell function
pub fn line(
    palette: &Palette,
    quiet: bool,
    index: i64,
    total: &str,
    label: &[u8],
    status: &[u8],
    detail: &[u8],
    elapsed: &[u8],
    multibyte: bool,
) -> Vec<u8> {
    if quiet {
        return Vec::new();
    }
    let mut out = line_head(palette, index, total);
    out.push(b' ');
    out.extend_from_slice(&line_cells(
        palette, label, status, status, detail, multibyte,
    ));
    out.push(b' ');
    out.extend_from_slice(&column6(elapsed));
    out.push(b'\n');
    out
}

/// ASCII spinner frames, selected when [`ascii_mode`] holds.
const ASCII_FRAMES: [&str; 4] = ["/", "-", "\\", "|"];
/// Braille spinner frames for working UTF-8 terminals.
const UNICODE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// `_ui_live_line`: the carriage-return redraw without a trailing
/// newline. A `running` status shows the next spinner frame and
/// advances `spinner` modulo the frame count; any other status
/// prints literally and leaves the spinner alone. Quiet returns
/// before either, so a silenced line never advances the cycle.
#[allow(clippy::too_many_arguments)] // positional parity with the ported shell function
pub fn live_line(
    palette: &Palette,
    quiet: bool,
    index: i64,
    total: &str,
    label: &[u8],
    status: &[u8],
    detail: &[u8],
    elapsed: &[u8],
    spinner: &mut u64,
    ascii: bool,
    multibyte: bool,
) -> Vec<u8> {
    if quiet {
        return Vec::new();
    }
    let frame: &[u8] = if status == b"running" {
        let frames = if ascii {
            &ASCII_FRAMES[..]
        } else {
            &UNICODE_FRAMES[..]
        };
        let text = frames[(*spinner as usize) % frames.len()];
        *spinner = (*spinner + 1) % frames.len() as u64;
        text.as_bytes()
    } else {
        status
    };
    let mut out = b"\r\x1b[K".to_vec();
    out.extend_from_slice(&line_head(palette, index, total));
    out.push(b' ');
    out.extend_from_slice(&line_cells(
        palette, label, status, frame, detail, multibyte,
    ));
    out.push(b' ');
    out.extend_from_slice(&column6(elapsed));
    out
}

/// `_ui_status`: `  <status-cell> <detail>` after clearing any live
/// line. Returns the output with the new live flag.
pub fn status(
    palette: &Palette,
    quiet: bool,
    live_active: bool,
    status: &[u8],
    detail: &[u8],
    multibyte: bool,
) -> (Vec<u8>, bool) {
    if quiet {
        return (Vec::new(), live_active);
    }
    let (mut out, live_active) = clear_live(live_active);
    out.extend_from_slice(b"  ");
    out.extend_from_slice(color(status, palette).as_bytes());
    out.extend_from_slice(&cell(status, 8, multibyte));
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    out.extend_from_slice(detail);
    out.push(b'\n');
    (out, live_active)
}

/// `_ui_section`: indented bold-white title after clearing any live
/// line. Returns the output with the new live flag.
pub fn section(
    palette: &Palette,
    quiet: bool,
    live_active: bool,
    title: &[u8],
    _multibyte: bool,
) -> (Vec<u8>, bool) {
    if quiet {
        return (Vec::new(), live_active);
    }
    let (mut out, live_active) = clear_live(live_active);
    out.extend_from_slice(b"  ");
    out.extend_from_slice(palette.bold.as_bytes());
    out.extend_from_slice(palette.white.as_bytes());
    out.extend_from_slice(title);
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b'\n');
    (out, live_active)
}

/// `_ui_detail`: indented dim line after clearing any live line.
/// Returns the output with the new live flag.
pub fn detail(
    palette: &Palette,
    quiet: bool,
    live_active: bool,
    text: &[u8],
    _multibyte: bool,
) -> (Vec<u8>, bool) {
    if quiet {
        return (Vec::new(), live_active);
    }
    let (mut out, live_active) = clear_live(live_active);
    out.extend_from_slice(b"    ");
    out.extend_from_slice(palette.dim.as_bytes());
    out.extend_from_slice(text);
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b'\n');
    (out, live_active)
}

/// `_ui_item`: named row with an optional dim trailer, after clearing
/// any live line. Returns the output with the new live flag.
pub fn item(
    palette: &Palette,
    quiet: bool,
    live_active: bool,
    status: &[u8],
    name: &[u8],
    detail: Option<&[u8]>,
    multibyte: bool,
) -> (Vec<u8>, bool) {
    if quiet {
        return (Vec::new(), live_active);
    }
    let (mut out, live_active) = clear_live(live_active);
    out.extend_from_slice(b"  ");
    out.extend_from_slice(color(status, palette).as_bytes());
    out.extend_from_slice(&cell(status, 8, multibyte));
    out.extend_from_slice(palette.reset.as_bytes());
    out.push(b' ');
    match detail {
        Some(detail) if !detail.is_empty() => {
            out.extend_from_slice(&pad(name, 28, multibyte));
            out.push(b' ');
            out.extend_from_slice(palette.dim.as_bytes());
            out.extend_from_slice(detail);
            out.extend_from_slice(palette.reset.as_bytes());
            out.push(b'\n');
        }
        _ => {
            out.extend_from_slice(name);
            out.push(b'\n');
        }
    }
    (out, live_active)
}

/// Parse a bar/label width the way shell arithmetic reads it for
/// sizing: trimmed decimal digits with one optional `+`. Anything
/// else (including octal-looking and hex spellings, which bash would
/// read differently) yields `None` and the bar reads empty — the
/// same out-of-contract boundary `log::is_quiet` documents.
fn parse_width(text: &str) -> Option<usize> {
    let digits = text.trim().strip_prefix('+').unwrap_or_else(|| text.trim());
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    if digits.len() > 6 {
        return None;
    }
    if digits.is_empty() {
        return Some(0);
    }
    digits.parse().ok()
}

/// `_ui_progress_bar`: `[###---] done/total` with ASCII or block
/// glyphs. A non-positive total prints nothing; a malformed width
/// reads empty the way the shell arithmetic error does.
pub fn progress_bar(done: i64, total: i64, width: &str, ascii: bool) -> Vec<u8> {
    if total <= 0 {
        return Vec::new();
    }
    let width = match parse_width(width) {
        Some(width) => width,
        None => return Vec::new(),
    };
    let mut filled = done * width as i64 / total;
    if filled > width as i64 {
        filled = width as i64;
    }
    let empty = width as i64 - filled;
    let (fill_char, empty_char) = if ascii { ("#", "-") } else { ("━", "·") };
    let mut bar = String::new();
    for _ in 0..filled.max(0) {
        bar.push_str(fill_char);
    }
    for _ in 0..empty.max(0) {
        bar.push_str(empty_char);
    }
    let done_text = done.to_string();
    let total_text = total.to_string();
    let digits = done_text.len().max(total_text.len());
    format!("[{bar}] {done_text:>digits$}/{total_text}").into_bytes()
}

/// `_ui_progress_detail_with_label`: padded label, bar, and optional
/// suffix. A malformed label width empties just the label cell (the
/// shell error stays inside that command substitution); a malformed
/// bar width empties the bar the same way.
#[allow(clippy::too_many_arguments)] // positional parity with the ported shell function
pub fn progress_detail_with_label(
    label: &[u8],
    done: i64,
    total: i64,
    suffix: Option<&[u8]>,
    label_width: &str,
    bar_width: &str,
    ascii: bool,
    multibyte: bool,
) -> Vec<u8> {
    let mut out = match parse_width(label_width) {
        Some(width) => cell(label, width, multibyte),
        None => Vec::new(),
    };
    out.push(b' ');
    out.extend_from_slice(&progress_bar(done, total, bar_width, ascii));
    if let Some(suffix) = suffix {
        if !suffix.is_empty() {
            out.push(b' ');
            out.extend_from_slice(suffix);
        }
    }
    out
}

/// `_dot_progress_detail` (`repos/pull.sh`): the labeled detail when
/// the total is positive, nothing otherwise. Totals arrive canonical
/// from shell arithmetic upstream.
pub fn progress_detail(
    label: &[u8],
    done: i64,
    total: i64,
    bar_width: &str,
    ascii: bool,
    multibyte: bool,
) -> Vec<u8> {
    if total <= 0 {
        return Vec::new();
    }
    progress_detail_with_label(label, done, total, None, "18", bar_width, ascii, multibyte)
}

/// Parse shell-arithmetic decimal input: trimmed, one optional `+`
/// or `-`, ASCII digits only. Overlong digit strings read `None`
/// (past int64 range — no producer emits them); hex, octal, and
/// other bash-arithmetic spellings are out of contract, the same
/// boundary `log::is_quiet` documents.
fn decimal_value(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let (negative, digits) = match trimmed.strip_prefix(['+', '-']) {
        Some(rest) => (trimmed.starts_with('-'), rest),
        None => (false, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    if digits.len() > 18 {
        return None;
    }
    let value: i64 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    Some(if negative { -value } else { value })
}

/// `[[ $text -gt 0 ]]` for canonical and normalized spellings;
/// malformed input is falsy (the shell errors).
fn int_gt_zero(text: &str) -> bool {
    decimal_value(text).is_some_and(|value| value > 0)
}

/// `[[ $text -eq 0 ]]`: decimals compare literally; anything else
/// bash reads as an unset-variable name coerces to 0 (so `abc` and
/// the empty string are zero), while malformed arithmetic such as
/// `1abc` errors falsy.
fn int_is_zero(text: &str) -> bool {
    if let Some(value) = decimal_value(text) {
        return value == 0;
    }
    let trimmed = text.trim();
    trimmed.is_empty() || is_bare_name(trimmed)
}

/// Bash arithmetic resolves a bare `name` token as a variable; no
/// producer exports one, so every such name reads as unset (zero).
fn is_bare_name(text: &str) -> bool {
    let mut bytes = text.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// One live progress stage: the `DOT_UI_*` globals as owned state.
/// Quiet, live-redraw, counting, and glyph modes resolve once at
/// [`Stage::begin`] (production reads them from the environment);
/// per-call environment reads in shell become explicit parameters
/// on the methods that need them.
pub struct Stage {
    palette: Palette,
    quiet: bool,
    live: bool,
    multibyte: bool,
    ascii: bool,
    index: i64,
    total: String,
    label: Vec<u8>,
    detail: Vec<u8>,
    started_secs: i64,
    spinner: u64,
    /// Set by every live emission (shell: `DOT_UI_LIVE_ACTIVE=1` at
    /// the end of `_ui_live_line`) so the next clear knows a live
    /// line is on screen.
    live_active: bool,
}

impl Stage {
    /// `_ui_begin`: open the run with `total` stages. `total`
    /// interpolates literally later, so it stays a string.
    pub fn begin(
        palette: Palette,
        total: &str,
        quiet: bool,
        live: bool,
        multibyte: bool,
        ascii: bool,
    ) -> Self {
        Stage {
            palette,
            quiet,
            live,
            multibyte,
            ascii,
            index: 0,
            total: total.to_string(),
            label: Vec::new(),
            detail: Vec::new(),
            started_secs: 0,
            spinner: 0,
            live_active: false,
        }
    }

    /// `_ui_stage_start`: advance the counter, bind the label (empty
    /// details read `working`, like `${2:-working}`), and render the
    /// opening line — plus the live redraw when verbose callers also
    /// want the plain line.
    pub fn start(
        &mut self,
        label: &[u8],
        detail: Option<&[u8]>,
        now_secs: i64,
        verbose: Option<&str>,
    ) -> Vec<u8> {
        if self.quiet {
            return Vec::new();
        }
        self.index += 1;
        self.label = label.to_vec();
        self.detail = match detail {
            Some(detail) if !detail.is_empty() => detail.to_vec(),
            _ => b"working".to_vec(),
        };
        self.started_secs = now_secs;
        self.spinner = 0;
        if self.live {
            let mut out = Vec::new();
            if crate::log::is_quiet(verbose) {
                out.extend_from_slice(&line(
                    &self.palette,
                    false,
                    self.index,
                    &self.total,
                    &self.label,
                    b"running",
                    &self.detail,
                    b"0s",
                    self.multibyte,
                ));
            }
            out.extend_from_slice(&live_line(
                &self.palette,
                false,
                self.index,
                &self.total,
                &self.label,
                b"running",
                &self.detail,
                b"0s",
                &mut self.spinner,
                self.ascii,
                self.multibyte,
            ));
            self.live_active = true;
            out
        } else {
            line(
                &self.palette,
                false,
                self.index,
                &self.total,
                &self.label,
                b"running",
                &self.detail,
                b"0s",
                self.multibyte,
            )
        }
    }

    /// `_ui_stage_update`: re-render with a new detail — live redraw,
    /// newline progress for verbose non-live callers, silence
    /// otherwise.
    pub fn update(&mut self, detail: &[u8], now_secs: i64, verbose: Option<&str>) -> Vec<u8> {
        if self.quiet {
            return Vec::new();
        }
        self.detail = detail.to_vec();
        let stamp = elapsed(now_secs, self.started_secs);
        if self.live {
            let out = live_line(
                &self.palette,
                false,
                self.index,
                &self.total,
                &self.label,
                b"running",
                &self.detail,
                &stamp,
                &mut self.spinner,
                self.ascii,
                self.multibyte,
            );
            self.live_active = true;
            out
        } else if crate::log::is_quiet(verbose) {
            line(
                &self.palette,
                false,
                self.index,
                &self.total,
                &self.label,
                b"running",
                &self.detail,
                &stamp,
                self.multibyte,
            )
        } else {
            Vec::new()
        }
    }

    /// `_ui_stage_tick`: heartbeat redraw for live callers, silence
    /// otherwise. An empty detail reads `working`, like the shell
    /// default when no stage ever started.
    pub fn tick(&mut self, now_secs: i64) -> Vec<u8> {
        if self.quiet || !self.live {
            return Vec::new();
        }
        let detail = if self.detail.is_empty() {
            b"working".as_slice()
        } else {
            &self.detail
        };
        let stamp = elapsed(now_secs, self.started_secs);
        let out = live_line(
            &self.palette,
            false,
            self.index,
            &self.total,
            &self.label,
            b"running",
            detail,
            &stamp,
            &mut self.spinner,
            self.ascii,
            self.multibyte,
        );
        self.live_active = true;
        out
    }

    /// `_ui_stage_finish`: clear any live line, then close with the
    /// final status. Quiet returns before either, leaving state
    /// untouched like the shell early return.
    pub fn finish(&mut self, status: &[u8], detail: &[u8], now_secs: i64) -> Vec<u8> {
        if self.quiet {
            return Vec::new();
        }
        let stamp = elapsed(now_secs, self.started_secs);
        let (mut out, live_active) = clear_live(self.live_active);
        self.live_active = live_active;
        out.extend_from_slice(&line(
            &self.palette,
            false,
            self.index,
            &self.total,
            &self.label,
            status,
            detail,
            &stamp,
            self.multibyte,
        ));
        out
    }

    /// `_ui_stage_note`: one status row through [`status`].
    pub fn note(&mut self, status_text: &[u8], detail: &[u8]) -> Vec<u8> {
        if self.quiet {
            return Vec::new();
        }
        let (out, live_active) = status(
            &self.palette,
            false,
            self.live_active,
            status_text,
            detail,
            self.multibyte,
        );
        self.live_active = live_active;
        out
    }

    /// `_ui_stage`: advance the counter, then the header line —
    /// the `[index/total]` form for a positive total, the bare label
    /// otherwise. Returns the complete bytes including the
    /// `_log_header` outer paint and newline, so production writes
    /// them directly (routing the composed text back through the
    /// shared log header would double-paint, exactly like the shell
    /// nesting does — the fold keeps non-UTF-8 labels byte-exact).
    pub fn header_text(&mut self, label: &[u8]) -> Vec<u8> {
        if self.quiet {
            return Vec::new();
        }
        self.index += 1;
        let mut inner = Vec::new();
        if int_gt_zero(&self.total) {
            inner.extend_from_slice(self.palette.cyan.as_bytes());
            inner.extend_from_slice(format!("[{}/{}]", self.index, self.total).as_bytes());
            inner.extend_from_slice(self.palette.reset.as_bytes());
            inner.push(b' ');
        }
        inner.extend_from_slice(self.palette.bold.as_bytes());
        inner.extend_from_slice(self.palette.white.as_bytes());
        inner.extend_from_slice(label);
        inner.extend_from_slice(self.palette.reset.as_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(self.palette.bold.as_bytes());
        out.extend_from_slice(self.palette.white.as_bytes());
        out.extend_from_slice(&inner);
        out.extend_from_slice(self.palette.reset.as_bytes());
        out.push(b'\n');
        out
    }

    /// `_dot_maybe_stage_progress`: render labeled progress through
    /// [`Stage::update`] only for a positive total with an unset-or-zero
    /// verbose flag (malformed counts and nonzero flags read silent,
    /// like the shell arithmetic errors).
    pub fn maybe_progress(
        &mut self,
        label: &[u8],
        done: i64,
        total: i64,
        now_secs: i64,
        verbose: Option<&str>,
        bar_width: &str,
    ) -> Vec<u8> {
        if self.quiet || !int_gt_zero(&self.total) {
            return Vec::new();
        }
        let verbose_silent = match verbose {
            None => true,
            Some(flag) => int_is_zero(flag),
        };
        if !verbose_silent {
            return Vec::new();
        }
        let rendered = progress_detail(label, done, total, bar_width, self.ascii, self.multibyte);
        self.update(&rendered, now_secs, verbose)
    }
}

/// `_ui_done`: `Done in Ns.` (or `Done with errors`), plus the
/// reload hint when one applies. `status` defaults to `0` when
/// absent, like `${1:-0}`; `hint` is the precomputed
/// [`reload_hint`] text.
pub fn done(
    palette: &Palette,
    quiet: bool,
    status: Option<&str>,
    started_secs: i64,
    now_secs: i64,
    hint: &[u8],
) -> Vec<u8> {
    if quiet {
        return Vec::new();
    }
    let message = if int_is_zero(status.unwrap_or("0")) {
        "Done"
    } else {
        "Done with errors"
    };
    let stamp = elapsed(now_secs, started_secs);
    let mut out = Vec::new();
    out.extend_from_slice(palette.bold.as_bytes());
    out.extend_from_slice(palette.white.as_bytes());
    out.extend_from_slice(message.as_bytes());
    out.extend_from_slice(b" in ");
    out.extend_from_slice(&stamp);
    if hint.is_empty() {
        out.extend_from_slice(palette.reset.as_bytes());
        out.push(b'\n');
    } else {
        out.extend_from_slice(b".");
        out.extend_from_slice(palette.reset.as_bytes());
        out.push(b' ');
        out.extend_from_slice(hint);
        out.push(b'\n');
    }
    out
}

/// `_ui_shell_reload_hint`: empty once the checkpoint records a
/// reload, else the shell-specific source line, falling back to the
/// `HOME` rc files when no known shell resolves. An absent
/// `shell_name` reads the files branch; note bash backfills an unset
/// `SHELL` from the login shell while Rust sees the raw environment,
/// so production agreement there assumes `SHELL` is exported (every
/// login path exports it).
pub fn reload_hint(
    dot_update_reloads_shell: Option<&str>,
    shell_name: Option<&str>,
    home_has_bashrc: bool,
    home_has_zshrc: bool,
) -> Vec<u8> {
    if crate::log::is_quiet(dot_update_reloads_shell) {
        return Vec::new();
    }
    match shell_name {
        Some("bash") => b"Reload your shell: source ~/.bashrc".to_vec(),
        Some("zsh") => b"Reload your shell: source ~/.zshrc".to_vec(),
        _ => {
            if home_has_bashrc && !home_has_zshrc {
                b"Reload your shell: source ~/.bashrc".to_vec()
            } else {
                b"Reload your shell: source ~/.zshrc".to_vec()
            }
        }
    }
}

/// `_ui_normal_shell_name`: basename without one leading dash, kept
/// only for `bash`/`zsh`.
pub fn normal_shell_name(path: &str) -> Option<&str> {
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.strip_prefix('-').unwrap_or(base);
    match base {
        "bash" | "zsh" => Some(base),
        _ => None,
    }
}

/// Pure core of `_ui_parent_shell_name`: the `/proc` reading wins,
/// `ps` output (first line, surrounding blanks trimmed) is the
/// fallback. Production feeds the process reads; the shell body
/// itself is unpinnable process state, so tests pin this core plus
/// the normalization above it.
pub fn parent_shell_name(proc_comm: Option<&str>, ps_comm: Option<&str>) -> Option<String> {
    if let Some(comm) = proc_comm {
        if let Some(name) = normal_shell_name(comm) {
            return Some(name.to_string());
        }
    }
    if let Some(raw) = ps_comm {
        let first = raw.split('\n').next().unwrap_or("");
        let trimmed = first.trim_matches(|c: char| c.is_ascii_whitespace());
        if let Some(name) = normal_shell_name(trimmed) {
            return Some(name.to_string());
        }
    }
    None
}
