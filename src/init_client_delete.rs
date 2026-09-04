//! Park-and-verify safe deletion for `lib/dot/init-client.sh`.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the seven deletion primitives from
//! `_dot_init_candidate_matches_git` through
//! `_dot_init_delete_parked_generation`: the candidate/git matcher,
//! the delete-park path, the leaf and parent delete matchers, the
//! private-directory matchers, and the parked-generation remover. The
//! file-generic `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state, absorbed into
//! [`Result`] the way earlier slices absorb engine diagnostics). The
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family on
//! `rust-port-slice-41` (`init_client_identity`), the git-generation
//! binding on `rust-port-slice-43` (`init_client_generation`), the
//! per-entry staging family on `rust-port-slice-46`
//! (`init_client_entry`), the candidate/planning family on
//! `rust-port-slice-48` (`init_client_candidate`), and the record
//! journal on `rust-port-slice-51`/`rust-port-slice-54`
//! (`init_client_records`/`init_client_record`). The git-staging,
//! publish, rollback, confirm/plan, and resume families stay for
//! later slices; `_dot_init_git_delete_matches` in particular stays
//! out because it re-checks the generation marker owned by the
//! slice-43 lane.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the client root from `HOME`, the
//! run identity from `DOT_INIT_NONCE`, and the checkout from
//! `DOT_SOURCE_ROOT`. Library code must not read that ambient state
//! behind the engine, so `home` and `nonce` cross as explicit
//! parameters wherever the shell reads them, `source_root` crosses
//! where the park hash needs a checkout binding, the `REPLY`-carried
//! park path is returned, and the verifier callback
//! (`_dot_init_delete_parked_generation` takes a shell function name
//! plus arguments) crosses as a closure the caller binds over its
//! own arguments.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::temp;

