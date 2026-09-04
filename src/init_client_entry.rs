//! Per-entry publication staging for `lib/dot/init-client.sh`:
//! intent records, entry stage paths, and stage claim markers.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the eight staging primitives from
//! `_dot_init_write_private_line` through
//! `_dot_init_stage_claim_remove`: the mode-600 single-line file
//! publisher, the transaction-derived entry stage path, the intent
//! record validator, and the five claim-marker helpers that prove a
//! stage directory belongs to this run. The file-generic
//! `_dot_init_error` diagnostic stays unported (a bare
//! `printf ... >&2; return 1` with no family state, absorbed into
//! [`Result`] the way earlier slices absorb engine diagnostics).
//! The transaction-directory lifecycle lives on
//! `rust-port-slice-35` (`init_client_transaction`), the host-git
//! identity family on `rust-port-slice-41`
//! (`init_client_identity`), the git-generation binding on
//! `rust-port-slice-43` (`init_client_generation`), and the record,
//! publish, delete, and rollback families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_NONCE` global and the client root from `HOME`.
//! Library code must not mutate the process environment behind the
//! engine, so those cross here as explicit parameters; the
//! `REPLY`-carried outputs (`_dot_init_entry_stage`,
//! `_dot_init_entry_intent`, `_dot_init_stage_claim_file`) return
//! their values instead.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};
use crate::repos_overlays::init_safe_relative_path;
use crate::temp;

/// First line of a stage claim marker: proves the file is ours
/// before any field is trusted. Both engines reject any other
/// header.
pub const STAGE_CLAIM_HEADER: &str = "cgraf78 dot publication stage claim v1";

/// File name of the claim marker inside a stage directory.
pub const STAGE_CLAIM_NAME: &str = ".dot-init-stage-claim-v1";

