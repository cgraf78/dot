//! Differential parity and phase-benchmark coverage for the native
//! link phase (`src/repos_link_all.rs`, porting `_link_overlays`
//! from `lib/dot/repos/overlays.sh`).
//!
//! Every case runs the live shell phase and its Rust twin on twin
//! fixtures (identical content, separate directories) and compares
//! exit codes, stdout/stderr streams, manifest records, and the
//! converged HOME trees. Stage-row elapsed stamps (`0s`, `12s`)
//! read from each side's own clock, so both dumps scrub them before
//! comparing; everything else compares byte for byte. The closing
//! benchmark times one multi-file link phase on each side and
//! reports both medians alongside the parity assertion.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_link_all;
use dot::test_support::TempDir;

/// Run one shell snippet with the overlay runtime sourced.
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile").arg("--norc").arg("-c").arg(format!(
        ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/overlays.sh\"\n. \"$1/lib/dot/repos/model.sh\"\n. \"$1/lib/dot/repos/pull.sh\"\n. \"$1/lib/dot/temp.sh\"\n. \"$1/lib/dot/reserved.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n. \"$1/lib/dot/repos/config.sh\"\n. \"$1/lib/dot/log.sh\"\n. \"$1/lib/dot/progress-ui.sh\"\n. \"$1/lib/dot/init-client.sh\"\n{snippet}"
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

/// Replace one side root with `{SIDE}` so twin dumps compare, and
/// scrub stage-row elapsed stamps (each side reads its own clock).
fn scrub(dump: &str, root: &Path) -> String {
    let dump = dump.replace(&root.to_string_lossy().into_owned(), "{SIDE}");
    let mut scrubbed = String::with_capacity(dump.len());
    let bytes = dump.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'(' {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 1 && end < bytes.len() && bytes[end] == b's' {
                let close = end + 1;
                if close < bytes.len() && bytes[close] == b')' {
                    scrubbed.push_str("(STAMP)");
                    index = close + 1;
                    continue;
                }
            }
        }
        scrubbed.push(bytes[index] as char);
        index += 1;
    }
    scrubbed
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

/// One fixture side: home, overlay checkout(s), and manifests.
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

/// Shell `_link_overlays` over one side plus a full state dump.
/// `ui_total`/`jobs` export the counted-UI and fan-out knobs;
/// `prelude` runs extra setup (authority seeds, second overlay)
/// before the phase call.
struct ShellCase<'a> {
    sync: &'a str,
    verbose: bool,
    ui_total: Option<&'a str>,
    jobs: Option<&'a str>,
    prelude: &'a str,
}

/// Dump one linked side from the shell: rc, streams, manifest
/// records (sorted), and the HOME tree.
fn shell_snippet(side: &Side, case: &ShellCase<'_>) -> String {
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
    if case.verbose {
        out.push_str("export DOT_VERBOSE=1; ");
    }
    if let Some(total) = case.ui_total {
        out.push_str(&format!("export DOT_UI_TOTAL={}; ", sq(total)));
    }
    if let Some(jobs) = case.jobs {
        out.push_str(&format!("export DOT_UPDATE_JOBS={}; ", sq(jobs)));
    }
    out.push_str(&format!(
        "_base_git() {{ git --git-dir={} --work-tree={} \"$@\"; }}; ",
        sq(&side.root.join("base.git").to_string_lossy()),
        sq(&side.home.to_string_lossy()),
    ));
    out.push_str(case.prelude);
    out.push(' ');
    out.push_str(concat!(
        "cap_out=$(mktemp); cap_err=$(mktemp); ",
        "_link_overlays >\"$cap_out\" 2>\"$cap_err\"; code=$?; ",
        "printf 'rc=%s\\n' \"$code\"; ",
        "printf 'out='; cat \"$cap_out\"; printf 'err='; cat \"$cap_err\"; rm -f \"$cap_out\" \"$cap_err\"; ",
        "if [[ -f \"$DOT_OVERLAY_MANIFEST\" ]]; then LC_ALL=C sort \"$DOT_OVERLAY_MANIFEST\" | while IFS= read -r line; do printf 'man\\t%s\\n' \"$line\"; done; fi; ",
        "_base_git ls-files -v 2>/dev/null | LC_ALL=C sort | while IFS= read -r line; do printf 'idx\\t%s\\n' \"$line\"; done; ",
        "cd \"$HOME\" && find . -mindepth 1 \\( -name '.git*' \\) -prune -o -print0 | LC_ALL=C sort -z | ",
        "while IFS= read -r -d '' p; do ",
        "if [[ -L $p ]]; then printf 'tree\\tlink\\t%s\\t%s\\n' \"$p\" \"$(readlink \"$p\")\"; ",
        "elif [[ -d $p ]]; then printf 'tree\\tdir\\t%s\\n' \"$p\"; ",
        "elif [[ -f $p ]]; then printf 'tree\\tfile\\t%s\\t%s\\n' \"$p\" \"$(sha256sum <\"$p\" | cut -d' ' -f1)\"; ",
        "else printf 'tree\\tother\\t%s\\n' \"$p\"; fi; done\n",
    ));
    out
}

