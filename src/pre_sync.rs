//! Client pre-sync extensions from `lib/dot/pre-sync.sh`.
//!
//! Ports `_dot_pre_sync_specs` (enumerate and validate the
//! `pre-sync.d` entry points) and `_run_pre_sync_extensions` (run
//! each entry point with a fresh one-use overlay context). The
//! shell's worker spawn (`_dot_extension_worker_run` from
//! `extension-worker-launch.sh`) belongs to a later slice, so
//! [`run`] takes the spawn as a caller-supplied [`Runner`]
//! closure; everything around it — stage gate, spec enumeration,
//! per-extension scratch directory plus `result` channel, context
//! creation, the failure warning, and break-on-first-failure — is
//! the port.
//!
//! Like the earlier ports the library never prints: spec identity
//! failures carry the exact `dot: ...` line the shell emits, and
//! message-less shell `return 1` paths surface as
//! [`Error::Refused`]. Collected warnings carry the
//! `  warning: ...` text the shell hands to `_warn`; the caller
//! renders them (for example with [`crate::log::Log::warn`]).
//!
//! Engine boundaries: extension paths cross into string logic via
//! lossy conversion (the `profiles` precedent), so non-UTF8 names
//! compare lossy where the shell compares raw bytes; directory
//! iteration skips dotfiles exactly like the shell glob (no
//! `dotglob`), and entries sort by raw filename bytes like
//! `LC_ALL=C` glob order.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::extension_trust::Inputs;

/// One enumerated extension: the sortable `key` (basename minus
/// `.sh`, minus `.serial`) and its script path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Sort key printed before the tab in the specs listing.
    pub key: String,
    /// Extension entry-point path (`$root/$filename`).
    pub script: PathBuf,
}

/// Pre-sync failure: `Usage` is a wrong stage (shell exit 2),
/// `Refused` a failed check (shell exit 1), and `Invalid` an
/// announced failure carrying the exact `dot: ...` line. Neither
/// silent variant prints; callers report their own warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong stage, silent (shell exit 2).
    Usage,
    /// Failed validation, silent (shell exit 1).
    Refused,
    /// Announced failure; carries the full `dot: ...` line.
    Invalid(String),
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(&self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Refused | Error::Invalid(_) => 1,
        }
    }
}

/// Fixed lifecycle stages `_run_pre_sync_extensions` accepts.
const STAGES: [&str; 2] = ["prepare", "reconcile"];

/// `_dot_extension_directory_validate`: the extensions-root
/// component walk plus a real-directory, never-a-symlink gate
/// with the directory stat check. There is no public
/// `extension_trust` entry for this composition, so it lives
/// here next to its only caller.
fn directory_validate(path: &Path, extensions_dir: &str, euid: u32) -> bool {
    if !crate::extension_trust::parent_components_validate(path, extensions_dir, euid) {
        return false;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    crate::extension_trust::directory_stat(path, euid)
}

/// Split `key` into its duplicate-detection identity:
/// an optional `[0-9]+[-_]` prefix is stripped, and the rest
/// must match `[a-z][a-z0-9-]*` exactly like the shell's
/// `^([0-9]+[-_])?([a-z][a-z0-9-]*)$` (byte-oriented under
/// `LC_ALL=C`; the ASCII-only checks below agree byte for
/// byte, and non-ASCII input fails the class either way).
fn identity_of(key: &str) -> Option<&str> {
    let bytes = key.as_bytes();
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let mut start = 0;
    if digits > 0 && digits < bytes.len() && (bytes[digits] == b'-' || bytes[digits] == b'_') {
        start = digits + 1;
    }
    // `start` only advances past ASCII bytes, so the slice
    // boundary is always a character boundary.
    let candidate = &key[start..];
    match candidate.bytes().next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return None,
    }
    if candidate
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Some(candidate)
    } else {
        None
    }
}

/// A failed [`specs`] call: the rows the shell had already
/// printed before the failure, plus the failure itself. The
/// shell streams each row as it validates, so a late identity
/// error leaves earlier rows on stdout; callers that capture
/// the listing (like [`run`]) discard them exactly like the
/// shell's command substitution does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecsError {
    /// Rows listed before the failure, in glob order.
    pub emitted: Vec<Spec>,
    /// The failure itself.
    pub error: Error,
}