/// A validated entry intent record: the shell's `REPLY` from
/// [`entry_intent`] split into its six tab fields. The device and
/// inode fields stay strings because the `pending` phase spells
/// them `-`, exactly as the shell carries them.
pub struct EntryIntent {
    /// `pending`, `staged`, or `prepared`: how far publication got.
    pub phase: String,
    /// Home-relative stage directory bound to this entry.
    pub stage: String,
    /// Stage device (`-` while pending).
    pub dev: String,
    /// Stage inode (`-` while pending).
    pub ino: String,
    /// Published-file device (`-` until prepared).
    pub next_dev: String,
    /// Published-file inode (`-` until prepared).
    pub next_ino: String,
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`. `symlink_metadata` never
/// follows, so a link reports its own type and fails the gate on
/// both engines.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Effective-uid ownership (`test -O`): the shell gate requires
/// the marker to be ours. An unreadable identity fails closed,
/// like the shell's failed `stat`.
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// Append one path component with a plain `/` separator, like the
/// shell's `"$dir/$base"`: a `home` with a trailing slash keeps
/// its doubled separator instead of being normalized away.
fn join_slash(dir: &Path, component: &str) -> PathBuf {
    let mut out = dir.as_os_str().to_os_string();
    out.push("/");
    out.push(component);
    PathBuf::from(out)
}

/// `_dot_init_write_private_line`: publish one `line` (plus its
/// newline) at `file` at mode 600 through a sibling temp. With
/// `replace`, an existing file is replaced without touching a
/// late directory; otherwise the destination must be absent, like
/// the shell's `_dot_move_replace_nodir` / `_dot_move_noreplace`
/// split on the literal `true` third argument.
///
/// Like the shell, a failure after the sibling exists (body
/// write, chmod, or a live destination winning the race) leaves
/// the sibling behind: nothing later in the family reads those
/// names, so the shapes stay comparable across engines.
pub fn write_private_line(
    file: &Path,
    line: &str,
    replace: bool,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let temporary = temp::sibling_tmp_for(file)?;
    let mut body = line.as_bytes().to_vec();
    body.push(b'\n');
    std::fs::write(&temporary, &body).map_err(|source| Error::Io {
        context: "write private line",
        source,
    })?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| Error::Io {
            context: "chmod private line",
            source,
        },
    )?;
    if replace {
        temp::move_replace_nodir_cached(&temporary, file, cache)
    } else {
        temp::move_noreplace_cached(&temporary, file, cache)
    }
}

/// `_dot_init_entry_stage`: the transaction-derived stage path
/// for `path`: `$HOME/[$parent/].dot-init-entry.$nonce.$hash`
/// where `hash` is `git hash-object --stdin` over the raw path
/// bytes and `parent` is empty for a top-level entry, exactly as
/// the shell's `${path%/*}` (a trailing slash keeps its empty
/// leaf parent, since only tree paths ever arrive here).
/// `source_root` is the `DOT_SOURCE_ROOT` binding the hash
/// subprocess runs under; the digest depends only on the bytes.
pub fn entry_stage(home: &Path, path: &str, nonce: &str, source_root: &Path) -> Result<PathBuf> {
    let hash = temp::file_text_digest(source_root, path.as_bytes())?;
    let parent = match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    };
    let mut name = OsString::from(".dot-init-entry.");
    name.push(nonce);
    name.push(".");
    name.push(hash.as_str());
    if parent.is_empty() {
        Ok(join_slash(home, &name.to_string_lossy()))
    } else {
        let mut out = home.as_os_str().to_os_string();
        out.push("/");
        out.push(parent);
        out.push("/");
        out.push(name);
        Ok(PathBuf::from(out))
    }
}

/// True for a nonempty all-ASCII-digit field: the shell's
/// `[[ $value =~ ^[0-9]+$ ]]`.
fn is_digits(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(|byte| byte.is_ascii_digit())
}

/// `_dot_init_entry_intent`: validate the intent record at `file`
/// against `expected_mode`/`expected_oid`/`expected_path` and
/// report its six fields. The record must be a real file holding
/// one line (trailing newlines strip like the shell's
/// command substitution, an interior newline fails), exactly
/// nine tab fields — a tenth empty field from a trailing tab
/// passes, mirroring the shell's `read` into ten variables with
/// an empty `extra` — the expected mode/oid/path, the stage
/// [`entry_stage`] derives for this run, a known phase, and
/// phase-shaped device/inode fields (`prepared` binds all four
/// to digits, `staged` binds the stage pair with the `next` pair
/// still `-`, `pending` leaves all four `-`).
pub fn entry_intent(
    file: &Path,
    expected_mode: &str,
    expected_oid: &str,
    expected_path: &str,
    home: &Path,
    nonce: &str,
    source_root: &Path,
) -> Result<EntryIntent> {
    if !is_real_file(file) {
        return Err(Error::Usage {
            message: "entry intent is not a regular file",
        });
    }
    let mut bytes = std::fs::read(file).map_err(|source| Error::Io {
        context: "read entry intent",
        source,
    })?;
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.contains(&b'\n') {
        return Err(Error::Usage {
            message: "entry intent spans multiple lines",
        });
    }
    let parts: Vec<&[u8]> = bytes.split(|byte| *byte == b'\t').collect();
    let fields: &[&[u8]] = match parts.len() {
        9 => &parts,
        10 if parts[9].is_empty() => &parts[..9],
        _ => {
            return Err(Error::Usage {
                message: "entry intent has the wrong field count",
            });
        }
    };
    let expected_stage = entry_stage(home, expected_path, nonce, source_root)?;
    let mut prefix = home.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    let expected_rel: &[u8] = match expected_stage
        .as_os_str()
        .as_bytes()
        .strip_prefix(&prefix[..])
    {
        Some(rest) => rest,
        None => {
            return Err(Error::Usage {
                message: "entry stage escapes the client root",
            });
        }
    };
    let (phase, mode, oid, path, stage) = (fields[0], fields[1], fields[2], fields[3], fields[4]);
    let (dev, ino, next_dev, next_ino) = (fields[5], fields[6], fields[7], fields[8]);
    if !matches!(phase, b"pending" | b"staged" | b"prepared") {
        return Err(Error::Usage {
            message: "entry intent has an unknown phase",
        });
    }
    if mode != expected_mode.as_bytes()
        || oid != expected_oid.as_bytes()
        || path != expected_path.as_bytes()
        || stage != expected_rel
    {
        return Err(Error::Usage {
            message: "entry intent does not match its entry",
        });
    }
    let bound = match phase {
        b"prepared" => {
            is_digits(dev) && is_digits(ino) && is_digits(next_dev) && is_digits(next_ino)
        }
        b"staged" => is_digits(dev) && is_digits(ino) && next_dev == b"-" && next_ino == b"-",
        _ => dev == b"-" && ino == b"-" && next_dev == b"-" && next_ino == b"-",
    };
    if !bound {
        return Err(Error::Usage {
            message: "entry intent binds the wrong generation",
        });
    }
    let text = |field: &[u8]| {
        String::from_utf8(field.to_vec()).map_err(|_| Error::Usage {
            message: "entry intent is not UTF-8",
        })
    };
    Ok(EntryIntent {
        phase: text(phase)?,
        stage: text(stage)?,
        dev: text(dev)?,
        ino: text(ino)?,
        next_dev: text(next_dev)?,
        next_ino: text(next_ino)?,
    })
}

/// `_dot_init_stage_claim_file`: `<stage>/.dot-init-stage-claim-v1`.
/// Plain byte concatenation like the shell's `$1/...`, so a
/// `stage` with a trailing slash keeps its doubled separator
/// instead of being normalized away.
pub fn stage_claim_file(stage: &Path) -> PathBuf {
    join_slash(stage, STAGE_CLAIM_NAME)
}

/// Expected claim-marker bytes for one run: the header plus
/// `kind`/`nonce`/`path` lines, exactly as the shell's grouped
/// `printf` builds them before piping into
/// `_dot_stdin_matches_file`.
fn stage_claim_body(kind: &str, nonce: &str, path: &str) -> Vec<u8> {
    format!("{STAGE_CLAIM_HEADER}\nkind={kind}\nnonce={nonce}\npath={path}\n").into_bytes()
}

/// `_dot_init_stage_claim_matches`: the claim marker under `stage`
/// proves this run owns it. `kind` must be `entry` or `parent`,
/// `path` a safe home-relative path, and the marker a real file
/// owned by us at mode 600 with a single link whose bytes equal
/// `stage_claim_body()`. A failed read or hash run fails the
/// match rather than erroring, like the shell's `[[ ... ]]`
/// chain past a failed substitution.
pub fn stage_claim_matches(
    stage: &Path,
    kind: &str,
    path: &str,
    nonce: &str,
    source_root: &Path,
) -> bool {
    if !matches!(kind, "entry" | "parent") {
        return false;
    }
    if !init_safe_relative_path(path) {
        return false;
    }
    let marker = stage_claim_file(stage);
    if !is_real_file(&marker) || !owned_by_us(&marker) {
        return false;
    }
    let mode = match temp::file_mode(&marker) {
        Ok(mode) => mode,
        Err(_) => return false,
    };
    let links = match temp::path_nlink(&marker) {
        Ok(links) => links,
        Err(_) => return false,
    };
    if mode != 0o600 || links != 1 {
        return false;
    }
    let expected = stage_claim_body(kind, nonce, path);
    temp::stdin_matches_file(source_root, &expected, &marker).unwrap_or(false)
}

/// `_dot_init_stage_claim_write`: publish the claim marker for
/// (`stage`, `kind`, `path`) at mode 600 without replacing a live
/// marker, then verify it with [`stage_claim_matches`]. The
/// marker must be absent first, and the sibling temp carries the
/// bytes exactly like the shell's `_dot_sibling_tmp_for` plus
/// `_dot_move_noreplace`.
///
/// Like the shell, the write is unverified up front: an unknown
/// `kind` or unsafe `path` still publishes its bytes, and the
/// trailing gate fails — leaving the marker behind, so the
/// shapes stay comparable across engines.
pub fn stage_claim_write(
    stage: &Path,
    kind: &str,
    path: &str,
    nonce: &str,
    source_root: &Path,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let marker = stage_claim_file(stage);
    if std::fs::symlink_metadata(&marker).is_ok() {
        return Err(Error::Usage {
            message: "stage claim already exists",
        });
    }
    let temporary = temp::sibling_tmp_for(&marker)?;
    std::fs::write(&temporary, stage_claim_body(kind, nonce, path)).map_err(|source| {
        Error::Io {
            context: "write stage claim",
            source,
        }
    })?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| Error::Io {
            context: "chmod stage claim",
            source,
        },
    )?;
    temp::move_noreplace_cached(&temporary, &marker, cache)?;
    if !stage_claim_matches(stage, kind, path, nonce, source_root) {
        return Err(Error::Usage {
            message: "stage claim does not match after write",
        });
    }
    Ok(())
}

/// `_dot_init_stage_claim_only`: the stage holds nothing but its
/// claim marker. A directory read replaces the shell's
/// `nullglob`/`dotglob` enumeration (dotfiles included, a
/// missing directory reading as empty), and the lone entry must
/// carry exactly [`STAGE_CLAIM_NAME`].
pub fn stage_claim_only(stage: &Path) -> bool {
    let mut entries = Vec::new();
    let read = match std::fs::read_dir(stage) {
        Ok(read) => read,
        Err(_) => return false,
    };
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return false,
        };
        entries.push(entry.file_name());
    }
    entries.len() == 1 && entries[0].as_os_str().as_bytes() == STAGE_CLAIM_NAME.as_bytes()
}

/// `_dot_init_stage_claim_remove`: drop the claim marker after it
/// revalidates for (`stage`, `kind`, `path`), releasing the stage
/// for the empty-directory check and rename. A mismatch fails
/// without touching the marker, like the shell's gate before
/// `rm -f`; a marker already gone reads as removed, like `rm
/// -f` on a missing path.
pub fn stage_claim_remove(
    stage: &Path,
    kind: &str,
    path: &str,
    nonce: &str,
    source_root: &Path,
) -> Result<()> {
    if !stage_claim_matches(stage, kind, path, nonce, source_root) {
        return Err(Error::Usage {
            message: "stage claim does not match",
        });
    }
    let marker = stage_claim_file(stage);
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            context: "remove stage claim",
            source,
        }),
    }
}
