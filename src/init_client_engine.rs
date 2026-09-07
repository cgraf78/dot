//! Production bindings for the native init-client command.
//!
//! This module owns the orchestration wiring for resume, rollback,
//! and fresh initialization. The underlying identity, transaction,
//! journal, plan, generation, publication, entry, deletion,
//! candidate, and parent operations remain in their focused
//! `init_client_*` modules. This boundary adapts their data shapes
//! without duplicating their behavior.
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell threads its run identity through the
//! `DOT_INIT_*` globals plus `HOME`, `XDG_STATE_HOME`,
//! `DOT_SOURCE_ROOT`, and `DOT_INIT_SKIP_PROVIDER`. Those cross
//! here as [`EngineCtx`]; the run-mutating pair (`DOT_INIT_GIT_DEV`
//! / `DOT_INIT_GIT_INO`, refreshed by `_dot_init_set_git_identity`
//! during staging) threads through an interior cell instead, so the
//! record rewrites observe the staged identity exactly like the
//! shell's globals do. `REPLY`-carried outputs surface as return
//! values, and every rendered report returns its bytes for the
//! caller to emit. Move caches are memoization only (which `mv`
//! binary was probed), so each closure probes fresh instead of
//! sharing one through the call tree.
//!
//! Convergence crosses as the [`Production::new`] `on_converge`
//! application-service closure. Production binds the native update
//! engine directly, while focused tests can inject a deterministic
//! verdict without recursively invoking the CLI.
//!
//! Byte-fidelity boundary: `$HOME/...` joins concatenate bytes like
//! the shell, preserving a doubled separator on trailing-slash
//! inputs instead of normalizing it away (the delete-lane
//! precedent). Journal words cross the UTF-8 boundary with
//! `from_utf8_lossy` (the candidate-lane precedent) except where a
//! hash covers the bytes: [`delete::delete_park_path`] hashes its
//! key, so a non-UTF8 key refuses instead of parking under a lossy
//! name (fail closed). Move caches are bypassed per call (see the
//! engine boundary), which never changes bytes.
//!
//! Error boundary: the deep init functions fail bare (`|| return
//! 1`), so only the dispatcher prints diagnostics. [`Production`]
//! mirrors that split: [`Production::resume`] maps every resume
//! failure to the shell's fixed resume text (the command module
//! renders `Usage` verbatim, so the fixed text travels as `Usage`
//! too), [`Production::rollback`] propagates the rollback tree's
//! own collapse (whose messages are the shell's three rollback
//! diagnostics), and [`Production::run_fresh`] renders the fresh
//! tail's own diagnostic sites directly.

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::errors::{Error, Result};
use crate::init_client_adopt::{self as adopt, AdoptError};
use crate::init_client_candidate::{self as candidate, CandidateScope};
use crate::init_client_command::{FreshInputs, InitReport};
use crate::init_client_delete as delete;
use crate::init_client_entry as entry;
use crate::init_client_generation as generation;
use crate::init_client_git as git;
use crate::init_client_identity as identity;
use crate::init_client_parent as parent;
use crate::init_client_plan as plan;
use crate::init_client_publish as publish;
use crate::init_client_publish_intent as intent;
use crate::init_client_record::{self as record, TransactionRecord};
use crate::init_client_resume as resume;
use crate::init_client_rollback as rollback;
use crate::init_client_safe_path as safe_path;
use crate::init_client_transaction as transaction;
use crate::repos_base::Topology;
use crate::temp;
use crate::{reserved, xdg};

/// The shell's fixed resume-failure text
/// (`_dot_init_error 'initialization transaction could not be
/// resumed safely'`): every resume-step failure renders exactly
/// this, so [`Production::resume`] maps every inner error onto it
/// (twin of the command module's inline bytes, which stay private
/// to that module).
const RESUME_FAILED: &str = "initialization transaction could not be resumed safely";

/// Explicit process inputs for [`Production`]: the shell's `HOME`,
/// `XDG_STATE_HOME`, `DOT_SOURCE_ROOT`, and the validated
/// `DOT_INIT_SKIP_PROVIDER` flag (`true` is `1`; the command gate
/// already rejected every other spelling before the engine runs).
#[derive(Debug, Clone, Copy)]
pub struct EngineCtx<'a> {
    /// Client root (`HOME`).
    pub home: &'a str,
    /// State root override (`XDG_STATE_HOME`, empty counts as
    /// unset, like the shell).
    pub xdg_state_home: &'a str,
    /// Source checkout (`DOT_SOURCE_ROOT`): feeds stage-ownership
    /// recovery, record revisions, and `DOT_BIN` derivation, like
    /// every other content-hash caller.
    pub source_root: &'a Path,
    /// `DOT_INIT_SKIP_PROVIDER` is `1`.
    pub skip_provider: bool,
    /// Captured process-local Shdeps policy override.
    pub shdeps_update_policy: Option<&'a str>,
    /// Process working directory: anchors the reserved-roots probe
    /// and the publish git binding, like the shell's inherited cwd
    /// (the dispatcher reads it; it is process state, like `HOME`).
    /// A symlinked cwd reads physical here while the shell's `$PWD`
    /// stays logical (documented edge; the reserved check
    /// canonicalizes existing directories either way).
    pub cwd: &'a Path,
}

/// Production wiring for one `dot init` run: the closures
/// `run_init` binds, with the update-engine
/// convergence injected (see the module docs). One instance serves
/// a single command invocation; the staged git identity cell starts
/// at the journal values and tracks `_dot_init_set_git_identity`
/// refreshes during staging.
pub struct Production<'a> {
    home: PathBuf,
    home_text: &'a str,
    xdg_state_home: &'a str,
    source_root: &'a Path,
    skip_provider: bool,
    shdeps_update_policy: Option<&'a str>,
    cwd: &'a Path,
    /// Current staged git identity (`DOT_INIT_GIT_DEV` /
    /// `DOT_INIT_GIT_INO`): seeded from the journal, refreshed by
    /// the `set_git_identity` adapters during staging, read by the
    /// record rewrites.
    git_identity: RefCell<(String, String)>,
    /// Update-engine convergence (see the module docs).
    on_converge: &'a dyn Fn() -> Result<()>,
}

impl<'a> Production<'a> {
    /// Bind the parent publisher to the same native collaborators used by the
    /// production engine. Keeping this crate-private builder beside the
    /// orchestration prevents tests and callers from reconstructing subtly
    /// different claim and identity semantics.
    fn parent_hooks<'b>(&'b self, journal: &'b TransactionRecord) -> parent::ParentHooks<'b> {
        parent::ParentHooks {
            parent_record: Box::new(|transaction: &Path, relative: &[u8]| {
                let relative = std::str::from_utf8(relative).map_err(|_| Error::Usage {
                    message: "parent path is not UTF-8",
                })?;
                record::parent_record(
                    transaction,
                    relative,
                    &self.home,
                    &journal.nonce,
                    self.source_root,
                )
                .map(|found| parent_reply(&found))
            }),
            write_private_line: Box::new(|file: &Path, line: &[u8], replace: bool| {
                let line = std::str::from_utf8(line).map_err(|_| Error::Usage {
                    message: "intent line is not UTF-8",
                })?;
                let mut cache = temp::MoveCache::default();
                entry::write_private_line(file, line, replace, &mut cache)
            }),
            stage_claim_write: Box::new(|stage: &Path, kind: &str, path: &[u8]| {
                let path = std::str::from_utf8(path).map_err(|_| Error::Usage {
                    message: "claim path is not UTF-8",
                })?;
                let mut cache = temp::MoveCache::default();
                entry::stage_claim_write(
                    stage,
                    kind,
                    path,
                    &journal.nonce,
                    self.source_root,
                    &mut cache,
                )
            }),
            stage_claim_matches: Box::new(|stage: &Path, kind: &str, path: &[u8]| {
                let path = std::str::from_utf8(path).map_err(|_| Error::Usage {
                    message: "claim path is not UTF-8",
                })?;
                matched(
                    entry::stage_claim_matches(stage, kind, path, &journal.nonce, self.source_root),
                    "stage claim does not match",
                )
            }),
            stage_claim_only: Box::new(|stage: &Path| {
                matched(
                    entry::entry_stage_only_next(stage),
                    "stage is not claim-only",
                )
            }),
            stage_claim_remove: Box::new(|stage: &Path, kind: &str, path: &[u8]| {
                let path = std::str::from_utf8(path).map_err(|_| Error::Usage {
                    message: "claim path is not UTF-8",
                })?;
                entry::stage_claim_remove(stage, kind, path, &journal.nonce, self.source_root)
            }),
            private_directory_matches: Box::new(
                |path: &Path, identity: Option<&str>, mode: Option<&str>| {
                    matched(
                        delete::private_directory_matches(path, identity, mode),
                        "private directory does not match",
                    )
                },
            ),
            private_empty_directory_matches: Box::new(
                |path: &Path, identity: Option<&str>, mode: Option<&str>| {
                    matched(
                        delete::private_empty_directory_matches(path, identity, mode),
                        "private directory is not empty",
                    )
                },
            ),
        }
    }

