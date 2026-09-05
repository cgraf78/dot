//! Transaction record journal for `lib/dot/init-client.sh`: the
//! write/read/advance cycle plus the parent and prior lookups.
//!
//! The shell file holds 78 functions — too big for one lane — so
//! this module owns only the five record primitives from
//! `_dot_init_safe_value` through `_dot_init_prior_record` in file
//! order, skipping what other lanes already own: the transaction
//! record lifecycle (`_dot_init_write_record`,
//! `_dot_init_read_record`, `_dot_init_record_phase`) with the
//! parent-intent and prior-snapshot readers (`_dot_init_parent_record`,
//! `_dot_init_prior_record`). The file-generic `_dot_init_error`
//! diagnostic stays unported (a bare `printf ... >&2; return 1`
//! with no family state, absorbed into [`Result`] the way earlier
//! slices absorb engine diagnostics). The sanitizers stay where
//! they are: `_dot_init_safe_relative_path` already lives in the
//! base tree as [`crate::repos_overlays::init_safe_relative_path`]
//! and this module reuses it through byte-local twins, the way the
//! identity and candidate lanes vendor their own copies.
//!
//! Lane map, so the integrator can stack without overlap: the
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family on
//! `rust-port-slice-41` (`init_client_identity`), the git-generation
//! binding on `rust-port-slice-43` (`init_client_generation`), the
//! per-entry staging family on `rust-port-slice-46`
//! (`init_client_entry`), and the candidate planning family on
//! `rust-port-slice-48` (`init_client_candidate`). The publish
//! (`publish_intent`, `publish_one`, `publish_worktree`,
//! `published_stage_matches`, `published_intent_matches`,
//! `cleanup_published_stage`), delete, rollback, resume, and status
//! families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals, the client root from `HOME`, the dot
//! binary from `DOT_BIN`, and the source checkout from
//! `DOT_SOURCE_ROOT`. Library code must not read process
//! environment behind the engine, so those cross here as explicit
//! parameters (the [`RecordFields`] bundle); the `REPLY`-carried
//! outputs (`_dot_init_read_record`'s thirteen globals,
//! `_dot_init_parent_record`, `_dot_init_prior_record`) return
//! structs instead.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};
use crate::temp;

/// First line of every transaction record: proves the file is ours
/// before any field is trusted. Both engines reject any other
/// header.
pub const RECORD_HEADER: &str = "cgraf78 dot initialization transaction v1";

/// Commit the shell records when `DOT_INIT_COMMIT` is unset or
/// empty: forty zero nibbles, exactly the shell's
/// `${DOT_INIT_COMMIT:-000...0}` default.
pub const ZERO_COMMIT: &str = "0000000000000000000000000000000000000000";

/// Largest record the shell will parse: `size -le 16384`, checked
/// from the file length before any byte is trusted.
const MAX_RECORD_SIZE: u64 = 16384;

/// Exact line count of a record: the header plus thirteen
/// `key=value` lines. The shell counts with `read`, so a missing
/// trailing newline still yields its final line and adds no
/// phantom line — the split below mirrors that.
const RECORD_LINES: usize = 14;

