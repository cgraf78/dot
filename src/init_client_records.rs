//! Transaction records, candidate trees, and path snapshots for
//! `lib/dot/init-client.sh`.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the seven record/tree/snapshot primitives
//! from `_dot_init_symlink_blob_safe` through
//! `_dot_init_path_state_matches`: the symlink-blob byte gate, the
//! transaction-record publisher and validator, the `ls-tree` journal
//! builder with its worktree gate, and the two worktree-state
//! snapshot helpers. The file-generic `_dot_init_error` diagnostic
//! stays unported (a bare `printf ... >&2; return 1` with no family
//! state, absorbed into [`Result`] the way earlier slices absorb
//! engine diagnostics). The transaction-directory lifecycle lives on
//! `rust-port-slice-35` (`init_client_transaction`), the host-git
//! identity family on `rust-port-slice-41` (`init_client_identity`),
//! the git-generation binding on `rust-port-slice-43`
//! (`init_client_generation`), the per-entry staging family on
//! `rust-port-slice-46` (`init_client_entry`), and the plan,
//! publish, delete, and rollback families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the client root from `HOME`,
//! the run nonce and git binding from `DOT_INIT_*` globals, the
//! install roots from `SHDEPS_*` globals, and the source checkout
//! from `DOT_SOURCE_ROOT`. Library code must not mutate the process
//! environment behind the engine, so those cross here as explicit
//! parameters. `REPLY`-carried outputs return their values instead:
//! the record fields arrive as [`InitRecord`], the snapshot line as
//! [`PathSnapshot`].

use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use crate::errors::{Error, Result};
use crate::reserved;
use crate::temp;

/// First line of a transaction record: proves the file is ours
/// before any field is trusted. Both engines reject any other
/// header.
pub const RECORD_HEADER: &str = "cgraf78 dot initialization transaction v1";

/// Largest record the validator accepts, in bytes: the shell's
/// `wc -c` gate (`-le 16384`).
pub const MAX_RECORD_BYTES: u64 = 16384;

/// Largest candidate tree the journal builder accepts: the shell's
/// `count -le 100000` gate.
pub const MAX_TREE_ENTRIES: usize = 100_000;

/// Largest symlink blob the byte gate accepts, in bytes: the
/// shell's `size -gt 4096` rejection.
pub const MAX_SYMLINK_BLOB_BYTES: u64 = 4096;

/// A validated transaction record: the shell's `DOT_INIT_*` globals
/// from [`read_record`] as owned strings. Every value already passed
/// the shell's `_dot_init_safe_value` screening (nonempty, no
/// tab/newline/carriage-return), so consumers can embed the fields
/// without rechecking.
pub struct InitRecord {
    /// Lifecycle phase (`prepared`, `backing-up`, …).
    pub phase: String,
    /// Repository URL the client initializes from.
    pub origin: String,
    /// Pinned repository identity.
    pub identity: String,
    /// Branch to converge.
    pub branch: String,
    /// Commit to publish (40- or 64-hex).
    pub commit: String,
    /// Client git directory (`$HOME/.dotfiles` or `$HOME/.git`).
    pub git_dir: String,
    /// Client worktree (exactly `$HOME`).
    pub worktree: String,
    /// Backup directory (`-` for none).
    pub backup: String,
    /// The `dot` binary under test.
    pub dot: String,
    /// Source revision that wrote the record (40- or 64-hex).
    pub dot_revision: String,
    /// Run nonce binding every stage path.
    pub nonce: String,
    /// Publishing git directory device (`-` when unbound).
    pub git_dev: String,
    /// Publishing git directory inode (`-` when unbound).
    pub git_ino: String,
}

/// Environment [`write_record`] publishes around its explicit
/// arguments: the shell's `$HOME`, `$DOT_BIN`, `$DOT_SOURCE_ROOT`,
/// and the `DOT_INIT_*` run globals with the shell's own defaults
/// (`commit` falls back to forty zeros, `nonce` to `legacy`, the git
/// device/inode pair to `-`/`-`).
pub struct WriteRecordInputs<'a> {
    /// Client `$HOME`.
    pub home: &'a str,
    /// The `dot` binary path (`$DOT_BIN`).
    pub dot_bin: &'a str,
    /// `$DOT_INIT_COMMIT` override.
    pub commit: Option<&'a str>,
    /// `$DOT_INIT_NONCE` override.
    pub nonce: Option<&'a str>,
    /// `$DOT_INIT_GIT_DEV` override.
    pub git_dev: Option<&'a str>,
    /// `$DOT_INIT_GIT_INO` override.
    pub git_ino: Option<&'a str>,
    /// Checkout the `dot_revision` probe runs under
    /// (`$DOT_SOURCE_ROOT`).
    pub source_root: &'a Path,
}