/// `_dot_pre_sync_specs`: list the `pre-sync.d` entry points as
/// `key<TAB>script` rows in glob order. An unset extensions
/// directory, or a root that is neither present nor a symlink,
/// lists nothing (shell `return 0`); anything later that fails
/// refuses silently, except bad or duplicate identities, which
/// carry the shell's `dot: ...` line. `overlays` feeds entry
/// validation exactly like [`crate::extension_trust::file_validate`].
pub fn specs(inputs: &Inputs, overlays: &[String]) -> Result<Vec<Spec>, SpecsError> {
    if inputs.extensions_dir.is_empty() {
        return Ok(Vec::new());
    }
    // `root=${DOT_EXTENSIONS_DIR:-}/pre-sync.d`: the unset case
    // already returned above, so the suffix always anchors here.
    let root = format!("{}/pre-sync.d", inputs.extensions_dir);
    let root_path = Path::new(&root);
    // `[[ ! -e $root && ! -L $root ]]` reads through links for
    // `-e`, so a dangling symlink still enters validation and
    // refuses there.
    let present = root_path.exists()
        || std::fs::symlink_metadata(root_path).is_ok_and(|meta| meta.file_type().is_symlink());
    if !present {
        return Ok(Vec::new());
    }
    if !directory_validate(root_path, &inputs.extensions_dir, inputs.euid) {
        return Err(SpecsError {
            emitted: Vec::new(),
            error: Error::Refused,
        });
    }
    let map_refused = |emitted: Vec<Spec>| SpecsError {
        emitted,
        error: Error::Refused,
    };
    let entries = std::fs::read_dir(root_path).map_err(|_| map_refused(Vec::new()))?;
    let mut names: Vec<Vec<u8>> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| map_refused(Vec::new()))?;
        let name = entry.file_name();
        // The shell glob never sees dotfiles (no `dotglob`).
        if name.as_bytes().starts_with(b".") {
            continue;
        }
        if !name.as_bytes().ends_with(b".sh") {
            continue;
        }
        names.push(name.as_bytes().to_vec());
    }
    // Shell glob order under `LC_ALL=C` is raw byte order.
    names.sort();
    let mut seen: HashSet<String> = HashSet::new();
    let mut found = Vec::with_capacity(names.len());
    for raw in &names {
        let name = std::ffi::OsString::from_vec(raw.clone());
        let script = root_path.join(&name);
        if !crate::extension_trust::file_validate(&script, inputs, overlays) {
            return Err(map_refused(found));
        }
        let text = name.to_string_lossy().into_owned();
        // `${key%.sh}` then `${key%.serial}`, in that order.
        let without_sh = text.strip_suffix(".sh").unwrap_or(text.as_str());
        let key = without_sh
            .strip_suffix(".serial")
            .unwrap_or(without_sh)
            .to_string();
        let identity = match identity_of(&key) {
            Some(identity) => identity,
            None => {
                return Err(SpecsError {
                    emitted: found,
                    error: Error::Invalid(format!(
                        "dot: invalid pre-sync extension identity: {text}"
                    )),
                });
            }
        };
        if !seen.insert(identity.to_string()) {
            return Err(SpecsError {
                emitted: found,
                error: Error::Invalid(format!(
                    "dot: duplicate pre-sync extension identity: {identity}"
                )),
            });
        }
        found.push(Spec {
            key,
            script: PathBuf::from(format!("{root}/{text}")),
        });
    }
    Ok(found)
}

/// One worker invocation: everything
/// `_run_pre_sync_extensions` hands to
/// `_dot_extension_worker_run` (`mode` is always `"pre-sync"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// Spec key of the extension being run.
    pub key: String,
    /// Extension entry-point path.
    pub script: PathBuf,
    /// Fresh per-extension scratch directory (`$temporary`).
    pub temporary: PathBuf,
    /// Result channel (`$temporary/result`, mode 0600).
    pub result: PathBuf,
    /// One-use context path (`REPLY_PATH`).
    pub context: PathBuf,
    /// One-use context token (`REPLY_TOKEN`).
    pub token: String,
}

/// Worker spawn injected by the caller: `true` is worker exit 0,
/// any worker failure (any nonzero exit) is `false`.
pub type Runner<'a> = dyn FnMut(&Call) -> bool + 'a;

