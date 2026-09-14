//! The init plan-review and conflict-backup family of `lib/dot/init-client.sh`:
//! the plan summary, the backup confirmation prompt, the conflict move into
//! the backup, the backup restore, and the completion-record publication.
//!
//! The shell file holds 79 functions — too big for one lane — so this module
//! owns only the five functions from `_dot_init_confirm` through
//! `_dot_init_publish_completed`. The file-generic `_dot_init_error`
//! diagnostic stays unported (a bare `printf ... >&2; return 1` with no
//! family state, absorbed into [`Result`] the way earlier slices absorb
//! engine diagnostics); the one call site that needs its bytes
//! ([`confirm`] on a non-interactive session) emits that single literal
//! inline so stderr stays comparable across engines. The sanitizers
//! already live in the base tree
//! ([`repos_overlays::init_safe_relative_path`]).
//! The transaction lifecycle (`state_root`, `transaction_dir`,
//! `completed_file`, `private_directory`, `prepare_transaction`,
//! `transaction_stage_owned`, `recover_transaction_stages`,
//! `publish_transaction`) is in flight on `rust-port-slice-35`, the
//! host-git identity family on `rust-port-slice-41`, the generation binding
//! on `rust-port-slice-43`, the per-entry staging family on
//! `rust-port-slice-46`, the candidate planning family on
//! `rust-port-slice-48`, the record journal on `rust-port-slice-54`, and
//! the deletion parking family on `rust-port-slice-55`; the git staging,
//! publish pipeline, published verification, rollback, status, and command
//! families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the `HOME`,
//! `XDG_STATE_HOME`, `DOT_SOURCE_ROOT`, and `DOT_INIT_SKIP_PROVIDER`
//! environment. Library code must not mutate the process environment
//! behind the engine, so those cross here as explicit parameters;
//! `REPLY`-carried outputs return their values. `_dot_init_confirm`
//! answers on `/dev/tty` directly, so the terminal stays a fixed path,
//! not a parameter. The tree matcher (`_dot_init_path_state_matches`,
//! owned by the unmerged candidate lane) crosses as a `&dyn Fn` closure
//! with its match arguments bound at each call site, exactly like the
//! deletion lane's verifier.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::repos_overlays;
use crate::temp;
use crate::xdg;

/// Diagnostic `_dot_init_confirm` prints when conflicts exist but the
/// session is non-interactive: the `_dot_init_error` format
/// (`dot init: %s`) applied to this family's literal at that call site.
const CONFIRM_NONINTERACTIVE: &str =
    "dot init: conflicts require --yes in a noninteractive session\n";

/// Prompt `_dot_init_confirm` writes to the terminal before reading the
/// answer. No trailing newline, exactly like the shell's `printf`.
const CONFIRM_PROMPT: &str = "Continue? [y/N] ";

/// Header `_dot_init_confirm` prints above the conflicting paths.
const CONFIRM_HEADER: &str = "dot init: conflicting paths will be backed up:\n";

/// Fixed `/dev/tty` path both engines prompt and read on. Absolute by
/// construction: the shell hardcodes it, so the port does too instead
/// of threading a parameter the oracle cannot vary.
const TTY_PATH: &str = "/dev/tty";

/// Split text the way the shell's `while IFS= read -r line` loop sees
/// it: bytes divide on `\n`, a missing trailing newline still yields
/// its final line, and a trailing newline adds no phantom empty line.
/// Carriage returns stay put — `str::lines` would strip a trailing
/// `\r` the shell keeps, so this splits manually.
fn shell_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// Split one manifest or probe line the way `IFS=$'\t' read -r`
/// assigns `count` variables. Tab is IFS whitespace, so leading and
/// trailing tab runs vanish and every inner run — however long —
/// delimits a single field; spaces are data, never delimiters. The
/// last variable keeps its remainder raw, inner tabs included (the
/// shell does not re-split it), while missing fields read empty.
/// Probed against bash 5.2 before porting; the unit tests below pin
/// each corner.
fn read_tab_fields(line: &str, count: usize) -> Vec<&str> {
    let mut fields = Vec::with_capacity(count);
    if count == 0 {
        return fields;
    }
    let stripped = line.trim_matches('\t');
    if stripped.is_empty() {
        fields.resize(count, "");
        return fields;
    }
    let mut rest = stripped;
    for _ in 0..count - 1 {
        match rest.find('\t') {
            Some(index) => {
                fields.push(&rest[..index]);
                rest = rest[index + 1..].trim_start_matches('\t');
            }
            None => {
                fields.push(rest);
                rest = "";
            }
        }
    }
    fields.push(rest);
    while fields.len() < count {
        fields.push("");
    }
    fields
}

