//! Init plan review and conflict safekeeping for `lib/dot/init-client.sh`:
//! the plan summary, the confirmation gate, the conflict backup and
//! restore pair, and the completion publication.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the five contiguous functions from
//! `_dot_init_confirm` through `_dot_init_publish_completed` in file
//! order: the operator sees the plan ([`plan_summary`]), approves it
//! ([`confirm`]), conflicting worktree state is stashed aside
//! ([`move_conflicts`]), restored when a transaction rolls back
//! ([`restore_backups`]), and a successful run stamps its record
//! ([`publish_completed`]).
//!
//! Lane map, so the integrator can stack without overlap: the
//! transaction-directory lifecycle lives on `rust-port-slice-35`
//! (`init_client_transaction`), the host-git identity family on
//! `rust-port-slice-41` (`init_client_identity`), the git-generation
//! binding on `rust-port-slice-43` (`init_client_generation`), the
//! per-entry staging family on `rust-port-slice-46`
//! (`init_client_entry`), the candidate planning family on
//! `rust-port-slice-48` (`init_client_candidate`), the transaction
//! record journal on `rust-port-slice-51` (`init_client_records`) and
//! `rust-port-slice-54` (`init_client_record`), and the
//! deletion-parking family on `rust-port-slice-55`
//! (`init_client_delete`). The file-generic `_dot_init_error`
//! diagnostic stays unported (a bare `printf ... >&2; return 1` with
//! no family state, absorbed into [`Result`] the way earlier slices
//! absorb engine diagnostics). The publish (`publish_intent`,
//! `publish_one`, `publish_worktree`, `published_stage_matches`,
//! `published_intent_matches`, `cleanup_published_stage`), git-stage
//! (`stage_git`, `publish_git`), rollback, resume, status, and
//! command-dispatch families stay for later slices, as do the small
//! shared guards (`safe_value`, `safe_relative_path` — the latter
//! already lives in the base tree as
//! [`crate::repos_overlays::init_safe_relative_path`], which this
//! module mirrors with a byte-local twin the way the record lane
//! does).
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals and the worktree root from `HOME`. Library
//! code must not read process environment behind the engine, so the
//! backup root, home, source checkout, and skip-provider flag cross
//! here as explicit parameters. The shell derives the completed path
//! from its own globals (`_dot_init_completed_file`, owned by the
//! transaction lane) and the conflict prompt from the fixed
//! `/dev/tty`; here the completed path crosses as a parameter and
//! the terminal crosses as a path the engine always sets to
//! `/dev/tty` (a parameter, rather than a hardcoded open, so tests
//! can pin the refusal rows without a controlling terminal).
//! `REPLY`-carried outputs surface as return values, and the two
//! rendered reports ([`confirm`], [`plan_summary`]) return their
//! stderr bytes for the caller to emit, keeping this module free of
//! ambient file descriptors. Cross-lane predicates the shell calls
//! by name (`_dot_init_path_state_matches` from the candidate lane,
//! `_dot_init_private_directory` from the transaction lane) cross as
//! closures, the way the delete lane takes its verifier.
//!
//! Byte-fidelity boundary: every `$HOME/$path` join concatenates
//! bytes like the shell, preserving a doubled separator on
//! trailing-slash inputs instead of normalizing it away (the delete
//! lane precedent). Journal text crosses the UTF-8 boundary with
//! `from_utf8_lossy`, the candidate lane precedent, so non-UTF8
//! journal bytes can diverge from the shell exactly the way they do
//! on sibling lanes. `LC_ALL=C` is pinned around every child process
//! so git and sort-ordered output read English and byte-ordered on
//! both engines.
//!
//! `read` exactness: manifest rows are parsed with the shell's
//! `IFS=$'\t' read -r` semantics — leading tabs stripped, tab runs
//! collapsing between fields, the last variable keeping the raw
//! remainder with its tabs intact, missing variables reading empty —
//! not a plain tab split, which mis-assigns rows with leading or
//! doubled tabs (see `read_row`). Loop framing matches the
//! ingestion: `while read` bodies never run for an unterminated
//! final line (direct reads and `sort -r` output alike — `sort`
//! terminates the tail first), `cut` does emit its tail, and `read`
//! silently drops NUL bytes while `cut` preserves them (all probed
//! against bash and GNU coreutils; see `read_loop_lines` and
//! [`cut_lines`).

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::errors::{Error, Result};
use crate::temp;

