//! Differential parity and micro-benchmark coverage for the native
//! link hot loop (`src/repos_link_exec.rs`, porting `_link_overlay`
//! from `lib/dot/repos/overlays.sh`).
//!
//! Every case runs the live shell function and its Rust twin on twin
//! fixtures (identical content, separate directories) and compares
//! exit codes, replies, stdout/stderr streams, manifest records,
//! installed-path sets, skip-worktree bits, and converged HOME
//! trees. Manifest and tree listings compare sorted: walk order is
//! filesystem order on both sides, stable on one host but not a
//! cross-host contract.
//!
//! The closing benchmark times one 60-file overlay link on each
//! side and reports both medians alongside the parity assertion,
//! so the hot-loop speedup carries numbers from every run.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_link_exec;
use dot::test_support::TempDir;

/// Run one shell snippet with the overlay runtime sourced (the link
/// loop shares the config and reserved helpers of the leaf layer).
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/overlays.sh\"\n. \"$1/lib/dot/reserved.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/repos/config.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/init-client.sh\"\n{snippet}"
    ));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
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

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Replace one side root with `{SIDE}` so twin dumps compare.
fn scrub(dump: &str, root: &Path) -> String {
    dump.replace(&root.to_string_lossy().into_owned(), "{SIDE}")
}

/// Run `git` hermetically inside `cwd` (no user or system config).
fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut cmd = Command::new("git");
    cmd.args(args);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &path)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One fixture side: home, overlay checkout, and link scaffolding.
struct Side {
    root: PathBuf,
    home: PathBuf,
    ov_path: PathBuf,
    url: String,
    manifest: String,
    legacy: String,
}

/// Seed one side: git overlay checkout plus an optional base repo
/// (separate git dir, home work tree) with committed files.
fn setup_side(
    fix: &Path,
    tag: &str,
    payload: &[(&str, &str)],
    base_files: &[(&str, &str)],
) -> Side {
    let root = fix.join(tag);
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("side home");
    let work = root.join("ov-work");
    std::fs::create_dir_all(work.join("home")).expect("work home");
    for (rel, body) in payload {
        stage(&work.join("home"), rel, body.as_bytes());
    }
    git(&work, &home, &["init", "-b", "main"]);
    git(&work, &home, &["add", "-A"]);
    git(&work, &home, &["commit", "-qm", "seed", "--allow-empty"]);
    let ov_path = root.join("ov");
    let work_text = work.to_string_lossy().into_owned();
    let ov_text = ov_path.to_string_lossy().into_owned();
    git(&root, &home, &["clone", "-q", &work_text, &ov_text]);
    if !base_files.is_empty() {
        for (rel, body) in base_files {
            stage(&home, rel, body.as_bytes());
        }
        // Separate git dir with home as the work tree: the topology
        // the link loop addresses through `_base_git`.
        let git_dir = root.join("base.git").to_string_lossy().into_owned();
        let home_text = home.to_string_lossy().into_owned();
        git(
            &home,
            &home,
            &[
                "init",
                "-b",
                "main",
                "--separate-git-dir",
                &git_dir,
                &home_text,
            ],
        );
        git(&home, &home, &["add", "-A"]);
        git(&home, &home, &["commit", "-qm", "base"]);
    }
    let manifest = root.join("manifest.tsv").to_string_lossy().into_owned();
    let legacy = root.join("legacy.tsv").to_string_lossy().into_owned();
    Side {
        root,
        home,
        ov_path,
        url: work_text,
        manifest,
        legacy,
    }
}

/// Shell `_link_overlay` over one side plus a full state dump:
/// rc/reply/status, reserved roots, manifest records, installed
/// paths, skip-worktree flags, and the HOME tree. `extra` exports
/// case vars (`SYNC`, `VERBOSE`, foreign-target setup runs before
/// the link call through `prelude`).
struct ShellCase<'a> {
    sync: &'a str,
    verbose: bool,
    authority: &'a [(&'a str, &'a str)],
    prelude: &'a str,
}

