//! Per-entry publication staging for `lib/dot/init-client.sh`:
//! intent records, entry stage paths, stage claim markers,
//! entry-stage validation, and single-entry publication.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns twelve functions in two chapters. The staging
//! chapter (the `rust-port-slice-46` lane) owns the eight
//! primitives from `_dot_init_write_private_line` through
//! `_dot_init_stage_claim_remove`: the mode-600 single-line file
//! publisher, the transaction-derived entry stage path, the intent
//! record validator, and the five claim-marker helpers that prove a
//! stage directory belongs to this run. The publication chapter
//! (the `rust-port-slice-67` lane) owns the four contiguous
//! functions from `_dot_init_entry_stage_valid` through
//! `_dot_init_publish_one` in file order: the stage-directory gate
//! ([`entry_stage_valid`]), the stage-content gate
//! ([`entry_stage_only_next`]), the staged `next` cleanup
//! ([`discard_staged_next`]), and the pending / staged publication
//! driver ([`publish_one`]). The lanes merged by deduplication:
//! the shapes both chapters need ([`EntryIntent`],
//! [`STAGE_CLAIM_NAME`], `owned_by_us`, `join_slash`) are defined
//! once below. The file-generic `_dot_init_error` diagnostic stays
//! unported (a bare `printf ... >&2; return 1` with no family
//! state, absorbed into [`Result`] the way earlier slices absorb
//! engine diagnostics).
//! The transaction-directory lifecycle lives on
//! `rust-port-slice-35` (`init_client_transaction`), the host-git
//! identity family on `rust-port-slice-41`
//! (`init_client_identity`), the git-generation binding on
//! `rust-port-slice-43` (`init_client_generation`), and the record,
//! publish, delete, and rollback families live on their own lanes
//! (`init_client_record`, `init_client_records`,
//! `init_client_publish`, `init_client_delete`,
//! `init_client_rollback`). The prior-record reader above the
//! publication chapter (`_dot_init_prior_record`) lives on another
//! lane and is not touched here.
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
//! their values instead. The publication chapter takes the same
//! coordinates plus its staging neighbors as caller-supplied
//! closures (see [`PublishOneInputs`]): the nonce never crosses at
//! all (it lives inside the injected staging closures). Git runs
//! plain like the shell's bare `git` (see
//! [`crate::repos_base::run_git`]): blob bytes are
//! locale-independent, and git's own diagnostics are not part of
//! the ported surface. The umask the tracked-mode setter honors
//! crosses as [`PublishOneInputs::mask`], the way the staged-clone
//! lane takes its ceiling mask.
//!
//! Byte-fidelity boundary: every `$HOME/$path` join concatenates
//! bytes like the shell (see `join_slash`), preserving a doubled
//! separator on trailing-slash inputs instead of normalizing it
//! away. The `next` home-relative reference is the intent's stage
//! plus `/next` by construction, exactly what the shell's
//! `${next#"$HOME"/}` strip yields. The `git show` redirect
//! pre-creates its target, so a failed show still leaves the
//! (partial) bytes behind like the shell. Command substitution
//! drops NUL bytes before the symlink-target gate, and `rm -f`
//! passes when its target vanished underneath it.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};
use crate::repos_base::run_git;
use crate::repos_overlays::init_safe_relative_path;
use crate::temp::{self, MoveCache};

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

/// File name of the staged publication candidate inside a stage
/// directory: the shell's `$stage/next`.
pub const NEXT_NAME: &str = "next";

/// `_dot_init_parent_directories`: ensure the published parents of
/// `path` exist under the client root, recording each step in
/// `transaction`. Injected because the transaction lane is
/// unmerged; tests feed a closure running the live shell.
pub type EnsureParents<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// `_dot_init_entry_intent`: validate the intent record at `file`
/// against the expected mode, object id, and path. Injected
/// because the neighbor lane is unmerged; tests feed a closure
/// running the live shell.
pub type ReadIntent<'a> = dyn Fn(&Path, &str, &str, &str) -> Result<EntryIntent> + 'a;

/// `_dot_init_stage_claim_matches` for kind `entry`: the claim
/// marker under `stage` proves this run owns it for `path`.
/// Injected because the claim lane is unmerged; tests feed a
/// closure running the live shell.
pub type ClaimMatches<'a> = dyn Fn(&Path, &str) -> bool + 'a;

/// `_dot_init_stage_claim_write` for kind `entry`: publish the
/// claim marker for (`stage`, `path`). Injected because the claim
/// lane is unmerged; tests feed a closure running the live shell.
pub type ClaimWrite<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// `_dot_init_stage_claim_remove` for kind `entry`: drop the claim
/// marker after it revalidates. Injected because the claim lane is
/// unmerged; tests feed a closure running the live shell.
pub type ClaimRemove<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// `_dot_init_write_private_line`: publish one line at `file`,
/// replacing when `replace` is set. Injected because the record
/// lane is unmerged; tests feed a closure running the live shell.
pub type WriteIntentLine<'a> = dyn Fn(&Path, &str, bool) -> Result<()> + 'a;