/// Worktree-state matcher: the candidate lane's
/// `_dot_init_path_state_matches` by position
/// (`target kind dev ino mode size value`), injected because that
/// lane is unmerged. Tests feed either a stub or a closure that runs
/// the live shell predicate, so the orchestration below stays
/// differentially covered either way.
pub type StateMatches<'a> = dyn Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool + 'a;

/// Private-directory provision: the transaction lane's
/// `_dot_init_private_directory` (`mkdir -p` plus the real-directory
/// gate plus `chmod 0700`), injected because that lane is unmerged.
pub type EnsurePrivateDir<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Header of the confirmation listing, printed when the conflicts
/// manifest is non-empty: the shell's first `printf` in
/// `_dot_init_confirm`.
const CONFIRM_HEADER: &[u8] = b"dot init: conflicting paths will be backed up:\n";

/// Confirmation prompt written to the terminal: the shell's
/// `printf 'Continue? [y/N] ' >/dev/tty`.
const CONFIRM_PROMPT: &[u8] = b"Continue? [y/N] ";

/// Answers the confirmation gate accepts: the shell's
/// `[[ $answer == y || $answer == Y || $answer == yes || $answer == YES ]]`.
const CONFIRM_YES: [&str; 4] = ["y", "Y", "yes", "YES"];

/// Preview scratch the plan summary renders the candidate config
/// into: the shell's `$candidate/dot-config.preview`.
const PREVIEW_NAME: &str = "dot-config.preview";

/// Config blob the plan summary inspects inside the candidate:
/// the shell's `$branch:.config/dot/config`.
const CONFIG_BLOB: &str = ".config/dot/config";

/// Child script that loads the preview through the real config
/// engine and prints the three plan fields: the shell's
/// `--noprofile --norc -c '...'` body verbatim, so provider, policy,
/// and extension defaults stay owned by `config.sh` on both engines.
const CONFIG_PROBE: &str = "set -euo pipefail
. \"$DOT_SOURCE_ROOT/lib/dot/config.sh\"
dot_config_load \"$1\"
printf \"%s\\t%s\\t%s\\n\" \"$DOT_DEPENDENCY_PROVIDER\" \"$DOT_SHDEPS_UPDATE_POLICY\" \"${DOT_EXTENSION_API:+enabled}\"
";

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// A regular file reached through any non-symlink chain: the shell's
/// bare `[[ -f $path ]]`, which follows symlinks. Used only for the
/// restore gate, where the shell tests exactly this.
fn is_file_following(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Effective-uid ownership (`test -O`): the shell gate requires the
/// path to be ours. An unreadable identity fails closed, like the
/// shell's failed `stat`. (Twin of the delete lane's gate; kept
/// local because that lane is unmerged.)
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// Raw bytes of a path, so `$HOME/` prefix work and `$HOME/$path`
/// joins behave like shell string operations even when `home` has a
/// trailing slash (the doubled separator is preserved, never
/// normalized away).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Append one `/`-separated leaf, like the shell's `"$base/$leaf"`.
/// Byte concatenation, so a `base` with a trailing slash keeps its
/// doubled separator exactly like the shell's expansion does.
fn join2(base: &Path, leaf: &str) -> PathBuf {
    let mut joined = path_bytes(base).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(leaf.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// Strip the shortest `/*` suffix, like the shell's
/// `${path%/*}`: up to the last slash, or the whole string when
/// there is no slash. Returned as bytes so the no-slash case (the
/// shell keeps the full string, even empty) mirrors exactly.
fn strip_last_component(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(position) => &path[..position],
        None => path,
    }
}

/// Frame file bytes as the shell's `cut -f1` sees them: bytes
/// divide on `\n`, a missing trailing newline still yields its final
/// line (probed: `cut` terminates the tail itself), and a trailing
/// newline adds no phantom empty line. NUL bytes pass through:
/// `cut` preserves them (probed).
fn cut_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// Frame file bytes as a shell `while read` loop over a direct
/// `done <file` redirect iterates them:
/// bytes divide on `\n` and the final chunk is always dropped — it
/// is either the phantom after a trailing newline or an
/// unterminated tail whose variables `read` assigns but whose body
/// never runs (probed against bash with both pipes and file
/// redirects; the sibling split-and-keep convention mis-runs such
/// tails). Feeds [`move_conflicts`], which reads its journal
/// directly. NUL bytes never survive
/// `read` (bash drops them silently, probed), so they are stripped
/// up front.
fn read_loop_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    lines.pop();
    lines.iter().map(|line| line.replace('\0', "")).collect()
}
/// Frame file bytes as a shell `while read` loop over `sort -r`
/// output iterates them: `sort` terminates the final line first
/// (probed on GNU coreutils: an unterminated tail gains its newline
/// before the loop reads it), so only the phantom after a trailing
/// newline drops. Feeds [`restore_backups`]. NUL bytes strip like
/// above, since the loop still reads through `read`.
fn sort_loop_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines.iter().map(|line| line.replace('\0', "")).collect()
}

