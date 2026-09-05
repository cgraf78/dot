//! The intent publisher of `lib/dot/init-client.sh`: recording one
//! entry's pending intent before its stage is prepared.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only `_dot_init_publish_intent` (lines 927-938):
//! derive the transaction stage for the entry ([`publish_intent`]),
//! validate the existing record when one is already present, or
//! publish the pending line at mode `0600` when the path is still
//! free.
//!
//! Lane map, so the integrator can stack without overlap: the
//! per-entry staging family (`_dot_init_entry_stage`,
//! `_dot_init_entry_intent`, `_dot_init_write_private_line`) lives
//! on `rust-port-slice-46` (`init_client_entry`) and is unmerged
//! here, so those three call sites cross as closures in
//! [`PublishIntentHooks`] — one per shell call site, the way the
//! rollback lane binds its verifiers. The sibling `publish_one`,
//! the worktree publisher, and the published-state recovery family
//! stay for their own lanes. Nothing above line 927 and nothing
//! below line 938 is ported here.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_NONCE` global (inside the stage derivation) and the
//! worktree root from `HOME`. Library code must not read process
//! environment behind the engine, so the stage derivation crosses
//! as a closure (capturing whatever run identity its owner needs)
//! and the worktree root crosses as `home`. `REPLY`-carried outputs
//! surface as return values. Every shell refusal in this function
//! is a bare `return 1` with no diagnostic of its own, so every
//! refusal here surfaces as [`Error::Usage`](crate::errors::Error::Usage);
//! diagnostics printed by callees stay owned by their lanes.
//!
//! Byte-fidelity boundary: the `${REPLY#"$HOME"/}` strip keeps the
//! whole stage path on a miss, like the shell's expansion, and the
//! pending line joins its nine tab fields with no trailing spaces,
//! exactly the shell's `$'...'` spelling (the trailing newline is
//! the line publisher's, not this function's).

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::errors::Result;

/// `_dot_init_entry_stage` by position (`path`), returning the
/// transaction-derived stage path for the entry.
pub type EntryStage<'a> = dyn Fn(&[u8]) -> Result<PathBuf> + 'a;

/// `_dot_init_entry_intent` by position (`file mode oid path`),
/// validating the existing record and discarding its fields like
/// the shell's `>/dev/null`.
pub type EntryIntentCheck<'a> = dyn Fn(&Path, &str, &str, &[u8]) -> Result<()> + 'a;

/// `_dot_init_write_private_line` for a fresh intent
/// (`file line`): the shell call site never passes the `true`
/// third argument, so replacement never happens here.
pub type WritePrivateLine<'a> = dyn Fn(&Path, &[u8]) -> Result<()> + 'a;

/// The three out-of-scope call sites the intent publisher needs,
/// one boxed closure each. Boxed (not borrowed) so rows can build
/// the whole set in a helper and move it into the call.
pub struct PublishIntentHooks<'a> {
    /// Runs `_dot_init_entry_stage`.
    pub entry_stage: Box<EntryStage<'a>>,
    /// Runs `_dot_init_entry_intent`.
    pub entry_intent: Box<EntryIntentCheck<'a>>,
    /// Runs `_dot_init_write_private_line`.
    pub write_private_line: Box<WritePrivateLine<'a>>,
}

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Raw bytes of a path, so the `$HOME/` prefix strip behaves like
/// the shell string operation even when `home` has a trailing
/// slash.
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// The shell's `${stage#"$HOME"/}`: strip the worktree prefix only
/// when the bytes match, otherwise keep the whole path — the
/// expansion never fails, it just stops matching.
fn strip_home_prefix<'a>(home: &Path, stage: &'a [u8]) -> &'a [u8] {
    let mut prefix = path_bytes(home).to_vec();
    prefix.push(b'/');
    stage.strip_prefix(&prefix[..]).unwrap_or(stage)
}

/// `_dot_init_publish_intent`: publish the pending intent for one
/// entry (`file mode oid path`). A present intent file validates
/// against the expected triple and stays untouched; an absent one
/// receives the nine-field pending line. Paths stay byte-exact so
/// non-UTF8 entries read like the shell's.
pub fn publish_intent(
    hooks: &PublishIntentHooks<'_>,
    file: &Path,
    mode: &str,
    oid: &str,
    path: &[u8],
    home: &Path,
) -> Result<()> {
    let stage = (hooks.entry_stage)(path)?;
    let stage_rel = strip_home_prefix(home, path_bytes(&stage));
    let mut line = b"pending".to_vec();
    line.push(b'\t');
    line.extend_from_slice(mode.as_bytes());
    line.push(b'\t');
    line.extend_from_slice(oid.as_bytes());
    line.push(b'\t');
    line.extend_from_slice(path);
    line.push(b'\t');
    line.extend_from_slice(stage_rel);
    line.extend_from_slice(b"\t-\t-\t-\t-");
    if exists_lexical(file) {
        return (hooks.entry_intent)(file, mode, oid, path);
    }
    (hooks.write_private_line)(file, &line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_keeps_path_on_miss() {
        let home = Path::new("/home/op");
        assert_eq!(
            strip_home_prefix(home, b"/home/op/a/.dot-init-entry.n.h"),
            b"a/.dot-init-entry.n.h",
        );
        assert_eq!(strip_home_prefix(home, b"/elsewhere/x"), b"/elsewhere/x",);
        // A `home` with a trailing slash keeps the doubled
        // separator behavior of the shell expansion.
        let slashed = Path::new("/home/op/");
        assert_eq!(strip_home_prefix(slashed, b"/home/op//a/x"), b"a/x",);
    }

    #[test]
    fn pending_line_has_nine_fields() {
        let hooks = PublishIntentHooks {
            entry_stage: Box::new(|_| Ok(PathBuf::from("/home/op/.dot-init-entry.n.h"))),
            entry_intent: Box::new(|_, _, _, _| {
                panic!("no intent file in this row");
            }),
            write_private_line: Box::new(|_, line| {
                assert_eq!(
                    line.split(|byte| *byte == b'\t').count(),
                    9,
                    "pending line field count: {line:?}",
                );
                assert!(line.starts_with(b"pending\t100644\thash\tdoc.txt\t"));
                assert!(line.ends_with(b"\t-\t-\t-\t-"));
                Ok(())
            }),
        };
        publish_intent(
            &hooks,
            Path::new("/home/op/intent"),
            "100644",
            "hash",
            b"doc.txt",
            Path::new("/home/op"),
        )
        .expect("publish");
    }

    #[test]
    fn existing_file_validates_instead_of_writing() {
        let hooks = PublishIntentHooks {
            entry_stage: Box::new(|_| Ok(PathBuf::from("/home/op/.dot-init-entry.n.h"))),
            entry_intent: Box::new(|_, mode, oid, path| {
                assert_eq!(mode, "100644");
                assert_eq!(oid, "hash");
                assert_eq!(path, b"doc.txt");
                Ok(())
            }),
            write_private_line: Box::new(|_, _| {
                panic!("must not write over an existing intent");
            }),
        };
        // `exists_lexical` is true for this very source file, so
        // the port must take the validation branch.
        publish_intent(
            &hooks,
            Path::new(file!()),
            "100644",
            "hash",
            b"doc.txt",
            Path::new("/home/op"),
        )
        .expect("validate");
    }
}
