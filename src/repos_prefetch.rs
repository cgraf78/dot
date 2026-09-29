//! Speculative overlay remote probes for `dot update`.
//!
//! An update fetches the base first and the overlays in later rounds
//! (phase one, then profile additions), and every round pays a full remote
//! round trip after the previous one finishes. On multi-overlay clients those
//! serial round trips dominate a no-op update. [`start`] probes every
//! already-cloned overlay while the base fetch runs, and each overlay round
//! asks [`Prefetch::take_proof`] whether its own fetch can be skipped.
//!
//! A probe is read-only: it runs `git ls-remote` and records the local
//! transport inputs it ran under. It never writes refs or objects, so an update
//! that stops early (failed base pull, refusing pre-sync, interruption) leaves
//! every overlay exactly as the unprefetched engine would. A round skips its
//! fetch only when [`proves_current`] shows that the fetch would change
//! nothing observable; any doubt, failure, or stderr output falls back to the
//! unchanged fetch path, which then reports errors exactly as before.
//!
//! Two consequences are deliberate and documented in `docs/extensions.md`:
//! probes contact already-cloned overlay remotes before pre-sync runs (a
//! transport change made by pre-sync voids them), and a skipped fetch reflects
//! the remote as of the probe, a few seconds earlier than the round.
//!
//! The [`Prefetch`] value is owned by one update's sync phase and passed down
//! to the pull lanes; dropping it abandons and joins every unconsumed probe,
//! so no probe outlives the phase that started it.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Upper bound on one whole probe. A probe that cannot answer in this window
/// is worth less than a fresh fetch, which the round then performs itself.
const PROBE_DEADLINE: Duration = Duration::from_secs(60);

/// How long a round waits for a probe still in flight before abandoning it
/// and fetching normally. Probes start before the base fetch, so a healthy
/// one is normally finished by the time its round asks; this only bounds the
/// extra wait a stalled probe can add in front of the unchanged fetch.
const CONSUME_GRACE: Duration = Duration::from_secs(10);
const CONSUME_POLL: Duration = Duration::from_millis(5);

/// Advertisements larger than this are not worth retaining; the round fetches.
const PROBE_CAPTURE_LIMIT: usize = 4 * 1024 * 1024;

/// Configuration listing used for both the fingerprint and the modeled
/// settings: every scope, NUL-delimited, each entry preceded by its scope.
pub const SCOPED_CONFIG: &[&str] = &["config", "-z", "--list", "--show-scope"];

/// The `fetch.*` settings [`proves_current`] models and validates. Any other
/// `fetch.*` key refuses the proof: fetch parses them at startup, so even a
/// setting that cannot matter to a no-op could make the real fetch fail.
const FETCH_KEYS: &[&str] = &["prune", "prunetags", "recursesubmodules"];

/// `remote.<name>.*` settings that are modeled in [`proves_current`] or read
/// and validated by Git's shared remote configuration for `ls-remote` and
/// `fetch` alike (a malformed value fails the probe too).
const REMOTE_KEYS: &[&str] = &[
    "url",
    "pushurl",
    "fetch",
    "push",
    "prune",
    "prunetags",
    "tagopt",
    "followremotehead",
    "skipdefaultupdate",
    "skipfetchall",
    "proxy",
    "proxyauthmethod",
    "uploadpack",
    "receivepack",
];

/// A proof older than this, measured from when the remote was queried, is
/// discarded and the round fetches. Rounds normally consume proofs within
/// seconds; this bounds staleness when an earlier round runs long.
const MAX_PROOF_AGE: Duration = Duration::from_secs(60);

/// Largest SSH client configuration worth fingerprinting.
const SSH_CONFIG_LIMIT: u64 = 1024 * 1024;

/// One started probe: its worker, its own abandonment flag, and the SSH
/// configuration path it fingerprinted, so the round re-reads the same file.
struct Pending {
    handle: JoinHandle<Option<Probe>>,
    abandon: Arc<AtomicBool>,
    ssh_config: PathBuf,
}

