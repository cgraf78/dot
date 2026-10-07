//! Directories the overlay link phase created under `$HOME`, so stale
//! cleanup can take them away again.
//!
//! Linking `deep/er/x.conf` into a home without `deep/` creates both
//! parents. When the overlay later stops shipping the file, removing the
//! link alone would strand empty `deep/er/` and `deep/` directories that
//! no one asked for. Removing every directory that empties would be wrong
//! the other way: the user may own it (`~/.ssh`, a directory made before
//! the overlay linked into it). So the link phase records each directory
//! it creates, with its `dev:ino`, and stale cleanup removes a parent only
//! when it is empty, still recorded, and still that same directory.
//!
//! The record lives beside the manifest at `<manifest>.dirs`, one
//! `<dev:ino>\t<home-relative path>` line per directory. It is private
//! (`0600`, owned by the caller) and replaced atomically. The manifest
//! format stays untouched, so older Dot releases keep reading it; they
//! simply leave the record alone.
//!
//! Every failure fails toward keeping directories: an unreadable,
//! unsafe, or malformed record reads empty (and is replaced on the next
//! save), a directory created but not saved (an interrupted run) is
//! never removed, and a directory removed and recreated since it was
//! recorded fails the identity check (see [`crate::persisted_identity`],
//! which also keeps the identity valid across a reboot that renumbers the
//! device) unless the filesystem handed it the same inode number again;
//! even then only an empty directory goes.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::persisted_identity::{self, LiveIdentity};
use crate::repos_overlays;
use crate::temp::{self, MoveTool};

/// One directory the link phase created: its home-relative path and the
/// `dev:ino` it had right after creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedDir {
    /// Home-relative path.
    pub rel: String,
    /// `dev:ino` of the created directory.
    pub identity: String,
}

impl CreatedDir {
    /// The directory just created at `home/rel`, or `None` when it cannot
    /// be stat'ed as a real directory (it is then never recorded, so never
    /// removed).
    pub fn probe(home: &str, rel: &str) -> Option<Self> {
        let meta = std::fs::symlink_metadata(format!("{home}/{rel}")).ok()?;
        meta.file_type().is_dir().then(|| Self {
            rel: rel.to_string(),
            identity: temp::identity_string((meta.dev(), meta.ino())),
        })
    }
}

/// The record path for `manifest`.
pub fn record_path(manifest: &str) -> PathBuf {
    PathBuf::from(format!("{manifest}.dirs"))
}

/// The directories recorded for one link phase, loaded at its start and
/// saved at its end.
#[derive(Debug, Default)]
pub struct CreatedDirs {
    /// Home-relative path to recorded `dev:ino`.
    dirs: BTreeMap<String, String>,
    /// When the loaded record was written: the bound
    /// [`persisted_identity::matches`] holds a renumbered device to.
    journaled: Option<SystemTime>,
    /// Whether the record needs rewriting.
    changed: bool,
}

/// Whether `identity` reads as a recorded `dev:ino`.
fn identity_shape(identity: &str) -> bool {
    identity.split_once(':').is_some_and(|(dev, ino)| {
        !dev.is_empty()
            && !ino.is_empty()
            && dev.bytes().all(|byte| byte.is_ascii_digit())
            && ino.bytes().all(|byte| byte.is_ascii_digit())
    })
}

impl CreatedDirs {
    /// Load the record beside `manifest`. Missing reads empty; a record
    /// that is not a private regular file owned by `euid`, or holds a line
    /// that does not parse, reads empty and is replaced on the next save.
    pub fn load(manifest: &str, euid: u32) -> Self {
        let path = record_path(manifest);
        if std::fs::symlink_metadata(&path).is_err() {
            return Self::default();
        }
        let untrusted = Self {
            changed: true,
            ..Self::default()
        };
        if !repos_overlays::private_regular_file(&path, euid) {
            return untrusted;
        }
        let Ok(content) = std::fs::read(&path) else {
            return untrusted;
        };
        let mut dirs = BTreeMap::new();
        for line in repos_overlays::stream_lines(&content) {
            let Some((identity, rel)) = line.split_once('\t') else {
                return untrusted;
            };
            if !identity_shape(identity) || !repos_overlays::init_safe_relative_path(rel) {
                return untrusted;
            }
            dirs.insert(rel.to_string(), identity.to_string());
        }
        Self {
            dirs,
            journaled: persisted_identity::journal_time(&path),
            changed: false,
        }
    }

