//! Engine file-safety primitives: sibling temp files, stat and identity
//! probes, private-file validation, umask-bounded modes, sanitized Git,
//! content digests, and no-replace moves.
//!
//! Content hashes come from `git hash-object` (Git is already a required
//! engine dependency), moves go through the `mv` binary with a
//! `-nT`/`-nh` capability probe (GNU and BSD `mv` differ on late
//! directories), and the process umask is read without mutating process
//! state. Callers thread `source_root`, the umask, and a [`MoveCache`]
//! explicitly so tests can pin every knob without process-global
//! mutation. The hook-facing generation tokens and crash-safe file
//! transactions (`dot_file_generation` and friends) live in the public
//! hook runtime, `lib/dot/public/hook-runtime-v1/temp.sh`.
//!
//! Unix-only, like the engine itself: device/inode identities,
//! permission bits, and the umask have no portable spelling.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};

fn cancellation_checkpoint() -> Result<()> {
    crate::cancellation::check().map_err(|_| Error::Usage {
        message: "operation interrupted",
    })
}

/// `REPLY` capacity the shell never exceeds: sibling temps carry the
/// destination basename plus `.tmp.` plus six random characters.
const TMP_SUFFIX_LEN: usize = 6;
/// Retries for a colliding sibling-temp name before giving up; the
/// counter fallback below makes even one collision unlikely.
/// Crate-visible so the init transaction stage allocator shares the
/// exact retry budget instead of inventing a second one.
pub(crate) const TMP_RETRIES: usize = 100;

