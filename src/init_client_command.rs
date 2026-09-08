//! The `dot init` command dispatcher, `dot_init_command` in
//! `lib/dot/init-client.sh` (lines 1789-1870): argument parsing, the
//! `--status` / `--rollback` modes, the `DOT_INIT_SKIP_PROVIDER`
//! gate, identity/branch resolution, and the transaction
//! recover/resume sequencing.
//!
//! The shell file holds 79 functions — too big for one lane — so
//! this module owns only the command entry point through the
//! live-transaction resume (`return 0` at line 1870). Everything
//! from the completed-file branch on (lines 1872+) arrives through
//! the [`CommandEngine::fresh`] continuation, owned by a later
//! slice: the shell has no stopping point there, so the engine
//! carries the resolved inputs forward instead of re-deciding them.
//!
//! Lane map, so the integrator can stack without overlap: the usage
//! text and status report live on `rust-port-slice-73`
//! ([`crate::init_client_adopt`]), the repository identity and
//! branch validation on `rust-port-slice-41`
//! ([`crate::init_client_identity`]), the transaction-directory
//! lifecycle on `rust-port-slice-35`
//! ([`crate::init_client_transaction`]), and the transaction record
//! journal on `rust-port-slice-54`
//! ([`crate::init_client_record`]). The resume and rollback
//! orchestrators ([`crate::init_client_resume`],
//! [`crate::init_client_rollback`]) and the remote default-branch
//! probe stay behind the engine closures — the former need dep
//! trees the integrator builds, the latter needs the network — and
//! the file-generic `_dot_init_error` diagnostic stays unported (a
//! bare `printf ... >&2; return 1` with no family state, absorbed
//! into [`InitReport`] the way earlier slices absorb engine
//! diagnostics).
//!
//! The port stays MSRV-clean (Rust 1.85): no let-chains, no
//! `Command::envs`.
//!
//! Engine boundary: the shell reads the run identity from the
//! `DOT_INIT_*` globals, the client root from `HOME`, the state
//! root from `XDG_STATE_HOME`, and the provider skip from
//! `DOT_INIT_SKIP_PROVIDER`. Library code must not read process
//! environment behind the engine, so those cross here as explicit
//! parameters ([`CommandEnv`]); `REPLY`-carried outputs surface as
//! return values, and every rendered report returns its bytes for
//! the caller to emit, keeping this module free of ambient file
//! descriptors.
//!
//! Byte-fidelity boundary: argument parsing matches on raw bytes
//! like the shell `case`, so non-UTF8 words diagnose with their
//! original octets (never U+FFFD replacements). Values crossing
//! into the `&str` ports narrow explicitly: a non-UTF8 origin can
//! never normalize, so it reports `unsupported repository URL`
//! (the candidate lane precedent for failing closed), and a
//! non-UTF8 `--branch` value reports `invalid branch`.
//!
//! Exit-code boundary: codes mirror the production process, which
//! runs under `set -euo pipefail` (`bin/dot`, `lib/dot/main.sh`),
//! not the bare function. An unknown option is code `1`: the
//! shell's `_dot_init_error` exits before its trailing `return 2`
//! ever runs (pinned against `bin/dot`). Every other site's code
//! coincides under both settings, and the differential tests run
//! the oracle in engine mode (`set -euo pipefail` around the call,
//! the resume/rollback lane precedent) so the harness agrees with
//! production too.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::init_client_adopt as adopt;
use crate::init_client_identity as identity;
use crate::init_client_record::{self as record, TransactionRecord};
use crate::init_client_transaction as transaction;

/// `dot init` run modes: the shell's `mode` local (`run` default,
/// `--status`, `--rollback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitMode {
    /// A real run: resolve identity/branch, then recover/resume or
    /// continue through [`CommandEngine::fresh`].
    Run,
    /// `--status`: report the durable initialization state.
    Status,
    /// `--rollback`: undo a published transaction before it commits.
    Rollback,
}

/// Parsed `dot init` arguments: the shell's `branch`, `yes`, `mode`,
/// and `origin` locals. Empty `origin`/`branch` spell absence,
/// exactly like the shell's empty-string defaults (a positional
/// empty word is absorbed, never an origin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInit {
    /// Requested mode.
    pub mode: InitMode,
    /// Requested repository URL (empty when absent).
    pub origin: Vec<u8>,
    /// Requested branch (empty when absent: resolve the default).
    pub branch: Vec<u8>,
    /// `--yes` was given (skip confirmation in the fresh tail).
    pub yes: bool,
}

