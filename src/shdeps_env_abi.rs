//! Shdeps provider environment and bounded ABI probe, part 3 of
//! `lib/dot/providers/shdeps.sh`.
//!
//! This family prepares the process the Shdeps bootstrap runs in and
//! probes the provider binary it selects: the caller-policy restore
//! (`_dot_shdeps_restore_caller_env`), the directory and flag setup
//! (`_dot_shdeps_configure_env`), the synchronous bounded runner
//! (`_dot_shdeps_run_bounded`), and the ABI probe plus its comparison
//! (`_dot_shdeps_binary_abi_version`, `_dot_shdeps_binary_abi`).
//! Part 1 (the lock reader and installer trust predicates) lives on
//! the unmerged `rust-port-slice-37` lane and part 2 (the re-exec
//! checkpoint record) on `rust-port-slice-40`; this module stacks
//! beside them once all land, which is why the ABI comparison takes
//! its expected value as a parameter instead of re-reading the lock.
//!
//! Later lanes own the remainder: installer selection
//! (`_dot_shdeps_development_checkout_valid`, `_dot_shdeps_installer`),
//! the bootstrap download (`_dot_shdeps_download_installer`), the
//! provider orchestration (`_ensure_shdeps`), and the re-exec itself
//! (`_dot_provider_maybe_reexec`, which ends in `exec` and needs an
//! interpreter decision this layer never makes).
//!
//! Engine boundaries: every shell `_warn` diagnostic folds into the
//! status or `None` refusal, like parts 1 and 2 folded theirs —
//! warnings are caller UI, the refusal is the contract. The bounded
//! runner supervises one direct child with standard pipes rather
//! than the shell's cleanup job table and process-group kill, so
//! rows pin leaf commands (no surviving descendants); the timeout
//! warning text stays unsaid and `124` is the contract. A spawn
//! failure reads `127`, like the shell's missing-command `$?`
//! (probing a directory would read `126` on the shell; rows pin
//! files). A signaled child maps to `128` plus the signal, like the
//! shell `$?`. The `DOT_FORCE` / `DOT_QUIET` flags accept decimal
//! spellings only — the shell's `-eq` also honors hex like `0x1`,
//! which stays unreproduced and unrowed. An ABI timeout that passes
//! the digit gate but overflows `u64` saturates instead of wrapping
//! like the shell's `intmax` arithmetic; rows pin realistic values.
//! The executable gate checks any exec bit rather than the full
//! owner/group/other `-x` matrix, which agrees on the same-user
//! fixtures both sides stage.

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;

/// Restored caller policy from [`restore_caller_env`]: `Some` means
/// the shell exports the value, `None` means it unsets the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredEnv {
    /// Value to export as `SHDEPS_FORCE`, or `None` to unset it.
    pub force: Option<String>,
    /// Value to export as `SHDEPS_LIB`, or `None` to unset it.
    pub lib: Option<String>,
}

/// `_dot_shdeps_restore_caller_env`: restore the genuine caller
/// `SHDEPS_FORCE` / `SHDEPS_LIB` policy saved at provider load, so a
/// later re-exec can tell caller values from configured ones. Each
/// `*_set` is the raw `_DOT_SHDEPS_CALLER_*_SET` marker (`"x"` when
/// the caller had the variable); any other marker unsets, like the
/// shell `== x` gate.
pub fn restore_caller_env(force_set: &str, force: &str, lib_set: &str, lib: &str) -> RestoredEnv {
    RestoredEnv {
        force: if force_set == "x" {
            Some(force.to_string())
        } else {
            None
        },
        lib: if lib_set == "x" {
            Some(lib.to_string())
        } else {
            None
        },
    }
}

/// Raw inputs for [`configure_env`], mirroring the exact environment
/// the shell reads: empty strings behave like unset for the
/// defaulted directories (`${VAR:-default}`), while the XDG and home
/// values arrive raw (empty when unset), like `checkpoint_path` in
/// part 2 takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigureInputs<'a> {
    /// Raw `$XDG_CONFIG_HOME` (empty when unset).
    pub xdg_config_home: &'a str,
    /// Raw `$HOME`.
    pub home: &'a str,
    /// Raw `$SHDEPS_INSTALL_DIR` (empty falls back to the default).
    pub install_dir: &'a str,
    /// Raw `$SHDEPS_BIN_DIR` (empty falls back to the default).
    pub bin_dir: &'a str,
    /// Raw `$SHDEPS_GIT_DEV_DIR` (empty falls back to the default).
    pub git_dev_dir: &'a str,
    /// Raw `$DOT_FORCE` (empty behaves like `"0"`).
    pub dot_force: &'a str,
    /// Raw `$DOT_QUIET` (empty behaves like `"0"`).
    pub dot_quiet: &'a str,
}

