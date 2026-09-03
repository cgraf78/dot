//! Crash-safe file transactions (`lib/dot/temp.sh`).
//!
//! Generation tokens bind a destination's identity (parent device and
//! inode plus a content digest) so a conditional replace/remove only
//! fires when the live file still matches what the caller staged
//! against; prepare/quarantine/commit phases with a journal record
//! make every crash window retryable or explicitly recoverable.
//! Content hashes come from the same `git hash-object` baseline the
//! shell uses (Git is already a required engine dependency), moves go
//! through the same `mv` binary with the same `-nT`/`-nh` capability
//! probe (GNU and BSD `mv` differ on late directories, and the probe
//! matrix is exactly what the shell suite pins), and `umask` is read
//! from the engine process the way the shell reads its own —
//! `std` offers no `umask(2)` binding, and the shell pays a fork per
//! read too. Callers thread [`LockCtx`] (the `DOT_TEST` /
//! `DOT_UPDATE_LOCK_TOKEN` gate), `source_root` (the
//! `DOT_SOURCE_ROOT` binding, see [`source_root`]), the umask, and a
//! [`MoveCache`] explicitly so differential tests can pin every knob
//! without process-global mutation.
//!
//! Unix-only, like the engine itself: device/inode identities,
//! permission bits, and the umask have no portable spelling, and the
//! shell this ports never ran anywhere else.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};

/// `REPLY` capacity the shell never exceeds: sibling temps carry the
/// destination basename plus `.tmp.` plus six random characters.
const TMP_SUFFIX_LEN: usize = 6;
/// Retries for a colliding sibling-temp name before giving up; the
/// counter fallback below makes even one collision unlikely.
const TMP_RETRIES: usize = 100;

/// True when `path` carries a byte the transaction layer rejects
/// outright: newline, carriage return, or tab. The shell tests
/// membership with `case` glob patterns; bytes are exact under
/// `LC_ALL=C`, and non-UTF8 names without these bytes stay usable.
fn has_control_char(path: &Path) -> bool {
    path.as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'\n' | b'\r' | b'\t'))
}

/// `_dot_sibling_tmp_for`: create `dir/base.tmp.XXXXXX` (empty, mode
/// 600) after `mkdir -p` on the parent, returning its path. The
/// six-character suffix is drawn from `/dev/urandom` over the mktemp
/// alphabet with a pid/counter fallback, and creation uses `O_EXCL`
/// (`create_new`) with a retry loop, so a guessed name can neither be
/// squatted nor followed — the same guarantee `mktemp` gives the shell.
pub fn sibling_tmp_for(dst: &Path) -> Result<PathBuf> {
    let dir = dst.parent().unwrap_or_else(|| Path::new("/"));
    let base = dst.file_name().ok_or(Error::Usage {
        message: "destination has no file name",
    })?;
    std::fs::create_dir_all(dir).map_err(|source| Error::Io {
        context: "create sibling temp parent",
        source,
    })?;
    let mut prefix = base.to_os_string();
    prefix.push(".tmp.");
    for _ in 0..TMP_RETRIES {
        // `OsString::truncate` is still unstable: rebuild the name per
        // attempt instead of truncating back to the prefix.
        let mut name = prefix.clone();
        name.push(random_suffix());
        let candidate = dir.join(&name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                // Taken: the next iteration tries a fresh suffix.
            }
            Err(source) => {
                return Err(Error::Io {
                    context: "create sibling temp file",
                    source,
                });
            }
        }
    }
    Err(Error::Usage {
        message: "sibling temp names keep colliding",
    })
}

/// Six mktemp-alphabet characters from `/dev/urandom`; pid, time, and
/// a process-wide counter mixed in when urandom is unavailable, so the
/// fallback is still unique per call within a process.
fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut bytes = [0u8; TMP_SUFFIX_LEN];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| {
            use std::io::Read as _;
            file.read_exact(&mut bytes)
        })
        .is_err()
    {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut seed = std::process::id() as u64 ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        seed ^= seed.wrapping_mul(0xBF58_476D_1CE4_E5B9) >> 27;
        for slot in bytes.iter_mut() {
            seed = seed.wrapping_mul(0x94D0_49BB_1331_11EB);
            *slot = (seed >> 32) as u8;
        }
    }
    bytes
        .iter()
        // Modulo the alphabet length (62), not a power of two: `% 64`
        // indexes past the end for bytes 62 and 63.
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect()
}

/// `_dot_path_identity`: `stat -c '%d:%i'` as `(device, inode)`.
/// `stat` follows symlinks (no `-P` anywhere in this domain), so a
/// link reports its target's identity. Callers format
/// [`identity_string`]; the pair itself drives the parent-swap and
/// file-swap comparisons.
pub fn path_identity(path: &Path) -> Result<(u64, u64)> {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
        context: "stat path identity",
        source,
    })?;
    Ok((meta.dev(), meta.ino()))
}

/// Render a `(device, inode)` pair exactly like `stat -c '%d:%i'`:
/// decimal, colon-separated. Both engines compare these strings.
pub fn identity_string(identity: (u64, u64)) -> String {
    format!("{}:{}", identity.0, identity.1)
}

/// Permission bits (`stat -c '%a'`): the low twelve mode bits. The
/// shell prints them without leading zeros (`644`, `700`); format
/// with `{:o}` to match.
pub fn file_mode(path: &Path) -> Result<u32> {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
        context: "stat file mode",
        source,
    })?;
    Ok(meta.mode() & 0o7777)
}

/// `stat -c '%s'`: file size in bytes.
pub fn file_size(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
        context: "stat file size",
        source,
    })?;
    Ok(meta.size())
}

/// `stat -c '%u'`: owning uid.
pub fn path_uid(path: &Path) -> Result<u32> {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
        context: "stat file owner",
        source,
    })?;
    Ok(meta.uid())
}

/// `stat -c '%h'`: hard-link count.
pub fn path_nlink(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path).map_err(|source| Error::Io {
        context: "stat link count",
        source,
    })?;
    Ok(meta.nlink())
}

/// Current effective uid, forked from `id -u` exactly like the shell
/// (see `platform::require_sudo`): no libc binding for parity.
pub fn current_uid() -> Option<u32> {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
}

/// `_dot_private_dir_validate`: a real directory (never a symlink)
/// at mode 700 owned by us.
pub fn private_dir_validate(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat private dir",
        source,
    })?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "not a private directory",
        });
    }
    // `symlink_metadata` on a symlink reports the link itself, so a
    // passing `is_dir` already excludes links; the explicit check
    // above mirrors the shell's `[[ -d $path && ! -L $path ]]` shape.
    let mode = meta.mode() & 0o7777;
    let uid = current_uid().ok_or(Error::Usage {
        message: "cannot determine owner",
    })?;
    if mode != 0o700 || meta.uid() != uid {
        return Err(Error::Usage {
            message: "private directory has wrong mode or owner",
        });
    }
    Ok(())
}

/// `_dot_private_control_file_validate`: a regular file (never a
/// symlink) at mode 600, owned by us, with exactly one link — so no
/// second name can mutate the bytes out from under the reader.
pub fn private_control_file_validate(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat control file",
        source,
    })?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "not a control file",
        });
    }
    let uid = current_uid().ok_or(Error::Usage {
        message: "cannot determine owner",
    })?;
    if meta.mode() & 0o7777 != 0o600 || meta.uid() != uid || meta.nlink() != 1 {
        return Err(Error::Usage {
            message: "control file has wrong mode, owner, or link count",
        });
    }
    Ok(())
}

