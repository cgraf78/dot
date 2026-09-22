//! Repository configuration validation and repair.
//!
//! `is_worktree`, `effective_url`, and the origin-comparison logic
//! are owned by [`crate::overlays`] (reused here, not duplicated);
//! this module adds the remaining `config.sh` helpers
//! (`_repo_has_upstream`, `_overlay_origin_matches` shape,
//! `_ensure_repo_config`) on [`crate::repos_base::run_git`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Memoized `git config` reads by command prefix plus arguments.
/// `ensure_repo_config` runs before the pull phase and again in the
/// link phase; each run re-reads both keys against unchanging
/// state, so the second run shares the first answers. Only spawn
/// failures re-probe, so a transient failure never sticks; every
/// definitive answer pins (an empty read always triggers the repair
/// write below, which invalidates immediately).
static CONFIG_CACHE: OnceLock<Mutex<HashMap<Vec<u8>, String>>> = OnceLock::new();

fn config_cache() -> &'static Mutex<HashMap<Vec<u8>, String>> {
    CONFIG_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn config_cache_key(prefix: &[OsString], args: &[&str]) -> Vec<u8> {
    // The key omits the git program and process environment: both
    // are stable across the update run, and any future mid-run
    // change to either must invalidate beside the writers.
    let mut key = Vec::new();
    for arg in prefix {
        key.extend_from_slice(arg.as_os_str().as_bytes());
        key.push(0);
    }
    for arg in args {
        key.extend_from_slice(arg.as_bytes());
        key.push(0);
    }
    key
}

/// Drop every memoized config answer. Call after arbitrary user
/// code (hooks), git passthrough, provider runs, and staged clones
/// landing fresh checkouts. Fetch and rebase never write config,
/// so the pull phase deliberately does NOT invalidate: the
/// post-pull ensure is the read this cache exists to absorb.
/// Over-invalidation only costs a re-probe; a missed invalidation
/// would skip a needed config repair.
pub(crate) fn invalidate_config_cache() {
    if let Ok(mut cache) = config_cache().lock() {
        cache.clear();
    }
}

fn cached_config_read(prefix: &[OsString], args: &[&str]) -> Option<String> {
    // Match the uncached path under cancellation: the supervised
    // spawn observes the latched signal and fails, which reads as
    // missing. Serving a stale hit here would let teardown proceed
    // as if uninterrupted.
    if crate::cancellation::check().is_err() {
        return config_read_uncached(prefix, args);
    }
    let key = config_cache_key(prefix, args);
    if let Ok(cache) = config_cache().lock() {
        if let Some(hit) = cache.get(&key) {
            return Some(hit.clone());
        }
    }
    let answer = config_read_uncached(prefix, args)?;
    if let Ok(mut cache) = config_cache().lock() {
        cache.insert(key, answer.clone());
    }
    Some(answer)
}

fn config_read_uncached(prefix: &[OsString], args: &[&str]) -> Option<String> {
    let output = crate::repos_base::run_git(prefix, args)?;
    if !output.status.success() {
        return Some(String::new());
    }
    // Shell `$(...)` strips trailing newlines only.
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
    )
}

/// `_overlay_is_worktree` / `_overlay_effective_url`, owned by
/// [`crate::overlays`] and re-exported for config-track callers.
pub use crate::overlays::{effective_url, is_worktree};

/// `_repo_has_upstream`: `"$@" rev-parse --abbrev-ref
/// --symbolic-full-name '@{u}'`, true iff git exits 0 (stdout
/// ignored; both engines silence it). Shares the memoized raw
/// probe with fetch preparation: `Some` (even empty) is exactly
/// the exit-0 case.
pub fn has_upstream(prefix: &[OsString]) -> bool {
    crate::overlays::cached_upstream_raw(prefix).is_some()
}

/// `_overlay_origin_matches` adapter: `Ok(url)` reads `(true, url)`
/// and `Err(diagnostic)` reads `(false, diagnostic)`, exactly the
/// shell `REPLY`-plus-exit-status shape (`<missing>` when origin is
/// absent, the recorded URL on a single entry whether or not it
/// matches, `<multiple origin URLs>` when ambiguous).
pub fn origin_matches(path: &Path, expected: &str) -> (bool, String) {
    match crate::overlays::origin_matches(path, expected) {
        Ok(url) => (true, url),
        Err(diagnostic) => (false, diagnostic),
    }
}