/// Join `relative` onto `base` with a literal `/`, like the shell's
/// `$base/$relative`: a `base` with a trailing slash keeps its doubled
/// separator instead of being normalized away.
fn concat_join(base: &Path, relative: &str) -> PathBuf {
    let mut joined = base.as_os_str().to_os_string();
    joined.push("/");
    joined.push(relative);
    PathBuf::from(joined)
}

/// Lexical existence: the shell's `[[ -e $path || -L $path ]]`, which
/// is false only when no directory entry exists at all (a dangling
/// symlink still counts as present). `symlink_metadata` never follows,
/// so it reports exactly this shape.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `_dot_init_private_directory`: `mkdir -p` plus mode 0700, failing
/// when a non-directory occupies the path. Twin of the transaction
/// module's three-line helper, kept local because that module is a
/// sibling owner, not a shared helper — the same reason the
/// generation lane twins its ownership gate. Both steps fork their
/// tools through [`run_fs_tool`] so their diagnostics match the
/// shell's byte for byte.
fn ensure_private_dir(path: &Path, stderr: &mut dyn std::io::Write) -> Result<()> {
    run_fs_tool(
        "mkdir",
        &[std::ffi::OsStr::new("-p"), path.as_os_str()],
        "create private directory",
        stderr,
    )?;
    let meta = std::fs::symlink_metadata(path).map_err(|source| Error::Io {
        context: "stat private directory",
        source,
    })?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "private directory is not a directory",
        });
    }
    run_fs_tool(
        "chmod",
        &[std::ffi::OsStr::new("0700"), path.as_os_str()],
        "chmod private directory",
        stderr,
    )
}

/// Effective-uid ownership (`test -O`): the shell gate requires the
/// completion record to be ours. An unreadable identity fails closed,
/// like the shell's failed `stat`. (Twin of the generation module's
/// gate; kept local because that module is a sibling owner.)
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// Whether a confirmation answer accepts the backup plan: exactly
/// `y`, `Y`, `yes`, or `yes`-uppercase. The shell matches the raw
/// `read -r` result with no trimming, so padded or cased variants
/// (` Yes`, `yes `, `y\n`) all decline here too.
pub fn confirm_answer_is_yes(answer: &str) -> bool {
    matches!(answer, "y" | "Y" | "yes" | "YES")
}

/// Whether `/dev/tty` is readable and writable: the shell's
/// `[[ -r /dev/tty && -w /dev/tty ]]` gate, which checks access
/// permission bits rather than opening. A split `test -r` plus
/// `test -w` child mirrors that exactly (including exotic shapes
/// where the bits pass but the open fails); `std` has no `access(2)`
/// binding, and the shell pays the same kind of fork for `umask(2)`
/// in [`temp::read_umask`].
fn tty_interactive() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("test -r /dev/tty && test -w /dev/tty")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run a fatal filesystem tool (`cp`, `mkdir -p`, `chmod`) the way the
/// shell invokes it: the tool's stderr flows to `stderr` verbatim
/// (the shell inherits it), stdout is discarded, and any nonzero exit
/// fails — the shell's `|| return 1`. This mirrors
/// [`temp::mkdir_forwarded`], which forks for the same
/// byte-identical-diagnostic reason, but fatal here instead of
/// best-effort. A missing tool binary fails silently: the shell's own
/// lookup diagnostic names its interpreter, which the port cannot
/// reproduce.
fn run_fs_tool(
    program: &str,
    args: &[&std::ffi::OsStr],
    context: &'static str,
    stderr: &mut dyn std::io::Write,
) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| Error::Usage { message: context })?;
    stderr
        .write_all(&output.stderr)
        .map_err(|source| Error::Io {
            context: "forward tool diagnostics",
            source,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Usage { message: context })
    }
}