impl Pending {
    fn abandon_and_join(self) {
        self.abandon.store(true, Ordering::Release);
        let _ = self.handle.join();
    }
}

/// What one probe observed: the tracking branch it resolved, the transport
/// inputs in force when it started, and the remote's branch and tag
/// advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `@{u}` as `remote/branch`, exactly as the fetch path resolves it.
    pub upstream: String,
    /// File-backed configuration fingerprint ([`config_fingerprint`]).
    pub config: Vec<u8>,
    /// Bytes of the user's SSH client configuration, or `None` when absent.
    pub ssh_config: Option<Vec<u8>>,
    /// Advertised branch name to object id.
    pub heads: BTreeMap<String, String>,
    /// Advertised tag name to (unpeeled) object id.
    pub tags: BTreeMap<String, String>,
    /// When the remote query started; proofs expire a minute after it.
    pub observed: Instant,
}

/// The probes started for one sync phase, keyed by overlay record path.
/// Dropping it abandons every probe no round consumed and joins its worker.
pub struct Prefetch {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Prefetch {
    /// A poisoned lock only means a lane panicked while holding it; the map
    /// itself stays consistent, so keep using it.
    fn pending(&self) -> MutexGuard<'_, HashMap<String, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Wait, within the consume grace, until no probe is still running, and
    /// abandon any that overrun it. Called before an overlay fleet starts so
    /// speculative work never overlaps the fetch lanes, keeping total remote
    /// concurrency within `DOT_UPDATE_JOBS`. Finished probes stay available
    /// to [`Self::take_proof`].
    pub fn settle(&self) {
        let grace = Instant::now() + CONSUME_GRACE;
        loop {
            let running = self
                .pending()
                .values()
                .any(|entry| !entry.handle.is_finished());
            if !running {
                return;
            }
            if Instant::now() >= grace {
                break;
            }
            std::thread::sleep(CONSUME_POLL);
        }
        let overrun: Vec<Pending> = {
            let mut map = self.pending();
            let keys: Vec<String> = map
                .iter()
                .filter(|(_, entry)| !entry.handle.is_finished())
                .map(|(key, _)| key.clone())
                .collect();
            keys.iter().filter_map(|key| map.remove(key)).collect()
        };
        for entry in &overrun {
            entry.abandon.store(true, Ordering::Release);
        }
        for entry in overrun {
            entry.abandon_and_join();
        }
    }

