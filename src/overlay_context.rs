//! One-use authorization contexts for isolated workers (slice 9).
//!
//! Ports `lib/dot/overlay-context.sh`: the shared field gate, path
//! and record validators, the mode/set/stage matrix, random tokens,
//! and NUL-framed context file creation and single-use consumption
//! with open-descriptor TOCTOU checks.
//!
//! Like the earlier ports the library never prints: failures carry
//! the message the shell emits after `dot: overlay context: `, and
//! message-less shell `return 1` paths surface as
//! [`Error::Refused`]. `stat`-based identity uses
//! [`std::os::unix::fs::MetadataExt`] instead of shelling out to
//! GNU/BSD `stat`, which keeps the macOS/Linux split portable.
//! `File::metadata` is `fstat` on the open description, so it is the
//! direct equivalent of the shell's `/proc/self/fd` re-stat —
//! strictly stronger than re-resolving the pathname.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};

/// `_DOT_OVERLAY_CONTEXT_MAGIC`.
pub const MAGIC: &str = "DOT_OVERLAY_CONTEXT";
/// `_DOT_OVERLAY_CONTEXT_VERSION`.
pub const VERSION: &str = "1";
/// `_DOT_OVERLAY_CONTEXT_MAX_BYTES`.
pub const MAX_BYTES: u64 = 1048576;
/// `_DOT_OVERLAY_CONTEXT_MAX_RECORDS`.
pub const MAX_RECORDS: usize = 256;

/// Overlay-context failure (the shell uses exit 1 throughout, and
/// exit 2 only for wrong arity, which the typed signatures make
/// unrepresentable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Message-less refusal (bare shell `return 1`).
    Refused,
    /// Announced failure; carries the text after
    /// `dot: overlay context: `.
    Invalid(String),
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(&self) -> i32 {
        1
    }
}

/// `_dot_overlay_field_safe`: no record delimiter, C0 control byte,
/// or DEL — plus the `od -An -t u1 | awk` repeat-marker
/// fail-closed quirk shared with the descriptor scan (any two
/// consecutive identical 16-byte chunks reject, exactly like the
/// shell's accidental strictness). Bash cannot hold NUL, so the
/// decoder validates that byte separately as structure; as a
/// standalone gate NUL rejects via the C0 rule.
pub fn field_safe(value: &[u8]) -> bool {
    if value
        .iter()
        .any(|byte| *byte == b'|' || *byte < 32 || *byte == 127)
    {
        return false;
    }
    let mut offset = 0;
    while offset + 32 <= value.len() {
        if value[offset..offset + 16] == value[offset + 16..offset + 32] {
            return false;
        }
        offset += 16;
    }
    true
}

/// Owner/mode/nlink/device/inode identity from `fstat`-style
/// metadata: `(uid, mode & 0o7777, nlink, dev, ino)`. The shell's
/// octal-digit check on `stat` output is upheld by construction —
/// the typed API cannot produce non-octal modes.
fn identity(meta: &std::fs::Metadata) -> (u32, u32, u64, u64, u64) {
    (
        meta.uid(),
        meta.mode() & 0o7777,
        meta.nlink(),
        meta.dev(),
        meta.ino(),
    )
}

/// `_dot_overlay_context_directory_safe`: a real directory, never a
/// symlink, owned by us with no group/other permission bits.
pub fn directory_safe(path: &Path, euid: u32) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    let (uid, mode, _, _, _) = identity(&meta);
    uid == euid && mode & 0o077 == 0
}

/// `_dot_overlay_context_file_safe`: a regular file, never a
/// symlink, owned by us at exactly mode 600 with one link, within
/// the size bound and written in the freshness window
/// (`mtime <= now + 5 && now - mtime <= 300`).
pub fn file_safe(path: &Path, euid: u32, now_secs: i64) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    let (uid, mode, links, _, _) = identity(&meta);
    if uid != euid || mode != 0o600 || links != 1 {
        return false;
    }
    if meta.size() > MAX_BYTES {
        return false;
    }
    let mtime = match meta.modified().ok().and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|span| i64::try_from(span.as_secs()).ok())
    }) {
        Some(mtime) => mtime,
        None => return false,
    };
    mtime <= now_secs + 5 && now_secs - mtime <= 300
}

