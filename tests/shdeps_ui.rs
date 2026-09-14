//! Differential parity tests for the shdeps-ui group record against
//! `lib/dot/providers/shdeps-ui.sh`: group labels, summary text, and
//! the remember/record/display state family. Every case runs the live
//! shell function and its Rust twin on identical inputs and compares
//! stdout bytes exactly.
//!
//! Only this family is sourced: the prompt, render, event, proc, and
//! FIFO lanes are still shell and stay out of the oracle. Each shell
//! child starts from the reset-established map declarations (see
//! [`SOURCES`]), matching a fresh `State::new()` on the Rust side.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::shdeps_ui::{State, group_label, summary_text};
use dot::test_support::TempDir;

/// Sources for the group-record chapter: the adapter plus
/// `progress-ui.sh` for the `_join_comma` the summary shares, then
/// the four `DOT_UI_SHDEPS_*` map declarations exactly as
/// `_shdeps_ui_reset` establishes them. The reset function itself is
/// a later lane (it also owns the prompt, jq-probe, and summary
/// globals), but the record family never runs without those
/// declarations in production (`_run_shdeps_update_ui` resets before
/// the event loop): without them bash auto-vivifies the first
/// subscript assignment as an indexed array, so a second distinct
/// group reads back index `0` as already seen and the order list
/// loses it. The Rust [`State`] always carries real maps, matching
/// the post-reset shell.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/progress-ui.sh\"\n",
    ". \"$1/lib/dot/providers/shdeps-ui.sh\"\n",
    "DOT_UI_SHDEPS_GROUP_ORDER=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_SEEN=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_LABELS=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_ITEMS=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_SUMMARIES=()\n",
);

/// Shared dump: discovery order, then per-group display label, item
/// blob, and summary blob. Blobs print raw between a prefix and an
/// `<end>` trailer so embedded tabs and newlines compare exactly.
const DUMP: &str = concat!(
    "for g in \"${DOT_UI_SHDEPS_GROUP_ORDER[@]}\"; do printf 'order:<%s>\\n' \"$g\"; done\n",
    "for g in \"${DOT_UI_SHDEPS_GROUP_ORDER[@]}\"; do\n",
    "  printf 'display:<%s>=' \"$g\"; _shdeps_display_label \"$g\"; printf '\\n'\n",
    "  printf 'items:<%s>:' \"$g\"; printf '%s' \"${DOT_UI_SHDEPS_GROUP_ITEMS[$g]}\"; printf '<end>\\n'\n",
    "  printf 'summary:<%s>:' \"$g\"; printf '%s' \"${DOT_UI_SHDEPS_GROUP_SUMMARIES[$g]}\"; printf '<end>\\n'\n",
    "done\n",
);