    /// Consume the probe for `path`, if one was started, and report whether
    /// it proves that fetching `upstream` now would change nothing. Returns
    /// `false` whenever the caller must fetch. A probe still in flight gets
    /// a bounded grace period to finish before it is abandoned.
    pub fn take_proof(&self, path: &Path, upstream: &str) -> bool {
        let Some(key) = path.to_str() else {
            return false;
        };
        let Some(pending) = self.pending().remove(key) else {
            return false;
        };
        let grace = Instant::now() + CONSUME_GRACE;
        while !pending.handle.is_finished() {
            if Instant::now() >= grace {
                pending.abandon_and_join();
                return false;
            }
            std::thread::sleep(CONSUME_POLL);
        }
        let ssh_config = pending.ssh_config.clone();
        let Ok(Some(probe)) = pending.handle.join() else {
            return false;
        };
        // Read local state first so the age check also covers that read.
        let Some(now) = current_state(path, upstream, &ssh_config) else {
            return false;
        };
        probe.observed.elapsed() <= MAX_PROOF_AGE
            && proves_current(
                &probe,
                upstream,
                &now.config,
                now.ssh_config.as_deref(),
                &now.refs,
            )
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        let pending: Vec<Pending> = self.pending().drain().map(|(_, entry)| entry).collect();
        // Signal every probe first so they stop concurrently, then join.
        for entry in &pending {
            entry.abandon.store(true, Ordering::Release);
        }
        for entry in pending {
            entry.abandon_and_join();
        }
    }
}

/// Start at most `limit` probes, one per overlay checkout path in `paths`
/// (callers pass only existing checkouts). `ssh_config` is the SSH client
/// configuration whose bytes join the transport fingerprint.
pub fn start(paths: &[String], ssh_config: &Path, limit: usize) -> Prefetch {
    let host_git = crate::init_client_identity::carry_host_git();
    let mut pending = HashMap::new();
    for path in paths {
        if pending.len() >= limit {
            break;
        }
        if pending.contains_key(path) {
            continue;
        }
        let abandon = Arc::new(AtomicBool::new(false));
        let worker_abandon = Arc::clone(&abandon);
        let worker_path = PathBuf::from(path);
        let worker_ssh = ssh_config.to_path_buf();
        let host_git = host_git.clone();
        let spawned = std::thread::Builder::new()
            .name("dot-prefetch".to_string())
            .spawn(move || {
                let _host_git = host_git.bind();
                probe(&worker_path, &worker_ssh, &worker_abandon)
            });
        // A thread that cannot start is only a missed optimization.
        if let Ok(handle) = spawned {
            pending.insert(
                path.clone(),
                Pending {
                    handle,
                    abandon,
                    ssh_config: ssh_config.to_path_buf(),
                },
            );
        }
    }
    Prefetch {
        pending: Mutex::new(pending),
    }
}

/// The local inputs a proof compares against, read when the round asks.
struct CurrentState {
    config: Vec<u8>,
    ssh_config: Option<Vec<u8>>,
    refs: String,
}

fn current_state(path: &Path, upstream: &str, ssh_config: &Path) -> Option<CurrentState> {
    // The same fail-closed check the probe applied; the checkout may have
    // gained submodules since.
    if path.join(".gitmodules").exists() {
        return None;
    }
    let remote = crate::repos_pull_support::upstream_remote(upstream)?;
    let prefix = git_prefix(path);
    // Local reads for the calling round: nothing abandons them, and the
    // deadline only keeps a wedged repository from stalling the round.
    let never = AtomicBool::new(false);
    let deadline = Instant::now() + PROBE_DEADLINE;
    let config = git_stdout(&prefix, SCOPED_CONFIG, &never, deadline)?;
    let tracking = format!("refs/remotes/{remote}/");
    let refs = git_stdout(
        &prefix,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            tracking.as_str(),
            "refs/tags/",
        ],
        &never,
        deadline,
    )?;
    Some(CurrentState {
        config,
        ssh_config: read_optional(ssh_config)?,
        refs: String::from_utf8(refs).ok()?,
    })
}

/// Run one probe under a single absolute deadline. The transport inputs are
/// captured before `ls-remote` starts, so a change racing the probe can only
/// make the later comparison fail, never make stale results look current.
fn probe(path: &Path, ssh_config: &Path, abandon: &AtomicBool) -> Option<Probe> {
    let deadline = Instant::now() + PROBE_DEADLINE;
    // Submodule recursion can be configured per submodule in `.gitmodules`,
    // outside the configuration this module models; never skip such fetches.
    if path.join(".gitmodules").exists() {
        return None;
    }
    let prefix = git_prefix(path);
    let raw = git_stdout(
        &prefix,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        abandon,
        deadline,
    )?;
    let upstream = String::from_utf8(raw).ok()?.trim().to_string();
    let remote = crate::repos_pull_support::upstream_remote(&upstream)?.to_string();
    let config = config_fingerprint(&git_stdout(&prefix, SCOPED_CONFIG, abandon, deadline)?)?;
    let ssh = read_optional(ssh_config)?;
    let observed = Instant::now();
    let mut command = crate::init_client_identity::host_git_command();
    command
        .args(&prefix)
        .args(["ls-remote", "--heads", "--tags", remote.as_str()])
        .stdin(std::process::Stdio::null())
        // The probe runs in its own session without a terminal. Refuse every
        // interactive credential path up front so a remote that needs a
        // prompt fails fast and the foreground fetch prompts as before.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GCM_INTERACTIVE", "never");
    // Detach matches the existing quiet overlay fetch: a transport helper
    // that deliberately outlives Git (an SSH ControlPersist master) is the
    // user's configuration, not a leak. Abandonment and the deadline still
    // stop the probe itself through the strict teardown path.
    let output = crate::cleanup::run_session_output_abandonable(
        command,
        Some(deadline),
        PROBE_CAPTURE_LIMIT,
        crate::cleanup::LingerPolicy::Detach,
        abandon,
    )
    .ok()?;
    // Anything on stderr (host-key notices, redirects, deprecations) is text
    // the foreground fetch would have shown; keep that path authoritative.
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    let (heads, tags) = parse_advertisement(std::str::from_utf8(&output.stdout).ok()?)?;
    Some(Probe {
        upstream,
        config,
        ssh_config: ssh,
        heads,
        tags,
        observed,
    })
}