    /// Bind one run: explicit process inputs plus the convergence
    /// closure. No filesystem or process touch happens here; every
    /// effect runs inside [`Production::resume`],
    /// [`Production::rollback`], or [`Production::run_fresh`].
    pub fn new(ctx: EngineCtx<'a>, on_converge: &'a dyn Fn() -> Result<()>) -> Self {
        Self {
            home: PathBuf::from(ctx.home),
            home_text: ctx.home,
            xdg_state_home: ctx.xdg_state_home,
            source_root: ctx.source_root,
            skip_provider: ctx.skip_provider,
            shdeps_update_policy: ctx.shdeps_update_policy,
            cwd: ctx.cwd,
            git_identity: RefCell::new((String::from("-"), String::from("-"))),
            on_converge,
        }
    }

    /// Run update convergence through the injected application service.
    fn converge(&self) -> Result<()> {
        (self.on_converge)()
    }

    /// `DOT_BIN` for journal writes: `$DOT_SOURCE_ROOT/bin/dot`,
    /// like a development checkout derives it. Standalone releases record the
    /// verified packaged binary at the release root.
    fn dot_bin(&self) -> PathBuf {
        crate::startup::release_binary(self.source_root)
            .unwrap_or_else(|| self.source_root.join("bin/dot"))
    }
}

/// Empty report with an exit code: the shell's bare `return 1`
/// sites (twin of the command module's private helper, which stays
/// private to that module).
fn silent(code: i32) -> InitReport {
    InitReport {
        stdout: Vec::new(),
        stderr: Vec::new(),
        code,
    }
}

/// `_dot_init_error` rendering: `dot init: {message}` on stderr,
/// exit `1` (twin of the command module's private helper).
fn diagnostic(message: &[u8]) -> InitReport {
    let mut stderr = b"dot init: ".to_vec();
    stderr.extend_from_slice(message);
    stderr.push(b'\n');
    InitReport {
        stdout: Vec::new(),
        stderr,
        code: 1,
    }
}

/// A path that exists as anything but a missing name: the shell's
/// `[[ -e $path || -L $path ]]`, which also sees dangling symlinks.
/// `symlink_metadata` never follows, so a link reports itself.
fn exists_lexical(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Raw bytes of a path, so `$HOME/` joins behave like shell string
/// operations even on non-UTF8 inputs.
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Append one `/`-separated leaf, like the shell's `"$base/$leaf"`.
/// Byte concatenation, so a `base` with a trailing slash keeps its
/// doubled separator exactly like the shell's expansion does.
fn join_leaf(base: &Path, leaf: &str) -> PathBuf {
    let mut joined = path_bytes(base).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(leaf.as_bytes());
    PathBuf::from(OsString::from_vec(joined))
}

/// `$HOME/$rel` by byte concatenation, like the shell's
/// `target=$HOME/$path` (the delete-lane precedent).
fn join_home(home: &Path, rel: &[u8]) -> PathBuf {
    let mut joined = path_bytes(home).to_vec();
    joined.push(b'/');
    joined.extend_from_slice(rel);
    PathBuf::from(OsString::from_vec(joined))
}

/// Lift a boolean gate into the engine result: every refusal here
/// feeds a caller that collapses it (the resume wrapper, the
/// rollback publisher), so the message never reaches a stream.
fn matched(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Usage { message })
    }
}

/// View a path as text for the `&str` ports: journal-derived paths
/// are engine vocabulary the lanes gate to UTF-8, so a non-UTF8
/// path refuses instead of crossing lossy into a hashed or
/// compared spelling (fail closed).
fn path_text(path: &Path) -> Result<&str> {
    std::str::from_utf8(path_bytes(path)).map_err(|_| Error::Usage {
        message: "path is not UTF-8",
    })
}

/// Re-serialize a validated entry intent into the six-field
/// `$REPLY` tab join the rollback chapter parses (`phase`,
/// home-relative `stage`, `dev`, `ino`, `next_dev`, `next_ino`):
/// the shell joins exactly these validated fields, so the join is
/// exact.
fn intent_reply(intent: &entry::EntryIntent) -> Vec<u8> {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        intent.phase, intent.stage, intent.dev, intent.ino, intent.next_dev, intent.next_ino
    )
    .into_bytes()
}

/// Re-serialize a validated parent record into the five-field
/// `$REPLY` tab join the rollback chapter parses (`phase`,
/// home-relative `stage`, `dev`, `ino`, `mode`).
fn parent_reply(record: &record::ParentRecord) -> Vec<u8> {
    format!(
        "{}\t{}\t{}\t{}\t{}",
        record.phase, record.stage, record.dev, record.ino, record.mode
    )
    .into_bytes()
}

/// A silent infrastructure refusal: the shell's bare `|| return 1`
/// sites carry no diagnostic, so the adapter must not be `Usage`
/// (the rollback dispatcher renders `Usage` verbatim). The value
/// never reaches a stream — only its silence does.
fn silent_refusal(step: &'static str) -> Error {
    Error::Command {
        command: step.to_string(),
        status: None,
    }
}

/// Effective publication mask after the startup `g-w,o-w` ceiling.
fn publication_mask(mask: u32) -> u32 {
    crate::startup::ensure_umask_ceiling(mask)
}

/// Remove one path like `rm -rf`: missing paths succeed, symlinks
/// remove the link, directories recurse.
fn remove_forced(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            context: "remove path",
            source,
        }),
        Ok(meta) => {
            if meta.file_type().is_symlink() || meta.file_type().is_file() {
                std::fs::remove_file(path).map_err(|source| Error::Io {
                    context: "remove path",
                    source,
                })
            } else if meta.file_type().is_dir() {
                std::fs::remove_dir_all(path).map_err(|source| Error::Io {
                    context: "remove path",
                    source,
                })
            } else {
                std::fs::remove_file(path).map_err(|source| Error::Io {
                    context: "remove path",
                    source,
                })
            }
        }
    }
}

impl<'a> Production<'a> {
    /// Rewrite the transaction record at `phase`: the journal's
    /// stable identity plus the CURRENT staged git identity (see
    /// the module docs), the derived `DOT_BIN`, and the run roots.
    fn write_journal(&self, record: &Path, phase: &str, journal: &TransactionRecord) -> Result<()> {
        let identity = self.git_identity.borrow();
        let dot_bin = self.dot_bin();
        let dot_bin_text = dot_bin.to_string_lossy();
        let git_dir = PathBuf::from(&journal.git_dir);
        let fields = record::RecordFields {
            origin: &journal.origin,
            identity: &journal.identity,
            branch: &journal.branch,
            backup: &journal.backup,
            git_dir: Some(git_dir.as_path()),
            commit: Some(journal.commit.as_str()),
            nonce: Some(journal.nonce.as_str()),
            git_dev: Some(identity.0.as_str()),
            git_ino: Some(identity.1.as_str()),
            dot_bin: dot_bin_text.as_ref(),
            home: &self.home,
            source_root: self.source_root,
        };
        let mut cache = temp::MoveCache::default();
        record::write_record(record, phase, &fields, &mut cache)
    }

    /// Seed the staged-identity cell from a freshly read journal:
    /// later `_dot_init_set_git_identity` refreshes overwrite it,
    /// exactly like the shell's globals.
    fn seed_identity(&self, journal: &TransactionRecord) {
        *self.git_identity.borrow_mut() = (journal.git_dev.clone(), journal.git_ino.clone());
    }