/// Device/ino/link/uid report from the system `lsof`
/// (`-a -p PID -d FD -FDiku`), like `_dot_overlay_context_lsof`.
/// Returns `(dev, ino, links, uid)`.
fn lsof_report(fd: i32) -> Option<(u64, u64, u64, u32)> {
    let pid = std::process::id();
    let mut output = None;
    for bin in ["/usr/sbin/lsof", "/usr/bin/lsof"] {
        let executable = std::fs::symlink_metadata(bin)
            .map(|meta| !meta.is_dir() && meta.mode() & 0o111 != 0)
            .unwrap_or(false);
        if !executable {
            continue;
        }
        output = std::process::Command::new(bin)
            .args([
                "-a",
                "-p",
                &pid.to_string(),
                "-d",
                &fd.to_string(),
                "-FDiku",
            ])
            .output()
            .ok()
            .filter(|result| result.status.success())
            .map(|result| result.stdout);
        // The shell returns after the first usable binary whether or
        // not the query itself succeeded.
        break;
    }
    let output = output?;
    let mut dev = None;
    let mut ino = None;
    let mut links = None;
    let mut uid = None;
    for line in output.split(|byte| *byte == b'\n') {
        let (kind, value) = match line.split_first() {
            Some(split) => split,
            None => continue,
        };
        match kind {
            b'D' => dev = parse_dev(value),
            b'i' => ino = parse_uint(value),
            b'k' => links = parse_uint(value),
            b'u' => uid = parse_uint(value).and_then(|parsed| u32::try_from(parsed).ok()),
            _ => {}
        }
    }
    Some((dev?, ino?, links?, uid?))
}

/// Parse an unsigned decimal (`%d`-formatted device numbers may
/// arrive hex-prefixed, which the shell's `printf %d` accepts).
fn parse_dev(raw: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        parse_uint(raw)
    }
}

/// Parse an unsigned decimal field.
fn parse_uint(raw: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `_dot_overlay_context_open_file_stat`: identity of an
/// already-open descriptor. `fstat` on the handle is the direct
/// equivalent of the shell's `/proc/self/fd` re-stat; when it
/// disagrees (Darwin devfs synthetic metadata), the `lsof`
/// fallback carries forward the verified 0600 mode exactly like
/// the shell. Returns `(uid, mode, links, dev, ino)`.
fn open_file_stat(
    file: &std::fs::File,
    expected_dev: u64,
    expected_ino: u64,
    euid: u32,
) -> Option<(u32, u32, u64, u64, u64)> {
    if let Ok(meta) = file.metadata() {
        let (uid, mode, links, dev, ino) = identity(&meta);
        if uid == euid && mode == 0o600 && dev == expected_dev && ino == expected_ino {
            return Some((uid, mode, links, dev, ino));
        }
    }
    let (dev, ino, links, uid) = lsof_report(file.as_raw_fd())?;
    if uid == euid && dev == expected_dev && ino == expected_ino {
        return Some((uid, 0o600, links, dev, ino));
    }
    None
}

/// `_dot_overlay_context_absolute_canonical`: an absolute,
/// normalized record path — lexically validated only, never
/// resolved.
pub fn absolute_canonical(path: &[u8]) -> bool {
    if !field_safe(path) || path.is_empty() {
        return false;
    }
    if path == b"/" || !path.starts_with(b"/") || path.ends_with(b"/") {
        return false;
    }
    if path.windows(2).any(|pair| pair == b"//") {
        return false;
    }
    for marker in [b"/./".as_slice(), b"/../".as_slice()] {
        if path.windows(marker.len()).any(|window| window == marker) {
            return false;
        }
    }
    if path.ends_with(b"/.") || path.ends_with(b"/..") {
        return false;
    }
    true
}

/// `_dot_overlay_record_validate`: exactly six `|`-separated
/// fields with a consistent name, normalized paths, a matching
/// descriptor identity, and a coherent optional/sync shape.
/// `home` anchors the git-descriptor home convention.
pub fn record_validate(record: &[u8], home: &str) -> bool {
    let fields: Vec<&[u8]> = record.split(|byte| *byte == b'|').collect();
    if fields.len() != 6 {
        return false;
    }
    let [name, path, url, descriptor, optional, sync] = fields[..] else {
        return false;
    };
    if sync.is_empty() {
        return false;
    }
    if !(field_safe(name) && field_safe(url) && field_safe(optional) && field_safe(sync)) {
        return false;
    }
    if name.is_empty() || !name[0].is_ascii_lowercase() {
        return false;
    }
    if !name
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        || name == b"dotfiles"
    {
        return false;
    }
    if !absolute_canonical(path) || !absolute_canonical(descriptor) {
        return false;
    }
    if !descriptor.ends_with(b".conf") {
        return false;
    }
    let mut stem = descriptor
        .strip_suffix(b".conf".as_slice())
        .unwrap_or(descriptor);
    if let Some(rest) = stem.strip_suffix(b".local".as_slice()) {
        stem = rest;
    }
    let base = stem.rsplit(|byte| *byte == b'/').next().unwrap_or(stem);
    let stemmed = match base.iter().position(|byte| *byte == b'-') {
        Some(dash) if dash > 0 && base[..dash].iter().all(|byte| byte.is_ascii_digit()) => {
            &base[dash + 1..]
        }
        _ => base,
    };
    if stemmed.is_empty() || stemmed != name {
        return false;
    }
    if optional != b"true" && optional != b"false" {
        return false;
    }
    match sync {
        b"git" => {
            if url.is_empty() {
                return false;
            }
            let expected = format!("{home}/.dotfiles-{}", String::from_utf8_lossy(name));
            path == expected.as_bytes()
        }
        b"none" => url.is_empty() && optional == b"false",
        _ => false,
    }
}

/// `_dot_overlay_context_matrix_valid`: the exact
/// mode/set/stage triple table.
pub fn matrix_valid(mode: &str, set_kind: &str, stage: &str) -> bool {
    matches!(
        (mode, set_kind, stage),
        ("pre-sync", "eligible", "prepare")
            | ("pre-sync", "eligible", "reconcile")
            | ("merge", "active", "none")
            | ("deactivate", "retiring", "none")
            | ("doctor", "active", "none")
    )
}

/// `_dot_overlay_context_token`: 32 random bytes as 64 lowercase
/// hex digits, read from `/dev/urandom` like the shell's `od`.
pub fn token() -> Option<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut bytes)
        .ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A consumed context: the decoded `OVERLAYS` records plus the