/// Dump one linked side from the shell and print the comparison
/// text to stdout (captured by the harness, not the link call).
fn shell_snippet(
    side: &Side,
    case: &ShellCase<'_>,
    inv_root: &Path,
    manifest_new: &Path,
) -> String {
    let mut out = format!("export HOME={}; ", sq(&side.home.to_string_lossy()));
    out.push_str(&format!(
        "export DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={}; ",
        sq(&side.manifest),
        sq(&side.legacy),
    ));
    out.push_str(&format!(
        "OVERLAYS=('ov|{}|{}|||{}'); ",
        side.ov_path.to_string_lossy(),
        side.url,
        case.sync,
    ));
    out.push_str("declare -A _overlay_inventory_files=(); ");
    out.push_str("declare -A _overlay_inventory_source_roots=(); ");
    out.push_str("declare -A _overlay_inventory_source_identities=(); ");
    out.push_str("declare -A _overlay_authority_paths=(); ");
    out.push_str("declare -A _overlay_authority_targets=(); ");
    out.push_str("declare -A _overlay_current_paths=(); ");
    out.push_str("declare -A _base_tracked=(); ");
    out.push_str(&format!(
        "_base_git() {{ git --git-dir={} --work-tree={} \"$@\"; }}; ",
        sq(&side.root.join("base.git").to_string_lossy()),
        sq(&side.home.to_string_lossy()),
    ));
    out.push_str(case.prelude);
    out.push(' ');
    out.push_str(&format!(
        "_overlay_prepare_inventories {} || {{ printf 'rc=1\\n'; exit 0; }}; ",
        sq(&inv_root.to_string_lossy())
    ));
    out.push_str("inv=${_overlay_inventory_files[ov]}; ");
    out.push_str("_dot_reserved_roots_snapshot || { printf 'rc=1\\n'; exit 0; }; ");
    out.push_str("printf 'roots-begin\\n%s\\nroots-end\\n' \"$REPLY\"; ");
    out.push_str(&format!(
        "_overlay_manifest_new={}; ",
        sq(&manifest_new.to_string_lossy())
    ));
    out.push_str("_overlay_reserved_roots=$REPLY; ");
    out.push_str("while IFS= read -r tf; do [[ -n $tf ]] || continue; _base_tracked[$tf]=1; done < <(_base_git ls-files 2>/dev/null); ");
    for (rel, target) in case.authority {
        out.push_str(&format!(
            "_overlay_authority_paths[{}]=1; _overlay_authority_targets[{}]={}; ",
            sq(rel),
            sq(&format!("{rel}\t{target}")),
            sq(target),
        ));
    }
    if case.verbose {
        out.push_str("export DOT_VERBOSE=1; ");
    }
    out.push_str(&format!(
        concat!(
            "cap_out=$(mktemp); cap_err=$(mktemp); ",
            "_link_overlay ov {} \"$inv\" {} >\"$cap_out\" 2>\"$cap_err\"; code=$?; ",
            "printf 'rc=%s\\nreply=%s\\nstatus=%s\\n' \"$code\" \"$REPLY\" \"$REPLY_STATUS\"; ",
            "printf 'out='; cat \"$cap_out\"; printf 'err='; cat \"$cap_err\"; rm -f \"$cap_out\" \"$cap_err\"; ",
            "LC_ALL=C sort {} | while IFS= read -r line; do printf 'man\\t%s\\n' \"$line\"; done; ",
            "for k in $(printf '%s\\n' \"${{!_overlay_current_paths[@]}}\" | LC_ALL=C sort); do printf 'cur\\t%s\\n' \"$k\"; done; ",
            "_base_git ls-files -v 2>/dev/null | LC_ALL=C sort | while IFS= read -r line; do printf 'idx\\t%s\\n' \"$line\"; done; ",
            "cd \"$HOME\" && find . -mindepth 1 \\( -name '.git*' \\) -prune -o -print0 | LC_ALL=C sort -z | ",
            "while IFS= read -r -d '' p; do ",
            "if [[ -L $p ]]; then printf 'tree\\tlink\\t%s\\t%s\\n' \"$p\" \"$(readlink \"$p\")\"; ",
            "elif [[ -d $p ]]; then printf 'tree\\tdir\\t%s\\n' \"$p\"; ",
            "elif [[ -f $p ]]; then printf 'tree\\tfile\\t%s\\t%s\\n' \"$p\" \"$(sha256sum <\"$p\" | cut -d' ' -f1)\"; ",
            "else printf 'tree\\tother\\t%s\\n' \"$p\"; fi; done\n",
        ),
        sq(&side.ov_path.to_string_lossy()),
        sq(case.sync),
        sq(&manifest_new.to_string_lossy()),
    ));
    out
}

/// Parsed shell dump: streams plus the comparison rows.
struct ShellDump {
    text: String,
    roots: String,
}