/// Run the shell phase once; returns the scrubbed dump.
fn run_shell(side: &Side, case: &ShellCase<'_>) -> String {
    let (code, out, _serr) = shell_run(&side.home, &shell_snippet(side, case));
    assert_eq!(code, 0, "harness exit");
    scrub(&String::from_utf8(out).expect("shell dump"), &side.root)
}

/// Walk one HOME into the shell `tree` dump shape (`.git*` pruned).
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

/// Unix seconds for the stage stamps (each side reads its own
/// clock; the scrubber normalizes them).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_secs() as i64
}

/// Run the Rust twin over one side and render the shell dump shape.
fn run_rust(side: &Side, case: &ShellCase<'_>) -> String {
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
    let base = base_git_dir.exists().then(|| dot::repos_base::Base {
        topology: dot::repos_base::Topology::Separate,
        client_git_dir: base_git_dir.to_string_lossy().into_owned(),
        home: home_text.clone(),
    });
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let log = dot::log::Log::new(false, false);
    let sync = if case.sync.is_empty() {
        "git"
    } else {
        case.sync
    };
    let entry = format!("ov|{ov_text}|{}|||{sync}", side.url);
    let entries = vec![entry];
    let total = case.ui_total.unwrap_or("0");
    let mut stage =
        dot::progress_ui::Stage::begin(palette.clone(), total, false, false, false, true);
    let inputs = repos_link_all::Inputs {
        entries: &entries,
        home: &home_text,
        manifest: &side.manifest,
        legacy_manifest: &side.legacy,
        update_jobs: case.jobs,
        ui_total: case.ui_total,
        dot_verbose: if case.verbose { Some("1") } else { None },
        dot_quiet: None,
        dest: &dest,
        base: base.as_ref(),
        euid,
        source_root_git: &side.root,
        tmp: &side.root,
        tool: &tool,
        palette: &palette,
        multibyte: false,
        bar_width: "8",
        log: &log,
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let outcome =
        repos_link_all::link_overlays(&inputs, &mut stage, &mut out, &mut err, now_secs());
    let mut text = format!("rc={}\n", outcome.rc);
    text.push_str("out=");
    text.push_str(&String::from_utf8_lossy(&out));
    text.push_str("err=");
    text.push_str(&String::from_utf8_lossy(&err));
    for line in sorted_lines(Path::new(&side.manifest)) {
        text.push_str(&format!("man\t{line}\n"));
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

/// One twin comparison: identical fixture content on separate
/// sides, shell phase then Rust twin, dumps byte-equal. `rerun`
/// repeats the phase on each converged side and compares the
/// second runs (the steady-state tally).
fn check_phase(
    tag: &str,
    payload: &[(&str, &str)],
    base: &[(&str, &str)],
    case: &ShellCase<'_>,
    rerun: bool,
) {
    let fix = TempDir::new(&format!("linkall-{tag}")).expect("fixture dir");
    let shell_side = setup_side(fix.path(), "shell", payload, base);
    let rust_side = setup_side(fix.path(), "rust", payload, base);
    let shell_first = run_shell(&shell_side, case);
    let rust_first = run_rust(&rust_side, case);
    assert_eq!(rust_first, shell_first, "{tag} first-run parity");
    if rerun {
        let shell_second = run_shell(&shell_side, case);
        let rust_second = run_rust(&rust_side, case);
        assert_eq!(rust_second, shell_second, "{tag} second-run parity");
        assert!(
            shell_second.starts_with("rc=0\n"),
            "{tag} shell converges: {shell_second}"
        );
    } else {
        assert!(
            shell_first.starts_with("rc=0\n"),
            "{tag} shell converges: {shell_first}"
        );
    }
}

#[test]
fn fresh_link_all_matches_shell() {
    let case = ShellCase {
        sync: "git",
        verbose: false,
        ui_total: None,
        jobs: None,
        prelude: "",
    };
    check_phase(
        "fresh",
        &[
            ("app.conf", "fresh app\n"),
            ("sub/nested.conf", "nested\n"),
            ("data.txt", "data\n"),
        ],
        &[],
        &case,
        false,
    );
}

#[test]
fn converged_second_run_matches_shell() {
    let case = ShellCase {
        sync: "git",
        verbose: false,
        ui_total: None,
        jobs: Some("2"),
        prelude: "",
    };
    check_phase(
        "converged",
        &[("app.conf", "steady\n"), ("sub/deep.conf", "deep\n")],
        &[("tracked.txt", "base\n")],
        &case,
        true,
    );
}

#[test]
fn counted_ui_matches_shell() {
    let case = ShellCase {
        sync: "git",
        verbose: true,
        ui_total: Some("4"),
        jobs: None,
        prelude: "",
    };
    check_phase(
        "counted",
        &[("app.conf", "counted\n"), ("other.conf", "other\n")],
        &[],
        &case,
        false,
    );
}

#[test]
fn skips_match_shell() {
    // A git overlay that is not a worktree warns and continues;
    // the local overlay still links. The prelude swaps the git
    // checkout for a plain directory and stages the local source.
    let fix = TempDir::new("linkall-skips").expect("fixture dir");
    let payload = &[("keep.conf", "keep\n")];
    let shell_side = setup_side(fix.path(), "shell", payload, &[]);
    let rust_side = setup_side(fix.path(), "rust", payload, &[]);
    // Replace each git checkout with a plain directory: the
    // declaration still claims `git` sync, so both sides warn and
    // continue through the standard single-entry path.
    for side in [&shell_side, &rust_side] {
        let ov = &side.ov_path;
        let backup = side.root.join("ov-backup");
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(ov, &backup).expect("stash checkout");
        std::fs::create_dir_all(ov.join("home")).expect("flat ov home");
        std::fs::write(ov.join("home/flat.conf"), b"flat\n").expect("flat payload");
    }
    let flat_case = ShellCase {
        sync: "git",
        verbose: false,
        ui_total: None,
        jobs: None,
        prelude: "",
    };
    let shell_dump = run_shell(&shell_side, &flat_case);
    let rust_dump = run_rust(&rust_side, &flat_case);
    assert_eq!(rust_dump, shell_dump, "non-worktree parity");
    assert!(
        shell_dump.starts_with("rc=0\n"),
        "shell skips: {shell_dump}"
    );
    assert!(
        shell_dump.contains("not a Git worktree"),
        "shell warns: {shell_dump}"
    );
}

#[test]
fn origin_mismatch_matches_shell() {
    let fix = TempDir::new("linkall-mismatch").expect("fixture dir");
    let payload = &[("app.conf", "mismatch\n")];
    let shell_side = setup_side(fix.path(), "shell", payload, &[]);
    let rust_side = setup_side(fix.path(), "rust", payload, &[]);
    for side in [&shell_side, &rust_side] {
        git(
            &side.ov_path,
            &side.home,
            &["remote", "set-url", "origin", "file:///elsewhere.git"],
        );
    }
    let case = ShellCase {
        sync: "git",
        verbose: false,
        ui_total: None,
        jobs: None,
        prelude: "",
    };
    let shell_dump = run_shell(&shell_side, &case);
    let rust_dump = run_rust(&rust_side, &case);
    assert_eq!(rust_dump, shell_dump, "mismatch parity");
    assert!(
        shell_dump.starts_with("rc=0\n"),
        "shell continues: {shell_dump}"
    );
    assert!(
        shell_dump.contains("does not match its configured URL"),
        "shell warns: {shell_dump}"
    );
}

#[test]
fn stale_cleanup_matches_shell() {
    // Two-phase: link with an extra file, drop it from the overlay,
    // link again. The stale symlink is removed and the manifest
    // rewritten on both sides.
    let fix = TempDir::new("linkall-stale").expect("fixture dir");
    let shell_side = setup_side(
        fix.path(),
        "shell",
        &[("keep.conf", "keep\n"), ("gone.conf", "gone\n")],
        &[("shadow.txt", "base shadow\n")],
    );
    let rust_side = setup_side(
        fix.path(),
        "rust",
        &[("keep.conf", "keep\n"), ("gone.conf", "gone\n")],
        &[("shadow.txt", "base shadow\n")],
    );
    let case = ShellCase {
        sync: "git",
        verbose: false,
        ui_total: None,
        jobs: Some("2"),
        prelude: "",
    };
    let shell_first = run_shell(&shell_side, &case);
    let rust_first = run_rust(&rust_side, &case);
    assert_eq!(rust_first, shell_first, "stale first-run parity");
    for side in [&shell_side, &rust_side] {
        let work = side.root.join("ov-work");
        std::fs::remove_file(work.join("home/gone.conf")).expect("drop file");
        git(&work, &side.home, &["add", "-A"]);
        git(&work, &side.home, &["commit", "-qm", "drop"]);
        git(&side.ov_path, &side.home, &["fetch", "-q", "origin"]);
        git(
            &side.ov_path,
            &side.home,
            &["reset", "-q", "--hard", "origin/main"],
        );
    }
    let shell_second = run_shell(&shell_side, &case);
    let rust_second = run_rust(&rust_side, &case);
    assert_eq!(rust_second, shell_second, "stale second-run parity");
    assert!(
        shell_second.contains("removed: gone.conf"),
        "shell cleans: {shell_second}"
    );
}

/// Median helper for the benchmark.
fn median_ms(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
fn benchmark_phase_reports_medians() {
    // One 60-file overlay per side; fresh phase per round on
    // fresh homes so every sample links the full set.
    let fix = TempDir::new("linkall-bench").expect("fixture dir");
    let payload: Vec<(String, String)> = (0..60)
        .map(|index| {
            (
                format!("f{index:02}.conf"),
                format!("bench payload {index}\n"),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = payload
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    let case = ShellCase {
        sync: "git",
        verbose: false,
        ui_total: None,
        jobs: None,
        prelude: "",
    };
    let mut shell_ms = Vec::new();
    let mut rust_ms = Vec::new();
    for round in 0..5 {
        let shell_side = setup_side(fix.path(), &format!("bench-shell-{round}"), &refs, &[]);
        let rust_side = setup_side(fix.path(), &format!("bench-rust-{round}"), &refs, &[]);
        let start = std::time::Instant::now();
        let shell_dump = run_shell(&shell_side, &case);
        shell_ms.push(start.elapsed().as_millis());
        let start = std::time::Instant::now();
        let rust_dump = run_rust(&rust_side, &case);
        rust_ms.push(start.elapsed().as_millis());
        assert_eq!(rust_dump, shell_dump, "bench round {round} parity");
    }
    eprintln!(
        "link_all phase medians over 5 fresh runs: shell={}ms rust={}ms (runs shell={:?} rust={:?})",
        median_ms(shell_ms.clone()),
        median_ms(rust_ms.clone()),
        shell_ms,
        rust_ms,
    );
}
