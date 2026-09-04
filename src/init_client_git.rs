//! Git staging and publication for `lib/dot/init-client.sh`.
//!
//! The shell file holds 79 functions — too big for one lane — so this
//! module owns only the two contiguous functions from
//! `_dot_init_stage_git` (line 798) through `_dot_init_publish_git`
//! (line 870) in file order: the operator's checkout is cloned into
//! the backup-root stage ([`stage_git`]) and then moved into the live
//! git directory ([`publish_git`]).
//!
//! Lane map, so the integrator can stack without overlap: the
//! transaction-directory lifecycle (including
//! `_dot_init_private_directory`) lives on another lane, the
//! transaction record journal (including the `_dot_init_record_phase`
//! neighbor at line 790) on the record lane, the git-generation
//! binding (`_dot_init_generation_matches`,
//! `_dot_init_write_generation_marker`) on the generation lane, the
//! host-git identity family (`_dot_init_set_git_identity`) on the
//! identity lane, the metadata-modes walk
//! (`_dot_init_configure_git_metadata_modes` at line 780) on its own
//! lane, and the exclusive move (`_dot_move_noreplace`) in
//! [`crate::temp`]. None of those are merged here, so every one
//! crosses as a closure on [`GitStageDeps`]. The candidate-match
//! neighbor at line 872 stays for its own later slice.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals and the worktree root from `HOME`. Library
//! code must not read process environment behind the engine, so the
//! record path, backup root, live git directory, origin, branch,
//! commit, identity, nonce, and home cross as explicit parameters on
//! [`GitStageInputs`]. Shell-global side effects the engine owns
//! (`DOT_INIT_PHASE`, `DOT_INIT_GIT_DEV`, `DOT_INIT_GIT_INO`) stay
//! behind the injected closures: `record_phase` and `set_git_identity`
//! take exactly the shell's argument lists, and the engine threads
//! the ambient values through the closure environments the way the
//! shell threads them through globals.
//!
//! Byte-fidelity boundary: every `$BACKUP/git-stage` join
//! concatenates bytes like the shell, preserving a doubled separator
//! on trailing-slash inputs instead of normalizing it away (the plan
//! lane precedent). Journal text crosses the UTF-8 boundary as
//! `&str`, the candidate lane precedent, so non-UTF8 run values can
//! diverge from the shell exactly the way they do on sibling lanes.
//! `LC_ALL=C` is pinned around every child process so git output
//! reads English on both engines, and `HOME` is steered at `home`
//! for the same reason (the plan lane precedent): the engine's home
//! is the value the shell's own git children would inherit. The
//! marker gate replicates GNU `grep -Fqx` byte for byte, including
//! its two surprises (both probed): a `-F` pattern splits on
//! newlines into sub-patterns, and NUL separates framed lines. The
//! split semantics are GNU-specific, so the adversarial differential
//! row only runs where the oracle grep reports GNU.
//!
//! Verdict boundary: the shell reports every failure as exit 1, so
//! only the `Ok`/`Err` verdict compares across engines, never the
//! error payload. Filesystem gates refuse with [`Error::Usage`],
//! filesystem I/O failures with [`Error::Io`], and child-process
//! failures with [`Error::Command`], the plan lane precedent.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};

/// Private-directory provision: the transaction lane's
/// `_dot_init_private_directory` (`mkdir -p` plus the real-directory
/// gate plus `chmod 0700`), injected because that lane is unmerged.
pub type EnsurePrivateDir<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Generation check: the generation lane's
/// `_dot_init_generation_matches` (generation marker plus branch-tip
/// comparison), injected because that lane is unmerged. Answers
/// false for every refusal, like the shell's `return 1`.
pub type GenerationMatches<'a> = dyn Fn(&Path) -> bool + 'a;