/// Run the shell side once: prepare, snapshot, link, dump.
fn run_shell(side: &Side, case: &ShellCase<'_>, tag: &str) -> ShellDump {
    let inv_root = side.root.join(format!("inv-{tag}"));
    std::fs::create_dir_all(&inv_root).expect("inv root");
    let manifest_new = side.root.join(format!("manifest-new-{tag}"));
    let _ = std::fs::remove_file(&manifest_new);
    std::fs::write(&manifest_new, b"").expect("manifest seed");
    let (code, out, _serr) = shell_run(
        &side.home,
        &shell_snippet(side, case, &inv_root, &manifest_new),
    );
    assert_eq!(code, 0, "harness exit");
    let text = scrub(&String::from_utf8(out).expect("shell dump"), &side.root);
    let mut roots: Vec<String> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line == "roots-begin" {
            inside = true;
            continue;
        }
        if line == "roots-end" {
            inside = false;
            continue;
        }
        if inside {
            roots.push(line.to_string());
        }
    }
    ShellDump {
        text,
        roots: roots.join("\n"),
    }
}

/// Walk one HOME into the shell `tree` dump shape (`.git*` pruned).
/// Sorts by path like the snippet's `sort -z`, not by row text.
fn dump_tree(home: &Path) -> Vec<String> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut rows: Vec<(Vec<u8>, String)> = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut children: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read home") {
            children.push(entry.expect("home entry").path());
        }
        for child in children {
            let base = child.file_name().expect("basename").as_bytes().to_vec();
            if base.starts_with(b".git") {
                continue;
            }
            let rel = child
                .strip_prefix(home)
                .expect("home prefix")
                .to_string_lossy()
                .into_owned();
            let rel = format!("./{rel}");
            let ftype = child.symlink_metadata().expect("home meta").file_type();
            // Sort key mirrors `find ... -print0 | sort -z`: the raw
            // path bytes, so directories sort before their children.
            let mut key = Vec::from(rel.as_bytes());
            key.push(0);
            if ftype.is_symlink() {
                let target = std::fs::read_link(&child).expect("readlink");
                rows.push((
                    key,
                    format!("tree\tlink\t{rel}\t{}", target.to_string_lossy()),
                ));
            } else if ftype.is_dir() {
                stack.push(child);
                rows.push((key, format!("tree\tdir\t{rel}")));
            } else if ftype.is_file() {
                let bytes = std::fs::read(&child).expect("read file");
                rows.push((key, format!("tree\tfile\t{rel}\t{}", sha256(&bytes))));
            } else {
                rows.push((key, format!("tree\tother\t{rel}")));
            }
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, line)| line).collect()
}

