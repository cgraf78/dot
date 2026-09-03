//! Marked-block config merging (slice 6: merge layer start).
//!
//! Ports `lib/dot/merge-block.sh` exactly: block assembly with
//! modeline stripping, prefix-matched stripping (single marker and
//! whole family), and atomic publish of hand-managed content plus
//! managed blocks. File effects (sibling temps, digest-skipped
//! writes, atomic replace) reuse [`crate::temp`]; path and move
//! identity errors surface as [`Error`] so callers map to the same
//! exit codes as the shell.
//!
//! Like the shell, every function is a pure function of explicit
//! inputs plus the filesystem: the git source root (for content
//! digests) and the move-tool cache travel in a [`Ctx`].

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use crate::errors::Error;
use crate::temp::{self, MoveCache};

/// Shared inputs for the publishing half: the git source root for
/// content digests (`_dot_stdin_matches_file` runs `git hash-object`
/// under it) and the probed move tool (`_dot_publish_prepared_regular`
/// memoizes `DOT_MOVE_BIN`/`DOT_MOVE_MODE` per process).
pub struct Ctx<'a> {
    /// Directory `git hash-object` runs under.
    pub source_root: &'a Path,
    /// Move-tool probe cache (one per engine run).
    pub cache: &'a mut MoveCache,
}

/// Whitespace the shell `[[:space:]]` class matches in the C locale:
/// space, tab, newline, vertical tab, form feed, carriage return.
/// (Rust `char::is_whitespace` is Unicode-aware and would also trim
/// non-breaking spaces the shell keeps.)
fn is_shell_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// Trim shell whitespace from both ends, byte-exact. Shared with
/// [`crate::merges`]: the shell repeats this `${var%%...}`
/// idiom in every merge helper.
pub fn trim_shell_space(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && is_shell_space(bytes[start]) {
        start += 1;
    }
    while end > start && is_shell_space(bytes[end - 1]) {
        end -= 1;
    }
    &text[start..end]
}

/// True for a modeline the shell `grep -v` pair drops: `#`,
/// shell whitespace, then `vim:` — or `#`, shell whitespace, then
/// `-*-`. (The shell sees `\r` as content, so matching splits on
/// `\n` only instead of `str::lines`, which would eat it.)
fn is_modeline(line: &str) -> bool {
    let Some(after_hash) = line.strip_prefix('#') else {
        return false;
    };
    let head = after_hash.trim_start_matches(is_shell_space_char);
    head.starts_with("vim:") || head.starts_with("-*-")
}

/// `char` view of [`is_shell_space`] for prefix trimming.
fn is_shell_space_char(ch: char) -> bool {
    ch.is_ascii() && is_shell_space(ch as u8)
}

/// `_mb_build`: assemble a marked block. `marker` carries its own
/// comment prefix (callers pass `"# <name>"`); `body` loses modelines
/// and surrounding whitespace first. No trailing newline, like the
/// shell `printf` (callers join blocks with blank lines).
pub fn build(marker: &str, source: &str, body: &str) -> String {
    let kept: Vec<&str> = lines_of(body)
        .into_iter()
        .filter(|line| !is_modeline(line))
        .collect();
    let joined = kept.join("\n");
    let trimmed = trim_shell_space(&joined);
    format!(
        "{marker} begin\n# DO NOT EDIT: changes will be overwritten by dot update\n# source: {source}\n{trimmed}\n{marker} end"
    )
}

/// Split into lines the way the shell `printf '%s\n'` + capture
/// pipeline sees them: one element per newline, with a trailing
/// partial line kept (the added newline is stripped on capture).
fn lines_of(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// Rejoin lines, dropping the trailing newline the command
/// substitution always strips.
fn join_lines(lines: &[&str]) -> String {
    let mut out = lines.join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// `_mb_strip`: remove the first line containing `{marker} begin`
/// through the next line containing `{marker} end` (inclusive; to
/// end of input when no end line follows). A missing marker returns
/// the input with trailing newlines stripped, exactly like the
/// shell `printf | sed` capture. Matching is a plain substring on
/// lines, which is what the shell BRE amounts to once the marker is
/// escaped.
pub fn strip(marker: &str, input: &str) -> String {
    let begin = format!("{marker} begin");
    let end = format!("{marker} end");
    let lines = lines_of(input);
    if !lines.iter().any(|line| line.contains(&begin)) {
        return join_lines(&lines);
    }
    // Like `sed "/begin/,/end/d"`: every begin line opens a range
    // closed by the next end line (or end of input), and all ranges
    // drop — not just the first.
    let mut kept: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].contains(&begin) {
            index += 1;
            while index < lines.len() && !lines[index].contains(&end) {
                index += 1;
            }
            index += 1;
        } else {
            kept.push(lines[index]);
            index += 1;
        }
    }
    join_lines(&kept)
}