/// Metadata-modes walk: `_dot_init_configure_git_metadata_modes`
/// (`core.sharedRepository` plus the owner-only metadata walk),
/// injected because its lane is unmerged.
pub type ConfigureMetadataModes<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Git identity capture: the identity lane's
/// `_dot_init_set_git_identity` (resolves the git directory's device
/// and inode into the run), injected because that lane is unmerged.
pub type SetGitIdentity<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Generation-marker writer: the generation lane's
/// `_dot_init_write_generation_marker`, injected because that lane
/// is unmerged.
pub type WriteGenerationMarker<'a> = dyn Fn(&Path) -> Result<()> + 'a;

/// Exclusive move: `_dot_move_noreplace` (the [`crate::temp`] move
/// once the integrator wires its cache through the closure
/// environment), injected because the move owns engine temp state.
pub type MoveNoreplace<'a> = dyn Fn(&Path, &Path) -> Result<()> + 'a;

/// Phase journal: the record lane's `_dot_init_record_phase`
/// (rewrites the transaction record at a new phase), injected
/// because that neighbor is already ported on its own lane. Takes
/// exactly the shell's two arguments; every other record field
/// stays ambient to the closure, the way the shell re-reads its
/// globals per call.
pub type RecordPhase<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// Run-supplied git-stage body: everything `_dot_init_stage_git`
/// and `_dot_init_publish_git` take beyond the injected lanes. One
/// bundle because the nine values always travel together — nine
/// flat parameters would trip `clippy::too_many_arguments`, the
/// plan-lane `PlanInputs` precedent.
pub struct GitStageInputs<'a> {
    /// Transaction record (`$1`).
    pub record: &'a Path,
    /// Backup root (`DOT_INIT_BACKUP`): anchors the stage.
    pub backup: &'a Path,
    /// Live git directory (`DOT_INIT_GIT_DIR`).
    pub git_dir: &'a Path,
    /// Expected origin URL (`DOT_INIT_ORIGIN`): the clone source.
    pub origin: &'a Path,
    /// Branch being installed (`DOT_INIT_BRANCH`).
    pub branch: &'a str,
    /// Locked commit (`DOT_INIT_COMMIT`).
    pub commit: &'a str,
    /// Canonical repository identity (`DOT_INIT_IDENTITY`).
    pub identity: &'a str,
    /// Run nonce (`DOT_INIT_NONCE`).
    pub nonce: &'a str,
    /// Client root (`HOME`): the `core.worktree` value and the
    /// `HOME` steered into every git child.
    pub home: &'a Path,
}

/// Cross-lane collaborators for [`stage_git`] and [`publish_git`].
/// One bundle for the same arity reason as [`GitStageInputs`]; the
/// engine (and the parity tests) fill each slot with the owning
/// lane's function or, until those lanes merge, a closure running
/// the live shell helper.
pub struct GitStageDeps<'a> {
    /// `_dot_init_private_directory`.
    pub ensure_private_dir: &'a EnsurePrivateDir<'a>,
    /// `_dot_init_generation_matches`.
    pub generation_matches: &'a GenerationMatches<'a>,
    /// `_dot_init_configure_git_metadata_modes`.
    pub configure_metadata_modes: &'a ConfigureMetadataModes<'a>,
    /// `_dot_init_set_git_identity`.
    pub set_git_identity: &'a SetGitIdentity<'a>,
    /// `_dot_init_write_generation_marker`.
    pub write_generation_marker: &'a WriteGenerationMarker<'a>,
    /// `_dot_move_noreplace`.
    pub move_noreplace: &'a MoveNoreplace<'a>,
    /// `_dot_init_record_phase`.
    pub record_phase: &'a RecordPhase<'a>,
}

/// Header of the stage identity marker: the shell's first `printf`
/// in `_dot_init_stage_git`.
const STAGE_HEADER: &[u8] = b"cgraf78 dot Git stage v1\n";

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A real directory, never a symlink: the shell's
/// `[[ -d $path && ! -L $path ]]`.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.file_type().is_symlink())
        && std::fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A real regular file, never a symlink: the shell's
