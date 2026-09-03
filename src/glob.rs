//! Shell `case`-pattern matching over bytes (C locale).
//!
//! Shared by family-key filtering (`families.sh`, where caller patterns
//! arrive via an unquoted `$pattern`). Byte-oriented because the shell
//! operates on bytes under `LC_ALL=C`: `*`, `?`, and `[...]` classes
//! apply per byte, and `/` has no special status inside `case`
//! patterns (unlike pathname expansion). `|` from a variable is
//! literal — alternation is `case` syntax parsed before expansion, so
//! an expanded `a|b` never splits (pinned by
//! `pipe_from_variable_is_literal`).
//!
//! Pinned against bash 5.x, the engine's runtime (resolved via
//! `DOT_BASH`). The macOS system bash 3.2 trampoline differs in at
//! least one corner — a trailing lone backslash (`a\` matches `a\`
//! on 5.x, not on 3.2) — and is not a supported engine runtime, so
//! the differential harness resolves bash via PATH exactly like the
//! shell suite does.

/// Match `text` against shell glob `pattern`.
///
/// Supports `*` (any run, possibly empty), `?` (exactly one byte),
/// `\x` (literal `x`; a trailing lone `\` is literal), and `[...]`
/// classes with `!`/`^` negation, `a-z` ranges, and a literal leading
/// `]`. An unclosed `[` is a literal `[`. Backslash inside a class is
/// literal (POSIX); descending ranges contribute their endpoints as
/// literals (bash never matches the span itself).
pub fn matches(pattern: &[u8], text: &[u8]) -> bool {
    let (mut px, mut tx) = (0usize, 0usize);
    // Backtrack point: pattern index just past the last `*`, and the
    // text index where that `*` currently ends.
    let (mut star_px, mut star_tx) = (None::<usize>, 0usize);
    while tx < text.len() {
        if px < pattern.len() {
            match pattern[px] {
                b'*' => {
                    star_px = Some(px + 1);
                    star_tx = tx;
                    px += 1;
                    continue;
                }
                b'?' => {
                    px += 1;
                    tx += 1;
                    continue;
                }
                b'\\' => {
                    let literal = pattern.get(px + 1).copied().unwrap_or(b'\\');
                    if literal == text[tx] {
                        px += if px + 1 < pattern.len() { 2 } else { 1 };
                        tx += 1;
                        continue;
                    }
                }
                b'[' => {
                    if let Some(consumed) = class_match(&pattern[px..], text[tx]) {
                        px += consumed;
                        tx += 1;
                        continue;
                    }
                    // Unclosed bracket: literal `[` (falls into the
                    // byte comparison below via the mismatch path only
                    // when the bytes differ; handle equality here).
                    if pattern[px] == text[tx] {
                        px += 1;
                        tx += 1;
                        continue;
                    }
                }
                byte => {
                    if byte == text[tx] {
                        px += 1;
                        tx += 1;
                        continue;
                    }
                }
            }
        }
        // Mismatch (or pattern exhausted): give the last `*` one more
        // byte, or fail when there is no `*` to extend.
        match star_px {
            Some(resume) => {
                star_tx += 1;
                tx = star_tx;
                px = resume;
            }
            None => return false,
        }
    }
    while px < pattern.len() && pattern[px] == b'*' {
        px += 1;
    }
    px == pattern.len()
}

