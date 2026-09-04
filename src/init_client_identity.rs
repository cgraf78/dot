//! Host Git selection and repository identity, part 2 of
//! `lib/dot/init-client.sh`: the pinned host `git` executable outside
//! both client-controlled roots, its shell-function guard, the client
//! repository identity normalization, branch-name validation, and
//! remote default-branch resolution.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the five helpers from `_dot_init_select_host_git`
//! through `_dot_init_remote_default_branch`. Part 1 (the
//! transaction-directory lifecycle) lives on its own lane, the
//! `_dot_init_safe_value` / `_dot_init_safe_relative_path` predicates
//! already live behind [`crate::repos_overlays`], and the file-generic
//! `_dot_init_error` diagnostic stays unported: a bare
//! `printf 'dot init: %s\n' ... >&2; return 1` with no family state,
//! absorbed here into the `Result` payloads [`NO_HOST_GIT`] /
//! [`GIT_SHADOWED`], which the caller renders with the same
//! `dot init: ` prefix. Record, candidate, generation, claim, and
//! rollback families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::temp::{TMP_RETRIES, random_suffix};

/// `_dot_init_error` payload when no host Git survives the
/// client-root exclusions. Rendered as `dot init: {NO_HOST_GIT}` on
/// stderr, exactly like the shell.
pub const NO_HOST_GIT: &str = "host Git is unavailable outside HOME and the Dot checkout";

/// `_dot_init_error` payload when a shell function named `git`
/// shadows the executable. Rendered as `dot init: {GIT_SHADOWED}`
/// on stderr, exactly like the shell.
pub const GIT_SHADOWED: &str = "a shell function named git cannot be used during initialization";

/// `_dot_init_safe_value`: nonempty with no tab, newline, or
/// carriage-return bytes. The same rule already guards
/// [`crate::repos_overlays`]; it is repeated here (not imported)
/// because that copy is private to its own call sites and this
/// family needs only the bare predicate, never the path form.
fn safe_value(value: &str) -> bool {
    !value.is_empty() && !value.contains(['\t', '\n', '\r'])
}

/// `cd -P -- dir && pwd -P`: the physical directory with symlinks,
/// `.`, and `..` resolved. `canonicalize` fails on the same inputs
/// the shell `cd` rejects (missing or unreachable directories, the
/// empty string), so `None` carries the shell's `return 1`.
fn physical(path: &str) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Strict parent resolution for [`realpath_default`]: every lexical
/// component must exist, with symlinks in leading positions
/// followed as they are met (like `realpath` itself). A `..`
/// therefore cannot smuggle past a missing directory the way a
/// plain `canonicalize` of the parent would allow.
fn strict_resolve(path: &Path) -> Option<PathBuf> {
    use std::path::Component::{CurDir, Normal, ParentDir, Prefix, RootDir};
    let last = path.components().next_back();
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Prefix(_) | RootDir => out = PathBuf::from("/"),
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            Normal(part) => {
                out.push(part);
                let meta = std::fs::symlink_metadata(&out).ok()?;
                if meta.file_type().is_symlink() {
                    out = std::fs::canonicalize(&out).ok()?;
                } else if !meta.is_dir() && Some(component) != last {
                    // A leading file blocks descent, like
                    // `realpath`'s not-a-directory failure.
                    return None;
                }
            }
        }
    }
    std::fs::symlink_metadata(&out).ok()?;
    Some(out)
}

/// coreutils `realpath` without flags: every component but the last
/// must exist, so a missing leaf still resolves against its
/// parent (while two missing components, or a `..` past a missing
/// directory, fail). `canonicalize` demands the whole path, hence
/// the strict-parent fallback; a trailing `..`/`.` with nothing
/// resolvable refuses, like the shell.
fn realpath_default(path: &str) -> Option<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }
    let path = Path::new(path);
    let leaf = path.file_name()?;
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    let mut resolved = strict_resolve(parent)?;
    // The parent must stay a directory: a leaf under a file is
    // `realpath`'s not-a-directory failure, not a resolution.
    if !resolved.is_dir() {
        return None;
    }
    resolved.push(leaf);
    Some(resolved)
}

/// Whether `candidate` is usable as the pinned host Git: a regular
/// file, never a symlink (`[[ -f ... && ! -L ... ]]`; the metadata
/// read never follows links, so a link to an executable still
/// fails), with any execute bit set (`-x`). The bit test matches
/// `access(X_OK)` exactly for root and for owner-held fixtures; a
/// group/other-only executable owned by the caller is the one
/// corner where the bits over-accept, and no caller constructs it.
fn is_host_candidate(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::symlink_metadata(candidate) {
        Ok(meta) => {
            meta.file_type().is_file()
                && !meta.file_type().is_symlink()
                && meta.permissions().mode() & 0o111 != 0
        }
        Err(_) => false,
    }
}