/// `_dot_init_confirm`: accept the conflict-backup plan in `manifest`.
/// An empty or missing manifest needs no confirmation and prints
/// nothing. Otherwise the conflicting paths (first tab field per
/// line, two-space indented, exactly like `cut -f1 | sed 's/^/  /'`)
/// go to `stderr`; `yes` skips the prompt, and without it the answer
/// is read from `/dev/tty`, declining everything but `y`/`Y`/`yes`/
/// `YES`. The non-interactive diagnostic goes to `stderr` inline —
/// the file-generic `_dot_init_error` stays unported.
pub fn confirm(manifest: &Path, yes: bool, stderr: &mut dyn std::io::Write) -> Result<()> {
    use std::io::Write as _;
    let size = match std::fs::metadata(manifest) {
        Ok(meta) => meta.len(),
        Err(_) => return Ok(()),
    };
    if size == 0 {
        return Ok(());
    }
    let bytes = std::fs::read(manifest).map_err(|source| Error::Io {
        context: "read conflict manifest",
        source,
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Usage {
        message: "conflict manifest is not UTF-8",
    })?;
    let mut listed = Vec::new();
    listed.extend_from_slice(CONFIRM_HEADER.as_bytes());
    for line in shell_lines(text) {
        let field = line.split('\t').next().unwrap_or("");
        listed.extend_from_slice(b"  ");
        listed.extend_from_slice(field.as_bytes());
        listed.push(b'\n');
    }
    stderr.write_all(&listed).map_err(|source| Error::Io {
        context: "write confirm listing",
        source,
    })?;
    if yes {
        return Ok(());
    }
    if !tty_interactive() {
        stderr
            .write_all(CONFIRM_NONINTERACTIVE.as_bytes())
            .map_err(|source| Error::Io {
                context: "write confirm diagnostic",
                source,
            })?;
        return Err(Error::Usage {
            message: "conflicts require --yes in a noninteractive session",
        });
    }
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(TTY_PATH)
        .map_err(|source| Error::Io {
            context: "open terminal",
            source,
        })?;
    tty.write_all(CONFIRM_PROMPT.as_bytes())
        .map_err(|source| Error::Io {
            context: "write confirm prompt",
            source,
        })?;
    let mut answer = String::new();
    {
        use std::io::BufRead as _;
        std::io::BufReader::new(&mut tty)
            .read_line(&mut answer)
            .map_err(|source| Error::Io {
                context: "read confirm answer",
                source,
            })?;
    }
    if answer.ends_with('\n') {
        answer.pop();
    }
    if confirm_answer_is_yes(&answer) {
        Ok(())
    } else {
        Err(Error::Usage {
            message: "backup plan declined",
        })
    }
}

/// Inputs to [`plan_summary`]: the candidate checkout, the branch and
/// tree under review, the planned backup directory, and the engine
/// environment the shell reads from its globals.
pub struct PlanInputs<'a> {
    /// Candidate checkout holding the branch under review.
    pub candidate: &'a Path,
    /// Branch under review, as in `$branch:.config/dot/config`.
    pub branch: &'a str,
    /// Candidate tree listing whose newline count is reported.
    pub tree: &'a Path,
    /// Planned backup directory, printed verbatim.
    pub backup: &'a Path,
    /// Repository identity string, printed verbatim.
    pub identity: &'a str,
    /// Engine home, backing `HOME` for the config probe child.
    pub home: &'a Path,
    /// Engine source root, backing `DOT_SOURCE_ROOT` for the probe.
    pub source_root: &'a Path,
    /// The shell's `${DOT_INIT_SKIP_PROVIDER:-0} == 1`: annotate a
    /// non-`none` provider as skipped for this invocation.
    pub skip_provider: bool,
}

