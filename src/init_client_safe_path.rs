//! The shared path guard of `lib/dot/init-client.sh`: the small
//! spelling gate every lane validates home-relative paths with.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only `_dot_init_safe_relative_path` (lines
//! 27-45): a home-relative spelling with no escapes and no `.git`
//! component. Its sibling `_dot_init_safe_value` (lines 23-25)
//! stays a private helper, exactly the only other line range this
//! chapter touches.
//!
//! Lane map, so the integrator can stack without overlap: this is
//! the first lane to publish the guard as `init_client_*` API. The
//! base tree already carries the same predicate as
//! [`crate::repos_overlays::init_safe_relative_path`], which takes
//! `&str` shaped for its own call sites; this module takes raw
//! bytes instead, so non-UTF8 spellings read exactly like the
//! shell's instead of stopping at the UTF-8 boundary. All other
//! init chapters — transaction, identity, generation, entry,
//! candidate, records, delete, plan, publish, rollback, git, adopt,
//! resume — stay for their own lanes.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Byte-fidelity boundary: `case` arms with `*` match across `/`,
//! so every arm below is a plain prefix/suffix/substring check, and
//! `${component,,}` under `LC_ALL=C` folds ASCII only —
//! [`safe_relative_path`] uses ASCII case folding for the same
//! reason, never Unicode `to_lowercase`. Inputs that survive the
//! `case` arms split cleanly on `/` (no leading, trailing, or
//! doubled separators left), so the component loop needs none of
//! the shell `read -a` framing.

/// `_dot_init_safe_value`: nonempty with no tab, newline, or
/// carriage-return bytes.
fn safe_value(path: &[u8]) -> bool {
    !path.is_empty()
        && !path
            .iter()
            .any(|byte| matches!(byte, b'\t' | b'\n' | b'\r'))
}

/// `_dot_init_safe_relative_path`: a home-relative path with no
/// escapes and no `.git` component. Pure predicate over the raw
/// spelling: empty, absolute, dot-led, dot-trailing,
/// double-slash, and control-byte spellings refuse, and every
/// `/`-separated component must differ from `.git` under ASCII
/// folding, exactly like the shell's `${component,,}` comparison.
pub fn safe_relative_path(path: &[u8]) -> bool {
    if !safe_value(path) {
        return false;
    }
    if path.starts_with(b"/")
        || path == b"."
        || path == b".."
        || path.starts_with(b"./")
        || path.starts_with(b"../")
        || path.windows(3).any(|window| window == b"/./")
        || path.windows(4).any(|window| window == b"/../")
        || path.ends_with(b"/")
        || path.ends_with(b"/.")
        || path.ends_with(b"/..")
        || path.windows(2).any(|window| window == b"//")
    {
        return false;
    }
    !path
        .split(|byte| *byte == b'/')
        .any(|component| component.eq_ignore_ascii_case(b".git"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_spellings() {
        assert!(safe_relative_path(b"a"));
        assert!(safe_relative_path(b"a/b/c.txt"));
        assert!(safe_relative_path(b".hidden/a"));
        assert!(safe_relative_path(b".gitignore"));
    }

    #[test]
    fn rejects_escapes() {
        for path in [
            &b""[..],
            b"/",
            b"/a",
            b"a/",
            b".",
            b"..",
            b"./a",
            b"../a",
            b"a/./b",
            b"a/../b",
            b"a/.",
            b"a/..",
            b"a//b",
            b"a\tb",
            b"a\nb",
            b"a\rb",
        ] {
            assert!(!safe_relative_path(path), "accepted {path:?}");
        }
    }

    #[test]
    fn rejects_git_components_ascii_only() {
        for path in [&b".git"[..], b".GIT", b"a/.Git/b", b"a/b/.gIT"] {
            assert!(!safe_relative_path(path), "accepted {path:?}");
        }
        // Non-ASCII lookalikes never fold under `LC_ALL=C`.
        assert!(safe_relative_path(".GİT/a".as_bytes()));
        // Near misses stay valid.
        assert!(safe_relative_path(b"a/.github/b"));
        assert!(safe_relative_path(b"a/x.git/b"));
    }
}