    /// `_dot_init_resume_transaction` with every cross-lane step
    /// bound to its ported owner (see the module docs for the lane
    /// map). The shell prints its fixed resume text for every
    /// failure here, so every error maps onto it. Convergence keeps
    /// its own output in the application-service stream buffers.
    pub fn resume(
        &self,
        transaction: &Path,
        record: &Path,
        journal: &TransactionRecord,
    ) -> Result<()> {
        self.seed_identity(journal);
        let backup = Path::new(&journal.backup);
        let git_dir = Path::new(&journal.git_dir);
        let origin = Path::new(&journal.origin);
        let ensure_private_dir = |path: &Path| {
            matched(
                transaction::private_directory(path),
                "cannot provision private directory",
            )
        };
        let path_identity = |path: &Path| temp::path_identity(path).map(temp::identity_string);
        let repo_identity = |origin: &str| {
            identity::repo_identity(origin).ok_or(Error::Usage {
                message: "unsupported repository URL",
            })
        };
        let record_phase = |record: &Path, phase: &str| self.write_journal(record, phase, journal);
        let state_matches = |target: &Path,
                             kind: &str,
                             dev: &str,
                             ino: &str,
                             mode: &str,
                             size: &str,
                             value: &str| {
            candidate::path_state_matches(target, kind, dev, ino, mode, size, value)
        };
        let move_conflicts = |manifest: &Path, backup: &Path| {
            let mut cache = temp::MoveCache::default();
            plan::move_conflicts(
                manifest,
                backup,
                &self.home,
                self.source_root,
                &state_matches,
                &ensure_private_dir,
                &mut cache,
            )
        };
        let generation_matches = |git_dir: &Path| {
            generation::generation_marker_matches(
                git_dir,
                &journal.nonce,
                &journal.commit,
                &journal.identity,
            )
        };
        let git_generation_matches = |git_dir: &Path| {
            generation::generation_matches(
                git_dir,
                &journal.branch,
                &journal.nonce,
                &journal.commit,
                &journal.identity,
            )
        };
        let configure_metadata_modes =
            |git_dir: &Path| generation::configure_git_metadata_modes(git_dir);
        let set_git_identity = |git_dir: &Path| {
            let (dev, ino) = generation::set_git_identity(git_dir)?;
            *self.git_identity.borrow_mut() = (dev.to_string(), ino.to_string());
            Ok(())
        };
        let write_generation_marker = |git_dir: &Path| {
            let mut cache = temp::MoveCache::default();
            generation::write_generation_marker(
                git_dir,
                &journal.nonce,
                &journal.commit,
                &journal.identity,
                &mut cache,
            )
        };
        let move_noreplace = |source: &Path, target: &Path| {
            let mut cache = temp::MoveCache::default();
            temp::move_noreplace_cached(source, target, &mut cache)
        };
        let stage_deps = git::GitStageDeps {
            ensure_private_dir: &ensure_private_dir,
            generation_matches: &git_generation_matches,
            configure_metadata_modes: &configure_metadata_modes,
            set_git_identity: &set_git_identity,
            write_generation_marker: &write_generation_marker,
            move_noreplace: &move_noreplace,
            record_phase: &record_phase,
        };
        // The shell re-reads its globals per staging call, so every
        // call carries the journal-stable identity with its own
        // record path.
        let stage_git = |record: &Path| {
            let inputs = git::GitStageInputs {
                record,
                backup,
                git_dir,
                origin,
                branch: &journal.branch,
                commit: &journal.commit,
                identity: &journal.identity,
                nonce: &journal.nonce,
                home: &self.home,
            };
            git::stage_git(&inputs, &stage_deps)
        };
        let publish_git = |record: &Path| {
            let inputs = git::GitStageInputs {
                record,
                backup,
                git_dir,
                origin,
                branch: &journal.branch,
                commit: &journal.commit,
                identity: &journal.identity,
                nonce: &journal.nonce,
                home: &self.home,
            };
            git::publish_git(&inputs, &stage_deps)
        };
        let live = resume::LiveGitDeps {
            path_identity: &path_identity,
            generation_matches: &generation_matches,
            repo_identity: &repo_identity,
        };
        let publish_git_binding = publish::PublishGit {
            git_dir,
            commit: &journal.commit,
            branch: &journal.branch,
            work_dir: self.cwd,
        };
        let prior_record = |prior: &Path, path: &str| {
            record::prior_record(prior, path).map(|entry| publish::PriorRecord {
                kind: entry.kind,
                dev: entry.dev,
                ino: entry.ino,
                mode: entry.mode,
                size: entry.size,
                value: entry.value,
            })
        };
        let candidate_matches_git = |mode: &str, oid: &str, path: &str| {
            delete::candidate_matches_git(git_dir, &journal.commit, mode, oid, path, &self.home)
        };
        let publish_one =
            |transaction: &Path, intent_file: &Path, mode: &str, oid: &str, path: &str| {
                self.publish_one_step(transaction, intent_file, mode, oid, path, journal)
            };
        let entry_intent = |file: &Path, mode: &str, oid: &str, path: &str| {
            entry::entry_intent(
                file,
                mode,
                oid,
                path,
                &self.home,
                &journal.nonce,
                self.source_root,
            )
            .map(|intent| publish::IntentRecord {
                phase: intent.phase,
                stage: intent.stage,
                dev: intent.dev,
                ino: intent.ino,
                next_dev: intent.next_dev,
                next_ino: intent.next_ino,
            })
        };
        let private_directory_matches = |path: &Path, identity: &str, mode: &str| {
            delete::private_directory_matches(path, non_empty(identity), non_empty(mode))
        };
        let stage_only_next = |stage: &Path| entry::entry_stage_only_next(stage);
        let stage_claim_matches = |stage: &Path, kind: &str, path: &str| {
            entry::stage_claim_matches(stage, kind, path, &journal.nonce, self.source_root)
        };
        let private_empty_directory_matches = |path: &Path, identity: &str, mode: &str| {
            delete::private_empty_directory_matches(path, non_empty(identity), non_empty(mode))
        };
        let stage_claim_remove = |stage: &Path, kind: &str, path: &str| {
            entry::stage_claim_remove(stage, kind, path, &journal.nonce, self.source_root)
        };
        let publish_intent = |file: &Path, mode: &str, oid: &str, path: &str| {
            self.publish_intent_step(file, mode, oid, path, journal)
        };
        let stages = publish::StageHooks {
            private_directory_matches: &private_directory_matches,
            stage_only_next: &stage_only_next,
            stage_claim_matches: &stage_claim_matches,
            private_empty_directory_matches: &private_empty_directory_matches,
            stage_claim_remove: &stage_claim_remove,
        };
        let hooks = publish::PublishHooks {
            prior_record: &prior_record,
            candidate_matches_git: &candidate_matches_git,
            path_state_matches: &state_matches,
            publish_intent: &publish_intent,
            publish_one: &publish_one,
            entry_intent: &entry_intent,
            stages,
        };
        let publish_worktree = |transaction: &Path| {
            publish::publish_worktree(
                transaction,
                &self.home,
                backup,
                &publish_git_binding,
                &hooks,
            )
        };
        let forward_converge = || self.converge();
        let publish_completed = |record: &Path| {
            let completed = transaction::completed_file(self.home_text, self.xdg_state_home)
                .map(PathBuf::from)
                .map_err(|_| silent_refusal("resolve completed file"))?;
            let mut cache = temp::MoveCache::default();
            plan::publish_completed(record, &completed, &ensure_private_dir, &mut cache)
        };
        let deps = resume::ResumeDeps {
            live: &live,
            record_phase: &record_phase,
            move_conflicts: &move_conflicts,
            stage_git: &stage_git,
            publish_git: &publish_git,
            publish_worktree: &publish_worktree,
            forward_converge: &forward_converge,
            publish_completed: &publish_completed,
        };
        let git = resume::LiveGitInputs {
            git_dir,
            git_dev: &journal.git_dev,
            git_ino: &journal.git_ino,
            nonce: &journal.nonce,
            identity: &journal.identity,
            branch: &journal.branch,
            home: &self.home,
        };
        let inputs = resume::ResumeInputs {
            transaction,
            record,
            phase: &journal.phase,
            backup,
            nonce: &journal.nonce,
            git,
        };
        match resume::resume_transaction(&inputs, &deps) {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::Usage {
                message: RESUME_FAILED,
            }),
        }
    }
}

/// An empty shell `${var:-}` default reads as absent: the verifier
/// lanes take `None` for absent identity and mode arguments.
fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