/// Read the engine process umask by asking `sh` (whose builtin reports
/// the inherited mask): `std` has no `umask(2)` binding, and the shell
/// pays the same fork with `mask=$(umask)`.
pub fn read_umask() -> Result<u32> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg("umask")
        .output()
        .map_err(|source| Error::Io {
            context: "read umask",
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Usage {
            message: "umask query failed",
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let digits = text.trim();
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Usage {
            message: "umask query returned non-octal output",
        });
    }
    u32::from_str_radix(digits, 8).map_err(|_| Error::Usage {
        message: "umask query returned non-octal output",
    })
}

/// `_dot_apply_tracked_file_mode`: force a git-tracked mode (`100644`
/// or `100755`) onto a real file. The shell spells this with omitted-who
/// symbolic modes (`chmod '=rw'` then `chmod +x`), which honor the
/// effective umask even when a parent default ACL granted broader
/// permissions at creation — so the port computes the same masked bits
/// explicitly: `0666 & !mask`, plus `0111 & !mask` for the executable
/// bit. Anything else (symlink, other git mode) fails.
pub fn apply_tracked_file_mode(path: &Path, git_mode: &str, mask: u32) -> Result<()> {
    let executable = match git_mode {
        "100644" => false,
        "100755" => true,
        _ => {
            return Err(Error::Usage {
                message: "unsupported tracked file mode",
            });
        }
    };
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat tracked file",
        source,
    })?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "tracked mode needs a regular file",
        });
    }
    let mut mode = 0o666 & !mask;
    if executable {
        mode |= 0o111 & !mask;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777)).map_err(
        |source| Error::Io {
            context: "chmod tracked file",
            source,
        },
    )
}

/// `_dot_apply_umask_ceiling`: clamp a real file or directory to
/// `mode & ceiling & ~mask`, rechecking the device/inode identity
/// before and after the chmod so a swapped path fails instead of
/// chmodding a stranger. `ceiling` defaults to `0o7777` like the
/// shell's `${2:-07777}`.
pub fn apply_umask_ceiling(path: &Path, ceiling: Option<u32>, mask: u32) -> Result<()> {
    let ceiling = ceiling.unwrap_or(0o7777);
    let identity = identity_string(path_identity(path).map_err(|_| Error::Usage {
        message: "cannot identify path for ceiling",
    })?);
    // Like the shell's `stat` (which lstates command-line symlinks):
    // the mode read sees the link itself (always 0o777), while the
    // chmod below follows it, so ceilings land on the link target.
    // There is deliberately no file-type gate (unlike the tracked-mode
    // setter): the shell fn stats and chmods whatever the path is.
    let meta = std::fs::symlink_metadata(path).map_err(|_| Error::Usage {
        message: "cannot stat path for ceiling",
    })?;
    let mode = meta.mode() & 0o7777;
    let normalized = mode & (ceiling & 0o7777) & !(mask & 0o777);
    if identity_string(path_identity(path).map_err(|_| Error::Usage {
        message: "path changed before ceiling",
    })?) != identity
    {
        return Err(Error::Usage {
            message: "path changed before ceiling",
        });
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(normalized)).map_err(
        |source| Error::Io {
            context: "chmod ceiling",
            source,
        },
    )?;
    if identity_string(path_identity(path).map_err(|_| Error::Usage {
        message: "path changed after ceiling",
    })?) != identity
    {
        return Err(Error::Usage {
            message: "path changed after ceiling",
        });
    }
    Ok(())
}

/// The selected Dot checkout for content hashing: `${DOT_SOURCE_ROOT:-$PWD}`.
pub fn source_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("DOT_SOURCE_ROOT") {
        if !root.is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    std::env::current_dir().map_err(|source| Error::Io {
        context: "determine source root",
        source,
    })
}

/// `_dot_sanitized_git`: internal Git calls must not inherit a caller's
/// selected repository, object store, hash default, or configuration.
/// Builds the `git` command with that isolation boundary applied:
/// the unset list plus `GIT_CONFIG_NOSYSTEM=1`,
/// `GIT_CONFIG_GLOBAL=/dev/null`, and the `-c safe.directory=` /
/// `-C source_root` binding. `git` itself resolves off the engine PATH
/// like the shell's `command git`.
pub fn sanitized_git<S: AsRef<std::ffi::OsStr>>(
    source_root: &Path,
    args: &[S],
) -> std::process::Command {
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
    let directory = source_root.as_os_str().as_bytes();
    let mut safe = b"safe.directory=".to_vec();
    safe.extend_from_slice(directory);
    cmd.arg("-c");
    // `Command::arg` takes `AsRef<OsStr>`; the byte-built `safe`
    // preserves non-UTF8 roots exactly.
    cmd.arg(std::ffi::OsStr::from_bytes(&safe));
    cmd.arg("-C");
    cmd.arg(source_root);
    cmd.args(args);
    cmd
}