/// `_dot_init_candidate_matches_git`: the home-relative `path`
/// holds `mode`/`oid` from `commit` in `git_dir`. Injected
/// because the candidate lane is unmerged; tests feed a closure
/// running the live shell.
pub type CandidateMatches<'a> = dyn Fn(&str, &str, &str, &str, &str) -> bool + 'a;

/// Inputs for [`publish_one`]: the publication coordinates plus
/// the unmerged-lane closures above.
pub struct PublishOneInputs<'a> {
    /// Client root: the shell's `HOME`.
    pub home: &'a Path,
    /// Transaction directory holding parent/intent records.
    pub transaction: &'a Path,
    /// Intent record path for this entry.
    pub intent: &'a Path,
    /// Object store for the blob reads: the shell's `git_dir`.
    pub git_dir: &'a str,
    /// Commit the blob is published from.
    pub commit: &'a str,
    /// Git mode of the entry (`100644`, `100755`, `120000`).
    pub mode: &'a str,
    /// Object id the published bytes must hash to.
    pub oid: &'a str,
    /// Home-relative path being published.
    pub path: &'a str,
    /// Live umask for the tracked-mode setter (the caller reads
    /// it, like the shell's inline symbolic `chmod` honors it).
    pub mask: u32,
    /// Parent-directory provision (the transaction lane).
    pub ensure_parents: &'a EnsureParents<'a>,
    /// Intent-record validation (the neighbor lane).
    pub read_intent: &'a ReadIntent<'a>,
    /// Stage-claim verification (the claim lane, kind `entry`).
    pub claim_matches: &'a ClaimMatches<'a>,
    /// Stage-claim publication (the claim lane, kind `entry`).
    pub claim_write: &'a ClaimWrite<'a>,
    /// Stage-claim release (the claim lane, kind `entry`).
    pub claim_remove: &'a ClaimRemove<'a>,
    /// Intent-line journal (the record lane).
    pub write_line: &'a WriteIntentLine<'a>,
    /// Published-content verification (the candidate lane).
    pub candidate_matches: &'a CandidateMatches<'a>,
}

/// Any filesystem presence, dangling links included: the shell's
/// `[[ -e $path || -L $path ]]`. `symlink_metadata` never
/// follows, so a link reports itself.
fn any_presence(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// True for a link-target candidate the journal can carry: the
/// shell's `_dot_init_safe_value` (nonempty, no tab, newline, or
/// carriage return).
fn safe_value(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && !bytes
            .iter()
            .any(|byte| matches!(byte, b'\t' | b'\n' | b'\r'))
}

/// Run `git --git-dir=<git_dir> show <commit>:<path>`: the
/// captured stdout plus whether git reported success. `None` git
/// output (git never ran) reads as empty bytes, like the shell's
/// redirect creating an empty file before a failed exec.
fn git_show(git_dir: &str, commit: &str, path: &str) -> (Vec<u8>, bool) {
    let prefix = [OsString::from("--git-dir"), OsString::from(git_dir)];
    let spec = format!("{commit}:{path}");
    match run_git(&prefix, &["show", spec.as_str()]) {
        Some(output) => (output.stdout, output.status.success()),
        None => (Vec::new(), false),
    }
}

/// `_dot_init_entry_stage_valid`: `stage` is an owned real
/// directory with no group/other permission bits, and — when
/// `expected_identity` is nonempty — its `dev:ino` identity
/// matches. The shell's octal-digit guard is subsumed by computing
/// the bits directly, the way the overlay lane's
/// `private_directory` does.
pub fn entry_stage_valid(stage: &Path, expected_identity: Option<&str>) -> bool {
    let meta = match std::fs::symlink_metadata(stage) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    // `symlink_metadata` reports the link itself, so a passing
    // `is_dir` already excludes links; the explicit gate mirrors
    // the shell's `[[ -d $stage && ! -L $stage ]]` shape.
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    if !owned_by_us(stage) {
        return false;
    }
    if meta.mode() & 0o077 != 0 {
        return false;
    }
    match expected_identity {
        None => true,
        // The shell defaults the missing argument to empty and
        // skips the check on `[[ -z $expected_identity ]]`.
        Some("") => true,
        Some(expected) => temp::path_identity(stage)
            .is_ok_and(|identity| temp::identity_string(identity) == expected),
    }
}

/// `_dot_init_entry_stage_only_next`: `stage` holds nothing but
/// the staged candidate and the claim marker. A directory read
/// replaces the shell's `nullglob`/`dotglob` enumeration
/// (dotfiles included, `.`/`..` excluded like the shell's `*`),
/// and every entry must carry exactly [`NEXT_NAME`] or
/// [`STAGE_CLAIM_NAME`]. A missing (or unreadable) stage expands
/// to nothing under `nullglob`, so the loop passes vacuously.
pub fn entry_stage_only_next(stage: &Path) -> bool {
    let read = match std::fs::read_dir(stage) {
        Ok(read) => read,
        Err(_) => return true,
    };
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return false,
        };
        let name = entry.file_name();
        let name = name.as_os_str().as_bytes();
        if name != NEXT_NAME.as_bytes() && name != STAGE_CLAIM_NAME.as_bytes() {
            return false;
        }
    }
    true
}

