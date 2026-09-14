//! The init transaction-directory lifecycle, part 1 of
//! `lib/dot/init-client.sh`: state-root resolution, the transaction
//! and completed paths, private directory setup, stage preparation
//! with an ownership marker, the ownership gate, orphan recovery,
//! and publication.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the directory lifecycle: the eight
//! functions from `_dot_init_state_root` through
//! `_dot_init_publish_transaction`, minus the file-generic
//! `_dot_init_error` diagnostic (a bare `printf ... >&2; return 1`
//! with no family state; the port absorbs it into [`Result`], the
//! way earlier slices absorb engine diagnostics). Record, claim,
//! generation, and rollback families stay for later slices.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::errors::{Error, Result};
use crate::temp::{self, MoveCache};
use crate::xdg;

/// Exact bytes of the preparation ownership marker
/// (`.dot-transaction-stage-v1`): a matching pathname is not
/// ownership evidence, so prepare publishes this exact private
/// content before the directory can become an orphan, and the gate
/// below removes only stages carrying these bytes.
pub const PREPARATION_MARKER: &[u8] = b"cgraf78 dot initialization preparation v1\n";

/// File name of the ownership marker inside a preparation stage.
pub const PREPARATION_MARKER_NAME: &str = ".dot-transaction-stage-v1";

/// `_dot_init_state_root`: `<xdg-state>/dot/init`. `xdg_state_home`
/// is the raw `$XDG_STATE_HOME` value (empty counts as unset, like
/// the shell); `home` must be absolute for the fallback, exactly as
/// in [`xdg::base`].
pub fn state_root(home: &str, xdg_state_home: &str) -> std::result::Result<String, xdg::Error> {
    xdg::path(xdg::Kind::State, "dot/init", xdg_state_home, home)
}

/// `_dot_init_transaction_dir`: `<state-root>/transaction`.
pub fn transaction_dir(
    home: &str,
    xdg_state_home: &str,
) -> std::result::Result<String, xdg::Error> {
    Ok(format!("{}/transaction", state_root(home, xdg_state_home)?))
}

/// `_dot_init_completed_file`: `<state-root>/completed`.
pub fn completed_file(home: &str, xdg_state_home: &str) -> std::result::Result<String, xdg::Error> {
    Ok(format!("{}/completed", state_root(home, xdg_state_home)?))
}

/// `_dot_init_private_directory`: `mkdir -p`, then require a real
/// directory (never a symlink) at mode 700. Unlike
/// [`temp::private_dir_validate`], this CREATES the directory and
/// does not check ownership: the shell repairs the mode on whatever
/// `mkdir -p` produced, so a pre-existing path gets clamped, not
/// rejected.
pub fn private_directory(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    // `is_dir` follows symlinks like `test -d`, so the explicit
    // link check mirrors the shell's `[[ -d $path && ! -L $path ]]`.
    if !path.is_dir() || path.is_symlink() {
        return false;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).is_ok()
}

/// Effective-uid ownership (`test -O`): the shell gate requires the
/// stage and its marker to be ours. An unreadable identity fails
/// closed, like the shell's failed `stat`.
fn owned_by_us(path: &Path) -> bool {
    match (temp::current_uid(), temp::path_uid(path)) {
        (Some(uid), Ok(owner)) => uid == owner,
        _ => false,
    }
}