/// published `REPLY_SET_KIND` / `REPLY_STAGE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// Decoded `name|path|url|descriptor|optional|sync` records.
    pub records: Vec<String>,
    /// `REPLY_SET_KIND`.
    pub set_kind: String,
    /// `REPLY_STAGE`.
    pub stage: String,
}

/// `_dot_overlay_context_create`: validate the directory, triple,
/// and records, then write the NUL-framed file and re-verify it.
/// Returns the context path and token (`REPLY_PATH` /
/// `REPLY_TOKEN`). Any failure removes a partially written file.
#[allow(clippy::too_many_arguments)]
pub fn create(
    directory: &Path,
    mode: &str,
    set_kind: &str,
    stage: &str,
    records: &[Vec<u8>],
    home: &str,
    euid: u32,
    now_secs: i64,
) -> Result<(PathBuf, String), Error> {
    let dir_text = String::from_utf8_lossy(directory.as_os_str().as_bytes()).into_owned();
    if !dir_text.starts_with('/') {
        return Err(Error::Invalid(format!(
            "context directory is not absolute: {dir_text}"
        )));
    }
    if !directory_safe(directory, euid) {
        return Err(Error::Invalid(format!(
            "unsafe context directory: {dir_text}"
        )));
    }
    if !matrix_valid(mode, set_kind, stage) {
        return Err(Error::Invalid(format!(
            "invalid mode/set/stage: {mode}/{set_kind}/{stage}"
        )));
    }
    if records.len() > MAX_RECORDS {
        return Err(Error::Invalid("too many overlay records".to_string()));
    }
    let mut seen = HashSet::new();
    for record in records {
        if !record_validate(record, home) {
            return Err(Error::Invalid("invalid overlay record".to_string()));
        }
        let name = record.split(|byte| *byte == b'|').next().unwrap_or(b"");
        if !seen.insert(name.to_vec()) {
            return Err(Error::Invalid(format!(
                "duplicate overlay record: {}",
                String::from_utf8_lossy(name)
            )));
        }
    }
    let token = token()
        .filter(|token| is_token(token))
        .ok_or(Error::Refused)?;
    let path = stage_file(directory)?;
    let mut body = Vec::new();
    for field in [MAGIC, VERSION, token.as_str(), mode, set_kind, stage] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    let count = records.len().to_string();
    body.extend_from_slice(count.as_bytes());
    body.push(0);
    for record in records {
        for field in record.split(|byte| *byte == b'|') {
            body.extend_from_slice(field);
            body.push(0);
        }
    }
    let written = std::fs::write(&path, &body)
        .ok()
        .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok())
        .and_then(|()| file_safe(&path, euid, now_secs).then_some(()));
    if written.is_none() {
        let _ = std::fs::remove_file(&path);
        return Err(Error::Refused);
    }
    Ok((path, token))
}

/// Whether `token` matches the `^[0-9a-f]{64}$` gate the shell
/// applies to generated and presented tokens alike.
fn is_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Allocate the context file (`mktemp` in the directory).
/// The shell relies on `mktemp` randomness; retry `create_new`
/// with fresh `/dev/urandom` suffixes instead.
fn stage_file(directory: &Path) -> Result<PathBuf, Error> {
    use std::io::Read as _;
    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        if std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut random| random.read_exact(&mut suffix).ok())
            .is_none()
        {
            return Err(Error::Refused);
        }
        let name: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = directory.join(format!(".dot-overlay-context.{name}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(Error::Refused),
        }
    }
    Err(Error::Refused)
}