/// `_ensure_repo_config`: always succeeds and prints nothing. A
/// `None` base (shell: missing topology, `_base_repo_exists`
/// false) does nothing. Otherwise each key is read first —
/// `config --bool core.fsmonitor` but plain `config
/// status.showUntrackedFiles` (the missing `--bool` on the second
/// key is shell-faithful, not an oversight) — a failed read counts
/// as `""` like the shell `$(... || true)`, and a value other than
/// the target is rewritten with `config <key> <target>`, ignoring
/// all errors like the shell `|| true`.
pub fn ensure_repo_config(base: Option<&[OsString]>) {
    let Some(prefix) = base else {
        return;
    };
    for (key, target, boolean) in [
        ("core.fsmonitor", "false", true),
        ("status.showUntrackedFiles", "no", false),
    ] {
        let args: &[&str] = if boolean {
            &["config", "--bool", key]
        } else {
            &["config", key]
        };
        let current = cached_config_read(prefix, args).unwrap_or_default();
        if current != target {
            let _ = crate::repos_base::run_git(prefix, &["config", key, target]);
            // The repair may have changed the pinned read; the next
            // ensure re-probes instead of trusting it.
            invalidate_config_cache();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    struct ConfigGit {
        _scope: dot_test_support::TempDir,
        log: std::path::PathBuf,
        shim: std::path::PathBuf,
    }

    impl ConfigGit {
        fn answering(tag: &str, boolean: &str, plain: &str) -> Self {
            let scope = dot_test_support::TempDir::new_exec(&format!("config-git-{tag}"))
                .expect("config git scope");
            let log = scope.path().join("invocations.log");
            let shim = scope.path().join("git");
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nif [ \"$3\" = config ]; then\n  if [ \"$4\" = --bool ]; then printf '%s\\n' \"{boolean}\";\n  elif [ -z \"$5\" ]; then printf '%s\\n' \"{plain}\";\n  fi\nfi\n",
                    log = log.display(),
                ),
            )
            .expect("config git shim");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
                .expect("config git mode");
            Self {
                _scope: scope,
                log,
                shim,
            }
        }

        fn compliant(tag: &str) -> Self {
            Self::answering(tag, "false", "no")
        }

        fn invocations(&self) -> usize {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .count()
        }

        fn writes(&self) -> usize {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter(|line| {
                    line.contains("core.fsmonitor false") || line.contains("showUntrackedFiles no")
                })
                .count()
        }

        fn prefix(&self, dir: &Path) -> Vec<OsString> {
            vec![OsString::from("-C"), dir.as_os_str().to_os_string()]
        }
    }

    #[test]
    fn repeated_ensure_repo_config_reads_once() {
        let _serial = TEST_SERIAL.lock();
        let git = ConfigGit::compliant("dedup");
        let repo = git._scope.path().join("repo");
        let prefix = git.prefix(&repo);
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            ensure_repo_config(Some(&prefix));
            ensure_repo_config(Some(&prefix));
        });
        assert_eq!(git.invocations(), 2);
        assert_eq!(git.writes(), 0);
    }

    #[test]
    fn ensure_repo_config_rewrites_mismatches_and_reprobes() {
        let _serial = TEST_SERIAL.lock();
        let git = ConfigGit::answering("rewrite", "true", "yes");
        let repo = git._scope.path().join("repo");
        let prefix = git.prefix(&repo);
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            ensure_repo_config(Some(&prefix));
            ensure_repo_config(Some(&prefix));
        });
        // Each ensure reads both keys (the write between invalidates
        // the pinned read) and rewrites both mismatches.
        assert_eq!(git.invocations(), 8);
        assert_eq!(git.writes(), 4);
    }

    #[test]
    fn config_invalidation_reprobes() {
        let _serial = TEST_SERIAL.lock();
        let git = ConfigGit::compliant("invalidate");
        let repo = git._scope.path().join("repo");
        let prefix = git.prefix(&repo);
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            ensure_repo_config(Some(&prefix));
            assert_eq!(git.invocations(), 2);
            invalidate_config_cache();
            ensure_repo_config(Some(&prefix));
        });
        assert_eq!(git.invocations(), 4);
    }

    #[test]
    fn config_reads_key_prefixes_separately() {
        let _serial = TEST_SERIAL.lock();
        let git = ConfigGit::compliant("keys");
        let first = git._scope.path().join("first");
        let second = git._scope.path().join("second");
        let first_prefix = git.prefix(&first);
        let second_prefix = git.prefix(&second);
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            ensure_repo_config(Some(&first_prefix));
            ensure_repo_config(Some(&second_prefix));
            ensure_repo_config(Some(&first_prefix));
            ensure_repo_config(Some(&second_prefix));
        });
        assert_eq!(git.invocations(), 4);
    }

    #[test]
    fn config_git_refusals_pin_empty() {
        let _serial = TEST_SERIAL.lock();
        let scope =
            dot_test_support::TempDir::new_exec("config-refuse").expect("refusing git scope");
        let log = scope.path().join("invocations.log");
        let shim = scope.path().join("git");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nexit 1\n",
                log = log.display(),
            ),
        )
        .expect("refusing git shim");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .expect("refusing git mode");
        let repo = scope.path().join("repo");
        let prefix = vec![OsString::from("-C"), repo.as_os_str().to_os_string()];
        crate::init_client_identity::with_host_git(shim.as_path(), || {
            assert_eq!(
                cached_config_read(&prefix, &["config", "--bool", "core.fsmonitor"]),
                Some(String::new())
            );
            assert_eq!(
                cached_config_read(&prefix, &["config", "--bool", "core.fsmonitor"]),
                Some(String::new())
            );
        });
        assert_eq!(
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn config_read_failures_stay_uncached() {
        let _serial = TEST_SERIAL.lock();
        let git = ConfigGit::compliant("no-poison");
        let repo = git._scope.path().join("repo");
        let prefix = git.prefix(&repo);
        let missing = git._scope.path().join("no-such-git");
        crate::init_client_identity::with_host_git(missing.as_path(), || {
            ensure_repo_config(Some(&prefix));
            ensure_repo_config(Some(&prefix));
        });
        crate::init_client_identity::with_host_git(git.shim.as_path(), || {
            ensure_repo_config(Some(&prefix));
        });
        // Nothing pinned the spawn failures: the working git still
        // reads both keys fresh.
        assert_eq!(git.invocations(), 2);
    }
}