fn sha256(bytes: &[u8]) -> String {
    // Shell out to the same tool the snippet uses so digests agree
    // without a new dependency.
    use std::io::Write as _;
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sha256sum");
    child
        .stdin
        .as_mut()
        .expect("sha stdin")
        .write_all(bytes)
        .expect("sha write");
    let result = child.wait_with_output().expect("sha output");
    String::from_utf8_lossy(&result.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Sorted lines of a text file (empty when missing).
fn sorted_lines(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

/// Run the Rust twin over one side and render the shell dump shape.
#[allow(clippy::too_many_arguments)]
fn run_rust(
    side: &Side,
    case: &ShellCase<'_>,
    tag: &str,
    roots: &str,
    authority: &[(String, String)],
    jobs: Option<&str>,
) -> String {
    // Same inventory input the shell prepare builds, through the
    // native prep port (covered differentially in tests/link_prep).
    let inv_root = side.root.join(format!("rinv-{tag}"));
    std::fs::create_dir_all(&inv_root).expect("rust inv root");
    let (entries, prep_home, prep_jobs) = prep_inputs(side, case, jobs);
    let prep_jobs_ref = prep_jobs.as_deref();
    let prep_bundle = dot::repos_link_prep::Inputs {
        entries: &entries,
        home: &prep_home,
        update_jobs: prep_jobs_ref,
    };
    let prepared =
        dot::repos_link_prep::prepare_inventories(&prep_bundle, &inv_root).expect("rust prepare");
    let inv_path = prepared.inventories.get("ov").expect("ov inventory");
    let inventory = std::fs::read(inv_path).expect("read inventory");
    // Frozen local-source identity flows from preparation into the
    // link call, exactly like the shell inventory maps.
    let frozen_root = prepared.source_roots.get("ov").cloned();
    let frozen_identity = prepared.source_identities.get("ov").cloned();
    let manifest_new = side.root.join(format!("rmanifest-new-{tag}"));
    let _ = std::fs::remove_file(&manifest_new);
    std::fs::write(&manifest_new, b"").expect("rust manifest seed");
    let home_text = side.home.to_string_lossy().into_owned();
    let ov_text = side.ov_path.to_string_lossy().into_owned();
    let palette = dot::progress_ui::Palette::empty();
    let pwd = std::fs::canonicalize(&side.home)
        .expect("canonical home")
        .to_string_lossy()
        .into_owned();
    let dest = dot::repos_overlays::DestinationInputs {
        home: home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![ov_text.clone()],
        init_backup: None,
        pwd,
    };
    let base_git_dir = side.root.join("base.git");
    let has_base = base_git_dir.exists();
    let base = has_base.then(|| dot::repos_base::Base {
        topology: dot::repos_base::Topology::Separate,
        client_git_dir: base_git_dir.to_string_lossy().into_owned(),
        home: home_text.clone(),
    });
    let mut base_tracked = HashSet::new();
    if let Some(base) = &base {
        let prefix = base.git_prefix().expect("git prefix");
        let output = dot::repos_base::run_git(&prefix, &["ls-files"]).expect("ls-files");
        assert!(output.status.success(), "ls-files rc");
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if !line.is_empty() {
                base_tracked.insert(line.to_string());
            }
        }
    }
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let sync = if case.sync.is_empty() {
        "git"
    } else {
        case.sync
    };
    let entry = format!("ov|{ov_text}|{}|||{sync}", side.url);
    let overlays = vec![entry];
    let inputs = repos_link_exec::Inputs {
        name: "ov",
        path: &ov_text,
        sync,
        home: &home_text,
        overlay_home: &format!("{ov_text}/home"),
        overlays: &overlays,
        dest: &dest,
        reserved_roots: Some(roots),
        authority_targets: authority,
        base: base.as_ref(),
        base_tracked: &base_tracked,
        manifest: &side.manifest,
        legacy_manifest: &side.legacy,
        manifest_new: &manifest_new,
        source_root: frozen_root.as_deref(),
        source_identity: frozen_identity.as_deref(),
        euid,
        source_root_git: &side.root,
        tmp: &side.root,
        tool: &tool,
        palette: &palette,
        multibyte: false,
        dot_quiet: None,
        dot_verbose: if case.verbose { Some("1") } else { None },
        ui_total: None,
    };
    let mut state = repos_link_exec::OverlayState::new();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let outcome =
        repos_link_exec::link_overlay(&inputs, &mut state, &inventory, &mut out, &mut err);
    let (reply, status) = match &outcome {
        repos_link_exec::Outcome::Changed(reply) => (reply.clone(), "changed".to_string()),
        repos_link_exec::Outcome::Current(reply) => (reply.clone(), "current".to_string()),
        repos_link_exec::Outcome::Failed => (String::new(), String::new()),
    };
    // Dump order mirrors the snippet: roots, rc/reply/status,
    // streams, manifest, installed paths, index flags, tree.
    let mut text = format!("roots-begin\n{roots}\nroots-end\n");
    text.push_str(&format!(
        "rc={}\nreply={reply}\nstatus={status}\n",
        outcome_rc(&outcome)
    ));
    text.push_str("out=");
    text.push_str(&String::from_utf8_lossy(&out));
    text.push_str("err=");
    text.push_str(&String::from_utf8_lossy(&err));
    for line in sorted_lines(&manifest_new) {
        text.push_str(&format!("man\t{line}\n"));
    }
    let mut current: Vec<&String> = state.current.iter().collect();
    current.sort();
    for rel in current {
        text.push_str(&format!("cur\t{rel}\n"));
    }
    if let Some(base) = &base {
        let prefix = base.git_prefix().expect("git prefix");
        let output = dot::repos_base::run_git(&prefix, &["ls-files", "-v"]).expect("ls-files -v");
        let mut flags: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        flags.sort();
        for flag in flags {
            text.push_str(&format!("idx\t{flag}\n"));
        }
    }
    for line in dump_tree(&side.home) {
        text.push_str(&format!("{line}\n"));
    }
    scrub(&text, &side.root)
}

fn outcome_rc(outcome: &repos_link_exec::Outcome) -> i32 {
    match outcome {
        repos_link_exec::Outcome::Changed(_) | repos_link_exec::Outcome::Current(_) => 0,
        repos_link_exec::Outcome::Failed => 1,
    }
}

/// Prep inputs for the Rust inventory build (mirrors the shell
/// prepare call inside the snippet).
fn prep_inputs(
    side: &Side,
    case: &ShellCase<'_>,
    jobs: Option<&str>,
) -> (Vec<String>, String, Option<String>) {
    let entry = format!(
        "ov|{}|{}|||{}",
        side.ov_path.to_string_lossy(),
        side.url,
        case.sync,
    );
    (
        vec![entry],
        side.home.to_string_lossy().into_owned(),
        jobs.map(str::to_string),
    )
}

/// Blank the failure reply/status pair: the shell leaks an
/// incidental `REPLY` on failure paths that no caller reads (see the
/// module docs), so only the code, rows, and state compare there.
fn normalize_failure(mut text: String) -> String {
    if text.contains("\nrc=1\n") {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        for line in &mut lines {
            if line.starts_with("reply=") || line.starts_with("status=") {
                *line = line.split('=').next().unwrap_or("").to_string() + "=";
            }
        }
        text = lines.join("\n") + "\n";
    }
    text
}

/// Drive both twins over twin sides and require identical dumps.
fn assert_twins(
    fix: &Path,
    tag: &str,
    payload: &[(&str, &str)],
    base_files: &[(&str, &str)],
    case: &ShellCase<'_>,
    setup: &dyn Fn(&Side),
) {
    let shell_side = setup_side(fix, &format!("{tag}-shell"), payload, base_files);
    let rust_side = setup_side(fix, &format!("{tag}-rust"), payload, base_files);
    setup(&shell_side);
    setup(&rust_side);
    let shell = run_shell(&shell_side, case, tag);
    let authority: Vec<(String, String)> = case
        .authority
        .iter()
        .map(|(rel, target)| (rel.to_string(), target.to_string()))
        .collect();
    let rust = run_rust(&rust_side, case, tag, &shell.roots, &authority, None);
    assert_eq!(
        normalize_failure(shell.text),
        normalize_failure(rust),
        "shell/Rust dumps differ for {tag}"
    );
}

#[test]
fn fresh_link_matches_shell() {
    let dir = TempDir::new("linkexec-fresh").expect("fixture dir");
    let fix = dir.path();
    let case = ShellCase {
        sync: "git",
        verbose: false,
        authority: &[],
        prelude: "",
    };
    assert_twins(
        fix,
        "fresh",
        &[("a.conf", "a\n"), ("sub/b.conf", "b\n"), ("c.conf", "c\n")],
        &[],
        &case,
        &|_| {},
    );
}

#[test]
fn converged_second_run_matches_shell() {
    let dir = TempDir::new("linkexec-converged").expect("fixture dir");
    let fix = dir.path();
    let payload = &[("a.conf", "a\n"), ("sub/b.conf", "b\n")];
    let case = ShellCase {
        sync: "git",
        verbose: false,
        authority: &[],
        prelude: "",
    };
    let shell_side = setup_side(fix, "conv-shell", payload, &[]);
    let rust_side = setup_side(fix, "conv-rust", payload, &[]);
    // First run converges both sides (asserted in fresh_link).
    let first = run_shell(&shell_side, &case, "first");
    let authority: Vec<(String, String)> = Vec::new();
    let _ = run_rust(&rust_side, &case, "first", &first.roots, &authority, None);
    // Second run must agree it is all current.
    let second = run_shell(&shell_side, &case, "second");
    assert!(second.text.contains("status=current\n"), "shell converges");
    let rerun = run_rust(&rust_side, &case, "second", &second.roots, &authority, None);
    assert_eq!(
        normalize_failure(second.text),
        normalize_failure(rerun),
        "converged reruns differ"
    );
}

#[test]
fn reserved_path_refuses_both() {
    let dir = TempDir::new("linkexec-reserved").expect("fixture dir");
    let fix = dir.path();
    // The manifest destination is always authority: an overlay
    // shipping it must fail on both sides with the same warning.
    // Point both manifests at the shipped rel inside home.
    let mut shell_side = setup_side(
        fix,
        "res-shell",
        &[("a.conf", "a\n"), ("manifest.tsv", "x\n")],
        &[],
    );
    let mut rust_side = setup_side(
        fix,
        "res-rust",
        &[("a.conf", "a\n"), ("manifest.tsv", "x\n")],
        &[],
    );
    shell_side.manifest = shell_side
        .home
        .join("manifest.tsv")
        .to_string_lossy()
        .into_owned();
    rust_side.manifest = rust_side
        .home
        .join("manifest.tsv")
        .to_string_lossy()
        .into_owned();
    let case = ShellCase {
        sync: "git",
        verbose: false,
        authority: &[],
        prelude: "",
    };
    let shell = run_shell(&shell_side, &case, "res");
    assert!(
        shell.text.contains("\nrc=1\n"),
        "shell refuses: {}",
        shell.text
    );
    let authority: Vec<(String, String)> = Vec::new();
    let rust = run_rust(&rust_side, &case, "res", &shell.roots, &authority, None);
    assert_eq!(
        normalize_failure(shell.text),
        normalize_failure(rust),
        "refusal dumps differ"
    );
}

/// Destination pre-staging: plant one foreign path in `$HOME`.
fn plant(side: &Side, rel: &str, kind: &str) {
    let dst = side.home.join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).expect("plant parents");
    }
    // A base-committed file may already occupy the destination;
    // the prelude replaces it like a foreign writer would.
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_dir_all(&dst);
    match kind {
        "symlink" => std::os::unix::fs::symlink("/elsewhere/file", &dst).expect("plant link"),
        "file" => std::fs::write(&dst, b"mine\n").expect("plant file"),
        "dir" => std::fs::create_dir_all(&dst).expect("plant dir"),
        _ => panic!("unknown plant kind"),
    }
}