/// Kinds `_dot_init_delete_park_path` accepts: a published leaf, a
/// published parent directory, or the staged git directory. Kept as
/// the shell's three literals (not an enum) so the park-name segment
/// renders without a mapping table.
const PARK_KINDS: &[&str] = &["leaf", "parent", "git"];

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`. `symlink_metadata` never follows,
/// so a link reports its own type and fails the gate on both
/// engines.
fn is_real_dir(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.is_dir() && !meta.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Effective-uid ownership (`test -O`): the shell gate requires the
/// path to be ours. An unreadable identity fails closed, like the
/// shell's failed `stat`. (Twin of the generation module's gate;
/// kept local because that module is a sibling owner, not a shared
/// helper.)
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// Present in any form: the shell's `[[ -e $path || -L $path ]]`
/// (a dangling symlink counts as present). `symlink_metadata`
/// succeeds for exactly those shapes.
fn any_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `_dot_path_identity` as a string, following symlinks like the
/// shell's `stat` (no `-P` anywhere in this domain). A missing path
/// renders empty, mirroring the shell's `$(... || true)` capture.
fn live_identity(path: &Path) -> String {
    temp::path_identity(path)
        .map(temp::identity_string)
        .unwrap_or_default()
}

/// `_dot_init_delete_park_path`: the same-parent park name
/// `<parent>/.dot-init-delete.<nonce>.<kind>.<hash>`, where `hash`
/// is `git hash-object --stdin` over `kind\key` (no trailing
/// newline, exactly like the shell's `printf '%s\t%s'`). The
/// parent is the bytes before the last slash (`${target%/*}`), so a
/// target with no slash — or nothing before its only slash — fails
/// instead of parking at the filesystem root.
pub fn delete_park_path(
    target: &Path,
    kind: &str,
    key: &str,
    nonce: &str,
    source_root: &Path,
) -> Result<PathBuf> {
    if !PARK_KINDS.contains(&kind) {
        return Err(Error::Usage {
            message: "unknown delete park kind",
        });
    }
    let bytes = target.as_os_str().as_bytes();
    let Some(slash) = bytes.iter().rposition(|byte| *byte == b'/') else {
        return Err(Error::Usage {
            message: "delete target has no parent",
        });
    };
    let parent = &bytes[..slash];
    if parent.is_empty() {
        return Err(Error::Usage {
            message: "delete target has no parent",
        });
    }
    let mut keyed = Vec::with_capacity(kind.len() + 1 + key.len());
    keyed.extend_from_slice(kind.as_bytes());
    keyed.push(b'\t');
    keyed.extend_from_slice(key.as_bytes());
    let hash = temp::file_text_digest(source_root, &keyed)?;
    let mut park = parent.to_vec();
    park.extend_from_slice(b"/.dot-init-delete.");
    park.extend_from_slice(nonce.as_bytes());
    park.push(b'.');
    park.extend_from_slice(kind.as_bytes());
    park.push(b'.');
    park.extend_from_slice(hash.as_bytes());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&park)))
}

/// Run `git --git-dir=<git_dir> <args>` with the shell's spelling:
/// the flag is one `--git-dir=` argument (byte-built, so non-UTF8
/// stores survive), `LC_ALL=C` pins diagnostics English, and output
/// trailing newlines strip exactly like a `$(...)` capture. The flag
/// pins the object store — a sha256 directory hashes sha256 — while
/// content hashing itself never needs a worktree, so the verdicts
/// follow the shell's even for stores this host never checks out.
/// `target` appends one path argument when present. `None` when git
/// fails to run or reports failure, which the matchers read as a
/// mismatch like the shell's `|| return 1`.
fn run_git_dir(git_dir: &Path, args: &[&str], target: Option<&Path>) -> Option<String> {
    let mut flag = std::ffi::OsString::from("--git-dir=");
    flag.push(git_dir.as_os_str());
    let mut cmd = Command::new("git");
    cmd.arg(flag);
    cmd.args(args);
    if let Some(file) = target {
        cmd.arg(file.as_os_str());
    }
    let output = cmd
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
    )
}

/// Hash in-memory bytes through
/// `git --git-dir=<git_dir> hash-object --stdin`, exactly like the
/// shell's `printf ... | git --git-dir="$git_dir" hash-object
/// --stdin` (no `--no-filters`: blob filters apply, like the
/// shell).
fn hash_stdin_git(git_dir: &Path, payload: &[u8]) -> Option<String> {
    use std::io::Write as _;
    let mut flag = std::ffi::OsString::from("--git-dir=");
    flag.push(git_dir.as_os_str());
    let mut child = Command::new("git")
        .arg(flag)
        .args(["hash-object", "--stdin"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .as_mut()?
        .write_all(payload)
        .map_err(|_| ())
        .ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
    )
}

/// `_dot_init_candidate_matches_git`: the live `$HOME/$path` still
/// carries the candidate's bytes and shape. Regular files compare
/// content through `git hash-object --no-filters` plus the execute-bit
/// class of the mode; anything else — including a missing target —
/// fails.
///
/// Symlinks hash the raw link-target bytes plus the one trailing
/// newline `readlink` prints: the shell captures
/// `$(readlink ...; printf .)` and strips only the sentinel dot, so
/// readlink's own terminating newline survives into the hashed
/// bytes. Real blob oids therefore never match on this arm (both
/// engines agree); only the oid of `target + "\n"` passes. The port
/// appends the same byte rather than "fixing" the shell, so a future
/// shell fix surfaces as a differential failure instead of silent
/// drift.
///
/// `commit` is accepted and ignored exactly like the shell, which
/// takes it positionally but never reads it; `home` is the `HOME`
/// binding.
pub fn candidate_matches_git(
    git_dir: &Path,
    commit: &str,
    mode: &str,
    oid: &str,
    home: &Path,
    path: &Path,
) -> bool {
    let _ = commit;
    let mut raw = home.as_os_str().as_bytes().to_vec();
    raw.push(b'/');
    raw.extend_from_slice(path.as_os_str().as_bytes());
    let target = PathBuf::from(std::ffi::OsStr::from_bytes(&raw));
    if mode == "120000" {
        let link = match std::fs::read_link(&target) {
            Ok(link) => link,
            Err(_) => return false,
        };
        let mut bytes = link.as_os_str().as_bytes().to_vec();
        bytes.push(b'\n');
        return hash_stdin_git(git_dir, &bytes).is_some_and(|actual| actual == oid);
    }
    if mode != "100644" && mode != "100755" {
        return false;
    }
    let meta = match std::fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    let actual_oid = match run_git_dir(
        git_dir,
        &["hash-object", "--no-filters", "--"],
        Some(&target),
    ) {
        Some(actual_oid) => actual_oid,
        None => return false,
    };
    if actual_oid != oid {
        return false;
    }
    let executable = meta.mode() & 0o7777 & 0o111 != 0;
    executable == (mode == "100755")
}

/// `_dot_init_leaf_delete_matches`: the parked candidate still has
/// the prepared identity, lives under `home`, and still matches the
/// candidate in git. The `HOME/` prefix is a byte prefix like the
/// shell's `[[ $candidate == "$HOME/"* ]]`, so a `home` with a
/// trailing slash keeps its doubled separator on both engines.
pub fn leaf_delete_matches(
    candidate: &Path,
    expected_identity: &str,
    home: &Path,
    git_dir: &Path,
    commit: &str,
    mode: &str,
    oid: &str,
) -> bool {
    if live_identity(candidate) != expected_identity {
        return false;
    }
    let mut prefix = home.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    let Some(relative) = candidate
        .as_os_str()
        .as_bytes()
        .strip_prefix(prefix.as_slice())
    else {
        return false;
    };
    let relative = PathBuf::from(std::ffi::OsStr::from_bytes(relative));
    candidate_matches_git(git_dir, commit, mode, oid, home, &relative)
}

/// Render permission bits the way the shell's `stat` probe does.
/// GNU `stat -c '%a'` prints the full 12-bit mode, but BSD
/// `stat -f '%Lp'` — the fallback the shell forks on macOS —
/// prints only the low nine permission bits (`L` selects user,
/// group, and other, dropping suid/sgid/sticky). A `4700` staging
/// dir therefore renders `4700` on Linux but `700` on macOS;
/// match the platform tool, not the raw bits.
#[cfg(target_os = "macos")]
fn render_mode(bits: u32) -> String {
    format!("{:o}", bits & 0o777)
}

/// GNU rendering for [`render_mode`]: the full 12-bit mode, like
/// `stat -c '%a'`.
#[cfg(not(target_os = "macos"))]
fn render_mode(bits: u32) -> String {
    format!("{:o}", bits)
}

/// `_dot_init_private_directory_matches`: a real directory owned by
/// us whose group/other permission bits are clear
/// (`8#$mode & 077 == 0`, so setuid-only extras like `4700` pass
/// the mask on both engines), with optional exact identity and
/// mode-string checks. The mode renders through [`render_mode`],
/// so the `expected_mode` comparison is a plain string equality
/// like the shell's quoted `==` — including a `4700` dir failing
/// a `4700` expectation on macOS, exactly like the shell.
pub fn private_directory_matches(
    path: &Path,
    expected_identity: Option<&str>,
    expected_mode: Option<&str>,
) -> bool {
    if !is_real_dir(path) || !owned_by_us(path) {
        return false;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let bits = meta.mode() & 0o7777;
    if bits & 0o077 != 0 {
        return false;
    }
    if expected_mode.is_some_and(|wanted| render_mode(bits) != wanted) {
        return false;
    }
    match expected_identity {
        None => true,
        Some(wanted) => live_identity(path) == wanted,
    }
}

/// `_dot_init_private_empty_directory_matches`: the private-directory
/// gate plus zero entries. Directory listing follows the shell's
/// `nullglob`/`dotglob` glob: every name counts (dotfiles
/// included), and an unreadable directory expands to nothing — so a
/// `read_dir` failure reads as empty, exactly like the shell.
pub fn private_empty_directory_matches(
    path: &Path,
    expected_identity: Option<&str>,
    expected_mode: Option<&str>,
) -> bool {
    if !private_directory_matches(path, expected_identity, expected_mode) {
        return false;
    }
    match std::fs::read_dir(path) {
        Ok(entries) => entries.filter_map(|entry| entry.ok()).next().is_none(),
        Err(_) => true,
    }
}

/// `_dot_init_parent_delete_matches`: the parked candidate is an
/// empty private directory with the prepared identity and mode.
/// Both filters are required positionally (the rollback caller
/// always passes concrete values), so they cross as plain strings.
pub fn parent_delete_matches(
    candidate: &Path,
    expected_identity: &str,
    expected_mode: &str,
) -> bool {
    private_empty_directory_matches(candidate, Some(expected_identity), Some(expected_mode))
}

/// `_dot_init_delete_parked_generation`: remove `target` only after
/// parking it at `park` with an exclusive same-parent rename and
/// validating the parked inode. When the park already exists (a
/// resumed run), the move is skipped and the pre-existing park is
/// validated in place. A failed verification restores a generation
/// parked by this invocation — but only while the original
/// destination is still vacant — and still reports failure. The
/// parked generation is revalidated immediately before removal, and
/// a target that reappeared meanwhile (`target_won`) fails the
/// removal even after the bytes check out.
///
/// `verifier` is the shell's function-name-plus-arguments callback
/// as a closure over the parked path; `remover` is the shell's
/// `leaf`/`parent`/`tree` word (`rm -f` / `rmdir` / `rm -rf`,
/// including `rm`'s tolerance of an already-absent park). Moves go
/// through [`temp::move_noreplace_cached`], so a late winner fails
/// closed exactly like the shell's `mv -n`.
pub fn delete_parked_generation(
    target: &Path,
    park: &Path,
    remover: &str,
    verifier: &dyn Fn(&Path) -> bool,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    let mut parked_now = false;
    if !any_exists(park) {
        if !any_exists(target) {
            return Ok(());
        }
        temp::move_noreplace_cached(target, park, cache)?;
        parked_now = true;
    }
    if !verifier(park) {
        if parked_now && !any_exists(target) {
            let parked_identity =
                temp::identity_string(temp::path_identity(park).map_err(|_| Error::Usage {
                    message: "cannot identify parked generation",
                })?);
            temp::move_noreplace_cached(park, target, cache)?;
            if live_identity(target) != parked_identity {
                return Err(Error::Usage {
                    message: "parked generation changed during restore",
                });
            }
        }
        return Err(Error::Usage {
            message: "parked generation failed verification",
        });
    }
    let target_won = any_exists(target);
    if !verifier(park) {
        return Err(Error::Usage {
            message: "parked generation changed before removal",
        });
    }
    match remover {
        "leaf" => match std::fs::remove_file(park) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    context: "remove parked leaf",
                    source,
                });
            }
        },
        "parent" => std::fs::remove_dir(park).map_err(|source| Error::Io {
            context: "remove parked parent",
            source,
        })?,
        "tree" => match std::fs::remove_dir_all(park) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    context: "remove parked tree",
                    source,
                });
            }
        },
        _ => {
            return Err(Error::Usage {
                message: "unknown parked generation remover",
            });
        }
    }
    if !any_exists(park) && !target_won {
        Ok(())
    } else {
        Err(Error::Usage {
            message: "parked generation removal raced",
        })
    }
}
