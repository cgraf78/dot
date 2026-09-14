//! The init deletion-parking family of `lib/dot/init-client.sh`: the
//! same-parent park path, the worktree-content match gate, the three
//! per-kind delete validators, the two private-directory gates, and
//! the parked-generation remover.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the eight functions from
//! `_dot_init_delete_park_path` through
//! `_dot_init_delete_parked_generation`, plus the two small match
//! gates they call that no lane has claimed: the worktree-content
//! gate `_dot_init_candidate_matches_git` (used by the leaf
//! validator) and the private-directory pair
//! `_dot_init_private_directory_matches` /
//! `_dot_init_private_empty_directory_matches` (used by the parent
//! validator). The file-generic `_dot_init_error` diagnostic stays
//! unported (a bare `printf ... >&2; return 1` with no family state,
//! absorbed into [`Result`] the way earlier slices absorb engine
//! diagnostics). The transaction lifecycle, host-git identity,
//! git-generation binding, per-entry staging, candidate planning,
//! and record journal families live on sibling
//! `rust-port-slice-{35,41,43,46,48,51,54}`; the rollback, publish,
//! status, and command-dispatch families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_NONCE`, `DOT_INIT_COMMIT`, `DOT_INIT_IDENTITY`, and
//! `DOT_INIT_BRANCH` globals and the worktree root from `HOME`.
//! Library code must not mutate the process environment behind the
//! engine, so those cross here as explicit parameters. The shell
//! passes its verifier by function name (`"$verifier" "$park" "$@"`)
//! with trailing match arguments; here the verifier crosses as a
//! `&dyn Fn(&Path) -> bool` closure with those arguments already
//! bound, and the remover name (`leaf`/`parent`/`tree`) crosses as a
//! plain string. `REPLY` outputs surface as return values.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::temp;

/// File name of the staged-generation marker inside a git directory
/// (twin of the sibling generation lane's constant, kept local
/// because that lane is unmerged).
const GENERATION_MARKER_NAME: &str = "dot-init-generation-v1";

/// First line of a generation marker: proves the file is ours before
/// any field is trusted (twin of the sibling generation lane's
/// constant, kept local because that lane is unmerged).
const GENERATION_HEADER: &str = "cgraf78 dot client generation v1";

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A symlink of any kind (dangling included): the shell's
/// `[[ -L $path ]]`.
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink())
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Effective-uid ownership (`test -O`): the shell gate requires the
/// path to be ours. An unreadable identity fails closed, like the
/// shell's failed `stat`. (Twin of the sibling generation lane's
/// gate; kept local because that lane is unmerged.)
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// `stat -c '%d:%i'` rendered as text, or empty when the stat fails:
/// the shell's `$(_dot_path_identity "$path" 2>/dev/null || true)`.
/// `stat` follows symlinks on both engines.
fn live_identity_string(path: &Path) -> String {
    temp::path_identity(path)
        .map(temp::identity_string)
        .unwrap_or_default()
}

/// Raw bytes of a path, so `$HOME/` prefix checks and `$HOME/$path`
/// joins behave like shell string operations even when `home` has a
/// trailing slash (the doubled separator is preserved, never
/// normalized away).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// `$HOME/$path` by byte concatenation, like the shell's
/// `target=$HOME/$5`.
fn join_home(home: &Path, path: &str) -> PathBuf {
    let mut joined = path_bytes(home).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(path.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// The shell's `${candidate#"$HOME"/}` guarded by
/// `[[ $candidate == "$HOME/"* ]]`: byte-prefix match only, so a
/// `home` with a trailing slash demands its doubled separator.
fn strip_home_prefix<'a>(home: &Path, candidate: &'a Path) -> Option<&'a str> {
    let home = path_bytes(home);
    let candidate = path_bytes(candidate);
    if candidate.len() <= home.len()
        || candidate[home.len()] != b'/'
        || candidate[..home.len()] != *home
    {
        return None;
    }
    std::str::from_utf8(&candidate[home.len() + 1..]).ok()
}

