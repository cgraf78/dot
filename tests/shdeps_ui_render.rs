//! Differential parity tests for `src/shdeps_ui_render.rs` against
//! the live shell (`lib/dot/providers/shdeps-ui.sh`, part 2): the
//! session reset, the prompt pause/resume pair, and the four
//! renderers (verbose rows, verbose sections, wanted-status rows,
//! and group summaries). Every case runs the live shell function and
//! its Rust twin on identical inputs and compares stdout bytes
//! exactly.
//!
//! Only this family plus the `_ui_*` primitives it renders through
//! are sourced: the group record the renderers read is planted
//! literally on both sides (the record lane itself is still shell),
//! and the event, proc, and FIFO lanes stay out of the oracle. The
//! `_C_*` palette slots carry distinctive markers so every test
//! proves slot selection through the already ported `progress_ui`
//! twins. The display fallback the renderers take as a parameter is
//! read once from the live `_shdeps_group_label`, keeping the
//! part-1 vocabulary single-sourced in the shell.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::progress_ui::Palette;
use dot::shdeps_ui_render::{
    Ui, have_jq, print_group_items_with_status, print_group_summaries, print_verbose_group_rows,
    print_verbose_items, prompt_pause, prompt_resume, reset,
};
use dot::test_support::TempDir;

/// Sources for the render chapter: the progress primitives plus the
/// adapter, the marker palette, and the four `DOT_UI_SHDEPS_*` map
/// declarations exactly as `_shdeps_ui_reset` establishes them.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/progress-ui.sh\"\n",
    ". \"$1/lib/dot/providers/shdeps-ui.sh\"\n",
    "_C_RESET='<R>'\n",
    "_C_BOLD='<B>'\n",
    "_C_DIM='<D>'\n",
    "_C_GREEN='<G>'\n",
    "_C_YELLOW='<Y>'\n",
    "_C_RED='<E>'\n",
    "_C_BLUE='<U>'\n",
    "_C_CYAN='<C>'\n",
    "_C_WHITE='<W>'\n",
    "DOT_UI_SHDEPS_GROUP_ORDER=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_SEEN=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_LABELS=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_ITEMS=()\n",
    "declare -gA DOT_UI_SHDEPS_GROUP_SUMMARIES=()\n",
);

/// Run one shell snippet with the chapter sourced. `argv` arrives as
/// `$2..`; `extra` overrides the scrubbed environment per row (used
/// for `PATH`, `DOT_QUIET`, and the threshold); byte-exact stdout is
/// the oracle.
fn shell_run(
    fixture: &Path,
    argv: &[&OsStr],
    extra: &[(&str, &str)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
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
    for (key, value) in extra {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Marker palette matching the harness preamble.
fn marker_palette() -> Palette {
    Palette {
        reset: "<R>".to_string(),
        bold: "<B>".to_string(),
        dim: "<D>".to_string(),
        green: "<G>".to_string(),
        yellow: "<Y>".to_string(),
        red: "<E>".to_string(),
        blue: "<U>".to_string(),
        cyan: "<C>".to_string(),
        white: "<W>".to_string(),
    }
}

/// Plain renderer environment over `palette`: visible output with
/// byte cell counting, like the harness locale.
fn plain_ui(palette: &Palette) -> Ui<'_> {
    Ui {
        palette,
        quiet: false,
        multibyte: false,
    }
}

/// Quote arbitrary bytes as a `$'...'` shell word.
fn shq(bytes: &[u8]) -> String {
    let mut word = String::from("$'");
    for byte in bytes {
        match byte {
            b'\t' => word.push_str("\\t"),
            b'\n' => word.push_str("\\n"),
            b'\r' => word.push_str("\\r"),
            b'\'' => word.push_str("\\'"),
            b'\\' => word.push_str("\\\\"),
            0x20..=0x7e => word.push(*byte as char),
            _ => word.push_str(&format!("\\x{byte:02x}")),
        }
    }
    word.push('\'');
    word
}

/// Shell snippet planting the record maps literally: discovery order
/// plus the label, item, and summary blobs. The printers never
/// consult the `_SEEN` map, so only the order ships.
fn plant(
    order: &[Vec<u8>],
    labels: &HashMap<Vec<u8>, Vec<u8>>,
    items: &HashMap<Vec<u8>, Vec<u8>>,
    summaries: &HashMap<Vec<u8>, Vec<u8>>,
) -> String {
    let mut snippet = String::from("DOT_UI_SHDEPS_GROUP_ORDER=(");
    for group in order {
        snippet.push_str(&shq(group));
        snippet.push(' ');
    }
    snippet.push_str(")\n");
    for (name, map) in [
        ("DOT_UI_SHDEPS_GROUP_LABELS", labels),
        ("DOT_UI_SHDEPS_GROUP_ITEMS", items),
        ("DOT_UI_SHDEPS_GROUP_SUMMARIES", summaries),
    ] {
        snippet.push_str(name);
        snippet.push_str("=(");
        let mut keys: Vec<&Vec<u8>> = map.keys().collect();
        keys.sort();
        for key in keys {
            snippet.push('[');
            snippet.push_str(&shq(key));
            snippet.push_str("]=");
            snippet.push_str(&shq(&map[key]));
            snippet.push(' ');
        }
        snippet.push_str(")\n");
    }
    snippet
}

/// Display fallback table read live from the shell vocabulary, so
/// the part-1 `_shdeps_group_label` stays single-sourced in the
/// oracle. Covers every group the scenarios leave unlabeled; groups
/// must not contain newlines (the table is line-oriented).
fn fallback_table(fixture: &Path, groups: &[Vec<u8>]) -> HashMap<Vec<u8>, Vec<u8>> {
    for group in groups {
        assert!(
            !group.contains(&b'\n'),
            "fallback oracle group with newline: {group:?}"
        );
    }
    // `$0` is the harness name and `$1` the repo; the groups arrive
    // as `$2..`, so one shift leaves exactly the groups.
    let snippet = "shift 1\ni=0\nfor g in \"$@\"; do i=$((i + 1)); printf 'fb%s:<%s>=' \"$i\" \"$g\"; _shdeps_group_label \"$g\"; printf '\\n'; done\n";
    let argv: Vec<&OsStr> = groups
        .iter()
        .map(|group| OsStr::from_bytes(group))
        .collect();
    let (code, out, err) = shell_run(fixture, &argv, &[], snippet);
    assert_eq!(code, 0, "fallback oracle exit");
    assert!(err.is_empty(), "fallback oracle stderr");
    let lines: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
    assert_eq!(
        lines.len(),
        groups.len() + 1,
        "one oracle line per group plus the trailing split"
    );
    let mut table = HashMap::new();
    for (index, group) in groups.iter().enumerate() {
        let line = lines[index];
        let prefix = format!("fb{}:<", index + 1).into_bytes();
        assert!(
            line.starts_with(&prefix),
            "oracle prefix for {group:?}: {line:?}"
        );
        let rest = &line[prefix.len()..];
        assert!(
            rest.starts_with(group.as_slice()),
            "oracle echo for {group:?}: {line:?}"
        );
        let tail = &rest[group.len()..];
        assert!(
            tail.starts_with(b">="),
            "oracle separator for {group:?}: {line:?}"
        );
        table.insert(group.clone(), tail[">=".len()..].to_vec());
    }
    table
}

/// Display resolver over explicit labels plus the oracle fallback,
/// mirroring the shell `${...:-...}` lookup the renderers share.
fn resolver<'a>(
    labels: &'a HashMap<Vec<u8>, Vec<u8>>,
    oracle: &'a HashMap<Vec<u8>, Vec<u8>>,
) -> impl Fn(&[u8]) -> Vec<u8> + 'a {
    move |group: &[u8]| match labels.get(group) {
        Some(label) if !label.is_empty() => label.clone(),
        _ => oracle.get(group).cloned().unwrap_or_else(|| group.to_vec()),
    }
}