/// Current epoch seconds without launching `date`.
pub(crate) fn epoch_seconds() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Current local time in the shell's `%Y%m%d%H%M%S` shape.
///
/// `localtime_r` uses the process timezone database just like `date`, while
/// avoiding an unsupervised helper between cancellation checkpoints and a
/// durable publication.
pub(crate) fn local_timestamp() -> Option<String> {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    if now == -1 {
        return None;
    }
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers refer to live, correctly aligned objects for the
    // duration of the call. `localtime_r` initializes `local` on success.
    if unsafe { libc::localtime_r(&now, local.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: successful `localtime_r` initialized the value above.
    let local = unsafe { local.assume_init() };
    Some(format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    ))
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
    cancellation_checkpoint()?;
    std::fs::create_dir_all(dir).map_err(|source| Error::Io {
        context: "create sibling temp parent",
        source,
    })?;
    let mut prefix = base.to_os_string();
    prefix.push(".tmp.");
    for _ in 0..TMP_RETRIES {
        cancellation_checkpoint()?;
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
/// Crate-visible so the init transaction stage allocator draws from
/// the same mktemp alphabet instead of duplicating the generator.
pub(crate) fn random_suffix() -> String {
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

/// Current effective uid from the Unix process credentials.
pub fn current_uid() -> Option<u32> {
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    Some(unsafe { libc::geteuid() })
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

/// Parse the numeric form emitted by `/proc/self/status` and `sh -c umask`.
fn parse_umask(value: &str) -> Option<u32> {
    let value = value.trim_matches(|ch: char| ch.is_ascii_whitespace());
    if value.is_empty() || value.len() > 4 || !value.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
    {
        return None;
    }
    let mask = u32::from_str_radix(value, 8).ok()?;
    (mask <= 0o777).then_some(mask)
}

/// Read the engine process umask without mutating process-global state.
pub fn read_umask() -> Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(mask) = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:").and_then(parse_umask))
        {
            return Ok(mask);
        }
    }

    let shell = if Path::new("/bin/sh").is_file() {
        "/bin/sh"
    } else {
        "sh"
    };
    let mut command = std::process::Command::new(shell);
    command.args(["-c", "umask"]);
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Strict,
    )
    .map_err(|source| Error::Io {
        context: "read umask",
        source,
    })?;
    if !output.status.success() {
        return Err(Error::Usage {
            message: "cannot read umask",
        });
    }
    let value = String::from_utf8(output.stdout).map_err(|_| Error::Usage {
        message: "invalid umask output",
    })?;
    parse_umask(&value).ok_or(Error::Usage {
        message: "invalid umask output",
    })
}

#[cfg(test)]
mod umask_tests {
    use super::parse_umask;

    #[test]
    fn numeric_umask_parser_is_strict() {
        for (value, expected) in [
            ("0022", Some(0o022)),
            (" 0077\n", Some(0o077)),
            ("0", Some(0)),
            ("0777", Some(0o777)),
            ("", None),
            ("Umask:\t0022", None),
            ("00022", None),
            ("0788", None),
            ("1000", None),
            ("u=rwx,g=rx,o=rx", None),
        ] {
            assert_eq!(parse_umask(value), expected, "{value:?}");
        }
    }
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

/// `_dot_sanitized_git`: internal Git calls must not inherit a caller's
/// selected repository, object store, hash default, or configuration.
/// Builds the `git` command with that isolation boundary applied:
/// the unset list plus `GIT_CONFIG_NOSYSTEM=1`,
/// `GIT_CONFIG_GLOBAL=/dev/null`, and the `-c safe.directory=` /
/// `-C source_root` binding. `git` itself is the bound host Git
/// ([`crate::init_client_identity::host_git_command`]), falling back to
/// the engine PATH like the shell's `command git`.
pub fn sanitized_git<S: AsRef<std::ffi::OsStr>>(
    source_root: &Path,
    args: &[S],
) -> std::process::Command {
    let mut cmd = crate::init_client_identity::host_git_command();
    sanitize_git_env(&mut cmd);
    bind_source_git(&mut cmd, source_root);
    cmd.args(args);
    cmd
}

/// Apply `_dot_sanitized_git` environment isolation to an already-selected
/// Git executable. Runtime-bound callers use this form so executable lookup
/// remains tied to their immutable PATH without duplicating Git policy.
pub(crate) fn sanitize_git_env(cmd: &mut std::process::Command) {
    scrub_repository_selectors(cmd);
    const UNSET: &[&str] = &[
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_DEFAULT_HASH",
    ];
    for var in UNSET {
        cmd.env_remove(var);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
}

/// Git variables that point a command at a different repository, work tree,
/// index, or object store. A Git hook exports several of them (a pre-commit
/// hook sets `GIT_INDEX_FILE`), so a probe of some other checkout inherits a
/// foreign index unless they are removed.
const REPOSITORY_SELECTORS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
];

/// Remove only the [`REPOSITORY_SELECTORS`], keeping the user's Git
/// configuration (unlike [`sanitize_git_env`], which also isolates config):
/// read-only inspection of the user's own checkouts should honor their
/// `safe.directory`, fsmonitor, and similar settings.
pub(crate) fn scrub_repository_selectors(cmd: &mut std::process::Command) {
    for var in REPOSITORY_SELECTORS {
        cmd.env_remove(var);
    }
}

/// Bind one sanitized Git command to the selected source checkout, including
/// the explicit trust required when a container mount is host-owned.
pub(crate) fn bind_source_git(cmd: &mut std::process::Command, source_root: &Path) {
    let directory = source_root.as_os_str().as_bytes();
    let mut safe = b"safe.directory=".to_vec();
    safe.extend_from_slice(directory);
    cmd.arg("-c");
    // `Command::arg` takes `AsRef<OsStr>`; the byte-built `safe`
    // preserves non-UTF8 roots exactly.
    cmd.arg(std::ffi::OsStr::from_bytes(&safe));
    cmd.arg("-C");
    cmd.arg(source_root);
}

/// Run `git hash-object` under the sanitized binding; `stdin` feeds
/// `--stdin` when present. Returns the raw hash line. Crate-visible
/// for the `_overlay_replacement_hash_object` port, which is this
/// same `_dot_hash_object` boundary under its overlay name.
pub(crate) fn hash_object<S: AsRef<std::ffi::OsStr>>(
    source_root: &Path,
    args: &[S],
    stdin: Option<&[u8]>,
) -> Result<String> {
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
    let output = match stdin {
        Some(input) => crate::cleanup::run_session_output_with_input(
            cmd,
            input,
            None,
            crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
            crate::cleanup::LingerPolicy::Detach,
        ),
        None => crate::cleanup::run_session_output(
            cmd,
            None,
            crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
            crate::cleanup::LingerPolicy::Detach,
        ),
    }
    .map_err(|source| Error::Io {
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
    if crate::cancellation::check().is_err() {
        return false;
    }
    let mut command = std::process::Command::new(mv_bin);
    command
        .args(flags)
        .arg("--")
        .arg(source)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Plain file move, no descendants: Detach skips the host-wide completion
    // scan; cancellation still takes the strict path.
    crate::cleanup::run_session_status(command, crate::cleanup::LingerPolicy::Detach) == 0
}

/// Identity of a path for move verification: plain `stat` (never
/// `-L`) like `_dot_path_identity`, missing as `None` so a vanished
/// target compares unequal. No-follow matters both ways: a dangling
/// staged link still has an identity to verify by (the shell moves
/// those fine), and a late symlink reports its own identity, never
/// its target's.
fn move_identity(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    use std::os::unix::fs::MetadataExt as _;
    Some(identity_string((meta.dev(), meta.ino())))
}

/// `_dot_move_noreplace` with an explicit tool: publish `source` at an
/// absent `target` without replacing a late file, symlink, or empty
/// directory. BSD `mv` can briefly nest the source in a late
/// directory; exact inode recovery moves only that source back out,
/// and every shape still reports failure.
pub fn move_noreplace_with(source: &Path, target: &Path, tool: &MoveTool) -> Result<()> {
    cancellation_checkpoint()?;
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
        // Restoring the exact staged inode is cleanup, not new publication;
        // use a direct rename so a just-latched cancellation cannot prevent
        // un-nesting the failed move.
        let _ = std::fs::rename(&nested, source);
    }
    Err(Error::Usage {
        message: "move would replace an existing target",
    })
}

/// `_dot_move_replace_nodir` with an explicit tool: replace a known
/// engine-owned non-directory destination, with the same nesting
/// recovery as [`move_noreplace_with`].
pub fn move_replace_nodir_with(source: &Path, target: &Path, tool: &MoveTool) -> Result<()> {
    cancellation_checkpoint()?;
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
        let _ = std::fs::rename(&nested, source);
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

/// Unguarded `mkdir -p`: fork the same tool the shell pays for so
/// its diagnostics match byte for byte, forwarding stderr verbatim
/// to `warnings` while reporting success. Callers keep going after
/// a failure exactly like the shell does past a failed `mkdir`.
pub fn mkdir_forwarded(path: &Path, warnings: &mut dyn std::io::Write) -> bool {
    if crate::cancellation::check().is_err() {
        return false;
    }
    let mut command = std::process::Command::new("mkdir");
    command.arg("-p").arg(path);
    forward_mkdir(command, warnings)
}

/// Run a prepared `mkdir` command for [`mkdir_forwarded`]; split out so
/// tests can substitute a command that cannot launch or dies by signal.
fn forward_mkdir(command: std::process::Command, warnings: &mut dyn std::io::Write) -> bool {
    // Plain mkdir leaf, no descendants: Detach skips the host-wide completion
    // scan; cancellation still takes the strict path.
    let outcome = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
    );
    // Read the latch only for a death by signal, where the outcome itself
    // cannot say whether a handled signal caused it. An interrupted run
    // already reports `ErrorKind::Interrupted`.
    forward_mkdir_outcome(outcome, crate::cancellation::check().is_err(), warnings)
}

/// Report a forwarded `mkdir` outcome, leaving a diagnostic for every
/// failure.
///
/// The tool's own stderr stays byte-exact. A failure `mkdir` never got to
/// describe (the launch or its supervision failed, or the process died by
/// signal before writing) gets one `mkdir: <reason>` line instead of
/// vanishing. The shell leaves a line on stderr in those cases too: a
/// lookup or fork error, or a job-status report. Without it, a caller
/// that then fails closed (`backup_dir`) fails with no visible cause. A
/// handled signal owns the outcome, so an interrupted run stays silent:
/// supervision reports it as `ErrorKind::Interrupted`, and `latched` covers
/// a signal death the outcome cannot attribute. Classifying the error by
/// its kind avoids re-reading a latch that a concurrent owner may already
/// have reset.
fn forward_mkdir_outcome(
    outcome: std::io::Result<std::process::Output>,
    latched: bool,
    warnings: &mut dyn std::io::Write,
) -> bool {
    match outcome {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            if !output.stderr.is_empty() {
                let _ = warnings.write_all(&output.stderr);
            } else if !latched {
                let _ = writeln!(warnings, "mkdir: {}", output.status);
            }
            false
        }
        Err(error) => {
            if error.kind() != std::io::ErrorKind::Interrupted {
                let _ = writeln!(warnings, "mkdir: {error}");
            }
            false
        }
    }
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

/// Clamp a whole tree to the umask ceiling (the port of the retired
/// shell `_dot_apply_git_metadata_modes`). The shell streamed `find
/// -print0`; the port walks depth-first with per-directory sorted names
/// instead of raw readdir order, so repeated runs are deterministic. The
/// success end state is order-independent (every entry gets the same
/// ceiling); like the shell, the first unclampable entry aborts the walk.
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

#[cfg(test)]
mod tests {
    use super::*;
    use dot_test_support::TempDir;

    #[test]
    fn leaf_mkdir_runs_without_host_process_snapshot() {
        let dir = TempDir::new("leaf-mkdir-no-scan").expect("scratch");
        let target = dir.path().join("nested/dir");
        let mut warnings = Vec::new();
        crate::cleanup::reset_global_process_snapshot_calls();
        assert!(mkdir_forwarded(&target, &mut warnings));
        assert!(target.is_dir());
        assert_eq!(
            crate::cleanup::global_process_snapshot_calls(),
            0,
            "deterministic mkdir leaf must not pay a host-wide /proc walk"
        );
    }

    #[test]
    fn mkdir_launch_failure_leaves_a_diagnostic() {
        // A latched signal would legitimately silence the diagnostic.
        let _signals = crate::cleanup::hold_signal_ownership_for_test();
        let mut warnings = Vec::new();
        let command = std::process::Command::new("/definitely/missing/dot-mkdir");
        assert!(!forward_mkdir(command, &mut warnings));
        let text = String::from_utf8(warnings).expect("utf-8 diagnostic");
        assert!(text.starts_with("mkdir: "), "{text:?}");
        assert!(text.contains("os error 2"), "{text:?}");
        assert!(text.ends_with('\n'), "{text:?}");
    }

    #[test]
    fn mkdir_signal_death_leaves_a_diagnostic() {
        // Synthetic status: the engine's helpers must not spawn a shell
        // (tests/no-private-engine-test), and a death by signal is all
        // this branch needs.
        use std::os::unix::process::ExitStatusExt as _;
        let mut warnings = Vec::new();
        let killed = std::process::Output {
            status: std::process::ExitStatus::from_raw(libc::SIGKILL),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!forward_mkdir_outcome(Ok(killed), false, &mut warnings));
        let text = String::from_utf8(warnings).expect("utf-8 diagnostic");
        assert!(text.starts_with("mkdir: signal: 9"), "{text:?}");
        assert!(text.ends_with('\n'), "{text:?}");
    }

    #[test]
    fn mkdir_stderr_is_forwarded_verbatim_without_a_second_line() {
        use std::os::unix::process::ExitStatusExt as _;
        let mut warnings = Vec::new();
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"mkdir: x: Not a directory\n".to_vec(),
        };
        assert!(!forward_mkdir_outcome(Ok(output), false, &mut warnings));
        assert_eq!(warnings, b"mkdir: x: Not a directory\n");
    }

    #[test]
    fn interrupted_mkdir_stays_silent() {
        use std::os::unix::process::ExitStatusExt as _;
        let mut warnings = Vec::new();
        let interrupted = std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "subprocess interrupted by signal",
        );
        // The error kind alone silences it, even with the latch already reset.
        assert!(!forward_mkdir_outcome(
            Err(interrupted),
            false,
            &mut warnings
        ));
        let killed = std::process::Output {
            status: std::process::ExitStatus::from_raw(libc::SIGTERM),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!forward_mkdir_outcome(Ok(killed), true, &mut warnings));
        assert!(warnings.is_empty());
    }

    #[test]
    fn leaf_move_runs_without_host_process_snapshot() {
        let mv = resolve_mv().expect("test requires mv on PATH");
        let dir = TempDir::new("leaf-move-no-scan").expect("scratch");
        let source = dir.path().join("source.txt");
        let target = dir.path().join("target.txt");
        std::fs::write(&source, b"leaf").expect("fixture");
        crate::cleanup::reset_global_process_snapshot_calls();
        assert!(run_mv(&mv, &[], &source, &target));
        assert!(target.is_file());
        assert_eq!(
            crate::cleanup::global_process_snapshot_calls(),
            0,
            "deterministic leaf move must not pay a host-wide /proc walk"
        );
    }
}