/// Outcome of [`run`]: the shell exit status plus the warning
/// texts the shell hands to `_warn` (rendered by the caller;
/// [`crate::log::Log::warn`] appends the newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Shell exit status: 0, or 1 after the first worker failure.
    pub status: i32,
    /// `  warning: pre-sync extension failed: {name}` per failure.
    pub warnings: Vec<String>,
}

/// Allocate a `mktemp -d`-style scratch directory under `root`
/// (`dot.` plus hex randomness, mode 0700). Like the shell
/// allocator the path is owned by the caller once published.
fn mktemp_dir(root: &Path) -> Result<PathBuf, Error> {
    use std::io::Read as _;
    for _ in 0..16 {
        let mut random = [0u8; 8];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut source| source.read_exact(&mut random))
            .is_err()
        {
            return Err(Error::Refused);
        }
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = root.join(format!("dot.{name}"));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                // `create_dir` honors the umask; `mktemp -d`
                // publishes 0700 regardless.
                if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).is_err()
                {
                    let _ = std::fs::remove_dir(&path);
                    return Err(Error::Refused);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(Error::Refused),
        }
    }
    Err(Error::Refused)
}

/// Basename of `path`, like `${script##*/`.
fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `_run_pre_sync_extensions`: run every enumerated extension
/// for `stage` (`prepare` or `reconcile`, else [`Error::Usage`])
/// with a fresh scratch directory, result channel, and one-use
/// `pre-sync`/`eligible` context. `records` are the
/// `name|path|url|descriptor|optional|sync` rows sealed into
/// each context; `scratch_root` is the `$TMPDIR` equivalent the
/// shell's `mktemp -d` allocates under; `now_secs` is the
/// `date +%s` instant.
///
/// A failing worker records its warning, removes its scratch
/// directory, and stops the run with status 1; a scratch
/// removal after a successful worker refuses instead (shell
/// `|| return 1`). Scratch or result allocation and context
/// creation failures leave the scratch directory registered
/// with the caller — the shell leaves those paths on its
/// cleanup registry for the same reason — and refuse.
#[allow(clippy::too_many_arguments)]
pub fn run(
    stage: &str,
    records: &[Vec<u8>],
    inputs: &Inputs,
    overlays: &[String],
    now_secs: i64,
    scratch_root: &Path,
    runner: &mut Runner<'_>,
) -> Result<Outcome, Error> {
    if !STAGES.contains(&stage) {
        return Err(Error::Usage);
    }
    // A captured listing discards partial rows on failure,
    // exactly like the shell's `specs=$(...) || return 1`.
    let found = specs(inputs, overlays).map_err(|failed| failed.error)?;
    if found.is_empty() {
        return Ok(Outcome {
            status: 0,
            warnings: Vec::new(),
        });
    }
    let mut warnings = Vec::new();
    let mut status = 0;
    for spec in &found {
        let temporary = mktemp_dir(scratch_root)?;
        let result = temporary.join("result");
        // `: >"$result"` plus `chmod 0600`, in either order the
        // end state is an empty 0600 channel.
        if std::fs::File::create(&result)
            .and_then(|_| std::fs::set_permissions(&result, std::fs::Permissions::from_mode(0o600)))
            .is_err()
        {
            return Err(Error::Refused);
        }
        let (context, token) = match crate::overlay_context::create(
            &temporary,
            "pre-sync",
            "eligible",
            stage,
            records,
            &inputs.home,
            inputs.euid,
            now_secs,
        ) {
            Ok(created) => created,
            Err(crate::overlay_context::Error::Invalid(message)) => {
                return Err(Error::Invalid(format!("dot: overlay context: {message}")));
            }
            Err(crate::overlay_context::Error::Refused) => return Err(Error::Refused),
        };
        let call = Call {
            key: spec.key.clone(),
            script: spec.script.clone(),
            temporary: temporary.clone(),
            result,
            context,
            token,
        };
        if !runner(&call) {
            warnings.push(format!(
                "  warning: pre-sync extension failed: {}",
                basename(&spec.script)
            ));
            let _ = std::fs::remove_dir_all(&temporary);
            status = 1;
            break;
        }
        if std::fs::remove_dir_all(&temporary).is_err() {
            return Err(Error::Refused);
        }
    }
    Ok(Outcome { status, warnings })
}