#[test]
fn skips_match_shell() {
    let dir = TempDir::new("linkexec-skips").expect("fixture dir");
    let fix = dir.path();
    // Foreign destinations under git sync without tracking stay
    // unmanaged-skipped only when guarded; these three never guard.
    for (tag, kind, payload) in [("untracked", "file", "f.conf"), ("dirway", "dir", "f.conf")] {
        let case = ShellCase {
            sync: "git",
            verbose: false,
            authority: &[],
            prelude: "",
        };
        assert_twins(fix, tag, &[(payload, "new\n")], &[], &case, &|side| {
            plant(side, payload, kind)
        });
    }
    // Filesystem sync guards every foreign symlink.
    let case = ShellCase {
        sync: "none",
        verbose: false,
        authority: &[],
        prelude: "",
    };
    assert_twins(
        fix,
        "nonelink",
        &[("f.conf", "new\n")],
        &[],
        &case,
        &|side| plant(side, "f.conf", "symlink"),
    );
}

#[test]
fn tracked_override_matches_shell() {
    let dir = TempDir::new("linkexec-override").expect("fixture dir");
    let fix = dir.path();
    // A clean tracked file shadows into an override link with the
    // skip-worktree bit set; a dirty one stays with a warning.
    for (tag, dirty) in [("clean", false), ("dirty", true)] {
        let case = ShellCase {
            sync: "git",
            verbose: false,
            authority: &[],
            prelude: "",
        };
        assert_twins(
            fix,
            tag,
            &[("f.conf", "new\n")],
            &[("f.conf", "base\n")],
            &case,
            &|side| {
                if dirty {
                    std::fs::write(side.home.join("f.conf"), b"mine\n").expect("dirty file");
                }
            },
        );
    }
}