/// Mirror of `IFS=$'\t' read -r v1..vN`: leading tabs are stripped,
/// tab runs collapse between fields, the last slot keeps the raw
/// remainder with its tabs intact, and missing slots read empty. A
/// plain `splitn` mis-assigns rows with leading or doubled tabs
/// (probed against bash: `'\tp\tk'` reads `p`,`k` and
/// `'p\t\tk'` reads `p`,`k`, while `splitn` yields a spurious
/// empty first or second field), so the manifest loops use this
/// instead. `out` must hold exactly the loop's variable count; the
/// remainder arm makes an oversized row land in the last slot, the
/// way extra words append to the shell's final variable.
fn read_row<'line>(line: &'line str, out: &mut [&'line str]) {
    let Some((last, head)) = out.split_last_mut() else {
        return;
    };
    let mut rest = line.trim_start_matches('\t');
    for slot in head {
        match rest.find('\t') {
            Some(position) => {
                *slot = &rest[..position];
                rest = rest[position..].trim_start_matches('\t');
            }
            None => {
                *slot = rest;
                rest = "";
            }
        }
    }
    *last = rest;
}

/// First tab-separated field with no stripping or collapsing: the
/// shell's `cut -f1`, which reports the raw bytes before the first
/// tab (empty when the line leads with one).
fn cut_first(line: &str) -> &str {
    match line.find('\t') {
        Some(position) => &line[..position],
        None => line,
    }
}

/// Byte-local twin of
/// [`crate::repos_overlays::init_safe_relative_path`]: a
/// home-relative path with no escapes and no `.git` component. Kept
/// local (not imported) because that copy takes `&str` field
/// borrows shaped for its own call sites while the manifest loops
/// here validate whole rows; the logic below mirrors it case for
/// case, including ASCII-only `.git` folding under `LC_ALL=C`. The
/// record lane vendors the same twin for the same reason.
fn safe_relative_bytes(path: &[u8]) -> bool {
    if path.is_empty() || path.contains(&b'\t') || path.contains(&b'\n') || path.contains(&b'\r') {
        return false;
    }
    if path.starts_with(b"/")
        || path == b"."
        || path == b".."
        || path.starts_with(b"./")
        || path.starts_with(b"../")
        || path.windows(3).any(|window| window == b"/./")
        || path.windows(4).any(|window| window == b"/../")
        || path.ends_with(b"/")
        || path.ends_with(b"/.")
        || path.ends_with(b"/..")
        || path.windows(2).any(|window| window == b"//")
    {
        return false;
    }
    !path.split(|byte| *byte == b'/').any(|component| {
        component.len() == 4
            && component
                .iter()
                .zip(b".git".iter())
                .all(|(got, want)| got.to_ascii_lowercase() == *want)
    })
}

/// Run `git -C <candidate> <args>` with `LC_ALL=C` pinned and `HOME`
/// steered at the test home, like the shell probe inherits from its
/// harness. Captures stdout; `None` when git cannot start or reports
/// failure, like the shell's `|| return 1` on the substitution —
/// git's own stderr is silenced (the candidate lane precedent).
fn git_in(home: &Path, candidate: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(candidate)
        .args(args)
        .env("LC_ALL", "C")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Alphabet for `mktemp` sequence fields: the shell's `mktemp`
/// `XXXXXX` draws from letters and digits.
const MKTEMP_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Monotonic fallback seed for the no-urandom path below.
static MKTEMP_FALLBACK: AtomicU64 = AtomicU64::new(0x2545_f491_4f6c_dd1d);

/// Six random alphanumeric bytes for a `mktemp` sequence field.
/// Reads `/dev/urandom` exactly (never `read_to_end`, which would
/// block forever on an infinite stream); when urandom is unavailable
/// a pid/time/counter xorshift fills in, which is still unique per
/// call in practice.
fn mktemp_suffix() -> [u8; 6] {
    if let Ok(file) = std::fs::File::open("/dev/urandom") {
        let mut bytes = [0u8; 6];
        let mut reader = file;
        if std::io::Read::read_exact(&mut reader, &mut bytes).is_ok() {
            let mut suffix = [0u8; 6];
            for (slot, byte) in suffix.iter_mut().zip(bytes.iter()) {
                *slot = MKTEMP_ALPHABET[usize::from(*byte) % MKTEMP_ALPHABET.len()];
            }
            return suffix;
        }
    }
    let mut seed = u64::from(std::process::id());
    seed ^= MKTEMP_FALLBACK.fetch_add(1, Ordering::Relaxed);
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        seed = seed
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(now.as_secs());
        seed ^= u64::from(now.subsec_nanos());
    }
    let mut suffix = [0u8; 6];
    for slot in suffix.iter_mut() {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        seed = seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
        *slot = MKTEMP_ALPHABET[(seed >> 58) as usize % MKTEMP_ALPHABET.len()];
    }
    suffix
}