/// Run `git -C <candidate> <args>`, pinning `LC_ALL=C` like every
/// other lane. `None` when git cannot start or fails — the shell's
/// `|| return 1`.
fn run_git_c(candidate: &Path, args: &[&str], capture: bool) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(candidate)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    if capture {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Load the candidate config through the isolated child the shell
/// forks: `config.sh` plus `dot_config_load` over the preview file,
/// reporting provider, shdeps policy, and extension state. The
/// child's stderr flows to `stderr` verbatim (the shell's redirection
/// inherits it), and any nonzero exit fails — the shell's
/// `|| return 1`. `HOME` and `DOT_SOURCE_ROOT` cross explicitly so
/// per-row test values never touch the process environment (the
/// skip-provider flag is read by the caller, after the child, exactly
/// like the shell).
fn load_config_values(
    preview: &Path,
    home: &Path,
    source_root: &Path,
    stderr: &mut dyn std::io::Write,
) -> Result<(String, String, String)> {
    const CHILD: &str = "set -euo pipefail\n. \"$DOT_SOURCE_ROOT/lib/dot/config.sh\"\ndot_config_load \"$1\"\nprintf \"%s\\t%s\\t%s\\n\" \"$DOT_DEPENDENCY_PROVIDER\" \"$DOT_SHDEPS_UPDATE_POLICY\" \"${DOT_EXTENSION_API:+enabled}\"\n";
    let child = Command::new("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(CHILD)
        .arg("--")
        .arg(preview)
        .env("LC_ALL", "C")
        .env("HOME", home)
        .env("DOT_SOURCE_ROOT", source_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| Error::Io {
            context: "spawn config probe",
            source,
        })?;
    stderr
        .write_all(&child.stderr)
        .map_err(|source| Error::Io {
            context: "forward config probe diagnostics",
            source,
        })?;
    if !child.status.success() {
        return Err(Error::Usage {
            message: "candidate configuration is invalid",
        });
    }
    let mut text = String::from_utf8_lossy(&child.stdout).into_owned();
    while text.ends_with('\n') {
        text.pop();
    }
    let fields = read_tab_fields(&text, 3);
    Ok((
        fields[0].to_string(),
        fields[1].to_string(),
        fields[2].to_string(),
    ))
}

/// `_dot_init_plan_summary`: describe the pending initialization on
/// `stderr` — repository, branch, tracked-path count, backup,
/// dependency provider, shdeps policy, and extension state. The count
/// is the tree's newline total (`wc -l` counts newlines, not lines).
/// When the branch carries `.config/dot/config`, its values come
/// from the isolated config child; otherwise the triple stays
/// `none`/`pinned`/`disabled`. A set `skip_provider` annotates a
/// non-`none` provider as skipped for this invocation.
///
/// Without `pipefail` the count pipeline reports `tr`'s status, so a
/// missing or unreadable tree still succeeds with an empty count —
/// mirrored here by counting only when the tree reads. `wc`'s own
/// diagnostic for that case is platform-specific (GNU versus BSD) and
/// is not replicated.
pub fn plan_summary(inputs: &PlanInputs<'_>, stderr: &mut dyn std::io::Write) -> Result<()> {
    let count = match std::fs::read(inputs.tree) {
        Ok(tree_bytes) => tree_bytes
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            .to_string(),
        Err(_) => String::new(),
    };
    let mut provider = "none".to_string();
    let mut policy = "pinned".to_string();
    let mut extensions = "disabled".to_string();
    let mut spec = inputs.branch.to_string();
    spec.push_str(":.config/dot/config");
    if run_git_c(inputs.candidate, &["cat-file", "-e", &spec], false).is_some() {
        let preview = concat_join(inputs.candidate, "dot-config.preview");
        let shown = run_git_c(inputs.candidate, &["show", &spec], true).ok_or(Error::Usage {
            message: "candidate configuration is invalid",
        })?;
        std::fs::write(&preview, &shown).map_err(|source| Error::Io {
            context: "write config preview",
            source,
        })?;
        let (found_provider, found_policy, found_extensions) =
            load_config_values(&preview, inputs.home, inputs.source_root, stderr)?;
        provider = found_provider;
        policy = found_policy;
        if !found_extensions.is_empty() {
            extensions = found_extensions;
        }
    }
    if inputs.skip_provider && provider != "none" {
        provider.push_str(" (skipped for this invocation)");
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"dot init plan:\n");
    out.extend_from_slice(b"  repository: ");
    out.extend_from_slice(inputs.identity.as_bytes());
    out.extend_from_slice(b"\n  branch: ");
    out.extend_from_slice(inputs.branch.as_bytes());
    out.extend_from_slice(b"\n  tracked paths: ");
    out.extend_from_slice(count.as_bytes());
    out.extend_from_slice(b"\n  backup: ");
    out.extend_from_slice(inputs.backup.as_os_str().as_bytes());
    out.extend_from_slice(b"\n  dependency provider: ");
    out.extend_from_slice(provider.as_bytes());
    out.extend_from_slice(b"\n  shdeps update policy: ");
    out.extend_from_slice(policy.as_bytes());
    out.extend_from_slice(b"\n  extensions: ");
    out.extend_from_slice(extensions.as_bytes());
    out.extend_from_slice(b"\n");
    stderr.write_all(&out).map_err(|source| Error::Io {
        context: "write plan summary",
        source,
    })
}

/// One conflict-manifest row: the home-relative path plus the six
/// snapshot fields `_dot_init_snapshot_path` recorded for it. Empty
/// fields mirror the shell's `read` assignment when a row is short;
/// over-long rows keep their tail in `value`, exactly like the last
/// variable keeping its remainder.
pub struct ManifestEntry<'a> {
    /// Home-relative path; empty rows are skipped by both engines.
    pub path: &'a str,
    /// Snapshot kind (`regular`, `symlink`, `directory`, `absent`).
    pub kind: &'a str,
    /// Expected device number, as decimal text.
    pub dev: &'a str,
    /// Expected inode number, as decimal text.
    pub ino: &'a str,
    /// Expected mode bits, as the shell prints them.
    pub mode: &'a str,
    /// Expected size in bytes, as decimal text.
    pub size: &'a str,
    /// Expected content identity (blob hash, link target, or `-`).
    pub value: &'a str,
}

/// The candidate lane's `_dot_init_snapshot_path` state check
/// (`_dot_init_path_state_matches`), bound by the caller because that
/// lane is still unmerged: whether the tree state at `path` still
/// equals the snapshotted `(kind, dev, ino, mode, size, value)`. The
/// deletion lane crosses its verifier the same way, as a `&dyn Fn`
/// closure with the match arguments bound at each call site.
pub type StateMatches<'a> = &'a dyn Fn(&Path, &str, &str, &str, &str, &str, &str) -> bool;

/// Parse one manifest line into its seven fields with
/// [`read_tab_fields`] semantics.
fn parse_manifest_entry(line: &str) -> ManifestEntry<'_> {
    let fields = read_tab_fields(line, 7);
    ManifestEntry {
        path: fields[0],
        kind: fields[1],
        dev: fields[2],
        ino: fields[3],
        mode: fields[4],
        size: fields[5],
        value: fields[6],
    }
}

