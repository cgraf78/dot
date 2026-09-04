//! Pull support primitives from `lib/dot/repos/pull.sh`.
//!
//! The conflict-log parser, the timestamped backup directory maker,
//! the locale-pinned pull runner, the worker-fleet accounting, and
//! the upstream preparation. The conflict-backup orchestrator stays
//! shell-side until the overlay-quarantine helpers it calls are
//! ported.

/// `_pull_conflicts_from_log`: list the untracked files a failed pull
/// names after its "untracked working tree files would be overwritten
/// by" marker, one per line.
///
/// Ports the embedded awk program line for line: lines before the
/// marker are ignored; a line of leading whitespace followed by a
/// non-space character emits with the whitespace run stripped; the
/// first other line after the marker ends the listing (like awk's
/// `exit`). The marker line itself never emits.
pub fn conflicts_from_log(log: &str) -> Vec<String> {
    const MARKER: &str = "untracked working tree files would be overwritten by";
    let mut files = Vec::new();
    let mut in_conflicts = false;
    for line in log.split('\n') {
        if !in_conflicts {
            if line.contains(MARKER) {
                in_conflicts = true;
            }
            continue;
        }
        // POSIX `[:space:]` exactly (space, \t, \n, \v, \f, \r):
        // Unicode whitespace must not strip, like the shell.
        let stripped = line.trim_start_matches([' ', '\t', '\n', '\x0B', '\x0C', '\r']);
        if !stripped.is_empty() && stripped.len() != line.len() {
            files.push(stripped.to_string());
        } else {
            break;
        }
    }
    files
}

/// Current `%Y%m%d%H%M%S` stamp from `date`, exactly like the shell:
/// `std` has no timezone-aware calendar, and forking `date` costs
/// the same fork the shell pays. `None` when `date` fails (callers
/// degrade exactly like the shell's empty substitution).
fn date_stamp() -> Option<String> {
    std::process::Command::new("date")
        .arg("+%Y%m%d%H%M%S")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string()
        })
}

/// `_backup_dir`: create `$HOME/.dot-backup/pull/<stamp>` and report
/// it (`Some`), falling back to `mktemp -d` when the stamped name
/// collides and to `None` when nothing is creatable — the shell's
/// `REPLY=""` plus exit 1. A failed `date` degrades exactly like the
/// shell's empty command substitution (the join keeps the root, whose
/// `mkdir` then succeeds on the existing directory).
pub fn backup_dir(home: &str) -> Option<std::path::PathBuf> {
    use std::path::Path;
    let root = Path::new(home).join(".dot-backup/pull");
    // Like the shell's unguarded `mkdir -p`: best effort, failures
    // surface at the stamped `mkdir`/`mktemp` below.
    let _ = std::fs::create_dir_all(&root);
    let stamp = date_stamp().unwrap_or_default();
    let backup = root.join(stamp);
    if std::fs::create_dir(&backup).is_ok() {
        return Some(backup);
    }
    let template = format!("{}.XXXXXX", backup.display());
    std::process::Command::new("mktemp")
        .args(["-d", &template])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            std::path::PathBuf::from(
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string(),
            )
        })
}

/// `_pull_cmd`: run `program` with `args` under `LC_ALL=C`, appending
/// `--quiet` in quiet mode.
///
/// Stdio inherits like the shell (a pull may prompt and always
/// streams); only the locale is pinned, because the conflict-backup
/// detector and the quiet-output filter match literal English git
/// messages. Returns the child exit code, 127 when spawning fails.
pub fn pull_cmd(quiet: bool, program: &str, args: &[&str]) -> i32 {
    let mut argv: Vec<&str> = Vec::with_capacity(args.len() + 1);
    argv.extend_from_slice(args);
    if quiet {
        argv.push("--quiet");
    }
    match std::process::Command::new(program)
        .args(&argv)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
    {
        Ok(status) => status.code().unwrap_or(127),
        Err(_) => 127,
    }
}

/// `_pull_overlay_result_prefix`: `<dir>/<idx>` with the worker
/// index zero-padded to three, exactly like `printf %s/%03d`.
/// `idx` is always a worker counter in practice.
pub fn result_prefix(dir: &str, idx: i64) -> String {
    format!("{dir}/{idx:03}")
}

/// Worker-fleet tally behind `_pull_overlay_record_status`:
/// per-status counters plus the changed-items accumulator the
/// deferred stage finish reports through.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PullTally {
    /// `DOT_PULL_OVERLAY_FAILED`.
    pub failed: u64,
    /// `DOT_PULL_OVERLAY_CHANGED`.
    pub changed: u64,
    /// `DOT_PULL_OVERLAY_SKIPPED`.
    pub skipped: u64,
    /// `DOT_PULL_OVERLAY_CURRENT`.
    pub current: u64,
    /// `DOT_PULL_OVERLAY_CHANGED_ITEMS`, newline-terminated lines.
    pub changed_items: String,
}

