//! Hermetic Git for test fixtures.
//!
//! Fixture repositories (seeds, origins, clones, scripted commits) are
//! test setup, not behavior under test, so they must not depend on the
//! developer's Git environment. A bare `Command::new("git")` inherits
//! all of it: a global `commit.gpgSign` fails every fixture commit, a
//! host without a configured identity cannot commit at all, a global
//! ignore or attributes file (read from the XDG default even without a
//! global config) silently drops or rewrites fixture content, exported
//! `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` rules (`insteadOf`, a
//! hooks path) apply to every command, and a `GIT_DIR` exported by an
//! outer hook redirects fixture commands at the hook's repository. The
//! first `git` on PATH may also be a machine-local launcher shim.
//!
//! [`git`] and [`isolate_git`] own that boundary for every fixture.
//! Product code under test keeps its own Git environment: nothing here
//! applies to commands a test does not build through these helpers.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Author and committer name of fixture commits made without `-c user.name`.
pub const GIT_USER_NAME: &str = "fixture";
/// Author and committer email of fixture commits made without `-c user.email`.
pub const GIT_USER_EMAIL: &str = "fixture@example.invalid";

/// Command-scope config every fixture Git sees.
///
/// Injected through `GIT_CONFIG_COUNT` rather than `GIT_AUTHOR_*` or a
/// config file: Git applies `-c` after `GIT_CONFIG_COUNT`, so a call
/// site's own `-c user.name=...` still wins, and no file has to outlive
/// the test binary. With system and global config disabled, the
/// identity is the only value Git would otherwise lack; signing, hooks,
/// ignore, and attributes are pinned as well because their defaults
/// still reach outside the fixture (`core.excludesFile` and
/// `core.attributesFile` fall back to `$XDG_CONFIG_HOME/git/*`).
///
/// Command scope also outranks a fixture repository's own config, so a
/// repo-local value for one of these keys does not apply to fixture
/// commands (product code under test, which never sees this
/// environment, still reads it). A fixture that needs a different value
/// passes `-c` on its own command.
const CONFIG: &[(&str, &str)] = &[
    ("user.name", GIT_USER_NAME),
    ("user.email", GIT_USER_EMAIL),
    ("commit.gpgSign", "false"),
    ("tag.gpgSign", "false"),
    ("core.hooksPath", "/dev/null"),
    ("core.excludesFile", "/dev/null"),
    ("core.attributesFile", "/dev/null"),
    // Git 3.0 moves the built-in defaults to SHA-256 and reftable. The
    // engine and these fixtures assume 40-hex object names and loose or
    // packed refs, so pin today's formats instead of letting a Git
    // upgrade on the host change every fixture repository. The keys
    // exist since Git 2.47; older Git ignores them.
    ("init.defaultObjectFormat", "sha1"),
    ("init.defaultRefFormat", "files"),
];

/// Inherited variables that would select another repository, identity,
/// template, config source, or repository format for a fixture command.
/// Diagnostics such as `GIT_TRACE` pass through on purpose. The shell
/// acceptance harness (`tests/run`) unsets the same list.
///
/// `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` need no scrub: Git reads
/// only the first `GIT_CONFIG_COUNT` pairs, which [`isolate_git`]
/// replaces.
const INHERITED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
    "GIT_TEMPLATE_DIR",
    // Repository format and transport policy: a SHA-256 or reftable
    // default changes the fixture layout, and a protocol allowlist
    // without `file` (or `GIT_PROTOCOL_FROM_USER=0`) refuses local clones.
    "GIT_DEFAULT_HASH",
    "GIT_DEFAULT_REF_FORMAT",
    "GIT_ALLOW_PROTOCOL",
    "GIT_PROTOCOL_FROM_USER",
    // Pathspec and attribute modes that change what `add`/`checkout`
    // touch.
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_ATTR_SOURCE",
];

/// A fixture `git` command: the real Git binary ([`real_tool`]) under
/// [`isolate_git`].
///
/// Call sites add arguments and any test-specific environment after
/// this; a later `env_clear` would discard the isolation, so commands
/// that need a cleared environment call [`isolate_git`] after clearing.
pub fn git() -> Command {
    let mut command = Command::new(real_tool("git"));
    isolate_git(&mut command);
    command
}