/// Run `git hash-object` under the sanitized binding; `stdin` feeds
/// `--stdin` when present. Returns the raw hash line.
fn hash_object<S: AsRef<std::ffi::OsStr>>(
    source_root: &Path,
    args: &[S],
    stdin: Option<&[u8]>,
) -> Result<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    // The subcommand lives here, not at the call sites: `sanitized_git`
    // only builds the isolated `git -c/-C` prefix (like `_dot_sanitized_git`,
    // which takes the subcommand as its first real argument).
    let mut full: Vec<&std::ffi::OsStr> = vec![std::ffi::OsStr::new("hash-object")];
    full.extend(args.iter().map(|arg| arg.as_ref()));
    let mut cmd = sanitized_git(source_root, &full);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|source| Error::Io {
        context: "spawn git hash-object",
        source,
    })?;
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .ok_or(Error::Usage {
                message: "hash-object stdin unavailable",
            })?
            .write_all(input)
            .map_err(|source| Error::Io {
                context: "feed git hash-object",
                source,
            })?;
    }
    let output = child.wait_with_output().map_err(|source| Error::Io {
        context: "wait git hash-object",
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "git hash-object".to_string(),
            status: Some(output.status.to_string()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `_dot_file_digest`: raw filter-free content hash of one file.
pub fn file_digest(source_root: &Path, path: &Path) -> Result<String> {
    hash_object(
        source_root,
        &[std::ffi::OsStr::new("--no-filters"), path.as_os_str()],
        None,
    )
}

/// `_dot_file_text_digest`: hash of short in-memory bytes, fed via
/// `--stdin` exactly like `printf '%s' "$1" | _dot_hash_object --stdin`.
pub fn file_text_digest(source_root: &Path, text: &[u8]) -> Result<String> {
    hash_object(source_root, &[std::ffi::OsStr::new("--stdin")], Some(text))
}

/// Hash two files with one `git hash-object` call and report whether
/// both digests are well-formed and equal (`_dot_files_equal`).
pub fn files_equal(source_root: &Path, first: &Path, second: &Path) -> Result<bool> {
    let output = hash_object(
        source_root,
        &[
            std::ffi::OsStr::new("--no-filters"),
            std::ffi::OsStr::new("--"),
            first.as_os_str(),
            second.as_os_str(),
        ],
        None,
    )?;
    Ok(hash_pair_equal(&output))
}

/// `_dot_stdin_matches_file`: hash piped bytes plus one file, then the
/// same pair check.
pub fn stdin_matches_file(source_root: &Path, stdin: &[u8], path: &Path) -> Result<bool> {
    let output = hash_object(
        source_root,
        &[
            std::ffi::OsStr::new("--no-filters"),
            std::ffi::OsStr::new("--stdin"),
            std::ffi::OsStr::new("--"),
            path.as_os_str(),
        ],
        Some(stdin),
    )?;
    Ok(hash_pair_equal(&output))
}

/// `_dot_hash_pair_equal`: the `hash-object` output must be exactly two
/// well-formed (40- or 64-hex) digests, one per line, and equal. The
/// shell's `$(...)` strips trailing newlines, so `trim` mirrors the
/// capture before splitting on the first newline; a second newline
/// (three or more hashes) fails like the shell's `$second` check.
pub fn hash_pair_equal(hashes: &str) -> bool {
    fn is_sha(text: &str) -> bool {
        // Lowercase only, like the shell's `[0-9a-f]` classes: git
        // never emits uppercase, and crafted uppercase must fail.
        (text.len() == 40 || text.len() == 64)
            && text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }
    let trimmed = hashes.trim_end_matches('\n');
    let Some((first, second)) = trimmed.split_once('\n') else {
        return false;
    };
    !second.contains('\n') && is_sha(first) && second == first
}

/// A resolved generation target (`_dot_file_target_resolve`): the
/// logical destination pinned to one stable physical parent, so
/// replacing a parent symlink cannot redirect a later conditional
/// update. Generation tokens bind both names.
#[derive(Debug, Clone)]
pub struct Target {
    /// Physical parent directory (`cd -P` / `pwd -P` equivalent).
    pub parent: PathBuf,
    /// `parent/base`: the guarded destination path.
    pub path: PathBuf,
    /// `dev:ino` of the parent, taken at resolve time.
    pub parent_id: String,
    /// Content hash of the `parent/base` string (binds the name).
    pub path_digest: String,
    /// `parent/.$base.dot-file-transaction-v1`: the journal directory.
    pub transaction: PathBuf,
}

/// Resolve a logical destination to a [`Target`]. The path must be
/// absolute, free of newline/CR/tab, and reduce to a real directory
/// parent plus a usable basename (not empty, `.`, or `..`).
pub fn file_target_resolve(source_root: &Path, path: &Path) -> Result<Target> {
    if !path.is_absolute() || has_control_char(path) {
        return Err(Error::Usage {
            message: "destination must be an absolute path without newline, CR, or tab",
        });
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    let base = path.file_name().ok_or(Error::Usage {
        message: "destination has no file name",
    })?;
    if base.is_empty() || base == "." || base == ".." {
        return Err(Error::Usage {
            message: "destination has no file name",
        });
    }
    // `cd -P -- dir && pwd -P`: canonicalize the parent (which must
    // exist), keeping the leaf itself unresolved — the leaf may be
    // absent, which is exactly the `remove` precondition.
    let physical = parent.canonicalize().map_err(|_| Error::Usage {
        message: "destination parent is not a physical directory",
    })?;
    if !physical.is_dir() || physical.is_symlink() {
        return Err(Error::Usage {
            message: "destination parent is not a physical directory",
        });
    }
    let identity = identity_string(path_identity(&physical).map_err(|_| Error::Usage {
        message: "cannot identify destination parent",
    })?);
    // The bound name is hashed as raw bytes (`"$physical/$base"`);
    // lossy rendering would change the digest for non-UTF8 leaves.
    let mut path_bytes = physical.as_os_str().as_bytes().to_vec();
    path_bytes.push(b'/');
    path_bytes.extend_from_slice(base.as_bytes());
    let digest = file_text_digest(source_root, &path_bytes)?;
    let mut transaction_name = std::ffi::OsString::from(".");
    transaction_name.push(base);
    transaction_name.push(".dot-file-transaction-v1");
    Ok(Target {
        parent: physical.clone(),
        path: physical.join(base),
        parent_id: identity,
        path_digest: digest,
        transaction: physical.join(transaction_name),
    })
}

/// A live file's identity block (`_dot_file_signature`):
/// `dev|ino|mode|size|digest` with `%a` minimal-octal mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Device number, decimal.
    pub device: u64,
    /// Inode number, decimal.
    pub inode: u64,
    /// Permission bits (`stat -c '%a'` numeric value).
    pub mode: u32,
    /// Size in bytes.
    pub size: u64,
    /// Raw content digest.
    pub digest: String,
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}|{}|{:o}|{}|{}",
            self.device, self.inode, self.mode, self.size, self.digest
        )
    }
}

/// The expected half of a generation token after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// `file` or `absent`.
    pub state: String,
    /// Digest binding the destination name.
    pub path_digest: String,
    /// `dev:ino` of the parent at stage time.
    pub parent_id: String,
    /// Live-file block when `state == "file"`, kept verbatim as written
    /// in the token (`dev|ino|mode|size|digest`): every comparison is a
    /// string comparison like the shell's, so normalizing here would
    /// accept journals the shell fails closed on. `None` when absent.
    pub signature: Option<String>,
}

/// `_dot_file_signature`: identity block for a real file, or an error
/// for anything else (absent, symlink, directory).
pub fn file_signature(source_root: &Path, path: &Path) -> Result<Signature> {
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat signature",
        source,
    })?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "signature needs a regular file",
        });
    }
    let identity = (meta.dev(), meta.ino());
    let digest = file_digest(source_root, path)?;
    Ok(Signature {
        device: identity.0,
        inode: identity.1,
        mode: meta.mode() & 0o7777,
        size: meta.size(),
        digest,
    })
}

/// `_dot_file_generation_raw`: stage a fresh token for `path`:
/// `v1|pathdigest|parentdev|parentino|state|signature...|checksum`.
pub fn file_generation_raw(source_root: &Path, path: &Path) -> Result<String> {
    let target = file_target_resolve(source_root, path)?;
    // A dangling symlink reports `Ok` symlink metadata: not a file,
    // not absent — the shell's `-e`/`-L` probe likewise rejects it as
    // neither usable state. Any stat failure reads as absent, exactly
    // like the shell's `-e`/`-L` test (a permission error here is as
    // unusable as a missing leaf).
    let state = match std::fs::symlink_metadata(&target.path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            let signature = file_signature(source_root, &target.path)?;
            format!("file|{signature}")
        }
        Ok(_) => {
            return Err(Error::Usage {
                message: "destination is not a regular file",
            });
        }
        Err(_) => "absent|-|-|-|-|-".to_string(),
    };
    let payload = format!(
        "v1|{}|{}|{}",
        target.path_digest,
        target.parent_id.replace(':', "|"),
        state
    );
    let checksum = file_text_digest(
        source_root,
        format!("dot-file-generation-v1|{payload}").as_bytes(),
    )?;
    Ok(format!("{payload}|{checksum}"))
}