/// Environment [`candidate_tree`] resolves the reserved-roots
/// inventory from: the shell's `$HOME`, `$XDG_STATE_HOME`,
/// `$SHDEPS_INSTALL_DIR`, `$SHDEPS_STATE_DIR`, `$DOT_INIT_BACKUP`,
/// the `OVERLAYS` link paths, the working directory for physical
/// mapping, and `$DOT_SOURCE_ROOT` for the launcher exception.
pub struct CandidateTreeInputs<'a> {
    /// Repository listing the candidate (`git -C` target).
    pub repo: &'a Path,
    /// Branch to inventory.
    pub branch: &'a str,
    /// Journal destination (`tree.tsv`).
    pub output: &'a Path,
    /// Client `$HOME`.
    pub home: &'a str,
    /// Raw `$XDG_STATE_HOME` (empty counts as unset, like the shell).
    pub xdg_state_home: &'a str,
    /// `$SHDEPS_INSTALL_DIR` override.
    pub install_dir: Option<&'a str>,
    /// `$SHDEPS_STATE_DIR` override.
    pub state_dir: Option<&'a str>,
    /// `OVERLAYS` link paths.
    pub overlay_paths: &'a [String],
    /// `$DOT_INIT_BACKUP` override (`-`/empty stays excluded).
    pub init_backup: Option<&'a str>,
    /// Working directory for physical root mapping.
    pub pwd: &'a str,
    /// Checkout holding `support/client-launcher.sh`.
    pub source_root: &'a Path,
}

/// One `_dot_init_snapshot_path` line: the six tab fields describing
/// live worktree state. An absent path reports `kind` `absent` with
/// every other field `-`, exactly like the shell.
pub struct PathSnapshot {
    /// `absent`, `regular`, `symlink`, or `directory`.
    pub kind: String,
    /// Device decimal (`-` while absent).
    pub dev: String,
    /// Inode decimal (`-` while absent).
    pub ino: String,
    /// `stat %a` octal (`-` while absent).
    pub mode: String,
    /// `stat %s` bytes (`-` while absent).
    pub size: String,
    /// Content binding: blob hash, link target, `-`, or `-`.
    pub value: String,
}

impl PathSnapshot {
    /// Render the six fields as the shell's tab-separated line
    /// (without a trailing newline: the shell's `printf` adds it at
    /// the call site).
    pub fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.kind, self.dev, self.ino, self.mode, self.size, self.value
        )
    }
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`. `symlink_metadata` never follows,
/// so a link reports its own type and fails the gate on both
/// engines.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Effective-uid ownership (`test -O`): several shell gates require
/// the record to be ours. An unreadable identity fails closed, like
/// the shell's failed `stat`.
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// `_dot_init_safe_value`: nonempty with no tab, newline, or
/// carriage-return bytes. Operates on bytes like the shell's glob
/// match, so it also screens values `str` cannot hold.
fn is_safe_value(value: &[u8]) -> bool {
    !value.is_empty()
        && !value
            .iter()
            .any(|byte| matches!(byte, b'\t' | b'\n' | b'\r'))
}

/// True for a nonempty all-ASCII-hex field of exactly `len` bytes:
/// the shell's `[[ $value =~ ^[0-9a-fA-F]{40}$ ]]` shape.
fn is_hex(value: &[u8], len: usize) -> bool {
    value.len() == len && value.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// True for a 40- or 64-hex digest: the shell's
/// `^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$` alternation. Both branches
/// carry anchors, so the substring-matching `=~` still demands a
/// full-string match.
fn is_commit(value: &[u8]) -> bool {
    is_hex(value, 40) || is_hex(value, 64)
}

/// True for a 40-to-64-hex digest: the candidate tree's
/// `^[0-9a-fA-F]{40,64}$` interval, which (unlike [`is_commit`])
/// also accepts the odd lengths between the two object formats.
fn is_tree_oid(value: &[u8]) -> bool {
    (40..=64).contains(&value.len()) && value.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// True for a nonempty all-ASCII-digit field: the shell's
/// `[[ $value =~ ^[0-9]+$ ]]`.
fn is_digits(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(|byte| byte.is_ascii_digit())
}

/// True for a nonce: the shell's `^[A-Za-z0-9._-]+$`.
fn is_nonce(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().all(
            |byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'),
        )
}

/// Run bare `git -C dir` with `LC_ALL=C` pinned and stderr nulled,
/// capturing stdout. The shell runs these inventory probes as bare
/// `git` (only the source-checkout probe sanitizes); the locale pin
/// keeps diagnostics English on both engines. A spawn failure is
/// [`Error::Io`]; callers grade a nonzero status themselves, exactly
/// like the shell's `if ! git ...` arms.
fn git_capture(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<std::process::Output> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C");
    cmd.arg(dir);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env("LC_ALL", "C");
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|source| Error::Io {
        context: "spawn git",
        source,
    })?;
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .ok_or(Error::Usage {
                message: "git stdin unavailable",
            })?
            .write_all(input)
            .map_err(|source| Error::Io {
                context: "feed git stdin",
                source,
            })?;
    }
    child.wait_with_output().map_err(|source| Error::Io {
        context: "wait git",
        source,
    })
}

/// Read one blob's bytes: `git -C repo show branch:path`. A failure
/// (missing git, bad ref, bad path) yields EMPTY bytes, exactly like
/// the shell's `git ... show ... | consumer` pipe feeding nothing:
/// every caller grades those bytes through the same content gates,
/// so an empty blob and a failed read share one verdict without a
/// special case.
fn git_show_bytes(repo: &Path, branch: &str, path: &str) -> Vec<u8> {
    let spec = format!("{branch}:{path}");
    match git_capture(repo, &["show", spec.as_str()], None) {
        Ok(output) if output.status.success() => output.stdout,
        _ => Vec::new(),
    }
}

/// `_dot_init_branch_valid`: nonempty with `git check-ref-format
/// --branch` accepting it. Both engines resolve `git` off `PATH`,
/// so fixtures share one verdict.
fn branch_valid(branch: &[u8]) -> bool {
    if branch.is_empty() {
        return false;
    }
    let text = String::from_utf8_lossy(branch).into_owned();
    let mut cmd = std::process::Command::new("git");
    cmd.arg("check-ref-format");
    cmd.arg("--branch");
    cmd.arg(&text);
    cmd.env("LC_ALL", "C");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.status().is_ok_and(|status| status.success())
}

