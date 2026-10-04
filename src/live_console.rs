//! One writer pair for a run that draws a live progress row.
//!
//! A live stage row is redrawn in place on stdout: every redraw starts with a
//! carriage-return line erase and ends without a newline, so the terminal
//! cursor sits at the end of an unfinished line until the stage closes. Any
//! other output written meanwhile — a warning on stderr, a replayed pull log
//! or a status row whose writer did not know a row was open, a provider
//! diagnostic relayed mid-stage — used to land at the end of that row and
//! wrap across the terminal.
//!
//! [`LiveConsole`] owns both streams of one run so it sees every byte in the
//! order the terminal receives it (Dot's stdout and stderr already share one
//! ordered relay to the process descriptors). In live mode it keeps the two
//! streams from sharing a line:
//!
//! - before anything else is written while a row is open, it erases the row,
//!   so the message starts at column zero; the stage's next redraw (its
//!   heartbeat or its close) puts the row back below the message;
//! - stderr is passed through in whole lines: a trailing partial line waits
//!   for its newline, because a redraw in between would erase it, and child
//!   processes routinely write one diagnostic in several fragments. Anything
//!   on stdout other than a redraw (the row closing, other output) first
//!   releases a held fragment as its own line, so it stays inside its stage;
//!   an explicit flush or [`PENDING_LIMIT`] releases it as it is.
//!
//! Without live mode (pipes, cron, `--quiet`) both streams pass through
//! byte for byte: there is no row to protect and no terminal to address.
//!
//! The row state is tracked from the bytes themselves rather than from the
//! stage that drew them. Rows can be rendered on another thread (the provider
//! supervisor) and reach the streams later through a relay queue, so only the
//! write point knows what the terminal currently shows. The rule is the
//! terminal's own: a line that begins with a carriage return and has not been
//! terminated is a row redrawn in place.

use std::cell::RefCell;
use std::io::Write;

/// Carriage return plus erase-to-end-of-line: the prefix of every live row
/// redraw, and how this console clears a row before other output.
const ERASE_LINE: &[u8] = b"\r\x1b[K";

/// Most stderr bytes held back waiting for a newline. A stream that never
/// ends its line (a carriage-return progress meter, a flood) is released in
/// pieces this large rather than buffered without bound or hidden for good.
const PENDING_LIMIT: usize = 64 * 1024;

/// What the terminal shows on stdout's current line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// The cursor is at column zero of an empty line.
    Clean,
    /// An unterminated live row, drawn after a carriage return.
    Row,
    /// Unterminated ordinary text (a line written in several fragments).
    Text,
}

impl Line {
    /// The line state after `bytes` are written on top of `self`.
    fn after(self, bytes: &[u8]) -> Self {
        let (tail, prior) = match bytes.iter().rposition(|byte| *byte == b'\n') {
            Some(at) => (&bytes[at + 1..], Line::Clean),
            None => (bytes, self),
        };
        if tail.is_empty() {
            return prior;
        }
        match tail.iter().rposition(|byte| *byte == b'\r') {
            // A return plus erase leaves an empty line.
            Some(at) if tail[at..] == *ERASE_LINE => Line::Clean,
            // A bare return over an empty line leaves it empty; over text it
            // leaves that text on screen, to be erased before other output.
            Some(0) if tail.len() == 1 && prior == Line::Clean => Line::Clean,
            Some(_) => Line::Row,
            None if prior == Line::Row => Line::Row,
            None => Line::Text,
        }
    }
}

/// Which of the two streams a [`ConsoleStream`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Stdout,
    Stderr,
}

struct State<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    live: bool,
    line: Line,
    /// Stderr bytes after the last newline, held back in live mode.
    pending: Vec<u8>,
    /// A released partial stderr line is on screen, unterminated.
    err_open: bool,
}