/// Configured provider environment from [`configure_env`]: each
/// field is the value the shell exports under the matching `SHDEPS_*`
/// name, except `force` / `quiet`, which report whether the shell
/// exports `SHDEPS_FORCE=1` / `SHDEPS_QUIET=1` (`false` leaves any
/// prior caller value alone — the shell only ever sets these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredEnv {
    /// Directory the shell exports as `SHDEPS_CONF_DIR`.
    pub conf_dir: String,
    /// Directory the shell exports as `SHDEPS_HOOKS_DIR`.
    pub hooks_dir: String,
    /// Directory the shell exports as `SHDEPS_INSTALL_DIR`.
    pub install_dir: String,
    /// Directory the shell exports as `SHDEPS_BIN_DIR`.
    pub bin_dir: String,
    /// Directory the shell exports as `SHDEPS_GIT_DEV_DIR`.
    pub git_dev_dir: String,
    /// Whether the shell exports `SHDEPS_FORCE=1`.
    pub force: bool,
    /// Whether the shell exports `SHDEPS_QUIET=1`.
    pub quiet: bool,
}

/// Whether `raw` enables a `DOT_FORCE` / `DOT_QUIET` style flag, like
/// the shell `[[ "${VAR:-0}" -eq 1 ]]`: decimal `1` (with optional
/// sign and surrounding whitespace, which the shell's arithmetic
/// tolerates) enables; everything else — including empty,
/// non-numeric, and hex spellings the shell arithmetic would honor —
/// refuses.
fn dot_flag(raw: &str) -> bool {
    match raw.trim().parse::<i128>() {
        Ok(value) => value == 1,
        Err(_) => false,
    }
}

/// `_dot_shdeps_configure_env`: resolve the provider directories and
/// flags, or `None` when the `dot_xdg_path config shdeps` root is
/// unresolvable, like the shell `return 1`. Success always reports
/// `Some`, including the ordinary non-force, non-quiet path the
/// shell pins with its trailing `return 0`.
pub fn configure_env(inputs: &ConfigureInputs<'_>) -> Option<ConfiguredEnv> {
    let conf_dir = crate::xdg::path(
        crate::xdg::Kind::Config,
        "shdeps",
        inputs.xdg_config_home,
        inputs.home,
    )
    .ok()?;
    let hooks_dir = format!("{conf_dir}/hooks.d");
    let install_dir = if inputs.install_dir.is_empty() {
        format!("{}/.local/share", inputs.home)
    } else {
        inputs.install_dir.to_string()
    };
    let bin_dir = if inputs.bin_dir.is_empty() {
        format!("{}/.local/bin", inputs.home)
    } else {
        inputs.bin_dir.to_string()
    };
    let git_dev_dir = if inputs.git_dev_dir.is_empty() {
        format!("{}/git", inputs.home)
    } else {
        inputs.git_dev_dir.to_string()
    };
    Some(ConfiguredEnv {
        conf_dir,
        hooks_dir,
        install_dir,
        bin_dir,
        git_dev_dir,
        force: dot_flag(inputs.dot_force),
        quiet: dot_flag(inputs.dot_quiet),
    })
}

/// Captured result of [`run_bounded`]: the shell prints the command's
/// stdout to its own stdout and reports the exit through its status,
/// so both travel here instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutcome {
    /// Exit status: the child code, `124` on timeout, `1` on
    /// supervisor failure, `2` on usage errors.
    pub status: i32,
    /// Captured child stdout bytes (empty unless the child ran).
    pub stdout: Vec<u8>,
}

/// Whether `raw` is a bounded-run timeout, like the shell
/// `^[1-9][0-9]*$` gate: a non-empty decimal run with no leading
/// zero. Overlong runs that pass the gate but overflow `u64`
/// saturate, as documented at the module top.
fn parse_timeout(raw: &str) -> Option<u64> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if !matches!(bytes[0], b'1'..=b'9') {
        return None;
    }
    if !bytes.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match raw.parse::<u64>() {
        Ok(value) => Some(value),
        Err(_) => Some(u64::MAX),
    }
}

/// Map a reaped exit status to the shell `$?` the recorder subshell
/// would have stored: the code itself, or `128` plus the fatal
/// signal when signaled.
fn status_code(status: std::process::ExitStatus) -> i32 {
    match status.code() {
        Some(code) => code,
        None => match status.signal() {
            Some(signal) => 128 + signal,
            None => 1,
        },
    }
}