/// Whether `candidate` falls under `root` (equal to it, or beneath
/// it with a `/` boundary), mirroring the shell's `== root` /
/// `== root/*` pair. A root of `/` excludes everything, exactly
/// like the shell's leading `$root == /` disjunct.
fn under_root(candidate: &str, root: &str) -> bool {
    if root == "/" {
        return true;
    }
    candidate == root || candidate.starts_with(&format!("{root}/"))
}

/// `_dot_init_select_host_git`: first `git` on `path` (a raw
/// `$PATH` value, colon-split like the shell's `IFS=: read -a`)
/// that is an absolute directory resolving to a regular executable
/// file outside `home` and `source_root` (raw `$HOME` and
/// `$DOT_SOURCE_ROOT` values, resolved physically like the shell's
/// `cd -P`). Relative and unresolvable directories are skipped,
/// and a bare `/` directory probes `/git` (never `//git`).
/// Returns the selected path (`$REPLY` in the shell); `None` is the
/// shell's `return 1` with `REPLY` left empty.
pub fn select_host_git(home: &str, source_root: &str, path: &str) -> Option<String> {
    let home = physical(home)?;
    let source = physical(source_root)?;
    let home = home.to_string_lossy();
    let source = source.to_string_lossy();
    for directory in path.split(':') {
        if !directory.starts_with('/') {
            continue;
        }
        let physical = match physical(directory) {
            Some(physical) => physical,
            None => continue,
        };
        let physical = physical.to_string_lossy();
        let candidate = if physical == "/" {
            String::from("/git")
        } else {
            format!("{physical}/git")
        };
        if !is_host_candidate(Path::new(&candidate)) {
            continue;
        }
        if under_root(&candidate, &home) {
            continue;
        }
        if under_root(&candidate, &source) {
            continue;
        }
        return Some(candidate);
    }
    None
}

/// `_dot_init_bind_host_git`: pin the [`select_host_git`] result for
/// the whole transaction, or fail with the shell's stderr payload
/// ([`NO_HOST_GIT`] when nothing is selectable, [`GIT_SHADOWED`]
/// when a shell function named `git` would intercept the call).
/// `git_shadowed` injects the `declare -F git` probe: Rust has no
/// shell functions, so the engine passes `false` and only the
/// differential tests drive `true` for parity. The shell's
/// `hash -p` / `set -h` table update has no Rust spelling — there
/// is no shell hash table to mutate — so success returns the path
/// for the caller to invoke instead of mutating process-global
/// shell state, which is precisely the class of state the port
/// eliminates.
pub fn bind_host_git(
    home: &str,
    source_root: &str,
    path: &str,
    git_shadowed: bool,
) -> Result<String, &'static str> {
    let host = select_host_git(home, source_root, path).ok_or(NO_HOST_GIT)?;
    if git_shadowed {
        return Err(GIT_SHADOWED);
    }
    Ok(host)
}

/// Strip every trailing `/` (`while [[ $path == */ ]]`), then one
/// `.git` suffix (`${path%.git}`), exactly like the identity
/// normalizer. An all-slash input strips to empty and fails its
/// caller, like the shell's `-n` guard.
fn clean_identity_path(path: &str) -> &str {
    path.trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_end_matches('/'))
}

/// Lowercase ASCII-only like `${host,,}` under `LC_ALL=C` (never
/// the Unicode Kelvin-sign fold `to_lowercase` would add).
fn lower_host(host: &str) -> String {
    host.to_ascii_lowercase()
}

