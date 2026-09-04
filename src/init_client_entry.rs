//! Entry-stage validation and single-entry publication for
//! `lib/dot/init-client.sh`.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the four contiguous functions from
//! `_dot_init_entry_stage_valid` through `_dot_init_publish_one`
//! in file order: the stage-directory gate ([`entry_stage_valid`]),
//! the stage-content gate ([`entry_stage_only_next`]), the staged
//! `next` cleanup ([`discard_staged_next`]), and the pending /
//! staged publication driver ([`publish_one`]).
//!
//! The intent-record validator below this chapter
//! (`_dot_init_entry_intent`) and the prior-record reader above it
//! (`_dot_init_prior_record`) live on other lanes and are not
//! touched here. The staging primitives from the sibling
//! `rust-port-slice-46` lane (`write_private_line`, `entry_stage`,
//! the `EntryIntent` record, the stage-claim family) are likewise
//! owned elsewhere: the few shapes this chapter needs from that
//! family ([`EntryIntent`], [`STAGE_CLAIM_NAME`]) are mirrored
//! verbatim so the lanes merge by deduplication, and the rest
//! crosses as closures (see [`PublishOneInputs`]).
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_NONCE` global and the client root from `HOME`.
//! Library code must not read process environment behind the
//! engine, so the home root, transaction directory, intent path,
//! and git coordinates cross here as explicit parameters; the
//! nonce never crosses at all (it lives inside the injected
//! staging closures). Git runs plain like the shell's bare `git`
//! (see [`crate::repos_base::run_git`]): blob bytes are
//! locale-independent, and git's own diagnostics are not part of
//! the ported surface. The umask the tracked-mode setter honors
//! crosses as [`PublishOneInputs::mask`], the way the staged-clone
//! lane takes its ceiling mask.
//!
//! Byte-fidelity boundary: every `$HOME/$path` join concatenates
//! bytes like the shell (see `join_slash`), preserving a doubled
//! separator on trailing-slash inputs instead of normalizing it
//! away. The `next` home-relative reference is the intent's stage
//! plus `/next` by construction, exactly what the shell's
//! `${next#"$HOME"/}` strip yields. The `git show` redirect
//! pre-creates its target, so a failed show still leaves the
//! (partial) bytes behind like the shell. Command substitution
//! drops NUL bytes before the symlink-target gate, and `rm -f`
//! passes when its target vanished underneath it.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};
use crate::repos_base::run_git;
use crate::temp::{self, MoveCache};

/// File name of the claim marker inside a stage directory: the
/// shell's `.dot-init-stage-claim-v1`, mirrored verbatim from the
/// staging lane until the lanes merge.
pub const STAGE_CLAIM_NAME: &str = ".dot-init-stage-claim-v1";

/// File name of the staged publication candidate inside a stage
/// directory: the shell's `$stage/next`.
pub const NEXT_NAME: &str = "next";

/// A validated entry intent record: the staging lane's
/// `EntryIntent` mirrored verbatim (the shell's `REPLY` from
/// `_dot_init_entry_intent` split into its six tab fields) so the
/// lanes merge by deduplication. The device and inode fields stay
/// strings because the `pending` phase spells them `-`, exactly as
/// the shell carries them.
pub struct EntryIntent {
    /// `pending`, `staged`, or `prepared`: how far publication got.
    pub phase: String,
    /// Home-relative stage directory bound to this entry.
    pub stage: String,
    /// Stage device (`-` while pending).
    pub dev: String,
    /// Stage inode (`-` while pending).
    pub ino: String,
    /// Published-file device (`-` until prepared).
    pub next_dev: String,
    /// Published-file inode (`-` until prepared).
    pub next_ino: String,
}

/// `_dot_init_parent_directories`: ensure the published parents of
/// `path` exist under the client root, recording each step in
/// `transaction`. Injected because the transaction lane is
/// unmerged; tests feed a closure running the live shell.
pub type EnsureParents<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// `_dot_init_entry_intent`: validate the intent record at `file`
/// against the expected mode, object id, and path. Injected
/// because the neighbor lane is unmerged; tests feed a closure
/// running the live shell.
pub type ReadIntent<'a> = dyn Fn(&Path, &str, &str, &str) -> Result<EntryIntent> + 'a;

/// `_dot_init_stage_claim_matches` for kind `entry`: the claim
/// marker under `stage` proves this run owns it for `path`.
/// Injected because the claim lane is unmerged; tests feed a
/// closure running the live shell.
pub type ClaimMatches<'a> = dyn Fn(&Path, &str) -> bool + 'a;

/// `_dot_init_stage_claim_write` for kind `entry`: publish the
/// claim marker for (`stage`, `path`). Injected because the claim
/// lane is unmerged; tests feed a closure running the live shell.
pub type ClaimWrite<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// `_dot_init_stage_claim_remove` for kind `entry`: drop the claim
/// marker after it revalidates. Injected because the claim lane is
/// unmerged; tests feed a closure running the live shell.
pub type ClaimRemove<'a> = dyn Fn(&Path, &str) -> Result<()> + 'a;

/// `_dot_init_write_private_line`: publish one line at `file`,
/// replacing when `replace` is set. Injected because the record
/// lane is unmerged; tests feed a closure running the live shell.
pub type WriteIntentLine<'a> = dyn Fn(&Path, &str, bool) -> Result<()> + 'a;

/// `_dot_init_candidate_matches_git`: the home-relative `path`
/// holds `mode`/`oid` from `commit` in `git_dir`. Injected
/// because the candidate lane is unmerged; tests feed a closure
/// running the live shell.
pub type CandidateMatches<'a> = dyn Fn(&str, &str, &str, &str, &str) -> bool + 'a;

/// Inputs for [`publish_one`]: the publication coordinates plus
/// the unmerged-lane closures above.
pub struct PublishOneInputs<'a> {
    /// Client root: the shell's `HOME`.
    pub home: &'a Path,
    /// Transaction directory holding parent/intent records.
    pub transaction: &'a Path,
    /// Intent record path for this entry.
    pub intent: &'a Path,
    /// Object store for the blob reads: the shell's `git_dir`.
    pub git_dir: &'a str,
    /// Commit the blob is published from.
    pub commit: &'a str,
    /// Git mode of the entry (`100644`, `100755`, `120000`).
    pub mode: &'a str,
    /// Object id the published bytes must hash to.
    pub oid: &'a str,
    /// Home-relative path being published.
    pub path: &'a str,
    /// Live umask for the tracked-mode setter (the caller reads
    /// it, like the shell's inline symbolic `chmod` honors it).
    pub mask: u32,
    /// Parent-directory provision (the transaction lane).
    pub ensure_parents: &'a EnsureParents<'a>,
    /// Intent-record validation (the neighbor lane).
    pub read_intent: &'a ReadIntent<'a>,
    /// Stage-claim verification (the claim lane, kind `entry`).
    pub claim_matches: &'a ClaimMatches<'a>,
    /// Stage-claim publication (the claim lane, kind `entry`).
    pub claim_write: &'a ClaimWrite<'a>,
    /// Stage-claim release (the claim lane, kind `entry`).
    pub claim_remove: &'a ClaimRemove<'a>,
    /// Intent-line journal (the record lane).
    pub write_line: &'a WriteIntentLine<'a>,
    /// Published-content verification (the candidate lane).
    pub candidate_matches: &'a CandidateMatches<'a>,
}