impl<'a> Production<'a> {
    /// One `_dot_init_publish_one` entry for the worktree loop,
    /// with the parent/entry/record/candidate collaborators bound
    /// to their ported owners.
    #[allow(clippy::too_many_arguments)]
    fn publish_one_step(
        &self,
        transaction: &Path,
        intent_file: &Path,
        mode: &str,
        oid: &str,
        path: &str,
        journal: &TransactionRecord,
    ) -> Result<()> {
        let ensure_parents = |transaction: &Path, relative: &str| {
            let hooks = self.parent_hooks(journal);
            let mut cache = temp::MoveCache::default();
            parent::parent_directories(
                &hooks,
                transaction,
                relative.as_bytes(),
                &self.home,
                &journal.nonce,
                &mut cache,
            )
        };
        let read_intent = |file: &Path, mode: &str, oid: &str, path: &str| {
            entry::entry_intent(
                file,
                mode,
                oid,
                path,
                &self.home,
                &journal.nonce,
                self.source_root,
            )
        };
        let claim_matches = |stage: &Path, path: &str| {
            entry::stage_claim_matches(stage, "entry", path, &journal.nonce, self.source_root)
        };
        let claim_write = |stage: &Path, path: &str| {
            let mut cache = temp::MoveCache::default();
            entry::stage_claim_write(
                stage,
                "entry",
                path,
                &journal.nonce,
                self.source_root,
                &mut cache,
            )
        };
        let claim_remove = |stage: &Path, path: &str| {
            entry::stage_claim_remove(stage, "entry", path, &journal.nonce, self.source_root)
        };
        let write_line = |file: &Path, line: &str, replace: bool| {
            let mut cache = temp::MoveCache::default();
            entry::write_private_line(file, line, replace, &mut cache)
        };
        // Positional order is `(git_dir, commit, mode, oid,
        // path)`: the entry lane threads its own coordinates, so
        // the journal stays out of it.
        let candidate_matches = |git_dir: &str, commit: &str, mode: &str, oid: &str, path: &str| {
            delete::candidate_matches_git(Path::new(git_dir), commit, mode, oid, path, &self.home)
        };
        let inputs = entry::PublishOneInputs {
            home: &self.home,
            transaction,
            intent: intent_file,
            git_dir: &journal.git_dir,
            commit: &journal.commit,
            mode,
            oid,
            path,
            mask: publication_mask(temp::read_umask()?),
            ensure_parents: &ensure_parents,
            read_intent: &read_intent,
            claim_matches: &claim_matches,
            claim_write: &claim_write,
            claim_remove: &claim_remove,
            write_line: &write_line,
            candidate_matches: &candidate_matches,
        };
        let mut moves = temp::MoveCache::default();
        entry::publish_one(&inputs, &mut moves)
    }

    /// One `_dot_init_publish_intent` record for the worktree loop,
    /// with the entry collaborators bound to their ported owners.
    fn publish_intent_step(
        &self,
        file: &Path,
        mode: &str,
        oid: &str,
        path: &str,
        journal: &TransactionRecord,
    ) -> Result<()> {
        let hooks = intent::PublishIntentHooks {
            entry_stage: Box::new(|path: &[u8]| {
                let path = std::str::from_utf8(path).map_err(|_| Error::Usage {
                    message: "entry path is not UTF-8",
                })?;
                entry::entry_stage(&self.home, path, &journal.nonce, self.source_root)
            }),
            entry_intent: Box::new(|file: &Path, mode: &str, oid: &str, path: &[u8]| {
                let path = std::str::from_utf8(path).map_err(|_| Error::Usage {
                    message: "entry path is not UTF-8",
                })?;
                entry::entry_intent(
                    file,
                    mode,
                    oid,
                    path,
                    &self.home,
                    &journal.nonce,
                    self.source_root,
                )
                .map(|_| ())
            }),
            write_private_line: Box::new(|file: &Path, line: &[u8]| {
                let line = std::str::from_utf8(line).map_err(|_| Error::Usage {
                    message: "intent line is not UTF-8",
                })?;
                let mut cache = temp::MoveCache::default();
                entry::write_private_line(file, line, false, &mut cache)
            }),
        };
        intent::publish_intent(&hooks, file, mode, oid, path.as_bytes(), &self.home)
    }

    /// Read one transaction record into the rollback chapter's run
    /// identity plus the generation-match pair the parked-git
    /// verifier curries (branch and repository identity travel
    /// beside the record context, like the shell's globals do).
    /// Any failure (missing, malformed, unreadable) is the shell's
    /// `no recoverable transaction` at the caller.
    fn read_ctx(
        &self,
        record: &Path,
    ) -> std::result::Result<(rollback::RecordCtx, String, String), Error> {
        let journal = record::read_record(record, &self.home)?;
        let ctx = rollback::RecordCtx {
            phase: journal.phase.clone(),
            backup: PathBuf::from(journal.backup.clone()),
            nonce: journal.nonce.clone(),
            git_dir: PathBuf::from(journal.git_dir.clone()),
            commit: journal.commit.clone(),
            git_identity: format!("{}:{}", journal.git_dev, journal.git_ino),
        };
        Ok((ctx, journal.branch.clone(), journal.identity.clone()))
    }

    /// `_dot_init_rollback` with every cross-lane step bound to its
    /// ported owner. The rollback tree already collapses its inner
    /// failures onto the shell's three rollback diagnostics (or
    /// stays silent on the bare sites), so errors propagate
    /// unchanged.
    pub fn rollback(&self, at: &Path) -> Result<()> {
        let transaction = transaction::transaction_dir(self.home_text, self.xdg_state_home)
            .map(PathBuf::from)
            .map_err(|_| silent_refusal("resolve transaction directory"))?;
        let (ctx, branch, repo_identity) =
            self.read_ctx(&transaction.join("record"))
                .map_err(|_| Error::Usage {
                    message: "no recoverable transaction",
                })?;
        let state_matches = |target: &Path,
                             kind: &str,
                             dev: &str,
                             ino: &str,
                             mode: &str,
                             size: &str,
                             value: &str| {
            candidate::path_state_matches(target, kind, dev, ino, mode, size, value)
        };
        let entry_intent = |intent: &Path, mode: &str, oid: &str, path: &Path| {
            let path = path_text(path)?;
            entry::entry_intent(
                intent,
                mode,
                oid,
                path,
                &self.home,
                &ctx.nonce,
                self.source_root,
            )
            .map(|found| intent_reply(&found))
        };
        let delete_park_path = |target: &Path, kind: &str, key: &[u8]| {
            let key = std::str::from_utf8(key).map_err(|_| Error::Usage {
                message: "park key is not UTF-8",
            })?;
            delete::delete_park_path(target, kind, key, &ctx.nonce)
        };
        let remove_parked_leaf = |target: &Path,
                                  park: &Path,
                                  identity: &str,
                                  git_dir: &Path,
                                  commit: &str,
                                  mode: &str,
                                  oid: &str| {
            let verifier = |park: &Path| {
                delete::leaf_delete_matches(park, identity, git_dir, commit, mode, oid, &self.home)
            };
            let mut cache = temp::MoveCache::default();
            matched(
                delete::delete_parked_generation(target, park, "leaf", &verifier, &mut cache),
                "parked leaf does not match",
            )
        };
        let entry_stage_valid = |stage: &Path, expected: Option<&str>| {
            matched(
                entry::entry_stage_valid(stage, expected),
                "entry stage is not valid",
            )
        };
        let stage_claim_matches = |stage: &Path, kind: &str, path: &Path| {
            let path = path_text(path)?;
            matched(
                entry::stage_claim_matches(stage, kind, path, &ctx.nonce, self.source_root),
                "stage claim does not match",
            )
        };
        let entry_stage_only_next = |stage: &Path| {
            matched(
                entry::entry_stage_only_next(stage),
                "stage holds more than next",
            )
        };
        let discard_staged_next = |stage: &Path| entry::discard_staged_next(stage);
        let path_identity = |path: &Path| {
            temp::path_identity(path)
                .ok()
                .map(temp::identity_string)
                .unwrap_or_default()
        };
        let candidate_matches_git =
            |git_dir: &Path, commit: &str, mode: &str, oid: &str, relative: &str| {
                matched(
                    delete::candidate_matches_git(git_dir, commit, mode, oid, relative, &self.home),
                    "candidate does not match git",
                )
            };
        let stage_claim_remove = |stage: &Path, kind: &str, path: &Path| {
            let path = path_text(path)?;
            entry::stage_claim_remove(stage, kind, path, &ctx.nonce, self.source_root)
        };
        let parent_record = |transaction: &Path, parent: &Path| {
            let parent = path_text(parent)?;
            record::parent_record(
                transaction,
                parent,
                &self.home,
                &ctx.nonce,
                self.source_root,
            )
            .map(|found| parent_reply(&found))
        };
        let safe_relative_path = |parent: &Path| {
            matched(
                safe_path::safe_relative_path(path_bytes(parent)),
                "parent path is not safe",
            )
        };
        let remove_parked_parent = |target: &Path, park: &Path, identity: &str, mode: &str| {
            let verifier = |park: &Path| delete::parent_delete_matches(park, identity, mode);
            let mut cache = temp::MoveCache::default();
            matched(
                delete::delete_parked_generation(target, park, "parent", &verifier, &mut cache),
                "parked parent does not match",
            )
        };
        let private_directory_matches =
            |stage: &Path, identity: Option<&str>, mode: Option<&str>| {
                matched(
                    delete::private_directory_matches(stage, identity, mode),
                    "private directory does not match",
                )
            };
        let stage_claim_only =
            |stage: &Path| matched(entry::stage_claim_only(stage), "stage is not claim-only");
        let private_empty_directory_matches =
            |stage: &Path, identity: Option<&str>, mode: Option<&str>| {
                matched(
                    delete::private_empty_directory_matches(stage, identity, mode),
                    "private directory is not empty",
                )
            };
        let remove_parked_tree = |git_dir: &Path, park: &Path, identity: &str| {
            let verifier = |park: &Path| {
                delete::git_delete_matches(
                    park,
                    identity,
                    &ctx.nonce,
                    &ctx.commit,
                    &repo_identity,
                    &branch,
                )
            };
            let mut cache = temp::MoveCache::default();
            matched(
                delete::delete_parked_generation(git_dir, park, "tree", &verifier, &mut cache),
                "parked git tree does not match",
            )
        };
        let transaction_dir = || {
            transaction::transaction_dir(self.home_text, self.xdg_state_home)
                .map(PathBuf::from)
                .map_err(|_| silent_refusal("resolve transaction directory"))
        };
        let read_record = |record: &Path| self.read_ctx(record).map(|(ctx, _, _)| ctx);
        let restore_backups = |backup: &Path| {
            let mut cache = temp::MoveCache::default();
            plan::restore_backups(backup, &self.home, &state_matches, &mut cache)
                .map_err(|_| silent_refusal("restore backups"))
        };
        let deps = rollback::RollbackDeps {
            entry_intent: Box::new(entry_intent),
            delete_park_path: Box::new(delete_park_path),
            remove_parked_leaf: Box::new(remove_parked_leaf),
            entry_stage_valid: Box::new(entry_stage_valid),
            stage_claim_matches: Box::new(stage_claim_matches),
            entry_stage_only_next: Box::new(entry_stage_only_next),
            discard_staged_next: Box::new(discard_staged_next),
            path_identity: Box::new(path_identity),
            candidate_matches_git: Box::new(candidate_matches_git),
            stage_claim_remove: Box::new(stage_claim_remove),
            parent_record: Box::new(parent_record),
            safe_relative_path: Box::new(safe_relative_path),
            remove_parked_parent: Box::new(remove_parked_parent),
            private_directory_matches: Box::new(private_directory_matches),
            stage_claim_only: Box::new(stage_claim_only),
            private_empty_directory_matches: Box::new(private_empty_directory_matches),
            remove_parked_tree: Box::new(remove_parked_tree),
            transaction_dir: Box::new(transaction_dir),
            read_record: Box::new(read_record),
            restore_backups: Box::new(restore_backups),
        };
        rollback::rollback(&deps, at)
    }
}

