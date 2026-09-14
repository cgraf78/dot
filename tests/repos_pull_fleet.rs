//! Direct behavioral tests for the native overlay worker fleet.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::progress_ui::{Palette, Stage};
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull_fleet::{
    PullAllInputs, PullOverlaysInputs, RepoPullStatus, active_overlays, drain_result_dir,
    overlay_capture, parse_overlay, pull_all, pull_overlays, pull_overlays_serial,
};
use dot::repos_pull_queries::CandidateEnv;
use dot_test_support::TempDir;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {}",
        repo.display()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(repo: &Path, name: &str, content: &str) {
    std::fs::write(repo.join(name), content).expect("fixture file");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", name]);
}

fn clone(origin: &Path, path: &Path) {
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(origin)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn clone");
    assert!(status.success(), "clone {}", path.display());
}

fn palette() -> Palette {
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

fn stage() -> Stage {
    Stage::begin(palette(), "0", false, false, false, true)
}

fn candidate(home: &str) -> CandidateEnv {
    CandidateEnv {
        home: home.into(),
        checkout: format!("{home}/.local/share/cgraf78/dot"),
        pwd: home.into(),
        source_root: env!("CARGO_MANIFEST_DIR").into(),
        state_home: format!("{home}/.local/state"),
        install_root: format!("{home}/.local/share"),
        provider_state: format!("{home}/.local/state/shdeps"),
        overlay_paths: Vec::new(),
        init_backup: None,
    }
}

struct Fleet {
    _dir: TempDir,
    home: PathBuf,
    origins: Vec<PathBuf>,
    overlays: Vec<PathBuf>,
}

impl Fleet {
    fn new(tag: &str, count: usize) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir(&home).expect("home");
        git(&home, &["init", "-q"]);
        commit(&home, "base.txt", "base\n");
        let mut origins = Vec::new();
        let mut overlays = Vec::new();
        for index in 0..count {
            let origin = dir.path().join(format!("origin-{index}"));
            std::fs::create_dir(&origin).expect("origin");
            git(&origin, &["init", "-q"]);
            commit(&origin, "overlay.txt", "v1\n");
            let overlay = dir.path().join(format!("overlay-{index}"));
            clone(&origin, &overlay);
            origins.push(origin);
            overlays.push(overlay);
        }
        Self {
            _dir: dir,
            home,
            origins,
            overlays,
        }
    }

    fn entries(&self) -> Vec<String> {
        self.overlays
            .iter()
            .enumerate()
            .map(|(index, path)| {
                format!(
                    "ovl{index}|{}|{}|x|false|git",
                    path.display(),
                    self.origins[index].display()
                )
            })
            .collect()
    }
}

fn run_overlays(
    fleet: &Fleet,
    parallel: bool,
    jobs: Option<&str>,
) -> (dot::repos_pull_fleet::PullOverlaysOutcome, String, String) {
    let home = fleet.home.to_string_lossy().into_owned();
    let entries = fleet.entries();
    let dest = DestinationInputs {
        pwd: home.clone(),
        home: home.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let candidate = candidate(&home);
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home.clone(),
    };
    let palette = palette();
    let log = Log::new(false, false);
    let manifest = fleet
        ._dir
        .path()
        .join("manifest.tsv")
        .to_string_lossy()
        .into_owned();
    let legacy = fleet
        ._dir
        .path()
        .join("legacy.tsv")
        .to_string_lossy()
        .into_owned();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let empty: &[OsString] = &[];
    let inputs = PullOverlaysInputs {
        entries: &entries,
        extra_args: empty,
        home: &home,
        ui_total: None,
        dot_quiet: Some("0"),
        dot_verbose: Some("0"),
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
        manifest: &manifest,
        legacy_manifest: &legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: fleet._dir.path(),
        tool: &tool,
        log: &log,
    };
    let mut stage = stage();
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let outcome = if parallel {
        pull_overlays(&inputs, &mut stage, &mut moves, &mut out, &mut warnings)
    } else {
        pull_overlays_serial(&inputs, &mut stage, &mut moves, &mut out, &mut warnings)
    };
    (
        outcome,
        String::from_utf8(out).expect("stdout"),
        String::from_utf8(warnings).expect("warnings"),
    )
}

#[test]
fn overlay_parser_and_filter_preserve_declaration_semantics() {
    let parsed = parse_overlay("name|path|url|conf|true|git|extra");
    assert_eq!(
        (
            parsed.name,
            parsed.path,
            parsed.url,
            parsed.optional_raw,
            parsed.sync
        ),
        ("name", "path", "url", "true", "git|extra")
    );
    let entries = vec![
        "first|/missing|url||true|git".to_string(),
        "disabled|/missing|url|||none".to_string(),
        "inactive|/missing||||git".to_string(),
        "default|/missing|url|||".to_string(),
    ];
    let active = active_overlays(&entries);
    assert_eq!(
        active.iter().map(|entry| entry.name).collect::<Vec<_>>(),
        ["first", "default"]
    );
    assert!(active[0].optional);
}

#[test]
fn drain_replays_nonempty_logs_in_sorted_order_and_removes_scratch() {
    let dir = TempDir::new("fleet-drain").expect("fixture dir");
    std::fs::write(dir.path().join("010.log"), "tenth\n").expect("log");
    std::fs::write(dir.path().join("001.log"), "first\n").expect("log");
    std::fs::write(dir.path().join("002.log"), "").expect("log");
    std::fs::write(dir.path().join("001.rc"), "0").expect("rc");
    let path = dir.path().to_path_buf();
    let mut out = Vec::new();
    drain_result_dir(&path, &mut out);
    assert_eq!(out, b"first\ntenth\n");
    assert!(!path.exists());
}