/// Create `dir/prefixXXXXXX` with mode `0600`: the shell's
/// `mktemp "$root/.completed.XXXXXX"`. Retries occupied names the
/// way `mktemp` picks a fresh sequence, and fails when the directory
/// itself is unusable. The template prefix (not just the directory)
/// matches the shell's so leftover-temperature rows in the parity
/// tests can recognize the file by name on both engines.
fn mktemp_in(dir: &Path, prefix: &str) -> Result<PathBuf> {
    for _ in 0..100 {
        let suffix = mktemp_suffix();
        let mut name = OsString::from(prefix);
        name.push(OsString::from_vec(suffix.to_vec()));
        let candidate = dir.join(Path::new(&name));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(Error::Io {
                    context: "create temporary file",
                    source,
                });
            }
        }
    }
    Err(Error::Usage {
        message: "temporary file name is occupied",
    })
}

/// `_dot_init_confirm`: report the conflicting paths and, unless the
/// operator passed `--yes`, require an interactive yes on the
/// terminal. Returns the stderr listing (header plus one
/// two-space-indented first field per manifest row, the shell's
/// `cut -f1 | sed 's/^/  /'`); an empty manifest short-circuits with
/// no output and no terminal touch, exactly like the shell's
/// `[[ -s $manifest ]] || return 0`.
///
/// The terminal crosses as `tty` (the engine always passes
/// `/dev/tty`): the prompt goes through one write-only open and the
/// answer through a separate read-only open, mirroring the shell's
/// `>/dev/tty` print plus `read </dev/tty`. An unopenable terminal,
/// an EOF answer, and anything but `y`/`Y`/`yes`/`YES` all refuse.
/// A partial final line without its newline refuses too: the shell's
/// `read ... || return 1` fails on EOF even when it assigned bytes.
pub fn confirm(manifest: &Path, yes: bool, tty: &Path) -> Result<Vec<u8>> {
    if !std::fs::metadata(manifest).is_ok_and(|meta| meta.len() > 0) {
        return Ok(Vec::new());
    }
    // A manifest the size gate accepted but the read cannot serve is
    // a torn journal; the shell's `cut` would print a partial list
    // and carry on, which has no faithful rendering here, so fail.
    let content = std::fs::read(manifest).map_err(|source| Error::Io {
        context: "read conflicts manifest",
        source,
    })?;
    let text = String::from_utf8_lossy(&content);
    let mut out = CONFIRM_HEADER.to_vec();
    for line in cut_lines(&text) {
        out.extend_from_slice(b"  ");
        out.extend_from_slice(cut_first(line).as_bytes());
        out.push(b'\n');
    }
    if yes {
        return Ok(out);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(tty)
        .and_then(|mut terminal| terminal.write_all(CONFIRM_PROMPT))
        .map_err(|_| Error::Usage {
            message: "conflicts require --yes in a noninteractive session",
        })?;
    let terminal = std::fs::File::open(tty).map_err(|_| Error::Usage {
        message: "conflicts require --yes in a noninteractive session",
    })?;
    let mut reader = std::io::BufReader::new(terminal);
    let mut answer = Vec::new();
    {
        use std::io::BufRead as _;
        reader
            .read_until(b'\n', &mut answer)
            .map_err(|_| Error::Usage {
                message: "conflicts require --yes in a noninteractive session",
            })?;
    }
    let Some(body) = answer.strip_suffix(b"\n".as_slice()) else {
        return Err(Error::Usage {
            message: "confirmation answer was not affirmative",
        });
    };
    let text = std::str::from_utf8(body).unwrap_or("");
    if CONFIRM_YES.contains(&text) {
        Ok(out)
    } else {
        Err(Error::Usage {
            message: "confirmation answer was not affirmative",
        })
    }
}

/// Inputs for [`plan_summary`]: the shell's five positionals plus
/// the run context, bundled the way the record lane bundles its
/// thirteen record fields — eight flat parameters trip
/// `clippy::too_many_arguments`, and these eight always travel
/// together.
pub struct PlanInputs<'a> {
    /// Candidate checkout holding the config blob (`$1`).
    pub candidate: &'a Path,
    /// Branch being installed (`$2`).
    pub branch: &'a str,
    /// Candidate tree journal to count (`$3`).
    pub tree: &'a Path,
    /// Backup root printed in the report (`$4`).
    pub backup: &'a str,
    /// Repository identity printed in the report (`$5`).
    pub identity: &'a str,
    /// Client root (`HOME`): steers the config-probe child.
    pub home: &'a Path,
    /// Source checkout (`DOT_SOURCE_ROOT`): the probe sources
    /// `config.sh` from here.
    pub source_root: &'a Path,
    /// `DOT_INIT_SKIP_PROVIDER` is `1`: annotate a real provider.
    pub skip_provider: bool,
}