/// Run one shell snippet with the chapter sourced. `argv` arrives as
/// `$2..`; byte-exact stdout is the oracle.
fn shell_run(fixture: &Path, argv: &[&OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", fixture)
        .env("DOT_TEST", "1")
        .current_dir(fixture)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

#[test]
fn group_label_rows_agree() {
    let fixture = TempDir::new("shdeps-ui-label").expect("fixture dir");
    let rows: Vec<Vec<u8>> = vec![
        b"packages".to_vec(),
        b"github-releases".to_vec(),
        b"github-repos".to_vec(),
        b"cargo".to_vec(),
        b"go".to_vec(),
        b"uv".to_vec(),
        b"npm".to_vec(),
        b"custom".to_vec(),
        b"other".to_vec(),
        b"".to_vec(),
        // A group a newer shdeps emits that dot does not know: the
        // shell `*)` arm shows the key instead of `Other`.
        b"pip".to_vec(),
        b"my-group".to_vec(),
        // Case arms are exact: near-misses pass through verbatim.
        b"Packages".to_vec(),
        b"CARGO".to_vec(),
        "grüße".as_bytes().to_vec(),
        // Non-UTF-8 keys stay bytes on both sides.
        b"\xff-group".to_vec(),
    ];
    for group in &rows {
        let group_arg = OsStr::from_bytes(group);
        let (code, out, err) =
            shell_run(fixture.path(), &[group_arg], "_shdeps_group_label \"$2\"\n");
        assert_eq!(code, 0, "harness exit");
        assert!(err.is_empty(), "harness stderr");
        assert_eq!(out, group_label(group), "label for {group:?}");
    }
}

#[test]
fn summary_text_rows_agree() {
    let fixture = TempDir::new("shdeps-ui-summary").expect("fixture dir");
    // (changed, current, skipped, failed, warnings, omit-warnings-arg)
    // `omit` drops the fifth shell word so `${5:-0}` defaults it,
    // while Rust passes the same `0` explicitly.
    let rows: Vec<(i64, i64, i64, i64, i64, bool)> = vec![
        (0, 0, 0, 0, 0, false),
        (0, 0, 0, 0, 0, true),
        (2, 5, 1, 0, 0, false),
        (0, 0, 0, 3, 0, false),
        (0, 4, 0, 0, 2, false),
        (1, 2, 3, 4, 5, false),
        (0, 7, 0, 1, 0, false),
        (3, 0, 0, 0, 0, false),
        (0, 0, 2, 0, 0, false),
        // A lone warning still suppresses the `0 current` fallback.
        (0, 0, 0, 0, 1, false),
        // Current drops out whenever any other part renders.
        (0, 0, 5, 2, 0, false),
        // Degenerate negatives: no part renders, so the `0 current`
        // fallback carries the raw value, like the shell.
        (0, -5, 0, 0, 0, false),
        (0, 0, 0, -2, 0, false),
    ];
    for (changed, current, skipped, failed, warnings, omit) in rows {
        let snippet = if omit {
            "_shdeps_summary_text \"$2\" \"$3\" \"$4\" \"$5\"\n".to_string()
        } else {
            "_shdeps_summary_text \"$2\" \"$3\" \"$4\" \"$5\" \"$6\"\n".to_string()
        };
        let args: Vec<String> = vec![
            changed.to_string(),
            current.to_string(),
            skipped.to_string(),
            failed.to_string(),
            warnings.to_string(),
        ];
        let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let (code, out, err) = shell_run(fixture.path(), &argv, &snippet);
        assert_eq!(code, 0, "harness exit");
        assert!(err.is_empty(), "harness stderr");
        assert_eq!(
            out,
            summary_text(changed, current, skipped, failed, warnings),
            "summary for ({changed}, {current}, {skipped}, {failed}, {warnings})",
        );
    }
}

/// Rust ops applied to a fresh [`State`].
type Apply = Box<dyn Fn(&mut State)>;
/// Rust mirror of a shell probe snippet after the shared dump.
type Probe = Box<dyn Fn(&State) -> Vec<u8>>;

/// One scripted state scenario: shell ops plus the Rust ops that
/// must leave the identical record, with an optional probe snippet
/// after the shared dump (and its Rust mirror) for display labels
/// outside the discovery order.
struct Scenario {
    /// Short label for failure messages.
    name: &'static str,
    /// Extra `$2..` words for the shell snippet.
    argv: Vec<Vec<u8>>,
    /// Shell ops; [`DUMP`] then `tail` follow in the runner.
    ops: &'static str,
    /// Rust ops applied to a fresh [`State`].
    apply: Apply,
    /// Shell probe after the dump.
    tail: &'static str,
    /// Rust mirror of the probe output.
    tail_rust: Probe,
    /// Whether the shell side must diagnose on stderr (the
    /// empty-group refusal below): stdout still compares exactly,
    /// but the harness diagnostic assertion is lifted.
    expect_stderr: bool,
}

/// Render the Rust dump in the exact [`DUMP`] layout.
fn rust_dump(state: &State) -> Vec<u8> {
    let mut out = Vec::new();
    for group in state.order() {
        out.extend_from_slice(b"order:<");
        out.extend_from_slice(group);
        out.extend_from_slice(b">\n");
    }
    for group in state.order() {
        out.extend_from_slice(b"display:<");
        out.extend_from_slice(group);
        out.extend_from_slice(b">=");
        out.extend_from_slice(&state.display_label(group));
        out.extend_from_slice(b"\n");
        out.extend_from_slice(b"items:<");
        out.extend_from_slice(group);
        out.extend_from_slice(b">:");
        if let Some(blob) = state.items_blob(group) {
            out.extend_from_slice(blob);
        }
        out.extend_from_slice(b"<end>\n");
        out.extend_from_slice(b"summary:<");
        out.extend_from_slice(group);
        out.extend_from_slice(b">:");
        if let Some(blob) = state.summary_blob(group) {
            out.extend_from_slice(blob);
        }
        out.extend_from_slice(b"<end>\n");
    }
    out
}

#[test]
fn record_rows_agree() {
    let scenarios: Vec<Scenario> = vec![
        Scenario {
            name: "remember-dedup",
            argv: Vec::new(),
            ops: "_shdeps_remember_group 'cargo'; _shdeps_remember_group 'cargo'; _shdeps_remember_group 'go'\n",
            apply: Box::new(|state| {
                state.remember_group(b"cargo");
                state.remember_group(b"cargo");
                state.remember_group(b"go");
            }),
            tail: "",
            tail_rust: Box::new(|_| Vec::new()),
            expect_stderr: false,
        },
        Scenario {
            name: "items-empty-fields",
            argv: Vec::new(),
            ops: concat!(
                "_shdeps_record_item 'cargo' 'changed' 'ripgrep' 'fast search'\n",
                "_shdeps_record_item 'cargo' 'failed' '' ''\n",
            ),
            apply: Box::new(|state| {
                state.record_item(b"cargo", b"changed", b"ripgrep", b"fast search");
                state.record_item(b"cargo", b"failed", b"", b"");
            }),
            tail: "",
            tail_rust: Box::new(|_| Vec::new()),
            expect_stderr: false,
        },
        Scenario {
            name: "empty-group-refused",
            argv: Vec::new(),
            // The shell's empty assoc subscript aborts each record
            // call storing nothing (the `bad array subscript`
            // diagnostics land on stderr); the display fallback
            // still expands to `Other` with exit 0.
            ops: concat!(
                "_shdeps_remember_group ''\n",
                "_shdeps_record_item '' 'ok' 'mystery' 'no group'\n",
                "_shdeps_record_group_summary '' '' 'ok' '0' '3' '0' '0' '10' '0'\n",
            ),
            apply: Box::new(|state| {
                state.remember_group(b"");
                state.record_item(b"", b"ok", b"mystery", b"no group");
                state.record_group_summary(b"", b"", b"ok", 0, 3, 0, 0, b"10", 0);
            }),
            tail: "printf 'probe:<%s>\\n' \"$(_shdeps_display_label '')\"\n",
            tail_rust: Box::new(|state| {
                let mut out = b"probe:<".to_vec();
                out.extend_from_slice(&state.display_label(b""));
                out.extend_from_slice(b">\n");
                out
            }),
            expect_stderr: true,
        },
        Scenario {
            name: "summary-derived-label",
            argv: Vec::new(),
            ops: "_shdeps_record_group_summary 'cargo' '' 'changed' '1' '2' '0' '0' '1500' '0'\n",
            apply: Box::new(|state| {
                state.record_group_summary(b"cargo", b"", b"changed", 1, 2, 0, 0, b"1500", 0);
            }),
            tail: "",
            tail_rust: Box::new(|_| Vec::new()),
            expect_stderr: false,
        },
        Scenario {
            name: "summary-explicit-label-empty-elapsed-unknown-group",
            argv: Vec::new(),
            ops: concat!(
                "_shdeps_record_group_summary 'pip' 'Pip Extra' 'ok' '0' '9' '0' '0' '' '0'\n",
                "_shdeps_record_group_summary 'brew' '' 'ok' '0' '3' '0' '0' '200' '0'\n",
            ),
            apply: Box::new(|state| {
                state.record_group_summary(b"pip", b"Pip Extra", b"ok", 0, 9, 0, 0, b"", 0);
                state.record_group_summary(b"brew", b"", b"ok", 0, 3, 0, 0, b"200", 0);
            }),
            tail: "",
            tail_rust: Box::new(|_| Vec::new()),
            expect_stderr: false,
        },
        Scenario {
            name: "summary-default-warnings",
            argv: Vec::new(),
            // Eight shell words: `${9:-0}` defaults the warnings.
            ops: "_shdeps_record_group_summary 'uv' '' 'warning' '0' '1' '0' '0' '42'\n",
            apply: Box::new(|state| {
                state.record_group_summary(b"uv", b"", b"warning", 0, 1, 0, 0, b"42", 0);
            }),
            tail: "",
            tail_rust: Box::new(|_| Vec::new()),
            expect_stderr: false,
        },
        Scenario {
            name: "combined-overwrite",
            argv: Vec::new(),
            // The second summary overwrites the first record while
            // the items keep appending and the order stays single.
            ops: concat!(
                "_shdeps_record_item 'npm' 'changed' 'left-pad' 'new api'\n",
                "_shdeps_record_group_summary 'npm' '' 'changed' '1' '4' '0' '0' '900' '0'\n",
                "_shdeps_record_item 'npm' 'failed' 'right-pad' 'rate limited'\n",
                "_shdeps_record_group_summary 'npm' 'NPM' 'failed' '1' '4' '0' '1' '950' '0'\n",
            ),
            apply: Box::new(|state| {
                state.record_item(b"npm", b"changed", b"left-pad", b"new api");
                state.record_group_summary(b"npm", b"", b"changed", 1, 4, 0, 0, b"900", 0);
                state.record_item(b"npm", b"failed", b"right-pad", b"rate limited");
                state.record_group_summary(b"npm", b"NPM", b"failed", 1, 4, 0, 1, b"950", 0);
            }),
            // A never-recorded group still resolves its label.
            tail: "printf 'probe:<%s>\\n' \"$(_shdeps_display_label 'other')\"\n",
            tail_rust: Box::new(|state| {
                let mut out = b"probe:<".to_vec();
                out.extend_from_slice(&state.display_label(b"other"));
                out.extend_from_slice(b">\n");
                out
            }),
            expect_stderr: false,
        },
        Scenario {
            name: "non-utf8-group",
            argv: vec![b"\xff-group".to_vec()],
            ops: concat!(
                "_shdeps_remember_group \"$2\"\n",
                "_shdeps_record_item \"$2\" 'ok' 'n' 'd'\n",
            ),
            apply: Box::new(|state| {
                state.remember_group(b"\xff-group");
                state.record_item(b"\xff-group", b"ok", b"n", b"d");
            }),
            tail: "printf 'probe:<%s>=' \"$2\"; _shdeps_display_label \"$2\"; printf '\\n'\n",
            tail_rust: Box::new(|state| {
                let mut out = b"probe:<".to_vec();
                out.extend_from_slice(b"\xff-group");
                out.extend_from_slice(b">=");
                out.extend_from_slice(&state.display_label(b"\xff-group"));
                out.extend_from_slice(b"\n");
                out
            }),
            expect_stderr: false,
        },
    ];
    for scenario in &scenarios {
        let fixture = TempDir::new("shdeps-ui-record").expect("fixture dir");
        let snippet = format!("{}{}{}", scenario.ops, DUMP, scenario.tail);
        let argv: Vec<&OsStr> = scenario
            .argv
            .iter()
            .map(|word| OsStr::from_bytes(word))
            .collect();
        let (code, out, err) = shell_run(fixture.path(), &argv, &snippet);
        assert_eq!(code, 0, "harness exit for {}", scenario.name);
        if scenario.expect_stderr {
            assert!(
                !err.is_empty(),
                "missing refusal diagnostic for {}",
                scenario.name
            );
        } else {
            assert!(err.is_empty(), "harness stderr for {}", scenario.name);
        }
        let mut state = State::new();
        (scenario.apply)(&mut state);
        let mut want = rust_dump(&state);
        want.extend_from_slice(&(scenario.tail_rust)(&state));
        assert_eq!(out, want, "record for {}", scenario.name);
    }
}