/// Parse `git ls-remote --heads --tags` output into branch and tag maps.
/// Peeled `^{}` lines are ignored because fetch stores the unpeeled tag.
/// Returns `None` for any line it does not understand.
pub fn parse_advertisement(
    text: &str,
) -> Option<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let mut heads = BTreeMap::new();
    let mut tags = BTreeMap::new();
    for line in text.lines() {
        let (oid, name) = line.split_once('\t')?;
        if oid.is_empty() || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        if name.ends_with("^{}") {
            continue;
        }
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            heads.insert(branch.to_string(), oid.to_string());
        } else {
            let tag = name.strip_prefix("refs/tags/")?;
            tags.insert(tag.to_string(), oid.to_string());
        }
    }
    Some((heads, tags))
}

/// Whether a `git fetch --quiet --no-write-fetch-head <remote>` issued now
/// would leave every ref unchanged, given the probe and the current local
/// state. Conservative by construction: any configuration it does not model
/// (custom refspecs, forced tag fetches, tag pruning, recursive submodule
/// fetches, remote-HEAD policies that can write or warn, and any unlisted
/// `fetch.*` or `remote.<name>.*` key) returns `false`.
///
/// A skipped fetch also skips its automatic maintenance; that is equivalent
/// because auto-maintenance thresholds only move when objects are added, and
/// every operation that adds them (a real fetch, merge, or commit) runs its
/// own.
///
/// `config` is current [`SCOPED_CONFIG`] output, `ssh_config` the current
/// SSH client configuration bytes, and `local_refs` current
/// `for-each-ref --format='%(objectname) %(refname)'` output for the remote's
/// tracking namespace and `refs/tags/`.
pub fn proves_current(
    probe: &Probe,
    upstream: &str,
    config: &[u8],
    ssh_config: Option<&[u8]>,
    local_refs: &str,
) -> bool {
    // Pre-sync may rewrite remotes, URL rewriting, or SSH host aliases after
    // the probe ran; any such change voids the advertisement.
    if probe.upstream != upstream
        || config_fingerprint(config).as_deref() != Some(probe.config.as_slice())
        || probe.ssh_config.as_deref() != ssh_config
    {
        return false;
    }
    let Some((remote, branch)) = upstream.split_once('/') else {
        return false;
    };
    let Some(settings) = parse_config(config) else {
        return false;
    };
    let key = |name: &str| format!("remote.{remote}.{name}");
    let expected_refspec = format!("+refs/heads/*:refs/remotes/{remote}/*");
    if settings.get(&key("fetch")).map(Vec::as_slice) != Some(&[expected_refspec][..]) {
        return false;
    }
    // Git parses every occurrence of a key, not just the winning one, so an
    // earlier malformed value would fail the real fetch. Validate them all.
    let is_bool = |value: &str| bool_value(value).is_some();
    let recursion = |value: &str| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "false" | "no" | "off" | "0" | "on-demand"
        )
    };
    let head_policy =
        |value: &str| matches!(value.to_ascii_lowercase().as_str(), "create" | "never");
    let valid = every(&settings, &key("tagopt"), |value| value == "--no-tags")
        && every(&settings, &key("prune"), is_bool)
        && every(&settings, "fetch.prune", is_bool)
        && every(&settings, &key("prunetags"), is_bool)
        && every(&settings, "fetch.prunetags", is_bool)
        && every(&settings, &key("followremotehead"), head_policy)
        && every(&settings, "fetch.recursesubmodules", recursion)
        && every(&settings, "submodule.recurse", is_bool);
    if !valid {
        return false;
    }
    let tags_followed = last(&settings, &key("tagopt")).is_none();
    let Some(prune) = flag(&settings, &key("prune"), "fetch.prune") else {
        return false;
    };
    if flag(&settings, &key("prunetags"), "fetch.prunetags") != Some(false)
        || flag(&settings, "submodule.recurse", "submodule.recurse") != Some(false)
    {
        return false;
    }
    // Fail closed on every fetch or remote setting this model has not
    // examined (bundle URIs, mirrors, remote helpers, future keys, ...).
    let remote_prefix = format!("remote.{remote}.");
    for name in settings.keys() {
        if let Some(rest) = name.strip_prefix("fetch.") {
            if !FETCH_KEYS.contains(&rest) {
                return false;
            }
        } else if let Some(rest) = name.strip_prefix(remote_prefix.as_str()) {
            if !REMOTE_KEYS.contains(&rest) {
                return false;
            }
        }
    }
    // Remote HEAD policies other than create/never can write or warn; the
    // checks above already refused them. Recursion is modeled only when
    // disabled or on demand (a no-op fetch brings no new superproject
    // commits, so on-demand fetches nothing).
    let follow_head = last(&settings, &key("followremotehead"))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let tracking_prefix = format!("refs/remotes/{remote}/");
    let mut tracking: HashMap<&str, &str> = HashMap::new();
    let mut local_tags: HashMap<&str, &str> = HashMap::new();
    for line in local_refs.lines() {
        let Some((oid, name)) = line.split_once(' ') else {
            return false;
        };
        if let Some(rest) = name.strip_prefix(tracking_prefix.as_str()) {
            tracking.insert(rest, oid);
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            local_tags.insert(tag, oid);
        }
    }
    if !probe.heads.contains_key(branch) {
        return false;
    }
    if probe
        .heads
        .iter()
        .any(|(name, oid)| tracking.get(name.as_str()) != Some(&oid.as_str()))
    {
        return false;
    }
    // Fetch may create a missing remote HEAD unless the policy forbids it.
    if follow_head != "never" && !tracking.contains_key("HEAD") {
        return false;
    }
    if prune
        && tracking
            .keys()
            .any(|name| *name != "HEAD" && !probe.heads.contains_key(*name))
    {
        return false;
    }
    // Auto-following fetches every advertised tag whose object is local; a
    // missing or different local tag means the fetch could change it.
    if tags_followed
        && probe
            .tags
            .iter()
            .any(|(name, oid)| local_tags.get(name.as_str()) != Some(&oid.as_str()))
    {
        return false;
    }
    true
}

