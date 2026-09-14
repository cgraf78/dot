//! Simple base-plus-overlay repo commands from
//! `lib/dot/repos/commands.sh`.
//!
//! The iteration ([`crate::repos_git::each_existing`]), git
//! invocation ([`crate::repos_git::repo_git`],
//! [`crate::repos_git::repo_git_fetch`]), and mtime-noise
//! normalization ([`crate::repos_dirty::normalize_filtered`]) live in
//! sibling modules; this
//! module owns only the header table ([`header_text`]) and the
//! fetch/push/diff/status one/all wrappers, mirroring
//! `_repo_simple_header`, `_repo_*_one`, and `_repo_*_all`.

use std::ffi::OsString;
use std::io::Write;

use crate::log::Log;
use crate::repos_base::{Base, RepoKind};
use crate::repos_dirty::normalize_filtered;
use crate::repos_git::{each_existing, repo_git, repo_git_fetch};

/// Header bytes `_repo_simple_header` prints for one repo, including the
/// shell `echo` trailing newline, or `None` when the shell prints nothing
/// (an unknown `op`; every known op prints for both kinds).
///
/// The `diff`/`status` overlay row carries its leading blank line: the
/// shell runs a bare `echo ""` before `_header` there.
pub fn header_text(op: &str, kind: RepoKind, name: &str) -> Option<String> {
    match (op, kind) {
        ("fetch", RepoKind::Base) => Some(String::from("==> Fetching dotfiles...\n")),
        ("fetch", RepoKind::Overlay) => Some(format!("==> Fetching {name} dotfiles...\n")),
        ("push", RepoKind::Base) => Some(String::from("==> Pushing dotfiles...\n")),
        ("push", RepoKind::Overlay) => Some(format!("==> Pushing {name} dotfiles...\n")),
        ("diff" | "status", RepoKind::Base) => Some(String::from("==> dotfiles\n")),
        ("diff" | "status", RepoKind::Overlay) => Some(format!("\n==> {name} dotfiles\n")),
        _ => None,
    }
}

/// Emit the one-repo header for `op` through [`Log::header`].
///
/// [`header_text`] carries both shell `echo` newlines, while
/// `Log::header` appends its own: exactly one trailing newline is
/// stripped first, and a leading blank line (the `diff`/`status`
/// overlay `echo ""`, which runs BEFORE `_header`) is emitted
/// separately so it stays outside the header paint, byte-identical
/// to the shell. A `None` header prints nothing, like the shell case
/// table falling through.
fn print_header(log: &Log, out: &mut dyn Write, op: &str, kind: RepoKind, name: &str) {
    if let Some(text) = header_text(op, kind, name) {
        let (lead, body) = match text.strip_prefix('\n') {
            Some(rest) => ("\n", rest),
            None => ("", text.as_str()),
        };
        let _ = out.write_all(lead.as_bytes());
        log.header(out, body.strip_suffix('\n').unwrap_or(body));
    }
}

