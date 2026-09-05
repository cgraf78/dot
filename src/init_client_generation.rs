//! The init git-generation binding of `lib/dot/init-client.sh`: the
//! marker path inside a staged git directory, marker publication,
//! marker validation, the branch-tip check, git identity capture,
//! and git metadata mode setup.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the six functions from
//! `_dot_init_generation_marker` through
//! `_dot_init_configure_git_metadata_modes`. The file-generic
//! `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state, absorbed into
//! [`Result`] the way earlier slices absorb engine diagnostics).
//! The transaction-directory lifecycle lives in the sibling
//! transaction module, the host-git identity family is in flight on
//! `rust-port-slice-41`, and the record, claim, publish, and
//! rollback families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_NONCE`, `DOT_INIT_COMMIT`, `DOT_INIT_IDENTITY`, and
//! `DOT_INIT_BRANCH` globals. Library code must not mutate the
//! process environment behind the engine, so those cross here as
//! explicit parameters; `_dot_init_set_git_identity` likewise
//! returns its `dev:ino` pair instead of assigning
//! `DOT_INIT_GIT_DEV`/`DOT_INIT_GIT_INO`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::temp;

/// First line of a generation marker: proves the file is ours before
/// any field is trusted. The shell compares this literal on the
/// first `read` iteration; both engines reject any other header.
pub const GENERATION_HEADER: &str = "cgraf78 dot client generation v1";

/// File name of the generation marker inside a git directory.
pub const GENERATION_MARKER_NAME: &str = "dot-init-generation-v1";

/// `_dot_init_generation_marker`: `<git-dir>/dot-init-generation-v1`.
/// Plain byte concatenation like the shell's `printf '%s/...'`, so a
/// `git_dir` with a trailing slash keeps its doubled separator
/// instead of being normalized away.
pub fn generation_marker(git_dir: &Path) -> PathBuf {
    let mut marker = git_dir.as_os_str().to_os_string();
    marker.push("/");
    marker.push(GENERATION_MARKER_NAME);
    PathBuf::from(marker)
}