impl State<'_> {
    /// Erase an open live row so the next bytes start at column zero.
    fn erase_row(&mut self) -> std::io::Result<()> {
        if self.line == Line::Row {
            self.out.write_all(ERASE_LINE)?;
            self.line = Line::Clean;
        }
        Ok(())
    }

    fn write_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // A write that starts with its own carriage return repositions to
        // column zero itself: that is the stage redrawing or closing its row.
        let repositions = bytes.first() == Some(&b'\r');
        // A plain redraw leaves a held stderr fragment alone (it is not on
        // screen to be erased); anything else (the row closing, other
        // output) releases it first, so it stays inside its stage.
        if !(repositions && !bytes.contains(&b'\n')) {
            self.release_pending(true)?;
        }
        // A partial stderr line already on screen would be overwritten.
        if self.err_open {
            self.err.write_all(b"\n")?;
            self.err_open = false;
        }
        if !repositions {
            self.erase_row()?;
        }
        self.out.write_all(bytes)?;
        self.line = self.line.after(bytes);
        Ok(())
    }

    /// Write `bytes` to stderr after erasing an open row. The erase is
    /// best effort: stderr may reach a log file while stdout's terminal is
    /// gone, so a failed erase still delivers the message, then reports.
    fn emit_stderr(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let erased = self.erase_row();
        self.err.write_all(bytes)?;
        erased
    }

    fn write_stderr(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let held = self.pending.len();
        self.pending.extend_from_slice(bytes);
        // Only the new bytes can hold a newline: `pending` never does.
        if let Some(at) = bytes.iter().rposition(|byte| *byte == b'\n') {
            let rest = self.pending.split_off(held + at + 1);
            let lines = std::mem::replace(&mut self.pending, rest);
            self.err_open = false;
            self.emit_stderr(&lines)?;
        }
        if self.pending.len() >= PENDING_LIMIT {
            self.release_pending(false)?;
        }
        Ok(())
    }

    /// Write a held partial stderr line now: as its own line when
    /// `terminate` holds, otherwise as it is (left open on screen).
    fn release_pending(&mut self, terminate: bool) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut partial = std::mem::take(&mut self.pending);
        if terminate {
            partial.push(b'\n');
        }
        self.err_open = !terminate;
        self.emit_stderr(&partial)
    }
}

/// The shared stdout/stderr pair for one live progress run (see the module
/// documentation). Hand out its streams with [`LiveConsole::stdout`] and
/// [`LiveConsole::stderr`]; call [`LiveConsole::finish`] once the run has
/// stopped writing.
pub struct LiveConsole<'a> {
    state: RefCell<State<'a>>,
}

impl<'a> LiveConsole<'a> {
    /// Coordinate `out` and `err`. `live` is whether this run redraws stage
    /// rows in place; without it both streams pass through untouched.
    pub fn new(out: &'a mut dyn Write, err: &'a mut dyn Write, live: bool) -> Self {
        LiveConsole {
            state: RefCell::new(State {
                out,
                err,
                live,
                line: Line::Clean,
                pending: Vec::new(),
                err_open: false,
            }),
        }
    }

    /// The stdout half.
    pub fn stdout(&self) -> ConsoleStream<'_, 'a> {
        ConsoleStream {
            console: self,
            target: Target::Stdout,
        }
    }

    /// The stderr half.
    pub fn stderr(&self) -> ConsoleStream<'_, 'a> {
        ConsoleStream {
            console: self,
            target: Target::Stderr,
        }
    }

    /// End the run's output: end a row a stage left open (an interrupted or
    /// abandoned stage) or unfinished stdout text with a newline, so the last
    /// progress the run reached stays visible, then any held or open partial
    /// stderr line, so the shell prompt that follows starts on its own line.
    pub fn finish(&self) -> std::io::Result<()> {
        let mut state = self.state.borrow_mut();
        if !state.live {
            return Ok(());
        }
        // Each step runs even if an earlier one failed: stderr may still
        // be deliverable when stdout is not. The first failure is reported.
        let ended = if state.line == Line::Clean {
            Ok(())
        } else {
            state.line = Line::Clean;
            state.out.write_all(b"\n")
        };
        let released = state.release_pending(true);
        let closed = if state.err_open {
            state.err_open = false;
            state.err.write_all(b"\n")
        } else {
            Ok(())
        };
        ended.and(released).and(closed)
    }
}

/// One half of a [`LiveConsole`].
pub struct ConsoleStream<'c, 'a> {
    console: &'c LiveConsole<'a>,
    target: Target,
}

