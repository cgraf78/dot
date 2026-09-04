//! Profile lifecycle ledger (`lib/dot/profile-lifecycle.sh`).
//!
//! Persists and retires overlay-owned side effects across profile
//! changes: the ledger file lists the overlays whose
//! `profile-deactivate` entry point may still need to run,
//! [`prepare`] refreshes that list from the eligible/current sets
//! before a sync, [`retire`] runs the entry points the new profile
//! drops, and [`commit`] records the survivors for next time.
//!
//! The worker spawn itself (`_dot_extension_worker_run`) belongs to
//! a later slice, so [`run_one`] takes execution as a [`WorkerRun`]
//! seam: the ported plumbing (script resolution, scratch directory,
//! authorization context, exit-code/output relay, warning routing,
//! cleanup) is exact, and only the leaf process spawn is injected.
//! Differential tests inject the live shell worker there, so the
//! comparison still covers everything this module owns.
//!
//! Like the earlier ports the library never prints: `_warn` lines go
//! to the caller's `warnings` buffer through [`Log::warn`] (which
//! reproduces the color gate), and `_log` output goes to `out`
//! through [`Log::log`] (which reproduces the quiet gate). The shell
//! array globals arrive as explicit slices, and the shell
//! environment predicates (`DOT_PROFILES_PRESENT`,
//! `_dot_extensions_enabled`, `DOT_VERBOSE`) arrive as resolved
//! booleans, so tests inject fixtures deterministically.
//!
//! Two deliberate determinism choices where the shell is vague: the
//! shell iterates its `retained`/`current` maps in hash order when
//! hunting for the first failure, so multi-fault inputs report a
//! random record first; the port scans ledger order (then sorted
//! name order for the current set) and documents the single-fault
//! rows that keep both sides on the same record. And the shell
//! `mkdir -p` under `umask 077` becomes explicit `0o700` directory
//! creation, since the library cannot set the process umask.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::log::Log;

/// Ledger size bound in bytes: `_dot_profile_lifecycle_file_safe`
/// rejects anything larger than 1 MiB.
pub const MAX_LEDGER_BYTES: u64 = 1048576;

/// Context triple [`run_one`] mints per deactivation, like the
/// shell's `_dot_overlay_context_create "$result_dir" deactivate
/// retiring none "$record"`.
const CONTEXT_MODE: &str = "deactivate";
/// Set kind half of the deactivation context triple.
const CONTEXT_SET_KIND: &str = "retiring";
/// Stage half of the deactivation context triple.
const CONTEXT_STAGE: &str = "none";

/// Script-resolution failure for [`deactivation_script`]: `Missing`
/// is the absent entry point (shell exit 2), `Refused` a failed
/// check (shell exit 1). Neither prints; callers report their own
/// warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptError {
    /// Failed validation, silent (shell exit 1).
    Refused,
    /// No entry point at the fixed spelling, silent (shell exit 2).
    Missing,
}

impl ScriptError {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            ScriptError::Refused => 1,
            ScriptError::Missing => 2,
        }
    }
}

/// Outcome of one worker execution: exit code plus combined
/// stdout/stderr bytes, like the shell's `output=$(... 2>&1)` with
/// `$?` alongside. The bytes are raw; [`run_one`] strips trailing
/// newlines the way command substitution does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOutcome {
    /// Worker exit code.
    pub rc: i32,
    /// Combined stdout/stderr bytes.
    pub output: Vec<u8>,
}

/// Executes one validated deactivation script under the worker
/// protocol: the `_dot_extension_worker_run` leaf that belongs to a
/// later slice. Arguments mirror its call in [`run_one`]: the fixed
/// entry-point script, the scratch directory, the `has-deactivate`
/// result file inside it, and the minted context path plus token.
pub trait WorkerRun {
    /// Run `script` and return its exit code with combined output.
    fn run(
        &mut self,
        script: &Path,
        result_dir: &Path,
        result_file: &Path,
        context: &Path,
        token: &str,
    ) -> WorkerOutcome;
}