/// `_dot_init_write_generation_marker`: publish the four-line marker
/// (header plus `nonce`/`commit`/`identity`) at mode 600 without
/// replacing a live marker. The sibling temp carries the bytes and
/// [`temp::move_noreplace_cached`] publishes them, exactly like the
/// shell's `_dot_sibling_tmp_for` plus `_dot_move_noreplace`.
///
/// Like the shell, a failure after the sibling exists (body write,
/// chmod, or a live destination winning the race) leaves the
/// sibling behind: nothing later in the family reads those names,
/// so the shapes stay comparable across engines.
pub fn write_generation_marker(
    git_dir: &Path,
    nonce: &str,
    commit: &str,
    identity: &str,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let marker = generation_marker(git_dir);
    let temporary = temp::sibling_tmp_for(&marker)?;
    let body =
        format!("{GENERATION_HEADER}\nnonce={nonce}\ncommit={commit}\nidentity={identity}\n");
    std::fs::write(&temporary, body.as_bytes()).map_err(|source| Error::Io {
        context: "write generation marker",
        source,
    })?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| Error::Io {
            context: "chmod generation marker",
            source,
        },
    )?;
    temp::move_noreplace_cached(&temporary, &marker, cache)
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`. `symlink_metadata` never follows,
/// so a link reports its own type and fails the gate on both
/// engines.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Effective-uid ownership (`test -O`): the shell gate requires the
/// marker to be ours. An unreadable identity fails closed, like the
/// shell's failed `stat`. (Twin of the transaction module's gate;
/// kept local because that module is a sibling owner, not a shared
/// helper.)
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// `_dot_init_generation_marker_matches`: the marker exists under a
/// real `git_dir`, is a real file owned by us, and holds exactly the
/// header plus one `nonce=`, one `commit=`, and one `identity=` line
/// equal to the expected run identity. Duplicate or unknown keys,
/// lines without `=`, and any line-count other than four all fail,
/// mirroring the shell's `seen` map and `count -eq 4` gate.
///
/// Line splitting follows the shell's `read`: bytes divide on `\n`,
/// a missing trailing newline still yields its final line, and a
/// trailing newline adds no phantom empty line. Carriage returns
/// stay put, so a `\r`-tainted value can only match a `\r`-tainted
/// expectation on both engines.
pub fn generation_marker_matches(
    git_dir: &Path,
    nonce: &str,
    commit: &str,
    identity: &str,
) -> bool {
    if !is_real_dir(git_dir) {
        return false;
    }
    let marker = generation_marker(git_dir);
    if !is_real_file(&marker) || !owned_by_us(&marker) {
        return false;
    }
    let bytes = match std::fs::read(&marker) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() || lines[0] != GENERATION_HEADER.as_bytes() {
        return false;
    }
    let mut seen_nonce = false;
    let mut seen_commit = false;
    let mut seen_identity = false;
    for line in &lines[1..] {
        let Some(equal) = line.iter().position(|byte| *byte == b'=') else {
            return false;
        };
        let (key, value) = (&line[..equal], &line[equal + 1..]);
        match key {
            b"nonce" if !seen_nonce => {
                seen_nonce = true;
                if value != nonce.as_bytes() {
                    return false;
                }
            }
            b"commit" if !seen_commit => {
                seen_commit = true;
                if value != commit.as_bytes() {
                    return false;
                }
            }
            b"identity" if !seen_identity => {
                seen_identity = true;
                if value != identity.as_bytes() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    lines.len() == 4 && seen_nonce && seen_commit && seen_identity
}

/// `_dot_init_generation_matches`: the marker matches AND
/// `refs/heads/<branch>` inside `git_dir` resolves to `commit`.
/// The rev-parse stdout comparison strips trailing newlines exactly
/// like the shell's `$(...)`; a failed git run (missing ref,
/// missing store) fails the match rather than erroring, also like
/// the shell's `[[ ... ]]` on the empty substitution.
pub fn generation_matches(
    git_dir: &Path,
    branch: &str,
    nonce: &str,
    commit: &str,
    identity: &str,
) -> bool {
    if !generation_marker_matches(git_dir, nonce, commit, identity) {
        return false;
    }
    let output = match Command::new("git")
        .arg(OsString::from(format!("--git-dir={}", git_dir.display())))
        .arg("rev-parse")
        .arg(format!("refs/heads/{branch}"))
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let mut actual = output.stdout.as_slice();
    while actual.last() == Some(&b'\n') {
        actual = &actual[..actual.len() - 1];
    }
    actual == commit.as_bytes()
}

/// `_dot_init_set_git_identity`: report the git directory's
/// `dev:ino` pair (`stat -c '%d:%i'`, following symlinks like the
/// shell's `stat`). The shell assigns the pair to `DOT_INIT_GIT_DEV`
/// and `DOT_INIT_GIT_INO`; the port returns it for the caller to
/// bind.
pub fn set_git_identity(git_dir: &Path) -> Result<(u64, u64)> {
    temp::path_identity(git_dir)
}

/// `_dot_init_configure_git_metadata_modes`: pin
/// `core.sharedRepository` to `0700`, then clamp the whole metadata
/// tree to the live-umask ceiling. The mask is read here (not
/// passed in) because the shell's `_dot_apply_umask_ceiling` reads
/// the live `umask` at each entry; the umask cannot change mid-run,
/// so one read is equivalent.
pub fn configure_git_metadata_modes(git_dir: &Path) -> Result<()> {
    let status = Command::new("git")
        .arg(OsString::from(format!("--git-dir={}", git_dir.display())))
        .args(["config", "core.sharedRepository", "0700"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| Error::Io {
            context: "run git config sharedRepository",
            source,
        })?;
    if !status.success() {
        return Err(Error::Command {
            command: "git config core.sharedRepository".to_string(),
            status: Some(status.to_string()),
        });
    }
    let mask = temp::read_umask()?;
    temp::apply_git_metadata_modes(git_dir, mask)
}
