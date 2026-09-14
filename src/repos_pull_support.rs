//! Pull support primitives from `lib/dot/repos/pull.sh`.
//!
//! The conflict-log parser, the timestamped backup directory maker,
//! the locale-pinned pull runner, the worker-fleet accounting, and
//! the upstream preparation. The conflict-backup orchestrator lives
//! in [`crate::repos_pull_backup`].

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
///
/// The shell's first `mkdir -p` is unguarded, so its diagnostics leak
/// to stderr while creation continues below; the port forks the same
/// tool and forwards those bytes to `warnings` verbatim (the
/// `date_stamp` precedent: forking costs what the shell pays and keeps
/// the bytes identical). The stamped `mkdir` and the `mktemp`
/// fallback stay suppressed on both sides.
pub fn backup_dir(home: &str, warnings: &mut dyn std::io::Write) -> Option<std::path::PathBuf> {
    use std::path::Path;
    let root = Path::new(home).join(".dot-backup/pull");
    match std::process::Command::new("mkdir")
        .arg("-p")
        .arg(&root)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let _ = warnings.write_all(&output.stderr);
        }
        // No `mkdir` to leak from: continue to the stamped attempt
        // like the shell continues after a failed lookup.
        Err(_) => {}
    }
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

/// One word of `printf %q` quoting, mirroring C-locale bash on raw
/// bytes: empty reads `''`, words of `[A-Za-z0-9_@%+=:,./-]` stay
/// literal, other printable bytes take bare backslash escapes, and
/// anything else (controls, DEL, non-ASCII) takes the `$'...'`
/// form with C mnemonics (`\a\b\E\f\n\r\t\v`) or octal escapes.
/// Non-ASCII bytes always take octal here; under a UTF-8 locale bash
/// would print them literally, but both spellings re-parse to the
/// same bytes, so the quoted command stays correct everywhere.
pub fn shell_quote(text: &[u8]) -> String {
    fn safe(byte: u8) -> bool {
        matches!(
            byte,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'_'
                | b'@'
                | b'%'
                | b'+'
                | b'='
                | b':'
                | b','
                | b'.'
                | b'/'
                | b'-'
        )
    }
    if text.is_empty() {
        return "''".to_string();
    }
    if text.iter().all(|byte| safe(*byte)) {
        return String::from_utf8_lossy(text).into_owned();
    }
    if text.iter().all(|byte| (0x20..0x7f).contains(byte)) {
        let mut out = String::new();
        for byte in text {
            if safe(*byte) {
                out.push(*byte as char);
            } else {
                out.push('\\');
                out.push(*byte as char);
            }
        }
        return out;
    }
    let mut out = String::from("$'");
    for byte in text {
        match byte {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0a => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            0x0d => out.push_str("\\r"),
            0x1b => out.push_str("\\E"),
            b'\'' => out.push_str("\\'"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(*byte as char),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out.push('\'');
    out
}

/// The adoption command for an origin mismatch: `remote add` for a
/// missing origin, `config --replace-all` for multiple URLs, else
/// `remote set-url`. Paths and URLs go through [`shell_quote`].
pub fn adopt_command(path: &str, expected: &str, actual: &str) -> String {
    let path = shell_quote(path.as_bytes());
    let expected = shell_quote(expected.as_bytes());
    match actual {
        "<missing>" => format!("git -C {path} remote add origin {expected}"),
        "<multiple origin URLs>" => {
            format!("git -C {path} config --replace-all remote.origin.url {expected}")
        }
        _ => format!("git -C {path} remote set-url origin {expected}"),
    }
}

/// Inputs for [`origin_mismatch`]: the overlay identity, the
/// observed state, and the raw UI flags.
pub struct OriginMismatch<'a> {
    /// Overlay name for messages.
    pub name: &'a str,
    /// Checkout path, quoted into the adopt command.
    pub path: &'a str,
    /// Configured URL, quoted into the adopt command.
    pub expected: &'a str,
    /// Observed state: `<missing>`, `<multiple origin URLs>`, or a URL.
    pub actual: &'a str,
    /// `DOT_UI_TOTAL`: counted UI takes status rows when `> 0`.
    pub ui_total: Option<&'a str>,
    /// `DOT_QUIET`: status rows stay silent at arithmetic 1, like
    /// `_ui_status`; warnings always print.
    pub quiet: Option<&'a str>,
}

/// `_overlay_origin_mismatch`: explain the mismatch and show the
/// adoption command, as warning status rows under counted UI or as
/// stderr warnings otherwise. Returns `(stdout, stderr,
/// live_active)`; the flag always drains because both rows clear
/// through it in turn.
pub fn origin_mismatch(
    palette: &crate::progress_ui::Palette,
    live_active: bool,
    multibyte: bool,
    details: &OriginMismatch<'_>,
) -> (Vec<u8>, Vec<u8>, bool) {
    let adopt = adopt_command(details.path, details.expected, details.actual);
    let quiet = crate::progress_ui::arith_value(details.quiet.unwrap_or("0")) == Some(1);
    if details
        .ui_total
        .and_then(crate::progress_ui::arith_value)
        .is_some_and(|total| total > 0)
    {
        let first = format!(
            "{} overlay origin mismatch: expected {}, found {}",
            details.name, details.expected, details.actual
        );
        let second = format!("verify the checkout, then adopt it with: {adopt}");
        let (mut out, live) = crate::progress_ui::status(
            palette,
            quiet,
            live_active,
            b"warning",
            first.as_bytes(),
            multibyte,
        );
        let (rest, live) = crate::progress_ui::status(
            palette,
            quiet,
            live,
            b"warning",
            second.as_bytes(),
            multibyte,
        );
        out.extend_from_slice(&rest);
        return (out, Vec::new(), live);
    }
    let mut err = Vec::new();
    for line in [
        format!(
            "  warning: {} overlay origin does not match its configured URL",
            details.name
        ),
        format!("    expected: {}", details.expected),
        format!("    found:    {}", details.actual),
        format!("    verify the checkout, then adopt it with: {adopt}"),
    ] {
        err.extend_from_slice(palette.yellow.as_bytes());
        err.extend_from_slice(line.as_bytes());
        err.extend_from_slice(palette.reset.as_bytes());
        err.push(b'\n');
    }
    (Vec::new(), err, live_active)
}