/// Run-supplied record body: everything `_dot_init_write_record`
/// and `_dot_init_record_phase` take beyond the destination and
/// phase. One bundle because the phase advance re-resolves every
/// field from the live run (never from the file it overwrites),
/// exactly like the shell re-reads its globals per call.
pub struct RecordFields<'a> {
    /// Expected origin URL (`DOT_INIT_ORIGIN`).
    pub origin: &'a str,
    /// Canonical repository identity (`DOT_INIT_IDENTITY`).
    pub identity: &'a str,
    /// Branch being installed (`DOT_INIT_BRANCH`).
    pub branch: &'a str,
    /// Backup root (`DOT_INIT_BACKUP`).
    pub backup: &'a str,
    /// Live git directory (`DOT_INIT_GIT_DIR`). `None` or empty
    /// selects `$HOME/.dotfiles`, like the shell's `${7:-...}`.
    pub git_dir: Option<&'a Path>,
    /// Locked commit (`DOT_INIT_COMMIT`). `None` or empty selects
    /// [`ZERO_COMMIT`].
    pub commit: Option<&'a str>,
    /// Run nonce (`DOT_INIT_NONCE`). `None` or empty selects
    /// `legacy`.
    pub nonce: Option<&'a str>,
    /// Staged git device (`DOT_INIT_GIT_DEV`). `None` or empty
    /// selects `-`.
    pub git_dev: Option<&'a str>,
    /// Staged git inode (`DOT_INIT_GIT_INO`). `None` or empty
    /// selects `-`.
    pub git_ino: Option<&'a str>,
    /// Dot binary path (`DOT_BIN`).
    pub dot_bin: &'a str,
    /// Client root (`HOME`): the `worktree` line and the default
    /// git directory anchor on this.
    pub home: &'a Path,
    /// Source checkout (`DOT_SOURCE_ROOT`): `dot_revision` is
    /// `git rev-parse HEAD` bound here, like `_dot_source_git`.
    pub source_root: &'a Path,
}

/// A validated transaction record: the shell's thirteen
/// `DOT_INIT_*` globals from [`read_record`] as owned values.
/// Device and inode fields stay strings because an unbound run
/// spells them `-`, exactly as the shell carries them.
pub struct TransactionRecord {
    /// Lifecycle phase (`prepared`, `backing-up`, ...).
    pub phase: String,
    /// Expected origin URL.
    pub origin: String,
    /// Canonical repository identity.
    pub identity: String,
    /// Branch being installed.
    pub branch: String,
    /// Locked commit (40 or 64 hex nibbles).
    pub commit: String,
    /// Live git directory (`$HOME/.dotfiles` or `$HOME/.git`).
    pub git_dir: String,
    /// Client root (always `HOME`).
    pub worktree: String,
    /// Backup root (`-` or under `$HOME/.dot-backup/`).
    pub backup: String,
    /// Dot binary path.
    pub dot: String,
    /// Source checkout revision (40 or 64 hex nibbles).
    pub dot_revision: String,
    /// Run nonce.
    pub nonce: String,
    /// Staged git device (`-` or digits).
    pub git_dev: String,
    /// Staged git inode (`-` or digits).
    pub git_ino: String,
}

/// A validated parent intent: the shell's `REPLY` from
/// [`parent_record`] split into its five tab fields. Device,
/// inode, and mode stay strings because the `pending` phase
/// spells them `-`, exactly as the shell carries them.
pub struct ParentRecord {
    /// `pending` or `prepared`: how far parent publication got.
    pub phase: String,
    /// Home-relative stage directory bound to this parent.
    pub stage: String,
    /// Stage device (`-` while pending).
    pub dev: String,
    /// Stage inode (`-` while pending).
    pub ino: String,
    /// Stage mode (`-` while pending).
    pub mode: String,
}

/// One prior-snapshot entry: the shell's `REPLY` from
/// [`prior_record`] split into its six tab fields. The value
/// keeps the rest of the line verbatim (later tabs included),
/// like the shell's `read` into seven variables.
pub struct PriorEntry {
    /// Frozen kind (`absent`, `regular`, `symlink`, `directory`).
    pub kind: String,
    /// Frozen device (`-` when absent).
    pub dev: String,
    /// Frozen inode (`-` when absent).
    pub ino: String,
    /// Frozen octal mode (`-` when absent).
    pub mode: String,
    /// Frozen byte size (`-` when absent).
    pub size: String,
    /// Content token: blob oid, link target, or `-`.
    pub value: String,
}

/// `_dot_init_safe_value`: nonempty with no tab, newline, or
/// carriage-return byte. (Twin of the base tree's gate inside
/// `repos_overlays`; kept local because that module is a sibling
/// owner, not a shared helper.)
fn safe_value(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| !matches!(byte, b'\t' | b'\n' | b'\r'))
}