/// Split a delimited token the way `IFS='|' read` (or `IFS=$'\t'`
/// read) assigns trailing variables: the first N-1 variables take the
/// first N-1 fields, and the last variable takes the unsplit
/// remainder. Returns the leading fields only when the remainder is
/// empty — i.e. exactly N fields, or N+1 with a trailing empty field
/// (a trailing delimiter assigns `""` to the last variable, which the
/// shell's `-z` test accepts).
fn split_trailing(fields: &str, delimiter: char, leading: usize) -> Option<Vec<&str>> {
    let parts: Vec<&str> = fields.split(delimiter).collect();
    if parts.len() == leading {
        return Some(parts);
    }
    if parts.len() == leading + 1 && parts[leading].is_empty() {
        return Some(parts[..leading].to_vec());
    }
    None
}

/// True for a decimal digit string (`^[0-9]+$`).
fn is_uint(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// True for an octal digit string (`^[0-7]+$`).
fn is_octal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
}

/// True for a lowercase hex digest of either hash length.
fn is_digest(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64)
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `_dot_file_generation_validate`: check a token's shape, field
/// classes, and checksum, returning the expected half. Rejects any
/// token carrying newline/CR/tab first, like the shell.
pub fn generation_validate(source_root: &Path, token: &str) -> Result<Expected> {
    if token.contains(['\n', '\r', '\t']) {
        return Err(Error::Usage {
            message: "generation token carries control characters",
        });
    }
    let fields = split_trailing(token, '|', 11).ok_or(Error::Usage {
        message: "generation token has wrong field count",
    })?;
    let (version, path_digest, parent_device, parent_inode, state) =
        (fields[0], fields[1], fields[2], fields[3], fields[4]);
    let (leaf_device, leaf_inode, mode, size, digest, checksum) = (
        fields[5], fields[6], fields[7], fields[8], fields[9], fields[10],
    );
    if version != "v1"
        || !is_digest(path_digest)
        || !is_uint(parent_device)
        || !is_uint(parent_inode)
    {
        return Err(Error::Usage {
            message: "generation token header malformed",
        });
    }
    let signature = match state {
        "absent" => {
            if leaf_device != "-"
                || leaf_inode != "-"
                || mode != "-"
                || size != "-"
                || digest != "-"
            {
                return Err(Error::Usage {
                    message: "absent generation carries file fields",
                });
            }
            None
        }
        "file" => {
            if !is_uint(leaf_device)
                || !is_uint(leaf_inode)
                || !is_octal(mode)
                || !is_uint(size)
                || !is_digest(digest)
            {
                return Err(Error::Usage {
                    message: "file generation fields malformed",
                });
            }
            // Stored verbatim (see the `signature` docs): classes are
            // validated, numerics never parsed, so no normalization can
            // sneak in through leading zeros or huge digit strings.
            Some(format!("{leaf_device}|{leaf_inode}|{mode}|{size}|{digest}"))
        }
        _ => {
            return Err(Error::Usage {
                message: "generation state is not file or absent",
            });
        }
    };
    if !is_digest(checksum) {
        return Err(Error::Usage {
            message: "generation checksum malformed",
        });
    }
    // Mirror `payload=${token%|*}` (shortest trailing `|*`): for a
    // trailing-delimiter token this strips only the final pipe, so
    // both spellings checksum the same eleven fields.
    let payload = token
        .rsplit_once('|')
        .map(|(head, _)| head)
        .unwrap_or(token);
    let expected = file_text_digest(
        source_root,
        format!("dot-file-generation-v1|{payload}").as_bytes(),
    )?;
    if expected != checksum {
        return Err(Error::Usage {
            message: "generation checksum mismatch",
        });
    }
    Ok(Expected {
        state: state.to_string(),
        path_digest: path_digest.to_string(),
        parent_id: format!("{parent_device}:{parent_inode}"),
        signature,
    })
}

/// The `_dot_file_transaction_lock_valid` gate: `${DOT_TEST:-0} == 1`
/// (test harness) or a non-empty `DOT_UPDATE_LOCK_TOKEN` (update holds
/// the lock). Threaded explicitly so tests pin it without env games.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockCtx {
    /// Value of `DOT_TEST` after `${DOT_TEST:-0}` defaulting.
    pub test_mode: bool,
    /// Whether `DOT_UPDATE_LOCK_TOKEN` is set and non-empty.
    pub token_present: bool,
}

impl LockCtx {
    /// Read the gate from the process environment, exactly like the
    /// shell: unset or empty `DOT_TEST` counts as `0`.
    pub fn from_env() -> Self {
        let test_mode = std::env::var("DOT_TEST").is_ok_and(|value| value == "1");
        let token_present =
            std::env::var("DOT_UPDATE_LOCK_TOKEN").is_ok_and(|value| !value.is_empty());
        Self {
            test_mode,
            token_present,
        }
    }

    /// True when file transactions may run.
    pub fn valid(self) -> bool {
        self.test_mode || self.token_present
    }
}

/// A validated transaction journal (`$transaction/record`):
/// `v1\toperation\tphase\texpected\tcandidate`.
#[derive(Debug, Clone)]
pub struct Record {
    /// `replace` or `remove`.
    pub operation: String,
    /// `prepared`, `quarantined`, or `committed`.
    pub phase: String,
    /// The generation the transaction is conditional on.
    pub expected: Expected,
    /// Staged replacement identity for `replace`, verbatim token text
    /// (string-compared, never normalized); `None` for `remove`.
    pub candidate: Option<String>,
}

/// `_dot_file_transaction_record_read`: validate the journal's control
/// file, first line, shape, operation/phase words, expected token, and
/// candidate block. Takes the transaction directory (the journal is
/// always `record` inside, exactly like the shell). Only the first
/// line is read, like the shell's single `read` (trailing lines are
/// ignored there too).
pub fn record_read(source_root: &Path, transaction: &Path) -> Result<Record> {
    let record = transaction.join("record");
    private_control_file_validate(&record)?;
    let content = std::fs::read(record).map_err(|source| Error::Io {
        context: "read transaction record",
        source,
    })?;
    let first = content.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
    let line = std::str::from_utf8(first).map_err(|_| Error::Usage {
        message: "transaction record is not UTF-8",
    })?;
    let fields = split_trailing(line, '\t', 5).ok_or(Error::Usage {
        message: "transaction record has wrong field count",
    })?;
    if fields[0] != "v1" {
        return Err(Error::Usage {
            message: "transaction record version is not v1",
        });
    }
    if fields[1] != "replace" && fields[1] != "remove" {
        return Err(Error::Usage {
            message: "transaction record operation unknown",
        });
    }
    if fields[2] != "prepared" && fields[2] != "quarantined" && fields[2] != "committed" {
        return Err(Error::Usage {
            message: "transaction record phase unknown",
        });
    }
    let expected = generation_validate(source_root, fields[3])?;
    let candidate = if fields[1] == "replace" {
        validate_candidate(fields[4])?;
        Some(fields[4].to_string())
    } else {
        if fields[4] != "-" {
            return Err(Error::Usage {
                message: "remove record carries a candidate",
            });
        }
        None
    };
    Ok(Record {
        operation: fields[1].to_string(),
        phase: fields[2].to_string(),
        expected,
        candidate,
    })
}

/// Validate a record candidate block (`dev|ino|mode|size|digest`,
/// either hash length): the shell's two alternation branches in one
/// check. The text itself is stored and compared verbatim elsewhere.
fn validate_candidate(text: &str) -> Result<()> {
    let fields: Vec<&str> = text.split('|').collect();
    if fields.len() != 5
        || !is_uint(fields[0])
        || !is_uint(fields[1])
        || !is_octal(fields[2])
        || !is_uint(fields[3])
        || !is_digest(fields[4])
    {
        return Err(Error::Usage {
            message: "transaction candidate malformed",
        });
    }
    Ok(())
}