/// `[[ -f $path && ! -L $path ]]`. `symlink_metadata` already
/// refuses to follow, so `is_file` alone is the whole gate.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Raw bytes of a path, so `$BACKUP/git-stage` joins behave like
/// shell string operations even when `backup` has a trailing slash
/// (the doubled separator is preserved, never normalized away).
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Append one `/`-separated leaf, like the shell's
/// `"$base/$leaf"`. Byte concatenation, so a `base` with a trailing
/// slash keeps its doubled separator exactly like the shell's
/// expansion does.
fn join2(base: &Path, leaf: &str) -> PathBuf {
    let mut bytes = path_bytes(base).to_vec();
    bytes.push(b'/');
    bytes.extend_from_slice(leaf.as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
}

/// Stage container (`$DOT_INIT_BACKUP/git-stage`): the shell's
/// `container=$DOT_INIT_BACKUP/git-stage`.
fn stage_container(backup: &Path) -> PathBuf {
    join2(backup, "git-stage")
}

/// Stage identity marker (`$container/identity`).
fn stage_marker(container: &Path) -> PathBuf {
    join2(container, "identity")
}

/// Staged repository (`$container/repo`, also
/// `$DOT_INIT_BACKUP/git-stage/repo` in `_dot_init_publish_git`).
fn staged_repo(container: &Path) -> PathBuf {
    join2(container, "repo")
}

/// Frame file bytes as the shell's `grep -x` sees them: bytes divide
/// on `\n` and on NUL (probed: GNU `grep -Fqx` matches `junk`
/// inside `nonce=x\0junk`, so NUL separates lines for matching),
/// a missing trailing separator still yields its final line, and a
/// trailing separator adds no phantom empty line.
fn grep_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = content
        .split(|byte| *byte == b'\n' || *byte == b'\0')
        .collect();
    if content.ends_with(b"\n") || content.ends_with(b"\0") {
        lines.pop();
    }
    lines
}

/// True when `needle` matches one full line of `content`: the
/// shell's `grep -Fqx`, fixed strings with whole-line anchoring.
/// GNU `grep` splits a `-F` pattern on newlines into one
/// sub-pattern per piece (probed: pattern `Z\n` matches a file
/// holding only an empty line, so the trailing newline contributes
/// an empty sub-pattern), and the match succeeds when any piece
/// equals any framed line — including NUL-framed ones, per
/// [`grep_lines`].
fn has_full_line(content: &[u8], needle: &[u8]) -> bool {
    let lines = grep_lines(content);
    needle
        .split(|byte| *byte == b'\n')
        .any(|piece| lines.iter().any(|line| *line == piece))
}

/// Strip every trailing newline, like command substitution: the
/// shell's `current=$(git ... rev-parse ...)`.
fn command_output(body: Vec<u8>) -> Vec<u8> {
    let mut end = body.len();
    while end > 0 && body[end - 1] == b'\n' {
        end -= 1;
    }
    body[..end].to_vec()
}

/// Run one `git` child the way the shell's plain `git` invocations
/// do: `LC_ALL=C` pinned, `HOME` steered at the engine home, no
/// controlling terminal, output captured and discarded (every call
/// site here is quiet on success; failures surface as the status).
fn git(home: &Path, args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    let mut command = Command::new("git");
    command
        .args(args)
        .env("LC_ALL", "C")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command.output().map_err(|source| Error::Io {
        context: "spawn git",
        source,
    })
}

/// Run one quiet `git` child and refuse unless it exits zero: the
/// shell's `git ... || return 1`.
fn git_ok(home: &Path, command: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = git(home, args)?;
    if !output.status.success() {
        return Err(Error::Command {
            command: command.to_string(),
            status: Some(format!("{}", output.status)),
        });
    }
    Ok(())
}