/// `_dot_init_prepare_transaction`: ensure the transaction parent is
/// private, then allocate a `{transaction}.prepare.XXXXXX` sibling
/// at mode 700 holding the ownership marker at mode 600, returning
/// the stage path (`$REPLY` in the shell).
///
/// `transaction` must be absolute with a parent — every caller
/// derives it from the XDG state root. Like the shell, a failure
/// after the directory exists (marker write, marker chmod) leaves
/// the stage behind: recovery skips markerless stages on both
/// engines, so the shapes stay comparable.
pub fn prepare_transaction(transaction: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let parent = transaction
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(Error::Usage {
            message: "transaction has no parent directory",
        })?;
    if !private_directory(parent) {
        return Err(Error::Usage {
            message: "cannot prepare transaction parent",
        });
    }
    for _ in 0..temp::TMP_RETRIES {
        // The shell template is `"${transaction}.prepare.XXXXXX"`;
        // the six random characters come from the shared mktemp
        // alphabet so both engines draw the same name shape.
        let mut name = transaction.as_os_str().to_os_string();
        name.push(".prepare.");
        name.push(temp::random_suffix());
        let stage = PathBuf::from(name);
        match std::fs::create_dir(&stage) {
            Ok(()) => {
                // `mktemp -d` creates 0700 regardless of umask while
                // `create_dir` honors it, hence the shell's explicit
                // `chmod 0700` — repeated here for the same reason.
                if std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).is_err()
                {
                    let _ = std::fs::remove_dir_all(&stage);
                    return Err(Error::Usage {
                        message: "cannot mode transaction stage",
                    });
                }
                let marker = stage.join(PREPARATION_MARKER_NAME);
                if std::fs::write(&marker, PREPARATION_MARKER).is_err()
                    || std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600))
                        .is_err()
                {
                    return Err(Error::Usage {
                        message: "cannot write transaction stage marker",
                    });
                }
                return Ok(stage);
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                // Taken: the next iteration tries a fresh suffix.
            }
            Err(source) => {
                return Err(Error::Io {
                    context: "create transaction stage",
                    source,
                });
            }
        }
    }
    Err(Error::Usage {
        message: "transaction stage names keep colliding",
    })
}

/// `_dot_init_transaction_stage_owned`: the stage is a real
/// directory (never a symlink) owned by us with no group/other
/// permission bits, and its marker is a 600 single-link file we own
/// holding exactly [`PREPARATION_MARKER`]. `source_root` feeds the
/// content hash (`_dot_source_git hash-object`), like every other
/// [`temp::stdin_matches_file`] caller.
pub fn transaction_stage_owned(source_root: &Path, stage: &Path) -> bool {
    if !stage.is_dir() || stage.is_symlink() {
        return false;
    }
    let mode = match temp::file_mode(stage) {
        Ok(mode) => mode,
        Err(_) => return false,
    };
    // `stat -c %a` output is always octal digits, so the shell's
    // `*[^0-7]*` guard is vacuous and only the group/other mask
    // matters — note this admits e.g. setuid bits exactly like
    // `((8#$mode & 077)) == 0` does.
    if mode & 0o77 != 0 {
        return false;
    }
    if !owned_by_us(stage) {
        return false;
    }
    let marker = stage.join(PREPARATION_MARKER_NAME);
    if temp::private_control_file_validate(&marker).is_err() {
        return false;
    }
    temp::stdin_matches_file(source_root, PREPARATION_MARKER, &marker).unwrap_or(false)
}

/// `_dot_init_recover_transaction_stages`: remove every
/// `{transaction}.prepare.*` sibling the ownership gate accepts;
/// forged or foreign stages stay put. Only a failed removal fails,
/// like the shell's `rm -rf ... || return 1`; an unreadable parent
/// matches the shell's nullglob-empty expansion and succeeds.
pub fn recover_transaction_stages(source_root: &Path, transaction: &Path) -> bool {
    let parent = match transaction
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent,
        None => return true,
    };
    let file_name = match transaction.file_name() {
        Some(name) => name,
        None => return true,
    };
    let mut prefix = file_name.to_os_string();
    prefix.push(".prepare.");
    let prefix = prefix.as_bytes();
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_name().as_bytes().starts_with(prefix) {
            continue;
        }
        let stage = entry.path();
        if !transaction_stage_owned(source_root, &stage) {
            continue;
        }
        if std::fs::remove_dir_all(&stage).is_err() {
            return false;
        }
    }
    true
}

/// `_dot_init_publish_transaction`: the stage must pass the
/// ownership gate and carry a `record` regular file (never a
/// symlink); then move it onto the transaction path without
/// replacing a late arrival, like `_dot_move_noreplace`. `cache`
/// carries the probed `mv` spelling, exactly as the file
/// transaction publisher does.
pub fn publish_transaction(
    source_root: &Path,
    stage: &Path,
    transaction: &Path,
    cache: &mut MoveCache,
) -> bool {
    if !transaction_stage_owned(source_root, stage) {
        return false;
    }
    // `[[ -f $stage/record && ! -L $stage/record ]]`: `-f` follows
    // links, so the no-follow metadata check must exclude symlinks
    // explicitly — a link to a regular file fails here on purpose.
    match std::fs::symlink_metadata(stage.join("record")) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        _ => return false,
    }
    temp::move_noreplace_cached(stage, transaction, cache).is_ok()
}