/// `_dot_init_move_conflicts`: move every conflicting home path in
/// `manifest` under `backup`, preserving the manifest rows for the
/// restore. The first call copies the manifest to
/// `backup/manifest` at mode 600; a later call with a byte-different
/// manifest fails instead of mixing generations. Rows whose
/// destination already matches (and whose home path is already gone)
/// are the interrupted-move residue and pass through; every other
/// row must still match at home before its exclusive move.
///
/// `home` backs the `$HOME/$path` joins, `source_root` backs the
/// manifest-equality hash sandbox, and `state_matches` is the
/// candidate lane's `_dot_init_path_state_matches`, bound by the
/// caller because that lane is still unmerged. Moves publish through
/// `cache`, exactly like the shell's `_dot_move_noreplace`. Tool
/// diagnostics (`mkdir`, `cp`, `chmod`) flow to `stderr`, which the
/// shell inherits for the same calls.
pub fn move_conflicts(
    manifest: &Path,
    backup: &Path,
    home: &Path,
    source_root: &Path,
    state_matches: StateMatches<'_>,
    cache: &mut temp::MoveCache,
    stderr: &mut dyn std::io::Write,
) -> Result<()> {
    ensure_private_dir(backup, stderr)?;
    let stored = concat_join(backup, "manifest");
    if exists_lexical(&stored) {
        let same = temp::files_equal(source_root, manifest, &stored).map_err(|_| Error::Usage {
            message: "conflict manifest changed under backup",
        })?;
        if !same {
            return Err(Error::Usage {
                message: "conflict manifest changed under backup",
            });
        }
    } else {
        run_fs_tool(
            "cp",
            &[manifest.as_os_str(), stored.as_os_str()],
            "copy conflict manifest",
            stderr,
        )?;
        run_fs_tool(
            "chmod",
            &[std::ffi::OsStr::new("0600"), stored.as_os_str()],
            "chmod conflict manifest",
            stderr,
        )?;
    }
    let bytes = std::fs::read(manifest).map_err(|source| Error::Io {
        context: "read conflict manifest",
        source,
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Usage {
        message: "conflict manifest is not UTF-8",
    })?;
    for line in shell_lines(text) {
        let entry = parse_manifest_entry(line);
        if entry.path.is_empty() {
            continue;
        }
        let destination = concat_join(backup, entry.path);
        let target = concat_join(home, entry.path);
        if state_matches(
            &destination,
            entry.kind,
            entry.dev,
            entry.ino,
            entry.mode,
            entry.size,
            entry.value,
        ) && !exists_lexical(&target)
        {
            continue;
        }
        if !state_matches(
            &target,
            entry.kind,
            entry.dev,
            entry.ino,
            entry.mode,
            entry.size,
            entry.value,
        ) {
            return Err(Error::Usage {
                message: "conflicting path changed before backup",
            });
        }
        if let Some(parent) = destination.parent() {
            run_fs_tool(
                "mkdir",
                &[std::ffi::OsStr::new("-p"), parent.as_os_str()],
                "create backup parent",
                stderr,
            )?;
        }
        if exists_lexical(&destination) {
            return Err(Error::Usage {
                message: "backup destination is occupied",
            });
        }
        temp::move_noreplace_cached(&target, &destination, cache)?;
    }
    Ok(())
}

/// `_dot_init_restore_backups`: move every parked generation recorded
/// in `backup/manifest` back home, deepest paths first (the manifest
/// sorts in reverse byte order, like `LC_ALL=C sort -r`). A missing
/// backup directory or manifest means nothing was ever parked and
/// restores nothing. Each parked source must still match its row, the
/// home path must be vacant, and the row's path must pass the base
/// sanitizer — then the exclusive move runs, recreating home parents
/// as needed. A manifest that vanishes after the gate reads as an
/// empty sort on the shell side, so an unreadable manifest restores
/// nothing here too; only non-UTF-8 bytes fail, matching the record
/// lane's fail-closed stance on unreachable writers' output.
pub fn restore_backups(
    backup: &Path,
    home: &Path,
    state_matches: StateMatches<'_>,
    cache: &mut temp::MoveCache,
    stderr: &mut dyn std::io::Write,
) -> Result<()> {
    let backup_meta = std::fs::symlink_metadata(backup);
    let manifest = concat_join(backup, "manifest");
    let manifest_meta = std::fs::metadata(&manifest);
    match (backup_meta, manifest_meta) {
        (Ok(dir), Ok(file)) => {
            if !dir.is_dir() || dir.file_type().is_symlink() || !file.is_file() {
                return Ok(());
            }
        }
        _ => return Ok(()),
    }
    let bytes = match std::fs::read(&manifest) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(()),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Usage {
        message: "backup manifest is not UTF-8",
    })?;
    let mut lines = shell_lines(text);
    lines.sort_by(|left, right| right.cmp(left));
    for line in lines {
        let entry = parse_manifest_entry(line);
        if !repos_overlays::init_safe_relative_path(entry.path) {
            return Err(Error::Usage {
                message: "backup path is not safe",
            });
        }
        let source = concat_join(backup, entry.path);
        if !exists_lexical(&source) {
            continue;
        }
        if !state_matches(
            &source,
            entry.kind,
            entry.dev,
            entry.ino,
            entry.mode,
            entry.size,
            entry.value,
        ) {
            return Err(Error::Usage {
                message: "parked path changed before restore",
            });
        }
        let target = concat_join(home, entry.path);
        if exists_lexical(&target) {
            return Err(Error::Usage {
                message: "restore destination is occupied",
            });
        }
        let parent = match entry.path.rsplit_once('/') {
            Some((dir, _)) => concat_join(home, dir),
            None => home.to_path_buf(),
        };
        run_fs_tool(
            "mkdir",
            &[std::ffi::OsStr::new("-p"), parent.as_os_str()],
            "create restore parent",
            stderr,
        )?;
        temp::move_noreplace_cached(&source, &target, cache)?;
    }
    Ok(())
}

