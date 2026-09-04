//! Differential parity tests for the worker decision kernels
//! (`lib/dot/extension-worker.sh`) against the live shell: the
//! overlay-protocol whitelist, the merge/doctor API file lists, the
//! mode and entry-point mapping, the source-root and result-path
//! gates, the combined main precheck, and the deactivate retiring
//! set.
//!
//! The sourcing and execution boundary itself stays shell-side (the
//! hook-sourcing precedent): loaders source files and unset helpers,
//! and `_dot_extension_worker_main` sources client code and runs the
//! entry point. Only the pure decisions are ported, so each row
//! drives the same `case` / `[[ ]]` fragment on the shell side that
//! the Rust kernel mirrors.
//!
//! Separate binary because the rows build temp fixtures per side; the
//! harness pins `LC_ALL=C` with `env_clear` like
//! `tests/repos_pull_base.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::extension_worker::{self, DOCTOR_API_RELPATH, MERGE_API_RELPATHS, OVERLAY_PROTOCOL_KEEP};
use dot::test_support::TempDir;

/// Run one shell snippet with a pinned locale and a scrubbed
/// environment.
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(snippet);
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .current_dir(home)
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[test]
fn overlay_protocol_keep_agrees() {
    let dir = TempDir::new("worker-keep").expect("fixture dir");
    let home = dir.path();
    let mut candidates: Vec<String> = OVERLAY_PROTOCOL_KEEP
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    candidates.extend(
        [
            "",
            "merge",
            "doctor",
            "_overlay_link_target_extra",
            "_overlay_link_targe",
            "_overlay_checkout_matches2",
            "_overlay_is_worktre",
            "_OVERLAY_LINK_TARGET",
            "_overlay_parse_manifest_record ",
            " _overlay_link_target",
            "_overlay_origin_matche",
            "x_overlay_link_target",
            "_overlay_effective_ur",
        ]
        .iter()
        .map(|name| (*name).to_string()),
    );
    for candidate in &candidates {
        let snippet = format!(
            "name={}; case $name in _overlay_link_target | _overlay_private_regular_file | _overlay_parse_manifest_record | _overlay_manifest_safe | _overlay_is_worktree | _overlay_effective_url | _overlay_origin_matches | _overlay_checkout_matches) printf 'rc=0\\n';; *) printf 'rc=1\\n';; esac",
            sq(candidate),
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness keep {candidate:?}");
        let rust = extension_worker::overlay_protocol_keep(candidate);
        assert_eq!(
            format!("rc={}\n", i32::from(!rust)),
            String::from_utf8(out).expect("keep dump"),
            "keep code for {candidate:?}"
        );
        assert_eq!(serr, b"", "keep stderr for {candidate:?}");
    }
    assert_eq!(OVERLAY_PROTOCOL_KEEP.len(), 8, "whitelist length");
}

#[test]
fn protocol_survivors_agree() {
    let dir = TempDir::new("worker-survivors").expect("fixture dir");
    let home = dir.path();
    let before = vec![
        "keep_me".to_string(),
        "_overlay_link_target".to_string(),
        "old_helper".to_string(),
    ];
    let after = vec![
        "keep_me".to_string(),
        "_overlay_link_target".to_string(),
        "old_helper".to_string(),
        "_overlay_private_regular_file".to_string(),
        "new_helper".to_string(),
        "_overlay_checkout_matches".to_string(),
        "another_new".to_string(),
    ];
    let before_shell = before
        .iter()
        .map(|name| sq(name))
        .collect::<Vec<_>>()
        .join(" ");
    let after_shell = after
        .iter()
        .map(|name| sq(name))
        .collect::<Vec<_>>()
        .join(" ");
    let snippet = format!(
        "before=({before_shell}); after=({after_shell}); \
         survivors=(); for name in \"${{after[@]}}\"; do \
         is_before=1; for known in \"${{before[@]}}\"; do [[ $name == \"$known\" ]] && is_before=0; done; \
         if [[ $is_before -eq 0 ]]; then survivors+=(\"$name\"); continue; fi; \
         case $name in _overlay_link_target | _overlay_private_regular_file | _overlay_parse_manifest_record | _overlay_manifest_safe | _overlay_is_worktree | _overlay_effective_url | _overlay_origin_matches | _overlay_checkout_matches) survivors+=(\"$name\");; esac; done; \
         printf '%s\\n' \"${{survivors[@]}}\""
    );
    let (code, out, serr) = shell_run(home, &snippet);
    assert_eq!(code, 0, "shell harness survivors");
    assert_eq!(serr, b"", "survivors stderr");
    let shell_list = String::from_utf8(out)
        .expect("survivors dump")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let rust = extension_worker::protocol_survivors(&before, &after);
    assert_eq!(rust, shell_list, "survivor list");
    // A loader with no pre-existing functions keeps exactly the
    // whitelisted newcomers.
    let empty: Vec<String> = Vec::new();
    let newcomers = vec![
        "_overlay_effective_url".to_string(),
        "stray_helper".to_string(),
    ];
    let rust_empty = extension_worker::protocol_survivors(&empty, &newcomers);
    assert_eq!(rust_empty, vec!["_overlay_effective_url".to_string()]);
}

#[test]
fn api_file_lists_agree() {
    let dir = TempDir::new("worker-api").expect("fixture dir");
    let home = dir.path();
    let repo = env!("CARGO_MANIFEST_DIR");
    // Every listed file exists on both sides.
    for rel in MERGE_API_RELPATHS
        .iter()
        .chain(std::iter::once(&DOCTOR_API_RELPATH))
    {
        let snippet = format!(
            "if [[ -f {root}/{rel} ]]; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            root = sq(repo),
            rel = sq(rel),
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness api {rel:?}");
        let rust_exists = Path::new(repo).join(rel).is_file();
        assert_eq!(
            format!("rc={}\n", i32::from(!rust_exists)),
            String::from_utf8(out).expect("api dump"),
            "api existence for {rel:?}"
        );
        assert_eq!(serr, b"", "api stderr for {rel:?}");
    }
    // The joined Rust paths match the shell concatenation.
    let joined = extension_worker::merge_api_paths(repo);
    assert_eq!(joined.len(), MERGE_API_RELPATHS.len());
    for (path, rel) in joined.iter().zip(MERGE_API_RELPATHS.iter()) {
        assert_eq!(path, &PathBuf::from(format!("{repo}/{rel}")));
    }
    assert_eq!(
        extension_worker::doctor_api_path(repo),
        PathBuf::from(format!("{repo}/{DOCTOR_API_RELPATH}"))
    );
    // The Rust lists match the live shell file: extract the sourced
    // relpaths from each loader body and compare in order.
    let (code, out, serr) = shell_run(
        home,
        "awk '/^_dot_extension_worker_load_merge_api\\(\\)/{flag=1} flag{print} /^}/{if(flag)exit}' \"$DOT_SOURCE_ROOT/lib/dot/extension-worker.sh\" | sed -n 's|.*\\$DOT_SOURCE_ROOT/\\([^\"]*\\)\".*|\\1|p'",
    );
    assert_eq!(code, 0, "shell harness merge list");
    assert_eq!(serr, b"", "merge list stderr");
    let shell_merge = String::from_utf8(out)
        .expect("merge list")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(shell_merge, MERGE_API_RELPATHS.to_vec(), "merge list");
    let (code, out, serr) = shell_run(
        home,
        "awk '/^_dot_extension_worker_load_doctor_api\\(\\)/{flag=1} flag{print} /^}/{if(flag)exit}' \"$DOT_SOURCE_ROOT/lib/dot/extension-worker.sh\" | sed -n 's|.*\\$DOT_SOURCE_ROOT/\\([^\"]*\\)\".*|\\1|p'",
    );
    assert_eq!(code, 0, "shell harness doctor list");
    assert_eq!(serr, b"", "doctor list stderr");
    let shell_doctor = String::from_utf8(out)
        .expect("doctor list")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(shell_doctor, vec![DOCTOR_API_RELPATH.to_string()]);
}

#[test]
fn mode_and_entry_point_agree() {
    let dir = TempDir::new("worker-mode").expect("fixture dir");
    let home = dir.path();
    for mode in [
        "merge",
        "pre-sync",
        "deactivate",
        "doctor",
        "",
        "Merge",
        "MERGE",
        "pre_sync",
        " pre-sync",
        "pre-sync ",
        "unknown",
        "merge ",
        "docto",
        "deactivate ",
    ] {
        let snippet = format!(
            "mode={m}; case $mode in merge | pre-sync | deactivate | doctor) printf 'valid=1\\n';; *) printf 'valid=0\\n';; esac; entry=none; case $mode in merge) entry=merge;; pre-sync) entry=prepare;; deactivate) entry=deactivate;; doctor) entry=doctor;; esac; printf 'entry=%s\\n' \"$entry\"",
            m = sq(mode),
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness mode {mode:?}");
        assert_eq!(serr, b"", "mode stderr for {mode:?}");
        let dump = String::from_utf8(out).expect("mode dump");
        let rust = extension_worker::Mode::parse(mode);
        let want_valid = i32::from(rust.is_some());
        let want_entry = rust.map(|parsed| parsed.entry_point()).unwrap_or("none");
        assert_eq!(
            format!("valid={want_valid}\nentry={want_entry}\n"),
            dump,
            "mode dump for {mode:?}"
        );
        // The canonical spelling round-trips on valid modes.
        if let Some(parsed) = rust {
            assert_eq!(parsed.as_str(), mode);
        }
    }
}

#[test]
fn source_root_and_result_agree() {
    let dir = TempDir::new("worker-root").expect("fixture dir");
    let home = dir.path();
    let repo = env!("CARGO_MANIFEST_DIR");
    let afile = home.join("afile");
    std::fs::write(&afile, b"x").expect("write afile");
    let afile_text = afile.to_string_lossy().into_owned();
    std::os::unix::fs::symlink("lib", home.join("linklib")).expect("symlink");
    let candidates = vec![
        repo.to_string(),
        String::new(),
        "relative".to_string(),
        "/nonexistent-dot-root".to_string(),
        afile_text.clone(),
        "/".to_string(),
        format!("{repo}/"),
    ];
    for root in &candidates {
        let snippet = format!(
            "root={r}; case $root in /*) shape=0;; *) shape=1;; esac; if [[ -d $root/lib/dot && ! -L $root/lib/dot ]]; then lib=0; else lib=1; fi; printf 'shape=%s\\nlib=%s\\n' \"$shape\" \"$lib\"",
            r = sq(root),
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness root {root:?}");
        assert_eq!(serr, b"", "root stderr for {root:?}");
        let dump = String::from_utf8(out).expect("root dump");
        let want_shape = i32::from(!extension_worker::source_root_shape_ok(root));
        let want_lib = i32::from(!extension_worker::lib_dot_dir_ok(root));
        assert_eq!(
            format!("shape={want_shape}\nlib={want_lib}\n"),
            dump,
            "root dump for {root:?}"
        );
        assert_eq!(
            extension_worker::source_root_valid(root),
            extension_worker::source_root_shape_ok(root) && extension_worker::lib_dot_dir_ok(root),
            "combined root for {root:?}"
        );
    }
    // A symlinked `lib/dot` refuses on both sides.
    let fake = TempDir::new("worker-fakeroot").expect("fake root");
    let fake_root = fake.path().join("root");
    std::fs::create_dir_all(fake_root.join("real")).expect("fake dirs");
    std::os::unix::fs::symlink("real", fake_root.join("lib")).expect("symlink lib");
    let fake_text = fake_root.to_string_lossy().into_owned();
    assert!(!extension_worker::lib_dot_dir_ok(&fake_text));
    let snippet = format!(
        "root={}; if [[ -d $root/lib/dot && ! -L $root/lib/dot ]]; then printf 'lib=0\\n'; else printf 'lib=1\\n'; fi",
        sq(&fake_text),
    );
    let (code, out, _) = shell_run(home, &snippet);
    assert_eq!(code, 0, "shell harness symlinked lib");
    assert_eq!(
        String::from_utf8(out).expect("symlink dump"),
        "lib=1\n",
        "symlinked lib dump"
    );
    // Result paths: only the empty string refuses.
    for result in ["", "x", "/tmp/result", " "] {
        let snippet = format!(
            "result={}; if [[ -n $result ]]; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            sq(result),
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness result {result:?}");
        assert_eq!(serr, b"", "result stderr for {result:?}");
        assert_eq!(
            format!(
                "rc={}\n",
                i32::from(!extension_worker::result_path_valid(result))
            ),
            String::from_utf8(out).expect("result dump"),
            "result dump for {result:?}"
        );
    }
}

#[test]
fn main_precheck_agrees() {
    let dir = TempDir::new("worker-precheck").expect("fixture dir");
    let home = dir.path();
    let repo = env!("CARGO_MANIFEST_DIR");
    // (label, argc, mode, root override, result, want code)
    let rows: Vec<(&str, usize, String, String, String, i32)> = vec![
        (
            "good-merge",
            5,
            "merge".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            0,
        ),
        (
            "good-presync",
            5,
            "pre-sync".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            0,
        ),
        (
            "good-deactivate",
            5,
            "deactivate".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            0,
        ),
        (
            "good-doctor",
            5,
            "doctor".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            0,
        ),
        (
            "argc-low",
            4,
            "merge".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            2,
        ),
        (
            "argc-high",
            6,
            "merge".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            2,
        ),
        (
            "argc-zero",
            0,
            "merge".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            2,
        ),
        (
            "bad-mode",
            5,
            "unknown".to_string(),
            repo.to_string(),
            "/tmp/r".to_string(),
            2,
        ),
        (
            "empty-mode",
            5,
            String::new(),
            repo.to_string(),
            "/tmp/r".to_string(),
            2,
        ),
        (
            "empty-root",
            5,
            "merge".to_string(),
            String::new(),
            "/tmp/r".to_string(),
            1,
        ),
        (
            "relative-root",
            5,
            "merge".to_string(),
            "relative".to_string(),
            "/tmp/r".to_string(),
            1,
        ),
        (
            "missing-root",
            5,
            "merge".to_string(),
            "/nonexistent-dot-root".to_string(),
            "/tmp/r".to_string(),
            1,
        ),
        (
            "empty-result",
            5,
            "merge".to_string(),
            repo.to_string(),
            String::new(),
            1,
        ),
        (
            "argc-beats-root",
            4,
            "merge".to_string(),
            String::new(),
            String::new(),
            2,
        ),
        (
            "mode-beats-root",
            5,
            "unknown".to_string(),
            String::new(),
            String::new(),
            2,
        ),
    ];
    for (label, argc, mode, root, result, want) in &rows {
        let snippet = format!(
            "argc={a}; mode={m}; root={r}; result={t}; \
             if [[ $argc -ne 5 ]]; then printf 'rc=2\\n'; \
             else case $mode in merge | pre-sync | deactivate | doctor) ;; *) printf 'rc=2\\n'; exit 0;; esac; \
             case $root in /*) ;; *) printf 'rc=1\\n'; exit 0;; esac; \
             if [[ ! -d $root/lib/dot || -L $root/lib/dot ]]; then printf 'rc=1\\n'; exit 0; fi; \
             if [[ -z $result ]]; then printf 'rc=1\\n'; else printf 'rc=0\\n'; fi; fi",
            a = argc,
            m = sq(mode),
            r = sq(root),
            t = sq(result),
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness precheck {label}");
        assert_eq!(serr, b"", "precheck stderr for {label}");
        let shell_dump = String::from_utf8(out).expect("precheck dump");
        let rust = extension_worker::main_precheck(*argc, mode, root, result);
        let rcode = match rust {
            Ok(_) => 0,
            Err(error) => error.code(),
        };
        assert_eq!(rcode, *want, "want code for {label}");
        assert_eq!(
            format!("rc={rcode}\n"),
            shell_dump,
            "precheck dump for {label}"
        );
    }
}

#[test]
fn deactivate_set_agrees() {
    let dir = TempDir::new("worker-deactset").expect("fixture dir");
    let home = dir.path();
    for (kind, count) in [
        ("retiring", 1),
        ("retiring", 0),
        ("retiring", 2),
        ("active", 1),
        ("", 1),
        ("retiring ", 1),
        ("RETIRING", 1),
        ("eligible", 1),
    ] {
        let snippet = format!(
            "kind={k}; count={c}; if [[ $kind == retiring && $count -eq 1 ]]; then printf 'rc=0\\n'; else printf 'rc=1\\n'; fi",
            k = sq(kind),
            c = count,
        );
        let (code, out, serr) = shell_run(home, &snippet);
        assert_eq!(code, 0, "shell harness set {kind:?}/{count}");
        assert_eq!(serr, b"", "set stderr for {kind:?}/{count}");
        let rust = extension_worker::deactivate_set_valid(kind, count);
        assert_eq!(
            format!("rc={}\n", i32::from(!rust)),
            String::from_utf8(out).expect("set dump"),
            "set dump for {kind:?}/{count}"
        );
    }
}