/// `_dot_profile_lifecycle_file_safe`: an owned regular file, never
/// a symlink, with no group/other permission bits, exactly one
/// link, within [`MAX_LEDGER_BYTES`]. Silent on both sides; the
/// caller warns. The shell spells this through
/// `_overlay_private_regular_file` plus `wc -c`; the typed `stat`
/// fields uphold the octal-digit check by construction, and
/// `metadata.len` reads the size without consuming the file.
pub fn file_safe(path: &Path, euid: u32) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    if meta.uid() != euid {
        return false;
    }
    if meta.mode() & 0o7777 & 0o077 != 0 {
        return false;
    }
    if meta.nlink() != 1 {
        return false;
    }
    meta.len() <= MAX_LEDGER_BYTES
}

/// Split ledger bytes the way `while IFS= read -r` sees them:
/// `\n`-separated with `\r` preserved, a missing final newline
/// still yielding its line, and a single trailing newline NOT
/// yielding an extra empty line. An empty file yields zero lines
/// (the shell loop body never runs), which the caller reports as
/// an empty ledger rather than a malformed one.
fn ledger_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

/// `_dot_profile_lifecycle_load`: read the ledger into
/// `records`. `None` (or an empty path) is the unset
/// `DOT_PROFILE_LIFECYCLE_LEDGER` and fails silently like the
/// shell's `[[ -n $ledger ]] || return 1`. A missing ledger is a
/// clean empty list; every other failure warns through `log` and
/// returns `false`. Records validate through
/// [`crate::overlay_context::record_validate`] anchored at `home`,
/// and duplicates reject on the record name (`${line%%|*}`).
///
/// Like the shell (which clears `DOT_PROFILE_LIFECYCLE_RECORDS`
/// first and appends line by line), `records` is cleared on entry
/// and a failure keeps whatever validated before it — callers
/// that only need the success shape read the return value.
pub fn load(
    ledger: Option<&Path>,
    home: &str,
    euid: u32,
    log: &Log,
    warnings: &mut dyn std::io::Write,
    records: &mut Vec<String>,
) -> bool {
    records.clear();
    let ledger = match ledger {
        Some(path) if !path.as_os_str().is_empty() => path,
        _ => return false,
    };
    let display = ledger.display().to_string();
    if std::fs::symlink_metadata(ledger).is_err() {
        return true;
    }
    if !file_safe(ledger, euid) {
        log.warn(
            warnings,
            &format!("  warning: unsafe profile lifecycle ledger: {display}"),
        );
        return false;
    }
    let bytes = match std::fs::read(ledger) {
        Ok(bytes) => bytes,
        Err(_) => {
            log.warn(
                warnings,
                &format!("  warning: empty profile lifecycle ledger: {display}"),
            );
            return false;
        }
    };
    let lines = ledger_lines(&bytes);
    let mut lines = lines.iter();
    match lines.next() {
        Some(first) if *first == b"version=1" => (),
        _ => {
            let which = if bytes.is_empty() {
                "empty"
            } else {
                "unsupported"
            };
            log.warn(
                warnings,
                &format!("  warning: {which} profile lifecycle ledger: {display}"),
            );
            return false;
        }
    }
    let mut seen = HashSet::new();
    for line in lines {
        if line.is_empty() {
            log.warn(
                warnings,
                &format!("  warning: malformed profile lifecycle ledger: {display}"),
            );
            return false;
        }
        if !crate::overlay_context::record_validate(line, home) {
            log.warn(warnings, "  warning: invalid profile lifecycle record");
            return false;
        }
        let name = line.split(|byte| *byte == b'|').next().unwrap_or(b"");
        if !seen.insert(name.to_vec()) {
            log.warn(
                warnings,
                &format!(
                    "  warning: duplicate profile lifecycle record: {}",
                    String::from_utf8_lossy(name)
                ),
            );
            return false;
        }
        // Records passed the ASCII field gates, so lossy text is
        // exact (the `profiles` precedent for string boundaries).
        records.push(String::from_utf8_lossy(line).into_owned());
    }
    true
}