/// `_pull_overlay_record_status`: bump the tally for `status` and
/// return the `"<name> <status>"` summary line, or `None` for an
/// empty status (the shell returns before appending). Unknown
/// statuses still summarize but tally nothing, like the shell's
/// `case` fall-through.
pub fn record_status(name: &str, status: &str, tally: &mut PullTally) -> Option<String> {
    if status.is_empty() {
        return None;
    }
    match status {
        "failed" => tally.failed += 1,
        "changed" => {
            tally.changed += 1;
            tally.changed_items.push_str(name);
            tally.changed_items.push_str(" dotfiles updated\n");
        }
        "cloned" => {
            tally.changed += 1;
            tally.changed_items.push_str(name);
            tally.changed_items.push_str(" dotfiles cloned\n");
        }
        "skipped" => tally.skipped += 1,
        "current" => tally.current += 1,
        _ => {}
    }
    Some(format!("{name} {status}"))
}

/// `_pull_overlay_active`: a live worktree, or any entry with a
/// configured URL. The shell also receives the overlay name and the
/// optional flag, but neither decides.
pub fn overlay_active(path: &std::path::Path, url: &str) -> bool {
    crate::overlays::is_worktree(path) || !url.is_empty()
}

/// `_pull_overlay_count`: entries with a `git` sync (the default
/// when the field is empty) that are active. Entries split like
/// `IFS='|' read ...` over `name|path|url|conf|optional|sync`,
/// with any surplus fields folded into the sync field.
pub fn overlay_count(entries: &[&str]) -> usize {
    entries
        .iter()
        .filter(|entry| {
            let mut fields = entry.split('|');
            let _name = fields.next().unwrap_or("");
            let path = fields.next().unwrap_or("");
            let url = fields.next().unwrap_or("");
            let _conf = fields.next().unwrap_or("");
            let _optional = fields.next().unwrap_or("");
            let rest = fields.collect::<Vec<_>>().join("|");
            let sync = if rest.is_empty() {
                "git"
            } else {
                rest.as_str()
            };
            if sync != "git" {
                return false;
            }
            overlay_active(std::path::Path::new(path), url)
        })
        .count()
}

/// Resolve `@{u}` to its remote name, or `None` when there is no
/// usable `remote/branch` shape (missing slash, empty remote).
fn upstream_remote(upstream: &str) -> Option<&str> {
    let (remote, branch) = upstream.split_once('/')?;
    if remote.is_empty() || branch.is_empty() {
        return None;
    }
    Some(remote)
}

/// Run `git rev-parse` under `prefix`, returning trimmed stdout on
/// success (empty on any failure, like `$(... || true)` downstream
/// of an `|| return`).
fn rev_parse(prefix: &[std::ffi::OsString], args: &[&str]) -> Option<String> {
    let mut full = vec!["rev-parse"];
    full.extend_from_slice(args);
    let output = crate::repos_base::run_git(prefix, &full)?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(text)
}

/// `_repo_prepare_base_upstream`: fetch the base checkout's upstream
/// remote and resolve the fetched tip. Returns the commit id, or the
/// shell's numeric failure: 1 for no usable upstream, 2 for a failed
/// fetch, 3 for an unresolvable tip.
pub fn prepare_base_upstream(base: &crate::repos_base::Base) -> Result<String, u8> {
    let prefix = base.git_prefix().ok_or(1u8)?;
    let upstream =
        rev_parse(&prefix, &["--abbrev-ref", "--symbolic-full-name", "@{u}"]).ok_or(1u8)?;
    let remote = upstream_remote(&upstream).ok_or(1u8)?;
    if crate::repos_git::run_git_streaming(
        &prefix,
        &["fetch", "--quiet", "--no-write-fetch-head", remote],
    ) != 0
    {
        return Err(2);
    }
    let tip = format!("{upstream}^{{commit}}");
    rev_parse(&prefix, &["--verify", tip.as_str()]).ok_or(3u8)
}

/// `_repo_prepare_overlay_upstream`: fetch one overlay's upstream
/// remote and resolve the fetched tip. Same shape as
/// [`prepare_base_upstream`], except an unresolvable tip is also a 2
/// and fetch diagnostics stay quiet when `quiet_errors` holds.
pub fn prepare_overlay_upstream(path: &std::path::Path, quiet_errors: bool) -> Result<String, u8> {
    let prefix = vec![std::ffi::OsString::from("-C"), path.as_os_str().to_owned()];
    let upstream =
        rev_parse(&prefix, &["--abbrev-ref", "--symbolic-full-name", "@{u}"]).ok_or(1u8)?;
    let remote = upstream_remote(&upstream).ok_or(1u8)?;
    let fetch = ["fetch", "--quiet", "--no-write-fetch-head", remote];
    let fetched = if quiet_errors {
        crate::repos_base::run_git(&prefix, &fetch).is_some_and(|output| output.status.success())
    } else {
        crate::repos_git::run_git_streaming(&prefix, &fetch) == 0
    };
    if !fetched {
        return Err(2);
    }
    let tip = format!("{upstream}^{{commit}}");
    rev_parse(&prefix, &["--verify", tip.as_str()]).ok_or(2u8)
}