/// Strip every trailing newline, exactly like command substitution:
/// git prints its object ids followed by one newline, and the shell
/// compares the chomped text.
fn chomp_newlines(bytes: &[u8]) -> &[u8] {
    let mut text = bytes;
    while text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    text
}

/// Run `git --git-dir=<git_dir> <args>`, pinning `LC_ALL=C` like
/// every other lane. The directory crosses as one `--git-dir=`
/// argument built from raw bytes (never a lossy `display`), and
/// `stdin_bytes` feeds the child's stdin when present. `None` when
/// git cannot start, fails, or (for callers that need stdout) emits
/// nothing usable — the shell's `|| return 1` / empty-substitution
/// failure modes.
fn run_git_dir(git_dir: &Path, args: &[&str], stdin_bytes: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut dir_arg = OsString::from("--git-dir=");
    dir_arg.push(git_dir);
    let mut child = Command::new("git")
        .arg(dir_arg)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(payload) = stdin_bytes {
        child.stdin.as_mut()?.write_all(payload).ok()?;
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// `git hash-object --stdin` over raw bytes (content hashing is
/// store-independent, so no `--git-dir`): the shell's
/// `printf ... | git hash-object --stdin`.
fn hash_stdin_bytes(payload: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| Error::Io {
            context: "spawn git hash-object",
            source,
        })?;
    child
        .stdin
        .as_mut()
        .ok_or(Error::Usage {
            message: "git hash-object has no stdin",
        })?
        .write_all(payload)
        .map_err(|source| Error::Io {
            context: "feed git hash-object",
            source,
        })?;
    let output = child.wait_with_output().map_err(|source| Error::Io {
        context: "reap git hash-object",
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "git hash-object --stdin".to_string(),
            status: Some(output.status.to_string()),
        });
    }
    Ok(String::from_utf8_lossy(chomp_newlines(&output.stdout)).into_owned())
}

/// `git --git-dir=<git_dir> hash-object --stdin` over raw bytes,
/// chomped like the shell's `actual_oid=$(...)`.
fn hash_stdin_git_dir(git_dir: &Path, payload: &[u8]) -> Option<String> {
    run_git_dir(git_dir, &["hash-object", "--stdin"], Some(payload))
        .map(|stdout| String::from_utf8_lossy(chomp_newlines(&stdout)).into_owned())
}

/// `_dot_init_delete_park_path`: the same-parent park name for one
/// doomed path: `<parent>/.dot-init-delete.<nonce>.<kind>.<hash>`
/// where `hash` is `git hash-object --stdin` over `"<kind>\t<key>"`
/// (no trailing newline, exactly like the shell's
/// `printf '%s\t%s'`). Only `leaf`, `parent`, and `git` kinds park;
/// anything else is a usage error, also like the shell's `case`
/// gate.
///
/// The parent is the shell's `${target%/*}` on bytes: the text
/// before the last slash. A target with no slash, an empty parent,
/// or a target that already equals its parent (a bare `/`-less
/// name) fails, so the park always stays beside its target and a
/// rename can never cross filesystems. `nonce` is `DOT_INIT_NONCE`.
pub fn delete_park_path(target: &Path, kind: &str, key: &str, nonce: &str) -> Result<PathBuf> {
    match kind {
        "leaf" | "parent" | "git" => {}
        _ => {
            return Err(Error::Usage {
                message: "delete park kind must be leaf, parent, or git",
            });
        }
    }
    let mut payload = Vec::with_capacity(kind.len() + key.len() + 1);
    payload.extend_from_slice(kind.as_bytes());
    payload.push(b'\t');
    payload.extend_from_slice(key.as_bytes());
    let hash = hash_stdin_bytes(&payload)?;
    let target = path_bytes(target);
    let slash = target.iter().rposition(|byte| *byte == b'/');
    let parent = match slash {
        Some(position) => &target[..position],
        None => target,
    };
    if parent.is_empty() || parent == target {
        return Err(Error::Usage {
            message: "delete target has no parent directory",
        });
    }
    let mut park = parent.to_vec();
    park.push(b'/');
    park.extend_from_slice(b".dot-init-delete.");
    park.extend_from_slice(nonce.as_bytes());
    park.push(b'.');
    park.extend_from_slice(kind.as_bytes());
    park.push(b'.');
    park.extend_from_slice(hash.as_bytes());
    Ok(PathBuf::from(OsString::from_vec(park)))
}