/// Ensure `path` itself is a directory, creating every missing
/// ancestor. Missing levels arrive with mode `0o700`
/// (the shell's `umask 077 && mkdir -p`); pre-existing levels keep
/// their modes, and the caller gates them with
/// [`crate::overlay_context::directory_safe`].
fn mkdir_private_all(path: &Path) -> bool {
    let mut missing: Vec<&Path> = Vec::new();
    let mut current = path;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(_) => break,
            Err(_) => {
                missing.push(current);
                match current.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => current = parent,
                    _ => return false,
                }
            }
        }
    }
    for dir in missing.iter().rev() {
        if std::fs::create_dir(dir).is_err() {
            return false;
        }
        if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err() {
            return false;
        }
    }
    true
}

/// Allocate a fresh file inside `directory` (`mktemp` with a random
/// suffix, `create_new` retried like
/// [`crate::overlay_context`]'s staging). The caller owns the mode.
fn mktemp_file(directory: &Path, prefix: &str) -> Option<PathBuf> {
    use std::io::Read as _;
    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut random| random.read_exact(&mut suffix).ok())?;
        let name: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = directory.join(format!("{prefix}.{name}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// `_dot_profile_lifecycle_write`: atomically replace the ledger
/// with `version=1` plus `records`. A missing parent chain is
/// created private (mode `0o700` per level), the body lands in a
/// `0o600` staging file, and `rename` publishes it, so readers
/// never see a partial ledger. Silent on both sides (`true` is
/// shell exit 0). The ledger must name a file inside a directory
/// part; a bare filename fails closed where the shell would
/// mistake the name for a directory.
pub fn write(ledger: &Path, records: &[String], euid: u32) -> bool {
    if ledger.as_os_str().is_empty() {
        return false;
    }
    let directory = match ledger.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => return false,
    };
    if std::fs::symlink_metadata(directory).is_err() && !mkdir_private_all(directory) {
        return false;
    }
    if !crate::overlay_context::directory_safe(directory, euid) {
        return false;
    }
    let Some(temporary) = mktemp_file(directory, ".profile-overlay-lifecycle") else {
        return false;
    };
    let failed = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .is_err()
        || {
            let mut body = Vec::from(b"version=1\n".as_slice());
            for record in records {
                body.extend_from_slice(record.as_bytes());
                body.push(b'\n');
            }
            std::fs::write(&temporary, &body).is_err()
        }
        || std::fs::rename(&temporary, ledger).is_err();
    if failed {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    let _ = std::fs::remove_file(&temporary);
    true
}

/// `_dot_profile_deactivation_script`: resolve the fixed
/// `<checkout>/dot/profile-deactivate` entry point for a ledger
/// record. The record validates first (silent refusal), then the
/// script must exist (a dangling symlink counts, like the shell's
/// `[[ -e || -L ]]`), then
/// [`crate::extension_trust::deactivation_validate`] authorizes it
/// against the saved Git identity. Returns the script path.
pub fn deactivation_script(record: &str, home: &str, euid: u32) -> Result<String, ScriptError> {
    if !crate::overlay_context::record_validate(record.as_bytes(), home) {
        return Err(ScriptError::Refused);
    }
    let mut fields = record.split('|');
    let path = fields.nth(1).unwrap_or("");
    let script = format!("{path}/dot/profile-deactivate");
    if std::fs::symlink_metadata(&script).is_err() {
        return Err(ScriptError::Missing);
    }
    crate::extension_trust::deactivation_validate(record, &script, home, euid)
        .map_err(|_| ScriptError::Refused)?;
    Ok(script)
}

/// Record name (`${record%%|*}`), tolerating malformed rows the
/// caller already decided to keep (prepare/commit only ever pass
/// loaded or engine-built records, whose names the shell reads the
/// same way).
fn record_name(record: &str) -> &str {
    record.split('|').next().unwrap_or("")
}

/// Inputs for [`prepare`], replacing the shell globals: the
/// profile/extensions predicates, the three overlay sets, the
/// ledger location, and the identity the trust checks run under.
/// `prior` is the caller's stored records (the shell global);
/// absent profiles leave them untouched.
pub struct PrepareInputs<'a> {
    /// `DOT_PROFILES_PRESENT -eq 1`.
    pub present: bool,
    /// `_dot_extensions_enabled` (`DOT_EXTENSION_API` is `1` with a
    /// non-empty `DOT_EXTENSIONS_DIR`).
    pub extensions_enabled: bool,
    /// `ELIGIBLE_OVERLAY_NAMES`.
    pub eligible: &'a [String],
    /// `PHASE_ONE_ACTIVE_OVERLAYS`.
    pub phase_one: &'a [String],
    /// `ACTIVE_OVERLAYS`.
    pub active: &'a [String],
    /// Records the caller holds (returned as-is when profiles are
    /// absent).
    pub prior: &'a [String],
    /// `DOT_PROFILE_LIFECYCLE_LEDGER` (`None` is unset).
    pub ledger: Option<&'a Path>,
    /// `$HOME` anchoring record validation.
    pub home: &'a str,
    /// `$EUID` for the trust checks.
    pub euid: u32,
    /// Logger for the `_warn` lines.
    pub log: &'a Log,
}

/// Outcome of [`prepare`]: the records the shell
/// `DOT_PROFILE_LIFECYCLE_RECORDS` global would hold after the
/// call, plus whether the shell returned exit 0. A failure keeps
/// the loaded records (the shell assigns the refreshed union only
/// on the success path), so even failing outcomes carry state the
/// tests compare.
pub struct Prepared {
    /// Records the shell global would hold.
    pub records: Vec<String>,
    /// Whether the shell returned exit 0.
    pub succeeded: bool,
}

/// `_dot_profile_lifecycle_prepare`: refresh the ledger before a
/// sync. Absent profiles are a no-op keeping `prior`. Otherwise
/// the ledger loads, then — with extensions disabled — any
/// retained-but-ineligible overlay fails pending-deactivation; with
/// extensions enabled every retained-but-ineligible entry point
/// must still validate, every current entry point either refreshes
/// its retained record or (when missing) must not be retained, and
/// the union is written back in byte-sorted name order (the shell's
/// `LC_ALL=C sort`).
///
/// Failure scans run in ledger order, then sorted name order for
/// the current set; the shell scans hash order, so only
/// single-fault inputs name the same record on both sides (both
/// still fail).
pub fn prepare(inputs: &PrepareInputs<'_>, warnings: &mut dyn std::io::Write) -> Prepared {
    if !inputs.present {
        return Prepared {
            records: inputs.prior.to_vec(),
            succeeded: true,
        };
    }
    let mut loaded = Vec::new();
    if !load(
        inputs.ledger,
        inputs.home,
        inputs.euid,
        inputs.log,
        warnings,
        &mut loaded,
    ) {
        return Prepared {
            records: loaded,
            succeeded: false,
        };
    }
    let eligible: HashSet<&str> = inputs.eligible.iter().map(String::as_str).collect();
    let mut current: HashMap<&str, &str> = HashMap::new();
    for record in inputs.phase_one.iter().chain(inputs.active.iter()) {
        current.insert(record_name(record), record);
    }
    let mut retained: HashMap<&str, &str> = HashMap::new();
    for record in &loaded {
        retained.insert(record_name(record), record);
    }
    if !inputs.extensions_enabled {
        for record in &loaded {
            let name = record_name(record);
            if !eligible.contains(name) {
                inputs.log.warn(
                    warnings,
                    &format!(
                        "  warning: profile deactivation pending while extensions are disabled: {name}"
                    ),
                );
                return Prepared {
                    records: loaded,
                    succeeded: false,
                };
            }
        }
        return Prepared {
            records: loaded,
            succeeded: true,
        };
    }
    for record in &loaded {
        let name = record_name(record);
        if eligible.contains(name) {
            continue;
        }
        if deactivation_script(retained[name], inputs.home, inputs.euid).is_err() {
            inputs.log.warn(
                warnings,
                &format!("  warning: unsafe retiring overlay entrypoint: {name}"),
            );
            return Prepared {
                records: loaded,
                succeeded: false,
            };
        }
    }
    let mut names: Vec<&str> = current.keys().copied().collect();
    names.sort();
    for name in names {
        let record = current[name];
        match deactivation_script(record, inputs.home, inputs.euid) {
            Ok(_) => _ = retained.insert(name, record),
            Err(ScriptError::Missing) => {
                if retained.contains_key(name) {
                    inputs.log.warn(
                        warnings,
                        &format!(
                            "  warning: active overlay removed profile deactivation entrypoint: {name}"
                        ),
                    );
                    return Prepared {
                        records: loaded,
                        succeeded: false,
                    };
                }
            }
            Err(ScriptError::Refused) => {
                inputs.log.warn(
                    warnings,
                    &format!("  warning: unsafe profile deactivation entrypoint: {name}"),
                );
                return Prepared {
                    records: loaded,
                    succeeded: false,
                };
            }
        }
    }
    let mut names: Vec<&str> = retained.keys().copied().collect();
    names.sort();
    let prepared: Vec<String> = names
        .iter()
        .map(|name| retained[name].to_string())
        .collect();
    let ledger = match inputs.ledger {
        Some(path) if !path.as_os_str().is_empty() => path,
        _ => {
            return Prepared {
                records: loaded,
                succeeded: false,
            };
        }
    };
    if !write(ledger, &prepared, inputs.euid) {
        return Prepared {
            records: loaded,
            succeeded: false,
        };
    }
    Prepared {
        records: prepared,
        succeeded: true,
    }
}

/// Inputs for [`commit`]: the profile/extensions predicates, the
/// stored ledger records, the eligible and active sets, and the
/// ledger location plus trust identity for the rewrite.
pub struct CommitInputs<'a> {
    /// `DOT_PROFILES_PRESENT -eq 1`.
    pub present: bool,
    /// `_dot_extensions_enabled`.
    pub extensions_enabled: bool,
    /// Stored ledger records (`DOT_PROFILE_LIFECYCLE_RECORDS`).
    pub retained: &'a [String],
    /// `ELIGIBLE_OVERLAY_NAMES`.
    pub eligible: &'a [String],
    /// `ACTIVE_OVERLAYS`.
    pub active: &'a [String],
    /// `DOT_PROFILE_LIFECYCLE_LEDGER` (`None` is unset).
    pub ledger: Option<&'a Path>,
    /// `$HOME` anchoring record validation.
    pub home: &'a str,
    /// `$EUID` for the trust checks.
    pub euid: u32,
}