/// Split [`SCOPED_CONFIG`] output into `(scope, entry)` pairs, where an entry
/// is `key\nvalue` or a bare `key`. `None` for output it does not understand.
fn scoped_entries(bytes: &[u8]) -> Option<Vec<(&str, &str)>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text.split('\0');
    let mut entries = Vec::new();
    while let Some(scope) = fields.next() {
        if scope.is_empty() {
            // The listing ends with a terminator, leaving one empty field.
            return fields.next().is_none().then_some(entries);
        }
        entries.push((scope, fields.next()?));
    }
    Some(entries)
}

/// Fingerprint of the effective configuration: every entry in listing order
/// except command-scope HTTP extra headers. Git wrappers commonly inject
/// per-invocation request headers (tracing or correlation IDs) through
/// `git -c`, which would make every fingerprint unique; the probe and the
/// fetch share the same process environment, so such headers cannot differ
/// between them in any way the environment did not already decide. Headers
/// from configuration files (credentials, for example) must match exactly.
pub fn config_fingerprint(scoped: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for (scope, entry) in scoped_entries(scoped)? {
        let key = entry.split_once('\n').map_or(entry, |(key, _)| key);
        let inert = scope == "command" && key.starts_with("http.") && key.ends_with(".extraheader");
        if !inert {
            out.extend_from_slice(scope.as_bytes());
            out.push(0);
            out.extend_from_slice(entry.as_bytes());
            out.push(0);
        }
    }
    Some(out)
}