#[test]
fn tracked_foreign_symlink_matches_shell() {
    let dir = TempDir::new("linkexec-symlink").expect("fixture dir");
    let fix = dir.path();
    // A tracked foreign symlink stays unless the recovery authority
    // names its exact target, in which case it replaces.
    let plain = ShellCase {
        sync: "git",
        verbose: false,
        authority: &[],
        prelude: "",
    };
    assert_twins(
        fix,
        "unmanaged",
        &[("f.conf", "new\n")],
        &[("f.conf", "base\n")],
        &plain,
        &|side| plant(side, "f.conf", "symlink"),
    );
    let owned = ShellCase {
        sync: "git",
        verbose: false,
        authority: &[("f.conf", "/elsewhere/file")],
        prelude: "",
    };
    assert_twins(
        fix,
        "authority",
        &[("f.conf", "new\n")],
        &[("f.conf", "base\n")],
        &owned,
        &|side| plant(side, "f.conf", "symlink"),
    );
}

/// Median of a millisecond sample (upper middle for even counts).
fn median_ms(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Hot-loop benchmark: one 60-file overlay linked fresh five times
/// per side. Reports both medians alongside the parity assertion so
/// the native loop carries numbers from every run. Timing asserts
/// nothing (CI hosts vary); the medians below are the claim.
#[test]
fn benchmark_sixty_files_reports_medians() {
    use std::time::Instant;
    let dir = TempDir::new("linkexec-bench").expect("fixture dir");
    let fix = dir.path();
    let mut payload: Vec<(String, String)> = Vec::new();
    for index in 0..60 {
        payload.push((format!("f{index:02}.conf"), format!("body {index}\n")));
    }
    for index in 0..5 {
        payload.push((format!("sub/n{index}.conf"), format!("nested {index}\n")));
    }
    let payload_refs: Vec<(&str, &str)> = payload
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    // One shared overlay checkout; homes stay fresh per run.
    let shell_base = setup_side(fix, "bench-shell", &payload_refs, &[]);
    let rust_base = setup_side(fix, "bench-rust", &payload_refs, &[]);
    let case = ShellCase {
        sync: "git",
        verbose: false,
        authority: &[],
        prelude: "",
    };
    // Inventories depend only on the overlay: build once per side.
    let shell_inv_root = fix.join("bench-shell-inv");
    std::fs::create_dir_all(&shell_inv_root).expect("shell inv");
    let (shell_inv_code, shell_inv_out, _) = shell_run(
        &shell_base.home,
        &format!(
            "export HOME={}; OVERLAYS=('ov|{}|{}|||git'); declare -A _overlay_inventory_files=() _overlay_inventory_source_roots=() _overlay_inventory_source_identities=(); _overlay_prepare_inventories {}; printf 'inv=%s\n' \"${{_overlay_inventory_files[ov]}}\"; _dot_reserved_roots_snapshot; printf 'roots-begin\n%s\nroots-end\n' \"$REPLY\"\n",
            sq(&shell_base.home.to_string_lossy()),
            shell_base.ov_path.to_string_lossy(),
            shell_base.url,
            sq(&shell_inv_root.to_string_lossy()),
        ),
    );
    assert_eq!(shell_inv_code, 0, "bench prepare");
    let shell_inv_text = String::from_utf8(shell_inv_out).expect("inv dump");
    let shell_inv = shell_inv_text
        .lines()
        .find_map(|line| line.strip_prefix("inv="))
        .expect("inv path")
        .to_string();
    let shell_inventory = std::fs::read(&shell_inv).expect("shell inventory");
    let mut shell_roots = Vec::new();
    let mut inside = false;
    for line in shell_inv_text.lines() {
        if line == "roots-begin" {
            inside = true;
            continue;
        }
        if line == "roots-end" {
            inside = false;
            continue;
        }
        if inside {
            shell_roots.push(line.to_string());
        }
    }
    let shell_roots = shell_roots.join("\n");
    let (entries, prep_home, _) = prep_inputs(&rust_base, &case, None);
    let prep_bundle = dot::repos_link_prep::Inputs {
        entries: &entries,
        home: &prep_home,
        update_jobs: None,
    };
    let rust_inv_root = fix.join("bench-rust-inv");
    std::fs::create_dir_all(&rust_inv_root).expect("rust inv");
    let prepared = dot::repos_link_prep::prepare_inventories(&prep_bundle, &rust_inv_root)
        .expect("rust prepare");
    let rust_inventory =
        std::fs::read(prepared.inventories.get("ov").expect("ov")).expect("rust inventory");
    // Same entry sets on both sides (sorted: order is not contractual).
    let mut shell_set: Vec<&[u8]> = shell_inventory
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    let mut rust_set: Vec<&[u8]> = rust_inventory
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    shell_set.sort();
    rust_set.sort();
    assert_eq!(shell_set.len(), 65, "shell inventory size");
    assert_eq!(rust_set.len(), 65, "rust inventory size");
    let palette = dot::progress_ui::Palette::empty();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let mut shell_ms = Vec::new();
    let mut rust_ms = Vec::new();
    for round in 0..5 {
        // Fresh shell home per round.
        let home = fix.join(format!("bench-shell-home-{round}"));
        std::fs::create_dir_all(&home).expect("round home");
        let manifest_new = fix.join(format!("bench-shell-man-{round}"));
        std::fs::write(&manifest_new, b"").expect("round manifest");
        let ov_text = shell_base.ov_path.to_string_lossy().into_owned();
        let snippet = format!(
            "export HOME={}; OVERLAYS=('ov|{ov_text}|{}|||git'); declare -A _overlay_authority_paths=() _overlay_authority_targets=() _overlay_current_paths=() _base_tracked=(); _overlay_reserved_roots={}; _overlay_manifest_new={}; _base_git() {{ return 1; }}; start=$(date +%s%N); _link_overlay ov {} {} git >/dev/null; code=$?; end=$(date +%s%N); printf 'rc=%s\nms=%s\nreply=%s\n' \"$code\" \"$(((end - start) / 1000000))\" \"$REPLY\"; LC_ALL=C sort {} | wc -l\n",
            sq(&home.to_string_lossy()),
            shell_base.url,
            sq(&shell_roots),
            sq(&manifest_new.to_string_lossy()),
            sq(&ov_text),
            sq(&shell_inv),
            sq(&manifest_new.to_string_lossy()),
        );
        let (code, out, _) = shell_run(&home, &snippet);
        assert_eq!(code, 0, "bench harness");
        let text = String::from_utf8(out).expect("bench dump");
        assert!(text.starts_with("rc=0\n"), "shell link rc: {text}");
        assert!(
            text.contains("reply=ov overlay linked 65\n"),
            "shell reply: {text}"
        );
        let ms: u128 = text
            .lines()
            .find_map(|line| line.strip_prefix("ms="))
            .expect("ms")
            .parse()
            .expect("ms number");
        shell_ms.push(ms);
        // Fresh Rust home per round.
        let home = fix.join(format!("bench-rust-home-{round}"));
        std::fs::create_dir_all(&home).expect("round home");
        let manifest_new = fix.join(format!("bench-rust-man-{round}"));
        std::fs::write(&manifest_new, b"").expect("round manifest");
        let ov_text = rust_base.ov_path.to_string_lossy().into_owned();
        let home_text = home.to_string_lossy().into_owned();
        let pwd = std::fs::canonicalize(&home)
            .expect("canonical")
            .to_string_lossy()
            .into_owned();
        let dest = dot::repos_overlays::DestinationInputs {
            home: home_text.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![ov_text.clone()],
            init_backup: None,
            pwd,
        };
        // Twin overlay paths: rel derivation strips this prefix,
        // so it must match the inventory's own prefix.
        let ov_text = rust_base.ov_path.to_string_lossy().into_owned();
        let entry = format!("ov|{ov_text}|{}|||git", rust_base.url);
        let overlays = vec![entry];
        let empty: HashSet<String> = HashSet::new();
        let no_targets: Vec<(String, String)> = Vec::new();
        let inputs = repos_link_exec::Inputs {
            name: "ov",
            path: &ov_text,
            sync: "git",
            home: &home_text,
            overlay_home: &format!("{ov_text}/home"),
            overlays: &overlays,
            dest: &dest,
            reserved_roots: Some(&shell_roots),
            authority_targets: &no_targets,
            base: None,
            base_tracked: &empty,
            manifest: "/nonexistent/manifest.tsv",
            legacy_manifest: "/nonexistent/legacy.tsv",
            manifest_new: &manifest_new,
            source_root: None,
            source_identity: None,
            euid,
            source_root_git: fix,
            tmp: fix,
            tool: &tool,
            palette: &palette,
            multibyte: false,
            dot_quiet: None,
            dot_verbose: None,
            ui_total: None,
        };
        let mut state = repos_link_exec::OverlayState::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let start = Instant::now();
        let outcome =
            repos_link_exec::link_overlay(&inputs, &mut state, &rust_inventory, &mut out, &mut err);
        let ms = start.elapsed().as_millis();
        assert!(
            matches!(outcome, repos_link_exec::Outcome::Changed(_)),
            "rust link outcome"
        );
        assert_eq!(state.current.len(), 65, "rust record count");
        rust_ms.push(ms);
    }
    println!(
        "link_exec 65-file overlay medians over 5 fresh runs: shell={}ms rust={}ms (runs shell={:?} rust={:?})",
        median_ms(shell_ms.clone()),
        median_ms(rust_ms.clone()),
        shell_ms,
        rust_ms,
    );
}

#[test]
fn verbose_rows_match_shell() {
    let dir = TempDir::new("linkexec-verbose").expect("fixture dir");
    let fix = dir.path();
    let case = ShellCase {
        sync: "git",
        verbose: true,
        authority: &[],
        prelude: "",
    };
    assert_twins(fix, "verbose", &[("a.conf", "a\n")], &[], &case, &|_| {});
}
