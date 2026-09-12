//! Platform, host, and executable predicates.
//!
//! Owns WSL detection (env vars or a
//! case-insensitive `microsoft` in the kernel osrelease), `uname -s`
//! platform names (`darwin` canonicalized to `macos`), short-hostname
//! detection, comma-spec matching with `!` exclusions, Termux's dual
//! `linux`+`android` identity, slash-vs-PATH tool lookup, and the
//! sudo escalation ladder. Lowercasing is ASCII-only, matching the
//! shell's `${var,,}` under the C locale the engine pins.
//!
//! The shell reports wrong arity with exit 2; Rust surfaces the same
//! split as [`Error`] so callers map to identical exit codes.

use std::path::Path;

/// Platform failure, mirroring the shell exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Wrong arity or malformed value (shell exit 2).
    Usage,
    /// Detection failed: `uname`/`hostname` unusable (shell exit 1).
    Unavailable,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Unavailable => 1,
        }
    }
}

/// Whether the host is WSL: either WSL env marker is non-empty (an
/// empty value counts as unset, like the shell's `-n` test) or the
/// kernel osrelease mentions `microsoft` case-insensitively
/// (`grep -qi`, so any line counts).
pub fn is_wsl(distro: &str, interop: &str, osrelease: Option<&str>) -> bool {
    if !distro.is_empty() || !interop.is_empty() {
        return true;
    }
    osrelease.is_some_and(|content| content.to_ascii_lowercase().contains("microsoft"))
}

/// Canonical platform name from a `uname -s` value: WSL wins outright,
/// otherwise ASCII-lowercase with `darwin` folded to `macos`.
pub fn platform_name(uname_s: &str, wsl: bool) -> String {
    if wsl {
        return "wsl".to_string();
    }
    let lowered = uname_s.to_ascii_lowercase();
    if lowered == "darwin" {
        "macos".to_string()
    } else {
        lowered
    }
}