/// `_mb_strip_family`: remove every block whose begin/end lines both
/// start with `marker_prefix` and end with `" begin"` / `" end"`.
/// Lines inside a block are skipped even when they match neither
/// (only a prefix + `" end"` line closes the block).
pub fn strip_family(marker_prefix: &str, input: &str) -> String {
    let mut kept = Vec::new();
    let mut in_block = false;
    for line in lines_of(input) {
        let opens = line.starts_with(marker_prefix) && line.ends_with(" begin");
        let closes = line.starts_with(marker_prefix) && line.ends_with(" end");
        if !in_block && opens {
            in_block = true;
            continue;
        }
        if in_block {
            if closes {
                in_block = false;
            }
            continue;
        }
        kept.push(line);
    }
    join_lines(&kept)
}

/// Squeeze repeated empty lines to one (the shell `awk` keeps every
/// non-empty line and an empty line only after a non-empty one,
/// including one leading blank).
fn squeeze_empty(text: &str) -> String {
    let mut out = Vec::new();
    let mut blank = false;
    for line in lines_of(text) {
        if line.is_empty() {
            if !blank {
                blank = true;
                out.push(line);
            }
        } else {
            blank = false;
            out.push(line);
        }
    }
    out.join("\n")
}

/// Read a file the way `current="$(cat "$dst")"` does: bytes as
/// text with trailing newlines stripped. (NUL bytes cannot survive
/// a shell variable; configs are text.)
fn read_current(path: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        context: "read merge destination",
        source,
    })?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(join_lines(&lines_of(&text)))
}

/// `_mb_finalize`: normalize `rest`, append `blocks` after a blank
/// line each, and atomically publish when the destination differs.
/// Skips the write (but still succeeds) when the destination already
/// holds the result.
pub fn finalize(dst: &Path, rest: &str, blocks: &[&str], ctx: &mut Ctx<'_>) -> Result<(), Error> {
    let squeezed = squeeze_empty(&format!("{rest}\n"));
    let trimmed = trim_shell_space(&squeezed);
    let mut result = trimmed.to_string();
    for block in blocks {
        if result.is_empty() {
            result.push_str(block);
        } else {
            result.push_str("\n\n");
            result.push_str(block);
        }
    }
    result.push('\n');
    if temp::stdin_matches_file(ctx.source_root, result.as_bytes(), dst).unwrap_or(false) {
        return Ok(());
    }
    let tmp = temp::sibling_tmp_for(dst)?;
    if let Err(source) = std::fs::write(&tmp, result.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Io {
            context: "write merge temp",
            source,
        });
    }
    if let Err(source) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Io {
            context: "chmod merge temp",
            source,
        });
    }
    if let Err(error) = temp::publish_prepared_regular(&tmp, dst, ctx.cache) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Create the destination parent (`mkdir -p` + mode 700) when it is
/// not already a directory, like `_mb_merge` / `_mb_merge_family`.
fn ensure_parent(dst: &Path) -> Result<(), Error> {
    let dir = dst.parent().unwrap_or_else(|| Path::new("/"));
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|source| Error::Io {
        context: "create merge parent",
        source,
    })?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        Error::Io {
            context: "chmod merge parent",
            source,
        }
    })
}

/// Marker for one block: text before the first `" begin"`, i.e. the
/// shell `${block%% begin*}`.
fn block_marker(block: &str) -> &str {
    match block.find(" begin") {
        Some(index) => &block[..index],
        None => block,
    }
}

/// `_mb_merge`: strip each block's own marker from the current file,
/// then finalize with the blocks appended.
pub fn merge(dst: &Path, blocks: &[&str], ctx: &mut Ctx<'_>) -> Result<(), Error> {
    ensure_parent(dst)?;
    let current = if dst.is_file() {
        read_current(dst)?
    } else {
        String::new()
    };
    let mut rest = current;
    for block in blocks {
        let marked = strip(block_marker(block), &rest);
        rest = marked;
    }
    finalize(dst, &rest, blocks, ctx)
}

/// `_mb_merge_family`: strip the whole marker family from the
/// current file, then finalize with the blocks appended.
pub fn merge_family(
    dst: &Path,
    marker_prefix: &str,
    blocks: &[&str],
    ctx: &mut Ctx<'_>,
) -> Result<(), Error> {
    ensure_parent(dst)?;
    let current = if dst.is_file() {
        read_current(dst)?
    } else {
        String::new()
    };
    let rest = strip_family(marker_prefix, &current);
    finalize(dst, &rest, blocks, ctx)
}