/// Render a candidate block for the journal (`-` for `remove`).
fn render_candidate(candidate: Option<&Signature>) -> String {
    candidate
        .map(|signature| signature.to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// `_dot_file_transaction_record_write`: stage `$transaction/record.next`
/// (mode 600 from birth, control-validated) then publish it over
/// `record` — replacing when a record exists, exclusively creating
/// otherwise. A losing shape leaves `record.next` behind exactly like
/// the shell (a wedged directory stays wedged until retried after
/// manual repair); only the `chmod`/validation failures remove it.
pub fn record_write(
    transaction: &Path,
    operation: &str,
    phase: &str,
    expected_token: &str,
    candidate: Option<&Signature>,
    cache: &mut MoveCache,
) -> Result<()> {
    use std::io::Write as _;
    if !symlink_metadata_dir(transaction)? {
        return Err(Error::Usage {
            message: "transaction is not a directory",
        });
    }
    let next = transaction.join("record.next");
    if next.symlink_metadata().is_ok() {
        return Err(Error::Usage {
            message: "transaction record.next already exists",
        });
    }
    let body = format!(
        "v1\t{operation}\t{phase}\t{expected_token}\t{}\n",
        render_candidate(candidate)
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&next).map_err(|source| Error::Io {
        context: "create transaction record.next",
        source,
    })?;
    // A failed write leaves `record.next` behind, like the shell's
    // unchecked redirect: the next attempt fails on the exists check.
    if file.write_all(body.as_bytes()).is_err() {
        return Err(Error::Usage {
            message: "cannot stage transaction record",
        });
    }
    drop(file);
    let cleanup_next = |result: Result<()>| -> Result<()> {
        if result.is_err() {
            let _ = std::fs::remove_file(&next);
        }
        result
    };
    cleanup_next(set_mode(&next, 0o600))?;
    cleanup_next(
        private_control_file_validate(&next).map_err(|_| Error::Usage {
            message: "staged record failed control validation",
        }),
    )?;
    // NOTE: from here on `record.next` is intentionally left behind on
    // failure, mirroring the shell.
    let record = transaction.join("record");
    match record.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
            move_replace_nodir_cached(&next, &record, cache)
        }
        Ok(_) => Err(Error::Usage {
            message: "transaction record is not a regular file",
        }),
        Err(_) => move_noreplace_cached(&next, &record, cache),
    }
}

/// True for a real directory that is not a symlink.
fn symlink_metadata_dir(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_dir() && !meta.file_type().is_symlink()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            context: "stat transaction dir",
            source,
        }),
    }
}

/// Set exact permission bits (numeric `chmod`).
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        Error::Io {
            context: "chmod transaction file",
            source,
        }
    })
}

/// Which `mv` spelling moves without nesting into a late directory:
/// GNU `mv -nT`/`-fT` (treat target as a file) or BSD `mv -nh`/`-fh`
/// (do not follow a target symlink). Probed once per binary, exactly
/// like `_dot_detect_move_tool`.
#[derive(Debug, Clone)]
pub struct MoveTool {
    /// Resolved `mv` binary (`type -P mv` equivalent).
    pub bin: PathBuf,
    /// True for the GNU `-T` spelling, false for BSD `-h`.
    pub no_target_dir: bool,
}

/// Process cache for [`MoveTool`]: the shell memoizes `DOT_MOVE_BIN` /
/// `DOT_MOVE_MODE` and revalidates when the PATH lookup changes, so
/// the port keys on the resolved binary too. Engine callers hold one
/// per run; tests use a fresh cache per case for determinism.
#[derive(Debug, Clone, Default)]
pub struct MoveCache {
    tool: Option<MoveTool>,
}

impl MoveCache {
    /// Resolve and probe as needed; a cached tool is reused only while
    /// the same executable still resolves off PATH (the shell's
    /// `-x $DOT_MOVE_BIN && $DOT_MOVE_BIN == $mv_bin` check).
    pub fn tool(&mut self) -> Result<MoveTool> {
        let resolved = resolve_mv().ok_or(Error::Usage {
            message: "no mv on PATH",
        })?;
        if let Some(tool) = &self.tool {
            if tool.bin == resolved && is_executable(&tool.bin) {
                return Ok(tool.clone());
            }
        }
        let tool = detect_move_tool(&resolved)?;
        self.tool = Some(tool.clone());
        Ok(tool)
    }
}

/// First executable `mv` off the engine PATH (`type -P mv`).
fn resolve_mv() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("mv");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// True for a regular file with any execute bit (POSIX `type -P`
/// only reports executables).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