/// Borrow `each_existing` callback argv as `&str` for the `&[&str]`
/// one-function calls.
///
/// Iteration hands callbacks `OsString` argv (the shell passes raw argv
/// words through); the one-functions take `&str` like sibling
/// `repo_git`. The lossy conversion is exact for every portable argv.
fn argv_to_strs(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// `_repo_fetch_one`: print the fetch header, then fetch through
/// [`repo_git_fetch`], propagating its exit code like the shell (the
/// fetch call is the function's last command).
/// Eight parameters mirror the shell `_repo_fetch_one` operand order
/// (`log`/`out` stand in for redirected stdout); bundling them would
/// obscure the call-site parity the differential tests pin.
#[allow(clippy::too_many_arguments)]
pub fn fetch_one(
    log: &Log,
    out: &mut dyn Write,
    base: &Base,
    kind: RepoKind,
    name: &str,
    path: &str,
    extra: &[&str],
    mask: u32,
) -> i32 {
    print_header(log, out, "fetch", kind, name);
    repo_git_fetch(base, kind, path, extra, mask)
}

/// `_repo_push_one`: print the push header, then `push` through
/// [`repo_git`].
///
/// A failed base push keeps the shell's hard-fail (exactly exit 1); a
/// failed overlay push warns on `err` and returns success so one stale
/// overlay cannot block publishing the base repo.
/// Eight parameters mirror the shell `_repo_push_one` operand order
/// (plus the `err` sink for the overlay warning); see [`fetch_one`].
#[allow(clippy::too_many_arguments)]
pub fn push_one(
    log: &Log,
    out: &mut dyn Write,
    err: &mut dyn Write,
    base: &Base,
    kind: RepoKind,
    name: &str,
    path: &str,
    extra: &[&str],
) -> i32 {
    print_header(log, out, "push", kind, name);
    let mut argv: Vec<&str> = Vec::with_capacity(extra.len() + 1);
    argv.push("push");
    argv.extend(extra.iter().copied());
    let rc = repo_git(base, kind, path, &argv);
    if rc == 0 {
        return 0;
    }
    if kind == RepoKind::Base {
        return 1;
    }
    log.warn(err, &format!("  warning: {name} dotfiles push failed"));
    0
}

/// `_repo_diff_one`: print the diff header, then `diff` through
/// [`repo_git`], propagating git's exit code like the shell.
pub fn diff_one(
    log: &Log,
    out: &mut dyn Write,
    base: &Base,
    kind: RepoKind,
    name: &str,
    path: &str,
    extra: &[&str],
) -> i32 {
    print_header(log, out, "diff", kind, name);
    let mut argv: Vec<&str> = Vec::with_capacity(extra.len() + 1);
    argv.push("diff");
    argv.extend(extra.iter().copied());
    repo_git(base, kind, path, &argv)
}

/// `_repo_status_one`: print the status header, then `status` through
/// [`repo_git`], propagating git's exit code like the shell.
pub fn status_one(
    log: &Log,
    out: &mut dyn Write,
    base: &Base,
    kind: RepoKind,
    name: &str,
    path: &str,
    extra: &[&str],
) -> i32 {
    print_header(log, out, "status", kind, name);
    let mut argv: Vec<&str> = Vec::with_capacity(extra.len() + 1);
    argv.push("status");
    argv.extend(extra.iter().copied());
    repo_git(base, kind, path, &argv)
}

/// `_repo_fetch_all`: fetch every existing repo via [`each_existing`]
/// with a [`fetch_one`] closure, returning the iteration result.
///
/// Parity: the shell `_repo_fetch_all` does NOT normalize first
/// (there is no `_normalize_filtered` call there), so neither does
/// this.
pub fn fetch_all(
    log: &Log,
    out: &mut dyn Write,
    base: &Base,
    overlays: &[String],
    home: &str,
    extra: &[OsString],
    mask: u32,
) -> i32 {
    let mut callback =
        |kind: RepoKind, name: &str, path: &str, _url: &str, args: &[OsString]| -> i32 {
            let owned = argv_to_strs(args);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            fetch_one(log, &mut *out, base, kind, name, path, &refs, mask)
        };
    each_existing(base, overlays, home, extra, &mut callback)
}

/// `_repo_push_all`: normalize mtime noise first (like the shell
/// `_normalize_filtered` call), then push every existing repo via
/// [`each_existing`] with a [`push_one`] closure.
pub fn push_all(
    log: &Log,
    out: &mut dyn Write,
    err: &mut dyn Write,
    base: &Base,
    overlays: &[String],
    home: &str,
    extra: &[OsString],
) -> i32 {
    let prefix = base.git_prefix();
    normalize_filtered(prefix.as_deref(), overlays);
    let mut callback =
        |kind: RepoKind, name: &str, path: &str, _url: &str, args: &[OsString]| -> i32 {
            let owned = argv_to_strs(args);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            push_one(log, &mut *out, &mut *err, base, kind, name, path, &refs)
        };
    each_existing(base, overlays, home, extra, &mut callback)
}

/// `_repo_diff_all`: normalize mtime noise first (like the shell
/// `_normalize_filtered` call), then diff every existing repo via
/// [`each_existing`] with a [`diff_one`] closure.
pub fn diff_all(
    log: &Log,
    out: &mut dyn Write,
    base: &Base,
    overlays: &[String],
    home: &str,
    extra: &[OsString],
) -> i32 {
    let prefix = base.git_prefix();
    normalize_filtered(prefix.as_deref(), overlays);
    let mut callback =
        |kind: RepoKind, name: &str, path: &str, _url: &str, args: &[OsString]| -> i32 {
            let owned = argv_to_strs(args);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            diff_one(log, &mut *out, base, kind, name, path, &refs)
        };
    each_existing(base, overlays, home, extra, &mut callback)
}

/// `_repo_status_all`: normalize mtime noise first (like the shell
/// `_normalize_filtered` call), then report every existing repo via
/// [`each_existing`] with a [`status_one`] closure.
pub fn status_all(
    log: &Log,
    out: &mut dyn Write,
    base: &Base,
    overlays: &[String],
    home: &str,
    extra: &[OsString],
) -> i32 {
    let prefix = base.git_prefix();
    normalize_filtered(prefix.as_deref(), overlays);
    let mut callback =
        |kind: RepoKind, name: &str, path: &str, _url: &str, args: &[OsString]| -> i32 {
            let owned = argv_to_strs(args);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            status_one(log, &mut *out, base, kind, name, path, &refs)
        };
    each_existing(base, overlays, home, extra, &mut callback)
}