/// Try to match `class` (starting at `[`) against `byte`.
///
/// Returns the consumed pattern length (including brackets) on a
/// successful match, `None` when the class does not match or is
/// unclosed (the caller then treats `[` literally).
///
/// Reverse-engineered against bash 5.3 (`LC_ALL=C`) and pinned by the
/// `glob_exotics_match_shell_case` oracle, which rules over these
/// comments whenever they disagree:
/// - `\X` contributes `X` as a literal member (`\]` closes nothing,
///   `\\` contributes `\`, `\-` contributes `-`); a trailing lone
///   `\` is a literal member, usually leaving the class unclosed.
/// - A range triple contributes its span when ascending and nothing
///   at all when descending — the start is not scored, the end is
///   consumed (`[c-a]` matches neither endpoint).
/// - A leading or trailing `-` is literal and stages as a range
///   start (`[--0]` spans). A `-` after a consumed range start is
///   literal too (`[a-c-e-g]` keeps `-` while `e-g` still ranges).
/// - After a DESCENDING range, the next `-` stages shadowed: it may
///   still open a range (`[c-A--b]` spans `-`..`b`) but never scores
///   literally itself (`[\\--0]` refuses `-` while `0` matches).
fn class_match(class: &[u8], byte: u8) -> Option<usize> {
    debug_assert_eq!(class.first(), Some(&b'['));
    let mut ix = 1;
    let negate = matches!(class.get(ix), Some(b'!') | Some(b'^'));
    if negate {
        ix += 1;
    }
    let mut matched = false;
    // A `]` in first position is literal.
    let mut first = true;
    // Previous member: a staged range start, or a literal (including a
    // leading `-`) that a following `-` may still range from.
    let mut prev: Option<u8> = None;
    // Set by a descending range: the next staged dash ranges but
    // never scores. Cleared by anything that is not a dash.
    let mut dash_shadow = false;
    loop {
        let current = *class.get(ix)?;
        if current == b']' && !first {
            matched |= !dash_shadow && prev.is_some_and(|start| start == byte);
            ix += 1;
            return if matched != negate { Some(ix) } else { None };
        }
        first = false;
        // No let-chains (MSRV 1.85): nested `if`s instead.
        if current == b'\\' {
            if let Some(&escaped) = class.get(ix + 1) {
                // Escaped member: scores like an ordinary one and stages
                // as a range start (`[\--0]` spans from `-`).
                matched |= !dash_shadow && prev.is_some_and(|start| start == byte);
                prev = Some(escaped);
                dash_shadow = false;
                ix += 2;
                continue;
            }
        }
        if current == b'-' {
            match (prev, class.get(ix + 1)) {
                (Some(start), Some(&end)) if end != b']' => {
                    if start <= end {
                        matched |= (start..=end).contains(&byte);
                        dash_shadow = false;
                    } else {
                        // Void triple: start, dash, and end vanish;
                        // arm the shadow for the next dash.
                        dash_shadow = true;
                    }
                    prev = None;
                    ix += 2;
                    continue;
                }
                // Leading, trailing, or post-range dash: literal, and
                // staged for ranging. A shadowed dash keeps its shadow:
                // it stages but never scores itself.
                _ => {
                    matched |= !dash_shadow && prev.is_some_and(|start| start == byte);
                    matched |= !dash_shadow && byte == b'-';
                    prev = Some(b'-');
                    ix += 1;
                    continue;
                }
            }
        }
        // Ordinary member: score the previous one, stage this.
        matched |= !dash_shadow && prev.is_some_and(|start| start == byte);
        prev = Some(current);
        dash_shadow = false;
        ix += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_question_and_literals() {
        assert!(matches(b"*", b""));
        assert!(matches(b"*", b"anything/at-all"));
        assert!(matches(b"a*c", b"ac"));
        assert!(matches(b"a*c", b"abc"));
        assert!(!matches(b"a*c", b"ab"));
        assert!(matches(b"?", b"x"));
        assert!(!matches(b"?", b""));
        assert!(!matches(b"??", b"x"));
        assert!(matches(b"a?c", b"abc"));
        // `|` from a variable is literal, not alternation.
        assert!(!matches(b"a|b", b"a"));
        assert!(matches(b"a|b", b"a|b"));
    }

    #[test]
    fn backslash_escapes() {
        assert!(matches(b"a\\*b", b"a*b"));
        assert!(!matches(b"a\\*b", b"axxb"));
        assert!(matches(b"\\\\", b"\\"));
        // Trailing lone backslash is literal.
        assert!(matches(b"a\\", b"a\\"));
        assert!(!matches(b"a\\", b"ab"));
    }

    #[test]
    fn classes() {
        assert!(matches(b"[abc]", b"b"));
        assert!(!matches(b"[abc]", b"d"));
        assert!(matches(b"[!abc]", b"d"));
        assert!(!matches(b"[!abc]", b"a"));
        assert!(matches(b"[^abc]", b"d"));
        assert!(matches(b"[a-z]", b"m"));
        assert!(!matches(b"[a-z]", b"M"));
        // Leading `]` is literal.
        assert!(matches(b"[]a]", b"]"));
        assert!(matches(b"[]a]", b"a"));
        assert!(!matches(b"[]a]", b"b"));
        // Unclosed bracket is literal.
        assert!(matches(b"[ab", b"[ab"));
        assert!(!matches(b"[ab", b"a"));
    }

    #[test]
    fn escapes_in_classes() {
        // `\X` contributes `X` as a literal member and never closes:
        // `[\]]` is the one-char class {`]`}.
        assert!(matches(b"[\\]]", b"]"));
        assert!(!matches(b"[\\]]", b"\\"));
        assert!(matches(b"[a\\]c]", b"a"));
        assert!(matches(b"[a\\]c]", b"]"));
        assert!(matches(b"[a\\]c]", b"c"));
        assert!(!matches(b"[a\\]c]", b"a]c"));
        // `\\` contributes a backslash member ...
        assert!(matches(b"[\\\\]", b"\\"));
        assert!(matches(b"[a\\\\c]", b"\\"));
        assert!(matches(b"[a\\\\c]", b"a"));
        // ... which still opens ranges: `[a\\-c]` spans `\`..`c`.
        assert!(matches(b"[a\\\\-c]", b"b"));
        assert!(matches(b"[a\\\\-c]", b"\\"));
        assert!(!matches(b"[a\\\\-c]", b"-"));
        // An escaped dash stages as a range start.
        assert!(matches(b"[\\--0]", b"-"));
        assert!(matches(b"[\\--0]", b"."));
        assert!(matches(b"[\\--0]", b"0"));
        assert!(!matches(b"[\\--0]", b"\\"));
    }

    #[test]
    fn ranges_and_dashes() {
        assert!(matches(b"[a-c]", b"b"));
        assert!(!matches(b"[a-c]", b"d"));
        assert!(matches(b"[-ac]", b"-"));
        assert!(matches(b"[-ac]", b"a"));
        assert!(!matches(b"[-ac]", b"b"));
        assert!(matches(b"[ac-]", b"-"));
        assert!(matches(b"[ac-]", b"c"));
        assert!(!matches(b"[ac-]", b"b"));
        // Leading dash still ranges from itself: `[--0]` spans.
        assert!(matches(b"[--0]", b"."));
        assert!(matches(b"[--0]", b"-"));
        assert!(matches(b"[--0]", b"0"));
        assert!(!matches(b"[--0]", b"1"));
        // A lone leading dash is literal: `[-a]` is `-` or `a`.
        assert!(matches(b"[-a]", b"-"));
        assert!(matches(b"[-a]", b"a"));
        assert!(!matches(b"[-a]", b"."));
        // Descending ranges contribute nothing, endpoints included
        // (bash oracle: `glob_exotics_match_shell_case`).
        assert!(!matches(b"[c-a]", b"c"));
        assert!(!matches(b"[c-a]", b"a"));
        assert!(!matches(b"[c-a]", b"b"));
        // Negated range.
        assert!(matches(b"[!a-c]", b"d"));
        assert!(!matches(b"[!a-c]", b"b"));
        // A dash after a consumed range is literal, and later groups
        // still range: `[a-c-e-g]` keeps `-` but refuses `d`.
        assert!(matches(b"[a-c-e-g]", b"b"));
        assert!(matches(b"[a-c-e-g]", b"-"));
        assert!(matches(b"[a-c-e-g]", b"f"));
        assert!(!matches(b"[a-c-e-g]", b"d"));
        assert!(matches(b"[a-c-]", b"-"));
        // After a DESCENDING range the next dash stages shadowed: it
        // opens ranges but never scores (`[\\--0]` keeps only `0`).
        assert!(matches(b"[\\\\--0]", b"0"));
        assert!(!matches(b"[\\\\--0]", b"-"));
        assert!(!matches(b"[\\\\--0]", b"."));
        // ... while a shadowed dash still ranges (`[c-A--b]` spans).
        assert!(matches(b"[c-A--b]", b"-"));
        assert!(matches(b"[c-A--b]", b"."));
        assert!(matches(b"[c-A--b]", b"b"));
    }

    #[test]
    fn pipe_from_variable_is_literal() {
        // `case $key in $pattern)` with `pattern='a|b'` matches only
        // the literal three bytes; there is no alternation after
        // expansion.
        for text in [b"a".as_slice(), b"b".as_slice(), b"ab".as_slice()] {
            assert!(!matches(b"a|b", text), "text: {text:?}");
        }
        assert!(matches(b"a|b", b"a|b"));
    }
}