/// Non-Unix fallback: executability has no bit to test.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `_dot_detect_move_tool` for one binary: probe `mv -nT` on a scratch
/// directory, falling back to `mv -nh`. The probe tree lives under the
/// system temp dir (`${TMPDIR:-/tmp}` via `std::env::temp_dir`) with a
/// unique leaf, and is removed either way — mirroring the shell's
/// `mktemp -d` / `rm -rf` / `rmdir` shape.
fn detect_move_tool(mv_bin: &Path) -> Result<MoveTool> {
    // Process-wide counter plus pid: parallel probes must not share a
    // directory (one probe's cleanup would nuke another's tree), the
    // same uniqueness `mktemp -d` gives the shell.
    static PROBES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let probe = std::env::temp_dir().join(format!(
        "dot-move-tools-{}-{}",
        std::process::id(),
        PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let source = probe.join("source");
    let moved = probe.join("moved");
    let _ = std::fs::remove_dir_all(&probe);
    let no_target_dir = if std::fs::create_dir_all(&source).is_ok()
        && run_mv(mv_bin, &["-nT"], &source, &moved)
        && moved.is_dir()
        && !source.exists()
    {
        let _ = std::fs::remove_dir_all(&probe);
        true
    } else {
        let _ = std::fs::remove_dir_all(&probe);
        if std::fs::create_dir_all(&source).is_ok()
            && run_mv(mv_bin, &["-nh"], &source, &moved)
            && moved.is_dir()
            && !source.exists()
        {
            let _ = std::fs::remove_dir_all(&probe);
            false
        } else {
            let _ = std::fs::remove_dir_all(&probe);
            return Err(Error::Usage {
                message: "mv supports neither -T nor -h",
            });
        }
    };
    Ok(MoveTool {
        bin: mv_bin.to_path_buf(),
        no_target_dir,
    })
}

/// Run `mv` with flags, swallowing output: success is decided by the
/// aftermath checks, exactly like the shell's `|| true` plus tests.
fn run_mv(mv_bin: &Path, flags: &[&str], source: &Path, target: &Path) -> bool {
    std::process::Command::new(mv_bin)
        .args(flags)
        .arg("--")
        .arg(source)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Identity of a path for move verification: `stat` (following) like
/// `_dot_path_identity`, missing as `None` so a vanished target
/// compares unequal. Following matters: a target symlink pointing at
/// the source itself reports the source identity, and the shell calls
/// that shape success.
fn move_identity(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    use std::os::unix::fs::MetadataExt as _;
    Some(identity_string((meta.dev(), meta.ino())))
}

/// `_dot_move_noreplace` with an explicit tool: publish `source` at an
/// absent `target` without replacing a late file, symlink, or empty
/// directory. BSD `mv` can briefly nest the source in a late
/// directory; exact inode recovery moves only that source back out,
/// and every shape still reports failure.
pub fn move_noreplace_with(source: &Path, target: &Path, tool: &MoveTool) -> Result<()> {
    let identity = move_identity(source).ok_or(Error::Usage {
        message: "move source has no identity",
    })?;
    if tool.no_target_dir {
        run_mv(&tool.bin, &["-nT"], source, target);
    } else {
        run_mv(&tool.bin, &["-nh"], source, target);
    }
    if move_identity(target) == Some(identity.clone()) {
        return Ok(());
    }
    let nested = target.join(source.file_name().unwrap_or_default());
    if target
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
        && move_identity(&nested) == Some(identity)
    {
        // Best-effort un-nesting; the move still failed.
        let _ = std::process::Command::new(&tool.bin)
            .arg(&nested)
            .arg(source)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Err(Error::Usage {
        message: "move would replace an existing target",
    })
}

/// `_dot_move_replace_nodir` with an explicit tool: replace a known
/// engine-owned non-directory destination, with the same nesting
/// recovery as [`move_noreplace_with`].
pub fn move_replace_nodir_with(source: &Path, target: &Path, tool: &MoveTool) -> Result<()> {
    let identity = move_identity(source).ok_or(Error::Usage {
        message: "move source has no identity",
    })?;
    if tool.no_target_dir {
        run_mv(&tool.bin, &["-fT"], source, target);
    } else {
        run_mv(&tool.bin, &["-fh"], source, target);
    }
    if move_identity(target) == Some(identity.clone()) {
        return Ok(());
    }
    let nested = target.join(source.file_name().unwrap_or_default());
    if target
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
        && move_identity(&nested) == Some(identity)
    {
        let _ = std::process::Command::new(&tool.bin)
            .arg(&nested)
            .arg(source)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Err(Error::Usage {
        message: "replace move failed",
    })
}

/// Cached `_dot_move_noreplace`.
pub fn move_noreplace_cached(source: &Path, target: &Path, cache: &mut MoveCache) -> Result<()> {
    let tool = cache.tool()?;
    move_noreplace_with(source, target, &tool)
}

/// Cached `_dot_move_replace_nodir`.
pub fn move_replace_nodir_cached(
    source: &Path,
    target: &Path,
    cache: &mut MoveCache,
) -> Result<()> {
    let tool = cache.tool()?;
    move_replace_nodir_with(source, target, &tool)
}

/// `_dot_publish_prepared_regular`: publish a prepared regular file
/// without following or nesting into a late directory or symlink. An
/// existing regular target is replaced only while its no-follow
/// identity still matches; an absent target uses exclusive creation.
pub fn publish_prepared_regular(source: &Path, target: &Path, cache: &mut MoveCache) -> Result<()> {
    let source_meta = std::fs::symlink_metadata(source).map_err(|source| Error::Io {
        context: "stat publish source",
        source,
    })?;
    if !source_meta.is_file() || source_meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "publish source is not a regular file",
        });
    }
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            let target_meta = std::fs::symlink_metadata(target).map_err(|source| Error::Io {
                context: "stat publish target",
                source,
            })?;
            if !target_meta.is_file() || target_meta.file_type().is_symlink() {
                return Err(Error::Usage {
                    message: "publish target is not a regular file",
                });
            }
            let before = move_identity(target);
            if move_identity(target) != before {
                return Err(Error::Usage {
                    message: "publish target changed underfoot",
                });
            }
            move_replace_nodir_cached(source, target, cache)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            move_noreplace_cached(source, target, cache)
        }
        Err(_) => move_noreplace_cached(source, target, cache),
    }
}

/// A fresh 0700 scratch directory `$parent/$prefix$random` for probe
/// and init trees (the shell's `mktemp -d` plus `chmod 700` plus
/// private validation, in one helper). The prefix is `OsStr`: journal
/// names may be non-UTF8, and the suffix is ASCII.
fn private_scratch_dir(parent: &Path, prefix: &std::ffi::OsStr) -> Result<PathBuf> {
    for _ in 0..TMP_RETRIES {
        let mut name = prefix.to_os_string();
        name.push(random_suffix());
        let dir = parent.join(&name);
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                let good = set_mode(&dir, 0o700).is_ok() && private_dir_validate(&dir).is_ok();
                if good {
                    return Ok(dir);
                }
                let _ = std::fs::remove_dir(&dir);
                return Err(Error::Usage {
                    message: "scratch dir failed validation",
                });
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(Error::Io {
                    context: "create scratch dir",
                    source,
                });
            }
        }
    }
    Err(Error::Usage {
        message: "scratch dir names keep colliding",
    })
}

/// `_dot_file_transaction_entries_validate`: the journal holds only
/// known entries — `record`/`record.next` as control files,
/// `candidate`/`previous` as regular files — nothing else. Checks are
/// order-independent (every entry must pass), so raw readdir order is
/// fine here.
pub fn transaction_entries_validate(transaction: &Path) -> Result<()> {
    private_dir_validate(transaction)?;
    for entry in std::fs::read_dir(transaction).map_err(|source| Error::Io {
        context: "list transaction dir",
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            context: "list transaction dir",
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        if name == "record" || name == "record.next" {
            private_control_file_validate(&path)?;
        } else if name == "candidate" || name == "previous" {
            let meta = std::fs::symlink_metadata(&path).map_err(|source| Error::Io {
                context: "stat transaction entry",
                source,
            })?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(Error::Usage {
                    message: "transaction payload is not a regular file",
                });
            }
        } else {
            return Err(Error::Usage {
                message: "transaction holds an unknown entry",
            });
        }
    }
    Ok(())
}

/// Is `path` present in any form (the shell's `-e`/`-L` test)?
fn any_exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Remove a transaction payload or journal file after asserting it is
/// a regular non-symlink; anything else (or a removal failure) fails.
fn remove_transaction_file(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat transaction entry",
        source,
    })?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "transaction entry is not a regular file",
        });
    }
    std::fs::remove_file(path).map_err(|source| Error::Io {
        context: "remove transaction entry",
        source,
    })
}

/// `_dot_file_transaction_discard_private`: validate, remove the two
/// payloads and both journal spellings, then drop the directory.
pub fn transaction_discard_private(transaction: &Path) -> Result<()> {
    transaction_entries_validate(transaction)?;
    for name in ["candidate", "previous", "record.next", "record"] {
        let path = transaction.join(name);
        if any_exists(&path) {
            remove_transaction_file(&path)?;
        }
    }
    std::fs::remove_dir(transaction).map_err(|source| Error::Io {
        context: "remove transaction dir",
        source,
    })
}