/// Effective-uid ownership (`test -O`): the shell gate requires
/// the stage to be ours. An unreadable identity fails closed,
/// like the shell's failed `stat`. Mirrored verbatim from the
/// staging lane until the lanes merge.
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// Append one path component with a plain `/` separator, like the
/// shell's `"$dir/$base"`: a `home` with a trailing slash keeps
/// its doubled separator instead of being normalized away.
/// Mirrored verbatim from the staging lane until the lanes merge.
fn join_slash(dir: &Path, component: &str) -> PathBuf {
    let mut out = dir.as_os_str().to_os_string();
    out.push("/");
    out.push(component);
    PathBuf::from(out)
}

/// Any filesystem presence, dangling links included: the shell's
/// `[[ -e $path || -L $path ]]`. `symlink_metadata` never
/// follows, so a link reports itself.
fn any_presence(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// True for a link-target candidate the journal can carry: the
/// shell's `_dot_init_safe_value` (nonempty, no tab, newline, or
/// carriage return).
fn safe_value(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && !bytes
            .iter()
            .any(|byte| matches!(byte, b'\t' | b'\n' | b'\r'))
}

/// Run `git --git-dir=<git_dir> show <commit>:<path>`: the
/// captured stdout plus whether git reported success. `None` git
/// output (git never ran) reads as empty bytes, like the shell's
/// redirect creating an empty file before a failed exec.
fn git_show(git_dir: &str, commit: &str, path: &str) -> (Vec<u8>, bool) {
    let prefix = [OsString::from("--git-dir"), OsString::from(git_dir)];
    let spec = format!("{commit}:{path}");
    match run_git(&prefix, &["show", spec.as_str()]) {
        Some(output) => (output.stdout, output.status.success()),
        None => (Vec::new(), false),
    }
}

/// `_dot_init_entry_stage_valid`: `stage` is an owned real
/// directory with no group/other permission bits, and — when
/// `expected_identity` is nonempty — its `dev:ino` identity
/// matches. The shell's octal-digit guard is subsumed by computing
/// the bits directly, the way the overlay lane's
/// `private_directory` does.
pub fn entry_stage_valid(stage: &Path, expected_identity: Option<&str>) -> bool {
    let meta = match std::fs::symlink_metadata(stage) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    // `symlink_metadata` reports the link itself, so a passing
    // `is_dir` already excludes links; the explicit gate mirrors
    // the shell's `[[ -d $stage && ! -L $stage ]]` shape.
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    if !owned_by_us(stage) {
        return false;
    }
    if meta.mode() & 0o077 != 0 {
        return false;
    }
    match expected_identity {
        None => true,
        // The shell defaults the missing argument to empty and
        // skips the check on `[[ -z $expected_identity ]]`.
        Some("") => true,
        Some(expected) => temp::path_identity(stage)
            .is_ok_and(|identity| temp::identity_string(identity) == expected),
    }
}

/// `_dot_init_entry_stage_only_next`: `stage` holds nothing but
/// the staged candidate and the claim marker. A directory read
/// replaces the shell's `nullglob`/`dotglob` enumeration
/// (dotfiles included, `.`/`..` excluded like the shell's `*`),
/// and every entry must carry exactly [`NEXT_NAME`] or
/// [`STAGE_CLAIM_NAME`]. A missing (or unreadable) stage expands
/// to nothing under `nullglob`, so the loop passes vacuously.
pub fn entry_stage_only_next(stage: &Path) -> bool {
    let read = match std::fs::read_dir(stage) {
        Ok(read) => read,
        Err(_) => return true,
    };
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return false,
        };
        let name = entry.file_name();
        let name = name.as_os_str().as_bytes();
        if name != NEXT_NAME.as_bytes() && name != STAGE_CLAIM_NAME.as_bytes() {
            return false;
        }
    }
    true
}