/// `_dot_init_discard_staged_next`: drop the staged candidate
/// after the stage gates for content. A missing candidate is
/// already discarded; anything that is neither a real file nor a
/// symlink (a directory, fifo, socket) refuses, like the shell's
/// type gate before `rm -f`.
pub fn discard_staged_next(stage: &Path) -> Result<()> {
    if !entry_stage_only_next(stage) {
        return Err(Error::Usage {
            message: "entry stage holds more than its next",
        });
    }
    let next = join_slash(stage, NEXT_NAME);
    let meta = match std::fs::symlink_metadata(&next) {
        Ok(meta) => meta,
        // `[[ -e $next || -L $next ]]` missed: nothing to discard.
        Err(_) => return Ok(()),
    };
    // `[[ (-f $next && ! -L $next) || -L $next ]]`:
    // `symlink_metadata` never follows, so `is_file` already
    // excludes links.
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "entry next is not a file",
        });
    }
    match std::fs::remove_file(&next) {
        Ok(()) => Ok(()),
        // `rm -f` passes when the name vanished underneath it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            context: "remove entry next",
            source,
        }),
    }
}

/// `_dot_init_publish_one`: drive one entry from its intent record
/// to its home path through the stage directory. A `pending`
/// intent claims (or revalidates) the stage and binds it to its
/// device/inode; a `staged` intent turns the committed blob into
/// the `next` candidate and binds that too; then the candidate is
/// verified, moved into place without replacing a live path, and
/// the stage is released. Any other phase skips both transitions
/// and runs the final verification with the intent's bindings,
/// which fails once the stage is gone — like the shell falling
/// through to the same checks.
pub fn publish_one(inputs: &PublishOneInputs<'_>, moves: &mut MoveCache) -> Result<()> {
    let target = join_slash(inputs.home, inputs.path);
    (inputs.ensure_parents)(inputs.transaction, inputs.path)?;
    let intent = (inputs.read_intent)(inputs.intent, inputs.mode, inputs.oid, inputs.path)?;
    let stage = join_slash(inputs.home, &intent.stage);
    let next = join_slash(&stage, NEXT_NAME);
    let next_rel = format!("{}/{NEXT_NAME}", intent.stage);
    let mut phase = intent.phase.clone();
    let mut stage_dev = intent.dev.clone();
    let mut stage_ino = intent.ino.clone();
    let mut next_dev = intent.next_dev.clone();
    let mut next_ino = intent.next_ino.clone();
    if phase == "pending" {
        if any_presence(&stage) {
            if !entry_stage_valid(&stage, None) {
                return Err(Error::Usage {
                    message: "entry stage is not valid",
                });
            }
            if !(inputs.claim_matches)(&stage, inputs.path) {
                return Err(Error::Usage {
                    message: "entry stage claim does not match",
                });
            }
            if !entry_stage_only_next(&stage) {
                return Err(Error::Usage {
                    message: "entry stage holds more than its next",
                });
            }
            if any_presence(&next) {
                return Err(Error::Usage {
                    message: "entry next already exists",
                });
            }
        } else {
            std::fs::create_dir(&stage).map_err(|source| Error::Io {
                context: "create entry stage",
                source,
            })?;
            std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).map_err(
                |source| Error::Io {
                    context: "chmod entry stage",
                    source,
                },
            )?;
            (inputs.claim_write)(&stage, inputs.path)?;
        }
        let (dev, ino) = temp::path_identity(&stage).map_err(|_| Error::Usage {
            message: "entry stage has no identity",
        })?;
        stage_dev = dev.to_string();
        stage_ino = ino.to_string();
        // The staged line binds the container before the blob
        // redirect below can leave partial bytes: a crash during
        // `git show` then rolls back without mistaking the
        // incomplete file for a finished candidate.
        let line = format!(
            "staged\t{}\t{}\t{}\t{}\t{stage_dev}\t{stage_ino}\t-\t-",
            inputs.mode, inputs.oid, inputs.path, intent.stage,
        );
        (inputs.write_line)(inputs.intent, line.as_str(), true)?;
        phase = "staged".to_string();
    }
    if phase == "staged" {
        let wanted = format!("{stage_dev}:{stage_ino}");
        if !entry_stage_valid(&stage, Some(wanted.as_str())) {
            return Err(Error::Usage {
                message: "entry stage is not valid",
            });
        }
        if !(inputs.claim_matches)(&stage, inputs.path) {
            return Err(Error::Usage {
                message: "entry stage claim does not match",
            });
        }
        discard_staged_next(&stage)?;
        match inputs.mode {
            "100644" | "100755" => {
                // The shell's redirect creates the file before git
                // runs, so a failed show still leaves the (partial)
                // bytes behind for rollback to sweep.
                let (body, ok) = git_show(inputs.git_dir, inputs.commit, inputs.path);
                std::fs::write(&next, &body).map_err(|source| Error::Io {
                    context: "write entry next",
                    source,
                })?;
                if !ok {
                    return Err(Error::Usage {
                        message: "entry blob read failed",
                    });
                }
                temp::apply_tracked_file_mode(&next, inputs.mode, inputs.mask)?;
            }
            "120000" => {
                let (mut link_target, ok) = git_show(inputs.git_dir, inputs.commit, inputs.path);
                if !ok {
                    return Err(Error::Usage {
                        message: "entry blob read failed",
                    });
                }
                // Command substitution drops NUL bytes before the
                // value gate ever sees them.
                link_target.retain(|byte| *byte != 0);
                if !safe_value(&link_target) {
                    return Err(Error::Usage {
                        message: "entry link target is not a safe value",
                    });
                }
                std::os::unix::fs::symlink(OsString::from_vec(link_target), &next).map_err(
                    |source| Error::Io {
                        context: "link entry next",
                        source,
                    },
                )?;
            }
            _ => {
                return Err(Error::Usage {
                    message: "entry has an unsupported mode",
                });
            }
        }
        if !(inputs.candidate_matches)(
            inputs.git_dir,
            inputs.commit,
            inputs.mode,
            inputs.oid,
            next_rel.as_str(),
        ) {
            return Err(Error::Usage {
                message: "entry candidate does not match git",
            });
        }
        let (dev, ino) = temp::path_identity(&next).map_err(|_| Error::Usage {
            message: "entry next has no identity",
        })?;
        next_dev = dev.to_string();
        next_ino = ino.to_string();
        let line = format!(
            "prepared\t{}\t{}\t{}\t{}\t{stage_dev}\t{stage_ino}\t{next_dev}\t{next_ino}",
            inputs.mode, inputs.oid, inputs.path, intent.stage,
        );
        (inputs.write_line)(inputs.intent, line.as_str(), true)?;
    }
    let wanted_stage = format!("{stage_dev}:{stage_ino}");
    if !entry_stage_valid(&stage, Some(wanted_stage.as_str())) {
        return Err(Error::Usage {
            message: "entry stage is not valid",
        });
    }
    if !(inputs.claim_matches)(&stage, inputs.path) {
        return Err(Error::Usage {
            message: "entry stage claim does not match",
        });
    }
    if !entry_stage_only_next(&stage) {
        return Err(Error::Usage {
            message: "entry stage holds more than its next",
        });
    }
    let wanted_next = format!("{next_dev}:{next_ino}");
    // The shell reads the identity through `|| true`: a missing
    // candidate compares as empty and fails the gate.
    let landed = temp::path_identity(&next).map_err(|_| Error::Usage {
        message: "entry next has no identity",
    })?;
    if temp::identity_string(landed) != wanted_next {
        return Err(Error::Usage {
            message: "entry next changed under us",
        });
    }
    if !(inputs.candidate_matches)(
        inputs.git_dir,
        inputs.commit,
        inputs.mode,
        inputs.oid,
        next_rel.as_str(),
    ) {
        return Err(Error::Usage {
            message: "entry candidate does not match git",
        });
    }
    temp::move_noreplace_cached(&next, &target, moves)?;
    let placed = temp::path_identity(&target).map_err(|_| Error::Usage {
        message: "published entry has no identity",
    })?;
    if temp::identity_string(placed) != wanted_next {
        return Err(Error::Usage {
            message: "published entry changed under us",
        });
    }
    (inputs.claim_remove)(&stage, inputs.path)?;
    std::fs::remove_dir(&stage).map_err(|source| Error::Io {
        context: "remove entry stage",
        source,
    })?;
    if !(inputs.candidate_matches)(
        inputs.git_dir,
        inputs.commit,
        inputs.mode,
        inputs.oid,
        inputs.path,
    ) {
        return Err(Error::Usage {
            message: "published entry does not match git",
        });
    }
    Ok(())
}