/// `_dot_file_transaction_cleanup`: retire payload files while the
/// authoritative journal stays at its deterministic name (a crash
/// stays retryable), then rename the record-only directory out of the
/// active namespace and discard it — avoiding both a blocking empty
/// directory and orphaned backups.
pub fn transaction_cleanup(transaction: &Path, cache: &mut MoveCache) -> Result<()> {
    transaction_entries_validate(transaction)?;
    private_control_file_validate(&transaction.join("record"))?;
    for name in ["candidate", "previous", "record.next"] {
        let path = transaction.join(name);
        if any_exists(&path) {
            remove_transaction_file(&path)?;
        }
    }
    let parent = transaction.parent().unwrap_or_else(|| Path::new("/"));
    let staging = private_scratch_dir(parent, std::ffi::OsStr::new("cleanup.")).map_err(|_| {
        Error::Usage {
            message: "cannot stage transaction cleanup dir",
        }
    })?;
    if move_noreplace_cached(transaction, &staging.join("transaction"), cache).is_err() {
        let _ = std::fs::remove_dir(&staging);
        return Err(Error::Usage {
            message: "cannot retire transaction dir",
        });
    }
    // The moved directory keeps its journal: discard by value, then
    // drop the staging root. A discard failure still reports — the
    // shell surfaces the discard status either way.
    let discard = transaction_discard_private(&staging.join("transaction"));
    let unstage = std::fs::remove_dir(&staging).map_err(|source| Error::Io {
        context: "remove cleanup staging dir",
        source,
    });
    discard.and(unstage)
}

/// `_dot_file_transaction_restore_previous`: move a quarantined
/// `previous` back, but only onto an absent name (a late winner fails
/// closed instead of being replaced).
pub fn transaction_restore_previous(
    previous: &Path,
    destination: &Path,
    cache: &mut MoveCache,
) -> Result<()> {
    if any_exists(destination) {
        return Err(Error::Usage {
            message: "restore destination already exists",
        });
    }
    move_noreplace_cached(previous, destination, cache)
}

/// A staged transaction plus everything the phases pass along (the
/// shell's `DOT_FILE_TRANSACTION_*` globals as one value).
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The journal directory (`parent/.$base.dot-file-transaction-v1`).
    pub transaction: PathBuf,
    /// Guarded destination path.
    pub destination: PathBuf,
    /// Staging source for `replace`; `None` for `remove`.
    pub source: Option<PathBuf>,
    /// `replace` or `remove` (passed through unchecked, like the shell:
    /// an unknown operation fails later at journal read).
    pub operation: String,
    /// The generation token the transaction is conditional on.
    pub expected_token: String,
    /// Staged replacement identity for `replace`.
    pub candidate: Option<Signature>,
    /// Destination binding (for crash recovery after quarantine).
    pub target: Target,
}

/// Fresh identity render for an existing regular file (`None` when
/// absent); anything else fails — shared by recover and quarantine.
fn live_signature(source_root: &Path, path: &Path) -> Result<Option<String>> {
    if !any_exists(path) {
        return Ok(None);
    }
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat live path",
        source,
    })?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "live path is not a regular file",
        });
    }
    Ok(Some(file_signature(source_root, path)?.to_string()))
}

/// `_dot_file_generation`: gate, resolve, recover any leftover
/// journal, then stage a fresh token.
pub fn file_generation(
    source_root: &Path,
    lock: LockCtx,
    path: &Path,
    cache: &mut MoveCache,
) -> Result<String> {
    if !lock.valid() {
        return Err(Error::Usage {
            message: "file transactions need DOT_TEST=1 or a lock token",
        });
    }
    let target = file_target_resolve(source_root, path)?;
    transaction_recover(
        source_root,
        &target.path,
        &target.transaction,
        &target,
        cache,
    )?;
    file_generation_raw(source_root, path)
}

/// Recover a transaction left by abrupt process termination
/// (`_dot_file_transaction_recover`). The record's atomic phase change
/// is the remove commit point; replacement publication is
/// independently recognizable from the recorded candidate identity.
/// Fresh renders compare against verbatim journal text throughout,
/// exactly like the shell's string comparisons.
pub fn transaction_recover(
    source_root: &Path,
    destination: &Path,
    transaction: &Path,
    target: &Target,
    cache: &mut MoveCache,
) -> Result<()> {
    if !any_exists(transaction) {
        return Ok(());
    }
    transaction_entries_validate(transaction)?;
    let record = record_read(source_root, transaction)?;
    if record.expected.path_digest != target.path_digest
        || record.expected.parent_id != target.parent_id
    {
        return Err(Error::Usage {
            message: "stale transaction journal for another destination",
        });
    }
    let live = live_signature(source_root, destination)?;
    let previous = transaction.join("previous");
    let staged = live_signature(source_root, &previous)?;
    if staged.is_some() && (record.expected.state != "file" || staged != record.expected.signature)
    {
        // A replacement that won the final pre-mutation race was
        // quarantined. Put it back when possible; if another writer
        // already filled the name, retain both versions and fail
        // closed for explicit operator recovery.
        if live.is_none() {
            transaction_restore_previous(&previous, destination, cache)?;
            transaction_cleanup(transaction, cache)?;
        } else {
            return Err(Error::Usage {
                message: "quarantined replacement conflicts with live file",
            });
        }
        return Ok(());
    }
    match record.phase.as_str() {
        "prepared" => {
            if staged.is_some() && live.is_none() {
                transaction_restore_previous(&previous, destination, cache)?;
            }
        }
        "quarantined" => {
            let published =
                record.operation == "replace" && live.is_some() && live == record.candidate;
            if !published && staged.is_some() && live.is_none() {
                transaction_restore_previous(&previous, destination, cache)?;
            }
        }
        _ => {} // `committed`: nothing left to decide.
    }
    transaction_cleanup(transaction, cache)
}

/// `_dot_file_transaction_prepare`: validate the gate, token, and live
/// generation, then journal a `prepared` record in a fresh init
/// directory, publish the directory at the transaction name, and (for
/// `replace`) stage the source as `candidate` with an identity
/// recheck. Every failure unwinds exactly what the shell unwinds.
pub fn transaction_prepare(
    source_root: &Path,
    lock: LockCtx,
    operation: &str,
    source: Option<&Path>,
    destination: &Path,
    expected_token: &str,
    cache: &mut MoveCache,
) -> Result<Prepared> {
    if !lock.valid() {
        return Err(Error::Usage {
            message: "file transactions need DOT_TEST=1 or a lock token",
        });
    }
    generation_validate(source_root, expected_token)?;
    let target = file_target_resolve(source_root, destination)?;
    transaction_recover(
        source_root,
        &target.path,
        &target.transaction,
        &target,
        cache,
    )?;
    if file_generation_raw(source_root, destination)? != expected_token {
        return Err(Error::Usage {
            message: "destination generation changed before prepare",
        });
    }
    let mut candidate: Option<Signature> = None;
    if operation == "replace" {
        let src = source.ok_or(Error::Usage {
            message: "replace needs a source",
        })?;
        let meta = std::fs::symlink_metadata(src).map_err(|source| Error::Io {
            context: "stat replace source",
            source,
        })?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(Error::Usage {
                message: "replace source is not a regular file",
            });
        }
        // The source must already live beside the transaction: only a
        // same-directory rename keeps the staging atomic, and binding
        // the parent blocks symlink redirection of the source.
        let src_target = file_target_resolve(source_root, src)?;
        if src_target.parent != target.parent {
            return Err(Error::Usage {
                message: "replace source is outside the destination directory",
            });
        }
        candidate = Some(file_signature(source_root, src)?);
    }
    let mut init_prefix = target
        .transaction
        .file_name()
        .ok_or(Error::Usage {
            message: "transaction has no file name",
        })?
        .to_os_string();
    init_prefix.push(".init.");
    let init = private_scratch_dir(&target.parent, &init_prefix)?;
    if let Err(source) = record_write(
        &init,
        operation,
        "prepared",
        expected_token,
        candidate.as_ref(),
        cache,
    ) {
        let _ = transaction_discard_private(&init);
        return Err(source);
    }
    if let Err(source) = move_noreplace_cached(&init, &target.transaction, cache) {
        let _ = transaction_discard_private(&init);
        return Err(source);
    }
    if operation == "replace" {
        let src = source.ok_or(Error::Usage {
            message: "replace needs a source",
        })?;
        let staged = target.transaction.join("candidate");
        if let Err(source) = move_noreplace_cached(src, &staged, cache) {
            let _ = transaction_cleanup(&target.transaction, cache);
            return Err(source);
        }
        let moved = file_signature(source_root, &staged)
            .map(|signature| signature.to_string())
            .unwrap_or_default();
        let wanted = candidate.as_ref().map(|signature| signature.to_string());
        if Some(moved) != wanted {
            let _ = move_noreplace_cached(&staged, src, cache);
            let _ = transaction_cleanup(&target.transaction, cache);
            return Err(Error::Usage {
                message: "staged candidate changed underfoot",
            });
        }
    }
    Ok(Prepared {
        transaction: target.transaction.clone(),
        destination: target.path.clone(),
        source: source.map(|path| path.to_path_buf()),
        operation: operation.to_string(),
        expected_token: expected_token.to_string(),
        candidate,
        target,
    })
}

