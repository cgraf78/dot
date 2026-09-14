//! Differential parity tests for merge-hook mechanics against
//! `lib/dot/merge-hooks.sh`: hook root resolution, family discovery
//! and markers, home expansion, text writes, and the `jq` JSON layer
//! (including warnings and corrupt-destination rebuilds).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::merge_hooks;
use dot::temp;
use dot::test_support::TempDir;

/// Run one shell snippet with the merge-hook libraries sourced.
/// `argv` arrives as `$2..`; `extra_env` sets (`Some`) or removes
/// (`None`) variables. Returns exit code, stdout, and stderr.
fn shell_run(
    fixture: &Path,
    argv: &[&std::ffi::OsStr],
    extra_env: &[(&str, Option<&str>)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/temp.sh\"\n. \"$1/lib/dot/families.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/merge-hooks.sh\"\n{snippet}"
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
        .stderr(Stdio::piped());
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
    (
        output.status.code().unwrap_or(99),
        output.stdout,
        output.stderr,
    )
}

/// Drop bash's own exec-failure notice (`file: line N: jq: command
/// not found`) from shell stderr before comparing. It is shell
/// interpreter noise, not engine output: the path and line number
/// shift with the source file, so no port can reproduce it. `jq`'s
/// own diagnostics (when `jq` runs) still compare verbatim.
fn without_exec_noise<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines
        .filter(|line| !line.contains(": jq: command not found"))
        .map(str::to_string)
        .collect()
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Sandbox HOME at `home` with `XDG_CONFIG_HOME` removed, on both sides.
fn home_env(home: &Path) -> Vec<(&str, Option<&str>)> {
    vec![
        ("HOME", Some(home.as_os_str().to_str().expect("utf8 home"))),
        ("XDG_CONFIG_HOME", None),
    ]
}

#[test]
fn hook_paths_agree() {
    let dir = TempDir::new("mh-paths").expect("fixture dir");
    let root = dir.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let env = home_env(&home);
    let env_refs: Vec<(&str, Option<&str>)> = env.to_vec();
    for xdg in ["", "/custom/xdg"] {
        let mut env = env_refs.clone();
        env.push(("XDG_CONFIG_HOME", Some(xdg)));
        // `_merge_hook_dir` prints the path itself.
        let (code, out, _) = shell_run(root, &[], &env, "_merge_hook_dir");
        assert_eq!(code, 0, "shell hook dir xdg={xdg:?}");
        let shell = String::from_utf8(out).expect("dir text");
        let rust = merge_hooks::hook_dir(xdg, home.to_str().expect("utf8")).expect("rust hook dir");
        assert_eq!(
            format!("{}\n", rust.display()),
            shell,
            "hook dir for xdg={xdg:?}"
        );
    }
    // Source and family joins, plus the ordered file stream.
    let hooks = merge_hooks::hook_dir("", home.to_str().expect("utf8")).expect("hooks root");
    let fam = hooks.join("ssh");
    stage(
        root,
        "home/.config/dot/merge-hooks.d/ssh/10-base",
        b"Host a\n",
    );
    stage(
        root,
        "home/.config/dot/merge-hooks.d/ssh/20-extra",
        b"Host b\n",
    );
    stage(
        root,
        "home/.config/dot/merge-hooks.d/ssh/30-group.replace/10-winner",
        b"Host c\n",
    );
    stage(
        root,
        "home/.config/dot/merge-hooks.d/ssh/README",
        b"notes\n",
    );
    let (code, out, _) = shell_run(
        root,
        &[],
        &env_refs,
        "printf '%s\\n' \"$(_merge_hook_source ssh)\"; printf '%s\\n' \"$(_merge_hook_family ssh)\"; _merge_hook_family_files ssh",
    );
    assert_eq!(code, 0, "shell hook paths");
    let shell = String::from_utf8(out).expect("paths text");
    let mut rust = format!(
        "{}\n{}\n",
        merge_hooks::hook_source(&hooks, OsStr::new("ssh")).display(),
        hooks.join("ssh").display(),
    );
    // The `_merge_hook_family` line above already proves the join;
    // pin the named alias to the same path.
    assert_eq!(
        merge_hooks::family(&hooks, OsStr::new("ssh")),
        hooks.join("ssh"),
        "family join"
    );
    for path in merge_hooks::family_files(&fam).expect("rust files") {
        rust.push_str(&format!("{}\n", path.display()));
    }
    assert_eq!(rust, shell, "hook source/family/files");
    // Pattern filtering and relative identity helpers.
    let (code, out, _) = shell_run(
        root,
        &[fam.as_os_str()],
        &env_refs,
        "_merge_hook_family_files_matching ssh '1*' '3*'; _merge_hook_family_relpath ssh \"$2/30-group.replace/10-winner\"; _merge_hook_family_marker_name ssh \"$2/30-group.replace/10-winner\"",
    );
    assert_eq!(code, 0, "shell matching helpers");
    let shell = String::from_utf8(out).expect("matching text");
    let winner = fam.join("30-group.replace/10-winner");
    let mut rust = String::new();
    for path in merge_hooks::family_files_matching(&fam, &[b"1*", b"3*"]).expect("rust match") {
        rust.push_str(&format!("{}\n", path.display()));
    }
    // The shell family helpers resolve through the hooks root, so
    // drive the Rust side through the same test-only family name.
    let rel = merge_hooks::family_relpath(&hooks.join("ssh"), &winner);
    rust.push_str(&format!(
        "{}\n{}\n",
        rel.to_string_lossy(),
        merge_hooks::family_marker_name(&rel).to_string_lossy()
    ));
    // Normalize the absolute fixture prefix: the shell stream above
    // already printed it, so compare the tail shapes instead.
    let shell_tail: Vec<&str> = shell.lines().collect();
    let rust_tail: Vec<&str> = rust.lines().collect();
    assert_eq!(rust_tail.len(), shell_tail.len(), "matching line count");
    for (r, s) in rust_tail.iter().zip(shell_tail.iter()) {
        assert!(
            s.ends_with(r),
            "matching stream tail: shell={s:?} rust={r:?}"
        );
    }
    assert_eq!(rel.to_string_lossy(), "30-group.replace/10-winner");
    assert_eq!(
        merge_hooks::family_marker_name(&rel).to_string_lossy(),
        "30-group.replace_10-winner"
    );
}

#[test]
fn expand_home_cases_agree() {
    let dir = TempDir::new("mh-expand").expect("fixture dir");
    let root = dir.path();
    let home = "/home/tester";
    let values = [
        "$HOME/.ssh",
        "${HOME}/.ssh",
        "~",
        "~/doc",
        "~other",
        "/abs",
        "rel",
        "",
        "$HOME",
        "${HOME}",
        "~/$HOME",
        "$HOME~",
        "$$HOME",
    ];
    for value in values {
        let (code, out, _) = shell_run(
            root,
            &[value.as_ref()],
            &[("HOME", Some(home))],
            "_merge_hook_expand_home \"$2\"",
        );
        assert_eq!(code, 0, "shell expand {value:?}");
        let shell = String::from_utf8(out).expect("expand text");
        let rust = merge_hooks::expand_home(value, home);
        assert_eq!(format!("{rust}\n"), shell, "expand parity for {value:?}");
    }
}

#[test]
fn write_text_twins_agree() {
    for (label, initial) in [
        ("absent", None),
        ("same", Some("line\n")),
        ("different", Some("old\n")),
    ] {
        let sdir = TempDir::new(&format!("mh-write-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("mh-write-{label}-rust")).expect("rust dir");
        let dst_s = sdir.path().join("out/conf");
        let dst_r = rdir.path().join("out/conf");
        if let Some(body) = initial {
            stage(sdir.path(), "out/conf", body.as_bytes());
            stage(rdir.path(), "out/conf", body.as_bytes());
        } else {
            std::fs::create_dir_all(sdir.path().join("out")).expect("shell parent");
            std::fs::create_dir_all(rdir.path().join("out")).expect("rust parent");
        }
        let (scode, _, _) = shell_run(
            sdir.path(),
            &[dst_s.as_os_str(), "line".as_ref()],
            &[],
            "_merge_hook_write_text_if_changed \"$2\" \"$3\"",
        );
        let mut cache = temp::MoveCache::default();
        let mut warnings = Vec::new();
        let rcode = merge_hooks::write_text_if_changed(
            &dst_r,
            "line",
            &mut merge_hooks::Ctx {
                source_root: rdir.path(),
                cache: &mut cache,
                warnings: &mut warnings,
            },
        );
        assert_eq!(rcode.is_ok(), scode == 0, "write code for {label}");
        assert!(warnings.is_empty(), "no warnings for {label}");
        assert_eq!(
            std::fs::read(&dst_r).expect("rust dst"),
            std::fs::read(&dst_s).expect("shell dst"),
            "write bytes for {label}"
        );
    }
}

#[test]
fn jq_layer_twins_agree() {
    let filter = "$s[0] * $d[0]";
    type Case<'a> = (&'a str, Option<&'a [u8]>, &'a [u8]);
    let cases: &[Case<'_>] = &[
        ("install", None, b"{\"a\": 1}\n"),
        ("merge", Some(b"{\"a\": 1}\n"), b"{\"b\": 2}\n"),
        ("corrupt", Some(b"not json\n"), b"{\"b\": 2}\n"),
        ("empty", Some(b""), b"{\"b\": 2}\n"),
        ("bad-src", Some(b"{\"a\": 1}\n"), b"not json\n"),
    ];
    for (label, dst_body, src_body) in cases {
        let sdir = TempDir::new(&format!("mh-jq-{label}-shell")).expect("shell dir");
        let rdir = TempDir::new(&format!("mh-jq-{label}-rust")).expect("rust dir");
        let src_s = stage(sdir.path(), "src.json", src_body);
        let src_r = stage(rdir.path(), "src.json", src_body);
        let dst_s = sdir.path().join("dst.json");
        let dst_r = rdir.path().join("dst.json");
        if let Some(body) = dst_body {
            stage(sdir.path(), "dst.json", body);
            stage(rdir.path(), "dst.json", body);
        }
        let snippet = "label=\"$5\"; src=\"$2\"; dst=\"$3\"; filter=\"$4\"; _merge_hook_jq_layer \"$label\" \"$src\" \"$dst\" \"$filter\"";
        let (scode, _, serr) = shell_run(
            sdir.path(),
            &[
                src_s.as_os_str(),
                dst_s.as_os_str(),
                filter.as_ref(),
                format!("case-{label}").as_str().as_ref(),
            ],
            &[],
            snippet,
        );
        let mut cache = temp::MoveCache::default();
        let mut warnings = Vec::new();
        let rcode = merge_hooks::jq_layer(
            &format!("case-{label}"),
            &src_r,
            &dst_r,
            filter,
            &mut merge_hooks::Ctx {
                source_root: rdir.path(),
                cache: &mut cache,
                warnings: &mut warnings,
            },
        );
        assert_eq!(rcode.is_ok(), scode == 0, "jq code for {label}");
        // Warning texts agree up to the fixture root both sides embed.
        let normalize =
            |root: &Path, text: &str| text.replace(&root.to_string_lossy().into_owned(), "<root>");
        let shell_warn: Vec<String> = without_exec_noise(String::from_utf8_lossy(&serr).lines())
            .into_iter()
            .map(|line| normalize(sdir.path(), &line))
            .collect();
        let rust_warn: Vec<String> = warnings
            .iter()
            .map(|line| normalize(rdir.path(), line))
            .collect();
        assert_eq!(rust_warn, shell_warn, "jq warnings for {label}");
        // Absent `jq` installs nothing on either side: compare
        // presence and bytes together.
        assert_eq!(
            std::fs::read(&dst_r).ok(),
            std::fs::read(&dst_s).ok(),
            "jq bytes for {label}"
        );
    }
    // A failing filter warns and leaves the destination alone.
    let sdir = TempDir::new("mh-jq-badfilter-shell").expect("shell dir");
    let rdir = TempDir::new("mh-jq-badfilter-rust").expect("rust dir");
    let src_s = stage(sdir.path(), "src.json", b"{\"a\": 1}\n");
    let src_r = stage(rdir.path(), "src.json", b"{\"a\": 1}\n");
    let dst_s = stage(sdir.path(), "dst.json", b"{\"a\": 0}\n");
    let dst_r = stage(rdir.path(), "dst.json", b"{\"a\": 0}\n");
    let (scode, _, serr) = shell_run(
        sdir.path(),
        &[
            src_s.as_os_str(),
            dst_s.as_os_str(),
            "?!".as_ref(),
            "case-bad".as_ref(),
        ],
        &[],
        "_merge_hook_jq_layer \"$5\" \"$2\" \"$3\" \"$4\"",
    );
    let mut cache = temp::MoveCache::default();
    let mut warnings = Vec::new();
    let rcode = merge_hooks::jq_layer(
        "case-bad",
        &src_r,
        &dst_r,
        "?!",
        &mut merge_hooks::Ctx {
            source_root: rdir.path(),
            cache: &mut cache,
            warnings: &mut warnings,
        },
    );
    assert_eq!(rcode.is_ok(), scode == 0, "bad filter code");
    // `jq` diagnostics plus the skip warning agree line for line
    // (fixture roots normalized).
    let shell_warn: Vec<String> = without_exec_noise(String::from_utf8_lossy(&serr).lines())
        .into_iter()
        .map(|line| line.replace(&sdir.path().to_string_lossy().into_owned(), "<root>"))
        .collect();
    let rust_warn: Vec<String> = warnings
        .iter()
        .map(|line| line.replace(&rdir.path().to_string_lossy().into_owned(), "<root>"))
        .collect();
    assert_eq!(rust_warn, shell_warn, "bad filter warnings");
    // Without `jq` the failed rebuild deletes the destination on
    // both sides; with `jq` the failed merge leaves it alone.
    assert_eq!(
        std::fs::read(&dst_r).ok(),
        std::fs::read(&dst_s).ok(),
        "bad filter dst"
    );
}

#[test]
fn jq_available_agrees() {
    let dir = TempDir::new("mh-jqavail").expect("fixture dir");
    let (code, out, _) = shell_run(
        dir.path(),
        &[],
        &[],
        "_merge_hook_jq_available; printf '%s' \"$?\"",
    );
    assert_eq!(code, 0, "shell probe runs");
    let shell = String::from_utf8(out).expect("probe text");
    assert_eq!(merge_hooks::jq_available(), shell == "0", "jq availability");
}