/// `_dot_init_candidate_matches_git`: the `$HOME/$path` worktree
/// entry carries exactly the tracked `mode`/`oid` generation. The
/// `commit` revision selects nothing here — the shell binds `$2` and
/// never reads it — but the arity stays so callers map positionally.
///
/// Symlinks hash their `readlink` bytes *plus the trailing newline*:
/// the shell's `link_target=$(readlink "$target"; printf .)` trick
/// preserves `readlink`'s own newline (`${link_target%.}` only drops
/// the sentinel dot), so `git hash-object --stdin` sees
/// `"<target>\n"`. A live symlink therefore hashes differently from
/// its tree blob (which stores the bare target) and never matches —
/// that is the shell's observed behavior, reproduced byte for byte.
/// Regular files hash through `git hash-object --no-filters` with an
/// owner-execute check (`100755` needs any execute bit,
/// `100644` needs none); any other mode fails outright.
pub fn candidate_matches_git(
    git_dir: &Path,
    _commit: &str,
    mode: &str,
    oid: &str,
    path: &str,
    home: &Path,
) -> bool {
    let target = join_home(home, path);
    match mode {
        "120000" => {
            if !is_symlink(&target) {
                return false;
            }
            let link = match std::fs::read_link(&target) {
                Ok(link) => link,
                Err(_) => return false,
            };
            let mut payload = link.as_os_str().as_bytes().to_vec();
            payload.push(b'\n');
            match hash_stdin_git_dir(git_dir, &payload) {
                Some(actual) => actual == oid,
                None => false,
            }
        }
        "100644" | "100755" => {
            if !is_real_file(&target) {
                return false;
            }
            match hash_live_file(git_dir, &target) {
                Some(actual) => {
                    if actual != oid {
                        return false;
                    }
                }
                None => return false,
            }
            let raw = match temp::file_mode(&target) {
                Ok(mode) => mode,
                Err(_) => return false,
            };
            if mode == "100755" {
                raw & 0o111 != 0
            } else {
                raw & 0o111 == 0
            }
        }
        _ => false,
    }
}

/// `git --git-dir=<git_dir> hash-object --no-filters -- <target>`,
/// chomped like the shell's `actual_oid=$(...)`.
fn hash_live_file(git_dir: &Path, target: &Path) -> Option<String> {
    let target = target.as_os_str().to_os_string();
    let mut dir_arg = OsString::from("--git-dir=");
    dir_arg.push(git_dir);
    let output = Command::new("git")
        .arg(dir_arg)
        .args(["hash-object", "--no-filters", "--"])
        .arg(target)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(chomp_newlines(&output.stdout)).into_owned())
}

/// `_dot_init_private_directory_matches`: a real directory owned by
/// us whose permission bits grant nothing to group or other
/// (`mode & 077 == 0`). The optional identity (`dev:ino` text) and
/// mode (bare octal text like `stat` prints, so `"0700"` never equals
/// `"700"`) narrow the match when present, exactly like the shell's
/// `${2:-}` / `${3:-}` defaults.
pub fn private_directory_matches(
    path: &Path,
    expected_identity: Option<&str>,
    expected_mode: Option<&str>,
) -> bool {
    if !is_real_dir(path) || !owned_by_us(path) {
        return false;
    }
    let raw = match temp::file_mode(path) {
        Ok(mode) => mode,
        Err(_) => return false,
    };
    let text = format!("{raw:o}");
    if !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return false;
    }
    if raw & 0o77 != 0 {
        return false;
    }
    if expected_mode.is_some_and(|mode| mode != text) {
        return false;
    }
    if expected_identity.is_some_and(|identity| live_identity_string(path) != identity) {
        return false;
    }
    true
}