/// `_dot_source_git rev-parse HEAD`: the source revision
/// [`write_record`] embeds. Mirrors the shell's sanitized probe
/// (`_dot_sanitized_git -c safe.directory=... -C ...`): the `GIT_*`
/// overrides drop out, system/global config stays off, and only the
/// already-selected checkout is trusted. Command substitution
/// strips trailing newlines, hence the trim.
fn source_revision(source_root: &Path) -> Result<String> {
    const UNSET: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_INDEX_FILE",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_DEFAULT_HASH",
    ];
    let mut cmd = std::process::Command::new("git");
    for var in UNSET {
        cmd.env_remove(var);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("LC_ALL", "C");
    let mut safe = b"safe.directory=".to_vec();
    safe.extend_from_slice(source_root.as_os_str().as_bytes());
    cmd.arg("-c");
    cmd.arg(std::ffi::OsStr::from_bytes(&safe));
    cmd.arg("-C");
    cmd.arg(source_root);
    cmd.arg("rev-parse");
    cmd.arg("HEAD");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    let child = cmd.spawn().map_err(|source| Error::Io {
        context: "spawn git rev-parse",
        source,
    })?;
    let output = child.wait_with_output().map_err(|source| Error::Io {
        context: "wait git rev-parse",
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "git rev-parse HEAD".to_string(),
            status: Some(output.status.to_string()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string())
}

/// `_dot_init_symlink_blob_safe`: a symlink blob is publishable only
/// when it is short and carries no byte the TSV journals cannot
/// represent. The shell reads the blob through a temp file without
/// command substitution (variables cannot retain NUL) and rejects
/// NUL/tab/newline/carriage-return via `od` plus `awk`; holding the
/// `git show` bytes in memory reaches the same verdict without the
/// scratch file, which neither engine's callers observe.
pub fn symlink_blob_safe(repo: &Path, branch: &str, path: &str) -> bool {
    const FORBIDDEN: &[u8] = &[0x00, 0x09, 0x0a, 0x0d];
    let bytes = git_show_bytes(repo, branch, path);
    if bytes.is_empty() || bytes.len() as u64 > MAX_SYMLINK_BLOB_BYTES {
        return false;
    }
    !bytes.iter().any(|byte| FORBIDDEN.contains(byte))
}

/// `_dot_init_write_record`: publish the fourteen-line transaction
/// record at `destination` at mode 600. An existing destination is
/// replaced without touching a late directory; otherwise the
/// destination must be absent, like the shell's
/// `_dot_move_replace_nodir` / `_dot_move_noreplace` split on the
/// existence probe. `git_dir` defaults to `$HOME/.dotfiles`, and the
/// remaining run bindings arrive via `inputs` with the shell's own
/// fallbacks.
///
/// Like the shell, a body-write failure removes the sibling temp
/// before returning (nothing later in the family reads those names,
/// so the shapes stay comparable); a later failure (chmod, or a live
/// destination winning the race) leaves the sibling behind.
#[allow(clippy::too_many_arguments)]
pub fn write_record(
    destination: &Path,
    phase: &str,
    origin: &str,
    identity: &str,
    branch: &str,
    backup: &str,
    git_dir: Option<&str>,
    inputs: &WriteRecordInputs<'_>,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    const ZERO_COMMIT: &str = "0000000000000000000000000000000000000000";
    // The revision probe runs before the sibling exists, like the
    // shell probing before `_dot_sibling_tmp_for`: a failing probe
    // leaves no residue on either engine.
    let dot_revision = source_revision(inputs.source_root)?;
    let git_default = format!("{}/.dotfiles", inputs.home);
    let git_dir = git_dir.unwrap_or(git_default.as_str());
    let mut body = String::from(RECORD_HEADER);
    body.push('\n');
    for (key, value) in [
        ("phase", phase),
        ("origin", origin),
        ("identity", identity),
        ("branch", branch),
        ("commit", inputs.commit.unwrap_or(ZERO_COMMIT)),
        ("git_dir", git_dir),
        ("worktree", inputs.home),
        ("backup", backup),
        ("dot", inputs.dot_bin),
        ("dot_revision", dot_revision.as_str()),
        ("nonce", inputs.nonce.unwrap_or("legacy")),
        ("git_dev", inputs.git_dev.unwrap_or("-")),
        ("git_ino", inputs.git_ino.unwrap_or("-")),
    ] {
        body.push_str(key);
        body.push('=');
        body.push_str(value);
        body.push('\n');
    }
    let temporary = temp::sibling_tmp_for(destination)?;
    if let Err(source) = std::fs::write(&temporary, body.as_bytes()) {
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
    if std::fs::symlink_metadata(destination).is_ok() {
        temp::move_replace_nodir_cached(&temporary, destination, cache)
    } else {
        temp::move_noreplace_cached(&temporary, destination, cache)
    }
}

/// `_dot_init_read_record`: validate the record at `record` and
/// report its thirteen fields as [`InitRecord`] (the shell exports
/// them as `DOT_INIT_*` globals). The record must be a real file we
/// own with no group/other permission bits, at most
/// [`MAX_RECORD_BYTES`] bytes, holding exactly fourteen lines: the
/// [`RECORD_HEADER`] plus thirteen unique known `key=value` pairs
/// with safe values. The cross-field gates then pin the phase
/// vocabulary, the `$HOME`-bound git directory and worktree, the
/// digest shapes, the `dot` path shape, the nonce charset, the paired
/// git device/inode fields, and the backup location.
///
/// NUL bytes never reach the parser: the shell's `read` drops them
/// silently (probed: a `phase=prepa<NUL>red` line validates as
/// `prepared`), so the port strips them after the size gate, which
/// measures the raw file like the shell's `wc -c`. Values must be
/// UTF-8 to enter the returned struct; the shell carries raw bytes,
/// so a non-UTF8 value that clears every gate is the one corner
/// where the port fails closed and the shell does not.
pub fn read_record(record: &Path, home: &str) -> Result<InitRecord> {
    const KEYS: [&[u8]; 13] = [
        b"phase",
        b"origin",
        b"identity",
        b"branch",
        b"commit",
        b"git_dir",
        b"worktree",
        b"backup",
        b"dot",
        b"dot_revision",
        b"nonce",
        b"git_dev",
        b"git_ino",
    ];
    if !is_real_file(record) {
        return Err(Error::Usage {
            message: "transaction record is not a regular file",
        });
    }
    if !owned_by_us(record) {
        return Err(Error::Usage {
            message: "transaction record is not ours",
        });
    }
    let mode = temp::file_mode(record)?;
    if mode & 0o77 != 0 {
        return Err(Error::Usage {
            message: "transaction record is not private",
        });
    }
    let mut body = std::fs::read(record).map_err(|source| Error::Io {
        context: "read transaction record",
        source,
    })?;
    if body.len() as u64 > MAX_RECORD_BYTES {
        return Err(Error::Usage {
            message: "transaction record is too large",
        });
    }
    body.retain(|byte| *byte != 0);
    let mut lines: Vec<&[u8]> = body.split(|byte| *byte == b'\n').collect();
    if body.last() == Some(&b'\n') {
        lines.pop();
    }
    if lines.len() != 14 {
        return Err(Error::Usage {
            message: "transaction record has the wrong line count",
        });
    }
    if lines[0] != RECORD_HEADER.as_bytes() {
        return Err(Error::Usage {
            message: "transaction record has a bad header",
        });
    }
    use std::collections::HashMap;
    let mut fields: HashMap<&[u8], &[u8]> = HashMap::new();
    for line in &lines[1..] {
        let Some(eq) = line.iter().position(|byte| *byte == b'=') else {
            return Err(Error::Usage {
                message: "transaction record line has no key",
            });
        };
        let (key, rest) = line.split_at(eq);
        let value = &rest[1..];
        if !is_safe_value(value) {
            return Err(Error::Usage {
                message: "transaction record value is unsafe",
            });
        }
        if !KEYS.contains(&key) {
            return Err(Error::Usage {
                message: "transaction record has an unknown key",
            });
        }
        if fields.insert(key, value).is_some() {
            return Err(Error::Usage {
                message: "transaction record repeats a key",
            });
        }
    }
    let field = |key: &[u8]| {
        fields.get(key).copied().ok_or(Error::Usage {
            message: "transaction record misses a key",
        })
    };
    let (phase, origin, identity, branch) = (
        field(b"phase")?,
        field(b"origin")?,
        field(b"identity")?,
        field(b"branch")?,
    );
    let (commit, git_dir, worktree, backup) = (
        field(b"commit")?,
        field(b"git_dir")?,
        field(b"worktree")?,
        field(b"backup")?,
    );
    let (dot, dot_revision, nonce) = (field(b"dot")?, field(b"dot_revision")?, field(b"nonce")?);
    let (git_dev, git_ino) = (field(b"git_dev")?, field(b"git_ino")?);
    const PHASES: [&[u8]; 9] = [
        b"prepared",
        b"backing-up",
        b"backed-up",
        b"git-staging",
        b"git-staged",
        b"publishing",
        b"checkout",
        b"converging",
        b"complete",
    ];
    if !PHASES.contains(&phase) {
        return Err(Error::Usage {
            message: "transaction record has an unknown phase",
        });
    }
    let home_bytes = home.as_bytes();
    let mut dotfiles = home_bytes.to_vec();
    dotfiles.extend_from_slice(b"/.dotfiles");
    let mut git_home = home_bytes.to_vec();
    git_home.extend_from_slice(b"/.git");
    if git_dir != dotfiles.as_slice() && git_dir != git_home.as_slice() {
        return Err(Error::Usage {
            message: "transaction record binds a foreign git directory",
        });
    }
    if worktree != home_bytes {
        return Err(Error::Usage {
            message: "transaction record binds a foreign worktree",
        });
    }
    if !branch_valid(branch) {
        return Err(Error::Usage {
            message: "transaction record branch is invalid",
        });
    }
    if !is_commit(commit) {
        return Err(Error::Usage {
            message: "transaction record commit is invalid",
        });
    }
    if !dot.starts_with(b"/")
        || dot.windows(2).any(|pair| pair == b"//")
        || dot.windows(3).any(|trio| trio == b"/./")
        || dot.windows(4).any(|quad| quad == b"/../")
    {
        return Err(Error::Usage {
            message: "transaction record dot path is invalid",
        });
    }
    if !is_commit(dot_revision) {
        return Err(Error::Usage {
            message: "transaction record revision is invalid",
        });
    }
    if !is_nonce(nonce) {
        return Err(Error::Usage {
            message: "transaction record nonce is invalid",
        });
    }
    let unbound = git_dev == b"-" && git_ino == b"-";
    let bound = is_digits(git_dev) && is_digits(git_ino);
    if !unbound && !bound {
        return Err(Error::Usage {
            message: "transaction record git identity is invalid",
        });
    }
    if backup != b"-" {
        let mut prefix = home_bytes.to_vec();
        prefix.extend_from_slice(b"/.dot-backup/");
        if !backup.starts_with(prefix.as_slice()) {
            return Err(Error::Usage {
                message: "transaction record backup is invalid",
            });
        }
    }
    let text = |value: &[u8]| {
        String::from_utf8(value.to_vec()).map_err(|_| Error::Usage {
            message: "transaction record value is not UTF-8",
        })
    };
    Ok(InitRecord {
        phase: text(phase)?,
        origin: text(origin)?,
        identity: text(identity)?,
        branch: text(branch)?,
        commit: text(commit)?,
        git_dir: text(git_dir)?,
        worktree: text(worktree)?,
        backup: text(backup)?,
        dot: text(dot)?,
        dot_revision: text(dot_revision)?,
        nonce: text(nonce)?,
        git_dev: text(git_dev)?,
        git_ino: text(git_ino)?,
    })
}

/// `${SHDEPS_INSTALL_DIR:-$HOME/.local/share}`: an empty override
/// counts as unset, like the shell's `:-`.
fn install_root(inputs: &CandidateTreeInputs<'_>) -> String {
    match inputs.install_dir {
        Some(dir) if !dir.is_empty() => dir.to_string(),
        _ => format!("{}/.local/share", inputs.home),
    }
}

/// The reserved-roots inventory behind [`candidate_tree`]: `None`
/// reads reserved (fail closed), exactly like the shell's
/// `_dot_reserved_roots_snapshot || return 0`.
fn tree_roots(inputs: &CandidateTreeInputs<'_>) -> Option<Vec<String>> {
    let state_home =
        crate::xdg::base(crate::xdg::Kind::State, inputs.xdg_state_home, inputs.home).ok()?;
    let install_root = install_root(inputs);
    let provider_state = match inputs.state_dir {
        Some(dir) if !dir.is_empty() => dir.to_string(),
        _ => format!("{state_home}/shdeps"),
    };
    let init_backup = match inputs.init_backup {
        Some(backup) if !backup.is_empty() && backup != "-" => Some(backup.to_string()),
        _ => None,
    };
    reserved::reserved_roots(
        &reserved::RootsInput {
            home: inputs.home.to_string(),
            state_home,
            install_root,
            provider_state,
            overlay_paths: inputs.overlay_paths.to_vec(),
            init_backup,
        },
        inputs.pwd,
    )
    .ok()
}

/// Whether the candidate target is reserved: the shell's
/// `dot_candidate_path_is_reserved`, whose absolute-path arity gate
/// reports non-absolute targets as NOT reserved (`return 2`) and
/// whose snapshot failure reports reserved (fail closed). `roots` is
/// the `tree_roots` inventory; `checkout` is the client checkout
/// the install-sibling check derives from, like the shell's
/// `${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot`.
fn tree_target_reserved(
    target: &str,
    roots: &Option<Vec<String>>,
    inputs: &CandidateTreeInputs<'_>,
) -> bool {
    if !target.starts_with('/') {
        return false;
    }
    let roots = match roots {
        Some(roots) => roots,
        None => return true,
    };
    let checkout = format!("{}/cgraf78/dot", install_root(inputs));
    reserved::candidate_path_is_reserved_from_roots(
        target,
        roots,
        inputs.home,
        checkout.as_str(),
        inputs.pwd,
    )
}

/// Validate one `ls-tree -z` record for [`candidate_tree`], returning
/// its journal line (`mode\toid\tpath\n`). The header splits on
/// IFS whitespace like the shell's `read`, so a fourth word lands in
/// the oid and fails its digest gate; the path keeps its raw bytes
/// up to the tab, and non-UTF8 paths fail (the reserved check below
/// needs `str`, the codebase's usual lossy-conversion boundary).
fn candidate_entry(
    inputs: &CandidateTreeInputs<'_>,
    entry: &[u8],
    roots: &Option<Vec<String>>,
) -> Option<Vec<u8>> {
    let tab = entry.iter().position(|byte| *byte == b'\t')?;
    let (header, rest) = entry.split_at(tab);
    let path = &rest[1..];
    let mut words = header
        .split(|byte| *byte == b' ' || *byte == b'\t' || *byte == b'\n')
        .filter(|word| !word.is_empty());
    let (mode, file_type, oid) = match (words.next(), words.next(), words.next()) {
        (Some(mode), Some(file_type), Some(oid)) if words.next().is_none() => {
            (mode, file_type, oid)
        }
        _ => return None,
    };
    if file_type != b"blob"
        || (mode != b"100644" && mode != b"100755" && mode != b"120000")
        || !is_tree_oid(oid)
    {
        return None;
    }
    let path_text = std::str::from_utf8(path).ok()?;
    if !crate::repos_overlays::init_safe_relative_path(path_text) {
        return None;
    }
    if mode == b"120000" && !symlink_blob_safe(inputs.repo, inputs.branch, path_text) {
        return None;
    }
    let mut target = inputs.home.as_bytes().to_vec();
    target.push(b'/');
    target.extend_from_slice(path);
    let target = String::from_utf8(target).ok()?;
    if tree_target_reserved(target.as_str(), roots, inputs) {
        // The one client-owned control-plane exception is the
        // generated regular command adapter: its exact bytes are
        // checked against this release, like the shell's launcher
        // comparison, so a repository cannot smuggle another
        // executable into the reserved front door.
        if path != b".local/bin/dot" || mode != b"100755" {
            return None;
        }
        let shown = git_show_bytes(inputs.repo, inputs.branch, path_text);
        let launcher = inputs.source_root.join("support/client-launcher.sh");
        if !temp::stdin_matches_file(inputs.source_root, &shown, &launcher).unwrap_or(false) {
            return None;
        }
    }
    let mut line = Vec::with_capacity(mode.len() + oid.len() + path.len() + 3);
    line.extend_from_slice(mode);
    line.push(b'\t');
    line.extend_from_slice(oid);
    line.push(b'\t');
    line.extend_from_slice(path);
    line.push(b'\n');
    Some(line)
}

/// `_dot_init_candidate_tree`: inventory `branch` of `repo` into the
/// `output` journal (`mode\toid\tpath` per line). Every record
/// must be a safe relative blob path with a known mode and digest;
/// symlink blobs pass [`symlink_blob_safe`], and reserved targets
/// pass only through the release-byte launcher exception. The
/// journal truncates first like the shell's leading `: >`, holds the
/// accepted lines, and truncates again on any rejection — including
/// an empty tree and anything past [`MAX_TREE_ENTRIES`] entries — so
/// a failed run always leaves an empty journal behind.
///
/// The shell stages the `ls-tree` bytes through a cleanup-tracked
/// temp file; the port holds them in memory instead. That scratch
/// file is always removed on both engines (its removal failing after
/// a valid tree is the one unobservable corner this port does not
/// reproduce).
pub fn candidate_tree(inputs: &CandidateTreeInputs<'_>) -> Result<()> {
    let truncate = || {
        let _ = std::fs::File::create(inputs.output);
    };
    if let Err(source) = std::fs::File::create(inputs.output) {
        return Err(Error::Io {
            context: "create candidate tree",
            source,
        });
    }
    let output = git_capture(
        inputs.repo,
        &["ls-tree", "-rz", "--full-tree", inputs.branch],
        None,
    )?;
    if !output.status.success() {
        truncate();
        return Err(Error::Usage {
            message: "git ls-tree failed",
        });
    }
    let mut raw = output.stdout;
    if raw.last() == Some(&0) {
        raw.pop();
    }
    let roots = tree_roots(inputs);
    let mut journal: Vec<u8> = Vec::new();
    let mut count = 0usize;
    let mut valid = true;
    for entry in raw.split(|byte| *byte == 0) {
        match candidate_entry(inputs, entry, &roots) {
            Some(line) => {
                journal.extend_from_slice(&line);
                count += 1;
                if count > MAX_TREE_ENTRIES {
                    valid = false;
                    break;
                }
            }
            None => {
                valid = false;
                break;
            }
        }
    }
    if !valid || count == 0 {
        truncate();
        return Err(Error::Usage {
            message: "invalid candidate tree",
        });
    }
    if std::fs::write(inputs.output, &journal).is_err() {
        truncate();
        return Err(Error::Usage {
            message: "invalid candidate tree",
        });
    }
    Ok(())
}

/// Join `$HOME` and a tree-relative path with a plain `/`
/// separator, like the shell's `"$HOME/$path"`: a `home` with a
/// trailing slash keeps its doubled separator instead of being
/// normalized away.
fn home_join(home: &str, path: &str) -> Vec<u8> {
    let mut out = home.as_bytes().to_vec();
    out.push(b'/');
    out.extend_from_slice(path.as_bytes());
    out
}

/// `_dot_init_candidate_matches_path`: the worktree target still
/// carries the candidate's bytes and shape. Symlinks compare their
/// (newline-stripped, like the shell's command substitution) target
/// against the blob; regular files compare content through
/// [`temp::stdin_matches_file`] and then prove their execute bits
/// match the mode. Anything else — including a missing target —
/// fails.
pub fn candidate_matches_path(
    repo: &Path,
    branch: &str,
    mode: &str,
    path: &str,
    home: &str,
    source_root: &Path,
) -> bool {
    let raw = home_join(home, path);
    let target = Path::new(std::ffi::OsStr::from_bytes(&raw));
    if std::fs::symlink_metadata(target).is_err() {
        return false;
    }
    let shown = git_show_bytes(repo, branch, path);
    if mode == "120000" {
        if !std::fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return false;
        }
        let mut link = match std::fs::read_link(target) {
            Ok(link) => link.as_os_str().as_bytes().to_vec(),
            Err(_) => Vec::new(),
        };
        link.retain(|byte| *byte != 0);
        while link.last() == Some(&b'\n') {
            link.pop();
        }
        return shown == link;
    }
    if mode != "100644" && mode != "100755" {
        return false;
    }
    if !is_real_file(target) {
        return false;
    }
    if !temp::stdin_matches_file(source_root, &shown, target).unwrap_or(false) {
        return false;
    }
    let mode_bits = match temp::file_mode(target) {
        Ok(mode_bits) => mode_bits,
        Err(_) => return false,
    };
    if mode == "100755" {
        mode_bits & 0o111 != 0
    } else {
        mode_bits & 0o111 == 0
    }
}

/// The absent snapshot: the shell's `absent\t-\t-\t-\t-\t-`.
fn absent_snapshot() -> PathSnapshot {
    PathSnapshot {
        kind: "absent".to_string(),
        dev: "-".to_string(),
        ino: "-".to_string(),
        mode: "-".to_string(),
        size: "-".to_string(),
        value: "-".to_string(),
    }
}

/// `_dot_init_snapshot_path`: describe live worktree state as a
/// [`PathSnapshot`]. A missing path reports the absent snapshot;
/// anything present binds its device, inode, `stat %a` mode, and
/// `stat %s` size. Plain `stat` never follows the final component
/// (only `-L` does), so a symlink — dangling or not — reports the
/// link itself, exactly like `symlink_metadata` does here. The value
/// is kind-shaped: the filter-free blob hash for regular files, the
/// newline-stripped link target for symlinks (command substitution
/// strips trailing newlines, and drops NULs like `read_record`
/// observed), and `-` for directories. Any other file type fails
/// outright.
pub fn snapshot_path(path: &Path, source_root: &Path) -> Result<PathSnapshot> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return Ok(absent_snapshot()),
    };
    let file_type = meta.file_type();
    let (dev, ino) = (meta.dev(), meta.ino());
    let mode = format!("{:o}", meta.mode() & 0o7777);
    let size = meta.len().to_string();
    let value = if file_type.is_symlink() {
        let link = std::fs::read_link(path).map_err(|source| Error::Io {
            context: "read snapshot link",
            source,
        })?;
        let mut raw = link.as_os_str().as_bytes().to_vec();
        raw.retain(|byte| *byte != 0);
        while raw.last() == Some(&b'\n') {
            raw.pop();
        }
        let text = String::from_utf8(raw).map_err(|_| Error::Usage {
            message: "snapshot link is not UTF-8",
        })?;
        if !is_safe_value(text.as_bytes()) {
            return Err(Error::Usage {
                message: "snapshot link is unsafe",
            });
        }
        text
    } else if file_type.is_file() {
        let digest = temp::file_digest(source_root, path)?;
        if !is_commit(digest.as_bytes()) {
            return Err(Error::Usage {
                message: "snapshot digest is invalid",
            });
        }
        digest
    } else if file_type.is_dir() {
        "-".to_string()
    } else {
        return Err(Error::Usage {
            message: "snapshot kind is unsupported",
        });
    };
    Ok(PathSnapshot {
        kind: if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_file() {
            "regular"
        } else {
            "directory"
        }
        .to_string(),
        dev: dev.to_string(),
        ino: ino.to_string(),
        mode,
        size,
        value,
    })
}