/// `_dot_init_plan_summary`: render what this run is about to do.
/// Returns the report bytes the shell prints to stderr: the
/// repository identity, branch, tracked-path count (the shell's
/// `wc -l`, so newline bytes, not lines), backup root, dependency
/// provider, shdeps update policy, and extension state.
///
/// The provider triple comes from the live config engine: when the
/// candidate holds `$branch:.config/dot/config`, its bytes land in
/// `$candidate/dot-config.preview` (left behind on later failure,
/// like the shell's) and a child `bash` sources `config.sh` against
/// it with `HOME` and `DOT_SOURCE_ROOT` steered at `home` and
/// `source_root`. Only the child's first output line is read, the
/// shell's `read` on the herestring, and an empty extension word
/// reports `disabled`. When `skip_provider` is set (the shell's
/// `DOT_INIT_SKIP_PROVIDER=1`) a non-`none` provider is annotated.
/// A missing config blob keeps the compiled-in defaults, and a
/// failed `git show` or failed child refuses. An unreadable tree
/// refuses too — matching the engine, which runs under `set -o
/// pipefail` (`lib/dot/main.sh`) so the `wc` failure in the count
/// pipeline propagates. Without `pipefail` the shell would report an
/// empty count instead; the parity tests pin the engine behavior by
/// setting the flag in every summary probe.
pub fn plan_summary(inputs: &PlanInputs<'_>) -> Result<Vec<u8>> {
    let candidate = inputs.candidate;
    let branch = inputs.branch;
    let tree = inputs.tree;
    let backup = inputs.backup;
    let identity = inputs.identity;
    let home = inputs.home;
    let source_root = inputs.source_root;
    let skip_provider = inputs.skip_provider;
    let content = std::fs::read(tree).map_err(|source| Error::Io {
        context: "read candidate tree",
        source,
    })?;
    let count = content.iter().filter(|byte| **byte == b'\n').count();
    let mut provider = b"none".to_vec();
    let mut policy = b"pinned".to_vec();
    let mut extensions = b"disabled".to_vec();
    let object = format!("{branch}:{CONFIG_BLOB}");
    if git_in(home, candidate, &["cat-file", "-e", &object]).is_some() {
        let shown = git_in(home, candidate, &["show", &object]).ok_or(Error::Command {
            command: "git show candidate config".to_string(),
            status: Some("non-zero exit".to_string()),
        })?;
        let preview = join2(candidate, PREVIEW_NAME);
        std::fs::write(&preview, &shown).map_err(|source| Error::Io {
            context: "write config preview",
            source,
        })?;
        let probed = Command::new("bash")
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(CONFIG_PROBE)
            .arg("dot-plan-sh")
            .arg(&preview)
            .env("LC_ALL", "C")
            .env("HOME", home)
            .env("DOT_SOURCE_ROOT", source_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|source| Error::Io {
                context: "probe candidate config",
                source,
            })?;
        if !probed.status.success() {
            return Err(Error::Command {
                command: "load candidate config".to_string(),
                status: Some("non-zero exit".to_string()),
            });
        }
        // Command substitution strips every trailing newline; the
        // herestring adds one back and `read` takes the first line.
        let text = String::from_utf8_lossy(&probed.stdout);
        let first = text.split('\n').next().unwrap_or("");
        let mut fields = ["", "", ""];
        read_row(first, &mut fields);
        provider = fields[0].as_bytes().to_vec();
        policy = fields[1].as_bytes().to_vec();
        if !fields[2].is_empty() {
            extensions = fields[2].as_bytes().to_vec();
        }
    }
    if skip_provider && provider != b"none" {
        provider.extend_from_slice(b" (skipped for this invocation)");
    }
    let mut out = b"dot init plan:\n".to_vec();
    out.extend_from_slice(format!("  repository: {identity}\n").as_bytes());
    out.extend_from_slice(format!("  branch: {branch}\n").as_bytes());
    out.extend_from_slice(format!("  tracked paths: {count}\n").as_bytes());
    out.extend_from_slice(format!("  backup: {backup}\n").as_bytes());
    out.extend_from_slice(format!("  dependency provider: {}\n", lossy(&provider)).as_bytes());
    out.extend_from_slice(format!("  shdeps update policy: {}\n", lossy(&policy)).as_bytes());
    out.extend_from_slice(format!("  extensions: {}\n", lossy(&extensions)).as_bytes());
    Ok(out)
}