impl Write for ConsoleStream<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut state = self.console.state.borrow_mut();
        match (state.live, self.target) {
            (false, Target::Stdout) => state.out.write_all(bytes)?,
            (false, Target::Stderr) => state.err.write_all(bytes)?,
            (true, Target::Stdout) => state.write_stdout(bytes)?,
            (true, Target::Stderr) => state.write_stderr(bytes)?,
        }
        Ok(bytes.len())
    }

    /// Flushing stderr releases a held partial line: an explicit flush asks
    /// for the bytes to be visible now.
    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self.console.state.borrow_mut();
        match self.target {
            Target::Stdout => state.out.flush(),
            Target::Stderr => {
                if state.live {
                    state.release_pending(false)?;
                }
                state.err.flush()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both streams captured into one ordered transcript, the way a terminal
    /// receives them, with each chunk tagged by stream.
    #[derive(Default)]
    struct Terminal {
        chunks: Vec<(Target, Vec<u8>)>,
    }

    impl Terminal {
        fn bytes(&self) -> Vec<u8> {
            self.chunks
                .iter()
                .flat_map(|(_, bytes)| bytes.iter().copied())
                .collect()
        }

        fn stream(&self, target: Target) -> Vec<u8> {
            self.chunks
                .iter()
                .filter(|(owner, _)| *owner == target)
                .flat_map(|(_, bytes)| bytes.iter().copied())
                .collect()
        }
    }

    struct Half<'t> {
        terminal: &'t RefCell<Terminal>,
        target: Target,
    }

    impl Write for Half<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.terminal
                .borrow_mut()
                .chunks
                .push((self.target, bytes.to_vec()));
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `script` against a console over a shared transcript.
    fn run(live: bool, script: impl FnOnce(&LiveConsole<'_>)) -> Terminal {
        let terminal = RefCell::new(Terminal::default());
        {
            let mut out = Half {
                terminal: &terminal,
                target: Target::Stdout,
            };
            let mut err = Half {
                terminal: &terminal,
                target: Target::Stderr,
            };
            let console = LiveConsole::new(&mut out, &mut err, live);
            script(&console);
        }
        terminal.into_inner()
    }

    const ROW: &[u8] = b"\r\x1b[K[1/5] Repos      /        dotfiles     0s";
    const CLOSE: &[u8] = b"\r\x1b[K[1/5] Repos      failed   1 repo failed     1s\n";

    #[test]
    fn stderr_line_erases_an_open_row_first() {
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console.stderr().write_all(b"    fatal: gone\n").unwrap();
            console.stdout().write_all(CLOSE).unwrap();
        });
        let mut expected = ROW.to_vec();
        expected.extend_from_slice(b"\r\x1b[K    fatal: gone\n");
        expected.extend_from_slice(CLOSE);
        assert_eq!(terminal.bytes(), expected);
        // The erase is stdout's own: stderr carries only the message.
        assert_eq!(terminal.stream(Target::Stderr), b"    fatal: gone\n");
    }

    #[test]
    fn foreign_stdout_line_erases_an_open_row_first() {
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console
                .stdout()
                .write_all(b"Done with errors in 1s.\n")
                .unwrap();
        });
        let mut expected = ROW.to_vec();
        expected.extend_from_slice(b"\r\x1b[KDone with errors in 1s.\n");
        assert_eq!(terminal.bytes(), expected);
    }

    #[test]
    fn row_redraws_and_closes_pass_through_unchanged() {
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console.stdout().write_all(ROW).unwrap();
            console.stdout().write_all(CLOSE).unwrap();
        });
        assert_eq!(terminal.bytes(), [ROW, ROW, CLOSE].concat());
    }

    #[test]
    fn closed_or_cleared_rows_need_no_erase() {
        // After a close (newline) or a bare clear the cursor is already at
        // column zero, so nothing extra is written before the message.
        for prefix in [CLOSE, b"\r\x1b[K".as_slice()] {
            let terminal = run(true, |console| {
                console.stdout().write_all(ROW).unwrap();
                console.stdout().write_all(prefix).unwrap();
                console.stderr().write_all(b"  warning: x\n").unwrap();
            });
            assert_eq!(
                terminal.bytes(),
                [ROW, prefix, b"  warning: x\n".as_slice()].concat()
            );
        }
    }

    #[test]
    fn fragmented_stdout_line_is_not_erased_between_fragments() {
        // `writeln!` with arguments reaches the stream in several writes;
        // only a row (a line that began with a carriage return) is erased.
        let terminal = run(true, |console| {
            let short = "abc";
            write!(console.stdout(), "  continuing with dot {short}").unwrap();
            console.stdout().write_all(b"\n").unwrap();
            console.stderr().write_all(b"  warning: x\n").unwrap();
        });
        assert_eq!(
            terminal.bytes(),
            b"  continuing with dot abc\n  warning: x\n".as_slice()
        );
    }

    #[test]
    fn partial_stderr_line_survives_a_redraw() {
        // A child's diagnostic arrives in fragments with a heartbeat redraw
        // between them; the redraw must neither erase nor split the line.
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console.stderr().write_all(b"error: ").unwrap();
            console.stdout().write_all(ROW).unwrap();
            console.stderr().write_all(b"boom\nwarning: ").unwrap();
            console.stderr().write_all(b"tail\n").unwrap();
        });
        let mut expected = [ROW, ROW].concat();
        expected.extend_from_slice(b"\r\x1b[Kerror: boom\nwarning: tail\n");
        assert_eq!(terminal.bytes(), expected);
    }

    #[test]
    fn held_fragment_is_released_inside_its_stage_when_the_row_closes() {
        // An unterminated diagnostic must not drift past its stage's close
        // (or the next stage's output): it is released as its own line first.
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console.stderr().write_all(b"error: no newline").unwrap();
            console.stdout().write_all(CLOSE).unwrap();
        });
        assert_eq!(
            terminal.bytes(),
            [ROW, b"\r\x1b[Kerror: no newline\n".as_slice(), CLOSE].concat()
        );
        let foreign = run(true, |console| {
            console.stderr().write_all(b"error: no newline").unwrap();
            console.stdout().write_all(b"Done in 1s.\n").unwrap();
        });
        assert_eq!(foreign.bytes(), b"error: no newline\nDone in 1s.\n");
    }

    #[test]
    fn flushed_fragment_is_ended_before_a_redraw_overwrites_it() {
        let terminal = run(true, |console| {
            console.stderr().write_all(b"prompt? ").unwrap();
            console.stderr().flush().unwrap();
            console.stderr().write_all(b"more").unwrap();
            console.stderr().flush().unwrap();
            console.stdout().write_all(ROW).unwrap();
        });
        assert_eq!(
            terminal.bytes(),
            [b"prompt? more\n".as_slice(), ROW].concat()
        );
    }

    #[test]
    fn row_close_ends_an_open_fragment_once() {
        // A flushed fragment is on screen and more is held behind it: the
        // close releases the rest and ends the line exactly once.
        let terminal = run(true, |console| {
            console.stderr().write_all(b"one ").unwrap();
            console.stderr().flush().unwrap();
            console.stderr().write_all(b"two").unwrap();
            console.stdout().write_all(CLOSE).unwrap();
        });
        assert_eq!(terminal.bytes(), [b"one two\n".as_slice(), CLOSE].concat());
    }

    #[test]
    fn stderr_is_delivered_even_when_the_row_erase_fails() {
        /// A terminal that takes the row, then goes away.
        struct HangsUp {
            accepted: bool,
        }
        impl Write for HangsUp {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if std::mem::replace(&mut self.accepted, true) {
                    return Err(std::io::Error::other("terminal gone"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut out = HangsUp { accepted: false };
        let mut err = Vec::new();
        {
            let console = LiveConsole::new(&mut out, &mut err, true);
            console.stdout().write_all(ROW).unwrap();
            // The erase fails; the message still reaches stderr.
            assert!(console.stderr().write_all(b"error: kept\n").is_err());
            assert!(console.stderr().write_all(b"tail").is_ok());
            assert!(console.finish().is_err());
        }
        assert_eq!(err, b"error: kept\ntail\n");
    }

    #[test]
    fn finish_ends_the_row_then_a_held_fragment() {
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console.stderr().write_all(b"tail without newline").unwrap();
            console.finish().unwrap();
        });
        assert_eq!(
            terminal.bytes(),
            [ROW, b"\ntail without newline\n".as_slice()].concat()
        );
        let flushed = run(true, |console| {
            console.stderr().write_all(b"open").unwrap();
            console.stderr().flush().unwrap();
            console.finish().unwrap();
        });
        assert_eq!(flushed.bytes(), b"open\n");
        let text = run(true, |console| {
            console.stdout().write_all(b"unfinished").unwrap();
            console.finish().unwrap();
        });
        assert_eq!(text.bytes(), b"unfinished\n");
    }

    #[test]
    fn endless_partial_stderr_line_is_released_once_it_reaches_the_limit() {
        let chunk = vec![b'x'; PENDING_LIMIT / 2];
        let terminal = run(true, |console| {
            console.stderr().write_all(&chunk).unwrap();
            console
                .stderr()
                .write_all(&chunk[..chunk.len() - 1])
                .unwrap();
        });
        assert!(terminal.bytes().is_empty(), "released below the limit");
        let terminal = run(true, |console| {
            console.stderr().write_all(&chunk).unwrap();
            console.stderr().write_all(&chunk).unwrap();
        });
        assert_eq!(terminal.stream(Target::Stderr).len(), PENDING_LIMIT);
        // A tail after an early newline counts against the limit too, and
        // only the tail: the line before it is written on its own.
        let mut flood = b"head\n".to_vec();
        flood.extend(std::iter::repeat_n(b'y', PENDING_LIMIT - 1));
        let terminal = run(true, |console| {
            console.stderr().write_all(&flood).unwrap();
        });
        assert_eq!(terminal.stream(Target::Stderr), b"head\n");
        flood.push(b'y');
        let terminal = run(true, |console| {
            console.stderr().write_all(&flood).unwrap();
            console.stdout().write_all(ROW).unwrap();
        });
        // The released tail is left open; a redraw ends it first.
        let mut expected = flood.clone();
        expected.push(b'\n');
        expected.extend_from_slice(ROW);
        assert_eq!(terminal.bytes(), expected);
    }

    #[test]
    fn finish_ends_an_abandoned_row_with_a_newline() {
        let terminal = run(true, |console| {
            console.stdout().write_all(ROW).unwrap();
            console.finish().unwrap();
        });
        assert_eq!(terminal.bytes(), [ROW, b"\n".as_slice()].concat());
        let closed = run(true, |console| {
            console.stdout().write_all(CLOSE).unwrap();
            console.finish().unwrap();
        });
        assert_eq!(closed.bytes(), CLOSE);
    }

    #[test]
    fn without_live_mode_both_streams_pass_through_byte_for_byte() {
        let terminal = run(false, |console| {
            console
                .stdout()
                .write_all(b"[1/5] Repos      running  x  0s")
                .unwrap();
            console.stderr().write_all(b"error: ").unwrap();
            console.stdout().write_all(b"\n").unwrap();
            console.stderr().write_all(b"boom\n").unwrap();
            console.stderr().write_all(b"tail").unwrap();
            console.stderr().flush().unwrap();
            console.finish().unwrap();
        });
        assert_eq!(
            terminal.chunks,
            vec![
                (Target::Stdout, b"[1/5] Repos      running  x  0s".to_vec()),
                (Target::Stderr, b"error: ".to_vec()),
                (Target::Stdout, b"\n".to_vec()),
                (Target::Stderr, b"boom\n".to_vec()),
                (Target::Stderr, b"tail".to_vec()),
            ]
        );
    }

    #[test]
    fn line_state_follows_terminal_semantics() {
        assert_eq!(Line::Clean.after(b"text"), Line::Text);
        assert_eq!(Line::Clean.after(b"text\n"), Line::Clean);
        assert_eq!(Line::Clean.after(ROW), Line::Row);
        assert_eq!(Line::Row.after(b" more"), Line::Row);
        assert_eq!(Line::Row.after(b"\r"), Line::Row);
        assert_eq!(Line::Clean.after(b"\r"), Line::Clean);
        assert_eq!(Line::Clean.after(b"text\r"), Line::Row);
        assert_eq!(Line::Text.after(b"\r"), Line::Row);
        assert_eq!(Line::Row.after(ERASE_LINE), Line::Clean);
        assert_eq!(Line::Text.after(b"done\n[2/5] x"), Line::Text);
        assert_eq!(Line::Clean.after(b"a\n\r\x1b[K[2/5] y"), Line::Row);
        assert_eq!(Line::Row.after(b""), Line::Row);
    }
}