/// `_dot_profile_lifecycle_commit`: record the survivors after a
/// sync. Absent profiles or disabled extensions are a silent
/// no-op (`true`). Otherwise the new ledger keeps the retained
/// eligible-but-inactive records in ledger order, then the active
/// records whose entry point still validates in sorted name order;
/// an active record with a missing entry point drops silently
/// (shell exit 2), while an unsafe one fails the commit with no
/// warning (the shell bare `return 1`). The result is written
/// back; `true` is shell exit 0.
pub fn commit(inputs: &CommitInputs<'_>) -> bool {
    if !inputs.present || !inputs.extensions_enabled {
        return true;
    }
    let eligible: HashSet<&str> = inputs.eligible.iter().map(String::as_str).collect();
    let mut active: HashMap<&str, &str> = HashMap::new();
    for record in inputs.active {
        active.insert(record_name(record), record);
    }
    let mut committed: Vec<String> = Vec::new();
    for record in inputs.retained {
        let name = record_name(record);
        if !eligible.contains(name) {
            continue;
        }
        if active.contains_key(name) {
            continue;
        }
        committed.push(record.clone());
    }
    let mut names: Vec<&str> = active.keys().copied().collect();
    names.sort();
    for name in names {
        match deactivation_script(active[name], inputs.home, inputs.euid) {
            Ok(_) => committed.push(active[name].to_string()),
            Err(ScriptError::Missing) => (),
            Err(ScriptError::Refused) => return false,
        }
    }
    let ledger = match inputs.ledger {
        Some(path) if !path.as_os_str().is_empty() => path,
        _ => return false,
    };
    write(ledger, &committed, inputs.euid)
}

