//! Pull support primitives from `lib/dot/repos/pull.sh`.
//!
//! The conflict-log parser, the timestamped backup directory maker,
//! and the locale-pinned pull runner. The conflict-backup
//! orchestrator stays shell-side until the overlay-quarantine
//! helpers it calls are ported.

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