/// Lossy text for report rendering: provider words are engine
/// vocabulary (ASCII in practice), and the candidate lane renders
/// its diagnostics the same way.
fn lossy(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// `_dot_init_move_conflicts`: stash every conflicting worktree path
/// under `backup`, recording the manifest there first so a later run
/// (or rollback) can tell whose backup it is. A stored manifest that
/// already matches is reused; one that differs refuses, like the
/// shell's `_dot_files_equal` gate.
///
/// Rows parse with `read_row` (`path kind dev ino mode size
/// value`); blank paths skip. A row whose backup destination already
/// holds the recorded state while the home path is still absent is
/// done and skips; otherwise the home path must hold the recorded
/// state, the destination's parent is created, the destination must
/// be absent, and the home path moves over with the exclusive
/// same-filesystem rename both engines share. `source_root` feeds
/// the manifest comparison, `state_matches` is the candidate lane's
/// predicate, `ensure_private_dir` is the transaction lane's
/// provisioner, and `cache` backs the moves.
pub fn move_conflicts(
    manifest: &Path,
    backup: &Path,
    home: &Path,
    source_root: &Path,
    state_matches: &StateMatches<'_>,
    ensure_private_dir: &EnsurePrivateDir<'_>,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    ensure_private_dir(backup)?;
    let stored = join2(backup, "manifest");
    if !exists_lexical(&stored) {
        std::fs::copy(manifest, &stored).map_err(|source| Error::Io {
            context: "stage conflicts manifest",
            source,
        })?;
        std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| Error::Io {
                context: "chmod conflicts manifest",
                source,
            },
        )?;
    } else if !temp::files_equal(source_root, manifest, &stored).unwrap_or(false) {
        return Err(Error::Usage {
            message: "conflict manifest changed",
        });
    }
    let content = std::fs::read(manifest).map_err(|source| Error::Io {
        context: "read conflicts manifest",
        source,
    })?;
    let text = String::from_utf8_lossy(&content);
    for line in read_loop_lines(&text) {
        let mut fields = ["", "", "", "", "", "", ""];
        read_row(&line, &mut fields);
        let [path, kind, dev, ino, mode, size, value] = fields;
        if path.is_empty() {
            continue;
        }
        let destination = join2(backup, path);
        let live = join2(home, path);
        if state_matches(&destination, kind, dev, ino, mode, size, value) && !exists_lexical(&live)
        {
            continue;
        }
        if !state_matches(&live, kind, dev, ino, mode, size, value) {
            return Err(Error::Usage {
                message: "worktree path changed during backup",
            });
        }
        let parent = strip_last_component(path_bytes(&destination));
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(std::ffi::OsStr::from_bytes(parent))
            .map_err(|source| Error::Io {
                context: "create backup parent",
                source,
            })?;
        if exists_lexical(&destination) {
            return Err(Error::Usage {
                message: "backup destination is occupied",
            });
        }
        temp::move_noreplace_cached(&live, &destination, cache)?;
    }
    Ok(())
}