#[test]
fn capture_writes_indexed_status_and_result_files() {
    let fleet = Fleet::new("fleet-capture", 1);
    commit(&fleet.origins[0], "changed.txt", "changed\n");
    let home = fleet.home.to_string_lossy().into_owned();
    let entries = fleet.entries();
    let active = active_overlays(&entries);
    let dest = DestinationInputs {
        pwd: home.clone(),
        home: home.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let candidate = candidate(&home);
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home.clone(),
    };
    let palette = palette();
    let logger = Log::new(false, false);
    let manifest = fleet
        ._dir
        .path()
        .join("manifest.tsv")
        .to_string_lossy()
        .into_owned();
    let legacy = fleet
        ._dir
        .path()
        .join("legacy.tsv")
        .to_string_lossy()
        .into_owned();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let inputs = PullOverlaysInputs {
        entries: &entries,
        extra_args: &[],
        home: &home,
        ui_total: None,
        dot_quiet: Some("0"),
        dot_verbose: Some("0"),
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
        manifest: &manifest,
        legacy_manifest: &legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: fleet._dir.path(),
        tool: &tool,
        log: &logger,
    };
    let result = fleet._dir.path().join("results");
    std::fs::create_dir(&result).expect("results");
    assert_eq!(
        overlay_capture(7, &result, &active[0], &inputs, &mut moves),
        ("changed".into(), 0)
    );
    assert_eq!(std::fs::read_to_string(result.join("007.rc")).unwrap(), "0");
    assert_eq!(
        std::fs::read_to_string(result.join("007.status")).unwrap(),
        "changed"
    );
    assert!(result.join("007.log").is_file());
    assert_eq!(
        std::fs::read_to_string(fleet.overlays[0].join("changed.txt")).unwrap(),
        "changed\n"
    );
}

#[test]
fn serial_and_parallel_fleets_report_current_changed_and_skipped_in_order() {
    for parallel in [false, true] {
        let fleet = Fleet::new(
            if parallel {
                "fleet-parallel"
            } else {
                "fleet-serial"
            },
            3,
        );
        commit(&fleet.origins[0], "changed.txt", "changed\n");
        git(&fleet.overlays[2], &["branch", "--unset-upstream"]);
        let (outcome, _out, warnings) = run_overlays(&fleet, parallel, Some("2"));
        assert_eq!(outcome.rc, 0);
        assert_eq!(outcome.done, 3);
        assert_eq!(outcome.reply, "ovl0 changed, ovl1 current, ovl2 skipped");
        assert_eq!(
            (
                outcome.tally.changed,
                outcome.tally.current,
                outcome.tally.skipped,
                outcome.tally.failed
            ),
            (1, 1, 1, 0)
        );
        assert_eq!(outcome.tally.changed_items, "ovl0 dotfiles updated\n");
        assert!(warnings.is_empty());
        assert_eq!(
            std::fs::read_to_string(fleet.overlays[0].join("changed.txt")).unwrap(),
            "changed\n"
        );
    }
}

#[test]
fn parallel_fleet_stress_keeps_declaration_order() {
    let fleet = Fleet::new("fleet-stress", 16);
    for origin in &fleet.origins {
        commit(origin, "changed.txt", "changed\n");
    }
    let (outcome, _, warnings) = run_overlays(&fleet, true, Some("4"));
    assert_eq!(outcome.rc, 0);
    assert_eq!(outcome.tally.changed, 16);
    assert_eq!(
        outcome.summaries,
        (0..16)
            .map(|index| format!("ovl{index} changed"))
            .collect::<Vec<_>>()
    );
    assert!(warnings.is_empty());
}

#[test]
fn pull_all_aggregates_base_and_overlay_outcomes() {
    let fleet = Fleet::new("fleet-all", 1);
    commit(&fleet.origins[0], "changed.txt", "changed\n");
    let home = fleet.home.to_string_lossy().into_owned();
    let entries = fleet.entries();
    let dest = DestinationInputs {
        pwd: home.clone(),
        home: home.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let candidate = candidate(&home);
    let base = Base {
        topology: Topology::Ordinary,
        client_git_dir: String::new(),
        home: home.clone(),
    };
    let palette = palette();
    let log = Log::new(false, false);
    let manifest = fleet
        ._dir
        .path()
        .join("manifest.tsv")
        .to_string_lossy()
        .into_owned();
    let legacy = fleet
        ._dir
        .path()
        .join("legacy.tsv")
        .to_string_lossy()
        .into_owned();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let inputs = PullAllInputs {
        entries: &entries,
        extra_args: &[],
        home: &home,
        dot_quiet: Some("0"),
        dot_verbose: Some("0"),
        ui_total: None,
        update_jobs: Some("2"),
        bar_width: "8",
        defer_finish: None,
        palette: &palette,
        multibyte: false,
        ascii: true,
        candidate: &candidate,
        base: &base,
        quarantine: None,
        overlays: &entries,
        dest: &dest,
        manifest: &manifest,
        legacy_manifest: &legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: fleet._dir.path(),
        tool: &tool,
        log: &log,
    };
    let mut stage = stage();
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let outcome = pull_all(&inputs, &mut stage, &mut moves, &mut out, &mut warnings, 0);
    assert_eq!(outcome.status, RepoPullStatus::Changed);
    assert_eq!(outcome.rc, 0);
    assert_eq!(
        (outcome.changed, outcome.skipped, outcome.failed),
        (1, 1, 0)
    );
    assert_eq!(outcome.summary, "1 repo changed, 1 repo skipped");
    assert_eq!(outcome.changed_items, ["ovl0 dotfiles updated"]);
    assert!(warnings.is_empty());
}