/// `_dot_init_repo_identity`: normalize a repository URL to its
/// canonical identity. `file://` URLs and absolute paths resolve
/// through `realpath` (with its missing-leaf tolerance, see the
/// private helper below) and print as `file://<resolved>`;
/// `http(s)://`, `ssh://`, and scp-like `host:path` shapes print
/// as `git://<lowercased-host>/<cleaned-path>`. Only lowercase
/// scheme prefixes special-case — an uppercase `SSH://` falls
/// through to the scp-like arm, exactly like the shell's
/// case-sensitive patterns — and only the host lowercases, never
/// the path. `None` is the shell's silent `return 1` (unsafe
/// values, relative `file://` targets, userinfo or ports on
/// `http(s)`, colons in `ssh` hosts, empty hosts or paths).
pub fn repo_identity(url: &str) -> Option<String> {
    if !safe_value(url) {
        return None;
    }
    if let Some(path) = url.strip_prefix("file://") {
        if !path.starts_with('/') {
            return None;
        }
        let resolved = realpath_default(path)?;
        return Some(format!("file://{}", resolved.to_string_lossy()));
    }
    if url.starts_with('/') {
        let resolved = realpath_default(url)?;
        return Some(format!("file://{}", resolved.to_string_lossy()));
    }
    if let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    {
        let (host, path) = rest.split_once('/')?;
        if host.is_empty() || path.is_empty() {
            return None;
        }
        if host.contains(['@', ':']) {
            return None;
        }
        let path = clean_identity_path(path);
        if path.is_empty() {
            return None;
        }
        return Some(format!("git://{}/{}", lower_host(host), path));
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let (mut host, path) = rest.split_once('/')?;
        if host.is_empty() || path.is_empty() {
            return None;
        }
        // `userinfo=${host%@*}` keeps through the LAST `@`; the
        // host strip below removes through the FIRST.
        let userinfo = match host.rfind('@') {
            Some(idx) => &host[..idx],
            None => host,
        };
        if host.contains('@') {
            if let Some(idx) = host.find('@') {
                host = &host[idx + 1..];
            }
        }
        if userinfo.contains(':') || host.contains(':') {
            return None;
        }
        let path = clean_identity_path(path);
        if path.is_empty() {
            return None;
        }
        return Some(format!("git://{}/{}", lower_host(host), path));
    }
    if let Some(idx) = url.find(':') {
        let (mut host, mut path) = (&url[..idx], &url[idx + 1..]);
        if host.is_empty() || path.is_empty() || host.contains('/') {
            return None;
        }
        if let Some(at) = host.find('@') {
            host = &host[at + 1..];
        }
        path = path.trim_start_matches('/');
        path = path.trim_end_matches('/');
        path = path.strip_suffix(".git").unwrap_or(path);
        if host.is_empty() || path.is_empty() {
            return None;
        }
        return Some(format!("git://{}/{}", lower_host(host), path));
    }
    None
}

/// `_dot_init_branch_valid`: nonempty and accepted by
/// `git check-ref-format --branch` (stderr nulled, like the shell).
/// The emptiness short-circuit avoids the spawn, like the shell's
/// `[[ -n $1 ]] &&`. The shell reports git's raw exit code (128
/// for a malformed name, 1 for empty); every in-repo caller only
/// branches on zero versus nonzero, so the port reports the same
/// verdict as a boolean instead of threading git's code.
pub fn branch_valid(branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    git_status(&["check-ref-format", "--branch", branch])
}

/// Run `git` with `args` under `LC_ALL=C` (stdin null, stdio
/// nulled for inspections, stderr nulled like every shell caller
/// here) and report success. `false` covers spawn failure and
/// non-zero exit alike, like the shell's `||` chains.
fn git_status(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run `git` with `args` (borrowed `OsStr` for caller-owned paths)
/// under `LC_ALL=C`, capturing stdout with stderr nulled.
/// `None` on spawn failure; callers check the exit status, like the
/// shell's `$(... || true)` captures.
fn git_output<S: AsRef<OsStr>>(args: &[S]) -> Option<std::process::Output> {
    Command::new("git")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
}

/// Command-substitution trimming: every trailing newline strips,
/// like `$(...)`.
fn chomped(output: &[u8]) -> &str {
    let mut text = output;
    while let Some(rest) = text.strip_suffix(b"\n") {
        text = rest;
    }
    std::str::from_utf8(text).unwrap_or("")
}

/// Whether `text` is a well-formed object id (40 or 64 hex
/// digits), like the shell's `^[0-9a-fA-F]{40,64}$`.
fn is_oid(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Shell `while read` lines without the `|| [[ -n $line ]]`
/// guard: split on newlines, dropping an unterminated tail (the
/// shell's failed final `read` never runs its body). Carriage
/// returns stay data — unlike `str::lines`, nothing strips them —
/// and the empty segment behind a trailing newline processes
/// harmlessly on both sides.
fn shell_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        lines.truncate(lines.len().saturating_sub(1));
    }
    lines
}