/// How [`parse`] finished: immediate `--help`, parsed arguments, or
/// a ready-to-emit failure report. `--help`/`-h` anywhere wins
/// immediately, like the shell's in-loop `return 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Print [`adopt::usage`] to stdout, exit `0`.
    Help,
    /// Parsed arguments for [`run`].
    Args(ParsedInit),
    /// Argument failure (unknown option is code `1` with a
    /// diagnostic; arity failures are a silent code `2`).
    Failure(InitReport),
}

/// Rendered `dot_init_command` result: the stdout report, the stderr
/// diagnostic (empty unless the shell prints), and the shell exit
/// code (`0` success, `1` error, `2` usage). Mirrors the
/// [`adopt::StatusReport`] shape so the dispatcher treats both
/// uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    /// Standard-output report bytes.
    pub stdout: Vec<u8>,
    /// Standard-error diagnostic bytes.
    pub stderr: Vec<u8>,
    /// Process exit code.
    pub code: i32,
}

/// Explicit process inputs for [`run`]: the shell's `HOME`,
/// `XDG_STATE_HOME`, `DOT_INIT_SKIP_PROVIDER`, and source checkout.
pub struct CommandEnv<'a> {
    /// Client root (`HOME`).
    pub home: &'a str,
    /// State root override (`XDG_STATE_HOME`, empty counts as
    /// unset, like the shell).
    pub xdg_state_home: &'a str,
    /// Provider skip (`DOT_INIT_SKIP_PROVIDER`; `None` is unset,
    /// which the shell defaults to `0`).
    pub skip_provider: Option<&'a str>,
    /// Source checkout (`DOT_SOURCE_ROOT`): feeds stage-ownership
    /// recovery, like every other content-hash caller.
    pub source_root: &'a Path,
}

/// Fresh-init continuation inputs (the line-1872+ tail): everything
/// the later slice needs without re-deriving it — the requested
/// origin, its canonical identity, the resolved branch, and the
/// confirmation bypass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshInputs {
    /// Requested repository URL.
    pub origin: String,
    /// Canonical repository identity.
    pub identity: String,
    /// Resolved branch (explicit or remote default).
    pub branch: String,
    /// `--yes` was given.
    pub yes: bool,
}

/// Cross-lane engine for [`run`]: one closure per out-of-scope call
/// the ported range makes. Tests feed either stubs or closures
/// running the live shell functions; production binds the ported
/// modules (deep dep trees arrive with the integrator).
pub struct CommandEngine<'a> {
    /// `_dot_init_remote_default_branch`: resolve the remote's
    /// default branch. `None` is any failure, the shell's silent
    /// `return 1`. Injected (not called directly) because it clones
    /// from the network.
    pub remote_default_branch: &'a dyn Fn(&str) -> Option<String>,
    /// `_dot_init_resume_transaction`: replay the live transaction
    /// forward. `Err(Error::Usage)` carries an explicit diagnostic;
    /// any other error renders the shell's fixed resume-failure
    /// text.
    pub resume: &'a dyn Fn(&Path, &Path, &TransactionRecord) -> Result<(), Error>,
    /// `_dot_init_rollback`: roll back the recoverable transaction.
    /// `Err(Error::Usage)` carries the shell's diagnostic text
    /// verbatim; any other error stays silent with code `1`, like
    /// the shell's failed restore/removal sites.
    pub rollback: &'a dyn Fn(&Path) -> Result<(), Error>,
    /// The line-1872+ tail (completed-file branch, adoption, fresh
    /// candidate build): runs with the resolved inputs and returns
    /// its own report.
    pub fresh: &'a dyn Fn(&FreshInputs) -> InitReport,
}