/// `_dot_init_restore_backups`: move stashed conflicts home, newest
/// manifest row first (the shell's `LC_ALL=C sort -r`, a descending
/// byte sort). A missing backup root or manifest is a successful
/// no-op, like the shell's opening gate; an unreadable manifest past
/// that gate also restores nothing, because the shell's failed
/// `sort` feeds the loop empty input and still returns zero.
///
/// Every row still validates before it moves: the path must be a
/// safe home-relative spelling (`safe_relative_bytes`), the stashed
/// source must hold the recorded state, and the home path must be
/// absent. Absent stashes skip; anything else refuses.
pub fn restore_backups(
    backup: &Path,
    home: &Path,
    state_matches: &StateMatches<'_>,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    let stored = join2(backup, "manifest");
    if !(is_real_dir(backup) && is_file_following(&stored)) {
        return Ok(());
    }
    // See the module docs: a failed read feeds the loop nothing and
    // the shell still succeeds, so mirror that instead of failing.
    let Ok(content) = std::fs::read(&stored) else {
        return Ok(());
    };
    let text = String::from_utf8_lossy(&content);
    let mut lines = sort_loop_lines(&text);
    lines.sort_by(|left, right| right.cmp(left));
    for line in lines {
        let mut fields = ["", "", "", "", "", "", ""];
        read_row(&line, &mut fields);
        let [path, kind, dev, ino, mode, size, value] = fields;
        if !safe_relative_bytes(path.as_bytes()) {
            return Err(Error::Usage {
                message: "backup path is unsafe",
            });
        }
        let source = join2(backup, path);
        if !exists_lexical(&source) {
            continue;
        }
        if !state_matches(&source, kind, dev, ino, mode, size, value) {
            return Err(Error::Usage {
                message: "stashed path changed during restore",
            });
        }
        let live = join2(home, path);
        if exists_lexical(&live) {
            return Err(Error::Usage {
                message: "restore destination is occupied",
            });
        }
        let parent = if let Some(position) = path.rfind('/') {
            join2(home, &path[..position])
        } else {
            home.to_path_buf()
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(&parent)
            .map_err(|source| Error::Io {
                context: "create restore parent",
                source,
            })?;
        temp::move_noreplace_cached(&source, &live, cache)?;
    }
    Ok(())
}

/// `_dot_init_publish_completed`: stamp the transaction `record` as
/// the durable completion marker. `completed` crosses as a path (the
/// transaction lane computes it); its parent is provisioned, the
/// record copies through a `.completed.XXXXXX` sibling at mode
/// `0600`, and the sibling moves into place — replacing only a
/// real, operator-owned regular file, exactly like the shell's
/// `-f`/`-L`/`-O` gate. Every failure leaves the sibling behind,
/// like the shell's abandoned `mktemp` output, so failure rows in
/// the parity tests recognize it by its `.completed.` prefix.
pub fn publish_completed(
    record: &Path,
    completed: &Path,
    ensure_private_dir: &EnsurePrivateDir<'_>,
    cache: &mut temp::MoveCache,
) -> Result<()> {
    let root = strip_last_component(path_bytes(completed));
    let root = Path::new(std::ffi::OsStr::from_bytes(root));
    ensure_private_dir(root)?;
    let temporary = mktemp_in(root, ".completed.")?;
    std::fs::copy(record, &temporary).map_err(|source| Error::Io {
        context: "copy completion record",
        source,
    })?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| Error::Io {
            context: "chmod completion record",
            source,
        },
    )?;
    if exists_lexical(completed) {
        if !(is_real_file(completed) && owned_by_us(completed)) {
            return Err(Error::Usage {
                message: "completion record is not ours",
            });
        }
        temp::move_replace_nodir_cached(&temporary, completed, cache)?;
    } else {
        temp::move_noreplace_cached(&temporary, completed, cache)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        cut_first, cut_lines, read_loop_lines, read_row, safe_relative_bytes, sort_loop_lines,
        strip_last_component,
    };

    /// Split one line into exactly `out.len()` variables.
    fn split<const N: usize>(line: &str) -> [String; N] {
        let mut out: [&str; N] = [""; N];
        read_row(line, &mut out);
        out.map(str::to_string)
    }

    #[test]
    fn read_row_matches_shell_ifs_tab() {
        // Probed against bash `IFS=$'\t' read -r a b c d e f g`:
        // leading tabs strip, tab runs collapse, the last variable
        // keeps the raw remainder, missing variables read empty.
        assert_eq!(
            split::<7>("p\tk\td\ti\tm\ts\tv").as_slice(),
            ["p", "k", "d", "i", "m", "s", "v"]
        );
        assert_eq!(
            split::<7>("p\tk").as_slice(),
            ["p", "k", "", "", "", "", ""]
        );
        assert_eq!(
            split::<7>("p\tk\td\ti\tm\ts\tv\te1\te2").as_slice(),
            ["p", "k", "d", "i", "m", "s", "v\te1\te2"]
        );
        assert_eq!(
            split::<7>("a\tb\tc\td\te\tf\tg1\t\tg2\th3").as_slice(),
            ["a", "b", "c", "d", "e", "f", "g1\t\tg2\th3"]
        );
        assert_eq!(
            split::<7>("\tp\tk").as_slice(),
            ["p", "k", "", "", "", "", ""]
        );
        assert_eq!(
            split::<7>("p\t\tk").as_slice(),
            ["p", "k", "", "", "", "", ""]
        );
        assert_eq!(
            split::<7>("p\tk\t").as_slice(),
            ["p", "k", "", "", "", "", ""]
        );
        assert_eq!(split::<2>("\tx").as_slice(), ["x", ""]);
        assert_eq!(split::<3>("").as_slice(), ["", "", ""]);
    }

    #[test]
    fn cut_lines_frames_like_cut() {
        // `cut` emits the unterminated tail and preserves NULs.
        assert_eq!(cut_lines("one\ntwo"), vec!["one", "two"]);
        assert_eq!(cut_lines("one\n"), vec!["one"]);
        assert_eq!(cut_lines("one\n\ntwo\n"), vec!["one", "", "two"]);
        assert_eq!(cut_lines("a\0b\n"), vec!["a\0b"]);
        assert!(cut_lines("").is_empty());
    }

    #[test]
    fn read_loop_lines_frames_like_read_loop() {
        // `while read` bodies never run for the unterminated tail,
        // and NUL bytes never survive `read`.
        assert_eq!(read_loop_lines("one\ntwo"), vec!["one".to_string()]);
        assert_eq!(read_loop_lines("one\n"), vec!["one".to_string()]);
        assert_eq!(
            read_loop_lines("one\n\ntwo\n"),
            vec!["one".to_string(), "".to_string(), "two".to_string()]
        );
        assert_eq!(read_loop_lines("a\0b\n"), vec!["ab".to_string()]);
        assert!(read_loop_lines("").is_empty());
        assert!(read_loop_lines("tail-only").is_empty());
    }

    #[test]
    fn sort_loop_lines_keeps_normalized_tail() {
        // `sort -r` terminates the tail before the loop reads it.
        let two: Vec<String> = vec!["one".to_string(), "two".to_string()];
        assert_eq!(sort_loop_lines("one\ntwo"), two);
        assert_eq!(sort_loop_lines("one\n"), vec!["one".to_string()]);
        assert!(sort_loop_lines("").is_empty());
    }

    #[test]
    fn cut_first_matches_cut_f1() {
        assert_eq!(cut_first("a\tb"), "a");
        assert_eq!(cut_first("NOTAB"), "NOTAB");
        assert_eq!(cut_first("\tp"), "");
        assert_eq!(cut_first(""), "");
    }

    #[test]
    fn strip_last_component_matches_shell_expansion() {
        assert_eq!(strip_last_component(b"a/b/c"), b"a/b");
        assert_eq!(strip_last_component(b"/p"), b"");
        assert_eq!(strip_last_component(b"noslash"), b"noslash");
        assert_eq!(strip_last_component(b"a/"), b"a");
    }

    #[test]
    fn safe_relative_bytes_rejects_escapes() {
        assert!(safe_relative_bytes(b"a/b"));
        assert!(safe_relative_bytes(b".dotfiles/x"));
        assert!(!safe_relative_bytes(b""));
        assert!(!safe_relative_bytes(b"/abs"));
        assert!(!safe_relative_bytes(b"a/../../b"));
        assert!(!safe_relative_bytes(b"a/.git/x"));
        assert!(!safe_relative_bytes(b"a/.GIT/x"));
        assert!(!safe_relative_bytes(b"a/"));
        assert!(!safe_relative_bytes(b"a//b"));
        assert!(!safe_relative_bytes(b"a\tb"));
        // Probed against `_dot_init_safe_relative_path`: interior
        // dot segments refuse, while bare `GIT` (no dot) passes.
        assert!(!safe_relative_bytes(b"a/../b"));
        assert!(!safe_relative_bytes(b"a/./b"));
        assert!(!safe_relative_bytes(b"../x"));
        assert!(safe_relative_bytes(b"a b"));
        assert!(safe_relative_bytes(b"-"));
        assert!(safe_relative_bytes(b"GIT"));
    }
}