/// `_dot_init_private_empty_directory_matches`: the private-directory
/// gate above, plus no entries at all. The shell globs with
/// `nullglob` and `dotglob` (dotfiles count, `.`/`..` never do), so
/// an unreadable directory also reports empty — a failed read here
/// matches that, and per-entry read errors simply skip, the way the
/// glob skips names it cannot see.
pub fn private_empty_directory_matches(
    path: &Path,
    expected_identity: Option<&str>,
    expected_mode: Option<&str>,
) -> bool {
    if !private_directory_matches(path, expected_identity, expected_mode) {
        return false;
    }
    match std::fs::read_dir(path) {
        Ok(entries) => entries.filter_map(|entry| entry.ok()).count() == 0,
        Err(_) => true,
    }
}

/// `_dot_init_leaf_delete_matches`: the parked candidate still has
/// the recorded identity, still lives under `home`, and still
/// carries the tracked `mode`/`oid` generation. The identity
/// compares as text with a failed stat counting as empty, exactly
/// like the shell's `$(... || true)`.
pub fn leaf_delete_matches(
    candidate: &Path,
    expected_identity: &str,
    git_dir: &Path,
    commit: &str,
    mode: &str,
    oid: &str,
    home: &Path,
) -> bool {
    if live_identity_string(candidate) != expected_identity {
        return false;
    }
    let relative = match strip_home_prefix(home, candidate) {
        Some(relative) => relative,
        None => return false,
    };
    candidate_matches_git(git_dir, commit, mode, oid, relative, home)
}

/// `_dot_init_parent_delete_matches`: the parked candidate is still
/// the unchanged empty mode-locked directory the intent recorded —
/// nothing more than the empty-directory gate above.
pub fn parent_delete_matches(
    candidate: &Path,
    expected_identity: &str,
    expected_mode: &str,
) -> bool {
    private_empty_directory_matches(candidate, Some(expected_identity), Some(expected_mode))
}

/// `_dot_init_git_delete_matches`: the parked candidate still has
/// the recorded git-directory identity and still carries this run's
/// generation marker and branch tip. The generation check twins the
/// sibling generation lane (unmerged): marker header plus one
/// `nonce=`/`commit=`/`identity=` line each, then
/// `refs/heads/<branch>` resolving to `commit`. Like the shell, the
/// generation gate runs against the candidate itself (the parked
/// generation), so there is no separate store parameter.
///
/// `nonce`, `commit`, `identity`, and `branch` are `DOT_INIT_NONCE`,
/// `DOT_INIT_COMMIT`, `DOT_INIT_IDENTITY`, and `DOT_INIT_BRANCH`.
pub fn git_delete_matches(
    candidate: &Path,
    expected_identity: &str,
    nonce: &str,
    commit: &str,
    identity: &str,
    branch: &str,
) -> bool {
    if live_identity_string(candidate) != expected_identity {
        return false;
    }
    generation_matches(candidate, nonce, commit, identity, branch)
}

/// `<git_dir>/dot-init-generation-v1` by byte concatenation, like the
/// shell's `printf '%s/...'` (twin of the sibling generation lane's
/// helper, kept local because that lane is unmerged).
fn generation_marker(git_dir: &Path) -> PathBuf {
    let mut marker = git_dir.as_os_str().to_os_string();
    marker.push("/");
    marker.push(GENERATION_MARKER_NAME);
    PathBuf::from(marker)
}