/// Detect the live platform: WSL markers from the environment plus
/// `/proc/sys/kernel/osrelease` when readable, then `uname -s`.
pub fn detect_platform() -> Result<String, Error> {
    let distro = std::env::var("WSL_DISTRO_NAME").unwrap_or_default();
    let interop = std::env::var("WSL_INTEROP").unwrap_or_default();
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let mut command = std::process::Command::new("uname");
    command.arg("-s");
    let output = crate::cleanup::run_session_output(
        command,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Strict,
    )
    .map_err(|_| Error::Unavailable)?;
    if !output.status.success() {
        return Err(Error::Unavailable);
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(platform_name(
        raw.trim_end_matches(['\r', '\n']),
        is_wsl(&distro, &interop, osrelease.as_deref()),
    ))
}

/// Short hostname canonicalization: ASCII-lowercase, like the shell's
/// `${value,,}` under the C locale.
pub fn host_name(raw: &str) -> String {
    raw.to_ascii_lowercase()
}

/// Detect the live short hostname: `hostname -s`, falling back to
/// plain `hostname` exactly like the shell's `||` chain.
pub fn detect_host() -> Result<String, Error> {
    for args in [&["-s"][..], &[][..]] {
        // No let-chains: the crate MSRV is 1.85 and let-chains need
        // 1.88. Same for the other two sites like this one.
        let mut command = std::process::Command::new("hostname");
        command.args(args);
        let output = match crate::cleanup::run_session_output(
            command,
            None,
            crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
            crate::cleanup::LingerPolicy::Strict,
        ) {
            Ok(output) => output,
            Err(_) => continue,
        };
        if !output.status.success() {
            continue;
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        return Ok(host_name(raw.trim_end_matches(['\r', '\n'])));
    }
    Err(Error::Unavailable)
}

/// Match a comma spec against current values (`_dot_match_specs`).
///
/// An empty spec matches everything. Only the first line participates:
/// the shell splits with `read -a`, which stops at the first newline,
/// so everything from `\n` on is invisible. Empty items are skipped
/// (so a trailing comma changes nothing). Items starting with `!` are
/// exclusions checked FIRST — and, like inclusion, compared LITERALLY:
/// both right-hand sides sit inside double quotes
/// (`[[ $normalized == "!$current" ]]`), so an expansion there never
/// acts as a pattern even when a current value carries glob
/// metacharacters (only a bare `$var` would). A spec with no inclusion
/// items matches unless excluded; otherwise at least one inclusion
/// must equal a current value. `lowercase` lowercases each item
/// (ASCII, C-locale `${item,,}`) before comparing.
pub fn match_specs(spec: &str, lowercase: bool, currents: &[&str]) -> bool {
    // `read -a` consumes one line; later lines never become items.
    let spec = spec.split('\n').next().unwrap_or("");
    if spec.is_empty() {
        return true;
    }
    let fold = |item: &str| {
        if lowercase {
            item.to_ascii_lowercase()
        } else {
            item.to_string()
        }
    };
    let mut has_include = false;
    for item in spec.split(',') {
        if item.is_empty() {
            continue;
        }
        let normalized = fold(item);
        if !normalized.starts_with('!') {
            has_include = true;
        }
        // Exclusion: literal `!`-prefixed equality (the quotes make
        // even metachar values inert).
        for current in currents {
            if normalized
                .strip_prefix('!')
                .is_some_and(|tail| tail == *current)
            {
                return false;
            }
        }
    }
    if !has_include {
        return true;
    }
    for item in spec.split(',') {
        if item.is_empty() {
            continue;
        }
        let normalized = fold(item);
        // Inclusion: `[[ $item == "$current" ]]`, quoted, so literal.
        if currents.iter().any(|current| normalized == *current) {
            return true;
        }
    }
    false
}

/// Match a platform spec: exactly one spec string (anything else is a
/// usage error, shell exit 2). Termux keeps both durable identities —
/// the kernel platform plus `android` — matching the provider ABI.
pub fn platform_matches(spec: Option<&str>, platform: &str, termux: bool) -> Result<bool, Error> {
    let spec = spec.ok_or(Error::Usage)?;
    if termux {
        Ok(match_specs(spec, false, &[platform, "android"]))
    } else {
        Ok(match_specs(spec, false, &[platform]))
    }
}

/// Match a host spec: exactly one spec string, compared lowercased.
pub fn host_matches(spec: Option<&str>, host: &str) -> Result<bool, Error> {
    let spec = spec.ok_or(Error::Usage)?;
    Ok(match_specs(spec, true, &[host]))
}

/// Whether a command name resolves (`_dot_tool_present`).
///
/// Exactly one non-empty name (anything else is exit 2). A name
/// containing `/` is an existence probe (`[[ -e ]]`, following
/// symlinks); otherwise each colon-separated `path_dirs` entry is
/// searched the way the shell's `command -v` searches for external
/// commands: the first stat-able non-directory wins — executability
/// is NOT required (a `644` file on PATH satisfies `command -v`;
/// pinned live against bash). Shell builtins and functions also
/// satisfy `command -v`; that lookup is intentionally out of contract
/// — engine callers pass external tool names. Empty PATH entries are
/// skipped, matching `find_gum` convention.
pub fn tool_present(name: Option<&str>, path_dirs: &str) -> Result<bool, Error> {
    let name = match name {
        Some(name) if !name.is_empty() => name,
        _ => return Err(Error::Usage),
    };
    if name.contains('/') {
        return Ok(Path::new(name).exists());
    }
    Ok(path_dirs
        .split(':')
        .any(|dir| !dir.is_empty() && is_path_command(&Path::new(dir).join(name))))
}

/// `command -v` candidacy: stat-able (symlinks followed) and not a
/// directory. Permissions, fifos, and sockets pass exactly as in bash.
fn is_path_command(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| !meta.is_dir())
}

/// Sudo escalation decision table (`_require_sudo`): root passes, then
/// passwordless `sudo -n true`, then quiet mode fails closed, and only
/// then the interactive `sudo true` prompt (whose outcome the caller
/// supplies — the library never blocks on a tty in tests).
pub fn decide_sudo(
    euid_is_root: bool,
    nopass_ok: bool,
    quiet: bool,
    prompt_ok: &impl Fn() -> bool,
) -> bool {
    if euid_is_root {
        return true;
    }
    if nopass_ok {
        return true;
    }
    if quiet {
        return false;
    }
    prompt_ok()
}

/// Live `_require_sudo`: `id -u` for root (the shell forks `id`, so no
/// libc binding is needed for parity), then the [`decide_sudo`]
/// ladder with real `sudo` probes. `quiet` is the verbatim
/// `DOT_QUIET` value; only exactly `1` suppresses the prompt, like the
/// shell's `-eq 1`.
pub fn require_sudo(quiet: &str) -> bool {
    // TEMP-DIAG-180: remove with the recvmsg diag.
    #[cfg(test)]
    let sudo_started = std::time::Instant::now();
    #[cfg(test)]
    eprintln!(
        "TEMP-DIAG-180: sudo-probe id-start at {}ms",
        sudo_started.elapsed().as_millis()
    );
    let mut id = std::process::Command::new("id");
    id.arg("-u");
    let euid_output = crate::cleanup::run_session_output(
        id,
        None,
        crate::cleanup::COMMAND_CAPTURE_LIMIT_BYTES,
        crate::cleanup::LingerPolicy::Strict,
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    // Faithful to `[[ $(id -u) -eq 0 ]]`: bash arithmetic coerces empty
    // or non-numeric output to 0, so only an explicit nonzero uid
    // denies the root fast path (notably when PATH lacks `id`).
    let euid_is_root = euid_output
        .as_deref()
        .is_none_or(|text| !matches!(text.parse::<i64>(), Ok(uid) if uid != 0));
    // TEMP-DIAG-180: remove with the recvmsg diag.
    #[cfg(test)]
    eprintln!(
        "TEMP-DIAG-180: sudo-probe id-end at {}ms",
        sudo_started.elapsed().as_millis()
    );
    let probe = |extra: &[&str], interactive: bool| {
        // TEMP-DIAG-180: remove with the recvmsg diag.
        #[cfg(test)]
        eprintln!(
            "TEMP-DIAG-180: sudo-probe {}-start at {}ms",
            if interactive {
                "interactive"
            } else {
                "noninteractive"
            },
            sudo_started.elapsed().as_millis()
        );
        let mut command = std::process::Command::new("sudo");
        command.args(extra).arg("true");
        let accepted = if interactive {
            command
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
            crate::cleanup::run_foreground_status(command) == 0
        } else {
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::cleanup::run_session_status(command, crate::cleanup::LingerPolicy::Strict) == 0
        };
        // TEMP-DIAG-180: remove with the recvmsg diag.
        #[cfg(test)]
        eprintln!(
            "TEMP-DIAG-180: sudo-probe {}-end accepted={} at {}ms",
            if interactive {
                "interactive"
            } else {
                "noninteractive"
            },
            accepted,
            sudo_started.elapsed().as_millis()
        );
        accepted
    };
    decide_sudo(euid_is_root, probe(&["-n"], false), quiet == "1", &|| {
        probe(&[], true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::CommandExt as _;

    #[test]
    fn wsl_markers_and_osrelease() {
        assert!(is_wsl("Ubuntu", "", None));
        assert!(is_wsl("", "x", None));
        // Empty counts as unset.
        assert!(!is_wsl("", "", None));
        assert!(is_wsl("", "", Some("5.15.90.1-microsoft-standard-WSL2\n")));
        assert!(is_wsl("", "", Some("MICROSOFT x86_64")));
        assert!(!is_wsl("", "", Some("6.1.0-18-amd64\n")));
        assert!(!is_wsl("", "", None));
    }

    #[test]
    fn platform_names_fold_darwin() {
        assert_eq!(platform_name("Linux", false), "linux");
        assert_eq!(platform_name("Darwin", false), "macos");
        assert_eq!(platform_name("DARWIN", false), "macos");
        assert_eq!(platform_name("FreeBSD", false), "freebsd");
        assert_eq!(platform_name("Linux", true), "wsl");
        assert_eq!(platform_name("anything", true), "wsl");
    }

    #[test]
    fn spec_matrix() {
        // Empty spec matches everything, even with no currents.
        assert!(match_specs("", false, &[]));
        assert!(match_specs("", false, &["linux"]));
        // Inclusion.
        assert!(match_specs("linux", false, &["linux"]));
        assert!(!match_specs("linux", false, &["macos"]));
        assert!(match_specs("macos,linux", false, &["linux"]));
        // Trailing/empty items change nothing.
        assert!(match_specs("linux,", false, &["linux"]));
        assert!(match_specs(",linux", false, &["linux"]));
        // Only separators: every item is empty, so (like the shell's
        // `read -a` split) the spec behaves as if it had no items.
        assert!(match_specs(",", false, &["linux"]));
        // Exclusion-only specs match unless excluded.
        assert!(match_specs("!macos", false, &["linux"]));
        assert!(!match_specs("!linux", false, &["linux"]));
        // Exclusions win over inclusions.
        assert!(!match_specs("linux,!linux", false, &["linux"]));
        assert!(match_specs("linux,!macos", false, &["linux"]));
        // `!` alone excludes nothing and includes nothing.
        assert!(match_specs("!", false, &["!"]));
        // Only the first line is read; the rest is invisible.
        assert!(match_specs("linux\nevil", false, &["linux"]));
        assert!(!match_specs("nomatch\nlinux", false, &["linux"]));
        assert!(match_specs("\nlinux", false, &["linux"]));
        // Case modes.
        assert!(!match_specs("LINUX", false, &["linux"]));
        assert!(match_specs("LINUX", true, &["linux"]));
        assert!(!match_specs("!LINUX", true, &["linux"]));
        // No currents: inclusions fail, pure exclusions pass.
        assert!(!match_specs("linux", false, &[]));
        assert!(match_specs("!linux", false, &[]));
    }

    #[test]
    fn exclusion_values_are_literal() {
        // Both right-hand sides sit inside double quotes, so a current
        // value carrying glob metacharacters never acts as a pattern
        // (adversarial review caught this inverted).
        assert!(match_specs("!anything", false, &["*"]));
        assert!(!match_specs("!*", false, &["*"]));
        assert!(match_specs("!linux", false, &["lin*"]));
        assert!(!match_specs("!lin*", false, &["lin*"]));
        assert!(!match_specs("other", false, &["*"]));
    }

    #[test]
    fn inclusion_is_literal() {
        // The inclusion side is quoted in the shell: `*` never globs.
        assert!(!match_specs("*", false, &["linux"]));
        assert!(match_specs("*", false, &["*"]));
    }

    #[test]
    fn arity_errors() {
        assert_eq!(platform_matches(None, "linux", false), Err(Error::Usage));
        assert_eq!(host_matches(None, "h"), Err(Error::Usage));
        assert_eq!(tool_present(None, "/bin"), Err(Error::Usage));
        assert_eq!(tool_present(Some(""), "/bin"), Err(Error::Usage));
        assert_eq!(Error::Usage.code(), 2);
        assert_eq!(Error::Unavailable.code(), 1);
    }

    #[test]
    fn termux_adds_android_identity() {
        assert_eq!(platform_matches(Some("android"), "linux", true), Ok(true));
        assert_eq!(platform_matches(Some("android"), "linux", false), Ok(false));
        assert_eq!(platform_matches(Some("linux"), "linux", true), Ok(true));
    }

    #[test]
    fn sudo_ladder() {
        let yes = || true;
        let no = || false;
        assert!(decide_sudo(true, false, true, &no));
        assert!(decide_sudo(false, true, true, &no));
        assert!(!decide_sudo(false, false, true, &yes));
        assert!(decide_sudo(false, false, false, &yes));
        assert!(!decide_sudo(false, false, false, &no));
    }

    // TEMP-DIAG-180: remove with the recvmsg diag. Nonblocking drain
    // of the sudo helper's PTY master for failure triage.
    fn drain_pty_master(master: &std::os::fd::OwnedFd) -> Vec<u8> {
        use std::os::fd::AsRawFd as _;
        let master_fd = master.as_raw_fd();
        let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe {
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        let mut output = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = unsafe { libc::read(master_fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if count <= 0 || output.len() > 65536 {
                break;
            }
            output.extend_from_slice(&chunk[..count as usize]);
        }
        output
    }

    #[test]
    fn interactive_sudo_keeps_the_foreground_tty_and_resumes_for_cleanup() {
        const HELPER: &str = "DOT_SUDO_PTY_HELPER";
        if std::env::var_os(HELPER).is_some() {
            let signals = crate::cleanup::Signals::install().unwrap();
            let accepted = require_sudo("");
            let status = signals.finish(i32::from(!accepted));
            assert_eq!(status, 128 + libc::SIGTERM);
            assert!(
                std::path::Path::new(&std::env::var_os("DOT_TEST_SUDO_CLEANED").unwrap()).exists(),
                "stopped interactive sudo did not resume to run its TERM handler"
            );
            return;
        }

        let scope = dot_test_support::TempDir::new("sudo-foreground-pty").unwrap();
        let bin = scope.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let id = bin.join("id");
        std::fs::write(&id, "#!/bin/sh\nprintf '1000\\n'\n").unwrap();
        std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ready = scope.path().join("sudo.ready");
        let cleaned = scope.path().join("sudo.cleaned");
        let sudo = bin.join("sudo");
        std::fs::write(
            &sudo,
            "#!/bin/sh\nif [ \"${1:-}\" = -n ]; then exit 1; fi\n/usr/bin/python3 -c 'import os,sys; sys.exit(0 if all(os.isatty(fd) and os.tcgetpgrp(fd) == os.getpgrp() for fd in (0,1,2)) else 9)' || exit $?\ntrap ': >\"$DOT_TEST_SUDO_CLEANED\"; exit 0' TERM\n: >\"$DOT_TEST_SUDO_READY\"\nkill -STOP $$\nwhile :; do :; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors; null name/termios/
        // winsize pointers request defaults.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    // macOS takes *mut termios/*mut winsize while Linux takes
                    // *const; null_mut() satisfies both through coercion.
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: successful openpty returned uniquely owned descriptors.
        let master = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "platform::tests::interactive_sudo_keeps_the_foreground_tty_and_resumes_for_cleanup",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .env("PATH", &bin)
            .env("DOT_TEST_SUDO_READY", &ready)
            .env("DOT_TEST_SUDO_CLEANED", &cleaned)
            .stdin(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stdout(std::process::Stdio::from(slave.try_clone().unwrap()))
            .stderr(std::process::Stdio::from(slave));
        // SAFETY: the child is single-threaded after fork. These calls create
        // a fresh session, acquire fd 0's PTY as controlling terminal, and
        // place the child in its foreground process group before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0
                    // ioctl request is c_ulong on Linux/macOS but c_int on Android;
                    // the inferred cast matches each platform's declaration.
                    || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0
                    || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) < 0
                {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        // TEMP-DIAG-180: widen from 3s while diagnosing whether macOS CI
        // is slow (python startup under load) or stuck (probe never
        // returns). Revert or justify with the sudo-probe timestamps.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !ready.exists() && std::time::Instant::now() < ready_deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !ready.exists() {
            // TEMP-DIAG-180: remove with the recvmsg diag. Drain the
            // helper's PTY output so macOS CI shows whether the helper
            // panicked or sudo rejected the foreground check.
            eprintln!(
                "TEMP-DIAG-180: sudo-pty helper output: {:?}",
                String::from_utf8_lossy(&drain_pty_master(&master))
            );
        }
        assert!(
            ready.exists(),
            "interactive sudo did not observe its foreground PTY"
        );
        // SAFETY: the fixture owns the test subprocess and its signal handler.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                // TEMP-DIAG-180: remove with the recvmsg diag. The
                // interactive-end marker below shows whether require_sudo
                // returned (stuck after teardown) or never did (stuck in
                // teardown of the stopped sudo).
                eprintln!(
                    "TEMP-DIAG-180: sudo-pty helper output at stop timeout: {:?}",
                    String::from_utf8_lossy(&drain_pty_master(&master))
                );
                panic!("PTY helper did not stop");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(status.success(), "PTY helper failed with {status:?}");
        assert!(
            cleaned.exists(),
            "interactive sudo cleanup marker is absent"
        );
    }
}