/// True for a nonempty all-ASCII-digit field: the shell's
/// `[[ $value =~ ^[0-9]+$ ]]`.
fn is_digits(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(|byte| byte.is_ascii_digit())
}

/// True for a nonempty all-ASCII-octal-digit field: the shell's
/// `[[ $mode =~ ^[0-7]+$ ]]` for a prepared parent stage.
fn is_octal(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(|byte| matches!(byte, b'0'..=b'7'))
}

/// True for a 40- or 64-nibble hex object id: the shell's
/// `^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$`.
fn is_oid(value: &[u8]) -> bool {
    (value.len() == 40 || value.len() == 64) && value.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// True for a nonce: the shell's `^[A-Za-z0-9._-]+$`.
fn is_nonce(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`. `symlink_metadata` never
/// follows, so a link reports its own type and fails the gate on
/// both engines.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Any existing path or dangling link: the shell's
/// `[[ -e $path || -L $path ]]`.
fn exists_or_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Effective-uid ownership (`test -O`): the shell gate requires
/// the record to be ours. An unreadable identity fails closed,
/// like the shell's failed `-O`. (Twin of the generation and
/// entry modules' gates; kept local because those modules are
/// sibling owners, not shared helpers.)
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// Append one path component with a plain `/` separator, like the
/// shell's `"$dir/$base"`: a `dir` with a trailing slash keeps
/// its doubled separator instead of being normalized away.
fn join_slash(dir: &Path, component: &str) -> PathBuf {
    let mut out = dir.as_os_str().to_os_string();
    out.push("/");
    out.push(component);
    PathBuf::from(out)
}

/// Branch-name gate: the shell's one-line `_dot_init_branch_valid`
/// (`[[ -n $1 ]] && git check-ref-format --branch "$1"`), inlined
/// here the way the candidate module inlines its git probes — the
/// identity family's `branch_valid` stays the canonical port on
/// its lane, and an empty name fails on both engines without
/// forking.
fn branch_valid(branch: &[u8]) -> bool {
    use std::process::Stdio;
    if branch.is_empty() {
        return false;
    }
    std::process::Command::new("git")
        .arg("check-ref-format")
        .arg("--branch")
        .arg(std::ffi::OsStr::from_bytes(branch))
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The source checkout revision: `_dot_source_git rev-parse HEAD`
/// under the sanitized binding. Trailing newlines chomp like the
/// shell's command substitution; the bytes otherwise cross
/// untouched, so no UTF-8 assumption sneaks in before the body
/// is assembled.
fn source_revision(source_root: &Path) -> Result<Vec<u8>> {
    use std::process::Stdio;
    let output = temp::sanitized_git(source_root, &["rev-parse", "HEAD"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|source| Error::Io {
            context: "read source revision",
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "git rev-parse HEAD".to_string(),
            status: Some(output.status.to_string()),
        });
    }
    let mut revision = output.stdout;
    while revision.last() == Some(&b'\n') {
        revision.pop();
    }
    Ok(revision)
}

/// `_dot_init_write_record`: publish the fourteen-line journal at
/// `destination` at mode 600 through a sibling temp. A live file
/// or link is replaced without touching a late directory,
/// otherwise the destination must be absent, like the shell's
/// `_dot_move_replace_nodir` / `_dot_move_noreplace` split.
/// Values cross raw (the shell validates on read, never on
/// write); unset-or-empty optionals take the shell's `:-`
/// defaults.
///
/// Like the shell, a failed body write removes the sibling
/// before failing, while a later failure (chmod, or a live
/// destination winning the race) leaves it behind: nothing later
/// in the family reads those names, so the shapes stay
/// comparable across engines.
pub fn write_record(
    destination: &Path,
    phase: &str,
    fields: &RecordFields<'_>,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let revision = source_revision(fields.source_root)?;
    fn defined(value: Option<&str>) -> Option<&str> {
        value.filter(|text| !text.is_empty())
    }
    let commit = defined(fields.commit).unwrap_or(ZERO_COMMIT);
    let nonce = defined(fields.nonce).unwrap_or("legacy");
    let git_dev = defined(fields.git_dev).unwrap_or("-");
    let git_ino = defined(fields.git_ino).unwrap_or("-");
    let git_dir = match fields.git_dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => join_slash(fields.home, ".dotfiles"),
    };
    let temporary = temp::sibling_tmp_for(destination)?;
    let mut body = Vec::new();
    body.extend_from_slice(RECORD_HEADER.as_bytes());
    body.push(b'\n');
    for (key, value) in [
        ("phase", phase.as_bytes()),
        ("origin", fields.origin.as_bytes()),
        ("identity", fields.identity.as_bytes()),
        ("branch", fields.branch.as_bytes()),
        ("commit", commit.as_bytes()),
        ("git_dir", git_dir.as_os_str().as_bytes()),
        ("worktree", fields.home.as_os_str().as_bytes()),
        ("backup", fields.backup.as_bytes()),
        ("dot", fields.dot_bin.as_bytes()),
        ("dot_revision", &revision[..]),
        ("nonce", nonce.as_bytes()),
        ("git_dev", git_dev.as_bytes()),
        ("git_ino", git_ino.as_bytes()),
    ] {
        body.extend_from_slice(key.as_bytes());
        body.push(b'=');
        body.extend_from_slice(value);
        body.push(b'\n');
    }
    if let Err(source) = std::fs::write(&temporary, &body) {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::Io {
            context: "write transaction record",
            source,
        });
    }
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| Error::Io {
            context: "chmod transaction record",
            source,
        },
    )?;
    if exists_or_link(destination) {
        temp::move_replace_nodir_cached(&temporary, destination, cache)
    } else {
        temp::move_noreplace_cached(&temporary, destination, cache)
    }
}

/// Strict UTF-8 for one validated record field: reachable records
/// are written by [`write_record`] from `&str`, so anything else
/// is hand-crafted input failing closed, the way the entry
/// module's intent validator treats non-UTF-8 fields.
fn record_text(field: &[u8]) -> Result<String> {
    String::from_utf8(field.to_vec()).map_err(|_| Error::Usage {
        message: "transaction record is not UTF-8",
    })
}

/// `_dot_init_read_record`: validate the journal at `record` and
/// report its thirteen fields. The file must be a real file owned
/// by us at no more than 16384 bytes with group/other permission
/// bits clear (the shell's `stat` spelling compared as octal, so
/// the bit test here agrees on every spelling); then exactly
/// fourteen lines (header plus one `key=value` each, no
/// duplicates, no unknown keys, values passing the local
/// `safe_value` gate),
/// the required fields nonempty, `git_dir` and `worktree` pinned
/// to `home`, a known phase, a well-formed branch, hex commit and
/// revision, an absolute `dot` without doubled or dot segments, a
/// class-shaped nonce, paired `-`/digits device fields, and a `-`
/// or `$HOME/.dot-backup/`-rooted backup.
///
/// Line splitting follows the shell's `read`: bytes divide on
/// `\n`, a missing trailing newline still yields its final line,
/// and a trailing newline adds no phantom empty line. Carriage
/// returns stay put, so only mutually agreeing bytes pass on both
/// engines.
pub fn read_record(record: &Path, home: &Path) -> Result<TransactionRecord> {
    if !is_real_file(record) || !owned_by_us(record) {
        return Err(Error::Usage {
            message: "transaction record is not ours",
        });
    }
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::symlink_metadata(record).map_err(|source| Error::Io {
        context: "stat transaction record",
        source,
    })?;
    if meta.len() > MAX_RECORD_SIZE {
        return Err(Error::Usage {
            message: "transaction record is too large",
        });
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(Error::Usage {
            message: "transaction record is group- or world-accessible",
        });
    }
    let bytes = std::fs::read(record).map_err(|source| Error::Io {
        context: "read transaction record",
        source,
    })?;
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    if lines.len() != RECORD_LINES {
        return Err(Error::Usage {
            message: "transaction record has the wrong line count",
        });
    }
    if lines[0] != RECORD_HEADER.as_bytes() {
        return Err(Error::Usage {
            message: "transaction record has the wrong header",
        });
    }
    let mut seen = [false; 13];
    let mut values: [Option<&[u8]>; 13] = [None; 13];
    for line in &lines[1..] {
        let mark = match line.iter().position(|byte| *byte == b'=') {
            Some(mark) => mark,
            None => {
                return Err(Error::Usage {
                    message: "transaction record line has no value",
                });
            }
        };
        let (key, value) = (&line[..mark], &line[mark + 1..]);
        if !safe_value(value) {
            return Err(Error::Usage {
                message: "transaction record value is unsafe",
            });
        }
        let slot = match key {
            b"phase" => 0,
            b"origin" => 1,
            b"identity" => 2,
            b"branch" => 3,
            b"commit" => 4,
            b"git_dir" => 5,
            b"worktree" => 6,
            b"backup" => 7,
            b"dot" => 8,
            b"dot_revision" => 9,
            b"nonce" => 10,
            b"git_dev" => 11,
            b"git_ino" => 12,
            _ => {
                return Err(Error::Usage {
                    message: "transaction record has an unknown key",
                });
            }
        };
        if seen[slot] {
            return Err(Error::Usage {
                message: "transaction record repeats a key",
            });
        }
        seen[slot] = true;
        values[slot] = Some(value);
    }
    let mut fields: [&[u8]; 13] = [b""; 13];
    for (slot, value) in values.iter().enumerate() {
        match value {
            Some(value) => fields[slot] = value,
            None => {
                return Err(Error::Usage {
                    message: "transaction record misses a key",
                });
            }
        }
    }
    let (phase, origin, identity, branch, commit) =
        (fields[0], fields[1], fields[2], fields[3], fields[4]);
    let (git_dir, worktree, backup, dot, dot_revision) =
        (fields[5], fields[6], fields[7], fields[8], fields[9]);
    let (nonce, git_dev, git_ino) = (fields[10], fields[11], fields[12]);
    let home_bytes = home.as_os_str().as_bytes();
    if phase.is_empty()
        || origin.is_empty()
        || identity.is_empty()
        || branch.is_empty()
        || backup.is_empty()
    {
        return Err(Error::Usage {
            message: "transaction record misses a required value",
        });
    }
    let mut dotfiles = home_bytes.to_vec();
    dotfiles.extend_from_slice(b"/.dotfiles");
    let mut dotgit = home_bytes.to_vec();
    dotgit.extend_from_slice(b"/.git");
    if git_dir != dotfiles.as_slice() && git_dir != dotgit.as_slice() {
        return Err(Error::Usage {
            message: "transaction record leaves the client root",
        });
    }
    if worktree != home_bytes {
        return Err(Error::Usage {
            message: "transaction record leaves the client root",
        });
    }
    if !matches!(
        phase,
        b"prepared"
            | b"backing-up"
            | b"backed-up"
            | b"git-staging"
            | b"git-staged"
            | b"publishing"
            | b"checkout"
            | b"converging"
            | b"complete"
    ) {
        return Err(Error::Usage {
            message: "transaction record has an unknown phase",
        });
    }
    if !branch_valid(branch) {
        return Err(Error::Usage {
            message: "transaction record branch is invalid",
        });
    }
    if !is_oid(commit) {
        return Err(Error::Usage {
            message: "transaction record commit is malformed",
        });
    }
    if !dot.starts_with(b"/")
        || contains_sub(dot, b"//")
        || contains_sub(dot, b"/./")
        || contains_sub(dot, b"/../")
    {
        return Err(Error::Usage {
            message: "transaction record dot path is unsafe",
        });
    }
    if !is_oid(dot_revision) {
        return Err(Error::Usage {
            message: "transaction record revision is malformed",
        });
    }
    if !is_nonce(nonce) {
        return Err(Error::Usage {
            message: "transaction record nonce is malformed",
        });
    }
    let unbound = git_dev == b"-" && git_ino == b"-";
    let bound = is_digits(git_dev) && is_digits(git_ino);
    if !unbound && !bound {
        return Err(Error::Usage {
            message: "transaction record binds half a generation",
        });
    }
    let mut dot_backup = home_bytes.to_vec();
    dot_backup.extend_from_slice(b"/.dot-backup/");
    if backup != b"-" && !backup.starts_with(&dot_backup[..]) {
        return Err(Error::Usage {
            message: "transaction record backup escapes the backup root",
        });
    }
    Ok(TransactionRecord {
        phase: record_text(phase)?,
        origin: record_text(origin)?,
        identity: record_text(identity)?,
        branch: record_text(branch)?,
        commit: record_text(commit)?,
        git_dir: record_text(git_dir)?,
        worktree: record_text(worktree)?,
        backup: record_text(backup)?,
        dot: record_text(dot)?,
        dot_revision: record_text(dot_revision)?,
        nonce: record_text(nonce)?,
        git_dev: record_text(git_dev)?,
        git_ino: record_text(git_ino)?,
    })
}

/// `_dot_init_record_phase`: rewrite the journal at `record` with
/// `phase`, re-resolving every other field from the live run. A
/// one-line delegation like the shell; the caller carries the new
/// phase the way the shell's `DOT_INIT_PHASE` global does.
pub fn record_phase(
    record: &Path,
    phase: &str,
    fields: &RecordFields<'_>,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    write_record(record, phase, fields, cache)
}

/// True when `needle` occurs in `haystack`: the shell's
/// `[[ $dot == *...* ]]` infix gates, which have no slice
/// shorthand.
fn contains_sub(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// `_dot_init_parent_record`: validate the parent intent at
/// `$transaction/parent-intent.$hash` (the hash is
/// `git hash-object --stdin` over the raw relative bytes, via
/// the shared text digest) and report its five fields. The file
/// must be a real file holding one line (trailing newlines strip
/// like the shell's command substitution, an interior newline
/// fails) with at most six tab fields — a seventh empty field
/// from a trailing tab passes, mirroring the shell's `read` into
/// seven variables with an empty `extra`. The phase is `pending`
/// or `prepared`, the recorded parent is `relative`, the stage is
/// the transaction-derived
/// `$HOME/[$parent/].dot-init-parent.$nonce.$hash` path (home
/// bytes concatenate like the shell's `"$HOME/..."`, so a home
/// with a trailing slash keeps its doubled separator), and the
/// device/inode/mode fields are digits and octal digits once
/// prepared, `-` while pending.
pub fn parent_record(
    transaction: &Path,
    relative: &str,
    home: &Path,
    nonce: &str,
    source_root: &Path,
) -> Result<ParentRecord> {
    let hash = temp::file_text_digest(source_root, relative.as_bytes())?;
    let file = join_slash(transaction, &format!("parent-intent.{hash}"));
    if !is_real_file(&file) {
        return Err(Error::Usage {
            message: "parent intent is not a regular file",
        });
    }
    let mut bytes = std::fs::read(&file).map_err(|source| Error::Io {
        context: "read parent intent",
        source,
    })?;
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.contains(&b'\n') {
        return Err(Error::Usage {
            message: "parent intent spans multiple lines",
        });
    }
    let parts: Vec<&[u8]> = bytes.split(|byte| *byte == b'\t').collect();
    if parts.len() > 7 || (parts.len() == 7 && !parts[6].is_empty()) {
        return Err(Error::Usage {
            message: "parent intent has the wrong field count",
        });
    }
    let mut fields: [&[u8]; 6] = [b""; 6];
    for (slot, part) in parts.iter().take(6).enumerate() {
        fields[slot] = part;
    }
    let (phase, parent, stage_rel, dev, ino, mode) = (
        fields[0], fields[1], fields[2], fields[3], fields[4], fields[5],
    );
    if !matches!(phase, b"pending" | b"prepared") {
        return Err(Error::Usage {
            message: "parent intent has an unknown phase",
        });
    }
    if parent != relative.as_bytes() {
        return Err(Error::Usage {
            message: "parent intent is for another parent",
        });
    }
    let mut expected = home.as_os_str().as_bytes().to_vec();
    if let Some(mark) = relative.rfind('/') {
        expected.push(b'/');
        expected.extend_from_slice(&relative.as_bytes()[..mark]);
    }
    // The shell strips exactly one trailing separator
    // (`${expected%/}`), so a home with several keeps the rest —
    // a loop here would collapse them and diverge.
    if expected.last() == Some(&b'/') {
        expected.pop();
    }
    expected.extend_from_slice(b"/.dot-init-parent.");
    expected.extend_from_slice(nonce.as_bytes());
    expected.push(b'.');
    expected.extend_from_slice(hash.as_bytes());
    let mut prefix = home.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    let stripped: &[u8] = match expected.strip_prefix(&prefix[..]) {
        Some(rest) => rest,
        None => &expected[..],
    };
    if stage_rel != stripped {
        return Err(Error::Usage {
            message: "parent intent binds another stage",
        });
    }
    if phase == b"prepared" {
        if !is_digits(dev) || !is_digits(ino) || !is_octal(mode) {
            return Err(Error::Usage {
                message: "parent intent binds the wrong generation",
            });
        }
    } else if dev != b"-" || ino != b"-" || mode != b"-" {
        return Err(Error::Usage {
            message: "parent intent binds the wrong generation",
        });
    }
    let text = |field: &[u8]| {
        String::from_utf8(field.to_vec()).map_err(|_| Error::Usage {
            message: "parent intent is not UTF-8",
        })
    };
    Ok(ParentRecord {
        phase: text(phase)?,
        stage: text(stage_rel)?,
        dev: text(dev)?,
        ino: text(ino)?,
        mode: text(mode)?,
    })
}

/// `_dot_init_prior_record`: report the first snapshot line in
/// `prior` whose path is `wanted`. Fields split on tabs with the
/// value keeping the rest of the line verbatim (later tabs
/// included), like the shell's `read` into seven variables with
/// no `extra` gate; short lines pad with empty fields the same
/// way. No match fails, like the shell falling off the loop.
///
/// The shell loops on a bare `read` (no `|| [[ -n ... ]]`
/// fallback, unlike the journal reader), so a final line without
/// its trailing newline never runs the loop body: it is
/// invisible to the lookup, and dropped here too.
pub fn prior_record(prior: &Path, wanted: &str) -> Result<PriorEntry> {
    let bytes = std::fs::read(prior).map_err(|source| Error::Io {
        context: "read prior snapshot",
        source,
    })?;
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    // Splitting always yields a trailing element: empty past a
    // final newline (the phantom line `read` never reports), or
    // the unterminated tail `read` discards. Either way it goes.
    let _ = lines.pop();
    for line in lines {
        let parts: Vec<&[u8]> = line.splitn(7, |byte| *byte == b'\t').collect();
        if parts[0] != wanted.as_bytes() {
            continue;
        }
        let mut fields: [&[u8]; 7] = [b""; 7];
        for (slot, part) in parts.iter().take(7).enumerate() {
            fields[slot] = part;
        }
        let text = |field: &[u8]| {
            String::from_utf8(field.to_vec()).map_err(|_| Error::Usage {
                message: "prior snapshot is not UTF-8",
            })
        };
        return Ok(PriorEntry {
            kind: text(fields[1])?,
            dev: text(fields[2])?,
            ino: text(fields[3])?,
            mode: text(fields[4])?,
            size: text(fields[5])?,
            value: text(fields[6])?,
        });
    }
    Err(Error::Usage {
        message: "prior snapshot has no such path",
    })
}