/// Twin of the sibling generation lane's marker validator (that lane
/// is unmerged, so the check lives here too): the marker sits under
/// a real `git_dir`, is a real file owned by us, and holds exactly
/// the header plus one `nonce=`, one `commit=`, and one `identity=`
/// line equal to this run. Duplicate or unknown keys, lines without
/// `=`, and any line-count other than four all fail.
///
/// Line splitting follows the shell's `read`: bytes divide on `\n`,
/// a missing trailing newline still yields its final line, a
/// trailing newline adds no phantom empty line, and carriage returns
/// stay put.
fn generation_marker_matches(git_dir: &Path, nonce: &str, commit: &str, identity: &str) -> bool {
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

/// Twin of the sibling generation lane's branch-tip check (that lane
/// is unmerged): the marker matches AND `refs/heads/<branch>`
/// resolves to `commit`. The rev-parse comparison chomps trailing
/// newlines exactly like the shell's `$(...)`; a failed git run
/// fails the match, also like the shell.
fn generation_matches(
    git_dir: &Path,
    nonce: &str,
    commit: &str,
    identity: &str,
    branch: &str,
) -> bool {
    if !generation_marker_matches(git_dir, nonce, commit, identity) {
        return false;
    }
    let reference = format!("refs/heads/{branch}");
    let stdout = match run_git_dir(git_dir, &["rev-parse", &reference], None) {
        Some(stdout) => stdout,
        None => return false,
    };
    chomp_newlines(&stdout) == commit.as_bytes()
}

/// `_dot_init_delete_parked_generation`: remove exactly one validated
/// generation. Deleting by pathname is unsafe after validation —
/// another process can replace that pathname before `rm` runs — so
/// the caller first selects one generation with an exclusive
/// same-parent rename into `park`, the `verifier` validates the
/// parked inode and contents (twice: before and right before
/// removal, so a changed parked generation is preserved rather than
/// deleted merely for occupying the park name), and only that parked
/// generation goes to the `remover`: `leaf` (`rm -f`), `parent`
/// (`rmdir`), or `tree` (`rm -rf`); anything else fails.
///
/// When `park` starts vacant and the target exists, the move happens
/// here; when both start vacant there is nothing to do (success,
/// like the shell's early `return 0`). When validation fails after
/// this call parked the target, the original is moved back — but
/// only while the destination stayed vacant and only when the
/// restored identity still matches, exactly like the shell. A
/// `target` that reappears between validation and removal
/// (`target_won`) fails the call even after a clean removal, also
/// like the shell's trailing gate.
///
/// `cache` backs both `_dot_move_noreplace` steps.
pub fn delete_parked_generation(
    target: &Path,
    park: &Path,
    remover: &str,
    verifier: &dyn Fn(&Path) -> bool,
    cache: &mut temp::MoveCache,
) -> bool {
    let mut parked_now = false;
    if !exists_lexical(park) {
        if !exists_lexical(target) {
            return true;
        }
        if temp::move_noreplace_cached(target, park, cache).is_err() {
            return false;
        }
        parked_now = true;
    }
    if !verifier(park) {
        if parked_now && !exists_lexical(target) {
            let parked_identity = match temp::path_identity(park) {
                Ok(identity) => temp::identity_string(identity),
                Err(_) => return false,
            };
            if temp::move_noreplace_cached(park, target, cache).is_err() {
                return false;
            }
            if live_identity_string(target) != parked_identity {
                return false;
            }
        }
        return false;
    }
    let target_won = exists_lexical(target);
    // Revalidate immediately before removal so a changed parked
    // generation is preserved rather than deleted merely because it
    // occupies the park name.
    if !verifier(park) {
        return false;
    }
    match remover {
        "leaf" => {
            if let Err(source) = std::fs::remove_file(park) {
                if source.kind() != std::io::ErrorKind::NotFound {
                    return false;
                }
            }
        }
        "parent" => {
            if std::fs::remove_dir(park).is_err() {
                return false;
            }
        }
        "tree" => {
            if let Err(source) = std::fs::remove_dir_all(park) {
                if source.kind() != std::io::ErrorKind::NotFound {
                    return false;
                }
            }
        }
        _ => return false,
    }
    !exists_lexical(park) && !target_won
}