/// Fresh-run adoption outcome: the shell's `adoption_rc` (`0`
/// adopted, `1` no repository, `2` mismatch, `3` failed adoption).
enum AdoptOutcome {
    /// Adopted and converged: the command succeeds silently.
    Adopted,
    /// No adoptable repository: fall through to the candidate
    /// build, like the shell's unmatched `adoption_rc=1`.
    Absent,
    /// A repository exists but is untrusted: the mismatch
    /// diagnostic.
    Mismatch,
    /// An exactly matched repository failed adoption: silent
    /// failure (or the converge boundary, when reached).
    Failed,
}

impl<'a> Production<'a> {
    /// Select the legacy separate Git directory directly from the client.
    /// Ordinary adoption remains the `Missing` branch in `adopt_existing`,
    /// which validates `$HOME/.git` and its exact worktree root.
    fn selected_topology(&self) -> Topology {
        let dotfiles = join_leaf(&self.home, ".dotfiles");
        match std::fs::symlink_metadata(dotfiles) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Topology::Separate,
            _ => Topology::Missing,
        }
    }

    /// Run `_dot_init_adopt_existing` with the adoption engine
    /// bound to the ported owners.
    fn adopt_existing(&self, inputs: &FreshInputs) -> AdoptOutcome {
        let dotfiles = join_leaf(&self.home, ".dotfiles");
        let single_origin = |topology: Topology| {
            let scope = match topology {
                Topology::Separate => publish::OriginScope::Separate { git_dir: &dotfiles },
                _ => publish::OriginScope::Ordinary,
            };
            publish::single_origin(&scope, &self.home)
                .ok()
                .and_then(|bytes| {
                    // Command substitution strips every trailing
                    // newline; origins are newline-free by the
                    // safe-value gate, so one strip equals all.
                    let mut text = bytes;
                    while text.last() == Some(&b'\n') {
                        text.pop();
                    }
                    String::from_utf8(text).ok()
                })
        };
        let repo_identity = |origin: &str| identity::repo_identity(origin);
        let transaction_dir = || {
            transaction::transaction_dir(self.home_text, self.xdg_state_home)
                .map(PathBuf::from)
                .ok()
        };
        let prepare_transaction =
            |transaction: &Path| transaction::prepare_transaction(transaction).ok();
        let dot_bin = self.dot_bin();
        let dot_bin_text = dot_bin.to_string_lossy();
        let write_record = |fields: &adopt::RecordFields<'_>| {
            let journal = record::RecordFields {
                origin: fields.origin,
                identity: fields.identity,
                branch: fields.branch,
                backup: fields.backup,
                git_dir: Some(fields.git_dir),
                commit: Some(fields.commit),
                nonce: Some(fields.nonce),
                git_dev: Some(fields.git_dev),
                git_ino: Some(fields.git_ino),
                dot_bin: dot_bin_text.as_ref(),
                home: &self.home,
                source_root: self.source_root,
            };
            let mut cache = temp::MoveCache::default();
            record::write_record(fields.record, fields.phase, &journal, &mut cache).is_ok()
        };
        let publish_transaction = |stage: &Path, transaction: &Path| {
            let mut cache = temp::MoveCache::default();
            transaction::publish_transaction(self.source_root, stage, transaction, &mut cache)
        };
        let forward_converge = |_topology: Topology, _git_dir: &Path| self.converge().is_ok();
        let ensure_private_dir = |path: &Path| {
            matched(
                transaction::private_directory(path),
                "cannot provision private directory",
            )
        };
        let publish_completed = |record: &Path| {
            let completed = match transaction::completed_file(self.home_text, self.xdg_state_home) {
                Ok(completed) => PathBuf::from(completed),
                Err(_) => return false,
            };
            let mut cache = temp::MoveCache::default();
            plan::publish_completed(record, &completed, &ensure_private_dir, &mut cache).is_ok()
        };
        let engine = adopt::AdoptEngine {
            single_origin: &single_origin,
            repo_identity: &repo_identity,
            transaction_dir: &transaction_dir,
            prepare_transaction: &prepare_transaction,
            write_record: &write_record,
            publish_transaction: &publish_transaction,
            forward_converge: &forward_converge,
            publish_completed: &publish_completed,
        };
        match adopt::adopt_existing(
            &self.home,
            self.selected_topology(),
            &inputs.origin,
            &inputs.identity,
            &inputs.branch,
            &engine,
        ) {
            Ok(_) => AdoptOutcome::Adopted,
            Err(AdoptError::NoRepository) => AdoptOutcome::Absent,
            Err(AdoptError::Mismatch) => AdoptOutcome::Mismatch,
            Err(AdoptError::Failed) => AdoptOutcome::Failed,
        }
    }

    /// Build the candidate planner scope: the reserved-roots
    /// inventory from the same environment the shell probe sees
    /// (`SHDEPS_*` overrides with the usual empty-counts-as-unset
    /// rule, no overlays and no backup this early in a fresh run).
    /// A failed snapshot fails the candidate, like the shell's
    /// reserved verdict on an unreadable inventory.
    fn candidate_scope(&self) -> Option<CandidateScope> {
        let home = self.home_text.to_string();
        let install_root = match std::env::var("SHDEPS_INSTALL_DIR") {
            Ok(dir) if !dir.is_empty() => dir,
            _ => format!("{home}/.local/share"),
        };
        let state_home = xdg::base(xdg::Kind::State, self.xdg_state_home, self.home_text).ok()?;
        let provider_state = match std::env::var("SHDEPS_STATE_DIR") {
            Ok(dir) if !dir.is_empty() => dir,
            _ => format!("{state_home}/shdeps"),
        };
        let roots = reserved::reserved_roots(
            &reserved::RootsInput {
                home: home.clone(),
                state_home,
                install_root: install_root.clone(),
                provider_state,
                overlay_paths: Vec::new(),
                init_backup: None,
            },
            &self.cwd.to_string_lossy(),
        )
        .ok()?;
        Some(CandidateScope {
            home,
            checkout: format!("{install_root}/cgraf78/dot"),
            pwd: self.cwd.to_string_lossy().into_owned(),
            source_root: self.source_root.to_path_buf(),
            roots,
        })
    }

    /// Allocate the candidate checkout (`mktemp -d
    /// <state>/.candidate.XXXXXX` at mode 700, like the shell):
    /// six mktemp-alphabet characters drawn from the shared
    /// generator, retrying collisions the way `mktemp` does. A
    /// failure after the directory exists leaves it behind, like
    /// the shell's split `mktemp` plus `chmod`.
    fn make_candidate(&self, state_root: &Path) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        for _ in 0..temp::TMP_RETRIES {
            let mut name = state_root.as_os_str().to_os_string();
            name.push(".candidate.");
            name.push(temp::random_suffix());
            let candidate = PathBuf::from(name);
            match std::fs::create_dir(&candidate) {
                Ok(()) => {
                    if std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700))
                        .is_ok()
                    {
                        return Some(candidate);
                    }
                    return None;
                }
                Err(_) => continue,
            }
        }
        None
    }

    /// The line-1872+ fresh tail of `dot_init_command`:
    /// completed-file fast path, adoption, candidate build, plan
    /// review, confirmation, staging, publication, and the closing
    /// resume. Reports carry the shell's streams: the plan and
    /// confirmation print to stderr as they run (so failures after
    /// them keep the printed bytes), while stdout stays empty —
    /// only the convergence step prints there, through the caller's
    /// application-service stream buffers.
    pub fn run_fresh(&self, inputs: &FreshInputs) -> InitReport {
        let transaction = match transaction::transaction_dir(self.home_text, self.xdg_state_home) {
            Ok(directory) => PathBuf::from(directory),
            Err(_) => return silent(1),
        };
        let completed = match transaction::completed_file(self.home_text, self.xdg_state_home) {
            Ok(file) => PathBuf::from(file),
            Err(_) => return silent(1),
        };
        if exists_lexical(&completed) {
            let journal = match record::read_record(&completed, &self.home) {
                Ok(journal) => journal,
                Err(_) => {
                    let mut message = b"malformed completion record: ".to_vec();
                    message.extend_from_slice(path_bytes(&completed));
                    return diagnostic(&message);
                }
            };
            if journal.identity != inputs.identity || journal.branch != inputs.branch {
                return diagnostic(
                    b"initialized client belongs to a different repository or branch",
                );
            }
            if journal.phase != "complete" {
                return diagnostic(b"completion record is not in the complete phase");
            }
            let dotfiles = join_leaf(&self.home, ".dotfiles");
            let dot_git = join_leaf(&self.home, ".git");
            if journal.git_dir.as_bytes() == path_bytes(&dotfiles)
                && !exists_lexical(Path::new(&journal.git_dir))
                && !exists_lexical(&dot_git)
            {
                // Removing the separate git directory is the
                // documented manual recovery boundary: retire only
                // this already-validated completion record, then
                // let the normal transaction rebuild the client.
                if remove_forced(&completed).is_err() {
                    return silent(1);
                }
            } else {
                let git_dir = PathBuf::from(&journal.git_dir);
                let path_identity =
                    |path: &Path| temp::path_identity(path).map(temp::identity_string);
                let generation_matches = |git_dir: &Path| {
                    generation::generation_marker_matches(
                        git_dir,
                        &journal.nonce,
                        &journal.commit,
                        &journal.identity,
                    )
                };
                let repo_identity = |origin: &str| {
                    identity::repo_identity(origin).ok_or(Error::Usage {
                        message: "unsupported repository URL",
                    })
                };
                let live = resume::LiveGitInputs {
                    git_dir: &git_dir,
                    git_dev: &journal.git_dev,
                    git_ino: &journal.git_ino,
                    nonce: &journal.nonce,
                    identity: &journal.identity,
                    branch: &journal.branch,
                    home: &self.home,
                };
                let deps = resume::LiveGitDeps {
                    path_identity: &path_identity,
                    generation_matches: &generation_matches,
                    repo_identity: &repo_identity,
                };
                if !resume::live_git_matches_record(&live, &deps) {
                    return diagnostic(
                        b"initialized client Git generation no longer matches its record",
                    );
                }
                return match self.converge() {
                    Ok(()) => silent(0),
                    Err(_) => silent(1),
                };
            }
        }
        match self.adopt_existing(inputs) {
            AdoptOutcome::Adopted => return silent(0),
            AdoptOutcome::Absent => {}
            AdoptOutcome::Mismatch => {
                return diagnostic(
                    b"existing client repository does not match the requested origin and branch",
                );
            }
            AdoptOutcome::Failed => {
                return silent(1);
            }
        }
        let state_root = match transaction::state_root(self.home_text, self.xdg_state_home) {
            Ok(root) => PathBuf::from(root),
            Err(_) => return silent(1),
        };
        if !transaction::private_directory(&state_root) {
            return silent(1);
        }
        let candidate = match self.make_candidate(&state_root) {
            Some(candidate) => candidate,
            None => return silent(1),
        };
        // Clone streams merge into every later report: the shell
        // prints them inline (a quiet success prints nothing, so
        // the accumulators usually stay empty).
        let clone = git_clone(&inputs.origin, &inputs.branch, &candidate);
        if !clone.cloned {
            let _ = remove_forced(&candidate);
            return InitReport {
                stdout: clone.stdout,
                stderr: clone.stderr,
                code: 1,
            };
        }
        let (out_stdout, mut out_stderr) = (clone.stdout, clone.stderr);
        let commit = match git_rev_parse(&candidate, &inputs.branch) {
            Some(commit) => commit,
            None => {
                let _ = remove_forced(&candidate);
                return InitReport {
                    stdout: out_stdout,
                    stderr: out_stderr,
                    code: 1,
                };
            }
        };
        if !commit_valid(&commit) {
            // The shell's shape gate has no cleanup: the candidate
            // stays behind, exactly like production.
            return InitReport {
                stdout: out_stdout,
                stderr: out_stderr,
                code: 1,
            };
        }
        let scope = match self.candidate_scope() {
            Some(scope) => scope,
            None => {
                let _ = remove_forced(&candidate);
                out_stderr.extend_from_slice(
                    b"dot init: candidate tree is empty, unsafe, or contains unsupported entries\n",
                );
                return InitReport {
                    stdout: out_stdout,
                    stderr: out_stderr,
                    code: 1,
                };
            }
        };
        let tree = candidate.join("tree.tsv");
        let prior = candidate.join("prior.tsv");
        let conflicts = candidate.join("conflicts.tsv");
        if candidate::candidate_tree(&candidate, &inputs.branch, &tree, &scope).is_err() {
            let _ = remove_forced(&candidate);
            out_stderr.extend_from_slice(
                b"dot init: candidate tree is empty, unsafe, or contains unsupported entries\n",
            );
            return InitReport {
                stdout: out_stdout,
                stderr: out_stderr,
                code: 1,
            };
        }
        if candidate::build_prior_and_conflicts(
            &candidate,
            &inputs.branch,
            &tree,
            &prior,
            &conflicts,
            &scope,
        )
        .is_err()
        {
            // The shell's prior gate has no cleanup either.
            return InitReport {
                stdout: out_stdout,
                stderr: out_stderr,
                code: 1,
            };
        }
        let backup = join_home(
            &self.home,
            format!(
                ".dot-backup/{}-{}",
                backup_stamp().unwrap_or_default(),
                std::process::id()
            )
            .as_bytes(),
        );
        let backup_text = backup.to_string_lossy();
        match plan::plan_summary(&plan::PlanInputs {
            candidate: &candidate,
            branch: &inputs.branch,
            tree: &tree,
            backup: backup_text.as_ref(),
            identity: &inputs.identity,
            home: &self.home,
            skip_provider: self.skip_provider,
            env_policy: self.shdeps_update_policy,
        }) {
            Ok(report) => out_stderr.extend_from_slice(&report),
            Err(_) => {
                let _ = remove_forced(&candidate);
                out_stderr.extend_from_slice(b"dot init: candidate configuration is invalid\n");
                return InitReport {
                    stdout: out_stdout,
                    stderr: out_stderr,
                    code: 1,
                };
            }
        };
        match plan::confirm(&conflicts, inputs.yes, Path::new("/dev/tty")) {
            Ok(report) => out_stderr.extend_from_slice(&report),
            Err(Error::Usage { message })
                if message == "conflicts require --yes in a noninteractive session" =>
            {
                out_stderr.extend_from_slice(&confirm_listing(&conflicts));
                out_stderr.extend_from_slice(b"dot init: ");
                out_stderr.extend_from_slice(message.as_bytes());
                out_stderr.push(b'\n');
                let _ = remove_forced(&candidate);
                return InitReport {
                    stdout: out_stdout,
                    stderr: out_stderr,
                    code: 1,
                };
            }
            Err(_) => {
                out_stderr.extend_from_slice(&confirm_listing(&conflicts));
                let _ = remove_forced(&candidate);
                return InitReport {
                    stdout: out_stdout,
                    stderr: out_stderr,
                    code: 1,
                };
            }
        }
        // Every failure past this point keeps the printed plan and
        // confirmation bytes: the shell printed them before failing.
        let carried = |stderr: Vec<u8>| InitReport {
            stdout: out_stdout.clone(),
            stderr,
            code: 1,
        };
        let stage = match transaction::prepare_transaction(&transaction) {
            Ok(stage) => stage,
            Err(_) => return carried(out_stderr),
        };
        for (source, name) in [
            (&tree, "tree.tsv"),
            (&prior, "prior.tsv"),
            (&conflicts, "conflicts.tsv"),
        ] {
            if std::fs::copy(source, stage.join(name)).is_err() {
                return carried(out_stderr);
            }
        }
        {
            use std::os::unix::fs::PermissionsExt as _;
            if std::fs::set_permissions(
                stage.join("tree.tsv"),
                std::fs::Permissions::from_mode(0o600),
            )
            .is_err()
                || std::fs::set_permissions(
                    stage.join("prior.tsv"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .is_err()
                || std::fs::set_permissions(
                    stage.join("conflicts.tsv"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .is_err()
            {
                return carried(out_stderr);
            }
        }
        let nonce = match fresh_nonce() {
            Some(nonce) => nonce,
            None => return carried(out_stderr),
        };
        let dot_bin = self.dot_bin();
        let dot_bin_text = dot_bin.to_string_lossy();
        let fields = record::RecordFields {
            origin: &inputs.origin,
            identity: &inputs.identity,
            branch: &inputs.branch,
            backup: backup_text.as_ref(),
            git_dir: None,
            commit: Some(commit.as_str()),
            nonce: Some(nonce.as_str()),
            git_dev: Some("-"),
            git_ino: Some("-"),
            dot_bin: dot_bin_text.as_ref(),
            home: &self.home,
            source_root: self.source_root,
        };
        let mut cache = temp::MoveCache::default();
        if record::write_record(&stage.join("record"), "prepared", &fields, &mut cache).is_err() {
            return carried(out_stderr);
        }
        if !transaction::publish_transaction(self.source_root, &stage, &transaction, &mut cache) {
            return carried(out_stderr);
        }
        let _ = remove_forced(&candidate);
        let record = transaction.join("record");
        let journal = match record::read_record(&record, &self.home) {
            Ok(journal) => journal,
            Err(_) => return carried(out_stderr),
        };
        match self.resume(&transaction, &record, &journal) {
            Ok(()) => InitReport {
                stdout: out_stdout,
                stderr: out_stderr,
                code: 0,
            },
            Err(_) => carried(out_stderr),
        }
    }
}

/// `git clone --quiet --no-checkout --branch <branch>
/// --single-branch -- <origin> <candidate>`: the shell's candidate
/// checkout. A quiet clone prints nothing on success; its failure
/// diagnostics reach the command's own stdout and stderr on the
/// shell, so they are captured here and merged into the report
/// streams by the caller (never dropped, never bypassed).
fn git_clone(origin: &str, branch: &str, candidate: &Path) -> CloneReport {
    match identity::host_git_command()
        .arg("clone")
        .arg("--quiet")
        .arg("--no-checkout")
        .arg("--branch")
        .arg(branch)
        .arg("--single-branch")
        .arg("--")
        .arg(origin)
        .arg(candidate)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => CloneReport {
            stdout: output.stdout,
            stderr: output.stderr,
            cloned: true,
        },
        Ok(output) => CloneReport {
            stdout: output.stdout,
            stderr: output.stderr,
            cloned: false,
        },
        Err(_) => CloneReport {
            stdout: Vec::new(),
            stderr: Vec::new(),
            cloned: false,
        },
    }
}

/// Captured candidate-clone streams plus the verdict.
struct CloneReport {
    /// Clone's own stdout (empty under `--quiet`, kept for parity).
    stdout: Vec<u8>,
    /// Clone's own stderr (the failure diagnostics).
    stderr: Vec<u8>,
    /// Whether the checkout landed.
    cloned: bool,
}

/// `git -C <candidate> rev-parse <branch>^{commit}` with command
/// substitution chomping: the locked commit, or `None` when git
/// cannot report it.
fn git_rev_parse(candidate: &Path, branch: &str) -> Option<String> {
    let output = identity::host_git_command()
        .arg("-C")
        .arg(candidate)
        .arg("rev-parse")
        .arg(format!("{branch}^{{commit}}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = output.stdout;
    while text.last() == Some(&b'\n') {
        text.pop();
    }
    String::from_utf8(text).ok()
}

/// Whether `commit` is a well-formed object id: the shell's
/// `[[ $commit =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]]`, covering
/// SHA-1 and SHA-256 generations.
fn commit_valid(commit: &str) -> bool {
    (commit.len() == 40 || commit.len() == 64)
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `date +%Y%m%d%H%M%S` for the backup stamp: the same binary the
/// shell calls, so timezone and shape agree by construction.
fn backup_stamp() -> Option<String> {
    let output = Command::new("date")
        .arg("+%Y%m%d%H%M%S")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = output.stdout;
    while text.last() == Some(&b'\n') {
        text.pop();
    }
    String::from_utf8(text).ok()
}

/// The run nonce `"$(date +%s).$$.$RANDOM"`: epoch seconds from the
/// same `date` binary, the process id, and a `$RANDOM`-shaped
/// 0-32767 draw from `/dev/urandom` (wall-clock nanos when
/// urandom is unreadable, so the shape never fails).
fn fresh_nonce() -> Option<String> {
    let output = Command::new("date")
        .arg("+%s")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut secs = output.stdout;
    while secs.last() == Some(&b'\n') {
        secs.pop();
    }
    let secs = String::from_utf8(secs).ok()?;
    // Bounded two-byte read: `/dev/urandom` is endless, so a
    // whole-file read would block forever filling memory.
    let draw = std::fs::File::open("/dev/urandom")
        .and_then(|mut source| {
            use std::io::Read as _;
            let mut pair = [0u8; 2];
            source.read_exact(&mut pair).map(|()| pair)
        })
        .map(|pair| u16::from_ne_bytes(pair) as u64)
        .unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| u64::from(elapsed.subsec_nanos()) % 32768)
                .unwrap_or(0)
        });
    Some(format!("{}.{}.{}", secs, std::process::id(), draw % 32768))
}

/// Twin of the plan lane's confirm listing for the failure path:
/// `plan::confirm` returns its bytes only on success, but the shell
/// prints the header plus the first-field listing before refusing,
/// so the fresh runner re-renders exactly those bytes when confirm
/// fails. The cut rule, the two-space indent, and the GNU tail rule
/// mirror the port line for line; the conflicts-require-yes
/// differential row pins them.
fn confirm_listing(manifest: &Path) -> Vec<u8> {
    const HEADER: &[u8] = b"dot init: conflicting paths will be backed up:\n";
    let Ok(content) = std::fs::read(manifest) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&content);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    let bare_tail = cfg!(target_os = "macos") && !content.ends_with(b"\n");
    let mut out = HEADER.to_vec();
    for (index, line) in lines.iter().enumerate() {
        let first = match line.find('\t') {
            Some(position) => &line[..position],
            None => line,
        };
        out.extend_from_slice(b"  ");
        out.extend_from_slice(first.as_bytes());
        if index + 1 < lines.len() || !bare_tail {
            out.push(b'\n');
        }
    }
    out
}

#[cfg(test)]
mod parent_hook_tests {
    use std::io::Write as _;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::process::{Command, Stdio};

    use super::*;
    use dot_test_support::TempDir;

    #[test]
    fn publication_mask_enforces_the_startup_write_ceiling() {
        assert_eq!(publication_mask(0o002), 0o022);
        assert_eq!(publication_mask(0o077), 0o077);
    }

    fn journal(home: &Path) -> TransactionRecord {
        TransactionRecord {
            phase: "prepared".into(),
            origin: "https://example.invalid/dotfiles.git".into(),
            identity: "https://example.invalid/dotfiles".into(),
            branch: "main".into(),
            commit: "a".repeat(40),
            git_dir: home.join(".dotfiles").to_string_lossy().into_owned(),
            worktree: home.to_string_lossy().into_owned(),
            backup: "-".into(),
            dot: env!("CARGO_MANIFEST_DIR").to_owned() + "/bin/dot",
            dot_revision: "b".repeat(40),
            nonce: "parent-test".into(),
            git_dev: "-".into(),
            git_ino: "-".into(),
        }
    }

    fn with_parent_fixture(test: impl FnOnce(&Production<'_>, &TransactionRecord, &Path, &Path)) {
        let root = TempDir::new("engine-parent-hooks").expect("fixture root");
        let home = root.path().join("home");
        let state = root.path().join("state");
        let transaction = state.join("transaction");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&transaction).expect("transaction");
        let home_text = home.to_string_lossy().into_owned();
        let state_text = state.to_string_lossy().into_owned();
        let converge = || Ok(());
        let production = Production::new(
            EngineCtx {
                home: &home_text,
                xdg_state_home: &state_text,
                source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
                skip_provider: false,
                shdeps_update_policy: None,
                cwd: &home,
            },
            &converge,
        );
        let record = journal(&home);
        test(&production, &record, &home, &transaction);
    }

    fn ensure(
        production: &Production<'_>,
        record: &TransactionRecord,
        transaction: &Path,
        path: &[u8],
    ) -> Result<()> {
        let hooks = production.parent_hooks(record);
        let mut cache = temp::MoveCache::default();
        parent::parent_directories(
            &hooks,
            transaction,
            path,
            &production.home,
            &record.nonce,
            &mut cache,
        )
    }

    fn hash(bytes: &[u8]) -> String {
        let mut child = Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn git hash-object");
        child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().expect("hash output");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn stage_rel(parent: &str, nonce: &str) -> String {
        let directory = parent
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        let leaf = format!(".dot-init-parent.{nonce}.{}", hash(parent.as_bytes()));
        if directory.is_empty() {
            leaf
        } else {
            format!("{directory}/{leaf}")
        }
    }

    fn intent(transaction: &Path, parent: &str, line: &str) -> PathBuf {
        let path = transaction.join(format!("parent-intent.{}", hash(parent.as_bytes())));
        std::fs::write(&path, line).expect("intent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn production_parent_hooks_publish_nested_parents_and_rerun() {
        with_parent_fixture(|production, record, home, transaction| {
            ensure(production, record, transaction, b"a/b/file").expect("first publish");
            ensure(production, record, transaction, b"a/b/file").expect("stable rerun");
            assert!(home.join("a").is_dir());
            assert!(home.join("a/b").is_dir());
        });
    }

    #[test]
    fn production_parent_hooks_refuse_file_and_symlink_occupants() {
        with_parent_fixture(|production, record, home, transaction| {
            std::fs::write(home.join("file"), b"occupied").expect("file occupant");
            assert!(ensure(production, record, transaction, b"file/child").is_err());

            symlink("missing", home.join("link")).expect("dangling link");
            assert!(ensure(production, record, transaction, b"link/child").is_err());
        });
    }

    #[test]
    fn parent_top_level_is_noop() {
        with_parent_fixture(|p, r, home, tx| {
            ensure(p, r, tx, b"file").unwrap();
            assert!(std::fs::read_dir(home).unwrap().next().is_none());
        });
    }

    #[test]
    fn parent_empty_path_is_noop() {
        with_parent_fixture(|p, r, home, tx| {
            ensure(p, r, tx, b"").unwrap();
            assert!(std::fs::read_dir(home).unwrap().next().is_none());
        });
    }

    #[test]
    fn parent_root_path_is_noop() {
        with_parent_fixture(|p, r, home, tx| {
            ensure(p, r, tx, b"/").unwrap();
            assert!(std::fs::read_dir(home).unwrap().next().is_none());
        });
    }

    #[test]
    fn parent_single_missing_level_is_published() {
        with_parent_fixture(|p, r, home, tx| {
            ensure(p, r, tx, b"a/file").unwrap();
            assert!(home.join("a").is_dir());
        });
    }

    #[test]
    fn parent_trailing_slash_publishes_named_directory() {
        with_parent_fixture(|p, r, home, tx| {
            ensure(p, r, tx, b"a/").unwrap();
            assert!(home.join("a").is_dir());
        });
    }

    #[test]
    fn parent_existing_directory_is_preserved() {
        with_parent_fixture(|p, r, home, tx| {
            std::fs::create_dir(home.join("a")).unwrap();
            std::fs::set_permissions(home.join("a"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            ensure(p, r, tx, b"a/b/file").unwrap();
            assert_eq!(temp::file_mode(&home.join("a")).unwrap(), 0o755);
            assert!(home.join("a/b").is_dir());
        });
    }

    #[test]
    fn parent_directory_symlink_is_refused() {
        with_parent_fixture(|p, r, home, tx| {
            std::fs::create_dir(home.join("target")).unwrap();
            symlink("target", home.join("a")).unwrap();
            assert!(ensure(p, r, tx, b"a/file").is_err());
        });
    }

    #[test]
    fn parent_pending_record_resumes() {
        with_parent_fixture(|p, r, home, tx| {
            let stage = stage_rel("a", &r.nonce);
            intent(tx, "a", &format!("pending\ta\t{stage}\t-\t-\t-\n"));
            ensure(p, r, tx, b"a/file").unwrap();
            assert!(home.join("a").is_dir());
        });
    }

    #[test]
    fn parent_foreign_stage_is_refused() {
        with_parent_fixture(|p, r, home, tx| {
            std::fs::create_dir(home.join(stage_rel("a", &r.nonce))).unwrap();
            assert!(ensure(p, r, tx, b"a/file").is_err());
        });
    }

    #[test]
    fn parent_stale_record_is_refused() {
        with_parent_fixture(|p, r, _home, tx| {
            let stage = stage_rel("a", "another-run");
            intent(tx, "a", &format!("pending\ta\t{stage}\t-\t-\t-\n"));
            assert!(ensure(p, r, tx, b"a/file").is_err());
        });
    }

    #[test]
    fn parent_intent_directory_is_refused() {
        with_parent_fixture(|p, r, _home, tx| {
            std::fs::create_dir(tx.join(format!("parent-intent.{}", hash(b"a")))).unwrap();
            assert!(ensure(p, r, tx, b"a/file").is_err());
        });
    }

    #[test]
    fn parent_prepared_stage_resumes_and_removes_claim() {
        with_parent_fixture(|p, r, home, tx| {
            let relative = stage_rel("a", &r.nonce);
            let stage = home.join(&relative);
            std::fs::create_dir(&stage).unwrap();
            std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut cache = temp::MoveCache::default();
            entry::stage_claim_write(&stage, "parent", "a", &r.nonce, p.source_root, &mut cache)
                .unwrap();
            let metadata = std::fs::metadata(&stage).unwrap();
            intent(
                tx,
                "a",
                &format!(
                    "prepared\ta\t{relative}\t{}\t{}\t{:o}\n",
                    metadata.dev(),
                    metadata.ino(),
                    metadata.permissions().mode() & 0o7777
                ),
            );
            ensure(p, r, tx, b"a/file").unwrap();
            assert!(home.join("a").is_dir());
            assert!(!entry::stage_claim_file(&home.join("a")).exists());
        });
    }

    #[test]
    fn parent_mid_tree_file_is_refused_after_preserving_ancestor() {
        with_parent_fixture(|p, r, home, tx| {
            std::fs::create_dir(home.join("a")).unwrap();
            std::fs::write(home.join("a/b"), b"occupied").unwrap();
            assert!(ensure(p, r, tx, b"a/b/file").is_err());
            assert!(home.join("a").is_dir());
        });
    }
}