/// Allocate an unused stage path under `scratch` for the probe
/// clone. The shell's `_dot_cleanup_mktemp -d` template
/// (`$TMPDIR/dot.XXXXXXXX`) is allocator-internal — only success
/// versus failure crosses the boundary — so this draws from the
/// shared mktemp alphabet with the shared retry budget instead.
/// `scratch` itself is never created: a missing parent fails like
/// the shell's failed `mktemp`.
fn fresh_stage(scratch: &Path) -> Option<PathBuf> {
    for _ in 0..TMP_RETRIES {
        let stage = scratch.join(format!("dot-init-remote.{}", random_suffix()));
        match std::fs::create_dir(&stage) {
            Ok(()) => return Some(stage),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                // Taken: the next iteration tries a fresh suffix.
            }
            Err(_) => return None,
        }
    }
    None
}

/// Remove the probe stage, mirroring `_dot_cleanup_remove_path`
/// (`rm -rf`): absent paths succeed, links remove the link, and
/// only a failed removal fails.
fn remove_stage(stage: &Path) -> bool {
    match std::fs::symlink_metadata(stage) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
        Ok(meta) => {
            let removed = if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                std::fs::remove_dir_all(stage)
            } else {
                std::fs::remove_file(stage)
            };
            removed.is_ok()
        }
    }
}

/// Parse one `ls-remote --symref` line (`ref<TAB>oid`, the shell's
/// `IFS=$'\t' read -r ref oid`): a `ref: refs/heads/` advertised
/// head updates `branch`, a hex first field with a `HEAD` second
/// field updates `head_oid`. A line without a tab reads its whole
/// body as the first field with an empty second, exactly like
/// `read` with two variables.
fn parse_advertised(line: &str, branch: &mut String, head_oid: &mut String) {
    let (first, second) = match line.split_once('\t') {
        Some(pair) => pair,
        None => (line, ""),
    };
    if let Some(name) = first.strip_prefix("ref: refs/heads/") {
        branch.clear();
        branch.push_str(name);
    } else if !first.is_empty()
        && first.bytes().all(|byte| byte.is_ascii_hexdigit())
        && second == "HEAD"
    {
        head_oid.clear();
        head_oid.push_str(first);
    }
}