/// Parse `dot init` arguments exactly like the shell loop:
/// `--help`/`-h` short-circuit, `--yes` sets the bypass, `--branch`
/// consumes the next word verbatim (even a flag-looking one),
/// `--status`/`--rollback` set the mode, `--*` diagnoses, and the
/// first positional binds the origin while a second is a silent
/// usage error. An arity-starved `--branch` is a silent usage error
/// too, like the shell's `(($# >= 2)) || return 2`.
pub fn parse(argv: &[Vec<u8>]) -> ParseOutcome {
    let mut mode = InitMode::Run;
    let mut yes = false;
    let mut branch = Vec::new();
    let mut origin = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_slice();
        if arg == b"--help".as_slice() || arg == b"-h".as_slice() {
            return ParseOutcome::Help;
        } else if arg == b"--yes".as_slice() {
            yes = true;
            index += 1;
        } else if arg == b"--branch".as_slice() {
            if index + 1 >= argv.len() {
                return ParseOutcome::Failure(silent(2));
            }
            branch = argv[index + 1].clone();
            index += 2;
        } else if arg == b"--status".as_slice() {
            mode = InitMode::Status;
            index += 1;
        } else if arg == b"--rollback".as_slice() {
            mode = InitMode::Rollback;
            index += 1;
        } else if arg.starts_with(b"--") {
            // Production runs under `set -euo pipefail` (`bin/dot`,
            // `lib/dot/main.sh`), so the shell's `_dot_init_error`
            // exits the process with `1` before its trailing
            // `return 2` ever runs: an unknown option is code `1`,
            // pinned against `bin/dot` (not the unreachable `2`).
            let mut message = b"unknown option: ".to_vec();
            message.extend_from_slice(arg);
            return ParseOutcome::Failure(diagnostic(&message));
        } else if !origin.is_empty() {
            return ParseOutcome::Failure(silent(2));
        } else {
            origin = argv[index].clone();
            index += 1;
        }
    }
    ParseOutcome::Args(ParsedInit {
        mode,
        origin,
        branch,
        yes,
    })
}

/// Run `dot_init_command` over parsed arguments with the engine:
/// mode dispatch (before the provider gate, like the shell),
/// `DOT_INIT_SKIP_PROVIDER` validation, origin requirement,
/// identity/branch resolution, then transaction recovery with the
/// live-transaction resume. A transaction-free run continues
/// through [`CommandEngine::fresh`].
pub fn run(env: &CommandEnv<'_>, engine: &CommandEngine<'_>, argv: &[Vec<u8>]) -> InitReport {
    let parsed = match parse(argv) {
        ParseOutcome::Help => {
            return InitReport {
                stdout: adopt::usage(),
                stderr: Vec::new(),
                code: 0,
            };
        }
        ParseOutcome::Args(parsed) => parsed,
        ParseOutcome::Failure(report) => return report,
    };
    match parsed.mode {
        InitMode::Status => {
            if !parsed.origin.is_empty() || !parsed.branch.is_empty() {
                return silent(2);
            }
            return run_status(env);
        }
        InitMode::Rollback => {
            if !parsed.origin.is_empty() || !parsed.branch.is_empty() {
                return silent(2);
            }
            return run_rollback(engine, env.home);
        }
        InitMode::Run => {}
    }
    // The shell's `${DOT_INIT_SKIP_PROVIDER:-0}` defaults both the
    // unset and the empty variable to `0`: only `0` and `1` pass.
    let skip = env
        .skip_provider
        .filter(|value| !value.is_empty())
        .unwrap_or("0");
    match skip {
        "0" | "1" => {}
        _ => {
            return InitReport {
                stdout: Vec::new(),
                stderr: b"dot init: DOT_INIT_SKIP_PROVIDER must be 0 or 1\n".to_vec(),
                code: 2,
            };
        }
    }
    if parsed.origin.is_empty() {
        return InitReport {
            stdout: Vec::new(),
            stderr: adopt::usage(),
            code: 2,
        };
    }
    let origin = match String::from_utf8(parsed.origin) {
        Ok(origin) => origin,
        Err(error) => {
            let mut message = b"unsupported repository URL: ".to_vec();
            message.extend_from_slice(error.as_bytes());
            return diagnostic(&message);
        }
    };
    let identity = match identity::repo_identity(&origin) {
        Some(identity) => identity,
        None => {
            let mut message = b"unsupported repository URL: ".to_vec();
            message.extend_from_slice(origin.as_bytes());
            return diagnostic(&message);
        }
    };
    let branch = if parsed.branch.is_empty() {
        match (engine.remote_default_branch)(&origin) {
            Some(branch) => branch,
            None => {
                return diagnostic(b"could not resolve a non-empty remote default branch");
            }
        }
    } else {
        match String::from_utf8(parsed.branch) {
            Ok(branch) => branch,
            Err(error) => {
                let mut message = b"invalid branch: ".to_vec();
                message.extend_from_slice(error.as_bytes());
                return diagnostic(&message);
            }
        }
    };
    if !identity::branch_valid(&branch) {
        let mut message = b"invalid branch: ".to_vec();
        message.extend_from_slice(branch.as_bytes());
        return diagnostic(&message);
    }
    let transaction = match transaction::transaction_dir(env.home, env.xdg_state_home) {
        Ok(directory) => PathBuf::from(directory),
        Err(_) => return silent(1),
    };
    if !transaction::recover_transaction_stages(env.source_root, &transaction) {
        return silent(1);
    }
    if exists_lexical(&transaction) {
        let record = transaction.join("record");
        let journal = match record::read_record(&record, Path::new(env.home)) {
            Ok(journal) => journal,
            Err(_) => {
                let mut message = b"malformed initialization transaction: ".to_vec();
                message.extend_from_slice(transaction.as_os_str().as_bytes());
                return diagnostic(&message);
            }
        };
        if journal.identity != identity || journal.branch != branch {
            return diagnostic(b"existing transaction belongs to a different repository or branch");
        }
        match (engine.resume)(&transaction, &record, &journal) {
            Ok(()) => silent(0),
            Err(Error::Usage { message }) => diagnostic(message.as_bytes()),
            Err(_) => diagnostic(b"initialization transaction could not be resumed safely"),
        }
    } else {
        (engine.fresh)(&FreshInputs {
            origin,
            identity,
            branch,
            yes: parsed.yes,
        })
    }
}