/// Inputs for [`run_one`]: the record to deactivate plus the
/// runtime the worker needs. `tmpdir` is `${TMPDIR:-/tmp}`,
/// `now_secs` the `date +%s` instant for context freshness, and
/// `verbose` whether `DOT_VERBOSE` equals `1` (the `_log` quiet
/// gate itself lives in `log`, like the shell's `_log`).
pub struct RunInputs<'a> {
    /// Ledger record to deactivate.
    pub record: &'a str,
    /// `$HOME` anchoring record validation.
    pub home: &'a str,
    /// `$EUID` for the trust checks.
    pub euid: u32,
    /// Scratch parent (`${TMPDIR:-/tmp}`).
    pub tmpdir: &'a Path,
    /// Current time in epoch seconds.
    pub now_secs: i64,
    /// `DOT_VERBOSE -eq 1`.
    pub verbose: bool,
    /// Logger for `_warn` lines and the verbose `_log` relay.
    pub log: &'a Log,
}

/// Strip trailing newlines like shell command substitution does
/// (`output=$(... 2>&1)`); carriage returns survive, exactly as in
/// the shell variable.
fn command_output(output: &[u8]) -> &[u8] {
    let mut end = output.len();
    while end > 0 && output[end - 1] == b'\n' {
        end -= 1;
    }
    &output[..end]
}