/// `_dot_init_publish_completed`: publish the transaction `record` as
/// the durable completion marker. The marker path derives from the
/// XDG state root (`<state>/dot/init/completed`, via the shared
/// [`xdg`] primitive the transaction lane also builds on), its parent
/// is ensured private, and the record copies through a sibling temp
/// at mode 600. A live marker must be a regular file we own and is
/// replaced without following it; otherwise the publish is exclusive.
/// Like the shell, a failure after the sibling exists leaves the
/// sibling behind for the next run's cleanup. Tool diagnostics (`cp`,
/// `chmod`) flow to `stderr`, which the shell inherits for the same
/// calls.
pub fn publish_completed(
    record: &Path,
    home: &str,
    xdg_state_home: &str,
    cache: &mut temp::MoveCache,
    stderr: &mut dyn std::io::Write,
) -> Result<()> {
    let root = xdg::path(xdg::Kind::State, "dot/init", xdg_state_home, home).map_err(|_| {
        Error::Usage {
            message: "init state root is unresolvable",
        }
    })?;
    let completed = PathBuf::from(format!("{root}/completed"));
    if let Some(parent) = completed.parent() {
        ensure_private_dir(parent, stderr)?;
    }
    let temporary = temp::sibling_tmp_for(&completed)?;
    run_fs_tool(
        "cp",
        &[record.as_os_str(), temporary.as_os_str()],
        "copy completion record",
        stderr,
    )?;
    run_fs_tool(
        "chmod",
        &[std::ffi::OsStr::new("0600"), temporary.as_os_str()],
        "chmod completion record",
        stderr,
    )?;
    if exists_lexical(&completed) {
        let meta = std::fs::symlink_metadata(&completed).map_err(|source| Error::Io {
            context: "stat completion marker",
            source,
        })?;
        if !meta.is_file() || meta.file_type().is_symlink() || !owned_by_us(&completed) {
            return Err(Error::Usage {
                message: "completion marker is not ours",
            });
        }
        temp::move_replace_nodir_cached(&temporary, &completed, cache)
    } else {
        temp::move_noreplace_cached(&temporary, &completed, cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_fields_mirror_shell_read() {
        assert_eq!(read_tab_fields("a\tb\tc", 3), vec!["a", "b", "c"]);
        assert_eq!(
            read_tab_fields("a", 3),
            vec!["a", "", ""],
            "short rows leave the rest empty"
        );
        assert_eq!(
            read_tab_fields("a\t\tc", 7),
            vec!["a", "c", "", "", "", "", ""],
            "tab runs collapse instead of yielding empty fields"
        );
        assert_eq!(
            read_tab_fields("a\tb\t", 7),
            vec!["a", "b", "", "", "", "", ""],
            "trailing tabs strip before assignment"
        );
        assert_eq!(
            read_tab_fields("\ta\tb", 3),
            vec!["a", "b", ""],
            "leading tabs strip before assignment"
        );
        assert_eq!(
            read_tab_fields("1\t2\t3\t4\t5\t6\t7\t8", 7),
            vec!["1", "2", "3", "4", "5", "6", "7\t8"],
            "the last variable keeps its remainder raw"
        );
        assert_eq!(
            read_tab_fields("1\t2\t3\t4\t5\t6\t7\t\t8", 7),
            vec!["1", "2", "3", "4", "5", "6", "7\t\t8"],
            "the raw remainder keeps inner tab runs"
        );
        assert_eq!(
            read_tab_fields("a b\tc d", 3),
            vec!["a b", "c d", ""],
            "spaces are data under a tab-only IFS"
        );
        assert_eq!(
            read_tab_fields("", 3),
            vec!["", "", ""],
            "empty lines assign empty fields"
        );
        assert_eq!(
            read_tab_fields("\t\t", 2),
            vec!["", ""],
            "bare tab runs assign empty fields"
        );
        assert_eq!(
            read_tab_fields("anything", 0),
            Vec::<&str>::new(),
            "zero variables assign nothing"
        );
    }

    #[test]
    fn shell_lines_mirror_read_loop() {
        assert_eq!(shell_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(
            shell_lines("a\nb"),
            vec!["a", "b"],
            "a missing trailing newline still yields its final line"
        );
        assert_eq!(
            shell_lines("a\r\nb\r\n"),
            vec!["a\r", "b\r"],
            "returns stay put"
        );
        assert_eq!(shell_lines(""), Vec::<&str>::new());
    }

    #[test]
    fn confirm_answers_match_shell_literals() {
        for yes in ["y", "Y", "yes", "YES"] {
            assert!(confirm_answer_is_yes(yes), "{yes} accepts");
        }
        for no in ["", "n", "N", "Yes", "yes ", " y", "y\n", "y\r", "ye"] {
            assert!(!confirm_answer_is_yes(no), "{no:?} declines");
        }
    }
}