    /// Record a directory the link phase just created.
    pub fn record(&mut self, dir: CreatedDir) {
        self.dirs.insert(dir.rel, dir.identity);
        self.changed = true;
    }

    /// The live identity of `home/rel` when it is a real directory that is
    /// still the recorded one.
    fn still_recorded(&self, home: &str, rel: &str) -> Option<LiveIdentity> {
        let identity = self.dirs.get(rel)?;
        let path = PathBuf::from(format!("{home}/{rel}"));
        let meta = std::fs::symlink_metadata(&path).ok()?;
        let live = LiveIdentity::from_metadata(&path, &meta, false);
        (meta.file_type().is_dir() && persisted_identity::matches(identity, &live, self.journaled))
            .then_some(live)
    }

    /// After the link at `home/rel` was removed, remove its parents the
    /// link phase created, deepest first, while each is empty, recorded,
    /// and still the recorded directory. Stops at the first parent that
    /// fails any of those (`rmdir` itself refuses a non-empty one, so a
    /// file that appears in between is never lost). Returns the removed
    /// home-relative paths.
    pub fn prune_parents(&mut self, home: &str, rel: &str) -> Vec<String> {
        let mut removed = Vec::new();
        let mut dir = Path::new(rel).parent();
        while let Some(parent) = dir {
            let Some(parent_rel) = parent.to_str().filter(|text| !text.is_empty()) else {
                break;
            };
            if self.still_recorded(home, parent_rel).is_none()
                || crate::cancellation::check().is_err()
                || std::fs::remove_dir(format!("{home}/{parent_rel}")).is_err()
            {
                break;
            }
            self.dirs.remove(parent_rel);
            self.changed = true;
            removed.push(parent_rel.to_string());
            dir = parent.parent();
        }
        removed
    }

    /// Publish the record beside `manifest` when it changed, dropping
    /// entries whose directory is gone or was replaced and rewriting the
    /// rest with their live `dev:ino` (a device renumbered by a reboot is
    /// then matched exactly again, instead of by an ever-later journal
    /// time bound). An empty record
    /// removes the file instead, so a home without created directories
    /// carries no record at all. Returns false when the record could not
    /// be written or removed; the previous record then stays, which only
    /// ever keeps directories.
    pub fn save(&mut self, manifest: &str, home: &str, tool: &MoveTool) -> bool {
        if !self.changed {
            return true;
        }
        let checked: Vec<(String, Option<LiveIdentity>)> = self
            .dirs
            .keys()
            .map(|rel| (rel.clone(), self.still_recorded(home, rel)))
            .collect();
        for (rel, live) in checked {
            match live {
                Some(live) => {
                    self.dirs
                        .insert(rel, temp::identity_string((live.dev, live.ino)));
                }
                None => {
                    self.dirs.remove(&rel);
                }
            }
        }
        let path = record_path(manifest);
        if self.dirs.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => true,
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            };
        }
        let mut content = String::new();
        for (rel, identity) in &self.dirs {
            content.push_str(identity);
            content.push('\t');
            content.push_str(rel);
            content.push('\n');
        }
        let Some(staged) = repos_overlays::stage_sibling(&path, content.as_bytes()) else {
            return false;
        };
        let moved = if std::fs::symlink_metadata(&path).is_ok() {
            temp::move_replace_nodir_with(&staged, &path, tool)
        } else {
            temp::move_noreplace_with(&staged, &path, tool)
        };
        if moved.is_err() {
            let _ = std::fs::remove_file(&staged);
            return false;
        }
        self.changed = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_shape_takes_only_digit_pairs() {
        assert!(identity_shape("2049:131"));
        assert!(!identity_shape("2049"));
        assert!(!identity_shape(":131"));
        assert!(!identity_shape("2049:"));
        assert!(!identity_shape("-1:131"));
        assert!(!identity_shape("20 49:131"));
    }
}