/// Allocate the `mktemp -d` scratch directory for one deactivation
/// (mode `0o700` regardless of umask, like `mktemp -d`).
fn mktemp_dir(tmpdir: &Path) -> Option<PathBuf> {
    use std::io::Read as _;
    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut random| random.read_exact(&mut suffix).ok())?;
        let name: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = tmpdir.join(format!("dot.{name}"));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).is_ok() {
                    return Some(path);
                }
                let _ = std::fs::remove_dir_all(&path);
                return None;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// `_dot_profile_lifecycle_run_one`: run one overlay's
/// `profile-deactivate` entry point. An unresolvable record fails
/// silently (shell exit 1, even for the missing-entry-point exit
/// 2); an unmakable scratch directory does the same. Otherwise a
/// `deactivate`/`retiring`/`none` authorization context is minted
/// over the scratch directory and the [`WorkerRun`] seam executes
/// the script with the `has-deactivate` result file beside it. A
/// failed context mints the literal
/// `could not create deactivation context` output the shell falls
/// back to. The scratch directory is always removed. Nonzero
/// worker output warns (when non-empty) and relays the worker's
/// exit code; successful output relays to `out` only when verbose
/// (through [`Log::log`]); returns the shell exit code.
pub fn run_one(
    inputs: &RunInputs<'_>,
    worker: &mut dyn WorkerRun,
    out: &mut dyn std::io::Write,
    warnings: &mut dyn std::io::Write,
) -> i32 {
    let script = match deactivation_script(inputs.record, inputs.home, inputs.euid) {
        Ok(script) => script,
        Err(_) => return 1,
    };
    let Some(result_dir) = mktemp_dir(inputs.tmpdir) else {
        return 1;
    };
    let result_file = result_dir.join("has-deactivate");
    let context = match crate::overlay_context::create(
        &result_dir,
        CONTEXT_MODE,
        CONTEXT_SET_KIND,
        CONTEXT_STAGE,
        &[inputs.record.as_bytes().to_vec()],
        inputs.home,
        inputs.euid,
        inputs.now_secs,
    ) {
        Ok((context, token)) => Some((context, token)),
        Err(_) => None,
    };
    let outcome = match context {
        Some((context, token)) => worker.run(
            Path::new(&script),
            &result_dir,
            &result_file,
            &context,
            &token,
        ),
        None => WorkerOutcome {
            rc: 1,
            output: b"could not create deactivation context".to_vec(),
        },
    };
    let _ = std::fs::remove_dir_all(&result_dir);
    // Worker bytes cross into warning/log text lossily (the engine
    // string-boundary precedent); test fixtures stay ASCII.
    let text = String::from_utf8_lossy(command_output(&outcome.output)).into_owned();
    if outcome.rc != 0 {
        if !text.is_empty() {
            inputs.log.warn(warnings, &text);
        }
        return outcome.rc;
    }
    if !text.is_empty() && inputs.verbose {
        inputs.log.log(out, &text);
    }
    0
}

