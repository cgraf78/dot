//! Quiet-gated logging helpers (slice 2 foundations).
//!
//! Ports `lib/dot/log.sh` exactly: color enablement (tty stdout plus
//! unset-or-empty `NO_COLOR`), the quiet gate on `DOT_QUIET`, and the
//! six helpers with their stdout/stderr routing. Message CONTENT stays
//! caller-owned; callers pass pre-joined text (`echo "$@"` joins with
//! spaces, so a single Rust `&str` carries the same bytes).

use std::io::Write;

// ANSI sequences copied from `lib/dot/log.sh` (color block, lines 9-19).
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[0;90m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const WHITE: &str = "\x1b[38;2;255;255;255m";

/// True when `DOT_QUIET` silences quiet-gated helpers.
///
/// The shell uses `[[ "$DOT_QUIET" -eq 1 ]]` (bash arithmetic). In-tree
/// producers only ever emit `0` (the `constants.sh` default) or `1`, so
/// this mirrors arithmetic for decimal spellings — surrounding
/// whitespace, a single leading `+`/`-`, and leading zeros — via
/// digit-string normalization (no overflow on absurd inputs). Exotic
/// bash-arithmetic spellings such as `0x1` are out of contract: no
/// producer emits them and the strict config parser would not bless
/// them either.
pub fn is_quiet(dot_quiet: Option<&str>) -> bool {
    let text = match dot_quiet {
        None => return false,
        Some(text) => text.trim(),
    };
    let (negative, digits) = match text.strip_prefix(['+', '-']) {
        Some(rest) => (text.starts_with('-'), rest),
        None => (false, text),
    };
    !negative && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && {
        // Numeric value 1: all leading zeros with a single trailing 1.
        let stripped = digits.trim_start_matches('0');
        stripped == "1"
    }
}

/// Logging configuration: color switch plus quiet gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Log {
    /// Emit ANSI sequences (shell: `[[ -t 1 && -z ${NO_COLOR:-} ]]`).
    colored: bool,
    /// Suppress quiet-gated helpers (shell: `DOT_QUIET -eq 1`).
    quiet: bool,
}

impl Log {
    /// Build from resolved switches.
    pub fn new(colored: bool, quiet: bool) -> Self {
        Log { colored, quiet }
    }

    /// Build exactly like the shell: `is_tty_stdout` is whether fd 1 is
    /// a terminal, `no_color` the value (or absence) of `NO_COLOR`,
    /// `dot_quiet` the value (or absence) of `DOT_QUIET`.
    pub fn from_env(is_tty_stdout: bool, no_color: Option<&str>, dot_quiet: Option<&str>) -> Self {
        // `-z ${NO_COLOR:-}` is true when unset OR empty: only a
        // non-empty NO_COLOR disables color (same rule as `ui`).
        let colored = matches!((is_tty_stdout, no_color), (true, None) | (true, Some("")));
        Log {
            colored,
            quiet: is_quiet(dot_quiet),
        }
    }

    /// Whether ANSI sequences are emitted.
    pub fn colored(self) -> bool {
        self.colored
    }

    /// Whether quiet-gated helpers are suppressed.
    pub fn quiet(self) -> bool {
        self.quiet
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.colored {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    /// `_log`: plain message unless quiet (stdout).
    pub fn log(&self, out: &mut dyn Write, text: &str) {
        if !self.quiet {
            let _ = writeln!(out, "{text}");
        }
    }

    /// `_header`: bright bold-white header, always prints (stdout).
    pub fn header(&self, out: &mut dyn Write, text: &str) {
        let _ = writeln!(out, "{}", self.paint(&format!("{BOLD}{WHITE}"), text));
    }

    /// `_log_header`: bright bold-white header unless quiet (stdout).
    pub fn log_header(&self, out: &mut dyn Write, text: &str) {
        if !self.quiet {
            self.header(out, text);
        }
    }

    /// `_log_ok`: green message unless quiet (stdout).
    pub fn ok(&self, out: &mut dyn Write, text: &str) {
        if !self.quiet {
            let _ = writeln!(out, "{}", self.paint(GREEN, text));
        }
    }

    /// `_log_dim`: dim message unless quiet (stdout).
    pub fn dim(&self, out: &mut dyn Write, text: &str) {
        if !self.quiet {
            let _ = writeln!(out, "{}", self.paint(DIM, text));
        }
    }

    /// `_warn`: yellow message, always prints (stderr).
    pub fn warn(&self, err_out: &mut dyn Write, text: &str) {
        let _ = writeln!(err_out, "{}", self.paint(YELLOW, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_gate_matches_shell_arithmetic_for_realistic_values() {
        // (input, expected): verified against
        // `[[ "${DOT_QUIET:-0}" -eq 1 ]]` for each row.
        for (input, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("2"), false),
            (Some("01"), true),
            (Some("+1"), true),
            (Some(" 1"), true),
            (Some("1 "), true),
            (Some("-1"), false),
            (Some("abc"), false),
            (Some("1x"), false),
            (Some("007"), false),
        ] {
            assert_eq!(is_quiet(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn from_env_matches_shell_color_rule() {
        assert!(Log::from_env(true, None, None).colored());
        assert!(Log::from_env(true, Some(""), None).colored());
        assert!(!Log::from_env(true, Some("1"), None).colored());
        assert!(!Log::from_env(false, None, None).colored());
        assert!(Log::from_env(false, None, Some("1")).quiet());
        assert!(!Log::from_env(false, None, Some("0")).quiet());
    }

    #[test]
    fn plain_branch_layout() {
        let log = Log::new(false, false);
        let mut out = Vec::new();
        let mut err = Vec::new();
        log.log(&mut out, "m");
        log.header(&mut out, "h");
        log.log_header(&mut out, "lh");
        log.ok(&mut out, "ok");
        log.dim(&mut out, "d");
        log.warn(&mut err, "w");
        assert_eq!(out, b"m\nh\nlh\nok\nd\n");
        assert_eq!(err, b"w\n");
    }

    #[test]
    fn quiet_suppresses_gated_helpers_only() {
        let log = Log::new(false, true);
        let mut out = Vec::new();
        let mut err = Vec::new();
        log.log(&mut out, "m");
        log.header(&mut out, "h");
        log.log_header(&mut out, "lh");
        log.ok(&mut out, "ok");
        log.dim(&mut out, "d");
        log.warn(&mut err, "w");
        // `_header` and `_warn` always print, like the shell.
        assert_eq!(out, b"h\n");
        assert_eq!(err, b"w\n");
    }

    #[test]
    fn colored_branch_byte_layout() {
        let log = Log::new(true, false);
        let mut out = Vec::new();
        let mut err = Vec::new();
        log.log(&mut out, "m");
        log.header(&mut out, "h");
        log.ok(&mut out, "ok");
        log.dim(&mut out, "d");
        log.warn(&mut err, "w");
        assert_eq!(
            out,
            b"m\n\x1b[1m\x1b[38;2;255;255;255mh\x1b[0m\n\x1b[32mok\x1b[0m\n\x1b[0;90md\x1b[0m\n"
        );
        assert_eq!(err, b"\x1b[33mw\x1b[0m\n");
    }
}