/// One executable probe named `name` under `dir`.
fn stage_exec(dir: &Path, name: &str) {
    let path = dir.join(name);
    std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("stage probe");
    let mut perms = std::fs::metadata(&path).expect("probe meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("probe exec bit");
}

#[test]
fn have_jq_rows_agree() {
    let fixture = TempDir::new("shdeps-ui-render-jq").expect("fixture dir");
    let root = fixture.path();
    let with_jq = root.join("with-jq");
    let dull_jq = root.join("dull-jq");
    std::fs::create_dir_all(&with_jq).expect("dirs");
    std::fs::create_dir_all(&dull_jq).expect("dirs");
    stage_exec(&with_jq, "jq");
    // Default-mode `command -v` reports PATH entries regardless of
    // the exec bit: a dull file still probes true.
    std::fs::write(dull_jq.join("jq"), b"#!/bin/sh\nexit 0\n").expect("stage dull jq");
    std::fs::write(root.join("dark-jq-file"), b"x").expect("stage dark");
    let mut perms = std::fs::metadata(root.join("dark-jq-file"))
        .expect("dark meta")
        .permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(root.join("dark-jq-file"), perms).expect("dark bits");
    let dark_dir = root.join("dark-dir");
    std::fs::create_dir_all(&dark_dir).expect("dark dir");
    std::fs::rename(root.join("dark-jq-file"), dark_dir.join("jq")).expect("move dark");
    // A directory spells the name but is not a command, while a
    // symlink to a file resolves like one and a symlink to a
    // directory (or nowhere) never does.
    std::fs::create_dir_all(root.join("dir-jq").join("jq")).expect("dir jq");
    let link_dir = root.join("link-dir");
    std::fs::create_dir_all(&link_dir).expect("link dir");
    std::os::unix::fs::symlink(with_jq.join("jq"), link_dir.join("jq")).expect("symlink");
    let dir_link_dir = root.join("dir-link-dir");
    std::fs::create_dir_all(&dir_link_dir).expect("dir link dir");
    std::os::unix::fs::symlink(root.join("dir-jq"), dir_link_dir.join("jq")).expect("dir symlink");
    let dead_link_dir = root.join("dead-link-dir");
    std::fs::create_dir_all(&dead_link_dir).expect("dead link dir");
    std::os::unix::fs::symlink(root.join("missing-target"), dead_link_dir.join("jq"))
        .expect("dead symlink");
    // A non-regular executable still probes true, like the shell.
    let fifo_dir = root.join("fifo-dir");
    std::fs::create_dir_all(&fifo_dir).expect("fifo dir");
    let status = Command::new("mkfifo")
        .arg(fifo_dir.join("jq"))
        .status()
        .expect("spawn mkfifo");
    assert!(status.success(), "mkfifo fixture");

    let missing = root.join("missing");
    let rows: Vec<(&str, String, bool)> = vec![
        ("exec", with_jq.to_string_lossy().into_owned(), true),
        ("non-exec", dull_jq.to_string_lossy().into_owned(), true),
        ("unreadable", dark_dir.to_string_lossy().into_owned(), true),
        ("missing-dir", missing.to_string_lossy().into_owned(), false),
        ("empty-path", String::new(), false),
        (
            "second-hit",
            format!(
                "{}:{}",
                missing.to_string_lossy(),
                with_jq.to_string_lossy()
            ),
            true,
        ),
        (
            "directory",
            root.join("dir-jq").to_string_lossy().into_owned(),
            false,
        ),
        ("symlink", link_dir.to_string_lossy().into_owned(), true),
        (
            "dir-symlink",
            dir_link_dir.to_string_lossy().into_owned(),
            false,
        ),
        (
            "dead-symlink",
            dead_link_dir.to_string_lossy().into_owned(),
            false,
        ),
        ("fifo", fifo_dir.to_string_lossy().into_owned(), true),
    ];
    for (name, path_dirs, expect) in &rows {
        let path_arg = OsStr::from_bytes(path_dirs.as_bytes());
        let snippet = "PATH=\"$2\"\nif command -v jq >/dev/null 2>&1; then printf 'jq:1\\n'; else printf 'jq:0\\n'; fi\n";
        let (code, out, err) = shell_run(root, &[path_arg], &[], snippet);
        assert_eq!(code, 0, "harness exit for {name}");
        assert!(err.is_empty(), "harness stderr for {name}");
        let want = format!("jq:{}\n", if *expect { 1 } else { 0 }).into_bytes();
        assert_eq!(out, want, "shell probe for {name}");
        assert_eq!(have_jq(path_dirs), *expect, "rust probe for {name}");
    }
}

/// Shell dump of the post-reset session globals plus the map sizes.
/// The maps restart empty beside the session on the part-1 record
/// lane; pinning their zero sizes here guards the shell half of that
/// split while the session bytes compare differentially.
const RESET_DUMP: &str = concat!(
    "printf 'status:<%s>\\n' \"$DOT_UI_SHDEPS_STATUS\"\n",
    "printf 'summary:<%s>\\n' \"$DOT_UI_SHDEPS_SUMMARY\"\n",
    "printf 'has_jq:<%s>\\n' \"$DOT_UI_SHDEPS_HAS_JQ\"\n",
    "printf 'active:<%s>\\n' \"$DOT_UI_SHDEPS_PROMPT_ACTIVE\"\n",
    "printf 'ack_set:<%s>\\n' \"${DOT_UI_SHDEPS_PROMPT_ACK_FD+x}\"\n",
    "printf 'order_n:%s\\n' \"${#DOT_UI_SHDEPS_GROUP_ORDER[@]}\"\n",
    "printf 'seen_n:%s\\n' \"${#DOT_UI_SHDEPS_GROUP_SEEN[@]}\"\n",
    "printf 'labels_n:%s\\n' \"${#DOT_UI_SHDEPS_GROUP_LABELS[@]}\"\n",
    "printf 'items_n:%s\\n' \"${#DOT_UI_SHDEPS_GROUP_ITEMS[@]}\"\n",
    "printf 'summaries_n:%s\\n' \"${#DOT_UI_SHDEPS_GROUP_SUMMARIES[@]}\"\n",
);

/// Stale session globals plus map junk, proving the reset overwrites
/// rather than merges.
const RESET_SETUP: &str = concat!(
    "DOT_UI_SHDEPS_STATUS=stale\n",
    "DOT_UI_SHDEPS_SUMMARY=stale\n",
    "DOT_UI_SHDEPS_HAS_JQ=9\n",
    "DOT_UI_SHDEPS_PROMPT_ACK_FD=99\n",
    "DOT_UI_SHDEPS_GROUP_ORDER=(stale)\n",
    "DOT_UI_SHDEPS_GROUP_SEEN=([stale]=1)\n",
    "DOT_UI_SHDEPS_GROUP_LABELS=([stale]=junk)\n",
    "DOT_UI_SHDEPS_GROUP_ITEMS=([stale]=junk)\n",
    "DOT_UI_SHDEPS_GROUP_SUMMARIES=([stale]=junk)\n",
);

#[test]
fn reset_rows_agree() {
    let fixture = TempDir::new("shdeps-ui-render-reset").expect("fixture dir");
    let root = fixture.path();
    let with_jq = root.join("with-jq");
    let without_jq = root.join("without-jq");
    std::fs::create_dir_all(&with_jq).expect("dirs");
    std::fs::create_dir_all(&without_jq).expect("dirs");
    stage_exec(&with_jq, "jq");
    // (probe path, probed result, preset prompt state)
    let rows: Vec<(&str, PathBuf, bool, &str)> = vec![
        (
            "jq-active",
            with_jq.clone(),
            true,
            "DOT_UI_SHDEPS_PROMPT_ACTIVE=1\n",
        ),
        (
            "jq-idle",
            with_jq.clone(),
            true,
            "DOT_UI_SHDEPS_PROMPT_ACTIVE=0\n",
        ),
        (
            "no-jq-active",
            without_jq.clone(),
            false,
            "DOT_UI_SHDEPS_PROMPT_ACTIVE=1\n",
        ),
        (
            "no-jq-idle",
            without_jq.clone(),
            false,
            "DOT_UI_SHDEPS_PROMPT_ACTIVE=0\n",
        ),
    ];
    // The fourth tuple slot presets the shell prompt flag (reset
    // clears it either way); the port builds the same fresh session
    // from the probe alone, while the stale shell setup proves the
    // overwrite instead of a merge.
    for (name, probe_dir, expect_jq, preset) in &rows {
        let path_value = probe_dir.to_string_lossy().into_owned();
        let snippet = format!("{RESET_SETUP}{preset}_shdeps_ui_reset\n{RESET_DUMP}");
        let (code, out, err) = shell_run(root, &[], &[("PATH", path_value.as_str())], &snippet);
        assert_eq!(code, 0, "harness exit for {name}");
        assert!(err.is_empty(), "harness stderr for {name}");
        let session = reset(*expect_jq);
        let mut want = Vec::new();
        want.extend_from_slice(b"status:<");
        want.extend_from_slice(&session.status);
        want.extend_from_slice(b">\nsummary:<");
        want.extend_from_slice(&session.summary);
        want.extend_from_slice(b">\n");
        want.extend_from_slice(
            format!(
                "has_jq:<{}>\nactive:<{}>\nack_set:<>\n",
                if session.has_jq { 1 } else { 0 },
                if session.prompt_active { 1 } else { 0 },
            )
            .as_bytes(),
        );
        want.extend_from_slice(b"order_n:0\nseen_n:0\nlabels_n:0\nitems_n:0\nsummaries_n:0\n");
        assert_eq!(out, want, "reset for {name}");
    }
}

/// Prompt dump shared by the pause/resume rows: the flag plus the
/// acknowledgment file (`$2`), whose trailing newline the shell
/// command substitution strips on both sides alike.
const PROMPT_DUMP: &str = concat!(
    "printf 'active:<%s>\\n' \"$DOT_UI_SHDEPS_PROMPT_ACTIVE\"\n",
    "if [[ -e \"$2\" ]]; then printf 'ack:<%s>\\n' \"$(<\"$2\")\"; else printf 'ack:<absent>\\n'; fi\n",
);

/// Rust twin of [`PROMPT_DUMP`]: flag line plus the file content with
/// the same trailing-newline strip.
fn prompt_dump(active: bool, ack_file: &Path) -> Vec<u8> {
    let mut out = format!("active:<{}>\n", if active { 1 } else { 0 }).into_bytes();
    match std::fs::read(ack_file) {
        Ok(mut bytes) => {
            while bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            out.extend_from_slice(b"ack:<");
            out.extend_from_slice(&bytes);
            out.extend_from_slice(b">\n");
        }
        Err(_) => out.extend_from_slice(b"ack:<absent>\n"),
    }
    out
}

#[test]
fn prompt_rows_agree() {
    // (name, live preset, ack setup, ack value for the port)
    // Every pause row pauses twice: the second pause proves the live
    // flag the first pause cleared, staying silent on stdout while
    // the acknowledgment still fires.
    let rows: Vec<(&str, &str, &str, &str)> = vec![
        (
            "pause-live-fd",
            "DOT_UI_LIVE_ACTIVE=1\n",
            "rm -f \"$2\"\nexec {ack}>\"$2\"\nDOT_UI_SHDEPS_PROMPT_ACK_FD=$ack\n_shdeps_prompt_pause\n_shdeps_prompt_pause\n",
            "10",
        ),
        (
            "pause-quiet-fd",
            "DOT_UI_LIVE_ACTIVE=0\n",
            "rm -f \"$2\"\nexec {ack}>\"$2\"\nDOT_UI_SHDEPS_PROMPT_ACK_FD=$ack\n_shdeps_prompt_pause\n_shdeps_prompt_pause\n",
            "10",
        ),
        (
            "pause-live-no-fd",
            "DOT_UI_LIVE_ACTIVE=1\n",
            "rm -f \"$2\"\nunset DOT_UI_SHDEPS_PROMPT_ACK_FD\n_shdeps_prompt_pause\n_shdeps_prompt_pause\n",
            "",
        ),
        (
            "pause-quiet-bad-fd",
            "DOT_UI_LIVE_ACTIVE=0\n",
            "rm -f \"$2\"\nDOT_UI_SHDEPS_PROMPT_ACK_FD=abc\n_shdeps_prompt_pause\n_shdeps_prompt_pause\n",
            "abc",
        ),
        (
            "resume",
            "DOT_UI_LIVE_ACTIVE=0\n",
            "rm -f \"$2\"\nDOT_UI_SHDEPS_PROMPT_ACTIVE=1\n_shdeps_prompt_resume\n",
            "",
        ),
        (
            "pause-then-resume",
            "DOT_UI_LIVE_ACTIVE=0\n",
            "rm -f \"$2\"\nexec {ack}>\"$2\"\nDOT_UI_SHDEPS_PROMPT_ACK_FD=$ack\n_shdeps_prompt_pause\n_shdeps_prompt_pause\n_shdeps_prompt_resume\n",
            "10",
        ),
    ];
    for (name, live_setup, ops, ack_value) in &rows {
        let fixture = TempDir::new("shdeps-ui-render-prompt").expect("fixture dir");
        let ack_file = fixture.path().join("ack");
        let ack_arg = OsStr::from_bytes(ack_file.as_os_str().as_bytes());
        let snippet = format!("DOT_UI_SHDEPS_PROMPT_ACTIVE=0\n{live_setup}{ops}{PROMPT_DUMP}");
        let (code, out, err) = shell_run(fixture.path(), &[ack_arg], &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name}");
        assert!(err.is_empty(), "harness stderr for {name}");
        // Replay the ops through the port: resume-only rows never
        // pause, pause rows pause twice (then resume for the last
        // row), appending every acknowledgment like the shell.
        let mut session = dot::shdeps_ui_render::Session {
            status: b"ok".to_vec(),
            summary: b"dependencies checked".to_vec(),
            has_jq: false,
            prompt_active: *name == "resume",
        };
        // The shell already ran above against this path; replay the
        // port against a fresh file so acknowledgments never double.
        std::fs::remove_file(&ack_file).ok();
        let mut live = live_setup.contains("LIVE_ACTIVE=1");
        let mut want = Vec::new();
        if *name != "resume" {
            for _ in 0..2 {
                let (bytes, next, ack) = prompt_pause(&mut session, live, ack_value);
                want.extend_from_slice(&bytes);
                live = next;
                if let Some(token) = ack {
                    let mut prior = std::fs::read(&ack_file).unwrap_or_default();
                    prior.extend_from_slice(&token);
                    std::fs::write(&ack_file, &prior).expect("write ack");
                }
            }
            if *name == "pause-then-resume" {
                prompt_resume(&mut session);
            }
        } else {
            prompt_resume(&mut session);
        }
        want.extend_from_slice(&prompt_dump(session.prompt_active, &ack_file));
        assert_eq!(out, want, "prompt for {name}");
    }
}

/// One verbose-rows scenario: the record maps plus the queried
/// label. `shell_ops` replaces the literal planting when set, so the
/// record cross-check rows drive the real `_shdeps_record_item`
/// while the port still reads the literal twin.
struct GroupRowsCase {
    name: &'static str,
    order: Vec<Vec<u8>>,
    labels: Vec<(Vec<u8>, Vec<u8>)>,
    items: Vec<(Vec<u8>, Vec<u8>)>,
    label: Vec<u8>,
    live: bool,
    shell_ops: Option<&'static str>,
}

#[test]
fn verbose_group_rows_agree() {
    let fixture = TempDir::new("shdeps-ui-render-rows").expect("fixture dir");
    let oracle = fallback_table(
        fixture.path(),
        &[
            b"cargo".to_vec(),
            b"pip".to_vec(),
            b"c*".to_vec(),
            b"?".to_vec(),
            b"[a]".to_vec(),
            b"\xff-group".to_vec(),
        ],
    );
    let cargo_items = b"changed\trg\tfast search\nfailed\tfd\t\n".to_vec();
    let go_items = b"ok\tg\t\n".to_vec();
    let base_labels = vec![
        (b"cargo".to_vec(), b"Cargo".to_vec()),
        (b"go".to_vec(), b"Go".to_vec()),
    ];
    let base_items = vec![
        (b"cargo".to_vec(), cargo_items.clone()),
        (b"go".to_vec(), go_items.clone()),
    ];
    let base_order = vec![b"cargo".to_vec(), b"go".to_vec()];
    let cases = vec![
        GroupRowsCase {
            name: "cargo-label",
            order: base_order.clone(),
            labels: base_labels.clone(),
            items: base_items.clone(),
            label: b"Cargo".to_vec(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "go-label",
            order: base_order.clone(),
            labels: base_labels.clone(),
            items: base_items.clone(),
            label: b"Go".to_vec(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "no-match",
            order: base_order.clone(),
            labels: base_labels.clone(),
            items: base_items.clone(),
            label: b"Other".to_vec(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "fallback",
            order: vec![b"pip".to_vec()],
            labels: Vec::new(),
            items: vec![(b"pip".to_vec(), b"ok\tp\td\n".to_vec())],
            label: oracle[&b"pip".to_vec()].clone(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "empty-recorded-label",
            order: vec![b"pip".to_vec()],
            labels: vec![(b"pip".to_vec(), Vec::new())],
            items: vec![(b"pip".to_vec(), b"ok\tp\td\n".to_vec())],
            label: oracle[&b"pip".to_vec()].clone(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "shared-label",
            order: Vec::new(),
            labels: vec![
                (b"github-releases".to_vec(), b"GitHub".to_vec()),
                (b"github-repos".to_vec(), b"GitHub".to_vec()),
            ],
            items: vec![
                (b"github-releases".to_vec(), b"changed\trel\tv2\n".to_vec()),
                (b"github-repos".to_vec(), b"ok\trepo\t\n".to_vec()),
            ],
            label: b"GitHub".to_vec(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "dedup",
            order: vec![b"cargo".to_vec(), b"cargo".to_vec(), b"go".to_vec()],
            labels: base_labels.clone(),
            items: base_items.clone(),
            label: b"Cargo".to_vec(),
            live: false,
            shell_ops: None,
        },
        // The dedup gate quotes the group, so `c*` never matches
        // `cargo` as a pattern: both print under their own labels. A
        // globbing dedup would swallow the second group here.
        GroupRowsCase {
            name: "star-literal",
            order: vec![b"cargo".to_vec(), b"c*".to_vec()],
            labels: base_labels.clone(),
            items: vec![
                (b"cargo".to_vec(), cargo_items.clone()),
                (b"c*".to_vec(), b"ok\tstar\td\n".to_vec()),
            ],
            label: oracle[&b"c*".to_vec()].clone(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "star-first",
            order: vec![b"c*".to_vec(), b"cargo".to_vec()],
            labels: base_labels.clone(),
            items: vec![
                (b"cargo".to_vec(), cargo_items.clone()),
                (b"c*".to_vec(), b"ok\tstar\td\n".to_vec()),
            ],
            label: b"Cargo".to_vec(),
            live: false,
            shell_ops: None,
        },
        // `?` and `[...]` spellings stay literal too.
        GroupRowsCase {
            name: "question-literal",
            order: vec![b"?".to_vec(), b"cargo".to_vec()],
            labels: base_labels.clone(),
            items: vec![
                (b"cargo".to_vec(), cargo_items.clone()),
                (b"?".to_vec(), b"ok\tq\td\n".to_vec()),
            ],
            label: oracle[&b"?".to_vec()].clone(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "bracket-literal",
            order: vec![b"cargo".to_vec(), b"[a]".to_vec()],
            labels: base_labels.clone(),
            items: vec![
                (b"cargo".to_vec(), cargo_items.clone()),
                (b"[a]".to_vec(), b"ok\tb\td\n".to_vec()),
            ],
            label: oracle[&b"[a]".to_vec()].clone(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "non-utf8",
            order: vec![b"\xff-group".to_vec()],
            labels: Vec::new(),
            items: vec![(
                b"\xff-group".to_vec(),
                b"changed\n\xff-name\t\xff-detail\n".to_vec(),
            )],
            label: oracle[&b"\xff-group".to_vec()].clone(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "empty-line-skip",
            order: vec![b"cargo".to_vec()],
            labels: base_labels.clone(),
            items: vec![(
                b"cargo".to_vec(),
                b"changed\ta\tb\n\nfailed\tc\td\n".to_vec(),
            )],
            label: b"Cargo".to_vec(),
            live: false,
            shell_ops: None,
        },
        GroupRowsCase {
            name: "live",
            order: base_order.clone(),
            labels: base_labels.clone(),
            items: base_items.clone(),
            label: b"Cargo".to_vec(),
            live: true,
            shell_ops: None,
        },
        // The shell side plants through the real record function;
        // the port reads the literal twin, pinning the blob format
        // the renderers assume.
        GroupRowsCase {
            name: "record-crosscheck",
            order: vec![b"cargo".to_vec()],
            labels: Vec::new(),
            items: vec![(b"cargo".to_vec(), b"changed\trg\tfast\n".to_vec())],
            label: oracle[&b"cargo".to_vec()].clone(),
            live: false,
            shell_ops: Some("_shdeps_record_item 'cargo' 'changed' 'rg' 'fast'\n"),
        },
    ];
    for case in &cases {
        let fixture = TempDir::new("shdeps-ui-render-row").expect("fixture dir");
        let palette = marker_palette();
        let ui = plain_ui(&palette);
        let order = &case.order;
        let labels: HashMap<Vec<u8>, Vec<u8>> = case.labels.iter().cloned().collect();
        let items: HashMap<Vec<u8>, Vec<u8>> = case.items.iter().cloned().collect();
        let snippet = match case.shell_ops {
            Some(ops) => format!(
                "{ops}DOT_UI_LIVE_ACTIVE={}\n_shdeps_print_verbose_group_rows {}\n",
                if case.live { 1 } else { 0 },
                shq(&case.label),
            ),
            None => format!(
                "{}DOT_UI_LIVE_ACTIVE={}\n_shdeps_print_verbose_group_rows {}\n",
                plant(order, &labels, &items, &HashMap::new()),
                if case.live { 1 } else { 0 },
                shq(&case.label),
            ),
        };
        let (code, out, err) = shell_run(fixture.path(), &[], &[], &snippet);
        assert_eq!(code, 0, "harness exit for {}", case.name);
        assert!(err.is_empty(), "harness stderr for {}", case.name);
        let resolve = resolver(&labels, &oracle);
        let (want, _) = print_verbose_group_rows(
            &ui,
            case.live,
            order,
            &items,
            &labels,
            &resolve,
            &case.label,
        );
        assert_eq!(out, want, "rows for {}", case.name);
    }
}

/// One verbose-sections scenario.
struct SectionsCase {
    name: &'static str,
    order: Vec<Vec<u8>>,
    labels: Vec<(Vec<u8>, Vec<u8>)>,
    items: Vec<(Vec<u8>, Vec<u8>)>,
    verbose: Option<&'static str>,
    live: bool,
    quiet: bool,
}

#[test]
fn verbose_items_agree() {
    let fixture = TempDir::new("shdeps-ui-render-sections").expect("fixture dir");
    let oracle = fallback_table(
        fixture.path(),
        &[
            b"cargo".to_vec(),
            b"go".to_vec(),
            b"pip".to_vec(),
            b"github-releases".to_vec(),
            b"github-repos".to_vec(),
        ],
    );
    let labels = vec![
        (b"cargo".to_vec(), b"Cargo".to_vec()),
        (b"go".to_vec(), b"Go".to_vec()),
    ];
    let items = vec![
        (
            b"cargo".to_vec(),
            b"changed\trg\tfast search\nfailed\tfd\t\n".to_vec(),
        ),
        (b"go".to_vec(), b"ok\tg\t\n".to_vec()),
    ];
    let order = vec![b"cargo".to_vec(), b"go".to_vec()];
    let cases = vec![
        SectionsCase {
            name: "silent",
            order: order.clone(),
            labels: labels.clone(),
            items: items.clone(),
            verbose: None,
            live: false,
            quiet: false,
        },
        SectionsCase {
            name: "two-sections",
            order: order.clone(),
            labels: labels.clone(),
            items: items.clone(),
            verbose: Some("1"),
            live: false,
            quiet: false,
        },
        // Both GitHub groups share one display label, so one section
        // holds both groups' rows in known order.
        SectionsCase {
            name: "github-merge",
            order: Vec::new(),
            labels: Vec::new(),
            items: vec![
                (b"github-repos".to_vec(), b"ok\trepo\t\n".to_vec()),
                (b"github-releases".to_vec(), b"changed\trel\tv2\n".to_vec()),
            ],
            verbose: Some("1"),
            live: false,
            quiet: false,
        },
        SectionsCase {
            name: "unknown-append",
            order: vec![b"cargo".to_vec(), b"pip".to_vec()],
            labels: vec![(b"cargo".to_vec(), b"Cargo".to_vec())],
            items: vec![
                (b"cargo".to_vec(), b"changed\trg\tfast\n".to_vec()),
                (b"pip".to_vec(), b"ok\tp\td\n".to_vec()),
            ],
            verbose: Some("1"),
            live: false,
            quiet: false,
        },
        // `go` is discovered but rowless, so no section appears.
        SectionsCase {
            name: "rows-gate",
            order: order.clone(),
            labels: labels.clone(),
            items: vec![(b"cargo".to_vec(), b"changed\trg\tfast\n".to_vec())],
            verbose: Some("1"),
            live: false,
            quiet: false,
        },
        SectionsCase {
            name: "live",
            order: order.clone(),
            labels: labels.clone(),
            items: items.clone(),
            verbose: Some("1"),
            live: true,
            quiet: false,
        },
        SectionsCase {
            name: "quiet",
            order: order.clone(),
            labels: labels.clone(),
            items: items.clone(),
            verbose: Some("1"),
            live: false,
            quiet: true,
        },
        SectionsCase {
            name: "verbose-zero",
            order: order.clone(),
            labels: labels.clone(),
            items: items.clone(),
            verbose: Some("0"),
            live: false,
            quiet: false,
        },
    ];
    for case in &cases {
        let fixture = TempDir::new("shdeps-ui-render-section").expect("fixture dir");
        let palette = marker_palette();
        let ui = Ui {
            palette: &palette,
            quiet: case.quiet,
            multibyte: false,
        };
        let labels: HashMap<Vec<u8>, Vec<u8>> = case.labels.iter().cloned().collect();
        let items: HashMap<Vec<u8>, Vec<u8>> = case.items.iter().cloned().collect();
        let mut extra = Vec::new();
        if let Some(verbose) = case.verbose {
            extra.push(("DOT_VERBOSE", verbose));
        }
        if case.quiet {
            extra.push(("DOT_QUIET", "1"));
        }
        let snippet = format!(
            "{}DOT_UI_LIVE_ACTIVE={}\n_shdeps_print_verbose_items\n",
            plant(&case.order, &labels, &items, &HashMap::new()),
            if case.live { 1 } else { 0 },
        );
        let (code, out, err) = shell_run(fixture.path(), &[], &extra, &snippet);
        assert_eq!(code, 0, "harness exit for {}", case.name);
        assert!(err.is_empty(), "harness stderr for {}", case.name);
        let resolve = resolver(&labels, &oracle);
        let verbose = case.verbose == Some("1");
        let (want, _) = print_verbose_items(
            &ui,
            case.live,
            verbose,
            &case.order,
            &items,
            &labels,
            &resolve,
        );
        assert_eq!(out, want, "sections for {}", case.name);
    }
}

#[test]
fn group_items_with_status_agree() {
    let fixture = TempDir::new("shdeps-ui-render-wanted").expect("fixture dir");
    let palette = marker_palette();
    let ui = plain_ui(&palette);
    let empty: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    struct WantedRow {
        name: &'static str,
        blob: Vec<u8>,
        group: Vec<u8>,
        wanted: Vec<u8>,
    }
    let rows = vec![
        WantedRow {
            name: "changed-filter",
            blob: b"changed\ta\t1\nfailed\tb\t2\nchanged\tc\t\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"changed".to_vec(),
        },
        WantedRow {
            name: "failed-filter",
            blob: b"changed\ta\t1\nfailed\tb\t2\nwarning\tc\t3\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"failed".to_vec(),
        },
        WantedRow {
            name: "missing-group",
            blob: b"changed\ta\t1\n".to_vec(),
            group: b"go".to_vec(),
            wanted: b"changed".to_vec(),
        },
        WantedRow {
            name: "empty-blob",
            blob: Vec::new(),
            group: b"cargo".to_vec(),
            wanted: b"changed".to_vec(),
        },
        // Short reads: no tabs, one tab, and tabs kept in detail.
        WantedRow {
            name: "shapes",
            blob: b"solo\na\tb\nx\ty\tc\td\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"a".to_vec(),
        },
        WantedRow {
            name: "shapes-solo",
            blob: b"solo\na\tb\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"solo".to_vec(),
        },
        // IFS-whitespace reads: runs collapse, edges strip.
        WantedRow {
            name: "ws-collapse",
            blob: b"a\t\tb\tc\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"a".to_vec(),
        },
        WantedRow {
            name: "ws-leading",
            blob: b"\tfoo\tbar\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"foo".to_vec(),
        },
        WantedRow {
            name: "ws-trailing",
            blob: b"a\tb\t\t\n".to_vec(),
            group: b"cargo".to_vec(),
            wanted: b"a".to_vec(),
        },
    ];
    for row in &rows {
        let fixture = TempDir::new("shdeps-ui-render-want").expect("fixture dir");
        let items: HashMap<Vec<u8>, Vec<u8>> = [(row.group.clone(), row.blob.clone())]
            .into_iter()
            .collect();
        let snippet = format!(
            "{}DOT_UI_LIVE_ACTIVE=0\n_shdeps_print_group_items_with_status {} {}\n",
            plant(&[], &empty, &items, &empty),
            shq(&row.group),
            shq(&row.wanted),
        );
        let (code, out, err) = shell_run(fixture.path(), &[], &[], &snippet);
        assert_eq!(code, 0, "harness exit for {}", row.name);
        assert!(err.is_empty(), "harness stderr for {}", row.name);
        let (want, _) = print_group_items_with_status(&ui, false, &items, &row.group, &row.wanted);
        assert_eq!(out, want, "wanted rows for {}", row.name);
    }
    // The shell side plants through the real record function while
    // the port reads the literal twin.
    let items: HashMap<Vec<u8>, Vec<u8>> =
        [(b"cargo".to_vec(), b"failed\tfd\trate limited\n".to_vec())]
            .into_iter()
            .collect();
    let snippet = concat!(
        "_shdeps_record_item 'cargo' 'failed' 'fd' 'rate limited'\n",
        "DOT_UI_LIVE_ACTIVE=0\n",
        "_shdeps_print_group_items_with_status 'cargo' 'failed'\n",
    );
    let (code, out, err) = shell_run(fixture.path(), &[], &[], snippet);
    assert_eq!(code, 0, "harness exit for record-crosscheck");
    assert!(err.is_empty(), "harness stderr for record-crosscheck");
    let (want, _) = print_group_items_with_status(&ui, false, &items, b"cargo", b"failed");
    assert_eq!(out, want, "wanted rows for record-crosscheck");
}

/// One group-summary scenario. `verbose`/`threshold` mirror the raw
/// environment reads (`None` leaves the variable unset; `Some("")`
/// sets it empty, which reads unset too).
struct SummaryCase {
    name: &'static str,
    order: Vec<Vec<u8>>,
    summaries: Vec<(Vec<u8>, Vec<u8>)>,
    items: Vec<(Vec<u8>, Vec<u8>)>,
    verbose: Option<&'static str>,
    threshold: Option<&'static str>,
    live: bool,
    expect_stderr: bool,
}

#[test]
fn group_summaries_agree() {
    let changed_rows = b"changed\ta\t1\nfailed\tb\t2\n".to_vec();
    let failed_rows = b"changed\ta\t1\nfailed\tb\t2\nwarning\tc\t3\n".to_vec();
    let cases = vec![
        SummaryCase {
            name: "verbose-silent",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"changed\tCargo: 1 changed\t900".to_vec(),
            )],
            items: vec![(b"cargo".to_vec(), changed_rows.clone())],
            verbose: Some("1"),
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "changed-plain",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"changed\tCargo: 1 changed\t900".to_vec(),
            )],
            items: vec![(b"cargo".to_vec(), changed_rows.clone())],
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "ok-and-skipped-silent",
            order: vec![b"go".to_vec(), b"npm".to_vec()],
            summaries: vec![
                (b"go".to_vec(), b"ok\tGo all good\t10".to_vec()),
                (b"npm".to_vec(), b"skipped\tNPM s\t5".to_vec()),
            ],
            items: vec![(b"go".to_vec(), b"ok\tg\t\n".to_vec())],
            verbose: Some("0"),
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "failed-timed",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo: 1 failed\t1500".to_vec())],
            items: vec![(b"cargo".to_vec(), failed_rows.clone())],
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "warning-timed",
            order: vec![b"uv".to_vec()],
            summaries: vec![(b"uv".to_vec(), b"warning\tUV: 1 warning\t42".to_vec())],
            items: vec![(b"uv".to_vec(), b"warning\tu\told\n".to_vec())],
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        // An `ok` group reaching the threshold still reports, with
        // elapsed attached.
        SummaryCase {
            name: "ok-over-threshold",
            order: vec![b"go".to_vec()],
            summaries: vec![(b"go".to_vec(), b"ok\tGo fine\t150".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: Some("100"),
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "changed-over-threshold",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"changed\tCargo: 1 changed\t900".to_vec(),
            )],
            items: vec![(b"cargo".to_vec(), changed_rows.clone())],
            verbose: None,
            threshold: Some("100"),
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "unknown-status",
            order: vec![b"pip".to_vec()],
            summaries: vec![(b"pip".to_vec(), b"bogus\tPip odd\t7".to_vec())],
            items: vec![(b"pip".to_vec(), b"bogus\tp\td\n".to_vec())],
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "empty-elapsed",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\t".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "non-numeric-elapsed",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\tabc".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        // Invalid-octal spellings fail the shell arithmetic with a
        // diagnostic and read false; the port reads the same branch
        // with no diagnostic to print, so only stdout compares.
        SummaryCase {
            name: "threshold-error",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\t08".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: Some("100"),
            live: false,
            expect_stderr: true,
        },
        // Identifier-like spellings read as unset shell variables
        // (`0`) with no diagnostic: against a positive threshold both
        // sides stay false and silent.
        SummaryCase {
            name: "identifier-elapsed",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\tabc".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: Some("100"),
            live: false,
            expect_stderr: false,
        },
        // A failed group reads the same timed note whether or not the
        // threshold fires, so the zero-read agrees here too.
        SummaryCase {
            name: "identifier-elapsed-zero",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\tabc".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: Some("0"),
            live: false,
            expect_stderr: false,
        },
        // A malformed threshold reads false the same way, with the
        // diagnostic staying shell-side.
        SummaryCase {
            name: "bad-threshold",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\t50".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: Some("08"),
            live: false,
            expect_stderr: true,
        },
        SummaryCase {
            name: "embedded-newline",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"failed\tFirst\t5\nSTALE\tINJECTED\t9".to_vec(),
            )],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        // The tab is IFS whitespace, so the empty middle field
        // collapses: the record reads status `failed`, detail `5`,
        // and a defaulted `0` elapsed.
        SummaryCase {
            name: "empty-detail",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\t\t5".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        // A zero threshold hits every group, pinning all three
        // duration arms in one scenario.
        SummaryCase {
            name: "rendering-trio",
            order: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            summaries: vec![
                (b"a".to_vec(), b"ok\tA\t1500".to_vec()),
                (b"b".to_vec(), b"ok\tB\t12000".to_vec()),
                (b"c".to_vec(), b"ok\tC\t42".to_vec()),
            ],
            items: Vec::new(),
            verbose: None,
            threshold: Some("0"),
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "skip-missing",
            order: vec![b"cargo".to_vec(), b"ghost".to_vec(), b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"changed\tCargo: 1 changed\t900".to_vec(),
            )],
            items: vec![(b"cargo".to_vec(), changed_rows.clone())],
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "live",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"changed\tCargo: 1 changed\t900".to_vec(),
            )],
            items: vec![(b"cargo".to_vec(), changed_rows.clone())],
            verbose: None,
            threshold: None,
            live: true,
            expect_stderr: false,
        },
        SummaryCase {
            name: "empty-threshold",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(
                b"cargo".to_vec(),
                b"changed\tCargo: 1 changed\t900".to_vec(),
            )],
            items: vec![(b"cargo".to_vec(), changed_rows.clone())],
            verbose: None,
            threshold: Some(""),
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "negative-elapsed",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\t-5".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        // Leading zeros never take the canonical branch: the shell
        // renders the raw spelling while the port matches it through
        // the same fallback.
        SummaryCase {
            name: "leading-zero-elapsed",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tCargo bad\t010".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
        SummaryCase {
            name: "missing-elapsed-field",
            order: vec![b"cargo".to_vec()],
            summaries: vec![(b"cargo".to_vec(), b"failed\tJust detail".to_vec())],
            items: Vec::new(),
            verbose: None,
            threshold: None,
            live: false,
            expect_stderr: false,
        },
    ];
    for case in &cases {
        let fixture = TempDir::new("shdeps-ui-render-summary").expect("fixture dir");
        let palette = marker_palette();
        let ui = plain_ui(&palette);
        let summaries: HashMap<Vec<u8>, Vec<u8>> = case.summaries.iter().cloned().collect();
        let items: HashMap<Vec<u8>, Vec<u8>> = case.items.iter().cloned().collect();
        let mut extra = Vec::new();
        if let Some(verbose) = case.verbose {
            extra.push(("DOT_VERBOSE", verbose));
        }
        if let Some(threshold) = case.threshold {
            extra.push(("DOT_UPDATE_SUBPHASE_THRESHOLD_MS", threshold));
        }
        let snippet = format!(
            "{}DOT_UI_LIVE_ACTIVE={}\n_shdeps_print_group_summaries\n",
            plant(&case.order, &HashMap::new(), &items, &summaries),
            if case.live { 1 } else { 0 },
        );
        let (code, out, err) = shell_run(fixture.path(), &[], &extra, &snippet);
        assert_eq!(code, 0, "harness exit for {}", case.name);
        if case.expect_stderr {
            assert!(
                !err.is_empty(),
                "missing arithmetic diagnostic for {}",
                case.name
            );
        } else {
            assert!(err.is_empty(), "harness stderr for {}", case.name);
        }
        let verbose = case.verbose == Some("1");
        let threshold = case.threshold.map(str::as_bytes);
        let (want, _) = print_group_summaries(
            &ui,
            case.live,
            verbose,
            threshold,
            &case.order,
            &summaries,
            &items,
        );
        assert_eq!(out, want, "summaries for {}", case.name);
    }
}
