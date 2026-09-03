//! Differential parity tests for merge orchestration helpers against
//! `lib/dot/merges.sh`: label derivation, serial detection, job
//! counts, summaries, and result prefixes. The parallel batch runner
//! and worker capture stay shell until the progress-UI, worker, and
//! overlay-context slices land.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use dot::merges;
use dot::test_support::TempDir;

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
        ". \"$1/lib/dot/merges.sh\"\n. \"$1/lib/dot/update.sh\"\n{snippet}"
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