/// `_dot_init_path_state_matches`: the path still shows the
/// `expected` snapshot — same type, same device/inode, same mode and
/// size strings, and a re-read value (blob hash, link target, or
/// nothing for directories). Identity, mode, and size read the link
/// itself for symlinks (plain `stat` never follows the final
/// component). An `absent` expectation matches only a missing path.
/// Anything unreadable fails, like the shell's short-circuit
/// `|| return 1` arms.
pub fn path_state_matches(path: &Path, expected: &PathSnapshot, source_root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    if expected.kind == "absent" {
        return std::fs::symlink_metadata(path).is_err();
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let file_type = meta.file_type();
    let kind_ok = match expected.kind.as_str() {
        "regular" => !file_type.is_symlink() && file_type.is_file(),
        "symlink" => file_type.is_symlink(),
        "directory" => !file_type.is_symlink() && file_type.is_dir(),
        _ => false,
    };
    if !kind_ok {
        return false;
    }
    if temp::identity_string((meta.dev(), meta.ino()))
        != format!("{}:{}", expected.dev, expected.ino)
    {
        return false;
    }
    if format!("{:o}", meta.mode() & 0o7777) != expected.mode {
        return false;
    }
    if meta.len().to_string() != expected.size {
        return false;
    }
    if expected.kind == "regular" {
        match temp::file_digest(source_root, path) {
            Ok(digest) => digest == expected.value,
            Err(_) => false,
        }
    } else if expected.kind == "symlink" {
        let mut link = match std::fs::read_link(path) {
            Ok(link) => link.as_os_str().as_bytes().to_vec(),
            Err(_) => return false,
        };
        link.retain(|byte| *byte != 0);
        while link.last() == Some(&b'\n') {
            link.pop();
        }
        link == expected.value.as_bytes()
    } else {
        true
    }
}
