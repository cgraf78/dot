//! Differential parity tests for the overlay worker fleet
//! (`lib/dot/repos/pull.sh`) against the live shell: the capture
//! worker, the drain replay, the serial fallback, the bound fan-out
//! with declaration-order replay, and the top-level synchronized-set
//! pull.
//!
//! Separate binary because the rows drive real `git clone`/`git pull`
//! runs: each side builds its own origins plus checkouts under
//! disjoint directories, so paths and hashes normalize before
//! comparing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::progress_ui::{Palette, Stage};
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull_fleet::{
    PullAllInputs, PullOverlaysInputs, active_overlays, drain_result_dir, overlay_capture,
    pull_all, pull_overlays, pull_overlays_serial,
};
use dot::repos_pull_queries::CandidateEnv;
use dot::test_support::TempDir;

/// Sources for the fleet chapter: the pull runtime plus the job
/// bound (`update.sh`) and the repo filter (`repos/dirty.sh`) the
/// fan-out and top-level pull read.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/repos/dirty.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/progress-ui.sh\"\n",
    ". \"$1/lib/dot/run.sh\"\n",
    ". \"$1/lib/dot/update.sh\"\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the pull runtime sourced. The locale
/// stays pinned: git output must read English on both engines.
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
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

/// Run git for fixtures, with a pinned identity for commits.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// Write `bytes` to `dir/name`, creating parents.
fn stage(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Commit everything with `message`.
fn commit(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Replace side-local paths and hex runs (hashes, stamps, modes)
/// so twin dumps compare. Fixture words avoid long lowercase-hex
/// runs by construction.
fn normalize(text: &str, home: &str, origin: &str) -> String {
    let text = text.replace(home, "@HOME@").replace(origin, "@ORIGIN@");
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let run = bytes[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
        if run >= 7 {
            out.push_str("@HEX@");
            index += run;
        } else {
            out.push(bytes[index] as char);
            index += 1;
        }
    }
    out
}

/// Replace the trailing elapsed stamp (` 0s`, ` 12s`) on stage
/// lines so wall-clock timing never flakes the comparison.
fn normalize_elapsed(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with('[') {
            if let Some(stripped) = line.strip_suffix('\n') {
                if let Some((head, _)) = stripped.rsplit_once(' ') {
                    out.push_str(head);
                    out.push_str(" @ELAPSED@\n");
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    out
}

/// Candidate environment mirroring the shell preamble.
fn candidate_env(home_text: &str) -> CandidateEnv {
    CandidateEnv {
        home: home_text.to_string(),
        checkout: format!("{home_text}/.local/share/cgraf78/dot"),
        pwd: home_text.to_string(),
        source_root: env!("CARGO_MANIFEST_DIR").to_string(),
        state_home: format!("{home_text}/.local/state"),
        install_root: format!("{home_text}/.local/share"),
        provider_state: format!("{home_text}/.local/state/shdeps"),
        overlay_paths: Vec::new(),
        init_backup: None,
    }
}

/// Piped-output palette: the shell disables colors off-tty, so the
/// port matches with every cell empty.
fn plain_palette() -> Palette {
    Palette {
        reset: String::new(),
        bold: String::new(),
        dim: String::new(),
        green: String::new(),
        yellow: String::new(),
        red: String::new(),
        blue: String::new(),
        cyan: String::new(),
        white: String::new(),
    }
}

/// Empty stage for fleet runs: piped (never live), ASCII bars under
/// the pinned `LC_ALL=C`, byte-counted cells.
fn fleet_stage(ui_total: Option<&str>, quiet: bool) -> Stage {
    Stage::begin(
        plain_palette(),
        ui_total.unwrap_or("0"),
        quiet,
        false,
        false,
        true,
    )
}

/// One twin side for capture rows: an origin plus a clone at
/// `$HOME/overlay`.
struct CaptureSide {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    origin: PathBuf,
    origin_text: String,
    overlay_text: String,
    manifest: String,
    legacy: String,
}

impl CaptureSide {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).expect("origin dir");
        git(&origin, &["init", "-q"]);
        stage(&origin, "home/overlay.txt", b"v1\n");
        commit(&origin, "seed");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home dir");
        let overlay = home.join("overlay");
        let status = Command::new("git")
            .arg("clone")
            .arg("-q")
            .arg(&origin)
            .arg(&overlay)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn git clone");
        assert!(status.success(), "clone for {tag}");
        let home_text = home.to_string_lossy().into_owned();
        let origin_text = origin.to_string_lossy().into_owned();
        let overlay_text = overlay.to_string_lossy().into_owned();
        CaptureSide {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            origin,
            origin_text,
            overlay_text,
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }
}

/// Shell preamble for capture rows.
fn capture_preamble(
    side: &CaptureSide,
    ui_total: Option<&str>,
    quiet: bool,
    verbose: bool,
) -> String {
    format!(
        "export HOME={h} DOT_QUIET={q} DOT_VERBOSE={v} XDG_STATE_HOME={h}/.local/state SHDEPS_INSTALL_DIR={h}/.local/share; \
         {u}DOT_OVERLAY_ROLLBACK_PATHS=(); DOT_OVERLAY_ROLLBACK_TARGETS=(); OVERLAYS=(); ACTIVE_OVERLAYS=(); \
         DOT_OVERLAY_MANIFEST={m} DOT_OVERLAY_LEGACY_MANIFEST={l}; DOT_BASE_TOPOLOGY=ordinary; ",
        h = sq(&side.home_text),
        q = u8::from(quiet),
        v = u8::from(verbose),
        u = ui_total
            .map(|total| format!("export DOT_UI_TOTAL={total}; "))
            .unwrap_or_default(),
        m = sq(&side.manifest),
        l = sq(&side.legacy),
    )
}

#[test]
fn overlay_capture_writes_indexed_files() {
    // (tag, ui_total, quiet, verbose, setup): `changed` advances the
    // origin so the pull moves; `current` stays put.
    for (tag, ui_total, quiet, verbose, changed) in [
        ("capture-current", None, false, false, false),
        ("capture-changed", None, false, false, true),
    ] {
        let shell_side = CaptureSide::build(&format!("{tag}-shell"));
        let rust_side = CaptureSide::build(&format!("{tag}-rust"));
        if changed {
            stage(&shell_side.origin, "home/newfile.txt", b"from origin\n");
            commit(&shell_side.origin, "add newfile");
            stage(&rust_side.origin, "home/newfile.txt", b"from origin\n");
            commit(&rust_side.origin, "add newfile");
        }
        let shell_result = shell_side.home.join("results");
        std::fs::create_dir_all(&shell_result).expect("shell results");
        let shell_result_text = shell_result.to_string_lossy().into_owned();
        let snippet = format!(
            "{}{}",
            capture_preamble(&shell_side, ui_total, quiet, verbose),
            format_args!(
                "_pull_overlay_capture 7 {} {} {} {} {}; code=$?; printf 'rc=%s\\n' \"$code\"; \
                 printf 'log<<<\\n'; cat {}/007.log; printf '>>>\\n'; \
                 printf 'rcfile=%s\\n' \"$(cat {}/007.rc)\"; \
                 printf 'status=%s\\n' \"$(cat {}/007.status)\"",
                sq(&shell_result_text),
                sq("wname"),
                sq(&shell_side.overlay_text),
                sq(&shell_side.origin_text),
                "false",
                sq(&shell_result_text),
                sq(&shell_result_text),
                sq(&shell_result_text),
            ),
        );
        let (code, out, err) = shell_run(&shell_side.home, &snippet);
        assert_eq!(code, 0, "harness exit for {tag}");
        assert!(err.is_empty(), "capture stderr for {tag}: {err:?}");
        let shell_dump = normalize(
            &String::from_utf8(out).expect("shell dump"),
            &shell_side.home_text,
            &shell_side.origin_text,
        );

        let rust_result = rust_side.home.join("results");
        std::fs::create_dir_all(&rust_result).expect("rust results");
        let home_text = rust_side.home_text.clone();
        let dest = DestinationInputs {
            pwd: home_text.clone(),
            home: home_text.clone(),
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths: vec![],
            init_backup: None,
        };
        let mut moves = dot::temp::MoveCache::default();
        let tool = moves.tool().expect("move tool");
        let base = Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home_text.clone(),
        };
        let candidate = candidate_env(&home_text);
        let logger = Log::new(false, false);
        let palette = plain_palette();
        let empty: &[OsString] = &[];
        let entries = [format!(
            "wname|{}|{}|x|false|git",
            rust_side.overlay_text, rust_side.origin_text
        )];
        let active = active_overlays(&entries);
        assert_eq!(active.len(), 1, "capture row is active");
        let quiet_text = if quiet { "1" } else { "0" };
        let verbose_text = if verbose { "1" } else { "0" };
        let fleet_inputs = PullOverlaysInputs {
            entries: &entries,
            extra_args: empty,
            home: &home_text,
            ui_total,
            dot_quiet: Some(quiet_text),
            dot_verbose: Some(verbose_text),
            update_jobs: None,
            progress_done: None,
            progress_total: None,
            bar_width: "8",
            palette: &palette,
            multibyte: false,
            ascii: true,
            candidate: &candidate,
            base: &base,
            quarantine: None,
            overlays: &[],
            dest: &dest,
            manifest: &rust_side.manifest,
            legacy_manifest: &rust_side.legacy,
            euid: dot::temp::current_uid().expect("uid"),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            tmp: &rust_side.home,
            tool: &tool,
            log: &logger,
        };
        let (_status, rc) = overlay_capture(7, &rust_result, &active[0], &fleet_inputs, &mut moves);
        assert_eq!(rc, 0, "capture pull rc for {tag}");
        let log = std::fs::read_to_string(rust_result.join("007.log")).expect("rust log");
        let rc_file = std::fs::read_to_string(rust_result.join("007.rc")).expect("rust rc");
        let status_file =
            std::fs::read_to_string(rust_result.join("007.status")).expect("rust status");
        let rust_dump = format!("rc=0\nlog<<<\n{log}>>>\nrcfile={rc_file}\nstatus={status_file}\n");
        let rust_dump = normalize(&rust_dump, &rust_side.home_text, &rust_side.origin_text);
        assert_eq!(rust_dump, shell_dump, "capture files for {tag}");
    }
}

#[test]
fn drain_replays_logs_in_order_and_removes_dir() {
    let dir = TempDir::new("fleet-drain").expect("fixture dir");
    for (tag, files) in [
        ("empty-only", vec![("001.log", ""), ("002.log", "")]),
        (
            "mixed",
            vec![
                ("001.log", "first\n"),
                ("002.log", ""),
                ("010.log", "tenth\n"),
            ],
        ),
    ] {
        let shell_root = dir.path().join(format!("{tag}-shell"));
        let rust_root = dir.path().join(format!("{tag}-rust"));
        for root in [&shell_root, &rust_root] {
            std::fs::create_dir_all(root).expect("result dir");
        }
        for (name, content) in &files {
            std::fs::write(shell_root.join(name), content).expect("shell log");
            std::fs::write(rust_root.join(name), content).expect("rust log");
        }
        // Extra non-log files are ignored by both sides.
        std::fs::write(shell_root.join("001.rc"), "0").expect("shell rc");
        std::fs::write(rust_root.join("001.rc"), "0").expect("rust rc");
        let (code, out, _) = shell_run(
            dir.path(),
            &format!(
                "_pull_overlay_drain_workers {}; printf 'gone=%s\\n' \"$([[ -e {} ]] && printf no || printf yes)\"",
                sq(&shell_root.to_string_lossy()),
                sq(&shell_root.to_string_lossy()),
            ),
        );
        assert_eq!(code, 0, "harness exit for {tag}");
        let shell_text = String::from_utf8(out).expect("shell drain");
        let mut rust_out = Vec::new();
        drain_result_dir(&rust_root, &mut rust_out);
        let gone = if rust_root.exists() { "no" } else { "yes" };
        let mut rust_text = String::from_utf8(rust_out).expect("rust drain");
        rust_text.push_str(&format!("gone={gone}\n"));
        assert_eq!(rust_text, shell_text, "drain parity for {tag}");
    }
}

/// One twin side for fleet rows: a home dir plus `count` origins
/// with clones at `$HOME/ovl{i}`.
struct FleetSide {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    origins: Vec<PathBuf>,
    overlays: Vec<PathBuf>,
    manifest: String,
    legacy: String,
}

impl FleetSide {
    fn build(tag: &str, count: usize) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home dir");
        let mut origins = Vec::new();
        let mut overlays = Vec::new();
        for index in 0..count {
            let origin = dir.path().join(format!("origin{index}"));
            std::fs::create_dir_all(&origin).expect("origin dir");
            git(&origin, &["init", "-q"]);
            stage(&origin, "home/overlay.txt", b"v1\n");
            commit(&origin, "seed");
            let overlay = home.join(format!("ovl{index}"));
            let status = Command::new("git")
                .arg("clone")
                .arg("-q")
                .arg(&origin)
                .arg(&overlay)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn git clone");
            assert!(status.success(), "clone {index} for {tag}");
            origins.push(origin);
            overlays.push(overlay);
        }
        let home_text = home.to_string_lossy().into_owned();
        FleetSide {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            origins,
            overlays,
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }

    /// `OVERLAYS` entries (`ovl{i}|path|origin-url|x|false|git`).
    fn entries(&self) -> Vec<String> {
        self.overlays
            .iter()
            .enumerate()
            .map(|(index, overlay)| {
                format!(
                    "ovl{index}|{}|{}|x|false|git",
                    overlay.to_string_lossy(),
                    self.origins[index].to_string_lossy(),
                )
            })
            .collect()
    }
}

/// Per-row overlay fixture after the seed clones.
fn setup_fleet_side(side: &FleetSide, tag: &str) {
    match tag {
        "current-2" | "current-2-jobs1" => {}
        "changed-2" => {
            for origin in &side.origins {
                stage(origin, "home/newfile.txt", b"from origin\n");
                commit(origin, "add newfile");
            }
        }
        "mixed-3" | "mixed-3-jobs1" => {
            stage(&side.origins[0], "home/newfile.txt", b"from origin\n");
            commit(&side.origins[0], "add newfile");
            git(&side.overlays[2], &["branch", "--unset-upstream"]);
        }
        _ => unreachable!("unknown fleet row {tag}"),
    }
}

/// Shell preamble for fleet rows: home, UI flags, empty rollback
/// records, manifests, progress, and the `OVERLAYS` array.
#[allow(clippy::too_many_arguments)]
fn fleet_preamble(
    side: &FleetSide,
    entries: &[String],
    ui_total: Option<&str>,
    quiet: bool,
    verbose: bool,
    jobs: Option<&str>,
    done: &str,
    total: &str,
) -> String {
    let quoted: Vec<String> = entries.iter().map(|entry| sq(entry)).collect();
    format!(
        "export HOME={h} DOT_QUIET={q} DOT_VERBOSE={v} XDG_STATE_HOME={h}/.local/state SHDEPS_INSTALL_DIR={h}/.local/share; \
         {u}DOT_OVERLAY_ROLLBACK_PATHS=(); DOT_OVERLAY_ROLLBACK_TARGETS=(); OVERLAYS=({o}); ACTIVE_OVERLAYS=(); \
         DOT_OVERLAY_MANIFEST={m} DOT_OVERLAY_LEGACY_MANIFEST={l}; DOT_BASE_TOPOLOGY=ordinary; \
         DOT_REPO_PROGRESS_DONE={d} DOT_REPO_PROGRESS_TOTAL={t}; {j}",
        h = sq(&side.home_text),
        q = u8::from(quiet),
        v = u8::from(verbose),
        u = ui_total
            .map(|value| format!("export DOT_UI_TOTAL={value}; "))
            .unwrap_or_default(),
        o = quoted.join(" "),
        m = sq(&side.manifest),
        l = sq(&side.legacy),
        d = done,
        t = total,
        j = jobs
            .map(|value| format!("export DOT_UPDATE_JOBS={value}; "))
            .unwrap_or_default(),
    )
}

/// Aftermath probe: one `st=` line per overlay rel.
fn fleet_probe(side: &FleetSide, rel: &str) -> String {
    let mut out = String::new();
    for (index, overlay) in side.overlays.iter().enumerate() {
        out.push_str(&format!(
            "p={}; if [[ -f \"$p\" ]]; then printf 'st{index}={rel}:file:%s\\n' \"$(cat \"$p\")\"; \
             else printf 'st{index}={rel}:absent\\n'; fi; ",
            sq(&format!("{}/{rel}", overlay.to_string_lossy())),
        ));
    }
    out
}

/// Rust aftermath dump mirroring [`fleet_probe`].
fn fleet_rust_probe(side: &FleetSide, rel: &str) -> String {
    let mut out = String::new();
    for (index, overlay) in side.overlays.iter().enumerate() {
        match std::fs::read(overlay.join(rel)) {
            Ok(bytes) => out.push_str(&format!(
                "st{index}={rel}:file:{}\n",
                String::from_utf8_lossy(&bytes).trim_end_matches('\n')
            )),
            Err(_) => out.push_str(&format!("st{index}={rel}:absent\n")),
        }
    }
    out
}

/// Run one fleet row on twin sides through both the shell fan-out
/// (`_pull_overlays` or `_pull_overlays_serial`) and the Rust port,
/// comparing stdout, stderr, reply, tallies, progress, and
/// aftermath.
#[allow(clippy::too_many_arguments)]
fn check_fleet_row(
    tag: &str,
    count: usize,
    serial: bool,
    ui_total: Option<&str>,
    quiet: bool,
    verbose: bool,
    jobs: Option<&str>,
    probe_rel: &str,
) {
    let shell_side = FleetSide::build(&format!("{tag}-shell"), count);
    let rust_side = FleetSide::build(&format!("{tag}-rust"), count);
    setup_fleet_side(&shell_side, tag);
    setup_fleet_side(&rust_side, tag);
    let shell_entries = shell_side.entries();
    let prefix = fleet_preamble(
        &shell_side,
        &shell_entries,
        ui_total,
        quiet,
        verbose,
        jobs,
        "0",
        "0",
    );
    // `_pull_overlays_serial` reads the `_active_entries` globals
    // owned by the fan-out, so serial rows build them directly (all
    // test entries are active) and finish the reply/progress exactly
    // like the fallback branch does.
    let call = if serial {
        "_active_entries=(\"${OVERLAYS[@]}\"); _summaries=(); _done=0; _total=0; \
         DOT_PULL_OVERLAY_CURRENT=0; DOT_PULL_OVERLAY_CHANGED=0; DOT_PULL_OVERLAY_CHANGED_ITEMS=\"\"; \
         DOT_PULL_OVERLAY_FAILED=0; DOT_PULL_OVERLAY_SKIPPED=0; \
         _pull_overlays_serial; DOT_REPO_PROGRESS_DONE=\"$_done\"; REPLY=$(_join_comma \"${_summaries[@]}\"); "
    } else {
        "_pull_overlays; "
    };
    let snippet = format!(
        "{}{} code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"; \
         printf 'done=%s\\n' \"$DOT_REPO_PROGRESS_DONE\"; \
         printf 'tally=%s/%s/%s/%s\\n' \"$DOT_PULL_OVERLAY_CURRENT\" \"$DOT_PULL_OVERLAY_CHANGED\" \
         \"$DOT_PULL_OVERLAY_FAILED\" \"$DOT_PULL_OVERLAY_SKIPPED\"; \
         printf 'items<<<%s>>>' \"$DOT_PULL_OVERLAY_CHANGED_ITEMS\"; {}",
        prefix,
        format_args!("{call}"),
        fleet_probe(&shell_side, probe_rel),
    );
    let (code, out, err) = shell_run(&shell_side.home, &snippet);
    assert_eq!(code, 0, "harness exit for {tag}");
    let shell_dir = shell_side._dir.path().to_string_lossy().into_owned();
    let shell_out = normalize(
        &String::from_utf8(out).expect("shell dump"),
        &shell_side.home_text,
        &shell_dir,
    );
    let shell_err = normalize(
        &String::from_utf8(err).expect("shell warnings"),
        &shell_side.home_text,
        &shell_dir,
    );

    let home_text = rust_side.home_text.clone();
    let rust_entries = rust_side.entries();
    let dest = DestinationInputs {
        pwd: home_text.clone(),
        home: home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    let candidate = candidate_env(&home_text);
    let logger = Log::new(false, false);
    let palette = plain_palette();
    let empty: &[OsString] = &[];
    let quiet_text = if quiet { "1" } else { "0" };
    let verbose_text = if verbose { "1" } else { "0" };
    let fleet_inputs = PullOverlaysInputs {
        entries: &rust_entries,
        extra_args: empty,
        home: &home_text,
        ui_total,
        dot_quiet: Some(quiet_text),
        dot_verbose: Some(verbose_text),
        update_jobs: jobs,
        progress_done: Some("0"),
        progress_total: Some("0"),
        bar_width: "8",
        palette: &palette,
        multibyte: false,
        ascii: true,
        candidate: &candidate,
        base: &base,
        quarantine: None,
        overlays: &[],
        dest: &dest,
        manifest: &rust_side.manifest,
        legacy_manifest: &rust_side.legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &rust_side.home,
        tool: &tool,
        log: &logger,
    };
    let mut stage = fleet_stage(ui_total, quiet);
    let mut stdout = Vec::new();
    let mut warnings = Vec::new();
    let outcome = if serial {
        pull_overlays_serial(
            &fleet_inputs,
            &mut stage,
            &mut moves,
            &mut stdout,
            &mut warnings,
        )
    } else {
        pull_overlays(
            &fleet_inputs,
            &mut stage,
            &mut moves,
            &mut stdout,
            &mut warnings,
        )
    };
    let mut rust_stdout = String::from_utf8(stdout).expect("rust stdout");
    rust_stdout.push_str(&format!(
        "rc={}\nreply={}\ndone={}\ntally={}/{}/{}/{}\nitems<<<{}>>>",
        outcome.rc,
        outcome.reply,
        outcome.done,
        outcome.tally.current,
        outcome.tally.changed,
        outcome.tally.failed,
        outcome.tally.skipped,
        outcome.tally.changed_items,
    ));
    rust_stdout.push_str(&fleet_rust_probe(&rust_side, probe_rel));
    let rust_dir = rust_side._dir.path().to_string_lossy().into_owned();
    let rust_stdout = normalize(&rust_stdout, &rust_side.home_text, &rust_dir);
    let rust_err = normalize(
        &String::from_utf8(warnings).expect("rust warnings"),
        &rust_side.home_text,
        &rust_dir,
    );
    // Both sides normalize side dirs to placeholders; the reply and
    // tally lines contain no paths, and pull headers contain names
    // only, so the dumps compare directly.
    assert_eq!(rust_stdout, shell_out, "fleet stdout for {tag}");
    assert_eq!(rust_err, shell_err, "fleet stderr for {tag}");
}

#[test]
fn pull_overlays_serial_rows_agree() {
    for (tag, count, ui_total, quiet, verbose, jobs, rel) in [
        ("current-2", 2, None, false, false, None, "home/overlay.txt"),
        ("changed-2", 2, None, false, false, None, "home/newfile.txt"),
        ("mixed-3", 3, None, false, false, None, "home/newfile.txt"),
    ] {
        check_fleet_row(tag, count, true, ui_total, quiet, verbose, jobs, rel);
    }
}

#[test]
fn pull_overlays_parallel_rows_agree() {
    for (tag, count, ui_total, quiet, verbose, jobs, rel) in [
        (
            "current-2",
            2,
            None,
            false,
            false,
            Some("2"),
            "home/overlay.txt",
        ),
        (
            "changed-2",
            2,
            None,
            false,
            false,
            Some("2"),
            "home/newfile.txt",
        ),
        (
            "mixed-3",
            3,
            None,
            false,
            false,
            Some("2"),
            "home/newfile.txt",
        ),
        (
            "mixed-3-jobs1",
            3,
            None,
            false,
            false,
            Some("1"),
            "home/newfile.txt",
        ),
        (
            "current-2-jobs1",
            2,
            None,
            false,
            true,
            Some("1"),
            "home/overlay.txt",
        ),
    ] {
        check_fleet_row(tag, count, false, ui_total, quiet, verbose, jobs, rel);
    }
}

#[test]
fn pull_overlays_fallback_matches_serial() {
    // Blocked scratch (`TMPDIR` is a file) forces the shell fan-out
    // onto its serial branch; the Rust serial entry must agree. Only
    // current rows run here: changed rows need scratch for their
    // parent snapshots, so a blocked `TMPDIR` fails the pull itself
    // on both engines (not just the fan-out gate).
    let shell_side = FleetSide::build("fallback-shell", 2);
    let rust_side = FleetSide::build("fallback-rust", 2);
    let shell_entries = shell_side.entries();
    let blocker = shell_side.home.join("scratch-blocker");
    std::fs::write(&blocker, b"blocker\n").expect("blocker file");
    let snippet = format!(
        "{}export TMPDIR={}; _pull_overlays; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"; \
         printf 'done=%s\\n' \"$DOT_REPO_PROGRESS_DONE\"; \
         printf 'tally=%s/%s/%s/%s\\n' \"$DOT_PULL_OVERLAY_CURRENT\" \"$DOT_PULL_OVERLAY_CHANGED\" \
         \"$DOT_PULL_OVERLAY_FAILED\" \"$DOT_PULL_OVERLAY_SKIPPED\"; \
         printf 'items<<<%s>>>' \"$DOT_PULL_OVERLAY_CHANGED_ITEMS\"",
        fleet_preamble(
            &shell_side,
            &shell_entries,
            None,
            false,
            false,
            Some("2"),
            "0",
            "0"
        ),
        sq(&blocker.to_string_lossy()),
    );
    let (code, out, err) = shell_run(&shell_side.home, &snippet);
    assert_eq!(code, 0, "harness exit for fallback");
    let shell_dir = shell_side._dir.path().to_string_lossy().into_owned();
    let shell_out = normalize(
        &String::from_utf8(out).expect("shell dump"),
        &shell_side.home_text,
        &shell_dir,
    );
    let shell_err = normalize(
        &String::from_utf8(err).expect("shell warnings"),
        &shell_side.home_text,
        &shell_dir,
    );

    let home_text = rust_side.home_text.clone();
    let rust_entries = rust_side.entries();
    let dest = DestinationInputs {
        pwd: home_text.clone(),
        home: home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    let candidate = candidate_env(&home_text);
    let logger = Log::new(false, false);
    let palette = plain_palette();
    let empty: &[OsString] = &[];
    let fleet_inputs = PullOverlaysInputs {
        entries: &rust_entries,
        extra_args: empty,
        home: &home_text,
        ui_total: None,
        dot_quiet: Some("0"),
        dot_verbose: Some("0"),
        update_jobs: Some("2"),
        progress_done: Some("0"),
        progress_total: Some("0"),
        bar_width: "8",
        palette: &palette,
        multibyte: false,
        ascii: true,
        candidate: &candidate,
        base: &base,
        quarantine: None,
        overlays: &[],
        dest: &dest,
        manifest: &rust_side.manifest,
        legacy_manifest: &rust_side.legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &rust_side.home,
        tool: &tool,
        log: &logger,
    };
    let mut stage = fleet_stage(None, false);
    let mut stdout = Vec::new();
    let mut warnings = Vec::new();
    let outcome = pull_overlays_serial(
        &fleet_inputs,
        &mut stage,
        &mut moves,
        &mut stdout,
        &mut warnings,
    );
    let mut rust_stdout = String::from_utf8(stdout).expect("rust stdout");
    rust_stdout.push_str(&format!(
        "rc={}\nreply={}\ndone={}\ntally={}/{}/{}/{}\nitems<<<{}>>>",
        outcome.rc,
        outcome.reply,
        outcome.done,
        outcome.tally.current,
        outcome.tally.changed,
        outcome.tally.failed,
        outcome.tally.skipped,
        outcome.tally.changed_items,
    ));
    let rust_dir = rust_side._dir.path().to_string_lossy().into_owned();
    let rust_stdout = normalize(&rust_stdout, &home_text, &rust_dir);
    let rust_err = normalize(
        &String::from_utf8(warnings).expect("rust warnings"),
        &home_text,
        &rust_dir,
    );
    assert_eq!(rust_stdout, shell_out, "fallback stdout");
    assert_eq!(rust_err, shell_err, "fallback stderr");
}

/// One twin side for `_repo_pull_all` rows: a base origin plus a
/// base clone at `$HOME`, with `count` overlay origins and clones
/// beside (not under) the base so the two generations never share
/// tracked paths.
struct AllSide {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    base_origin: PathBuf,
    origins: Vec<PathBuf>,
    overlays: Vec<PathBuf>,
    manifest: String,
    legacy: String,
}

impl AllSide {
    fn build(tag: &str, count: usize) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let base_origin = dir.path().join("base-origin");
        std::fs::create_dir_all(&base_origin).expect("base origin");
        git(&base_origin, &["init", "-q"]);
        stage(&base_origin, "base.txt", b"v1\n");
        commit(&base_origin, "seed");
        let home = dir.path().join("home");
        let status = Command::new("git")
            .arg("clone")
            .arg("-q")
            .arg(&base_origin)
            .arg(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn git clone");
        assert!(status.success(), "clone base for {tag}");
        let mut origins = Vec::new();
        let mut overlays = Vec::new();
        for index in 0..count {
            let origin = dir.path().join(format!("origin{index}"));
            std::fs::create_dir_all(&origin).expect("origin dir");
            git(&origin, &["init", "-q"]);
            stage(&origin, "home/overlay.txt", b"v1\n");
            commit(&origin, "seed");
            let overlay = dir.path().join(format!("ovl{index}"));
            let status = Command::new("git")
                .arg("clone")
                .arg("-q")
                .arg(&origin)
                .arg(&overlay)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn git clone");
            assert!(status.success(), "clone {index} for {tag}");
            origins.push(origin);
            overlays.push(overlay);
        }
        let home_text = home.to_string_lossy().into_owned();
        AllSide {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            base_origin,
            origins,
            overlays,
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }

    fn entries(&self) -> Vec<String> {
        self.overlays
            .iter()
            .enumerate()
            .map(|(index, overlay)| {
                format!(
                    "ovl{index}|{}|{}|x|false|git",
                    overlay.to_string_lossy(),
                    self.origins[index].to_string_lossy(),
                )
            })
            .collect()
    }
}

/// Per-row fixture for `_repo_pull_all` rows.
fn setup_all_side(side: &AllSide, tag: &str) {
    match tag {
        "all-current" => {}
        "all-changed" => {
            stage(&side.base_origin, "newbase.txt", b"from origin\n");
            commit(&side.base_origin, "add newbase");
            for origin in &side.origins {
                stage(origin, "home/newfile.txt", b"from origin\n");
                commit(origin, "add newfile");
            }
        }
        "all-skipped-overlay" => {
            git(&side.overlays[0], &["branch", "--unset-upstream"]);
        }
        _ => unreachable!("unknown pull-all row {tag}"),
    }
}

/// Run one `_repo_pull_all` row on twin sides, comparing stdout,
/// stderr, status, tallies, and aftermath. `defer` exercises the
/// `DOT_PULL_DEFER_FINISH=1` branch (no stage finish).
#[allow(clippy::too_many_arguments)]
fn check_pull_all_row(tag: &str, count: usize, quiet: bool, verbose: bool, defer: bool) {
    let shell_side = AllSide::build(&format!("{tag}-shell"), count);
    let rust_side = AllSide::build(&format!("{tag}-rust"), count);
    setup_all_side(&shell_side, tag);
    setup_all_side(&rust_side, tag);
    let shell_entries = shell_side.entries();
    let quoted: Vec<String> = shell_entries.iter().map(|entry| sq(entry)).collect();
    let snippet = format!(
        "export HOME={h} DOT_QUIET={q} DOT_VERBOSE={v} XDG_STATE_HOME={h}/.local/state SHDEPS_INSTALL_DIR={h}/.local/share; \
         DOT_OVERLAY_ROLLBACK_PATHS=(); DOT_OVERLAY_ROLLBACK_TARGETS=(); OVERLAYS=({o}); ACTIVE_OVERLAYS=(); \
         DOT_OVERLAY_MANIFEST={m} DOT_OVERLAY_LEGACY_MANIFEST={l}; DOT_BASE_TOPOLOGY=ordinary; \
         SECONDS=0; {d}_repo_pull_all; code=$?; printf 'rc=%s\\n' \"$code\"; \
         printf 'agg=%s/%s/%s/%s\\n' \"$DOT_REPO_AGG_CURRENT\" \"$DOT_REPO_AGG_CHANGED\" \
         \"$DOT_REPO_AGG_FAILED\" \"$DOT_REPO_AGG_SKIPPED\"; \
         printf 'items<<<%s>>>' \"$DOT_REPO_AGG_CHANGED_ITEMS\"; \
         p={b}; if [[ -f \"$p\" ]]; then printf 'base=file:%s\\n' \"$(cat \"$p\")\"; else printf 'base=absent\\n'; fi; \
         p={v0}; if [[ -f \"$p\" ]]; then printf 'ovl=file:%s\\n' \"$(cat \"$p\")\"; else printf 'ovl=absent\\n'; fi",
        h = sq(&shell_side.home_text),
        q = u8::from(quiet),
        v = u8::from(verbose),
        o = quoted.join(" "),
        m = sq(&shell_side.manifest),
        l = sq(&shell_side.legacy),
        d = if defer {
            "export DOT_PULL_DEFER_FINISH=1; ".to_string()
        } else {
            String::new()
        },
        b = sq(&format!("{}/newbase.txt", shell_side.home_text)),
        v0 = sq(&format!(
            "{}/home/newfile.txt",
            shell_side
                .overlays
                .first()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        )),
    );
    let (code, out, err) = shell_run(&shell_side.home, &snippet);
    assert_eq!(code, 0, "harness exit for {tag}");
    let shell_dir = shell_side._dir.path().to_string_lossy().into_owned();
    let shell_out = normalize_elapsed(&normalize(
        &String::from_utf8(out).expect("shell dump"),
        &shell_side.home_text,
        &shell_dir,
    ));
    let shell_err = normalize(
        &String::from_utf8(err).expect("shell warnings"),
        &shell_side.home_text,
        &shell_dir,
    );

    let home_text = rust_side.home_text.clone();
    let rust_entries = rust_side.entries();
    let dest = DestinationInputs {
        pwd: home_text.clone(),
        home: home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    let candidate = candidate_env(&home_text);
    let logger = Log::new(false, false);
    let palette = plain_palette();
    let empty: &[OsString] = &[];
    let quiet_text = if quiet { "1" } else { "0" };
    let verbose_text = if verbose { "1" } else { "0" };
    let defer_text = if defer { Some("1") } else { None };
    let all_inputs = PullAllInputs {
        entries: &rust_entries,
        extra_args: empty,
        home: &home_text,
        dot_quiet: Some(quiet_text),
        dot_verbose: Some(verbose_text),
        ui_total: None,
        update_jobs: Some("2"),
        bar_width: "8",
        defer_finish: defer_text,
        palette: &palette,
        multibyte: false,
        ascii: true,
        candidate: &candidate,
        base: &base,
        quarantine: None,
        overlays: &rust_entries,
        dest: &dest,
        manifest: &rust_side.manifest,
        legacy_manifest: &rust_side.legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &rust_side.home,
        tool: &tool,
        log: &logger,
    };
    let mut stage = fleet_stage(None, quiet);
    let mut stdout = Vec::new();
    let mut warnings = Vec::new();
    let outcome = pull_all(
        &all_inputs,
        &mut stage,
        &mut moves,
        &mut stdout,
        &mut warnings,
        0,
    );
    // Shell deferred rows report aggregation globals; non-deferred
    // rows finish the stage inline. Mirror both shapes after the
    // shared rc line so the dumps compare.
    let mut rust_stdout = String::from_utf8(stdout).expect("rust stdout");
    rust_stdout.push_str(&format!("rc={}\n", outcome.rc));
    if defer {
        rust_stdout.push_str(&format!(
            "agg={}/{}/{}/{}\nitems<<<{}>>>",
            outcome.current,
            outcome.changed,
            outcome.failed,
            outcome.skipped,
            outcome
                .changed_items
                .iter()
                .map(|item| format!("{item}\n"))
                .collect::<String>(),
        ));
    } else {
        // Non-deferred shell rows print no agg globals (they are
        // unset); the stage summary plus changed notes already went
        // to stdout above. Only the aftermath probes follow.
        rust_stdout.push_str("agg=///\nitems<<<>>>");
    }
    let base_probe = match std::fs::read(rust_side.home.join("newbase.txt")) {
        Ok(bytes) => format!(
            "base=file:{}\n",
            String::from_utf8_lossy(&bytes).trim_end_matches('\n')
        ),
        Err(_) => "base=absent\n".to_string(),
    };
    let ovl_probe = rust_side
        .overlays
        .first()
        .map(
            |overlay| match std::fs::read(overlay.join("home/newfile.txt")) {
                Ok(bytes) => format!(
                    "ovl=file:{}\n",
                    String::from_utf8_lossy(&bytes).trim_end_matches('\n')
                ),
                Err(_) => "ovl=absent\n".to_string(),
            },
        )
        .unwrap_or_else(|| "ovl=absent\n".to_string());
    rust_stdout.push_str(&base_probe);
    rust_stdout.push_str(&ovl_probe);
    let rust_dir = rust_side._dir.path().to_string_lossy().into_owned();
    let rust_stdout = normalize_elapsed(&normalize(&rust_stdout, &home_text, &rust_dir));
    let rust_err = normalize(
        &String::from_utf8(warnings).expect("rust warnings"),
        &home_text,
        &rust_dir,
    );
    assert_eq!(rust_stdout, shell_out, "pull-all stdout for {tag}");
    assert_eq!(rust_err, shell_err, "pull-all stderr for {tag}");
    assert_eq!(
        outcome.status.as_str(),
        if outcome.failed > 0 {
            "failed"
        } else if outcome.changed > 0 {
            "changed"
        } else {
            "ok"
        },
        "pull-all status coherence for {tag}"
    );
    assert_eq!(outcome.deferred, defer, "pull-all defer flag for {tag}");
}

#[test]
fn pull_all_rows_agree() {
    for (tag, count, quiet, verbose, defer) in [
        ("all-current", 1, false, false, false),
        ("all-changed", 1, false, false, false),
        ("all-skipped-overlay", 1, false, false, false),
        ("all-changed", 1, false, false, true),
    ] {
        check_pull_all_row(tag, count, quiet, verbose, defer);
    }
}

#[test]
fn fleet_parallel_stress_keeps_order() {
    // Stress for the new concurrency (plan: stress, not just
    // parity): eight current overlays under a bound of two, run
    // three times. Every run must replay declaration order with a
    // full current tally and leave no scratch behind.
    let side = FleetSide::build("fleet-stress", 8);
    let entries = side.entries();
    let home_text = side.home_text.clone();
    let dest = DestinationInputs {
        pwd: home_text.clone(),
        home: home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home_text.clone(),
    };
    let candidate = candidate_env(&home_text);
    let logger = Log::new(false, false);
    let palette = plain_palette();
    let empty: &[OsString] = &[];
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let inputs = PullOverlaysInputs {
        entries: &entries,
        extra_args: empty,
        home: &home_text,
        ui_total: None,
        dot_quiet: Some("0"),
        dot_verbose: Some("0"),
        update_jobs: Some("2"),
        progress_done: Some("0"),
        progress_total: Some("0"),
        bar_width: "8",
        palette: &palette,
        multibyte: false,
        ascii: true,
        candidate: &candidate,
        base: &base,
        quarantine: None,
        overlays: &[],
        dest: &dest,
        manifest: &side.manifest,
        legacy_manifest: &side.legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &side.home,
        tool: &tool,
        log: &logger,
    };
    let want: Vec<String> = (0..8).map(|index| format!("ovl{index} current")).collect();
    for _ in 0..3 {
        let mut stage = fleet_stage(None, false);
        let mut run_moves = dot::temp::MoveCache::default();
        let mut stdout = Vec::new();
        let mut warnings = Vec::new();
        let outcome = pull_overlays(
            &inputs,
            &mut stage,
            &mut run_moves,
            &mut stdout,
            &mut warnings,
        );
        assert_eq!(outcome.rc, 0, "stress rc");
        assert_eq!(outcome.summaries, want, "stress order");
        assert_eq!(outcome.tally.current, 8, "stress tally");
        assert_eq!(outcome.tally.changed, 0, "stress changed");
        assert_eq!(outcome.tally.failed, 0, "stress failed");
        assert!(warnings.is_empty(), "stress warnings");
    }
}