/// Apply the fixture Git environment to `command`.
///
/// For commands built around another Git program (a fixture shim, a
/// resolved path) or with a cleared environment. Variables a call site
/// sets afterwards (for example a pinned `GIT_AUTHOR_DATE`) win.
pub fn isolate_git(command: &mut Command) -> &mut Command {
    for key in INHERITED {
        command.env_remove(key);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", CONFIG.len().to_string());
    for (index, (key, value)) in CONFIG.iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{index}"), key)
            .env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
    command
}

/// First `tool` on the test process PATH that is not a launcher shim.
///
/// Developer machines may put a launcher (`~/.local/bin/<tool>`) first
/// on PATH; it can read user state and route commands, and Dot's own
/// host-tool selection never picks it either. The launcher is excluded
/// by every known home: ambient `HOME` races with parallel tests that
/// mutate the process environment, so the passwd home is consulted too.
/// Missing the launcher once resolved a shim's real Git to a wrapper
/// script that hung a revision probe for the full marker bound.
pub fn real_tool(tool: &str) -> PathBuf {
    real_tool_in(tool, &std::env::var_os("PATH").expect("test PATH"))
}

/// [`real_tool`] over an explicit search `path`, for fixtures that must
/// resolve the same binary as an engine run under a fixture PATH.
pub fn real_tool_in(tool: &str, path: &OsStr) -> PathBuf {
    let launchers = launcher_paths(
        tool,
        std::env::var_os("HOME").as_deref(),
        passwd_home_dir().as_deref(),
    );
    select_real_tool(tool, path, &launchers)
}

/// Launcher-shim paths to exclude from tool resolution, one per known
/// home. Pure over its inputs so the passwd fallback is pinned with
/// synthetic homes instead of mutating process state.
pub fn launcher_paths(
    tool: &str,
    ambient_home: Option<&OsStr>,
    passwd_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut launchers = Vec::new();
    if let Some(home) = ambient_home {
        launchers.push(PathBuf::from(home).join(".local/bin").join(tool));
    }
    if let Some(home) = passwd_home {
        launchers.push(home.join(".local/bin").join(tool));
    }
    launchers
}

/// First PATH entry for `tool` that is a file and not an excluded
/// launcher shim.
pub fn select_real_tool(tool: &str, path: &OsStr, launchers: &[PathBuf]) -> PathBuf {
    std::env::split_paths(path)
        .map(|dir| dir.join(tool))
        .find(|candidate| {
            candidate.is_file() && !launchers.iter().any(|launcher| launcher == candidate)
        })
        .expect("real native tool")
}

/// The process owner's home directory from passwd, immune to process-wide
/// `HOME` mutation by parallel tests (writers serialize on their own
/// guard, but readers like [`real_tool`] do not take it).
///
/// Resolved once and cached, since the owner's home cannot change under a
/// running test binary: before `main` where the platform runs initializers
/// ([`RESOLVE_PASSWD_HOME`]), otherwise on the first call. A failed lookup
/// stays cached as absent instead of retrying, because a retry would put
/// the lookup back into the parallel phase the initializer keeps it out
/// of; ambient `HOME` still excludes its own launcher then.
fn passwd_home_dir() -> Option<PathBuf> {
    // Take the initializer's address so the linker keeps its object; plain
    // `used` lets the macOS linker drop it from test binaries.
    std::hint::black_box(&RESOLVE_PASSWD_HOME);
    PASSWD_HOME.get_or_init(passwd_home_dir_uncached).clone()
}

/// Whether the passwd home was resolved before `main`, outside the
/// parallel phase. Lets a test prove the linker kept
/// [`RESOLVE_PASSWD_HOME`] in a binary that depends on this crate, where a
/// dropped initializer would silently move the lookup back into it.
pub fn passwd_home_resolved_before_main() -> bool {
    std::hint::black_box(&RESOLVE_PASSWD_HOME);
    RESOLVED_BEFORE_MAIN.load(std::sync::atomic::Ordering::Acquire)
}

static PASSWD_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
static RESOLVED_BEFORE_MAIN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "C" fn resolve_passwd_home() {
    let _ = PASSWD_HOME.set(passwd_home_dir_uncached());
    RESOLVED_BEFORE_MAIN.store(true, std::sync::atomic::Ordering::Release);
}

// Look up the passwd home while the test binary is still single-threaded.
// The engine forks every owned child for its pre-exec launch handshake, and
// on macOS `backup_dir`'s root `mkdir` child died before that handshake
// ("failed to fill whole buffer") within milliseconds of the binary
// starting, when parallel tests make their first, still uncached `git()`
// lookup. Before the handshake the child runs nothing that can die, so it
// died inside libSystem's own fork handling; `getpwuid_r` (libinfo talking
// to opendirectoryd) was the libSystem machinery other threads could be
// inside at that moment, and forking during it is the suspected trigger.
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    unsafe(link_section = ".init_array")
)]
static RESOLVE_PASSWD_HOME: extern "C" fn() = resolve_passwd_home;

fn passwd_home_dir_uncached() -> Option<PathBuf> {
    // SAFETY: getpwuid_r writes only the local entry and buffer; the
    // status, result pointer, and directory pointer are all checked
    // before the directory is read, and the buffer outlives the read.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut capacity = 8192usize;
        loop {
            let mut buffer = vec![0u8; capacity];
            let status = libc::getpwuid_r(
                libc::getuid(),
                &mut entry,
                buffer.as_mut_ptr() as *mut libc::c_char,
                buffer.len(),
                &mut result,
            );
            if status == 0 {
                if result.is_null() || entry.pw_dir.is_null() {
                    return None;
                }
                let dir = std::ffi::CStr::from_ptr(entry.pw_dir);
                return Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())));
            }
            if status != libc::ERANGE || capacity >= 1 << 20 {
                return None;
            }
            capacity *= 2;
        }
    }
}
