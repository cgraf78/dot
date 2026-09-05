//! Public presentation helpers (slice 2 foundations).
//!
//! Ports `lib/dot/public/ui.sh` exactly: the named color map, `#rrggbb`
//! validation, gum/tty/plain renderer selection, and the title and
//! summary-box layouts. Output CONTENT stays caller-owned here as in
//! the shell; these helpers only prevent styling from drifting.
//!
//! The shell reports arity problems with exit 2 and gum absence with
//! exit 1; Rust surfaces the same split as [`Error`] so callers map to
//! identical exit codes.

use std::io::Write;
use std::path::{Path, PathBuf};

/// UI failure, mirroring the shell exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity or malformed value (shell exit 2).
    Usage,
    /// Gum requested but unavailable (shell exit 1 from `has_gum`).
    Unavailable,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Unavailable => 1,
        }
    }
}

/// Resolve a color name or `#rrggbb` literal to the validated hex value.
///
/// The shell matches names first, then the hex glob — order matters only
/// in that a name can never be mistaken for hex input. Owned return
/// because validated hex input is borrowed from the caller.
pub fn color_hex(name: &str) -> Result<String, Error> {
    match name {
        "green" => Ok("#3fb950".to_string()),
        "red" => Ok("#f85149".to_string()),
        "yellow" => Ok("#d29922".to_string()),
        "magenta" => Ok("#bc8cff".to_string()),
        "dim" => Ok("#8b949e".to_string()),
        _ if is_hex_color(name) => Ok(name.to_string()),
        _ => Err(Error::Usage),
    }
}

/// Validate `#rrggbb` (exactly six hex digits, either case).
pub fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(|b| b.is_ascii_hexdigit())
}

/// Split a validated `#rrggbb` into decimal components.
///
/// The shell prints `r;g;b` directly; Rust returns the triple and lets
/// the renderer format it, keeping parsing separate from output.
pub fn hex_to_rgb(hex: &str) -> Result<(u8, u8, u8), Error> {
    if !is_hex_color(hex) {
        return Err(Error::Usage);
    }
    // Slicing is safe: validated above as `#` + 6 ASCII hex digits.
    let pair = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16).map_err(|_| Error::Usage)
    };
    Ok((pair(1..3)?, pair(3..5)?, pair(5..7)?))
}