/// Parse [`SCOPED_CONFIG`] output into key to values in precedence order.
/// Keys keep Git's own normalization (lowercase section and variable,
/// case-preserved subsection). A bare key (`[fetch] prune` with no `=`) is
/// Git's implicit boolean true, so it records `true`; `key =` records an empty
/// value, which Git reads as false.
fn parse_config(bytes: &[u8]) -> Option<BTreeMap<String, Vec<String>>> {
    let mut settings: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, entry) in scoped_entries(bytes)? {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
        settings
            .entry(key.to_string())
            .or_default()
            .push(value.to_string());
    }
    Some(settings)
}

fn last<'a>(settings: &'a BTreeMap<String, Vec<String>>, key: &str) -> Option<&'a str> {
    settings.get(key)?.last().map(String::as_str)
}

/// Resolve a boolean the way fetch does: the remote-specific key wins over
/// the global fallback, and both default to false. `None` means the value is
/// not a boolean Git would accept, so the caller must not guess.
fn flag(settings: &BTreeMap<String, Vec<String>>, key: &str, fallback: &str) -> Option<bool> {
    match last(settings, key).or_else(|| last(settings, fallback)) {
        Some(value) => bool_value(value),
        None => Some(false),
    }
}

/// Whether every occurrence of `key` (none counts) satisfies `valid`.
fn every(
    settings: &BTreeMap<String, Vec<String>>,
    key: &str,
    valid: impl Fn(&str) -> bool,
) -> bool {
    settings
        .get(key)
        .is_none_or(|values| values.iter().all(|value| valid(value)))
}

/// Git's boolean spellings; `None` for anything Git would reject.
fn bool_value(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Read a file whose absence is meaningful: `Some(None)` when it does not
/// exist, `None` (refuse the proof) when it cannot be read safely. Only a
/// bounded regular file qualifies: reading a FIFO or device could block this
/// thread where no cancellation can reach it.
fn read_optional(path: &Path) -> Option<Option<Vec<u8>>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // Non-blocking so opening a FIFO cannot wait for a writer; the metadata
    // check below then rejects anything but a regular file.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > SSH_CONFIG_LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(SSH_CONFIG_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= SSH_CONFIG_LIMIT).then_some(Some(bytes))
}

fn git_prefix(path: &Path) -> Vec<OsString> {
    vec![OsString::from("-C"), path.as_os_str().to_owned()]
}

/// Captured stdout of one inspection command, or `None` on any failure. Runs
/// under the caller's abandonment flag and absolute deadline so no step of a
/// probe can outlive its owner. Any stderr output (a configuration warning,
/// for example) is text a real fetch would have shown, so it refuses the
/// proof too.
fn git_stdout(
    prefix: &[OsString],
    args: &[&str],
    abandon: &AtomicBool,
    deadline: Instant,
) -> Option<Vec<u8>> {
    let mut command = crate::init_client_identity::host_git_command();
    command
        .args(prefix)
        .args(args)
        .stdin(std::process::Stdio::null());
    let output = crate::cleanup::run_session_output_abandonable(
        command,
        Some(deadline),
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Detach,
        abandon,
    )
    .ok()?;
    (output.status.success() && output.stderr.is_empty()).then_some(output.stdout)
}