/// `_dot_init_discard_staged_next`: drop the staged candidate
/// after the stage gates for content. A missing candidate is
/// already discarded; anything that is neither a real file nor a
/// symlink (a directory, fifo, socket) refuses, like the shell's
/// type gate before `rm -f`.
pub fn discard_staged_next(stage: &Path) -> Result<()> {
    if !entry_stage_only_next(stage) {
        return Err(Error::Usage {
            message: "entry stage holds more than its next",
        });
    }
    let next = join_slash(stage, NEXT_NAME);
    let meta = match std::fs::symlink_metadata(&next) {
        Ok(meta) => meta,
        // `[[ -e $next || -L $next ]]` missed: nothing to discard.
        Err(_) => return Ok(()),
    };
    // `[[ (-f $next && ! -L $next) || -L $next ]]`:
    // `symlink_metadata` never follows, so `is_file` already
    // excludes links.
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return Err(Error::Usage {
            message: "entry next is not a file",
        });
    }
    match std::fs::remove_file(&next) {
        Ok(()) => Ok(()),
        // `rm -f` passes when the name vanished underneath it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            context: "remove entry next",
            source,
        }),
    }
}

/// `_dot_init_publish_one`: drive one entry from its intent record
/// to its home path through the stage directory. A `pending`
/// intent claims (or revalidates) the stage and binds it to its
/// device/inode; a `staged` intent turns the committed blob into
/// the `next` candidate and binds that too; then the candidate is
/// verified, moved into place without replacing a live path, and
/// the stage is released. Any other phase skips both transitions
/// and runs the final verification with the intent's bindings,
/// which fails once the stage is gone — like the shell falling
/// through to the same checks.
pub fn publish_one(inputs: &PublishOneInputs<'_>, moves: &mut MoveCache) -> Result<()> {
    let target = join_slash(inputs.home, inputs.path);
    (inputs.ensure_parents)(inputs.transaction, inputs.path)?;
    let intent = (inputs.read_intent)(inputs.intent, inputs.mode, inputs.oid, inputs.path)?;
    let stage = join_slash(inputs.home, &intent.stage);
    let next = join_slash(&stage, NEXT_NAME);
    let next_rel = format!("{}/{NEXT_NAME}", intent.stage);
    let mut phase = intent.phase.clone();
    let mut stage_dev = intent.dev.clone();
    let mut stage_ino = intent.ino.clone();
    let mut next_dev = intent.next_dev.clone();
    let mut next_ino = intent.next_ino.clone();
    if phase == "pending" {
        if any_presence(&stage) {
            if !entry_stage_valid(&stage, None) {
                return Err(Error::Usage {
                    message: "entry stage is not valid",
                });
            }
            if !(inputs.claim_matches)(&stage, inputs.path) {
                return Err(Error::Usage {
                    message: "entry stage claim does not match",
                });
            }
            if !entry_stage_only_next(&stage) {
                return Err(Error::Usage {
                    message: "entry stage holds more than its next",
                });
            }
            if any_presence(&next) {
                return Err(Error::Usage {
                    message: "entry next already exists",
                });
            }
        } else {
            std::fs::create_dir(&stage).map_err(|source| Error::Io {
                context: "create entry stage",
                source,
            })?;
            std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).map_err(
                |source| Error::Io {
                    context: "chmod entry stage",
                    source,
                },
            )?;
            (inputs.claim_write)(&stage, inputs.path)?;
        }
        let (dev, ino) = temp::path_identity(&stage).map_err(|_| Error::Usage {
            message: "entry stage has no identity",
        })?;
        stage_dev = dev.to_string();
        stage_ino = ino.to_string();
        // The staged line binds the container before the blob
        // redirect below can leave partial bytes: a crash during
        // `git show` then rolls back without mistaking the
        // incomplete file for a finished candidate.
        let line = format!(
            "staged\t{}\t{}\t{}\t{}\t{stage_dev}\t{stage_ino}\t-\t-",
            inputs.mode, inputs.oid, inputs.path, intent.stage,
        );
        (inputs.write_line)(inputs.intent, line.as_str(), true)?;
        phase = "staged".to_string();
    }
    if phase == "staged" {
        let wanted = format!("{stage_dev}:{stage_ino}");
        if !entry_stage_valid(&stage, Some(wanted.as_str())) {
            return Err(Error::Usage {
                message: "entry stage is not valid",
            });
        }
        if !(inputs.claim_matches)(&stage, inputs.path) {
            return Err(Error::Usage {
                message: "entry stage claim does not match",
            });
        }
        discard_staged_next(&stage)?;
        match inputs.mode {
            "100644" | "100755" => {
                // The shell's redirect creates the file before git
                // runs, so a failed show still leaves the (partial)
                // bytes behind for rollback to sweep.
                let (body, ok) = git_show(inputs.git_dir, inputs.commit, inputs.path);
                std::fs::write(&next, &body).map_err(|source| Error::Io {
                    context: "write entry next",
                    source,
                })?;
                if !ok {
                    return Err(Error::Usage {
                        message: "entry blob read failed",
                    });
                }
                temp::apply_tracked_file_mode(&next, inputs.mode, inputs.mask)?;
            }
            "120000" => {
                let (mut link_target, ok) = git_show(inputs.git_dir, inputs.commit, inputs.path);
                if !ok {
                    return Err(Error::Usage {
                        message: "entry blob read failed",
                    });
                }
                // Command substitution drops NUL bytes before the
                // value gate ever sees them.
                link_target.retain(|byte| *byte != 0);
                if !safe_value(&link_target) {
                    return Err(Error::Usage {
                        message: "entry link target is not a safe value",
                    });
                }
                std::os::unix::fs::symlink(OsString::from_vec(link_target), &next).map_err(
                    |source| Error::Io {
                        context: "link entry next",
                        source,
                    },
                )?;
            }
            _ => {
                return Err(Error::Usage {
                    message: "entry has an unsupported mode",
                });
            }
        }
        if !(inputs.candidate_matches)(
            inputs.git_dir,
            inputs.commit,
            inputs.mode,
            inputs.oid,
            next_rel.as_str(),
        ) {
            return Err(Error::Usage {
                message: "entry candidate does not match git",
            });
        }
        let (dev, ino) = temp::path_identity(&next).map_err(|_| Error::Usage {
            message: "entry next has no identity",
        })?;
        next_dev = dev.to_string();
        next_ino = ino.to_string();
        let line = format!(
            "prepared\t{}\t{}\t{}\t{}\t{stage_dev}\t{stage_ino}\t{next_dev}\t{next_ino}",
            inputs.mode, inputs.oid, inputs.path, intent.stage,
        );
        (inputs.write_line)(inputs.intent, line.as_str(), true)?;
    }
    let wanted_stage = format!("{stage_dev}:{stage_ino}");
    if !entry_stage_valid(&stage, Some(wanted_stage.as_str())) {
        return Err(Error::Usage {
            message: "entry stage is not valid",
        });
    }
    if !(inputs.claim_matches)(&stage, inputs.path) {
        return Err(Error::Usage {
            message: "entry stage claim does not match",
        });
    }
    if !entry_stage_only_next(&stage) {
        return Err(Error::Usage {
            message: "entry stage holds more than its next",
        });
    }
    let wanted_next = format!("{next_dev}:{next_ino}");
    // The shell reads the identity through `|| true`: a missing
    // candidate compares as empty and fails the gate.
    let landed = temp::path_identity(&next).map_err(|_| Error::Usage {
        message: "entry next has no identity",
    })?;
    if temp::identity_string(landed) != wanted_next {
        return Err(Error::Usage {
            message: "entry next changed under us",
        });
    }
    if !(inputs.candidate_matches)(
        inputs.git_dir,
        inputs.commit,
        inputs.mode,
        inputs.oid,
        next_rel.as_str(),
    ) {
        return Err(Error::Usage {
            message: "entry candidate does not match git",
        });
    }
    temp::move_noreplace_cached(&next, &target, moves)?;
    let placed = temp::path_identity(&target).map_err(|_| Error::Usage {
        message: "published entry has no identity",
    })?;
    if temp::identity_string(placed) != wanted_next {
        return Err(Error::Usage {
            message: "published entry changed under us",
        });
    }
    (inputs.claim_remove)(&stage, inputs.path)?;
    std::fs::remove_dir(&stage).map_err(|source| Error::Io {
        context: "remove entry stage",
        source,
    })?;
    if !(inputs.candidate_matches)(
        inputs.git_dir,
        inputs.commit,
        inputs.mode,
        inputs.oid,
        inputs.path,
    ) {
        return Err(Error::Usage {
            message: "published entry does not match git",
        });
    }
    Ok(())
}