/// `_dot_init_status` through the adopt lane: the status engine's
/// three derivations bind the direct ports (transaction directory,
/// completion file, record read projected onto the four printed
/// fields), so no new closure crosses for this mode.
fn run_status(env: &CommandEnv<'_>) -> InitReport {
    let transaction_dir = || {
        transaction::transaction_dir(env.home, env.xdg_state_home)
            .ok()
            .map(PathBuf::from)
    };
    let completed_file = || {
        transaction::completed_file(env.home, env.xdg_state_home)
            .ok()
            .map(PathBuf::from)
    };
    let home = Path::new(env.home);
    let read_record = |record: &Path| {
        record::read_record(record, home)
            .ok()
            .map(|journal| adopt::StatusRecord {
                phase: journal.phase,
                origin: journal.origin,
                branch: journal.branch,
                backup: journal.backup,
            })
    };
    let engine = adopt::StatusEngine {
        transaction_dir: &transaction_dir,
        completed_file: &completed_file,
        read_record: &read_record,
    };
    let report = adopt::status(&engine);
    InitReport {
        stdout: report.stdout,
        stderr: report.stderr,
        code: i32::from(report.code),
    }
}

/// `_dot_init_rollback` through the engine closure: usage errors
/// carry the shell diagnostic verbatim, anything else (a failed
/// restore or removal, like the shell's trailing `|| return 1`
/// sites) stays silent with code `1`.
fn run_rollback(engine: &CommandEngine<'_>, home: &str) -> InitReport {
    match (engine.rollback)(Path::new(home)) {
        Ok(()) => silent(0),
        Err(Error::Usage { message }) => diagnostic(message.as_bytes()),
        Err(_) => silent(1),
    }
}

/// Empty report with an exit code: the shell's bare `return 1` /
/// `return 2` sites (underivable state paths, arity failures).
fn silent(code: i32) -> InitReport {
    InitReport {
        stdout: Vec::new(),
        stderr: Vec::new(),
        code,
    }
}

