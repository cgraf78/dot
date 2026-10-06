//! Hermetic driver for the public hook runtime under
//! `lib/dot/public/hook-runtime-v1`.
//!
//! The shell runtime is the live implementation behind the `dot_*` hook API
//! (`hook-api-v1.tsv`): `worker.sh` sources these libraries, under
//! `set -euo pipefail`, before a merge, pre-sync, or deactivate hook runs.
//! Tests source the same merge-mode libraries in the same order and run a
//! script body against them, so every assertion pins the code user hooks
//! actually call. The worker's earlier steps stay out: the overlay-protocol
//! helpers from `repos/` and their unset pass, its `umask 077`, and its
//! shell-option resets. Cases that depend on those (such as symlinked
//! extension files, which need the overlay helpers) belong in end-to-end
//! hook tests.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Libraries `worker.sh` sources for merge-family hooks, in its order.
const LIBRARIES: &[&str] = &[
    "xdg.sh",
    "hook-runtime-v1/log.sh",
    "hook-runtime-v1/temp.sh",
    "hook-runtime-v1/merge-block.sh",
    "hook-runtime-v1/families.sh",
    "hook-runtime-v1/merge-hooks.sh",
    "hook-runtime-v1/hook-api.sh",
];

/// Shell helpers available to every script: `rc CMD...` prints the status
/// of one command without tripping `set -e`.
const HELPERS: &str = "rc() { if \"$@\"; then printf '0\\n'; else printf '%s\\n' \"$?\"; fi; }\n";

/// One runtime invocation under construction.
pub struct Runtime {
    command: Command,
}

impl Runtime {
    /// Run `script` with the runtime sourced, `home` as `HOME` and working
    /// directory, the caller's `PATH`, the C locale, and `DOT_TEST=1` (the
    /// outer-lock gate generation capture requires; tests drop it to pin
    /// the refusal). Positional arguments follow via [`Runtime::arg`].
    pub fn new(home: &Path, script: &str) -> Self {
        let mut body = String::from("set -euo pipefail\n");
        for library in LIBRARIES {
            body.push_str(&format!(
                ". \"$DOT_SOURCE_ROOT/lib/dot/public/{library}\"\n"
            ));
        }
        body.push_str(HELPERS);
        body.push_str(script);
        let mut command = Command::new(dot_test_support::bash());
        command
            .args(["--noprofile", "--norc", "-c"])
            .arg(body)
            .arg("dot-hook-runtime")
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("DOT_SOURCE_ROOT", env!("CARGO_MANIFEST_DIR"))
            .env("DOT_TEST", "1")
            .env("LC_ALL", "C")
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self { command }
    }

    /// Append one positional argument (`$1`, `$2`, ...).
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    /// Append positional arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    /// Set one environment variable for the runtime.
    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.command.env(key, value);
        self
    }

    /// Remove one environment variable the constructor set.
    #[allow(dead_code)] // Only the generation suite drops the lock gate.
    pub fn env_remove(mut self, key: &str) -> Self {
        self.command.env_remove(key);
        self
    }

    /// Run to completion and return the raw process output.
    pub fn output(mut self) -> Output {
        self.command.output().expect("run the public hook runtime")
    }

    /// Run to completion, require success with empty stderr, and return
    /// stdout.
    pub fn stdout(self) -> Vec<u8> {
        let output = self.output();
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "hook runtime failed: {:?}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
}