/// `_dot_file_transaction_quarantine`: recheck the live generation,
/// then move the destination aside as `previous` (or assert absence)
/// and advance the journal. A racing writer fails the transaction and
/// sends the candidate home; the journal unwinds but the transaction
/// directory stays for an explicit retry, like the shell.
pub fn transaction_quarantine(
    source_root: &Path,
    prepared: &Prepared,
    cache: &mut MoveCache,
) -> Result<()> {
    let transaction = &prepared.transaction;
    if file_generation_raw(source_root, &prepared.destination)? != prepared.expected_token {
        if transaction.join("candidate").exists() {
            let source = prepared.source.as_ref().ok_or(Error::Usage {
                message: "staged candidate has no source",
            })?;
            move_noreplace_cached(&transaction.join("candidate"), source, cache)?;
        }
        transaction_cleanup(transaction, cache)?;
        return Err(Error::Usage {
            message: "destination changed before quarantine",
        });
    }
    let expected = generation_validate(source_root, &prepared.expected_token)?;
    if expected.state == "file" {
        let previous = transaction.join("previous");
        move_noreplace_cached(&prepared.destination, &previous, cache)?;
        let staged = file_signature(source_root, &previous)?.to_string();
        if Some(staged) != expected.signature {
            // A winner slipped in between: put the moved file back and
            // fail either way, with no cleanup — the operator retries.
            let _ = transaction_restore_previous(&previous, &prepared.destination, cache);
            return Err(Error::Usage {
                message: "destination changed during quarantine",
            });
        }
    } else if any_exists(&prepared.destination) {
        return Err(Error::Usage {
            message: "destination appeared during quarantine",
        });
    }
    record_write(
        transaction,
        &prepared.operation,
        "quarantined",
        &prepared.expected_token,
        prepared.candidate.as_ref(),
        cache,
    )
}

/// `_dot_commit_tmp_if_generation`: prepare, quarantine, publish the
/// candidate, journal `committed`, and retire the transaction. A
/// quarantine or publication failure runs recovery (best effort, like
/// the shell's `|| true`) before failing; a `committed`-journal
/// failure propagates with the transaction left in place.
pub fn commit_tmp_if_generation(
    source_root: &Path,
    lock: LockCtx,
    source: &Path,
    destination: &Path,
    expected_token: &str,
    cache: &mut MoveCache,
) -> Result<()> {
    let prepared = transaction_prepare(
        source_root,
        lock,
        "replace",
        Some(source),
        destination,
        expected_token,
        cache,
    )?;
    if transaction_quarantine(source_root, &prepared, cache).is_err() {
        let _ = transaction_recover(
            source_root,
            &prepared.destination,
            &prepared.transaction,
            &prepared.target,
            cache,
        );
        return Err(Error::Usage {
            message: "commit quarantine failed",
        });
    }
    let staged = prepared.transaction.join("candidate");
    if move_noreplace_cached(&staged, &prepared.destination, cache).is_err() {
        let _ = transaction_recover(
            source_root,
            &prepared.destination,
            &prepared.transaction,
            &prepared.target,
            cache,
        );
        return Err(Error::Usage {
            message: "commit publication failed",
        });
    }
    record_write(
        &prepared.transaction,
        "replace",
        "committed",
        &prepared.expected_token,
        prepared.candidate.as_ref(),
        cache,
    )?;
    transaction_cleanup(&prepared.transaction, cache)
}

/// `_dot_remove_if_generation`: the remove twin of
/// [`commit_tmp_if_generation`]. After quarantine the destination must
/// be gone (it was moved aside as `previous`); a resurrected name
/// fails with the transaction left staged, like the shell.
pub fn remove_if_generation(
    source_root: &Path,
    lock: LockCtx,
    destination: &Path,
    expected_token: &str,
    cache: &mut MoveCache,
) -> Result<()> {
    let prepared = transaction_prepare(
        source_root,
        lock,
        "remove",
        None,
        destination,
        expected_token,
        cache,
    )?;
    if transaction_quarantine(source_root, &prepared, cache).is_err() {
        let _ = transaction_recover(
            source_root,
            &prepared.destination,
            &prepared.transaction,
            &prepared.target,
            cache,
        );
        return Err(Error::Usage {
            message: "remove quarantine failed",
        });
    }
    if any_exists(&prepared.destination) {
        return Err(Error::Usage {
            message: "destination reappeared before remove commit",
        });
    }
    record_write(
        &prepared.transaction,
        "remove",
        "committed",
        &prepared.expected_token,
        None,
        cache,
    )?;
    transaction_cleanup(&prepared.transaction, cache)
}

/// `_dot_apply_git_metadata_modes`: clamp a whole tree to the umask
/// ceiling. The shell streams `find -print0`; the port walks
/// depth-first with per-directory sorted names instead of raw readdir
/// order, so repeated runs are deterministic. The success end state is
/// order-independent (every entry gets the same ceiling); like the
/// shell, the first unclampable entry aborts the walk.
pub fn apply_git_metadata_modes(root: &Path, mask: u32) -> Result<()> {
    let meta = std::fs::symlink_metadata(root).map_err(|source| Error::Io {
        context: "stat metadata root",
        source,
    })?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "metadata root is not a directory",
        });
    }
    apply_umask_ceiling(root, None, mask)?;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|source| Error::Io {
            context: "list metadata tree",
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                context: "list metadata tree",
                source,
            })?;
            names.push(entry.file_name());
        }
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let meta = std::fs::symlink_metadata(&path).map_err(|source| Error::Io {
                context: "stat metadata entry",
                source,
            })?;
            if meta.file_type().is_symlink() {
                return Err(Error::Usage {
                    message: "metadata tree holds a symlink",
                });
            }
            if meta.is_dir() {
                apply_umask_ceiling(&path, None, mask)?;
                stack.push(path);
            } else if meta.is_file() {
                apply_umask_ceiling(&path, None, mask)?;
            } else {
                return Err(Error::Usage {
                    message: "metadata tree holds a special file",
                });
            }
        }
    }
    Ok(())
}
