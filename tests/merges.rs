//! Differential parity tests for merge orchestration helpers against
//! `lib/dot/merges.sh`: label derivation, serial detection, job
//! counts, summaries, result prefixes, progress details, hook-spec
//! collection, and the merge-result parse plus render halves with
//! the capture decision kernel. The parallel batch runner, worker
//! capture, and top-level `_run_merges` stay shell (process-group
//! orchestration), as does the spec-discovery trust envelope.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::merges;
use dot::progress_ui::Palette;
use dot::test_support::TempDir;

/// Progress-detail parity row: label, done, total, label width, bar
/// width (`None` widths take the shell `:-` defaults).
type ProgressRow<'a> = (&'a str, i64, i64, Option<&'a str>, Option<&'a str>);

/// Capture parity row: has_merge, rc, and elapsed record bytes
/// (`None` leaves the file absent), verbosity knobs, log bytes.
type CaptureRow<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
    &'a str,
    &'a str,
);

/// Run one shell snippet with `merges.sh` sourced. `argv` arrives as
/// `$2..`; `extra_env` sets (`Some`) or removes (`None`) variables.
fn shell_run(
    fixture: &Path,
    argv: &[&std::ffi::OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/merges.sh\"\n. \"$1/lib/dot/update.sh\"\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", fixture)
        .current_dir(fixture)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in extra_env {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
    let output = cmd.output().expect("spawn bash");
    (output.status.code().unwrap_or(99), output.stdout)
}

#[test]
fn trim_cases_agree() {
    let dir = TempDir::new("merges-trim").expect("fixture dir");
    let cases = [
        "  padded  ",
        "\t\ttabs\n",
        "",
        "   ",
        "no-pad",
        "inner  space",
        "line\nbreak",
    ];
    for value in cases {
        let (code, out) = shell_run(dir.path(), &[value.as_ref()], &[], "_merge_trim \"$2\"");
        assert_eq!(code, 0, "shell trim {value:?}");
        let shell = String::from_utf8(out).expect("trim text");
        assert_eq!(merges::trim(value), shell, "trim parity for {value:?}");
    }
}

#[test]
fn label_cases_agree() {
    let dir = TempDir::new("merges-label").expect("fixture dir");
    let cases = [
        "10-foo.sh",
        "02_ssh.serial.sh",
        "plain.sh",
        "noext",
        "/hooks/20-bar.sh",
        "10-.sh",
        "9-x",
        "1_2-3.sh",
        "-x.sh",
        "1x.sh",
        "007-z.serial.sh",
        "10-UPPER.sh",
        "10-.serial.sh",
    ];
    for script in cases {
        let (code, out) = shell_run(
            dir.path(),
            &[script.as_ref()],
            &[],
            "_merge_label_from_script \"$2\"",
        );
        assert_eq!(code, 0, "shell label {script:?}");
        let shell = String::from_utf8(out).expect("label text");
        let rust = merges::label_from_script(OsStr::new(script));
        assert_eq!(rust.to_string_lossy(), shell, "label parity for {script:?}");
    }
}

#[test]
fn serial_cases_agree() {
    let dir = TempDir::new("merges-serial").expect("fixture dir");
    let cases = [
        ("10-a.serial.sh", true),
        ("10-a.sh", false),
        (".serial.sh", true),
        ("serial.sh", false),
        ("10-a.SERIAL.SH", false),
        ("x.serial.sh.bak", false),
    ];
    for (script, expected) in cases {
        let (code, out) = shell_run(
            dir.path(),
            &["key".as_ref(), script.as_ref()],
            &[],
            "_merge_hook_is_serial \"$2\" \"$3\"; printf '%s' \"$?\"",
        );
        assert_eq!(code, 0, "shell serial {script:?}");
        let shell = String::from_utf8(out).expect("serial text");
        assert_eq!(
            merges::is_serial(script),
            expected,
            "rust serial {script:?}"
        );
        assert_eq!(
            merges::is_serial(script),
            shell == "0",
            "serial parity for {script:?}"
        );
    }
}

#[test]
fn jobs_cases_agree() {
    let dir = TempDir::new("merges-jobs").expect("fixture dir");
    let cases = [
        ("", ""),
        ("4", ""),
        ("0", ""),
        ("00", ""),
        ("007", ""),
        ("abc", ""),
        (" 4", ""),
        ("4 ", ""),
        ("", "8"),
        ("3", "8"),
        ("0", "0"),
        ("", "bogus"),
        ("2", "bogus"),
    ];
    for (merge_jobs, update_jobs) in cases {
        let env = [
            ("DOT_MERGE_JOBS", Some(merge_jobs)),
            ("DOT_UPDATE_JOBS", Some(update_jobs)),
        ];
        let (code, out) = shell_run(dir.path(), &[], &env, "_merge_parallel_jobs");
        assert_eq!(code, 0, "shell jobs {merge_jobs:?}/{update_jobs:?}");
        let shell = String::from_utf8(out).expect("jobs text");
        // Both sides take the pinned knobs directly; no process env
        // mutation is needed on either side.
        let rust = merges::parallel_jobs(merge_jobs, update_jobs);
        assert_eq!(
            format!("{rust}\n"),
            shell,
            "jobs parity for {merge_jobs:?}/{update_jobs:?}"
        );
    }
}

#[test]
fn cpu_count_kernel_agrees() {
    // Kernel table: (getconf, uname, sysctl) -> count. The live
    // row below runs the real `getconf` on this machine.
    let live_getconf = String::from_utf8(
        Command::new("getconf")
            .arg("_NPROCESSORS_ONLN")
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    let live_getconf = live_getconf.trim();
    assert_eq!(
        merges::cpu_count_select(live_getconf, "Linux", ""),
        merges::cpu_count(),
        "live cpu chain"
    );
    assert!(!merges::cpu_count().is_empty(), "cpu count prints");
    for (getconf, uname, sysctl, expected) in [
        ("8", "Linux", "", "8"),
        ("", "Linux", "", "4"),
        ("", "Darwin", "10", "10"),
        ("", "Darwin", "", "4"),
        ("bogus", "Linux", "", "4"),
        ("0", "Linux", "", "1"),
        ("00", "Darwin", "8", "1"),
        ("", "Darwin", "0", "1"),
        ("16", "Darwin", "4", "16"),
    ] {
        assert_eq!(
            merges::cpu_count_select(getconf, uname, sysctl),
            expected,
            "cpu kernel for {getconf:?}/{uname}/{sysctl:?}"
        );
    }
}

#[test]
fn summaries_agree() {
    let dir = TempDir::new("merges-summary").expect("fixture dir");
    for count in [0, 1, 2, 17] {
        let text = count.to_string();
        let (code, out) = shell_run(
            dir.path(),
            &[text.as_ref()],
            &[],
            "_merge_summary \"$2\"; printf '|'; _merge_failure_summary \"$2\"; printf '|'; _merge_warning_summary \"$2\" 1",
        );
        assert_eq!(code, 0, "shell summaries {count}");
        let shell = String::from_utf8(out).expect("summary text");
        let rust = format!(
            "{}|{}|{}",
            merges::summary(count),
            merges::failure_summary(count),
            merges::warning_summary(count, 1)
        );
        assert_eq!(rust, shell, "summary parity for {count}");
    }
    assert_eq!(
        merges::warning_summary(5, 0),
        "5 configs merged, 0 config hooks failed"
    );
    assert_eq!(
        merges::warning_summary(1, 1),
        "0 configs merged, 1 config hook failed"
    );
}

#[test]
fn result_prefix_agrees() {
    let dir = TempDir::new("merges-prefix").expect("fixture dir");
    for index in [0, 1, 42, 999, 1000, 12345] {
        let text = index.to_string();
        let (code, out) = shell_run(
            dir.path(),
            &["/results".as_ref(), text.as_ref()],
            &[],
            "_merge_result_prefix \"$2\" \"$3\"",
        );
        assert_eq!(code, 0, "shell prefix {index}");
        let shell = String::from_utf8(out).expect("prefix text");
        assert_eq!(
            merges::result_prefix("/results", index),
            shell,
            "prefix parity for {index}"
        );
    }
}

#[test]
fn progress_detail_cases_agree() {
    let dir = TempDir::new("merges-progress").expect("fixture dir");
    // (label, done, total, label width, bar width); `None` widths
    // exercise the shell `:-` defaults. `LC_ALL=C` forces the ASCII
    // bar and byte-counted cells on the shell side, matching
    // `ascii=true, multibyte=false`.
    let cases: [ProgressRow<'_>; 7] = [
        ("ssh", 1, 4, None, None),
        ("overlays", 2, 5, Some("10"), Some("12")),
        ("", 0, 3, None, None),
        ("done-hook", 5, 5, None, None),
        ("overfull", 9, 5, None, None),
        ("uni-hööks", 1, 2, None, None),
        ("zero-total", 1, 0, None, None),
    ];
    for (label, done, total, label_width, bar_width) in cases {
        let env = [
            ("DOT_UI_PROGRESS_LABEL_WIDTH", label_width),
            ("DOT_UI_PROGRESS_WIDTH", bar_width),
        ];
        let done_text = done.to_string();
        let total_text = total.to_string();
        let (code, out) = shell_run(
            dir.path(),
            &[label.as_ref(), done_text.as_ref(), total_text.as_ref()],
            &env,
            "_merge_progress_detail \"$3\" \"$4\" \"$2\"",
        );
        assert_eq!(code, 0, "shell progress {label:?}");
        assert_eq!(
            merges::progress_detail(
                label.as_bytes(),
                done,
                total,
                label_width.unwrap_or("18"),
                bar_width.unwrap_or("8"),
                true,
                false,
            ),
            out,
            "progress parity for {label:?}",
        );
    }
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

#[test]
fn result_label_cases_agree() {
    let dir = TempDir::new("merges-result-label").expect("fixture dir");
    // (script, log bytes, status, elapsed ms). The stubbed `_ui_item`
    // / `_ui_detail` expose exactly the label, detail rows, and
    // duration the shell resolved.
    let cases: [(&str, &str, &str, i64); 8] = [
        (
            "10-foo.sh",
            "Friendly Name\nsecond line\nthird\n",
            "ok",
            250,
        ),
        (
            "02_ssh.serial.sh",
            "\n  \nSpaced Label  \n  detail one\n\n",
            "warning",
            1500,
        ),
        ("plain.sh", "", "ok", 0),
        ("10-UPPER.sh", "   \t  \n", "ok", 10500),
        ("noext", "only\n", "ok", 999),
        ("a.sh", "line without trailing newline", "warning", 10000),
        ("b.sh", "l1\r\nl2\r\n", "ok", 5),
        ("c.sh", "hüüks target\n detail \n", "ok", 42),
    ];
    let snippet = concat!(
        "_ui_item() { printf 'ITEM|%s|%s|%s\\n' \"$1\" \"$2\" \"$3\"; }\n",
        "_ui_detail() { printf 'DETAIL|%s\\n' \"$1\"; }\n",
        "_print_merge_result \"$2\" \"$3\" \"$4\" \"$5\"",
    );
    for (index, (script, log, status, elapsed)) in cases.iter().enumerate() {
        let log_path = stage(dir.path(), &format!("hook-{index}.log"), log.as_bytes());
        let elapsed_text = elapsed.to_string();
        let (code, out) = shell_run(
            dir.path(),
            &[
                script.as_ref(),
                elapsed_text.as_ref(),
                log_path.as_os_str(),
                status.as_ref(),
            ],
            &[],
            snippet,
        );
        assert_eq!(code, 0, "shell result {script:?}");
        let shell = String::from_utf8(out).expect("result text");
        let (label, details) = merges::result_label(OsStr::new(script), log);
        let duration =
            String::from_utf8(dot::progress_ui::duration_ms(*elapsed)).expect("duration");
        let mut expected = format!("ITEM|{status}|{}|{duration}\n", label.to_string_lossy());
        for line in &details {
            expected.push_str(&format!("DETAIL|{line}\n"));
        }
        assert_eq!(expected, shell, "result label parity for {script:?}");
    }
}

/// Marker palette matching the render-test preamble below.
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

#[test]
fn render_result_cases_agree() {
    let dir = TempDir::new("merges-render").expect("fixture dir");
    // (script, log bytes, status, elapsed ms): label fallback, empty
    // details, and every duration branch. `DOT_QUIET` stays unset so
    // the shell rows print, matching `quiet=false`.
    let cases: [(&str, &str, &str, i64); 4] = [
        (
            "10-foo.sh",
            "Friendly Name\nsecond line\nthird\n",
            "ok",
            250,
        ),
        ("10-UPPER.sh", "   \n", "warning", 10500),
        ("plain.sh", "solo\n", "ok", 999),
        ("b.sh", "l1\r\nl2\r\n", "warning", 1500),
    ];
    let snippet = concat!(
        "_C_RESET='<R>' _C_BOLD='<B>' _C_DIM='<D>' _C_GREEN='<G>' ",
        "_C_YELLOW='<Y>' _C_RED='<E>' _C_BLUE='<U>' _C_CYAN='<C>' _C_WHITE='<W>'\n",
        "_print_merge_result \"$2\" \"$3\" \"$4\" \"$5\"",
    );
    for (index, (script, log, status, elapsed)) in cases.iter().enumerate() {
        let log_path = stage(dir.path(), &format!("render-{index}.log"), log.as_bytes());
        let elapsed_text = elapsed.to_string();
        let (code, out) = shell_run(
            dir.path(),
            &[
                script.as_ref(),
                elapsed_text.as_ref(),
                log_path.as_os_str(),
                status.as_ref(),
            ],
            &[],
            snippet,
        );
        assert_eq!(code, 0, "shell render {script:?}");
        let (label, details) = merges::result_label(OsStr::new(script), log);
        let detail_bytes: Vec<Vec<u8>> = details
            .iter()
            .map(|line| line.as_bytes().to_vec())
            .collect();
        use std::os::unix::ffi::OsStrExt as _;
        let (rust, live) = merges::render_result(
            &marker_palette(),
            false,
            false,
            status.as_bytes(),
            label.as_os_str().as_bytes(),
            *elapsed,
            &detail_bytes,
            false,
        );
        assert!(!live, "render leaves live inactive for {script:?}");
        assert_eq!(rust, out, "render parity for {script:?}");
    }
}

/// Render one [`merges::CaptureAction`] the way the stubbed shell
/// branch printers do, with the shell exit code.
fn expected_capture(action: &merges::CaptureAction, key: &str, prefix: &str) -> (String, i32) {
    match action {
        merges::CaptureAction::Skipped => (String::new(), 1),
        merges::CaptureAction::ShowResult {
            warning,
            elapsed_ms,
        } => (
            format!(
                "RESULT|{key}|{elapsed_ms}|{prefix}.log|{}\n",
                if *warning { "warning" } else { "ok" },
            ),
            0,
        ),
        merges::CaptureAction::ShowLogWarning => (
            format!("LOGFILE|{key}|{prefix}.log\nWARN|  warning: merge failed\n"),
            0,
        ),
        merges::CaptureAction::ShowEmptyWarning => {
            (format!("WARN|  warning: merge failed: {key}\n"), 0)
        }
        merges::CaptureAction::Silent { .. } => (String::new(), 0),
    }
}

#[test]
fn capture_cases_agree() {
    let dir = TempDir::new("merges-capture").expect("fixture dir");
    let results = dir.path().join("results");
    std::fs::create_dir_all(&results).expect("results dir");
    let prefix = merges::result_prefix(results.to_str().expect("utf8"), 7);
    // (has_merge, rc, elapsed, verbose, quiet, log bytes). `None`
    // record contents leave the file absent, taking shell defaults.
    let cases: [CaptureRow<'_>; 18] = [
        (None, None, None, "0", "0", ""),
        (Some("0"), Some("0"), Some("0"), "0", "0", ""),
        (Some("1"), Some("0"), Some("0"), "0", "0", ""),
        (Some("1"), Some("0"), Some("42"), "1", "0", ""),
        (Some("1"), Some("1"), Some("250"), "0", "0", "boom\n"),
        (Some("1"), Some("1"), Some("250"), "0", "0", ""),
        (Some("1"), Some("2"), Some("1500"), "1", "0", "boom\n"),
        (Some("1"), None, Some("0"), "0", "0", "x\n"),
        (Some("1"), Some("bogus"), Some("7"), "0", "0", "x\n"),
        (Some("1\n"), Some("0\n"), Some("9\n"), "0", "0", ""),
        (Some("1"), Some("1"), Some("0"), "1", "1", "x\n"),
        (Some("1"), Some("0"), None, "1", "0", ""),
        (Some("2"), Some("0"), Some("0"), "1", "0", ""),
        (Some("1"), Some("00"), Some("0"), "0", "0", ""),
        (Some("1"), Some("3"), Some("5"), "bogus", "0", ""),
        (Some("1"), Some("0"), Some("0"), "1", "bogus", ""),
        (Some("1"), Some("1abc"), Some("7"), "0", "0", "x\n"),
        (Some("1"), Some("1abc"), Some("7"), "1", "0", ""),
    ];
    let snippet = concat!(
        "_print_merge_result() { printf 'RESULT|%s|%s|%s|%s\\n' \"$1\" \"$2\" \"$3\" \"$4\"; }\n",
        "_logfile_print() { printf 'LOGFILE|%s|%s\\n' \"$1\" \"$2\"; }\n",
        "_warn() { printf 'WARN|%s\\n' \"$1\"; }\n",
        "code=0\n",
        "_print_merge_capture \"$2\" \"$3\" \"$4\" || code=$?\n",
        "printf 'rc=%s\\n' \"$code\"",
    );
    for (index, (has_merge, rc, elapsed, verbose, quiet, log)) in cases.iter().enumerate() {
        for (suffix, content) in [
            ("has_merge", has_merge),
            ("rc", rc),
            ("elapsed_ms", elapsed),
        ] {
            let path = std::path::PathBuf::from(format!("{prefix}.{suffix}"));
            match content {
                Some(text) => std::fs::write(&path, text.as_bytes()).expect("record write"),
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        std::fs::write(format!("{prefix}.log"), log.as_bytes()).expect("log write");
        let env = [("DOT_VERBOSE", Some(*verbose)), ("DOT_QUIET", Some(*quiet))];
        let (code, out) = shell_run(
            dir.path(),
            &["10-foo".as_ref(), "7".as_ref(), results.as_os_str()],
            &env,
            snippet,
        );
        assert_eq!(code, 0, "shell capture harness {index}");
        let shell = String::from_utf8(out).expect("capture text");
        let action =
            merges::capture_action(*has_merge, *rc, *elapsed, verbose, quiet, !log.is_empty());
        let (mut expected, want_rc) = expected_capture(&action, "10-foo", &prefix);
        expected.push_str(&format!("rc={want_rc}\n"));
        assert_eq!(
            shell, expected,
            "capture parity for row {index} ({action:?})"
        );
    }
}

/// Byte-ordered `*.sh` names under `hooks`, like the shell glob
/// under `LC_ALL=C` (`*` never matches dotfiles).
fn glob_names(hooks: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    for entry in std::fs::read_dir(hooks).expect("read hooks") {
        let name = entry.expect("dir entry").file_name();
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = name.as_os_str().as_bytes();
        if bytes.starts_with(b".") || !bytes.ends_with(b".sh") {
            continue;
        }
        names.push(bytes.to_vec());
    }
    names.sort();
    names
        .iter()
        .map(|name| {
            use std::os::unix::ffi::OsStringExt as _;
            std::ffi::OsString::from_vec(name.clone())
        })
        .collect()
}

#[test]
fn hook_specs_cases_agree() {
    // Production runs under `pipefail`, so enable it here too: a
    // discovery error must surface as rc 1. Trust validation is
    // stubbed — the kernel under test is keys, identities,
    // duplicates, and the C-locale sort.
    let snippet = concat!(
        "set -o pipefail\n",
        "_dot_extensions_enabled() { return 0; }\n",
        "_dot_extension_root_validate() { return 0; }\n",
        "_dot_extension_directory_validate() { return 0; }\n",
        "_dot_extension_file_validate() { return 0; }\n",
        "_merge_hook_specs; printf 'rc=%s\\n' \"$?\"",
    );
    // Valid set: numeric prefixes sort as bytes (`10-aaa` before
    // `9-zzz`), serials key without `.serial`.
    let dir = TempDir::new("merges-specs").expect("fixture dir");
    let hooks = dir.path().join("ext").join("merge-hooks.d");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    for name in [
        "10-foo.sh",
        "9-zzz.sh",
        "10-aaa.sh",
        "20-bar.serial.sh",
        "plain.sh",
    ] {
        std::fs::write(hooks.join(name), b"# hook\n").expect("hook file");
    }
    let ext = dir.path().join("ext");
    let env = [("DOT_EXTENSIONS_DIR", Some(ext.to_str().expect("utf8")))];
    let (code, out) = shell_run(dir.path(), &[], &env, snippet);
    assert_eq!(code, 0, "shell specs harness");
    let shell = String::from_utf8(out).expect("specs text");
    let names = glob_names(&hooks);
    let full: Vec<std::ffi::OsString> = names
        .iter()
        .map(|name| hooks.join(name).into_os_string())
        .collect();
    let refs: Vec<&OsStr> = full.iter().map(|path| path.as_os_str()).collect();
    let specs = merges::collect_specs(&refs).expect("rust specs");
    let mut expected = String::new();
    for (key, script) in &specs {
        expected.push_str(&format!(
            "{}\t{}\n",
            key.to_string_lossy(),
            script.to_string_lossy()
        ));
    }
    expected.push_str("rc=0\n");
    assert_eq!(shell, expected, "specs parity");
    // Empty hook dir: no specs, still success.
    let empty = TempDir::new("merges-specs-empty").expect("fixture dir");
    let empty_hooks = empty.path().join("ext").join("merge-hooks.d");
    std::fs::create_dir_all(&empty_hooks).expect("empty hooks dir");
    let empty_ext = empty.path().join("ext");
    let empty_env = [(
        "DOT_EXTENSIONS_DIR",
        Some(empty_ext.to_str().expect("utf8")),
    )];
    let (code, out) = shell_run(empty.path(), &[], &empty_env, snippet);
    assert_eq!(code, 0, "shell empty specs harness");
    assert_eq!(
        String::from_utf8(out).expect("empty specs text"),
        "rc=0\n",
        "empty specs parity"
    );
    assert_eq!(
        merges::collect_specs(&[]).expect("rust empty specs"),
        Vec::new(),
        "rust empty specs"
    );
    // Invalid identity aborts before any later line: `BAD.sh` sorts
    // first in byte order, so no partial spec line precedes it.
    let bad = TempDir::new("merges-specs-bad").expect("fixture dir");
    let bad_hooks = bad.path().join("ext").join("merge-hooks.d");
    std::fs::create_dir_all(&bad_hooks).expect("bad hooks dir");
    for name in ["BAD.sh", "zzz.sh"] {
        std::fs::write(bad_hooks.join(name), b"# hook\n").expect("hook file");
    }
    let bad_ext = bad.path().join("ext");
    let bad_env = [("DOT_EXTENSIONS_DIR", Some(bad_ext.to_str().expect("utf8")))];
    let (code, out) = shell_run(bad.path(), &[], &bad_env, snippet);
    assert_eq!(code, 0, "shell bad specs harness");
    assert_eq!(
        String::from_utf8(out).expect("bad specs text"),
        "rc=1\n",
        "invalid identity aborts"
    );
    let bad_names = glob_names(&bad_hooks);
    let bad_full: Vec<std::ffi::OsString> = bad_names
        .iter()
        .map(|name| bad_hooks.join(name).into_os_string())
        .collect();
    let bad_refs: Vec<&OsStr> = bad_full.iter().map(|path| path.as_os_str()).collect();
    let error = merges::collect_specs(&bad_refs).expect_err("rust rejects BAD.sh");
    assert_eq!(
        error,
        merges::SpecError::InvalidIdentity(OsStr::new("BAD.sh").to_os_string()),
        "invalid identity names the basename"
    );
    assert_eq!(
        error.to_string(),
        "dot: invalid merge-hook identity: BAD.sh",
        "invalid identity message"
    );
    // Duplicate identity: the keeper line streams out before the
    // abort, so the shell shows one sorted partial line plus rc 1.
    let dup = TempDir::new("merges-specs-dup").expect("fixture dir");
    let dup_hooks = dup.path().join("ext").join("merge-hooks.d");
    std::fs::create_dir_all(&dup_hooks).expect("dup hooks dir");
    for name in ["10-foo.sh", "foo.sh"] {
        std::fs::write(dup_hooks.join(name), b"# hook\n").expect("hook file");
    }
    let dup_ext = dup.path().join("ext");
    let dup_env = [("DOT_EXTENSIONS_DIR", Some(dup_ext.to_str().expect("utf8")))];
    let (code, out) = shell_run(dup.path(), &[], &dup_env, snippet);
    assert_eq!(code, 0, "shell dup specs harness");
    let keeper = dup_hooks.join("10-foo.sh");
    assert_eq!(
        String::from_utf8(out).expect("dup specs text"),
        format!("10-foo\t{}\nrc=1\n", keeper.display()),
        "duplicate streams the keeper then aborts"
    );
    let dup_names = glob_names(&dup_hooks);
    let dup_full: Vec<std::ffi::OsString> = dup_names
        .iter()
        .map(|name| dup_hooks.join(name).into_os_string())
        .collect();
    let dup_refs: Vec<&OsStr> = dup_full.iter().map(|path| path.as_os_str()).collect();
    let error = merges::collect_specs(&dup_refs).expect_err("rust rejects duplicates");
    assert_eq!(
        error,
        merges::SpecError::DuplicateIdentity(OsStr::new("foo").to_os_string()),
        "duplicate names the identity"
    );
    assert_eq!(
        error.to_string(),
        "dot: duplicate merge-hook identity: foo",
        "duplicate message"
    );
}