/// Borrow a `&Path` as `&OsStr` for child argument lists.
fn as_arg(path: &Path) -> &std::ffi::OsStr {
    path.as_os_str()
}

/// `_dot_init_stage_git`: clone the locked branch into the
/// backup-root stage (or adopt the live or staged checkout when its
/// generation already matches) and journal the run through
/// `git-staging` and `git-staged`.
///
/// Returns `Ok(())` with the stage marker, staged repository, and
/// transaction record in their committed states; every refusal
/// leaves the earlier states exactly where the shell leaves them
/// (a created marker stays, the first journal entry stays).
pub fn stage_git(inputs: &GitStageInputs<'_>, deps: &GitStageDeps<'_>) -> Result<()> {
    let container = stage_container(inputs.backup);
    let marker = stage_marker(&container);
    let repo = staged_repo(&container);

    (deps.ensure_private_dir)(inputs.backup)?;
    if !exists_lexical(&container) {
        (deps.ensure_private_dir)(&container)?;
        let mut body = STAGE_HEADER.to_vec();
        body.extend_from_slice(format!("nonce={}\n", inputs.nonce).as_bytes());
        body.extend_from_slice(format!("commit={}\n", inputs.commit).as_bytes());
        body.extend_from_slice(format!("identity={}\n", inputs.identity).as_bytes());
        std::fs::write(&marker, &body).map_err(|source| Error::Io {
            context: "write stage identity marker",
            source,
        })?;
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| Error::Io {
                context: "chmod stage identity marker",
                source,
            },
        )?;
    }
    if !(is_real_dir(&container) && is_real_file(&marker)) {
        return Err(Error::Usage {
            message: "git stage is not ours",
        });
    }
    let content = std::fs::read(&marker).map_err(|source| Error::Io {
        context: "read stage identity marker",
        source,
    })?;
    // Fixed-string whole-line matches, like the shell's three
    // `grep -Fqx` probes (see [`has_full_line`]). The right-hand
    // side of the shell's `[[ $current == "$DOT_INIT_COMMIT" ]]`
    // below is fully quoted, so that gate compares literally —
    // plain byte equality there, never pattern matching.
    for (key, value) in [
        ("nonce", inputs.nonce),
        ("commit", inputs.commit),
        ("identity", inputs.identity),
    ] {
        let mut needle = Vec::with_capacity(key.len() + 1 + value.len());
        needle.extend_from_slice(key.as_bytes());
        needle.push(b'=');
        needle.extend_from_slice(value.as_bytes());
        if !has_full_line(&content, &needle) {
            return Err(Error::Usage {
                message: "git stage identity changed",
            });
        }
    }

    (deps.record_phase)(inputs.record, "git-staging")?;
    if exists_lexical(inputs.git_dir) {
        if !(deps.generation_matches)(inputs.git_dir) {
            return Err(Error::Usage {
                message: "live git directory is a foreign generation",
            });
        }
        (deps.configure_metadata_modes)(inputs.git_dir)?;
        (deps.set_git_identity)(inputs.git_dir)?;
        (deps.record_phase)(inputs.record, "git-staged")?;
        return Ok(());
    }
    if exists_lexical(&repo) {
        if (deps.generation_matches)(&repo) {
            (deps.configure_metadata_modes)(&repo)?;
            (deps.set_git_identity)(&repo)?;
            (deps.record_phase)(inputs.record, "git-staged")?;
            return Ok(());
        }
        if !is_real_dir(&repo) {
            return Err(Error::Usage {
                message: "staged git path is not a directory",
            });
        }
        std::fs::remove_dir_all(&repo).map_err(|source| Error::Io {
            context: "remove stale staged git directory",
            source,
        })?;
    }
    let branch_ref = format!("refs/heads/{}", inputs.branch);
    let remote_ref = format!("refs/remotes/origin/{}", inputs.branch);
    let branch_remote = format!("branch.{}.remote", inputs.branch);
    let branch_merge = format!("branch.{}.merge", inputs.branch);
    let repo_arg = as_arg(&repo);
    let home_arg = as_arg(inputs.home);
    let origin_arg = as_arg(inputs.origin);
    let branch_arg = std::ffi::OsStr::new(inputs.branch);
    let commit_arg = std::ffi::OsStr::new(inputs.commit);
    let branch_ref_arg = std::ffi::OsStr::new(&branch_ref);
    let remote_ref_arg = std::ffi::OsStr::new(&remote_ref);
    git_ok(
        inputs.home,
        "git clone stage",
        &[
            std::ffi::OsStr::new("-c"),
            std::ffi::OsStr::new("core.sharedRepository=0700"),
            std::ffi::OsStr::new("clone"),
            std::ffi::OsStr::new("--quiet"),
            std::ffi::OsStr::new("--bare"),
            std::ffi::OsStr::new("--no-hardlinks"),
            std::ffi::OsStr::new("--branch"),
            branch_arg,
            std::ffi::OsStr::new("--single-branch"),
            std::ffi::OsStr::new("--"),
            origin_arg,
            repo_arg,
        ],
    )?;
    let output = git(
        inputs.home,
        &[
            std::ffi::OsStr::new("--git-dir"),
            repo_arg,
            std::ffi::OsStr::new("rev-parse"),
            branch_ref_arg,
        ],
    )?;
    if !output.status.success() {
        return Err(Error::Command {
            command: "git rev-parse stage tip".to_string(),
            status: Some(format!("{}", output.status)),
        });
    }
    if command_output(output.stdout) != inputs.commit.as_bytes() {
        return Err(Error::Usage {
            message: "staged tip is not the locked commit",
        });
    }
    // The shell's post-clone binding, in file order: bare layout
    // off, worktree home, untracked listing off, fsmonitor off, the
    // origin refspec, the tracking ref, and the branch upstream.
    let configs: [&[&std::ffi::OsStr]; 7] = [
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new("core.bare"),
            std::ffi::OsStr::new("false"),
        ],
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new("core.worktree"),
            home_arg,
        ],
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new("status.showUntrackedFiles"),
            std::ffi::OsStr::new("no"),
        ],
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new("core.fsmonitor"),
            std::ffi::OsStr::new("false"),
        ],
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new("remote.origin.fetch"),
            std::ffi::OsStr::new("+refs/heads/*:refs/remotes/origin/*"),
        ],
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new(&branch_remote),
            std::ffi::OsStr::new("origin"),
        ],
        &[
            std::ffi::OsStr::new("config"),
            std::ffi::OsStr::new(&branch_merge),
            branch_ref_arg,
        ],
    ];
    for config in configs {
        let mut args: Vec<&std::ffi::OsStr> = Vec::with_capacity(config.len() + 2);
        args.push(std::ffi::OsStr::new("--git-dir"));
        args.push(repo_arg);
        args.extend_from_slice(config);
        git_ok(inputs.home, "git config stage", &args)?;
    }
    git_ok(
        inputs.home,
        "git update-ref stage tip",
        &[
            std::ffi::OsStr::new("--git-dir"),
            repo_arg,
            std::ffi::OsStr::new("update-ref"),
            remote_ref_arg,
            commit_arg,
        ],
    )?;
    (deps.write_generation_marker)(&repo)?;
    (deps.configure_metadata_modes)(&repo)?;
    (deps.set_git_identity)(&repo)?;
    (deps.record_phase)(inputs.record, "git-staged")?;
    Ok(())
}