/// `_dot_shdeps_run_bounded`: run `argv` with stdout captured,
/// stdin closed, and stderr inherited or discarded per
/// `stderr_mode` (`"inherit-stderr"` / `"discard-stderr"`), killing
/// the child past `timeout` seconds. `timeout` must pass the
/// `^[1-9][0-9]*$` gate, `label` must be non-empty (it only names
/// the shell's timeout warning, which this port folds into the
/// status), and `argv` must be non-empty — any violation, or an
/// unknown mode, reports `2` with empty output, like the shell.
pub fn run_bounded(
    timeout: &str,
    label: &str,
    stderr_mode: &str,
    argv: &[OsString],
) -> BoundedOutcome {
    let empty = BoundedOutcome {
        status: 2,
        stdout: Vec::new(),
    };
    let seconds = match parse_timeout(timeout) {
        Some(value) => value,
        None => return empty,
    };
    if label.is_empty() || argv.is_empty() {
        return empty;
    }
    let discard_stderr = match stderr_mode {
        "inherit-stderr" => false,
        "discard-stderr" => true,
        _ => return empty,
    };
    let mut command = std::process::Command::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    if discard_stderr {
        command.stderr(std::process::Stdio::null());
    } else {
        command.stderr(std::process::Stdio::inherit());
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            // The shell's recorder stores `127` for a missing
            // command; see the module docs for the directory caveat.
            return BoundedOutcome {
                status: 127,
                stdout: Vec::new(),
            };
        }
    };
    let taken = match child.stdout.take() {
        Some(taken) => taken,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return BoundedOutcome {
                status: 1,
                stdout: Vec::new(),
            };
        }
    };
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut pipe = taken;
        let _ = pipe.read_to_end(&mut output);
        output
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status_code(status);
                let stdout = reader.join().unwrap_or_default();
                return BoundedOutcome {
                    status: code,
                    stdout,
                };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The shell drops the partial capture on timeout;
                    // the join only reaps the drain thread.
                    let _ = reader.join();
                    return BoundedOutcome {
                        status: 124,
                        stdout: Vec::new(),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return BoundedOutcome {
                    status: 1,
                    stdout: Vec::new(),
                };
            }
        }
    }
}

/// Normalize the ABI probe timeout from the raw
/// `$_DOT_SHDEPS_ABI_TIMEOUT_SECONDS` value: a valid
/// `^[1-9][0-9]*$` run wins, anything else (including unset) falls
/// back to `10`, like the shell.
pub fn abi_timeout(raw: &str) -> u64 {
    parse_timeout(raw).unwrap_or(10)
}

/// Whether `binary` may be probed, like the shell
/// `[[ -n $binary && -x $binary ]]` gate: non-empty with any exec
/// bit set. The ownership-aware `-x` matrix is approximated as
/// documented at the module top.
fn binary_executable(binary: &Path) -> bool {
    if binary.as_os_str().is_empty() {
        return false;
    }
    match std::fs::metadata(binary) {
        Ok(meta) => meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// `_dot_shdeps_binary_abi_version`: probe `binary __api version`
/// under the `raw_timeout` bound with stderr discarded, returning
/// the probe text with command-substitution trailing newlines
/// stripped, or `None` for every shell refusal (empty or
/// non-executable binary, supervisor failure, nonzero exit), like
/// the shell clearing `REPLY` with exit 1.
pub fn abi_version(binary: &Path, raw_timeout: &str) -> Option<String> {
    if !binary_executable(binary) {
        return None;
    }
    let seconds = abi_timeout(raw_timeout);
    let argv = [
        OsString::from(binary),
        OsString::from("__api"),
        OsString::from("version"),
    ];
    // The shell reuses its bounded runner for the probe; this lane
    // owns that runner, so the call stays in-module.
    let outcome = run_bounded(
        &seconds.to_string(),
        "provider ABI probe",
        "discard-stderr",
        &argv,
    );
    if outcome.status != 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&outcome.stdout);
    Some(text.trim_end_matches('\n').to_string())
}

/// `_dot_shdeps_binary_abi`: whether the `binary` probe reports
/// exactly `abi:{expected}`. `expected` is the pinned value the
/// shell reads via part-1 `_dot_shdeps_lock_value abi`, passed in by
/// the caller so this lane stacks without duplicating that reader;
/// `$_SHDEPSW_BIN` arrives as `binary` and
/// `$_DOT_SHDEPS_ABI_TIMEOUT_SECONDS` as `raw_timeout`.
pub fn binary_abi(binary: &Path, expected_abi: &str, raw_timeout: &str) -> bool {
    match abi_version(binary, raw_timeout) {
        Some(reported) => reported == format!("abi:{expected_abi}"),
        None => false,
    }
}