/// Locate a usable `gum` binary: on PATH, executable, and answering
/// `gum style --help` successfully (the shell's three gates in order).
pub fn find_gum(path_dirs: &str) -> Option<PathBuf> {
    // `PATH` splitting is platform-specific; the engine is Unix-only in
    // practice (sibling crates gate Unix modules on cfg(unix)), and the
    // shell this ports runs under POSIX `type -P` semantics. Split on
    // `:` explicitly rather than via an env lookup so tests inject
    // fixture dirs deterministically.
    for dir in path_dirs.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join("gum");
        if !is_executable_file(&candidate) {
            continue;
        }
        let ok = std::process::Command::new(&candidate)
            .arg("style")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if ok {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// How a title/box is rendered (shell: gum, else tty-ANSI, else plain).
#[derive(Debug, Clone)]
pub enum Renderer {
    /// Pipe through `gum style` (validates color first, like the shell).
    Gum {
        /// Resolved gum binary. Must be pre-validated with
        /// [`find_gum`]: the shell only reaches its gum branch after
        /// the same gates pass, and an unvalidated binary that fails
        /// at render time reports [`Error::Unavailable`] instead of
        /// falling back to another renderer.
        binary: PathBuf,
    },
    /// ANSI styling (shell: `[[ -t 1 && -z ${NO_COLOR:-} ]]`).
    Ansi,
    /// No styling (piped output or `NO_COLOR` set non-empty).
    Plain,
}

impl Renderer {
    /// Select exactly like the shell: explicit gum binary wins, else a
    /// tty without a non-empty `NO_COLOR` gets ANSI, else plain.
    pub fn select(gum: Option<PathBuf>, is_tty: bool, no_color: Option<&str>) -> Self {
        if let Some(binary) = gum {
            return Renderer::Gum { binary };
        }
        // `-z ${NO_COLOR:-}` is true when unset OR empty: only a
        // non-empty NO_COLOR disables color.
        match (is_tty, no_color) {
            (true, None) | (true, Some("")) => Renderer::Ansi,
            _ => Renderer::Plain,
        }
    }
}

/// Write a title block: blank, bold text, blank (ANSI) or plain lines.
/// The shell `dot_ui_title` takes no color (gum uses fixed 212).
pub fn title(out: &mut dyn Write, renderer: &Renderer, text: &str) -> Result<(), Error> {
    match renderer {
        Renderer::Gum { binary } => {
            // Argument order mirrors the shell invocation exactly.
            // Gum's stdout is piped through `out` (the shell inherits
            // its stdout the same way) so every renderer has one sink.
            let output = std::process::Command::new(binary)
                .arg("style")
                .arg("--bold")
                .arg("--foreground")
                .arg("212")
                .arg("--border")
                .arg("normal")
                .arg("--padding")
                .arg("0 2")
                .arg(text)
                .output()
                .map_err(|_| Error::Unavailable)?;
            if !output.status.success() {
                return Err(Error::Unavailable);
            }
            out.write_all(&output.stdout)
                .map_err(|_| Error::Unavailable)
        }
        Renderer::Ansi => writeln!(out, "\n\x1b[1m{text}\x1b[0m\n").map_err(|_| Error::Unavailable),
        Renderer::Plain => writeln!(out, "\n{text}\n").map_err(|_| Error::Unavailable),
    }
}

/// Write a summary box: 32-wide `═` rules around the text, ANSI-bold
/// colored in tty mode, gum-styled with the validated color otherwise.
pub fn summary_box(
    out: &mut dyn Write,
    renderer: &Renderer,
    color: &str,
    text: &str,
) -> Result<(), Error> {
    // The shell resolves the color BEFORE choosing gum-vs-tty, so an
    // invalid color fails even when gum would be used: same order here.
    let hex = color_hex(color)?;
    const RULE: &str = "════════════════════════════════";
    match renderer {
        Renderer::Gum { binary } => {
            let output = std::process::Command::new(binary)
                .arg("style")
                .arg("--bold")
                .arg("--foreground")
                .arg(&hex)
                .arg("--border")
                .arg("rounded")
                .arg("--padding")
                .arg("0 2")
                .arg(text)
                .output()
                .map_err(|_| Error::Unavailable)?;
            if !output.status.success() {
                return Err(Error::Unavailable);
            }
            out.write_all(&output.stdout)
                .map_err(|_| Error::Unavailable)
        }
        Renderer::Ansi => {
            let (r, g, b) = hex_to_rgb(&hex)?;
            writeln!(out, "{RULE}").map_err(|_| Error::Unavailable)?;
            writeln!(out, "\x1b[1;38;2;{r};{g};{b}m{text}\x1b[0m")
                .map_err(|_| Error::Unavailable)?;
            writeln!(out, "{RULE}").map_err(|_| Error::Unavailable)
        }
        Renderer::Plain => {
            writeln!(out, "{RULE}").map_err(|_| Error::Unavailable)?;
            writeln!(out, "{text}").map_err(|_| Error::Unavailable)?;
            writeln!(out, "{RULE}").map_err(|_| Error::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_colors_and_hex_passthrough() {
        for (name, expected) in [
            ("green", "#3fb950"),
            ("red", "#f85149"),
            ("yellow", "#d29922"),
            ("magenta", "#bc8cff"),
            ("dim", "#8b949e"),
            ("#123abc", "#123abc"),
        ] {
            assert_eq!(color_hex(name).as_deref(), Ok(expected));
        }
        assert!(is_hex_color("#ABCDEF"));
        assert!(is_hex_color("#abcdef"));
        assert!(is_hex_color("#123456"));
        for bad in [
            "", "green ", "#abc", "#abcdefg", "abcdef", "#abcde!", "#ABCDE", "##12345",
        ] {
            assert_eq!(color_hex(bad), Err(Error::Usage), "input: {bad:?}");
            assert!(!is_hex_color(bad), "input: {bad:?}");
        }
    }

    #[test]
    fn hex_to_rgb_components() {
        assert_eq!(hex_to_rgb("#000000"), Ok((0, 0, 0)));
        assert_eq!(hex_to_rgb("#ffffff"), Ok((255, 255, 255)));
        assert_eq!(hex_to_rgb("#3fb950"), Ok((0x3f, 0xb9, 0x50)));
        assert_eq!(hex_to_rgb("#ABCDEF"), Ok((0xab, 0xcd, 0xef)));
        assert_eq!(hex_to_rgb("nope"), Err(Error::Usage));
        assert_eq!(hex_to_rgb("#12345"), Err(Error::Usage));
    }

    #[test]
    fn renderer_selection_matches_shell() {
        use std::path::PathBuf;
        let gum = Some(PathBuf::from("/bin/gum"));
        assert!(matches!(
            Renderer::select(gum.clone(), false, Some("1")),
            Renderer::Gum { .. }
        ));
        assert!(matches!(Renderer::select(None, true, None), Renderer::Ansi));
        // NO_COLOR set-but-empty still colors (shell `-z` test).
        assert!(matches!(
            Renderer::select(None, true, Some("")),
            Renderer::Ansi
        ));
        assert!(matches!(
            Renderer::select(None, true, Some("1")),
            Renderer::Plain
        ));
        assert!(matches!(
            Renderer::select(None, false, None),
            Renderer::Plain
        ));
    }

    #[test]
    fn plain_title_and_box_layout() {
        let mut out = Vec::new();
        title(&mut out, &Renderer::Plain, "Hello").expect("title");
        assert_eq!(out, b"\nHello\n\n");
        out.clear();
        summary_box(&mut out, &Renderer::Plain, "green", "Hello").expect("box");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "Hello");
        // 32 box-drawing chars per rule line.
        assert_eq!(lines[0].chars().count(), 32);
        assert_eq!(lines[0], lines[2]);
    }

    #[test]
    fn ansi_title_and_box_layout() {
        let mut out = Vec::new();
        title(&mut out, &Renderer::Ansi, "Hi").expect("title");
        assert_eq!(out, b"\n\x1b[1mHi\x1b[0m\n\n");
        out.clear();
        summary_box(&mut out, &Renderer::Ansi, "#3fb950", "Hi").expect("box");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\x1b[1;38;2;63;185;80mHi\x1b[0m"), "{text:?}");
    }

    #[test]
    fn invalid_color_fails_before_rendering() {
        let mut out = Vec::new();
        assert_eq!(
            summary_box(&mut out, &Renderer::Plain, "chartreuse", "Hi"),
            Err(Error::Usage)
        );
        assert!(out.is_empty());
    }
}