/// `_dot_init_publish_git`: move the staged repository into the live
/// git directory (skipped when the live directory already exists),
/// revalidate the generation there, capture the identity, and
/// journal the run as `publishing`.
pub fn publish_git(inputs: &GitStageInputs<'_>, deps: &GitStageDeps<'_>) -> Result<()> {
    let staged = staged_repo(&stage_container(inputs.backup));
    if !exists_lexical(inputs.git_dir) {
        if !(deps.generation_matches)(&staged) {
            return Err(Error::Usage {
                message: "staged git directory is a foreign generation",
            });
        }
        (deps.move_noreplace)(&staged, inputs.git_dir)?;
    }
    if !(deps.generation_matches)(inputs.git_dir) {
        return Err(Error::Usage {
            message: "live git directory is a foreign generation",
        });
    }
    (deps.set_git_identity)(inputs.git_dir)?;
    (deps.record_phase)(inputs.record, "publishing")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{command_output, grep_lines, has_full_line, join2};
    use std::path::Path;

    #[test]
    fn stage_join_concatenates_bytes() {
        // Trailing-slash bases keep the doubled separator, exactly
        // like the shell's `"$BACKUP/git-stage"` expansion.
        assert_eq!(
            join2(Path::new("/b"), "git-stage"),
            Path::new("/b/git-stage")
        );
        assert_eq!(
            join2(Path::new("/b/"), "git-stage").as_os_str(),
            std::ffi::OsStr::new("/b//git-stage")
        );
    }

    #[test]
    fn full_line_match_frames_like_grep_fqx() {
        assert!(has_full_line(b"a\nnonce=x\nb\n", b"nonce=x"));
        assert!(has_full_line(b"nonce=x", b"nonce=x"));
        assert!(!has_full_line(b"nonce=x\n", b"nonce="));
        assert!(!has_full_line(b"Xnonce=x\n", b"nonce=x"));
        assert!(!has_full_line(b"nonce=xx\n", b"nonce=x"));
        assert!(!has_full_line(b"", b"nonce=x"));
        assert!(!has_full_line(b"", b""));
        // GNU `grep -F` splits the pattern on newlines (all probed
        // against grep 3.12): any piece may carry the match, and a
        // trailing newline contributes an empty piece.
        assert!(has_full_line(b"nonce=a\nb\n", b"nonce=a\nb"));
        assert!(has_full_line(b"\n", b"Z\n"));
        assert!(!has_full_line(b"q\n", b"Z\n"));
        // NUL separates framed lines, so a NUL-framed piece
        // matches while the joined bytes never do.
        assert!(has_full_line(b"nonce=x\0junk\n", b"nonce=x"));
        assert!(has_full_line(b"\0nonce=x\n", b"nonce=x"));
        assert!(!has_full_line(b"ab\0", b""));
        assert!(has_full_line(b"ab\0\n", b""));
    }

    #[test]
    fn grep_lines_frames_like_grep() {
        assert!(grep_lines(b"").is_empty());
        assert_eq!(grep_lines(b"one\n"), vec![b"one".as_slice()]);
        assert_eq!(grep_lines(b"one"), vec![b"one".as_slice()]);
        assert_eq!(
            grep_lines(b"one\n\ntwo\n"),
            vec![b"one".as_slice(), b"".as_slice(), b"two".as_slice()]
        );
        assert_eq!(
            grep_lines(b"a\0\0b\n"),
            vec![b"a".as_slice(), b"".as_slice(), b"b".as_slice()]
        );
        assert_eq!(grep_lines(b"ab\0"), vec![b"ab".as_slice()]);
    }

    #[test]
    fn command_output_strips_trailing_newlines() {
        assert_eq!(command_output(b"abc\n".to_vec()), b"abc");
        assert_eq!(command_output(b"abc\n\n".to_vec()), b"abc");
        assert_eq!(command_output(b"abc".to_vec()), b"abc");
        assert_eq!(command_output(b"\n".to_vec()), b"");
        assert_eq!(command_output(Vec::new()), b"");
        // Interior newlines survive, like command substitution.
        assert_eq!(command_output(b"a\nb\n".to_vec()), b"a\nb");
    }
}