/// Inputs for [`retire`]: the profile/extensions predicates, the
/// stored ledger records, the eligible set, and the [`RunInputs`]
/// fields each deactivation needs.
pub struct RetireInputs<'a> {
    /// `DOT_PROFILES_PRESENT -eq 1`.
    pub present: bool,
    /// `_dot_extensions_enabled`.
    pub extensions_enabled: bool,
    /// Stored ledger records (`DOT_PROFILE_LIFECYCLE_RECORDS`), in
    /// ledger order.
    pub retained: &'a [String],
    /// `ELIGIBLE_OVERLAY_NAMES`.
    pub eligible: &'a [String],
    /// `$HOME` anchoring record validation.
    pub home: &'a str,
    /// `$EUID` for the trust checks.
    pub euid: u32,
    /// Scratch parent (`${TMPDIR:-/tmp}`).
    pub tmpdir: &'a Path,
    /// Current time in epoch seconds.
    pub now_secs: i64,
    /// `DOT_VERBOSE -eq 1`.
    pub verbose: bool,
    /// Logger for `_warn` lines and the verbose `_log` relay.
    pub log: &'a Log,
}

/// `_dot_profile_lifecycle_retire`: run every stored deactivation
/// whose overlay is no longer eligible, in ledger order. Each
/// failure warns `profile deactivation failed: <name>` and latches
/// the return to 1; absent profiles or disabled extensions are a
/// silent no-op (shell exit 0). Returns the shell exit code.
pub fn retire(
    inputs: &RetireInputs<'_>,
    worker: &mut dyn WorkerRun,
    out: &mut dyn std::io::Write,
    warnings: &mut dyn std::io::Write,
) -> i32 {
    if !inputs.present || !inputs.extensions_enabled {
        return 0;
    }
    let eligible: HashSet<&str> = inputs.eligible.iter().map(String::as_str).collect();
    let mut failed = 0;
    for record in inputs.retained {
        let name = record_name(record);
        if eligible.contains(name) {
            continue;
        }
        let run = RunInputs {
            record,
            home: inputs.home,
            euid: inputs.euid,
            tmpdir: inputs.tmpdir,
            now_secs: inputs.now_secs,
            verbose: inputs.verbose,
            log: inputs.log,
        };
        if run_one(&run, worker, out, warnings) != 0 {
            inputs.log.warn(
                warnings,
                &format!("  warning: profile deactivation failed: {name}"),
            );
            failed = 1;
        }
    }
    failed
}