/// `_dot_overlay_context_consume`: single-use decoding with the
/// full open-descriptor TOCTOU sequence — verify, bind, re-verify,
/// unlink before parsing the already-bound handle, then decode the
/// NUL frame. `now_secs` is the `date +%s` instant.
pub fn consume(
    context: &Path,
    token: &str,
    expected_mode: &str,
    home: &str,
    euid: u32,
    now_secs: i64,
) -> Result<Decoded, Error> {
    use std::io::Read as _;
    let text = context.as_os_str().as_bytes();
    if !text.starts_with(b"/") {
        return Err(Error::Refused);
    }
    let parent = context.parent().ok_or(Error::Refused)?;
    if !directory_safe(parent, euid) {
        return Err(Error::Refused);
    }
    if !file_safe(context, euid, now_secs) {
        return Err(Error::Refused);
    }
    let (_, _, _, path_dev, path_ino) = path_identity(context).ok_or(Error::Refused)?;
    let file = std::fs::File::open(context).map_err(|_| Error::Refused)?;
    if !file_safe(context, euid, now_secs) {
        return Err(Error::Refused);
    }
    let (_, _, _, again_dev, again_ino) = path_identity(context).ok_or(Error::Refused)?;
    if (again_dev, again_ino) != (path_dev, path_ino) {
        return Err(Error::Refused);
    }
    let (_, _, links, _, _) =
        open_file_stat(&file, path_dev, path_ino, euid).ok_or(Error::Refused)?;
    if links != 1 {
        return Err(Error::Refused);
    }
    // Remove the pathname before parsing the already-bound
    // descriptor so replacement or reuse cannot grant authority.
    std::fs::remove_file(context).map_err(|_| Error::Refused)?;
    let (_, _, unlinked, _, _) =
        open_file_stat(&file, path_dev, path_ino, euid).ok_or(Error::Refused)?;
    if unlinked != 0 {
        return Err(Error::Refused);
    }
    let mut body = Vec::new();
    file.take(MAX_BYTES + 2)
        .read_to_end(&mut body)
        .map_err(|_| Error::Refused)?;
    // `read -d ''` leaves a nonempty final field when the frame is
    // not NUL-terminated; an empty file simply fails the count
    // gate below.
    if !body.ends_with(b"\0") && !body.is_empty() {
        return Err(Error::Refused);
    }
    let mut fields: Vec<&[u8]> = body.split(|byte| *byte == 0).collect();
    if body.ends_with(b"\0") {
        fields.pop();
    }
    if fields.len() < 7 {
        return Err(Error::Refused);
    }
    if fields[0] != MAGIC.as_bytes()
        || fields[1] != VERSION.as_bytes()
        || fields[2] != token.as_bytes()
        || fields[3] != expected_mode.as_bytes()
    {
        return Err(Error::Refused);
    }
    let set_kind = String::from_utf8_lossy(fields[4]);
    let stage = String::from_utf8_lossy(fields[5]);
    let count_text = String::from_utf8_lossy(fields[6]);
    if !is_token(token) || !is_count(&count_text) {
        return Err(Error::Refused);
    }
    let count: usize = count_text.parse().map_err(|_| Error::Refused)?;
    if count > MAX_RECORDS {
        return Err(Error::Refused);
    }
    // `fields[3]` was verified byte-equal to `expected_mode`
    // above; either spelling checks the same triple.
    if !matrix_valid(expected_mode, &set_kind, &stage) {
        return Err(Error::Refused);
    }
    if fields.len() != 7 + count * 6 {
        return Err(Error::Refused);
    }
    let mut seen = HashSet::new();
    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        let window = &fields[7 + index * 6..7 + index * 6 + 6];
        let mut record = Vec::new();
        for (position, field) in window.iter().enumerate() {
            if position > 0 {
                record.push(b'|');
            }
            record.extend_from_slice(field);
        }
        if !record_validate(&record, home) {
            return Err(Error::Refused);
        }
        let name = window[0].to_vec();
        if !seen.insert(name) {
            return Err(Error::Refused);
        }
        records.push(String::from_utf8_lossy(&record).into_owned());
    }
    Ok(Decoded {
        records,
        set_kind: set_kind.into_owned(),
        stage: stage.into_owned(),
    })
}

/// `^(0|[1-9][0-9]*)$` with no leading zeros.
fn is_count(text: &str) -> bool {
    if text == "0" {
        return true;
    }
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) && !text.starts_with('0')
}

/// Path identity `(dev, ino)` following links, like
/// `stat -L` on `/proc/self/fd`.
fn path_identity(path: &Path) -> Option<(u32, u32, u64, u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some(identity(&meta))
}