/// `_dot_init_error` rendering: `dot init: {message}` on stderr,
/// exit `1`. Every shell call site in the ported range ends here
/// with code `1` under production errexit (the unknown-option
/// `return 2` is unreachable there; see [`parse`]).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn words(argv: &[&str]) -> Vec<Vec<u8>> {
        argv.iter().map(|word| word.as_bytes().to_vec()).collect()
    }

    fn args(parsed: ParseOutcome) -> ParsedInit {
        match parsed {
            ParseOutcome::Args(parsed) => parsed,
            ParseOutcome::Help | ParseOutcome::Failure(_) => {
                panic!("expected parsed args, got {parsed:?}")
            }
        }
    }

    #[test]
    fn empty_argv_is_a_bare_run() {
        assert_eq!(
            args(parse(&[])),
            ParsedInit {
                mode: InitMode::Run,
                origin: Vec::new(),
                branch: Vec::new(),
                yes: false,
            }
        );
    }

    #[test]
    fn flags_bind_positionally_like_shell() {
        let parsed = args(parse(&words(&["--yes", "--branch", "main", "origin"])));
        assert_eq!(parsed.mode, InitMode::Run);
        assert!(parsed.yes);
        assert_eq!(parsed.branch, b"main".to_vec());
        assert_eq!(parsed.origin, b"origin".to_vec());
    }

    #[test]
    fn help_wins_from_any_position() {
        for argv in [
            words(&["--help"]),
            words(&["-h"]),
            words(&["--help", "--yes", "foo"]),
            words(&["origin", "--help"]),
            words(&["--status", "-h", "extra"]),
        ] {
            assert_eq!(parse(&argv), ParseOutcome::Help, "argv: {argv:?}");
        }
    }

    #[test]
    fn branch_consumes_the_next_word_verbatim() {
        let parsed = args(parse(&words(&["--branch", "--yes", "origin"])));
        assert_eq!(parsed.branch, b"--yes".to_vec());
        assert_eq!(parsed.origin, b"origin".to_vec());
        assert!(!parsed.yes);
        let parsed = args(parse(&words(&["--branch", "a", "--branch", "b"])));
        assert_eq!(parsed.branch, b"b".to_vec());
    }

    #[test]
    fn modes_set_without_consuming_positionals() {
        let parsed = args(parse(&words(&["--status", "origin"])));
        assert_eq!(parsed.mode, InitMode::Status);
        assert_eq!(parsed.origin, b"origin".to_vec());
        let parsed = args(parse(&words(&["--rollback"])));
        assert_eq!(parsed.mode, InitMode::Rollback);
    }

    #[test]
    fn unknown_options_carry_the_spelling_at_code_one() {
        for option in ["--frobnicate", "--branch=x", "--", "--YES"] {
            match parse(&words(&[option])) {
                ParseOutcome::Failure(report) => {
                    assert_eq!(report.code, 1, "option: {option}");
                    assert!(report.stdout.is_empty(), "option: {option}");
                    assert_eq!(
                        report.stderr,
                        format!("dot init: unknown option: {option}\n").into_bytes(),
                        "option: {option}"
                    );
                }
                ParseOutcome::Help | ParseOutcome::Args(_) => {
                    panic!("expected failure for {option}");
                }
            }
        }
    }

    #[test]
    fn unknown_option_echoes_raw_bytes() {
        let raw = vec![0x66, 0x6F, 0xFF, 0x62];
        let mut option = b"--".to_vec();
        option.extend_from_slice(&raw);
        match parse(&[option.clone()]) {
            ParseOutcome::Failure(report) => {
                let mut expected = b"dot init: unknown option: --".to_vec();
                expected.extend_from_slice(&raw);
                expected.push(b'\n');
                assert_eq!(report.stderr, expected);
                assert_eq!(report.code, 1);
            }
            ParseOutcome::Help | ParseOutcome::Args(_) => {
                panic!("expected failure for {option:?}");
            }
        }
    }

    #[test]
    fn starved_branch_and_second_origins_are_silent_code_two() {
        for argv in [
            words(&["--branch"]),
            words(&["--yes", "--branch"]),
            words(&["a", "b"]),
            words(&["a", ""]),
        ] {
            match parse(&argv) {
                ParseOutcome::Failure(report) => {
                    assert_eq!(report, silent(2), "argv: {argv:?}");
                }
                ParseOutcome::Help | ParseOutcome::Args(_) => {
                    panic!("expected failure for {argv:?}");
                }
            }
        }
    }

    #[test]
    fn leading_empties_are_absent_not_origins() {
        let parsed = args(parse(&words(&["", "", "origin"])));
        assert_eq!(parsed.origin, b"origin".to_vec());
    }
}