/// `_dot_init_remote_default_branch`: resolve the remote's default
/// branch for `url` through three strategies, first hit wins: the
/// `origin/HEAD` symbolic ref of a `--no-checkout` probe clone
/// (verified with `show-ref`, like the shell), the `ls-remote
/// --symref` advertisement (a branch-valid head with a well-formed
/// head object), then the probe clone's remote branches
/// (`main` preferred, else the lone branch; any branch-valid
/// failure aborts, like the shell). `scratch` is the mktemp parent
/// (the shell's `${TMPDIR:-/tmp}` cleanup allocator); stage names
/// beneath it are internal and always removed before returning.
/// `None` is the shell's silent `return 1`: unclonable URLs,
/// unreadable advertisements, ambiguous branches, and failed stage
/// removals all refuse the same way.
pub fn remote_default_branch(url: &str, scratch: &Path) -> Option<String> {
    let stage = fresh_stage(scratch)?;
    // `_dot_cleanup_mktemp -d` publishes a directory, then the
    // shell `rmdir`s it so the clone below creates it fresh.
    if std::fs::remove_dir(&stage).is_err() {
        return None;
    }
    let stage_text = stage.to_string_lossy().into_owned();
    let clone_args: [&OsStr; 6] = [
        OsStr::new("clone"),
        OsStr::new("--quiet"),
        OsStr::new("--no-checkout"),
        OsStr::new("--"),
        OsStr::new(url),
        OsStr::new(&stage_text),
    ];
    let clone_ok = git_output(&clone_args).is_some_and(|output| output.status.success());
    let mut selected = String::new();
    if clone_ok {
        let symref_args: [&OsStr; 5] = [
            OsStr::new("-C"),
            OsStr::new(&stage_text),
            OsStr::new("symbolic-ref"),
            OsStr::new("--short"),
            OsStr::new("refs/remotes/origin/HEAD"),
        ];
        let mut branch = git_output(&symref_args)
            .filter(|output| output.status.success())
            .map(|output| chomped(&output.stdout).to_string())
            .unwrap_or_default();
        branch = branch
            .strip_prefix("origin/")
            .unwrap_or(&branch)
            .to_string();
        if branch_valid(&branch) {
            let wanted = format!("refs/remotes/origin/{branch}");
            let check_args: [&OsStr; 6] = [
                OsStr::new("-C"),
                OsStr::new(&stage_text),
                OsStr::new("show-ref"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new(&wanted),
            ];
            if git_output(&check_args).is_some_and(|output| output.status.success()) {
                selected = branch;
            }
        }
    }
    if selected.is_empty() {
        // The shell spills the advertisement through a temp file;
        // only its bytes cross the boundary, so capture directly.
        // A failed `ls-remote` skips parsing, like the shell's
        // `if`, and the temp-file removal (`|| true`) needs no
        // spelling when nothing was created.
        let ls_args: [&OsStr; 6] = [
            OsStr::new("ls-remote"),
            OsStr::new("--symref"),
            OsStr::new("--exit-code"),
            OsStr::new("--"),
            OsStr::new(url),
            OsStr::new("HEAD"),
        ];
        if let Some(output) = git_output(&ls_args) {
            if output.status.success() {
                let mut branch = String::new();
                let mut head_oid = String::new();
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                for line in shell_lines(&text) {
                    parse_advertised(line, &mut branch, &mut head_oid);
                }
                if branch_valid(&branch) && is_oid(&head_oid) {
                    selected = branch;
                }
            }
        }
    }
    if selected.is_empty() && clone_ok {
        let list_args: [&OsStr; 4] = [
            OsStr::new("-C"),
            OsStr::new(&stage_text),
            OsStr::new("for-each-ref"),
            OsStr::new("--format=%(refname:strip=3)"),
        ];
        let mut branches: Vec<String> = Vec::new();
        if let Some(output) = git_output(&list_args) {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                for line in shell_lines(&text) {
                    if line.is_empty() || line == "HEAD" {
                        continue;
                    }
                    if !branch_valid(line) {
                        remove_stage(&stage);
                        return None;
                    }
                    branches.push(line.to_string());
                }
            }
        }
        if branches.iter().any(|branch| branch == "main") {
            selected = String::from("main");
        } else if branches.len() == 1 {
            selected = branches.pop().unwrap_or_default();
        }
    }
    if !remove_stage(&stage) {
        return None;
    }
    if !branch_valid(&selected) {
        return None;
    }
    Some(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_lines_update_branch_and_oid() {
        let mut branch = String::new();
        let mut head_oid = String::new();
        parse_advertised("ref: refs/heads/main\tHEAD", &mut branch, &mut head_oid);
        assert_eq!((branch.as_str(), head_oid.as_str()), ("main", ""));
        parse_advertised(
            "68cdca099e3d29e5e6e57c575576bc6ae10ac650\tHEAD",
            &mut branch,
            &mut head_oid,
        );
        assert_eq!(
            (branch.as_str(), head_oid.as_str()),
            ("main", "68cdca099e3d29e5e6e57c575576bc6ae10ac650")
        );
    }

    #[test]
    fn advertised_lines_ignore_noise() {
        let mut branch = String::from("keep");
        let mut head_oid = String::from("keep");
        // No tab: the whole line is the first field, like `read`
        // with two variables; a bare hex line is not a head without
        // its `HEAD` second field.
        parse_advertised(
            "68cdca099e3d29e5e6e57c575576bc6ae10ac650",
            &mut branch,
            &mut head_oid,
        );
        // Non-head refs and short oids never move the parse.
        parse_advertised(
            "68cdca099e3d29e5e6e57c575576bc6ae10ac650\trefs/heads/main",
            &mut branch,
            &mut head_oid,
        );
        parse_advertised("xyz\tHEAD", &mut branch, &mut head_oid);
        parse_advertised("", &mut branch, &mut head_oid);
        assert_eq!((branch.as_str(), head_oid.as_str()), ("keep", "keep"));
    }

    #[test]
    fn shell_lines_drop_only_an_unterminated_tail() {
        assert_eq!(shell_lines(""), Vec::<&str>::new());
        assert_eq!(shell_lines("a\n"), vec!["a", ""]);
        assert_eq!(shell_lines("a"), Vec::<&str>::new());
        assert_eq!(shell_lines("a\nb"), vec!["a"]);
        // Carriage returns stay data, unlike `str::lines`.
        assert_eq!(shell_lines("a\r\n"), vec!["a\r", ""]);
    }

    #[test]
    fn identity_paths_strip_slashes_then_one_git_suffix() {
        assert_eq!(clean_identity_path("a/b/"), "a/b");
        assert_eq!(clean_identity_path("a.git"), "a");
        assert_eq!(clean_identity_path("a.git.git"), "a.git");
        assert_eq!(clean_identity_path("a.git/"), "a");
        assert_eq!(clean_identity_path("///"), "");
        assert_eq!(clean_identity_path(".git"), "");
    }
}
